# `lib/protocol/` —— 线格式层（**零 Flutter 依赖**）

> 状态：**全部落地**——LLM 子协议（片 1-Dart）+ 控制面与 REST（片 5）。

## 铁律（这一条不是风格偏好）

**本目录下任何文件都不许 `import 'package:flutter/...'`，只能用 `dart:core` / `dart:convert` /
`dart:typed_data`。**

两个理由：

1. 这样这一层能在**纯 Dart 测试环境**里跑，golden 语料断言不需要 widget binding；
2. 这是「P2+ 若改成 Rust 内核 + 窄门面」的**隔离边界**（设计蓝图 §7.10）——届时只换掉
   `protocol/` 与 `net/` 两层，`domain/` / `state/` / `ui/` 一行不动。一旦有 Flutter 类型
   （`Color`、`TextSpan`、`ChangeNotifier`…）漏进来，这条边界就没了，而且是悄悄没的。

## 现有文件

| 文件 | 职责 | 状态 |
|---|---|---|
| `codec.dart` | 线格式原语：带字段名的类型化读取器、`Option`/`default` 的两套写出规则、UTF-8 夹紧、开放式 C 型枚举、内部标签枚举骨架 | ✅ |
| `llm_enums.dart` | 10 个开放式 C 型枚举（线上是裸字符串，`Known \| Other(原文)`） | ✅ |
| `llm_model.dart` | 块 / 用量 / 对话元信息 / 轮记录 / 增量项 / 审批，含三个脱敏包装 | ✅ |
| `llm_frame.dart` | `LlmFrame` 30 个变体（**内部标签** `"op"`，未知 op 降级成 `LlmUnknown`） | ✅ |
| `remote_frame.dart` | 外层 `RemoteFrame` 信封（**externally tagged**，只实现移动端用得到的子集） | ✅ 子集 |
| `rest_dto.dart` | `LoginRequest` / `AuthResponse` / `DeviceRecord` … + `LumenRoutes` 路径常量 | ✅ 片 5 |
| `remote_c2s.dart` | 控制面上行，**只实现手机会发的 6 个变体** | ✅ 片 5 |
| `remote_s2c.dart` | 控制面下行，**全部 14 个变体** + 四个封闭式 C 型枚举 | ✅ 片 5 |

## 控制面与数据面的口径**相反**（片 5 新增，读之前别动那两个文件）

| | 内层 `LlmFrame`（数据面） | 外层 `RemoteC2S` / `RemoteS2C`（控制面） |
|---|---|---|
| 标签 | internally tagged（`"op"`） | externally tagged（变体名当唯一 key） |
| 未知变体 | 降级成 `Unknown`，同帧其余内容照常 | **整条报废**（serde 的外部标签枚举不支持 `other`） |
| 这一层要写 | 一条 `default:` 分支 | **每个变体都实现出来**，没有 default 可写 |

两侧的覆盖要求也不对称，这是刻意的：

- **S2C 全实现**（14 个）。手机是接收方，收到一条没实现的变体就是整条消息失败。哪怕是
  「理论上收不到」的镜像消息——「理论上」是对服务端**当前行为**的假设，不是协议保证。
- **C2S 只实现 6 个**。`RequestControl` / `DeclineControl` / `EndSession` / `Relay` 是桌面端与
  被控端的；手机发得出 `RequestControl` 才是真危险（占住那台 PC 的镜像独占位，而且**老服务端
  会照常执行成功**、两端都察觉不到）。不实现 = 类型层面发不出去。

**unit 变体是裸字符串**（`"Ping"` / `"Pong"`），不是 `{"Ping":{}}`。心跳走的正是这条。

四个封闭式 C 型枚举（`DenyReason` / `PairingFailReason` / `EndReason` / `RemoteRole`）
**没有 `Other` 兜底**，与 LLM 那十个开放枚举不是一回事：收到不认识的取值就是整条消息报废，
而 `ControlDenied` 是「发起被拒」的**唯一**回执。⚠ 前两个有重名取值（`Expired` /
`TooManyAttempts`）但语义不同，共用一套解析会静默串味。

## 三条已经被测试钉死的规则

写新类型前先读懂这三条，它们各有一组语料守着（`mobile/test/protocol/golden_test.dart`）：

1. **`Option` + `skip_serializing_if` 的字段，`toJson` 里必须让键整个消失**，不能写成
   `'model': null`。用 `writeOpt()`，别手写 `out['model'] = model`。
   而**带 `default` 但不带 skip** 的字段（`fork_session` / `caps` / `usage` …）相反：
   输出里一定要出现。两者混淆是手写模型最常见的漂移。
2. **整数字段只认 `int`，不接受 `double`。** Dart 的 `num` 相等跨 `int`/`double`
   （`240262 == 240262.0` 为真），所以一处把整数存成 `double` 几乎不会被往返断言发现。
   读取器已经在 `readInt` 里把关，别绕过它写 `m['seq'] as num`。
3. **未知内容降级但不回显。** 每个内部标签枚举都有 `Unknown` 兜底，未知标签值连同整个
   body 一起丢弃；**不得**把未知键攒进 `extra` map 再写回——PC → 手机是白名单转发，
   任何「先收下来、以后再说」的容器都是白名单的后门。

## 权威源与契约

- **权威定义是 Rust 侧 `crates/lumen-protocol/src/llm.rs`**。设计蓝图 §5.11 的骨架是草案、
  可能落后于代码——冲突时以 `llm.rs` 为准。
- **契约说明书**：`crates/lumen-protocol/tests/golden/mobile/README.md`。改协议前先读它的
  §5「加新变体的流程」——**语料先行**是硬顺序，反过来做会留一个静默的洞。
- **语料位置**：`../crates/lumen-protocol/tests/golden/mobile/*.json`，直接按相对路径读，
  **不要复制一份进 `mobile/`**（复制品会漂移；`test/protocol/golden_corpus_path_test.dart`
  已经把这条路径钉住了）。

## 往返断言只覆盖了一半，另一半在哪

`encode∘decode` 的不动点检查对**同类型字段互换完全免疫**——把 `known_seq` 与 `known_turn`
在解码和编码两侧同时接反，65 条语料会**全部照绿**。实测确认过这一点：只有
`golden_test.dart` 末尾那组**取值断言**能抓住它。所以：

**给已有变体加同型字段时，记得回去给那组取值断言补一行。** 语料已经刻意给相邻同型字段
配了互不相同的取值，就是为了让这类断言写得出来。
