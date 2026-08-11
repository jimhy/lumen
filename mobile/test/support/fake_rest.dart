/// 可编程的假 REST 后端，给 `AuthController` 这类「上面还有一层逻辑」的测试用。
///
/// 与 `rest_client_test.dart` 里那个内联的 `_FakeAdapter` 分工不同：那个测的是
/// `RestClient` **自己**的重试与错误映射，需要逐次控制每个响应；这个测的是**它上面**的
/// 逻辑，只需要「登录成功 / 登录失败 / 注册失败」这几档，外加把请求体记下来供断言。
library;

import 'dart:convert';
import 'dart:typed_data';

import 'package:dio/dio.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';

/// 假后端。默认一切成功。
final class FakeRestBackend {
  /// 登录返回的状态码。
  int loginStatus = 200;

  /// `loginStatus` 非 2xx 时返回的错误码。
  String loginErrorCode = 'invalid_credentials';

  /// 注册返回的状态码。
  int registerStatus = 200;

  /// `registerStatus` 非 2xx 时返回的错误码。
  String registerErrorCode = 'email_taken';

  int loginCalls = 0;
  int registerCalls = 0;

  /// 最后一次登录上报的设备信息——`hw_id` / `device_id` 这类「错了会静默产生幽灵设备」
  /// 的字段只能在这里断言。
  DeviceInfo? lastLoginDevice;

  /// 组一个接到本假后端的 dio。
  Dio dio() => Dio()..httpClientAdapter = _Adapter(this);

  ResponseBody _handle(RequestOptions options) {
    final String path = options.path;
    if (path == LumenRoutes.login) {
      loginCalls++;
      final JsonMap body =
          asJsonMap(jsonDecode(options.data as String), 'login body');
      lastLoginDevice =
          DeviceInfo.fromJson(asJsonMap(body['device'], 'device'));
      return loginStatus == 200
          ? _json(200, <String, Object?>{
              'protocol_version': 4,
              'token': 'tok-1',
              'expires_at': 4102444800,
              'user': <String, Object?>{
                'id': 'u1',
                'email': 'a@b.c',
                'display_name': 'a',
              },
              'device_id': 'phone-1',
            })
          : _json(loginStatus,
              <String, Object?>{'code': loginErrorCode, 'message': 'x'});
    }
    if (path == LumenRoutes.register) {
      registerCalls++;
      return registerStatus == 200
          ? _json(200, <String, Object?>{
              'id': 'u1',
              'email': 'a@b.c',
              'display_name': 'a',
            })
          : _json(registerStatus,
              <String, Object?>{'code': registerErrorCode, 'message': 'x'});
    }
    if (path == LumenRoutes.refresh) {
      return _json(200,
          <String, Object?>{'token': 'tok-2', 'expires_at': 4102444800});
    }
    return _json(404, <String, Object?>{'code': 'not_found', 'message': 'x'});
  }
}

ResponseBody _json(int status, Object? body) => ResponseBody.fromString(
      jsonEncode(body),
      status,
      headers: <String, List<String>>{
        Headers.contentTypeHeader: <String>['application/json'],
      },
    );

final class _Adapter implements HttpClientAdapter {
  _Adapter(this._backend);

  final FakeRestBackend _backend;

  @override
  Future<ResponseBody> fetch(
    RequestOptions options,
    Stream<Uint8List>? requestStream,
    Future<void>? cancelFuture,
  ) async =>
      _backend._handle(options);

  @override
  void close({bool force = false}) {}
}
