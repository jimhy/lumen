/// [`TokenStore`] 的真机实现：Keychain（iOS）/ Keystore 包裹密钥的加密存储（Android）。
///
/// ⚠ Android 那半**不是** EncryptedSharedPreferences：`flutter_secure_storage` 从 v10 起
/// 弃用了 Jetpack Security 那条路（Google 自己弃用了它），改成自管的 AES-GCM 密文 +
/// Keystore 里的包裹密钥。旧数据由插件在首次访问时自动迁移。
///
/// ## 为什么它不是 `providers.dart` 里的默认值
///
/// `flutter_secure_storage` 走 platform channel，在 `flutter test` 的 Dart VM 里不可用。
/// 设成默认值就等于**全部**纯 Dart 测试都要起 platform channel。所以默认值仍是
/// [`InMemoryTokenStore`]，真机在 `main.dart` 用 `ProviderScope(overrides:)` 换上它。
///
/// ## 这个文件里可测的部分已经搬走了
///
/// 编解码在 [`SessionTokens.toStorageJson`] / [`SessionTokens.fromStorageJson`]（零 Flutter
/// 依赖，`test/data/token_store_test.dart` 钉着）。这里只剩「调 platform channel + 出错怎么办」
/// 这一层——它**没有**纯 Dart 测试能覆盖，所以每一条失败分支都写成了显式代码而不是靠
/// 异常自然冒泡。
///
/// ## 读是热路径，不能每次都问 Keychain
///
/// `rest_client.dart` 的 `_authed` **每发一个请求就 `read()` 一次**。platform channel 往返
/// 虽快也是毫秒级，且 Keychain 在 iOS 上有过「首次访问阻塞几百毫秒」的实测报告。
/// 所以这里在内存里留一份权威副本：冷启动读一次，此后 [read] 直接返回内存值。
///
/// ## 降级：写失败不抛，但必须留痕
///
/// `login()` / `_refresh()` 里的 `await _tokens.write(...)` **一处都没有 try**——写失败
/// 若冒泡出去，用户看到的是登录永远停在转圈。所以这里吞掉异常，改成
/// 「内存里照常可用 + `logError` + [degraded] 置位」，UI 那条横幅由 `devices_page` 显示。
/// 这是协作铁律 6（无声降级禁令）要的形状：**降级本身允许，无声不允许**。
library;

import 'dart:async';
import 'dart:convert';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/protocol/codec.dart';

const String _tag = 'token-store';

/// 安全存储里的键名。
///
/// **改它等于让所有用户登出一次**（老键的值还在，但没人再去读）。真要改格式，
/// 走 [kSessionTokensStorageVersion]，别改键名。
const String _kKey = 'lumen.session';

/// iOS 钥匙串选项。
///
/// - `first_unlock_this_device`：**重启后首次解锁**即可读——WS 断线重连可能发生在
///   用户没点亮屏幕的时候，用 `unlocked` 会让后台重连拿不到 token 而莫名掉线；
///   `this_device` 则挡住 iCloud 钥匙串同步，因为这份凭据里的 `device_id` 是**按设备**
///   发的，同步到另一台设备既无用又平白多一处泄漏面。
/// - `synchronizable: false`：同上，显式写出来而不是靠默认值。
const IOSOptions _iosOptions = IOSOptions(
  accessibility: KeychainAccessibility.first_unlock_this_device,
  synchronizable: false,
);

/// Android 选项。
///
/// 用默认值即可，但有两处是**刻意的**，不是没想过：
///
/// - `resetOnError` 默认 `true`：Keystore 在部分机型上会因系统升级 / 备份还原而损坏
///   （`BadPaddingException` 一类）。保持 `true` 让插件在读到损坏数据时清掉重来，
///   表现是「被登出一次」；改成 `false` 则每次读都抛，表现是「App 打不开」。
/// - **不用 `AndroidOptions.biometric()`**：§9.3 那条「生物锁 ⇄ 免码」的绑定规则，
///   前提是免码直连存在，而片 4b 的止血让隐藏会话每次都要念码。前提不成立时加生物锁
///   只是给每次冷启动加一道指纹，换不来安全增量。等 `device_pairs` 有了会话种类列，
///   这里和那条规则要**一起**补。
const AndroidOptions _androidOptions = AndroidOptions();

/// Keychain / Keystore 实现。
final class SecureTokenStore implements TokenStore {
  SecureTokenStore({FlutterSecureStorage? storage})
      : _storage = storage ??
            const FlutterSecureStorage(
              iOptions: _iosOptions,
              aOptions: _androidOptions,
            );

  final FlutterSecureStorage _storage;

  final StreamController<bool> _degradedChanges =
      StreamController<bool>.broadcast();

  /// 内存里的权威副本。见文件头「读是热路径」。
  SessionTokens? _cached;

  /// 是否已经从安全存储加载过一次。
  ///
  /// 无论那次成功还是失败都置位：Keystore 故障是持久的，每个 HTTP 请求重试一次
  /// 只会把一次故障放大成持续的 platform channel 抖动。
  bool _loaded = false;

  bool _degraded = false;

  @override
  bool get degraded => _degraded;

  @override
  Stream<bool> get degradedChanges => _degradedChanges.stream;

  @override
  Future<SessionTokens?> read() async {
    if (_loaded) return _cached;
    _loaded = true;
    final String? raw;
    try {
      raw = await _storage.read(key: _kKey);
    } on Object catch (e) {
      // 读不出来 = 当作没登录。**不置 degraded**：此时还没有任何凭据要保护，
      // 用户看到的就是登录页，那本身已经是最清楚的提示了。
      logWarn(_tag, '安全存储读取失败，按未登录处理', error: e);
      return null;
    }
    if (raw == null) return null;
    try {
      _cached = SessionTokens.fromStorageJson(
        asJsonMap(jsonDecode(raw), '本机凭据'),
      );
    } on Object catch (e) {
      // 损坏或跨版本读不懂。**顺手删掉**：留着它下次启动还要再失败一次，
      // 而它已经没有任何价值了。
      logWarn(_tag, '本机凭据无法解析，已清除（需重新登录）', error: e);
      unawaited(_deleteQuietly());
      return null;
    }
    return _cached;
  }

  @override
  Future<void> write(SessionTokens tokens) async {
    // ★ 先更新内存副本、**再** await 落盘：`login()` 之后紧跟着就有请求要取 token，
    // 而 await 之间是可以被别的异步任务插进来的。顺序反了就会出现
    // 「刚登录成功、第一个请求却拿到 null」这种只在真机上偶发的怪事。
    _cached = tokens;
    _loaded = true;
    try {
      await _storage.write(key: _kKey, value: jsonEncode(tokens.toStorageJson()));
      _setDegraded(false);
    } on Object catch (e) {
      // 不抛：调用方（`rest_client.login` / `_refresh`）都没有 try，抛出去的表现是
      // 登录永远停在转圈。降级成「本次会话可用、下次冷启动要重登」，并让 UI 说出来。
      logError(_tag, '凭据写入安全存储失败，本次登录只存在于内存中', error: e);
      _setDegraded(true);
    }
  }

  @override
  Future<void> clear() async {
    // 内存先清：即使删除失败，本次会话也必须立刻失去凭据——`_refresh` 失败时调它，
    // 留着旧 token 会让后续每个请求都再走一遍「401 → 续期 → 失败」。
    _cached = null;
    _loaded = true;
    // 登出是「回到干净状态」，把降级标志一并复位，否则登录页会顶着一条上一次会话的横幅。
    _setDegraded(false);
    await _deleteQuietly();
  }

  /// 关闭降级通知流。
  ///
  /// `main.dart` 里这个 store 与 App 同寿，实际不会被调用；留着是为了让它能在
  /// widget 测试里被干净地拆掉。
  Future<void> dispose() => _degradedChanges.close();

  Future<void> _deleteQuietly() async {
    try {
      await _storage.delete(key: _kKey);
    } on Object catch (e) {
      // 删不掉是真的没辙（凭据仍在设备上，但已过期或已被服务端拒）。如实记一条，
      // 不升级成 degraded：degraded 说的是「新凭据存不住」，与这件事不是一回事。
      logWarn(_tag, '清除本机凭据失败', error: e);
    }
  }

  void _setDegraded(bool next) {
    if (_degraded == next) return;
    _degraded = next;
    if (!_degradedChanges.isClosed) _degradedChanges.add(next);
  }
}
