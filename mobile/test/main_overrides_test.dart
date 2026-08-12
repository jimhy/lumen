/// `main.dart` 那张覆盖清单的守卫。
///
/// ## 它守的是一个「不会让任何测试变红」的失败模式
///
/// 平台实现全靠 `ProviderScope(overrides: …)` 注入。漏掉一条，App 照样编译、照样启动、
/// 测试照样全绿——只是那个功能悄悄退回了测试用的默认实现：
///
/// - 漏 `tokenStoreProvider` ⇒ 凭据只在内存里 ⇒ **每次冷启动都要重新登录**；
/// - 漏 `rawMachineIdProvider` ⇒ `hw_id` 恒 null ⇒ **每次重装都长一台幽灵设备**；
/// - 漏 `chatStoreProvider` ⇒ 历史只在内存里 ⇒ **杀进程重开全空**。
///
/// 三条都要在真机上用一整轮操作才发现。所以这条测试存在的意义就是把它们提前到
/// `flutter test`。
///
/// ## 为什么可以在纯 Dart 测试里构造这些平台实现
///
/// 它们的**构造**都不碰 platform channel：`FlutterSecureStorage` / `MethodChannel` /
/// `DeviceInfoPlugin` 都是懒的，`SqfliteChatStore` 的路径解析也要等到第一次读写。
/// 所以这里只读 provider、**绝不调用它们的方法**——调了就会撞上 MissingPluginException。
library;

import 'package:flutter/widgets.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/data/device_identity.dart';
import 'package:lumen_mobile/data/memory_chat_store.dart';
import 'package:lumen_mobile/data/platform_device.dart';
import 'package:lumen_mobile/data/secure_token_store.dart';
import 'package:lumen_mobile/data/sqflite_chat_store.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/main.dart' as app;
import 'package:lumen_mobile/state/auth_controller.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/pair/mobile_scanner_qr.dart';
import 'package:lumen_mobile/ui/pair/qr_scanner.dart';

/// 需要平台实现的 provider 有几个。
///
/// 写死一个数字看着笨，但它是这条测试唯一能挡住「**加**了新注入点却忘了覆盖」的手段
/// ——下面那些 `isA` 断言只能验已经想到的那几个。数字对不上时，回 `main.dart`
/// 的覆盖清单表补一行，再改这里。
const int _kPlatformOverrideCount = 5;

void main() {
  test('真机 scope 覆盖了每一个平台注入点', () {
    final ProviderScope scope = app.deviceScope(child: const SizedBox());
    final ProviderContainer c =
        ProviderContainer.test(overrides: scope.overrides);

    expect(c.read(tokenStoreProvider), isA<SecureTokenStore>());
    expect(c.read(rawMachineIdProvider), isA<PlatformMachineId>());
    expect(c.read(deviceNameProvider), isA<PlatformDeviceName>());
    expect(c.read(qrScannerProvider), isA<MobileScannerQrScanner>());
    expect(c.read(chatStoreProvider), isA<SqfliteChatStore>());
  });

  test('★ 覆盖条数与 main.dart 的清单一致（加了新注入点这里会红）', () {
    expect(
      app.deviceScope(child: const SizedBox()).overrides,
      hasLength(_kPlatformOverrideCount),
      reason: '新增平台实现时，请同时更新 main.dart 的覆盖清单表与这里的常量',
    );
  });

  test('★ provider 的默认值仍然是纯 Dart 的（别把平台实现设成默认值）', () {
    // 这条守的是片 5 定的那条规矩。把平台实现写成默认值，代价是**全部** widget 测试
    // 从此都要起 platform channel——而纯 Dart 测试是这个工程能快速跑起来的全部原因。
    final ProviderContainer c = ProviderContainer.test();

    expect(c.read(tokenStoreProvider), isA<InMemoryTokenStore>());
    expect(c.read(rawMachineIdProvider), isA<StaticMachineId>());
    expect(c.read(deviceNameProvider), isA<StaticDeviceName>());
    expect(c.read(qrScannerProvider), isA<UnavailableQrScanner>());
    expect(c.read(chatStoreProvider), isA<MemoryChatStore>());
  });

  test('★ 没有相机时不给扫码入口（默认实现报 available = false）', () {
    // 与 `pair_page.dart` 里 `_canScan` 那一条呼应：available 是 false 时整个按钮
    // 不画，而不是画一个点了没反应的（§14-6 无声降级禁令）。
    expect(const UnavailableQrScanner().available, isFalse);
    expect(const MobileScannerQrScanner().available, isTrue);
  });
}
