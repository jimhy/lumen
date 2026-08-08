//! M7 移动端 LLM 远程控制：headless LLM CLI 子进程的**结构化对话数据面**。
//!
//! 与 M5.3 终端镜像「传 VT 字节、控制端喂 `Terminal::advance` 复现整屏」不同，本子协议传的是
//! **归一化的结构化对话**：被控端(PC)把 `claude -p --output-format stream-json` 一类 headless CLI
//! 的事件流解析成与厂商无关的 [`LlmBlock`]，控制端(手机)据此渲染气泡流（用户消息 / 助手回复 /
//! 工具调用卡片），**不渲染任何终端画面**。
//!
//! # ⚠ 铁律一：PC → 手机必须是「白名单转发」，绝不是「黑名单过滤」或原样透传
//!
//! 这条是**安全硬约束**，不是风格偏好，来自 2026-08-08 本机对 Claude Code CLI 2.1.226 的两次实测
//! （脱敏样本见 `docs/调研/M7-claude-headless-stream-json-实测样本-2026-08-08.jsonl` = 样本 A
//! 失败路径，与 `…-实测样本B-工具调用-2026-08-08.jsonl` = 样本 B 成功路径）：
//!
//! - headless 的 stdout 事件流上会出现 `system.subtype = "hook_started"` / `"hook_response"`。
//!   实测其中一条 `hook_response` 的 **`stdout` 字段**携带了 **4106 字符**（6976 UTF-8 字节）的私人
//!   记忆库全文——用户配置的任意 `SessionStart` / `PreToolUse` hook 的 stdout 都会原样出现在这里，
//!   可能含密钥、私人笔记、内网地址。
//!   （**字段与单位都别记错**：全文在 `stdout`，同一行的 `output` 是**另一个独立字段**、只装短摘要
//!   （样本 A 脱敏后只剩 75 字符占位符，样本 B 同类行实测 42 字符）；`4106` 是**字符数**不是字节数。
//!   拿 `output` 去找那 4106 字符会找不到，进而怀疑整条铁律。）
//!   （**脱敏本身也踩过黑名单的坑**：样本 A 初次脱敏只替换了 `output`——那个短摘要——漏了同一行真正
//!   装着全文的 `stdout`；「列举要挡的东西」这条路连人工做都会漏，何况代码。）
//! - `system.subtype = "init"` 一行就带走大量**本机环境信息**：`memory_paths` 绝对路径、40 个工具名、
//!   `plugins` / `mcp_servers` / `slash_commands` / `agents` / `skills` 清单。
//! - 样本 B 又添两条**公开 Messages API 里根本没有**的 Claude Code 扩展字段：`user` 行顶层的
//!   `tool_use_result`（实测 `file.content` 装着 11 720 字符的**文件正文**）与 `assistant` 行顶层的
//!   `request_id`、`message.content[].caller`。它们的存在正是「先解析、不认识的原样中继」必然漏内容
//!   的证据——本模块对前者只提取定长元数据（见 [`LlmToolResultDetail`]），对后两者**一律不上行**
//!   （理由分别写在 [`LlmBlock::ToolUse`] 与 [`LlmTurnOutcome`] 的文档里）。
//!
//! 若被控端「先解析、遇到不认识的就原样中继」，上面这些**必然**经中继服务器流到手机。故：
//!
//! 1. 被控端只允许把**本模块显式定义的字段**填进帧后上行；CLI 事件里任何未被本模块建模的东西
//!    **一律就地丢弃**，不得以「先传上去、手机端再决定显不显示」为由放行。
//! 2. **本模块在类型上就不给透传留口子**：全模块**没有** `Raw(serde_json::Value)` 这类直通变体，
//!    也没有 `extra: Map<String, Value>` 这类尾巴字段。唯一的 [`serde_json::Value`] 是
//!    [`LlmToolInput`]（仅承载**工具入参**，见其文档），除此之外任何新增 `Value` 字段都应视为
//!    对本铁律的破坏，评审时直接打回。
//! 3. 开放式 C 型枚举（[`LlmStopReason`] 等）的 `Other(String)` 只承载**短机器可读标识**，被控端
//!    填充前必须按 [`LLM_ENUM_WIRE_MAX_BYTES`] 夹紧，**严禁**把自由文本 / 日志 / hook 输出塞进去
//!    当「反正保留原文」的后门。
//! 4. [`LlmAgentInfo`] **刻意不含** `tools` / `plugins` / `mcp_servers` / `memory_paths` 字段——
//!    手机端渲染工具卡片所需的工具名，随 [`LlmBlock::ToolUse`] 的 `name` 自然到达，不需要也不允许
//!    预先上行全量清单。同理 [`LlmUsage`] 只留归一化计数，上游 `usage` 里的 `service_tier` /
//!    `inference_geo` / `iterations` 一律不上行。
//!
//! # 铁律二：错误不是普通文本
//!
//! 实测**认证失败不是一条独立事件**，而是一条**普通 `assistant` 消息 + 普通 `text` 块**
//! （正文 `"Failed to authenticate. API Error: 403 Request not allowed"`），只有顶层
//! `is_api_error_message = true` 与 `error = "authentication_failed"` 能把它和正常回复区分开。
//! 朴素解析器会把它渲染成模型的正常气泡。被控端**必须先判 `is_api_error_message` 再决定块类型**，
//! 详见 [`LlmBlock::Error`]。
//!
//! 同理一轮的成败**只能**看 `result` 行的 `is_error`：实测复现了 `subtype = "success"` 与
//! `is_error = true` 同时出现（样本 A），而样本 B 的成功路径 `subtype` **仍然是 `"success"`**——
//! 两次采样同一个值、成败相反，`subtype` 的判据价值为零。详见 [`LlmTurnOutcome`]。
//!
//! # 铁律三：思考是**状态信号**，不是内容
//!
//! 样本 B 实测：Claude Opus 5 的 `thinking` 块**有 `signature`（456 字符不透明 base64）而
//! `thinking` 字段恒为空串**——模型默认把思考内容 `omitted`，而 `claude --help` 里**没有任何开关**
//! 能打开它（`--effort` / `--forward-subagent-text` 都不是）。
//!
//! 产品后果是硬的：手机端「可折叠思考块」在 Claude 上**永远展不开**。故本模块把思考建模成
//! [`LlmThinking`] 这个**互斥三态**而不是「一个可能为空的文本字段」——`Omitted` 分支在类型上就
//! 没有可绑定的文本，UI 想依赖也依赖不上；等哪天 CLI 真给了 summarized 文本，被控端改发
//! [`LlmThinking::Text`] 即可，协议零改动。能力位见 [`LlmAgentFeatures::thinking_text`]。
//!
//! # 为什么整套子协议装在一个 [`crate::remote::RemoteFrame::Llm`] 变体里
//!
//! [`crate::remote::RemoteFrame`] 是 serde 默认的 **externally tagged** 枚举（变体名作为 JSON 对象
//! 唯一 key），serde **不允许**在这种表示上使用 `#[serde(other)]` 兜底变体，因此老端收到任何未知
//! 变体名都是整帧 `from_value` 失败 → 丢弃（表现为功能静默不生效）。
//!
//! 本模块的 [`LlmFrame`] 改用 **internally tagged**（`#[serde(tag = "op")]`）并带
//! `#[serde(other)] Unknown`：**未知 `op`（哪怕带任意嵌套 body）会降级成 [`LlmFrame::Unknown`]
//! 而不是让整帧报废**。于是「新增变体炸老对端」的代价**只付一次**（`RemoteFrame::Llm` 这一个变体
//! 对现存 v3 桌面端仍不可识别，由 [`LlmFrame::Hello`] 握手超时兜底提示升级），此后 LLM 子协议
//! 无论怎么长都永久前向兼容，且服务端与 QUIC 通路零改动。
//!
//! 同一套 `other` 兜底**下沉到每一层可增长的类型**（[`LlmBlock`] / [`LlmDeltaItem`] /
//! [`LlmToolStatus`] / [`LlmConvState`] / [`LlmPermissionDecision`] / [`LlmPermissionResolvedBy`]），
//! 老端收到未来内容时只降级**那一项**，同帧其余内容照常解析。
//!
//! **C 型枚举是这套兜底救不回来的一层**：它们在线上就是一个裸字符串（`"EndTurn"`），
//! `#[serde(other)]` 用不上。故一律走本模块的 `open_enum!` 宏（`untagged` + `Other(String)`）。
//! 若某个 C 型字段忘了用宏而直接 `derive`，新端发来的未知值会让**整帧**解析失败被丢弃——
//! 这是 `LlmFrame` 的 `other` 兜不住的。
//!
//! **且这层兜底只保未知「值」、不保「形状」变更**：实测 `{"op":"TurnEnded", …, "stop": 42}` →
//! `Err("data did not match any variant of untagged enum LlmStopReason")`，**整帧报废**，
//! [`LlmFrame::Unknown`] 也救不回来。这不是纯理论——同模块的 [`LlmToolStatus`] / [`LlmConvState`]
//! 走的正是 `#[serde(tag = "kind")]` 的对象形态，说明「某个枚举日后从裸字符串升级成带附加字段的
//! 对象」在本模块内就有先例。故 **C 型枚举字段的线格式一旦定为裸字符串就不得再改形状**；
//! 确需附加信息时**另开一个 `#[serde(default)]` 兄弟字段**（[`LlmTurnOutcome`] 对 `stop` 就是
//! 这么做的：`terminal_reason` / `api_error_status` 是新字段，没有去动 `stop` 的形状），
//! 不要动原字段。
//!
//! # 保序与自愈（对齐 part3d D3）
//!
//! - [`LlmFrame::Attached`] / [`LlmFrame::ConvStarted`] / [`LlmFrame::TurnSnapshot`] /
//!   [`LlmFrame::HistoryPage`] 是**基线帧**，必先于该对话任何 [`LlmFrame::Delta`] 到达，且携带
//!   `seq`（发出基线那一刻的水位）。
//! - 控制端**丢弃 `seq` 不大于基线的迟到 `Delta`**。这条规则把 M6 QUIC 直连与 WS 中继翻转瞬间的
//!   乱序**自动收敛**（两条通路无全局序），无需任何屏障机制。
//! - `Delta.seq` 在同一 `(conv_id, conv_generation)` 内**严格递增且连续**；控制端一发现断裂就发
//!   [`LlmFrame::TurnFetch`] 拉整轮定稿快照覆盖重建。
//! - **诚实提示**：这条自愈**协议本身没有强制机制**（没有屏障、没有序号校验），全靠两端实现自觉——
//!   与现有「`SubscriptionStarted` 必先于 `OutputWithId`」同类风险。若被控端某条路径忘了在基线帧里
//!   填正确 `seq`，表现是手机上气泡文字重复或缺段，且只在直连切换瞬间偶现，极难复现。
//!
//! # 与桌面端的关系（M7 §12 ⑪⑫ 拍板）
//!
//! 手机侧的 LLM 对话是**隐藏会话**：它不是 Tab / 窗格，桌面端不为它开任何会话 UI，只用一个图标
//! 标识「当前有手机会话在跑」。这正是 [`ConvId`] 必须与 `(TabId, SessionId)` **完全正交**的另一条
//! 理由——它根本不进 app 的窗格账本。
//!
//! 桌面端不做会话 UI 的代价是「手机远程让 PC 跑 Bash」在桌面端没有可见记录，故**审计必须落在
//! 被控端的结构化日志上**：本协议已把审计条目所需的最小事实都摆在类型里——谁（[`LlmConvMeta`] 的
//! `agent` / `cwd` / `created_ms`）、哪一轮（[`LlmFrame::TurnStarted`] 的 `turn` / `started_ms`）、
//! 跑了什么（[`LlmBlock::ToolUse`] 的 `name` / `title`）、批没批（[`LlmFrame::PermissionResolved`]
//! 的 `decision` / `by`）。被控端落审计日志时**必须只记这些字段**，不得整帧 `{frame:?}` 落盘——
//! 那会把正文与工具入参一起写进磁盘，本模块的 `Debug` 脱敏（见 [`LlmText`] / [`LlmPath`] /
//! [`LlmToolInput`]）正是为此兜底。

use serde::{Deserialize, Serialize};

// ── id 体系 ───────────────────────────────────────────────────────────────────

/// 对话唯一标识。**由被控端(PC)分配**（会话所有权在 PC：子进程、transcript、附件目录都在 PC；
/// PC 重启后控制端必须以 PC 的清单为准），自增 `u64`、关闭后不复用。
///
/// **与 `(TabId, SessionId)` 完全正交、独立 id 空间**：LLM 对话不是终端窗格，绝不复用
/// [`crate::remote::SessionId`]——app 侧离屏纹理回收、`pane_textures`、附件纹理键
/// `(session_id, attachment_id)` 全部按 `SessionId` 挂账，共用 id 空间会静默串台 / 误删纹理。
///
/// # 分配方必须把取值留在 `i64::MAX` 以内
/// 类型是 `u64`，但**控制端是 Dart，`int` 是 64 位有符号**：`jsonDecode` 遇到超过 `i64::MAX`
/// 的字面量会退化成 `double` 并丢精度，两端从此静默不一致。自增计数器实际撞不到 2^63，
/// 但这条约束属于**分配方**（被控端 `crates/lumen-app/src/llm_runner/`），必须写在类型旁边而不是
/// 只写在测试里——`tests/mobile_golden.rs` 的 `语料自身的硬规矩` 有一条 lint 对**语料**执行它，
/// 但那条 lint 管不到运行时真分配出来的值。同样适用于 [`ConvGeneration`] 与各处 `seq` /
/// `cost_micro_usd`。
pub type ConvId = u64;

/// 对话代（generation）。被控端每次**重建**该对话的运行时（进程重启 / 恢复 / 换 CLI）即换新值。
///
/// 照抄 v3 内置编辑器 `session_generation` 的陈旧应答守卫：控制端重挂、换对端、或被控端重启后，
/// 迟到的旧代应答必须被丢弃，否则会落到新对话上。**收到的 `conv_generation` 不等于本地记录的帧
/// 一律丢弃**（不是「尽量丢」——这条是不变量，漏一处就串台）。
///
/// 取值同样必须留在 `i64::MAX` 以内，理由见 [`ConvId`]。
pub type ConvGeneration = u64;

/// 轮次号（一次用户提交 → 一次停止 为一轮）。同一对话内自 1 起自增、不复用；`0` 是「尚无任何一轮」
/// 的哨兵（[`LlmConvMeta::cur_turn`] 用）。
pub type TurnNo = u32;

/// 块在**本轮内**的序号。跨轮重新从 0 起——**轮号 + 块号才唯一**，只拿 `block_id` 当 key 会在
/// 下一轮静默覆盖上一轮的块。
///
/// **同一轮内 [`LlmTurnRecord::user`] 与 [`LlmTurnRecord::assistant`] 共用同一编号空间**，
/// `(turn, block_id)` 全局唯一。这条是不变量，不是建议：[`LlmDeltaItem::TextAppend`] /
/// `BlockEnd` / `Dropped` 只带 `block_id` 一个键，控制端归约时用 `HashMap<BlockId, _>` 是最自然
/// 的写法——一旦被控端按「user 从 0 编、assistant 也从 0 编」实现，助手的首个流式块就会**静默
/// 覆盖用户气泡**，而且只在「用户消息 + 助手立刻开始流式」这个最常见路径上出现。
pub type BlockId = u32;

// ── 脱敏包装（对齐 remote.rs 的 EditPath / EditChunk 范式）─────────────────────

/// 对话正文 / 工具输出等**敏感文本**。JSON 上就是普通字符串，`Debug` 永远只打字符数。
///
/// LLM 对话正文是隐私重灾区（密码、密钥、私有代码都可能出现在 prompt 里）。这属**预防性**设计：
/// `remote_ws.rs` 当前没有整帧 `{frame:?}` 日志，但只要将来有人加一条，裸 `String` 就会把正文
/// 原样写进日志文件——与 `EditPath` 当初的动机完全一致。
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LlmText(String);

impl LlmText {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// 追加（合并器把同一块的连续 [`LlmDeltaItem::TextAppend`] 拼成一条时用，见
    /// [`LLM_DELTA_FLUSH_MS`]）。
    pub fn push_str(&mut self, s: &str) {
        self.0.push_str(s);
    }

    /// UTF-8 字节数（流控按字节夹紧，不能按字符数——中文一字 3 字节，按字符会低估 3 倍）。
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for LlmText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlmText")
            .field("chars", &self.0.chars().count())
            .finish()
    }
}

/// 被控端本机路径（cwd / 附件绝对路径）。JSON 上是普通字符串，`Debug` 永远脱敏。
///
/// 路径本身就是环境信息（用户名、项目名、内网盘符都在里面），与 [`crate::remote::EditPath`] 同理。
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LlmPath(String);

impl LlmPath {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for LlmPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LlmPath(<redacted>)")
    }
}

/// 工具入参。**本模块唯一的 [`serde_json::Value`]**——每个工具的 schema 自定义且随上游迭代，
/// 任何归一化都是有损且必然追不上（`Read` 的 `file_path` 对 `Bash` 的 `command`）。故原样透传，
/// 由控制端按 [`LlmBlock::ToolUse`] 的 `name` 决定怎么渲染卡片，未知工具回退成折叠 JSON。
///
/// **它是「工具入参」的口子，不是「任意 JSON」的口子**：只允许承载模型给出的一次工具调用参数，
/// 绝不允许被复用来夹带 CLI 原生事件、hook 输出或 init 环境信息（见模块文档铁律一）。
///
/// 包 newtype 只为一件事：`Debug` 脱敏。`Bash{command}` / `Edit{new_string}` 里可能带密钥，
/// 裸 `serde_json::Value` 一进 `format!("{frame:?}")` 就把密钥原样写进日志——这是设计初稿被
/// 对抗验证点名的漏洞（初稿只包了正文与路径，工具入参裸奔）。
///
/// `Debug` 里的 `bytes` 需要一次序列化，是 `O(n)`；这是诊断路径、非热路径，可接受。
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LlmToolInput(serde_json::Value);

impl LlmToolInput {
    #[must_use]
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn get(&self) -> &serde_json::Value {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> serde_json::Value {
        self.0
    }
}

impl std::fmt::Debug for LlmToolInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields = self.0.as_object().map_or(0, serde_json::Map::len);
        let bytes = serde_json::to_string(&self.0).map_or(0, |s| s.len());
        formatter
            .debug_struct("LlmToolInput")
            .field("fields", &fields)
            .field("bytes", &bytes)
            .finish()
    }
}

// ── 开放式 C 型枚举 ───────────────────────────────────────────────────────────

/// 生成「已知变体 + `Other(String)` 兜底」的开放式枚举。
///
/// serde 的 `#[serde(other)]` 只能用在 internally / adjacently tagged 枚举上，对这种「线上就是
/// 一个裸字符串」的 C 型枚举不可用；用 `#[serde(untagged)]` 先试已知集合、失败落 `Other(String)`，
/// 既保持紧凑线格式，又**保留未知原文**（可写日志、可 UI 原样兜底展示）。
///
/// **使用边界**：
/// - `untagged` 反序列化要先把内容缓冲再逐变体试，错误信息退化成 "data did not match any variant"。
///   只用于**低频帧**的字段（每轮一次 / 每会话一次），严禁用在高频 [`LlmFrame::Delta`] 的热路径上。
/// - `Other` 只装**短机器可读标识**。**被控端构造时只许用 `from_wire_clamped`**，它按
///   [`LLM_ENUM_WIRE_MAX_BYTES`] 在 UTF-8 字符边界夹紧后才落 `Other`（模块文档铁律一之 3）。
///   直接写 `Other(上游原文)` 是对该铁律的破坏，评审时打回。
/// - **线格式是本协议自己的 `PascalCase`，不是上游原文**：`$known` 变体不加 `#[serde(rename)]` 时
///   线上就是 Rust 变体名，而上游多为 `snake_case`（实测 `"api_error"` / `"completed"` /
///   `"five_hour"` / `"out_of_credits"`）。故被控端的正确姿势是**先查自己的归一化映射表**
///   （`"api_error"` → [`LlmTerminalReasonKnown::ApiError`]），映射表没有的才把**上游原文**交给
///   `from_wire_clamped` 落进 `Other`。直接拿上游 `snake_case` 喂 `from_wire_clamped` 会让一个
///   本端明明认识的值静默落进 `Other`，UI 从此走「未知值」分支——不会报错、只会长期失真。
/// - `Other("EndTurn")` 序列化后与 `Known(EndTurn)` 线上完全相同，回来会归一成 `Known`——
///   即**往返不是恒等**。这是刻意的：本端认识的值就该收敛到已知集合，不留两个等价表示。
///
/// **`Debug` 是手写的、不是 derive 的**：`Other` 分支只打前 [`LLM_ENUM_DEBUG_HEAD_BYTES`] 字节
/// 加总长（形如 `LlmStopReason::Other("api_err…", 4096B)`）。derive 会把 `Other` 原文原样写进
/// `format!("{frame:?}")`——那正好绕开模块文档 :91 那条「审计日志只记白名单字段」的兜底：
/// 夹紧只在**发送侧**成立，反序列化进来的 `Other` 长度不受本端控制（`Deserialize` 里不校验长度，
/// 见 [`LLM_ENUM_WIRE_MAX_BYTES`] 的说明），故日志侧必须自己再守一道。
///
/// **`$known` 变体禁止加 `#[serde(rename)]` 之外的线格式属性**：`from_wire_clamped` 与 `as_wire`
/// 都走 serde 本身取值（不是 `stringify!` 也不是 `Debug`），二者天然一致；但若哪天给 `$known`
/// 加了 `#[serde(untagged)]` / 非字符串表示，这两个函数会一起失效。
macro_rules! open_enum {
    ($(#[$m:meta])* $name:ident, $known:ident { $($(#[$vm:meta])* $v:ident),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $known { $($(#[$vm])* $v),+ }

        $(#[$m])*
        #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(untagged)]
        pub enum $name {
            /// 本端认识的值。
            Known($known),
            /// 更高版本对端发来的未知值（**保留原文，勿丢**）。
            ///
            /// 本端构造**只许**走 [`Self::from_wire_clamped`]，不要直接写这个变体。
            Other(String),
        }

        impl $name {
            $(#[allow(non_upper_case_globals)]
              pub const $v: Self = Self::Known($known::$v);)+

            /// **被控端把上游原文落成本类型的唯一合法入口**。
            ///
            /// 先按 serde 的线格式试已知集合（认识就收敛成 `Known`，不留两个等价表示），
            /// 不认识才落 `Other`，且落之前按 [`LLM_ENUM_WIRE_MAX_BYTES`] 在 **UTF-8 字符边界**
            /// 截断（见 [`clamp_enum_wire`]）——中文标识按字节切会切出非法 UTF-8，`String` 构造
            /// 直接 panic，故不能用裸 `&s[..N]`。
            ///
            /// 没有这个函数时 `LLM_ENUM_WIRE_MAX_BYTES` 就只是个装饰常量：`Deserialize` 侧不校验
            /// 长度（`untagged` 里做长度校验会把「超长」变成整帧 `Err`，比留原文更糟），全靠发送侧
            /// 自觉——那正是对抗验证点名的「声称补了防线、实际只加了个数字」。
            #[must_use]
            pub fn from_wire_clamped(wire: &str) -> Self {
                let de: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
                    serde::de::IntoDeserializer::into_deserializer(wire);
                match <$known as serde::Deserialize>::deserialize(de) {
                    Ok(known) => Self::Known(known),
                    Err(_) => Self::Other(clamp_enum_wire(wire).to_owned()),
                }
            }

            /// 线上原文（日志 / UI 兜底展示用）。
            ///
            /// `Known` 分支走 **serde 自身**取值而不是 `format!("{k:?}")`：后者拿的是 Debug 名，
            /// 与线格式当前恰好相等纯属没人加过 `#[serde(rename)]`；一旦加了（做跨 CLI 归一化时
            /// 很容易发生），`as_wire()` 会返回 Rust 变体名而线上是 rename 后的值，UI / 日志 /
            /// 线格式三方静默不一致且没有测试会红。这里消除双事实源。
            #[must_use]
            pub fn as_wire(&self) -> std::borrow::Cow<'_, str> {
                match self {
                    Self::Known(k) => match serde_json::to_value(k) {
                        // C 型枚举序列化必是 JSON 字符串；走到别的分支说明 `$known` 被加了
                        // 非字符串表示（见本宏文档最后一段），此时退化成 "?" 而不是 panic。
                        Ok(serde_json::Value::String(s)) => std::borrow::Cow::Owned(s),
                        _ => std::borrow::Cow::Borrowed("?"),
                    },
                    Self::Other(s) => std::borrow::Cow::Borrowed(s),
                }
            }

            /// 本端是否认识该值（UI 决定「显示图标」还是「显示原文」时用）。
            #[must_use]
            pub fn is_known(&self) -> bool {
                matches!(self, Self::Known(_))
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Known(k) => write!(formatter, "{}::{k:?}", stringify!($name)),
                    Self::Other(s) => write!(
                        formatter,
                        "{}::Other({:?}{}, {}B)",
                        stringify!($name),
                        clamp_utf8(s, LLM_ENUM_DEBUG_HEAD_BYTES),
                        if s.len() > LLM_ENUM_DEBUG_HEAD_BYTES { "…" } else { "" },
                        s.len()
                    ),
                }
            }
        }
    };
}

/// 在 **UTF-8 字符边界**上把 `s` 截到不超过 `max` 字节。
///
/// 裸 `&s[..max]` 对多字节字符会 panic（`byte index is not a char boundary`）——上游的
/// 错误标识含中文 / emoji 完全可能，这不是理论风险。
fn clamp_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 把上游原文夹到 [`LLM_ENUM_WIRE_MAX_BYTES`] 以内（UTF-8 字符边界安全）。
///
/// 被控端把 CLI 给的未知标识落进任何开放式 C 型枚举的 `Other` 之前**必须**过这一道；
/// 通常不用直接调用它——用各枚举的 `from_wire_clamped` 即可，那里已经调过。
#[must_use]
pub fn clamp_enum_wire(s: &str) -> &str {
    clamp_utf8(s, LLM_ENUM_WIRE_MAX_BYTES)
}

open_enum!(
    /// 后端 LLM CLI 种类。与 app 侧 `llm_cli::LlmCliKind` 语义对齐但**协议侧独立定义**
    /// （本 crate 零平台依赖，不能反向依赖 lumen-app）。
    LlmAgentKind,
    LlmAgentKindKnown {
        Claude,
        Codex,
        Gemini,
        Kimi,
    }
);

open_enum!(
    /// 权限模式（归一化）。Claude 侧直接映射 `--permission-mode`；其它 CLI 语义不同的由被控端做
    /// 最近似映射，并在 [`LlmAgentInfo::permission_modes`] 里只声明自己**真正支持**的那几个。
    LlmPermissionMode,
    LlmPermissionModeKnown {
        Manual,
        AcceptEdits,
        Auto,
        Bypass,
        DontAsk,
        Plan,
    }
);

open_enum!(
    /// 一轮结束的原因（**展示用**）。判成败请看 [`LlmTurnOutcome`]，不是这里——
    /// 实测存在「`stop` 看起来正常、`is_error` 为真」的组合。
    LlmStopReason,
    LlmStopReasonKnown {
        /// 正常收尾。
        EndTurn,
        /// 触达输出 token 上限。
        MaxTokens,
        /// 停在工具调用上（等待工具结果 / 等待权限）。
        ToolUse,
        /// 被控制端 [`LlmFrame::Interrupt`] 或本地中断。
        Interrupted,
        /// 模型拒答。
        Refusal,
        /// 出错终止（详情见 [`LlmTurnOutcome`] 或随后的 [`LlmFrame::Error`]）。
        Error,
    }
);

open_enum!(
    /// 一轮的**终止通道**，对应 Claude headless `result` 行的 `terminal_reason`。
    ///
    /// **只声明本机实测取到的值**（样本 A 失败路径 `"api_error"`、样本 B 成功路径 `"completed"`），
    /// 其余一律走 `Other` 保留原文——上游枚举集未经采样，臆造出来的变体只会让映射表长期骗人。
    ///
    /// 上游是 `snake_case`、本枚举线上是 `PascalCase`，被控端需自带映射表（见 `open_enum!` 文档
    /// 「线格式是本协议自己的 PascalCase」那条）。
    LlmTerminalReason,
    LlmTerminalReasonKnown {
        /// 正常跑完（样本 B 实测 `"completed"`，同行 `is_error = false`）。
        ///
        /// **它不是「成功」的判据**——判成败只看 [`LlmTurnOutcome::is_error`]。本变体只说明
        /// 「这一轮是从正常通道收的场」，与 `subtype` 那种两次采样同一个值的字段划清界限。
        Completed,
        /// 上游 API 报错终止（样本 A 实测 `"api_error"`；同帧 [`LlmTurnOutcome::api_error_status`]
        /// 带 HTTP 状态码 403）。
        ApiError,
    }
);

open_enum!(
    /// 限流窗口的**当前判定**，对应 `rate_limit_event.rate_limit_info.status`。
    ///
    /// 只声明实测到的 `"allowed"`；`"rejected"` / `"warning"` 之类**未采样**，一律走 `Other`
    /// 原样保留（控制端对未知值应按「状态不明」展示，**不得**默认当成放行）。
    LlmRateLimitStatus,
    LlmRateLimitStatusKnown {
        /// 额度充足，请求被放行（实测值）。
        Allowed,
    }
);

open_enum!(
    /// 限流窗口的**周期**，对应 `rate_limit_info.rateLimitType`（实测 `"five_hour"`）。
    ///
    /// 手机端据此把 [`LlmRateLimit::resets_at_secs`] 渲染成「5 小时额度 14:30 重置」而不是一个
    /// 裸时间戳。周产 / 月产等窗口未采样，不臆造。
    LlmRateLimitWindow,
    LlmRateLimitWindowKnown {
        /// 5 小时滚动窗口（实测值）。
        FiveHour,
    }
);

open_enum!(
    /// 超额（overage）计费的当前状态，对应 `rate_limit_info.overageStatus`（实测 `"rejected"`）。
    LlmOverageStatus,
    LlmOverageStatusKnown {
        /// 超额计费被拒（实测值；原因见 [`LlmRateLimit::overage_disabled_reason`]）。
        Rejected,
    }
);

open_enum!(
    /// 超额计费被禁用的原因，对应 `rate_limit_info.overageDisabledReason`（实测
    /// `"out_of_credits"`）。这是手机端唯一能给出**可操作提示**（「去充值」）的字段。
    LlmOverageDisabledReason,
    LlmOverageDisabledReasonKnown {
        /// 余额耗尽（实测值）。
        OutOfCredits,
    }
);

open_enum!(
    /// 对话（子进程）退出原因。
    LlmExitReason,
    LlmExitReasonKnown {
        /// 控制端 [`LlmFrame::Close`] 主动关闭。
        Closed,
        /// CLI 子进程自行退出（[`LlmFrame::ConvExited::code`] 带退出码）。
        ///
        /// **实测提醒**：认证失败时进程 `exit code = 1` 且 **stderr 为空**——失败详情只在 stdout
        /// 的事件流里。被控端不得只看退出码就编错误文案。
        ProcessExited,
        /// 启动失败（可执行文件不存在 / 权限不足）。
        SpawnFailed,
        /// CLI 输出无法解析成本协议模型，被控端主动放弃。
        ProtocolError,
        /// 空闲超时回收。
        Idle,
        /// 被控端 Lumen 退出 / 登出。
        HostShutdown,
    }
);

open_enum!(
    /// 机器可读错误码（控制端按此本地化提示，**不靠字符串匹配**——沿用
    /// [`crate::remote::DenyReason`] 的既定风格）。
    ///
    /// 上游给的机器可读字符串（实测 `"authentication_failed"`）映射不到已知码时落
    /// `Other(原文)`，**不要**硬塞成 [`LlmErrorCodeKnown::Io`]——那会让手机端提示词彻底失真。
    LlmErrorCode,
    LlmErrorCodeKnown {
        /// 请求的 agent 在被控端不可用（未安装 / 不在 PATH）。
        AgentNotFound,
        /// cwd 不存在 / 不是目录 / 越权。
        CwdInvalid,
        /// CLI 未登录或凭据过期（实测 `error = "authentication_failed"` 映射到此）。
        AuthRequired,
        /// 上游限流。
        RateLimited,
        /// 上游过载。
        Overloaded,
        /// 上下文超限。
        ContextOverflow,
        /// 请求被取消。
        Cancelled,
        /// `conv_generation` 不匹配（重挂 / 重启后的陈旧请求）。
        StaleSession,
        /// 触达 [`LLM_MAX_CONVS`] 并发对话上限。
        LimitReached,
        /// 其它 IO 错误（粗粒度兜底）。
        Io,
        /// 协议违例（字段缺失 / 状态机不允许）。
        Protocol,
    }
);

// ── 归一化消息模型 ────────────────────────────────────────────────────────────
//
// **刻意没有 `LlmRole`（作者）类型**：本模型没有「消息」这一层，只有 轮 → 块，作者维度已由
// [`LlmTurnRecord`] 的 `user` / `assistant` 两个数组表达；而 `system` 通道在本协议里根本不存在
// ——上游 `system` 行（`init` / `hook_started` / `hook_response`）按模块文档铁律一一律不上行。
// 没有承载点的 `pub` 类型就是线协议里的死类型，需要时再加（[`LlmBlock`] 的 `other` 兜底让后加
// 零兼容成本）。

/// 工具执行结果状态。
///
/// # 被控端怎么从上游映射（`is_error` **可能整个键都不存在**）
/// 样本 B 第 9 行实测的 `tool_result` 块只有 `type` / `tool_use_id` / `content` 三个键——
/// **`is_error` 不是 `false`，是压根没有**。故映射规则必须写成：
///
/// - `is_error` 缺键 **或** 为 `false` → [`Self::Ok`]；
/// - `is_error == true` → [`Self::Error`]；
/// - 权限拒绝 / 中断由被控端自己的状态机给出 [`Self::Denied`] / [`Self::Cancelled`]。
///
/// 反面写法有两种，都会出事：把 `is_error` 当**必填**去 `?` / `expect` 会让**每一次正常的工具
/// 结果**都解析失败（正常路径全灭）；反过来用「键是否存在」判断这块是不是工具结果，则会在上游
/// 哪天补上 `is_error: false` 时静默改变分支。只认「缺键 = 非错误」这一条。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmToolStatus {
    Ok,
    Error,
    /// 被用户在权限审批里拒绝。
    Denied,
    Cancelled,
    /// 更高版本对端的未知状态（**只用于反序列化兜底，永不发送**）。
    #[serde(other)]
    Unknown,
}

/// 图片 / 文件附件。**内容不走本协议**：控制端先用现成的
/// [`crate::remote::RemoteFrame::PutBegin`] / `PutChunk` / `PutEnd` 通道把字节上传到
/// [`LlmFrame::ConvStarted`] 回带的 `attach_dir`，拿到被控端绝对路径后再在 [`LlmFrame::Send`]
/// 里引用。
///
/// **为什么不 base64 塞进 `Send`**：单张手机照片 3–5 MB，base64 后直接顶穿服务端 4 MiB 单帧上限，
/// 而中继侧超限是**断整条 WS**、QUIC 侧是**静默丢帧**，两种失败模式都极难排查。
///
/// **为什么路径语义天然成立**：app 侧 `llm_attachments.rs` 的 `compile_prompt` 本来就是把本机
/// 绝对路径写进提示词让 CLI 自己读文件；headless CLI 跑在 PC 上、附件也落在 PC 上，零改造。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmAttachment {
    /// 被控端绝对路径（脱敏，见 [`LlmPath`]）。
    pub path: LlmPath,
    /// 展示用文件名。
    ///
    /// **是 [`LlmText`] 不是裸 `String`**（线格式不变，`Debug` 只打字符数）：手机相册的文件名
    /// 同样是用户内容（人名、单据号、截图标题、`我的身份证-xxx.png`）。同结构体里 `path` 已经
    /// 脱敏，只脱一半是半道门——模块文档「审计日志不得整帧落盘」的兜底会在这里破口。
    pub name: LlmText,
    /// MIME 类型（如 `image/png`）。
    pub mime: String,
    /// 字节数（控制端用于展示 / 校验）。
    pub len: u64,
}

/// 思考块的**内容可得性**——本模块把「思考」从内容降级成状态信号的落点（模块文档铁律三）。
///
/// # 为什么是互斥三态，而不是「一个可能为空的 `text`」
/// 样本 B 实测 Claude Opus 5 的 `thinking` 块里 `thinking` 字段**恒为空串**、只有 `signature`
/// 有值，且 CLI **无开关**可打开。若沿用旧设计的 `Thinking { text: LlmText }`：
///
/// - 手机端拿到的永远是空字符串，UI 只能渲染一个**永远展不开的折叠块**；
/// - 更糟的是那个字段会**诱导被控端往里填东西**——最顺手的就是把 456 字符的 `signature`
///   或整段上游 JSON 塞进去「反正有个文本字段」，白名单铁律当场破功。
///
/// 三态把这两条同时堵死：[`Self::Omitted`] 是 unit 变体，**类型上就没有可绑定的文本**。
///
/// # 为什么不干脆删掉思考块的文本
/// 本模型是跨 CLI 的（[`LlmAgentKind`] 有四家）。「Claude 给不出文本」不等于「没人给得出」——
/// 别家 CLI 的推理摘要是实打实的正文。删字段会让协议在别家 CLI 上无损表达不了，且日后 Claude
/// 一旦暴露 summarized 开关还要再改一次形状（C 型枚举那条「线格式定了就不许改形状」的教训）。
/// 保留 [`Self::Text`] 分支则两边都成立：被控端**能拿到就发 `Text`、拿不到就发 `Omitted`**，
/// 协议本身不需要再动。
///
/// 本次形状变更发生在 [`LLM_PROTO_VERSION`] `= 1` **落地之前**（全仓无任何实现在读写本类型），
/// 故不构成破坏性升级，也就没有跟着抬版本号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmThinking {
    /// 有明文可展示。**Claude 上目前取不到这个分支**（见类型文档），别家 CLI 或将来的
    /// summarized 模式才会用到。只有本分支能接 [`LlmDeltaItem::TextAppend`]。
    Text { text: LlmText },
    /// 上游只给了加密占位、明文不可得（Anthropic 的 `redacted_thinking`）。
    ///
    /// 与 [`Self::Omitted`] 的区别是**归因**：本变体是**上游策略**加密，Omitted 是**CLI 展示
    /// 设置**省略。手机端文案不同（「该段思考已加密」vs「当前 CLI 不输出思考内容」），别合并。
    Redacted,
    /// 上游把思考内容整个省略了：**样本 B 实测的 Claude 分支**（`thinking: ""` + 非空
    /// `signature`，CLI 无开关）。
    ///
    /// 控制端只应据此显示「正在思考 / 思考了 N 秒」这类**状态**，与桌面端 `llm_hud.rs` 现有做法
    /// 一致。`signature` 是上游不透明凭据，**不上行**（对手机端零用途，且属白名单外内容）。
    Omitted,
    /// 更高版本对端的未知思考形态（**只用于反序列化兜底，永不发送**）。
    #[serde(other)]
    Unknown,
}

/// 工具结果的**结构化摘要**——上游 `user` 行顶层 `tool_use_result` 的归一化落点。
///
/// # 它解决什么
/// 样本 B 实测一次 `Read` 的工具结果同时给了两份东西：`message.content[].content` 里 11 720 字符
/// 的**文件正文**，以及顶层 `tool_use_result.file` 里的 `{filePath, content, numLines, startLine,
/// totalLines}`。后者足以把卡片渲染成「读取 README.md 第 1–165 行（共 165 行）」，
/// **完全不需要把正文推到手机**——这对流量与手机端渲染性能是决定性的。
///
/// # 三条硬约束
/// 1. **本类型只装元数据，不装内容**：[`Self::File`] 刻意**没有** `content` 字段。正文的唯一
///    承载点仍是 [`LlmBlock::ToolResult`] 的 `output`，且必须按 [`LLM_TOOL_OUTPUT_MAX_BYTES`]
///    夹紧、把裁掉的字节数写进 `truncated_bytes`。有了本摘要，被控端**可以（也应该）把 `output`
///    夹得更狠、甚至夹成空串**——卡片照样有信息，缺的部分由用户按需点开再走
///    [`LlmFrame::TurnFetch`] / 文件通道取。
/// 2. **未知形态一律不上行**：只有 `Read` 系的 `file` 形态被实测到，`Bash` / `Edit` / `Write` 的
///    `tool_use_result` 形状**未知**。被控端遇到没建模的形态只能填 `None`（白名单铁律），
///    **不得**臆造字段名，更不得开一个 `serde_json::Value` 口子把上游对象整个搬过来。
/// 3. `#[serde(other)]` 兜底管的是**接收侧**：日后补 `Bash` / `Edit` 变体时，老控制端把该摘要
///    降级成 [`Self::Unknown`]（卡片退回纯文本样式），同帧其余内容照常解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmToolResultDetail {
    /// 文件读取型结果（实测来源：`tool_use_result.file`，Claude 的 `Read` 工具）。
    File {
        /// 被读文件的绝对路径（脱敏，见 [`LlmPath`]）。对应 `file.filePath`。
        path: LlmPath,
        /// 本次读取的起始行（1 基）。对应 `file.startLine`（实测 1）。
        start_line: u32,
        /// 本次读取的行数。对应 `file.numLines`（实测 165）。
        line_count: u32,
        /// 文件总行数。对应 `file.totalLines`（实测 165）。
        ///
        /// `total_lines > start_line + line_count - 1` 时手机端应展示「仅读取了部分」，
        /// 这正是本摘要相对「一坨正文」的全部价值。
        total_lines: u32,
    },
    /// 更高版本对端的未知摘要形态（**只用于反序列化兜底，永不发送**）。
    #[serde(other)]
    Unknown,
}

/// **归一化对话块**——与 CLI 厂商无关的最小公共模型。
///
/// 「块」是 Claude / Codex / Gemini / Kimi 的共同结构（消息由块组成：文本、思考、工具调用、
/// 工具结果），这四类是三家的交集，能无损映射。
///
/// 内部标签 + `#[serde(other)]`：未来新增块类型（如音频 / 图表）时，老控制端把该块降级成
/// [`LlmBlock::Unknown`]，**同一帧里其余块照常解析**，气泡流只少一块、不整帧丢弃。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmBlock {
    /// 普通文本（Markdown 原文，渲染由控制端负责）。
    Text { text: LlmText },
    /// 思考 / 推理过程。**内容可得性见 [`LlmThinking`]**——Claude 上恒为
    /// [`LlmThinking::Omitted`]，手机端只能把它当「正在思考」的状态信号（模块文档铁律三）。
    ///
    /// 块本身仍然保留：思考**发生过**这件事有展示价值（时间轴上的一段耗时、HUD 上的状态），
    /// 缺的只是可展开的正文。
    Thinking { content: LlmThinking },
    /// 工具调用。入参按 [`LLM_TOOL_INPUT_MAX_BYTES`] 夹紧，超出部分由 `truncated_bytes` 声明
    /// （与 [`LlmBlock::ToolResult`] 对称）。
    ///
    /// # 实测到但**刻意不上行**的字段：`caller`
    /// 样本 B 的 `tool_use` 块除 `type` / `id` / `name` / `input` 外还有一个公开 Messages API
    /// **没有**的 `caller`（实测值 `{"type": "direct"}`，是**对象**不是字符串）。三条理由决定不建模：
    ///
    /// 1. **语义未知**：只采到 `direct` 一个值，其余取值与含义都靠猜；按未知语义建的字段，
    ///    手机端任何据此渲染的差异都是错的。
    /// 2. **它是对象**：要承载就得建模一个未采样的嵌套结构，或者开一个 `serde_json::Value`
    ///    口子——后者是模块文档铁律一之 2 明令禁止的。
    /// 3. **需求已被覆盖**：手机端真正需要的「这次调用属于哪个子代理」已由
    ///    [`LlmBlockEntry::parent_call_id`]（上游 `parent_tool_use_id`）表达。
    ///
    /// 日后若采到 `caller` 的取值集且证明有渲染价值，再按 `open_enum!` 加一个**定长**字段，
    /// 不是现在。
    ToolUse {
        /// 上游给的调用 id，用于与 [`LlmBlock::ToolResult`] 配对。**不可为空**。
        /// 实测形如 `"toolu_019Vk9BtrSBpc2dtv9YBPTY3"`。
        call_id: String,
        /// 工具名（如 `Read` / `Bash` / `Edit`），控制端据此选卡片样式。
        name: String,
        /// 人类可读的一行摘要（如「读取 main.rs」），被控端尽力生成，无则 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<LlmText>,
        /// 工具入参**原样透传、不归一化**（见 [`LlmToolInput`]），但按
        /// [`LLM_TOOL_INPUT_MAX_BYTES`] 夹紧——`Write` / `Edit` 的入参里装的是整份文件正文。
        input: LlmToolInput,
        /// 入参被裁掉的字节数（未裁则 `None`）。
        ///
        /// **这个字段是「不许丢块」的类型保证**：没有它时，被控端面对 5 MiB 的 `Write` 入参
        /// 只有两条路——发出去炸链路，或者静默丢掉整个 `ToolUse`（工具卡片凭空消失、后续
        /// `ToolResult` 的 `call_id` 配不上对）。有了它，第三条路（裁剪 + 可观测声明）在协议上
        /// 成立，与 [`LlmDeltaItem::Dropped`] 是同一条「降级必须可观测」的原则。
        ///
        /// `#[serde(default)]`：老端不带本字段时解释成「未裁剪」，加字段零兼容成本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated_bytes: Option<u64>,
    },
    /// 工具结果。输出按 [`LLM_TOOL_OUTPUT_MAX_BYTES`] 夹紧，超出部分由 `truncated_bytes` 声明。
    ToolResult {
        /// 与 [`LlmBlock::ToolUse`] 的 `call_id` 配对。
        call_id: String,
        /// 见 [`LlmToolStatus`]——上游 `is_error` **可能整个键不存在**，缺键按 `Ok` 解释。
        status: LlmToolStatus,
        /// 结果正文（上游 `tool_result.content`，实测是**纯字符串**而非块数组）。
        output: LlmText,
        /// 被截断的字节数（未截断则 `None`）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated_bytes: Option<u64>,
        /// **结构化摘要**（见 [`LlmToolResultDetail`]）：有它就能在不推送正文的前提下把卡片
        /// 渲染成「读取 README.md 第 1–165 行（共 165 行）」。
        ///
        /// `None` = 上游没给、或给的是本端**尚未建模**的形态（`Bash` / `Edit` 等，见
        /// [`LlmToolResultDetail`] 约束 2）。这时手机端退回按 `output` 渲染纯文本。
        ///
        /// `#[serde(default)]`：老端不带本字段时解释成「无摘要」，加字段零兼容成本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<LlmToolResultDetail>,
    },
    /// 图片块（用户发来的附件回显，或模型产出的图）。
    Image { attachment: LlmAttachment },
    /// **错误块**——某次调用 / 某条消息是错误，而不是模型的正常输出。
    ///
    /// **这个变体是硬性要求，不是可选优化**（模块文档铁律二）：实测认证失败以一条**普通
    /// `assistant` 消息 + 普通 `text` 块**投递，正文是
    /// `"Failed to authenticate. API Error: 403 Request not allowed"`，只有顶层
    /// `is_api_error_message = true` 能识别。被控端的映射顺序必须是：
    ///
    /// 1. 先读 `assistant` 行顶层的 `is_api_error_message`；
    /// 2. 为真 → 整条消息的文本内容落本变体，`code` 取顶层 `error` 字段映射
    ///    （实测 `"authentication_failed"` → [`LlmErrorCodeKnown::AuthRequired`]；映射不到就
    ///    `Other(原文)`）；
    /// 3. 为假 → 才按 [`LlmBlock::Text`] / [`LlmBlock::Thinking`] 等常规块处理。
    ///
    /// 顺序反了就会把「403 请求被拒」渲染成模型的正常回复气泡，用户完全看不出这是故障。
    Error {
        code: LlmErrorCode,
        message: LlmText,
    },
    /// 更高版本对端的未知块类型（**只用于反序列化兜底，永不发送**）。
    #[serde(other)]
    Unknown,
}

/// **带归属信息的块条目**——块在轮内的身份（`block_id`）与嵌套归属（`parent_call_id`）不属于块
/// 内容本身，故拆成外层信封，[`LlmDeltaItem::BlockStart`] 与 [`LlmTurnRecord`] 共用同一份定义
/// （两处各写一遍迟早写歪）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmBlockEntry {
    /// 块在**本轮内**的序号（见 [`BlockId`]）。
    pub block_id: BlockId,
    /// **嵌套归属**：本块属于哪一次工具调用产生的子代理输出，对应 Claude headless 每行顶层的
    /// `parent_tool_use_id`（实测该字段存在，本次采样为 `null`）。
    ///
    /// 归一化模型里没有「消息」这一层（只有 轮 → 块），故上游挂在消息上的归属维度只能落到**块**
    /// 上：同一条子代理消息的所有块共享同一个 `parent_call_id`。值与 [`LlmBlock::ToolUse`] 的
    /// `call_id` 同一空间，控制端据此把子代理气泡缩进到父工具卡片里。
    ///
    /// `None` = 主对话直接产出。**不要**用空串表达「无归属」——`Option` 才能让缺省与空值可区分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<String>,
    /// 块内容。
    pub block: LlmBlock,
}

/// 一轮的 token / 花费统计（**归一化后的最小集合**）。全部字段 `#[serde(default)]`，上游给不出的填 0。
///
/// 实测上游 `result.usage` 还带 `service_tier` / `inference_geo`（样本 A `""`、样本 B
/// `"not_available"`）/ `iterations[]` / `speed` / `cache_creation{ephemeral_*}` 等字段，
/// **一律不上行**（模块文档铁律一）：它们是上游账务与本机环境信息，对手机端 UI 无用，却会让
/// 「usage 透传」变成又一条环境泄漏通道。同理样本 B 新观测到的 `ttft_ms` / `ttft_stream_ms` /
/// `time_to_request_ms` 也不上行——手机端已有 `started_ms` / `ended_ms` 可算墙钟耗时。
///
/// # `modelUsage` 必须**提取**，不许透传
/// 样本 B 的 `result.modelUsage` 是一个**按模型名做 key 的 map**，值里的字段全是 `camelCase`
/// （`inputTokens` / `outputTokens` / `cacheReadInputTokens` / `cacheCreationInputTokens` /
/// `webSearchRequests` / `costUSD` / `contextWindow` / `maxOutputTokens` / `canonicalModel` /
/// `provider`）。它**不能**原样上行：
///
/// - key 是动态的（模型名），本模块所有类型都是定长字段，没有 map 类型的容身之处；
/// - `canonicalModel` / `provider` 是上游账务标识，属白名单外内容。
///
/// 被控端的正确做法是**提取成本定长结构**：token 数与花费**跨模型累加**（一轮可能同时用了主模型
/// 与子代理模型），而 [`Self::context_limit`] / [`Self::max_output_tokens`] 取
/// **[`LlmConvMeta::model`] 对应那一项**——不同模型的窗口不同，求和 / 取最大都是错的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LlmUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// 花费，单位**微美元**（1e-6 USD）。用整数避免 `f64` 让本类型失去 `Eq`，也避免浮点累加误差
    /// （对齐 [`crate::remote::RemoteFrame`] 因 `f32` 权重而无法 `derive(Eq)` 的教训）。
    ///
    /// 上游 `total_cost_usd` 是浮点且**自带二进制表示误差**——样本 B 实测原值就是
    /// `0.24026150000000002`（不是 `0.2402615`）。被控端换算时**向上取整**：
    /// `(0.24026150000000002 * 1e6).ceil() = 240_262`。宁可多报 1e-6 美元也不虚报 0，
    /// 更不要 `as u64` 直接截断（那会把 0.9 µUSD 的一轮显示成免费）。
    #[serde(default)]
    pub cost_micro_usd: u64,
    /// **当前上下文占用 token**（HUD 进度条的分子）。`0` = 未知。
    ///
    /// # 这不再是估算值——但算法必须写死，否则会算出 300%
    /// 设计初稿写的是「估算值，headless 无终端画面可刮」。样本 B 推翻了这个结论：上游每条
    /// `assistant` 行的 `message.usage` 与 `result.usage` 里的
    /// `input_tokens` / `cache_read_input_tokens` / `cache_creation_input_tokens` 都是**真值**，
    /// 三者之和就是那次请求真实送进模型的提示词 token 数。
    ///
    /// **但只能取「最后一次」，不能累加**：`result.usage` 是**整轮累加**（`usage.iterations[]`
    /// 是每次 API 调用的明细数组），一轮 10 次工具调用累加出来的 input_tokens 是同一份上下文被
    /// 数了 10 遍。被控端必须取**本轮最后一次** API 调用的那三项之和（末条 `assistant` 行的
    /// `message.usage`，或 `usage.iterations[]` 的末元素），而
    /// [`Self::input_tokens`] / [`Self::output_tokens`] / `cache_*` 仍是**整轮累加值**（那是账务
    /// 口径，用于展示「这一轮花了多少」）。两个口径不同，同一个结构里并存是刻意的。
    #[serde(default)]
    pub context_used: u64,
    /// 上下文窗口上限（`0` = 未知）——HUD 进度条的分母。
    ///
    /// 来源是 `result.modelUsage[主模型].contextWindow`（样本 B 实测**存在且是真值**）。
    /// 这条推翻了设计文档 §7.9「上下文占用只能估算」的结论：配合 [`Self::context_used`]
    /// 可以算出**真实**占用百分比，手机端不必再显示一个自己编的数。
    ///
    /// # 边界：轮中拿不到，**不许自行推算**
    /// `modelUsage` **只在 `result` 行出现**，即只有轮末拿得到。而本结构会随 [`LlmFrame::Attached`] /
    /// [`LlmFrame::ConvStarted`] / [`LlmFrame::HelloAck`] 里的 [`LlmConvMeta::usage`] 在**轮次进行中**
    /// 下发。轮中只允许两种填法：
    ///
    /// 1. **沿用上一轮的窗口值**（此时百分比才是「估算」，手机端文案要带「约」）；
    /// 2. **首轮结束前填 `0`**（= 未知，手机端**不显示百分比**，也不显示占位数字）。
    ///
    /// **不得**按模型名去查一张硬编码的窗口表来「补」这个值——那正是 §7.9 整节重写要废掉的做法，
    /// 会让手机端显示一个瞎猜的百分比（模型别名一变就是 300% 或 3%），比不显示更糟。
    #[serde(default)]
    pub context_limit: u64,
    /// 本轮所用模型的**单次最大输出 token**（`0` = 未知）。
    ///
    /// 来源 `result.modelUsage[主模型].maxOutputTokens`（样本 B 实测存在）。它与
    /// [`Self::context_limit`] 是两个不同的上限，手机端据此解释
    /// [`LlmStopReasonKnown::MaxTokens`]——「这轮撞的是单次输出上限（N tokens），不是上下文满了」。
    /// 两者混为一谈会给出完全错误的排障建议（「清理上下文」对撞输出上限毫无用处）。
    #[serde(default)]
    pub max_output_tokens: u64,
}

/// 一轮的**权威成败判定**（对应 Claude headless `result` 行）。
///
/// # 不得靠 `subtype` 判成败
/// 两次采样把这条钉死了：样本 A（认证失败）`subtype = "success"` + `is_error = true`，
/// 样本 B（正常跑完）`subtype = "success"` + `is_error = false`——**同一个 `subtype` 值、
/// 相反的成败**。`subtype` 描述的是「这轮怎么收的场」，不是「成没成」。判定顺序**只能**是：
///
/// 1. [`Self::is_error`]——唯一权威布尔；
/// 2. [`Self::terminal_reason`]——终止通道（样本 A `"api_error"` / 样本 B `"completed"`），用于分类；
/// 3. [`Self::api_error_status`]——HTTP 状态码（实测 `403`），用于本地化文案。
///
/// 三者都缺省（`is_error = false` + 两个 `None`）表示「正常收尾」，与老端不带本结构时的
/// `#[serde(default)]` 结果一致，故加字段不破坏兼容。
///
/// # 实测到但**刻意不上行**的字段：`request_id`
/// 样本 B 的每条 `assistant` 行都带 `request_id`（形如 `"req_011CdqKZnWo93WokLNU7b2m2"`），
/// 与上游对单排障确实有用。仍然不进协议，理由三条：
///
/// 1. 它是**上游账号侧的标识符**，与 `session_id` / `uuid` 同类，默认落在白名单之外；
/// 2. 手机端对它**零用途**——用户不会拿 `request_id` 去找上游对单，能对单的是 PC 侧运维；
/// 3. 真正需要它的场景在**被控端**，而被控端本来就该把它写进自己的结构化审计日志
///    （模块文档最后一段允许记的正是这类**非正文、非入参**的定长标识），不需要绕经中继服务器
///    和手机再传回来。
///
/// 换言之：不是「没价值」，是「价值不在手机这一端」。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LlmTurnOutcome {
    /// **唯一权威**的成败标志。
    #[serde(default)]
    pub is_error: bool,
    /// 终止通道（如上游 API 报错）。缺省 `None` = 上游没给。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<LlmTerminalReason>,
    /// 上游 HTTP 状态码（实测 403）。缺省 `None` = 与 HTTP 无关或上游没给。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<u16>,
}

/// **账号额度状态**——上游 `rate_limit_event.rate_limit_info` 的归一化落点。
///
/// # 为什么值得单独建模
/// 样本 B 第 10 行实测到一个设计里完全没有的**顶层事件类型** `rate_limit_event`：
/// `{status, resetsAt, rateLimitType, overageStatus, overageDisabledReason, isUsingOverage}`。
/// 这恰恰是手机端最该显示的东西——用户在外面远程盯着 PC 跑 agent，「还能跑多久 / 几点恢复」
/// 比任何 token 数都重要，而这是唯一给得出**恢复时刻**的通道。
///
/// # 全部字段可缺省
/// 只观测到一次事件、一种取值组合，上游随时可能不给某个键。除 `status` 外全部 `Option` /
/// `#[serde(default)]`，缺省一律解释成「上游没说」，**不得**解释成「否」——
/// 把「没说超额」当成「没在超额」会给出错误的计费提示。
///
/// 这条对**布尔字段同样成立**，且恰恰是布尔最容易破功：[`Self::using_overage`] 因此是
/// `Option<bool>` 而不是裸 `bool`。裸 `bool` + `#[serde(default)]` 会把「上游没给
/// `isUsingOverage` 键」和「上游明确给了 `false`」压成同一个值，类型上就再也分不开——那正是本段
/// 明令禁止的那件事。宁可让调用方多写一次 `match`，也不要在类型里埋一个不可逆的信息丢失。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmRateLimit {
    /// 当前窗口判定（实测 `"allowed"`）。未知值见 [`LlmRateLimitStatus`]。
    pub status: LlmRateLimitStatus,
    /// 窗口周期（实测 `"five_hour"`）。`None` = 上游没给。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<LlmRateLimitWindow>,
    /// 额度重置时刻，**Unix 秒**（实测 `1786211400`）。`None` = 上游没给。
    ///
    /// # 单位与类型都是坑，写死在这里
    /// - 上游 `resetsAt` 是**秒**，而本模块其它所有时间戳（`created_ms` / `started_ms` /
    ///   `ended_ms`）都是**毫秒**。字段名带 `_secs` 就是为了让「忘了乘 1000」在**读代码时**
    ///   就露馅——否则手机上会显示 1970 年 1 月 21 日重置。
    /// - 类型是 `i64` 不是 `u32`：`u32` 的 Unix 秒 2106 年溢出，且与本模块其它时间戳的 `i64`
    ///   同类型，换算成毫秒后可直接与它们比较，不用来回 `from`。
    /// - **换算必须写成 `secs.checked_mul(1_000)`（或 `saturating_mul`），不许裸 `* 1_000`**：本字段
    ///   是**对端可控**值，取值可达 `i64::MAX`；裸乘法在 debug 构建下是 overflow panic、release 下
    ///   静默回绕成负数（额度条显示 1970 年或直接崩）。溢出即视为「上游给了垃圾值」，按 `None`
    ///   处理（不显示恢复时刻），不要拿回绕后的数去渲染。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_secs: Option<i64>,
    /// 超额计费状态（实测 `"rejected"`）。`None` = 上游没给。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<LlmOverageStatus>,
    /// 超额计费被禁用的原因（实测 `"out_of_credits"`）。`None` = 上游没给 / 未被禁用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_disabled_reason: Option<LlmOverageDisabledReason>,
    /// 当前是否正在吃超额额度（上游 `isUsingOverage`，实测 `false`）。
    ///
    /// `Option<bool>` 而非裸 `bool`：`None` = **上游没给这个键**，与实测 `Some(false)` 是两件事
    /// （见类型文档「全部字段可缺省」）。手机端把 `None` 渲染成「未在超额」就是本类型点名要避免的
    /// 错误计费提示；`None` 的正确处理是**不显示超额相关的任何文案**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub using_overage: Option<bool>,
}

/// 某个 agent 支持的可选能力。全部 `#[serde(default)]`，缺省即「不支持」，保证新增能力位不会让
/// 老端整帧解析失败。
///
/// **被控端怎么填**：优先用 headless `system.subtype = "init"` 行的 `capabilities` 数组做**真实
/// 能力探测**（实测值 `["interrupt_receipt_v1","interrupt_cancel_queued_v1","msg_lifecycle_v1"]`），
/// 比猜 CLI 版本号可靠得多——例如 `interrupt` 位应由 `capabilities` 含 `interrupt_receipt_v1`
/// 推出，而不是「≥ 某版本就当支持」。
///
/// **注意**：`capabilities` 只是探测**依据**，不是可上行内容；同一行 `init` 里的 `memory_paths` /
/// `plugins` / `mcp_servers` / 40 个工具名一律就地丢弃（模块文档铁律一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LlmAgentFeatures {
    /// 支持流式增量（否则只能整轮返回）。
    #[serde(default)]
    pub streaming: bool,
    /// 会产出 [`LlmBlock::Thinking`]（**只说明块会出现，不保证有正文**）。
    #[serde(default)]
    pub thinking: bool,
    /// 思考块**带得出可展示正文**（即会出现 [`LlmThinking::Text`]）。
    ///
    /// **实测 Claude Opus 5 = `false`**：`thinking` 字段恒为空串，`claude --help` 里没有任何开关
    /// 能打开（模块文档铁律三）。与 [`Self::thinking`] 分成两位不是冗余——手机端据此决定
    /// 「思考区是否给可展开的交互」：`thinking && !thinking_text` 时只画状态条，画了折叠箭头
    /// 而永远展不开是明确的产品缺陷。
    #[serde(default)]
    pub thinking_text: bool,
    /// 支持交互式权限审批（能发 [`LlmFrame::PermissionRequest`]）。
    ///
    /// **注意**：本机实测的 `claude --help` 中**没有** `--permission-prompt-tool`，故当前 Claude
    /// 分支应为 `false`（协议先行、实现后补）。控制端据此位决定是否展示「审批」入口，避免出现
    /// 永远等不到回应的 UI。
    #[serde(default)]
    pub interactive_permission: bool,
    /// 支持 `--resume` / `--fork-session` 续接历史会话。**未实测**（`--resume` 语义本轮未采样）。
    #[serde(default)]
    pub resume: bool,
    /// 支持附件。
    #[serde(default)]
    pub attachments: bool,
    /// 支持中断进行中的一轮。
    #[serde(default)]
    pub interrupt: bool,
}

/// 被控端可用的一个 agent。
///
/// **刻意不含** `tools` / `plugins` / `mcp_servers` / `slash_commands` / `memory_paths`：那些是
/// `init` 行里的本机环境信息，一律不上行（模块文档铁律一之 4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmAgentInfo {
    pub agent: LlmAgentKind,
    /// 是否真的可用（探测到可执行文件且能起来）。`false` 时控制端置灰但仍展示，便于用户知道
    /// 「装了就能用」。
    pub available: bool,
    /// CLI 版本号（如 `"2.1.226"`）。仅用于排障与能力兜底，**不作能力判据**（判据是
    /// [`LlmAgentFeatures`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 可选模型清单（空 = 用 CLI 默认）。
    #[serde(default)]
    pub models: Vec<String>,
    /// 本 agent **真正支持**的权限模式子集。
    #[serde(default)]
    pub permission_modes: Vec<LlmPermissionMode>,
    #[serde(default)]
    pub features: LlmAgentFeatures,
}

/// 对话运行态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmConvState {
    /// 子进程启动中。
    Starting,
    /// 空闲，可接收 [`LlmFrame::Send`]。
    Idle,
    /// 正在生成。
    Running,
    /// 卡在权限审批上，等 [`LlmFrame::PermissionReply`]。
    WaitingPermission,
    /// 已退出（终态）。
    Exited,
    /// 更高版本对端的未知状态（**只用于反序列化兜底，永不发送**）。
    #[serde(other)]
    Unknown,
}

/// 「这个对话是从哪个终端窗格拉起来的」弱关联。
///
/// **纯展示 / 跳转用途，绝不作为路由键**：对话的路由键恒为 [`ConvId`]。窗格可能早已关闭，而
/// `SessionId` 关闭后不复用，故这里存的是历史快照、**允许悬空**——控制端解析不到对应窗格时
/// 静默忽略即可，不得据此判定对话失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRef {
    pub tab_id: crate::remote::TabId,
    pub session_id: crate::remote::SessionId,
}

/// 对话元信息（列表项 + 基线帧共用）。
///
/// 桌面端不做会话 UI（M7 §12-⑫ 拍板：只留一个图标标识），故本结构同时是桌面端**图标弹层与
/// 结构化审计日志的唯一数据来源**：`agent` + `cwd` + `created_ms` 就是审计条目的「谁 / 在哪 /
/// 何时」三要素。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmConvMeta {
    pub conv_id: ConvId,
    pub conv_generation: ConvGeneration,
    pub agent: LlmAgentKind,
    /// 工作目录（CLI 的 cwd，也是附件与 transcript 的定位依据）。
    pub cwd: LlmPath,
    /// 展示标题（被控端取首条用户消息前若干字，或用户自定义）。
    pub title: LlmText,
    pub state: LlmConvState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<LlmPermissionMode>,
    /// CLI 自己的会话 id（Claude 的 `--session-id`），供 `--resume` 续接与 transcript 定位。
    /// **与 [`ConvId`] 是两个空间**：前者属 CLI、后者属本协议，不可互换。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_session_id: Option<String>,
    /// 可选的终端窗格来源（见 [`PaneRef`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PaneRef>,
    /// 当前（最新）轮次号；`0` 表示尚无任何一轮。
    pub cur_turn: TurnNo,
    /// 创建时刻（Unix 毫秒）。
    pub created_ms: i64,
    /// 最后活动时刻（Unix 毫秒）。
    pub updated_ms: i64,
    /// 累计用量。
    #[serde(default)]
    pub usage: LlmUsage,
}

/// 一轮的**定稿记录**（历史分页与 [`LlmFrame::TurnSnapshot`] 用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmTurnRecord {
    pub turn: TurnNo,
    /// 用户侧块（文本 + 附件回显）。
    #[serde(default)]
    pub user: Vec<LlmBlockEntry>,
    /// 助手侧块（按产出顺序，即渲染顺序）。
    #[serde(default)]
    pub assistant: Vec<LlmBlockEntry>,
    /// 停止原因；`None` = 本轮尚未结束（拉取进行中的那一轮时会出现）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<LlmStopReason>,
    /// 成败判定（见 [`LlmTurnOutcome`]，**不看 `stop`、更不看上游 `subtype`**）。
    #[serde(default)]
    pub outcome: LlmTurnOutcome,
    #[serde(default)]
    pub usage: LlmUsage,
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    /// `true` = 本记录是完整定稿；`false` = 因积压降级丢过中间增量（见 [`LlmDeltaItem::Dropped`]），
    /// 内容可能有缺口。
    #[serde(default)]
    pub complete: bool,
    /// 本记录**为满足单帧字节预算而删掉的块数**（`0` = 一块没删，常态）。
    ///
    /// # 为什么自愈路径也要能声明「装不下」
    /// 本结构是 [`LlmFrame::TurnSnapshot`] 与 [`LlmFrame::HistoryPage`] 的载荷，而
    /// `TurnSnapshot` 是缺口的**唯一**补齐通道。一轮 128 次工具调用（agentic 会话的常态）即使
    /// 每个 `output` 都按 [`LLM_TOOL_OUTPUT_MAX_BYTES`] 夹过，整轮仍达 ~4.01 MiB，顶穿中继
    /// 4 MiB 单帧上限 → 断整条 WS（或 QUIC 侧静默丢帧）→ **本来用来补缺口的那条帧自己丢了**，
    /// 缺口永久补不上。
    ///
    /// 于是被控端必须能交付一个**删减过但可观测**的快照：按
    /// [`LLM_TURN_SNAPSHOT_MAX_BYTES`] 从**最老的助手块**起删（保留轮尾，那是用户最想看的），
    /// 删几块写几块。控制端据此展示「本轮有 N 块因过大未能同步」，而不是显示一段莫名其妙的空白。
    ///
    /// 与 [`Self::complete`] 的分工：`complete = false` 说的是「**流式过程中**丢过增量」，
    /// 本字段说的是「**这一帧装不下**，定稿时主动删的」。两者可同时发生，语义不重叠。
    #[serde(default)]
    pub blocks_omitted: u32,
}

/// 流式增量项。多条打包进一个 [`LlmFrame::Delta`]（见 [`LLM_DELTA_FLUSH_MS`] 合并窗口）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmDeltaItem {
    /// 新块开始。`entry.block` 携带该块的定型头部：[`LlmBlock::Text`] 的 `text` 为首片（可空串），
    /// [`LlmBlock::ToolUse`] 则已含完整 `name` / `input`（**工具入参不流式**——上游是整块给出的）。
    ///
    /// [`LlmBlock::Thinking`] 的头部就是它的**终态**（[`LlmThinking::Omitted`] / `Redacted` 根本
    /// 没有文本可追加），只有 [`LlmThinking::Text`] 分支才会跟 [`Self::TextAppend`]。
    BlockStart { entry: LlmBlockEntry },
    /// 文本追加——**高频帧的主力**。仅对 [`LlmBlock::Text`] 与 `Thinking{content: `
    /// [`LlmThinking::Text`]`}` 两种块有效；对 `Thinking{Omitted}` / `Thinking{Redacted}`
    /// 发本项是协议违例（那两个分支在类型上就没有可追加的目标），控制端收到应忽略。
    ///
    /// 合并器在打包时把同一 `block_id` 的连续 `TextAppend` **拼成一条**，把「每 token 一帧」
    /// 压成「每帧多 token」，这是省流量的大头。
    TextAppend { block_id: BlockId, text: LlmText },
    /// 块定稿。
    ///
    /// `block = None`（常态，省流量）：按控制端流式拼接的结果定稿。
    /// `block = Some(..)`：被控端**已知**本块发生过丢弃 / 降级，携带终态完整块**覆盖修正**。
    BlockEnd {
        block_id: BlockId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block: Option<LlmBlock>,
    },
    /// 声明「本块有 `bytes` 字节的中间增量被丢弃了」。
    ///
    /// 让降级**可观测**：控制端可据此展示「内容有缺口，点击补齐」并发 [`LlmFrame::TurnFetch`]。
    /// （反面教材：[`crate::remote::RemoteFrame::PaneOp`] 是 fire-and-forget 无结果帧，
    /// 「对端不认识这个变体」与「执行了但还没推状态」在调用方看来完全无法区分。）
    Dropped { block_id: BlockId, bytes: u64 },
    /// 更高版本对端的未知增量项（**只用于反序列化兜底，永不发送**）。
    #[serde(other)]
    Unknown,
}

/// 权限审批决定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmPermissionDecision {
    /// 允许。`remember = true` 表示本对话内后续同名工具自动放行。
    Allow {
        #[serde(default)]
        remember: bool,
    },
    /// 拒绝。
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<LlmText>,
    },
    /// 更高版本对端的未知决定（**只用于反序列化兜底，永不发送**）。
    ///
    /// **被控端收到必须按拒绝处理**——「认不出的审批结果」在安全上只能保守解释，绝不能当放行。
    #[serde(other)]
    Unknown,
}

/// 一次权限审批最终由谁裁决（用于把「已在别处处理」广播给控制端）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LlmPermissionResolvedBy {
    /// 用户显式回复。
    User,
    /// 超过 [`LLM_PERMISSION_TIMEOUT_SECS`] 未回复，被控端自动拒绝。
    Timeout,
    /// 被 `permission_mode` 策略自动放行 / 拒绝，未曾询问用户。
    PolicyAuto,
    /// 对话被关闭 / 进程退出，请求作废。
    ConvClosed,
    /// 更高版本对端的未知裁决方（**只用于反序列化兜底，永不发送**）。
    #[serde(other)]
    Unknown,
}

// ── 帧 ────────────────────────────────────────────────────────────────────────

/// LLM 对话子协议帧。装进 [`crate::remote::RemoteFrame::Llm`] 经服务端**盲转**（服务端零改动）。
///
/// **内部标签 + `other` 兜底**是本枚举存在的全部理由：未知 `op` 降级成 [`Self::Unknown`] 而非
/// 整帧报废。新增变体时只需保证：
///
/// - 用**结构体变体或 unit 变体**（内部标签**不支持** newtype / tuple 变体——那正是
///   [`crate::remote::RemoteFrame`] 无法整体改内部标签的原因：`Output(Vec<u8>)` / `Echo(String)`
///   在内部标签下根本无法表达）；
/// - 给可选字段挂 `#[serde(default)]`，否则老端发来的旧帧会因缺字段而整帧失败；
/// - 不要引入裸 [`serde_json::Value`] 字段（模块文档铁律一）。
///
/// 方向标注：`控→被` = 控制端(手机) → 被控端(PC)；`被→控` 反之。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum LlmFrame {
    // ── 能力协商（唯一可执行的版本门）──────────────────────────────────────
    /// 控→被：会话建立后立即发。老端不认识 `RemoteFrame::Llm` → 收不到 → 控制端
    /// [`LLM_HELLO_TIMEOUT_SECS`] 秒超时 → UI 明确提示升级 PC 端（而非静默无反应）。
    ///
    /// **超时判据必须是「没收到 [`Self::HelloAck`]」**，不能依赖任何错误回执——老端整帧丢弃后
    /// **不会回任何东西**。握手状态还必须随会话重建复位（重挂 / QUIC↔中继翻转），否则一次偶发
    /// 超时会让 LLM 面在整个会话生命周期内假死。
    Hello {
        /// 本子协议版本 [`LLM_PROTO_VERSION`]。
        llm_proto: u32,
        /// 可选子能力标签（如 `"perm"` / `"attach"` / `"thinking"`），供细粒度灰度——
        /// 比单调版本号表达力强。
        #[serde(default)]
        caps: Vec<String>,
    },
    /// 被→控：能力应答，顺带回带 agent 清单与现存对话清单（省两个往返）。
    HelloAck {
        llm_proto: u32,
        #[serde(default)]
        caps: Vec<String>,
        #[serde(default)]
        agents: Vec<LlmAgentInfo>,
        #[serde(default)]
        convs: Vec<LlmConvMeta>,
        /// **额度状态的基线**：被控端最近一次观测到的 [`LlmRateLimit`]（没观测过则 `None`）。
        ///
        /// # 为什么基线在这里而不在 [`LlmConvMeta`] 里
        /// [`Self::RateLimit`] 是纯播报帧，只在上游**发生**限流事件时才有。没有基线的话，手机端
        /// 从挂载到下一次事件之间（可能是几十分钟）额度条只能是空的——这正是本模块反复强调的
        /// 「基线帧必先于增量」在一条新通路上的同一个坑。
        ///
        /// 放在握手帧而不是 [`LlmConvMeta`]：额度是**账号级**的，塞进每个对话的元信息里会在
        /// 对话清单里复制 N 份同一个值，且必然出现 N 份不一致的过期副本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit: Option<LlmRateLimit>,
        /// 上面那份基线的**观测时刻**（Unix 毫秒），与 [`Self::RateLimit`] 的 `observed_ms`
        /// **同一时钟域**（都是被控端 PC 的墙钟）。`rate_limit` 为 `Some` 时必填。
        ///
        /// # 没有它，latest-wins 在基线这条通路上就执行不了
        /// [`Self::RateLimit`] 刻意不领 `seq`，去乱序**全靠** `observed_ms` 比大小。若基线不带
        /// 时间戳，控制端拿到基线后对任何后到的播报都无从判新旧：一条在会话重建 / QUIC↔中继翻转
        /// 期间迟到的**旧**播报会无条件覆盖**更新**的基线，而额度事件间隔可达数小时，错误状态会
        /// 长期驻留在额度条上。
        ///
        /// 控制端唯一的自救本来是给基线打自己的 `now()`——但那是**手机**的墙钟，拿去和 PC 的
        /// `observed_ms` 比是引入一个新 bug（两端时钟差几分钟很常见）。跨设备比时间戳这件事本身
        /// 不成立，所以时间戳必须由**观测方**（PC）打，随基线一起下发。
        ///
        /// 与 [`Self::Detach`] 补代号是同一类修补：**不变量（latest-wins）必须在类型上执行得了**，
        /// 而不是写在文档里靠实现者自觉。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit_observed_ms: Option<i64>,
    },

    // ── 发现 ────────────────────────────────────────────────────────────────
    /// 控→被：列出可用 agent。`req_id` 复用控制端现有的 `RemoteWs::next_req_id()`
    /// （单调、跳 0 哨兵、**进程内永不重置**——这条隐式不变量是陈旧应答去重的基础，勿另起计数器）。
    ListAgents { req_id: u64 },
    /// 被→控：agent 清单。
    AgentList {
        req_id: u64,
        agents: Vec<LlmAgentInfo>,
    },
    /// 控→被：列出现存对话。
    ListConvs { req_id: u64 },
    /// 被→控：对话清单。
    ConvList {
        req_id: u64,
        convs: Vec<LlmConvMeta>,
    },

    // ── 生命周期 ────────────────────────────────────────────────────────────
    /// 控→被：按 cwd 启动一个 headless 对话。
    Start {
        req_id: u64,
        agent: LlmAgentKind,
        /// 工作目录（被控端校验存在性与越权）。
        cwd: LlmPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<LlmPermissionMode>,
        /// 续接已有 CLI 会话（映射 `--resume <session-id>`）。**`--resume` 语义本轮未实测**，
        /// 实现前需采样确认。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
        /// 与 `resume` 配合，映射 `--fork-session`（不覆盖原会话）。
        #[serde(default)]
        fork_session: bool,
        /// 映射 `--append-system-prompt`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        append_system_prompt: Option<LlmText>,
        /// 映射 `--allowedTools`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
        /// 可选的终端窗格来源（见 [`PaneRef`]）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<PaneRef>,
    },
    /// 被→控：**基线帧**。会话已启动，`conv_id` / `conv_generation` 在 `meta` 里。
    ///
    /// `attach_dir` 是本对话的附件暂存目录（控制端用 `PutBegin` 往这里传图，再在 [`Self::Send`]
    /// 里引用返回的绝对路径）。`seq` 是本对话增量水位起点。
    ConvStarted {
        req_id: u64,
        meta: LlmConvMeta,
        attach_dir: LlmPath,
        seq: u64,
    },
    /// 被→控：启动失败。**与 [`Self::ConvStarted`] 互斥**——不用「`err` 优先于 `status`」那种并列
    /// `Option<err>` 形状（那是 `PutResult` / `NewTabResult` 留下的坑：类型上不体现优先级、全靠
    /// 注释约定），这里用互斥变体从类型上排除歧义。
    ConvFailed {
        req_id: u64,
        code: LlmErrorCode,
        message: LlmText,
    },
    /// 控→被：发一条用户消息。附件字节**不在本帧内**（见 [`LlmAttachment`]）。
    Send {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        req_id: u64,
        text: LlmText,
        #[serde(default)]
        attachments: Vec<LlmAttachment>,
    },
    /// 控→被：中断进行中的一轮。
    Interrupt {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        req_id: u64,
    },
    /// 控→被：结束对话并回收子进程。
    Close {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        req_id: u64,
    },
    /// 被→控：对话（子进程）已退出。终态。
    ConvExited {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        seq: u64,
        reason: LlmExitReason,
        /// 子进程退出码。**别只看它**：实测认证失败时 `exit code = 1` 且 stderr 为空，
        /// 真正的失败原因只在 stdout 事件流里（见 [`LlmExitReasonKnown::ProcessExited`]）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<LlmText>,
    },

    // ── 订阅与重连补齐（与 part3d 的 SubscribeSession → SubscriptionStarted 同构）──
    /// 控→被：订阅一个对话的实时流，并声明本端已知进度用于增量补齐。
    ///
    /// 首次订阅填 `known_generation = 0`（哨兵：一定不匹配 → 被控端回全量基线）。
    Attach {
        conv_id: ConvId,
        #[serde(default)]
        known_generation: ConvGeneration,
        #[serde(default)]
        known_seq: u64,
        #[serde(default)]
        known_turn: TurnNo,
    },
    /// 被→控：**基线帧，必先于该对话任何 [`Self::Delta`] 到达**（D3 保序）。
    ///
    /// `seq` 是发出本基线那一刻的水位：控制端**丢弃 `seq` 不大于本值的迟到 Delta**，由此把 QUIC
    /// 与中继两条通路的切换乱序自动收敛。
    /// `resync_required = true` 表示代不匹配或缺口过大，控制端应清空本地对话状态后用
    /// [`Self::HistoryReq`] / [`Self::TurnFetch`] 重建。
    Attached {
        meta: LlmConvMeta,
        seq: u64,
        #[serde(default)]
        resync_required: bool,
    },
    /// 控→被：取消订阅（手机切走该对话；被控端据此停发增量但**不结束**对话）。
    ///
    /// `conv_generation` 带 `#[serde(default)]`，`0` = 老控制端没带、被控端按「不校验」兼容处理
    /// （过渡语义，等两端都升级后可收紧成必校验）。
    ///
    /// **为什么这条也要带代号**：[`ConvGeneration`] 的不变量是「代不匹配的帧一律丢弃」，本帧曾是
    /// 唯一指向具体对话却不带代号的帧，**类型上就执行不了那条不变量**。后果是对话运行时重建
    /// （进程重启 / 恢复 / 换 CLI）换代之后，一条翻转期间迟到的旧代 `Detach` 会把**新一代**的订阅
    /// 退掉——手机上表现为「明明停在这个对话页，增量却停了」，且只在直连↔中继翻转瞬间偶现。
    Detach {
        conv_id: ConvId,
        #[serde(default)]
        conv_generation: ConvGeneration,
    },

    // ── 流式增量 ────────────────────────────────────────────────────────────
    /// 被→控：一批增量。`seq` 在同一 `(conv_id, conv_generation)` 内**严格递增且连续**；
    /// 控制端发现断裂即发 [`Self::TurnFetch`] 拉整轮快照覆盖重建。
    Delta {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        seq: u64,
        turn: TurnNo,
        items: Vec<LlmDeltaItem>,
    },
    /// 控→被：确认已消费到 `seq`，维持 [`LLM_DELTA_WINDOW`] 滑动窗口背压。
    ///
    /// **与文件传输 ACK 背压的关键差异（必须写死在这里，实现时极易照抄错）**：文件传输窗口满时
    /// 可以**停止读磁盘**；而这里的源是 CLI 子进程 stdout，**停读会填满管道缓冲区并阻塞 / 卡死
    /// CLI 进程本身**。所以窗口满时被控端必须继续 read 走、转为本地积压；积压超
    /// [`LLM_BACKLOG_MAX_BYTES`] 时丢弃中间增量、只保留终态块，并发 [`LlmDeltaItem::Dropped`]
    /// 与 [`Self::TurnEnded`] 的 `truncated = true`，控制端据此发 [`Self::TurnFetch`] 补齐。
    /// 这样「不阻塞 CLI」与「内存不无界增长」同时成立。
    DeltaAck {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        seq: u64,
    },
    /// 被→控：新一轮开始（用户消息已被 CLI 接收，回显归一化后的用户块）。
    TurnStarted {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        seq: u64,
        turn: TurnNo,
        user: Vec<LlmBlockEntry>,
        started_ms: i64,
    },
    /// 被→控：一轮结束，带停止原因、成败判定与用量 / 花费。
    ///
    /// **成败看 `outcome`，不看 `stop`，更不看上游 `subtype`**（见 [`LlmTurnOutcome`]）。
    /// `truncated = true` 表示本轮曾因积压丢过增量，控制端应发 [`Self::TurnFetch`] 补齐。
    TurnEnded {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        seq: u64,
        turn: TurnNo,
        stop: LlmStopReason,
        /// 权威成败判定。`#[serde(default)]`：老被控端不带本字段时按「正常收尾」解释。
        #[serde(default)]
        outcome: LlmTurnOutcome,
        usage: LlmUsage,
        ended_ms: i64,
        #[serde(default)]
        truncated: bool,
    },
    /// 被→控：**账号额度状态**播报（上游 `rate_limit_event`，见 [`LlmRateLimit`]）。
    ///
    /// # 为什么是独立帧，不是块
    /// 它在实测流里**夹在两条 `assistant` 行之间**、不属于任何一条消息，且可以在**没有任何一轮
    /// 在跑**的时候到达。做成 [`LlmBlock`] 就得硬塞进某一轮的块序列里——那既编不出合理的
    /// `block_id`，又会让「气泡流」里混进一条不是对话内容的东西。它是**状态**，归宿是手机端顶部
    /// 的额度条，不是气泡。
    ///
    /// # 本帧**不占用 [`Self::Delta`] 的 `seq` 空间**
    /// 这条是硬约束：`Delta.seq` 的不变量是「同一 `(conv_id, conv_generation)` 内严格递增且
    /// **连续**，一断裂就发 [`Self::TurnFetch`]」。若本帧也领一个 seq，控制端每收到一次限流播报
    /// 就会看见一个「缺口」并触发一次整轮快照拉取——一条纯状态广播把自愈通道变成了刷屏器。
    /// 故本帧刻意**没有 `seq` 字段**。
    ///
    /// 代价是它与增量流之间无序：改用 `observed_ms` 做 **latest-wins**（控制端丢弃比已记录更旧
    /// 的播报），这在 QUIC↔中继翻转时足够——额度状态是幂等的当前值，不是需要按序回放的事件。
    ///
    /// **握手基线走的是同一套比较**：[`Self::HelloAck`] 的 `rate_limit` 配了一个成对的
    /// `rate_limit_observed_ms`（同一时钟域），控制端对基线与播报**用同一个 `observed_ms` 判新旧**，
    /// 不许对基线另打手机本地的 `now()`（跨设备时钟比较是新 bug，理由写在那个字段上）。
    ///
    /// # 语义是**账号级**的，不是对话级
    /// `conv_id` / `conv_generation` 只表示「这条播报是在哪个对话的流上观测到的」（保留代号是为了
    /// 让「代不匹配一律丢弃」这条不变量在类型上可执行，教训见 [`Self::Detach`]）。控制端应据此
    /// 维护**一个全局额度条**，而不是每个对话一份——同一账号下 N 个对话看到的是同一份额度。
    RateLimit {
        conv_id: ConvId,
        #[serde(default)]
        conv_generation: ConvGeneration,
        /// 被控端**观测到**该事件的时刻（Unix 毫秒），用于 latest-wins 去乱序。
        observed_ms: i64,
        info: LlmRateLimit,
    },

    // ── 历史与自愈（按需分页，对齐 part3d）──────────────────────────────────
    /// 控→被：拉单轮定稿快照（seq 断裂 / `truncated` / 直连切换后的自愈通道）。
    TurnFetch {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        req_id: u64,
        turn: TurnNo,
    },
    /// 被→控：单轮快照。**基线语义**：控制端以本记录整体覆盖该轮本地状态，并按 `seq` 更新水位。
    ///
    /// **`record` 未必是完整的一轮**：被控端先按 [`LLM_TURN_SNAPSHOT_MAX_BYTES`] 夹紧字节，
    /// 删掉的块数写在 [`LlmTurnRecord::blocks_omitted`]。控制端**必须读那个字段**并把「有 N 块
    /// 太大未同步」显示出来——本帧是缺口的唯一补齐通道，它自己超限就会被中继断连 / QUIC 丢帧，
    /// 缺口永久补不上（见 [`LLM_TURN_SNAPSHOT_MAX_BYTES`] 的长注释）。
    TurnSnapshot {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        req_id: u64,
        seq: u64,
        record: LlmTurnRecord,
    },
    /// 控→被：向前翻历史，取 `before_turn` 之前的最多 `max_turns` 轮（`max_turns` 由被控端夹到
    /// [`LLM_HISTORY_MAX_TURNS_PER_PAGE`]）。
    HistoryReq {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        req_id: u64,
        /// 取严格小于本轮号的历史；填 `cur_turn + 1` 表示从最新往回取。
        before_turn: TurnNo,
        max_turns: u16,
    },
    /// 被→控：一页历史（`turns` 按轮号**升序**）。
    ///
    /// **`turns.len()` 常小于请求的 `max_turns`**：被控端先按 [`LLM_HISTORY_PAGE_MAX_BYTES`]
    /// 字节预算夹紧再按轮数夹紧。控制端**必须以 `oldest_turn` 推进游标、以 `has_more` 判断是否
    /// 到底**，绝不能假设「请求 N 轮就回 N 轮」——否则会卡在补不齐的缺口上永久空白
    /// （part3d `clear_inflight_gap` 踩过同一个坑）。
    HistoryPage {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        req_id: u64,
        /// 本页最老的轮号（即下次 `before_turn` 应填的值）。
        oldest_turn: TurnNo,
        turns: Vec<LlmTurnRecord>,
        has_more: bool,
    },

    // ── 权限审批 ────────────────────────────────────────────────────────────
    /// 被→控：请求用户批准一次工具调用。仅当 [`LlmAgentFeatures::interactive_permission`]
    /// 为 `true` 时可能出现。
    PermissionRequest {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        /// 审批请求号（**被控端**分配，与 `req_id` 不同空间，别拿去和 `req_id` 比）。
        request_id: u64,
        turn: TurnNo,
        tool: String,
        /// 待批准的工具入参（见 [`LlmToolInput`]：`Debug` 脱敏，`Bash{command}` 里的密钥不进日志）。
        input: LlmToolInput,
        /// 人类可读的影响预览（如「将写入 3 个文件」）。
        preview: LlmText,
        /// 剩余有效秒数（见 [`LLM_PERMISSION_TIMEOUT_SECS`]）。
        expires_in_secs: u32,
    },
    /// 控→被：用户的审批结果。
    PermissionReply {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        request_id: u64,
        decision: LlmPermissionDecision,
    },
    /// 被→控：该审批已被裁决（含超时自动拒绝 / 策略自动放行 / 对话关闭作废）。控制端据此收起
    /// 审批 UI，**不依赖自己是否发过 [`Self::PermissionReply`]**（那条可能在翻转中丢了）。
    ///
    /// 本帧同时是桌面端安全审计的锚点之一（M7 §12-⑫：桌面端只留图标，可回溯性靠结构化日志）。
    PermissionResolved {
        conv_id: ConvId,
        conv_generation: ConvGeneration,
        request_id: u64,
        decision: LlmPermissionDecision,
        by: LlmPermissionResolvedBy,
    },

    /// 被→控：与具体对话无关或无法归入上述应答的错误（`conv_id` / `req_id` 尽力回带）。
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conv_id: Option<ConvId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        req_id: Option<u64>,
        code: LlmErrorCode,
        message: LlmText,
    },

    /// 更高版本对端发来的未知 `op`（**只用于反序列化兜底，永不发送**）。
    ///
    /// 这是本子协议前向兼容的根：收到它意味着对端有本端不支持的新能力，应 `log::debug` 后忽略、
    /// 必要时提示用户升级——而不是让整帧解析失败。
    #[serde(other)]
    Unknown,
}

// ── 常量 ──────────────────────────────────────────────────────────────────────

/// LLM 子协议版本。与 [`crate::PROTOCOL_VERSION`] **独立演进**（后者管控制面 / 终端数据面）。
/// 因 [`LlmFrame`] 自带 `other` 兜底，本版本号只用于能力协商与排障，**不作阻断依据**。
pub const LLM_PROTO_VERSION: u32 = 1;

/// 增量合并窗口（毫秒）。33 ms ≈ 30 fps，与 UI 刷新率对齐：再快人眼无感，白白增加帧数与中继
/// CPU（服务端每帧一次 JSON parse + serialize，非零拷贝）。
pub const LLM_DELTA_FLUSH_MS: u64 = 33;

/// 单个 [`LlmFrame::Delta`] 的 JSON 字节夹紧上限。达到即立刻 flush 并开新帧。
/// 128 KiB 远低于服务端 4 MiB 单帧上限（`server/lumen-server/src/ws.rs` 的 `MAX_WS_MESSAGE`）。
///
/// # 它是**每帧上限**，不是「攒到这么多就发一次」的触发阈值
/// 两者听起来像同一件事，落到实现上差一个数量级：只当触发阈值用的话，一次
/// `pump` 排空整条 runner 事件通道（cap 4096）后仍然只产出**一帧**，字节数不设防，
/// 直接顶穿 `MAX_WS_MESSAGE` 把整条 WS 打断（终端镜像一起断）。被控端侧的落点是
/// `remote_ws/llm.rs` 的 `LlmPlane::flush_conv`——它按本常量**拆帧**，每批各领一个 `seq`。
pub const LLM_DELTA_MAX_BYTES: usize = 128 * 1024;

/// 在途未 ACK 的 [`LlmFrame::Delta`] 帧数窗口（背压）。对齐 [`crate::remote::FETCH_WINDOW`]
/// 的成熟范式。
///
/// **类型是 `u32` 不是 `u64`**：本仓库惯例是「计数类上限一律 `u32`」
/// （[`crate::remote::FETCH_WINDOW`] / [`crate::remote::REMOTE_MAX_SESSIONS`] /
/// `LIST_DIR_RECURSIVE_MAX_ENTRIES`），`u64` 留给字节数与长度（`FETCH_MAX_LEN` / `EDIT_MAX_LEN`）。
/// 写成 `u64` 会让实现侧的在途计数器跟着变 `u64`，与既有背压代码类型不一致、平白多一层 `u64::from`。
///
/// **值取 16 而非 [`crate::remote::FETCH_WINDOW`] 的 8，不是笔误**：LLM 增量帧远小于文件块
/// （[`LLM_DELTA_MAX_BYTES`] 128 KiB vs `FETCH_MAX_LEN`），窗口放宽一倍仍远低于中继内存压力线。
pub const LLM_DELTA_WINDOW: u32 = 16;

/// 窗口满时被控端本地积压的字节上限。超出即丢弃中间增量、只保留终态块，并发
/// [`LlmDeltaItem::Dropped`] + [`LlmFrame::TurnEnded`] 的 `truncated: true`。
///
/// **不可改成「停止读子进程 stdout」**：那会填满管道缓冲区并卡死 CLI 进程本身
/// （见 [`LlmFrame::DeltaAck`] 的长注释）。
///
/// # 为什么是 2 MiB 而不是和 `MAX_WS_MESSAGE` 一样的 4 MiB
/// 原值就是 4 MiB，与 `server/lumen-server/src/ws.rs` 的 `MAX_WS_MESSAGE` **恰好相等**。
/// 两个上限取值相等是个陷阱：积压削到上限之内（`shed_backlog` 只削到本值的一半，且终态项
/// 无条件保留，故实际驻留可到 ~2 MiB+）之后，ACK 一到就要把这堆东西发出去，JSON 转义还会
/// 再放大一截——「刚好合规的积压」直接长成「超限的帧」。
///
/// 现在 flush 侧已按 [`LLM_DELTA_MAX_BYTES`] 拆帧，单帧不会再超限；本值降到 2 MiB 是**第二道**
/// ——让积压总量本身就明显低于单帧上限，两道防线不共用同一个数字。
pub const LLM_BACKLOG_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 单页历史最多轮数（被控端对 [`LlmFrame::HistoryReq`] 的 `max_turns` 的硬夹紧）。
pub const LLM_HISTORY_MAX_TURNS_PER_PAGE: u16 = 20;

/// 单页历史字节预算。**先于轮数生效**——满载 20 轮实测可达 ~1.1 MB（见测试
/// `llm_历史分页满载单帧不超上限`），故短页是正常结果，控制端必须以 `oldest_turn` / `has_more`
/// 推进游标（见 [`LlmFrame::HistoryPage`]）。
pub const LLM_HISTORY_PAGE_MAX_BYTES: usize = 1024 * 1024;

/// 单轮快照（[`LlmFrame::TurnSnapshot`] 的 `record`）字节预算。**先于块数生效**。
///
/// # 为什么自愈通道也必须有预算——这是唯一一条补缺口的路
/// `TurnSnapshot` 是「`Delta.seq` 断裂 / `TurnEnded.truncated` → [`LlmFrame::TurnFetch`] → 整轮
/// 覆盖重建」的**唯一**自愈路径。它一旦自己超限，缺口就永久补不上：中继侧超 4 MiB 是**断整条
/// WS**（`server/lumen-server/src/ws.rs` 的 `MAX_WS_MESSAGE`），QUIC 侧是**静默丢帧**——两种
/// 失败都表现为「手机上那段气泡永远空白」，且只在长轮次上偶现。
///
/// **不是理论风险**：即使每个 [`LlmBlock::ToolResult`] 的 `output` 都已按
/// [`LLM_TOOL_OUTPUT_MAX_BYTES`] 夹紧，128 个块的一轮实测就是 4 209 693 B = 4.01 MiB，**已顶穿 4 MiB**
/// （见测试 `llm_单轮快照满载单帧不超上限`）。一轮 128 次工具调用对 agentic 会话是常态。
///
/// 与 [`LLM_HISTORY_PAGE_MAX_BYTES`] 同量级、同语义（先按字节夹、再按条目夹）；超预算时被控端
/// **必须**删减块并把删掉的条数写进 [`LlmTurnRecord::blocks_omitted`]，**不得静默丢**。
pub const LLM_TURN_SNAPSHOT_MAX_BYTES: usize = 1024 * 1024;

/// 单个 [`LlmBlock::ToolResult`] 输出的字节上限，超出截断并填 `truncated_bytes`。
pub const LLM_TOOL_OUTPUT_MAX_BYTES: usize = 32 * 1024;

/// 单个 [`LlmBlock::ToolUse`] 的 `input`（[`LlmToolInput`]）序列化后的字节上限。
///
/// # 为什么入参也要夹紧——它曾是全模块唯一「无上限且结构上无法声明截断」的字段
/// Claude 的 `Write` / `Edit` 工具，`input` 里装的就是**整份文件正文**。实测
/// `Delta{items:[BlockStart{ToolUse{Write, input.content = 5 MiB}}]}` 序列化后 5.00 MiB，直接顶穿
/// 服务端 4 MiB 单帧上限；哪怕只有 1 MiB 正文，也已是 [`LLM_DELTA_MAX_BYTES`] 的 8 倍。
///
/// **[`LLM_DELTA_MAX_BYTES`] 那条「达到即 flush 并开新帧」在这里救不了场**：超限的是**单个
/// item**，flush 拆不开；而 [`LlmDeltaItem::BlockStart`] 已写死「工具入参不流式」（上游整块给出）。
///
/// **超限时被控端必须怎么做**（这条是硬要求，不是建议）：保留 `call_id` / `name` / `title`，
/// 只把 `input` 换成**裁剪后的摘要**（例如保留除超长值以外的全部键、超长值替换成长度占位），
/// 并把被裁掉的字节数填进 [`LlmBlock::ToolUse`] 的 `truncated_bytes`——**不得丢掉整个块**。
/// 丢块会让工具卡片凭空消失、后续 [`LlmBlock::ToolResult`] 的 `call_id` 配不上对，正是仓库
/// 明令禁止的无声降级。
pub const LLM_TOOL_INPUT_MAX_BYTES: usize = 64 * 1024;

/// 开放式 C 型枚举 `Other(String)` 的线上字节夹紧上限（模块文档铁律一之 3）。
///
/// 存在的唯一目的是**堵住「反正保留原文」这个后门**：没有它，被控端一时省事就会把上游的整段
/// 错误文本 / hook 输出塞进 `Other`，白名单转发当场破功。64 字节对任何机器可读标识都绰绰有余
/// （实测最长的是 `"interrupt_cancel_queued_v1"`，26 字节）。
///
/// # 它在哪里**真的**生效，在哪里不生效
/// - **发送侧生效**：被控端只许用各枚举的 `from_wire_clamped`（内部调 [`clamp_enum_wire`]）落
///   `Other`，超长在那里被截。
/// - **接收侧刻意不校验**：`untagged` 的 `Deserialize` 里加长度校验会把「对端发来超长值」变成
///   **整帧 `Err`**——比留着原文更糟（整帧丢弃 vs 一个字段偏长）。故收进来的 `Other` 长度不受
///   本端控制，日志侧由手写的 `Debug`（只打前 [`LLM_ENUM_DEBUG_HEAD_BYTES`] 字节 + 总长）兜底。
pub const LLM_ENUM_WIRE_MAX_BYTES: usize = 64;

/// 开放式 C 型枚举 `Other(String)` 在 `Debug` 里最多显示的字节数（后面接 `…` 与总长）。
///
/// 这是**最后一道**兜底：收进来的 `Other` 不受 [`LLM_ENUM_WIRE_MAX_BYTES`] 约束（见上），
/// 若 `Debug` 是 derive 的，一个恶意 / 图省事的对端塞进来的整段 hook 输出会随
/// `format!("{frame:?}")` 原样写进审计日志文件。16 字节足够认出是哪个标识，又不够泄漏内容。
pub const LLM_ENUM_DEBUG_HEAD_BYTES: usize = 16;

/// 单个被控端同时存活的对话上限（防控制端反复 [`LlmFrame::Start`] fork 出无界子进程；对齐
/// [`crate::remote::REMOTE_MAX_SESSIONS`] 的防护思路）。超限回 [`LlmErrorCodeKnown::LimitReached`]。
pub const LLM_MAX_CONVS: u32 = 8;

/// 控制端等待 [`LlmFrame::HelloAck`] 的超时秒数。超时即判定对端不支持 LLM 面，UI 提示升级 PC 端。
pub const LLM_HELLO_TIMEOUT_SECS: u64 = 5;

/// 权限审批未回复的自动拒绝超时秒数。
pub const LLM_PERMISSION_TIMEOUT_SECS: u32 = 120;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::RemoteFrame;

    /// 把一条 LLM 帧装进外层信封。[`RemoteFrame::Llm`] 持的是 `Box<LlmFrame>`（理由见该变体的
    /// 文档：不装箱会把 `RemoteFrame` 从 144 B 顶到 312 B），`Box` 对 serde 完全透明、线格式不变。
    fn 装帧(frame: LlmFrame) -> RemoteFrame {
        RemoteFrame::Llm(Box::new(frame))
    }

    /// 拆回内层帧。`Box` 不支持穿透模式匹配，故解包收在这里，不让每个测试各写一遍。
    fn 拆帧(frame: RemoteFrame) -> LlmFrame {
        let RemoteFrame::Llm(内) = frame else {
            panic!("应是 RemoteFrame::Llm");
        };
        *内
    }

    /// 装箱的**约束本身**：LLM 子协议再怎么长，都不许把外层信封撑大。
    ///
    /// 这条守的是一个没有任何可见症状的退化——去掉 `Box` 时全部测试照绿、线格式一字节不变，
    /// 只有每一个 `RemoteFrame`（含最高频的 `Output(Vec<u8>)`）悄悄按 `LlmFrame` 的尺寸搬运。
    /// clippy 的 `large_enum_variant` 在这里不会报（最大与次大变体之差不足阈值），故只能靠断言。
    #[test]
    fn llm_子协议不得撑大外层信封() {
        let 信封 = std::mem::size_of::<RemoteFrame>();
        let 内层 = std::mem::size_of::<LlmFrame>();
        assert!(
            信封 < 内层,
            "RemoteFrame({信封} B) 不应被 LlmFrame({内层} B) 驱动——`Llm` 变体的 `Box` 掉了？"
        );
    }

    /// 构造一个各字段都非默认值的对话元信息，供多个测试复用。
    fn 样本元信息() -> LlmConvMeta {
        LlmConvMeta {
            conv_id: 11,
            conv_generation: 3,
            agent: LlmAgentKind::Claude,
            cwd: LlmPath::new("F:\\secret-project"),
            title: LlmText::new("重构渲染器"),
            state: LlmConvState::Running,
            model: Some("claude-opus-5".into()),
            permission_mode: Some(LlmPermissionMode::AcceptEdits),
            cli_session_id: Some("7e0c-…".into()),
            origin: Some(PaneRef {
                tab_id: 2,
                session_id: 9,
            }),
            cur_turn: 4,
            created_ms: 1_000,
            updated_ms: 2_000,
            usage: LlmUsage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 40,
                cost_micro_usd: 1_234,
                context_used: 5_000,
                context_limit: 200_000,
                max_output_tokens: 64_000,
            },
        }
    }

    /// 前向兼容铁律 1：未知 `op`（带任意嵌套 body）降级成 `Unknown`，**不是** `Err`。
    ///
    /// 这条是「代价只付一次」整个论证的唯一客观依据——本仓库此前零处 `#[serde(other)]`，
    /// 该前提在落地前没有先例可对照。
    #[test]
    fn llm_未知op降级为unknown不炸整帧() {
        let v =
            serde_json::json!({"Llm": {"op": "VoiceInput", "conv_id": 3, "deep": {"a": [1, 2]}}});
        assert_eq!(
            RemoteFrame::from_value(&v).expect("未知 op 必须降级而非报错"),
            装帧(LlmFrame::Unknown)
        );
        // 兜底变体自身也能往返（虽然规定永不发送，但不能是「一序列化就炸」的半成品）。
        let unknown = 装帧(LlmFrame::Unknown);
        let back = RemoteFrame::from_value(&unknown.to_value().expect("to_value")).expect("往返");
        assert_eq!(back, unknown);
    }

    /// 前向兼容铁律 2：未知**增量项** / 未知**块类型**各自就地降级，同帧其余内容照常解析。
    #[test]
    fn llm_未知增量项与未知块类型降级不炸() {
        let v = serde_json::json!({"Llm": {"op": "Delta", "conv_id": 1, "conv_generation": 2,
            "seq": 3, "turn": 1, "items": [
                {"kind": "TextAppend", "block_id": 1, "text": "hi"},
                {"kind": "BlockStart", "entry": {"block_id": 2,
                    "block": {"kind": "Audio", "pcm": "AA", "rate": 48000}}},
                {"kind": "AudioAppend", "block_id": 9, "pcm": "AA"}]}});
        let LlmFrame::Delta { items, .. } =
            拆帧(RemoteFrame::from_value(&v).expect("整帧必须活下来"))
        else {
            panic!("应解析成 Delta");
        };
        assert_eq!(items.len(), 3, "未知项不得让同帧其余项消失");
        // 已知项照常解析。
        assert!(matches!(items[0], LlmDeltaItem::TextAppend { .. }));
        // 未知块类型：外层 BlockStart 还在，块本身降级成 Unknown（气泡流只少一块）。
        let LlmDeltaItem::BlockStart { entry } = &items[1] else {
            panic!("BlockStart 本身是已知项，不应降级");
        };
        assert_eq!(entry.block_id, 2);
        assert_eq!(entry.block, LlmBlock::Unknown);
        // 未知增量项整项降级。
        assert_eq!(items[2], LlmDeltaItem::Unknown);

        // 状态类同样兜底：未知工具状态 / 对话态 / 审批决定都不得炸整帧。
        let tool_result: LlmBlock = serde_json::from_value(serde_json::json!({
            "kind": "ToolResult", "call_id": "c1",
            "status": {"kind": "PartiallyOk"}, "output": "out"
        }))
        .expect("未知工具状态应降级");
        let LlmBlock::ToolResult { status, .. } = tool_result else {
            panic!("应解析成 ToolResult");
        };
        assert_eq!(status, LlmToolStatus::Unknown);

        let state: LlmConvState =
            serde_json::from_value(serde_json::json!({"kind": "Hibernating"})).expect("未知对话态");
        assert_eq!(state, LlmConvState::Unknown);

        let decision: LlmPermissionDecision =
            serde_json::from_value(serde_json::json!({"kind": "AllowOnce", "ttl": 30}))
                .expect("未知审批决定");
        assert_eq!(
            decision,
            LlmPermissionDecision::Unknown,
            "未知审批决定必须落 Unknown（被控端按拒绝处理）"
        );
    }

    /// 前向兼容铁律 3：开放式 C 型枚举保留未知原文（`other` 兜不住的那一层）。
    #[test]
    fn llm_未知c型枚举值保留原文() {
        let v = serde_json::json!({"Llm": {"op": "TurnEnded", "conv_id": 1, "conv_generation": 2,
            "seq": 3, "turn": 1, "stop": "BudgetExceeded", "usage": {}, "ended_ms": 0,
            "outcome": {"is_error": true, "terminal_reason": "quota_exhausted",
                        "api_error_status": 429}}});
        let LlmFrame::TurnEnded { stop, outcome, .. } =
            拆帧(RemoteFrame::from_value(&v).expect("未知枚举值不得炸整帧"))
        else {
            panic!("应解析成 TurnEnded");
        };
        assert_eq!(stop, LlmStopReason::Other("BudgetExceeded".into()));
        assert_eq!(stop.as_wire(), "BudgetExceeded");
        assert!(!stop.is_known());
        // 成败判定：只看 is_error（这里连 stop 都不认识，照样必须判成失败）。
        assert!(outcome.is_error);
        assert_eq!(
            outcome.terminal_reason,
            Some(LlmTerminalReason::Other("quota_exhausted".into()))
        );
        assert_eq!(outcome.api_error_status, Some(429));

        // 已知值照常收敛到 Known，且 as_wire 给出线上原文。
        let known: LlmStopReason = serde_json::from_value(serde_json::json!("EndTurn")).unwrap();
        assert_eq!(known, LlmStopReason::EndTurn);
        assert_eq!(known.as_wire(), "EndTurn");
        assert!(known.is_known());
        // 老端不带 outcome 时按「正常收尾」解释（加字段不破坏兼容）。
        let legacy = serde_json::json!({"Llm": {"op": "TurnEnded", "conv_id": 1,
            "conv_generation": 2, "seq": 3, "turn": 1, "stop": "EndTurn", "usage": {},
            "ended_ms": 0}});
        let LlmFrame::TurnEnded { outcome, .. } =
            拆帧(RemoteFrame::from_value(&legacy).expect("缺 outcome 的旧帧"))
        else {
            panic!("应解析成 TurnEnded");
        };
        assert_eq!(outcome, LlmTurnOutcome::default());
        assert!(!outcome.is_error);
    }

    /// 对齐 `v3_内置编辑帧经value往返且debug脱敏`：正文 / 路径 / **工具入参**绝不进 `Debug`。
    ///
    /// 工具入参那两条是设计初稿被对抗验证点名的漏洞（初稿只包了正文与路径，
    /// `ToolUse.input` / `PermissionRequest.input` 是裸 `serde_json::Value`），此处专门守着。
    #[test]
    fn llm_帧经value往返且debug脱敏() {
        // ① 正文 + 附件路径。
        let send = 装帧(LlmFrame::Send {
            conv_id: 1,
            conv_generation: 2,
            req_id: 5,
            text: LlmText::new("我的密码是 hunter2"),
            attachments: vec![LlmAttachment {
                path: LlmPath::new("C:/secret/a.png"),
                // 文件名同样是用户内容（这里刻意含 hunter2），必须一并脱敏。
                name: LlmText::new("我的身份证-hunter2.png"),
                mime: "image/png".into(),
                len: 9,
            }],
        });
        assert_eq!(
            RemoteFrame::from_value(&send.to_value().expect("to_value")).expect("from_value"),
            send
        );
        let dbg = format!("{send:?}");
        assert!(!dbg.contains("hunter2"), "正文/附件名泄漏进 Debug: {dbg}");
        assert!(!dbg.contains("secret"), "路径泄漏进 Debug: {dbg}");

        // ② PermissionRequest.input（初稿漏洞之一）。
        let perm = 装帧(LlmFrame::PermissionRequest {
            conv_id: 1,
            conv_generation: 2,
            request_id: 7,
            turn: 1,
            tool: "Bash".into(),
            input: LlmToolInput::new(serde_json::json!({"command": "export TOKEN=hunter2"})),
            preview: LlmText::new("将执行一条命令"),
            expires_in_secs: LLM_PERMISSION_TIMEOUT_SECS,
        });
        assert_eq!(
            RemoteFrame::from_value(&perm.to_value().expect("to_value")).expect("from_value"),
            perm
        );
        let dbg = format!("{perm:?}");
        assert!(!dbg.contains("hunter2"), "审批入参泄漏进 Debug: {dbg}");
        assert!(dbg.contains("fields"), "应只打字段数/字节数: {dbg}");

        // ③ ToolUse.input（初稿漏洞之二）——经 Delta 的嵌套路径也不得泄漏。
        let tool = 装帧(LlmFrame::Delta {
            conv_id: 1,
            conv_generation: 2,
            seq: 8,
            turn: 1,
            items: vec![LlmDeltaItem::BlockStart {
                entry: LlmBlockEntry {
                    block_id: 0,
                    parent_call_id: Some("call_parent".into()),
                    block: LlmBlock::ToolUse {
                        call_id: "call_1".into(),
                        name: "Edit".into(),
                        title: Some(LlmText::new("写入 .env")),
                        input: LlmToolInput::new(
                            serde_json::json!({"new_string": "API_KEY=hunter2"}),
                        ),
                        truncated_bytes: None,
                    },
                },
            }],
        });
        assert_eq!(
            RemoteFrame::from_value(&tool.to_value().expect("to_value")).expect("from_value"),
            tool
        );
        let dbg = format!("{tool:?}");
        assert!(!dbg.contains("hunter2"), "工具入参泄漏进 Debug: {dbg}");
        assert!(!dbg.contains("写入 .env"), "工具摘要泄漏进 Debug: {dbg}");

        // ④ 错误块的正文同样脱敏（认证失败正文里可能带账号信息）。
        let err = 装帧(LlmFrame::ConvFailed {
            req_id: 3,
            code: LlmErrorCode::AuthRequired,
            message: LlmText::new("Failed to authenticate. hunter2"),
        });
        assert!(
            !format!("{err:?}").contains("hunter2"),
            "错误正文泄漏进 Debug"
        );
    }

    /// 满载往返：**每个** `LlmFrame` 变体都经 `to_value` / `from_value` 恒等。
    ///
    /// `变体名` 那个穷尽 `match` 是刻意的——将来给 [`LlmFrame`] 加变体时会**编译失败**，
    /// 强制补样本，避免新变体长期没有往返覆盖（本仓库 `RemoteFrame` 的往返测试就是靠人肉
    /// 记得补，已有变体漏测的先例）。
    #[test]
    fn llm_全变体满载往返() {
        fn 变体名(frame: &LlmFrame) -> &'static str {
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

        let meta = 样本元信息();
        let agent = LlmAgentInfo {
            agent: LlmAgentKind::Claude,
            available: true,
            version: Some("2.1.226".into()),
            models: vec!["claude-opus-5".into()],
            permission_modes: vec![
                LlmPermissionMode::Manual,
                LlmPermissionMode::Other("YoloMode".into()),
            ],
            features: LlmAgentFeatures {
                streaming: true,
                thinking: true,
                // 实测 Claude 拿不到思考正文，这里刻意与 `thinking` 取不同值：两位若总是同真同假，
                // 就没人会发现自己把它们当成了一位（那正是「画了折叠箭头却永远展不开」的来源）。
                thinking_text: false,
                interactive_permission: false,
                resume: true,
                attachments: true,
                interrupt: true,
            },
        };
        let entry = LlmBlockEntry {
            block_id: 0,
            parent_call_id: Some("call_sub".into()),
            block: LlmBlock::Text {
                text: LlmText::new("你好"),
            },
        };
        // `user` 用 0,1、`assistant` 接着用 2..5 是**刻意连号**，不是随手写的样本：
        // 同一轮内两个数组共用同一编号空间是硬不变量（见 [`BlockId`] 文档）——分别从 0 编号会让
        // `TextAppend` / `BlockEnd` / `Dropped` 的 `block_id` 产生歧义、静默覆盖用户气泡。
        let record = LlmTurnRecord {
            turn: 4,
            user: vec![
                entry.clone(),
                LlmBlockEntry {
                    block_id: 1,
                    parent_call_id: None,
                    block: LlmBlock::Image {
                        attachment: LlmAttachment {
                            path: LlmPath::new("C:/attach/1.png"),
                            name: LlmText::new("1.png"),
                            mime: "image/png".into(),
                            len: 1024,
                        },
                    },
                },
            ],
            assistant: vec![
                LlmBlockEntry {
                    block_id: 2,
                    parent_call_id: None,
                    // 满载样本取 Redacted 而不是 Text：Omitted / Redacted 这两个 unit 分支若不
                    // 进往返样本，「思考块可以没有正文」这件事在线格式上就没人守。
                    block: LlmBlock::Thinking {
                        content: LlmThinking::Redacted,
                    },
                },
                LlmBlockEntry {
                    block_id: 3,
                    parent_call_id: None,
                    block: LlmBlock::ToolUse {
                        call_id: "c1".into(),
                        name: "Bash".into(),
                        title: Some(LlmText::new("列目录")),
                        input: LlmToolInput::new(serde_json::json!({"command": "ls"})),
                        // 非 None：与 ToolResult.truncated_bytes 对称的「入参被裁」声明，
                        // 满载样本必须覆盖到，否则加了字段却没人守它的线格式。
                        truncated_bytes: Some(5_242_880),
                    },
                },
                LlmBlockEntry {
                    block_id: 4,
                    parent_call_id: Some("c1".into()),
                    block: LlmBlock::ToolResult {
                        call_id: "c1".into(),
                        status: LlmToolStatus::Denied,
                        output: LlmText::new("out"),
                        truncated_bytes: Some(12),
                        // 结构化摘要必须进满载样本：它是「不推正文也能渲染卡片」的载体，
                        // 路径又是脱敏类型，漏测就等于没人守 LlmPath 在这一层的 Debug 兜底。
                        detail: Some(LlmToolResultDetail::File {
                            path: LlmPath::new("F:\\secret-project\\README.md"),
                            start_line: 1,
                            line_count: 165,
                            total_lines: 165,
                        }),
                    },
                },
                LlmBlockEntry {
                    block_id: 5,
                    parent_call_id: None,
                    block: LlmBlock::Error {
                        code: LlmErrorCode::Other("authentication_failed".into()),
                        message: LlmText::new("API Error: 403"),
                    },
                },
            ],
            stop: Some(LlmStopReason::Error),
            outcome: LlmTurnOutcome {
                is_error: true,
                terminal_reason: Some(LlmTerminalReason::ApiError),
                api_error_status: Some(403),
            },
            usage: meta.usage,
            started_ms: 1,
            ended_ms: Some(2),
            complete: false,
            blocks_omitted: 3,
        };

        let frames = vec![
            LlmFrame::Hello {
                llm_proto: LLM_PROTO_VERSION,
                caps: vec!["perm".into(), "attach".into()],
            },
            LlmFrame::HelloAck {
                llm_proto: LLM_PROTO_VERSION,
                caps: vec!["thinking".into()],
                agents: vec![agent.clone()],
                convs: vec![meta.clone()],
                // 额度基线必须进满载样本：它是 RateLimit 播报帧唯一的「挂载即可见」通道，
                // 漏测等于没人守「基线帧必先于增量」在这条新通路上的落实。
                rate_limit: Some(LlmRateLimit {
                    status: LlmRateLimitStatus::Allowed,
                    window: Some(LlmRateLimitWindow::FiveHour),
                    resets_at_secs: Some(1_786_211_400),
                    overage_status: Some(LlmOverageStatus::Rejected),
                    overage_disabled_reason: Some(LlmOverageDisabledReason::OutOfCredits),
                    using_overage: Some(false),
                }),
                // 基线的观测时刻与 RateLimit 播报帧同一时钟域（都是 PC 墙钟）——没有它，
                // 控制端对基线与播报无法做 latest-wins（详见该字段文档）。
                rate_limit_observed_ms: Some(1_786_211_000_000),
            },
            LlmFrame::ListAgents { req_id: 1 },
            LlmFrame::AgentList {
                req_id: 1,
                agents: vec![agent],
            },
            LlmFrame::ListConvs { req_id: 2 },
            LlmFrame::ConvList {
                req_id: 2,
                convs: vec![meta.clone()],
            },
            LlmFrame::Start {
                req_id: 3,
                agent: LlmAgentKind::Other("Qwen".into()),
                cwd: LlmPath::new("F:/repo"),
                model: Some("m".into()),
                permission_mode: Some(LlmPermissionMode::Plan),
                resume: Some("sess-1".into()),
                fork_session: true,
                append_system_prompt: Some(LlmText::new("你是")),
                allowed_tools: Some(vec!["Read".into(), "Bash".into()]),
                origin: Some(PaneRef {
                    tab_id: 1,
                    session_id: 2,
                }),
            },
            LlmFrame::ConvStarted {
                req_id: 3,
                meta: meta.clone(),
                attach_dir: LlmPath::new("F:/attach"),
                seq: 100,
            },
            LlmFrame::ConvFailed {
                req_id: 3,
                code: LlmErrorCode::AgentNotFound,
                message: LlmText::new("找不到 claude"),
            },
            LlmFrame::Send {
                conv_id: 11,
                conv_generation: 3,
                req_id: 4,
                text: LlmText::new("跑一下测试"),
                attachments: vec![LlmAttachment {
                    path: LlmPath::new("C:/attach/2.png"),
                    name: LlmText::new("2.png"),
                    mime: "image/png".into(),
                    len: 7,
                }],
            },
            LlmFrame::Interrupt {
                conv_id: 11,
                conv_generation: 3,
                req_id: 5,
            },
            LlmFrame::Close {
                conv_id: 11,
                conv_generation: 3,
                req_id: 6,
            },
            LlmFrame::ConvExited {
                conv_id: 11,
                conv_generation: 3,
                seq: 200,
                reason: LlmExitReason::ProcessExited,
                code: Some(1),
                message: Some(LlmText::new("exit 1")),
            },
            LlmFrame::Attach {
                conv_id: 11,
                known_generation: 3,
                known_seq: 99,
                known_turn: 4,
            },
            LlmFrame::Attached {
                meta: meta.clone(),
                seq: 101,
                resync_required: true,
            },
            LlmFrame::Detach {
                conv_id: 11,
                conv_generation: 3,
            },
            LlmFrame::Delta {
                conv_id: 11,
                conv_generation: 3,
                seq: 102,
                turn: 4,
                items: vec![
                    LlmDeltaItem::BlockStart {
                        entry: entry.clone(),
                    },
                    LlmDeltaItem::TextAppend {
                        block_id: 0,
                        text: LlmText::new("增量"),
                    },
                    LlmDeltaItem::BlockEnd {
                        block_id: 0,
                        block: Some(LlmBlock::Text {
                            text: LlmText::new("终态"),
                        }),
                    },
                    LlmDeltaItem::BlockEnd {
                        block_id: 1,
                        block: None,
                    },
                    LlmDeltaItem::Dropped {
                        block_id: 0,
                        bytes: 4096,
                    },
                    LlmDeltaItem::Unknown,
                ],
            },
            LlmFrame::DeltaAck {
                conv_id: 11,
                conv_generation: 3,
                seq: 102,
            },
            LlmFrame::TurnStarted {
                conv_id: 11,
                conv_generation: 3,
                seq: 103,
                turn: 5,
                user: vec![entry],
                started_ms: 10,
            },
            LlmFrame::TurnEnded {
                conv_id: 11,
                conv_generation: 3,
                seq: 104,
                turn: 5,
                stop: LlmStopReason::MaxTokens,
                outcome: LlmTurnOutcome {
                    is_error: true,
                    terminal_reason: Some(LlmTerminalReason::ApiError),
                    api_error_status: Some(403),
                },
                usage: meta.usage,
                ended_ms: 20,
                truncated: true,
            },
            LlmFrame::RateLimit {
                conv_id: 11,
                conv_generation: 3,
                observed_ms: 1_786_211_000_000,
                info: LlmRateLimit {
                    status: LlmRateLimitStatus::Allowed,
                    window: Some(LlmRateLimitWindow::FiveHour),
                    resets_at_secs: Some(1_786_211_400),
                    overage_status: Some(LlmOverageStatus::Rejected),
                    overage_disabled_reason: Some(LlmOverageDisabledReason::OutOfCredits),
                    using_overage: Some(false),
                },
            },
            LlmFrame::TurnFetch {
                conv_id: 11,
                conv_generation: 3,
                req_id: 7,
                turn: 4,
            },
            LlmFrame::TurnSnapshot {
                conv_id: 11,
                conv_generation: 3,
                req_id: 7,
                seq: 105,
                record: record.clone(),
            },
            LlmFrame::HistoryReq {
                conv_id: 11,
                conv_generation: 3,
                req_id: 8,
                before_turn: 5,
                max_turns: LLM_HISTORY_MAX_TURNS_PER_PAGE,
            },
            LlmFrame::HistoryPage {
                conv_id: 11,
                conv_generation: 3,
                req_id: 8,
                oldest_turn: 1,
                turns: vec![record],
                has_more: true,
            },
            LlmFrame::PermissionRequest {
                conv_id: 11,
                conv_generation: 3,
                request_id: 42,
                turn: 5,
                tool: "Bash".into(),
                input: LlmToolInput::new(serde_json::json!({"command": "rm -rf ."})),
                preview: LlmText::new("将删除目录"),
                expires_in_secs: LLM_PERMISSION_TIMEOUT_SECS,
            },
            LlmFrame::PermissionReply {
                conv_id: 11,
                conv_generation: 3,
                request_id: 42,
                decision: LlmPermissionDecision::Allow { remember: true },
            },
            LlmFrame::PermissionResolved {
                conv_id: 11,
                conv_generation: 3,
                request_id: 42,
                decision: LlmPermissionDecision::Deny {
                    reason: Some(LlmText::new("超时")),
                },
                by: LlmPermissionResolvedBy::Timeout,
            },
            LlmFrame::Error {
                conv_id: Some(11),
                req_id: Some(9),
                code: LlmErrorCode::StaleSession,
                message: LlmText::new("代不匹配"),
            },
            LlmFrame::Unknown,
        ];

        let mut 覆盖 = std::collections::BTreeSet::new();
        for frame in frames {
            覆盖.insert(变体名(&frame));
            // 模拟服务端盲转：RemoteFrame → Value → Relay 原样取出 → 还原。
            let wrapped = 装帧(frame.clone());
            let value = wrapped.to_value().expect("to_value");
            let back = RemoteFrame::from_value(&value).expect("from_value");
            assert_eq!(back, wrapped, "变体 {} 往返不恒等", 变体名(&frame));
        }
        assert_eq!(
            覆盖.len(),
            30,
            "LlmFrame 变体数与样本数不一致，新增变体必须补样本"
        );
    }

    /// `RemoteFrame` 这一层的语义**不得**被本次改动改变：`Llm` 能往返，而未知**顶层**变体名
    /// 仍然整帧 `Err`。
    ///
    /// 这条是刻意的：`RemoteFrame` 是 externally tagged，serde 不支持在其上加 `#[serde(other)]`；
    /// 就算支持也不该加——顶层兜底会把「对端太老」伪装成「收到一个空帧」，让
    /// `remote_ws.rs` 那条「解析失败即丢弃」的告警彻底失去证据价值。前向兼容的兜底**只放在
    /// [`LlmFrame`] 这一层**。
    #[test]
    fn llm_顶层变体往返且未知顶层变体仍整帧报错() {
        let frame = 装帧(LlmFrame::Hello {
            llm_proto: LLM_PROTO_VERSION,
            caps: Vec::new(),
        });
        let value = frame.to_value().expect("to_value");
        // 线格式确认：外部标签 `Llm` 包着内部标签 `op`，服务端仍只见一个不透明 Value。
        assert_eq!(value["Llm"]["op"], "Hello");
        assert_eq!(RemoteFrame::from_value(&value).expect("from_value"), frame);

        // 未知顶层变体：必须仍是 Err（老端行为不变，调用方 warn 后丢弃）。
        let 未来帧 = serde_json::json!({"HoloProjection": {"tab_id": 1}});
        assert!(
            RemoteFrame::from_value(&未来帧).is_err(),
            "顶层枚举不得被加上 other 兜底：那会把「对端太老」伪装成一个可解析的空帧"
        );
        // 连 unit 变体形态的未知顶层名也一样。
        let 未来unit = serde_json::json!("HoloStreamHello");
        assert!(
            RemoteFrame::from_value(&未来unit).is_err(),
            "未知 unit 变体名同样整帧 Err"
        );
    }

    /// 对齐 `part3d_订阅快照满载单帧不超上限`：聚合型变体必须有满载断言。
    ///
    /// 同时给出 [`LLM_HISTORY_PAGE_MAX_BYTES`] 必须**先于**轮数生效的实证——满载 20 轮
    /// 已远超 1 MiB 字节预算。
    #[test]
    fn llm_历史分页满载单帧不超上限() {
        let record = LlmTurnRecord {
            turn: 1,
            user: vec![LlmBlockEntry {
                block_id: 0,
                parent_call_id: None,
                block: LlmBlock::Text {
                    text: LlmText::new("x".repeat(2_000)),
                },
            }],
            assistant: vec![
                LlmBlockEntry {
                    block_id: 1,
                    parent_call_id: None,
                    block: LlmBlock::Text {
                        text: LlmText::new("y".repeat(20_000)),
                    },
                },
                LlmBlockEntry {
                    block_id: 2,
                    parent_call_id: None,
                    block: LlmBlock::ToolResult {
                        call_id: "c".into(),
                        status: LlmToolStatus::Ok,
                        output: LlmText::new("z".repeat(LLM_TOOL_OUTPUT_MAX_BYTES)),
                        truncated_bytes: Some(1),
                        detail: None,
                    },
                },
            ],
            stop: Some(LlmStopReason::EndTurn),
            outcome: LlmTurnOutcome::default(),
            usage: LlmUsage::default(),
            started_ms: 0,
            ended_ms: Some(1),
            complete: true,
            blocks_omitted: 0,
        };
        let frame = 装帧(LlmFrame::HistoryPage {
            conv_id: 1,
            conv_generation: 2,
            req_id: 3,
            oldest_turn: 1,
            turns: vec![record; LLM_HISTORY_MAX_TURNS_PER_PAGE as usize],
            has_more: true,
        });
        let bytes = serde_json::to_string(&frame.to_value().expect("to_value"))
            .expect("序列化")
            .len();
        assert!(
            bytes < 4 * 1024 * 1024,
            "历史页顶穿 4 MiB 单帧上限: {bytes}"
        );
        assert!(
            bytes > LLM_HISTORY_PAGE_MAX_BYTES,
            "满载 20 轮应超字节预算（{bytes} B），否则「字节预算先于轮数」这条约束就是空话"
        );
    }

    /// 自愈通道也必须有字节预算：**128 块的一轮就顶穿中继 4 MiB 单帧上限**。
    ///
    /// 这条把实测数字钉死。`TurnSnapshot` 是 seq 断裂 / `truncated` 的**唯一**补齐通道，它自己
    /// 超限 → 中继断整条 WS / QUIC 静默丢帧 → 缺口永久补不上、手机上那段气泡永远空白。
    /// 故 [`LLM_TURN_SNAPSHOT_MAX_BYTES`] 与 [`LlmTurnRecord::blocks_omitted`] 必须成对存在：
    /// 前者定「删到多少」，后者让删这件事**可观测**。
    #[test]
    fn llm_单轮快照满载单帧不超上限() {
        /// 造 `blocks` 个 [`LlmBlock::ToolResult`]，每个 `output` 都**已经**按
        /// [`LLM_TOOL_OUTPUT_MAX_BYTES`] 夹过（即前提给足：单块已是合规的最坏情况）。
        ///
        /// 只放 `ToolResult` 不放配对的 `ToolUse`：后者每块才百余字节，量级上不影响结论，
        /// 混进来只会让「128 块 = 4.01 MiB」这个可复算的数字变得说不清。
        fn 满载快照(blocks: u32) -> LlmTurnRecord {
            let assistant = (0..blocks)
                .map(|i| LlmBlockEntry {
                    block_id: i,
                    parent_call_id: None,
                    block: LlmBlock::ToolResult {
                        call_id: format!("c{i}"),
                        status: LlmToolStatus::Ok,
                        output: LlmText::new("z".repeat(LLM_TOOL_OUTPUT_MAX_BYTES)),
                        truncated_bytes: Some(1),
                        detail: None,
                    },
                })
                .collect();
            LlmTurnRecord {
                turn: 1,
                user: Vec::new(),
                assistant,
                stop: Some(LlmStopReason::EndTurn),
                outcome: LlmTurnOutcome::default(),
                usage: LlmUsage::default(),
                started_ms: 0,
                ended_ms: Some(1),
                complete: true,
                blocks_omitted: 0,
            }
        }

        let 帧字节 = |record: LlmTurnRecord| -> usize {
            let frame = 装帧(LlmFrame::TurnSnapshot {
                conv_id: 1,
                conv_generation: 2,
                req_id: 3,
                seq: 4,
                record,
            });
            serde_json::to_string(&frame.to_value().expect("to_value"))
                .expect("序列化")
                .len()
        };

        // ① 128 块就已顶穿 4 MiB（本测试实测 4 209 693 B = 4.01 MiB）——一轮 128 次工具调用对
        //    agentic 会话是常态不是极端。这是本测试存在的全部理由。
        let 顶穿 = 帧字节(满载快照(128));
        assert!(
            顶穿 > 4 * 1024 * 1024,
            "128 块应顶穿中继 4 MiB 单帧上限（本测试实测 4 209 693 B），实得 {顶穿} B；\
             若上游把 LLM_TOOL_OUTPUT_MAX_BYTES 调小了，本断言的实证前提就变了，请重算而不是删断言"
        );

        // ② 字节预算必须**先于**块数生效：远不到 128 块就已超预算，故被控端只能按字节删。
        let 超预算 = 帧字节(满载快照(40));
        assert!(
            超预算 > LLM_TURN_SNAPSHOT_MAX_BYTES,
            "40 块（{超预算} B）应已超 {LLM_TURN_SNAPSHOT_MAX_BYTES} B 预算，\
             否则「字节预算先于块数」这条约束就是空话"
        );

        // ③ 按预算删减后必须安全落地，且删减是**可观测**的（blocks_omitted 非 0）。
        let mut 删减后 = 满载快照(24);
        删减后.blocks_omitted = 104;
        let 删减后字节 = 帧字节(删减后.clone());
        assert!(
            删减后字节 <= LLM_TURN_SNAPSHOT_MAX_BYTES,
            "删到 16 块应落在预算内，实得 {删减后字节} B"
        );
        assert_ne!(
            删减后.blocks_omitted, 0,
            "删了块却不声明就是静默降级——正是 LlmDeltaItem::Dropped 存在的同一条理由"
        );
        // 老端不带本字段时按「一块没删」解释（加字段零兼容成本）。
        let mut 老端 = serde_json::to_value(&删减后).expect("to_value");
        老端.as_object_mut().expect("obj").remove("blocks_omitted");
        let 回来: LlmTurnRecord = serde_json::from_value(老端).expect("缺 blocks_omitted 的旧记录");
        assert_eq!(回来.blocks_omitted, 0);
    }

    /// [`LLM_ENUM_WIRE_MAX_BYTES`] 必须是**可执行**的防线，不是装饰常量。
    ///
    /// 两条各守一头：`from_wire_clamped` 守发送侧（原文进不来），手写 `Debug` 守日志侧
    /// （收进来的超长 `Other` 也写不进审计日志）。缺任何一条，模块文档 :91 那句
    /// 「`Debug` 脱敏正是为这条兜底」就是空头承诺。
    #[test]
    fn llm_开放枚举夹紧与debug脱敏() {
        // ① 发送侧：4 KiB 的 hook 输出经 from_wire_clamped 后线上原文不超上限。
        let 长原文 = format!("SECRET-hook-output-{}", "A".repeat(4096));
        let clamped = LlmStopReason::from_wire_clamped(&长原文);
        assert!(!clamped.is_known());
        assert!(
            clamped.as_wire().len() <= LLM_ENUM_WIRE_MAX_BYTES,
            "夹紧失效：{} B",
            clamped.as_wire().len()
        );

        // ② 已知值仍收敛成 Known（夹紧不能顺手把认识的值也打成 Other）。
        assert_eq!(
            LlmStopReason::from_wire_clamped("EndTurn"),
            LlmStopReason::EndTurn
        );
        // as_wire 走 serde 而非 Debug：这条把「双事实源」钉死。
        assert_eq!(
            serde_json::to_value(LlmStopReasonKnown::EndTurn).expect("to_value"),
            serde_json::json!(LlmStopReason::EndTurn.as_wire().as_ref()),
            "as_wire 必须与 serde 线格式一致（不得依赖 Debug 名）"
        );

        // ③ UTF-8 字符边界：中文标识按字节切会切出非法 UTF-8，from_wire_clamped 必须不 panic。
        let 中文 = "错误-".repeat(64); // 每个「错误-」7 字节
        let c = LlmErrorCode::from_wire_clamped(&中文);
        assert!(c.as_wire().len() <= LLM_ENUM_WIRE_MAX_BYTES);
        assert!(中文.starts_with(c.as_wire().as_ref()), "必须是前缀截断");

        // ④ 日志侧：**反序列化进来的**超长 Other（不受发送侧夹紧约束）也不得原样进 Debug。
        let 越界: LlmStopReason =
            serde_json::from_value(serde_json::json!(长原文.clone())).expect("超长值不得整帧报错");
        assert_eq!(越界, LlmStopReason::Other(长原文.clone()));
        let dbg = format!("{越界:?}");
        assert!(
            !dbg.contains(&长原文),
            "Other 原文原样进 Debug，审计日志会把 hook 输出写进磁盘: {dbg}"
        );
        assert!(dbg.len() < 64, "Debug 应只打头部 + 总长，实得: {dbg}");
        assert!(
            dbg.contains(&format!("{}B", 长原文.len())),
            "Debug 必须带总长，否则看不出被截了多少: {dbg}"
        );
    }

    /// 铁律三：思考在协议上是**状态**，不是内容。
    ///
    /// 样本 B 实测 `thinking` 字段恒为空串（只有 `signature` 有值），且 CLI 无开关可打开。
    /// 本测试守住三件事：`Omitted` / `Redacted` 在**线格式上就没有文本字段**（UI 想依赖也依赖
    /// 不上）、`Text` 分支仍在（别家 CLI 与将来的 summarized 模式零协议改动）、未来形态就地降级。
    #[test]
    fn llm_思考块只承载状态不承载文本() {
        // ① 两个 unit 分支：线上只有 kind，没有任何可绑定的文本。
        for (content, 期望) in [
            (LlmThinking::Omitted, "Omitted"),
            (LlmThinking::Redacted, "Redacted"),
        ] {
            let block = LlmBlock::Thinking { content };
            let v = serde_json::to_value(&block).expect("to_value");
            assert_eq!(v["content"]["kind"], 期望);
            assert!(
                v["content"].get("text").is_none(),
                "{期望} 分支不得有文本字段——「永远展不开的折叠块」正是从这里长出来的"
            );
            assert_eq!(
                serde_json::from_value::<LlmBlock>(v).expect("往返"),
                block,
                "{期望} 往返不恒等"
            );
        }

        // ② Text 分支必须保留：Claude 拿不到正文 ≠ 别家 CLI 拿不到。删了它，日后要么表达不了
        //    别家的推理摘要，要么得再改一次形状（C 型枚举那条教训）。
        let 有文本 = LlmBlock::Thinking {
            content: LlmThinking::Text {
                text: LlmText::new("先读 README"),
            },
        };
        let v = serde_json::to_value(&有文本).expect("to_value");
        assert_eq!(v["content"]["text"], "先读 README");
        assert_eq!(serde_json::from_value::<LlmBlock>(v).expect("往返"), 有文本);
        // 思考正文同样是用户内容（会复述 prompt），脱敏不能漏。
        assert!(
            !format!("{有文本:?}").contains("先读 README"),
            "思考正文泄漏进 Debug"
        );

        // ③ 将来 CLI 真给了 summarized 形态时，老控制端只降级这一块。
        let 未来: LlmBlock = serde_json::from_value(serde_json::json!({
            "kind": "Thinking",
            "content": {"kind": "Summarized", "text": "…", "tokens": 42}
        }))
        .expect("未知思考形态必须降级而非报错");
        assert_eq!(
            未来,
            LlmBlock::Thinking {
                content: LlmThinking::Unknown
            }
        );

        // ④ 能力位是两位不是一位：出思考块 ≠ 出得了思考正文（Claude 实测就是前真后假）。
        let 能力 = LlmAgentFeatures {
            thinking: true,
            thinking_text: false,
            ..LlmAgentFeatures::default()
        };
        assert!(能力.thinking && !能力.thinking_text);
        assert_eq!(
            serde_json::to_value(能力).expect("to_value")["thinking_text"],
            false
        );
    }

    /// 结构化工具结果：**只带元数据、不带正文**，未知形态就地降级。
    #[test]
    fn llm_工具结果摘要只带元数据且未知形态降级() {
        let block = LlmBlock::ToolResult {
            call_id: "toolu_019Vk9BtrSBpc2dtv9YBPTY3".into(),
            status: LlmToolStatus::Ok,
            // 「只发摘要」策略：正文一个字节都不推，全部记进 truncated_bytes（可观测降级）。
            output: LlmText::default(),
            truncated_bytes: Some(12_272),
            detail: Some(LlmToolResultDetail::File {
                // 合成路径，不是采样里那条真实路径——绝对路径本身就是本模块要挡的环境信息
                // （用户名、项目名、内网盘符都在里面），测试源码里同样不该留一份。
                path: LlmPath::new("F:\\secret-project\\README.md"),
                start_line: 1,
                line_count: 165,
                total_lines: 165,
            }),
        };
        let v = serde_json::to_value(&block).expect("to_value");
        assert_eq!(v["detail"]["kind"], "File");
        assert!(
            v["detail"].get("content").is_none(),
            "摘要里不得出现正文字段——不推 11 720 字符文件正文正是本类型存在的全部理由"
        );
        assert_eq!(serde_json::from_value::<LlmBlock>(v).expect("往返"), block);
        // 摘要给全模块新增了一个 LlmPath，Debug 兜底必须跟着覆盖到这一层。
        assert!(
            !format!("{block:?}").contains("secret-project"),
            "摘要里的路径泄漏进 Debug"
        );
        // 整块上行成本：12 KiB 正文 → 不到 256 B。这就是「元数据代替正文」的量化依据。
        let 线上 = serde_json::to_string(&block).expect("序列化");
        assert!(
            线上.len() < 256,
            "摘要块应远小于正文，实得 {} B",
            线上.len()
        );

        // 老端不带 detail → None（加字段零兼容成本）。
        let 老: LlmBlock = serde_json::from_value(serde_json::json!({
            "kind": "ToolResult", "call_id": "c", "status": {"kind": "Ok"}, "output": "out"
        }))
        .expect("缺 detail 的旧块必须能解析");
        let LlmBlock::ToolResult {
            detail,
            truncated_bytes,
            ..
        } = 老
        else {
            panic!("应解析成 ToolResult");
        };
        assert_eq!(detail, None);
        assert_eq!(truncated_bytes, None);

        // 未来形态（`Bash` / `Edit` 的 tool_use_result 形状**未采样**，日后补变体）降级成
        // Unknown，同块其余字段照常——卡片退回纯文本样式，不是整块消失。
        let 未来: LlmBlock = serde_json::from_value(serde_json::json!({
            "kind": "ToolResult", "call_id": "c", "status": {"kind": "Ok"}, "output": "out",
            "detail": {"kind": "Diff", "added": 3, "removed": 1}
        }))
        .expect("未知摘要形态必须降级而非报错");
        let LlmBlock::ToolResult { detail, output, .. } = 未来 else {
            panic!("应解析成 ToolResult");
        };
        assert_eq!(detail, Some(LlmToolResultDetail::Unknown));
        assert_eq!(output.as_str(), "out", "未知摘要不得连累同块其余字段");
    }

    /// 限流帧：**不占 [`LlmFrame::Delta`] 的 seq 空间**，全部可选字段缺省即「上游没说」。
    #[test]
    fn llm_限流帧不占seq且缺字段解释成未知() {
        let 帧 = 装帧(LlmFrame::RateLimit {
            conv_id: 11,
            conv_generation: 3,
            observed_ms: 1_786_211_000_000,
            info: LlmRateLimit {
                status: LlmRateLimitStatus::Allowed,
                window: Some(LlmRateLimitWindow::FiveHour),
                resets_at_secs: Some(1_786_211_400),
                overage_status: Some(LlmOverageStatus::Rejected),
                overage_disabled_reason: Some(LlmOverageDisabledReason::OutOfCredits),
                using_overage: Some(false),
            },
        });
        let v = 帧.to_value().expect("to_value");
        assert_eq!(RemoteFrame::from_value(&v).expect("from_value"), 帧);
        // 这条断言守的是一条会造成刷屏的隐性 bug：本帧一旦领 seq，控制端每收到一次限流播报就
        // 会看见 Delta 序列「断裂」并触发一次 TurnFetch 整轮拉取。
        assert!(
            v["Llm"].get("seq").is_none(),
            "限流帧不得占用 Delta 的 seq 空间: {v}"
        );

        // 单位守卫：resetsAt 是**秒**，本模块其余时间戳是**毫秒**，两者量级差 1000 倍。
        let 内层 = 拆帧(帧.clone());
        let LlmFrame::RateLimit {
            observed_ms, info, ..
        } = &内层
        else {
            panic!("应是 RateLimit");
        };
        assert_eq!(info.resets_at_secs, Some(1_786_211_400));
        // 换算一律走 checked_mul：本字段对端可控，裸 `* 1_000` 在 debug 下 overflow panic、
        // release 下回绕成负数。这段是实现者会照抄的范本，范本本身必须是安全写法。
        assert!(
            info.resets_at_secs
                .and_then(|秒| 秒.checked_mul(1_000))
                .is_some_and(|毫秒| 毫秒 > *observed_ms),
            "换算成毫秒后应晚于观测时刻；若这条反了，多半是把秒当毫秒用了（手机上会显示 1970 年）"
        );
        // 对端可控值真的会溢出：`i64::MAX` 秒换算成毫秒必须得到 None，而不是 panic / 负数。
        assert_eq!(i64::MAX.checked_mul(1_000), None);

        // 基线与播报**必须可比较**：`HelloAck` 的额度基线配了同一时钟域的 `rate_limit_observed_ms`，
        // 控制端拿它和本帧的 `observed_ms` 直接比大小做 latest-wins。缺了它，一条迟到的旧播报会
        // 无条件覆盖更新的基线，而额度事件间隔可达数小时。
        let 基线 = 装帧(LlmFrame::HelloAck {
            llm_proto: LLM_PROTO_VERSION,
            caps: Vec::new(),
            agents: Vec::new(),
            convs: Vec::new(),
            rate_limit: Some(info.clone()),
            rate_limit_observed_ms: Some(*observed_ms - 1),
        });
        let LlmFrame::HelloAck {
            rate_limit,
            rate_limit_observed_ms,
            ..
        } = 拆帧(RemoteFrame::from_value(&基线.to_value().expect("to_value")).expect("往返"))
        else {
            panic!("应是 HelloAck");
        };
        assert_eq!(rate_limit.as_ref(), Some(info));
        assert!(
            rate_limit_observed_ms.expect("基线必须带观测时刻") < *observed_ms,
            "基线比播报旧 ⇒ 播报应胜出；这条比较能做，正是因为两者同一时钟域"
        );

        // 只有 status 是必填，其余键缺省 → 一律 `None` = **上游没说**，不得解释成「否」。
        // `using_overage` 也是 `Option<bool>`：`None`（没给键）与 `Some(false)`（实测没在超额）
        // 是两件事，压成一个裸 bool 就再也分不开了。
        let 极简: LlmRateLimit =
            serde_json::from_value(serde_json::json!({"status": "Allowed"})).expect("缺字段");
        assert_eq!(极简.status, LlmRateLimitStatus::Allowed);
        assert_eq!(极简.window, None);
        assert_eq!(极简.resets_at_secs, None);
        assert_eq!(极简.overage_status, None);
        assert_eq!(极简.overage_disabled_reason, None);
        assert_eq!(极简.using_overage, None);
        // 反向：上游明确给了 false 时才是 Some(false)，与上面那条 None 可区分。
        let 明确: LlmRateLimit = serde_json::from_value(
            serde_json::json!({"status": "Allowed", "using_overage": false}),
        )
        .expect("显式 false");
        assert_eq!(明确.using_overage, Some(false));

        // 未知取值保原文（`"rejected"` / `"warning"` 之类都没采样到，只能靠这条兜住）。
        let 未知: LlmRateLimit = serde_json::from_value(serde_json::json!({
            "status": "Throttled", "window": "OneWeek", "overage_status": "Pending"
        }))
        .expect("未知值不得炸整个结构");
        assert_eq!(未知.status, LlmRateLimitStatus::Other("Throttled".into()));
        assert_eq!(
            未知.window,
            Some(LlmRateLimitWindow::Other("OneWeek".into()))
        );
        assert_eq!(
            未知.overage_status,
            Some(LlmOverageStatus::Other("Pending".into()))
        );
        assert!(!未知.status.is_known());
    }

    /// `modelUsage` 是**按模型名 key 的 map** 且字段全是 camelCase——必须**提取**成定长结构，
    /// 不许原样透传（模块文档铁律一）。同时钉死「真实上下文占用」的算法。
    #[test]
    fn llm_模型用量map提取成定长结构() {
        // 字段名逐个取自样本 B 实测；**数值是构造的**——原行被 PowerShell 重定向的编码损坏，
        // 只抠出了字段名与结构。第二个 key 取一个显然合成的名字，避免被误读成实测模型。
        let model_usage = serde_json::json!({
            "claude-opus-5": {
                "inputTokens": 3, "outputTokens": 24, "cacheReadInputTokens": 53883,
                "cacheCreationInputTokens": 18013, "webSearchRequests": 0, "costUSD": 0.2402615,
                "contextWindow": 1000000, "maxOutputTokens": 64000,
                "canonicalModel": "claude-opus-5", "provider": "anthropic"
            },
            "another-model": {
                "inputTokens": 10, "outputTokens": 5, "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 0, "webSearchRequests": 0, "costUSD": 0.0001,
                "contextWindow": 200000, "maxOutputTokens": 8192,
                "canonicalModel": "another-model", "provider": "anthropic"
            }
        });

        // 被控端该怎么提取：token / 花费**跨模型累加**，窗口上限**只取主模型那一项**。
        let 主模型 = "claude-opus-5";
        let mut usage = LlmUsage::default();
        for (名, 项) in model_usage.as_object().expect("modelUsage 是 map") {
            let 取 = |k: &str| 项[k].as_u64().unwrap_or(0);
            usage.input_tokens += 取("inputTokens");
            usage.output_tokens += 取("outputTokens");
            usage.cache_read_tokens += 取("cacheReadInputTokens");
            usage.cache_write_tokens += 取("cacheCreationInputTokens");
            if 名 == 主模型 {
                usage.context_limit = 取("contextWindow");
                usage.max_output_tokens = 取("maxOutputTokens");
            }
        }
        // 花费：上游浮点自带表示误差（样本 B 原值就是 0.24026150000000002），**向上取整**。
        usage.cost_micro_usd = (0.240_261_500_000_000_02_f64 * 1e6).ceil() as u64;
        // 上下文占用取本轮**最后一次** API 调用（样本 B 第 11 行 message.usage）的三项之和。
        usage.context_used = 2 + 4_784 + 33_556;

        assert_eq!(
            usage.cost_micro_usd, 240_262,
            "0.9 µUSD 的一轮不得被截断成免费"
        );
        assert_eq!(
            usage.context_limit, 1_000_000,
            "窗口上限必须取主模型那一项：跨模型求和 / 取最大都会给出一个不存在的窗口"
        );
        assert_eq!(usage.max_output_tokens, 64_000);
        assert_ne!(
            usage.context_limit, 0,
            "contextWindow 是真值 → 手机端能算真实占用百分比，不必再自己编一个估算数"
        );

        // 两个口径必须分开：账务是整轮累加，上下文占用是末次真值。
        let 累加占用 = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
        assert!(
            累加占用 > usage.context_used,
            "整轮累加（{累加占用}）必然大于末次真实占用（{}）——照累加口径画进度条，\
             一轮几十次工具调用就会显示占用超过 100%",
            usage.context_used
        );

        // 提取结果里不得残留 map 的任何痕迹。
        let 线上 = serde_json::to_string(&usage).expect("序列化");
        for 禁字 in [
            "contextWindow",
            "maxOutputTokens",
            "cacheReadInputTokens",
            "canonicalModel",
            "provider",
            "costUSD",
            "claude-opus-5",
            "another-model",
        ] {
            assert!(
                !线上.contains(禁字),
                "modelUsage 被原样透传了「{禁字}」: {线上}"
            );
        }
    }

    // ── 样本 B 实测行（2026-08-08，工具调用成功路径）──────────────────────────
    //
    // 内联成常量而不是读 `docs/调研/*.jsonl`：测试不该依赖文档目录（文档挪窝、脱敏重写都会让
    // 测试莫名其妙地红）。
    //
    // **与样本文件的差异只有一处，列全在这里**（「样本驱动」的全部价值在于「没人手改过」，
    // 一处未声明的删改就让下一个人无从判断还有没有别的删改）：
    // `样本B_助手思考` 的 `signature` 从 456 字符截到 76 字符——它是上游不透明凭据，不参与任何断言
    // （断言只看「`thinking` 为空 + `signature` 非空」）。除此之外**逐字节一致**，脱敏占位照抄。

    /// 第 8 行：`tool_use` 块的真实键集是 type / id / name / input / **caller**，
    /// 行顶层还有 **request_id**。后两者本模块刻意不建模。
    ///
    /// （`input.file_path` 在样本里已随第 9 行的 `file.filePath` 一并脱敏成 `<<REDACTED>>`——
    /// 同一次采样里同一个绝对路径出现在两处，头一版脱敏只换了后者，是「黑名单式脱敏必漏」
    /// 又一个活样本，只不过这次漏在了脱敏脚本自己身上。）
    const 样本B_助手工具调用: &str = r#"{"type": "assistant", "message": {"model": "claude-opus-5", "id": "msg_011CdqKZp4of3WwcK7Sx52Kj", "type": "message", "role": "assistant", "content": [{"type": "tool_use", "id": "toolu_019Vk9BtrSBpc2dtv9YBPTY3", "name": "Read", "input": {"file_path": "<<REDACTED>>"}, "caller": {"type": "direct"}}], "stop_reason": null, "stop_sequence": null, "stop_details": null, "usage": {"input_tokens": 1, "cache_creation_input_tokens": 13229, "cache_read_input_tokens": 20327, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 13229}, "output_tokens": 21, "service_tier": "standard", "inference_geo": "not_available"}, "diagnostics": null, "context_management": null}, "parent_tool_use_id": null, "session_id": "92373355-d1b5-4084-8476-22f0504fcfd7", "uuid": "84e69a79-7e05-4611-9316-4b70f9f66aaf", "timestamp": "2026-08-08T12:58:30.738Z", "request_id": "req_011CdqKZnWo93WokLNU7b2m2"}"#;

    /// 第 9 行：`tool_result` 块**没有 `is_error` 键**、`content` 是纯字符串；行顶层多出一个
    /// 公开 Messages API 没有的 `tool_use_result`。
    const 样本B_用户工具结果: &str = r#"{"type": "user", "message": {"role": "user", "content": [{"tool_use_id": "toolu_019Vk9BtrSBpc2dtv9YBPTY3", "type": "tool_result", "content": "<<REDACTED 12272 chars>>"}]}, "parent_tool_use_id": null, "session_id": "92373355-d1b5-4084-8476-22f0504fcfd7", "uuid": "59b094f7-d005-48d2-b958-0c30b231c472", "timestamp": "2026-08-08T12:58:30.757Z", "tool_use_result": {"type": "text", "file": {"filePath": "<<REDACTED>>", "content": "<<REDACTED 11720 chars of file body>>", "numLines": 165, "startLine": 1, "totalLines": 165}}}"#;

    /// 第 10 行：全新的顶层事件类型 `rate_limit_event`。
    const 样本B_限流事件: &str = r#"{"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "resetsAt": 1786211400, "rateLimitType": "five_hour", "overageStatus": "rejected", "overageDisabledReason": "out_of_credits", "isUsingOverage": false}, "uuid": "e88466fe-058f-4693-998f-96d787a14a8c", "session_id": "92373355-d1b5-4084-8476-22f0504fcfd7"}"#;

    /// 第 11 行：`thinking` 块——**`thinking` 是空串**，只有 `signature` 有值。
    /// （**本常量是唯一一处非逐字节复制**：`signature` 原文 456 字符不透明 base64，此处只留前
    /// 76 字符；截短不影响任何断言，断言只看「`thinking` 为空 + `signature` 非空」这两件事。）
    const 样本B_助手思考: &str = r#"{"type": "assistant", "message": {"model": "claude-opus-5", "id": "msg_011CdqKZyycMy8EDp7qAaoRS", "type": "message", "role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "CAISzwIKhwEIEBgCKkCzTwfwfFMxkrnqQFwswhnLjCLoKxmngSV8f24MmHNFjChQTxCHGwi8qQce"}], "stop_reason": null, "stop_sequence": null, "stop_details": null, "usage": {"input_tokens": 2, "cache_creation_input_tokens": 4784, "cache_read_input_tokens": 33556, "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 4784}, "output_tokens": 3, "service_tier": "standard", "inference_geo": "not_available"}, "diagnostics": null, "context_management": null}, "parent_tool_use_id": null, "session_id": "92373355-d1b5-4084-8476-22f0504fcfd7", "uuid": "935de19c-82a3-4c37-9f60-42450db9ea24", "timestamp": "2026-08-08T12:58:33.306Z", "request_id": "req_011CdqKZxbX8imQsHuhzYEtL"}"#;

    /// 第 12 行（`result`）**按抠出的结构重建**：原行被 PowerShell `>` 重定向的编码损坏
    /// （UTF-8 字节被按 GBK 解码再重编码，不可逆）。**只保留已抠出确切值的字段**——
    /// `ttft_ms` / `ttft_stream_ms` / `time_to_request_ms` 与 `modelUsage` 的**数值**没抠出来，
    /// 故一概不写进本常量（`modelUsage` 的字段名走 `llm_模型用量map提取成定长结构` 那条测试）。
    const 样本B_结果行重建: &str = r#"{"type": "result", "subtype": "success", "is_error": false, "stop_reason": "end_turn", "terminal_reason": "completed", "duration_ms": 25900, "duration_api_ms": 25727, "total_cost_usd": 0.24026150000000002}"#;

    /// **样本 B 实测行驱动**：本模块必须能表达这次真实采样的每一条事实，且**只**上行白名单内容。
    ///
    /// 这条测试是本轮所有形状改动的验收——它同时钉住 4 件在设计初稿里全错的事：
    /// 思考没有正文、工具结果有结构化元数据、限流是独立事件、`terminal_reason` 有 `completed`。
    #[test]
    fn llm_样本b实测行可被本协议表达() {
        let 取 = |s: &str| -> serde_json::Value {
            serde_json::from_str(s).expect("样本行必须合法")
        };

        // ① tool_use：caller / request_id 实测存在，本模块刻意不上行。
        let 助手 = 取(样本B_助手工具调用);
        let 调用 = &助手["message"]["content"][0];
        assert_eq!(调用["type"], "tool_use");
        assert!(
            调用.get("caller").is_some(),
            "caller 在样本里确实存在——不建模是取舍，不是没看见"
        );
        assert!(助手.get("request_id").is_some(), "request_id 同上");
        let 调用块 = LlmBlockEntry {
            block_id: 0,
            // 实测 parent_tool_use_id 为 null → None（不是空串）。
            parent_call_id: 助手["parent_tool_use_id"].as_str().map(str::to_owned),
            block: LlmBlock::ToolUse {
                call_id: 调用["id"].as_str().expect("id").to_owned(),
                name: 调用["name"].as_str().expect("name").to_owned(),
                title: Some(LlmText::new("读取 README.md")),
                input: LlmToolInput::new(调用["input"].clone()),
                truncated_bytes: None,
            },
        };
        assert_eq!(调用块.parent_call_id, None);
        let 线上 = serde_json::to_string(&调用块).expect("序列化");
        assert!(!线上.contains("caller"), "caller 不得上行: {线上}");
        assert!(!线上.contains("req_011"), "request_id 不得上行: {线上}");
        assert!(
            !线上.contains("92373355"),
            "CLI session_id 不得随块上行: {线上}"
        );

        // ② thinking：空串 + 有 signature → Omitted（不是 Text("")，更不是把 signature 塞进去）。
        let 思考行 = 取(样本B_助手思考);
        let 思考块 = &思考行["message"]["content"][0];
        assert_eq!(思考块["type"], "thinking");
        assert_eq!(
            思考块["thinking"], "",
            "thinking 恒为空串——这是把思考降级成状态信号的全部依据"
        );
        assert!(
            !思考块["signature"].as_str().expect("signature").is_empty(),
            "只有 signature 有值"
        );
        let 归一思考 = LlmBlock::Thinking {
            content: LlmThinking::Omitted,
        };
        let 线上 = serde_json::to_string(&归一思考).expect("序列化");
        assert!(!线上.contains("signature"), "不透明凭据不得上行: {线上}");
        assert!(!线上.contains("text"), "Omitted 线上不该有文本字段: {线上}");

        // ③ tool_result：is_error 是「键不存在」；正文用 tool_use_result.file 的元数据代替。
        let 用户 = 取(样本B_用户工具结果);
        let 结果块 = &用户["message"]["content"][0];
        assert_eq!(结果块["type"], "tool_result");
        assert!(
            结果块.get("is_error").is_none(),
            "实测 is_error 是**键不存在**，不是 false——按必填解析会让每次正常工具结果都失败"
        );
        assert!(结果块["content"].is_string(), "content 实测是纯字符串");
        let 状态 = match 结果块.get("is_error").and_then(serde_json::Value::as_bool) {
            Some(true) => LlmToolStatus::Error,
            // 缺键与 false 走同一条：见 [`LlmToolStatus`] 的映射规则。
            _ => LlmToolStatus::Ok,
        };
        assert_eq!(状态, LlmToolStatus::Ok);
        let 文件 = &用户["tool_use_result"]["file"];
        let 取行 = |v: &serde_json::Value| {
            u32::try_from(v.as_u64().unwrap_or(0)).expect("行号溢出 u32 说明样本不对劲")
        };
        let 结果 = LlmBlock::ToolResult {
            call_id: 结果块["tool_use_id"]
                .as_str()
                .expect("tool_use_id")
                .to_owned(),
            status: 状态,
            // 正文（样本里已被脱敏占位替换，占位串自己记着实测长度 12 272 字符）不上行。
            output: LlmText::default(),
            truncated_bytes: Some(12_272),
            detail: Some(LlmToolResultDetail::File {
                path: LlmPath::new(文件["filePath"].as_str().expect("filePath")),
                start_line: 取行(&文件["startLine"]),
                line_count: 取行(&文件["numLines"]),
                total_lines: 取行(&文件["totalLines"]),
            }),
        };
        assert_eq!(
            结果,
            LlmBlock::ToolResult {
                call_id: "toolu_019Vk9BtrSBpc2dtv9YBPTY3".into(),
                status: LlmToolStatus::Ok,
                output: LlmText::default(),
                truncated_bytes: Some(12_272),
                detail: Some(LlmToolResultDetail::File {
                    path: LlmPath::new("<<REDACTED>>"),
                    start_line: 1,
                    line_count: 165,
                    total_lines: 165,
                }),
            }
        );
        let 线上 = serde_json::to_string(&结果).expect("序列化");
        assert!(!线上.contains("file body"), "文件正文不得上行: {线上}");
        assert!(
            线上.len() < 256,
            "12 KiB 正文的一次 Read 应压成不到 256 B，实得 {} B",
            线上.len()
        );

        // ④ rate_limit_event：新的顶层事件类型 → 独立帧。
        let 限流 = 取(样本B_限流事件);
        assert_eq!(限流["type"], "rate_limit_event");
        let 信息 = &限流["rate_limit_info"];
        // **`status` 缺席 = 整条事件丢弃，不发 `LlmFrame::RateLimit`**：它是本事件存在的前提，
        // 也正是它唯一一个没有 `#[serde(default)]` 的原因（`LlmRateLimitStatus` 没有有意义的
        // 默认值，`Other("")` 是协议自己定义为噪声的东西，`open_enum!` 明令禁止塞进 `Other`）。
        // 这段是被控端归一化的范本，别把 `None` 写成 `from_wire_clamped("")`。
        let Some(状态原文) = 信息["status"].as_str() else {
            panic!("样本里 status 必在；真实被控端在这里应 return、丢弃整条事件");
        };
        let 归一限流 = LlmRateLimit {
            status: match 状态原文 {
                "allowed" => LlmRateLimitStatus::Allowed,
                其它 => LlmRateLimitStatus::from_wire_clamped(其它),
            },
            window: 信息["rateLimitType"].as_str().map(|w| match w {
                "five_hour" => LlmRateLimitWindow::FiveHour,
                其它 => LlmRateLimitWindow::from_wire_clamped(其它),
            }),
            resets_at_secs: 信息["resetsAt"].as_i64(),
            overage_status: 信息["overageStatus"].as_str().map(|s| match s {
                "rejected" => LlmOverageStatus::Rejected,
                其它 => LlmOverageStatus::from_wire_clamped(其它),
            }),
            overage_disabled_reason: 信息["overageDisabledReason"].as_str().map(|r| match r {
                "out_of_credits" => LlmOverageDisabledReason::OutOfCredits,
                其它 => LlmOverageDisabledReason::from_wire_clamped(其它),
            }),
            // 没有 `unwrap_or(false)`：键缺席就是 `None`（上游没说），不是「没在超额」。
            using_overage: 信息["isUsingOverage"].as_bool(),
        };
        assert_eq!(归一限流.status, LlmRateLimitStatus::Allowed);
        assert_eq!(归一限流.window, Some(LlmRateLimitWindow::FiveHour));
        assert_eq!(归一限流.resets_at_secs, Some(1_786_211_400));
        assert_eq!(归一限流.overage_status, Some(LlmOverageStatus::Rejected));
        assert_eq!(
            归一限流.overage_disabled_reason,
            Some(LlmOverageDisabledReason::OutOfCredits)
        );
        assert_eq!(
            归一限流.using_overage,
            Some(false),
            "样本里 isUsingOverage 显式给了 false ⇒ Some(false)；没给键才是 None"
        );

        // ⑤ result：terminal_reason 实测 "completed"，且 subtype 与样本 A 一模一样。
        let 结果行 = 取(样本B_结果行重建);
        assert_eq!(
            结果行["subtype"], "success",
            "样本 A（失败）与样本 B（成功）的 subtype 是同一个值——判成败只能看 is_error"
        );
        let outcome = LlmTurnOutcome {
            // 缺键按 false 解释，与本字段的 #[serde(default)] 一致；两次采样该键都在。
            is_error: 结果行["is_error"].as_bool().unwrap_or(false),
            terminal_reason: 结果行["terminal_reason"].as_str().map(|w| match w {
                "completed" => LlmTerminalReason::Completed,
                "api_error" => LlmTerminalReason::ApiError,
                其它 => LlmTerminalReason::from_wire_clamped(其它),
            }),
            api_error_status: 结果行
                .get("api_error_status")
                .and_then(serde_json::Value::as_u64)
                .and_then(|s| u16::try_from(s).ok()),
        };
        assert!(!outcome.is_error);
        assert_eq!(outcome.terminal_reason, Some(LlmTerminalReason::Completed));
        assert_eq!(
            outcome.api_error_status, None,
            "成功路径没有 api_error_status"
        );
        // 花费：上游浮点原值就带表示误差，向上取整。
        let 花费 = (结果行["total_cost_usd"].as_f64().expect("cost") * 1e6).ceil() as u64;
        assert_eq!(花费, 240_262);

        // ⑥ 归一化映射表不可省：上游是 snake_case，本协议线上是 PascalCase。直接拿上游原文喂
        //    from_wire_clamped，一个本端明明认识的值会静默落进 Other，UI 从此走「未知」分支。
        assert_eq!(
            LlmTerminalReason::from_wire_clamped("completed"),
            LlmTerminalReason::Other("completed".into()),
            "这正是 open_enum! 文档里那条「先查映射表」存在的理由"
        );
        assert_eq!(
            LlmTerminalReason::from_wire_clamped("Completed"),
            LlmTerminalReason::Completed
        );
        assert_eq!(
            LlmRateLimitWindow::from_wire_clamped("five_hour"),
            LlmRateLimitWindow::Other("five_hour".into())
        );
    }
}
