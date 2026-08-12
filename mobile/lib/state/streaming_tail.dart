/// 流式末块的缓冲——**唯一高频变化的东西，刻意与列表隔离**。
///
/// ## 为什么它不进 `items`
///
/// 一次流式回复里，末块每几十毫秒就变一次。若把它作为列表的最后一个条目，
/// 每次变化都要重建整张 `ListView`——那正是长对话卡顿的头号来源。
/// 做法是：末块挂在这个 [ValueNotifier] 上，列表尾部放一个
/// `ValueListenableBuilder` 单独订阅它，**全程不重建整表**；块闭合时才把它定稿成一个
/// 真正的 [ChatItem] 推进 `items`。
///
/// ## 33 毫秒
///
/// 与协议侧的 `LLM_DELTA_FLUSH_MS = 33` 同源（≈ 30 fps，与 UI 刷新率对齐）：再快人眼无感，
/// 白白增加帧数与重绘。**被控端已经按 33ms 合并过一次**，这里再合一次是因为一帧 `Delta`
/// 里可能装着多条 `TextAppend`，逐条通知等于把上游省下的帧数又吐回来。
///
/// ## ★ 先拼接，再交渲染层做字素簇切分
///
/// 增量可能把一个 ZWJ 家庭 emoji 拆成两半，**两半各自都是合法字符串**，协议层无从阻止。
/// 所以这里只做字符串拼接、**绝不**按字素簇处理——每收一条就切一次在 Flutter 上是可见闪烁。
library;

import 'package:flutter/foundation.dart';
import 'package:lumen_mobile/net/scheduler.dart';

/// 合并窗口。与协议侧 `LLM_DELTA_FLUSH_MS` 同源。
const Duration kTailFlushWindow = Duration(milliseconds: 33);

/// 末块的当前内容。
@immutable
final class TailSnapshot {
  const TailSnapshot({this.blockId, this.text = ''});

  /// 正在流的块号。null = 当前没有末块（列表尾部不画东西）。
  final int? blockId;

  /// 已拼接的正文。
  final String text;

  bool get isEmpty => blockId == null;

  @override
  bool operator ==(Object other) =>
      other is TailSnapshot && other.blockId == blockId && other.text == text;

  @override
  int get hashCode => Object.hash(blockId, text);

  @override
  String toString() => 'TailSnapshot(block=$blockId, ${text.length} 字符)';
}

/// 末块缓冲：累积 → 按 [kTailFlushWindow] 合并 → 通知。
final class StreamingTail extends ValueNotifier<TailSnapshot> {
  StreamingTail({Scheduler scheduler = const RealScheduler()})
      : _flush = TimerSlot(scheduler),
        super(const TailSnapshot());

  final TimerSlot _flush;
  final StringBuffer _pending = StringBuffer();

  int? _blockId;
  String _committed = '';

  /// 当前正在流的块号。
  int? get blockId => _blockId;

  /// 追加一段增量。
  ///
  /// [blockId] 与当前不同 ⇒ 上一块没有正常闭合就来了新块。这不该发生（`BlockEnd` 会先到），
  /// 但真发生时**先把旧的冲出去**再开新块，而不是把两块的文字混在一起——混在一起产生的是
  /// 一段读起来通顺、实际上张冠李戴的正文，比丢一段更难发现。
  void append(int blockId, String text) {
    if (_blockId != null && _blockId != blockId) {
      _flushNow();
      _reset();
    }
    _blockId = blockId;
    _pending.write(text);
    _flush.setIfIdle(kTailFlushWindow, _flushNow);
  }

  /// 定稿：把末块内容取走并清空。
  ///
  /// 返回的是**已拼接的完整正文**，调用方据此造 [ChatItem]。
  String takeAndClear() {
    _flush.cancel();
    final String all = _committed + _pending.toString();
    _reset();
    value = const TailSnapshot();
    return all;
  }

  /// 丢弃末块（对话被重置 / 代次推进时）。
  void clear() {
    _flush.cancel();
    _reset();
    value = const TailSnapshot();
  }

  @override
  void dispose() {
    _flush.cancel();
    super.dispose();
  }

  void _reset() {
    _pending.clear();
    _committed = '';
    _blockId = null;
  }

  void _flushNow() {
    if (_pending.isEmpty) return;
    _committed += _pending.toString();
    _pending.clear();
    value = TailSnapshot(blockId: _blockId, text: _committed);
  }
}
