/// 把控制面的隐藏会话接到对话归约器上。
///
/// 它只做**两次拆包与两次装包**，没有业务逻辑——但这四步每一步都有一个容易踩的坑：
///
/// ```text
/// 收：S2CRelayTo.payload（不透明 JSON）
///       → RemoteFrame.fromJson   ← 外层 externally tagged
///       → RemoteLlm.frame        ← 内层 internally tagged（"op"）
///       → ConversationController.applyFrame
///
/// 发：LlmFrame
///       → RemoteLlm 包一层
///       → C2SRelayTo{sessionId, payload}
///       → WsClient.send
/// ```
///
/// ## ★ 解包失败只丢那一帧
///
/// `RemoteFrame.fromJson` 对未实现的外层变体会**抛异常**（外部标签枚举没有 other 兜底）。
/// 这里必须逐帧 try/catch：一帧解不出就丢一帧，`sessionId` 还在、订阅还在。
/// 让它冒出去会连整条订阅一起断掉——那是把一个显示问题升级成了会话中断。
///
/// ## ★ 发不出去要说出来
///
/// `WsClient.send` 在无连接时**静默丢弃**（与桌面端同语义）。归约器发的 `Attach` /
/// `TurnFetch` 都是**自愈**用的，丢了就等于自愈没发生、而本地水位线还停在缺口前。
/// 所以这里把 `send` 的返回值原样透传给归约器，并在失败时留一条 warn。
library;

import 'dart:async';

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/net/scheduler.dart';
import 'package:lumen_mobile/net/ws_client.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_frame.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/state/link_controller.dart';

const String _tag = 'conv-session';

/// 一条隐藏会话上的一个对话。
final class ConversationSession {
  ConversationSession({
    required LinkController link,
    required WsClient ws,
    required this.convId,
    required this.sessionId,
    Scheduler scheduler = const RealScheduler(),
  })  : _ws = ws,
        controller = ConversationController(
          convId: convId,
          send: (LlmFrame frame) => _sendVia(ws, sessionId, frame),
          scheduler: scheduler,
        ) {
    _sub = link.sessionFrames.listen(_onRelay);
  }

  final int convId;

  /// 承载本对话的隐藏会话 id（`RelayTo` 的路由键）。
  final int sessionId;

  final WsClient _ws;

  /// 归约器。UI 订阅它的 [ConversationController.snapshots] 与
  /// [ConversationController.tail]。
  final ConversationController controller;

  late final StreamSubscription<S2CRelayTo> _sub;

  /// 主动发一帧（发消息 / 中断 / 拉历史都走它）。
  bool send(LlmFrame frame) => _sendVia(_ws, sessionId, frame);

  Future<void> dispose() async {
    await _sub.cancel();
    await controller.dispose();
  }

  void _onRelay(S2CRelayTo msg) {
    if (msg.sessionId != sessionId) return; // LinkController 已过滤，这里再兜一道。
    final LlmFrame? frame = _unwrap(msg.payload);
    if (frame == null) return;
    controller.applyFrame(frame);
  }

  /// 两层拆包。任何一层失败都只丢这一帧。
  LlmFrame? _unwrap(Object? payload) {
    final RemoteFrame outer;
    try {
      outer = RemoteFrame.fromJson(payload);
    } on Object catch (e) {
      // 外层没有 other 兜底，未实现的变体就是整条报废——只丢这一帧，别断订阅。
      logWarn(_tag, '数据面外层信封解析失败，已丢弃这一帧', error: e);
      return null;
    }
    if (outer is! RemoteLlm) {
      logWarn(_tag, '隐藏会话上收到非 LLM 数据面帧（${outer.runtimeType}），已丢弃');
      return null;
    }
    return outer.frame;
  }

  static bool _sendVia(WsClient ws, int sessionId, LlmFrame frame) {
    final bool ok = ws.send(C2SRelayTo(
      sessionId: sessionId,
      payload: RemoteLlm(frame),
    ));
    if (!ok) {
      // 归约器发的是自愈帧（Attach / TurnFetch），丢了就等于自愈没发生。
      logWarn(_tag, '没连上，${frame.variantName} 被丢弃（自愈未发生）');
    }
    return ok;
  }
}
