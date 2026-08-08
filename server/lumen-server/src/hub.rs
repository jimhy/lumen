//! M5.3 终端远程：WebSocket 中继的**纯内存状态机**（presence + 配对 + 控制
//! 独占 + 会话生命周期 + 数据面盲转）。
//!
//! # 为何单锁
//! [`Hub`] 把 `peers`/`sessions`/`pending` 三张表收进**一把** [`std::sync::Mutex`]
//! （[`HubState`]）。早期设计的三把独立锁需规定全局加锁顺序、稍有不慎即 ABBA
//! 死锁；单锁让每个操作在一次加锁内原子完成「检查 + 改状态 + 投递出站消息」，
//! 从根上杜绝死锁与 TOCTOU 竞态。临界区内**绝不 `.await`**（投递走
//! [`tokio::sync::mpsc::UnboundedSender::send`]，同步非阻塞），故同步锁安全。
//!
//! # 为何零 DB 依赖
//! 设备名、`last_seen` 等 DB 交互全在 `ws.rs`（连接握手阶段，锁外 `await`）完成；
//! [`Hub`] 只吃已备好的内存参数（[`Hub::register`] 收 `name`/`user_id`）。如此
//! 状态机可脱离 Postgres / axum 完整单测（见本文件 `tests`）。
//!
//! # 连接代次（conn_id）
//! 同一 `device_id` 重复连接（两个客户端实例）时，新连接**驱逐**旧连接：旧会话
//! 拆除、旧 `pending` 取消、旧连接收 [`ToClient::Close`]。每条连接持唯一递增
//! `conn_id`，[`Hub::handle`] / [`Hub::disconnect`] 以它为守卫——旧连接的迟到
//! 消息与断开清理都不会误伤已就位的新连接。
//!
//! # M7 片 4b：两个会话桶（**镜像** vs **隐藏**）
//! M6 之前「一台设备至多一条会话」是**结构性事实**而非策略：[`HubState::sessions`]
//! 以 `device_id` 为键，HashMap 的键唯一 ⇒ 一台设备最多出现在一条会话里。M7 之后
//! 手机要在桌面镜像开着的同时接入，于是仲裁按**会话种类**分成两个桶：
//!
//! ```text
//! 镜像桶（sessions，判定代码一行未改）
//!   ├─ 一台 PC 同时只被 1 个桌面控制端镜像 …… AlreadyControlled
//!   └─ 一个桌面控制端同时只控 1 台 ………………… ControllerBusy
//!
//! 隐藏桶（hidden，本片新增，判定在 open_hidden）
//!   ├─ 一台 PC 至多 MAX_HIDDEN_SESSIONS_PER_TARGET 条 …… AlreadyControlled
//!   ├─ 同一 (手机, PC) 至多 1 条 …………………………………… ControllerBusy
//!   └─ 一部手机同时只连 1 台 PC ………………………………… ControllerBusy
//!
//! 两桶之间：完全不互斥  ← 这就是「桌面镜像开着时手机可以同时接入」
//!
//! 两桶共用、完全不变：SelfControl / Offline / CrossUser（跨账户的唯一边界）
//! ```
//!
//! ## 跨桶仍互斥的**唯一**一处：`pending`
//! [`HubState::pending`] 按被控端 device_id 单槽，两类配对共用。所以手机正在与某
//! PC 配对的 [`PAIRING_TTL_SECS`] 秒里，桌面控制端请求同一台 PC 会被
//! [`DenyReason::TargetPairing`] 拒，反之亦然。
//!
//! **这是「有意未改」而不是「漏改」**：配对码是给人念的，同一时刻 PC 屏幕上只该
//! 出现一个码。后人若来「顺手把 pending 也按种类分桶」，那台 PC 上会同时挂出两个
//! 九位数字，用户念错的概率远大于并发配对带来的收益。
//!
//! ## 为什么另开一张 `hidden` 表而不是把 `sessions` 改成按会话 id 索引
//! `sessions` 是**双端双插**结构，[`relay`] 靠它做 O(1) 热路径路由——终端镜像每秒
//! 几十帧全走那里。改成按会话 id 索引要重写 `relay` / [`teardown_session`] /
//! [`establish_session`] 三处，并推翻既有的 13 个测试。为一个纯增量能力付这个代价
//! 不划算。另立门户 ⇒ **镜像路径零回归**（既有 13 个测试一个字未改，这是回归安全
//! 的硬证据）。
//!
//! ## 为什么不给 `hidden` 加 `by_device` 反查索引
//! 隐藏会话总数上限 = 在线 PC 数 × [`MAX_HIDDEN_SESSIONS_PER_TARGET`]，量级极小；
//! [`teardown_hidden_for_device`] 与上限计数都是**冷路径**（断线、发起请求），线性
//! 扫 `hidden.values()` 够用。少一张必须手工同步的索引表 = 少一类一致性 bug，这是
//! 本文件单锁设计一贯的取舍。**阈值**：单机隐藏会话数超过 10⁴ 时再来加索引。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use lumen_protocol::remote::{
    DenyReason, EndReason, HiddenSessionId, PairingFailReason, RemoteC2S, RemoteS2C, Role,
    MAX_HIDDEN_SESSIONS_PER_TARGET,
};
use lumen_protocol::{MIN_SUPPORTED_VERSION, PROTOCOL_VERSION};
use tokio::sync::mpsc::UnboundedSender;

use crate::auth;

/// 配对码有效期（秒）：被控端展示后超时未完成即作废。
pub const PAIRING_TTL_SECS: i64 = 120;
/// 配对码最大尝试次数：连错此数后该配对作废（防暴力）。
pub const PAIRING_MAX_ATTEMPTS: u32 = 5;

/// [`RemoteC2S::ClientHello`] 的 `caps` 最多允许几项，超出即整条忽略。
///
/// 这是**对端可控输入的入口夹紧**，不是容量规划：当前协议只定义了一个能力位
/// （[`HIDDEN_CAP`]），8 已经给未来留了足够余量。理由见 [`PeerHandle::supports_hidden`]。
const MAX_CAPS_LEN: usize = 8;
/// 单个能力标签的最大字节数，超出即整条忽略。`"hidden"` 是 6 字节，32 是宽松上限。
const MAX_CAP_LEN: usize = 32;
/// 被控端在 [`RemoteC2S::ClientHello`] 里上报的「可承载隐藏会话」能力位。
///
/// 未上报此位的连接（老版本客户端根本不发 `ClientHello`）会在 [`open_hidden`] 里
/// **于生成配对码之前**被回 [`DenyReason::Offline`]：老 PC 上不会出现一个念了也
/// 没用的配对码横幅，手机也不必干等 [`PAIRING_TTL_SECS`] 秒 TTL。
///
/// **权威定义在协议 crate**（线格式的一部分，被控端写、服务端读）——这里只是转出
/// 来方便本模块引用，**绝不要在这里另写一个 `"hidden"` 字面量**：两份字面量任何一次
/// 手滑改名都会让服务端把所有新版 PC 判成老端，而编译器一声不吭。
pub use lumen_protocol::remote::HIDDEN_CAP;

/// 服务端 → 单条客户端连接的内部投递信号（经各连接的 mpsc 通道）。
#[derive(Debug)]
pub enum ToClient {
    /// 一条要写给客户端的协议消息。
    Msg(Box<RemoteS2C>),
    /// 要求该连接优雅关闭（被新连接驱逐时下发）。
    Close,
}

/// 在线连接句柄（驻留 [`HubState::peers`]）。
struct PeerHandle {
    /// 账户 id（跨用户隔离校验用）。
    user_id: String,
    /// 设备显示名（握手时由 DB 取，缓存于此）。
    name: String,
    /// 连接代次（驱逐 / 迟到消息守卫）。
    conn_id: u64,
    /// 出站通道发送端（接收端在该连接的 socket 写循环里）。
    tx: UnboundedSender<ToClient>,
    /// M7 片 4b：该连接是否上报了 [`HIDDEN_CAP`]（由 [`RemoteC2S::ClientHello`] 填入）。
    /// **登记时为 false**——老客户端不发 `ClientHello`，新客户端也要等第一条消息到达才
    /// 填得上，所以 false 既表示「老端」也表示「新端但还没说话」，两者在 [`open_hidden`]
    /// 里同样被拒。
    ///
    /// # 为什么存一个 `bool` 而不是原样存下 `caps: Vec<String>`
    /// `ClientHello.caps` 是**攻击者完全可控、无长度上限**的 `Vec<String>`，唯一的界是
    /// `ws.rs` 的 `MAX_WS_MESSAGE = 4 MiB`。`["a","a",…]` 这种形状在线上 4 字节/元素、
    /// 在堆上约 40 字节/元素 ⇒ 一条 4 MiB 消息换约 40 MB 常驻堆，而 `ClientHello` 可以在
    /// 一条连接上无限次重发。更要命的是消费侧：判「目标支不支持隐藏会话」原本是在
    /// [`Hub::lock`] 临界区内对这个 `Vec` 做线性扫，而那把单锁**同时串行化所有用户的镜像
    /// `relay` 热路径**——一个普通账户注册两台自家设备（A 报巨型 caps、B 狂发
    /// `OpenHidden{target:A}`，`precheck` 的 `CrossUser` 只挡跨账户）就能把全服中继拖停，
    /// 属跨租户 DoS。
    ///
    /// 预计算成 `bool` 之后：常驻内存恒定、锁内判定 O(1)、锁内不再扫任何对端可控长度的容器。
    /// 日后真要做能力灰度，也应该是「白名单里的已知位各占一个 bool / 一个 bitflags」，
    /// **不要**把原始字符串数组请回来。
    ///
    /// # 残余竞态（诚实记账，不装作没有）
    /// PC 刚连上、它的 `ClientHello` 尚未被处理时，手机的
    /// [`RemoteC2S::OpenHidden`] 从**另一条 socket** 到达 → 误判为不支持 → 回
    /// [`DenyReason::Offline`]。跨 socket 无全局序，这个窗口**无法在内存态里根除**。
    /// 自愈路径：手机侧的发起超时 + 重试，第二次必中；窗口 = 一次 WS 握手到首条
    /// 消息处理，毫秒级。若日后仍困扰，把能力落到 `devices` 表一列（REST 注册设备
    /// 时写入、`ws.rs` 的握手 SELECT 顺手带出），竞态才彻底消失——那要动 DB 与
    /// REST DTO，当前不值。
    supports_hidden: bool,
}

/// M7 片 4b：一次配对流程要建立的**会话种类**。
///
/// # 它只活在服务端内存里，**绝不上线缆**
/// 上线缆的是「客户端发了哪个变体」（[`RemoteC2S::RequestControl`] 还是
/// [`RemoteC2S::OpenHidden`]），不是这个字段。这正是「新增变体」相对「给已有变体
/// 加 kind 字段」的关键差别：老服务端遇到未知**变体**是整条丢弃（可被客户端超时
/// 救回），遇到未知**字段**是静默忽略后**按老语义执行成功**（手机凭空占住那台 PC
/// 的镜像独占位，两端都察觉不到）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    /// 桌面控制端的镜像会话（进 [`HubState::sessions`]）。
    Mirror,
    /// 手机端的隐藏会话（进 [`HubState::hidden`]）。
    Hidden,
}

/// M7 片 4b：一条活跃隐藏会话（key = 服务端分配的 [`HiddenSessionId`]）。
///
/// # 不变量
/// - `controller` ≠ `target`（由 [`precheck`] 的 `SelfControl` 保证）。
/// - 两者同账户（由 [`precheck`] 的 `CrossUser` 保证，这是唯一的跨账户边界）。
/// - 建立时两者都在线；之后任一端断线即由 [`teardown_hidden_for_device`] 整条拆除，
///   **不存在半挂状态**。
/// - 与 [`SessionEntry`] 不同，这里**不双插**：一条会话在表里只有一个条目，
///   路由靠 `session_id` 而非「发送者是谁」。因此同一台 PC 可以同时是多条隐藏会话
///   的 `target`——这正是镜像桶结构上做不到、而隐藏桶必须做到的事。
struct HiddenEntry {
    /// 发起方（手机）device_id。
    controller: String,
    /// 被控端（PC）device_id。
    target: String,
}

/// 活跃会话条目（双端各插一条，互指对端）。
///
/// part1 中继逻辑与独占判定只需「对端是谁」（不区分角色——角色已随
/// [`RemoteS2C::SessionStarted`] 告知客户端）。part2/3 若需服务端感知角色
/// （如输入仲裁），在此追加 `role` 字段即可。
struct SessionEntry {
    /// 对端设备 id。
    peer: String,
}

/// 未决配对（key = 被控端 device_id）。
struct Pending {
    /// 发起请求的控制端 device_id（[`RemoteC2S::SubmitPairing`] 须由它提交）。
    controller_device_id: String,
    /// 服务端生成、仅下发给被控端展示的 9 位配对码。
    code: String,
    /// 剩余尝试次数。
    attempts_left: u32,
    /// 创建时刻（Unix 秒），用于过期判定。
    created_at: i64,
    /// M7 片 4b：配对成功后要建哪一类会话（见 [`ControlKind`]，纯内存、不上线缆）。
    /// [`submit_pairing`] 的**唯一**改动就是末尾按它分派；TTL / 尝试次数 /
    /// 提交者绑定校验全部原样共用一份。
    kind: ControlKind,
}

/// 单锁保护的全部中继状态。
#[derive(Default)]
struct HubState {
    /// 在线连接：device_id → 句柄。
    peers: HashMap<String, PeerHandle>,
    /// 活跃**镜像**会话：device_id → 条目（控制端与被控端各一条）。
    sessions: HashMap<String, SessionEntry>,
    /// 未决配对：被控端 device_id → 配对态。**两类会话共用这一张单槽表**
    /// （见模块头「跨桶仍互斥的唯一一处」）。
    pending: HashMap<String, Pending>,
    /// M7 片 4b：活跃**隐藏**会话：session_id → 条目（不双插，见 [`HiddenEntry`]）。
    hidden: HashMap<HiddenSessionId, HiddenEntry>,
}

/// 远程控制中继枢纽（`AppState` 持 `Arc<Hub>`）。
#[derive(Default)]
pub struct Hub {
    state: Mutex<HubState>,
    /// 连接代次发号器（单调递增，唯一）。
    conn_seq: AtomicU64,
    /// M7 片 4b：隐藏会话 id 发号器（照 [`Self::conn_seq`] 的写法，
    /// `wrapping_add(1)` 保证发出的 id 恒不为 0——0 是协议侧的哨兵）。
    hidden_seq: AtomicU64,
}

impl Hub {
    /// 新建空枢纽。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 取一次性递增的连接代次。
    fn next_conn_id(&self) -> u64 {
        self.conn_seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// 加锁（poison 时取回内部值而非 panic / `unwrap`——临界区内无 panic 代码）。
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 登记一条新连接，返回其连接代次。
    ///
    /// 若同 `device_id` 已有旧连接，先**驱逐**：拆其会话、取消其 pending、给旧连接
    /// 发 [`ToClient::Close`]，再登记新连接。调用方在拿到 `conn_id` 后应立即给本
    /// 连接发 [`RemoteS2C::Welcome`]（见 [`Self::welcome`]）。
    pub fn register(
        &self,
        device_id: &str,
        user_id: String,
        name: String,
        tx: UnboundedSender<ToClient>,
    ) -> u64 {
        let conn_id = self.next_conn_id();
        let mut st = self.lock();
        if let Some(old) = st.peers.remove(device_id) {
            teardown_session(&mut st, device_id, EndReason::Replaced);
            // M7 片 4b：旧连接的隐藏会话同样要拆。少这一行，被驱逐的那台设备会在
            // `hidden` 里留下永久幽灵条目，一路吃掉目标 PC 的隐藏会话名额。
            teardown_hidden_for_device(&mut st, device_id, EndReason::Replaced);
            cancel_pending_as_controller(&mut st, device_id);
            cancel_pending_as_target(&mut st, device_id, DenyReason::Offline);
            let _ = old.tx.send(ToClient::Close);
        }
        st.peers.insert(
            device_id.to_string(),
            PeerHandle {
                user_id,
                name,
                conn_id,
                tx,
                // 能力位等 ClientHello 到达后由 set_caps 填入（见
                // PeerHandle::supports_hidden 的残余竞态说明）。
                supports_hidden: false,
            },
        );
        conn_id
    }

    /// 构造本连接的 [`RemoteS2C::Welcome`]（协议版本协商 + 确认 device_id）。
    #[must_use]
    pub fn welcome(device_id: &str) -> RemoteS2C {
        RemoteS2C::Welcome {
            protocol_version: PROTOCOL_VERSION,
            min_supported_version: MIN_SUPPORTED_VERSION,
            device_id: device_id.to_string(),
        }
    }

    /// 连接断开：以 `conn_id` 守卫后注销 peer、拆会话、取消相关 pending、通知对端。
    pub fn disconnect(&self, device_id: &str, conn_id: u64) {
        let mut st = self.lock();
        if !is_current(&st, device_id, conn_id) {
            return; // 已被新连接取代：本次断开不触动新连接状态。
        }
        st.peers.remove(device_id);
        teardown_session(&mut st, device_id, EndReason::PeerDisconnected);
        // M7 片 4b：该设备参与的**全部**隐藏会话一并拆除（它可能同时是多条会话的
        // target）。镜像会话只会有一条，隐藏会话不是——这就是不能沿用
        // `teardown_session` 的原因。
        teardown_hidden_for_device(&mut st, device_id, EndReason::PeerDisconnected);
        cancel_pending_as_controller(&mut st, device_id);
        cancel_pending_as_target(&mut st, device_id, DenyReason::Offline);
    }

    /// 控制端发起控制（`RequestControl`）。`paired`=该对设备已持久化信任（由 `ws.rs`
    /// 查 DB 得出）：为 true 则**跳过配对码直连**（首次配对后免重输，海风哥拍板）。
    ///
    /// `conn_id` 守卫：被驱逐的旧连接迟到消息一律忽略。
    pub fn request_control(&self, from: &str, conn_id: u64, target: &str, paired: bool) {
        let mut st = self.lock();
        if !is_current(&st, from, conn_id) {
            return;
        }
        request_control(&mut st, from, target, paired);
    }

    /// M7 片 4b：手机端发起**隐藏会话**（`OpenHidden`）。与
    /// [`Self::request_control`] 平级、互不排斥——桌面镜像开着时这条依然能成。
    ///
    /// # `paired` 恒为 `false`（调用方 `ws.rs` 写死）
    /// 参数保留是为了与 [`Self::request_control`] 同形、且日后 `device_pairs` 加上
    /// `kind` 列后能直接接上真值；**当前隐藏会话每次都要念配对码**。
    /// 为什么必须这样，见 [`submit_pairing`] 里 `ControlKind::Hidden` 分支的长注释
    /// （一句话：`device_pairs` 没有种类列，两类会话共用一行信任是双向越权）。
    pub fn open_hidden(&self, from: &str, conn_id: u64, target: &str, paired: bool) {
        let mut st = self.lock();
        if !is_current(&st, from, conn_id) {
            return;
        }
        open_hidden(&mut st, from, target, paired, &self.hidden_seq);
    }

    /// 控制端提交配对码（`SubmitPairing`）。校验成功并**新建**会话时返回
    /// `Some((控制端, 被控端))`，供 `ws.rs` 持久化这对设备的信任（之后免重输）。
    ///
    /// M7 片 4b 起对**两类会话共用**：建哪一类由未决配对自身记着的
    /// [`ControlKind`] 决定，客户端无从指定（它当初发的是哪个变体，就已经定死了）。
    pub fn submit_pairing(
        &self,
        from: &str,
        conn_id: u64,
        target: &str,
        code: &str,
    ) -> Option<(String, String)> {
        let mut st = self.lock();
        if !is_current(&st, from, conn_id) {
            return None;
        }
        submit_pairing(&mut st, from, target, code, &self.hidden_seq)
    }

    /// 处理其余客户端消息（拒绝 / 结束 / 中继 / 心跳）。`RequestControl` /
    /// `SubmitPairing` 因需 DB（配对信任）走 [`Self::request_control`] /
    /// [`Self::submit_pairing`]。`conn_id` 守卫：旧连接迟到消息忽略。
    pub fn handle(&self, device_id: &str, conn_id: u64, msg: RemoteC2S) {
        let mut st = self.lock();
        if !is_current(&st, device_id, conn_id) {
            return;
        }
        match msg {
            RemoteC2S::DeclineControl => decline_control(&mut st, device_id),
            RemoteC2S::EndSession => {
                teardown_session(&mut st, device_id, EndReason::PeerLeft);
            }
            RemoteC2S::Relay(value) => relay(&st, device_id, value),
            RemoteC2S::Ping => send_msg(&st.peers, device_id, RemoteS2C::Pong),
            // ── M7 片 4b：隐藏会话 ───────────────────────────────────────────
            RemoteC2S::ClientHello {
                protocol_version,
                caps,
            } => set_caps(&mut st, device_id, protocol_version, &caps),
            RemoteC2S::RelayTo {
                session_id,
                payload,
            } => relay_hidden(&st, device_id, session_id, payload),
            RemoteC2S::EndHidden { session_id } => {
                end_hidden(&mut st, device_id, session_id);
            }
            // 这三类经专用方法（带 DB 配对信任），不应到此。
            RemoteC2S::RequestControl { .. }
            | RemoteC2S::SubmitPairing { .. }
            | RemoteC2S::OpenHidden { .. } => {
                tracing::warn!("RequestControl/SubmitPairing/OpenHidden 不应经 handle，已忽略");
            }
        }
    }

    /// 后台定时清理：移除过期 pending 并通知双方（控制端 `ControlDenied{Expired}`、
    /// 被控端 `PairingCancelled{Expired}`）。由 `main.rs` 周期调用。
    pub fn gc(&self) {
        let mut st = self.lock();
        purge_expired(&mut st);
    }
}

/// `device_id` 当前在线连接的代次是否等于 `conn_id`。
fn is_current(st: &HubState, device_id: &str, conn_id: u64) -> bool {
    st.peers.get(device_id).is_some_and(|p| p.conn_id == conn_id)
}

/// 向某设备投递一条消息（设备不在线则静默丢弃；通道已关亦忽略）。
fn send_msg(peers: &HashMap<String, PeerHandle>, device_id: &str, msg: RemoteS2C) {
    if let Some(peer) = peers.get(device_id) {
        let _ = peer.tx.send(ToClient::Msg(Box::new(msg)));
    }
}

/// 生成 9 位十进制配对码。
///
/// 取 uuid v4（122 位随机熵）低位对 10^9 取模——`2^122 mod 10^9` 引入的取模偏置
/// 在 `2^-90` 量级，可忽略；配合 [`PAIRING_MAX_ATTEMPTS`] 与 [`PAIRING_TTL_SECS`]
/// 足以抵御暴力猜测。
fn gen_pairing_code() -> String {
    let n = uuid::Uuid::new_v4().as_u128() % 1_000_000_000;
    format!("{n:09}")
}

/// 拆除 `device_id` 参与的会话（若有）：移除双端条目并通知对端 [`RemoteS2C::SessionEnded`]。
/// 返回是否确实拆了一个会话。
fn teardown_session(st: &mut HubState, device_id: &str, reason: EndReason) -> bool {
    let Some(entry) = st.sessions.remove(device_id) else {
        return false;
    };
    let peer = entry.peer;
    st.sessions.remove(&peer);
    send_msg(&st.peers, &peer, RemoteS2C::SessionEnded { reason });
    true
}

/// 取消 `device_id` 作为**控制端**发起的全部未决 pending，并通知各被控端
/// [`RemoteS2C::PairingCancelled`]（dismiss 配对码）。
fn cancel_pending_as_controller(st: &mut HubState, device_id: &str) {
    let targets: Vec<String> = st
        .pending
        .iter()
        .filter(|(_, p)| p.controller_device_id == device_id)
        .map(|(t, _)| t.clone())
        .collect();
    for t in targets {
        st.pending.remove(&t);
        send_msg(
            &st.peers,
            &t,
            RemoteS2C::PairingCancelled {
                reason: DenyReason::ControllerLeft,
            },
        );
    }
}

/// 取消 `device_id` 作为**被控端**的未决 pending（若有），通知控制端 [`RemoteS2C::ControlDenied`]。
fn cancel_pending_as_target(st: &mut HubState, device_id: &str, reason: DenyReason) {
    if let Some(p) = st.pending.remove(device_id) {
        send_msg(
            &st.peers,
            &p.controller_device_id,
            RemoteS2C::ControlDenied {
                target_device_id: device_id.to_string(),
                reason,
            },
        );
    }
}

/// 移除全部过期 pending 并通知双方。
fn purge_expired(st: &mut HubState) {
    let now = auth::now_secs();
    let expired: Vec<(String, String)> = st
        .pending
        .iter()
        .filter(|(_, p)| now - p.created_at >= PAIRING_TTL_SECS)
        .map(|(t, p)| (t.clone(), p.controller_device_id.clone()))
        .collect();
    for (target, controller) in expired {
        st.pending.remove(&target);
        send_msg(
            &st.peers,
            &controller,
            RemoteS2C::ControlDenied {
                target_device_id: target.clone(),
                reason: DenyReason::Expired,
            },
        );
        send_msg(
            &st.peers,
            &target,
            RemoteS2C::PairingCancelled {
                reason: DenyReason::Expired,
            },
        );
    }
}

/// 向控制端下发一条 [`RemoteS2C::ControlDenied`]（两类发起路径共用）。
fn deny_to(st: &HubState, controller_id: &str, target: &str, reason: DenyReason) {
    send_msg(
        &st.peers,
        controller_id,
        RemoteS2C::ControlDenied {
            target_device_id: target.to_string(),
            reason,
        },
    );
}

/// 两类发起路径（[`request_control`] / [`open_hidden`]）**共用**的前置校验：
/// 自控 / 目标在线 / 同账户，通过则返回 `(控制端名, 目标名)`。
///
/// # 这是本片对老路径的唯一一次重构，且**行为等价**
/// 原 `request_control` 里「控制端不在 `peers`」走的是 `return`（不发任何东西），
/// 这里统一成 `Err(Offline)`。二者**可观察行为完全相同**：调用方拿到 `Err` 会调
/// [`deny_to`] → [`send_msg`] → `peers.get(controller_id)` 为 `None` → 静默丢弃。
/// 换言之那条消息本来就发不出去。三个既有测试（跨账户控制被拒 / 自控被拒 /
/// 离线目标被拒）守着这段等价性。
///
/// **刻意不含 `purge_expired`**：两条路径清完过期 pending 之后的判定不同
/// （镜像看 `sessions`，隐藏看 `hidden`），把它留在各自的调用点更能看清顺序。
fn precheck(
    st: &HubState,
    controller_id: &str,
    target: &str,
) -> Result<(String, String), DenyReason> {
    // 自控。
    if controller_id == target {
        return Err(DenyReason::SelfControl);
    }
    // 控制端身份（必在 peers——刚通过 handle 守卫）。
    let Some(controller) = st.peers.get(controller_id) else {
        return Err(DenyReason::Offline);
    };
    let controller_user = controller.user_id.clone();
    let controller_name = controller.name.clone();
    // 目标在线。
    let Some(target_peer) = st.peers.get(target) else {
        return Err(DenyReason::Offline);
    };
    // 同账户（禁跨用户）——这是**唯一**的跨账户边界，同账户内不做隔离。
    if target_peer.user_id != controller_user {
        return Err(DenyReason::CrossUser);
    }
    Ok((controller_name, target_peer.name.clone()))
}

/// 控制端发起**镜像**控制请求：层层校验（自控 / 在线 / 同账户 / 控制端空闲 /
/// 目标空闲 / 目标无配对中），通过则生成配对码、存 pending、通知双方。
///
/// M7 片 4b 起前三段走共用的 [`precheck`]，**独占判定（下面三个 if）一行未改**：
/// 它们只看 `sessions` / `pending`，看不见 `hidden`，所以桌面镜像不受隐藏会话影响。
fn request_control(st: &mut HubState, controller_id: &str, target: &str, paired: bool) {
    let deny = |st: &HubState, reason: DenyReason| deny_to(st, controller_id, target, reason);

    let (controller_name, target_name) = match precheck(st, controller_id, target) {
        Ok(names) => names,
        Err(reason) => {
            deny(st, reason);
            return;
        }
    };
    // 惰性清理过期 pending（避免 contains_key 命中陈旧项）。
    purge_expired(st);
    // 控制端已在会话中 / 已发起其它配对 → 忙（控制端同一刻只控一台）。
    if st.sessions.contains_key(controller_id)
        || st
            .pending
            .values()
            .any(|p| p.controller_device_id == controller_id)
    {
        deny(st, DenyReason::ControllerBusy);
        return;
    }
    // 目标已被控。
    if st.sessions.contains_key(target) {
        deny(st, DenyReason::AlreadyControlled);
        return;
    }
    // 目标正与他人配对中。
    if st.pending.contains_key(target) {
        deny(st, DenyReason::TargetPairing);
        return;
    }
    // 已配对信任（首次配对后）：跳过配对码，直接建会话（免重输，海风哥拍板）。
    if paired {
        establish_session(st, controller_id, target);
        return;
    }
    // 首次：生成码、登记 pending、双向通知。
    let code = gen_pairing_code();
    let now = auth::now_secs();
    st.pending.insert(
        target.to_string(),
        Pending {
            controller_device_id: controller_id.to_string(),
            code: code.clone(),
            attempts_left: PAIRING_MAX_ATTEMPTS,
            created_at: now,
            kind: ControlKind::Mirror,
        },
    );
    let ttl = u32::try_from(PAIRING_TTL_SECS).unwrap_or(120);
    send_msg(
        &st.peers,
        target,
        RemoteS2C::ControlRequested {
            controller_device_id: controller_id.to_string(),
            controller_name,
            pairing_code: code,
            expires_in_secs: ttl,
        },
    );
    send_msg(
        &st.peers,
        controller_id,
        RemoteS2C::PairingNeeded {
            target_device_id: target.to_string(),
            target_name,
            expires_in_secs: ttl,
        },
    );
}

/// 原子建立会话：复检独占（配对/直连期间对端可能已被占用）+ 取双方名 → 双向插入
/// [`SessionEntry`] 并发 [`RemoteS2C::SessionStarted`]。成功返回 `true`；失败发
/// [`RemoteS2C::ControlDenied`] 给控制端并返回 `false`。被「配对码成功」与「已配对
/// 直连」两条路径共用。
fn establish_session(st: &mut HubState, controller_id: &str, target: &str) -> bool {
    let deny = |st: &HubState, reason: DenyReason| {
        send_msg(
            &st.peers,
            controller_id,
            RemoteS2C::ControlDenied {
                target_device_id: target.to_string(),
                reason,
            },
        );
    };
    if st.sessions.contains_key(controller_id) {
        deny(st, DenyReason::ControllerBusy);
        return false;
    }
    if st.sessions.contains_key(target) {
        deny(st, DenyReason::AlreadyControlled);
        return false;
    }
    let controller_name = st.peers.get(controller_id).map(|p| p.name.clone());
    let target_name = st.peers.get(target).map(|p| p.name.clone());
    // 双方均须在线（断线会清 peers）。
    let (Some(controller_name), Some(target_name)) = (controller_name, target_name) else {
        deny(st, DenyReason::Offline);
        return false;
    };
    st.sessions.insert(
        controller_id.to_string(),
        SessionEntry {
            peer: target.to_string(),
        },
    );
    st.sessions.insert(
        target.to_string(),
        SessionEntry {
            peer: controller_id.to_string(),
        },
    );
    send_msg(
        &st.peers,
        controller_id,
        RemoteS2C::SessionStarted {
            peer_device_id: target.to_string(),
            peer_name: target_name,
            role: Role::Controller,
        },
    );
    send_msg(
        &st.peers,
        target,
        RemoteS2C::SessionStarted {
            peer_device_id: controller_id.to_string(),
            peer_name: controller_name,
            role: Role::Controlled,
        },
    );
    true
}

/// 控制端提交配对码：校验提交者身份 + 有效期 + 码值，成功经 [`establish_session`]
/// （镜像）或 [`establish_hidden`]（隐藏）建会话并返回 `Some((控制端, 被控端))`
/// （供 `ws.rs` 持久化信任）；失败按原因回 [`RemoteS2C::PairingResult`] 并返回 `None`。
///
/// # M7 片 4b：本函数的**唯一**改动是末尾那个 `match p_kind`
/// 提交者绑定校验、TTL、尝试次数递减、[`RemoteS2C::PairingResult`] 全部**一行未改**、
/// 两类会话共用同一份。这是选「新增变体」而不是「另开一套配对状态机」的全部理由：
/// 下面 `p_owner != controller_id` 那一行是整套配对安全强度的唯一支点，抄成两份就
/// 等于把这条不变量放进两个会独立演化的地方。
///
/// `hidden_seq` 只在分派到隐藏分支时才取号，镜像分支不消耗 id。
fn submit_pairing(
    st: &mut HubState,
    controller_id: &str,
    target: &str,
    code: &str,
    hidden_seq: &AtomicU64,
) -> Option<(String, String)> {
    let fail = |st: &HubState, reason: PairingFailReason, attempts_left: u32| {
        send_msg(
            &st.peers,
            controller_id,
            RemoteS2C::PairingResult {
                reason,
                attempts_left,
            },
        );
    };

    // 读出 pending 快照（释放借用后再改状态）。
    let Some((p_code, p_created, p_owner, p_attempts, p_kind)) = st.pending.get(target).map(|p| {
        (
            p.code.clone(),
            p.created_at,
            p.controller_device_id.clone(),
            p.attempts_left,
            p.kind,
        )
    }) else {
        fail(st, PairingFailReason::NoPending, 0);
        return None;
    };
    // 提交者必须是当初发起请求的控制端（取自鉴权 device_id，杜绝抢答/跨用户）。
    // 不命中按「无此配对」回复，避免向无关方泄漏配对存在与否。
    //
    // ★ 二维码配对与手机隐藏会话**均**依赖这一行：放宽即整套配对强度塌掉
    //（任一同账户设备都能抢答别人正在念的那个码）。
    if p_owner != controller_id {
        fail(st, PairingFailReason::NoPending, 0);
        return None;
    }
    // 过期（>= TTL 即失效：配对码恰好有效 PAIRING_TTL_SECS 秒，elapsed∈[0,TTL)）。
    if auth::now_secs() - p_created >= PAIRING_TTL_SECS {
        st.pending.remove(target);
        fail(st, PairingFailReason::Expired, 0);
        send_msg(
            &st.peers,
            target,
            RemoteS2C::PairingCancelled {
                reason: DenyReason::Expired,
            },
        );
        return None;
    }
    // 码错：递减尝试，归零即作废。
    if p_code != code {
        let left = p_attempts.saturating_sub(1);
        if left == 0 {
            st.pending.remove(target);
            fail(st, PairingFailReason::TooManyAttempts, 0);
            send_msg(
                &st.peers,
                target,
                RemoteS2C::PairingCancelled {
                    reason: DenyReason::TooManyAttempts,
                },
            );
        } else {
            if let Some(p) = st.pending.get_mut(target) {
                p.attempts_left = left;
            }
            fail(st, PairingFailReason::InvalidCode, left);
        }
        return None;
    }
    // 码对：原子建会话（establish_* 内复检独占 + 取名 + 发会话开始消息）。
    st.pending.remove(target);
    match p_kind {
        // 镜像：成功即返回这对设备 → ws.rs 持久化信任，之后免重输（直到一端被删）。
        ControlKind::Mirror => establish_session(st, controller_id, target)
            .then(|| (controller_id.to_string(), target.to_string())),
        // ★ **隐藏会话恒不持久化信任**（返回 `None` ⇒ `ws.rs` 跳过 `persist_pair`）。
        //
        // # 这不是遗漏，是修掉一条同意范围逃逸
        // `device_pairs` 只有 `(user_id, dev_lo, dev_hi)`，**没有会话种类列**，
        // `is_paired` / `persist_pair` 都是 kind-agnostic。原来两类会话共用同一行信任，
        // 于是双向都越权：
        //
        // - 手机为「跟 PC 上的 LLM 说话」念一次码，这行信任**同时**授予它日后免码建立
        //   **镜像**会话的能力（全屏 + 远程输入）；
        // - 反向更隐蔽：一台早年为镜像配过对的桌面端可以免码开一条**隐藏**会话，而隐藏
        //   会话在被控端**没有横幅、没有指示器、没有结束入口**（那三处 UI 落点都还空着），
        //   等于把一条有 UI 的通道换成同等权力、零指示的通道。
        //
        // 完整方案是给 `device_pairs` 加 `kind` 列（或另开一张 `device_pairs_hidden`）
        // 并把 `ControlKind` 一路穿到 `is_paired` / `persist_pair`，两类信任互不通用。
        // 那要动 DB schema 与迁移，故本轮取**最小止血**：隐藏会话每次都要念码。
        // 与之配对的另一半在 `ws.rs` 的 `OpenHidden` 臂——它恒传 `paired = false`，
        // 堵住反向那条。两处**必须同时成立**，改动其一前请先读另一处。
        ControlKind::Hidden => {
            let ok = establish_hidden(st, controller_id, target, next_hidden_id(hidden_seq));
            tracing::debug!("隐藏会话配对完成（ok={ok}），按设计不持久化配对信任");
            None
        }
    }
}

/// 被控端拒绝当前未决配对（key = 被控端自身 device_id）：清 pending、通知控制端。
fn decline_control(st: &mut HubState, controlled_id: &str) {
    if let Some(p) = st.pending.remove(controlled_id) {
        send_msg(
            &st.peers,
            &p.controller_device_id,
            RemoteS2C::ControlDenied {
                target_device_id: controlled_id.to_string(),
                reason: DenyReason::RejectedByUser,
            },
        );
    }
}

/// **镜像**会话数据面盲转：查发送者所在会话的对端，把不透明帧原样转给对端；
/// 无会话则丢弃。
///
/// M7 片 4b **一行未改**：路由键仍是「发送者 device_id」，因为镜像桶里一台设备
/// 恒定只有一条会话。隐藏会话不能沿用这条路径（一台 PC 可能挂着多条），走
/// [`relay_hidden`]。
fn relay(st: &HubState, from: &str, value: serde_json::Value) {
    if let Some(entry) = st.sessions.get(from) {
        let peer = entry.peer.clone();
        send_msg(&st.peers, &peer, RemoteS2C::Relay(value));
    } else {
        tracing::debug!("relay 无活跃会话，丢弃（from={from}）");
    }
}

// ═══════════════════════ M7 片 4b：隐藏会话（以下全部为新增）═══════════════════════

/// 从发号器取一个隐藏会话 id：单调递增、**恒不为 0**（0 是协议侧哨兵）。
///
/// 与 `Hub::next_conn_id` 同款写法。允许出现空洞（例如某次建会话在复检里失败），
/// 不变量是「单调 + 不复用」而不是「连续」。
fn next_hidden_id(seq: &AtomicU64) -> HiddenSessionId {
    seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

/// 记下该连接自报的能力位（[`RemoteC2S::ClientHello`]）。
///
/// `protocol_version` 只进日志——这是服务端第一次能在排障时区分客户端版本；
/// 版本协商本身仍由 [`RemoteS2C::Welcome`] 在客户端侧完成，服务端**不**据此拒连
/// （拒连会把「能用但功能少一点」变成「完全用不了」）。
/// # 入口夹紧与日志降级（两条都是安全要求，不是洁癖）
/// `caps` 是对端可控、无长度上限的 `Vec<String>`（见 [`PeerHandle::supports_hidden`]）。
///
/// 1. **超形状直接整条丢弃**：合法的 `caps` 是「几个短标签」，
///    `> MAX_CAPS_LEN` 项或单项 `> MAX_CAP_LEN` 字节的一律不是正常客户端会发的东西。
///    丢弃而不是截断——截断会让一条畸形消息仍然「部分生效」，判起来更难。
/// 2. **日志按 `debug!` 且只打条数**：老写法是 `info!(… caps={caps:?})`，而服务端默认
///    `EnvFilter::new("info")`，加上 `ClientHello` 可在一条连接上无限次重发、全服务端没有
///    任何 rate limit ⇒ **零成本刷爆日志盘**。排障需要的是「这个设备报没报隐藏能力」，
///    不是那个数组的原文。
fn set_caps(st: &mut HubState, device_id: &str, protocol_version: u32, caps: &[String]) {
    if caps.len() > MAX_CAPS_LEN || caps.iter().any(|c| c.len() > MAX_CAP_LEN) {
        tracing::debug!(
            "ClientHello 的 caps 形状异常（{} 项），整条忽略（device={device_id}）",
            caps.len()
        );
        return;
    }
    let Some(peer) = st.peers.get_mut(device_id) else {
        return;
    };
    peer.supports_hidden = caps.iter().any(|c| c == HIDDEN_CAP);
    tracing::debug!(
        "ClientHello device={device_id} protocol_version={protocol_version} caps_len={} hidden={}",
        caps.len(),
        peer.supports_hidden
    );
}

/// 目标 PC 当前挂着几条隐藏会话（冷路径线性扫，见模块头「为什么不加反查索引」）。
fn hidden_count_for_target(st: &HubState, target: &str) -> usize {
    st.hidden.values().filter(|e| e.target == target).count()
}

/// 该设备是否已作为**发起方**占着一条隐藏会话。
///
/// 覆盖隐藏桶的两条 `ControllerBusy`：「同一 (手机, PC) 至多 1 条」与「一部手机同时
/// 只连 1 台 PC」——两者合起来就是「这部手机在 `hidden` 里作为 controller 出现过」。
fn hidden_has_controller(st: &HubState, controller_id: &str) -> bool {
    st.hidden.values().any(|e| e.controller == controller_id)
}

/// 手机端发起**隐藏会话**：前置校验（共用 [`precheck`]）→ **caps 门** → 隐藏桶
/// 独占判定 → 建会话或走配对码。
///
/// # 与 [`request_control`] 的关系：平级、互不排斥
/// 本函数**从不读写 `sessions`**，`request_control` 也**从不读写 `hidden`**。
/// 「桌面镜像开着时手机可以同时接入」这件事，就是靠这两句话成立的——不是靠某个
/// 运行时的 `if kind == …`。
fn open_hidden(
    st: &mut HubState,
    controller_id: &str,
    target: &str,
    paired: bool,
    hidden_seq: &AtomicU64,
) {
    let deny = |st: &HubState, reason: DenyReason| deny_to(st, controller_id, target, reason);

    let (controller_name, target_name) = match precheck(st, controller_id, target) {
        Ok(names) => names,
        Err(reason) => {
            deny(st, reason);
            return;
        }
    };
    // ★ caps 门：目标未上报「可承载隐藏会话」（老版本客户端根本不发 ClientHello）
    // → 在**生成配对码之前**就拒。老 PC 上不会出现一个念了也没用的横幅，手机也
    // 立刻拿到明确拒绝而不是干等 TTL。
    //
    // 复用 Offline 而不是新增拒因：新增 DenyReason 变体会让**老客户端**整条
    // ControlDenied 解析失败，丢掉它唯一的拒绝回执 → UI 永久转圈。而且「PC 不在
    // 线」与「PC 版本过低」指向的用户动作完全相同（去看那台 PC），合并成一句话
    // 不是偷懒。
    if !target_supports_hidden(st, target) {
        tracing::debug!("open_hidden 目标未上报 {HIDDEN_CAP} 能力，按 Offline 拒（target={target}）");
        deny(st, DenyReason::Offline);
        return;
    }
    // 惰性清理过期 pending（与镜像路径同一处理，避免 contains_key 命中陈旧项）。
    purge_expired(st);
    // 发起方已占着一条隐藏会话 / 已发起其它未决配对 → 忙。
    if hidden_has_controller(st, controller_id)
        || st
            .pending
            .values()
            .any(|p| p.controller_device_id == controller_id)
    {
        deny(st, DenyReason::ControllerBusy);
        return;
    }
    // 目标的隐藏会话名额已满（与镜像名额相互独立）。
    if hidden_count_for_target(st, target) >= MAX_HIDDEN_SESSIONS_PER_TARGET {
        deny(st, DenyReason::AlreadyControlled);
        return;
    }
    // 目标正与他人配对中 —— **这是唯一一处跨桶互斥，有意保留**（配对码是给人念的，
    // 同一时刻 PC 屏幕上只该出现一个码，详见模块头）。不要当 bug 修掉。
    if st.pending.contains_key(target) {
        deny(st, DenyReason::TargetPairing);
        return;
    }
    // 已配对信任：跳过配对码直建（与镜像路径同一张 device_pairs，见 Hub::open_hidden
    // 文档里记的那条安全取舍）。
    if paired {
        establish_hidden(st, controller_id, target, next_hidden_id(hidden_seq));
        return;
    }
    // 首次：生成码、登记 pending（kind=Hidden）、双向通知。
    let code = gen_pairing_code();
    st.pending.insert(
        target.to_string(),
        Pending {
            controller_device_id: controller_id.to_string(),
            code: code.clone(),
            attempts_left: PAIRING_MAX_ATTEMPTS,
            created_at: auth::now_secs(),
            kind: ControlKind::Hidden,
        },
    );
    let ttl = u32::try_from(PAIRING_TTL_SECS).unwrap_or(120);
    // 被控端收的是**独立变体** HiddenControlRequested：复用 ControlRequested 会让
    // 老 PC 展示一个码、配对成功后收不到自己认识的 SessionStarted（服务端发的是
    // HiddenSessionStarted，老端整条丢弃）→ 横幅永久残留；用户点「拒绝」还会得到
    // 「我拒绝成功了」的错觉而实际会话已建 —— 那是安全误导。
    send_msg(
        &st.peers,
        target,
        RemoteS2C::HiddenControlRequested {
            controller_device_id: controller_id.to_string(),
            controller_name,
            pairing_code: code,
            expires_in_secs: ttl,
        },
    );
    // 发起方收的是**复用**的 PairingNeeded：它只说「请输入对方展示的码」，与会话
    // 种类无关，且手机端本就是新客户端，无老端兼容问题。
    //
    // `target_name` 直接用 `precheck` 已经取好的那份，**不再查第二次表**：老写法在这里
    // `st.peers.get(target).map_or_else(String::new, …)` 重取一遍并带一个静默的空串兜底，
    // 与同文件 `request_control`（543 拿到、606 直接用）无理由地分叉。那个空串兜底一旦
    // 因后续重构真的命中（例如取名被挪到某次状态变更之后），手机的配对对话框会显示一个
    // 空设备名，且不报错、不记日志。
    send_msg(
        &st.peers,
        controller_id,
        RemoteS2C::PairingNeeded {
            target_device_id: target.to_string(),
            target_name,
            expires_in_secs: ttl,
        },
    );
}

/// 目标连接是否上报了 [`HIDDEN_CAP`]。
///
/// **O(1)，锁内不扫任何对端可控长度的容器**——判据在 [`set_caps`] 里就已经算好了。
/// 原因见 [`PeerHandle::supports_hidden`] 的长注释（老写法的线性扫在 [`Hub::lock`] 临界区
/// 内，而那把单锁同时串行化全服的镜像 `relay` 热路径）。
fn target_supports_hidden(st: &HubState, target: &str) -> bool {
    st.peers.get(target).is_some_and(|p| p.supports_hidden)
}

/// 原子建立一条隐藏会话。
///
/// **复检**隐藏桶名额（配对的 120 秒里名额可能已被别人占掉）+ 取双方名 → 插
/// [`HiddenEntry`] 并向双方发 [`RemoteS2C::HiddenSessionStarted`]。成功返回 `true`；
/// 失败发 [`RemoteS2C::ControlDenied`] 给发起方并返回 `false`。
///
/// 被「配对码成功」与「已配对直连」两条路径共用——与 [`establish_session`] 同构，
/// 少了这个复检就能从配对路径绕过上限。
fn establish_hidden(
    st: &mut HubState,
    controller_id: &str,
    target: &str,
    session_id: HiddenSessionId,
) -> bool {
    let deny = |st: &HubState, reason: DenyReason| deny_to(st, controller_id, target, reason);

    if hidden_has_controller(st, controller_id) {
        deny(st, DenyReason::ControllerBusy);
        return false;
    }
    if hidden_count_for_target(st, target) >= MAX_HIDDEN_SESSIONS_PER_TARGET {
        deny(st, DenyReason::AlreadyControlled);
        return false;
    }
    // 能力位复检：配对期间目标可能换成了老版本连接（重连驱逐会把 caps 清空）。
    if !target_supports_hidden(st, target) {
        deny(st, DenyReason::Offline);
        return false;
    }
    let controller_name = st.peers.get(controller_id).map(|p| p.name.clone());
    let target_name = st.peers.get(target).map(|p| p.name.clone());
    // 双方均须在线（断线会清 peers）。
    let (Some(controller_name), Some(target_name)) = (controller_name, target_name) else {
        deny(st, DenyReason::Offline);
        return false;
    };
    st.hidden.insert(
        session_id,
        HiddenEntry {
            controller: controller_id.to_string(),
            target: target.to_string(),
        },
    );
    send_msg(
        &st.peers,
        controller_id,
        RemoteS2C::HiddenSessionStarted {
            session_id,
            peer_device_id: target.to_string(),
            peer_name: target_name,
            role: Role::Controller,
        },
    );
    send_msg(
        &st.peers,
        target,
        RemoteS2C::HiddenSessionStarted {
            session_id,
            peer_device_id: controller_id.to_string(),
            peer_name: controller_name,
            role: Role::Controlled,
        },
    );
    true
}

/// 隐藏会话数据面盲转：按 `session_id` 查会话、**校验发送者是该会话成员**后转给对端。
///
/// # 为何必须校验成员（这不是防御式编程，是一条真实攻击面）
/// `session_id` 是全局单调 u64，**可被同账户任一设备猜中**——[`precheck`] 里的
/// `CrossUser` 只挡跨账户，同账户内本就无隔离。若只做「查得到就转」，任一同账户
/// 设备即可向别人的隐藏会话注入帧；而隐藏会话的载荷里含「向 LLM 发消息」这种指令，
/// 那就是直接往别人 PC 上投喂 prompt。
///
/// # 为何不命中要**静默丢弃、不回错误**
/// 回执会变成一个探测「哪些 `session_id` 存在」的旁路（试一遍 1..N，有回执的就是
/// 活会话）。对齐 [`relay`] 的既有口径：转不出去就丢，不告诉发送者为什么。
///
/// 载荷 `payload` 全程不被反序列化，**日志里也绝不打印它**（那里面是对话正文）。
fn relay_hidden(
    st: &HubState,
    from: &str,
    session_id: HiddenSessionId,
    payload: serde_json::Value,
) {
    let Some(entry) = st.hidden.get(&session_id) else {
        tracing::debug!("relay_to 无此隐藏会话，丢弃（from={from} sid={session_id}）");
        return;
    };
    let peer = if from == entry.controller {
        &entry.target
    } else if from == entry.target {
        &entry.controller
    } else {
        tracing::debug!("relay_to 发送者非会话成员，丢弃（from={from} sid={session_id}）");
        return;
    };
    send_msg(
        &st.peers,
        peer,
        RemoteS2C::RelayTo {
            session_id,
            payload,
        },
    );
}

/// 任一端主动结束一条隐藏会话（[`RemoteC2S::EndHidden`]）。
///
/// 同样要做成员校验：否则同账户任一设备猜中 sid 就能拆掉别人的会话（拒绝服务）。
fn end_hidden(st: &mut HubState, from: &str, session_id: HiddenSessionId) {
    let is_member = st
        .hidden
        .get(&session_id)
        .is_some_and(|e| e.controller == from || e.target == from);
    if !is_member {
        tracing::debug!("end_hidden 非会话成员或会话不存在，忽略（from={from} sid={session_id}）");
        return;
    }
    teardown_hidden_one(st, session_id, EndReason::PeerLeft, Some(from));
}

/// 拆除一条隐藏会话，并通知**除 `except` 外**的成员。返回该会话此前是否存在。
///
/// `except` 用于「主动方不回执」：发 [`RemoteC2S::EndHidden`] 的那端已经知道结果，
/// 断线的那端也根本收不到（它已被移出 `peers`）。
fn teardown_hidden_one(
    st: &mut HubState,
    session_id: HiddenSessionId,
    reason: EndReason,
    except: Option<&str>,
) -> bool {
    let Some(entry) = st.hidden.remove(&session_id) else {
        return false;
    };
    for member in [&entry.controller, &entry.target] {
        if except.is_some_and(|e| e == member) {
            continue;
        }
        send_msg(
            &st.peers,
            member,
            RemoteS2C::HiddenSessionEnded { session_id, reason },
        );
    }
    true
}

/// 拆除 `device_id` 参与的**全部**隐藏会话（它可能同时是多条会话的 target）。
///
/// # 为何不能沿用 [`teardown_session`]
/// 那个函数假定「一台设备至多一条会话」（`sessions.remove(device_id)` 一次就完了）。
/// 隐藏桶恰恰打破了这个假定，所以这里必须先收集再逐条拆。**这也是拆表而不是复用
/// `sessions` 的直接后果之一**，写在这里免得后人以为是重复代码。
fn teardown_hidden_for_device(st: &mut HubState, device_id: &str, reason: EndReason) {
    let sids: Vec<HiddenSessionId> = st
        .hidden
        .iter()
        .filter(|(_, e)| e.controller == device_id || e.target == device_id)
        .map(|(sid, _)| *sid)
        .collect();
    for sid in sids {
        teardown_hidden_one(st, sid, reason, Some(device_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_protocol::remote::RemoteFrame;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    /// 注册一台设备，返回 (conn_id, 该连接的接收端)。
    fn join(hub: &Hub, device_id: &str, user_id: &str, name: &str) -> (u64, UnboundedReceiver<ToClient>) {
        let (tx, rx) = unbounded_channel();
        let cid = hub.register(device_id, user_id.to_string(), name.to_string(), tx);
        (cid, rx)
    }

    /// 取下一条协议消息（非 Close）；无则 panic（测试断言）。
    fn next_msg(rx: &mut UnboundedReceiver<ToClient>) -> RemoteS2C {
        match rx.try_recv() {
            Ok(ToClient::Msg(m)) => *m,
            Ok(ToClient::Close) => panic!("收到 Close，期望消息"),
            Err(e) => panic!("无消息: {e:?}"),
        }
    }

    /// 排空并返回全部已到协议消息。
    fn drain(rx: &mut UnboundedReceiver<ToClient>) -> Vec<RemoteS2C> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ToClient::Msg(m) = ev {
                out.push(*m);
            }
        }
        out
    }

    /// 驱动一次完整成功配对，返回 (controller_rx, controlled_rx)。
    fn pair_ok(
        hub: &Hub,
        c_id: &str,
        c_cid: u64,
        t_id: &str,
        c_rx: &mut UnboundedReceiver<ToClient>,
        t_rx: &mut UnboundedReceiver<ToClient>,
    ) {
        hub.request_control(c_id, c_cid, t_id, false);
        // 被控端收到 ControlRequested（含配对码）。
        let code = match next_msg(t_rx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            other => panic!("期望 ControlRequested，得 {other:?}"),
        };
        // 控制端收到 PairingNeeded。
        assert!(matches!(next_msg(c_rx), RemoteS2C::PairingNeeded { .. }));
        let _ = hub.submit_pairing(c_id, c_cid, t_id, &code);
        // 双方收到 SessionStarted。
        assert!(matches!(
            next_msg(c_rx),
            RemoteS2C::SessionStarted { role: Role::Controller, .. }
        ));
        assert!(matches!(
            next_msg(t_rx),
            RemoteS2C::SessionStarted { role: Role::Controlled, .. }
        ));
    }

    #[test]
    fn 配对成功并建立双向会话() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "user-1", "控制机");
        let (_t_cid, mut t_rx) = join(&hub, "tgt", "user-1", "被控机");
        pair_ok(&hub, "ctrl", c_cid, "tgt", &mut c_rx, &mut t_rx);
        // SessionStarted 对端信息正确。
        // （pair_ok 已断言角色；此处补一条 relay 验证通路。）
        let frame = RemoteFrame::Echo("ping".into()).to_value().expect("to_value");
        hub.handle("ctrl", c_cid, RemoteC2S::Relay(frame.clone()));
        match next_msg(&mut t_rx) {
            RemoteS2C::Relay(v) => assert_eq!(
                RemoteFrame::from_value(&v).expect("还原帧"),
                RemoteFrame::Echo("ping".into())
            ),
            other => panic!("期望 Relay，得 {other:?}"),
        }
    }

    #[test]
    fn 错误配对码递减尝试且不建会话() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        let (_t, mut t_rx) = join(&hub, "tgt", "u", "t");
        hub.request_control("ctrl", c_cid, "tgt", false);
        let _code = match next_msg(&mut t_rx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            o => panic!("{o:?}"),
        };
        let _ = next_msg(&mut c_rx); // PairingNeeded
        let _ = hub.submit_pairing("ctrl", c_cid, "tgt", "000000000");
        match next_msg(&mut c_rx) {
            RemoteS2C::PairingResult { reason: PairingFailReason::InvalidCode, attempts_left } => {
                assert_eq!(attempts_left, PAIRING_MAX_ATTEMPTS - 1);
            }
            o => panic!("期望 InvalidCode，得 {o:?}"),
        }
    }

    #[test]
    fn 连错超限作废配对() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        let (_t, mut t_rx) = join(&hub, "tgt", "u", "t");
        hub.request_control("ctrl", c_cid, "tgt", false);
        let real = match next_msg(&mut t_rx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            o => panic!("{o:?}"),
        };
        let _ = drain(&mut c_rx);
        // 用一个保证错误的码（与真码不同）。
        let wrong = if real == "111111111" { "222222222" } else { "111111111" };
        for _ in 0..PAIRING_MAX_ATTEMPTS {
            let _ = hub.submit_pairing("ctrl", c_cid, "tgt", wrong);
        }
        let msgs = drain(&mut c_rx);
        assert!(msgs.iter().any(|m| matches!(
            m,
            RemoteS2C::PairingResult { reason: PairingFailReason::TooManyAttempts, .. }
        )));
        // 被控端收到取消。
        assert!(drain(&mut t_rx).iter().any(|m| matches!(
            m,
            RemoteS2C::PairingCancelled { reason: DenyReason::TooManyAttempts }
        )));
    }

    #[test]
    fn 已配对免码直连() {
        // paired=true：跳过配对码，双方直接 SessionStarted（无 ControlRequested/PairingNeeded）。
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "ctrl", "u", "c");
        let (_t, mut trx) = join(&hub, "tgt", "u", "t");
        hub.request_control("ctrl", c, "tgt", true);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::SessionStarted { role: Role::Controller, .. }
        ));
        assert!(matches!(
            next_msg(&mut trx),
            RemoteS2C::SessionStarted { role: Role::Controlled, .. }
        ));
        // 不应有配对码相关消息。
        assert!(drain(&mut trx).is_empty());
    }

    #[test]
    fn 跨账户控制被拒() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "user-A", "c");
        let (_t, _t_rx) = join(&hub, "tgt", "user-B", "t");
        hub.request_control("ctrl", c_cid, "tgt", false);
        assert!(matches!(
            next_msg(&mut c_rx),
            RemoteS2C::ControlDenied { reason: DenyReason::CrossUser, .. }
        ));
    }

    #[test]
    fn 自控被拒() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        hub.request_control("ctrl", c_cid, "ctrl", false);
        assert!(matches!(
            next_msg(&mut c_rx),
            RemoteS2C::ControlDenied { reason: DenyReason::SelfControl, .. }
        ));
    }

    #[test]
    fn 离线目标被拒() {
        let hub = Hub::new();
        let (c_cid, mut c_rx) = join(&hub, "ctrl", "u", "c");
        hub.request_control("ctrl", c_cid, "ghost", false);
        assert!(matches!(
            next_msg(&mut c_rx),
            RemoteS2C::ControlDenied { reason: DenyReason::Offline, .. }
        ));
    }

    #[test]
    fn 目标已被控时第三方被拒() {
        let hub = Hub::new();
        let (c1, mut c1rx) = join(&hub, "ctrl1", "u", "c1");
        let (_t, mut trx) = join(&hub, "tgt", "u", "t");
        pair_ok(&hub, "ctrl1", c1, "tgt", &mut c1rx, &mut trx);
        let (c2, mut c2rx) = join(&hub, "ctrl2", "u", "c2");
        hub.request_control("ctrl2", c2, "tgt", false);
        assert!(matches!(
            next_msg(&mut c2rx),
            RemoteS2C::ControlDenied { reason: DenyReason::AlreadyControlled, .. }
        ));
    }

    #[test]
    fn 非发起者无法抢答配对() {
        let hub = Hub::new();
        let (c1, mut c1rx) = join(&hub, "ctrl1", "u", "c1");
        let (_t, mut trx) = join(&hub, "tgt", "u", "t");
        hub.request_control("ctrl1", c1, "tgt", false);
        let code = match next_msg(&mut trx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            o => panic!("{o:?}"),
        };
        let _ = drain(&mut c1rx);
        // ctrl2 不是发起者，即便知道真码也应被拒（NoPending，不泄漏）。
        let (c2, mut c2rx) = join(&hub, "ctrl2", "u", "c2");
        let _ = hub.submit_pairing("ctrl2", c2, "tgt", &code);
        assert!(matches!(
            next_msg(&mut c2rx),
            RemoteS2C::PairingResult { reason: PairingFailReason::NoPending, .. }
        ));
        // 原配对仍在：ctrl1 用真码仍可成功。
        // （此处不再重复，覆盖点是抢答被挡。）
    }

    #[test]
    fn 被控端拒绝控制请求() {
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "ctrl", "u", "c");
        let (t, mut trx) = join(&hub, "tgt", "u", "t");
        hub.request_control("ctrl", c, "tgt", false);
        let _ = next_msg(&mut trx); // ControlRequested
        let _ = next_msg(&mut crx); // PairingNeeded
        hub.handle("tgt", t, RemoteC2S::DeclineControl);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::ControlDenied { reason: DenyReason::RejectedByUser, .. }
        ));
    }

    #[test]
    fn 断线拆会话并通知对端() {
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "ctrl", "u", "c");
        let (t, mut trx) = join(&hub, "tgt", "u", "t");
        pair_ok(&hub, "ctrl", c, "tgt", &mut crx, &mut trx);
        hub.disconnect("tgt", t);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::SessionEnded { reason: EndReason::PeerDisconnected }
        ));
        // 会话已拆：控制端再 relay 应被丢弃（对端收不到）。
        let frame = RemoteFrame::Echo("x".into()).to_value().expect("to_value");
        hub.handle("ctrl", c, RemoteC2S::Relay(frame));
        assert!(drain(&mut trx).is_empty());
    }

    #[test]
    fn 重复连接驱逐旧连接() {
        let hub = Hub::new();
        let (_c1, mut c1rx) = join(&hub, "dev", "u", "d");
        // 同 device 第二次连接：旧连接应收 Close。
        let (_c2, _c2rx) = join(&hub, "dev", "u", "d");
        let got_close = {
            let mut closed = false;
            while let Ok(ev) = c1rx.try_recv() {
                if matches!(ev, ToClient::Close) {
                    closed = true;
                }
            }
            closed
        };
        assert!(got_close, "旧连接应被驱逐收到 Close");
    }

    #[test]
    fn 旧连接迟到消息被守卫忽略() {
        let hub = Hub::new();
        let (c1, _c1rx) = join(&hub, "dev", "u", "d");
        let (_c2, _c2rx) = join(&hub, "dev", "u", "d"); // 驱逐 c1
        let (_t, mut trx) = join(&hub, "tgt", "u", "t");
        // 用旧 conn_id 发消息：应被 is_current 守卫忽略，目标收不到请求。
        hub.request_control("dev", c1, "tgt", false);
        assert!(drain(&mut trx).is_empty());
    }

    #[test]
    fn 控制端已忙不能再控第二台() {
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "ctrl", "u", "c");
        let (_t1, mut t1rx) = join(&hub, "t1", "u", "t1");
        pair_ok(&hub, "ctrl", c, "t1", &mut crx, &mut t1rx);
        let (_t2, _t2rx) = join(&hub, "t2", "u", "t2");
        hub.request_control("ctrl", c, "t2", false);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::ControlDenied { reason: DenyReason::ControllerBusy, .. }
        ));
    }

    // ════════════════ M7 片 4b：隐藏会话（以上 13 个既有测试一字未改）════════════════

    /// 让某设备上报 [`HIDDEN_CAP`]（模拟新版本 PC 连上后的第一条 `ClientHello`）。
    fn say_hello(hub: &Hub, device_id: &str, conn_id: u64) {
        hub.handle(
            device_id,
            conn_id,
            RemoteC2S::ClientHello {
                protocol_version: PROTOCOL_VERSION,
                caps: vec![HIDDEN_CAP.to_string()],
            },
        );
    }

    /// 驱动一次完整成功的**隐藏会话**配对，返回服务端分配的 `session_id`。
    fn open_hidden_ok(
        hub: &Hub,
        c_id: &str,
        c_cid: u64,
        t_id: &str,
        c_rx: &mut UnboundedReceiver<ToClient>,
        t_rx: &mut UnboundedReceiver<ToClient>,
    ) -> HiddenSessionId {
        hub.open_hidden(c_id, c_cid, t_id, false);
        // 被控端收到的是**独立变体** HiddenControlRequested（不是 ControlRequested）。
        let code = match next_msg(t_rx) {
            RemoteS2C::HiddenControlRequested { pairing_code, .. } => pairing_code,
            other => panic!("期望 HiddenControlRequested，得 {other:?}"),
        };
        // 发起方收到**复用**的 PairingNeeded。
        assert!(matches!(next_msg(c_rx), RemoteS2C::PairingNeeded { .. }));
        let _ = hub.submit_pairing(c_id, c_cid, t_id, &code);
        let sid = match next_msg(c_rx) {
            RemoteS2C::HiddenSessionStarted {
                session_id,
                role: Role::Controller,
                ..
            } => session_id,
            other => panic!("期望 HiddenSessionStarted(Controller)，得 {other:?}"),
        };
        assert!(matches!(
            next_msg(t_rx),
            RemoteS2C::HiddenSessionStarted { session_id, role: Role::Controlled, .. }
                if session_id == sid
        ));
        assert_ne!(sid, 0, "session_id 0 是哨兵，服务端不得分配");
        sid
    }

    #[test]
    fn 隐藏会话配对不得写入设备信任_也不得复用镜像信任() {
        // 本片修掉的一条**同意范围逃逸**。两个方向各一半：
        //  a) 手机为「跟 PC 上的 LLM 说话」念一次码，那行信任不得顺带授出**镜像**权限；
        //  b) 一台早年为镜像配过对的桌面端，不得免码开出一条**隐藏**会话
        //     （隐藏会话在被控端无横幅、无指示器、无结束入口）。
        // 服务端这一半的判据是 `submit_pairing` 的返回值：`Some` ⇒ ws.rs 会 persist_pair。
        let hub = Hub::new();
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        let (t, mut trx) = join(&hub, "pc", "u", "PC");
        say_hello(&hub, "pc", t);

        hub.open_hidden("phone", p, "pc", false);
        let code = match next_msg(&mut trx) {
            RemoteS2C::HiddenControlRequested { pairing_code, .. } => pairing_code,
            other => panic!("期望 HiddenControlRequested，得 {other:?}"),
        };
        assert!(matches!(next_msg(&mut prx), RemoteS2C::PairingNeeded { .. }));

        let persisted = hub.submit_pairing("phone", p, "pc", &code);
        assert!(
            persisted.is_none(),
            "隐藏会话配对成功也不得返回设备对 —— 返回了 ws.rs 就会把它写进 device_pairs，\
             而那张表没有会话种类列，等于顺带授出镜像权限"
        );
        // 会话本身仍然建起来了（只是不留信任）。
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::HiddenSessionStarted { role: Role::Controller, .. }
        ));
        assert!(matches!(
            next_msg(&mut trx),
            RemoteS2C::HiddenSessionStarted { role: Role::Controlled, .. }
        ));

        // 对照组：镜像配对**仍然**要返回设备对，否则免码重连就没了。
        let (c, mut crx) = join(&hub, "desk", "u", "桌面");
        let (_t2, mut t2rx) = join(&hub, "pc2", "u", "PC2");
        hub.request_control("desk", c, "pc2", false);
        let code2 = match next_msg(&mut t2rx) {
            RemoteS2C::ControlRequested { pairing_code, .. } => pairing_code,
            other => panic!("期望 ControlRequested，得 {other:?}"),
        };
        assert!(matches!(next_msg(&mut crx), RemoteS2C::PairingNeeded { .. }));
        assert_eq!(
            hub.submit_pairing("desk", c, "pc2", &code2),
            Some(("desk".to_string(), "pc2".to_string())),
            "镜像配对的免码信任是既有行为，不得被本次改动误伤"
        );
    }

    #[test]
    fn 畸形caps被整条忽略且不改变已判定的能力位() {
        // `ClientHello.caps` 是对端可控、无长度上限的 Vec<String>，且可在一条连接上无限重发。
        // 入口不夹紧 ⇒ 一条 4 MiB 消息换约 40 MB 常驻堆 + 刷爆默认级别日志。
        let hub = Hub::new();
        let (t, mut trx) = join(&hub, "pc", "u", "PC");
        let (p, mut prx) = join(&hub, "phone", "u", "手机");

        // 项数超限：整条忽略 ⇒ 能力位仍是 false ⇒ 在生成配对码之前被拒。
        hub.handle(
            "pc",
            t,
            RemoteC2S::ClientHello {
                protocol_version: PROTOCOL_VERSION,
                caps: (0..MAX_CAPS_LEN + 1)
                    .map(|_| HIDDEN_CAP.to_string())
                    .collect(),
            },
        );
        hub.open_hidden("phone", p, "pc", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::Offline, .. }
        ));
        assert!(drain(&mut trx).is_empty(), "被拒时 PC 上不该出现配对横幅");

        // 单项超长：同样整条忽略。
        hub.handle(
            "pc",
            t,
            RemoteC2S::ClientHello {
                protocol_version: PROTOCOL_VERSION,
                caps: vec!["x".repeat(MAX_CAP_LEN + 1)],
            },
        );
        hub.open_hidden("phone", p, "pc", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::Offline, .. }
        ));

        // 合规的 caps 照常生效。
        say_hello(&hub, "pc", t);
        hub.open_hidden("phone", p, "pc", false);
        assert!(matches!(
            next_msg(&mut trx),
            RemoteS2C::HiddenControlRequested { .. }
        ));
    }

    #[test]
    fn 桌面镜像与手机隐藏会话可并存() {
        // ★ 本片存在的全部理由：同一台 PC 上，桌面控制端的镜像会话与手机的隐藏会话
        // 同时活着，且两条数据面互不串台。
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "desktop", "u", "桌面");
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        // 1) 桌面控制端先建镜像会话。
        pair_ok(&hub, "desktop", c, "pc", &mut crx, &mut trx);
        // 2) 手机在镜像会话**开着**的情况下建隐藏会话——改动前这里会被
        //    AlreadyControlled 直接拒（独占仲裁按被控设备 device_id 做）。
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        let sid = open_hidden_ok(&hub, "phone", p, "pc", &mut prx, &mut trx);
        // 3) 镜像会话仍在：桌面 → PC 的 Relay 照常送达。
        let frame = RemoteFrame::Echo("mirror".into()).to_value().expect("to_value");
        hub.handle("desktop", c, RemoteC2S::Relay(frame));
        assert!(matches!(next_msg(&mut trx), RemoteS2C::Relay(_)));
        // 4) 隐藏会话数据面走 RelayTo{sid}，PC 收得到且带回 sid。
        let llm = RemoteFrame::Echo("hidden".into()).to_value().expect("to_value");
        hub.handle(
            "phone",
            p,
            RemoteC2S::RelayTo {
                session_id: sid,
                payload: llm.clone(),
            },
        );
        match next_msg(&mut trx) {
            RemoteS2C::RelayTo { session_id, payload } => {
                assert_eq!(session_id, sid);
                assert_eq!(
                    RemoteFrame::from_value(&payload).expect("还原"),
                    RemoteFrame::Echo("hidden".into())
                );
            }
            other => panic!("期望 RelayTo，得 {other:?}"),
        }
        // 5) 隐藏会话的帧**绝不**串到镜像会话去：桌面控制端一条都不该收到。
        assert!(
            drain(&mut crx).is_empty(),
            "隐藏会话的数据面串进了镜像会话"
        );
        // 6) 反向同理：PC 经 sid 回帧只到手机，不到桌面。
        hub.handle(
            "pc",
            t,
            RemoteC2S::RelayTo {
                session_id: sid,
                payload: llm,
            },
        );
        assert!(matches!(next_msg(&mut prx), RemoteS2C::RelayTo { .. }));
        assert!(drain(&mut crx).is_empty(), "PC 的隐藏会话回帧串进了镜像会话");
    }

    #[test]
    fn 隐藏会话数达上限被拒() {
        let hub = Hub::new();
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        // 占满 MAX_HIDDEN_SESSIONS_PER_TARGET 个名额。
        for i in 0..MAX_HIDDEN_SESSIONS_PER_TARGET {
            let id = format!("phone-{i}");
            let (p, mut prx) = join(&hub, &id, "u", "手机");
            open_hidden_ok(&hub, &id, p, "pc", &mut prx, &mut trx);
        }
        // 再来一部：AlreadyControlled（复用拒因，手机端文案负责说出具体数字）。
        let (p, mut prx) = join(&hub, "phone-extra", "u", "多出来的手机");
        hub.open_hidden("phone-extra", p, "pc", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::AlreadyControlled, .. }
        ));
        // 被拒发生在**生成配对码之前**：PC 上不该多出一个横幅。
        assert!(drain(&mut trx).is_empty(), "超限请求不得在被控端弹出配对码");
        // 而**镜像**名额与隐藏名额相互独立：隐藏桶满员时桌面控制端照样能建镜像会话。
        let (c, mut crx) = join(&hub, "desktop", "u", "桌面");
        pair_ok(&hub, "desktop", c, "pc", &mut crx, &mut trx);
    }

    #[test]
    fn 同一手机不能开第二条隐藏会话() {
        let hub = Hub::new();
        let (t1, mut t1rx) = join(&hub, "pc1", "u", "PC1");
        let (t2, mut t2rx) = join(&hub, "pc2", "u", "PC2");
        say_hello(&hub, "pc1", t1);
        say_hello(&hub, "pc2", t2);
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        open_hidden_ok(&hub, "phone", p, "pc1", &mut prx, &mut t1rx);
        // 一部手机同时只连 1 台 PC。
        hub.open_hidden("phone", p, "pc2", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::ControllerBusy, .. }
        ));
        assert!(drain(&mut t2rx).is_empty());
        // 对同一台 PC 再开一条也是 ControllerBusy。
        hub.open_hidden("phone", p, "pc1", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::ControllerBusy, .. }
        ));
    }

    #[test]
    fn 非成员的_relay_to_被静默丢弃() {
        // R21：session_id 是全局单调 u64、**可猜**，CrossUser 只挡跨账户。没有成员
        // 校验的话，同账户任一设备就能往别人 PC 上投喂 prompt。
        let hub = Hub::new();
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        let sid = open_hidden_ok(&hub, "phone", p, "pc", &mut prx, &mut trx);
        // 同账户的第三台设备猜中了 sid。
        let (e, mut erx) = join(&hub, "evil", "u", "同账户另一台");
        let frame = RemoteFrame::Echo("注入".into()).to_value().expect("to_value");
        hub.handle(
            "evil",
            e,
            RemoteC2S::RelayTo {
                session_id: sid,
                payload: frame.clone(),
            },
        );
        assert!(drain(&mut trx).is_empty(), "非成员的帧被转给了被控端");
        assert!(drain(&mut prx).is_empty(), "非成员的帧被转给了发起方");
        // 且**不回执**——回执会变成探测「哪些 sid 存在」的旁路。
        assert!(drain(&mut erx).is_empty(), "不命中不得回执");
        // 猜错 sid 同样静默。
        hub.handle(
            "evil",
            e,
            RemoteC2S::RelayTo {
                session_id: sid.wrapping_add(999),
                payload: frame,
            },
        );
        assert!(drain(&mut erx).is_empty());
        // 非成员也不能拆掉别人的会话（否则就是一个拒绝服务面）。
        hub.handle("evil", e, RemoteC2S::EndHidden { session_id: sid });
        assert!(drain(&mut trx).is_empty(), "非成员拆掉了别人的隐藏会话");
        assert!(drain(&mut prx).is_empty());
    }

    #[test]
    fn 未上报能力的老端被拒隐藏会话() {
        // 老 PC 根本不发 ClientHello ⇒ caps 为空 ⇒ 在生成配对码**之前**被回 Offline。
        // 「无声降级」在这里被消除：手机立刻拿到明确拒绝，不必干等 120 秒 TTL。
        let hub = Hub::new();
        let (_t, mut trx) = join(&hub, "old-pc", "u", "老版本 PC");
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        hub.open_hidden("phone", p, "old-pc", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::Offline, .. }
        ));
        // 老 PC 上不能出现一个念了也没用的配对码横幅。
        assert!(drain(&mut trx).is_empty(), "老端不得收到配对码");
        // 而老 PC 的**镜像**会话完全不受影响（它一个字节的线格式都没变）。
        let (c, mut crx) = join(&hub, "desktop", "u", "桌面");
        pair_ok(&hub, "desktop", c, "old-pc", &mut crx, &mut trx);
    }

    #[test]
    fn 断线拆除该设备全部隐藏会话且不动镜像会话() {
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "desktop", "u", "桌面");
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        pair_ok(&hub, "desktop", c, "pc", &mut crx, &mut trx);
        let (p1, mut p1rx) = join(&hub, "phone-1", "u", "手机一");
        let (p2, mut p2rx) = join(&hub, "phone-2", "u", "手机二");
        let sid1 = open_hidden_ok(&hub, "phone-1", p1, "pc", &mut p1rx, &mut trx);
        let sid2 = open_hidden_ok(&hub, "phone-2", p2, "pc", &mut p2rx, &mut trx);
        assert_ne!(sid1, sid2, "session_id 必须不复用");

        // 1) 一部手机断线：只拆它那一条，另一条与镜像会话都不受影响。
        hub.disconnect("phone-1", p1);
        assert!(matches!(
            next_msg(&mut trx),
            RemoteS2C::HiddenSessionEnded { session_id, reason: EndReason::PeerDisconnected }
                if session_id == sid1
        ));
        assert!(drain(&mut crx).is_empty(), "手机断线波及了镜像会话");
        let frame = RemoteFrame::Echo("still-alive".into()).to_value().expect("to_value");
        hub.handle(
            "phone-2",
            p2,
            RemoteC2S::RelayTo {
                session_id: sid2,
                payload: frame.clone(),
            },
        );
        assert!(matches!(next_msg(&mut trx), RemoteS2C::RelayTo { .. }));

        // 2) PC 断线：**全部**隐藏会话一起拆（它同时是多条会话的 target），
        //    镜像会话也拆（那是既有行为，走 teardown_session）。
        hub.disconnect("pc", t);
        let phone2_msgs = drain(&mut p2rx);
        assert!(
            phone2_msgs.iter().any(|m| matches!(
                m,
                RemoteS2C::HiddenSessionEnded { session_id, reason: EndReason::PeerDisconnected }
                    if *session_id == sid2
            )),
            "PC 断线未拆除隐藏会话，得 {phone2_msgs:?}"
        );
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::SessionEnded { reason: EndReason::PeerDisconnected }
        ));
        // 3) 会话已拆：再发 RelayTo 转不出去（幽灵条目会在这里暴露）。
        hub.handle(
            "phone-2",
            p2,
            RemoteC2S::RelayTo {
                session_id: sid2,
                payload: frame,
            },
        );
        assert!(drain(&mut p2rx).is_empty());
    }

    #[test]
    fn 主动结束隐藏会话通知对端且不回执自己() {
        let hub = Hub::new();
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        let sid = open_hidden_ok(&hub, "phone", p, "pc", &mut prx, &mut trx);
        hub.handle("phone", p, RemoteC2S::EndHidden { session_id: sid });
        assert!(matches!(
            next_msg(&mut trx),
            RemoteS2C::HiddenSessionEnded { session_id, reason: EndReason::PeerLeft }
                if session_id == sid
        ));
        assert!(drain(&mut prx).is_empty(), "主动方不该收到回执");
        // 拆完之后名额释放：同一部手机可以再开一条。
        let sid2 = open_hidden_ok(&hub, "phone", p, "pc", &mut prx, &mut trx);
        assert_ne!(sid, sid2, "session_id 不得复用");
    }

    #[test]
    fn 隐藏会话配对期间桌面控制端被拒配对中() {
        // 唯一一处**有意保留**的跨桶互斥：pending 按 target 单槽。配对码是给人念的，
        // 同一时刻 PC 屏幕上只该出现一个码。这条断言防止后人把它当 bug「修掉」。
        let hub = Hub::new();
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        hub.open_hidden("phone", p, "pc", false);
        assert!(matches!(
            next_msg(&mut trx),
            RemoteS2C::HiddenControlRequested { .. }
        ));
        let _ = next_msg(&mut prx); // PairingNeeded
        let (c, mut crx) = join(&hub, "desktop", "u", "桌面");
        hub.request_control("desktop", c, "pc", false);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::ControlDenied { reason: DenyReason::TargetPairing, .. }
        ));
        // 反向：镜像配对进行中，手机也拿 TargetPairing。
        let hub2 = Hub::new();
        let (t2, mut t2rx) = join(&hub2, "pc", "u", "工作站");
        say_hello(&hub2, "pc", t2);
        let (c2, mut c2rx) = join(&hub2, "desktop", "u", "桌面");
        hub2.request_control("desktop", c2, "pc", false);
        assert!(matches!(next_msg(&mut t2rx), RemoteS2C::ControlRequested { .. }));
        let _ = next_msg(&mut c2rx); // PairingNeeded
        let (p2, mut p2rx) = join(&hub2, "phone", "u", "手机");
        hub2.open_hidden("phone", p2, "pc", false);
        assert!(matches!(
            next_msg(&mut p2rx),
            RemoteS2C::ControlDenied { reason: DenyReason::TargetPairing, .. }
        ));
    }

    #[test]
    fn 隐藏会话已配对免码直连() {
        // paired=true：跳过配对码，双方直接 HiddenSessionStarted（与镜像路径同构）。
        let hub = Hub::new();
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        let (p, mut prx) = join(&hub, "phone", "u", "手机");
        hub.open_hidden("phone", p, "pc", true);
        let sid = match next_msg(&mut prx) {
            RemoteS2C::HiddenSessionStarted { session_id, role: Role::Controller, .. } => session_id,
            other => panic!("期望 HiddenSessionStarted，得 {other:?}"),
        };
        assert!(matches!(
            next_msg(&mut trx),
            RemoteS2C::HiddenSessionStarted { session_id, role: Role::Controlled, .. }
                if session_id == sid
        ));
        // 免码路径不该有任何配对码消息。
        assert!(drain(&mut trx).is_empty());
        // 免码路径同样受 caps 门与上限约束：老端直连也被拒。
        let (_old, mut oldrx) = join(&hub, "old-pc", "u", "老 PC");
        let (p2, mut p2rx) = join(&hub, "phone-2", "u", "手机二");
        hub.open_hidden("phone-2", p2, "old-pc", true);
        assert!(matches!(
            next_msg(&mut p2rx),
            RemoteS2C::ControlDenied { reason: DenyReason::Offline, .. }
        ));
        assert!(drain(&mut oldrx).is_empty());
    }

    #[test]
    fn 隐藏会话跨账户与自控被拒() {
        // precheck 是等价重构：两条发起路径共用同一套 SelfControl / CrossUser 判定。
        let hub = Hub::new();
        let (t, _trx) = join(&hub, "pc", "user-B", "别人的 PC");
        say_hello(&hub, "pc", t);
        let (p, mut prx) = join(&hub, "phone", "user-A", "手机");
        hub.open_hidden("phone", p, "pc", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::CrossUser, .. }
        ));
        hub.open_hidden("phone", p, "phone", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::SelfControl, .. }
        ));
        hub.open_hidden("phone", p, "ghost", false);
        assert!(matches!(
            next_msg(&mut prx),
            RemoteS2C::ControlDenied { reason: DenyReason::Offline, .. }
        ));
    }

    #[test]
    fn 重连驱逐旧连接时拆除其隐藏会话() {
        // register 里的 teardown_hidden_for_device：少这一行，被驱逐的设备会在
        // hidden 里留下永久幽灵条目，一路吃掉目标 PC 的名额（片 2 §6.9 缺陷⑤同形）。
        let hub = Hub::new();
        let (t, mut trx) = join(&hub, "pc", "u", "工作站");
        say_hello(&hub, "pc", t);
        let mut last = 0;
        for round in 0..(MAX_HIDDEN_SESSIONS_PER_TARGET + 2) {
            // 同 device_id 重连：驱逐上一条连接。名额若不还回来，第 MAX+1 轮就会
            // 被 AlreadyControlled 拒。
            let (p, mut prx) = join(&hub, "phone", "u", "手机");
            if round > 0 {
                // 驱逐通知先到（对端 PC 收 HiddenSessionEnded{Replaced}），清掉再往下走。
                let evicted = drain(&mut trx);
                assert!(
                    evicted.iter().any(|m| matches!(
                        m,
                        RemoteS2C::HiddenSessionEnded { session_id, reason: EndReason::Replaced }
                            if *session_id == last
                    )),
                    "重连驱逐未拆除旧连接的隐藏会话，得 {evicted:?}"
                );
            }
            last = open_hidden_ok(&hub, "phone", p, "pc", &mut prx, &mut trx);
        }
    }

    #[test]
    fn 老客户端不发_clienthello_不影响镜像会话() {
        // 兼容矩阵「新服务端 × 老客户端」：老端从不发 ClientHello，caps 恒为空，
        // 但镜像的请求 / 配对 / 会话 / 中继 / 断开五条路径必须一个字节不变地照跑。
        let hub = Hub::new();
        let (c, mut crx) = join(&hub, "old-ctrl", "u", "老控制端");
        let (t, mut trx) = join(&hub, "old-tgt", "u", "老被控端");
        pair_ok(&hub, "old-ctrl", c, "old-tgt", &mut crx, &mut trx);
        let frame = RemoteFrame::Echo("ping".into()).to_value().expect("to_value");
        hub.handle("old-ctrl", c, RemoteC2S::Relay(frame));
        assert!(matches!(next_msg(&mut trx), RemoteS2C::Relay(_)));
        hub.handle("old-tgt", t, RemoteC2S::EndSession);
        assert!(matches!(
            next_msg(&mut crx),
            RemoteS2C::SessionEnded { reason: EndReason::PeerLeft }
        ));
    }
}
