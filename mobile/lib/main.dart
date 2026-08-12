/// Lumen 移动端入口。
///
/// 这里刻意只做两件事：装上 [ProviderScope]（**含真机实现的覆盖**）、把 App 交给 [LumenApp]。
/// **不要**在 main() 里塞初始化逻辑——所有需要 await 的初始化（token 读取、DB 打开、
/// 设备身份计算）都应该做成 provider，理由有二：
/// 1. provider 能被 `ProviderScope(overrides: …)` 在纯 Dart 单测里换掉，写死在 main()
///    里的初始化换不掉；
/// 2. main() 里 await 会拖长白屏时间，而移动端「回前台秒恢复」是 §7.8 的硬指标。
///
/// ## ★ 真机实现在这里覆盖，**不是**改 provider 的默认值（片 5 定的规矩）
///
/// `providers.dart` 里那些默认实现都是**能在 `flutter test` 里跑的**（内存 / 固定值）。
/// 把平台实现设成默认值的代价是：`flutter test` 从此需要 platform channel，
/// 而纯 Dart 测试是这个工程能快速跑起来的全部原因。
///
/// 覆盖清单（漏一个的症状都不是崩溃，而是「功能看起来在，但没真的生效」）：
///
/// | provider | 真机实现 | 漏了会怎样 |
/// |---|---|---|
/// | `chatStoreProvider` | [SqfliteChatStore] | **杀进程重开历史全空**（内存版一关就没） |
/// | `tokenStoreProvider` | [SecureTokenStore] | **每次冷启动都要重新登录** |
/// | `rawMachineIdProvider` | [PlatformMachineId] | **所有设备算出同一个 `hw_id`**（后果最重） |
/// | `deviceNameProvider` | [PlatformDeviceName] | 设备列表里每台手机都叫「我的手机」 |
/// | `qrScannerProvider` | [MobileScannerQrScanner] | 配对页没有扫码入口，只能手输 9 位码 |
///
/// 这张表由 `test/main_overrides_test.dart` 钉着——它断言 [deviceScope] 覆盖到了
/// 每一个需要平台实现的 provider。加了新的平台注入点，那条测试会红着提醒你回来补这里。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/app.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/data/platform_device.dart';
import 'package:lumen_mobile/data/secure_token_store.dart';
import 'package:lumen_mobile/data/sqflite_chat_store.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/pair/mobile_scanner_qr.dart';
import 'package:path/path.dart' as p;
import 'package:sqflite/sqflite.dart';

void main() => runApp(deviceScope(child: const LumenApp()));

/// 装好全部真机实现的 [ProviderScope]。
///
/// 抽成函数**不是**为了复用（只有 [main] 一个调用方），是为了让它可测：
/// `test/main_overrides_test.dart` 拿 `deviceScope(...).overrides` 与「需要平台实现的
/// provider 清单」逐条对账。写在 `runApp` 的参数里就只能靠人肉复查，而这一处漏了
/// **不会让任何测试变红**——这正是本片要堵的失败模式。
///
/// ⚠ 返回类型是 [ProviderScope] 而不是覆盖列表：`Override` 是 riverpod 的内部类型，
/// 没有出现在它的 public export 里，写不出来（而 `always_declare_return_types` 又要求
/// 必须写返回类型）。下面那个列表字面量同理**不能**标元素类型，靠参数上下文推断。
///
/// ⚠ 这些实现的**构造**都是纯 Dart 的（`const` 或懒解析），platform channel 要等到
/// 第一次真正读写才碰——所以那条测试可以放心地实例化它们，只要不调用方法。
ProviderScope deviceScope({required Widget child}) => ProviderScope(
      overrides: [
        chatStoreProvider.overrideWithValue(_realChatStore()),
        tokenStoreProvider.overrideWithValue(SecureTokenStore()),
        rawMachineIdProvider.overrideWithValue(const PlatformMachineId()),
        deviceNameProvider.overrideWithValue(const PlatformDeviceName()),
        qrScannerProvider.overrideWithValue(const MobileScannerQrScanner()),
      ],
      child: child,
    );

/// 真机上的本地库。
///
/// 路径解析是**懒的**（[SqfliteChatStore] 的构造是同步的，`getDatabasesPath()`
/// 要等到第一次读写时才跑）——所以这个函数里没有任何 await，符合 main() 的纪律。
ChatStore _realChatStore() => SqfliteChatStore(
      resolvePath: () async => p.join(await getDatabasesPath(), 'lumen_chat.db'),
    );
