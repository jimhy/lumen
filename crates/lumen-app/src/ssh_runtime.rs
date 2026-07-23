//! Main-thread adapter between the SSH transport actor and Lumen's terminal.
//!
//! Each profile owns an independent transport and terminal state.  The UI only
//! chooses which profile is active; switching profiles or application modes
//! never tears down the other background connections.

use std::collections::HashMap;
use std::time::Duration;

use lumen_ssh::{
    Command, ConnectionConfig, ConnectionMode, Credential, DisconnectReason, Event, EventErrorKind,
    HostKeyIdentity, KeepaliveConfig, MetricsConfig, ServerMetrics, SshConnection, TerminalSize,
};
use lumen_term::Terminal;

use crate::settings::ViewMode;
use crate::ssh::{AuthMethod, HostKeyTrust, SshProfile};

const DEFAULT_ROWS: usize = 36;
const DEFAULT_COLUMNS: usize = 120;
const SSH_SCROLLBACK: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    CredentialRequired,
    Connecting,
    AwaitingHostKey,
    Connected,
    Disconnecting,
    Disconnected,
    Error,
    HostKeyChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownHostKey {
    pub profile_id: String,
    pub algorithm: String,
    pub sha256_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedHostKey {
    pub expected_algorithm: String,
    pub expected_sha256_fingerprint: String,
    pub presented_algorithm: String,
    pub presented_sha256_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetricsSummary {
    pub cpu: String,
    pub memory: String,
    pub load: String,
    pub root_disk: String,
    pub network: String,
    pub uptime: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeView {
    pub profile_id: String,
    pub profile_name: String,
    pub endpoint: String,
    pub state: ConnectionState,
    pub detail: Option<String>,
    pub metrics: Option<MetricsSummary>,
    pub metrics_error: Option<String>,
    pub unknown_host_key: Option<UnknownHostKey>,
    pub changed_host_key: Option<ChangedHostKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionTestState {
    Connecting,
    AwaitingHostKey {
        algorithm: String,
        fingerprint: String,
    },
    Success,
    Error {
        message: String,
    },
    HostKeyChanged {
        expected_algorithm: String,
        expected_fingerprint: String,
        presented_algorithm: String,
        presented_fingerprint: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionTestView {
    pub form_id: u64,
    /// 表单连接字段的单调修订号；不包含也不派生任何秘密。
    pub connection_revision: u64,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub state: ConnectionTestState,
}

impl ConnectionTestView {
    #[must_use]
    pub fn matches_target(&self, form_id: u64, host: &str, port: u16, username: &str) -> bool {
        self.form_id == form_id
            && self.host == host
            && self.port == port
            && self.username == username
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    pub active_terminal_changed: bool,
    pub active_status_changed: bool,
    pub active_became_connected: bool,
    /// 本批明确发生密码认证或私钥解析/解密错误的 profile。
    /// 调用方必须令这些 profile 下次连接强制提示，避免反复自动尝试坏凭据。
    pub credential_failures: Vec<String>,
    /// 本批成功进入 Connected 的 profile；调用方可清除其强制提示标记。
    pub connected_profiles: Vec<String>,
    /// 当前服务器表单的独立连接测试状态发生变化。
    pub connection_test_changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectIntent {
    AlreadyRunning,
    Password,
    PrivateKey,
    Agent,
    AwaitingHostKey,
    HostKeyChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EndpointIdentity {
    host: String,
    port: u16,
    username: String,
}

impl EndpointIdentity {
    fn from_profile(profile: &SshProfile) -> Self {
        Self {
            host: profile.host.clone(),
            port: profile.port,
            username: profile.username.clone(),
        }
    }

    fn matches_profile(&self, profile: &SshProfile) -> bool {
        self.host == profile.host && self.port == profile.port && self.username == profile.username
    }
}

struct RuntimeSession {
    profile_name: String,
    endpoint: String,
    endpoint_identity: EndpointIdentity,
    terminal: Terminal,
    connection: Option<SshConnection>,
    state: ConnectionState,
    detail: Option<String>,
    metrics: Option<ServerMetrics>,
    metrics_error: Option<String>,
    unknown_host_key: Option<HostKeyIdentity>,
    changed_host_key: Option<(HostKeyIdentity, HostKeyIdentity)>,
}

impl RuntimeSession {
    fn new(profile: &SshProfile) -> Self {
        Self {
            profile_name: profile.name.clone(),
            endpoint: endpoint(profile),
            endpoint_identity: EndpointIdentity::from_profile(profile),
            terminal: Terminal::new(DEFAULT_ROWS, DEFAULT_COLUMNS, SSH_SCROLLBACK),
            connection: None,
            state: ConnectionState::CredentialRequired,
            detail: None,
            metrics: None,
            metrics_error: None,
            unknown_host_key: None,
            changed_host_key: None,
        }
    }

    fn refresh_metadata(&mut self, profile: &SshProfile) {
        self.profile_name.clone_from(&profile.name);
        self.endpoint = endpoint(profile);
        self.endpoint_identity = EndpointIdentity::from_profile(profile);
    }

    fn is_running(&self) -> bool {
        self.connection.is_some()
            && matches!(
                self.state,
                ConnectionState::Connecting
                    | ConnectionState::Connected
                    | ConnectionState::Disconnecting
            )
    }

    fn blocks_input(&self) -> bool {
        matches!(
            self.state,
            ConnectionState::CredentialRequired
                | ConnectionState::AwaitingHostKey
                | ConnectionState::HostKeyChanged
        )
    }
}

struct ConnectionTestAttempt {
    form_id: u64,
    connection_revision: u64,
    endpoint_identity: EndpointIdentity,
    connection: Option<SshConnection>,
    state: ConnectionTestState,
}

#[derive(Default)]
pub struct SshRuntime {
    sessions: HashMap<String, RuntimeSession>,
    active_profile_id: Option<String>,
    connection_test: Option<ConnectionTestAttempt>,
}

impl SshRuntime {
    pub fn select_for_connect(&mut self, profile: &SshProfile) -> ConnectIntent {
        self.active_profile_id = Some(profile.id.clone());
        let session = self
            .sessions
            .entry(profile.id.clone())
            .or_insert_with(|| RuntimeSession::new(profile));

        if session.is_running() {
            return ConnectIntent::AlreadyRunning;
        }
        // 未决主机密钥属于产生它的旧 endpoint。配置在弹窗期间被编辑时，
        // 不能先用新 profile 刷新身份再复用旧指纹提示。
        if session.unknown_host_key.is_some() {
            return ConnectIntent::AwaitingHostKey;
        }
        if session.changed_host_key.is_some() {
            return ConnectIntent::HostKeyChanged;
        }
        session.refresh_metadata(profile);
        session.state = ConnectionState::CredentialRequired;
        session.detail = None;
        match profile.auth_method {
            AuthMethod::Password => ConnectIntent::Password,
            AuthMethod::PrivateKey => ConnectIntent::PrivateKey,
            AuthMethod::Agent => ConnectIntent::Agent,
        }
    }

    pub fn start(&mut self, profile: &SshProfile, credential: Credential) -> Result<(), String> {
        self.active_profile_id = Some(profile.id.clone());
        let session = self
            .sessions
            .entry(profile.id.clone())
            .or_insert_with(|| RuntimeSession::new(profile));
        session.refresh_metadata(profile);

        // Replacing one profile's failed attempt does not affect any other
        // profile. Dropping the old handle only signals its dedicated actor.
        session.connection.take();
        session.unknown_host_key = None;
        session.changed_host_key = None;
        session.detail = None;
        session.metrics_error = None;

        let (rows, columns) = {
            let grid = session.terminal.grid();
            (grid.rows().max(1), grid.cols().max(1))
        };
        let mut config = connection_config(profile, credential);
        config.terminal = terminal_size(rows, columns);

        match SshConnection::start(config) {
            Ok(connection) => {
                session.connection = Some(connection);
                session.state = ConnectionState::Connecting;
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                session.state = ConnectionState::Error;
                session.detail = Some(message.clone());
                Err(message)
            }
        }
    }

    /// Start an isolated network/host-key/authentication probe for one editor form.
    ///
    /// The transport probe stops before opening a channel, PTY or remote shell. It is
    /// deliberately separate from `sessions`, so testing an unsaved draft cannot
    /// replace the active terminal or affect another server's connection.
    pub fn start_connection_test(
        &mut self,
        form_id: u64,
        connection_revision: u64,
        profile: &SshProfile,
        credential: Credential,
    ) -> Result<(), String> {
        // Dropping a prior handle only signals its actor; it never joins on the UI thread.
        self.connection_test.take();

        let endpoint_identity = EndpointIdentity::from_profile(profile);
        let mut config = connection_config(profile, credential);
        config.mode = ConnectionMode::Probe;
        // Defense in depth. The transport also forces both features off for Probe.
        config.keepalive = KeepaliveConfig {
            enabled: false,
            ..KeepaliveConfig::default()
        };
        config.metrics = MetricsConfig {
            enabled: false,
            ..MetricsConfig::default()
        };

        match SshConnection::start(config) {
            Ok(connection) => {
                self.connection_test = Some(ConnectionTestAttempt {
                    form_id,
                    connection_revision,
                    endpoint_identity,
                    connection: Some(connection),
                    state: ConnectionTestState::Connecting,
                });
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                self.connection_test = Some(ConnectionTestAttempt {
                    form_id,
                    connection_revision,
                    endpoint_identity,
                    connection: None,
                    state: ConnectionTestState::Error {
                        message: message.clone(),
                    },
                });
                Err(message)
            }
        }
    }

    pub fn fail_connection_test(
        &mut self,
        form_id: u64,
        connection_revision: u64,
        profile: &SshProfile,
        message: impl Into<String>,
    ) {
        self.connection_test = Some(ConnectionTestAttempt {
            form_id,
            connection_revision,
            endpoint_identity: EndpointIdentity::from_profile(profile),
            connection: None,
            state: ConnectionTestState::Error {
                message: message.into(),
            },
        });
    }

    pub fn cancel_connection_test(&mut self, form_id: u64) -> bool {
        if self
            .connection_test
            .as_ref()
            .is_some_and(|attempt| attempt.form_id == form_id)
        {
            self.connection_test = None;
            true
        } else {
            false
        }
    }

    pub fn cancel_any_connection_test(&mut self) -> bool {
        self.connection_test.take().is_some()
    }

    #[must_use]
    pub fn connection_test_view(&self) -> Option<ConnectionTestView> {
        let attempt = self.connection_test.as_ref()?;
        Some(ConnectionTestView {
            form_id: attempt.form_id,
            connection_revision: attempt.connection_revision,
            host: attempt.endpoint_identity.host.clone(),
            port: attempt.endpoint_identity.port,
            username: attempt.endpoint_identity.username.clone(),
            state: attempt.state.clone(),
        })
    }

    pub fn disconnect_active(&mut self) {
        let Some(session) = self.active_session_mut() else {
            return;
        };
        if let Some(connection) = &session.connection {
            match connection.send(Command::Disconnect) {
                Ok(()) => {
                    session.state = ConnectionState::Disconnecting;
                    session.detail = None;
                }
                Err(error) => {
                    session.state = ConnectionState::Error;
                    session.detail = Some(error.to_string());
                }
            }
        }
    }

    pub fn remove_profile(&mut self, profile_id: &str) {
        // Dropping the handle signals cancellation to the profile's dedicated
        // actor. Other profile sessions remain untouched.
        self.sessions.remove(profile_id);
        if self.active_profile_id.as_deref() == Some(profile_id) {
            self.active_profile_id = None;
        }
    }

    pub fn send_input(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        let Some(session) = self.active_session_mut() else {
            return Err("No active SSH session".to_owned());
        };
        if session.state != ConnectionState::Connected {
            return Err("SSH session is not connected".to_owned());
        }
        session.terminal.grid_mut().scroll_to_bottom();
        let result = session
            .connection
            .as_ref()
            .ok_or_else(|| "SSH connection is closed".to_owned())?
            .send(Command::Input(bytes));
        if let Err(error) = result {
            let message = error.to_string();
            if matches!(error, lumen_ssh::CommandSendError::Closed) {
                session.state = ConnectionState::Error;
                session.detail = Some(message.clone());
            }
            return Err(message);
        }
        Ok(())
    }

    pub fn resize_active(&mut self, rows: usize, columns: usize) -> bool {
        let Some(session) = self.active_session_mut() else {
            return false;
        };
        let rows = rows.max(1);
        let columns = columns.max(1);
        let old = {
            let grid = session.terminal.grid();
            (grid.rows(), grid.cols())
        };
        if old == (rows, columns) {
            return false;
        }
        session.terminal.resize(rows, columns);
        if let Some(connection) = &session.connection {
            if let Err(error) = connection.send(Command::Resize(terminal_size(rows, columns))) {
                session.detail = Some(error.to_string());
            }
        }
        true
    }

    pub fn scroll_active(&mut self, lines: isize) -> bool {
        let Some(session) = self.active_session_mut() else {
            return false;
        };
        if lines == 0 {
            return false;
        }
        let before = session.terminal.grid().display_offset();
        session.terminal.grid_mut().scroll_display(lines);
        before != session.terminal.grid().display_offset()
    }

    pub fn drain(&mut self) -> DrainOutcome {
        let active_id = self.active_profile_id.clone();
        let mut outcome = DrainOutcome::default();
        let profile_ids = self.sessions.keys().cloned().collect::<Vec<_>>();

        for profile_id in profile_ids {
            let Some(session) = self.sessions.get_mut(&profile_id) else {
                continue;
            };
            let before_state = session.state.clone();
            let before_detail = session.detail.clone();
            let before_metrics = session.metrics.clone();
            let before_metrics_error = session.metrics_error.clone();
            let before_unknown = session.unknown_host_key.clone();
            let before_changed = session.changed_host_key.clone();
            let mut terminal_changed = false;
            let mut events = Vec::new();
            if let Some(connection) = &session.connection {
                connection.drain(&mut events);
            }
            for event in events {
                let credential_failed = event_is_saved_credential_failure(&event);
                let connected = matches!(&event, Event::Connected { .. });
                let changed = apply_event(session, event);
                terminal_changed |= changed;
                if credential_failed
                    && !outcome
                        .credential_failures
                        .iter()
                        .any(|existing| existing == &profile_id)
                {
                    outcome.credential_failures.push(profile_id.clone());
                }
                if connected
                    && !outcome
                        .connected_profiles
                        .iter()
                        .any(|existing| existing == &profile_id)
                {
                    outcome.connected_profiles.push(profile_id.clone());
                }
                if changed {
                    // Reply before processing a later Disconnected event from
                    // the same drain batch, otherwise DSR/DA responses could
                    // be lost when the handle is cleared.
                    reply_to_terminal_queries(session);
                }
            }

            if active_id.as_deref() == Some(profile_id.as_str()) {
                outcome.active_terminal_changed |= terminal_changed;
                outcome.active_status_changed |= before_state != session.state
                    || before_detail != session.detail
                    || before_metrics != session.metrics
                    || before_metrics_error != session.metrics_error
                    || before_unknown != session.unknown_host_key
                    || before_changed != session.changed_host_key;
                outcome.active_became_connected |= before_state != ConnectionState::Connected
                    && session.state == ConnectionState::Connected;
            }
        }
        outcome.connection_test_changed = self.drain_connection_test();
        outcome
    }

    fn drain_connection_test(&mut self) -> bool {
        let Some(attempt) = self.connection_test.as_mut() else {
            return false;
        };
        let before_state = attempt.state.clone();
        let before_live = attempt.connection.is_some();
        let mut events = Vec::new();
        if let Some(connection) = &attempt.connection {
            connection.drain(&mut events);
        }
        for event in events {
            let disconnected = matches!(event, Event::Disconnected { .. });
            apply_connection_test_event(attempt, event);
            if disconnected {
                attempt.connection = None;
            }
        }
        before_state != attempt.state || before_live != attempt.connection.is_some()
    }

    pub fn has_live_connections(&self) -> bool {
        self.sessions
            .values()
            .any(|session| session.connection.is_some())
            || self
                .connection_test
                .as_ref()
                .is_some_and(|attempt| attempt.connection.is_some())
    }

    pub fn has_active_terminal(&self) -> bool {
        self.active_session().is_some()
    }

    pub fn active_accepts_input(&self) -> bool {
        self.active_session()
            .is_some_and(|session| session.state == ConnectionState::Connected)
    }

    pub fn active_blocks_input(&self) -> bool {
        self.active_session()
            .is_some_and(RuntimeSession::blocks_input)
    }

    pub fn active_terminal(&self) -> Option<&Terminal> {
        self.active_session().map(|session| &session.terminal)
    }

    pub fn active_terminal_mut(&mut self) -> Option<&mut Terminal> {
        self.active_session_mut()
            .map(|session| &mut session.terminal)
    }

    pub fn active_cursor(&self) -> Option<(usize, usize, bool)> {
        let grid = self.active_terminal()?.grid();
        Some((grid.cursor.row, grid.cursor.col, grid.cursor.visible))
    }

    pub fn active_view(&self) -> Option<RuntimeView> {
        let profile_id = self.active_profile_id.as_ref()?;
        let session = self.sessions.get(profile_id)?;
        Some(RuntimeView {
            profile_id: profile_id.clone(),
            profile_name: session.profile_name.clone(),
            endpoint: session.endpoint.clone(),
            state: session.state.clone(),
            detail: session.detail.clone(),
            metrics: session.metrics.as_ref().map(format_metrics),
            metrics_error: session.metrics_error.clone(),
            unknown_host_key: session
                .unknown_host_key
                .as_ref()
                .map(|identity| UnknownHostKey {
                    profile_id: profile_id.clone(),
                    algorithm: identity.algorithm.clone(),
                    sha256_fingerprint: identity.sha256_fingerprint.clone(),
                }),
            changed_host_key: session
                .changed_host_key
                .as_ref()
                .map(|(expected, presented)| ChangedHostKey {
                    expected_algorithm: expected.algorithm.clone(),
                    expected_sha256_fingerprint: expected.sha256_fingerprint.clone(),
                    presented_algorithm: presented.algorithm.clone(),
                    presented_sha256_fingerprint: presented.sha256_fingerprint.clone(),
                }),
        })
    }

    pub fn confirm_unknown_host_key(
        &mut self,
        profile_id: &str,
        algorithm: &str,
        fingerprint: &str,
    ) -> bool {
        let Some(session) = self.sessions.get_mut(profile_id) else {
            return false;
        };
        let matches = session.unknown_host_key.as_ref().is_some_and(|identity| {
            identity.algorithm == algorithm && identity.sha256_fingerprint == fingerprint
        });
        if matches {
            session.unknown_host_key = None;
            session.state = ConnectionState::CredentialRequired;
            session.detail = None;
        }
        matches
    }

    /// 主机密钥确认提交前重新核对产生该提示的结构化 endpoint 和指纹。
    ///
    /// `user@host:port` 只用于展示，不能承担安全身份比较：用户名或主机名中的
    /// `@`/`:` 可能让不同字段组合得到相同字符串。
    pub fn unknown_host_key_matches(
        &self,
        profile: &SshProfile,
        algorithm: &str,
        fingerprint: &str,
    ) -> bool {
        self.sessions.get(&profile.id).is_some_and(|session| {
            session.endpoint_identity.matches_profile(profile)
                && session.unknown_host_key.as_ref().is_some_and(|identity| {
                    identity.algorithm == algorithm && identity.sha256_fingerprint == fingerprint
                })
        })
    }

    pub fn dismiss_unknown_host_key(&mut self, profile_id: &str) {
        let Some(session) = self.sessions.get_mut(profile_id) else {
            return;
        };
        session.unknown_host_key = None;
        session.state = ConnectionState::Disconnected;
        session.detail = Some("Host key was not trusted".to_owned());
    }

    fn active_session(&self) -> Option<&RuntimeSession> {
        self.sessions.get(self.active_profile_id.as_ref()?)
    }

    fn active_session_mut(&mut self) -> Option<&mut RuntimeSession> {
        let profile_id = self.active_profile_id.as_ref()?.clone();
        self.sessions.get_mut(&profile_id)
    }
}

fn connection_config(profile: &SshProfile, credential: Credential) -> ConnectionConfig {
    let mut config =
        ConnectionConfig::new(&profile.host, profile.port, &profile.username, credential);
    config.connect_timeout =
        Duration::from_secs(u64::from(profile.connect_timeout_secs.clamp(1, 300)));
    config.keepalive = match profile.keep_alive_secs {
        Some(seconds) => KeepaliveConfig {
            enabled: true,
            // Transport deliberately rejects sub-five-second keepalives.
            interval: Duration::from_secs(u64::from(seconds.clamp(5, 3_600))),
            maximum_missed_replies: 3,
        },
        None => KeepaliveConfig {
            enabled: false,
            ..KeepaliveConfig::default()
        },
    };
    config.metrics = MetricsConfig {
        enabled: profile.monitor_enabled,
        ..MetricsConfig::default()
    };
    config.trusted_host_key = profile.trusted_host_key.as_ref().map(host_key_identity);
    config
}

fn apply_connection_test_event(attempt: &mut ConnectionTestAttempt, event: Event) {
    match event {
        Event::Connecting => {
            attempt.state = ConnectionTestState::Connecting;
        }
        Event::HostKeyUnknown { presented } => {
            attempt.state = ConnectionTestState::AwaitingHostKey {
                algorithm: presented.algorithm,
                fingerprint: presented.sha256_fingerprint,
            };
        }
        Event::HostKeyChanged {
            expected,
            presented,
        } => {
            attempt.state = ConnectionTestState::HostKeyChanged {
                expected_algorithm: expected.algorithm,
                expected_fingerprint: expected.sha256_fingerprint,
                presented_algorithm: presented.algorithm,
                presented_fingerprint: presented.sha256_fingerprint,
            };
        }
        Event::ConnectionTestSucceeded { .. } => {
            attempt.state = ConnectionTestState::Success;
        }
        Event::Error(error) => {
            attempt.state = ConnectionTestState::Error {
                message: error.message,
            };
        }
        Event::Disconnected { reason } => {
            if matches!(attempt.state, ConnectionTestState::Connecting) {
                attempt.state = ConnectionTestState::Error {
                    message: disconnect_reason(reason).to_owned(),
                };
            }
        }
        Event::Connected { .. }
        | Event::Data(_)
        | Event::ExtendedData { .. }
        | Event::Eof
        | Event::ExitStatus(_)
        | Event::ExitSignal { .. }
        | Event::Metrics(_)
        | Event::MetricsError { .. } => {
            attempt.state = ConnectionTestState::Error {
                message: "SSH connection test received an unexpected shell event".to_owned(),
            };
        }
    }
}

fn event_is_saved_credential_failure(event: &Event) -> bool {
    matches!(
        event,
        Event::Error(error)
            if matches!(
                error.kind,
                EventErrorKind::Authentication | EventErrorKind::PrivateKey
            )
    )
}

fn apply_event(session: &mut RuntimeSession, event: Event) -> bool {
    match event {
        Event::Connecting => {
            session.state = ConnectionState::Connecting;
            session.detail = None;
            false
        }
        Event::HostKeyUnknown { presented } => {
            session.state = ConnectionState::AwaitingHostKey;
            session.unknown_host_key = Some(presented);
            session.changed_host_key = None;
            session.detail = None;
            false
        }
        Event::HostKeyChanged {
            expected,
            presented,
        } => {
            session.state = ConnectionState::HostKeyChanged;
            session.unknown_host_key = None;
            session.changed_host_key = Some((expected, presented));
            session.detail = Some("The server host key changed; connection was blocked".to_owned());
            false
        }
        Event::Connected { .. } => {
            session.state = ConnectionState::Connected;
            session.detail = None;
            false
        }
        Event::ConnectionTestSucceeded { .. } => {
            session.state = ConnectionState::Error;
            session.detail =
                Some("An SSH connection test event reached an interactive session".to_owned());
            false
        }
        Event::Data(bytes) | Event::ExtendedData { data: bytes, .. } => {
            session.terminal.advance(&bytes);
            true
        }
        Event::Eof => {
            session.detail = Some("SSH shell reached end of stream".to_owned());
            false
        }
        Event::ExitStatus(status) => {
            session.detail = Some(format!("SSH shell exited with status {status}"));
            false
        }
        Event::ExitSignal { signal, .. } => {
            session.detail = Some(format!("SSH shell exited after signal {signal}"));
            false
        }
        Event::Metrics(metrics) => {
            session.metrics = Some(metrics);
            session.metrics_error = None;
            false
        }
        Event::MetricsError { message } => {
            // Monitoring is explicitly non-fatal: retain the terminal and the
            // last good sample while surfacing the monitor error separately.
            session.metrics_error = Some(message);
            false
        }
        Event::Error(error) => {
            session.state = ConnectionState::Error;
            session.detail = Some(error.message);
            false
        }
        Event::Disconnected { reason } => {
            session.connection.take();
            if !matches!(
                session.state,
                ConnectionState::AwaitingHostKey
                    | ConnectionState::HostKeyChanged
                    | ConnectionState::Error
            ) {
                session.state = ConnectionState::Disconnected;
                if session.detail.is_none() && reason != DisconnectReason::Requested {
                    session.detail = Some(disconnect_reason(reason).to_owned());
                }
            }
            false
        }
    }
}

fn reply_to_terminal_queries(session: &mut RuntimeSession) {
    let responses = session.terminal.take_responses();
    if responses.is_empty() {
        return;
    }
    let Some(connection) = &session.connection else {
        return;
    };
    if let Err(error) = connection.send(Command::Input(responses)) {
        session.state = ConnectionState::Error;
        session.detail = Some(format!("Could not answer terminal query: {error}"));
    }
}

fn terminal_size(rows: usize, columns: usize) -> TerminalSize {
    TerminalSize::new(
        u32::try_from(columns).unwrap_or(10_000).clamp(1, 10_000),
        u32::try_from(rows).unwrap_or(10_000).clamp(1, 10_000),
    )
}

fn host_key_identity(trust: &HostKeyTrust) -> HostKeyIdentity {
    HostKeyIdentity::new(&trust.algorithm, &trust.fingerprint)
}

fn endpoint(profile: &SshProfile) -> String {
    format!("{}@{}:{}", profile.username, profile.host, profile.port)
}

fn disconnect_reason(reason: DisconnectReason) -> &'static str {
    match reason {
        DisconnectReason::Requested => "SSH connection closed",
        DisconnectReason::HandleDropped => "SSH connection handle closed",
        DisconnectReason::HostKeyUnknown => "SSH host key is awaiting confirmation",
        DisconnectReason::HostKeyChanged => "SSH host key changed",
        DisconnectReason::AuthenticationFailed => "SSH authentication failed",
        DisconnectReason::ConnectionFailed => "SSH connection failed",
        DisconnectReason::ShellClosed => "SSH shell closed",
        DisconnectReason::ShellExited => "SSH shell exited",
        DisconnectReason::EventBackpressure => "SSH event queue exceeded its safe boundary",
    }
}

pub fn should_route_terminal_input(
    view_mode: ViewMode,
    terminal_focused: bool,
    modal_open: bool,
    active_connected: bool,
) -> bool {
    view_mode.is_ssh() && terminal_focused && !modal_open && active_connected
}

pub fn format_metrics(metrics: &ServerMetrics) -> MetricsSummary {
    let cpu = metrics
        .cpu_usage_percent
        .map_or_else(|| "—".to_owned(), |value| format!("{value:.1}%"));
    let memory = used_total(metrics.memory.used_bytes, metrics.memory.total_bytes);
    let root_disk = used_total(metrics.root_disk.used_bytes, metrics.root_disk.total_bytes);
    let load = format!(
        "{:.2} / {:.2} / {:.2}",
        metrics.load_average_1m, metrics.load_average_5m, metrics.load_average_15m
    );
    let receive = metrics
        .network
        .receive_bytes_per_second
        .map_or_else(|| "—".to_owned(), rate);
    let transmit = metrics
        .network
        .transmit_bytes_per_second
        .map_or_else(|| "—".to_owned(), rate);
    MetricsSummary {
        cpu,
        memory,
        load,
        root_disk,
        network: format!("↓ {receive}  ↑ {transmit}"),
        uptime: uptime(metrics.uptime_seconds),
    }
}

fn used_total(used: u64, total: u64) -> String {
    let percentage = if total == 0 {
        0.0
    } else {
        used as f64 * 100.0 / total as f64
    };
    format!(
        "{} / {} ({percentage:.1}%)",
        byte_size(used as f64),
        byte_size(total as f64)
    )
}

fn rate(bytes_per_second: f64) -> String {
    format!("{}/s", byte_size(bytes_per_second.max(0.0)))
}

fn byte_size(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn uptime(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_ssh::{NetworkMetrics, StorageMetrics, SystemMemoryMetrics};

    fn profile(auth_method: AuthMethod) -> SshProfile {
        SshProfile {
            id: "ssh_0123456789abcdef0123456789abcdef".to_owned(),
            name: "dev".to_owned(),
            host: "server.example".to_owned(),
            port: 22,
            username: "alice".to_owned(),
            auth_method,
            group_id: None,
            sort_order: 0,
            initial_directory: None,
            connect_timeout_secs: 15,
            keep_alive_secs: Some(30),
            monitor_enabled: true,
            trusted_host_key: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn input_is_never_routed_to_ssh_from_other_modes_or_modals() {
        assert!(!should_route_terminal_input(
            ViewMode::Local,
            true,
            false,
            true
        ));
        assert!(!should_route_terminal_input(
            ViewMode::Remote,
            true,
            false,
            true
        ));
        assert!(!should_route_terminal_input(
            ViewMode::Ssh,
            true,
            true,
            true
        ));
        assert!(should_route_terminal_input(
            ViewMode::Ssh,
            true,
            false,
            true
        ));
    }

    #[test]
    fn connection_test_view_requires_exact_structured_target() {
        let runtime = SshRuntime {
            connection_test: Some(ConnectionTestAttempt {
                form_id: 7,
                connection_revision: 3,
                endpoint_identity: EndpointIdentity {
                    host: "b@c".to_owned(),
                    port: 22,
                    username: "a".to_owned(),
                },
                connection: None,
                state: ConnectionTestState::Connecting,
            }),
            ..SshRuntime::default()
        };
        let view = runtime.connection_test_view().expect("test view");
        assert_eq!(view.connection_revision, 3);
        assert!(view.matches_target(7, "b@c", 22, "a"));
        assert!(!view.matches_target(7, "c", 22, "a@b"));
        assert!(!view.matches_target(8, "b@c", 22, "a"));
    }

    #[test]
    fn probe_result_preserves_host_key_and_success_across_disconnect() {
        let mut attempt = ConnectionTestAttempt {
            form_id: 9,
            connection_revision: 4,
            endpoint_identity: EndpointIdentity {
                host: "server.example".to_owned(),
                port: 22,
                username: "alice".to_owned(),
            },
            connection: None,
            state: ConnectionTestState::Connecting,
        };
        apply_connection_test_event(
            &mut attempt,
            Event::HostKeyUnknown {
                presented: HostKeyIdentity::new(
                    "ssh-ed25519",
                    "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABC",
                ),
            },
        );
        assert!(matches!(
            attempt.state,
            ConnectionTestState::AwaitingHostKey { .. }
        ));
        apply_connection_test_event(
            &mut attempt,
            Event::Disconnected {
                reason: DisconnectReason::HostKeyUnknown,
            },
        );
        assert!(matches!(
            attempt.state,
            ConnectionTestState::AwaitingHostKey { .. }
        ));

        apply_connection_test_event(
            &mut attempt,
            Event::ConnectionTestSucceeded {
                host_key: HostKeyIdentity::new(
                    "ssh-ed25519",
                    "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABC",
                ),
            },
        );
        apply_connection_test_event(
            &mut attempt,
            Event::Disconnected {
                reason: DisconnectReason::Requested,
            },
        );
        assert_eq!(attempt.state, ConnectionTestState::Success);
    }

    #[test]
    fn host_key_state_requires_exact_confirmation_and_never_accepts_changed_key() {
        let p = profile(AuthMethod::Agent);
        let mut runtime = SshRuntime::default();
        assert_eq!(runtime.select_for_connect(&p), ConnectIntent::Agent);
        let session = runtime.sessions.get_mut(&p.id).expect("session");
        let unknown = HostKeyIdentity::new(
            "ssh-ed25519",
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABC",
        );
        apply_event(
            session,
            Event::HostKeyUnknown {
                presented: unknown.clone(),
            },
        );
        assert_eq!(session.state, ConnectionState::AwaitingHostKey);
        assert!(!runtime.confirm_unknown_host_key(&p.id, "ssh-ed25519", "SHA256:wrong"));
        assert!(runtime.confirm_unknown_host_key(
            &p.id,
            &unknown.algorithm,
            &unknown.sha256_fingerprint
        ));

        let session = runtime.sessions.get_mut(&p.id).expect("session");
        apply_event(
            session,
            Event::HostKeyChanged {
                expected: unknown,
                presented: HostKeyIdentity::new(
                    "ssh-ed25519",
                    "SHA256:ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abc",
                ),
            },
        );
        assert_eq!(session.state, ConnectionState::HostKeyChanged);
        assert!(!runtime.confirm_unknown_host_key(
            &p.id,
            "ssh-ed25519",
            "SHA256:ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abc"
        ));
    }

    #[test]
    fn 主机密钥确认按结构化endpoint拒绝展示字符串碰撞() {
        let mut original = profile(AuthMethod::Agent);
        original.username = "a".to_owned();
        original.host = "b@c".to_owned();
        let mut colliding = original.clone();
        colliding.username = "a@b".to_owned();
        colliding.host = "c".to_owned();
        assert_eq!(
            endpoint(&original),
            endpoint(&colliding),
            "回归样例必须能碰撞旧展示字符串"
        );

        let mut runtime = SshRuntime::default();
        assert_eq!(runtime.select_for_connect(&original), ConnectIntent::Agent);
        let presented = HostKeyIdentity::new(
            "ssh-ed25519",
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABC",
        );
        let session = runtime.sessions.get_mut(&original.id).expect("session");
        apply_event(
            session,
            Event::HostKeyUnknown {
                presented: presented.clone(),
            },
        );
        assert!(runtime.unknown_host_key_matches(
            &original,
            &presented.algorithm,
            &presented.sha256_fingerprint,
        ));
        assert_eq!(
            runtime.select_for_connect(&colliding),
            ConnectIntent::AwaitingHostKey,
            "重新点击连接不得把未决指纹改绑到新 endpoint"
        );
        assert!(
            !runtime.unknown_host_key_matches(
                &colliding,
                &presented.algorithm,
                &presented.sha256_fingerprint,
            ),
            "host/port/username 任一字段变化都必须让旧确认失效"
        );
    }

    #[test]
    fn metrics_summary_formats_all_required_monitor_fields() {
        let summary = format_metrics(&ServerMetrics {
            cpu_usage_percent: Some(12.5),
            load_average_1m: 0.1,
            load_average_5m: 0.2,
            load_average_15m: 0.3,
            memory: SystemMemoryMetrics {
                total_bytes: 8 * 1024 * 1024 * 1024,
                used_bytes: 2 * 1024 * 1024 * 1024,
                available_bytes: 6 * 1024 * 1024 * 1024,
            },
            root_disk: StorageMetrics {
                total_bytes: 100 * 1024 * 1024 * 1024,
                used_bytes: 25 * 1024 * 1024 * 1024,
                available_bytes: 75 * 1024 * 1024 * 1024,
            },
            network: NetworkMetrics {
                received_bytes: 1,
                transmitted_bytes: 2,
                receive_bytes_per_second: Some(1024.0),
                transmit_bytes_per_second: Some(2.0 * 1024.0 * 1024.0),
            },
            uptime_seconds: 183_900.0,
        });
        assert_eq!(summary.cpu, "12.5%");
        assert!(summary.memory.contains("25.0%"));
        assert_eq!(summary.load, "0.10 / 0.20 / 0.30");
        assert!(summary.root_disk.contains("25.0%"));
        assert_eq!(summary.network, "↓ 1.0 KiB/s  ↑ 2.0 MiB/s");
        assert_eq!(summary.uptime, "2d 3h 5m");
    }

    #[test]
    fn each_profile_keeps_an_independent_background_session() {
        let first = profile(AuthMethod::Password);
        let mut second = first.clone();
        second.id = "ssh_abcdef0123456789abcdef0123456789".to_owned();
        second.name = "prod".to_owned();
        let mut runtime = SshRuntime::default();
        assert_eq!(runtime.select_for_connect(&first), ConnectIntent::Password);
        assert_eq!(runtime.select_for_connect(&second), ConnectIntent::Password);
        assert_eq!(runtime.sessions.len(), 2);
        assert_eq!(
            runtime.active_profile_id.as_deref(),
            Some(second.id.as_str())
        );
    }

    #[test]
    fn only_password_authentication_and_private_key_errors_invalidate_saved_credentials() {
        for kind in [EventErrorKind::Authentication, EventErrorKind::PrivateKey] {
            assert!(event_is_saved_credential_failure(&Event::Error(
                lumen_ssh::EventError {
                    kind,
                    message: "safe fixed message".to_owned(),
                }
            )));
        }
        for kind in [
            EventErrorKind::Runtime,
            EventErrorKind::Network,
            EventErrorKind::Agent,
            EventErrorKind::Channel,
            EventErrorKind::EventBackpressure,
        ] {
            assert!(!event_is_saved_credential_failure(&Event::Error(
                lumen_ssh::EventError {
                    kind,
                    message: "safe fixed message".to_owned(),
                }
            )));
        }
    }
}
