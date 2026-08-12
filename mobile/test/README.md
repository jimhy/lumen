# `test/` —— Dart 单元测试与 widget 测试

## 怎么跑（**别直接敲 `flutter test`**）

```powershell
.\tool\test.ps1              # Windows：已包掉「必须清掉 *_proxy 才不卡死」这个坑
./tool/test.sh               # bash / WSL / CI 本地复现
```

理由见仓库 `mobile/README.md`「本机环境三个坑」。CI 上没有代理，`mobile.yml` 直接跑
`flutter test`，两边都成立。

## 现有测试（230 项）

| 文件 | 守什么 |
|---|---|
| `app_boot_test.dart` | `ProviderScope` + `MaterialApp.router` + go_router 这条接线还活着 |
| `pubspec_lock_test.dart` | lock 里不得残留镜像 URL（否则 CI 的 `--enforce-lockfile` 必红） |
| `protocol/corpus.dart` | **不是测试**，是三个 golden 测试共用的语料读取与 `deepJsonEquals` |
| `protocol/golden_corpus_path_test.dart` | 语料**还在原地**、载荷键与文件名前缀相符 |
| `protocol/golden_test.dart` | 内层 `LlmFrame` ⇄ Dart 模型（往返 + 变体覆盖 + 取值断言 + `debug_forbidden`） |
| `protocol/control_plane_golden_test.dart` | 控制面 `c2s_*` / `s2c_*` / `enums_*`（片 5） |
| `protocol/rest_golden_test.dart` | REST DTO `rest_*`，含「可选字段的键必须消失」与密码/token 脱敏（片 5） |
| `core/env_test.dart` | origin 规范化，**与 `cloud.rs` 同一批用例**（片 5） |
| `data/device_identity_test.dart` | `hw_id` 的三个独立算出来的向量（片 5） |
| `net/backoff_test.dart` | 指数序列 + 全抖动下界 50%（片 5） |
| `net/ws_client_test.dart` | 心跳 / Pong 看门狗 / 退避 / 前后台 / 代次守卫，22 项（片 5） |
| `net/rest_client_test.dart` | 401 → refresh → retry 的三个坑，13 项（片 5） |
| `state/link_controller_test.dart` | 控制面状态机，含 **12 秒发起超时**，25 项（片 5） |
| `support/fake_scheduler.dart` / `support/fake_ws_socket.dart` | 手动推进的假时钟与内存假 socket |

## 还没写的（随对应分片一起补）

| 文件 | 守什么 | 分片 |
|---|---|---|
| `state/conversation_reduce_test.dart` | 归约器的幂等 / 空洞补齐 / emoji 拆半 | 片 7 |
| `domain/line_diff_test.dart` | 行级 LCS | 片 9 |

## 时间相关的行为怎么测（片 5 立的规矩）

**不要用真 `Timer`。** 四条时间线（心跳 25s / Pong 看门狗 15s / 后台宽限 25s / 发起超时 12s）
如果要真的等，现实里的结果一定是「它们没有测试」——而这四条每一条错了都只在真机弱网下现形。

`net/scheduler.dart` 把「延迟执行」抽成接口，测试注入 `support/fake_scheduler.dart`，
25 秒就是一行 `clock.advance(...)`，还测得了「刚好没到 / 刚好到了」的边界。
同理 `ws_client.dart` 零 `dart:io` 依赖，socket 也是注入的。

**不用 `package:fake_async`**：它是 `flutter_test` 的传递依赖，直接 import 会被
`depend_on_referenced_packages` 拦下；显式加进 dev_dependencies 又要动 `pubspec.lock`。

## 写 golden 测试前必读的两条（否则会写出「全绿但没测到东西」的测试）

1. **`deepJsonEquals` 的叶子比较必须连类型一起比。** Dart 的 `num` 相等**跨 int/double**：
   `240262 == 240262.0` 为 `true`。不加 `if ((a is int) != (b is int)) return false;`，
   一个把 `seq` / `cost_micro_usd` 存成 `double` 的模型会对**几乎全部**语料绿灯通过。
2. **往返断言对「同型字段接反」完全免疫。** `conv_id`/`conv_generation`、`seq`/`turn`、
   `input_tokens`/`output_tokens` 互换后 `encode∘decode` 照样是不动点。语料已经刻意给它们
   配了互不相同的取值，但那只是弹药——必须另外写**取值断言**去用它
   （`crates/lumen-protocol/tests/golden/mobile/README.md` §3 第 7 条列了具体该断言哪些）。

## 边界：什么不进 `test/`

走 platform channel 的东西（`sqflite` 真开库、`flutter_secure_storage` 真读 Keystore）
进 `integration_test/`。**⚠ `sqflite` 在 `flutter test` 下不可用**，DAO 单测要么用内存实现，
要么用 dev 依赖 `sqflite_common_ffi`。
