/// 控制面 WebSocket 长连接。
///
/// 与桌面端 `remote_ws.rs` **刻意不同的三处**（照抄桌面实现是错的）：
///
/// 1. **退避**：桌面固定 3 秒；这里指数 + 抖动（见 `backoff.dart` 的长注释）。
/// 2. **Pong 看门狗**：仍 25 秒发一次 `Ping`（对齐服务端 25 秒 `last_seen` 节流与 45 秒
///    在线窗口），但**额外**加一条——15 秒收不到 `Pong` 就主动断开重连。移动网络的半开
///    连接极常见，而服务端**不发 Ping、也没有读超时**（那是片 11 才补的既有空白）。
///    没有这条看门狗，半开连接会一直装作活着：界面一切正常、心跳照发，但什么都不动。
/// 3. **生命周期**：进后台先起 25 秒宽限计时器（应付「切出去看一眼验证码」），到点主动
///    关 socket；回前台立刻重连、不等退避。
///
/// ## 三条硬约束
///
/// - **token 走 `Authorization` 头**，服务端 `ws.rs` 明确不接受 query（避免反代日志泄漏）。
///   这条同时是「将来出 Web 版」的硬阻塞点：浏览器的 `WebSocket()` 设不了请求头。
/// - **[send] 在无连接时丢弃**（与桌面端同语义）⇒ 需要送达保证的请求必须走 outbox +
///   `req_id` 超时（片 10），不能假设发出去了。这里让它**返回 bool** 而不是纯静默——
///   调用方至少有机会知道。
/// - **收到不认识的消息不断连**：只记 `warn` 后丢弃这一条，与服务端 `ws.rs` 对畸形消息的
///   处理口径一致（记 debug 后继续读）。为一条没实现的变体断掉整条连接，是把一个显示问题
///   升级成了可用性问题。
library;

import 'dart:async';
import 'dart:convert';

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/net/backoff.dart';
import 'package:lumen_mobile/net/scheduler.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';

const String _tag = 'ws';

/// 连接状态。
enum WsStatus {
  /// 没连着（含「退避等待中」——对 UI 而言两者没区别）。
  disconnected,

  /// 正在建立连接。
  connecting,

  /// 已建立。**不代表服务端已认出你**：`Welcome` 还在路上。
  connected,
}

/// 一条已建立的 WebSocket，只暴露本层用得到的四件事。
///
/// 窄接口而不是直接用 `WebSocketChannel`：让 `ws_client.dart` 零 `dart:io` 依赖，
/// 于是退避、心跳、看门狗、生命周期全都能在纯 Dart 测试里驱动。
abstract interface class WsSocket {
  /// 文本帧。二进制帧由实现层丢弃（服务端只发文本）。
  Stream<String> get messages;

  void send(String text);

  Future<void> close();
}

/// 建立一条连接。`token` 由调用方在**每次**连接前现取（可能刚被续期过）。
typedef WsSocketOpener = Future<WsSocket> Function(String url, String token);

/// 取当前 token；没有（未登录 / 已失效）返回 null。
typedef TokenProvider = Future<String?> Function();

/// 控制面长连接客户端。
final class WsClient {
  WsClient({
    required this.wsUrl,
    required TokenProvider tokenProvider,
    required WsSocketOpener opener,
    Scheduler scheduler = const RealScheduler(),
    Backoff? backoff,
    this.pingInterval = const Duration(seconds: 25),
    this.pongTimeout = const Duration(seconds: 15),
    this.backgroundGrace = const Duration(seconds: 25),
  })  : _tokenProvider = tokenProvider,
        _opener = opener,
        _backoff = backoff ?? Backoff(),
        _pingSlot = TimerSlot(scheduler),
        _pongSlot = TimerSlot(scheduler),
        _retrySlot = TimerSlot(scheduler),
        _graceSlot = TimerSlot(scheduler);

  /// `wss://host/api/v1/ws`。
  final String wsUrl;

  /// 心跳间隔。25 秒是对齐服务端的两个数：`last_seen` 节流 25 秒、在线窗口 45 秒。
  final Duration pingInterval;

  /// Pong 看门狗。**移动端独有**，见文件头。
  final Duration pongTimeout;

  /// 进后台后的宽限期。
  final Duration backgroundGrace;

  final TokenProvider _tokenProvider;
  final WsSocketOpener _opener;
  final Backoff _backoff;
  final TimerSlot _pingSlot;
  final TimerSlot _pongSlot;
  final TimerSlot _retrySlot;
  final TimerSlot _graceSlot;

  final StreamController<RemoteS2C> _inbound =
      StreamController<RemoteS2C>.broadcast();
  final StreamController<WsStatus> _statusEvents =
      StreamController<WsStatus>.broadcast();

  WsSocket? _socket;
  StreamSubscription<String>? _sub;
  bool _wantOpen = false;
  bool _inBackground = false;
  bool _disposed = false;
  WsStatus _status = WsStatus.disconnected;

  /// 连接代次。每次拆除自增，异步的 `_connect` 用它判断「我 await 期间世界变了没有」。
  ///
  /// 没有这个守卫的话，一次「start → 立刻 stop → opener 才 returns」会把一条谁也不想要的
  /// socket 挂上去，而且它不在任何计时器的管辖内——表现是「明明退出登录了还在收消息」。
  int _generation = 0;

  /// 服务端下行消息。**不含 `Pong`**：它被看门狗就地消费，外抛只会变成噪声。
  Stream<RemoteS2C> get inbound => _inbound.stream;

  /// 连接状态变化。
  Stream<WsStatus> get statusChanges => _statusEvents.stream;

  WsStatus get status => _status;

  /// 已失败几次（UI 可据此把「重连中」升级成「连不上服务器」）。
  int get retryAttempts => _backoff.attempt;

  /// 开始维持连接。幂等。
  void start() {
    if (_disposed || _wantOpen) return;
    _wantOpen = true;
    _backoff.reset();
    unawaited(_connect());
  }

  /// 停止并断开。用于退出登录 / 切换服务器。
  void stop() {
    _wantOpen = false;
    _teardown();
    _retrySlot.cancel();
    _graceSlot.cancel();
  }

  /// 网络恢复 / 回到前台：**立刻重连，不等退避**。
  ///
  /// 已经连着时什么都不做——`kick` 的语义是「别再等了」，不是「重来一次」。
  void kick() {
    if (_disposed || !_wantOpen || _inBackground) return;
    _backoff.reset();
    _retrySlot.cancel();
    if (_socket != null || _status == WsStatus.connecting) return;
    unawaited(_connect());
  }

  /// App 进入后台：起宽限计时器，到点才真的关。
  ///
  /// 用 `setIfIdle`：反复触发的生命周期回调（某些机型会连发两次）不该把宽限期一次次续上，
  /// 否则「切出去 10 分钟」也永远不会到点。
  void onBackground() {
    if (_disposed) return;
    _inBackground = true;
    _graceSlot.setIfIdle(backgroundGrace, () {
      logInfo(_tag, '后台宽限到期，主动断开（回前台会立刻重连）');
      _teardown();
      _retrySlot.cancel();
    });
  }

  /// App 回到前台：取消宽限、立刻重连。
  void onForeground() {
    if (_disposed) return;
    _inBackground = false;
    _graceSlot.cancel();
    kick();
  }

  /// 发一条控制面消息。
  ///
  /// 返回 `false` 表示**没有连接、这条被丢弃了**。调用方要么能接受丢失（心跳、`Detach`），
  /// 要么必须自己排队重发（`Send` 走 outbox）。
  bool send(RemoteC2S msg) {
    final WsSocket? socket = _socket;
    if (socket == null) {
      logDebug(_tag, '无连接，丢弃 ${msg.variantName}');
      return false;
    }
    socket.send(jsonEncode(msg.toJson()));
    return true;
  }

  /// 释放。之后本对象不可再用。
  Future<void> dispose() async {
    _disposed = true;
    stop();
    await _inbound.close();
    await _statusEvents.close();
  }

  // ── 内部 ───────────────────────────────────────────────────────────────────

  Future<void> _connect() async {
    if (_disposed || !_wantOpen || _inBackground) return;
    final int gen = _generation;
    _setStatus(WsStatus.connecting);

    final String? token = await _tokenProvider();
    if (gen != _generation || !_wantOpen || _disposed) return;
    if (token == null) {
      // 没 token 不是网络问题，但也不能干等：上层可能正在续期，退避重试即可。
      logWarn(_tag, '没有可用 token，稍后重试');
      _scheduleRetry();
      return;
    }

    try {
      final WsSocket socket = await _opener(wsUrl, token);
      if (gen != _generation || !_wantOpen || _disposed) {
        // 世界变了（stop / 后台 / 又一次 kick）：这条连接没人要，就地关掉。
        unawaited(socket.close());
        return;
      }
      _attach(socket);
    } on Object catch (e) {
      if (gen != _generation) return;
      logWarn(_tag, '连接失败，将退避重连', error: e);
      _scheduleRetry();
    }
  }

  void _attach(WsSocket socket) {
    _socket = socket;
    _sub = socket.messages.listen(
      _onMessage,
      onError: (Object e) {
        logWarn(_tag, '连接出错', error: e);
        _scheduleRetry();
      },
      onDone: () {
        logInfo(_tag, '连接已关闭');
        _scheduleRetry();
      },
      cancelOnError: true,
    );
    _setStatus(WsStatus.connected);
    _backoff.reset();
    // 控制面握手：手机的 caps 恒为空（"hidden" 是 PC 的能力位，报了就是谎报）。
    send(const C2SClientHello(protocolVersion: kLumenProtocolVersion));
    _armPing();
  }

  /// 装下一次心跳。
  void _armPing() => _pingSlot.set(pingInterval, _onPingDue);

  void _onPingDue() {
    if (!send(const C2SPing())) return; // 已经断了，重连路径会接手
    // 看门狗只在**真的发出去**之后才起：连接不存在时起它，只会在 15 秒后再触发一次
    // 本来就已经在跑的重连。
    _pongSlot.set(pongTimeout, _onPongOverdue);
    _armPing();
  }

  void _onPongOverdue() {
    logWarn(_tag, 'Pong 超时（${pongTimeout.inSeconds}s），判定半开连接，主动重连');
    _scheduleRetry();
  }

  void _onMessage(String text) {
    final Object? json;
    try {
      json = jsonDecode(text);
    } on FormatException catch (e) {
      logWarn(_tag, '收到非 JSON 文本，已丢弃', error: e);
      return;
    }
    final RemoteS2C msg;
    try {
      msg = RemoteS2C.fromJson(json);
    } on Object catch (e) {
      // 外部标签枚举没有 other 兜底 ⇒ 本端不认识的变体就是整条丢失。
      // **只丢这一条、不断连**：为一条没实现的消息断掉连接，是把显示问题升级成可用性问题。
      logWarn(_tag, '收到本端不认识的控制面消息，已丢弃（服务端比本端新？）', error: e);
      return;
    }
    if (msg is S2CPong) {
      _pongSlot.cancel();
      return;
    }
    _inbound.add(msg);
  }

  /// 拆连接（不改 `_wantOpen`）。
  void _teardown() {
    _generation++;
    _pingSlot.cancel();
    _pongSlot.cancel();
    final StreamSubscription<String>? sub = _sub;
    final WsSocket? socket = _socket;
    _sub = null;
    _socket = null;
    unawaited(sub?.cancel());
    unawaited(socket?.close());
    _setStatus(WsStatus.disconnected);
  }

  void _scheduleRetry() {
    _teardown();
    if (_disposed || !_wantOpen || _inBackground) return;
    final Duration delay = _backoff.next();
    logDebug(_tag, '第 ${_backoff.attempt} 次重连将在 ${delay.inMilliseconds}ms 后');
    _retrySlot.set(delay, () => unawaited(_connect()));
  }

  void _setStatus(WsStatus next) {
    if (_status == next) return;
    _status = next;
    if (!_statusEvents.isClosed) _statusEvents.add(next);
  }
}
