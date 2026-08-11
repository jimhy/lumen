/// 退避序列的验收。
///
/// 这个文件真正在守的是**下界**：`random` 若被写成 `nextInt(base)`（教科书 full jitter），
/// 第一次重连可能在 0ms 后发生，而刚断开时链路多半还没恢复——那一次几乎必然失败，
/// 只是把 attempt 推高一格。症状是「重连次数比预期多一倍」，没人会注意到。
library;

import 'dart:math';

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/net/backoff.dart';

/// 恒返回上界的假随机（`nextInt(n)` 的上界是开区间，故给 n-1）。
final class _MaxRandom implements Random {
  const _MaxRandom();

  @override
  bool nextBool() => true;

  @override
  double nextDouble() => 1;

  @override
  int nextInt(int max) => max - 1;
}

/// 恒返回 0 的假随机。
final class _MinRandom implements Random {
  const _MinRandom();

  @override
  bool nextBool() => false;

  @override
  double nextDouble() => 0;

  @override
  int nextInt(int max) => 0;
}

void main() {
  group('指数序列', () {
    test('基准延迟是 1,2,4,8,16,30 秒，之后恒 30', () {
      final Backoff b = Backoff(random: const _MinRandom());
      final List<int> bases = <int>[];
      for (int i = 0; i < 8; i++) {
        bases.add(b.currentBaseMs);
        b.next();
      }
      expect(bases, <int>[1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);
    });

    test('上限被 maxMs 夹住，不是被 1<<n 夹住', () {
      // 1000 * (1 << 5) = 32000，比 maxMs 大 —— 若少了 min()，第六次就会等 32 秒。
      final Backoff b = Backoff(random: const _MinRandom());
      for (int i = 0; i < 5; i++) {
        b.next();
      }
      expect(b.currentBaseMs, 30000);
    });
  });

  group('全抖动的下界是 50%，不是 0', () {
    test('最小取值 = base/2', () {
      final Backoff b = Backoff(random: const _MinRandom());
      expect(b.next().inMilliseconds, 500);
      expect(b.next().inMilliseconds, 1000);
      expect(b.next().inMilliseconds, 2000);
    });

    test('最大取值 = base', () {
      final Backoff b = Backoff(random: const _MaxRandom());
      expect(b.next().inMilliseconds, 1000);
      expect(b.next().inMilliseconds, 2000);
      expect(b.next().inMilliseconds, 4000);
    });

    test('真随机下每次都落在 [base/2, base] 内', () {
      final Backoff b = Backoff(random: Random(20260810));
      for (int i = 0; i < 50; i++) {
        final int base = b.currentBaseMs;
        final int ms = b.next().inMilliseconds;
        expect(ms, greaterThanOrEqualTo(base ~/ 2));
        expect(ms, lessThanOrEqualTo(base));
      }
    });

    test('抖动真的在抖（不是恒定值）', () {
      // 没有这条，一个把 nextInt 结果丢掉的实现也能通过上面全部断言，
      // 而「全国用户同频重连」正是抖动要挡的那件事。
      final Backoff b = Backoff(random: Random(7));
      final Set<int> seen = <int>{};
      for (int i = 0; i < 20; i++) {
        b.reset();
        seen.add(b.next().inMilliseconds);
      }
      expect(seen.length, greaterThan(1));
    });
  });

  test('reset 回到第一格', () {
    final Backoff b = Backoff(random: const _MinRandom());
    for (int i = 0; i < 4; i++) {
      b.next();
    }
    expect(b.attempt, 4);
    b.reset();
    expect(b.attempt, 0);
    expect(b.next().inMilliseconds, 500);
  });
}
