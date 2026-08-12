/// 凭据的**落盘编解码**。
///
/// 这一层单独测的理由：真正的存储（`SecureTokenStore`）走 platform channel，在
/// `flutter test` 里一行都跑不了。把「编解码」与「调 Keychain」切开之后，前者是纯
/// Dart、能被完整覆盖，后者只剩没有分支的 IO——这样「凭据存进去又读出来对不上」
/// 这类 bug 不必等到真机才暴露。
///
/// ★ 重点在**读不懂时的行为**：损坏 / 跨版本的凭据必须变成一个明确的异常，
/// 由调用方翻译成「没登录」。悄悄返回一份缺字段的 [SessionTokens] 才是最坏的结果
/// ——它会带着空 token 去打服务端，用户看到的是「登录了但一直 401」。
library;

import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/data/token_store.dart';
import 'package:lumen_mobile/protocol/codec.dart';

const SessionTokens _sample = SessionTokens(
  token: 'eyJhbGciOiJIUzI1NiJ9.payload.sig',
  expiresAt: 1786500000,
  deviceId: 'dev-abc',
  origin: 'https://lumen.example.com',
  userId: 'user-42',
);

void main() {
  group('落盘往返', () {
    test('五个字段一个不少地回来', () {
      final SessionTokens back = SessionTokens.fromStorageJson(
        asJsonMap(jsonDecode(jsonEncode(_sample.toStorageJson())), '凭据'),
      );

      expect(back.token, _sample.token);
      expect(back.expiresAt, _sample.expiresAt);
      expect(back.deviceId, _sample.deviceId);
      expect(back.origin, _sample.origin);
      // ★ userId 最容易被漏：它只在**登录**时由服务端回传，冷启动恢复登录态那条路径
      // 拿不到。丢了它，重启 App 之后扫码会一律判成 ForeignAccount——那看起来像
      // 「扫码功能坏了」，不像「凭据少存了一个字段」。
      expect(back.userId, _sample.userId);
    });

    test('带版本号写出去', () {
      expect(_sample.toStorageJson()['v'], kSessionTokensStorageVersion);
    });
  });

  group('读不懂时抛，不猜', () {
    test('版本号对不上', () {
      final JsonMap raw = _sample.toStorageJson()
        ..['v'] = kSessionTokensStorageVersion + 1;

      expect(
        () => SessionTokens.fromStorageJson(raw),
        throwsA(isA<WireFormatException>()),
      );
    });

    test('没有版本号（老格式 / 别的东西）', () {
      final JsonMap raw = _sample.toStorageJson()..remove('v');

      expect(
        () => SessionTokens.fromStorageJson(raw),
        throwsA(isA<WireFormatException>()),
      );
    });

    test('★ 缺字段一律抛，不给默认值', () {
      for (final String key in <String>[
        'token',
        'exp',
        'device_id',
        'origin',
        'user_id',
      ]) {
        final JsonMap raw = _sample.toStorageJson()..remove(key);

        expect(
          () => SessionTokens.fromStorageJson(raw),
          throwsA(isA<WireFormatException>()),
          reason: '缺 $key 时给默认值，等于拿半份凭据去打服务端',
        );
      }
    });

    test('类型不对也抛（exp 写成字符串）', () {
      final JsonMap raw = _sample.toStorageJson()..['exp'] = '1786500000';

      expect(
        () => SessionTokens.fromStorageJson(raw),
        throwsA(isA<WireFormatException>()),
      );
    });
  });

  test('★ toString 不吐 token', () {
    // 凭据会进日志（`logWarn(_tag, '…', error: e)` 一类），而这份 token 七天内无法作废。
    expect(_sample.toString(), isNot(contains(_sample.token)));
  });

  group('内存实现', () {
    test('不报降级——它是刻意的测试替身，不是坏掉的存储', () {
      expect(InMemoryTokenStore().degraded, isFalse);
    });

    test('写完能读回来，clear 之后为空', () async {
      final InMemoryTokenStore store = InMemoryTokenStore();

      await store.write(_sample);
      expect((await store.read())?.deviceId, 'dev-abc');

      await store.clear();
      expect(await store.read(), isNull);
    });
  });
}
