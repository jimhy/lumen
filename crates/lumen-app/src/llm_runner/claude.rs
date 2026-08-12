//! **Claude Code 适配器**：把 `claude -p --output-format stream-json` 的私有事件流翻译成
//! [`lumen_protocol::llm`] 的归一化模型。
//!
//! 本文件是 §6.6 那张「归一化事件映射表」的**唯一可执行副本**——表里右列写出来的字段就是白名单
//! 本身，**没写出来的字段一律不出 PC**（`llm.rs` 模块文档铁律一在被控端的最后一道落点）。
//!
//! # 一、依据分级：哪些是实测、哪些是推定（改代码前先看这里）
//!
//! 全部结论来自 2026-08-08 本机 CLI **2.1.226** 的两次采样，脱敏样本已复制进
//! `llm_runner/fixtures/`（**测试只读 crate 内的这两份，不读 `docs/`**——测试依赖文档目录会在
//! 有人整理文档时无声变红）：
//!
//! | 分支 | 依据 | 备注 |
//! |---|---|---|
//! | `system/init`、`assistant.text`、`is_api_error_message`、`result` | ✅ 样本 A | 失败路径（403） |
//! | `tool_use`、`thinking`、`tool_result`、`tool_use_result.file`、`rate_limit_event` | ✅ 样本 B | 成功路径 |
//! | `control_response` | ⚠ 有实测**无样本行**（三次手工 round-trip），**形状是推的** | 见 [`ClaudeDecoder::decode_control_response`] |
//! | `system/status`、`user{isReplay}`、`redacted_thinking` | ⚠ **两份样本共 22 行里一行都没有** | 分支留着但算未验证代码 |
//! | `control_request/can_use_tool` | ⚠ 通道存在、payload **完全未知** | **刻意不建模**，见 [`ClaudeDecoder::decode_line`] |
//!
//! **⚠ 行在补样之前一律不许改成 ✅**，也不许因为「公开 Messages API 是这么写的」就当成事实——
//! 这个 CLI 的 `tool_use` 上就多了一个公开 API 没有的 `caller`，`user` 行上多了一个
//! `tool_use_result`，`assistant` 行上多了 `request_id`。推定过一次就会再推第二次。
//!
//! # 二、四条只有实测才知道的坑（每一条都在代码里有对应分支）
//!
//! 1. **认证失败长得像一条正常回复**：它是一条**普通 `assistant` 消息 + 普通 `text` 块**
//!    （正文 `"Failed to authenticate. API Error: 403 Request not allowed"`），只有顶层
//!    `is_api_error_message = true` 与 `error = "authentication_failed"` 能识别。故
//!    [`ClaudeDecoder::decode_assistant`] **先判 `is_api_error_message` 再决定块类型**，
//!    顺序反了手机上就会出现一个「模型跟你说它认证失败了」的假气泡。
//! 2. **成败只能看 `result.is_error`**：`subtype` 两次采样都是 `"success"`，一次伴 `is_error=true`
//!    一次伴 `false`。本文件**根本不解析 `subtype`**——不解析比「解析了但不用」更安全。
//! 3. **`tool_result.is_error` 实测是「键不存在」而不是 `false`**：字段必须可缺省，缺键按
//!    [`LlmToolStatus::Ok`]。把它当必填会让**每一次正常的工具结果**都解析失败。
//! 4. **思考正文恒为空串**：`thinking` 块有 456 字符 `signature`、正文是 `""`，且 CLI 无开关。
//!    故 Claude 上恒为 [`LlmThinking::Omitted`]，`signature` **不出 PC**。
//!
//! # 三、白名单是怎么在**类型层**执行的（不是靠自觉）
//!
//! - 入站结构体**只声明要用的字段**。`hook_response.stdout`、`init.memory_paths`、
//!   `tool_use.caller`、`tool_use_result.file.content`、`assistant.request_id` 等
//!   **在本文件里根本没有对应字段**，serde 直接跳过、不分配。「忘了丢」这件事写不出来。
//! - 出站载荷只用协议类型（[`LlmBlock`] / [`LlmUsage`] / …），而 `lumen_protocol::llm` 全模块
//!   没有 `Raw(serde_json::Value)` 直通变体，唯一的 [`LlmToolInput`] 是工具入参专用口子。
//! - **工具输出 / 思考正文 / 错误正文 / 工具入参 / 开放枚举取值 / 标识串**在反序列化那一刻就被
//!   夹紧（[`Clamped`] / [`MaybeClamped`] / [`clamp_tool_input`] / 开放枚举的 `from_wire_clamped` /
//!   [`clamp_ident`]），本端结构里永远只存夹紧后的那份。被否决的写法是「先 `String` 收下 12 MB
//!   正文、再 `truncate`」——那份完整正文会在内存里存在一整个函数的时间，且极易被顺手 `clone`
//!   进日志。
//!
//!   ⚠ **唯一的例外是 `text` 块正文**（[`LlmBlock::Text`]，见 [`ClaudeDecoder::decode_assistant`]）：
//!   它**不夹紧**。硬夹 32 KiB 会截断合法的长回答，那是功能缺失而不是安全加固。它的上界由**片 4**
//!   的 `LLM_DELTA_MAX_BYTES` 预算 + 上游自己的 `max_output_tokens` 共同约束，不在本层。
//!   这条以前写成「上游正文一律夹紧」，与实现不符，容易让下一个人以为本层已经兜住了。
//!
//! # 四、被否决的替代方案（写在这里省得下一个人再走一遍）
//!
//! 1. **content block 用 `#[serde(tag = "type")]` 内部标签枚举**（最直观的写法）：内部标签的
//!    `Deserialize` 会先把整个 map **缓冲进 `Content`**，于是那条 12 MB 的 `tool_result.content`
//!    必然被完整物化一次，[`Clamped`] 的「反序列化即夹紧」当场失效。改用**扁平联合结构体**
//!    [`ContentBlock`] + 按 `type` 分派，serde_json 直接流进各字段，零缓冲。
//! 2. **`title` 自动生成**（「读取 README.md」）：要给每个工具写一份入参 schema 的解释逻辑，而
//!    `Bash`/`Edit`/`Write` 的入参形态**没采过样**。手机端拿着 `name` + `input` 自己渲染即可，
//!    这里恒填 `None`。
//! 3. **`--replay-user-messages`**（设计骨架里有）：该开关本身**未在样本上验证过**，而 CLI 遇到
//!    不认识的**命令行参数**是**启动即失败**（不是像未知 stdin `type` 那样静默忽略）——拿整个
//!    功能去赌一个没验证过的开关不值得。见 [`ClaudeAdapter::build_args`]。
//! 4. **`--disallowedTools` 硬编码黑名单**（设计骨架里有 `"Bash(rm *) …"`）：规则串的语法没验证过，
//!    **写错的规则匹配不到任何东西**，得到的是一道看着存在、实则不设防的围栏——比没有更糟
//!    （§14-6 无声降级禁令）。P0 的围栏是 `--add-dir` + `--permission-mode`；黑名单等谁验证了
//!    语法再由调用方经 `LaunchSpec` 传进来。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lumen_protocol::llm::{
    LlmAgentFeatures, LlmAgentKind, LlmAttachment, LlmBlock, LlmBlockEntry, LlmErrorCode,
    LlmOverageDisabledReason, LlmOverageStatus, LlmPath, LlmPermissionMode, LlmPermissionModeKnown,
    LlmRateLimit, LlmRateLimitStatus, LlmRateLimitWindow, LlmStopReason, LlmTerminalReason,
    LlmText, LlmThinking, LlmToolInput, LlmToolResultDetail, LlmToolStatus, LlmTurnOutcome,
    LlmUsage, LLM_TOOL_INPUT_MAX_BYTES, LLM_TOOL_OUTPUT_MAX_BYTES,
};
use serde::{Deserialize, Serialize};

use super::adapter::{
    ControlCmd, DecodeError, DecodeErrorKind, EncodeError, LaunchSpec, LlmAgentAdapter,
    LlmAgentDecoder, LlmAgentEncoder, StdinLine, TurnCursor,
};
use super::event::{LineEnvelope, RunnerEvent, StartedInfo, TurnEndedInfo};

// ── 常量：可执行名 / 能力标记 / 上游取值 ─────────────────────────────────────

/// 可执行文件名。**走 PATH 查找，不写死绝对路径**（见 [`LlmAgentAdapter::program`]）。
pub const CLAUDE_PROGRAM: &str = "claude";

/// `system/init` 的 `capabilities` 里表示「支持中断」的标记（实测值）。
///
/// 能力探测**读这个数组，不去比 `claude_code_version`**：版本号与能力的对应关系没人维护，
/// 猜出来的映射长期骗人（`llm.rs` 的 [`LlmAgentFeatures`] 文档点名要求这么做）。
pub const CAP_INTERRUPT: &str = "interrupt_receipt_v1";

/// 实测 `capabilities` 的另外两个值。**目前不映射成任何能力位**，列出来只为让下一个人知道
/// 「这两个我们看见了、是有意没用」，而不是漏了。
///
/// - `interrupt_cancel_queued_v1`：中断时连排队消息一起取消。P0 不排队多条消息，无对应能力位。
/// - `msg_lifecycle_v1`：消息生命周期事件。未采到它对应的事件形态，不敢据此点亮任何位。
pub const CAP_OBSERVED_UNMAPPED: &[&str] = &["interrupt_cancel_queued_v1", "msg_lifecycle_v1"];

/// 上游 `error` 字段里表示「未登录 / 凭据过期」的机器可读串（实测值）。
const ERROR_AUTH_FAILED: &str = "authentication_failed";

/// CLI 给「非模型产出的合成消息」打的**哨兵模型名**（样本 A 第 8 行实测
/// `message.model = "<synthetic>"`，那一行正是认证失败播报）。
///
/// **它不是一个模型名**，绝不能写进 [`ClaudeDecoder::last_message_model`]：写进去之后
/// [`ClaudeDecoder::pick_model_window`] 的第 1 顺位必然查不到，第 2 顺位查 `init` 报的模型名
/// （实测 `"claude-opus-5[1m]"`，与 `modelUsage` 的 key `"claude-opus-5"` **不是同一个串**）
/// 也查不到，第 3 顺位又因 `map.len() > 1`（主模型 + 子代理模型是 agentic 会话常态）放弃 ⇒
/// `context_limit` / `max_output_tokens` 落 `(0, 0)`，手机端进度条整个消失。
/// 那不是「保守地报未知」，是被一个**已知的哨兵值**污染出来的假未知。
const MODEL_SYNTHETIC: &str = "<synthetic>";

/// 标识类字段（`tool_use.id` / `tool_use.name` / `control_response.request_id`）的字节上限。
///
/// 这几个字段**上游可控且此前完全不夹紧**，而单行上限是 `MAX_LINE_BYTES` = 32 MiB：
/// 一行畸形/恶意上游行就能造出一个 `name` 长达数十 MB 的 `ToolUse` 块，片 4 把它包成
/// `LlmFrame::Delta` 后直接顶穿中继 4 MiB 单帧上限——`llm.rs` 把这个失败模式
/// （中继断整条 WS / QUIC 静默丢帧）明确点名为要防的东西。
///
/// 256 B 的依据：实测 `toolu_` 前缀的 id 是 29 字节、工具名最长 `TodoWrite` 9 字节，
/// 留了近一个数量级的余量。**两端一致地夹紧**，所以 `ToolUse.call_id` 与 `ToolResult.call_id`
/// 即使真的越限也仍然配得上对。
const IDENT_MAX_BYTES: usize = 256;

/// 按 [`IDENT_MAX_BYTES`] 夹紧一个标识串（落在 UTF-8 字符边界上）。
fn clamp_ident(id: &str) -> String {
    id[..floor_char_boundary(id, IDENT_MAX_BYTES)].to_owned()
}

// ── 反序列化即夹紧的文本 ─────────────────────────────────────────────────────

/// **反序列化那一刻就按 `MAX` 字节夹紧**的字符串。
///
/// # 为什么不是「先收 `String` 再 `truncate`」
/// `tool_result.content` 实测 12 272 字符，一次大文件 `Read` 可到 MB 级。先收后截意味着那份完整
/// 正文要在本端结构里活一整个函数——期间任何一条 `log::debug!("{line:?}")` 或一次顺手的 `clone`
/// 都会把用户源码正文带进日志/内存快照。本类型把「只留夹紧后那份」下沉到 `Deserialize`：
/// 上游正文只在 serde_json 的临时 scratch 缓冲里出现一次，**从不进入任何长寿命对象**。
///
/// # 夹紧必须落在 UTF-8 字符边界上
/// 裸 `&s[..MAX]` 对多字节字符直接 panic（`byte index is not a char boundary`）。上游正文里有中文
/// 是常态（这是个中文项目），所以这不是理论风险，[`floor_char_boundary`] 就是为它存在的。
///
/// # `dropped` 不是装饰
/// 它是 [`LlmBlock::ToolResult::truncated_bytes`] 的数据源。协议侧把「截断必须可声明」写成了硬
/// 要求：没有这个数字，被控端面对超长正文只剩「发出去炸链路」和「静默丢整块」两条路，后者会让
/// 工具卡片凭空消失、后续 `call_id` 配不上对。
#[derive(Clone, PartialEq, Eq)]
pub struct Clamped<const MAX: usize> {
    kept: String,
    dropped: u64,
}

impl<const MAX: usize> Clamped<MAX> {
    /// 从已有字符串夹紧（测试与非 serde 路径用）。
    #[must_use]
    pub fn clamp(text: &str) -> Self {
        let end = floor_char_boundary(text, MAX);
        Self {
            kept: text[..end].to_owned(),
            dropped: (text.len() - end) as u64,
        }
    }

    /// 就地夹紧一个已经拥有所有权的 `String`（省一次拷贝）。
    fn clamp_owned(mut text: String) -> Self {
        let end = floor_char_boundary(&text, MAX);
        let dropped = (text.len() - end) as u64;
        text.truncate(end);
        Self {
            kept: text,
            dropped,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.kept
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// 被裁掉的字节数；`None` = 一个字节都没裁（可直接填协议的 `truncated_bytes`）。
    #[must_use]
    pub fn truncated_bytes(&self) -> Option<u64> {
        (self.dropped > 0).then_some(self.dropped)
    }

    /// 转成协议文本（消费自身，避免再拷一次）。
    #[must_use]
    pub fn into_text(self) -> LlmText {
        LlmText::new(self.kept)
    }
}

impl<const MAX: usize> Default for Clamped<MAX> {
    fn default() -> Self {
        Self {
            kept: String::new(),
            dropped: 0,
        }
    }
}

/// **只打长度、不打正文**——本类型装的正是用户的文件正文与工具输出。
impl<const MAX: usize> std::fmt::Debug for Clamped<MAX> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Clamped")
            .field("kept", &self.kept.len())
            .field("dropped", &self.dropped)
            .finish()
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for Clamped<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Visit<const MAX: usize>;
        impl<const MAX: usize> serde::de::Visitor<'_> for Visit<MAX> {
            type Value = Clamped<MAX>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("字符串")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Clamped::clamp(v))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(Clamped::clamp_owned(v))
            }
        }
        // 刻意**不**实现 `visit_seq` / `visit_map`：公开 Messages API 允许 `content` 是块数组，
        // 但这个 CLI 实测给的是纯字符串。真出现数组形态时应当是一条**响亮的** `DecodeFailed`
        // （§14-6 禁止无声降级），而不是悄悄产出一个空 output 的工具卡片。
        de.deserialize_string(Visit::<MAX>)
    }
}

/// **上游形状未知**时的夹紧文本：是字符串就夹紧，不是字符串就排空。
///
/// # 这个类型存在的理由（对抗验证抓到的一族缺陷）
/// [`ContentBlock`] 是 `assistant` 与 `user` 两种行**共用**的扁平联合结构，`content` 键被硬类型成
/// [`Clamped`]（只接受 JSON 字符串）。而 serde 的 map 值反序列化失败是**整行级**的：只要同一行里
/// 任何一个块的 `content` 不是字符串，`parse_line` 就整条返回
/// [`DecodeErrorKind::Schema`]，**同行的模型正文与 `tool_use` 块一起消失**。
///
/// 这直接违反 `adapter.rs` 写死的 trait 契约「一行里有 3 个 content block、第 2 个解不动时，
/// 正确做法是把第 1、3 个照常产出」，也与本文件「未知块类型静默跳过」自相矛盾——未知块类型只有
/// 在**不带 `content` 键**时才真的被跳过。
///
/// 触发面不是理论的：`web_search_tool_result` / `code_execution_tool_result` / `mcp_tool_result` /
/// `document` / `search_result` 这些块的 `content` 都是**数组**，而两份样本的
/// `usage.server_tool_use.web_search_requests` 键都存在，证明这个 CLI 构建里 web search 通道
/// 是开着的；`tool_result.content` 的数组形态也是公开 API 的合法形状。
///
/// # 仍然**不缓冲**
/// 非字符串形态用 [`serde::de::IgnoredAny`] 逐元素排空：不物化、不建树。
/// 这与模块文档「被否决的替代方案 1」（`untagged` / 内部标签会把整个 map 缓冲进 `Content`）
/// 划清界限——那条禁令依然有效，本类型没有破它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaybeClamped<const MAX: usize> {
    /// 上游给的是字符串，已按 `MAX` 夹紧。
    Text(Clamped<MAX>),
    /// 上游给的是数组 / 对象 / 数字……**已排空、零物化**。
    ///
    /// 调用方必须自己决定这是「静默跳过」还是「响亮报错」：`assistant` 侧的未知块静默跳过，
    /// `user` 侧的 `tool_result` 必须响亮（那是手机端工具卡片的正文）。
    NonText,
}

impl<'de, const MAX: usize> Deserialize<'de> for MaybeClamped<MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Visit<const MAX: usize>;
        impl<'de, const MAX: usize> serde::de::Visitor<'de> for Visit<MAX> {
            type Value = MaybeClamped<MAX>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("字符串（其它形态会被排空）")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(MaybeClamped::Text(Clamped::clamp(v)))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(MaybeClamped::Text(Clamped::clamp_owned(v)))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                // 逐元素排空：`IgnoredAny` 只走词法、不建任何值。
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(MaybeClamped::NonText)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(MaybeClamped::NonText)
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(MaybeClamped::NonText)
            }

            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(MaybeClamped::NonText)
            }

            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(MaybeClamped::NonText)
            }

            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(MaybeClamped::NonText)
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(MaybeClamped::NonText)
            }
        }
        de.deserialize_any(Visit::<MAX>)
    }
}

impl<const MAX: usize> MaybeClamped<MAX> {
    /// 是字符串就取出夹紧后的那份，否则给一个空的。第二个返回值 = 「上游给了、但不是字符串」。
    ///
    /// **键缺席与「给了个数组」必须分开**：前者是常态（`text` 块没有 `content` 键），
    /// 后者是我们没建模的新形状，得让它响亮。
    fn into_text_or_empty(this: Option<Self>) -> (Clamped<MAX>, bool) {
        match this {
            None => (Clamped::default(), false),
            Some(Self::Text(text)) => (text, false),
            Some(Self::NonText) => (Clamped::default(), true),
        }
    }
}

/// 「是 JSON 对象就按 `T` 解，其它形态一律排空并给 `None`」。
///
/// # 为什么 `tool_use_result` 必须走它
/// `UserLine.tool_use_result` 初稿是 `Option<ToolUseResult<'a>>`，而 `ToolUseResult` 是 derive 出来
/// 的结构体 ⇒ serde 走 `deserialize_struct`，遇到**非对象**取值（字符串 / 数组 / 数字）直接 `Err`，
/// **整条 `user` 行被丢**。后果比丢一个摘要严重得多：那一行里的 `tool_result` 块根本不会产出，
/// 手机端的工具卡片永远等不到结果，该 `call_id` 从此成为孤儿——协议侧 `LlmBlock::ToolUse` 文档
/// 把「卡片凭空消失 + `call_id` 配不上对」点名为**禁止**的无声降级。
///
/// 而本文件自己就写着「`Bash`/`Edit`/`Write` 的 `tool_use_result` 形态完全未知」：**既然形态未知，
/// 就不能把它硬绑成一个具体的 struct 形状**。
///
/// `file` 字段同样走它（同一根因：`file` 若是字符串，内层 derive 照样会打死整行）。
fn opt_object<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visit<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visit<T> {
        type Value = Option<T>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("JSON 对象（其它形态会被排空）")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            // 交回给 `T` 的 derive 实现：`file.content` 仍由 `ToolResultFile` 的字段白名单挡掉，
            // 一个字节都不物化。
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(Some)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }
    de.deserialize_any(Visit(std::marker::PhantomData))
}

/// `message.content` 的两种合法形状：块数组（实测）与**纯字符串**（公开 Messages API 合法）。
///
/// 初稿硬绑 `Vec<ContentBlock>`，上游给 `"content":"纯字符串"` 时整行全灭。这里把字符串形态
/// 归一成「一个 `text` 块」——那正是它在语义上的意思，不是编造。其它形态（对象 / 数字）排空成
/// 空表：`assistant` 行本来就允许空 content。
fn content_blocks<'de, 'a, D>(de: D) -> Result<Vec<ContentBlock<'a>>, D::Error>
where
    D: serde::Deserializer<'de>,
    'de: 'a,
{
    fn text_block<'a>(text: Cow<'a, str>) -> Vec<ContentBlock<'a>> {
        vec![ContentBlock {
            ty: Some(Cow::Borrowed("text")),
            text: Some(text),
            ..ContentBlock::default()
        }]
    }

    struct Visit<'a>(std::marker::PhantomData<&'a ()>);
    impl<'de: 'a, 'a> serde::de::Visitor<'de> for Visit<'a> {
        type Value = Vec<ContentBlock<'a>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("content 块数组或纯字符串")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(block) = seq.next_element::<ContentBlock<'a>>()? {
                out.push(block);
            }
            Ok(out)
        }

        fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
            Ok(text_block(Cow::Borrowed(v)))
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(text_block(Cow::Owned(v.to_owned())))
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(text_block(Cow::Owned(v)))
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(Vec::new())
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }
    de.deserialize_any(Visit(std::marker::PhantomData))
}

/// 在 **UTF-8 字符边界**上求「不超过 `max` 字节」的最大下标。
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

// ── 入站线格式（**只声明白名单内的字段**）────────────────────────────────────

/// `system/init` 行。同一行实测 22 个 key，这里只声明 5 个。
///
/// **刻意不声明**：`tools`（40 个工具名）/ `skills` / `plugins` / `mcp_servers` /
/// `slash_commands` / `agents` / `memory_paths`（绝对路径）/ `apiKeySource` / `output_style` /
/// `analytics_disabled` / `product_feedback_disabled` / `fast_mode_*`——一律本机环境信息。
#[derive(Debug, Deserialize)]
struct InitLine<'a> {
    #[serde(default, borrow)]
    cwd: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    model: Option<Cow<'a, str>>,
    #[serde(default, rename = "permissionMode", borrow)]
    permission_mode: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    claude_code_version: Option<Cow<'a, str>>,
    #[serde(default)]
    capabilities: Option<CapabilityField>,
}

/// `capabilities` 的**两层容错**包装。
///
/// 能力探测是 `init` 唯一不可替代的用途，而 `init` 解析失败的后果是 [`RunnerEvent::Started`]
/// 永远发不出来、runner 卡在 `Booting`。所以这个字段的形状变更**不许打死整条 init**：
/// 外层不是数组就整个吃掉，内层元素不是字符串就跳过那一个。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CapabilityField {
    List(Vec<CapabilityToken>),
    Other(serde::de::IgnoredAny),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CapabilityToken {
    Name(String),
    Other(serde::de::IgnoredAny),
}

/// `system/status` 行。⚠ **两份样本里一行都没有**，键名按 `init` 的 `permissionMode` 推定，
/// 同时接受 `permission_mode` 蛇形写法。补样之前这整个分支算未验证代码。
#[derive(Debug, Deserialize)]
struct StatusLine<'a> {
    #[serde(default, rename = "permissionMode", alias = "permission_mode", borrow)]
    permission_mode: Option<Cow<'a, str>>,
}

/// `assistant` 行。
///
/// **刻意不声明**：`request_id`（上游账号侧标识，`llm.rs` 的 [`LlmTurnOutcome`] 文档逐条说明了
/// 为什么它「有价值但价值不在手机那一端」）、`timestamp`、`message.{id,container,stop_details,
/// stop_sequence,context_management,diagnostics,service_tier}`。
///
/// `message.stop_reason` 也不声明：实测**流式中途是显式 `null`**，把它当停止原因上报会让手机
/// 每收一条中间消息就闪一次「已停止」。真正的停止原因只看 `result` 行。
#[derive(Debug, Deserialize)]
struct AssistantLine<'a> {
    #[serde(borrow)]
    message: AssistantMessage<'a>,
    #[serde(default, borrow)]
    parent_tool_use_id: Option<Cow<'a, str>>,
    /// 机器可读错误标识（实测 `"authentication_failed"`）。只在 `is_api_error_message` 为真时有意义。
    #[serde(default, borrow)]
    error: Option<Cow<'a, str>>,
    /// **铁律二的唯一识别位**。缺省 `false`（正常消息不带这个键）。
    #[serde(default)]
    is_api_error_message: bool,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage<'a> {
    /// 本条消息实际用的模型（实测 `"claude-opus-5"`，与 `init.model` 的
    /// `"claude-opus-5[1m]"` **不是同一个串**）。只用来在 `result.modelUsage` 里挑对条目，
    /// **不上行**（对话的模型名由 `init` 那一份经 `LlmConvMeta` 给出）。
    #[serde(default, borrow)]
    model: Option<Cow<'a, str>>,
    /// 见 [`content_blocks`]：块数组与纯字符串两种形状都收。
    #[serde(default, borrow, deserialize_with = "content_blocks")]
    content: Vec<ContentBlock<'a>>,
    #[serde(default)]
    usage: Option<PromptUsage>,
}

/// 一次 API 调用**真实送进模型的提示词 token 数**的三个来源。
///
/// 三者之和就是 [`LlmUsage::context_used`]（HUD 进度条的分子）。**只能取本轮最后一次调用，
/// 不能累加**：`result.usage` 是整轮汇总，一轮 10 次工具调用累加出来的 `input_tokens` 是同一份
/// 上下文被数了 10 遍，照它画进度条会超过 100%。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct PromptUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

impl PromptUsage {
    fn prompt_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }
}

/// **扁平联合**的 content block：一个结构体覆盖 `text` / `thinking` / `tool_use` / `tool_result`
/// 四种块，按 [`Self::ty`] 分派。
///
/// 为什么不是 `#[serde(tag = "type")]` 枚举——见模块文档「被否决的替代方案 1」（内部标签会缓冲
/// 整个 map，[`Clamped`] 的反序列化即夹紧当场失效）。
///
/// **刻意不声明**：`signature`（456 字符不透明 base64，对手机零用途）、`caller`
/// （实测 `{"type":"direct"}`，公开 API 没有的 Claude Code 扩展、语义未知、且是**对象**）。
#[derive(Debug, Default, Deserialize)]
struct ContentBlock<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Option<Cow<'a, str>>,
    /// `text` 块正文。
    #[serde(default, borrow)]
    text: Option<Cow<'a, str>>,
    /// `thinking` 块正文。**Claude 实测恒为空串**。
    #[serde(default, borrow)]
    thinking: Option<Cow<'a, str>>,
    /// `tool_use.id`（实测 `"toolu_019Vk9BtrSBpc2dtv9YBPTY3"`）。
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    /// `tool_use.name`（实测 `"Read"`）。
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    /// `tool_use.input`。**协议里唯一允许的 `serde_json::Value` 口子**，按
    /// [`LLM_TOOL_INPUT_MAX_BYTES`] 夹紧后上行。
    #[serde(default)]
    input: Option<serde_json::Value>,
    /// `tool_result.tool_use_id`。
    #[serde(default, borrow)]
    tool_use_id: Option<Cow<'a, str>>,
    /// `tool_result.content`（实测**纯字符串**，12 272 字符）。反序列化即夹紧。
    ///
    /// 类型是 [`MaybeClamped`] 而不是 [`Clamped`]：这个键在 `web_search_tool_result` 等块上
    /// 是**数组**，硬绑字符串会让整行连同同行的正文块一起消失（见该类型文档）。
    #[serde(default)]
    content: Option<MaybeClamped<LLM_TOOL_OUTPUT_MAX_BYTES>>,
    /// `tool_result.is_error`。**实测「键不存在」而不是 `false`** ⇒ 必须可缺省。
    #[serde(default)]
    is_error: Option<bool>,
}

/// `user` 行。
#[derive(Debug, Deserialize)]
struct UserLine<'a> {
    #[serde(borrow)]
    message: UserMessage<'a>,
    #[serde(default, borrow)]
    parent_tool_use_id: Option<Cow<'a, str>>,
    /// **Claude Code 扩展，公开 Messages API 没有**（样本 B 第 9 行）。
    ///
    /// 走 [`opt_object`]：形态完全未知的字段**不能**硬绑成一个具体的 struct 形状，
    /// 否则「上游给了个字符串」= 整条 `user` 行连同 `tool_result` 块一起消失。
    #[serde(default, borrow, deserialize_with = "opt_object")]
    tool_use_result: Option<ToolUseResult<'a>>,
    /// ⚠ 未实测（需 `--replay-user-messages`，P0 不传该开关）。
    #[serde(default, rename = "isReplay")]
    is_replay: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UserMessage<'a> {
    /// 见 [`content_blocks`]（与 [`AssistantMessage::content`] 同一套容错）。
    #[serde(default, borrow, deserialize_with = "content_blocks")]
    content: Vec<ContentBlock<'a>>,
}

/// `user` 行顶层的 `tool_use_result`。
///
/// **`type` 刻意不作判别依据**：只观测到 `"text"` 一个值，取值域完全未知，拿一个只见过一次的值
/// 当 discriminant 是把「样本 = 全集」这个错误写进代码。真正的判别依据是 `file` 键在不在。
#[derive(Debug, Deserialize)]
struct ToolUseResult<'a> {
    /// 同样走 [`opt_object`]：`file` 若不是对象，内层 derive 一样会打死整行。
    #[serde(default, borrow, deserialize_with = "opt_object")]
    file: Option<ToolResultFile<'a>>,
}

/// `tool_use_result.file`——**只取元数据，绝不取 `content`**。
///
/// 实测 `file.content` 是 11 720 字符的**用户文件正文**。本结构体里**根本没有 `content` 字段**，
/// serde 扫过去直接丢弃、不分配。这就是「白名单在类型层执行」最直白的一处：
/// 「忘了过滤 `content`」这件事在这里写不出来。
#[derive(Debug, Deserialize)]
struct ToolResultFile<'a> {
    #[serde(default, rename = "filePath", borrow)]
    file_path: Option<Cow<'a, str>>,
    #[serde(default, rename = "numLines")]
    num_lines: Option<u32>,
    #[serde(default, rename = "startLine")]
    start_line: Option<u32>,
    #[serde(default, rename = "totalLines")]
    total_lines: Option<u32>,
}

/// 顶层 `rate_limit_event` 行（样本 B 第 10 行）。
#[derive(Debug, Deserialize)]
struct RateLimitLine<'a> {
    #[serde(borrow)]
    rate_limit_info: RateLimitInfo<'a>,
}

#[derive(Debug, Deserialize)]
struct RateLimitInfo<'a> {
    #[serde(default, borrow)]
    status: Option<Cow<'a, str>>,
    /// **Unix 秒**（实测 `1786211400`），不是毫秒。
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(default, rename = "rateLimitType", borrow)]
    rate_limit_type: Option<Cow<'a, str>>,
    #[serde(default, rename = "overageStatus", borrow)]
    overage_status: Option<Cow<'a, str>>,
    #[serde(default, rename = "overageDisabledReason", borrow)]
    overage_disabled_reason: Option<Cow<'a, str>>,
    #[serde(default, rename = "isUsingOverage")]
    is_using_overage: Option<bool>,
}

/// `result` 行。
///
/// **刻意不声明 `subtype`**：两次采样都是 `"success"`、成败相反——一个判据价值为零的字段，
/// 不解析比「解析了但不用」更安全（不留任何被后人误用的入口）。
///
/// 同样不声明：`ttft_ms` / `ttft_stream_ms` / `time_to_request_ms`（性能画像）、
/// `num_turns` / `duration_ms` / `duration_api_ms`（P0 无承载点，手机端有
/// `started_ms`/`ended_ms` 可算墙钟）、`usage.{service_tier,inference_geo,iterations,speed,
/// server_tool_use,cache_creation}`、`fast_mode_*`。
#[derive(Debug, Deserialize)]
struct ResultLine<'a> {
    /// **唯一权威的成败判定**。
    #[serde(default)]
    is_error: bool,
    #[serde(default, borrow)]
    stop_reason: Option<Cow<'a, str>>,
    /// 实测两个值：`"completed"`（成功）/ `"api_error"`（失败）。**不是成败判据**。
    #[serde(default, borrow)]
    terminal_reason: Option<Cow<'a, str>>,
    /// HTTP 状态码（实测 403）。
    #[serde(default)]
    api_error_status: Option<u16>,
    /// 实测 `0.24026150000000002`——**IEEE 754 双精度且自带表示误差**，不是定点小数。
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    usage: Option<ResultUsage>,
    /// 按模型名做 key 的 map。**只摘 `contextWindow` / `maxOutputTokens` 两个数字**，
    /// `canonicalModel` / `provider` / `costUSD` / `webSearchRequests` 等一律不出 PC（§5.0-4）。
    #[serde(default, rename = "modelUsage")]
    model_usage: Option<BTreeMap<String, ModelUsageEntry>>,
    /// **元素结构未实测** ⇒ 用 [`serde::de::IgnoredAny`] 只数条数，元素内容一个字节都不物化。
    #[serde(default)]
    permission_denials: Option<Vec<serde::de::IgnoredAny>>,
    /// 失败路径上是错误摘要、成功路径上是整段回答。夹紧后上行。
    #[serde(default)]
    result: Option<Clamped<LLM_TOOL_OUTPUT_MAX_BYTES>>,
}

/// `result.usage`——**整轮累加**的账务口径（与 [`PromptUsage`] 的「最后一次」口径不同，
/// 两个口径在 [`LlmUsage`] 里刻意并存）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct ResultUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ModelUsageEntry {
    #[serde(default, rename = "contextWindow")]
    context_window: Option<u64>,
    #[serde(default, rename = "maxOutputTokens")]
    max_output_tokens: Option<u64>,
}

/// `control_response` 行。
///
/// ⚠ **全表唯一「有实测、无样本行」的项**：三次手工 round-trip（`initialize` /
/// `set_permission_mode` / `interrupt`）都成功，但没留下可对照的原始行，**嵌套形状是推的**。
/// 故这里同时接受两种放法（嵌套在 `response` 里 / 直接在顶层），两处都取不到 `request_id`
/// 时**响亮地报 [`DecodeErrorKind::Unmodeled`]**，而不是猜一个 id 编出一条 ack。
#[derive(Debug, Deserialize)]
struct ControlResponseLine<'a> {
    #[serde(default, borrow)]
    response: Option<ControlResponseBody<'a>>,
    #[serde(default, borrow)]
    request_id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    subtype: Option<Cow<'a, str>>,
}

#[derive(Debug, Deserialize)]
struct ControlResponseBody<'a> {
    #[serde(default, borrow)]
    request_id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    subtype: Option<Cow<'a, str>>,
}

// ── 出站线格式（§6.4：只经 serde，绝不手拼）─────────────────────────────────

/// 一条用户消息。形状与 CLI **自己回吐**的 `user` 行一致（样本 B 第 9 行的
/// `message.content[]` 就是数组形态），不是照公开文档抄的。
#[derive(Debug, Serialize)]
struct OutUserLine<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    message: OutUserMessage<'a>,
}

#[derive(Debug, Serialize)]
struct OutUserMessage<'a> {
    role: &'static str,
    content: [OutTextBlock<'a>; 1],
}

#[derive(Debug, Serialize)]
struct OutTextBlock<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    text: &'a str,
}

/// 一条 control 请求。
#[derive(Debug, Serialize)]
struct OutControlRequest<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    request_id: &'a str,
    request: OutControlBody<'a>,
}

#[derive(Debug, Serialize)]
struct OutControlBody<'a> {
    subtype: &'static str,
    /// 只有 `set_permission_mode` 用。
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'a str>,
}

// ── 上游取值 → 归一化枚举的映射表 ───────────────────────────────────────────
//
// **每张表都必须先查、查不到才 `from_wire_clamped`**（`open_enum!` 文档点名的姿势）：
// 本协议的线格式是自己的 `PascalCase`，而上游是 `snake_case` / `camelCase`。直接拿上游原文喂
// `from_wire_clamped` 会让一个本端明明认识的值静默落进 `Other`，UI 从此走「未知值」分支——
// 不报错、只长期失真。

/// `permissionMode`（上游 camelCase）→ 归一化。
///
/// 只有 `"acceptEdits"` 是实测值（两份样本的 `init` 都是它）；其余三行来自 `--permission-mode`
/// 的取值集，标 ⚠。认不出的一律 `Other(原文)`——**不映射到任何已知值**，宁可 UI 显示原文。
fn permission_mode_from_wire(wire: &str) -> LlmPermissionMode {
    match wire {
        "acceptEdits" => LlmPermissionMode::AcceptEdits, // ✅ 实测
        "default" => LlmPermissionMode::Manual,          // ⚠ 推定
        "plan" => LlmPermissionMode::Plan,               // ⚠ 推定
        "bypassPermissions" => LlmPermissionMode::Bypass, // ⚠ 推定（只读方向；写方向永不产出）
        other => LlmPermissionMode::from_wire_clamped(other),
    }
}

/// 归一化 → `--permission-mode` / `set_permission_mode` 的上游取值。
///
/// # 为什么四种情况都拒
/// - [`LlmPermissionModeKnown::Bypass`]：P0 桌面端与手机端都不提供这个开关（§4.4 / §6.8.4）。
///   [`LaunchSpec::set_permission_mode`] 已经在类型层挡了一道，这里是第二道——`ControlCmd`
///   是另一条入口，只挡一条就是半道门。
/// - `Auto` / `DontAsk`：Claude 没有对应模式。**编一个最近似的映射比拒绝危险得多**——
///   把 `DontAsk` 映射成 `acceptEdits` 等于替用户放宽了权限。
/// - `Other(_)`：认不出的权限模式在安全上只能保守解释，原样转给 CLI 等于让上游替我们决定放行范围。
fn permission_mode_to_wire(mode: &LlmPermissionMode) -> Result<&'static str, EncodeError> {
    match mode {
        LlmPermissionMode::Known(LlmPermissionModeKnown::AcceptEdits) => Ok("acceptEdits"),
        LlmPermissionMode::Known(LlmPermissionModeKnown::Manual) => Ok("default"),
        LlmPermissionMode::Known(LlmPermissionModeKnown::Plan) => Ok("plan"),
        _ => Err(EncodeError::Unsupported),
    }
}

/// `result.stop_reason` → [`LlmStopReason`]。
///
/// 实测样本 A 是 `"stop_sequence"`——**已知集合里没有它**，故落 `Other("stop_sequence")`。
/// 这正是开放枚举存在的理由：不认识的值原样保留，而不是硬塞进一个语义不符的已知变体。
fn stop_reason_from_wire(wire: &str) -> LlmStopReason {
    match wire {
        "end_turn" => LlmStopReason::EndTurn,
        "max_tokens" => LlmStopReason::MaxTokens,
        "tool_use" => LlmStopReason::ToolUse,
        "refusal" => LlmStopReason::Refusal,
        other => LlmStopReason::from_wire_clamped(other),
    }
}

/// `result.terminal_reason` → [`LlmTerminalReason`]。两个值都是实测的。
fn terminal_reason_from_wire(wire: &str) -> LlmTerminalReason {
    match wire {
        "completed" => LlmTerminalReason::Completed,
        "api_error" => LlmTerminalReason::ApiError,
        other => LlmTerminalReason::from_wire_clamped(other),
    }
}

/// `assistant.error` → [`LlmErrorCode`]。
///
/// **这张表刻意只有一行**：只有 `"authentication_failed"` 是实测值。多写一行
/// `"rate_limit_error" => RateLimited` 看着无害，实则是把猜测写成了映射——猜错时手机端的提示
/// 文案会长期指向错误的排障方向，而且不会有任何报错。认不出的落 `Other(原文)`，UI 至少能原样显示。
fn error_code_from_wire(wire: &str) -> LlmErrorCode {
    match wire {
        ERROR_AUTH_FAILED => LlmErrorCode::AuthRequired,
        other => LlmErrorCode::from_wire_clamped(other),
    }
}

fn rate_limit_status_from_wire(wire: &str) -> LlmRateLimitStatus {
    match wire {
        "allowed" => LlmRateLimitStatus::Allowed,
        other => LlmRateLimitStatus::from_wire_clamped(other),
    }
}

fn rate_limit_window_from_wire(wire: &str) -> LlmRateLimitWindow {
    match wire {
        "five_hour" => LlmRateLimitWindow::FiveHour,
        other => LlmRateLimitWindow::from_wire_clamped(other),
    }
}

fn overage_status_from_wire(wire: &str) -> LlmOverageStatus {
    match wire {
        "rejected" => LlmOverageStatus::Rejected,
        other => LlmOverageStatus::from_wire_clamped(other),
    }
}

fn overage_reason_from_wire(wire: &str) -> LlmOverageDisabledReason {
    match wire {
        "out_of_credits" => LlmOverageDisabledReason::OutOfCredits,
        other => LlmOverageDisabledReason::from_wire_clamped(other),
    }
}

/// `total_cost_usd`（f64）→ **微美元整数**，**向上取整**。
///
/// 协议侧写死了这条：宁可多报 1e-6 美元也不虚报 0，更不要 `as u64` 直接截断（那会把 0.9 µUSD
/// 的一轮显示成免费）。实测 `0.24026150000000002` → `240262`。
///
/// `NaN` / 负数 / 非有限值一律按 `0`（= 未知）：那是上游给了垃圾值，编一个数出来更糟。
fn cost_micro_usd(cost: Option<f64>) -> u64 {
    let Some(cost) = cost else { return 0 };
    if !cost.is_finite() || cost <= 0.0 {
        return 0;
    }
    let micros = (cost * 1e6).ceil();
    if micros >= 18_446_744_073_709_551_615.0 {
        u64::MAX
    } else {
        micros as u64
    }
}

/// `resetsAt` 的清洗：**Unix 秒**，非正值与「换算成毫秒会溢出」的值一律按 `None`。
///
/// 协议侧对消费端写了「必须 `checked_mul(1_000)`」；在**生产端**先过滤一次是更早的那道门——
/// 一个 `i64::MAX` 的 `resetsAt` 传下去，每一个消费者都得记得自己防一遍。
fn sanitize_resets_at(secs: Option<i64>) -> Option<i64> {
    let secs = secs?;
    if secs <= 0 {
        return None;
    }
    secs.checked_mul(1_000).map(|_| secs)
}

/// 工具入参夹紧：**保留块、只裁值**。
///
/// 协议侧把处置写成了硬要求（[`LLM_TOOL_INPUT_MAX_BYTES`] 的文档）：保留 `call_id` / `name`，
/// 只把 `input` 换成裁剪后的摘要，并把裁掉的字节数填进 `truncated_bytes`——**不得丢掉整个块**。
/// 丢块会让工具卡片凭空消失、后续 `ToolResult` 的 `call_id` 配不上对。
///
/// 策略：对象形态按「值最大的键优先」逐个换成长度占位，直到进预算；非对象形态（数组 / 裸串）
/// 或换完仍超预算时整体清空，`truncated_bytes` 记原始长度。
///
/// **长度用 `serde_json::to_vec` 精确量，不用估算**：这是预算/白名单机制，估算值偏小就会让
/// 超预算的入参溜过去顶穿单帧上限。代价是超预算路径上多一次序列化，而这条路径本来就是异常路径。
fn clamp_tool_input(mut value: serde_json::Value) -> (LlmToolInput, Option<u64>) {
    let original = json_len(&value);
    if original <= LLM_TOOL_INPUT_MAX_BYTES {
        return (LlmToolInput::new(value), None);
    }
    if let serde_json::Value::Object(map) = &mut value {
        let mut by_size: Vec<(String, usize)> = map
            .iter()
            .map(|(key, val)| (key.clone(), json_len(val)))
            .collect();
        by_size.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let mut current = original;
        for (key, len) in by_size {
            if current <= LLM_TOOL_INPUT_MAX_BYTES {
                break;
            }
            // 占位符是**我们自己造的短串**，不含上游任何内容。
            let placeholder = serde_json::Value::String(format!("<<{len} bytes clamped>>"));
            let placeholder_len = json_len(&placeholder);
            map.insert(key, placeholder);
            current = current.saturating_sub(len).saturating_add(placeholder_len);
        }
        if current <= LLM_TOOL_INPUT_MAX_BYTES {
            return (
                LlmToolInput::new(value),
                Some((original.saturating_sub(current)) as u64),
            );
        }
    }
    (
        LlmToolInput::new(serde_json::Value::Object(serde_json::Map::new())),
        Some(original as u64),
    )
}

/// 一个 JSON 值序列化后的**精确**字节数。
fn json_len(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

/// 解析一整行到某个入站结构体。
///
/// **绝不保留 serde 的错误消息**：`serde_json::Error` 的 `Display` 在类型不匹配时会把**实际值**
/// 写进消息（`invalid type: string "sk-ant-…", expected u64`），而错误会进 `log::warn` 与审计
/// 日志——那正好绕开整套白名单。这里只留代码里写死的站点名。
fn parse_line<'a, T: Deserialize<'a>>(raw: &'a str, at: &'static str) -> Result<T, DecodeError> {
    serde_json::from_str::<T>(raw).map_err(|_| DecodeError {
        at,
        kind: DecodeErrorKind::Schema,
    })
}

// ── 适配器：工厂 + 静态描述 ──────────────────────────────────────────────────

/// Claude Code 的适配器。**无状态**（只描述「这个 CLI 长什么样」），每起一个会话调一次
/// [`LlmAgentAdapter::split`] 产出该会话专属的编解码两半。
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeAdapter;

impl ClaudeAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl LlmAgentAdapter for ClaudeAdapter {
    fn kind(&self) -> LlmAgentKind {
        LlmAgentKind::Claude
    }

    fn program(&self) -> &'static str {
        CLAUDE_PROGRAM
    }

    /// 命令行参数。
    ///
    /// # 与设计骨架（§6.9）的两处偏离，都是为了不拿整个功能去赌未验证的开关
    /// 1. **不传 `--replay-user-messages`**：该开关本身未在样本上验证过，而**未知命令行参数是
    ///    启动即失败**（与「未知 stdin `type` 被静默忽略」完全不同的失败模式）。它换来的
    ///    `isReplay` 送达回执，手机端本来就知道自己发了什么。要打开只需在这里加两行 + 打开
    ///    [`ClaudeDecoder::decode_user`] 里那条已经写好的 `is_replay` 分支。
    /// 2. **不硬编码 `--disallowedTools`**：规则串语法未验证，写错的规则匹配不到任何东西，
    ///    得到一道看着存在实则不设防的围栏。P0 围栏 = `--add-dir` + `--permission-mode`；
    ///    黑名单等语法被验证后由调用方经 `LaunchSpec` 传。
    ///
    /// `--verbose` 不是可选项：`-p` + `--output-format stream-json` 下缺它拿不到完整事件流。
    fn build_args(&self, spec: &LaunchSpec) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
        ];

        // `--resume` 与 `--session-id` 互斥：前者接续已有会话，后者给新会话钉一个我们生成的 id。
        // ⚠ `--resume` 在 `--input-format stream-json` 下的语义**未实测**（§12-④）。
        if spec.resume {
            args.push("--resume".into());
            args.push(spec.cli_session_id.clone());
            if spec.fork_session {
                args.push("--fork-session".into());
            }
        } else {
            args.push("--session-id".into());
            args.push(spec.cli_session_id.clone());
        }

        // 权限围栏：把可访问范围钉死在用户在桌面选定的工作目录。
        args.push("--add-dir".into());
        args.push(spec.workspace.display().to_string());

        if let Some(mode) = spec.permission_mode() {
            match permission_mode_to_wire(mode) {
                Ok(wire) => {
                    args.push("--permission-mode".into());
                    args.push(wire.into());
                }
                // 走不到（`LaunchSpec::set_permission_mode` 已在类型层挡掉 Bypass 与未知值），
                // 但**宁可用 CLI 的默认模式，也不能把认不出的模式原样传下去**。
                Err(_) => log::warn!("claude: 权限模式无法映射到 --permission-mode，改用 CLI 默认"),
            }
        }
        if let Some(model) = &spec.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        if let Some(prompt) = &spec.append_system_prompt {
            args.push("--append-system-prompt".into());
            args.push(prompt.as_str().to_owned());
        }
        if let Some(tools) = &spec.allowed_tools {
            if !tools.is_empty() {
                // ⚠ 参数名与分隔符未实测（`--allowedTools` 的 camelCase 写法来自 §4.4 对
                // `--disallowedTools` 的记录）。空表不传：传一个空串可能被解释成「一个都不许用」。
                args.push("--allowedTools".into());
                args.push(tools.join(","));
            }
        }
        args
    }

    /// P0 恒返回空表。
    ///
    /// Tier 2（`PreToolUse` hook 回调）才需要下发 `LUMEN_LLM_HOOK_ADDR` / `LUMEN_LLM_HOOK_TOKEN`，
    /// 而 Tier 2 尚未落地、其环境变量契约也还没定。**这里不能先造一对名字**：造出来的名字会被
    /// 当成既成契约，等真做 Tier 2 时要么将错就错、要么改两处。
    ///
    /// `hook_endpoint` 在 P0 恒为 `None`；真传进来了就**留痕**，绝不静默吞掉。
    fn build_env(&self, spec: &LaunchSpec) -> Vec<(String, String)> {
        if spec.hook_endpoint.is_some() {
            log::warn!("claude: LaunchSpec 带了 hook_endpoint，但 Tier 2 尚未落地，本次忽略");
        }
        Vec::new()
    }

    /// 收到 `system/init` 之前的兜底能力位。
    ///
    /// # 哪几位是「静态事实」、哪一位靠探测
    /// - `streaming` / `thinking` / `thinking_text` / `attachments` 是**对 Claude 的实测事实**，
    ///   与 CLI 版本无关，直接写死；
    /// - `interrupt` 是**唯一靠 `capabilities` 探测**的一位，故基线为 `false`
    ///   （「保守 = 少报」：多报一个 interrupt 会让手机画出一个按下去没反应的中断按钮，
    ///   那比没有按钮更糟）；
    /// - `interactive_permission` = `false`：本机 `claude --help` **没有** `--permission-prompt-tool`，
    ///   `can_use_tool` 通道的 payload 也未验证；
    /// - `resume` = `false`：`--resume` 在 `--input-format stream-json` 下的语义未实测（§12-④）。
    ///
    /// `streaming = true` 指的是**块级**流式（每条 `assistant` 行一到就转发），**不是 token 级**
    /// ——上游 headless 是整块给出 `message.content[]` 的，没有 token 增量可转。填 `false` 会让
    /// 手机端按「只能整轮返回」处理，那与事实不符且体验更差。
    fn baseline_features(&self) -> LlmAgentFeatures {
        LlmAgentFeatures {
            streaming: true,
            thinking: true,
            thinking_text: false,
            interactive_permission: false,
            resume: false,
            attachments: true,
            interrupt: false,
        }
    }

    /// 产出该会话专属的编解码两半，并在这里**建立二者的共享状态**。
    ///
    /// 共享两件东西（§6.9 缺陷修正 ②③④）：
    /// - `turn`：轮号。主线程侧的 [`ClaudeEncoder`] 每成功编码一条用户消息 +1，读线程侧的
    ///   [`ClaudeDecoder`] **只读**。见 [`ClaudeEncoder::encode_user_message`] 里的时序说明。
    /// - `inflight`：在途 control 请求表。**这正是 `split` 必须是唯一出生入口的原因**——
    ///   若两半各持一份，主线程往自己那份 `insert`、读线程从另一份 `remove`，**恒为 `None`**，
    ///   所有 control 的成败关联永久丢失且不报任何错。
    fn split(&self) -> (Box<dyn LlmAgentEncoder>, Box<dyn LlmAgentDecoder>) {
        let turn = Arc::new(AtomicU64::new(0));
        let inflight = Arc::new(Mutex::new(HashMap::new()));
        let encoder = ClaudeEncoder {
            turn: Arc::clone(&turn),
            inflight: Arc::clone(&inflight),
            next_request_id: 0,
        };
        let decoder = ClaudeDecoder {
            turn,
            inflight,
            cursor: TurnCursor::new(),
            baseline: self.baseline_features(),
            conv_model: None,
            last_message_model: None,
            last_prompt_turn: 0,
            last_prompt_tokens: 0,
        };
        (Box::new(encoder), Box::new(decoder))
    }
}

// ── 出站编码器（主线程独占）──────────────────────────────────────────────────

/// 在途 control 请求的类别（只用于本机排障日志，**不上行**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Initialize,
    Interrupt,
    SetPermissionMode,
}

impl ControlKind {
    fn subtype(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Interrupt => "interrupt",
            Self::SetPermissionMode => "set_permission_mode",
        }
    }
}

type Inflight = Arc<Mutex<HashMap<String, ControlKind>>>;

/// Claude 的出站编码器。**主线程独占**。
pub struct ClaudeEncoder {
    turn: Arc<AtomicU64>,
    inflight: Inflight,
    next_request_id: u64,
}

impl std::fmt::Debug for ClaudeEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeEncoder")
            .field("turn", &self.turn.load(Ordering::Relaxed))
            .field("next_request_id", &self.next_request_id)
            .finish()
    }
}

impl ClaudeEncoder {
    /// 自增的 control 请求 id。带 `lumen-` 前缀，便于在混合日志里一眼认出是我们发的。
    fn next_request_id(&mut self) -> String {
        self.next_request_id = self.next_request_id.saturating_add(1);
        format!("lumen-{}", self.next_request_id)
    }

    fn encode_control_request(
        &mut self,
        kind: ControlKind,
        mode: Option<&str>,
    ) -> Result<StdinLine, EncodeError> {
        let request_id = self.next_request_id();
        let line = StdinLine::from_json(&OutControlRequest {
            ty: "control_request",
            request_id: &request_id,
            request: OutControlBody {
                subtype: kind.subtype(),
                mode,
            },
        })?;
        // **先编码成功、再登记在途**：编码失败时不留下一条永远等不到应答的幽灵记录。
        if let Ok(mut map) = self.inflight.lock() {
            map.insert(request_id, kind);
        }
        Ok(line)
    }
}

/// 把附件编成提示词里的路径映射，与桌面端 `llm_attachments.rs::compile_prompt` 同一套路。
///
/// **附件字节不走本协议**：控制端先用现成的 `PutBegin`/`PutChunk`/`PutEnd` 通道把文件传到被控端，
/// 这里拿到的只是**被控端本机绝对路径**，写进提示词让 CLI 自己去读。
fn compile_prompt(text: &str, attachments: &[LlmAttachment]) -> String {
    if attachments.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + attachments.len() * 96);
    out.push_str(text);
    out.push_str("\n\n附件映射（请按下列路径读取对应文件）：\n");
    for attachment in attachments {
        out.push_str(attachment.name.as_str());
        out.push_str(" = \"");
        out.push_str(attachment.path.as_str());
        out.push_str("\"\n");
    }
    out
}

impl LlmAgentEncoder for ClaudeEncoder {
    /// 编码一条用户消息。
    ///
    /// # 轮号推进的时序（与 [`super::LlmRunner::submit`] 严格配对）
    /// `submit` 的顺序是「先 `encode_user_message`、再 `current_turn.fetch_add`、最后 `send`」。
    /// 本函数**在编码成功之后**才 `fetch_add`，于是两个计数器逐条同步：编码失败时两边都不加，
    /// 编码成功时两边都加一。读线程随后解出的第一批事件读到的已经是**新**轮号，本轮的开头
    /// 不会被记到上一轮上。
    ///
    /// 为什么不直接复用 `LlmRunner.current_turn`：`split()` 没有参数、`spawn_reader_thread` 也没
    /// 把那个 `Arc` 传给 decoder（片 2 的既有形状）。改 trait 签名会波及全部适配器，而
    /// `adapter.rs` 的模块文档本来就规定「共享状态由实现方在 `split` 里建」——轮号正是这种状态。
    fn encode_user_message(
        &mut self,
        text: &str,
        attachments: &[LlmAttachment],
    ) -> Result<StdinLine, EncodeError> {
        let prompt = compile_prompt(text, attachments);
        let line = StdinLine::from_json(&OutUserLine {
            ty: "user",
            message: OutUserMessage {
                role: "user",
                content: [OutTextBlock {
                    ty: "text",
                    text: &prompt,
                }],
            },
        })?;
        self.turn.fetch_add(1, Ordering::AcqRel);
        Ok(line)
    }

    /// 编码一条控制指令。
    ///
    /// [`ControlCmd::PermissionReply`] 恒返回 [`EncodeError::Unsupported`]：`can_use_tool` 通道的
    /// payload **完全未验证**，我们连收都收不明白，更不该猜一个回复形状发回去。调用方按契约静默跳过。
    /// 回滚一个轮号。见 [`LlmAgentEncoder::rollback_turn`]。
    ///
    /// `fetch_sub` 而不是 `store(turn - 1)`：这个计数器**只有主线程写**（`submit` 是唯一调用
    /// 路径），但读线程随时在读，`fetch_sub` 才与 `fetch_add` 对称。
    /// 用 `saturating`-语义没有意义：调用点保证此前刚 `fetch_add` 过一次，值必 ≥ 1。
    fn rollback_turn(&mut self) {
        self.turn.fetch_sub(1, Ordering::AcqRel);
    }

    fn encode_control(&mut self, cmd: &ControlCmd) -> Result<StdinLine, EncodeError> {
        match cmd {
            ControlCmd::Initialize => self.encode_control_request(ControlKind::Initialize, None),
            ControlCmd::Interrupt => self.encode_control_request(ControlKind::Interrupt, None),
            ControlCmd::SetPermissionMode(mode) => {
                let wire = permission_mode_to_wire(mode)?;
                self.encode_control_request(ControlKind::SetPermissionMode, Some(wire))
            }
            ControlCmd::PermissionReply { .. } => Err(EncodeError::Unsupported),
        }
    }
}

// ── 入站解码器（读线程独占）──────────────────────────────────────────────────

/// Claude 的入站解码器。**读线程独占**。
pub struct ClaudeDecoder {
    /// 主线程推进、这里只读。
    turn: Arc<AtomicU64>,
    inflight: Inflight,
    /// 块号游标：**按轮重置**，且同一轮内 user 与 assistant 共用一个编号空间。
    cursor: TurnCursor,
    baseline: LlmAgentFeatures,
    /// `init` 报的模型名（实测 `"claude-opus-5[1m]"`）。只用于在 `modelUsage` 里挑条目。
    conv_model: Option<String>,
    /// 最后一条 `assistant` 行的 `message.model`（实测 `"claude-opus-5"`，与上面**不同串**）。
    last_message_model: Option<String>,
    /// [`Self::last_prompt_tokens`] 属于哪一轮。
    last_prompt_turn: u64,
    /// 本轮**最后一次** API 调用的提示词 token 数（= [`LlmUsage::context_used`]）。
    last_prompt_tokens: u64,
}

impl std::fmt::Debug for ClaudeDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaudeDecoder")
            .field("turn", &self.turn.load(Ordering::Relaxed))
            .field("block_cursor_turn", &self.cursor.turn())
            .finish()
    }
}

impl ClaudeDecoder {
    fn current_turn(&self) -> u64 {
        self.turn.load(Ordering::Acquire)
    }

    /// 产出一个挂在当前轮上的块条目。
    fn push_block(
        &mut self,
        turn: u64,
        parent: Option<&str>,
        block: LlmBlock,
        out: &mut Vec<RunnerEvent>,
    ) {
        let entry = LlmBlockEntry {
            block_id: self.cursor.next_block_id(turn),
            parent_call_id: parent.map(str::to_owned),
            block,
        };
        out.push(RunnerEvent::Block { turn, entry });
    }

    // ── system/init ─────────────────────────────────────────────────────────

    fn decode_init(
        &mut self,
        env: &LineEnvelope<'_>,
        raw: &str,
        out: &mut Vec<RunnerEvent>,
    ) -> Result<(), DecodeError> {
        let line: InitLine<'_> = parse_line(raw, "system.init")?;

        let mut features = self.baseline;
        if let Some(CapabilityField::List(tokens)) = &line.capabilities {
            for token in tokens {
                if let CapabilityToken::Name(name) = token {
                    if name == CAP_INTERRUPT {
                        features.interrupt = true;
                    }
                }
            }
        }

        self.conv_model = line.model.as_deref().map(str::to_owned);

        out.push(RunnerEvent::Started(Box::new(StartedInfo {
            // `session_id` 取自信封（所有行共有），不在 init 里再声明一遍。
            cli_session_id: env.session_id.as_deref().unwrap_or_default().to_owned(),
            model: line.model.as_deref().map(str::to_owned),
            // 上游没给 cwd 时填空串而不是猜一个：`LlmPath` 是脱敏包装，空串在 UI 上就是「未知」。
            cwd: LlmPath::new(line.cwd.as_deref().unwrap_or_default()),
            permission_mode: line
                .permission_mode
                .as_deref()
                .map(permission_mode_from_wire),
            cli_version: line.claude_code_version.as_deref().map(str::to_owned),
            features,
        })));
        Ok(())
    }

    // ── system/status（⚠ 未实测）────────────────────────────────────────────

    fn decode_status(&mut self, raw: &str, out: &mut Vec<RunnerEvent>) -> Result<(), DecodeError> {
        let line: StatusLine<'_> = parse_line(raw, "system.status")?;
        let Some(wire) = line.permission_mode.as_deref() else {
            // 没有 `permissionMode` 就说明这行不是我们推定的那个形状。**报 `Unmodeled` 而不是
            // 静默跳过**：这一行在两份样本里一次都没出现过，真出现时必须有人看见。
            return Err(DecodeError {
                at: "system.status.permissionMode",
                kind: DecodeErrorKind::Unmodeled,
            });
        };
        out.push(RunnerEvent::PermissionModeChanged {
            mode: permission_mode_from_wire(wire),
        });
        Ok(())
    }

    // ── assistant ───────────────────────────────────────────────────────────

    /// `assistant` 行 → 若干块。
    ///
    /// **判定顺序是硬要求**（`llm.rs` 铁律二）：先读顶层 `is_api_error_message`，为真则整条消息的
    /// 文本落 [`LlmBlock::Error`]；为假才按常规块处理。顺序反了就会把「403 请求被拒」渲染成模型的
    /// 正常回复气泡，用户完全看不出这是故障。
    fn decode_assistant(
        &mut self,
        raw: &str,
        out: &mut Vec<RunnerEvent>,
    ) -> Result<(), DecodeError> {
        let line: AssistantLine<'_> = parse_line(raw, "assistant")?;
        let turn = self.current_turn();
        let parent = line.parent_tool_use_id.as_deref();

        // `<synthetic>` 是哨兵不是模型名，**必须过滤**（见 [`MODEL_SYNTHETIC`]）：
        // 一次 api-error 合成消息就能把这个字段永久污染成一个查不到的键，而它**跨轮不重置**。
        if let Some(model) = line
            .message
            .model
            .as_deref()
            .filter(|model| *model != MODEL_SYNTHETIC)
        {
            self.last_message_model = Some(model.to_owned());
        }
        // 本轮**最后一次** API 调用的提示词 token 数——每来一条 assistant 行就覆盖一次，
        // `result` 到达时手上留下的正是最后一次。轮号一起记下来，`decode_result` 靠它判断
        // 「这份数字是不是本轮的」；对不上就报 0（未知），而不是把上一轮的分子拿来用。
        if let Some(usage) = line.message.usage {
            self.last_prompt_turn = turn;
            self.last_prompt_tokens = usage.prompt_tokens();
        }

        if line.is_api_error_message {
            // 错误消息实测就是一条普通 text 块；把所有 text 拼起来当错误正文。
            let mut message = String::new();
            for block in &line.message.content {
                if block.ty.as_deref() == Some("text") {
                    if let Some(text) = block.text.as_deref() {
                        if !message.is_empty() {
                            message.push('\n');
                        }
                        message.push_str(text);
                    }
                }
            }
            // 上游连 `error` 键都没给时只能落粗粒度兜底。**不填 `Protocol`**——那会让手机端
            // 提示「协议违例」，把用户的排障方向从「去登录 / 看额度」引到「Lumen 坏了」。
            let code = line
                .error
                .as_deref()
                .map_or(LlmErrorCode::Io, error_code_from_wire);
            self.push_block(
                turn,
                parent,
                LlmBlock::Error {
                    code,
                    message: Clamped::<LLM_TOOL_OUTPUT_MAX_BYTES>::clamp(&message).into_text(),
                },
                out,
            );
            // 实测这种消息里只有 text 块。真出现别的块时**照常产出**（它们不是错误正文的一部分，
            // 丢掉就是无声降级）；组合未实测，故在这里就地写明。
        }

        for block in line.message.content {
            let ty = block.ty.as_deref().unwrap_or_default();
            match ty {
                // 错误路径上文本已经落进 Error 块，不再重复产出一个气泡。
                "text" if line.is_api_error_message => {}
                "text" => {
                    let text = block.text.as_deref().unwrap_or_default();
                    self.push_block(
                        turn,
                        parent,
                        LlmBlock::Text {
                            text: LlmText::new(text),
                        },
                        out,
                    );
                }
                "thinking" => {
                    // **Claude 实测正文恒为空串** ⇒ 恒走 `Omitted` 分支，块只承担「有过思考」
                    // 这个事实。`Text` 分支在 Claude 上是**不可达的未验证代码**，留着是因为
                    // `llm.rs` 明写「被控端能拿到就发 Text、拿不到就发 Omitted」——真有一天上游
                    // 给了正文，静默丢掉才是错的。`signature` 在结构体里根本没声明，不出 PC。
                    let content = match block.thinking.as_deref() {
                        Some(text) if !text.is_empty() => LlmThinking::Text {
                            text: Clamped::<LLM_TOOL_OUTPUT_MAX_BYTES>::clamp(text).into_text(),
                        },
                        _ => LlmThinking::Omitted,
                    };
                    self.push_block(turn, parent, LlmBlock::Thinking { content }, out);
                }
                // ⚠ 块类型名来自公开 Messages API，**本机未采到真样**（§12-①）。
                // 走到这条分支说明可以把它从「未取到」清单里划掉了。
                "redacted_thinking" => {
                    self.push_block(
                        turn,
                        parent,
                        LlmBlock::Thinking {
                            content: LlmThinking::Redacted,
                        },
                        out,
                    );
                }
                "tool_use" => {
                    let Some(call_id) = block.id.as_deref().filter(|id| !id.is_empty()) else {
                        // `call_id` 是与 `ToolResult` 配对的唯一钥匙，空的块产出来只会制造
                        // 一张永远等不到结果的卡片。
                        return Err(DecodeError {
                            at: "assistant.content.tool_use.id",
                            kind: DecodeErrorKind::Schema,
                        });
                    };
                    let call_id = clamp_ident(call_id);
                    let name = clamp_ident(block.name.as_deref().unwrap_or_default());
                    let (input, truncated_bytes) =
                        clamp_tool_input(block.input.unwrap_or(serde_json::Value::Null));
                    self.push_block(
                        turn,
                        parent,
                        LlmBlock::ToolUse {
                            call_id,
                            name,
                            // 恒 `None`：见模块文档「被否决的替代方案 2」。
                            title: None,
                            input,
                            truncated_bytes,
                        },
                        out,
                    );
                }
                // 未知块类型**静默跳过**（归一化硬规则 3）：`assistant` 行里出现新块类型是上游加
                // 功能的常态，为它报一条协议错误只会刷屏。整行被丢的风险由 `classify` 那一层的
                // 未识别计数兜底。
                _ => {}
            }
        }
        Ok(())
    }

    // ── user ────────────────────────────────────────────────────────────────

    /// `user` 行 → 工具结果块（+ 结构化摘要）。
    fn decode_user(&mut self, raw: &str, out: &mut Vec<RunnerEvent>) -> Result<(), DecodeError> {
        let line: UserLine<'_> = parse_line(raw, "user")?;
        let turn = self.current_turn();
        let parent = line.parent_tool_use_id.as_deref();

        // ⚠ 未实测分支：P0 不传 `--replay-user-messages`，故这条恒不触发。留着是为了打开那个
        // 开关时只需改 `build_args`，不用再动解码器。
        if line.is_replay == Some(true) {
            let mut blocks = Vec::new();
            for block in &line.message.content {
                if block.ty.as_deref() == Some("text") {
                    blocks.push(LlmBlockEntry {
                        block_id: self.cursor.next_block_id(turn),
                        parent_call_id: parent.map(str::to_owned),
                        block: LlmBlock::Text {
                            text: LlmText::new(block.text.as_deref().unwrap_or_default()),
                        },
                    });
                }
            }
            out.push(RunnerEvent::TurnUserEcho { turn, blocks });
            return Ok(());
        }

        // 顶层 `tool_use_result` 与块里的 `tool_result` 是**两处**数据，得先确定它们配不配得上对。
        // 实测形态是「一行一个 tool_result 块 + 一个 tool_use_result」。**只有恰好一个块时才敢把
        // 摘要挂上去**：多个块时上游没有任何字段说明摘要属于哪一个，猜一个挂上去等于给手机端一张
        // 张冠李戴的卡片。
        let tool_result_count = line
            .message
            .content
            .iter()
            .filter(|block| block.ty.as_deref() == Some("tool_result"))
            .count();
        let mut detail = if tool_result_count == 1 {
            line.tool_use_result.as_ref().and_then(build_tool_detail)
        } else {
            None
        };

        for block in line.message.content {
            if block.ty.as_deref() != Some("tool_result") {
                // `user` 行里出现别的块类型未实测；静默跳过，同归一化硬规则 3。
                continue;
            }
            let Some(call_id) = block.tool_use_id.as_deref().filter(|id| !id.is_empty()) else {
                return Err(DecodeError {
                    at: "user.message.content.tool_result.tool_use_id",
                    kind: DecodeErrorKind::Schema,
                });
            };
            let call_id = clamp_ident(call_id);
            // `content` 存在但不是字符串（`web_search_tool_result` 等新块形态）：
            // **块照常产出**（call_id 不能变孤儿），但要留一条**响亮**的协议错误——
            // 那正是「去补一次采样」的信号。静默给一张空卡片才是 §14-6 禁止的无声降级。
            let (content, content_unmodeled) = MaybeClamped::into_text_or_empty(block.content);
            let truncated_bytes = content.truncated_bytes();
            // **缺键 = 非错误**（实测 `is_error` 整个键都不存在）。`Denied` / `Cancelled` 由
            // 被控端自己的状态机给出，不是从这里推的。
            let status = if block.is_error.unwrap_or(false) {
                LlmToolStatus::Error
            } else {
                LlmToolStatus::Ok
            };
            self.push_block(
                turn,
                parent,
                LlmBlock::ToolResult {
                    call_id,
                    status,
                    output: content.into_text(),
                    truncated_bytes,
                    detail: detail.take(),
                },
                out,
            );
            if content_unmodeled {
                out.push(RunnerEvent::ProtocolError {
                    kind: super::event::ProtocolErrorKind::DecodeFailed {
                        at: "user.message.content.tool_result.content",
                    },
                });
            }
        }
        Ok(())
    }

    // ── rate_limit_event ────────────────────────────────────────────────────

    fn decode_rate_limit(
        &mut self,
        raw: &str,
        out: &mut Vec<RunnerEvent>,
    ) -> Result<(), DecodeError> {
        let line: RateLimitLine<'_> = parse_line(raw, "rate_limit_event")?;
        let info = line.rate_limit_info;
        // `status` 是 `LlmRateLimit` 里唯一的必填字段，缺了这条播报就没有意义——**不编一个
        // `Other("")` 顶上**（`open_enum!` 明令禁止把空串塞进 `Other`）。
        let Some(status) = info.status.as_deref() else {
            return Err(DecodeError {
                at: "rate_limit_event.rate_limit_info.status",
                kind: DecodeErrorKind::Schema,
            });
        };
        out.push(RunnerEvent::RateLimit {
            info: Box::new(LlmRateLimit {
                status: rate_limit_status_from_wire(status),
                window: info
                    .rate_limit_type
                    .as_deref()
                    .map(rate_limit_window_from_wire),
                resets_at_secs: sanitize_resets_at(info.resets_at),
                overage_status: info.overage_status.as_deref().map(overage_status_from_wire),
                overage_disabled_reason: info
                    .overage_disabled_reason
                    .as_deref()
                    .map(overage_reason_from_wire),
                // **保持 `Option`**：`None` = 上游没给这个键，与 `Some(false)` 是两件事。
                using_overage: info.is_using_overage,
            }),
        });
        Ok(())
    }

    // ── result ──────────────────────────────────────────────────────────────

    fn decode_result(&mut self, raw: &str, out: &mut Vec<RunnerEvent>) -> Result<(), DecodeError> {
        let line: ResultLine<'_> = parse_line(raw, "result")?;
        let turn = self.current_turn();
        let usage_in = line.usage.unwrap_or_default();
        // **先取裁掉的字节数再转成文本**：`into_text` 会消费掉 `Clamped`，那个数字随之丢失。
        let result_text = line.result.filter(|text| !text.is_empty());
        let result_truncated_bytes = result_text.as_ref().and_then(Clamped::truncated_bytes);
        let (context_limit, max_output_tokens) = self.pick_model_window(line.model_usage.as_ref());

        let usage = LlmUsage {
            // 账务口径：整轮累加。
            input_tokens: usage_in.input_tokens,
            output_tokens: usage_in.output_tokens,
            cache_read_tokens: usage_in.cache_read_input_tokens,
            cache_write_tokens: usage_in.cache_creation_input_tokens,
            cost_micro_usd: cost_micro_usd(line.total_cost_usd),
            // 进度条口径：本轮**最后一次** API 调用。两个口径在同一结构里并存是刻意的。
            context_used: if self.last_prompt_turn == turn {
                self.last_prompt_tokens
            } else {
                0
            },
            context_limit,
            max_output_tokens,
        };

        // 上游没给 `stop_reason` 时按成败推一个：这是**推导**不是编造——`is_error` 是权威判定，
        // 由它反推「这轮是出错停的还是正常收的」不引入任何新信息。
        let stop = line.stop_reason.as_deref().map_or(
            if line.is_error {
                LlmStopReason::Error
            } else {
                LlmStopReason::EndTurn
            },
            stop_reason_from_wire,
        );

        out.push(RunnerEvent::TurnEnded(Box::new(TurnEndedInfo {
            turn,
            stop,
            outcome: LlmTurnOutcome {
                is_error: line.is_error,
                terminal_reason: line
                    .terminal_reason
                    .as_deref()
                    .map(terminal_reason_from_wire),
                api_error_status: line.api_error_status,
            },
            usage,
            // 元素结构未实测 ⇒ **只转条数**（§5.0-4）。
            denials: line
                .permission_denials
                .map_or(0, |items| u32::try_from(items.len()).unwrap_or(u32::MAX)),
            result_text: result_text.map(Clamped::into_text),
            result_truncated_bytes,
        })));
        Ok(())
    }

    /// 从 `modelUsage` 里挑出**本对话主模型**那一条的窗口值。
    ///
    /// # 为什么不能求和、也不能取最大
    /// `llm.rs` 的 [`LlmUsage::context_limit`] 写死了：不同模型的窗口不同，一轮里可能同时用了主
    /// 模型与子代理模型，求和 / 取最大都会给出一个不存在的分母，进度条从此长期失真。
    ///
    /// # 挑选顺序（前两条靠模型名，第三条靠「只有一个候选」这个事实）
    /// 1. 最后一条 `assistant` 行的 `message.model`（实测 `"claude-opus-5"`）；
    /// 2. `init` 报的 `model`（实测 `"claude-opus-5[1m]"`，**与上面不是同一个串**，故两条都试）；
    /// 3. map 里**恰好只有一项**时用它——单模型会话是常态，这一条能覆盖绝大多数情况；
    /// 4. 以上都不成立 → `(0, 0)` = 未知。**手机端据此不显示百分比**，而不是显示一个瞎猜的数。
    ///
    /// ⚠ key 的确切形态**没有直接样本**（样本 A 的 `modelUsage` 是空 map，样本 B 的 `result` 行
    /// 被编码损坏），只有 §3.3-B9 记的「按模型名做 key」。第 3 条就是给这份不确定性兜底的。
    fn pick_model_window(&self, usage: Option<&BTreeMap<String, ModelUsageEntry>>) -> (u64, u64) {
        let Some(map) = usage.filter(|map| !map.is_empty()) else {
            return (0, 0);
        };
        let entry = self
            .last_message_model
            .as_deref()
            .and_then(|model| map.get(model))
            .or_else(|| self.conv_model.as_deref().and_then(|model| map.get(model)))
            .or_else(|| {
                if map.len() == 1 {
                    map.values().next()
                } else {
                    None
                }
            });
        entry.map_or((0, 0), |entry| {
            (
                entry.context_window.unwrap_or(0),
                entry.max_output_tokens.unwrap_or(0),
            )
        })
    }

    // ── control_response（⚠ 形状是推的）─────────────────────────────────────

    fn decode_control_response(
        &mut self,
        raw: &str,
        out: &mut Vec<RunnerEvent>,
    ) -> Result<(), DecodeError> {
        let line: ControlResponseLine<'_> = parse_line(raw, "control_response")?;
        let body = line.response.as_ref();
        let request_id = body
            .and_then(|body| body.request_id.as_deref())
            .or(line.request_id.as_deref());
        let subtype = body
            .and_then(|body| body.subtype.as_deref())
            .or(line.subtype.as_deref());

        let Some(request_id) = request_id.filter(|id| !id.is_empty()) else {
            return Err(DecodeError {
                at: "control_response.request_id",
                kind: DecodeErrorKind::Unmodeled,
            });
        };
        // 成功判据只认 `"success"`：认不出的 subtype 一律按**失败**处理。中断按钮宁可报「没送到」
        // 让用户再点一次，也不能报「送到了」而实际没有——那是一个没有反馈的黑洞。
        let ok = subtype == Some("success");

        // 在途表：取出来只为在本机日志里说清「是哪条 control 的应答」。**取不到不影响产出事件**
        // ——若因为我们对 `request_id` 的形状推错而丢掉整条 ack，中断反馈就整个没了。
        let kind = self
            .inflight
            .lock()
            .ok()
            .and_then(|mut map| map.remove(request_id));
        if let Some(kind) = kind {
            log::debug!("claude: control {} 应答 ok={ok}", kind.subtype());
        }

        out.push(RunnerEvent::ControlAck {
            request_id: clamp_ident(request_id),
            ok,
        });
        Ok(())
    }
}

/// `tool_use_result.file` → [`LlmToolResultDetail::File`]。
///
/// **四个字段缺一不可**：协议侧这四个字段都不是 `Option`，缺哪个都得自己编一个默认值填进去，
/// 而「填 0」在手机上会渲染成「第 0–0 行（共 0 行）」——一条看起来精确、实则是我们编的信息。
/// 宁可整个摘要退成 `None`（手机端退回按 `output` 渲染纯文本），这正是
/// [`LlmToolResultDetail`] 约束 2 要的行为。
///
/// `file.content`（11 720 字符的用户文件正文）在 [`ToolResultFile`] 里**根本没有字段**，
/// 到不了这里。
fn build_tool_detail(result: &ToolUseResult<'_>) -> Option<LlmToolResultDetail> {
    let file = result.file.as_ref()?;
    Some(LlmToolResultDetail::File {
        path: LlmPath::new(file.file_path.as_deref()?),
        start_line: file.start_line?,
        line_count: file.num_lines?,
        total_lines: file.total_lines?,
    })
}

impl LlmAgentDecoder for ClaudeDecoder {
    /// 第二层解码：信封已由 [`super::event::classify`] 判过白名单，这里按类型分派。
    ///
    /// **`control_request` 刻意不建模**：`can_use_tool` 的触发时机与 payload 完全未验证
    /// （二进制里出现过这两个词，但没有任何一条样本行）。按公开推定去解一个安全相关的通道，
    /// 错了的表现是「手机上的审批弹窗内容不对」或「永远等不到弹窗」——都是无声的。
    /// 这里返回 [`DecodeErrorKind::Unmodeled`]，CLI 真发了就会**响亮地**留下一条协议错误，
    /// 那正是去补采样的信号。`interactive_permission` 能力位同时为 `false`，手机端不会画审批入口。
    fn decode_line(
        &mut self,
        env: &LineEnvelope<'_>,
        raw: &str,
        out: &mut Vec<RunnerEvent>,
    ) -> Result<(), DecodeError> {
        match env.ty.as_ref() {
            "system" => match env.subtype_str() {
                Some("init") => self.decode_init(env, raw, out),
                Some("status") => self.decode_status(raw, out),
                // `classify` 只放行 init / status 两种 subtype，这里理论上不可达。
                _ => Err(DecodeError {
                    at: "system.subtype",
                    kind: DecodeErrorKind::Unmodeled,
                }),
            },
            "assistant" => self.decode_assistant(raw, out),
            "user" => self.decode_user(raw, out),
            "result" => self.decode_result(raw, out),
            "rate_limit_event" => self.decode_rate_limit(raw, out),
            "control_response" => self.decode_control_response(raw, out),
            "control_request" => Err(DecodeError {
                at: "control_request",
                kind: DecodeErrorKind::Unmodeled,
            }),
            // `classify` 的白名单保证走不到这里。
            _ => Err(DecodeError {
                at: "envelope.type",
                kind: DecodeErrorKind::Unmodeled,
            }),
        }
    }
}

// ── 其余三家 CLI 的占位适配器 ────────────────────────────────────────────────

/// Codex / Gemini / Kimi 的**占位**适配器：能力位全 `false`，编解码一律拒绝。
///
/// # 为什么是空壳而不是「先照 Claude 抄一份」
/// 这三家的事件流、命令行参数、stdin 协议**一行都没采过**。照抄一份会得到一个「看起来支持四家
/// CLI」的假象：手机端会把它们列出来、用户点了之后进程要么起不来、要么起来了但一条消息都解不出。
/// 空壳至少是诚实的——[`LlmAgentAdapter::baseline_features`] 全 `false`，
/// [`LlmAgentEncoder::encode_user_message`] 直接 [`EncodeError::Unsupported`]，
/// 调用方在**第一次提交时**就知道不支持，而不是在一段沉默之后。
///
/// **P0 不注册这三个适配器**（`LlmAgentInfo::available = false`）；本类型存在只为让
/// [`LlmAgentKind`] 的四个变体各有归属，避免下一个人以为「漏写了」。
#[derive(Debug, Clone)]
pub struct PlaceholderAdapter {
    kind: LlmAgentKind,
    /// ⚠ **可执行名未经核实**，只是个占位。真做这家 CLI 时第一件事就是核它。
    program: &'static str,
}

impl PlaceholderAdapter {
    pub const CODEX: Self = Self {
        kind: LlmAgentKind::Codex,
        program: "codex",
    };
    pub const GEMINI: Self = Self {
        kind: LlmAgentKind::Gemini,
        program: "gemini",
    };
    pub const KIMI: Self = Self {
        kind: LlmAgentKind::Kimi,
        program: "kimi",
    };
}

impl LlmAgentAdapter for PlaceholderAdapter {
    fn kind(&self) -> LlmAgentKind {
        self.kind.clone()
    }

    fn program(&self) -> &'static str {
        self.program
    }

    /// 空表。**不臆造参数形态**——猜错的参数是「启动即失败」。
    fn build_args(&self, _spec: &LaunchSpec) -> Vec<String> {
        Vec::new()
    }

    fn build_env(&self, _spec: &LaunchSpec) -> Vec<(String, String)> {
        Vec::new()
    }

    /// 全 `false`：一个能力都不敢报。
    fn baseline_features(&self) -> LlmAgentFeatures {
        LlmAgentFeatures::default()
    }

    fn split(&self) -> (Box<dyn LlmAgentEncoder>, Box<dyn LlmAgentDecoder>) {
        (Box::new(UnsupportedCodec), Box::new(UnsupportedCodec))
    }
}

/// 占位适配器的编解码两半：什么都不会，且**说清楚自己不会**。
#[derive(Debug, Clone, Copy)]
struct UnsupportedCodec;

impl LlmAgentEncoder for UnsupportedCodec {
    fn encode_user_message(
        &mut self,
        _text: &str,
        _attachments: &[LlmAttachment],
    ) -> Result<StdinLine, EncodeError> {
        Err(EncodeError::Unsupported)
    }

    fn encode_control(&mut self, _cmd: &ControlCmd) -> Result<StdinLine, EncodeError> {
        Err(EncodeError::Unsupported)
    }
}

impl LlmAgentDecoder for UnsupportedCodec {
    fn decode_line(
        &mut self,
        _env: &LineEnvelope<'_>,
        _raw: &str,
        _out: &mut Vec<RunnerEvent>,
    ) -> Result<(), DecodeError> {
        Err(DecodeError {
            at: "placeholder",
            kind: DecodeErrorKind::Unmodeled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_runner::event::{classify, LineVerdict, MalformedKind, ProtocolErrorKind};
    use lumen_protocol::llm::{LlmConvMeta, LlmDeltaItem, LlmFrame};
    use std::path::PathBuf;

    // ── fixture ─────────────────────────────────────────────────────────────
    //
    // **测试只读 crate 内的这两份副本，绝不读 `docs/`**：测试依赖文档目录会在有人整理文档时
    // 无声变红，而且会让「跑测试」隐式依赖仓库布局。两份副本与 `docs/调研/` 下的原件逐字节相同
    // （复制时校验过 sha256）。
    //
    // ⚠ **两份 fixture 都是脱敏版**：样本 A 第 4 行那 4106 字符私人记忆库原文、样本 B 第 9 行那
    // 11720 字符文件正文都已被替换成占位符。下面的负向测试断言的是**占位符没被透传**，
    // 强度低于原始样本 —— 见 `hook行的任何内容都不出现在归一化帧里` 的注释。

    /// 样本 A：失败路径（403 认证失败），9 行。
    const SAMPLE_A: &str = include_str!("fixtures/claude-stream-json-sample-a-2026-08-08.jsonl");
    /// 样本 B：成功路径 + 工具调用，13 行（其中 2 行因采样编码损坏，文件里是 `_note` 占位）。
    const SAMPLE_B: &str =
        include_str!("fixtures/claude-stream-json-sample-b-tooluse-2026-08-08.jsonl");

    /// 把一份样本逐行喂进「白名单 → 适配器」这条完整链路，产出与真实读线程一致的事件序列。
    ///
    /// 复制的是 `mod.rs::decode_chunk` 的处置（那是私有函数），保证这里测的路径与线上一致。
    fn run_sample(decoder: &mut ClaudeDecoder, sample: &str) -> Vec<RunnerEvent> {
        let mut events = Vec::new();
        for line in sample.lines() {
            match classify(line) {
                LineVerdict::Blank => {}
                LineVerdict::Parse(env) => {
                    if let Err(err) = decoder.decode_line(&env, line, &mut events) {
                        events.push(RunnerEvent::ProtocolError {
                            kind: ProtocolErrorKind::DecodeFailed { at: err.at },
                        });
                    }
                }
                LineVerdict::DropSilently { tag } => events.push(RunnerEvent::LineDropped { tag }),
                LineVerdict::UnknownSchema { tag } => events.push(RunnerEvent::ProtocolError {
                    kind: ProtocolErrorKind::UnknownSchema { tag },
                }),
                LineVerdict::Malformed { kind, bytes } => {
                    events.push(RunnerEvent::ProtocolError {
                        kind: ProtocolErrorKind::MalformedEnvelope { kind, bytes },
                    });
                }
            }
        }
        events
    }

    /// 单独一个解码器（不关心轮号推进的用例用它）。
    ///
    /// 用具体类型而不是 [`LlmAgentAdapter::split`] 返回的 `Box<dyn …>`，是因为多数用例要断言
    /// 解码器内部状态（轮号 / 块号游标）；`split_pair` 覆盖了「两半共享同一份 Arc」这条不变量。
    fn new_decoder() -> ClaudeDecoder {
        split_pair().1
    }

    /// 一对**共享同一份 Arc** 的编解码两半（测轮号同步用），与
    /// [`ClaudeAdapter::split`] 建立的共享关系逐字段一致。
    fn split_pair() -> (ClaudeEncoder, ClaudeDecoder) {
        let turn = Arc::new(AtomicU64::new(0));
        let inflight: Inflight = Arc::new(Mutex::new(HashMap::new()));
        let encoder = ClaudeEncoder {
            turn: Arc::clone(&turn),
            inflight: Arc::clone(&inflight),
            next_request_id: 0,
        };
        let decoder = ClaudeDecoder {
            turn,
            inflight,
            cursor: TurnCursor::new(),
            baseline: ClaudeAdapter::new().baseline_features(),
            conv_model: None,
            last_message_model: None,
            last_prompt_turn: 0,
            last_prompt_tokens: 0,
        };
        (encoder, decoder)
    }

    fn decode_one(decoder: &mut ClaudeDecoder, line: &str) -> Vec<RunnerEvent> {
        let mut out = Vec::new();
        let LineVerdict::Parse(env) = classify(line) else {
            panic!("该行应被白名单放行");
        };
        decoder
            .decode_line(&env, line, &mut out)
            .expect("该行应可解码");
        out
    }

    /// **片 4 的迷你版**：把归一化事件拼成真正会上行的 [`LlmFrame`]。
    ///
    /// 负向测试必须断言在**序列化后的帧**上——只断言 `RunnerEvent` 会漏掉「序列化时把某个字段
    /// 又带出去了」这类问题，而帧才是真正过网的东西。
    fn to_frames(events: &[RunnerEvent]) -> Vec<LlmFrame> {
        let mut frames = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let seq = index as u64 + 1;
            match event {
                RunnerEvent::Block { turn, entry } => frames.push(LlmFrame::Delta {
                    conv_id: 1,
                    conv_generation: 1,
                    seq,
                    turn: u32::try_from(*turn).unwrap_or(u32::MAX),
                    items: vec![LlmDeltaItem::BlockStart {
                        entry: entry.clone(),
                    }],
                }),
                RunnerEvent::TurnUserEcho { turn, blocks } => frames.push(LlmFrame::TurnStarted {
                    conv_id: 1,
                    conv_generation: 1,
                    seq,
                    turn: u32::try_from(*turn).unwrap_or(u32::MAX),
                    user: blocks.clone(),
                    started_ms: 0,
                    // 本 fixture 走的是「上游事件 → 帧」这一段，与控制端的幂等键无关
                    // （回带发生在 `remote_ws/llm.rs::begin_turn`）。
                    client_msg_id: None,
                }),
                RunnerEvent::TurnEnded(info) => frames.push(LlmFrame::TurnEnded {
                    conv_id: 1,
                    conv_generation: 1,
                    seq,
                    turn: u32::try_from(info.turn).unwrap_or(u32::MAX),
                    stop: info.stop.clone(),
                    outcome: info.outcome.clone(),
                    usage: info.usage,
                    ended_ms: 0,
                    truncated: false,
                }),
                RunnerEvent::RateLimit { info } => frames.push(LlmFrame::RateLimit {
                    conv_id: 1,
                    conv_generation: 1,
                    observed_ms: 0,
                    info: (**info).clone(),
                }),
                // `Started` **另走 [`started_json`]**：它在片 4 里落进 `LlmConvMeta`
                // （随 ConvStarted / Attached / HelloAck 过网），形状与 `LlmFrame::Delta`
                // 完全不同，塞进这里反而看不清。**但它必须被序列化断言覆盖**，理由见该函数。
                //
                // `LineDropped` / `ProtocolError` / `ControlAck` / `PermissionModeChanged`
                // 在片 4 里落到本机日志或控制通道，不承载上游内容。
                _ => {}
            }
        }
        frames
    }

    fn frames_json(events: &[RunnerEvent]) -> String {
        serde_json::to_string(&to_frames(events)).expect("帧必可序列化")
    }

    /// **片 4 的迷你版之二**：把 [`StartedInfo`] 拼成真正会过网的 [`LlmConvMeta`] 并序列化。
    ///
    /// # 这个函数补的是一个真实的断言盲区
    /// 本文件自己立的规矩是「负向测试必须断言在**序列化后的帧**上——只断言 `RunnerEvent` 会漏掉
    /// 『序列化时把某个字段又带出去了』这类问题」。但 `to_frames` 的 `_ => {}` 把
    /// `RunnerEvent::Started` 整个丢掉了，于是**整套白名单负向测试没有任何一条覆盖到
    /// `StartedInfo` 的序列化形态**——而它恰恰是全部事件里承载本机环境信息最多的那一个
    /// （`cwd` / `model` / `cli_session_id` / `permission_mode` 全部随 `LlmConvMeta` 过网）。
    ///
    /// 更要命的是仅剩那条 init 负向测试只看 `format!("{events:?}")`，而 `StartedInfo.cwd` 是
    /// [`LlmPath`]、`Debug` 恒为 `LlmPath(<redacted>)` ⇒ **任何被塞进 `LlmPath` / `LlmText` 型
    /// 字段的本机信息都能同时通过 Debug 断言并原样上网**。脱敏包装保护的是日志，不是线格式。
    fn started_json(events: &[RunnerEvent]) -> String {
        let metas: Vec<LlmConvMeta> = events
            .iter()
            .filter_map(|event| match event {
                RunnerEvent::Started(info) => Some(LlmConvMeta {
                    conv_id: 1,
                    conv_generation: 1,
                    agent: LlmAgentKind::Claude,
                    cwd: info.cwd.clone(),
                    title: LlmText::new(""),
                    state: lumen_protocol::llm::LlmConvState::Idle,
                    model: info.model.clone(),
                    permission_mode: info.permission_mode.clone(),
                    cli_session_id: Some(info.cli_session_id.clone()),
                    origin: None,
                    cur_turn: 0,
                    created_ms: 0,
                    updated_ms: 0,
                    usage: LlmUsage::default(),
                }),
                _ => None,
            })
            .collect();
        // `cli_version` 与 `features` 不进 `LlmConvMeta`（前者只供排障、后者按 agent 维度走
        // `LlmAgentInfo`），但它们同样来自 `init` 行，故一并序列化进来一起做泄漏断言。
        let extra: Vec<(&Option<String>, &LlmAgentFeatures)> = events
            .iter()
            .filter_map(|event| match event {
                RunnerEvent::Started(info) => Some((&info.cli_version, &info.features)),
                _ => None,
            })
            .collect();
        format!(
            "{}{}",
            serde_json::to_string(&metas).expect("元信息必可序列化"),
            serde_json::to_string(&extra).expect("附加项必可序列化"),
        )
    }

    // ── 样本 A：失败路径全量回放 ────────────────────────────────────────────

    #[test]
    fn 样本a逐行回放_六条hook被丢弃_init认证错误与result各就各位() {
        let mut decoder = new_decoder();
        let events = run_sample(&mut decoder, SAMPLE_A);

        // 3 行 hook_started + 3 行 hook_response 全部被白名单挡掉，且只留标识不留内容。
        let dropped: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                RunnerEvent::LineDropped { tag } => Some(tag.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            dropped,
            vec![
                "system/hook_started",
                "system/hook_started",
                "system/hook_started",
                "system/hook_response",
                "system/hook_response",
                "system/hook_response",
            ]
        );

        // init：只取六项，能力位由 capabilities 探测出来。
        let RunnerEvent::Started(info) = events
            .iter()
            .find(|e| matches!(e, RunnerEvent::Started(_)))
            .expect("必须有 Started")
        else {
            unreachable!()
        };
        assert_eq!(info.cli_session_id, "033a4c13-ad45-4762-b0a3-c274e950fe5c");
        assert_eq!(info.model.as_deref(), Some("claude-opus-5[1m]"));
        assert_eq!(info.cli_version.as_deref(), Some("2.1.226"));
        assert_eq!(info.permission_mode, Some(LlmPermissionMode::AcceptEdits));
        assert!(
            info.features.interrupt,
            "capabilities 含 interrupt_receipt_v1"
        );
        assert!(info.features.thinking);
        assert!(
            !info.features.thinking_text,
            "Claude Opus 5 实测思考正文恒为空"
        );
        assert!(!info.features.interactive_permission);

        // 认证失败：必须是 Error 块，**不是** Text 气泡（铁律二）。
        let blocks: Vec<&LlmBlock> = events
            .iter()
            .filter_map(|e| match e {
                RunnerEvent::Block { entry, .. } => Some(&entry.block),
                _ => None,
            })
            .collect();
        assert_eq!(blocks.len(), 1, "样本 A 只有一条 assistant 消息");
        let LlmBlock::Error { code, message } = blocks[0] else {
            panic!(
                "is_api_error_message=true 必须落 Error 块，实际 {:?}",
                blocks[0]
            );
        };
        assert_eq!(*code, LlmErrorCode::AuthRequired);
        assert_eq!(
            message.as_str(),
            "Failed to authenticate. API Error: 403 Request not allowed"
        );

        // result：成败只看 is_error；subtype 是 "success" 但这一轮是失败的。
        let RunnerEvent::TurnEnded(info) = events
            .iter()
            .find(|e| matches!(e, RunnerEvent::TurnEnded(_)))
            .expect("必须有 TurnEnded")
        else {
            unreachable!()
        };
        assert!(info.outcome.is_error, "subtype=success 但 is_error=true");
        assert_eq!(
            info.outcome.terminal_reason,
            Some(LlmTerminalReason::ApiError)
        );
        assert_eq!(info.outcome.api_error_status, Some(403));
        // 已知集合里没有 "stop_sequence" ⇒ 原样落 Other，不硬塞进某个已知变体。
        assert_eq!(info.stop, LlmStopReason::from_wire_clamped("stop_sequence"));
        assert!(!info.stop.is_known());
        assert_eq!(info.denials, 0);
        assert_eq!(
            info.result_text.as_ref().map(LlmText::as_str),
            Some("Failed to authenticate. API Error: 403 Request not allowed")
        );
    }

    // ── 样本 B：成功路径 + 工具调用全量回放 ─────────────────────────────────

    #[test]
    fn 样本b逐行回放_工具调用_工具结果_限流_思考四种事件都对() {
        let mut decoder = new_decoder();
        let events = run_sample(&mut decoder, SAMPLE_B);

        let blocks: Vec<&LlmBlockEntry> = events
            .iter()
            .filter_map(|e| match e {
                RunnerEvent::Block { entry, .. } => Some(entry),
                _ => None,
            })
            .collect();
        assert_eq!(blocks.len(), 3, "tool_use + tool_result + thinking");

        // ① tool_use：caller 不上行、input 原样（未超预算故无截断）。
        let LlmBlock::ToolUse {
            call_id,
            name,
            title,
            input,
            truncated_bytes,
        } = &blocks[0].block
        else {
            panic!("首块应为 ToolUse，实际 {:?}", blocks[0].block);
        };
        assert_eq!(call_id, "toolu_019Vk9BtrSBpc2dtv9YBPTY3");
        assert_eq!(name, "Read");
        assert!(title.is_none(), "title 恒 None，见模块文档");
        assert!(truncated_bytes.is_none());
        assert!(input.get().get("file_path").is_some());
        assert!(
            input.get().get("caller").is_none(),
            "caller 是语义未知的对象，绝不上行"
        );

        // ② tool_result：is_error 键不存在 ⇒ Ok；顶层 tool_use_result 变成结构化摘要。
        let LlmBlock::ToolResult {
            call_id,
            status,
            output,
            truncated_bytes,
            detail,
        } = &blocks[1].block
        else {
            panic!("次块应为 ToolResult，实际 {:?}", blocks[1].block);
        };
        assert_eq!(call_id, "toolu_019Vk9BtrSBpc2dtv9YBPTY3", "与调用配得上对");
        assert_eq!(*status, LlmToolStatus::Ok, "is_error 缺键必须解释成 Ok");
        assert_eq!(output.as_str(), "<<REDACTED 12272 chars>>");
        assert!(truncated_bytes.is_none(), "脱敏后的占位符没超 32 KiB");
        let Some(LlmToolResultDetail::File {
            path,
            start_line,
            line_count,
            total_lines,
        }) = detail
        else {
            panic!("Read 的 tool_use_result 应产出 File 摘要，实际 {detail:?}");
        };
        assert_eq!(path.as_str(), "<<REDACTED>>");
        assert_eq!((*start_line, *line_count, *total_lines), (1, 165, 165));

        // ③ thinking：恒 Omitted，signature 一个字节都不出现。
        assert_eq!(
            blocks[2].block,
            LlmBlock::Thinking {
                content: LlmThinking::Omitted
            }
        );

        // ④ rate_limit_event：四个开放枚举全部命中已知值，resetsAt 是 Unix 秒。
        let RunnerEvent::RateLimit { info } = events
            .iter()
            .find(|e| matches!(e, RunnerEvent::RateLimit { .. }))
            .expect("必须有 RateLimit")
        else {
            unreachable!()
        };
        assert_eq!(info.status, LlmRateLimitStatus::Allowed);
        assert_eq!(info.window, Some(LlmRateLimitWindow::FiveHour));
        assert_eq!(info.resets_at_secs, Some(1_786_211_400));
        assert_eq!(info.overage_status, Some(LlmOverageStatus::Rejected));
        assert_eq!(
            info.overage_disabled_reason,
            Some(LlmOverageDisabledReason::OutOfCredits)
        );
        assert_eq!(
            info.using_overage,
            Some(false),
            "上游明确给了 false，与「没给」是两件事"
        );

        // ⑤ 两行被采样编码损坏的占位行：识别成缺 type 的畸形行，不崩、不透传。
        let missing_type = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    RunnerEvent::ProtocolError {
                        kind: ProtocolErrorKind::MalformedEnvelope {
                            kind: MalformedKind::MissingType,
                            ..
                        }
                    }
                )
            })
            .count();
        assert_eq!(missing_type, 2);
    }

    #[test]
    fn 样本b的思考块块号与工具块共用同一编号空间() {
        let mut decoder = new_decoder();
        let events = run_sample(&mut decoder, SAMPLE_B);
        let ids: Vec<u32> = events
            .iter()
            .filter_map(|e| match e {
                RunnerEvent::Block { entry, .. } => Some(entry.block_id),
                _ => None,
            })
            .collect();
        // 同一轮内 user 的 tool_result 与 assistant 的块**共用**一个编号空间，
        // 否则助手的块会静默覆盖用户块（`llm.rs:150-157` 记的那个坑）。
        assert_eq!(ids, vec![0, 1, 2]);
    }

    // ── 负向测试 ①：hook 行的任何内容都不出现在归一化帧里 ───────────────────

    #[test]
    fn hook行的任何内容都不出现在归一化帧里() {
        // ⚠⚠ **这条测试的强度说明，别让下一个人以为够了** ⚠⚠
        //
        // 入库 fixture 是**脱敏版**：样本 A 第 4 行原本携带 4106 字符 / 6976 UTF-8 字节的本机私人
        // 记忆库全文，现在 `output` / `stdout` / `stderr` 三个字段都已被替换成占位符，该行现存
        // 448 字节。所以本测试断言的是**占位符字符串没被透传**，
        // **不是**「真实的 4106 字符内容没出现」——那个强度在这份 fixture 上**拿不到**。
        //
        // 要拿到原强度，必须按设计文档 §3.3-B14 的正确命令（`[Console]::OutputEncoding` 切 UTF-8
        // 或用 cmd / Git Bash 重定向，**绝不能用 PowerShell 的 `>`**）重采一份**仅本地、不入库**
        // 的原始样本，把它临时放进 fixtures 再跑一遍本测试，跑完即删。
        //
        // 另外：断言必须**同时**覆盖 `output` 与 `stdout` 两个字段。只断言 `output` 就是重犯
        // 「一次人工脱敏挑错字段」那个错——第一次脱敏换掉的正是 `output` 那个 75 字符短摘要，
        // 而真正装着全文的 `stdout` 漏在里面。
        let mut decoder = new_decoder();
        let events = run_sample(&mut decoder, SAMPLE_A);

        // 先证明这条测试**不是在空集合上做的**：样本 A 确实产出了真帧。
        let json = frames_json(&events);
        assert!(
            json.contains("403 Request not allowed"),
            "样本 A 必须产出真实帧，否则本测试等于什么都没测"
        );

        // hook 行的两个内容字段，逐个断言。
        // **同时跑在 `started_json` 上**：`StartedInfo` 也是序列化后过网的东西，
        // 把它排除在断言范围之外就是本文件自己点名的那类漏洞（见 `started_json` 文档）。
        let started = started_json(&events);
        for needle in [
            // `stdout`：真正装着私人记忆库全文的那个字段。
            "REDACTED stdout",
            "4106 chars of local hook output",
            // `output`：短摘要字段（第一次脱敏只换了它）。
            "REDACTED output",
            "75 chars of local hook output",
            // 连 hook 的标识本身都不该出现在帧里。
            "SessionStart",
            "hook_name",
            "hook_id",
            "62b96fdd-773b-4838-85f6-af8ac77d6f15",
        ] {
            assert!(
                !json.contains(needle),
                "归一化帧里泄漏了 hook 内容：{needle}"
            );
            assert!(
                !started.contains(needle),
                "Started（片 4 的 LlmConvMeta）里泄漏了 hook 内容：{needle}"
            );
        }

        // 审计日志走的是 `Debug`，那条路径也得堵——`format!("{events:?}")` 是最容易被顺手写出来的。
        let debug = format!("{events:?}");
        for needle in ["REDACTED stdout", "REDACTED output", "SessionStart"] {
            assert!(
                !debug.contains(needle),
                "事件 Debug 里泄漏了 hook 内容：{needle}"
            );
        }

        // 同样的断言对样本 B 的 hook 行（`stdout` 4797 字符）也要成立。
        let mut decoder = new_decoder();
        let events_b = run_sample(&mut decoder, SAMPLE_B);
        let json_b = frames_json(&events_b);
        let started_b = started_json(&events_b);
        for needle in [
            "REDACTED stdout",
            "4797 chars",
            "REDACTED output",
            "42 chars",
        ] {
            assert!(
                !json_b.contains(needle),
                "样本 B 的归一化帧里泄漏了 hook 内容：{needle}"
            );
            assert!(
                !started_b.contains(needle),
                "样本 B 的 Started 里泄漏了 hook 内容：{needle}"
            );
        }
    }

    // ── 负向测试 ②：用户文件正文不出现在归一化帧里 ─────────────────────────

    #[test]
    fn 用户文件正文不出现在归一化帧里_只留元数据() {
        // 同样的强度说明：fixture 已脱敏，`tool_use_result.file.content` 那 11720 字符文件正文
        // 现在是一个占位符。本测试断言的是「连占位符都没被透传」。
        let mut decoder = new_decoder();
        let events = run_sample(&mut decoder, SAMPLE_B);
        let json = frames_json(&events);

        // ① `tool_use_result.file.content`（用户文件正文）**一个字节都不出现**。
        assert!(
            !json.contains("11720 chars of file body"),
            "file.content 是用户文件正文，绝不能上行"
        );
        // ② 元数据照常上行——这正是「结构化摘要」相对「一坨正文」的价值。
        assert!(json.contains("165"), "numLines/totalLines 应保留");
        // ③ `tool_result.content` 走的是**夹紧**通路而不是原样透传：这里占位符只有 24 字节，
        //    夹不掉任何东西，所以只能断言它经过了同一条 `Clamped` 通路（超限行为另有专测）。
        assert!(json.contains("<<REDACTED 12272 chars>>"));

        // ④ `filePath` 是本机绝对路径，落点必须是 `LlmPath`——它的 `Debug` 恒为
        //    `LlmPath(<redacted>)`，于是审计日志那条路径天然堵死。
        let frames = to_frames(&events);
        let debug = format!("{frames:?}");
        assert!(
            debug.contains("LlmPath(<redacted>)"),
            "路径必须经 LlmPath 包装"
        );
        assert!(
            !debug.contains("<<REDACTED>>"),
            "Debug 里不该出现路径原文（这里恰是占位符）"
        );
    }

    // ── Clamped：反序列化即夹紧 ─────────────────────────────────────────────

    #[test]
    fn clamped在反序列化时就夹紧且落在utf8字符边界上() {
        #[derive(Deserialize)]
        struct Holder {
            text: Clamped<8>,
        }
        // 每个汉字 3 字节：8 字节上限落在第 2 个字与第 3 个字之间，必须退到字符边界（6 字节）。
        let holder: Holder = serde_json::from_str(r#"{"text":"中文测试"}"#).unwrap();
        assert_eq!(holder.text.as_str(), "中文");
        assert_eq!(holder.text.truncated_bytes(), Some(6));
        assert!(holder
            .text
            .as_str()
            .is_char_boundary(holder.text.as_str().len()));

        // 恰好等于上限：不裁。
        let holder: Holder = serde_json::from_str(r#"{"text":"12345678"}"#).unwrap();
        assert_eq!(holder.text.as_str(), "12345678");
        assert_eq!(holder.text.truncated_bytes(), None);

        // 带转义的字符串走的是 serde_json 的 scratch 路径（`visit_str`），同样要夹紧。
        let holder: Holder = serde_json::from_str(r#"{"text":"a\nb\nc\nd\ne\nf"}"#).unwrap();
        assert_eq!(holder.text.as_str(), "a\nb\nc\nd\n", "11 字节裁到 8 字节");
        assert_eq!(holder.text.truncated_bytes(), Some(3));
    }

    #[test]
    fn clamped的debug只打长度不打正文() {
        let clamped = Clamped::<4>::clamp("sk-ant-super-secret");
        let rendered = format!("{clamped:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("kept"));
    }

    #[test]
    fn 超长工具结果被夹紧且截断字节数被声明() {
        let big = "x".repeat(LLM_TOOL_OUTPUT_MAX_BYTES + 1000);
        let line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}}}}"#
        );
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, &line);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!("应产出一个块");
        };
        let LlmBlock::ToolResult {
            output,
            truncated_bytes,
            ..
        } = &entry.block
        else {
            panic!("应为 ToolResult");
        };
        assert_eq!(output.len_bytes(), LLM_TOOL_OUTPUT_MAX_BYTES);
        assert_eq!(*truncated_bytes, Some(1000));
    }

    // ── 工具入参夹紧 ────────────────────────────────────────────────────────

    #[test]
    fn 超长工具入参保留块只裁值并声明截断字节数() {
        // 模拟一次 `Write`：入参里装着整份文件正文。
        let huge = "y".repeat(LLM_TOOL_INPUT_MAX_BYTES * 2);
        let value = serde_json::json!({"file_path": "F:\\a\\b.rs", "content": huge});
        let (input, truncated) = clamp_tool_input(value);
        assert!(truncated.is_some_and(|n| n > LLM_TOOL_INPUT_MAX_BYTES as u64));
        // **块没被丢**：小键原样保留，只有超长值被换成我们自己造的短占位。
        assert_eq!(
            input.get().get("file_path").and_then(|v| v.as_str()),
            Some("F:\\a\\b.rs")
        );
        let content = input.get().get("content").and_then(|v| v.as_str()).unwrap();
        assert!(content.starts_with("<<"), "{content}");
        assert!(!content.contains("yyyy"), "上游正文不得残留");
        assert!(json_len(input.get()) <= LLM_TOOL_INPUT_MAX_BYTES);
    }

    #[test]
    fn 非对象形态的超长入参整体清空而不是丢块() {
        let value = serde_json::Value::String("z".repeat(LLM_TOOL_INPUT_MAX_BYTES * 2));
        let (input, truncated) = clamp_tool_input(value);
        assert!(input
            .get()
            .as_object()
            .is_some_and(serde_json::Map::is_empty));
        assert!(truncated.is_some());
    }

    // ── 铁律二：先判 is_api_error_message ────────────────────────────────────

    #[test]
    fn 同样的文本没有错误标志时是普通气泡_有标志时是错误块() {
        let text = "Failed to authenticate. API Error: 403 Request not allowed";
        let normal = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        );
        let flagged = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}},"error":"authentication_failed","is_api_error_message":true}}"#
        );

        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, &normal);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        assert!(
            matches!(entry.block, LlmBlock::Text { .. }),
            "没有标志位就是普通气泡"
        );

        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, &flagged);
        assert_eq!(events.len(), 1, "错误路径下不再重复产出一个文本气泡");
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        assert!(matches!(entry.block, LlmBlock::Error { .. }));
    }

    #[test]
    fn 认不出的错误标识落other而不是硬塞成io() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"boom"}]},"error":"brand_new_failure","is_api_error_message":true}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        let LlmBlock::Error { code, .. } = &entry.block else {
            panic!()
        };
        assert!(!code.is_known());
        assert_eq!(code.as_wire(), "brand_new_failure");
    }

    // ── 未知块类型 / 未知行 ─────────────────────────────────────────────────

    #[test]
    fn 未知content块类型被跳过_同一行其余块照常产出() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"video","url":"x"},{"type":"text","text":"hi"},{"type":"brand_new","payload":{"a":1}}]}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        assert_eq!(events.len(), 1, "只剩那个 text 块");
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        assert_eq!(
            entry.block,
            LlmBlock::Text {
                text: LlmText::new("hi")
            }
        );
    }

    #[test]
    fn 权限审批通道刻意不建模_响亮报unmodeled而不是猜一个形状() {
        let line = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash"}}"#;
        let LineVerdict::Parse(env) = classify(line) else {
            panic!("白名单里有 control_request")
        };
        let mut decoder = new_decoder();
        let mut out = Vec::new();
        let err = decoder.decode_line(&env, line, &mut out).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::Unmodeled);
        assert!(out.is_empty(), "不得凭空造一条审批事件");
    }

    // ── control_response ────────────────────────────────────────────────────

    #[test]
    fn control应答两种嵌套都认_认不出的subtype一律当失败() {
        let mut decoder = new_decoder();
        let nested = r#"{"type":"control_response","response":{"request_id":"lumen-1","subtype":"success"}}"#;
        let events = decode_one(&mut decoder, nested);
        assert_eq!(
            events[0],
            RunnerEvent::ControlAck {
                request_id: "lumen-1".into(),
                ok: true
            }
        );

        let flat = r#"{"type":"control_response","request_id":"lumen-2","subtype":"error"}"#;
        let events = decode_one(&mut decoder, flat);
        assert_eq!(
            events[0],
            RunnerEvent::ControlAck {
                request_id: "lumen-2".into(),
                ok: false
            }
        );

        // 认不出的 subtype 必须按失败处理：中断按钮宁可报「没送到」，也不能报「送到了」而实际没有。
        let weird = r#"{"type":"control_response","request_id":"lumen-3","subtype":"queued"}"#;
        let events = decode_one(&mut decoder, weird);
        assert_eq!(
            events[0],
            RunnerEvent::ControlAck {
                request_id: "lumen-3".into(),
                ok: false
            }
        );
    }

    #[test]
    fn control应答取不到request_id时报unmodeled而不是编一个() {
        let line = r#"{"type":"control_response","response":{"status":"ok"}}"#;
        let LineVerdict::Parse(env) = classify(line) else {
            panic!()
        };
        let mut decoder = new_decoder();
        let mut out = Vec::new();
        let err = decoder.decode_line(&env, line, &mut out).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::Unmodeled);
        assert!(out.is_empty());
    }

    // ── result：口径与映射 ──────────────────────────────────────────────────

    #[test]
    fn 上下文占用取本轮最后一次调用而不是整轮累加() {
        // 这条测试守的是「进度条不会超过 100%」：`result.usage` 是整轮累加，
        // 一轮 10 次工具调用累加出来的 input_tokens 是同一份上下文被数了 10 遍。
        let (mut encoder, mut decoder) = split_pair();
        encoder.encode_user_message("hi", &[]).unwrap();

        for (input, cache_read, cache_create) in [(1u64, 20_327u64, 13_229u64), (2, 33_556, 4_784)]
        {
            let line = format!(
                r#"{{"type":"assistant","message":{{"model":"claude-opus-5","role":"assistant","content":[],"usage":{{"input_tokens":{input},"cache_read_input_tokens":{cache_read},"cache_creation_input_tokens":{cache_create}}}}}}}"#
            );
            let _ = decode_one(&mut decoder, &line);
        }
        // 整轮累加值（3 / 53883 / 18013）与「最后一次」（2 + 33556 + 4784 = 38342）刻意不同。
        let result = r#"{"type":"result","is_error":false,"stop_reason":"end_turn","terminal_reason":"completed","total_cost_usd":0.24026150000000002,"usage":{"input_tokens":3,"output_tokens":24,"cache_read_input_tokens":53883,"cache_creation_input_tokens":18013},"modelUsage":{"claude-opus-5":{"contextWindow":1000000,"maxOutputTokens":64000,"costUSD":0.24,"canonicalModel":"x","provider":"y"}},"permission_denials":[],"result":"done"}"#;
        let events = decode_one(&mut decoder, result);
        let RunnerEvent::TurnEnded(info) = &events[0] else {
            panic!()
        };
        assert_eq!(info.usage.context_used, 38_342, "必须是最后一次，不是累加");
        assert_eq!(info.usage.input_tokens, 3, "账务口径仍是整轮累加");
        assert_eq!(info.usage.context_limit, 1_000_000);
        assert_eq!(info.usage.max_output_tokens, 64_000);
        // 向上取整：宁可多报 1e-6 美元也不虚报 0。
        assert_eq!(info.usage.cost_micro_usd, 240_262);
        assert_eq!(
            info.outcome.terminal_reason,
            Some(LlmTerminalReason::Completed)
        );
        assert!(!info.outcome.is_error);
        assert_eq!(info.stop, LlmStopReason::EndTurn);
    }

    #[test]
    fn modelusage只摘两个数字_其余账务字段一个都不上行() {
        let (mut encoder, mut decoder) = split_pair();
        encoder.encode_user_message("hi", &[]).unwrap();
        let result = r#"{"type":"result","is_error":false,"modelUsage":{"claude-opus-5":{"contextWindow":200000,"maxOutputTokens":32000,"costUSD":9.99,"canonicalModel":"secret-model","provider":"secret-provider","webSearchRequests":7}}}"#;
        let events = decode_one(&mut decoder, result);
        let json = frames_json(&events);
        for needle in [
            "secret-model",
            "secret-provider",
            "9.99",
            "webSearchRequests",
        ] {
            assert!(!json.contains(needle), "modelUsage 里的 {needle} 不该上行");
        }
        assert!(json.contains("200000") && json.contains("32000"));
    }

    #[test]
    fn modelusage多条且模型名对不上时报未知而不是求和或取最大() {
        let (mut encoder, mut decoder) = split_pair();
        encoder.encode_user_message("hi", &[]).unwrap();
        let result = r#"{"type":"result","is_error":false,"modelUsage":{"model-a":{"contextWindow":200000,"maxOutputTokens":8000},"model-b":{"contextWindow":1000000,"maxOutputTokens":64000}}}"#;
        let events = decode_one(&mut decoder, result);
        let RunnerEvent::TurnEnded(info) = &events[0] else {
            panic!()
        };
        assert_eq!(
            (info.usage.context_limit, info.usage.max_output_tokens),
            (0, 0),
            "0 = 未知，手机端不显示百分比；求和/取最大都会给出一个不存在的分母"
        );
    }

    #[test]
    fn permission_denials只转条数_元素内容一个字节都不上行() {
        let (mut encoder, mut decoder) = split_pair();
        encoder.encode_user_message("hi", &[]).unwrap();
        let result = r#"{"type":"result","is_error":false,"permission_denials":[{"tool":"Bash","command":"rm -rf /home/secret"},{"tool":"Write"}]}"#;
        let events = decode_one(&mut decoder, result);
        let RunnerEvent::TurnEnded(info) = &events[0] else {
            panic!()
        };
        assert_eq!(info.denials, 2);
        assert!(!format!("{events:?}").contains("secret"));
    }

    #[test]
    fn 成本非法值一律按未知而不是编一个数() {
        assert_eq!(cost_micro_usd(None), 0);
        assert_eq!(cost_micro_usd(Some(f64::NAN)), 0);
        assert_eq!(cost_micro_usd(Some(-1.0)), 0);
        assert_eq!(cost_micro_usd(Some(f64::INFINITY)), 0);
        // 0.9 µUSD 不能被截断成 0（免费）。
        assert_eq!(cost_micro_usd(Some(0.000_000_9)), 1);
    }

    #[test]
    fn 缺stop_reason时按is_error推导而不是留空() {
        let (mut encoder, mut decoder) = split_pair();
        encoder.encode_user_message("hi", &[]).unwrap();
        let events = decode_one(&mut decoder, r#"{"type":"result","is_error":true}"#);
        let RunnerEvent::TurnEnded(info) = &events[0] else {
            panic!()
        };
        assert_eq!(info.stop, LlmStopReason::Error);

        let events = decode_one(&mut decoder, r#"{"type":"result","is_error":false}"#);
        let RunnerEvent::TurnEnded(info) = &events[0] else {
            panic!()
        };
        assert_eq!(info.stop, LlmStopReason::EndTurn);
    }

    // ── rate_limit_event 的边界 ─────────────────────────────────────────────

    #[test]
    fn 限流事件缺status时报schema而不是编一个other空串() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"resetsAt":1786211400}}"#;
        let LineVerdict::Parse(env) = classify(line) else {
            panic!()
        };
        let mut decoder = new_decoder();
        let mut out = Vec::new();
        let err = decoder.decode_line(&env, line, &mut out).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::Schema);
        assert!(out.is_empty());
    }

    #[test]
    fn 垃圾resets_at按未知处理而不是渲染成1970年() {
        assert_eq!(sanitize_resets_at(None), None);
        assert_eq!(sanitize_resets_at(Some(0)), None);
        assert_eq!(sanitize_resets_at(Some(-1)), None);
        // 换算成毫秒会溢出 i64 ⇒ 视为垃圾值。
        assert_eq!(sanitize_resets_at(Some(i64::MAX)), None);
        assert_eq!(sanitize_resets_at(Some(1_786_211_400)), Some(1_786_211_400));
    }

    #[test]
    fn 上游没给isusingoverage时保持none而不是压成false() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::RateLimit { info } = &events[0] else {
            panic!()
        };
        assert_eq!(
            info.using_overage, None,
            "「没说超额」不等于「没在超额」——压成 false 会给出错误的计费提示"
        );
        assert_eq!(info.window, None);
        assert_eq!(info.overage_status, None);
    }

    // ── tool_use_result 的保守规则 ──────────────────────────────────────────

    #[test]
    fn 未建模的tool_use_result形态一律不产出摘要() {
        // `Bash` / `Edit` / `Write` 的形态完全未知：只要没有 `file` 键就退回 `None`，
        // 手机端按 output 渲染纯文本，而不是拿一个我们臆造的结构去画卡片。
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"tool_use_result":{"type":"bash","stdout":"hello","exitCode":0}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        let LlmBlock::ToolResult { detail, .. } = &entry.block else {
            panic!()
        };
        assert!(detail.is_none());
        assert!(
            !format!("{events:?}").contains("hello"),
            "未建模字段不得泄漏"
        );
    }

    // ── 宽进：一个字段解不动**不许连累同行其它块** ───────────────────────────
    //
    // `adapter.rs` 把这条写成了 trait 契约（「一行里有 3 个 content block、第 2 个解不动时，
    // 正确做法是把第 1、3 个照常产出」），但初稿把两个**形状未知**的字段硬绑成了具体类型，
    // 而 serde 的 map 值失败是整行级的 ⇒「上游多一种块形态」= 整行内容全丢。

    #[test]
    fn content是数组的块不许连累同行的正文块() {
        // `web_search_tool_result` / `code_execution_tool_result` / `mcp_tool_result` /
        // `document` / `search_result` 的 `content` 都是**数组**；两份样本的
        // `usage.server_tool_use.web_search_requests` 键都存在，证明这条通道是开着的。
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"前"},{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"t","url":"https://example.invalid"}]},{"type":"text","text":"后"}]}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                RunnerEvent::Block { entry, .. } => match &entry.block {
                    LlmBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["前", "后"],
            "第 2 个块解不动时，第 1、3 个必须照常产出"
        );
        // 数组内容一个字节都不许物化出来。
        let json = frames_json(&events);
        assert!(!json.contains("example.invalid"));
        assert!(!json.contains("web_search_result"));
    }

    #[test]
    fn tool_result的content是数组时_块照常产出且响亮报错() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"里面"}]}]}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);

        // ① 卡片必须产出——否则这个 call_id 成孤儿，手机端永远等不到结果。
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!("tool_result 块必须照常产出，实际 {events:?}")
        };
        let LlmBlock::ToolResult {
            call_id, output, ..
        } = &entry.block
        else {
            panic!()
        };
        assert_eq!(call_id, "t1");
        assert!(output.as_str().is_empty());

        // ② 但必须**响亮**（§14-6 禁止无声降级）：这条协议错误就是「去补采样」的信号。
        assert_eq!(
            events[1],
            RunnerEvent::ProtocolError {
                kind: ProtocolErrorKind::DecodeFailed {
                    at: "user.message.content.tool_result.content"
                }
            }
        );
        assert!(!format!("{events:?}").contains("里面"), "数组内容不得物化");
    }

    #[test]
    fn message的content是纯字符串时归一成一个text块() {
        // 公开 Messages API 允许 `"content": "..."`，初稿硬绑 `Vec<ContentBlock>` ⇒ 整行全灭。
        let line =
            r#"{"type":"assistant","message":{"role":"assistant","content":"纯字符串正文"}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!("必须产出一个块，实际 {events:?}")
        };
        let LlmBlock::Text { text } = &entry.block else {
            panic!()
        };
        assert_eq!(text.as_str(), "纯字符串正文");
    }

    #[test]
    fn tool_use_result非对象时_工具结果块仍然产出() {
        // 本文件自己写着「Bash/Edit/Write 的 tool_use_result 形态完全未知」——既然未知，
        // 就不能把它硬绑成一个具体的 struct 形状。初稿那条测试用的 payload 恰好是对象，
        // 所以测不出这个洞。
        for payload in [
            r#""plain string""#,
            "[1,2]",
            "42",
            "true",
            "null",
            r#"{"file":"不是对象"}"#,
        ] {
            let line = format!(
                r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok"}}]}},"tool_use_result":{payload}}}"#
            );
            let mut decoder = new_decoder();
            let events = decode_one(&mut decoder, &line);
            let RunnerEvent::Block { entry, .. } = &events[0] else {
                panic!("payload={payload} 时 ToolResult 块必须仍然产出，实际 {events:?}")
            };
            let LlmBlock::ToolResult {
                call_id,
                output,
                detail,
                ..
            } = &entry.block
            else {
                panic!()
            };
            assert_eq!(call_id, "t1");
            assert_eq!(output.as_str(), "ok", "同一行的 content 不该被连累");
            assert!(detail.is_none(), "形态未建模时摘要退成 None");
        }
    }

    #[test]
    fn 标识类字段被夹紧_越限也仍然配得上对() {
        let long = "t".repeat(IDENT_MAX_BYTES * 4);
        let use_line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"{long}","name":"{long}","input":{{}}}}]}}}}"#
        );
        let result_line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{long}","content":"ok"}}]}}}}"#
        );
        let mut decoder = new_decoder();
        let a = decode_one(&mut decoder, &use_line);
        let b = decode_one(&mut decoder, &result_line);
        let (RunnerEvent::Block { entry: ua, .. }, RunnerEvent::Block { entry: ub, .. }) =
            (&a[0], &b[0])
        else {
            panic!()
        };
        let (
            LlmBlock::ToolUse {
                call_id: id_a,
                name,
                ..
            },
            LlmBlock::ToolResult { call_id: id_b, .. },
        ) = (&ua.block, &ub.block)
        else {
            panic!()
        };
        assert_eq!(id_a.len(), IDENT_MAX_BYTES);
        assert_eq!(name.len(), IDENT_MAX_BYTES);
        assert_eq!(id_a, id_b, "两端一致地夹紧 ⇒ 越限也仍然配得上对");
    }

    #[test]
    fn 轮号回滚必须撤销编码时的自增() {
        // `LlmRunner::submit` 在「编码成功但送不进 stdin 队列」时要把**两个**计数器一起退回去
        // （见 `submit` 的 `# 送不出去时必须把两个计数器一起回滚`）。这里守的是本适配器这一半：
        // `rollback_turn` 必须与 `encode_user_message` 的 `fetch_add` 严格对称。
        let (mut encoder, decoder) = split_pair();
        assert_eq!(decoder.current_turn(), 0);
        encoder.encode_user_message("hi", &[]).expect("编码成功");
        assert_eq!(decoder.current_turn(), 1);
        encoder.rollback_turn();
        assert_eq!(
            decoder.current_turn(),
            0,
            "不回滚就会烧掉一个轮号：CLI 从没收过这条消息，手机端的 turn 序列会出现永久空洞"
        );
        // 回滚后再发一条，拿到的仍然是 1（轮号没被浪费）。
        encoder.encode_user_message("hi", &[]).expect("编码成功");
        assert_eq!(decoder.current_turn(), 1);
    }

    #[test]
    fn 合成消息的哨兵模型名不得污染窗口挑选() {
        // 样本 A 第 8 行实测 `message.model = "<synthetic>"`（认证失败播报），
        // 而 `last_message_model` **跨轮不重置**：不过滤就会被永久污染。
        let mut decoder = new_decoder();
        decode_one(
            &mut decoder,
            r#"{"type":"assistant","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"x"}]},"is_api_error_message":true,"error":"authentication_failed"}"#,
        );
        decode_one(
            &mut decoder,
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"y"}]}}"#,
        );
        let events = decode_one(
            &mut decoder,
            r#"{"type":"result","is_error":false,"modelUsage":{"claude-opus-5":{"contextWindow":1000000,"maxOutputTokens":64000},"claude-haiku-4-5":{"contextWindow":200000,"maxOutputTokens":8192}}}"#,
        );
        let RunnerEvent::TurnEnded(info) = &events[0] else {
            panic!()
        };
        assert_eq!(
            (info.usage.context_limit, info.usage.max_output_tokens),
            (1_000_000, 64_000),
            "多于一项 modelUsage 时必须靠模型名挑中主模型，而不是被 <synthetic> 顶成 (0,0)"
        );
    }

    #[test]
    fn result正文被夹紧时必须声明裁掉了多少字节() {
        let long = "阿".repeat(LLM_TOOL_OUTPUT_MAX_BYTES); // 每字 3 字节，必定越限
        let line = format!(r#"{{"type":"result","is_error":false,"result":"{long}"}}"#);
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, &line);
        let RunnerEvent::TurnEnded(info) = &events[0] else {
            panic!()
        };
        let truncated = info
            .result_truncated_bytes
            .expect("越限却没有声明裁掉多少 = 静默截断，§14-6 禁止");
        let kept = info.result_text.as_ref().expect("正文仍要有").len_bytes();
        assert!(kept <= LLM_TOOL_OUTPUT_MAX_BYTES);
        assert_eq!(kept as u64 + truncated, long.len() as u64);
    }

    #[test]
    fn file摘要缺任何一个数字都整体退成none() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"tool_use_result":{"type":"text","file":{"filePath":"F:\\x","numLines":10,"startLine":1}}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        let LlmBlock::ToolResult { detail, .. } = &entry.block else {
            panic!()
        };
        assert!(
            detail.is_none(),
            "缺 totalLines 就得整体退回 None——填 0 会渲染成「共 0 行」这种我们编的信息"
        );
    }

    #[test]
    fn 一行多个tool_result时不把摘要张冠李戴() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a"},{"type":"tool_result","tool_use_id":"t2","content":"b"}]},"tool_use_result":{"type":"text","file":{"filePath":"F:\\x","numLines":10,"startLine":1,"totalLines":10}}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        assert_eq!(events.len(), 2);
        for event in &events {
            let RunnerEvent::Block { entry, .. } = event else {
                panic!()
            };
            let LlmBlock::ToolResult { detail, .. } = &entry.block else {
                panic!()
            };
            assert!(detail.is_none(), "上游没说摘要属于哪一个，就一个都不挂");
        }
    }

    #[test]
    fn 工具结果的is_error为真时状态是error() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true}]}}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        let LlmBlock::ToolResult { status, .. } = &entry.block else {
            panic!()
        };
        assert_eq!(*status, LlmToolStatus::Error);
    }

    // ── 嵌套归属 ────────────────────────────────────────────────────────────

    #[test]
    fn parent_tool_use_id落到块条目的parent_call_id上() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"sub"}]},"parent_tool_use_id":"toolu_parent"}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::Block { entry, .. } = &events[0] else {
            panic!()
        };
        assert_eq!(entry.parent_call_id.as_deref(), Some("toolu_parent"));
    }

    // ── 轮号：编解码两半共享同一份计数 ──────────────────────────────────────

    #[test]
    fn 编码一条用户消息后解码侧立刻看到新轮号且块号重置() {
        let (mut encoder, mut decoder) = split_pair();
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#;

        encoder.encode_user_message("第一轮", &[]).unwrap();
        let events = decode_one(&mut decoder, line);
        let turns: Vec<u64> = events
            .iter()
            .map(|e| match e {
                RunnerEvent::Block { turn, .. } => *turn,
                _ => panic!(),
            })
            .collect();
        assert_eq!(turns, vec![1, 1]);
        let ids: Vec<u32> = events
            .iter()
            .map(|e| match e {
                RunnerEvent::Block { entry, .. } => entry.block_id,
                _ => panic!(),
            })
            .collect();
        assert_eq!(ids, vec![0, 1]);

        encoder.encode_user_message("第二轮", &[]).unwrap();
        let events = decode_one(&mut decoder, line);
        let RunnerEvent::Block { turn, entry } = &events[0] else {
            panic!()
        };
        assert_eq!(*turn, 2);
        assert_eq!(entry.block_id, 0, "块号必须按轮重置");
    }

    #[test]
    fn 编码失败时轮号不推进() {
        // `StdinLine::from_json` 对合法输入不会失败，这里直接验证「成功一次加一次」这个不变量：
        // `LlmRunner::submit` 的顺序是「先 encode、再 fetch_add」，两个计数器必须逐条对齐。
        let (mut encoder, decoder) = split_pair();
        assert_eq!(decoder.current_turn(), 0);
        encoder.encode_user_message("x", &[]).unwrap();
        assert_eq!(decoder.current_turn(), 1);
        encoder.encode_control(&ControlCmd::Interrupt).unwrap();
        assert_eq!(decoder.current_turn(), 1, "control 不推进轮号");
    }

    // ── 出站编码 ────────────────────────────────────────────────────────────

    #[test]
    fn 用户消息是一整行且多行正文被转义() {
        let (mut encoder, _decoder) = split_pair();
        let line = encoder.encode_user_message("第一行\n第二行", &[]).unwrap();
        let bytes = line.as_bytes();
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        let value: serde_json::Value =
            serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("必须是合法 JSON");
        assert_eq!(value["type"], "user");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"][0]["type"], "text");
        assert_eq!(value["message"]["content"][0]["text"], "第一行\n第二行");
    }

    #[test]
    fn 附件被编成提示词里的路径映射() {
        let (mut encoder, _decoder) = split_pair();
        let attachment = LlmAttachment {
            path: LlmPath::new("F:\\tmp\\a.png"),
            name: LlmText::new("图1"),
            mime: "image/png".into(),
            len: 10,
        };
        let line = encoder
            .encode_user_message("看这张图", &[attachment])
            .unwrap();
        let bytes = line.as_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        let text = value["message"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("看这张图"));
        assert!(text.contains("图1 = \"F:\\tmp\\a.png\""), "{text}");
    }

    #[test]
    fn control请求形状与自增request_id() {
        let (mut encoder, _decoder) = split_pair();
        for (cmd, subtype) in [
            (ControlCmd::Initialize, "initialize"),
            (ControlCmd::Interrupt, "interrupt"),
        ] {
            let line = encoder.encode_control(&cmd).unwrap();
            let bytes = line.as_bytes();
            let value: serde_json::Value =
                serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
            assert_eq!(value["type"], "control_request");
            assert_eq!(value["request"]["subtype"], subtype);
            assert!(value["request_id"].as_str().unwrap().starts_with("lumen-"));
            assert!(
                value["request"].get("mode").is_none(),
                "非 set 指令不带 mode"
            );
        }
        let line = encoder
            .encode_control(&ControlCmd::SetPermissionMode(
                LlmPermissionMode::AcceptEdits,
            ))
            .unwrap();
        let bytes = line.as_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(value["request"]["subtype"], "set_permission_mode");
        assert_eq!(value["request"]["mode"], "acceptEdits");
        assert_eq!(value["request_id"], "lumen-3");
    }

    #[test]
    fn bypass与无对应模式的权限指令在编码层被拒() {
        let (mut encoder, _decoder) = split_pair();
        for mode in [
            LlmPermissionMode::Bypass,
            LlmPermissionMode::Auto,
            LlmPermissionMode::DontAsk,
            LlmPermissionMode::from_wire_clamped("brand_new_mode"),
        ] {
            assert_eq!(
                encoder.encode_control(&ControlCmd::SetPermissionMode(mode)),
                Err(EncodeError::Unsupported),
                "编不出来比编一个最近似的安全"
            );
        }
        // 权限审批通道未验证 ⇒ 不发。
        assert_eq!(
            encoder.encode_control(&ControlCmd::PermissionReply {
                request_id: "r1".into(),
                allow: true,
                reason: None,
            }),
            Err(EncodeError::Unsupported)
        );
    }

    #[test]
    fn 编码器的debug不打正文() {
        let (encoder, _decoder) = split_pair();
        let rendered = format!("{encoder:?}");
        assert!(rendered.contains("ClaudeEncoder"));
        assert!(!rendered.contains("inflight"), "在途表不该被 Debug 打出来");
    }

    // ── 命令行参数 ──────────────────────────────────────────────────────────

    fn spec() -> LaunchSpec {
        LaunchSpec::new(
            LlmAgentKind::Claude,
            PathBuf::from("F:\\ws"),
            "sid-1".to_owned(),
        )
    }

    #[test]
    fn 参数含stream_json三件套与围栏_且不含未验证的开关() {
        let args = ClaudeAdapter::new().build_args(&spec());
        let joined = args.join(" ");
        assert!(joined.contains("-p"));
        assert!(joined.contains("--input-format stream-json"));
        assert!(joined.contains("--output-format stream-json"));
        assert!(joined.contains("--verbose"));
        assert!(joined.contains("--session-id sid-1"));
        assert!(joined.contains("--add-dir F:\\ws"));
        // 两个刻意不传的开关，见 `build_args` 文档。
        assert!(!joined.contains("--replay-user-messages"));
        assert!(!joined.contains("--disallowedTools"));
    }

    #[test]
    fn resume与session_id互斥() {
        let mut spec = spec();
        spec.resume = true;
        spec.fork_session = true;
        let args = ClaudeAdapter::new().build_args(&spec).join(" ");
        assert!(args.contains("--resume sid-1"));
        assert!(args.contains("--fork-session"));
        assert!(!args.contains("--session-id"));
    }

    #[test]
    fn 权限模式与可选参数按需追加() {
        let mut spec = spec();
        spec.set_permission_mode(LlmPermissionMode::AcceptEdits)
            .unwrap();
        spec.model = Some("claude-opus-5".into());
        spec.append_system_prompt = Some(LlmText::new("守规矩"));
        spec.allowed_tools = Some(vec!["Read".into(), "Grep".into()]);
        let args = ClaudeAdapter::new().build_args(&spec).join(" ");
        assert!(args.contains("--permission-mode acceptEdits"));
        assert!(args.contains("--model claude-opus-5"));
        assert!(args.contains("--append-system-prompt 守规矩"));
        assert!(args.contains("--allowedTools Read,Grep"));

        // 空的 allowed_tools 不传：传空串可能被解释成「一个都不许用」。
        spec.allowed_tools = Some(Vec::new());
        let args = ClaudeAdapter::new().build_args(&spec).join(" ");
        assert!(!args.contains("--allowedTools"));
    }

    #[test]
    fn p0不下发任何环境变量() {
        assert!(ClaudeAdapter::new().build_env(&spec()).is_empty());
    }

    // ── init 的容错 ─────────────────────────────────────────────────────────

    #[test]
    fn capabilities形状变了也不能打死整条init() {
        // 能力探测是 init 唯一不可替代的用途；解析失败 = runner 永远卡在 Booting。
        for caps in [
            r#""capabilities":"not-a-list","#,
            r#""capabilities":[{"name":"x"},"interrupt_receipt_v1"],"#,
            r#""capabilities":[],"#,
            "",
        ] {
            let line = format!(
                r#"{{"type":"system","subtype":"init","session_id":"s","model":"m","cwd":"F:\\x","permissionMode":"acceptEdits","claude_code_version":"2.1.226",{caps}"memory_paths":{{"a":"b"}}}}"#
            );
            let mut decoder = new_decoder();
            let events = decode_one(&mut decoder, &line);
            let RunnerEvent::Started(info) = &events[0] else {
                panic!("init 必须产出 Started")
            };
            assert_eq!(info.cli_version.as_deref(), Some("2.1.226"));
            // 只有那条含 interrupt_receipt_v1 的才点亮 interrupt 位。
            assert_eq!(info.features.interrupt, caps.contains(CAP_INTERRUPT));
        }
    }

    #[test]
    fn init的本机环境字段一个都不出现在started里() {
        // 键与值都要断言：`memory_paths` 的键是文件名、值是**绝对路径**，两者都是本机信息。
        let line = r#"{"type":"system","subtype":"init","session_id":"s","cwd":"F:\\x","model":"m","tools":["Bash","Read"],"memory_paths":{"CLAUDE.md":"F:\\secret\\notes"},"mcp_servers":{"lumen-mcp":"stdio"},"plugins":["plugin-alpha"],"slash_commands":["/x"],"agents":["a"],"skills":["s"],"apiKeySource":"none","output_style":"concise"}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let debug = format!("{events:?}");
        // **关键**：这一份才是真正过网的东西。`StartedInfo.cwd` 是 `LlmPath`，`Debug` 恒为
        // `LlmPath(<redacted>)` ⇒ 只断言 Debug 的话，任何被塞进脱敏包装的本机信息都能
        // 同时通过 Debug 断言并原样上网。
        let started = started_json(&events);
        for needle in [
            "Bash",
            "Read",
            "CLAUDE.md",
            "secret",
            "notes",
            "mcp",
            "lumen-mcp",
            "plugin-alpha",
            "apiKeySource",
            "output_style",
            "/x",
        ] {
            assert!(
                !debug.contains(needle),
                "init 的环境字段泄漏进 Debug：{needle}"
            );
            assert!(
                !started.contains(needle),
                "init 的环境字段泄漏进序列化后的 Started：{needle}"
            );
        }
        // 反向条件：这条断言不能是在空串上做的——白名单内的那几项必须真的在里面。
        assert!(started.contains("2.1.226") || started.contains("\"m\""));
    }

    #[test]
    fn started序列化后只带白名单内的六项() {
        let line = r#"{"type":"system","subtype":"init","session_id":"sid-1","cwd":"F:\\work","model":"claude-opus-5[1m]","permissionMode":"acceptEdits","claude_code_version":"2.1.226","capabilities":["interrupt_receipt_v1"]}"#;
        let mut decoder = new_decoder();
        let events = decode_one(&mut decoder, line);
        let started = started_json(&events);
        // 六项白名单：cli_session_id / model / cwd / permission_mode / cli_version / features。
        for needle in [
            "sid-1",
            "claude-opus-5[1m]",
            "F:\\\\work",
            "AcceptEdits",
            "2.1.226",
        ] {
            assert!(
                started.contains(needle),
                "白名单内的 {needle} 必须真的过网（否则这条负向测试是在空集合上做的）"
            );
        }
    }

    // ── system/status（⚠ 未实测分支）────────────────────────────────────────

    #[test]
    fn status行两种键名都认_取不到时报unmodeled() {
        let mut decoder = new_decoder();
        for line in [
            r#"{"type":"system","subtype":"status","permissionMode":"plan"}"#,
            r#"{"type":"system","subtype":"status","permission_mode":"plan"}"#,
        ] {
            let events = decode_one(&mut decoder, line);
            assert_eq!(
                events[0],
                RunnerEvent::PermissionModeChanged {
                    mode: LlmPermissionMode::Plan
                }
            );
        }
        let line = r#"{"type":"system","subtype":"status","state":"busy"}"#;
        let LineVerdict::Parse(env) = classify(line) else {
            panic!()
        };
        let mut out = Vec::new();
        let err = decoder.decode_line(&env, line, &mut out).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::Unmodeled);
    }

    // ── 占位适配器 ──────────────────────────────────────────────────────────

    #[test]
    fn 占位适配器能力全false且编解码一律拒绝() {
        for adapter in [
            PlaceholderAdapter::CODEX,
            PlaceholderAdapter::GEMINI,
            PlaceholderAdapter::KIMI,
        ] {
            assert_eq!(adapter.baseline_features(), LlmAgentFeatures::default());
            assert!(adapter.build_args(&spec()).is_empty());
            assert!(adapter.build_env(&spec()).is_empty());
            let (mut encoder, mut decoder) = adapter.split();
            assert_eq!(
                encoder.encode_user_message("hi", &[]),
                Err(EncodeError::Unsupported)
            );
            assert_eq!(
                encoder.encode_control(&ControlCmd::Interrupt),
                Err(EncodeError::Unsupported)
            );
            let line = r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#;
            let LineVerdict::Parse(env) = classify(line) else {
                panic!()
            };
            let mut out = Vec::new();
            assert!(decoder.decode_line(&env, line, &mut out).is_err());
        }
        assert_eq!(PlaceholderAdapter::CODEX.kind(), LlmAgentKind::Codex);
        assert_eq!(PlaceholderAdapter::GEMINI.kind(), LlmAgentKind::Gemini);
        assert_eq!(PlaceholderAdapter::KIMI.kind(), LlmAgentKind::Kimi);
    }

    // ── 映射表本身 ──────────────────────────────────────────────────────────

    #[test]
    fn 上游蛇形值必须先查映射表再落other() {
        // 直接拿上游 snake_case 喂 `from_wire_clamped` 会让本端明明认识的值静默落进 `Other`，
        // UI 从此走「未知值」分支——不报错、只长期失真。这条测试守的就是「先查表」。
        assert_eq!(
            terminal_reason_from_wire("completed"),
            LlmTerminalReason::Completed
        );
        assert!(!LlmTerminalReason::from_wire_clamped("completed").is_known());

        assert_eq!(stop_reason_from_wire("end_turn"), LlmStopReason::EndTurn);
        assert!(!LlmStopReason::from_wire_clamped("end_turn").is_known());

        assert_eq!(
            permission_mode_from_wire("acceptEdits"),
            LlmPermissionMode::AcceptEdits
        );
        assert_eq!(
            rate_limit_window_from_wire("five_hour"),
            LlmRateLimitWindow::FiveHour
        );
        assert_eq!(
            overage_reason_from_wire("out_of_credits"),
            LlmOverageDisabledReason::OutOfCredits
        );
        assert_eq!(
            error_code_from_wire(ERROR_AUTH_FAILED),
            LlmErrorCode::AuthRequired
        );
    }

    #[test]
    fn 实测到但未映射的能力标记列表与实测值一致() {
        // 这条测试的作用是**留痕**：这两个标记我们看见了、是有意没映射，不是漏了。
        assert_eq!(CAP_OBSERVED_UNMAPPED.len(), 2);
        assert!(CAP_OBSERVED_UNMAPPED.contains(&"interrupt_cancel_queued_v1"));
        assert!(CAP_OBSERVED_UNMAPPED.contains(&"msg_lifecycle_v1"));
        assert!(SAMPLE_A.contains(CAP_INTERRUPT));
        for cap in CAP_OBSERVED_UNMAPPED {
            assert!(SAMPLE_A.contains(cap), "{cap} 应在样本里");
        }
    }
}
