/// 发件箱与离线补齐的验收（片 10）。
///
/// 这一片的 bug 有个共同点：**症状都是「消息凭空多一条或少一条」，而且没有任何报错**。
/// 所以每一条断言都对着一种具体的丢失 / 重复形态写。
///
/// | 组 | 守的是什么 |
/// |---|---|
/// | 乐观发送 | 点了发送气泡就在（弱网下不会「按了没反应」） |
/// | 幂等重发 | 重发带**原** id（换新 id = 主动制造重复消息） |
/// | 销账 | 回带确认后停止重发（否则每次重连都白发一遍） |
/// | 上限 | 到 3 次转「未知」并**停手**（被控端去重表只有 32 条且会淘汰） |
/// | 恢复 | 杀进程重开能看到历史与在途消息，且水位线**不会偏高** |
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/data/memory_chat_store.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/conversation_controller.dart';
import 'package:lumen_mobile/state/outbox.dart';

import '../support/fake_scheduler.dart';

/// 记录副作用先后的 store 包装——**只为守「先落库、再发帧」那条不变量**。
final class _TracingStore implements ChatStore {
  _TracingStore(this._inner, this._trace);

  final ChatStore _inner;
  final List<String> _trace;

  @override
  Future<void> enqueueOutbox(OutboxEntry entry) {
    _trace.add('落库');
    return _inner.enqueueOutbox(entry);
  }

  @override
  dynamic noSuchMethod(Invocation invocation) =>
      // 其余方法原样转发。用 noSuchMethod 而不是逐个写 12 个 override：
      // 那 12 个都是纯委托，写出来只会让「哪个方法被插了桩」淹没在样板里。
      (_inner as dynamic).noSuchMethod(invocation);

  @override
  Future<void> open() => _inner.open();

  @override
  Future<void> close() => _inner.close();

  @override
  Future<StoredConv?> loadConv(int convId) => _inner.loadConv(convId);

  @override
  Future<void> saveConv(StoredConv conv) => _inner.saveConv(conv);

  @override
  Future<List<StoredItem>> loadItems(int convId) => _inner.loadItems(convId);

  @override
  Future<void> upsertItems(int convId, List<StoredItem> items) =>
      _inner.upsertItems(convId, items);

  @override
  Future<void> deleteTurn(int convId, int turn) =>
      _inner.deleteTurn(convId, turn);

  @override
  Future<void> clearItems(int convId) => _inner.clearItems(convId);

  @override
  Future<List<OutboxEntry>> loadOutbox(int convId) => _inner.loadOutbox(convId);

  @override
  Future<void> updateOutbox(String clientMsgId,
          {int? attempts, OutboxState? state}) =>
      _inner.updateOutbox(clientMsgId, attempts: attempts, state: state);

  @override
  Future<void> removeOutbox(String clientMsgId) =>
      _inner.removeOutbox(clientMsgId);
}

LlmConvMeta meta({int gen = 1}) => LlmConvMeta(
      convId: 11,
      convGeneration: gen,
      agent: const OpenEnum<LlmAgentKindKnown>.known(LlmAgentKindKnown.claude),
      cwd: const LlmPath('F:/proj'),
      title: const LlmText('测试对话'),
      state: LlmConvState.running,
      curTurn: 1,
      createdMs: 0,
      updatedMs: 0,
      usage: const LlmUsage(),
    );

void main() {
  late FakeScheduler clock;
  late MemoryChatStore store;
  late List<LlmFrame> sent;

  /// `send` 的开关：模拟「没连上」。
  late bool online;

  /// 可预测的 id 序列——断言里出现真 UUID 就只能写正则，一眼看不出哪条是哪条。
  late int idSeq;

  late Outbox outbox;
  late ConversationController conv;

  setUp(() {
    clock = FakeScheduler();
    store = MemoryChatStore();
    sent = <LlmFrame>[];
    online = true;
    idSeq = 0;
    outbox = Outbox(
      convId: 11,
      store: store,
      newId: () => 'msg-${++idSeq}',
      send: (OutboxEntry e) {
        if (!online) return false;
        sent.add(LlmSend(
          convId: 11,
          convGeneration: conv.generation,
          reqId: conv.nextReqId(),
          text: LlmText(e.text),
          clientMsgId: e.clientMsgId,
        ));
        return true;
      },
    );
    conv = ConversationController(
      convId: 11,
      send: (LlmFrame f) {
        sent.add(f);
        return true;
      },
      scheduler: clock,
      store: store,
      outbox: outbox,
    );
  });

  tearDown(() async => conv.dispose());

  /// 建立基线：代次 1、水位 100。
  void attach({int gen = 1, int seq = 100}) {
    conv.applyFrame(LlmAttached(meta: meta(gen: gen), seq: seq));
  }

  List<LlmSend> sends() => sent.whereType<LlmSend>().toList();

  group('乐观发送', () {
    test('点了发送，气泡立刻在列表里（哪怕没连上）', () async {
      attach();
      online = false;
      await conv.submit('跑一下测试', nowMs: 1000);

      final List<ChatPending> pending = conv.state.pending;
      expect(pending, hasLength(1));
      expect(pending.single.text, '跑一下测试');
      expect(pending.single.state, OutboxDelivery.queued,
          reason: '没连上 ⇒ 待发送，重连会自动补发');
      expect(sends(), isEmpty);
    });

    test('★ 先落库、再发帧（发出去了却没落库 = 消息永久消失）', () async {
      // ⚠ 只断言「submit 之后库里有」是**测不出顺序反了**的：两种顺序下这个断言都绿。
      // 所以这里记录两个副作用的先后。
      final List<String> trace = <String>[];
      final _TracingStore traced = _TracingStore(store, trace);
      final Outbox ob = Outbox(
        convId: 11,
        store: traced,
        newId: () => 'msg-x',
        send: (OutboxEntry e) {
          trace.add('发帧');
          return true;
        },
      );
      final ConversationController c = ConversationController(
        convId: 11,
        send: (LlmFrame f) => true,
        scheduler: FakeScheduler(),
        store: traced,
        outbox: ob,
      );
      addTearDown(c.dispose);

      await c.submit('第一条', nowMs: 1000);

      expect(trace, <String>['落库', '发帧'],
          reason: '反过来的话，「发出去了但没落库」那一瞬间进程被杀，消息就永远消失了');
      expect((await store.loadOutbox(11)).single.text, '第一条');
    });

    test('连上时发出去的帧带 clientMsgId 与当前代次', () async {
      attach(gen: 7);
      await conv.submit('跑一下测试', nowMs: 1000);

      final LlmSend f = sends().single;
      expect(f.clientMsgId, 'msg-1');
      expect(f.convGeneration, 7, reason: '代次只有归约器一个权威副本');
      expect(conv.state.pending.single.state, OutboxDelivery.sent);
    });

    test('乐观气泡排在所有定稿条目之后（它是「我刚发的」）', () async {
      attach();
      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[
          LlmBlockEntry(blockId: 0, block: LlmBlockText(LlmText('老消息'))),
        ],
        startedMs: 0,
      ));
      await conv.submit('新消息', nowMs: 1000);

      final List<ChatItem> visible = conv.state.visibleItems;
      expect(visible.last, isA<ChatPending>());
      expect(visible.last.turn, kPendingTurn);
    });
  });

  group('幂等重发', () {
    test('★ 重连后重发的是**同一个** clientMsgId', () async {
      attach();
      online = false;
      await conv.submit('跑一下测试', nowMs: 1000);
      expect(sends(), isEmpty);

      online = true;
      await conv.flushOutbox();

      expect(sends().single.clientMsgId, 'msg-1',
          reason: '换新 id 重发 = 主动制造一条重复消息；被控端按这个 id 去重');
    });

    test('★ 发不出去不计入尝试次数（那次尝试根本没发生）', () async {
      attach();
      online = false;
      await conv.submit('x', nowMs: 1000);
      await conv.flushOutbox();
      await conv.flushOutbox();

      final OutboxEntry row = (await store.loadOutbox(11)).single;
      expect(row.attempts, 0);
      expect(row.state, OutboxState.queued);
    });
  });

  group('销账', () {
    test('★ TurnStarted 回带我的 id ⇒ 气泡消失、库里也清掉', () async {
      attach();
      await conv.submit('跑一下测试', nowMs: 1000);
      expect(conv.state.pending, hasLength(1));

      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[
          LlmBlockEntry(
              blockId: 0, block: LlmBlockText(LlmText('跑一下测试'))),
        ],
        startedMs: 0,
        clientMsgId: 'msg-1',
      ));
      // 销账是异步的（要写库），让 microtask 队列跑完。
      await Future<void>.delayed(Duration.zero);

      expect(conv.state.pending, isEmpty, reason: '真实的 user 块顶上来了');
      expect(await store.loadOutbox(11), isEmpty);
      expect(conv.state.items.single, isA<ChatText>());
    });

    test('★ 回带的是别人的 id ⇒ 我的气泡不动', () async {
      // 对话表是全端共用的：另一部手机 / 桌面控制端 / PC 本地都可能开一轮。
      attach();
      await conv.submit('我的消息', nowMs: 1000);

      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[],
        startedMs: 0,
        clientMsgId: '别的控制端的-id',
      ));
      await Future<void>.delayed(Duration.zero);

      expect(conv.state.pending, hasLength(1));
    });

    test('★ 不带 clientMsgId 的老被控端 ⇒ 不许当成失败，也不许乱销账', () async {
      attach();
      await conv.submit('我的消息', nowMs: 1000);

      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[],
        startedMs: 0,
      ));
      await Future<void>.delayed(Duration.zero);

      expect(conv.state.pending, hasLength(1),
          reason: '没有回带 ⇒ 无从判断，保持在途、等用户手动处理');
    });
  });

  group('自动重发的上限', () {
    test('★ 到 $kOutboxMaxAttempts 次转「送达状态未知」并停手', () async {
      attach();
      await conv.submit('x', nowMs: 1000); // 第 1 次
      for (int i = 1; i < kOutboxMaxAttempts; i++) {
        await conv.flushOutbox();
      }
      expect(sends(), hasLength(kOutboxMaxAttempts));
      expect(conv.state.pending.single.state, OutboxDelivery.uncertain);

      // 到顶之后再 flush 多少次都不再发——被控端的去重表只有 32 条且会淘汰，
      // 无限重发早晚会越过那个窗口而被真的执行第二遍。
      await conv.flushOutbox();
      await conv.flushOutbox();
      expect(sends(), hasLength(kOutboxMaxAttempts));
    });

    test('★ 用户手动重发用的是**新** id', () async {
      attach();
      await conv.submit('x', nowMs: 1000);
      await conv.resendByUser('msg-1', nowMs: 2000);

      expect(sends().map((LlmSend f) => f.clientMsgId), <String>['msg-1', 'msg-2'],
          reason: '沿用旧 id 会被被控端当成重复丢掉 —— 用户点了重发却什么也没发生');
      expect(await store.loadOutbox(11), hasLength(1), reason: '旧行要清掉');
    });

    test('用户放弃一条消息', () async {
      attach();
      await conv.submit('x', nowMs: 1000);
      await conv.discardPending('msg-1');

      expect(conv.state.pending, isEmpty);
      expect(await store.loadOutbox(11), isEmpty);
    });
  });

  group('落库与恢复', () {
    /// 造一个共用同一个库的新控制器——模拟「杀进程重开」。
    ConversationController reopen() => ConversationController(
          convId: 11,
          send: (LlmFrame f) => true,
          scheduler: FakeScheduler(),
          store: store,
          outbox: Outbox(
            convId: 11,
            store: store,
            newId: () => 'msg-${++idSeq}',
            send: (OutboxEntry e) => true,
          ),
        );

    test('★ 杀进程重开：历史条目、水位线、在途消息都回来了', () async {
      attach(seq: 100);
      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[
          LlmBlockEntry(blockId: 0, block: LlmBlockText(LlmText('我问的'))),
        ],
        startedMs: 0,
      ));
      online = false;
      await conv.submit('还没发出去的', nowMs: 1000);
      await conv.flushPersistence();

      final ConversationController back = reopen();
      addTearDown(back.dispose);
      await back.restore();

      expect(back.state.items.single, isA<ChatText>());
      expect((back.state.items.single as ChatText).markdown, '我问的');
      expect(back.state.items.single.role, ChatRole.user, reason: '角色画反了更误导');
      expect(back.state.lastSeq, 101);
      expect(back.state.meta?.title.value, '测试对话');
      expect(back.state.pending.single.text, '还没发出去的');
    });

    test('★ 恢复之后 Attach 用的是恢复出来的水位线', () async {
      attach(seq: 100);
      await conv.flushPersistence();

      final List<LlmFrame> out = <LlmFrame>[];
      final ConversationController back = ConversationController(
        convId: 11,
        send: (LlmFrame f) {
          out.add(f);
          return true;
        },
        scheduler: FakeScheduler(),
        store: store,
      );
      addTearDown(back.dispose);
      await back.restore();
      back.attach();

      final LlmAttach f = out.single as LlmAttach;
      expect(f.knownSeq, 100,
          reason: '从 0 开始补会让被控端整段重发、甚至回 resync_required');
      expect(f.knownGeneration, 1);
    });

    test('★ 一帧都没收到时 Attach 发的是 0，不是 -1', () async {
      // 本地哨兵 kNoSeqYet 是 -1（片 7 为了区分「真的收到过 seq=0」才引入的），
      // 而负数不是合法线格式。
      final List<LlmFrame> out = <LlmFrame>[];
      final ConversationController fresh = ConversationController(
        convId: 11,
        send: (LlmFrame f) {
          out.add(f);
          return true;
        },
        scheduler: FakeScheduler(),
      );
      addTearDown(fresh.dispose);
      fresh.attach();

      expect((out.single as LlmAttach).knownSeq, 0);
    });

    test('★ 水位线宁可偏低：节流窗口没到就崩，补齐会多补而不是漏补', () async {
      attach(seq: 100);
      await conv.flushPersistence();
      // 之后又来了增量，但没到落盘时刻。
      conv.applyFrame(const LlmDelta(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        items: <LlmDeltaItem>[
          LlmDeltaBlockStart(
            LlmBlockEntry(blockId: 1, block: LlmBlockText(LlmText('x'))),
          ),
        ],
      ));

      // 直接读库（不 flush）：水位线还停在 100。
      final StoredConv stored = (await store.loadConv(11))!;
      expect(stored.lastSeq, 100,
          reason: '偏低 ⇒ 重连多补一段（幂等会吸收）；偏高 ⇒ 那段内容永远补不回来');
    });

    test('★ 代次前进会清掉本地存档，但**不清**在途消息', () async {
      attach(gen: 1, seq: 100);
      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[
          LlmBlockEntry(blockId: 0, block: LlmBlockText(LlmText('旧对话'))),
        ],
        startedMs: 0,
      ));
      online = false;
      await conv.submit('还没送到的', nowMs: 1000);

      // 被控端重建了对话（进程重启等）。
      conv.applyFrame(LlmAttached(meta: meta(gen: 2), seq: 0));
      await Future<void>.delayed(Duration.zero);

      expect(conv.state.items, isEmpty);
      expect(await store.loadItems(11), isEmpty,
          reason: '不清的话，重开 App 会看到两条对话拼在一起');
      expect(conv.state.pending, hasLength(1),
          reason: '在途消息还没送到，与被控端重建对话无关');
    });

    test('恢复时坏掉的那一行被跳过，其余照常', () async {
      await store.saveConv(const StoredConv(
        convId: 11,
        generation: 1,
        lastSeq: 5,
        metaJson: '{}',
        updatedMs: 0,
      ));
      await store.upsertItems(11, <StoredItem>[
        const StoredItem(
          turn: 1,
          blockId: 0,
          role: ChatRole.assistant,
          payloadJson: '这不是 json',
        ),
        StoredItem(
          turn: 1,
          blockId: 1,
          role: ChatRole.assistant,
          payloadJson: encodeItemPayload(const LlmBlockEntry(
            blockId: 1,
            block: LlmBlockText(LlmText('好的那条')),
          )),
        ),
      ]);

      final ConversationController back = reopen();
      addTearDown(back.dispose);
      await back.restore();

      expect(back.state.items, hasLength(1));
      expect((back.state.items.single as ChatText).markdown, '好的那条');
    });

    test('★ 快照覆盖一整轮时，库里那个「快照里没有的块」必须消失', () async {
      // 变异测试补的（把 deleteTurn 去掉后原来那批断言全绿）。
      // 只 upsert 不删的症状：重开 App 后历史里多出一个别处都没有的气泡，
      // 而在线时看不出来（内存里那份是 where(turn != …) 重建的）。
      attach(seq: 100);
      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[
          LlmBlockEntry(blockId: 0, block: LlmBlockText(LlmText('第 0 块'))),
          LlmBlockEntry(blockId: 1, block: LlmBlockText(LlmText('第 1 块'))),
        ],
        startedMs: 0,
      ));
      await Future<void>.delayed(Duration.zero);
      expect(await store.loadItems(11), hasLength(2));

      // 被控端的定稿快照里**只有一块**——第 1 块被它判定不该存在。
      conv.applyFrame(const LlmTurnSnapshot(
        convId: 11,
        convGeneration: 1,
        seq: 102,
        reqId: 1,
        record: LlmTurnRecord(
          turn: 1,
          user: <LlmBlockEntry>[
            LlmBlockEntry(blockId: 0, block: LlmBlockText(LlmText('第 0 块'))),
          ],
          assistant: <LlmBlockEntry>[],
          usage: LlmUsage(),
          startedMs: 0,
        ),
      ));
      await Future<void>.delayed(Duration.zero);

      expect(conv.state.items, hasLength(1), reason: '内存里的');
      expect(await store.loadItems(11), hasLength(1), reason: '★ 库里的也要少掉那一块');
    });

    test('★ 前一条被确认后，后一条乐观气泡的 key 不许变', () async {
      // 变异测试补的（把 blockId 固定成 0 之后原来那批断言全绿）。
      // key 一变，ListView 的 ValueKey 与 Markdown memo 缓存一起失效 ——
      // 症状是「前面那条一被确认，后面那条就闪一下」，只在连发两条时出现。
      attach();
      await conv.submit('第一条', nowMs: 1000);
      await conv.submit('第二条', nowMs: 1001);
      // 两条同时在途时 key 必须互不相同 —— 否则 ListView 会拿到重复的 ValueKey，
      // 两个气泡争同一个 Element。
      expect(
        conv.state.pending.map((ChatPending p) => p.key).toSet(),
        hasLength(2),
      );
      final String keyBefore = conv.state.pending
          .firstWhere((ChatPending p) => p.text == '第二条')
          .key;

      conv.applyFrame(const LlmTurnStarted(
        convId: 11,
        convGeneration: 1,
        seq: 101,
        turn: 1,
        user: <LlmBlockEntry>[],
        startedMs: 0,
        clientMsgId: 'msg-1',
      ));
      await Future<void>.delayed(Duration.zero);

      final ChatPending left = conv.state.pending.single;
      expect(left.text, '第二条');
      expect(left.key, keyBefore, reason: '销账不该让还在途的那条换 key');
    });

    test('★ 已经有在线数据了就不许被本地存档覆盖', () async {
      attach(seq: 100);
      await conv.flushPersistence();
      // 存档里是 seq=100；现在在线又推进到了 200。
      conv.applyFrame(LlmAttached(meta: meta(), seq: 200));

      await conv.restore();
      expect(conv.state.lastSeq, 200, reason: '在线数据永远是权威');
    });
  });
}
