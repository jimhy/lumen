/// **片 5 的协议验收**：控制面（`c2s_*` / `s2c_*` / `enums_*` 语料）与 Dart 模型对齐。
///
/// 与 `golden_test.dart`（内层帧）**口径相反**的三处，正是这个文件单独存在的理由：
///
/// 1. 外部标签枚举**没有** `other` 兜底 ⇒ 未知变体必须**抛异常**，不是降级；
/// 2. S2C 要求**全实现**（手机是接收方），C2S 刻意**只实现 6 个**（发得出 `RequestControl`
///    才是真危险）；
/// 3. 四个 C 型枚举也没有 `Other` ⇒ 每个取值都要实现，靠 `enums_control_plane.json` 对账。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/remote_c2s.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';

import 'corpus.dart';

void main() {
  final List<GoldenCase> corpus = loadCorpus();
  final List<GoldenCase> c2sCases =
      corpus.where((GoldenCase c) => c.c2s != null).toList();
  final List<GoldenCase> s2cCases =
      corpus.where((GoldenCase c) => c.s2c != null).toList();

  GoldenCase caseNamed(String name) =>
      corpus.firstWhere((GoldenCase c) => c.name == name);

  group('c2s 语料 ⇄ RemoteC2S', () {
    test('语料条数与手机实现的变体数一致', () {
      expect(c2sCases, isNotEmpty, reason: '一条 c2s 语料都没读到，路径或前缀改了？');
      expect(
        c2sCases.length,
        RemoteC2S.implementedVariants.length,
        reason: '手机会发的变体每个一条语料，不多不少',
      );
    });

    for (final GoldenCase c in c2sCases) {
      test('${c.name}：${c.note.split('。').first}', () {
        final RemoteC2S decoded = RemoteC2S.fromJson(c.c2s);
        expect(decoded.variantName, c.expectVariant,
            reason: '${c.name} 解出的变体与申报不符');
        final Object? actual = decoded.toJson();
        expect(
          deepJsonEquals(actual, c.c2s),
          isTrue,
          reason: '${c.name} 往返不等。\n'
              '--- 实际 ---\n${pretty(actual)}\n'
              '--- 期望 ---\n${pretty(c.c2s)}',
        );
      });
    }
  });

  group('s2c 语料 ⇄ RemoteS2C', () {
    test('语料覆盖 Rust 侧的**全部** 14 个变体', () {
      final Set<String> declared = <String>{
        for (final GoldenCase c in s2cCases) c.expectVariant!,
      };
      expect(
        declared,
        RemoteS2C.implementedVariants,
        reason: '手机是接收方：收到一条没实现的变体就是**整条消息**解析失败。'
            '哪怕是「理论上收不到」的镜像消息也要实现——那只是对服务端当前行为的假设。',
      );
    });

    for (final GoldenCase c in s2cCases) {
      test('${c.name}：${c.note.split('。').first}', () {
        final RemoteS2C decoded = RemoteS2C.fromJson(c.s2c);
        expect(decoded.variantName, c.expectVariant,
            reason: '${c.name} 解出的变体与申报不符');
        final Object? actual = decoded.toJson();
        expect(
          deepJsonEquals(actual, c.s2c),
          isTrue,
          reason: '${c.name} 往返不等。\n'
              '--- 实际 ---\n${pretty(actual)}\n'
              '--- 期望 ---\n${pretty(c.s2c)}',
        );
      });
    }
  });

  group('外部标签枚举的三条硬规矩', () {
    test('unit 变体是裸字符串，不是对象', () {
      // 心跳走的正是这条：编码成 {"Ping":{}} 会让整条链路静默失效
      // （服务端解析失败后只记 debug 继续读，不会告诉你）。
      expect(const C2SPing().toJson(), 'Ping');
      expect(RemoteC2S.fromJson('Ping'), isA<C2SPing>());
      expect(const S2CPong().toJson(), 'Pong');
      expect(RemoteS2C.fromJson('Pong'), isA<S2CPong>());
      expect(
        () => RemoteS2C.fromJson(<String, Object?>{'Pong': <String, Object?>{}}),
        throwsA(isA<WireFormatException>()),
        reason: 'unit 变体写成对象应当解析失败，而不是被宽容',
      );
    });

    test('未知变体必须抛异常，不许兜底', () {
      expect(
        () => RemoteS2C.fromJson(<String, Object?>{'FutureVariant': <String, Object?>{}}),
        throwsA(isA<WireFormatException>()),
      );
      expect(
        () => RemoteC2S.fromJson(<String, Object?>{'FutureVariant': <String, Object?>{}}),
        throwsA(isA<WireFormatException>()),
      );
    });

    test('手机刻意不实现的 4 个 C2S 变体，解码也要报错', () {
      // 不是「解得出但不发」，是**类型层面不存在**：手机发得出 RequestControl 会占住那台 PC
      // 的镜像独占位，而且老服务端会照常执行成功、两端都察觉不到。
      for (final String variant in <String>[
        'RequestControl',
        'DeclineControl',
        'Relay',
        'EndSession',
      ]) {
        expect(
          () => RemoteC2S.fromJson(<String, Object?>{variant: <String, Object?>{}}),
          throwsA(isA<WireFormatException>()),
          reason: '$variant 是桌面端 / 被控端的消息，手机端不该实现',
        );
        expect(RemoteC2S.implementedVariants.contains(variant), isFalse);
      }
      // EndSession 在线上是 unit 变体（裸字符串），另走一条路径。
      expect(
        () => RemoteC2S.fromJson('EndSession'),
        throwsA(isA<WireFormatException>()),
      );
    });
  });

  group('封闭式 C 型枚举（enums_control_plane.json）', () {
    final GoldenCase enumsCase = caseNamed('enums_control_plane.json');
    final Map<String, List<String>> declared = enumsCase.enums;

    test('语料确实带了四个枚举', () {
      expect(
        declared.keys.toSet(),
        <String>{'DenyReason', 'PairingFailReason', 'EndReason', 'Role'},
      );
    });

    /// 语料取值集合与 Dart 侧 enum 的 `wire` 集合必须**相等**：
    /// 少了 = 有取值没实现（收到就整条报废）；多了 = 实现了 Rust 侧不存在的取值。
    void checkEnum(String name, List<WireNamed> values) {
      test('$name 的取值与语料逐个对齐', () {
        expect(
          values.map((WireNamed v) => v.wire).toSet(),
          declared[name]!.toSet(),
          reason: '$name 没有 Other 兜底：取值对不上就是整条消息报废，'
              '而 ControlDenied 是「发起被拒」的唯一回执',
        );
      });
    }

    checkEnum('DenyReason', DenyReason.values);
    checkEnum('PairingFailReason', PairingFailReason.values);
    checkEnum('EndReason', EndReason.values);
    checkEnum('Role', RemoteRole.values);

    test('未知取值必须报错，不许落 Other', () {
      expect(
        () => RemoteS2C.fromJson(<String, Object?>{
          'ControlDenied': <String, Object?>{
            'target_device_id': 'pc-1',
            'reason': 'FutureReason',
          },
        }),
        throwsA(isA<WireFormatException>()),
      );
    });

    test('DenyReason 与 PairingFailReason 的重名取值不串味', () {
      // 两者都有 Expired / TooManyAttempts，但出现在不同消息里、语义不同：
      // 一个是「这次发起被拒」，一个是「这次输码失败」。共用一套解析就会静默串味。
      final RemoteS2C denied = RemoteS2C.fromJson(<String, Object?>{
        'ControlDenied': <String, Object?>{
          'target_device_id': 'pc-1',
          'reason': 'Expired',
        },
      });
      final RemoteS2C failed = RemoteS2C.fromJson(<String, Object?>{
        'PairingResult': <String, Object?>{
          'reason': 'Expired',
          'attempts_left': 0,
        },
      });
      expect((denied as S2CControlDenied).reason, DenyReason.expired);
      expect((failed as S2CPairingResult).reason, PairingFailReason.expired);
      // 类型不同 ⇒ 混用在编译期就不通过，这正是分开两个 enum 的价值。
      expect(DenyReason.expired.wire, PairingFailReason.expired.wire);
    });
  });

  group('字段取值断言（往返测不出来的那一半）', () {
    test('s2c_welcome：两个版本号刻意不同，别接反', () {
      final S2CWelcome m =
          RemoteS2C.fromJson(caseNamed('s2c_welcome.json').s2c) as S2CWelcome;
      expect(m.protocolVersion, 4);
      expect(m.minSupportedVersion, 3);
      expect(m.deviceId, 'phone-1');
    });

    test('c2s_client_hello：本端常量与语料同源，caps 为空', () {
      final C2SClientHello m = RemoteC2S.fromJson(
        caseNamed('c2s_client_hello.json').c2s,
      ) as C2SClientHello;
      expect(m.protocolVersion, kLumenProtocolVersion,
          reason: '语料里的版本号与 kLumenProtocolVersion 必须是同一个数');
      expect(m.caps, isEmpty, reason: '"hidden" 是 PC 的能力位，手机报了就是谎报');
    });

    test('c2s_submit_pairing：配对码是字符串，前导零不能丢', () {
      final C2SSubmitPairing m = RemoteC2S.fromJson(
        caseNamed('c2s_submit_pairing.json').c2s,
      ) as C2SSubmitPairing;
      expect(m.code, '012345678');
      expect(m.code.length, 9, reason: '当成数字处理会掉一位');
      expect(m.target, 'pc-1');
    });

    test('s2c_hidden_session_started：sid / 对端 / 角色三格都对', () {
      final S2CHiddenSessionStarted m = RemoteS2C.fromJson(
        caseNamed('s2c_hidden_session_started.json').s2c,
      ) as S2CHiddenSessionStarted;
      expect(m.sessionId, 7);
      expect(m.peerDeviceId, 'pc-1');
      expect(m.peerName, '海风的工作站');
      expect(m.role, RemoteRole.controller);
    });

    test('两条 RelayTo 的 sid 刻意不同（上行 7 / 下行 9）', () {
      final C2SRelayTo up =
          RemoteC2S.fromJson(caseNamed('c2s_relay_to.json').c2s) as C2SRelayTo;
      final S2CRelayTo down =
          RemoteS2C.fromJson(caseNamed('s2c_relay_to.json').s2c) as S2CRelayTo;
      expect(up.sessionId, 7);
      expect(down.sessionId, 9);
      // 下行载荷保持不透明：解析失败要能被隔离在数据面，不打掉整条控制面消息。
      expect(down.payload, isA<Map<String, Object?>>());
    });

    test('s2c_pairing_result：剩余次数与失败原因', () {
      final S2CPairingResult m = RemoteS2C.fromJson(
        caseNamed('s2c_pairing_result.json').s2c,
      ) as S2CPairingResult;
      expect(m.reason, PairingFailReason.invalidCode);
      expect(m.attemptsLeft, 4);
    });
  });

  group('调试渲染不泄漏配对码', () {
    test('SubmitPairing / ControlRequested 的 toString 不含码', () {
      // 配对码是一次性口令，进日志没有任何用处，而日志会被截图、被崩溃上报带走。
      final RemoteC2S submit =
          RemoteC2S.fromJson(caseNamed('c2s_submit_pairing.json').c2s);
      expect(submit.toString().contains('012345678'), isFalse);

      final RemoteS2C requested =
          RemoteS2C.fromJson(caseNamed('s2c_hidden_control_requested.json').s2c);
      expect(requested.toString().contains('012345678'), isFalse);
    });

    test('RelayTo 的 toString 不含载荷（那里面是对话正文）', () {
      final RemoteS2C relay =
          RemoteS2C.fromJson(caseNamed('s2c_relay_to.json').s2c);
      expect(relay.toString().contains('DeltaAck'), isFalse);
      expect(relay.toString().contains('9'), isTrue, reason: 'sid 仍要能看到');
    });
  });
}
