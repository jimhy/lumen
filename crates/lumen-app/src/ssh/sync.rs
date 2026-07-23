use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError};
use lumen_protocol::ssh_sync::{
    SshAuthMethod, SshChange, SshGroup as WireGroup, SshHostKeyTrust, SshMutation,
    SshMutationOperation, SshProfile as WireProfile, SshSyncChange, SshSyncRequest,
    SshSyncResponse, SSH_SYNC_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};

use crate::cloud::{CloudClient, CloudError};

use super::model::{AuthMethod, HostKeyTrust, SshGroup, SshProfile};

pub(super) const SYNC_JOURNAL_FORMAT_VERSION: u32 = 1;
const SYNC_BATCH_LIMIT: u16 = 200;
/// 服务端全局 body 上限为 1 MiB；留出约 10% 余量给 schema/游标等 JSON 外壳。
const SYNC_REQUEST_BUDGET_BYTES: usize = 900 * 1024;
const SYNC_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

/// 与库存、认证绑定一起写入同一 generation 的同步状态。
///
/// 这里只允许放协议 allowlist 类型。认证材料属于 `SshLocalBinding`，不得进入本结构。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct SyncJournal {
    pub server_cursor: i64,
    pub outbox: Vec<SshMutation>,
    pub deferred_changes: Vec<SshSyncChange>,
    /// 未登录清单一旦被某账号认领，就永久绑定该账号，防止之后登录另一个账号时误导入。
    pub claimed_account_id: Option<String>,
}

/// 后台线程可安全持有的脱敏请求快照。
#[derive(Clone, PartialEq, Eq)]
pub struct SshSyncSnapshot {
    account_id: String,
    request: SshSyncRequest,
}

impl fmt::Debug for SshSyncSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshSyncSnapshot")
            .field("account_id", &self.account_id)
            .field("cursor", &self.request.cursor)
            .field("mutation_count", &self.request.mutations.len())
            .finish()
    }
}

impl SshSyncSnapshot {
    pub(super) fn new(account_id: String, journal: &SyncJournal) -> Self {
        let mut request = SshSyncRequest {
            schema_version: SSH_SYNC_SCHEMA_VERSION,
            cursor: journal.server_cursor,
            limit: SYNC_BATCH_LIMIT,
            mutations: Vec::new(),
        };
        let mut encoded_bytes = serde_json::to_vec(&request).map_or(0, |bytes| bytes.len());
        for mutation in journal.outbox.iter().take(usize::from(SYNC_BATCH_LIMIT)) {
            let mutation_bytes =
                serde_json::to_vec(mutation).map_or(usize::MAX, |bytes| bytes.len());
            let separator_bytes = usize::from(!request.mutations.is_empty());
            let next_bytes = encoded_bytes
                .saturating_add(separator_bytes)
                .saturating_add(mutation_bytes);
            if next_bytes > SYNC_REQUEST_BUDGET_BYTES && !request.mutations.is_empty() {
                break;
            }
            request.mutations.push(mutation.clone());
            encoded_bytes = next_bytes;
            // 所有经库存校验的单条 mutation 都远小于预算；若未来 schema 扩展打破
            // 该前提，仍发送队首一条，让服务端返回明确错误而不是永久饿死队头。
            if encoded_bytes > SYNC_REQUEST_BUDGET_BYTES {
                break;
            }
        }
        Self {
            account_id,
            request,
        }
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn request(&self) -> &SshSyncRequest {
        &self.request
    }
}

/// 一个 HTTP 同步请求的成功结果。必须交给持有 `SshStore` 的主线程应用。
pub struct SshSyncCompleted {
    pub snapshot: SshSyncSnapshot,
    pub response: SshSyncResponse,
}

impl fmt::Debug for SshSyncCompleted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshSyncCompleted")
            .field("snapshot", &self.snapshot)
            .field("ack_count", &self.response.acks.len())
            .field("change_count", &self.response.changes.len())
            .field("next_cursor", &self.response.next_cursor)
            .field("has_more", &self.response.has_more)
            .finish()
    }
}

/// 后台请求失败。错误不包含请求/响应正文。
#[derive(Debug)]
pub struct SshSyncFailed {
    pub account_id: String,
    pub error: CloudError,
}

#[derive(Debug)]
pub enum SshSyncEvent {
    Completed(SshSyncCompleted),
    Failed(SshSyncFailed),
}

/// 事件成功入队后调用的无参数唤醒器。可捕获 winit `EventLoopProxy`，数据层本身
/// 不依赖 winit，也不会把请求、响应或错误正文交给回调。
pub type SshSyncNotifier = Arc<dyn Fn() + Send + Sync + 'static>;

/// SSH 配置同步 worker。
///
/// worker 只持有 [`SshSyncSnapshot`]，不会打开数据目录或写 `SshStore`。本地编辑后调用
/// [`Self::update_snapshot_and_trigger`]；应用回包后调用 [`Self::update_snapshot`]，仅在
/// `has_more` 或仍有 outbox 时再 [`Self::trigger`]。所有事件都在 UI owner 线程消费。
///
/// 登出/切账号时，调用方应先清空共享 token，再 drop worker；已在途 HTTP 无法取消，
/// 但 stop 标记会阻止该 worker 发布回包或发起后续请求。
pub struct SshSyncWorker {
    latest_snapshot: Arc<RwLock<SshSyncSnapshot>>,
    wake: Sender<()>,
    events: Receiver<SshSyncEvent>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl fmt::Debug for SshSyncWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshSyncWorker")
            .field("running", &self.join.is_some())
            .finish()
    }
}

impl SshSyncWorker {
    /// 启动后立即同步，之后每 30 秒同步一次；网络失败按 2/5/10/30/60 秒退避。
    pub fn start(
        base_url: String,
        token: Arc<RwLock<String>>,
        initial_snapshot: SshSyncSnapshot,
    ) -> std::io::Result<Self> {
        Self::start_with_notifier(base_url, token, initial_snapshot, None)
    }

    /// 与 [`Self::start`] 相同，但每个事件成功入队后调用一次 notifier。
    pub fn start_with_notifier(
        base_url: String,
        token: Arc<RwLock<String>>,
        initial_snapshot: SshSyncSnapshot,
        notifier: Option<SshSyncNotifier>,
    ) -> std::io::Result<Self> {
        let latest_snapshot = Arc::new(RwLock::new(initial_snapshot));
        // latest-wins：快照放共享槽，容量 1 的 channel 只表示“有新工作”，拖放不会
        // 排队复制完整清单。
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        // owner 暂时卡顿时最多保留少量事件；Full 时丢事件并依赖幂等周期重试。
        let (event_tx, event_rx) = crossbeam_channel::bounded(8);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_snapshot = Arc::clone(&latest_snapshot);
        let worker_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("lumen-ssh-sync".to_owned())
            .spawn(move || {
                worker_loop(
                    &base_url,
                    &token,
                    &worker_snapshot,
                    &wake_rx,
                    &event_tx,
                    &worker_stop,
                    notifier.as_ref(),
                );
            })?;
        Ok(Self {
            latest_snapshot,
            wake: wake_tx,
            events: event_rx,
            stop,
            join: Some(join),
        })
    }

    /// 只替换 worker 持有的最新脱敏快照，不立即请求。
    ///
    /// 应用成功回包后用本方法刷新 cursor，避免“空回包 → 更新 → 立即再请求”的循环。
    pub fn update_snapshot(&self, snapshot: SshSyncSnapshot) {
        match self.latest_snapshot.write() {
            Ok(mut latest) => *latest = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    /// 替换最新快照并立即安排一次同步。本地 CRUD 成功后使用本方法。
    pub fn update_snapshot_and_trigger(&self, snapshot: SshSyncSnapshot) {
        self.update_snapshot(snapshot);
        let _ = self.wake.try_send(());
    }

    /// 立即安排一次同步（例如用户点击重试）。
    pub fn trigger(&self) {
        let _ = self.wake.try_send(());
    }

    /// 非阻塞取一个事件。主线程应循环调用直到返回 `None`。
    pub fn poll(&self) -> Option<SshSyncEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for SshSyncWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
        if let Some(join) = self.join.take() {
            // ureq 无法取消正在途中的阻塞 I/O；把 join 交给 reaper，登出/切账号/UI
            // Drop 立即返回。HTTP 有 25 秒总超时，reaper 最终必会退出。
            let _ = thread::Builder::new()
                .name("lumen-ssh-sync-reaper".to_owned())
                .spawn(move || {
                    let _ = join.join();
                });
        }
    }
}

fn worker_loop(
    base_url: &str,
    token: &Arc<RwLock<String>>,
    latest_snapshot: &Arc<RwLock<SshSyncSnapshot>>,
    wake: &Receiver<()>,
    events: &Sender<SshSyncEvent>,
    stop: &AtomicBool,
    notifier: Option<&SshSyncNotifier>,
) {
    let client = CloudClient::new(base_url.to_owned());
    let mut due = Instant::now();
    let mut retry_index = 0_usize;

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        if now < due {
            match wake.recv_timeout(due.saturating_duration_since(now)) {
                Ok(()) => {
                    due = Instant::now();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if stop.load(Ordering::Acquire) {
            return;
        }

        let current_token = token.read().map(|guard| guard.clone()).unwrap_or_default();
        if current_token.is_empty() {
            due = Instant::now() + SYNC_INTERVAL;
            continue;
        }
        let sent_snapshot = read_latest_snapshot(latest_snapshot);
        match client.sync_ssh(&current_token, sent_snapshot.request()) {
            Ok(response) => {
                // Drop/切账号发生在请求途中：服务端可能已收请求，但本 worker 不再
                // 发布回包或继续下一次上传。
                if stop.load(Ordering::Acquire) {
                    return;
                }
                retry_index = 0;
                if publish_event(
                    events,
                    notifier,
                    SshSyncEvent::Completed(SshSyncCompleted {
                        snapshot: sent_snapshot,
                        response,
                    }),
                ) == PublishResult::Disconnected
                {
                    return;
                }
                // 下一页必须等 owner 应用游标并回送新快照，避免重放旧 cursor。
                due = Instant::now() + SYNC_INTERVAL;
            }
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if publish_event(
                    events,
                    notifier,
                    SshSyncEvent::Failed(SshSyncFailed {
                        account_id: sent_snapshot.account_id,
                        error,
                    }),
                ) == PublishResult::Disconnected
                {
                    return;
                }
                let delay = RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)];
                retry_index = (retry_index + 1).min(RETRY_DELAYS.len() - 1);
                due = Instant::now() + delay;
            }
        }

        // 请求期间有再编辑时，容量 1 的信号会在这里触发一次 latest-wins 请求。
        match wake.try_recv() {
            Ok(()) => due = Instant::now(),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return,
        }
    }
}

fn read_latest_snapshot(latest: &RwLock<SshSyncSnapshot>) -> SshSyncSnapshot {
    match latest.read() {
        Ok(snapshot) => snapshot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublishResult {
    Enqueued,
    Full,
    Disconnected,
}

fn publish_event(
    events: &Sender<SshSyncEvent>,
    notifier: Option<&SshSyncNotifier>,
    event: SshSyncEvent,
) -> PublishResult {
    match events.try_send(event) {
        Ok(()) => {
            if let Some(notifier) = notifier {
                // UI 唤醒器不应因一次 panic 杀死同步线程。
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notifier()));
            }
            PublishResult::Enqueued
        }
        Err(TrySendError::Full(_)) => PublishResult::Full,
        Err(TrySendError::Disconnected(_)) => PublishResult::Disconnected,
    }
}

pub(super) fn wire_group(group: &SshGroup) -> WireGroup {
    WireGroup {
        id: group.id.clone(),
        name: group.name.clone(),
        sort_order: group.sort_order,
        created_at_ms: group.created_at_ms,
        updated_at_ms: group.updated_at_ms,
    }
}

pub(super) fn wire_profile(profile: &SshProfile) -> WireProfile {
    WireProfile {
        id: profile.id.clone(),
        name: profile.name.clone(),
        host: profile.host.clone(),
        port: profile.port,
        username: profile.username.clone(),
        auth_method: match profile.auth_method {
            AuthMethod::Password => SshAuthMethod::Password,
            AuthMethod::PrivateKey => SshAuthMethod::PrivateKey,
            AuthMethod::Agent => SshAuthMethod::Agent,
        },
        group_id: profile.group_id.clone(),
        sort_order: profile.sort_order,
        initial_directory: profile.initial_directory.clone(),
        connect_timeout_secs: profile.connect_timeout_secs,
        keep_alive_secs: profile.keep_alive_secs,
        monitor_enabled: profile.monitor_enabled,
        trusted_host_key: profile
            .trusted_host_key
            .as_ref()
            .map(|key| SshHostKeyTrust {
                algorithm: key.algorithm.clone(),
                fingerprint: key.fingerprint.clone(),
            }),
        created_at_ms: profile.created_at_ms,
        updated_at_ms: profile.updated_at_ms,
    }
}

pub(super) fn local_group(group: WireGroup) -> SshGroup {
    SshGroup {
        id: group.id,
        name: group.name,
        sort_order: group.sort_order,
        created_at_ms: group.created_at_ms,
        updated_at_ms: group.updated_at_ms,
    }
}

pub(super) fn local_profile(profile: WireProfile) -> SshProfile {
    SshProfile {
        id: profile.id,
        name: profile.name,
        host: profile.host,
        port: profile.port,
        username: profile.username,
        auth_method: match profile.auth_method {
            SshAuthMethod::Password => AuthMethod::Password,
            SshAuthMethod::PrivateKey => AuthMethod::PrivateKey,
            SshAuthMethod::Agent => AuthMethod::Agent,
        },
        group_id: profile.group_id,
        sort_order: profile.sort_order,
        initial_directory: profile.initial_directory,
        connect_timeout_secs: profile.connect_timeout_secs,
        keep_alive_secs: profile.keep_alive_secs,
        monitor_enabled: profile.monitor_enabled,
        trusted_host_key: profile.trusted_host_key.map(|key| HostKeyTrust {
            algorithm: key.algorithm,
            fingerprint: key.fingerprint,
        }),
        created_at_ms: profile.created_at_ms,
        updated_at_ms: profile.updated_at_ms,
    }
}

pub(super) fn mutation_entity(operation: &SshMutationOperation) -> (&'static str, &str) {
    match operation {
        SshMutationOperation::UpsertGroup { group } => ("group", &group.id),
        SshMutationOperation::DeleteGroup { group_id } => ("group", group_id),
        SshMutationOperation::UpsertProfile { profile } => ("profile", &profile.id),
        SshMutationOperation::DeleteProfile { profile_id } => ("profile", profile_id),
    }
}

pub(super) fn change_entity(change: &SshChange) -> (&'static str, &str) {
    match change {
        SshChange::UpsertGroup { group } => ("group", &group.id),
        SshChange::DeleteGroup { group_id } => ("group", group_id),
        SshChange::UpsertProfile { profile } => ("profile", &profile.id),
        SshChange::DeleteProfile { profile_id } => ("profile", profile_id),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    fn snapshot(cursor: i64) -> SshSyncSnapshot {
        SshSyncSnapshot {
            account_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            request: SshSyncRequest {
                schema_version: SSH_SYNC_SCHEMA_VERSION,
                cursor,
                limit: SYNC_BATCH_LIMIT,
                mutations: Vec::new(),
            },
        }
    }

    fn failed_event() -> SshSyncEvent {
        SshSyncEvent::Failed(SshSyncFailed {
            account_id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            error: CloudError::Network("offline".to_owned()),
        })
    }

    #[test]
    fn notifier仅在事件成功入队后触发() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_notifier = Arc::clone(&calls);
        let notifier: SshSyncNotifier = Arc::new(move || {
            calls_for_notifier.fetch_add(1, Ordering::SeqCst);
        });
        let (sender, receiver) = crossbeam_channel::bounded(1);

        assert!(matches!(
            publish_event(&sender, Some(&notifier), failed_event()),
            PublishResult::Enqueued
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            publish_event(&sender, Some(&notifier), failed_event()),
            PublishResult::Full
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(receiver);
        assert!(matches!(
            publish_event(&sender, Some(&notifier), failed_event()),
            PublishResult::Disconnected
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn 快照更新latest_wins且唤醒信号有界合并() {
        let latest_snapshot = Arc::new(RwLock::new(snapshot(0)));
        let (wake, _wake_receiver) = crossbeam_channel::bounded(1);
        let (_event_sender, events) = crossbeam_channel::bounded(1);
        let worker = SshSyncWorker {
            latest_snapshot: Arc::clone(&latest_snapshot),
            wake,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            join: None,
        };
        for cursor in 1..=100 {
            worker.update_snapshot_and_trigger(snapshot(cursor));
        }
        assert_eq!(read_latest_snapshot(&latest_snapshot).request.cursor, 100);
        assert_eq!(worker.wake.len(), 1);
    }

    #[test]
    fn 大字段outbox按请求字节预算有序分批且队首可发送() {
        let mut journal = SyncJournal::default();
        for index in 0..200_u32 {
            journal.outbox.push(SshMutation {
                mutation_id: format!("mut_{index:032x}"),
                base_revision: 0,
                operation: SshMutationOperation::UpsertProfile {
                    profile: WireProfile {
                        id: format!("ssh_{index:032x}"),
                        name: format!("server-{index}"),
                        host: "server.example.com".to_owned(),
                        port: 22,
                        username: "lumen".to_owned(),
                        auth_method: SshAuthMethod::PrivateKey,
                        group_id: None,
                        sort_order: index,
                        // 多字节字符覆盖“字符数合法但 UTF-8/JSON 字节数大”的边界。
                        initial_directory: Some(format!("/srv/{}", "界".repeat(4_090))),
                        connect_timeout_secs: 15,
                        keep_alive_secs: Some(30),
                        monitor_enabled: true,
                        trusted_host_key: None,
                        created_at_ms: 1,
                        updated_at_ms: 2,
                    },
                },
            });
        }

        let snapshot =
            SshSyncSnapshot::new("550e8400-e29b-41d4-a716-446655440000".to_owned(), &journal);
        assert!(!snapshot.request.mutations.is_empty());
        assert!(snapshot.request.mutations.len() < journal.outbox.len());
        assert_eq!(
            snapshot.request.mutations[0].mutation_id,
            journal.outbox[0].mutation_id
        );
        assert_eq!(
            snapshot.request.mutations.last().unwrap().mutation_id,
            journal.outbox[snapshot.request.mutations.len() - 1].mutation_id
        );
        let encoded = serde_json::to_vec(&snapshot.request).unwrap();
        assert!(encoded.len() <= SYNC_REQUEST_BUDGET_BYTES);
    }

    #[test]
    fn drop不等待慢worker完成() {
        let latest_snapshot = Arc::new(RwLock::new(snapshot(0)));
        let (wake, _wake_receiver) = crossbeam_channel::bounded(1);
        let (_event_sender, events) = crossbeam_channel::bounded(1);
        let join = thread::spawn(|| thread::sleep(Duration::from_millis(300)));
        let worker = SshSyncWorker {
            latest_snapshot,
            wake,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            join: Some(join),
        };
        let started = Instant::now();
        drop(worker);
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
