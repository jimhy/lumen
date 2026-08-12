/// 工具卡片的呈现（片 9）。
///
/// 领域判断全在 `domain/tool_card.dart`（纯函数、已测）；这里只负责画。
///
/// ## ★ 展开状态**不存在本 widget 里**
///
/// 气泡流是 `ListView.builder`，滚出屏幕的条目会被销毁。把 `expanded` 放进本 widget 的
/// `State`，用户展开一张卡片、往下滚一屏再滚回来，它就自己折叠了——一个不报错、
/// 只让人觉得「这 App 有点怪」的 bug。
///
/// 所以展开状态由 `message_list.dart` 按条目 key 存在一个 `Set` 里，本 widget 只收
/// [expanded] 与 [onToggle]。
///
/// ## ★ 没有可展开内容时**不画箭头**
///
/// 与「正在思考…」同一条理由：一个点开什么都没有的箭头比没有箭头更糟。
/// 判据是 `ToolCard.expandable`。
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:lumen_mobile/domain/text_diff.dart';
import 'package:lumen_mobile/domain/tool_card.dart';
import 'package:lumen_mobile/domain/tool_shape.dart';

/// 等宽字体族。命令输出与 diff 不用等宽就对不齐。
const List<String> kMonoFallback = <String>['Menlo', 'Consolas', 'monospace'];

/// 一张工具卡片。
class ToolCardTile extends StatelessWidget {
  const ToolCardTile({
    required this.card,
    required this.expanded,
    this.onToggle,
    this.onFetchFull,
    super.key,
  });

  final ToolCard card;

  /// 当前是否展开。**由父级持有**，见库文档。
  final bool expanded;

  final VoidCallback? onToggle;

  /// 正文被 PC 侧夹掉时，拉取完整内容（发 `TurnFetch`）。
  final VoidCallback? onFetchFull;

  @override
  Widget build(BuildContext context) {
    final ColorScheme scheme = Theme.of(context).colorScheme;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: <Widget>[
        _header(context, scheme),
        if (expanded && card.expandable) ...<Widget>[
          const SizedBox(height: 6),
          _body(context, scheme),
        ],
        if (expanded && card.truncatedBytes != null)
          _truncatedNotice(context, scheme),
      ],
    );
  }

  Widget _header(BuildContext context, ColorScheme scheme) {
    final Widget row = Row(
      mainAxisSize: MainAxisSize.min,
      children: <Widget>[
        Icon(_iconOf(card.shape), size: 14, color: scheme.onSurfaceVariant),
        const SizedBox(width: 6),
        Flexible(
          child: Text(
            card.summary,
            style: const TextStyle(fontSize: 13),
            maxLines: 2,
            overflow: TextOverflow.ellipsis,
          ),
        ),
        // ★ 不可展开就没有箭头。
        if (card.expandable) ...<Widget>[
          const SizedBox(width: 4),
          Icon(
            expanded ? Icons.expand_less : Icons.expand_more,
            size: 16,
            color: scheme.onSurfaceVariant,
          ),
        ],
      ],
    );
    if (!card.expandable) return row;
    return InkWell(onTap: onToggle, child: row);
  }

  Widget _body(BuildContext context, ColorScheme scheme) => switch (card.body) {
        ToolBodyNone() => const SizedBox.shrink(),
        ToolBodyText(:final String text) => _codeBlock(context, scheme, text),
        ToolBodyJson(:final String pretty) => _codeBlock(context, scheme, pretty),
        ToolBodyDiff(:final DiffResult diff, :final String? path) =>
          _diffBlock(context, scheme, diff, path),
      };

  /// 代码块：等宽 + **横向可滚**（长行不能把气泡撑破，也不该被硬折）。
  Widget _codeBlock(BuildContext context, ColorScheme scheme, String text) =>
      _framed(
        scheme,
        Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: <Widget>[
            SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: SelectableText(
                text,
                style: const TextStyle(
                  fontSize: 12,
                  fontFamilyFallback: kMonoFallback,
                ),
              ),
            ),
            _copyButton(context, text),
          ],
        ),
      );

  Widget _diffBlock(
    BuildContext context,
    ColorScheme scheme,
    DiffResult diff,
    String? path,
  ) {
    switch (diff) {
      case DiffTooLarge(:final int oldLines, :final int newLines):
        // ★ 说出来，而不是画个空的或者转圈到死（§14-6 无声降级禁令）。
        return _framed(
          scheme,
          Text(
            '改动过大（$oldLines 行 → $newLines 行），不在手机上展开',
            style: TextStyle(fontSize: 12, color: scheme.onSurfaceVariant),
          ),
        );
      case DiffLines(:final List<DiffLine> lines, :final int added, :final int removed):
        return _framed(
          scheme,
          Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: <Widget>[
              Text(
                '+$added −$removed${path == null ? '' : ' · $path'}',
                style: TextStyle(fontSize: 11, color: scheme.onSurfaceVariant),
              ),
              const SizedBox(height: 4),
              SingleChildScrollView(
                scrollDirection: Axis.horizontal,
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: <Widget>[
                    for (final DiffLine l in lines) _diffRow(scheme, l),
                  ],
                ),
              ),
              _copyButton(context, lines.map((DiffLine l) => l.toString()).join('\n')),
            ],
          ),
        );
    }
  }

  Widget _diffRow(ColorScheme scheme, DiffLine line) {
    // ★ 颜色**不是唯一判据**：行首恒带 +/-/空格。色盲用户与灰度截图下照样读得出来。
    final (Color? bg, Color fg, String mark) = switch (line.op) {
      DiffOp.add => (scheme.primaryContainer, scheme.onPrimaryContainer, '+'),
      DiffOp.del => (scheme.errorContainer, scheme.onErrorContainer, '-'),
      DiffOp.keep => (null, scheme.onSurfaceVariant, ' '),
    };
    return Container(
      color: bg,
      padding: const EdgeInsets.symmetric(horizontal: 2),
      child: Text(
        '$mark${line.text}',
        style: TextStyle(
          fontSize: 12,
          color: fg,
          fontFamilyFallback: kMonoFallback,
        ),
      ),
    );
  }

  Widget _framed(ColorScheme scheme, Widget child) => Container(
        width: double.infinity,
        padding: const EdgeInsets.all(6),
        decoration: BoxDecoration(
          color: scheme.surface,
          borderRadius: BorderRadius.circular(6),
        ),
        child: child,
      );

  Widget _copyButton(BuildContext context, String text) => Align(
        alignment: Alignment.centerRight,
        child: TextButton.icon(
          onPressed: () async {
            await Clipboard.setData(ClipboardData(text: text));
            // `mounted` 由 ScaffoldMessenger 自己兜；这里只在还挂着时提示。
            if (context.mounted) {
              ScaffoldMessenger.of(context).showSnackBar(
                const SnackBar(content: Text('已复制')),
              );
            }
          },
          icon: const Icon(Icons.copy, size: 14),
          label: const Text('复制', style: TextStyle(fontSize: 12)),
        ),
      );

  /// 正文被夹紧过 ⇒ 给一个按需拉取的入口。
  ///
  /// **必须说出被夹掉多少**：只写「内容已截断」会让用户以为少了一两行，
  /// 而实测一次 `Read` 被夹掉的是 12 KB。
  Widget _truncatedNotice(BuildContext context, ColorScheme scheme) => Padding(
        padding: const EdgeInsets.only(top: 4),
        child: InkWell(
          onTap: onFetchFull,
          child: Row(
            mainAxisSize: MainAxisSize.min,
            children: <Widget>[
              Icon(Icons.unfold_more, size: 13, color: scheme.tertiary),
              const SizedBox(width: 4),
              Flexible(
                child: Text(
                  '还有 ${card.truncatedBytes} 字节未同步'
                  '${onFetchFull == null ? '' : ' · 点击拉取'}',
                  style: TextStyle(fontSize: 12, color: scheme.tertiary),
                ),
              ),
            ],
          ),
        ),
      );
}

/// 语义图标名 → `IconData`。**这一层在 UI 里，`domain/` 不 import material。**
IconData _iconOf(ToolShape shape) => switch (iconNameOf(shape)) {
      'description' => Icons.description_outlined,
      'edit_note' => Icons.edit_note,
      'note_add' => Icons.note_add_outlined,
      'terminal' => Icons.terminal,
      'search' => Icons.search,
      'public' => Icons.public,
      'checklist' => Icons.checklist,
      'account_tree' => Icons.account_tree_outlined,
      _ => Icons.build_outlined,
    };
