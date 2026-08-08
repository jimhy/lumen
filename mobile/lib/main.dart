/// Lumen 移动端入口。
///
/// 这里刻意只做两件事：装上 [ProviderScope]、把 App 交给 [LumenApp]。
/// **不要**在 main() 里塞初始化逻辑——所有需要 await 的初始化（token 读取、DB 打开、
/// 设备身份计算）都应该做成 provider，理由有二：
/// 1. provider 能被 `ProviderScope(overrides: …)` 在纯 Dart 单测里换掉，写死在 main()
///    里的初始化换不掉；
/// 2. main() 里 await 会拖长白屏时间，而移动端「回前台秒恢复」是 §7.8 的硬指标。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/app.dart';

void main() {
  runApp(const ProviderScope(child: LumenApp()));
}
