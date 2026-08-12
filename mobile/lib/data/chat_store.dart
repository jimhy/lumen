/// 对话的本地持久化**接口**（片 10）——**零 Flutter 依赖、零 sqflite 依赖**。
///
/// ## 为什么是接口
///
/// `sqflite` 走 platform channel，在 `flutter test` 的 Dart VM 里**根本不存在**。把 DAO 藏在
/// 接口后是蓝图 §7.7 明写的要求，它挡住的是两种坏形态：
///
/// 1. 「本地测试全绿、真机崩」——测试里根本没跑过真 SQL；
/// 2. 「为了能测而把持久化写进 widget test」——归约逻辑被迫和 UI 绑死。
///
/// 于是有两个实现：[MemoryChatStore]（纯内存，归约器的单测用它）与 `SqfliteChatStore`
/// （真 SQL，`flutter test` 里靠 `sqflite_common_ffi` 也跑得动，见 `sqflite_chat_store.dart`）。
///
/// ## ★ 三张表的主键与蓝图 §7.7 不同——那句话被真实协议证伪了
///
/// 蓝图写 `events` 表主键是 `(conv_id, seq)`，理由是「天然给了 last_seq 水位线和幂等去重」。
/// **在真实协议下这个主键会冲突**：片 7 用回放语料实证 `TurnEnded` 的 `seq` 与前一条 `Delta`
/// **相同**（`sample_a` 里都是 1），而且一条 `Delta` 帧里装的是**多个**块的增量——
/// `seq` 从来就不是「一个条目」的标识。
///
/// 这里改成 `(conv_id, turn, block_id)`，也就是 [ChatItem.key] 本来的定义。收益不只是「不冲突」：
/// 落库的 upsert 语义与内存里 `ConversationController._upsert` **变成同一件事**，
/// 「快照覆盖一整轮」也退化成一条 `DELETE WHERE turn = ?`。
///
/// 水位线 `lastSeq` 因此**移到 `conversations` 表**上（它是每对话一个值，本来也不该按行存）。
library;

import 'dart:convert';

import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

/// 一条对话的存档头。
final class StoredConv {
  const StoredConv({
    required this.convId,
    required this.generation,
    required this.lastSeq,
    required this.metaJson,
    required this.updatedMs,
  });

  final int convId;

  /// 对话代次。**恢复出来的历史只对同一代次有效**：代次前进说明被控端重建了对话
  /// （进程重启等），旧条目与新对话拼在一起就是两条对话的混合体。
  final int generation;

  /// 已消费到的增量水位线。重连后 `Attach{known_seq}` 用它，[kNoSeqYet] = 一帧都没收到。
  final int lastSeq;

  /// [LlmConvMeta] 的原样 JSON。
  ///
  /// **刻意整块存而不是拆成列**：meta 有 14 个字段且还在长（`model` / `permission_mode` /
  /// `cli_session_id` 都是后加的），拆列意味着协议每加一个字段就要写一次 schema 迁移。
  /// 需要按字段查询时再拆——目前一个都不需要（只有「按 conv_id 取回来整个」这一种用法）。
  final String metaJson;

  final int updatedMs;

  /// 解回 meta。**解不出来时返回 null 而不是抛**：一条坏存档不该让 App 起不来。
  LlmConvMeta? decodeMeta() {
    try {
      return LlmConvMeta.fromJson(jsonDecode(metaJson));
    } on Object {
      return null;
    }
  }
}

/// events 表里的一行 = 一个**定稿条目**。
final class StoredItem {
  const StoredItem({
    required this.turn,
    required this.blockId,
    required this.role,
    required this.payloadJson,
  });

  final int turn;
  final int blockId;
  final ChatRole role;

  /// 条目本体，见 [encodeItemPayload]。
  final String payloadJson;
}

/// outbox 行的送达状态。
///
/// **三态不是两态**：少了 [uncertain] 就只能在「无限自动重发」和「静默丢弃」之间二选一，
/// 两个都违反 §14-6 无声降级禁令。
enum OutboxState {
  /// 还没成功写进 socket ⇒ 被控端**一定没收到**，重连后自动重发是安全的。
  queued,

  /// 已写进 socket，等 `TurnStarted` 回带确认。**中途断线时无法确知对方收没收到**。
  sent,

  /// 自动重发够了次数仍没等到回带确认 ⇒ **停止自动重发**，交给用户决定。
  ///
  /// UI 必须说出「对方可能已经收到过」——用户点重发时会用一个**新的** `clientMsgId`
  /// （那是用户明确表示「我要再发一次」，不再是去重范畴）。
  uncertain,
}

/// outbox 里的一条待送 / 待确认消息。
final class OutboxEntry {
  const OutboxEntry({
    required this.clientMsgId,
    required this.convId,
    required this.text,
    required this.createdMs,
    this.attempts = 0,
    this.state = OutboxState.queued,
  });

  /// **幂等键，同时是主键**。被控端按它去重（`LlmFrame.Send.clientMsgId`），
  /// 并随 `TurnStarted` 原样回带。
  final String clientMsgId;

  final int convId;
  final String text;
  final int createdMs;

  /// 已尝试送出几次。到 [kOutboxMaxAttempts] 仍未确认就转 [OutboxState.uncertain]。
  final int attempts;

  final OutboxState state;

  OutboxEntry copyWith({int? attempts, OutboxState? state}) => OutboxEntry(
        clientMsgId: clientMsgId,
        convId: convId,
        text: text,
        createdMs: createdMs,
        attempts: attempts ?? this.attempts,
        state: state ?? this.state,
      );

  @override
  String toString() =>
      'OutboxEntry($clientMsgId, ${state.name}, 第 $attempts 次, ${text.length} 字符)';
}

/// 自动重发的上限。到点转 [OutboxState.uncertain]，**不再自动发**。
///
/// 为什么要有上限：被控端的去重表只有 32 条且会淘汰（`LLM_CLIENT_MSG_DEDUP_MAX`），
/// 一条永远等不到确认的消息若无限重发，早晚会在某次重连时越过那个窗口而被真的执行第二遍。
/// 3 次覆盖「弱网连抖两下」，再多就该让用户知道了。
const int kOutboxMaxAttempts = 3;

/// 持久化接口。**所有方法都可能因磁盘故障抛异常**，调用方自己决定降级策略。
abstract interface class ChatStore {
  /// 打开 / 建表。**必须先 await 它**再用其余方法。
  Future<void> open();

  // ── conversations ──────────────────────────────────────────────────────────

  Future<StoredConv?> loadConv(int convId);

  Future<void> saveConv(StoredConv conv);

  // ── events ─────────────────────────────────────────────────────────────────

  /// 按 `(turn, block_id)` 升序取回一条对话的全部条目。
  Future<List<StoredItem>> loadItems(int convId);

  /// 插入或覆盖若干条目（主键 `(conv_id, turn, block_id)`）。
  Future<void> upsertItems(int convId, List<StoredItem> items);

  /// 删掉一整轮——`TurnSnapshot` 是权威，覆盖重建前要先清干净。
  ///
  /// **不能只 upsert 不删**：快照里少了某个块（被控端判定它不该存在）时，
  /// 只 upsert 会把那个块永远留在本地，表现为「历史里多出一个别处没有的气泡」。
  Future<void> deleteTurn(int convId, int turn);

  /// 清空一条对话的全部条目（代次前进时用）。
  Future<void> clearItems(int convId);

  // ── outbox ─────────────────────────────────────────────────────────────────

  /// 入队一条待送消息。主键冲突即覆盖（重发同一条时不产生第二行）。
  Future<void> enqueueOutbox(OutboxEntry entry);

  /// 取回一条对话里所有**还没销账**的消息，按 `created_ms` 升序（发送顺序）。
  Future<List<OutboxEntry>> loadOutbox(int convId);

  /// 更新一条 outbox 行的尝试次数 / 状态。行不存在时**静默忽略**（它可能刚被销账）。
  Future<void> updateOutbox(String clientMsgId,
      {int? attempts, OutboxState? state});

  /// 销账：被控端已确认收到（`TurnStarted` 回带了这个 id）。
  Future<void> removeOutbox(String clientMsgId);

  Future<void> close();
}

// ── 条目载荷的编解码 ─────────────────────────────────────────────────────────
//
// 存的是**协议原文**（`LlmBlockEntry` 的 JSON），不是 `ChatItem` 的逆序列化。
//
// 这样做的收益：编解码复用协议层（golden 语料守着的那一层），重建复用
// `ConversationController` 里已经在用的 `_itemOf`——两条已测通路，不新增第三条映射。
// 代价是 `ChatGap` 没有对应的协议块，得单独开一个信封形态。

/// 载荷信封的判别键。
const String _kKind = 't';

/// 普通块：整条 `LlmBlockEntry`。
const String _kBlock = 'b';

/// 内容缺口（`ChatGap`）：只有字节数。
const String _kGap = 'g';

/// 把一个条目编码成落库载荷。
///
/// [source] 是产生这个条目的协议块；从流式末块拼出来的文本条目由调用方合成一个。
/// **[ChatGap] 必须走 [encodeGapPayload]**——它在协议里没有对应的块。
String encodeItemPayload(LlmBlockEntry source) =>
    jsonEncode(<String, Object?>{_kKind: _kBlock, 'e': source.toJson()});

/// 缺口条目的载荷。
///
/// 缺口**必须落库**：跳过它等于让用户重开 App 后读到一段「看起来完整、其实少了中间几句」
/// 的回复，且再也没有任何迹象（§14-6 无声降级禁令）。
String encodeGapPayload(int bytes) =>
    jsonEncode(<String, Object?>{_kKind: _kGap, 'n': bytes});

/// 解出来的载荷：要么是一个协议块，要么是一个缺口。
sealed class ItemPayload {
  const ItemPayload();
}

final class BlockPayload extends ItemPayload {
  const BlockPayload(this.entry);
  final LlmBlockEntry entry;
}

final class GapPayload extends ItemPayload {
  const GapPayload(this.bytes);
  final int bytes;
}

/// 解码一条载荷。**解不出来时返回 null**（跳过这一条，别让一行坏数据顶掉整段历史）。
ItemPayload? decodeItemPayload(String json) {
  try {
    final JsonMap m = asJsonMap(jsonDecode(json), 'ItemPayload');
    return switch (m[_kKind]) {
      _kBlock => BlockPayload(LlmBlockEntry.fromJson(m['e'])),
      _kGap => GapPayload(readInt(m, 'n')),
      _ => null,
    };
  } on Object {
    return null;
  }
}
