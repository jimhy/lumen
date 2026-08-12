/// 统一日志出口。
///
/// ## 纪律：降级路径必须 `log` **并且** UI 可见
///
/// §14-6 是「无声降级禁令」，不是「无日志降级禁令」——**只 log 不给 UI 不算数**，
/// 用户在手机上看不到日志。这个文件只保证「有地方可 log」，另一半（UI 可见）
/// 由各 controller 的错误态负责。
///
/// ## 为什么可替换 sink
///
/// 单测要断言「这条降级路径真的报了」。把出口做成可替换的，测试里换成收集器即可，
/// 不必去截 stdout（那在 `flutter test` 的并行 isolate 下并不可靠）。
library;

import 'dart:developer' as developer;

/// 日志级别。
enum LogLevel {
  /// 排障细节，发布包里也保留（手机上没有 attach 调试器的机会）。
  debug(500, 'DEBUG'),

  /// 正常的关键节点：连上了、会话建立了、登出了。
  info(800, 'INFO'),

  /// **降级发生了**：某条消息没认出来、重连了、载荷解析失败。
  warn(900, 'WARN'),

  /// 本端逻辑错误或不可恢复的失败。
  error(1000, 'ERROR');

  const LogLevel(this.severity, this.label);

  /// `dart:developer` 的级别数值。
  final int severity;
  final String label;
}

/// 日志出口签名。
typedef LogSink = void Function(
  LogLevel level,
  String tag,
  String message, {
  Object? error,
});

void _developerSink(
  LogLevel level,
  String tag,
  String message, {
  Object? error,
}) {
  developer.log(
    message,
    name: 'lumen.$tag',
    level: level.severity,
    error: error,
  );
}

LogSink _sink = _developerSink;

/// 换掉日志出口（单测用）。返回旧的，便于 `addTearDown` 还原。
LogSink setLogSink(LogSink sink) {
  final LogSink old = _sink;
  _sink = sink;
  return old;
}

/// 排障细节。
void logDebug(String tag, String message) =>
    _sink(LogLevel.debug, tag, message);

/// 关键节点。
void logInfo(String tag, String message) => _sink(LogLevel.info, tag, message);

/// **降级发生了**。凡是走到这里的路径，都该问一句「用户在界面上看得到吗」。
void logWarn(String tag, String message, {Object? error}) =>
    _sink(LogLevel.warn, tag, message, error: error);

/// 本端逻辑错误。
void logError(String tag, String message, {Object? error}) =>
    _sink(LogLevel.error, tag, message, error: error);
