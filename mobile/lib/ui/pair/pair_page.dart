/// 配对页：输入那台电脑屏幕上显示的 9 位码。
///
/// ## ★ 不做「记住这台电脑、下次免码」
///
/// 隐藏会话**每次都要念码**：服务端 `ws.rs` 的 `OpenHidden` 臂恒传 `paired = false`，
/// `hub.rs::submit_pairing` 的 Hidden 分支恒不写 `device_pairs`。那是片 4b 的止血——
/// `device_pairs` 没有会话种类列，共用一行信任会让「手机为跟 LLM 说话念的那次码」
/// 顺带授出**镜像**权限（全屏 + 远程输入）。
///
/// 所以这一页不能有「记住」开关。UI 上把「每次都要念码」讲清楚，用户才不会当成 bug。
///
/// ## 倒计时用服务端给的 TTL
///
/// `expiresInSecs` 来自 `PairingNeeded`，**不在客户端写死 120**：服务端改了 TTL 而客户端
/// 没跟着改，表现是「倒计时还剩 30 秒但码已经失效」，用户会以为是自己输慢了。
///
/// ## 扫码是这一页的一个模式，不是另一个页面（片 12）
///
/// 取景直接嵌在本页里、用 [_ScanMode] 切换，**没有** `Navigator.push`。理由是这一页
/// 由 `router.dart` 的 redirect 管着：状态一离开 `LinkPairing`（配对成功 / 被拒 / 超时），
/// 整页会被换走。取景要是压在导航栈的另一层上，就得自己再处理一遍「页没了但相机还开着」，
/// 而那正是最容易漏的一种泄漏。
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/protocol/pairing_qr.dart';
import 'package:lumen_mobile/state/auth_controller.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/link_messages.dart';

const String _tag = 'pair-page';

/// 本页的两种模式。
enum _ScanMode {
  /// 手输 9 位码。**永远可用**，也是没有相机时的唯一模式。
  manual,

  /// 相机取景。
  camera,
}

/// 配对页。
class PairPage extends ConsumerStatefulWidget {
  const PairPage({super.key});

  @override
  ConsumerState<PairPage> createState() => _PairPageState();
}

class _PairPageState extends ConsumerState<PairPage> {
  final TextEditingController _code = TextEditingController();
  Timer? _tick;

  /// 剩余秒数。进入本页时按服务端 TTL 起算。
  int _remaining = 0;

  _ScanMode _mode = _ScanMode.manual;

  /// 上一段被拒的二维码文本，以及它的判定。
  ///
  /// **去重的判据是文本本身，不是时间**：同一张码在镜头前每帧都会命中，不记住它就会
  /// 每秒刷十几条同样的错误文案。换一张码（哪怕仍然错）应当立刻给出新的反馈，
  /// 所以不能用「若干秒内不再提示」那种时间窗——那会把「换了张码还是不对」吞掉。
  String? _rejectedText;
  PairingQrError? _rejectedError;

  @override
  void dispose() {
    _tick?.cancel();
    _code.dispose();
    super.dispose();
  }

  /// 按服务端给的 TTL 起倒计时。**每秒一次**，到 0 即停——不做「到 0 自动重发」，
  /// 那会在用户不知情的情况下让对方电脑再弹一次码。
  void _startCountdown(int seconds) {
    if (_tick != null && _remaining > 0) return;
    _tick?.cancel();
    _remaining = seconds;
    _tick = Timer.periodic(const Duration(seconds: 1), (Timer timer) {
      if (!mounted) {
        timer.cancel();
        return;
      }
      setState(() => _remaining = _remaining > 0 ? _remaining - 1 : 0);
      if (_remaining == 0) timer.cancel();
    });
  }

  @override
  Widget build(BuildContext context) {
    final LinkController? link = ref.watch(linkControllerProvider);
    if (link == null) {
      return const Scaffold(body: Center(child: Text('尚未选定服务器')));
    }
    return StreamBuilder<LinkState>(
      stream: link.states,
      initialData: link.state,
      builder: (BuildContext context, AsyncSnapshot<LinkState> snapshot) {
        final LinkState state = snapshot.data ?? const LinkIdle();
        if (state is! LinkPairing) {
          // 状态已经走掉了（建成 / 被拒 / 超时），路由的 redirect 会把页面换走，
          // 这一帧先画个占位，不闪错误。
          return const Scaffold(body: Center(child: CircularProgressIndicator()));
        }
        WidgetsBinding.instance.addPostFrameCallback(
          (_) => _startCountdown(state.expiresInSecs),
        );
        return Scaffold(
          appBar: AppBar(
            title: Text('连接 ${state.targetName}'),
            leading: IconButton(
              icon: const Icon(Icons.close),
              tooltip: '取消',
              onPressed: link.close,
            ),
          ),
          body: Center(
            child: ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 420),
              child: SingleChildScrollView(
                padding: const EdgeInsets.all(24),
                child: switch (_mode) {
                  _ScanMode.manual => _form(context, link, state),
                  _ScanMode.camera => _camera(context, link, state),
                },
              ),
            ),
          ),
        );
      },
    );
  }

  Widget _form(BuildContext context, LinkController link, LinkPairing state) {
    final ThemeData theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: <Widget>[
        Text(
          '在 ${state.targetName} 的屏幕上会显示一个 9 位配对码，把它输在这里。',
          style: theme.textTheme.bodyLarge,
        ),
        const SizedBox(height: 8),
        // 把「每次都要念码」讲明白：这是刻意的安全取舍，不是还没做的功能。
        Text(
          '出于安全考虑，每次连接都需要重新输入配对码。',
          style: theme.textTheme.bodySmall
              ?.copyWith(color: theme.colorScheme.onSurfaceVariant),
        ),
        const SizedBox(height: 20),
        TextField(
          controller: _code,
          autofocus: true,
          keyboardType: TextInputType.number,
          maxLength: kPairingCodeLen,
          // 只收数字：服务端比对的是逐字符相等，让用户输进空格或字母只会白白
          // 消耗掉 5 次尝试里的一次。
          inputFormatters: <TextInputFormatter>[
            FilteringTextInputFormatter.digitsOnly,
          ],
          style: theme.textTheme.headlineSmall?.copyWith(letterSpacing: 6),
          textAlign: TextAlign.center,
          decoration: const InputDecoration(
            border: OutlineInputBorder(),
            counterText: '',
            hintText: '000000000',
          ),
          onSubmitted: (_) => _submit(link),
        ),
        const SizedBox(height: 8),
        Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: <Widget>[
            Text(
              _remaining > 0 ? '剩余 $_remaining 秒' : '配对码可能已过期',
              style: theme.textTheme.bodySmall,
            ),
            Text(
              '还可以试 ${state.attemptsLeft} 次',
              style: theme.textTheme.bodySmall,
            ),
          ],
        ),
        if (state.lastError != null) ...<Widget>[
          const SizedBox(height: 12),
          Text(
            pairingFailMessage(state.lastError!, state.attemptsLeft),
            style: TextStyle(color: theme.colorScheme.error),
          ),
        ],
        const SizedBox(height: 20),
        FilledButton(
          onPressed: () => _submit(link),
          child: const Text('连接'),
        ),
        const SizedBox(height: 12),
        // 扫码入口。**没有相机就不画**（`available == false`）：手输这条路本来就完整可用，
        // 画一个点了没反应的按钮才是坏掉的功能（§14-6 无声降级禁令）。
        if (_canScan) ...<Widget>[
          OutlinedButton.icon(
            onPressed: () => setState(() {
              _mode = _ScanMode.camera;
              // 换模式即清掉上一轮的拒绝痕迹：留着会让重新打开相机时先闪一条
              // 上次的错误，用户不知道它说的是哪张码。
              _rejectedText = null;
              _rejectedError = null;
            }),
            icon: const Icon(Icons.qr_code_scanner),
            label: const Text('扫描电脑上的二维码'),
          ),
        ],
      ],
    );
  }

  /// 相机取景 + 一条随时可回手输的出路。
  Widget _camera(BuildContext context, LinkController link, LinkPairing state) {
    final ThemeData theme = Theme.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: <Widget>[
        Text(
          '把镜头对准 ${state.targetName} 屏幕上的二维码。',
          style: theme.textTheme.bodyLarge,
        ),
        const SizedBox(height: 16),
        // 取景固定成正方形：二维码是方的，用整屏高度既没用又会把下面那条出路挤出屏幕。
        AspectRatio(
          aspectRatio: 1,
          child: ClipRRect(
            borderRadius: BorderRadius.circular(12),
            child: ref
                .watch(qrScannerProvider)
                .view(onCode: (String text) => _onScanned(link, text)),
          ),
        ),
        const SizedBox(height: 12),
        if (_rejectedError != null)
          Text(
            qrErrorMessage(_rejectedError!),
            textAlign: TextAlign.center,
            style: TextStyle(
              // ForeignServer 是钓鱼信号，其余三种多半只是扫错了东西——用颜色把这两类
              // 分开，文案本身的语气也已经分开了（见 `qrErrorMessage`）。
              color: _rejectedError == PairingQrError.foreignServer
                  ? theme.colorScheme.error
                  : theme.colorScheme.onSurfaceVariant,
            ),
          ),
        const SizedBox(height: 12),
        Row(
          mainAxisAlignment: MainAxisAlignment.spaceBetween,
          children: <Widget>[
            Text(
              _remaining > 0 ? '剩余 $_remaining 秒' : '配对码可能已过期',
              style: theme.textTheme.bodySmall,
            ),
            Text(
              '还可以试 ${state.attemptsLeft} 次',
              style: theme.textTheme.bodySmall,
            ),
          ],
        ),
        const SizedBox(height: 12),
        TextButton(
          onPressed: () => setState(() => _mode = _ScanMode.manual),
          child: const Text('改用手输 9 位码'),
        ),
      ],
    );
  }

  /// 本机能不能扫码。
  ///
  /// 两个条件缺一不可：有相机实现，**且**拿得到账户指纹。后者是四重校验里
  /// [`PairingQrError.foreignAccount`] 那一重的比对基准——拿不到它就没法判断这张码
  /// 是不是别人的，此时宁可不给扫码入口，也不能放一道形同虚设的校验过去。
  bool get _canScan =>
      ref.watch(qrScannerProvider).available && _userFingerprint() != null;

  /// 当前账户的指纹。取不到返回 null。
  String? _userFingerprint() {
    final AuthState? auth = ref.read(authControllerProvider)?.state;
    if (auth is! AuthLoggedIn) return null;
    final String id = auth.user.id;
    return id.isEmpty ? null : accountFingerprint(id);
  }

  /// 扫到一段文本。
  void _onScanned(LinkController link, String text) {
    // 同一张码每帧都会命中，靠文本去重（理由见 `_rejectedText` 的注释）。
    if (text == _rejectedText) return;
    final String? fingerprint = _userFingerprint();
    if (fingerprint == null) {
      // 理论上到不了这里（`_canScan` 已经挡过一次），但登录态是会在页面活着时变的。
      logWarn(_tag, '扫到码时拿不到账户指纹，已忽略');
      return;
    }
    final PairingQrError? verdict =
        link.submitScannedQr(text, userFingerprint: fingerprint);
    if (verdict == null) {
      // 已提交。**留在相机模式**等服务端回话：这时候切回手输会让用户以为要再输一次，
      // 而 redirect 马上就会把整页换走（成功进对话页，失败回设备列表）。
      setState(() {
        _rejectedText = null;
        _rejectedError = null;
      });
      return;
    }
    setState(() {
      _rejectedText = text;
      _rejectedError = verdict;
    });
  }

  void _submit(LinkController link) {
    final String code = _code.text.trim();
    if (code.length != kPairingCodeLen) return;
    link.submitCode(code);
    _code.clear();
  }
}
