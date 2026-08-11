/// App 能起来的最小冒烟测试。
///
/// 它守的不是业务，是**工程地基**：`ProviderScope` + `MaterialApp.router` + `go_router`
/// 这条接线一旦断了（比如 riverpod 大版本升级改了 `ProviderScope` 语义、go_router 改了
/// `routerConfig` 形状），这里会当场红。
///
/// 片 6 起首页是登录页——未登录时路由的 `redirect` 必须把任何位置都送回 `/login`，
/// 这一条同时守着那个 redirect。
library;

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/app.dart';

void main() {
  testWidgets('App 启动后停在登录页的「选服务器」一步', (WidgetTester tester) async {
    await tester.pumpWidget(const ProviderScope(child: LumenApp()));
    await tester.pumpAndSettle();

    // 文案与 lib/ui/login/login_page.dart 一致；改文案要同步改这里。
    expect(find.text('登录 Lumen'), findsOneWidget);
    expect(find.text('服务器地址'), findsOneWidget);
    // 还没选服务器，账号表单不该出现——两步分开正是为了让失败模式可区分。
    expect(find.text('邮箱'), findsNothing);
  });
}
