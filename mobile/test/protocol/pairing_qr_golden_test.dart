/// **片 6 的扫码验收**：`qr_*` 语料与 Dart 侧的四重校验对齐。
///
/// 这一层守的重点**不是往返**（那只是顺手验的），而是 `validate` 的判定与顺序：
/// 形状（魔数 / 码格式）先于身份（服务器 / 账户 / 设备），且四种拒绝互不混淆——
/// 每一种对应一条不同的 UI 文案，合并成一句「二维码无效」，用户就永远不知道自己是
/// 扫错了设备、扫了别人的码，还是**遇到了钓鱼**。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/protocol/codec.dart';
import 'package:lumen_mobile/protocol/pairing_qr.dart';

import 'corpus.dart';

void main() {
  final List<GoldenCase> corpus = loadCorpus();
  final List<GoldenCase> qrCases =
      corpus.where((GoldenCase c) => c.env.containsKey('qr')).toList();

  JsonMap checkOf(GoldenCase c) => asJsonMap(c.env['qr_check'], 'qr_check');

  group('qr 语料 ⇄ Dart 模型与四重校验', () {
    test('语料条数够，且五种结果都有', () {
      expect(qrCases, isNotEmpty, reason: '一条 qr 语料都没读到，路径或前缀改了？');
      final Set<String> verdicts = <String>{
        for (final GoldenCase c in qrCases) checkOf(c)['verdict']! as String,
      };
      expect(
        verdicts,
        <String>{'Ok', 'Malformed', 'ForeignServer', 'ForeignAccount', 'WrongTarget'},
        reason: '少一种就有一条 UI 文案分支没人测过',
      );
    });

    for (final GoldenCase c in qrCases) {
      test('${c.name}：${c.note.split('。').first}', () {
        final JsonMap raw = asJsonMap(c.env['qr'], 'qr');
        final PairingQrPayload payload = PairingQrPayload.fromJson(raw);

        // 顺手验往返：字段是单字母短名，写错一个在这里就露馅。
        expect(
          deepJsonEquals(payload.toJson(), raw),
          isTrue,
          reason: '${c.name} 载荷往返不等。\n'
              '--- 实际 ---\n${pretty(payload.toJson())}\n'
              '--- 期望 ---\n${pretty(raw)}',
        );

        final JsonMap check = checkOf(c);
        final PairingQrError? actual = payload.validate(
          expectedOrigin: check['origin']! as String,
          expectedUserFingerprint: check['user_fingerprint']! as String,
          expectedTarget: check['target']! as String,
        );
        expect(
          actual?.code ?? 'Ok',
          check['verdict'],
          reason: '${c.name} 的校验结果与语料声明不符\n—— note: ${c.note}',
        );
      });
    }
  });

  group('账户指纹与 Rust 侧同源', () {
    test('语料里的指纹能被本端算出来', () {
      // 语料里的 u 是 sha256("550e8400-e29b-41d4-a716-446655440000") 的前 16 个 hex。
      // 这条把「两端各写一遍指纹算法」这个漂移点钉死——算法一不同，所有扫码都会判成
      // ForeignAccount，而那看起来像功能没做好。
      expect(
        accountFingerprint('550e8400-e29b-41d4-a716-446655440000'),
        'a3a9e1ed9732cab2',
      );
      expect(
        accountFingerprint('999e8400-e29b-41d4-a716-446655440999'),
        'c91724cad3fbbf79',
      );
    });

    test('长度固定、全小写 hex', () {
      final String fp = accountFingerprint('whatever');
      expect(fp.length, kAccountFingerprintLen);
      expect(fp, matches(RegExp(r'^[0-9a-f]+$')));
    });
  });

  group('解析失败一律当 Malformed（不是崩溃）', () {
    test('不是 JSON', () {
      expect(
        () => PairingQrPayload.parse('随手扫到的一张二维码'),
        throwsA(isA<WireFormatException>()),
      );
    });

    test('是 JSON 但缺字段', () {
      expect(
        () => PairingQrPayload.parse('{"m":"lumen.pair.v1"}'),
        throwsA(isA<WireFormatException>()),
      );
    });

    test('e 是字符串而不是整数', () {
      expect(
        () => PairingQrPayload.parse(
          '{"m":"lumen.pair.v1","o":"https://a.com","u":"a3a9e1ed9732cab2",'
          '"t":"pc-1","c":"012345678","e":"1786342908"}',
        ),
        throwsA(isA<WireFormatException>()),
      );
    });
  });

  group('配对码的位数判定', () {
    PairingQrError? verdictFor(String code) => PairingQrPayload(
          magic: kPairingQrMagic,
          origin: 'https://a.com',
          userFingerprint: 'a3a9e1ed9732cab2',
          target: 'pc-1',
          code: code,
          expiresAt: 0,
        ).validate(
          expectedOrigin: 'https://a.com',
          expectedUserFingerprint: 'a3a9e1ed9732cab2',
          expectedTarget: 'pc-1',
        );

    test('前导零必须保留（不能用 int.tryParse 判定）', () {
      // int.tryParse("012345678") 得到 12345678，再拼回去就少一位——
      // 而服务端比对的是逐字符相等。
      expect(verdictFor('012345678'), isNull);
    });

    test('长度不对 / 含非数字 / 带正负号与空白，一律 Malformed', () {
      for (final String bad in <String>[
        '12345678', // 8 位
        '0123456789', // 10 位
        '01234567x',
        '+12345678',
        ' 12345678',
        '',
      ]) {
        expect(verdictFor(bad), PairingQrError.malformed, reason: '「$bad」应当被拒');
      }
    });
  });

  group('过期时刻不参与校验', () {
    test('e 为 0（1970 年）也照样通过', () {
      // 服务端 TTL 才是权威；两端时钟偏差不得造成用户无法自助的故障。
      final PairingQrError? verdict = const PairingQrPayload(
        magic: kPairingQrMagic,
        origin: 'https://a.com',
        userFingerprint: 'a3a9e1ed9732cab2',
        target: 'pc-1',
        code: '012345678',
        expiresAt: 0,
      ).validate(
        expectedOrigin: 'https://a.com',
        expectedUserFingerprint: 'a3a9e1ed9732cab2',
        expectedTarget: 'pc-1',
      );
      expect(verdict, isNull);
    });
  });

  group('调试渲染不泄漏配对码', () {
    test('toString 里没有那 9 位数字', () {
      const PairingQrPayload p = PairingQrPayload(
        magic: kPairingQrMagic,
        origin: 'https://a.com',
        userFingerprint: 'a3a9e1ed9732cab2',
        target: 'pc-1',
        code: '012345678',
        expiresAt: 0,
      );
      expect(p.toString().contains('012345678'), isFalse);
      expect(p.toString().contains('pc-1'), isTrue, reason: '目标设备要能看到，便于排障');
    });
  });
}
