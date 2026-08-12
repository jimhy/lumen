/// LLM 子协议帧（**内部标签**，标签字段名固定为 `"op"`）。
///
/// 权威定义是 `crates/lumen-protocol/src/llm.rs` 的 `LlmFrame`。这一层装进外层
/// `RemoteFrame::Llm` 信封经服务端盲转，见 `remote_frame.dart`——**两层机制不同**
/// （外层 externally tagged、内层 internally tagged），别用同一个解析器。
///
/// ## 未知 `op` 必须降级，不是报错
///
/// [LlmUnknown] 是本子协议前向兼容的根：收到它意味着对端有本端不支持的新能力，
/// 应记一条 debug 后忽略，而不是让整帧解析失败。未知 body **整个丢弃**，不保留不回显。
///
/// ## 方向
///
/// `控→被` = 控制端（手机）→ 被控端（PC）；`被→控` 反之。
library;

import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';

/// LLM 对话子协议帧。
sealed class LlmFrame {
  const LlmFrame();

  /// 变体名，与 golden 语料的 `expect_variant` 对账。
  ///
  /// Dart 没有 Rust 的穷尽 `match`，「少实现一个变体就红」全靠这个申报值被收成集合
  /// 去和语料对齐——所以它必须与 Rust 变体名**逐字相同**，也就是线上的 `op` 值。
  String get variantName;

  JsonMap toJson();

  static LlmFrame fromJson(Object? json) => decodeTagged(
        json,
        tagKey: 'op',
        label: 'LlmFrame',
        cases: _cases,
        fallback: const LlmUnknown(),
      );

  static final Map<String, LlmFrame Function(JsonMap)> _cases =
      <String, LlmFrame Function(JsonMap)>{
    'Hello': (JsonMap m) => LlmHello(
          llmProto: readInt(m, 'llm_proto'),
          caps: readListOr(m, 'caps', asWireString),
        ),
    'HelloAck': (JsonMap m) => LlmHelloAck(
          llmProto: readInt(m, 'llm_proto'),
          caps: readListOr(m, 'caps', asWireString),
          agents: readListOr(m, 'agents', LlmAgentInfo.fromJson),
          convs: readListOr(m, 'convs', LlmConvMeta.fromJson),
          rateLimit: readObjectOpt(m, 'rate_limit', LlmRateLimit.fromJson),
          rateLimitObservedMs: readIntOpt(m, 'rate_limit_observed_ms'),
        ),
    'ListAgents': (JsonMap m) => LlmListAgents(reqId: readInt(m, 'req_id')),
    'AgentList': (JsonMap m) => LlmAgentList(
          reqId: readInt(m, 'req_id'),
          agents: readList(m, 'agents', LlmAgentInfo.fromJson),
        ),
    'ListConvs': (JsonMap m) => LlmListConvs(reqId: readInt(m, 'req_id')),
    'ConvList': (JsonMap m) => LlmConvList(
          reqId: readInt(m, 'req_id'),
          convs: readList(m, 'convs', LlmConvMeta.fromJson),
        ),
    'Start': (JsonMap m) => LlmStart(
          reqId: readInt(m, 'req_id'),
          agent: llmAgentKind.read(m, 'agent'),
          cwd: LlmPath(readString(m, 'cwd')),
          model: readStringOpt(m, 'model'),
          permissionMode: llmPermissionMode.readOpt(m, 'permission_mode'),
          resume: readStringOpt(m, 'resume'),
          forkSession: readBoolOr(m, 'fork_session', fallback: false),
          appendSystemPrompt: m['append_system_prompt'] == null
              ? null
              : LlmText(readString(m, 'append_system_prompt')),
          allowedTools: readListOpt(m, 'allowed_tools', asWireString),
          origin: readObjectOpt(m, 'origin', PaneRef.fromJson),
        ),
    'ConvStarted': (JsonMap m) => LlmConvStarted(
          reqId: readInt(m, 'req_id'),
          meta: LlmConvMeta.fromJson(m['meta']),
          attachDir: LlmPath(readString(m, 'attach_dir')),
          seq: readInt(m, 'seq'),
        ),
    'ConvFailed': (JsonMap m) => LlmConvFailed(
          reqId: readInt(m, 'req_id'),
          code: llmErrorCode.read(m, 'code'),
          message: LlmText(readString(m, 'message')),
        ),
    'Send': (JsonMap m) => LlmSend(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          reqId: readInt(m, 'req_id'),
          text: LlmText(readString(m, 'text')),
          attachments: readListOr(m, 'attachments', LlmAttachment.fromJson),
          clientMsgId: readStringOpt(m, 'client_msg_id'),
        ),
    'Interrupt': (JsonMap m) => LlmInterrupt(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          reqId: readInt(m, 'req_id'),
        ),
    'Close': (JsonMap m) => LlmClose(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          reqId: readInt(m, 'req_id'),
        ),
    'ConvExited': (JsonMap m) => LlmConvExited(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          seq: readInt(m, 'seq'),
          reason: llmExitReason.read(m, 'reason'),
          code: readIntOpt(m, 'code'),
          message: m['message'] == null ? null : LlmText(readString(m, 'message')),
        ),
    'Attach': (JsonMap m) => LlmAttach(
          convId: readInt(m, 'conv_id'),
          knownGeneration: readIntOr(m, 'known_generation', 0),
          knownSeq: readIntOr(m, 'known_seq', 0),
          knownTurn: readIntOr(m, 'known_turn', 0),
        ),
    'Attached': (JsonMap m) => LlmAttached(
          meta: LlmConvMeta.fromJson(m['meta']),
          seq: readInt(m, 'seq'),
          resyncRequired: readBoolOr(m, 'resync_required', fallback: false),
        ),
    'Detach': (JsonMap m) => LlmDetach(
          convId: readInt(m, 'conv_id'),
          convGeneration: readIntOr(m, 'conv_generation', 0),
        ),
    'Delta': (JsonMap m) => LlmDelta(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          seq: readInt(m, 'seq'),
          turn: readInt(m, 'turn'),
          items: readList(m, 'items', LlmDeltaItem.fromJson),
        ),
    'DeltaAck': (JsonMap m) => LlmDeltaAck(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          seq: readInt(m, 'seq'),
        ),
    'TurnStarted': (JsonMap m) => LlmTurnStarted(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          seq: readInt(m, 'seq'),
          turn: readInt(m, 'turn'),
          user: readList(m, 'user', LlmBlockEntry.fromJson),
          startedMs: readInt(m, 'started_ms'),
          clientMsgId: readStringOpt(m, 'client_msg_id'),
        ),
    'TurnEnded': (JsonMap m) => LlmTurnEnded(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          seq: readInt(m, 'seq'),
          turn: readInt(m, 'turn'),
          stop: llmStopReason.read(m, 'stop'),
          outcome: readObjectOr(
            m,
            'outcome',
            LlmTurnOutcome.fromJson,
            LlmTurnOutcome.new,
          ),
          usage: LlmUsage.fromJson(m['usage']),
          endedMs: readInt(m, 'ended_ms'),
          truncated: readBoolOr(m, 'truncated', fallback: false),
        ),
    'RateLimit': (JsonMap m) => LlmRateLimitFrame(
          convId: readInt(m, 'conv_id'),
          convGeneration: readIntOr(m, 'conv_generation', 0),
          observedMs: readInt(m, 'observed_ms'),
          info: LlmRateLimit.fromJson(m['info']),
        ),
    'TurnFetch': (JsonMap m) => LlmTurnFetch(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          reqId: readInt(m, 'req_id'),
          turn: readInt(m, 'turn'),
        ),
    'TurnSnapshot': (JsonMap m) => LlmTurnSnapshot(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          reqId: readInt(m, 'req_id'),
          seq: readInt(m, 'seq'),
          record: LlmTurnRecord.fromJson(m['record']),
        ),
    'HistoryReq': (JsonMap m) => LlmHistoryReq(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          reqId: readInt(m, 'req_id'),
          beforeTurn: readInt(m, 'before_turn'),
          maxTurns: readInt(m, 'max_turns'),
        ),
    'HistoryPage': (JsonMap m) => LlmHistoryPage(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          reqId: readInt(m, 'req_id'),
          oldestTurn: readInt(m, 'oldest_turn'),
          turns: readList(m, 'turns', LlmTurnRecord.fromJson),
          hasMore: readBool(m, 'has_more'),
        ),
    'PermissionRequest': (JsonMap m) => LlmPermissionRequest(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          requestId: readInt(m, 'request_id'),
          turn: readInt(m, 'turn'),
          tool: readString(m, 'tool'),
          input: LlmToolInput(m['input']),
          preview: LlmText(readString(m, 'preview')),
          expiresInSecs: readInt(m, 'expires_in_secs'),
        ),
    'PermissionReply': (JsonMap m) => LlmPermissionReply(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          requestId: readInt(m, 'request_id'),
          decision: LlmPermissionDecision.fromJson(m['decision']),
        ),
    'PermissionResolved': (JsonMap m) => LlmPermissionResolved(
          convId: readInt(m, 'conv_id'),
          convGeneration: readInt(m, 'conv_generation'),
          requestId: readInt(m, 'request_id'),
          decision: LlmPermissionDecision.fromJson(m['decision']),
          by: LlmPermissionResolvedBy.fromJson(m['by']),
        ),
    'Error': (JsonMap m) => LlmError(
          convId: readIntOpt(m, 'conv_id'),
          reqId: readIntOpt(m, 'req_id'),
          code: llmErrorCode.read(m, 'code'),
          message: LlmText(readString(m, 'message')),
        ),
  };

  /// 本端实现的全部变体名（含 [LlmUnknown]），供覆盖矩阵断言对账。
  static Set<String> get implementedVariants =>
      <String>{..._cases.keys, 'Unknown'};
}

// ── 能力协商 ─────────────────────────────────────────────────────────────────

/// 控→被：会话建立后立即发。
///
/// 老端不认识 `RemoteFrame::Llm` 会**整帧丢弃且不回任何东西**，所以超时判据必须是
/// 「没收到 [LlmHelloAck]」，不能等任何错误回执。握手状态还必须随会话重建复位
/// （重挂 / 直连↔中继翻转），否则一次偶发超时会让 LLM 面在整个会话生命周期内假死。
final class LlmHello extends LlmFrame {
  const LlmHello({required this.llmProto, this.caps = const <String>[]});

  final int llmProto;

  /// 可选子能力标签（如 `"perm"` / `"attach"`），供细粒度灰度——比单调版本号表达力强。
  final List<String> caps;

  @override
  String get variantName => 'Hello';

  @override
  JsonMap toJson() =>
      <String, Object?>{'op': 'Hello', 'llm_proto': llmProto, 'caps': caps};

  @override
  String toString() => 'LlmFrame.Hello(llmProto: $llmProto, caps: $caps)';
}

/// 被→控：能力应答，顺带回带 agent 清单与现存对话清单（省两个往返）。
final class LlmHelloAck extends LlmFrame {
  const LlmHelloAck({
    required this.llmProto,
    this.caps = const <String>[],
    this.agents = const <LlmAgentInfo>[],
    this.convs = const <LlmConvMeta>[],
    this.rateLimit,
    this.rateLimitObservedMs,
  });

  final int llmProto;
  final List<String> caps;
  final List<LlmAgentInfo> agents;
  final List<LlmConvMeta> convs;

  /// **额度状态的基线**：没有它，从挂载到下一次限流事件之间（可能几十分钟）额度条
  /// 只能是空的。放在握手帧而不是每个对话的元信息里，因为额度是**账号级**的。
  final LlmRateLimit? rateLimit;

  /// 上面那份基线的**观测时刻**（Unix 毫秒），与 [LlmRateLimitFrame.observedMs]
  /// **同一时钟域**（都是被控端 PC 的墙钟）。
  ///
  /// 没有它，latest-wins 在基线这条通路上就执行不了：一条在会话重建期间迟到的**旧**
  /// 播报会无条件覆盖**更新**的基线。控制端不得给基线打手机本地的 `now()` 来自救——
  /// 跨设备比时钟是引入一个新 bug。
  final int? rateLimitObservedMs;

  @override
  String get variantName => 'HelloAck';

  @override
  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'op': 'HelloAck',
      'llm_proto': llmProto,
      'caps': caps,
      'agents': agents.map((LlmAgentInfo a) => a.toJson()).toList(),
      'convs': convs.map((LlmConvMeta c) => c.toJson()).toList(),
    };
    writeOpt(out, 'rate_limit', rateLimit?.toJson());
    writeOpt(out, 'rate_limit_observed_ms', rateLimitObservedMs);
    return out;
  }

  @override
  String toString() => 'LlmFrame.HelloAck(llmProto: $llmProto, caps: $caps, '
      'agents: $agents, convs: $convs, rateLimit: $rateLimit, '
      'rateLimitObservedMs: $rateLimitObservedMs)';
}

// ── 发现 ─────────────────────────────────────────────────────────────────────

/// 控→被：列出可用 agent。
final class LlmListAgents extends LlmFrame {
  const LlmListAgents({required this.reqId});

  final int reqId;

  @override
  String get variantName => 'ListAgents';

  @override
  JsonMap toJson() => <String, Object?>{'op': 'ListAgents', 'req_id': reqId};

  @override
  String toString() => 'LlmFrame.ListAgents(reqId: $reqId)';
}

/// 被→控：agent 清单。
final class LlmAgentList extends LlmFrame {
  const LlmAgentList({required this.reqId, required this.agents});

  final int reqId;
  final List<LlmAgentInfo> agents;

  @override
  String get variantName => 'AgentList';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'AgentList',
        'req_id': reqId,
        'agents': agents.map((LlmAgentInfo a) => a.toJson()).toList(),
      };

  @override
  String toString() => 'LlmFrame.AgentList(reqId: $reqId, agents: $agents)';
}

/// 控→被：列出现存对话。
final class LlmListConvs extends LlmFrame {
  const LlmListConvs({required this.reqId});

  final int reqId;

  @override
  String get variantName => 'ListConvs';

  @override
  JsonMap toJson() => <String, Object?>{'op': 'ListConvs', 'req_id': reqId};

  @override
  String toString() => 'LlmFrame.ListConvs(reqId: $reqId)';
}

/// 被→控：对话清单。
final class LlmConvList extends LlmFrame {
  const LlmConvList({required this.reqId, required this.convs});

  final int reqId;
  final List<LlmConvMeta> convs;

  @override
  String get variantName => 'ConvList';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'ConvList',
        'req_id': reqId,
        'convs': convs.map((LlmConvMeta c) => c.toJson()).toList(),
      };

  @override
  String toString() => 'LlmFrame.ConvList(reqId: $reqId, convs: $convs)';
}

// ── 生命周期 ─────────────────────────────────────────────────────────────────

/// 控→被：按 cwd 启动一个 headless 对话。
final class LlmStart extends LlmFrame {
  const LlmStart({
    required this.reqId,
    required this.agent,
    required this.cwd,
    this.model,
    this.permissionMode,
    this.resume,
    this.forkSession = false,
    this.appendSystemPrompt,
    this.allowedTools,
    this.origin,
  });

  final int reqId;
  final OpenEnum<LlmAgentKindKnown> agent;

  /// 工作目录（被控端校验存在性与越权）。
  final LlmPath cwd;

  final String? model;
  final OpenEnum<LlmPermissionModeKnown>? permissionMode;

  /// 续接已有 CLI 会话。**`--resume` 语义未实测**，实现前需采样确认。
  final String? resume;

  /// 与 [resume] 配合，映射 `--fork-session`（不覆盖原会话）。
  /// **带默认值但不带 skip**——输出里一定出现。
  final bool forkSession;

  final LlmText? appendSystemPrompt;
  final List<String>? allowedTools;
  final PaneRef? origin;

  @override
  String get variantName => 'Start';

  @override
  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'op': 'Start',
      'req_id': reqId,
      'agent': agent.toJson(),
      'cwd': cwd.toJson(),
    };
    writeOpt(out, 'model', model);
    writeOpt(out, 'permission_mode', permissionMode?.toJson());
    writeOpt(out, 'resume', resume);
    out['fork_session'] = forkSession;
    writeOpt(out, 'append_system_prompt', appendSystemPrompt?.toJson());
    writeOpt(out, 'allowed_tools', allowedTools);
    writeOpt(out, 'origin', origin?.toJson());
    return out;
  }

  @override
  String toString() => 'LlmFrame.Start(reqId: $reqId, agent: $agent, cwd: $cwd, '
      'model: $model, permissionMode: $permissionMode, resume: $resume, '
      'forkSession: $forkSession, appendSystemPrompt: $appendSystemPrompt, '
      'allowedTools: $allowedTools, origin: $origin)';
}

/// 被→控：**基线帧**。会话已启动。
final class LlmConvStarted extends LlmFrame {
  const LlmConvStarted({
    required this.reqId,
    required this.meta,
    required this.attachDir,
    required this.seq,
  });

  final int reqId;
  final LlmConvMeta meta;

  /// 本对话的附件暂存目录：控制端用 `PutBegin` 往这里传图，再在 [LlmSend] 里引用路径。
  final LlmPath attachDir;

  /// 本对话增量水位起点。
  final int seq;

  @override
  String get variantName => 'ConvStarted';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'ConvStarted',
        'req_id': reqId,
        'meta': meta.toJson(),
        'attach_dir': attachDir.toJson(),
        'seq': seq,
      };

  @override
  String toString() => 'LlmFrame.ConvStarted(reqId: $reqId, meta: $meta, '
      'attachDir: $attachDir, seq: $seq)';
}

/// 被→控：启动失败。**与 [LlmConvStarted] 互斥**——用互斥变体从类型上排除歧义，
/// 而不是并列一个 `Option<err>` 靠注释约定优先级。
final class LlmConvFailed extends LlmFrame {
  const LlmConvFailed({
    required this.reqId,
    required this.code,
    required this.message,
  });

  final int reqId;
  final OpenEnum<LlmErrorCodeKnown> code;
  final LlmText message;

  @override
  String get variantName => 'ConvFailed';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'ConvFailed',
        'req_id': reqId,
        'code': code.toJson(),
        'message': message.toJson(),
      };

  @override
  String toString() =>
      'LlmFrame.ConvFailed(reqId: $reqId, code: $code, message: $message)';
}

/// 控→被：发一条用户消息。附件字节**不在本帧内**。
final class LlmSend extends LlmFrame {
  const LlmSend({
    required this.convId,
    required this.convGeneration,
    required this.reqId,
    required this.text,
    this.attachments = const <LlmAttachment>[],
    this.clientMsgId,
  });

  final int convId;
  final int convGeneration;
  final int reqId;
  final LlmText text;
  final List<LlmAttachment> attachments;

  /// 本条消息的**幂等键**（outbox 行的 id）。被控端按它去重，并随 [LlmTurnStarted] 回带。
  ///
  /// ★ **不能拿 [reqId] 代替**：`req_id` 是每个控制端各自分配的，而被控端的对话表全端共用
  /// （一台 PC 上可能同时挂着桌面镜像控制端与另一部手机），两个空间完全重叠。
  ///
  /// ⚠ 它**故意不脱敏**（裸 `String`、原样进 `toString()` 与日志），所以**绝不能**往里塞
  /// 消息正文之类的用户内容——那等于把内容写进对端的日志文件。
  final String? clientMsgId;

  @override
  String get variantName => 'Send';

  @override
  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'op': 'Send',
      'conv_id': convId,
      'conv_generation': convGeneration,
      'req_id': reqId,
      'text': text.toJson(),
      'attachments': attachments.map((LlmAttachment a) => a.toJson()).toList(),
    };
    writeOpt(out, 'client_msg_id', clientMsgId);
    return out;
  }

  @override
  String toString() => 'LlmFrame.Send(convId: $convId, '
      'convGeneration: $convGeneration, reqId: $reqId, text: $text, '
      'attachments: $attachments, clientMsgId: $clientMsgId)';
}

/// 控→被：中断进行中的一轮。
final class LlmInterrupt extends LlmFrame {
  const LlmInterrupt({
    required this.convId,
    required this.convGeneration,
    required this.reqId,
  });

  final int convId;
  final int convGeneration;
  final int reqId;

  @override
  String get variantName => 'Interrupt';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'Interrupt',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'req_id': reqId,
      };

  @override
  String toString() => 'LlmFrame.Interrupt(convId: $convId, '
      'convGeneration: $convGeneration, reqId: $reqId)';
}

/// 控→被：结束对话并回收子进程。
final class LlmClose extends LlmFrame {
  const LlmClose({
    required this.convId,
    required this.convGeneration,
    required this.reqId,
  });

  final int convId;
  final int convGeneration;
  final int reqId;

  @override
  String get variantName => 'Close';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'Close',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'req_id': reqId,
      };

  @override
  String toString() => 'LlmFrame.Close(convId: $convId, '
      'convGeneration: $convGeneration, reqId: $reqId)';
}

/// 被→控：对话（子进程）已退出。终态。
final class LlmConvExited extends LlmFrame {
  const LlmConvExited({
    required this.convId,
    required this.convGeneration,
    required this.seq,
    required this.reason,
    this.code,
    this.message,
  });

  final int convId;
  final int convGeneration;
  final int seq;
  final OpenEnum<LlmExitReasonKnown> reason;

  /// 子进程退出码。**别只看它**：实测认证失败时退出码 1 且 stderr 为空。
  final int? code;

  final LlmText? message;

  @override
  String get variantName => 'ConvExited';

  @override
  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'op': 'ConvExited',
      'conv_id': convId,
      'conv_generation': convGeneration,
      'seq': seq,
      'reason': reason.toJson(),
    };
    writeOpt(out, 'code', code);
    writeOpt(out, 'message', message?.toJson());
    return out;
  }

  @override
  String toString() => 'LlmFrame.ConvExited(convId: $convId, '
      'convGeneration: $convGeneration, seq: $seq, reason: $reason, '
      'code: $code, message: $message)';
}

// ── 订阅与重连补齐 ────────────────────────────────────────────────────────────

/// 控→被：订阅一个对话的实时流，并声明本端已知进度用于增量补齐。
///
/// 首次订阅填 `knownGeneration = 0`（哨兵：一定不匹配 → 被控端回全量基线）。
final class LlmAttach extends LlmFrame {
  const LlmAttach({
    required this.convId,
    this.knownGeneration = 0,
    this.knownSeq = 0,
    this.knownTurn = 0,
  });

  final int convId;
  final int knownGeneration;
  final int knownSeq;
  final int knownTurn;

  @override
  String get variantName => 'Attach';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'Attach',
        'conv_id': convId,
        'known_generation': knownGeneration,
        'known_seq': knownSeq,
        'known_turn': knownTurn,
      };

  @override
  String toString() => 'LlmFrame.Attach(convId: $convId, '
      'knownGeneration: $knownGeneration, knownSeq: $knownSeq, '
      'knownTurn: $knownTurn)';
}

/// 被→控：**基线帧，必先于该对话任何 [LlmDelta] 到达**。
final class LlmAttached extends LlmFrame {
  const LlmAttached({
    required this.meta,
    required this.seq,
    this.resyncRequired = false,
  });

  final LlmConvMeta meta;

  /// 发出本基线那一刻的水位：控制端**丢弃 `seq` 不大于本值的迟到 Delta**，
  /// 由此把直连与中继两条通路的切换乱序自动收敛。
  final int seq;

  /// `true` = 代不匹配或缺口过大，控制端应清空本地对话状态后重建。
  final bool resyncRequired;

  @override
  String get variantName => 'Attached';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'Attached',
        'meta': meta.toJson(),
        'seq': seq,
        'resync_required': resyncRequired,
      };

  @override
  String toString() => 'LlmFrame.Attached(meta: $meta, seq: $seq, '
      'resyncRequired: $resyncRequired)';
}

/// 控→被：取消订阅（被控端据此停发增量但**不结束**对话）。
///
/// [convGeneration] 带默认值 `0` = 老控制端没带、按「不校验」兼容处理。
/// **为什么这条也要带代号**：不带的话「代不匹配一律丢弃」这条不变量在类型上就执行不了，
/// 一条翻转期间迟到的旧代 Detach 会把**新一代**的订阅退掉。
final class LlmDetach extends LlmFrame {
  const LlmDetach({required this.convId, this.convGeneration = 0});

  final int convId;
  final int convGeneration;

  @override
  String get variantName => 'Detach';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'Detach',
        'conv_id': convId,
        'conv_generation': convGeneration,
      };

  @override
  String toString() =>
      'LlmFrame.Detach(convId: $convId, convGeneration: $convGeneration)';
}

// ── 流式增量 ─────────────────────────────────────────────────────────────────

/// 被→控：一批增量。
///
/// [seq] 在同一 `(convId, convGeneration)` 内**严格递增且连续**；控制端发现断裂
/// 即发 [LlmTurnFetch] 拉整轮快照覆盖重建。
final class LlmDelta extends LlmFrame {
  const LlmDelta({
    required this.convId,
    required this.convGeneration,
    required this.seq,
    required this.turn,
    required this.items,
  });

  final int convId;
  final int convGeneration;
  final int seq;
  final int turn;
  final List<LlmDeltaItem> items;

  @override
  String get variantName => 'Delta';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'Delta',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'seq': seq,
        'turn': turn,
        'items': items.map((LlmDeltaItem i) => i.toJson()).toList(),
      };

  @override
  String toString() => 'LlmFrame.Delta(convId: $convId, '
      'convGeneration: $convGeneration, seq: $seq, turn: $turn, items: $items)';
}

/// 控→被：确认已消费到 `seq`，维持滑动窗口背压。
final class LlmDeltaAck extends LlmFrame {
  const LlmDeltaAck({
    required this.convId,
    required this.convGeneration,
    required this.seq,
  });

  final int convId;
  final int convGeneration;
  final int seq;

  @override
  String get variantName => 'DeltaAck';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'DeltaAck',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'seq': seq,
      };

  @override
  String toString() => 'LlmFrame.DeltaAck(convId: $convId, '
      'convGeneration: $convGeneration, seq: $seq)';
}

/// 被→控：新一轮开始（回显归一化后的用户块）。
final class LlmTurnStarted extends LlmFrame {
  const LlmTurnStarted({
    required this.convId,
    required this.convGeneration,
    required this.seq,
    required this.turn,
    required this.user,
    required this.startedMs,
    this.clientMsgId,
  });

  final int convId;
  final int convGeneration;
  final int seq;
  final int turn;
  final List<LlmBlockEntry> user;
  final int startedMs;

  /// 触发本轮的那条 [LlmSend] 的幂等键，被控端原样回带。
  ///
  /// 这是本端**唯一**能确知「我那条消息已被接收」的信号 —— outbox 据此销账、停止重发。
  ///
  /// **null 不等于失败**：本轮由别的控制端发起、由 PC 本地发起、或对端是不带本字段的老被控端
  /// 时它都缺席。此时 outbox 只能退回「用户手动重试」，绝不能把它当成「发送失败」自动重发。
  final String? clientMsgId;

  @override
  String get variantName => 'TurnStarted';

  @override
  JsonMap toJson() {
    final JsonMap out = <String, Object?>{
      'op': 'TurnStarted',
      'conv_id': convId,
      'conv_generation': convGeneration,
      'seq': seq,
      'turn': turn,
      'user': user.map((LlmBlockEntry e) => e.toJson()).toList(),
      'started_ms': startedMs,
    };
    writeOpt(out, 'client_msg_id', clientMsgId);
    return out;
  }

  @override
  String toString() => 'LlmFrame.TurnStarted(convId: $convId, '
      'convGeneration: $convGeneration, seq: $seq, turn: $turn, user: $user, '
      'startedMs: $startedMs, clientMsgId: $clientMsgId)';
}

/// 被→控：一轮结束。**成败看 [outcome]，不看 [stop]。**
final class LlmTurnEnded extends LlmFrame {
  const LlmTurnEnded({
    required this.convId,
    required this.convGeneration,
    required this.seq,
    required this.turn,
    required this.stop,
    required this.usage,
    required this.endedMs,
    this.outcome = const LlmTurnOutcome(),
    this.truncated = false,
  });

  final int convId;
  final int convGeneration;
  final int seq;
  final int turn;
  final OpenEnum<LlmStopReasonKnown> stop;
  final LlmTurnOutcome outcome;
  final LlmUsage usage;
  final int endedMs;

  /// `true` = 本轮曾因积压丢过增量，控制端应发 [LlmTurnFetch] 补齐。
  final bool truncated;

  @override
  String get variantName => 'TurnEnded';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'TurnEnded',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'seq': seq,
        'turn': turn,
        'stop': stop.toJson(),
        'outcome': outcome.toJson(),
        'usage': usage.toJson(),
        'ended_ms': endedMs,
        'truncated': truncated,
      };

  @override
  String toString() => 'LlmFrame.TurnEnded(convId: $convId, '
      'convGeneration: $convGeneration, seq: $seq, turn: $turn, stop: $stop, '
      'outcome: $outcome, usage: $usage, endedMs: $endedMs, '
      'truncated: $truncated)';
}

/// 被→控：**账号额度状态**播报。
///
/// 类名带 `Frame` 后缀是为了与载荷类型 [LlmRateLimit] 区分（Rust 侧靠 `LlmFrame::`
/// 前缀天然分开，Dart 的类名在同一命名空间里会撞）。
///
/// **本帧刻意没有 `seq`**：`Delta.seq` 的不变量是「严格递增且**连续**，一断裂就拉快照」，
/// 若本帧也领 seq，每收到一次限流播报就会看见一个缺口并触发一次整轮拉取——
/// 一条纯状态广播把自愈通道变成了刷屏器。代价是它与增量流之间无序，改用
/// [observedMs] 做 **latest-wins**。
///
/// 语义是**账号级**的：[convId] 只表示「这条播报是在哪个对话的流上观测到的」，
/// 控制端应据此维护**一个全局额度条**，而不是每个对话一份。
final class LlmRateLimitFrame extends LlmFrame {
  const LlmRateLimitFrame({
    required this.convId,
    required this.observedMs,
    required this.info,
    this.convGeneration = 0,
  });

  final int convId;
  final int convGeneration;

  /// 被控端**观测到**该事件的时刻（Unix 毫秒），用于 latest-wins 去乱序。
  final int observedMs;

  final LlmRateLimit info;

  @override
  String get variantName => 'RateLimit';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'RateLimit',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'observed_ms': observedMs,
        'info': info.toJson(),
      };

  @override
  String toString() => 'LlmFrame.RateLimit(convId: $convId, '
      'convGeneration: $convGeneration, observedMs: $observedMs, info: $info)';
}

// ── 历史与自愈 ───────────────────────────────────────────────────────────────

/// 控→被：拉单轮定稿快照（seq 断裂 / 被截断 / 直连切换后的自愈通道）。
final class LlmTurnFetch extends LlmFrame {
  const LlmTurnFetch({
    required this.convId,
    required this.convGeneration,
    required this.reqId,
    required this.turn,
  });

  final int convId;
  final int convGeneration;
  final int reqId;
  final int turn;

  @override
  String get variantName => 'TurnFetch';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'TurnFetch',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'req_id': reqId,
        'turn': turn,
      };

  @override
  String toString() => 'LlmFrame.TurnFetch(convId: $convId, '
      'convGeneration: $convGeneration, reqId: $reqId, turn: $turn)';
}

/// 被→控：单轮快照。**基线语义**：控制端以本记录整体覆盖该轮本地状态。
///
/// **`record` 未必是完整的一轮**——控制端必须读 [LlmTurnRecord.blocksOmitted]
/// 并把「有 N 块太大未同步」显示出来。
final class LlmTurnSnapshot extends LlmFrame {
  const LlmTurnSnapshot({
    required this.convId,
    required this.convGeneration,
    required this.reqId,
    required this.seq,
    required this.record,
  });

  final int convId;
  final int convGeneration;
  final int reqId;
  final int seq;
  final LlmTurnRecord record;

  @override
  String get variantName => 'TurnSnapshot';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'TurnSnapshot',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'req_id': reqId,
        'seq': seq,
        'record': record.toJson(),
      };

  @override
  String toString() => 'LlmFrame.TurnSnapshot(convId: $convId, '
      'convGeneration: $convGeneration, reqId: $reqId, seq: $seq, '
      'record: $record)';
}

/// 控→被：向前翻历史。
final class LlmHistoryReq extends LlmFrame {
  const LlmHistoryReq({
    required this.convId,
    required this.convGeneration,
    required this.reqId,
    required this.beforeTurn,
    required this.maxTurns,
  });

  final int convId;
  final int convGeneration;
  final int reqId;

  /// 取严格小于本轮号的历史；填 `curTurn + 1` 表示从最新往回取。
  final int beforeTurn;

  final int maxTurns;

  @override
  String get variantName => 'HistoryReq';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'HistoryReq',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'req_id': reqId,
        'before_turn': beforeTurn,
        'max_turns': maxTurns,
      };

  @override
  String toString() => 'LlmFrame.HistoryReq(convId: $convId, '
      'convGeneration: $convGeneration, reqId: $reqId, '
      'beforeTurn: $beforeTurn, maxTurns: $maxTurns)';
}

/// 被→控：一页历史（`turns` 按轮号**升序**）。
///
/// **`turns.length` 常小于请求的 `maxTurns`**：被控端先按字节预算夹紧再按轮数夹紧。
/// 控制端**必须以 [oldestTurn] 推进游标、以 [hasMore] 判断是否到底**，
/// 绝不能假设「请求 N 轮就回 N 轮」——否则会卡在补不齐的缺口上永久空白。
final class LlmHistoryPage extends LlmFrame {
  const LlmHistoryPage({
    required this.convId,
    required this.convGeneration,
    required this.reqId,
    required this.oldestTurn,
    required this.turns,
    required this.hasMore,
  });

  final int convId;
  final int convGeneration;
  final int reqId;

  /// 本页最老的轮号（即下次 `beforeTurn` 应填的值）。
  final int oldestTurn;

  final List<LlmTurnRecord> turns;
  final bool hasMore;

  @override
  String get variantName => 'HistoryPage';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'HistoryPage',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'req_id': reqId,
        'oldest_turn': oldestTurn,
        'turns': turns.map((LlmTurnRecord t) => t.toJson()).toList(),
        'has_more': hasMore,
      };

  @override
  String toString() => 'LlmFrame.HistoryPage(convId: $convId, '
      'convGeneration: $convGeneration, reqId: $reqId, '
      'oldestTurn: $oldestTurn, turns: $turns, hasMore: $hasMore)';
}

// ── 权限审批 ─────────────────────────────────────────────────────────────────

/// 被→控：请求用户批准一次工具调用。
final class LlmPermissionRequest extends LlmFrame {
  const LlmPermissionRequest({
    required this.convId,
    required this.convGeneration,
    required this.requestId,
    required this.turn,
    required this.tool,
    required this.input,
    required this.preview,
    required this.expiresInSecs,
  });

  final int convId;
  final int convGeneration;

  /// 审批请求号（**被控端**分配，与 `reqId` 不同空间，别拿去和 `reqId` 比）。
  final int requestId;

  final int turn;
  final String tool;
  final LlmToolInput input;

  /// 人类可读的影响预览（如「将写入 3 个文件」）。
  final LlmText preview;

  final int expiresInSecs;

  @override
  String get variantName => 'PermissionRequest';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'PermissionRequest',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'request_id': requestId,
        'turn': turn,
        'tool': tool,
        'input': input.toJson(),
        'preview': preview.toJson(),
        'expires_in_secs': expiresInSecs,
      };

  @override
  String toString() => 'LlmFrame.PermissionRequest(convId: $convId, '
      'convGeneration: $convGeneration, requestId: $requestId, turn: $turn, '
      'tool: $tool, input: $input, preview: $preview, '
      'expiresInSecs: $expiresInSecs)';
}

/// 控→被：用户的审批结果。
final class LlmPermissionReply extends LlmFrame {
  const LlmPermissionReply({
    required this.convId,
    required this.convGeneration,
    required this.requestId,
    required this.decision,
  });

  final int convId;
  final int convGeneration;
  final int requestId;
  final LlmPermissionDecision decision;

  @override
  String get variantName => 'PermissionReply';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'PermissionReply',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'request_id': requestId,
        'decision': decision.toJson(),
      };

  @override
  String toString() => 'LlmFrame.PermissionReply(convId: $convId, '
      'convGeneration: $convGeneration, requestId: $requestId, '
      'decision: $decision)';
}

/// 被→控：该审批已被裁决。
///
/// 控制端据此收起审批 UI，**不依赖自己是否发过 [LlmPermissionReply]**
/// （那条可能在翻转中丢了）。
final class LlmPermissionResolved extends LlmFrame {
  const LlmPermissionResolved({
    required this.convId,
    required this.convGeneration,
    required this.requestId,
    required this.decision,
    required this.by,
  });

  final int convId;
  final int convGeneration;
  final int requestId;
  final LlmPermissionDecision decision;
  final LlmPermissionResolvedBy by;

  @override
  String get variantName => 'PermissionResolved';

  @override
  JsonMap toJson() => <String, Object?>{
        'op': 'PermissionResolved',
        'conv_id': convId,
        'conv_generation': convGeneration,
        'request_id': requestId,
        'decision': decision.toJson(),
        'by': by.toJson(),
      };

  @override
  String toString() => 'LlmFrame.PermissionResolved(convId: $convId, '
      'convGeneration: $convGeneration, requestId: $requestId, '
      'decision: $decision, by: $by)';
}

/// 被→控：与具体对话无关或无法归入上述应答的错误。
final class LlmError extends LlmFrame {
  const LlmError({
    required this.code,
    required this.message,
    this.convId,
    this.reqId,
  });

  final int? convId;
  final int? reqId;
  final OpenEnum<LlmErrorCodeKnown> code;
  final LlmText message;

  @override
  String get variantName => 'Error';

  @override
  JsonMap toJson() {
    final JsonMap out = <String, Object?>{'op': 'Error'};
    writeOpt(out, 'conv_id', convId);
    writeOpt(out, 'req_id', reqId);
    out['code'] = code.toJson();
    out['message'] = message.toJson();
    return out;
  }

  @override
  String toString() => 'LlmFrame.Error(convId: $convId, reqId: $reqId, '
      'code: $code, message: $message)';
}

/// 更高版本对端发来的未知 `op`（**只用于反序列化兜底，永不发送**）。
///
/// 它自己也必须能恒等往返（`{"op":"Unknown"}`）——不能是「一序列化就炸」的半成品。
/// 未知 body 在解码时就被**整个丢弃**，这里没有任何可回显的残留。
final class LlmUnknown extends LlmFrame {
  const LlmUnknown();

  @override
  String get variantName => 'Unknown';

  @override
  JsonMap toJson() => <String, Object?>{'op': 'Unknown'};

  @override
  String toString() => 'LlmFrame.Unknown';
}
