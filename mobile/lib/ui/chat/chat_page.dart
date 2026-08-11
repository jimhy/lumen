/// 对话页：状态条 + 额度条 + 气泡流 + 输入框。
///
/// ## 三条订阅**刻意分开**
///
/// - `snapshot` → 条目列表变化时重建（低频）
/// - `tail` → 末块变化时只重建列表尾部那一个 widget（高频，在 `message_list.dart` 里）
/// - 额度 → 跟着 snapshot 走（账号级、变化极低频）
///
/// 合并成一个订阅的后果是：每 33 毫秒重建一次整页。
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/state/conversation_session.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/chat/chat_rate_limit_bar.dart';
import 'package:lumen_mobile/ui/chat/chat_status_bar.dart';
import 'package:lumen_mobile/ui/chat/message_list.dart';

/// 对话页。
class ChatPage extends ConsumerStatefulWidget {
  const ChatPage({super.key});

  @override
  ConsumerState<ChatPage> createState() => _ChatPageState();
}

class _ChatPageState extends ConsumerState<ChatPage> {
  final TextEditingController _input = TextEditingController();

  @override
  void dispose() {
    _input.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final ConversationSession? session = ref.watch(conversationSessionProvider);
    final LinkController? link = ref.watch(linkControllerProvider);
    if (session == null || link == null) {
      // 没有活跃会话——路由的 redirect 会把页面换走，这一帧先画个占位。
      return const Scaffold(body: Center(child: CircularProgressIndicator()));
    }
    return StreamBuilder<ConversationSnapshot>(
      stream: session.controller.snapshots,
      initialData: session.controller.state,
      builder: (BuildContext context, AsyncSnapshot<ConversationSnapshot> snap) {
        final ConversationSnapshot state =
            snap.data ?? const ConversationSnapshot();
        return Scaffold(
          appBar: AppBar(
            title: Text(state.meta?.title.value ?? '对话'),
            actions: <Widget>[
              IconButton(
                icon: const Icon(Icons.link_off),
                tooltip: '断开',
                onPressed: link.close,
              ),
            ],
          ),
          body: Column(
            children: <Widget>[
              ChatStatusBar(
                snapshot: state,
                onInterrupt: () => _interrupt(session, state),
              ),
              ChatRateLimitBar(info: state.rateLimit),
              if (state.blocksOmitted > 0) _omittedNotice(context, state),
              Expanded(
                child: MessageList(
                  snapshot: state,
                  tail: session.controller.tail,
                  onFillGap: (ChatGap gap) => _fillGap(session, gap),
                  onResend: (ChatPending item) => session.controller
                      .resendByUser(item.clientMsgId, nowMs: _nowMs()),
                  onDiscard: (ChatPending item) =>
                      session.controller.discardPending(item.clientMsgId),
                ),
              ),
              _composer(context, session, state),
            ],
          ),
        );
      },
    );
  }

  /// 快照声明「有 N 块因过大未能同步」——**必须说出来**，否则用户读到的是一段
  /// 看起来完整、实际缺块的回复。
  Widget _omittedNotice(BuildContext context, ConversationSnapshot state) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    return Container(
      width: double.infinity,
      color: scheme.tertiaryContainer,
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      child: Text(
        '本轮有 ${state.blocksOmitted} 块内容因过大未能同步',
        style: TextStyle(fontSize: 12, color: scheme.onTertiaryContainer),
      ),
    );
  }

  Widget _composer(
    BuildContext context,
    ConversationSession session,
    ConversationSnapshot state,
  ) {
    return SafeArea(
      top: false,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 6, 12, 8),
        child: Row(
          children: <Widget>[
            Expanded(
              child: TextField(
                controller: _input,
                minLines: 1,
                maxLines: 5,
                textInputAction: TextInputAction.newline,
                decoration: const InputDecoration(
                  hintText: '说点什么…',
                  border: OutlineInputBorder(),
                  isDense: true,
                ),
              ),
            ),
            const SizedBox(width: 8),
            IconButton.filled(
              onPressed: state.turnRunning ? null : () => _send(session, state),
              icon: const Icon(Icons.arrow_upward),
              tooltip: '发送',
            ),
          ],
        ),
      ),
    );
  }

  /// 发消息。**走发件箱，不直接发帧**（片 10）。
  ///
  /// ## ★ 现在无论如何都清输入框，这是相对片 7 的行为变更
  ///
  /// 老写法是「`send` 返回 false 就不清空」——因为那时消息真的丢了，把用户的话留在
  /// 输入框是唯一的补救。现在消息**先落库再发**：发不出去只是状态是「待发送」，
  /// 气泡已经在列表里、重连会自动补发。此时还留在输入框反而制造了两份同样的内容，
  /// 用户很可能再按一次发送。
  void _send(ConversationSession session, ConversationSnapshot state) {
    final String text = _input.text.trim();
    if (text.isEmpty) return;
    _input.clear();
    unawaited(session.controller.submit(text, nowMs: _nowMs()));
  }

  void _interrupt(ConversationSession session, ConversationSnapshot state) {
    final LlmConvMeta? meta = state.meta;
    if (meta == null) return;
    session.send(LlmInterrupt(
      convId: session.convId,
      convGeneration: meta.convGeneration,
      reqId: session.controller.nextReqId(),
    ));
  }

  /// 点「内容有缺口」：拉整轮快照覆盖重建。
  void _fillGap(ConversationSession session, ChatGap gap) {
    final LlmConvMeta? meta = session.controller.state.meta;
    if (meta == null) return;
    session.send(LlmTurnFetch(
      convId: session.convId,
      convGeneration: meta.convGeneration,
      reqId: session.controller.nextReqId(),
      turn: gap.turn,
    ));
  }

  static int _nowMs() => DateTime.now().millisecondsSinceEpoch;
}
