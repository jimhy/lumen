/// 环境配置：服务端地址与它的规范化。**不放任何密钥。**
///
/// ## 为什么 origin 要有一套严格的规范化，而不是拿用户输入直接拼 URL
///
/// 两条理由，第二条是硬的：
///
/// 1. 用户会输入 `example.com` / `https://example.com/` / `https://EXAMPLE.com:443`，
///    它们是同一台服务器，但**当成三个字符串会得到三个不同的 `hw_id`**
///    （`hw_id = sha256(canonical_origin ‖ 0x00 ‖ 原始机器标识)`）⇒ 同一台手机在服务端
///    分裂成三台幽灵设备。规范化是 `hw_id` 稳定的前提。
/// 2. 这套规则要与桌面端 `cloud.rs::canonical_server_origin` **逐字节一致**，否则同一个
///    用户在 PC 与手机上输入同一个地址，两端算出的 origin 不同——将来任何按 origin 分区的
///    东西（凭据存储、`hw_id`）都会静默错位。
///
/// 拒绝而不是「尽力修正」的那几种输入（带路径 / 带查询串 / 带用户名密码 / 非 http(s)），
/// 都是「用户以为自己填的是服务器地址，其实填了别的东西」的信号——静默截断会让他连上一个
/// 他没打算连的地方。
library;

/// 服务端地址不合法。
///
/// 这是**用户输入错误**，不是异常情况，所以走 `Result` 而不是抛出；本类型只是错误负载。
final class InvalidServerOrigin {
  const InvalidServerOrigin(this.reason);

  /// 中文原因，可直接展示。
  final String reason;

  @override
  String toString() => '服务器地址不合法：$reason';
}

/// 把用户输入规范化成 `scheme://host[:port]`。
///
/// 与桌面端 `cloud.rs::canonical_server_origin` 同规则：
///
/// - 不带 scheme 时补 `https://`（**不是** `http://`）；
/// - scheme 与 host 小写；
/// - 去掉默认端口（http:80 / https:443）；
/// - 只允许根路径（`""` 或 `"/"`），其余一律拒绝；
/// - 拒绝用户名 / 密码 / 查询串 / 片段 / 控制字符 / 反斜杠 / 尾点域名。
///
/// 返回 null 表示不合法（调用方包成 [`InvalidServerOrigin`]）。
String? canonicalServerOrigin(String raw) {
  if (raw.codeUnits.any((int c) => c < 0x20 || c == 0x7f)) return null;
  final String input = raw.trim();
  if (input.isEmpty) return null;
  // 反斜杠：桌面端把它当 authority 终止符后判定「有多余后缀」而拒绝。Dart 的 Uri 会把它
  // 当普通路径字符，两端行为就分叉了——显式拒掉，别指望两套 URL 解析器口径一致。
  if (input.contains(r'\')) return null;

  final String candidate = input.contains('://') ? input : 'https://$input';
  // authority 以 ':' 结尾（`https://host:`）：Uri 会把它解析成 port 缺省，桌面端则拒绝。
  final int schemeEnd = candidate.indexOf('://') + 3;
  final String remainder = candidate.substring(schemeEnd);
  final int authorityEnd = _firstIndexOfAny(remainder, const <String>['/', '?', '#']);
  final String authority =
      authorityEnd < 0 ? remainder : remainder.substring(0, authorityEnd);
  final String suffix =
      authorityEnd < 0 ? '' : remainder.substring(authorityEnd);
  if (authority.contains('@') || authority.endsWith(':')) return null;
  if (suffix != '' && suffix != '/') return null;

  final Uri? uri = Uri.tryParse(candidate);
  if (uri == null) return null;
  final String scheme = uri.scheme.toLowerCase();
  if (scheme != 'http' && scheme != 'https') return null;
  if (uri.userInfo.isNotEmpty) return null;
  if (uri.hasQuery || uri.hasFragment) return null;
  if (uri.path != '' && uri.path != '/') return null;
  final String host = uri.host.toLowerCase();
  if (host.isEmpty) return null;
  // 尾点域名（`example.com.`）与不带尾点的是同一台机器，但字符串不同 ⇒ 两个 hw_id。
  if (host.endsWith('.')) return null;

  final int defaultPort = scheme == 'http' ? 80 : 443;
  final bool explicitNonDefault = uri.hasPort && uri.port != defaultPort;
  // IPv6 字面量：Uri.host 不带方括号，拼回去必须补上，否则 `http://::1:8080` 无法再解析。
  final String hostPart = host.contains(':') ? '[$host]' : host;
  return explicitNonDefault
      ? '$scheme://$hostPart:${uri.port}'
      : '$scheme://$hostPart';
}

int _firstIndexOfAny(String s, List<String> needles) {
  int best = -1;
  for (final String n in needles) {
    final int i = s.indexOf(n);
    if (i >= 0 && (best < 0 || i < best)) best = i;
  }
  return best;
}

/// 一台服务器的连接参数（REST base 与 WS URL 都从同一个 origin 派生）。
final class ServerEndpoint {
  const ServerEndpoint._(this.origin);

  /// 规范化后的 origin，形如 `https://lumen.example.com`。
  final String origin;

  /// 从用户输入构造。失败返回 null。
  static ServerEndpoint? tryParse(String raw) {
    final String? origin = canonicalServerOrigin(raw);
    return origin == null ? null : ServerEndpoint._(origin);
  }

  /// 明文 HTTP（非本地回环）。**调用方要么拒绝、要么显著告警**——
  /// token 走 `Authorization` 头，明文链路上等于裸奔。
  bool get isInsecure =>
      origin.startsWith('http://') &&
      !origin.startsWith('http://127.0.0.1') &&
      !origin.startsWith('http://localhost') &&
      !origin.startsWith('http://[::1]');

  /// REST 基址（不带尾斜杠，路径常量自带前导斜杠）。
  String get restBase => origin;

  /// WebSocket 地址：`http→ws` / `https→wss`，路径固定 `/api/v1/ws`。
  String get wsUrl {
    final String ws = origin.startsWith('https://')
        ? origin.replaceFirst('https://', 'wss://')
        : origin.replaceFirst('http://', 'ws://');
    return '$ws/api/v1/ws';
  }

  @override
  bool operator ==(Object other) =>
      other is ServerEndpoint && other.origin == origin;

  @override
  int get hashCode => origin.hashCode;

  @override
  String toString() => 'ServerEndpoint($origin)';
}
