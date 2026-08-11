/// 设备列表页：选一台 PC 发起隐藏会话。
///
/// ## ⚠ 不按在线状态重排
///
/// 服务端按**注册时间升序**返回，这是一次修复的结果：老实现按 `last_seen DESC`，
/// 而 `last_seen` 每次心跳都变，导致列表每刷新一次就重排、设备跳位。在客户端「把在线的
/// 排前面」会把那条修复原样重新引入——在线状态同样每次心跳都可能变。
///
/// 要突出在线设备，用**样式**（图标、灰度），不用顺序。离线设备也**照常列出**、
/// 只是不能点：删掉它们会让用户以为设备丢了。
///
/// ## 在线状态天然滞后
///
/// 服务端的判据是 `last_seen` 落在在线窗口内，而 `last_seen` 由对方设备的心跳维持。
/// 一台 PC 刚断电，这里还会显示在线约一个窗口的时间。下拉刷新解决不了这件事，
/// 所以这一页不假装它能——真正的判据是发起之后的 `ControlDenied{Offline}`。
library;

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:lumen_mobile/protocol/remote_s2c.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';
import 'package:lumen_mobile/state/device_list_controller.dart';
import 'package:lumen_mobile/state/link_controller.dart';
import 'package:lumen_mobile/state/providers.dart';
import 'package:lumen_mobile/ui/link_messages.dart';

/// 设备列表页。
class DevicesPage extends ConsumerStatefulWidget {
  const DevicesPage({super.key});

  @override
  ConsumerState<DevicesPage> createState() => _DevicesPageState();
}

class _DevicesPageState extends ConsumerState<DevicesPage> {
  @override
  void initState() {
    super.initState();
    // 首帧之后再拉：provider 在 build 期间读写会触发 Riverpod 的重入断言。
    WidgetsBinding.instance.addPostFrameCallback((_) {
      ref.read(deviceListControllerProvider)?.refresh();
      ref.read(wsClientProvider)?.start();
    });
  }

  @override
  Widget build(BuildContext context) {
    final DeviceListController? devices =
        ref.watch(deviceListControllerProvider);
    final LinkController? link = ref.watch(linkControllerProvider);
    if (devices == null || link == null) {
      return const Scaffold(body: Center(child: Text('尚未选定服务器')));
    }
    return Scaffold(
      appBar: AppBar(
        title: const Text('我的设备'),
        actions: <Widget>[
          IconButton(
            onPressed: devices.refresh,
            icon: const Icon(Icons.refresh),
            tooltip: '刷新',
          ),
        ],
      ),
      body: Column(
        children: <Widget>[
          _linkBanner(context, link),
          Expanded(child: _list(context, devices, link)),
        ],
      ),
    );
  }

  /// 顶部状态条：发起中 / 被拒 / 失败 / 已连接。
  ///
  /// 拒绝与失败**必须看得见**（§14-6 无声降级禁令）——它们是用户唯一能看到的
  /// 「为什么没连上」。
  Widget _linkBanner(BuildContext context, LinkController link) {
    return StreamBuilder<LinkState>(
      stream: link.states,
      initialData: link.state,
      builder: (BuildContext context, AsyncSnapshot<LinkState> snapshot) {
        final LinkState state = snapshot.data ?? const LinkIdle();
        final ColorScheme scheme = Theme.of(context).colorScheme;
        return switch (state) {
          LinkIdle() || LinkPairing() => const SizedBox.shrink(),
          LinkRequesting() => const LinearProgressIndicator(),
          LinkActive(:final String peerName) => MaterialBanner(
              content: Text('已连接到 $peerName'),
              actions: <Widget>[
                TextButton(onPressed: link.close, child: const Text('断开')),
              ],
            ),
          LinkDenied(:final DenyReason reason) => _errorBanner(
              context,
              scheme,
              link,
              denyReasonMessage(reason),
            ),
          LinkFailed(:final LinkFailure reason) => _errorBanner(
              context,
              scheme,
              link,
              linkFailureMessage(reason),
            ),
        };
      },
    );
  }

  Widget _errorBanner(
    BuildContext context,
    ColorScheme scheme,
    LinkController link,
    String message,
  ) {
    return MaterialBanner(
      backgroundColor: scheme.errorContainer,
      content: Text(message, style: TextStyle(color: scheme.onErrorContainer)),
      actions: <Widget>[
        TextButton(onPressed: link.dismissError, child: const Text('知道了')),
      ],
    );
  }

  Widget _list(
    BuildContext context,
    DeviceListController devices,
    LinkController link,
  ) {
    return StreamBuilder<DeviceListState>(
      stream: devices.states,
      initialData: devices.state,
      builder: (BuildContext context, AsyncSnapshot<DeviceListState> snapshot) {
        final DeviceListState state = snapshot.data ?? const DeviceListLoading();
        return switch (state) {
          DeviceListLoading() =>
            const Center(child: CircularProgressIndicator()),
          DeviceListError(:final error) => Center(
              child: Padding(
                padding: const EdgeInsets.all(24),
                child: Column(
                  mainAxisSize: MainAxisSize.min,
                  children: <Widget>[
                    Text(error.userMessage, textAlign: TextAlign.center),
                    const SizedBox(height: 12),
                    OutlinedButton(
                      onPressed: devices.refresh,
                      child: const Text('重试'),
                    ),
                  ],
                ),
              ),
            ),
          DeviceListLoaded(:final List<DeviceRecord> devices) =>
            RefreshIndicator(
              onRefresh: () async =>
                  ref.read(deviceListControllerProvider)?.refresh(),
              child: ListView.builder(
                itemCount: devices.length,
                itemBuilder: (BuildContext context, int index) =>
                    _tile(context, devices[index], link),
              ),
            ),
        };
      },
    );
  }

  Widget _tile(BuildContext context, DeviceRecord device, LinkController link) {
    // 本机不能控制自己（服务端回 SelfControl），离线的连不上——两种都**只禁用不隐藏**。
    final bool connectable = device.online && !device.isSelf;
    final String subtitle = <String>[
      device.os,
      device.appVersion,
      if (device.isSelf) '本机' else if (!device.online) '离线',
    ].join(' · ');
    return ListTile(
      leading: Icon(
        device.isSelf ? Icons.smartphone : Icons.computer,
        color: device.online
            ? Theme.of(context).colorScheme.primary
            : Theme.of(context).disabledColor,
      ),
      title: Text(device.name),
      subtitle: Text(subtitle),
      enabled: connectable,
      onTap: connectable ? () => link.open(device.id) : null,
    );
  }
}
