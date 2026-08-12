/// 重连退避：**指数 + 全抖动**（1s → 30s 上限）。
///
/// ## 为什么不照抄桌面端的固定 3 秒
///
/// 桌面端 `remote_ws.rs` 是固定 3 秒重连，那在一台机器上没问题。移动端不行：
/// 一次服务端重启 / 一段地铁隧道会让**同一批用户在同一时刻**掉线，固定间隔意味着他们
/// 之后每 3 秒**同频**敲一次服务端——而服务端出站通道当前是无界 mpsc、零背压
/// （R11/R24），最需要喘息的时刻正是被打得最狠的时刻。
///
/// 抖动是为了打散这个同频，指数是为了让长时间不可用的场景不至于一直高频重试。
///
/// ## 全抖动的下界为什么是 50% 而不是 0
///
/// 教科书上的「full jitter」是 `random(0, base)`，下界 0。这里取 `random(base/2, base)`：
/// 下界 0 意味着**第一次重连可能在 0ms 后发生**，而刚断开时链路多半还没恢复，这一次几乎
/// 必然失败、只是把 `attempt` 推高一格。留 50% 下界换来「每次重试都真的等了一会儿」。
library;

import 'dart:math';

/// 指数退避 + 全抖动的延迟序列。
///
/// 用法：连接失败调 [next] 拿下次延迟；连上（或用户显式重连）调 [reset]。
final class Backoff {
  Backoff({
    this.baseMs = 1000,
    this.maxMs = 30000,
    this.maxShift = 5,
    Random? random,
  })  : assert(baseMs > 0, 'baseMs 必须为正'),
        assert(maxMs >= baseMs, 'maxMs 不能小于 baseMs'),
        _random = random ?? Random();

  /// 第一次重连的基准延迟。
  final int baseMs;

  /// 基准延迟的上限。
  final int maxMs;

  /// 指数移位的上限（`1 << maxShift`）。钉住它是为了不让 `1 << attempt` 在长时间掉线后
  /// 溢出——Dart 的 `int` 在 Web 上是 53 位有效位，移到 60 位就开始给出无意义的数。
  final int maxShift;

  final Random _random;

  int _attempt = 0;

  /// 已经失败了几次（诊断用；UI 可据此把「重连中」升级成「连不上服务器」）。
  int get attempt => _attempt;

  /// 当前这一格的基准延迟（不含抖动），用于测试与日志。
  int get currentBaseMs =>
      min(maxMs, baseMs * (1 << min(_attempt, maxShift)));

  /// 取下一次重连延迟并推进一格。
  Duration next() {
    final int base = currentBaseMs;
    final int half = base ~/ 2;
    // nextInt 的上界是开区间，+1 让抖动能取到 base 本身。
    final int jitter = _random.nextInt(base - half + 1);
    _attempt++;
    return Duration(milliseconds: half + jitter);
  }

  /// 连上了 / 用户显式重连 / 网络恢复：回到第一格。
  void reset() => _attempt = 0;
}
