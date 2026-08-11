/// `hw_id` 的验收。
///
/// 三条断言各守一个「算得出结果、但结果与桌面端不同」的错法——这类错误不会报错，
/// 只会让同一台手机在服务端分裂成两台设备：
///
/// 1. 忘了中间那个 `0x00` 分隔符；
/// 2. 用 `codeUnits`（UTF-16）而不是 `utf8.encode`；
/// 3. hex 输出成大写。
///
/// 期望值是**独立算出来的**（`printf 'origin\x00id' | sha256sum`），不是把实现跑一遍
/// 抄下来——后者只能证明实现是确定性的，证明不了它算对了。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/data/device_identity.dart';

/// `printf 'https://lumen.example.com\x00vendor-id-1234' | sha256sum`
const String _asciiVector =
    'bad765eae393ddbac43b764c18cb5b9286b482e1238adff3df0e7bc64e56c776';

/// `printf 'https://lumen.example.com\x00中文机器' | sha256sum`（UTF-8 字节）
const String _cjkVector =
    '0c7c540fc587d35ea23f5d8fee9347b406edab9f4f9b06012d8f2b485436d631';

void main() {
  group('computeHwId 与桌面端逐字节一致', () {
    test('ASCII 向量', () {
      expect(
        computeHwId(
          canonicalOrigin: 'https://lumen.example.com',
          rawMachineId: 'vendor-id-1234',
        ),
        _asciiVector,
      );
    });

    test('非 ASCII 机器标识按 UTF-8 参与哈希', () {
      // 用 codeUnits（UTF-16）会算出另一个值，而且**不会报错**。
      expect(
        computeHwId(
          canonicalOrigin: 'https://lumen.example.com',
          rawMachineId: '中文机器',
        ),
        _cjkVector,
      );
    });

    test('hex 是小写（桌面端 HEX = "0123456789abcdef"）', () {
      final String id = computeHwId(
        canonicalOrigin: 'https://lumen.example.com',
        rawMachineId: 'vendor-id-1234',
      );
      expect(id, id.toLowerCase());
      expect(id.length, 64);
    });

    test('0x00 分隔符不能省：拼接歧义必须被挡住', () {
      // 没有分隔符时 ("https://a.com", "bc") 与 ("https://a.comb", "c") 会撞成同一个哈希。
      final String a =
          computeHwId(canonicalOrigin: 'https://a.com', rawMachineId: 'bc');
      final String b =
          computeHwId(canonicalOrigin: 'https://a.comb', rawMachineId: 'c');
      expect(a, isNot(b));
    });
  });

  group('伪名化：不同 origin 得到不同 hw_id', () {
    test('同一台机器在两台服务器上不可关联', () {
      // 这是应用商店隐私清单上的必答题，也是绕这道哈希的全部理由。
      final String onA = computeHwId(
        canonicalOrigin: 'https://a.example.com',
        rawMachineId: 'same-device',
      );
      final String onB = computeHwId(
        canonicalOrigin: 'https://b.example.com',
        rawMachineId: 'same-device',
      );
      expect(onA, isNot(onB));
    });
  });

  group('DeviceIdentity', () {
    test('取不到机器标识时返回 null，**不编一个随机值**', () async {
      // 随机兜底等于每次启动都是一台新机器，比没有 hw_id 更糟。
      const DeviceIdentity id = DeviceIdentity(StaticMachineId(null));
      expect(await id.hwIdFor('https://lumen.example.com'), isNull);
    });

    test('空串也当作取不到', () async {
      const DeviceIdentity id = DeviceIdentity(StaticMachineId(''));
      expect(await id.hwIdFor('https://lumen.example.com'), isNull);
    });

    test('正常路径与 computeHwId 一致', () async {
      const DeviceIdentity id = DeviceIdentity(StaticMachineId('vendor-id-1234'));
      expect(await id.hwIdFor('https://lumen.example.com'), _asciiVector);
    });
  });
}
