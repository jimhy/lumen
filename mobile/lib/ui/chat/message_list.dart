/// 气泡流。**长对话不卡的三招全在这个文件里。**
///
/// 1. **流式末块渲染成纯 `Text`，块闭合才切成 Markdown**——既省解析，又避免半截
///    code fence 造成的闪烁（```` ``` ```` 刚流出一半时 Markdown 会把后面全当代码块）。
/// 2. **Markdown 按 `(itemKey, revision)` memo**（见 `ui/common/markdown_view.dart`），
///    条目一旦定稿就再也不重复解析。
/// 3. **`ListView.builder(reverse: true)` + `ValueKey` + `findChildIndexCallback`**，
///    末块挂在独立的 `ValueListenableBuilder` 上——**全程不重建整表**。
///
/// `reverse: true` 还天然锚在底部：流式增长不会把用户滚跑，也**不需要**每次增量调
/// `animateTo`（那是长对话卡顿的头号来源）。
///
/// ## 索引换算
///
/// `reverse: true` 下 index 0 是**视觉最底部**。自下而上依次是：
/// 末块（若有）→「正在思考…」（若有）→ items 的倒序。
library;

import 'package:flutter/material.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/domain/tool_card.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/state/streaming_tail.dart';
import 'package:lumen_mobile/ui/chat/chat_bubbles.dart';
import 'package:lumen_mobile/ui/chat/tool_card_tile.dart';
import 'package:lumen_mobile/ui/common/markdown_view.dart';

/// 气泡流列表。
class MessageList extends StatefulWidget {
  const MessageList({
    required this.snapshot,
    required this.tail,
    this.onFillGap,
    this.onResend,
    this.onDiscard,
    this.onFetchTurn,
    super.key,
  });

  final ConversationSnapshot snapshot;

  /// 末块。**单独订阅**，不进 [snapshot]。
  final StreamingTail tail;

  /// 点击「内容有缺口」时补齐（发 `TurnFetch`）。
  final void Function(ChatGap gap)? onFillGap;

  /// 手动重发一条「送达状态未知」的消息（片 10）。
  final void Function(ChatPending item)? onResend;

  /// 放弃一条消息。
  final void Function(ChatPending item)? onDiscard;

  /// 拉取某一轮的完整内容（片 9：工具正文被 PC 侧夹紧时按需拉）。
  ///
  /// 与 [onFillGap] 走的是同一条自愈通路（`TurnFetch`），只是入口不同：
  /// 那个是「这里有缺口」，这个是「这里被夹短了」。
  final void Function(int turn)? onFetchTurn;

  @override
  State<MessageList> createState() => _MessageListState();
}

class _MessageListState extends State<MessageList> {
  /// 每个列表一份，页面销毁即随之释放——全局单例会在页面没了之后继续攥着一堆 Widget。
  final MarkdownMemo _memo = MarkdownMemo();

  /// 已展开的工具卡片（按 [ChatItem.key]）。
  ///
  /// ★ **必须存在这里，不能存在卡片自己的 State 里**：气泡流是 `ListView.builder`，
  /// 滚出屏幕的条目会被销毁重建。存在卡片里的话，用户展开一张、往下滚一屏再滚回来，
  /// 它就自己折叠了——一个不报错、只让人觉得「这 App 有点怪」的 bug。
  final Set<String> _expanded = <String>{};

  @override
  void dispose() {
    _memo.clear();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return ValueListenableBuilder<TailSnapshot>(
      valueListenable: widget.tail,
      builder: (BuildContext context, TailSnapshot tail, Widget? _) {
        final List<ChatItem> items = widget.snapshot.items;
        final List<ChatPending> pending = widget.snapshot.pending;
        final bool hasTail = !tail.isEmpty;
        final bool thinking = widget.snapshot.thinking;
        // 自下而上：在途消息倒序 → 末块 → 思考指示 → items 倒序。
        //
        // ★ 在途消息在**最底部**，比正在流的末块还靠下。理由是聊天软件的普适直觉：
        // 「我刚发的那句」永远在最下面。把它排在末块上方（即按 items 顺序拼接）在
        // 「模型回复到一半、用户又发一条」时会让新消息插在回复中间，读起来像是它先发生的。
        final int extras = (hasTail ? 1 : 0) + (thinking ? 1 : 0);
        final int below = pending.length;

        return ListView.builder(
          reverse: true,
          padding: const EdgeInsets.symmetric(vertical: 8),
          itemCount: below + items.length + extras,
          findChildIndexCallback: (Key key) {
            if (key is! ValueKey<String>) return null;
            final int atPending =
                pending.indexWhere((ChatPending i) => i.key == key.value);
            if (atPending >= 0) return below - 1 - atPending;
            final int at = items.indexWhere((ChatItem i) => i.key == key.value);
            if (at < 0) return null;
            // items 倒序排在「在途 + extras」之后。
            return items.length - 1 - at + extras + below;
          },
          itemBuilder: (BuildContext context, int index) {
            if (index < below) return _bubble(pending[below - 1 - index]);
            final int afterPending = index - below;
            if (hasTail && afterPending == 0) return _tailBubble(tail);
            final int afterTail = afterPending - (hasTail ? 1 : 0);
            if (thinking && afterTail == 0) return const ThinkingIndicator();
            final int at = items.length - 1 - (afterTail - (thinking ? 1 : 0));
            if (at < 0 || at >= items.length) return const SizedBox.shrink();
            return _bubble(items[at]);
          },
        );
      },
    );
  }

  /// 流式末块：**纯 Text**，不走 Markdown。
  Widget _tailBubble(TailSnapshot tail) => ChatBubble(
        role: ChatRole.assistant,
        // 没有 ValueKey：它每几十毫秒变一次，给 key 反而妨碍复用。
        child: Text(tail.text),
      );

  Widget _bubble(ChatItem item) {
    final Widget body = switch (item) {
      ChatText(:final String markdown) =>
        _memo.of(item.key, item.revision, () => MarkdownView(data: markdown)),
      ChatToolCall() => _toolCard(item, toolCallCard(item)),
      ChatToolResult() => _toolCard(item, toolResultCard(item)),
      ChatImage() => ImageTile(item: item),
      ChatError() => ErrorTile(item: item),
      ChatGap() => GapTile(item: item, onFill: widget.onFillGap),
      ChatPending() => PendingTile(
          item: item,
          onResend: widget.onResend,
          onDiscard: widget.onDiscard,
        ),
      ChatUnknown() => const UnsupportedTile(),
    };
    return ChatBubble(
      key: ValueKey<String>(item.key),
      role: item.role,
      child: body,
    );
  }

  /// 工具卡片。展开状态按条目 key 存在 [_expanded] 里，滚动不丢。
  Widget _toolCard(ChatItem item, ToolCard card) => ToolCardTile(
        card: card,
        expanded: _expanded.contains(item.key),
        onToggle: () => setState(() {
          if (!_expanded.remove(item.key)) _expanded.add(item.key);
        }),
        // 夹紧过才给拉取入口；父级没接这个回调时按钮自然是禁用的。
        onFetchFull: card.truncatedBytes == null || widget.onFetchTurn == null
            ? null
            : () => widget.onFetchTurn!(item.turn),
      );
}
