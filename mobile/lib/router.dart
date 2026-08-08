/// go_router 路由表。
///
/// 设计蓝图 §7.2 拍板的五条路由：
/// `/login`、`/devices`、`/pair/:id`、`/pick/:id`、`/chat/:conv`。
/// 片 0b 只落一条 `/` 占位路由——**刻意不先把五条空路由摆上**：空路由会让
/// `flutter analyze` 绿着、`go_router` 也绿着，然后在真机上点进去是白屏，
/// 正是 §14-6「无声降级禁令」要挡的形态。要么有页面，要么没这条路由。
///
/// 片 5 接手时的落地顺序：先加 `/login` 与 `/devices`，再把 `redirect` 接到
/// `LinkState`（§7.5 的显式 sealed 状态机），最后才是 `/chat/:conv`。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';

/// 全局路由实例。
///
/// 做成 provider 而不是顶层 `final`：片 5 起 `redirect` 要读登录态，
/// 顶层 final 拿不到 `ref`，到时候改就要动 `app.dart` 的接线。
final appRouterProvider = Provider<GoRouter>((ref) {
  return GoRouter(
    initialLocation: '/',
    routes: <RouteBase>[
      GoRoute(
        path: '/',
        builder: (context, state) => const _FoundationPlaceholderPage(),
      ),
    ],
  );
});

/// 片 0b 的占位首页：**明说自己是占位**，不假装成功能。
class _FoundationPlaceholderPage extends StatelessWidget {
  const _FoundationPlaceholderPage();

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Lumen')),
      body: const Center(
        child: Padding(
          padding: EdgeInsets.all(24),
          child: Text(
            // widget 测试按这段文字断言，改文案要同步改 test/app_boot_test.dart。
            'M7 片 0b：工程地基已就位，业务功能尚未落地。',
            textAlign: TextAlign.center,
          ),
        ),
      ),
    );
  }
}
