/// 额度状态条。**额度正常时整条不显示。**
///
/// ## 为什么值得占屏幕
///
/// 用户在外面用手机干活，最坏的体验是「发了一句话，转圈半天，其实 PC 那头卡在限流上」。
/// 这正是 §14-6「无声降级禁令」要挡的形态——**额度用尽是必须说出来的状态，
/// 不是可以静默等待的状态**。
///
/// ## 显示时刻，不显示倒计时
///
/// 手机进后台**不跑计时器**，回前台会看到一个停住的倒计时——那比不显示更糟，
/// 因为它看起来像在走。所以把 `resetsAtSecs` 转成本地**时刻**（「约 01:50 恢复」）。
///
/// ## 四个字段全是开放枚举
///
/// 实测只见到一组取值，**别当全集**：未知值直接显示原文，不要 fallback 成「未知」，
/// 更不要 `switch` 穷尽后 `throw`。
///
/// ## `usingOverage == null` 与 `false` 是两件事
///
/// null = 上游没给这个键。把它渲染成「未在超额」正是这里要避免的错误计费提示——
/// null 的正确处理是**不显示超额相关的任何文案**。
library;

import 'package:flutter/material.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

/// 额度条。[info] 为 null 或额度正常时返回零高度。
class ChatRateLimitBar extends StatelessWidget {
  const ChatRateLimitBar({required this.info, super.key});

  final LlmRateLimit? info;

  /// 是否该显示。
  ///
  /// 判据刻意**不是**「status != allowed」一条：超额被拒（`overageStatus`）与超额被禁
  /// （`overageDisabledReason`）时 status 仍可能是 allowed，而那两种状态用户同样必须知道。
  static bool shouldShow(LlmRateLimit? info) {
    if (info == null) return false;
    final bool statusOk =
        info.status.known == LlmRateLimitStatusKnown.allowed;
    // ⚠ 未知 status **不得**默认当成放行——按「状态不明」显示出来。
    if (!statusOk) return true;
    if (info.overageStatus != null) return true;
    if (info.overageDisabledReason != null) return true;
    return false;
  }

  @override
  Widget build(BuildContext context) {
    final LlmRateLimit? rl = info;
    if (!shouldShow(rl)) return const SizedBox.shrink();

    final ColorScheme scheme = Theme.of(context).colorScheme;
    return Container(
      width: double.infinity,
      color: scheme.errorContainer,
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
      child: Row(
        children: <Widget>[
          Icon(Icons.hourglass_bottom, size: 16, color: scheme.onErrorContainer),
          const SizedBox(width: 6),
          Expanded(
            child: Text(
              describe(rl!),
              style: TextStyle(fontSize: 12, color: scheme.onErrorContainer),
            ),
          ),
        ],
      ),
    );
  }

  /// 组一句人话。**纯函数，好测。**
  static String describe(LlmRateLimit info, {DateTime? now}) {
    final StringBuffer out = StringBuffer(_windowLabel(info.window));
    out.write(_statusLabel(info.status));
    final String? at = _resetAt(info.resetsAtSecs);
    if (at != null) out.write(' · 约 $at 恢复');
    // usingOverage 为 null 时**什么都不说**：null 与 false 不是一回事。
    if (info.usingOverage == true) out.write('（正在使用超额）');
    if (info.overageDisabledReason != null) {
      out.write('（超额已禁用：${info.overageDisabledReason!.wire}）');
    }
    return out.toString();
  }

  /// 窗口措辞。未知值**直接显示原文**。
  static String _windowLabel(OpenEnum<LlmRateLimitWindowKnown>? window) {
    if (window == null) return '额度';
    return switch (window.known) {
      LlmRateLimitWindowKnown.fiveHour => '五小时额度',
      // 未知取值：显示原文而不是「未知额度」——原文至少能让用户念给我们听。
      null => '${window.wire} 额度',
    };
  }

  /// 状态措辞。
  ///
  /// `allowed` 也可能走到这里——`shouldShow` 在「超额被拒 / 超额被禁」时同样要显示，
  /// 而那两种情况下 status 仍是 allowed。此时**不加状态词**，让后面的超额说明自己说话；
  /// 硬安一个「受限」是在陈述一件与 status 相反的事。
  static String _statusLabel(OpenEnum<LlmRateLimitStatusKnown> status) {
    if (status.known == LlmRateLimitStatusKnown.allowed) return '';
    // 未知 status **不当放行**，也不编一个说法——把原文显示出来，用户至少能念给我们听。
    return status.known == null ? '（状态：${status.wire}）' : '已用尽';
  }

  /// Unix **秒** → 本地 `HH:MM`。
  ///
  /// ⚠ 该值对端可控、可达 i64 上界：先夹紧再乘 1000，**不能裸乘**
  /// （裸乘在 Dart 上不会 panic，但会静默回绕成一个荒谬的时刻）。
  static String? _resetAt(int? secs) {
    if (secs == null || secs <= 0) return null;
    // 上限取 100 年后：再大的值一定是异常数据，显示它只会让人以为功能坏了。
    const int sane = 4102444800; // 2100-01-01
    if (secs > sane) return null;
    final DateTime at =
        DateTime.fromMillisecondsSinceEpoch(secs * 1000).toLocal();
    return '${at.hour.toString().padLeft(2, '0')}:'
        '${at.minute.toString().padLeft(2, '0')}';
  }
}
