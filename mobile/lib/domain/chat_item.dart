/// 对话列表里的一个**定稿条目**——**零 Flutter 依赖**。
///
/// 归约器（`state/conversation_controller.dart`）把协议帧规约成这些条目，UI 只认它们。
/// 中间这一层存在的理由是：协议层的 `LlmBlock` 是**线格式**的形状，而列表要的是
/// 「一个稳定 key + 一个渲染形态 + 一个修订号」。直接把 `LlmBlock` 喂给 ListView，
/// 每次协议加一个字段都会波及渲染层。
///
/// ## ★ key 必须是 `(turn, blockId)`，不能只用 blockId
///
/// `blockId` 是块在**本轮内**的序号，跨轮从 0 重新开始；而且同一轮里 user 与 assistant
/// **共用同一个编号空间**。只拿 blockId 当 key，助手的首个流式块会**静默覆盖用户气泡**
/// ——用户看到自己刚发的话变成了模型的回复，而没有任何报错。
///
/// ## revision 是给 memo 缓存用的
///
/// Markdown 解析很贵，条目一旦定稿就不该再解析第二次。渲染层按 `(key, revision)` 做 memo；
/// 只有**块被覆盖修正**（`BlockEnd` 带终态块）时 revision 才自增，缓存随之失效一次。
library;

import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

/// 条目属于谁。
enum ChatRole {
  /// 用户发出的（`TurnStarted.user` 里的块）。
  user,

  /// 模型产出的（`Delta` / `TurnSnapshot.assistant` 里的块）。
  assistant,
}

/// 列表条目。
sealed class ChatItem {
  const ChatItem({
    required this.turn,
    required this.blockId,
    required this.role,
    this.revision = 0,
    this.parentCallId,
  });

  /// 轮号。与 [blockId] 合起来才是稳定 key，见库文档。
  final int turn;

  /// 块在本轮内的序号。
  final int blockId;

  final ChatRole role;

  /// 修订号：块被终态覆盖时自增，memo 缓存据此失效。
  final int revision;

  /// 嵌套归属：本块属于哪一次工具调用产生的子代理输出。
  ///
  /// null = 主对话直接产出。**不要用空串表达「无归属」**——那会让「归属于一个 id 为空串的
  /// 调用」和「没有归属」在下游变成同一件事。
  final String? parentCallId;

  /// 稳定 key。ListView 的 `ValueKey` 与 memo 缓存都用它。
  String get key => '$turn:$blockId';
}

/// 文本块（Markdown 原文，渲染由 UI 负责）。
final class ChatText extends ChatItem {
  const ChatText({
    required super.turn,
    required super.blockId,
    required super.role,
    required this.markdown,
    super.revision,
    super.parentCallId,
  });

  final String markdown;

  ChatText bumped(String next) => ChatText(
        turn: turn,
        blockId: blockId,
        role: role,
        markdown: next,
        revision: revision + 1,
        parentCallId: parentCallId,
      );

  @override
  String toString() => 'ChatText($key, ${markdown.length} 字符, r$revision)';
}

/// 一次工具调用。
///
/// 入参**刻意不在这里展开**：`LlmToolInput` 是脱敏包装，它的 `value` 是 `Object?`
/// （任意 JSON），展开成字符串就等于把可能含密钥的原文摊进列表内存。片 9 的卡片按需渲染。
final class ChatToolCall extends ChatItem {
  const ChatToolCall({
    required super.turn,
    required super.blockId,
    required super.role,
    required this.callId,
    required this.name,
    required this.input,
    this.title,
    this.truncatedBytes,
    super.revision,
    super.parentCallId,
  });

  /// 与 [ChatToolResult.callId] 配对。
  final String callId;

  /// 工具名（`Read` / `Bash` / …）。片 9 按它选卡片形态，未知名一律走通用兜底。
  final String name;

  /// PC 侧归一化出来的一行人话摘要。null 时 UI 自己按 [name] 兜底。
  final LlmText? title;

  final LlmToolInput input;

  /// 入参被夹紧掉的字节数（null = 没夹）。
  final int? truncatedBytes;

  @override
  String toString() => 'ChatToolCall($key, $name, call=$callId)';
}

/// 一次工具结果。
///
/// ## ★ 摘要走 [detail]，不走 [output]
///
/// 实测一次 `Read` 的 `tool_result.content` 是 **12272 字符**（一个 165 行的 README），
/// 而 `detail` 只有 4 个小字段就够画出「读取 README.md 第 1–165 行（共 165 行）」。
/// 这不是优化是必须：手机走中继 + 可能是蜂窝网，一轮里 Read 十个文件就是 120 KB，
/// 其中 99% 用户根本不会看（卡片默认折叠）——**折叠只是 UI 折叠，字节照样过了网、进了内存**。
///
/// `detail == null`（非 File 类工具，形态未实测）时才退回按 [output] 渲染。
final class ChatToolResult extends ChatItem {
  const ChatToolResult({
    required super.turn,
    required super.blockId,
    required super.role,
    required this.callId,
    required this.status,
    required this.output,
    this.detail,
    this.truncatedBytes,
    super.revision,
    super.parentCallId,
  });

  final String callId;
  final LlmToolStatus status;

  /// 正文（PC 侧已按上限夹紧）。**只在 [detail] 为 null 时才用它渲染。**
  final LlmText output;

  /// 结构化摘要。
  final LlmToolResultDetail? detail;

  /// 正文被夹掉的字节数（null = 没夹）。有值时 UI 要提示「已截断，点开拉全文」。
  final int? truncatedBytes;

  @override
  String toString() => 'ChatToolResult($key, ${status.wire}, call=$callId)';
}

/// 图片附件。
final class ChatImage extends ChatItem {
  const ChatImage({
    required super.turn,
    required super.blockId,
    required super.role,
    required this.attachment,
    super.revision,
    super.parentCallId,
  });

  final LlmAttachment attachment;

  @override
  String toString() => 'ChatImage($key, ${attachment.mime})';
}

/// 错误块。
///
/// ⚠ 实测**认证失败是以一条普通 assistant 消息 + 普通 text 块**投递的，不是错误块。
/// 所以「看到 ChatError 才算出错」是错的——真正的成败判据在 `LlmTurnOutcome.isError`。
final class ChatError extends ChatItem {
  const ChatError({
    required super.turn,
    required super.blockId,
    required super.role,
    required this.code,
    required this.message,
    super.revision,
    super.parentCallId,
  });

  final OpenEnum<LlmErrorCodeKnown> code;
  final LlmText message;

  @override
  String toString() => 'ChatError($key, ${code.wire})';
}

/// **内容缺口**：这一块有 [bytes] 字节的中间增量被丢弃了。
///
/// 让降级可见（§14-6 无声降级禁令）：UI 画成「内容有缺口，点击补齐」，点击发 `TurnFetch`。
/// 悄悄跳过它的后果是用户读到一段**看起来完整但少了中间几句**的回复。
final class ChatGap extends ChatItem {
  const ChatGap({
    required super.turn,
    required super.blockId,
    required super.role,
    required this.bytes,
    super.revision,
    super.parentCallId,
  });

  final int bytes;

  @override
  String toString() => 'ChatGap($key, $bytes 字节)';
}

/// 本端不认识的块。
///
/// 画一行「本版本不支持的内容」而不是**跳过**：跳过等于让用户读到一段少了东西的回复
/// 却毫不知情。前向兼容的代价要看得见。
final class ChatUnknown extends ChatItem {
  const ChatUnknown({
    required super.turn,
    required super.blockId,
    required super.role,
    super.revision,
    super.parentCallId,
  });

  @override
  String toString() => 'ChatUnknown($key)';
}
