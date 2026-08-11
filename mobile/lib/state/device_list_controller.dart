/// 设备列表状态。
///
/// ## ⚠ 不要重排
///
/// 服务端按**注册时间升序**返回，这是一次修复的结果：老实现按 `last_seen DESC`，
/// 而 `last_seen` 每次心跳都变，导致列表每刷新一次就重排、设备跳位（海风哥反馈）。
/// 在客户端「把在线的排前面」会把这条修复原样重新引入——在线状态同样每次心跳都可能变。
///
/// 要突出在线设备，用**样式**（图标、灰度），不要用顺序。
///
/// ## 刷新时机
///
/// 服务端的在线判据是 `last_seen` 落在在线窗口内，而 `last_seen` 由对方设备的心跳维持。
/// 也就是说这份列表**天然是滞后的**：一台 PC 刚断电，这里还会显示在线约一个窗口的时间。
/// 手动下拉刷新解决不了这件事，UI 不必假装它能。真正的判据是发起之后的
/// `ControlDenied{Offline}`。
library;

import 'dart:async';

import 'package:lumen_mobile/core/result.dart';
import 'package:lumen_mobile/net/net_error.dart';
import 'package:lumen_mobile/net/rest_client.dart';
import 'package:lumen_mobile/protocol/rest_dto.dart';

/// 设备列表的三态。
sealed class DeviceListState {
  const DeviceListState();
}

/// 正在拉取（含首次与刷新）。
final class DeviceListLoading extends DeviceListState {
  const DeviceListLoading();

  @override
  String toString() => 'DeviceListLoading';
}

/// 拉到了。
final class DeviceListLoaded extends DeviceListState {
  const DeviceListLoaded(this.devices);

  /// **保持服务端顺序**（注册时间升序）。
  final List<DeviceRecord> devices;

  /// 可发起连接的设备：在线、且不是本机（服务端会回 `SelfControl`）。
  ///
  /// 这是**过滤**不是排序——UI 用它决定按钮能不能点，列表本身仍然全量按原序显示，
  /// 否则用户会以为自己的设备丢了。
  List<DeviceRecord> get connectable => devices
      .where((DeviceRecord d) => d.online && !d.isSelf)
      .toList(growable: false);

  @override
  String toString() => 'DeviceListLoaded(${devices.length} 台)';
}

/// 拉取失败。
final class DeviceListError extends DeviceListState {
  const DeviceListError(this.error);

  final NetError error;

  @override
  String toString() => 'DeviceListError($error)';
}

/// 拉取并持有设备列表。
final class DeviceListController {
  DeviceListController(this._rest);

  final RestClient _rest;

  final StreamController<DeviceListState> _states =
      StreamController<DeviceListState>.broadcast();

  DeviceListState _state = const DeviceListLoading();
  bool _inFlight = false;

  Stream<DeviceListState> get states => _states.stream;

  DeviceListState get state => _state;

  /// 拉一次。
  ///
  /// 并发调用会被合并成一次：下拉刷新在弱网下很容易被连点，而每一次都是一个真实请求，
  /// 而服务端**零背压**（R11/R24）——客户端侧的自我节制是这套系统里唯一的那道闸。
  Future<void> refresh() async {
    if (_inFlight) return;
    _inFlight = true;
    _emit(const DeviceListLoading());
    try {
      final Result<DeviceListResponse, NetError> r = await _rest.listDevices();
      _emit(switch (r) {
        Ok<DeviceListResponse, NetError>(:final DeviceListResponse value) =>
          DeviceListLoaded(value.devices),
        Err<DeviceListResponse, NetError>(:final NetError error) =>
          DeviceListError(error),
      });
    } finally {
      _inFlight = false;
    }
  }

  Future<void> dispose() => _states.close();

  void _emit(DeviceListState next) {
    _state = next;
    if (!_states.isClosed) _states.add(next);
  }
}
