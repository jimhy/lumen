/// [`QrScanner`] 的真机实现。**全 App 只有这一个文件 import `mobile_scanner`**，
/// 换扫码包只改这里（与 `markdown_view.dart` 对 `flutter_markdown_plus` 的隔离同款）。
///
/// ## 这里没有业务判断
///
/// 五重闸门全在别处：状态闸在 `link_controller.dart::submitScannedQr` 的
/// `is! LinkPairing`，四重载荷校验在 `protocol/pairing_qr.dart::validate`。
/// 这个文件只负责「把相机画面放上去，识别到什么就原样交出去」——**刻意不在这里过滤**，
/// 哪怕明显不是 Lumen 的码。理由是过滤条件一旦分散到两处，将来改校验规则时必然漏改一处，
/// 而漏改的表现是「有些码扫不出来」，查起来要先怀疑相机。
///
/// ## 权限被拒不是异常路径，是常见路径
///
/// 用户点「不允许」是一次正常选择。所以 [errorBuilder] 给的是一句人话加一条出路
/// （回去手输 9 位码），不是一个错误堆栈——手输那条路本来就完整可用。
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/ui/pair/qr_scanner.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

const String _tag = 'qr-scan';

/// 基于 `mobile_scanner` 的实现。
final class MobileScannerQrScanner implements QrScanner {
  const MobileScannerQrScanner();

  @override
  bool get available => true;

  @override
  Widget view({required void Function(String code) onCode}) =>
      _ScannerView(onCode: onCode);
}

class _ScannerView extends StatefulWidget {
  const _ScannerView({required this.onCode});

  final void Function(String code) onCode;

  @override
  State<_ScannerView> createState() => _ScannerViewState();
}

class _ScannerViewState extends State<_ScannerView> {
  /// ⚠ controller 必须由 State 持有并 dispose：漏掉的表现是**相机一直开着**
  /// （取景页关掉之后指示灯还亮），属于隐私事故级别的疏忽，而不会有任何报错。
  late final MobileScannerController _controller = MobileScannerController(
    // 只认二维码。配对码是我们自己出的，形态确定——放开一维码只会增加误识别
    // 和 MLKit 的解码开销。
    formats: const <BarcodeFormat>[BarcodeFormat.qrCode],
    // normal 自带 250ms 节流。不用 noDuplicates：那是按「上一次识别结果」去重的，
    // 而我们要的去重语义是「这段文本刚被拒过就别再报」——那条在 pair_page 里，
    // 因为只有那里知道校验结果。
    detectionSpeed: DetectionSpeed.normal,
    facing: CameraFacing.back,
  );

  @override
  void dispose() {
    unawaited(_controller.dispose());
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => MobileScanner(
        controller: _controller,
        onDetect: _onDetect,
        onDetectError: (Object e, StackTrace _) =>
            // 单帧解码失败是常态（手抖、对不上焦），记 debug 不打扰用户。
            logDebug(_tag, '单帧解码失败：$e'),
        errorBuilder: _errorBuilder,
        fit: BoxFit.cover,
      );

  void _onDetect(BarcodeCapture capture) {
    for (final Barcode code in capture.barcodes) {
      final String? raw = code.rawValue;
      if (raw == null || raw.isEmpty) continue;
      widget.onCode(raw);
      // 一帧里出现多张码时只取第一张：连着上报会让用户看到几条错误文案闪过，
      // 而他并不知道镜头里有第二张码。
      return;
    }
  }

  Widget _errorBuilder(BuildContext context, MobileScannerException error) {
    final ThemeData theme = Theme.of(context);
    final String message = switch (error.errorCode) {
      MobileScannerErrorCode.permissionDenied =>
        '没有相机权限。可以在系统设置里打开，或者直接手输下面的 9 位配对码。',
      MobileScannerErrorCode.unsupported => '这台设备不支持扫码，请手输 9 位配对码。',
      // 其余都是控制器状态类错误（已释放 / 未初始化 / 正在初始化…），对用户是同一件事：
      // 相机这条路现在走不通，但手输那条一直是通的。**不把内部错误码拼给用户看**。
      _ => '相机打不开，请手输 9 位配对码。',
    };
    // 这条日志带原始错误码，是排查真机问题时唯一的线索。
    logWarn(_tag, '取景失败', error: error);
    return ColoredBox(
      color: theme.colorScheme.surfaceContainerHighest,
      child: Center(
        child: Padding(
          padding: const EdgeInsets.all(24),
          child: Text(message, textAlign: TextAlign.center),
        ),
      ),
    );
  }
}
