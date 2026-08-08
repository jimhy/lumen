# `test/` —— Dart 单元测试与 widget 测试

## 怎么跑（**别直接敲 `flutter test`**）

```powershell
.\tool\test.ps1              # Windows：已包掉「必须清掉 *_proxy 才不卡死」这个坑
./tool/test.sh               # bash / WSL / CI 本地复现
```

理由见仓库 `mobile/README.md`「本机环境三个坑」。CI 上没有代理，`mobile.yml` 直接跑
`flutter test`，两边都成立。

## 现有测试

| 文件 | 守什么 |
|---|---|
| `app_boot_test.dart` | `ProviderScope` + `MaterialApp.router` + go_router 这条接线还活着 |
| `protocol/golden_corpus_path_test.dart` | golden 语料**还在原地**、信封形状仍是 README §1 那个样子 |

## 片 1-Dart 要补的（§7.2）

| 文件 | 守什么 |
|---|---|
| `protocol/golden_test.dart` | 语料 ⇄ Dart 模型（往返 + 变体覆盖 + 取值断言 + `debug_forbidden`） |
| `protocol/codec_test.dart` | externally / internally tagged 两套原语 |
| `state/conversation_reduce_test.dart` | 归约器的幂等 / 空洞补齐 / emoji 拆半 |
| `domain/line_diff_test.dart` | 行级 LCS |

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
