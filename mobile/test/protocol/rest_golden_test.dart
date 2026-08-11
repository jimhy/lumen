/// **片 5 的 REST 验收**：`rest_*` 语料与 Dart DTO 对齐。
///
/// 这一层此前**两端都没有守卫**：桌面端与服务端共用同一份 Rust 类型所以永远不会漂，
/// 手机端是第二份手写实现——改一个字段名，Rust 侧全绿，手机端要等到有人真的点了登录才发现。
library;

import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';
import 'package:flutter_test/flutter_test.dart';

import 'corpus.dart';

/// `rest_type` → 「解出来再编回去」。新增 DTO 要同时加进这里与 Rust 侧的 `REST类型全集`。
final Map<String, JsonMap Function(JsonMap)> _roundTrip =
    <String, JsonMap Function(JsonMap)>{
  'RegisterRequest': (JsonMap m) => RegisterRequest.fromJson(m).toJson(),
  'LoginRequest': (JsonMap m) => LoginRequest.fromJson(m).toJson(),
  'AuthResponse': (JsonMap m) => AuthResponse.fromJson(m).toJson(),
  'RefreshResponse': (JsonMap m) => RefreshResponse.fromJson(m).toJson(),
  'DeviceListResponse': (JsonMap m) => DeviceListResponse.fromJson(m).toJson(),
  'ApiError': (JsonMap m) => ApiError.fromJson(m).toJson(),
};

void main() {
  final List<GoldenCase> corpus = loadCorpus();
  final List<GoldenCase> restCases =
      corpus.where((GoldenCase c) => c.rest != null).toList();

  GoldenCase caseNamed(String name) =>
      corpus.firstWhere((GoldenCase c) => c.name == name);

  group('rest 语料 ⇄ Dart DTO', () {
    test('六个顶层 DTO 都有语料', () {
      final Set<String> covered =
          restCases.map((GoldenCase c) => c.restType!).toSet();
      expect(covered, _roundTrip.keys.toSet());
    });

    for (final GoldenCase c in restCases) {
      test('${c.name}：${c.note.split('。').first}', () {
        final JsonMap Function(JsonMap)? rt = _roundTrip[c.restType];
        expect(rt, isNotNull, reason: '${c.name} 的 rest_type 不认识：${c.restType}');
        final JsonMap actual = rt!(asJsonMap(c.rest, 'rest'));
        expect(
          deepJsonEquals(actual, c.rest),
          isTrue,
          reason: '${c.name} 往返不等。\n'
              '--- 实际 ---\n${pretty(actual)}\n'
              '--- 期望 ---\n${pretty(c.rest)}',
        );
      });
    }
  });

  group('DeviceInfo 的两个可选字段（最容易漂的一处）', () {
    test('首次登录：device_id / hw_id 的键必须整个消失，不是写成 null', () {
      // 服务端 upsert_device 拿一个查不到的 device_id 会 **INSERT 新行**，
      // hw_id 正是防这件事的键。写成 null 现在还能工作（服务端 filter 走同一条分支），
      // 直到某次有人把它改成空串——账户里就开始长幽灵设备。
      final JsonMap json = LoginRequest.fromJson(
        asJsonMap(caseNamed('rest_login_request_first_time.json').rest, 'rest'),
      ).toJson();
      final JsonMap device = asJsonMap(json['device'], 'device');
      expect(device.containsKey('device_id'), isFalse);
      expect(device.containsKey('hw_id'), isFalse);
      expect(device['name'], '海风的手机');
    });

    test('回访：两个字段都在，且没有接反', () {
      final LoginRequest r = LoginRequest.fromJson(
        asJsonMap(caseNamed('rest_login_request.json').rest, 'rest'),
      );
      expect(r.device.deviceId, 'phone-1');
      expect(r.device.hwId, startsWith('9f2c4e7a'));
      expect(r.device.hwId!.length, 64, reason: 'sha256 的 hex 是 64 个字符');
      expect(r.device.os, 'android');
      expect(r.email, 'haifeng@example.com');
    });
  });

  group('字段取值断言', () {
    test('rest_auth_response：两个整数刻意量级不同，别接反', () {
      final AuthResponse r = AuthResponse.fromJson(
        asJsonMap(caseNamed('rest_auth_response.json').rest, 'rest'),
      );
      expect(r.protocolVersion, 4);
      expect(r.expiresAt, 1786944000);
      expect(r.deviceId, 'phone-1');
      expect(r.user.displayName, 'haifeng');
      expect(r.user.id, 'user-1');
    });

    test('rest_device_list：保持服务端顺序，不按在线状态重排', () {
      // 服务端按注册时间升序返回，这是一次修复的结果（老实现按 last_seen 降序，
      // 每次心跳都会让列表重排、设备跳位）。在客户端重排会把那条修复重新引入。
      final DeviceListResponse r = DeviceListResponse.fromJson(
        asJsonMap(caseNamed('rest_device_list.json').rest, 'rest'),
      );
      expect(
        r.devices.map((DeviceRecord d) => d.id).toList(),
        <String>['pc-1', 'phone-1', 'laptop-2'],
      );
      expect(r.devices[0].online, isTrue);
      expect(r.devices[0].isSelf, isFalse);
      expect(r.devices[1].isSelf, isTrue, reason: '第二台才是本机，两个布尔别接反');
      expect(r.devices[2].online, isFalse);
      expect(r.devices[2].lastSeen, 1785000000);
    });

    test('rest_refresh_response：新 token 与新到期时刻', () {
      final RefreshResponse r = RefreshResponse.fromJson(
        asJsonMap(caseNamed('rest_refresh_response.json').rest, 'rest'),
      );
      expect(r.token, endsWith('renewed'));
      expect(r.expiresAt, 1787548800);
    });
  });

  group('密码与 token 不进调试渲染', () {
    // Rust 侧 LoginRequest 是 derive(Debug)，`{:?}` 会打出明文密码。
    // 手机上的日志更容易被截图、被崩溃上报带走，这一层不复制那个坑。
    test('LoginRequest.toString() 不含密码', () {
      final LoginRequest r = LoginRequest.fromJson(
        asJsonMap(caseNamed('rest_login_request.json').rest, 'rest'),
      );
      expect(r.toString().contains('correct horse'), isFalse);
      expect(r.toString().contains('haifeng@example.com'), isTrue,
          reason: '邮箱要能看到，否则排障时分不清是哪个账号');
    });

    test('RegisterRequest / AuthResponse / RefreshResponse 同样脱敏', () {
      final RegisterRequest reg = RegisterRequest.fromJson(
        asJsonMap(caseNamed('rest_register_request.json').rest, 'rest'),
      );
      expect(reg.toString().contains('correct horse'), isFalse);

      final AuthResponse auth = AuthResponse.fromJson(
        asJsonMap(caseNamed('rest_auth_response.json').rest, 'rest'),
      );
      expect(auth.toString().contains('eyJhbGciOiJIUzI1NiJ9'), isFalse);

      final RefreshResponse refresh = RefreshResponse.fromJson(
        asJsonMap(caseNamed('rest_refresh_response.json').rest, 'rest'),
      );
      expect(refresh.toString().contains('eyJhbGciOiJIUzI1NiJ9'), isFalse);
    });

    test('DeviceInfo.toString() 只打 hw_id 前 8 位', () {
      final LoginRequest r = LoginRequest.fromJson(
        asJsonMap(caseNamed('rest_login_request.json').rest, 'rest'),
      );
      final String rendered = r.device.toString();
      expect(rendered.contains('9f2c4e7a'), isTrue);
      expect(rendered.contains(r.device.hwId!), isFalse);
    });
  });
}
