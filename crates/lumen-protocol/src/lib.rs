//! Lumen 远程控制协议（M5）：客户端与 `lumen-server` 共享的线缆类型。
//!
//! 本 crate **零平台依赖**（纯 `serde` 结构体），同时被 Windows 客户端
//! （`lumen-app`，经 `ureq` 发 REST）、本地测试 server 与 Linux 生产
//! server（`lumen-server`，axum）依赖，保证两端类型不漂移。
//!
//! 所有响应带协议版本（[`PROTOCOL_VERSION`]）；REST 路径集中在 [`routes`]
//! 模块，避免客户端/服务端各写一份字符串而拼错。
//!
//! 覆盖范围按里程碑推进：**M5.1** 账户 + 设备登记 + 设置/历史同步（本文件
//! 已含）；M5.2 设备在线、M5.3 终端远程、M5.4 文件传输的消息后续在此扩展。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

pub mod remote;

/// 协议版本号。任何破坏性变更必须递增；登录响应回传，客户端可比对。
///
/// **v2（M5.3 part3d Phase 1）**：远程镜像数据面由「无 id 单焦点窗格」`Output`/`Resize`
/// 一次性切到「`(TabId, SessionId)` 双 id 多会话」`OutputWithId`/`ResizeWithId`/
/// `SubscriptionStarted`（K2：不双发灰度）。旧 v1 客户端收新帧 `from_value` 失败即丢弃、
/// 镜像空白，故须配 [`MIN_SUPPORTED_VERSION`] 版本门把 v1 挡在配对前。
pub const PROTOCOL_VERSION: u32 = 2;

/// 服务端仍兼容的最低客户端协议版本（M5.3 WebSocket `Welcome` 下发；低于此
/// 的客户端应提示用户升级）。当前 = [`PROTOCOL_VERSION`]，破坏性裁撤旧消息时上调。
///
/// part3d Phase 1 上调至 2：part3d 双 id 数据面与 v1 单焦点镜像不兼容，两端须同为 ≥2。
pub const MIN_SUPPORTED_VERSION: u32 = 2;

/// REST 端点路径（客户端与服务端共用，避免字符串漂移）。
pub mod routes {
    /// 健康检查 `GET`。
    pub const HEALTH: &str = "/api/v1/health";
    /// 注册 `POST`。
    pub const REGISTER: &str = "/api/v1/auth/register";
    /// 登录 `POST`（成功即登记/更新本设备）。
    pub const LOGIN: &str = "/api/v1/auth/login";
    /// 续期 token `POST`（需 Bearer 现有**有效** token，换发新 token；M5 自动续期，免 7 天到期掉线）。
    pub const REFRESH: &str = "/api/v1/auth/refresh";
    /// 设备列表 `GET`（需 Bearer token）。
    pub const DEVICES: &str = "/api/v1/devices";
    /// 偏好设置同步：`GET` 拉取 / `PUT` 推送。
    pub const SYNC_SETTINGS: &str = "/api/v1/sync/settings";
    /// 命令历史同步：`GET ?since=<ts_ms>` 拉取 / `POST` 推送。
    pub const SYNC_HISTORY: &str = "/api/v1/sync/history";
    /// 设备心跳 `POST`（保持本设备在线，刷新 `last_seen`；M5.2）。
    pub const HEARTBEAT: &str = "/api/v1/heartbeat";
    /// 远程控制 WebSocket 长连接 `GET`（升级；M5.3 终端远程，需 `Authorization` 头）。
    pub const WS: &str = "/api/v1/ws";

    /// 单设备路径（重命名 `PATCH` / 删除 `DELETE`）。
    #[must_use]
    pub fn device(id: &str) -> String {
        format!("/api/v1/devices/{id}")
    }
}

/// 统一错误响应体（HTTP 4xx/5xx 时返回）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiError {
    /// 机器可读错误码（如 `email_taken`、`invalid_credentials`）。
    pub code: String,
    /// 人类可读说明（英文，UI 侧可按 `code` 自行本地化）。
    pub message: String,
}

impl ApiError {
    /// 构造一个错误响应体。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// 客户端上报的设备信息（登录时携带）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    /// 已有设备 id（首次登录为 `None`，由服务端分配并回传，客户端持久化后续带上）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// **稳定硬件标识**（Windows 取 `MachineGuid`）：对「更新 app / 删本地文件 / 换数据
    /// 目录 / 服务端 DB 重置」全都不变，只在重装系统时才变。服务端据 `(user_id, hw_id)`
    /// 幂等认领同一物理机的唯一设备行，杜绝「带空/异 device_id 就分裂出幽灵设备」。
    /// `Option` + `skip_serializing_if`：老服务端忽略该字段、老客户端不发该字段，双向兼容；
    /// 取不到（受限机器/非 Windows）为 `None`，服务端退化回按 `device_id` 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hw_id: Option<String>,
    /// 设备显示名（默认取机器名，用户可改）。
    pub name: String,
    /// 操作系统标识（如 `windows`）。
    pub os: String,
    /// 客户端版本（如 `0.1.9`）。
    pub app_version: String,
}

/// 注册请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// 账户邮箱。
    pub email: String,
    /// 明文密码（仅传输用，服务端 argon2 哈希后落库，绝不明文存储）。
    pub password: String,
}

/// 登录请求（成功即登记/更新本设备的 `last_seen`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    /// 账户邮箱。
    pub email: String,
    /// 明文密码（仅传输用）。
    pub password: String,
    /// 本设备信息（首次 `device_id` 为 `None`，由服务端分配）。
    pub device: DeviceInfo,
}

/// 账户公开信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInfo {
    /// 账户 id（uuid 字符串）。
    pub id: String,
    /// 邮箱。
    pub email: String,
    /// 展示名（注册时取邮箱 `@` 前段）。
    pub display_name: String,
}

/// 注册/登录成功响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    /// 服务端协议版本（客户端可比对）。
    pub protocol_version: u32,
    /// Bearer token（JWT，短期，客户端持久化用于后续鉴权）。
    pub token: String,
    /// token 过期 Unix 秒。
    pub expires_at: i64,
    /// 账户信息。
    pub user: UserInfo,
    /// 本设备 id（首次登录由服务端分配，客户端需持久化）。
    pub device_id: String,
}

/// token 续期响应（`POST /auth/refresh`）：用现有有效 token 换发新 token，避免 7 天到期掉线。
/// 仅含新 token 与到期时间——账户/设备信息客户端已有，无需重复回传。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshResponse {
    /// 新 Bearer token（JWT，客户端持久化覆盖旧 token）。
    pub token: String,
    /// 新 token 过期 Unix 秒。
    pub expires_at: i64,
}

/// 设备列表项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    /// 设备 id（uuid 字符串）。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 操作系统标识。
    pub os: String,
    /// 客户端版本。
    pub app_version: String,
    /// 是否在线（M5.2 心跳维护；M5.1 暂以 `last_seen` 是否在阈值内粗略判定）。
    pub online: bool,
    /// 最近活跃 Unix 秒。
    pub last_seen: i64,
    /// 是否为发起请求的本设备。
    pub is_self: bool,
}

/// 设备列表响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceListResponse {
    /// 同账户下全部设备（在线优先由客户端排序）。
    pub devices: Vec<DeviceRecord>,
}

/// 重命名设备请求（`PATCH /devices/{id}`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameDeviceRequest {
    /// 新显示名。
    pub name: String,
}

/// 偏好设置同步载荷（按 `version` 做 last-write-wins 的整体 blob）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsSync {
    /// 单调递增版本（客户端每次本地变更自增；服务端只接受更大的 `version`）。
    pub version: i64,
    /// 偏好数据（客户端序列化的 JSON 字符串；服务端不解释、原样存取）。
    pub data: String,
}

/// 命令历史条目（与客户端 `history.jsonl` 对齐的同步形态）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    /// 命令文本。
    pub text: String,
    /// 录入时刻 Unix 毫秒（去重键 = `text` + `ts`）。
    pub ts: i64,
    /// 录入时 cwd（跨机仅供展示/过滤，可空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 退出码（可空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// 历史推送请求（多设备来源按 `text`+`ts` 去重合并）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPushRequest {
    /// 本批要上行的历史条目。
    pub entries: Vec<HistoryEntry>,
}

/// 历史拉取响应（`since` 之后的增量）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPullResponse {
    /// `since` 之后新增的历史条目（按 `ts` 升序）。
    pub entries: Vec<HistoryEntry>,
    /// 本批最大 `ts`（客户端存为下次 `since` 水位线；空批回传请求的 `since`）。
    pub watermark: i64,
    /// 本批是否被单批上限截断、`watermark` 之后仍有更多——客户端应据此用新 `since=watermark`
    /// 立即续拉，而非等下次周期同步。`#[serde(default)]` = 旧服务端缺该字段时按 `false` 处理。
    #[serde(default)]
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 设备路径拼接() {
        assert_eq!(routes::device("abc-123"), "/api/v1/devices/abc-123");
    }

    #[test]
    fn 鉴权响应往返() {
        let resp = AuthResponse {
            protocol_version: PROTOCOL_VERSION,
            token: "t".into(),
            expires_at: 123,
            user: UserInfo {
                id: "u1".into(),
                email: "a@b.c".into(),
                display_name: "a".into(),
            },
            device_id: "d1".into(),
        };
        let json = serde_json::to_string(&resp).expect("序列化");
        let back: AuthResponse = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.device_id, "d1");
        assert_eq!(back.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn 设备信息可省略id() {
        // 首次登录 device_id = None，不应出现在 JSON 里。
        let info = DeviceInfo {
            device_id: None,
            hw_id: None,
            name: "PC".into(),
            os: "windows".into(),
            app_version: "0.1.9".into(),
        };
        let json = serde_json::to_string(&info).expect("序列化");
        assert!(!json.contains("device_id"), "None 的 device_id 不应序列化");
        assert!(!json.contains("hw_id"), "None 的 hw_id 不应序列化");
        let back: DeviceInfo = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, info);
    }

    #[test]
    fn 设备信息带hw_id往返() {
        // hw_id 非空：应出现在 JSON 里且往返一致；老服务端遇未知/缺字段也能解析。
        let info = DeviceInfo {
            device_id: Some("d1".into()),
            hw_id: Some("MACHINE-GUID-1234".into()),
            name: "PC".into(),
            os: "windows".into(),
            app_version: "0.1.9".into(),
        };
        let json = serde_json::to_string(&info).expect("序列化");
        assert!(json.contains("hw_id"), "非空 hw_id 应序列化");
        let back: DeviceInfo = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, info);
        // 老客户端（无 hw_id 字段）的 JSON：hw_id 缺省为 None，向后兼容。
        let legacy: DeviceInfo =
            serde_json::from_str(r#"{"name":"PC","os":"windows","app_version":"0.1.9"}"#)
                .expect("老 JSON 应可解析");
        assert_eq!(legacy.hw_id, None);
        assert_eq!(legacy.device_id, None);
    }

    #[test]
    fn 历史条目可选字段省略() {
        let e = HistoryEntry {
            text: "ls".into(),
            ts: 1,
            cwd: None,
            exit_code: None,
        };
        let json = serde_json::to_string(&e).expect("序列化");
        assert!(!json.contains("cwd"));
        assert!(!json.contains("exit_code"));
    }

    #[test]
    fn 续期响应往返() {
        let resp = RefreshResponse {
            token: "new-jwt".into(),
            expires_at: 1_782_999_999,
        };
        let json = serde_json::to_string(&resp).expect("序列化");
        let back: RefreshResponse = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.token, "new-jwt");
        assert_eq!(back.expires_at, 1_782_999_999);
    }

    #[test]
    fn 历史拉取响应_has_more_往返与默认() {
        // 显式 has_more=true 往返保真。
        let resp = HistoryPullResponse {
            entries: vec![],
            watermark: 99,
            has_more: true,
        };
        let json = serde_json::to_string(&resp).expect("序列化");
        let back: HistoryPullResponse = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.watermark, 99);
        assert!(back.has_more);
        // 旧服务端响应缺 has_more → serde(default) 落 false（混版本前向兼容）。
        let legacy: HistoryPullResponse =
            serde_json::from_str(r#"{"entries":[],"watermark":7}"#).expect("旧响应反序列化");
        assert_eq!(legacy.watermark, 7);
        assert!(!legacy.has_more, "缺字段应默认 false");
    }
}
