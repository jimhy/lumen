/// 对话页各组件的**纯函数**部分。
///
/// 这里测的都是「说错了会误导用户」的文案与判据，不测像素：
/// 额度条什么时候该出现、上下文百分比什么时候不该显示、工具结果摘要走哪个字段。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/ui/chat/chat_bubbles.dart';
import 'package:lumen_mobile/ui/chat/chat_rate_limit_bar.dart';
import 'package:lumen_mobile/ui/chat/chat_status_bar.dart';

LlmRateLimit rl({
  LlmRateLimitStatusKnown? status = LlmRateLimitStatusKnown.allowed,
  String? unknownStatus,
  bool window = false,
  int? resetsAtSecs,
  LlmOverageStatusKnown? overage,
  LlmOverageDisabledReasonKnown? disabled,
  bool? usingOverage,
}) =>
    LlmRateLimit(
      status: unknownStatus != null
          ? OpenEnum<LlmRateLimitStatusKnown>.other(unknownStatus)
          : OpenEnum<LlmRateLimitStatusKnown>.known(status!),
      window: window
          ? const OpenEnum<LlmRateLimitWindowKnown>.known(
              LlmRateLimitWindowKnown.fiveHour)
          : null,
      resetsAtSecs: resetsAtSecs,
      overageStatus: overage == null
          ? null
          : OpenEnum<LlmOverageStatusKnown>.known(overage),
      overageDisabledReason: disabled == null
          ? null
          : OpenEnum<LlmOverageDisabledReasonKnown>.known(disabled),
      usingOverage: usingOverage,
    );

void main() {
  group('额度条：什么时候显示', () {
    test('没有额度信息 ⇒ 不显示', () {
      expect(ChatRateLimitBar.shouldShow(null), isFalse);
    });

    test('一切正常 ⇒ 整条不显示（不打扰）', () {
      expect(ChatRateLimitBar.shouldShow(rl()), isFalse);
    });

    test('★ status 正常但超额被拒 ⇒ 仍要显示', () {
      // 判据不能只看 status：超额被拒/被禁时 status 仍是 allowed，
      // 而那两种状态用户同样必须知道。
      expect(
        ChatRateLimitBar.shouldShow(
            rl(overage: LlmOverageStatusKnown.rejected)),
        isTrue,
      );
      expect(
        ChatRateLimitBar.shouldShow(
            rl(disabled: LlmOverageDisabledReasonKnown.outOfCredits)),
        isTrue,
      );
    });

    test('★ 未知 status 不当放行', () {
      // 「未知值默认放行」会让一个真实的限流状态被静默吞掉。
      expect(
        ChatRateLimitBar.shouldShow(rl(status: null, unknownStatus: 'Throttled')),
        isTrue,
      );
    });
  });

  group('额度条：文案', () {
    test('已用尽 + 恢复时刻', () {
      final String s = ChatRateLimitBar.describe(rl(
        status: null,
        unknownStatus: 'Exhausted',
        window: true,
        resetsAtSecs: 1786211400,
      ));
      expect(s.contains('五小时额度'), isTrue);
      expect(s.contains('恢复'), isTrue, reason: '要说什么时候能用');
      // 时刻是本地时区换算的结果，不写死具体数字——蓝图里那个「约 14:30」是随手编的示例。
      expect(RegExp(r'\d{2}:\d{2}').hasMatch(s), isTrue);
    });

    test('★ 未知窗口显示原文，不 fallback 成「未知」', () {
      const LlmRateLimit info = LlmRateLimit(
        status: OpenEnum<LlmRateLimitStatusKnown>.other('Exhausted'),
        window: OpenEnum<LlmRateLimitWindowKnown>.other('SevenDay'),
      );
      expect(ChatRateLimitBar.describe(info).contains('SevenDay'), isTrue);
    });

    test('★ usingOverage == null ⇒ 不说任何超额的话', () {
      // null 与 false 是两件事。把 null 渲染成「未在超额」就是错误的计费提示。
      expect(ChatRateLimitBar.describe(rl(usingOverage: null)).contains('超额'),
          isFalse);
      expect(ChatRateLimitBar.describe(rl(usingOverage: false)).contains('超额'),
          isFalse);
      expect(ChatRateLimitBar.describe(rl(usingOverage: true)).contains('超额'),
          isTrue);
    });

    test('★ status 正常时不硬安一个「受限」', () {
      // 这条只在超额信息存在时才会显示，措辞该让超额信息自己说话。
      final String s =
          ChatRateLimitBar.describe(rl(overage: LlmOverageStatusKnown.rejected));
      expect(s.contains('受限'), isFalse);
      expect(s.contains('已用尽'), isFalse);
    });

    test('荒谬的恢复时刻不显示（对端可控的值）', () {
      // resetsAtSecs 可达 i64 上界。显示一个 2 万年后的时刻只会让人以为功能坏了。
      final String s = ChatRateLimitBar.describe(rl(
        status: null,
        unknownStatus: 'Exhausted',
        resetsAtSecs: 999999999999,
      ));
      expect(s.contains('恢复'), isFalse);
    });
  });

  group('状态条', () {
    ConversationSnapshot snap({
      int used = 0,
      int limit = 0,
      int cost = 0,
      bool running = false,
    }) =>
        ConversationSnapshot(
          meta: LlmConvMeta(
            convId: 1,
            convGeneration: 1,
            agent: const OpenEnum<LlmAgentKindKnown>.known(
                LlmAgentKindKnown.claude),
            cwd: const LlmPath('/p'),
            title: const LlmText('t'),
            state: LlmConvState.idle,
            curTurn: 1,
            createdMs: 0,
            updatedMs: 0,
            usage: LlmUsage(
              contextUsed: used,
              contextLimit: limit,
              costMicroUsd: cost,
            ),
          ),
          turnRunning: running,
        );

    test('★ 分母未知 ⇒ 不显示百分比（绝不查硬编码窗口表来补）', () {
      expect(ChatStatusBar.contextLabel(snap(used: 38342)), isNull);
    });

    test('轮末是真值，不加「约」', () {
      expect(ChatStatusBar.contextLabel(snap(used: 420000, limit: 1000000)),
          '上下文 42%');
    });

    test('★ 轮中是估算，要加「约」', () {
      // 轮中沿用上一轮的窗口值算出来的是估算，说成真值是在夸大精度。
      expect(
        ChatStatusBar.contextLabel(
            snap(used: 420000, limit: 1000000, running: true)),
        '上下文 约 42%',
      );
    });

    test('花费为 0 时不占位', () {
      expect(ChatStatusBar.costLabel(const LlmUsage()), isNull);
    });

    test('不足一分钱时多给两位小数', () {
      // 否则一屏的调用全显示成 $0.00，用户会以为没花钱。
      expect(ChatStatusBar.costLabel(const LlmUsage(costMicroUsd: 1234)),
          '\$0.0012');
      expect(ChatStatusBar.costLabel(const LlmUsage(costMicroUsd: 1500000)),
          '\$1.50');
    });
  });

  group('工具结果摘要', () {
    ChatToolResult result({
      LlmToolResultDetail? detail,
      String output = '',
      LlmToolStatus status = LlmToolStatus.ok,
    }) =>
        ChatToolResult(
          turn: 1,
          blockId: 0,
          role: ChatRole.assistant,
          callId: 'c1',
          status: status,
          output: LlmText(output),
          detail: detail,
        );

    test('★ 有 detail ⇒ 走结构化摘要，不碰正文', () {
      // 一次 Read 的正文实测 12272 字符；detail 四个小字段就够画出这句话。
      final String s = ToolResultTile.summarize(result(
        detail: const LlmToolResultDetailFile(
          path: LlmPath('README.md'),
          startLine: 1,
          lineCount: 165,
          totalLines: 165,
        ),
        output: '这段正文不该出现在摘要里' * 100,
      ));
      expect(s, '读取 README.md 第 1–165 行（共 165 行）');
      expect(s.contains('不该出现'), isFalse);
    });

    test('没有 detail ⇒ 退回正文首行并截断', () {
      final String s = ToolResultTile.summarize(
          result(output: '第一行\n第二行\n第三行'));
      expect(s, '第一行');
    });

    test('非 ok 状态说出来', () {
      expect(ToolResultTile.summarize(result(status: LlmToolStatus.denied)),
          '工具被拒绝');
      expect(ToolResultTile.summarize(result(status: LlmToolStatus.error)),
          '工具出错');
    });
  });
}
