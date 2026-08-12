/// **用真实数据回放**：把 PC 侧归一化真跑出来的 LlmFrame 灌进归约器。
///
/// 语料来自 `crates/lumen-protocol/tests/golden/mobile/replay/`，
/// 由 `cargo test -p lumen-app -- --ignored 生成片7回放语料` 产出——它走的是**生产路径**
/// （白名单门 `classify` → 归一化 `decode_line` → 片 4 的 `LlmPlane::pump`），
/// 不是手写的。
///
/// ## 它比手工构造的测试多抓到什么
///
/// 第一次跑就撞出一个手工用例看不见的 bug：**被控端的第一帧 seq 是 0**，
/// 而归约器的 `lastSeq` 初值也是 0，幂等规则把整条对话的第一帧丢掉了。
/// 手工用例的 seq 全从 100 起，正好绕开了它。现在 `kNoSeqYet = -1`。
///
/// ⚠ 蓝图片 7 那句「回放语料直接用样本 B」**字面照做不通**：样本 B 是 Claude 的原始
/// stream-json（`"op"` 出现 0 次），喂给 `LlmFrame.fromJson` 不会报错、而是每行静默变成
/// `LlmUnknown`——最难查的失败模式。所以才有了上面那个生成器。
library;

import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:path/path.dart' as p;

import '../support/fake_scheduler.dart';

const String _replayDir =
    '../crates/lumen-protocol/tests/golden/mobile/replay';

List<LlmFrame> loadReplay(String file) {
  final File f = File(p.normalize(p.join(Directory.current.path, _replayDir, file)));
  return f
      .readAsLinesSync()
      .where((String l) => l.trim().isNotEmpty)
      .map((String l) => LlmFrame.fromJson(jsonDecode(l)))
      .toList();
}

void main() {
  late FakeScheduler clock;
  late ConversationController conv;

  setUp(() {
    clock = FakeScheduler();
    conv = ConversationController(
      convId: 1,
      send: (LlmFrame _) => true,
      scheduler: clock,
    );
  });

  tearDown(() async => conv.dispose());

  group('语料本身', () {
    test('两份都在，且每一行都是本端认识的 LlmFrame', () {
      for (final String name in <String>[
        'sample_b_llmframes.jsonl',
        'sample_a_llmframes.jsonl',
      ]) {
        final List<LlmFrame> frames = loadReplay(name);
        expect(frames, isNotEmpty, reason: '$name 是空的？');
        for (final LlmFrame f in frames) {
          // 解成 LlmUnknown 说明 Dart 侧漏实现了一个变体——而它**不会报错**，
          // 只会让那一帧静默消失。这条断言是那个失败模式的唯一出口。
          expect(f, isNot(isA<LlmUnknown>()),
              reason: '$name 里有本端不认识的帧：${f.variantName}');
        }
      }
    });

    test('样本 B 覆盖工具调用 / 工具结果 / 思考 / 限流', () {
      final List<LlmFrame> frames = loadReplay('sample_b_llmframes.jsonl');
      final List<String> ops =
          frames.map((LlmFrame f) => f.variantName).toList();
      expect(ops, contains('TurnStarted'));
      expect(ops.where((String o) => o == 'Delta').length, greaterThanOrEqualTo(3));
      expect(ops, contains('RateLimit'));
    });
  });

  group('回放样本 B（工具调用一轮）', () {
    setUp(() {
      for (final LlmFrame f in loadReplay('sample_b_llmframes.jsonl')) {
        conv.applyFrame(f);
        clock.advance(kTailFlushWindowForTest);
      }
    });

    test('★ 每一帧都被接受（第一帧 seq=0 不能被幂等规则吃掉）', () {
      // lastSeq 从 -1 起才接得住 seq=0。用 0 当初值时这条会红。
      expect(conv.state.lastSeq, greaterThanOrEqualTo(3));
    });

    test('工具调用与工具结果都落成了条目', () {
      final List<ChatItem> items = conv.state.items;
      expect(items.whereType<ChatToolCall>().length, 1);
      expect(items.whereType<ChatToolResult>().length, 1);

      final ChatToolCall call = items.whereType<ChatToolCall>().single;
      expect(call.name, 'Read');

      // 摘要走 detail（结构化），不是把 12272 字符的正文搬到手机上。
      final ChatToolResult result = items.whereType<ChatToolResult>().single;
      expect(result.detail, isA<LlmToolResultDetailFile>());
      expect(result.status, LlmToolStatus.ok);
    });

    test('★ 思考只翻状态位，一条条目都不留', () {
      // 样本 B 里那条 thinking 的正文是空串（Claude 恒为 Omitted）。
      expect(conv.state.thinking, isFalse, reason: '块已闭合，状态该落下');
      expect(conv.state.items.whereType<ChatUnknown>(), isEmpty,
          reason: '思考块不该以任何形式占列表条目');
    });

    test('限流被记下来，且不推进增量水位线', () {
      expect(conv.state.rateLimit, isNotNull);
      expect(conv.state.rateLimit!.window, isNotNull);
      expect(conv.state.rateLimit!.resetsAtSecs, isNotNull);
    });

    test('没有流式文本——这是语料的已知缺口，不是归约器的问题', () {
      // 两份样本里一个 LlmBlock::Text 都没有（样本 B 全是工具调用+思考）。
      // 文本流的渲染由手写语料 frame_delta.json / edge_text_append_split.json 覆盖，
      // 见 replay/README.md。这条断言把「缺口」钉成显式事实，免得后人以为是 bug。
      expect(conv.state.items.whereType<ChatText>(), isEmpty);
    });
  });

  group('回放样本 A（403 认证失败一轮）', () {
    setUp(() {
      for (final LlmFrame f in loadReplay('sample_a_llmframes.jsonl')) {
        conv.applyFrame(f);
        clock.advance(kTailFlushWindowForTest);
      }
    });

    test('错误块落成条目', () {
      expect(conv.state.items.whereType<ChatError>().length, 1);
    });

    test('★ 轮结束后 turnRunning 落下', () {
      // 否则「中断」按钮会一直亮着，而根本没有轮在跑。
      expect(conv.state.turnRunning, isFalse);
    });
  });
}

/// 与 `streaming_tail.dart` 的合并窗口同值。
///
/// 这里不 import 那个文件只为一个常量——回放测试关心的是「推进足够久让窗口到期」，
/// 给一个宽松的值即可。
const Duration kTailFlushWindowForTest = Duration(milliseconds: 50);
