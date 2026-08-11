/// 气泡与各类条目的呈现。
///
/// 片 7 只做**能读**：文本气泡、工具调用/结果的一行摘要、缺口提示、思考指示。
/// 工具卡片的展开、diff、复制是片 9 的活——这里刻意**不**预留展开箭头，
/// 因为一个点开什么都没有的箭头比没有箭头更糟。
library;

import 'package:flutter/material.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

/// 一个气泡外壳。用户靠右、模型靠左。
class ChatBubble extends StatelessWidget {
  const ChatBubble({required this.role, required this.child, super.key});

  final ChatRole role;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    final bool mine = role == ChatRole.user;
    return Align(
      alignment: mine ? Alignment.centerRight : Alignment.centerLeft,
      child: Container(
        margin: const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
        constraints: BoxConstraints(
          maxWidth: MediaQuery.sizeOf(context).width * 0.86,
        ),
        decoration: BoxDecoration(
          color: mine ? scheme.primaryContainer : scheme.surfaceContainerHighest,
          borderRadius: BorderRadius.circular(12),
        ),
        child: child,
      ),
    );
  }
}

/// 「正在思考…」。
///
/// ★ **只做状态指示，不做可折叠内容块**：Claude 的思考正文恒为空串，
/// 做成能展开的就是给用户一个点开什么都没有的坑。它不占列表条目、不落库。
class ThinkingIndicator extends StatelessWidget {
  const ThinkingIndicator({super.key});

  @override
  Widget build(BuildContext context) {
    final ThemeData theme = Theme.of(context);
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 6),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          SizedBox(
            width: 12,
            height: 12,
            child: CircularProgressIndicator(
              strokeWidth: 2,
              color: theme.colorScheme.outline,
            ),
          ),
          const SizedBox(width: 8),
          Text(
            '正在思考…',
            style: theme.textTheme.bodySmall
                ?.copyWith(color: theme.colorScheme.outline),
          ),
        ],
      ),
    );
  }
}

/// 一次工具调用的摘要行。
class ToolCallTile extends StatelessWidget {
  const ToolCallTile({required this.item, super.key});

  final ChatToolCall item;

  @override
  Widget build(BuildContext context) {
    // title 是 PC 侧归一化出来的一行人话；没有就退回工具名。
    final String label = item.title?.value ?? item.name;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: <Widget>[
        const Icon(Icons.build_outlined, size: 14),
        const SizedBox(width: 6),
        Flexible(child: Text(label, style: const TextStyle(fontSize: 13))),
      ],
    );
  }
}

/// 一次工具结果的摘要行。
///
/// ★ **摘要走 `detail`，不走 `output`**：一次 `Read` 的正文实测 12272 字符，
/// 而 `detail` 四个小字段就够画出「读取 README.md 第 1–165 行（共 165 行）」。
/// `detail == null`（非文件类工具，形态未实测）时才退回正文的**首行**。
class ToolResultTile extends StatelessWidget {
  const ToolResultTile({required this.item, super.key});

  final ChatToolResult item;

  @override
  Widget build(BuildContext context) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    final bool bad = item.status != LlmToolStatus.ok;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: <Widget>[
        Icon(
          bad ? Icons.error_outline : Icons.check_circle_outline,
          size: 14,
          color: bad ? scheme.error : scheme.primary,
        ),
        const SizedBox(width: 6),
        Flexible(child: Text(summarize(item), style: const TextStyle(fontSize: 13))),
      ],
    );
  }

  /// 组一句摘要。**纯函数，好测。**
  static String summarize(ChatToolResult item) {
    final LlmToolResultDetail? detail = item.detail;
    if (detail is LlmToolResultDetailFile) {
      final int last = detail.startLine + detail.lineCount - 1;
      final String range = '第 ${detail.startLine}–$last 行（共 ${detail.totalLines} 行）';
      // 只读了一部分时要说出来，否则用户以为模型看了全文。
      return '读取 ${detail.path.value} $range';
    }
    if (item.status != LlmToolStatus.ok) {
      return '工具${_statusLabel(item.status)}';
    }
    // 未实测形态：退回正文首行，**不猜字段名**。
    final String first = item.output.value.split('\n').first;
    final String head = first.length > 60 ? '${first.substring(0, 60)}…' : first;
    return head.isEmpty ? '工具已完成' : head;
  }

  static String _statusLabel(LlmToolStatus status) => switch (status) {
        LlmToolStatus.ok => '完成',
        LlmToolStatus.error => '出错',
        LlmToolStatus.denied => '被拒绝',
        LlmToolStatus.cancelled => '被取消',
        LlmToolStatus.unknown => '状态未知',
      };
}

/// 图片附件（片 7 只画一行说明，缩略图是 P1 的活）。
class ImageTile extends StatelessWidget {
  const ImageTile({required this.item, super.key});

  final ChatImage item;

  @override
  Widget build(BuildContext context) => Row(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          const Icon(Icons.image_outlined, size: 14),
          const SizedBox(width: 6),
          Flexible(
            child: Text(item.attachment.name.value,
                style: const TextStyle(fontSize: 13)),
          ),
        ],
      );
}

/// 错误块。
class ErrorTile extends StatelessWidget {
  const ErrorTile({required this.item, super.key});

  final ChatError item;

  @override
  Widget build(BuildContext context) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    return Text(
      '${item.code.wire}：${item.message.value}',
      style: TextStyle(fontSize: 13, color: scheme.error),
    );
  }
}

/// **内容缺口**：点一下补齐。
///
/// 悄悄跳过它，用户会读到一段看起来完整、实际少了中间几句的回复（§14-6 无声降级禁令）。
class GapTile extends StatelessWidget {
  const GapTile({required this.item, this.onFill, super.key});

  final ChatGap item;
  final void Function(ChatGap gap)? onFill;

  @override
  Widget build(BuildContext context) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    return InkWell(
      onTap: onFill == null ? null : () => onFill!(item),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          Icon(Icons.link_off, size: 14, color: scheme.tertiary),
          const SizedBox(width: 6),
          Text(
            '内容有缺口（${item.bytes} 字节）${onFill == null ? '' : ' · 点击补齐'}',
            style: TextStyle(fontSize: 13, color: scheme.tertiary),
          ),
        ],
      ),
    );
  }
}

/// **乐观发送中的用户消息**（片 10）。
///
/// ## 三态各自说什么
///
/// | 状态 | 角标 | 为什么这么说 |
/// |---|---|---|
/// | [OutboxDelivery.queued] | 「待发送」+ 时钟 | 还没连上，**对方一定没收到**，重连会自动发 |
/// | [OutboxDelivery.sent] | 一个空心勾 | 已送出，等对方确认。**不是「成功」** |
/// | [OutboxDelivery.uncertain] | 「送达状态未知」+ 重发 / 删除 | 见下 |
///
/// ★ [OutboxDelivery.uncertain] 的文案**必须说出「对方可能已经收到」**。
/// 写成「发送失败」是错的：那会让用户放心地再发一遍，而实际上对方很可能已经执行过一次
/// ——「让 PC 跑一遍 Bash」这种事重复执行的代价是真的。
class PendingTile extends StatelessWidget {
  const PendingTile({
    required this.item,
    this.onResend,
    this.onDiscard,
    super.key,
  });

  final ChatPending item;
  final void Function(ChatPending item)? onResend;
  final void Function(ChatPending item)? onDiscard;

  @override
  Widget build(BuildContext context) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.end,
      mainAxisSize: MainAxisSize.min,
      children: <Widget>[
        Text(item.text),
        const SizedBox(height: 4),
        switch (item.state) {
          OutboxDelivery.queued => _hint(scheme, Icons.schedule, '待发送 · 连上后自动发出'),
          OutboxDelivery.sent => _hint(scheme, Icons.done, '已送出'),
          OutboxDelivery.uncertain => _uncertain(context, scheme),
        },
      ],
    );
  }

  Widget _hint(ColorScheme scheme, IconData icon, String text) => Row(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          Icon(icon, size: 13, color: scheme.outline),
          const SizedBox(width: 4),
          Text(text, style: TextStyle(fontSize: 12, color: scheme.outline)),
        ],
      );

  Widget _uncertain(BuildContext context, ColorScheme scheme) => Column(
        crossAxisAlignment: CrossAxisAlignment.end,
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          Row(
            mainAxisSize: MainAxisSize.min,
            children: <Widget>[
              Icon(Icons.help_outline, size: 13, color: scheme.error),
              const SizedBox(width: 4),
              Flexible(
                child: Text(
                  // ★ 不写「发送失败」——见类文档。
                  '送达状态未知，对方可能已经收到过',
                  style: TextStyle(fontSize: 12, color: scheme.error),
                ),
              ),
            ],
          ),
          Row(
            mainAxisSize: MainAxisSize.min,
            children: <Widget>[
              TextButton(
                onPressed: onResend == null ? null : () => onResend!(item),
                child: const Text('再发一次'),
              ),
              TextButton(
                onPressed: onDiscard == null ? null : () => onDiscard!(item),
                child: const Text('删除'),
              ),
            ],
          ),
        ],
      );
}

/// 本版本不认识的块。
///
/// 画出来而不是跳过：跳过等于让用户读到一段少了东西的回复却毫不知情。
class UnsupportedTile extends StatelessWidget {
  const UnsupportedTile({super.key});

  @override
  Widget build(BuildContext context) => Text(
        '（本版本不支持的内容）',
        style: TextStyle(
          fontSize: 13,
          color: Theme.of(context).colorScheme.outline,
        ),
      );
}
