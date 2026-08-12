# M7 golden 语料 —— Rust 与 Dart 两端共用的唯一事实源

这个目录里的 `*.json` 不是 Rust 的测试夹具，是**协议本身的可执行规格**。

- Rust 侧 `crates/lumen-protocol/tests/mobile_golden.rs` 断言「语料 ⇄ `LlmFrame`」一致；
- Dart 侧 `mobile/test/protocol/golden_test.dart`（**已落地**，M7 片 1-Dart）读同一批文件
  断言「语料 ⇄ Dart 模型」一致。

两条**合起来**把**线格式的形状**钉死。只做一边等于没做：改 Rust 变体名会让 Rust 侧红，
但 Dart 少实现一个变体只有 Dart 侧红得出来。

> ⚠ **形状 ≠ 语义**。往返断言是 `encode∘decode` 的不动点检查，对**同类型字段互换完全免疫**：
> Dart 把 `known_seq` 解到字段 B、`known_turn` 解到字段 A，只要 `encode` 按同样的键名写回去，
> 全部语料照绿。相邻同型字段一大把（`conv_id`/`conv_generation`、`seq`/`turn`、
> `input_tokens`/`output_tokens`、`start_line`/`line_count`/`total_lines`、`request_id`/`req_id`）。
> 语料已经给它们配了**互不相同**的取值，但那只是弹药——Dart 侧必须另外写少量**取值断言**去用它，
> 见 §3 第 7 条。

> 为什么不用 flutter_rust_bridge 直接共用类型：FRB **不走 serde**，桥过去的是它自己的 codec，
> 线格式根本带不过来；加上 Android 要 NDK + cargo-ndk + 3 个 ABI、iOS 的 xcframework 只能在 macOS
> 上产出（作者本机是 Windows）、cdylib 进 `[workspace] members` 会污染 `cargo clippy --workspace`
> 与无 `-p` 的发版构建。详见设计蓝图 §4.2。语料这条路的代价只有一条：**改协议要手工同步 Dart，
> 但 CI 会红，不会静默**。

---

## 1. 文件格式

每个文件一个用例，外层是**信封**，帧原文完整地收在 `frame` 一个键下面：

```jsonc
{
  // 中文一句话：这条语料守的是什么。Dart 实现者只看这一行就该知道自己错在哪。
  "note": "……",

  // 可选，缺省 "ok"。"decode_error" = 这条**必须**解析失败（目前只有一条）。
  "expect": "ok",

  // expect == "ok" 时必填：期望解出的 LlmFrame 变体名。
  // Dart 没有 Rust 的穷尽 match，覆盖矩阵只能靠这个申报值。
  "expect_variant": "Delta",

  // 线格式帧原文（internally tagged，标签字段名固定为 "op"）。
  // 这就是 RemoteFrame::Llm 信封里那一层，可以逐字节当样本用。
  "frame": { "op": "Delta", "...": "..." },

  // 可选：重新序列化后的期望值。缺省 = 与 frame 逐值相等。
  // 只有刻意考察「缺省被补上默认值 / 未知被降级」的用例才写它。
  "reserialized": { "op": "Delta", "...": "..." },

  // 可选：绝不允许出现在调试渲染（Rust `{:?}` / Dart `toString()`）里的敏感子串。
  "debug_forbidden": ["hunter2"],

  // 可选，只有 envelope_*.json 用：**外层 RemoteFrame 信封**的线格式样本。
  // 每一项都要能解成 RemoteFrame 并原样往返。见 §3 第 6 条。
  "envelopes": [{ "Llm": { "op": "Detach", "...": "..." } }, "P2pStreamHello"]
}
```

> 真实文件里**没有注释**（严格 JSON）。信封套在帧外面是刻意的：`note` 若塞进 `{"op": …}` 内部，
> serde 会把它当未知字段丢掉，`reserialized` 就永远对不上，语料也不再是「可逐字节粘贴的线格式样本」。

## 2. 等价判定规则（Dart 侧必须逐条照做）

记 `D` = 反序列化、`S` = 序列化，`期望 = reserialized ?? frame`：

| # | 规则 | 为什么 |
|---|---|---|
| 1 | **按解析后的 JSON 值比，不按字符串比** | 键序不算差异（Rust 侧 `serde_json::Map` 是 `BTreeMap`，输出恒按键名排序；Dart 的 `Map` 保插入序）。但 `1` 与 `1.0` **算**差异——两端的 JSON 解析器都区分整数与浮点。 |
| 2 | **「缺省字段被补上默认值」算等价，但必须明写在 `reserialized` 里** | 带 `#[serde(default)]` 且**没有** `skip_serializing_if` 的字段（`Hello.caps` / `Start.fork_session` / `TurnEnded.outcome` / `LlmUsage` 全体 …）在输出里一定出现。判定规则**不是**「测试自动放过多出来的默认值」——那要求测试内置一份默认值表，而那份表在 Dart 侧必然是第二份、必然漂移。写进语料后，「谁被补了默认值」在 review 的 diff 上一眼可见。 |
| 3 | **`Option` + `skip_serializing_if`：输入写 `null` 与键缺席等价，但输出里该键必须消失** | Dart 的 `toJson` 写出 `'model': null` 就与 Rust 不同。见 `edge_null_option.json`——手写模型最常见的一处漂移。 |
| 4 | **未知内容降级、不报错** | 未知 `op` → `LlmUnknown`（连同整个 body **一起丢弃，不保留不回显**）；未知块 / 增量项 / `kind` 值 → 各自那一层的 `Unknown`，**同帧其余内容照常**；未知**字段**被静默丢弃。 |
| 5 | **开放式 C 型枚举的未知「值」原样保留、不降级** | 线上是裸字符串（`"EndTurn"`），`#[serde(other)]` 在这一层用不上。Dart 侧必须用 `sealed class { Known \| Other(String) }` 或等价形状；直接映射成 Dart `enum` 会丢原文并抛异常。 |
| 6 | **不动点**：`D(期望) == D(frame)` 且 `S(D(期望)) == 期望` | 挡的是手写错的 `reserialized`。没有它，一个瞎写的期望值会让整条往返断言变成自说自话。 |

### 关于「未知字段不得回显」

规则 4 里那半句是**安全约束**，不是风格偏好：Dart 侧**不得**把未知键攒进一个 `extra` map 再原样
回显。PC → 手机是**白名单转发**，任何「先收下来、以后再说」的容器都是白名单的后门（实测 headless
的 `hook_response.stdout` 里出现过 4106 字符的私人记忆库全文）。`compat_unknown_field_dropped.json`
专门守这条。

## 3. Dart 侧要做什么

1. 读**这个目录**（不要复制一份到 `mobile/`——复制品会漂移；用相对路径或构建期软链）。
2. 对每个 `expect == "ok"` 的文件：
   - `decode(frame)` 必须成功；
   - `decode(frame).variantName == expect_variant`；
   - `encode(decode(frame))` 与 `期望` **深度相等**（按规则 1 的口径写一个 `deepJsonEquals`，
     不要用 `jsonEncode(a) == jsonEncode(b)` —— 那是在比键序）；
   - 不动点：`encode(decode(期望)) == 期望`。

   ⚠ **`deepJsonEquals` 的叶子比较必须连类型一起比**，否则规则 1 那句「`1` 与 `1.0` 算差异」
   在 Dart 里根本执行不了：

   ```dart
   // Dart 的 num 相等**跨 int/double**：
   240262 == 240262.0   // → true ！
   // 所以叶子比较要写成：
   if ((a is int) != (b is int)) return false;   // 或 a.runtimeType != b.runtimeType
   ```

   不加这一条，一个把 `cost_micro_usd` / `seq` / `context_used` 存成 `double` 的 Dart 模型
   会对**几乎全部**语料绿灯通过（只有 `edge_int_boundary.json` 里 `conv_id` 那个
   `9223372036854775807` 会因为 double 表示不了而露馅），§3 表里那整行「int 是 64 位有符号、
   会退化成 double 丢精度」的警告就等于白写。
3. 对 `expect == "decode_error"` 的文件：`decode(frame)` 必须**抛异常**。
   目前只有 `compat_open_enum_shape_change_is_fatal.json` 一条；把它宽容成 `Other("42")`
   反而与 Rust 不一致。
4. 对带 `debug_forbidden` 的文件：`decode(frame).toString()` 里不得出现列表里的任何子串。
5. **覆盖断言**：把全部文件的 `expect_variant` 收成集合，与 Dart 侧 `LlmFrame` 的 sealed 子类清单
   对账——少实现一个变体就红。
6. **信封**：Relay 载荷是 `{"Llm": <frame>}` 这一层套一层。外层 `RemoteFrame` 是 serde 默认的
   **externally tagged**（变体名当唯一 key，unit 变体是**裸字符串** `"P2pStreamHello"` 而不是对象），
   内层 `LlmFrame` 才是 **internally tagged**（`"op"` 字段）。两层机制不同，别用同一个解析器。
   带 `envelopes` 的文件（`envelope_llm_and_unit.json`）给出了这一层的**逐字节样本**：
   数组里每一项都要能解成 `RemoteFrame` 并原样往返。
7. **字段取值断言**（往返断言测不出来的那一半）：往返对称的模型即使把两个**同型**字段接反也照样
   全绿。所以至少要对下面三条语料各写 3–5 个取值断言，把相邻同型字段一一钉死：

   | 语料 | 断言示例 |
   |---|---|
   | `frame_attach.json` | `knownGeneration == 3`、`knownSeq == 102`、`knownTurn == 4` |
   | `frame_turn_snapshot.json` | `record.turn == 4`、`usage.inputTokens == 3`、`usage.outputTokens == 24`、`detail.startLine == 1`、`detail.lineCount == 165`、`detail.totalLines == 165` |
   | `frame_delta.json` | `convId == 11`、`convGeneration == 3`、`seq == 102`、`turn == 4`（四个数刻意各不相同）、`items[1].blockId == 2` |

   语料已经刻意给这些字段配了互不相同的值，就是为了让这类断言写得出来。

### Dart 侧的四个具体坑（语料里各有一条专门守着）

| 坑 | 语料 | 说明 |
|---|---|---|
| `int` 是 64 位**有符号** | `edge_int_boundary.json` | `jsonDecode` 遇到 `> i64::MAX` 的字面量会退化成 `double` 并丢精度。Rust 侧 `seq` / `bytes` / `len` / `cost_micro_usd` 是 `u64`，**协议表达得了 ≠ 语料可以用**——`mobile_golden.rs` 里有 lint 扫全部语料执行这个上界。（若日后要跑 `dart2js` / Web，安全整数上限还要再降到 2^53−1。） |
| `String` 按 **UTF-16** 计长 | `edge_unicode.json` | Rust 的 `LlmText::len_bytes()` 是 **UTF-8 字节**。凡是按长度夹紧的地方（`LLM_TOOL_OUTPUT_MAX_BYTES` = 32 KiB、`LLM_ENUM_WIRE_MAX_BYTES` = 64 B）Dart 侧必须先 `utf8.encode` 再数字节，直接用 `s.length` 会少算 2–3 倍。 |
| 增量可能把一个 emoji 拆两半 | `edge_text_append_split.json` | ZWJ 家庭 emoji 的前后半各自都是**合法**字符串，协议层无从阻止。归约器必须**先拼接、再交渲染层做字素簇切分**，不能每收一条就按字素簇渲染（在 Flutter 上是可见闪烁）。 |
| 夹紧要落在字符边界 | `edge_max_len.json` | `fromWireClamped` 按 64 字节截时必须退到字符边界（该用例的 `stop` 是 63 个 `A` + 「中」+「尾」，裸切第 64 字节会切出半个「中」）。注意该用例**期望原样往返、不截断**——夹紧只在**发送侧**生效，接收侧刻意不校验长度。 |

## 4. 语料分类

| 前缀 | 载荷键 | 数量 | 守什么 |
|---|---|---|---|
| `frame_*` | `frame` | 30 | 每个 `LlmFrame` 变体各一条，**字段逐个写全**（所以 `reserialized` 一律缺省）。这是「Dart 少实现一个变体就红」的载体。 |
| `compat_*` | `frame` | 13 | 前向兼容：未知 `op` / 未知字段 / 未知块 / 未知增量项 / 未知 `kind` / 未知开放枚举值（**5 条，覆盖全部 10 个开放枚举**），外加一条**期望失败**的已知致命边界。 |
| `edge_*` | `frame` | 13 | 边界：空数组、缺省字段、`null`、整数上界、多字节与 emoji、增量切分、长度上限附近、审批裁决方的四种来源。 |
| `redact_*` | `frame` | 4 | 脱敏承载点：正文/附件、工具入参、审批入参、开放枚举 `Other`。**密钥全是合成假值**，仓库里不得出现任何真实凭证。 |
| `envelope_*` | `frame` | 1 | 外层 `RemoteFrame` 信封的逐字节样本（externally tagged + 裸字符串 unit 变体）。这一层此前只有散文没有语料。 |
| `c2s_*` | `c2s` | 6 | **控制面上行**（手机 → 服务端）。只有手机**会发**的 6 个变体，另外 4 个刻意不实现，见 §7。 |
| `s2c_*` | `s2c` | 14 | **控制面下行**（服务端 → 手机）。`RemoteS2C` 的**全部** 14 个变体，一个都不能少，见 §7。 |
| `rest_*` | `rest` + `rest_type` | 7 | REST DTO：注册 / 登录（回访与首次两条）/ 鉴权响应 / 续期 / 设备列表 / 错误体。 |
| `enums_*` | `enums` | 1 | 控制面四个 C 型枚举（`DenyReason` / `PairingFailReason` / `EndReason` / `Role`）的取值**全集**。 |

**一个文件只放一种载荷**，五个载荷键恰好有一个（`mobile_golden.rs` 的 `语料自身的硬规矩` 里有断言）。
前缀与载荷键的对应关系也在那条断言里，改文件名不改载荷会当场红。

### 覆盖矩阵怎么被强制执行

Rust 侧对 `llm.rs` 里**每一个带 `#[serde(other)]` 的类型**（共 9 个：`LlmFrame` / `LlmBlock` /
`LlmDeltaItem` / `LlmThinking` / `LlmToolStatus` / `LlmToolResultDetail` / `LlmPermissionDecision` /
`LlmPermissionResolvedBy` / `LlmConvState`）断言**全部变体都被语料覆盖**。

这条线不是随手划的：`#[serde(other)]` 恰好标出了「这一层会长新变体、老对端必须降级而不是报错」的
每一处，也正是 Dart 侧必须写出一条 `default:` 分支的每一处。漏一个语料 = 有一条 Dart 降级路径没人
测过。

`mobile_golden.rs` 的 `协议源文件的可增长点数目未变` 会**直接读 `src/llm.rs`** 数三个数字并钉死：
`#[serde(other)]` = 9、`#[serde(tag = …)]` = 9、`skip_serializing_if` = 33。前两个必须**相等**——
数目不等就说明有 tagged 枚举漏了 `other` 兜底，那比漏语料严重得多（未知 `kind` 直接**整帧报废**，
`LlmUnknown` 也救不回来）。第三个的用途见 §5「改已有变体时」。

### 开放式 C 型枚举：**每一个都要有 `Other` 语料**

`LlmStopReason` 一类（线上是裸字符串）不进上面那张覆盖账本，但有它们自己的硬要求：
**10 个开放枚举，每一个都必须至少有一条 `Other` 语料。**

理由是两端不对称：Rust 侧 10 个 `open_enum!` 是**同一个宏**生成的十份同样的代码，覆盖 2 个等于覆盖
10 个；**Dart 侧要手写 10 个 sealed class**，语料覆盖了几个就只证明了几个。风险最高的是
`LlmErrorCode`（`ConvFailed.code` / `Error.code` / `LlmBlock::Error.code`）与 `LlmExitReason`——
新错误码恰恰是最先长出来的东西，而那正是「出事时唯一告诉用户出了什么事」的那一帧。

| 开放枚举 | 覆盖它的语料 |
|---|---|
| `LlmStopReason` / `LlmTerminalReason` | `compat_unknown_open_enum_values.json`、`redact_open_enum_other.json` |
| `LlmAgentKind` / `LlmPermissionMode` | `compat_unknown_open_enum_values_agent.json` |
| `LlmErrorCode` | `compat_unknown_open_enum_values_error_code.json` |
| `LlmExitReason` | `compat_unknown_open_enum_values_exit_reason.json` |
| `LlmRateLimitStatus` / `LlmRateLimitWindow` / `LlmOverageStatus` / `LlmOverageDisabledReason` | `compat_unknown_open_enum_values_rate_limit.json` |

## 5. 加新变体的流程（**先加语料，两侧同时红**）

这个顺序是硬的，不是建议：

1. **先在这里加语料**（`frame_<新变体>.json`），此时 Rust 与 Dart **两侧同时红**——
   Rust 说「解析失败 / 变体名对不上」，Dart 说「未知 op 落到了 `LlmUnknown`」。
2. 改 `crates/lumen-protocol/src/llm.rs` 加变体。
   `mobile_golden.rs` 里的穷尽 `match` 会**编译失败**逼你把新名字补进 `变体表!` 宏；
   补完之后覆盖断言才认这条语料。Rust 侧转绿。
3. 改 Dart 模型加对应 sealed 子类，Dart 侧转绿。
4. 提交时三处必须在同一个 PR 里：语料、`llm.rs`、Dart 模型。

**反过来做（先改 Rust、回头补语料）会留一个静默的洞**：Rust 侧改完自己就绿了，Dart 侧要等到有人
真的在手机上点到那个新功能才发现没实现。语料先行的意义就是把这个洞变成一条红线。

### 改**已有**变体时

改字段名 / 加必填字段 → Rust 侧往返断言当场红（语料对不上）。**不要**顺手改语料让它变绿——
先想清楚这是不是破坏性变更：老对端发来的旧帧还能不能解析？该加的是 `#[serde(default)]` 还是新变体？
`llm.rs` 模块文档里那条「C 型枚举线格式一旦定为裸字符串就不得改形状」的教训就是这么来的。

**第 4 步（最容易漏、也最常发生的那种改法）：给已有变体加可选字段时，必须同时改一条已有 `frame_*`
语料把新字段填上 `Some`。**

因为整套强制链条只对「**变体**」有效、对「**字段**」无效：加一个
`#[serde(default, skip_serializing_if = "Option::is_none")] Option<T>` 之后——语料照常解析、
序列化跳过该键、`frame == reserialized` 不变、`变体表!` 与 `覆盖::收帧` 的 `{ .. }` 模式压根不看
字段。**Rust 全绿、覆盖断言全绿、Dart 实现者永远收不到信号。** 而这正是本协议扩展的主要方式
（`llm.rs` 里已有 35 个 `Option + skip` 字段，每一个目前都至少被一条语料填了 `Some`）。

> **它真的绊住过人**（2026-08-11，片 10）：给 `Send` 与 `TurnStarted` 各加一个 `client_msg_id`
> 时全套 harness 照绿，只有这个数字红了，才回来补上 `frame_send.json` / `frame_turn_started.json`
> 的两个 `Some`。**一次真实拦截 = 这条规矩不是纸面纪律。**

唯一的机械兜底是 `协议源文件的可增长点数目未变` 里那个 `skip_serializing_if == 35`：
它不会告诉你该改哪条语料，只会在你加字段时**绊你一下**，逼你回来读这一段。

## 6. 失败了怎么办

```bash
cargo test -p lumen-protocol --test mobile_golden -- --nocapture
```

往返不等价时，失败信息里会把**实际输出**按 pretty JSON 打出来（键按名排序）。确认那份输出确实是你
想要的语义之后，把它粘进对应语料的 `reserialized` 字段。

> 刻意**没有** `UPDATE_GOLDEN=1` 自动重写：本 crate 的 `serde_json` 没开 `preserve_order`，回写会把
> 全部手写语料按键名重排，`note` 与人工编排的字段顺序一起被冲掉，review diff 变成噪声。多一步手工
> 粘贴，换回「每次改语料都有人真的看过」。

---

## 7. 控制面与 REST 语料（M7 片 5 新增）

### 7.1 与数据面**口径相反**：这里没有任何兜底

内层 `LlmFrame` 是 **internally tagged + `#[serde(other)]`**，未知 `op` 降级成 `Unknown`；
外层 `RemoteC2S` / `RemoteS2C` 是 serde 默认的 **externally tagged**，而外部标签枚举
**不支持** `other` —— 未知变体就是整条 `from_str` 失败。于是两侧对 Dart 实现者的要求正好相反：

| | 数据面 `LlmFrame` | 控制面 `RemoteC2S` / `RemoteS2C` |
|---|---|---|
| 未知变体 | 降级成 `Unknown`，同帧其余内容照常 | **整条报废**，两端都必须抛错 |
| Dart 侧要写 | 一条 `default:` 分支 | **每个变体都实现出来**，没有 default 可写 |
| 漏实现的后果 | 那条内容不显示 | 那条**消息**整条丢失 |

`Ping` / `Pong` 这类 unit 变体在线上是**裸 JSON 字符串**（`"Ping"`），不是 `{"Ping":{}}`。
心跳走的正是这条，假设「消息一定是 object」的解析器会在每 25 秒一次的心跳上直接炸。

### 7.2 两侧覆盖要求不对称，这是刻意的

- **S2C 必须全部 14 个都实现**。手机是接收方，收到一条没实现的变体就是整条解析失败。
  哪怕是「手机理论上收不到」的镜像消息（`SessionStarted` / `Relay` / `ControlRequested` …）
  ——「理论上收不到」是对服务端**当前行为**的假设，不是协议保证，而 `DeviceKind`
  「手机不可被控」是 P1 才做的软策略。解出来记一条日志丢掉，成本几十行；假设它不会来，
  代价是一条静默失效的连接。
- **C2S 刻意只实现 6 个**。`RequestControl` / `DeclineControl` / `EndSession` / `Relay`
  是桌面控制端与被控端用的，手机发得出去才是真危险——`RequestControl` 会占住那台 PC 的
  **镜像独占位**，而且老服务端会照常执行成功、两端都察觉不到。不实现 = 类型系统层面发不出去，
  比在文档里写一句「不要发」硬。理由逐条写在 `mobile_golden.rs` 的 `手机不发的C2S` 常量里，
  那份清单同时是覆盖断言的一半：**有语料的 ∪ 刻意不发的 == 全集，且两者不相交**。

### 7.3 四个 C 型枚举没有 `Other` 兜底

`enums_control_plane.json` 里那四个枚举与 LLM 子协议那十个开放枚举**不是一回事**：
开放枚举有 `Other(String)` 保底，这四个没有。收到不认识的取值就是整条消息报废，
而 `ControlDenied` 恰恰是「发起被拒」的**唯一**回执，丢了它 UI 就永久转圈
（这也正是 `DenyReason` 刻意零新增变体的原因）。

⚠ `DenyReason` 与 `PairingFailReason` 有两个**重名**取值（`Expired` / `TooManyAttempts`），
但它们出现在不同消息里、语义不同（前者是「这次发起被拒」，后者是「这次配对码校验失败」）。
Dart 侧共用一套解析就会静默串味。

Rust 侧还额外断言**线格式恒等于变体名**（没有任何 `serde(rename)`），因为 Dart 侧是手写的第二份
取值表：哪天有人加了 rename，那张表会静默对不上。

### 7.4 REST DTO：此前**两端都没有守卫**

REST DTO 不在 `RemoteFrame` 里，桌面端与服务端共用同一份 Rust 类型所以永远不会漂；
手机端是第二份手写实现——改一个字段名，Rust 侧全绿，手机端要等到有人真的点了登录才发现。

最需要守的是 `DeviceInfo` 那两个 `Option + skip_serializing_if`（`device_id` / `hw_id`）：
写成 `"hw_id": null` 与键缺席在**服务端**看是同一条分支（`filter(|s| !s.is_empty())`），
所以线上看不出区别，直到某次有人把 `null` 改成空串——账户里就开始长幽灵设备
（`handlers.rs` 的 `upsert_device`：无 `hw_id` 时带一个查不到的 `device_id` 会 **INSERT 新行**）。

`rest_type` 与 `rest` 必须同时出现，取值见 `mobile_golden.rs` 的 `REST类型全集`；
新增一个 DTO 要同时加进那个常量与分派 `match`，否则测试当场红。
