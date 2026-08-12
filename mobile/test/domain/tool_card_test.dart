/// 工具卡片领域模型的验收（片 9）。
///
/// ## 这一组守的是同一条纪律：**尝试 + 兜底，绝不假定字段存在**
///
/// 蓝图 §3.3 的「仍未取到」表写着：`Bash` / `Edit` / `Write` 的 `tool_use_result` 形态
/// **一个都没实测过**，只有 `Read` 的见过真样本。§7.6 那句「绝不许照着 `file` 的字段名
/// 去猜 `Bash` 会不会有个 `stdout`」是硬约束。
///
/// 所以下面大半的用例都在喂**畸形输入**：缺字段、错类型、空对象、标量、null。
/// 期望一律是「退回兜底」，不是抛异常、也不是画一个空卡片。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/domain/text_diff.dart';
import 'package:lumen_mobile/domain/tool_card.dart';
import 'package:lumen_mobile/domain/tool_shape.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

ChatToolCall call(String name, Object? input, {LlmText? title}) => ChatToolCall(
      turn: 1,
      blockId: 0,
      role: ChatRole.assistant,
      callId: 'c1',
      name: name,
      input: LlmToolInput(input),
      title: title,
    );

ChatToolResult result({
  LlmToolResultDetail? detail,
  String output = '',
  LlmToolStatus status = LlmToolStatus.ok,
  int? truncatedBytes,
}) =>
    ChatToolResult(
      turn: 1,
      blockId: 1,
      role: ChatRole.assistant,
      callId: 'c1',
      status: status,
      output: LlmText(output),
      detail: detail,
      truncatedBytes: truncatedBytes,
    );

void main() {
  group('形态映射', () {
    test('已知工具各归各位', () {
      expect(shapeOf('Read'), ToolShape.read);
      expect(shapeOf('Edit'), ToolShape.edit);
      expect(shapeOf('MultiEdit'), ToolShape.edit);
      expect(shapeOf('Write'), ToolShape.write);
      expect(shapeOf('Bash'), ToolShape.bash);
      expect(shapeOf('Grep'), ToolShape.search);
      expect(shapeOf('WebSearch'), ToolShape.web);
      expect(shapeOf('TodoWrite'), ToolShape.todo);
      expect(shapeOf('Task'), ToolShape.task);
    });

    test('★ 没见过的工具名一律兜底（CLI 的工具清单会持续新增）', () {
      expect(shapeOf('未来才有的工具'), ToolShape.generic);
      expect(shapeOf(''), ToolShape.generic);
      expect(shapeOf('read'), ToolShape.generic, reason: '大小写敏感是刻意的');
    });

    test('每个形态都有图标名，一个不漏', () {
      for (final ToolShape s in ToolShape.values) {
        expect(iconNameOf(s), isNotEmpty, reason: '${s.name} 没有图标');
      }
    });
  });

  group('调用卡片：摘要', () {
    test('PC 侧给了 title 就用它（被控端比本端猜得准）', () {
      final ToolCard c = toolCallCard(call(
        'Bash',
        <String, Object?>{'command': 'ls'},
        title: const LlmText('列出仓库根目录'),
      ));
      expect(c.summary, '列出仓库根目录');
    });

    test('没有 title 时按形态从入参组一句', () {
      expect(
        toolCallCard(call('Read', <String, Object?>{'file_path': 'a/b.rs'})).summary,
        '读取 a/b.rs',
      );
      expect(
        toolCallCard(call('Bash', <String, Object?>{'command': 'cargo test'})).summary,
        'cargo test',
      );
      expect(
        toolCallCard(call('Grep', <String, Object?>{'pattern': 'TODO'})).summary,
        'TODO',
      );
    });

    test('★ 取不到字段就只报工具名，不画一个半截摘要', () {
      expect(toolCallCard(call('Read', <String, Object?>{})).summary, 'Read');
      expect(
        toolCallCard(call('Read', <String, Object?>{'file_path': ''})).summary,
        'Read',
        reason: '空串算取不到——「读取 」比「Read」更糟',
      );
      expect(
        toolCallCard(call('Read', <String, Object?>{'file_path': 42})).summary,
        'Read',
        reason: '类型不对也算取不到',
      );
    });
  });

  group('★ Edit 的 diff：尝试 + 兜底', () {
    test('两边都齐 ⇒ 画 diff', () {
      final ToolCard c = toolCallCard(call('Edit', <String, Object?>{
        'file_path': 'src/main.rs',
        'old_string': 'let x = 1;',
        'new_string': 'let x = 2;',
      }));
      final ToolBodyDiff body = c.body as ToolBodyDiff;
      expect(body.path, 'src/main.rs');
      expect((body.diff as DiffLines).added, 1);
      expect((body.diff as DiffLines).removed, 1);
    });

    test('★ 缺 new_string ⇒ 退回折叠 JSON，不画空 diff', () {
      final ToolCard c = toolCallCard(call('Edit', <String, Object?>{
        'file_path': 'a.rs',
        'old_string': 'x',
      }));
      expect(c.body, isA<ToolBodyJson>());
    });

    test('★ old_string 类型不对 ⇒ 退回折叠 JSON', () {
      final ToolCard c = toolCallCard(call('Edit', <String, Object?>{
        'old_string': <String>['不是字符串'],
        'new_string': 'y',
      }));
      expect(c.body, isA<ToolBodyJson>());
    });

    test('★ 工具改了名（MultiEdit 的入参形状不同）⇒ 照样兜住', () {
      // MultiEdit 实际是 {file_path, edits: [...]}，没有顶层 old/new_string。
      final ToolCard c = toolCallCard(call('MultiEdit', <String, Object?>{
        'file_path': 'a.rs',
        'edits': <Object?>[
          <String, Object?>{'old_string': 'a', 'new_string': 'b'},
        ],
      }));
      expect(c.shape, ToolShape.edit, reason: '形态认得出来');
      expect(c.body, isA<ToolBodyJson>(), reason: '但字段取不到就老实折叠 JSON');
    });

    test('diff 里没有 file_path 时 path 是 null，不编一个出来', () {
      final ToolCard c = toolCallCard(call('Edit', <String, Object?>{
        'old_string': 'a',
        'new_string': 'b',
      }));
      expect((c.body as ToolBodyDiff).path, isNull);
    });
  });

  group('★ 畸形入参一律不抛', () {
    test('入参是标量 / 数组 / null', () {
      expect(toolCallCard(call('Bash', 'ls -la')).body, isA<ToolBodyJson>());
      expect(toolCallCard(call('Bash', <Object?>[1, 2])).body, isA<ToolBodyJson>());
      // null 入参没什么可展开的 —— 不给展开箭头。
      final ToolCard c = toolCallCard(call('Bash', null));
      expect(c.body, isA<ToolBodyNone>());
      expect(c.expandable, isFalse);
    });

    test('空对象 ⇒ 折叠 JSON（是「{}」而不是崩溃）', () {
      expect(toolCallCard(call('X', <String, Object?>{})).body, isA<ToolBodyJson>());
    });

    test('Write 的 content 直接当正文；缺了就折叠 JSON', () {
      expect(
        toolCallCard(call('Write', <String, Object?>{'content': 'hello'})).body,
        isA<ToolBodyText>(),
      );
      expect(
        toolCallCard(call('Write', <String, Object?>{'file_path': 'a'})).body,
        isA<ToolBodyJson>(),
      );
    });
  });

  group('结果卡片', () {
    test('★ 有 detail ⇒ 走结构化摘要，一个字节正文都不碰', () {
      // 实测一次 Read 的 output 是 12272 字符，而这四个小字段就够画出摘要。
      final ToolCard c = toolResultCard(result(
        detail: const LlmToolResultDetailFile(
          path: LlmPath('README.md'),
          startLine: 1,
          lineCount: 165,
          totalLines: 165,
        ),
        truncatedBytes: 12272,
      ));
      expect(c.summary, '读取 README.md 第 1–165 行（共 165 行）');
      expect(c.body, isA<ToolBodyNone>(), reason: 'PC 侧本来就没传正文');
      expect(c.truncatedBytes, 12272, reason: 'UI 要据此给「拉取完整内容」入口');
    });

    test('★ 即使 output 里有内容，有 detail 时摘要也不碰它', () {
      // 这条守的是「detail 优先」不是靠 output 恰好为空才成立的。
      final ToolCard c = toolResultCard(result(
        detail: const LlmToolResultDetailFile(
          path: LlmPath('README.md'),
          startLine: 1,
          lineCount: 165,
          totalLines: 165,
        ),
        output: '这段正文不该出现在摘要里' * 100,
      ));
      expect(c.summary, '读取 README.md 第 1–165 行（共 165 行）');
      expect(c.summary.contains('不该出现'), isFalse);
      // 但正文既然传上来了，展开时就该看得见——不显示它等于白花了那些流量。
      expect(c.body, isA<ToolBodyText>());
    });

    test('只读了一部分时把总行数说出来', () {
      final ToolCard c = toolResultCard(result(
        detail: const LlmToolResultDetailFile(
          path: LlmPath('big.log'),
          startLine: 100,
          lineCount: 50,
          totalLines: 9000,
        ),
      ));
      expect(c.summary, '读取 big.log 第 100–149 行（共 9000 行）');
    });

    test('没有 detail ⇒ 退回正文首行并截断', () {
      final ToolCard c = toolResultCard(result(output: '第一行\n第二行'));
      expect(c.summary, '第一行');
      expect(c.shape, ToolShape.generic);
      expect(c.body, isA<ToolBodyText>());
    });

    test('未知的 detail 形态也走兜底，不当成 File', () {
      final ToolCard c = toolResultCard(result(
        detail: const LlmToolResultDetailUnknown(),
        output: 'something',
      ));
      expect(c.shape, ToolShape.generic);
    });

    test('非 ok 状态优先说出来', () {
      expect(
        toolResultCard(result(status: LlmToolStatus.denied, output: 'x')).summary,
        '工具被拒绝',
      );
      expect(
        toolResultCard(result(status: LlmToolStatus.error, output: 'x')).summary,
        '工具出错',
      );
    });

    test('空正文 ⇒ 没有可展开的，不给箭头', () {
      final ToolCard c = toolResultCard(result());
      expect(c.summary, '工具已完成');
      expect(c.expandable, isFalse);
    });
  });

  group('prettyJson', () {
    test('缩进好，且对不可编码的值不抛', () {
      expect(prettyJson(<String, Object?>{'a': 1}), '{\n  "a": 1\n}');
      // 未知形态的最终归宿必须对任何输入都活着。
      expect(prettyJson(Object()), isNotEmpty);
    });
  });
}
