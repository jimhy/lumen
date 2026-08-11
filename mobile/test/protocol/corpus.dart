/// golden 语料的读取与比较工具——三个 golden 测试文件共用。
///
/// 抽出来是因为片 5 之后语料分成了四类（内层帧 / 控制面 / REST / 枚举），
/// 三个测试文件各读一类。复制三份 `deepJsonEquals` 的下场是可预见的：
/// 有人只放松了其中一份，而放松了的那份**照样全绿**。
library;

import 'dart:convert';
import 'dart:io';

import 'package:lumen_mobile/protocol/codec.dart';
import 'package:path/path.dart' as p;

/// 语料目录（相对 `mobile/` 包根）。**不许在 mobile/ 下复制一份**——复制品会漂移。
const String corpusRelative = '../crates/lumen-protocol/tests/golden/mobile';

/// 一条语料。
final class GoldenCase {
  GoldenCase(this.name, this.env);

  final String name;
  final JsonMap env;

  String get note => env['note']! as String;

  String get expectKind => (env['expect'] as String?) ?? 'ok';

  String? get expectVariant => env['expect_variant'] as String?;

  /// 内层 `LlmFrame` 原文（只有帧语料有）。
  Object? get frame => env['frame'];

  /// 控制面上行原文。
  Object? get c2s => env['c2s'];

  /// 控制面下行原文。
  Object? get s2c => env['s2c'];

  /// REST DTO 原文。
  Object? get rest => env['rest'];

  /// [rest] 的类型名。
  String? get restType => env['rest_type'] as String?;

  /// 控制面 C 型枚举的取值全集。
  Map<String, List<String>> get enums {
    final Object? raw = env['enums'];
    if (raw is! JsonMap) return const <String, List<String>>{};
    return raw.map(
      (String k, Object? v) => MapEntry<String, List<String>>(
        k,
        (v! as List<Object?>).cast<String>(),
      ),
    );
  }

  /// 期望的重新序列化结果。缺省 = 与 [frame] 逐值相等。
  Object? get expected =>
      env.containsKey('reserialized') ? env['reserialized'] : env['frame'];

  List<String> get debugForbidden =>
      (env['debug_forbidden'] as List<Object?>? ?? const <Object?>[])
          .cast<String>();

  List<Object?> get envelopes =>
      env['envelopes'] as List<Object?>? ?? const <Object?>[];

  bool get isFrame => env.containsKey('frame');
}

Directory corpusDir() =>
    Directory(p.normalize(p.join(Directory.current.path, corpusRelative)));

/// 读全部语料，按文件名升序（失败信息才可复现）。
List<GoldenCase> loadCorpus() {
  final List<File> files = corpusDir()
      .listSync()
      .whereType<File>()
      .where((File f) => p.extension(f.path) == '.json')
      .toList()
    ..sort((File a, File b) => a.path.compareTo(b.path));
  return files
      .map((File f) => GoldenCase(
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

/// 打印前按键名排序，让失败 diff 与 Rust 侧的输出对得上（那边是 BTreeMap）。
String pretty(Object? json) =>
    const JsonEncoder.withIndent('  ').convert(_sortKeys(json));

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
