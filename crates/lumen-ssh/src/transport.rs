use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{
    bounded, Receiver, Sender, TryRecvError as CrossbeamTryRecvError,
    TrySendError as CrossbeamTrySendError,
};
use russh::client;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg, Disconnect};
use thiserror::Error;
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};
use tokio::sync::mpsc;
use tokio::time::{timeout, MissedTickBehavior};
use zeroize::Zeroizing;

use crate::credential::{Credential, SecretString};
use crate::host_key::{decide_host_key, HostKeyDecision, HostKeyIdentity};
use crate::metrics::{MetricsAccumulator, ServerMetrics, LINUX_METRICS_COMMAND};

const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_COMMANDS_PER_TICK: usize = 64;
const MAX_MONITOR_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PRIVATE_KEY_BYTES: usize = 1024 * 1024;
#[cfg(windows)]
const WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

static CONNECTION_THREAD_ID: AtomicU64 = AtomicU64::new(1);

pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub credential: Credential,
    pub trusted_host_key: Option<HostKeyIdentity>,
    pub terminal: TerminalSize,
    pub connect_timeout: Duration,
    pub keepalive: KeepaliveConfig,
    pub metrics: MetricsConfig,
    pub queues: QueueConfig,
}

impl ConnectionConfig {
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        credential: Credential,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            credential,
            trusted_host_key: None,
            terminal: TerminalSize::default(),
            connect_timeout: Duration::from_secs(15),
            keepalive: KeepaliveConfig::default(),
            metrics: MetricsConfig::default(),
            queues: QueueConfig::default(),
        }
    }

    fn validate(&self) -> Result<(), StartError> {
        validate_text(&self.host, 255, "host")?;
        validate_text(&self.username, 256, "username")?;
        if self.host.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(StartError::InvalidConfig("host"));
        }
        if self.port == 0 {
            return Err(StartError::InvalidConfig("port"));
        }
        self.terminal.validate()?;
        if self.connect_timeout.is_zero() || self.connect_timeout > Duration::from_secs(300) {
            return Err(StartError::InvalidConfig("connect timeout"));
        }
        self.keepalive.validate()?;
        self.metrics.validate()?;
        self.queues.validate()?;
        if let Some(host_key) = &self.trusted_host_key {
            host_key
                .validate()
                .map_err(|_| StartError::InvalidConfig("trusted host key"))?;
        }
        Ok(())
    }
}

impl fmt::Debug for ConnectionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectionConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("credential", &self.credential.kind_name())
            .field("trusted_host_key", &self.trusted_host_key)
            .field("terminal", &self.terminal)
            .field("connect_timeout", &self.connect_timeout)
            .field("keepalive", &self.keepalive)
            .field("metrics", &self.metrics)
            .field("queues", &self.queues)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u32,
    pub rows: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl TerminalSize {
    #[must_use]
    pub const fn new(columns: u32, rows: u32) -> Self {
        Self {
            columns,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn validate(self) -> Result<(), StartError> {
        if self.columns == 0 || self.rows == 0 || self.columns > 10_000 || self.rows > 10_000 {
            return Err(StartError::InvalidConfig("terminal size"));
        }
        Ok(())
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self::new(120, 36)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeepaliveConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub maximum_missed_replies: usize,
}

impl KeepaliveConfig {
    fn validate(self) -> Result<(), StartError> {
        if self.enabled
            && (self.interval < Duration::from_secs(5)
                || self.interval > Duration::from_secs(3600)
                || !(1..=100).contains(&self.maximum_missed_replies))
        {
            return Err(StartError::InvalidConfig("keepalive"));
        }
        Ok(())
    }

    fn interval_option(self) -> Option<Duration> {
        self.enabled.then_some(self.interval)
    }
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(30),
            maximum_missed_replies: 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub command_timeout: Duration,
}

impl MetricsConfig {
    fn validate(self) -> Result<(), StartError> {
        if self.interval < Duration::from_secs(1)
            || self.interval > Duration::from_secs(3600)
            || self.command_timeout < Duration::from_secs(1)
            || self.command_timeout > Duration::from_secs(60)
        {
            return Err(StartError::InvalidConfig("metrics"));
        }
        Ok(())
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(5),
            command_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueConfig {
    pub command_capacity: usize,
    pub event_capacity: usize,
    pub maximum_pending_event_bytes: usize,
    pub maximum_input_bytes: usize,
}

impl QueueConfig {
    fn validate(self) -> Result<(), StartError> {
        if !(1..=4096).contains(&self.command_capacity)
            || !(1..=4096).contains(&self.event_capacity)
            || !(1024..=64 * 1024 * 1024).contains(&self.maximum_pending_event_bytes)
            || !(1..=4 * 1024 * 1024).contains(&self.maximum_input_bytes)
        {
            return Err(StartError::InvalidConfig("queue limits"));
        }
        Ok(())
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            command_capacity: 256,
            event_capacity: 512,
            maximum_pending_event_bytes: 4 * 1024 * 1024,
            maximum_input_bytes: 1024 * 1024,
        }
    }
}

pub enum Command {
    Input(Vec<u8>),
    Resize(TerminalSize),
    Disconnect,
}

impl fmt::Debug for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(data) => formatter
                .debug_struct("Input")
                .field("bytes", &data.len())
                .finish(),
            Self::Resize(size) => formatter.debug_tuple("Resize").field(size).finish(),
            Self::Disconnect => formatter.write_str("Disconnect"),
        }
    }
}

pub enum Event {
    Connecting,
    HostKeyUnknown {
        presented: HostKeyIdentity,
    },
    HostKeyChanged {
        expected: HostKeyIdentity,
        presented: HostKeyIdentity,
    },
    Connected {
        host_key: HostKeyIdentity,
    },
    Data(Vec<u8>),
    ExtendedData {
        stream: u32,
        data: Vec<u8>,
    },
    Eof,
    ExitStatus(u32),
    ExitSignal {
        signal: String,
        core_dumped: bool,
    },
    Metrics(ServerMetrics),
    MetricsError {
        message: String,
    },
    Error(EventError),
    Disconnected {
        reason: DisconnectReason,
    },
}

impl fmt::Debug for Event {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connecting => formatter.write_str("Connecting"),
            Self::HostKeyUnknown { presented } => formatter
                .debug_struct("HostKeyUnknown")
                .field("presented", presented)
                .finish(),
            Self::HostKeyChanged {
                expected,
                presented,
            } => formatter
                .debug_struct("HostKeyChanged")
                .field("expected", expected)
                .field("presented", presented)
                .finish(),
            Self::Connected { host_key } => formatter
                .debug_struct("Connected")
                .field("host_key", host_key)
                .finish(),
            Self::Data(data) => formatter
                .debug_struct("Data")
                .field("bytes", &data.len())
                .finish(),
            Self::ExtendedData { stream, data } => formatter
                .debug_struct("ExtendedData")
                .field("stream", stream)
                .field("bytes", &data.len())
                .finish(),
            Self::Eof => formatter.write_str("Eof"),
            Self::ExitStatus(status) => formatter.debug_tuple("ExitStatus").field(status).finish(),
            Self::ExitSignal {
                signal,
                core_dumped,
            } => formatter
                .debug_struct("ExitSignal")
                .field("signal", signal)
                .field("core_dumped", core_dumped)
                .finish(),
            Self::Metrics(metrics) => formatter.debug_tuple("Metrics").field(metrics).finish(),
            Self::MetricsError { message } => formatter
                .debug_struct("MetricsError")
                .field("message", message)
                .finish(),
            Self::Error(error) => formatter.debug_tuple("Error").field(error).finish(),
            Self::Disconnected { reason } => formatter
                .debug_struct("Disconnected")
                .field("reason", reason)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    Requested,
    HandleDropped,
    HostKeyUnknown,
    HostKeyChanged,
    AuthenticationFailed,
    ConnectionFailed,
    ShellClosed,
    ShellExited,
    EventBackpressure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventError {
    pub kind: EventErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventErrorKind {
    Runtime,
    Network,
    Authentication,
    PrivateKey,
    Agent,
    Channel,
    EventBackpressure,
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error("invalid SSH connection configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("could not start SSH connection thread")]
    Thread(#[source] std::io::Error),
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum CommandSendError {
    #[error("SSH command queue is full")]
    Full,
    #[error("SSH connection is closed")]
    Closed,
    #[error("SSH input exceeds the configured boundary")]
    InputTooLarge,
    #[error("invalid SSH terminal size")]
    InvalidTerminalSize,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum EventReceiveError {
    #[error("no SSH event is ready")]
    Empty,
    #[error("SSH event stream is closed")]
    Closed,
}

pub struct SshConnection {
    command_tx: Sender<Command>,
    event_rx: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
    maximum_input_bytes: usize,
    thread: Option<JoinHandle<()>>,
}

impl SshConnection {
    pub fn start(config: ConnectionConfig) -> Result<Self, StartError> {
        config.validate()?;

        let queues = config.queues;
        let (command_tx, command_rx) = bounded(queues.command_capacity);
        let (event_tx, event_rx) = bounded(queues.event_capacity);
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let thread_number = CONNECTION_THREAD_ID.fetch_add(1, Ordering::Relaxed);

        let thread = thread::Builder::new()
            .name(format!("lumen-ssh-{thread_number}"))
            .spawn(move || connection_thread(config, command_rx, event_tx, thread_cancelled))
            .map_err(StartError::Thread)?;

        Ok(Self {
            command_tx,
            event_rx,
            cancelled,
            maximum_input_bytes: queues.maximum_input_bytes,
            thread: Some(thread),
        })
    }

    /// Enqueues a command without blocking the calling thread.
    pub fn send(&self, command: Command) -> Result<(), CommandSendError> {
        try_send_command(&self.command_tx, command, self.maximum_input_bytes)
    }

    /// Receives one event without blocking the calling thread.
    pub fn try_recv(&self) -> Result<Event, EventReceiveError> {
        self.event_rx.try_recv().map_err(|error| match error {
            CrossbeamTryRecvError::Empty => EventReceiveError::Empty,
            CrossbeamTryRecvError::Disconnected => EventReceiveError::Closed,
        })
    }

    pub fn drain(&self, destination: &mut Vec<Event>) {
        destination.extend(self.event_rx.try_iter());
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Requests disconnect and waits for the dedicated connection thread.
    pub fn shutdown(mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(Command::Disconnect);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl fmt::Debug for SshConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshConnection")
            .field("finished", &self.is_finished())
            .finish_non_exhaustive()
    }
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(Command::Disconnect);
    }
}

fn try_send_command(
    sender: &Sender<Command>,
    command: Command,
    maximum_input_bytes: usize,
) -> Result<(), CommandSendError> {
    match &command {
        Command::Input(data) if data.len() > maximum_input_bytes => {
            return Err(CommandSendError::InputTooLarge);
        }
        Command::Resize(size) if size.validate().is_err() => {
            return Err(CommandSendError::InvalidTerminalSize);
        }
        _ => {}
    }
    sender.try_send(command).map_err(|error| match error {
        CrossbeamTrySendError::Full(_) => CommandSendError::Full,
        CrossbeamTrySendError::Disconnected(_) => CommandSendError::Closed,
    })
}

fn connection_thread(
    config: ConnectionConfig,
    command_rx: Receiver<Command>,
    event_tx: Sender<Event>,
    cancelled: Arc<AtomicBool>,
) {
    let mut sink = EventSink::new(
        event_tx,
        config.queues.event_capacity,
        config.queues.maximum_pending_event_bytes,
    );

    let runtime = match build_current_thread_runtime() {
        Ok(runtime) => runtime,
        Err(_) => {
            sink.prioritize_terminal_events();
            let _ = sink.emit(Event::Error(EventError {
                kind: EventErrorKind::Runtime,
                message: "could not initialize the SSH runtime".to_owned(),
            }));
            let _ = sink.emit(Event::Disconnected {
                reason: DisconnectReason::ConnectionFailed,
            });
            sink.flush_blocking(Duration::from_millis(250));
            return;
        }
    };

    let result = runtime.block_on(run_connection(
        config,
        command_rx,
        &mut sink,
        Arc::clone(&cancelled),
    ));

    let reason = match result {
        Ok(reason) => reason,
        Err(failure) => {
            if failure.kind == EventErrorKind::EventBackpressure {
                sink.prioritize_terminal_events();
            }
            let _ = sink.emit(Event::Error(EventError {
                kind: failure.kind,
                message: failure.message.to_owned(),
            }));
            failure.reason
        }
    };
    let _ = sink.emit(Event::Disconnected { reason });
    runtime.block_on(sink.flush_async(Duration::from_millis(250)));
}

fn build_current_thread_runtime() -> Result<Runtime, std::io::Error> {
    RuntimeBuilder::new_current_thread().enable_all().build()
}

async fn run_connection(
    mut config: ConnectionConfig,
    command_rx: Receiver<Command>,
    sink: &mut EventSink,
    cancelled: Arc<AtomicBool>,
) -> Result<DisconnectReason, Failure> {
    sink.emit(Event::Connecting)?;
    let host_key_decision = Arc::new(Mutex::new(None));
    let handler = StrictHostKeyHandler {
        expected: config.trusted_host_key.take(),
        decision: Arc::clone(&host_key_decision),
    };
    let russh_config = Arc::new(client::Config {
        keepalive_interval: config.keepalive.interval_option(),
        keepalive_max: config.keepalive.maximum_missed_replies,
        nodelay: true,
        ..client::Config::default()
    });

    let address = (config.host.as_str(), config.port);
    let connect = timeout(
        config.connect_timeout,
        client::connect(russh_config, address, handler),
    );
    tokio::pin!(connect);
    let connect_result = tokio::select! {
        result = &mut connect => Some(result),
        () = wait_until_cancelled(Arc::clone(&cancelled)) => None,
    };
    let Some(connect_result) = connect_result else {
        return Ok(DisconnectReason::HandleDropped);
    };

    let mut session = match connect_result {
        Ok(Ok(session)) => session,
        Ok(Err(_)) => {
            return host_key_rejection_or_failure(&host_key_decision, sink);
        }
        Err(_) => {
            return Err(Failure::network("SSH connection timed out"));
        }
    };

    let trusted_host_key = match read_host_key_decision(&host_key_decision) {
        Some(HostKeyDecision::Trusted(host_key)) => host_key,
        Some(HostKeyDecision::Unknown { presented }) => {
            sink.emit(Event::HostKeyUnknown { presented })?;
            return Ok(DisconnectReason::HostKeyUnknown);
        }
        Some(HostKeyDecision::Changed {
            expected,
            presented,
        }) => {
            sink.emit(Event::HostKeyChanged {
                expected,
                presented,
            })?;
            return Ok(DisconnectReason::HostKeyChanged);
        }
        None => {
            return Err(Failure::network(
                "SSH handshake completed without a verified host key",
            ));
        }
    };

    let authentication_result = unless_cancelled(
        authenticate(&mut session, &config.username, config.credential),
        &cancelled,
    )
    .await;
    let Some(authentication_result) = authentication_result else {
        disconnect_quickly(&session).await;
        return Ok(DisconnectReason::HandleDropped);
    };
    let authenticated = authentication_result?;
    if !authenticated {
        disconnect_quickly(&session).await;
        return Err(Failure::authentication("SSH authentication was rejected"));
    }

    let shell_result = unless_cancelled(session.channel_open_session(), &cancelled).await;
    let Some(shell_result) = shell_result else {
        disconnect_quickly(&session).await;
        return Ok(DisconnectReason::HandleDropped);
    };
    let mut shell =
        shell_result.map_err(|_| Failure::channel("could not open the SSH shell channel"))?;
    let pty_result = unless_cancelled(
        shell.request_pty(
            true,
            "xterm-256color",
            config.terminal.columns,
            config.terminal.rows,
            config.terminal.pixel_width,
            config.terminal.pixel_height,
            &[],
        ),
        &cancelled,
    )
    .await;
    let Some(pty_result) = pty_result else {
        disconnect_quickly(&session).await;
        return Ok(DisconnectReason::HandleDropped);
    };
    pty_result.map_err(|_| Failure::channel("the SSH server rejected the PTY request"))?;
    let shell_request = unless_cancelled(shell.request_shell(true), &cancelled).await;
    let Some(shell_request) = shell_request else {
        disconnect_quickly(&session).await;
        return Ok(DisconnectReason::HandleDropped);
    };
    shell_request.map_err(|_| Failure::channel("the SSH server rejected the shell request"))?;
    sink.emit(Event::Connected {
        host_key: trusted_host_key,
    })?;

    let loop_result = drive_shell(
        &session,
        &mut shell,
        command_rx,
        sink,
        cancelled,
        config.metrics,
    )
    .await;
    disconnect_quickly(&session).await;
    loop_result
}

fn host_key_rejection_or_failure(
    decision: &Arc<Mutex<Option<HostKeyDecision>>>,
    sink: &mut EventSink,
) -> Result<DisconnectReason, Failure> {
    match read_host_key_decision(decision) {
        Some(HostKeyDecision::Unknown { presented }) => {
            sink.emit(Event::HostKeyUnknown { presented })?;
            Ok(DisconnectReason::HostKeyUnknown)
        }
        Some(HostKeyDecision::Changed {
            expected,
            presented,
        }) => {
            sink.emit(Event::HostKeyChanged {
                expected,
                presented,
            })?;
            Ok(DisconnectReason::HostKeyChanged)
        }
        _ => Err(Failure::network("could not establish the SSH connection")),
    }
}

async fn authenticate(
    session: &mut client::Handle<StrictHostKeyHandler>,
    username: &str,
    credential: Credential,
) -> Result<bool, Failure> {
    match credential {
        Credential::Password(password) => session
            .authenticate_password(username, password.expose())
            .await
            .map(|result| result.success())
            .map_err(|_| Failure::authentication("SSH password authentication failed")),
        Credential::PrivateKey(private_key) => {
            authenticate_private_key(
                session,
                username,
                &private_key.path,
                private_key.passphrase.as_ref(),
            )
            .await
        }
        Credential::Agent => authenticate_agent(session, username).await,
    }
}

async fn authenticate_private_key(
    session: &mut client::Handle<StrictHostKeyHandler>,
    username: &str,
    path: &Path,
    passphrase: Option<&SecretString>,
) -> Result<bool, Failure> {
    let encoded = read_private_key(path)?;
    let encoded = std::str::from_utf8(&encoded)
        .map_err(|_| Failure::private_key("the SSH private key is not valid UTF-8"))?;
    let key = russh::keys::decode_secret_key(encoded, passphrase.map(SecretString::expose))
        .map_err(|_| Failure::private_key("could not decode the SSH private key"))?;
    let hash_algorithm = session
        .best_supported_rsa_hash()
        .await
        .map_err(|_| Failure::private_key("could not negotiate an SSH key algorithm"))?
        .flatten();
    session
        .authenticate_publickey(
            username,
            PrivateKeyWithHashAlg::new(Arc::new(key), hash_algorithm),
        )
        .await
        .map(|result| result.success())
        .map_err(|_| Failure::private_key("SSH private-key authentication failed"))
}

fn read_private_key(path: &Path) -> Result<Zeroizing<Vec<u8>>, Failure> {
    let file = std::fs::File::open(path)
        .map_err(|_| Failure::private_key("could not read the SSH private key"))?;
    let maximum_plus_marker = MAX_PRIVATE_KEY_BYTES.saturating_add(1);
    let mut encoded = Zeroizing::new(Vec::with_capacity(64 * 1024));
    file.take(maximum_plus_marker as u64)
        .read_to_end(&mut encoded)
        .map_err(|_| Failure::private_key("could not read the SSH private key"))?;
    if encoded.len() > MAX_PRIVATE_KEY_BYTES {
        return Err(Failure::private_key(
            "the SSH private key exceeds the local safety limit",
        ));
    }
    Ok(encoded)
}

type DynamicAgent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

async fn authenticate_agent(
    session: &mut client::Handle<StrictHostKeyHandler>,
    username: &str,
) -> Result<bool, Failure> {
    let mut agent = connect_platform_agent().await?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|_| Failure::agent("could not list SSH agent identities"))?;
    let hash_algorithm = session
        .best_supported_rsa_hash()
        .await
        .map_err(|_| Failure::agent("could not negotiate an SSH agent key algorithm"))?
        .flatten();

    for identity in identities {
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                let identity_hash = key.algorithm().is_rsa().then_some(hash_algorithm).flatten();
                session
                    .authenticate_publickey_with(username, key, identity_hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                let identity_hash = certificate
                    .algorithm()
                    .is_rsa()
                    .then_some(hash_algorithm)
                    .flatten();
                session
                    .authenticate_certificate_with(username, certificate, identity_hash, &mut agent)
                    .await
            }
        }
        .map_err(|_| Failure::agent("SSH agent signing failed"))?;

        if result.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(unix)]
async fn connect_platform_agent() -> Result<DynamicAgent, Failure> {
    AgentClient::connect_env()
        .await
        .map(AgentClient::dynamic)
        .map_err(|_| Failure::agent("SSH_AUTH_SOCK is unavailable"))
}

#[cfg(windows)]
async fn connect_platform_agent() -> Result<DynamicAgent, Failure> {
    let openssh = timeout(
        Duration::from_secs(1),
        AgentClient::connect_named_pipe(WINDOWS_AGENT_PIPE),
    )
    .await;
    if let Ok(Ok(agent)) = openssh {
        return Ok(agent.dynamic());
    }

    AgentClient::connect_pageant()
        .await
        .map(AgentClient::dynamic)
        .map_err(|_| Failure::agent("neither Windows OpenSSH agent nor Pageant is available"))
}

#[cfg(not(any(unix, windows)))]
async fn connect_platform_agent() -> Result<DynamicAgent, Failure> {
    Err(Failure::agent(
        "SSH agent authentication is unsupported on this platform",
    ))
}

async fn drive_shell(
    session: &client::Handle<StrictHostKeyHandler>,
    shell: &mut Channel<client::Msg>,
    command_rx: Receiver<Command>,
    sink: &mut EventSink,
    cancelled: Arc<AtomicBool>,
    metrics_config: MetricsConfig,
) -> Result<DisconnectReason, Failure> {
    let mut command_tick = tokio::time::interval(COMMAND_POLL_INTERVAL);
    command_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (monitor_tx, mut monitor_rx) = mpsc::channel::<MonitorResult>(1);
    let mut monitor_in_flight = false;
    let mut next_monitor_at = Instant::now();
    let mut last_metrics_at = None;
    let mut metrics_accumulator = MetricsAccumulator::new();
    let mut saw_exit_status = false;

    loop {
        tokio::select! {
            _ = command_tick.tick() => {
                sink.flush()?;
                if cancelled.load(Ordering::Acquire) {
                    return Ok(DisconnectReason::HandleDropped);
                }

                for _ in 0..MAX_COMMANDS_PER_TICK {
                    match command_rx.try_recv() {
                        Ok(Command::Input(data)) => {
                            if !data.is_empty() {
                                let write = unless_cancelled(shell.data_bytes(data), &cancelled).await;
                                let Some(write) = write else {
                                    return Ok(DisconnectReason::HandleDropped);
                                };
                                write
                                    .map_err(|_| Failure::channel("could not write to the SSH shell"))?;
                            }
                        }
                        Ok(Command::Resize(size)) => {
                            let resize = unless_cancelled(
                                shell.window_change(
                                    size.columns,
                                    size.rows,
                                    size.pixel_width,
                                    size.pixel_height,
                                ),
                                &cancelled,
                            )
                            .await;
                            let Some(resize) = resize else {
                                return Ok(DisconnectReason::HandleDropped);
                            };
                            resize
                                .map_err(|_| Failure::channel("could not resize the SSH shell"))?;
                        }
                        Ok(Command::Disconnect) => return Ok(DisconnectReason::Requested),
                        Err(CrossbeamTryRecvError::Empty) => break,
                        Err(CrossbeamTryRecvError::Disconnected) => {
                            return Ok(DisconnectReason::HandleDropped);
                        }
                    }
                }

                if metrics_config.enabled
                    && !monitor_in_flight
                    && Instant::now() >= next_monitor_at
                {
                    next_monitor_at = Instant::now() + metrics_config.interval;
                    let monitor_channel = unless_cancelled(
                        timeout(metrics_config.command_timeout, session.channel_open_session()),
                        &cancelled,
                    )
                    .await;
                    let Some(monitor_channel) = monitor_channel else {
                        return Ok(DisconnectReason::HandleDropped);
                    };
                    match monitor_channel {
                        Ok(Ok(channel)) => {
                            let exec = unless_cancelled(
                                timeout(
                                    metrics_config.command_timeout,
                                    channel.exec(true, LINUX_METRICS_COMMAND),
                                ),
                                &cancelled,
                            )
                            .await;
                            let Some(exec) = exec else {
                                return Ok(DisconnectReason::HandleDropped);
                            };
                            if matches!(exec, Ok(Ok(()))) {
                                monitor_in_flight = true;
                                spawn_monitor_collection(
                                    channel,
                                    monitor_tx.clone(),
                                    metrics_config.command_timeout,
                                );
                            } else {
                                sink.emit(Event::MetricsError {
                                    message: "could not start the Linux monitor command".to_owned(),
                                })?;
                            }
                        }
                        Ok(Err(_)) | Err(_) => {
                            sink.emit(Event::MetricsError {
                                message: "could not open the Linux monitor channel".to_owned(),
                            })?;
                        }
                    }
                }
            }
            monitor = monitor_rx.recv(), if monitor_in_flight => {
                monitor_in_flight = false;
                match monitor {
                    Some(Ok(output)) => {
                        let now = Instant::now();
                        let elapsed = last_metrics_at.map(|previous: Instant| now.saturating_duration_since(previous));
                        match metrics_accumulator.update(&output, elapsed) {
                            Ok(metrics) => {
                                last_metrics_at = Some(now);
                                sink.emit(Event::Metrics(metrics))?;
                            }
                            Err(error) => {
                                sink.emit(Event::MetricsError {
                                    message: error.to_string(),
                                })?;
                            }
                        }
                    }
                    Some(Err(message)) => {
                        sink.emit(Event::MetricsError {
                            message: message.to_owned(),
                        })?;
                    }
                    None => {
                        sink.emit(Event::MetricsError {
                            message: "Linux monitor task ended unexpectedly".to_owned(),
                        })?;
                    }
                }
            }
            message = shell.wait() => {
                match message {
                    Some(ChannelMsg::Data { data }) => {
                        sink.emit(Event::Data(data.to_vec()))?;
                    }
                    Some(ChannelMsg::ExtendedData { data, ext }) => {
                        sink.emit(Event::ExtendedData {
                            stream: ext,
                            data: data.to_vec(),
                        })?;
                    }
                    Some(ChannelMsg::Eof) => sink.emit(Event::Eof)?,
                    Some(ChannelMsg::Close) | None => {
                        return Ok(if saw_exit_status {
                            DisconnectReason::ShellExited
                        } else {
                            DisconnectReason::ShellClosed
                        });
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        saw_exit_status = true;
                        sink.emit(Event::ExitStatus(exit_status))?;
                    }
                    Some(ChannelMsg::ExitSignal {
                        signal_name,
                        core_dumped,
                        ..
                    }) => {
                        saw_exit_status = true;
                        sink.emit(Event::ExitSignal {
                            signal: format!("{signal_name:?}"),
                            core_dumped,
                        })?;
                    }
                    _ => {}
                }
            }
        }
    }
}

type MonitorResult = Result<String, &'static str>;

fn spawn_monitor_collection(
    channel: Channel<client::Msg>,
    sender: mpsc::Sender<MonitorResult>,
    command_timeout: Duration,
) {
    tokio::spawn(async move {
        let result = match timeout(command_timeout, collect_monitor_output(channel)).await {
            Ok(result) => result,
            Err(_) => Err("Linux monitor command timed out"),
        };
        let _ = sender.send(result).await;
    });
}

async fn collect_monitor_output(mut channel: Channel<client::Msg>) -> MonitorResult {
    let mut output = Vec::new();
    let mut exit_status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                if output.len().saturating_add(data.len()) > MAX_MONITOR_OUTPUT_BYTES {
                    return Err("Linux monitor output exceeded its boundary");
                }
                output.extend_from_slice(&data);
            }
            ChannelMsg::ExitStatus {
                exit_status: status,
            } => exit_status = Some(status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    if exit_status.is_some_and(|status| status != 0) {
        return Err("Linux monitor command failed");
    }
    String::from_utf8(output).map_err(|_| "Linux monitor output was not UTF-8")
}

async fn wait_until_cancelled(cancelled: Arc<AtomicBool>) {
    while !cancelled.load(Ordering::Acquire) {
        tokio::time::sleep(COMMAND_POLL_INTERVAL).await;
    }
}

async fn unless_cancelled<F>(future: F, cancelled: &Arc<AtomicBool>) -> Option<F::Output>
where
    F: Future,
{
    tokio::pin!(future);
    tokio::select! {
        output = &mut future => Some(output),
        () = wait_until_cancelled(Arc::clone(cancelled)) => None,
    }
}

async fn disconnect_quickly(session: &client::Handle<StrictHostKeyHandler>) {
    let _ = timeout(
        Duration::from_millis(250),
        session.disconnect(Disconnect::ByApplication, "", "en"),
    )
    .await;
}

struct StrictHostKeyHandler {
    expected: Option<HostKeyIdentity>,
    decision: Arc<Mutex<Option<HostKeyDecision>>>,
}

impl client::Handler for StrictHostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let presented = HostKeyIdentity {
            algorithm: server_public_key.algorithm().to_string(),
            sha256_fingerprint: server_public_key.fingerprint(HashAlg::Sha256).to_string(),
        };
        let decision = decide_host_key(self.expected.as_ref(), &presented);
        let trusted = matches!(decision, HostKeyDecision::Trusted(_));
        if let Ok(mut slot) = self.decision.lock() {
            *slot = Some(decision);
        } else {
            return Ok(false);
        }
        Ok(trusted)
    }
}

fn read_host_key_decision(
    decision: &Arc<Mutex<Option<HostKeyDecision>>>,
) -> Option<HostKeyDecision> {
    decision.lock().ok().and_then(|value| value.clone())
}

#[derive(Clone, Copy, Debug)]
struct Failure {
    kind: EventErrorKind,
    message: &'static str,
    reason: DisconnectReason,
}

impl Failure {
    const fn runtime(message: &'static str) -> Self {
        Self {
            kind: EventErrorKind::Runtime,
            message,
            reason: DisconnectReason::ConnectionFailed,
        }
    }

    const fn network(message: &'static str) -> Self {
        Self {
            kind: EventErrorKind::Network,
            message,
            reason: DisconnectReason::ConnectionFailed,
        }
    }

    const fn authentication(message: &'static str) -> Self {
        Self {
            kind: EventErrorKind::Authentication,
            message,
            reason: DisconnectReason::AuthenticationFailed,
        }
    }

    const fn private_key(message: &'static str) -> Self {
        Self {
            kind: EventErrorKind::PrivateKey,
            message,
            reason: DisconnectReason::AuthenticationFailed,
        }
    }

    const fn agent(message: &'static str) -> Self {
        Self {
            kind: EventErrorKind::Agent,
            message,
            reason: DisconnectReason::AuthenticationFailed,
        }
    }

    const fn channel(message: &'static str) -> Self {
        Self {
            kind: EventErrorKind::Channel,
            message,
            reason: DisconnectReason::ShellClosed,
        }
    }

    const fn event_backpressure() -> Self {
        Self {
            kind: EventErrorKind::EventBackpressure,
            message: "SSH event consumer could not keep up",
            reason: DisconnectReason::EventBackpressure,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SinkError {
    Closed,
    Backpressure,
}

impl From<SinkError> for Failure {
    fn from(error: SinkError) -> Self {
        match error {
            SinkError::Closed => Failure::runtime("SSH event consumer was dropped"),
            SinkError::Backpressure => Failure::event_backpressure(),
        }
    }
}

struct EventSink {
    sender: Sender<Event>,
    pending: VecDeque<Event>,
    pending_bytes: usize,
    maximum_pending_events: usize,
    maximum_pending_bytes: usize,
}

impl EventSink {
    fn new(
        sender: Sender<Event>,
        maximum_pending_events: usize,
        maximum_pending_bytes: usize,
    ) -> Self {
        Self {
            sender,
            pending: VecDeque::new(),
            pending_bytes: 0,
            maximum_pending_events,
            maximum_pending_bytes,
        }
    }

    fn emit(&mut self, event: Event) -> Result<(), SinkError> {
        self.flush()?;
        if self.pending.is_empty() {
            match self.sender.try_send(event) {
                Ok(()) => return Ok(()),
                Err(CrossbeamTrySendError::Full(event)) => {
                    return self.queue(event);
                }
                Err(CrossbeamTrySendError::Disconnected(_)) => {
                    return Err(SinkError::Closed);
                }
            }
        }
        self.queue(event)
    }

    fn queue(&mut self, event: Event) -> Result<(), SinkError> {
        let event_bytes = event_payload_bytes(&event);
        if self.pending.len() >= self.maximum_pending_events
            || self.pending_bytes.saturating_add(event_bytes) > self.maximum_pending_bytes
        {
            return Err(SinkError::Backpressure);
        }

        match (self.pending.back_mut(), event) {
            (Some(Event::Data(existing)), Event::Data(mut incoming)) => {
                existing.append(&mut incoming);
            }
            (
                Some(Event::ExtendedData {
                    stream: existing_stream,
                    data: existing,
                }),
                Event::ExtendedData {
                    stream,
                    data: mut incoming,
                },
            ) if *existing_stream == stream => {
                existing.append(&mut incoming);
            }
            (_, event) => self.pending.push_back(event),
        }
        self.pending_bytes = self.pending_bytes.saturating_add(event_bytes);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        while let Some(event) = self.pending.pop_front() {
            let event_bytes = event_payload_bytes(&event);
            match self.sender.try_send(event) {
                Ok(()) => {
                    self.pending_bytes = self.pending_bytes.saturating_sub(event_bytes);
                }
                Err(CrossbeamTrySendError::Full(event)) => {
                    self.pending.push_front(event);
                    break;
                }
                Err(CrossbeamTrySendError::Disconnected(_)) => {
                    return Err(SinkError::Closed);
                }
            }
        }
        Ok(())
    }

    fn prioritize_terminal_events(&mut self) {
        self.pending.clear();
        self.pending_bytes = 0;
    }

    async fn flush_async(&mut self, maximum_wait: Duration) {
        let deadline = Instant::now() + maximum_wait;
        while !self.pending.is_empty() && Instant::now() < deadline {
            if self.flush().is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn flush_blocking(&mut self, maximum_wait: Duration) {
        let deadline = Instant::now() + maximum_wait;
        while !self.pending.is_empty() && Instant::now() < deadline {
            if self.flush().is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn event_payload_bytes(event: &Event) -> usize {
    match event {
        Event::Data(data) | Event::ExtendedData { data, .. } => data.len(),
        _ => 1,
    }
}

fn validate_text(value: &str, maximum_bytes: usize, field: &'static str) -> Result<(), StartError> {
    if value.is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control) {
        return Err(StartError::InvalidConfig(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_debug_redacts_all_credentials() {
        let config = ConnectionConfig::new(
            "example.test",
            22,
            "alice",
            Credential::password("never-print-this-password"),
        );
        let debug = format!("{config:?}");
        assert!(debug.contains("password"));
        assert!(!debug.contains("never-print-this-password"));

        let key_config = ConnectionConfig::new(
            "example.test",
            22,
            "alice",
            Credential::private_key(
                r"C:\Users\alice\.ssh\id_ed25519",
                Some(SecretString::new("never-print-this-passphrase")),
            ),
        );
        let debug = format!("{key_config:?}");
        assert!(debug.contains("private-key"));
        assert!(!debug.contains("id_ed25519"));
        assert!(!debug.contains("never-print-this-passphrase"));
    }

    #[test]
    fn command_queue_is_bounded_and_nonblocking() {
        let (sender, _receiver) = bounded(1);
        assert_eq!(
            try_send_command(&sender, Command::Input(vec![1]), 1024),
            Ok(())
        );
        assert_eq!(
            try_send_command(&sender, Command::Resize(TerminalSize::default()), 1024),
            Err(CommandSendError::Full)
        );
        assert_eq!(
            try_send_command(&sender, Command::Input(vec![0; 1025]), 1024),
            Err(CommandSendError::InputTooLarge)
        );
    }

    #[test]
    fn event_boundary_fails_closed_without_unbounded_growth() {
        let (sender, _receiver) = bounded(1);
        let mut sink = EventSink::new(sender, 1, 4);
        assert_eq!(sink.emit(Event::Connecting), Ok(()));
        assert_eq!(sink.emit(Event::Data(vec![1, 2, 3, 4])), Ok(()));
        assert_eq!(
            sink.emit(Event::Data(vec![5])),
            Err(SinkError::Backpressure)
        );
        assert_eq!(sink.pending_bytes, 4);
        assert_eq!(sink.pending.len(), 1);
    }

    #[test]
    fn command_and_event_debug_hide_terminal_payloads() {
        let command = format!("{:?}", Command::Input(b"typed-secret".to_vec()));
        let event = format!("{:?}", Event::Data(b"server-secret".to_vec()));
        assert!(!command.contains("typed-secret"));
        assert!(!event.contains("server-secret"));
        assert!(command.contains("12"));
        assert!(event.contains("13"));
    }

    #[test]
    fn runtime_is_current_thread() {
        let runtime = build_current_thread_runtime().expect("runtime");
        runtime.block_on(async {
            assert_eq!(
                tokio::runtime::Handle::current().runtime_flavor(),
                tokio::runtime::RuntimeFlavor::CurrentThread
            );
        });
    }

    #[test]
    fn invalid_terminal_command_is_rejected_before_enqueue() {
        let (sender, receiver) = bounded(1);
        let invalid = TerminalSize::new(0, 24);
        assert_eq!(
            try_send_command(&sender, Command::Resize(invalid), 1024),
            Err(CommandSendError::InvalidTerminalSize)
        );
        assert!(receiver.is_empty());
    }

    #[test]
    fn keepalive_can_be_disabled_explicitly() {
        let enabled = KeepaliveConfig::default();
        assert_eq!(enabled.interval_option(), Some(Duration::from_secs(30)));

        let disabled = KeepaliveConfig {
            enabled: false,
            interval: Duration::ZERO,
            maximum_missed_replies: 0,
        };
        assert!(disabled.validate().is_ok());
        assert_eq!(disabled.interval_option(), None);
    }

    #[test]
    fn private_key_file_read_is_bounded() {
        let path = std::env::temp_dir().join(format!(
            "lumen-ssh-oversized-key-{}-{}.pem",
            std::process::id(),
            CONNECTION_THREAD_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, vec![b'x'; MAX_PRIVATE_KEY_BYTES + 1]).expect("write test key");
        let result = read_private_key(&path);
        let _ = std::fs::remove_file(path);
        assert!(result.is_err());
    }
}
