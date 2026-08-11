/// golden 语料的**可达性与信封形状**守卫（片 0b 版，不涉及任何 Dart 协议模型）。
///
/// 为什么片 0b 就要有这个测试：`crates/lumen-protocol/tests/golden/mobile/README.md` 明写
/// 「读**那个目录**，不要复制一份到 `mobile/`——复制品会漂移」。这句话在片 1-Dart 落地之前
/// 完全没有机器保障：路径写错、语料被挪走、`flutter test` 的工作目录不是包根，三种情况都
/// 只会在片 1 写到一半时才炸，而那时人会以为是自己的协议模型有问题。
///
/// 所以这里先把**路径**和**信封字段**钉死。它不断言任何协议语义（那是片 1 的
/// `golden_test.dart` 的事），只回答两个问题：
/// 1. 从 `mobile/` 出发，语料还在不在原地？
/// 2. 每个文件是不是仍然是 README §1 描述的那个信封形状？
///
/// ⚠ 数量下界写成 55 而不是当前实际值：语料只增不减是常态，写死实际值会让每次加语料都要
/// 回来改这里；写下界既能挡住「目录被清空/挪走」，又不制造无谓维护。上界不设。
library;

import 'dart:convert';
import 'dart:io';

import 'package:collection/collection.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:path/path.dart' as p;

/// 语料目录（相对包根）。`flutter test` 的工作目录**就是** `mobile/`，这条相对路径成立；
/// 若哪天不成立，下面第一条测试会直接报出实际的 `Directory.current`，不用猜。
const String _corpusRelative = '../crates/lumen-protocol/tests/golden/mobile';

/// 语料条数下界，见文件头注释。
const int _minCorpusFiles = 55;

Directory _corpusDir() =>
    Directory(p.normalize(p.join(Directory.current.path, _corpusRelative)));

List<File> _corpusFiles() {
  final List<File> files = _corpusDir()
      .listSync()
      .whereType<File>()
      .where((f) => p.extension(f.path) == '.json')
      .toList();
  files.sort((a, b) => a.path.compareTo(b.path));
  return files;
}

void main() {
  group('golden 语料可达性', () {
    test('语料目录存在（不许在 mobile/ 下复制一份）', () {
      final Directory dir = _corpusDir();
      expect(
        dir.existsSync(),
        isTrue,
        reason: '找不到语料目录 ${dir.path}。'
            '当前工作目录是 ${Directory.current.path}——'
            '`flutter test` 必须在 mobile/ 包根下跑（用 tool/test.ps1）。',
      );
    });

    test('契约说明书 README.md 与语料同在', () {
      // 这份 README 是 Dart 实现者的必读件；它要是不在了，说明有人搬了目录。
      final File readme = File(p.join(_corpusDir().path, 'README.md'));
      expect(readme.existsSync(), isTrue, reason: '缺 ${readme.path}');
    });

    test('语料条数不少于 $_minCorpusFiles 条', () {
      expect(_corpusFiles().length, greaterThanOrEqualTo(_minCorpusFiles));
    });
  });

  group('语料信封形状（README §1）', () {
    test('每个文件都是合法 JSON 对象且带非空 note', () {
      for (final File f in _corpusFiles()) {
        final Object? raw = jsonDecode(f.readAsStringSync());
        expect(raw, isA<Map<String, dynamic>>(), reason: '${f.path} 顶层不是对象');

        final Map<String, dynamic> env = raw! as Map<String, dynamic>;
        final Object? note = env['note'];
        expect(note, isA<String>(), reason: '${f.path} 缺 note');
        expect((note! as String).trim(), isNotEmpty, reason: '${f.path} note 为空');
      }
    });

    test('expect 只有 ok / decode_error 两种取值', () {
      for (final File f in _corpusFiles()) {
        final Map<String, dynamic> env =
            jsonDecode(f.readAsStringSync()) as Map<String, dynamic>;
        // 缺省是 "ok"（README §1）。
        final String expectKind = (env['expect'] as String?) ?? 'ok';
        expect(
          expectKind,
          anyOf('ok', 'decode_error'),
          reason: '${f.path} 的 expect 是未知取值 "$expectKind"',
        );
      }
    });

    test('带变体的语料都申报了 expect_variant', () {
      // 这个申报值是覆盖矩阵的唯一载体：Dart 没有 Rust 的穷尽 match，
      // 「Dart 少实现一个变体就红」全靠把这些申报值收成集合去对账。
      // REST 与枚举语料没有「变体」这回事，故不要求（片 5）。
      for (final File f in _corpusFiles()) {
        final Map<String, dynamic> env =
            jsonDecode(f.readAsStringSync()) as Map<String, dynamic>;
        if (((env['expect'] as String?) ?? 'ok') != 'ok') continue;
        final bool hasVariant = env.containsKey('frame') ||
            env.containsKey('c2s') ||
            env.containsKey('s2c');
        if (!hasVariant) continue;

        final Object? variant = env['expect_variant'];
        expect(variant, isA<String>(), reason: '${f.path} 缺 expect_variant');
        expect((variant! as String).trim(), isNotEmpty, reason: '${f.path} expect_variant 为空');
      }
    });

    test('五个载荷键恰好有一个，且与文件名前缀相符', () {
      // 片 5 起语料分四类。一个文件混放两种载荷，会让「遍历某一类」变成要靠读注释才知道
      // 对不对的事——而两端各遍历一次，漏判在哪一端都只是静默少测几条。
      // Rust 侧 `语料自身的硬规矩` 有同款断言，两边一起守。
      const Map<String, String> prefixToKey = <String, String>{
        'frame_': 'frame',
        'compat_': 'frame',
        'edge_': 'frame',
        'redact_': 'frame',
        'envelope_': 'frame',
        'c2s_': 'c2s',
        's2c_': 's2c',
        'rest_': 'rest',
        'enums_': 'enums',
      };
      for (final File f in _corpusFiles()) {
        final String name = p.basename(f.path);
        final Map<String, dynamic> env =
            jsonDecode(f.readAsStringSync()) as Map<String, dynamic>;
        final List<String> present = <String>[
          for (final String k in <String>['frame', 'c2s', 's2c', 'rest', 'enums'])
            if (env.containsKey(k)) k,
        ];
        final String? expected = prefixToKey.entries
            .where((MapEntry<String, String> e) => name.startsWith(e.key))
            .map((MapEntry<String, String> e) => e.value)
            .firstOrNull;
        expect(expected, isNotNull, reason: '$name 没有已知的分类前缀');
        expect(present, <String>[expected!], reason: '$name 的载荷与前缀不符');
      }
    });
  });
}
