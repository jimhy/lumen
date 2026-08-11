/// 两个 [ChatStore] 实现跑同一套契约，外加各自专属的几条。
///
/// ## ★ 真 SQL 在 `flutter test` 里是怎么跑起来的
///
/// `sqflite` 走 platform channel，Dart VM 里没有实现。`sqflite_common_ffi` 把
/// `databaseFactory` 换成一个进程内的 sqlite —— 于是 `SqfliteChatStore` 里那些**真的 SQL**
/// （建表、`ConflictAlgorithm.replace`、`ORDER BY`、事务）在纯 Dart 测试里就会被真的执行。
///
/// 这一步不做的话，本文件就只剩下「内存实现自己跟自己对账」，而 SQL 里的错字要等真机才现形。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/data/memory_chat_store.dart';
import 'package:lumen_mobile/data/sqflite_chat_store.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:sqflite_common_ffi/sqflite_ffi.dart';

import 'chat_store_contract.dart';

void main() {
  setUpAll(() {
    sqfliteFfiInit();
    databaseFactory = databaseFactoryFfi;
  });

  runChatStoreContract('内存', MemoryChatStore.new);
  runChatStoreContract('sqflite(ffi)', SqfliteChatStore.inMemory);

  group('条目载荷的编解码', () {
    test('普通块往返：连 parent_call_id 一起', () {
      const LlmBlockEntry entry = LlmBlockEntry(
        blockId: 7,
        parentCallId: 'call-abc',
        block: LlmBlockText(LlmText('嵌套子代理的输出')),
      );
      final ItemPayload? back = decodeItemPayload(encodeItemPayload(entry));

      final BlockPayload payload = back! as BlockPayload;
      expect(payload.entry.blockId, 7);
      expect(payload.entry.parentCallId, 'call-abc',
          reason: '归属丢了会让子代理输出跑回主对话');
      expect((payload.entry.block as LlmBlockText).text.value, '嵌套子代理的输出');
    });

    test('★ 缺口条目也要落库（跳过它就是无声降级）', () {
      // 不存它的症状：重开 App 后读到一段「看起来完整、其实少了中间几句」的回复，
      // 且再也没有任何迹象。§14-6 挡的正是这个形态。
      final ItemPayload? back = decodeItemPayload(encodeGapPayload(4096));
      expect((back! as GapPayload).bytes, 4096);
    });

    test('未知块降级后往返仍然是未知块，不抛', () {
      // 前向兼容：老版本 App 读到新版本写下的块。
      const LlmBlockEntry entry =
          LlmBlockEntry(blockId: 0, block: LlmBlockUnknown());
      final ItemPayload? back = decodeItemPayload(encodeItemPayload(entry));
      expect((back! as BlockPayload).entry.block, isA<LlmBlockUnknown>());
    });

    test('★ 坏载荷返回 null 而不是抛（一行坏数据不该顶掉整段历史）', () {
      expect(decodeItemPayload('不是 json'), isNull);
      expect(decodeItemPayload('{"t":"没见过的形态"}'), isNull);
      expect(decodeItemPayload('{"t":"b"}'), isNull, reason: '缺 entry 也要兜住');
    });
  });

  group('StoredConv.decodeMeta', () {
    test('坏 meta 返回 null 而不是让 App 起不来', () {
      const StoredConv bad = StoredConv(
        convId: 1,
        generation: 1,
        lastSeq: 0,
        metaJson: '{截断的',
        updatedMs: 0,
      );
      expect(bad.decodeMeta(), isNull);
    });
  });

  group('MemoryChatStore 专属', () {
    test('关掉之后再写要当场炸（与 SQL 版对齐）', () async {
      final MemoryChatStore store = MemoryChatStore();
      await store.open();
      await store.close();
      expect(
        () => store.upsertItems(1, <StoredItem>[itemAt(1, 0, 'x')]),
        throwsA(isA<StateError>()),
      );
    });
  });

  group('SqfliteChatStore 专属', () {
    test('★ 懒打开：没显式 open() 也能直接用（忘了 open 这个 bug 不该存在）', () async {
      final SqfliteChatStore store = SqfliteChatStore.inMemory();
      addTearDown(store.close);
      expect(await store.loadItems(1), isEmpty);
    });

    test('★ 并发首次访问只打开一次库', () async {
      // 没有并发保护时会 openDatabase 两次，后一个句柄覆盖前一个、前一个再也没人关。
      // 真机上的症状是「用久了打不开库」，几乎不可能在开发期复现。
      int opened = 0;
      final SqfliteChatStore store = SqfliteChatStore(resolvePath: () async {
        opened++;
        return inMemoryDatabasePath;
      });
      addTearDown(store.close);
      await Future.wait<Object?>(<Future<Object?>>[
        store.loadItems(1),
        store.loadOutbox(1),
        store.loadConv(1),
      ]);
      expect(opened, 1);
    });

    test('★ 未知的 role / state 有确定的兜底，不抛', () async {
      // 这两个值来自本端的老版本或磁盘损坏。抛异常 = 一行坏数据顶掉整段历史。
      final SqfliteChatStore store = SqfliteChatStore.inMemory();
      await store.open();
      addTearDown(store.close);

      final Database db =
          await databaseFactory.openDatabase(inMemoryDatabasePath);
      // 直接写脏行绕过 DAO —— 模拟老版本 / 损坏。
      await db.insert('events', <String, Object?>{
        'conv_id': 1,
        'turn': 1,
        'block_id': 0,
        'role': '未来才有的角色',
        'payload_json': encodeGapPayload(1),
      });
      await db.insert('outbox', <String, Object?>{
        'client_msg_id': 'x',
        'conv_id': 1,
        'text': 't',
        'created_ms': 1,
        'attempts': 0,
        'state': '未来才有的状态',
      });

      expect((await store.loadItems(1)).single.role, ChatRole.assistant,
          reason: '把模型说的话画成用户自己说的更容易误导');
      expect((await store.loadOutbox(1)).single.state, OutboxState.uncertain,
          reason: '宁可让用户看到「送达状态未知」，也不要静默当成已送达');
    });
  });
}
