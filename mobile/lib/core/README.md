# `lib/core/` —— 跨层基础设施

> 状态：**M7 片 5 已落地**。

## 文件

| 文件 | 职责 | 测试 |
|---|---|---|
| `result.dart` | sealed `Ok` \| `Err`，**禁 throw 穿层** | 随各层一起 |
| `env.dart` | 服务器地址规范化 + `ServerEndpoint`（REST base / WS URL / 明文判据） | `test/core/env_test.dart` |
| `log.dart` | 统一日志出口（可替换 sink，单测可断言「这条降级真的报了」） | 随各层一起 |

## 为什么是 `Result` 而不是异常

Dart 的异常不进类型签名，调用方不看实现就不知道哪些函数会抛、抛什么。协议层与网络层的
失败是**常规路径**（对端老版本、帧解析失败、连接断开、密码错），不是异常情况，用 sealed
`Result` 表达能拿到「漏处理错误分支编译不过」的保护——与 Rust 侧 `Result` 同源。

例外只有一处：**协议层内部的致命线格式错误**（`WireFormatException`）。golden 语料里有一条
用例就是要求 `decode` 抛异常（`compat_open_enum_shape_change_is_fatal.json`，宽容成
`Other("42")` 反而与 Rust 不一致）。那个异常**不穿层**：net 层收到它立刻转成 `Err`。

## `canonicalServerOrigin` 必须与桌面端逐字节一致

`hw_id = sha256(canonical_origin ‖ 0x00 ‖ 机器标识)`，origin 差一个字符就是另一台设备：

- 对**用户**而言，`example.com` / `https://example.com/` / `https://EXAMPLE.com:443`
  是同一台服务器，不收敛就会让同一台手机在服务端分裂成三台幽灵设备；
- 对**两端**而言，PC 与手机输入同一个地址却算出不同 origin，将来任何按 origin 分区的东西
  （凭据存储、`hw_id`）都会静默错位。

`test/core/env_test.dart` 的合法/非法两组用例**逐条抄自** `cloud.rs` 的
`canonical_origin_规范化` 与 `canonical_origin_拒绝非根与注入`。

⚠ 有一处两端的 URL 解析器口径不同、必须显式补齐：**反斜杠**。桌面端把 `\` 当 authority
终止符后判「有多余后缀」而拒绝；Dart 的 `Uri` 会把它当普通路径字符。`env.dart` 显式拒掉，
不指望两套解析器自己对齐。

## 日志纪律

降级路径必须 `log` **并且** UI 可见（§14-6 无声降级禁令）。**只 log 不给 UI 不算数**——
用户在手机上看不到日志。这个文件只保证「有地方可 log」，另一半由各 controller 的错误态负责。
