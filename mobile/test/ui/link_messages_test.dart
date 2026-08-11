/// 文案守卫。
///
/// 这些不是装饰：它们是用户唯一能看到的「为什么没连上」。片 6 的设计里有三条是**硬要求**，
/// 说错了会把用户引到错误的方向，这里逐条钉住。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/protocol/pairing_qr.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/ui/link_messages.dart';

void main() {
  group('每个取值都有文案，且互不相同', () {
    test('十个 DenyReason 全覆盖', () {
      // switch 是穷尽的，加取值编译不过——这条守的是「文案没被复制粘贴成同一句」。
      final Set<String> messages =
          DenyReason.values.map(denyReasonMessage).toSet();
      expect(messages.length, DenyReason.values.length,
          reason: '有两个拒因共用了同一句文案，用户分不出发生了什么');
      for (final String m in messages) {
        expect(m.trim(), isNotEmpty);
      }
    });

    test('五个 LinkFailure 全覆盖', () {
      final Set<String> messages =
          LinkFailure.values.map(linkFailureMessage).toSet();
      expect(messages.length, LinkFailure.values.length);
    });

    test('四个 PairingQrError 全覆盖', () {
      final Set<String> messages =
          PairingQrError.values.map(qrErrorMessage).toSet();
      expect(messages.length, PairingQrError.values.length);
    });
  });

  group('★ 三条说错了会把用户引错方向的', () {
    test('AlreadyControlled 必须给出具体数字', () {
      // 隐藏会话在被控端**没有任何界面痕迹**：只说「已被占用」，用户会去找一个
      // 并不存在的占用者。
      final String m = denyReasonMessage(DenyReason.alreadyControlled);
      expect(m.contains('$kMaxHiddenSessionsPerTarget'), isTrue,
          reason: '文案里必须出现上限数字，实得：$m');
      expect(m.contains('已被占用'), isFalse);
    });

    test('Offline 必须同时覆盖「版本过低」', () {
      // 服务端在目标未上报 hidden 能力位时复用了这条拒因（新增拒因会打掉老客户端
      // 唯一的回执），所以「不在线」与「那台 PC 的 Lumen 太旧」在线上是同一个值。
      final String m = denyReasonMessage(DenyReason.offline);
      expect(m.contains('版本'), isTrue, reason: '只说「不在线」会让用户白等，实得：$m');
    });

    test('serverTooOld 指向版本而不是网络', () {
      // 让用户去重启路由器是最坏的引导——这条是老服务端 / 老 PC 完全无回音时的
      // 唯一判定结果。
      final String m = linkFailureMessage(LinkFailure.serverTooOld);
      expect(m.contains('版本'), isTrue);
      expect(m.contains('网络'), isFalse, reason: '实得：$m');
    });

    test('TargetPairing 不说「已被占用」', () {
      // 配对码是给人念的，同一时刻那台电脑屏幕上只该有一个码——这是「稍候」，不是「被占」。
      final String m = denyReasonMessage(DenyReason.targetPairing);
      expect(m.contains('稍候'), isTrue, reason: '实得：$m');
    });
  });

  group('扫码的四种拒绝语气分明', () {
    test('ForeignServer 是安全警告，其余不是', () {
      final String phishing = qrErrorMessage(PairingQrError.foreignServer);
      expect(phishing.contains('安全'), isTrue, reason: '实得：$phishing');
      // 多屏场景下扫错设备很常见，**不是**攻击，语气要平实、可自行纠正。
      final String wrong = qrErrorMessage(PairingQrError.wrongTarget);
      expect(wrong.contains('安全'), isFalse, reason: '实得：$wrong');
      expect(wrong.contains('当心'), isFalse);
    });

    test('三种拒绝各自指向不同的原因词', () {
      expect(qrErrorMessage(PairingQrError.malformed).contains('Lumen'), isTrue);
      expect(
        qrErrorMessage(PairingQrError.foreignAccount).contains('别人'),
        isTrue,
      );
      expect(qrErrorMessage(PairingQrError.wrongTarget).contains('电脑'), isTrue);
    });
  });

  group('输码失败文案带剩余次数', () {
    test('码错时报出还能试几次', () {
      expect(
        pairingFailMessage(PairingFailReason.invalidCode, 3).contains('3'),
        isTrue,
      );
    });
  });
}
