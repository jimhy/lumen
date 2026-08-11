/// 控制面**上行**消息（手机 → 服务端）——**零 Flutter 依赖**。
///
/// 线格式是 serde 默认的 **externally tagged**，与 `remote_frame.dart` 那一层同款：
///
/// ```text
/// 结构体变体：{"OpenHidden": {"target": "pc-1"}}
/// unit 变体： "Ping"                            ← 裸 JSON 字符串，不是 {"Ping":{}}
/// ```
///
/// ## 只实现手机**会发**的 6 个变体，这是安全决策不是省事
///
/// Rust 侧 `RemoteC2S` 有 10 个变体，另外 4 个是桌面控制端与被控端用的。它们不实现，
/// 手机在类型层面就发不出去——比在文档里写一句「不要发」硬得多：
///
/// | 不实现的变体 | 手机发出去会怎样 |
/// |---|---|
/// | `RequestControl` | 建的是**镜像**会话，凭空占住那台 PC 的镜像独占位。更糟的是**老服务端会照常执行成功**（它认识这个变体），两端都察觉不到。手机要的是 [`C2SOpenHidden`]。 |
/// | `EndSession` | 拆的是本设备唯一的**镜像**会话；手机拆隐藏会话用带 id 的 [`C2SEndHidden`]（一台 PC 上可能挂着多条）。 |
/// | `Relay` | 镜像数据面盲转，按「发送者是谁」路由，在一台 PC 挂多条隐藏会话时结构性失效。手机走带 `session_id` 的 [`C2SRelayTo`]。 |
/// | `DeclineControl` | 被控端拒绝未决配对用的。手机端 P0 不做被控端。 |
///
/// 这份清单在 Rust 侧有个对应物：`mobile_golden.rs` 的 `手机不发的C2S` 常量，
/// 覆盖断言要求「有语料的 ∪ 刻意不发的 == 全集」，两边一起改才过得去。
///
/// ## 未知变体报错，不兜底
///
/// 外部标签枚举**不支持** `#[serde(other)]`，Rust 侧遇到未知变体就是整条 `from_str` 失败。
/// 这一层宽容成「未知消息」反而与 Rust 不一致。前向兼容的代价只在内层 `LlmFrame` 付。
library;

import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/remote_frame.dart';

/// 本端实现的远程控制协议版本（对齐 Rust 的 `PROTOCOL_VERSION`）。
///
/// 只用在 [`C2SClientHello`] 上报，**不做版本门**：服务端零版本校验，两端在协议层也看不到
/// 对端版本。真正的版本不匹配靠两条兜底判定——`OpenHidden` 的 12 秒发起超时（老服务端）
/// 与服务端的 caps 门（老 PC，回 `Offline`）。
///
/// `c2s_client_hello.json` 语料里写的就是这个数字，测试会把两者对上。
const int kLumenProtocolVersion = 4;

/// 手机发给服务端的控制面消息。
sealed class RemoteC2S {
  const RemoteC2S();

  /// 线格式。**注意返回类型是 `Object?`**：unit 变体序列化成裸字符串。
  Object? toJson();

  /// Rust 侧的变体名（golden 语料的 `expect_variant` 就是它）。
  String get variantName;

  /// 本端实现了的变体名。覆盖断言拿它与语料对账，少一个或多一个都红。
  static const Set<String> implementedVariants = <String>{
    'ClientHello',
    'OpenHidden',
    'SubmitPairing',
    'RelayTo',
    'EndHidden',
    'Ping',
  };

  /// 解码。**手机端其实不需要它**（C2S 是本端发出去的），存在只为让 golden 语料能做
  /// 双向断言——只测编码的话，一个把 `target` 写进错误键名的模型照样「往返一致」。
  static RemoteC2S fromJson(Object? json) {
    if (json is String) {
      if (json == 'Ping') return const C2SPing();
      throw WireFormatException('RemoteC2S 未实现的 unit 变体 "$json"');
    }
    final JsonMap m = asJsonMap(json, 'RemoteC2S');
    if (m.length != 1) {
      throw WireFormatException(
        'RemoteC2S 外部标签枚举期望恰好一个键，实际有 ${m.length} 个',
      );
    }
    final String tag = m.keys.first;
    final Object? body = m[tag];
    return switch (tag) {
      'ClientHello' => C2SClientHello.fromJson(asJsonMap(body, 'ClientHello')),
      'OpenHidden' => C2SOpenHidden.fromJson(asJsonMap(body, 'OpenHidden')),
      'SubmitPairing' =>
        C2SSubmitPairing.fromJson(asJsonMap(body, 'SubmitPairing')),
      'RelayTo' => C2SRelayTo.fromJson(asJsonMap(body, 'RelayTo')),
      'EndHidden' => C2SEndHidden.fromJson(asJsonMap(body, 'EndHidden')),
      _ => throw WireFormatException(
          'RemoteC2S 变体 "$tag" 未在移动端实现——手机只发 6 种，'
          '其余是桌面端 / 被控端的（理由见本文件头部那张表）',
        ),
    };
  }
}

/// 连上后的**第一条**消息：上报协议版本与能力位。
///
/// `caps` 恒为空：`"hidden"` 表示「本端可承载隐藏会话」，那是 PC 的能力，
/// 手机是发起方，报了就是谎报。服务端据这个字段判定**目标 PC** 是否支持隐藏会话。
///
/// ⚠ 这是**控制面**握手（服务端消费）。数据面另有一个 `LlmFrame.hello`（对端 PC 消费，
/// 走 RelayTo 载荷）。两者层次不同、消费者不同，别把两层能力位搅在一起。
final class C2SClientHello extends RemoteC2S {
  const C2SClientHello({required this.protocolVersion, this.caps = const []});

  final int protocolVersion;
  final List<String> caps;

  factory C2SClientHello.fromJson(JsonMap m) => C2SClientHello(
        protocolVersion: readInt(m, 'protocol_version'),
        caps: readList(m, 'caps', asWireString),
      );

  @override
  String get variantName => 'ClientHello';

  @override
  JsonMap toJson() => <String, Object?>{
        'ClientHello': <String, Object?>{
          'protocol_version': protocolVersion,
          'caps': caps,
        },
      };

  @override
  String toString() => 'RemoteC2S.ClientHello(v$protocolVersion, caps=$caps)';
}

/// 发起一条到 `target` 的**隐藏会话**。
///
/// ★ **必须自带 12 秒发起超时**（`link_controller.dart` 实现，那是片 5/6 的硬验收项）：
/// 老服务端不认识这个变体，整条解析失败后只记一条 debug 就继续读，**完全无回音**；
/// 目标 PC 若是半开连接仍挂在服务端的 `peers` 里，同样没有任何回音。不自己超时就是无限转圈。
final class C2SOpenHidden extends RemoteC2S {
  const C2SOpenHidden(this.target);

  /// 目标（被控端）设备 id。
  final String target;

  factory C2SOpenHidden.fromJson(JsonMap m) =>
      C2SOpenHidden(readString(m, 'target'));

  @override
  String get variantName => 'OpenHidden';

  @override
  JsonMap toJson() => <String, Object?>{
        'OpenHidden': <String, Object?>{'target': target},
      };

  @override
  String toString() => 'RemoteC2S.OpenHidden($target)';
}

/// 提交 PC 屏幕上展示的 9 位配对码。
///
/// **隐藏会话每次都要念码**：服务端 `ws.rs` 的 `OpenHidden` 臂恒传 `paired = false`，
/// `hub.rs` 的 `submit_pairing` 在 Hidden 分支恒不写 `device_pairs`。这不是遗漏，是在堵
/// 「镜像信任与隐藏信任串用」这条静默提权路径（`device_pairs` 没有会话种类列）。
/// 所以手机端**不要**做「记住这台电脑、下次免码」的 UI。
final class C2SSubmitPairing extends RemoteC2S {
  const C2SSubmitPairing({required this.target, required this.code});

  final String target;

  /// 配对码。**是字符串不是数字**——它可能带前导零。
  final String code;

  factory C2SSubmitPairing.fromJson(JsonMap m) => C2SSubmitPairing(
        target: readString(m, 'target'),
        code: readString(m, 'code'),
      );

  @override
  String get variantName => 'SubmitPairing';

  @override
  JsonMap toJson() => <String, Object?>{
        'SubmitPairing': <String, Object?>{'target': target, 'code': code},
      };

  /// 配对码不进调试渲染：它是一次性的口令，日志里留一份没有任何用处。
  @override
  String toString() => 'RemoteC2S.SubmitPairing($target, code=<9 位>)';
}

/// 隐藏会话数据面盲转。
///
/// `sessionId` 在**信封**上：一台 PC 可同时挂多条隐藏会话，光凭「发送者是谁」已无法定位
/// 对端——这正是镜像那条 `Relay` 的路由方式在多会话下结构性失效之处。
///
/// 载荷用强类型 [`RemoteFrame`] 而不是裸 JSON，是**发送侧**的刻意选择：手机发得出去的东西
/// 必须是本端构造的、类型认得的。接收侧相反（见 `remote_s2c.dart` 的 `S2CRelayTo`），
/// 那边保持不透明，好让一帧解析失败不至于打断整条控制面。
final class C2SRelayTo extends RemoteC2S {
  const C2SRelayTo({required this.sessionId, required this.payload});

  final int sessionId;
  final RemoteFrame payload;

  factory C2SRelayTo.fromJson(JsonMap m) => C2SRelayTo(
        sessionId: readInt(m, 'session_id'),
        payload: RemoteFrame.fromJson(m['payload']),
      );

  @override
  String get variantName => 'RelayTo';

  @override
  JsonMap toJson() => <String, Object?>{
        'RelayTo': <String, Object?>{
          'session_id': sessionId,
          'payload': payload.toJson(),
        },
      };

  @override
  String toString() => 'RemoteC2S.RelayTo(sid=$sessionId, $payload)';
}

/// 主动结束一条隐藏会话。
///
/// 发起拆除的一端**不收回执**（服务端只通知另一端），所以本端必须就地清状态，
/// 不能等 `HiddenSessionEnded` 回来才动。
final class C2SEndHidden extends RemoteC2S {
  const C2SEndHidden(this.sessionId);

  final int sessionId;

  factory C2SEndHidden.fromJson(JsonMap m) =>
      C2SEndHidden(readInt(m, 'session_id'));

  @override
  String get variantName => 'EndHidden';

  @override
  JsonMap toJson() => <String, Object?>{
        'EndHidden': <String, Object?>{'session_id': sessionId},
      };

  @override
  String toString() => 'RemoteC2S.EndHidden(sid=$sessionId)';
}

/// 应用层心跳。25 秒一次，对齐服务端的 25 秒 `last_seen` 节流与 45 秒在线窗口。
///
/// unit 变体，线上是**裸字符串** `"Ping"`。这是唯一每 25 秒必发的消息，
/// 编码成 `{"Ping":{}}` 会让整条链路静默失效（服务端解析失败后只记 debug 继续读）。
final class C2SPing extends RemoteC2S {
  const C2SPing();

  @override
  String get variantName => 'Ping';

  @override
  String toJson() => 'Ping';

  @override
  String toString() => 'RemoteC2S.Ping';
}
