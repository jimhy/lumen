# `lib/domain/` —— 领域模型（协议之上、UI 之下）

> 状态：**空壳**，由 M7 **片 6** 填。

## 计划文件（§7.2）

| 文件 | 职责 |
|---|---|
| `chat_item.dart` | sealed `ChatItem` / `ToolCall`（**归约后**的条目，不是线格式类型） |
| `tool_render.dart` | 工具名 → 卡片形态映射（**必须有未知兜底**，CLI 会持续新增工具名） |
| `line_diff.dart` | `old_string` / `new_string` → 行级 LCS 统一 diff（零依赖） |

## 为什么这一层与 `protocol/` 分开

`protocol/` 的类型是**线格式的形状**，字段名、可选性、降级语义都由 Rust 侧
`crates/lumen-protocol/src/llm.rs` 说了算，改它要走「先加语料、两侧同时红」的流程。
`domain/` 的类型是**渲染需要的形状**（合并过的气泡、拼好的工具调用、算好的 diff），
只对 UI 负责。把两者混成一个类型的代价是：一次协议加字段就要动 UI，一次 UI 改版就想改协议。

`tool_render.dart` 的枚举兜底（§7.6）：

```dart
enum ToolShape { read, edit, write, bash, search, web, todo, task, generic }
// 未知工具名一律落 generic —— 不是 throw、不是 null。
```
