/// [ChatStore] 的纯内存实现——**零 Flutter 依赖、零 sqflite 依赖**。
///
/// 两个用途，缺一不可：
///
/// 1. **归约器的单测**：`ConversationController` 的行为要在纯 Dart 里推得动，
///    挂一个真数据库进去就把「归约逻辑」的测试变成了「归约 + SQL」的测试。
/// 2. **没有库也能跑**：真机上 `openDatabase` 可能失败（磁盘满、被安全软件拦、
///    存储权限被回收）。那时应该退化成「这次不落库，对话照常」，
///    **不是**开不起来——见 `providers.dart` 的 `chatStoreProvider`。
///
/// ## ★ 它不是「假实现」，语义必须与 SQL 版逐条对齐
///
/// 主键冲突即覆盖、按 `(turn, block_id)` 升序、outbox 按 `created_ms` 升序、
/// `updateOutbox` 对不存在的行静默忽略——四条都有测试**同时**跑在两个实现上
/// （`test/data/chat_store_contract.dart` 是共享的契约测试）。
/// 少对齐一条，单测就会在真机上变成一句「明明测过」。
library;

import 'package:lumen_mobile/data/chat_store.dart';

/// 内存实现。
final class MemoryChatStore implements ChatStore {
  final Map<int, StoredConv> _convs = <int, StoredConv>{};

  /// `convId → (turn, blockId) → 条目`。用嵌套 map 表达复合主键。
  final Map<int, Map<String, StoredItem>> _items = <int, Map<String, StoredItem>>{};

  /// `clientMsgId → 行`。与 SQL 版一样：幂等键就是主键。
  final Map<String, OutboxEntry> _outbox = <String, OutboxEntry>{};

  bool _closed = false;

  @override
  Future<void> open() async {}

  @override
  Future<StoredConv?> loadConv(int convId) async => _convs[convId];

  @override
  Future<void> saveConv(StoredConv conv) async {
    _guard();
    _convs[conv.convId] = conv;
  }

  @override
  Future<List<StoredItem>> loadItems(int convId) async {
    final List<StoredItem> all =
        (_items[convId]?.values ?? const <StoredItem>[]).toList();
    all.sort((StoredItem a, StoredItem b) {
      final int byTurn = a.turn.compareTo(b.turn);
      return byTurn != 0 ? byTurn : a.blockId.compareTo(b.blockId);
    });
    return all;
  }

  @override
  Future<void> upsertItems(int convId, List<StoredItem> items) async {
    _guard();
    final Map<String, StoredItem> bucket =
        _items.putIfAbsent(convId, () => <String, StoredItem>{});
    for (final StoredItem item in items) {
      bucket['${item.turn}:${item.blockId}'] = item;
    }
  }

  @override
  Future<void> deleteTurn(int convId, int turn) async {
    _guard();
    _items[convId]?.removeWhere((_, StoredItem v) => v.turn == turn);
  }

  @override
  Future<void> clearItems(int convId) async {
    _guard();
    _items.remove(convId);
  }

  @override
  Future<void> enqueueOutbox(OutboxEntry entry) async {
    _guard();
    _outbox[entry.clientMsgId] = entry;
  }

  @override
  Future<List<OutboxEntry>> loadOutbox(int convId) async {
    final List<OutboxEntry> rows = _outbox.values
        .where((OutboxEntry e) => e.convId == convId)
        .toList()
      ..sort((OutboxEntry a, OutboxEntry b) =>
          a.createdMs.compareTo(b.createdMs));
    return rows;
  }

  @override
  Future<void> updateOutbox(String clientMsgId,
      {int? attempts, OutboxState? state}) async {
    _guard();
    final OutboxEntry? cur = _outbox[clientMsgId];
    // 行不存在 = 它刚被销账。静默忽略，与 SQL 的 `UPDATE … WHERE` 命中 0 行同语义。
    if (cur == null) return;
    _outbox[clientMsgId] = cur.copyWith(attempts: attempts, state: state);
  }

  @override
  Future<void> removeOutbox(String clientMsgId) async {
    _guard();
    _outbox.remove(clientMsgId);
  }

  @override
  Future<void> close() async {
    _closed = true;
  }

  /// 关掉之后再写是调用方的生命周期 bug，**要当场炸**而不是静默丢数据。
  ///
  /// SQL 版在同样的情形下会抛 `DatabaseException`，这里手工对齐——否则「关了还写」
  /// 这个 bug 只会在真机上现形。
  void _guard() {
    if (_closed) {
      throw StateError('MemoryChatStore 已关闭，不能再写');
    }
  }
}
