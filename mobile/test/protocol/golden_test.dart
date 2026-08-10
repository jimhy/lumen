/// **片 1-Dart 的验收测试**：读 Rust 侧那**同一批** golden 语料，断言 Dart 模型与
/// 线格式一致。
///
/// 语料目录：`../crates/lumen-protocol/tests/golden/mobile/`（不复制进 `mobile/`，
/// 复制品会漂移；路径本身由 `golden_corpus_path_test.dart` 钉住）。
/// 契约说明书是那个目录里的 `README.md`，本文件逐条实现它的 §2 与 §3。
///
/// ## 往返断言只测了一半
///
/// `encode∘decode` 的不动点检查对**同类型字段互换完全免疫**：把 `known_seq` 解到字段 B、
/// `known_turn` 解到字段 A，只要 `encode` 按同样的键名写回去，全部语料照绿。相邻同型
/// 字段一大把（`conv_id`/`conv_generation`、`seq`/`turn`、`start_line`/`line_count`）。
/// 所以另有一组**取值断言**（见文件末尾）把它们一一钉死——语料已经刻意给这些字段配了
/// 互不相同的值，就是为了让这类断言写得出来。
library;

import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/llm_enums.dart';
import 'package:lumen_mobile/protocol/llm_frame.dart';
import 'package:lumen_mobile/protocol/llm_model.dart';
import 'package:lumen_mobile/protocol/remote_frame.dart';
import 'package:path/path.dart' as p;

const String _corpusRelative = '../crates/lumen-protocol/tests/golden/mobile';

/// 一条语料。
final class _Case {
  _Case(this.name, this.env);

  final String name;
  final JsonMap env;

  String get note => env['note']! as String;
  String get expectKind => (env['expect'] as String?) ?? 'ok';
  String? get expectVariant => env['expect_variant'] as String?;
  Object? get frame => env['frame'];

  /// 期望的重新序列化结果。缺省 = 与 `frame` 逐值相等。
  Object? get expected => env.containsKey('reserialized') ? env['reserialized'] : env['frame'];

  List<String> get debugForbidden =>
      (env['debug_forbidden'] as List<Object?>? ?? const <Object?>[])
          .cast<String>();

  List<Object?> get envelopes =>
      (env['envelopes'] as List<Object?>? ?? const <Object?>[]);
}

List<_Case> _loadCorpus() {
  final Directory dir =
      Directory(p.normalize(p.join(Directory.current.path, _corpusRelative)));
  final List<File> files = dir
      .listSync()
      .whereType<File>()
      .where((File f) => p.extension(f.path) == '.json')
      .toList()
    ..sort((File a, File b) => a.path.compareTo(b.path));
  return files
      .map((File f) => _Case(
            p.basename(f.path),
            jsonDecode(f.readAsStringSync()) as JsonMap,
          ))
      .toList();
}

/// 深度相等——**叶子连类型一起比**。
///
/// Dart 的 `num` 相等是跨 `int`/`double` 的（`240262 == 240262.0` 为真），不比类型
/// 就等于放过「把整数存成 double」这类漂移：那样几乎全部语料都会绿灯通过，只有
/// `edge_int_boundary.json` 里 `9223372036854775807` 那一个会因 double 表示不了而露馅。
///
/// 也**不能**用 `jsonEncode(a) == jsonEncode(b)` —— 那是在比键序，而键序不算差异
/// （Rust 侧输出恒按键名排序，Dart 的 Map 保插入序）。
bool deepJsonEquals(Object? a, Object? b) {
  if (a is Map<String, Object?> && b is Map<String, Object?>) {
    if (a.length != b.length) return false;
    for (final String k in a.keys) {
      if (!b.containsKey(k)) return false;
      if (!deepJsonEquals(a[k], b[k])) return false;
    }
    return true;
  }
  if (a is List<Object?> && b is List<Object?>) {
    if (a.length != b.length) return false;
    for (int i = 0; i < a.length; i++) {
      if (!deepJsonEquals(a[i], b[i])) return false;
    }
    return true;
  }
  // 一边是容器一边不是 ⇒ 直接不等（别落到下面的标量比较上）。
  if (a is Map<Object?, Object?> ||
      b is Map<Object?, Object?> ||
      a is List<Object?> ||
      b is List<Object?>) {
    return false;
  }
  if (a is num && b is num) {
    if ((a is int) != (b is int)) return false;
    return a == b;
  }
  return a == b;
}

String _pretty(Object? json) =>
    const JsonEncoder.withIndent('  ').convert(_sortKeys(json));

/// 打印前按键名排序，让失败 diff 与 Rust 侧的输出对得上（那边是 BTreeMap）。
Object? _sortKeys(Object? json) {
  if (json is Map<String, Object?>) {
    final List<String> keys = json.keys.toList()..sort();
    return <String, Object?>{
      for (final String k in keys) k: _sortKeys(json[k]),
    };
  }
  if (json is List<Object?>) return json.map(_sortKeys).toList();
  return json;
}

void main() {
  final List<_Case> corpus = _loadCorpus();

  group('语料 ⇄ Dart 模型（契约 §3 第 1–4 条）', () {
    for (final _Case c in corpus) {
      test('${c.name}：${c.note.split('。').first}', () {
        if (c.expectKind == 'decode_error') {
          // C 型枚举的**形状**变了时整帧报废，LlmUnknown 也救不回来。
          // 宽容成 Other("42") 反而与 Rust 不一致。
          expect(
            () => LlmFrame.fromJson(c.frame),
            throwsA(isA<WireFormatException>()),
            reason: '${c.name} 期望解码失败，但它解出来了',
          );
          return;
        }

        final LlmFrame decoded = LlmFrame.fromJson(c.frame);

        expect(
          decoded.variantName,
          c.expectVariant,
          reason: '${c.name} 解出的变体与语料申报的不符',
        );

        final JsonMap actual = decoded.toJson();
        expect(
          deepJsonEquals(actual, c.expected),
          isTrue,
          reason: '${c.name} 往返后与期望不等。\n'
              '--- 实际 ---\n${_pretty(actual)}\n'
              '--- 期望 ---\n${_pretty(c.expected)}',
        );

        // 不动点：挡的是手写错的 reserialized。没有这一条，一个瞎写的期望值会让
        // 整条往返断言变成自说自话。
        final JsonMap refeed = LlmFrame.fromJson(c.expected).toJson();
        expect(
          deepJsonEquals(refeed, c.expected),
          isTrue,
          reason: '${c.name} 的 reserialized 不是不动点。\n'
              '--- 再编码 ---\n${_pretty(refeed)}\n'
              '--- 期望 ---\n${_pretty(c.expected)}',
        );

        // D(期望) 与 D(frame) 必须是同一个值。两者都是模型，比它们的编码即可
        // （编码是确定性的，相等的编码等价于相等的模型）。
        expect(
          deepJsonEquals(refeed, actual),
          isTrue,
          reason: '${c.name} 从 frame 与从 reserialized 解出的模型不一致',
        );

        for (final String secret in c.debugForbidden) {
          expect(
            decoded.toString().contains(secret),
            isFalse,
            reason: '${c.name} 的 toString() 泄漏了 "$secret"。'
                '调试渲染会进审计日志与崩溃栈，敏感内容必须被包装类型挡住。\n'
                '实际渲染：${decoded.toString()}',
          );
        }
      });
    }
  });

  group('覆盖矩阵（契约 §3 第 5 条）', () {
    test('语料申报的每个变体，Dart 侧都实现了', () {
      final Set<String> declared = <String>{
        for (final _Case c in corpus)
          if (c.expectKind == 'ok') c.expectVariant!,
      };
      final Set<String> missing =
          declared.difference(LlmFrame.implementedVariants);
      expect(
        missing,
        isEmpty,
        reason: 'Dart 侧缺这些 LlmFrame 变体：$missing。'
            '语料先行是硬顺序——先加语料让两侧同时红，再各自补实现。',
      );
    });

    test('Dart 侧实现的每个变体，语料都覆盖到了', () {
      final Set<String> declared = <String>{
        for (final _Case c in corpus)
          if (c.expectKind == 'ok') c.expectVariant!,
      };
      final Set<String> uncovered =
          LlmFrame.implementedVariants.difference(declared);
      expect(
        uncovered,
        isEmpty,
        reason: '这些变体有实现却没有语料：$uncovered。'
            '没有语料 = 这条降级路径没人测过。',
      );
    });

    test('十个开放式枚举各有一条 Other 语料', () {
      // Rust 侧十个枚举是同一个宏生成的，覆盖 2 个等于覆盖 10 个；
      // Dart 侧是手写十遍，**语料覆盖了几个就只证明了几个**。
      final List<OpenEnumCodec<WireNamed>> codecs = <OpenEnumCodec<WireNamed>>[
        llmAgentKind,
        llmPermissionMode,
        llmStopReason,
        llmTerminalReason,
        llmRateLimitStatus,
        llmRateLimitWindow,
        llmOverageStatus,
        llmOverageDisabledReason,
        llmExitReason,
        llmErrorCode,
      ];
      expect(codecs.length, 10, reason: '开放枚举数目变了，先回去读契约 §4');
      for (final OpenEnumCodec<WireNamed> codec in codecs) {
        final OpenEnum<WireNamed> other =
            codec.fromWireClamped('__lumen_unknown_value__');
        expect(other.isKnown, isFalse, reason: '${codec.label} 未知值应落 Other');
        expect(other.wire, '__lumen_unknown_value__',
            reason: '${codec.label} 的 Other 必须保留原文');
      }
    });
  });

  group('外层信封（契约 §3 第 6 条）', () {
    test('envelopes 数组里每一项都能解成 RemoteFrame 并原样往返', () {
      final List<Object?> samples = <Object?>[
        for (final _Case c in corpus) ...c.envelopes,
      ];
      expect(samples, isNotEmpty, reason: '一条 envelopes 样本都没有，语料被改过？');
      for (final Object? sample in samples) {
        final RemoteFrame frame = RemoteFrame.fromJson(sample);
        expect(
          deepJsonEquals(frame.toJson(), sample),
          isTrue,
          reason: '外层信封往返不等。\n'
              '--- 实际 ---\n${_pretty(frame.toJson())}\n'
              '--- 期望 ---\n${_pretty(sample)}',
        );
      }
    });

    test('unit 变体是裸字符串，不是对象', () {
      // 假设「载荷一定是 object」会在心跳上直接炸。
      expect(RemoteFrame.fromJson('Ping'), isA<RemoteUnit>());
      expect(const RemoteUnit(RemoteUnitKind.pong).toJson(), 'Pong');
      expect(
        () => RemoteFrame.fromJson(<String, Object?>{'P2pStreamHello': <String, Object?>{}}),
        throwsA(isA<WireFormatException>()),
      );
    });

    test('外部标签枚举没有 other 兜底：未知变体必须报错', () {
      // 前向兼容的代价只在内层 LlmFrame 付；这一层宽容反而与 Rust 不一致。
      expect(
        () => RemoteFrame.fromJson(<String, Object?>{'FutureVariant': 1}),
        throwsA(isA<WireFormatException>()),
      );
    });
  });

  group('字段取值断言（契约 §3 第 7 条：往返测不出来的那一半）', () {
    _Case caseNamed(String name) =>
        corpus.firstWhere((_Case c) => c.name == name);

    test('frame_attach：三个同型字段没有接反', () {
      final LlmAttach f =
          LlmFrame.fromJson(caseNamed('frame_attach.json').frame) as LlmAttach;
      expect(f.convId, 11);
      expect(f.knownGeneration, 3);
      expect(f.knownSeq, 102);
      expect(f.knownTurn, 4);
    });

    test('frame_delta：四个数刻意各不相同，逐个钉死', () {
      final LlmDelta f =
          LlmFrame.fromJson(caseNamed('frame_delta.json').frame) as LlmDelta;
      expect(f.convId, 11);
      expect(f.convGeneration, 3);
      expect(f.seq, 102);
      expect(f.turn, 4);

      final LlmDeltaTextAppend second = f.items[1] as LlmDeltaTextAppend;
      expect(second.blockId, 2);
      expect(second.text.value, 'render/glyph.rs');

      // BlockEnd 不带 block 是常态（省流量，按本地拼接结果定稿）。
      expect((f.items[4] as LlmDeltaBlockEnd).block, isNull);
      expect((f.items[5] as LlmDeltaBlockEnd).block, isA<LlmBlockText>());

      final LlmDeltaDropped dropped = f.items[6] as LlmDeltaDropped;
      expect(dropped.blockId, 2);
      expect(dropped.bytes, 4096);
    });

    test('frame_turn_snapshot：轮号 / 用量 / 文件摘要三组同型字段', () {
      final LlmTurnSnapshot f = LlmFrame.fromJson(
        caseNamed('frame_turn_snapshot.json').frame,
      ) as LlmTurnSnapshot;

      expect(f.record.turn, 4);
      expect(f.record.usage.inputTokens, 3);
      expect(f.record.usage.outputTokens, 24);
      expect(f.record.usage.contextUsed, 38342);
      expect(f.record.usage.contextLimit, 1000000);
      expect(f.record.blocksOmitted, 3);
      expect(f.record.complete, isFalse);

      final LlmBlockToolResult result =
          f.record.assistant[3].block as LlmBlockToolResult;
      final LlmToolResultDetailFile detail =
          result.detail! as LlmToolResultDetailFile;
      expect(detail.startLine, 1);
      expect(detail.lineCount, 165);
      expect(detail.totalLines, 165);

      // 「只发元数据、正文一字节不推」的范式：output 空串 + truncated_bytes 有值。
      expect(result.output.value, isEmpty);
      expect(result.truncatedBytes, 12272);

      // outcome 是权威成败判定，不看 stop。
      expect(f.record.outcome.isError, isTrue);
      expect(f.record.outcome.terminalReason?.known,
          LlmTerminalReasonKnown.apiError);
      expect(f.record.outcome.apiErrorStatus, 403);
    });

    test('edge_int_boundary：i64 上界不退化成 double', () {
      final LlmTurnEnded f = LlmFrame.fromJson(
        caseNamed('edge_int_boundary.json').frame,
      ) as LlmTurnEnded;
      expect(f.convId, 9223372036854775807);
      expect(f.turn, 4294967295);
      expect(f.endedMs, -1);
      expect(f.usage.costMicroUsd, 9007199254740991);
    });
  });

  group('断言器自身的守卫', () {
    // 这一组守的是 deepJsonEquals 本身。它要是写松了，上面 65 条语料会集体变成
    // 「自说自话的绿灯」——而那种绿灯比红灯危险得多，因为没人会再去看它。
    test('整数与浮点算差异（Dart 的 num 相等跨 int/double）', () {
      expect(240262 == 240262.0, isTrue, reason: 'Dart 语言本身就是这样，所以才需要下面这条');
      expect(deepJsonEquals(240262, 240262.0), isFalse);
      expect(
        deepJsonEquals(<String, Object?>{'seq': 1}, <String, Object?>{'seq': 1.0}),
        isFalse,
      );
    });

    test('键序不算差异', () {
      expect(
        deepJsonEquals(
          <String, Object?>{'a': 1, 'b': 2},
          <String, Object?>{'b': 2, 'a': 1},
        ),
        isTrue,
      );
    });

    test('多一个键、少一个键、数组换序都算差异', () {
      expect(
        deepJsonEquals(<String, Object?>{'a': 1}, <String, Object?>{'a': 1, 'b': 2}),
        isFalse,
      );
      expect(
        deepJsonEquals(<String, Object?>{'a': 1, 'b': 2}, <String, Object?>{'a': 1}),
        isFalse,
      );
      expect(deepJsonEquals(<Object?>[1, 2], <Object?>[2, 1]), isFalse);
      // null 与键缺席在**输出侧**不等价（输入侧才等价）。
      expect(
        deepJsonEquals(<String, Object?>{'model': null}, <String, Object?>{}),
        isFalse,
      );
    });

    test('容器与标量不混比', () {
      expect(deepJsonEquals(<Object?>[], 0), isFalse);
      expect(deepJsonEquals(<String, Object?>{}, ''), isFalse);
    });
  });

  group('Dart 专属的坑（契约 §3 表）', () {
    test('String 按 UTF-16 计长，凡是夹紧都必须先 utf8.encode', () {
      const String cjk = '中文';
      expect(cjk.length, 2, reason: 'UTF-16 码元数');
      expect(utf8Length(cjk), 6, reason: 'UTF-8 字节数，差 3 倍');
      expect(const LlmText('中文').lengthInBytes, 6);
    });

    test('夹紧落在字符边界上，不切出半个字符', () {
      final String s = '${'A' * 63}中尾';
      // 裸切第 64 字节会切进「中」的中间。
      expect(clampUtf8(s, 64), 'A' * 63);
      expect(utf8Length(clampUtf8(s, 70)), lessThanOrEqualTo(70));
    });

    test('开放枚举的 Other 只打前 16 字节加总长，不打全文', () {
      final OpenEnum<LlmStopReasonKnown> other =
          llmStopReason.fromWireClamped('unknown_reason__SECRET-TAIL');
      expect(other.toString().contains('SECRET-TAIL'), isFalse);
      expect(other.toString().contains('unknown_reason__'), isTrue);
      // 原文本身必须完整保留，脱敏只发生在调试渲染上。
      expect(other.wire, 'unknown_reason__SECRET-TAIL');
    });

    test('本端认识的值收敛成 Known，不留两个等价表示', () {
      expect(llmStopReason.fromWireClamped('EndTurn').isKnown, isTrue);
      expect(llmStopReason.fromWireClamped('EndTurn').known,
          LlmStopReasonKnown.endTurn);
    });
  });
}
