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
///
/// ⚠ `tokenStoreProvider`（Keychain / Keystore）与 `rawMachineIdProvider`
/// （`device_info_plus`）**还没接**，见交接文档。它们漏了的症状分别是
/// 「每次冷启动都要重新登录」与「所有设备算出同一个 hw_id」。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/app.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/data/sqflite_chat_store.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:path/path.dart' as p;
import 'package:sqflite/sqflite.dart';

void main() {
  runApp(
    ProviderScope(
      // ⚠ 这里**不能**写显式的元素类型：`Override` 是 riverpod 的内部类型，
      // 没有出现在它的 public export 里，写出来编译不过。靠参数类型推断。
      overrides: [chatStoreProvider.overrideWithValue(_realChatStore())],
      child: const LumenApp(),
    ),
  );
}

/// 真机上的本地库。
///
/// 路径解析是**懒的**（[SqfliteChatStore] 的构造是同步的，`getDatabasesPath()`
/// 要等到第一次读写时才跑）——所以这个函数里没有任何 await，符合 main() 的纪律。
ChatStore _realChatStore() => SqfliteChatStore(
      resolvePath: () async => p.join(await getDatabasesPath(), 'lumen_chat.db'),
    );
