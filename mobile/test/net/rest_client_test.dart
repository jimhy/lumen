/// REST 客户端的验收，重点是 **401 → refresh → retry** 这条路上的三个坑：
///
/// 1. 续期请求自己 401 时**不许再触发续期**（那是一个打服务端的循环）；
/// 2. 重放**只做一次**；
/// 3. 续期失败要**清掉本地凭据**，否则后续每个请求都走一遍「401 → 续期 → 失败」，
///    把一次失效放大成持续打服务端。
///
/// 三条都不是假想：拦截器版本的实现在这三处各有一个已知的坑法，这也是这里显式手写的原因。
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:dio/dio.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/core/env.dart';
import 'package:lumen_mobile/core/result.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/net/net_error.dart';
import 'package:lumen_mobile/net/rest_client.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';

/// 一次被记录下来的请求。
final class _Call {
  _Call(this.method, this.path, this.bearer);

  final String method;
  final String path;
  final String? bearer;

  @override
  String toString() => '$method $path (${bearer ?? "无 token"})';
}

/// 可编程的假 HTTP 适配器。
final class _FakeAdapter implements HttpClientAdapter {
  _FakeAdapter(this.handler);

  final Future<ResponseBody> Function(_Call call) handler;
  final List<_Call> calls = <_Call>[];

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<Uint8List>? requestStream,
    Future<void>? cancelFuture,
  ) {
    final Object? auth = options.headers['Authorization'];
    final _Call call = _Call(
      options.method,
      options.path,
      auth is String ? auth.replaceFirst('Bearer ', '') : null,
    );
    calls.add(call);
    return handler(call);
  }

  @override
  void close({bool force = false}) {}
}

ResponseBody _json(int status, Object? body) => ResponseBody.fromString(
      jsonEncode(body),
      status,
      headers: <String, List<String>>{
        Headers.contentTypeHeader: <String>['application/json'],
      },
    );

const JsonMapLike _devicesBody = <String, Object?>{
  'devices': <Object?>[
    <String, Object?>{
      'id': 'pc-1',
      'name': '工作站',
      'os': 'windows',
      'app_version': '1.0.30',
      'online': true,
      'last_seen': 1786342700,
      'is_self': false,
    },
  ],
};

/// 语义与 `JsonMap` 相同，只为常量声明可读。
typedef JsonMapLike = Map<String, Object?>;

void main() {
  late InMemoryTokenStore tokens;
  late ServerEndpoint endpoint;

  setUp(() {
    endpoint = ServerEndpoint.tryParse('https://lumen.example.com')!;
    tokens = InMemoryTokenStore();
  });

  RestClient build(_FakeAdapter adapter) {
    final Dio dio = Dio()..httpClientAdapter = adapter;
    return RestClient(endpoint: endpoint, tokens: tokens, dio: dio);
  }

  Future<void> seedToken({String token = 'old-token', int expiresAt = 4102444800}) =>
      tokens.write(SessionTokens(
        token: token,
        expiresAt: expiresAt,
        deviceId: 'phone-1',
        origin: endpoint.origin,
      ));

  group('登录', () {
    test('成功后就地写入 TokenStore', () async {
      final _FakeAdapter adapter = _FakeAdapter((_Call c) async => _json(200, <String, Object?>{
            'protocol_version': 4,
            'token': 'fresh-token',
            'expires_at': 1786944000,
            'user': <String, Object?>{
              'id': 'u1',
              'email': 'a@b.c',
              'display_name': 'a',
            },
            'device_id': 'phone-1',
          }));
      final Result<AuthResponse, NetError> r = await build(adapter).login(
        email: 'a@b.c',
        password: 'pw',
        device: const DeviceInfo(name: '手机', os: 'android', appVersion: '0.1.0'),
      );

      expect(r.isOk, isTrue);
      final SessionTokens? stored = await tokens.read();
      expect(stored!.token, 'fresh-token');
      expect(stored.deviceId, 'phone-1');
      expect(stored.origin, endpoint.origin,
          reason: '凭据按 origin 分区：换服务器后旧 token 一文不值');
    });

    test('密码错 ⇒ ApiFailure，文案按 code 而不是 message', () async {
      final _FakeAdapter adapter = _FakeAdapter((_Call c) async => _json(401, <String, Object?>{
            'code': 'invalid_credentials',
            'message': 'invalid email or password',
          }));
      final Result<AuthResponse, NetError> r = await build(adapter).login(
        email: 'a@b.c',
        password: 'wrong',
        device: const DeviceInfo(name: '手机', os: 'android', appVersion: '0.1.0'),
      );

      final NetError e = r.errorOrNull!;
      expect(e, isA<ApiFailure>());
      expect(e.userMessage, '邮箱或密码不正确');
      expect(adapter.calls.length, 1,
          reason: '登录本身不该走 401 重试——那条路是给带 token 的请求准备的');
    });
  });

  group('401 → refresh → retry', () {
    test('续期成功后用新 token 重放一次', () async {
      await seedToken();
      final _FakeAdapter adapter = _FakeAdapter((_Call c) async {
        if (c.path == LumenRoutes.refresh) {
          return _json(200, <String, Object?>{
            'token': 'new-token',
            'expires_at': 1787548800,
          });
        }
        return c.bearer == 'new-token'
            ? _json(200, _devicesBody)
            : _json(401, <String, Object?>{'code': 'unauthorized', 'message': 'x'});
      });

      final Result<DeviceListResponse, NetError> r =
          await build(adapter).listDevices();

      expect(r.isOk, isTrue);
      expect(
        adapter.calls.map((_Call c) => '${c.path}:${c.bearer}').toList(),
        <String>[
          '${LumenRoutes.devices}:old-token',
          '${LumenRoutes.refresh}:old-token',
          '${LumenRoutes.devices}:new-token',
        ],
      );
      expect((await tokens.read())!.token, 'new-token');
    });

    test('续期自己 401 ⇒ Unauthorized，且**不递归**、清掉本地凭据', () async {
      await seedToken();
      final _FakeAdapter adapter = _FakeAdapter(
        (_Call c) async => _json(401, <String, Object?>{'code': 'x', 'message': 'y'}),
      );

      final Result<DeviceListResponse, NetError> r =
          await build(adapter).listDevices();

      expect(r.errorOrNull, isA<Unauthorized>());
      expect(
        adapter.calls.map((_Call c) => c.path).toList(),
        <String>[LumenRoutes.devices, LumenRoutes.refresh],
        reason: '续期失败就到此为止，不许再来一轮',
      );
      expect(await tokens.read(), isNull,
          reason: '留着失效凭据 = 之后每个请求都走一遍失败的续期');
    });

    test('重放仍 401 ⇒ 只试一次就放弃', () async {
      await seedToken();
      final _FakeAdapter adapter = _FakeAdapter((_Call c) async {
        if (c.path == LumenRoutes.refresh) {
          return _json(200, <String, Object?>{
            'token': 'new-token',
            'expires_at': 1787548800,
          });
        }
        return _json(401, <String, Object?>{'code': 'x', 'message': 'y'});
      });

      final Result<DeviceListResponse, NetError> r =
          await build(adapter).listDevices();

      expect(r.errorOrNull, isA<Unauthorized>());
      expect(
        adapter.calls.where((_Call c) => c.path == LumenRoutes.devices).length,
        2,
        reason: '再试就是打服务端',
      );
    });

    test('没有本地凭据时直接 Unauthorized，不发请求', () async {
      final _FakeAdapter adapter =
          _FakeAdapter((_Call c) async => _json(200, _devicesBody));
      final Result<DeviceListResponse, NetError> r =
          await build(adapter).listDevices();

      expect(r.errorOrNull, isA<Unauthorized>());
      expect(adapter.calls, isEmpty);
    });
  });

  group('错误映射', () {
    test('链路失败 ⇒ NetworkFailure，且 toString 不带细节', () async {
      await seedToken();
      final _FakeAdapter adapter = _FakeAdapter((_Call c) async {
        throw DioException.connectionTimeout(
          timeout: const Duration(seconds: 10),
          requestOptions: RequestOptions(path: c.path),
        );
      });

      final NetError e = (await build(adapter).listDevices()).errorOrNull!;
      expect(e, isA<NetworkFailure>());
      expect(e.userMessage, contains('连不上服务器'));
      expect(e.toString(), 'NetworkFailure([redacted])',
          reason: '错误详情常带 URL 与响应体片段，日志会被截图带走');
    });

    test('2xx 但响应体对不上 ⇒ DecodeFailure，文案指向升级', () async {
      await seedToken();
      final _FakeAdapter adapter = _FakeAdapter(
        (_Call c) async => _json(200, <String, Object?>{'unexpected': true}),
      );

      final NetError e = (await build(adapter).listDevices()).errorOrNull!;
      expect(e, isA<DecodeFailure>());
      expect(e.userMessage, contains('升级'),
          reason: '解析失败几乎总是版本不匹配，让用户重试没有意义');
    });

    test('非 2xx 且响应体不是 ApiError ⇒ 用状态码合成一个', () async {
      await seedToken();
      final _FakeAdapter adapter = _FakeAdapter(
        (_Call c) async => ResponseBody.fromString('<html>502</html>', 502),
      );

      final NetError e = (await build(adapter).listDevices()).errorOrNull!;
      expect(e, isA<ApiFailure>());
      // 合成而不是回 DecodeFailure：用户此刻需要的是「服务器拒绝了，码是 xxx」，
      // 而不是「本 App 看不懂服务器的拒绝理由」。
      expect(e.userMessage, contains('http_502'));
    });

    test('未知 code 展示成带码的提示，不吞成「网络错误」', () async {
      await seedToken();
      final _FakeAdapter adapter = _FakeAdapter((_Call c) async =>
          _json(429, <String, Object?>{'code': 'rate_limited', 'message': 'slow down'}));

      final NetError e = (await build(adapter).listDevices()).errorOrNull!;
      expect(e.userMessage, '服务器返回错误（rate_limited）');
    });
  });

  group('提前续期', () {
    test('还早时不发请求', () async {
      await seedToken(expiresAt: 4102444800); // 2100 年
      final _FakeAdapter adapter =
          _FakeAdapter((_Call c) async => _json(200, <String, Object?>{}));
      expect(await build(adapter).refreshIfNeeded(), isTrue);
      expect(adapter.calls, isEmpty);
    });

    test('快到期时续一次', () async {
      final int soon =
          DateTime.now().add(const Duration(hours: 2)).millisecondsSinceEpoch ~/ 1000;
      await seedToken(expiresAt: soon);
      final _FakeAdapter adapter = _FakeAdapter((_Call c) async => _json(200, <String, Object?>{
            'token': 'renewed',
            'expires_at': 4102444800,
          }));

      expect(await build(adapter).refreshIfNeeded(), isTrue);
      expect(adapter.calls.single.path, LumenRoutes.refresh);
      expect((await tokens.read())!.token, 'renewed');
    });

    test('没登录时返回 false', () async {
      final _FakeAdapter adapter =
          _FakeAdapter((_Call c) async => _json(200, <String, Object?>{}));
      expect(await build(adapter).refreshIfNeeded(), isFalse);
      expect(adapter.calls, isEmpty);
    });
  });
}
