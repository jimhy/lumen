/// **发件箱**：乐观发送 + 去重重发 + 销账（片 10）。
///
/// ## 一句话
///
/// 用户点发送 → 气泡立刻出现 → 落库 → 尝试送出 → 等被控端把 `clientMsgId` 回带回来 → 销账。
///
/// ## ★ 三条不变量（写错了症状都是「消息凭空多一条或少一条」）
///
/// 1. **先落库、再发帧。** 反过来的话，「发出去了但没落库」这一瞬间进程被杀，
///    消息就永远消失了——而用户明明看见气泡出现过。
/// 2. **重发一律带原来的 `clientMsgId`。** 被控端按它去重（`LLM_CLIENT_MSG_DEDUP_MAX`）。
///    换一个新 id 重发 = 主动制造一条重复消息。
/// 3. **自动重发有上限**（[kOutboxMaxAttempts]），到点转 [OutboxState.uncertain] 并停手。
///    被控端的去重表只有 32 条且会淘汰，无限重发早晚会越过那个窗口而被真的执行第二遍。
///
/// ## ★ 为什么「送出去了」不等于「送到了」
///
/// `WsClient.send` 返回 true 只表示**写进了 socket**。中继服务器可能还没把它转给 PC，
/// 手机就断线了。所以 [OutboxState.sent] 是「不确定」而不是「成功」——
/// 唯一的成功信号是 `TurnStarted.clientMsgId` 原样回带（见协议那两个字段的文档）。
///
/// ## 时序：销账与重试是并发的
///
/// 「收到回带确认」与「重连后重发」可能在同一毫秒发生。所以：
/// - 销账走 `removeOutbox`，重试走 `updateOutbox`，后者对**不存在的行静默忽略**
///   （`ChatStore` 契约里专门有一条断言钉着）；
/// - 内存里的 [pending] 以 `clientMsgId` 为键，重复销账是幂等的。
library;

import 'dart:async';

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/domain/chat_item.dart';

const String _tag = 'outbox';

/// 生成一个幂等键。**必须全局唯一**——它是被控端去重表的键。
typedef ClientMsgIdGen = String Function();

/// 把一条消息真正送出去。返回 false = 没连上、这帧被丢弃。
typedef OutboxSender = bool Function(OutboxEntry entry);

/// 发件箱。**不持有 UI，也不认识协议帧**——送出去这一步由 [OutboxSender] 注入。
final class Outbox {
  Outbox({
    required this.convId,
    required ChatStore store,
    required OutboxSender send,
    required ClientMsgIdGen newId,
  })  : _store = store,
        _send = send,
        _newId = newId;

  final int convId;
  final ChatStore _store;
  final OutboxSender _send;
  final ClientMsgIdGen _newId;

  /// 在途消息，`clientMsgId → 行`。**保持插入顺序**（Dart 的 Map 是有序的），
  /// 于是渲染顺序就是发送顺序。
  final Map<String, OutboxEntry> _entries = <String, OutboxEntry>{};

  /// 每条在途消息的**稳定** blockId（在 [kPendingTurn] 这一「轮」内递增）。
  ///
  /// ★ **不能用列表下标**：销账会让后面的下标整体前移，于是一条还在途的气泡会突然换一个
  /// key，`ListView` 的 `ValueKey` 与 Markdown memo 缓存一起失效——表现为「前面那条一被
  /// 确认，后面那条就闪一下」。这种 bug 只在连发两条且第一条先确认时出现。
  final Map<String, int> _blockIds = <String, int>{};

  int _nextBlockId = 0;

  /// 当前在途的消息，按发送顺序。
  List<OutboxEntry> get entries => _entries.values.toList(growable: false);

  bool get isEmpty => _entries.isEmpty;

  /// 渲染用的乐观气泡。
  List<ChatPending> pendingItems() => _entries.values
      .map((OutboxEntry e) => ChatPending(
            blockId: _blockIds[e.clientMsgId] ?? 0,
            clientMsgId: e.clientMsgId,
            text: e.text,
            state: _deliveryOf(e.state),
          ))
      .toList(growable: false);

  /// 从库里读回上次没送完的消息（App 启动 / 会话重建时）。
  ///
  /// **只读不发**：什么时候重发是调用方的决定（要等会话真的建立起来）。
  Future<void> restore() async {
    final List<OutboxEntry> rows = await _store.loadOutbox(convId);
    _entries.clear();
    _blockIds.clear();
    _nextBlockId = 0;
    for (final OutboxEntry e in rows) {
      _entries[e.clientMsgId] = e;
      _blockIds[e.clientMsgId] = _nextBlockId++;
    }
    if (rows.isNotEmpty) {
      logInfo(_tag, '恢复了 ${rows.length} 条未确认的消息');
    }
  }

  /// 用户点了发送。返回这条消息的 `clientMsgId`。
  ///
  /// **不 await 送出**：落库是要等的（不变量 1），送出是尽力而为。
  Future<String> submit(String text, {required int nowMs}) async {
    final OutboxEntry entry = OutboxEntry(
      clientMsgId: _newId(),
      convId: convId,
      text: text,
      createdMs: nowMs,
    );
    // 不变量 1：先落库。
    await _store.enqueueOutbox(entry);
    _entries[entry.clientMsgId] = entry;
    _blockIds[entry.clientMsgId] = _nextBlockId++;
    await _attempt(entry);
    return entry.clientMsgId;
  }

  /// 会话（重新）建立后，把还没确认的都再送一遍。
  ///
  /// 重发的是**同一个 `clientMsgId`**（不变量 2），被控端负责去重。
  Future<void> flush() async {
    for (final OutboxEntry entry in entries) {
      if (entry.state == OutboxState.uncertain) {
        // 不变量 3：到顶了就不再自动发，等用户拍板。
        continue;
      }
      await _attempt(entry);
    }
  }

  /// 被控端确认收到了（`TurnStarted` 回带了这个 id）⇒ 销账。
  ///
  /// 对不认识的 id **静默返回 false**：那是别的控制端发的消息，或本端上一次安装留下的。
  Future<bool> ack(String clientMsgId) async {
    if (!_entries.containsKey(clientMsgId)) return false;
    _entries.remove(clientMsgId);
    _blockIds.remove(clientMsgId);
    await _store.removeOutbox(clientMsgId);
    logInfo(_tag, '消息已确认送达：$clientMsgId');
    return true;
  }

  /// 用户手动重发一条 [OutboxState.uncertain] 的消息。
  ///
  /// ★ **用新的 `clientMsgId`**：这是用户明确表示「我要再发一次」，不再是去重范畴。
  /// 沿用旧 id 会被被控端当成重复丢掉——用户点了重发却什么也没发生，是最糟的一种响应。
  Future<String?> resendByUser(String clientMsgId, {required int nowMs}) async {
    final OutboxEntry? old = _entries[clientMsgId];
    if (old == null) return null;
    await discard(clientMsgId);
    return submit(old.text, nowMs: nowMs);
  }

  /// 用户放弃一条消息（不再重试）。
  Future<void> discard(String clientMsgId) async {
    _entries.remove(clientMsgId);
    _blockIds.remove(clientMsgId);
    await _store.removeOutbox(clientMsgId);
  }

  /// 送一次，并把结果记回库与内存。
  Future<void> _attempt(OutboxEntry entry) async {
    final int attempts = entry.attempts + 1;
    final bool ok = _send(entry);
    // 送不出去 ⇒ 停留在 queued：被控端**一定没收到**，重连后重发是安全的，
    // 而且这一次不该计入尝试次数（根本没发生过尝试）。
    if (!ok) {
      logWarn(_tag, '没连上，${entry.clientMsgId} 留在队列里等重连');
      _update(entry.copyWith(state: OutboxState.queued));
      return;
    }
    final OutboxState next = attempts >= kOutboxMaxAttempts
        ? OutboxState.uncertain
        : OutboxState.sent;
    if (next == OutboxState.uncertain) {
      logWarn(
        _tag,
        '${entry.clientMsgId} 送了 $attempts 次仍未收到确认，转「送达状态未知」交给用户',
      );
    }
    _update(entry.copyWith(attempts: attempts, state: next));
    await _store.updateOutbox(entry.clientMsgId,
        attempts: attempts, state: next);
  }

  void _update(OutboxEntry next) {
    // 只在还没被销账时写回：`ack` 可能在 `_attempt` 的 await 之间跑完。
    if (_entries.containsKey(next.clientMsgId)) {
      _entries[next.clientMsgId] = next;
    }
  }

  static OutboxDelivery _deliveryOf(OutboxState s) => switch (s) {
        OutboxState.queued => OutboxDelivery.queued,
        OutboxState.sent => OutboxDelivery.sent,
        OutboxState.uncertain => OutboxDelivery.uncertain,
      };
}
