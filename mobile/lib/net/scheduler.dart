/// 可注入的定时器。
///
/// ## 为什么不直接用 `Timer`
///
/// 片 5 里有**四个**靠时间驱动的行为，每一个错了都只在真机弱网下才现形：
///
/// | 计时器 | 时长 | 错了会怎样 |
/// |---|---|---|
/// | 心跳 | 25s | 服务端 45 秒在线窗口过期，设备在别人的列表里显示离线 |
/// | Pong 看门狗 | 15s | 半开连接不被发现，「界面正常但什么都不动」 |
/// | 后台宽限 | 25s | 切出去看一眼验证码回来就断线重连，或反过来长期占着 socket |
/// | 发起超时 | 12s | 老服务端完全无回音，UI 无限转圈 |
///
/// 直接用 `Timer` 的话，测这四条就得让测试**真的等** 12–25 秒（还没法测「刚好没到」与
/// 「刚好到了」的边界）。于是现实里的结果一定是：这四条根本没有测试。
///
/// 注入之后，测试里用一个手动推进的假时钟，25 秒是一行 `advance(Duration(seconds: 25))`。
///
/// ## 为什么不用 `package:fake_async`
///
/// 它是 `flutter_test` 的**传递**依赖，直接 import 会被 `depend_on_referenced_packages`
/// 拦下；显式加进 dev_dependencies 又要动 `pubspec.lock`（本机 pub 的坑见 `tool/test.sh`）。
/// 而这里要假的只有「延迟执行」一件事，自己写一个接口比引一个包更小、也更好读。
library;

import 'dart:async';

/// 一个可取消的延迟任务。
abstract interface class ScheduledTask {
  /// 取消。**必须幂等**：重连路径上「取消一个已经触发过的计时器」是常态。
  void cancel();

  /// 还没触发也没被取消。
  bool get isActive;
}

/// 延迟执行的出口。
abstract interface class Scheduler {
  /// `delay` 之后执行 `action`。
  ScheduledTask schedule(Duration delay, void Function() action);
}

/// 生产实现：`dart:async` 的 `Timer`。
final class RealScheduler implements Scheduler {
  const RealScheduler();

  @override
  ScheduledTask schedule(Duration delay, void Function() action) =>
      _TimerTask(Timer(delay, action));
}

final class _TimerTask implements ScheduledTask {
  _TimerTask(this._timer);

  final Timer _timer;

  @override
  void cancel() => _timer.cancel();

  @override
  bool get isActive => _timer.isActive;
}

/// 一个「可能没有」的计时器槽位：换新任务时自动取消旧的。
///
/// 这个小东西存在的理由是一类真实的 bug：重连路径上忘了取消旧计时器，于是两个心跳定时器
/// 同时活着，Ping 变成 12.5 秒一次；再重连一次就是 8 秒一次——**症状是流量变大、日志变多，
/// 但功能全对**，几乎不可能在开发期发现。
final class TimerSlot {
  TimerSlot(this._scheduler);

  final Scheduler _scheduler;
  ScheduledTask? _task;

  /// 是否有任务在等着触发。
  bool get isActive => _task?.isActive ?? false;

  /// 装一个新任务，**先取消旧的**。
  void set(Duration delay, void Function() action) {
    cancel();
    _task = _scheduler.schedule(delay, action);
  }

  /// 只在槽位空着时装（用于「已经在等的就别重置」的语义，例如后台宽限计时器）。
  void setIfIdle(Duration delay, void Function() action) {
    if (isActive) return;
    set(delay, action);
  }

  void cancel() {
    _task?.cancel();
    _task = null;
  }
}
