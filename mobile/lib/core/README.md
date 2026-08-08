# `lib/core/` —— 跨层基础设施

> 状态：**空壳**，由 M7 **片 5** 填（`Result` 可能提前到片 1-Dart）。

## 计划文件（§7.2）

| 文件 | 职责 |
|---|---|
| `env.dart` | 服务端地址等环境配置（**不放任何密钥**） |
| `log.dart` | 统一日志出口 |
| `result.dart` | sealed `Ok` \| `Err`，**禁 throw 穿层** |

## 为什么是 `Result` 而不是异常

Dart 的异常不进类型签名，调用方不看实现就不知道哪些函数会抛、抛什么。协议层与网络层的
失败是**常规路径**（对端老版本、帧解析失败、连接断开），不是异常情况，用 sealed `Result`
表达能拿到「漏处理错误分支编译不过」的保护——与 Rust 侧 `Result` 同源。

例外只有一处：**golden 语料里 `expect == "decode_error"` 的用例要求 `decode` 抛异常**
（`compat_open_enum_shape_change_is_fatal.json`，把它宽容成 `Other("42")` 反而与 Rust 不一致）。
那是协议层内部的致命边界，不穿层。

## 日志纪律

降级路径必须 `log` + UI 可见（§14-6 无声降级禁令）。**只 log 不给 UI 不算数**——
用户在手机上看不到日志。
