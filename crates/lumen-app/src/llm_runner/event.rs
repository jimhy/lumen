//! 上游一行 JSON → 内部事件的**第一层**：信封解析 + 白名单判定 + 归一化事件词表。
//!
//! # 分层：这一层做什么、不做什么
//!
//! - **做**：把一行文本解析出所有 CLI 事件共有的顶层信封（`type` / `subtype` / `session_id` /
//!   `uuid`），并回答一个问题——**这一行要不要继续解析**。
//! - **不做**：认识任何一家 CLI 的具体消息形状。那是 [`super::adapter::LlmAgentDecoder`]
//!   （第二层，片 3 给 Claude 实现）的事。
//!
//! 分层的价值在于白名单只有**一个**入口：任何一行想被继续解析，都必须先过
//! [`classify`]。反面做法（每个适配器各自决定丢什么）已经在设计评审里被否决——
//! 白名单散在 N 个适配器里，等于 N 份必须同步维护的安全清单，漏一处就是一次内容泄漏。
//!
//! # ⚠ 白名单铁律（`lumen_protocol::llm` 模块文档铁律一在被控端的落点）
//!
//! **2026-08-08 本机实测**（脱敏样本见 `docs/调研/M7-claude-headless-stream-json-实测样本*.jsonl`）：
//!
//! - `system.subtype = "hook_response"` 的 **`stdout` 字段**携带 4106 字符（6976 UTF-8 字节）的
//!   **本机私人记忆库全文**——用户配置的任意 `SessionStart` / `PreToolUse` hook 的 stdout 都会
//!   原样出现在这里，可能含密钥、私人笔记、内网地址。
//! - `system.subtype = "init"` 一行带走 `memory_paths` 绝对路径、40 个工具名、`plugins` /
//!   `mcp_servers` / `slash_commands` / `agents` / `skills` 清单。
//!
//! 故本模块的默认行为是**丢弃**，不是透传：
//!
//! 1. [`DENY_SYSTEM_SUBTYPES`] 是**第一道门**（deny-first），在白名单之前判。它单独并不足以
//!    保证安全（黑名单永远列不全，这正是铁律一的起因），但把它放在最前面意味着：**将来任何人
//!    误把 `system` 的白名单放宽，也带不出 hook 内容**。两道门是刻意的冗余。
//! 2. [`classify`] 只对 [`PARSE_TOP_TYPES`] 里的顶层 `type` 返回
//!    [`LineVerdict::Parse`]，其余一律丢。
//! 3. 被丢的行**只把标识（`type` / `subtype`）计入去重计数**（[`UnknownEventTally`]），
//!    **绝不记内容**——`hook_response` 的内容正是我们要挡住的那类东西。
//!
//! # 另一条容易破功的地方：错误信息本身也会泄漏内容
//!
//! `serde_json::Error` 的 `Display` 在类型不匹配时会把**实际值**写进消息（例如
//! `invalid type: string "sk-ant-…", expected u64`）。所以本模块**从不保存 serde 错误字符串**，
//! 只留 [`serde_json::error::Category`] 与行列号（见 [`MalformedKind`]）。这不是洁癖：协议错误
//! 是要写进审计日志与 `log::warn` 的，一条带原文的错误消息会绕过上面整套白名单。

use std::collections::BTreeMap;

use lumen_protocol::llm::{
    clamp_enum_wire, LlmAgentFeatures, LlmBlockEntry, LlmPath, LlmPermissionMode, LlmRateLimit,
    LlmStopReason, LlmText, LlmToolInput, LlmTurnOutcome, LlmUsage,
};
use serde::Deserialize;

// ── 白名单 / 黑名单表 ─────────────────────────────────────────────────────────

/// **第一道门**：无论白名单怎么改，这些 `system` 子类型一律丢弃。
///
/// 实测二者都会携带用户 hook 的 stdout/stderr 全文（见模块文档）。`hook_started` 本身不带内容，
/// 但与 `hook_response` 成对出现、语义同源，一并挡掉可以避免"只挡了带内容的那半边"这种半道门。
pub const DENY_SYSTEM_SUBTYPES: &[&str] = &["hook_started", "hook_response"];

/// `system` 行里**允许继续解析**的子类型。
///
/// - `init`：只取 `capabilities` / `model` / `cwd` / `permissionMode` / `claude_code_version`
///   五项（能力探测与基线元信息），同一行其余 17 个 key 一律就地丢弃。
/// - `status`：⚠ **两份样本共 22 行里都没有任何 `status` 行**，形态属推定。留在白名单里是因为
///   §6.6 表把它列为 `permission_mode` 的刷新通道；补样之前适配器解不出它只会得到一条
///   [`ProtocolErrorKind::DecodeFailed`]，不影响其余事件。
pub const PARSE_SYSTEM_SUBTYPES: &[&str] = &["init", "status"];

/// **允许继续解析**的顶层 `type`。不在表内的一律丢弃 + 计数。
///
/// `stream_event` 之类未观测到的类型**刻意不预留**：凭空写进白名单的类型名一旦拼错，
/// 表现是"某天上游真的发了这个类型而我们仍然丢"，比现在多一条未识别计数更难查。
pub const PARSE_TOP_TYPES: &[&str] = &[
    // 助手消息（文本 / 思考 / 工具调用；`is_api_error_message` 也在这条上）。
    "assistant",
    // 用户消息回显（`isReplay`）与工具结果（`tool_result` + 顶层 `tool_use_result`）。
    "user",
    // 一轮的权威成败判定。
    "result",
    // 账号额度状态（样本 B 新观测到的顶层事件）。
    "rate_limit_event",
    // 控制通道（`initialize` / `set_permission_mode` / `interrupt` 的应答）。
    "control_response",
    // 反向控制请求（`can_use_tool`；⚠ 通道存在但触发时机与 payload 未验证）。
    "control_request",
    // `system` 行按 `subtype` 二次判定，见 `classify`。
    "system",
];

/// [`UnknownEventTally`] 最多记多少个**不同**的标识。
///
/// 有上限不是性能考虑：`subtype` 是上游可控字符串，没有上限时一个每行都换名字的上游
/// （或一段乱码流被误判成 JSON）会让这张表无界增长。超出后并入 [`UnknownEventTally::overflow`]。
pub const UNKNOWN_EVENT_TALLY_MAX_KEYS: usize = 32;

// ── 信封 ──────────────────────────────────────────────────────────────────────

/// 一行 CLI 事件的**顶层信封**——所有行共有的四个字段。
///
/// # 为什么是借用而不是 `String`
/// 这是热路径（每行一次），四个字段都是短标识。`Cow` + `#[serde(borrow)]` 让**无转义**的常见
/// 情形零分配，有转义时自动退化成 `Owned`（直接写 `&'a str` 会在遇到 `\u` 转义时整行解析失败——
/// 那是一个只在特定内容下偶发的 bug，不能留）。
///
/// # 刻意**不加** `deny_unknown_fields`
/// CLI 每个小版本都在加字段（实测 `init` 一行 22 个 key）。严格模式等于给自己埋一颗版本地雷：
/// 上游加一个键，我们整条流全灭。这与"严出"不矛盾——**宽进（解析容忍未知字段）、严出
/// （转发只放白名单）**，两条各管一头。
#[derive(Debug, Clone, Deserialize)]
pub struct LineEnvelope<'a> {
    /// 顶层事件类型（`system` / `assistant` / `user` / `result` / …）。**必填**，缺失即畸形。
    #[serde(rename = "type", borrow)]
    pub ty: std::borrow::Cow<'a, str>,
    /// 子类型，只有 `system` / `result` 行有。
    #[serde(default, borrow)]
    pub subtype: Option<std::borrow::Cow<'a, str>>,
    /// CLI 自己的会话 id。**与 `super::RunnerId` 是两个空间**，不可互换。
    #[serde(default, borrow)]
    pub session_id: Option<std::borrow::Cow<'a, str>>,
    /// 本条事件的唯一 id，供去重（归一化硬规则 1：`turn_id` 由 runner 自增，`uuid` 只做去重）。
    #[serde(default, borrow)]
    pub uuid: Option<std::borrow::Cow<'a, str>>,
}

impl LineEnvelope<'_> {
    /// 便于匹配的 `&str` 视图。
    #[must_use]
    pub fn subtype_str(&self) -> Option<&str> {
        self.subtype.as_deref()
    }
}

/// 信封解析失败的**分类**——刻意不带任何原文（见模块文档最后一段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedKind {
    /// 这行根本不是 JSON（或 JSON 语法错）。`line`/`column` 来自 serde，纯位置信息。
    Syntax { line: usize, column: usize },
    /// 是合法 JSON，但形状对不上信封（例如 `type` 是数字、或整行是个数组）。
    Shape,
    /// 是合法 JSON 对象，但**没有 `type` 键**。样本文件里两条因编码损坏被替换成
    /// `{"_note":…,"_len":…}` 占位的行走的就是这条路径。
    MissingType,
    /// JSON 在行尾就断了（理论上不会发生——切分器保证给的是完整行；留着是为了让
    /// `Category::Eof` 有个明确归属，而不是被含糊地并进 `Syntax`）。
    Eof,
}

/// [`classify`] 的判定结果。**四个分支与设计蓝图 §6.6 表的四种处置一一对应**。
#[derive(Debug)]
pub enum LineVerdict<'a> {
    /// 空行 / 纯空白：静默跳过，不计数、不报错。CLI 正常不会发空行，但管道边界、
    /// 进程被 kill 时的半行残留都可能产生，把它当协议错误只会刷屏。
    Blank,
    /// 白名单内，交给适配器做第二层解析。
    Parse(LineEnvelope<'a>),
    /// 白名单外但**形状已知**（`system` 的 hook / 未知 subtype）：静默丢弃 + 去重计数。
    DropSilently { tag: DropTag },
    /// **未知顶层 `type`**：丢弃 + 去重计数，并额外报一条
    /// [`ProtocolErrorKind::UnknownSchema`]（§6.6 表末第二行的要求）。
    ///
    /// 与 [`Self::DropSilently`] 分开不是形式主义：未知 `type` 意味着**上游出现了我们完全没
    /// 建模的事件类别**（比如把关键信息挪到了新类型上），这是需要告警的；而未知 `system`
    /// subtype 在实测里就是 hook 那一类噪声，报错等于自我刷屏。
    UnknownSchema { tag: DropTag },
    /// 连信封都解析不出。
    Malformed { kind: MalformedKind, bytes: usize },
}

/// 被丢弃事件的**去重计数键**：`type` 或 `type/subtype`。
///
/// **只装标识、绝不装内容**（归一化硬规则 3 的那条 ⚠）。两段各按
/// [`lumen_protocol::llm::LLM_ENUM_WIRE_MAX_BYTES`] 在 **UTF-8 字符边界**夹紧——
/// 上游给的 `subtype` 是对端可控字符串，裸 `&s[..N]` 遇到多字节字符会 panic。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DropTag(String);

impl DropTag {
    #[must_use]
    fn new(ty: &str, subtype: Option<&str>) -> Self {
        match subtype {
            Some(sub) => Self(format!("{}/{}", clamp_enum_wire(ty), clamp_enum_wire(sub))),
            None => Self(clamp_enum_wire(ty).to_owned()),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DropTag {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 未识别 / 被丢弃事件的**去重计数**。
///
/// 归一化硬规则 3 的缓解措施：跳得太狠的反面风险是"新版本 CLI 把关键信息挪到新 subtype 里，
/// 我们表现为功能悄悄少了一块而无任何告警"。故把跳过的标识做去重计数，供桌面图标弹层的诊断行
/// 显示「本会话出现 N 种未识别事件」并写进审计日志。
#[derive(Debug, Default)]
pub struct UnknownEventTally {
    counts: BTreeMap<DropTag, u64>,
    /// 超过 [`UNKNOWN_EVENT_TALLY_MAX_KEYS`] 个不同标识后的合并计数。
    overflow: u64,
    total: u64,
}

impl UnknownEventTally {
    pub fn record(&mut self, tag: &DropTag) {
        self.total = self.total.saturating_add(1);
        if let Some(slot) = self.counts.get_mut(tag) {
            *slot = slot.saturating_add(1);
            return;
        }
        if self.counts.len() >= UNKNOWN_EVENT_TALLY_MAX_KEYS {
            self.overflow = self.overflow.saturating_add(1);
            return;
        }
        self.counts.insert(tag.clone(), 1);
    }

    /// 不同标识的种数（弹层诊断行的「N 种未识别事件」）。
    #[must_use]
    pub fn distinct(&self) -> usize {
        self.counts.len()
    }

    /// 累计丢弃行数。
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// 因超过种数上限而未单独计数的行数。
    #[must_use]
    pub fn overflow(&self) -> u64 {
        self.overflow
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DropTag, u64)> {
        self.counts.iter().map(|(k, v)| (k, *v))
    }
}

/// 剥掉行首的 UTF-8 BOM（`U+FEFF`）。**只剥一次、只在行首**。
///
/// # 为什么非剥不可
/// `str::trim` 按 Unicode `White_Space` 判定，而 **`U+FEFF` 不在其中**——带 BOM 的
/// `system/init` 行会直接落 [`MalformedKind::Syntax`]（`serde_json` 不吃 BOM）。后果隐蔽得可怕：
/// [`RunnerEvent::Started`] 永不产生 ⇒ 能力位停在 baseline（`interrupt=false`）、
/// `cwd`/`model`/`cli_version` 永远未知、`cli_session_id` 不一致的告警永不触发，
/// 而 runner 看上去一切正常（`Block` 事件照常把它推进 `Busy`）。
///
/// BOM 不是理论风险：Windows 上一旦经 `.cmd` shim / PowerShell 包装起 CLI，重编码链路就会带上
/// 它（样本 B 里那两行「因编码损坏」的占位正是这类链路的证据）。
///
/// **调用方必须把剥完的那份同时用于 `classify` 与 `decode_line`**，否则解码器还会拿到带 BOM
/// 的原文再失败一次（见 `mod.rs` 的 `decode_chunk`）。
#[must_use]
pub fn strip_bom(line: &str) -> &str {
    line.strip_prefix('\u{feff}').unwrap_or(line)
}

/// 对超长行**不做**「缺 `type` 键」的二次探测，直接判 [`MalformedKind::Shape`]。
///
/// [`has_no_type_key`] 会把整行再建一棵 `serde_json::Value` 树，堆开销是原文的数倍；而单行上限
/// 是 [`super::MAX_LINE_BYTES`] = 32 MiB，一条超长但 `type` 类型不符的行能在读线程里瞬时吃掉
/// 几百 MB。这恰恰是 `adapter.rs` 论证「不用 `serde_json::Value`」时点名要避免的开销——
/// 白名单层自己又把它请回来就说不过去了。
///
/// 64 KiB 的依据：实测最长的一行是 8677 字节（`SessionStart` hook 的 `additionalContext`），
/// 而「缺 `type` 键」这种形态出现在**占位 / 截断行**上，本来就短。
pub const TYPE_PROBE_MAX_BYTES: usize = 64 * 1024;

/// **白名单判定的唯一入口**。
///
/// 判定顺序是有意义的，改动顺序前请先读模块文档：
/// 0. 剥 BOM（见 [`strip_bom`]）；
/// 1. 空行 → [`LineVerdict::Blank`]；
/// 2. 信封解析（失败 → [`LineVerdict::Malformed`]，**不带原文**）；
/// 3. **deny-first**：`system` 的 [`DENY_SYSTEM_SUBTYPES`] 无条件丢；
/// 4. `system` 的 [`PARSE_SYSTEM_SUBTYPES`] 放行，其余 subtype 静默丢；
/// 5. 其它顶层类型按 [`PARSE_TOP_TYPES`] 放行，不在表内 → [`LineVerdict::UnknownSchema`]。
#[must_use]
pub fn classify(line: &str) -> LineVerdict<'_> {
    let line = strip_bom(line);
    if line.trim().is_empty() {
        return LineVerdict::Blank;
    }

    let env: LineEnvelope<'_> = match serde_json::from_str(line) {
        Ok(env) => env,
        Err(err) => {
            // **只取分类与位置，绝不取 `err.to_string()`**：serde 的类型错误消息里带实际值。
            let kind = match err.classify() {
                serde_json::error::Category::Syntax => MalformedKind::Syntax {
                    line: err.line(),
                    column: err.column(),
                },
                serde_json::error::Category::Eof => MalformedKind::Eof,
                // `Data` 既可能是"缺 `type`"也可能是"`type` 不是字符串"。区分这两者要再解析一次，
                // 不值得——但"缺 `type`"是实测见过的形态（样本里的占位行），值得单独一探：
                // 只在这一步做一次**廉价**的键存在性检查。
                serde_json::error::Category::Data | serde_json::error::Category::Io => {
                    if has_no_type_key(line) {
                        MalformedKind::MissingType
                    } else {
                        MalformedKind::Shape
                    }
                }
            };
            return LineVerdict::Malformed {
                kind,
                bytes: line.len(),
            };
        }
    };

    let ty = env.ty.as_ref();

    if ty == "system" {
        let sub = env.subtype_str();
        // ① deny-first：hook 内容永远出不去，与白名单怎么写无关。
        if sub.is_some_and(|s| DENY_SYSTEM_SUBTYPES.contains(&s)) {
            return LineVerdict::DropSilently {
                tag: DropTag::new(ty, sub),
            };
        }
        // ② 白名单。
        if sub.is_some_and(|s| PARSE_SYSTEM_SUBTYPES.contains(&s)) {
            return LineVerdict::Parse(env);
        }
        // ③ `system` 的未知 subtype 是实测里的噪声主力，静默丢 + 计数，不报协议错误。
        return LineVerdict::DropSilently {
            tag: DropTag::new(ty, sub),
        };
    }

    if PARSE_TOP_TYPES.contains(&ty) {
        return LineVerdict::Parse(env);
    }

    LineVerdict::UnknownSchema {
        tag: DropTag::new(ty, None),
    }
}

/// 廉价判断"这行 JSON 对象里没有 `type` 键"。只在信封解析已经失败后调用一次，非热路径。
///
/// **超过 [`TYPE_PROBE_MAX_BYTES`] 直接返回 `false`**（即报 [`MalformedKind::Shape`]）：
/// 这一步会把整行建成 `serde_json::Value` 树，对 MB 级的行是几倍原文的堆开销。
/// 分不清 `MissingType` 与 `Shape` 只损失一点排障精度，吃掉几百 MB 内存则是真事故。
fn has_no_type_key(line: &str) -> bool {
    if line.len() > TYPE_PROBE_MAX_BYTES {
        return false;
    }
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(serde_json::Value::Object(map)) => !map.contains_key("type"),
        _ => false,
    }
}

// ── 归一化事件词表 ────────────────────────────────────────────────────────────

/// 协议 / 解析层面的错误。**不致命**——除 [`Self::StdinRejected`] / [`Self::StdinWriteFailed`]
/// 外都只影响一行，读线程继续读下一行（§6.3 理由 3 的"自愈点"）。
///
/// **所有变体都不带上游原文**：这是白名单铁律在错误路径上的延伸。带 `&'static str` 的
/// `at` 是**代码里写死的站点名**（如 `"assistant.message.content"`），不是上游数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolErrorKind {
    /// 未知顶层 `type`（§6.6 表末第二行）。整行丢弃，继续读下一行。
    UnknownSchema { tag: DropTag },
    /// 信封都解析不出。
    MalformedEnvelope { kind: MalformedKind, bytes: usize },
    /// 单行超过 [`super::MAX_LINE_BYTES`]，**整行丢弃**（绝不截断后硬喂 serde，见 `lines.rs`）。
    OverlongLine { bytes: usize },
    /// 白名单内但适配器解不动（字段形状与本端模型不符）。
    DecodeFailed { at: &'static str },
    /// **我们写进 stdin 的行被 CLI 判为非法 JSON**。
    ///
    /// 实测：`printf 'this is not json\n'` 喂进去，CLI **立刻 exit 1** 并在 stderr 打
    /// `Error parsing streaming input line: …: SyntaxError: …`——**一个坏字节 = 会话猝死**
    /// （§6.4）。识别依据就是 stderr 里的那句话（见 [`super::STDIN_REJECT_MARKER`]）。
    ///
    /// 这是整个子系统里最不该发生的错误：出现它说明 [`super::adapter::StdinLine`] 的构造保证
    /// 被绕过了（有人手拼了字符串，或给 `Serialize` 实现加了 raw value）。
    StdinRejected,
    /// 写 stdin 失败（管道断 / 写了半行）。
    ///
    /// **半行已经出去了 = 会话必死，不得重试**：重试会把第二条完整消息接在半行后面，
    /// 拼出一行更离谱的非法 JSON。正确处置是立刻停这个 runner。
    StdinWriteFailed,
}

/// `system/init` 归一化后的启动信息（§6.6 表第一行的右列）。
///
/// **白名单收窄**：实测 `init` 一行 22 个 key，这里只留 6 项。`memory_paths` / `plugins` /
/// `mcp_servers` / `agents` / `skills` / `slash_commands` / `tools` / `apiKeySource`
/// **一律不出 PC**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedInfo {
    /// CLI 自己的会话 id。正常情况下等于我们用 `--session-id` 传进去的那个 uuid；
    /// **不相等要告警**（说明 CLI 没接受我们的 id，`--resume` 会对不上）。
    pub cli_session_id: String,
    pub model: Option<String>,
    /// CLI 报告的 cwd（脱敏包装，见 [`LlmPath`]）。
    pub cwd: LlmPath,
    pub permission_mode: Option<LlmPermissionMode>,
    /// `claude_code_version`。**仅供排障，不作能力判据**——判据是 `features`。
    pub cli_version: Option<String>,
    /// 由 `init` 行的 `capabilities` 数组**真实探测**出的能力位。
    ///
    /// 实测值 `["interrupt_receipt_v1","interrupt_cancel_queued_v1","msg_lifecycle_v1"]`。
    /// 例如 `interrupt` 位由 `capabilities` 含 `interrupt_receipt_v1` 推出，
    /// **不要**去比 `cli_version`（版本号与能力的对应关系没人维护，猜出来的映射长期骗人）。
    /// 注意 `capabilities` 只是探测**依据**，不是可上行内容，原文不出 PC。
    pub features: LlmAgentFeatures,
}

/// 一轮结束的归一化信息（§6.6 的 `TurnEnded`）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEndedInfo {
    pub turn: u64,
    pub stop: LlmStopReason,
    /// **唯一权威的成败判定**。实测 `subtype` 两次都是 `"success"`、一次伴 `is_error=true`
    /// 一次伴 `false`——`subtype` **不上行、也不作判据**。
    pub outcome: LlmTurnOutcome,
    pub usage: LlmUsage,
    /// `permission_denials` 的**条数**。元素结构未实测，故只转条数（§5.0-4）。
    pub denials: u32,
    /// `result` 行的 `result` 字段（失败路径上装的是错误摘要）。
    pub result_text: Option<LlmText>,
    /// [`Self::result_text`] 被夹紧时裁掉的字节数；`None` = 一个字节都没裁。
    ///
    /// **降级必须可观测**（§14-6）：`result` 在成功路径上装的是整段最终回答，32 KiB ≈ 8k token，
    /// 长回答（大段代码 / 报告）越界完全现实。初稿 `.map(Clamped::into_text)` 把
    /// `Clamped::truncated_bytes()` 直接丢掉，成了全文件唯一一处**静默截断**——
    /// 别处（`ToolUse.truncated_bytes` / `ToolResult.truncated_bytes`）都把裁掉的字节数声明出来了。
    pub result_truncated_bytes: Option<u64>,
}

/// **runner 内部归一化事件**——读线程产出、主线程消费、片 4 据此拼 [`lumen_protocol::llm::LlmFrame`]。
///
/// # 为什么载荷直接用协议类型（[`LlmBlockEntry`] / [`LlmUsage`] / …）
/// 这是本枚举最重要的一个设计决定：**白名单在类型上就执行了**。协议侧
/// `lumen_protocol::llm` 全模块没有 `Raw(serde_json::Value)` 这类直通变体（唯一的
/// [`LlmToolInput`] 是"工具入参"的专用口子），所以只要事件载荷只用协议类型，
/// "把上游某个没建模的字段顺手带出去"这件事**在实现里根本写不出来**。
///
/// 被否决的替代方案：定义一套 runner 私有的中间模型再在片 4 转换成协议类型。那会多一层
/// 一一对应的样板代码，而且中间模型天然会长出 `extra: serde_json::Value` 这类"先存着以后再说"
/// 的字段——铁律一当场破功。
///
/// # 变体分两族
/// - **runner 自产**（`Spawned` / `Exited` / `LineDropped` / `ProtocolError`）：与 CLI 无关，
///   本模块与 [`super`] 自己产生。
/// - **适配器产出**（其余）：由 [`super::adapter::LlmAgentDecoder`] 从上游行归一化而来
///   （片 3 实现 Claude 分支）。
#[derive(Debug, Clone, PartialEq)]
pub enum RunnerEvent {
    // ── runner 自产 ─────────────────────────────────────────────────────────
    /// 子进程已 spawn。`pid` 供审计日志与排障。
    Spawned { pid: Option<u32> },
    /// 子进程退出。
    ///
    /// **`code` 单独不足以判因**：实测认证失败时 `exit code = 1` 且 **stderr 完全为空**，
    /// 失败信息只在 stdout 事件流里。故本变体额外带 `stderr_empty`，归因走
    /// [`super::ExitAttribution`]。
    Exited {
        code: Option<i32>,
        /// `true` = 是我们主动 kill / 优雅停止的，不是 CLI 自己退的。
        killed: bool,
        /// 退出时 stderr 尾部缓冲是否为空。
        stderr_empty: bool,
    },
    /// 一行被白名单挡掉（**只带标识、不带内容**）。
    LineDropped { tag: DropTag },
    /// 协议 / 解析错误。见 [`ProtocolErrorKind`]。
    ProtocolError { kind: ProtocolErrorKind },

    // ── 适配器产出（§6.6 右列）───────────────────────────────────────────────
    /// `system/init` → 会话就绪。`Box` 是因为本变体载荷显著大于其它变体
    /// （不 box 会把整个枚举撑大到最大变体的尺寸，而本变体每会话只出现一次）。
    Started(Box<StartedInfo>),
    /// `system/status` → 权限模式变更。⚠ 该行未实测。
    PermissionModeChanged { mode: LlmPermissionMode },
    /// 用户消息送达回执（`user` 且 `isReplay:true`）。⚠ 未实测，见 §6.6。
    TurnUserEcho {
        turn: u64,
        blocks: Vec<LlmBlockEntry>,
    },
    /// 一个**已定稿**的归一化块（文本 / 思考状态 / 工具调用 / 工具结果 / 错误块）。
    ///
    /// P0 不做块内流式：上游 headless 是**整块**给出的（`assistant` 行一次带完整
    /// `message.content[]`），没有 token 级增量可转发。片 4 把它包成
    /// `LlmDeltaItem::BlockStart` + `BlockEnd` 一起发。
    Block { turn: u64, entry: LlmBlockEntry },
    /// 账号额度状态播报（顶层 `rate_limit_event`）。**账号级、不属于任何一轮**。
    RateLimit { info: Box<LlmRateLimit> },
    /// 一轮结束。
    TurnEnded(Box<TurnEndedInfo>),
    /// 反向权限询问（`control_request` / `can_use_tool`）。⚠ 通道未实测。
    ///
    /// **尚无构造点**：`can_use_tool` 的 payload 完全未验证，`claude.rs` 刻意不建模它
    /// （见 `ClaudeDecoder::decode_line`）。变体先留着，是因为 `apply()` 的状态机已经为它
    /// 写好了 `AwaitingPermission` 那条分支。
    #[allow(dead_code)]
    PermissionAsk {
        /// **CLI 侧**的请求 id（字符串），与协议侧 `LlmFrame::PermissionRequest.request_id`
        /// （被控端分配的 `u64`）是两个空间，片 4 负责建映射。
        request_id: String,
        tool: String,
        input: LlmToolInput,
    },
    /// 我们发出的 `control_request` 的应答（`control_response`）。
    ///
    /// 设计表把它标成"内部消费、不外发"——**不外发是指不进线协议**，但主线程仍需要它来判断
    /// 「interrupt 到底送到没有」，否则中断按钮会变成一个没有反馈的黑洞。
    ControlAck { request_id: String, ok: bool },
}

impl RunnerEvent {
    /// 本事件在 transcript 环形缓冲里的**近似字节占用**。
    ///
    /// 只用于 [`super::TRANSCRIPT_MAX_BYTES`] 的预算控制，故只要求"同量级"不要求精确：
    /// 结构体自身大小 + 堆上文本/入参的字节数。刻意**不做**一次 `serde_json::to_vec` 去量真实
    /// 大小——那是每事件一次全量序列化，纯粹为了记账而付出的 O(n) 开销，得不偿失。
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        // 写成局部变量而不是 `const`：函数体内的 `const` 项**拿不到外层 impl 的 `Self`**
        // （item 不继承外层泛型环境），会直接编译失败。
        let base = std::mem::size_of::<Self>();
        base + match self {
            Self::Spawned { .. } | Self::Exited { .. } => 0,
            Self::LineDropped { tag } => tag.as_str().len(),
            Self::ProtocolError { kind } => match kind {
                ProtocolErrorKind::UnknownSchema { tag } => tag.as_str().len(),
                _ => 0,
            },
            Self::Started(info) => {
                info.cli_session_id.len()
                    + info.cwd.as_str().len()
                    + info.model.as_ref().map_or(0, String::len)
                    + info.cli_version.as_ref().map_or(0, String::len)
            }
            Self::PermissionModeChanged { .. } => 0,
            Self::TurnUserEcho { blocks, .. } => blocks.iter().map(block_entry_bytes).sum(),
            Self::Block { entry, .. } => block_entry_bytes(entry),
            Self::RateLimit { .. } => 0,
            Self::TurnEnded(info) => info.result_text.as_ref().map_or(0, LlmText::len_bytes),
            Self::PermissionAsk {
                request_id,
                tool,
                input,
            } => request_id.len() + tool.len() + tool_input_bytes(input),
            Self::ControlAck { request_id, .. } => request_id.len(),
        }
    }
}

/// 一个块条目的近似堆占用。
fn block_entry_bytes(entry: &LlmBlockEntry) -> usize {
    use lumen_protocol::llm::{LlmBlock, LlmThinking};
    let parent = entry.parent_call_id.as_ref().map_or(0, String::len);
    parent
        + match &entry.block {
            LlmBlock::Text { text } => text.len_bytes(),
            LlmBlock::Thinking { content } => match content {
                LlmThinking::Text { text } => text.len_bytes(),
                _ => 0,
            },
            LlmBlock::ToolUse {
                call_id,
                name,
                title,
                input,
                ..
            } => {
                call_id.len()
                    + name.len()
                    + title.as_ref().map_or(0, LlmText::len_bytes)
                    + tool_input_bytes(input)
            }
            LlmBlock::ToolResult {
                call_id, output, ..
            } => call_id.len() + output.len_bytes(),
            LlmBlock::Image { attachment } => {
                attachment.path.as_str().len() + attachment.name.len_bytes() + attachment.mime.len()
            }
            LlmBlock::Error { message, .. } => message.len_bytes(),
            LlmBlock::Unknown => 0,
        }
}

/// 工具入参的近似占用。
///
/// `LlmToolInput` 只暴露 `get() -> &serde_json::Value`，没有"字节数"接口。逐层递归量一次
/// 比 `to_string().len()` 便宜（后者要真的分配一份完整 JSON 文本）。
fn tool_input_bytes(input: &LlmToolInput) -> usize {
    fn walk(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Null => 4,
            serde_json::Value::Bool(_) => 5,
            serde_json::Value::Number(_) => 8,
            serde_json::Value::String(s) => s.len(),
            serde_json::Value::Array(items) => items.iter().map(walk).sum::<usize>() + items.len(),
            serde_json::Value::Object(map) => map
                .iter()
                .map(|(k, v)| k.len() + walk(v) + 3)
                .sum::<usize>(),
        }
    }
    walk(input.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag_of(v: &LineVerdict<'_>) -> String {
        match v {
            LineVerdict::DropSilently { tag } | LineVerdict::UnknownSchema { tag } => {
                tag.as_str().to_owned()
            }
            other => panic!("期望被丢弃，实际 {other:?}"),
        }
    }

    #[test]
    fn 空行与纯空白静默跳过() {
        assert!(matches!(classify(""), LineVerdict::Blank));
        assert!(matches!(classify("   \t "), LineVerdict::Blank));
    }

    #[test]
    fn 行首bom必须被剥掉_否则整条init白丢() {
        // `str::trim` 按 Unicode White_Space 判定，**U+FEFF 不在其中** ⇒ 不剥就落 Syntax，
        // 后果是 Started 永不产生、能力位永远停在 baseline，而 runner 看着一切正常。
        let line = "\u{feff}{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}";
        let LineVerdict::Parse(env) = classify(line) else {
            panic!("带 BOM 的 init 必须照常放行，实际 {:?}", classify(line));
        };
        assert_eq!(env.ty.as_ref(), "system");
        assert_eq!(env.subtype_str(), Some("init"));

        // 只剥一个、只在行首：BOM 出现在中间是内容，不是标记。
        assert_eq!(strip_bom("\u{feff}\u{feff}x"), "\u{feff}x");
        assert_eq!(strip_bom("x\u{feff}"), "x\u{feff}");
        // 只有 BOM 的一行 = 空行。
        assert!(matches!(classify("\u{feff}"), LineVerdict::Blank));
    }

    #[test]
    fn 超长行不再为了区分missing_type而重建整棵value树() {
        // 这一步会把整行建成 `serde_json::Value`，堆开销是原文数倍；行上限是 32 MiB。
        // 超过闸门就直接判 Shape，宁可损失一点排障精度。
        // `type` 是数字（合法 JSON、形状不符）+ 一大段填充把行撑过闸门。
        let long = format!(
            "{{\"type\":42,\"pad\":\"{}\"}}",
            "a".repeat(TYPE_PROBE_MAX_BYTES)
        );
        assert!(long.len() > TYPE_PROBE_MAX_BYTES);
        let LineVerdict::Malformed { kind, .. } = classify(&long) else {
            panic!("`type` 不是字符串的行应当是 Malformed");
        };
        assert_eq!(kind, MalformedKind::Shape);
        // 闸门之内仍然照常区分「缺 type 键」。
        let short = r#"{"_note":"占位","_len":12}"#;
        let LineVerdict::Malformed { kind, .. } = classify(short) else {
            panic!("缺 type 的行应当是 Malformed");
        };
        assert_eq!(kind, MalformedKind::MissingType);
    }

    #[test]
    fn hook行一律丢弃_这是白名单铁律的第一道门() {
        // 实测形状（样本 A 第 1、4 行），`stdout` 字段装着本机私人内容。
        let started = r#"{"type":"system","subtype":"hook_started","hook_id":"h1","uuid":"u","session_id":"s"}"#;
        let response = r#"{"type":"system","subtype":"hook_response","stdout":"私人记忆库全文","output":"摘要","uuid":"u","session_id":"s"}"#;
        assert_eq!(tag_of(&classify(started)), "system/hook_started");
        assert_eq!(tag_of(&classify(response)), "system/hook_response");
    }

    #[test]
    fn deny表排在白名单之前_即使被误加进白名单也带不出内容() {
        // 这条测试保护的是**顺序**：DENY_SYSTEM_SUBTYPES 与 PARSE_SYSTEM_SUBTYPES
        // 若哪天出现交集，deny 必须赢。
        for sub in DENY_SYSTEM_SUBTYPES {
            assert!(
                !PARSE_SYSTEM_SUBTYPES.contains(sub),
                "{sub} 同时出现在 deny 与 parse 表里"
            );
            let line = format!(r#"{{"type":"system","subtype":"{sub}"}}"#);
            assert!(matches!(classify(&line), LineVerdict::DropSilently { .. }));
        }
    }

    #[test]
    fn init行放行并取到信封四要素() {
        let line = r#"{"type":"system","subtype":"init","cwd":"F:\\x","session_id":"sid-1","uuid":"uu-1","model":"opus"}"#;
        let LineVerdict::Parse(env) = classify(line) else {
            panic!("init 必须放行");
        };
        assert_eq!(env.ty, "system");
        assert_eq!(env.subtype_str(), Some("init"));
        assert_eq!(env.session_id.as_deref(), Some("sid-1"));
        assert_eq!(env.uuid.as_deref(), Some("uu-1"));
    }

    #[test]
    fn system未知subtype静默丢弃并计数() {
        let line = r#"{"type":"system","subtype":"brand_new_thing"}"#;
        assert_eq!(tag_of(&classify(line)), "system/brand_new_thing");
        // 无 subtype 的 system 行同样被丢（形状未知）。
        assert_eq!(tag_of(&classify(r#"{"type":"system"}"#)), "system");
    }

    #[test]
    fn 白名单顶层类型全部放行() {
        for ty in PARSE_TOP_TYPES {
            if *ty == "system" {
                continue; // system 走 subtype 二次判定，另有用例。
            }
            let line = format!(r#"{{"type":"{ty}"}}"#);
            assert!(
                matches!(classify(&line), LineVerdict::Parse(_)),
                "{ty} 应放行"
            );
        }
    }

    #[test]
    fn 未知顶层类型报unknown_schema而不是静默丢() {
        let v = classify(r#"{"type":"lumen_unknown_type"}"#);
        assert!(matches!(v, LineVerdict::UnknownSchema { .. }));
        assert_eq!(tag_of(&v), "lumen_unknown_type");
    }

    #[test]
    fn 非json行报语法错且不带原文() {
        let LineVerdict::Malformed { kind, bytes } = classify("this is not json") else {
            panic!("应判为畸形");
        };
        assert!(matches!(kind, MalformedKind::Syntax { .. }));
        assert_eq!(bytes, "this is not json".len());
        // 关键：错误分类里没有任何地方能装下原文。
        let rendered = format!("{kind:?}");
        assert!(!rendered.contains("not json"));
    }

    #[test]
    fn 缺type键的对象被识别成missing_type() {
        // 样本文件里因编码损坏被替换成占位的那两行就是这个形状。
        let LineVerdict::Malformed { kind, .. } = classify(r#"{"_note":"redacted","_len":123}"#)
        else {
            panic!("应判为畸形");
        };
        assert_eq!(kind, MalformedKind::MissingType);
    }

    #[test]
    fn type不是字符串时报shape而不泄漏取值() {
        let LineVerdict::Malformed { kind, .. } = classify(r#"{"type":{"secret":"sk-ant-xxx"}}"#)
        else {
            panic!("应判为畸形");
        };
        assert_eq!(kind, MalformedKind::Shape);
        assert!(!format!("{kind:?}").contains("sk-ant"));
    }

    #[test]
    fn 顶层不是对象时不panic() {
        assert!(matches!(classify("[1,2,3]"), LineVerdict::Malformed { .. }));
        assert!(matches!(classify("42"), LineVerdict::Malformed { .. }));
        assert!(matches!(classify("\"x\""), LineVerdict::Malformed { .. }));
    }

    #[test]
    fn 信封字段带转义时不退化成解析失败() {
        // `&'a str` 在这里会失败，`Cow` 不会——这正是选 Cow 的原因。
        let line = r#"{"type":"assistant","uuid":"a\u0041b"}"#;
        let LineVerdict::Parse(env) = classify(line) else {
            panic!("带转义的信封必须仍能解析");
        };
        assert_eq!(env.uuid.as_deref(), Some("aAb"));
    }

    #[test]
    fn droptag按utf8字符边界夹紧_中文subtype不panic() {
        let long: String = "中".repeat(200); // 600 字节，远超 LLM_ENUM_WIRE_MAX_BYTES。
        let line = format!(r#"{{"type":"system","subtype":"{long}"}}"#);
        let tag = tag_of(&classify(&line));
        assert!(tag.starts_with("system/中"));
        assert!(tag.len() <= "system/".len() + 64);
        // 夹紧结果必须仍是合法 UTF-8（能走到这里就已经证明了，再断言一次防回归）。
        assert!(tag.chars().all(|c| c != '\u{FFFD}'));
    }

    #[test]
    fn 计数只记标识_种数封顶后并入overflow() {
        let mut tally = UnknownEventTally::default();
        for i in 0..(UNKNOWN_EVENT_TALLY_MAX_KEYS + 10) {
            tally.record(&DropTag::new("system", Some(&format!("sub{i}"))));
        }
        assert_eq!(tally.distinct(), UNKNOWN_EVENT_TALLY_MAX_KEYS);
        assert_eq!(tally.overflow(), 10);
        assert_eq!(tally.total(), UNKNOWN_EVENT_TALLY_MAX_KEYS as u64 + 10);
        // 重复标识累加而不占新槽位。
        let tag = DropTag::new("system", Some("sub0"));
        tally.record(&tag);
        assert_eq!(tally.distinct(), UNKNOWN_EVENT_TALLY_MAX_KEYS);
        assert_eq!(tally.iter().find(|(k, _)| **k == tag).unwrap().1, 2);
    }

    #[test]
    fn 事件字节估算随文本增长() {
        use lumen_protocol::llm::{LlmBlock, LlmBlockEntry};
        let small = RunnerEvent::Block {
            turn: 1,
            entry: LlmBlockEntry {
                block_id: 0,
                parent_call_id: None,
                block: LlmBlock::Text {
                    text: LlmText::new("hi"),
                },
            },
        };
        let big = RunnerEvent::Block {
            turn: 1,
            entry: LlmBlockEntry {
                block_id: 0,
                parent_call_id: None,
                block: LlmBlock::Text {
                    text: LlmText::new("x".repeat(10_000)),
                },
            },
        };
        assert!(big.approx_bytes() >= small.approx_bytes() + 9_000);
    }
}
