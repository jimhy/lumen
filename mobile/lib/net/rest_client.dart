/// REST 客户端：注册 / 登录 / 续期 / 设备列表 / 心跳。
///
/// ## 401 → refresh → retry 是**显式**写的，不是 dio 拦截器
///
/// 拦截器版本读起来短，但有两个坑在这个协议下都是真的会踩的：
///
/// 1. **递归续期**。续期请求自己收到 401 时，拦截器会再触发一次续期——服务端 JWT 过期后
///    `refresh` 必然 401，于是变成一个打服务端的循环。这里把续期走**不带重试**的通道
///    （[`_raw`]），从结构上不可能递归。
/// 2. **重试次数说不清**。「只重试一次」在拦截器里要靠给 `RequestOptions` 打标记来实现，
///    而那个标记在重放时容易丢。显式写就是一行 `if`。
///
/// ## 续期失败 = 退回登录页，不是重试
///
/// 服务端 `refresh` 除验签外还会确认**设备行仍存在**。所以「续期也 401」意味着 token 真的
/// 过期了，或者这台设备被从账户里删了——两种情况都只能重新登录。收到 [`Unauthorized`] 时
/// 调用方要清本地 token 并回登录页。
///
/// ## 为什么不在这里做「明文 HTTP 拦截」
///
/// `ServerEndpoint.isInsecure` 只是判据，拒不拒绝是**产品决定**（自建服务器 + 局域网 IP
/// 是真实用法）。这一层只提供事实，由登录页决定是拒绝还是显著告警。
library;

import 'dart:convert';

import 'package:dio/dio.dart';
import 'package:lumen_mobile/core/env.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/core/result.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/net/net_error.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';

const String _tag = 'rest';

/// 服务端全局请求体上限是 1 MiB；本端超时留足弱网余量但不至于让用户干等。
const Duration _kConnectTimeout = Duration(seconds: 10);
const Duration _kReceiveTimeout = Duration(seconds: 20);

/// Lumen 服务端的 REST 接口。
final class RestClient {
  RestClient({
    required this.endpoint,
    required TokenStore tokens,
    Dio? dio,
  })  : _tokens = tokens,
        _dio = dio ?? Dio() {
    _dio.options = _dio.options.copyWith(
      baseUrl: endpoint.restBase,
      connectTimeout: _kConnectTimeout,
      receiveTimeout: _kReceiveTimeout,
      // 状态码全部放行，错误在下面统一分派——不让 dio 用异常表达业务失败。
      validateStatus: (int? _) => true,
      responseType: ResponseType.plain,
    );
  }

  final ServerEndpoint endpoint;
  final TokenStore _tokens;
  final Dio _dio;

  /// 注册。**不登记设备、不返回 token**，注册完还要走一次 [login]。
  Future<Result<UserInfo, NetError>> register({
    required String email,
    required String password,
  }) async {
    final _HttpResult r = await _raw(
      'POST',
      LumenRoutes.register,
      body: RegisterRequest(email: email, password: password).toJson(),
    );
    return r.decode(UserInfo.fromJson);
  }

  /// 登录。成功后**就地写入** [`TokenStore`]——调用方少一步就少一次「忘了存」的机会。
  Future<Result<AuthResponse, NetError>> login({
    required String email,
    required String password,
    required DeviceInfo device,
  }) async {
    final _HttpResult r = await _raw(
      'POST',
      LumenRoutes.login,
      body: LoginRequest(email: email, password: password, device: device)
          .toJson(),
    );
    final Result<AuthResponse, NetError> parsed = r.decode(AuthResponse.fromJson);
    if (parsed case Ok<AuthResponse, NetError>(:final AuthResponse value)) {
      await _tokens.write(SessionTokens(
        token: value.token,
        expiresAt: value.expiresAt,
        deviceId: value.deviceId,
        origin: endpoint.origin,
      ));
    }
    return parsed;
  }

  /// 设备列表。
  Future<Result<DeviceListResponse, NetError>> listDevices() async {
    final Result<_HttpResult, NetError> r =
        await _authed('GET', LumenRoutes.devices);
    return switch (r) {
      Ok<_HttpResult, NetError>(:final _HttpResult value) =>
        value.decode(DeviceListResponse.fromJson),
      Err<_HttpResult, NetError>(:final NetError error) =>
        Err<DeviceListResponse, NetError>(error),
    };
  }

  /// 心跳：刷新本设备 `last_seen`，让它在别人的设备列表里保持在线。
  ///
  /// WS 那条 `Ping` 也会刷（服务端 25 秒节流），所以连着的时候不必额外调这个；
  /// 它是给「App 在前台但还没建立 WS」那段窗口用的。
  Future<Result<void, NetError>> heartbeat() async {
    final Result<_HttpResult, NetError> r =
        await _authed('POST', LumenRoutes.heartbeat, body: const <String, Object?>{});
    return switch (r) {
      Ok<_HttpResult, NetError>() => const Ok<void, NetError>(null),
      Err<_HttpResult, NetError>(:final NetError error) =>
        Err<void, NetError>(error),
    };
  }

  /// 当前 token 快到期就续一次。返回是否现在持有可用 token。
  ///
  /// 提前续期而不是等 401：WS 长连接没有重放机制，token 在连接存活期间过期的表现是
  /// 「莫名其妙掉线」。
  Future<bool> refreshIfNeeded({DateTime? now}) async {
    final SessionTokens? cur = await _tokens.read();
    if (cur == null) return false;
    if (!cur.needsRefresh(now ?? DateTime.now())) return true;
    return _refresh();
  }

  // ── 内部 ───────────────────────────────────────────────────────────────────

  /// 带鉴权的请求：401 时续期一次并重放，仍失败即 [`Unauthorized`]。
  Future<Result<_HttpResult, NetError>> _authed(
    String method,
    String path, {
    Object? body,
  }) async {
    final SessionTokens? cur = await _tokens.read();
    if (cur == null) return const Err<_HttpResult, NetError>(Unauthorized());

    _HttpResult r = await _raw(method, path, body: body, bearer: cur.token);
    if (r.status != 401) return Ok<_HttpResult, NetError>(r);

    logInfo(_tag, '收到 401，尝试续期后重放一次');
    if (!await _refresh()) {
      return const Err<_HttpResult, NetError>(Unauthorized());
    }
    final SessionTokens? renewed = await _tokens.read();
    if (renewed == null) return const Err<_HttpResult, NetError>(Unauthorized());

    r = await _raw(method, path, body: body, bearer: renewed.token);
    // 重放**只做一次**：还是 401 就是真的没资格了，再试是打服务端。
    return r.status == 401
        ? const Err<_HttpResult, NetError>(Unauthorized())
        : Ok<_HttpResult, NetError>(r);
  }

  /// 换发新 token。**走不带重试的通道**，从结构上杜绝递归续期。
  Future<bool> _refresh() async {
    final SessionTokens? cur = await _tokens.read();
    if (cur == null) return false;
    final _HttpResult r =
        await _raw('POST', LumenRoutes.refresh, bearer: cur.token);
    final Result<RefreshResponse, NetError> parsed =
        r.decode(RefreshResponse.fromJson);
    switch (parsed) {
      case Ok<RefreshResponse, NetError>(:final RefreshResponse value):
        await _tokens.write(SessionTokens(
          token: value.token,
          expiresAt: value.expiresAt,
          deviceId: cur.deviceId,
          origin: cur.origin,
        ));
        return true;
      case Err<RefreshResponse, NetError>(:final NetError error):
        // 续期失败 = 这台设备被踢了或 token 已过期，**清掉本地凭据**：留着只会让后续
        // 每一个请求都走一遍「401 → 续期 → 失败」，把一次失效放大成持续打服务端。
        logWarn(_tag, '续期失败，清空本地凭据', error: error);
        await _tokens.clear();
        return false;
    }
  }

  /// 发一个请求，不做任何重试。
  Future<_HttpResult> _raw(
    String method,
    String path, {
    Object? body,
    String? bearer,
  }) async {
    try {
      final Response<String> resp = await _dio.request<String>(
        path,
        data: body == null ? null : jsonEncode(body),
        options: Options(
          method: method,
          headers: <String, Object?>{
            if (bearer != null) 'Authorization': 'Bearer $bearer',
            if (body != null) Headers.contentTypeHeader: Headers.jsonContentType,
          },
        ),
      );
      return _HttpResult(resp.statusCode ?? 0, resp.data ?? '');
    } on DioException catch (e) {
      logWarn(_tag, '$method $path 网络失败', error: e);
      return _HttpResult.networkFailure(e.message ?? e.type.name);
    }
  }
}

/// 一次 HTTP 往返的结果：要么有状态码与正文，要么是链路失败。
final class _HttpResult {
  const _HttpResult(this.status, this.text) : networkError = null;

  const _HttpResult.networkFailure(String detail)
      : status = 0,
        text = '',
        networkError = detail;

  final int status;
  final String text;
  final String? networkError;

  /// 把正文解成 `T`，并把非 2xx 映射成 [`ApiFailure`]。
  Result<T, NetError> decode<T>(T Function(JsonMap) parse) {
    final String? net = networkError;
    if (net != null) {
      return Err<T, NetError>(NetworkFailure(net));
    }
    if (status < 200 || status >= 300) {
      return Err<T, NetError>(ApiFailure(status: status, body: _errorBody()));
    }
    try {
      return Ok<T, NetError>(parse(asJsonMap(jsonDecode(text), '响应体')));
    } on Object catch (e) {
      // 解析失败几乎总是版本不匹配（服务端改了字段名），文案因此指向升级而不是重试。
      logWarn(_tag, '响应体解析失败', error: e);
      return Err<T, NetError>(DecodeFailure('$e'));
    }
  }

  /// 尽力把错误体解成 [`ApiError`]；解不出就用 HTTP 状态码合成一个。
  ///
  /// 合成而不是回 [`DecodeFailure`]：用户此刻需要的是「服务器拒绝了，码是 xxx」，
  /// 而不是「本 App 看不懂服务器的拒绝理由」——后者把一个明确的失败变成了一句废话。
  ApiError _errorBody() {
    try {
      return ApiError.fromJson(asJsonMap(jsonDecode(text), '错误体'));
    } on Object {
      return ApiError(code: 'http_$status', message: 'HTTP $status');
    }
  }
}
