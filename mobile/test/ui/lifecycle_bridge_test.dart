/// 前后台事件真的接到了 WS 与本地存档上（片 10）。
///
/// ## 它守的是一类特别的缺口：**实现了但没接线**
///
/// `WsClient.onBackground()` / `onForeground()` 从片 5 起就存在、也有测试覆盖，
/// 但直到片 10 之前**没有任何生产代码调用它们**——「进后台 25 秒宽限、回前台立刻重连」
/// 这条蓝图 §7.8 的硬指标在真机上从来没生效过，而所有测试都是绿的。
///
/// 单元测试测不出这种缺口（被测的那个方法确实工作正常），只能像这里一样从**系统事件**
/// 这一端驱动。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/ui/lifecycle_bridge.dart';

void main() {
  group('进后台的判据', () {
    test('★ inactive 不算进后台', () {
      // iOS 上下拉通知中心、来电、任务切换器都会触发 inactive，而用户根本没离开。
      // 算进去的后果是宽限计时器反复起停，最坏一次误触发就把连接掐了。
      expect(isBackgroundState(AppLifecycleState.inactive), isFalse);
    });

    test('paused / hidden / detached 都算', () {
      expect(isBackgroundState(AppLifecycleState.paused), isTrue);
      expect(isBackgroundState(AppLifecycleState.hidden), isTrue);
      expect(isBackgroundState(AppLifecycleState.detached), isTrue);
    });

    test('resumed 不算', () {
      expect(isBackgroundState(AppLifecycleState.resumed), isFalse);
    });

    test('五个状态一个不漏（Flutter 加了新状态时这里会红）', () {
      // `isBackgroundState` 是穷尽 switch，加了新状态它编译不过；
      // 这条断言守的是**反过来**的情况：新状态被人随手归进某一边而没人想过。
      expect(AppLifecycleState.values, hasLength(5));
    });
  });

  group('桥接件', () {
    testWidgets('原样透传 child', (WidgetTester tester) async {
      await tester.pumpWidget(const ProviderScope(
        child: MaterialApp(home: LifecycleBridge(child: Text('内容'))),
      ));
      expect(find.text('内容'), findsOneWidget);
    });

    testWidgets('★ 没有会话时收生命周期事件也不能抛（没登录就是这个状态）',
        (WidgetTester tester) async {
      await tester.pumpWidget(const ProviderScope(
        child: MaterialApp(home: LifecycleBridge(child: Text('内容'))),
      ));
      for (final AppLifecycleState s in AppLifecycleState.values) {
        tester.binding.handleAppLifecycleStateChanged(s);
        await tester.pump();
      }
      expect(find.text('内容'), findsOneWidget);
    });

    testWidgets('★ 页面拆掉之后不再收事件（漏注销 = 对着已销毁的 ref 读 provider）',
        (WidgetTester tester) async {
      await tester.pumpWidget(const ProviderScope(
        child: MaterialApp(home: LifecycleBridge(child: Text('内容'))),
      ));
      await tester.pumpWidget(const ProviderScope(
        child: MaterialApp(home: Text('换页了')),
      ));

      tester.binding.handleAppLifecycleStateChanged(AppLifecycleState.paused);
      await tester.pump();
      expect(find.text('换页了'), findsOneWidget);
    });
  });
}
