# `lib/state/` —— Riverpod 状态层

> 状态：**空壳**，由 M7 **片 5 / 片 6** 填。

## 计划文件（§7.2）

| 文件 | 职责 |
|---|---|
| `providers.dart` | 全部 provider 定义收口（一处可查，避免满仓库找 provider） |
| `link_controller.dart` | **显式 sealed** `LinkState` 控制面状态机 |
| `device_list_controller.dart` | 设备列表 |
| `conversation_controller.dart` | 流式归约 + outbox + backfill |
| `streaming_tail.dart` | 末块 `ValueNotifier`（隔离高频重绘） |

## 两条要点

### 1. 控制面状态机用**显式 sealed**，不照抄桌面的隐式 `Option`（§7.5）

桌面端用 4 个 `Option` 字段隐式表达状态（`pairing` / `incoming` / `session` / `reattach`）。
移动端状态少、跃迁全，改成显式 sealed，UI 可以直接 `switch` 出页面——同时拿到「漏处理新状态
编译不过」的保护，这正是选 Dart 3 sealed class 的原因（§4.1）。

### 2. 归约器的三条不变量（§7.6，写错了症状极难复现）

- **幂等**：`seq <= lastSeq` 直接丢——补齐与实时会重叠。
- **空洞即补齐**：`seq > lastSeq + 1` 时发 `LlmAttach{knownSeq: lastSeq}` 并 return，
  **不要**先应用再补，那会让 UI 先显示一段错位内容。
- **先拼接、再交渲染层做字素簇切分**：增量可能把一个 ZWJ emoji 拆两半，两半各自都是**合法**
  字符串，协议层无从阻止（`edge_text_append_split.json`）。每收一条就按字素簇渲染在 Flutter 上
  是**可见闪烁**。

### 性能三招（长对话卡顿全出在这三处）

1. 流式期间末块渲染成纯 `Text`、块闭合才切成 Markdown；
2. Markdown Widget 按 `(itemId, revision)` memo 缓存 + LRU 裁剪；
3. `ListView.builder(reverse: true)` + `ValueKey` + `findChildIndexCallback`，
   流式尾巴挂独立 `ValueNotifier`、30 Hz 合并通知，**全程不重建整表**。
   `reverse: true` 天然锚底，不需要每次 delta 调 `animateTo`（那是长对话卡顿的头号来源）。
