/// 对话顶部状态条：模型 / 上下文占用 / 花费 / 中断。
///
/// ## ★ 上下文占用 ≠ 额度剩余
///
/// 两个条子**不要合并**：上下文占用说的是「这段对话塞了多满」，额度说的是「你这五小时
/// 还能不能用」。合并之后用户看到一个 90% 会以为自己快没额度了。额度在
/// `chat_rate_limit_bar.dart`。
///
/// ## 分母未知时不显示百分比
///
/// `contextLimit == 0` = 未知（首轮结束前拿不到窗口值）。此时**不显示**，
/// 且**绝不**按模型名去查一张硬编码的窗口表来「补」它——猜出来的百分比比不显示更糟，
/// 用户没法判断它准不准。
///
/// ## 轮中的数字要带「约」
///
/// 轮末的 `usage` 是**真值**（分母来自上游给的窗口值，不是我们猜的），可以直接写
/// 「上下文 42%」；轮中沿用上一轮窗口算出来的是**估算**，要标「约 42%」。
library;

import 'package:flutter/material.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';

/// 顶部状态条。
class ChatStatusBar extends StatelessWidget {
  const ChatStatusBar({
    required this.snapshot,
    this.onInterrupt,
    super.key,
  });

  final ConversationSnapshot snapshot;

  /// 中断当前轮。null = 不显示中断按钮。
  final VoidCallback? onInterrupt;

  @override
  Widget build(BuildContext context) {
    final ThemeData theme = Theme.of(context);
    final LlmConvMeta? meta = snapshot.meta;
    final TextStyle? style = theme.textTheme.bodySmall;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      color: theme.colorScheme.surfaceContainerHighest,
      child: Row(
        children: <Widget>[
          Expanded(
            child: Wrap(
              spacing: 12,
              runSpacing: 2,
              crossAxisAlignment: WrapCrossAlignment.center,
              children: <Widget>[
                Text(meta?.model ?? '连接中…', style: style),
                if (contextLabel(snapshot) case final String label)
                  Text(label, style: style),
                if (costLabel(meta?.usage) case final String label)
                  Text(label, style: style),
              ],
            ),
          ),
          if (snapshot.turnRunning && onInterrupt != null)
            TextButton(onPressed: onInterrupt, child: const Text('中断')),
        ],
      ),
    );
  }

  /// 上下文占用文案。分母未知时返回 null（整项不显示）。
  ///
  /// **纯函数，好测。**
  static String? contextLabel(ConversationSnapshot snapshot) {
    final double? pct = snapshot.contextPercent;
    if (pct == null) return null;
    final int rounded = (pct * 100).round();
    // 轮中是估算（分母沿用上一轮），轮末才是真值——用「约」把这件事说出来。
    return snapshot.turnRunning ? '上下文 约 $rounded%' : '上下文 $rounded%';
  }

  /// 花费文案。0 时返回 null（没花钱就别占位）。
  ///
  /// 单位是**微美元**（1e-6 USD），用整数避免浮点累加误差——这里才转成显示用的美元。
  static String? costLabel(LlmUsage? usage) {
    final int micro = usage?.costMicroUsd ?? 0;
    if (micro <= 0) return null;
    final double usd = micro / 1000000;
    // 小于 1 分钱时多给两位，否则一屏的调用全显示成 $0.00。
    return usd < 0.01 ? '\$${usd.toStringAsFixed(4)}' : '\$${usd.toStringAsFixed(2)}';
  }
}
