/// 全部 provider 的定义收口。
///
/// 一处可查，避免满仓库找 provider（蓝图 §7.2）。片 5 只接**网络与控制面**这一层；
/// 页面级 provider（登录表单、配对倒计时…）随片 6 的页面一起加。
///
/// ## 为什么这一层的 provider 大多是可空的
///
/// 没选服务器（没登录）时，`RestClient` / `WsClient` **不存在**，而不是「存在但指向空地址」。
/// 让类型说出这件事，UI 就必须显式处理「还没登录」这条分支；给一个指向空地址的假客户端，
/// 换来的是运行期一堆连不上的请求和一句含糊的「网络错误」。
///
/// ## 依赖注入点
///
/// [`tokenStoreProvider`] 与 [`rawMachineIdProvider`] 的默认实现都是**能在测试里跑的**
/// （内存 / 固定值）。真机实现（Keychain / Keystore、`device_info_plus`）在片 12 由
/// `main.dart` 用 `ProviderScope(overrides: …)` 覆盖——这样 `flutter test` 不需要
/// platform channel，也就不会出现「本地测试全绿、真机崩」。
library;

import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/core/env.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/data/device_identity.dart';
import 'package:lumen_mobile/data/memory_chat_store.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/net/io_ws_socket.dart';
import 'package:lumen_mobile/net/rest_client.dart';
import 'package:lumen_mobile/net/ws_client.dart';
import 'package:lumen_mobile/state/auth_controller.dart';
import 'package:lumen_mobile/state/conversation_session.dart';
import 'package:lumen_mobile/state/device_list_controller.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/outbox.dart';
import 'package:lumen_mobile/ui/pair/qr_scanner.dart';
import 'package:uuid/uuid.dart';

/// 当前服务器（规范化后的 origin）。未选定为 null。
final NotifierProvider<ServerEndpointNotifier, ServerEndpoint?>
    serverEndpointProvider =
    NotifierProvider<ServerEndpointNotifier, ServerEndpoint?>(
  ServerEndpointNotifier.new,
);

/// 持有当前服务器。
final class ServerEndpointNotifier extends Notifier<ServerEndpoint?> {
  @override
  ServerEndpoint? build() => null;

  /// 选定一台服务器。
  void select(ServerEndpoint endpoint) => state = endpoint;

  /// 退出登录 / 换服务器。
  void clear() => state = null;
}

/// 凭据存放。默认内存实现，真机实现（`SecureTokenStore`）在片 12 覆盖。
final Provider<TokenStore> tokenStoreProvider =
    Provider<TokenStore>((Ref ref) => InMemoryTokenStore());

/// 原始机器标识来源。默认「取不到」，真机实现（`PlatformMachineId`）在片 12 覆盖。
///
/// 默认值刻意是 `null` 而不是一个假 id：假 id 会让 `hw_id` 看起来在工作，
/// 而它算出来的是全体测试机共用的同一个值——那比没有更糟。
final Provider<RawMachineIdSource> rawMachineIdProvider =
    Provider<RawMachineIdSource>((Ref ref) => const StaticMachineId(null));

/// 设备身份（`hw_id` 计算）。
final Provider<DeviceIdentity> deviceIdentityProvider =
    Provider<DeviceIdentity>(
  (Ref ref) => DeviceIdentity(ref.watch(rawMachineIdProvider)),
);

/// 设备显示名。默认值是个通用名字，真机实现（`device_info_plus`）在 `main.dart` 覆盖。
final Provider<DeviceNameSource> deviceNameProvider =
    Provider<DeviceNameSource>((Ref ref) => const StaticDeviceName('我的手机'));

/// 扫码取景。默认「没有相机」，真机实现（`mobile_scanner`）在 `main.dart` 覆盖。
///
/// 默认值报 `available = false`，于是配对页**根本不画**扫码入口——手输 9 位码
/// 这条路本来就完整可用，画一个点了没反应的按钮才是坏掉的功能（铁律 6）。
final Provider<QrScanner> qrScannerProvider =
    Provider<QrScanner>((Ref ref) => const UnavailableQrScanner());

/// 登录 / 注册。未选服务器时为 null。
final Provider<AuthController?> authControllerProvider =
    Provider<AuthController?>((Ref ref) {
  final ServerEndpoint? endpoint = ref.watch(serverEndpointProvider);
  final RestClient? rest = ref.watch(restClientProvider);
  if (endpoint == null || rest == null) return null;
  final AuthController controller = AuthController(
    rest: rest,
    tokens: ref.watch(tokenStoreProvider),
    identity: ref.watch(deviceIdentityProvider),
    deviceName: ref.watch(deviceNameProvider),
    endpoint: endpoint,
  );
  ref.onDispose(() => controller.dispose());
  return controller;
});

/// REST 客户端。未选服务器时为 null。
final Provider<RestClient?> restClientProvider = Provider<RestClient?>((Ref ref) {
  final ServerEndpoint? endpoint = ref.watch(serverEndpointProvider);
  if (endpoint == null) return null;
  return RestClient(endpoint: endpoint, tokens: ref.watch(tokenStoreProvider));
});

/// 控制面长连接。未选服务器时为 null。
///
/// **不在这里 `start()`**：什么时候开始连是登录流程的决定（要先有 token），
/// 在 provider 构造里连会让「打开 App 就疯狂重试」成为默认行为。
final Provider<WsClient?> wsClientProvider = Provider<WsClient?>((Ref ref) {
  final ServerEndpoint? endpoint = ref.watch(serverEndpointProvider);
  if (endpoint == null) return null;
  final TokenStore tokens = ref.watch(tokenStoreProvider);
  final WsClient client = WsClient(
    wsUrl: endpoint.wsUrl,
    tokenProvider: () async => (await tokens.read())?.token,
    opener: IoWsSocket.open,
  );
  ref.onDispose(() => client.dispose());
  return client;
});

/// 控制面状态机。未选服务器时为 null。
final Provider<LinkController?> linkControllerProvider =
    Provider<LinkController?>((Ref ref) {
  final WsClient? ws = ref.watch(wsClientProvider);
  if (ws == null) return null;
  final ServerEndpoint? endpoint = ref.watch(serverEndpointProvider);
  if (endpoint == null) return null;
  final LinkController controller =
      LinkController(ws: ws, origin: endpoint.origin);
  ref.onDispose(() => controller.dispose());
  return controller;
});

/// 本地存档（片 10）。**默认是内存实现**，真机在 `main.dart` 覆盖成 sqflite。
///
/// 与 `tokenStoreProvider` / `rawMachineIdProvider` 同款注入点，理由也一样：
/// `sqflite` 走 platform channel，把它设成默认值就等于 `flutter test` 全得起
/// platform channel。
///
/// ⚠ **它的默认实现是「能跑但不持久」**：内存版一关 App 就没了。真机忘了覆盖的症状
/// 不是崩溃、而是「杀进程重开历史空了」——所以 `main.dart` 那处覆盖有一条测试钉着
/// （`test/main_overrides_test.dart`；片 12 之前这条注释指的是 `app_boot_test.dart`，
/// 但那个文件里从来没有这条断言，覆盖清单当时其实是没人守的）。
final Provider<ChatStore> chatStoreProvider =
    Provider<ChatStore>((Ref ref) => MemoryChatStore());

/// 幂等键生成器。**必须全局唯一**——它是被控端去重表的键。
///
/// 做成 provider 是为了测试能换成可预测的序列（`msg-1` / `msg-2`…）；
/// 断言里出现一个真 UUID 就只能写成正则，一眼看不出哪条是哪条。
final Provider<ClientMsgIdGen> clientMsgIdProvider =
    Provider<ClientMsgIdGen>((Ref ref) {
  const Uuid uuid = Uuid();
  return uuid.v4;
});

/// 当前隐藏会话上的对话。没有活跃会话时为 null。
///
/// **会话建立即创建、断开即销毁**：LinkActive 里的 sessionId 是 RelayTo 的路由键，
/// 而断线重连后它会变——把 session 挂在 LinkState 上，代次一换旧的自动被 dispose，
/// 不需要额外的失效逻辑。
///
/// convId 暂时写死 1：片 7 只做单对话，多对话列表与切换是 P1（蓝图 §10.2）。
/// 真正的 convId 由被控端在 ConvStarted 里给出——接多对话时改这里为「按 convId 建一组」。
///
/// ★ **`start()` 在这里 fire-and-forget**（片 10）：它要 await 落盘，而 provider 的
/// 构造必须是同步的。中途失败只记日志——恢复不了历史时正确的行为是「按空历史继续」，
/// 而不是让对话页整个打不开。
final Provider<ConversationSession?> conversationSessionProvider =
    Provider<ConversationSession?>((Ref ref) {
  final LinkController? link = ref.watch(linkControllerProvider);
  final WsClient? ws = ref.watch(wsClientProvider);
  if (link == null || ws == null) return null;
  final LinkState state = ref.watch(linkStateProvider).value ?? const LinkIdle();
  if (state is! LinkActive) return null;
  final ConversationSession session = ConversationSession(
    link: link,
    ws: ws,
    convId: 1,
    sessionId: state.sessionId,
    store: ref.watch(chatStoreProvider),
    newClientMsgId: ref.watch(clientMsgIdProvider),
  );
  unawaited(session.start().catchError((Object e) {
    logWarn('providers', '对话启动失败（历史与补齐可能不完整）', error: e);
  }));
  ref.onDispose(() => session.dispose());
  return session;
});

/// 控制面状态流。让依赖它的 provider 能随状态变化重建。
final StreamProvider<LinkState> linkStateProvider =
    StreamProvider<LinkState>((Ref ref) {
  final LinkController? link = ref.watch(linkControllerProvider);
  if (link == null) return const Stream<LinkState>.empty();
  // 先补一个当前值：Stream 本身不带初值，不补的话页面要等下一次状态变化才画得出来。
  return Stream<LinkState>.multi((StreamController<LinkState> out) {
    out.add(link.state);
    final StreamSubscription<LinkState> sub = link.states.listen(out.add);
    out.onCancel = sub.cancel;
  });
});

/// 设备列表。未选服务器时为 null。
final Provider<DeviceListController?> deviceListControllerProvider =
    Provider<DeviceListController?>((Ref ref) {
  final RestClient? rest = ref.watch(restClientProvider);
  if (rest == null) return null;
  final DeviceListController controller = DeviceListController(rest);
  ref.onDispose(() => controller.dispose());
  return controller;
});
