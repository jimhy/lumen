//! 账号级 SSH 服务器清单增量同步。
//!
//! 与 `settings_sync` blob 和远程控制 WebSocket 完全隔离。身份只取自
//! [`AuthUser`]；请求体没有 user/device 字段。每个请求在一个数据库事务内先
//! 幂等应用 mutations，再按账号 revision 拉取增量。

use axum::extract::State;
use axum::Json;
use lumen_protocol::ssh_sync::{
    SshAuthMethod, SshChange, SshGroup, SshHostKeyTrust, SshMutation, SshMutationAck,
    SshMutationOperation, SshMutationStatus, SshProfile, SshSyncChange, SshSyncRequest,
    SshSyncResponse, SSH_GROUP_ID_PREFIX, SSH_MUTATION_ID_PREFIX, SSH_PROFILE_ID_PREFIX,
    SSH_SYNC_SCHEMA_VERSION,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_postgres::{Row, Transaction};

use crate::auth::{self, AuthUser};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const MAX_BATCH: usize = 200;
/// 与 `lumen-app::ssh::inventory::MAX_GROUPS` 保持一致。
const MAX_GROUPS: usize = 1_000;
/// 与 `lumen-app::ssh::inventory::MAX_PROFILES` 保持一致。
const MAX_PROFILES: usize = 10_000;
/// 账号在活跃记录和墓碑之间可消耗的终身实体槽位。
///
/// 删除现存实体只是把活跃行换成墓碑，不增加总量；删除从未存在的 ID 和新建
/// 实体只有在仍有槽位时才允许，因此墓碑/ID 探测不能无限灌库。
const MAX_RETAINED_ENTITIES: usize = 100_000;
const MAX_GROUP_NAME_LEN: usize = 50;
const MAX_PROFILE_NAME_LEN: usize = 100;
const MAX_HOST_LEN: usize = 255;
const MAX_USERNAME_LEN: usize = 128;
const MAX_INITIAL_DIRECTORY_LEN: usize = 4_096;
const MAX_HOST_KEY_ALGORITHM_LEN: usize = 64;
const MAX_HOST_KEY_FINGERPRINT_LEN: usize = 256;
const MAX_CONNECT_TIMEOUT_SECS: u32 = 300;
const MIN_KEEP_ALIVE_SECS: u32 = 1;
const MAX_KEEP_ALIVE_SECS: u32 = 3600;

/// `POST /api/v1/sync/ssh`。
pub async fn sync_ssh(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<SshSyncRequest>,
) -> AppResult<Json<SshSyncResponse>> {
    validate_request(&req)?;

    let mut client = state.pool.get().await?;
    let transaction = client.transaction().await?;
    // AuthUser 已在提取阶段查过一次。事务内再持有设备行共享锁，封住“鉴权
    // 完成后、mutation 落库前设备恰好被删除”的竞态。
    if transaction
        .query_opt(
            "SELECT 1 FROM devices WHERE id=$1 AND user_id=$2 FOR SHARE",
            &[&user.device_id, &user.user_id],
        )
        .await?
        .is_none()
    {
        return Err(AppError::Unauthorized);
    }
    transaction
        .execute(
            "INSERT INTO ssh_sync_heads (user_id,current_revision) VALUES ($1,0) \
             ON CONFLICT (user_id) DO NOTHING",
            &[&user.user_id],
        )
        .await?;
    let row = transaction
        .query_one(
            "SELECT current_revision FROM ssh_sync_heads WHERE user_id=$1 FOR UPDATE",
            &[&user.user_id],
        )
        .await?;
    let mut current_revision: i64 = row.get(0);
    // 游标代表客户端已经实际应用的账号 revision。未来游标既可能掩盖服务端
    // 变更，也会污染设备 checkpoint，所以必须在任何 mutation 落库前拒绝。
    if req.cursor > current_revision {
        return Err(AppError::BadRequest(format!(
            "cursor {} 超过账号当前 revision {current_revision}",
            req.cursor
        )));
    }
    let mut acks = Vec::with_capacity(req.mutations.len());

    for mutation in &req.mutations {
        let ack = apply_mutation(&transaction, &user, mutation, &mut current_revision).await?;
        acks.push(ack);
    }

    transaction
        .execute(
            "UPDATE ssh_sync_heads SET current_revision=$1 WHERE user_id=$2",
            &[&current_revision, &user.user_id],
        )
        .await?;

    let (changes, next_cursor, has_more) = pull_changes(
        &transaction,
        &user.user_id,
        req.cursor,
        usize::from(req.limit),
    )
    .await?;
    transaction
        .execute(
            "INSERT INTO ssh_sync_checkpoints (user_id,device_id,cursor,updated_at) \
             VALUES ($1,$2,$3,$4) \
             ON CONFLICT (user_id,device_id) DO UPDATE \
             SET cursor=GREATEST(ssh_sync_checkpoints.cursor,EXCLUDED.cursor),\
             updated_at=EXCLUDED.updated_at",
            &[
                &user.user_id,
                &user.device_id,
                // 本次响应尚未被客户端确认应用；只有请求携带的 cursor 才是 ACK。
                &req.cursor,
                &auth::now_secs(),
            ],
        )
        .await?;
    transaction.commit().await?;

    Ok(Json(SshSyncResponse {
        schema_version: SSH_SYNC_SCHEMA_VERSION,
        acks,
        changes,
        next_cursor,
        has_more,
    }))
}

fn validate_request(req: &SshSyncRequest) -> AppResult<()> {
    if req.schema_version != SSH_SYNC_SCHEMA_VERSION {
        return Err(AppError::BadRequest(format!(
            "不支持的 SSH 同步 schema_version {}",
            req.schema_version
        )));
    }
    if req.cursor < 0 {
        return Err(AppError::BadRequest("cursor 不能为负数".into()));
    }
    if req.limit == 0 || usize::from(req.limit) > MAX_BATCH {
        return Err(AppError::BadRequest(format!(
            "limit 必须在 1..={MAX_BATCH}"
        )));
    }
    if req.mutations.len() > MAX_BATCH {
        return Err(AppError::BadRequest(format!(
            "单批 mutation 数 {} 超过上限 {MAX_BATCH}",
            req.mutations.len()
        )));
    }
    for mutation in &req.mutations {
        validate_mutation(mutation)?;
    }
    Ok(())
}

fn validate_mutation(mutation: &SshMutation) -> AppResult<()> {
    validate_prefixed_id(&mutation.mutation_id, SSH_MUTATION_ID_PREFIX, "mutation_id")?;
    if mutation.base_revision < 0 {
        return Err(AppError::BadRequest("base_revision 不能为负数".into()));
    }
    match &mutation.operation {
        SshMutationOperation::UpsertGroup { group } => validate_group(group),
        SshMutationOperation::DeleteGroup { group_id } => {
            validate_prefixed_id(group_id, SSH_GROUP_ID_PREFIX, "group_id")
        }
        SshMutationOperation::UpsertProfile { profile } => validate_profile(profile),
        SshMutationOperation::DeleteProfile { profile_id } => {
            validate_prefixed_id(profile_id, SSH_PROFILE_ID_PREFIX, "profile_id")
        }
    }
}

fn validate_group(group: &SshGroup) -> AppResult<()> {
    validate_prefixed_id(&group.id, SSH_GROUP_ID_PREFIX, "group.id")?;
    validate_required_text(&group.name, MAX_GROUP_NAME_LEN, true, "group.name")?;
    validate_millis(group.created_at_ms, "group.created_at_ms")?;
    validate_millis(group.updated_at_ms, "group.updated_at_ms")
}

fn validate_profile(profile: &SshProfile) -> AppResult<()> {
    validate_prefixed_id(&profile.id, SSH_PROFILE_ID_PREFIX, "profile.id")?;
    validate_required_text(&profile.name, MAX_PROFILE_NAME_LEN, true, "profile.name")?;
    validate_required_text(&profile.host, MAX_HOST_LEN, false, "profile.host")?;
    validate_required_text(
        &profile.username,
        MAX_USERNAME_LEN,
        false,
        "profile.username",
    )?;
    if profile.port == 0 {
        return Err(AppError::BadRequest("profile.port 必须在 1..=65535".into()));
    }
    if let Some(group_id) = &profile.group_id {
        validate_prefixed_id(group_id, SSH_GROUP_ID_PREFIX, "profile.group_id")?;
    }
    if let Some(path) = &profile.initial_directory {
        validate_optional_text(
            path,
            MAX_INITIAL_DIRECTORY_LEN,
            true,
            "profile.initial_directory",
        )?;
    }
    if profile.connect_timeout_secs == 0 || profile.connect_timeout_secs > MAX_CONNECT_TIMEOUT_SECS
    {
        return Err(AppError::BadRequest(format!(
            "profile.connect_timeout_secs 必须在 1..={MAX_CONNECT_TIMEOUT_SECS}"
        )));
    }
    if let Some(keep_alive) = profile.keep_alive_secs {
        if !(MIN_KEEP_ALIVE_SECS..=MAX_KEEP_ALIVE_SECS).contains(&keep_alive) {
            return Err(AppError::BadRequest(format!(
                "profile.keep_alive_secs 必须在 {MIN_KEEP_ALIVE_SECS}..={MAX_KEEP_ALIVE_SECS}"
            )));
        }
    }
    if let Some(host_key) = &profile.trusted_host_key {
        validate_required_text(
            &host_key.algorithm,
            MAX_HOST_KEY_ALGORITHM_LEN,
            false,
            "profile.trusted_host_key.algorithm",
        )?;
        validate_required_text(
            &host_key.fingerprint,
            MAX_HOST_KEY_FINGERPRINT_LEN,
            false,
            "profile.trusted_host_key.fingerprint",
        )?;
    }
    validate_millis(profile.created_at_ms, "profile.created_at_ms")?;
    validate_millis(profile.updated_at_ms, "profile.updated_at_ms")
}

fn validate_prefixed_id(value: &str, prefix: &str, field: &str) -> AppResult<()> {
    if value.len() != prefix.len() + 32
        || !value.starts_with(prefix)
        || !value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::BadRequest(format!(
            "{field} 必须为 {prefix} 加 32 位小写十六进制"
        )));
    }
    Ok(())
}

fn validate_required_text(
    value: &str,
    max: usize,
    allow_whitespace: bool,
    field: &str,
) -> AppResult<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest(format!("{field} 不能为空")));
    }
    validate_optional_text(value, max, allow_whitespace, field)
}

fn validate_optional_text(
    value: &str,
    max: usize,
    allow_whitespace: bool,
    field: &str,
) -> AppResult<()> {
    if value.chars().any(char::is_control) {
        return Err(AppError::BadRequest(format!("{field} 不能包含控制字符")));
    }
    let trimmed = value.trim();
    if trimmed.chars().count() > max {
        return Err(AppError::BadRequest(format!("{field} 超过 {max} 字符")));
    }
    if !allow_whitespace && trimmed.chars().any(char::is_whitespace) {
        return Err(AppError::BadRequest(format!("{field} 不能包含空白字符")));
    }
    Ok(())
}

fn validate_millis(value: u64, field: &str) -> AppResult<()> {
    if value > i64::MAX as u64 {
        return Err(AppError::BadRequest(format!("{field} 超出范围")));
    }
    Ok(())
}

#[derive(Serialize)]
struct CanonicalMutation<'a> {
    schema_version: u32,
    base_revision: i64,
    operation: &'a SshMutationOperation,
}

/// 对无 map 的 schema v1 mutation 生成稳定 SHA-256。账号由复合主键绑定，
/// mutation_id 是查找键，摘要绑定其余会影响语义的正文。
fn mutation_request_digest(mutation: &SshMutation) -> AppResult<String> {
    let canonical = serde_json::to_vec(&CanonicalMutation {
        schema_version: SSH_SYNC_SCHEMA_VERSION,
        base_revision: mutation.base_revision,
        operation: &mutation.operation,
    })
    .map_err(|error| AppError::Internal(format!("SSH mutation 序列化失败: {error}")))?;
    let digest = Sha256::digest(canonical);
    let mut encoded = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn validate_duplicate_digest(stored: Option<&str>, current: &str) -> AppResult<()> {
    if stored != Some(current) {
        return Err(AppError::BadRequest(
            "mutation_id 已用于不同的 SSH 同步请求".into(),
        ));
    }
    Ok(())
}

async fn apply_mutation(
    tx: &Transaction<'_>,
    user: &AuthUser,
    mutation: &SshMutation,
    current_revision: &mut i64,
) -> AppResult<SshMutationAck> {
    let request_digest = mutation_request_digest(mutation)?;
    if let Some(row) = tx
        .query_opt(
            "SELECT status,revision,error_code,request_digest FROM ssh_mutations \
             WHERE user_id=$1 AND mutation_id=$2",
            &[&user.user_id, &mutation.mutation_id],
        )
        .await?
    {
        let stored_digest: Option<String> = row.get(3);
        // 开发版旧表迁移后历史行的摘要为 NULL，无法证明正文相同，安全拒绝。
        validate_duplicate_digest(stored_digest.as_deref(), &request_digest)?;
        return Ok(SshMutationAck {
            mutation_id: mutation.mutation_id.clone(),
            status: SshMutationStatus::Duplicate,
            revision: row.get(1),
            error_code: row.get(2),
        });
    }

    // schema v1 的冲突规则是服务端接收顺序 LWW。base_revision 即使落后于
    // current_revision 也不拒绝；它与最终 revision 一起留在 ssh_mutations
    // 供审计/冲突观测。只有永久墓碑可以阻止已删 ID 被陈旧客户端复活。
    if mutation.base_revision < *current_revision {
        tracing::debug!(
            user_id = %user.user_id,
            mutation_id = %mutation.mutation_id,
            base_revision = mutation.base_revision,
            current_revision = *current_revision,
            "应用陈旧 base_revision 的 SSH LWW mutation"
        );
    }
    let outcome = match &mutation.operation {
        SshMutationOperation::UpsertGroup { group } => {
            upsert_group(tx, user, group, current_revision).await?
        }
        SshMutationOperation::DeleteGroup { group_id } => {
            delete_group(tx, user, group_id, current_revision).await?
        }
        SshMutationOperation::UpsertProfile { profile } => {
            upsert_profile(tx, user, profile, current_revision).await?
        }
        SshMutationOperation::DeleteProfile { profile_id } => {
            delete_profile(tx, user, profile_id, current_revision).await?
        }
    };
    let status_text = outcome.status.as_db_str();
    tx.execute(
        "INSERT INTO ssh_mutations \
         (user_id,mutation_id,device_id,base_revision,request_digest,status,revision,error_code,created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        &[
            &user.user_id,
            &mutation.mutation_id,
            &user.device_id,
            &mutation.base_revision,
            &request_digest,
            &status_text,
            &outcome.revision,
            &outcome.error_code,
            &auth::now_secs(),
        ],
    )
    .await?;
    tx.execute(
        "INSERT INTO ssh_sync_audit \
         (user_id,device_id,mutation_id,entity_kind,entity_id,operation,result,revision,created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        &[
            &user.user_id,
            &user.device_id,
            &mutation.mutation_id,
            &outcome.entity_kind,
            &outcome.entity_id,
            &outcome.operation,
            &status_text,
            &outcome.revision,
            &auth::now_secs(),
        ],
    )
    .await?;

    Ok(SshMutationAck {
        mutation_id: mutation.mutation_id.clone(),
        status: outcome.status,
        revision: outcome.revision,
        error_code: outcome.error_code,
    })
}

struct MutationOutcome {
    status: SshMutationStatus,
    revision: Option<i64>,
    error_code: Option<String>,
    entity_kind: &'static str,
    entity_id: String,
    operation: &'static str,
}

impl MutationOutcome {
    fn applied(
        revision: i64,
        entity_kind: &'static str,
        entity_id: &str,
        operation: &'static str,
    ) -> Self {
        Self {
            status: SshMutationStatus::Applied,
            revision: Some(revision),
            error_code: None,
            entity_kind,
            entity_id: entity_id.to_string(),
            operation,
        }
    }

    fn rejected(
        code: &'static str,
        entity_kind: &'static str,
        entity_id: &str,
        operation: &'static str,
    ) -> Self {
        Self {
            status: SshMutationStatus::Rejected,
            revision: None,
            error_code: Some(code.to_string()),
            entity_kind,
            entity_id: entity_id.to_string(),
            operation,
        }
    }
}

trait MutationStatusDb {
    fn as_db_str(&self) -> &'static str;
}

impl MutationStatusDb for SshMutationStatus {
    fn as_db_str(&self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Duplicate => "duplicate",
            Self::Rejected => "rejected",
        }
    }
}

fn allocate_revision(current_revision: &mut i64) -> AppResult<i64> {
    *current_revision = current_revision
        .checked_add(1)
        .ok_or_else(|| AppError::Internal("SSH sync revision overflow".into()))?;
    Ok(*current_revision)
}

async fn has_tombstone(
    tx: &Transaction<'_>,
    user_id: &str,
    kind: &str,
    entity_id: &str,
) -> AppResult<bool> {
    Ok(tx
        .query_opt(
            "SELECT 1 FROM ssh_tombstones \
             WHERE user_id=$1 AND entity_kind=$2 AND entity_id=$3",
            &[&user_id, &kind, &entity_id],
        )
        .await?
        .is_some())
}

async fn entity_exists(
    tx: &Transaction<'_>,
    user_id: &str,
    kind: &str,
    entity_id: &str,
) -> AppResult<bool> {
    let query = match kind {
        "group" => "SELECT 1 FROM ssh_groups WHERE user_id=$1 AND id=$2",
        "profile" => "SELECT 1 FROM ssh_profiles WHERE user_id=$1 AND id=$2",
        _ => return Err(AppError::Internal("未知 SSH 实体类型".into())),
    };
    Ok(tx
        .query_opt(query, &[&user_id, &entity_id])
        .await?
        .is_some())
}

async fn active_quota_reached(tx: &Transaction<'_>, user_id: &str, kind: &str) -> AppResult<bool> {
    let (query, max) = match kind {
        "group" => (
            "SELECT 1 FROM ssh_groups WHERE user_id=$1 OFFSET $2 LIMIT 1",
            MAX_GROUPS,
        ),
        "profile" => (
            "SELECT 1 FROM ssh_profiles WHERE user_id=$1 OFFSET $2 LIMIT 1",
            MAX_PROFILES,
        ),
        _ => return Err(AppError::Internal("未知 SSH 实体类型".into())),
    };
    let offset =
        i64::try_from(max - 1).map_err(|_| AppError::Internal("SSH 配额超出数据库范围".into()))?;
    Ok(tx.query_opt(query, &[&user_id, &offset]).await?.is_some())
}

async fn retained_quota_reached(tx: &Transaction<'_>, user_id: &str) -> AppResult<bool> {
    let offset = i64::try_from(MAX_RETAINED_ENTITIES - 1)
        .map_err(|_| AppError::Internal("SSH 保留实体配额超出数据库范围".into()))?;
    Ok(tx
        .query_opt(
            "SELECT 1 FROM (\
                 SELECT 1 FROM ssh_groups WHERE user_id=$1 \
                 UNION ALL SELECT 1 FROM ssh_profiles WHERE user_id=$1 \
                 UNION ALL SELECT 1 FROM ssh_tombstones WHERE user_id=$1\
             ) AS retained_entities OFFSET $2 LIMIT 1",
            &[&user_id, &offset],
        )
        .await?
        .is_some())
}

async fn upsert_group(
    tx: &Transaction<'_>,
    user: &AuthUser,
    group: &SshGroup,
    current_revision: &mut i64,
) -> AppResult<MutationOutcome> {
    if has_tombstone(tx, &user.user_id, "group", &group.id).await? {
        return Ok(MutationOutcome::rejected(
            "deleted_entity",
            "group",
            &group.id,
            "upsert",
        ));
    }
    let exists = entity_exists(tx, &user.user_id, "group", &group.id).await?;
    if !exists && active_quota_reached(tx, &user.user_id, "group").await? {
        return Ok(MutationOutcome::rejected(
            "group_quota_exceeded",
            "group",
            &group.id,
            "upsert",
        ));
    }
    if !exists && retained_quota_reached(tx, &user.user_id).await? {
        return Ok(MutationOutcome::rejected(
            "entity_retention_quota_exceeded",
            "group",
            &group.id,
            "upsert",
        ));
    }
    if tx
        .query_opt(
            "SELECT id FROM ssh_groups \
             WHERE user_id=$1 AND lower(name)=lower($2) AND id<>$3",
            &[&user.user_id, &group.name.trim(), &group.id],
        )
        .await?
        .is_some()
    {
        return Ok(MutationOutcome::rejected(
            "group_name_conflict",
            "group",
            &group.id,
            "upsert",
        ));
    }
    let revision = allocate_revision(current_revision)?;
    let sort_order = i64::from(group.sort_order);
    let created_at_ms = group.created_at_ms as i64;
    let updated_at_ms = group.updated_at_ms as i64;
    let name = group.name.trim();
    tx.execute(
        "INSERT INTO ssh_groups \
         (user_id,id,name,sort_order,created_at_ms,updated_at_ms,revision,updated_by_device) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
         ON CONFLICT (user_id,id) DO UPDATE SET \
         name=EXCLUDED.name,sort_order=EXCLUDED.sort_order,created_at_ms=EXCLUDED.created_at_ms,\
         updated_at_ms=EXCLUDED.updated_at_ms,revision=EXCLUDED.revision,\
         updated_by_device=EXCLUDED.updated_by_device",
        &[
            &user.user_id,
            &group.id,
            &name,
            &sort_order,
            &created_at_ms,
            &updated_at_ms,
            &revision,
            &user.device_id,
        ],
    )
    .await?;
    Ok(MutationOutcome::applied(
        revision, "group", &group.id, "upsert",
    ))
}

async fn delete_group(
    tx: &Transaction<'_>,
    user: &AuthUser,
    group_id: &str,
    current_revision: &mut i64,
) -> AppResult<MutationOutcome> {
    if has_tombstone(tx, &user.user_id, "group", group_id).await? {
        return Ok(MutationOutcome::rejected(
            "deleted_entity",
            "group",
            group_id,
            "delete",
        ));
    }
    let group_exists = entity_exists(tx, &user.user_id, "group", group_id).await?;
    if !group_exists && retained_quota_reached(tx, &user.user_id).await? {
        return Ok(MutationOutcome::rejected(
            "entity_retention_quota_exceeded",
            "group",
            group_id,
            "delete",
        ));
    }

    // 已有未分组项排在前面，目标组内项按原顺序稳定追加。LIMIT 让异常旧库
    // 即使已超过当前配额也不会把整表读入内存；正常账号最多返回 MAX_PROFILES。
    let profile_limit = i64::try_from(MAX_PROFILES + 1)
        .map_err(|_| AppError::Internal("SSH 服务器配额超出数据库范围".into()))?;
    let profile_rows = tx
        .query(
            "SELECT id,group_id,sort_order FROM ssh_profiles \
             WHERE user_id=$1 AND (group_id IS NULL OR group_id=$2) \
             ORDER BY CASE WHEN group_id IS NULL THEN 0 ELSE 1 END,sort_order,id \
             LIMIT $3",
            &[&user.user_id, &group_id, &profile_limit],
        )
        .await?;
    if profile_rows.len() > MAX_PROFILES {
        return Ok(MutationOutcome::rejected(
            "profile_quota_exceeded",
            "group",
            group_id,
            "delete",
        ));
    }

    let revision = allocate_revision(current_revision)?;
    tx.execute(
        "DELETE FROM ssh_groups WHERE user_id=$1 AND id=$2",
        &[&user.user_id, &group_id],
    )
    .await?;
    write_tombstone(tx, user, "group", group_id, revision, auth::now_secs()).await?;

    // 统一压缩到 0..N-1，既稳定追加，也不会被恶意 u32::MAX sort_order 推到
    // wire 范围外。每个实际变化的 profile 都分配独立 revision，确保可分页同步。
    for (index, row) in profile_rows.into_iter().enumerate() {
        let profile_id: String = row.get(0);
        let old_group_id: Option<String> = row.get(1);
        let old_sort_order: i64 = row.get(2);
        let new_sort_order = i64::try_from(index)
            .map_err(|_| AppError::Internal("SSH 服务器排序超出数据库范围".into()))?;
        if old_group_id.is_none() && old_sort_order == new_sort_order {
            continue;
        }
        let profile_revision = allocate_revision(current_revision)?;
        tx.execute(
            "UPDATE ssh_profiles SET group_id=NULL,sort_order=$1,revision=$2,\
             updated_by_device=$3 WHERE user_id=$4 AND id=$5",
            &[
                &new_sort_order,
                &profile_revision,
                &user.device_id,
                &user.user_id,
                &profile_id,
            ],
        )
        .await?;
    }
    Ok(MutationOutcome::applied(
        revision, "group", group_id, "delete",
    ))
}

async fn upsert_profile(
    tx: &Transaction<'_>,
    user: &AuthUser,
    profile: &SshProfile,
    current_revision: &mut i64,
) -> AppResult<MutationOutcome> {
    if has_tombstone(tx, &user.user_id, "profile", &profile.id).await? {
        return Ok(MutationOutcome::rejected(
            "deleted_entity",
            "profile",
            &profile.id,
            "upsert",
        ));
    }
    let exists = entity_exists(tx, &user.user_id, "profile", &profile.id).await?;
    if !exists && active_quota_reached(tx, &user.user_id, "profile").await? {
        return Ok(MutationOutcome::rejected(
            "profile_quota_exceeded",
            "profile",
            &profile.id,
            "upsert",
        ));
    }
    if !exists && retained_quota_reached(tx, &user.user_id).await? {
        return Ok(MutationOutcome::rejected(
            "entity_retention_quota_exceeded",
            "profile",
            &profile.id,
            "upsert",
        ));
    }

    // 非法、别的账号或已经删除的 group 一律规范化为未分组，不恢复旧组。
    let group_id = if let Some(group_id) = &profile.group_id {
        tx.query_opt(
            "SELECT 1 FROM ssh_groups WHERE user_id=$1 AND id=$2",
            &[&user.user_id, group_id],
        )
        .await?
        .map(|_| group_id.clone())
    } else {
        None
    };
    let revision = allocate_revision(current_revision)?;
    let auth_method = auth_method_to_db(profile.auth_method);
    let sort_order = i64::from(profile.sort_order);
    let timeout = i64::from(profile.connect_timeout_secs);
    let keep_alive = profile.keep_alive_secs.map(i64::from);
    let created_at_ms = profile.created_at_ms as i64;
    let updated_at_ms = profile.updated_at_ms as i64;
    let initial_directory = profile
        .initial_directory
        .as_deref()
        .map(str::trim)
        .filter(|directory| !directory.is_empty());
    let (host_key_algorithm, host_key_fingerprint) =
        profile
            .trusted_host_key
            .as_ref()
            .map_or((None, None), |key| {
                (
                    Some(key.algorithm.trim().to_string()),
                    Some(key.fingerprint.trim().to_string()),
                )
            });
    let name = profile.name.trim();
    let host = profile.host.trim();
    let username = profile.username.trim();
    tx.execute(
        "INSERT INTO ssh_profiles \
         (user_id,id,name,host,port,username,auth_method,group_id,sort_order,initial_directory,\
          connect_timeout_secs,keep_alive_secs,monitor_enabled,host_key_algorithm,\
          host_key_fingerprint,created_at_ms,updated_at_ms,revision,updated_by_device) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19) \
         ON CONFLICT (user_id,id) DO UPDATE SET \
         name=EXCLUDED.name,host=EXCLUDED.host,port=EXCLUDED.port,username=EXCLUDED.username,\
         auth_method=EXCLUDED.auth_method,group_id=EXCLUDED.group_id,\
         sort_order=EXCLUDED.sort_order,initial_directory=EXCLUDED.initial_directory,\
         connect_timeout_secs=EXCLUDED.connect_timeout_secs,\
         keep_alive_secs=EXCLUDED.keep_alive_secs,monitor_enabled=EXCLUDED.monitor_enabled,\
         host_key_algorithm=EXCLUDED.host_key_algorithm,\
         host_key_fingerprint=EXCLUDED.host_key_fingerprint,\
         created_at_ms=EXCLUDED.created_at_ms,updated_at_ms=EXCLUDED.updated_at_ms,\
         revision=EXCLUDED.revision,updated_by_device=EXCLUDED.updated_by_device",
        &[
            &user.user_id,
            &profile.id,
            &name,
            &host,
            &i32::from(profile.port),
            &username,
            &auth_method,
            &group_id,
            &sort_order,
            &initial_directory,
            &timeout,
            &keep_alive,
            &profile.monitor_enabled,
            &host_key_algorithm,
            &host_key_fingerprint,
            &created_at_ms,
            &updated_at_ms,
            &revision,
            &user.device_id,
        ],
    )
    .await?;
    Ok(MutationOutcome::applied(
        revision,
        "profile",
        &profile.id,
        "upsert",
    ))
}

async fn delete_profile(
    tx: &Transaction<'_>,
    user: &AuthUser,
    profile_id: &str,
    current_revision: &mut i64,
) -> AppResult<MutationOutcome> {
    if has_tombstone(tx, &user.user_id, "profile", profile_id).await? {
        return Ok(MutationOutcome::rejected(
            "deleted_entity",
            "profile",
            profile_id,
            "delete",
        ));
    }
    let profile_exists = entity_exists(tx, &user.user_id, "profile", profile_id).await?;
    if !profile_exists && retained_quota_reached(tx, &user.user_id).await? {
        return Ok(MutationOutcome::rejected(
            "entity_retention_quota_exceeded",
            "profile",
            profile_id,
            "delete",
        ));
    }
    let revision = allocate_revision(current_revision)?;
    tx.execute(
        "DELETE FROM ssh_profiles WHERE user_id=$1 AND id=$2",
        &[&user.user_id, &profile_id],
    )
    .await?;
    write_tombstone(tx, user, "profile", profile_id, revision, auth::now_secs()).await?;
    Ok(MutationOutcome::applied(
        revision, "profile", profile_id, "delete",
    ))
}

async fn write_tombstone(
    tx: &Transaction<'_>,
    user: &AuthUser,
    kind: &str,
    entity_id: &str,
    revision: i64,
    deleted_at: i64,
) -> AppResult<()> {
    tx.execute(
        "INSERT INTO ssh_tombstones \
         (user_id,entity_kind,entity_id,revision,deleted_by_device,deleted_at) \
         VALUES ($1,$2,$3,$4,$5,$6) \
         ON CONFLICT (user_id,entity_kind,entity_id) DO UPDATE SET \
         revision=EXCLUDED.revision,deleted_by_device=EXCLUDED.deleted_by_device,\
         deleted_at=EXCLUDED.deleted_at",
        &[
            &user.user_id,
            &kind,
            &entity_id,
            &revision,
            &user.device_id,
            &deleted_at,
        ],
    )
    .await?;
    Ok(())
}

async fn pull_changes(
    tx: &Transaction<'_>,
    user_id: &str,
    cursor: i64,
    limit: usize,
) -> AppResult<(Vec<SshSyncChange>, i64, bool)> {
    let query_limit = i64::try_from(limit.saturating_add(1))
        .map_err(|_| AppError::Internal("SSH 拉取上限超出数据库范围".into()))?;
    let mut changes = Vec::new();
    for row in tx
        .query(
            "SELECT id,name,sort_order,created_at_ms,updated_at_ms,revision,updated_by_device \
             FROM ssh_groups WHERE user_id=$1 AND revision>$2 \
             ORDER BY revision,id LIMIT $3",
            &[&user_id, &cursor, &query_limit],
        )
        .await?
    {
        changes.push(group_change(&row)?);
    }
    for row in tx
        .query(
            "SELECT id,name,host,port,username,auth_method,group_id,sort_order,initial_directory,\
             connect_timeout_secs,keep_alive_secs,monitor_enabled,host_key_algorithm,\
             host_key_fingerprint,created_at_ms,updated_at_ms,revision,updated_by_device \
             FROM ssh_profiles WHERE user_id=$1 AND revision>$2 \
             ORDER BY revision,id LIMIT $3",
            &[&user_id, &cursor, &query_limit],
        )
        .await?
    {
        changes.push(profile_change(&row)?);
    }
    for row in tx
        .query(
            "SELECT entity_kind,entity_id,revision,deleted_by_device \
             FROM ssh_tombstones WHERE user_id=$1 AND revision>$2 \
             ORDER BY revision,entity_kind,entity_id LIMIT $3",
            &[&user_id, &cursor, &query_limit],
        )
        .await?
    {
        changes.push(tombstone_change(&row)?);
    }
    Ok(finalize_changes(changes, cursor, limit))
}

fn finalize_changes(
    mut changes: Vec<SshSyncChange>,
    cursor: i64,
    limit: usize,
) -> (Vec<SshSyncChange>, i64, bool) {
    changes.sort_by_key(|change| change.revision);
    let has_more = changes.len() > limit;
    changes.truncate(limit);
    let next_cursor = changes.last().map_or(cursor, |change| change.revision);
    (changes, next_cursor, has_more)
}

fn group_change(row: &Row) -> AppResult<SshSyncChange> {
    Ok(SshSyncChange {
        revision: row.get(5),
        updated_by_device: row.get(6),
        change: SshChange::UpsertGroup {
            group: SshGroup {
                id: row.get(0),
                name: row.get(1),
                sort_order: db_u32(row.get(2), "ssh_groups.sort_order")?,
                created_at_ms: db_u64(row.get(3), "ssh_groups.created_at_ms")?,
                updated_at_ms: db_u64(row.get(4), "ssh_groups.updated_at_ms")?,
            },
        },
    })
}

fn profile_change(row: &Row) -> AppResult<SshSyncChange> {
    let host_key_algorithm: Option<String> = row.get(12);
    let host_key_fingerprint: Option<String> = row.get(13);
    let trusted_host_key = match (host_key_algorithm, host_key_fingerprint) {
        (Some(algorithm), Some(fingerprint)) => Some(SshHostKeyTrust {
            algorithm,
            fingerprint,
        }),
        _ => None,
    };
    Ok(SshSyncChange {
        revision: row.get(16),
        updated_by_device: row.get(17),
        change: SshChange::UpsertProfile {
            profile: SshProfile {
                id: row.get(0),
                name: row.get(1),
                host: row.get(2),
                port: u16::try_from(row.get::<_, i32>(3))
                    .map_err(|_| AppError::Internal("数据库中的 SSH 端口无效".into()))?,
                username: row.get(4),
                auth_method: auth_method_from_db(&row.get::<_, String>(5))?,
                group_id: row.get(6),
                sort_order: db_u32(row.get(7), "ssh_profiles.sort_order")?,
                initial_directory: row.get(8),
                connect_timeout_secs: db_u32(row.get(9), "ssh_profiles.connect_timeout_secs")?,
                keep_alive_secs: row
                    .get::<_, Option<i64>>(10)
                    .map(|value| db_u32(value, "ssh_profiles.keep_alive_secs"))
                    .transpose()?,
                monitor_enabled: row.get(11),
                trusted_host_key,
                created_at_ms: db_u64(row.get(14), "ssh_profiles.created_at_ms")?,
                updated_at_ms: db_u64(row.get(15), "ssh_profiles.updated_at_ms")?,
            },
        },
    })
}

fn tombstone_change(row: &Row) -> AppResult<SshSyncChange> {
    let kind: String = row.get(0);
    let id: String = row.get(1);
    let change = match kind.as_str() {
        "group" => SshChange::DeleteGroup { group_id: id },
        "profile" => SshChange::DeleteProfile { profile_id: id },
        _ => return Err(AppError::Internal("数据库中的 SSH 墓碑类型无效".into())),
    };
    Ok(SshSyncChange {
        revision: row.get(2),
        updated_by_device: row.get(3),
        change,
    })
}

fn auth_method_to_db(method: SshAuthMethod) -> &'static str {
    match method {
        SshAuthMethod::Password => "password",
        SshAuthMethod::PrivateKey => "private_key",
        SshAuthMethod::Agent => "agent",
    }
}

fn auth_method_from_db(value: &str) -> AppResult<SshAuthMethod> {
    match value {
        "password" => Ok(SshAuthMethod::Password),
        "private_key" => Ok(SshAuthMethod::PrivateKey),
        "agent" => Ok(SshAuthMethod::Agent),
        _ => Err(AppError::Internal("数据库中的 SSH 认证方式无效".into())),
    }
}

fn db_u32(value: i64, field: &str) -> AppResult<u32> {
    u32::try_from(value)
        .map_err(|_| AppError::Internal(format!("数据库中的 {field} 超出 u32 范围")))
}

fn db_u64(value: i64, field: &str) -> AppResult<u64> {
    u64::try_from(value).map_err(|_| AppError::Internal(format!("数据库中的 {field} 不能为负数")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::config::Config;
    use crate::hub::Hub;
    use axum::body::Body;
    use axum::extract::{FromRequest, FromRequestParts};
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use deadpool_postgres::Pool;

    fn id(prefix: &str, value: u128) -> String {
        format!("{prefix}{value:032x}")
    }

    fn group_id(value: u128) -> String {
        id(SSH_GROUP_ID_PREFIX, value)
    }

    fn profile_id(value: u128) -> String {
        id(SSH_PROFILE_ID_PREFIX, value)
    }

    fn mutation_id(value: u128) -> String {
        id(SSH_MUTATION_ID_PREFIX, value)
    }

    fn profile() -> SshProfile {
        SshProfile {
            id: profile_id(1),
            name: "Production".into(),
            host: "example.test".into(),
            port: 22,
            username: "alice".into(),
            auth_method: SshAuthMethod::PrivateKey,
            group_id: Some(group_id(1)),
            sort_order: 0,
            initial_directory: Some("/srv/app".into()),
            connect_timeout_secs: 15,
            keep_alive_secs: Some(30),
            monitor_enabled: true,
            trusted_host_key: Some(SshHostKeyTrust {
                algorithm: "ssh-ed25519".into(),
                fingerprint: "SHA256:abc".into(),
            }),
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn request(operation: SshMutationOperation) -> SshSyncRequest {
        SshSyncRequest {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            cursor: 0,
            limit: 200,
            mutations: vec![SshMutation {
                mutation_id: mutation_id(1),
                base_revision: 0,
                operation,
            }],
        }
    }

    #[test]
    fn 合法profile通过纯校验() {
        validate_request(&request(SshMutationOperation::UpsertProfile {
            profile: profile(),
        }))
        .expect("合法");
    }

    #[test]
    fn 请求批量和游标边界被拒绝() {
        let mut req = request(SshMutationOperation::DeleteProfile {
            profile_id: profile_id(1),
        });
        req.limit = 0;
        assert!(validate_request(&req).is_err());
        req.limit = 201;
        assert!(validate_request(&req).is_err());
        req.limit = 200;
        req.cursor = -1;
        assert!(validate_request(&req).is_err());
        req.cursor = 0;
        req.mutations = (0..=MAX_BATCH)
            .map(|i| SshMutation {
                mutation_id: mutation_id(i as u128),
                base_revision: 0,
                operation: SshMutationOperation::DeleteProfile {
                    profile_id: profile_id(i as u128),
                },
            })
            .collect();
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn 字段长度端口超时和keepalive被拒绝() {
        let mut value = profile();
        value.port = 0;
        assert!(validate_profile(&value).is_err());
        value.port = 22;
        value.connect_timeout_secs = MAX_CONNECT_TIMEOUT_SECS + 1;
        assert!(validate_profile(&value).is_err());
        value.connect_timeout_secs = 15;
        value.keep_alive_secs = Some(MIN_KEEP_ALIVE_SECS - 1);
        assert!(validate_profile(&value).is_err());
        value.keep_alive_secs = Some(30);
        value.host = "h".repeat(MAX_HOST_LEN + 1);
        assert!(validate_profile(&value).is_err());

        value.host = "example.test".into();
        value.initial_directory = Some("a".repeat(MAX_INITIAL_DIRECTORY_LEN));
        validate_profile(&value).expect("客户端允许的 4096 字符初始目录必须可同步");
    }

    #[test]
    fn 分组名限制按字符而非字节() {
        let valid = SshGroup {
            id: group_id(1),
            name: "生产".repeat(MAX_GROUP_NAME_LEN / 2),
            sort_order: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        validate_group(&valid).expect("中文名按字符计数");
        let mut invalid = valid;
        invalid.name = "组".repeat(MAX_GROUP_NAME_LEN + 1);
        assert!(validate_group(&invalid).is_err());
    }

    #[test]
    fn id规则和所有文本控制字符被拒绝() {
        assert!(validate_prefixed_id(&group_id(1), SSH_GROUP_ID_PREFIX, "group").is_ok());
        assert!(validate_prefixed_id(
            "grp_ABCDEF00000000000000000000000000",
            SSH_GROUP_ID_PREFIX,
            "group"
        )
        .is_err());
        assert!(validate_prefixed_id("group-1", SSH_GROUP_ID_PREFIX, "group").is_err());

        let invalid_group = SshGroup {
            id: group_id(2),
            name: "prod\n".into(),
            sort_order: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        assert!(validate_group(&invalid_group).is_err());

        let mut value = profile();
        value.name = "prod\0server".into();
        assert!(validate_profile(&value).is_err());
        value = profile();
        value.host = "example.\ntest".into();
        assert!(validate_profile(&value).is_err());
        value = profile();
        value.username = "root\tadmin".into();
        assert!(validate_profile(&value).is_err());
        value = profile();
        value.initial_directory = Some("/srv/\0app".into());
        assert!(validate_profile(&value).is_err());
        value = profile();
        value.trusted_host_key = Some(SshHostKeyTrust {
            algorithm: "ssh-\0ed25519".into(),
            fingerprint: "SHA256:abc".into(),
        });
        assert!(validate_profile(&value).is_err());
    }

    #[test]
    fn base_revision不参与lww拒绝() {
        let mut req = request(SshMutationOperation::UpsertProfile { profile: profile() });
        req.cursor = 9;
        req.mutations[0].base_revision = 1;
        validate_request(&req).expect("陈旧 base_revision 仅用于冲突观测，不应被拒绝");

        req.mutations[0].base_revision = 10;
        validate_request(&req).expect("未来 base_revision 也只记录审计，不作为并发写条件");

        req.mutations[0].base_revision = -1;
        assert!(validate_request(&req).is_err());
    }

    #[test]
    fn mutation摘要稳定且绑定base与操作正文() {
        let first = request(SshMutationOperation::UpsertProfile { profile: profile() })
            .mutations
            .remove(0);
        let mut same_payload_new_id = first.clone();
        same_payload_new_id.mutation_id = mutation_id(2);
        assert_eq!(
            mutation_request_digest(&first).expect("摘要"),
            mutation_request_digest(&same_payload_new_id).expect("摘要")
        );

        let mut changed_base = first.clone();
        changed_base.base_revision = 1;
        assert_ne!(
            mutation_request_digest(&first).expect("摘要"),
            mutation_request_digest(&changed_base).expect("摘要")
        );

        let changed_operation = SshMutation {
            operation: SshMutationOperation::DeleteProfile {
                profile_id: profile_id(1),
            },
            ..first.clone()
        };
        assert_ne!(
            mutation_request_digest(&first).expect("摘要"),
            mutation_request_digest(&changed_operation).expect("摘要")
        );

        let digest = mutation_request_digest(&first).expect("摘要");
        validate_duplicate_digest(Some(&digest), &digest).expect("同正文幂等重试");
        assert!(validate_duplicate_digest(Some("different"), &digest).is_err());
        assert!(
            validate_duplicate_digest(None, &digest).is_err(),
            "旧表迁移产生的 NULL 摘要无法证明请求相同，必须安全拒绝"
        );
    }

    fn deleted_group_change(revision: i64, id_value: u128) -> SshSyncChange {
        SshSyncChange {
            revision,
            updated_by_device: "00000000-0000-0000-0000-000000000001".into(),
            change: SshChange::DeleteGroup {
                group_id: group_id(id_value),
            },
        }
    }

    #[test]
    fn 三类limit加一结果全局排序截断不漏最早变更() {
        // 模拟每一类 SQL 都只返回各自最早的 limit+1（这里 limit=3）。
        // 任一来源在第 4 条之后的 revision 都不可能进入全局最早 3 条。
        let mut bounded_sources = [1, 10, 20, 30]
            .into_iter()
            .chain([2, 3, 40, 50])
            .chain([4, 5, 6, 7])
            .enumerate()
            .map(|(index, revision)| deleted_group_change(revision, index as u128 + 1))
            .collect::<Vec<_>>();
        bounded_sources.reverse();

        let (changes, next_cursor, has_more) = finalize_changes(bounded_sources, 0, 3);
        assert_eq!(
            changes
                .iter()
                .map(|change| change.revision)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(next_cursor, 3);
        assert!(has_more);
    }

    #[test]
    fn revision严格递增且防溢出() {
        let mut revision = 4;
        assert_eq!(allocate_revision(&mut revision).expect("分配"), 5);
        assert_eq!(revision, 5);
        revision = i64::MAX;
        assert!(allocate_revision(&mut revision).is_err());
    }

    #[test]
    fn 数据库无符号字段越界不会静默归零() {
        assert_eq!(db_u32(7, "test").expect("范围内"), 7);
        assert!(db_u32(-1, "test").is_err());
        assert!(db_u32(i64::from(u32::MAX) + 1, "test").is_err());
        assert!(db_u64(-1, "test").is_err());
    }

    #[tokio::test]
    async fn 未知秘密字段由json提取器返回422且不进入handler() {
        let request = Request::builder()
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{
                    "schema_version":1,"cursor":0,"limit":200,"mutations":[{
                        "mutation_id":"m1","base_revision":0,
                        "operation":{"type":"delete_profile","profile_id":"p1"},
                        "password":"must-not-enter-handler"
                    }]
                }"#,
            ))
            .expect("请求");
        let rejection = Json::<SshSyncRequest>::from_request(request, &())
            .await
            .expect_err("未知字段必须被提取器拒绝");
        assert_eq!(
            rejection.into_response().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    fn test_db_url() -> String {
        std::env::var("LUMEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://lumen_user:lumen_password@127.0.0.1:5544/lumen?sslmode=disable".to_string()
        })
    }

    async fn setup_db() -> (AppState, AuthUser) {
        let pool = crate::db::create_pool(&test_db_url()).expect("建连接池");
        static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        SCHEMA
            .get_or_init(|| async {
                crate::db::init_schema(&pool).await.expect("建表");
            })
            .await;
        let user_id = uuid::Uuid::new_v4().to_string();
        let device_id = uuid::Uuid::new_v4().to_string();
        let client = pool.get().await.expect("取连接");
        client
            .execute(
                "INSERT INTO users (id,email,display_name,password_hash,created_at) \
                 VALUES ($1,$2,$3,$4,$5)",
                &[
                    &user_id,
                    &format!("{user_id}@ssh-sync.test"),
                    &"test",
                    &"unused",
                    &auth::now_secs(),
                ],
            )
            .await
            .expect("建临时用户");
        client
            .execute(
                "INSERT INTO devices \
                 (id,user_id,name,os,app_version,last_seen,created_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$6)",
                &[
                    &device_id,
                    &user_id,
                    &"test-device",
                    &"windows",
                    &"test",
                    &auth::now_secs(),
                ],
            )
            .await
            .expect("建临时设备");
        let config = Arc::new(Config {
            database_url: test_db_url(),
            bind_addr: "127.0.0.1:0".into(),
            jwt_secret: "test".into(),
            token_ttl_secs: 60,
            online_window_secs: 45,
            stun_bind_addr: "127.0.0.1:0".into(),
        });
        (
            AppState {
                pool,
                config,
                hub: Arc::new(Hub::new()),
                throttle: Arc::new(crate::throttle::Throttle::new()),
            },
            AuthUser { user_id, device_id },
        )
    }

    async fn teardown_db(pool: &Pool, user_id: &str) {
        if let Ok(client) = pool.get().await {
            let _ = client
                .execute("DELETE FROM users WHERE id=$1", &[&user_id])
                .await;
        }
    }

    #[tokio::test]
    #[ignore = "需要可达的 Postgres"]
    async fn 删除设备立即吊销旧jwt和续期鉴权() {
        let (state, user) = setup_db().await;
        let (token, _) =
            auth::issue_token(&state.config.jwt_secret, &user.user_id, &user.device_id, 60)
                .expect("签发 token");

        let request = Request::builder()
            .header("authorization", format!("Bearer {token}"))
            .body(())
            .expect("请求");
        let (mut parts, _) = request.into_parts();
        AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("设备存在时鉴权成功");

        let client = state.pool.get().await.expect("取连接");
        client
            .execute(
                "DELETE FROM devices WHERE id=$1 AND user_id=$2",
                &[&user.device_id, &user.user_id],
            )
            .await
            .expect("删除设备");
        let request = Request::builder()
            .header("authorization", format!("Bearer {token}"))
            .body(())
            .expect("请求");
        let (mut parts, _) = request.into_parts();
        let err = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect_err("已删除设备的旧 JWT 必须立即失效");
        assert!(matches!(err, AppError::Unauthorized));

        let sync_request = SshSyncRequest {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            cursor: 0,
            limit: 1,
            mutations: Vec::new(),
        };
        let err = sync_ssh(State(state.clone()), user.clone(), Json(sync_request))
            .await
            .expect_err("SSH handler 的事务内复核也必须拒绝已删设备");
        assert!(matches!(err, AppError::Unauthorized));
        teardown_db(&state.pool, &user.user_id).await;
    }

    #[tokio::test]
    #[ignore = "需要可达的 Postgres"]
    async fn db事务_幂等墓碑删组归未分组与组名唯一() {
        let (state, user) = setup_db().await;
        let group = SshGroup {
            id: group_id(1),
            name: "Production".into(),
            sort_order: 0,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let deleted_group_id = group.id.clone();
        let mut server = profile();
        server.group_id = Some(group.id.clone());
        server.sort_order = u32::MAX;
        let server_id = server.id.clone();
        let mut ungrouped = profile();
        ungrouped.id = profile_id(2);
        ungrouped.group_id = None;
        ungrouped.sort_order = u32::MAX;
        let request = SshSyncRequest {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            cursor: 0,
            limit: 200,
            mutations: vec![
                SshMutation {
                    mutation_id: mutation_id(1),
                    base_revision: 0,
                    operation: SshMutationOperation::UpsertGroup {
                        group: group.clone(),
                    },
                },
                SshMutation {
                    mutation_id: mutation_id(2),
                    base_revision: 0,
                    operation: SshMutationOperation::UpsertGroup {
                        group: SshGroup {
                            id: group_id(2),
                            name: "production".into(),
                            ..group.clone()
                        },
                    },
                },
                SshMutation {
                    mutation_id: mutation_id(3),
                    base_revision: 0,
                    operation: SshMutationOperation::UpsertProfile { profile: ungrouped },
                },
                SshMutation {
                    mutation_id: mutation_id(4),
                    base_revision: 0,
                    operation: SshMutationOperation::UpsertProfile { profile: server },
                },
                SshMutation {
                    mutation_id: mutation_id(5),
                    base_revision: 0,
                    operation: SshMutationOperation::DeleteGroup {
                        group_id: group.id.clone(),
                    },
                },
                SshMutation {
                    mutation_id: mutation_id(6),
                    base_revision: 0,
                    operation: SshMutationOperation::UpsertGroup {
                        group: group.clone(),
                    },
                },
            ],
        };
        let first = sync_ssh(State(state.clone()), user.clone(), Json(request.clone()))
            .await
            .expect("首次同步")
            .0;
        assert_eq!(first.acks[0].status, SshMutationStatus::Applied);
        assert_eq!(
            first.acks[1].error_code.as_deref(),
            Some("group_name_conflict")
        );
        assert_eq!(first.acks[2].status, SshMutationStatus::Applied);
        assert_eq!(first.acks[3].status, SshMutationStatus::Applied);
        assert_eq!(first.acks[4].status, SshMutationStatus::Applied);
        assert_eq!(first.acks[5].error_code.as_deref(), Some("deleted_entity"));
        assert!(first.changes.iter().any(|change| matches!(
            &change.change,
            SshChange::DeleteGroup { group_id } if group_id == &deleted_group_id
        )));
        assert!(first.changes.iter().any(|change| matches!(
            &change.change,
            SshChange::UpsertProfile { profile } if profile.group_id.is_none()
        )));
        assert!(
            first
                .changes
                .windows(2)
                .all(|pair| pair[0].revision < pair[1].revision),
            "响应必须按账号 revision 严格升序"
        );
        let group_tombstone_revision = first
            .changes
            .iter()
            .find_map(|change| match &change.change {
                SshChange::DeleteGroup { group_id } if group_id == &deleted_group_id => {
                    Some(change.revision)
                }
                _ => None,
            })
            .expect("组墓碑 revision");
        assert!(
            first.changes.iter().all(|change| {
                !matches!(
                    &change.change,
                    SshChange::UpsertProfile { profile } if profile.group_id.is_none()
                ) || change.revision > group_tombstone_revision
            }),
            "墓碑应先分配 revision，随后每个搬迁 profile 独立分配更大 revision"
        );
        let mut ungrouped_orders = first
            .changes
            .iter()
            .filter_map(|change| match &change.change {
                SshChange::UpsertProfile { profile } if profile.group_id.is_none() => {
                    Some(profile.sort_order)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        ungrouped_orders.sort_unstable();
        assert_eq!(ungrouped_orders, vec![0, 1]);

        // 同一现存记录的陈旧 base_revision 按接收顺序 LWW，仍然应用。
        let mut updated_profile = profile();
        updated_profile.id = server_id;
        updated_profile.group_id = None;
        updated_profile.name = "Production LWW".into();
        let lww_request = SshSyncRequest {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            cursor: first.next_cursor,
            limit: 200,
            mutations: vec![SshMutation {
                mutation_id: mutation_id(7),
                base_revision: 0,
                operation: SshMutationOperation::UpsertProfile {
                    profile: updated_profile,
                },
            }],
        };
        let lww = sync_ssh(
            State(state.clone()),
            user.clone(),
            Json(lww_request.clone()),
        )
        .await
        .expect("陈旧 base 的 LWW 更新")
        .0;
        assert_eq!(lww.acks[0].status, SshMutationStatus::Applied);

        let retry = sync_ssh(State(state.clone()), user.clone(), Json(request.clone()))
            .await
            .expect("幂等重试")
            .0;
        assert!(retry
            .acks
            .iter()
            .all(|ack| ack.status == SshMutationStatus::Duplicate));

        // checkpoint 只确认客户端在请求前已经应用的 cursor，不抢先确认响应。
        let client = state.pool.get().await.expect("取连接");
        let checkpoint: i64 = client
            .query_one(
                "SELECT cursor FROM ssh_sync_checkpoints WHERE user_id=$1 AND device_id=$2",
                &[&user.user_id, &user.device_id],
            )
            .await
            .expect("查 checkpoint")
            .get(0);
        assert_eq!(checkpoint, lww_request.cursor);

        let mut future_cursor = request.clone();
        future_cursor.cursor = lww.next_cursor + 1;
        let err = sync_ssh(State(state.clone()), user.clone(), Json(future_cursor))
            .await
            .expect_err("未来 cursor 必须在应用 mutation 前拒绝");
        assert!(matches!(err, AppError::BadRequest(_)));

        let digest_reuse = SshSyncRequest {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            cursor: 0,
            limit: 200,
            mutations: vec![SshMutation {
                mutation_id: mutation_id(1),
                base_revision: 0,
                operation: SshMutationOperation::DeleteGroup {
                    group_id: deleted_group_id,
                },
            }],
        };
        let err = sync_ssh(State(state.clone()), user.clone(), Json(digest_reuse))
            .await
            .expect_err("同 mutation_id 的不同正文必须拒绝");
        assert!(matches!(err, AppError::BadRequest(_)));
        teardown_db(&state.pool, &user.user_id).await;
    }
}
