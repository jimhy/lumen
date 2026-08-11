/// 流式对话的归约器：协议帧 → 列表条目。
///
/// ## 三条不变量（写错了症状极难复现，逐条都有测试钉着）
///
/// 1. **幂等**：`seq <= lastSeq` 直接丢。补齐与实时**会**重叠——`Attach` 之后被控端会把
///    水位线之后的增量重发一遍，而实时流并没有停。
/// 2. **空洞即补齐**：`seq > lastSeq + 1` 时发 `Attach{knownSeq: lastSeq}` 并 **return**。
///    **不要先应用再补**——那会让 UI 先显示一段错位内容，用户看到的是一段读起来通顺、
///    实际上少了中间几句的回复。
/// 3. **先拼接、再交渲染层做字素簇切分**（在 `streaming_tail.dart` 里）：增量可能把一个
///    ZWJ emoji 拆两半，两半各自都是合法字符串，协议层无从阻止。
///
/// ## ★ seq 的严格性**只对 Delta 成立**（这条是被真实语料推翻一次之后才写对的）
///
/// 最初按 golden 语料的连号推断「所有带 seq 的帧共用一套严格水位线」。
/// **真实回放语料把它证伪了**：`replay/sample_a_llmframes.jsonl` 里
/// `Delta(seq=1)` 之后紧跟 `TurnEnded(seq=1)`——**同号**。
/// 按严格幂等（`seq <= lastSeq` 丢）会把 TurnEnded 整条吃掉，症状是
/// 「一轮永远结束不了」：中断按钮一直亮着、轮末的 usage 永远刷不上去。
///
/// 现在的规则：
///
/// | 帧 | 判据 | 理由 |
/// |---|---|---|
/// | `Delta` | `seq <= lastSeq` 丢 + 空洞补齐 | 它的文档明写「严格递增且连续」，且重复应用会让文字翻倍 |
/// | 其余带 seq 的帧 | 只丢 `seq < lastSeq` | 它们的 seq 是**参照点**（「本轮到这个水位为止」），不占独立号 |
///
/// 重复到达的 TurnEnded 会被重复应用一次（多发一次 TurnFetch），
/// 那比丢掉整条 TurnEnded 轻得多。
///
/// ## 「正在思考…」只做状态指示
///
/// Claude 的 `thinking` 块正文**恒为空串**（Opus 5 默认 `display=omitted`，CLI 无开关），
/// 所以可折叠思考块在 Claude 上永远是个**点开什么都没有的空盒子**。这里把它规约成一个
/// `thinking` 布尔，**不进 items、不落库**——它是瞬时状态，不是内容。
///
/// ## 额度是**账号级**的，不是每个对话一份
///
/// `RateLimit` 帧刻意没有 `seq`（与增量流无序），用 `observedMs` 做 latest-wins。
/// 它的 `convId` 只表示「在哪条流上观测到」。
library;

import 'dart:async';
import 'dart:convert';

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/data/chat_store.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/net/scheduler.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/outbox.dart';
import 'package:lumen_mobile/state/streaming_tail.dart';

const String _tag = 'conv';

/// 水位线的落库节流窗口（片 10）。
///
/// ## ★ 水位线**宁可偏低，不可偏高**
///
/// 流式期间 `lastSeq` 每 33 ms 就变一次，每次都写库等于给手机加一个 30 Hz 的磁盘写。
/// 所以按秒节流——代价是崩溃时最多丢 1 秒的水位线推进。
///
/// 这个代价之所以可接受，是因为**方向是安全的**：水位线偏低 ⇒ 重连时 `Attach{known_seq}`
/// 会多补一段已经有的内容，而归约器的幂等规则（`seq <= lastSeq` 丢）正好把它吸收掉。
/// 反过来「偏高」才是灾难：那一段内容永远补不回来，两端都没有任何迹象。
///
/// ⇒ 所以只在**内容真的落库之后**才推水位线，绝不能反过来。
const Duration kWatermarkFlushWindow = Duration(seconds: 1);

/// 把一帧发给被控端。返回 false = 没连上、这帧被丢弃。
typedef LlmFrameSender = bool Function(LlmFrame frame);

/// 「一帧都还没收到」的水位线哨兵。
///
/// ★ **不能用 0**：被控端的第一帧 seq 就是 **0**（实测回放语料里 `TurnStarted` 的
/// seq 是 0），拿 0 当初值会让幂等规则 `seq <= lastSeq` 把整条对话的第一帧丢掉，
/// 而后续帧又会因为「空洞」触发一次白跑的补齐。
///
/// 这个 bug 在手工构造的测试里看不见——那些用例的 seq 都从 100 起。
/// 它是**真实语料回放**第一次跑就撞出来的。
const int kNoSeqYet = -1;

/// 对话的当前快照（不含高频变化的末块，那在 [ConversationController.tail]）。
final class ConversationSnapshot {
  const ConversationSnapshot({
    this.items = const <ChatItem>[],
    this.pending = const <ChatPending>[],
    this.lastSeq = kNoSeqYet,
    this.meta,
    this.thinking = false,
    this.rateLimit,
    this.turnRunning = false,
    this.blocksOmitted = 0,
  });

  /// 定稿条目，按 `(turn, blockId)` 升序。
  final List<ChatItem> items;

  /// 乐观发送中、还没被确认的用户消息（片 10）。
  ///
  /// **与 [items] 分开存**：它们的生命周期完全不同（被确认后消失，由 `TurnStarted.user`
  /// 里的真实块顶上），而且它们**不落 events 表**——outbox 表才是它们的家。
  /// 混进 `items` 会让「归约器只处理协议帧」这条边界破掉，测试也就没法单独断言归约结果。
  final List<ChatPending> pending;

  /// 渲染用的完整列表：定稿在前，乐观气泡在后（[kPendingTurn] 保证它们排在最后）。
  List<ChatItem> get visibleItems =>
      pending.isEmpty ? items : <ChatItem>[...items, ...pending];

  /// 已消费到的增量水位线。[kNoSeqYet] = 一帧都还没收到。
  final int lastSeq;

  /// 对话元信息（模型 / 用量 / 运行态）。null = 还没收到基线帧。
  final LlmConvMeta? meta;

  /// 是否正在思考。**只做状态指示**，见库文档。
  final bool thinking;

  /// 账号级额度状态。null = 还没收到过任何额度信息 ⇒ 额度条整条不显示。
  final LlmRateLimit? rateLimit;

  /// 是否有一轮正在跑（`TurnStarted` 到 `TurnEnded` 之间）。
  final bool turnRunning;

  /// 快照声明「有 N 块因过大未能同步」。>0 时 UI **必须**说出来。
  final int blocksOmitted;

  /// 上下文占用百分比。null = 不显示（分母未知）。
  ///
  /// **`contextLimit == 0` 时返回 null**，且**绝不**按模型名去查一张硬编码的窗口表来「补」
  /// 这个值——猜出来的百分比比不显示更糟，用户没法判断它准不准。
  double? get contextPercent {
    final LlmUsage? usage = meta?.usage;
    if (usage == null || usage.contextLimit <= 0) return null;
    return usage.contextUsed / usage.contextLimit;
  }

  ConversationSnapshot copyWith({
    List<ChatItem>? items,
    List<ChatPending>? pending,
    int? lastSeq,
    LlmConvMeta? meta,
    bool? thinking,
    LlmRateLimit? rateLimit,
    bool? turnRunning,
    int? blocksOmitted,
  }) =>
      ConversationSnapshot(
        items: items ?? this.items,
        pending: pending ?? this.pending,
        lastSeq: lastSeq ?? this.lastSeq,
        meta: meta ?? this.meta,
        thinking: thinking ?? this.thinking,
        rateLimit: rateLimit ?? this.rateLimit,
        turnRunning: turnRunning ?? this.turnRunning,
        blocksOmitted: blocksOmitted ?? this.blocksOmitted,
      );

  @override
  String toString() => 'ConversationSnapshot(${items.length} 条 + '
      '${pending.length} 在途, seq=$lastSeq, thinking=$thinking)';
}

/// 一条对话的归约器。
final class ConversationController {
  ConversationController({
    required this.convId,
    required LlmFrameSender send,
    Scheduler scheduler = const RealScheduler(),
    ChatStore? store,
    Outbox? outbox,
  })  : _send = send,
        _store = store,
        _outbox = outbox,
        _watermark = TimerSlot(scheduler),
        tail = StreamingTail(scheduler: scheduler);

  /// 本控制器负责的对话。
  final int convId;

  final LlmFrameSender _send;

  /// 本地存档。**null = 不落库**——归约逻辑的单测大多不需要库，
  /// 而真机上 `openDatabase` 失败时也要能退化成「这次不落库，对话照常」。
  final ChatStore? _store;

  /// 发件箱。null = 本对话只读（还没接输入框时就是这样）。
  final Outbox? _outbox;

  /// 水位线的节流写盘槽，见 [kWatermarkFlushWindow]。
  final TimerSlot _watermark;

  /// 有没有还没写下去的水位线 / meta 变化。
  bool _convDirty = false;

  /// 流式末块。**高频变化，与 [snapshots] 分开订阅**，否则每次增量都会重建整表。
  final StreamingTail tail;

  final StreamController<ConversationSnapshot> _snapshots =
      StreamController<ConversationSnapshot>.broadcast();

  ConversationSnapshot _state = const ConversationSnapshot();

  /// 当前对话代次。收到代次更高的基线帧时清空重建。
  int _generation = 0;

  /// 末块所属的轮号（定稿时要用）。
  int _tailTurn = 0;

  /// 额度的观测时刻，latest-wins 的判据。
  int _rateLimitObservedMs = -1;

  Stream<ConversationSnapshot> get snapshots => _snapshots.stream;

  ConversationSnapshot get state => _state;

  /// 当前对话代次。发件箱装 `Send` 时要用——**它只有这一个权威副本**。
  int get generation => _generation;

  /// 下一个请求号。
  ///
  /// 与桌面端的 `RemoteWs::next_req_id()` 同一个角色：**每个控制端各自**的空间，
  /// 只用来把错误回执关联回请求。**绝不能拿它当幂等键**——被控端的对话表是全端共用的，
  /// 两个控制端的 `req_id` 完全重叠（这正是 `clientMsgId` 存在的理由）。
  int nextReqId() => ++_reqSeq;

  int _reqSeq = 0;

  /// 喂一帧进来。**这是唯一入口。**
  void applyFrame(LlmFrame frame) {
    switch (frame) {
      // ── 基线帧：建立/重建水位线 ────────────────────────────────────────────
      case LlmAttached(:final LlmConvMeta meta, :final int seq, :final bool resyncRequired):
        _applyBaseline(meta, seq, resync: resyncRequired);
      case LlmConvStarted(:final LlmConvMeta meta, :final int seq):
        _applyBaseline(meta, seq, resync: false);

      // ── 增量流 ──────────────────────────────────────────────────────────
      case LlmDelta():
        // 只有 Delta 走严格判据：它是唯一「严格递增且连续」的流，
        // 也是唯一重复应用会造成可见错误（文字翻倍）的帧。
        if (!_admit(frame.seq, frame.convGeneration, strict: true)) return;
        _applyDelta(frame);
      case LlmTurnStarted():
        if (!_admit(frame.seq, frame.convGeneration)) return;
        _applyTurnStarted(frame);
      case LlmTurnEnded():
        if (!_admit(frame.seq, frame.convGeneration)) return;
        _applyTurnEnded(frame);
      case LlmTurnSnapshot():
        if (!_admit(frame.seq, frame.convGeneration)) return;
        _applySnapshot(frame.record);
      case LlmConvExited():
        if (!_admit(frame.seq, frame.convGeneration)) return;
        _emit(_state.copyWith(turnRunning: false, thinking: false));

      // ── 无 seq 的旁路 ────────────────────────────────────────────────────
      case LlmRateLimitFrame():
        _applyRateLimit(frame.observedMs, frame.info);
      case LlmHistoryPage(:final List<LlmTurnRecord> turns):
        for (final LlmTurnRecord record in turns) {
          _applySnapshot(record, sortAfter: false);
        }
        _emit(_state.copyWith(items: _sorted(_state.items)));
      case LlmError(:final OpenEnum<LlmErrorCodeKnown> code, :final LlmText message):
        logWarn(_tag, '对话出错：${code.wire} ${message.value}');

      // 其余帧与渲染无关（Hello/HelloAck 等在会话层处理）。
      default:
        break;
    }
  }

  /// 发一条用户消息（片 10）。没有发件箱时返回 null（本对话只读）。
  ///
  /// 气泡**立刻**出现（乐观），送达状态由 [ChatPending.state] 画出来。
  Future<String?> submit(String text, {required int nowMs}) async {
    final Outbox? outbox = _outbox;
    if (outbox == null) {
      logWarn(_tag, '本对话没有发件箱，消息未发出');
      return null;
    }
    final String id = await outbox.submit(text, nowMs: nowMs);
    _emitPending();
    return id;
  }

  /// 挂载到被控端并请求补齐离线期间的增量（片 10）。
  ///
  /// **必须在 [restore] 之后调用**：`knownSeq` 用的是恢复出来的水位线，
  /// 早一步调就会从 0 开始补——被控端要么整段重发（浪费），要么已经淘汰了那段
  /// transcript 而回一个 `resync_required`（用户看到历史被整个重建）。
  bool attach() => _send(LlmAttach(
        convId: convId,
        knownGeneration: _generation,
        knownSeq: _wireSeq(_state.lastSeq),
      ));

  /// 会话（重新）建立后：把还没确认的消息再送一遍。
  ///
  /// 重发带的是**原来的** `clientMsgId`，被控端按它去重（协议 `Send.client_msg_id`）。
  Future<void> flushOutbox() async {
    final Outbox? outbox = _outbox;
    if (outbox == null) return;
    await outbox.flush();
    _emitPending();
  }

  /// 用户手动重发一条「送达状态未知」的消息。**用新 id**，见 [Outbox.resendByUser]。
  Future<void> resendByUser(String clientMsgId, {required int nowMs}) async {
    await _outbox?.resendByUser(clientMsgId, nowMs: nowMs);
    _emitPending();
  }

  /// 用户放弃一条消息。
  Future<void> discardPending(String clientMsgId) async {
    await _outbox?.discard(clientMsgId);
    _emitPending();
  }

  /// 从本地存档恢复：历史条目 + 水位线 + 未确认的消息。
  ///
  /// **必须在开始喂帧之前调用**（`ConversationSession` 保证这个顺序）。它自带一道保护：
  /// 已经收到过基线帧就整个跳过——在线数据永远是权威，恢复出来的历史不许覆盖它。
  Future<void> restore() async {
    final ChatStore? store = _store;
    if (store == null) return;
    if (_state.meta != null) {
      logWarn(_tag, '已经有在线数据了，跳过本地恢复');
      return;
    }
    try {
      final StoredConv? conv = await store.loadConv(convId);
      await _outbox?.restore();
      if (conv == null) {
        _emitPending();
        return;
      }
      final List<StoredItem> rows = await store.loadItems(convId);
      final List<ChatItem> items = <ChatItem>[];
      for (final StoredItem row in rows) {
        final ChatItem? item = _itemFromStored(row);
        // 解不出来的那一条**跳过**、其余照常——一行坏数据不该顶掉整段历史。
        if (item != null) items.add(item);
      }
      _generation = conv.generation;
      _emit(ConversationSnapshot(
        items: items,
        pending: _outbox?.pendingItems() ?? const <ChatPending>[],
        lastSeq: conv.lastSeq,
        meta: conv.decodeMeta(),
        rateLimit: _state.rateLimit,
      ));
      logInfo(_tag, '从本地恢复了 ${items.length} 条历史（水位 ${conv.lastSeq}）');
    } on Object catch (e) {
      // 存档读不出来 ⇒ 当作没有历史继续跑。**不能让它把对话页整个挡住。**
      logWarn(_tag, '本地存档恢复失败，按空历史继续', error: e);
    }
  }

  /// 把攒着的水位线立刻写下去（进后台 / 断开 / 退出前调用）。
  Future<void> flushPersistence() async {
    _watermark.cancel();
    await _writeConv();
  }

  Future<void> dispose() async {
    _watermark.cancel();
    // dispose 是最后一次落库的机会：节流窗口里还攒着的水位线丢了，
    // 下次重连就要多补一段（安全但浪费）。
    await _writeConv();
    tail.dispose();
    await _snapshots.close();
  }

  // ── 不变量 ─────────────────────────────────────────────────────────────────

  /// 幂等 + 空洞补齐。返回 false = 这一帧不该被应用。
  ///
  /// [strict] 只对 `Delta` 为真，理由见库文档那张表。
  bool _admit(int seq, int generation, {bool strict = false}) {
    if (generation != _generation) {
      // 代次变了说明被控端重建了对话（进程重启等）。**丢弃旧代次的迟到帧**，
      // 应用它们会把新对话污染成两条对话的混合体。
      if (generation < _generation) {
        logDebug(_tag, '丢弃旧代次帧（$generation < $_generation）');
        return false;
      }
      logWarn(_tag, '代次前进（$_generation → $generation），清空重建');
      _resetForGeneration(generation);
    }
    // 不变量 1：幂等。非 Delta 帧的 seq 是参照点、可与最后一条 Delta 同号，
    // 用 <= 会把 TurnEnded 吃掉（真实语料实证，见库文档）。
    if (strict ? seq <= _state.lastSeq : seq < _state.lastSeq) return false;
    if (strict && seq > _state.lastSeq + 1) {
      // 不变量 2：**先补齐再说**，不能先应用。
      logWarn(_tag, '增量出现空洞（收到 $seq，本地 ${_state.lastSeq}），请求补齐');
      attach();
      return false;
    }
    return true;
  }

  // ── 各帧的处理 ─────────────────────────────────────────────────────────────

  void _applyBaseline(LlmConvMeta meta, int seq, {required bool resync}) {
    if (resync || meta.convGeneration != _generation) {
      _resetForGeneration(meta.convGeneration);
    }
    _generation = meta.convGeneration;
    _emit(_state.copyWith(meta: meta, lastSeq: seq));
  }

  void _applyDelta(LlmDelta frame) {
    for (final LlmDeltaItem item in frame.items) {
      switch (item) {
        case LlmDeltaBlockStart(:final LlmBlockEntry entry):
          _onBlockStart(frame.turn, entry);
        case LlmDeltaTextAppend(:final int blockId, :final LlmText text):
          _tailTurn = frame.turn;
          tail.append(blockId, text.value);
        case LlmDeltaBlockEnd(:final int blockId, :final LlmBlock? block):
          _onBlockEnd(frame.turn, blockId, block);
        case LlmDeltaDropped(:final int blockId, :final int bytes):
          // 让降级可见：画成「内容有缺口，点击补齐」。
          // 缺口**也要落库**：不落的话重开 App 后读到的是一段「看起来完整、其实少了
          // 中间几句」的回复，且再无任何迹象（§14-6 无声降级禁令）。
          _upsert(
            ChatGap(
              turn: frame.turn,
              blockId: blockId,
              role: ChatRole.assistant,
              bytes: bytes,
            ),
            payload: encodeGapPayload(bytes),
          );
        case LlmDeltaUnknown():
          break; // 前向兼容：同帧其余项照常。
      }
    }
    _emit(_state.copyWith(lastSeq: frame.seq));
  }

  void _onBlockStart(int turn, LlmBlockEntry entry) {
    // 思考块只翻状态位，**不进 items**——Claude 上它的正文恒为空串。
    if (entry.block is LlmBlockThinking) {
      _emit(_state.copyWith(thinking: true));
      return;
    }
    // 文本块的正文靠 TextAppend 流进来，这里先不落条目（落了会与末块重复画一份）。
    if (entry.block is LlmBlockText) return;
    _upsert(_itemOf(turn, entry, ChatRole.assistant),
        payload: encodeItemPayload(entry));
  }

  void _onBlockEnd(int turn, int blockId, LlmBlock? block) {
    if (block is LlmBlockThinking || (block == null && _state.thinking)) {
      // 思考结束：状态位落下，不留任何条目。
      _emit(_state.copyWith(thinking: false));
      if (block is LlmBlockThinking) return;
    }
    final String tailText = tail.blockId == blockId ? tail.takeAndClear() : '';
    if (block != null) {
      // 被控端声明本块降级过，带来了终态：**用它覆盖**本地拼接结果。
      final LlmBlockEntry entry = LlmBlockEntry(blockId: blockId, block: block);
      _upsert(_itemOf(turn, entry, ChatRole.assistant),
          payload: encodeItemPayload(entry));
      return;
    }
    if (tailText.isEmpty) return; // 非文本块的 BlockEnd，条目在 BlockStart 时已落。
    _commitText(turn, blockId, tailText);
  }

  /// 把流式拼接出来的文本定稿成条目并落库。
  ///
  /// 落库时**合成**一个协议块（`LlmBlock::Text`）而不是给 `ChatText` 写一套自己的序列化：
  /// 存的是协议原文，重建走的是 `_itemOf` —— 两条已经被 golden 语料与现有测试守着的通路，
  /// 不新增第三条映射。
  void _commitText(int turn, int blockId, String markdown) {
    final LlmBlockEntry entry = LlmBlockEntry(
      blockId: blockId,
      block: LlmBlockText(LlmText(markdown)),
    );
    _upsert(
      ChatText(
        turn: turn,
        blockId: blockId,
        role: ChatRole.assistant,
        markdown: markdown,
      ),
      payload: encodeItemPayload(entry),
    );
  }

  void _applyTurnStarted(LlmTurnStarted frame) {
    for (final LlmBlockEntry entry in frame.user) {
      _upsert(_itemOf(frame.turn, entry, ChatRole.user),
          payload: encodeItemPayload(entry));
    }
    _emit(_state.copyWith(lastSeq: frame.seq, turnRunning: true, thinking: false));
    // ★ 销账：这一轮是我发的那条消息触发的 ⇒ 乐观气泡功成身退，
    // `frame.user` 里的真实块顶上来（上面那个循环刚落的就是它）。
    final String? acked = frame.clientMsgId;
    if (acked != null) {
      _fireAndForget(() async {
        if (await _outbox?.ack(acked) ?? false) _emitPending();
      });
    }
  }

  void _applyTurnEnded(LlmTurnEnded frame) {
    // 末块可能还没等到 BlockEnd 就轮结束了（被控端崩了/被中断），先定稿再收尾。
    final String leftover = tail.takeAndClear();
    if (leftover.isNotEmpty) {
      _commitText(_tailTurn, tail.blockId ?? 0, leftover);
    }
    final LlmConvMeta? meta = _state.meta;
    _emit(_state.copyWith(
      // 同号帧不该让水位线倒退。
      lastSeq: frame.seq > _state.lastSeq ? frame.seq : _state.lastSeq,
      turnRunning: false,
      thinking: false,
      // 轮末的 usage 是**真值**（分母来自上游的 contextWindow，不是我们猜的）。
      meta: meta == null ? null : _withUsage(meta, frame.usage),
    ));
    // 轮末是个天然的落盘点：这一轮的内容全定稿了，且此后可能长时间没有帧
    // （节流窗口靠新事件驱动，闲下来就永远不到期）。
    _fireAndForget(flushPersistence);
    if (frame.truncated) {
      // 本轮曾因积压丢过增量 ⇒ 拉整轮快照覆盖重建。
      logWarn(_tag, '第 ${frame.turn} 轮被截断过，请求整轮快照');
      _send(LlmTurnFetch(
        convId: convId,
        convGeneration: _generation,
        reqId: frame.seq,
        turn: frame.turn,
      ));
    }
  }

  /// 用一轮的定稿记录**整体覆盖**该轮的本地条目。
  void _applySnapshot(LlmTurnRecord record, {bool sortAfter = true}) {
    final List<ChatItem> kept = _state.items
        .where((ChatItem i) => i.turn != record.turn)
        .toList(growable: true);
    final List<StoredItem> rows = <StoredItem>[];
    for (final LlmBlockEntry entry in record.user) {
      kept.add(_itemOf(record.turn, entry, ChatRole.user));
      rows.add(_rowOf(record.turn, entry, ChatRole.user));
    }
    for (final LlmBlockEntry entry in record.assistant) {
      kept.add(_itemOf(record.turn, entry, ChatRole.assistant));
      rows.add(_rowOf(record.turn, entry, ChatRole.assistant));
    }
    if (tail.blockId != null && _tailTurn == record.turn) {
      // 快照是权威，末块的半成品作废——留着会与快照里的定稿块重复。
      tail.clear();
    }
    // ★ 落库也必须是**整轮覆盖**：只 upsert 不删，快照里少掉的那个块会永远留在本地，
    // 表现为「历史里多出一个别处都没有的气泡」。内存里那个 `where(turn != …)` 的
    // 对应物就是这条 DELETE。
    _fireAndForget(() async {
      final ChatStore? store = _store;
      if (store == null) return;
      await store.deleteTurn(convId, record.turn);
      await store.upsertItems(convId, rows);
    });
    _emit(_state.copyWith(
      items: sortAfter ? _sorted(kept) : kept,
      blocksOmitted: record.blocksOmitted,
    ));
  }

  void _applyRateLimit(int observedMs, LlmRateLimit info) {
    // latest-wins：本帧无 seq，与增量流无序，只能按观测时刻比。
    if (observedMs < _rateLimitObservedMs) {
      logDebug(_tag, '丢弃过期的额度播报（$observedMs < $_rateLimitObservedMs）');
      return;
    }
    _rateLimitObservedMs = observedMs;
    _emit(_state.copyWith(rateLimit: info));
  }

  // ── 工具 ───────────────────────────────────────────────────────────────────

  /// 把一个协议块转成列表条目。
  ChatItem _itemOf(int turn, LlmBlockEntry entry, ChatRole role) {
    final int blockId = entry.blockId;
    final String? parent = entry.parentCallId;
    return switch (entry.block) {
      LlmBlockText(:final LlmText text) => ChatText(
          turn: turn,
          blockId: blockId,
          role: role,
          markdown: text.value,
          parentCallId: parent,
        ),
      LlmBlockToolUse(
        :final String callId,
        :final String name,
        :final LlmToolInput input,
        :final LlmText? title,
        :final int? truncatedBytes,
      ) =>
        ChatToolCall(
          turn: turn,
          blockId: blockId,
          role: role,
          callId: callId,
          name: name,
          input: input,
          title: title,
          truncatedBytes: truncatedBytes,
          parentCallId: parent,
        ),
      LlmBlockToolResult(
        :final String callId,
        :final LlmToolStatus status,
        :final LlmText output,
        :final LlmToolResultDetail? detail,
        :final int? truncatedBytes,
      ) =>
        ChatToolResult(
          turn: turn,
          blockId: blockId,
          role: role,
          callId: callId,
          status: status,
          output: output,
          detail: detail,
          truncatedBytes: truncatedBytes,
          parentCallId: parent,
        ),
      LlmBlockImage(:final LlmAttachment attachment) => ChatImage(
          turn: turn,
          blockId: blockId,
          role: role,
          attachment: attachment,
          parentCallId: parent,
        ),
      LlmBlockError(:final OpenEnum<LlmErrorCodeKnown> code, :final LlmText message) =>
        ChatError(
          turn: turn,
          blockId: blockId,
          role: role,
          code: code,
          message: message,
          parentCallId: parent,
        ),
      // 思考块正常不会走到这里（BlockStart 就拦下了）；快照里出现时同样不落条目，
      // 但快照走的是 _itemOf，所以给一个「不显示内容」的占位而不是丢掉整块。
      LlmBlockThinking() => ChatUnknown(
          turn: turn,
          blockId: blockId,
          role: role,
          parentCallId: parent,
        ),
      LlmBlockUnknown() => ChatUnknown(
          turn: turn,
          blockId: blockId,
          role: role,
          parentCallId: parent,
        ),
    };
  }

  /// 插入或覆盖同 key 的条目。覆盖时 revision 自增，memo 缓存据此失效。
  ///
  /// [payload] 是这个条目的落库载荷（见 `data/chat_store.dart`）。**给了才落库**——
  /// 归约器里有几处 upsert 是纯 UI 状态（不该进 events 表）。
  void _upsert(ChatItem item, {String? payload}) {
    final List<ChatItem> next = List<ChatItem>.of(_state.items);
    final int at = next.indexWhere((ChatItem i) => i.key == item.key);
    if (at < 0) {
      next.add(item);
    } else if (item is ChatText && next[at] is ChatText) {
      next[at] = (next[at] as ChatText).bumped(item.markdown);
    } else {
      next[at] = item;
    }
    if (payload != null) {
      final StoredItem row = StoredItem(
        turn: item.turn,
        blockId: item.blockId,
        role: item.role,
        payloadJson: payload,
      );
      _fireAndForget(() async => _store?.upsertItems(convId, <StoredItem>[row]));
    }
    _emit(_state.copyWith(items: next));
  }

  /// 落库行（快照那条路径要一次攒一整轮，不逐条 upsert）。
  StoredItem _rowOf(int turn, LlmBlockEntry entry, ChatRole role) => StoredItem(
        turn: turn,
        blockId: entry.blockId,
        role: role,
        payloadJson: encodeItemPayload(entry),
      );

  /// 从存档行重建一个条目。**解不出来返回 null**（跳过这一条，别丢整段历史）。
  ChatItem? _itemFromStored(StoredItem row) {
    final ItemPayload? payload = decodeItemPayload(row.payloadJson);
    return switch (payload) {
      BlockPayload(:final LlmBlockEntry entry) =>
        _itemOf(row.turn, entry, row.role),
      GapPayload(:final int bytes) => ChatGap(
          turn: row.turn,
          blockId: row.blockId,
          role: row.role,
          bytes: bytes,
        ),
      null => null,
    };
  }

  /// 把当前的乐观气泡推给 UI。
  void _emitPending() =>
      _emit(_state.copyWith(pending: _outbox?.pendingItems() ?? const <ChatPending>[]));

  /// 水位线 + meta 的节流落库。见 [kWatermarkFlushWindow]。
  void _markConvDirty() {
    if (_store == null) return;
    _convDirty = true;
    // 已经在等的那一枪不重置——重置会让「持续不断的流」永远等不到落盘时刻
    // （每次 delta 都把 deadline 往后推 1 秒 = 一轮跑完之前一次都不写）。
    if (_watermark.isActive) return;
    _watermark.set(kWatermarkFlushWindow, () => _fireAndForget(_writeConv));
  }

  Future<void> _writeConv() async {
    final ChatStore? store = _store;
    if (store == null || !_convDirty) return;
    final LlmConvMeta? meta = _state.meta;
    // 还没有 meta 就没什么可存的：`conv_id` 之外一个字段都填不出来，
    // 存一行空壳只会让下次 restore 拿到一个 meta 为 null 的「有效」存档。
    if (meta == null) return;
    _convDirty = false;
    await store.saveConv(StoredConv(
      convId: convId,
      generation: _generation,
      lastSeq: _state.lastSeq,
      metaJson: jsonEncode(meta.toJson()),
      updatedMs: meta.updatedMs,
    ));
  }

  /// 本地水位线 → 线格式。
  ///
  /// 协议里 `knownSeq` 的 0 是「尚未同步」哨兵，**负数不是合法线格式**，
  /// 而本地的 [kNoSeqYet] 恰恰是 -1（那是片 7 为了区分「真的收到过 seq=0」才引入的）。
  /// 少了这一步转换，第一次 `Attach` 会发出 `known_seq: -1`。
  static int _wireSeq(int local) => local < 0 ? 0 : local;

  /// 跑一个不等结果的异步副作用。**异常只记日志**。
  ///
  /// 落库失败不该把归约打断：手机上磁盘满 / 权限被回收是真实存在的，
  /// 那时正确的行为是「这次没存上，对话照常」，而不是对话页整个挂掉。
  void _fireAndForget(Future<void> Function() work) {
    unawaited(work().catchError((Object e, StackTrace s) {
      logWarn(_tag, '本地存档写入失败（对话不受影响）', error: e);
    }));
  }

  List<ChatItem> _sorted(List<ChatItem> items) {
    final List<ChatItem> copy = List<ChatItem>.of(items);
    copy.sort((ChatItem a, ChatItem b) {
      final int byTurn = a.turn.compareTo(b.turn);
      // 同一轮内 user 与 assistant 共用编号空间，blockId 升序就是渲染顺序。
      return byTurn != 0 ? byTurn : a.blockId.compareTo(b.blockId);
    });
    return copy;
  }

  LlmConvMeta _withUsage(LlmConvMeta meta, LlmUsage usage) => LlmConvMeta(
        convId: meta.convId,
        convGeneration: meta.convGeneration,
        agent: meta.agent,
        cwd: meta.cwd,
        title: meta.title,
        state: meta.state,
        curTurn: meta.curTurn,
        createdMs: meta.createdMs,
        updatedMs: meta.updatedMs,
        model: meta.model,
        permissionMode: meta.permissionMode,
        cliSessionId: meta.cliSessionId,
        origin: meta.origin,
        usage: usage,
      );

  void _resetForGeneration(int generation) {
    _generation = generation;
    _rateLimitObservedMs = -1;
    tail.clear();
    // ★ 存档也要一起清。代次前进 = 被控端重建了对话（进程重启等），旧条目与新对话
    // 拼在一起就是两条对话的混合体——而它在**重开 App 之后**才会现形，那时已经没人
    // 记得中间发生过一次代次前进。
    _fireAndForget(() async => _store?.clearItems(convId));
    // 额度是**账号级**的，跨代次仍然有效——清它等于把一条刚播报的限流状态抹掉。
    // 在途消息同理：它们还没送到，与被控端重建对话无关，**不能跟着一起清**。
    _state = ConversationSnapshot(
      rateLimit: _state.rateLimit,
      pending: _state.pending,
    );
  }

  void _emit(ConversationSnapshot next) {
    // 水位线或 meta 动了就排一次落库。**放在这里而不是各个 apply 里**：
    // 少写一处的症状是「某条路径下水位线永远不前进」，而它只在那条路径上才现形。
    if (next.lastSeq != _state.lastSeq || !identical(next.meta, _state.meta)) {
      _markConvDirty();
    }
    _state = next;
    if (!_snapshots.isClosed) _snapshots.add(next);
  }
}
