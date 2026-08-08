# `lib/data/` —— 本地持久化与设备身份

> 状态：**空壳**，由 M7 **片 5** 填。

## 计划文件（§7.2 / §7.7）

| 文件 | 职责 |
|---|---|
| `chat_db.dart` | **抽象 DAO**（单测可换内存实现） |
| `chat_db_sqflite.dart` | sqflite 实现，三张表 |
| `token_store.dart` | `flutter_secure_storage` + 生物识别包裹 |
| `device_identity.dart` | `hw_id = sha256(origin ‖ 0x00 ‖ vendorId)` |

## 三张表（§7.7）

- `conversations`：会话元信息
- `events`：主键 `(conv_id, seq)` —— 天然给了断线补齐所需的 `last_seq` 水位线和幂等去重
- `outbox`：乐观发送队列

## 三条硬约束

1. **⚠ `sqflite` 在 `flutter test` 下不可用**（走 platform channel，Dart VM 没有实现）。
   所以 DAO **必须**藏在 `chat_db.dart` 的抽象接口后，单测用内存实现或 dev 依赖里的
   `sqflite_common_ffi`。否则一定会长出这两种坏形态之一：「本地测试全绿、真机崩」，
   或者「为了能测而把持久化写进 widget test」。
2. **JWT 必须进 Keychain / Keystore，不是 SharedPreferences**：服务端 JWT 无 `jti`、无版本号、
   改密码不失效，默认 TTL 7 天，**唯一撤销手段是删设备行**，泄漏代价高。
3. **⚠ iOS 的 `identifierForVendor` 卸载重装即变**（R18）→ 新 `device_id` → 服务端旧 pair
   永久残留成幽灵信任对。需配套「长期离线设备自动清理」或显式删除设备 UI（§9.4）。
