/// 登录态的验收。重点是三条「错了会静默产生幽灵设备或误导用户」的：
///
/// 1. 注册失败**不能**接着去登录（否则用户看到的是「密码错」而不是「注册失败」）；
/// 2. `restore` 只认属于当前 origin 的凭据；
/// 3. 登录时**只在 origin 相同**时才带上旧 `device_id`。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/core/env.dart';
import 'package:lumen_mobile/data/device_identity.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/net/net_error.dart';
import 'package:lumen_mobile/net/rest_client.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';
import 'package:lumen_mobile/state/auth_controller.dart';

import '../support/fake_rest.dart';

void main() {
  late ServerEndpoint endpoint;
  late InMemoryTokenStore tokens;
  late FakeRestBackend backend;

  AuthController build() => AuthController(
        rest: RestClient(
          endpoint: endpoint,
          tokens: tokens,
          dio: backend.dio(),
        ),
        tokens: tokens,
        identity: const DeviceIdentity(StaticMachineId('vendor-id-1234')),
        deviceName: const StaticDeviceName('测试机'),
        endpoint: endpoint,
      );

  setUp(() {
    endpoint = ServerEndpoint.tryParse('https://lumen.example.com')!;
    tokens = InMemoryTokenStore();
    backend = FakeRestBackend();
  });

  group('登录', () {
    test('成功后进入已登录态并带出 device_id', () async {
      final AuthController auth = build();
      await auth.login(email: 'a@b.c', password: 'pw');
      final AuthState state = auth.state;
      expect(state, isA<AuthLoggedIn>());
      expect((state as AuthLoggedIn).deviceId, 'phone-1');
    });

    test('首次登录不带 device_id，但**必须**带 hw_id', () async {
      // 没有 hw_id 时，服务端拿一个查不到的 device_id 会 INSERT 新行——
      // 这正是「重装即产幽灵设备」的来源。
      final AuthController auth = build();
      await auth.login(email: 'a@b.c', password: 'pw');
      final DeviceInfo sent = backend.lastLoginDevice!;
      expect(sent.deviceId, isNull);
      expect(sent.hwId, isNotNull);
      expect(sent.hwId!.length, 64);
      expect(sent.os, anyOf('android', 'ios'));
    });

    test('换服务器后不带旧 device_id', () async {
      // 带着另一台服务器的 id 去登录，服务端查不到就会新建一台设备。
      await tokens.write(const SessionTokens(
        token: 't',
        expiresAt: 4102444800,
        deviceId: '别的服务器上的-id',
        origin: 'https://other.example.com',
      ));
      final AuthController auth = build();
      await auth.login(email: 'a@b.c', password: 'pw');
      expect(backend.lastLoginDevice!.deviceId, isNull);
    });

    test('同一服务器回访会带上旧 device_id', () async {
      await tokens.write(SessionTokens(
        token: 't',
        expiresAt: 4102444800,
        deviceId: 'phone-1',
        origin: endpoint.origin,
      ));
      final AuthController auth = build();
      await auth.login(email: 'a@b.c', password: 'pw');
      expect(backend.lastLoginDevice!.deviceId, 'phone-1');
    });

    test('密码错 ⇒ AuthFailed，且不是「未注册」', () async {
      backend.loginStatus = 401;
      backend.loginErrorCode = 'invalid_credentials';
      final AuthController auth = build();
      await auth.login(email: 'a@b.c', password: 'wrong');
      final AuthFailed failed = auth.state as AuthFailed;
      expect(failed.isUserNotFound, isFalse);
      expect(failed.error.userMessage, '邮箱或密码不正确');
    });

    test('未注册 ⇒ isUserNotFound 为真（UI 据此给注册入口）', () async {
      backend.loginStatus = 401;
      backend.loginErrorCode = 'user_not_found';
      final AuthController auth = build();
      await auth.login(email: 'a@b.c', password: 'pw');
      expect((auth.state as AuthFailed).isUserNotFound, isTrue);
    });
  });

  group('注册并登录', () {
    test('注册成功后自动登录', () async {
      final AuthController auth = build();
      await auth.registerThenLogin(email: 'a@b.c', password: 'pw');
      expect(auth.state, isA<AuthLoggedIn>());
      expect(backend.registerCalls, 1);
      expect(backend.loginCalls, 1);
    });

    test('★ 注册失败就停下，不接着去登录', () async {
      // 接着登录会让用户看到「邮箱或密码不正确」——而真正的问题是邮箱已被占用。
      backend.registerStatus = 409;
      backend.registerErrorCode = 'email_taken';
      final AuthController auth = build();
      await auth.registerThenLogin(email: 'a@b.c', password: 'pw');

      expect(auth.state, isA<AuthFailed>());
      expect((auth.state as AuthFailed).error.userMessage, '该邮箱已被注册');
      expect(backend.loginCalls, 0, reason: '注册失败之后不该再打一次登录');
    });
  });

  group('恢复登录态', () {
    test('没有凭据时保持未登录', () async {
      final AuthController auth = build();
      await auth.restore();
      expect(auth.state, isA<AuthLoggedOut>());
    });

    test('★ 只认属于当前 origin 的凭据', () async {
      // 拿另一台服务器的 token 去打新服务器只会收 401，而用户会读成「密码错了」。
      await tokens.write(const SessionTokens(
        token: 't',
        expiresAt: 4102444800,
        deviceId: 'phone-1',
        origin: 'https://other.example.com',
      ));
      final AuthController auth = build();
      await auth.restore();
      expect(auth.state, isA<AuthLoggedOut>());
      expect(await tokens.read(), isNotNull,
          reason: '不属于本 origin 只是忽略，不该删掉——换回去还能用');
    });

    test('同 origin 的凭据能恢复', () async {
      await tokens.write(SessionTokens(
        token: 't',
        expiresAt: 4102444800,
        deviceId: 'phone-1',
        origin: endpoint.origin,
      ));
      final AuthController auth = build();
      await auth.restore();
      expect((auth.state as AuthLoggedIn).deviceId, 'phone-1');
    });
  });

  group('登出与失效', () {
    test('登出清掉凭据', () async {
      await tokens.write(SessionTokens(
        token: 't',
        expiresAt: 4102444800,
        deviceId: 'phone-1',
        origin: endpoint.origin,
      ));
      final AuthController auth = build();
      await auth.logout();
      expect(await tokens.read(), isNull);
      expect(auth.state, isA<AuthLoggedOut>());
    });

    test('会话失效标记成 Unauthorized', () {
      final AuthController auth = build();
      auth.markSessionExpired();
      expect((auth.state as AuthFailed).error, isA<Unauthorized>());
    });
  });
}
