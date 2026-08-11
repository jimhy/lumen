# `lib/state/` —— 状态层

> 状态：**片 5 落了控制面状态机与设备列表**；对话归约在片 7。

## 文件

| 文件 | 职责 | 状态 |
|---|---|---|
| `link_controller.dart` | **显式 sealed** `LinkState` 控制面状态机（含 12 秒发起超时） | ✅ 片 5（`test/state/link_controller_test.dart`，25 项） |
| `device_list_controller.dart` | 设备列表三态 + 并发刷新合并 | ✅ 片 5 |
| `providers.dart` | 全部 provider 定义收口 | ✅ 片 5（页面级 provider 随片 6 加） |
| `conversation_controller.dart` | 流式归约 + outbox + backfill | 片 7 |
| `streaming_tail.dart` | 末块 `ValueNotifier`（隔离高频重绘） | 片 7 |

## ★ 三条与蓝图不同、以**服务端代码**为准的事实（片 5 落地时核对出来的）

蓝图 §7.5 写于片 4b 之前，这三条已经被服务端的实际实现推翻，**以代码为准**：

1. **隐藏会话每次都要念配对码，没有「已配对免码直连」。**
   `ws.rs` 的 `OpenHidden` 臂恒传 `paired = false`，`hub.rs::submit_pairing` 在 Hidden 分支
   恒不写 `device_pairs`。那是片 4b 的**止血**：`device_pairs` 没有会话种类列，共用一行信任
   会让「手机为跟 LLM 说话念的那次码」顺带授出**镜像**权限（全屏 + 远程输入），反向则让老的
   镜像信任静默开出一条无横幅、无指示器的隐藏通道。
   ⇒ **不要做「记住这台电脑、下次免码」的 UI。**
   （`_onHiddenStarted` 仍允许 `LinkRequesting → LinkActive` 直接跃迁：服务端 `open_hidden`
   里那条 `if paired` 分支还在，将来给 `device_pairs` 加了 kind 列就会走活。）
2. **断线即会话消失。** 服务端 `disconnect` 会立刻 `teardown_hidden_for_device`，没有重连
   续挂。所以 WS 一断就回错误态，重连后要重新发起（并重新念码）。蓝图里那个 `LinkReattaching`
   因此**没有实现**——留一个永远到不了的状态，只会让后人以为有重连续挂机制。
3. **主动结束不回执。** `teardown_hidden_one` 对主动方 `except`，发 `EndHidden` 的一端收不到
   `HiddenSessionEnded`。所以 `close()` 必须就地清状态，等回执是等不到的。

## ★ `OpenHidden` 的 12 秒超时是硬验收项

老服务端不认识 `OpenHidden`，整条 `from_str` 失败后只记一条 debug 就继续读——**完全无回音**；
目标 PC 若是半开连接仍挂在服务端 `peers` 里，同样没有回音。没有这个超时就是无限转圈。

**超时文案指向「服务器/PC 版本过低」，不是「网络错误」**：让用户去重启路由器是最坏的引导。
同理 `DenyReason.offline` 也要覆盖「版本过低」——服务端在目标未上报 `hidden` 能力位时复用了
这条拒因（新增拒因会打掉老客户端的唯一回执）。

## 控制面状态机用**显式 sealed**，不照抄桌面的隐式 `Option`

桌面端用 4 个 `Option` 字段隐式表达状态（`pairing` / `incoming` / `session` / `reattach`），
于是「配对中且已有会话」这种非法组合在类型上**可表示**，只能靠代码自觉。移动端状态少、
跃迁全，改成显式 sealed：UI 直接 `switch` 出页面，漏处理一个状态编译不过。

## 设备列表：⚠ 不要重排

服务端按**注册时间升序**返回，这是一次修复的结果：老实现按 `last_seen DESC`，而 `last_seen`
每次心跳都变，导致列表每刷新一次就重排、设备跳位（海风哥反馈）。在客户端「把在线的排前面」
会把这条修复原样重新引入。要突出在线设备，用**样式**，不要用顺序。

## 归约器的三条不变量（片 7，写错了症状极难复现）

- **幂等**：`seq <= lastSeq` 直接丢——补齐与实时会重叠。
- **空洞即补齐**：`seq > lastSeq + 1` 时发 `LlmAttach{knownSeq: lastSeq}` 并 return，
  **不要**先应用再补，那会让 UI 先显示一段错位内容。
- **先拼接、再交渲染层做字素簇切分**：增量可能把一个 ZWJ emoji 拆两半，两半各自都是**合法**
  字符串，协议层无从阻止（`edge_text_append_split.json`）。每收一条就按字素簇渲染在 Flutter
  上是**可见闪烁**。

## 性能三招（片 7，长对话卡顿全出在这三处）

1. 流式期间末块渲染成纯 `Text`、块闭合才切成 Markdown；
2. Markdown Widget 按 `(itemId, revision)` memo 缓存 + LRU 裁剪；
3. `ListView.builder(reverse: true)` + `ValueKey` + `findChildIndexCallback`，流式尾巴挂
   独立 `ValueNotifier`、30 Hz 合并通知，**全程不重建整表**。`reverse: true` 天然锚底，
   不需要每次 delta 调 `animateTo`（那是长对话卡顿的头号来源）。
