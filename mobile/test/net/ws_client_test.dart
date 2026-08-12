/// WS 长连接的验收：**与桌面端刻意不同的那三处**逐条测到。
///
/// 心跳 25s / Pong 看门狗 15s / 后台宽限 25s / 退避——四条时间线全部靠 [`FakeScheduler`]
/// 驱动，测试里推进虚拟时钟即可，不需要真的等，也测得了「刚好没到」与「刚好到了」的边界。
library;

import 'dart:async';
import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/net/backoff.dart';
import 'package:lumen_mobile/net/ws_client.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';

import '../support/fake_scheduler.dart';
import '../support/fake_ws_socket.dart';

/// 取一条已发消息的变体名。
String _variantOf(String wire) {
  final Object? json = jsonDecode(wire);
  return RemoteC2S.fromJson(json).variantName;
}

List<String> _variantsOf(FakeWsSocket socket) =>
    socket.sent.map(_variantOf).toList();

void main() {
  late FakeScheduler clock;
  late FakeWsOpener opener;
  late WsClient client;
  late String token;

  /// 建一个客户端。退避用固定 1 秒基准，测试里一律 advance 2 秒以覆盖抖动区间。
  WsClient build() => WsClient(
        wsUrl: 'wss://lumen.example.com/api/v1/ws',
        tokenProvider: () async => token,
        opener: opener.open,
        scheduler: clock,
        backoff: Backoff(baseMs: 1000, maxMs: 1000),
      );

  setUp(() {
    clock = FakeScheduler();
    opener = FakeWsOpener();
    token = 'tok-1';
    client = build();
  });

  tearDown(() async => client.dispose());

  /// 起连接并等异步握手跑完。
  Future<void> connect() async {
    client.start();
    await pumpEventQueue();
  }

  /// 推进虚拟时钟并等其中的异步部分跑完。
  Future<void> tick(Duration by) async {
    clock.advance(by);
    await pumpEventQueue();
  }

  group('建立连接', () {
    test('连上后立刻发 ClientHello，caps 为空', () async {
      await connect();
      expect(client.status, WsStatus.connected);
      expect(_variantsOf(opener.last), <String>['ClientHello']);

      final C2SClientHello hello =
          RemoteC2S.fromJson(jsonDecode(opener.last.sent.first))
              as C2SClientHello;
      expect(hello.protocolVersion, kLumenProtocolVersion);
      expect(hello.caps, isEmpty, reason: '"hidden" 是 PC 的能力位，手机报了就是谎报');
    });

    test('token 在**每次**连接前现取，不是构造时取一次', () async {
      await connect();
      expect(opener.tokens, <String>['tok-1']);

      token = 'tok-2'; // 模拟期间续了期
      await opener.last.serverClose();
      await pumpEventQueue();
      await tick(const Duration(seconds: 2));

      expect(opener.tokens, <String>['tok-1', 'tok-2'],
          reason: '重连必须用新 token，否则续期后第一次重连必然 401 掉线');
    });

    test('start 幂等，重复调用不会开第二条连接', () async {
      await connect();
      client.start();
      await pumpEventQueue();
      expect(opener.connectCount, 1);
    });
  });

  group('心跳与 Pong 看门狗', () {
    test('25 秒发一次 Ping', () async {
      await connect();
      await tick(const Duration(seconds: 24));
      expect(_variantsOf(opener.last), <String>['ClientHello']);

      await tick(const Duration(seconds: 1));
      expect(_variantsOf(opener.last), <String>['ClientHello', 'Ping']);
    });

    test('收到 Pong 后连接不受影响，且 Pong 不外抛', () async {
      final List<RemoteS2C> seen = <RemoteS2C>[];
      await connect();
      client.inbound.listen(seen.add);

      await tick(const Duration(seconds: 25));
      opener.last.receive(jsonEncode(const S2CPong().toJson()));
      await pumpEventQueue();
      await tick(const Duration(seconds: 20)); // 远超 15 秒看门狗

      expect(client.status, WsStatus.connected);
      expect(opener.connectCount, 1);
      expect(seen, isEmpty, reason: 'Pong 由看门狗就地消费，外抛只会变成噪声');
    });

    test('15 秒收不到 Pong 就判定半开连接并重连', () async {
      await connect();
      final FakeWsSocket first = opener.last;

      await tick(const Duration(seconds: 25)); // 发 Ping
      await tick(const Duration(seconds: 14));
      expect(client.status, WsStatus.connected, reason: '还没到 15 秒');

      await tick(const Duration(seconds: 1));
      expect(first.closed, isTrue, reason: '半开连接必须主动关掉');
      expect(client.status, WsStatus.disconnected);

      await tick(const Duration(seconds: 2)); // 退避到期
      expect(opener.connectCount, 2);
    });

    test('心跳频率不会因重连而翻倍', () async {
      // 忘了取消旧计时器的症状是 Ping 变成 12.5 秒一次——功能全对、只有流量变大，
      // 几乎不可能在开发期发现。这条守的就是它。
      await connect();
      await opener.last.serverClose();
      await pumpEventQueue();
      await tick(const Duration(seconds: 2)); // 重连

      final FakeWsSocket second = opener.last;
      for (int i = 0; i < 3; i++) {
        await tick(const Duration(seconds: 25));
        second.receive(jsonEncode(const S2CPong().toJson()));
        await pumpEventQueue();
      }
      expect(
        _variantsOf(second).where((String v) => v == 'Ping').length,
        3,
        reason: '三个心跳周期就该只有三条 Ping',
      );
    });
  });

  group('入站消息', () {
    test('认识的消息外抛', () async {
      final List<RemoteS2C> seen = <RemoteS2C>[];
      await connect();
      client.inbound.listen(seen.add);

      opener.last.receive(jsonEncode(const S2CWelcome(
        protocolVersion: 4,
        minSupportedVersion: 3,
        deviceId: 'phone-1',
      ).toJson()));
      await pumpEventQueue();

      expect(seen.single, isA<S2CWelcome>());
    });

    test('不认识的变体只丢这一条，**不断连**', () async {
      // 为一条没实现的消息断掉连接，是把显示问题升级成可用性问题。
      final List<RemoteS2C> seen = <RemoteS2C>[];
      await connect();
      client.inbound.listen(seen.add);

      opener.last.receive('{"FutureVariant":{"x":1}}');
      opener.last.receive(jsonEncode(const S2CPong().toJson()));
      opener.last.receive(jsonEncode(const S2CSessionEnded(EndReason.peerLeft).toJson()));
      await pumpEventQueue();

      expect(client.status, WsStatus.connected);
      expect(opener.last.closed, isFalse);
      expect(seen.single, isA<S2CSessionEnded>(), reason: '后续消息照常处理');
    });

    test('非 JSON 文本同样只丢这一条', () async {
      await connect();
      opener.last.receive('这不是 JSON');
      await pumpEventQueue();
      expect(client.status, WsStatus.connected);
    });

    test('链路出错走重连', () async {
      await connect();
      opener.last.serverError(StateError('链路炸了'));
      await pumpEventQueue();
      expect(client.status, WsStatus.disconnected);

      await tick(const Duration(seconds: 2));
      expect(opener.connectCount, 2);
    });
  });

  group('发送', () {
    test('无连接时返回 false（静默丢弃语义）', () {
      // 与桌面端同语义 ⇒ 需要送达保证的请求必须走 outbox + req_id 超时。
      expect(client.send(const C2SOpenHidden('pc-1')), isFalse);
    });

    test('有连接时写出去并返回 true', () async {
      await connect();
      expect(client.send(const C2SOpenHidden('pc-1')), isTrue);
      expect(_variantsOf(opener.last), <String>['ClientHello', 'OpenHidden']);
    });
  });

  group('退避重连', () {
    test('连接失败后按退避重试，成功即重置', () async {
      opener.failTimes = 2;
      await connect();
      expect(opener.connectCount, 1);
      expect(client.retryAttempts, 1);

      await tick(const Duration(seconds: 2));
      expect(opener.connectCount, 2);
      expect(client.retryAttempts, 2);

      await tick(const Duration(seconds: 2));
      expect(opener.connectCount, 3);
      expect(client.status, WsStatus.connected);
      expect(client.retryAttempts, 0, reason: '连上就该回到第一格');
    });

    test('stop 之后不再重连', () async {
      await connect();
      client.stop();
      await pumpEventQueue();
      expect(opener.last.closed, isTrue);

      await tick(const Duration(seconds: 60));
      expect(opener.connectCount, 1);
      expect(clock.pendingCount, 0, reason: '停掉之后不该留任何计时器');
    });

    test('stop 早于取 token：连都不会去连', () async {
      client.start();
      client.stop();
      await pumpEventQueue();

      expect(opener.connectCount, 0);
      expect(client.status, WsStatus.disconnected);
    });

    test('stop 晚于取 token：迟到的那条连接会被就地关掉', () async {
      // 代次守卫。没有它，一条谁也不想要的 socket 会挂上去且不受任何计时器管辖——
      // 表现是「明明退出登录了还在收消息」。
      opener.gate = Completer<void>();
      client.start();
      await pumpEventQueue(); // 卡在 opener 里

      client.stop();
      opener.gate!.complete(); // 连接此刻才建立成功
      await pumpEventQueue();

      expect(opener.sockets.single.closed, isTrue);
      expect(client.status, WsStatus.disconnected);
    });
  });

  group('前后台生命周期', () {
    test('进后台先宽限 25 秒，到点才断', () async {
      await connect();
      client.onBackground();

      await tick(const Duration(seconds: 24));
      expect(client.status, WsStatus.connected, reason: '切出去看一眼验证码不该断');

      await tick(const Duration(seconds: 1));
      expect(client.status, WsStatus.disconnected);
      expect(opener.sockets.single.closed, isTrue);
    });

    test('后台期间不重连', () async {
      await connect();
      client.onBackground();
      await tick(const Duration(seconds: 25));
      await tick(const Duration(minutes: 5));
      expect(opener.connectCount, 1, reason: '后台反复重连只会耗电');
    });

    test('反复触发的 onBackground 不会把宽限期一次次续上', () async {
      await connect();
      client.onBackground();
      await tick(const Duration(seconds: 10));
      client.onBackground(); // 某些机型会连发两次
      await tick(const Duration(seconds: 15));
      expect(client.status, WsStatus.disconnected,
          reason: '25 秒是从第一次进后台算起的');
    });

    test('回前台立刻重连，不等退避', () async {
      await connect();
      client.onBackground();
      await tick(const Duration(seconds: 25));
      expect(client.status, WsStatus.disconnected);

      client.onForeground();
      await pumpEventQueue();
      expect(opener.connectCount, 2, reason: 'kick 的语义就是「别再等了」');
      expect(client.status, WsStatus.connected);
    });

    test('已经连着时 kick 不重开连接', () async {
      await connect();
      client.kick();
      await pumpEventQueue();
      expect(opener.connectCount, 1);
    });
  });
}
