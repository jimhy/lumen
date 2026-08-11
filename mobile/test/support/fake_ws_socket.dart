/// 内存里的假 WebSocket，替换 [`WsSocket`]。
///
/// 存在的理由与 `fake_scheduler.dart` 相同：`ws_client.dart` 刻意零 `dart:io` 依赖，
/// 于是「连上 → 心跳 → 半开 → 重连」这整条线可以在纯 Dart 里跑完，不需要起服务器。
library;

import 'dart:async';

import 'package:lumen_mobile/net/ws_client.dart';

/// 一条假连接。
final class FakeWsSocket implements WsSocket {
  final StreamController<String> _incoming =
      StreamController<String>.broadcast();

  /// 本端发出去的原始 JSON 文本，按顺序。
  final List<String> sent = <String>[];

  bool closed = false;

  @override
  Stream<String> get messages => _incoming.stream;

  @override
  void send(String text) => sent.add(text);

  @override
  Future<void> close() async {
    closed = true;
    if (!_incoming.isClosed) await _incoming.close();
  }

  /// 模拟服务端下发一条消息。
  void receive(String text) {
    if (!_incoming.isClosed) _incoming.add(text);
  }

  /// 模拟服务端关闭连接（触发 `onDone`）。
  Future<void> serverClose() async {
    if (!_incoming.isClosed) await _incoming.close();
  }

  /// 模拟链路出错（触发 `onError`）。
  void serverError(Object error) {
    if (!_incoming.isClosed) _incoming.addError(error);
  }
}

/// 记录每次连接、并把 [`FakeWsSocket`] 交出去的 opener。
final class FakeWsOpener {
  FakeWsOpener({this.failTimes = 0});

  /// 前 N 次连接抛异常（测退避）。
  int failTimes;

  /// 非 null 时 [open] 会挂在它上面，直到测试 `complete()`。
  ///
  /// 用来构造「连接建立到一半，世界变了」这个竞态——那正是 `WsClient` 里那个代次守卫
  /// 要挡的场景，没有它测不出来。
  Completer<void>? gate;

  /// 依次建立过的连接。
  final List<FakeWsSocket> sockets = <FakeWsSocket>[];

  /// 每次连接拿到的 token（验证「每次连接前现取」）。
  final List<String> tokens = <String>[];

  int get connectCount => sockets.length + _failed;
  int _failed = 0;

  /// 最后一条连接。
  FakeWsSocket get last => sockets.last;

  Future<WsSocket> open(String url, String token) async {
    tokens.add(token);
    final Completer<void>? g = gate;
    if (g != null) await g.future;
    if (_failed < failTimes) {
      _failed++;
      throw StateError('假连接失败 #$_failed');
    }
    final FakeWsSocket socket = FakeWsSocket();
    sockets.add(socket);
    return socket;
  }
}
