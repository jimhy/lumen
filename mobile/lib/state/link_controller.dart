/// 控制面状态机：从「点了某台 PC」到「隐藏会话建立」的全过程。
///
/// ## 显式 sealed，不照抄桌面端的隐式 `Option`
///
/// 桌面端 `remote_ws.rs` 用 4 个 `Option` 字段隐式表达状态（`pairing` / `incoming` /
/// `session` / `reattach`），于是「配对中且已有会话」这种非法组合在类型上是**可表示的**，
/// 只能靠代码自觉。移动端状态少、跃迁全，改成显式 sealed：UI 直接 `switch` 出页面，
/// 漏处理一个状态编译不过。
///
/// ## ★ 三条与蓝图不同、以**服务端代码**为准的事实
///
/// 1. **隐藏会话每次都要念配对码，没有「已配对免码直连」。** 蓝图 §7.5 写的「已配对设备
///    跳过配对码直达 `LinkActive`」对镜像会话成立，对隐藏会话**不成立**：`ws.rs` 的
///    `OpenHidden` 臂恒传 `paired = false`，`hub.rs::submit_pairing` 在 Hidden 分支恒不写
///    `device_pairs`。那是片 4b 的止血——`device_pairs` 没有会话种类列，共用一行信任会让
///    「手机为跟 LLM 说话念的那次码」顺带授出**镜像**权限。
///    所以**不要做「记住这台电脑」的 UI**。
///    （[`_onHiddenStarted`] 仍允许从 `LinkRequesting` 直接跃迁到 `LinkActive`：
///    服务端 `open_hidden` 里那条 `if paired` 分支还在，将来加了 kind 列就会走活。）
/// 2. **断线即会话消失。** 服务端 `disconnect` 会立刻 `teardown_hidden_for_device`，
///    没有重连续挂。所以 WS 一断就回 [`LinkIdle`]，重连后要重新发起（并重新念码）。
///    蓝图里那个 `LinkReattaching` 状态因此**没有实现**——留一个永远到不了的状态，
///    只会让后人以为有重连续挂机制。
/// 3. **主动结束不回执。** 服务端 `teardown_hidden_one` 对主动方 `except`，
///    发 `EndHidden` 的一端收不到 `HiddenSessionEnded`。所以 [`close`] 必须就地清状态。
///
/// ## ★ `OpenHidden` 的 12 秒超时是硬验收项
///
/// 老服务端不认识 `OpenHidden`，整条解析失败后只记一条 debug 就继续读——**完全无回音**；
/// 目标 PC 若是半开连接仍挂在服务端 `peers` 里，同样没有回音。没有这个超时就是无限转圈。
/// 超时文案指向「服务器/PC 版本过低」而**不是**「网络错误」：让用户去重启路由器是最坏的引导。
library;

import 'dart:async';

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/net/scheduler.dart';
import 'package:lumen_mobile/net/ws_client.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';

const String _tag = 'link';

/// `OpenHidden` 发起超时。见文件头「硬验收项」。
const Duration kOpenHiddenTimeout = Duration(seconds: 12);

// ── 状态 ─────────────────────────────────────────────────────────────────────

/// 控制面状态。
sealed class LinkState {
  const LinkState();
}

/// 没有在途请求，也没有会话。
final class LinkIdle extends LinkState {
  const LinkIdle();

  @override
  String toString() => 'LinkIdle';
}

/// 已发出 `OpenHidden`，在等服务端回音（[`kOpenHiddenTimeout`] 内）。
final class LinkRequesting extends LinkState {
  const LinkRequesting(this.target);

  final String target;

  @override
  String toString() => 'LinkRequesting($target)';
}

/// 服务端要求念码。
final class LinkPairing extends LinkState {
  const LinkPairing({
    required this.target,
    required this.targetName,
    required this.expiresInSecs,
    required this.attemptsLeft,
    this.lastError,
  });

  final String target;
  final String targetName;

  /// 服务端给的 TTL。**用它做倒计时，别在客户端写死 120。**
  final int expiresInSecs;

  /// 剩余尝试次数。服务端初值 5，每次输错递减。
  final int attemptsLeft;

  /// 上一次输码失败的原因（首次进入为 null）。
  final PairingFailReason? lastError;

  LinkPairing copyWith({int? attemptsLeft, PairingFailReason? lastError}) =>
      LinkPairing(
        target: target,
        targetName: targetName,
        expiresInSecs: expiresInSecs,
        attemptsLeft: attemptsLeft ?? this.attemptsLeft,
        lastError: lastError ?? this.lastError,
      );

  @override
  String toString() =>
      'LinkPairing($target, left=$attemptsLeft, err=${lastError?.wire})';
}

/// 隐藏会话已建立。
final class LinkActive extends LinkState {
  const LinkActive({
    required this.sessionId,
    required this.peerDeviceId,
    required this.peerName,
    required this.generation,
  });

  /// 后续 `RelayTo` / `EndHidden` 的路由键。
  final int sessionId;

  final String peerDeviceId;
  final String peerName;

  /// 每次建会话自增。**所有在途的数据面请求/应答按它做陈旧丢弃**——对齐桌面端
  /// `advance_edit_generation` 的守卫范式：断线重连后 `sessionId` 会变，但异步回来的
  /// 旧响应不会自己知道这件事。
  final int generation;

  @override
  String toString() =>
      'LinkActive(sid=$sessionId, $peerDeviceId, gen=$generation)';
}

/// 发起被服务端拒绝。[`DenyReason`] 决定文案。
final class LinkDenied extends LinkState {
  const LinkDenied({required this.target, required this.reason});

  final String target;
  final DenyReason reason;

  @override
  String toString() => 'LinkDenied($target, ${reason.wire})';
}

/// 本端判定的失败（超时 / 配对作废 / 链路断）。与 [`LinkDenied`] 分开，
/// 因为那条是**服务端明确回执**，这条没有回执可依——文案与可操作建议都不同。
final class LinkFailed extends LinkState {
  const LinkFailed({required this.target, required this.reason});

  final String target;
  final LinkFailure reason;

  @override
  String toString() => 'LinkFailed($target, $reason)';
}

/// 本端判定的失败原因。
enum LinkFailure {
  /// [`kOpenHiddenTimeout`] 内没有任何回音。**别把它说成「网络错误」**：
  /// 最可能的原因是服务端或那台 PC 的版本过低（老端对新变体完全无回音）。
  serverTooOld,

  /// 配对码过期。
  pairingExpired,

  /// 输错次数用尽，本次配对作废，要重新发起。
  pairingTooManyAttempts,

  /// 服务端说没有对应的未决配对（多半是超时后被清掉了）。
  pairingNoPending,

  /// 建立过程中或会话期间连接断了。服务端会立刻拆掉隐藏会话，重连后要重新发起。
  linkLost,
}

// ── 控制器 ───────────────────────────────────────────────────────────────────

/// 控制面状态机。
///
/// 只做控制面：数据面（LLM 帧）由 [`sessionFrames`] 转出去给会话层（片 7）。
final class LinkController {
  LinkController({
    required WsClient ws,
    Scheduler scheduler = const RealScheduler(),
    this.openTimeout = kOpenHiddenTimeout,
  })  : _ws = ws,
        _openSlot = TimerSlot(scheduler) {
    _inboundSub = ws.inbound.listen(_onMessage);
    _statusSub = ws.statusChanges.listen(_onWsStatus);
  }

  final WsClient _ws;
  final TimerSlot _openSlot;
  final Duration openTimeout;

  late final StreamSubscription<RemoteS2C> _inboundSub;
  late final StreamSubscription<WsStatus> _statusSub;

  final StreamController<LinkState> _states =
      StreamController<LinkState>.broadcast();
  final StreamController<S2CRelayTo> _frames =
      StreamController<S2CRelayTo>.broadcast();

  LinkState _state = const LinkIdle();
  int _generation = 0;

  /// 状态变化。
  Stream<LinkState> get states => _states.stream;

  LinkState get state => _state;

  /// 当前会话的数据面帧（已按 `sessionId` 过滤）。载荷仍是不透明的，由会话层解析。
  Stream<S2CRelayTo> get sessionFrames => _frames.stream;

  /// 当前会话代次。数据面用它丢弃陈旧响应。
  int get generation => _generation;

  /// 发起到 `target` 的隐藏会话。
  ///
  /// 已经在途或已有会话时**拒绝重入**：服务端对同一部手机的第二条会话会回
  /// `ControllerBusy`，本端先拦一道能省一次往返，也避免把 UI 状态覆盖掉。
  bool open(String target) {
    if (_state is LinkRequesting || _state is LinkPairing || _state is LinkActive) {
      logWarn(_tag, '已有在途请求或会话，忽略这次 open($target)');
      return false;
    }
    if (!_ws.send(C2SOpenHidden(target))) {
      // 没连上就发不出去（send 是静默丢弃语义）。不要留在 Requesting 干等 12 秒——
      // 本端已经知道它没发出去了。
      _setState(LinkFailed(target: target, reason: LinkFailure.linkLost));
      return false;
    }
    _setState(LinkRequesting(target));
    _armOpenTimeout(target);
    return true;
  }

  /// 提交用户输入的配对码。只在 [`LinkPairing`] 下有效。
  bool submitCode(String code) {
    final LinkState s = _state;
    if (s is! LinkPairing) {
      logWarn(_tag, '当前不在配对态，忽略 submitCode');
      return false;
    }
    return _ws.send(C2SSubmitPairing(target: s.target, code: code));
  }

  /// 主动结束当前会话。
  ///
  /// **就地清状态**：服务端对主动方不回执（`teardown_hidden_one` 的 `except`），
  /// 等 `HiddenSessionEnded` 是等不到的。
  void close() {
    final LinkState s = _state;
    if (s is LinkActive) {
      _ws.send(C2SEndHidden(s.sessionId));
    }
    _openSlot.cancel();
    _setState(const LinkIdle());
  }

  /// 回到空闲（用户点掉错误提示）。
  void dismissError() {
    if (_state is LinkDenied || _state is LinkFailed) {
      _setState(const LinkIdle());
    }
  }

  Future<void> dispose() async {
    _openSlot.cancel();
    await _inboundSub.cancel();
    await _statusSub.cancel();
    await _states.close();
    await _frames.close();
  }

  // ── 内部 ───────────────────────────────────────────────────────────────────

  void _armOpenTimeout(String target) {
    _openSlot.set(openTimeout, () {
      // 走到这里说明服务端**一个字都没回**。老服务端不认识 OpenHidden（整条解析失败后
      // 继续读），或目标 PC 是半开连接仍挂在 peers 里——两种都没有任何错误回执可等。
      logWarn(_tag, 'OpenHidden ${openTimeout.inSeconds}s 无回音，判定对端版本过低');
      _setState(LinkFailed(target: target, reason: LinkFailure.serverTooOld));
    });
  }

  void _onWsStatus(WsStatus status) {
    if (status == WsStatus.connected) return;
    // 断线：服务端会立刻拆掉本设备参与的全部隐藏会话，没有重连续挂。
    final LinkState s = _state;
    final String? target = switch (s) {
      LinkRequesting(:final String target) => target,
      LinkPairing(:final String target) => target,
      LinkActive(:final String peerDeviceId) => peerDeviceId,
      LinkIdle() || LinkDenied() || LinkFailed() => null,
    };
    if (target == null) return;
    _openSlot.cancel();
    logWarn(_tag, '连接断开，会话已失效（服务端会立刻拆除），回到空闲');
    _setState(LinkFailed(target: target, reason: LinkFailure.linkLost));
  }

  void _onMessage(RemoteS2C msg) {
    switch (msg) {
      case S2CPairingNeeded():
        _onPairingNeeded(msg);
      case S2CPairingResult():
        _onPairingResult(msg);
      case S2CControlDenied():
        _onDenied(msg);
      case S2CHiddenSessionStarted():
        _onHiddenStarted(msg);
      case S2CHiddenSessionEnded():
        _onHiddenEnded(msg);
      case S2CRelayTo():
        _onRelayTo(msg);
      // 以下是发给**被控端**或镜像控制端的，手机端正常收不到。
      // 记 warn 后丢弃——「正常收不到」是对服务端当前行为的假设，不是协议保证。
      case S2CWelcome():
        logInfo(_tag, '服务端确认本机为 ${msg.deviceId}');
      case S2CSessionStarted():
        logError(_tag, '收到镜像会话 SessionStarted——手机端不该发起镜像会话，发起路径被改错了');
      case S2CControlRequested() ||
            S2CHiddenControlRequested() ||
            S2CPairingCancelled() ||
            S2CSessionEnded() ||
            S2CRelay():
        logWarn(_tag, '收到面向被控端 / 镜像端的 ${msg.variantName}，已丢弃');
      case S2CPong():
        // WsClient 就地消费，正常到不了这里。
        break;
    }
  }

  void _onPairingNeeded(S2CPairingNeeded msg) {
    final LinkState s = _state;
    if (s is! LinkRequesting || s.target != msg.targetDeviceId) {
      logWarn(_tag, '收到与当前状态不符的 PairingNeeded（${msg.targetDeviceId}），已丢弃');
      return;
    }
    // 服务端已经回音了，发起超时的使命结束；之后的时限由配对码 TTL 接管。
    _openSlot.cancel();
    _setState(LinkPairing(
      target: msg.targetDeviceId,
      targetName: msg.targetName,
      expiresInSecs: msg.expiresInSecs,
      // 服务端不下发初始次数，只在失败时回剩余次数。这个 5 与 hub.rs 的
      // PAIRING_MAX_ATTEMPTS 同源，改那边要改这里。
      attemptsLeft: 5,
    ));
  }

  void _onPairingResult(S2CPairingResult msg) {
    final LinkState s = _state;
    if (s is! LinkPairing) {
      logWarn(_tag, '当前不在配对态，丢弃 PairingResult');
      return;
    }
    // attempts_left == 0 表示这次配对已**作废**，不是「还能试 0 次」：
    // UI 必须退回去重新发起，留在输码页只会让用户对着一个已经不存在的配对继续输。
    if (msg.attemptsLeft == 0) {
      _setState(LinkFailed(target: s.target, reason: _failureOf(msg.reason)));
      return;
    }
    _setState(s.copyWith(attemptsLeft: msg.attemptsLeft, lastError: msg.reason));
  }

  LinkFailure _failureOf(PairingFailReason reason) => switch (reason) {
        PairingFailReason.expired => LinkFailure.pairingExpired,
        PairingFailReason.tooManyAttempts => LinkFailure.pairingTooManyAttempts,
        PairingFailReason.noPending => LinkFailure.pairingNoPending,
        // 码错但次数归零：对用户而言就是「试完了」。
        PairingFailReason.invalidCode => LinkFailure.pairingTooManyAttempts,
      };

  void _onDenied(S2CControlDenied msg) {
    final LinkState s = _state;
    final String? current = switch (s) {
      LinkRequesting(:final String target) => target,
      LinkPairing(:final String target) => target,
      LinkIdle() || LinkActive() || LinkDenied() || LinkFailed() => null,
    };
    if (current != msg.targetDeviceId) {
      logWarn(_tag, '收到与当前目标无关的 ControlDenied（${msg.targetDeviceId}），已丢弃');
      return;
    }
    _openSlot.cancel();
    _setState(LinkDenied(target: msg.targetDeviceId, reason: msg.reason));
  }

  void _onHiddenStarted(S2CHiddenSessionStarted msg) {
    _openSlot.cancel();
    if (_state is! LinkRequesting && _state is! LinkPairing) {
      // 服务端建了会话而本端不在等 —— **仍然接受**：拒绝的话，那条会话会在服务端一直挂着，
      // 占着目标 PC 的隐藏名额（上限 2），而没有任何一端会去拆它。
      logWarn(_tag, '在非等待态收到 HiddenSessionStarted，仍接受以免会话泄漏');
    }
    _generation++;
    _setState(LinkActive(
      sessionId: msg.sessionId,
      peerDeviceId: msg.peerDeviceId,
      peerName: msg.peerName,
      generation: _generation,
    ));
  }

  void _onHiddenEnded(S2CHiddenSessionEnded msg) {
    final LinkState s = _state;
    if (s is! LinkActive || s.sessionId != msg.sessionId) {
      logWarn(_tag, '收到非当前会话的 HiddenSessionEnded（sid=${msg.sessionId}），已丢弃');
      return;
    }
    logInfo(_tag, '会话 ${msg.sessionId} 结束：${msg.reason.wire}');
    _setState(const LinkIdle());
  }

  void _onRelayTo(S2CRelayTo msg) {
    final LinkState s = _state;
    if (s is! LinkActive || s.sessionId != msg.sessionId) {
      // 服务端已做成员校验，这里再按 sessionId 过一道：一部手机同时只有一条会话，
      // 对不上就是陈旧帧（上一条会话的尾巴），喂给数据面会污染当前对话。
      logWarn(_tag, '收到非当前会话的数据面帧（sid=${msg.sessionId}），已丢弃');
      return;
    }
    _frames.add(msg);
  }

  void _setState(LinkState next) {
    _state = next;
    if (!_states.isClosed) _states.add(next);
  }
}
