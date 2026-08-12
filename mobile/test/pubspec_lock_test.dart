/// `pubspec.lock` 的**纪律断言**：锁文件里的 hosted url 必须是 `pub.dev`。
///
/// # 它守的是什么
/// `flutter pub get` 会把当前 `PUB_HOSTED_URL` 写进 `pubspec.lock` 的每一条 `url`，
/// 而 `tool/test.ps1 -Pub` / `tool/test.sh --pub`（README 推荐给新人的第一条命令）
/// 为了绕开国内网络会把它设成 `https://pub.flutter-io.cn`。
///
/// CI（`.github/workflows/mobile.yml`）跑的是 `flutter pub get --enforce-lockfile`，
/// 且刻意**不设** `PUB_HOSTED_URL`。pub 把 hosted URL 当作依赖身份的一部分——url 不同
/// 即视为不同来源——于是锁文件与解析结果对不上，报
/// `Would change N dependencies.` / `Unable to satisfy pubspec.yaml using pubspec.lock`，
/// **CI 第一步就红，100% 必红**。
///
/// 这个故障最坏的地方在于它**不报错、不改版本号、只改 URL**：`git diff` 里满屏
/// `url: "..."` 变更，版本号一个没动，看上去像「没变化」，而且要等推上去跑一次 CI 才发现。
///
/// # 为什么放在 test/ 而不是 CI 的一个 step
/// 放这里，本地 `.\tool\test.ps1` 当场就红——修的人在自己机器上就能闭环，不必推一次、
/// 等一轮 CI、再改一次。两个脚本在 `pub get` 之后已经会自动归一化回写，本测试是那道
/// 自动化失效时的兜底。
///
/// # 允许的下载源 ≠ 锁文件里的 url
/// `PUB_HOSTED_URL` 该做的事是**改下载源**，不该在产物里留痕。镜像与源站的归档字节一致，
/// 所以把 url 换回 `pub.dev` 之后 `sha256` 无需改动，本机 Pub 缓存也照样命中。
library;

import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

void main() {
  test('pubspec.lock 里不得残留镜像 URL（否则 CI 的 --enforce-lockfile 必红）', () {
    // 测试的工作目录是包根（mobile/），与 tool/*.sh|ps1 里 cd 到的位置一致。
    final lock = File('pubspec.lock');
    expect(
      lock.existsSync(),
      isTrue,
      reason: 'pubspec.lock 必须入库（App 而非 library，锁定可复现构建）',
    );

    final text = lock.readAsStringSync();
    final mirrors = 'pub.flutter-io.cn'.allMatches(text).length;
    expect(
      mirrors,
      0,
      reason: '发现 $mirrors 处镜像 URL。跑一次 `.\\tool\\test.ps1 -Pub` 会自动归一化；'
          '或手动把 pubspec.lock 里的 https://pub.flutter-io.cn 全量换成 https://pub.dev '
          '（sha256 不用改）。PUB_HOSTED_URL 只影响下载源，不许留在 lock 里。',
    );

    // 正向断言：确实有 hosted 依赖且都指向 pub.dev——防止哪天 lock 被清空后本测试
    // 变成一条永远绿的空断言。
    final official = 'https://pub.dev'.allMatches(text).length;
    expect(
      official,
      greaterThan(50),
      reason: 'pubspec.lock 里的 hosted 依赖数异常（$official 条），锁文件可能被截断或清空',
    );
  });
}
