/// [ChatStore] 的**共享契约测试**——同一批断言跑在两个实现上。
///
/// ## 为什么要共享
///
/// 归约器的单测挂 [MemoryChatStore]，真机跑 `SqfliteChatStore`。两者只要有一条语义
/// 对不上，「本地全绿」就会在真机上变成一句「明明测过」。这正是蓝图 §7.7 那条
/// 「DAO 必须藏在抽象接口后」想挡的坏形态的**另一半**：藏在接口后还不够，
/// 两个实现得被同一批断言钉着。
///
/// 逐条对应一种真实的漂移：
///
/// | 断言 | 不对齐时的症状 |
/// |---|---|
/// | 主键冲突即覆盖 | 内存版 upsert、SQL 版 UNIQUE 冲突抛异常 ⇒ 流式写到一半整条挂掉 |
/// | `(turn, block_id)` 升序 | 内存版按插入序 ⇒ 重开 App 后气泡顺序与在线时不同 |
/// | outbox 按 `created_ms` 升序 | 重发顺序乱 ⇒ 用户连发的两句话被倒过来问 |
/// | 更新不存在的行静默忽略 | SQL 命中 0 行无事，内存版若抛 ⇒ 销账与重试竞争时崩 |
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

/// 造一条最小条目。
StoredItem itemAt(int turn, int blockId, String text,
        {ChatRole role = ChatRole.assistant}) =>
    StoredItem(
      turn: turn,
      blockId: blockId,
      role: role,
      payloadJson: encodeItemPayload(LlmBlockEntry(
        blockId: blockId,
        block: LlmBlockText(LlmText(text)),
      )),
    );

/// 造一条 outbox 行。
OutboxEntry outboxAt(String id, int createdMs, {int convId = 1}) => OutboxEntry(
      clientMsgId: id,
      convId: convId,
      text: '第 $id 条',
      createdMs: createdMs,
    );

/// 把整套契约跑在 [make] 造出来的实现上。
void runChatStoreContract(String label, ChatStore Function() make) {
  group('ChatStore 契约（$label）', () {
    late ChatStore store;

    setUp(() async {
      store = make();
      await store.open();
    });

    tearDown(() async => store.close());

    test('conversations：写入后读回来一致，同 id 覆盖', () async {
      expect(await store.loadConv(1), isNull, reason: '没写过就该是 null');

      await store.saveConv(const StoredConv(
        convId: 1,
        generation: 3,
        lastSeq: 42,
        metaJson: '{"conv_id":1}',
        updatedMs: 100,
      ));
      StoredConv? got = await store.loadConv(1);
      expect(got?.generation, 3);
      expect(got?.lastSeq, 42);

      // 水位线前进是最高频的一次写，必须是覆盖不是插第二行。
      await store.saveConv(const StoredConv(
        convId: 1,
        generation: 3,
        lastSeq: 99,
        metaJson: '{"conv_id":1}',
        updatedMs: 200,
      ));
      got = await store.loadConv(1);
      expect(got?.lastSeq, 99);
    });

    test('events：按 (turn, block_id) 升序返回，与插入顺序无关', () async {
      // 刻意乱序插入：真实流式里「上一轮的末块」确实可能晚于新一轮的首块落库。
      await store.upsertItems(1, <StoredItem>[
        itemAt(2, 1, 'b'),
        itemAt(1, 0, 'a'),
        itemAt(2, 0, 'c'),
      ]);
      final List<StoredItem> got = await store.loadItems(1);
      expect(
        got.map((StoredItem i) => '${i.turn}:${i.blockId}').toList(),
        <String>['1:0', '2:0', '2:1'],
      );
    });

    test('events：主键冲突即覆盖（不是抛异常，也不是插第二行）', () async {
      await store.upsertItems(1, <StoredItem>[itemAt(1, 0, '半截')]);
      await store.upsertItems(1, <StoredItem>[itemAt(1, 0, '定稿了')]);

      final List<StoredItem> got = await store.loadItems(1);
      expect(got, hasLength(1), reason: '同一个 (turn, block_id) 只该有一行');
      final ItemPayload? payload = decodeItemPayload(got.single.payloadJson);
      final LlmBlockText block =
          (payload! as BlockPayload).entry.block as LlmBlockText;
      expect(block.text.value, '定稿了');
    });

    test('events：deleteTurn 只清那一轮，clearItems 清整条对话', () async {
      await store.upsertItems(1, <StoredItem>[itemAt(1, 0, 'a'), itemAt(2, 0, 'b')]);
      await store.upsertItems(2, <StoredItem>[itemAt(1, 0, '别的对话')]);

      await store.deleteTurn(1, 1);
      expect((await store.loadItems(1)).map((StoredItem i) => i.turn), <int>[2]);
      expect(await store.loadItems(2), hasLength(1), reason: '不许波及别的对话');

      await store.clearItems(1);
      expect(await store.loadItems(1), isEmpty);
      expect(await store.loadItems(2), hasLength(1));
    });

    test('events：角色往返不丢（画反了比丢了更误导）', () async {
      await store.upsertItems(1, <StoredItem>[
        itemAt(1, 0, '我问的', role: ChatRole.user),
        itemAt(1, 1, '它答的'),
      ]);
      final List<StoredItem> got = await store.loadItems(1);
      expect(got[0].role, ChatRole.user);
      expect(got[1].role, ChatRole.assistant);
    });

    test('outbox：按 created_ms 升序（发送顺序就是重发顺序）', () async {
      await store.enqueueOutbox(outboxAt('c', 300));
      await store.enqueueOutbox(outboxAt('a', 100));
      await store.enqueueOutbox(outboxAt('b', 200));

      expect(
        (await store.loadOutbox(1)).map((OutboxEntry e) => e.clientMsgId),
        <String>['a', 'b', 'c'],
      );
    });

    test('outbox：同 clientMsgId 覆盖，不产生第二行', () async {
      await store.enqueueOutbox(outboxAt('a', 100));
      await store.enqueueOutbox(outboxAt('a', 100));
      expect(await store.loadOutbox(1), hasLength(1));
    });

    test('outbox：只返回本对话的行', () async {
      await store.enqueueOutbox(outboxAt('a', 100));
      await store.enqueueOutbox(outboxAt('b', 100, convId: 2));
      expect((await store.loadOutbox(1)).single.clientMsgId, 'a');
    });

    test('outbox：更新次数与状态往返', () async {
      await store.enqueueOutbox(outboxAt('a', 100));
      await store.updateOutbox('a', attempts: 2, state: OutboxState.sent);

      final OutboxEntry got = (await store.loadOutbox(1)).single;
      expect(got.attempts, 2);
      expect(got.state, OutboxState.sent);
      expect(got.text, '第 a 条', reason: '没传的字段不许被清掉');
    });

    test('★ outbox：更新一条已被销账的行是静默忽略，不是抛', () async {
      // 销账（收到回带确认）与重试计时器是并发的：先销账后更新是正常时序，
      // 在这里抛异常会让一次成功的送达以崩溃收场。
      await store.removeOutbox('从来没有过的 id');
      await store.updateOutbox('从来没有过的 id', attempts: 9);
      expect(await store.loadOutbox(1), isEmpty);
    });

    test('outbox：销账后就查不到了', () async {
      await store.enqueueOutbox(outboxAt('a', 100));
      await store.removeOutbox('a');
      expect(await store.loadOutbox(1), isEmpty);
    });
  });
}
