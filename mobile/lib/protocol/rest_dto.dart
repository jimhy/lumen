/// REST 接口的请求 / 响应体——**零 Flutter 依赖**。
///
/// 对应 Rust 侧 `crates/lumen-protocol/src/lib.rs` 里那批结构体（**权威定义在那里**）。
/// 路径常量见 [`LumenRoutes`]。
///
/// ## 这一层此前两端都没有守卫
///
/// REST DTO 不在 `RemoteFrame` 里：桌面端与服务端共用同一份 Rust 类型，所以永远不会漂；
/// 手机端是**第二份手写实现**——改一个字段名，Rust 侧全绿，手机端要等到有人真的点了登录
/// 才发现。片 5 起 `rest_*.json` 语料把两侧钉在一起。
///
/// ## 最容易漂的一处：`DeviceInfo` 的两个可选字段
///
/// `deviceId` / `hwId` 是 `Option + skip_serializing_if`：**为 null 时键必须整个消失**，
/// 不能写成 `"hw_id": null`。这里犯错的后果比在 LLM 帧里重——服务端
/// `handlers.rs` 的 `upsert_device` 拿一个查不到的 `device_id` 会**INSERT 新行**，
/// 而 `hw_id` 正是防这件事的键。写成 null 现在还能工作（服务端 `filter(|s| !s.is_empty())`
/// 走同一条分支），直到某次有人把它改成空串——账户里就开始长幽灵设备。
///
/// ## 密码与 token 不进 `toString()`
///
/// Rust 侧 `LoginRequest` 是 `derive(Debug)`，`{:?}` 会打出明文密码——**这一层不复制那个坑**。
/// 手机上的日志更容易被截图、被崩溃上报带走。
library;

import 'package:lumen_mobile/protocol/codec.dart';

/// REST 路径。与 Rust 侧 `lumen_protocol::routes` 逐字对应。
///
/// 写成常量而不是在调用处拼字符串：路径改了要一处改完，且拼错在编译期就露馅。
abstract final class LumenRoutes {
  /// 健康检查 `GET`（顺带回服务端协议版本）。
  static const String health = '/api/v1/health';

  /// 注册 `POST`。**不登记设备**，注册完还要走一次登录才拿得到 token。
  static const String register = '/api/v1/auth/register';

  /// 登录 `POST`（成功即登记 / 更新本设备）。
  static const String login = '/api/v1/auth/login';

  /// 续期 `POST`（需 Bearer **现有有效** token）。
  static const String refresh = '/api/v1/auth/refresh';

  /// 设备列表 `GET`。
  static const String devices = '/api/v1/devices';

  /// 心跳 `POST`（刷新本设备 `last_seen`）。
  static const String heartbeat = '/api/v1/heartbeat';

  /// 远程控制 WebSocket `GET`（升级）。
  ///
  /// ⚠ token 必须走 `Authorization` 头，服务端**不接受** query 参数
  /// （理由是避免反代日志泄漏）。这条同时是「将来出 Web 版」的硬阻塞点。
  static const String ws = '/api/v1/ws';

  /// 单设备路径（重命名 `PATCH` / 删除 `DELETE`）。
  static String device(String id) => '/api/v1/devices/$id';
}

/// 客户端上报的设备信息（登录时携带）。
final class DeviceInfo {
  const DeviceInfo({
    required this.name,
    required this.os,
    required this.appVersion,
    this.deviceId,
    this.hwId,
  });

  /// 已有设备 id（首次登录为 null，由服务端分配并回传，**客户端必须持久化**）。
  final String? deviceId;

  /// 稳定硬件伪名 `sha256(canonical_origin ‖ 0x00 ‖ 原始机器标识)` 的 hex。
  ///
  /// 服务端据 `(user_id, hw_id)` **幂等认领**同一台物理机的唯一行，杜绝「带空 / 异
  /// `device_id` 就分裂出幽灵设备」。**原始机器标识永不上网**；不同 origin 得到不同伪名，
  /// 所以服务端无法跨服务关联同一台手机。取不到时为 null（服务端退化回按 `device_id` 处理）。
  final String? hwId;

  final String name;

  /// `android` / `ios`。
  final String os;

  final String appVersion;

  factory DeviceInfo.fromJson(JsonMap m) => DeviceInfo(
        deviceId: readStringOpt(m, 'device_id'),
        hwId: readStringOpt(m, 'hw_id'),
        name: readString(m, 'name'),
        os: readString(m, 'os'),
        appVersion: readString(m, 'app_version'),
      );

  JsonMap toJson() {
    final JsonMap out = <String, Object?>{};
    // 顺序无关（按值比较），但两个可选字段必须走 writeOpt：null 时键整个消失。
    writeOpt(out, 'device_id', deviceId);
    writeOpt(out, 'hw_id', hwId);
    out['name'] = name;
    out['os'] = os;
    out['app_version'] = appVersion;
    return out;
  }

  /// `hwId` 只打前 8 位：它是伪名不是秘密，但完整值进日志没有任何用处。
  @override
  String toString() {
    final String hw = hwId == null ? '无' : '${hwId!.substring(0, 8)}…';
    return 'DeviceInfo($name/$os/$appVersion, id=${deviceId ?? "无"}, hw=$hw)';
  }
}

/// 注册请求。
final class RegisterRequest {
  const RegisterRequest({required this.email, required this.password});

  final String email;

  /// 明文密码（仅传输用，服务端 argon2 哈希后落库）。
  final String password;

  factory RegisterRequest.fromJson(JsonMap m) => RegisterRequest(
        email: readString(m, 'email'),
        password: readString(m, 'password'),
      );

  JsonMap toJson() =>
      <String, Object?>{'email': email, 'password': password};

  @override
  String toString() => 'RegisterRequest($email, password=<已隐藏>)';
}

/// 登录请求（成功即登记 / 更新本设备）。
final class LoginRequest {
  const LoginRequest({
    required this.email,
    required this.password,
    required this.device,
  });

  final String email;
  final String password;
  final DeviceInfo device;

  factory LoginRequest.fromJson(JsonMap m) => LoginRequest(
        email: readString(m, 'email'),
        password: readString(m, 'password'),
        device: readObject(m, 'device', DeviceInfo.fromJson),
      );

  JsonMap toJson() => <String, Object?>{
        'email': email,
        'password': password,
        'device': device.toJson(),
      };

  @override
  String toString() => 'LoginRequest($email, password=<已隐藏>, $device)';
}

/// 账户公开信息。
final class UserInfo {
  const UserInfo({
    required this.id,
    required this.email,
    required this.displayName,
  });

  final String id;
  final String email;

  /// 展示名（注册时取邮箱 `@` 前段）。
  final String displayName;

  factory UserInfo.fromJson(JsonMap m) => UserInfo(
        id: readString(m, 'id'),
        email: readString(m, 'email'),
        displayName: readString(m, 'display_name'),
      );

  JsonMap toJson() => <String, Object?>{
        'id': id,
        'email': email,
        'display_name': displayName,
      };

  @override
  String toString() => 'UserInfo($displayName <$email>)';
}

/// 登录成功响应。
final class AuthResponse {
  const AuthResponse({
    required this.protocolVersion,
    required this.token,
    required this.expiresAt,
    required this.user,
    required this.deviceId,
  });

  /// 服务端协议版本。手机端可比对，但**不要**据此拒绝登录：版本不匹配的后果在
  /// 隐藏会话那条路上有专门的判定（`OpenHidden` 超时 + caps 门），不在这里。
  final int protocolVersion;

  /// Bearer token（JWT）。**必须进 Keychain / Keystore**，不是 SharedPreferences：
  /// 服务端 JWT 无 `jti`、无版本号、改密码不失效，默认 TTL 7 天，唯一撤销手段是删设备行。
  final String token;

  /// 到期时刻，**Unix 秒**（不是毫秒）。
  final int expiresAt;

  final UserInfo user;

  /// 服务端分配的本设备 id，**客户端必须持久化**并在下次登录带回去。
  final String deviceId;

  factory AuthResponse.fromJson(JsonMap m) => AuthResponse(
        protocolVersion: readInt(m, 'protocol_version'),
        token: readString(m, 'token'),
        expiresAt: readInt(m, 'expires_at'),
        user: readObject(m, 'user', UserInfo.fromJson),
        deviceId: readString(m, 'device_id'),
      );

  JsonMap toJson() => <String, Object?>{
        'protocol_version': protocolVersion,
        'token': token,
        'expires_at': expiresAt,
        'user': user.toJson(),
        'device_id': deviceId,
      };

  @override
  String toString() =>
      'AuthResponse(v$protocolVersion, token=<已隐藏>, exp=$expiresAt, $user, $deviceId)';
}

/// 续期响应：只有新 token 与新到期时刻，账户与设备信息客户端已有。
final class RefreshResponse {
  const RefreshResponse({required this.token, required this.expiresAt});

  final String token;
  final int expiresAt;

  factory RefreshResponse.fromJson(JsonMap m) => RefreshResponse(
        token: readString(m, 'token'),
        expiresAt: readInt(m, 'expires_at'),
      );

  JsonMap toJson() =>
      <String, Object?>{'token': token, 'expires_at': expiresAt};

  @override
  String toString() => 'RefreshResponse(token=<已隐藏>, exp=$expiresAt)';
}

/// 设备列表项。
final class DeviceRecord {
  const DeviceRecord({
    required this.id,
    required this.name,
    required this.os,
    required this.appVersion,
    required this.online,
    required this.lastSeen,
    required this.isSelf,
  });

  final String id;
  final String name;
  final String os;
  final String appVersion;

  /// 服务端按 `last_seen` 与在线窗口算好的，**客户端不自己判**。
  final bool online;

  /// 最近活跃时刻，Unix 秒。
  final int lastSeen;

  /// 是否为发起请求的本机（不能控制自己 ⇒ 列表里要禁用它的连接按钮）。
  final bool isSelf;

  factory DeviceRecord.fromJson(JsonMap m) => DeviceRecord(
        id: readString(m, 'id'),
        name: readString(m, 'name'),
        os: readString(m, 'os'),
        appVersion: readString(m, 'app_version'),
        online: readBool(m, 'online'),
        lastSeen: readInt(m, 'last_seen'),
        isSelf: readBool(m, 'is_self'),
      );

  JsonMap toJson() => <String, Object?>{
        'id': id,
        'name': name,
        'os': os,
        'app_version': appVersion,
        'online': online,
        'last_seen': lastSeen,
        'is_self': isSelf,
      };

  @override
  String toString() =>
      'DeviceRecord($name/$os, ${online ? "在线" : "离线"}${isSelf ? "/本机" : ""})';
}

/// 设备列表响应。
///
/// ⚠ 服务端按**注册时间升序**返回，这是一次修复的结果：老实现按 `last_seen` 降序，
/// 而 `last_seen` 每次心跳都变，导致列表每刷新一次就重排、设备跳位（海风哥反馈）。
/// **手机端不要再按在线状态重排整表**，那会把这条修复重新引入。
final class DeviceListResponse {
  const DeviceListResponse(this.devices);

  final List<DeviceRecord> devices;

  factory DeviceListResponse.fromJson(JsonMap m) => DeviceListResponse(
        readList(
          m,
          'devices',
          (Object? e) => DeviceRecord.fromJson(asJsonMap(e, '设备项')),
        ),
      );

  JsonMap toJson() => <String, Object?>{
        'devices': devices.map((DeviceRecord d) => d.toJson()).toList(),
      };

  @override
  String toString() => 'DeviceListResponse(${devices.length} 台)';
}

/// HTTP 4xx / 5xx 的统一错误体。
///
/// **按 [`code`] 本地化，不许拿 [`message`] 做字符串匹配**——它是英文散文、随时会改。
/// 未知 code 展示成「服务器返回错误（<code>）」并把原文记日志，
/// **不要**吞成一句「网络错误」（§14-6 无声降级禁令）。
final class ApiError {
  const ApiError({required this.code, required this.message});

  /// 机器可读错误码：`invalid_credentials` / `user_not_found` / `email_taken` …
  final String code;

  /// 人类可读说明（英文）。
  final String message;

  factory ApiError.fromJson(JsonMap m) => ApiError(
        code: readString(m, 'code'),
        message: readString(m, 'message'),
      );

  JsonMap toJson() => <String, Object?>{'code': code, 'message': message};

  @override
  String toString() => 'ApiError($code: $message)';
}
