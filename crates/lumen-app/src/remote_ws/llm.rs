//! M7 片 4：**LLM 数据面状态机**——把 `RemoteFrame::Llm` 与 [`crate::llm_runner`] 接上。
//!
//! 片 0 落了协议（`lumen-protocol/src/llm.rs`，30 个 `LlmFrame` 变体），片 2/3 落了
//! headless runner 与 Claude 适配器，两边之间那条线一直是 `remote_ws.rs` 里一个显式
//! `RemoteFrame::Llm(_) => {}` 空臂。本模块就是那条线。
//!
//! ```text
//!  控制端(手机 / 另一台 PC)                 被控端(本机 PC)
//!        │                                        │
//!        │  RemoteFrame::Llm(LlmFrame::…)         │
//!        ├───────────────► apply_relay ───────────┤
//!        │                     │                  │
//!        │                     ▼                  │
//!        │              LlmPlane::enqueue         │  ← 只入队，不在这里执行
//!        │                     │                  │
//!        │        （同一轮事件循环，晚一步）        │
//!        │                     ▼                  │
//!        │   LlmPlane::pump(runners, events) ─────┤  ← 唯一持 &mut LlmRunnerManager 的地方
//!        │        ①入站帧 → runner 操作            │
//!        │        ②runner 事件 → LlmFrame          │
//!        │        ③33 ms 合并窗口 flush            │
//!        │                     │                  │
//!        │◄──── send_frame ────┘                  │
//! ```
//!
//! # 一、为什么入站帧要「先入队、晚一步执行」
//!
//! `apply_relay` 挂在 `RemoteWs::poll()` 上，那条路径**拿不到** `LlmRunnerManager`
//! ——管理器挂在 `AppState.llm_runners`，与 `AppState.remote_ws` 是**平级字段**
//! （`llm_runner` 模块文档一：容器必须与 `tabs` 平行，绝不塞进 `RemoteWs`）。
//!
//! 被否决的三个替代方案，各自的硬伤：
//!
//! | 方案 | 硬伤 |
//! |---|---|
//! | 把 `LlmRunnerManager` 搬进 `RemoteWs` | runner 生命周期与远程会话**刻意解耦**（§6.7：手机断线 runner 照跑）。搬进去之后 `stop()` / `teardown_session_state` 会顺手把子进程一起清掉，正是硬契约禁止的那件事 |
//! | 给 `apply_relay` 加 `&mut LlmRunnerManager` 参数 | 要一路改 `apply` / `apply_server` / `poll`，而 `poll()` 的调用点在 `main.rs`——本轮明令不改那个文件 |
//! | 在 `apply_relay` 里直接起子进程 | 起进程要 IO（建附件目录、探 PATH），塞进 WS 帧解析路径上等于让一次慢盘把控制面卡住 |
//!
//! 入队的延迟代价是**一次事件循环迭代**：`main.rs` 里 `pump_remote()`（收帧）与
//! `pump_llm_runners()`（本模块的泵）是相邻两行，同一轮 `PtyWake` 内跑完，实测量级 <1 ms。
//!
//! # 二、seq 的分配规则（这条错了自愈通道会变成刷屏器）
//!
//! `LlmFrame::Delta.seq` 的协议不变量是「同一 `(conv_id, conv_generation)` 内**严格递增
//! 且连续**，一断裂控制端就发 `TurnFetch` 拉整轮快照」。而带 `seq` 字段的帧有 7 种
//! （`ConvStarted` / `Attached` / `Delta` / `TurnStarted` / `TurnEnded` / `ConvExited` /
//! `TurnSnapshot`）。**只有 `Delta` 消费序号，其余 6 种都只是给当前水位盖个戳**：
//!
//! ```text
//!   Delta(seq=1) TurnStarted(seq=1) Delta(seq=2) Delta(seq=3) TurnEnded(seq=3)
//!                     ▲ 盖戳，不占号                                ▲ 盖戳
//! ```
//!
//! 若让 `TurnStarted` 也领一个号，`Delta` 序列立刻变成 1、3、4 —— 控制端每一轮都会看见
//! 一个「缺口」并触发一次整轮快照拉取。协议注释在 `LlmFrame::RateLimit` 上把这个坑写死过
//! 一次（那一帧干脆不带 `seq`），本模块把同一条规则落到实现上。
//!
//! # 三、白名单严出：本模块能守住的那一道，与守不住的那一道
//!
//! 上游内容的白名单在 `llm_runner::event::classify`（第一道门）与 `claude.rs` 的归一化
//! （第二道门）就已经执行完了：`RunnerEvent` 的载荷**全部是协议类型**，没有
//! `serde_json::Value` 直通口子，所以「顺手把上游某个没建模的字段带出去」在类型上写不出来。
//!
//! 本模块守的是**第三道**——本机产物别混进帧里，具体两条：
//!
//! 1. **`ExitAttribution::FromStderr{tail}` 绝不上行**。退出归因里那份 `tail` 是 CLI
//!    stderr 的最后 64 行，装着本机路径与内部堆栈（`llm_runner` 模块文档二末条为此专门
//!    把 stderr 排除在 transcript 之外）。[`exit_message`] 只回**本端写死的短文案 + 退出码**，
//!    有单测 `片4_退出归因不得把stderr尾部写进帧` 盯着。
//! 2. **兜底变体永不发送**。`LlmFrame::Unknown` / `LlmBlock::Unknown` / `LlmDeltaItem::Unknown`
//!    是反序列化兜底，发出去等于告诉对端「我这有个我自己也不认识的东西」。[`sendable`]
//!    在出站口拦掉，有单测盯着。
//!
//! # 四、方向不对称：手机 → PC 不需要白名单
//!
//! 那个方向的内容是用户自己在手机上敲的、目的地是他自己的 PC，不存在「把 A 的隐私发给 B」。
//! 那个方向真正的风险是**注入**，由服务端的会话成员校验挡（设计蓝图 §8.5-6），不在本模块。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lumen_protocol::llm::{
    BlockId, ConvGeneration, ConvId, LlmAgentInfo, LlmAgentKind, LlmBlock, LlmBlockEntry,
    LlmConvMeta, LlmConvState, LlmDeltaItem, LlmErrorCode, LlmExitReason, LlmFrame, LlmPath,
    LlmPermissionMode, LlmRateLimit, LlmText, LlmTurnOutcome, LlmTurnRecord, LlmUsage, TurnNo,
    LLM_BACKLOG_MAX_BYTES, LLM_DELTA_FLUSH_MS, LLM_DELTA_MAX_BYTES, LLM_DELTA_WINDOW,
    LLM_HELLO_TIMEOUT_SECS, LLM_HISTORY_MAX_TURNS_PER_PAGE, LLM_HISTORY_PAGE_MAX_BYTES,
    LLM_MAX_CONVS, LLM_PROTO_VERSION, LLM_TURN_SNAPSHOT_MAX_BYTES,
};
use lumen_protocol::remote::Role;

use crate::llm_runner::adapter::{ControlCmd, LaunchSpec, LlmAgentAdapter};
use crate::llm_runner::claude::ClaudeAdapter;
use crate::llm_runner::event::{ProtocolErrorKind, RunnerEvent, StartedInfo, TurnEndedInfo};
use crate::llm_runner::{LlmRunnerManager, RunnerError, RunnerId};

// ── 常量 ──────────────────────────────────────────────────────────────────────

/// 每个对话保留多少**已定稿**的轮记录，供 `TurnFetch` / `HistoryReq` 回放。
///
/// 这是自愈通道的弹药量：控制端发现 `Delta.seq` 断裂后靠 `TurnFetch` 拉整轮快照覆盖重建，
/// 记录被淘汰掉那一轮就永远补不齐了。定 64 是按「一次远程 agent 会话的可回看深度」取的，
/// 与 `LLM_HISTORY_MAX_TURNS_PER_PAGE = 20` 同量级（三页多一点）。
///
/// **有上限本身是刚需**：轮记录里含整轮的块（工具结果已按 `LLM_TOOL_OUTPUT_MAX_BYTES` 夹过，
/// 但 64 KiB × N 块仍可观），不封顶就是又一条随会话时长无界增长的内存路径。
const CONV_TURN_HISTORY_MAX: usize = 64;

/// 单个对话在**代际不匹配**时回给控制端的错误上限（节流）。
///
/// 陈旧代的帧在 QUIC↔中继翻转瞬间会成批到达，每条都回一个 `Error` 帧等于把翻转抖动
/// 放大成一次刷屏。超过这个数就只计数不回帧（本地仍留 `log::warn`）。
const STALE_REPLY_MAX: u32 = 4;

/// 「收到了本端处理不了的帧」这类日志的节流间隔。
///
/// 直接每帧 `log::warn!` 会在对端版本更高时把日志刷爆（对端每 33 ms 一帧增量）；
/// 完全不打则回到本次要修的那个老问题——默认日志级别下**零证据**。
const UNSUPPORTED_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// 对话标题取用户首条消息的前多少个**字符**（不是字节）。
const TITLE_MAX_CHARS: usize = 40;

/// 附件暂存根目录名（在系统临时目录下）。每个对话一个 `conv-<id>` 子目录。
const ATTACH_DIR_ROOT: &str = "lumen_llm_attach";

// ── 出站信封 ──────────────────────────────────────────────────────────────────

/// 本模块的**唯一副作用出口**：帧与通知都先落进 [`LlmPlane::out`] / [`LlmPlane::peer_too_old`]，
/// 由 `remote_ws.rs` 统一投递。
///
/// 这样做的收益是本模块可以在**没有 socket、没有会话、没有子进程**的前提下被单测完整驱动
/// ——仓库里 `RemoteWs` 那批测试全靠 `recv_relay` 从 mpsc 里捞帧，能测的东西受限于必须先
/// 起一条真通道；本模块的状态机测试连通道都不用起。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum LlmOut {
    /// 一条待发的 LLM 帧（`remote_ws.rs` 包成 `RemoteFrame::Llm` 走 `send_frame`）。
    Frame(Box<LlmFrame>),
}

// ── 控制端握手态（§5.4：唯一可执行的版本门）─────────────────────────────────

/// 控制端一侧的能力握手状态机。
///
/// # 为什么超时判据只能是「没收到 `HelloAck`」
/// 老被控端（v3 及更早的 `lumen-protocol`）的 `RemoteFrame` 没有 `Llm` 变体，
/// `RemoteFrame::from_value` 会**整帧解析失败**，然后 `apply_relay` 丢弃、**不回任何东西**。
/// 所以「等一个错误回执」这条路根本不存在——只能等超时。
#[derive(Debug, Clone, PartialEq)]
enum HelloState {
    /// 还没有会话，或本端不是控制端。
    Idle,
    /// 已发 `Hello`，在等 `HelloAck`。
    Waiting {
        /// 判定「对端不支持」的时刻（发出时刻 + [`LLM_HELLO_TIMEOUT_SECS`]）。
        deadline: Instant,
    },
    /// 对端支持，能力已协商。
    Ready {
        /// 对端的 LLM 子协议版本。**不作阻断依据**（`LlmFrame` 自带 `other` 兜底），
        /// 只用于排障与灰度。
        llm_proto: u32,
        /// 对端声明的子能力标签。
        caps: Vec<String>,
    },
    /// 超时判定：对端太老 / 没有 LLM 面。已推过一次 `Notice::LlmPeerTooOld`。
    Unsupported,
}

/// 控制端对**一个对话**的接收态——只装保序与自愈需要的东西。
///
/// # 为什么这么薄
/// P0 桌面端**不做 LLM 会话 UI**（设计蓝图 §12-⑫ 拍板：只留一个图标标识），所以桌面端
/// 作为控制端时没有任何东西要渲染。但保序与背压**不能因此省掉**：
///
/// - 不发 `DeltaAck`，被控端的 [`LLM_DELTA_WINDOW`] 窗口 16 帧就填满、此后全部转本地积压，
///   积压满了开始丢增量——一个「只是不显示」的端会把对端拖进降级路径。
/// - 不查 `seq` 连续性，就没人触发 `TurnFetch`，自愈通道形同虚设。
///
/// 换句话说：**这半边不是为本端 UI 服务的，是为对端的流控正确性服务的。**
#[derive(Debug, Clone)]
struct CtlConv {
    /// 基线帧带来的代号。代不匹配的帧一律丢弃（`ConvGeneration` 的不变量）。
    generation: ConvGeneration,
    /// 已消费到的 `Delta.seq` 水位（基线帧会把它直接顶到基线值）。
    seq: u64,
    /// 已知的最新轮号，`TurnFetch` 要用它指定拉哪一轮。
    turn: TurnNo,
    /// 已经为「本轮缺口」发过 `TurnFetch`，等应答期间不重复发。
    fetch_inflight: bool,
}

// ── 被控端对话态 ──────────────────────────────────────────────────────────────

/// 正在累积的一轮（尚未定稿）。定稿后转成 [`LlmTurnRecord`] 进 [`Conv::turns`]。
#[derive(Debug, Clone)]
struct TurnBuild {
    turn: TurnNo,
    user: Vec<LlmBlockEntry>,
    assistant: Vec<LlmBlockEntry>,
    started_ms: i64,
    /// 本轮曾因积压降级丢过增量（→ `TurnEnded.truncated` / `LlmTurnRecord.complete = false`）。
    truncated: bool,
}

/// 合并窗口里的一项：增量本体 + **它来自 transcript 的哪一条**。
///
/// # 为什么 tseq 必须逐项记，不能一批共用一个
/// `DeltaAck` 回来时要反向 ack runner 的 transcript（`LlmRunner::ack`），而
/// [`LlmPlane::flush_conv`] 现在会把一池 `pending` **拆成多帧**发（见那里的长注释）。
/// 一池共用一个 tseq 的老写法在拆帧后会失真：第 2 帧起拿不到真实 tseq，控制端把这些帧
/// 全 ack 完之后 runner 那边的 transcript 反而一条都没被销掉——环形缓冲白白涨到上限
/// 再从最老一端淘汰，而那正是「补不齐的缺口」的来源。
#[derive(Debug, Clone)]
struct PendingItem {
    item: LlmDeltaItem,
    /// 产生它的 transcript 记录序号（`0` = 合成项，如 [`LlmDeltaItem::Dropped`]）。
    tseq: u64,
}

/// 被控端持有的一个对话。
#[derive(Debug)]
struct Conv {
    runner: RunnerId,
    generation: ConvGeneration,
    meta: LlmConvMeta,
    attach_dir: LlmPath,
    /// 已分配到的最大 `Delta.seq`。**只有 `Delta` 递增它**（见模块文档二）。
    seq: u64,
    /// 控制端 `DeltaAck` 确认到的 `seq`。
    acked_seq: u64,
    /// 在途未确认的 `Delta` **帧数**（不是字节数）——[`LLM_DELTA_WINDOW`] 背压的计数器。
    inflight: u32,
    /// 控制端是否订阅着本对话（`Attach` / `Detach` 维护）。
    ///
    /// **它同时是实时流的出站闸门**：未订阅时 [`push_delta`] 不入 `pending`、不领 seq，
    /// [`LlmPlane::emit_stream`] 不发轮头/轮尾/退出帧。理由见 [`LlmPlane::emit_stream`]。
    subscribed: bool,
    /// 「下次 `Attach` 必须让控制端整体重建」。
    ///
    /// # 为什么必须有这么一个显式标志
    /// 水位 `seq` 只在 `Delta` **成功发出**时递增。而有两条路径会让内容进了本端的轮记录、
    /// 却从来没有变成过 `Delta`：
    ///
    /// 1. [`LlmPlane::reset_protocol_state`]（断线 / 会话结束）把 `pending` 整个丢掉；
    /// 2. 未订阅期间 [`push_delta`] 根本不入 `pending`。
    ///
    /// 两条路径都**不动 `seq`**，于是控制端回来 `Attach{known_seq}` 时 `known_seq == conv.seq`
    /// 恰好成立——它会认为自己是同步的，而那段内容它永远看不到，两端都没有任何迹象。
    /// 这正是本轮修掉的那个静默丢失。置上这个标志，[`LlmPlane::attach`] 就会回
    /// `resync_required: true`，控制端清空本地状态走 `HistoryReq` / `TurnFetch` 重建。
    resync_due: bool,
    /// 33 ms 合并窗口里攒着的增量项。窗口满时它同时兼任**本地积压**。
    pending: Vec<PendingItem>,
    /// `pending` 的近似字节数（用于 [`LLM_DELTA_MAX_BYTES`] 与 [`LLM_BACKLOG_MAX_BYTES`]）。
    pending_bytes: usize,
    /// 合并窗口到期时刻；`None` = 当前没有待发增量。
    flush_at: Option<Instant>,
    /// `pending` 里那批增量属于**哪一轮**。
    ///
    /// # 为什么不能在 flush 时读 `meta.cur_turn`
    /// 轮号是**帧级**字段（`LlmDeltaItem` 上没有 turn），而换轮与 flush 是两件独立的事：
    /// 上一轮的最后几个块还躺在 33 ms 合并窗口里时新一轮就可能开始（工具调用密集的会话里
    /// 常态）。那时读 `meta.cur_turn` 会给这批块打上**新轮号**，手机端按 `(turn, block_id)`
    /// 归约时会把它们挂到错误的一轮上——一个不报错、只会让气泡串位置的 bug。
    pending_turn: TurnNo,
    /// 每条**已发出**的 `Delta` 对应的 transcript seq，供 `DeltaAck` 到达时反向 ack runner
    /// （两个 seq 是**不同空间**：协议 seq 只数 Delta 帧，transcript seq 数每一条事件）。
    /// 长度不超过 [`LLM_DELTA_WINDOW`]。
    ///
    /// 条目**只在 [`LlmPlane::flush_conv`] 真发出一帧时**才产生，`.0` 恒非 0。
    /// 老写法在 `push_delta` 里先塞一个 `(0, tseq)` 占位、flush 时补号，拆帧之后那个
    /// 「每池至多一个占位」的前提不再成立（一池会发出 N 帧），故占位机制整个删掉。
    ack_map: VecDeque<(u64, u64)>,
    /// 正在累积的一轮。
    cur: Option<TurnBuild>,
    /// 已定稿的轮（升序，上限 [`CONV_TURN_HISTORY_MAX`]）。
    turns: VecDeque<LlmTurnRecord>,
    /// 已回过多少条代际不匹配错误（[`STALE_REPLY_MAX`] 节流）。
    stale_replies: u32,
    /// 被白名单挡掉的上游行数（**只计数、不记内容**，进本地日志，不上行）。
    dropped_lines: u64,
}

impl Conv {
    /// 当前水位（给基线帧盖戳用）。
    const fn watermark(&self) -> u64 {
        self.seq
    }

    /// 窗口是否已满（满则新增量只进本地积压，**不停止读子进程**）。
    const fn window_full(&self) -> bool {
        self.inflight >= LLM_DELTA_WINDOW
    }
}

// ── 泵的上下文 ────────────────────────────────────────────────────────────────

/// [`LlmPlane::pump`] 的入参打包。
///
/// 打成结构体而不是 5 个参数，是为了守住《代码大全》那条「参数 ≤ 7」的同时，让新增上下文
/// （比如日后要传审计日志句柄）不必改所有调用点的签名。
pub(super) struct PumpCtx<'a> {
    /// 本帧时刻。**由调用方传入而不是内部取 `Instant::now()`**：合并窗口、握手超时全靠它，
    /// 传进来才能在单测里把时间往前拨。
    pub now: Instant,
    /// 本端在当前会话里的角色；`None` = 无会话（此时只做清理，不产出任何帧）。
    pub role: Option<Role>,
    /// headless runner 容器。**本模块是唯一持它可变引用的地方**。
    pub runners: &'a mut LlmRunnerManager,
    /// 本帧 `LlmRunnerManager::pump()` 产出的事件。
    pub events: &'a [(RunnerId, u64, RunnerEvent)],
    /// `RemoteWs::req_seq` 本体。**绝不另起计数器**——见 [`next_req_id`]。
    pub req_seq: &'a mut u64,
}

// ── 平面本体 ──────────────────────────────────────────────────────────────────

/// LLM 数据面状态机。挂在 `RemoteWs.llm` 上，两个角色的状态**刻意放在同一个结构里**：
/// 一台 PC 在不同会话里可能既当控制端又当被控端，分成两个结构会让「切换角色时该清哪个」
/// 变成又一处要靠自觉维护的清单。
#[derive(Debug, Default)]
pub(super) struct LlmPlane {
    // ── 控制端半边 ──
    hello: HelloStateCell,
    ctl_convs: HashMap<ConvId, CtlConv>,
    /// 待推给 UI 的「对端太老」通知（`RemoteWs` 每帧取走转 `Notice::LlmPeerTooOld`）。
    peer_too_old: bool,

    // ── 被控端半边 ──
    convs: HashMap<ConvId, Conv>,
    by_runner: HashMap<RunnerId, ConvId>,
    next_conv_id: ConvId,
    /// 会话结束 / 断线时要还给 runner 的订阅计数。
    ///
    /// **为什么要缓一帧**：五个既有清理点（`stop` / `end_session` / `begin_reattach` /
    /// `teardown_session_state` / `SessionStarted`）**没有一个**持有
    /// `&mut LlmRunnerManager`（管理器在 `AppState` 上，与 `RemoteWs` 平级）。硬要在那里拿到
    /// 引用就得改 `main.rs` 的调用点。缓一帧到 [`LlmPlane::pump`] 落地是本轮改动面最小的做法，
    /// 代价是「订阅计数归零」比「协议态清空」晚一个事件循环——而订阅计数**只影响空闲回收与
    /// 保留期**（`LlmRunner::attach` 文档），晚一帧无任何可观察后果。
    pending_detach: Vec<RunnerId>,

    // ── 共用 ──
    /// 入站帧队列（见模块文档一）。
    inbox: VecDeque<LlmFrame>,
    /// 出站帧队列。
    out: Vec<LlmOut>,
    /// 上次打「本端处理不了这帧」日志的时刻（[`UNSUPPORTED_LOG_INTERVAL`] 节流）。
    last_unsupported_log: Option<Instant>,
    /// 自上次打日志以来被压掉的条数（补在下一条日志里，避免「只看见 1 条」的错觉）。
    suppressed_unsupported: u64,
}

/// [`HelloState`] 的 `Default` 载体。
///
/// `HelloState` 自身不 derive `Default`（枚举的默认值该是哪个变体不是自明的，写成
/// `#[derive(Default)] #[default] Idle` 会让人以为「默认 = 没在等」是个约定俗成的语义）。
/// 包一层 newtype 把默认值的选择写在这里，连同理由。
#[derive(Debug)]
struct HelloStateCell(HelloState);

impl Default for HelloStateCell {
    /// 默认 [`HelloState::Idle`]：`RemoteWs::default()` 时既没有会话也没有角色，
    /// 任何「等待中」的默认值都会让第一次 `tick` 误判超时。
    fn default() -> Self {
        Self(HelloState::Idle)
    }
}

impl LlmPlane {
    // ── 生命周期与清理 ──────────────────────────────────────────────────────

    /// 会话建立。控制端立即发 `Hello` 并起 [`LLM_HELLO_TIMEOUT_SECS`] 秒计时。
    ///
    /// # 被控端为什么不主动发任何东西
    /// 握手是**单向**的：能力协商的诉求方是想用 LLM 面的那一端（控制端）。被控端主动发
    /// `HelloAck` 没有对应的 `req_id`，老控制端收到也只会整帧丢弃，纯属浪费一帧。
    pub(super) fn on_session_started(&mut self, role: Role, now: Instant) {
        self.reset_protocol_state();
        if role != Role::Controller {
            self.hello = HelloStateCell(HelloState::Idle);
            return;
        }
        self.send_hello(now);
    }

    /// 链路翻转（QUIC↔中继）或断线重挂成功后的复位。
    ///
    /// 设计蓝图 §5.4-5 把这条列为握手清单的第 5 项，理由是：`resubscribe_after_switch()`
    /// 只重发终端订阅、**对 LLM 流毫无作用**；不在这里重发 `Hello` 并把已订阅对话标记为
    /// 待重同步，一次偶发超时会让 LLM 面在整个会话生命周期内假死。
    /// # 已判定「对端太老」之后为什么不重置超时计时
    /// 老写法无条件把握手态打回 `Waiting{deadline}`，于是 QUIC↔中继**每翻转一次**就会在
    /// 5 秒后再判一次超时、再推一条 `Notice::LlmPeerTooOld`——链路抖动的现场会变成周期性
    /// 弹窗（清 `peer_too_old` 的那句在 `reset_protocol_state` 里，这条路径根本走不到）。
    /// 对端支不支持 LLM 面是**对端的属性**，不随本端换了条链路而改变；真升级了会话也会重建
    /// （`on_session_started` → `reset_protocol_state`），那才是该重新判定的时机。
    /// 故 `Unsupported` 之后只补发一条 `Hello`（万一对端真换了），**不重开计时、不再弹**。
    pub(super) fn on_link_switched(&mut self, role: Role, now: Instant) {
        if role != Role::Controller {
            return;
        }
        // 已订阅的对话全部标记「水位不可信」：翻转瞬间两条通路无全局序，下一条基线帧
        // （`Attached`）会把水位顶到正确值，在那之前来的 `Delta` 一律当缺口处理。
        for conv in self.ctl_convs.values_mut() {
            conv.fetch_inflight = false;
        }
        if matches!(self.hello.0, HelloState::Unsupported) {
            self.out.push(LlmOut::Frame(Box::new(LlmFrame::Hello {
                llm_proto: LLM_PROTO_VERSION,
                caps: Vec::new(),
            })));
            return;
        }
        self.send_hello(now);
    }

    /// **第 6 个清理点**：会话结束 / 断线 / 切换。
    ///
    /// 与既有五个清理点（`stop` / `end_session` / `begin_reattach` /
    /// `teardown_session_state` / `apply_server` 的 `SessionStarted` 臂）挂在**同样的位置**。
    ///
    /// # 这里**不杀** headless 子进程，是硬契约不是疏漏
    /// 设计蓝图 §6.7：手机进后台 / 隧道断 → 服务端 `teardown_session` → PC 收 `SessionEnded`，
    /// 此时 runner **必须继续跑**，事件继续写进带 seq 的 transcript 环形缓冲，等手机回来
    /// `Attach{known_seq}` 补发。一个 turn 可以跑几分钟，杀掉就是把用户的活儿丢了。
    ///
    /// 所以本方法清的只有**协议侧**的东西：订阅态、水位、合并窗口、握手态。
    /// 对话本身（`convs` / `by_runner`）**保留**——`ConvId` 是被控端分配的、跨会话稳定，
    /// 手机重连后按同一个 id `Attach` 回来。
    pub(super) fn on_session_ended(&mut self) {
        self.reset_protocol_state();
    }

    /// Lumen 登出 / `RemoteWs::stop()`。
    ///
    /// 与 [`Self::on_session_ended`] 的区别：登出是「不打算再有会话了」，故连对话注册表
    /// 一起清。**子进程仍不在这里杀**——`LlmRunnerManager` 的 `Drop`（进程退出）与空闲回收
    /// 负责，而登出并不意味着用户想终止正在跑的 agent（他可能只是切了账号）。
    pub(super) fn on_stop(&mut self) {
        self.reset_protocol_state();
        // 对话记录清掉，但先把订阅计数还给 runner（否则那些 runner 会因 `subscribers > 0`
        // 永远不进空闲回收，直到 Lumen 退出）。
        for conv in self.convs.values() {
            if conv.subscribed {
                self.pending_detach.push(conv.runner);
            }
        }
        self.convs.clear();
        self.by_runner.clear();
    }

    /// 清空**协议侧**状态（订阅、水位、合并窗口、握手、收发队列）。
    fn reset_protocol_state(&mut self) {
        self.hello = HelloStateCell(HelloState::Idle);
        self.ctl_convs.clear();
        self.peer_too_old = false;
        self.inbox.clear();
        self.out.clear();
        for conv in self.convs.values_mut() {
            if conv.subscribed {
                conv.subscribed = false;
                self.pending_detach.push(conv.runner);
            }
            // 攒在合并窗口里、还没领到 seq 的那批增量在这里被丢掉。它们**已经进了轮记录**
            // （`on_block` 先写 `build.assistant` 再 `push_delta`），所以内容没丢；丢的是
            // 「控制端能靠 seq 发现它们缺了」这件事——`seq` 没动过。故必须记账，
            // 否则控制端回来时 `known_seq == conv.seq` 成立，它会以为自己是同步的。
            if !conv.pending.is_empty() {
                conv.resync_due = true;
            }
            conv.pending.clear();
            conv.pending_bytes = 0;
            conv.flush_at = None;
            conv.ack_map.clear();
            conv.inflight = 0;
            conv.stale_replies = 0;
        }
    }

    // ── 控制端握手 ──────────────────────────────────────────────────────────

    /// 发一条 `Hello` 并进入等待。
    fn send_hello(&mut self, now: Instant) {
        self.hello = HelloStateCell(HelloState::Waiting {
            deadline: now + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS),
        });
        self.out.push(LlmOut::Frame(Box::new(LlmFrame::Hello {
            llm_proto: LLM_PROTO_VERSION,
            // P0 不做子能力灰度：本端支持的就是 `LLM_PROTO_VERSION` 声明的全集。
            // 空数组比编几个没人消费的标签诚实。
            caps: Vec::new(),
        })));
    }

    /// 握手超时判定。每帧调用（挂在 `RemoteWs::poll` 上，与 `tick_reattach` 同一处）。
    ///
    /// 返回是否发生了状态变化（调用方据此请求重绘）。
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let HelloState::Waiting { deadline } = self.hello.0 else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.hello = HelloStateCell(HelloState::Unsupported);
        self.peer_too_old = true;
        log::warn!(
            "LLM 能力握手超时（{LLM_HELLO_TIMEOUT_SECS} 秒未收到 HelloAck）：\
             对端 Lumen 不支持 LLM 远程控制面，终端镜像与文件传输不受影响"
        );
        true
    }

    /// 握手还剩多久到期。
    pub(super) fn hello_deadline(&self) -> Option<Instant> {
        match self.hello.0 {
            HelloState::Waiting { deadline } => Some(deadline),
            _ => None,
        }
    }

    /// 本平面**下一个需要被唤醒的时刻**（握手超时 / 合并窗口到期，取较早者）。
    ///
    /// # 为什么必须有这个东西
    /// `main.rs` 设的是 `ControlFlow::Wait`：**没有外部事件就不跑循环**。而本模块有两件事
    /// 是纯计时驱动的：
    ///
    /// 1. 握手的 5 秒判定——不排唤醒最坏要等 25 秒一次的 WS 保活才轮得到；
    /// 2. **33 ms 合并窗口的到期**——一轮的最后几个块推进窗口后，如果 CLI 正好去跑一个几分钟
    ///    的工具（不产生任何事件、也就没有任何唤醒），那批增量会在本地压几分钟。
    ///    手机上的表现是「说到一半停住了」，而 PC 侧一切正常——这类只在特定节奏下偶现的
    ///    问题几乎不可能靠复现定位。
    ///
    /// # 这个时刻交给谁去实现（**曾经写错过，别再照抄**）
    /// 本注释一度写着「交给 `egui::Context::request_repaint_after` 是不改 `main.rs` 就能拿到
    /// 定时唤醒的唯一现成通路」。**那是错的。** 那条链的终点是
    /// `egui_repaint_at` → `ControlFlow::WaitUntil` → `about_to_wait` 里 `request_redraw`
    /// → `WindowEvent::RedrawRequested`，而 `RedrawRequested` **只渲染、不跑泵**：
    /// `pump_remote()` 与 `pump_llm_runners()` 在全仓只有 `main.rs` 的 `user_event(PtyWake)`
    /// 两个调用点。于是这两件纯计时的事实际靠 WS 后台线程 25 秒一次 Ping 的 Pong 回包
    /// 顺带推进——33 ms 的合并窗口最坏压 25 秒才发出，正是上面第 2 条描述的那个症状。
    ///
    /// 仓库自己早记过这个坑（`llm_runner` 模块文档：`ControlFlow::Wait` 下
    /// `ctx.request_repaint()` **叫不醒空闲事件循环**），其余所有定时/后台路径一律走
    /// `proxy.send_event(PtyWake)`。故现在的实现是 `RemoteWs::tick_llm` 把这个时刻交给
    /// [`super::LlmWaker`]（一条常驻定时线程，到点 `nudge` = 重绘标记 + `PtyWake`），
    /// `request_repaint_after` 仍然照发，但只是让 UI 侧的重绘一起跟上，不再是唯一依靠。
    pub(super) fn next_wake(&self) -> Option<Instant> {
        self.convs
            .values()
            .filter_map(|c| c.flush_at)
            .chain(self.hello_deadline())
            .min()
    }

    /// 取走「对端太老」通知（一次性）。
    pub(super) fn take_peer_too_old(&mut self) -> bool {
        std::mem::take(&mut self.peer_too_old)
    }

    /// 对端是否已确认支持 LLM 面——**UI 置灰 LLM 入口的判据，消费者在后续的 UI 片**。
    ///
    /// 老写法标的是 `#[cfg(test)]` 而注释写「供 UI 置灰入口用」：那样它在**生产构建里根本
    /// 不存在**，注释描述的能力也就不存在。改成 `allow(dead_code)` + 如实说明消费者在哪一片，
    /// 与本文件里 `hidden_session_count` / `hidden_peers` 三处的写法对齐。
    #[allow(dead_code)]
    pub(super) fn peer_ready(&self) -> bool {
        matches!(self.hello.0, HelloState::Ready { .. })
    }

    // ── 收发队列 ────────────────────────────────────────────────────────────

    /// 入站一帧（`apply_relay` 调）。只入队，执行在 [`Self::pump`]（见模块文档一）。
    pub(super) fn enqueue(&mut self, frame: LlmFrame, now: Instant) {
        if matches!(frame, LlmFrame::Unknown) {
            // 对端 `op` 比本端新。这是**前向兼容生效**的正常路径（协议自带 `other` 兜底），
            // 但必须留证据：默认日志级别下零证据正是本轮要修的那个老问题。
            self.log_unsupported(now, "对端发来本端不认识的 LLM op（对端版本更新）");
            return;
        }
        self.inbox.push_back(frame);
    }

    /// 取走待发帧。
    pub(super) fn take_out(&mut self) -> Vec<LlmOut> {
        std::mem::take(&mut self.out)
    }

    /// 节流日志：同一类问题 [`UNSUPPORTED_LOG_INTERVAL`] 内只打一条，并把压掉的条数补上。
    fn log_unsupported(&mut self, now: Instant, what: &str) {
        let due = self
            .last_unsupported_log
            .is_none_or(|t| now.duration_since(t) >= UNSUPPORTED_LOG_INTERVAL);
        if !due {
            self.suppressed_unsupported = self.suppressed_unsupported.saturating_add(1);
            return;
        }
        let suppressed = std::mem::take(&mut self.suppressed_unsupported);
        self.last_unsupported_log = Some(now);
        if suppressed == 0 {
            log::warn!("{what}");
        } else {
            log::warn!("{what}（同类已压制 {suppressed} 条）");
        }
    }

    // ── 泵 ──────────────────────────────────────────────────────────────────

    /// 每帧一次的推进：入站帧 → runner 操作；runner 事件 → 帧；合并窗口 flush。
    ///
    /// 返回**出站队列是否有东西要发**（调用方据此请求重绘）。
    ///
    /// 刻意不是「本次 pump 新产出了几帧」：`Hello` 是在
    /// [`Self::on_session_started`] / [`Self::on_link_switched`] 里入队的，跟本次 pump 无关，
    /// 但它同样需要被投出去、也同样需要一次重绘。按「增量」算会让会话建立后的第一帧
    /// `Hello` 被判成「无事发生」——那正是 `ControlFlow::Wait` 下最不该丢的一次唤醒。
    pub(super) fn pump(&mut self, ctx: &mut PumpCtx<'_>) -> bool {
        self.apply_pending_detach(ctx.runners);
        while let Some(frame) = self.inbox.pop_front() {
            self.dispatch(frame, ctx);
        }
        // **无论有没有会话都要吃事件**：§6.7 的硬契约是「手机断线 runner 照跑、内容继续累积」，
        // 不吃就等于把断线期间的产出真丢了（`RunnerEvent` 是按引用传进来的一次性批次）。
        // 真正该被会话状态挡住的是**出站**，那道闸落在 `push_delta` / `emit_stream` 的
        // `Conv::subscribed` 判据上——而 `reset_protocol_state` 在会话结束时已把所有
        // `subscribed` 清成 false，故 `role == None` 时本方法结构上产不出任何流式帧。
        self.ingest_events(ctx);
        if ctx.role.is_some() {
            self.flush_all(ctx.now);
        }
        self.reap_convs(ctx.runners);
        !self.out.is_empty()
    }

    /// 回收已被 [`LlmRunnerManager`] 收走的对话条目。
    ///
    /// # 不做这一步会复现片 2 的 §6.9 缺陷 ⑤
    /// 那条缺陷是「`runners` 只增不减 ⇒ 4 次崩溃之后永久无法再起新会话」。本模块的
    /// `convs` 有完全一样的形状：[`Self::start_conv`] 拿 `convs` 的规模去比
    /// [`LLM_MAX_CONVS`]，只进不出的话，8 个对话退出之后手机端再点「新建」只会拿到
    /// `LimitReached`，而且**重启 Lumen 才能恢复**。
    ///
    /// 回收判据借用管理器自己的保留期（`RUNNER_EXITED_RETENTION` 5 分钟 + 「transcript 已被
    /// 读完」）：runner 还在管理器里就说明还可能有人来补看最后的错误，这时不能扔。
    /// 借它的判据而不是另立一个计时器，是为了**只有一份保留期语义**。
    fn reap_convs(&mut self, runners: &LlmRunnerManager) {
        let gone: Vec<ConvId> = self
            .convs
            .iter()
            .filter(|(_, c)| {
                c.meta.state == LlmConvState::Exited
                    && !c.subscribed
                    && runners.get(c.runner).is_none()
            })
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            if let Some(conv) = self.convs.remove(&id) {
                self.by_runner.remove(&conv.runner);
                log::debug!("LLM 对话 {id} 已回收（{} 也已被管理器收走）", conv.runner);
            }
        }
    }

    /// 还没退出的对话数——[`LLM_MAX_CONVS`] 的判据。
    ///
    /// **不能直接用 `convs.len()`**：已退出但还在保留期里的条目（等手机回来补看错误）
    /// 不占并发名额，否则一次连续崩溃会把用户挡在 8 个名额之外好几分钟。
    fn live_convs(&self) -> usize {
        self.convs
            .values()
            .filter(|c| c.meta.state != LlmConvState::Exited)
            .count()
    }

    /// 把缓了一帧的订阅计数还给 runner（见 [`Self::pending_detach`] 的长注释）。
    fn apply_pending_detach(&mut self, runners: &mut LlmRunnerManager) {
        for id in self.pending_detach.drain(..) {
            if let Some(runner) = runners.get_mut(id) {
                runner.detach();
            }
        }
    }

    /// 按角色分发一帧。角色不对的帧一律丢弃 + 节流日志。
    ///
    /// **不做「宽松接受」**：`Delta` 只可能由被控端发出，本端若是被控端却收到 `Delta`，
    /// 要么是对端实现有 bug，要么是有人在中继上做手脚——两种都该丢，不该猜。
    fn dispatch(&mut self, frame: LlmFrame, ctx: &mut PumpCtx<'_>) {
        match ctx.role {
            Some(Role::Controlled) => self.on_controller_request(frame, ctx),
            Some(Role::Controller) => self.on_host_event(frame, ctx),
            None => self.log_unsupported(ctx.now, "无会话状态下收到 LLM 帧，丢弃"),
        }
    }

    // ── 被控端：处理控制端指令 ──────────────────────────────────────────────

    /// 被控端收到控制端的一条指令。
    ///
    /// **此处刻意违反《代码大全》「单函数 ≤ 60 行」的目标值（本体 82 行，仍远低于 200 行硬上限），
    /// 理由：这是一个对 `LlmFrame` 的穷尽 `match`，穷尽性本身是它最重要的性质。**
    /// 拆成两半就必须在其中一半加通配臂，而通配臂会让今后新增的 `LlmFrame` 变体不再触发
    /// E0004 —— 那正是 `remote_ws.rs` 的 `apply_relay` 用一整段注释守着的东西
    /// （「新增数据面变体必须显式决定怎么处理的唯一强制点」）。函数长度可以补注释，
    /// 静默漏处理一个新变体补不回来。每条臂的真实逻辑都已下沉到独立方法，本体只做分发。
    fn on_controller_request(&mut self, frame: LlmFrame, ctx: &mut PumpCtx<'_>) {
        match frame {
            LlmFrame::Hello { llm_proto, .. } => self.reply_hello_ack(llm_proto, ctx),
            LlmFrame::ListAgents { req_id } => {
                let agents = available_agents();
                self.emit(LlmFrame::AgentList { req_id, agents });
            }
            LlmFrame::ListConvs { req_id } => {
                let convs = self.conv_metas();
                self.emit(LlmFrame::ConvList { req_id, convs });
            }
            LlmFrame::Start { .. } => self.start_conv(frame, ctx),
            LlmFrame::Send {
                conv_id,
                conv_generation,
                req_id,
                text,
                attachments,
            } => self.on_send(conv_id, conv_generation, req_id, &text, &attachments, ctx),
            LlmFrame::Interrupt {
                conv_id,
                conv_generation,
                req_id,
            } => self.control(
                conv_id,
                conv_generation,
                req_id,
                &ControlCmd::Interrupt,
                ctx,
            ),
            LlmFrame::Close {
                conv_id,
                conv_generation,
                req_id,
            } => self.close_conv(conv_id, conv_generation, req_id, ctx),
            LlmFrame::Attach {
                conv_id,
                known_generation,
                known_seq,
                ..
            } => self.attach(conv_id, known_generation, known_seq, ctx),
            LlmFrame::Detach {
                conv_id,
                conv_generation,
            } => self.detach(conv_id, conv_generation, ctx),
            LlmFrame::DeltaAck {
                conv_id,
                conv_generation,
                seq,
            } => self.on_delta_ack(conv_id, conv_generation, seq, ctx),
            LlmFrame::TurnFetch {
                conv_id,
                conv_generation,
                req_id,
                turn,
            } => self.on_turn_fetch(conv_id, conv_generation, req_id, turn),
            LlmFrame::HistoryReq {
                conv_id,
                conv_generation,
                req_id,
                before_turn,
                max_turns,
            } => self.on_history_req(conv_id, conv_generation, req_id, before_turn, max_turns),
            // 方向是对的，但**这条通路在 P0 不存在**：`can_use_tool` 的 payload 完全未实测，
            // 片 3 刻意没建模它，故本端从不发 `PermissionRequest`，也就不该收到应答。
            // 单独一臂而不是并进「方向不符」：把「对端搞错了方向」与「本端还没做这条通路」
            // 混成同一条日志，现场排障时是两次误判。
            LlmFrame::PermissionReply {
                conv_id,
                request_id,
                ..
            } => log::warn!(
                "LLM 对话 {conv_id} 收到权限应答 #{request_id}，但本端未实现交互式权限审批\
                 （`LlmAgentFeatures::interactive_permission` 恒为 false），丢弃"
            ),
            other => self.reject_wrong_direction(&other, ctx.now),
        }
    }

    /// 回一条 `HelloAck`，顺带把 agent 清单与现存对话清单捎上（省两个往返）。
    fn reply_hello_ack(&mut self, peer_proto: u32, ctx: &PumpCtx<'_>) {
        if peer_proto != LLM_PROTO_VERSION {
            // **不阻断**：`LlmFrame` 自带 `other` 兜底，版本号只用于排障与灰度
            // （`LLM_PROTO_VERSION` 的类型文档写死了这一条）。
            log::info!("LLM 子协议版本 本端={LLM_PROTO_VERSION} 对端={peer_proto}（不阻断）");
        }
        let _ = ctx;
        let convs = self.conv_metas();
        self.emit(LlmFrame::HelloAck {
            llm_proto: LLM_PROTO_VERSION,
            caps: Vec::new(),
            agents: available_agents(),
            convs,
            // P0 尚未把 `rate_limit_event` 的观测值挂到账号级基线上（本模块只在事件到达时
            // 播报 `LlmFrame::RateLimit`）。**宁可回 `None` 也不回一个编出来的值**：
            // 控制端对 `None` 的正确表现是「额度未知」，对一个假值的表现是「显示错的额度」。
            rate_limit: None,
            rate_limit_observed_ms: None,
        });
    }

    /// 现存对话的元信息清单（按 `conv_id` 升序，便于控制端稳定渲染）。
    fn conv_metas(&self) -> Vec<LlmConvMeta> {
        let mut metas: Vec<LlmConvMeta> = self.convs.values().map(|c| c.meta.clone()).collect();
        metas.sort_by_key(|m| m.conv_id);
        metas
    }

    /// 起一个 headless 对话。
    ///
    /// **刻意超出 60 行的目标值（68 行）**，理由：其中 14 行是 `LlmFrame::Start` 的字段解构、
    /// 另有 11 行是两处必须留在原地的注记（cwd 围栏的诚实缺口、`--resume` 与 `--session-id`
    /// 的互斥语义）。把「解构 + 建 `LaunchSpec`」拆出去要传 8 个参数——**超参数上限，且
    /// 那 8 个参数就是 `Start` 变体本身**，等于为了函数行数再造一个同形结构体。
    /// 真正的执行体（`runners.start` / `register_conv` / `fail_start`）都已经在外面。
    fn start_conv(&mut self, frame: LlmFrame, ctx: &mut PumpCtx<'_>) {
        let LlmFrame::Start {
            req_id,
            agent,
            cwd,
            model,
            permission_mode,
            resume,
            fork_session,
            append_system_prompt,
            allowed_tools,
            origin,
        } = frame
        else {
            return;
        };
        if self.live_convs() >= LLM_MAX_CONVS as usize {
            self.fail_start(req_id, LlmErrorCode::LimitReached, "对话数已达上限");
            return;
        }
        if agent != LlmAgentKind::Claude {
            // 片 3 只落了 Claude 适配器。**显式拒绝而不是静默降级到 Claude**——
            // 用户选了 Codex 却跑起 Claude 是最糟的一种「成功」。
            self.fail_start(
                req_id,
                LlmErrorCode::AgentNotFound,
                "本端仅支持 Claude（其余 CLI 的适配器尚未落地）",
            );
            return;
        }
        // ⚠ **越权校验这里只有「存在且是目录」这一层**（由 `LlmRunnerManager::start` 执行，
        // 失败回 `CwdInvalid`）。协议注释写的是「被控端校验存在性**与越权**」，而本仓库
        // 目前没有任何「允许的工作目录根」概念——文件传输侧的 `handle_put_begin` 同样是
        // 按不透明路径直接落盘。凭空定一个根出来会与既有文件面行为不一致，故本片**不做**，
        // 并把这条缺口如实记在这里：控制端能指定任意 PC 上存在的目录起 agent。
        // 真正的边界在配对信任（只有配过对的设备能建会话）与 §8.5-6 的会话成员校验。
        let workspace = PathBuf::from(cwd.as_str());
        // `--resume` 与 `--session-id` 在 `claude.rs::build_args` 里是**互斥**的两条路：
        // 续接时 `--resume <cli_session_id>` 复用对方给的 id，新开时 `--session-id <新 uuid>`
        // 由我们钉一个。所以「要续接哪个会话」这件事的落点就是 `cli_session_id` 本身，
        // 而不是另开一个字段——写成 `spec.resume = resume.is_some()` 却仍生成新 uuid，
        // 会得到一条 `--resume <一个从没存在过的 id>`。
        // ⚠ `--resume` 在 `--input-format stream-json` 下的语义本轮未实测（§12-④）。
        let resuming = resume.is_some();
        let mut spec = LaunchSpec::new(
            agent.clone(),
            workspace,
            resume.unwrap_or_else(new_cli_session_id),
        );
        spec.resume = resuming;
        spec.fork_session = fork_session;
        spec.model.clone_from(&model);
        spec.append_system_prompt = append_system_prompt;
        spec.allowed_tools = allowed_tools;
        if let Some(mode) = permission_mode.clone() {
            if let Err(err) = spec.set_permission_mode(mode) {
                self.fail_start(req_id, runner_error_code(&err), &err.to_string());
                return;
            }
        }
        match ctx.runners.start(&ClaudeAdapter::new(), &spec) {
            Ok(runner) => {
                let conv_id =
                    self.register_conv(req_id, runner, &spec, model, permission_mode, origin);
                // **`ConvStarted` 是基线帧**（协议原文），发起 `Start` 的控制端由此即为订阅方。
                // 不在这里订上，实时流会被 `Conv::subscribed` 闸门整段挡掉，且 runner 的订阅
                // 计数恒为 0 会让它落进空闲回收——「起了个对话然后什么都不发」。
                self.subscribe(conv_id, ctx.runners);
            }
            Err(err) => {
                log::warn!("LLM 对话启动失败：{err}");
                self.fail_start(req_id, runner_error_code(&err), &err.to_string());
            }
        }
    }

    /// 注册一个刚起来的对话并回 `ConvStarted`，返回分配到的 `conv_id`。
    fn register_conv(
        &mut self,
        req_id: u64,
        runner: RunnerId,
        spec: &LaunchSpec,
        model: Option<String>,
        permission_mode: Option<LlmPermissionMode>,
        origin: Option<lumen_protocol::llm::PaneRef>,
    ) -> ConvId {
        self.next_conv_id += 1;
        let conv_id = self.next_conv_id;
        let now_ms = now_ms();
        let meta = LlmConvMeta {
            conv_id,
            // 代号用随机数而不是自增：手机跨 PC 重启后按 `ConvId` 重挂，若代号也从 1 起自增，
            // 「重启前的第 1 代」与「重启后的第 1 代」会撞上，陈旧守卫当场失效。
            conv_generation: random_generation(),
            agent: spec.agent.clone(),
            cwd: LlmPath::new(spec.workspace.display().to_string()),
            title: LlmText::new(dir_label(&spec.workspace)),
            state: LlmConvState::Starting,
            model,
            permission_mode,
            cli_session_id: Some(spec.cli_session_id.clone()),
            origin,
            cur_turn: 0,
            created_ms: now_ms,
            updated_ms: now_ms,
            usage: LlmUsage::default(),
        };
        let attach_dir = ensure_attach_dir(conv_id);
        let conv = Conv {
            runner,
            generation: meta.conv_generation,
            meta: meta.clone(),
            attach_dir: attach_dir.clone(),
            seq: 0,
            acked_seq: 0,
            inflight: 0,
            subscribed: false,
            resync_due: false,
            pending: Vec::new(),
            pending_bytes: 0,
            flush_at: None,
            pending_turn: 0,
            ack_map: VecDeque::new(),
            cur: None,
            turns: VecDeque::new(),
            stale_replies: 0,
            dropped_lines: 0,
        };
        self.convs.insert(conv_id, conv);
        self.by_runner.insert(runner, conv_id);
        log::info!("LLM 对话 {conv_id} 已建立（{runner}）");
        self.emit(LlmFrame::ConvStarted {
            req_id,
            meta,
            attach_dir,
            seq: 0,
        });
        conv_id
    }

    /// 把一个对话标记为「控制端正在收流」并把订阅计数记到 runner 上。
    ///
    /// `Start`（`ConvStarted` 是基线帧）与 `Attach` 两条路共用同一份逻辑——分成两处写过一次
    /// 之后，其中一处漏掉 `runner.attach()` 就是一个「跑着跑着被空闲回收了」的幽灵故障。
    fn subscribe(&mut self, conv_id: ConvId, runners: &mut LlmRunnerManager) {
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        if conv.subscribed {
            return;
        }
        conv.subscribed = true;
        if let Some(runner) = runners.get_mut(conv.runner) {
            runner.attach();
        }
    }

    /// 起会话失败的统一出口。
    fn fail_start(&mut self, req_id: u64, code: LlmErrorCode, message: &str) {
        self.emit(LlmFrame::ConvFailed {
            req_id,
            code,
            message: LlmText::new(message),
        });
    }

    /// 提交一条用户消息（`Send`）：**先过附件围栏**，再交给 runner。
    ///
    /// 围栏不过时回 `Error{CwdInvalid}` 而**不是静默剔除**：静默剔除会让控制端以为图片已经
    /// 发出去了，模型却根本没看见——一个不报错、只会让对话答非所问的 bug。
    fn on_send(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        req_id: u64,
        text: &LlmText,
        attachments: &[lumen_protocol::llm::LlmAttachment],
        ctx: &mut PumpCtx<'_>,
    ) {
        let Some(conv) = self.checked_mut(conv_id, generation, Some(req_id)) else {
            return;
        };
        let rejected = attachments_outside_fence(&conv.attach_dir, attachments);
        if !rejected {
            submit_message(conv, ctx.runners, text, attachments);
            return;
        }
        log::warn!("LLM 对话 {conv_id} 的 Send 含越界附件路径，整条拒绝");
        self.emit(LlmFrame::Error {
            conv_id: Some(conv_id),
            req_id: Some(req_id),
            code: LlmErrorCode::CwdInvalid,
            // **固定文案、不回显路径**：回显等于把「这条路径在被控端上存在与否」这一位
            // 信息也一并奉还，而控制端本来就知道自己发的是什么。
            message: LlmText::new("附件路径不在本对话的附件暂存目录内，已拒绝整条消息"),
        });
    }

    /// 发一条控制指令（中断 / 改权限模式）。
    fn control(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        req_id: u64,
        cmd: &ControlCmd,
        ctx: &mut PumpCtx<'_>,
    ) {
        let Some(conv) = self.checked_mut(conv_id, generation, Some(req_id)) else {
            return;
        };
        let Some(runner) = ctx.runners.get_mut(conv.runner) else {
            return;
        };
        if let Err(err) = runner.control(cmd) {
            log::warn!("LLM 对话 {conv_id} 控制指令失败：{err}");
        }
    }

    /// 关闭一个对话（优雅停止：先关 stdin 让 CLI 见 EOF 自然退出）。
    ///
    /// **这里不移除 `convs` 里的条目**：真正的终态由 `RunnerEvent::Exited` 驱动，
    /// 那时才发 `ConvExited`。提前移除会让退出事件找不到归属、`ConvExited` 永远发不出去，
    /// 手机上表现为「点了关闭，转圈不停」——正是本片要根治的那类症状。
    fn close_conv(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        req_id: u64,
        ctx: &mut PumpCtx<'_>,
    ) {
        let Some(conv) = self.checked_mut(conv_id, generation, Some(req_id)) else {
            return;
        };
        let runner = conv.runner;
        if let Err(err) = ctx.runners.close(runner) {
            log::warn!("LLM 对话 {conv_id} 关闭失败：{err}");
        }
    }

    /// 控制端订阅一个对话，回 `Attached` 基线帧。
    ///
    /// # `known_seq` 落后时为什么是「整体重建」而不是「从 transcript 补发」
    /// §6.7 的硬契约是「手机断线不杀进程，等它回来把断线期间的内容给它」。**内容不能丢**
    /// 是契约，「必须走 transcript 环形缓冲补发」只是片 2 当初设想的实现手段。本片选了
    /// 另一条同样满足契约、且明显更安全的路：
    ///
    /// - **`conv.turns` / `conv.cur` 是与连接状态无关的完整真相**。`on_runner_event` 无论
    ///   有没有会话都照常把块写进 `TurnBuild`，`reset_protocol_state` 也从不碰它们。所以
    ///   `HistoryReq` / `TurnFetch` 能给出的是**定稿轮记录**，比按 seq 补发的增量流更全
    ///   （后者还会漏掉 `shed_backlog` 降级时丢掉的中间增量）。
    /// - **`replay_from` 那条路要重跑 `on_runner_event`，而它是有状态且不幂等的**：
    ///   重跑会二次 `push_back` 进 `conv.turns`、重开 / 重关 `cur`、二次
    ///   `accumulate_usage`（用量当场翻倍）、并重发 `TurnStarted` / `TurnEnded`。
    ///   要正确补发就得另写一条「只读 transcript → 拼 `LlmDeltaItem`」的旁路，
    ///   并且为了让补发帧的 seq 与当初发出去的那些**逐一对上**，还要在 `Conv` 上常驻一份
    ///   (proto_seq → transcript_seq) 的锚点环。收益是省一次整轮拉取，代价是一整套新的
    ///   序号不变量——在「控制端还没有任何实现」的当下，这笔账不划算。
    ///
    /// 故本方法的判据是「**只要不是严格同步，就要求重建**」：
    /// `known_generation != conv.generation`（对话运行时重建过）、
    /// `known_seq != conv.seq`（落后或超前皆然）、
    /// 或 [`Conv::resync_due`]（有内容根本没进过 `Delta` 流）。
    ///
    /// 代价是控制端多拉一次整轮；换掉的是「断线期间的产出永久看不见、且两端零迹象」。
    fn attach(
        &mut self,
        conv_id: ConvId,
        known_generation: ConvGeneration,
        known_seq: u64,
        ctx: &mut PumpCtx<'_>,
    ) {
        if !self.convs.contains_key(&conv_id) {
            self.emit(LlmFrame::Error {
                conv_id: Some(conv_id),
                req_id: None,
                code: LlmErrorCode::StaleSession,
                message: LlmText::new("对话不存在（可能已退出并被回收）"),
            });
            return;
        }
        self.subscribe(conv_id, ctx.runners);
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        // 代不匹配 = 对话运行时重建过（进程重启 / 恢复 / 换 CLI），控制端手上那份状态整体作废；
        // 水位不等 = 落后（断线期间有产出）或超前（换了对端 / 本端重启过），两种都不能当同步；
        // `resync_due` = 有内容压根没进过 `Delta` 流，光看 seq 发现不了（见该字段的长注释）。
        let resync_required =
            known_generation != conv.generation || known_seq != conv.seq || conv.resync_due;
        // 只在**真回了一次基线**之后才清：这一帧就是「重建从此刻开始」的那条线。
        conv.resync_due = false;
        let seq = conv.watermark();
        let generation = conv.generation;
        let meta = conv.meta.clone();
        if resync_required {
            log::info!(
                "LLM 对话 {conv_id} 重新订阅需整体重建（对端 gen={known_generation}/seq={known_seq}，\
                 本端 gen={generation}/seq={seq}）"
            );
        }
        self.emit(LlmFrame::Attached {
            meta,
            seq,
            resync_required,
        });
    }

    /// 控制端取消订阅（**不结束对话**——§6.7 硬契约）。
    fn detach(&mut self, conv_id: ConvId, generation: ConvGeneration, ctx: &mut PumpCtx<'_>) {
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        // `conv_generation` 带 `#[serde(default)]`，`0` = 老控制端没带 ⇒ 按「不校验」兼容处理
        // （协议注释里写死的过渡语义）。非 0 且不等则必须丢弃：翻转期间迟到的旧代 `Detach`
        // 会把**新一代**的订阅退掉，手机上表现为「明明停在这个对话页，增量却停了」。
        if generation != 0 && generation != conv.generation {
            log::warn!("LLM 对话 {conv_id} 收到陈旧代的 Detach（{generation}），丢弃");
            return;
        }
        if conv.subscribed {
            conv.subscribed = false;
            // 攒着还没发的那批增量当场丢掉（没人在听了），并记账：它们已在轮记录里，
            // 但控制端光看 seq 发现不了缺了这一段，故下次 `Attach` 必须整体重建。
            if !conv.pending.is_empty() {
                conv.resync_due = true;
                conv.pending.clear();
                conv.pending_bytes = 0;
                conv.flush_at = None;
            }
            if let Some(runner) = ctx.runners.get_mut(conv.runner) {
                runner.detach();
            }
        }
    }

    /// 控制端确认消费到 `seq`：滑动窗口前移 + 反向 ack runner 的 transcript。
    fn on_delta_ack(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        seq: u64,
        ctx: &mut PumpCtx<'_>,
    ) {
        let Some(conv) = self.checked_mut(conv_id, generation, None) else {
            return;
        };
        if seq <= conv.acked_seq {
            return; // 迟到 / 重复 ACK，幂等忽略。
        }
        conv.acked_seq = seq;
        let mut transcript_seq = 0;
        while let Some((proto, tseq)) = conv.ack_map.front().copied() {
            // `ack_map` 的条目只在 `flush_conv` 真发出一帧时产生，`proto` 恒非 0
            // （老写法里的「未 flush 占位」机制已随拆帧一并删除，见 `Conv::ack_map` 的注释）。
            if proto > seq {
                break;
            }
            transcript_seq = tseq;
            conv.ack_map.pop_front();
            conv.inflight = conv.inflight.saturating_sub(1);
        }
        let runner = conv.runner;
        if transcript_seq > 0 {
            if let Some(r) = ctx.runners.get_mut(runner) {
                r.ack(transcript_seq);
            }
        }
    }

    /// 拉单轮定稿快照（`Delta.seq` 断裂 / `truncated` 后的**唯一**自愈通道）。
    fn on_turn_fetch(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        req_id: u64,
        turn: TurnNo,
    ) {
        let Some(conv) = self.checked_mut(conv_id, generation, Some(req_id)) else {
            return;
        };
        let seq = conv.watermark();
        let record = conv
            .turns
            .iter()
            .find(|r| r.turn == turn)
            .cloned()
            .or_else(|| {
                conv.cur
                    .as_ref()
                    .filter(|b| b.turn == turn)
                    .map(build_record)
            });
        let Some(record) = record else {
            self.emit(LlmFrame::Error {
                conv_id: Some(conv_id),
                req_id: Some(req_id),
                code: LlmErrorCode::StaleSession,
                message: LlmText::new("该轮已不在可补齐窗口内"),
            });
            return;
        };
        self.emit(LlmFrame::TurnSnapshot {
            conv_id,
            conv_generation: generation,
            req_id,
            seq,
            record: clamp_record(record, LLM_TURN_SNAPSHOT_MAX_BYTES),
        });
    }

    /// 向前翻历史。
    fn on_history_req(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        req_id: u64,
        before_turn: TurnNo,
        max_turns: u16,
    ) {
        let Some(conv) = self.checked_mut(conv_id, generation, Some(req_id)) else {
            return;
        };
        let want = max_turns.clamp(1, LLM_HISTORY_MAX_TURNS_PER_PAGE) as usize;
        // 从新到旧收，收满 `want` 轮或撞上字节预算就停；再翻回升序发出。
        let mut picked: Vec<LlmTurnRecord> = Vec::new();
        let mut bytes = 0usize;
        for record in conv.turns.iter().rev().filter(|r| r.turn < before_turn) {
            if picked.len() >= want {
                break;
            }
            let size = record_bytes(record);
            if !picked.is_empty() && bytes + size > LLM_HISTORY_PAGE_MAX_BYTES {
                break;
            }
            bytes += size;
            picked.push(record.clone());
        }
        let oldest_turn = picked.last().map_or(before_turn, |r| r.turn);
        let has_more = conv.turns.iter().any(|r| r.turn < oldest_turn);
        picked.reverse();
        self.emit(LlmFrame::HistoryPage {
            conv_id,
            conv_generation: generation,
            req_id,
            oldest_turn,
            turns: picked,
            has_more,
        });
    }

    /// 取对话的可变引用并做**代际校验**（`ConvGeneration` 的不变量）。
    ///
    /// 校验失败时按 [`STALE_REPLY_MAX`] 节流回 `Error{StaleSession}`：控制端据此知道
    /// 「我手上的代过期了，该重新 `Attach`」，而不是对着一条永远不回的请求转圈。
    fn checked_mut(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        req_id: Option<u64>,
    ) -> Option<&mut Conv> {
        let known = self.convs.get(&conv_id).map(|c| c.generation);
        match known {
            Some(g) if g == generation => self.convs.get_mut(&conv_id),
            _ => {
                self.reject_stale(conv_id, generation, req_id, known);
                None
            }
        }
    }

    /// 代际不匹配 / 对话不存在的统一拒绝出口。
    fn reject_stale(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        req_id: Option<u64>,
        known: Option<ConvGeneration>,
    ) {
        log::warn!(
            "LLM 对话 {conv_id} 代际不匹配（收到 {generation}，本端 {known:?}），丢弃该请求"
        );
        let over_quota = self.convs.get_mut(&conv_id).is_some_and(|conv| {
            conv.stale_replies = conv.stale_replies.saturating_add(1);
            conv.stale_replies > STALE_REPLY_MAX
        });
        if over_quota {
            return;
        }
        self.emit(LlmFrame::Error {
            conv_id: Some(conv_id),
            req_id,
            code: LlmErrorCode::StaleSession,
            message: LlmText::new("对话代号不匹配，请重新 Attach"),
        });
    }

    /// 方向不对 / 本端不该收到的帧。
    fn reject_wrong_direction(&mut self, frame: &LlmFrame, now: Instant) {
        // **只打变体名，不打帧内容**：`LlmFrame` 的 `Debug` 虽已按 `LlmText` / `LlmPath` /
        // `LlmToolInput` 脱敏，但整帧 Debug 仍会带出 `cli_session_id`、块结构等元信息，
        // 而这条日志的诊断价值只在「哪个 op 走错了方向」。
        let name = frame_op_name(frame);
        self.log_unsupported(now, &format!("收到方向不符的 LLM 帧 op={name}，丢弃"));
    }

    // ── 控制端：处理被控端事件 ──────────────────────────────────────────────

    /// 控制端收到被控端的一条事件。
    ///
    /// P0 桌面端不渲染 LLM 会话（§12-⑫），故这一半只做三件事：**握手收口**、
    /// **发 `DeltaAck` 维持对端窗口**、**发现 seq 断裂就发 `TurnFetch`**。理由见 [`CtlConv`]。
    ///
    /// **同样刻意超出 60 行的目标值（63 行）**，理由与 [`Self::on_controller_request`] 相同：
    /// 穷尽 `match` 不拆。
    fn on_host_event(&mut self, frame: LlmFrame, ctx: &mut PumpCtx<'_>) {
        match frame {
            LlmFrame::HelloAck {
                llm_proto, caps, ..
            } => {
                log::info!("LLM 能力握手完成：对端子协议 v{llm_proto}，caps={caps:?}");
                self.hello = HelloStateCell(HelloState::Ready { llm_proto, caps });
            }
            LlmFrame::Attached { meta, seq, .. } => {
                // 基线帧：把水位直接顶到基线值，此前的迟到 `Delta` 一律丢弃（D3 保序）。
                self.ctl_convs.insert(
                    meta.conv_id,
                    CtlConv {
                        generation: meta.conv_generation,
                        seq,
                        turn: meta.cur_turn,
                        fetch_inflight: false,
                    },
                );
            }
            LlmFrame::ConvStarted { meta, seq, .. } => {
                self.ctl_convs.insert(
                    meta.conv_id,
                    CtlConv {
                        generation: meta.conv_generation,
                        seq,
                        turn: meta.cur_turn,
                        fetch_inflight: false,
                    },
                );
            }
            LlmFrame::Delta {
                conv_id,
                conv_generation,
                seq,
                turn,
                ..
            } => self.on_delta(conv_id, conv_generation, seq, turn, ctx),
            LlmFrame::TurnStarted {
                conv_id, turn, seq, ..
            }
            | LlmFrame::TurnEnded {
                conv_id, turn, seq, ..
            } => self.note_turn(conv_id, turn, seq),
            LlmFrame::TurnSnapshot {
                conv_id,
                seq,
                record,
                ..
            } => {
                // 快照是基线：覆盖水位并解除 `TurnFetch` 在途标记（缺口已补齐）。
                if let Some(c) = self.ctl_convs.get_mut(&conv_id) {
                    c.seq = c.seq.max(seq);
                    c.turn = c.turn.max(record.turn);
                    c.fetch_inflight = false;
                }
            }
            LlmFrame::ConvExited { conv_id, .. } => {
                self.ctl_convs.remove(&conv_id);
            }
            other => self.note_host_misc(&other, ctx.now),
        }
    }

    /// 控制端收到一批增量：保序判定 → `DeltaAck`（或 `TurnFetch`）。
    fn on_delta(
        &mut self,
        conv_id: ConvId,
        generation: ConvGeneration,
        seq: u64,
        turn: TurnNo,
        ctx: &mut PumpCtx<'_>,
    ) {
        let Some(conv) = self.ctl_convs.get_mut(&conv_id) else {
            return; // 没订阅过的对话，丢弃（可能是别的控制端的流被误转过来）。
        };
        if generation != conv.generation {
            return; // 代不匹配一律丢弃（不是「尽量丢」——漏一处就串台）。
        }
        conv.turn = conv.turn.max(turn);
        if seq <= conv.seq {
            return; // 基线之后的迟到帧：QUIC↔中继翻转的乱序在这里被自动收敛。
        }
        if seq != conv.seq + 1 {
            // 断裂 ⇒ 整轮快照覆盖重建。**同一轮只发一次**，否则一个缺口会引发 N 次拉取。
            let expected = conv.seq + 1; // 必须在改水位**之前**取，否则日志里的「期望」恒等于「收到」。
            let already = conv.fetch_inflight;
            conv.fetch_inflight = true;
            conv.seq = seq;
            if already {
                return;
            }
            let req_id = next_req_id(ctx.req_seq);
            log::warn!(
                "LLM 对话 {conv_id} 的 Delta 序号断裂（期望 {expected}，收到 {seq}），拉整轮快照补齐"
            );
            self.emit(LlmFrame::TurnFetch {
                conv_id,
                conv_generation: generation,
                req_id,
                turn,
            });
            return;
        }
        conv.seq = seq;
        // **每条 Delta 都回 ACK**，不做批量：批量 ACK 省下的是几十字节的小帧，
        // 换来的是「窗口在对端那边多停留一会儿」——而对端窗口一满就转本地积压、
        // 积压一满就丢中间增量。这笔账在任何链路上都不划算。
        self.emit(LlmFrame::DeltaAck {
            conv_id,
            conv_generation: generation,
            seq,
        });
    }

    /// 记录轮号与水位（`TurnStarted` / `TurnEnded` 只盖戳、不占号，见模块文档二）。
    fn note_turn(&mut self, conv_id: ConvId, turn: TurnNo, seq: u64) {
        if let Some(conv) = self.ctl_convs.get_mut(&conv_id) {
            conv.turn = conv.turn.max(turn);
            conv.seq = conv.seq.max(seq);
        }
    }

    /// 控制端侧暂不消费的帧（`RateLimit` / `PermissionRequest` / `AgentList` / …）。
    ///
    /// P0 桌面端没有 LLM UI，这些帧没有渲染去处。**记 `info` 不记 `warn`**：它们是对端的
    /// 正常行为，不是故障；打成 warn 会把真正的问题淹掉。
    fn note_host_misc(&mut self, frame: &LlmFrame, now: Instant) {
        match frame {
            LlmFrame::Error { code, .. } => {
                log::warn!("LLM 对端报错：{code:?}");
            }
            LlmFrame::ConvFailed { code, .. } => {
                log::warn!("LLM 对话启动失败：{code:?}");
            }
            other => {
                let name = frame_op_name(other);
                log::debug!("控制端暂不消费的 LLM 帧 op={name}（P0 桌面端无 LLM UI）");
                let _ = now;
            }
        }
    }

    // ── runner 事件 → 帧 ────────────────────────────────────────────────────

    /// 把本帧的 runner 事件转成上行帧。
    fn ingest_events(&mut self, ctx: &mut PumpCtx<'_>) {
        for (runner, tseq, event) in ctx.events {
            let Some(conv_id) = self.by_runner.get(runner).copied() else {
                continue; // 不是经由本平面起的 runner（本地 HUD 起的），不上行。
            };
            self.on_runner_event(conv_id, *tseq, event, ctx.now);
        }
    }

    /// 一条 runner 事件的归一化上行。
    fn on_runner_event(&mut self, conv_id: ConvId, tseq: u64, event: &RunnerEvent, now: Instant) {
        match event {
            RunnerEvent::Spawned { .. } => self.set_state(conv_id, LlmConvState::Starting),
            RunnerEvent::Started(info) => self.on_started(conv_id, info),
            RunnerEvent::TurnUserEcho { turn, blocks } => {
                self.on_user_echo(conv_id, *turn, blocks);
            }
            RunnerEvent::Block { turn, entry } => {
                self.on_block(conv_id, *turn, entry, tseq, now);
            }
            RunnerEvent::TurnEnded(info) => self.on_turn_ended(conv_id, info),
            RunnerEvent::RateLimit { info } => self.on_rate_limit(conv_id, info),
            RunnerEvent::Exited {
                code,
                killed,
                stderr_empty,
            } => self.on_exited(conv_id, *code, *killed, *stderr_empty),
            RunnerEvent::PermissionModeChanged { mode } => {
                if let Some(conv) = self.convs.get_mut(&conv_id) {
                    conv.meta.permission_mode = Some(mode.clone());
                }
            }
            RunnerEvent::LineDropped { .. } => {
                if let Some(conv) = self.convs.get_mut(&conv_id) {
                    conv.dropped_lines = conv.dropped_lines.saturating_add(1);
                }
            }
            RunnerEvent::ProtocolError { kind } => self.on_protocol_error(conv_id, kind),
            // `PermissionAsk` 的上游通道（`can_use_tool`）未实测，片 3 刻意没建模，
            // 故本变体在 P0 无构造点；`ControlAck` 是内部消费、不进线协议。
            RunnerEvent::PermissionAsk { .. } | RunnerEvent::ControlAck { .. } => {}
        }
    }

    /// `system/init` 到达：把真实探测出来的元信息补进 `meta`。
    fn on_started(&mut self, conv_id: ConvId, info: &StartedInfo) {
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        conv.meta.model.clone_from(&info.model);
        conv.meta.cli_session_id = Some(info.cli_session_id.clone());
        if let Some(mode) = &info.permission_mode {
            conv.meta.permission_mode = Some(mode.clone());
        }
        conv.meta.state = LlmConvState::Idle;
        conv.meta.updated_ms = now_ms();
        // `info.cwd` / `info.cli_version` / `info.features` **刻意不上行**：
        // cwd 本端已有（`Start` 时就是我们给的）、版本号与能力位属排障信息，
        // 手机端的能力判据是 `HelloAck.agents[].features`，不是每个对话各带一份。
    }

    /// 用户消息回执（`isReplay`）。
    fn on_user_echo(&mut self, conv_id: ConvId, turn: u64, blocks: &[LlmBlockEntry]) {
        let turn = clamp_turn(turn);
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        match conv.cur.as_mut().filter(|b| b.turn == turn) {
            // 同一轮：`Send` 那一刻已经发过 `TurnStarted`，这里只把用户块换成 CLI 回吐的
            // 归一化版本（更准），**不重发帧**——重发会让手机端的气泡流多一条重复的用户消息。
            Some(build) => build.user = blocks.to_vec(),
            None => {
                let started_ms = now_ms();
                self.begin_turn(conv_id, turn, blocks.to_vec(), started_ms);
            }
        }
    }

    /// 一个已定稿的归一化块。
    ///
    /// P0 不做块内流式（上游 headless 是**整块**给出的），故一个块打成
    /// `BlockStart` + `BlockEnd{block: None}` 两项一起发。
    fn on_block(
        &mut self,
        conv_id: ConvId,
        turn: u64,
        entry: &LlmBlockEntry,
        tseq: u64,
        now: Instant,
    ) {
        let turn = clamp_turn(turn);
        // 上游先出块、`TurnStarted` 还没发出去的情形（`isReplay` 未实测，不能指望它先到）：
        // 就地补一轮，否则这些块会挂在一个不存在的轮上，手机端永远等不到轮头。
        if self
            .convs
            .get(&conv_id)
            .is_some_and(|c| c.cur.as_ref().is_none_or(|b| b.turn != turn))
        {
            self.begin_turn(conv_id, turn, Vec::new(), now_ms());
        }
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        if let Some(build) = conv.cur.as_mut() {
            build.assistant.push(entry.clone());
        }
        conv.meta.state = LlmConvState::Running;
        conv.meta.updated_ms = now_ms();
        conv.pending_turn = turn;
        push_delta(
            conv,
            LlmDeltaItem::BlockStart {
                entry: entry.clone(),
            },
            tseq,
            now,
        );
        push_delta(
            conv,
            LlmDeltaItem::BlockEnd {
                block_id: entry.block_id,
                block: None,
            },
            tseq,
            now,
        );
    }

    /// 开一轮并发 `TurnStarted`。
    fn begin_turn(
        &mut self,
        conv_id: ConvId,
        turn: TurnNo,
        user: Vec<LlmBlockEntry>,
        started_ms: i64,
    ) {
        // **先 flush 再换轮**：`Delta.turn` 取的是 `Conv::pending_turn`，而那批增量属于**上一轮**。
        // 换轮之后再冲会把上一轮的块打上新轮号（见 `Conv::pending_turn` 的长注释）。
        self.flush_conv(conv_id);
        // 换轮 = 上一轮无论如何都定稿了（哪怕没收到 `result`），否则它会永远挂在 `cur` 上、
        // 既进不了历史也拉不到快照。
        self.finish_turn(conv_id, None);
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        conv.cur = Some(TurnBuild {
            turn,
            user: user.clone(),
            assistant: Vec::new(),
            started_ms,
            truncated: false,
        });
        conv.meta.cur_turn = turn;
        conv.meta.state = LlmConvState::Running;
        conv.meta.updated_ms = started_ms;
        let (conv_id_v, generation, seq) = (conv.meta.conv_id, conv.generation, conv.watermark());
        self.emit_stream(
            conv_id,
            LlmFrame::TurnStarted {
                conv_id: conv_id_v,
                conv_generation: generation,
                seq,
                turn,
                user,
                started_ms,
            },
        );
    }

    /// 一轮结束：先把合并窗口里的增量 flush 出去（保序），再发 `TurnEnded`。
    fn on_turn_ended(&mut self, conv_id: ConvId, info: &TurnEndedInfo) {
        // **必须先 flush**：`TurnEnded` 是轮的封口，排在未发出的 `Delta` 之前到达会让
        // 手机端先看见「这轮结束了」再收到本轮内容，时间线是乱的。
        self.flush_conv(conv_id);
        let ended_ms = now_ms();
        self.finish_turn(conv_id, Some(info));
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        accumulate_usage(&mut conv.meta.usage, &info.usage);
        conv.meta.state = LlmConvState::Idle;
        conv.meta.updated_ms = ended_ms;
        let truncated = conv
            .turns
            .back()
            .is_some_and(|record| !record.complete || record.blocks_omitted > 0);
        let (id, generation, seq) = (conv.meta.conv_id, conv.generation, conv.watermark());
        self.emit_stream(
            conv_id,
            LlmFrame::TurnEnded {
                conv_id: id,
                conv_generation: generation,
                seq,
                turn: clamp_turn(info.turn),
                stop: info.stop.clone(),
                outcome: info.outcome.clone(),
                usage: info.usage,
                ended_ms,
                truncated,
            },
        );
    }

    /// 把 `cur` 定稿进 `turns`。
    fn finish_turn(&mut self, conv_id: ConvId, info: Option<&TurnEndedInfo>) {
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        let Some(build) = conv.cur.take() else {
            return;
        };
        let mut record = build_record(&build);
        if let Some(info) = info {
            record.stop = Some(info.stop.clone());
            record.outcome = info.outcome.clone();
            record.usage = info.usage;
            record.ended_ms = Some(now_ms());
        }
        conv.turns.push_back(record);
        while conv.turns.len() > CONV_TURN_HISTORY_MAX {
            conv.turns.pop_front();
        }
    }

    /// 账号额度播报。**刻意不领 seq**（协议注释：领了 seq 就会让每次限流事件在控制端
    /// 制造一个「缺口」并触发一次整轮拉取——一条纯状态广播把自愈通道变成刷屏器）。
    fn on_rate_limit(&mut self, conv_id: ConvId, info: &LlmRateLimit) {
        let Some(conv) = self.convs.get(&conv_id) else {
            return;
        };
        let generation = conv.generation;
        self.emit_stream(
            conv_id,
            LlmFrame::RateLimit {
                conv_id,
                conv_generation: generation,
                observed_ms: now_ms(),
                info: info.clone(),
            },
        );
    }

    /// 子进程退出：flush → 定稿未完的轮 → `ConvExited`（终态）。
    fn on_exited(&mut self, conv_id: ConvId, code: Option<i32>, killed: bool, stderr_empty: bool) {
        self.flush_conv(conv_id);
        self.finish_turn(conv_id, None);
        let Some(conv) = self.convs.get_mut(&conv_id) else {
            return;
        };
        conv.meta.state = LlmConvState::Exited;
        conv.meta.updated_ms = now_ms();
        // 附件暂存夹回收：里面装的是控制端传上来的**用户照片**，对话已终态就不该再留在
        // 临时目录里等一次系统清理。best-effort——删不掉（外部程序占用）就算了，
        // 不为一次清理失败影响终态帧的投递。
        if let Err(err) = std::fs::remove_dir_all(conv.attach_dir.as_str()) {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::debug!("LLM 附件暂存夹回收失败：{err}");
            }
        }
        let (id, generation, seq) = (conv.meta.conv_id, conv.generation, conv.watermark());
        let reason: LlmExitReason = if killed {
            LlmExitReason::Closed
        } else {
            LlmExitReason::ProcessExited
        };
        log::info!(
            "LLM 对话 {id} 已退出（code={code:?} killed={killed} stderr_empty={stderr_empty}）"
        );
        self.emit_stream(
            conv_id,
            LlmFrame::ConvExited {
                conv_id: id,
                conv_generation: generation,
                seq,
                reason,
                code,
                // **绝不是 stderr 尾部**：那份 tail 装着本机路径与内部堆栈。见模块文档三。
                message: exit_hint(code, killed, stderr_empty),
            },
        );
    }

    /// 协议 / 解析错误。
    fn on_protocol_error(&mut self, conv_id: ConvId, kind: &ProtocolErrorKind) {
        // 只有 stdin 相关的两种是**会话级致命**（半行已经出去了 = 会话必死），
        // 其余只影响一行，读线程会继续读下一行，不值得打扰控制端。
        let fatal = matches!(
            kind,
            ProtocolErrorKind::StdinRejected | ProtocolErrorKind::StdinWriteFailed
        );
        log::warn!("LLM 对话 {conv_id} 协议错误：{kind:?}（致命={fatal}）");
        if !fatal {
            return;
        }
        self.emit(LlmFrame::Error {
            conv_id: Some(conv_id),
            req_id: None,
            code: LlmErrorCode::Protocol,
            // 固定文案：`ProtocolErrorKind` 的所有变体都刻意不带上游原文，
            // 这里也不能把 `{kind:?}` 拼进去当「更详细的说明」。
            message: LlmText::new("与 CLI 的输入通道已损坏，本对话不可再收发消息"),
        });
    }

    /// 改一个对话的运行态。
    fn set_state(&mut self, conv_id: ConvId, state: LlmConvState) {
        if let Some(conv) = self.convs.get_mut(&conv_id) {
            conv.meta.state = state;
            conv.meta.updated_ms = now_ms();
        }
    }

    // ── 合并窗口与背压（§5.7）───────────────────────────────────────────────

    /// 到期的合并窗口全部 flush。
    fn flush_all(&mut self, now: Instant) {
        let due: Vec<ConvId> = self
            .convs
            .iter()
            .filter(|(_, c)| {
                !c.pending.is_empty()
                    && (c.pending_bytes >= LLM_DELTA_MAX_BYTES
                        || c.flush_at.is_some_and(|t| now >= t))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.flush_conv(id);
            // 窗口满时 `flush_conv` 转本地积压并**不清 `flush_at`**。必须在这里把它往后推：
            // 不推的话 [`Self::next_wake`] 会永远返回一个**过去**的时刻，而 `RemoteWs::tick_llm`
            // 对过去时刻排的是 `request_repaint_after(1ms)` —— 于是在对端 ACK 回来之前，
            // 整个事件循环以 1 kHz 空转（仓库里 `ControlFlow` 粘性 `WaitUntil(过去)` 造成
            // 单核拉满是有过事故记录的，见 `main.rs` 那段注释）。往后推一个合并窗口 =
            // 33 ms 一次重试，既不空转又不会让积压干等。
            if let Some(conv) = self.convs.get_mut(&id) {
                if !conv.pending.is_empty() {
                    conv.flush_at = Some(now + Duration::from_millis(LLM_DELTA_FLUSH_MS));
                }
            }
        }
    }

    /// 把一个对话攒着的增量**立刻**打成一帧 `Delta` 发出（忽略 33 ms 合并窗口）。
    ///
    /// 合并窗口的到期判断在 [`Self::flush_all`]；本方法是保序 flush 的入口
    /// （轮结束 / 进程退出前必须先把在攒的增量冲出去）。但**仍然尊重 ACK 窗口**
    /// ——窗口满了还硬发就等于没有背压。
    /// # 单帧字节上限：[`LLM_DELTA_MAX_BYTES`] 在这里是**每帧上限**，不是触发阈值
    /// 该常量的类型文档原文是「达到即立刻 flush 并开新帧……远低于服务端 4 MiB 单帧上限」。
    /// 老实现只把它当 [`Self::flush_all`] 的**触发条件**用，本方法则 `mem::take` 整池打成
    /// 一帧——于是单帧字节数**没有上限**。这不是理论风险：
    ///
    /// - `LlmRunner::pump_once` 一次 `while let Ok(ev) = rx.try_recv()` 排空整条有界通道
    ///   （cap 4096），[`Self::pump`] 把它们全部 `push_delta` 进 `pending`，帧尾只 flush 一次；
    /// - 单个 `ToolResult` 可达 `LLM_TOOL_OUTPUT_MAX_BYTES` = 32 KiB、单个 `ToolUse` 入参
    ///   可达 64 KiB，而协议注释自己算过「一轮 128 次工具调用 = 4.01 MiB，已顶穿 4 MiB」；
    /// - 服务端 `MAX_WS_MESSAGE` 是 4 MiB，超限**断整条 WS**——终端镜像跟着一起断。
    ///
    /// 故这里按批取：从 `pending` **前部**累加到 [`LLM_DELTA_MAX_BYTES`] 为止各成一帧，
    /// 每批各领一个 seq、各自在 `ack_map` 里占一格。单项自身就超限时**仍单独成帧**
    /// （取不出批 = 死循环，而单项超限只能靠上游的 `LLM_TOOL_OUTPUT_MAX_BYTES` 夹紧兜底）。
    ///
    /// 循环由 ACK 窗口封顶：最多 [`LLM_DELTA_WINDOW`] 帧 × 128 KiB ≈ 2 MiB，之后窗口满转本地
    /// 积压，剩下的留在 `pending` 等 ACK。
    fn flush_conv(&mut self, conv_id: ConvId) {
        loop {
            let Some(conv) = self.convs.get_mut(&conv_id) else {
                return;
            };
            if conv.pending.is_empty() {
                conv.flush_at = None;
                return;
            }
            // 未订阅时不该有 pending（`push_delta` 挡在前面）；真出现了就地清掉并记账，
            // 不能发——发出去的 Delta 在服务端查不到会话，纯粹白烧带宽还把 seq 顶上去。
            if !conv.subscribed {
                conv.pending.clear();
                conv.pending_bytes = 0;
                conv.flush_at = None;
                conv.resync_due = true;
                return;
            }
            if conv.window_full() {
                // 窗口满 ⇒ 转本地积压。**绝不停止读子进程 stdout**：那会填满管道缓冲区并卡死
                // CLI 进程本身（`LlmFrame::DeltaAck` 的长注释把这条与文件传输背压的差异写死了）。
                shed_backlog(conv);
                return;
            }
            let batch = take_delta_batch(conv);
            conv.seq += 1;
            conv.inflight = conv.inflight.saturating_add(1);
            let seq = conv.seq;
            conv.ack_map.push_back((seq, batch.tseq));
            while conv.ack_map.len() > LLM_DELTA_WINDOW as usize {
                conv.ack_map.pop_front();
            }
            if conv.pending.is_empty() {
                conv.flush_at = None;
            }
            let turn = conv.pending_turn;
            let (id, generation) = (conv.meta.conv_id, conv.generation);
            self.emit(LlmFrame::Delta {
                conv_id: id,
                conv_generation: generation,
                seq,
                turn,
                items: batch.items,
            });
        }
    }

    /// 只发给**订阅中**对话的实时流帧（`Delta` 之外的那几种：轮头 / 轮尾 / 退出 / 限流）。
    ///
    /// # 为什么不能像老实现那样无条件 `emit`
    /// [`PumpCtx::role`] 的文档写着「`None` = 无会话，此时只做清理，不产出任何帧」，协议里
    /// `Detach` 就是「取消订阅」。老实现两处都没落地：会话断开后仍在跑的 runner 会继续把帧
    /// 打进中继（服务端 `relay()` 查不到会话直接丢弃），后果不只是白烧带宽——`conv.seq` 与
    /// `inflight` 一路涨到窗口满、积压涨到上限后 `shed_backlog` 把 `build.truncated` 置真，
    /// 于是一个**内容其实完整**的轮被标成 `complete=false` / `TurnEnded.truncated=true`，
    /// 控制端回来还要为它多发一次 `TurnFetch`。
    ///
    /// 未订阅时内容并不丢：块早在 `on_block` 里就写进 `TurnBuild` 了，控制端下次 `Attach`
    /// 会拿到 `resync_required: true`（[`Conv::resync_due`]）并整体重建。
    fn emit_stream(&mut self, conv_id: ConvId, frame: LlmFrame) {
        if !self.convs.get(&conv_id).is_some_and(|c| c.subscribed) {
            return;
        }
        self.emit(frame);
    }

    /// 出站口：**唯一**往 [`Self::out`] 塞帧的地方，白名单严出的第三道门在这里（模块文档三）。
    fn emit(&mut self, frame: LlmFrame) {
        if !sendable(&frame) {
            // 走到这里说明代码里某处构造了兜底变体。debug 构建直接炸出来，release 丢弃 + 告警
            // ——静默发出去才是最糟的（对端会把它当成一个真的「未知能力」）。
            debug_assert!(false, "试图发送兜底变体：{}", frame_op_name(&frame));
            log::error!(
                "LLM 出站帧含兜底变体，已丢弃（op={}）",
                frame_op_name(&frame)
            );
            return;
        }
        self.out.push(LlmOut::Frame(Box::new(frame)));
    }
}

// ── 自由函数 ──────────────────────────────────────────────────────────────────

/// 分配一个 `req_id`。
///
/// **计数器本体是 `RemoteWs::req_seq`，绝不另起一个**：设计蓝图 §5.5 把「单调、跳 0 哨兵、
/// 进程内永不重置」列为陈旧应答去重的基础，两个计数器各自单调不等于合起来单调。
pub(super) fn next_req_id(seq: &mut u64) -> u64 {
    *seq = seq.wrapping_add(1);
    if *seq == 0 {
        *seq = 1;
    }
    *seq
}

/// 把一条增量项压进合并窗口。
///
/// **未订阅时直接丢弃并置 [`Conv::resync_due`]**：没人在听的实时流不该占 seq、不该占内存。
/// 内容不丢——调用方 [`LlmPlane::on_block`] 已经先把块写进 `TurnBuild`，控制端下次 `Attach`
/// 会被要求整体重建。
fn push_delta(conv: &mut Conv, item: LlmDeltaItem, tseq: u64, now: Instant) {
    if !conv.subscribed {
        conv.resync_due = true;
        return;
    }
    conv.pending_bytes = conv.pending_bytes.saturating_add(delta_item_bytes(&item));
    conv.pending.push(PendingItem { item, tseq });
    if conv.flush_at.is_none() {
        conv.flush_at = Some(now + Duration::from_millis(LLM_DELTA_FLUSH_MS));
    }
}

/// 从 `pending` **前部**切一批出来，累计字节不超过 [`LLM_DELTA_MAX_BYTES`]。
///
/// 恒至少取 1 项：单项自身就超限时也要让它单独成帧，否则 [`LlmPlane::flush_conv`] 的循环
/// 取不出东西又清不掉 `pending` ⇒ 死循环。
fn take_delta_batch(conv: &mut Conv) -> DeltaBatch {
    let mut bytes = 0usize;
    let mut take = 0usize;
    for p in &conv.pending {
        let size = delta_item_bytes(&p.item);
        if take > 0 && bytes + size > LLM_DELTA_MAX_BYTES {
            break;
        }
        bytes += size;
        take += 1;
    }
    let rest = conv.pending.split_off(take);
    let batch = std::mem::replace(&mut conv.pending, rest);
    conv.pending_bytes = conv.pending_bytes.saturating_sub(bytes);
    DeltaBatch {
        // 本批对应的最大 transcript seq。`max` 而不是「最后一项」：`shed_backlog` 合成的
        // `Dropped` 项 tseq 为 0，会排在真项后面，取末项会把 ack 位点倒退回 0。
        tseq: batch.iter().map(|p| p.tseq).max().unwrap_or(0),
        items: batch.into_iter().map(|p| p.item).collect(),
    }
}

/// [`take_delta_batch`] 的产物：一帧 `Delta` 的载荷 + 它对应的 transcript ack 位点。
struct DeltaBatch {
    items: Vec<LlmDeltaItem>,
    tseq: u64,
}

/// 积压超限时的降级：丢中间增量、只留终态项，并把丢掉的字节声明成 `Dropped`。
///
/// **降级必须可观测**：控制端收到 `Dropped` 就知道「这块有缺口」，配合 `TurnEnded.truncated`
/// 去发 `TurnFetch` 补齐。静默丢是仓库明令禁止的（`LlmDeltaItem::Dropped` 的类型文档拿
/// `RemoteFrame::PaneOp` 当反面教材）。
fn shed_backlog(conv: &mut Conv) {
    if conv.pending_bytes <= LLM_BACKLOG_MAX_BYTES {
        return;
    }
    let mut kept: Vec<PendingItem> = Vec::with_capacity(conv.pending.len());
    let mut shed: BTreeMap<BlockId, u64> = BTreeMap::new();
    let mut bytes = 0usize;
    for pending in std::mem::take(&mut conv.pending) {
        let size = delta_item_bytes(&pending.item);
        // 已经降到预算之内就不再丢了：目标是把内存拉回线内，不是清空。
        if bytes + size <= LLM_BACKLOG_MAX_BYTES / 2 {
            bytes += size;
            kept.push(pending);
            continue;
        }
        match &pending.item {
            // 终态项恒保留：块的封口丢了，控制端连「这块结束了」都不知道。
            LlmDeltaItem::BlockEnd { .. } | LlmDeltaItem::Dropped { .. } => {
                bytes += size;
                kept.push(pending);
            }
            LlmDeltaItem::BlockStart { entry } => {
                *shed.entry(entry.block_id).or_default() += size as u64;
            }
            LlmDeltaItem::TextAppend { block_id, text } => {
                *shed.entry(*block_id).or_default() += text.len_bytes() as u64;
            }
            LlmDeltaItem::Unknown => {}
        }
    }
    for (block_id, dropped) in shed {
        kept.push(PendingItem {
            item: LlmDeltaItem::Dropped {
                block_id,
                bytes: dropped,
            },
            // 合成项没有对应的 transcript 记录：`0` 会在 `take_delta_batch` 的 `max` 里被
            // 真项盖过，不会把 ack 位点拖回去。
            tseq: 0,
        });
        bytes += 24; // `Dropped` 是定长小项，粗算即可。
    }
    log::warn!(
        "LLM 对话 {} 本地积压超 {LLM_BACKLOG_MAX_BYTES} 字节，已丢弃中间增量（保留终态）",
        conv.meta.conv_id
    );
    conv.pending = kept;
    conv.pending_bytes = bytes;
    if let Some(build) = conv.cur.as_mut() {
        build.truncated = true;
    }
}

/// 一条增量项的近似字节数（合并窗口与积压预算用，只要求同量级）。
fn delta_item_bytes(item: &LlmDeltaItem) -> usize {
    const ITEM_OVERHEAD: usize = 48;
    ITEM_OVERHEAD
        + match item {
            LlmDeltaItem::BlockStart { entry } => block_bytes(&entry.block),
            LlmDeltaItem::TextAppend { text, .. } => text.len_bytes(),
            LlmDeltaItem::BlockEnd { block, .. } => block.as_ref().map_or(0, block_bytes),
            LlmDeltaItem::Dropped { .. } | LlmDeltaItem::Unknown => 0,
        }
}

/// 一个块的近似字节数。
fn block_bytes(block: &LlmBlock) -> usize {
    match block {
        LlmBlock::Text { text } => text.len_bytes(),
        LlmBlock::Thinking { .. } | LlmBlock::Unknown => 0,
        LlmBlock::ToolUse { name, input, .. } => {
            name.len() + serde_json::to_vec(input.get()).map_or(0, |v| v.len())
        }
        LlmBlock::ToolResult {
            call_id, output, ..
        } => call_id.len() + output.len_bytes(),
        LlmBlock::Image { attachment } => attachment.path.as_str().len(),
        LlmBlock::Error { message, .. } => message.len_bytes(),
    }
}

/// 一条轮记录的近似字节数（分页预算用）。
fn record_bytes(record: &LlmTurnRecord) -> usize {
    const RECORD_OVERHEAD: usize = 128;
    RECORD_OVERHEAD
        + record
            .user
            .iter()
            .chain(record.assistant.iter())
            .map(|e| block_bytes(&e.block) + 48)
            .sum::<usize>()
}

/// 把 `TurnBuild` 定稿成 [`LlmTurnRecord`]。
fn build_record(build: &TurnBuild) -> LlmTurnRecord {
    LlmTurnRecord {
        turn: build.turn,
        user: build.user.clone(),
        assistant: build.assistant.clone(),
        stop: None,
        outcome: LlmTurnOutcome::default(),
        usage: LlmUsage::default(),
        started_ms: build.started_ms,
        ended_ms: None,
        complete: !build.truncated,
        blocks_omitted: 0,
    }
}

/// 把一条轮记录夹到字节预算内。**从最老的助手块起删**（保留轮尾，那是用户最想看的），
/// 删几块就把几块写进 `blocks_omitted`。
///
/// 自愈通道自己超限 = 缺口永久补不上（中继侧超 4 MiB 断整条 WS、QUIC 侧静默丢帧），
/// 这个夹紧不是优化、是 `LLM_TURN_SNAPSHOT_MAX_BYTES` 类型文档写死的硬要求。
fn clamp_record(mut record: LlmTurnRecord, budget: usize) -> LlmTurnRecord {
    let mut omitted = 0u32;
    while record_bytes(&record) > budget && !record.assistant.is_empty() {
        record.assistant.remove(0);
        omitted += 1;
    }
    // 用户块也删不下来时只能认了：至少 `blocks_omitted` 是诚实的。
    while record_bytes(&record) > budget && record.user.len() > 1 {
        record.user.remove(0);
        omitted += 1;
    }
    record.blocks_omitted = omitted;
    if omitted > 0 {
        record.complete = false;
    }
    record
}

/// 用量累加（对话级累计）。
fn accumulate_usage(total: &mut LlmUsage, turn: &LlmUsage) {
    total.input_tokens = total.input_tokens.saturating_add(turn.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(turn.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(turn.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(turn.cache_write_tokens);
    total.cost_micro_usd = total.cost_micro_usd.saturating_add(turn.cost_micro_usd);
    // 下面三个是**当前值不是累加值**：上下文占用与窗口大小按最新一轮取，
    // 累加会得到一个随轮数线性增长的假数字。
    total.context_used = turn.context_used;
    total.context_limit = turn.context_limit;
    total.max_output_tokens = turn.max_output_tokens;
}

/// **附件路径围栏**：任何一条附件不落在本对话的 `attach_dir` 之内就返回 `true`（整条拒绝）。
///
/// # 这道门为什么必须存在
/// `LlmFrame::Send.attachments[].path` 是**控制端全权指定**的绝对路径，`submit_message` 把它
/// 原样传给 runner，`claude.rs::compile_prompt` 会把它逐字拼进提示词并附一句「请按下列路径
/// 读取对应文件」。没有这道门，控制端就能点名让被控端 PC 读**任意**本机文件，内容再经
/// assistant 块顺着 `LlmFrame::Delta` 流回控制端——这是白名单严出（模块文档三）在反方向上
/// 的一个整口子，而不是「理论风险」。
///
/// `attach_dir` 存在的**全部意义**就是当这道围栏：协议的设计是「控制端先用 `PutBegin` /
/// `PutChunk` / `PutEnd` 把图片传到这里，再按绝对路径引用」（`ensure_attach_dir` 与协议侧
/// `LlmAttachment` 的注释）。引用一个不在这里的路径，按定义就不是一条合法的附件。
///
/// # 判据用 `canonicalize` 而不是字符串前缀比较
/// 裸 `starts_with` 挡不住 `<attach_dir>\..\..\Users\x\.ssh\id_rsa`，也挡不住符号链接与
/// Windows 上的 8.3 短名。两侧都过 `canonicalize` 之后比较的是解析完的真实路径，
/// 前缀语义才成立（`Path::starts_with` 按**路径分量**比对，不会把 `C:\a` 当成 `C:\ab` 的前缀）。
///
/// # 取不到规范路径一律判越界
/// 附件不存在 / 权限不足 / 目录没建起来 —— 这些情况下无从证明它在围栏内，
/// 而「证明不了就放行」正是围栏最常见的失效方式。
///
/// ⚠ 与之相对，`Start.cwd` 那条口子**仍然敞着**（见 [`LlmPlane::start_conv`] 里的记账）：
/// 本仓库没有「允许的工作目录根」概念，凭空定一个会与既有文件面行为不一致。两者的差别在于
/// cwd 是用户自己在控制端选的工作目录（且受配对信任约束），而附件路径本该由被控端自己发放。
fn attachments_outside_fence(
    attach_dir: &LlmPath,
    attachments: &[lumen_protocol::llm::LlmAttachment],
) -> bool {
    if attachments.is_empty() {
        return false;
    }
    let Ok(root) = std::fs::canonicalize(attach_dir.as_str()) else {
        log::warn!("LLM 附件暂存目录不可解析，本条 Send 的附件一律按越界处理");
        return true;
    };
    attachments
        .iter()
        .any(|att| !std::fs::canonicalize(att.path.as_str()).is_ok_and(|p| p.starts_with(&root)))
}

/// 提交一条用户消息。
///
/// **调用前必须已过 [`attachments_outside_fence`]**：本函数把 `attachments` 原样交给 runner，
/// 自身不做任何路径校验。
fn submit_message(
    conv: &mut Conv,
    runners: &mut LlmRunnerManager,
    text: &LlmText,
    attachments: &[lumen_protocol::llm::LlmAttachment],
) {
    let Some(runner) = runners.get_mut(conv.runner) else {
        return;
    };
    match runner.submit(text.as_str(), attachments) {
        Ok(_turn) => {
            if conv.meta.cur_turn == 0 {
                // 首条消息定标题（被控端职责，见 `LlmConvMeta::title`）。
                conv.meta.title = title_of(text.as_str());
            }
            conv.meta.state = LlmConvState::Running;
            conv.meta.updated_ms = now_ms();
        }
        Err(err) => log::warn!("LLM 对话 {} 提交消息失败：{err}", conv.meta.conv_id),
    }
}

/// 取文本首行的前 [`TITLE_MAX_CHARS`] 个字符作标题。
fn title_of(text: &str) -> LlmText {
    let head: String = text
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(TITLE_MAX_CHARS)
        .collect();
    if head.is_empty() {
        LlmText::new("新对话")
    } else {
        LlmText::new(head)
    }
}

/// 目录末段（对话初始标题）。
fn dir_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// 退出提示。
///
/// # 这个函数是白名单严出的执行者之一
/// 诱人的写法是把 [`ExitAttribution::FromStderr`] 的 `tail` 拼进来——「反正用户想知道
/// 为什么挂了」。那份 tail 是 CLI stderr 的最后 64 行，装着本机绝对路径与内部堆栈，
/// `llm_runner` 模块文档二为此专门把 stderr 排除在 transcript 之外（transcript 是手机
/// 重连补发的数据源）。从这里塞回去等于把那道门从背后打开。
///
/// 故本函数**只回本端写死的短文案 + 退出码**，真正的诊断留在 PC 侧的日志与审计里。
fn exit_hint(code: Option<i32>, killed: bool, stderr_empty: bool) -> Option<LlmText> {
    if killed {
        return None; // 我们自己关的，没什么可解释的。
    }
    match (code, stderr_empty) {
        (None | Some(0), _) => None,
        // 实测：认证失败时 `exit code = 1` 且 stderr 完全为空，唯一的失败依据在最后一条
        // `result` 里——那条已经作为 `TurnEnded.outcome` 上行过了，这里只提示去看它。
        (Some(c), true) => Some(LlmText::new(format!(
            "CLI 异常退出（退出码 {c}，无诊断输出；失败原因见本轮结果）"
        ))),
        (Some(c), false) => Some(LlmText::new(format!(
            "CLI 异常退出（退出码 {c}，诊断输出已记入本机日志）"
        ))),
    }
}

/// [`RunnerError`] → 协议错误码。
fn runner_error_code(err: &RunnerError) -> LlmErrorCode {
    match err {
        RunnerError::LimitReached(_) => LlmErrorCode::LimitReached,
        RunnerError::CliNotFound(_) => LlmErrorCode::AgentNotFound,
        RunnerError::CwdInvalid => LlmErrorCode::CwdInvalid,
        RunnerError::NotFound(_) | RunnerError::Exited => LlmErrorCode::StaleSession,
        RunnerError::Encode(_) | RunnerError::StdinClosed | RunnerError::Backlogged(_) => {
            LlmErrorCode::Protocol
        }
        // P0 桌面端与手机端都不提供 bypass 开关；认不出的权限模式只能保守拒绝。
        RunnerError::BypassForbidden | RunnerError::UnknownPermissionMode => LlmErrorCode::Protocol,
        RunnerError::Spawn(_) => LlmErrorCode::Io,
    }
}

/// 本端可用的 agent 清单。
///
/// **`available` 靠真探测 PATH，不靠「装了就当有」**：回一个假的 `true` 会让手机端把入口
/// 点亮，用户点下去才拿到 `AgentNotFound`——那是把一次明确的「没装」拖成一次失败的启动。
fn available_agents() -> Vec<LlmAgentInfo> {
    let adapter = ClaudeAdapter::new();
    vec![LlmAgentInfo {
        agent: adapter.kind(),
        available: program_on_path(adapter.program()),
        // 版本号要真跑一次 `claude --version`（一次进程启动）才拿得到。**不为一条排障字段
        // 付一次 spawn**：真正的版本号会随 `system/init` 到达（`StartedInfo::cli_version`），
        // 那时才是有依据的值。
        version: None,
        // 模型清单同理：CLI 没有「列出可用模型」的稳定接口，编一份清单必然过期。
        models: Vec::new(),
        // 只声明 `claude.rs::permission_mode_to_wire` 真映射得出来的那三个。
        // **`Bypass` 刻意不在列**：P0 桌面端与手机端都不提供这个开关（§4.4 / §6.8.4），
        // `LaunchSpec::set_permission_mode` 在类型层已挡一道，这里是「不声明就不会被选」的第二道。
        permission_modes: vec![
            LlmPermissionMode::Manual,
            LlmPermissionMode::AcceptEdits,
            LlmPermissionMode::Plan,
        ],
        features: adapter.baseline_features(),
    }]
}

/// 可执行文件是否在 PATH 上。
///
/// 走 PATH 查找而不是写死绝对路径（`LlmAgentAdapter::program` 的类型文档）。Windows 上
/// 还要按 `PATHEXT` 试后缀——只试裸文件名会把 `claude.cmd`（npm 全局安装的常见形态）
/// 判成「没装」。
fn program_on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_owned())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        Vec::new()
    };
    std::env::split_paths(&paths).any(|dir| {
        let base = dir.join(program);
        base.is_file()
            || exts
                .iter()
                .any(|ext| base.with_extension(&ext[1..]).is_file())
    })
}

/// 建（或复用）一个对话的附件暂存目录。
///
/// 控制端先用现成的 `PutBegin` / `PutChunk` / `PutEnd` 把图片字节传到这里，再在
/// `LlmFrame::Send.attachments` 里按绝对路径引用（协议侧 `LlmAttachment` 的设计）。
///
/// 建目录失败时回一个仍然合法的路径而不是报错：附件是可选能力，为它挡掉整个对话的启动
/// 不成比例；真到传文件那一步失败，`PutResult` 会带明确的 `FsErr`。
fn ensure_attach_dir(conv_id: ConvId) -> LlmPath {
    let dir = std::env::temp_dir()
        .join(ATTACH_DIR_ROOT)
        .join(format!("conv-{conv_id}"));
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::warn!("LLM 附件目录创建失败（附件功能不可用）：{err}");
    }
    LlmPath::new(dir.display().to_string())
}

/// runner 内部轮号（`u64`）→ 协议轮号（`u32`）。
///
/// 片 2 刻意把内部计数器定成 `u64`「不为了对齐线格式在内部承担溢出讨论，转换留给片 4」。
/// 这里就是那个落点：饱和而不是回绕——回绕会让轮号从 `u32::MAX` 跳回 0，
/// 控制端的 `HashMap<TurnNo, _>` 当场串台。
fn clamp_turn(turn: u64) -> TurnNo {
    TurnNo::try_from(turn).unwrap_or(TurnNo::MAX)
}

/// 一帧的 `op` 名（只用于日志，**不含任何帧内容**）。
fn frame_op_name(frame: &LlmFrame) -> &'static str {
    match frame {
        LlmFrame::Hello { .. } => "Hello",
        LlmFrame::HelloAck { .. } => "HelloAck",
        LlmFrame::ListAgents { .. } => "ListAgents",
        LlmFrame::AgentList { .. } => "AgentList",
        LlmFrame::ListConvs { .. } => "ListConvs",
        LlmFrame::ConvList { .. } => "ConvList",
        LlmFrame::Start { .. } => "Start",
        LlmFrame::ConvStarted { .. } => "ConvStarted",
        LlmFrame::ConvFailed { .. } => "ConvFailed",
        LlmFrame::Send { .. } => "Send",
        LlmFrame::Interrupt { .. } => "Interrupt",
        LlmFrame::Close { .. } => "Close",
        LlmFrame::ConvExited { .. } => "ConvExited",
        LlmFrame::Attach { .. } => "Attach",
        LlmFrame::Attached { .. } => "Attached",
        LlmFrame::Detach { .. } => "Detach",
        LlmFrame::Delta { .. } => "Delta",
        LlmFrame::DeltaAck { .. } => "DeltaAck",
        LlmFrame::TurnStarted { .. } => "TurnStarted",
        LlmFrame::TurnEnded { .. } => "TurnEnded",
        LlmFrame::RateLimit { .. } => "RateLimit",
        LlmFrame::TurnFetch { .. } => "TurnFetch",
        LlmFrame::TurnSnapshot { .. } => "TurnSnapshot",
        LlmFrame::HistoryReq { .. } => "HistoryReq",
        LlmFrame::HistoryPage { .. } => "HistoryPage",
        LlmFrame::PermissionRequest { .. } => "PermissionRequest",
        LlmFrame::PermissionReply { .. } => "PermissionReply",
        LlmFrame::PermissionResolved { .. } => "PermissionResolved",
        LlmFrame::Error { .. } => "Error",
        LlmFrame::Unknown => "Unknown",
    }
}

/// **发送侧白名单闸门**：兜底变体永不上线。
///
/// `LlmFrame::Unknown` / `LlmBlock::Unknown` / `LlmDeltaItem::Unknown` /
/// `LlmConvState::Unknown` 全部标着「只用于反序列化兜底，永不发送」。发出去的后果不是崩，
/// 是**语义污染**：对端会把它当成「我不认识的新能力」，从此走未知值分支——一个不会报错、
/// 只会长期失真的 bug。
fn sendable(frame: &LlmFrame) -> bool {
    match frame {
        LlmFrame::Unknown => false,
        LlmFrame::Delta { items, .. } => items.iter().all(sendable_item),
        LlmFrame::TurnStarted { user, .. } => user.iter().all(|e| sendable_block(&e.block)),
        LlmFrame::TurnSnapshot { record, .. } => sendable_record(record),
        LlmFrame::HistoryPage { turns, .. } => turns.iter().all(sendable_record),
        LlmFrame::Attached { meta, .. } | LlmFrame::ConvStarted { meta, .. } => {
            meta.state != LlmConvState::Unknown
        }
        LlmFrame::ConvList { convs, .. } | LlmFrame::HelloAck { convs, .. } => {
            convs.iter().all(|m| m.state != LlmConvState::Unknown)
        }
        _ => true,
    }
}

fn sendable_item(item: &LlmDeltaItem) -> bool {
    match item {
        LlmDeltaItem::Unknown => false,
        LlmDeltaItem::BlockStart { entry } => sendable_block(&entry.block),
        LlmDeltaItem::BlockEnd { block, .. } => block.as_ref().is_none_or(sendable_block),
        LlmDeltaItem::TextAppend { .. } | LlmDeltaItem::Dropped { .. } => true,
    }
}

fn sendable_block(block: &LlmBlock) -> bool {
    !matches!(block, LlmBlock::Unknown)
}

fn sendable_record(record: &LlmTurnRecord) -> bool {
    record
        .user
        .iter()
        .chain(record.assistant.iter())
        .all(|e| sendable_block(&e.block))
}

/// 当前 Unix 毫秒。取不到（系统时钟早于 1970）时回 0——**不 panic**：
/// 一个时钟异常的机器不该让整条 LLM 通路挂掉，时间戳错了顶多让排序失真。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// 随机、非 0 的对话代号。
///
/// 取值必须留在 `i64::MAX` 以内（`ConvGeneration` 的类型文档）：控制端是 Dart，
/// `int` 是 64 位**有符号**，`jsonDecode` 遇到超过 `i64::MAX` 的字面量会退化成 `double`
/// 并丢精度，两端从此静默不一致。故这里显式右移一位。
fn random_generation() -> ConvGeneration {
    use rand_core::{OsRng, RngCore};
    loop {
        let g = OsRng.next_u64() >> 1;
        if g != 0 {
            return g;
        }
    }
}

/// 生成一个 v4 形状的 uuid 当 CLI 会话 id。
///
/// 不引 `uuid` crate：全仓没有这个依赖，为一个 36 字符的字符串加一条依赖不划算。
/// 版本位与变体位按 RFC 4122 置好——CLI 侧对 `--session-id` 的格式校验是否严格未实测，
/// 给一个**合规**的 uuid 是成本最低的稳妥做法。
fn new_cli_session_id() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_runner::ExitAttribution;
    use lumen_protocol::llm::{
        LlmStopReason, LlmThinking, LlmToolStatus, LLM_TOOL_OUTPUT_MAX_BYTES,
    };

    /// 造一个不起任何子进程的管理器（`Waker::noop` 是片 2 为此留的口子）。
    fn manager() -> LlmRunnerManager {
        LlmRunnerManager::new(crate::llm_runner::Waker::noop())
    }

    /// 跑一次泵，返回本次产出的帧。
    fn pump(plane: &mut LlmPlane, role: Option<Role>, now: Instant) -> Vec<LlmFrame> {
        let mut runners = manager();
        let mut req_seq = 0u64;
        let mut ctx = PumpCtx {
            now,
            role,
            runners: &mut runners,
            events: &[],
            req_seq: &mut req_seq,
        };
        plane.pump(&mut ctx);
        take_frames(plane)
    }

    /// 跑一次泵并喂入 runner 事件。
    fn pump_events(
        plane: &mut LlmPlane,
        now: Instant,
        events: &[(RunnerId, u64, RunnerEvent)],
    ) -> Vec<LlmFrame> {
        let mut runners = manager();
        let mut req_seq = 0u64;
        let mut ctx = PumpCtx {
            now,
            role: Some(Role::Controlled),
            runners: &mut runners,
            events,
            req_seq: &mut req_seq,
        };
        plane.pump(&mut ctx);
        take_frames(plane)
    }

    fn take_frames(plane: &mut LlmPlane) -> Vec<LlmFrame> {
        plane
            .take_out()
            .into_iter()
            .map(|LlmOut::Frame(f)| *f)
            .collect()
    }

    /// 手工挂一个不带真 runner 的对话（本模块的状态机与子进程无关，这样才测得动）。
    fn seed_conv(plane: &mut LlmPlane, conv_id: ConvId, generation: ConvGeneration) {
        let meta = LlmConvMeta {
            conv_id,
            conv_generation: generation,
            agent: LlmAgentKind::Claude,
            cwd: LlmPath::new("F:\\proj"),
            title: LlmText::new("t"),
            state: LlmConvState::Idle,
            model: None,
            permission_mode: None,
            cli_session_id: None,
            origin: None,
            cur_turn: 0,
            created_ms: 0,
            updated_ms: 0,
            usage: LlmUsage::default(),
        };
        let runner = RunnerId(conv_id);
        plane.convs.insert(
            conv_id,
            Conv {
                runner,
                generation,
                meta,
                attach_dir: LlmPath::new("F:\\att"),
                seq: 0,
                acked_seq: 0,
                inflight: 0,
                // **默认订阅**：`ConvStarted` 与 `Attached` 都是基线帧，生产路径上
                // 一个对话被建起来时控制端就在收流。未订阅是特例，由需要它的测试自己置。
                subscribed: true,
                resync_due: false,
                pending: Vec::new(),
                pending_bytes: 0,
                flush_at: None,
                pending_turn: 0,
                ack_map: VecDeque::new(),
                cur: None,
                turns: VecDeque::new(),
                stale_replies: 0,
                dropped_lines: 0,
            },
        );
        plane.by_runner.insert(runner, conv_id);
    }

    fn text_block(block_id: BlockId, text: &str) -> LlmBlockEntry {
        LlmBlockEntry {
            block_id,
            parent_call_id: None,
            block: LlmBlock::Text {
                text: LlmText::new(text),
            },
        }
    }

    // ── 握手（§5.4）─────────────────────────────────────────────────────────

    #[test]
    fn 片4_控制端会话建立即发hello() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        plane.on_session_started(Role::Controller, now);
        let frames = take_frames(&mut plane);
        assert!(
            matches!(frames.as_slice(), [LlmFrame::Hello { llm_proto, .. }] if *llm_proto == LLM_PROTO_VERSION),
            "控制端必须在会话建立后立即发 Hello，实际 {frames:?}"
        );
    }

    #[test]
    fn 片4_被控端不主动发握手帧() {
        let mut plane = LlmPlane::default();
        plane.on_session_started(Role::Controlled, Instant::now());
        assert!(take_frames(&mut plane).is_empty(), "被控端不该主动发帧");
    }

    #[test]
    fn 片4_五秒无helloack判对端太老并只提示一次() {
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        plane.on_session_started(Role::Controller, t0);
        let _ = take_frames(&mut plane);

        // 未到期不判。
        assert!(!plane.tick(t0 + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS - 1)));
        assert!(!plane.take_peer_too_old());

        // 到期判定，且**只推一次**（第二次 tick 不再重复通知，否则会每帧刷屏）。
        assert!(plane.tick(t0 + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS)));
        assert!(plane.take_peer_too_old(), "超时必须推 LlmPeerTooOld");
        assert!(!plane.tick(t0 + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS + 10)));
        assert!(!plane.take_peer_too_old(), "同一次超时不得重复通知");
    }

    #[test]
    fn 片4_收到helloack即取消超时判定() {
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        plane.on_session_started(Role::Controller, t0);
        let _ = take_frames(&mut plane);
        plane.enqueue(
            LlmFrame::HelloAck {
                llm_proto: LLM_PROTO_VERSION,
                caps: vec!["perm".into()],
                agents: Vec::new(),
                convs: Vec::new(),
                rate_limit: None,
                rate_limit_observed_ms: None,
            },
            t0,
        );
        let _ = pump(&mut plane, Some(Role::Controller), t0);
        assert!(plane.peer_ready());
        assert!(!plane.tick(t0 + Duration::from_secs(60)));
        assert!(!plane.take_peer_too_old(), "已握手成功不得再判超时");
    }

    #[test]
    fn 片4_握手态随会话重建复位() {
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        plane.on_session_started(Role::Controller, t0);
        let _ = take_frames(&mut plane);
        assert!(plane.tick(t0 + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS)));
        assert!(plane.take_peer_too_old());

        // 关键：一次偶发超时**不得**让 LLM 面在整个会话生命周期内假死（§5.4-5）。
        plane.on_session_started(Role::Controller, t0);
        assert!(
            matches!(take_frames(&mut plane).as_slice(), [LlmFrame::Hello { .. }]),
            "会话重建必须重发 Hello"
        );
        assert!(plane.hello_deadline().is_some());
    }

    #[test]
    fn 片4_链路翻转重发hello() {
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        plane.on_session_started(Role::Controller, t0);
        let _ = take_frames(&mut plane);
        plane.on_link_switched(Role::Controller, t0 + Duration::from_secs(1));
        assert!(
            matches!(take_frames(&mut plane).as_slice(), [LlmFrame::Hello { .. }]),
            "QUIC↔中继翻转后必须重发 Hello（resubscribe_after_switch 只管终端流）"
        );
        // 被控端不参与。
        plane.on_link_switched(Role::Controlled, t0);
        assert!(take_frames(&mut plane).is_empty());
    }

    // ── 被控端分发 ──────────────────────────────────────────────────────────

    #[test]
    fn 片4_被控端收hello回helloack带agent清单() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        plane.enqueue(
            LlmFrame::Hello {
                llm_proto: LLM_PROTO_VERSION,
                caps: Vec::new(),
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        let [LlmFrame::HelloAck {
            llm_proto, agents, ..
        }] = frames.as_slice()
        else {
            panic!("应回一条 HelloAck，实际 {frames:?}");
        };
        assert_eq!(*llm_proto, LLM_PROTO_VERSION);
        assert_eq!(agents.len(), 1, "P0 只有 Claude 一个适配器");
        assert_eq!(agents[0].agent, LlmAgentKind::Claude);
    }

    #[test]
    fn 片4_未知op在入队处就被挡下不进inbox() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        plane.enqueue(LlmFrame::Unknown, now);
        assert!(plane.inbox.is_empty(), "兜底变体不该进入分发");
        assert!(pump(&mut plane, Some(Role::Controlled), now).is_empty());
    }

    #[test]
    fn 片4_方向不符的帧被丢弃() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        // 被控端不可能收到 Delta（那是它自己发的方向）。
        plane.enqueue(
            LlmFrame::Delta {
                conv_id: 1,
                conv_generation: 1,
                seq: 1,
                turn: 1,
                items: Vec::new(),
            },
            now,
        );
        assert!(pump(&mut plane, Some(Role::Controlled), now).is_empty());
    }

    #[test]
    fn 片4_无会话时收帧不产出任何东西() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        plane.enqueue(LlmFrame::ListAgents { req_id: 1 }, now);
        assert!(pump(&mut plane, None, now).is_empty());
    }

    #[test]
    fn 片4_不存在的agent显式拒绝而不是降级到claude() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        plane.enqueue(
            LlmFrame::Start {
                req_id: 7,
                agent: LlmAgentKind::Codex,
                cwd: LlmPath::new("F:\\proj"),
                model: None,
                permission_mode: None,
                resume: None,
                fork_session: false,
                append_system_prompt: None,
                allowed_tools: None,
                origin: None,
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            matches!(frames.as_slice(), [LlmFrame::ConvFailed { req_id: 7, code, .. }]
                if *code == LlmErrorCode::AgentNotFound),
            "非 Claude 必须显式拒绝，实际 {frames:?}"
        );
    }

    // ── 代际守卫 ────────────────────────────────────────────────────────────

    #[test]
    fn 片4_代际不匹配的请求被拒并节流() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 0xabc);
        for i in 0..(STALE_REPLY_MAX + 3) {
            plane.enqueue(
                LlmFrame::TurnFetch {
                    conv_id: 1,
                    conv_generation: 0xdef, // 错的代
                    req_id: u64::from(i) + 1,
                    turn: 1,
                },
                now,
            );
        }
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        assert_eq!(
            frames.len(),
            STALE_REPLY_MAX as usize,
            "陈旧代请求必须按 STALE_REPLY_MAX 节流，实际 {frames:?}"
        );
        assert!(frames
            .iter()
            .all(|f| matches!(f, LlmFrame::Error { code, .. }
            if *code == LlmErrorCode::StaleSession)));
    }

    #[test]
    fn 片4_陈旧代detach不得退掉新一代订阅() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 9);
        plane.convs.get_mut(&1).expect("conv").subscribed = true;
        plane.enqueue(
            LlmFrame::Detach {
                conv_id: 1,
                conv_generation: 8, // 上一代
            },
            now,
        );
        let _ = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            plane.convs[&1].subscribed,
            "旧代 Detach 必须丢弃，否则手机停在对话页却收不到增量"
        );
        // 老控制端不带代号（0）时按兼容处理，正常生效。
        plane.enqueue(
            LlmFrame::Detach {
                conv_id: 1,
                conv_generation: 0,
            },
            now,
        );
        let _ = pump(&mut plane, Some(Role::Controlled), now);
        assert!(!plane.convs[&1].subscribed);
    }

    #[test]
    fn 片4_attach水位落后必须要求重建_不得静默认为已同步() {
        // 这是本模块曾经最危险的一条静默失败：手机断线重连（`known_seq < conv.seq`）
        // 既不补发任何东西、也不置 `resync_required`，手机把水位直接顶到当前值就认为
        // 自己是同步的，断线期间产出的全部内容永久看不到，且两端零迹象。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 5);
        plane.convs.get_mut(&1).expect("conv").seq = 12;

        plane.enqueue(
            LlmFrame::Attach {
                conv_id: 1,
                known_generation: 5,
                known_seq: 3,
                known_turn: 1,
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            matches!(
                frames.as_slice(),
                [LlmFrame::Attached {
                    seq: 12,
                    resync_required: true,
                    ..
                }]
            ),
            "同代但水位落后 ⇒ 必须要求整体重建（走 HistoryReq/TurnFetch），实际 {frames:?}"
        );

        // 代不匹配 / 水位超前 ⇒ 同样要求重同步。
        for (gen, seq) in [(4u64, 3u64), (5, 99)] {
            plane.enqueue(
                LlmFrame::Attach {
                    conv_id: 1,
                    known_generation: gen,
                    known_seq: seq,
                    known_turn: 0,
                },
                now,
            );
            let frames = pump(&mut plane, Some(Role::Controlled), now);
            assert!(
                matches!(
                    frames.as_slice(),
                    [LlmFrame::Attached {
                        resync_required: true,
                        ..
                    }]
                ),
                "代={gen} 水位={seq} 应要求重同步，实际 {frames:?}"
            );
        }
    }

    #[test]
    fn 片4_attach水位严格相等且无遗留才判已同步() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 5);
        plane.convs.get_mut(&1).expect("conv").seq = 12;

        plane.enqueue(
            LlmFrame::Attach {
                conv_id: 1,
                known_generation: 5,
                known_seq: 12,
                known_turn: 1,
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            matches!(
                frames.as_slice(),
                [LlmFrame::Attached {
                    seq: 12,
                    resync_required: false,
                    ..
                }]
            ),
            "严格同步的重新订阅不该让手机白拉一整轮，实际 {frames:?}"
        );
    }

    #[test]
    fn 片4_断线丢掉的未发增量必须在下次attach要求重建() {
        // `reset_protocol_state` 把攒在合并窗口里、还没领 seq 的那批增量丢掉，而 `seq`
        // 一动不动 ⇒ 光比水位发现不了。少了 `resync_due` 这条账，手机回来会认为自己同步。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 5);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            conv.seq = 7;
            conv.pending.push(PendingItem {
                item: LlmDeltaItem::TextAppend {
                    block_id: 0,
                    text: LlmText::new("断线时还没发出去的这段"),
                },
                tseq: 3,
            });
            conv.pending_bytes = 32;
        }
        plane.on_session_ended();
        assert!(
            plane.convs[&1].resync_due,
            "丢了未发增量必须记账，否则下一次 Attach 会静默判成已同步"
        );

        plane.enqueue(
            LlmFrame::Attach {
                conv_id: 1,
                known_generation: 5,
                known_seq: 7, // 与本端水位**严格相等**
                known_turn: 1,
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            matches!(
                frames.as_slice(),
                [LlmFrame::Attached {
                    resync_required: true,
                    ..
                }]
            ),
            "水位相等但有内容没进过 Delta 流 ⇒ 仍须重建，实际 {frames:?}"
        );
        assert!(
            !plane.convs[&1].resync_due,
            "回过一次基线之后该标志必须清掉，否则每次 Attach 都白拉一轮"
        );
    }

    // ── seq 与流控（§5.7 / §5.8）────────────────────────────────────────────

    #[test]
    fn 片4_只有delta消费seq其余帧只盖水位戳() {
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        seed_conv(&mut plane, 1, 3);
        let runner = RunnerId(1);
        let events = vec![
            (
                runner,
                1,
                RunnerEvent::Block {
                    turn: 1,
                    entry: text_block(0, "a"),
                },
            ),
            (
                runner,
                2,
                RunnerEvent::TurnEnded(Box::new(TurnEndedInfo {
                    turn: 1,
                    stop: LlmStopReason::EndTurn,
                    outcome: LlmTurnOutcome::default(),
                    usage: LlmUsage::default(),
                    denials: 0,
                    result_text: None,
                    result_truncated_bytes: None,
                })),
            ),
        ];
        let frames = pump_events(&mut plane, t0, &events);
        let seqs: Vec<(&'static str, u64)> = frames
            .iter()
            .filter_map(|f| match f {
                LlmFrame::TurnStarted { seq, .. } => Some(("TurnStarted", *seq)),
                LlmFrame::Delta { seq, .. } => Some(("Delta", *seq)),
                LlmFrame::TurnEnded { seq, .. } => Some(("TurnEnded", *seq)),
                _ => None,
            })
            .collect();
        assert_eq!(
            seqs,
            vec![("TurnStarted", 0), ("Delta", 1), ("TurnEnded", 1)],
            "TurnStarted/TurnEnded 只盖水位戳；让它们领号会让 Delta 序列出现假缺口"
        );
    }

    #[test]
    fn 片4_delta连续且轮结束前先flush() {
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        seed_conv(&mut plane, 1, 3);
        let runner = RunnerId(1);
        // 两个块 + 轮结束：块还没到 33 ms 窗口，但 TurnEnded 必须先把它们冲出去。
        let events = vec![
            (
                runner,
                1,
                RunnerEvent::Block {
                    turn: 1,
                    entry: text_block(0, "a"),
                },
            ),
            (
                runner,
                2,
                RunnerEvent::Block {
                    turn: 1,
                    entry: text_block(1, "b"),
                },
            ),
            (
                runner,
                3,
                RunnerEvent::TurnEnded(Box::new(TurnEndedInfo {
                    turn: 1,
                    stop: LlmStopReason::EndTurn,
                    outcome: LlmTurnOutcome::default(),
                    usage: LlmUsage::default(),
                    denials: 0,
                    result_text: None,
                    result_truncated_bytes: None,
                })),
            ),
        ];
        let frames = pump_events(&mut plane, t0, &events);
        let order: Vec<&'static str> = frames.iter().map(frame_op_name).collect();
        assert_eq!(
            order,
            vec!["TurnStarted", "Delta", "TurnEnded"],
            "轮封口必须排在本轮增量之后，实际 {frames:?}"
        );
        let LlmFrame::Delta { items, .. } = &frames[1] else {
            panic!("第二帧应是 Delta");
        };
        assert_eq!(items.len(), 4, "两个块各打成 BlockStart + BlockEnd");
    }

    #[test]
    fn 片4_ack窗口满则不再发新delta() {
        let mut plane = LlmPlane::default();
        let mut now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        let runner = RunnerId(1);
        let mut sent = 0usize;
        for i in 0..(LLM_DELTA_WINDOW + 6) {
            let events = vec![(
                runner,
                u64::from(i) + 1,
                RunnerEvent::Block {
                    turn: 1,
                    entry: text_block(i, "x"),
                },
            )];
            // 两步：先把块推进合并窗口，再把时钟拨过 33 ms 让窗口到期。
            // 同一次 pump 里推进 + 到期是不可能的（窗口从**推进那一刻**起算），
            // 一步写法会得到「每两次迭代才出一帧」的假象。
            let _ = pump_events(&mut plane, now, &events);
            now += Duration::from_millis(LLM_DELTA_FLUSH_MS + 1);
            sent += pump_events(&mut plane, now, &[])
                .iter()
                .filter(|f| matches!(f, LlmFrame::Delta { .. }))
                .count();
        }
        assert_eq!(
            sent, LLM_DELTA_WINDOW as usize,
            "在途未 ACK 的 Delta 帧数必须封在 LLM_DELTA_WINDOW"
        );
        // ACK 到一半，窗口应重新打开。
        plane.enqueue(
            LlmFrame::DeltaAck {
                conv_id: 1,
                conv_generation: 3,
                seq: u64::from(LLM_DELTA_WINDOW),
            },
            now,
        );
        now += Duration::from_millis(LLM_DELTA_FLUSH_MS + 1);
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            frames.iter().any(|f| matches!(f, LlmFrame::Delta { .. })),
            "ACK 后窗口打开，积压应继续发出，实际 {frames:?}"
        );
    }

    #[test]
    fn 片4_控制端逐条ack并在断裂时拉整轮快照() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        plane.ctl_convs.insert(
            9,
            CtlConv {
                generation: 2,
                seq: 0,
                turn: 1,
                fetch_inflight: false,
            },
        );
        let delta = |seq: u64| LlmFrame::Delta {
            conv_id: 9,
            conv_generation: 2,
            seq,
            turn: 4,
            items: Vec::new(),
        };
        plane.enqueue(delta(1), now);
        let frames = pump(&mut plane, Some(Role::Controller), now);
        assert!(
            matches!(frames.as_slice(), [LlmFrame::DeltaAck { seq: 1, .. }]),
            "连续帧必须回 ACK，实际 {frames:?}"
        );

        // 跳号 ⇒ TurnFetch，且同一缺口只发一次。
        plane.enqueue(delta(5), now);
        plane.enqueue(delta(6), now);
        let frames = pump(&mut plane, Some(Role::Controller), now);
        assert_eq!(
            frames
                .iter()
                .filter(|f| matches!(f, LlmFrame::TurnFetch { turn: 4, .. }))
                .count(),
            1,
            "一个缺口只该触发一次整轮拉取，实际 {frames:?}"
        );
    }

    #[test]
    fn 片4_控制端丢弃基线之前的迟到delta与错代帧() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        plane.ctl_convs.insert(
            9,
            CtlConv {
                generation: 2,
                seq: 10,
                turn: 1,
                fetch_inflight: false,
            },
        );
        // 基线水位 10：seq ≤ 10 的一律丢（QUIC↔中继翻转乱序在此收敛）。
        plane.enqueue(
            LlmFrame::Delta {
                conv_id: 9,
                conv_generation: 2,
                seq: 7,
                turn: 1,
                items: Vec::new(),
            },
            now,
        );
        // 代不匹配同样丢。
        plane.enqueue(
            LlmFrame::Delta {
                conv_id: 9,
                conv_generation: 3,
                seq: 11,
                turn: 1,
                items: Vec::new(),
            },
            now,
        );
        assert!(pump(&mut plane, Some(Role::Controller), now).is_empty());
    }

    // ── 第 6 个清理点 ───────────────────────────────────────────────────────

    #[test]
    fn 片4_会话结束清订阅态但不清对话本身() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            conv.subscribed = true;
            conv.inflight = 5;
            conv.pending.push(PendingItem {
                item: LlmDeltaItem::TextAppend {
                    block_id: 0,
                    text: LlmText::new("x"),
                },
                tseq: 1,
            });
            conv.pending_bytes = 1;
            conv.flush_at = Some(now);
        }
        plane.ctl_convs.insert(
            1,
            CtlConv {
                generation: 3,
                seq: 1,
                turn: 1,
                fetch_inflight: true,
            },
        );

        plane.on_session_ended();

        let conv = &plane.convs[&1];
        assert!(!conv.subscribed, "订阅态必须跟着会话清");
        assert_eq!(conv.inflight, 0, "在途计数必须清（对端已经收不到了）");
        assert!(conv.pending.is_empty() && conv.flush_at.is_none());
        assert!(plane.ctl_convs.is_empty() && plane.inbox.is_empty());
        assert_eq!(plane.pending_detach, vec![RunnerId(1)]);
        // §6.7 硬契约：对话与子进程**不得**随会话结束消失。
        assert!(plane.convs.contains_key(&1), "会话结束不得丢掉对话本身");
    }

    #[test]
    fn 片4_登出清对话注册表并归还订阅计数() {
        let mut plane = LlmPlane::default();
        seed_conv(&mut plane, 1, 3);
        plane.convs.get_mut(&1).expect("conv").subscribed = true;
        plane.on_stop();
        assert!(plane.convs.is_empty() && plane.by_runner.is_empty());
        // `reset_protocol_state` 已把 `subscribed` 置 false 并推过一次，`on_stop` 自己那一遍
        // 因此不会重复推——订阅计数恰好归还一次。
        assert_eq!(plane.pending_detach, vec![RunnerId(1)]);
    }

    // ── 白名单严出（模块文档三）─────────────────────────────────────────────

    #[test]
    fn 片4_退出归因不得把stderr尾部写进帧() {
        // 归因里那份 tail 是本机路径与内部堆栈，绝不能进上行帧。
        let attribution = ExitAttribution::FromStderr {
            code: 1,
            tail: vec!["F:\\私密项目\\src\\main.rs:12 panic".into()],
        };
        let ExitAttribution::FromStderr { code, tail } = &attribution else {
            unreachable!();
        };
        let hint = exit_hint(Some(*code), false, false).expect("非零退出应有提示");
        assert!(
            !hint.as_str().contains(&tail[0]),
            "退出提示里出现了 stderr 原文：{}",
            hint.as_str()
        );
        assert!(hint.as_str().contains('1'), "退出码属于可上行的定长信息");
        // 我们自己关的：不编任何理由。
        assert!(exit_hint(Some(1), true, true).is_none());
        assert!(exit_hint(Some(0), false, true).is_none());
    }

    #[test]
    fn 片4_兜底变体在出站口被拦下() {
        assert!(!sendable(&LlmFrame::Unknown));
        assert!(!sendable(&LlmFrame::Delta {
            conv_id: 1,
            conv_generation: 1,
            seq: 1,
            turn: 1,
            items: vec![LlmDeltaItem::Unknown],
        }));
        assert!(!sendable(&LlmFrame::Delta {
            conv_id: 1,
            conv_generation: 1,
            seq: 1,
            turn: 1,
            items: vec![LlmDeltaItem::BlockStart {
                entry: LlmBlockEntry {
                    block_id: 0,
                    parent_call_id: None,
                    block: LlmBlock::Unknown,
                },
            }],
        }));
        // 正常内容照发。
        assert!(sendable(&LlmFrame::Delta {
            conv_id: 1,
            conv_generation: 1,
            seq: 1,
            turn: 1,
            items: vec![
                LlmDeltaItem::BlockStart {
                    entry: text_block(0, "hi")
                },
                LlmDeltaItem::BlockEnd {
                    block_id: 0,
                    block: None
                },
            ],
        }));
    }

    #[test]
    fn 片4_每个帧变体都有op名() {
        // `frame_op_name` 是穷尽 match：协议加变体时这里编译期就会红。
        // 顺便守住「日志里只出现 op 名、不出现帧内容」这条——名字是 `&'static str`，
        // 类型上就带不出内容。
        assert_eq!(frame_op_name(&LlmFrame::Unknown), "Unknown");
        assert_eq!(
            frame_op_name(&LlmFrame::ListAgents { req_id: 1 }),
            "ListAgents"
        );
    }

    // ── 积压降级 ────────────────────────────────────────────────────────────

    #[test]
    fn 片4_积压超限丢中间增量保留终态并声明dropped() {
        let mut plane = LlmPlane::default();
        seed_conv(&mut plane, 1, 3);
        let conv = plane.convs.get_mut(&1).expect("conv");
        conv.cur = Some(TurnBuild {
            turn: 1,
            user: Vec::new(),
            assistant: Vec::new(),
            started_ms: 0,
            truncated: false,
        });
        // 撑爆积压预算：每项 1 MiB 文本。
        let big = "x".repeat(1024 * 1024);
        for i in 0..8u32 {
            conv.pending.push(PendingItem {
                item: LlmDeltaItem::BlockStart {
                    entry: text_block(i, &big),
                },
                tseq: u64::from(i) * 2 + 1,
            });
            conv.pending.push(PendingItem {
                item: LlmDeltaItem::BlockEnd {
                    block_id: i,
                    block: None,
                },
                tseq: u64::from(i) * 2 + 2,
            });
        }
        conv.pending_bytes = conv.pending.iter().map(|p| delta_item_bytes(&p.item)).sum();
        assert!(conv.pending_bytes > LLM_BACKLOG_MAX_BYTES);

        shed_backlog(conv);

        assert!(
            conv.pending_bytes <= LLM_BACKLOG_MAX_BYTES,
            "降级后必须回到预算内，实际 {}",
            conv.pending_bytes
        );
        assert_eq!(
            conv.pending
                .iter()
                .filter(|p| matches!(p.item, LlmDeltaItem::BlockEnd { .. }))
                .count(),
            8,
            "终态项恒保留（丢了封口控制端连块结束都不知道）"
        );
        assert!(
            conv.pending
                .iter()
                .any(|p| matches!(p.item, LlmDeltaItem::Dropped { .. })),
            "降级必须可观测：要发 Dropped"
        );
        assert!(
            conv.cur.as_ref().is_some_and(|b| b.truncated),
            "本轮要标记 truncated，控制端据此发 TurnFetch 补齐"
        );
    }

    // ── 自愈通道 ────────────────────────────────────────────────────────────

    #[test]
    fn 片4_turnfetch回定稿快照且超预算时声明删了几块() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            let big = "y".repeat(200 * 1024);
            conv.turns.push_back(LlmTurnRecord {
                turn: 2,
                user: vec![text_block(0, "问题")],
                assistant: (1..12).map(|i| text_block(i, &big)).collect(),
                stop: None,
                outcome: LlmTurnOutcome::default(),
                usage: LlmUsage::default(),
                started_ms: 1,
                ended_ms: Some(2),
                complete: true,
                blocks_omitted: 0,
            });
        }
        plane.enqueue(
            LlmFrame::TurnFetch {
                conv_id: 1,
                conv_generation: 3,
                req_id: 42,
                turn: 2,
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        let [LlmFrame::TurnSnapshot { record, .. }] = frames.as_slice() else {
            panic!("应回一条 TurnSnapshot，实际 {frames:?}");
        };
        assert!(
            record_bytes(record) <= LLM_TURN_SNAPSHOT_MAX_BYTES,
            "自愈通道自己超限 = 缺口永久补不上"
        );
        assert!(record.blocks_omitted > 0, "删了块必须声明，不得静默丢");
        assert!(!record.complete);
    }

    #[test]
    fn 片4_历史分页按字节预算短页并推进游标() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            let big = "z".repeat(400 * 1024);
            for turn in 1..=6u32 {
                conv.turns.push_back(LlmTurnRecord {
                    turn,
                    user: Vec::new(),
                    assistant: vec![text_block(0, &big)],
                    stop: None,
                    outcome: LlmTurnOutcome::default(),
                    usage: LlmUsage::default(),
                    started_ms: 0,
                    ended_ms: None,
                    complete: true,
                    blocks_omitted: 0,
                });
            }
        }
        plane.enqueue(
            LlmFrame::HistoryReq {
                conv_id: 1,
                conv_generation: 3,
                req_id: 5,
                before_turn: 7,
                max_turns: 20,
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        let [LlmFrame::HistoryPage {
            oldest_turn,
            turns,
            has_more,
            ..
        }] = frames.as_slice()
        else {
            panic!("应回一页历史，实际 {frames:?}");
        };
        assert!(
            turns.len() < 6,
            "字节预算先于轮数生效，短页是正常结果（实际 {} 轮）",
            turns.len()
        );
        assert!(turns.windows(2).all(|w| w[0].turn < w[1].turn), "必须升序");
        assert_eq!(*oldest_turn, turns[0].turn, "游标必须是本页最老轮号");
        assert!(*has_more, "还有更老的轮");
    }

    // ── 事件归一化 ──────────────────────────────────────────────────────────

    #[test]
    fn 片4_块事件先补轮头再发增量() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        let frames = pump_events(
            &mut plane,
            now,
            &[(
                RunnerId(1),
                1,
                RunnerEvent::Block {
                    turn: 3,
                    entry: LlmBlockEntry {
                        block_id: 0,
                        parent_call_id: None,
                        block: LlmBlock::Thinking {
                            content: LlmThinking::Omitted,
                        },
                    },
                },
            )],
        );
        // 轮头是**基线帧**，立刻发；块进 33 ms 合并窗口，下一次到期的 pump 才成帧。
        assert_eq!(
            frames.iter().map(frame_op_name).collect::<Vec<_>>(),
            vec!["TurnStarted"],
            "块先于轮头到达时必须就地补一轮，否则块挂在不存在的轮上"
        );
        assert!(matches!(frames[0], LlmFrame::TurnStarted { turn: 3, .. }));

        let later = now + Duration::from_millis(LLM_DELTA_FLUSH_MS + 1);
        let frames = pump_events(&mut plane, later, &[]);
        let [LlmFrame::Delta {
            seq: 1,
            turn: 3,
            items,
            ..
        }] = frames.as_slice()
        else {
            panic!("合并窗口到期后应发出本轮增量，实际 {frames:?}");
        };
        assert_eq!(items.len(), 2, "一个块打成 BlockStart + BlockEnd");
    }

    #[test]
    fn 片4_进程退出发convexited终态() {
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        // 保持订阅：否则同一次 pump 里 `reap_convs` 就会把条目收走（手机没在看，
        // 而测试里那个空管理器等价于「runner 已被回收」）。
        plane.convs.get_mut(&1).expect("conv").subscribed = true;
        let frames = pump_events(
            &mut plane,
            now,
            &[(
                RunnerId(1),
                1,
                RunnerEvent::Exited {
                    code: Some(1),
                    killed: false,
                    stderr_empty: true,
                },
            )],
        );
        let [LlmFrame::ConvExited {
            reason,
            code,
            message,
            ..
        }] = frames.as_slice()
        else {
            panic!("应发 ConvExited，实际 {frames:?}");
        };
        assert_eq!(*reason, LlmExitReason::ProcessExited);
        assert_eq!(*code, Some(1));
        assert!(message.is_some(), "非零退出要给用户一句话，别只丢个退出码");
        assert_eq!(plane.convs[&1].meta.state, LlmConvState::Exited);
    }

    #[test]
    fn 片4_工具结果块的字节估算把正文算进去() {
        // `delta_item_bytes` 是积压预算的唯一依据，漏算某类块 = 那类块可以无限堆。
        let entry = LlmBlockEntry {
            block_id: 0,
            parent_call_id: None,
            block: LlmBlock::ToolResult {
                call_id: "toolu_1".into(),
                status: LlmToolStatus::Ok,
                output: LlmText::new("w".repeat(4096)),
                truncated_bytes: None,
                detail: None,
            },
        };
        let bytes = delta_item_bytes(&LlmDeltaItem::BlockStart { entry });
        assert!(bytes >= 4096, "工具结果正文必须计入预算，实际 {bytes}");
    }

    #[test]
    fn 片4_换轮前必须先冲掉上一轮攒着的增量() {
        // `Delta.turn` 是帧级字段，而换轮与 flush 是两件独立的事：上一轮的最后几个块还在
        // 33 ms 窗口里时新一轮就可能开始。不先冲就会给它们打上新轮号，手机端按
        // `(turn, block_id)` 归约时把气泡挂到错误的一轮上。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        let runner = RunnerId(1);
        let _ = pump_events(
            &mut plane,
            now,
            &[(
                runner,
                1,
                RunnerEvent::Block {
                    turn: 1,
                    entry: text_block(0, "第一轮的尾巴"),
                },
            )],
        );
        // 第 1 轮的块还压在窗口里（未到 33 ms），第 2 轮紧接着开始。
        assert!(
            !plane.convs[&1].pending.is_empty(),
            "前提：块确实还没冲出去"
        );
        let frames = pump_events(
            &mut plane,
            now,
            &[(
                runner,
                2,
                RunnerEvent::Block {
                    turn: 2,
                    entry: text_block(0, "第二轮"),
                },
            )],
        );
        let delta_turns: Vec<TurnNo> = frames
            .iter()
            .filter_map(|f| match f {
                LlmFrame::Delta { turn, .. } => Some(*turn),
                _ => None,
            })
            .collect();
        assert_eq!(
            delta_turns,
            vec![1],
            "换轮时冲出去的那一帧必须仍标第 1 轮，实际 {frames:?}"
        );
    }

    #[test]
    fn 片4_已退出的对话被回收_否则并发名额永久烧掉() {
        // 与片 2 §6.9 缺陷 ⑤ 同形：只增不减的注册表 + 按规模判上限 =
        // 「N 次崩溃之后永久无法再起新会话，重启才能恢复」。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        for id in 1..=LLM_MAX_CONVS as u64 {
            seed_conv(&mut plane, id, 3);
            let conv = plane.convs.get_mut(&id).expect("conv");
            conv.meta.state = LlmConvState::Exited;
            // 回收的三个判据之一就是「没人订阅」（还订着说明可能有人来补看最后的错误）。
            // `seed_conv` 默认订阅，这里显式退订才是本测试要覆盖的那个形态。
            conv.subscribed = false;
        }
        assert_eq!(plane.live_convs(), 0, "已退出的不占并发名额");
        // 真 runner 一个都不存在（`manager()` 是空的）⇒ 全部可回收。
        let _ = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            plane.convs.is_empty() && plane.by_runner.is_empty(),
            "管理器已收走 runner 的对话条目必须跟着回收"
        );
    }

    #[test]
    fn 片4_订阅中的已退出对话不被回收() {
        // 手机正停在这个对话页上等着看「为什么挂了」，这时候把记录扔了 = 它永远看不到。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            conv.meta.state = LlmConvState::Exited;
            conv.subscribed = true;
        }
        let _ = pump(&mut plane, Some(Role::Controlled), now);
        assert!(plane.convs.contains_key(&1));
    }

    // ── 单帧字节上限（拆帧）────────────────────────────────────────────────

    #[test]
    fn 片4_大批增量必须拆成多帧且每帧不超过单帧上限() {
        // 老实现 `mem::take` 整池打一帧 ⇒ 单帧无上限 ⇒ 顶穿服务端 4 MiB 单帧上限，
        // 整条 WS 被打断（终端镜像一起断）。
        let mut plane = LlmPlane::default();
        seed_conv(&mut plane, 1, 3);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            // 200 个 32 KiB 的工具结果 —— 协议注释自己写着「一轮 128 次工具调用对
            // agentic 会话是常态」，且单个 output 已按 LLM_TOOL_OUTPUT_MAX_BYTES 夹过。
            let payload = "z".repeat(LLM_TOOL_OUTPUT_MAX_BYTES);
            for i in 0..200u32 {
                let item = LlmDeltaItem::BlockStart {
                    entry: LlmBlockEntry {
                        block_id: i,
                        parent_call_id: None,
                        block: LlmBlock::ToolResult {
                            call_id: format!("c{i}"),
                            status: LlmToolStatus::Ok,
                            output: LlmText::new(&payload),
                            truncated_bytes: None,
                            detail: None,
                        },
                    },
                };
                conv.pending_bytes += delta_item_bytes(&item);
                conv.pending.push(PendingItem {
                    item,
                    tseq: u64::from(i) + 1,
                });
            }
            assert!(
                conv.pending_bytes > LLM_DELTA_MAX_BYTES * 4,
                "前提：这批增量必须显著超过单帧上限，否则本测试测不到拆帧"
            );
        }

        plane.flush_conv(1);
        let frames = take_frames(&mut plane);
        assert!(frames.len() > 1, "必须拆成多帧，实际 {} 帧", frames.len());

        let mut seqs = Vec::new();
        for frame in &frames {
            let LlmFrame::Delta { seq, items, .. } = frame else {
                panic!("只该产出 Delta，实际 {frame:?}");
            };
            let bytes: usize = items.iter().map(delta_item_bytes).sum();
            assert!(
                bytes <= LLM_DELTA_MAX_BYTES,
                "单帧 {bytes} 字节已超 LLM_DELTA_MAX_BYTES={LLM_DELTA_MAX_BYTES}"
            );
            seqs.push(*seq);
        }
        // seq 必须严格递增且**连续**——断裂即触发控制端整轮重拉。
        assert_eq!(
            seqs,
            (1..=seqs.len() as u64).collect::<Vec<_>>(),
            "拆出来的每帧各领一个连续 seq"
        );
        // 窗口封顶：最多 LLM_DELTA_WINDOW 帧，剩下的留在 pending 等 ACK。
        assert!(seqs.len() <= LLM_DELTA_WINDOW as usize);
        assert_eq!(
            plane.convs[&1].ack_map.len(),
            seqs.len(),
            "每帧各在 ack_map 里占一格（占位补号机制已删除）"
        );
        assert!(
            plane.convs[&1]
                .ack_map
                .iter()
                .all(|(proto, tseq)| *proto != 0 && *tseq != 0),
            "每格都要带真实的 (协议 seq, transcript seq)，否则 ACK 回来销不掉 transcript"
        );
    }

    #[test]
    fn 片4_单项自身超限时仍单独成帧_不得死循环() {
        // 取不出批 = `flush_conv` 的循环既清不掉 pending 也发不出帧 ⇒ 挂死主线程。
        let mut plane = LlmPlane::default();
        seed_conv(&mut plane, 1, 3);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            let item = LlmDeltaItem::TextAppend {
                block_id: 0,
                text: LlmText::new("x".repeat(LLM_DELTA_MAX_BYTES * 2)),
            };
            conv.pending_bytes = delta_item_bytes(&item);
            conv.pending.push(PendingItem { item, tseq: 1 });
        }
        plane.flush_conv(1);
        assert_eq!(take_frames(&mut plane).len(), 1);
        assert!(plane.convs[&1].pending.is_empty());
    }

    #[test]
    fn 片4_积压上限必须明显低于服务端单帧上限() {
        // 两个上限取值相等是个陷阱：「刚好合规的积压」ACK 一到就长成「超限的帧」。
        // 服务端 `MAX_WS_MESSAGE` 是 4 MiB（server/lumen-server/src/ws.rs）。
        const SERVER_MAX_WS_MESSAGE: usize = 4 * 1024 * 1024;
        // `const {}` 而不是运行期 `assert!`：这三个都是编译期常量，写成 const 块之后
        // 调错值连**编译**都过不去，比「跑一次测试才发现」更早一步。
        const {
            assert!(
                LLM_BACKLOG_MAX_BYTES < SERVER_MAX_WS_MESSAGE,
                "积压上限不得达到服务端单帧上限：两个上限取值相等时，\
                 「刚好合规的积压」ACK 一到就长成「超限的帧」"
            );
            assert!(
                LLM_DELTA_MAX_BYTES < LLM_BACKLOG_MAX_BYTES,
                "单帧上限必须小于积压上限，否则拆帧这道防线形同虚设"
            );
        }
    }

    // ── 出站闸门：无会话 / 未订阅 ──────────────────────────────────────────

    #[test]
    fn 片4_无会话时runner事件只进轮记录不产出任何帧() {
        // `PumpCtx::role` 的文档写死「None = 无会话，此时只做清理，不产出任何帧」。
        // 但事件**仍然要吃**——§6.7 硬契约是「手机断线 runner 照跑、内容继续累积」。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        plane.on_session_ended(); // 清订阅态（模拟会话断开）
        let _ = take_frames(&mut plane);

        let runner = RunnerId(1);
        let events = vec![
            (
                runner,
                1u64,
                RunnerEvent::Block {
                    turn: 1,
                    entry: text_block(0, "断线期间产出的内容"),
                },
            ),
            (
                runner,
                2u64,
                RunnerEvent::TurnEnded(Box::new(TurnEndedInfo {
                    turn: 1,
                    stop: LlmStopReason::EndTurn,
                    outcome: LlmTurnOutcome::default(),
                    usage: LlmUsage::default(),
                    denials: 0,
                    result_text: None,
                    result_truncated_bytes: None,
                })),
            ),
        ];
        let mut runners = manager();
        let mut req_seq = 0u64;
        let mut ctx = PumpCtx {
            now,
            role: None,
            runners: &mut runners,
            events: &events,
            req_seq: &mut req_seq,
        };
        plane.pump(&mut ctx);

        assert!(
            take_frames(&mut plane).is_empty(),
            "无会话时一帧都不该产出（服务端 relay 查不到会话只会丢弃，白烧带宽还把 seq 顶上去）"
        );
        assert_eq!(plane.convs[&1].seq, 0, "没人在听就不该消费 seq");
        assert!(plane.convs[&1].pending.is_empty(), "不该占内存");
        assert_eq!(
            plane.convs[&1].turns.len(),
            1,
            "内容必须照常进轮记录 —— 断线不杀进程、产出不能丢（§6.7）"
        );
        assert!(plane.convs[&1].resync_due, "跳过的实时流必须记账");
    }

    #[test]
    fn 片4_detach之后不再产出实时流() {
        // 协议里 Detach 就是「取消订阅」。老实现只把 runner 的 subscribers 减一，实时流照发。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        plane.enqueue(
            LlmFrame::Detach {
                conv_id: 1,
                conv_generation: 3,
            },
            now,
        );
        let _ = pump(&mut plane, Some(Role::Controlled), now);
        assert!(!plane.convs[&1].subscribed);

        let frames = pump_events(
            &mut plane,
            now,
            &[(
                RunnerId(1),
                1,
                RunnerEvent::Block {
                    turn: 1,
                    entry: text_block(0, "取消订阅之后的内容"),
                },
            )],
        );
        assert!(
            frames.is_empty(),
            "Detach 之后不得再发任何实时流帧，实际 {frames:?}"
        );
        assert_eq!(plane.convs[&1].seq, 0);
        assert!(plane.convs[&1].resync_due);
    }

    // ── 附件路径围栏（白名单严出的反方向）──────────────────────────────────

    #[test]
    fn 片4_附件路径必须落在本对话的暂存目录内() {
        let dir = std::env::temp_dir().join("lumen_llm_attach_fence_test");
        std::fs::create_dir_all(&dir).expect("建测试目录");
        let inside = dir.join("photo.png");
        std::fs::write(&inside, b"x").expect("写测试附件");
        let attach_dir = LlmPath::new(dir.display().to_string());

        let ok = [lumen_protocol::llm::LlmAttachment {
            path: LlmPath::new(inside.display().to_string()),
            name: LlmText::new("photo.png"),
            mime: "image/png".to_owned(),
            len: 1,
        }];
        assert!(
            !attachments_outside_fence(&attach_dir, &ok),
            "暂存目录内的附件必须放行"
        );

        // 目录穿越：`<attach_dir>\..\` 出去之后 canonicalize 会解掉 `..`，前缀判据立刻不成立。
        let outside = dir
            .join("..")
            .join("lumen_llm_attach_fence_test_outside.txt");
        std::fs::write(&outside, b"secret").expect("写越界文件");
        let bad = [lumen_protocol::llm::LlmAttachment {
            path: LlmPath::new(outside.display().to_string()),
            name: LlmText::new("photo.png"),
            mime: "image/png".to_owned(),
            len: 1,
        }];
        assert!(
            attachments_outside_fence(&attach_dir, &bad),
            "目录穿越必须被挡下：否则控制端能点名让 PC 读任意本机文件，内容再经 assistant 块回流"
        );

        // 不存在的路径：证明不了在围栏内 ⇒ 判越界（「证明不了就放行」是围栏最常见的失效方式）。
        let ghost = [lumen_protocol::llm::LlmAttachment {
            path: LlmPath::new(dir.join("不存在.png").display().to_string()),
            name: LlmText::new("photo.png"),
            mime: "image/png".to_owned(),
            len: 1,
        }];
        assert!(attachments_outside_fence(&attach_dir, &ghost));

        // 没有附件时不做任何 IO，恒放行。
        assert!(!attachments_outside_fence(&attach_dir, &[]));

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 片4_越界附件让整条send被拒而不是静默剔除() {
        // 静默剔除会让手机以为图片发出去了，模型却根本没看见 —— 一个不报错、
        // 只会让对话答非所问的 bug。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        plane.enqueue(
            LlmFrame::Send {
                conv_id: 1,
                conv_generation: 3,
                req_id: 7,
                text: LlmText::new("看看这张图"),
                attachments: vec![lumen_protocol::llm::LlmAttachment {
                    path: LlmPath::new("C:\\Users\\someone\\.ssh\\id_rsa"),
                    name: LlmText::new("id_rsa"),
                    mime: "application/octet-stream".to_owned(),
                    len: 1,
                }],
            },
            now,
        );
        let frames = pump(&mut plane, Some(Role::Controlled), now);
        assert!(
            matches!(
                frames.as_slice(),
                [LlmFrame::Error {
                    code: LlmErrorCode::CwdInvalid,
                    req_id: Some(7),
                    ..
                }]
            ),
            "越界附件必须整条拒绝并带 req_id，实际 {frames:?}"
        );
    }

    // ── 链路翻转不重复弹「对端太老」──────────────────────────────────────

    #[test]
    fn 片4_判定对端太老之后链路翻转不再重复提示() {
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        plane.on_session_started(Role::Controller, t0);
        let _ = take_frames(&mut plane);
        assert!(plane.tick(t0 + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS)));
        assert!(plane.take_peer_too_old(), "第一次必须提示");

        for i in 1..=2u64 {
            let at = t0 + Duration::from_secs(10 * i);
            plane.on_link_switched(Role::Controller, at);
            assert!(
                matches!(take_frames(&mut plane).as_slice(), [LlmFrame::Hello { .. }]),
                "翻转仍要补发 Hello（万一对端真升级了）"
            );
            assert!(
                plane.hello_deadline().is_none(),
                "已判定 Unsupported 之后不得重开 5 秒计时"
            );
            assert!(
                !plane.tick(at + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS + 1)),
                "不得再次判超时"
            );
            assert!(
                !plane.take_peer_too_old(),
                "链路抖动不该变成周期性弹窗（第 {i} 次翻转）"
            );
        }
    }

    #[test]
    fn 片4_窗口满时合并窗口往后推_不留过去时刻空转() {
        // `next_wake` 返回过去时刻 ⇒ `tick_llm` 排 1 ms 重绘 ⇒ 事件循环 1 kHz 空转。
        let mut plane = LlmPlane::default();
        let now = Instant::now();
        seed_conv(&mut plane, 1, 3);
        {
            let conv = plane.convs.get_mut(&1).expect("conv");
            conv.inflight = LLM_DELTA_WINDOW; // 窗口已满
            conv.pending.push(PendingItem {
                item: LlmDeltaItem::TextAppend {
                    block_id: 0,
                    text: LlmText::new("x"),
                },
                tseq: 1,
            });
            conv.pending_bytes = 1;
            conv.flush_at = Some(now - Duration::from_secs(1)); // 早就到期了
        }
        let _ = pump(&mut plane, Some(Role::Controlled), now);
        let next = plane.next_wake().expect("仍有待发增量");
        assert!(
            next > now,
            "窗口满时必须把合并窗口往后推，实际 {next:?} <= {now:?}"
        );
        assert!(
            !plane.convs[&1].pending.is_empty(),
            "积压不丢，等 ACK 回来再发"
        );
    }

    #[test]
    fn 片4_next_wake取握手与合并窗口的较早者() {
        // `ControlFlow::Wait` 下这两件事都是纯计时驱动的，漏排唤醒 = 增量在本地压到下一次
        // 外部事件（最坏 25 秒一次的 WS 保活）。
        let mut plane = LlmPlane::default();
        let t0 = Instant::now();
        assert!(plane.next_wake().is_none(), "无事可等时不排唤醒");

        plane.on_session_started(Role::Controller, t0);
        let hello_at = t0 + Duration::from_secs(LLM_HELLO_TIMEOUT_SECS);
        assert_eq!(plane.next_wake(), Some(hello_at));

        seed_conv(&mut plane, 1, 3);
        let flush_at = t0 + Duration::from_millis(LLM_DELTA_FLUSH_MS);
        plane.convs.get_mut(&1).expect("conv").flush_at = Some(flush_at);
        assert_eq!(plane.next_wake(), Some(flush_at), "取较早者");
    }

    #[test]
    fn 片4_轮号溢出饱和而不是回绕() {
        assert_eq!(clamp_turn(7), 7);
        assert_eq!(clamp_turn(u64::from(u32::MAX) + 5), u32::MAX);
    }

    #[test]
    fn 片4_对话代号非零且留在i64范围内() {
        for _ in 0..64 {
            let g = random_generation();
            assert!(g != 0, "0 是哨兵");
            assert!(g <= i64::MAX as u64, "Dart 的 int 是有符号 64 位");
        }
    }

    #[test]
    fn 片4_cli会话id是合规uuid_v4() {
        let id = new_cli_session_id();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version 4");
        assert!(matches!(&parts[3][0..1], "8" | "9" | "a" | "b"), "variant");
        assert_ne!(id, new_cli_session_id(), "两次不得相同");
    }

    #[test]
    fn 片4_req_id分配跳过0哨兵() {
        let mut seq = u64::MAX - 1;
        assert_eq!(next_req_id(&mut seq), u64::MAX);
        assert_eq!(next_req_id(&mut seq), 1, "回绕必须跳过 0 哨兵");
    }

    #[test]
    fn 片4_用量累加token但上下文占用取最新() {
        let mut total = LlmUsage {
            input_tokens: 10,
            output_tokens: 20,
            context_used: 5_000,
            context_limit: 200_000,
            ..LlmUsage::default()
        };
        accumulate_usage(
            &mut total,
            &LlmUsage {
                input_tokens: 3,
                output_tokens: 4,
                context_used: 9_000,
                context_limit: 200_000,
                ..LlmUsage::default()
            },
        );
        assert_eq!((total.input_tokens, total.output_tokens), (13, 24));
        assert_eq!(
            total.context_used, 9_000,
            "上下文占用是当前值，累加会得到一个随轮数线性增长的假数字"
        );
    }

    #[test]
    fn 片4_标题取首行且按字符夹紧() {
        let title = title_of("重构渲染器\n第二行不该进标题");
        assert_eq!(title.as_str(), "重构渲染器");
        let long = "阿".repeat(TITLE_MAX_CHARS + 20);
        assert_eq!(
            title_of(&long).as_str().chars().count(),
            TITLE_MAX_CHARS,
            "按字符夹紧（按字节切会在多字节字符上 panic）"
        );
        assert_eq!(title_of("   ").as_str(), "新对话");
    }

    // ── 片 7 回放语料生成器 ─────────────────────────────────────────────────

    /// 把两份 Claude CLI 的**原始 stream-json** 采样跑完整条 PC 侧链路，把真正会过网的
    /// [`LlmFrame`] 逐帧写成 JSONL，供 M7 片 7 的手机端离线回放用。
    ///
    /// # 为什么必须生成，而不能直接拿样本当语料
    /// 样本是**归一化的输入**，不是输出：它的每一行是 `{"type":"assistant",…}` 这种 CLI
    /// 私有形状，而手机端 Dart 侧解的是内部标签为 `"op"` 的 [`LlmFrame`]。把样本直接喂给
    /// Dart，每一行都会**静默**落到 `LlmUnknown` 兜底变体上——测试全绿、画面全空。
    /// 交接文档里「回放语料直接用样本 B」那句话按字面做必然是这个下场。
    ///
    /// # 走的是哪条路径
    /// 与线上「读线程 + 泵」**逐段同构**：
    /// `event::classify`（白名单严入）→ `LlmAgentDecoder::decode_line`（片 3 归一化）
    /// → [`LlmPlane::pump`]（片 4 的轮号 / seq / 合并窗口 / 白名单严出）→ 出站帧。
    /// 前两段抄的是 `llm_runner::decode_chunk` 的处置（那是私有函数），第三段就是生产代码本身。
    ///
    /// # 私人内容为何不会进语料
    /// hook 行（`system/hook_started` / `system/hook_response`）在**第一段**就被白名单挡下，
    /// 只产出 `RunnerEvent::LineDropped { tag }`（只留标签、不留内容），而 `LineDropped`
    /// 在片 4 里只累加一个计数器、不产帧。`system/init` 的 `cwd` / `plugins` / `skills` /
    /// `slash_commands` 同理：`on_started` 只把 model / cli_session_id / permission_mode
    /// 三项写进 `Conv::meta`，而 `meta` 只随 `ConvStarted` / `HelloAck` 过网，不进本文件。
    /// [`回放泄漏自查`] 把这条不变量钉死——出了任何一个禁用键或私人内容标记就地失败，不写文件。
    ///
    /// 跑法：`cargo test -p lumen-app -- --ignored 生成片7回放语料 --nocapture`
    #[test]
    #[ignore = "手动运行：产出片 7 的回放语料"]
    fn 生成片7回放语料() {
        /// crate 内的那两份副本（与 `docs/调研/` 下的原件逐字节相同）。**不读 `docs/`**：
        /// 让测试依赖文档目录会在有人整理文档时无声变红。
        const 样本A: &str =
            include_str!("../llm_runner/fixtures/claude-stream-json-sample-a-2026-08-08.jsonl");
        const 样本B: &str = include_str!(
            "../llm_runner/fixtures/claude-stream-json-sample-b-tooluse-2026-08-08.jsonl"
        );

        let 目录 = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../lumen-protocol/tests/golden/mobile/replay");
        std::fs::create_dir_all(&目录).expect("建回放语料目录");

        for (样本, 文件名) in [
            (样本B, "sample_b_llmframes.jsonl"),
            (样本A, "sample_a_llmframes.jsonl"),
        ] {
            let frames = 回放一份样本(样本);
            let jsonl: String = frames
                .iter()
                .map(|f| serde_json::to_string(f).expect("帧必可序列化") + "\n")
                .collect();
            回放泄漏自查(&jsonl);

            let 路径 = 目录.join(文件名);
            std::fs::write(&路径, &jsonl).expect("写回放语料");

            let mut 直方图: BTreeMap<&'static str, usize> = BTreeMap::new();
            for f in &frames {
                *直方图.entry(frame_op_name(f)).or_default() += 1;
            }
            println!("{文件名}：{} 帧 {直方图:?}", frames.len());
        }
    }

    /// 一份样本 → 一串出站 [`LlmFrame`]。
    ///
    /// # 时钟为什么要一行推两拍
    /// 每行**先泵一次带事件的**（进合并窗口），再泵一次**空的**（窗口到期、冲出去）。
    /// [`LlmPlane::pump`] 的次序是「吃事件 → flush」，只泵一次的话下一行的事件会在同一次
    /// flush 前被吃进去，于是相邻几行的增量并成一条巨大的 `Delta`——回放出来看不到任何
    /// 流式过程，而片 7 要验的恰恰是流式渲染。33 ms 合并窗口本身是生产行为，这里只是把
    /// 「上游两行间隔 > 33 ms」这个常见情形喂给它，没有绕开任何一行代码。
    fn 回放一份样本(样本: &str) -> Vec<LlmFrame> {
        use crate::llm_runner::event::{classify, LineVerdict};

        let (mut encoder, mut decoder) = ClaudeAdapter::new().split();
        // **用真编码器推轮号**：`ClaudeDecoder` 的轮号来自与编码器共享的那个 `AtomicU64`，
        // 不发这一下，全部块都会挂在 turn 0 上（0 在协议里是「还没有轮」的形状）。
        // 这一步与 `LlmRunner::submit` 里的调用完全一致，只是我们不把 stdin 行真写出去。
        let _ = encoder
            .encode_user_message("把这份文件读一下", &[])
            .expect("编码用户消息");

        let mut plane = LlmPlane::default();
        seed_conv(&mut plane, 1, 1);
        let runner = RunnerId(1);
        let t0 = Instant::now();
        let mut tseq = 0u64;
        let mut frames: Vec<LlmFrame> = Vec::new();

        for (i, line) in 样本.lines().enumerate() {
            let mut events: Vec<RunnerEvent> = Vec::new();
            match classify(line) {
                LineVerdict::Blank => {}
                LineVerdict::Parse(env) => {
                    if let Err(err) = decoder.decode_line(&env, line, &mut events) {
                        events.push(RunnerEvent::ProtocolError {
                            kind: ProtocolErrorKind::DecodeFailed { at: err.at },
                        });
                    }
                }
                LineVerdict::DropSilently { tag } => {
                    events.push(RunnerEvent::LineDropped { tag });
                }
                LineVerdict::UnknownSchema { tag } => {
                    events.push(RunnerEvent::ProtocolError {
                        kind: ProtocolErrorKind::UnknownSchema { tag },
                    });
                }
                LineVerdict::Malformed { kind, bytes } => {
                    events.push(RunnerEvent::ProtocolError {
                        kind: ProtocolErrorKind::MalformedEnvelope { kind, bytes },
                    });
                }
            }
            let batch: Vec<(RunnerId, u64, RunnerEvent)> = events
                .into_iter()
                .map(|e| {
                    tseq += 1;
                    (runner, tseq, e)
                })
                .collect();
            let 本行 = t0 + Duration::from_millis(100 * (i as u64 + 1));
            frames.extend(pump_events(&mut plane, 本行, &batch));
            frames.extend(pump_events(
                &mut plane,
                本行 + Duration::from_millis(LLM_DELTA_FLUSH_MS + 1),
                &[],
            ));
        }
        frames
    }

    /// 回放语料的**泄漏自查**：出了任何一条就地 panic，绝不落盘。
    ///
    /// 断言在**序列化后的文本**上而不是帧的 `Debug` 上：`LlmPath` / `LlmText` 的 `Debug` 恒为
    /// `<redacted>`，脱敏包装保护的是日志、不是线格式（同 `claude.rs::started_json` 那段注释）。
    /// 真正过网的、也是真正会被入库的，是这份 JSON。
    ///
    /// ⚠ 两份 fixture 都是**脱敏版**：私人内容已被替换成占位符，所以本函数的强度上限是
    /// 「占位符没被透传」。它靠的是**占位符自带来源标记**这一点：hook 与 init 的占位符写作
    /// `<<REDACTED stdout: 4797 chars of local hook output>>` / `<<REDACTED local env: …>>`，
    /// 而合法转发的工具结果占位符是 `<<REDACTED 12272 chars>>`——前者的来源标记一旦出现在
    /// 产物里，就说明白名单漏了一整个字段，而不只是漏了一个值。
    fn 回放泄漏自查(jsonl: &str) {
        // 一、只存在于 CLI 原始形状、绝不该出现在 `LlmFrame` 里的键。
        for 禁用键 in [
            "\"stdout\"",
            "\"stderr\"",
            "\"cwd\"",
            "\"plugins\"",
            "\"skills\"",
            "\"tools\"",
            "\"agents\"",
            "\"slash_commands\"",
            "\"memory_paths\"",
            "\"mcp_servers\"",
            "\"hook_id\"",
            "\"hook_name\"",
            "\"hook_event\"",
            "\"apiKeySource\"",
            "\"permissionMode\"",
            "\"claude_code_version\"",
            "\"session_id\"",
            "\"request_id\"",
            "\"parent_tool_use_id\"",
            "\"signature\"",
            "\"uuid\"",
        ] {
            assert!(
                !jsonl.contains(禁用键),
                "回放语料里出现了 {禁用键}——白名单漏了一整个字段，绝不能入库"
            );
        }
        // 二、私人内容的来源标记（占位符里那半句话），以及 hook 行独有的标识。
        for 私人标记 in [
            "of local hook output",
            "local env:",
            "SessionStart",
            "hook_response",
            "hook_started",
        ] {
            assert!(
                !jsonl.contains(私人标记),
                "回放语料里出现了私人内容标记 {私人标记:?}——绝不能入库"
            );
        }
    }
}
