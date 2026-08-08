# `integration_test/` —— 真机集成测试

> 状态：**空壳**，由 M7 **片 5 之后**填。**CI 不跑这里**（`mobile.yml` 只跑
> `flutter analyze` + `flutter test`，跑真机集成要模拟器，代价与收益不成比例）。

## 边界：什么该进这里，什么不该

| 该进 | 不该进 |
|---|---|
| 走 platform channel 的东西：`flutter_secure_storage` 真读 Keystore、`sqflite` 真开库、`device_info_plus` 真取 vendorId | 协议编解码（纯 Dart，进 `test/protocol/`） |
| 端到端冒烟：登录 → 设备列表 → 配对 → 发一句话 → 收到流式回复 | 归约逻辑（可用 `ProviderScope(overrides:)` 在 `test/` 里跑） |

## ⚠ 有两件事集成测试也证明不了，必须两机肉眼验收（§14-5）

配对 / 扫码 / 流式渲染 / 断线补齐 —— 这四项需要「一台 PC + 一部手机」的真实拓扑，
自动化测试环境里没有对端。**每片过两机验收**是硬要求，不是可选项。
