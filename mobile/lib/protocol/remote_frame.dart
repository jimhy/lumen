/// 外层 `RemoteFrame` 信封——**externally tagged**，与内层 `LlmFrame` 的
/// **internally tagged** 是两套机制，别用同一个解析器。
///
/// ```text
/// 外层：{"Llm": {"op": "Detach", ...}}     变体名当唯一的 key
/// 外层："P2pStreamHello"                   unit 变体是**裸 JSON 字符串**，不是 {"X":{}}
/// 内层：       {"op": "Detach", ...}       标签是 body 里的一个字段
/// ```
///
/// 把 `op` 直接摊到 Relay 载荷顶层是 Dart 侧最容易犯的错；假设「载荷一定是 object」
/// 则会在心跳（unit 变体）上直接炸。
///
/// ## 只实现移动端用得到的子集
///
/// Rust 侧 `RemoteFrame` 有 69 个变体，绝大多数是终端镜像用的——移动端不镜像终端画面，
/// 那些变体一个都用不上。这里只实现 [RemoteLlm] 与四个 unit 变体，其余一律解析失败。
///
/// ## 未知变体**必须**报错，不许兜底
///
/// 这一层是 serde 默认的外部标签枚举，而外部标签枚举**不支持** `#[serde(other)]`
/// ——Rust 侧遇到未知变体就是整帧 `Err`，Dart 侧宽容成一个「未知信封」反而两端不一致。
/// 内层 `LlmFrame` 才有 `Unknown` 兜底，前向兼容的代价只在那一层付。
library;

import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';

/// 无载荷的变体。线上是**裸字符串**。
enum RemoteUnitKind implements WireNamed {
  ping('Ping'),
  pong('Pong'),
  endSession('EndSession'),
  p2pStreamHello('P2pStreamHello');

  const RemoteUnitKind(this.wire);

  @override
  final String wire;
}

/// 远程控制协议的外层帧（移动端子集）。
sealed class RemoteFrame {
  const RemoteFrame();

  /// 注意返回类型是 `Object?` 而不是 `JsonMap`：unit 变体序列化成裸字符串。
  Object? toJson();

  static RemoteFrame fromJson(Object? json) {
    if (json is String) {
      for (final RemoteUnitKind k in RemoteUnitKind.values) {
        if (k.wire == json) return RemoteUnit(k);
      }
      throw WireFormatException('RemoteFrame 未知的 unit 变体 "$json"');
    }
    final JsonMap m = asJsonMap(json, 'RemoteFrame');
    if (m.length != 1) {
      throw WireFormatException(
        'RemoteFrame 外部标签枚举期望恰好一个键，实际有 ${m.length} 个',
      );
    }
    final String tag = m.keys.first;
    if (tag == 'Llm') return RemoteLlm(LlmFrame.fromJson(m['Llm']));
    throw WireFormatException('RemoteFrame 变体 "$tag" 未在移动端实现');
  }
}

/// LLM 对话子协议的信封。服务端对它**盲转**（不反序列化内层）。
final class RemoteLlm extends RemoteFrame {
  const RemoteLlm(this.frame);

  final LlmFrame frame;

  @override
  JsonMap toJson() => <String, Object?>{'Llm': frame.toJson()};

  @override
  String toString() => 'RemoteFrame.Llm($frame)';
}

/// 无载荷变体（心跳等）。
final class RemoteUnit extends RemoteFrame {
  const RemoteUnit(this.kind);

  final RemoteUnitKind kind;

  @override
  String toJson() => kind.wire;

  @override
  String toString() => 'RemoteFrame.${kind.wire}';
}
