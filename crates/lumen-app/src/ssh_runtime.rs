//! Main-thread adapter between the SSH transport actor and Lumen's terminal.
//!
//! Each session owns an independent transport and terminal state. Multiple
//! sessions may reference the same server profile; switching sessions or
//! application modes never tears down the other background connections.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lumen_ssh::{
    Command, ConnectionConfig, ConnectionMode, Credential, DirectoryEntry, DirectoryEntryKind,
    DirectoryError, DisconnectReason, Event, EventErrorKind, FileCommand, FileError, FileEvent,
    FileOperation, FileVersion, HostKeyIdentity, KeepaliveConfig, KillStatus, ManageAction,
    ManageOutcome, ManageRequest, MetricsConfig, ServerMetrics, SshConnection, TerminalSize,
};
use lumen_term::Terminal;

use crate::settings::ViewMode;
use crate::ssh::{AuthMethod, HostKeyTrust, SshProfile};

const DEFAULT_ROWS: usize = 36;
const DEFAULT_COLUMNS: usize = 120;
const SSH_SCROLLBACK: usize = 10_000;
const MONITOR_HISTORY_SAMPLES: usize = 60;
const SSH_CWD_REPORT_PREFIX: &[u8] = b"\x1b]9;9;";

pub type SshSessionId = u64;
pub type SshRuntimeId = u64;

static NEXT_SSH_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_runtime_id() -> SshRuntimeId {
    loop {
        let runtime_id = NEXT_SSH_RUNTIME_ID.fetch_add(1, Ordering::Relaxed);
        if runtime_id != 0 {
            return runtime_id;
        }
    }
}

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
    pub session_id: SshSessionId,
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

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeView {
    pub session_id: SshSessionId,
    pub profile_id: String,
    pub profile_name: String,
    pub endpoint: String,
    pub state: ConnectionState,
    pub detail: Option<String>,
    pub metrics: Option<MetricsSummary>,
    pub monitor: Option<MonitorView>,
    pub metrics_error: Option<String>,
    pub unknown_host_key: Option<UnknownHostKey>,
    pub changed_host_key: Option<ChangedHostKey>,
    pub process_search: Option<ProcessSearchView>,
    pub port_lookup: Option<PortLookupView>,
    pub kill_feedback: Option<KillFeedbackView>,
}

/// 进程名称搜索的展示状态（结果来自远端一次性 exec，不进入监控采样）。
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSearchView {
    pub query: String,
    pub loading: bool,
    pub error: bool,
    pub results: Vec<SshProcessEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SshProcessEntry {
    pub pid: u32,
    pub cpu_percent: f32,
    pub memory_percent: f32,
    pub command: String,
}

/// 端口占用查询的展示状态。`pid: None` 表示远端工具对其它用户的进程
/// 不可见（无权限），不是「没有进程」。
#[derive(Clone, Debug, PartialEq)]
pub struct PortLookupView {
    pub port: u16,
    pub loading: bool,
    pub error: bool,
    pub entries: Vec<SshPortEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshPortEntry {
    pub protocol: String,
    pub local_address: String,
    pub pid: Option<u32>,
    pub command: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KillFeedbackView {
    pub pid: u32,
    pub status: SshKillStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshKillStatus {
    Signalled,
    PermissionDenied,
    NoSuchProcess,
    /// 传输/通道层失败（未送达信号），与「权限不足」区分。
    Failed,
}

/// 系统监控面板使用的结构化快照与短期趋势。原始采样只保存在内存，
/// 不参与 SSH 配置同步。
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorView {
    pub metrics: ServerMetrics,
    pub cpu_history: Vec<f32>,
    pub receive_history: Vec<f64>,
    pub transmit_history: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshFileTreeRow {
    pub name: String,
    pub path: String,
    pub kind: DirectoryEntryKind,
    pub size: u64,
    pub depth: usize,
    pub expanded: bool,
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshFileTreeView {
    pub session_id: SshSessionId,
    pub profile_id: String,
    pub root: String,
    pub rows: Vec<SshFileTreeRow>,
    pub loading: bool,
    pub truncated: bool,
    pub error: Option<String>,
    pub search_query: Option<String>,
    pub search_rows: Vec<SshFileTreeRow>,
    pub search_loading: bool,
    pub search_truncated: bool,
    pub search_error: Option<String>,
}

/// 独立 SFTP 通道完成后交给主线程的结果。路径和正文不进入 `Debug`。
#[derive(Clone, PartialEq, Eq)]
pub enum SshFileRuntimeEvent {
    TextLoaded {
        session_id: SshSessionId,
        document_token: u64,
        bytes: Vec<u8>,
        version: FileVersion,
    },
    TextSaved {
        session_id: SshSessionId,
        document_token: u64,
        version: FileVersion,
    },
    TextConflict {
        session_id: SshSessionId,
        document_token: u64,
    },
    LocalCopyReady {
        session_id: SshSessionId,
        local_path: PathBuf,
    },
    OperationComplete {
        session_id: SshSessionId,
        operation: FileOperation,
        local_path: Option<PathBuf>,
    },
    Error {
        session_id: SshSessionId,
        document_token: Option<u64>,
        operation: SshFileAction,
        error: FileError,
        local_path: Option<PathBuf>,
    },
}

impl std::fmt::Debug for SshFileRuntimeEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TextLoaded {
                session_id,
                document_token,
                bytes,
                version,
            } => formatter
                .debug_struct("TextLoaded")
                .field("session_id", session_id)
                .field("document_token", document_token)
                .field("bytes", &bytes.len())
                .field("version", version)
                .finish(),
            Self::TextSaved {
                session_id,
                document_token,
                version,
            } => formatter
                .debug_struct("TextSaved")
                .field("session_id", session_id)
                .field("document_token", document_token)
                .field("version", version)
                .finish(),
            Self::TextConflict {
                session_id,
                document_token,
            } => formatter
                .debug_struct("TextConflict")
                .field("session_id", session_id)
                .field("document_token", document_token)
                .finish(),
            Self::LocalCopyReady {
                session_id,
                local_path: _,
            } => formatter
                .debug_struct("LocalCopyReady")
                .field("session_id", session_id)
                .field("local_path", &"<redacted>")
                .finish(),
            Self::OperationComplete {
                session_id,
                operation,
                local_path,
            } => formatter
                .debug_struct("OperationComplete")
                .field("session_id", session_id)
                .field("operation", operation)
                .field(
                    "local_path",
                    &local_path.as_ref().map(|_| "<redacted>"),
                )
                .finish(),
            Self::Error {
                session_id,
                document_token,
                operation,
                error,
                local_path,
            } => formatter
                .debug_struct("Error")
                .field("session_id", session_id)
                .field("document_token", document_token)
                .field("operation", operation)
                .field("error", error)
                .field("local_path", &local_path.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SshFileAction {
    Search,
    ReadText,
    WriteText,
    OpenLocalCopy,
    CreateDirectory,
    CreateFile,
    Rename,
    Delete,
    Download,
    Upload,
}

/// SSH 会话栏的一项。仅包含公开连接元数据与状态，不包含终端或凭据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshSessionView {
    pub session_id: SshSessionId,
    pub profile_id: String,
    pub profile_name: String,
    pub display_name: String,
    pub endpoint: String,
    pub state: ConnectionState,
    pub active: bool,
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
    /// 任一会话的连接态或 cwd 发生变化；用于刷新 SSH 会话栏中的状态点和默认名称。
    pub sessions_changed: bool,
    /// 本批 SFTP 操作结果；主线程消费后更新编辑器、打开本地副本或显示提示。
    pub file_events: Vec<SshFileRuntimeEvent>,
    /// 已断开、关闭或重连的会话；对应编辑器来源必须立即失效。
    pub editor_invalidated_sessions: Vec<SshSessionId>,
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
    profile_id: String,
    auth_method: AuthMethod,
    profile_name: String,
    /// 用户手动设置的会话名。`None` 时名称跟随当前 Linux cwd 的尾目录名。
    custom_title: Option<String>,
    endpoint: String,
    endpoint_identity: EndpointIdentity,
    terminal: Terminal,
    /// 最近一次完整 shell 提示符钩子已经上报 cwd，且此后尚未提交新命令。
    shell_idle: bool,
    /// 跨 transport 数据块识别 OSC 9;9 前缀所需的短尾巴。
    cwd_report_scan_tail: Vec<u8>,
    connection: Option<SshConnection>,
    state: ConnectionState,
    detail: Option<String>,
    metrics: Option<ServerMetrics>,
    cpu_history: VecDeque<f32>,
    receive_history: VecDeque<f64>,
    transmit_history: VecDeque<f64>,
    metrics_error: Option<String>,
    unknown_host_key: Option<HostKeyIdentity>,
    changed_host_key: Option<(HostKeyIdentity, HostKeyIdentity)>,
    file_tree_root: String,
    directory_listings: HashMap<String, DirectoryListing>,
    open_directories: HashSet<String>,
    pending_directories: HashMap<u64, String>,
    latest_directory_request: HashMap<String, u64>,
    next_directory_token: u64,
    file_tree_error: Option<String>,
    selected_file: Option<(String, bool)>,
    next_file_token: u64,
    pending_file_requests: HashMap<u64, PendingFileRequest>,
    editor_versions: HashMap<u64, EditorVersion>,
    search_query: Option<String>,
    search_entries: Vec<DirectoryEntry>,
    search_loading: bool,
    search_truncated: bool,
    search_error: Option<String>,
    latest_search_token: Option<u64>,
    next_manage_token: u64,
    pending_manage: HashMap<u64, PendingManage>,
    process_search: Option<ProcessSearchState>,
    port_lookup: Option<PortLookupState>,
    kill_feedback: Option<KillFeedbackState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingManage {
    Kill { pid: u32 },
    QueryPort,
    QueryProcess,
}

struct ProcessSearchState {
    query: String,
    loading: bool,
    error: bool,
    results: Vec<lumen_ssh::ProcessEntry>,
}

struct PortLookupState {
    port: u16,
    loading: bool,
    error: bool,
    entries: Vec<lumen_ssh::PortEntry>,
}

#[derive(Clone, Copy)]
struct KillFeedbackState {
    pid: u32,
    /// `None` 表示通道层失败（信号未送达）。
    status: Option<KillStatus>,
}

#[derive(Clone)]
struct DirectoryListing {
    entries: Vec<DirectoryEntry>,
    truncated: bool,
}

enum PendingFileRequest {
    Search {
        query: String,
    },
    TextRead {
        document_token: u64,
        path: String,
    },
    TextWrite {
        document_token: u64,
        path: String,
    },
    OpenLocalCopy {
        local_path: PathBuf,
    },
    Operation {
        action: SshFileAction,
        refresh_directories: Vec<String>,
        invalidate_subtree: Option<String>,
        local_path: Option<PathBuf>,
    },
}

struct EditorVersion {
    path: String,
    version: FileVersion,
}

impl RuntimeSession {
    fn new(profile: &SshProfile) -> Self {
        Self {
            profile_id: profile.id.clone(),
            auth_method: profile.auth_method,
            profile_name: profile.name.clone(),
            custom_title: None,
            endpoint: endpoint(profile),
            endpoint_identity: EndpointIdentity::from_profile(profile),
            terminal: Terminal::new(DEFAULT_ROWS, DEFAULT_COLUMNS, SSH_SCROLLBACK),
            shell_idle: false,
            cwd_report_scan_tail: Vec::with_capacity(SSH_CWD_REPORT_PREFIX.len() - 1),
            connection: None,
            state: ConnectionState::CredentialRequired,
            detail: None,
            metrics: None,
            cpu_history: VecDeque::with_capacity(MONITOR_HISTORY_SAMPLES),
            receive_history: VecDeque::with_capacity(MONITOR_HISTORY_SAMPLES),
            transmit_history: VecDeque::with_capacity(MONITOR_HISTORY_SAMPLES),
            metrics_error: None,
            unknown_host_key: None,
            changed_host_key: None,
            file_tree_root: ssh_file_tree_root(profile),
            directory_listings: HashMap::new(),
            open_directories: HashSet::new(),
            pending_directories: HashMap::new(),
            latest_directory_request: HashMap::new(),
            next_directory_token: 1,
            file_tree_error: None,
            selected_file: None,
            next_file_token: 1,
            pending_file_requests: HashMap::new(),
            editor_versions: HashMap::new(),
            search_query: None,
            search_entries: Vec::new(),
            search_loading: false,
            search_truncated: false,
            search_error: None,
            latest_search_token: None,
            next_manage_token: 1,
            pending_manage: HashMap::new(),
            process_search: None,
            port_lookup: None,
            kill_feedback: None,
        }
    }

    fn display_name(&self) -> String {
        self.custom_title
            .clone()
            .unwrap_or_else(|| linux_basename(&self.file_tree_root).to_owned())
    }

    fn refresh_metadata(&mut self, profile: &SshProfile) {
        self.auth_method = profile.auth_method;
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

    fn reset_file_tree(&mut self, profile: &SshProfile) {
        for token in self.pending_directories.keys().copied().collect::<Vec<_>>() {
            if let Some(connection) = &self.connection {
                let _ = connection.send(Command::CancelDirectory { token });
            }
        }
        self.file_tree_root = ssh_file_tree_root(profile);
        self.directory_listings.clear();
        self.open_directories.clear();
        self.open_directories.insert(self.file_tree_root.clone());
        self.pending_directories.clear();
        self.latest_directory_request.clear();
        self.file_tree_error = None;
        self.selected_file = None;
        cancel_file_requests(self);
        self.editor_versions.clear();
        self.search_query = None;
        self.search_entries.clear();
        self.search_loading = false;
        self.search_truncated = false;
        self.search_error = None;
        self.latest_search_token = None;
    }
}

struct ConnectionTestAttempt {
    form_id: u64,
    connection_revision: u64,
    endpoint_identity: EndpointIdentity,
    connection: Option<SshConnection>,
    state: ConnectionTestState,
}

pub struct SshRuntime {
    runtime_id: SshRuntimeId,
    sessions: HashMap<SshSessionId, RuntimeSession>,
    session_order: Vec<SshSessionId>,
    active_session_id: Option<SshSessionId>,
    next_session_id: SshSessionId,
    connection_test: Option<ConnectionTestAttempt>,
    pending_editor_invalidations: Vec<SshSessionId>,
}

impl Default for SshRuntime {
    fn default() -> Self {
        Self {
            runtime_id: allocate_runtime_id(),
            sessions: HashMap::new(),
            session_order: Vec::new(),
            active_session_id: None,
            next_session_id: 1,
            connection_test: None,
            pending_editor_invalidations: Vec::new(),
        }
    }
}

impl SshRuntime {
    #[must_use]
    pub const fn runtime_id(&self) -> SshRuntimeId {
        self.runtime_id
    }

    #[must_use]
    pub fn contains_session(&self, session_id: SshSessionId) -> bool {
        self.sessions.contains_key(&session_id)
    }

    fn invalidate_editor_session(&mut self, session_id: SshSessionId) {
        if !self.pending_editor_invalidations.contains(&session_id) {
            self.pending_editor_invalidations.push(session_id);
        }
    }

    /// 新建并激活一个独立会话，然后返回其认证意图。
    ///
    /// 即使同一个 profile 已有连接，本方法也总会分配新的会话。这样服务器
    /// 双击与“新增会话”都能打开独立的远端 shell。
    pub fn select_for_connect(&mut self, profile: &SshProfile) -> (SshSessionId, ConnectIntent) {
        let session_id = self.create_session(profile);
        let intent = self
            .connect_intent(session_id, profile)
            .expect("刚插入的 SSH 会话必须存在且属于当前 profile");
        (session_id, intent)
    }

    /// 获取一个既有会话下一步的连接意图。
    ///
    /// 该入口供主机密钥确认后继续同一会话使用；它不会创建或替换会话。
    pub fn connect_intent(
        &mut self,
        session_id: SshSessionId,
        profile: &SshProfile,
    ) -> Option<ConnectIntent> {
        let session = self.sessions.get_mut(&session_id)?;
        if session.profile_id != profile.id {
            return None;
        }
        self.active_session_id = Some(session_id);

        if session.is_running() {
            return Some(ConnectIntent::AlreadyRunning);
        }
        // 未决主机密钥属于产生它的旧 endpoint。配置在弹窗期间被编辑时，
        // 不能先用新 profile 刷新身份再复用旧指纹提示。
        if session.unknown_host_key.is_some() {
            return Some(ConnectIntent::AwaitingHostKey);
        }
        if session.changed_host_key.is_some() {
            return Some(ConnectIntent::HostKeyChanged);
        }
        session.refresh_metadata(profile);
        session.state = ConnectionState::CredentialRequired;
        session.detail = None;
        session.shell_idle = false;
        Some(match profile.auth_method {
            AuthMethod::Password => ConnectIntent::Password,
            AuthMethod::PrivateKey => ConnectIntent::PrivateKey,
            AuthMethod::Agent => ConnectIntent::Agent,
        })
    }

    /// 凭据提交前复核它仍指向原来的待认证会话。
    ///
    /// UI 对话框可以跨帧存在；期间会话可能被关闭，服务器配置也可能被
    /// 编辑。秘密写入本机安全存储前必须先通过这里的结构化检查。
    #[must_use]
    pub fn accepts_credential_for(
        &self,
        session_id: SshSessionId,
        profile: &SshProfile,
    ) -> bool {
        self.sessions.get(&session_id).is_some_and(|session| {
            session.profile_id == profile.id
                && session.endpoint_identity.matches_profile(profile)
                && session.auth_method == profile.auth_method
                && session.state == ConnectionState::CredentialRequired
                && session.unknown_host_key.is_none()
                && session.changed_host_key.is_none()
        })
    }

    pub fn start(
        &mut self,
        session_id: SshSessionId,
        profile: &SshProfile,
        credential: Credential,
    ) -> Result<(), String> {
        if !self.accepts_credential_for(session_id, profile) {
            return Err("SSH credential request is stale".to_owned());
        }
        self.invalidate_editor_session(session_id);
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        if session.profile_id != profile.id {
            return Err("SSH session does not belong to this profile".to_owned());
        }
        self.active_session_id = Some(session_id);
        session.refresh_metadata(profile);

        // Replacing one profile's failed attempt does not affect any other
        // profile. Dropping the old handle only signals its dedicated actor.
        session.connection.take();
        session.unknown_host_key = None;
        session.changed_host_key = None;
        session.detail = None;
        session.metrics = None;
        session.metrics_error = None;
        session.cpu_history.clear();
        session.receive_history.clear();
        session.transmit_history.clear();
        session.shell_idle = false;
        session.cwd_report_scan_tail.clear();
        session.reset_file_tree(profile);

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
                    session.shell_idle = false;
                }
                Err(error) => {
                    session.state = ConnectionState::Error;
                    session.detail = Some(error.to_string());
                    session.shell_idle = false;
                }
            }
        }
    }

    pub fn remove_profile(&mut self, profile_id: &str) {
        let session_ids = self
            .session_order
            .iter()
            .copied()
            .filter(|session_id| {
                self.sessions
                    .get(session_id)
                    .is_some_and(|session| session.profile_id == profile_id)
            })
            .collect::<Vec<_>>();
        for session_id in session_ids {
            self.close_session(session_id);
        }
    }

    /// 激活一个已打开的 SSH 会话；不会重连或改变其他后台连接。
    pub fn activate_session(&mut self, session_id: SshSessionId) -> bool {
        if !self.sessions.contains_key(&session_id) {
            return false;
        }
        self.active_session_id = Some(session_id);
        true
    }

    /// 设置一个已打开 SSH 会话的自定义名称。
    ///
    /// 非空名称经首尾去空白后固定，不再随 cwd 变化；空白名称清除
    /// 自定义值，立即恢复为当前 cwd 尾目录名并继续自动跟随。
    pub fn rename_session(&mut self, session_id: SshSessionId, name: &str) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        let name = name.trim();
        session.custom_title = (!name.is_empty()).then(|| name.to_owned());
        true
    }

    /// 关闭会话但保留服务器配置。若关闭当前会话，选择相邻会话。
    pub fn close_session(&mut self, session_id: SshSessionId) -> bool {
        let Some(index) = self
            .session_order
            .iter()
            .position(|existing| *existing == session_id)
        else {
            return false;
        };
        self.invalidate_editor_session(session_id);
        // Dropping the handle signals cancellation to this session's dedicated
        // actor. Other sessions, including the same profile's, remain untouched.
        self.sessions.remove(&session_id);
        self.session_order.remove(index);
        if self.active_session_id == Some(session_id) {
            self.active_session_id = self
                .session_order
                .get(index.min(self.session_order.len().saturating_sub(1)))
                .copied();
        }
        true
    }

    #[must_use]
    pub fn session_views(&self) -> Vec<SshSessionView> {
        self.session_order
            .iter()
            .filter_map(|session_id| {
                let session = self.sessions.get(session_id)?;
                Some(SshSessionView {
                    session_id: *session_id,
                    profile_id: session.profile_id.clone(),
                    profile_name: session.profile_name.clone(),
                    display_name: session.display_name(),
                    endpoint: session.endpoint.clone(),
                    state: session.state.clone(),
                    active: self.active_session_id.as_ref() == Some(session_id),
                })
            })
            .collect()
    }

    #[must_use]
    pub fn active_file_tree_view(&self) -> Option<SshFileTreeView> {
        let session_id = self.active_session_id?;
        let session = self.sessions.get(&session_id)?;
        let mut rows = Vec::new();
        append_file_tree_rows(session, &session.file_tree_root, 0, &mut rows);
        let search_rows = session
            .search_entries
            .iter()
            .map(|entry| SshFileTreeRow {
                name: entry.name.clone(),
                path: entry.path.clone(),
                kind: entry.kind,
                size: entry.size,
                depth: 0,
                expanded: false,
                loading: false,
            })
            .collect();
        let truncated = session
            .directory_listings
            .iter()
            .any(|(path, listing)| session.open_directories.contains(path) && listing.truncated);
        Some(SshFileTreeView {
            session_id,
            profile_id: session.profile_id.clone(),
            root: session.file_tree_root.clone(),
            rows,
            loading: !session.pending_directories.is_empty(),
            truncated,
            error: session.file_tree_error.clone(),
            search_query: session.search_query.clone(),
            search_rows,
            search_loading: session.search_loading,
            search_truncated: session.search_truncated,
            search_error: session.search_error.clone(),
        })
    }

    pub fn select_file(
        &mut self,
        session_id: SshSessionId,
        path: String,
        is_directory: bool,
    ) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        if !known_tree_path(session, &path, is_directory) {
            return false;
        }
        session.selected_file = Some((path, is_directory));
        true
    }

    pub fn clear_file_selection(&mut self, session_id: SshSessionId) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        session.selected_file = None;
        true
    }

    #[must_use]
    pub fn active_selected_file(&self) -> Option<(SshSessionId, String, String, bool, u64)> {
        let session_id = self.active_session_id?;
        let session = self.sessions.get(&session_id)?;
        let (path, is_directory) = session.selected_file.as_ref()?;
        let (name, size) = tree_entry_metadata(session, path)
            .unwrap_or_else(|| (linux_basename(path).to_owned(), 0));
        Some((session_id, path.clone(), name, *is_directory, size))
    }

    #[must_use]
    pub fn active_paste_target(&self) -> Option<(SshSessionId, String)> {
        let session_id = self.active_session_id?;
        let session = self.sessions.get(&session_id)?;
        let directory = match session.selected_file.as_ref() {
            Some((path, true)) if known_directory(session, path) => path.clone(),
            Some((path, false)) => linux_parent(path)
                .filter(|parent| known_directory(session, parent))
                .map_or_else(|| session.file_tree_root.clone(), str::to_owned),
            _ => session.file_tree_root.clone(),
        };
        Some((session_id, directory))
    }

    /// 展开或折叠一个来自当前树快照的目录。展示字符串不会被当作路径使用：
    /// 只有 runtime 缓存中仍存在的精确目录路径才会执行请求。
    pub fn toggle_directory(&mut self, session_id: SshSessionId, path: &str) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        if !known_directory(session, path) {
            return false;
        }
        if session.open_directories.remove(path) {
            return true;
        }
        session.open_directories.insert(path.to_owned());
        if !session.directory_listings.contains_key(path)
            && !session.latest_directory_request.contains_key(path)
        {
            request_directory(session, path);
        }
        true
    }

    pub fn refresh_directory(&mut self, session_id: SshSessionId, path: &str) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        if !known_directory(session, path) {
            return false;
        }
        cancel_directory_path(session, path);
        session.directory_listings.remove(path);
        session.open_directories.insert(path.to_owned());
        session.file_tree_error = None;
        request_directory(session, path)
    }

    pub fn search_files(
        &mut self,
        session_id: SshSessionId,
        query: String,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        cancel_search(session);
        let query = query.trim().to_owned();
        session.search_entries.clear();
        session.search_truncated = false;
        session.search_error = None;
        if query.is_empty() {
            session.search_query = None;
            session.search_loading = false;
            return Ok(());
        }
        if query.chars().count() < 2 {
            return Err("SSH file search requires at least two characters".to_owned());
        }
        let token = allocate_file_token(session)?;
        let command = FileCommand::Search {
            token,
            root: session.file_tree_root.clone(),
            query: query.clone(),
            maximum_depth: 32,
            maximum_results: 1000,
        };
        send_file_command(session, command)?;
        session
            .pending_file_requests
            .insert(token, PendingFileRequest::Search { query: query.clone() });
        session.search_query = Some(query);
        session.search_loading = true;
        session.latest_search_token = Some(token);
        Ok(())
    }

    pub fn read_text(
        &mut self,
        session_id: SshSessionId,
        document_token: u64,
        path: String,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        let token = allocate_file_token(session)?;
        send_file_command(
            session,
            FileCommand::ReadText {
                token,
                path: path.clone(),
            },
        )?;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::TextRead {
                document_token,
                path,
            },
        );
        Ok(())
    }

    pub fn write_text(
        &mut self,
        session_id: SshSessionId,
        document_token: u64,
        path: String,
        bytes: Vec<u8>,
        expected_sha256: [u8; 32],
        force: bool,
    ) -> Result<(), String> {
        if bytes.len() > crate::shell::text_editor::MAX_TEXT_FILE_BYTES {
            return Err("The edited SSH file exceeds the editor size limit".to_owned());
        }
        std::str::from_utf8(&bytes)
            .map_err(|_| "The SSH editor can save UTF-8 text only".to_owned())?;
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        let expected = if force {
            None
        } else {
            let editor = session
                .editor_versions
                .get(&document_token)
                .filter(|editor| editor.path == path)
                .ok_or_else(|| "The SSH editor revision is stale; reload the file".to_owned())?;
            if editor.version.sha256 != expected_sha256 {
                return Err("The SSH editor revision is stale; reload the file".to_owned());
            }
            Some(editor.version.clone())
        };
        let token = allocate_file_token(session)?;
        send_file_command(
            session,
            FileCommand::WriteTextAtomic {
                token,
                path: path.clone(),
                expected,
                content: bytes,
            },
        )?;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::TextWrite {
                document_token,
                path,
            },
        );
        Ok(())
    }

    pub fn open_local_copy(
        &mut self,
        session_id: SshSessionId,
        remote_path: String,
        display_name: &str,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        let token = allocate_file_token(session)?;
        let directory = std::env::temp_dir()
            .join("lumen_ssh_open")
            .join(session_id.to_string());
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("Could not prepare the SSH download folder: {error}"))?;
        let local_path = directory.join(format!(
            "{token}-{}",
            safe_local_file_name(display_name, token)
        ));
        send_file_command(
            session,
            FileCommand::Download {
                token,
                remote_path,
                local_path: local_path.clone(),
                overwrite: false,
            },
        )?;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::OpenLocalCopy { local_path },
        );
        Ok(())
    }

    pub fn create_entry(
        &mut self,
        session_id: SshSessionId,
        directory: String,
        name: &str,
        is_directory: bool,
    ) -> Result<(), String> {
        validate_linux_entry_name(name)?;
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        if !known_directory(session, &directory) {
            return Err("The SSH target directory is stale".to_owned());
        }
        let path = join_linux_path(&directory, name);
        let token = allocate_file_token(session)?;
        let (command, action) = if is_directory {
            (
                FileCommand::CreateDirectory { token, path },
                SshFileAction::CreateDirectory,
            )
        } else {
            (
                FileCommand::CreateFile { token, path },
                SshFileAction::CreateFile,
            )
        };
        send_file_command(session, command)?;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::Operation {
                action,
                refresh_directories: vec![directory],
                invalidate_subtree: None,
                local_path: None,
            },
        );
        Ok(())
    }

    pub fn delete_entry(
        &mut self,
        session_id: SshSessionId,
        path: String,
        is_directory: bool,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        if path == session.file_tree_root || !known_tree_path(session, &path, is_directory) {
            return Err("The SSH file selection is stale".to_owned());
        }
        let refresh_directory = linux_parent(&path).map(str::to_owned);
        let token = allocate_file_token(session)?;
        send_file_command(
            session,
            FileCommand::Delete {
                token,
                path,
                recursive: is_directory,
            },
        )?;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::Operation {
                action: SshFileAction::Delete,
                refresh_directories: refresh_directory.into_iter().collect(),
                invalidate_subtree: None,
                local_path: None,
            },
        );
        Ok(())
    }

    /// 同目录重命名（右键「重命名」）。与 [`Self::move_entry`] 共用 SFTP `rename`，区别是目标
    /// 目录不变、只换名字；服务端撞名回 `Conflict`（不覆盖）。目录改名后其子树缓存整体失效
    /// （子项路径前缀全变），故 `invalidate_subtree` 传原路径。
    pub fn rename_entry(
        &mut self,
        session_id: SshSessionId,
        path: String,
        is_directory: bool,
        new_name: &str,
    ) -> Result<(), String> {
        validate_linux_entry_name(new_name)?;
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        if path == session.file_tree_root || !known_tree_path(session, &path, is_directory) {
            return Err("The SSH file selection is stale".to_owned());
        }
        if new_name == linux_basename(&path) {
            return Ok(()); // 名字没变：不发命令（UI 已挡，此为双保险）。
        }
        let parent = linux_parent(&path)
            .ok_or_else(|| "The SSH source path is invalid".to_owned())?
            .to_owned();
        let destination = join_linux_path(&parent, new_name);
        let token = allocate_file_token(session)?;
        send_file_command(
            session,
            FileCommand::Rename {
                token,
                from: path.clone(),
                to: destination,
            },
        )?;
        session.selected_file = None;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::Operation {
                action: SshFileAction::Rename,
                refresh_directories: vec![parent],
                invalidate_subtree: Some(path),
                local_path: None,
            },
        );
        Ok(())
    }

    pub fn move_entry(
        &mut self,
        session_id: SshSessionId,
        source_path: String,
        source_is_directory: bool,
        target_directory: String,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        let (source_parent, destination) = plan_move_entry(
            session,
            &source_path,
            source_is_directory,
            &target_directory,
        )?;
        let token = allocate_file_token(session)?;
        send_file_command(
            session,
            FileCommand::Rename {
                token,
                from: source_path.clone(),
                to: destination,
            },
        )?;

        let refresh_directories = vec![target_directory, source_parent];
        session.selected_file = None;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::Operation {
                action: SshFileAction::Rename,
                refresh_directories,
                invalidate_subtree: Some(source_path),
                local_path: None,
            },
        );
        Ok(())
    }

    pub fn download_file(
        &mut self,
        session_id: SshSessionId,
        remote_path: String,
        local_path: PathBuf,
        overwrite: bool,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        let completed_local_path = local_path.clone();
        let token = allocate_file_token(session)?;
        send_file_command(
            session,
            FileCommand::Download {
                token,
                remote_path,
                local_path,
                overwrite,
            },
        )?;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::Operation {
                action: SshFileAction::Download,
                refresh_directories: Vec::new(),
                invalidate_subtree: None,
                local_path: Some(completed_local_path),
            },
        );
        Ok(())
    }

    pub fn upload_file(
        &mut self,
        session_id: SshSessionId,
        local_path: PathBuf,
        remote_directory: String,
        overwrite: bool,
    ) -> Result<(), String> {
        let name = local_path
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned)
            .ok_or_else(|| "The local file name is not valid UTF-8".to_owned())?;
        self.upload_entry(
            session_id,
            local_path,
            remote_directory,
            name,
            overwrite,
        )
    }

    pub fn upload_entry(
        &mut self,
        session_id: SshSessionId,
        local_path: PathBuf,
        remote_directory: String,
        destination_name: String,
        overwrite: bool,
    ) -> Result<(), String> {
        validate_linux_entry_name(&destination_name)?;
        let remote_path = join_linux_path(&remote_directory, &destination_name);
        let completed_local_path = local_path.clone();
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        if !known_directory(session, &remote_directory) {
            return Err("The SSH target directory is stale".to_owned());
        }
        let token = allocate_file_token(session)?;
        send_file_command(
            session,
            FileCommand::Upload {
                token,
                local_path,
                remote_path,
                overwrite,
            },
        )?;
        session.pending_file_requests.insert(
            token,
            PendingFileRequest::Operation {
                action: SshFileAction::Upload,
                refresh_directories: vec![remote_directory],
                invalidate_subtree: None,
                local_path: Some(completed_local_path),
            },
        );
        Ok(())
    }

    pub fn send_input(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let session_id = self
            .active_session_id
            .ok_or_else(|| "No active SSH session".to_owned())?;
        self.send_input_to(session_id, bytes)
    }

    pub fn send_input_to(
        &mut self,
        session_id: SshSessionId,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        if bytes.is_empty() {
            return Ok(());
        }
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        if session.state != ConnectionState::Connected {
            return Err("SSH session is not connected".to_owned());
        }
        let submits_line = bytes
            .iter()
            .any(|byte| matches!(*byte, b'\r' | b'\n'));
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
                session.shell_idle = false;
            }
            return Err(message);
        }
        if submits_line {
            session.shell_idle = false;
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

    /// 结束远端一个进程（`force` = SIGKILL，否则 SIGTERM）。结果异步到达，
    /// 经 [`RuntimeView::kill_feedback`] 呈现；下一轮监控采样会反映进程消失。
    pub fn kill_process(
        &mut self,
        session_id: SshSessionId,
        pid: u32,
        force: bool,
    ) -> Result<(), String> {
        let session = self.send_manage(
            session_id,
            PendingManage::Kill { pid },
            ManageAction::Kill { pid, force },
        )?;
        session.kill_feedback = None;
        Ok(())
    }

    /// 查询远端 TCP/UDP 端口的监听进程。
    pub fn query_port(&mut self, session_id: SshSessionId, port: u16) -> Result<(), String> {
        let session = self.send_manage(
            session_id,
            PendingManage::QueryPort,
            ManageAction::QueryPort { port },
        )?;
        session.port_lookup = Some(PortLookupState {
            port,
            loading: true,
            error: false,
            entries: Vec::new(),
        });
        Ok(())
    }

    /// 按名称（命令行子串、忽略大小写）搜索远端进程；`query` 为空时返回
    /// 全量进程列表（CPU 降序、上限 200 条，供详情弹窗使用）。
    pub fn search_processes(
        &mut self,
        session_id: SshSessionId,
        query: &str,
    ) -> Result<(), String> {
        let query = query.trim();
        let session = self.send_manage(
            session_id,
            PendingManage::QueryProcess,
            ManageAction::QueryProcess {
                query: query.to_owned(),
            },
        )?;
        // stale-while-revalidate：同 query 的自动刷新保留旧列表，只在返回时
        // 替换——清空重建会让 UI 每次刷新都闪「搜索中」占位（海风哥反馈）。
        match &mut session.process_search {
            Some(existing) if existing.query == query => {
                existing.loading = true;
                existing.error = false;
            }
            _ => {
                session.process_search = Some(ProcessSearchState {
                    query: query.to_owned(),
                    loading: true,
                    error: false,
                    results: Vec::new(),
                });
            }
        }
        Ok(())
    }

    pub fn clear_process_search(&mut self, session_id: SshSessionId) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        session.process_search.take().is_some()
    }

    pub fn clear_port_lookup(&mut self, session_id: SshSessionId) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        session.port_lookup.take().is_some()
    }

    pub fn dismiss_kill_feedback(&mut self, session_id: SshSessionId) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        session.kill_feedback.take().is_some()
    }

    fn send_manage(
        &mut self,
        session_id: SshSessionId,
        pending: PendingManage,
        action: ManageAction,
    ) -> Result<&mut RuntimeSession, String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "SSH session no longer exists".to_owned())?;
        if session.state != ConnectionState::Connected {
            return Err("SSH session is not connected".to_owned());
        }
        let token = session.next_manage_token;
        session.next_manage_token = session.next_manage_token.wrapping_add(1).max(1);
        let request = ManageRequest { token, action };
        let result = session
            .connection
            .as_ref()
            .ok_or_else(|| "SSH connection is closed".to_owned())?
            .send(Command::Manage(request));
        if let Err(error) = result {
            return Err(error.to_string());
        }
        session.pending_manage.insert(token, pending);
        Ok(session)
    }

    pub fn drain(&mut self) -> DrainOutcome {
        let active_id = self.active_session_id;
        let mut outcome = DrainOutcome {
            editor_invalidated_sessions: std::mem::take(
                &mut self.pending_editor_invalidations,
            ),
            ..DrainOutcome::default()
        };
        let session_ids = self.sessions.keys().copied().collect::<Vec<_>>();

        for session_id in session_ids {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            let profile_id = session.profile_id.clone();
            let before_state = session.state.clone();
            let before_detail = session.detail.clone();
            let before_metrics = session.metrics.clone();
            let before_metrics_error = session.metrics_error.clone();
            let before_unknown = session.unknown_host_key.clone();
            let before_changed = session.changed_host_key.clone();
            let before_file_tree_root = session.file_tree_root.clone();
            let mut terminal_changed = false;
            let mut directory_changed = false;
            let mut events = Vec::new();
            let mut directory_events = Vec::new();
            let mut file_events = Vec::new();
            if let Some(connection) = &session.connection {
                connection.drain(&mut events);
                connection.drain_directory(&mut directory_events);
                connection.drain_file(&mut file_events);
            }
            let mut disconnect_events = Vec::new();
            for event in events {
                if matches!(event, Event::Disconnected { .. }) {
                    disconnect_events.push(event);
                    continue;
                }
                let credential_failed = event_is_saved_credential_failure(&event);
                let connected = matches!(&event, Event::Connected { .. });
                let changed = apply_event(session, event);
                terminal_changed |= changed;
                if connected {
                    let root = session.file_tree_root.clone();
                    directory_changed |= request_directory(session, &root);
                }
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
            for event in directory_events {
                directory_changed |= apply_directory_event(session, event);
            }
            for event in file_events {
                directory_changed |= apply_file_event(
                    session_id,
                    session,
                    event,
                    &mut outcome.file_events,
                );
            }
            // 文件终态必须先于 Disconnected：否则断线清 pending 后，同批已经到达的
            // TextRead/TextWrite 成功帧会被当作陈旧事件丢弃，编辑器永久 Loading/Saving。
            for event in disconnect_events {
                if !outcome.editor_invalidated_sessions.contains(&session_id) {
                    outcome.editor_invalidated_sessions.push(session_id);
                }
                terminal_changed |= apply_event(session, event);
            }

            outcome.sessions_changed |= session_view_changed(
                &before_state,
                &before_file_tree_root,
                session,
            );

            if active_id == Some(session_id) {
                outcome.active_terminal_changed |= terminal_changed;
                outcome.active_status_changed |= before_state != session.state
                    || before_detail != session.detail
                    || before_metrics != session.metrics
                    || before_metrics_error != session.metrics_error
                    || before_unknown != session.unknown_host_key
                    || before_changed != session.changed_host_key
                    || directory_changed;
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

    /// 当前激活 SSH shell 是否处于可安全接收整条文件树 `cd` 命令的提示符。
    #[must_use]
    pub fn active_shell_idle(&self) -> bool {
        self.active_session().is_some_and(|session| {
            session.state == ConnectionState::Connected
                && session.shell_idle
                && !session.terminal.is_alt_screen()
        })
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
        let session_id = self.active_session_id?;
        let session = self.sessions.get(&session_id)?;
        Some(RuntimeView {
            session_id,
            profile_id: session.profile_id.clone(),
            profile_name: session.profile_name.clone(),
            endpoint: session.endpoint.clone(),
            state: session.state.clone(),
            detail: session.detail.clone(),
            metrics: session.metrics.as_ref().map(format_metrics),
            monitor: session.metrics.as_ref().map(|metrics| MonitorView {
                metrics: metrics.clone(),
                cpu_history: session.cpu_history.iter().copied().collect(),
                receive_history: session.receive_history.iter().copied().collect(),
                transmit_history: session.transmit_history.iter().copied().collect(),
            }),
            metrics_error: session.metrics_error.clone(),
            unknown_host_key: session
                .unknown_host_key
                .as_ref()
                .map(|identity| UnknownHostKey {
                    session_id,
                    profile_id: session.profile_id.clone(),
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
            process_search: session
                .process_search
                .as_ref()
                .map(|search| ProcessSearchView {
                    query: search.query.clone(),
                    loading: search.loading,
                    error: search.error,
                    results: search
                        .results
                        .iter()
                        .map(|entry| SshProcessEntry {
                            pid: entry.pid,
                            cpu_percent: entry.cpu_percent,
                            memory_percent: entry.memory_percent,
                            command: entry.command.clone(),
                        })
                        .collect(),
                }),
            port_lookup: session.port_lookup.as_ref().map(|lookup| PortLookupView {
                port: lookup.port,
                loading: lookup.loading,
                error: lookup.error,
                entries: lookup
                    .entries
                    .iter()
                    .map(|entry| SshPortEntry {
                        protocol: entry.protocol.clone(),
                        local_address: entry.local_address.clone(),
                        pid: entry.pid,
                        command: entry.command.clone(),
                    })
                    .collect(),
            }),
            kill_feedback: session.kill_feedback.map(|feedback| KillFeedbackView {
                pid: feedback.pid,
                status: match feedback.status {
                    Some(KillStatus::Signalled) => SshKillStatus::Signalled,
                    Some(KillStatus::PermissionDenied) => SshKillStatus::PermissionDenied,
                    Some(KillStatus::NoSuchProcess) => SshKillStatus::NoSuchProcess,
                    None => SshKillStatus::Failed,
                },
            }),
        })
    }

    pub fn confirm_unknown_host_key(
        &mut self,
        session_id: SshSessionId,
        algorithm: &str,
        fingerprint: &str,
    ) -> bool {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        let matches = session.unknown_host_key.as_ref().is_some_and(|identity| {
            identity.algorithm == algorithm && identity.sha256_fingerprint == fingerprint
        });
        if matches {
            session.unknown_host_key = None;
            session.state = ConnectionState::CredentialRequired;
            session.detail = None;
            session.shell_idle = false;
        }
        matches
    }

    /// 主机密钥确认提交前重新核对产生该提示的结构化 endpoint 和指纹。
    ///
    /// `user@host:port` 只用于展示，不能承担安全身份比较：用户名或主机名中的
    /// `@`/`:` 可能让不同字段组合得到相同字符串。
    pub fn unknown_host_key_matches(
        &self,
        session_id: SshSessionId,
        profile: &SshProfile,
        algorithm: &str,
        fingerprint: &str,
    ) -> bool {
        self.sessions.get(&session_id).is_some_and(|session| {
            session.profile_id == profile.id
                && session.endpoint_identity.matches_profile(profile)
                && session.unknown_host_key.as_ref().is_some_and(|identity| {
                    identity.algorithm == algorithm && identity.sha256_fingerprint == fingerprint
                })
        })
    }

    pub fn dismiss_unknown_host_key(&mut self, session_id: SshSessionId) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.unknown_host_key = None;
        session.state = ConnectionState::Disconnected;
        session.detail = Some("Host key was not trusted".to_owned());
        session.shell_idle = false;
    }

    fn active_session(&self) -> Option<&RuntimeSession> {
        self.sessions.get(&self.active_session_id?)
    }

    fn active_session_mut(&mut self) -> Option<&mut RuntimeSession> {
        let session_id = self.active_session_id?;
        self.sessions.get_mut(&session_id)
    }

    fn create_session(&mut self, profile: &SshProfile) -> SshSessionId {
        let session_id = self.next_session_id;
        self.next_session_id = self
            .next_session_id
            .checked_add(1)
            .expect("SSH session id space exhausted");
        self.session_order.push(session_id);
        self.sessions.insert(session_id, RuntimeSession::new(profile));
        self.active_session_id = Some(session_id);
        session_id
    }
}

fn session_view_changed(
    before_state: &ConnectionState,
    before_file_tree_root: &str,
    session: &RuntimeSession,
) -> bool {
    before_state != &session.state || before_file_tree_root != session.file_tree_root
}

fn ssh_file_tree_root(profile: &SshProfile) -> String {
    let candidate = profile.initial_directory.as_deref().unwrap_or("").trim();
    if candidate.starts_with('/')
        && candidate.len() <= 4096
        && !candidate.chars().any(char::is_control)
    {
        let trimmed = candidate.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_owned()
        } else {
            trimmed.to_owned()
        }
    } else {
        "/".to_owned()
    }
}

fn known_directory(session: &RuntimeSession, path: &str) -> bool {
    path == session.file_tree_root
        || session.directory_listings.values().any(|listing| {
            listing
                .entries
                .iter()
                .any(|entry| entry.path == path && entry.kind == DirectoryEntryKind::Directory)
        })
        || session.search_entries.iter().any(|entry| {
            (entry.path == path && entry.kind == DirectoryEntryKind::Directory)
                || linux_parent(&entry.path) == Some(path)
        })
}

fn known_tree_path(session: &RuntimeSession, path: &str, is_directory: bool) -> bool {
    if is_directory && path == session.file_tree_root {
        return true;
    }
    session.directory_listings.values().any(|listing| {
        listing.entries.iter().any(|entry| {
            entry.path == path
                && (entry.kind == DirectoryEntryKind::Directory) == is_directory
        })
    }) || session.search_entries.iter().any(|entry| {
        entry.path == path && (entry.kind == DirectoryEntryKind::Directory) == is_directory
    })
}

fn plan_move_entry(
    session: &RuntimeSession,
    source_path: &str,
    source_is_directory: bool,
    target_directory: &str,
) -> Result<(String, String), String> {
    if source_path == session.file_tree_root
        || !known_tree_path(session, source_path, source_is_directory)
    {
        return Err("The SSH file selection is stale".to_owned());
    }
    if !known_directory(session, target_directory) {
        return Err("The SSH target directory is stale".to_owned());
    }
    let source_parent = linux_parent(source_path)
        .ok_or_else(|| "The SSH source path is invalid".to_owned())?
        .to_owned();
    if source_parent == target_directory {
        return Err("The SSH item is already in that directory".to_owned());
    }
    if source_is_directory && linux_path_is_same_or_descendant(target_directory, source_path) {
        return Err("An SSH directory cannot be moved into itself".to_owned());
    }

    let destination_name = linux_basename(source_path);
    validate_linux_entry_name(destination_name)?;
    Ok((
        source_parent,
        join_linux_path(target_directory, destination_name),
    ))
}

fn tree_entry_metadata(session: &RuntimeSession, path: &str) -> Option<(String, u64)> {
    session
        .directory_listings
        .values()
        .flat_map(|listing| listing.entries.iter())
        .chain(session.search_entries.iter())
        .find(|entry| entry.path == path)
        .map(|entry| (entry.name.clone(), entry.size))
}

fn allocate_file_token(session: &mut RuntimeSession) -> Result<u64, String> {
    let start = session.next_file_token.max(1);
    let mut token = start;
    loop {
        if !session.pending_file_requests.contains_key(&token) {
            session.next_file_token = token.checked_add(1).unwrap_or(1);
            return Ok(token);
        }
        token = token.checked_add(1).unwrap_or(1);
        if token == start {
            return Err("SSH file request counter exhausted".to_owned());
        }
    }
}

fn send_file_command(session: &RuntimeSession, command: FileCommand) -> Result<(), String> {
    if session.state != ConnectionState::Connected {
        return Err("SSH session is not connected".to_owned());
    }
    session
        .connection
        .as_ref()
        .ok_or_else(|| "SSH connection is closed".to_owned())?
        .send_file(command)
        .map_err(|error| error.to_string())
}

fn cancel_search(session: &mut RuntimeSession) {
    let Some(token) = session.latest_search_token.take() else {
        return;
    };
    if let Some(connection) = &session.connection {
        let _ = connection.send_file(FileCommand::Cancel { token });
    }
    session.pending_file_requests.remove(&token);
    session.search_loading = false;
}

fn cancel_file_requests(session: &mut RuntimeSession) {
    if let Some(connection) = &session.connection {
        for token in session.pending_file_requests.keys().copied() {
            let _ = connection.send_file(FileCommand::Cancel { token });
        }
    }
    session.pending_file_requests.clear();
}

fn cancel_directory_path(session: &mut RuntimeSession, path: &str) {
    let Some(token) = session.latest_directory_request.remove(path) else {
        return;
    };
    session.pending_directories.remove(&token);
    if let Some(connection) = &session.connection {
        let _ = connection.send(Command::CancelDirectory { token });
    }
}

fn linux_parent(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    Some(if parent.is_empty() { "/" } else { parent })
}

fn linux_basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or(path)
}

fn join_linux_path(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", directory.trim_end_matches('/'))
    }
}

fn linux_path_is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_linux_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        Err("The SSH file name is invalid".to_owned())
    } else {
        Ok(())
    }
}

fn safe_local_file_name(name: &str, token: u64) -> String {
    let mut result = name
        .chars()
        .take(180)
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    while result.ends_with([' ', '.']) {
        result.pop();
    }
    if result.is_empty() || result == "." || result == ".." {
        format!("remote-{token}.txt")
    } else {
        result
    }
}

fn append_file_tree_rows(
    session: &RuntimeSession,
    parent: &str,
    depth: usize,
    rows: &mut Vec<SshFileTreeRow>,
) {
    let Some(listing) = session.directory_listings.get(parent) else {
        return;
    };
    for entry in &listing.entries {
        let expanded = entry.kind == DirectoryEntryKind::Directory
            && session.open_directories.contains(&entry.path);
        rows.push(SshFileTreeRow {
            name: entry.name.clone(),
            path: entry.path.clone(),
            kind: entry.kind,
            size: entry.size,
            depth,
            expanded,
            loading: session.latest_directory_request.contains_key(&entry.path),
        });
        if expanded {
            append_file_tree_rows(session, &entry.path, depth.saturating_add(1), rows);
        }
    }
}

fn request_directory(session: &mut RuntimeSession, path: &str) -> bool {
    if session.state != ConnectionState::Connected
        || session.latest_directory_request.contains_key(path)
    {
        return false;
    }
    let Some(connection) = &session.connection else {
        return false;
    };
    let token = session.next_directory_token;
    let Some(next_token) = token.checked_add(1) else {
        session.file_tree_error = Some("SSH directory request counter exhausted".to_owned());
        return false;
    };
    match connection.send(list_directory_command(token, path)) {
        Ok(()) => {
            session.next_directory_token = next_token;
            session.pending_directories.insert(token, path.to_owned());
            session
                .latest_directory_request
                .insert(path.to_owned(), token);
            session.file_tree_error = None;
            true
        }
        Err(error) => {
            session.file_tree_error = Some(error.to_string());
            false
        }
    }
}

fn list_directory_command(token: u64, path: &str) -> Command {
    Command::ListDirectory {
        token,
        path: path.to_owned(),
        // SSH 模式始终展示 Linux dot 文件/目录，不保留可变开关，避免树、
        // 刷新和 cwd 重载在不同请求间产生不一致结果。
        show_hidden: true,
    }
}

fn cancel_directory_requests(session: &mut RuntimeSession) {
    if let Some(connection) = &session.connection {
        for token in session.pending_directories.keys().copied() {
            let _ = connection.send(Command::CancelDirectory { token });
        }
    }
    session.pending_directories.clear();
    session.latest_directory_request.clear();
}

fn apply_directory_event(session: &mut RuntimeSession, event: Event) -> bool {
    match event {
        Event::DirectoryListing {
            token,
            mut entries,
            truncated,
        } => {
            let Some(path) = session.pending_directories.remove(&token) else {
                return false;
            };
            if session.latest_directory_request.get(&path).copied() != Some(token) {
                return false;
            }
            session.latest_directory_request.remove(&path);
            entries.retain(|entry| directory_entry_belongs_to(&path, entry));
            entries.sort_by(|left, right| {
                directory_kind_order(left.kind)
                    .cmp(&directory_kind_order(right.kind))
                    .then_with(|| {
                        left.name
                            .to_ascii_lowercase()
                            .cmp(&right.name.to_ascii_lowercase())
                    })
                    .then_with(|| left.name.cmp(&right.name))
            });
            session
                .directory_listings
                .insert(path, DirectoryListing { entries, truncated });
            session.file_tree_error = None;
            true
        }
        Event::DirectoryError { token, error } => {
            let Some(path) = session.pending_directories.remove(&token) else {
                return false;
            };
            if session.latest_directory_request.get(&path).copied() != Some(token) {
                return false;
            }
            session.latest_directory_request.remove(&path);
            if error != DirectoryError::Cancelled {
                session.file_tree_error = Some(directory_error_message(error).to_owned());
            }
            true
        }
        Event::ManageResult { token, result } => {
            let Some(pending) = session.pending_manage.remove(&token) else {
                return false;
            };
            match (pending, result) {
                (PendingManage::Kill { pid }, Ok(ManageOutcome::Kill(outcome))) => {
                    session.kill_feedback = Some(KillFeedbackState {
                        pid,
                        status: Some(outcome.status),
                    });
                }
                (PendingManage::Kill { .. }, Ok(_)) => return false,
                (PendingManage::Kill { pid }, Err(_)) => {
                    // 通道层失败（信号未送达），与远端回执的权限/不存在区分。
                    session.kill_feedback = Some(KillFeedbackState { pid, status: None });
                }
                (PendingManage::QueryPort, Ok(ManageOutcome::Ports(entries))) => {
                    if let Some(lookup) = &mut session.port_lookup {
                        lookup.loading = false;
                        lookup.entries = entries;
                    }
                }
                (PendingManage::QueryPort, Ok(_)) => return false,
                (PendingManage::QueryPort, Err(_)) => {
                    if let Some(lookup) = &mut session.port_lookup {
                        lookup.loading = false;
                        lookup.error = true;
                    }
                }
                (PendingManage::QueryProcess, Ok(ManageOutcome::Processes(results))) => {
                    if let Some(search) = &mut session.process_search {
                        search.loading = false;
                        search.results = results;
                    }
                }
                (PendingManage::QueryProcess, Ok(_)) => return false,
                (PendingManage::QueryProcess, Err(_)) => {
                    if let Some(search) = &mut session.process_search {
                        search.loading = false;
                        search.error = true;
                    }
                }
            }
            true
        }
        _ => false,
    }
}

fn apply_file_event(
    session_id: SshSessionId,
    session: &mut RuntimeSession,
    event: FileEvent,
    output: &mut Vec<SshFileRuntimeEvent>,
) -> bool {
    let token = event.token();
    if matches!(event, FileEvent::TransferProgress { .. }) {
        return false;
    }
    let Some(request) = session.pending_file_requests.remove(&token) else {
        return false;
    };
    match (request, event) {
        (
            PendingFileRequest::Search { query },
            FileEvent::SearchResults {
                mut entries,
                truncated,
                ..
            },
        ) => {
            if session.latest_search_token != Some(token)
                || session.search_query.as_deref() != Some(query.as_str())
            {
                return false;
            }
            session.latest_search_token = None;
            session.search_loading = false;
            entries.retain(valid_search_entry);
            entries.sort_by(|left, right| {
                directory_kind_order(left.kind)
                    .cmp(&directory_kind_order(right.kind))
                    .then_with(|| {
                        left.path
                            .to_ascii_lowercase()
                            .cmp(&right.path.to_ascii_lowercase())
                    })
                    .then_with(|| left.path.cmp(&right.path))
            });
            session.search_entries = entries;
            session.search_truncated = truncated;
            session.search_error = None;
            true
        }
        (
            PendingFileRequest::TextRead {
                document_token,
                path,
            },
            FileEvent::TextRead {
                content, version, ..
            },
        ) => {
            session.editor_versions.insert(
                document_token,
                EditorVersion {
                    path,
                    version: version.clone(),
                },
            );
            output.push(SshFileRuntimeEvent::TextLoaded {
                session_id,
                document_token,
                bytes: content,
                version,
            });
            false
        }
        (
            PendingFileRequest::TextWrite {
                document_token,
                path,
            },
            FileEvent::OperationComplete {
                operation: FileOperation::WriteText,
                version: Some(version),
                ..
            },
        ) => {
            session.editor_versions.insert(
                document_token,
                EditorVersion {
                    path: path.clone(),
                    version: version.clone(),
                },
            );
            let refreshed = linux_parent(&path).is_some_and(|parent| {
                refresh_cached_directory(session, parent)
            });
            output.push(SshFileRuntimeEvent::TextSaved {
                session_id,
                document_token,
                version,
            });
            refreshed
        }
        (
            PendingFileRequest::OpenLocalCopy { local_path },
            FileEvent::OperationComplete {
                operation: FileOperation::Download,
                ..
            },
        ) => {
            output.push(SshFileRuntimeEvent::LocalCopyReady {
                session_id,
                local_path,
            });
            false
        }
        (
            PendingFileRequest::Operation {
                action,
                refresh_directories,
                invalidate_subtree,
                local_path,
            },
            FileEvent::OperationComplete { operation, .. },
        ) => {
            if let Some(path) = invalidate_subtree {
                invalidate_cached_subtree(session, &path);
            }
            let refreshed = refresh_cached_directories(session, &refresh_directories);
            output.push(SshFileRuntimeEvent::OperationComplete {
                session_id,
                operation,
                local_path,
            });
            let _ = action;
            refreshed
        }
        (request, FileEvent::Error { error, .. }) => {
            let local_path = pending_local_path(&request);
            let (document_token, action) = match &request {
                PendingFileRequest::Search { .. } => {
                    if session.latest_search_token == Some(token) {
                        session.latest_search_token = None;
                        session.search_loading = false;
                        session.search_error = Some(error.to_string());
                    }
                    (None, SshFileAction::Search)
                }
                PendingFileRequest::TextRead { document_token, .. } => {
                    (Some(*document_token), SshFileAction::ReadText)
                }
                PendingFileRequest::TextWrite { document_token, .. } => {
                    if error == FileError::Conflict {
                        output.push(SshFileRuntimeEvent::TextConflict {
                            session_id,
                            document_token: *document_token,
                        });
                        return false;
                    }
                    (Some(*document_token), SshFileAction::WriteText)
                }
                PendingFileRequest::OpenLocalCopy { .. } => {
                    (None, SshFileAction::OpenLocalCopy)
                }
                PendingFileRequest::Operation { action, .. } => (None, *action),
            };
            output.push(SshFileRuntimeEvent::Error {
                session_id,
                document_token,
                operation: action,
                error,
                local_path,
            });
            false
        }
        (request, _) => {
            let (document_token, operation) = pending_file_identity(&request);
            let local_path = pending_local_path(&request);
            output.push(SshFileRuntimeEvent::Error {
                session_id,
                document_token,
                operation,
                error: FileError::RemoteIo,
                local_path,
            });
            false
        }
    }
}

fn pending_local_path(request: &PendingFileRequest) -> Option<PathBuf> {
    match request {
        PendingFileRequest::OpenLocalCopy { local_path } => Some(local_path.clone()),
        PendingFileRequest::Operation { local_path, .. } => local_path.clone(),
        PendingFileRequest::Search { .. }
        | PendingFileRequest::TextRead { .. }
        | PendingFileRequest::TextWrite { .. } => None,
    }
}

fn pending_file_identity(request: &PendingFileRequest) -> (Option<u64>, SshFileAction) {
    match request {
        PendingFileRequest::Search { .. } => (None, SshFileAction::Search),
        PendingFileRequest::TextRead { document_token, .. } => {
            (Some(*document_token), SshFileAction::ReadText)
        }
        PendingFileRequest::TextWrite { document_token, .. } => {
            (Some(*document_token), SshFileAction::WriteText)
        }
        PendingFileRequest::OpenLocalCopy { .. } => (None, SshFileAction::OpenLocalCopy),
        PendingFileRequest::Operation { action, .. } => (None, *action),
    }
}

fn valid_search_entry(entry: &DirectoryEntry) -> bool {
    entry.path.starts_with('/')
        && entry.path.len() <= 4096
        && !entry.path.chars().any(char::is_control)
        && !entry.name.is_empty()
        && entry.name != "."
        && entry.name != ".."
        && !entry.name.contains('/')
        && !entry.name.chars().any(char::is_control)
}

fn refresh_cached_directory(session: &mut RuntimeSession, path: &str) -> bool {
    if !known_directory(session, path) {
        return false;
    }
    cancel_directory_path(session, path);
    session.directory_listings.remove(path);
    session.open_directories.insert(path.to_owned());
    request_directory(session, path)
}

fn refresh_cached_directories(session: &mut RuntimeSession, paths: &[String]) -> bool {
    let mut refresh = Vec::with_capacity(paths.len());
    for path in paths {
        if !refresh.iter().any(|existing| existing == path) && known_directory(session, path) {
            refresh.push(path.clone());
        }
    }
    for path in &refresh {
        cancel_directory_path(session, path);
        session.directory_listings.remove(path);
        session.open_directories.insert(path.clone());
    }
    for path in &refresh {
        let _ = request_directory(session, path);
    }
    !refresh.is_empty()
}

fn invalidate_cached_subtree(session: &mut RuntimeSession, root: &str) {
    let pending_paths = session
        .latest_directory_request
        .keys()
        .filter(|path| linux_path_is_same_or_descendant(path, root))
        .cloned()
        .collect::<Vec<_>>();
    for path in pending_paths {
        cancel_directory_path(session, &path);
    }
    session
        .directory_listings
        .retain(|path, _| !linux_path_is_same_or_descendant(path, root));
    session
        .open_directories
        .retain(|path| !linux_path_is_same_or_descendant(path, root));
    if session
        .selected_file
        .as_ref()
        .is_some_and(|(path, _)| linux_path_is_same_or_descendant(path, root))
    {
        session.selected_file = None;
    }
    session
        .search_entries
        .retain(|entry| !linux_path_is_same_or_descendant(&entry.path, root));
}

fn directory_entry_belongs_to(parent: &str, entry: &DirectoryEntry) -> bool {
    if entry.name.is_empty()
        || entry.name == "."
        || entry.name == ".."
        || entry.name.contains('/')
        || entry.name.chars().any(char::is_control)
    {
        return false;
    }
    let expected = if parent == "/" {
        format!("/{}", entry.name)
    } else {
        format!("{parent}/{}", entry.name)
    };
    entry.path == expected
}

const fn directory_kind_order(kind: DirectoryEntryKind) -> u8 {
    match kind {
        DirectoryEntryKind::Directory => 0,
        DirectoryEntryKind::File => 1,
        DirectoryEntryKind::Symlink => 2,
        DirectoryEntryKind::Other => 3,
    }
}

const fn directory_error_message(error: DirectoryError) -> &'static str {
    match error {
        DirectoryError::InvalidRequest => "The remote directory path is invalid",
        DirectoryError::DuplicateToken => "The directory request is stale",
        DirectoryError::Busy => "Too many directory requests are running",
        DirectoryError::OpenFailed => "Could not open the remote directory service",
        DirectoryError::ExecFailed => "The server does not support directory browsing",
        DirectoryError::TimedOut => "The remote directory request timed out",
        DirectoryError::OutputTooLarge => "The remote directory contains too much data",
        DirectoryError::MalformedOutput => "The server returned an invalid directory listing",
        DirectoryError::CommandFailed => "The remote directory could not be read",
        DirectoryError::Cancelled => "The remote directory request was cancelled",
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
        | Event::MetricsError { .. }
        | Event::DirectoryListing { .. }
        | Event::DirectoryError { .. }
        | Event::ManageResult { .. } => {
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

/// 识别 transport 的 shell cwd 钩子前缀，并保留足够短的跨事件尾巴。
///
/// cwd 可能与上一帧相同，不能靠 [`Terminal::cwd`] 是否变化判断新提示符；
/// transport 每次提示符都会重新输出 OSC 9;9，看到该前缀即可恢复 idle。
fn observe_cwd_report_prefix(scan_tail: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let keep = SSH_CWD_REPORT_PREFIX.len().saturating_sub(1);
    let head_len = bytes.len().min(keep);
    let mut boundary = Vec::with_capacity(scan_tail.len() + head_len);
    boundary.extend_from_slice(scan_tail);
    boundary.extend_from_slice(&bytes[..head_len]);

    let seen = boundary
        .windows(SSH_CWD_REPORT_PREFIX.len())
        .any(|window| window == SSH_CWD_REPORT_PREFIX)
        || bytes
            .windows(SSH_CWD_REPORT_PREFIX.len())
            .any(|window| window == SSH_CWD_REPORT_PREFIX);

    scan_tail.clear();
    if keep > 0 {
        if bytes.len() >= keep {
            scan_tail.extend_from_slice(&bytes[bytes.len() - keep..]);
        } else {
            let start = boundary.len().saturating_sub(keep);
            scan_tail.extend_from_slice(&boundary[start..]);
        }
    }
    seen
}

fn apply_event(session: &mut RuntimeSession, event: Event) -> bool {
    match event {
        Event::Connecting => {
            session.state = ConnectionState::Connecting;
            session.detail = None;
            session.shell_idle = false;
            session.cwd_report_scan_tail.clear();
            false
        }
        Event::HostKeyUnknown { presented } => {
            session.state = ConnectionState::AwaitingHostKey;
            session.unknown_host_key = Some(presented);
            session.changed_host_key = None;
            session.detail = None;
            session.shell_idle = false;
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
            session.shell_idle = false;
            false
        }
        Event::Connected { .. } => {
            session.state = ConnectionState::Connected;
            session.detail = None;
            session.shell_idle = false;
            false
        }
        Event::ConnectionTestSucceeded { .. } => {
            session.state = ConnectionState::Error;
            session.detail =
                Some("An SSH connection test event reached an interactive session".to_owned());
            session.shell_idle = false;
            false
        }
        Event::Data(bytes) | Event::ExtendedData { data: bytes, .. } => {
            let shell_prompt_reported =
                observe_cwd_report_prefix(&mut session.cwd_report_scan_tail, &bytes);
            session.terminal.advance(&bytes);
            if shell_prompt_reported {
                session.shell_idle = true;
            }
            if let Some(root) = session
                .terminal
                .cwd()
                .and_then(normalize_reported_ssh_cwd)
                .filter(|root| root != &session.file_tree_root)
            {
                sync_file_tree_root(session, root);
            }
            true
        }
        Event::Eof => {
            session.detail = Some("SSH shell reached end of stream".to_owned());
            session.shell_idle = false;
            false
        }
        Event::ExitStatus(status) => {
            session.detail = Some(format!("SSH shell exited with status {status}"));
            session.shell_idle = false;
            false
        }
        Event::ExitSignal { signal, .. } => {
            session.detail = Some(format!("SSH shell exited after signal {signal}"));
            session.shell_idle = false;
            false
        }
        Event::Metrics(metrics) => {
            push_monitor_history(
                &mut session.cpu_history,
                metrics.cpu_usage_percent.unwrap_or(0.0),
            );
            push_monitor_history_f64(
                &mut session.receive_history,
                metrics.network.receive_bytes_per_second.unwrap_or(0.0),
            );
            push_monitor_history_f64(
                &mut session.transmit_history,
                metrics.network.transmit_bytes_per_second.unwrap_or(0.0),
            );
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
        Event::DirectoryListing { .. } | Event::DirectoryError { .. } => {
            // Directory events are drained from their independent lane and
            // handled by `apply_directory_event`.
            false
        }
        Event::ManageResult { .. } => {
            // Manage results share the independent lane with directory
            // events and are handled by `apply_directory_event`.
            false
        }
        Event::Error(error) => {
            session.state = ConnectionState::Error;
            session.detail = Some(error.message);
            session.shell_idle = false;
            false
        }
        Event::Disconnected { reason } => {
            cancel_directory_requests(session);
            cancel_file_requests(session);
            session.search_loading = false;
            session.latest_search_token = None;
            session.pending_manage.clear();
            session.process_search = None;
            session.port_lookup = None;
            session.kill_feedback = None;
            session.shell_idle = false;
            session.cwd_report_scan_tail.clear();
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

fn normalize_reported_ssh_cwd(path: &std::path::Path) -> Option<String> {
    let value = path.to_str()?;
    if !value.starts_with('/')
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return None;
    }
    let trimmed = value.trim_end_matches('/');
    Some(if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    })
}

fn sync_file_tree_root(session: &mut RuntimeSession, root: String) {
    cancel_directory_requests(session);
    cancel_search(session);
    session.file_tree_root = root;
    session.directory_listings.clear();
    session.open_directories.clear();
    session
        .open_directories
        .insert(session.file_tree_root.clone());
    session.selected_file = None;
    session.search_query = None;
    session.search_entries.clear();
    session.search_loading = false;
    session.search_truncated = false;
    session.search_error = None;
    session.file_tree_error = None;
    let root = session.file_tree_root.clone();
    let _ = request_directory(session, &root);
}

fn push_monitor_history(history: &mut VecDeque<f32>, value: f32) {
    if history.len() == MONITOR_HISTORY_SAMPLES {
        history.pop_front();
    }
    history.push_back(if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    });
}

fn push_monitor_history_f64(history: &mut VecDeque<f64>, value: f64) {
    if history.len() == MONITOR_HISTORY_SAMPLES {
        history.pop_front();
    }
    history.push_back(if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    });
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
        session.shell_idle = false;
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
    fn runtime_identity_changes_when_account_runtime_is_rebuilt() {
        let first = SshRuntime::default();
        let second = SshRuntime::default();
        assert_ne!(first.runtime_id(), second.runtime_id());
        assert_ne!(first.runtime_id(), 0);
        assert_ne!(second.runtime_id(), 0);
    }

    #[test]
    fn closing_session_reports_editor_source_invalidation() {
        let p = profile(AuthMethod::Agent);
        let mut runtime = SshRuntime::default();
        let (session_id, _) = runtime.select_for_connect(&p);
        assert!(runtime.close_session(session_id));
        assert_eq!(
            runtime.drain().editor_invalidated_sessions,
            vec![session_id]
        );
    }

    #[test]
    fn completed_text_read_is_applied_before_disconnect_clears_pending_requests() {
        let p = profile(AuthMethod::Agent);
        let mut session = RuntimeSession::new(&p);
        session.state = ConnectionState::Connected;
        session.pending_file_requests.insert(
            9,
            PendingFileRequest::TextRead {
                document_token: 77,
                path: "/tmp/a.txt".to_owned(),
            },
        );
        let version = FileVersion {
            size: 4,
            modified_seconds: Some(1),
            sha256: [7; 32],
        };
        let mut output = Vec::new();
        assert!(!apply_file_event(
            1,
            &mut session,
            FileEvent::TextRead {
                token: 9,
                content: b"text".to_vec(),
                version,
            },
            &mut output,
        ));
        assert!(!apply_event(
            &mut session,
            Event::Disconnected {
                reason: DisconnectReason::ShellClosed,
            },
        ));
        assert!(matches!(
            output.as_slice(),
            [SshFileRuntimeEvent::TextLoaded {
                document_token: 77,
                ..
            }]
        ));
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
        let (session_id, intent) = runtime.select_for_connect(&p);
        assert_eq!(intent, ConnectIntent::Agent);
        let session = runtime.sessions.get_mut(&session_id).expect("session");
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
        assert!(!runtime.confirm_unknown_host_key(session_id, "ssh-ed25519", "SHA256:wrong"));
        assert!(runtime.confirm_unknown_host_key(
            session_id,
            &unknown.algorithm,
            &unknown.sha256_fingerprint
        ));

        let session = runtime.sessions.get_mut(&session_id).expect("session");
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
            session_id,
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
        let (session_id, intent) = runtime.select_for_connect(&original);
        assert_eq!(intent, ConnectIntent::Agent);
        let presented = HostKeyIdentity::new(
            "ssh-ed25519",
            "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABC",
        );
        let session = runtime.sessions.get_mut(&session_id).expect("session");
        apply_event(
            session,
            Event::HostKeyUnknown {
                presented: presented.clone(),
            },
        );
        assert!(runtime.unknown_host_key_matches(
            session_id,
            &original,
            &presented.algorithm,
            &presented.sha256_fingerprint,
        ));
        assert_eq!(
            runtime.connect_intent(session_id, &colliding),
            Some(ConnectIntent::AwaitingHostKey),
            "重新点击连接不得把未决指纹改绑到新 endpoint"
        );
        assert!(
            !runtime.unknown_host_key_matches(
                session_id,
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
            details: None,
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
        let (first_session_id, first_intent) = runtime.select_for_connect(&first);
        let (second_session_id, second_intent) = runtime.select_for_connect(&second);
        assert_eq!(first_intent, ConnectIntent::Password);
        assert_eq!(second_intent, ConnectIntent::Password);
        assert_eq!(runtime.sessions.len(), 2);
        assert_ne!(first_session_id, second_session_id);
        assert_eq!(runtime.active_session_id, Some(second_session_id));
    }

    #[test]
    fn same_profile_can_open_multiple_independent_sessions() {
        let p = profile(AuthMethod::Password);
        let mut runtime = SshRuntime::default();
        let (first_session_id, first_intent) = runtime.select_for_connect(&p);
        let (second_session_id, second_intent) = runtime.select_for_connect(&p);
        assert_eq!(first_intent, ConnectIntent::Password);
        assert_eq!(second_intent, ConnectIntent::Password);
        assert_ne!(first_session_id, second_session_id);

        runtime
            .sessions
            .get_mut(&first_session_id)
            .expect("first session")
            .state = ConnectionState::Connected;
        assert_eq!(
            runtime
                .sessions
                .get(&second_session_id)
                .expect("second session")
                .state,
            ConnectionState::CredentialRequired,
            "同一服务器的连接状态必须按会话隔离"
        );

        let views = runtime.session_views();
        assert_eq!(views[0].profile_id, p.id);
        assert_eq!(views[1].profile_id, p.id);
        assert_eq!(views[0].display_name, "/");
        assert_eq!(views[1].display_name, "/");
        assert_eq!(views[0].session_id, first_session_id);
        assert_eq!(views[1].session_id, second_session_id);
    }

    #[test]
    fn ssh_session_title_follows_cwd_until_user_renames_it() {
        let mut p = profile(AuthMethod::Agent);
        p.initial_directory = Some("/home/alice/workspace".to_owned());
        let mut runtime = SshRuntime::default();
        let (session_id, _) = runtime.select_for_connect(&p);

        assert_eq!(runtime.session_views()[0].display_name, "workspace");

        {
            let session = runtime.sessions.get_mut(&session_id).expect("session");
            assert!(apply_event(
                session,
                Event::Data(b"\x1b]9;9;/srv/project\x07".to_vec())
            ));
        }
        assert_eq!(runtime.session_views()[0].display_name, "project");

        assert!(runtime.rename_session(session_id, "  production shell  "));
        assert_eq!(runtime.session_views()[0].display_name, "production shell");

        {
            let session = runtime.sessions.get_mut(&session_id).expect("session");
            assert!(apply_event(
                session,
                Event::Data(b"\x1b]9;9;/opt/services/api\x07".to_vec())
            ));
        }
        assert_eq!(
            runtime.session_views()[0].display_name,
            "production shell",
            "用户改名后 cwd 变化不得覆盖自定义标题"
        );

        assert!(runtime.rename_session(session_id, " \t "));
        assert_eq!(
            runtime.session_views()[0].display_name,
            "api",
            "空白名称应清除自定义标题并恢复当前 cwd"
        );

        {
            let session = runtime.sessions.get_mut(&session_id).expect("session");
            assert!(apply_event(
                session,
                Event::Data(b"\x1b]9;9;/\x07".to_vec())
            ));
        }
        assert_eq!(runtime.session_views()[0].display_name, "/");
        assert!(!runtime.rename_session(u64::MAX, "missing"));
    }

    #[test]
    fn cwd_change_is_reported_as_a_session_view_change() {
        let p = profile(AuthMethod::Agent);
        let mut session = RuntimeSession::new(&p);
        let before_state = session.state.clone();
        let before_file_tree_root = session.file_tree_root.clone();

        assert!(apply_event(
            &mut session,
            Event::Data(b"\x1b]9;9;/srv/background\x07".to_vec())
        ));
        assert!(session_view_changed(
            &before_state,
            &before_file_tree_root,
            &session
        ));
        assert_eq!(session.display_name(), "background");
    }

    #[test]
    fn ssh_directory_requests_always_include_hidden_entries() {
        match list_directory_command(42, "/srv/project") {
            Command::ListDirectory {
                token,
                path,
                show_hidden,
            } => {
                assert_eq!(token, 42);
                assert_eq!(path, "/srv/project");
                assert!(show_hidden, "SSH 目录请求必须始终包含 Linux dot 项");
            }
            _ => panic!("应生成 SSH ListDirectory 请求"),
        }
    }

    #[test]
    fn clearing_file_tree_selection_is_session_scoped() {
        let p = profile(AuthMethod::Password);
        let mut runtime = SshRuntime::default();
        let (session_id, _) = runtime.select_for_connect(&p);
        runtime
            .sessions
            .get_mut(&session_id)
            .expect("session")
            .selected_file = Some(("/home/alice/.ssh".to_owned(), true));

        assert!(runtime.clear_file_selection(session_id));
        assert!(
            runtime
                .sessions
                .get(&session_id)
                .expect("session")
                .selected_file
                .is_none()
        );
        assert!(!runtime.clear_file_selection(u64::MAX));
    }

    #[test]
    fn credential_submission_requires_the_original_pending_session_and_endpoint() {
        let profile = profile(AuthMethod::Password);
        let mut runtime = SshRuntime::default();
        let (session_id, _) = runtime.select_for_connect(&profile);
        assert!(runtime.accepts_credential_for(session_id, &profile));

        let mut edited = profile.clone();
        edited.host = "other.example.test".to_owned();
        assert!(!runtime.accepts_credential_for(session_id, &edited));

        let mut changed_auth = profile.clone();
        changed_auth.auth_method = AuthMethod::Agent;
        assert!(!runtime.accepts_credential_for(session_id, &changed_auth));

        runtime
            .sessions
            .get_mut(&session_id)
            .expect("pending session")
            .state = ConnectionState::Connected;
        assert!(!runtime.accepts_credential_for(session_id, &profile));

        assert!(runtime.close_session(session_id));
        assert!(!runtime.accepts_credential_for(session_id, &profile));
    }

    #[test]
    fn removing_profile_closes_all_of_its_sessions_only() {
        let removed = profile(AuthMethod::Agent);
        let mut kept = removed.clone();
        kept.id = "ssh_abcdef0123456789abcdef0123456789".to_owned();
        kept.name = "prod".to_owned();

        let mut runtime = SshRuntime::default();
        let (kept_session_id, _) = runtime.select_for_connect(&kept);
        runtime.select_for_connect(&removed);
        runtime.select_for_connect(&removed);
        runtime.remove_profile(&removed.id);

        assert_eq!(runtime.sessions.len(), 1);
        assert_eq!(runtime.active_session_id, Some(kept_session_id));
        assert_eq!(runtime.session_views()[0].profile_id, kept.id);
    }

    #[test]
    fn session_bar_keeps_open_order_and_switches_without_closing_background_sessions() {
        let first = profile(AuthMethod::Password);
        let mut second = first.clone();
        second.id = "ssh_abcdef0123456789abcdef0123456789".to_owned();
        second.name = "prod".to_owned();

        let mut runtime = SshRuntime::default();
        let (first_session_id, first_intent) = runtime.select_for_connect(&first);
        let (second_session_id, second_intent) = runtime.select_for_connect(&second);
        assert_eq!(first_intent, ConnectIntent::Password);
        assert_eq!(second_intent, ConnectIntent::Password);
        assert!(runtime.activate_session(first_session_id));

        let sessions = runtime.session_views();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, first_session_id);
        assert_eq!(sessions[0].profile_id, first.id);
        assert!(sessions[0].active);
        assert_eq!(sessions[1].session_id, second_session_id);
        assert_eq!(sessions[1].profile_id, second.id);
        assert!(!sessions[1].active);
        assert_eq!(runtime.sessions.len(), 2);
    }

    #[test]
    fn closing_active_session_selects_an_adjacent_session_and_keeps_profiles_external() {
        let first = profile(AuthMethod::Agent);
        let mut second = first.clone();
        second.id = "ssh_abcdef0123456789abcdef0123456789".to_owned();
        second.name = "prod".to_owned();
        let mut third = first.clone();
        third.id = "ssh_fedcba9876543210fedcba9876543210".to_owned();
        third.name = "stage".to_owned();

        let mut runtime = SshRuntime::default();
        let (first_session_id, _) = runtime.select_for_connect(&first);
        let (second_session_id, _) = runtime.select_for_connect(&second);
        let (third_session_id, _) = runtime.select_for_connect(&third);
        assert!(runtime.activate_session(second_session_id));
        assert!(runtime.close_session(second_session_id));

        assert_eq!(runtime.active_session_id, Some(third_session_id));
        assert_eq!(
            runtime
                .session_views()
                .iter()
                .map(|session| session.session_id)
                .collect::<Vec<_>>(),
            vec![first_session_id, third_session_id]
        );
        assert!(!runtime.close_session(second_session_id));
    }

    #[test]
    fn ssh_file_tree_root_keeps_linux_separators_and_rejects_windows_paths() {
        let mut p = profile(AuthMethod::Agent);
        p.initial_directory = Some("/home/alice/projects/".to_owned());
        assert_eq!(ssh_file_tree_root(&p), "/home/alice/projects");

        p.initial_directory = Some(r"C:\Users\alice".to_owned());
        assert_eq!(
            ssh_file_tree_root(&p),
            "/",
            "Windows 客户端不得把本机路径语义带入 Linux SSH 文件树"
        );
        p.initial_directory = None;
        assert_eq!(ssh_file_tree_root(&p), "/");
    }

    #[test]
    fn reported_ssh_cwd_is_normalized_without_accepting_unsafe_or_local_paths() {
        assert_eq!(
            normalize_reported_ssh_cwd(std::path::Path::new("/srv/app/")),
            Some("/srv/app".to_owned())
        );
        assert_eq!(
            normalize_reported_ssh_cwd(std::path::Path::new("////")),
            Some("/".to_owned())
        );
        for path in [
            r"C:\Users\alice",
            "relative/path",
            "/srv/../etc",
            "/srv/./app",
            "/srv/\u{001b}app",
        ] {
            assert_eq!(
                normalize_reported_ssh_cwd(std::path::Path::new(path)),
                None,
                "{path:?} must not become an SSH file-tree root"
            );
        }
    }

    #[test]
    fn terminal_cwd_report_replaces_only_that_sessions_file_tree_root() {
        let p = profile(AuthMethod::Agent);
        let mut session = RuntimeSession::new(&p);
        session.state = ConnectionState::Connected;
        session.file_tree_root = "/old".to_owned();
        session.open_directories.insert("/old".to_owned());
        session.directory_listings.insert(
            "/old".to_owned(),
            DirectoryListing {
                entries: Vec::new(),
                truncated: false,
            },
        );
        session.selected_file = Some(("/old/readme.txt".to_owned(), false));
        session.search_query = Some("readme".to_owned());
        session.search_entries.push(DirectoryEntry {
            name: "readme.txt".to_owned(),
            path: "/old/readme.txt".to_owned(),
            kind: DirectoryEntryKind::File,
            size: 4,
        });

        assert!(apply_event(
            &mut session,
            Event::Data(b"\x1b]9;9;/srv/project\x07".to_vec())
        ));

        assert_eq!(session.file_tree_root, "/srv/project");
        assert_eq!(
            session.open_directories,
            HashSet::from(["/srv/project".to_owned()])
        );
        assert!(session.directory_listings.is_empty());
        assert!(session.selected_file.is_none());
        assert!(session.search_query.is_none());
        assert!(session.search_entries.is_empty());
        assert!(session.shell_idle, "cwd 提示符上报后 shell 应恢复 idle");
    }

    #[test]
    fn ssh_prompt_marker_can_span_transport_events() {
        let mut tail = Vec::new();
        assert!(!observe_cwd_report_prefix(
            &mut tail,
            b"output\x1b]9;"
        ));
        assert!(observe_cwd_report_prefix(
            &mut tail,
            b"9;/srv/project\x07prompt"
        ));
        assert!(!observe_cwd_report_prefix(&mut tail, b"plain output"));
    }

    #[test]
    fn ssh_paste_target_uses_selected_files_parent_in_tree_and_search() {
        let p = profile(AuthMethod::Agent);
        let mut runtime = SshRuntime::default();
        let (session_id, _) = runtime.select_for_connect(&p);
        let session = runtime.sessions.get_mut(&session_id).expect("session");
        session.file_tree_root = "/srv".to_owned();
        session.directory_listings.insert(
            "/srv".to_owned(),
            DirectoryListing {
                entries: vec![
                    DirectoryEntry {
                        name: "app".to_owned(),
                        path: "/srv/app".to_owned(),
                        kind: DirectoryEntryKind::Directory,
                        size: 0,
                    },
                    DirectoryEntry {
                        name: "README.md".to_owned(),
                        path: "/srv/README.md".to_owned(),
                        kind: DirectoryEntryKind::File,
                        size: 10,
                    },
                ],
                truncated: false,
            },
        );

        session.selected_file = Some(("/srv/README.md".to_owned(), false));
        assert_eq!(
            runtime.active_paste_target(),
            Some((session_id, "/srv".to_owned()))
        );

        let session = runtime.sessions.get_mut(&session_id).expect("session");
        session.selected_file = Some(("/srv/app".to_owned(), true));
        assert_eq!(
            runtime.active_paste_target(),
            Some((session_id, "/srv/app".to_owned()))
        );

        let session = runtime.sessions.get_mut(&session_id).expect("session");
        session.search_entries = vec![DirectoryEntry {
            name: "main.rs".to_owned(),
            path: "/opt/project/main.rs".to_owned(),
            kind: DirectoryEntryKind::File,
            size: 20,
        }];
        session.selected_file = Some(("/opt/project/main.rs".to_owned(), false));
        assert_eq!(
            runtime.active_paste_target(),
            Some((session_id, "/opt/project".to_owned())),
            "搜索结果文件的父目录也应成为粘贴目标"
        );
    }

    fn move_test_session() -> RuntimeSession {
        let p = profile(AuthMethod::Agent);
        let mut session = RuntimeSession::new(&p);
        session.file_tree_root = "/srv".to_owned();
        session.directory_listings.insert(
            "/srv".to_owned(),
            DirectoryListing {
                entries: vec![
                    DirectoryEntry {
                        name: "app".to_owned(),
                        path: "/srv/app".to_owned(),
                        kind: DirectoryEntryKind::Directory,
                        size: 0,
                    },
                    DirectoryEntry {
                        name: "apple".to_owned(),
                        path: "/srv/apple".to_owned(),
                        kind: DirectoryEntryKind::Directory,
                        size: 0,
                    },
                    DirectoryEntry {
                        name: "archive".to_owned(),
                        path: "/srv/archive".to_owned(),
                        kind: DirectoryEntryKind::Directory,
                        size: 0,
                    },
                    DirectoryEntry {
                        name: "readme.txt".to_owned(),
                        path: "/srv/readme.txt".to_owned(),
                        kind: DirectoryEntryKind::File,
                        size: 12,
                    },
                ],
                truncated: false,
            },
        );
        session.directory_listings.insert(
            "/srv/app".to_owned(),
            DirectoryListing {
                entries: vec![DirectoryEntry {
                    name: "src".to_owned(),
                    path: "/srv/app/src".to_owned(),
                    kind: DirectoryEntryKind::Directory,
                    size: 0,
                }],
                truncated: false,
            },
        );
        session
    }

    #[test]
    fn ssh_move_plan_rejects_root_same_parent_and_directory_cycles() {
        let session = move_test_session();
        assert_eq!(
            plan_move_entry(&session, "/srv/readme.txt", false, "/srv/archive").unwrap(),
            ("/srv".to_owned(), "/srv/archive/readme.txt".to_owned())
        );
        assert_eq!(
            plan_move_entry(&session, "/srv/app", true, "/srv/apple").unwrap(),
            ("/srv".to_owned(), "/srv/apple/app".to_owned()),
            "路径组件不同的相同字符串前缀不得误判为子树"
        );

        for (source, is_directory, target) in [
            ("/srv", true, "/srv/archive"),
            ("/srv/readme.txt", false, "/srv"),
            ("/srv/app", true, "/srv/app"),
            ("/srv/app", true, "/srv/app/src"),
        ] {
            assert!(
                plan_move_entry(&session, source, is_directory, target).is_err(),
                "{source} -> {target} 必须被拒绝"
            );
        }
        assert!(
            plan_move_entry(&session, "/srv/readme.txt", true, "/srv/archive").is_err(),
            "过期或伪造的目录类型不可绕过缓存校验"
        );
        assert!(
            plan_move_entry(&session, "/srv/app", true, "/srv/missing").is_err(),
            "目标目录必须仍在当前 SSH 文件树缓存中"
        );
    }

    #[test]
    fn completed_ssh_move_invalidates_subtree_and_refreshes_both_directories() {
        let mut session = move_test_session();
        session.directory_listings.insert(
            "/srv/app/src".to_owned(),
            DirectoryListing {
                entries: Vec::new(),
                truncated: false,
            },
        );
        session.directory_listings.insert(
            "/srv/apple".to_owned(),
            DirectoryListing {
                entries: Vec::new(),
                truncated: false,
            },
        );
        session.open_directories = HashSet::from([
            "/srv".to_owned(),
            "/srv/app".to_owned(),
            "/srv/app/src".to_owned(),
            "/srv/apple".to_owned(),
        ]);
        session.selected_file = Some(("/srv/app/src/main.rs".to_owned(), false));
        session.search_entries = vec![
            DirectoryEntry {
                name: "main.rs".to_owned(),
                path: "/srv/app/src/main.rs".to_owned(),
                kind: DirectoryEntryKind::File,
                size: 1,
            },
            DirectoryEntry {
                name: "keep.txt".to_owned(),
                path: "/srv/apple/keep.txt".to_owned(),
                kind: DirectoryEntryKind::File,
                size: 1,
            },
        ];
        session
            .pending_directories
            .insert(41, "/srv/app/src".to_owned());
        session
            .latest_directory_request
            .insert("/srv/app/src".to_owned(), 41);
        session
            .pending_directories
            .insert(42, "/srv/apple".to_owned());
        session
            .latest_directory_request
            .insert("/srv/apple".to_owned(), 42);
        session.pending_file_requests.insert(
            9,
            PendingFileRequest::Operation {
                action: SshFileAction::Rename,
                refresh_directories: vec!["/srv/archive".to_owned(), "/srv".to_owned()],
                invalidate_subtree: Some("/srv/app".to_owned()),
                local_path: None,
            },
        );

        let mut events = Vec::new();
        assert!(apply_file_event(
            7,
            &mut session,
            FileEvent::OperationComplete {
                token: 9,
                operation: FileOperation::Rename,
                version: None,
            },
            &mut events,
        ));
        assert!(matches!(
            events.as_slice(),
            [SshFileRuntimeEvent::OperationComplete {
                session_id: 7,
                operation: FileOperation::Rename,
                ..
            }]
        ));
        assert!(!session.directory_listings.contains_key("/srv/app"));
        assert!(!session.directory_listings.contains_key("/srv/app/src"));
        assert!(
            !session.directory_listings.contains_key("/srv"),
            "源父目录缓存必须失效"
        );
        assert!(
            session.directory_listings.contains_key("/srv/apple"),
            "不相关子树缓存必须保留"
        );
        assert!(session.open_directories.contains("/srv"));
        assert!(
            session.open_directories.contains("/srv/archive"),
            "目标即便此前折叠且仅由源父 listing 得知，也必须进入刷新集合"
        );
        assert!(!session.open_directories.contains("/srv/app"));
        assert!(!session.open_directories.contains("/srv/app/src"));
        assert!(session.selected_file.is_none());
        assert_eq!(
            session
                .search_entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/srv/apple/keep.txt"]
        );
        assert!(!session.pending_directories.contains_key(&41));
        assert!(!session
            .latest_directory_request
            .contains_key("/srv/app/src"));
        assert_eq!(
            session.pending_directories.get(&42).map(String::as_str),
            Some("/srv/apple")
        );
    }

    #[test]
    fn directory_tree_drops_stale_replies_and_only_accepts_exact_children() {
        let p = profile(AuthMethod::Agent);
        let mut session = RuntimeSession::new(&p);
        session.open_directories.insert("/".to_owned());
        session.pending_directories.insert(10, "/".to_owned());
        session.pending_directories.insert(11, "/".to_owned());
        session.latest_directory_request.insert("/".to_owned(), 11);

        assert!(!apply_directory_event(
            &mut session,
            Event::DirectoryListing {
                token: 10,
                entries: vec![DirectoryEntry {
                    name: "stale".to_owned(),
                    path: "/stale".to_owned(),
                    kind: DirectoryEntryKind::Directory,
                    size: 0,
                }],
                truncated: false,
            }
        ));
        assert!(!session.directory_listings.contains_key("/"));

        assert!(apply_directory_event(
            &mut session,
            Event::DirectoryListing {
                token: 11,
                entries: vec![
                    DirectoryEntry {
                        name: "etc".to_owned(),
                        path: "/etc".to_owned(),
                        kind: DirectoryEntryKind::Directory,
                        size: 0,
                    },
                    DirectoryEntry {
                        name: "escape".to_owned(),
                        path: "/tmp/escape".to_owned(),
                        kind: DirectoryEntryKind::File,
                        size: 1,
                    },
                ],
                truncated: false,
            }
        ));
        let listing = session.directory_listings.get("/").expect("root listing");
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].path, "/etc");
    }

    #[test]
    fn disconnect_clears_directory_requests_without_affecting_terminal_error_semantics() {
        let p = profile(AuthMethod::Agent);
        let mut session = RuntimeSession::new(&p);
        session.state = ConnectionState::Connected;
        session
            .pending_directories
            .insert(7, "/home/alice".to_owned());
        session
            .latest_directory_request
            .insert("/home/alice".to_owned(), 7);

        assert!(!apply_event(
            &mut session,
            Event::Disconnected {
                reason: DisconnectReason::Requested,
            }
        ));
        assert!(session.pending_directories.is_empty());
        assert!(session.latest_directory_request.is_empty());
        assert_eq!(session.state, ConnectionState::Disconnected);
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
