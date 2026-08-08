/// App 能起来的最小冒烟测试。
///
/// 它守的不是业务，是**工程地基**：`ProviderScope` + `MaterialApp.router` + `go_router`
/// 这条接线一旦断了（比如 riverpod 大版本升级改了 `ProviderScope` 语义、go_router 改了
/// `routerConfig` 形状），这里会当场红，而不是等到片 5 写页面时才发现。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/app.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

void main() {
  testWidgets('App 能启动并渲染出片 0b 的占位首页', (tester) async {
    await tester.pumpWidget(const ProviderScope(child: LumenApp()));
    await tester.pumpAndSettle();

    // 文案与 lib/router.dart 的 _FoundationPlaceholderPage 一致；改文案要同步改这里。
    expect(find.text('M7 片 0b：工程地基已就位，业务功能尚未落地。'), findsOneWidget);
  });
}
