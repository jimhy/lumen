/// [ChatStore] 的 sqflite 实现（片 10，蓝图 §7.7 的三张表）。
///
/// ## 手写 SQL，不引 ORM
///
/// 蓝图否决了 drift（强依赖 `build_runner`）、Isar / Hive（维护状态不稳）与纯 JSONL
/// （分页去重要自己再实现一遍）。消息日志是「只追加 + 按主键倒查」的典型 SQL 负载，
/// 手写让**迁移完全可控**——而迁移正是这类 App 最容易出事的地方（用户不会重装来解决问题）。
///
/// ## ★ 它在 `flutter test` 里跑得动，但要调用方先装好 factory
///
/// `sqflite` 本体走 platform channel，Dart VM 里没有实现。测试侧用 `sqflite_common_ffi`
/// 把 `databaseFactory` 换掉即可（`test/data/chat_store_test.dart` 的 `setUpAll` 里那两行
/// `sqfliteFfiInit()` + `databaseFactory = databaseFactoryFfi`），
/// **本文件对此无感知**——它只用 `databaseFactory`，不 import ffi 包。
///
/// 这条边界是刻意的：本文件若自己去 `sqfliteFfiInit()`，生产包里就会多带一份 ffi 实现。
library;

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:sqflite/sqflite.dart';

const String _tag = 'chat-db';

/// schema 版本。**加列 / 改表就要 +1 并在 [_upgrade] 里补一段**。
const int kChatDbVersion = 1;

/// 库文件路径的解析器。
///
/// 做成 **异步函数**而不是一个 `String`：真机上路径来自 `getDatabasesPath()`，
/// 那是一次 platform channel 调用。写成同步参数就得在 `main()` 里 `await`，
/// 而那正是 `main.dart` 库文档里禁止的（白屏时间与「回前台秒恢复」的硬指标冲突）。
typedef DbPathResolver = Future<String> Function();

/// 三张表的 sqflite 实现。
///
/// ## ★ 懒打开
///
/// 构造是同步的（provider 需要），真正的 `openDatabase` 推迟到第一次用的时候。
/// 于是「忘了 `open()`」这个生命周期 bug 在类型上就不存在了——代价是打开失败会在
/// 第一次读写时才暴露，而那时的正确处置本来就是「这次没存上，对话照常」。
final class SqfliteChatStore implements ChatStore {
  SqfliteChatStore({required this.resolvePath});

  /// 测试用：直接给一个内存库。
  SqfliteChatStore.inMemory()
      : resolvePath = (() async => inMemoryDatabasePath);

  final DbPathResolver resolvePath;

  Database? _db;

  /// 正在打开的那一次。**并发保护**：两条路径同时第一次用时，
  /// 没有它就会 `openDatabase` 两次，第二次拿到的 handle 覆盖第一次的，
  /// 而第一次那个句柄再也没人关——真机上表现为「用久了打不开库」。
  Future<Database>? _opening;

  Future<Database> _ensure() async {
    final Database? ready = _db;
    if (ready != null) return ready;
    return _db = await (_opening ??= _openNow());
  }

  Future<Database> _openNow() async {
    final String path = await resolvePath();
    final Database db = await openDatabase(
      path,
      version: kChatDbVersion,
      onCreate: (Database db, int version) async => _create(db),
      onUpgrade: _upgrade,
      // ★ 外键**不开**：三张表刻意没有外键关系。events 与 outbox 都可以在
      // conversations 那一行还不存在时先落（消息可以在 ConvStarted 回来之前就入队），
      // 加外键只会把「先有蛋后有鸡」变成一次真机崩溃。
    );
    // 用 debug 不用 info：真机上这是沙箱路径（无所谓），但桌面 / `flutter test` 下它带用户名。
    // 日志的预期用法就是「把 lumen.log 发我看看」，能少一处路径就少一处。
    logDebug(_tag, '本地库已打开：$path');
    return db;
  }

  /// 预热。**不是必需的**（懒打开会兜住），但在会话建立时先跑一次可以把打开耗时
  /// 挪出第一次读写的路径。
  @override
  Future<void> open() async {
    await _ensure();
  }

  static Future<void> _create(Database db) async {
    await db.execute('''
      CREATE TABLE conversations (
        conv_id     INTEGER PRIMARY KEY,
        generation  INTEGER NOT NULL,
        last_seq    INTEGER NOT NULL,
        meta_json   TEXT    NOT NULL,
        updated_ms  INTEGER NOT NULL
      )
    ''');
    // ★ 主键是 (conv_id, turn, block_id) 而不是蓝图写的 (conv_id, seq)：
    //   TurnEnded 与 Delta 会同号（片 7 实证），且一条 Delta 装多个块的增量——
    //   seq 从来就不是「一个条目」的标识。理由全文见 chat_store.dart 的库文档。
    await db.execute('''
      CREATE TABLE events (
        conv_id      INTEGER NOT NULL,
        turn         INTEGER NOT NULL,
        block_id     INTEGER NOT NULL,
        role         TEXT    NOT NULL,
        payload_json TEXT    NOT NULL,
        PRIMARY KEY (conv_id, turn, block_id)
      )
    ''');
    await db.execute('''
      CREATE TABLE outbox (
        client_msg_id TEXT    PRIMARY KEY,
        conv_id       INTEGER NOT NULL,
        text          TEXT    NOT NULL,
        created_ms    INTEGER NOT NULL,
        attempts      INTEGER NOT NULL DEFAULT 0,
        state         TEXT    NOT NULL
      )
    ''');
    // 「取一条对话的待送队列」是每次重连都要跑的查询；没有它就是全表扫。
    await db.execute(
      'CREATE INDEX idx_outbox_conv ON outbox (conv_id, created_ms)',
    );
  }

  /// 迁移。**目前只有 v1，没有任何一段迁移代码**——但这个钩子必须留着并写明规矩：
  ///
  /// 加列一律 `ALTER TABLE … ADD COLUMN … DEFAULT …`（sqlite 支持，且不重建表）；
  /// **不许**用「删表重建」偷懒，那会把用户的历史对话一次清空。
  static Future<void> _upgrade(Database db, int from, int to) async {
    logWarn(_tag, '本地库需要迁移：v$from → v$to');
    // v1 是首版，还没有 from < to 的分支。新版本在这里按 from 逐级往上补。
  }

  @override
  Future<StoredConv?> loadConv(int convId) async {
    final List<Map<String, Object?>> rows = await (await _ensure()).query(
      'conversations',
      where: 'conv_id = ?',
      whereArgs: <Object?>[convId],
      limit: 1,
    );
    if (rows.isEmpty) return null;
    final Map<String, Object?> r = rows.first;
    return StoredConv(
      convId: r['conv_id']! as int,
      generation: r['generation']! as int,
      lastSeq: r['last_seq']! as int,
      metaJson: r['meta_json']! as String,
      updatedMs: r['updated_ms']! as int,
    );
  }

  @override
  Future<void> saveConv(StoredConv conv) async {
    await (await _ensure()).insert(
      'conversations',
      <String, Object?>{
        'conv_id': conv.convId,
        'generation': conv.generation,
        'last_seq': conv.lastSeq,
        'meta_json': conv.metaJson,
        'updated_ms': conv.updatedMs,
      },
      conflictAlgorithm: ConflictAlgorithm.replace,
    );
  }

  @override
  Future<List<StoredItem>> loadItems(int convId) async {
    final List<Map<String, Object?>> rows = await (await _ensure()).query(
      'events',
      where: 'conv_id = ?',
      whereArgs: <Object?>[convId],
      orderBy: 'turn ASC, block_id ASC',
    );
    return rows.map(_rowToItem).toList();
  }

  static StoredItem _rowToItem(Map<String, Object?> r) => StoredItem(
        turn: r['turn']! as int,
        blockId: r['block_id']! as int,
        role: _roleFromWire(r['role']! as String),
        payloadJson: r['payload_json']! as String,
      );

  @override
  Future<void> upsertItems(int convId, List<StoredItem> items) async {
    if (items.isEmpty) return;
    // 一批条目一个事务：流式期间每帧都可能落好几条，逐条自动提交会让磁盘 IO 成为
    // 33 ms 合并窗口之外的第二个卡顿源。
    await (await _ensure()).transaction((Transaction txn) async {
      for (final StoredItem item in items) {
        await txn.insert(
          'events',
          <String, Object?>{
            'conv_id': convId,
            'turn': item.turn,
            'block_id': item.blockId,
            'role': item.role.name,
            'payload_json': item.payloadJson,
          },
          conflictAlgorithm: ConflictAlgorithm.replace,
        );
      }
    });
  }

  @override
  Future<void> deleteTurn(int convId, int turn) async {
    await (await _ensure()).delete(
      'events',
      where: 'conv_id = ? AND turn = ?',
      whereArgs: <Object?>[convId, turn],
    );
  }

  @override
  Future<void> clearItems(int convId) async {
    await (await _ensure()).delete(
      'events',
      where: 'conv_id = ?',
      whereArgs: <Object?>[convId],
    );
  }

  @override
  Future<void> enqueueOutbox(OutboxEntry entry) async {
    await (await _ensure()).insert(
      'outbox',
      <String, Object?>{
        'client_msg_id': entry.clientMsgId,
        'conv_id': entry.convId,
        'text': entry.text,
        'created_ms': entry.createdMs,
        'attempts': entry.attempts,
        'state': entry.state.name,
      },
      conflictAlgorithm: ConflictAlgorithm.replace,
    );
  }

  @override
  Future<List<OutboxEntry>> loadOutbox(int convId) async {
    final List<Map<String, Object?>> rows = await (await _ensure()).query(
      'outbox',
      where: 'conv_id = ?',
      whereArgs: <Object?>[convId],
      orderBy: 'created_ms ASC',
    );
    return rows
        .map((Map<String, Object?> r) => OutboxEntry(
              clientMsgId: r['client_msg_id']! as String,
              convId: r['conv_id']! as int,
              text: r['text']! as String,
              createdMs: r['created_ms']! as int,
              attempts: r['attempts']! as int,
              state: _outboxStateFromWire(r['state']! as String),
            ))
        .toList();
  }

  @override
  Future<void> updateOutbox(String clientMsgId,
      {int? attempts, OutboxState? state}) async {
    final Map<String, Object?> values = <String, Object?>{};
    if (attempts != null) values['attempts'] = attempts;
    if (state != null) values['state'] = state.name;
    if (values.isEmpty) return;
    await (await _ensure()).update(
      'outbox',
      values,
      where: 'client_msg_id = ?',
      whereArgs: <Object?>[clientMsgId],
    );
  }

  @override
  Future<void> removeOutbox(String clientMsgId) async {
    await (await _ensure()).delete(
      'outbox',
      where: 'client_msg_id = ?',
      whereArgs: <Object?>[clientMsgId],
    );
  }

  @override
  Future<void> close() async {
    await _db?.close();
    _db = null;
  }
}

/// 角色的线值 → 枚举。**未知值一律当 assistant**。
///
/// 为什么不是「抛异常」：这是本地库，坏值只可能来自本端的老版本或磁盘损坏，
/// 让一行坏数据把整段历史顶掉是最差的处置。为什么不是「当 user」：把模型说的话
/// 画成用户自己说的，比反过来更容易误导（用户会以为自己发过这句话）。
ChatRole _roleFromWire(String wire) =>
    wire == ChatRole.user.name ? ChatRole.user : ChatRole.assistant;

/// outbox 状态的线值 → 枚举。未知值当 [OutboxState.uncertain]：
/// **宁可让用户看到「送达状态未知」，也不要静默当成已送达而丢掉一条消息。**
OutboxState _outboxStateFromWire(String wire) => OutboxState.values.firstWhere(
      (OutboxState s) => s.name == wire,
      orElse: () => OutboxState.uncertain,
    );
