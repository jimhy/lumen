/// 行级 diff 的验收（片 9）。
///
/// diff 写错的症状很特别：**它不会报错，只会让人读到一个错误的改动**。
/// 所以这里的断言全是「这一行到底该是 +、- 还是空格」，不测像素也不测性能。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/domain/text_diff.dart';

/// 把结果压成一个好读的字符串，断言写起来才不至于埋在结构里。
String render(DiffResult r) => switch (r) {
      DiffLines(:final List<DiffLine> lines) =>
        lines.map((DiffLine l) => l.toString()).join('\n'),
      DiffTooLarge() => '<太大>',
    };

void main() {
  group('基本形态', () {
    test('完全相同 ⇒ 全是 keep', () {
      expect(render(diffLines('a\nb', 'a\nb')), ' a\n b');
    });

    test('中间改一行', () {
      expect(
        render(diffLines('a\nb\nc', 'a\nB\nc')),
        ' a\n-b\n+B\n c',
      );
    });

    test('★ 同一处改动先删后增（反过来读像是两处改动）', () {
      final DiffLines r = diffLines('old', 'new') as DiffLines;
      expect(r.lines.first.op, DiffOp.del);
      expect(r.lines.last.op, DiffOp.add);
    });

    test('纯新增与纯删除', () {
      expect(render(diffLines('', 'a\nb')), '+a\n+b');
      expect(render(diffLines('a\nb', '')), '-a\n-b');
    });

    test('★ 空串切出来是空列表，不是一个空行', () {
      // 否则「新建一个空文件」会显示成「加了一个空行」。
      expect((diffLines('', '') as DiffLines).lines, isEmpty);
    });

    test('增删计数', () {
      final DiffLines r = diffLines('a\nb\nc', 'a\nX\nY\nc') as DiffLines;
      expect(r.removed, 1);
      expect(r.added, 2);
    });
  });

  group('★ 换行符归一化', () {
    test('CRLF 与 LF 的同一段文本不算改动', () {
      // 不归一化的话**每一行都会显示成改动过** —— Windows 上写出来的文件与模型
      // 生成的字符串常常一个带 \r 一个不带，这是最容易踩且最难看出来的一条。
      expect(render(diffLines('a\r\nb', 'a\nb')), ' a\n b');
    });

    test('单独的 CR 也归一化', () {
      expect(render(diffLines('a\rb', 'a\nb')), ' a\n b');
    });
  });

  group('公共前后缀', () {
    test('大段共享上下文只在中间产生改动', () {
      const String head = 'h1\nh2\nh3';
      const String tail = 't1\nt2\nt3';
      final DiffLines r =
          diffLines('$head\nX\n$tail', '$head\nY\n$tail') as DiffLines;
      expect(r.added, 1);
      expect(r.removed, 1);
      expect(r.lines, hasLength(8), reason: '6 行上下文 + 一删一增');
    });
  });

  group('★ 规模上限', () {
    test('超过上限时不算 LCS，如实报「太大」', () {
      // 有上限不是优化：两个 2000 行版本就是 400 万格 DP 表。
      final String big = List<String>.generate(kDiffMaxLines + 1, (int i) => 'l$i')
          .join('\n');
      final String big2 =
          List<String>.generate(kDiffMaxLines + 1, (int i) => 'x$i').join('\n');
      final DiffResult r = diffLines(big, big2);
      expect(r, isA<DiffTooLarge>());
      expect((r as DiffTooLarge).oldLines, kDiffMaxLines + 1);
      expect(r.newLines, kDiffMaxLines + 1);
    });

    test('★ 大文件但改动很小 ⇒ 剥掉公共前后缀后照样能算', () {
      // 这正是剥前后缀的意义：真实的 Edit 入参常常共享大段上下文。
      final List<String> lines =
          List<String>.generate(kDiffMaxLines * 3, (int i) => 'line$i');
      final String a = lines.join('\n');
      final List<String> changed = List<String>.of(lines)..[500] = '改了这一行';
      final DiffResult r = diffLines(a, changed.join('\n'));

      expect(r, isA<DiffLines>(), reason: '公共前后缀剥掉后中间只剩一行');
      expect((r as DiffLines).added, 1);
      expect(r.removed, 1);
    });
  });
}
