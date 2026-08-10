/// LLM 子协议的 **10 个开放式 C 型枚举**（线上是裸字符串）。
///
/// 权威定义是 `crates/lumen-protocol/src/llm.rs` 的 `open_enum!` 宏展开。
///
/// ## 为什么 Dart 侧要写 10 遍而 Rust 侧只写一个宏
///
/// Rust 侧 10 个枚举是**同一个宏**生成的十份同样的代码，语料覆盖 2 个等于覆盖 10 个；
/// Dart 侧每一个都是手写的，**语料覆盖了几个就只证明了几个**。所以 golden 语料要求
/// 每个开放枚举都至少有一条 `Other` 用例，风险最高的是 [llmErrorCode]（新错误码是最先
/// 长出来的东西，而那正是「出事时唯一告诉用户出了什么事」的那一格）与 [llmExitReason]。
///
/// ## 已知值只声明**实测到**的
///
/// 好几个枚举（[llmTerminalReason] / [llmRateLimitStatus] / [llmRateLimitWindow] /
/// [llmOverageStatus] / [llmOverageDisabledReason]）刻意只有一两个变体——那是本机实际
/// 采样到的全部取值。臆造变体会让映射表长期骗人：一个本端其实没见过的值被当成认识的，
/// UI 从此走错分支且没有任何报错。不认识的一律走 `Other` 保留原文。
library;

import 'package:lumen_mobile/protocol/codec.dart';

/// 后端 LLM CLI 种类。
enum LlmAgentKindKnown implements WireNamed {
  claude('Claude'),
  codex('Codex'),
  gemini('Gemini'),
  kimi('Kimi');

  const LlmAgentKindKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmAgentKindKnown> llmAgentKind =
    OpenEnumCodec<LlmAgentKindKnown>(LlmAgentKindKnown.values, 'LlmAgentKind');

/// 权限模式（归一化）。各 CLI 真正支持的子集见 `LlmAgentInfo.permissionModes`。
enum LlmPermissionModeKnown implements WireNamed {
  manual('Manual'),
  acceptEdits('AcceptEdits'),
  auto('Auto'),
  bypass('Bypass'),
  dontAsk('DontAsk'),
  plan('Plan');

  const LlmPermissionModeKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmPermissionModeKnown> llmPermissionMode =
    OpenEnumCodec<LlmPermissionModeKnown>(
  LlmPermissionModeKnown.values,
  'LlmPermissionMode',
);

/// 一轮结束的原因（**展示用**）。
///
/// **判成败不看它**——实测存在「`stop` 看起来正常、`is_error` 为真」的组合，
/// 权威判据是 `LlmTurnOutcome.isError`。
enum LlmStopReasonKnown implements WireNamed {
  endTurn('EndTurn'),
  maxTokens('MaxTokens'),
  toolUse('ToolUse'),
  interrupted('Interrupted'),
  refusal('Refusal'),
  error('Error');

  const LlmStopReasonKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmStopReasonKnown> llmStopReason =
    OpenEnumCodec<LlmStopReasonKnown>(LlmStopReasonKnown.values, 'LlmStopReason');

/// 一轮的**终止通道**。上游是 `snake_case`，本枚举线上是 `PascalCase`，
/// 被控端负责映射；这一侧只按线格式收。
enum LlmTerminalReasonKnown implements WireNamed {
  /// 正常跑完。**不是「成功」的判据**，只说明这一轮从正常通道收的场。
  completed('Completed'),

  /// 上游 API 报错终止（实测同帧带 HTTP 403）。
  apiError('ApiError');

  const LlmTerminalReasonKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmTerminalReasonKnown> llmTerminalReason =
    OpenEnumCodec<LlmTerminalReasonKnown>(
  LlmTerminalReasonKnown.values,
  'LlmTerminalReason',
);

/// 限流窗口的当前判定。
///
/// 只声明实测到的 `Allowed`；未知值**不得默认当成放行**，应按「状态不明」展示。
enum LlmRateLimitStatusKnown implements WireNamed {
  allowed('Allowed');

  const LlmRateLimitStatusKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmRateLimitStatusKnown> llmRateLimitStatus =
    OpenEnumCodec<LlmRateLimitStatusKnown>(
  LlmRateLimitStatusKnown.values,
  'LlmRateLimitStatus',
);

/// 限流窗口的周期。
enum LlmRateLimitWindowKnown implements WireNamed {
  fiveHour('FiveHour');

  const LlmRateLimitWindowKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmRateLimitWindowKnown> llmRateLimitWindow =
    OpenEnumCodec<LlmRateLimitWindowKnown>(
  LlmRateLimitWindowKnown.values,
  'LlmRateLimitWindow',
);

/// 超额计费状态。
enum LlmOverageStatusKnown implements WireNamed {
  rejected('Rejected');

  const LlmOverageStatusKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmOverageStatusKnown> llmOverageStatus =
    OpenEnumCodec<LlmOverageStatusKnown>(
  LlmOverageStatusKnown.values,
  'LlmOverageStatus',
);

/// 超额计费被禁用的原因。手机端唯一能给出可操作提示（「去充值」）的字段。
enum LlmOverageDisabledReasonKnown implements WireNamed {
  outOfCredits('OutOfCredits');

  const LlmOverageDisabledReasonKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmOverageDisabledReasonKnown> llmOverageDisabledReason =
    OpenEnumCodec<LlmOverageDisabledReasonKnown>(
  LlmOverageDisabledReasonKnown.values,
  'LlmOverageDisabledReason',
);

/// 对话（子进程）退出原因。
enum LlmExitReasonKnown implements WireNamed {
  closed('Closed'),

  /// CLI 子进程自行退出。**别只看退出码**：实测认证失败时退出码 1 且 stderr 为空，
  /// 真正的原因只在事件流里。
  processExited('ProcessExited'),
  spawnFailed('SpawnFailed'),
  protocolError('ProtocolError'),
  idle('Idle'),
  hostShutdown('HostShutdown');

  const LlmExitReasonKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmExitReasonKnown> llmExitReason =
    OpenEnumCodec<LlmExitReasonKnown>(LlmExitReasonKnown.values, 'LlmExitReason');

/// 机器可读错误码。控制端按此本地化提示，**不靠字符串匹配**。
///
/// 映射不到已知码时落 `Other(原文)`，**不要**硬塞成 [LlmErrorCodeKnown.io]——
/// 那会让手机端提示词彻底失真。
enum LlmErrorCodeKnown implements WireNamed {
  agentNotFound('AgentNotFound'),
  cwdInvalid('CwdInvalid'),
  authRequired('AuthRequired'),
  rateLimited('RateLimited'),
  overloaded('Overloaded'),
  contextOverflow('ContextOverflow'),
  cancelled('Cancelled'),
  staleSession('StaleSession'),
  limitReached('LimitReached'),
  io('Io'),
  protocol('Protocol');

  const LlmErrorCodeKnown(this.wire);

  @override
  final String wire;
}

const OpenEnumCodec<LlmErrorCodeKnown> llmErrorCode =
    OpenEnumCodec<LlmErrorCodeKnown>(LlmErrorCodeKnown.values, 'LlmErrorCode');
