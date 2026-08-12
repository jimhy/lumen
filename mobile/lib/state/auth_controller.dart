/// 登录态。
///
/// ## 「未注册就自动注册」**不做成静默的**
///
/// 分片表里写的是「404 自动注册」，这里落成**提示确认后再注册**，理由有两条，
/// 第二条是硬的：
///
/// 1. 用户不知道自己刚建了一个账号——而这个账号将来是他所有设备的信任根；
/// 2. **打错一个字母就会凭空建出一个垃圾账号**，而且他会以为自己登进了老账号，
///    然后困惑于「我的设备呢」。邮箱输入框没有任何拼写校验能挡住这个。
///
/// 多点一次「注册」的成本，换掉的是一类无法自助恢复的困惑。
///
/// ## 凭据按 origin 分区
///
/// 换服务器要清掉旧凭据：旧 token 打新服务器必然 401，而那会被用户读成「密码错了」。
library;

import 'dart:async';
import 'dart:io' show Platform;

import 'package:lumen_mobile/core/env.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/core/result.dart';
import 'package:lumen_mobile/data/device_identity.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/net/net_error.dart';
import 'package:lumen_mobile/net/rest_client.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';

const String _tag = 'auth';

/// 本端 App 版本，随 `pubspec.yaml` 的 `version` 手工对齐。
///
/// 刻意不引 `package_info_plus`：它走 platform channel，会让登录这条路径在
/// `flutter test` 里跑不起来，而这里要的只是一个上报给服务端的展示字符串。
const String kAppVersion = '0.1.0';

/// 设备显示名的来源。
///
/// 抽成接口的理由与 [`RawMachineIdSource`] 相同：`device_info_plus` 走 platform channel，
/// 在 `flutter test` 的 Dart VM 里不可用。
abstract interface class DeviceNameSource {
  Future<String> read();
}

/// 固定名字（测试与「读不到」两种场景）。
final class StaticDeviceName implements DeviceNameSource {
  const StaticDeviceName(this.value);

  final String value;

  @override
  Future<String> read() async => value;
}

// ── 状态 ─────────────────────────────────────────────────────────────────────

/// 登录态。
sealed class AuthState {
  const AuthState();
}

/// 未登录。
final class AuthLoggedOut extends AuthState {
  const AuthLoggedOut();

  @override
  String toString() => 'AuthLoggedOut';
}

/// 正在登录 / 注册。
final class AuthBusy extends AuthState {
  const AuthBusy();

  @override
  String toString() => 'AuthBusy';
}

/// 已登录。
final class AuthLoggedIn extends AuthState {
  const AuthLoggedIn({required this.user, required this.deviceId});

  final UserInfo user;

  /// 本机在服务端的设备 id。设备列表用它标出「本机」，也是 `SelfControl` 的判据。
  final String deviceId;

  @override
  String toString() => 'AuthLoggedIn(${user.displayName}, $deviceId)';
}

/// 登录失败。
final class AuthFailed extends AuthState {
  const AuthFailed(this.error);

  final NetError error;

  /// 服务端说这个邮箱没注册过——UI 据此显示「去注册」的引导，而不是笼统的报错。
  bool get isUserNotFound =>
      error is ApiFailure && (error as ApiFailure).body.code == 'user_not_found';

  @override
  String toString() => 'AuthFailed($error)';
}

// ── 控制器 ───────────────────────────────────────────────────────────────────

/// 登录 / 注册 / 登出。
final class AuthController {
  AuthController({
    required RestClient rest,
    required TokenStore tokens,
    required DeviceIdentity identity,
    required DeviceNameSource deviceName,
    required this.endpoint,
  })  : _rest = rest,
        _tokens = tokens,
        _identity = identity,
        _deviceName = deviceName;

  final RestClient _rest;
  final TokenStore _tokens;
  final DeviceIdentity _identity;
  final DeviceNameSource _deviceName;

  /// 本次登录针对的服务器。
  final ServerEndpoint endpoint;

  final StreamController<AuthState> _states =
      StreamController<AuthState>.broadcast();

  AuthState _state = const AuthLoggedOut();

  Stream<AuthState> get states => _states.stream;

  AuthState get state => _state;

  /// 启动时尝试恢复登录态。
  ///
  /// **只认属于当前 origin 的凭据**：换过服务器之后本地那份是另一台服务器的，
  /// 拿它去打新服务器只会收 401，而用户会把它读成「密码错了」。
  Future<void> restore() async {
    final SessionTokens? saved = await _tokens.read();
    if (saved == null) return;
    if (saved.origin != endpoint.origin) {
      logInfo(_tag, '本地凭据属于另一台服务器，已忽略（不清除，换回去还能用）');
      return;
    }
    // 顺手续一次期：token 在 WS 长连接存活期间过期的表现是「莫名其妙掉线」，
    // 而 WS 没有 REST 那样的重放机制。
    await _rest.refreshIfNeeded();
    _emit(AuthLoggedIn(
      // 恢复路径拿不到 UserInfo（服务端只在登录时回传），用邮箱占位——
      // 它只用于展示，真正的身份判据是 token 与 deviceId。
      // 恢复路径拿不到邮箱与展示名（服务端只在登录时回传 UserInfo），但 **id 必须有**
      // ——扫码校验要用它算账户指纹。id 存在 SessionTokens 里，见那边的注释。
      user: UserInfo(id: saved.userId, email: '', displayName: ''),
      deviceId: saved.deviceId,
    ));
  }

  /// 登录。
  Future<void> login({required String email, required String password}) async {
    _emit(const AuthBusy());
    final DeviceInfo device = await _describeThisDevice();
    final Result<AuthResponse, NetError> r =
        await _rest.login(email: email, password: password, device: device);
    _emit(switch (r) {
      Ok<AuthResponse, NetError>(:final AuthResponse value) =>
        AuthLoggedIn(user: value.user, deviceId: value.deviceId),
      Err<AuthResponse, NetError>(:final NetError error) => AuthFailed(error),
    });
  }

  /// 注册**并**登录。
  ///
  /// 服务端的 `register` 不登记设备、不回 token，所以注册完必须再走一次登录才拿得到
  /// `device_id`——两步之间任何一步失败都要如实报出来，不能让用户看到「注册成功」
  /// 却停在登录页。
  Future<void> registerThenLogin({
    required String email,
    required String password,
  }) async {
    _emit(const AuthBusy());
    final Result<UserInfo, NetError> reg =
        await _rest.register(email: email, password: password);
    if (reg case Err<UserInfo, NetError>(:final NetError error)) {
      _emit(AuthFailed(error));
      return;
    }
    await login(email: email, password: password);
  }

  /// 登出：清凭据回到未登录。
  Future<void> logout() async {
    await _tokens.clear();
    _emit(const AuthLoggedOut());
  }

  /// 收到 [`Unauthorized`] 时调用：凭据已被 `RestClient` 清掉，这里只同步状态。
  void markSessionExpired() {
    logWarn(_tag, '登录已失效，退回登录页');
    _emit(const AuthFailed(Unauthorized()));
  }

  Future<void> dispose() => _states.close();

  /// 组装上报给服务端的设备信息。
  Future<DeviceInfo> _describeThisDevice() async {
    final SessionTokens? saved = await _tokens.read();
    return DeviceInfo(
      // 只在 origin 相同时才带上旧的 device_id——带着另一台服务器的 id 去登录，
      // 服务端查不到就会 INSERT 一台新设备（hw_id 能兜住，但那是第二道防线）。
      deviceId: saved?.origin == endpoint.origin ? saved?.deviceId : null,
      hwId: await _identity.hwIdFor(endpoint.origin),
      name: await _deviceName.read(),
      os: Platform.isIOS ? 'ios' : 'android',
      appVersion: kAppVersion,
    );
  }

  void _emit(AuthState next) {
    _state = next;
    if (!_states.isClosed) _states.add(next);
  }
}
