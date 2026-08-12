/// App 壳：`MaterialApp.router` + 主题。
///
/// 现状（M7 片 0b「工程地基」）：只有一个占位首页，**没有任何业务功能**。
/// 这不是半成品掩饰——片 0b 的验收目标就是「`flutter analyze` / `flutter test` 在本机与 CI
/// 上都能跑绿」，所以这里放的是能编译、能被 widget test 断言到的最小真实内容。
/// 登录 / 设备列表 / 配对 / 项目选择 / 对话五个页面分别是片 5、6、7 的活，落到 `lib/ui/` 下，
/// 路由挂在 [appRouterProvider]（见 `router.dart`）。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/router.dart';
import 'package:lumen_mobile/ui/lifecycle_bridge.dart';

/// 顶层 App。
///
/// 用 [ConsumerWidget] 而不是 [StatelessWidget]：路由本身要读 provider
/// （片 5 起，`LinkState` 决定 redirect 到 /login 还是 /devices），提前把 `ref` 拿在手上，
/// 免得那时再改一次继承关系。
class LumenApp extends ConsumerWidget {
  /// 创建 App 壳。
  const LumenApp({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    // ★ [LifecycleBridge] 包在 `builder` 里而不是包在 `MaterialApp.router` 外面：
    // 它要 `ref.read(conversationSessionProvider)`，而那条 provider 链最终依赖路由
    // 决定出来的状态。包在外面同样能编译，但它会在路由初始化之前就跑一次
    // `didChangeAppLifecycleState`——那时 session 还是 null，第一次进后台不落盘。
    return MaterialApp.router(
      title: 'Lumen',
      debugShowCheckedModeBanner: false,
      builder: (BuildContext context, Widget? child) =>
          LifecycleBridge(child: child ?? const SizedBox.shrink()),
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(seedColor: const Color(0xFF3B6EA5)),
        useMaterial3: true,
      ),
      darkTheme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
          seedColor: const Color(0xFF3B6EA5),
          brightness: Brightness.dark,
        ),
        useMaterial3: true,
      ),
      routerConfig: ref.watch(appRouterProvider),
    );
  }
}
