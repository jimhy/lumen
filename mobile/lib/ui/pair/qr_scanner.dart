/// 扫码取景的注入点。
///
/// ## 为什么要抽这一层
///
/// 相机是这个 App 里**唯一**在 `flutter test` 里完全无法模拟的东西：`mobile_scanner`
/// 的 widget 一建起来就去要 platform channel。不抽的话，配对页那条「扫到码 → 五重闸门
/// → 四种不同的错误文案」的链路就一行测试也写不了——而那正是最需要测的一段。
///
/// 抽掉之后：`FakeQrScanner`（见 `test/support/fake_qr_scanner.dart`）返回几个按钮，
/// 点一下就等于「扫到了这段文本」，整条链路连同文案一起进 widget 测试。
/// 真机实现只剩「把相机画面放上去」这一件没有分支的事。
///
/// ## [available] 为什么在接口上
///
/// 协作铁律 6（无声降级禁令）：**不给一个点了没反应的按钮**。默认实现
/// [UnavailableQrScanner] 报 `false`，配对页据此**根本不画**扫码入口，用户看到的是
/// 一个纯手输的页面——那是完整可用的功能，不是坏掉的功能。
library;

import 'package:flutter/widgets.dart';

/// 扫码取景。
abstract interface class QrScanner {
  /// 本机是否具备扫码能力。
  ///
  /// `false` 时 [view] **不会**被调用，配对页也不画扫码入口。
  bool get available;

  /// 取景 Widget。
  ///
  /// [onCode] 在识别到码时调用，参数是码里的原始文本。⚠ **可能被连续调用很多次**
  /// （同一张码每帧都会命中），调用方必须自己做去重——`pair_page.dart` 靠
  /// 「记住上一段被拒的文本」处理，别把去重责任推给实现。
  Widget view({required void Function(String code) onCode});
}

/// 没有相机的默认实现。
///
/// 用在两处：`providers.dart` 的默认值（让纯 Dart 测试不碰 platform channel），
/// 以及将来可能出现的、没有相机的平台。
final class UnavailableQrScanner implements QrScanner {
  const UnavailableQrScanner();

  @override
  bool get available => false;

  /// [available] 是 `false` 时调用方不该走到这里。抛而不是返回一个占位 Widget：
  /// 占位 Widget 会变成一块「看起来是相机、其实什么也不会发生」的黑框。
  @override
  Widget view({required void Function(String code) onCode}) =>
      throw UnsupportedError('本机没有可用的扫码实现；调用前应先看 available');
}
