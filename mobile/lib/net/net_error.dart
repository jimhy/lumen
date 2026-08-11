/// 网络层错误。对齐桌面端 `cloud.rs::CloudError`，走 `Result` 不抛异常。
///
/// ## `toString()` 一律脱敏
///
/// 桌面端那份的 `Debug` 实现把 `Network` / `Decode` / `Api.message` 全部打成
/// `[redacted]`，这里照做，理由在手机上更强：日志会被截图、被崩溃上报带走，而错误详情里
/// 常带 URL（含服务器地址）与响应体片段。需要原文排障时用 [`detail`] 字段显式取。
library;

import 'package:lumen_mobile/protocol/rest_dto.dart';

/// 一次网络调用的失败。
sealed class NetError {
  const NetError();

  /// 给用户看的一句话。**不含技术细节**，UI 直接用。
  String get userMessage;
}

/// 连不上 / 超时 / 链路中断。
final class NetworkFailure extends NetError {
  const NetworkFailure(this.detail);

  /// 原始错误描述，**只进日志、不进 UI**。
  final String detail;

  @override
  String get userMessage => '连不上服务器，请检查网络后重试';

  @override
  String toString() => 'NetworkFailure([redacted])';
}

/// 服务端返回了业务错误（4xx / 5xx 且响应体是 [`ApiError`]）。
final class ApiFailure extends NetError {
  const ApiFailure({required this.status, required this.body});

  final int status;
  final ApiError body;

  /// **按 code 分派，不拿 message 做字符串匹配**——message 是英文散文、随时会改。
  ///
  /// 未知 code 展示成「服务器返回错误（<code>）」而不是笼统的「网络错误」：
  /// 用户至少能把这个码念给我们听，而「网络错误」会让他去重启路由器（§14-6）。
  @override
  String get userMessage => switch (body.code) {
        'invalid_credentials' => '邮箱或密码不正确',
        'user_not_found' => '该邮箱还没有注册',
        'email_taken' => '该邮箱已被注册',
        _ => '服务器返回错误（${body.code}）',
      };

  @override
  String toString() => 'ApiFailure($status, ${body.code})';
}

/// 响应体解析失败（字段缺失 / 类型不符 / 不是 JSON）。
///
/// 这**几乎总是**版本不匹配：服务端比本端新，改了字段名。所以文案指向升级而不是重试。
final class DecodeFailure extends NetError {
  const DecodeFailure(this.detail);

  final String detail;

  @override
  String get userMessage => '服务器返回了本版本无法解析的数据，请升级 App';

  @override
  String toString() => 'DecodeFailure([redacted])';
}

/// 鉴权失效：token 过期或设备行已被删，且续期也失败了。
///
/// 收到它的正确动作是**退回登录页并清掉本地 token**，不是重试——服务端 JWT 无 `jti`、
/// 改密码不失效，唯一撤销手段就是删设备行，所以「续期也失败」意味着这台设备真的被踢了。
final class Unauthorized extends NetError {
  const Unauthorized();

  @override
  String get userMessage => '登录已失效，请重新登录';

  @override
  String toString() => 'Unauthorized()';
}

/// 服务器地址不合法（规范化没通过）。
final class BadServerOrigin extends NetError {
  const BadServerOrigin(this.reason);

  final String reason;

  @override
  String get userMessage => '服务器地址不合法：$reason';

  @override
  String toString() => 'BadServerOrigin($reason)';
}
