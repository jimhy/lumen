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
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/protocol/pairing_qr.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/link_messages.dart';

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
                child: _form(context, link, state),
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
        // 扫码入口：等 PC 侧二维码渲染接上之后再放出来（被工具链问题挡住，
        // 见交接文档 §5.7）。数字码这条路径本来就够用，所以这里不放一个点了没反应的按钮
        // ——那正是 §14-6「无声降级禁令」要挡的形态。
      ],
    );
  }

  void _submit(LinkController link) {
    final String code = _code.text.trim();
    if (code.length != kPairingCodeLen) return;
    link.submitCode(code);
    _code.clear();
  }
}
