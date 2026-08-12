/// go_router 路由表。
///
/// 片 7 起有四条真路由：`/login`、`/devices`、`/pair`、`/chat`。
///
/// **刻意不先把空路由摆上**：空路由会让 `flutter analyze` 绿着、`go_router` 也绿着，
/// 然后在真机上点进去是白屏，正是 §14-6「无声降级禁令」要挡的形态。
/// 要么有页面，要么没这条路由。
///
/// ## `/chat` 不带 `:conv`
///
/// 与 `/pair` 同理：当前对话已经在 `conversationSessionProvider` 里，从 URL 再取一份
/// 就有了两个事实来源。多对话列表与切换是 P1，那时再考虑要不要带参数。
///
/// ## `/pair` 不带 `:id`
///
/// 蓝图里写的是 `/pair/:id`。这里不带参数，因为**目标设备已经在 `LinkPairing` 状态里**
/// ——从 URL 再取一份就有了两个事实来源，而它们会在「切到别的设备重新发起」时错开。
/// 路径参数在有深链接需求时才有价值，而配对页恰恰是**不能**深链接的：它必须在
/// 「已收到 `PairingNeeded`」之后才存在（那是扫码四重校验之外的第五重状态闸）。
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:go_router/go_router.dart';
import 'package:lumen_mobile/state/auth_controller.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/chat/chat_page.dart';
import 'package:lumen_mobile/ui/devices/devices_page.dart';
import 'package:lumen_mobile/ui/login/login_page.dart';
import 'package:lumen_mobile/ui/pair/pair_page.dart';

/// 全局路由实例。
final Provider<GoRouter> appRouterProvider = Provider<GoRouter>((Ref ref) {
  final _AuthLinkRefresh refresh = _AuthLinkRefresh(ref);
  ref.onDispose(refresh.dispose);
  return GoRouter(
    initialLocation: '/login',
    refreshListenable: refresh,
    redirect: (BuildContext context, GoRouterState state) {
      final AuthController? auth = ref.read(authControllerProvider);
      final bool loggedIn = auth?.state is AuthLoggedIn;
      if (!loggedIn) {
        return state.matchedLocation == '/login' ? null : '/login';
      }
      // 已登录：配对中就必须停在配对页——那是唯一能输码的地方，而配对码只有
      // 120 秒，把用户留在设备列表上等于让它白白过期。
      final LinkState link = ref.read(linkControllerProvider)?.state ?? const LinkIdle();
      if (link is LinkPairing) {
        return state.matchedLocation == '/pair' ? null : '/pair';
      }
      // 会话建立即进对话页——用户点设备的目的就是去说话，多一次点击没有意义。
      if (link is LinkActive) {
        return state.matchedLocation == '/chat' ? null : '/chat';
      }
      // 没有会话时 /chat 是空的，退回设备列表。
      return state.matchedLocation == '/login' ||
              state.matchedLocation == '/pair' ||
              state.matchedLocation == '/chat'
          ? '/devices'
          : null;
    },
    routes: <RouteBase>[
      GoRoute(
        path: '/login',
        builder: (BuildContext context, GoRouterState state) =>
            const LoginPage(),
      ),
      GoRoute(
        path: '/devices',
        builder: (BuildContext context, GoRouterState state) =>
            const DevicesPage(),
      ),
      GoRoute(
        path: '/pair',
        builder: (BuildContext context, GoRouterState state) => const PairPage(),
      ),
      GoRoute(
        path: '/chat',
        builder: (BuildContext context, GoRouterState state) => const ChatPage(),
      ),
    ],
  );
});

/// 把「登录态 + 控制面状态」两条流桥成 go_router 要的 [Listenable]。
///
/// 两个 controller 都会**随服务器切换而重建**（provider 依赖 `serverEndpointProvider`），
/// 所以不能只订阅一次——`ref.listen` 在实例换掉时重新挂钩，否则换服务器之后路由就再也
/// 不跟着状态走了，表现是「登录成功但停在登录页」。
final class _AuthLinkRefresh extends ChangeNotifier {
  _AuthLinkRefresh(this._ref) {
    _ref.listen<AuthController?>(
      authControllerProvider,
      (AuthController? _, AuthController? next) => _bindAuth(next),
      fireImmediately: true,
    );
    _ref.listen<LinkController?>(
      linkControllerProvider,
      (LinkController? _, LinkController? next) => _bindLink(next),
      fireImmediately: true,
    );
  }

  final Ref _ref;
  StreamSubscription<AuthState>? _authSub;
  StreamSubscription<LinkState>? _linkSub;

  void _bindAuth(AuthController? auth) {
    unawaited(_authSub?.cancel());
    _authSub = auth?.states.listen((AuthState _) => notifyListeners());
    notifyListeners();
  }

  void _bindLink(LinkController? link) {
    unawaited(_linkSub?.cancel());
    _linkSub = link?.states.listen((LinkState _) => notifyListeners());
    notifyListeners();
  }

  @override
  void dispose() {
    unawaited(_authSub?.cancel());
    unawaited(_linkSub?.cancel());
    super.dispose();
  }
}
