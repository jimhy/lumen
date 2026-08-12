/// 配对页扫码入口的验收（片 12）。
///
/// 控制器那一层（`link_controller_test.dart` 的「五重闸门里的后四重」）已经把**判定**
/// 测死了。这里测的是**页面**：入口该不该出现、判定结果怎么变成用户看得懂的一句话、
/// 以及同一张码在镜头前每帧命中时会不会刷屏。
///
/// 三条是真正的验收目标：
///
/// 1. **没有相机就不画扫码按钮**（§14-6：不给点了没反应的入口）；
/// 2. **四种拒绝各自一句不同的文案**——合并成「二维码无效」，用户就永远分不清自己是
///    扫错了设备、扫了别人的码，还是遇到了钓鱼；
/// 3. **被拒的码不提交**：页面上出现文案的同时，线上不能多出一条 `SubmitPairing`
///    （那会白白吃掉 5 次尝试里的一次）。
library;

import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/core/env.dart';
import 'package:lumen_mobile/data/device_identity.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/net/backoff.dart';
import 'package:lumen_mobile/net/rest_client.dart';
import 'package:lumen_mobile/net/ws_client.dart';
import 'package:lumen_mobile/protocol/pairing_qr.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';
import 'package:lumen_mobile/state/auth_controller.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/pair/pair_page.dart';
import 'package:lumen_mobile/ui/pair/qr_scanner.dart';

import '../support/fake_qr_scanner.dart';
import '../support/fake_rest.dart';
import '../support/fake_scheduler.dart';
import '../support/fake_ws_socket.dart';

const String _origin = 'https://lumen.example.com';

/// `FakeRestBackend` 登录回的账户 id。
const String _userId = 'u1';

void main() {
  late FakeScheduler clock;
  late FakeWsOpener opener;
  late WsClient ws;
  late LinkController link;
  late AuthController auth;
  late FakeRestBackend backend;
  late ServerEndpoint endpoint;

  setUp(() async {
    clock = FakeScheduler();
    opener = FakeWsOpener();
    endpoint = ServerEndpoint.tryParse(_origin)!;
    backend = FakeRestBackend();
    ws = WsClient(
      wsUrl: '$_origin/api/v1/ws',
      tokenProvider: () async => 'tok-1',
      opener: opener.open,
      scheduler: clock,
      backoff: Backoff(baseMs: 1000, maxMs: 1000),
    );
    link = LinkController(ws: ws, origin: _origin, scheduler: clock);
    final InMemoryTokenStore tokens = InMemoryTokenStore();
    auth = AuthController(
      rest: RestClient(endpoint: endpoint, tokens: tokens, dio: backend.dio()),
      tokens: tokens,
      identity: const DeviceIdentity(StaticMachineId('vendor-1')),
      deviceName: const StaticDeviceName('测试机'),
      endpoint: endpoint,
    );
    ws.start();
    // 页面要拿账户指纹判「这是不是别人的码」，所以必须真的登录一次。
    await auth.login(email: 'a@b.c', password: 'pw');
    await pumpEventQueue();
  });

  tearDown(() async {
    await link.dispose();
    await ws.dispose();
    await auth.dispose();
  });

  /// 造一张二维码的文本。默认全部匹配。
  String qrText({
    String magic = kPairingQrMagic,
    String origin = _origin,
    String? fingerprint,
    String target = 'pc-1',
    String code = '012345678',
  }) =>
      jsonEncode(PairingQrPayload(
        magic: magic,
        origin: origin,
        userFingerprint: fingerprint ?? accountFingerprint(_userId),
        target: target,
        code: code,
        expiresAt: 1786342908,
      ).toJson());

  /// 本端发出去的变体序列（跳过握手那条 ClientHello）。
  List<String> sentVariants() => opener.last.sent
      .map((String w) => RemoteC2S.fromJson(jsonDecode(w)).variantName)
      .where((String v) => v != 'ClientHello')
      .toList();

  /// 把配对页挂起来，并把控制器推进到「等输码」。
  ///
  /// ⚠ **两条 flutter_test 的规矩，踩了都是「测试永远不结束」**：
  ///
  /// 1. 用例体跑在 `FakeAsync` 里，`await pumpEventQueue()` 在这里等的是真实事件循环
  ///    ——**永远等不到**。要推进 stream 事件只能靠 `tester.pump()`（本文件最初就是
  ///    这么挂死的，flutter_tester 进程停在第一个用例上，CPU 几乎为零）。
  /// 2. 页面里有一条每秒一次的倒计时 `Timer.periodic`，所以也**不能** `pumpAndSettle()`
  ///    ——它会一直等到没有待处理帧为止，而倒计时永远在排下一帧。
  Future<void> mount(WidgetTester tester, QrScanner scanner) async {
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          linkControllerProvider.overrideWithValue(link),
          authControllerProvider.overrideWithValue(auth),
          qrScannerProvider.overrideWithValue(scanner),
        ],
        child: const MaterialApp(home: PairPage()),
      ),
    );
    // ★ 这一段必须在 `runAsync` 里：从假 socket 到 `LinkController` 中间隔着
    // `WsClient` 的两层 stream 转发，靠 `tester.pump()` 的 microtask flush 推不动
    // （实测状态一直停在 `LinkRequesting`，页面画的是转圈，`find.text('连接')`
    // 一个都找不到）。`runAsync` 把这段放回真实事件循环里跑。
    await tester.runAsync(() async {
      link.open('pc-1');
      opener.last.receive(jsonEncode(const S2CPairingNeeded(
        targetDeviceId: 'pc-1',
        targetName: '海风的工作站',
        expiresInSecs: 120,
      ).toJson()));
      await pumpEventQueue();
    });
    // 回到 FakeAsync，让页面的 StreamBuilder 用新状态重建。
    await tester.pump();
  }

  /// 点一下「扫到了这段文本」。
  Future<void> scan(WidgetTester tester, String text) async {
    await tester.tap(find.byKey(ValueKey<String>('scan:$text')));
    await tester.pump();
  }

  /// 进入取景模式。
  Future<void> openCamera(WidgetTester tester) async {
    await tester.tap(find.text('扫描电脑上的二维码'));
    await tester.pump();
  }

  testWidgets('★ 没有相机就不画扫码入口（手输那条路本来就完整可用）',
      (WidgetTester tester) async {
    await mount(tester, const UnavailableQrScanner());

    expect(find.text('扫描电脑上的二维码'), findsNothing);
    // 手输仍在。
    expect(find.text('连接'), findsOneWidget);
  });

  testWidgets('有相机时画入口，点进去是取景', (WidgetTester tester) async {
    await mount(tester, FakeQrScanner(codes: <String>[qrText()]));

    expect(find.text('扫描电脑上的二维码'), findsOneWidget);

    await openCamera(tester);
    expect(find.text('把镜头对准 海风的工作站 屏幕上的二维码。'), findsOneWidget);
    // 出路始终在。
    expect(find.text('改用手输 9 位码'), findsOneWidget);
  });

  testWidgets('扫到合法码 ⇒ 提交，并且不显示任何错误', (WidgetTester tester) async {
    final String good = qrText();
    await mount(tester, FakeQrScanner(codes: <String>[good]));
    await openCamera(tester);

    await scan(tester, good);

    expect(sentVariants(), <String>['OpenHidden', 'SubmitPairing']);
    expect(find.textContaining('这不是 Lumen'), findsNothing);
    // 留在取景模式等服务端回话——这时切回手输会让用户以为还要再输一次。
    expect(find.text('改用手输 9 位码'), findsOneWidget);
  });

  group('★ 四种拒绝各自一句不同的话，且都不提交', () {
    testWidgets('别的服务器 ⇒ 安全警告', (WidgetTester tester) async {
      final String evil = qrText(origin: 'https://evil.example.com');
      await mount(tester, FakeQrScanner(codes: <String>[evil]));
      await openCamera(tester);

      await scan(tester, evil);

      expect(find.textContaining('指向另一台服务器'), findsOneWidget);
      expect(sentVariants(), <String>['OpenHidden'], reason: '拒绝就是不发出去');
    });

    testWidgets('别人的账户', (WidgetTester tester) async {
      final String other = qrText(fingerprint: accountFingerprint('别人'));
      await mount(tester, FakeQrScanner(codes: <String>[other]));
      await openCamera(tester);

      await scan(tester, other);

      expect(find.text('这是别人账户的配对码'), findsOneWidget);
      expect(sentVariants(), <String>['OpenHidden']);
    });

    testWidgets('扫错了电脑', (WidgetTester tester) async {
      final String wrong = qrText(target: 'pc-2');
      await mount(tester, FakeQrScanner(codes: <String>[wrong]));
      await openCamera(tester);

      await scan(tester, wrong);

      expect(find.text('这个码与你要连接的电脑不符'), findsOneWidget);
      expect(sentVariants(), <String>['OpenHidden']);
    });

    testWidgets('随手扫到的别家二维码', (WidgetTester tester) async {
      const String junk = 'https://example.com/一张普通的二维码';
      await mount(tester, const FakeQrScanner(codes: <String>[junk]));
      await openCamera(tester);

      await scan(tester, junk);

      expect(find.text('这不是 Lumen 的配对二维码'), findsOneWidget);
      expect(sentVariants(), <String>['OpenHidden']);
    });
  });

  testWidgets('★ 同一张坏码连扫多次只显示一条（镜头里每帧都会命中）',
      (WidgetTester tester) async {
    final String wrong = qrText(target: 'pc-2');
    await mount(tester, FakeQrScanner(codes: <String>[wrong]));
    await openCamera(tester);

    await scan(tester, wrong);
    await scan(tester, wrong);
    await scan(tester, wrong);

    expect(find.text('这个码与你要连接的电脑不符'), findsOneWidget);
  });

  testWidgets('★ 换一张码要立刻给新反馈（去重不能按时间窗做）',
      (WidgetTester tester) async {
    final String wrongTarget = qrText(target: 'pc-2');
    final String evil = qrText(origin: 'https://evil.example.com');
    await mount(tester, FakeQrScanner(codes: <String>[wrongTarget, evil]));
    await openCamera(tester);

    await scan(tester, wrongTarget);
    expect(find.text('这个码与你要连接的电脑不符'), findsOneWidget);

    await scan(tester, evil);
    expect(find.textContaining('指向另一台服务器'), findsOneWidget);
    expect(find.text('这个码与你要连接的电脑不符'), findsNothing);
  });

  testWidgets('回手输时不留上一轮的错误文案', (WidgetTester tester) async {
    final String wrong = qrText(target: 'pc-2');
    await mount(tester, FakeQrScanner(codes: <String>[wrong]));
    await openCamera(tester);
    await scan(tester, wrong);
    expect(find.text('这个码与你要连接的电脑不符'), findsOneWidget);

    await tester.tap(find.text('改用手输 9 位码'));
    await tester.pump();
    expect(find.text('这个码与你要连接的电脑不符'), findsNothing);
    expect(find.text('连接'), findsOneWidget);

    // 再进相机也是干净的。
    await openCamera(tester);
    expect(find.text('这个码与你要连接的电脑不符'), findsNothing);
  });
}
