/// 控制面状态机的验收。
///
/// 用**真的** [`WsClient`] 配假 socket，而不是给 `LinkController` 再注入一层接口：
/// 这条链路上最容易错的正是「消息序列化成什么、按什么顺序发出去」，隔一层 mock 就把它测没了。
///
/// 重点是那条**硬验收项**：`OpenHidden` 12 秒无回音 ⇒ 判定「对端版本过低」。
/// 老服务端不认识这个变体，整条解析失败后只记 debug 就继续读——完全无回音，
/// 没有这个超时就是无限转圈。
library;

import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/net/backoff.dart';
import 'package:lumen_mobile/net/ws_client.dart';
import 'package:lumen_mobile/protocol/pairing_qr.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';
import 'package:lumen_mobile/state/link_controller.dart';

import '../support/fake_scheduler.dart';
import '../support/fake_ws_socket.dart';

void main() {
  late FakeScheduler clock;
  late FakeWsOpener opener;
  late WsClient ws;
  late LinkController link;

  setUp(() async {
    clock = FakeScheduler();
    opener = FakeWsOpener();
    ws = WsClient(
      wsUrl: 'wss://lumen.example.com/api/v1/ws',
      tokenProvider: () async => 'tok-1',
      opener: opener.open,
      scheduler: clock,
      backoff: Backoff(baseMs: 1000, maxMs: 1000),
    );
    link = LinkController(
      ws: ws,
      origin: 'https://lumen.example.com',
      scheduler: clock,
    );
    ws.start();
    await pumpEventQueue();
  });

  tearDown(() async {
    await link.dispose();
    await ws.dispose();
  });

  /// 服务端下发一条消息。
  Future<void> serverSend(RemoteS2C msg) async {
    opener.last.receive(jsonEncode(msg.toJson()));
    await pumpEventQueue();
  }

  Future<void> tick(Duration by) async {
    clock.advance(by);
    await pumpEventQueue();
  }

  /// 本端发出去的变体序列（跳过握手那条 ClientHello）。
  List<String> sentVariants() => opener.last.sent
      .map((String w) => RemoteC2S.fromJson(jsonDecode(w)).variantName)
      .where((String v) => v != 'ClientHello')
      .toList();

  /// 走到「已配对建立会话」为止。
  Future<void> reachActive({int sessionId = 7}) async {
    link.open('pc-1');
    await serverSend(const S2CPairingNeeded(
      targetDeviceId: 'pc-1',
      targetName: '海风的工作站',
      expiresInSecs: 120,
    ));
    link.submitCode('012345678');
    await serverSend(S2CHiddenSessionStarted(
      sessionId: sessionId,
      peerDeviceId: 'pc-1',
      peerName: '海风的工作站',
      role: RemoteRole.controller,
    ));
  }

  group('发起', () {
    test('open 发出 OpenHidden 并进入 Requesting', () {
      expect(link.open('pc-1'), isTrue);
      expect(link.state, isA<LinkRequesting>());
      expect(sentVariants(), <String>['OpenHidden']);
    });

    test('★ 12 秒无回音 ⇒ 判定对端版本过低（硬验收项）', () async {
      link.open('pc-1');
      await tick(const Duration(seconds: 11));
      expect(link.state, isA<LinkRequesting>(), reason: '还没到 12 秒');

      await tick(const Duration(seconds: 1));
      final LinkState s = link.state;
      expect(s, isA<LinkFailed>());
      expect((s as LinkFailed).reason, LinkFailure.serverTooOld);
      expect(s.target, 'pc-1');
    });

    test('收到回音后超时被取消，不会误判', () async {
      link.open('pc-1');
      await serverSend(const S2CPairingNeeded(
        targetDeviceId: 'pc-1',
        targetName: '海风的工作站',
        expiresInSecs: 120,
      ));
      // 20 秒：远超 12 秒发起超时，又不到「25 秒发 Ping + 15 秒看门狗」那条线
      // ——这里要证的是发起超时被取消了，不是把连接熬死。
      await tick(const Duration(seconds: 20));
      expect(link.state, isA<LinkPairing>(),
          reason: '之后的时限由配对码 TTL 接管，不该被发起超时打断');
    });

    test('没连上时不进 Requesting 干等 12 秒', () async {
      ws.stop();
      await pumpEventQueue();
      expect(link.open('pc-1'), isFalse);
      expect((link.state as LinkFailed).reason, LinkFailure.linkLost);
    });

    test('已有在途请求时拒绝重入', () {
      link.open('pc-1');
      expect(link.open('pc-2'), isFalse);
      expect((link.state as LinkRequesting).target, 'pc-1');
      expect(sentVariants(), <String>['OpenHidden'], reason: '第二次不该发出去');
    });
  });

  group('配对', () {
    test('PairingNeeded 带出目标名与 TTL', () async {
      link.open('pc-1');
      await serverSend(const S2CPairingNeeded(
        targetDeviceId: 'pc-1',
        targetName: '海风的工作站',
        expiresInSecs: 120,
      ));
      final LinkPairing s = link.state as LinkPairing;
      expect(s.targetName, '海风的工作站');
      expect(s.expiresInSecs, 120, reason: '倒计时用服务端给的数，别写死');
      expect(s.attemptsLeft, 5);
    });

    test('submitCode 发出 SubmitPairing', () async {
      link.open('pc-1');
      await serverSend(const S2CPairingNeeded(
        targetDeviceId: 'pc-1',
        targetName: 'PC',
        expiresInSecs: 120,
      ));
      expect(link.submitCode('012345678'), isTrue);
      expect(sentVariants(), <String>['OpenHidden', 'SubmitPairing']);
    });

    test('码错：剩余次数与错误原因回填，仍留在配对页', () async {
      link.open('pc-1');
      await serverSend(const S2CPairingNeeded(
        targetDeviceId: 'pc-1',
        targetName: 'PC',
        expiresInSecs: 120,
      ));
      await serverSend(const S2CPairingResult(
        reason: PairingFailReason.invalidCode,
        attemptsLeft: 4,
      ));
      final LinkPairing s = link.state as LinkPairing;
      expect(s.attemptsLeft, 4);
      expect(s.lastError, PairingFailReason.invalidCode);
    });

    test('attempts_left == 0 是「配对已作废」，必须离开配对页', () async {
      // 留在输码页会让用户对着一个已经不存在的配对继续输。
      link.open('pc-1');
      await serverSend(const S2CPairingNeeded(
        targetDeviceId: 'pc-1',
        targetName: 'PC',
        expiresInSecs: 120,
      ));
      await serverSend(const S2CPairingResult(
        reason: PairingFailReason.tooManyAttempts,
        attemptsLeft: 0,
      ));
      expect((link.state as LinkFailed).reason,
          LinkFailure.pairingTooManyAttempts);
    });

    test('不在配对态时 submitCode 不发消息', () {
      expect(link.submitCode('012345678'), isFalse);
      expect(sentVariants(), isEmpty);
    });
  });

  group('会话建立与结束', () {
    test('HiddenSessionStarted ⇒ Active，代次自增', () async {
      await reachActive();
      final LinkActive s = link.state as LinkActive;
      expect(s.sessionId, 7);
      expect(s.peerDeviceId, 'pc-1');
      expect(s.generation, 1);
      expect(link.generation, 1);
    });

    test('允许从 Requesting 直接跃迁到 Active（服务端那条 if paired 分支还在）', () async {
      link.open('pc-1');
      await serverSend(const S2CHiddenSessionStarted(
        sessionId: 3,
        peerDeviceId: 'pc-1',
        peerName: 'PC',
        role: RemoteRole.controller,
      ));
      expect((link.state as LinkActive).sessionId, 3);
    });

    test('非等待态收到 HiddenSessionStarted 仍接受，避免会话泄漏', () async {
      // 拒绝的话，那条会话会在服务端一直挂着、占着目标 PC 的隐藏名额（上限 2），
      // 而没有任何一端会去拆它。
      await serverSend(const S2CHiddenSessionStarted(
        sessionId: 11,
        peerDeviceId: 'pc-9',
        peerName: '别的 PC',
        role: RemoteRole.controller,
      ));
      expect((link.state as LinkActive).sessionId, 11);
    });

    test('close 就地清状态（服务端对主动方不回执）', () async {
      await reachActive();
      link.close();
      expect(link.state, isA<LinkIdle>());
      expect(sentVariants().last, 'EndHidden');
    });

    test('HiddenSessionEnded ⇒ Idle', () async {
      await reachActive();
      await serverSend(const S2CHiddenSessionEnded(
        sessionId: 7,
        reason: EndReason.peerDisconnected,
      ));
      expect(link.state, isA<LinkIdle>());
    });

    test('非当前会话的 HiddenSessionEnded 被丢弃', () async {
      await reachActive();
      await serverSend(const S2CHiddenSessionEnded(
        sessionId: 999,
        reason: EndReason.peerLeft,
      ));
      expect(link.state, isA<LinkActive>());
    });
  });

  group('拒绝与断线', () {
    test('ControlDenied ⇒ LinkDenied，带原始拒因', () async {
      link.open('pc-1');
      await serverSend(const S2CControlDenied(
        targetDeviceId: 'pc-1',
        reason: DenyReason.alreadyControlled,
      ));
      final LinkDenied s = link.state as LinkDenied;
      expect(s.reason, DenyReason.alreadyControlled);
      expect(s.target, 'pc-1');
    });

    test('与当前目标无关的 ControlDenied 被丢弃', () async {
      link.open('pc-1');
      await serverSend(const S2CControlDenied(
        targetDeviceId: 'pc-OTHER',
        reason: DenyReason.offline,
      ));
      expect(link.state, isA<LinkRequesting>());
    });

    test('会话期间断线 ⇒ LinkFailed(linkLost)', () async {
      // 服务端 disconnect 会立刻 teardown_hidden_for_device，没有重连续挂。
      await reachActive();
      await opener.last.serverClose();
      await pumpEventQueue();
      expect((link.state as LinkFailed).reason, LinkFailure.linkLost);
    });

    test('空闲时断线不产生错误态', () async {
      await opener.last.serverClose();
      await pumpEventQueue();
      expect(link.state, isA<LinkIdle>());
    });

    test('dismissError 回到空闲', () async {
      link.open('pc-1');
      await serverSend(const S2CControlDenied(
        targetDeviceId: 'pc-1',
        reason: DenyReason.offline,
      ));
      link.dismissError();
      expect(link.state, isA<LinkIdle>());
    });
  });

  group('数据面转发', () {
    test('当前会话的帧转出去', () async {
      final List<S2CRelayTo> frames = <S2CRelayTo>[];
      await reachActive();
      link.sessionFrames.listen(frames.add);

      await serverSend(const S2CRelayTo(
        sessionId: 7,
        payload: <String, Object?>{'Llm': <String, Object?>{'op': 'Detach'}},
      ));
      expect(frames.single.sessionId, 7);
    });

    test('陈旧会话的帧被丢弃（上一条会话的尾巴）', () async {
      final List<S2CRelayTo> frames = <S2CRelayTo>[];
      await reachActive();
      link.sessionFrames.listen(frames.add);

      await serverSend(const S2CRelayTo(sessionId: 6, payload: <String, Object?>{}));
      expect(frames, isEmpty, reason: '喂给数据面会污染当前对话');
    });

    test('没有会话时的帧被丢弃', () async {
      final List<S2CRelayTo> frames = <S2CRelayTo>[];
      link.sessionFrames.listen(frames.add);
      await serverSend(const S2CRelayTo(sessionId: 1, payload: <String, Object?>{}));
      expect(frames, isEmpty);
    });
  });

  group('扫码（五重闸门里的后四重）', () {
    const String myOrigin = 'https://lumen.example.com';
    const String myFingerprint = 'a3a9e1ed9732cab2';

    /// 造一张二维码的文本。默认全部匹配。
    String qrText({
      String magic = kPairingQrMagic,
      String origin = myOrigin,
      String fingerprint = myFingerprint,
      String target = 'pc-1',
      String code = '012345678',
    }) =>
        jsonEncode(PairingQrPayload(
          magic: magic,
          origin: origin,
          userFingerprint: fingerprint,
          target: target,
          code: code,
          expiresAt: 1786342908,
        ).toJson());

    /// 走到「等输码」为止。
    Future<void> reachPairing() async {
      link.open('pc-1');
      await serverSend(const S2CPairingNeeded(
        targetDeviceId: 'pc-1',
        targetName: 'PC',
        expiresInSecs: 120,
      ));
    }

    test('全部匹配 ⇒ 通过并把码提交上去', () async {
      await reachPairing();
      expect(
        link.submitScannedQr(qrText(), userFingerprint: myFingerprint),
        isNull,
      );
      expect(sentVariants(), <String>['OpenHidden', 'SubmitPairing']);
    });

    test('★ 第一重是状态闸：不在配对态时扫码无处可用', () async {
      // 相机页只在配对态存在，所以一张钓鱼码在别的时刻根本没有入口。
      expect(
        link.submitScannedQr(qrText(), userFingerprint: myFingerprint),
        PairingQrError.malformed,
      );
      expect(sentVariants(), isEmpty);
    });

    test('★ 别的服务器 ⇒ ForeignServer，且**不提交**', () async {
      await reachPairing();
      expect(
        link.submitScannedQr(
          qrText(origin: 'https://evil.example.com'),
          userFingerprint: myFingerprint,
        ),
        PairingQrError.foreignServer,
      );
      expect(sentVariants(), <String>['OpenHidden'], reason: '拒绝就是不发出去');
    });

    test('别人的账户 ⇒ ForeignAccount', () async {
      await reachPairing();
      expect(
        link.submitScannedQr(
          qrText(fingerprint: 'c91724cad3fbbf79'),
          userFingerprint: myFingerprint,
        ),
        PairingQrError.foreignAccount,
      );
    });

    test('扫到另一台电脑的码 ⇒ WrongTarget', () async {
      await reachPairing();
      expect(
        link.submitScannedQr(
          qrText(target: 'pc-OTHER'),
          userFingerprint: myFingerprint,
        ),
        PairingQrError.wrongTarget,
      );
    });

    test('魔数不对 / 随手扫到的二维码 ⇒ Malformed', () async {
      await reachPairing();
      expect(
        link.submitScannedQr(
          qrText(magic: 'something.else'),
          userFingerprint: myFingerprint,
        ),
        PairingQrError.malformed,
      );
      expect(
        link.submitScannedQr('https://www.example.com/一个普通网址',
            userFingerprint: myFingerprint),
        PairingQrError.malformed,
      );
      expect(sentVariants(), <String>['OpenHidden']);
    });

    test('码不是 9 位纯数字 ⇒ Malformed（别浪费用户的 5 次尝试）', () async {
      await reachPairing();
      expect(
        link.submitScannedQr(
          qrText(code: '01234567x'),
          userFingerprint: myFingerprint,
        ),
        PairingQrError.malformed,
      );
      expect(sentVariants(), <String>['OpenHidden']);
    });
  });

  group('面向别端的消息只丢不崩', () {
    test('镜像会话与被控端消息不会改变状态', () async {
      await reachActive();
      await serverSend(const S2CSessionStarted(
        peerDeviceId: 'pc-1',
        peerName: 'PC',
        role: RemoteRole.controller,
      ));
      await serverSend(const S2CSessionEnded(EndReason.peerLeft));
      await serverSend(const S2CPairingCancelled(DenyReason.expired));
      await serverSend(const S2CRelay(<String, Object?>{'Echo': 'hi'}));
      await serverSend(const S2CControlRequested(
        controllerDeviceId: 'desktop-9',
        controllerName: '书房台式机',
        pairingCode: '987654321',
        expiresInSecs: 120,
      ));

      expect(link.state, isA<LinkActive>(), reason: '这些都不该动隐藏会话的状态');
      expect((link.state as LinkActive).sessionId, 7);
    });
  });
}
