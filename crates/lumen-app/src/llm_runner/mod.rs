//! M7 片 2：**PC 端 headless LLM runner 的进程与解析底座**。
//!
//! 本模块管理一组「无 PTY 的 LLM CLI 子进程」，把它们的 stdout 事件流解析成
//! [`event::RunnerEvent`]，供片 4 拼成 `lumen_protocol::llm::LlmFrame` 上行给手机。
//! 桌面端**不做会话 UI**（M7 §12-⑫ 拍板），只在工具栏留一个图标 + 弹层用到这里的计数。
//!
//! ```text
//! llm_runner/
//!   mod.rs      ← 你在这里：容器、线程与通道拓扑、生命周期、环形缓冲、退出归因
//!   proc.rs     子进程（CREATE_NO_WINDOW / Job Object 进程树清理）
//!   lines.rs    增量行切分（不用 BufReader::lines，理由见该文件）
//!   event.rs    信封解析 + 白名单判定 + 归一化事件词表
//!   adapter.rs  多 CLI 适配接口（stdin 铁律的类型落点在这里）
//!   claude.rs   ← 片 3：Claude 分支的 Encoder/Decoder 实现 + §6.6 映射表的可执行副本
//!   fixtures/   片 3 的样本驱动测试语料（`docs/调研/` 下两份实测样本的逐字节副本）
//! ```
//!
//! # 一、落点：与 `tabs` 平行的独立容器（设计蓝图 §6.1）
//!
//! [`LlmRunnerManager`] 挂在 `AppState.llm_runners`，与 `tabs: Vec<Tab>` **平级**，
//! 对齐 `ssh_runtime.rs:641-649` 的 `SshRuntime` 范式。**绝不塞进 `Tab.panes`**——
//! 逐条硬伤都有代码依据：
//!
//! | 硬伤 | 代码依据 | 后果 |
//! |---|---|---|
//! | `panes` 恒非空不变量 | `session.rs:58-59` + `main.rs:7699-7701` | 关掉这个 LLM 对话会连带关掉整个 tab |
//! | `respawn_pane` 自动重起 shell | `main.rs:7746-7788` | LLM 进程退出被误判成 shell 退出，原位重起一个 PowerShell |
//! | 主循环只 drain `tabs` 内通道 | `main.rs:10127` | 挂在 tabs 之外的会话拿不到泵，也拿不到唤醒 |
//! | 渲染资源按 `SessionId` 挂账 | `main.rs:1233` / `main.rs:7635-7640` | 没有终端网格的会话会进入离屏纹理分配路径 |
//! | `Session` 构造不出「没有 PTY」的实例 | `session.rs:433` 第一件事就是 `PtySession::spawn` | 改成 `Option` 要动 6+ 处调用点 |
//! | 持久化会把它还原成一个 shell 窗格 | `sessions_store.rs:46-53` + `main.rs:9598` | 重启后变出一个终端 |
//! | 忙闲判定建立在终端协议信号上 | `session.rs:582-587`（OSC 9;4 / DEC 2026） | 对 headless 恒 false |
//!
//! 同理 [`RunnerId`] 是**独立 id 空间**，绝不与 `session::SessionId` / `TabId` 复用——
//! 复用会让 `pane_textures` / `release_pane_resources` 等按 `SessionId` 挂账的路径误伤。
//!
//! # 二、线程与通道拓扑（§6.2）
//!
//! ```text
//!    [主线程 / winit + egui]                              [子进程 claude.exe]
//!         │                                                      ▲
//!         │ submit()/interrupt() ──► in_tx ──► lumen-llm-in-N ────┘ stdin  (整行原子写)
//!         │  (主线程侧 try_send，  (crossbeam                写线程**独占** ChildStdin
//!         │   满则 Backlogged)      bounded 32)             —— §6.4 铁律 3 的执行点
//!         │                                                      │
//!         │ pump() ◄──────────────── ev_rx ◄── lumen-llm-out-N ◄──┘ stdout (LineSplitter)
//!         │   每帧 drain          (crossbeam bounded 4096，      读线程
//!         │                        满则**先唤醒再阻塞** = 背压)
//!         │                                                      │
//!         │ stderr_tail (Arc<Mutex>) ◄──────── lumen-llm-err-N ◄─┘ stderr (只留最后 64 行)
//!         │
//!         └─ try_wait() 每帧非阻塞收尸 → RunnerEvent::Exited
//! ```
//!
//! - **唤醒**：三个线程各持一个 [`Waker`]（`EventLoopProxy` + 全局 `wake_pending` 的成对封装），
//!   `swap(true, AcqRel)` 去重后 `proxy.send_event(PtyWake)`，与 `session.rs:470-474` /
//!   `remote_ws.rs` 的 `nudge` 共用**同一个全局标志**。
//!   `main.rs:374` 设了 `ControlFlow::Wait`，此时 `ctx.request_repaint()` **叫不醒空闲事件
//!   循环**（`remote.rs:109-111` 与 `p2p.rs:149-152` 两处注释都是踩坑记录）。
//! - **背压路径上也必须有唤醒**（对抗验证抓到的自锁，见 [`flush_events`]）：事件通道满时
//!   阻塞读线程是**故意的**，但阻塞**之前**必须先发一次唤醒，否则读线程等主线程来取、
//!   主线程等 `PtyWake` 才跑泵，两边互等。
//! - **泵的插入点**：`main.rs` 的 `state.pump_remote()` 旁边（在 `wake_pending.store(false)`
//!   **之后**，无丢唤醒），且**必须在 `if let Some(sub_id) = self.remote_ws.sub_target()`
//!   那层嵌套之外**——headless 会话与手机订阅哪个 tab 完全无关。
//! - **通道选型**：crossbeam（与 `completion_sidecar.rs:19` / `session.rs:18` 一致）。
//!   仓库并非只有 crossbeam——`remote_ws.rs:26` 用的是 `std::sync::mpsc`；这里选 crossbeam
//!   是因为要有界通道（背压）与 `is_empty()`。
//! - **为什么不设 wait 线程**：`Child::wait` 要 `&mut self` 且阻塞，与 `kill` 争同一个
//!   `Child`。`lumen-pty` 能用独立 wait 线程（`lib.rs:177-184`）是因为它把 `Child` 整个搬进了
//!   线程；这里主线程要读退出码做归因，`Child` 必须留在管理器里，故每帧 `try_wait()`。
//! - **stderr 为什么不走 `ev_rx`（本模块相对 §6.2 图的唯一偏离）**：stderr 里是 CLI 的诊断
//!   输出，可能带本机路径与内部堆栈。走 `ev_rx` 就会进 [`Transcript`] 环形缓冲，而那个缓冲
//!   是**手机重连补发的数据源**——等于给白名单开了一个后门。故 stderr 只落本地
//!   `Arc<Mutex<`[`StderrTail`]`>>`，供退出归因与本地审计日志使用，不进 transcript。
//!
//! # 三、生命周期：与手机会话完全解耦（§6.7 跨组件硬契约）
//!
//! ```text
//! 手机进后台 / 隧道断 ─► 服务端 teardown_session ─► PC 收 SessionEnded
//!      │
//!      ├─ ❌ 绝不能：杀掉 headless 子进程
//!      └─ ✅ 必须：runner 继续跑，事件写进带 seq 的环形缓冲
//! 手机回前台 ─► WS 重连 ─► Attach{conv_id, known_seq} ─► 基线 + 补发 seq 区间 ─► 恢复实时
//! ```
//!
//! **为什么这条是硬契约**：一个 turn 可以跑几分钟，而服务端 `hub::relay` 在无活跃会话时
//! **静默丢弃**（`hub.rs:589-591`），断线期间的事件发出去就没了。故每个 runner 自带**有上限**的
//! 本地 transcript 环形缓冲，重新订阅时按 `since_seq` 补发；**超上限要标记
//! [`Transcript::truncated_from`] 而不是假装完整**。
//!
//! 其余生命周期决策：
//!
//! | 项 | 决策 | 理由 |
//! |---|---|---|
//! | CLI 会话 id | Lumen 生成 uuid 用 `--session-id` 传入 | 映射在进程启动**前**成立，手机可立刻寻址、崩溃后可确定性 `--resume` |
//! | 并发上限 | [`MAX_HEADLESS_RUNNERS`] `= 4` | 对齐既有有界纪律（`remote_ws.rs:75/95/97`）。每个 claude 是数百 MB RSS 的完整进程，超限**显式拒绝**而非静默排队 |
//! | 空闲回收 | 无订阅**且**无进行中 turn 满 [`RUNNER_IDLE_TIMEOUT`] | **turn 中途绝不杀** |
//! | 优雅停止 | 先关 stdin（CLI 见 EOF 自然退出），宽限 [`RUNNER_STOP_GRACE`] 后 `TerminateJobObject` | — |
//! | transcript 上限 | [`TRANSCRIPT_MAX_EVENTS`] / [`TRANSCRIPT_MAX_BYTES`] | 4 runner × 16 MiB = 64 MiB 常驻上限 |
//!
//! # 四、退出归因：**退出码 1 不足以判因**（§6.6 表末行，实测）
//!
//! 实测认证失败时 `exit code = 1` 且 **stderr 完全为空**——失败信息只在 stdout 事件流里的
//! 最后一条 `result`（`is_error = true` / `terminal_reason = "api_error"` /
//! `api_error_status = 403`）。runner **不能只看 stderr 判因**，否则用户拿到「进程挂了，
//! 没有任何原因」。这条判定写在 [`attribute_exit`] 里，并有单测覆盖每一条分支。
//!
//! # 五、本模块依赖的两条仓库事实（有人破坏它们，进程树清理会静默失效）
//!
//! [`LlmRunnerManager`] 的 `Drop` 是正常退出路径上的清理点。它能跑到，依赖：
//! **lumen-app 全 crate 无 `std::process::exit`**，且 `Cargo.toml` 无 `panic = "abort"`
//! （`App` 持 `Option<AppState>`，`main` 结束时 drop）。仓库**不存在统一 shutdown hook**
//! ——`main.rs` 共 11 处 `event_loop.exit()`，只有 4 处相邻调用了 `history.flush_on_exit()`，
//! 所以这里刻意不去挂钩那些点，只靠 `Drop`。将来任何人加一处 `process::exit` 就会静默漏掉
//! 整棵 claude 进程树：Windows 侧只剩 Job Object 的 `KILL_ON_JOB_CLOSE` 兜底，
//! unix 侧直接漏孤儿。

pub mod adapter;
/// 片 3：Claude Code 的 stream-json ↔ 归一化模型（§6.6 映射表的唯一可执行副本）。
pub mod claude;
pub mod event;
pub mod lines;
pub mod proc;

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use lumen_protocol::llm::{LlmAgentFeatures, LlmAgentKind, LlmAttachment, LlmTurnOutcome};
use winit::event_loop::EventLoopProxy;

use crate::PtyWake;

use adapter::{ControlCmd, LaunchSpec, LlmAgentAdapter, LlmAgentEncoder, StdinLine};
use event::{ProtocolErrorKind, RunnerEvent, UnknownEventTally};
use lines::{Chunk, LineSplitter};
use proc::{ChildProc, TryWaitOutcome};

// ── 常量 ──────────────────────────────────────────────────────────────────────

/// headless runner 并发上限。
///
/// 对齐既有有界纪律（`DIR_SERVICE_WORKERS=4` / `EDIT_MAX_INFLIGHT=4` /
/// `DOWNLOAD_MAX_FILES=4`，`remote_ws.rs:75/95/97`）。每个 claude 是数百 MB RSS 的完整
/// node 进程，**超限必须显式拒绝**（[`RunnerError::LimitReached`]）而不是静默排队——
/// 静默排队会让手机端看到一个永远"启动中"的对话。
pub const MAX_HEADLESS_RUNNERS: usize = 4;

/// 单行字节上限。超限进入丢弃态并报 [`ProtocolErrorKind::OverlongLine`]，
/// **不杀会话、不截断喂 serde**（见 `lines.rs` 的"被否决的替代方案"）。
///
/// 32 MiB 看着大，但它是**上限不是常态**：稳态缓冲只有当前未完成行的长度（几百字节到几 KB）。
/// 定这么高是因为一次大文件 `Read` 的 `tool_result` 行实测可到 MB 级，定低了会把**合法**的
/// 工具结果整行丢掉——那是静默的功能缺失，比多占一段峰值内存糟糕得多。
/// 最坏情形 4 runner × 32 MiB = 128 MiB 峰值，且丢弃时立刻 `Vec::new()` 还给分配器。
pub const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

/// stderr 单行上限。stderr 是诊断输出，没有 MB 级正文的道理；定小一点避免一条畸形输出
/// 把内存吃掉。
pub const STDERR_MAX_LINE_BYTES: usize = 64 * 1024;

/// stderr 只保留最后这么多行（[`StderrTail`]）。
pub const STDERR_TAIL_LINES: usize = 64;

/// 主线程 ← 读线程 的事件通道容量。
///
/// 满则**阻塞读线程** → OS 管道回压 → CLI 自然减速。这是刻意的：丢事件会让手机端的
/// `seq` 出现缺口并触发整轮重拉，比让 CLI 慢一点糟糕得多。
pub const RUNNER_EVENT_CAP: usize = 4096;

/// 主线程 → 写线程 的 stdin 队列容量。
///
/// **不能无界**：这条队列的投喂方是**手机端**（片 4 的 `Send` 帧），堆在里面的是完整的 prompt
/// 正文。CLI 处理慢或写线程卡住时，无界队列的内存增长没有上限、也没有任何计数——
/// 与本模块其余每一处上限（[`MAX_HEADLESS_RUNNERS`] / [`MAX_LINE_BYTES`] / [`RUNNER_EVENT_CAP`] /
/// [`TRANSCRIPT_MAX_EVENTS`]）的纪律不一致。
///
/// 32 是「一个会话同时排队 32 条用户消息已经不正常」的量级。满则
/// [`RunnerError::Backlogged`] **显式拒绝**，与 [`RunnerError::LimitReached`] 同一套路
/// （宁可让手机端看到一条明确的拒绝，也不要静默排队）。
///
/// **绝不能在主线程上用阻塞 `send`**：那会让一个卡住的 CLI 直接冻住整个 UI 事件循环。
/// 故 [`LlmRunner::submit`] / [`LlmRunner::control`] / [`LlmRunner::begin_stop`] 一律 `try_send`。
pub const RUNNER_STDIN_CAP: usize = 32;

/// 读线程每次 `read()` 的块大小。
pub const READ_CHUNK_BYTES: usize = 64 * 1024;

/// 每 runner 本地 transcript 缓冲上限（手机断线期间的事件在此排队等重订阅补发）。
pub const TRANSCRIPT_MAX_EVENTS: usize = 20_000;
/// 同上，字节预算。4 runner × 16 MiB = 64 MiB 常驻上限。
pub const TRANSCRIPT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// 空闲回收：无订阅**且**无进行中 turn 达此时长后优雅停止。**turn 进行中永不回收。**
pub const RUNNER_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// 停止时先关 stdin 让 CLI 自然退出，超过此宽限期再 `TerminateJobObject`。
pub const RUNNER_STOP_GRACE: Duration = Duration::from_secs(3);

/// 进程已退出、但读线程还没读完管道里残留数据时的等待上限。
///
/// 超过就直接报 [`RunnerEvent::Exited`]。没有这个上限，一个卡死的读线程会让 runner 永远停在
/// "已退出但没报出去"的半状态里，既不回收也不告诉手机。
pub const EXIT_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// 已退出的 runner 在无人订阅时保留多久（供手机稍后重连补看最后的错误）。
pub const RUNNER_EXITED_RETENTION: Duration = Duration::from_secs(5 * 60);

/// CLI 判定我们喂进去的 stdin 行非法时打在 stderr 上的原文片段。
///
/// **实测**：`printf 'this is not json\n'` → 进程 exit 1 + stderr
/// `Error parsing streaming input line: this is not json: SyntaxError: …`。
/// 这是 [`ProtocolErrorKind::StdinRejected`] 的**唯一**判据（stdout 上没有任何对应事件）。
pub const STDIN_REJECT_MARKER: &str = "Error parsing streaming input line";

// ── 标识与状态 ────────────────────────────────────────────────────────────────

/// runner 标识。**独立 id 空间**，绝不与 `session::SessionId` / `TabId` 复用（见模块文档一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunnerId(pub u64);

impl std::fmt::Display for RunnerId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "runner#{}", self.0)
    }
}

/// runner 运行态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerState {
    /// 进程已 spawn，尚未收到 `system/init`。
    Booting,
    /// 已就绪，无进行中 turn。
    Idle,
    /// 有进行中 turn（已提交 user 消息、尚未收到 `result`）。
    Busy,
    /// 等待权限应答（Tier 1/2 生效后才会出现）。
    AwaitingPermission,
    /// 进程已退出。`code = None` 表示被我们 kill 或未取到退出码。
    Exited { code: Option<i32>, killed: bool },
}

impl RunnerState {
    /// 「有进行中 turn」——桌面图标第二个数字（`[📱0 · ⚙1]`）的口径，
    /// 也是空闲回收的禁止条件。
    #[must_use]
    pub fn is_working(self) -> bool {
        matches!(self, Self::Busy | Self::AwaitingPermission)
    }

    #[must_use]
    pub fn is_exited(self) -> bool {
        matches!(self, Self::Exited { .. })
    }
}

/// runner 操作失败。
///
/// 用 `thiserror`（§6.9 缺陷修正 ①：初稿直接 `#[derive(thiserror::Error)]` 但
/// **lumen-app 当时没有 thiserror 依赖**——workspace 有，crate 的 `[dependencies]` 里没有，
/// 只有 `anyhow`）。本片给 lumen-app 补了 `thiserror.workspace = true`，与仓库其它 crate 一致。
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("已达 headless 会话上限 {0}")]
    LimitReached(usize),
    /// 片 4 接线后由 `Close` / `Send` 的路由查表构造。
    #[allow(dead_code)]
    #[error("未找到 {0}")]
    NotFound(RunnerId),
    /// 片 4 接线后由 `submit` / `control` 的调用方看到。
    #[allow(dead_code)]
    #[error("runner 已退出")]
    Exited,
    #[error("CLI 可执行文件未找到：{0}")]
    CliNotFound(&'static str),
    #[error("工作目录不存在或不是目录")]
    CwdInvalid,
    #[error("启动失败：{0}")]
    Spawn(#[source] std::io::Error),
    #[error("出站消息编码失败：{0}")]
    Encode(#[from] adapter::EncodeError),
    #[error("stdin 写线程已结束（会话不可再收消息）")]
    StdinClosed,
    /// stdin 队列已满（[`RUNNER_STDIN_CAP`]）——CLI 消费不过来。
    ///
    /// **显式拒绝而不是静默排队**：排队会让手机端看到一条「已发送」但实际几分钟后才轮到的消息。
    #[error("stdin 队列已满（{0} 条待写），CLI 消费不过来")]
    Backlogged(usize),
    /// P0 桌面端与手机端都不提供 bypass 开关（M7 §12-⑫ / §4.4 / §6.8.4）。
    #[error("bypassPermissions 在 P0 不可用")]
    BypassForbidden,
    #[error("未知的权限模式（认不出的模式只能保守拒绝）")]
    UnknownPermissionMode,
}

// ── stdin 通道 ────────────────────────────────────────────────────────────────

/// 主线程 → 写线程的消息。
///
/// **载荷只能是 [`StdinLine`]**，不是 `Vec<u8>` / `String`：那个类型的构造函数是本模块里
/// 唯一能产出"合法 stdin 行"的入口（§6.4 铁律 1、2 的类型落点，见 `adapter.rs`）。
enum StdinMsg {
    /// 尚无生产调用点：唯一构造方 [`LlmRunner::submit`] / [`LlmRunner::control`] 要等片 4 接线。
    #[allow(dead_code)]
    Line(StdinLine),
    /// 优雅停止第一步：关掉 stdin 让 CLI 见 EOF 自然退出。
    CloseStdin,
}

// ── stderr 尾部缓冲 ───────────────────────────────────────────────────────────

/// stderr 的最后若干行。**只在本机使用，不进 transcript、不上行**（见模块文档二末条）。
#[derive(Debug, Default)]
pub struct StderrTail {
    lines: VecDeque<String>,
    /// 收到过多少**非空**行。`0` 就是实测里那条"stderr 完全为空"的判据。
    total: u64,
    /// 是否见过 [`STDIN_REJECT_MARKER`]。
    saw_stdin_reject: bool,
}

impl StderrTail {
    fn push(&mut self, line: String) {
        if line.trim().is_empty() {
            // 空行不计数：判「stderr 完全为空」时，一个孤零零的换行不该让归因走错分支。
            return;
        }
        self.total = self.total.saturating_add(1);
        if line.contains(STDIN_REJECT_MARKER) {
            self.saw_stdin_reject = true;
        }
        if self.lines.len() >= STDERR_TAIL_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// **实测认证失败时这里恒为 `true`**——所以它绝不能单独作为"没出错"的依据。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    #[must_use]
    pub fn saw_stdin_reject(&self) -> bool {
        self.saw_stdin_reject
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    #[must_use]
    pub fn to_vec(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

// ── transcript 环形缓冲 ───────────────────────────────────────────────────────

/// 带 `seq` 的一条 transcript 记录。
#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    /// 本 runner 内自 1 起严格递增、**连续**、不复用。
    ///
    /// 连续是硬要求：协议侧 `LlmFrame::Delta.seq` 的不变量是"断裂即触发整轮重拉"，
    /// 这里跳号就等于给控制端制造一个永远补不上的缺口。
    pub seq: u64,
    pub event: RunnerEvent,
}

/// 每 runner 的本地事件环形缓冲——**手机断线期间事件的唯一去处**（§6.7 硬契约）。
#[derive(Debug, Default)]
pub struct Transcript {
    entries: VecDeque<TranscriptEntry>,
    next_seq: u64,
    bytes: usize,
    /// 「从这个 seq 起才有数据」。`0` = 一条没丢。
    ///
    /// 超上限时**必须标记它而不是假装完整**：控制端据此展示"更早的内容已不可补齐"，
    /// 而不是拿到一段看起来连续、实则中间少了 3000 条的历史。
    truncated_from: u64,
    /// 累计被淘汰的条数（诊断 / 审计用）。
    dropped: u64,
}

impl Transcript {
    /// 追加一条并返回分配的 `seq`。超上限时从**最老**一端淘汰。
    pub fn push(&mut self, event: RunnerEvent) -> u64 {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.bytes = self.bytes.saturating_add(event.approx_bytes());
        self.entries.push_back(TranscriptEntry { seq, event });
        while self.entries.len() > TRANSCRIPT_MAX_EVENTS
            || (self.bytes > TRANSCRIPT_MAX_BYTES && self.entries.len() > 1)
        {
            // 保留至少一条：一条超大事件把预算撑爆时，全清空会让"最新状态"也消失。
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.event.approx_bytes());
            self.dropped = self.dropped.saturating_add(1);
            self.truncated_from = old.seq + 1;
        }
        seq
    }

    /// 已分配到的最大 `seq`（`0` = 还没有任何事件）。
    #[must_use]
    pub fn last_seq(&self) -> u64 {
        self.next_seq
    }

    /// 「从这个 seq 起才有数据」，`0` = 一条没丢。
    #[must_use]
    pub fn truncated_from(&self) -> u64 {
        self.truncated_from
    }

    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 片 4 的基线帧要靠它判断「这个对话有没有可补发的历史」。
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 控制端声明"我已消费到 `known_seq`"，是否存在**补不齐的缺口**。
    ///
    /// 有缺口时片 4 必须回 `Attached{resync_required: true}` 让手机清空本地状态重建，
    /// 而不是从缓冲里能发多少发多少——后者会拼出一段静默错位的对话。
    #[must_use]
    pub fn has_gap_after(&self, known_seq: u64) -> bool {
        // `saturating_add` 而不是裸 `+`：`known_seq` 来自控制端帧，是**对端可控**值，
        // 裸加在 debug 构建下是 overflow panic。
        self.truncated_from > 0 && known_seq.saturating_add(1) < self.truncated_from
    }

    /// 补发 `known_seq` 之后的全部事件。
    pub fn replay_from(&self, known_seq: u64) -> impl Iterator<Item = &TranscriptEntry> {
        self.entries.iter().filter(move |e| e.seq > known_seq)
    }
}

// ── 退出归因 ──────────────────────────────────────────────────────────────────

/// 子进程退出的**归因**。见模块文档四。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitAttribution {
    /// 退出码 0，正常收场。
    Normal,
    /// 是我们主动 kill / 优雅停止的。
    Killed,
    /// **我们喂了非法 JSON**：stderr 里出现了 [`STDIN_REJECT_MARKER`]。
    /// 这是子系统里最不该发生的错误，见 [`ProtocolErrorKind::StdinRejected`]。
    StdinRejected { code: i32 },
    /// 非零退出码**且 stderr 非空**：原因在 stderr 里，可直接引用尾部若干行。
    FromStderr { code: i32, tail: Vec<String> },
    /// **非零退出码但 stderr 完全为空**——实测的认证失败路径。
    /// 唯一的失败依据是最后一条 `result` 的 `is_error` / `terminal_reason` / `api_error_status`。
    FromLastResult {
        code: i32,
        outcome: Box<LlmTurnOutcome>,
    },
    /// 非零退出码、stderr 为空、**且连一条 `result` 都没有**：真的没有任何原因可报。
    ///
    /// 这个变体存在的意义就是**不许假装知道**：手机端应显示"进程异常退出（无诊断信息）"
    /// 而不是随便编一个 `AuthRequired`。
    Unattributable { code: i32 },
}

/// 退出归因的**纯函数**本体（抽出来只为可单测——真跑一个认证失败的 claude 太贵）。
///
/// 判定顺序即优先级，改动前请读模块文档四：
/// 1. 我们杀的 → [`ExitAttribution::Killed`]（不看退出码：`TerminateJobObject(1)` 给的就是 1）；
/// 2. 退出码 0 / 取不到 → [`ExitAttribution::Normal`]；
/// 3. stderr 里有 stdin 拒绝标记 → [`ExitAttribution::StdinRejected`]（**先于**通用 stderr 分支，
///    否则这条最要命的自身 bug 会被淹没在"stderr 里有点东西"里）；
/// 4. stderr 非空 → [`ExitAttribution::FromStderr`]；
/// 5. stderr 为空但有 `result` → [`ExitAttribution::FromLastResult`]（**实测路径**）；
/// 6. 都没有 → [`ExitAttribution::Unattributable`]。
#[must_use]
pub fn attribute_exit(
    code: Option<i32>,
    killed: bool,
    stderr: &StderrTail,
    last_outcome: Option<&LlmTurnOutcome>,
) -> ExitAttribution {
    if killed {
        return ExitAttribution::Killed;
    }
    let Some(code) = code.filter(|c| *c != 0) else {
        return ExitAttribution::Normal;
    };
    if stderr.saw_stdin_reject() {
        return ExitAttribution::StdinRejected { code };
    }
    if !stderr.is_empty() {
        return ExitAttribution::FromStderr {
            code,
            tail: stderr.to_vec(),
        };
    }
    match last_outcome {
        Some(outcome) => ExitAttribution::FromLastResult {
            code,
            outcome: Box::new(outcome.clone()),
        },
        None => ExitAttribution::Unattributable { code },
    }
}

// ── 单个 runner ───────────────────────────────────────────────────────────────

/// 一个 headless CLI 会话。
///
/// **`allow(dead_code)` 的范围收窄记账**：`agent` / `workspace` / `encoder` / `current_turn`
/// 四个字段与本类型的多数方法要等片 4 接线才有生产调用点。allow 打在**本类型上**而不是整个
/// `llm_runner` 模块上——模块级 allow 会把 3000 行的 `claude.rs` 一起罩住，
/// 那才是真正会藏住「写错了、没接上」的地方。
#[allow(dead_code)]
pub struct LlmRunner {
    id: RunnerId,
    agent: LlmAgentKind,
    /// CLI 自己的会话 id（我们用 `--session-id` 传进去的那个 uuid）。
    cli_session_id: String,
    workspace: PathBuf,
    proc: ChildProc,
    /// 主线程独占的出站编码器（§6.9 缺陷修正 ②③：编解码两半各归各的线程）。
    encoder: Box<dyn LlmAgentEncoder>,
    in_tx: Sender<StdinMsg>,
    ev_rx: Receiver<RunnerEvent>,
    /// 主线程 `submit()` 推进、读线程只读的当前轮号（§6.9 缺陷修正 ④）。
    ///
    /// **类型是 `u64` 而协议侧 `TurnNo` 是 `u32`**：这里不想为了对齐线格式而在内部承担溢出
    /// 讨论，转换留给片 4 在拼帧时做（`u32::try_from(...).unwrap_or(u32::MAX)`）。
    current_turn: Arc<AtomicU64>,
    stderr: Arc<Mutex<StderrTail>>,
    /// 读线程是否已读到 EOF 并退出。
    reader_done: Arc<AtomicBool>,
    /// stderr 线程是否已读到 EOF 并把最后一批行写进 [`StderrTail`]。
    ///
    /// 与 `reader_done` 同款，但守的是**另一件事**：退出归因要对 `StderrTail` 拍快照，
    /// 快照必须发生在 stderr flush 完之后（见 `pump_once` 第 4 步）。
    stderr_done: Arc<AtomicBool>,
    /// 写线程是否因写失败而退出。
    stdin_dead: Arc<AtomicBool>,
    stdin_fault_reported: bool,
    /// 还没进 pump 流的 `Spawned`（`Some(pid)` = 待补发）。见 `pump_once` 第 0 步。
    spawned_pending: Option<Option<u32>>,
    state: RunnerState,
    transcript: Transcript,
    /// 由 `system/init` 的 `capabilities` 覆盖前的兜底值。
    features: LlmAgentFeatures,
    tally: UnknownEventTally,
    /// 当前有多少个控制端订阅着（片 4 用 `Attach` / `Detach` 维护）。
    subscribers: u32,
    /// 控制端已 ACK 到的 seq（`DeltaAck`）。
    acked_seq: u64,
    last_activity: Instant,
    stopping_since: Option<Instant>,
    /// `try_wait` 首次看到进程退出的时刻。
    exit_seen_at: Option<Instant>,
    exit_code: Option<i32>,
    exit_reported: bool,
    /// 最后一条 `result` 的成败判定——**stderr 为空时唯一的失败依据**（模块文档四）。
    last_outcome: Option<LlmTurnOutcome>,
}

/// 同上：这一整块是片 4 的 API 面（`Attach` / `Detach` / `Send` / `Interrupt` / `Close` 的落点）。
#[allow(dead_code)]
impl LlmRunner {
    #[must_use]
    pub fn id(&self) -> RunnerId {
        self.id
    }

    #[must_use]
    pub fn agent(&self) -> &LlmAgentKind {
        &self.agent
    }

    #[must_use]
    pub fn cli_session_id(&self) -> &str {
        &self.cli_session_id
    }

    #[must_use]
    pub fn workspace(&self) -> &std::path::Path {
        &self.workspace
    }

    #[must_use]
    pub fn state(&self) -> RunnerState {
        self.state
    }

    #[must_use]
    pub fn features(&self) -> LlmAgentFeatures {
        self.features
    }

    #[must_use]
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    #[must_use]
    pub fn tally(&self) -> &UnknownEventTally {
        &self.tally
    }

    #[must_use]
    pub fn current_turn(&self) -> u64 {
        self.current_turn.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn subscribers(&self) -> u32 {
        self.subscribers
    }

    /// 控制端订阅 / 退订。**订阅数只影响空闲回收与保留期，绝不影响进程存活**——
    /// 这正是 §6.7 硬契约的落点（手机断线时 `subscribers` 归 0，但 runner 照跑）。
    pub fn attach(&mut self) {
        self.subscribers = self.subscribers.saturating_add(1);
        self.last_activity = Instant::now();
    }

    pub fn detach(&mut self) {
        self.subscribers = self.subscribers.saturating_sub(1);
        self.last_activity = Instant::now();
    }

    /// 控制端确认已消费到 `seq`。
    pub fn ack(&mut self, seq: u64) {
        self.acked_seq = self.acked_seq.max(seq);
    }

    /// 退出归因（见模块文档四）。进程还活着时返回 `None`。
    #[must_use]
    pub fn exit_attribution(&self) -> Option<ExitAttribution> {
        if !self.state.is_exited() {
            return None;
        }
        let stderr = self.stderr.lock().ok()?;
        Some(attribute_exit(
            self.exit_code,
            self.proc.killed(),
            &stderr,
            self.last_outcome.as_ref(),
        ))
    }

    /// stderr 尾部快照（本地审计日志用；**不上行**）。
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr.lock().map(|t| t.to_vec()).unwrap_or_default()
    }

    /// 提交一条用户消息，返回本轮的轮号。
    ///
    /// # Errors
    /// - [`RunnerError::Exited`]：进程已退出。
    /// - [`RunnerError::Encode`]：编码失败（**绝不降级发一条"差不多"的**，非法行会杀会话）。
    /// - [`RunnerError::StdinClosed`]：写线程已结束。
    /// - [`RunnerError::Backlogged`]：stdin 队列已满（[`RUNNER_STDIN_CAP`]）。
    ///
    /// 后两种**都不会消耗轮号**（见下）。
    ///
    /// # 轮号推进的时序
    /// 先 `fetch_add` 再发消息：读线程解码时读到的必须已经是**新**轮号，否则本轮的第一批
    /// 事件会被记到上一轮上。反过来（先发后加）存在一个真实的窗口——CLI 回吐用户消息回执
    /// 极快，实测 `--replay-user-messages` 是同步回吐的。
    ///
    /// # 送不出去时必须把两个计数器**一起回滚**
    /// 轮号有**两个**计数器：这里的 [`Self::current_turn`] 与 `ClaudeEncoder.turn`，
    /// 靠「`submit` 是唯一调用点、成对推进」保持同步。`send` 失败时 CLI 根本没收到消息、
    /// 不会有任何该轮事件，若不回滚，这个轮号就被**烧掉**了——手机端看到的 turn 序列出现
    /// 一个永远不会有内容的空洞。回滚而不是「改成发成功后再加」，是因为现在的顺序本身是对的
    /// （读线程必须先看到新轮号），换序的风险比回滚大。
    ///
    /// 回滚是安全的：此刻 `send` 失败 ⇒ 写线程从未看到这条消息 ⇒ CLI 不会产生任何事件 ⇒
    /// 读线程不可能已经读到新轮号。
    pub fn submit(
        &mut self,
        text: &str,
        attachments: &[LlmAttachment],
    ) -> Result<u64, RunnerError> {
        if self.state.is_exited() {
            return Err(RunnerError::Exited);
        }
        let line = self.encoder.encode_user_message(text, attachments)?;
        let turn = self.current_turn.fetch_add(1, Ordering::AcqRel) + 1;
        if let Err(err) = try_send_stdin(&self.in_tx, StdinMsg::Line(line)) {
            self.current_turn.fetch_sub(1, Ordering::AcqRel);
            self.encoder.rollback_turn();
            return Err(err);
        }
        self.state = RunnerState::Busy;
        self.last_activity = Instant::now();
        Ok(turn)
    }

    /// 发一条控制指令（interrupt / set_permission_mode / …）。
    ///
    /// # Errors
    /// 同 [`Self::submit`]；本适配器不支持该指令时返回
    /// [`RunnerError::Encode`]`(`[`adapter::EncodeError::Unsupported`]`)`，调用方可静默跳过。
    pub fn control(&mut self, cmd: &ControlCmd) -> Result<(), RunnerError> {
        if self.state.is_exited() {
            return Err(RunnerError::Exited);
        }
        let line = self.encoder.encode_control(cmd)?;
        try_send_stdin(&self.in_tx, StdinMsg::Line(line))?;
        self.last_activity = Instant::now();
        Ok(())
    }

    /// 优雅停止第一步：关 stdin 让 CLI 见 EOF 自然退出。
    /// 超过 [`RUNNER_STOP_GRACE`] 仍没退出，`pump` 会 `kill_tree`。
    pub fn begin_stop(&mut self) {
        if self.stopping_since.is_some() {
            return;
        }
        // 发不出去不算错误，两种情形都已有兜底：
        // - 写线程已死 ⇒ stdin 早就没了，正是我们想要的结果；
        // - 队列满（CLI 根本没在消费）⇒ 关 stdin 本来也救不了，[`RUNNER_STOP_GRACE`] 到点后
        //   `pump_once` 第 5 步会 `kill_tree`。
        // **绝不用阻塞 `send`**：那会在「CLI 卡死」时把主线程一起冻住。
        let _ = self.in_tx.try_send(StdinMsg::CloseStdin);
        self.stopping_since = Some(Instant::now());
    }

    /// 立刻杀掉整棵进程树（桌面弹层的红色「断开」按钮 / Lumen 退出）。
    pub fn kill(&mut self) {
        self.proc.kill_tree();
        self.stopping_since.get_or_insert_with(Instant::now);
    }

    /// 空闲回收判据。**turn 中途恒为 `false`**（§6.7：turn 中途绝不杀）。
    #[must_use]
    fn should_idle_reap(&self) -> bool {
        self.subscribers == 0
            && !self.state.is_working()
            && !self.state.is_exited()
            && self.last_activity.elapsed() >= RUNNER_IDLE_TIMEOUT
    }

    /// 是否可以从容器里真正移除。
    ///
    /// 条件：已退出**且已报出去**、无人订阅，且（transcript 已被读完 **或** 超过保留期）。
    /// 少任何一条都会让手机在"进程刚挂掉就重连"这个最需要看到错误的时刻拿到空。
    #[must_use]
    fn reapable(&self) -> bool {
        if !self.state.is_exited() || !self.exit_reported || self.subscribers > 0 {
            return false;
        }
        let consumed = self.acked_seq >= self.transcript.last_seq();
        let expired = self
            .exit_seen_at
            .is_some_and(|t| t.elapsed() >= RUNNER_EXITED_RETENTION);
        consumed || expired
    }

    /// 事件 → 状态机。
    fn apply(&mut self, ev: &RunnerEvent) {
        self.last_activity = Instant::now();
        match ev {
            RunnerEvent::Spawned { .. } => self.state = RunnerState::Booting,
            RunnerEvent::Started(info) => {
                self.features = info.features;
                if !self.state.is_working() && !self.state.is_exited() {
                    self.state = RunnerState::Idle;
                }
                if info.cli_session_id != self.cli_session_id {
                    // 不是致命错误，但 `--resume` 会对不上，必须留痕。
                    log::warn!(
                        "{} 的 CLI 会话 id 与我们传入的 --session-id 不一致，resume 将失效",
                        self.id
                    );
                }
            }
            RunnerEvent::TurnUserEcho { .. } | RunnerEvent::Block { .. } => {
                if !self.state.is_exited() {
                    self.state = RunnerState::Busy;
                }
            }
            RunnerEvent::PermissionAsk { .. } => {
                if !self.state.is_exited() {
                    self.state = RunnerState::AwaitingPermission;
                }
            }
            RunnerEvent::TurnEnded(info) => {
                self.last_outcome = Some(info.outcome.clone());
                if !self.state.is_exited() {
                    self.state = RunnerState::Idle;
                }
            }
            RunnerEvent::Exited { code, killed, .. } => {
                self.state = RunnerState::Exited {
                    code: *code,
                    killed: *killed,
                };
            }
            RunnerEvent::LineDropped { tag } => self.tally.record(tag),
            RunnerEvent::ProtocolError { kind } => {
                if let ProtocolErrorKind::UnknownSchema { tag } = kind {
                    self.tally.record(tag);
                }
            }
            RunnerEvent::PermissionModeChanged { .. }
            | RunnerEvent::RateLimit { .. }
            | RunnerEvent::ControlAck { .. } => {}
        }
    }

    /// 记一条事件：走状态机 → 进 transcript 领 seq → 交给调用方。
    ///
    /// 这里的 `clone` 是刻意接受的成本：transcript 要留一份供断线补发，调用方要一份去拼帧。
    /// 被否决的替代方案是"只返回 `(id, seq)` 让调用方回查 transcript"——那会在
    /// 环形缓冲刚好淘汰掉这条时变成一个只在高负载下偶发的空指针式 bug。
    fn record(&mut self, ev: RunnerEvent, out: &mut Vec<(RunnerId, u64, RunnerEvent)>) {
        self.apply(&ev);
        let seq = self.transcript.push(ev.clone());
        out.push((self.id, seq, ev));
    }

    /// 每帧一次的推进（见 [`LlmRunnerManager::pump`]）。
    fn pump_once(&mut self, out: &mut Vec<(RunnerId, u64, RunnerEvent)>) {
        // 0) 补发 `Spawned`。**必须在排空 ev_rx 之前**，它才拿得到本会话的 seq=1。
        //    初稿是在 `start()` 里直接 `transcript.push(..)`，绕过了 `record()` ⇒ 这条事件
        //    **只进 transcript、不进 pump 流**，片 4 若把 `pump()` 当完整事件流用，实时通道的
        //    seq 会从 2 起跳，而 `truncated_from` 仍是 0、`has_gap_after` 也不报缺口——
        //    一个类型上看不见、只能靠读代码发现的陷阱。改成在这里走 `record()`。
        if let Some(pid) = self.spawned_pending.take() {
            self.record(RunnerEvent::Spawned { pid }, out);
        }

        // 1) 排空读线程事件。
        //    这里的 `clone` 是有理由的、不是随手写的：直接写
        //    `while let Ok(ev) = self.ev_rx.try_recv()` 会把 `&self.ev_rx` 这个临时借用
        //    延长到**循环体结束**（2021 版临时值作用域规则），与体内 `self.record(..)` 的
        //    `&mut self` 冲突；改写成 `loop { let-else }` 能过借用检查，但会被
        //    `clippy::while_let_loop` 拦下。clone 一份接收端到局部之后，借用就落在局部变量上、
        //    与 `self` 无关。crossbeam 的 `Receiver` 是 `Arc` 语义，clone 只是一次原子加，
        //    且克隆体与原体共享同一条通道（不会漏收事件）。
        let rx = self.ev_rx.clone();
        while let Ok(ev) = rx.try_recv() {
            self.record(ev, out);
        }

        // 2) stdin 写线程故障（只报一次）。
        if self.stdin_dead.load(Ordering::Acquire) && !self.stdin_fault_reported {
            self.stdin_fault_reported = true;
            let rejected = self
                .stderr
                .lock()
                .map(|t| t.saw_stdin_reject())
                .unwrap_or(false);
            let kind = if rejected {
                ProtocolErrorKind::StdinRejected
            } else {
                ProtocolErrorKind::StdinWriteFailed
            };
            self.record(RunnerEvent::ProtocolError { kind }, out);
            // 半行已经出去了 = 会话必死，**不得重试**（见该错误的类型文档）。
            self.begin_stop();
        }

        // 3) 非阻塞收尸。三态：`Lost`（句柄异常）**也算已退出**，只是拿不到退出码——
        //    绝不能当成"还在跑"，那会让这个 runner 永久占着并发名额（见 `TryWaitOutcome`）。
        match self.proc.try_wait() {
            TryWaitOutcome::Running => {}
            TryWaitOutcome::Exited(status) => {
                self.exit_code = status.and_then(|s| s.code());
                self.exit_seen_at.get_or_insert_with(Instant::now);
            }
            TryWaitOutcome::Lost => {
                // 退出码留 `None` ⇒ 归因落 `Normal` / `Unattributable`。诚实地说"不知道"，
                // 好过永久挂起。
                self.exit_seen_at.get_or_insert_with(Instant::now);
            }
        }

        // 4) 报 Exited —— **必须等读线程把管道残留读完**，否则本轮最后几条事件（含那条
        //    唯一能解释失败原因的 `result`）会排在 Exited 之后，手机端先看到"退出"再看到
        //    "结果"，时间线是乱的。超过 EXIT_DRAIN_GRACE 则不再等（防卡死读线程把 runner 拖住）。
        //
        //    **stderr 线程也要等**：`stderr_empty` 是在这一刻对 `StderrTail` 拍的快照，而
        //    模块文档四那条最核心的归因（非零退出码 + stderr 完全为空 ⇒ 回查最后一条 result）
        //    完全建立在它上面。stderr 线程若还没 flush 完最后一批诊断行，快照就会是 `true`，
        //    于是**有 stderr 的场景被误判成没有**。
        if self.exit_seen_at.is_some() && !self.exit_reported {
            let drained = self.reader_done.load(Ordering::Acquire)
                && self.stderr_done.load(Ordering::Acquire)
                && self.ev_rx.is_empty();
            let overdue = self
                .exit_seen_at
                .is_some_and(|t| t.elapsed() >= EXIT_DRAIN_GRACE);
            if drained || overdue {
                self.exit_reported = true;
                let stderr_empty = self.stderr.lock().map(|t| t.is_empty()).unwrap_or(true);
                let ev = RunnerEvent::Exited {
                    code: self.exit_code,
                    killed: self.proc.killed(),
                    stderr_empty,
                };
                self.record(ev, out);
            }
        }

        // 5) 停止宽限 / 空闲回收。
        //    **两条是独立判断，不是 `else if`**（§6.9 缺陷修正 ⑥：初稿写成 `else if`，
        //    导致 try_wait 收尸后 `stopping_since` 仍 `Some`、空闲回收分支永不触发）。
        //
        //    `!self.proc.killed()` 是**幂等闸门**：`state` 只在收到 `Exited` 事件时才变，而第 4 步
        //    还要等 `reader_done` / `EXIT_DRAIN_GRACE`。没有这道闸门，宽限期满到状态落定之间的
        //    **每一帧**都会重发一次 `TerminateJobObject` + `child.kill()`，并刷一条
        //    「TerminateJobObject 返回失败（job 可能已空）」——把真正的失败淹没在日志里。
        if let Some(since) = self.stopping_since {
            if since.elapsed() >= RUNNER_STOP_GRACE
                && !self.state.is_exited()
                && !self.proc.killed()
            {
                self.proc.kill_tree();
            }
        } else if self.should_idle_reap() {
            log::info!("{} 空闲超时，优雅停止", self.id);
            self.begin_stop();
        }
    }
}

// ── 容器 ──────────────────────────────────────────────────────────────────────

/// 与 `AppState.tabs: Vec<Tab>` **平行**的独立容器（对齐 `ssh_runtime.rs:641-649`）。
///
/// ⑫ 拍板后**刻意没有** `bypass_opt_in: HashSet<RunnerId>` 字段：桌面端逐会话 bypass 开关
/// 已取消，P0 桌面端与手机端都不提供（§4.4 / §6.8.4 / §12-⑫）。这里留一行记账是因为初稿
/// 有过这个字段——不留下一个人会照着旧稿把它加回来。
pub struct LlmRunnerManager {
    runners: HashMap<RunnerId, LlmRunner>,
    /// 保序展示用（桌面图标弹层 / 手机列表）。
    order: Vec<RunnerId>,
    next_id: u64,
    waker: Waker,
}

impl LlmRunnerManager {
    /// 构造（**不 spawn 任何进程**）。
    ///
    /// `waker` 里的 `wake_pending` 必须是 `AppState` 那个**全局**标志，不能另起一个——唤醒去重
    /// 协议是全局一份（`session.rs` 模块文档），另起一个会让"主线程已清标志但另一个标志仍为
    /// true"变成丢唤醒。这条约束由 [`Waker::new`] 的文档承载。
    #[must_use]
    pub fn new(waker: Waker) -> Self {
        Self {
            runners: HashMap::new(),
            order: Vec::new(),
            next_id: 1,
            waker,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.runners.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runners.is_empty()
    }

    /// 「有进行中 turn」的 runner 数——桌面图标第二个数字 `[📱0 · ⚙1]`。
    ///
    /// **图标必须显示两个数**（§6.8.2）：只显示手机数会让 §6.7 那条「手机断线 runner 继续跑」
    /// 的核心契约在 UI 上彻底不可见——用户看到"0 部手机"却发现 CPU 在烧，比没有图标更糟。
    ///
    /// 与 `ids` / `iter` / `close` 一样，生产调用点在片 4（桌面图标弹层 + 手机会话列表）。
    #[allow(dead_code)]
    #[must_use]
    pub fn working_count(&self) -> usize {
        self.runners
            .values()
            .filter(|r| r.state.is_working())
            .count()
    }

    /// 按创建顺序的 id 列表。
    #[allow(dead_code)]
    #[must_use]
    pub fn ids(&self) -> &[RunnerId] {
        &self.order
    }

    #[must_use]
    pub fn get(&self, id: RunnerId) -> Option<&LlmRunner> {
        self.runners.get(&id)
    }

    pub fn get_mut(&mut self, id: RunnerId) -> Option<&mut LlmRunner> {
        self.runners.get_mut(&id)
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = &LlmRunner> {
        self.order.iter().filter_map(|id| self.runners.get(id))
    }

    /// 起一个新会话。
    ///
    /// # Errors
    /// - [`RunnerError::LimitReached`]：已达 [`MAX_HEADLESS_RUNNERS`]（**显式拒绝，不排队**）。
    /// - [`RunnerError::CwdInvalid`]：工作目录不存在 / 不是目录。
    /// - [`RunnerError::CliNotFound`]：可执行文件不在 PATH 上。
    /// - [`RunnerError::Spawn`]：其它 IO 错误。
    pub fn start(
        &mut self,
        adapter: &dyn LlmAgentAdapter,
        spec: &LaunchSpec,
    ) -> Result<RunnerId, RunnerError> {
        if self.runners.len() >= MAX_HEADLESS_RUNNERS {
            return Err(RunnerError::LimitReached(MAX_HEADLESS_RUNNERS));
        }
        if !spec.workspace.is_dir() {
            return Err(RunnerError::CwdInvalid);
        }
        // `spec.agent` 与适配器必须同源。不设运行时错误变体（这属于调用方拼错了参数、
        // 不是运行期状况），但 debug 构建下要立刻炸出来而不是起一个用错参数的进程。
        debug_assert_eq!(
            spec.agent,
            adapter.kind(),
            "LaunchSpec.agent 与适配器不一致"
        );

        let id = RunnerId(self.next_id);
        self.next_id += 1;

        let args = adapter.build_args(spec);
        let env = adapter.build_env(spec);
        let (proc, pipes) = ChildProc::spawn(adapter.program(), &args, &env, &spec.workspace)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    RunnerError::CliNotFound(adapter.program())
                } else {
                    RunnerError::Spawn(e)
                }
            })?;

        let (encoder, decoder) = adapter.split();
        let (in_tx, in_rx) = crossbeam_channel::bounded::<StdinMsg>(RUNNER_STDIN_CAP);
        let (ev_tx, ev_rx) = crossbeam_channel::bounded::<RunnerEvent>(RUNNER_EVENT_CAP);
        let stderr = Arc::new(Mutex::new(StderrTail::default()));
        let reader_done = Arc::new(AtomicBool::new(false));
        let stderr_done = Arc::new(AtomicBool::new(false));
        let stdin_dead = Arc::new(AtomicBool::new(false));
        let current_turn = Arc::new(AtomicU64::new(0));

        spawn_writer_thread(
            id,
            pipes.stdin,
            in_rx,
            Arc::clone(&stdin_dead),
            self.waker.clone(),
        );
        spawn_reader_thread(
            id,
            pipes.stdout,
            decoder,
            ev_tx,
            Arc::clone(&reader_done),
            self.waker.clone(),
        );
        spawn_stderr_thread(
            id,
            pipes.stderr,
            Arc::clone(&stderr),
            Arc::clone(&stderr_done),
            self.waker.clone(),
        );

        let pid = proc.pid();
        let runner = LlmRunner {
            id,
            agent: adapter.kind(),
            cli_session_id: spec.cli_session_id.clone(),
            workspace: spec.workspace.clone(),
            proc,
            encoder,
            in_tx,
            ev_rx,
            current_turn,
            stderr,
            reader_done,
            stderr_done,
            stdin_dead,
            stdin_fault_reported: false,
            spawned_pending: Some(pid),
            state: RunnerState::Booting,
            transcript: Transcript::default(),
            features: adapter.baseline_features(),
            tally: UnknownEventTally::default(),
            subscribers: 0,
            acked_seq: 0,
            last_activity: Instant::now(),
            stopping_since: None,
            exit_seen_at: None,
            exit_code: None,
            exit_reported: false,
            last_outcome: None,
        };
        // `Spawned` **不**在这里直接 `transcript.push`：那样它就绕过了 `record()`，
        // 永远不会出现在 `pump()` 的返回值里（见 `pump_once` 第 0 步的长注释）。
        // 它由第一次 `pump_once` 补发，并因此拿到本会话的 seq=1 —— 顺序仍然有保证，
        // 因为第 0 步排在"排空 ev_rx"之前。
        log::info!(
            "{id} 已启动：{} pid={pid:?} cwd={}",
            adapter.program(),
            spec.workspace.display()
        );
        self.runners.insert(id, runner);
        self.order.push(id);
        Ok(id)
    }

    /// 主动关闭一个会话（手机 `Close` / 桌面弹层「断开」）。
    ///
    /// # Errors
    /// [`RunnerError::NotFound`]。
    #[allow(dead_code)]
    pub fn close(&mut self, id: RunnerId) -> Result<(), RunnerError> {
        let runner = self.runners.get_mut(&id).ok_or(RunnerError::NotFound(id))?;
        runner.begin_stop();
        Ok(())
    }

    /// **主线程每帧调用**（`main.rs` 的 `pump_remote()` 旁边，且必须在
    /// `remote_ws.sub_target()` 那层嵌套之外）。
    ///
    /// 返回本帧产生的全部事件 `(runner, seq, event)`，按 runner 创建顺序、runner 内按 seq 递增。
    pub fn pump(&mut self) -> Vec<(RunnerId, u64, RunnerEvent)> {
        let mut out = Vec::new();
        // §6.9 缺陷修正 ⑤：初稿的 `for id in to_reap { let _ = id; }` 是空操作，
        // `self.runners` 只增不减；而 `start()` 用 `runners.len() >= MAX` 判上限 ——
        // 4 次崩溃之后**永久无法再起新会话**。这里是真正的回收。
        let mut to_reap: Vec<RunnerId> = Vec::new();

        // 借 `&self.order` 与 `&mut self.runners` 是**不相交字段**借用，编译器允许。
        for id in &self.order {
            let Some(runner) = self.runners.get_mut(id) else {
                continue;
            };
            runner.pump_once(&mut out);
            if runner.reapable() {
                to_reap.push(*id);
            }
        }

        for id in to_reap {
            log::info!("{id} 已回收");
            self.runners.remove(&id);
            self.order.retain(|x| *x != id);
            // （原先这里还有一行 `self.bypass_opt_in.remove(&id);`，随 ⑫ 一并删除。）
        }
        out
    }

    /// 杀掉全部会话（Lumen 退出 / 登出）。
    pub fn shutdown_all(&mut self) {
        for runner in self.runners.values_mut() {
            runner.kill();
        }
    }
}

impl Drop for LlmRunnerManager {
    /// Lumen 退出收尾。
    ///
    /// Windows 上 Job Object 的 `KILL_ON_JOB_CLOSE` 是最后保险：即使这里没跑到
    /// （崩溃 / 被强杀），OS 关闭 job 句柄时也会杀掉整棵树。
    ///
    /// **本 `Drop` 能跑到，依赖模块文档五那两条仓库事实**（无 `process::exit`、`panic=unwind`）。
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

// ── 三个线程 ──────────────────────────────────────────────────────────────────

/// 主线程唤醒器：`EventLoopProxy` 与那个**全局** `wake_pending` 标志的成对封装。
///
/// # 为什么要有这个类型（而不是继续到处传两个参数）
/// 1. **成对性**：标志与投递端必须永远同源。分开传，将来有人在某个线程里传了另一个标志
///    （或忘了传标志直接 `send_event`），去重协议就破了，表现是"偶尔丢一次唤醒、UI 卡到
///    下一次无关事件"——极难归因。
/// 2. **可测**：三个工作线程与整条 spawn→pump→Exited 链路此前**零测试覆盖**，正是因为它们
///    都要一个 `EventLoopProxy`，而 `EventLoop` 在测试线程里建不出来。[`Waker::noop`] 让
///    整条链路可以在普通单测里跑完（本模块底部那条集成测试就是这么来的）。
///
/// **被否决的替代方案**：把 `proxy` 换成 `Box<dyn Fn()>` 回调。那会丢掉 `Clone`（三个线程各要
/// 一份），也让"到底叫醒了什么"在类型上不可见。
#[derive(Clone)]
pub struct Waker {
    /// `None` = 哑唤醒器（测试 / 无事件循环环境）：只记账、不投事件。
    proxy: Option<EventLoopProxy<PtyWake>>,
    /// **必须是 `AppState` 那个全局标志**（见 [`LlmRunnerManager::new`]）。
    wake_pending: Arc<AtomicBool>,
    /// 累计**真正投出去**的 `PtyWake` 条数（去重之后）。诊断 + 测试用。
    sent: Arc<AtomicU64>,
}

impl Waker {
    /// 生产用构造。`wake_pending` 必须是 `AppState` 的**全局**标志。
    #[must_use]
    pub fn new(proxy: EventLoopProxy<PtyWake>, wake_pending: Arc<AtomicBool>) -> Self {
        Self {
            proxy: Some(proxy),
            wake_pending,
            sent: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 哑唤醒器：**只记账、不投事件**。测试专用（生产路径上一律用 [`Self::new`]）。
    #[must_use]
    pub fn noop() -> Self {
        Self {
            proxy: None,
            wake_pending: Arc::new(AtomicBool::new(false)),
            sent: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 去重唤醒。与 `session.rs:470-474` / `remote_ws.rs` 的 `nudge` 共用同一个全局标志，
    /// 故**多叫几次不产生额外 `PtyWake`**，成本可忽略。
    pub fn wake(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            self.sent.fetch_add(1, Ordering::Relaxed);
            if let Some(proxy) = &self.proxy {
                let _ = proxy.send_event(PtyWake);
            }
        }
    }

    /// 累计投出的 `PtyWake` 条数。
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// **测试专用**：模拟主线程消费掉一次唤醒（`main.rs` 收到 `PtyWake` 时会
    /// `wake_pending.store(false)`）。没有它就测不出"背压期间还叫不叫得醒主线程"。
    #[cfg(test)]
    fn consume(&self) {
        self.wake_pending.store(false, Ordering::Release);
    }
}

/// 把一条 stdin 消息塞进有界队列。**绝不阻塞主线程**（见 [`RUNNER_STDIN_CAP`]）。
fn try_send_stdin(tx: &Sender<StdinMsg>, msg: StdinMsg) -> Result<(), RunnerError> {
    match tx.try_send(msg) {
        Ok(()) => Ok(()),
        Err(crossbeam_channel::TrySendError::Full(_)) => {
            Err(RunnerError::Backlogged(RUNNER_STDIN_CAP))
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => Err(RunnerError::StdinClosed),
    }
}

/// **stdin 写线程**：`ChildStdin` 的**唯一**持有者（§6.4 铁律 1）。
fn spawn_writer_thread(
    id: RunnerId,
    stdin: std::process::ChildStdin,
    rx: Receiver<StdinMsg>,
    stdin_dead: Arc<AtomicBool>,
    waker: Waker,
) {
    let build = std::thread::Builder::new().name(format!("lumen-llm-in-{}", id.0));
    let spawned = build.spawn(move || {
        let mut stdin = stdin;
        while let Ok(msg) = rx.recv() {
            let StdinMsg::Line(line) = msg else {
                // CloseStdin：跳出循环即 drop stdin → CLI 见 EOF 自然退出。
                break;
            };
            // **整行一次写完再 flush**（§6.4 铁律 2）。`write_all` 内部会处理短写；
            // 一旦它返回 Err，**可能已经写出去半行** ⇒ 会话必死，绝不重试
            // （重试会把下一条完整消息接在半行后面，拼出更离谱的非法 JSON）。
            // 分两句写：`stdin.write_all(..).and_then(|()| stdin.flush())` 会让闭包在
            // `write_all` 的可变借用还没结束时再借一次 `stdin`。
            let wrote = stdin.write_all(line.as_bytes());
            if let Err(e) = wrote.and_then(|()| stdin.flush()) {
                log::warn!("{id} stdin 写失败（会话不可恢复）: {e}");
                stdin_dead.store(true, Ordering::Release);
                waker.wake();
                break;
            }
        }
        // 线程结束 → stdin 落地 drop → CLI 收到 EOF。
    });
    if let Err(e) = spawned {
        log::error!("{id} 启动 stdin 写线程失败: {e}");
    }
}

/// **stdout 读线程**：切行 → 白名单 → 适配器解码 → 事件通道。
fn spawn_reader_thread(
    id: RunnerId,
    stdout: std::process::ChildStdout,
    decoder: Box<dyn adapter::LlmAgentDecoder>,
    ev_tx: Sender<RunnerEvent>,
    reader_done: Arc<AtomicBool>,
    waker: Waker,
) {
    let build = std::thread::Builder::new().name(format!("lumen-llm-out-{}", id.0));
    // 线程内外各持一份：spawn 失败时外面这份要负责把标志置上，
    // 否则主线程会白等一个永远不会"读干净"的读线程，直到 EXIT_DRAIN_GRACE 才报退出。
    let done_flag = Arc::clone(&reader_done);
    let spawned = build.spawn(move || {
        let mut stdout = stdout;
        let mut decoder = decoder;
        let mut splitter = LineSplitter::new(MAX_LINE_BYTES);
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut events: Vec<RunnerEvent> = Vec::new();
        let mut buf = vec![0u8; READ_CHUNK_BYTES];

        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break, // EOF：进程已退出或关闭了 stdout。
                Ok(n) => splitter.feed(&buf[..n], &mut chunks),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::debug!("{id} stdout 读取结束: {e}");
                    break;
                }
            }
            for chunk in chunks.drain(..) {
                decode_chunk(chunk, decoder.as_mut(), &mut events);
            }
            if !flush_events(&ev_tx, &mut events, &waker) {
                // 主线程接收端已丢弃（应用退出）。**`break` 不是 `return`**：
                // 还要走到函数末尾把 `reader_done` 置上。
                break;
            }
        }

        // 最后一段没有换行的残留也要走一遍（否则排障时会看到事件流莫名少一条）。
        splitter.finish(&mut chunks);
        for chunk in chunks.drain(..) {
            decode_chunk(chunk, decoder.as_mut(), &mut events);
        }
        flush_events(&ev_tx, &mut events, &waker);

        // 标记"管道已读干净"，主线程据此决定何时报 Exited（见 `pump_once` 第 4 步）。
        reader_done.store(true, Ordering::Release);
        waker.wake();
    });
    if let Err(e) = spawned {
        log::error!("{id} 启动 stdout 读线程失败: {e}");
        done_flag.store(true, Ordering::Release);
    }
}

/// 一个切分块 → 若干归一化事件。**任何一步失败都只影响这一行**（§6.3 理由 3）。
fn decode_chunk(
    chunk: Chunk,
    decoder: &mut dyn adapter::LlmAgentDecoder,
    events: &mut Vec<RunnerEvent>,
) {
    let line = match chunk {
        Chunk::Overlong { bytes } => {
            events.push(RunnerEvent::ProtocolError {
                kind: ProtocolErrorKind::OverlongLine { bytes },
            });
            return;
        }
        Chunk::Line(line) => line,
    };
    // **同一份**剥完 BOM 的切片同时喂给白名单与解码器：只在 `classify` 里剥、把带 BOM 的原文
    // 传给 `decode_line`，会让解码器再失败一次（`serde_json` 不吃 BOM）。
    let line = event::strip_bom(&line);
    match event::classify(line) {
        event::LineVerdict::Blank => {}
        event::LineVerdict::Parse(env) => {
            if let Err(err) = decoder.decode_line(&env, line, events) {
                // 注意：`err` 之前已经 push 进 `events` 的事件仍然有效，不回滚。
                events.push(RunnerEvent::ProtocolError {
                    kind: ProtocolErrorKind::DecodeFailed { at: err.at },
                });
            }
        }
        event::LineVerdict::DropSilently { tag } => {
            events.push(RunnerEvent::LineDropped { tag });
        }
        event::LineVerdict::UnknownSchema { tag } => {
            events.push(RunnerEvent::ProtocolError {
                kind: ProtocolErrorKind::UnknownSchema { tag },
            });
        }
        event::LineVerdict::Malformed { kind, bytes } => {
            events.push(RunnerEvent::ProtocolError {
                kind: ProtocolErrorKind::MalformedEnvelope { kind, bytes },
            });
        }
    }
}

/// 把攒下的事件送进有界通道。返回 `false` 表示接收端已关闭，读线程应结束。
///
/// **有意用阻塞 `send` 而不是 `try_send`**：通道满时阻塞读线程 → 不读管道 → OS 管道缓冲填满
/// → CLI 的 write 阻塞 → CLI 自然减速。丢事件会让手机端 `seq` 断裂并触发整轮重拉，
/// 比让 CLI 慢一点糟糕得多。
///
/// # 但「阻塞前必须先叫醒主线程」——初稿在这里自锁过
///
/// 初稿是「先把整批 `send` 完，最后才 nudge 一次」。于是存在一个**真实的互等窗口**：
/// 一次 `decode_chunk` 批量产出 > [`RUNNER_EVENT_CAP`] 条事件时（单行合法上限是
/// [`MAX_LINE_BYTES`] 32 MiB，一条 `assistant` 行的 `message.content[]` 可以产出任意多个
/// `Block` 事件），读线程在第 4097 条上阻塞，而**这一批一次唤醒都还没发过**；
/// 主线程侧 `pump_llm_runners` 只挂在 `user_event(PtyWake)` 上，`ControlFlow::Wait` 下
/// 没有 `PtyWake` 就不会再跑泵。结果是读线程等主线程取、主线程等唤醒，整个 LLM 子系统停摆，
/// 直到某个**无关**来源（PTY 输出 / stderr 线程 / remote_ws）恰好发一次 `PtyWake` 才解开。
///
/// 修法：**队列满就先唤醒再阻塞**。[`Waker::wake`] 自带去重，多叫几次不产生额外 `PtyWake`。
fn flush_events(ev_tx: &Sender<RunnerEvent>, events: &mut Vec<RunnerEvent>, waker: &Waker) -> bool {
    if events.is_empty() {
        return true;
    }
    for ev in events.drain(..) {
        if ev_tx.is_full() {
            // 即将阻塞：主线程是唯一的消费者，现在不叫醒它就再也没人来取了。
            waker.wake();
        }
        if ev_tx.send(ev).is_err() {
            return false;
        }
    }
    waker.wake();
    true
}

/// **stderr 读线程**：只往本地尾部缓冲写，**不进 transcript、不上行**（见模块文档二末条）。
fn spawn_stderr_thread(
    id: RunnerId,
    stderr: std::process::ChildStderr,
    tail: Arc<Mutex<StderrTail>>,
    stderr_done: Arc<AtomicBool>,
    waker: Waker,
) {
    let build = std::thread::Builder::new().name(format!("lumen-llm-err-{}", id.0));
    // 与读线程同款：spawn 失败时外面这份负责置位，否则主线程会白等到 `EXIT_DRAIN_GRACE`。
    let done_flag = Arc::clone(&stderr_done);
    let spawned = build.spawn(move || {
        let mut stderr = stderr;
        let mut splitter = LineSplitter::new(STDERR_MAX_LINE_BYTES);
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut buf = vec![0u8; READ_CHUNK_BYTES];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => splitter.feed(&buf[..n], &mut chunks),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
            if push_stderr(&tail, &mut chunks) {
                // stderr 有内容通常意味着出事了，值得叫醒主循环去看一眼。
                waker.wake();
            }
        }
        splitter.finish(&mut chunks);
        push_stderr(&tail, &mut chunks);
        // **置位必须在最后一次 push 之后**：主线程拿这个标志决定何时对 `StderrTail` 拍快照，
        // 顺序反了就等于没做（见 `pump_once` 第 4 步）。
        stderr_done.store(true, Ordering::Release);
        waker.wake();
        log::debug!("{id} stderr 读线程结束");
    });
    if let Err(e) = spawned {
        log::error!("{id} 启动 stderr 读线程失败: {e}");
        done_flag.store(true, Ordering::Release);
    }
}

/// 把切好的 stderr 行写进尾部缓冲。返回是否真的写入了内容。
fn push_stderr(tail: &Mutex<StderrTail>, chunks: &mut Vec<Chunk>) -> bool {
    if chunks.is_empty() {
        return false;
    }
    let Ok(mut guard) = tail.lock() else {
        chunks.clear();
        return false;
    };
    let mut wrote = false;
    for chunk in chunks.drain(..) {
        match chunk {
            Chunk::Line(line) => {
                guard.push(line);
                wrote = true;
            }
            Chunk::Overlong { bytes } => {
                guard.push(format!(
                    "<stderr 单行超过 {STDERR_MAX_LINE_BYTES} 字节，已丢弃 {bytes} 字节>"
                ));
                wrote = true;
            }
        }
    }
    wrote
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_protocol::llm::{LlmBlock, LlmBlockEntry, LlmText};

    // ── 端到端：真进程 → 三个线程 → pump → Exited → 回收 ────────────────────
    //
    // 对抗验证的核心指控是「96 条单测没有一条真的 spawn 过进程；全片唯一没被测到的那一步，
    // 恰好就是坏的那一步」。下面这组测试就是那个洞的补丁：用一个**假 CLI**（把事先写好的
    // stream-json 打到 stdout 的 shell）走完 spawn → 读线程 → pump → Exited → reap 全流程，
    // 不需要真的装 claude。

    /// 测试用临时目录（不引 tempfile 依赖）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("lumen-llm-{tag}-{nanos}"));
            std::fs::create_dir_all(&dir).expect("建临时目录");
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 把一个文件原样打到 stdout 的「假 CLI」。
    ///
    /// **刻意用 `type` / `cat` 读文件而不是 `echo` 拼 JSON**：JSON 里全是引号，经 `cmd` 拼串
    /// 会掉进另一个转义泥潭，而这条测试要验的是**管道与线程**，不是 shell 转义。
    struct FakeCliAdapter {
        script: PathBuf,
    }

    impl LlmAgentAdapter for FakeCliAdapter {
        fn kind(&self) -> LlmAgentKind {
            // 与 spec.agent 一致（`start()` 有 debug_assert），也让 `split()` 能给真解码器。
            LlmAgentKind::Claude
        }

        fn program(&self) -> &'static str {
            if cfg!(windows) {
                "cmd"
            } else {
                "sh"
            }
        }

        fn build_args(&self, _spec: &LaunchSpec) -> Vec<String> {
            let path = self.script.display().to_string();
            if cfg!(windows) {
                vec!["/c".into(), "type".into(), path]
            } else {
                // `$0` 传路径，绕开一切 shell 引号问题。
                vec!["-c".into(), "cat \"$0\"".into(), path]
            }
        }

        fn build_env(&self, _spec: &LaunchSpec) -> Vec<(String, String)> {
            Vec::new()
        }

        fn baseline_features(&self) -> LlmAgentFeatures {
            claude::ClaudeAdapter::new().baseline_features()
        }

        fn split(&self) -> (Box<dyn LlmAgentEncoder>, Box<dyn adapter::LlmAgentDecoder>) {
            claude::ClaudeAdapter::new().split()
        }
    }

    /// 反复 `pump` 直到看见 `Exited`（带上限，卡住就失败而不是挂死）。
    fn pump_until_exit(manager: &mut LlmRunnerManager) -> Vec<(RunnerId, u64, RunnerEvent)> {
        let mut all = Vec::new();
        for _ in 0..600 {
            all.extend(manager.pump());
            if all
                .iter()
                .any(|(_, _, ev)| matches!(ev, RunnerEvent::Exited { .. }))
            {
                return all;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("6 秒内没等到 Exited，实际事件：{all:?}");
    }

    #[test]
    fn 端到端_假cli走完spawn到回收全流程() {
        let dir = TempDir::new("e2e");
        let script = dir.path().join("events.jsonl");
        // 两行真实形状的 stream-json：一条 init（产出 Started）、一条 result（产出 TurnEnded）。
        std::fs::write(
            &script,
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sid-e2e\",\
             \"model\":\"m\",\"cwd\":\"F:\\\\x\",\"claude_code_version\":\"2.1.226\",\
             \"capabilities\":[\"interrupt_receipt_v1\"]}\n\
             {\"type\":\"result\",\"is_error\":false,\"result\":\"ok\"}\n",
        )
        .expect("写假 CLI 脚本");

        let adapter = FakeCliAdapter {
            script: script.clone(),
        };
        let mut manager = LlmRunnerManager::new(Waker::noop());
        let spec = LaunchSpec::new(
            LlmAgentKind::Claude,
            dir.path().to_path_buf(),
            "sid-e2e".to_owned(),
        );
        let id = manager.start(&adapter, &spec).expect("假 CLI 必须能起得来");
        assert_eq!(manager.len(), 1);

        let events = pump_until_exit(&mut manager);

        // ① `Spawned` 必须**在 pump 流里**且拿到 seq=1（初稿它只进 transcript，实时流从 2 起跳）。
        let (first_id, first_seq, first_ev) = &events[0];
        assert_eq!(*first_id, id);
        assert_eq!(*first_seq, 1, "Spawned 必须是本会话的第一条事件");
        assert!(matches!(first_ev, RunnerEvent::Spawned { .. }));

        // ② seq 在 runner 内严格递增且连续——协议侧 Delta.seq 的不变量就建立在这上面。
        let seqs: Vec<u64> = events.iter().map(|(_, seq, _)| *seq).collect();
        assert_eq!(
            seqs,
            (1..=seqs.len() as u64).collect::<Vec<_>>(),
            "seq 必须自 1 起连续"
        );

        // ③ 解码器真的跑了：init → Started，result → TurnEnded。
        let started = events.iter().find_map(|(_, _, ev)| match ev {
            RunnerEvent::Started(info) => Some(info),
            _ => None,
        });
        let started = started.expect("必须解出 Started");
        assert_eq!(started.cli_session_id, "sid-e2e");
        assert!(started.features.interrupt, "capabilities 应被真实探测到");
        assert!(events
            .iter()
            .any(|(_, _, ev)| matches!(ev, RunnerEvent::TurnEnded(_))));

        // ④ 退出被报出来，且退出归因走到了「正常」。
        let exited = events
            .iter()
            .find_map(|(_, _, ev)| match ev {
                RunnerEvent::Exited { code, killed, .. } => Some((*code, *killed)),
                _ => None,
            })
            .expect("必须报 Exited");
        assert_eq!(exited, (Some(0), false));
        let runner = manager.get(id).expect("尚未回收");
        assert!(runner.state().is_exited());
        assert_eq!(runner.exit_attribution(), Some(ExitAttribution::Normal));

        // ⑤ 控制端 ACK 完 ⇒ 名额必须真的还回来（否则 4 次之后永远起不了新会话）。
        let last = runner.transcript().last_seq();
        manager.get_mut(id).expect("在").ack(last);
        manager.pump();
        assert!(manager.is_empty(), "ACK 完的已退出 runner 必须被回收");
    }

    #[test]
    fn 起不来的cli必须报_cli_not_found_而不是卡住() {
        struct MissingAdapter;
        impl LlmAgentAdapter for MissingAdapter {
            fn kind(&self) -> LlmAgentKind {
                LlmAgentKind::Claude
            }
            fn program(&self) -> &'static str {
                "lumen-绝对不存在的-cli-9f3a"
            }
            fn build_args(&self, _spec: &LaunchSpec) -> Vec<String> {
                Vec::new()
            }
            fn build_env(&self, _spec: &LaunchSpec) -> Vec<(String, String)> {
                Vec::new()
            }
            fn baseline_features(&self) -> LlmAgentFeatures {
                LlmAgentFeatures::default()
            }
            fn split(&self) -> (Box<dyn LlmAgentEncoder>, Box<dyn adapter::LlmAgentDecoder>) {
                claude::ClaudeAdapter::new().split()
            }
        }
        let dir = TempDir::new("missing");
        let mut manager = LlmRunnerManager::new(Waker::noop());
        let spec = LaunchSpec::new(
            LlmAgentKind::Claude,
            dir.path().to_path_buf(),
            "sid".to_owned(),
        );
        let err = manager
            .start(&MissingAdapter, &spec)
            .expect_err("不存在的 CLI 不该起得来");
        assert!(
            matches!(err, RunnerError::CliNotFound(name) if name == "lumen-绝对不存在的-cli-9f3a"),
            "实际 {err:?}"
        );
        assert!(manager.is_empty(), "起失败不得占用名额");
    }

    // ── 背压路径上的唤醒（初稿在这里自锁）──────────────────────────────────

    #[test]
    fn 事件通道满时必须在阻塞之前先唤醒主线程() {
        // 复刻自锁现场：容量 2 的通道、一批 5 条事件、没有任何消费者。
        // 初稿是「整批 send 完才 nudge 一次」⇒ 读线程卡在第 3 条上，而**一次唤醒都还没发过**，
        // 主线程永远不会来取（`ControlFlow::Wait` 下没有 PtyWake 就不跑泵）。
        let (tx, rx) = crossbeam_channel::bounded::<RunnerEvent>(2);
        let waker = Waker::noop();
        let mut events: Vec<RunnerEvent> = (0..5).map(|_| text_event(1)).collect();

        let sender = {
            let waker = waker.clone();
            std::thread::spawn(move || flush_events(&tx, &mut events, &waker))
        };

        // 扮演主线程：先等到「被叫醒」，再去取——**顺序是这条测试的全部意义**。
        // 若唤醒只在批尾发，这里会等满 3 秒然后失败。
        let deadline = Instant::now() + Duration::from_secs(3);
        while waker.sent() == 0 {
            assert!(Instant::now() < deadline, "阻塞前没有发出任何唤醒 = 自锁");
            std::thread::sleep(Duration::from_millis(5));
        }
        // 模拟主线程消费掉这次唤醒（`main.rs` 收到 PtyWake 后会清标志），
        // 再把通道排空，让读线程得以走完。
        waker.consume();
        let mut got = 0;
        while got < 5 {
            if rx.recv_timeout(Duration::from_secs(3)).is_ok() {
                got += 1;
            } else {
                panic!("读线程没能把 5 条事件送完（自锁）");
            }
        }
        assert!(sender.join().expect("读线程未 panic"));
    }

    #[test]
    fn stdin队列满与写线程已死必须报成两种不同的错() {
        // 队列有界（[`RUNNER_STDIN_CAP`]）是刻意的：投喂方是手机端，堆在里面的是完整 prompt 正文。
        // 满了要**显式拒绝**而不是静默排队，更不能用阻塞 `send`（那会冻住 UI 事件循环）。
        let (tx, rx) = crossbeam_channel::bounded::<StdinMsg>(1);
        assert!(try_send_stdin(&tx, StdinMsg::CloseStdin).is_ok());
        assert!(
            matches!(
                try_send_stdin(&tx, StdinMsg::CloseStdin),
                Err(RunnerError::Backlogged(_))
            ),
            "满了必须是 Backlogged（可重试），不能与 StdinClosed（不可恢复）混为一谈"
        );
        drop(rx);
        assert!(matches!(
            try_send_stdin(&tx, StdinMsg::CloseStdin),
            Err(RunnerError::StdinClosed)
        ));
    }

    #[test]
    fn 唤醒去重_未被消费时不重复投递() {
        let waker = Waker::noop();
        waker.wake();
        waker.wake();
        waker.wake();
        assert_eq!(waker.sent(), 1, "全局标志没被清之前不该重复投 PtyWake");
        waker.consume();
        waker.wake();
        assert_eq!(waker.sent(), 2);
    }

    fn text_event(len: usize) -> RunnerEvent {
        RunnerEvent::Block {
            turn: 1,
            entry: LlmBlockEntry {
                block_id: 0,
                parent_call_id: None,
                block: LlmBlock::Text {
                    text: LlmText::new("x".repeat(len)),
                },
            },
        }
    }

    // ── transcript ────────────────────────────────────────────────────────────

    #[test]
    fn transcript_seq自1起严格递增且连续() {
        let mut t = Transcript::default();
        let a = t.push(text_event(1));
        let b = t.push(text_event(1));
        let c = t.push(text_event(1));
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(t.last_seq(), 3);
        assert_eq!(t.truncated_from(), 0);
        assert!(!t.has_gap_after(0));
    }

    #[test]
    fn transcript_按条数淘汰并标记truncated_from() {
        let mut t = Transcript::default();
        for _ in 0..(TRANSCRIPT_MAX_EVENTS + 5) {
            t.push(text_event(1));
        }
        assert_eq!(t.len(), TRANSCRIPT_MAX_EVENTS);
        assert_eq!(t.dropped(), 5);
        // 丢了 seq 1..=5 ⇒ "从 6 起才有数据"。
        assert_eq!(t.truncated_from(), 6);
        // 手机说"我看到 4 了"⇒ 5 补不上 ⇒ 有缺口。
        assert!(t.has_gap_after(4));
        // 手机说"我看到 5 了"⇒ 从 6 起能接上 ⇒ 无缺口。
        assert!(!t.has_gap_after(5));
    }

    #[test]
    fn transcript_按字节淘汰但至少留一条() {
        let mut t = Transcript::default();
        // 单条就撑爆预算：不能把它也删掉，否则"最新状态"凭空消失。
        t.push(text_event(TRANSCRIPT_MAX_BYTES * 2));
        assert_eq!(t.len(), 1);
        // 再来一条 ⇒ 老的被淘汰。
        t.push(text_event(16));
        assert_eq!(t.len(), 1);
        assert_eq!(t.truncated_from(), 2);
        assert_eq!(t.replay_from(0).count(), 1);
    }

    #[test]
    fn transcript_补发只给known_seq之后的() {
        let mut t = Transcript::default();
        for _ in 0..5 {
            t.push(text_event(1));
        }
        let seqs: Vec<u64> = t.replay_from(2).map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 4, 5]);
        assert_eq!(t.replay_from(5).count(), 0);
    }

    // ── stderr 尾部 ───────────────────────────────────────────────────────────

    #[test]
    fn stderr尾部只留最后若干行且空行不计数() {
        let mut tail = StderrTail::default();
        assert!(tail.is_empty());
        tail.push(String::new());
        tail.push("   ".to_owned());
        assert!(tail.is_empty(), "纯空白行不得让 stderr 显得非空");
        for i in 0..(STDERR_TAIL_LINES + 10) {
            tail.push(format!("line{i}"));
        }
        assert!(!tail.is_empty());
        assert_eq!(tail.iter().count(), STDERR_TAIL_LINES);
        assert_eq!(tail.iter().next(), Some("line10"));
    }

    #[test]
    fn stderr识别出stdin被拒的实测原文() {
        let mut tail = StderrTail::default();
        tail.push(
            "Error parsing streaming input line: this is not json: SyntaxError: Unexpected token"
                .to_owned(),
        );
        assert!(tail.saw_stdin_reject());
    }

    // ── 退出归因（模块文档四）────────────────────────────────────────────────

    #[test]
    fn 退出归因_我们杀的优先于一切() {
        let tail = StderrTail::default();
        assert_eq!(
            attribute_exit(Some(1), true, &tail, None),
            ExitAttribution::Killed
        );
    }

    #[test]
    fn 退出归因_零退出码是正常() {
        let tail = StderrTail::default();
        assert_eq!(
            attribute_exit(Some(0), false, &tail, None),
            ExitAttribution::Normal
        );
        // 取不到退出码（被信号杀）同样不编原因。
        assert_eq!(
            attribute_exit(None, false, &tail, None),
            ExitAttribution::Normal
        );
    }

    #[test]
    fn 退出归因_stdin被拒优先于通用stderr分支() {
        let mut tail = StderrTail::default();
        tail.push("some other noise".to_owned());
        tail.push(format!("{STDIN_REJECT_MARKER}: bad: SyntaxError"));
        assert_eq!(
            attribute_exit(Some(1), false, &tail, None),
            ExitAttribution::StdinRejected { code: 1 }
        );
    }

    #[test]
    fn 退出归因_stderr非空时引用尾部() {
        let mut tail = StderrTail::default();
        tail.push("boom".to_owned());
        let got = attribute_exit(Some(2), false, &tail, None);
        assert_eq!(
            got,
            ExitAttribution::FromStderr {
                code: 2,
                tail: vec!["boom".to_owned()],
            }
        );
    }

    #[test]
    fn 退出归因_退出码1加空stderr必须回查最后一条result() {
        // 这就是实测的认证失败路径：exit=1、stderr 完全为空，
        // 失败信息只在 stdout 事件流里（terminal_reason=api_error / api_error_status=403）。
        use lumen_protocol::llm::LlmTerminalReason;
        let tail = StderrTail::default();
        let outcome = LlmTurnOutcome {
            is_error: true,
            terminal_reason: Some(LlmTerminalReason::ApiError),
            api_error_status: Some(403),
        };
        let got = attribute_exit(Some(1), false, &tail, Some(&outcome));
        let ExitAttribution::FromLastResult { code, outcome: got } = got else {
            panic!("stderr 为空 + 非零退出码必须走回查 result 的分支，实际 {got:?}");
        };
        assert_eq!(code, 1);
        assert!(got.is_error);
        assert_eq!(got.api_error_status, Some(403));
    }

    #[test]
    fn 退出归因_什么都没有时诚实地说不知道() {
        let tail = StderrTail::default();
        assert_eq!(
            attribute_exit(Some(1), false, &tail, None),
            ExitAttribution::Unattributable { code: 1 }
        );
    }

    // ── 状态 ──────────────────────────────────────────────────────────────────

    #[test]
    fn 忙状态口径覆盖等待权限() {
        assert!(RunnerState::Busy.is_working());
        assert!(RunnerState::AwaitingPermission.is_working());
        assert!(!RunnerState::Idle.is_working());
        assert!(!RunnerState::Booting.is_working());
        let exited = RunnerState::Exited {
            code: Some(1),
            killed: false,
        };
        assert!(!exited.is_working());
        assert!(exited.is_exited());
    }
}
