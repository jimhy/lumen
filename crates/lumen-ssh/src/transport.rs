use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::pin::Pin;
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
use russh::{Channel, ChannelMsg, Disconnect, Pty};
use thiserror::Error;
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};
use tokio::sync::mpsc;
use tokio::time::{timeout, MissedTickBehavior};
use zeroize::Zeroizing;

use crate::credential::{Credential, SecretString};
use crate::host_key::{decide_host_key, HostKeyDecision, HostKeyIdentity};
use crate::metrics::{MetricsAccumulator, ServerMetrics, LINUX_METRICS_COMMAND};
use crate::sftp::{
    self, FileCommand, FileCommandSendError, FileEvent, FileEventReceiveError, FileLanes,
};

#[path = "directory.rs"]
mod directory;
pub use directory::{DirectoryEntry, DirectoryEntryKind, DirectoryError};

#[path = "manage.rs"]
mod manage;
pub use manage::{
    KillOutcome, KillStatus, ManageAction, ManageError, ManageOutcome, ManageRequest, PortEntry,
    ProcessEntry,
};

const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_COMMANDS_PER_TICK: usize = 64;
const MAX_MONITOR_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PRIVATE_KEY_BYTES: usize = 1024 * 1024;
const SHELL_DETECT_COMMAND: &[u8] = b"printf '%s' \"$SHELL\"";
const SHELL_DETECT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SHELL_PATH_BYTES: usize = 1024;
const BASH_CWD_BOOTSTRAP_COMMAND: &[u8] = b"__lumen_cwd_hook(){ local __lumen_status=$?; printf '\\033]9;9;%s\\007' \"$PWD\"; return \"$__lumen_status\"; }; if declare -p PROMPT_COMMAND 2>/dev/null | grep -q '^declare -a'; then case \" ${PROMPT_COMMAND[*]} \" in *' __lumen_cwd_hook '*) :;; *) PROMPT_COMMAND+=(__lumen_cwd_hook);; esac; else case \";${PROMPT_COMMAND-};\" in *';__lumen_cwd_hook;'*) :;; *) PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND;}__lumen_cwd_hook\";; esac; fi; __lumen_cwd_hook\r";
const ZSH_CWD_BOOTSTRAP_COMMAND: &[u8] = b"autoload -Uz add-zsh-hook; __lumen_cwd_hook(){ local __lumen_status=$?; printf '\\033]9;9;%s\\007' \"$PWD\"; return \"$__lumen_status\"; }; add-zsh-hook precmd __lumen_cwd_hook; __lumen_cwd_hook\r";
const FISH_CWD_BOOTSTRAP_COMMAND: &[u8] = b"functions -e __lumen_cwd_hook 2>/dev/null; function __lumen_cwd_hook --on-event fish_prompt; set -l __lumen_status $status; printf '\\033]9;9;%s\\007' \"$PWD\"; return $__lumen_status; end; __lumen_cwd_hook\r";
const RESTORE_PTY_ECHO_COMMAND: &[u8] = b"stty echo\r";
const PTY_BOOTSTRAP_MODES: [(Pty, u32); 2] = [(Pty::ECHO, 0), (Pty::ECHONL, 0)];
#[cfg(windows)]
const WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

static CONNECTION_THREAD_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteShellKind {
    Bash,
    Zsh,
    Fish,
}

struct DirectoryLanes {
    command_rx: Receiver<Command>,
    event_tx: Sender<Event>,
}

struct ConnectionLanes {
    command_rx: Receiver<Command>,
    event_tx: Sender<Event>,
    directory: DirectoryLanes,
    file: FileLanes,
}

struct ShellLanes {
    command_rx: Receiver<Command>,
    directory: DirectoryLanes,
    file: FileLanes,
}

pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub credential: Credential,
    pub mode: ConnectionMode,
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
            mode: ConnectionMode::InteractiveShell,
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
            .field("mode", &self.mode)
            .field("trusted_host_key", &self.trusted_host_key)
            .field("terminal", &self.terminal)
            .field("connect_timeout", &self.connect_timeout)
            .field("keepalive", &self.keepalive)
            .field("metrics", &self.metrics)
            .field("queues", &self.queues)
            .finish()
    }
}

/// Work performed after strict host-key verification and authentication.
///
/// Probe connections intentionally stop before opening a session channel,
/// allocating a PTY, requesting a shell, or starting Linux monitoring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionMode {
    #[default]
    InteractiveShell,
    Probe,
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
    /// List one Linux directory over a separate, read-only exec channel.
    ///
    /// Results are received exclusively through
    /// [`SshConnection::drain_directory`], not the terminal event lane.
    ListDirectory {
        token: u64,
        path: String,
        show_hidden: bool,
    },
    /// Best-effort cancellation. The token is invalidated before the remote
    /// channel is closed, so any late result is harmless.
    CancelDirectory {
        token: u64,
    },
    /// Process management (kill / port lookup / name search) over short-lived
    /// exec channels. Routed to the independent lane like directory listings,
    /// and answered through [`SshConnection::drain_directory`].
    Manage(ManageRequest),
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
            Self::ListDirectory {
                token, show_hidden, ..
            } => formatter
                .debug_struct("ListDirectory")
                .field("token", token)
                .field("path", &"<redacted>")
                .field("show_hidden", show_hidden)
                .finish(),
            Self::CancelDirectory { token } => formatter
                .debug_struct("CancelDirectory")
                .field("token", token)
                .finish(),
            Self::Manage(request) => formatter
                .debug_struct("Manage")
                .field("token", &request.token)
                .field("action", &request.action)
                .finish(),
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
    ConnectionTestSucceeded {
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
    /// Bounded result from the independent directory event lane.
    DirectoryListing {
        token: u64,
        entries: Vec<DirectoryEntry>,
        truncated: bool,
    },
    /// Non-fatal directory service failure. It never changes the terminal
    /// connection state.
    DirectoryError {
        token: u64,
        error: DirectoryError,
    },
    /// Bounded result of a process management request on the independent
    /// lane. Like directory results, it never affects terminal state.
    ManageResult {
        token: u64,
        result: Result<ManageOutcome, manage::ManageError>,
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
            Self::ConnectionTestSucceeded { host_key } => formatter
                .debug_struct("ConnectionTestSucceeded")
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
            Self::DirectoryListing {
                token,
                entries,
                truncated,
            } => formatter
                .debug_struct("DirectoryListing")
                .field("token", token)
                .field("entries", &entries.len())
                .field("truncated", truncated)
                .finish(),
            Self::DirectoryError { token, error } => formatter
                .debug_struct("DirectoryError")
                .field("token", token)
                .field("error", error)
                .finish(),
            Self::ManageResult { token, result } => formatter
                .debug_struct("ManageResult")
                .field("token", token)
                .field("ok", &result.is_ok())
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
    #[error("invalid SSH directory request")]
    InvalidDirectoryRequest,
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
    directory_command_tx: Sender<Command>,
    directory_event_rx: Receiver<Event>,
    file_command_tx: Sender<FileCommand>,
    file_event_rx: Receiver<FileEvent>,
    file_progress_event_rx: Receiver<FileEvent>,
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
        let (directory_command_tx, directory_command_rx) = bounded(directory::COMMAND_CAPACITY);
        let (directory_event_tx, directory_event_rx) = bounded(directory::EVENT_CAPACITY);
        let (file_command_tx, file_command_rx) = bounded(sftp::COMMAND_CAPACITY);
        let (file_event_tx, file_event_rx) = bounded(sftp::EVENT_CAPACITY);
        let (file_progress_event_tx, file_progress_event_rx) =
            bounded(sftp::PROGRESS_EVENT_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let thread_number = CONNECTION_THREAD_ID.fetch_add(1, Ordering::Relaxed);

        let thread = thread::Builder::new()
            .name(format!("lumen-ssh-{thread_number}"))
            .spawn(move || {
                connection_thread(
                    config,
                    ConnectionLanes {
                        command_rx,
                        event_tx,
                        directory: DirectoryLanes {
                            command_rx: directory_command_rx,
                            event_tx: directory_event_tx,
                        },
                        file: FileLanes {
                            command_rx: file_command_rx,
                            event_tx: file_event_tx,
                            progress_event_tx: file_progress_event_tx,
                        },
                    },
                    thread_cancelled,
                );
            })
            .map_err(StartError::Thread)?;

        Ok(Self {
            command_tx,
            event_rx,
            directory_command_tx,
            directory_event_rx,
            file_command_tx,
            file_event_rx,
            file_progress_event_rx,
            cancelled,
            maximum_input_bytes: queues.maximum_input_bytes,
            thread: Some(thread),
        })
    }

    /// Enqueues a command without blocking the calling thread.
    pub fn send(&self, command: Command) -> Result<(), CommandSendError> {
        match command {
            command @ (Command::ListDirectory { .. }
            | Command::CancelDirectory { .. }
            | Command::Manage(_)) => {
                try_send_directory_command(&self.directory_command_tx, command)
            }
            command => try_send_command(&self.command_tx, command, self.maximum_input_bytes),
        }
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

    /// Receives one directory result without touching the terminal event lane.
    pub fn try_recv_directory(&self) -> Result<Event, EventReceiveError> {
        self.directory_event_rx
            .try_recv()
            .map_err(|error| match error {
                CrossbeamTryRecvError::Empty => EventReceiveError::Empty,
                CrossbeamTryRecvError::Disconnected => EventReceiveError::Closed,
            })
    }

    /// Drains the independent directory result lane without blocking.
    pub fn drain_directory(&self, destination: &mut Vec<Event>) {
        destination.extend(self.directory_event_rx.try_iter());
    }

    /// Enqueues an SFTP operation without blocking the terminal or caller.
    pub fn send_file(&self, command: FileCommand) -> Result<(), FileCommandSendError> {
        sftp::try_send(&self.file_command_tx, command)
    }

    /// Receives one independent SFTP event without blocking.
    pub fn try_recv_file(&self) -> Result<FileEvent, FileEventReceiveError> {
        match self.file_event_rx.try_recv() {
            Ok(event) => Ok(event),
            Err(terminal_error) => match self.file_progress_event_rx.try_recv() {
                Ok(event) => Ok(event),
                Err(progress_error) => match (terminal_error, progress_error) {
                    (CrossbeamTryRecvError::Disconnected, CrossbeamTryRecvError::Disconnected) => {
                        Err(FileEventReceiveError::Closed)
                    }
                    _ => Err(FileEventReceiveError::Empty),
                },
            },
        }
    }

    /// Drains independent SFTP events without touching the terminal event lane.
    pub fn drain_file(&self, destination: &mut Vec<FileEvent>) {
        destination.extend(self.file_event_rx.try_iter());
        destination.extend(self.file_progress_event_rx.try_iter());
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
        Command::ListDirectory { .. } | Command::CancelDirectory { .. } => {
            return Err(CommandSendError::InvalidDirectoryRequest);
        }
        _ => {}
    }
    sender.try_send(command).map_err(|error| match error {
        CrossbeamTrySendError::Full(_) => CommandSendError::Full,
        CrossbeamTrySendError::Disconnected(_) => CommandSendError::Closed,
    })
}

fn try_send_directory_command(
    sender: &Sender<Command>,
    command: Command,
) -> Result<(), CommandSendError> {
    match &command {
        Command::ListDirectory { token, path, .. } => {
            directory::validate_request(*token, path)
                .map_err(|_| CommandSendError::InvalidDirectoryRequest)?;
        }
        Command::CancelDirectory { token } if *token == 0 => {
            return Err(CommandSendError::InvalidDirectoryRequest);
        }
        Command::CancelDirectory { .. } => {}
        Command::Manage(request) => {
            manage::validate(request).map_err(|_| CommandSendError::InvalidDirectoryRequest)?;
        }
        Command::Input(_) | Command::Resize(_) | Command::Disconnect => {
            return Err(CommandSendError::InvalidDirectoryRequest);
        }
    }
    sender.try_send(command).map_err(|error| match error {
        CrossbeamTrySendError::Full(_) => CommandSendError::Full,
        CrossbeamTrySendError::Disconnected(_) => CommandSendError::Closed,
    })
}

fn connection_thread(config: ConnectionConfig, lanes: ConnectionLanes, cancelled: Arc<AtomicBool>) {
    let mut sink = EventSink::new(
        lanes.event_tx,
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
        ShellLanes {
            command_rx: lanes.command_rx,
            directory: lanes.directory,
            file: lanes.file,
        },
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
    lanes: ShellLanes,
    sink: &mut EventSink,
    cancelled: Arc<AtomicBool>,
) -> Result<DisconnectReason, Failure> {
    sink.emit(Event::Connecting)?;
    let mode = config.mode;
    let host_key_decision = Arc::new(Mutex::new(None));
    let handler = StrictHostKeyHandler {
        expected: config.trusted_host_key.take(),
        decision: Arc::clone(&host_key_decision),
    };
    let (keepalive_interval, keepalive_max) = effective_keepalive(mode, config.keepalive);
    let russh_config = Arc::new(client::Config {
        keepalive_interval,
        keepalive_max,
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

    if !opens_interactive_shell(mode) {
        if cancelled.load(Ordering::Acquire) {
            disconnect_quickly(&session).await;
            return Ok(DisconnectReason::HandleDropped);
        }
        sink.emit(Event::ConnectionTestSucceeded {
            host_key: trusted_host_key,
        })?;
        // This return is deliberately before channel_open_session. Therefore
        // a probe cannot allocate a PTY, request a shell, or enter drive_shell
        // (the only place where Linux metrics collection is scheduled).
        disconnect_quickly(&session).await;
        return Ok(DisconnectReason::Requested);
    }

    // Query the account's configured shell on an isolated exec channel before
    // writing any bootstrap text into the interactive PTY. Sending Bash syntax
    // blindly to fish/csh both prints noise and can corrupt the user's command
    // line. Detection is optional and tightly bounded: an unknown shell keeps
    // the terminal usable, only without automatic cwd tracking.
    let cwd_bootstrap = detect_cwd_bootstrap(&session, &cancelled).await;

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
            &PTY_BOOTSTRAP_MODES,
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
    if let Some(command) = cwd_bootstrap {
        let bootstrap = unless_cancelled(shell.data_bytes(command.to_vec()), &cancelled).await;
        let Some(bootstrap) = bootstrap else {
            disconnect_quickly(&session).await;
            return Ok(DisconnectReason::HandleDropped);
        };
        bootstrap
            .map_err(|_| Failure::channel("could not initialize SSH current-directory tracking"))?;
    }
    let restore_echo = unless_cancelled(
        shell.data_bytes(RESTORE_PTY_ECHO_COMMAND.to_vec()),
        &cancelled,
    )
    .await;
    let Some(restore_echo) = restore_echo else {
        disconnect_quickly(&session).await;
        return Ok(DisconnectReason::HandleDropped);
    };
    restore_echo.map_err(|_| Failure::channel("could not restore SSH terminal echo"))?;
    sink.emit(Event::Connected {
        host_key: trusted_host_key,
    })?;

    let loop_result =
        drive_shell(&session, &mut shell, lanes, sink, cancelled, config.metrics).await;
    disconnect_quickly(&session).await;
    loop_result
}

async fn detect_cwd_bootstrap(
    session: &client::Handle<StrictHostKeyHandler>,
    cancelled: &Arc<AtomicBool>,
) -> Option<&'static [u8]> {
    let opened = unless_cancelled(
        timeout(SHELL_DETECT_TIMEOUT, session.channel_open_session()),
        cancelled,
    )
    .await?;
    let channel = match opened {
        Ok(Ok(channel)) => channel,
        Ok(Err(_)) | Err(_) => return None,
    };
    let executed = unless_cancelled(
        timeout(
            SHELL_DETECT_TIMEOUT,
            channel.exec(true, SHELL_DETECT_COMMAND),
        ),
        cancelled,
    )
    .await?;
    if !matches!(executed, Ok(Ok(()))) {
        return None;
    }
    let collected = unless_cancelled(
        timeout(SHELL_DETECT_TIMEOUT, collect_shell_path(channel)),
        cancelled,
    )
    .await?;
    let shell_path = match collected {
        Ok(Some(shell_path)) => shell_path,
        Ok(None) | Err(_) => return None,
    };
    cwd_bootstrap_for_shell_path(&shell_path)
}

async fn collect_shell_path(mut channel: Channel<client::Msg>) -> Option<String> {
    let mut output = Vec::new();
    let mut exit_status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                if output.len().saturating_add(data.len()) > MAX_SHELL_PATH_BYTES {
                    return None;
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
        return None;
    }
    String::from_utf8(output).ok()
}

fn cwd_bootstrap_for_shell_path(path: &str) -> Option<&'static [u8]> {
    let executable = path
        .trim_matches(|character: char| character.is_ascii_whitespace() || character == '\0')
        .rsplit('/')
        .next()?
        .to_ascii_lowercase();
    let kind = match executable.as_str() {
        "bash" | "rbash" => RemoteShellKind::Bash,
        "zsh" => RemoteShellKind::Zsh,
        "fish" => RemoteShellKind::Fish,
        _ => return None,
    };
    Some(match kind {
        RemoteShellKind::Bash => BASH_CWD_BOOTSTRAP_COMMAND,
        RemoteShellKind::Zsh => ZSH_CWD_BOOTSTRAP_COMMAND,
        RemoteShellKind::Fish => FISH_CWD_BOOTSTRAP_COMMAND,
    })
}

const fn opens_interactive_shell(mode: ConnectionMode) -> bool {
    matches!(mode, ConnectionMode::InteractiveShell)
}

fn effective_keepalive(
    mode: ConnectionMode,
    keepalive: KeepaliveConfig,
) -> (Option<Duration>, usize) {
    match mode {
        ConnectionMode::InteractiveShell => (
            keepalive.interval_option(),
            keepalive.maximum_missed_replies,
        ),
        ConnectionMode::Probe => (None, 0),
    }
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
type DirectoryStartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<directory::Opened, (directory::Job, DirectoryError)>> + 'a>>;
type SftpStartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<crate::sftp::FileSession, crate::sftp::FileError>> + 'a>>;

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
    lanes: ShellLanes,
    sink: &mut EventSink,
    cancelled: Arc<AtomicBool>,
    metrics_config: MetricsConfig,
) -> Result<DisconnectReason, Failure> {
    let ShellLanes {
        command_rx,
        directory: directory_lanes,
        file: file_lanes,
    } = lanes;
    let mut command_tick = tokio::time::interval(COMMAND_POLL_INTERVAL);
    command_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (monitor_tx, mut monitor_rx) = mpsc::channel::<MonitorResult>(1);
    let mut monitor_in_flight = false;
    let mut next_monitor_at = Instant::now();
    let mut last_metrics_at = None;
    let mut metrics_accumulator = MetricsAccumulator::new();
    let mut saw_exit_status = false;
    let (directory_done_tx, mut directory_done_rx) = mpsc::channel::<(
        u64,
        Result<directory::Listing, DirectoryError>,
    )>(directory::MAX_CONCURRENT_REQUESTS);
    let mut directory_cancellations = HashMap::<u64, Arc<AtomicBool>>::new();
    let mut directory_waiting = VecDeque::<directory::Job>::new();
    let mut directory_start: Option<DirectoryStartFuture<'_>> = None;
    let mut sftp_start: Option<SftpStartFuture<'_>> = Some(Box::pin(sftp::open(session)));
    let mut file_lanes = Some(file_lanes);
    let mut _file_service = None;
    let (manage_done_tx, mut manage_done_rx) = mpsc::channel::<(
        u64,
        Result<ManageOutcome, manage::ManageError>,
    )>(manage::MAX_CONCURRENT_REQUESTS);
    let mut manage_in_flight = 0_usize;

    loop {
        if directory_start.is_none() {
            while let Some(job) = directory_waiting.pop_front() {
                if job.cancelled.load(Ordering::Acquire) {
                    directory_cancellations.remove(&job.request.token);
                    emit_directory_event(
                        &directory_lanes.event_tx,
                        Event::DirectoryError {
                            token: job.request.token,
                            error: DirectoryError::Cancelled,
                        },
                    );
                    continue;
                }
                directory_start = Some(Box::pin(directory::open_channel(
                    session,
                    job,
                    Arc::clone(&cancelled),
                )));
                break;
            }
        }

        tokio::select! {
            opened = async {
                sftp_start
                    .as_mut()
                    .expect("SFTP start branch is guarded")
                    .await
            }, if sftp_start.is_some() => {
                sftp_start = None;
                _file_service = Some(sftp::spawn_service(
                    opened,
                    file_lanes.take().expect("SFTP lanes are started once"),
                    Arc::clone(&cancelled),
                ));
            }
            started = async {
                directory_start
                    .as_mut()
                    .expect("directory start branch is guarded")
                    .await
            }, if directory_start.is_some() => {
                directory_start = None;
                match started {
                    Ok(opened) => {
                        let token = opened.job.request.token;
                        let parent = opened.job.request.path;
                        let request_cancelled = opened.job.cancelled;
                        let connection_cancelled = Arc::clone(&cancelled);
                        let sender = directory_done_tx.clone();
                        tokio::spawn(async move {
                            let result = directory::collect(
                                opened.channel,
                                parent,
                                request_cancelled,
                                connection_cancelled,
                            )
                            .await;
                            let _ = sender.send((token, result)).await;
                        });
                    }
                    Err((job, error)) => {
                        directory_cancellations.remove(&job.request.token);
                        emit_directory_event(
                            &directory_lanes.event_tx,
                            Event::DirectoryError {
                                token: job.request.token,
                                error,
                            },
                        );
                    }
                }
            }
            completed = directory_done_rx.recv(), if !directory_cancellations.is_empty() => {
                if let Some((token, result)) = completed {
                    directory_cancellations.remove(&token);
                    let event = match result {
                        Ok(listing) => Event::DirectoryListing {
                            token,
                            entries: listing.entries,
                            truncated: listing.truncated,
                        },
                        Err(error) => Event::DirectoryError { token, error },
                    };
                    emit_directory_event(&directory_lanes.event_tx, event);
                }
            }
            completed = manage_done_rx.recv(), if manage_in_flight > 0 => {
                if let Some((token, result)) = completed {
                    manage_in_flight = manage_in_flight.saturating_sub(1);
                    emit_directory_event(
                        &directory_lanes.event_tx,
                        Event::ManageResult { token, result },
                    );
                }
            }
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
                        Ok(Command::ListDirectory { .. }
                        | Command::CancelDirectory { .. }
                        | Command::Manage(_)) => {
                            // `SshConnection::send` routes these to the independent lane.
                        }
                        Ok(Command::Disconnect) => return Ok(DisconnectReason::Requested),
                        Err(CrossbeamTryRecvError::Empty) => break,
                        Err(CrossbeamTryRecvError::Disconnected) => {
                            return Ok(DisconnectReason::HandleDropped);
                        }
                    }
                }

                for _ in 0..MAX_COMMANDS_PER_TICK {
                    match directory_lanes.command_rx.try_recv() {
                        Ok(Command::ListDirectory {
                            token,
                            path,
                            show_hidden,
                        }) => {
                            let request = directory::Request {
                                token,
                                path,
                                show_hidden,
                            };
                            let error = if directory::validate_request(token, &request.path).is_err() {
                                Some(DirectoryError::InvalidRequest)
                            } else if directory_cancellations.contains_key(&token) {
                                Some(DirectoryError::DuplicateToken)
                            } else if directory_cancellations.len()
                                >= directory::MAX_CONCURRENT_REQUESTS
                            {
                                Some(DirectoryError::Busy)
                            } else {
                                None
                            };
                            if let Some(error) = error {
                                emit_directory_event(
                                    &directory_lanes.event_tx,
                                    Event::DirectoryError { token, error },
                                );
                                continue;
                            }

                            let request_cancelled = Arc::new(AtomicBool::new(false));
                            directory_cancellations
                                .insert(token, Arc::clone(&request_cancelled));
                            directory_waiting.push_back(directory::Job {
                                request,
                                cancelled: request_cancelled,
                            });
                        }
                        Ok(Command::CancelDirectory { token }) => {
                            if let Some(request) = directory_cancellations.get(&token) {
                                request.store(true, Ordering::Release);
                            }
                        }
                        Ok(Command::Manage(request)) => {
                            let error = if manage::validate(&request).is_err() {
                                Some(manage::ManageError::InvalidRequest)
                            } else if manage_in_flight >= manage::MAX_CONCURRENT_REQUESTS {
                                // 并发上限内静默排队没有意义（都是秒级短命令），
                                // 直接回 Busy，让 UI 重试即可。
                                Some(manage::ManageError::Busy)
                            } else {
                                None
                            };
                            if let Some(error) = error {
                                emit_directory_event(
                                    &directory_lanes.event_tx,
                                    Event::ManageResult {
                                        token: request.token,
                                        result: Err(error),
                                    },
                                );
                                continue;
                            }
                            let token = request.token;
                            let action = request.action.clone();
                            match manage::open_and_exec(session, &request, Arc::clone(&cancelled))
                                .await
                            {
                                Ok(channel) => {
                                    manage_in_flight += 1;
                                    let sender = manage_done_tx.clone();
                                    tokio::spawn(async move {
                                        let result = manage::finish(channel, &action).await;
                                        let _ = sender.send((token, result)).await;
                                    });
                                }
                                Err(error) => {
                                    emit_directory_event(
                                        &directory_lanes.event_tx,
                                        Event::ManageResult {
                                            token,
                                            result: Err(error),
                                        },
                                    );
                                }
                            }
                        }
                        Ok(Command::Input(_) | Command::Resize(_) | Command::Disconnect) => {
                            // Defense in depth: terminal commands cannot enter this lane.
                        }
                        Err(CrossbeamTryRecvError::Empty) => break,
                        Err(CrossbeamTryRecvError::Disconnected) => break,
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

/// Directory results deliberately use a separate bounded lane. If its
/// consumer disappears or stops draining, dropping a result must not affect
/// terminal data, input, or connection lifetime.
fn emit_directory_event(sender: &Sender<Event>, event: Event) {
    let _ = sender.try_send(event);
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
    fn connection_defaults_to_interactive_shell() {
        let config = ConnectionConfig::new("example.test", 22, "alice", Credential::agent());
        assert_eq!(config.mode, ConnectionMode::InteractiveShell);
        assert!(opens_interactive_shell(config.mode));

        let (interval, maximum_missed_replies) = effective_keepalive(config.mode, config.keepalive);
        assert_eq!(interval, Some(Duration::from_secs(30)));
        assert_eq!(maximum_missed_replies, 3);
    }

    #[test]
    fn cwd_bootstrap_is_shell_specific_hidden_and_preserves_bash_prompt_shape() {
        let bash = std::str::from_utf8(BASH_CWD_BOOTSTRAP_COMMAND).expect("ASCII Bash bootstrap");
        let zsh = std::str::from_utf8(ZSH_CWD_BOOTSTRAP_COMMAND).expect("ASCII Zsh bootstrap");
        let fish = std::str::from_utf8(FISH_CWD_BOOTSTRAP_COMMAND).expect("ASCII fish bootstrap");
        for bootstrap in [bash, zsh, fish] {
            assert!(bootstrap.contains(r"\033]9;9;%s\007"));
            assert!(bootstrap.contains("\"$PWD\""));
            assert!(bootstrap.ends_with('\r'));
        }
        assert!(bash.contains("declare -p PROMPT_COMMAND"));
        assert!(bash.contains("PROMPT_COMMAND+=(__lumen_cwd_hook)"));
        assert!(bash.contains("return \"$__lumen_status\""));
        assert!(zsh.contains("add-zsh-hook precmd"));
        assert!(fish.contains("--on-event fish_prompt"));
        assert!(!fish.contains("--on-variable PWD"));
        assert!(fish.contains("return $__lumen_status"));
        assert_eq!(PTY_BOOTSTRAP_MODES, [(Pty::ECHO, 0), (Pty::ECHONL, 0)]);
        assert_eq!(RESTORE_PTY_ECHO_COMMAND, b"stty echo\r");
    }

    #[test]
    fn fish_cwd_hook_marks_every_prompt_without_wrapping_or_recursing() {
        let fish = std::str::from_utf8(FISH_CWD_BOOTSTRAP_COMMAND).expect("ASCII fish bootstrap");
        assert!(fish.contains("function __lumen_cwd_hook --on-event fish_prompt"));
        assert!(fish.contains("functions -e __lumen_cwd_hook"));
        assert!(!fish.contains("function fish_prompt"));
        assert!(!fish.contains("functions -c fish_prompt"));
        assert_eq!(fish.matches("--on-event fish_prompt").count(), 1);
    }

    #[test]
    fn cwd_bootstrap_detection_never_sends_foreign_syntax_to_unknown_shells() {
        assert_eq!(
            cwd_bootstrap_for_shell_path("/bin/bash"),
            Some(BASH_CWD_BOOTSTRAP_COMMAND)
        );
        assert_eq!(
            cwd_bootstrap_for_shell_path(" /usr/local/bin/zsh\r\n"),
            Some(ZSH_CWD_BOOTSTRAP_COMMAND)
        );
        assert_eq!(
            cwd_bootstrap_for_shell_path("/opt/homebrew/bin/fish"),
            Some(FISH_CWD_BOOTSTRAP_COMMAND)
        );
        assert_eq!(cwd_bootstrap_for_shell_path("/bin/dash"), None);
        assert_eq!(cwd_bootstrap_for_shell_path("/bin/tcsh"), None);
        assert_eq!(cwd_bootstrap_for_shell_path(""), None);
    }

    #[test]
    fn probe_debug_and_execution_plan_are_explicit_and_secret_safe() {
        let mut config = ConnectionConfig::new(
            "example.test",
            22,
            "alice",
            Credential::password("probe-password-must-not-leak"),
        );
        config.mode = ConnectionMode::Probe;

        let debug = format!("{config:?}");
        assert!(debug.contains("Probe"));
        assert!(debug.contains("password"));
        assert!(!debug.contains("probe-password-must-not-leak"));
        assert!(!opens_interactive_shell(config.mode));

        let (interval, maximum_missed_replies) = effective_keepalive(config.mode, config.keepalive);
        assert_eq!(interval, None);
        assert_eq!(maximum_missed_replies, 0);
    }

    #[test]
    fn probe_success_has_a_dedicated_public_event() {
        let host_key = HostKeyIdentity::new(
            "ssh-ed25519",
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABC",
        );
        let event = Event::ConnectionTestSucceeded {
            host_key: host_key.clone(),
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("ConnectionTestSucceeded"));
        assert!(debug.contains("ssh-ed25519"));

        match event {
            Event::ConnectionTestSucceeded { host_key: reported } => assert_eq!(reported, host_key),
            _ => panic!("probe success must use its dedicated event"),
        }
    }

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
    fn directory_command_and_event_debug_hide_paths_and_contents() {
        let command = Command::ListDirectory {
            token: 7,
            path: "/secret/customer-a".to_owned(),
            show_hidden: true,
        };
        let event = Event::DirectoryListing {
            token: 7,
            entries: vec![DirectoryEntry {
                name: "private.txt".to_owned(),
                path: "/secret/customer-a/private.txt".to_owned(),
                kind: DirectoryEntryKind::File,
                size: 42,
            }],
            truncated: false,
        };
        let command_debug = format!("{command:?}");
        let event_debug = format!("{event:?}");

        assert!(command_debug.contains("token"));
        assert!(command_debug.contains("<redacted>"));
        assert!(!command_debug.contains("customer-a"));
        assert!(event_debug.contains("entries"));
        assert!(!event_debug.contains("private.txt"));
        assert!(!event_debug.contains("customer-a"));
    }

    #[test]
    fn directory_command_lane_is_independently_bounded_and_validated() {
        let (sender, receiver) = bounded(1);
        assert_eq!(
            try_send_directory_command(
                &sender,
                Command::ListDirectory {
                    token: 1,
                    path: "/srv".to_owned(),
                    show_hidden: false,
                },
            ),
            Ok(())
        );
        assert_eq!(
            try_send_directory_command(
                &sender,
                Command::ListDirectory {
                    token: 2,
                    path: "/srv/other".to_owned(),
                    show_hidden: false,
                },
            ),
            Err(CommandSendError::Full)
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(Command::ListDirectory { token: 1, .. })
        ));
        assert_eq!(
            try_send_directory_command(
                &sender,
                Command::ListDirectory {
                    token: 3,
                    path: "relative".to_owned(),
                    show_hidden: false,
                },
            ),
            Err(CommandSendError::InvalidDirectoryRequest)
        );
        assert!(receiver.is_empty());
    }

    #[test]
    fn full_directory_result_lane_never_backpressures_terminal_sink() {
        let (directory_sender, _directory_receiver) = bounded(1);
        directory_sender
            .try_send(Event::DirectoryError {
                token: 1,
                error: DirectoryError::Busy,
            })
            .expect("fill directory lane");
        emit_directory_event(
            &directory_sender,
            Event::DirectoryError {
                token: 2,
                error: DirectoryError::Busy,
            },
        );

        let (terminal_sender, terminal_receiver) = bounded(1);
        let mut terminal_sink = EventSink::new(terminal_sender, 1, 1024);
        assert_eq!(terminal_sink.emit(Event::Data(vec![1, 2, 3])), Ok(()));
        assert!(matches!(terminal_receiver.try_recv(), Ok(Event::Data(_))));
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
