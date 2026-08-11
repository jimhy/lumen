/// 会话凭据的存放。
///
/// ## 为什么必须是 Keychain / Keystore，不是 SharedPreferences
///
/// 服务端签的 JWT **无 `jti`、无版本号、改密码不失效**，默认 TTL 7 天，唯一的撤销手段是
/// 删掉那台设备行。也就是说：一次泄漏 = 对方拿到一张七天内无法作废的通行证，
/// 而这张通行证配合 `device_pairs` 免码直连，等于对方能免码接管已配对的 PC。
///
/// ## 生物锁与免码的强制绑定（§9.3，**这条是产品规则不是技术细节**）
///
/// 规则：**关掉生物锁 = 清空本机全部信任对**。理由是 `device_pairs` 把「每次都要物理接触
/// 那台 PC」这条不变量废掉了；手机端要提供等强度的替代，唯一可用的就是「物理持有一台已通过
/// 生物识别解锁的手机」。允许「免码开着、生物锁关着」，就是把一个已知会塌陷的组合交给用户。
///
/// > 片 5 只落地存储抽象与内存实现；平台实现（`flutter_secure_storage` + 生物识别包裹）
/// > 与上面那条规则的执行点在片 6 的登录/设置页。**接口先立在这里**，是为了让 rest_client
/// > 的 401 → refresh → retry 现在就能写完并被测到。
library;

/// 一次登录得到的凭据。
final class SessionTokens {
  const SessionTokens({
    required this.token,
    required this.expiresAt,
    required this.deviceId,
    required this.origin,
  });

  /// Bearer token（JWT）。
  final String token;

  /// 到期时刻，**Unix 秒**。
  final int expiresAt;

  /// 服务端分配的本设备 id。**必须持久化**：丢了它下次登录就会长出一台幽灵设备
  /// （`hw_id` 能兜住，但那是第二道防线）。
  final String deviceId;

  /// 这份凭据属于哪台服务器（规范化后的 origin）。
  ///
  /// 存进来是因为凭据是**按 origin 分区**的：同一个 App 换了自建服务器地址之后，
  /// 旧 token 一文不值且 `device_id` 也不通用。不带 origin 就会出现「换服务器后拿旧 token
  /// 打新服务器、收 401、以为是密码错了」。
  final String origin;

  /// 距到期不足 [within] 时视为「该续期了」。
  ///
  /// 提前续期而不是等 401：401 之后的重试要重放整个请求，而 WS 那条根本没有重放机制——
  /// token 在长连接存活期间过期，服务端下次校验直接断，用户看到的是莫名其妙的掉线。
  bool needsRefresh(DateTime now, {Duration within = const Duration(days: 1)}) =>
      now.add(within).millisecondsSinceEpoch ~/ 1000 >= expiresAt;

  @override
  String toString() =>
      'SessionTokens(token=<已隐藏>, exp=$expiresAt, $deviceId @ $origin)';
}

/// 凭据存取。
abstract interface class TokenStore {
  Future<SessionTokens?> read();

  Future<void> write(SessionTokens tokens);

  /// 退出登录 / 收到 [`Unauthorized`] 时清空。
  Future<void> clear();
}

/// 内存实现。单测用，**也用于「未登录」时的空实现**。
final class InMemoryTokenStore implements TokenStore {
  InMemoryTokenStore([this._tokens]);

  SessionTokens? _tokens;

  @override
  Future<SessionTokens?> read() async => _tokens;

  @override
  Future<void> write(SessionTokens tokens) async => _tokens = tokens;

  @override
  Future<void> clear() async => _tokens = null;
}
