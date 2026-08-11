/// 手动推进的假时钟，替换 [`Scheduler`]。
///
/// 片 5 有四条靠时间驱动的行为（心跳 25s / Pong 看门狗 15s / 后台宽限 25s /
/// 发起超时 12s）。用真 `Timer` 测它们意味着测试要真的等——现实里的结果一定是「这四条没有
/// 测试」，而它们每一条错了都只在真机弱网下才现形。
///
/// [advance] 会按到期顺序逐个触发，并且**能捕获触发过程中新注册的任务**（心跳就是这样
/// 一次次接上的）。
library;

import 'package:lumen_mobile/net/scheduler.dart';

/// 假调度器。
final class FakeScheduler implements Scheduler {
  Duration _now = Duration.zero;
  final List<_FakeTask> _tasks = <_FakeTask>[];

  /// 当前虚拟时刻。
  Duration get now => _now;

  /// 还在等待触发的任务数。用来断言「旧计时器真的被取消了」——重连路径上忘记取消旧心跳
  /// 的症状是 Ping 频率翻倍，功能全对、只有流量变大，几乎不可能在开发期发现。
  int get pendingCount => _tasks.where((_FakeTask t) => t.isActive).length;

  @override
  ScheduledTask schedule(Duration delay, void Function() action) {
    final _FakeTask task = _FakeTask(_now + delay, action);
    _tasks.add(task);
    return task;
  }

  /// 推进虚拟时钟。
  void advance(Duration by) {
    final Duration target = _now + by;
    while (true) {
      _tasks.removeWhere((_FakeTask t) => !t.isActive);
      _FakeTask? next;
      for (final _FakeTask t in _tasks) {
        if (t.at <= target && (next == null || t.at < next.at)) next = t;
      }
      if (next == null) break;
      _now = next.at;
      _tasks.remove(next);
      next.fire();
    }
    _now = target;
  }
}

final class _FakeTask implements ScheduledTask {
  _FakeTask(this.at, this.action);

  final Duration at;
  final void Function() action;
  bool _cancelled = false;
  bool _fired = false;

  @override
  void cancel() => _cancelled = true;

  @override
  bool get isActive => !_cancelled && !_fired;

  void fire() {
    _fired = true;
    action();
  }
}
