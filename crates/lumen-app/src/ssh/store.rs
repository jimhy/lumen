use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use rand_core::{OsRng, RngCore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use lumen_protocol::ssh_sync::{
    SshChange, SshMutation, SshMutationOperation, SshMutationStatus, SshSyncChange,
    SshSyncResponse, SSH_SYNC_SCHEMA_VERSION,
};

use super::credentials::{CredentialReference, CredentialSlot};
use super::inventory::{InventoryError, SshInventory};
use super::local::{SshLocalBinding, StorageScope};
use super::model::{new_id, AuthMethod, GroupId, NewSshProfile, ProfileId};
use super::sync::{
    change_entity, local_group, local_profile, mutation_entity, wire_group, wire_profile,
    SshSyncCompleted, SshSyncSnapshot, SyncJournal, SYNC_JOURNAL_FORMAT_VERSION,
};

const CURRENT_FORMAT_VERSION: u32 = 1;
const INVENTORY_FORMAT_VERSION: u32 = 1;
const BINDINGS_FORMAT_VERSION: u32 = 1;
const SCOPE_FORMAT_VERSION: u32 = 1;
const UNCLAIMED_SCOPE_MARKER_VERSION: &str = "scope-v1";
const MAX_CREDENTIAL_REF_CHARS: usize = 512;
const MAX_PRIVATE_KEY_PATH_CHARS: usize = 4_096;
const MAX_SYNC_OUTBOX: usize = 100_000;
const MAX_DEFERRED_CHANGES: usize = 10_000;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidScope(&'static str),
    UnsupportedVersion {
        file: &'static str,
        version: u32,
    },
    InvalidInventory(InventoryError),
    InvalidBinding(&'static str),
    InvalidSync(&'static str),
    ConcurrentModification {
        expected: Option<String>,
        actual: Option<String>,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "SSH 数据文件读写失败: {error}"),
            Self::Json(error) => write!(f, "SSH 数据文件格式错误: {error}"),
            Self::InvalidScope(message) => write!(f, "SSH 存储作用域非法: {message}"),
            Self::UnsupportedVersion { file, version } => {
                write!(f, "{file} 版本 {version} 高于当前支持版本")
            }
            Self::InvalidInventory(error) => write!(f, "SSH 库存数据非法: {error}"),
            Self::InvalidBinding(message) => write!(f, "SSH 本地认证绑定非法: {message}"),
            Self::InvalidSync(message) => write!(f, "SSH 同步数据非法: {message}"),
            Self::ConcurrentModification { .. } => {
                f.write_str("SSH 数据已被另一个实例修改，请重新加载后再试")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidInventory(error) => Some(error),
            Self::InvalidScope(_)
            | Self::UnsupportedVersion { .. }
            | Self::InvalidBinding(_)
            | Self::InvalidSync(_)
            | Self::ConcurrentModification { .. } => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<InventoryError> for StoreError {
    fn from(value: InventoryError) -> Self {
        Self::InvalidInventory(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InventoryFile {
    version: u32,
    inventory: SshInventory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BindingsFile {
    version: u32,
    bindings: Vec<SshLocalBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncJournalFile {
    version: u32,
    journal: SyncJournal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CurrentFile {
    version: u32,
    generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountScopeFile {
    version: u32,
    server_origin: String,
    account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSyncRejection {
    pub mutation_id: String,
    pub error_code: Option<String>,
}

/// 主线程成功应用一个同步回包后的摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSyncApplyReport {
    pub acknowledged: usize,
    pub rejected: Vec<SshSyncRejection>,
    pub applied_changes: usize,
    pub deferred_changes: usize,
    pub server_cursor: i64,
    /// 旧 cursor 回包不会回退数据；调用方可用此字段忽略旧的分页提示。
    pub stale_response: bool,
    /// 仅当该回包推进了当前 cursor 时才为服务端 `has_more`。
    pub has_more: bool,
}

/// 账号隔离的 SSH 库存存储。每次变更会先把库存、本地绑定和同步 journal 写入同一 generation，
/// 再原子切换 `current.json`，因此读者只会看见一组完整文件。
///
/// 每个 scope 的文件锁把 `current.json` generation 复核与原子切换串行化，陈旧实例
/// 只能得到 [`StoreError::ConcurrentModification`]。跨 scope 认领固定按 Local →
/// Account 获取双锁；UI 与同步 worker 仍应由单 owner 串行应用状态变更。
pub struct SshStore {
    directory: PathBuf,
    server_origin: Option<String>,
    account_id: Option<String>,
    generation: Option<String>,
    inventory: SshInventory,
    bindings: Vec<SshLocalBinding>,
    sync: SyncJournal,
}

impl fmt::Debug for SshStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshStore")
            .field("directory", &self.directory)
            .field("account_scoped", &self.account_id.is_some())
            .field("generation", &self.generation)
            .field("inventory", &self.inventory)
            .field(
                "bindings",
                &format_args!("<redacted:{}>", self.bindings.len()),
            )
            .field("server_cursor", &self.sync.server_cursor)
            .field("outbox_count", &self.sync.outbox.len())
            .field("deferred_change_count", &self.sync.deferred_changes.len())
            .finish()
    }
}

impl SshStore {
    pub fn load(data_root: &Path, scope: StorageScope) -> Result<Self, StoreError> {
        let account_identity = scope
            .account_identity()
            .map(|(server_origin, account_id)| (server_origin.to_owned(), account_id.to_owned()));
        let server_origin = account_identity
            .as_ref()
            .map(|(server_origin, _)| server_origin.clone());
        let account_id = scope
            .canonical_account_id()
            .map_err(StoreError::InvalidScope)?;
        let directory = scope
            .directory(data_root)
            .map_err(StoreError::InvalidScope)?;
        if let Some((server_origin, account_id)) = account_identity.as_ref() {
            initialize_or_validate_account_scope_file(&directory, server_origin, account_id)?;
        }
        let generation = read_current_generation(&directory)?;
        let (mut inventory, bindings, sync) = match generation.as_deref() {
            Some(generation) => read_generation(&directory, generation)?,
            None => read_legacy_state(&directory)?,
        };
        inventory.validate_loaded()?;
        validate_bindings(&inventory, &bindings)?;
        validate_sync_journal(&sync, account_id.as_deref())?;
        Ok(Self {
            directory,
            server_origin,
            account_id,
            generation,
            inventory,
            bindings,
            sync,
        })
    }

    /// 首次登录时把未登录清单认领给当前服务端 origin + 账号。认领标记先写回
    /// `unclaimed` generation；若随后账号 generation 写入失败，下次仍只允许同一
    /// origin + 账号重试，绝不会把这批数据导入别的存储作用域。
    ///
    /// 账号 generation 提交成功后会清空 `unclaimed` 的库存、binding 与临时认领
    /// 标记。这样一个 Credential Manager target 在任一时刻只有一个活动 store owner；
    /// 登出后的本地编辑或删除不会把账号缓存中的 binding 变成悬空引用。若进程恰在账号
    /// 提交与清空本地之间退出，下次加载会核对两边内容完全一致后完成清理。
    ///
    /// 应在该账号第一次启动同步前调用。账号缓存已经有任何数据/游标时保持原样，
    /// 防止把 unclaimed 清单混入一个既有账号。
    pub fn load_account_claiming_unclaimed(
        data_root: &Path,
        server_origin: &str,
        account_id: &str,
    ) -> Result<Self, StoreError> {
        Self::load_account_claiming_unclaimed_with_hook(
            data_root,
            server_origin,
            account_id,
            |_, _| {},
        )
    }

    fn load_account_claiming_unclaimed_with_hook(
        data_root: &Path,
        server_origin: &str,
        account_id: &str,
        after_account_import: impl FnOnce(&Path, &Path),
    ) -> Result<Self, StoreError> {
        let account_scope =
            StorageScope::account(server_origin, account_id).map_err(StoreError::InvalidScope)?;
        // 首次 load 负责初始化并验证账号 scope；随后固定按 Local → Account
        // 获取两个作用域的跨进程锁。所有认领调用都遵循同一顺序，普通 CRUD
        // 只获取一个作用域锁，因此不会形成反向等待环。
        let initial_account = Self::load(data_root, account_scope.clone())?;
        let initial_unclaimed = Self::load(data_root, StorageScope::Local)?;
        let local_write_lock = acquire_store_write_lock(&initial_unclaimed.directory)?;
        let account_write_lock = acquire_store_write_lock(&initial_account.directory)?;

        // 两把锁可能在首次 load 后才取得，期间 generation 可以变化。必须在
        // 持锁后重新加载两边，后续三次提交都以这份锁内快照为 CAS 基线。
        let mut account = Self::load(data_root, account_scope)?;
        let mut unclaimed = Self::load(data_root, StorageScope::Local)?;
        let canonical_origin = account
            .server_origin
            .clone()
            .ok_or(StoreError::InvalidScope("账号作用域缺少服务端 origin"))?;
        let canonical_account = account
            .account_id
            .clone()
            .ok_or(StoreError::InvalidScope("账号作用域缺少账号 ID"))?;
        let requested_claim = unclaimed_scope_marker(&canonical_origin, &canonical_account);
        if unclaimed.inventory.groups().is_empty()
            && unclaimed.inventory.profiles().is_empty()
            && unclaimed.bindings.is_empty()
        {
            return Ok(account);
        }
        let account_was_pristine = account.is_pristine_account_cache();
        match unclaimed.sync.claimed_account_id.as_deref() {
            Some(claimed) if claimed != requested_claim => return Ok(account),
            Some(_) if !account_was_pristine => {
                if account.inventory != unclaimed.inventory
                    || account.bindings != unclaimed.bindings
                {
                    return Err(StoreError::InvalidSync(
                        "认领恢复时账号与 unclaimed 数据不一致",
                    ));
                }
            }
            Some(_) => {}
            None => {
                if !account_was_pristine {
                    return Ok(account);
                }
                let mut claimed_sync = unclaimed.sync.clone();
                claimed_sync.claimed_account_id = Some(requested_claim.clone());
                unclaimed.commit_state_under_lock(
                    unclaimed.inventory.clone(),
                    unclaimed.bindings.clone(),
                    claimed_sync,
                    &local_write_lock,
                )?;
            }
        }

        if account_was_pristine {
            let imported_inventory = unclaimed.inventory.clone();
            let imported_bindings = unclaimed.bindings.clone();
            let mut imported_sync = SyncJournal::default();
            enqueue_inventory_diff(
                &mut imported_sync,
                &SshInventory::default(),
                &imported_inventory,
            )?;
            account.commit_state_under_lock(
                imported_inventory,
                imported_bindings,
                imported_sync,
                &account_write_lock,
            )?;
        }

        // 测试钩子位于最危险的 owner 重叠窗口：账号 binding 已提交，但
        // unclaimed 尚未清空。此时两把锁都必须仍在持有。
        after_account_import(&unclaimed.directory, &account.directory);
        unclaimed.commit_state_under_lock(
            SshInventory::default(),
            Vec::new(),
            SyncJournal::default(),
            &local_write_lock,
        )?;
        Ok(account)
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub fn server_origin(&self) -> Option<&str> {
        self.server_origin.as_deref()
    }

    pub fn server_cursor(&self) -> i64 {
        self.sync.server_cursor
    }

    pub fn pending_sync_mutations(&self) -> usize {
        self.sync.outbox.len()
    }

    /// 生成只含协议 allowlist 字段的网络快照。未登录作用域返回 `None`。
    pub fn sync_snapshot(&self) -> Option<SshSyncSnapshot> {
        self.account_id
            .clone()
            .map(|account_id| SshSyncSnapshot::new(account_id, &self.sync))
    }

    pub fn inventory(&self) -> &SshInventory {
        &self.inventory
    }

    pub fn bindings(&self) -> &[SshLocalBinding] {
        &self.bindings
    }

    pub fn binding(&self, profile_id: &str) -> Option<&SshLocalBinding> {
        self.bindings
            .iter()
            .find(|binding| binding.profile_id == profile_id)
    }

    pub fn create_group(&mut self, name: &str) -> Result<GroupId, StoreError> {
        self.change_inventory(|next| next.create_group(name))
    }

    pub fn rename_group(&mut self, id: &str, name: &str) -> Result<(), StoreError> {
        self.change_inventory(|next| next.rename_group(id, name))
    }

    pub fn delete_group(&mut self, id: &str) -> Result<(), StoreError> {
        self.change_inventory(|next| next.delete_group(id))
    }

    pub fn reorder_groups(&mut self, ids: &[GroupId]) -> Result<(), StoreError> {
        self.change_inventory(|next| next.reorder_groups(ids))
    }

    pub fn create_profile(&mut self, draft: NewSshProfile) -> Result<ProfileId, StoreError> {
        self.change_inventory(|next| next.create_profile(draft))
    }

    pub fn update_profile(&mut self, id: &str, draft: NewSshProfile) -> Result<(), StoreError> {
        let mut next_inventory = self.inventory.clone();
        next_inventory.update_profile(id, draft)?;
        let mut next_bindings = self.bindings.clone();
        sanitize_bindings_for_inventory_transition(
            &self.inventory,
            &next_inventory,
            &mut next_bindings,
        );

        let mut next_sync = self.sync.clone();
        enqueue_inventory_diff(&mut next_sync, &self.inventory, &next_inventory)?;
        self.commit_state(next_inventory, next_bindings, next_sync)
    }

    pub fn delete_profile(&mut self, id: &str) -> Result<(), StoreError> {
        let mut next_inventory = self.inventory.clone();
        next_inventory.delete_profile(id)?;
        let mut next_bindings = self.bindings.clone();
        next_bindings.retain(|binding| binding.profile_id != id);
        let mut next_sync = self.sync.clone();
        enqueue_inventory_diff(&mut next_sync, &self.inventory, &next_inventory)?;
        self.commit_state(next_inventory, next_bindings, next_sync)
    }

    pub fn move_profile(
        &mut self,
        id: &str,
        target_group_id: Option<&str>,
        target_index: usize,
    ) -> Result<(), StoreError> {
        self.change_inventory(|next| next.move_profile(id, target_group_id, target_index))
    }

    pub fn reorder_profiles_in_group(
        &mut self,
        group_id: Option<&str>,
        ordered_ids: &[ProfileId],
    ) -> Result<(), StoreError> {
        self.change_inventory(|next| next.reorder_profiles_in_group(group_id, ordered_ids))
    }

    pub fn upsert_binding(&mut self, binding: SshLocalBinding) -> Result<(), StoreError> {
        validate_binding(&self.inventory, &binding)?;
        let mut next = self.bindings.clone();
        if let Some(existing) = next
            .iter_mut()
            .find(|existing| existing.profile_id == binding.profile_id)
        {
            *existing = binding;
        } else {
            next.push(binding);
        }
        self.commit_state(self.inventory.clone(), next, self.sync.clone())
    }

    pub fn remove_binding(&mut self, profile_id: &str) -> Result<(), StoreError> {
        let mut next = self.bindings.clone();
        next.retain(|binding| binding.profile_id != profile_id);
        self.commit_state(self.inventory.clone(), next, self.sync.clone())
    }

    pub fn apply_sync_completed(
        &mut self,
        completed: SshSyncCompleted,
    ) -> Result<SshSyncApplyReport, StoreError> {
        self.apply_sync_response(&completed.snapshot, completed.response)
    }

    /// 在唯一持有 `SshStore` 的线程中应用回包。旧回包可以确认它实际发送过的
    /// mutation，但不能回退 cursor 或覆盖仍有本地 pending mutation 的记录。
    pub fn apply_sync_response(
        &mut self,
        snapshot: &SshSyncSnapshot,
        response: SshSyncResponse,
    ) -> Result<SshSyncApplyReport, StoreError> {
        validate_sync_response(self, snapshot, &response)?;
        let previous_cursor = self.sync.server_cursor;
        let stale_response =
            snapshot.request().cursor < previous_cursor || response.next_cursor < previous_cursor;

        let sent_mutations = snapshot
            .request()
            .mutations
            .iter()
            .map(|mutation| (mutation.mutation_id.as_str(), mutation))
            .collect::<HashMap<_, _>>();
        let acknowledged_ids = response
            .acks
            .iter()
            .map(|ack| ack.mutation_id.as_str())
            .collect::<HashSet<_>>();
        let rejected = response
            .acks
            .iter()
            .filter(|ack| ack.status == SshMutationStatus::Rejected || ack.error_code.is_some())
            .map(|ack| SshSyncRejection {
                mutation_id: ack.mutation_id.clone(),
                error_code: ack.error_code.clone(),
            })
            .collect::<Vec<_>>();

        let mut next_inventory = self.inventory.clone();
        let mut next_bindings = self.bindings.clone();
        let mut next_sync = self.sync.clone();
        next_sync
            .outbox
            .retain(|mutation| !acknowledged_ids.contains(mutation.mutation_id.as_str()));
        next_sync.server_cursor = next_sync.server_cursor.max(response.next_cursor);

        // 旧回包仍可幂等确认它实际发送过的 mutation，但绝不能据旧的 rejected
        // ack 删除当前较新的本地实体或凭据绑定。冲突清理由当前游标回包或权威
        // tombstone 驱动。
        if !stale_response {
            for ack in &response.acks {
                if ack.status != SshMutationStatus::Rejected && ack.error_code.is_none() {
                    continue;
                }
                let mutation = sent_mutations
                    .get(ack.mutation_id.as_str())
                    .ok_or(StoreError::InvalidSync("ack 不属于请求快照"))?;
                cleanup_rejected_local_entity(
                    &mut next_inventory,
                    &mut next_bindings,
                    &next_sync.outbox,
                    &mutation.operation,
                    ack.error_code.as_deref(),
                );
            }
        }

        let mut latest_by_entity: HashMap<(String, String), SshSyncChange> = HashMap::new();
        for change in next_sync.deferred_changes.drain(..).chain(
            response
                .changes
                .iter()
                .filter(|change| change.revision > previous_cursor)
                .cloned(),
        ) {
            let (kind, id) = change_entity(&change.change);
            let key = (kind.to_owned(), id.to_owned());
            match latest_by_entity.get(&key) {
                Some(existing) if existing.revision >= change.revision => {}
                _ => {
                    latest_by_entity.insert(key, change);
                }
            }
        }
        let mut ordered_changes = latest_by_entity
            .into_values()
            .map(|change| (change.revision, change))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect::<Vec<_>>();

        let mut applied_changes = 0_usize;
        loop {
            let mut deferred = Vec::new();
            let mut made_progress = false;
            for change in ordered_changes {
                if has_pending_entity(&next_sync.outbox, &change.change) {
                    deferred.push(change);
                    continue;
                }
                match apply_remote_change(&mut next_inventory, &mut next_bindings, &change.change)?
                {
                    RemoteApply::Applied => {
                        applied_changes += 1;
                        made_progress = true;
                    }
                    RemoteApply::Deferred => deferred.push(change),
                }
            }
            if deferred.is_empty() || !made_progress {
                ordered_changes = deferred;
                break;
            }
            ordered_changes = deferred;
        }
        if ordered_changes.len() > MAX_DEFERRED_CHANGES {
            return Err(StoreError::InvalidSync("待合并远端变更数量超过上限"));
        }
        next_sync.deferred_changes = ordered_changes;
        sanitize_bindings_for_inventory_transition(
            &self.inventory,
            &next_inventory,
            &mut next_bindings,
        );
        validate_bindings(&next_inventory, &next_bindings)?;
        validate_sync_journal(&next_sync, self.account_id.as_deref())?;

        let acknowledged = response.acks.len();
        let has_more = response.has_more && response.next_cursor > previous_cursor;
        let report = SshSyncApplyReport {
            acknowledged,
            rejected,
            applied_changes,
            deferred_changes: next_sync.deferred_changes.len(),
            server_cursor: next_sync.server_cursor,
            stale_response,
            has_more,
        };
        if next_inventory != self.inventory
            || next_bindings != self.bindings
            || next_sync != self.sync
        {
            self.commit_state(next_inventory, next_bindings, next_sync)?;
        }
        Ok(report)
    }

    fn change_inventory<T>(
        &mut self,
        mutate: impl FnOnce(&mut SshInventory) -> Result<T, InventoryError>,
    ) -> Result<T, StoreError> {
        let mut next = self.inventory.clone();
        let result = mutate(&mut next)?;
        let mut next_sync = self.sync.clone();
        enqueue_inventory_diff(&mut next_sync, &self.inventory, &next)?;
        self.commit_state(next, self.bindings.clone(), next_sync)?;
        Ok(result)
    }

    fn commit_state(
        &mut self,
        inventory: SshInventory,
        bindings: Vec<SshLocalBinding>,
        sync: SyncJournal,
    ) -> Result<(), StoreError> {
        // `--multi-instance` 也可能让两个进程同时写同一作用域。必须把
        // generation 复核到 current.json 原子替换的整个窗口串行化；否则
        // 两边都可能通过旧 generation 检查，后写者覆盖前写者，并让刚删除
        // 的旧 Credential Manager target 再次成为 current binding。
        let write_lock = acquire_store_write_lock(&self.directory)?;
        self.commit_state_under_lock(inventory, bindings, sync, &write_lock)
    }

    /// 在调用方已经持有当前作用域 `.write.lock` 时提交，避免认领事务
    /// 对同一文件锁重入。锁引用必须覆盖本函数整个调用。
    fn commit_state_under_lock(
        &mut self,
        inventory: SshInventory,
        bindings: Vec<SshLocalBinding>,
        sync: SyncJournal,
        _write_lock: &std::fs::File,
    ) -> Result<(), StoreError> {
        validate_bindings(&inventory, &bindings)?;
        validate_sync_journal(&sync, self.account_id.as_deref())?;
        match (self.server_origin.as_deref(), self.account_id.as_deref()) {
            (Some(server_origin), Some(account_id)) => {
                validate_account_scope_file(&self.directory, server_origin, account_id)?;
            }
            (None, None) => {}
            _ => return Err(StoreError::InvalidScope("账号作用域身份不完整")),
        }
        let actual_generation = read_current_generation(&self.directory)?;
        if actual_generation != self.generation {
            return Err(StoreError::ConcurrentModification {
                expected: self.generation.clone(),
                actual: actual_generation,
            });
        }

        let generation = new_id("gen_");
        let generations_directory = self.directory.join("generations");
        fs::create_dir_all(&generations_directory)?;
        let generation_directory = generations_directory.join(&generation);
        fs::create_dir(&generation_directory)?;

        let inventory_file = InventoryFile {
            version: INVENTORY_FORMAT_VERSION,
            inventory: inventory.clone(),
        };
        let bindings_file = BindingsFile {
            version: BINDINGS_FORMAT_VERSION,
            bindings: bindings.clone(),
        };
        let sync_file = SyncJournalFile {
            version: SYNC_JOURNAL_FORMAT_VERSION,
            journal: sync.clone(),
        };
        let write_result = (|| {
            write_new_json(
                &generation_directory.join("sync_state.json"),
                &inventory_file,
            )?;
            write_new_json(
                &generation_directory.join("local_bindings.json"),
                &bindings_file,
            )?;
            write_new_json(&generation_directory.join("sync_journal.json"), &sync_file)?;
            atomic_write_json(
                &self.directory.join("current.json"),
                &CurrentFile {
                    version: CURRENT_FORMAT_VERSION,
                    generation: generation.clone(),
                },
            )
        })();
        if write_result.is_err() {
            let _ = fs::remove_dir_all(&generation_directory);
            return write_result;
        }

        self.generation = Some(generation);
        self.inventory = inventory;
        self.bindings = bindings;
        self.sync = sync;
        Ok(())
    }

    fn is_pristine_account_cache(&self) -> bool {
        self.inventory.groups().is_empty()
            && self.inventory.profiles().is_empty()
            && self.bindings.is_empty()
            && self.sync.server_cursor == 0
            && self.sync.outbox.is_empty()
            && self.sync.deferred_changes.is_empty()
    }
}

fn acquire_store_write_lock(directory: &Path) -> Result<std::fs::File, StoreError> {
    fs::create_dir_all(directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".write.lock"))?;
    file.lock()?;
    Ok(file)
}

fn enqueue_inventory_diff(
    journal: &mut SyncJournal,
    before: &SshInventory,
    after: &SshInventory,
) -> Result<(), StoreError> {
    for group in before.groups() {
        if after.group(&group.id).is_none() {
            push_mutation(
                journal,
                SshMutationOperation::DeleteGroup {
                    group_id: group.id.clone(),
                },
            )?;
        }
    }
    for group in after.groups() {
        if before.group(&group.id) != Some(group) {
            push_mutation(
                journal,
                SshMutationOperation::UpsertGroup {
                    group: wire_group(group),
                },
            )?;
        }
    }
    for profile in before.profiles() {
        if after.profile(&profile.id).is_none() {
            push_mutation(
                journal,
                SshMutationOperation::DeleteProfile {
                    profile_id: profile.id.clone(),
                },
            )?;
        }
    }
    for profile in after.profiles() {
        if before.profile(&profile.id) != Some(profile) {
            push_mutation(
                journal,
                SshMutationOperation::UpsertProfile {
                    profile: wire_profile(profile),
                },
            )?;
        }
    }
    Ok(())
}

fn push_mutation(
    journal: &mut SyncJournal,
    operation: SshMutationOperation,
) -> Result<(), StoreError> {
    if journal.outbox.len() >= MAX_SYNC_OUTBOX {
        return Err(StoreError::InvalidSync("本地待同步变更数量超过上限"));
    }
    journal.outbox.push(SshMutation {
        mutation_id: new_id("mut_"),
        base_revision: journal.server_cursor,
        operation,
    });
    Ok(())
}

fn unclaimed_scope_marker(server_origin: &str, account_id: &str) -> String {
    format!("{UNCLAIMED_SCOPE_MARKER_VERSION}|{server_origin}|{account_id}")
}

fn validate_unclaimed_scope_marker(marker: &str) -> Result<(), StoreError> {
    let mut components = marker.split('|');
    let (Some(version), Some(server_origin), Some(account_id), None) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    ) else {
        return Err(StoreError::InvalidSync(
            "unclaimed 认领标记格式非法或字段缺失",
        ));
    };
    if version != UNCLAIMED_SCOPE_MARKER_VERSION {
        return Err(StoreError::InvalidSync("unclaimed 认领标记版本不受支持"));
    }
    let scope = StorageScope::account(server_origin, account_id)
        .map_err(|_| StoreError::InvalidSync("unclaimed 认领作用域非法"))?;
    let canonical_account = scope
        .canonical_account_id()
        .map_err(StoreError::InvalidScope)?
        .ok_or(StoreError::InvalidSync("unclaimed 认领账号缺失"))?;
    let canonical_origin = scope
        .server_origin()
        .ok_or(StoreError::InvalidSync("unclaimed 认领 origin 缺失"))?;
    if unclaimed_scope_marker(canonical_origin, &canonical_account) != marker {
        return Err(StoreError::InvalidSync(
            "unclaimed 认领作用域不是 canonical 形式",
        ));
    }
    Ok(())
}

fn validate_sync_journal(
    journal: &SyncJournal,
    account_id: Option<&str>,
) -> Result<(), StoreError> {
    if journal.server_cursor < 0 {
        return Err(StoreError::InvalidSync("server cursor 不能为负数"));
    }
    if journal.outbox.len() > MAX_SYNC_OUTBOX {
        return Err(StoreError::InvalidSync("本地待同步变更数量超过上限"));
    }
    if journal.deferred_changes.len() > MAX_DEFERRED_CHANGES {
        return Err(StoreError::InvalidSync("待合并远端变更数量超过上限"));
    }
    match (account_id, journal.claimed_account_id.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(StoreError::InvalidSync("账号缓存不能带 unclaimed 认领标记"));
        }
        (None, Some(claimed)) => {
            validate_unclaimed_scope_marker(claimed)?;
        }
        _ => {}
    }

    let mut mutation_ids = HashSet::with_capacity(journal.outbox.len());
    for mutation in &journal.outbox {
        if !valid_prefixed_id(&mutation.mutation_id, "mut_")
            || mutation.base_revision < 0
            || mutation.base_revision > journal.server_cursor
        {
            return Err(StoreError::InvalidSync("outbox mutation 元数据非法"));
        }
        if !mutation_ids.insert(mutation.mutation_id.as_str()) {
            return Err(StoreError::InvalidSync("outbox mutation_id 重复"));
        }
        let (kind, id) = mutation_entity(&mutation.operation);
        let expected_prefix = if kind == "group" { "grp_" } else { "ssh_" };
        if !valid_prefixed_id(id, expected_prefix) {
            return Err(StoreError::InvalidSync("outbox 实体 ID 非法"));
        }
    }

    let mut revisions = HashSet::with_capacity(journal.deferred_changes.len());
    for change in &journal.deferred_changes {
        if change.revision <= 0 || !revisions.insert(change.revision) {
            return Err(StoreError::InvalidSync("待合并远端 revision 非法或重复"));
        }
        let (kind, id) = change_entity(&change.change);
        let expected_prefix = if kind == "group" { "grp_" } else { "ssh_" };
        if !valid_prefixed_id(id, expected_prefix) {
            return Err(StoreError::InvalidSync("待合并远端实体 ID 非法"));
        }
    }
    Ok(())
}

fn validate_sync_response(
    store: &SshStore,
    snapshot: &SshSyncSnapshot,
    response: &SshSyncResponse,
) -> Result<(), StoreError> {
    let account_id = store
        .account_id
        .as_deref()
        .ok_or(StoreError::InvalidSync("未登录作用域不能应用同步回包"))?;
    if snapshot.account_id() != account_id {
        return Err(StoreError::InvalidSync("同步回包账号与当前缓存不一致"));
    }
    let request = snapshot.request();
    if request.schema_version != SSH_SYNC_SCHEMA_VERSION
        || response.schema_version != SSH_SYNC_SCHEMA_VERSION
    {
        return Err(StoreError::InvalidSync("SSH 同步 schema 不受支持"));
    }
    if request.cursor < 0 || request.cursor > store.sync.server_cursor {
        return Err(StoreError::InvalidSync("请求快照 cursor 非法"));
    }
    if response.next_cursor < request.cursor {
        return Err(StoreError::InvalidSync("回包 cursor 发生回退"));
    }
    if response.changes.len() > usize::from(request.limit) {
        return Err(StoreError::InvalidSync("远端 changes 数量超过请求上限"));
    }

    let sent_ids = request
        .mutations
        .iter()
        .map(|mutation| mutation.mutation_id.as_str())
        .collect::<HashSet<_>>();
    let mut ack_ids = HashSet::with_capacity(response.acks.len());
    if response.acks.len() != sent_ids.len() {
        return Err(StoreError::InvalidSync("ack 数量与请求 mutation 不一致"));
    }
    for ack in &response.acks {
        if !sent_ids.contains(ack.mutation_id.as_str()) || !ack_ids.insert(ack.mutation_id.as_str())
        {
            return Err(StoreError::InvalidSync("ack mutation_id 非法或重复"));
        }
        if ack.revision.is_some_and(|revision| revision <= 0) {
            return Err(StoreError::InvalidSync("ack revision 非法"));
        }
        let valid_error_code = ack.error_code.as_deref().is_none_or(|code| {
            !code.is_empty()
                && code.len() <= 64
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        });
        if !valid_error_code {
            return Err(StoreError::InvalidSync("ack error_code 非法"));
        }
        let valid_outcome = match ack.status {
            SshMutationStatus::Applied => ack.revision.is_some() && ack.error_code.is_none(),
            SshMutationStatus::Rejected => ack.revision.is_none() && ack.error_code.is_some(),
            // 服务端用 Duplicate 表示 mutation_id 已处理；它既可能复现一次
            // 成功，也可能复现一次拒绝，因此必须严格落在两种合法形态之一。
            SshMutationStatus::Duplicate => ack.revision.is_some() ^ ack.error_code.is_some(),
        };
        if !valid_outcome {
            return Err(StoreError::InvalidSync("ack 状态与结果字段不一致"));
        }
    }

    let mut prior_revision = request.cursor;
    for change in &response.changes {
        if change.revision <= prior_revision || change.revision > response.next_cursor {
            return Err(StoreError::InvalidSync("远端 changes revision 顺序非法"));
        }
        let (kind, id) = change_entity(&change.change);
        let expected_prefix = if kind == "group" { "grp_" } else { "ssh_" };
        if !valid_prefixed_id(id, expected_prefix) {
            return Err(StoreError::InvalidSync("远端实体 ID 非法"));
        }
        prior_revision = change.revision;
    }
    let expected_next_cursor = response
        .changes
        .last()
        .map_or(request.cursor, |change| change.revision);
    if response.next_cursor != expected_next_cursor {
        return Err(StoreError::InvalidSync(
            "回包 next_cursor 与最后一条 change 不一致",
        ));
    }
    if response.has_more && response.changes.len() != usize::from(request.limit) {
        return Err(StoreError::InvalidSync("has_more 回包未填满请求的变更批次"));
    }
    Ok(())
}

fn has_pending_entity(outbox: &[SshMutation], change: &SshChange) -> bool {
    let (change_kind, change_id) = change_entity(change);
    outbox.iter().any(|mutation| {
        let (mutation_kind, mutation_id) = mutation_entity(&mutation.operation);
        mutation_kind == change_kind && mutation_id == change_id
    })
}

fn cleanup_rejected_local_entity(
    inventory: &mut SshInventory,
    bindings: &mut Vec<SshLocalBinding>,
    remaining_outbox: &[SshMutation],
    operation: &SshMutationOperation,
    error_code: Option<&str>,
) {
    if !matches!(error_code, Some("deleted_entity" | "group_name_conflict")) {
        return;
    }
    let (kind, id) = mutation_entity(operation);
    if remaining_outbox.iter().any(|mutation| {
        let (pending_kind, pending_id) = mutation_entity(&mutation.operation);
        pending_kind == kind && pending_id == id
    }) {
        return;
    }
    match operation {
        SshMutationOperation::UpsertGroup { group } => {
            inventory.apply_synced_group_deletion(&group.id);
        }
        SshMutationOperation::UpsertProfile { profile } => {
            inventory.apply_synced_profile_deletion(&profile.id);
            bindings.retain(|binding| binding.profile_id != profile.id);
        }
        SshMutationOperation::DeleteGroup { .. } | SshMutationOperation::DeleteProfile { .. } => {}
    }
}

enum RemoteApply {
    Applied,
    Deferred,
}

fn apply_remote_change(
    inventory: &mut SshInventory,
    bindings: &mut Vec<SshLocalBinding>,
    change: &SshChange,
) -> Result<RemoteApply, StoreError> {
    match change {
        SshChange::UpsertGroup { group } => {
            match inventory.apply_synced_group(local_group(group.clone())) {
                Ok(()) => Ok(RemoteApply::Applied),
                Err(InventoryError::DuplicateGroupName) => Ok(RemoteApply::Deferred),
                Err(error) => Err(StoreError::InvalidInventory(error)),
            }
        }
        SshChange::DeleteGroup { group_id } => {
            inventory.apply_synced_group_deletion(group_id);
            Ok(RemoteApply::Applied)
        }
        SshChange::UpsertProfile { profile } => {
            match inventory.apply_synced_profile(local_profile(profile.clone())) {
                Ok(()) => Ok(RemoteApply::Applied),
                Err(InventoryError::InvalidGroup) => Ok(RemoteApply::Deferred),
                Err(error) => Err(StoreError::InvalidInventory(error)),
            }
        }
        SshChange::DeleteProfile { profile_id } => {
            inventory.apply_synced_profile_deletion(profile_id);
            bindings.retain(|binding| binding.profile_id != *profile_id);
            Ok(RemoteApply::Applied)
        }
    }
}

fn valid_prefixed_id(id: &str, prefix: &str) -> bool {
    id.len() == prefix.len() + 32
        && id.starts_with(prefix)
        && id[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn expected_account_scope_file(server_origin: &str, account_id: &str) -> AccountScopeFile {
    AccountScopeFile {
        version: SCOPE_FORMAT_VERSION,
        server_origin: server_origin.to_owned(),
        account_id: account_id.to_owned(),
    }
}

fn initialize_or_validate_account_scope_file(
    directory: &Path,
    server_origin: &str,
    account_id: &str,
) -> Result<(), StoreError> {
    let scope_path = directory.join("scope.json");
    match read_json::<AccountScopeFile>(&scope_path)? {
        Some(actual) => validate_account_scope_contents(&actual, server_origin, account_id),
        None => {
            if directory.exists() {
                let mut entries = fs::read_dir(directory)?;
                if entries.next().transpose()?.is_some() {
                    return Err(StoreError::InvalidScope(
                        "账号目录已有数据但缺少 scope.json",
                    ));
                }
            } else {
                fs::create_dir_all(directory)?;
            }
            atomic_write_json(
                &scope_path,
                &expected_account_scope_file(server_origin, account_id),
            )?;
            validate_account_scope_file(directory, server_origin, account_id)
        }
    }
}

fn validate_account_scope_file(
    directory: &Path,
    server_origin: &str,
    account_id: &str,
) -> Result<(), StoreError> {
    let actual = read_required_json::<AccountScopeFile>(&directory.join("scope.json"))?;
    validate_account_scope_contents(&actual, server_origin, account_id)
}

fn validate_account_scope_contents(
    actual: &AccountScopeFile,
    server_origin: &str,
    account_id: &str,
) -> Result<(), StoreError> {
    if actual.version != SCOPE_FORMAT_VERSION {
        return Err(StoreError::InvalidScope("scope.json 版本不受支持"));
    }
    if actual.server_origin != server_origin || actual.account_id != account_id {
        return Err(StoreError::InvalidScope(
            "scope.json 与请求的服务端 origin 或账号不一致",
        ));
    }
    Ok(())
}

fn read_current_generation(directory: &Path) -> Result<Option<String>, StoreError> {
    let Some(current) = read_json::<CurrentFile>(&directory.join("current.json"))? else {
        return Ok(None);
    };
    if current.version > CURRENT_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion {
            file: "current.json",
            version: current.version,
        });
    }
    if !valid_generation_id(&current.generation) {
        return Err(StoreError::InvalidBinding("current generation ID 非法"));
    }
    Ok(Some(current.generation))
}

fn read_generation(
    directory: &Path,
    generation: &str,
) -> Result<(SshInventory, Vec<SshLocalBinding>, SyncJournal), StoreError> {
    if !valid_generation_id(generation) {
        return Err(StoreError::InvalidBinding("current generation ID 非法"));
    }
    let generation_directory = directory.join("generations").join(generation);
    let inventory_file =
        read_required_json::<InventoryFile>(&generation_directory.join("sync_state.json"))?;
    let bindings_file =
        read_required_json::<BindingsFile>(&generation_directory.join("local_bindings.json"))?;
    let sync_file = read_json::<SyncJournalFile>(&generation_directory.join("sync_journal.json"))?;
    validate_inventory_version(&inventory_file)?;
    validate_bindings_version(&bindings_file)?;
    let sync = match sync_file {
        Some(file) => {
            validate_sync_version(&file)?;
            file.journal
        }
        None => {
            let mut journal = SyncJournal::default();
            enqueue_inventory_diff(
                &mut journal,
                &SshInventory::default(),
                &inventory_file.inventory,
            )?;
            journal
        }
    };
    Ok((inventory_file.inventory, bindings_file.bindings, sync))
}

/// 未发布的早期构建曾把两个文件直接写在 scope 目录。没有 `current.json` 时只读一次
/// 该布局；下一次成功变更会迁入 generation 布局。孤立 generation 不参与恢复。
fn read_legacy_state(
    directory: &Path,
) -> Result<(SshInventory, Vec<SshLocalBinding>, SyncJournal), StoreError> {
    let inventory = match read_json::<InventoryFile>(&directory.join("sync_state.json"))? {
        Some(file) => {
            validate_inventory_version(&file)?;
            file.inventory
        }
        None => SshInventory::default(),
    };
    let bindings = match read_json::<BindingsFile>(&directory.join("local_bindings.json"))? {
        Some(file) => {
            validate_bindings_version(&file)?;
            file.bindings
        }
        None => Vec::new(),
    };
    let mut sync = SyncJournal::default();
    enqueue_inventory_diff(&mut sync, &SshInventory::default(), &inventory)?;
    Ok((inventory, bindings, sync))
}

fn validate_inventory_version(file: &InventoryFile) -> Result<(), StoreError> {
    if file.version > INVENTORY_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion {
            file: "sync_state.json",
            version: file.version,
        });
    }
    Ok(())
}

fn validate_bindings_version(file: &BindingsFile) -> Result<(), StoreError> {
    if file.version > BINDINGS_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion {
            file: "local_bindings.json",
            version: file.version,
        });
    }
    Ok(())
}

fn validate_sync_version(file: &SyncJournalFile) -> Result<(), StoreError> {
    if file.version > SYNC_JOURNAL_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion {
            file: "sync_journal.json",
            version: file.version,
        });
    }
    Ok(())
}

fn valid_generation_id(id: &str) -> bool {
    id.len() == "gen_".len() + 32
        && id.starts_with("gen_")
        && id["gen_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_bindings(
    inventory: &SshInventory,
    bindings: &[SshLocalBinding],
) -> Result<(), StoreError> {
    if bindings.len() > inventory.profiles().len() {
        return Err(StoreError::InvalidBinding("绑定数量超过服务器数量"));
    }
    let mut seen = HashSet::with_capacity(bindings.len());
    for binding in bindings {
        validate_binding(inventory, binding)?;
        if !seen.insert(binding.profile_id.as_str()) {
            return Err(StoreError::InvalidBinding("同一服务器存在重复本地绑定"));
        }
    }
    Ok(())
}

/// 让只保留在本机的凭据绑定跟随一次库存切换，同时避免把旧 endpoint 的
/// 密码/私钥静默用于服务端同步下来的新目标。该函数只改下一 generation
/// 的 binding；Credential Manager 中旧 target 由主线程在提交成功后清理。
fn sanitize_bindings_for_inventory_transition(
    previous: &SshInventory,
    next: &SshInventory,
    bindings: &mut Vec<SshLocalBinding>,
) {
    bindings.retain_mut(|binding| {
        let Some(previous_profile) = previous.profile(&binding.profile_id) else {
            return false;
        };
        let Some(next_profile) = next.profile(&binding.profile_id) else {
            return false;
        };
        if previous_profile.host != next_profile.host
            || previous_profile.port != next_profile.port
            || previous_profile.username != next_profile.username
        {
            return false;
        }
        match next_profile.auth_method {
            AuthMethod::Password => {
                binding.private_key_path = None;
                binding.key_passphrase_credential_ref = None;
            }
            AuthMethod::PrivateKey => {
                binding.password_credential_ref = None;
            }
            AuthMethod::Agent => {
                binding.private_key_path = None;
                binding.password_credential_ref = None;
                binding.key_passphrase_credential_ref = None;
            }
        }
        binding.private_key_path.is_some()
            || binding.password_credential_ref.is_some()
            || binding.key_passphrase_credential_ref.is_some()
    });
}

fn validate_binding(inventory: &SshInventory, binding: &SshLocalBinding) -> Result<(), StoreError> {
    if binding.profile_id.is_empty() || inventory.profile(&binding.profile_id).is_none() {
        return Err(StoreError::InvalidBinding("绑定引用了不存在的服务器"));
    }
    if let Some(path) = &binding.private_key_path {
        let Some(text) = path.to_str() else {
            return Err(StoreError::InvalidBinding("私钥路径必须是有效 Unicode"));
        };
        if !path.is_absolute()
            || text.is_empty()
            || text.chars().count() > MAX_PRIVATE_KEY_PATH_CHARS
            || text.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidBinding("私钥路径非法或过长"));
        }
    }
    for (reference, expected_slot) in [
        (
            binding.password_credential_ref.as_deref(),
            CredentialSlot::Password,
        ),
        (
            binding.key_passphrase_credential_ref.as_deref(),
            CredentialSlot::KeyPassphrase,
        ),
    ] {
        let Some(reference) = reference else {
            continue;
        };
        if reference.is_empty()
            || reference.trim() != reference
            || reference.chars().count() > MAX_CREDENTIAL_REF_CHARS
            || reference.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidBinding("系统凭据引用非法或过长"));
        }
        let parsed = reference
            .parse::<CredentialReference>()
            .map_err(|_| StoreError::InvalidBinding("系统凭据引用格式非法"))?;
        if parsed.profile_id() != binding.profile_id || parsed.slot() != expected_slot {
            return Err(StoreError::InvalidBinding(
                "系统凭据引用与服务器或凭据用途不匹配",
            ));
        }
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(
            text.trim_start_matches('\u{feff}'),
        )?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_required_json<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    read_json(path)?.ok_or_else(|| {
        StoreError::Io(io::Error::new(
            ErrorKind::NotFound,
            format!("SSH generation 文件缺失: {}", path.display()),
        ))
    })
}

fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "SSH 数据文件路径无父目录"))?;
    fs::create_dir_all(directory)?;
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = unique_temporary_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.map_err(StoreError::Io)
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let mut random = [0_u8; 8];
    OsRng.fill_bytes(&mut random);
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ssh-data.json");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), suffix))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    // 账号作用域多了 server hash 与 account 两层目录，正常绝对路径也可能超过
    // Win32 的传统 MAX_PATH。`canonicalize` 在 Windows 返回 `\\?\` 扩展长度
    // 路径；target 可能尚不存在，因此只 canonicalize 其已存在的父目录。
    let source = fs::canonicalize(source)?;
    let target_parent = target
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "SSH 目标路径无父目录"))?;
    let target_name = target
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "SSH 目标路径无文件名"))?;
    let target = fs::canonicalize(target_parent)?.join(target_name);

    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target_wide = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: 两个 UTF-16 缓冲区均以 NUL 结尾，且在调用期间保持存活；
    // flags 只要求同卷替换目标并刷盘。
    let succeeded = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::AuthMethod;
    use super::*;
    use lumen_protocol::ssh_sync::{SshMutationAck, SshSyncChange, SshSyncResponse};
    use std::time::{SystemTime, UNIX_EPOCH};

    const TEST_ORIGIN: &str = "https://lumen.example";

    fn temporary_root(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lumen-ssh-store-{test_name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn draft(name: &str, group_id: Option<String>) -> NewSshProfile {
        NewSshProfile {
            name: name.to_owned(),
            host: "server.example.com".to_owned(),
            username: "lumen".to_owned(),
            auth_method: AuthMethod::PrivateKey,
            group_id,
            ..Default::default()
        }
    }

    fn account_id(suffix: u8) -> String {
        format!("550e8400-e29b-41d4-a716-4466554400{suffix:02}")
    }

    fn account_scope(suffix: u8) -> StorageScope {
        StorageScope::account(TEST_ORIGIN, &account_id(suffix)).unwrap()
    }

    fn password_ref(profile_id: &str) -> String {
        CredentialReference::password(profile_id).unwrap().target()
    }

    fn passphrase_ref(profile_id: &str) -> String {
        CredentialReference::key_passphrase(profile_id)
            .unwrap()
            .target()
    }

    fn response_for(
        snapshot: &SshSyncSnapshot,
        status: SshMutationStatus,
        next_cursor: i64,
        changes: Vec<SshSyncChange>,
    ) -> SshSyncResponse {
        SshSyncResponse {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            acks: snapshot
                .request()
                .mutations
                .iter()
                .enumerate()
                .map(|(index, mutation)| SshMutationAck {
                    mutation_id: mutation.mutation_id.clone(),
                    status,
                    revision: Some(index as i64 + 1),
                    error_code: None,
                })
                .collect(),
            changes,
            next_cursor,
            has_more: false,
        }
    }

    fn remote_group(
        id_digit: char,
        name: &str,
        sort_order: u32,
    ) -> lumen_protocol::ssh_sync::SshGroup {
        lumen_protocol::ssh_sync::SshGroup {
            id: format!("grp_{}", id_digit.to_string().repeat(32)),
            name: name.to_owned(),
            sort_order,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn remote_profile(
        id_digit: char,
        name: &str,
        group_id: Option<String>,
    ) -> lumen_protocol::ssh_sync::SshProfile {
        lumen_protocol::ssh_sync::SshProfile {
            id: format!("ssh_{}", id_digit.to_string().repeat(32)),
            name: name.to_owned(),
            host: "remote.example.com".to_owned(),
            port: 22,
            username: "lumen".to_owned(),
            auth_method: lumen_protocol::ssh_sync::SshAuthMethod::PrivateKey,
            group_id,
            sort_order: 0,
            initial_directory: Some("/srv".to_owned()),
            connect_timeout_secs: 15,
            keep_alive_secs: Some(30),
            monitor_enabled: true,
            trusted_host_key: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn change(revision: i64, change: SshChange) -> SshSyncChange {
        SshSyncChange {
            revision,
            updated_by_device: "device-remote".to_owned(),
            change,
        }
    }

    #[test]
    fn 离线编辑重启后outbox仍在且mutation_id稳定() {
        let root = temporary_root("offline-outbox");
        let scope = account_scope(10);
        let mut store = SshStore::load(&root, scope.clone()).unwrap();
        let group = store.create_group("离线").unwrap();
        store.create_profile(draft("offline", Some(group))).unwrap();
        let before = store.sync_snapshot().unwrap();
        let before_ids = before
            .request()
            .mutations
            .iter()
            .map(|mutation| mutation.mutation_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(before_ids.len(), 2);
        drop(store);

        let loaded = SshStore::load(&root, scope).unwrap();
        let after_ids = loaded
            .sync_snapshot()
            .unwrap()
            .request()
            .mutations
            .iter()
            .map(|mutation| mutation.mutation_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(after_ids, before_ids);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 服务端确认后才清理outbox且duplicate也幂等确认() {
        let root = temporary_root("ack-cleanup");
        let mut store = SshStore::load(&root, account_scope(11)).unwrap();
        store.create_group("确认").unwrap();
        let snapshot = store.sync_snapshot().unwrap();
        assert_eq!(store.pending_sync_mutations(), 1);
        let confirmed_group = match &snapshot.request().mutations[0].operation {
            SshMutationOperation::UpsertGroup { group } => group.clone(),
            _ => panic!("expected group upsert"),
        };

        let response = response_for(
            &snapshot,
            SshMutationStatus::Duplicate,
            1,
            vec![change(
                1,
                SshChange::UpsertGroup {
                    group: confirmed_group,
                },
            )],
        );
        let report = store.apply_sync_response(&snapshot, response).unwrap();
        assert_eq!(report.acknowledged, 1);
        assert_eq!(store.pending_sync_mutations(), 0);
        assert_eq!(store.server_cursor(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 远端增删改与删除分组归未分组按revision应用() {
        let root = temporary_root("remote-merge");
        let mut store = SshStore::load(&root, account_scope(12)).unwrap();
        let first = store.sync_snapshot().unwrap();
        let group = remote_group('a', "生产", 0);
        let profile = remote_profile('b', "web-old", Some(group.id.clone()));
        let response = response_for(
            &first,
            SshMutationStatus::Applied,
            2,
            vec![
                change(
                    1,
                    SshChange::UpsertGroup {
                        group: group.clone(),
                    },
                ),
                change(
                    2,
                    SshChange::UpsertProfile {
                        profile: profile.clone(),
                    },
                ),
            ],
        );
        store.apply_sync_response(&first, response).unwrap();
        assert_eq!(
            store.inventory().profile(&profile.id).unwrap().name,
            "web-old"
        );

        let second = store.sync_snapshot().unwrap();
        let mut updated = profile.clone();
        updated.name = "web-new".to_owned();
        updated.updated_at_ms = 3;
        let response = response_for(
            &second,
            SshMutationStatus::Applied,
            4,
            vec![
                change(
                    3,
                    SshChange::UpsertProfile {
                        profile: updated.clone(),
                    },
                ),
                change(
                    4,
                    SshChange::DeleteGroup {
                        group_id: group.id.clone(),
                    },
                ),
            ],
        );
        store.apply_sync_response(&second, response).unwrap();
        let local = store.inventory().profile(&profile.id).unwrap();
        assert_eq!(local.name, "web-new");
        assert_eq!(local.group_id, None);

        let third = store.sync_snapshot().unwrap();
        let response = response_for(
            &third,
            SshMutationStatus::Applied,
            5,
            vec![change(
                5,
                SshChange::DeleteProfile {
                    profile_id: profile.id.clone(),
                },
            )],
        );
        store.apply_sync_response(&third, response).unwrap();
        assert!(store.inventory().profile(&profile.id).is_none());
        assert_eq!(store.server_cursor(), 5);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unclaimed只可由首次认领origin与账号导入() {
        let root = temporary_root("claim-isolation");
        let mut local = SshStore::load(&root, StorageScope::Local).unwrap();
        let profile_id = local.create_profile(draft("local", None)).unwrap();
        local
            .upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: Some(root.join("keys").join("id_ed25519")),
                password_credential_ref: Some(password_ref(&profile_id)),
                key_passphrase_credential_ref: None,
            })
            .unwrap();
        drop(local);

        let account_a =
            SshStore::load_account_claiming_unclaimed(&root, TEST_ORIGIN, &account_id(13)).unwrap();
        assert!(account_a.inventory().profile(&profile_id).is_some());
        assert!(account_a.binding(&profile_id).is_some());
        assert!(account_a.pending_sync_mutations() > 0);
        let claimed_local = SshStore::load(&root, StorageScope::Local).unwrap();
        assert!(
            claimed_local.sync.claimed_account_id.is_none(),
            "转移完成后必须清除临时认领标记，允许后续全新的本地清单独立存在"
        );
        assert!(
            claimed_local.inventory().profiles().is_empty(),
            "认领成功后 unclaimed 不得保留会复活的服务器副本"
        );
        assert!(
            claimed_local.bindings().is_empty(),
            "Credential Manager target 必须只有账号 store 一个活动 owner"
        );

        let account_b =
            SshStore::load_account_claiming_unclaimed(&root, TEST_ORIGIN, &account_id(14)).unwrap();
        assert!(account_b.inventory().profiles().is_empty());
        assert!(account_b.bindings().is_empty());
        let other_origin = SshStore::load_account_claiming_unclaimed(
            &root,
            "https://other.example",
            &account_id(13),
        )
        .unwrap();
        assert!(other_origin.inventory().profiles().is_empty());
        assert!(other_origin.bindings().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 认领在账号提交后中断会核对并完成单owner清理() {
        let root = temporary_root("claim-recovery");
        let account_id = account_id(23);
        let requested_claim = unclaimed_scope_marker(TEST_ORIGIN, &account_id);
        let mut local = SshStore::load(&root, StorageScope::Local).unwrap();
        let profile_id = local.create_profile(draft("recover", None)).unwrap();
        local
            .upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: None,
                password_credential_ref: Some(password_ref(&profile_id)),
                key_passphrase_credential_ref: None,
            })
            .unwrap();

        // 模拟认领标记已提交、账号 generation 也已提交，但进程在清空
        // unclaimed 前退出。
        let imported_inventory = local.inventory.clone();
        let imported_bindings = local.bindings.clone();
        let mut claimed_sync = local.sync.clone();
        claimed_sync.claimed_account_id = Some(requested_claim);
        local
            .commit_state(
                imported_inventory.clone(),
                imported_bindings.clone(),
                claimed_sync,
            )
            .unwrap();
        let scope = StorageScope::account(TEST_ORIGIN, &account_id).unwrap();
        let mut account = SshStore::load(&root, scope).unwrap();
        let mut imported_sync = SyncJournal::default();
        enqueue_inventory_diff(
            &mut imported_sync,
            &SshInventory::default(),
            &imported_inventory,
        )
        .unwrap();
        account
            .commit_state(imported_inventory, imported_bindings, imported_sync)
            .unwrap();
        drop(account);
        drop(local);

        let recovered =
            SshStore::load_account_claiming_unclaimed(&root, TEST_ORIGIN, &account_id).unwrap();
        assert!(recovered.inventory().profile(&profile_id).is_some());
        assert!(recovered.binding(&profile_id).is_some());
        let local = SshStore::load(&root, StorageScope::Local).unwrap();
        assert!(local.inventory().profiles().is_empty());
        assert!(local.bindings().is_empty());
        assert!(local.sync.claimed_account_id.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 认领完成后新建的本地清单不阻断已有账号重登() {
        let root = temporary_root("post-claim-local");
        let account_id = account_id(24);
        let mut local = SshStore::load(&root, StorageScope::Local).unwrap();
        let claimed_profile = local.create_profile(draft("claimed", None)).unwrap();
        drop(local);

        let account =
            SshStore::load_account_claiming_unclaimed(&root, TEST_ORIGIN, &account_id).unwrap();
        assert!(account.inventory().profile(&claimed_profile).is_some());
        drop(account);

        // 模拟登出后在 Local 作用域新建另一台服务器。账号缓存已经存在，
        // 再次登录同账号时不应把新 Local 数据误判成崩溃恢复副本。
        let mut local = SshStore::load(&root, StorageScope::Local).unwrap();
        let local_profile = local
            .create_profile(draft("local-after-logout", None))
            .unwrap();
        drop(local);
        let account =
            SshStore::load_account_claiming_unclaimed(&root, TEST_ORIGIN, &account_id).unwrap();
        assert!(account.inventory().profile(&claimed_profile).is_some());
        assert!(account.inventory().profile(&local_profile).is_none());
        let local = SshStore::load(&root, StorageScope::Local).unwrap();
        assert!(local.inventory().profile(&local_profile).is_some());
        assert!(local.sync.claimed_account_id.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 认领事务持有双锁且并发local写不能打断凭据owner转移() {
        use std::sync::mpsc;
        use std::time::Duration;

        let root = temporary_root("claim-cross-scope-lock");
        let account_id = account_id(25);
        let mut local = SshStore::load(&root, StorageScope::Local).unwrap();
        let profile_id = local
            .create_profile(draft("claimed-with-secret", None))
            .unwrap();
        let credential_reference = password_ref(&profile_id);
        local
            .upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: None,
                password_credential_ref: Some(credential_reference.clone()),
                key_passphrase_credential_ref: None,
            })
            .unwrap();
        drop(local);

        // 模拟另一个已登出实例在认领开始前加载了 Local generation，并在
        // 账号 binding 已提交、Local 尚未清空的最危险窗口尝试删除 profile。
        let mut stale_local = SshStore::load(&root, StorageScope::Local).unwrap();
        let stale_profile_id = profile_id.clone();
        let (start_tx, start_rx) = mpsc::channel();
        let (attempting_tx, attempting_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            attempting_tx.send(()).unwrap();
            let result = stale_local.delete_profile(&stale_profile_id);
            result_tx.send(result).unwrap();
        });

        let claimed = SshStore::load_account_claiming_unclaimed_with_hook(
            &root,
            TEST_ORIGIN,
            &account_id,
            |local_directory, account_directory| {
                start_tx.send(()).unwrap();
                attempting_rx.recv().unwrap();

                // 钩子执行时双锁仍由认领线程持有；独立 handle 的非阻塞加锁
                // 必须失败，证明普通 Local writer 没有可插入的 owner 重叠窗口。
                for directory in [local_directory, account_directory] {
                    let probe = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(directory.join(".write.lock"))
                        .unwrap();
                    assert!(probe.try_lock().is_err());
                }
                assert!(matches!(
                    result_rx.try_recv(),
                    Err(mpsc::TryRecvError::Empty)
                ));
            },
        )
        .unwrap();

        let writer_result = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("认领释放锁后并发 writer 应完成");
        assert!(matches!(
            writer_result,
            Err(StoreError::ConcurrentModification { .. })
        ));
        writer.join().unwrap();

        assert_eq!(
            claimed
                .binding(&profile_id)
                .and_then(|binding| binding.password_credential_ref.as_deref()),
            Some(credential_reference.as_str())
        );
        let local = SshStore::load(&root, StorageScope::Local).unwrap();
        assert!(local.inventory().profile(&profile_id).is_none());
        assert!(local.binding(&profile_id).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 网络快照与同步journal绝不序列化本机秘密() {
        let root = temporary_root("sync-no-secrets");
        let scope = account_scope(15);
        let mut store = SshStore::load(&root, scope.clone()).unwrap();
        let profile_id = store.create_profile(draft("safe", None)).unwrap();
        let password_reference = password_ref(&profile_id);
        let passphrase_reference = passphrase_ref(&profile_id);
        store
            .upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: Some(root.join("PRIVATE_PATH_SENTINEL")),
                password_credential_ref: Some(password_reference.clone()),
                key_passphrase_credential_ref: Some(passphrase_reference.clone()),
            })
            .unwrap();
        let request_json = serde_json::to_string(store.sync_snapshot().unwrap().request()).unwrap();
        let directory = scope.directory(&root).unwrap();
        let generation = read_current_generation(&directory).unwrap().unwrap();
        let journal_json = fs::read_to_string(
            directory
                .join("generations")
                .join(generation)
                .join("sync_journal.json"),
        )
        .unwrap();
        for serialized in [&request_json, &journal_json] {
            assert!(!serialized.contains(&password_reference));
            assert!(!serialized.contains(&passphrase_reference));
            for forbidden in [
                "PRIVATE_PATH_SENTINEL",
                "private_key_path",
                "password_credential_ref",
                "key_passphrase_credential_ref",
                "passphrase",
            ] {
                assert!(!serialized.contains(forbidden), "{forbidden}");
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 陈旧回包可幂等确认但不能回退cursor或覆盖新值() {
        let root = temporary_root("stale-response");
        let mut store = SshStore::load(&root, account_scope(16)).unwrap();
        let group_id = store.create_group("旧名").unwrap();
        let stale_snapshot = store.sync_snapshot().unwrap();
        let old_group = match &stale_snapshot.request().mutations[0].operation {
            SshMutationOperation::UpsertGroup { group } => group.clone(),
            _ => panic!("expected group upsert"),
        };
        store.rename_group(&group_id, "新名").unwrap();
        let current_snapshot = store.sync_snapshot().unwrap();
        let current_group = wire_group(store.inventory().group(&group_id).unwrap());
        let current_response = response_for(
            &current_snapshot,
            SshMutationStatus::Applied,
            2,
            vec![change(
                2,
                SshChange::UpsertGroup {
                    group: current_group,
                },
            )],
        );
        store
            .apply_sync_response(&current_snapshot, current_response)
            .unwrap();

        let stale_response = SshSyncResponse {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            acks: vec![SshMutationAck {
                mutation_id: stale_snapshot.request().mutations[0].mutation_id.clone(),
                status: SshMutationStatus::Rejected,
                revision: None,
                error_code: Some("deleted_entity".to_owned()),
            }],
            changes: vec![change(1, SshChange::UpsertGroup { group: old_group })],
            next_cursor: 1,
            has_more: false,
        };
        let report = store
            .apply_sync_response(&stale_snapshot, stale_response)
            .unwrap();
        assert!(report.stale_response);
        assert_eq!(store.server_cursor(), 2);
        assert_eq!(store.inventory().group(&group_id).unwrap().name, "新名");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 请求期间的本地新编辑保护记录并在后续确认后收敛() {
        let root = temporary_root("pending-record-merge");
        let mut store = SshStore::load(&root, account_scope(17)).unwrap();
        let group_id = store.create_group("第一次").unwrap();
        let first_snapshot = store.sync_snapshot().unwrap();
        let first_group = match &first_snapshot.request().mutations[0].operation {
            SshMutationOperation::UpsertGroup { group } => group.clone(),
            _ => panic!("expected group upsert"),
        };

        store.rename_group(&group_id, "本地更新").unwrap();
        let first_response = response_for(
            &first_snapshot,
            SshMutationStatus::Applied,
            1,
            vec![change(1, SshChange::UpsertGroup { group: first_group })],
        );
        let report = store
            .apply_sync_response(&first_snapshot, first_response)
            .unwrap();
        assert_eq!(store.inventory().group(&group_id).unwrap().name, "本地更新");
        assert_eq!(store.pending_sync_mutations(), 1);
        assert_eq!(report.deferred_changes, 1);

        let second_snapshot = store.sync_snapshot().unwrap();
        let latest_group = wire_group(store.inventory().group(&group_id).unwrap());
        let second_response = response_for(
            &second_snapshot,
            SshMutationStatus::Applied,
            2,
            vec![change(
                2,
                SshChange::UpsertGroup {
                    group: latest_group,
                },
            )],
        );
        let report = store
            .apply_sync_response(&second_snapshot, second_response)
            .unwrap();
        assert_eq!(store.inventory().group(&group_id).unwrap().name, "本地更新");
        assert_eq!(store.pending_sync_mutations(), 0);
        assert_eq!(report.deferred_changes, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 非法服务端游标和ack形态不会污染本地同步状态() {
        let root = temporary_root("invalid-sync-response");
        let mut store = SshStore::load(&root, account_scope(18)).unwrap();
        let pristine = store.sync_snapshot().unwrap();
        let future_without_change = SshSyncResponse {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            acks: Vec::new(),
            changes: Vec::new(),
            next_cursor: 99,
            has_more: false,
        };
        assert!(store
            .apply_sync_response(&pristine, future_without_change)
            .is_err());
        assert_eq!(store.server_cursor(), 0);

        store.create_group("待确认").unwrap();
        let pending = store.sync_snapshot().unwrap();
        let malformed_ack = SshSyncResponse {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            acks: pending
                .request()
                .mutations
                .iter()
                .map(|mutation| SshMutationAck {
                    mutation_id: mutation.mutation_id.clone(),
                    status: SshMutationStatus::Applied,
                    revision: None,
                    error_code: Some("unexpected_error".to_owned()),
                })
                .collect(),
            changes: Vec::new(),
            next_cursor: 0,
            has_more: false,
        };
        assert!(store.apply_sync_response(&pending, malformed_ack).is_err());
        assert_eq!(store.pending_sync_mutations(), 1);
        assert_eq!(store.server_cursor(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 库存与本地绑定分文件且可往返加载() {
        let root = temporary_root("roundtrip");
        let scope =
            StorageScope::account(TEST_ORIGIN, "550E8400-E29B-41D4-A716-446655440000").unwrap();
        let mut store = SshStore::load(&root, scope.clone()).unwrap();
        assert_eq!(
            store.account_id(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        let group_id = store.create_group("生产").unwrap();
        let profile_id = store.create_profile(draft("web", Some(group_id))).unwrap();
        let key_path = root.join("keys").join("id_ed25519");
        let password_reference = password_ref(&profile_id);
        let passphrase_reference = passphrase_ref(&profile_id);
        store
            .upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: Some(key_path.clone()),
                password_credential_ref: Some(password_reference),
                key_passphrase_credential_ref: Some(passphrase_reference),
            })
            .unwrap();

        let directory = scope.directory(&root).unwrap();
        let scope_file =
            read_required_json::<AccountScopeFile>(&directory.join("scope.json")).unwrap();
        assert_eq!(
            scope_file,
            AccountScopeFile {
                version: SCOPE_FORMAT_VERSION,
                server_origin: TEST_ORIGIN.to_owned(),
                account_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            }
        );
        assert!(directory.join("current.json").is_file());
        let generation = read_current_generation(&directory).unwrap().unwrap();
        assert!(directory
            .join("generations")
            .join(&generation)
            .join("sync_state.json")
            .is_file());
        assert!(directory
            .join("generations")
            .join(&generation)
            .join("local_bindings.json")
            .is_file());
        assert!(directory
            .join("generations")
            .join(generation)
            .join("sync_journal.json")
            .is_file());
        let loaded = SshStore::load(&root, scope).unwrap();
        assert_eq!(loaded.inventory().profiles().len(), 1);
        assert_eq!(
            loaded.binding(&profile_id).unwrap().private_key_path,
            Some(key_path)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 可同步dto与库存文件绝不含本机秘密() {
        let root = temporary_root("no-secrets");
        let scope = StorageScope::Local;
        let mut store = SshStore::load(&root, scope.clone()).unwrap();
        let profile_id = store.create_profile(draft("safe", None)).unwrap();
        let password_reference = password_ref(&profile_id);
        let passphrase_reference = passphrase_ref(&profile_id);
        store
            .upsert_binding(SshLocalBinding {
                profile_id,
                private_key_path: Some(root.join("PRIVATE_KEY_PATH_SENTINEL")),
                password_credential_ref: Some(password_reference.clone()),
                key_passphrase_credential_ref: Some(passphrase_reference.clone()),
            })
            .unwrap();

        let sync_json = serde_json::to_string(&store.inventory().sync_dto()).unwrap();
        let directory = scope.directory(&root).unwrap();
        let generation = read_current_generation(&directory).unwrap().unwrap();
        let inventory_json = fs::read_to_string(
            directory
                .join("generations")
                .join(generation)
                .join("sync_state.json"),
        )
        .unwrap();
        assert!(!sync_json.contains(&password_reference));
        assert!(!sync_json.contains(&passphrase_reference));
        assert!(!inventory_json.contains(&password_reference));
        assert!(!inventory_json.contains(&passphrase_reference));
        for forbidden in [
            "PRIVATE_KEY_PATH_SENTINEL",
            "private_key_path",
            "credential_ref",
            "password",
            "passphrase",
        ] {
            assert!(!sync_json.contains(forbidden), "{forbidden}");
            assert!(!inventory_json.contains(forbidden), "{forbidden}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 不同账号读写完全隔离且重复保存可替换() {
        let root = temporary_root("isolation");
        let scope_a =
            StorageScope::account(TEST_ORIGIN, "550e8400-e29b-41d4-a716-446655440000").unwrap();
        let scope_b =
            StorageScope::account(TEST_ORIGIN, "550e8400-e29b-41d4-a716-446655440001").unwrap();
        let mut a = SshStore::load(&root, scope_a.clone()).unwrap();
        a.create_group("A-1").unwrap();
        a.create_group("A-2").unwrap();
        let b = SshStore::load(&root, scope_b.clone()).unwrap();

        assert_eq!(a.inventory().groups().len(), 2);
        assert!(b.inventory().groups().is_empty());
        assert_ne!(
            scope_a.directory(&root).unwrap(),
            scope_b.directory(&root).unwrap()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 相同账号在不同origin的库存和游标完全隔离() {
        let root = temporary_root("origin-isolation");
        let account_id = "550e8400-e29b-41d4-a716-446655440000";
        let scope_a = StorageScope::account("https://one.example", account_id).unwrap();
        let scope_b = StorageScope::account("https://two.example", account_id).unwrap();
        let mut first = SshStore::load(&root, scope_a.clone()).unwrap();
        let snapshot = first.sync_snapshot().unwrap();
        let response = response_for(
            &snapshot,
            SshMutationStatus::Applied,
            7,
            vec![change(
                7,
                SshChange::UpsertGroup {
                    group: remote_group('c', "only-a", 0),
                },
            )],
        );
        first.apply_sync_response(&snapshot, response).unwrap();
        let second = SshStore::load(&root, scope_b.clone()).unwrap();

        assert_eq!(first.server_origin(), Some("https://one.example"));
        assert_eq!(second.server_origin(), Some("https://two.example"));
        assert_eq!(first.account_id(), second.account_id());
        assert_eq!(first.inventory().groups().len(), 1);
        assert_eq!(first.server_cursor(), 7);
        assert!(second.inventory().groups().is_empty());
        assert_eq!(second.server_cursor(), 0);
        assert_ne!(
            scope_a.directory(&root).unwrap(),
            scope_b.directory(&root).unwrap()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scope元数据被篡改后加载与保存均fail_closed() {
        let root = temporary_root("scope-tamper");
        let scope = account_scope(22);
        let mut loaded = SshStore::load(&root, scope.clone()).unwrap();
        loaded.create_group("before-tamper").unwrap();
        let scope_path = scope.directory(&root).unwrap().join("scope.json");
        atomic_write_json(
            &scope_path,
            &AccountScopeFile {
                version: SCOPE_FORMAT_VERSION,
                server_origin: "https://attacker.example".to_owned(),
                account_id: account_id(22),
            },
        )
        .unwrap();

        assert!(matches!(
            SshStore::load(&root, scope),
            Err(StoreError::InvalidScope(_))
        ));
        assert!(matches!(
            loaded.create_group("after-tamper"),
            Err(StoreError::InvalidScope(_))
        ));
        assert_eq!(loaded.inventory().groups().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 变更保存失败时不提交内存() {
        let root = temporary_root("transaction");
        fs::write(&root, "not-a-directory").unwrap();
        let mut store = SshStore {
            directory: root.clone(),
            server_origin: None,
            account_id: None,
            generation: None,
            inventory: SshInventory::default(),
            bindings: Vec::new(),
            sync: SyncJournal::default(),
        };
        assert!(store.create_group("不会提交").is_err());
        assert!(store.inventory().groups().is_empty());
        fs::remove_file(root).unwrap();
    }

    #[test]
    fn 陈旧实例保存会被generation检查拒绝() {
        let root = temporary_root("concurrent");
        let scope = StorageScope::Local;
        let mut first = SshStore::load(&root, scope.clone()).unwrap();
        let mut stale = SshStore::load(&root, scope).unwrap();

        first.create_group("first").unwrap();
        assert!(matches!(
            stale.create_group("stale"),
            Err(StoreError::ConcurrentModification { .. })
        ));
        assert!(stale.inventory().groups().is_empty());

        let loaded = SshStore::load(&root, StorageScope::Local).unwrap();
        assert_eq!(loaded.inventory().groups().len(), 1);
        assert_eq!(loaded.inventory().groups()[0].name, "first");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 并发实例写入由作用域文件锁串行且只允许一个generation胜出() {
        let root = temporary_root("concurrent-lock");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = ["first", "second"].map(|name| {
            let root = root.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut store = SshStore::load(&root, StorageScope::Local).unwrap();
                barrier.wait();
                store.create_group(name)
            })
        });
        let results = handles.map(|handle| handle.join().expect("写入线程不应 panic"));
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(StoreError::ConcurrentModification { .. })))
                .count(),
            1
        );
        let loaded = SshStore::load(&root, StorageScope::Local).unwrap();
        assert_eq!(loaded.inventory().groups().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 加载拒绝重复与孤儿本地绑定() {
        let root = temporary_root("invalid-bindings");
        let directory = StorageScope::Local.directory(&root).unwrap();
        fs::create_dir_all(&directory).unwrap();
        let mut inventory = SshInventory::default();
        let profile_id = inventory.create_profile(draft("safe", None)).unwrap();
        atomic_write_json(
            &directory.join("sync_state.json"),
            &InventoryFile {
                version: INVENTORY_FORMAT_VERSION,
                inventory,
            },
        )
        .unwrap();
        let reference = password_ref(&profile_id);
        let binding = SshLocalBinding {
            profile_id,
            private_key_path: None,
            password_credential_ref: Some(reference),
            key_passphrase_credential_ref: None,
        };
        atomic_write_json(
            &directory.join("local_bindings.json"),
            &BindingsFile {
                version: BINDINGS_FORMAT_VERSION,
                bindings: vec![binding.clone(), binding],
            },
        )
        .unwrap();
        assert!(matches!(
            SshStore::load(&root, StorageScope::Local),
            Err(StoreError::InvalidBinding(_))
        ));

        let orphan = SshLocalBinding {
            profile_id: "ssh_00000000000000000000000000000000".to_owned(),
            private_key_path: None,
            password_credential_ref: None,
            key_passphrase_credential_ref: None,
        };
        atomic_write_json(
            &directory.join("local_bindings.json"),
            &BindingsFile {
                version: BINDINGS_FORMAT_VERSION,
                bindings: vec![orphan],
            },
        )
        .unwrap();
        assert!(matches!(
            SshStore::load(&root, StorageScope::Local),
            Err(StoreError::InvalidBinding(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 本机凭据引用必须匹配服务器与用途() {
        let root = temporary_root("credential-reference-scope");
        let mut store = SshStore::load(&root, StorageScope::Local).unwrap();
        let profile_id = store.create_profile(draft("one", None)).unwrap();
        let other_profile_id = store.create_profile(draft("two", None)).unwrap();

        for invalid_reference in [
            password_ref(&other_profile_id),
            passphrase_ref(&profile_id),
            "not-a-credential-reference".to_owned(),
        ] {
            let result = store.upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: None,
                password_credential_ref: Some(invalid_reference),
                key_passphrase_credential_ref: None,
            });
            assert!(matches!(result, Err(StoreError::InvalidBinding(_))));
        }
        assert!(store.binding(&profile_id).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 修改认证方式或endpoint会原子清理不再适用的本机绑定() {
        let root = temporary_root("sanitize-local-binding");
        let mut store = SshStore::load(&root, StorageScope::Local).unwrap();
        let profile_id = store.create_profile(draft("one", None)).unwrap();
        let password_reference = password_ref(&profile_id);
        let passphrase_reference = passphrase_ref(&profile_id);
        let key_path = root.join("keys").join("id_ed25519");
        store
            .upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: Some(key_path.clone()),
                password_credential_ref: Some(password_reference),
                key_passphrase_credential_ref: Some(passphrase_reference.clone()),
            })
            .unwrap();

        let mut private_key = draft("one", None);
        private_key.auth_method = AuthMethod::PrivateKey;
        store.update_profile(&profile_id, private_key).unwrap();
        let binding = store.binding(&profile_id).unwrap();
        assert_eq!(binding.private_key_path.as_ref(), Some(&key_path));
        assert!(binding.password_credential_ref.is_none());
        assert_eq!(
            binding.key_passphrase_credential_ref.as_deref(),
            Some(passphrase_reference.as_str())
        );

        let mut agent = draft("one", None);
        agent.auth_method = AuthMethod::Agent;
        store.update_profile(&profile_id, agent).unwrap();
        assert!(store.binding(&profile_id).is_none());

        let second_id = store.create_profile(draft("two", None)).unwrap();
        store
            .upsert_binding(SshLocalBinding {
                profile_id: second_id.clone(),
                private_key_path: None,
                password_credential_ref: Some(password_ref(&second_id)),
                key_passphrase_credential_ref: None,
            })
            .unwrap();
        let mut moved_endpoint = draft("two", None);
        moved_endpoint.host = "new.example.test".to_owned();
        store.update_profile(&second_id, moved_endpoint).unwrap();
        assert!(store.binding(&second_id).is_none());
        let _ = fs::remove_dir_all(root);
    }
}
