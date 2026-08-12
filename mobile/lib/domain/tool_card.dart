/// 工具卡片的**领域模型**（片 9）——**零 Flutter 依赖、全是纯函数**。
///
/// UI 只负责把这里算出来的东西画出来。这样「摘要怎么组、字段取不到怎么办、
/// diff 从哪来」这些真正容易出错的判断全都能在纯 Dart 里断言。
///
/// ## ★ 全文只有一条纪律：**尝试 + 兜底，绝不假定字段存在**
///
/// 蓝图 §3.3「仍未取到」表里写着：`Bash` / `Edit` / `Write` 的 `tool_use_result` 形态
/// **一个都没实测过**，只有 `Read` 的 `{type:"text", file:{…}}` 见过真样本。
/// §7.6 那句「**绝不许照着 `file` 的字段名去猜 `Bash` 会不会有个 `stdout`**」是硬约束。
///
/// 所以这里对**结果**只认 `LlmToolResultDetailFile` 这一种结构化形态，其余一律退回
/// 「夹紧字符串 + 折叠 JSON」。
///
/// ## 入参与结果**不是同一层**，这条边界要说清楚
///
/// 上面那条纪律管的是 `tool_use_result`（结果）。而 `tool_use.input`（入参）是另一回事：
/// 它是**模型必须按 schema 生成**的公开契约，`Edit` 的 `file_path` / `old_string` /
/// `new_string` 属于工具定义的一部分。所以这里**会**去读它来画 diff。
///
/// 但「更可靠」不等于「保证存在」——工具定义照样会随版本变。故取字段一律走
/// [_str] 这套：类型不对、键不在、值不是字符串，一律得到 `null`，调用方退回折叠 JSON。
/// [toolCardOf] 有一组测试专门喂它缺字段、错类型、空对象。
library;

import 'dart:convert';

import 'package:lumen_mobile/domain/chat_item.dart';
import 'package:lumen_mobile/domain/text_diff.dart';
import 'package:lumen_mobile/domain/tool_shape.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

/// 卡片折叠态那一行摘要 + 展开态该画什么。
final class ToolCard {
  const ToolCard({
    required this.shape,
    required this.summary,
    required this.body,
    this.truncatedBytes,
  });

  final ToolShape shape;

  /// 折叠态那一行人话。
  final String summary;

  /// 展开态的内容。[ToolBodyNone] = 没什么可展开的，UI **不画展开箭头**
  /// （一个点开什么都没有的箭头比没有箭头更糟，与「正在思考…」同一条理由）。
  final ToolBody body;

  /// 正文被 PC 侧夹掉的字节数。有值时 UI 要给「拉取完整内容」的入口。
  final int? truncatedBytes;

  bool get expandable => body is! ToolBodyNone;
}

/// 展开态的内容。
sealed class ToolBody {
  const ToolBody();
}

/// 没有可展开的内容。
final class ToolBodyNone extends ToolBody {
  const ToolBodyNone();
}

/// 纯文本（命令输出、文件正文…）。用等宽字体画。
final class ToolBodyText extends ToolBody {
  const ToolBodyText(this.text);
  final String text;
}

/// 行级 diff。
final class ToolBodyDiff extends ToolBody {
  const ToolBodyDiff({required this.path, required this.diff});

  /// 被改的文件（已过 `LlmPath` 脱敏）。null = 入参里没给。
  final String? path;

  final DiffResult diff;
}

/// 折叠的 JSON —— **一切未知形态的归宿**。
final class ToolBodyJson extends ToolBody {
  const ToolBodyJson(this.pretty);

  /// 已缩进好的 JSON 文本。
  final String pretty;
}

/// 一次工具**调用**的卡片（还没有结果时就是它）。
ToolCard toolCallCard(ChatToolCall call) {
  final ToolShape shape = shapeOf(call.name);
  final JsonMap? input = _map(call.input.value);

  // PC 侧归一化出来的一行人话优先——它是被控端按真实上下文写的，比本端猜得准。
  final String summary = call.title?.value ?? _callSummary(shape, call.name, input);

  return ToolCard(
    shape: shape,
    summary: summary,
    body: _callBody(shape, call, input),
    truncatedBytes: call.truncatedBytes,
  );
}

/// 一次工具**结果**的卡片。
ToolCard toolResultCard(ChatToolResult result) {
  final LlmToolResultDetail? detail = result.detail;

  // ★ 唯一被实测过的结构化形态。四个小字段就够画出一行准确摘要，
  //   而对应的 `output` 实测是 12272 字符——那 12 KB 用户根本不会看。
  if (detail is LlmToolResultDetailFile) {
    return ToolCard(
      shape: ToolShape.read,
      summary: _fileSummary(detail),
      // 正文**刻意不进 body**：PC 侧本来就没传（`output` 是空串 + `truncated_bytes`
      // 有值）。要看全文得按需拉，那是 UI 上的「拉取完整内容」按钮。
      body: result.output.value.isEmpty
          ? const ToolBodyNone()
          : ToolBodyText(result.output.value),
      truncatedBytes: result.truncatedBytes,
    );
  }

  // 其余一律兜底。**不猜字段名。**
  final String text = result.output.value;
  return ToolCard(
    shape: ToolShape.generic,
    summary: _resultSummary(result, text),
    body: text.isEmpty ? const ToolBodyNone() : ToolBodyText(text),
    truncatedBytes: result.truncatedBytes,
  );
}

// ── 摘要 ──────────────────────────────────────────────────────────────────────

String _fileSummary(LlmToolResultDetailFile d) {
  final int last = d.startLine + d.lineCount - 1;
  final String range = '第 ${d.startLine}–$last 行（共 ${d.totalLines} 行）';
  return '读取 ${d.path.value} $range';
}

String _resultSummary(ChatToolResult result, String text) {
  if (result.status != LlmToolStatus.ok) return '工具${statusLabel(result.status)}';
  // 未实测形态：退回正文首行，**不猜字段名**。
  final String first = text.split('\n').first;
  final String head = first.length > 60 ? '${first.substring(0, 60)}…' : first;
  return head.isEmpty ? '工具已完成' : head;
}

/// 没有 `title` 时，按形态从**入参**组一句。取不到字段就只报工具名。
String _callSummary(ToolShape shape, String name, JsonMap? input) {
  if (input == null) return name;
  final String? path = _str(input, 'file_path') ?? _str(input, 'path');
  return switch (shape) {
    ToolShape.read => path == null ? name : '读取 $path',
    ToolShape.edit => path == null ? name : '修改 $path',
    ToolShape.write => path == null ? name : '写入 $path',
    ToolShape.bash => _str(input, 'command') ?? name,
    ToolShape.search => _str(input, 'pattern') ?? _str(input, 'query') ?? name,
    ToolShape.web => _str(input, 'url') ?? _str(input, 'query') ?? name,
    ToolShape.todo || ToolShape.task || ToolShape.generic => name,
  };
}

// ── 展开态 ────────────────────────────────────────────────────────────────────

ToolBody _callBody(ToolShape shape, ChatToolCall call, JsonMap? input) {
  if (input == null) {
    // 入参不是对象（标量 / 数组 / null）。原样折叠成 JSON，别猜。
    return call.input.value == null
        ? const ToolBodyNone()
        : ToolBodyJson(prettyJson(call.input.value));
  }
  if (shape == ToolShape.edit) {
    final ToolBody? diff = _editDiff(input);
    if (diff != null) return diff;
    // 取不到 old/new ⇒ **退回折叠 JSON**，不画一个空 diff。
  }
  if (shape == ToolShape.write) {
    final String? content = _str(input, 'content');
    if (content != null) return ToolBodyText(content);
  }
  return ToolBodyJson(prettyJson(input));
}

/// 尝试从 `Edit` 的入参里取出前后文本画 diff。**任何一步取不到就返回 null。**
ToolBody? _editDiff(JsonMap input) {
  final String? oldText = _str(input, 'old_string');
  final String? newText = _str(input, 'new_string');
  if (oldText == null || newText == null) return null;
  return ToolBodyDiff(
    path: _str(input, 'file_path') ?? _str(input, 'path'),
    diff: diffLines(oldText, newText),
  );
}

// ── 取值：一律「取不到就 null」 ───────────────────────────────────────────────

/// 把 `LlmToolInput.value` 当对象读；不是对象就 null。
JsonMap? _map(Object? value) => value is JsonMap ? value : null;

/// 读一个字符串字段。**键不在、值不是字符串、值是空串，三种都算取不到。**
///
/// 空串也算取不到是刻意的：`file_path: ""` 画成「修改 」比画成「Edit」更糟。
///
/// 这条规则**也适用于 `old_string`**，尽管「空 old_string」在别处可以表示「新建文件」——
/// 那是 `Write` 的活，真实的 `Edit` 调用两边都非空。放宽它会让「字段存在但没值」
/// 和「新建文件」在下游变成同一件事，而前者其实是我们没看懂这次调用。
/// 宁可退回折叠 JSON：那至少如实说了「我只能把原始入参给你看」。
String? _str(JsonMap m, String key) {
  final Object? v = m[key];
  if (v is! String || v.isEmpty) return null;
  return v;
}

/// 缩进好的 JSON。**这是「未知形态」的最终归宿**，所以它必须对任何输入都不抛：
/// 含循环引用或不可编码的值时退回 `toString()`。
String prettyJson(Object? value) {
  try {
    return const JsonEncoder.withIndent('  ').convert(value);
  } on Object {
    return value.toString();
  }
}

/// 工具状态的人话。
String statusLabel(LlmToolStatus status) => switch (status) {
      LlmToolStatus.ok => '完成',
      LlmToolStatus.error => '出错',
      LlmToolStatus.denied => '被拒绝',
      LlmToolStatus.cancelled => '被取消',
      LlmToolStatus.unknown => '状态未知',
    };
