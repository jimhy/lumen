//! **多 CLI 适配层**：把 headless CLI 的私有协议翻译成 [`super::event::RunnerEvent`]。
//!
//! 本模块只定义接口与共享工具，**不含任何一家 CLI 的实现**（Claude 分支在片 3 的
//! `llm_runner/claude.rs`）。
//!
//! # 接口边界：吃什么、吐什么
//!
//! ```text
//!            主线程                                     读线程
//!   ┌──────────────────────────┐              ┌──────────────────────────┐
//!   │ LlmAgentEncoder          │              │ LlmAgentDecoder          │
//!   │  吃: &str / ControlCmd   │              │  吃: &LineEnvelope + &str│
//!   │  吐: StdinLine ──────────┼─► stdin      │  吐: Vec<RunnerEvent>    │
//!   │  错: EncodeError         │              │  错: DecodeError         │
//!   └──────────────────────────┘              └──────────────────────────┘
//!                 └──────── Arc<Mutex<在途表>> ────────┘
//!                        （control_request 的成败关联）
//! ```
//!
//! ## 为什么拆成 Encoder / Decoder 两半（设计蓝图 §6.9 缺陷修正 ②③）
//!
//! 初稿是一个 `LlmAgentAdapter` 由主线程持有、读线程调 `adapter.clone_boxed()` 复制一份。
//! 两个致命问题：
//!
//! 1. trait 上根本没有 `clone_boxed`，**编译不过**；
//! 2. 更严重的是它会把「在途 `control_request` 表」复制成两份——主线程 `encode_control`
//!    往自己那份 `insert`，读线程 `decode_line` 从**另一份** `remove`，**恒为 `None`**，
//!    所有 control 请求的成败关联永久丢失，而且不报任何错。
//!
//! 修正：适配器拆成**主线程独占的 Encoder** 与**读线程独占的 Decoder**，二者共享的在途表由
//! 实现方自己放进 `Arc<Mutex<…>>`（本模块不规定形状——不同 CLI 的 control 通道差异很大，
//! 硬塞一个通用表只会逼所有人绕开它）。`LlmAgentAdapter::split` 是这两半的**唯一**出生入口，
//! 于是"忘了共享"在实现时就很难写错。
//!
//! ## 为什么 `Decoder` 拿到的是 `&LineEnvelope` + `&str` 而不是 `serde_json::Value`
//!
//! 一行 12 MB 的 `tool_result` 解析成 `Value` 是几百万次小分配；而适配器实际只需要其中
//! 若干字段。让适配器**用自己的 `#[derive(Deserialize)]` 结构体直接反序列化**，
//! 既快又天然只物化白名单内的字段——"宽进严出"里的"严出"因此不必靠自觉。
//! 代价是信封被解析两次（一次拿 `type`，一次拿正文）；两次 serde 扫描远比一次
//! `Value` 建树便宜。

use std::path::PathBuf;

use lumen_protocol::llm::{
    BlockId, LlmAgentFeatures, LlmAgentKind, LlmAttachment, LlmPermissionMode, LlmText,
};
use serde::Serialize;

use super::event::RunnerEvent;
use super::RunnerError;

// ── §6.4 stdin 铁律：写进类型里，不写在注释里 ────────────────────────────────

/// **一整行 stdin 消息**——`llm_runner` 里唯一能写进子进程 `stdin` 的东西。
///
/// # 这个类型存在的全部理由（设计蓝图 §6.4，"全设计最硬的一条约束"）
///
/// > **实测**：`printf 'this is not json\n'` 喂进去，CLI **立刻 exit 1** 并在 stderr 打
/// > `Error parsing streaming input line: this is not json: SyntaxError: …`。
/// > 也就是说**一个坏字节 = 会话猝死**。
/// >
/// > **反向实测**：未知的顶层 `type`（如 `{"type":"lumen_unknown_type"}`）被**静默忽略、
/// > 进程存活**，所以前向兼容的能力探测是安全的。
///
/// 三条实现纪律，本类型逐条在**类型/API 层面**执行，而不是靠代码评审记得：
///
/// 1. **内容一律经 `serde_json::to_vec`**——[`Self::from_json`] 是本类型**唯一**的构造函数，
///    且只接受 `impl Serialize`。手拼字符串在模块外**根本写不出来**（内层 `Vec<u8>` 私有，
///    没有 `From<String>` / `From<&str>` / `new(bytes)` 任何入口）。
///    serde 保证用户正文里的换行被转义成 `\n` 两个字符而不会变成物理换行——手机端多行 prompt
///    会直接踩这个坑，手拼字符串遇到一个引号或换行就是一次会话猝死。
/// 2. **每条消息一个完整 `Vec<u8>`，末尾自带且只带一个 `\n`**——由构造函数追加，
///    调用方拿不到"没加换行"的实例。
/// 3. **单写线程独占 `ChildStdin`**——这条在 [`super`] 的拓扑上执行：`ChildStdin` 从
///    [`super::proc::ChildProc::spawn`] 出来就直接搬进写线程，管理器**不留副本**，
///    提交 prompt / 权限应答 / interrupt 三个调用点都只能往通道里发 `StdinLine`。
///    多处直写导致"某次 `write` 半途返回 = 半行 = 杀会话"这个 bug 因此写不出来。
///
/// # `Debug` 脱敏
/// 只打字节数。stdin 行里装的正是用户 prompt（密码、密钥、私有代码的重灾区），
/// 一条 `log::debug!("{line:?}")` 就会把它原样写进日志文件。
#[derive(Clone, PartialEq, Eq)]
pub struct StdinLine(Vec<u8>);

impl StdinLine {
    /// **唯一**的构造入口：序列化 → 校验无物理换行 → 追加一个 `\n`。
    ///
    /// # Errors
    /// - [`EncodeError::Serialize`]：`value` 的 `Serialize` 实现报错（自定义实现才可能）。
    /// - [`EncodeError::EmbeddedNewline`]：序列化结果里出现了**物理**换行。
    ///   正常 serde 输出不可能有（字符串里的换行会被转义），这条断言防的是将来有人给某个字段
    ///   用了 `serde_json::value::RawValue` 之类的直通表示——那会绕过转义，把一条消息拆成两行，
    ///   后半行必然是非法 JSON，会话当场猝死。**宁可这条消息发不出去，也不能杀掉会话。**
    pub fn from_json<T: Serialize + ?Sized>(value: &T) -> Result<Self, EncodeError> {
        // 只 map 掉 serde 错误、**不保留其消息**：serde_json 的错误文本会带实际值。
        let bytes = serde_json::to_vec(value).map_err(|_| EncodeError::Serialize)?;
        Self::from_serialized(bytes)
    }

    /// 换行守卫本体。单独抽出来只为**可测**：正常 serde 输出走不到 `EmbeddedNewline` 分支
    /// （字符串里的换行必被转义），而一条永远测不到的防线等于没有防线。
    fn from_serialized(mut bytes: Vec<u8>) -> Result<Self, EncodeError> {
        if bytes.contains(&b'\n') {
            return Err(EncodeError::EmbeddedNewline);
        }
        bytes.push(b'\n');
        Ok(Self(bytes))
    }

    /// 写线程用。**只有 [`super`] 及其子模块能拿到裸字节**（`pub(super)`），
    /// 外部拿不到也就无从"截一半再写"。
    #[must_use]
    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// 含结尾换行的总字节数（背压与审计计量用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 恒为 `false`（至少有一个 `\n`）。留着只为满足 `clippy::len_without_is_empty`，
    /// 顺带把"空行永不可能出现"这个不变量写成可执行的断言。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl std::fmt::Debug for StdinLine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StdinLine")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// 出站编码失败。**不带任何原文**（同 [`super::event::ProtocolErrorKind`] 的理由：
/// 错误消息会进日志，正文不能跟着进去）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// `Serialize` 实现自身报错。
    Serialize,
    /// 序列化结果含物理换行（见 [`StdinLine::from_json`]）。
    EmbeddedNewline,
    /// 本适配器不支持这条控制指令（调用方静默跳过即可，不是故障）。
    Unsupported,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Serialize => "出站消息序列化失败",
            Self::EmbeddedNewline => "出站消息含物理换行（会拆成两行杀死会话）",
            Self::Unsupported => "本 CLI 不支持该控制指令",
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for EncodeError {}

/// 入站解码失败。
///
/// # 为什么不是设计稿里的 `Result<(), String>`
/// `String` 是个装什么都行的口袋，最顺手的写法就是
/// `format!("bad tool_result: {raw}")`——**上游原文当场进了错误消息**，
/// 而错误消息会进 `log::warn` 与审计日志，白名单铁律在错误路径上破功。
/// 这里的 `at` 是**代码里写死的站点名**（如 `"assistant.message.content"`），
/// 类型上就装不下上游数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError {
    /// 出错的解析站点（`&'static str` ⇒ 必然来自代码，不可能来自上游）。
    pub at: &'static str,
    pub kind: DecodeErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// 字段缺失 / 类型不符。
    Schema,
    /// 形状已知但本端尚未建模（例如 `tool_use_result` 的非 `file` 形态）。
    /// 与 [`Self::Schema`] 分开：前者是"上游变了"，后者是"我们还没做"，排障方向完全不同。
    Unmodeled,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            DecodeErrorKind::Schema => "字段形状不符",
            DecodeErrorKind::Unmodeled => "本端尚未建模的形态",
        };
        write!(formatter, "{kind} @ {}", self.at)
    }
}

impl std::error::Error for DecodeError {}

// ── 启动参数 ──────────────────────────────────────────────────────────────────

/// Tier 2（`PreToolUse` hook 回调）用的环回端点 + 一次性令牌。
///
/// **P0 不产出**：权限审批通道（`can_use_tool`）本身尚未实测（§6.6 ⚠ 行）。这里先留形状，
/// 是为了让 [`LlmAgentAdapter::build_env`] 的签名在 Tier 2 落地时不用改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEndpoint {
    /// 形如 `127.0.0.1:PORT`。**只允许环回地址**——这个端点能驱动工具审批，
    /// 绑到任意网卡等于把审批权开放给局域网。
    pub addr: String,
    /// 一次性令牌（进程级，随会话结束作废）。
    pub token: String,
}

/// 一次会话启动的全部参数。
///
/// # `permission_mode` 为什么是私有字段
/// [`LlmPermissionMode::Bypass`] 在 P0 **不可达**：桌面端与手机端都不提供这个开关
/// （M7 §12-⑫ 拍板，§4.4 / §6.8.4）。把字段设为私有 + 只留一个会拒绝 `Bypass` 的 setter，
/// 意味着"忘了挡 bypass"这件事**编译期就发生不了**——而写在注释里的约束，
/// 半年后加一条 `spec.permission_mode = mode;` 的人不会看见。
///
/// 用户真要开只能自己手改 CLI 的 settings 文件，那条路由 CLI 自行生效、不经本结构体。
/// 那道摩擦力本身就是一层保护。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// 目标 CLI。
    pub agent: LlmAgentKind,
    /// 工作目录（同时是 `--add-dir` 权限围栏）。
    pub workspace: PathBuf,
    /// **Lumen 生成的 uuid**，用 `--session-id` 传进去。
    ///
    /// 进程启动**之前**映射就成立 ⇒ 手机可立刻寻址、崩溃后可确定性 `--resume` 同一个 id。
    /// 不传时 CLI 自己 mint 一个，那要等 `init` 事件才知道——中间那段时间无法寻址。
    pub cli_session_id: String,
    /// 续接已有 CLI 会话。⚠ `--resume` 语义本轮未实测。
    pub resume: bool,
    /// 与 `resume` 配合，映射 `--fork-session`（不覆盖原会话）。
    pub fork_session: bool,
    pub model: Option<String>,
    pub append_system_prompt: Option<LlmText>,
    pub allowed_tools: Option<Vec<String>>,
    /// Tier 2 才用，P0 恒 `None`。
    pub hook_endpoint: Option<HookEndpoint>,
    /// 见类型文档：私有，只能经 [`Self::set_permission_mode`] 写。
    permission_mode: Option<LlmPermissionMode>,
}

impl LaunchSpec {
    #[must_use]
    pub fn new(agent: LlmAgentKind, workspace: PathBuf, cli_session_id: String) -> Self {
        Self {
            agent,
            workspace,
            cli_session_id,
            resume: false,
            fork_session: false,
            model: None,
            append_system_prompt: None,
            allowed_tools: None,
            hook_endpoint: None,
            permission_mode: None,
        }
    }

    #[must_use]
    pub fn permission_mode(&self) -> Option<&LlmPermissionMode> {
        self.permission_mode.as_ref()
    }

    /// 设置权限模式。
    ///
    /// # Errors
    /// 传 [`LlmPermissionMode::Bypass`] 一律返回 [`RunnerError::BypassForbidden`]——
    /// 见类型文档。**未知值（`Other`）同样拒绝**：认不出的权限模式在安全上只能保守解释，
    /// 把它原样传给 CLI 等于让上游替我们决定放行范围。
    pub fn set_permission_mode(&mut self, mode: LlmPermissionMode) -> Result<(), RunnerError> {
        if mode == LlmPermissionMode::Bypass {
            return Err(RunnerError::BypassForbidden);
        }
        if !mode.is_known() {
            return Err(RunnerError::UnknownPermissionMode);
        }
        self.permission_mode = Some(mode);
        Ok(())
    }
}

/// 主线程发给写线程的控制指令（会被 [`LlmAgentEncoder::encode_control`] 编码成 stdin 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCmd {
    /// 握手（Claude: `control_request{subtype:"initialize"}`，已手工 round-trip 实测）。
    Initialize,
    /// 中途打断（Claude: `control_request{subtype:"interrupt"}`，已实测）。
    Interrupt,
    /// 改权限模式（Claude: `control_request{subtype:"set_permission_mode"}`，已实测）。
    SetPermissionMode(LlmPermissionMode),
    /// 权限审批应答。⚠ `can_use_tool` 通道未实测。
    PermissionReply {
        /// **CLI 侧**的请求 id（字符串）。
        request_id: String,
        allow: bool,
        /// 拒绝理由（可选，会回给模型）。
        reason: Option<LlmText>,
    },
}

// ── 三个 trait ────────────────────────────────────────────────────────────────

/// 一个 CLI 适配器的**静态描述 + 运行时两半的工厂**。
///
/// 实现体是无状态的（只描述"这个 CLI 长什么样"），故要求 `Send + Sync`：
/// 管理器持有一份 `&'static dyn LlmAgentAdapter` 或 `Arc<dyn …>`，每起一个会话调一次
/// [`Self::split`] 产出该会话专属的编解码两半。
pub trait LlmAgentAdapter: Send + Sync {
    fn kind(&self) -> LlmAgentKind;

    /// 可执行文件名（**走 PATH 查找，不写死绝对路径**）。
    ///
    /// 未找到时按**永久禁用**处理，参照 `completion_sidecar.rs:294-299` 对 pwsh 缺失的做法：
    /// 每次请求都白 spawn 一次 + 刷一条日志，比"启动一次、记住、以后直接拒"糟糕得多。
    fn program(&self) -> &'static str;

    /// 命令行参数。实现见片 3。
    fn build_args(&self, spec: &LaunchSpec) -> Vec<String>;

    /// **追加**的环境变量（Tier 2 的 hook 端点由此下发，hook 子进程继承 CLI 环境即可读到）。
    fn build_env(&self, spec: &LaunchSpec) -> Vec<(String, String)>;

    /// **能力探测的兜底值**——真正的能力位应由 `system/init` 行的 `capabilities` 数组推出
    /// （见 [`super::event::StartedInfo::features`]）。本方法只给"还没收到 `init`"那段时间
    /// 用一个保守的初值。
    ///
    /// **保守的含义是"少报"不是"多报"**：多报一个 `interrupt` 会让手机端画出一个按下去
    /// 没有任何反应的中断按钮，那比没有按钮更糟。
    fn baseline_features(&self) -> LlmAgentFeatures;

    /// 产出该会话专属的编解码两半。**共享状态（如在途 control 表）由实现方在此建立并
    /// 分别 `Arc::clone` 进两边**，见模块文档缺陷修正 ②③。
    fn split(&self) -> (Box<dyn LlmAgentEncoder>, Box<dyn LlmAgentDecoder>);
}

/// 出站编码器。**主线程独占**（`&mut self`，可变状态如自增的 `request_id` 计数器放这里）。
pub trait LlmAgentEncoder: Send {
    /// 把一条用户消息编码成**一整行**。
    ///
    /// `attachments` 只传**被控端本机绝对路径**（字节内容走 `PutBegin` 通道另行上传，
    /// 见 `LlmAttachment` 文档）；实现方把路径写进提示词让 CLI 自己读文件，与桌面端
    /// `llm_attachments.rs` 的 `compile_prompt` 同一套路。
    ///
    /// # Errors
    /// 见 [`EncodeError`]。**编码失败绝不能降级成"发个差不多的"**——非法行会直接杀死 CLI。
    fn encode_user_message(
        &mut self,
        text: &str,
        attachments: &[LlmAttachment],
    ) -> Result<StdinLine, EncodeError>;

    /// 编码一条控制指令。不支持的返回 [`EncodeError::Unsupported`]（调用方静默跳过）。
    ///
    /// # Errors
    /// 见 [`EncodeError`]。
    fn encode_control(&mut self, cmd: &ControlCmd) -> Result<StdinLine, EncodeError>;

    /// 撤销上一次 [`Self::encode_user_message`] 对**本实现自己那份**轮号计数器的推进。
    ///
    /// # 为什么需要这个方法
    /// 轮号有两份计数器：`LlmRunner.current_turn` 与实现方自己的（Claude 是
    /// `ClaudeEncoder.turn`），靠「[`super::LlmRunner::submit`] 是唯一调用点、成对推进」保持
    /// 同步。而 `submit` 在**编码成功但送不进 stdin 队列**时必须把两边一起退回去——CLI 根本没
    /// 收到这条消息，留着这个轮号只会在手机端的 turn 序列上留一个永远没有内容的空洞。
    ///
    /// 默认**空实现**：不自己维护轮号的适配器（占位适配器、以及任何直接读 `current_turn` 的
    /// 未来实现）无需覆写。覆写它的唯一判据是「`encode_user_message` 里有没有 `fetch_add`」。
    fn rollback_turn(&mut self) {}
}

/// 入站解码器。**读线程独占**（`&mut self`）。
pub trait LlmAgentDecoder: Send {
    /// 解码一整行。
    ///
    /// `env` 是 [`super::event::classify`] 已经解出并**判过白名单**的信封，
    /// `raw` 是同一行的原文（实现方按自己的结构体二次反序列化）。
    ///
    /// # Errors
    /// 返回 `Err` 表示这一行没法理解。调用方会发一条
    /// [`super::event::ProtocolErrorKind::DecodeFailed`] 并**继续读下一行**——
    /// **绝不断流**（§6.3 理由 3：按行切的全部价值就是每行都是一个重同步点）。
    ///
    /// 注意：返回 `Err` **之前已经 push 进 `out` 的事件仍然有效**，调用方不会回滚。
    /// 一行里有 3 个 content block、第 2 个解不动时，正确做法是把第 1、3 个照常产出。
    fn decode_line(
        &mut self,
        env: &super::event::LineEnvelope<'_>,
        raw: &str,
        out: &mut Vec<RunnerEvent>,
    ) -> Result<(), DecodeError>;
}

// ── 共享工具：轮号 / 块号游标 ────────────────────────────────────────────────

/// 「轮号 + 块号」游标——给 [`LlmAgentDecoder`] 实现复用。
///
/// # 它解决设计蓝图 §6.9 缺陷修正 ④ 的那个坑
/// 初稿在读线程里写死 `DecodeCtx { current_turn: 0, .. }`，而 `next_turn_id` 是**主线程独占
/// 字段** ⇒ 整套"turn / block 骨架"在实现里失效（`turn_id` 恒为 0，所有轮的块混成一堆）。
/// 修正：轮号由主线程 `submit()` 推进、放 `Arc<AtomicU64>`，读线程**只读**。
///
/// # 块号必须**按轮重置**
/// 协议侧 [`BlockId`] 的不变量是「跨轮重新从 0 起，`(turn, block_id)` 才唯一」，且
/// **同一轮内 user 与 assistant 共用一个编号空间**——否则助手的首个流式块会静默覆盖用户气泡
/// （`llm.rs:150-157` 把这个坑写得很细）。本游标把"轮变了就重置"做成唯一入口，
/// 让实现方不必各自记一份 `last_turn`。
#[derive(Debug, Default)]
pub struct TurnCursor {
    last_turn: u64,
    next_block: BlockId,
}

impl TurnCursor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 取当前轮的下一个块号；`turn` 与上次不同则先重置块号。
    pub fn next_block_id(&mut self, turn: u64) -> BlockId {
        if turn != self.last_turn {
            self.last_turn = turn;
            self.next_block = 0;
        }
        let id = self.next_block;
        // 饱和而不是回绕：一轮 4 294 967 295 个块是不可能的，但真撞上时"最后一个块号被反复
        // 复用"远比"块号回绕到 0 覆盖掉本轮开头"温和。
        self.next_block = self.next_block.saturating_add(1);
        id
    }

    /// 当前游标停在哪一轮（诊断用）。
    #[must_use]
    pub fn turn(&self) -> u64 {
        self.last_turn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Msg<'a> {
        #[serde(rename = "type")]
        ty: &'a str,
        text: &'a str,
    }

    #[test]
    fn stdin行自带且只带一个结尾换行() {
        let line = StdinLine::from_json(&Msg {
            ty: "user",
            text: "hello",
        })
        .expect("可序列化");
        let bytes = line.as_bytes();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(line.len(), bytes.len());
        assert!(!line.is_empty());
    }

    #[test]
    fn 多行prompt里的换行被serde转义_不会拆成两行() {
        // 这正是手机端多行 prompt 会踩的坑：手拼字符串时物理换行会把一条消息拆成两行，
        // 后半行必然是非法 JSON ⇒ CLI 立刻 exit 1。
        let line = StdinLine::from_json(&Msg {
            ty: "user",
            text: "第一行\n第二行\r\n第三行",
        })
        .expect("可序列化");
        let bytes = line.as_bytes();
        assert_eq!(
            bytes.iter().filter(|b| **b == b'\n').count(),
            1,
            "正文里的换行必须被转义成 \\n 两个字符"
        );
        assert!(!bytes[..bytes.len() - 1].contains(&b'\r'));
        // 反序列化回来内容不变。
        let text = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        let back: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(back["text"], "第一行\n第二行\r\n第三行");
    }

    #[test]
    fn 正文里的引号与反斜杠不会破坏行结构() {
        let line = StdinLine::from_json(&Msg {
            ty: "user",
            text: r#"他说"rm -rf C:\" 然后走了"#,
        })
        .expect("可序列化");
        let bytes = line.as_bytes();
        let text = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(text).is_ok());
    }

    #[test]
    fn 含物理换行的序列化结果被拒而不是杀会话() {
        // 模拟"将来有人给某字段用了 RawValue / 自定义 Serializer"：输出里出现物理换行。
        // 走不到这条分支不代表可以不要它——一条永远测不到的防线等于没有防线。
        let err = StdinLine::from_serialized(b"{\n\"a\":1}".to_vec()).expect_err("必须被拒");
        assert_eq!(err, EncodeError::EmbeddedNewline);
        // 正常字节照常通过并补上唯一的换行。
        let ok = StdinLine::from_serialized(b"{\"a\":1}".to_vec()).expect("合法");
        assert_eq!(ok.as_bytes(), b"{\"a\":1}\n");
    }

    #[test]
    fn stdin行的debug只打字节数不打正文() {
        let line = StdinLine::from_json(&Msg {
            ty: "user",
            text: "sk-ant-super-secret",
        })
        .expect("可序列化");
        let rendered = format!("{line:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("bytes"));
    }

    #[test]
    fn bypass权限模式在api层被拒() {
        let mut spec = LaunchSpec::new(
            LlmAgentKind::Claude,
            PathBuf::from("F:\\ws"),
            "sid".to_owned(),
        );
        assert!(matches!(
            spec.set_permission_mode(LlmPermissionMode::Bypass),
            Err(RunnerError::BypassForbidden)
        ));
        assert!(spec.permission_mode().is_none(), "被拒后不得留下副作用");
        // 未知值同样拒。
        assert!(matches!(
            spec.set_permission_mode(LlmPermissionMode::from_wire_clamped("brand_new_mode")),
            Err(RunnerError::UnknownPermissionMode)
        ));
        // 正常值放行。
        spec.set_permission_mode(LlmPermissionMode::AcceptEdits)
            .expect("acceptEdits 必须放行");
        assert_eq!(
            spec.permission_mode(),
            Some(&LlmPermissionMode::AcceptEdits)
        );
    }

    #[test]
    fn 块号按轮重置且轮内单调() {
        let mut cursor = TurnCursor::new();
        assert_eq!(cursor.next_block_id(1), 0);
        assert_eq!(cursor.next_block_id(1), 1);
        assert_eq!(cursor.next_block_id(1), 2);
        // 换轮 → 从 0 重新起（`(turn, block_id)` 才唯一）。
        assert_eq!(cursor.next_block_id(2), 0);
        assert_eq!(cursor.next_block_id(2), 1);
        assert_eq!(cursor.turn(), 2);
        // 回到旧轮同样重置（乱序不该产出重复编号之外的怪异行为）。
        assert_eq!(cursor.next_block_id(1), 0);
    }
}
