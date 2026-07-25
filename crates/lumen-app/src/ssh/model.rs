use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

pub type GroupId = String;
pub type ProfileId = String;

/// 认证方式仅表达用户偏好，不承载密码、密钥或凭据引用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    Password,
    PrivateKey,
    Agent,
}

/// 用户确认过的主机密钥。只同步公开的算法与指纹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKeyTrust {
    pub algorithm: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshGroup {
    pub id: GroupId,
    pub name: String,
    pub sort_order: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// SSH 连接元数据。秘密数据不属于该类型，见
/// [`crate::ssh::SshLocalBinding`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshProfile {
    pub id: ProfileId,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub group_id: Option<GroupId>,
    pub sort_order: u32,
    pub initial_directory: Option<String>,
    pub connect_timeout_secs: u32,
    pub keep_alive_secs: Option<u32>,
    pub monitor_enabled: bool,
    pub trusted_host_key: Option<HostKeyTrust>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// 新建或更新服务器时由 UI 提交的可同步字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSshProfile {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub group_id: Option<GroupId>,
    pub initial_directory: Option<String>,
    pub connect_timeout_secs: u32,
    pub keep_alive_secs: Option<u32>,
    pub monitor_enabled: bool,
    pub trusted_host_key: Option<HostKeyTrust>,
}

impl Default for NewSshProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            auth_method: AuthMethod::Password,
            group_id: None,
            initial_directory: None,
            connect_timeout_secs: 15,
            keep_alive_secs: Some(30),
            monitor_enabled: true,
            trusted_host_key: None,
        }
    }
}

/// 云同步使用的显式 allowlist DTO。不要给它添加任何本地绑定字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncGroupDto {
    pub id: String,
    pub name: String,
    pub sort_order: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl From<&SshGroup> for SyncGroupDto {
    fn from(value: &SshGroup) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            sort_order: value.sort_order,
            created_at_ms: value.created_at_ms,
            updated_at_ms: value.updated_at_ms,
        }
    }
}

/// 云同步使用的服务器 allowlist DTO。认证方式可以同步，认证材料不可以。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProfileDto {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub group_id: Option<String>,
    pub sort_order: u32,
    pub initial_directory: Option<String>,
    pub connect_timeout_secs: u32,
    pub keep_alive_secs: Option<u32>,
    pub monitor_enabled: bool,
    pub trusted_host_key: Option<HostKeyTrust>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl From<&SshProfile> for SyncProfileDto {
    fn from(value: &SshProfile) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            host: value.host.clone(),
            port: value.port,
            username: value.username.clone(),
            auth_method: value.auth_method,
            group_id: value.group_id.clone(),
            sort_order: value.sort_order,
            initial_directory: value.initial_directory.clone(),
            connect_timeout_secs: value.connect_timeout_secs,
            keep_alive_secs: value.keep_alive_secs,
            monitor_enabled: value.monitor_enabled,
            trusted_host_key: value.trusted_host_key.clone(),
            created_at_ms: value.created_at_ms,
            updated_at_ms: value.updated_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStateDto {
    pub groups: Vec<SyncGroupDto>,
    pub profiles: Vec<SyncProfileDto>,
}

pub(crate) fn new_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut id = String::with_capacity(prefix.len() + bytes.len() * 2);
    id.push_str(prefix);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        id.push(HEX[usize::from(byte >> 4)] as char);
        id.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    id
}
