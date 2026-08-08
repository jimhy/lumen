# `lib/protocol/` —— 线格式层（**零 Flutter 依赖**）

> 状态：**空壳**，由 M7 **片 1-Dart** 填。本文件是给那个人的交接说明。

## 铁律（这一条不是风格偏好）

**本目录下任何文件都不许 `import 'package:flutter/...'`，只能用 `dart:core` / `dart:convert` /
`dart:typed_data`。**

两个理由：

1. 这样这一层能在**纯 Dart 测试环境**里跑，golden 语料断言不需要 widget binding；
2. 这是「P2+ 若改成 Rust 内核 + 窄门面」的**隔离边界**（设计蓝图 §7.10）——届时只换掉
   `protocol/` 与 `net/` 两层，`domain/` / `state/` / `ui/` 一行不动。一旦有 Flutter 类型
   （`Color`、`TextSpan`、`ChangeNotifier`…）漏进来，这条边界就没了，而且是悄悄没的。

## 计划文件（§7.2）

| 文件 | 职责 |
|---|---|
| `codec.dart` | externally-tagged 编解码原语（`Tagged` / `taggedStruct` / `taggedUnit` / b64） |
| `rest_dto.dart` | `LoginRequest` / `AuthResponse` / `DeviceRecord` … |
| `remote_c2s.dart` | sealed `RemoteC2S` |
| `remote_s2c.dart` | sealed `RemoteS2C` |
| `remote_frame.dart` | sealed `RemoteFrame`（**只实现移动端用到的子集**，69 个变体不全要） |
| `llm_frame.dart` | `LlmFrame`（**内部标签** `"op"`，未知 op 降级成 `LlmUnknown`） |
| `llm_model.dart` | `LlmBlock` / `LlmDeltaItem` / `LlmConvMeta` … |

## 权威源与契约

- **权威定义是 Rust 侧 `crates/lumen-protocol/src/llm.rs`**（3271 行）。设计蓝图 §5.11 的骨架
  是草案、可能落后于代码——冲突时以 `llm.rs` 为准。
- **契约说明书**：`crates/lumen-protocol/tests/golden/mobile/README.md`。**先读完它再动手**，
  它逐条写了等价判定规则、四个 Dart 专属的坑（int 是 64 位有符号 / String 按 UTF-16 计长 /
  增量可能把 emoji 拆两半 / 夹紧要落在字符边界），以及「往返断言测不出来的那一半」。
- **语料位置**：`../crates/lumen-protocol/tests/golden/mobile/*.json`。**不要复制一份进
  `mobile/`**——复制品会漂移；用相对路径直接读（`test/protocol/golden_corpus_path_test.dart`
  已经把这条路径钉住了，改目录会当场红）。

## 两个最容易先踩的坑（语料里各有一条守着）

1. **外层 `RemoteFrame` 是 externally tagged，内层 `LlmFrame` 是 internally tagged**，
   两层机制不同，别用同一个解析器。unit 变体（`Ping` / `Pong` / `EndSession` /
   `P2pStreamHello`）序列化成**裸 JSON 字符串**而不是对象——假设「载荷一定是 object」
   会在心跳上直接炸。
2. **未知字段不得攒进 `extra` map 再回显**。PC → 手机是白名单转发（§5.0），任何
   「先收下来、以后再说」的容器都是白名单的后门。`compat_unknown_field_dropped.json`
   专门守这条。
