# `lib/data/` —— 本地持久化与设备身份

> 状态：**片 5 落了设备身份与凭据抽象**；三张表（sqflite）在片 10。

## 文件

| 文件 | 职责 | 状态 |
|---|---|---|
| `device_identity.dart` | `hw_id = hex(sha256(canonical_origin ‖ 0x00 ‖ 原始机器标识))` | ✅ 片 5 |
| `token_store.dart` | `SessionTokens` + `TokenStore` 抽象 + 内存实现 | ✅ 片 5（平台实现在片 6） |
| `chat_db.dart` | 抽象 DAO（单测可换内存实现） | 片 10 |
| `chat_db_sqflite.dart` | sqflite 实现，三张表 | 片 10 |

## `hw_id`：为什么必须做，以及三个不能改的细节

服务端 `handlers.rs::upsert_device` 在**没有** `hw_id` 时，拿一个查不到的 `device_id`
会 **INSERT 新行**（那边有测试明确断言这个行为）。手机重装、清数据、或服务端 DB 重置之后，
本地那个 `device_id` 就查不到了 ⇒ 每次都长一台新设备 ⇒ 设备列表全是幽灵，而账户有 32 台上限。

算法与 `cloud.rs::hardware_id_for_origin` **逐字节一致**，三个细节都不能改：

1. **hex 小写**（桌面端 `HEX = b"0123456789abcdef"`）；
2. 中间那个 `0x00` 分隔符不能省——没有它，`("https://a.com", "bc")` 与
   `("https://a.comb", "c")` 会撞成同一个哈希；
3. 原文按 **UTF-8 字节**参与哈希（Dart 必须 `utf8.encode`，用 `codeUnits`（UTF-16）
   会算出另一个值且**不报错**）。

`test/data/device_identity_test.dart` 的期望值是**独立算出来的**
（`printf 'origin\x00id' | sha256sum`），不是把实现跑一遍抄下来——后者只能证明实现是
确定性的，证明不了它算对了。

### 三条必须记住的性质

- **伪名化**：不同 origin 得到不同 `hw_id`，服务端无法跨服务关联同一台手机。这是应用商店
  隐私清单上的必答题，也是绕这道哈希而不是直接上报 `identifierForVendor` 的全部理由。
  **原始机器标识永不上网。**
- **`hw_id` 不是身份凭证**：它是客户端自报的，同账户内任何机器都能伪造另一台的 `hw_id`
  去认领其设备行。**绝不能把它当成免码信任的锚。**
- **取不到时返回 null，不许编一个随机值兜底**——那等于每次启动都是一台新机器，比没有更糟。

### ⚠ 已知缺口（R18，需要配套而不是硬修）

iOS 的 `identifierForVendor` 在「vendor 名下最后一个 app 被卸载」后会变：重装即分裂出新
设备行 → 新 `device_id` → 旧 `device_pairs` 变成**永久残留的幽灵信任对**（旧 `devices` 行
还在，不触发外键级联）。配合 32 台上限，长期会把用户挡在门外。需要「长期离线设备自动清理」
或移动端显式的删除设备 UI。

## 凭据

- **JWT 必须进 Keychain / Keystore，不是 SharedPreferences**：服务端 JWT 无 `jti`、无版本号、
  改密码不失效，默认 TTL 7 天，**唯一撤销手段是删设备行**，泄漏代价高。
- `SessionTokens` 带 `origin`：凭据是**按 origin 分区**的。不带它就会出现「换服务器后拿旧
  token 打新服务器、收 401、以为是密码错了」。
- **生物锁与免码强制绑定**（§9.3）：关掉生物锁 = 清空本机全部信任对。`device_pairs` 免码把
  「每次都要物理接触那台 PC」这条不变量废掉了，手机端唯一等强度的替代就是「物理持有一台已
  通过生物识别解锁的手机」。**执行点在片 6 的设置页**，片 5 只立了存储抽象。

## ⚠ `sqflite` 在 `flutter test` 下不可用

走 platform channel，Dart VM 没有实现。DAO **必须**藏在 `chat_db.dart` 的抽象接口后，
单测用内存实现或 dev 依赖里的 `sqflite_common_ffi`。否则一定会长出这两种坏形态之一：
「本地测试全绿、真机崩」，或「为了能测而把持久化写进 widget test」。

同理 `device_info_plus` 也走 platform channel，所以 `RawMachineIdSource` 是个接口，
默认实现返回 null，真机实现在片 6 由 `main.dart` 用 `ProviderScope(overrides:)` 覆盖。
