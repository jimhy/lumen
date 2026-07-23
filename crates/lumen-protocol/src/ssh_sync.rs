//! SSH 服务器清单的账号级增量同步协议。
//!
//! 本模块刻意采用显式 allowlist，而不是透传客户端本地模型。密码、私钥、私钥
//! 路径、口令和操作系统凭据引用都没有线缆字段；所有上行类型还启用
//! `deny_unknown_fields`，防止未来调用方把本地秘密“顺手”塞进请求。

use serde::{Deserialize, Serialize};

/// 当前 SSH 同步 schema 版本。它独立于远程控制 [`crate::PROTOCOL_VERSION`]。
pub const SSH_SYNC_SCHEMA_VERSION: u32 = 1;
/// 分组 ID：`grp_` + 32 位小写十六进制。
pub const SSH_GROUP_ID_PREFIX: &str = "grp_";
/// 服务器配置 ID：`ssh_` + 32 位小写十六进制。
pub const SSH_PROFILE_ID_PREFIX: &str = "ssh_";
/// 幂等变更 ID：`mut_` + 32 位小写十六进制。
pub const SSH_MUTATION_ID_PREFIX: &str = "mut_";

/// 单次请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshSyncRequest {
    /// 必须为 [`SSH_SYNC_SCHEMA_VERSION`]。
    pub schema_version: u32,
    /// 仅拉取 `revision > cursor` 的账号级变更。
    #[serde(default)]
    pub cursor: i64,
    /// 本批最多返回的变更数；服务端限制为 1..=200。
    #[serde(default = "default_limit")]
    pub limit: u16,
    /// 待幂等应用的本地变更。
    #[serde(default)]
    pub mutations: Vec<SshMutation>,
}

const fn default_limit() -> u16 {
    200
}

/// 一条可重试的客户端变更。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshMutation {
    /// 客户端生成的稳定幂等键（`mut_` + 32 位小写十六进制）；重试必须
    /// 原样复用。同一账号重复使用该 ID 却改变 `base_revision` 或操作正文会被拒绝。
    pub mutation_id: String,
    /// 客户端编辑前看到的服务端 revision，仅供审计和冲突观测。
    ///
    /// schema v1 按产品约定采用服务端接收顺序 LWW：同一现存记录即使
    /// `base_revision` 已陈旧也会应用；已删除 ID 则始终由墓碑阻止复活。
    pub base_revision: i64,
    /// 实际操作。
    pub operation: SshMutationOperation,
}

/// 支持的分组/服务器操作。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SshMutationOperation {
    /// 新建或覆盖一个分组。
    UpsertGroup {
        /// 可同步分组字段。
        group: SshGroup,
    },
    /// 删除分组；组内服务器由服务端逐条归入未分组。
    DeleteGroup {
        /// 稳定分组 id。
        group_id: String,
    },
    /// 新建或覆盖一个 SSH 服务器配置。
    UpsertProfile {
        /// 可同步服务器字段。
        profile: SshProfile,
    },
    /// 删除一个 SSH 服务器配置。
    DeleteProfile {
        /// 稳定服务器配置 id。
        profile_id: String,
    },
}

/// 可同步的 SSH 分组字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshGroup {
    /// 稳定分组 id。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 列表排序值。
    pub sort_order: u32,
    /// 客户端首次创建时间（Unix 毫秒，仅用于展示/诊断）。
    pub created_at_ms: u64,
    /// 客户端最近编辑时间（Unix 毫秒，仅用于展示/诊断）。
    pub updated_at_ms: u64,
}

/// 认证方式偏好；只表达类型，不承载认证材料。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SshAuthMethod {
    /// 本机密码凭据。
    Password,
    /// 本机私钥绑定。
    PrivateKey,
    /// 本机 SSH agent。
    Agent,
}

/// 已确认的公开主机密钥身份。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshHostKeyTrust {
    /// SSH 主机密钥算法。
    pub algorithm: String,
    /// 公开密钥指纹。
    pub fingerprint: String,
}

/// 可同步的 SSH 服务器连接元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshProfile {
    /// 稳定服务器配置 id。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 主机名或 IP。
    pub host: String,
    /// TCP 端口。
    pub port: u16,
    /// SSH 用户名。
    pub username: String,
    /// 认证方式偏好（不含密码、密钥、路径或凭据引用）。
    pub auth_method: SshAuthMethod,
    /// 所属真实分组；`None` 表示未分组。
    pub group_id: Option<String>,
    /// 组内排序值。
    pub sort_order: u32,
    /// 登录后的初始远程目录。
    pub initial_directory: Option<String>,
    /// 连接超时。
    pub connect_timeout_secs: u32,
    /// keepalive 周期；`None` 表示关闭。
    pub keep_alive_secs: Option<u32>,
    /// 是否采集服务器监控信息。
    pub monitor_enabled: bool,
    /// 用户确认过的公开主机指纹。
    pub trusted_host_key: Option<SshHostKeyTrust>,
    /// 客户端首次创建时间（Unix 毫秒）。
    pub created_at_ms: u64,
    /// 客户端最近编辑时间（Unix 毫秒）。
    pub updated_at_ms: u64,
}

/// 同步响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshSyncResponse {
    /// 当前响应 schema。
    pub schema_version: u32,
    /// 本批幂等应用结果，与请求 mutations 一一对应。
    pub acks: Vec<SshMutationAck>,
    /// `cursor` 之后的账号级变更，按 revision 升序。
    pub changes: Vec<SshSyncChange>,
    /// 客户端下一次请求应携带的游标。
    pub next_cursor: i64,
    /// 是否仍有更晚变更，若为 true 应立即续拉。
    pub has_more: bool,
}

/// mutation 的幂等处理结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshMutationAck {
    /// 请求中的幂等键。
    pub mutation_id: String,
    /// 处理状态。
    pub status: SshMutationStatus,
    /// 本操作最终对应的账号 revision；被拒绝时可为空。
    pub revision: Option<i64>,
    /// 被拒绝时的机器可读原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// mutation 处理状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SshMutationStatus {
    /// 本次已应用。
    Applied,
    /// 此 mutation_id 已处理，返回第一次的结果。
    Duplicate,
    /// 合法请求但因墓碑或记录冲突未应用。
    Rejected,
}

/// 一条服务端权威变更。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SshSyncChange {
    /// 账号内严格单调递增 revision。
    pub revision: i64,
    /// 产生变更的设备 id（由鉴权 token 决定）。
    pub updated_by_device: String,
    /// 变更正文。
    #[serde(flatten)]
    pub change: SshChange,
}

/// 服务端下行的分组/服务器变更。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SshChange {
    /// 分组当前权威值。
    UpsertGroup {
        /// 分组。
        group: SshGroup,
    },
    /// 分组墓碑。
    DeleteGroup {
        /// 已删除分组 id。
        group_id: String,
    },
    /// 服务器配置当前权威值。
    UpsertProfile {
        /// 服务器配置。
        profile: SshProfile,
    },
    /// 服务器配置墓碑。
    DeleteProfile {
        /// 已删除服务器配置 id。
        profile_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_profile_extra(extra: &str) -> String {
        let suffix = if extra.is_empty() {
            String::new()
        } else {
            format!(",{extra}")
        };
        format!(
            r#"{{
                "schema_version":1,"cursor":0,"limit":200,
                "mutations":[{{
                    "mutation_id":"m1","base_revision":0,
                    "operation":{{
                        "type":"upsert_profile",
                        "profile":{{
                            "id":"p1","name":"prod","host":"example.test","port":22,
                            "username":"alice","auth_method":"private_key","group_id":null,
                            "sort_order":0,"initial_directory":null,
                            "connect_timeout_secs":15,"keep_alive_secs":30,
                            "monitor_enabled":true,"trusted_host_key":null,
                            "created_at_ms":1,"updated_at_ms":2{suffix}
                        }}
                    }}
                }}]
            }}"#
        )
    }

    #[test]
    fn ssh同步路径固定() {
        assert_eq!(crate::routes::SYNC_SSH, "/api/v1/sync/ssh");
    }

    #[test]
    fn 未知秘密字段在所有上行层级被拒绝() {
        for field in [
            r#""password":"secret""#,
            r#""private_key":"pem""#,
            r#""private_key_path":"C:\\secret\\id_ed25519""#,
            r#""path":"C:\\secret\\id_ed25519""#,
            r#""passphrase":"secret""#,
            r#""credential_ref":"vault:1""#,
            r#""user_id":"victim""#,
            r#""device_id":"other-device""#,
            r#""revision":99"#,
        ] {
            let err = serde_json::from_str::<SshSyncRequest>(&request_with_profile_extra(field))
                .expect_err("allowlist 外字段必须拒绝");
            assert!(err.to_string().contains("unknown field"), "{field}: {err}");
        }
    }

    #[test]
    fn mutation和请求也拒绝身份或版本注入() {
        let top = r#"{"schema_version":1,"cursor":0,"limit":1,"mutations":[],"user_id":"u"}"#;
        assert!(serde_json::from_str::<SshSyncRequest>(top).is_err());

        let mutation = r#"{
            "schema_version":1,"cursor":0,"limit":1,
            "mutations":[{
                "mutation_id":"m","base_revision":0,"revision":8,
                "operation":{"type":"delete_profile","profile_id":"p"}
            }]
        }"#;
        assert!(serde_json::from_str::<SshSyncRequest>(mutation).is_err());
    }

    #[test]
    fn 合法请求与响应往返() {
        let req: SshSyncRequest =
            serde_json::from_str(&request_with_profile_extra("")).expect("合法请求");
        assert_eq!(req.schema_version, SSH_SYNC_SCHEMA_VERSION);
        assert_eq!(req.mutations.len(), 1);

        let resp = SshSyncResponse {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            acks: vec![SshMutationAck {
                mutation_id: "m1".into(),
                status: SshMutationStatus::Applied,
                revision: Some(1),
                error_code: None,
            }],
            changes: vec![],
            next_cursor: 1,
            has_more: false,
        };
        let json = serde_json::to_string(&resp).expect("序列化");
        assert_eq!(
            serde_json::from_str::<SshSyncResponse>(&json).expect("反序列化"),
            resp
        );
    }

    #[test]
    fn 下行结构同样拒绝allowlist外字段() {
        let response = r#"{
            "schema_version":1,
            "acks":[{
                "mutation_id":"mut_00000000000000000000000000000001",
                "status":"applied","revision":1,"unexpected":true
            }],
            "changes":[],"next_cursor":1,"has_more":false
        }"#;
        assert!(serde_json::from_str::<SshSyncResponse>(response).is_err());

        let change = r#"{
            "revision":1,"updated_by_device":"device",
            "type":"delete_group",
            "group_id":"grp_00000000000000000000000000000001",
            "private_key":"must-not-be-accepted"
        }"#;
        assert!(serde_json::from_str::<SshSyncChange>(change).is_err());
    }
}
