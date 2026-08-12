/// 移动端设备身份：`hw_id`。
///
/// ## 为什么必须做（不是「优化」）
///
/// 服务端 `handlers.rs::upsert_device` 在**没有** `hw_id` 时，拿一个查不到的 `device_id`
/// 会 **INSERT 新行**（那边有一条测试明确断言这个行为）。手机重装 App、清数据、或者服务端
/// DB 重置之后，本地那个 `device_id` 就查不到了 ⇒ 每次都长一台新设备 ⇒ 设备列表里全是幽灵，
/// 而账户有 32 台上限。
///
/// 有了 `hw_id`，服务端按 `(user_id, hw_id)` **幂等认领**同一台物理机的唯一行。
///
/// ## 算法与桌面端逐字节一致
///
/// `hw_id = hex(sha256(canonical_origin ‖ 0x00 ‖ 原始机器标识))`，对齐
/// `cloud.rs::hardware_id_for_origin`。三个细节都不能改：
///
/// - **hex 小写**（桌面端 `HEX = b"0123456789abcdef"`）；
/// - 中间那个 `0x00` 分隔符不能省——没有它，`("https://a.com", "bc")` 与
///   `("https://a.comb", "c")` 会撞成同一个哈希；
/// - 原文按 **UTF-8 字节**参与哈希（Dart 侧必须 `utf8.encode`，不能用 `codeUnits`：
///   那是 UTF-16，非 ASCII 会算出另一个值）。
///
/// ## 伪名化：原始机器标识**永不上网**
///
/// 不同 origin（用户自建服务器 A / B）拿到不同 `hw_id`，服务端因此无法跨服务关联同一台
/// 手机。这在应用商店的隐私清单上是必答题，也是这里绕一道哈希而不是直接上报
/// `identifierForVendor` 的全部理由。
///
/// ## `hw_id` **不是**身份凭证
///
/// 它是客户端自报的（服务端 `handlers.rs` 直接取），同账户内任何一台机器都能伪造另一台的
/// `hw_id` 去认领其设备行。**绝不能把它当成免码信任的锚**。
///
/// ## ⚠ 已知缺口（R18，无解只能配套）
///
/// iOS 的 `identifierForVendor` 在「vendor 名下最后一个 app 被卸载」后会变：重装即分裂出
/// 新设备行 → 新 `device_id` → 旧 `device_pairs` 变成**永久残留的幽灵信任对**（旧 `devices`
/// 行还在，不触发外键级联）。目前没有清理幽灵行的代码，配合 32 台上限，长期会把用户挡在
/// 门外。需要配套「长期离线设备自动清理」或移动端显式的删除设备 UI。
library;

import 'dart:convert';

import 'package:crypto/crypto.dart';

/// 计算 `hw_id`。
///
/// [canonicalOrigin] 必须是 `core/env.dart` 规范化过的 origin——传原始用户输入会让
/// `https://a.com` 与 `a.com` 算出两个不同的 `hw_id`，也就等于没做去重。
String computeHwId({
  required String canonicalOrigin,
  required String rawMachineId,
}) {
  final List<int> bytes = <int>[
    ...utf8.encode(canonicalOrigin),
    0,
    ...utf8.encode(rawMachineId),
  ];
  return sha256.convert(bytes).toString();
}

/// 原始机器标识的来源。
///
/// 抽成接口是因为 `device_info_plus` 走 platform channel，在 `flutter test` 的 Dart VM
/// 里**不可用**——不抽的话，`hw_id` 这条链路要么没有测试，要么测试里塞一个 widget binding。
abstract interface class RawMachineIdSource {
  /// iOS 取 `identifierForVendor`、Android 取 `Settings.Secure.ANDROID_ID`。
  ///
  /// 取不到返回 null：服务端会退化回「按 `device_id` 处理」的老路径，功能不受影响，
  /// 只是失去幽灵设备防护。**不要在这里编一个随机值兜底**——那等于每次启动都是一台新机器，
  /// 比没有 `hw_id` 更糟。
  Future<String?> read();
}

/// 固定值来源（测试与「取不到」两种场景）。
final class StaticMachineId implements RawMachineIdSource {
  const StaticMachineId(this.value);

  final String? value;

  @override
  Future<String?> read() async => value;
}

/// 设备身份：把 origin 与机器标识组合成上报用的 `hw_id`。
final class DeviceIdentity {
  const DeviceIdentity(this._source);

  final RawMachineIdSource _source;

  /// 取本机在该服务器下的 `hw_id`；机器标识拿不到时返回 null。
  Future<String?> hwIdFor(String canonicalOrigin) async {
    final String? raw = await _source.read();
    if (raw == null || raw.isEmpty) return null;
    return computeHwId(canonicalOrigin: canonicalOrigin, rawMachineId: raw);
  }
}
