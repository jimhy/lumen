/// sealed `Result`——**禁 throw 穿层**。
///
/// Dart 的异常不进类型签名，调用方不看实现就不知道哪些函数会抛、抛什么。而协议层与网络层的
/// 失败是**常规路径**（对端老版本、帧解析失败、连接断开、密码错），不是异常情况：用 sealed
/// `Result` 表达能拿到「漏处理错误分支编译不过」的保护，与 Rust 侧的 `Result` 同源。
///
/// ## 什么时候仍然用异常
///
/// 只有一处：`protocol/` 内部的**致命线格式错误**（[`WireFormatException`]）。它是
/// 「这条消息根本不该长这样」，且 golden 语料里有一条用例就是要求 `decode` 抛异常
/// （`compat_open_enum_shape_change_is_fatal.json`，宽容成 `Other("42")` 反而与 Rust 不一致）。
/// 那个异常**不穿层**：net 层收到它时立刻转成 [`Err`]，不让它冒到 UI。
library;

/// 成功或失败。
sealed class Result<T, E> {
  const Result();

  /// 成功时的值，失败时为 null。
  ///
  /// **只在确实想忽略错误时用**（例如「拿不到就用默认值」），否则用 `switch` 穷尽两支。
  T? get valueOrNull => switch (this) {
        Ok<T, E>(:final T value) => value,
        Err<T, E>() => null,
      };

  /// 失败时的错误，成功时为 null。
  E? get errorOrNull => switch (this) {
        Ok<T, E>() => null,
        Err<T, E>(:final E error) => error,
      };

  bool get isOk => this is Ok<T, E>;

  /// 只映射成功值，错误原样透传。
  Result<U, E> map<U>(U Function(T) f) => switch (this) {
        Ok<T, E>(:final T value) => Ok<U, E>(f(value)),
        Err<T, E>(:final E error) => Err<U, E>(error),
      };
}

/// 成功。
final class Ok<T, E> extends Result<T, E> {
  const Ok(this.value);

  final T value;

  @override
  bool operator ==(Object other) => other is Ok<T, E> && other.value == value;

  @override
  int get hashCode => Object.hash('Ok', value);

  @override
  String toString() => 'Ok($value)';
}

/// 失败。
final class Err<T, E> extends Result<T, E> {
  const Err(this.error);

  final E error;

  @override
  bool operator ==(Object other) => other is Err<T, E> && other.error == error;

  @override
  int get hashCode => Object.hash('Err', error);

  @override
  String toString() => 'Err($error)';
}
