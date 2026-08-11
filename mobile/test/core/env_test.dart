/// 服务器地址规范化——**与桌面端 `cloud.rs::canonical_server_origin` 用同一批用例**。
///
/// 两端必须逐字节一致：`hw_id = sha256(canonical_origin ‖ 0x00 ‖ 机器标识)`，
/// origin 差一个字符就是另一台设备。下面的合法/非法两组用例逐条抄自那边的
/// `canonical_origin_规范化` 与 `canonical_origin_拒绝非根与注入`。
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:lumen_mobile/core/env.dart';

void main() {
  group('规范化（与桌面端同一批用例）', () {
    test('补 https、小写化、去默认端口', () {
      expect(canonicalServerOrigin('Example.COM:443/'), 'https://example.com');
      expect(
        canonicalServerOrigin('HTTPS://Example.COM:443/'),
        'https://example.com',
      );
      expect(canonicalServerOrigin('http://127.0.0.1:80/'), 'http://127.0.0.1');
    });

    test('IPv6 字面量保留方括号', () {
      // Uri.host 不带方括号，拼回去必须补上——否则 `http://::1:8080` 再也解析不回来。
      expect(canonicalServerOrigin('https://[::1]:8443/'), 'https://[::1]:8443');
    });

    test('非默认端口保留', () {
      expect(canonicalServerOrigin('lumen.example.com:8443'),
          'https://lumen.example.com:8443');
      expect(canonicalServerOrigin('http://192.168.1.9:3000'),
          'http://192.168.1.9:3000');
    });

    test('不带 scheme 时补的是 https 而不是 http', () {
      // 补 http 会让一次手滑变成明文链路，而 token 走的是 Authorization 头。
      expect(canonicalServerOrigin('lumen.example.com'),
          'https://lumen.example.com');
    });

    test('同一台服务器的多种写法收敛成同一个 origin', () {
      // 这正是 hw_id 稳定的前提：三种写法算出三个 origin ⇒ 同一台手机分裂成三台幽灵设备。
      const List<String> sameServer = <String>[
        'lumen.example.com',
        'https://lumen.example.com',
        'https://lumen.example.com/',
        'https://LUMEN.example.com:443',
        '  https://lumen.example.com  ',
      ];
      final Set<String?> canonical =
          sameServer.map(canonicalServerOrigin).toSet();
      expect(canonical, <String>{'https://lumen.example.com'});
    });
  });

  group('拒绝（与桌面端同一批用例）', () {
    test('空 / 非 http(s) / 带路径 / 带凭据 / 控制字符一律拒绝', () {
      const List<String> invalid = <String>[
        '',
        '   ',
        'ftp://example.com',
        'https://example.com.',
        'https://@example.com',
        'https://user@example.com',
        'https://example.com:',
        'https://example.com/./',
        'https://example.com/a/../',
        'https://example.com/path',
        'https://example.com?query=1',
        'https://example.com#fragment',
        'https://example.com/\r\n',
      ];
      for (final String raw in invalid) {
        expect(
          canonicalServerOrigin(raw),
          isNull,
          reason: '应当拒绝：「$raw」',
        );
      }
    });

    test('反斜杠单独拒绝（两套 URL 解析器在这里口径不同）', () {
      // 桌面端把 `\` 当 authority 终止符后判「有多余后缀」而拒绝；Dart 的 Uri 会把它当
      // 普通路径字符。不显式拒掉，两端就会对同一个输入给出不同的 origin。
      expect(canonicalServerOrigin(r'https://example.com\evil'), isNull);
      expect(canonicalServerOrigin(r'example.com\..\x'), isNull);
    });
  });

  group('ServerEndpoint', () {
    test('REST base 与 WS URL 从同一个 origin 派生', () {
      final ServerEndpoint e = ServerEndpoint.tryParse('lumen.example.com')!;
      expect(e.restBase, 'https://lumen.example.com');
      expect(e.wsUrl, 'wss://lumen.example.com/api/v1/ws');
    });

    test('http 派生 ws（不是 wss）', () {
      final ServerEndpoint e = ServerEndpoint.tryParse('http://192.168.1.9:3000')!;
      expect(e.wsUrl, 'ws://192.168.1.9:3000/api/v1/ws');
    });

    test('明文 HTTP 判据：回环不算不安全', () {
      expect(ServerEndpoint.tryParse('http://192.168.1.9:3000')!.isInsecure, isTrue);
      expect(ServerEndpoint.tryParse('http://127.0.0.1:3000')!.isInsecure, isFalse);
      expect(ServerEndpoint.tryParse('http://localhost:3000')!.isInsecure, isFalse);
      expect(ServerEndpoint.tryParse('https://lumen.example.com')!.isInsecure, isFalse);
    });

    test('非法地址返回 null', () {
      expect(ServerEndpoint.tryParse('ftp://example.com'), isNull);
    });
  });
}
