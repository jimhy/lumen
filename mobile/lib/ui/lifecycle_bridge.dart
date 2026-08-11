/// 把系统的前后台事件接到 WS 长连接与本地存档上（片 10）。
///
/// ## 它接的是两条**片 5 就写好、但一直没人调用**的钩子
///
/// `WsClient.onBackground()` / `onForeground()` 从片 5 起就在那里，测试也覆盖着，
/// 但没有任何生产代码调用它们——于是「进后台 25 秒宽限、回前台立刻重连」这条
/// 蓝图 §7.8 的硬指标在真机上**从来没有生效过**。这类「实现了但没接线」的缺口
/// 不会让任何测试变红，只会在真机上表现为「切出去一会儿回来要转很久」。
///
/// ## ★ `inactive` 不算进后台
///
/// iOS 上下拉通知中心、来电、任务切换器**都会**触发 `inactive`，而用户根本没离开。
/// 把它当成进后台的后果是宽限计时器反复起停，最坏情况下一次误触发就把连接掐了。
/// 判据只认 `paused` / `hidden` / `detached` 这三个真正「不在前台」的状态。
///
/// ## 进后台顺带落一次盘
///
/// 水位线是按秒节流写的（`kWatermarkFlushWindow`）。进后台是系统可能随时杀掉进程的
/// 时刻，也是最后一次能落盘的机会——不写的话，最多丢 1 秒的水位线推进。
/// 那个方向是安全的（偏低只会多补一段），但白白多补是可以省掉的。
library;

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/core/log.dart';
import 'package:lumen_mobile/state/conversation_session.dart';
import 'package:lumen_mobile/state/providers.dart';

const String _tag = 'lifecycle';

/// 这个状态算不算「App 不在前台了」。
///
/// ★ **`inactive` 不算**：iOS 上下拉通知中心、来电、任务切换器都会触发它，而用户
/// 根本没离开。把它当成进后台的后果是宽限计时器反复起停，最坏一次误触发就把连接掐了。
///
/// 抽成顶层函数**是为了能被直接断言**——判据留在 widget 的 switch 里时，测试只能
/// 照抄一份自己的判据表来对比，那是自说自话的绿灯（08-09 §5.2 的形态）。
bool isBackgroundState(AppLifecycleState state) => switch (state) {
      AppLifecycleState.paused ||
      AppLifecycleState.hidden ||
      AppLifecycleState.detached =>
        true,
      AppLifecycleState.resumed || AppLifecycleState.inactive => false,
    };

/// 生命周期桥。包在路由外面。
class LifecycleBridge extends ConsumerStatefulWidget {
  const LifecycleBridge({required this.child, super.key});

  final Widget child;

  @override
  ConsumerState<LifecycleBridge> createState() => _LifecycleBridgeState();
}

class _LifecycleBridgeState extends ConsumerState<LifecycleBridge>
    with WidgetsBindingObserver {
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (isBackgroundState(state)) {
      logInfo(_tag, '进入后台');
      ref.read(wsClientProvider)?.onBackground();
      _flush();
      return;
    }
    // `inactive` 走到这里但**什么都不做**——它既不是进后台（见 [isBackgroundState]），
    // 也不该触发一次重连（那会让每次下拉通知中心都 kick 一下）。
    if (state == AppLifecycleState.resumed) {
      logInfo(_tag, '回到前台');
      ref.read(wsClientProvider)?.onForeground();
    }
  }

  void _flush() {
    final ConversationSession? session =
        ref.read(conversationSessionProvider);
    if (session == null) return;
    unawaited(session.controller.flushPersistence().catchError((Object e) {
      logWarn(_tag, '进后台落盘失败（下次重连会多补一段）', error: e);
    }));
  }

  @override
  Widget build(BuildContext context) => widget.child;
}
