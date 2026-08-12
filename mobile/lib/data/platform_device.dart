/// [`RawMachineIdSource`] 与 [`DeviceNameSource`] 的真机实现。
///
/// 与 `secure_token_store.dart` 同一类文件：只是 platform channel 的薄壳，
/// 默认值仍留在 `providers.dart`，真机在 `main.dart` 覆盖。
///
/// ## ★ Android 上 `device_info_plus` **拿不到**每台机器唯一的标识
///
/// `device_identity.dart` 的注释写的是「Android 取 `Settings.Secure.ANDROID_ID`」，
/// 而 `device_info_plus` 从 4.0.0 起就**移除了** `androidId`（Google 把它归类为持久标识符）。
/// 现在那个包里最像 id 的字段是 `AndroidDeviceInfo.id`，它是 **`Build.ID`——构建标签**
/// （形如 `TQ3A.230805.001`），同型号同 ROM 的机器**完全相同**。
///
/// 拿它当 `hw_id` 会踩中比「没有 hw_id」更糟的失败模式：服务端按 `(user_id, hw_id)`
/// **幂等认领**，同账户下两台同型号手机会认领到**同一行设备**，于是共用一个 `device_id`
/// ——配对信任、隐藏会话路由全部串台，而症状是间歇的、没有任何一处会报错。
///
/// 所以这里走 `MethodChannel` 直接读 `Settings.Secure.ANDROID_ID`（Kotlin 侧十几行，
/// 在 `MainActivity.kt`）。它不需要任何权限，Android 8.0 起按
/// 「应用签名密钥 + 用户 + 设备」三元组分配，**卸载重装同签名的 App 值不变**，恰好是
/// `hw_id` 要的那条不变量，而且天生就是按 App 伪名化的。
///
/// iOS 侧不需要 channel：`identifierForVendor` 在 `device_info_plus` 里就有。
/// 它的缺口（卸载最后一个同 vendor 应用后会变）是 R18，无解，已在
/// `device_identity.dart` 里记着。
library;

import 'dart:io' show Platform;

import 'package:device_info_plus/device_info_plus.dart';
import 'package:flutter/services.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/data/device_identity.dart';
import 'package:lumen_mobile/state/auth_controller.dart';

const String _tag = 'device';

/// 与 `MainActivity.kt` 里的常量**逐字对齐**。改一处不改另一处的表现是
/// `MissingPluginException` → `hw_id` 恒 null → 幽灵设备回来，而测试全绿。
const String kDeviceChannelName = 'io.github.jimhy.lumen/device';

/// 取 `ANDROID_ID` 的方法名。
const String kAndroidIdMethod = 'androidId';

/// Android 2.2 时代一批机器硬编码返回的 `ANDROID_ID`。
///
/// 现代设备不会再返回它，但山寨 ROM 上仍有实测报告。**必须当成「取不到」**：
/// 它是一个所有受影响机器共用的值，正是幂等认领最怕的那种输入。
const String _kBrokenAndroidId = '9774d56d682e549c';

/// 真机上的机器标识。
final class PlatformMachineId implements RawMachineIdSource {
  const PlatformMachineId({
    MethodChannel channel = const MethodChannel(kDeviceChannelName),
    DeviceInfoPlugin? deviceInfo,
  })  : _channel = channel,
        _deviceInfo = deviceInfo;

  final MethodChannel _channel;
  final DeviceInfoPlugin? _deviceInfo;

  @override
  Future<String?> read() async {
    try {
      if (Platform.isAndroid) return _sanitize(await _androidId());
      if (Platform.isIOS) {
        final IosDeviceInfo info =
            await (_deviceInfo ?? DeviceInfoPlugin()).iosInfo;
        return _sanitize(info.identifierForVendor);
      }
    } on Object catch (e) {
      // 取不到就是取不到。**绝不编一个随机值兜底**——那等于每次启动都是一台新机器，
      // 比没有 hw_id 更糟（`device_identity.dart` 的接口注释里写死了这条）。
      logWarn(_tag, '机器标识读取失败，本次按「无 hw_id」上报', error: e);
    }
    return null;
  }

  Future<String?> _androidId() =>
      _channel.invokeMethod<String>(kAndroidIdMethod);

  /// 空串、全空白、以及那个著名的坏值一律当作「取不到」。
  static String? _sanitize(String? raw) {
    final String? trimmed = raw?.trim();
    if (trimmed == null || trimmed.isEmpty) return null;
    if (trimmed.toLowerCase() == _kBrokenAndroidId) {
      logWarn(_tag, 'ANDROID_ID 是已知的共用坏值，按取不到处理');
      return null;
    }
    return trimmed;
  }
}

/// 真机上的设备显示名。
///
/// 这个名字只用于**展示**（设备列表里那一行），身份判据始终是 `device_id` / `hw_id`。
/// 所以它读不到时给一个通用名即可，不需要像 `hw_id` 那样宁缺毋滥。
final class PlatformDeviceName implements DeviceNameSource {
  const PlatformDeviceName({DeviceInfoPlugin? deviceInfo, this.fallback = '我的手机'})
      : _deviceInfo = deviceInfo;

  final DeviceInfoPlugin? _deviceInfo;

  /// 读不到时用的名字。
  final String fallback;

  @override
  Future<String> read() async {
    final DeviceInfoPlugin info = _deviceInfo ?? DeviceInfoPlugin();
    try {
      if (Platform.isAndroid) {
        final AndroidDeviceInfo a = await info.androidInfo;
        // `name` 是 `Settings.Global.DEVICE_NAME`（用户在设置里起的名字），最友好；
        // 部分 ROM 上是空的，退回「厂商 + 型号」。
        final String named = a.name.trim();
        if (named.isNotEmpty) return named;
        return _join(a.manufacturer, a.model);
      }
      if (Platform.isIOS) {
        final IosDeviceInfo i = await info.iosInfo;
        // ⚠ iOS 16 起，没有 `com.apple.developer.device-information.user-assigned-device-name`
        // 授权时 `name` 返回的是**设备型号**（「iPhone」），不是用户起的名字。
        // 那也够用——展示用途，且我们不打算为一个显示名去申请授权。
        final String named = i.name.trim();
        if (named.isNotEmpty) return named;
        return i.model;
      }
    } on Object catch (e) {
      logWarn(_tag, '设备名读取失败，用兜底名字', error: e);
    }
    return fallback;
  }

  String _join(String manufacturer, String model) {
    final String m = manufacturer.trim();
    final String d = model.trim();
    if (m.isEmpty) return d.isEmpty ? fallback : d;
    if (d.isEmpty) return m;
    // 型号里常常已经带了厂商名（「Xiaomi 14」/「Pixel 8」），再拼一次就成了
    // 「Xiaomi Xiaomi 14」。
    if (d.toLowerCase().startsWith(m.toLowerCase())) return d;
    return '$m $d';
  }
}
