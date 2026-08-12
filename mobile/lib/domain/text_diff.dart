/// 手写行级 diff（片 9）——**零 Flutter 依赖、纯函数**。
///
/// ## 为什么手写而不引包
///
/// 需求只有「两段文本 → 按行的增删保留」这一种，而 diff 包普遍带着字符级算法、
/// 补丁格式、合并冲突解决等一大堆用不上的东西。蓝图 §10.1 片 9 那行写的就是
/// 「手写行级 LCS diff」。
///
/// ## ★ 必须有规模上限，这不是优化
///
/// LCS 是 O(n×m) 的时空复杂度。两个 2000 行的版本就是 400 万格的 DP 表——在手机上
/// 那是几十 MB 内存 + 肉眼可见的卡顿，而用户很可能只是想扫一眼「改了哪个文件」。
///
/// 所以超过 [kDiffMaxLines] 时**不做 LCS**，直接返回 [DiffTooLarge]。UI 画成
/// 「改动过大（N 行 → M 行），不展开」——**说出来**，而不是画一个空的或者转圈到死
/// （§14-6 无声降级禁令）。
///
/// ## 先剥公共前后缀
///
/// 真实的 `Edit` 入参里 `old_string` 与 `new_string` 常常共享大段上下文（模型为了定位
/// 唯一匹配点会多带几行）。先剥掉公共前后缀再跑 LCS，能把绝大多数真实用例的规模
/// 压到十行以内——这一步同时让上面那个上限极少被触发。
library;

/// LCS 的规模上限（剥掉公共前后缀**之后**的行数，两边各自算）。
///
/// 400 行是按「一次 `Edit` 的合理改动量」取的。真要改 400 行以上的人不会在手机上看 diff。
const int kDiffMaxLines = 400;

/// 一行 diff。
enum DiffOp {
  /// 两边都有。
  keep,

  /// 只在新版本里。
  add,

  /// 只在旧版本里。
  del,
}

/// diff 的一行。
final class DiffLine {
  const DiffLine(this.op, this.text);

  final DiffOp op;
  final String text;

  @override
  bool operator ==(Object other) =>
      other is DiffLine && other.op == op && other.text == text;

  @override
  int get hashCode => Object.hash(op, text);

  @override
  String toString() =>
      '${switch (op) { DiffOp.keep => ' ', DiffOp.add => '+', DiffOp.del => '-' }}$text';
}

/// diff 的结果。
sealed class DiffResult {
  const DiffResult();
}

/// 算出来了。
final class DiffLines extends DiffResult {
  const DiffLines(this.lines);

  final List<DiffLine> lines;

  /// 新增行数（给摘要用）。
  int get added => lines.where((DiffLine l) => l.op == DiffOp.add).length;

  /// 删除行数。
  int get removed => lines.where((DiffLine l) => l.op == DiffOp.del).length;
}

/// 规模超限，**刻意不算**。UI 必须把这件事说出来。
final class DiffTooLarge extends DiffResult {
  const DiffTooLarge({required this.oldLines, required this.newLines});

  final int oldLines;
  final int newLines;
}

/// 行级 diff。
///
/// [oldText] / [newText] 用 `\n` 切行；`\r\n` 会先归一化——Windows 上写出来的文件
/// 与模型生成的字符串常常一个带 `\r` 一个不带，不归一化的话**每一行都会显示成改动过**。
DiffResult diffLines(String oldText, String newText) {
  final List<String> a = _splitLines(oldText);
  final List<String> b = _splitLines(newText);

  // ── 先剥公共前后缀 ────────────────────────────────────────────────────────
  int head = 0;
  while (head < a.length && head < b.length && a[head] == b[head]) {
    head++;
  }
  int tail = 0;
  while (tail < a.length - head &&
      tail < b.length - head &&
      a[a.length - 1 - tail] == b[b.length - 1 - tail]) {
    tail++;
  }

  final List<String> midA = a.sublist(head, a.length - tail);
  final List<String> midB = b.sublist(head, b.length - tail);

  if (midA.length > kDiffMaxLines || midB.length > kDiffMaxLines) {
    return DiffTooLarge(oldLines: a.length, newLines: b.length);
  }

  final List<DiffLine> out = <DiffLine>[
    for (int i = 0; i < head; i++) DiffLine(DiffOp.keep, a[i]),
    ..._lcsDiff(midA, midB),
    for (int i = a.length - tail; i < a.length; i++) DiffLine(DiffOp.keep, a[i]),
  ];
  return DiffLines(out);
}

/// 归一化换行并切行。
///
/// **空串切出来是空列表**，不是 `['']`：后者会让「新建一个空文件」显示成「加了一个空行」。
List<String> _splitLines(String s) {
  if (s.isEmpty) return const <String>[];
  return s.replaceAll('\r\n', '\n').replaceAll('\r', '\n').split('\n');
}

/// 标准 LCS + 回溯。调用方保证两边都不超过 [kDiffMaxLines]。
List<DiffLine> _lcsDiff(List<String> a, List<String> b) {
  if (a.isEmpty && b.isEmpty) return const <DiffLine>[];
  if (a.isEmpty) {
    return <DiffLine>[for (final String l in b) DiffLine(DiffOp.add, l)];
  }
  if (b.isEmpty) {
    return <DiffLine>[for (final String l in a) DiffLine(DiffOp.del, l)];
  }

  final int n = a.length;
  final int m = b.length;
  // dp[i][j] = a[i..] 与 b[j..] 的 LCS 长度。多开一行一列当哨兵，省掉边界判断。
  final List<List<int>> dp = List<List<int>>.generate(
    n + 1,
    (_) => List<int>.filled(m + 1, 0),
    growable: false,
  );
  for (int i = n - 1; i >= 0; i--) {
    for (int j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] == b[j]
          ? dp[i + 1][j + 1] + 1
          : (dp[i + 1][j] >= dp[i][j + 1] ? dp[i + 1][j] : dp[i][j + 1]);
    }
  }

  final List<DiffLine> out = <DiffLine>[];
  int i = 0;
  int j = 0;
  while (i < n && j < m) {
    if (a[i] == b[j]) {
      out.add(DiffLine(DiffOp.keep, a[i]));
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      // ★ 删在增前面：同一处改动渲染成「先 - 后 +」是所有 diff 工具的惯例，
      // 反过来读起来像是「先加了新的、又删了旧的」，读者会以为是两处改动。
      out.add(DiffLine(DiffOp.del, a[i]));
      i++;
    } else {
      out.add(DiffLine(DiffOp.add, b[j]));
      j++;
    }
  }
  while (i < n) {
    out.add(DiffLine(DiffOp.del, a[i++]));
  }
  while (j < m) {
    out.add(DiffLine(DiffOp.add, b[j++]));
  }
  return out;
}
