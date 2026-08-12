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
///
/// > 片 12 补上平台实现（`SecureTokenStore`，见 `secure_token_store.dart`）。
/// > **生物识别包裹仍然没做，而且这不是遗漏**：§9.3 那条规则的前提是「免码直连」，
/// > 而片 4b 的止血让隐藏会话**每次都要念码**（`ws.rs` 的 `OpenHidden` 臂恒传
/// > `paired = false`）。前提不成立时加生物锁只是给每次冷启动加一道指纹，
/// > 换不来任何安全增量。等 `device_pairs` 有了会话种类列、免码真的活过来，
/// > 那条规则连同生物锁一起补——**到时候要连着看这两处，别只加锁不加规则**。
library;

import 'package:lumen_mobile/protocol/codec.dart';

/// 本机存储格式的版本号。
///
/// 存储格式与**线格式无关**：它只有本机自己读写，没有对端要对账。留版本号是为了
/// 让将来的字段变更有一条明确的「读不懂就当没登录」的退路——见
/// [SessionTokens.fromStorageJson] 的注释。
const int kSessionTokensStorageVersion = 1;

/// 一次登录得到的凭据。
final class SessionTokens {
  const SessionTokens({
    required this.token,
    required this.expiresAt,
    required this.deviceId,
    required this.origin,
    required this.userId,
  });

  /// Bearer token（JWT）。
  final String token;

  /// 到期时刻，**Unix 秒**。
  final int expiresAt;

  /// 服务端分配的本设备 id。**必须持久化**：丢了它下次登录就会长出一台幽灵设备
  /// （`hw_id` 能兜住，但那是第二道防线）。
  final String deviceId;

  /// 账户 id。
  ///
  /// 存它是为了**扫码校验**：二维码里的 `u` 是 sha256(user_id) 的前 16 位，
  /// 手机端要算出同一个值才比得了。服务端只在**登录**时回传 UserInfo，
  /// 冷启动恢复登录态那条路径拿不到它——不存下来，重启 App 之后扫码就会一律
  /// 判成 ForeignAccount，而那看起来像「扫码功能坏了」。
  final String userId;

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

  /// 落盘用的形状。**不是线格式**，故意不叫 `toJson`。
  ///
  /// 名字里带 `Storage` 是一道防线：这个仓库里凡是 `toJson`/`fromJson` 都有 Rust 侧的
  /// 对端与 golden 语料钉着，而这一份只有本机自己读——两者混用会让后人以为改字段名
  /// 要去同步 Rust，或者反过来以为这份能随便改。
  JsonMap toStorageJson() => <String, Object?>{
        'v': kSessionTokensStorageVersion,
        'token': token,
        'exp': expiresAt,
        'device_id': deviceId,
        'origin': origin,
        'user_id': userId,
      };

  /// 从落盘形状还原。
  ///
  /// 版本不认识、字段缺失、类型不对，一律抛 [WireFormatException]——**调用方必须把它
  /// 当成「本机没有凭据」**（退回登录页），不能让它冒泡到启动路径。理由：这份数据在
  /// 安全存储里，损坏的现实原因是 Keystore 出过问题或跨版本升级，此时唯一能让用户
  /// 自助的动作就是重新登录；抛到 UI 只会变成一个打不开的 App。
  factory SessionTokens.fromStorageJson(JsonMap m) {
    final int version = readInt(m, 'v');
    if (version != kSessionTokensStorageVersion) {
      // **向前不兼容是刻意的**：老版本 App 读到新格式，只应退回登录页，
      // 绝不能猜字段——猜错的表现是「拿着半份凭据去打服务端」，比重登难查得多。
      throw WireFormatException(
        '凭据存储格式版本 $version 不是本端认识的 $kSessionTokensStorageVersion',
      );
    }
    return SessionTokens(
      token: readString(m, 'token'),
      expiresAt: readInt(m, 'exp'),
      deviceId: readString(m, 'device_id'),
      origin: readString(m, 'origin'),
      userId: readString(m, 'user_id'),
    );
  }

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

  /// 凭据是否**只活在内存里**。
  ///
  /// ## 为什么这条属于接口，而不是平台实现的内部细节
  ///
  /// 协作铁律 6（无声降级禁令）：凡是降级路径，必须 `log` **且 UI 可见**。
  /// 安全存储写失败之后 App 完全能继续用——直到用户杀掉进程，然后发现自己被登出了。
  /// 那正是「功能看起来在、其实没生效」的形态。放进接口，UI 就能无条件地问一句
  /// 「这份凭据存住了吗」，不必对具体实现做类型判断。
  bool get degraded;

  /// [degraded] 的变化。**只在真的变化时发**（false → true）。
  ///
  /// 与 `LinkController` 同款「当前值 + 变化流」两件套：横幅要在页面**建起来之后**
  /// 也能显示，只有流会漏掉建页之前发生的那次降级。
  Stream<bool> get degradedChanges;
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

  /// 恒 `false`：它**不是降级**，是刻意选择的测试替身。
  ///
  /// 报 `true` 会让每个用内存实现跑的 widget 测试都顶着一条降级横幅，
  /// 那条横幅就此变成噪音，真出降级时没人看得见。
  @override
  bool get degraded => false;

  @override
  Stream<bool> get degradedChanges => const Stream<bool>.empty();
}
