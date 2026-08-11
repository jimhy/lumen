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
///
/// ## ★ 启动顺序是硬的（片 10）
///
/// [start] 里那三步**不能换序**：
///
/// ```text
/// ① restore()      从库恢复历史 + 水位线 + 未确认的消息
/// ② 订阅 + attach() 用①恢复出来的水位线请求补齐
/// ③ flushOutbox()   重发未确认的（带原 clientMsgId，被控端去重）
/// ```
///
/// - ①②换序 ⇒ `knownSeq` 从 0 开始补，被控端要么整段重发、要么回 `resync_required`
///   （用户看到历史被整个重建）；
/// - ②③换序 ⇒ 先重发再补齐，补齐流里那条 `TurnStarted` 回带确认会晚于重发到达，
///   于是每次重连都白发一遍（功能正确、纯属浪费，且被控端日志刷一片「丢弃重复的 Send」）。
///
/// **订阅刻意放在 `restore()` 之后**：早订阅的话，恢复出来的历史会去和已经应用的在线帧
/// 抢同一个状态——归约器里那道「已经有 meta 就跳过恢复」的保护只能兜住最坏情况，
/// 靠它不如根本不制造竞争。
library;

import 'dart:async';

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/net/scheduler.dart';
import 'package:lumen_mobile/net/ws_client.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_frame.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/outbox.dart';

const String _tag = 'conv-session';

/// 一条隐藏会话上的一个对话。
final class ConversationSession {
  ConversationSession({
    required LinkController link,
    required WsClient ws,
    required this.convId,
    required this.sessionId,
    required ClientMsgIdGen newClientMsgId,
    Scheduler scheduler = const RealScheduler(),
    ChatStore? store,
  })  : _ws = ws,
        _link = link,
        _store = store {
    // ★ 归约器与发件箱**互相需要**：发件箱要读归约器的当前代次才能装出一条合法的
    // `Send`，归约器要持有发件箱才能在 `TurnStarted` 回带时销账。
    //
    // 用 `late` 打破这个环：下面这个闭包在**调用时**才读 `controller`，
    // 而那时构造函数早已跑完。
    final Outbox? outbox = store == null
        ? null
        : Outbox(
            convId: convId,
            store: store,
            newId: newClientMsgId,
            send: (OutboxEntry e) => _sendVia(
              ws,
              sessionId,
              LlmSend(
                convId: convId,
                // 代次由归约器持有，发件箱不另存一份——存两份就会有一份是旧的，
                // 而代次不匹配的 `Send` 会被被控端整条拒掉（症状：消息发不出去，
                // 界面上只有一个永远转不完的「待发送」）。
                convGeneration: controller.generation,
                reqId: controller.nextReqId(),
                text: LlmText(e.text),
                clientMsgId: e.clientMsgId,
              ),
            ),
          );
    controller = ConversationController(
      convId: convId,
      send: (LlmFrame frame) => _sendVia(ws, sessionId, frame),
      scheduler: scheduler,
      store: store,
      outbox: outbox,
    );
  }

  final int convId;

  /// 承载本对话的隐藏会话 id（`RelayTo` 的路由键）。
  final int sessionId;

  final WsClient _ws;
  final LinkController _link;
  final ChatStore? _store;

  /// 归约器。UI 订阅它的 [ConversationController.snapshots] 与
  /// [ConversationController.tail]。
  late final ConversationController controller;

  StreamSubscription<S2CRelayTo>? _sub;

  /// 恢复 → 订阅并补齐 → 重发。**顺序是硬的**，见库文档。
  Future<void> start() async {
    if (_store != null) await controller.restore();
    _sub = _link.sessionFrames.listen(_onRelay);
    if (!controller.attach()) {
      // 发不出去就等于没挂上：不要静默，否则表现为「对话页一直空着」。
      logWarn(_tag, '没连上，Attach 未发出，补齐没有发生');
      return;
    }
    await controller.flushOutbox();
  }

  /// 主动发一帧（中断 / 拉历史都走它；发消息走 [ConversationController.submit]）。
  bool send(LlmFrame frame) => _sendVia(_ws, sessionId, frame);

  Future<void> dispose() async {
    await _sub?.cancel();
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
