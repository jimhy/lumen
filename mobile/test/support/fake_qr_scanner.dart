/// 假的扫码取景。
///
/// 相机是这个 App 里唯一在 `flutter test` 中完全无法模拟的东西，而配对页那条
/// 「扫到码 → 五重闸门 → 四种不同文案」的链路又恰恰最需要测。这个替身把取景换成
/// 几个按钮：点 `scan:<文本>` 就等于「镜头里出现了这段文本」。
library;

import 'package:flutter/material.dart';
import 'package:lumen_mobile/ui/pair/qr_scanner.dart';

/// 按钮式取景。
final class FakeQrScanner implements QrScanner {
  const FakeQrScanner({required this.codes, this.available = true});

  /// 每段文本画一个按钮，key 是 `ValueKey('scan:<文本>')`。
  final List<String> codes;

  @override
  final bool available;

  @override
  Widget view({required void Function(String code) onCode}) => Column(
        mainAxisSize: MainAxisSize.min,
        children: <Widget>[
          for (final String code in codes)
            TextButton(
              key: ValueKey<String>('scan:$code'),
              onPressed: () => onCode(code),
              // 文本刻意不画出来：二维码载荷是一大段 JSON，画上去会把断言里的
              // `findsOneWidget` 变得难以定位（页面上到处都是同样的子串）。
              child: const Text('扫'),
            ),
        ],
      );
}
