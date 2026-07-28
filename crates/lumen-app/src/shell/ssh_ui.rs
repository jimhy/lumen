//! SSH 服务器列表与编辑表单。
//!
//! 本模块只负责展示和收集用户意图。它不持有 [`SshInventory`] 的可变引用，
//! 也不直接写磁盘或发起连接；调用方应依次处理 [`SshUiOutput::actions`]。
//! 可同步的 [`NewSshProfile`] 与仅存在于本机内存中的密码始终由不同类型承载。

use std::{
    collections::HashSet,
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use egui::{Color32, RichText};

use super::{secure_password_edit, theme::Palette};
use crate::ssh::{
    AuthMethod, GroupId, HostKeyTrust, NewSshProfile, ProfileId, SshGroup, SshInventory, SshProfile,
};

const ROW_HEIGHT: f32 = 30.0;
const PROFILE_ROW_HEIGHT: f32 = 40.0;
const SECTION_GAP: f32 = 8.0;
static NEXT_PROFILE_FORM_ID: AtomicU64 = AtomicU64::new(1);

/// SSH 页面文案。
///
/// 目前由调用方提供，便于在不改动 i18n 模块的前提下独立接入。所有字段都对应
/// 一个预期的 i18n 字段，默认值仅用于开发预览与测试。
#[derive(Debug, Clone, Copy)]
pub struct SshUiText {
    pub title: &'static str,
    pub search_hint: &'static str,
    pub new_profile: &'static str,
    pub new_group: &'static str,
    pub ungrouped: &'static str,
    pub empty_group: &'static str,
    pub no_search_results: &'static str,
    pub edit: &'static str,
    pub delete: &'static str,
    pub rename_group: &'static str,
    pub delete_group: &'static str,
    pub create_group_title: &'static str,
    pub rename_group_title: &'static str,
    pub delete_group_title: &'static str,
    pub delete_group_message: &'static str,
    pub delete_profile_title: &'static str,
    pub delete_profile_message: &'static str,
    pub group_name: &'static str,
    pub create_profile_title: &'static str,
    pub edit_profile_title: &'static str,
    pub profile_name: &'static str,
    pub host: &'static str,
    pub port: &'static str,
    pub username: &'static str,
    pub auth_method: &'static str,
    pub auth_password: &'static str,
    pub auth_private_key: &'static str,
    pub auth_agent: &'static str,
    pub password_required_hint: &'static str,
    pub password_saved_hint: &'static str,
    pub private_key_file: &'static str,
    pub choose_private_key: &'static str,
    pub key_passphrase: &'static str,
    pub credentials_local_only: &'static str,
    pub private_key_required_hint: &'static str,
    pub private_key_saved_hint: &'static str,
    pub private_key_invalid_hint: &'static str,
    pub group: &'static str,
    pub initial_directory: &'static str,
    pub connect_timeout: &'static str,
    pub keep_alive: &'static str,
    pub keep_alive_disabled: &'static str,
    pub monitor_enabled: &'static str,
    pub seconds: &'static str,
    pub save: &'static str,
    pub create: &'static str,
    pub test_connection: &'static str,
    pub test_connecting: &'static str,
    pub test_success: &'static str,
    pub test_failed: &'static str,
    pub test_host_key_unknown: &'static str,
    pub test_host_key_changed: &'static str,
    pub test_trust_and_retry: &'static str,
    pub host_key_algorithm: &'static str,
    pub host_key_fingerprint: &'static str,
    pub host_key_expected: &'static str,
    pub host_key_presented: &'static str,
    pub cancel: &'static str,
    pub confirm_delete: &'static str,
}

impl Default for SshUiText {
    fn default() -> Self {
        Self {
            title: "SSH 服务器",
            search_hint: "搜索服务器",
            new_profile: "新建服务器",
            new_group: "新建组",
            ungrouped: "未分组",
            empty_group: "暂无服务器",
            no_search_results: "没有匹配的服务器",
            edit: "编辑",
            delete: "删除",
            rename_group: "重命名",
            delete_group: "删除组",
            create_group_title: "新建 SSH 分组",
            rename_group_title: "重命名 SSH 分组",
            delete_group_title: "删除 SSH 分组",
            delete_group_message: "删除分组后，其中的服务器将移到“未分组”。",
            delete_profile_title: "删除 SSH 服务器",
            delete_profile_message: "此操作将删除服务器配置。",
            group_name: "组名称",
            create_profile_title: "新建 SSH 服务器",
            edit_profile_title: "编辑 SSH 服务器",
            profile_name: "名称",
            host: "主机",
            port: "端口",
            username: "用户名",
            auth_method: "认证方式",
            auth_password: "密码",
            auth_private_key: "私钥",
            auth_agent: "SSH Agent",
            password_required_hint: "密码只安全保存在当前设备，不会同步。",
            password_saved_hint: "留空则沿用当前设备已保存的密码。",
            private_key_file: "私钥文件",
            choose_private_key: "选择文件…",
            key_passphrase: "私钥口令（可选）",
            credentials_local_only:
                "密码和私钥口令将安全保存在本机，私钥文件路径仅保留在本机；均不会同步。",
            private_key_required_hint: "请选择本机私钥文件；文件和路径不会上传或同步。",
            private_key_saved_hint: "选择新文件可替换本机私钥；不选择则沿用当前设备的绑定。",
            private_key_invalid_hint: "所选私钥文件不存在，请重新选择。",
            group: "分组",
            initial_directory: "初始目录",
            connect_timeout: "连接超时",
            keep_alive: "Keepalive",
            keep_alive_disabled: "关闭",
            monitor_enabled: "显示服务器监控信息",
            seconds: "秒",
            save: "保存",
            create: "创建",
            test_connection: "测试连接",
            test_connecting: "正在测试连接…",
            test_success: "连接和认证成功",
            test_failed: "测试连接失败",
            test_host_key_unknown: "首次连接需要确认服务器主机密钥。",
            test_host_key_changed: "服务器主机密钥已变化，测试已阻断。",
            test_trust_and_retry: "信任并重新测试",
            host_key_algorithm: "算法",
            host_key_fingerprint: "SHA-256 指纹",
            host_key_expected: "已保存",
            host_key_presented: "服务器返回",
            cancel: "取消",
            confirm_delete: "确认删除",
        }
    }
}

impl SshUiText {
    /// 从当前语言表构造 SSH 页面文案。
    pub fn localized() -> Self {
        let strings = crate::i18n::strings();
        Self {
            title: strings.ssh_title,
            search_hint: strings.ssh_search_hint,
            new_profile: strings.ssh_new_profile,
            new_group: strings.ssh_new_group,
            ungrouped: strings.ssh_ungrouped,
            empty_group: strings.ssh_empty_group,
            no_search_results: strings.ssh_no_search_results,
            edit: strings.ssh_edit,
            delete: strings.ssh_delete,
            rename_group: strings.ssh_rename_group,
            delete_group: strings.ssh_delete_group,
            create_group_title: strings.ssh_create_group_title,
            rename_group_title: strings.ssh_rename_group_title,
            delete_group_title: strings.ssh_delete_group_title,
            delete_group_message: strings.ssh_delete_group_message,
            delete_profile_title: strings.ssh_delete_profile_title,
            delete_profile_message: strings.ssh_delete_profile_message,
            group_name: strings.ssh_group_name,
            create_profile_title: strings.ssh_create_profile_title,
            edit_profile_title: strings.ssh_edit_profile_title,
            profile_name: strings.ssh_profile_name,
            host: strings.ssh_host,
            port: strings.ssh_port,
            username: strings.ssh_username,
            auth_method: strings.ssh_auth_method,
            auth_password: strings.ssh_auth_password,
            auth_private_key: strings.ssh_auth_private_key,
            auth_agent: strings.ssh_auth_agent,
            password_required_hint: strings.ssh_password_required_hint,
            password_saved_hint: strings.ssh_password_saved_hint,
            private_key_file: strings.ssh_private_key_file,
            choose_private_key: strings.ssh_choose_private_key,
            key_passphrase: strings.ssh_key_passphrase,
            credentials_local_only: strings.ssh_credentials_memory_only,
            private_key_required_hint: strings.ssh_private_key_required_hint,
            private_key_saved_hint: strings.ssh_private_key_saved_hint,
            private_key_invalid_hint: strings.ssh_private_key_invalid_hint,
            group: strings.ssh_group,
            initial_directory: strings.ssh_initial_directory,
            connect_timeout: strings.ssh_connect_timeout,
            keep_alive: strings.ssh_keep_alive,
            keep_alive_disabled: strings.ssh_keep_alive_disabled,
            monitor_enabled: strings.ssh_monitor_enabled,
            seconds: strings.ssh_seconds,
            save: strings.ssh_save,
            create: strings.ssh_create,
            test_connection: strings.ssh_test_connection,
            test_connecting: strings.ssh_test_connecting,
            test_success: strings.ssh_test_success,
            test_failed: strings.ssh_test_failed,
            test_host_key_unknown: strings.ssh_test_host_key_unknown,
            test_host_key_changed: strings.ssh_test_host_key_changed,
            test_trust_and_retry: strings.ssh_test_trust_and_retry,
            host_key_algorithm: strings.ssh_host_key_algorithm,
            host_key_fingerprint: strings.ssh_host_key_fingerprint,
            host_key_expected: strings.ssh_host_key_expected,
            host_key_presented: strings.ssh_host_key_presented,
            cancel: strings.ssh_cancel,
            confirm_delete: strings.ssh_confirm_delete,
        }
    }
}

/// 单帧只读输入。
pub struct SshUiInput<'a> {
    pub inventory: &'a SshInventory,
    pub text: &'a SshUiText,
    pub connection_test: Option<&'a crate::ssh_runtime::ConnectionTestView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSubmitIntent {
    Save,
    TestConnection,
}

/// 新建/编辑表单的一次性提交。
///
/// `draft` 只包含允许持久化和同步的元数据；`password` 仅在主线程消费，
/// 未消费或处理失败时由 [`Drop`] 清零。
pub struct SshProfileSubmission {
    form_id: u64,
    connection_revision: u64,
    host_key_verified_for_current_endpoint: bool,
    editing_id: Option<ProfileId>,
    draft: NewSshProfile,
    password: String,
    private_key_path: Option<PathBuf>,
    key_passphrase: String,
    intent: ProfileSubmitIntent,
}

impl SshProfileSubmission {
    pub(crate) const fn form_id(&self) -> u64 {
        self.form_id
    }

    pub(crate) const fn connection_revision(&self) -> u64 {
        self.connection_revision
    }

    pub(crate) const fn host_key_verified_for_current_endpoint(&self) -> bool {
        self.host_key_verified_for_current_endpoint
    }

    #[cfg(test)]
    fn editing_id(&self) -> Option<&str> {
        self.editing_id.as_deref()
    }

    pub(crate) fn take_editing_id(&mut self) -> Option<ProfileId> {
        self.editing_id.take()
    }

    pub(crate) fn take_draft(&mut self) -> NewSshProfile {
        std::mem::take(&mut self.draft)
    }

    #[cfg(test)]
    fn password(&self) -> &str {
        &self.password
    }

    pub(crate) fn take_password(&mut self) -> String {
        std::mem::take(&mut self.password)
    }

    pub(crate) fn take_private_key_path(&mut self) -> Option<PathBuf> {
        self.private_key_path.take()
    }

    pub(crate) fn take_key_passphrase(&mut self) -> String {
        std::mem::take(&mut self.key_passphrase)
    }

    pub(crate) const fn intent(&self) -> ProfileSubmitIntent {
        self.intent
    }
}

impl fmt::Debug for SshProfileSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshProfileSubmission")
            .field("form_id", &self.form_id)
            .field("connection_revision", &self.connection_revision)
            .field(
                "host_key_verified_for_current_endpoint",
                &self.host_key_verified_for_current_endpoint,
            )
            .field("editing_id", &self.editing_id)
            .field("draft", &self.draft)
            .field("password", &"<redacted>")
            .field("private_key_path", &"<redacted>")
            .field("key_passphrase", &"<redacted>")
            .field("intent", &self.intent)
            .finish()
    }
}

impl Drop for SshProfileSubmission {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.password.zeroize();
        self.key_passphrase.zeroize();
    }
}

/// UI 向存储与连接层发送的动作。
#[derive(Debug)]
pub enum SshUiAction {
    CreateGroup {
        name: String,
    },
    RenameGroup {
        id: GroupId,
        name: String,
    },
    DeleteGroup {
        id: GroupId,
    },
    DeleteProfile {
        id: ProfileId,
    },
    MoveProfile {
        id: ProfileId,
        target_group_id: Option<GroupId>,
        target_index: usize,
    },
    ConnectProfile {
        id: ProfileId,
    },
    SubmitProfile(Box<SshProfileSubmission>),
    CancelConnectionTest {
        form_id: u64,
    },
}

/// 单帧输出。动作按用户在本帧的操作顺序排列。
#[derive(Debug, Default)]
pub struct SshUiOutput {
    pub actions: Vec<SshUiAction>,
}

#[derive(Debug)]
enum Dialog {
    CreateGroup { name: String },
    RenameGroup { id: GroupId, name: String },
    DeleteGroup { id: GroupId, name: String },
    EditProfile(Box<ProfileForm>),
    DeleteProfile { id: ProfileId, name: String },
}

struct ProfileForm {
    form_id: u64,
    connection_revision: u64,
    host_key_verified_for_current_endpoint: bool,
    editing_id: Option<ProfileId>,
    name: String,
    host: String,
    port: u16,
    username: String,
    auth_method: AuthMethod,
    password: String,
    private_key_path: Option<PathBuf>,
    key_passphrase: String,
    can_reuse_saved_private_key: bool,
    group_id: Option<GroupId>,
    initial_directory: String,
    connect_timeout_secs: u32,
    keep_alive_enabled: bool,
    keep_alive_secs: u32,
    monitor_enabled: bool,
    trusted_host_key: Option<crate::ssh::HostKeyTrust>,
}

#[derive(Debug, PartialEq, Eq)]
struct ConnectionFields {
    host: String,
    port: u16,
    username: String,
}

impl fmt::Debug for ProfileForm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileForm")
            .field("form_id", &self.form_id)
            .field("connection_revision", &self.connection_revision)
            .field(
                "host_key_verified_for_current_endpoint",
                &self.host_key_verified_for_current_endpoint,
            )
            .field("editing_id", &self.editing_id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("auth_method", &self.auth_method)
            .field("password", &"<redacted>")
            .field("private_key_path", &"<redacted>")
            .field("key_passphrase", &"<redacted>")
            .field(
                "can_reuse_saved_private_key",
                &self.can_reuse_saved_private_key,
            )
            .field("group_id", &self.group_id)
            .field("initial_directory", &self.initial_directory)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("keep_alive_enabled", &self.keep_alive_enabled)
            .field("keep_alive_secs", &self.keep_alive_secs)
            .field("monitor_enabled", &self.monitor_enabled)
            .field("trusted_host_key", &self.trusted_host_key)
            .finish()
    }
}

impl Drop for ProfileForm {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.password.zeroize();
        self.key_passphrase.zeroize();
    }
}

impl ProfileForm {
    fn create() -> Self {
        Self::from_draft(None, NewSshProfile::default())
    }

    fn edit(profile: &SshProfile) -> Self {
        Self::from_draft(
            Some(profile.id.clone()),
            NewSshProfile {
                name: profile.name.clone(),
                host: profile.host.clone(),
                port: profile.port,
                username: profile.username.clone(),
                auth_method: profile.auth_method,
                group_id: profile.group_id.clone(),
                initial_directory: profile.initial_directory.clone(),
                connect_timeout_secs: profile.connect_timeout_secs,
                keep_alive_secs: profile.keep_alive_secs,
                monitor_enabled: profile.monitor_enabled,
                trusted_host_key: profile.trusted_host_key.clone(),
            },
        )
    }

    fn from_draft(editing_id: Option<ProfileId>, draft: NewSshProfile) -> Self {
        let can_reuse_saved_private_key =
            editing_id.is_some() && draft.auth_method == AuthMethod::PrivateKey;
        Self {
            form_id: next_profile_form_id(),
            connection_revision: 0,
            host_key_verified_for_current_endpoint: false,
            editing_id,
            name: draft.name,
            host: draft.host,
            port: draft.port,
            username: draft.username,
            auth_method: draft.auth_method,
            password: String::new(),
            private_key_path: None,
            key_passphrase: String::new(),
            can_reuse_saved_private_key,
            group_id: draft.group_id,
            initial_directory: draft.initial_directory.unwrap_or_default(),
            connect_timeout_secs: draft.connect_timeout_secs,
            keep_alive_enabled: draft.keep_alive_secs.is_some(),
            keep_alive_secs: draft.keep_alive_secs.unwrap_or(30).max(1),
            monitor_enabled: draft.monitor_enabled,
            trusted_host_key: draft.trusted_host_key,
        }
    }

    fn valid(&self, inventory: &SshInventory) -> bool {
        !self.name.trim().is_empty()
            && !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && self.port != 0
            && self.connect_timeout_secs != 0
            && (!self.keep_alive_enabled || self.keep_alive_secs != 0)
            && self
                .group_id
                .as_deref()
                .is_none_or(|id| inventory.group(id).is_some())
            && match self.auth_method {
                AuthMethod::Password => self.editing_id.is_some() || !self.password.is_empty(),
                AuthMethod::PrivateKey => match self.private_key_path.as_deref() {
                    Some(path) => valid_private_key_path(path),
                    None => self.can_reuse_saved_private_key,
                },
                AuthMethod::Agent => true,
            }
    }

    fn to_draft(&self) -> NewSshProfile {
        NewSshProfile {
            name: self.name.trim().to_owned(),
            host: self.host.trim().to_owned(),
            port: self.port,
            username: self.username.trim().to_owned(),
            auth_method: self.auth_method,
            group_id: self.group_id.clone(),
            initial_directory: optional_trimmed(&self.initial_directory),
            connect_timeout_secs: self.connect_timeout_secs,
            keep_alive_secs: self.keep_alive_enabled.then_some(self.keep_alive_secs),
            monitor_enabled: self.monitor_enabled,
            trusted_host_key: self.trusted_host_key.clone(),
        }
    }

    fn test_submission(&mut self) -> SshProfileSubmission {
        // 即使连接字段未变化，重复点击测试也必须产生新的 revision，
        // 防止上一轮异步回包覆盖或伪装成本轮结果。
        self.bump_connection_revision();
        SshProfileSubmission {
            form_id: self.form_id,
            connection_revision: self.connection_revision,
            host_key_verified_for_current_endpoint: self.host_key_verified_for_current_endpoint,
            editing_id: self.editing_id.clone(),
            draft: self.to_draft(),
            // 测试期间表单保持打开，因此只在这条一次性消息中创建一份
            // 受 Drop 保护的副本；原缓冲仍由 ProfileForm 负责清零。
            password: self.password.clone(),
            private_key_path: self.private_key_path.clone(),
            key_passphrase: self.key_passphrase.clone(),
            intent: ProfileSubmitIntent::TestConnection,
        }
    }

    fn save_submission(&mut self) -> SshProfileSubmission {
        SshProfileSubmission {
            form_id: self.form_id,
            connection_revision: self.connection_revision,
            host_key_verified_for_current_endpoint: self.host_key_verified_for_current_endpoint,
            editing_id: self.editing_id.clone(),
            draft: self.to_draft(),
            password: std::mem::take(&mut self.password),
            private_key_path: self.private_key_path.take(),
            key_passphrase: std::mem::take(&mut self.key_passphrase),
            intent: ProfileSubmitIntent::Save,
        }
    }

    fn connection_fields(&self) -> ConnectionFields {
        ConnectionFields {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
        }
    }

    fn reconcile_connection_field_changes(
        &mut self,
        previous_fields: &ConnectionFields,
        previous_auth: AuthMethod,
        credential_changed: bool,
    ) {
        let endpoint_changed = self.host != previous_fields.host
            || self.port != previous_fields.port
            || self.username != previous_fields.username;
        let auth_changed = self.auth_method != previous_auth;
        if endpoint_changed {
            self.trusted_host_key = None;
            self.host_key_verified_for_current_endpoint = false;
        }
        if endpoint_changed || auth_changed {
            self.can_reuse_saved_private_key = false;
        }
        if endpoint_changed || auth_changed || credential_changed {
            self.bump_connection_revision();
        }
    }

    fn trust_host_key(&mut self, trust: HostKeyTrust) {
        if self.trusted_host_key.as_ref() != Some(&trust)
            || !self.host_key_verified_for_current_endpoint
        {
            self.trusted_host_key = Some(trust);
            self.host_key_verified_for_current_endpoint = true;
            self.bump_connection_revision();
        }
    }

    fn bump_connection_revision(&mut self) {
        self.connection_revision = self.connection_revision.saturating_add(1);
    }
}

fn valid_private_key_path(path: &Path) -> bool {
    path.is_absolute() && path.is_file()
}

fn next_profile_form_id() -> u64 {
    NEXT_PROFILE_FORM_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DropTarget {
    group_id: Option<GroupId>,
    /// 插入位置，以源服务器已经从原列表中移除后的列表为准。
    index: usize,
}

/// 监控卡片稳定 id（折叠状态持久化到 settings 的合法值全集，单一来源）。
pub const SSH_CARD_SYSTEM: &str = "system";
pub const SSH_CARD_CPU: &str = "cpu";
pub const SSH_CARD_MEMORY: &str = "memory";
pub const SSH_CARD_NETWORK: &str = "network";
pub const SSH_CARD_DISK: &str = "disk";
pub const SSH_CARD_PROCESSES: &str = "processes";

/// 全部合法监控卡片 id（settings 恢复时过滤未知值用）。
pub const SSH_MONITOR_CARD_IDS: &[&str] = &[
    SSH_CARD_SYSTEM,
    SSH_CARD_CPU,
    SSH_CARD_MEMORY,
    SSH_CARD_NETWORK,
    SSH_CARD_DISK,
    SSH_CARD_PROCESSES,
];

/// SSH 页面跨帧状态。
#[derive(Debug, Default)]
pub struct SshUiState {    search: String,
    selected_profile_id: Option<ProfileId>,
    collapsed_groups: HashSet<GroupId>,
    ungrouped_collapsed: bool,
    dialog: Option<Dialog>,
    dragged_profile_id: Option<ProfileId>,
    drop_target: Option<DropTarget>,
    /// 监控面板收起（右侧只剩一条展开手柄）。应用级偏好，跨会话共享。
    monitor_collapsed: bool,
    /// 监控面板各卡片的折叠状态（key = 卡片稳定 id）。
    collapsed_monitor_cards: HashSet<&'static str>,
    /// 监控显隐偏好自上次持久化后被修改过（main 据此写 settings）。
    monitor_prefs_dirty: bool,
    /// 进程详情弹窗是否打开。
    process_window_open: bool,
    /// 进程卡「按名称搜索」输入框文本。
    process_query: String,
    /// 进程卡「按端口查询」输入框文本。
    port_query: String,
    /// 等待二次确认的待终止 PID。
    kill_confirm: Option<u32>,
}

impl SshUiState {
    #[cfg(test)]
    pub fn set_search(&mut self, search: impl Into<String>) {
        self.search = search.into();
        if !self.search.trim().is_empty() {
            self.cancel_drag();
        }
    }

    pub fn selected_profile_id(&self) -> Option<&str> {
        self.selected_profile_id.as_deref()
    }

    pub fn select_profile(&mut self, id: Option<ProfileId>) {
        self.selected_profile_id = id;
    }

    /// 应用上锁时清理所有可能悬浮于解锁界面之上的瞬时 UI。
    pub fn close_for_app_lock(&mut self) {
        self.dialog = None;
        self.cancel_drag();
    }

    pub fn monitor_collapsed(&self) -> bool {
        self.monitor_collapsed
    }

    pub fn toggle_monitor_collapsed(&mut self) {
        self.monitor_collapsed = !self.monitor_collapsed;
        self.monitor_prefs_dirty = true;
    }

    pub fn set_monitor_collapsed(&mut self, collapsed: bool) {
        if self.monitor_collapsed != collapsed {
            self.monitor_collapsed = collapsed;
            self.monitor_prefs_dirty = true;
        }
    }

    pub fn monitor_card_collapsed(&self, card_id: &'static str) -> bool {
        self.collapsed_monitor_cards.contains(card_id)
    }

    pub fn toggle_monitor_card(&mut self, card_id: &'static str) {
        if !self.collapsed_monitor_cards.remove(card_id) {
            self.collapsed_monitor_cards.insert(card_id);
        }
        self.monitor_prefs_dirty = true;
    }

    /// 启动时从 settings 恢复监控显隐偏好（一次性灌入，不置脏）。
    pub fn load_monitor_prefs(&mut self, collapsed: bool, cards: &[String]) {
        self.monitor_collapsed = collapsed;
        self.collapsed_monitor_cards = cards
            .iter()
            .map(String::as_str)
            // 卡片 id 均为编译期常量；settings 里的未知 id 无法转为
            // &'static str，直接丢弃（无害，等价未折叠）。
            .filter_map(|id| SSH_MONITOR_CARD_IDS.iter().copied().find(|known| *known == id))
            .collect();
        self.monitor_prefs_dirty = false;
    }

    /// 导出卡片折叠状态供 settings 持久化。
    pub fn collapsed_monitor_cards_vec(&self) -> Vec<String> {
        let mut cards: Vec<String> = self
            .collapsed_monitor_cards
            .iter()
            .map(|id| (*id).to_owned())
            .collect();
        cards.sort_unstable();
        cards
    }

    /// 监控显隐偏好是否有未持久化的修改（main 消费后写盘）。
    pub fn take_monitor_prefs_dirty(&mut self) -> bool {
        std::mem::take(&mut self.monitor_prefs_dirty)
    }

    pub fn process_window_open(&self) -> bool {
        self.process_window_open
    }

    pub fn set_process_window_open(&mut self, open: bool) {
        self.process_window_open = open;
    }

    pub fn process_query(&self) -> &str {
        &self.process_query
    }

    pub fn process_query_mut(&mut self) -> &mut String {
        &mut self.process_query
    }

    pub fn port_query(&self) -> &str {
        &self.port_query
    }

    pub fn port_query_mut(&mut self) -> &mut String {
        &mut self.port_query
    }

    pub fn kill_confirm(&self) -> Option<u32> {
        self.kill_confirm
    }

    pub fn set_kill_confirm(&mut self, pid: Option<u32>) {
        self.kill_confirm = pid;
    }

    fn cancel_drag(&mut self) {
        self.dragged_profile_id = None;
        self.drop_target = None;
    }

    fn reconcile(&mut self, inventory: &SshInventory) {
        if self
            .selected_profile_id
            .as_deref()
            .is_some_and(|id| inventory.profile(id).is_none())
        {
            self.selected_profile_id = None;
        }
        if self
            .dragged_profile_id
            .as_deref()
            .is_some_and(|id| inventory.profile(id).is_none())
        {
            self.cancel_drag();
        }
        self.collapsed_groups
            .retain(|id| inventory.group(id).is_some());
    }
}

/// 在调用方给定的侧栏区域内绘制 SSH 服务器栏与弹窗。
pub fn show(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    input: SshUiInput<'_>,
    pal: &Palette,
) -> SshUiOutput {
    state.reconcile(input.inventory);
    let mut out = SshUiOutput::default();

    if ui.input(|events| events.key_pressed(egui::Key::Escape)) {
        if state.dragged_profile_id.is_some() {
            state.cancel_drag();
        } else {
            if let Some(Dialog::EditProfile(form)) = state.dialog.as_ref() {
                out.actions.push(SshUiAction::CancelConnectionTest {
                    form_id: form.form_id,
                });
            }
            state.dialog = None;
        }
    }

    ui.visuals_mut().override_text_color = Some(pal.fg);
    draw_header(ui, state, input.text, pal);
    ui.add_space(SECTION_GAP);
    draw_search(ui, state, input.text, pal);
    ui.add_space(SECTION_GAP);

    if !state.search.trim().is_empty() {
        state.cancel_drag();
    } else if state.dragged_profile_id.is_some() {
        state.drop_target = None;
    }

    let query = normalized_query(&state.search);
    let mut any_visible = false;
    egui::ScrollArea::vertical()
        .id_salt("ssh_server_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut groups: Vec<_> = input.inventory.groups().iter().collect();
            groups.sort_by_key(|group| group.sort_order);
            for group in groups {
                let all_profiles = input.inventory.profiles_in_group(Some(&group.id));
                let profiles = filtered_profiles(&all_profiles, group, &query);
                if !query.is_empty() && profiles.is_empty() {
                    continue;
                }
                any_visible = true;
                draw_group_section(
                    ui,
                    state,
                    input.inventory,
                    group,
                    &profiles,
                    input.text,
                    pal,
                    &mut out,
                );
                ui.add_space(4.0);
            }

            // “未分组”始终在所有用户分组之后。
            let all_ungrouped = input.inventory.profiles_in_group(None);
            let ungrouped: Vec<_> = all_ungrouped
                .iter()
                .copied()
                .filter(|profile| profile_matches(profile, &query))
                .collect();
            if query.is_empty() || !ungrouped.is_empty() {
                any_visible = true;
                draw_ungrouped_section(
                    ui,
                    state,
                    input.inventory,
                    &ungrouped,
                    input.text,
                    pal,
                    &mut out,
                );
            }
        });

    if !any_visible {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(input.text.no_search_results).color(pal.fg_dim));
        });
    }

    finish_drop(ui, state, input.inventory, &mut out);
    draw_dialog(ui.ctx(), state, input, pal, &mut out);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderIcon {
    NewProfile,
    NewGroup,
}

fn header_icon_button(
    ui: &mut egui::Ui,
    icon: HeaderIcon,
    tooltip: &str,
    pal: &Palette,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(28.0, 26.0), egui::Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 4.0, pal.bg_highlight);
    }

    let color = if hovered { pal.fg } else { pal.fg_dim };
    let stroke = egui::Stroke::new(1.2_f32, color);
    let center = rect.center();
    match icon {
        HeaderIcon::NewProfile => {
            let server = egui::Rect::from_center_size(
                center + egui::vec2(-2.5, -1.5),
                egui::vec2(13.0, 14.0),
            );
            ui.painter()
                .rect_stroke(server, 1.5, stroke, egui::StrokeKind::Middle);
            for y in [server.top() + 4.5, server.top() + 9.5] {
                ui.painter()
                    .circle_filled(egui::pos2(server.left() + 3.0, y), 1.0, color);
                ui.painter().line_segment(
                    [
                        egui::pos2(server.left() + 5.5, y),
                        egui::pos2(server.right() - 2.0, y),
                    ],
                    egui::Stroke::new(1.0_f32, color),
                );
            }
            paint_plus(ui.painter(), center + egui::vec2(6.0, 5.0), 3.0, color);
        }
        HeaderIcon::NewGroup => {
            let left = center.x - 8.0;
            let right = center.x + 6.0;
            let top = center.y - 6.0;
            let bottom = center.y + 6.0;
            ui.painter().add(egui::Shape::line(
                vec![
                    egui::pos2(left, bottom),
                    egui::pos2(left, top + 1.5),
                    egui::pos2(left + 5.0, top + 1.5),
                    egui::pos2(left + 7.0, top + 4.0),
                    egui::pos2(right, top + 4.0),
                    egui::pos2(right, bottom),
                    egui::pos2(left, bottom),
                ],
                stroke,
            ));
            paint_plus(ui.painter(), center + egui::vec2(6.0, 4.5), 3.0, color);
        }
    }

    response.on_hover_text(tooltip)
}

fn paint_plus(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32) {
    let stroke = egui::Stroke::new(1.3_f32, color);
    painter.line_segment(
        [
            egui::pos2(center.x - radius, center.y),
            egui::pos2(center.x + radius, center.y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y - radius),
            egui::pos2(center.x, center.y + radius),
        ],
        stroke,
    );
}

fn draw_chevron(ui: &mut egui::Ui, collapsed: bool, pal: &Palette) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 20.0), egui::Sense::hover());
    let color = if response.hovered() {
        pal.fg
    } else {
        pal.fg_dim
    };
    let stroke = egui::Stroke::new(1.4_f32, color);
    let center = rect.center();
    let points = if collapsed {
        [
            center + egui::vec2(-2.0, -4.0),
            center + egui::vec2(2.0, 0.0),
            center + egui::vec2(-2.0, 4.0),
        ]
    } else {
        [
            center + egui::vec2(-4.0, -2.0),
            center + egui::vec2(0.0, 2.0),
            center + egui::vec2(4.0, -2.0),
        ]
    };
    ui.painter().line_segment([points[0], points[1]], stroke);
    ui.painter().line_segment([points[1], points[2]], stroke);
}

fn overflow_button(ui: &mut egui::Ui, tooltip: &str, pal: &Palette) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(28.0, 24.0), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 3.0, pal.bg_highlight);
    }
    let color = if response.hovered() {
        pal.fg
    } else {
        pal.fg_dim
    };
    let center = rect.center();
    for offset in [-4.0, 0.0, 4.0] {
        ui.painter()
            .circle_filled(center + egui::vec2(offset, 0.0), 1.25, color);
    }
    response.on_hover_text(tooltip)
}

fn draw_header(ui: &mut egui::Ui, state: &mut SshUiState, text: &SshUiText, pal: &Palette) {
    ui.horizontal(|ui| {
        ui.heading(RichText::new(text.title).size(16.0).color(pal.fg));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if header_icon_button(ui, HeaderIcon::NewProfile, text.new_profile, pal).clicked() {
                state.dialog = Some(Dialog::EditProfile(Box::new(ProfileForm::create())));
            }
            if header_icon_button(ui, HeaderIcon::NewGroup, text.new_group, pal).clicked() {
                state.dialog = Some(Dialog::CreateGroup {
                    name: String::new(),
                });
            }
        });
    });
}

fn draw_search(ui: &mut egui::Ui, state: &mut SshUiState, text: &SshUiText, pal: &Palette) {
    let response = ui.add(
        egui::TextEdit::singleline(&mut state.search)
            .hint_text(text.search_hint)
            .desired_width(f32::INFINITY)
            .text_color(pal.fg)
            .background_color(pal.extreme_bg),
    );
    if response.changed() && !state.search.trim().is_empty() {
        state.cancel_drag();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupCommand {
    Rename,
    Delete,
}

fn group_action_menu(
    ui: &mut egui::Ui,
    text: &SshUiText,
    pal: &Palette,
    command: &mut Option<GroupCommand>,
) {
    ui.set_min_width(120.0);
    if ui.button(text.rename_group).clicked() {
        *command = Some(GroupCommand::Rename);
        ui.close();
    }
    if ui
        .button(RichText::new(text.delete_group).color(pal.error))
        .clicked()
    {
        *command = Some(GroupCommand::Delete);
        ui.close();
    }
}

fn apply_group_command(command: GroupCommand, group: &SshGroup, state: &mut SshUiState) {
    state.dialog = Some(match command {
        GroupCommand::Rename => Dialog::RenameGroup {
            id: group.id.clone(),
            name: group.name.clone(),
        },
        GroupCommand::Delete => Dialog::DeleteGroup {
            id: group.id.clone(),
            name: group.name.clone(),
        },
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileRowCommand {
    Connect,
    Edit,
    Delete,
}

fn profile_row_action_labels(text: &SshUiText) -> [&str; 2] {
    [text.edit, text.delete]
}

fn profile_overflow_menu(
    ui: &mut egui::Ui,
    text: &SshUiText,
    pal: &Palette,
    command: &mut Option<ProfileRowCommand>,
) {
    let [edit, delete] = profile_row_action_labels(text);
    ui.set_min_width(116.0);
    if ui.button(edit).clicked() {
        *command = Some(ProfileRowCommand::Edit);
        ui.close();
    }
    if ui.button(RichText::new(delete).color(pal.error)).clicked() {
        *command = Some(ProfileRowCommand::Delete);
        ui.close();
    }
}

fn profile_context_menu(
    ui: &mut egui::Ui,
    text: &SshUiText,
    pal: &Palette,
    command: &mut Option<ProfileRowCommand>,
) {
    profile_overflow_menu(ui, text, pal, command);
}

fn apply_profile_row_command(
    command: ProfileRowCommand,
    profile: &SshProfile,
    state: &mut SshUiState,
    out: &mut SshUiOutput,
) {
    match command {
        ProfileRowCommand::Connect => {
            out.actions.push(SshUiAction::ConnectProfile {
                id: profile.id.clone(),
            });
        }
        ProfileRowCommand::Edit => {
            state.dialog = Some(Dialog::EditProfile(Box::new(ProfileForm::edit(profile))));
        }
        ProfileRowCommand::Delete => {
            state.dialog = Some(Dialog::DeleteProfile {
                id: profile.id.clone(),
                name: profile.name.clone(),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_group_section(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    group: &SshGroup,
    profiles: &[&SshProfile],
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    let collapsed = state.collapsed_groups.contains(&group.id);
    let mut command = None;
    let mut overflow_clicked = false;
    let header = draw_group_header(ui, &group.name, collapsed, pal, |ui| {
        let overflow = overflow_button(
            ui,
            &format!("{} / {}", text.rename_group, text.delete_group),
            pal,
        );
        overflow_clicked = overflow.clicked();
        let _ = egui::Popup::menu(&overflow)
            .align(egui::RectAlign::BOTTOM_END)
            .width(136.0)
            .show(|ui| group_action_menu(ui, text, pal, &mut command));
    });
    header.context_menu(|ui| group_action_menu(ui, text, pal, &mut command));
    if let Some(command) = command {
        apply_group_command(command, group, state);
    }
    if header.clicked_by(egui::PointerButton::Primary) && !overflow_clicked {
        if collapsed {
            state.collapsed_groups.remove(&group.id);
        } else {
            state.collapsed_groups.insert(group.id.clone());
        }
    }
    register_group_drop_target(state, inventory, &header, Some(&group.id), profiles.len());

    if !collapsed {
        if profiles.is_empty() {
            empty_row(ui, text.empty_group, pal);
        } else {
            draw_profile_rows(
                ui,
                state,
                inventory,
                Some(&group.id),
                profiles,
                text,
                pal,
                out,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ungrouped_section(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    profiles: &[&SshProfile],
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    let header = draw_group_header(ui, text.ungrouped, state.ungrouped_collapsed, pal, |_| {});
    if header.clicked_by(egui::PointerButton::Primary) {
        state.ungrouped_collapsed = !state.ungrouped_collapsed;
    }
    register_group_drop_target(state, inventory, &header, None, profiles.len());

    if !state.ungrouped_collapsed {
        if profiles.is_empty() {
            empty_row(ui, text.empty_group, pal);
        } else {
            draw_profile_rows(ui, state, inventory, None, profiles, text, pal, out);
        }
    }
}

fn draw_group_header(
    ui: &mut egui::Ui,
    name: &str,
    collapsed: bool,
    pal: &Palette,
    trailing: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT + 4.0),
        egui::Sense::click(),
    );
    ui.painter().rect_filled(rect, 4.0, pal.bg_panel);
    if response.hovered() {
        ui.painter().rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.0_f32, pal.bg_highlight),
            egui::StrokeKind::Inside,
        );
    }

    let content_rect = rect.shrink2(egui::vec2(6.0, 3.0));
    let mut content_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(content_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    content_ui.set_clip_rect(content_rect.intersect(ui.clip_rect()));
    draw_chevron(&mut content_ui, collapsed, pal);
    content_ui.label(RichText::new(name).strong().color(pal.fg));
    content_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
    response
}

#[allow(clippy::too_many_arguments)]
fn draw_profile_rows(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    group_id: Option<&str>,
    profiles: &[&SshProfile],
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    for (visible_index, profile) in profiles.iter().enumerate() {
        let selected = state.selected_profile_id.as_deref() == Some(&profile.id);
        let sense = if state.search.trim().is_empty() {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::click()
        };
        let row_output = ui.push_id(("ssh_profile_row", &profile.id), |ui| {
            let (rect, row) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), PROFILE_ROW_HEIGHT), sense);
            let fill = if selected {
                pal.selection
            } else if row.hovered() {
                pal.bg_panel
            } else {
                Color32::TRANSPARENT
            };
            ui.painter().rect_filled(rect, 4.0, fill);

            // 操作区固定只保留一个矢量省略号按钮，避免悬停时文本左右跳动。
            // 两行文本各自限制在剩余宽度内，窄侧栏中也不会被控件覆盖。
            let content_rect = rect.shrink2(egui::vec2(6.0, 3.0));
            let action_width = 28.0;
            let action_rect = egui::Rect::from_min_max(
                egui::pos2(content_rect.right() - action_width, content_rect.top()),
                content_rect.right_bottom(),
            );
            let text_rect = egui::Rect::from_min_max(
                content_rect.left_top(),
                egui::pos2(action_rect.left() - 4.0, content_rect.bottom()),
            );

            let mut text_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(text_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            text_ui.set_clip_rect(text_rect.intersect(ui.clip_rect()));
            text_ui.spacing_mut().item_spacing.y = 0.0;
            let name = text_ui.add(
                egui::Label::new(RichText::new(&profile.name).color(pal.fg))
                    .truncate()
                    // egui 0.34 的 selectable 默认会把 click_and_drag 并回
                    // sense，吞掉行的双击并允许拖选文字——必须显式关掉。
                    .selectable(false)
                    .sense(egui::Sense::hover()),
            );
            if name.hovered() {
                name.on_hover_text(&profile.name);
            }
            let endpoint = format!("{}@{}:{}", profile.username, profile.host, profile.port);
            let endpoint_label = text_ui.add(
                egui::Label::new(RichText::new(&endpoint).small().color(pal.fg_dim))
                    .truncate()
                    .selectable(false)
                    .sense(egui::Sense::hover()),
            );
            if endpoint_label.hovered() {
                endpoint_label.on_hover_text(endpoint);
            }

            let mut command = None;
            let mut action_control_clicked = false;
            if selected || row.hovered() {
                let button_rect = egui::Rect::from_center_size(
                    action_rect.center(),
                    egui::vec2(action_width, 24.0),
                );
                let mut action_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(button_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                action_ui.set_clip_rect(button_rect.intersect(ui.clip_rect()));
                let overflow = overflow_button(
                    &mut action_ui,
                    &format!("{} / {}", text.edit, text.delete),
                    pal,
                );
                action_control_clicked |= overflow.clicked();
                let _ = egui::Popup::menu(&overflow)
                    .align(egui::RectAlign::BOTTOM_END)
                    .width(132.0)
                    .show(|ui| profile_overflow_menu(ui, text, pal, &mut command));
            }
            (row, command, action_control_clicked)
        });
        let (row, command, action_control_clicked) = row_output.inner;

        let mut context_command = None;
        row.context_menu(|ui| profile_context_menu(ui, text, pal, &mut context_command));
        let command = context_command.or(command);
        if command.is_some()
            || row.clicked_by(egui::PointerButton::Secondary)
            || action_control_clicked
        {
            state.selected_profile_id = Some(profile.id.clone());
        }
        if let Some(command) = command {
            apply_profile_row_command(command, profile, state, out);
        }

        if row.clicked_by(egui::PointerButton::Primary) && !action_control_clicked {
            state.selected_profile_id = Some(profile.id.clone());
        }
        if row.double_clicked_by(egui::PointerButton::Primary) && !action_control_clicked {
            apply_profile_row_command(ProfileRowCommand::Connect, profile, state, out);
        }
        if state.search.trim().is_empty()
            && row.drag_started_by(egui::PointerButton::Primary)
            && !action_control_clicked
        {
            state.dragged_profile_id = Some(profile.id.clone());
            state.drop_target = None;
        }
        register_row_drop_target(
            state,
            inventory,
            &row,
            group_id,
            visible_index,
            profiles.len(),
        );

        if state.dragged_profile_id.as_deref() == Some(&profile.id) && row.dragged() {
            ui.painter().rect_stroke(
                row.rect,
                4.0,
                egui::Stroke::new(1.0_f32, pal.accent),
                egui::StrokeKind::Inside,
            );
        }
    }
}

fn register_group_drop_target(
    state: &mut SshUiState,
    inventory: &SshInventory,
    response: &egui::Response,
    group_id: Option<&str>,
    displayed_len: usize,
) {
    if state.dragged_profile_id.is_none() || !response.hovered() {
        return;
    }
    let source_id = state.dragged_profile_id.as_deref().expect("已检查");
    state.drop_target = normalized_drop_target(inventory, source_id, group_id, displayed_len);
}

fn register_row_drop_target(
    state: &mut SshUiState,
    inventory: &SshInventory,
    response: &egui::Response,
    group_id: Option<&str>,
    visible_index: usize,
    displayed_len: usize,
) {
    if state.dragged_profile_id.is_none() || !response.hovered() {
        return;
    }
    let after = response
        .hover_pos()
        .is_some_and(|position| position.y >= response.rect.center().y);
    let boundary = (visible_index + usize::from(after)).min(displayed_len);
    let source_id = state.dragged_profile_id.as_deref().expect("已检查");
    state.drop_target = normalized_drop_target(inventory, source_id, group_id, boundary);
}

fn normalized_drop_target(
    inventory: &SshInventory,
    source_id: &str,
    target_group_id: Option<&str>,
    boundary_index: usize,
) -> Option<DropTarget> {
    let source = inventory.profile(source_id)?;
    let target_profiles = inventory.profiles_in_group(target_group_id);
    let boundary_index = boundary_index.min(target_profiles.len());
    let source_index = target_profiles
        .iter()
        .position(|profile| profile.id == source_id);
    let index = match source_index {
        Some(index) if index < boundary_index => boundary_index.saturating_sub(1),
        _ => boundary_index,
    };

    if source.group_id.as_deref() == target_group_id && source_index == Some(index) {
        return None;
    }
    Some(DropTarget {
        group_id: target_group_id.map(ToOwned::to_owned),
        index,
    })
}

fn finish_drop(
    ui: &egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    out: &mut SshUiOutput,
) {
    if state.dragged_profile_id.is_none() {
        return;
    }
    let cancelled = ui.input(|input| input.key_pressed(egui::Key::Escape));
    let released = ui.input(|input| input.pointer.any_released());
    if cancelled {
        state.cancel_drag();
    } else if released {
        let source_id = state.dragged_profile_id.take().expect("已检查");
        let target = state.drop_target.take();
        if inventory.profile(&source_id).is_some() {
            if let Some(target) = target {
                out.actions.push(SshUiAction::MoveProfile {
                    id: source_id,
                    target_group_id: target.group_id,
                    target_index: target.index,
                });
            }
        }
    }
}

fn draw_dialog(
    ctx: &egui::Context,
    state: &mut SshUiState,
    input: SshUiInput<'_>,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    let Some(mut dialog) = state.dialog.take() else {
        return;
    };
    let mut keep_open = true;
    let title = match &dialog {
        Dialog::CreateGroup { .. } => input.text.create_group_title,
        Dialog::RenameGroup { .. } => input.text.rename_group_title,
        Dialog::DeleteGroup { .. } => input.text.delete_group_title,
        Dialog::EditProfile(form) if form.editing_id.is_some() => input.text.edit_profile_title,
        Dialog::EditProfile(_) => input.text.create_profile_title,
        Dialog::DeleteProfile { .. } => input.text.delete_profile_title,
    };

    egui::Window::new(title)
        .id(egui::Id::new("ssh_ui_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(pal.bg_panel)
                .stroke(egui::Stroke::new(1.0_f32, pal.panel_outline)),
        )
        .show(ctx, |ui| match &mut dialog {
            Dialog::CreateGroup { name } => {
                keep_open = group_editor(ui, name, None, input.text, pal, out);
            }
            Dialog::RenameGroup { id, name } => {
                keep_open = group_editor(ui, name, Some(id), input.text, pal, out);
            }
            Dialog::DeleteGroup { id, name } => {
                ui.label(
                    RichText::new(format!("{}：{name}", input.text.delete_group_message))
                        .color(pal.fg),
                );
                keep_open = confirm_row(
                    ui,
                    input.text,
                    pal,
                    SshUiAction::DeleteGroup { id: id.clone() },
                    out,
                );
            }
            Dialog::EditProfile(form) => {
                keep_open = profile_editor(ui, form, input, pal, out);
            }
            Dialog::DeleteProfile { id, name } => {
                ui.label(
                    RichText::new(format!("{}：{name}", input.text.delete_profile_message))
                        .color(pal.fg),
                );
                keep_open = confirm_row(
                    ui,
                    input.text,
                    pal,
                    SshUiAction::DeleteProfile { id: id.clone() },
                    out,
                );
            }
        });

    if keep_open {
        state.dialog = Some(dialog);
    }
}

fn group_editor(
    ui: &mut egui::Ui,
    name: &mut String,
    editing_id: Option<&GroupId>,
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) -> bool {
    ui.label(RichText::new(text.group_name).color(pal.fg));
    let response = ui.add(
        egui::TextEdit::singleline(name)
            .desired_width(320.0)
            .char_limit(50)
            .text_color(pal.fg)
            .background_color(pal.extreme_bg),
    );
    response.request_focus();
    let valid = !name.trim().is_empty() && name.trim().chars().count() <= 50;
    let submit_from_keyboard =
        valid && response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));

    let mut keep_open = true;
    ui.horizontal(|ui| {
        if ui.button(text.cancel).clicked() {
            keep_open = false;
        }
        let submit = ui
            .add_enabled(valid, egui::Button::new(text.save))
            .clicked()
            || submit_from_keyboard;
        if submit {
            let name = name.trim().to_owned();
            let action = match editing_id {
                Some(id) => SshUiAction::RenameGroup {
                    id: id.clone(),
                    name,
                },
                None => SshUiAction::CreateGroup { name },
            };
            out.actions.push(action);
            keep_open = false;
        }
    });
    keep_open
}

fn confirm_row(
    ui: &mut egui::Ui,
    text: &SshUiText,
    pal: &Palette,
    action: SshUiAction,
    out: &mut SshUiOutput,
) -> bool {
    let mut keep_open = true;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button(text.cancel).clicked() {
            keep_open = false;
        }
        if ui
            .add(
                egui::Button::new(RichText::new(text.confirm_delete).color(pal.accent_fg))
                    .fill(pal.error),
            )
            .clicked()
        {
            out.actions.push(action);
            keep_open = false;
        }
    });
    keep_open
}

fn profile_editor(
    ui: &mut egui::Ui,
    form: &mut ProfileForm,
    input: SshUiInput<'_>,
    pal: &Palette,
    out: &mut SshUiOutput,
) -> bool {
    let previous_connection_fields = form.connection_fields();
    let previous_auth_method = form.auth_method;
    let mut credential_changed = false;
    egui::Grid::new("ssh_profile_form_grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            let _ = form_text_row(ui, input.text.profile_name, &mut form.name, pal);
            let _ = form_text_row(ui, input.text.host, &mut form.host, pal);
            let _ = form_number_row(ui, input.text.port, &mut form.port, "");
            let _ = form_text_row(ui, input.text.username, &mut form.username, pal);

            ui.label(input.text.auth_method);
            egui::ComboBox::from_id_salt("ssh_profile_auth_method")
                .selected_text(auth_method_text(form.auth_method, input.text))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut form.auth_method,
                        AuthMethod::Password,
                        input.text.auth_password,
                    );
                    ui.selectable_value(
                        &mut form.auth_method,
                        AuthMethod::PrivateKey,
                        input.text.auth_private_key,
                    );
                    ui.selectable_value(
                        &mut form.auth_method,
                        AuthMethod::Agent,
                        input.text.auth_agent,
                    );
                });
            if previous_auth_method != form.auth_method && form.auth_method != AuthMethod::Password
            {
                use zeroize::Zeroize as _;
                form.password.zeroize();
            }
            if previous_auth_method != form.auth_method {
                credential_changed = true;
                if form.auth_method != AuthMethod::PrivateKey {
                    use zeroize::Zeroize as _;
                    form.private_key_path = None;
                    form.key_passphrase.zeroize();
                }
            }
            ui.end_row();

            if form.auth_method == AuthMethod::Password {
                ui.label(input.text.auth_password);
                ui.vertical(|ui| {
                    credential_changed |= secure_password_edit(
                        ui,
                        "ssh_profile_password",
                        egui::TextEdit::singleline(&mut form.password)
                            .password(true)
                            .desired_width(260.0)
                            .text_color(pal.fg)
                            .background_color(pal.extreme_bg),
                    )
                    .changed();
                    let hint = if form.editing_id.is_some() {
                        input.text.password_saved_hint
                    } else {
                        input.text.password_required_hint
                    };
                    ui.label(RichText::new(hint).small().color(pal.fg_dim));
                });
                ui.end_row();
            }

            if form.auth_method == AuthMethod::PrivateKey {
                ui.label(input.text.private_key_file);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let path_text = form
                            .private_key_path
                            .as_ref()
                            .map_or_else(|| "—".to_owned(), |path| path.display().to_string());
                        let path_label = ui.add_sized(
                            [220.0, 24.0],
                            egui::Label::new(RichText::new(&path_text).small().color(pal.fg_dim))
                                .truncate()
                                .sense(egui::Sense::hover()),
                        );
                        if path_label.hovered() && form.private_key_path.is_some() {
                            path_label.on_hover_text(path_text);
                        }
                        if ui.button(input.text.choose_private_key).clicked() {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_title(input.text.choose_private_key)
                                .pick_file()
                            {
                                credential_changed |= form.private_key_path.as_ref() != Some(&path);
                                form.private_key_path = Some(path);
                            }
                        }
                    });

                    let (hint, color) = match form.private_key_path.as_deref() {
                        Some(path) if !valid_private_key_path(path) => {
                            (input.text.private_key_invalid_hint, pal.error)
                        }
                        Some(_) => (input.text.credentials_local_only, pal.fg_dim),
                        None if form.can_reuse_saved_private_key => {
                            (input.text.private_key_saved_hint, pal.fg_dim)
                        }
                        None => (input.text.private_key_required_hint, pal.warn),
                    };
                    ui.label(RichText::new(hint).small().color(color));
                });
                ui.end_row();

                if form.private_key_path.is_some() {
                    ui.label(input.text.key_passphrase);
                    ui.vertical(|ui| {
                        credential_changed |= secure_password_edit(
                            ui,
                            "ssh_profile_key_passphrase",
                            egui::TextEdit::singleline(&mut form.key_passphrase)
                                .password(true)
                                .desired_width(260.0)
                                .text_color(pal.fg)
                                .background_color(pal.extreme_bg),
                        )
                        .changed();
                        ui.label(
                            RichText::new(input.text.credentials_local_only)
                                .small()
                                .color(pal.fg_dim),
                        );
                    });
                    ui.end_row();
                }
            }

            ui.label(input.text.group);
            egui::ComboBox::from_id_salt("ssh_profile_group")
                .selected_text(group_name(
                    input.inventory,
                    form.group_id.as_deref(),
                    input.text.ungrouped,
                ))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.group_id, None, input.text.ungrouped);
                    let mut groups: Vec<_> = input.inventory.groups().iter().collect();
                    groups.sort_by_key(|group| group.sort_order);
                    for group in groups {
                        ui.selectable_value(
                            &mut form.group_id,
                            Some(group.id.clone()),
                            &group.name,
                        );
                    }
                });
            ui.end_row();

            let _ = form_text_row(
                ui,
                input.text.initial_directory,
                &mut form.initial_directory,
                pal,
            );
            let _ = form_number_row(
                ui,
                input.text.connect_timeout,
                &mut form.connect_timeout_secs,
                input.text.seconds,
            );

            ui.label(input.text.keep_alive);
            ui.horizontal(|ui| {
                ui.checkbox(&mut form.keep_alive_enabled, "");
                if form.keep_alive_enabled {
                    ui.add(egui::DragValue::new(&mut form.keep_alive_secs).range(1..=86_400));
                    ui.label(input.text.seconds);
                } else {
                    ui.label(RichText::new(input.text.keep_alive_disabled).color(pal.fg_dim));
                }
            });
            ui.end_row();

            ui.label(input.text.monitor_enabled);
            ui.checkbox(&mut form.monitor_enabled, "");
            ui.end_row();
        });

    form.reconcile_connection_field_changes(
        &previous_connection_fields,
        previous_auth_method,
        credential_changed,
    );
    draw_connection_test_status(ui, form, &input, pal, out);

    let mut keep_open = true;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button(input.text.cancel).clicked() {
            out.actions.push(SshUiAction::CancelConnectionTest {
                form_id: form.form_id,
            });
            keep_open = false;
        }
        let test_connecting = matching_connection_test(form, &input).is_some_and(|test| {
            matches!(
                &test.state,
                crate::ssh_runtime::ConnectionTestState::Connecting
            )
        });
        let test_label = if test_connecting {
            input.text.test_connecting
        } else {
            input.text.test_connection
        };
        if ui
            .add_enabled(
                form.valid(input.inventory) && !test_connecting,
                egui::Button::new(test_label),
            )
            .clicked()
        {
            out.actions.push(SshUiAction::CancelConnectionTest {
                form_id: form.form_id,
            });
            out.actions
                .push(SshUiAction::SubmitProfile(Box::new(form.test_submission())));
        }
        let button_text = if form.editing_id.is_some() {
            input.text.save
        } else {
            input.text.create
        };
        if ui
            .add_enabled(form.valid(input.inventory), egui::Button::new(button_text))
            .clicked()
        {
            out.actions.push(SshUiAction::CancelConnectionTest {
                form_id: form.form_id,
            });
            out.actions
                .push(SshUiAction::SubmitProfile(Box::new(form.save_submission())));
            keep_open = false;
        }
    });
    keep_open
}

fn matching_connection_test<'a>(
    form: &ProfileForm,
    input: &SshUiInput<'a>,
) -> Option<&'a crate::ssh_runtime::ConnectionTestView> {
    input.connection_test.filter(|test| {
        test.connection_revision == form.connection_revision
            && test.matches_target(
                form.form_id,
                form.host.trim(),
                form.port,
                form.username.trim(),
            )
    })
}

fn draw_connection_test_status(
    ui: &mut egui::Ui,
    form: &mut ProfileForm,
    input: &SshUiInput<'_>,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    let Some(test) = matching_connection_test(form, input) else {
        return;
    };

    ui.add_space(10.0);
    match &test.state {
        crate::ssh_runtime::ConnectionTestState::Connecting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(input.text.test_connecting).color(pal.warn));
            });
        }
        crate::ssh_runtime::ConnectionTestState::Success => {
            ui.label(RichText::new(input.text.test_success).color(pal.success));
        }
        crate::ssh_runtime::ConnectionTestState::Error { message } => {
            ui.label(RichText::new(input.text.test_failed).color(pal.error));
            ui.label(RichText::new(message).small().color(pal.fg_dim));
        }
        crate::ssh_runtime::ConnectionTestState::AwaitingHostKey {
            algorithm,
            fingerprint,
        } => {
            ui.label(RichText::new(input.text.test_host_key_unknown).color(pal.warn));
            connection_test_key_row(ui, input.text.host_key_algorithm, algorithm, pal);
            connection_test_key_row(ui, input.text.host_key_fingerprint, fingerprint, pal);
            if ui
                .add_enabled(
                    form.valid(input.inventory),
                    egui::Button::new(input.text.test_trust_and_retry),
                )
                .clicked()
            {
                form.trust_host_key(HostKeyTrust {
                    algorithm: algorithm.clone(),
                    fingerprint: fingerprint.clone(),
                });
                out.actions.push(SshUiAction::CancelConnectionTest {
                    form_id: form.form_id,
                });
                out.actions
                    .push(SshUiAction::SubmitProfile(Box::new(form.test_submission())));
            }
        }
        crate::ssh_runtime::ConnectionTestState::HostKeyChanged {
            expected_algorithm,
            expected_fingerprint,
            presented_algorithm,
            presented_fingerprint,
        } => {
            ui.label(RichText::new(input.text.test_host_key_changed).color(pal.error));
            ui.label(
                RichText::new(input.text.host_key_expected)
                    .strong()
                    .color(pal.fg),
            );
            connection_test_key_row(ui, input.text.host_key_algorithm, expected_algorithm, pal);
            connection_test_key_row(
                ui,
                input.text.host_key_fingerprint,
                expected_fingerprint,
                pal,
            );
            ui.label(
                RichText::new(input.text.host_key_presented)
                    .strong()
                    .color(pal.fg),
            );
            connection_test_key_row(ui, input.text.host_key_algorithm, presented_algorithm, pal);
            connection_test_key_row(
                ui,
                input.text.host_key_fingerprint,
                presented_fingerprint,
                pal,
            );
        }
    }
}

fn connection_test_key_row(ui: &mut egui::Ui, label: &str, value: &str, pal: &Palette) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{label}:")).small().color(pal.fg_dim));
        ui.label(RichText::new(value).small().monospace().color(pal.fg));
    });
}

fn form_text_row(ui: &mut egui::Ui, label: &str, value: &mut String, pal: &Palette) -> bool {
    ui.label(label);
    let changed = ui
        .add(
            egui::TextEdit::singleline(value)
                .desired_width(260.0)
                .text_color(pal.fg)
                .background_color(pal.extreme_bg),
        )
        .changed();
    ui.end_row();
    changed
}

fn form_number_row<T>(ui: &mut egui::Ui, label: &str, value: &mut T, suffix: &str) -> bool
where
    T: egui::emath::Numeric,
{
    ui.label(label);
    let changed = ui
        .horizontal(|ui| {
            let changed = ui
                .add(egui::DragValue::new(value).range(1..=86_400))
                .changed();
            if !suffix.is_empty() {
                ui.label(suffix);
            }
            changed
        })
        .inner;
    ui.end_row();
    changed
}

fn auth_method_text(method: AuthMethod, text: &SshUiText) -> &'static str {
    match method {
        AuthMethod::Password => text.auth_password,
        AuthMethod::PrivateKey => text.auth_private_key,
        AuthMethod::Agent => text.auth_agent,
    }
}

fn group_name<'a>(
    inventory: &'a SshInventory,
    group_id: Option<&str>,
    ungrouped: &'a str,
) -> &'a str {
    group_id
        .and_then(|id| inventory.group(id))
        .map_or(ungrouped, |group| group.name.as_str())
}

fn filtered_profiles<'a>(
    profiles: &[&'a SshProfile],
    group: &SshGroup,
    query: &str,
) -> Vec<&'a SshProfile> {
    if query.is_empty() || normalized_query(&group.name).contains(query) {
        profiles.to_vec()
    } else {
        profiles
            .iter()
            .copied()
            .filter(|profile| profile_matches(profile, query))
            .collect()
    }
}

fn profile_matches(profile: &SshProfile, query: &str) -> bool {
    query.is_empty()
        || normalized_query(&profile.name).contains(query)
        || normalized_query(&profile.host).contains(query)
        || normalized_query(&profile.username).contains(query)
}

fn normalized_query(value: &str) -> String {
    value.trim().to_lowercase()
}

fn optional_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn empty_row(ui: &mut egui::Ui, message: &str, pal: &Palette) {
    ui.add_sized(
        [ui.available_width(), ROW_HEIGHT],
        egui::Label::new(RichText::new(message).small().color(pal.fg_dim)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> SshInventory {
        let mut inventory = SshInventory::default();
        let group = inventory.create_group("生产").unwrap();
        for name in ["A", "B", "C"] {
            inventory
                .create_profile(NewSshProfile {
                    name: name.into(),
                    host: format!("{name}.example.com"),
                    username: "root".into(),
                    group_id: Some(group.clone()),
                    ..NewSshProfile::default()
                })
                .unwrap();
        }
        inventory
    }

    #[test]
    fn 同组向下拖动会换算为移除源行后的索引() {
        let inventory = inventory();
        let group_id = inventory.groups()[0].id.as_str();
        let profiles = inventory.profiles_in_group(Some(group_id));
        let source_id = &profiles[0].id;

        let target = normalized_drop_target(&inventory, source_id, Some(group_id), 2).unwrap();
        assert_eq!(target.index, 1);
    }

    #[test]
    fn 原位置落点不产生动作() {
        let inventory = inventory();
        let group_id = inventory.groups()[0].id.as_str();
        let profiles = inventory.profiles_in_group(Some(group_id));
        let source_id = &profiles[1].id;

        assert_eq!(
            normalized_drop_target(&inventory, source_id, Some(group_id), 1),
            None
        );
        assert_eq!(
            normalized_drop_target(&inventory, source_id, Some(group_id), 2),
            None
        );
    }

    #[test]
    fn 服务器行操作使用本地化文案并映射到明确动作() {
        let text = SshUiText {
            edit: "Edit",
            delete: "Delete",
            ..SshUiText::default()
        };
        assert_eq!(profile_row_action_labels(&text), ["Edit", "Delete"]);

        let inventory = inventory();
        let profile = inventory.profiles().first().unwrap();
        let mut state = SshUiState::default();
        let mut out = SshUiOutput::default();

        apply_profile_row_command(ProfileRowCommand::Connect, profile, &mut state, &mut out);
        assert!(matches!(
            out.actions.as_slice(),
            [SshUiAction::ConnectProfile { id }] if id == &profile.id
        ));

        apply_profile_row_command(ProfileRowCommand::Edit, profile, &mut state, &mut out);
        assert!(matches!(
            state.dialog.as_ref(),
            Some(Dialog::EditProfile(form))
                if form.editing_id.as_deref() == Some(profile.id.as_str())
        ));

        apply_profile_row_command(ProfileRowCommand::Delete, profile, &mut state, &mut out);
        assert!(matches!(
            state.dialog.as_ref(),
            Some(Dialog::DeleteProfile { id, name })
                if id == &profile.id && name == &profile.name
        ));
    }

    #[test]
    fn 分组菜单复用既有重命名与删除弹窗() {
        let inventory = inventory();
        let group = inventory.groups().first().unwrap();
        let mut state = SshUiState::default();

        apply_group_command(GroupCommand::Rename, group, &mut state);
        assert!(matches!(
            state.dialog.as_ref(),
            Some(Dialog::RenameGroup { id, name })
                if id == &group.id && name == &group.name
        ));

        apply_group_command(GroupCommand::Delete, group, &mut state);
        assert!(matches!(
            state.dialog.as_ref(),
            Some(Dialog::DeleteGroup { id, name })
                if id == &group.id && name == &group.name
        ));
    }

    #[test]
    fn 搜索与上锁都会清理拖放和弹窗() {
        let mut state = SshUiState {
            dialog: Some(Dialog::CreateGroup {
                name: "临时".into(),
            }),
            dragged_profile_id: Some("ssh_example".into()),
            drop_target: Some(DropTarget {
                group_id: None,
                index: 0,
            }),
            ..SshUiState::default()
        };

        state.set_search("prod");
        assert!(state.dragged_profile_id.is_none());
        assert!(state.drop_target.is_none());
        assert!(state.dialog.is_some());

        state.close_for_app_lock();
        assert!(state.dialog.is_none());
        assert!(state.dragged_profile_id.is_none());
        assert!(state.drop_target.is_none());
    }

    #[test]
    fn 表单只生成可同步的连接元数据() {
        let form = ProfileForm::create();
        let draft = form.to_draft();
        assert_eq!(draft.port, 22);
        assert_eq!(draft.auth_method, AuthMethod::Password);
        assert_eq!(draft.keep_alive_secs, Some(30));
        assert!(draft.trusted_host_key.is_none());
    }

    #[test]
    fn 新建密码认证必须填写密码且秘密不进入同步草稿() {
        let inventory = SshInventory::default();
        let mut form = ProfileForm::create();
        form.name = "server".to_owned();
        form.host = "server.example.test".to_owned();
        form.username = "alice".to_owned();

        assert!(!form.valid(&inventory));
        form.password = "local-only-secret".to_owned();
        assert!(form.valid(&inventory));

        let draft = form.to_draft();
        assert_eq!(draft.host, "server.example.test");
        assert_eq!(draft.username, "alice");
        assert!(!format!("{draft:?}").contains("local-only-secret"));
        assert!(!format!("{form:?}").contains("local-only-secret"));
    }

    #[test]
    fn 新建私钥认证必须先选择存在的本机文件且秘密不进入同步草稿() {
        let inventory = SshInventory::default();
        let mut form = ProfileForm::create();
        form.name = "key-server".to_owned();
        form.host = "server.example.test".to_owned();
        form.username = "alice".to_owned();
        form.auth_method = AuthMethod::PrivateKey;

        assert!(!form.valid(&inventory));
        form.private_key_path = Some(PathBuf::from("relative-id_ed25519"));
        assert!(!form.valid(&inventory), "相对路径不得通过表单校验");
        form.private_key_path = Some(std::env::temp_dir());
        assert!(!form.valid(&inventory), "目录不得被当作私钥文件");

        let key_path = std::env::temp_dir().join(format!(
            "lumen-ssh-form-key-{}-{}",
            std::process::id(),
            next_profile_form_id()
        ));
        std::fs::write(&key_path, b"test-key").expect("创建临时私钥");
        form.private_key_path = Some(key_path.clone());
        form.key_passphrase = "local-key-passphrase".to_owned();
        assert!(form.valid(&inventory));

        let draft = form.to_draft();
        assert_eq!(draft.auth_method, AuthMethod::PrivateKey);
        assert!(!format!("{draft:?}").contains(&key_path.display().to_string()));
        assert!(!format!("{draft:?}").contains("local-key-passphrase"));
        assert!(!format!("{form:?}").contains(&key_path.display().to_string()));
        assert!(!format!("{form:?}").contains("local-key-passphrase"));

        let mut submission = form.test_submission();
        assert_eq!(
            submission.take_private_key_path().as_deref(),
            Some(key_path.as_path())
        );
        assert_eq!(submission.take_key_passphrase(), "local-key-passphrase");
        assert_eq!(form.private_key_path.as_deref(), Some(key_path.as_path()));
        assert_eq!(form.key_passphrase, "local-key-passphrase");
        let _ = std::fs::remove_file(key_path);
    }

    #[test]
    fn 编辑私钥仅在目标和认证方式未变化时允许沿用本机绑定() {
        let inventory = inventory();
        let mut profile = inventory.profiles().first().unwrap().clone();
        profile.auth_method = AuthMethod::PrivateKey;
        let mut form = ProfileForm::edit(&profile);
        assert!(form.can_reuse_saved_private_key);
        assert!(form.valid(&inventory));
        form.private_key_path = Some(std::env::temp_dir());
        assert!(
            !form.valid(&inventory),
            "显式选择了无效路径时不得静默回退旧绑定"
        );
        form.private_key_path = None;

        let previous_fields = form.connection_fields();
        let previous_auth = form.auth_method;
        form.host = "other.example.test".to_owned();
        form.reconcile_connection_field_changes(&previous_fields, previous_auth, false);
        assert!(!form.can_reuse_saved_private_key);
        assert!(!form.valid(&inventory));

        let mut password_profile = profile;
        password_profile.auth_method = AuthMethod::Password;
        let mut switched = ProfileForm::edit(&password_profile);
        let previous_fields = switched.connection_fields();
        let previous_auth = switched.auth_method;
        switched.auth_method = AuthMethod::PrivateKey;
        switched.reconcile_connection_field_changes(&previous_fields, previous_auth, false);
        assert!(!switched.can_reuse_saved_private_key);
        assert!(!switched.valid(&inventory));
    }

    #[test]
    fn 编辑密码留空有效且测试提交保持表单秘密() {
        let inventory = inventory();
        let profile = inventory.profiles().first().unwrap();
        let mut form = ProfileForm::edit(profile);
        assert!(form.password.is_empty());
        assert!(form.valid(&inventory));

        form.password = "one-shot-secret".to_owned();
        let previous_revision = form.connection_revision;
        let mut submission = form.test_submission();
        assert_eq!(submission.intent(), ProfileSubmitIntent::TestConnection);
        assert_eq!(submission.form_id(), form.form_id);
        assert_eq!(
            submission.connection_revision(),
            previous_revision.saturating_add(1)
        );
        assert_eq!(submission.connection_revision(), form.connection_revision);
        assert_eq!(submission.editing_id(), Some(profile.id.as_str()));
        assert_eq!(submission.password(), "one-shot-secret");
        assert_eq!(form.password, "one-shot-secret");
        assert!(!format!("{submission:?}").contains("one-shot-secret"));
        assert_eq!(submission.take_password(), "one-shot-secret");
        assert!(submission.password().is_empty());
    }

    #[test]
    fn 每次打开服务器表单使用不同测试标识() {
        let first = ProfileForm::create();
        let second = ProfileForm::create();
        assert_ne!(first.form_id, second.form_id);
        assert!(!first.host_key_verified_for_current_endpoint);
        assert!(!second.host_key_verified_for_current_endpoint);
    }

    #[test]
    fn 显式信任会写入当前endpoint证明并随提交携带() {
        let mut form = ProfileForm::create();
        let previous_revision = form.connection_revision;
        form.trust_host_key(HostKeyTrust {
            algorithm: "ssh-ed25519".to_owned(),
            fingerprint: "SHA256:presented".to_owned(),
        });
        assert!(form.host_key_verified_for_current_endpoint);
        assert!(form.connection_revision > previous_revision);

        let submission = form.test_submission();
        assert!(submission.host_key_verified_for_current_endpoint());
        assert_eq!(submission.connection_revision(), form.connection_revision);
    }

    #[test]
    fn 连接测试结果必须同时匹配表单标识与结构化目标() {
        let mut form = ProfileForm::create();
        form.host = " server.example.test ".to_owned();
        form.port = 2222;
        form.username = " alice ".to_owned();
        let mut view = crate::ssh_runtime::ConnectionTestView {
            form_id: form.form_id,
            connection_revision: form.connection_revision,
            host: "server.example.test".to_owned(),
            port: 2222,
            username: "alice".to_owned(),
            state: crate::ssh_runtime::ConnectionTestState::Success,
        };
        let text = SshUiText::default();
        let inventory = SshInventory::default();
        let input = SshUiInput {
            inventory: &inventory,
            text: &text,
            connection_test: Some(&view),
        };
        assert!(matching_connection_test(&form, &input).is_some());

        view.username = "mallory".to_owned();
        let stale_input = SshUiInput {
            inventory: &inventory,
            text: &text,
            connection_test: Some(&view),
        };
        assert!(matching_connection_test(&form, &stale_input).is_none());
    }

    #[test]
    fn endpoint变化立即清除已信任主机密钥与证明并推进revision() {
        fn assert_endpoint_change_clears(change: impl FnOnce(&mut ProfileForm)) {
            let mut form = ProfileForm::create();
            form.trusted_host_key = Some(HostKeyTrust {
                algorithm: "ssh-ed25519".to_owned(),
                fingerprint: "SHA256:trusted".to_owned(),
            });
            form.host_key_verified_for_current_endpoint = true;
            let previous_fields = form.connection_fields();
            let previous_auth = form.auth_method;
            let previous_revision = form.connection_revision;

            change(&mut form);
            form.reconcile_connection_field_changes(&previous_fields, previous_auth, false);

            assert!(form.trusted_host_key.is_none());
            assert!(!form.host_key_verified_for_current_endpoint);
            assert!(form.connection_revision > previous_revision);
        }

        assert_endpoint_change_clears(|form| form.host = "new.example.test".to_owned());
        assert_endpoint_change_clears(|form| form.port = 2222);
        assert_endpoint_change_clears(|form| form.username = "other".to_owned());
    }

    #[test]
    fn 密码或认证方式变化会让旧连接测试结果失效() {
        let mut form = ProfileForm::create();
        form.host = "server.example.test".to_owned();
        form.port = 22;
        form.username = "alice".to_owned();
        let text = SshUiText::default();
        let inventory = SshInventory::default();
        let mut view = crate::ssh_runtime::ConnectionTestView {
            form_id: form.form_id,
            connection_revision: form.connection_revision,
            host: form.host.clone(),
            port: form.port,
            username: form.username.clone(),
            state: crate::ssh_runtime::ConnectionTestState::Success,
        };
        let current_input = SshUiInput {
            inventory: &inventory,
            text: &text,
            connection_test: Some(&view),
        };
        assert!(matching_connection_test(&form, &current_input).is_some());

        let previous_fields = form.connection_fields();
        let previous_auth = form.auth_method;
        form.auth_method = AuthMethod::Agent;
        form.reconcile_connection_field_changes(&previous_fields, previous_auth, false);
        let stale_auth_input = SshUiInput {
            inventory: &inventory,
            text: &text,
            connection_test: Some(&view),
        };
        assert!(matching_connection_test(&form, &stale_auth_input).is_none());

        view.connection_revision = form.connection_revision;
        let previous_fields = form.connection_fields();
        let previous_auth = form.auth_method;
        form.auth_method = AuthMethod::Password;
        form.password = "new-secret".to_owned();
        form.reconcile_connection_field_changes(&previous_fields, previous_auth, true);
        let stale_password_input = SshUiInput {
            inventory: &inventory,
            text: &text,
            connection_test: Some(&view),
        };
        assert!(matching_connection_test(&form, &stale_password_input).is_none());
    }
}
