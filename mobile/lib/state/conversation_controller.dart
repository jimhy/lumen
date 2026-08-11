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
/// ## seq 是**整条对话**一个序列，不是每种帧一个
///
/// 依据：`LlmAttached` 的文档说「控制端丢弃 seq 不大于本值的迟到 Delta」，而 golden 语料里
/// 同一对话的 `ConvStarted(100) → Delta(102) → TurnStarted(103) → TurnEnded(104) →
/// TurnSnapshot(105)` 是连号的。所以所有带 seq 的帧走同一套水位线。
///
/// ⚠ 万一这个推断错了（TurnStarted/TurnEnded 走独立序列），症状是**偶尔多发一次 Attach**
/// ——多一次补齐往返，数据不会错。反过来「不检查」的代价是丢帧永远不被发现。
/// 两害相权，这里选前者。
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

import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/net/scheduler.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/state/streaming_tail.dart';

const String _tag = 'conv';

/// 把一帧发给被控端。返回 false = 没连上、这帧被丢弃。
typedef LlmFrameSender = bool Function(LlmFrame frame);

/// 对话的当前快照（不含高频变化的末块，那在 [ConversationController.tail]）。
final class ConversationSnapshot {
  const ConversationSnapshot({
    this.items = const <ChatItem>[],
    this.lastSeq = 0,
    this.meta,
    this.thinking = false,
    this.rateLimit,
    this.turnRunning = false,
    this.blocksOmitted = 0,
  });

  /// 定稿条目，按 `(turn, blockId)` 升序。
  final List<ChatItem> items;

  /// 已消费到的增量水位线。
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
    int? lastSeq,
    LlmConvMeta? meta,
    bool? thinking,
    LlmRateLimit? rateLimit,
    bool? turnRunning,
    int? blocksOmitted,
  }) =>
      ConversationSnapshot(
        items: items ?? this.items,
        lastSeq: lastSeq ?? this.lastSeq,
        meta: meta ?? this.meta,
        thinking: thinking ?? this.thinking,
        rateLimit: rateLimit ?? this.rateLimit,
        turnRunning: turnRunning ?? this.turnRunning,
        blocksOmitted: blocksOmitted ?? this.blocksOmitted,
      );

  @override
  String toString() =>
      'ConversationSnapshot(${items.length} 条, seq=$lastSeq, thinking=$thinking)';
}

/// 一条对话的归约器。
final class ConversationController {
  ConversationController({
    required this.convId,
    required LlmFrameSender send,
    Scheduler scheduler = const RealScheduler(),
  })  : _send = send,
        tail = StreamingTail(scheduler: scheduler);

  /// 本控制器负责的对话。
  final int convId;

  final LlmFrameSender _send;

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
        if (!_admit(frame.seq, frame.convGeneration)) return;
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

  Future<void> dispose() async {
    tail.dispose();
    await _snapshots.close();
  }

  // ── 不变量 ─────────────────────────────────────────────────────────────────

  /// 幂等 + 空洞补齐。返回 false = 这一帧不该被应用。
  bool _admit(int seq, int generation) {
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
    if (seq <= _state.lastSeq) return false; // 不变量 1：幂等
    if (seq > _state.lastSeq + 1) {
      // 不变量 2：**先补齐再说**，不能先应用。
      logWarn(_tag, '增量出现空洞（收到 $seq，本地 ${_state.lastSeq}），请求补齐');
      _send(LlmAttach(
        convId: convId,
        knownGeneration: _generation,
        knownSeq: _state.lastSeq,
      ));
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
          _upsert(ChatGap(
            turn: frame.turn,
            blockId: blockId,
            role: ChatRole.assistant,
            bytes: bytes,
          ));
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
    _upsert(_itemOf(turn, entry, ChatRole.assistant));
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
      _upsert(_itemOf(turn, LlmBlockEntry(blockId: blockId, block: block),
          ChatRole.assistant));
      return;
    }
    if (tailText.isEmpty) return; // 非文本块的 BlockEnd，条目在 BlockStart 时已落。
    _upsert(ChatText(
      turn: turn,
      blockId: blockId,
      role: ChatRole.assistant,
      markdown: tailText,
    ));
  }

  void _applyTurnStarted(LlmTurnStarted frame) {
    for (final LlmBlockEntry entry in frame.user) {
      _upsert(_itemOf(frame.turn, entry, ChatRole.user));
    }
    _emit(_state.copyWith(lastSeq: frame.seq, turnRunning: true, thinking: false));
  }

  void _applyTurnEnded(LlmTurnEnded frame) {
    // 末块可能还没等到 BlockEnd 就轮结束了（被控端崩了/被中断），先定稿再收尾。
    final String leftover = tail.takeAndClear();
    if (leftover.isNotEmpty) {
      _upsert(ChatText(
        turn: _tailTurn,
        blockId: tail.blockId ?? 0,
        role: ChatRole.assistant,
        markdown: leftover,
      ));
    }
    final LlmConvMeta? meta = _state.meta;
    _emit(_state.copyWith(
      lastSeq: frame.seq,
      turnRunning: false,
      thinking: false,
      // 轮末的 usage 是**真值**（分母来自上游的 contextWindow，不是我们猜的）。
      meta: meta == null ? null : _withUsage(meta, frame.usage),
    ));
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
    for (final LlmBlockEntry entry in record.user) {
      kept.add(_itemOf(record.turn, entry, ChatRole.user));
    }
    for (final LlmBlockEntry entry in record.assistant) {
      kept.add(_itemOf(record.turn, entry, ChatRole.assistant));
    }
    if (tail.blockId != null && _tailTurn == record.turn) {
      // 快照是权威，末块的半成品作废——留着会与快照里的定稿块重复。
      tail.clear();
    }
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
  void _upsert(ChatItem item) {
    final List<ChatItem> next = List<ChatItem>.of(_state.items);
    final int at = next.indexWhere((ChatItem i) => i.key == item.key);
    if (at < 0) {
      next.add(item);
    } else if (item is ChatText && next[at] is ChatText) {
      next[at] = (next[at] as ChatText).bumped(item.markdown);
    } else {
      next[at] = item;
    }
    _emit(_state.copyWith(items: next));
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
    // 额度是**账号级**的，跨代次仍然有效——清它等于把一条刚播报的限流状态抹掉。
    _state = ConversationSnapshot(rateLimit: _state.rateLimit);
  }

  void _emit(ConversationSnapshot next) {
    _state = next;
    if (!_snapshots.isClosed) _snapshots.add(next);
  }
}
