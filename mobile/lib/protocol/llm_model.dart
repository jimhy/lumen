/// LLM 子协议的值类型——块、用量、对话元信息、增量项、审批。
///
/// 权威定义是 `crates/lumen-protocol/src/llm.rs`；等价判定规则见
/// `crates/lumen-protocol/tests/golden/mobile/README.md`。**零 Flutter 依赖**。
///
/// ## 三个脱敏包装不是装饰
///
/// [LlmText] / [LlmPath] / [LlmToolInput] 的存在理由只有一个：让 `toString()` 打不出
/// 敏感内容。对话正文里可能有密码密钥、路径里有用户名与项目名、工具入参里装的就是文件
/// 正文与命令行。崩溃栈与审计日志都会打这些对象，裸 `String` 一进插值就原样落盘。
/// golden 的四条 `redact_*` 语料各守一处。
///
/// ## 未知内容一律降级、且**不回显**
///
/// 每个内部标签枚举都有一个 `Unknown` 兜底（对应 Rust 的 `#[serde(other)]`），未知
/// 标签值连同整个 body 一起丢弃。**不得**把未知键攒进 `extra` map 再原样写回：
/// PC → 手机是白名单转发，任何「先收下来、以后再说」的容器都是白名单的后门。
library;

import 'dart:convert';

import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';

// ── 脱敏包装 ─────────────────────────────────────────────────────────────────

/// 对话正文 / 工具输出等**敏感文本**。线上就是普通字符串。
final class LlmText {
  const LlmText(this.value);

  final String value;

  /// UTF-8 字节数。**不是** `value.length`（那是 UTF-16 码元数，中文少算 3 倍）。
  int get lengthInBytes => utf8Length(value);

  bool get isEmpty => value.isEmpty;

  String toJson() => value;

  static LlmText fromJson(Object? json) =>
      json is String ? LlmText(json) : throw const WireFormatException('LlmText 期望字符串');

  @override
  bool operator ==(Object other) => other is LlmText && other.value == value;

  @override
  int get hashCode => value.hashCode;

  /// 只打字符数，永不打正文。
  @override
  String toString() => 'LlmText(chars: ${value.runes.length})';
}

/// 被控端本机路径。路径本身就是环境信息（用户名、项目名、内网盘符）。
final class LlmPath {
  const LlmPath(this.value);

  final String value;

  String toJson() => value;

  static LlmPath fromJson(Object? json) =>
      json is String ? LlmPath(json) : throw const WireFormatException('LlmPath 期望字符串');

  @override
  bool operator ==(Object other) => other is LlmPath && other.value == value;

  @override
  int get hashCode => value.hashCode;

  @override
  String toString() => 'LlmPath(<redacted>)';
}

/// 工具入参——**本协议唯一的任意 JSON 口子**。
///
/// 每个工具的 schema 自定义且随上游迭代，任何归一化都是有损且必然追不上，故原样透传。
/// **它是「工具入参」的口子，不是「任意 JSON」的口子**：绝不允许被复用来夹带 CLI 原生
/// 事件、hook 输出或环境信息。
final class LlmToolInput {
  const LlmToolInput(this.value);

  /// 原样保留的 JSON 值（对象 / 数组 / 标量都可能）。
  final Object? value;

  Object? toJson() => value;

  static LlmToolInput fromJson(Object? json) => LlmToolInput(json);

  /// 只打字段数与字节数，**不许** `jsonEncode` 整个 map。
  @override
  String toString() {
    final int fields = value is JsonMap ? (value! as JsonMap).length : 0;
    int bytes;
    try {
      bytes = utf8Length(jsonEncode(value));
    } on Object {
      bytes = 0;
    }
    return 'LlmToolInput(fields: $fields, bytes: $bytes)';
  }
}

// ── 内部标签枚举：工具状态 ────────────────────────────────────────────────────

/// 工具执行结果状态。
///
/// 上游 `is_error` **可能整个键都不存在**——缺键按 [ok] 解释，既不能当必填去读
/// （正常路径全灭），也不能拿「键是否存在」当分支依据（上游哪天补上 `false` 就静默改语义）。
enum LlmToolStatus implements WireNamed {
  ok('Ok'),
  error('Error'),

  /// 被用户在权限审批里拒绝。
  denied('Denied'),
  cancelled('Cancelled'),

  /// 更高版本对端的未知状态（**只用于反序列化兜底，永不发送**）。
  unknown('Unknown');

  const LlmToolStatus(this.wire);

  @override
  final String wire;

  JsonMap toJson() => taggedJson('kind', wire);

  static LlmToolStatus fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmToolStatus',
        cases: <String, LlmToolStatus Function(JsonMap)>{
          for (final LlmToolStatus s in LlmToolStatus.values)
            if (s != LlmToolStatus.unknown) s.wire: (_) => s,
        },
        fallback: LlmToolStatus.unknown,
      );
}

// ── 内部标签枚举：对话运行态 ──────────────────────────────────────────────────

/// 对话运行态。
enum LlmConvState implements WireNamed {
  starting('Starting'),
  idle('Idle'),
  running('Running'),
  waitingPermission('WaitingPermission'),
  exited('Exited'),
  unknown('Unknown');

  const LlmConvState(this.wire);

  @override
  final String wire;

  JsonMap toJson() => taggedJson('kind', wire);

  static LlmConvState fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmConvState',
        cases: <String, LlmConvState Function(JsonMap)>{
          for (final LlmConvState s in LlmConvState.values)
            if (s != LlmConvState.unknown) s.wire: (_) => s,
        },
        fallback: LlmConvState.unknown,
      );
}

// ── 内部标签枚举：审批裁决方 ──────────────────────────────────────────────────

/// 一次权限审批最终由谁裁决。
enum LlmPermissionResolvedBy implements WireNamed {
  user('User'),
  timeout('Timeout'),
  policyAuto('PolicyAuto'),
  convClosed('ConvClosed'),
  unknown('Unknown');

  const LlmPermissionResolvedBy(this.wire);

  @override
  final String wire;

  JsonMap toJson() => taggedJson('kind', wire);

  static LlmPermissionResolvedBy fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmPermissionResolvedBy',
        cases: <String, LlmPermissionResolvedBy Function(JsonMap)>{
          for (final LlmPermissionResolvedBy s in LlmPermissionResolvedBy.values)
            if (s != LlmPermissionResolvedBy.unknown) s.wire: (_) => s,
        },
        fallback: LlmPermissionResolvedBy.unknown,
      );
}

// ── 附件 ─────────────────────────────────────────────────────────────────────

/// 图片 / 文件附件。**内容不走本协议**：先用 `PutBegin`/`PutChunk`/`PutEnd` 把字节传到
/// `ConvStarted` 回带的 `attach_dir`，再在 `Send` 里按路径引用。
final class LlmAttachment {
  const LlmAttachment({
    required this.path,
    required this.name,
    required this.mime,
    required this.len,
  });

  final LlmPath path;

  /// 展示用文件名。**是 [LlmText] 不是裸 `String`**：相册文件名同样是用户内容
  /// （人名、单据号、`我的身份证-xxx.png`）。同结构里 `path` 已脱敏，只脱一半是半道门。
  final LlmText name;
  final String mime;
  final int len;

  JsonMap toJson() => <String, Object?>{
        'path': path.toJson(),
        'name': name.toJson(),
        'mime': mime,
        'len': len,
      };

  static LlmAttachment fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmAttachment');
    return LlmAttachment(
      path: LlmPath(readString(m, 'path')),
      name: LlmText(readString(m, 'name')),
      mime: readString(m, 'mime'),
      len: readInt(m, 'len'),
    );
  }

  @override
  String toString() =>
      'LlmAttachment(path: $path, name: $name, mime: $mime, len: $len)';
}

// ── 思考块内容 ───────────────────────────────────────────────────────────────

/// 思考块的**内容可得性**——互斥三态，不是「一个可能为空的 text」。
///
/// 实测 Claude Opus 5 的 `thinking` 恒为空串且 CLI 无开关，若沿用「可空文本」形状，
/// 手机端只能渲染一个永远展不开的折叠块，更糟的是那个字段会**诱导被控端往里塞东西**
/// （最顺手的就是把 signature 或整段上游 JSON 填进去），白名单铁律当场破功。
/// [LlmThinkingOmitted] 是无字段变体，**类型上就没有可绑定的文本**。
sealed class LlmThinking {
  const LlmThinking();

  JsonMap toJson();

  static LlmThinking fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmThinking',
        cases: <String, LlmThinking Function(JsonMap)>{
          'Text': (JsonMap m) => LlmThinkingText(LlmText(readString(m, 'text'))),
          'Redacted': (_) => const LlmThinkingRedacted(),
          'Omitted': (_) => const LlmThinkingOmitted(),
        },
        fallback: const LlmThinkingUnknown(),
      );
}

/// 有明文可展示。**只有本分支能接 `TextAppend`。**
final class LlmThinkingText extends LlmThinking {
  const LlmThinkingText(this.text);

  final LlmText text;

  @override
  JsonMap toJson() => taggedJson('kind', 'Text', <String, Object?>{'text': text.toJson()});

  @override
  String toString() => 'LlmThinking.Text(text: $text)';
}

/// 上游**策略**加密、明文不可得（Anthropic 的 `redacted_thinking`）。
///
/// 与 [LlmThinkingOmitted] 的区别是**归因**，手机端文案不同，别合并。
final class LlmThinkingRedacted extends LlmThinking {
  const LlmThinkingRedacted();

  @override
  JsonMap toJson() => taggedJson('kind', 'Redacted');

  @override
  String toString() => 'LlmThinking.Redacted';
}

/// **CLI 展示设置**省略了思考内容（实测的 Claude 分支）。只能当「正在思考」的状态信号。
final class LlmThinkingOmitted extends LlmThinking {
  const LlmThinkingOmitted();

  @override
  JsonMap toJson() => taggedJson('kind', 'Omitted');

  @override
  String toString() => 'LlmThinking.Omitted';
}

/// 未知思考形态（反序列化兜底）。
final class LlmThinkingUnknown extends LlmThinking {
  const LlmThinkingUnknown();

  @override
  JsonMap toJson() => taggedJson('kind', 'Unknown');

  @override
  String toString() => 'LlmThinking.Unknown';
}

// ── 工具结果的结构化摘要 ──────────────────────────────────────────────────────

/// 工具结果的**结构化摘要**——只装元数据，不装内容。
///
/// 有了它，卡片能渲染成「读取 README.md 第 1–165 行（共 165 行）」而**完全不需要把
/// 11 720 字符的文件正文推到手机**。正文的唯一承载点仍是 `ToolResult.output`。
sealed class LlmToolResultDetail {
  const LlmToolResultDetail();

  JsonMap toJson();

  static LlmToolResultDetail fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmToolResultDetail',
        cases: <String, LlmToolResultDetail Function(JsonMap)>{
          'File': (JsonMap m) => LlmToolResultDetailFile(
                path: LlmPath(readString(m, 'path')),
                startLine: readInt(m, 'start_line'),
                lineCount: readInt(m, 'line_count'),
                totalLines: readInt(m, 'total_lines'),
              ),
        },
        fallback: const LlmToolResultDetailUnknown(),
      );
}

/// 文件读取型结果。刻意**没有** `content` 字段。
final class LlmToolResultDetailFile extends LlmToolResultDetail {
  const LlmToolResultDetailFile({
    required this.path,
    required this.startLine,
    required this.lineCount,
    required this.totalLines,
  });

  final LlmPath path;

  /// 本次读取的起始行（1 基）。
  final int startLine;

  /// 本次读取的行数。
  final int lineCount;

  /// 文件总行数。`totalLines > startLine + lineCount - 1` 时应展示「仅读取了部分」。
  final int totalLines;

  @override
  JsonMap toJson() => taggedJson('kind', 'File', <String, Object?>{
        'path': path.toJson(),
        'start_line': startLine,
        'line_count': lineCount,
        'total_lines': totalLines,
      });

  @override
  String toString() => 'LlmToolResultDetail.File(path: $path, '
      'startLine: $startLine, lineCount: $lineCount, totalLines: $totalLines)';
}

/// 未知摘要形态（反序列化兜底，卡片退回纯文本样式）。
final class LlmToolResultDetailUnknown extends LlmToolResultDetail {
  const LlmToolResultDetailUnknown();

  @override
  JsonMap toJson() => taggedJson('kind', 'Unknown');

  @override
  String toString() => 'LlmToolResultDetail.Unknown';
}

// ── 归一化对话块 ─────────────────────────────────────────────────────────────

/// **归一化对话块**——与 CLI 厂商无关的最小公共模型。
sealed class LlmBlock {
  const LlmBlock();

  JsonMap toJson();

  static LlmBlock fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmBlock',
        cases: <String, LlmBlock Function(JsonMap)>{
          'Text': (JsonMap m) => LlmBlockText(LlmText(readString(m, 'text'))),
          'Thinking': (JsonMap m) =>
              LlmBlockThinking(LlmThinking.fromJson(m['content'])),
          'ToolUse': (JsonMap m) => LlmBlockToolUse(
                callId: readString(m, 'call_id'),
                name: readString(m, 'name'),
                title: m['title'] == null ? null : LlmText(readString(m, 'title')),
                input: LlmToolInput(m['input']),
                truncatedBytes: readIntOpt(m, 'truncated_bytes'),
              ),
          'ToolResult': (JsonMap m) => LlmBlockToolResult(
                callId: readString(m, 'call_id'),
                status: LlmToolStatus.fromJson(m['status']),
                output: LlmText(readString(m, 'output')),
                truncatedBytes: readIntOpt(m, 'truncated_bytes'),
                detail: m['detail'] == null
                    ? null
                    : LlmToolResultDetail.fromJson(m['detail']),
              ),
          'Image': (JsonMap m) => LlmBlockImage(LlmAttachment.fromJson(m['attachment'])),
          'Error': (JsonMap m) => LlmBlockError(
                code: llmErrorCode.read(m, 'code'),
                message: LlmText(readString(m, 'message')),
              ),
        },
        fallback: const LlmBlockUnknown(),
      );
}

/// 普通文本（Markdown 原文，渲染由控制端负责）。
final class LlmBlockText extends LlmBlock {
  const LlmBlockText(this.text);

  final LlmText text;

  @override
  JsonMap toJson() => taggedJson('kind', 'Text', <String, Object?>{'text': text.toJson()});

  @override
  String toString() => 'LlmBlock.Text(text: $text)';
}

/// 思考 / 推理过程。块本身保留（思考**发生过**有展示价值），缺的只是可展开的正文。
final class LlmBlockThinking extends LlmBlock {
  const LlmBlockThinking(this.content);

  final LlmThinking content;

  @override
  JsonMap toJson() =>
      taggedJson('kind', 'Thinking', <String, Object?>{'content': content.toJson()});

  @override
  String toString() => 'LlmBlock.Thinking(content: $content)';
}

/// 工具调用。入参不流式——上游是整块给出的。
final class LlmBlockToolUse extends LlmBlock {
  const LlmBlockToolUse({
    required this.callId,
    required this.name,
    required this.input,
    this.title,
    this.truncatedBytes,
  });

  /// 上游给的调用 id，用于与 [LlmBlockToolResult] 配对。
  final String callId;
  final String name;

  /// 人类可读的一行摘要。
  final LlmText? title;
  final LlmToolInput input;

  /// 入参被裁掉的字节数。**这个字段是「不许丢块」的类型保证**：没有它，被控端面对
  /// 5 MiB 的 `Write` 入参只有「发出去炸链路」与「静默丢掉整个 ToolUse」两条路。
  final int? truncatedBytes;

  @override
  JsonMap toJson() {
    final JsonMap body = <String, Object?>{'call_id': callId, 'name': name};
    writeOpt(body, 'title', title?.toJson());
    body['input'] = input.toJson();
    writeOpt(body, 'truncated_bytes', truncatedBytes);
    return taggedJson('kind', 'ToolUse', body);
  }

  @override
  String toString() => 'LlmBlock.ToolUse(callId: $callId, name: $name, '
      'title: $title, input: $input, truncatedBytes: $truncatedBytes)';
}

/// 工具结果。
final class LlmBlockToolResult extends LlmBlock {
  const LlmBlockToolResult({
    required this.callId,
    required this.status,
    required this.output,
    this.truncatedBytes,
    this.detail,
  });

  final String callId;
  final LlmToolStatus status;

  /// 结果正文（上游实测是**纯字符串**而非块数组）。
  final LlmText output;
  final int? truncatedBytes;

  /// 结构化摘要；`null` = 上游没给或是本端尚未建模的形态，此时退回按 [output] 渲染。
  final LlmToolResultDetail? detail;

  @override
  JsonMap toJson() {
    final JsonMap body = <String, Object?>{
      'call_id': callId,
      'status': status.toJson(),
      'output': output.toJson(),
    };
    writeOpt(body, 'truncated_bytes', truncatedBytes);
    writeOpt(body, 'detail', detail?.toJson());
    return taggedJson('kind', 'ToolResult', body);
  }

  @override
  String toString() => 'LlmBlock.ToolResult(callId: $callId, status: $status, '
      'output: $output, truncatedBytes: $truncatedBytes, detail: $detail)';
}

/// 图片块（用户附件回显或模型产出的图）。
final class LlmBlockImage extends LlmBlock {
  const LlmBlockImage(this.attachment);

  final LlmAttachment attachment;

  @override
  JsonMap toJson() =>
      taggedJson('kind', 'Image', <String, Object?>{'attachment': attachment.toJson()});

  @override
  String toString() => 'LlmBlock.Image(attachment: $attachment)';
}

/// **错误块**——某次调用 / 某条消息是错误，而不是模型的正常输出。
///
/// 这是硬性要求：实测认证失败以一条**普通 assistant 消息 + 普通 text 块**投递，
/// 只有顶层 `is_api_error_message` 能识别。顺序反了就会把「403 请求被拒」渲染成
/// 模型的正常回复气泡，用户完全看不出这是故障。
final class LlmBlockError extends LlmBlock {
  const LlmBlockError({required this.code, required this.message});

  final OpenEnum<LlmErrorCodeKnown> code;
  final LlmText message;

  @override
  JsonMap toJson() => taggedJson('kind', 'Error', <String, Object?>{
        'code': code.toJson(),
        'message': message.toJson(),
      });

  @override
  String toString() => 'LlmBlock.Error(code: $code, message: $message)';
}

/// 未知块类型（反序列化兜底）。气泡流只少一块，不整帧丢弃。
final class LlmBlockUnknown extends LlmBlock {
  const LlmBlockUnknown();

  @override
  JsonMap toJson() => taggedJson('kind', 'Unknown');

  @override
  String toString() => 'LlmBlock.Unknown';
}

// ── 块条目 ───────────────────────────────────────────────────────────────────

/// **带归属信息的块条目**。
final class LlmBlockEntry {
  const LlmBlockEntry({
    required this.blockId,
    required this.block,
    this.parentCallId,
  });

  /// 块在**本轮内**的序号。跨轮重新从 0 起——`(turn, blockId)` 才唯一。
  /// 同一轮内 `user` 与 `assistant` **共用同一编号空间**，只拿 blockId 当 key 会让
  /// 助手的首个流式块静默覆盖用户气泡。
  final int blockId;

  /// **嵌套归属**：本块属于哪一次工具调用产生的子代理输出。`null` = 主对话直接产出。
  /// 不要用空串表达「无归属」。
  final String? parentCallId;

  final LlmBlock block;

  JsonMap toJson() {
    final JsonMap out = <String, Object?>{'block_id': blockId};
    writeOpt(out, 'parent_call_id', parentCallId);
    out['block'] = block.toJson();
    return out;
  }

  static LlmBlockEntry fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmBlockEntry');
    return LlmBlockEntry(
      blockId: readInt(m, 'block_id'),
      parentCallId: readStringOpt(m, 'parent_call_id'),
      block: LlmBlock.fromJson(m['block']),
    );
  }

  @override
  String toString() => 'LlmBlockEntry(blockId: $blockId, '
      'parentCallId: $parentCallId, block: $block)';
}

// ── 用量与成败 ───────────────────────────────────────────────────────────────

/// 一轮的 token / 花费统计。全部字段可缺省（上游给不出的填 0），且**都不带
/// skip_serializing_if**——输出里一定出现。
final class LlmUsage {
  const LlmUsage({
    this.inputTokens = 0,
    this.outputTokens = 0,
    this.cacheReadTokens = 0,
    this.cacheWriteTokens = 0,
    this.costMicroUsd = 0,
    this.contextUsed = 0,
    this.contextLimit = 0,
    this.maxOutputTokens = 0,
  });

  final int inputTokens;
  final int outputTokens;
  final int cacheReadTokens;
  final int cacheWriteTokens;

  /// 花费，单位**微美元**（1e-6 USD）。用整数避免浮点累加误差。
  final int costMicroUsd;

  /// **当前上下文占用 token**（进度条的分子）。`0` = 未知。
  /// 口径是「本轮最后一次 API 调用」，不是整轮累加——累加出来的是同一份上下文被数了 N 遍。
  final int contextUsed;

  /// 上下文窗口上限（进度条的分母）。`0` = 未知，此时**不显示百分比**，
  /// 也**不得**按模型名查硬编码表去「补」这个值。
  final int contextLimit;

  /// 本轮所用模型的**单次最大输出 token**。与 [contextLimit] 是两个不同的上限，
  /// 混为一谈会给出完全错误的排障建议。
  final int maxOutputTokens;

  JsonMap toJson() => <String, Object?>{
        'input_tokens': inputTokens,
        'output_tokens': outputTokens,
        'cache_read_tokens': cacheReadTokens,
        'cache_write_tokens': cacheWriteTokens,
        'cost_micro_usd': costMicroUsd,
        'context_used': contextUsed,
        'context_limit': contextLimit,
        'max_output_tokens': maxOutputTokens,
      };

  static LlmUsage fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmUsage');
    return LlmUsage(
      inputTokens: readIntOr(m, 'input_tokens', 0),
      outputTokens: readIntOr(m, 'output_tokens', 0),
      cacheReadTokens: readIntOr(m, 'cache_read_tokens', 0),
      cacheWriteTokens: readIntOr(m, 'cache_write_tokens', 0),
      costMicroUsd: readIntOr(m, 'cost_micro_usd', 0),
      contextUsed: readIntOr(m, 'context_used', 0),
      contextLimit: readIntOr(m, 'context_limit', 0),
      maxOutputTokens: readIntOr(m, 'max_output_tokens', 0),
    );
  }

  @override
  String toString() => 'LlmUsage(input: $inputTokens, output: $outputTokens, '
      'cacheRead: $cacheReadTokens, cacheWrite: $cacheWriteTokens, '
      'costMicroUsd: $costMicroUsd, contextUsed: $contextUsed, '
      'contextLimit: $contextLimit, maxOutputTokens: $maxOutputTokens)';
}

/// 一轮的**权威成败判定**。
///
/// **不得靠上游 `subtype` 判成败**：两次采样里同一个 `subtype = "success"` 对应相反的
/// 成败。判定顺序只能是 [isError] → [terminalReason] → [apiErrorStatus]。
final class LlmTurnOutcome {
  const LlmTurnOutcome({
    this.isError = false,
    this.terminalReason,
    this.apiErrorStatus,
  });

  /// **唯一权威**的成败标志。
  final bool isError;

  /// 终止通道。`null` = 上游没给。
  final OpenEnum<LlmTerminalReasonKnown>? terminalReason;

  /// 上游 HTTP 状态码（实测 403）。`null` = 与 HTTP 无关或上游没给。
  final int? apiErrorStatus;

  JsonMap toJson() {
    final JsonMap out = <String, Object?>{'is_error': isError};
    writeOpt(out, 'terminal_reason', terminalReason?.toJson());
    writeOpt(out, 'api_error_status', apiErrorStatus);
    return out;
  }

  static LlmTurnOutcome fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmTurnOutcome');
    return LlmTurnOutcome(
      isError: readBoolOr(m, 'is_error', fallback: false),
      terminalReason: llmTerminalReason.readOpt(m, 'terminal_reason'),
      apiErrorStatus: readIntOpt(m, 'api_error_status'),
    );
  }

  @override
  String toString() => 'LlmTurnOutcome(isError: $isError, '
      'terminalReason: $terminalReason, apiErrorStatus: $apiErrorStatus)';
}

// ── 账号额度 ─────────────────────────────────────────────────────────────────

/// **账号额度状态**。用户在外面远程盯着 PC 跑 agent 时，「还能跑多久 / 几点恢复」
/// 比任何 token 数都重要，而这是唯一给得出**恢复时刻**的通道。
///
/// 除 [status] 外全部可缺省，缺省一律解释成「上游没说」，**不得**解释成「否」。
final class LlmRateLimit {
  const LlmRateLimit({
    required this.status,
    this.window,
    this.resetsAtSecs,
    this.overageStatus,
    this.overageDisabledReason,
    this.usingOverage,
  });

  final OpenEnum<LlmRateLimitStatusKnown> status;
  final OpenEnum<LlmRateLimitWindowKnown>? window;

  /// 额度重置时刻，**Unix 秒**——本模块其它所有时间戳都是**毫秒**。
  /// 字段名带 `Secs` 就是为了让「忘了乘 1000」在读代码时就露馅。
  /// 换算前要防溢出：本字段对端可控，取值可达 `i64` 上界。
  final int? resetsAtSecs;

  final OpenEnum<LlmOverageStatusKnown>? overageStatus;
  final OpenEnum<LlmOverageDisabledReasonKnown>? overageDisabledReason;

  /// 当前是否正在吃超额额度。
  ///
  /// **可空布尔而非裸布尔**：`null` = 上游没给这个键，与 `false` 是两件事。
  /// 把 `null` 渲染成「未在超额」正是这里点名要避免的错误计费提示——
  /// `null` 的正确处理是**不显示超额相关的任何文案**。
  final bool? usingOverage;

  JsonMap toJson() {
    final JsonMap out = <String, Object?>{'status': status.toJson()};
    writeOpt(out, 'window', window?.toJson());
    writeOpt(out, 'resets_at_secs', resetsAtSecs);
    writeOpt(out, 'overage_status', overageStatus?.toJson());
    writeOpt(out, 'overage_disabled_reason', overageDisabledReason?.toJson());
    writeOpt(out, 'using_overage', usingOverage);
    return out;
  }

  static LlmRateLimit fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmRateLimit');
    return LlmRateLimit(
      status: llmRateLimitStatus.read(m, 'status'),
      window: llmRateLimitWindow.readOpt(m, 'window'),
      resetsAtSecs: readIntOpt(m, 'resets_at_secs'),
      overageStatus: llmOverageStatus.readOpt(m, 'overage_status'),
      overageDisabledReason:
          llmOverageDisabledReason.readOpt(m, 'overage_disabled_reason'),
      usingOverage: readBoolOpt(m, 'using_overage'),
    );
  }

  @override
  String toString() => 'LlmRateLimit(status: $status, window: $window, '
      'resetsAtSecs: $resetsAtSecs, overageStatus: $overageStatus, '
      'overageDisabledReason: $overageDisabledReason, usingOverage: $usingOverage)';
}

// ── agent 能力与清单 ──────────────────────────────────────────────────────────

/// 某个 agent 支持的可选能力。全部缺省即「不支持」，保证新增能力位不会让老端整帧失败。
final class LlmAgentFeatures {
  const LlmAgentFeatures({
    this.streaming = false,
    this.thinking = false,
    this.thinkingText = false,
    this.interactivePermission = false,
    this.resume = false,
    this.attachments = false,
    this.interrupt = false,
  });

  final bool streaming;

  /// 会产出思考块（**只说明块会出现，不保证有正文**）。
  final bool thinking;

  /// 思考块**带得出可展示正文**。与 [thinking] 分成两位不是冗余：
  /// `thinking && !thinkingText` 时只该画状态条——画了折叠箭头却永远展不开是明确的产品缺陷。
  final bool thinkingText;

  final bool interactivePermission;
  final bool resume;
  final bool attachments;
  final bool interrupt;

  JsonMap toJson() => <String, Object?>{
        'streaming': streaming,
        'thinking': thinking,
        'thinking_text': thinkingText,
        'interactive_permission': interactivePermission,
        'resume': resume,
        'attachments': attachments,
        'interrupt': interrupt,
      };

  static LlmAgentFeatures fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmAgentFeatures');
    return LlmAgentFeatures(
      streaming: readBoolOr(m, 'streaming', fallback: false),
      thinking: readBoolOr(m, 'thinking', fallback: false),
      thinkingText: readBoolOr(m, 'thinking_text', fallback: false),
      interactivePermission:
          readBoolOr(m, 'interactive_permission', fallback: false),
      resume: readBoolOr(m, 'resume', fallback: false),
      attachments: readBoolOr(m, 'attachments', fallback: false),
      interrupt: readBoolOr(m, 'interrupt', fallback: false),
    );
  }

  @override
  String toString() => 'LlmAgentFeatures(streaming: $streaming, '
      'thinking: $thinking, thinkingText: $thinkingText, '
      'interactivePermission: $interactivePermission, resume: $resume, '
      'attachments: $attachments, interrupt: $interrupt)';
}

/// 被控端可用的一个 agent。
///
/// **刻意不含** `tools` / `plugins` / `mcp_servers` / `memory_paths`：那些是本机环境信息，
/// 一律不上行。
final class LlmAgentInfo {
  const LlmAgentInfo({
    required this.agent,
    required this.available,
    this.version,
    this.models = const <String>[],
    this.permissionModes = const <OpenEnum<LlmPermissionModeKnown>>[],
    this.features = const LlmAgentFeatures(),
  });

  final OpenEnum<LlmAgentKindKnown> agent;

  /// 是否真的可用。`false` 时控制端应置灰但仍展示，便于用户知道「装了就能用」。
  final bool available;

  /// CLI 版本号。仅用于排障，**不作能力判据**（判据是 [features]）。
  final String? version;

  final List<String> models;
  final List<OpenEnum<LlmPermissionModeKnown>> permissionModes;
  final LlmAgentFeatures features;

  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'agent': agent.toJson(),
      'available': available,
    };
    writeOpt(out, 'version', version);
    out['models'] = models;
    out['permission_modes'] =
        permissionModes.map((OpenEnum<LlmPermissionModeKnown> m) => m.toJson()).toList();
    out['features'] = features.toJson();
    return out;
  }

  static LlmAgentInfo fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmAgentInfo');
    return LlmAgentInfo(
      agent: llmAgentKind.read(m, 'agent'),
      available: readBool(m, 'available'),
      version: readStringOpt(m, 'version'),
      models: readListOr(m, 'models', asWireString),
      permissionModes:
          readListOr(m, 'permission_modes', llmPermissionMode.decode),
      features: readObjectOr(
        m,
        'features',
        LlmAgentFeatures.fromJson,
        LlmAgentFeatures.new,
      ),
    );
  }

  @override
  String toString() => 'LlmAgentInfo(agent: $agent, available: $available, '
      'version: $version, models: $models, permissionModes: $permissionModes, '
      'features: $features)';
}

// ── 对话元信息 ───────────────────────────────────────────────────────────────

/// 「这个对话是从哪个终端窗格拉起来的」弱关联。
///
/// **纯展示 / 跳转用途，绝不作为路由键**——路由键恒为 `convId`。窗格可能早已关闭，
/// 这里存的是历史快照、**允许悬空**，解析不到对应窗格时静默忽略即可。
final class PaneRef {
  const PaneRef({required this.tabId, required this.sessionId});

  final int tabId;
  final int sessionId;

  JsonMap toJson() => <String, Object?>{'tab_id': tabId, 'session_id': sessionId};

  static PaneRef fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'PaneRef');
    return PaneRef(tabId: readInt(m, 'tab_id'), sessionId: readInt(m, 'session_id'));
  }

  @override
  String toString() => 'PaneRef(tabId: $tabId, sessionId: $sessionId)';
}

/// 对话元信息（列表项 + 基线帧共用）。
final class LlmConvMeta {
  const LlmConvMeta({
    required this.convId,
    required this.convGeneration,
    required this.agent,
    required this.cwd,
    required this.title,
    required this.state,
    required this.curTurn,
    required this.createdMs,
    required this.updatedMs,
    this.model,
    this.permissionMode,
    this.cliSessionId,
    this.origin,
    this.usage = const LlmUsage(),
  });

  final int convId;
  final int convGeneration;
  final OpenEnum<LlmAgentKindKnown> agent;
  final LlmPath cwd;
  final LlmText title;
  final LlmConvState state;
  final String? model;
  final OpenEnum<LlmPermissionModeKnown>? permissionMode;

  /// CLI 自己的会话 id。**与 `convId` 是两个空间**，不可互换。
  final String? cliSessionId;

  final PaneRef? origin;

  /// 当前（最新）轮次号；`0` 表示尚无任何一轮。
  final int curTurn;

  /// 创建时刻（Unix **毫秒**）。
  final int createdMs;

  /// 最后活动时刻（Unix **毫秒**）。
  final int updatedMs;

  final LlmUsage usage;

  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'conv_id': convId,
      'conv_generation': convGeneration,
      'agent': agent.toJson(),
      'cwd': cwd.toJson(),
      'title': title.toJson(),
      'state': state.toJson(),
    };
    writeOpt(out, 'model', model);
    writeOpt(out, 'permission_mode', permissionMode?.toJson());
    writeOpt(out, 'cli_session_id', cliSessionId);
    writeOpt(out, 'origin', origin?.toJson());
    out['cur_turn'] = curTurn;
    out['created_ms'] = createdMs;
    out['updated_ms'] = updatedMs;
    out['usage'] = usage.toJson();
    return out;
  }

  static LlmConvMeta fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmConvMeta');
    return LlmConvMeta(
      convId: readInt(m, 'conv_id'),
      convGeneration: readInt(m, 'conv_generation'),
      agent: llmAgentKind.read(m, 'agent'),
      cwd: LlmPath(readString(m, 'cwd')),
      title: LlmText(readString(m, 'title')),
      state: LlmConvState.fromJson(m['state']),
      model: readStringOpt(m, 'model'),
      permissionMode: llmPermissionMode.readOpt(m, 'permission_mode'),
      cliSessionId: readStringOpt(m, 'cli_session_id'),
      origin: readObjectOpt(m, 'origin', PaneRef.fromJson),
      curTurn: readInt(m, 'cur_turn'),
      createdMs: readInt(m, 'created_ms'),
      updatedMs: readInt(m, 'updated_ms'),
      usage: readObjectOr(m, 'usage', LlmUsage.fromJson, LlmUsage.new),
    );
  }

  @override
  String toString() => 'LlmConvMeta(convId: $convId, '
      'convGeneration: $convGeneration, agent: $agent, cwd: $cwd, title: $title, '
      'state: $state, model: $model, permissionMode: $permissionMode, '
      'cliSessionId: $cliSessionId, origin: $origin, curTurn: $curTurn, '
      'createdMs: $createdMs, updatedMs: $updatedMs, usage: $usage)';
}

// ── 轮记录 ───────────────────────────────────────────────────────────────────

/// 一轮的**定稿记录**（历史分页与单轮快照用）。
final class LlmTurnRecord {
  const LlmTurnRecord({
    required this.turn,
    required this.startedMs,
    this.user = const <LlmBlockEntry>[],
    this.assistant = const <LlmBlockEntry>[],
    this.stop,
    this.outcome = const LlmTurnOutcome(),
    this.usage = const LlmUsage(),
    this.endedMs,
    this.complete = false,
    this.blocksOmitted = 0,
  });

  final int turn;

  /// 用户侧块（文本 + 附件回显）。
  final List<LlmBlockEntry> user;

  /// 助手侧块（按产出顺序，即渲染顺序）。
  final List<LlmBlockEntry> assistant;

  /// 停止原因；`null` = 本轮尚未结束。
  final OpenEnum<LlmStopReasonKnown>? stop;

  final LlmTurnOutcome outcome;
  final LlmUsage usage;
  final int startedMs;
  final int? endedMs;

  /// `true` = 完整定稿；`false` = 因积压降级丢过中间增量，内容可能有缺口。
  final bool complete;

  /// 本记录**为满足单帧字节预算而删掉的块数**（`0` = 一块没删，常态）。
  ///
  /// 与 [complete] 分工不同：`complete == false` 说的是「**流式过程中**丢过增量」，
  /// 本字段说的是「**这一帧装不下**，定稿时主动删的」。控制端**必须读这个字段**
  /// 并展示「本轮有 N 块因过大未能同步」——本帧是缺口的唯一补齐通道。
  final int blocksOmitted;

  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'turn': turn,
      'user': user.map((LlmBlockEntry e) => e.toJson()).toList(),
      'assistant': assistant.map((LlmBlockEntry e) => e.toJson()).toList(),
    };
    writeOpt(out, 'stop', stop?.toJson());
    out['outcome'] = outcome.toJson();
    out['usage'] = usage.toJson();
    out['started_ms'] = startedMs;
    writeOpt(out, 'ended_ms', endedMs);
    out['complete'] = complete;
    out['blocks_omitted'] = blocksOmitted;
    return out;
  }

  static LlmTurnRecord fromJson(Object? json) {
    final JsonMap m = asJsonMap(json, 'LlmTurnRecord');
    return LlmTurnRecord(
      turn: readInt(m, 'turn'),
      user: readListOr(m, 'user', LlmBlockEntry.fromJson),
      assistant: readListOr(m, 'assistant', LlmBlockEntry.fromJson),
      stop: llmStopReason.readOpt(m, 'stop'),
      outcome:
          readObjectOr(m, 'outcome', LlmTurnOutcome.fromJson, LlmTurnOutcome.new),
      usage: readObjectOr(m, 'usage', LlmUsage.fromJson, LlmUsage.new),
      startedMs: readInt(m, 'started_ms'),
      endedMs: readIntOpt(m, 'ended_ms'),
      complete: readBoolOr(m, 'complete', fallback: false),
      blocksOmitted: readIntOr(m, 'blocks_omitted', 0),
    );
  }

  @override
  String toString() => 'LlmTurnRecord(turn: $turn, user: $user, '
      'assistant: $assistant, stop: $stop, outcome: $outcome, usage: $usage, '
      'startedMs: $startedMs, endedMs: $endedMs, complete: $complete, '
      'blocksOmitted: $blocksOmitted)';
}

// ── 流式增量项 ───────────────────────────────────────────────────────────────

/// 流式增量项。多条打包进一个 `Delta` 帧。
sealed class LlmDeltaItem {
  const LlmDeltaItem();

  JsonMap toJson();

  static LlmDeltaItem fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmDeltaItem',
        cases: <String, LlmDeltaItem Function(JsonMap)>{
          'BlockStart': (JsonMap m) =>
              LlmDeltaBlockStart(LlmBlockEntry.fromJson(m['entry'])),
          'TextAppend': (JsonMap m) => LlmDeltaTextAppend(
                blockId: readInt(m, 'block_id'),
                text: LlmText(readString(m, 'text')),
              ),
          'BlockEnd': (JsonMap m) => LlmDeltaBlockEnd(
                blockId: readInt(m, 'block_id'),
                block: m['block'] == null ? null : LlmBlock.fromJson(m['block']),
              ),
          'Dropped': (JsonMap m) => LlmDeltaDropped(
                blockId: readInt(m, 'block_id'),
                bytes: readInt(m, 'bytes'),
              ),
        },
        fallback: const LlmDeltaUnknown(),
      );
}

/// 新块开始。`entry.block` 携带该块的定型头部。
final class LlmDeltaBlockStart extends LlmDeltaItem {
  const LlmDeltaBlockStart(this.entry);

  final LlmBlockEntry entry;

  @override
  JsonMap toJson() =>
      taggedJson('kind', 'BlockStart', <String, Object?>{'entry': entry.toJson()});

  @override
  String toString() => 'LlmDeltaItem.BlockStart(entry: $entry)';
}

/// 文本追加——**高频帧的主力**。
///
/// 归约器必须**先拼接、再交渲染层做字素簇切分**：增量可能把一个 ZWJ 家庭 emoji 拆两半，
/// 前后半各自都是合法字符串，协议层无从阻止；每收一条就按字素簇渲染在 Flutter 上是可见闪烁。
final class LlmDeltaTextAppend extends LlmDeltaItem {
  const LlmDeltaTextAppend({required this.blockId, required this.text});

  final int blockId;
  final LlmText text;

  @override
  JsonMap toJson() => taggedJson('kind', 'TextAppend', <String, Object?>{
        'block_id': blockId,
        'text': text.toJson(),
      });

  @override
  String toString() => 'LlmDeltaItem.TextAppend(blockId: $blockId, text: $text)';
}

/// 块定稿。`block == null`（常态，省流量）= 按本地拼接结果定稿；
/// 非 null = 被控端已知本块发生过丢弃，携带终态完整块**覆盖修正**。
final class LlmDeltaBlockEnd extends LlmDeltaItem {
  const LlmDeltaBlockEnd({required this.blockId, this.block});

  final int blockId;
  final LlmBlock? block;

  @override
  JsonMap toJson() {
    final JsonMap body = <String, Object?>{'block_id': blockId};
    writeOpt(body, 'block', block?.toJson());
    return taggedJson('kind', 'BlockEnd', body);
  }

  @override
  String toString() => 'LlmDeltaItem.BlockEnd(blockId: $blockId, block: $block)';
}

/// 声明「本块有 `bytes` 字节的中间增量被丢弃了」——让降级**可观测**。
final class LlmDeltaDropped extends LlmDeltaItem {
  const LlmDeltaDropped({required this.blockId, required this.bytes});

  final int blockId;
  final int bytes;

  @override
  JsonMap toJson() => taggedJson('kind', 'Dropped', <String, Object?>{
        'block_id': blockId,
        'bytes': bytes,
      });

  @override
  String toString() => 'LlmDeltaItem.Dropped(blockId: $blockId, bytes: $bytes)';
}

/// 未知增量项（反序列化兜底）。
final class LlmDeltaUnknown extends LlmDeltaItem {
  const LlmDeltaUnknown();

  @override
  JsonMap toJson() => taggedJson('kind', 'Unknown');

  @override
  String toString() => 'LlmDeltaItem.Unknown';
}

// ── 权限审批决定 ─────────────────────────────────────────────────────────────

/// 权限审批决定。
sealed class LlmPermissionDecision {
  const LlmPermissionDecision();

  JsonMap toJson();

  static LlmPermissionDecision fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'kind',
        label: 'LlmPermissionDecision',
        cases: <String, LlmPermissionDecision Function(JsonMap)>{
          'Allow': (JsonMap m) =>
              LlmPermissionAllow(remember: readBoolOr(m, 'remember', fallback: false)),
          'Deny': (JsonMap m) => LlmPermissionDeny(
                reason: m['reason'] == null ? null : LlmText(readString(m, 'reason')),
              ),
        },
        fallback: const LlmPermissionDecisionUnknown(),
      );
}

/// 允许。`remember == true` 表示本对话内后续同名工具自动放行。
final class LlmPermissionAllow extends LlmPermissionDecision {
  const LlmPermissionAllow({this.remember = false});

  final bool remember;

  @override
  JsonMap toJson() =>
      taggedJson('kind', 'Allow', <String, Object?>{'remember': remember});

  @override
  String toString() => 'LlmPermissionDecision.Allow(remember: $remember)';
}

/// 拒绝。
final class LlmPermissionDeny extends LlmPermissionDecision {
  const LlmPermissionDeny({this.reason});

  final LlmText? reason;

  @override
  JsonMap toJson() {
    final JsonMap body = <String, Object?>{};
    writeOpt(body, 'reason', reason?.toJson());
    return taggedJson('kind', 'Deny', body);
  }

  @override
  String toString() => 'LlmPermissionDecision.Deny(reason: $reason)';
}

/// 未知决定（反序列化兜底）。
///
/// **被控端收到必须按拒绝处理**——「认不出的审批结果」在安全上只能保守解释。
final class LlmPermissionDecisionUnknown extends LlmPermissionDecision {
  const LlmPermissionDecisionUnknown();

  @override
  JsonMap toJson() => taggedJson('kind', 'Unknown');

  @override
  String toString() => 'LlmPermissionDecision.Unknown';
}
