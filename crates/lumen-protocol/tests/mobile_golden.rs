//! 移动端 Dart 模型与本 crate 线格式的**对齐守卫**（不引 FFI 的等价方案，M7 §4.2 拍板）。
//!
//! `tests/golden/mobile/*.json` 是 Rust 与 Dart **两端共用的唯一事实源**：
//!
//! - 本测试断言「语料 ⇄ [`LlmFrame`]」双向一致；
//! - `mobile/test/protocol/golden_test.dart`（**M7 片 1-Dart，尚未落地**——仓库里还没有 `mobile/`
//!   目录）**将**读同一批文件断言「语料 ⇄ Dart 模型」一致。在它落地之前，本文件只保证 Rust 侧
//!   不漂；那半边今天可以随便漂，这一点必须写成将来时，不许用陈述句假装保证已经存在。
//!
//! 两条合起来把**线格式的形状**钉死。改 Rust 变体名 / 加必填字段 → **本测试**先红；
//! Dart 少实现一个变体 → `flutter test` 红。
//!
//! **但形状 ≠ 语义**：往返断言是 `encode∘decode` 的不动点检查，对**同类型字段互换完全免疫**——
//! Dart 把 `known_seq` 解到字段 B、`known_turn` 解到字段 A，只要 `encode` 按同样的键名写回去就
//! 恒等往返，全部语料照绿。相邻同型字段一大把（`conv_id`/`conv_generation`、`seq`/`turn`、
//! `input_tokens`/`output_tokens`、`start_line`/`line_count`/`total_lines`、`request_id`/`req_id`），
//! 所以语料给它们配了**互不相同**的取值，Dart 侧必须另外写少量**取值断言**去用它——
//! 见 `README.md` §3 第 7 条。契约与流程写在 `tests/golden/mobile/README.md`
//! （Dart 实现者的入口文档），本文件只放 Rust 侧断言。
//!
//! # 为什么是「手写 Dart 模型 + 共用语料」而不是 flutter_rust_bridge
//!
//! FRB **不走 serde**，桥过去的是它自己的 codec，线格式根本带不过来（§3.2-③）；再加上 Android 要
//! NDK + cargo-ndk + 3 个 ABI、iOS 的 xcframework 只能在 macOS 上产出（作者本机是 Windows），
//! 以及把 cdylib 加进 `[workspace] members` 会污染 `cargo clippy --workspace` 与无 `-p` 的发版构建。
//! 语料这条路的代价只有一条：改协议要手工同步 Dart——**但 CI 会红，不会静默**。
//!
//! # 等价判定规则（本文件的核心，Dart 侧必须逐条照做）
//!
//! 记 `D` = 反序列化、`S` = 序列化，语料文件里 `frame` 是输入、`reserialized` 是期望输出
//! （缺省即等于 `frame` 本身）：
//!
//! 1. **按 [`serde_json::Value`] 比，不按字符串比**。键序不算差异（`serde_json` 的 `Map` 是
//!    `BTreeMap`，输出恒按键名排序；手写语料不必跟着排）；`1` 与 `1.0` 算差异（前者是整数、
//!    后者是浮点，两端的 JSON 解析器都区分）。
//! 2. **「缺省字段被补上默认值」算等价，但必须明写**。`#[serde(default)]` 且**没有**
//!    `skip_serializing_if` 的字段（`Hello::caps` / `Start::fork_session` / `TurnEnded::outcome` /
//!    `LlmUsage` 全体 …）在输出里一定出现。判定规则**不是**「测试自动放过多出来的默认值」——那要求
//!    测试内置一份默认值表，而那份表在 Dart 侧必然是第二份、必然漂移。规则是：**语料作者把补完的
//!    结果写进 `reserialized`**，于是「谁被补了默认值」在 code review 的 diff 上一眼可见。
//! 3. **`Option` + `skip_serializing_if` 的字段：`null` 与键缺席在输入侧等价、在输出侧不等价**。
//!    输入写 `null` → `None` → 输出里该键**消失**。Dart 侧 `toJson` 写出 `'model': null` 就与 Rust
//!    不同（见 `edge_null_option.json`，这是手写模型最常见的一处漂移）。
//! 4. **未知内容降级、不报错**：未知 `op` → [`LlmFrame::Unknown`]（连同它的整个 body 一起丢弃，
//!    **不保留、不回显**）；未知块 / 增量项 / `kind` 值 → 各自那一层的 `Unknown`，**同帧其余内容
//!    照常**；未知**字段**被静默丢弃（不得攒进一个 `extra` map 再原样回显——那等于给白名单转发开后门）。
//! 5. **开放式 C 型枚举的未知「值」原样保留、不降级**（`Other(String)`），但它的**形状**变了就是
//!    整帧报废，[`LlmFrame::Unknown`] 也救不回来（`compat_open_enum_shape_change_is_fatal.json`
//!    把这条已知边界钉成红线，两端都必须在那里报错）。
//! 6. **不动点**：`D(期望) == D(输入)` 且 `S(D(期望)) == 期望`。这条挡的是手写错的 `reserialized`
//!    ——没有它，一个瞎写的期望值会让往返断言变成自说自话。
//!
//! # 被否决的做法（写在这里，免得下一个人再走一遍）
//!
//! - **`UPDATE_GOLDEN=1` 自动重写语料**（设计蓝图 §7.3 草案里提过）：本 crate 的 `serde_json` 没开
//!   `preserve_order`，回写会把全部手写语料按键名重排，`note` 与人工编排的字段顺序一起被冲掉，
//!   review diff 变成噪声。改成**失败时把期望值打印出来让人粘贴**——多一步手工，换回「每次改语料
//!   都有人真的看过」。
//! - **让测试自动放过「多出来的默认值」**：见上文规则 2，那需要一份默认值表，Dart 侧必然复制一份。
//! - **把整块 MiB 级 blob 放进语料**去守 `LLM_TOOL_OUTPUT_MAX_BYTES` / `LLM_TURN_SNAPSHOT_MAX_BYTES`：
//!   语料是给**人 review** 的，塞几 MB 就没人看了。字节预算已由 `llm.rs` 自己的
//!   `llm_单轮快照满载单帧不超上限` / `llm_历史分页满载单帧不超上限` 用构造数据守着，
//!   本语料只守**形状与语义**，两者分工不重叠。
//! - **语料里出现 `> i64::MAX` 的整数**：Rust 侧 `seq` / `bytes` / `len` / `cost_micro_usd` 都是
//!   `u64`，协议上确实塞得下；但 **Dart 的 `int` 是 64 位有符号**，`jsonDecode` 遇到超出的字面量会
//!   退化成 `double` 并丢精度，两端从此静默不一致。故 [`语料自身的硬规矩`] 里有一条 lint 扫全部
//!   语料执行这个上界——协议能表达 ≠ 语料可以用。
//!
//! # 四类语料（M7 片 5 起）
//!
//! 一个语料文件只放**一种**载荷，靠文件名前缀区分，四者互斥（[`语料自身的硬规矩`] 里有断言）：
//!
//! | 前缀 | 载荷键 | 守什么 | 谁在读 |
//! |---|---|---|---|
//! | `frame_` / `compat_` / `edge_` / `redact_` / `envelope_` | `frame` | 内层 [`LlmFrame`]（数据面） | 片 1 起 |
//! | `c2s_` | `c2s` | [`RemoteC2S`]（控制面，手机 → 服务端） | 片 5 起 |
//! | `s2c_` | `s2c` | [`RemoteS2C`]（控制面，服务端 → 手机） | 片 5 起 |
//! | `rest_` | `rest` + `rest_type` | REST DTO（登录 / 设备列表 / 错误体） | 片 5 起 |
//! | `enums_` | `enums` | 控制面四个 C 型枚举的**全集** | 片 5 起 |
//!
//! **控制面与数据面的前向兼容口径相反，这是四类分开的根本原因**：内层 `LlmFrame` 有
//! `#[serde(other)]` 兜底（未知 `op` 降级成 `Unknown`），而外层 [`RemoteC2S`] / [`RemoteS2C`] 是
//! serde 默认的**外部标签枚举**，它**不支持** `other` ——未知变体就是整条 `from_str` 失败。
//! 所以两侧对 Dart 实现者的要求也相反：数据面「必须有 default 分支」，控制面「必须把每个变体都
//! 实现出来」。后者没有任何运行时兜底，语料是唯一的强制手段。
//!
//! 同理，`enums_control_plane.json` 里那四个枚举（[`DenyReason`] / [`PairingFailReason`] /
//! [`EndReason`] / [`Role`]）与 LLM 子协议那十个开放枚举**不是一回事**：开放枚举有 `Other(String)`
//! 保底，这四个没有——收到不认识的取值就是整条消息报废，而 [`RemoteS2C::ControlDenied`] 恰恰是
//! 「发起被拒」的**唯一**回执，丢了它 UI 就永久转圈。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use lumen_protocol::llm::{
    LlmBlock, LlmBlockEntry, LlmConvMeta, LlmConvState, LlmDeltaItem, LlmFrame,
    LlmPermissionDecision, LlmPermissionResolvedBy, LlmThinking, LlmToolResultDetail,
    LlmToolStatus, LlmTurnRecord, LLM_ENUM_DEBUG_HEAD_BYTES,
};
use lumen_protocol::pairing_qr::{PairingQrError, PairingQrPayload};
use lumen_protocol::remote::{
    DenyReason, EndReason, PairingFailReason, RemoteC2S, RemoteFrame, RemoteS2C, Role,
};
use lumen_protocol::{
    ApiError, AuthResponse, DeviceListResponse, LoginRequest, RefreshResponse, RegisterRequest,
};

// ── 语料信封 ──────────────────────────────────────────────────────────────────

/// 一个语料文件的内容。
///
/// **信封套在帧外面、不混进帧里**：`note` / `expect_variant` 这类元信息若直接塞进
/// `{"op": …}` 对象内部，serde 会把它们当未知字段丢掉，`reserialized` 就永远对不上；更糟的是
/// 语料从此不是「逐字节可粘贴的线格式样本」。故帧原文完整地收在 [`Self::frame`] 一个键下面。
///
/// `deny_unknown_fields`：语料文件里写错一个键名（`reserialised` / `debug_forbid`）会**当场报错**，
/// 而不是被静默忽略成「这个用例没有期望值」——那种失败会伪装成「测试通过」。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct 用例 {
    /// 中文说明：这条语料**守的是什么**。Dart 实现者只看这一行就该知道自己错在哪。
    note: String,
    /// 期望的解析结果类别，缺省 [`期望::可解析`]。
    #[serde(default)]
    expect: 期望,
    /// 期望解出的变体名（[`LlmFrame`] / [`RemoteC2S`] / [`RemoteS2C`] 视载荷而定）。
    /// `expect = "ok"` 时必填，是覆盖矩阵的申报值（测试会拿它与**实际**解出的变体名对账，
    /// 防止申报与事实不符）。REST 与枚举语料没有变体概念，故不填。
    #[serde(default)]
    expect_variant: Option<String>,
    /// 线格式帧原文（内部标签 `op`，即 `RemoteFrame::Llm` 里那一层）。
    ///
    /// **片 5 起改成可选**：一个语料文件只放一种载荷，`frame` / `c2s` / `s2c` / `rest` / `enums`
    /// 五者恰好有一个（[`语料自身的硬规矩`] 里有断言）。下面那批只针对内层帧的测试一律先过
    /// [`帧语料`] 过滤，别的类别不会被它们误判成「缺 expect_variant」。
    #[serde(default)]
    frame: Option<serde_json::Value>,
    /// 控制面 C2S 消息原文（外部标签，手机 → 服务端）。
    #[serde(default)]
    c2s: Option<serde_json::Value>,
    /// 控制面 S2C 消息原文（外部标签，服务端 → 手机）。
    #[serde(default)]
    s2c: Option<serde_json::Value>,
    /// REST DTO 原文（普通结构体，无标签），必须配 [`Self::rest_type`] 指明是哪个类型。
    #[serde(default)]
    rest: Option<serde_json::Value>,
    /// [`Self::rest`] 的类型名，见 [`REST类型全集`]。
    #[serde(default)]
    rest_type: Option<String>,
    /// 控制面 C 型枚举的取值全集：`{"DenyReason": ["Offline", …], …}`。
    ///
    /// 这四个枚举**没有** `Other` 兜底，Dart 侧必须把每个取值都实现出来；本字段是
    /// 「Rust 加取值 ⇒ 覆盖断言红 ⇒ 补语料 ⇒ Dart 红」这条链的唯一载体。
    #[serde(default)]
    enums: BTreeMap<String, Vec<String>>,
    /// 扫码配对的二维码载荷（[`PairingQrPayload`]），必须配 [`Self::qr_check`]。
    #[serde(default)]
    qr: Option<serde_json::Value>,
    /// [`Self::qr`] 的四重校验期望。
    #[serde(default)]
    qr_check: Option<Qr校验>,
    /// 重新序列化后的期望值；缺省 = 与 [`Self::frame`] 逐值相等。
    ///
    /// 只有**刻意考察缺省 / 降级**的用例才写它；`frame_*` 系列一律把字段写全，让它保持缺省。
    #[serde(default)]
    reserialized: Option<serde_json::Value>,
    /// 绝不允许出现在调试渲染（Rust 的 `{:?}`、Dart 的 `toString()`）里的敏感子串。
    ///
    /// 语料里的密钥**全部是合成假值**，真实凭证一律不得入库。
    #[serde(default)]
    debug_forbidden: Vec<String>,
    /// **外层 `RemoteFrame` 信封**的线格式样本（可选，只有 `envelope_*.json` 用）。
    ///
    /// 每一项都必须能解成 [`RemoteFrame`] 并原样往返。存在的理由：此前全部语料的 `frame` 都是
    /// **内层**帧，Dart 的 golden 套件在结构上就没法测 README 自己点名最容易错的那一层
    /// （externally tagged、unit 变体是裸字符串 `"P2pStreamHello"` 而不是对象）。Rust 侧顺手验的那条
    /// 断言（[`语料双向往返语义等价`] 末尾）过不到 Dart 手里。
    #[serde(default)]
    envelopes: Vec<serde_json::Value>,
}

/// 二维码语料的校验期望：拿这三个「本机已知值」去 `validate`，应当得到 `verdict`。
///
/// 把**期望值**写进语料而不是只放一个合法载荷，是因为这一层真正要守的不是往返，
/// 而是**四重校验各自命中且顺序正确**——那才是扫码配对的全部安全性所在。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Qr校验 {
    /// 本机登录的规范化 origin。
    origin: String,
    /// 本机账户指纹。
    user_fingerprint: String,
    /// 本次要连接的目标设备。
    target: String,
    /// 期望结果：`Ok` 或 [`PairingQrError::code`] 的四个取值之一。
    verdict: String,
}

/// 语料对解析结果的期望。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
enum 期望 {
    /// 必须解析成功（含「未知内容降级成 `Unknown`」——降级也是成功）。
    #[default]
    #[serde(rename = "ok")]
    可解析,
    /// 必须整帧报废。**目前只有一条**：C 型枚举的线上形状被改（裸字符串 → 数字），
    /// `untagged` 匹配不上任何变体，`#[serde(other)]` 在这一层救不回来。
    #[serde(rename = "decode_error")]
    整帧报废,
}

/// 语料目录（`tests/golden/mobile`）。Dart 侧读的是同一个路径。
fn 语料目录() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/mobile")
}

/// 载入全部语料，按文件名升序（`read_dir` 的顺序随文件系统变，排序后失败信息才可复现）。
fn 载入语料() -> Vec<(String, 用例)> {
    let 目录 = 语料目录();
    let mut 文件: Vec<PathBuf> = std::fs::read_dir(&目录)
        .unwrap_or_else(|e| panic!("读不到语料目录 {}: {e}", 目录.display()))
        .map(|e| e.expect("目录项").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    文件.sort();
    assert!(!文件.is_empty(), "语料目录是空的：{}", 目录.display());

    文件
        .into_iter()
        .map(|路径| {
            let 名 = 路径
                .file_stem()
                .expect("文件名")
                .to_string_lossy()
                .into_owned();
            let 正文 = std::fs::read_to_string(&路径).expect("读语料");
            let 用例: 用例 = serde_json::from_str(&正文)
                .unwrap_or_else(|e| panic!("语料 {名}.json 不符合信封结构: {e}"));
            (名, 用例)
        })
        .collect()
}

/// 只保留**内层帧**语料。
///
/// 片 5 引入控制面与 REST 语料之后，「遍历全部语料」不再等于「遍历全部帧」——不过滤的话，
/// 一条 `c2s_*.json` 会被 [`语料覆盖全部可增长变体`] 判成「缺 expect_variant 的帧语料」，
/// 失败信息还会指向一个根本不存在的问题。
fn 帧语料() -> Vec<(String, 用例)> {
    载入语料()
        .into_iter()
        .filter(|(_, 用例)| 用例.frame.is_some())
        .collect()
}

impl 用例 {
    /// 帧原文。只在 [`帧语料`] 过滤之后调用。
    fn 帧(&self) -> serde_json::Value {
        self.frame.clone().expect("帧语料必须有 frame")
    }

    /// 申报的变体名（`expect = "ok"` 时必填）。
    fn 申报变体<'a>(&'a self, 名: &str) -> &'a str {
        self.expect_variant
            .as_deref()
            .unwrap_or_else(|| panic!("{名}.json 缺 expect_variant（Dart 侧靠它断言）"))
    }
}

// ── 变体表：名字清单与穷尽 match **同源** ─────────────────────────────────────

/// 为一个枚举同时生成「全变体名清单」与「取变体名的穷尽 `match`」。
///
/// **两者同源是这个宏存在的全部理由**。分开写会留一个静默的洞：给 [`LlmFrame`] 加变体时，
/// 穷尽 `match` 确实会编译失败逼人补一条 arm，但如果名字清单是另一份手写常量，补完 arm 就绿了
/// ——新变体永远没有语料，而 CI 一声不吭。同源之后链条是硬的：
/// 加变体 → `match` 不穷尽（**编译失败**）→ 补进本宏 → 名字进清单 → [`语料覆盖全部可增长变体`]
/// 发现该名字没有语料（**测试红**）→ 只能去写语料。
///
/// `$类型::$v { .. }` 对 unit 变体同样合法（`E::V {}` 是允许的结构体模式），故 `Unknown` 这类
/// 空变体不需要单开一条规则。
macro_rules! 变体表 {
    ($类型:ident, $清单:ident, $取名:ident, [$($v:ident),+ $(,)?]) => {
        /// 该类型的全部变体名（与下面的穷尽 `match` 同源，见 `变体表!` 宏文档）。
        const $清单: &[&str] = &[$(stringify!($v)),+];

        /// 取变体名。穷尽 `match`：加变体不改这里就编译不过。
        fn $取名(值: &$类型) -> &'static str {
            match 值 {
                $($类型::$v { .. } => stringify!($v),)+
            }
        }
    };
}

变体表!(
    LlmFrame,
    帧变体全集,
    帧变体名,
    [
        Hello,
        HelloAck,
        ListAgents,
        AgentList,
        ListConvs,
        ConvList,
        Start,
        ConvStarted,
        ConvFailed,
        Send,
        Interrupt,
        Close,
        ConvExited,
        Attach,
        Attached,
        Detach,
        Delta,
        DeltaAck,
        TurnStarted,
        TurnEnded,
        RateLimit,
        TurnFetch,
        TurnSnapshot,
        HistoryReq,
        HistoryPage,
        PermissionRequest,
        PermissionReply,
        PermissionResolved,
        Error,
        Unknown,
    ]
);
变体表!(
    LlmBlock,
    块变体全集,
    块变体名,
    [Text, Thinking, ToolUse, ToolResult, Image, Error, Unknown]
);
变体表!(
    LlmDeltaItem,
    增量项变体全集,
    增量项变体名,
    [BlockStart, TextAppend, BlockEnd, Dropped, Unknown]
);
变体表!(
    LlmThinking,
    思考变体全集,
    思考变体名,
    [Text, Redacted, Omitted, Unknown]
);
变体表!(
    LlmToolStatus,
    工具状态变体全集,
    工具状态名,
    [Ok, Error, Denied, Cancelled, Unknown]
);
变体表!(
    LlmToolResultDetail,
    工具摘要变体全集,
    工具摘要名,
    [File, Unknown]
);
变体表!(
    LlmPermissionDecision,
    审批决定变体全集,
    审批决定名,
    [Allow, Deny, Unknown]
);
变体表!(
    LlmPermissionResolvedBy,
    裁决方变体全集,
    裁决方名,
    [User, Timeout, PolicyAuto, ConvClosed, Unknown]
);
变体表!(
    LlmConvState,
    对话态变体全集,
    对话态名,
    [Starting, Idle, Running, WaitingPermission, Exited, Unknown]
);

// ── 控制面（片 5）：外部标签枚举，**没有 `other` 兜底** ───────────────────────
//
// 同一个 `变体表!` 宏，但对账口径与数据面相反：数据面要的是「每个变体都有语料，好验降级」，
// 控制面要的是「每个变体 Dart 都实现了，因为没有降级可言」。

变体表!(
    RemoteC2S,
    C2S变体全集,
    上行变体名,
    [
        RequestControl,
        SubmitPairing,
        DeclineControl,
        EndSession,
        Relay,
        Ping,
        ClientHello,
        OpenHidden,
        RelayTo,
        EndHidden,
    ]
);
变体表!(
    RemoteS2C,
    S2C变体全集,
    下行变体名,
    [
        Welcome,
        ControlRequested,
        PairingNeeded,
        PairingResult,
        ControlDenied,
        PairingCancelled,
        SessionStarted,
        SessionEnded,
        Relay,
        Pong,
        HiddenControlRequested,
        HiddenSessionStarted,
        HiddenSessionEnded,
        RelayTo,
    ]
);

变体表!(
    DenyReason,
    拒因全集,
    拒因名,
    [
        Offline,
        AlreadyControlled,
        ControllerBusy,
        TargetPairing,
        CrossUser,
        SelfControl,
        RejectedByUser,
        ControllerLeft,
        Expired,
        TooManyAttempts,
    ]
);
变体表!(
    PairingFailReason,
    配对失败全集,
    配对失败名,
    [InvalidCode, Expired, NoPending, TooManyAttempts]
);
变体表!(
    EndReason,
    结束原因全集,
    结束原因名,
    [PeerLeft, PeerDisconnected, Replaced]
);
变体表!(Role, 角色全集, 角色名, [Controller, Controlled]);

/// 手机端**刻意不实现**的 C2S 变体，逐条写明理由——这份清单本身就是设计决策的存放处。
///
/// [`控制面语料覆盖移动端子集`] 断言「有语料的 ∪ 本清单 == 全集」且两者**不相交**：
/// 协议加一个 C2S 变体时，穷尽 `match` 先让编译失败逼人补进 [`C2S变体全集`]，随后覆盖断言
/// 逼人二选一——要么给手机端写语料并实现，要么写进这里并说明为什么不实现。**没有默认值**。
const 手机不发的C2S: &[(&str, &str)] = &[
    (
        "RequestControl",
        "镜像会话的发起消息。手机发它 = 占住那台 PC 的镜像独占位（且老服务端会照常执行成功、\
         两端都察觉不到），手机要的是 OpenHidden。类型层不实现，比在文档里写一句「不要发」硬。",
    ),
    (
        "DeclineControl",
        "被控端拒绝未决配对用的。手机端在 P0 不做被控端。",
    ),
    (
        "EndSession",
        "拆的是本设备唯一的**镜像**会话；手机拆隐藏会话用带 id 的 EndHidden。\
         两者都是 unit/结构体变体、名字又像，实现出来只会被误用。",
    ),
    (
        "Relay",
        "镜像会话的数据面盲转，按「发送者是谁」路由，在一台 PC 挂多条隐藏会话时结构性失效。\
         手机走带 session_id 的 RelayTo。",
    ),
];

/// 移动端会用到的 REST DTO 顶层类型（嵌套的 `DeviceInfo` / `UserInfo` / `DeviceRecord`
/// 随顶层一起被覆盖）。[`rest语料双向往返`] 断言每一个都至少有一条语料。
const REST类型全集: &[&str] = &[
    "RegisterRequest",
    "LoginRequest",
    "AuthResponse",
    "RefreshResponse",
    "DeviceListResponse",
    "ApiError",
];

// ── 覆盖账本 ──────────────────────────────────────────────────────────────────

/// 语料实际触达到的变体集合。
///
/// **只统计 `llm.rs` 里带 `#[serde(other)]` 的 9 个类型**，这条线是有依据的、不是随手划的：
/// `#[serde(other)]` 恰好标出了「这一层会长新变体，且老对端必须降级而不是报错」的每一处，
/// 也正是 Dart 侧必须写出一条 `default:` 分支的每一处。漏一个语料 = 有一条 Dart 降级路径没人测过。
///
/// 开放式 C 型枚举（[`lumen_protocol::llm::LlmStopReason`] 一类，线上是裸字符串）**不在此列**，
/// 但它们有另一条硬要求：**每一个开放枚举都必须有一条 `Other` 语料**。
///
/// 理由是两端不对称——Rust 侧 10 个 `open_enum!` 是**同一个宏**生成的十份同样的代码，覆盖 2 个
/// 等于覆盖 10 个；**Dart 侧要手写 10 个 sealed class**，语料覆盖了几个就只证明了几个。
/// 覆盖它们的是 `compat_unknown_open_enum_values*.json` 五条 + `redact_open_enum_other.json`：
/// stop / terminal_reason、agent / permission_mode、error code、exit reason、
/// rate limit 的 status·window·overage_×2。（这里不做机械对账，因为「哪个字段是开放枚举」
/// 在类型层看不出来；靠 README §4 的清单 + review。）
#[derive(Default)]
struct 覆盖 {
    帧: BTreeSet<&'static str>,
    块: BTreeSet<&'static str>,
    增量项: BTreeSet<&'static str>,
    思考: BTreeSet<&'static str>,
    工具状态: BTreeSet<&'static str>,
    工具摘要: BTreeSet<&'static str>,
    审批决定: BTreeSet<&'static str>,
    裁决方: BTreeSet<&'static str>,
    对话态: BTreeSet<&'static str>,
}

impl 覆盖 {
    /// 走一整帧，把沿途遇到的每一层变体都记账。
    ///
    /// **这里的 `match` 也是穷尽的**（没有 `_ =>` 兜底）：新增一个「里面嵌了块 / 嵌了记录」的帧变体时
    /// 必须在这里表态，否则编译不过。若只写 `_ => {}`，新帧的嵌套内容会**不进覆盖账本**，
    /// 于是「加了变体也加了语料，但里面新的块类型没人测」这种半覆盖能一路绿到线上。
    fn 收帧(&mut self, 帧: &LlmFrame) {
        self.帧.insert(帧变体名(帧));
        match 帧 {
            LlmFrame::HelloAck { convs, .. } | LlmFrame::ConvList { convs, .. } => {
                for 元 in convs {
                    self.收元信息(元);
                }
            }
            LlmFrame::ConvStarted { meta, .. } | LlmFrame::Attached { meta, .. } => {
                self.收元信息(meta);
            }
            LlmFrame::Delta { items, .. } => {
                for 项 in items {
                    self.收增量项(项);
                }
            }
            LlmFrame::TurnStarted { user, .. } => self.收块条目(user),
            LlmFrame::TurnSnapshot { record, .. } => self.收记录(record),
            LlmFrame::HistoryPage { turns, .. } => {
                for 记录 in turns {
                    self.收记录(记录);
                }
            }
            LlmFrame::PermissionReply { decision, .. } => {
                self.审批决定.insert(审批决定名(decision));
            }
            LlmFrame::PermissionResolved { decision, by, .. } => {
                self.审批决定.insert(审批决定名(decision));
                self.裁决方.insert(裁决方名(by));
            }
            // 以下变体不嵌套任何可增长类型，只贡献帧名本身。
            LlmFrame::Hello { .. }
            | LlmFrame::ListAgents { .. }
            | LlmFrame::AgentList { .. }
            | LlmFrame::ListConvs { .. }
            | LlmFrame::Start { .. }
            | LlmFrame::ConvFailed { .. }
            | LlmFrame::Send { .. }
            | LlmFrame::Interrupt { .. }
            | LlmFrame::Close { .. }
            | LlmFrame::ConvExited { .. }
            | LlmFrame::Attach { .. }
            | LlmFrame::Detach { .. }
            | LlmFrame::DeltaAck { .. }
            | LlmFrame::TurnEnded { .. }
            | LlmFrame::RateLimit { .. }
            | LlmFrame::TurnFetch { .. }
            | LlmFrame::HistoryReq { .. }
            | LlmFrame::PermissionRequest { .. }
            | LlmFrame::Error { .. }
            | LlmFrame::Unknown => {}
        }
    }

    /// 对话元信息里唯一可增长的是运行态。
    fn 收元信息(&mut self, 元: &LlmConvMeta) {
        self.对话态.insert(对话态名(&元.state));
    }

    /// 一轮定稿记录：用户块与助手块**共用一个 `block_id` 空间**，覆盖统计不区分二者。
    fn 收记录(&mut self, 记录: &LlmTurnRecord) {
        self.收块条目(&记录.user);
        self.收块条目(&记录.assistant);
    }

    fn 收块条目(&mut self, 条目: &[LlmBlockEntry]) {
        for 条 in 条目 {
            self.收块(&条.block);
        }
    }

    fn 收增量项(&mut self, 项: &LlmDeltaItem) {
        self.增量项.insert(增量项变体名(项));
        match 项 {
            LlmDeltaItem::BlockStart { entry } => self.收块(&entry.block),
            LlmDeltaItem::BlockEnd { block, .. } => {
                if let Some(块) = block {
                    self.收块(块);
                }
            }
            LlmDeltaItem::TextAppend { .. }
            | LlmDeltaItem::Dropped { .. }
            | LlmDeltaItem::Unknown => {}
        }
    }

    fn 收块(&mut self, 块: &LlmBlock) {
        self.块.insert(块变体名(块));
        match 块 {
            LlmBlock::Thinking { content } => {
                self.思考.insert(思考变体名(content));
            }
            LlmBlock::ToolResult { status, detail, .. } => {
                self.工具状态.insert(工具状态名(status));
                if let Some(摘要) = detail {
                    self.工具摘要.insert(工具摘要名(摘要));
                }
            }
            LlmBlock::Text { .. }
            | LlmBlock::ToolUse { .. }
            | LlmBlock::Image { .. }
            | LlmBlock::Error { .. }
            | LlmBlock::Unknown => {}
        }
    }
}

/// 把「实际覆盖」与「全变体清单」对账，缺哪个报哪个。
fn 对账(类型名: &str, 实际: &BTreeSet<&'static str>, 全集: &[&str]) {
    let 缺失: Vec<&str> = 全集
        .iter()
        .copied()
        .filter(|名| !实际.contains(名))
        .collect();
    assert!(
        缺失.is_empty(),
        "{类型名} 有变体没有任何语料覆盖: {缺失:?}\n\
         请先往 tests/golden/mobile/ 加语料（两侧同时红），再各自实现——流程见该目录的 README.md"
    );
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

/// **双向断言**：语料 → [`LlmFrame`] → 语料，按 [`serde_json::Value`] 比语义等价。
///
/// 判定规则的完整表述在模块文档「等价判定规则」一节；本函数逐条执行它，外加两条只有 Rust 侧
/// 做得了的加固：
///
/// - **不动点**（规则 6）：`reserialized` 自己再走一遍解析/序列化必须原地不动，且与 `frame` 解出
///   同一个值。没有它，一个手写错的期望值会让整条往返断言变成自说自话。
/// - **信封形状**：`RemoteFrame::Llm` 是 serde 默认的 externally tagged 变体，线上是
///   `{"Llm": {"op": …}}` 这一层套一层。移动端最容易在这里搞错（把 `op` 直接摊到 Relay 载荷顶层），
///   故对**每一条**语料都顺手把信封也验一遍——零额外语料成本。
#[test]
fn 语料双向往返语义等价() {
    let mut 已见帧: BTreeSet<String> = BTreeSet::new();
    for (名, 用例) in 帧语料() {
        if 用例.expect == 期望::整帧报废 {
            continue;
        }
        let 帧 = 用例.帧();
        // 语料不该有两条一模一样的帧（复制粘贴出来的语料只增加维护量、不增加覆盖）。
        let 指纹 = serde_json::to_string(&帧).expect("序列化 frame");
        assert!(
            已见帧.insert(指纹),
            "{名}.json 的 frame 与另一条语料完全相同——语料要么加信息、要么别加"
        );

        let 解出: LlmFrame = serde_json::from_value(帧.clone())
            .unwrap_or_else(|e| panic!("{名}.json 必须能解析（未知内容应降级而不是报错）: {e}"));
        let 回写 = serde_json::to_value(&解出).expect("序列化");
        let 期望值 = 用例.reserialized.clone().unwrap_or_else(|| 帧.clone());

        if let Some(声明) = &用例.reserialized {
            assert_ne!(
                声明, &帧,
                "{名}.json 的 reserialized 与 frame 逐值相同——它是给「缺省被补上 / 未知被降级」用的，\
                 没有差异就该删掉，留着会让 review 的人以为这里有玄机"
            );
        }
        assert_eq!(
            回写,
            期望值,
            "{名}.json 往返不等价。\n\
             —— note: {}\n\
             —— 实际输出（键按名排序；确认无误后粘进 reserialized）：\n{}",
            用例.note,
            serde_json::to_string_pretty(&回写).expect("pretty")
        );

        // 不动点：期望值自己也必须解出同一个帧，且再序列化原地不动。
        let 期望解出: LlmFrame = serde_json::from_value(期望值.clone())
            .unwrap_or_else(|e| panic!("{名}.json 的期望输出自身必须可解析: {e}"));
        assert_eq!(
            期望解出, 解出,
            "{名}.json 的 reserialized 与 frame 解出的不是同一个值——期望值写错了"
        );
        assert_eq!(
            serde_json::to_value(&期望解出).expect("序列化"),
            期望值,
            "{名}.json 的 reserialized 不是不动点（再序列化一次就变了）"
        );

        // 信封形状：Relay 载荷是 {"Llm": <帧>}，服务端只见一个不透明 Value。
        let 信封 = RemoteFrame::Llm(Box::new(解出))
            .to_value()
            .expect("to_value");
        assert_eq!(
            信封,
            serde_json::json!({ "Llm": 期望值 }),
            "{名}.json 的 RemoteFrame 信封形状不对：外层是 externally tagged 的 \"Llm\"，\
             内层才是 internally tagged 的 \"op\""
        );
    }
}

/// **覆盖矩阵**：`llm.rs` 里每一个带 `#[serde(other)]` 的类型，其**全部**变体都必须被语料覆盖。
///
/// 加变体而不加语料时的失败链条（宏文档里有完整说明）：穷尽 `match` 编译失败 → 补进 `变体表!` →
/// 名字进清单 → 本测试报「有变体没有任何语料覆盖」。**没有**只补一处就能糊弄过去的路径。
///
/// 同时把语料自己申报的 `expect_variant` 与**实际**解出的变体名对账：申报值是给 Dart 侧当断言用的
/// （Dart 没有 Rust 的穷尽 `match`，只能靠申报值），申报错了在这里当场暴露。
#[test]
fn 语料覆盖全部可增长变体() {
    let mut 账 = 覆盖::default();
    for (名, 用例) in 帧语料() {
        if 用例.expect == 期望::整帧报废 {
            assert!(
                用例.expect_variant.is_none(),
                "{名}.json 声明整帧报废，就不该再申报 expect_variant"
            );
            continue;
        }
        let 解出: LlmFrame = serde_json::from_value(用例.帧()).expect("必须能解析");
        let 申报 = 用例.申报变体(&名);
        assert_eq!(
            申报,
            帧变体名(&解出),
            "{名}.json 申报的变体与实际解出的不一致"
        );
        账.收帧(&解出);
    }

    对账("LlmFrame", &账.帧, 帧变体全集);
    对账("LlmBlock", &账.块, 块变体全集);
    对账("LlmDeltaItem", &账.增量项, 增量项变体全集);
    对账("LlmThinking", &账.思考, 思考变体全集);
    对账("LlmToolStatus", &账.工具状态, 工具状态变体全集);
    对账("LlmToolResultDetail", &账.工具摘要, 工具摘要变体全集);
    对账("LlmPermissionDecision", &账.审批决定, 审批决定变体全集);
    对账("LlmPermissionResolvedBy", &账.裁决方, 裁决方变体全集);
    对账("LlmConvState", &账.对话态, 对话态变体全集);

    // 与 `llm.rs` 的 `llm_全变体满载往返` 用的是同一个数字。两处都写死 30 是刻意的：
    // 一处是「每个变体都能往返」，一处是「每个变体都有跨语言语料」，谁先漏都该有人被绊一下。
    assert_eq!(
        帧变体全集.len(),
        30,
        "LlmFrame 变体数变了：先确认 llm.rs 的 llm_全变体满载往返 也跟着改了，再改这个数"
    );
}

/// **降级不报错**：未知输入必须活着落地，唯一一条已知的致命边界必须**真的**致命。
///
/// 前半条是前向兼容的全部价值；后半条是诚实记账——`#[serde(other)]` 兜的是未知「值」，
/// 兜不住已定型字段的**形状**变更（`stop` 从裸字符串变成数字 → `untagged` 匹配不上任何变体 →
/// 整帧 `Err`）。把它写成一条**期望失败**的用例，比在文档里写一句「注意这里兜不住」有用得多：
/// 哪天有人给 `LlmStopReason` 加了个数字形态，这条会红，逼他去看协议里那条
/// 「C 型枚举线格式一旦定为裸字符串就不得改形状」的约束。
#[test]
fn 未知内容降级而不是报错() {
    let mut 报废条数 = 0_u32;
    for (名, 用例) in 帧语料() {
        let 结果 = serde_json::from_value::<LlmFrame>(用例.帧());
        match 用例.expect {
            期望::可解析 => {
                assert!(
                    结果.is_ok(),
                    "{名}.json 必须降级而不是报错（{}）: {:?}",
                    用例.note,
                    结果.err()
                );
            }
            期望::整帧报废 => {
                报废条数 += 1;
                assert!(
                    结果.is_err(),
                    "{名}.json 声明整帧报废，实际却解析成功了 —— 若是协议真的收紧了兜底范围，\
                     请改语料并同步 Dart 侧；不要直接删掉这条用例"
                );
            }
        }
    }
    assert_eq!(
        报废条数, 1,
        "「已知的致命边界」应当只有一条（C 型枚举形状变更）。多出来的每一条都是前向兼容被削弱了，\
         必须在 llm.rs 的模块文档里同步记账"
    );
}

/// **脱敏契约**：语料声明的敏感子串，绝不能出现在调试渲染里。
///
/// 这条是跨语言的：Rust 侧看 `{:?}`（`LlmText` / `LlmPath` / `LlmToolInput` 与开放枚举的手写
/// `Debug`），Dart 侧看 `toString()`。审计日志与崩溃栈都会打这个，两端漏一边就等于没做。
///
/// 顺带守两个反向条件，否则这条断言可以靠写错字符串轻松「通过」：
/// - 敏感串必须**真的在线格式里出现**（不然就是个不存在的串，测了个寂寞）；
/// - 每条声明了 `debug_forbidden` 的语料必须**真的**有内容可漏（列表非空）。
#[test]
fn 敏感值不得进调试渲染() {
    let mut 守到的语料 = 0_u32;
    for (名, 用例) in 帧语料() {
        if 用例.debug_forbidden.is_empty() {
            continue;
        }
        守到的语料 += 1;
        let 帧 = 用例.帧();
        let 解出: LlmFrame = serde_json::from_value(帧.clone()).expect("必须能解析");
        let 调试 = format!("{解出:?}");
        let 线上 = serde_json::to_string(&帧).expect("序列化");
        for 敏感 in &用例.debug_forbidden {
            assert!(
                线上.contains(敏感.as_str()),
                "{名}.json 的 debug_forbidden 里「{敏感}」在线格式里根本不存在——\
                 大概率是写错了字符串，这条断言等于没写"
            );
            assert!(
                !调试.contains(敏感.as_str()),
                "{名}.json 的敏感串「{敏感}」泄漏进了 Debug：\n{调试}"
            );
        }
    }
    assert!(
        守到的语料 >= 4,
        "脱敏语料至少要覆盖正文/附件、工具入参、审批入参、开放枚举 Other 四类承载点，实得 {守到的语料} 条"
    );

    // 开放枚举 `Other` 的头部截断是最后一道兜底：收进来的 `Other` 长度**不受本端夹紧约束**
    // （接收侧刻意不校验长度，那会把「对端发了超长值」升级成整帧报废）。故把敏感内容放在第
    // LLM_ENUM_DEBUG_HEAD_BYTES 字节之后，是唯一能测出「Debug 是不是 derive 的」的办法。
    assert_eq!(
        LLM_ENUM_DEBUG_HEAD_BYTES, 16,
        "redact_open_enum_other.json 把假密钥放在第 16 字节之后，头部长度变了要重造那条语料"
    );
}

/// **外层信封**：`envelope_*.json` 声明的 [`RemoteFrame`] 样本必须能解析并原样往返。
///
/// 这条与 [`语料双向往返语义等价`] 末尾那条不重复：那条是 Rust 侧**顺手**用内层帧拼出来的，
/// 拼的过程本身就假定了信封形状是对的；本条读的是**语料文件里逐字节写死的信封原文**，
/// 于是 Dart 侧也拿得到同一份样本。裸字符串 `"P2pStreamHello"` 那一项守的是「unit 变体不是 `{"P2pStreamHello":{}}`」。
#[test]
fn 外层信封的两种标签形状() {
    let mut 验过 = 0_u32;
    for (名, 用例) in 载入语料() {
        for 信封 in &用例.envelopes {
            验过 += 1;
            let 解出 = RemoteFrame::from_value(信封).unwrap_or_else(|e| {
                panic!("{名}.json 的 envelopes 项必须能解成 RemoteFrame: {e}\n{信封}")
            });
            let 回写 = 解出.to_value().expect("to_value");
            assert_eq!(
                &回写, 信封,
                "{名}.json 的信封往返不等价——外层是 externally tagged，\
                 unit 变体是裸字符串而不是对象"
            );
        }
    }
    assert!(
        验过 >= 2,
        "至少要有「{{\"Llm\": …}} 对象变体」与「裸字符串 unit 变体」两种信封样本，实得 {验过} 条"
    );
}

/// **源文件对账**：语料这套强制链条有两处只对「变体」有效、对「类型本身」无效的边界，
/// 用直接读 `src/llm.rs` 数魔数的方式钉住（沿用本文件已在用的「魔数钉法」）。
///
/// 1. **新增第 10 个带 `#[serde(other)]` 的枚举**：[`语料覆盖全部可增长变体`] 的 9 次
///    [`对账`] 调用是**硬编码**的，新类型不编译失败、不覆盖失败、不需要语料——
///    「没有只补一处就能糊弄过去的路径」在这个边界上不成立。
/// 2. **新加一个 `#[serde(tag = …)]` 枚举却忘了 `#[serde(other)]`**：那比漏语料严重得多
///    （未知 `kind` 直接**整帧报废**，`LlmFrame::Unknown` 也救不回来），而且同样一声不吭。
///    两个数字一一对应就说明没有漏网的 tagged 枚举。
/// 3. **给已有变体加 `Option<T> + skip_serializing_if`**：这是本协议扩展的**主要方式**，
///    而整套 harness 对它完全失明（语料照常解析、序列化跳过该键、往返不变、`{ .. }` 模式不看
///    字段）。数字变了就逼作者回 README §5 第 4 步去读「必须同时改一条已有语料把新字段填上
///    `Some`」。
#[test]
fn 协议源文件的可增长点数目未变() {
    let 源 = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/llm.rs"))
        .expect("读 src/llm.rs");

    /// 只数**真正的属性行**（行首去空白后以 `#[serde(` 开头），不数文档里提到它的散文。
    fn 属性行数(源: &str, 关键: &str) -> usize {
        源.lines()
            .map(str::trim_start)
            .filter(|行| 行.starts_with("#[serde(") && 行.contains(关键))
            .count()
    }

    assert_eq!(
        属性行数(&源, "other)"),
        9,
        "`#[serde(other)]` 的数目变了：新增了带 other 的类型就要进 `覆盖` 账本 + `对账` 调用，\
         否则它的降级路径在两端都没人测过"
    );
    assert_eq!(
        属性行数(&源, "tag = "),
        9,
        "`#[serde(tag = …)]` 与 `#[serde(other)]` 的数目必须相等：\
         数目不等说明有 tagged 枚举漏了 other 兜底，未知 kind 会让整帧报废"
    );
    assert_eq!(
        属性行数(&源, "skip_serializing_if"),
        33,
        "可选字段数目变了。给已有变体加 Option + skip 时**本 harness 不会替你发现**——\
         必须同时改一条已有 frame_* 语料把新字段填上 Some，见 README §5 第 4 步"
    );
}

/// **语料自身的硬规矩**——守语料的语料（跨语言语料最容易在这一层烂掉）。
///
/// 每一条都对应一种「Rust 侧全绿、Dart 侧却对不上」的真实失败：
///
/// 1. **文件名只许 ASCII 小写 + 下划线**：语料要被 Flutter 当资源路径读、在 Windows / macOS /
///    Linux 三种 CI 上都得能打开；中文文件名在非 UTF-8 代码页的 Windows shell 与某些打包器里会烂掉。
///    说明写在 `note` 里，不写在文件名上。
/// 2. **整数不得超过 `i64::MAX`**：Dart 的 `int` 是 64 位**有符号**，`jsonDecode` 遇到更大的字面量
///    退化成 `double` 并丢精度。协议里 `seq` / `bytes` / `len` 是 `u64`，表达得了 ≠ 语料可以用。
/// 3. **`note` 必须像句人话**：语料是给 Dart 实现者读的，一句「测试用」等于没写。
/// 4. **目录里不许有闲杂文件**：除 `README.md` 与 `replay/` 子目录外只准放 `.json`，
///    免得半成品语料被两端各自忽略。`replay/` 装的是**回放语料**（JSONL，一行一个
///    `LlmFrame`），形状与本目录的「一文件一用例」信封完全不同，故独立成目录、
///    不参与 `载入语料`；两端的枚举都只取本层的 `*.json`（Dart 侧 `whereType<File>()`
///    天然跳过目录）。
#[test]
fn 语料自身的硬规矩() {
    /// 递归找出第一个超过 `i64::MAX` 的整数（`serde_json` 会把它存成 `u64`）。
    fn 越界整数(值: &serde_json::Value) -> Option<u64> {
        match 值 {
            serde_json::Value::Number(n) => n.as_u64().filter(|v| *v > i64::MAX as u64),
            serde_json::Value::Array(a) => a.iter().find_map(越界整数),
            serde_json::Value::Object(o) => o.values().find_map(越界整数),
            _ => None,
        }
    }

    for 项 in std::fs::read_dir(语料目录()).expect("读语料目录") {
        let 路径 = 项.expect("目录项").path();
        let 文件名 = 路径.file_name().expect("文件名").to_string_lossy();
        // `replay/` 是唯一获准的子目录（回放语料，见本函数文档第 4 条）；除它以外
        // **任何**目录都要拦下来，否则「除 README.md 外只准放 .json」这条会被一个
        // 随手建的目录悄悄架空。
        assert!(
            文件名 == "README.md"
                || 文件名 == "replay"
                || 路径.extension().is_some_and(|e| e == "json"),
            "语料目录里混进了 {文件名}：除 README.md 与 replay/ 外只准放 .json"
        );
    }

    /// 文件名前缀 → 该类语料必须带的载荷键。
    const 前缀与载荷: &[(&str, &str)] = &[
        ("frame_", "frame"),
        ("compat_", "frame"),
        ("edge_", "frame"),
        ("redact_", "frame"),
        ("envelope_", "frame"),
        ("c2s_", "c2s"),
        ("s2c_", "s2c"),
        ("rest_", "rest"),
        ("enums_", "enums"),
        ("qr_", "qr"),
    ];

    for (名, 用例) in 载入语料() {
        assert!(
            名.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "语料文件名 {名}.json 必须是 ASCII 小写 + 下划线（Flutter 资源路径 / 跨平台 CI 兼容）"
        );
        let 载荷键 = 前缀与载荷
            .iter()
            .find(|(前缀, _)| 名.starts_with(前缀))
            .map(|(_, 键)| *键)
            .unwrap_or_else(|| {
                panic!(
                    "语料文件名 {名}.json 必须带分类前缀 frame_ / compat_ / edge_ / redact_ / \
                     envelope_ / c2s_ / s2c_ / rest_ / enums_"
                )
            });
        assert!(
            用例.note.chars().count() >= 20,
            "{名}.json 的 note 太短，写清它守的是什么（Dart 实现者只看这一行）"
        );

        // 一个文件只放一种载荷：混着放会让「遍历某一类语料」变成一件要靠读注释才知道对不对的事，
        // 而两端各遍历一次，漏判在哪一端都只是静默少测几条。
        let 实有: Vec<&str> = [
            ("frame", 用例.frame.is_some()),
            ("c2s", 用例.c2s.is_some()),
            ("s2c", 用例.s2c.is_some()),
            ("rest", 用例.rest.is_some()),
            ("enums", !用例.enums.is_empty()),
            ("qr", 用例.qr.is_some()),
        ]
        .into_iter()
        .filter_map(|(键, 有)| 有.then_some(键))
        .collect();
        assert_eq!(
            实有,
            vec![载荷键],
            "{名}.json 的载荷与文件名前缀不符：前缀要求恰好一个 \"{载荷键}\"，实得 {实有:?}"
        );

        // rest_type 与 rest 必须同进同出：少了它测试不知道该往哪个类型解，多了它是复制粘贴残留。
        assert_eq!(
            用例.rest.is_some(),
            用例.rest_type.is_some(),
            "{名}.json 的 rest 与 rest_type 必须同时出现（rest_type 指明往哪个 DTO 解）"
        );
        // 同理：只有载荷没有校验期望，等于只测了往返——而这一层要守的是四重校验。
        assert_eq!(
            用例.qr.is_some(),
            用例.qr_check.is_some(),
            "{名}.json 的 qr 与 qr_check 必须同时出现（光有载荷等于没测校验）"
        );
        // debug_forbidden 的断言只在内层帧那条通路上跑（脱敏包装都在 llm.rs 里），
        // 挂到别的语料上会被静默忽略——那正是「测了个寂寞」的形状。
        assert!(
            用例.debug_forbidden.is_empty() || 用例.frame.is_some(),
            "{名}.json 在非帧语料上写了 debug_forbidden，它不会被任何断言读到"
        );

        for (什么, 值) in [
            ("frame", 用例.frame.as_ref()),
            ("c2s", 用例.c2s.as_ref()),
            ("s2c", 用例.s2c.as_ref()),
            ("rest", 用例.rest.as_ref()),
            ("reserialized", 用例.reserialized.as_ref()),
        ] {
            if let Some(大数) = 值.and_then(越界整数) {
                panic!(
                    "{名}.json 的 {什么} 里有超过 i64::MAX 的整数 {大数}：Dart 的 int 是 64 位\
                     有符号，jsonDecode 会把它退化成 double 并丢精度，两端从此静默不一致"
                );
            }
        }
    }
}

// ── 控制面（片 5）：外部标签枚举 + REST DTO ───────────────────────────────────

/// **控制面双向断言**：语料 → [`RemoteC2S`] / [`RemoteS2C`] → 语料，逐值相等。
///
/// 与内层帧那条的三点不同，每一点都源自「外部标签枚举没有 `other` 兜底」：
///
/// 1. **没有 `reserialized`**：控制面结构体里一个 `#[serde(default)]` 都没有，输入即输出。
///    哪天真加了一个，这里会当场红——那正是该停下来想想「老端收到缺字段的消息会怎样」的时刻。
/// 2. **没有「未知降级」用例**：未知变体在这一层就是整条 `from_str` 失败，两端都必须报错。
/// 3. **unit 变体是裸字符串**（`"Ping"` / `"Pong"`），不是 `{"Ping":{}}`。心跳走的正是这条，
///    Dart 侧假设「消息一定是 object」会在每 25 秒一次的心跳上直接炸。
#[test]
fn 控制面语料双向往返() {
    /// 一条控制面语料的往返断言（C2S / S2C 两侧逻辑逐字相同，只有类型不同）。
    macro_rules! 往返 {
        ($名:expr, $用例:expr, $原文:expr, $类型:ty, $取名:ident, $方向:expr) => {{
            let 解出: $类型 = serde_json::from_value($原文.clone()).unwrap_or_else(|e| {
                panic!(
                    "{}.json 的 {} 必须能解析（控制面没有 other 兜底，解不出就是整条丢弃）: {e}",
                    $名, $方向
                )
            });
            assert_eq!(
                $用例.申报变体($名),
                $取名(&解出),
                "{}.json 申报的变体与实际解出的不一致",
                $名
            );
            assert_eq!(
                &serde_json::to_value(&解出).expect("序列化"),
                $原文,
                "{}.json 的 {} 往返不等价（外部标签：结构体变体是 {{\"名\": {{…}}}}、\
                 unit 变体是裸字符串）",
                $名,
                $方向
            );
        }};
    }

    let mut 条数 = 0_u32;
    for (名, 用例) in 载入语料() {
        if let Some(原文) = &用例.c2s {
            条数 += 1;
            往返!(&名, 用例, 原文, RemoteC2S, 上行变体名, "c2s");
        }
        if let Some(原文) = &用例.s2c {
            条数 += 1;
            往返!(&名, 用例, 原文, RemoteS2C, 下行变体名, "s2c");
        }
    }
    assert!(
        条数 >= 20,
        "控制面语料只剩 {条数} 条——手机会发的 6 个 C2S 变体与全部 14 个 S2C 变体都该有语料"
    );
}

/// **控制面覆盖矩阵**：S2C 全覆盖，C2S 按「会发的 ∪ 刻意不发的 == 全集」对账。
///
/// 两侧口径不同，因为两侧的失败形态不同：
///
/// - **S2C 必须全实现**。手机是接收方，收到一条没实现的变体就是整条解析失败。哪怕是
///   「手机理论上收不到」的镜像消息（`SessionStarted` / `Relay` …）——「理论上收不到」是
///   对服务端当前行为的假设，不是协议保证，而 `DeviceKind`「手机不可被控」到 P1 才做。
///   解出来记一条日志丢掉，成本是几十行；假设它不会来，代价是一条静默失效的连接。
/// - **C2S 刻意不全实现**。手机发得出 `RequestControl` 才是真危险（占住那台 PC 的镜像独占位，
///   且老服务端会照常执行成功）。不实现 = 类型系统层面发不出去，比文档里写一句「不要发」硬。
///   [`手机不发的C2S`] 里逐条写着理由。
#[test]
fn 控制面语料覆盖移动端子集() {
    let mut c2s见: BTreeSet<&'static str> = BTreeSet::new();
    let mut s2c见: BTreeSet<&'static str> = BTreeSet::new();
    for (_, 用例) in 载入语料() {
        if let Some(原文) = &用例.c2s {
            let 解出: RemoteC2S = serde_json::from_value(原文.clone()).expect("必须能解析");
            c2s见.insert(上行变体名(&解出));
        }
        if let Some(原文) = &用例.s2c {
            let 解出: RemoteS2C = serde_json::from_value(原文.clone()).expect("必须能解析");
            s2c见.insert(下行变体名(&解出));
        }
    }

    对账("RemoteS2C", &s2c见, S2C变体全集);

    let 不发: BTreeSet<&'static str> = 手机不发的C2S.iter().map(|(名, _)| *名).collect();
    for (名, 理由) in 手机不发的C2S {
        assert!(
            !c2s见.contains(名),
            "{名} 既有 c2s 语料又列在「手机不发」清单里——二选一：要么删语料，要么把它从清单里拿掉"
        );
        assert!(
            理由.chars().count() >= 20,
            "「手机不发 {名}」缺少像样的理由。这份清单是设计决策的存放处，写「不需要」等于没写"
        );
    }
    let 合计: BTreeSet<&'static str> = c2s见.union(&不发).copied().collect();
    对账("RemoteC2S（有语料的 ∪ 手机刻意不发的）", &合计, C2S变体全集);
}

/// **控制面 C 型枚举**：四个枚举的每一个取值都必须被语料点名，且**线格式恒等于变体名**。
///
/// 后半句是在钉一条容易被顺手破坏的事实：这四个枚举都没挂 `#[serde(rename)]`，所以线上的
/// 字符串就是 Rust 的变体名。哪天有人加了 rename，Dart 侧那张手写的取值表会静默对不上——
/// 而它们没有 `Other` 兜底，对不上就是整条 [`RemoteS2C::ControlDenied`] 报废，
/// 丢掉的正是「发起被拒」的唯一回执。
#[test]
fn 控制面枚举语料覆盖全部取值() {
    let mut 账: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    let mut 条数 = 0_u32;
    for (名, 用例) in 载入语料() {
        for (类型, 取值们) in &用例.enums {
            assert!(!取值们.is_empty(), "{名}.json 的 {类型} 取值表是空的");
            for 取值 in 取值们 {
                条数 += 1;
                let 原文 = serde_json::Value::String(取值.clone());
                /// 解析一个取值并返回它的变体名；解不出说明语料里的字符串不是合法取值。
                macro_rules! 解 {
                    ($类型:ty, $取名:ident) => {{
                        let 值: $类型 = serde_json::from_value(原文.clone()).unwrap_or_else(|e| {
                            panic!("{名}.json 的 {类型} 取值「{取值}」不是合法线格式: {e}")
                        });
                        $取名(&值)
                    }};
                }
                let 变体名 = match 类型.as_str() {
                    "DenyReason" => 解!(DenyReason, 拒因名),
                    "PairingFailReason" => 解!(PairingFailReason, 配对失败名),
                    "EndReason" => 解!(EndReason, 结束原因名),
                    "Role" => 解!(Role, 角色名),
                    其它 => panic!(
                        "{名}.json 里的枚举类型 \"{其它}\" 不认识：控制面 C 型枚举只有 \
                         DenyReason / PairingFailReason / EndReason / Role 四个"
                    ),
                };
                assert_eq!(
                    变体名, 取值,
                    "{名}.json：{类型} 的线格式与变体名不一致（多半是加了 serde rename）——\
                     Dart 侧那张手写取值表会静默对不上，而这四个枚举没有 Other 兜底"
                );
                账.entry(类型.clone()).or_default().insert(变体名);
            }
        }
    }
    assert!(条数 > 0, "一条控制面枚举语料都没有（enums_control_plane.json 丢了？）");

    对账("DenyReason", 账.get("DenyReason").unwrap_or(&BTreeSet::new()), 拒因全集);
    对账(
        "PairingFailReason",
        账.get("PairingFailReason").unwrap_or(&BTreeSet::new()),
        配对失败全集,
    );
    对账("EndReason", 账.get("EndReason").unwrap_or(&BTreeSet::new()), 结束原因全集);
    对账("Role", 账.get("Role").unwrap_or(&BTreeSet::new()), 角色全集);
}

/// **扫码配对**：载荷往返 + **四重校验各自命中**。
///
/// 这一层的语料与别处不同——它守的重点不是往返（那只是顺手验的），而是
/// `validate` 的**判定与顺序**：形状（魔数 / 码格式）先于身份（服务器 / 账户 / 设备），
/// 且四种拒绝互不混淆。理由在 `pairing_qr.rs` 里：每一种拒绝对应一条不同的 UI 文案，
/// 合并成一句「二维码无效」，用户就永远不知道自己是扫错了设备、扫了别人的码，
/// 还是**遇到了钓鱼**。
///
/// 二维码载荷**不上线缆**（只经屏幕→摄像头），但两端各有一份实现，所以它同样需要语料。
#[test]
fn 二维码语料的四重校验() {
    let mut 见过: BTreeSet<String> = BTreeSet::new();
    for (名, 用例) in 载入语料() {
        let (Some(原文), Some(检查)) = (&用例.qr, &用例.qr_check) else {
            continue;
        };
        let 载荷: PairingQrPayload = serde_json::from_value(原文.clone())
            .unwrap_or_else(|e| panic!("{名}.json 的 qr 必须能解成 PairingQrPayload: {e}"));

        // 顺手验往返：字段是单字母短名，写错一个在这里就露馅。
        assert_eq!(
            &serde_json::to_value(&载荷).expect("序列化"),
            原文,
            "{名}.json 的二维码载荷往返不等价"
        );

        let 实际 = match 载荷.validate(&检查.origin, &检查.user_fingerprint, &检查.target) {
            Ok(()) => "Ok".to_string(),
            Err(e) => e.code().to_string(),
        };
        assert_eq!(
            实际, 检查.verdict,
            "{名}.json 的校验结果与语料声明不符\n—— note: {}",
            用例.note
        );
        见过.insert(检查.verdict.clone());
    }

    // 五种结果都要有语料：少了任何一种，就有一条 UI 文案分支没人测过。
    let 全集: BTreeSet<String> = ["Ok", "Malformed", "ForeignServer", "ForeignAccount", "WrongTarget"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(
        见过, 全集,
        "二维码校验的五种结果必须各有语料（Ok 与四种拒绝），实得 {见过:?}"
    );
    // 与 PairingQrError 的变体数对账：加了新拒因而不加语料，这里会红。
    assert_eq!(
        [
            PairingQrError::Malformed,
            PairingQrError::ForeignServer,
            PairingQrError::ForeignAccount,
            PairingQrError::WrongTarget,
        ]
        .len() + 1,
        全集.len(),
        "PairingQrError 变体数变了：新拒因要补一条语料，并确认它有自己的 UI 文案"
    );
}

/// **REST DTO 往返**：登录 / 注册 / 续期 / 设备列表 / 错误体逐个过一遍线格式。
///
/// 这一层此前**两端都没有守卫**：REST DTO 不在 `RemoteFrame` 里，桌面端与服务端共用同一份
/// Rust 类型所以永远不会漂，而手机端是第二份手写实现——改一个字段名，Rust 侧全绿，
/// 手机端要等到有人真的点了登录才发现。
///
/// 最需要守的是 `DeviceInfo` 那两个 `Option + skip_serializing_if`：写成 `"hw_id": null`
/// 与键缺席在**服务端**看是同一条分支（`filter(|s| !s.is_empty())`），所以线上看不出区别，
/// 直到某次有人把 null 改成空串——账户里就开始长幽灵设备。
#[test]
fn rest语料双向往返() {
    let mut 见过: BTreeSet<&'static str> = BTreeSet::new();
    for (名, 用例) in 载入语料() {
        let (Some(原文), Some(类型)) = (&用例.rest, &用例.rest_type) else {
            continue;
        };
        /// 按 `rest_type` 分派到具体 DTO 做往返，返回（回写值, 类型名）。
        macro_rules! 分派 {
            ([$($t:ident),+ $(,)?]) => {
                match 类型.as_str() {
                    $(stringify!($t) => {
                        let 解出: $t = serde_json::from_value(原文.clone()).unwrap_or_else(|e| {
                            panic!("{名}.json 解成 {} 失败: {e}", stringify!($t))
                        });
                        (serde_json::to_value(&解出).expect("序列化"), stringify!($t))
                    })+
                    其它 => panic!(
                        "{名}.json 的 rest_type \"{其它}\" 不在 REST类型全集里——\
                         新增 DTO 要同时加进 REST类型全集与这个 match"
                    ),
                }
            };
        }
        let (回写, 类型名) = 分派!([
            RegisterRequest,
            LoginRequest,
            AuthResponse,
            RefreshResponse,
            DeviceListResponse,
            ApiError,
        ]);
        assert_eq!(
            &回写,
            原文,
            "{名}.json 的 REST 往返不等价。\n\
             —— note: {}\n\
             —— 实际输出（键按名排序；Option + skip 的字段应当整个消失，不是写成 null）：\n{}",
            用例.note,
            serde_json::to_string_pretty(&回写).expect("pretty")
        );
        见过.insert(类型名);
    }
    对账("REST DTO", &见过, REST类型全集);
}
