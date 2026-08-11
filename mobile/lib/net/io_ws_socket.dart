/// [`WsSocket`] 的真实实现（`dart:io` + `IOWebSocketChannel`）。
///
/// **单独一个文件**，为的是让 `ws_client.dart` 保持零 `dart:io` 依赖——退避、心跳、
/// 看门狗、生命周期那四条时间线才能在纯 Dart 测试里被驱动。这里只剩「怎么把一条真 socket
/// 接上去」，逻辑薄到不需要测试。
///
/// ⚠ `IOWebSocketChannel` 是「将来出 Web 版」的硬阻塞点：服务端 `ws.rs` 只认
/// `Authorization` 头、不接受 query token（避免反代日志泄漏），而浏览器的 `WebSocket()`
/// 设不了请求头。这条在 K1 已经拍板（原生 App，不做 Web/PWA），写在这里是给后人看的。
library;

import 'dart:async';

import 'package:lumen_mobile/net/ws_client.dart';
import 'package:web_socket_channel/io.dart';
import 'package:web_socket_channel/status.dart' as ws_status;

/// 真实的 WebSocket 连接。
final class IoWsSocket implements WsSocket {
  IoWsSocket._(this._channel);

  final IOWebSocketChannel _channel;

  /// [`WsSocketOpener`]：连上并等握手完成才返回。
  ///
  /// `pingInterval: null` 是刻意的——**用应用层 `Ping`，不用 WebSocket 协议层 ping**：
  /// 服务端把应用层 `Ping` 同时当作「刷新 last_seen」的信号（`ws.rs` 的节流就挂在那条臂上），
  /// 协议层 ping 走的是另一条路、服务端的 `Message::Ping` 分支直接忽略，
  /// 于是设备会在 45 秒后从别人的设备列表里消失，而连接本身好好的。
  static Future<WsSocket> open(String url, String token) async {
    final IOWebSocketChannel channel = IOWebSocketChannel.connect(
      Uri.parse(url),
      headers: <String, Object?>{'Authorization': 'Bearer $token'},
      pingInterval: null,
      connectTimeout: const Duration(seconds: 10),
    );
    // ready 抛出时连接没建立成功，交给 WsClient 的退避重连。
    await channel.ready;
    return IoWsSocket._(channel);
  }

  /// 只取文本帧：服务端只发文本（`ws.rs` 的出站一律 `Message::Text`），
  /// 二进制与控制帧在那边就被忽略了，这边同样丢弃。
  @override
  Stream<String> get messages =>
      _channel.stream.where((Object? e) => e is String).cast<String>();

  @override
  void send(String text) => _channel.sink.add(text);

  @override
  Future<void> close() => _channel.sink.close(ws_status.normalClosure);
}
