/// 归约器的验收。
///
/// 三条不变量各有一组，写错了症状都极难复现：幂等错了会重复渲染、空洞补齐错了会显示
/// 一段「读起来通顺但少了中间几句」的回复、字素簇错了是可见闪烁。
///
/// 另有两组守的是**只会静默出错**的东西：`(turn, blockId)` 复合 key（只用 blockId
/// 会让助手块覆盖用户气泡）、额度 latest-wins（乱序播报会把已恢复显示成仍在限流）。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/state/streaming_tail.dart';

import '../support/fake_scheduler.dart';

/// 一份最小的对话元信息。
LlmConvMeta meta({int gen = 1, int contextUsed = 0, int contextLimit = 0}) =>
    LlmConvMeta(
      convId: 11,
      convGeneration: gen,
      agent: const OpenEnum<LlmAgentKindKnown>.known(LlmAgentKindKnown.claude),
      cwd: const LlmPath('F:/proj'),
      title: const LlmText('测试对话'),
      state: LlmConvState.running,
      curTurn: 1,
      createdMs: 0,
      updatedMs: 0,
      usage: LlmUsage(contextUsed: contextUsed, contextLimit: contextLimit),
    );

LlmBlockEntry entry(int blockId, LlmBlock block) =>
    LlmBlockEntry(blockId: blockId, block: block);

void main() {
  late FakeScheduler clock;
  late List<LlmFrame> sent;
  late ConversationController conv;

  setUp(() {
    clock = FakeScheduler();
    sent = <LlmFrame>[];
    conv = ConversationController(
      convId: 11,
      send: (LlmFrame f) {
        sent.add(f);
        return true;
      },
      scheduler: clock,
    );
  });

  tearDown(() async => conv.dispose());

  /// 建立基线：代次 1、水位 100。
  void attach({int gen = 1, int seq = 100}) {
    conv.applyFrame(LlmAttached(meta: meta(gen: gen), seq: seq));
  }

  /// 造一帧 Delta。
  LlmDelta delta(int seq, int turn, List<LlmDeltaItem> items, {int gen = 1}) =>
      LlmDelta(
        convId: 11,
        convGeneration: gen,
        seq: seq,
        turn: turn,
        items: items,
      );

  /// 流一段文字并让 33ms 合并窗口到期。
  void streamText(int seq, int turn, int blockId, String text) {
    conv.applyFrame(delta(seq, turn, <LlmDeltaItem>[
      LlmDeltaTextAppend(blockId: blockId, text: LlmText(text)),
    ]));
    clock.advance(kTailFlushWindow);
  }

  group('不变量 1：幂等', () {
    test('同一 seq 重复喂不会重复落条目', () {
      attach();
      final LlmDelta f = delta(101, 1, <LlmDeltaItem>[
        LlmDeltaBlockStart(entry(0, const LlmBlockToolUse(
          callId: 'c1',
          name: 'Read',
          input: LlmToolInput(null),
        ))),
      ]);
      conv.applyFrame(f);
      conv.applyFrame(f);
      conv.applyFrame(f);
      expect(conv.state.items.length, 1);
      expect(conv.state.lastSeq, 101);
    });

    test('★ 重放同一帧不会让文字翻倍', () {
      // 补齐与实时**会**重叠：Attach 之后被控端把水位线之后的增量重发一遍，而实时流没停。
      // 判据写成 `seq < lastSeq` 时这条会红——重放的 TextAppend 被再拼一次，
      // 用户看到「你好你好」。上面那条「不会重复落条目」抓不住它，
      // 因为 _upsert 按 key 去重、重复应用只是覆盖。
      attach();
      final LlmDelta f = delta(101, 1, <LlmDeltaItem>[
        const LlmDeltaTextAppend(blockId: 0, text: LlmText('你好')),
      ]);
      conv.applyFrame(f);
      conv.applyFrame(f);
      clock.advance(kTailFlushWindow);
      expect(conv.tail.value.text, '你好');
    });

    test('比水位线低的迟到帧被丢弃', () {
      attach(seq: 100);
      conv.applyFrame(delta(99, 1, <LlmDeltaItem>[
        const LlmDeltaDropped(blockId: 0, bytes: 1),
      ]));
      expect(conv.state.items, isEmpty);
      expect(conv.state.lastSeq, 100);
    });
  });

  group('不变量 2：空洞即补齐', () {
    test('★ 跳号时发 Attach 并且**不应用**这一帧', () {
      // 先应用再补，会让 UI 先显示一段错位内容。
      attach(seq: 100);
      conv.applyFrame(delta(105, 1, <LlmDeltaItem>[
        const LlmDeltaDropped(blockId: 0, bytes: 4096),
      ]));

      expect(conv.state.items, isEmpty, reason: '这一帧不该被应用');
      expect(conv.state.lastSeq, 100, reason: '水位线不该前进');
      expect(sent.single, isA<LlmAttach>());
      final LlmAttach a = sent.single as LlmAttach;
      expect(a.convId, 11);
      expect(a.knownSeq, 100, reason: '要从本地水位线之后补');
      expect(a.knownGeneration, 1);
    });

    test('连号则正常应用，不发 Attach', () {
      attach(seq: 100);
      conv.applyFrame(delta(101, 1, <LlmDeltaItem>[
        const LlmDeltaDropped(blockId: 0, bytes: 1),
      ]));
      expect(conv.state.lastSeq, 101);
      expect(sent, isEmpty);
    });
  });

  group('末块：33ms 合并 + 定稿', () {
    test('增量先进 tail，不进 items', () {
      attach();
      streamText(101, 1, 0, '你好');
      expect(conv.state.items, isEmpty, reason: '流式期间不该落定稿条目');
      expect(conv.tail.value.text, '你好');
      expect(conv.tail.value.blockId, 0);
    });

    test('★ 先拼接、不按字素簇切——ZWJ emoji 被拆两半也要拼回去', () {
      // 增量可能把一个 ZWJ 家庭 emoji 拆两半，两半各自都是合法字符串。
      attach();
      conv.applyFrame(delta(101, 1, <LlmDeltaItem>[
        const LlmDeltaTextAppend(blockId: 0, text: LlmText('👨‍')),
        const LlmDeltaTextAppend(blockId: 0, text: LlmText('👩‍👧')),
      ]));
      clock.advance(kTailFlushWindow);
      expect(conv.tail.value.text, '👨‍👩‍👧');
    });

    test('BlockEnd 定稿成一个条目，tail 清空', () {
      attach();
      streamText(101, 1, 0, '你好');
      conv.applyFrame(delta(102, 1, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(blockId: 0),
      ]));
      expect(conv.state.items.single, isA<ChatText>());
      expect((conv.state.items.single as ChatText).markdown, '你好');
      expect(conv.tail.value.isEmpty, isTrue);
    });

    test('BlockEnd 带终态块 ⇒ 覆盖本地拼接结果', () {
      // block != null 表示被控端声明本块降级过（丢过增量），带来了权威终态。
      attach();
      streamText(101, 1, 0, '半截');
      conv.applyFrame(delta(102, 1, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(blockId: 0, block: LlmBlockText(LlmText('完整正文'))),
      ]));
      expect((conv.state.items.single as ChatText).markdown, '完整正文');
    });

    test('33ms 窗口未到时不通知（不是每条增量一帧）', () {
      attach();
      int notifications = 0;
      conv.tail.addListener(() => notifications++);
      conv.applyFrame(delta(101, 1, <LlmDeltaItem>[
        const LlmDeltaTextAppend(blockId: 0, text: LlmText('a')),
        const LlmDeltaTextAppend(blockId: 0, text: LlmText('b')),
        const LlmDeltaTextAppend(blockId: 0, text: LlmText('c')),
      ]));
      expect(notifications, 0, reason: '窗口未到，一次都不该通知');
      clock.advance(kTailFlushWindow);
      expect(notifications, 1, reason: '三条增量合成一次通知');
      expect(conv.tail.value.text, 'abc');
    });
  });

  group('★ key 是 (turn, blockId)，不是 blockId', () {
    test('不同轮的同号块互不覆盖', () {
      // 只拿 blockId 当 key，第 2 轮的块会静默覆盖第 1 轮的——用户看到自己刚发的话
      // 变成了模型的回复，且没有任何报错。
      attach();
      streamText(101, 1, 0, '第一轮');
      conv.applyFrame(delta(102, 1, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(blockId: 0),
      ]));
      streamText(103, 2, 0, '第二轮');
      conv.applyFrame(delta(104, 2, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(blockId: 0),
      ]));

      expect(conv.state.items.length, 2);
      expect(conv.state.items.map((ChatItem i) => i.key).toList(),
          <String>['1:0', '2:0']);
      expect((conv.state.items[0] as ChatText).markdown, '第一轮');
      expect((conv.state.items[1] as ChatText).markdown, '第二轮');
    });

    test('用户块与助手块在同一轮内共用编号空间，不冲突', () {
      attach();
      conv.applyFrame(LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[entry(0, const LlmBlockText(LlmText('用户问题')))],
        startedMs: 0,
      ));
      // 助手块接着往下编（blockId 1），不会踩到用户的 0。
      streamText(102, 1, 1, '模型回答');
      conv.applyFrame(delta(103, 1, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(blockId: 1),
      ]));

      expect(conv.state.items.length, 2);
      expect(conv.state.items[0].role, ChatRole.user);
      expect(conv.state.items[1].role, ChatRole.assistant);
    });
  });

  group('★ 思考只做状态指示，不进 items', () {
    test('Thinking 块翻状态位而不落条目', () {
      // Claude 的 thinking 正文恒为空串，做成可折叠块就是个点开什么都没有的空盒子。
      attach();
      conv.applyFrame(delta(101, 1, <LlmDeltaItem>[
        LlmDeltaBlockStart(entry(0, const LlmBlockThinking(LlmThinkingOmitted()))),
      ]));
      expect(conv.state.thinking, isTrue);
      expect(conv.state.items, isEmpty, reason: '思考不占列表条目');

      conv.applyFrame(delta(102, 1, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(
            blockId: 0, block: LlmBlockThinking(LlmThinkingOmitted())),
      ]));
      expect(conv.state.thinking, isFalse);
      expect(conv.state.items, isEmpty);
    });

    test('轮结束时思考状态一定落下', () {
      attach();
      conv.applyFrame(delta(101, 1, <LlmDeltaItem>[
        LlmDeltaBlockStart(entry(0, const LlmBlockThinking(LlmThinkingOmitted()))),
      ]));
      conv.applyFrame(const LlmTurnEnded(
        convId: 11,
        convGeneration: 1,
        seq: 102,
        turn: 1,
        stop: OpenEnum<LlmStopReasonKnown>.known(LlmStopReasonKnown.endTurn),
        usage: LlmUsage(),
        endedMs: 0,
      ));
      expect(conv.state.thinking, isFalse, reason: '否则「正在思考…」会永远转下去');
    });
  });

  group('降级要看得见', () {
    test('Dropped ⇒ 落一个 ChatGap 条目', () {
      // 悄悄跳过它，用户会读到一段看起来完整、实际少了中间几句的回复。
      attach();
      conv.applyFrame(delta(101, 1, <LlmDeltaItem>[
        const LlmDeltaDropped(blockId: 3, bytes: 4096),
      ]));
      final ChatGap gap = conv.state.items.single as ChatGap;
      expect(gap.bytes, 4096);
      expect(gap.blockId, 3);
    });

    test('TurnEnded 带 truncated ⇒ 主动拉整轮快照', () {
      attach();
      conv.applyFrame(const LlmTurnEnded(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 4,
        stop: OpenEnum<LlmStopReasonKnown>.known(LlmStopReasonKnown.endTurn),
        usage: LlmUsage(),
        endedMs: 0,
        truncated: true,
      ));
      final LlmTurnFetch fetch =
          sent.whereType<LlmTurnFetch>().single;
      expect(fetch.turn, 4);
      expect(fetch.convId, 11);
    });

    test('快照的 blocksOmitted 被带出来给 UI', () {
      attach();
      conv.applyFrame(const LlmTurnSnapshot(
        convId: 11,
        convGeneration: 1,
        reqId: 1,
        seq: 101,
        record: LlmTurnRecord(turn: 1, startedMs: 0, blocksOmitted: 3),
      ));
      expect(conv.state.blocksOmitted, 3);
    });
  });

  group('快照整轮覆盖', () {
    test('同一轮的旧条目被替换，别的轮不受影响', () {
      attach();
      streamText(101, 1, 0, '第一轮');
      conv.applyFrame(delta(102, 1, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(blockId: 0),
      ]));
      streamText(103, 2, 0, '第二轮半截');

      conv.applyFrame(const LlmTurnSnapshot(
        convId: 11,
        convGeneration: 1,
        reqId: 1,
        seq: 104,
        record: LlmTurnRecord(
          turn: 2,
          startedMs: 0,
          assistant: <LlmBlockEntry>[
            LlmBlockEntry(blockId: 0, block: LlmBlockText(LlmText('第二轮定稿'))),
          ],
        ),
      ));

      expect(conv.state.items.length, 2);
      expect((conv.state.items[0] as ChatText).markdown, '第一轮');
      expect((conv.state.items[1] as ChatText).markdown, '第二轮定稿');
      expect(conv.tail.value.isEmpty, isTrue, reason: '快照是权威，半成品末块作废');
    });
  });

  group('★ 额度是账号级的，latest-wins', () {
    LlmRateLimitFrame rl(int observedMs, LlmRateLimitStatusKnown status) =>
        LlmRateLimitFrame(
          convId: 11,
          observedMs: observedMs,
          info: LlmRateLimit(
            status: OpenEnum<LlmRateLimitStatusKnown>.known(status),
          ),
        );

    /// 带窗口信息的额度帧——用它才能验出「谁覆盖了谁」。
    LlmRateLimitFrame rlWindowed(int observedMs, {required bool withWindow}) =>
        LlmRateLimitFrame(
          convId: 11,
          observedMs: observedMs,
          info: LlmRateLimit(
            status: const OpenEnum<LlmRateLimitStatusKnown>.known(
                LlmRateLimitStatusKnown.allowed),
            window: withWindow
                ? const OpenEnum<LlmRateLimitWindowKnown>.known(
                    LlmRateLimitWindowKnown.fiveHour)
                : null,
            resetsAtSecs: withWindow ? 1786211400 : null,
          ),
        );

    test('★ 乱序播报：旧的不覆盖新的（按 observedMs 比，不是按到达顺序）', () {
      // 本帧刻意没有 seq、与增量流无序。不比 observedMs 的话，一条迟到的旧播报会把
      // 额度条上的「五小时 · 约 HH:MM 恢复」凭空抹成空白——用户以为限流已经解除了。
      attach();
      conv.applyFrame(rlWindowed(2000, withWindow: true));
      conv.applyFrame(rlWindowed(1000, withWindow: false));

      expect(conv.state.rateLimit!.window?.known, LlmRateLimitWindowKnown.fiveHour,
          reason: '旧播报不该抹掉新播报里的窗口');
      expect(conv.state.rateLimit!.resetsAtSecs, 1786211400,
          reason: '恢复时刻同样不该被抹掉');
    });

    test('更新的播报能覆盖旧的', () {
      attach();
      conv.applyFrame(rlWindowed(1000, withWindow: false));
      conv.applyFrame(rlWindowed(2000, withWindow: true));
      expect(conv.state.rateLimit!.window?.known, LlmRateLimitWindowKnown.fiveHour);
    });

    test('没收到过额度信息时为 null（额度条整条不显示）', () {
      attach();
      expect(conv.state.rateLimit, isNull);
    });

    test('额度帧不需要 seq，也不推进水位线', () {
      attach(seq: 100);
      conv.applyFrame(rl(1000, LlmRateLimitStatusKnown.allowed));
      expect(conv.state.lastSeq, 100);
    });

    test('代次前进时额度**不清空**（它是账号级的）', () {
      attach(gen: 1);
      conv.applyFrame(rl(1000, LlmRateLimitStatusKnown.allowed));
      conv.applyFrame(LlmAttached(meta: meta(gen: 2), seq: 200));
      expect(conv.state.rateLimit, isNotNull,
          reason: '清掉等于把一条刚播报的限流状态抹掉');
      expect(conv.state.items, isEmpty, reason: '但对话内容要清');
    });
  });

  group('上下文占用', () {
    test('contextLimit == 0 ⇒ 不显示百分比', () {
      // 绝不按模型名查硬编码窗口表来「补」这个值——猜出来的百分比比不显示更糟。
      attach();
      conv.applyFrame(LlmAttached(
        meta: meta(contextUsed: 38342, contextLimit: 0),
        seq: 101,
      ));
      expect(conv.state.contextPercent, isNull);
    });

    test('两个数都有 ⇒ 算出百分比', () {
      attach();
      conv.applyFrame(LlmAttached(
        meta: meta(contextUsed: 500000, contextLimit: 1000000),
        seq: 101,
      ));
      expect(conv.state.contextPercent, closeTo(0.5, 1e-9));
    });

    test('轮末的 usage 覆盖基线里的', () {
      attach();
      conv.applyFrame(const LlmTurnEnded(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        stop: OpenEnum<LlmStopReasonKnown>.known(LlmStopReasonKnown.endTurn),
        usage: LlmUsage(contextUsed: 250000, contextLimit: 1000000),
        endedMs: 0,
      ));
      expect(conv.state.contextPercent, closeTo(0.25, 1e-9));
    });
  });

  group('代次守卫', () {
    test('旧代次的迟到帧被丢弃', () {
      attach(gen: 2, seq: 100);
      conv.applyFrame(delta(101, 1, <LlmDeltaItem>[
        const LlmDeltaDropped(blockId: 0, bytes: 1),
      ], gen: 1));
      expect(conv.state.items, isEmpty,
          reason: '应用它会把新对话污染成两条对话的混合体');
    });

    test('resyncRequired 的 Attached 清空重建', () {
      attach();
      streamText(101, 1, 0, '旧内容');
      conv.applyFrame(delta(102, 1, <LlmDeltaItem>[
        const LlmDeltaBlockEnd(blockId: 0),
      ]));
      expect(conv.state.items, isNotEmpty);

      conv.applyFrame(LlmAttached(meta: meta(), seq: 200, resyncRequired: true));
      expect(conv.state.items, isEmpty);
      expect(conv.state.lastSeq, 200);
    });
  });
}
