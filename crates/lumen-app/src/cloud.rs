//! 客户端与 `lumen-server` 的 REST 通道（M5.1）。
//!
//! 用 `ureq` 同步阻塞请求，调用方在后台线程里使用（见 `shell/login_ui.rs`），
//! 不阻塞 UI 帧——与 F3 热更的后台线程模式一致，客户端不引入 tokio。
//!
//! 设备 id 持久化在应用数据目录、**登出后保留**，使同一物理机跨登录复用
//! 同一设备记录（避免在服务端重复登记设备）。

use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;

use lumen_protocol::ssh_sync::{SshSyncRequest, SshSyncResponse};
use lumen_protocol::{
    routes, ApiError, AuthResponse, DeviceListResponse, HistoryEntry, HistoryPullResponse,
    HistoryPushRequest, LoginRequest, RefreshResponse, RegisterRequest, RenameDeviceRequest,
    SettingsSync, UserInfo,
};
use sha2::{Digest, Sha256};
use url::{Host, Url};

const SSH_SYNC_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const SSH_SYNC_MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const SSH_SYNC_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const SSH_SYNC_MAX_ERROR_BYTES: usize = 64 * 1024;
/// 带账号凭据的 REST 请求绝不自动跟随重定向，避免密码、Bearer token 或 SSH DTO
/// 被 3xx 响应转发到另一个端点。
const REST_MAX_REDIRECTS: u32 = 0;

/// 进程内的服务端基址（懒初始化：环境变量 > 持久化(设置页) > **空**）。
/// **发布版不预设任何默认服务端地址**（含 localhost）——未配置时为空串，用户须在
/// 设置页填服务端地址；开发时用环境变量 `LUMEN_SERVER_URL` 指向本地服务端。
static SERVER_URL: RwLock<Option<String>> = RwLock::new(None);

/// 取服务端基址（规范 origin、缺协议补 `https://`；**未配置或无效返回空串**）。
///
/// 首次读取时按「`LUMEN_SERVER_URL` 环境变量 > 持久化(设置页) > 空」初始化；
/// 之后由 [`set_server_url`]（设置页输入）覆盖。供 `login_ui` / `remote` 共用。
/// 返回空 = 未配置服务端，调用方应提示用户先在设置里填地址。
pub fn server_url() -> String {
    if let Some(u) = SERVER_URL.read().ok().and_then(|g| g.clone()) {
        return u;
    }
    let raw = std::env::var("LUMEN_SERVER_URL").ok().unwrap_or_default();
    let normalized = canonical_server_origin(&raw).unwrap_or_default();
    if let Ok(mut g) = SERVER_URL.write() {
        *g = Some(normalized.clone());
    }
    normalized
}

/// 设置服务端基址（设置页输入用）：更新进程内全局。持久化由 settings.json 负责。
pub fn set_server_url(url: &str) {
    let normalized = canonical_server_origin(url).unwrap_or_default();
    if let Ok(mut g) = SERVER_URL.write() {
        *g = Some(normalized);
    }
}

/// 把用户输入严格规范为一个 HTTP(S) 服务端 origin。
///
/// 裸主机默认使用 HTTPS；只接受根 origin，不接受凭据、路径、查询、片段或控制字符。
/// 返回值统一为小写 scheme / URL 规范化 host、移除默认端口且不带尾 `/`。
pub fn canonical_server_origin(raw: &str) -> Result<String, CloudError> {
    if raw.chars().any(char::is_control) {
        return Err(CloudError::InvalidServerOrigin);
    }
    let input = raw.trim();
    if input.is_empty() {
        return Err(CloudError::InvalidServerOrigin);
    }
    let candidate = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let (_, remainder) = candidate
        .split_once("://")
        .ok_or(CloudError::InvalidServerOrigin)?;
    let authority_end = remainder
        .find(['/', '\\', '?', '#'])
        .unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let suffix = &remainder[authority_end..];
    if authority.contains('@') || authority.ends_with(':') || !matches!(suffix, "" | "/") {
        return Err(CloudError::InvalidServerOrigin);
    }
    let mut url = Url::parse(&candidate).map_err(|_| CloudError::InvalidServerOrigin)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
        || !matches!(url.path(), "" | "/")
    {
        return Err(CloudError::InvalidServerOrigin);
    }

    let default_port = match url.scheme() {
        "http" => 80,
        "https" => 443,
        _ => unreachable!("scheme 已在上方限制为 http/https"),
    };
    if url.port() == Some(default_port) {
        url.set_port(None)
            .map_err(|()| CloudError::InvalidServerOrigin)?;
    }
    url.set_path("");

    let host = match url.host().ok_or(CloudError::InvalidServerOrigin)? {
        Host::Domain(domain) if domain.ends_with('.') => {
            return Err(CloudError::InvalidServerOrigin);
        }
        Host::Domain(domain) => domain.to_owned(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => format!("[{address}]"),
    };
    let mut origin = format!("{}://{host}", url.scheme());
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Ok(origin)
}

/// 网络/协议错误。
#[derive(Clone, PartialEq, Eq)]
pub enum CloudError {
    /// 服务端地址不是严格的 HTTP(S) 根 origin。
    InvalidServerOrigin,
    /// 远端明文 HTTP 被策略阻止。
    InsecureTransport,
    /// 网络层错误（连不上、超时等）。
    Network(String),
    /// 服务端返回的业务错误（含机器码）。
    Api {
        /// HTTP 状态码。
        status: u16,
        /// 机器可读 code（如 `user_not_found`）。
        code: String,
        /// 人类可读说明。
        message: String,
    },
    /// 响应体解析失败。
    Decode(String),
}

impl fmt::Debug for CloudError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidServerOrigin => formatter.write_str("InvalidServerOrigin"),
            Self::InsecureTransport => formatter.write_str("InsecureTransport"),
            Self::Network(_) => formatter
                .debug_tuple("Network")
                .field(&"[redacted]")
                .finish(),
            Self::Api { status, .. } => formatter
                .debug_struct("Api")
                .field("status", status)
                .field("code", &self.code())
                .field("message", &"[redacted]")
                .finish(),
            Self::Decode(_) => formatter
                .debug_tuple("Decode")
                .field(&"[redacted]")
                .finish(),
        }
    }
}

impl CloudError {
    /// 机器可读错误码（非 Api 变体给占位）。
    pub fn code(&self) -> &str {
        match self {
            CloudError::InvalidServerOrigin => "invalid_server_origin",
            CloudError::InsecureTransport => "insecure_transport",
            CloudError::Api { code, .. } if valid_api_code(code) => code,
            CloudError::Api { .. } => "http_error",
            CloudError::Network(_) => "network",
            CloudError::Decode(_) => "decode",
        }
    }

    /// 面向用户的中文提示。
    pub fn user_message(&self) -> String {
        match self {
            CloudError::InvalidServerOrigin => {
                "服务端地址无效，请填写 HTTPS 根地址（例如 https://lumen.example.com）".to_string()
            }
            CloudError::InsecureTransport => "为保护账号数据，远程服务端必须使用 HTTPS".to_string(),
            CloudError::Network(_) => "无法连接服务器，请检查网络或服务端地址".to_string(),
            CloudError::Decode(_) => "服务器响应异常".to_string(),
            CloudError::Api { .. } => match self.code() {
                "invalid_credentials" => "密码错误".to_string(),
                "email_taken" => "该邮箱已注册".to_string(),
                "bad_request" => "邮箱或密码格式不正确".to_string(),
                _ => "服务器拒绝了请求，请稍后重试".to_string(),
            },
        }
    }
}

fn valid_api_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn sanitize_api_message(message: &str, fallback: &str) -> String {
    let mut clean = String::with_capacity(message.len().min(512));
    for character in message.chars().filter(|character| !character.is_control()) {
        if clean.len().saturating_add(character.len_utf8()) > 512 {
            break;
        }
        clean.push(character);
    }
    let clean = clean.trim();
    if clean.is_empty() {
        fallback.to_owned()
    } else {
        clean.to_owned()
    }
}

fn api_error(status: u16, api: Option<ApiError>, fallback: &str) -> CloudError {
    let (code, message) = api
        .map(|error| (error.code, error.message))
        .unwrap_or_else(|| ("http_error".to_owned(), fallback.to_owned()));
    let code = if valid_api_code(&code) {
        code
    } else {
        "http_error".to_owned()
    };
    CloudError::Api {
        status,
        code,
        message: sanitize_api_message(&message, fallback),
    }
}

/// 验证已配置服务端的传输策略。
///
/// HTTPS 始终允许；HTTP 仅允许真正的 loopback 地址，或由运维显式设置
/// `LUMEN_ALLOW_INSECURE_HTTP=1`。错误不携带原始 URL。
pub fn validate_server_transport(raw: &str) -> Result<(), CloudError> {
    let origin = canonical_server_origin(raw)?;
    let url = Url::parse(&origin).map_err(|_| CloudError::InvalidServerOrigin)?;
    let explicitly_allowed = std::env::var("LUMEN_ALLOW_INSECURE_HTTP")
        .ok()
        .is_some_and(|value| value == "1");
    if transport_allowed(&url, explicitly_allowed)? {
        Ok(())
    } else {
        Err(CloudError::InsecureTransport)
    }
}

/// 捕获并验证一个不可变的服务端 origin，供后台任务在启动前绑定。
///
/// 调用方应把返回的 owned `String` move 进 worker，之后不得再读取全局 [`server_url`]。
pub fn verified_server_origin(raw: &str) -> Result<String, CloudError> {
    let origin = canonical_server_origin(raw)?;
    validate_server_transport(&origin)?;
    Ok(origin)
}

fn transport_allowed(url: &Url, explicitly_allowed: bool) -> Result<bool, CloudError> {
    if url.scheme() == "https" {
        return Ok(true);
    }
    let loopback = match url.host().ok_or(CloudError::InvalidServerOrigin)? {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    };
    Ok(loopback || explicitly_allowed)
}

/// 与 `lumen-server` 通信的客户端。
pub struct CloudClient {
    base: Result<String, CloudError>,
    agent: ureq::Agent,
}

impl CloudClient {
    /// 以服务端基址新建客户端（连接 10s / 读 20s 超时）。
    pub fn new(base: impl Into<String>) -> Self {
        let raw = base.into();
        let base = canonical_server_origin(&raw);
        let agent = ureq::AgentBuilder::new()
            .redirects(REST_MAX_REDIRECTS)
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(20))
            .build();
        Self { base, agent }
    }

    fn checked_base(&self) -> Result<&str, CloudError> {
        let base = match &self.base {
            Ok(base) => base.as_str(),
            Err(error) => return Err(error.clone()),
        };
        validate_server_transport(base)?;
        Ok(base)
    }

    /// 发一次请求，返回响应体文本；非 2xx 映射为 [`CloudError::Api`]。
    fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> Result<String, CloudError> {
        // 必须先验证传输，之后才构造请求并附加 token / 密码请求体。
        let base = self.checked_base()?;
        let url = format!("{base}{path}");
        let mut req = self.agent.request(method, &url);
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let result = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(b),
            None => req.call(),
        };
        match result {
            Ok(resp) => resp
                .into_string()
                .map_err(|_| CloudError::Network("response read failed".to_owned())),
            Err(ureq::Error::Status(status, resp)) => {
                let api = read_limited_response(resp, SSH_SYNC_MAX_ERROR_BYTES)
                    .ok()
                    .and_then(|text| serde_json::from_str::<ApiError>(&text).ok());
                Err(api_error(status, api, "请求被服务端拒绝"))
            }
            Err(ureq::Error::Transport(_)) => Err(CloudError::Network("request failed".to_owned())),
        }
    }

    /// 反序列化 JSON 响应。
    fn decode<T: serde::de::DeserializeOwned>(txt: &str) -> Result<T, CloudError> {
        serde_json::from_str(txt)
            .map_err(|_| CloudError::Decode("response decoding failed".to_owned()))
    }

    /// 序列化请求体。
    fn encode(v: &impl serde::Serialize) -> Result<String, CloudError> {
        serde_json::to_string(v)
            .map_err(|_| CloudError::Decode("request encoding failed".to_owned()))
    }

    /// 注册账户。
    pub fn register(&self, email: &str, password: &str) -> Result<UserInfo, CloudError> {
        let body = Self::encode(&RegisterRequest {
            email: email.to_string(),
            password: password.to_string(),
        })?;
        let txt = self.send("POST", routes::REGISTER, None, Some(&body))?;
        Self::decode(&txt)
    }

    /// 登录（携带本设备信息）。
    pub fn login(&self, req: &LoginRequest) -> Result<AuthResponse, CloudError> {
        let body = Self::encode(req)?;
        let txt = self.send("POST", routes::LOGIN, None, Some(&body))?;
        Self::decode(&txt)
    }

    /// 设备列表。
    pub fn list_devices(&self, token: &str) -> Result<DeviceListResponse, CloudError> {
        let txt = self.send("GET", routes::DEVICES, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 设备心跳（保持本设备在线，刷新服务端 `last_seen`）。
    pub fn heartbeat(&self, token: &str) -> Result<(), CloudError> {
        self.send("POST", routes::HEARTBEAT, Some(token), None)?;
        Ok(())
    }

    /// 重命名设备。
    pub fn rename_device(&self, token: &str, id: &str, name: &str) -> Result<(), CloudError> {
        let body = Self::encode(&RenameDeviceRequest {
            name: name.to_string(),
        })?;
        self.send("PATCH", &routes::device(id), Some(token), Some(&body))?;
        Ok(())
    }

    /// 删除设备。
    pub fn delete_device(&self, token: &str, id: &str) -> Result<(), CloudError> {
        self.send("DELETE", &routes::device(id), Some(token), None)?;
        Ok(())
    }

    /// 拉取偏好设置。
    pub fn get_settings(&self, token: &str) -> Result<SettingsSync, CloudError> {
        let txt = self.send("GET", routes::SYNC_SETTINGS, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 推送偏好设置（返回服务端权威值）。
    pub fn put_settings(&self, token: &str, s: &SettingsSync) -> Result<SettingsSync, CloudError> {
        let body = Self::encode(s)?;
        let txt = self.send("PUT", routes::SYNC_SETTINGS, Some(token), Some(&body))?;
        Self::decode(&txt)
    }

    /// 用现有**有效** token 续期，返回新 token + 到期时间。客户端在 token 快到期时调用，免 7 天
    /// 到期后全面 401 掉线。旧 token 已过期则服务端 401（续期失败，需重新登录）。
    pub fn refresh_token(&self, token: &str) -> Result<RefreshResponse, CloudError> {
        let txt = self.send("POST", routes::REFRESH, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 拉取增量历史（`since` 为毫秒水位线）。
    pub fn pull_history(&self, token: &str, since: i64) -> Result<HistoryPullResponse, CloudError> {
        let path = format!("{}?since={since}", routes::SYNC_HISTORY);
        let txt = self.send("GET", &path, Some(token), None)?;
        Self::decode(&txt)
    }

    /// 推送历史（返回服务端新插入条数）。
    pub fn push_history(
        &self,
        token: &str,
        entries: Vec<HistoryEntry>,
    ) -> Result<u64, CloudError> {
        let body = Self::encode(&HistoryPushRequest { entries })?;
        let txt = self.send("POST", routes::SYNC_HISTORY, Some(token), Some(&body))?;
        let v: serde_json::Value = Self::decode(&txt)?;
        Ok(v.get("inserted").and_then(serde_json::Value::as_u64).unwrap_or(0))
    }

    /// 账号级 SSH 配置增量同步。
    ///
    /// 与通用 REST 方法分开限制请求/响应大小和总超时。错误仅保留结构化
    /// `ApiError`，绝不把未知响应正文塞进错误或日志。
    pub fn sync_ssh(
        &self,
        token: &str,
        request: &SshSyncRequest,
    ) -> Result<SshSyncResponse, CloudError> {
        // 与通用 send 同样：在序列化正文、附加 Bearer token 前先做传输策略检查。
        let base = self.checked_base()?;
        let body = Self::encode(request)?;
        if body.len() > SSH_SYNC_MAX_REQUEST_BYTES {
            return Err(CloudError::Decode(
                "SSH 同步请求超过本地安全大小限制".to_owned(),
            ));
        }
        let url = format!("{base}{}", routes::SYNC_SSH);
        let result = self
            .agent
            .post(&url)
            .timeout(SSH_SYNC_REQUEST_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&body);
        let response_text = match result {
            Ok(response) => read_limited_response(response, SSH_SYNC_MAX_RESPONSE_BYTES)?,
            Err(ureq::Error::Status(status, response)) => {
                let api = read_limited_response(response, SSH_SYNC_MAX_ERROR_BYTES)
                    .ok()
                    .and_then(|text| serde_json::from_str::<ApiError>(&text).ok());
                return Err(api_error(status, api, "SSH 同步请求被服务端拒绝"));
            }
            Err(ureq::Error::Transport(_)) => {
                return Err(CloudError::Network("SSH sync request failed".to_owned()));
            }
        };
        Self::decode(&response_text)
    }
}

fn read_limited_response(
    response: ureq::Response,
    maximum_bytes: usize,
) -> Result<String, CloudError> {
    let maximum_plus_marker = maximum_bytes.saturating_add(1);
    let mut bytes = Vec::with_capacity(maximum_plus_marker.min(64 * 1024));
    response
        .into_reader()
        .take(maximum_plus_marker as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CloudError::Network("response read failed".to_owned()))?;
    if bytes.len() > maximum_bytes {
        return Err(CloudError::Decode(
            "服务器响应超过本地安全大小限制".to_owned(),
        ));
    }
    String::from_utf8(bytes).map_err(|_| CloudError::Decode("服务器响应不是 UTF-8".to_owned()))
}

// ——— 设备 id 持久化（登出后保留，跨登录复用）———

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let bytes = digest.as_ref();
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn origin_storage_key(origin: &str) -> Result<String, CloudError> {
    let canonical = canonical_server_origin(origin)?;
    Ok(hex_digest(Sha256::digest(canonical.as_bytes())))
}

fn device_id_path_for_origin_at(
    data_root: &std::path::Path,
    origin: &str,
) -> Result<PathBuf, CloudError> {
    let key = origin_storage_key(origin)?;
    Ok(data_root.join("origins").join(key).join("device_id"))
}

fn device_id_path_for_origin(origin: &str) -> Option<PathBuf> {
    let data_root = crate::paths::data_dir()?;
    device_id_path_for_origin_at(&data_root, origin).ok()
}

/// 读取绑定到指定 canonical origin 的设备 id。
///
/// 不回退旧版全局 `device_id`，避免把服务 A 的稳定设备标识发给服务 B。
pub fn load_device_id_for_origin(origin: &str) -> Option<String> {
    load_device_id_from(&device_id_path_for_origin(origin)?)
}

/// 保存绑定到指定 canonical origin 的设备 id。
pub fn save_device_id_for_origin(origin: &str, id: &str) -> std::io::Result<()> {
    let Some(data_root) = crate::paths::data_dir() else {
        return Ok(());
    };
    let path = device_id_path_for_origin_at(&data_root, origin).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid server origin for device id",
        )
    })?;
    save_device_id_to(&path, id)
}

/// 仅当 profile 明确属于同一 canonical origin 时，用其中的镜像 id 修复该 origin
/// 分区的设备 id。旧 profile 没有 `auth_origin`，不会被自动迁移或上传。
pub fn reconcile_device_id_for_origin(
    origin: &str,
    profile_origin: Option<&str>,
    profile_device_id: Option<&str>,
) -> bool {
    let Ok(canonical) = canonical_server_origin(origin) else {
        return false;
    };
    let profile_matches = profile_origin
        .and_then(|saved| canonical_server_origin(saved).ok())
        .is_some_and(|saved| saved == canonical);
    if !profile_matches || load_device_id_for_origin(&canonical).is_some() {
        return false;
    }
    let Some(device_id) = profile_device_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    match save_device_id_for_origin(&canonical, device_id) {
        Ok(()) => {
            log::info!("已从同源 profile 修复服务端分区 device_id");
            true
        }
        Err(error) => {
            log::warn!("修复服务端分区 device_id 失败: {error}");
            false
        }
    }
}

/// 旧版全局设备 id 文件路径。仅用于本地兼容对账，联网鉴权不得读取。
fn device_id_path() -> Option<PathBuf> {
    crate::paths::data_file("device_id")
}

/// 读取旧版全局设备 id。仅供非联网的兼容逻辑使用。
pub fn load_device_id() -> Option<String> {
    load_device_id_from(&device_id_path()?)
}

/// 从指定路径读设备 id（拆出来供单测注入临时路径，不碰真实数据目录）。
/// 空 / 纯空白（老实现「半写成空」的残留）视作 `None`。
fn load_device_id_from(path: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim_start_matches('\u{feff}').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 保存旧版全局设备 id。仅供非联网的兼容逻辑使用；新登录必须调用
/// [`save_device_id_for_origin`]。
///
/// **原子写**（同目录临时文件 + rename，与 [`crate::profile`] 同款）：老实现用 `fs::write`
/// 截断写又静默吞错，一旦半写成空 / 写失败而 profile.json 仍有值，就会长期潜伏、直到某次
/// 重登带空 device_id、被服务端当新机造出「幽灵设备」。改原子写并把错误上抛由调用方处置。
/// 数据目录不可用时视为「本次运行不持久化」，返回 `Ok`。
pub fn save_device_id(id: &str) -> std::io::Result<()> {
    match device_id_path() {
        Some(p) => save_device_id_to(&p, id),
        None => Ok(()), // 数据目录不可用：本次运行不持久化（与 profile 同语义）。
    }
}

/// 原子写设备 id 到指定路径（同目录临时文件 + rename），拆出来供单测注入临时路径。
fn save_device_id_to(path: &std::path::Path, id: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, id.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 旧版启动对账。只修复不再参与联网的全局文件；新流程使用
/// [`reconcile_device_id_for_origin`]。
///
/// 独立文件与 `profile.device_id` 是同一个 id 的两处副本，但重登只读独立文件、运行期又
/// 从不再读它——一旦独立文件缺失 / 为空而 profile 仍有 id（历史写失败 / 外部清理 / 曾在别的
/// 数据目录写过），下次重登就会带空 id 造出幽灵。启动时若独立文件读不到而 profile 有值，
/// 用 profile 的值回写修复，把背离消灭在爆发之前。返回是否发生了修复（供日志）。
pub fn reconcile_device_id(profile_device_id: Option<&str>) -> bool {
    if load_device_id().is_some() {
        return false; // 独立文件已有值：无需修复。
    }
    let Some(pid) = profile_device_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false; // profile 也没有：首次登录前的正常状态。
    };
    match save_device_id(pid) {
        Ok(()) => {
            log::info!("已用 profile.device_id 回写修复缺失的 device_id 文件");
            true
        }
        Err(e) => {
            log::warn!("回写 device_id 文件失败: {e}");
            false
        }
    }
}

/// 返回原始稳定机器标识的进程内缓存，仅供生成按 origin 的不可关联伪名。
fn raw_hardware_id() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(read_machine_guid).clone()
}

/// 为指定 canonical origin 生成稳定、不可跨服务关联的硬件伪名：
/// `SHA256(origin || 0x00 || machine_id)`。
///
/// 原始 MachineGuid / machine-id 永不交给自建服务端；不同 origin 得到不同伪名。
pub fn hardware_id_for_origin(origin: &str) -> Option<String> {
    let canonical = canonical_server_origin(origin).ok()?;
    let machine_id = raw_hardware_id()?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hasher.update([0]);
    hasher.update(machine_id.as_bytes());
    Some(hex_digest(hasher.finalize()))
}

/// 读 `MachineGuid`（Windows 实现）。
#[cfg(windows)]
fn read_machine_guid() -> Option<String> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
    };
    // MachineGuid 是 36 字符 GUID 串，128 个 u16 足够容纳（含结尾 NUL）。
    let mut buf = [0u16; 128];
    let mut cb = u32::try_from(std::mem::size_of_val(&buf)).unwrap_or(0);
    // SAFETY: buf/cb 均为本地栈缓冲，指针在调用期间有效；RegGetValueW 至多写 cb 字节到 buf
    // 并把实际字节数回写 cb。子键 / 值名为静态宽字符串常量。RRF_SUBKEY_WOW6464KEY 强制 64 位
    // 视图（本应用 x64，避免 WOW64 重定向）。
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Cryptography"),
            w!("MachineGuid"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut cb),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    // cb 为字节数（含结尾 NUL）→ 元素数；去尾部 NUL 后按 UTF-16 解码。
    let elems = (usize::try_from(cb).unwrap_or(0) / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..elems]);
    let s = s.trim_end_matches('\u{0}').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// 读稳定机器标识（Linux 实现）：`/etc/machine-id`，回退
/// `/var/lib/dbus/machine-id`（无 systemd 的老发行版）。该值由
/// systemd/dbus 在系统首次启动时生成，跨重启 / 更新恒定，语义上等价
/// Windows 的 `MachineGuid`。读不到（受限容器 / 权限）返回 `None`。
#[cfg(target_os = "linux")]
fn read_machine_guid() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// 读稳定机器标识（macOS 实现）：`ioreg` 读 `IOPlatformExpertDevice` 的
/// `IOPlatformUUID`。该 UUID 绑定主板、跨系统更新恒定，语义等价
/// `MachineGuid`。`ioreg` 是 macOS 自带命令，零第三方依赖；读不到返回 `None`。
#[cfg(target_os = "macos")]
fn read_machine_guid() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // 目标行形如：    "IOPlatformUUID" = "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX"
    // 取 `IOPlatformUUID` 之后第一对引号中间的内容（split('"').nth(1)）。
    for line in text.lines() {
        let Some((_, after)) = line.split_once("IOPlatformUUID") else {
            continue;
        };
        if let Some(uuid) = after.split('"').nth(1) {
            let uuid = uuid.trim();
            if !uuid.is_empty() {
                return Some(uuid.to_string());
            }
        }
    }
    None
}

/// 其余平台（BSD 等）：无统一稳定标识源，返回 `None`（服务端退化按 `device_id`）。
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn read_machine_guid() -> Option<String> {
    None
}

/// 本机设备显示名。Windows 取 `COMPUTERNAME`；unix 优先 `HOSTNAME`
/// 环境变量、回退 `hostname` 命令；一律兜底 `Lumen-PC`。
pub fn device_name() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Lumen-PC".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(unix_hostname)
            .unwrap_or_else(|| "Lumen-PC".to_string())
    }
}

/// unix：调 `hostname` 命令取主机名（`HOSTNAME` 环境变量常未导出到进程）。
#[cfg(not(windows))]
fn unix_hostname() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 账号rest请求禁止自动重定向() {
        assert_eq!(REST_MAX_REDIRECTS, 0);
    }

    /// 每个测试独立临时目录，避免并行互踩，且绝不碰真实数据目录。
    fn temp_devid_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lumen_devid_test_{}_{name}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("device_id")
    }

    #[test]
    fn 设备id原子写往返() {
        let p = temp_devid_path("roundtrip");
        let _ = std::fs::remove_file(&p);
        // 缺失 → None。
        assert_eq!(load_device_id_from(&p), None);
        // 写入 → 读回一致；rename 后不应残留 .tmp。
        save_device_id_to(&p, "dev-123").expect("写盘");
        assert_eq!(load_device_id_from(&p), Some("dev-123".to_string()));
        assert!(!p.with_extension("tmp").exists(), "原子写后不应残留 tmp");
        // 覆盖写生效。
        save_device_id_to(&p, "dev-456").expect("覆盖");
        assert_eq!(load_device_id_from(&p), Some("dev-456".to_string()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 空白设备id视作缺失() {
        // 老实现「半写成空/纯空白」的残留必须被当成 None——否则重登会带空 id 造幽灵。
        let p = temp_devid_path("blank");
        save_device_id_to(&p, "   \r\n").expect("写空白");
        assert_eq!(load_device_id_from(&p), None);
        // BOM 前缀也应被剥离后判空/取值。
        std::fs::write(&p, "\u{feff}dev-bom").expect("写 BOM");
        assert_eq!(load_device_id_from(&p), Some("dev-bom".to_string()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 硬件标识幂等() {
        // 同一 origin 幂等，不同 origin 不可关联；对外只给 64 位十六进制摘要。
        let a = hardware_id_for_origin("https://a.example");
        assert_eq!(a, hardware_id_for_origin("a.example"));
        let b = hardware_id_for_origin("https://b.example");
        if let (Some(a), Some(b)) = (&a, &b) {
            assert_eq!(a.len(), 64);
            assert_ne!(a, b);
            assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        // Windows 上 MachineGuid 恒存在：必须真的读到一个合法 GUID（36 字符、含连字符），
        // 但该原值仅在本模块内参与哈希，绝不由联网 API 返回。
        #[cfg(windows)]
        {
            let hw = raw_hardware_id().expect("Windows 应能读到 MachineGuid");
            assert_eq!(hw.len(), 36, "MachineGuid 应为 36 字符 GUID：{hw}");
            assert_eq!(hw.matches('-').count(), 4, "GUID 应含 4 个连字符：{hw}");
        }
    }

    #[test]
    fn 错误码与服务端文案不泄漏() {
        let e = CloudError::Api {
            status: 404,
            code: "user_not_found".to_string(),
            message: "x".to_string(),
        };
        assert_eq!(e.code(), "user_not_found");
        let hostile = CloudError::Api {
            status: 500,
            code: "BAD\r\nCode".to_owned(),
            message: "secret response\r\ninjection".to_owned(),
        };
        assert_eq!(hostile.code(), "http_error");
        assert!(!hostile.user_message().contains("secret"));
        assert!(!hostile.user_message().contains("BAD"));
        let net = CloudError::Network("boom".to_string());
        assert!(net.user_message().contains("无法连接"));
    }

    #[test]
    fn canonical_origin_规范化() {
        assert_eq!(
            canonical_server_origin("Example.COM:443/").as_deref(),
            Ok("https://example.com")
        );
        assert_eq!(
            canonical_server_origin("HTTPS://Example.COM:443/").as_deref(),
            Ok("https://example.com")
        );
        assert_eq!(
            canonical_server_origin("http://127.0.0.1:80/").as_deref(),
            Ok("http://127.0.0.1")
        );
        assert_eq!(
            canonical_server_origin("https://[::1]:8443/").as_deref(),
            Ok("https://[::1]:8443")
        );
    }

    #[test]
    fn canonical_origin_拒绝非根与注入() {
        for invalid in [
            "",
            "   ",
            "ftp://example.com",
            "https://example.com.",
            "https://@example.com",
            "https://user@example.com",
            "https://example.com:",
            "https://example.com/./",
            "https://example.com/a/../",
            "https://example.com/path",
            "https://example.com?query=1",
            "https://example.com#fragment",
            "https://example.com/\r\n",
        ] {
            assert!(
                matches!(
                    canonical_server_origin(invalid),
                    Err(CloudError::InvalidServerOrigin)
                ),
                "应拒绝测试输入"
            );
        }
    }

    #[test]
    fn 明文传输仅允许回环或显式开关() {
        let https = Url::parse("https://example.com").expect("URL");
        let remote_http = Url::parse("http://192.0.2.1").expect("URL");
        let localhost = Url::parse("http://localhost:8787").expect("URL");
        let loopback_v4 = Url::parse("http://127.0.0.1:8787").expect("URL");
        let loopback_v6 = Url::parse("http://[::1]:8787").expect("URL");
        assert_eq!(transport_allowed(&https, false), Ok(true));
        assert_eq!(transport_allowed(&remote_http, false), Ok(false));
        assert_eq!(transport_allowed(&remote_http, true), Ok(true));
        assert_eq!(transport_allowed(&localhost, false), Ok(true));
        assert_eq!(transport_allowed(&loopback_v4, false), Ok(true));
        assert_eq!(transport_allowed(&loopback_v6, false), Ok(true));
    }

    #[test]
    fn 设备id按origin分区且不读旧全局文件() {
        let root =
            std::env::temp_dir().join(format!("lumen_origin_device_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let a = device_id_path_for_origin_at(&root, "https://a.example").expect("origin A");
        let a_equivalent =
            device_id_path_for_origin_at(&root, "a.example:443/").expect("origin A equivalent");
        let b = device_id_path_for_origin_at(&root, "https://b.example").expect("origin B");
        assert_eq!(a, a_equivalent);
        assert_ne!(a, b);
        save_device_id_to(&a, "device-a").expect("写 A");
        save_device_id_to(&b, "device-b").expect("写 B");
        assert_eq!(load_device_id_from(&a).as_deref(), Some("device-a"));
        assert_eq!(load_device_id_from(&b).as_deref(), Some("device-b"));
        assert_eq!(
            a.file_name().and_then(|value| value.to_str()),
            Some("device_id")
        );
        assert_eq!(
            a.parent()
                .and_then(std::path::Path::file_name)
                .and_then(|value| value.to_str())
                .map(str::len),
            Some(64)
        );
        assert_ne!(a, root.join("device_id"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
