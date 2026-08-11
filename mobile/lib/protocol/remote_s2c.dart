/// 控制面**下行**消息（服务端 → 手机）——**零 Flutter 依赖**。
///
/// 线格式与 `remote_c2s.dart` 同款（externally tagged，unit 变体是裸字符串 `"Pong"`），
/// 但**覆盖要求相反：这一层的 14 个变体一个都不能少**。
///
/// ## 为什么接收侧必须全实现
///
/// 外部标签枚举没有 `#[serde(other)]`，收到一条没实现的变体就是**整条消息**解析失败。
/// 其中有 5 条是「手机理论上收不到」的镜像会话消息（[`S2CSessionStarted`] /
/// [`S2CSessionEnded`] / [`S2CRelay`] / [`S2CControlRequested`] / [`S2CPairingCancelled`]），
/// 照样实现，理由是：
///
/// - 「理论上收不到」是对服务端**当前行为**的假设，不是协议保证。`DeviceKind`（「手机不可
///   被控」）是 P1 才做的软策略，P0 的服务端并不拦；
/// - 解出来记一条日志丢掉，成本是几十行；假设它不会来，代价是一条**静默失效**的连接
///   ——而这条连接上还跑着心跳，用户看到的是「界面一切正常但什么都不动」。
///
/// 收到这 5 条时正确的动作是 `log` **并且**丢弃，不是无声无息（§14-6 无声降级禁令）。
///
/// ## 载荷刻意保持不透明
///
/// [`S2CRelayTo`] 与 [`S2CRelay`] 的 `payload` 是 `Object?` 而不是 `RemoteFrame`——
/// 与上行那侧（`C2SRelayTo.payload` 是强类型）不对称，这是刻意的：
///
/// - **发送侧**用强类型，本端发得出去的东西必须是本端构造的、类型认得的；
/// - **接收侧**保持不透明，让「数据面某一帧解析失败」被隔离在数据面。若在这里就解成
///   `RemoteFrame`，PC 端将来加一个本端不认识的外层变体，就会把**整条控制面消息**打掉
///   ——连带 `session_id` 一起丢，上层连「哪条会话出的事」都不知道。
library;

import 'package:lumen_mobile/protocol/codec.dart';

// ── 四个封闭式 C 型枚举（没有 Other 兜底，取值全集见 enums_control_plane.json）──

/// 会话中本端扮演的角色。
enum RemoteRole implements WireNamed {
  /// 控制端（手机在隐藏会话里恒为此角色）。
  controller('Controller'),

  /// 被控端。
  controlled('Controlled');

  const RemoteRole(this.wire);

  @override
  final String wire;
}

/// 发起被拒 / 未决配对被取消的原因。
///
/// ⚠ **刻意零新增变体**：它随 [`S2CControlDenied`] 下行，加变体会让老客户端整条解析失败，
/// 丢掉的正是唯一的拒绝回执。数据面加变体有 5 秒握手超时兜底，控制面**没有任何兜底**。
enum DenyReason implements WireNamed {
  /// 目标不在线——**或者版本过低**。服务端在目标未上报 `hidden` 能力位时复用这条拒因
  /// （新增拒因会打掉老客户端），所以手机端文案要覆盖两种情况：
  /// 「请确认那台电脑的 Lumen 正在运行、已登录，且版本不低于 v1.1」。
  offline('Offline'),

  /// 该**类**会话的名额已满。隐藏桶的上限是 2，与镜像桶相互独立
  /// （桌面镜像开着时手机仍可接入）。**文案必须给出具体数字**——只说「已被占用」
  /// 会让用户去找一个并不存在的占用者。
  alreadyControlled('AlreadyControlled'),

  /// 发起方自己已占着该类会话的名额（本手机已连着一台 PC，或已有未决配对）。
  controllerBusy('ControllerBusy'),

  /// 目标正在与他人配对中。**这是唯一一处跨桶互斥且有意保留**：配对码是给人念的，
  /// 同一时刻 PC 屏幕上只该出现一个码。文案是「该电脑正在与另一台设备配对，请稍候」，
  /// 不是「已被占用」。
  targetPairing('TargetPairing'),

  /// 目标属于其它账户。
  crossUser('CrossUser'),

  /// 不能控制自己。
  selfControl('SelfControl'),

  /// 被控端用户主动拒绝。
  rejectedByUser('RejectedByUser'),

  /// 发起方在配对完成前断线 / 撤销。
  controllerLeft('ControllerLeft'),

  /// 配对超时。
  expired('Expired'),

  /// 配对码连续输错次数超限。
  tooManyAttempts('TooManyAttempts');

  const DenyReason(this.wire);

  @override
  final String wire;
}

/// 配对码校验失败的原因（[`S2CPairingResult`] 用）。
///
/// ⚠ 与 [`DenyReason`] 有两个**重名**取值（`Expired` / `TooManyAttempts`），但语义不同：
/// 这里说的是「这次输码失败了」，那边说的是「这次发起被拒了」。两套解析绝不能合并。
enum PairingFailReason implements WireNamed {
  /// 码错（还有剩余次数）。
  invalidCode('InvalidCode'),

  /// 配对已超时失效。
  expired('Expired'),

  /// 无对应的未决配对（已被取消 / 完成 / 从未发起）。
  ///
  /// 服务端在「提交者不是当初发起请求的那台设备」时也回这条——刻意不区分，
  /// 免得向无关方泄漏「某个配对存在与否」。
  noPending('NoPending'),

  /// 错误次数超限，配对作废。
  tooManyAttempts('TooManyAttempts');

  const PairingFailReason(this.wire);

  @override
  final String wire;
}

/// 会话结束的原因。
enum EndReason implements WireNamed {
  /// 对端主动结束。
  peerLeft('PeerLeft'),

  /// 对端断线。
  peerDisconnected('PeerDisconnected'),

  /// 本设备在别处重新登录，当前连接被顶替。
  replaced('Replaced');

  const EndReason(this.wire);

  @override
  final String wire;
}

// ── 消息 ─────────────────────────────────────────────────────────────────────

/// 服务端发给手机的控制面消息。
sealed class RemoteS2C {
  const RemoteS2C();

  /// 线格式。**注意返回类型是 `Object?`**：unit 变体序列化成裸字符串。
  Object? toJson();

  /// Rust 侧的变体名（golden 语料的 `expect_variant` 就是它）。
  String get variantName;

  /// 本端实现了的变体名。**必须是 Rust 侧的全集**，覆盖断言拿它与语料对账。
  static const Set<String> implementedVariants = <String>{
    'Welcome',
    'ControlRequested',
    'PairingNeeded',
    'PairingResult',
    'ControlDenied',
    'PairingCancelled',
    'SessionStarted',
    'SessionEnded',
    'Relay',
    'Pong',
    'HiddenControlRequested',
    'HiddenSessionStarted',
    'HiddenSessionEnded',
    'RelayTo',
  };

  static RemoteS2C fromJson(Object? json) {
    if (json is String) {
      if (json == 'Pong') return const S2CPong();
      throw WireFormatException('RemoteS2C 未知的 unit 变体 "$json"');
    }
    final JsonMap m = asJsonMap(json, 'RemoteS2C');
    if (m.length != 1) {
      throw WireFormatException(
        'RemoteS2C 外部标签枚举期望恰好一个键，实际有 ${m.length} 个',
      );
    }
    final String tag = m.keys.first;
    final Object? body = m[tag];
    return switch (tag) {
      'Welcome' => S2CWelcome.fromJson(asJsonMap(body, 'Welcome')),
      'ControlRequested' =>
        S2CControlRequested.fromJson(asJsonMap(body, 'ControlRequested')),
      'PairingNeeded' =>
        S2CPairingNeeded.fromJson(asJsonMap(body, 'PairingNeeded')),
      'PairingResult' =>
        S2CPairingResult.fromJson(asJsonMap(body, 'PairingResult')),
      'ControlDenied' =>
        S2CControlDenied.fromJson(asJsonMap(body, 'ControlDenied')),
      'PairingCancelled' =>
        S2CPairingCancelled.fromJson(asJsonMap(body, 'PairingCancelled')),
      'SessionStarted' =>
        S2CSessionStarted.fromJson(asJsonMap(body, 'SessionStarted')),
      'SessionEnded' => S2CSessionEnded.fromJson(asJsonMap(body, 'SessionEnded')),
      // newtype 变体：body **就是**载荷本身，不是一层字段对象。
      'Relay' => S2CRelay(body),
      'HiddenControlRequested' => S2CHiddenControlRequested.fromJson(
          asJsonMap(body, 'HiddenControlRequested')),
      'HiddenSessionStarted' => S2CHiddenSessionStarted.fromJson(
          asJsonMap(body, 'HiddenSessionStarted')),
      'HiddenSessionEnded' =>
        S2CHiddenSessionEnded.fromJson(asJsonMap(body, 'HiddenSessionEnded')),
      'RelayTo' => S2CRelayTo.fromJson(asJsonMap(body, 'RelayTo')),
      _ => throw WireFormatException(
          'RemoteS2C 变体 "$tag" 本端不认识（服务端比本端新？）——'
          '外部标签枚举没有 other 兜底，这条消息整条丢失',
        ),
    };
  }
}

/// 连上后服务端**立即**下发的第一条消息。
///
/// `minSupportedVersion` 自 v4 起**不再等于** `protocolVersion`（LLM 是纯增量能力，
/// 不该把 v3 桌面端挡在配对前）。两个数字别接反。
final class S2CWelcome extends RemoteS2C {
  const S2CWelcome({
    required this.protocolVersion,
    required this.minSupportedVersion,
    required this.deviceId,
  });

  final int protocolVersion;
  final int minSupportedVersion;

  /// 服务端据 JWT 确认的**本机**设备 id。
  final String deviceId;

  factory S2CWelcome.fromJson(JsonMap m) => S2CWelcome(
        protocolVersion: readInt(m, 'protocol_version'),
        minSupportedVersion: readInt(m, 'min_supported_version'),
        deviceId: readString(m, 'device_id'),
      );

  @override
  String get variantName => 'Welcome';

  @override
  JsonMap toJson() => <String, Object?>{
        'Welcome': <String, Object?>{
          'protocol_version': protocolVersion,
          'min_supported_version': minSupportedVersion,
          'device_id': deviceId,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.Welcome(v$protocolVersion, min=$minSupportedVersion, $deviceId)';
}

/// 发给**被控端**：有桌面控制端请求**镜像**控制你。手机端不做被控端，解出来记日志后丢弃。
final class S2CControlRequested extends RemoteS2C {
  const S2CControlRequested({
    required this.controllerDeviceId,
    required this.controllerName,
    required this.pairingCode,
    required this.expiresInSecs,
  });

  final String controllerDeviceId;
  final String controllerName;
  final String pairingCode;
  final int expiresInSecs;

  factory S2CControlRequested.fromJson(JsonMap m) => S2CControlRequested(
        controllerDeviceId: readString(m, 'controller_device_id'),
        controllerName: readString(m, 'controller_name'),
        pairingCode: readString(m, 'pairing_code'),
        expiresInSecs: readInt(m, 'expires_in_secs'),
      );

  @override
  String get variantName => 'ControlRequested';

  @override
  JsonMap toJson() => <String, Object?>{
        'ControlRequested': <String, Object?>{
          'controller_device_id': controllerDeviceId,
          'controller_name': controllerName,
          'pairing_code': pairingCode,
          'expires_in_secs': expiresInSecs,
        },
      };

  /// 配对码不进调试渲染。
  @override
  String toString() =>
      'RemoteS2C.ControlRequested($controllerDeviceId, code=<9 位>)';
}

/// 发给**发起方**（手机）：请求已送达，请输入对方屏幕上展示的码。
///
/// 隐藏会话刻意**复用**这条而不是新增变体——它只说「请输入码」，与会话种类无关。
/// `expiresInSecs` 是服务端给的 TTL，倒计时用它，别在客户端写死 120。
final class S2CPairingNeeded extends RemoteS2C {
  const S2CPairingNeeded({
    required this.targetDeviceId,
    required this.targetName,
    required this.expiresInSecs,
  });

  final String targetDeviceId;
  final String targetName;
  final int expiresInSecs;

  factory S2CPairingNeeded.fromJson(JsonMap m) => S2CPairingNeeded(
        targetDeviceId: readString(m, 'target_device_id'),
        targetName: readString(m, 'target_name'),
        expiresInSecs: readInt(m, 'expires_in_secs'),
      );

  @override
  String get variantName => 'PairingNeeded';

  @override
  JsonMap toJson() => <String, Object?>{
        'PairingNeeded': <String, Object?>{
          'target_device_id': targetDeviceId,
          'target_name': targetName,
          'expires_in_secs': expiresInSecs,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.PairingNeeded($targetDeviceId, ${expiresInSecs}s)';
}

/// 配对码校验结果，**只在失败时下发**（成功走 [`S2CHiddenSessionStarted`]）。
///
/// `attemptsLeft == 0` 表示这次配对已作废、要重新发起 `OpenHidden`，
/// 不是「还能再试 0 次」那么轻——UI 要退回设备列表而不是继续等用户输码。
final class S2CPairingResult extends RemoteS2C {
  const S2CPairingResult({required this.reason, required this.attemptsLeft});

  final PairingFailReason reason;
  final int attemptsLeft;

  factory S2CPairingResult.fromJson(JsonMap m) => S2CPairingResult(
        reason: readClosedEnum(
          m,
          'reason',
          PairingFailReason.values,
          'PairingFailReason',
        ),
        attemptsLeft: readInt(m, 'attempts_left'),
      );

  @override
  String get variantName => 'PairingResult';

  @override
  JsonMap toJson() => <String, Object?>{
        'PairingResult': <String, Object?>{
          'reason': reason.wire,
          'attempts_left': attemptsLeft,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.PairingResult(${reason.wire}, left=$attemptsLeft)';
}

/// 发起被拒的**唯一**回执。收不到它就只能靠超时。
final class S2CControlDenied extends RemoteS2C {
  const S2CControlDenied({required this.targetDeviceId, required this.reason});

  final String targetDeviceId;
  final DenyReason reason;

  factory S2CControlDenied.fromJson(JsonMap m) => S2CControlDenied(
        targetDeviceId: readString(m, 'target_device_id'),
        reason: readClosedEnum(m, 'reason', DenyReason.values, 'DenyReason'),
      );

  @override
  String get variantName => 'ControlDenied';

  @override
  JsonMap toJson() => <String, Object?>{
        'ControlDenied': <String, Object?>{
          'target_device_id': targetDeviceId,
          'reason': reason.wire,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.ControlDenied($targetDeviceId, ${reason.wire})';
}

/// 发给**被控端**：未决配对被取消，dismiss 那个码。手机端解出来记日志后丢弃。
///
/// ⚠ 它带的是 [`DenyReason`] 而不是 [`PairingFailReason`]。
final class S2CPairingCancelled extends RemoteS2C {
  const S2CPairingCancelled(this.reason);

  final DenyReason reason;

  factory S2CPairingCancelled.fromJson(JsonMap m) => S2CPairingCancelled(
        readClosedEnum(m, 'reason', DenyReason.values, 'DenyReason'),
      );

  @override
  String get variantName => 'PairingCancelled';

  @override
  JsonMap toJson() => <String, Object?>{
        'PairingCancelled': <String, Object?>{'reason': reason.wire},
      };

  @override
  String toString() => 'RemoteS2C.PairingCancelled(${reason.wire})';
}

/// **镜像**会话已建立。手机永远不该走到这条（它发的是 `OpenHidden` 不是 `RequestControl`）
/// ——真收到就说明发起路径被改错了，必须**告警**而不是无声丢弃。
///
/// 与 [`S2CHiddenSessionStarted`] 只差一个 `session_id` 字段。两条臂**绝不允许合并处理**：
/// 被控端处理这一条时会无条件重置镜像态，合并的后果是「手机一连上，桌面镜像立刻黑屏」。
final class S2CSessionStarted extends RemoteS2C {
  const S2CSessionStarted({
    required this.peerDeviceId,
    required this.peerName,
    required this.role,
  });

  final String peerDeviceId;
  final String peerName;
  final RemoteRole role;

  factory S2CSessionStarted.fromJson(JsonMap m) => S2CSessionStarted(
        peerDeviceId: readString(m, 'peer_device_id'),
        peerName: readString(m, 'peer_name'),
        role: readClosedEnum(m, 'role', RemoteRole.values, 'Role'),
      );

  @override
  String get variantName => 'SessionStarted';

  @override
  JsonMap toJson() => <String, Object?>{
        'SessionStarted': <String, Object?>{
          'peer_device_id': peerDeviceId,
          'peer_name': peerName,
          'role': role.wire,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.SessionStarted($peerDeviceId, ${role.wire})';
}

/// **镜像**会话结束。与隐藏会话的 [`S2CHiddenSessionEnded`] 是两条独立的臂
/// （那条带 `session_id`）；合并处理会让一条镜像会话结束把手机自己的隐藏会话一起清掉。
final class S2CSessionEnded extends RemoteS2C {
  const S2CSessionEnded(this.reason);

  final EndReason reason;

  factory S2CSessionEnded.fromJson(JsonMap m) =>
      S2CSessionEnded(readClosedEnum(m, 'reason', EndReason.values, 'EndReason'));

  @override
  String get variantName => 'SessionEnded';

  @override
  JsonMap toJson() => <String, Object?>{
        'SessionEnded': <String, Object?>{'reason': reason.wire},
      };

  @override
  String toString() => 'RemoteS2C.SessionEnded(${reason.wire})';
}

/// **镜像**会话的数据面来帧。newtype 变体：线上是 `{"Relay": <载荷>}`，
/// 载荷**直接就是**外层 `RemoteFrame` 信封，外面没有再套一层字段名。
///
/// 手机不镜像终端画面，收到只丢弃；但载荷必须原样往返，解析时不窥探、不改写。
final class S2CRelay extends RemoteS2C {
  const S2CRelay(this.payload);

  /// 不透明载荷。**刻意不解析**（见本文件头部「载荷刻意保持不透明」）。
  final Object? payload;

  @override
  String get variantName => 'Relay';

  @override
  JsonMap toJson() => <String, Object?>{'Relay': payload};

  /// 载荷内容不进调试渲染：那是别人的终端画面字节。
  @override
  String toString() => 'RemoteS2C.Relay(<不透明载荷>)';
}

/// 心跳应答。unit 变体，线上是**裸字符串**。
///
/// 移动端在它上面挂了一条桌面端没有的 **Pong 看门狗**：15 秒收不到就主动断开重连。
/// 服务端既不发 Ping 也没有读超时，而移动网络的半开连接极常见——不自己判就会长期挂在
/// 一条死连接上。
final class S2CPong extends RemoteS2C {
  const S2CPong();

  @override
  String get variantName => 'Pong';

  @override
  String toJson() => 'Pong';

  @override
  String toString() => 'RemoteS2C.Pong';
}

/// 发给**被控端 PC**：有手机请求建立隐藏会话，展示配对码。手机端解出来记日志后丢弃。
///
/// 刻意不复用 [`S2CControlRequested`]：复用的话老 PC 会展示一个码、念完之后收不到它认识的
/// `SessionStarted`（服务端发的是 `HiddenSessionStarted`，老端整条丢弃）⇒ 横幅永久残留，
/// 用户点「拒绝」还会得到「我拒绝成功了」的错觉而会话早已建立——那是安全误导。
final class S2CHiddenControlRequested extends RemoteS2C {
  const S2CHiddenControlRequested({
    required this.controllerDeviceId,
    required this.controllerName,
    required this.pairingCode,
    required this.expiresInSecs,
  });

  final String controllerDeviceId;
  final String controllerName;
  final String pairingCode;
  final int expiresInSecs;

  factory S2CHiddenControlRequested.fromJson(JsonMap m) =>
      S2CHiddenControlRequested(
        controllerDeviceId: readString(m, 'controller_device_id'),
        controllerName: readString(m, 'controller_name'),
        pairingCode: readString(m, 'pairing_code'),
        expiresInSecs: readInt(m, 'expires_in_secs'),
      );

  @override
  String get variantName => 'HiddenControlRequested';

  @override
  JsonMap toJson() => <String, Object?>{
        'HiddenControlRequested': <String, Object?>{
          'controller_device_id': controllerDeviceId,
          'controller_name': controllerName,
          'pairing_code': pairingCode,
          'expires_in_secs': expiresInSecs,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.HiddenControlRequested($controllerDeviceId, code=<9 位>)';
}

/// 隐藏会话已建立。手机在这条里拿到 `sessionId`——后续 `RelayTo` / `EndHidden` 的路由键。
///
/// 手机侧 `role` 恒为 [`RemoteRole.controller`]。
final class S2CHiddenSessionStarted extends RemoteS2C {
  const S2CHiddenSessionStarted({
    required this.sessionId,
    required this.peerDeviceId,
    required this.peerName,
    required this.role,
  });

  final int sessionId;
  final String peerDeviceId;
  final String peerName;
  final RemoteRole role;

  factory S2CHiddenSessionStarted.fromJson(JsonMap m) =>
      S2CHiddenSessionStarted(
        sessionId: readInt(m, 'session_id'),
        peerDeviceId: readString(m, 'peer_device_id'),
        peerName: readString(m, 'peer_name'),
        role: readClosedEnum(m, 'role', RemoteRole.values, 'Role'),
      );

  @override
  String get variantName => 'HiddenSessionStarted';

  @override
  JsonMap toJson() => <String, Object?>{
        'HiddenSessionStarted': <String, Object?>{
          'session_id': sessionId,
          'peer_device_id': peerDeviceId,
          'peer_name': peerName,
          'role': role.wire,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.HiddenSessionStarted(sid=$sessionId, $peerDeviceId, ${role.wire})';
}

/// 隐藏会话结束。**只有仍在线的那一端收得到**：主动发 `EndHidden` 的一端不回执，
/// 断线的一端已被移出服务端的 `peers`。所以本端主动结束时要就地清状态，别等这条回来。
final class S2CHiddenSessionEnded extends RemoteS2C {
  const S2CHiddenSessionEnded({required this.sessionId, required this.reason});

  final int sessionId;
  final EndReason reason;

  factory S2CHiddenSessionEnded.fromJson(JsonMap m) => S2CHiddenSessionEnded(
        sessionId: readInt(m, 'session_id'),
        reason: readClosedEnum(m, 'reason', EndReason.values, 'EndReason'),
      );

  @override
  String get variantName => 'HiddenSessionEnded';

  @override
  JsonMap toJson() => <String, Object?>{
        'HiddenSessionEnded': <String, Object?>{
          'session_id': sessionId,
          'reason': reason.wire,
        },
      };

  @override
  String toString() =>
      'RemoteS2C.HiddenSessionEnded(sid=$sessionId, ${reason.wire})';
}

/// 隐藏会话数据面来帧。
///
/// `payload` 不透明（见本文件头部）：上层拿到后自己 `RemoteFrame.fromJson` 并单独兜异常，
/// 这样一帧解析失败只丢那一帧，`sessionId` 还在，上层至少知道是哪条会话出的事。
final class S2CRelayTo extends RemoteS2C {
  const S2CRelayTo({required this.sessionId, required this.payload});

  final int sessionId;

  /// 外层 `RemoteFrame` 信封那一层的原文（`{"Llm": {"op": …}}`）。
  final Object? payload;

  factory S2CRelayTo.fromJson(JsonMap m) => S2CRelayTo(
        sessionId: readInt(m, 'session_id'),
        payload: m['payload'],
      );

  @override
  String get variantName => 'RelayTo';

  @override
  JsonMap toJson() => <String, Object?>{
        'RelayTo': <String, Object?>{
          'session_id': sessionId,
          'payload': payload,
        },
      };

  /// 载荷内容不进调试渲染：那里面是对话正文。
  @override
  String toString() => 'RemoteS2C.RelayTo(sid=$sessionId, <不透明载荷>)';
}
