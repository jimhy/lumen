#!/usr/bin/env bash
# Lumen 移动端测试包装（bash 版；Windows 上的等价物是 tool/test.ps1）。
#
# 为什么需要包装：本机常驻 http 代理，`flutter test` 会**无输出卡死**——它起一条本机 socket
# 让测试宿主与被测 isolate 通信，代理变量被继承进去之后这条本机连接也去走代理，永远等不到握手。
# 解法是把代理变量从**子进程的环境**里摘掉，`env -u` 正是干这个的（不动当前 shell、不动系统设置）。
#
# ⚠ CI 上没有代理：`.github/workflows/mobile.yml` 直接跑 `flutter test`。
# 在没有代理的机器上 `env -u` 是无操作，所以本脚本在两种环境下都能跑，不存在本地/CI 分叉。
#
# 用法：
#   ./tool/test.sh                      # 跑全部测试
#   ./tool/test.sh --pub                # 先 flutter pub get（走 pub.flutter-io.cn 镜像）
#   ./tool/test.sh --analyze            # 只跑 flutter analyze --fatal-infos
#   ./tool/test.sh test/protocol/golden_corpus_path_test.dart
set -euo pipefail

# 用脚本自身位置定位包根，不依赖调用者的 cwd：语料测试按相对路径 ../crates/... 读文件。
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# 大小写各一份都要摘：Dart 的 HttpClient.findProxyFromEnvironment 同时认 http_proxy / HTTP_PROXY。
# all_proxy Dart 不认，但 flutter 工具链里的其它组件认，一并摘掉。
NOPROXY=(env -u http_proxy -u https_proxy -u all_proxy -u ftp_proxy -u no_proxy
         -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u FTP_PROXY -u NO_PROXY)

MODE=test
ARGS=()
DO_PUB=0
for a in "$@"; do
  case "$a" in
    --pub)     DO_PUB=1 ;;
    --analyze) MODE=analyze ;;
    *)         ARGS+=("$a") ;;
  esac
done

if [ "$DO_PUB" = 1 ]; then
  echo "[test.sh] flutter pub get（镜像：pub.flutter-io.cn）"
  "${NOPROXY[@]}" \
    PUB_HOSTED_URL=https://pub.flutter-io.cn \
    FLUTTER_STORAGE_BASE_URL=https://storage.flutter-io.cn \
    flutter pub get
  # ★ 归一化回写：pub 会把当前 PUB_HOSTED_URL **写进 pubspec.lock 的每一条 url**，
  # 而 CI 跑的是 `flutter pub get --enforce-lockfile` 且不设 PUB_HOSTED_URL。
  # pub 把 hosted URL 当作依赖身份的一部分 ⇒ url 不同即视为不同来源 ⇒ 锁文件对不上
  # ⇒ `Unable to satisfy pubspec.yaml using pubspec.lock` ⇒ CI 第一步就红。
  #
  # **这是本地与 CI 之间唯一真实的分叉点**，且它不报错、不改版本号，只改 URL，
  # diff 看上去像「没变化」。不归一化就必红。sha256 不用动 —— 镜像与源站归档一致。
  sed -i 's#https://pub\.flutter-io\.cn#https://pub.dev#g' pubspec.lock
  echo "[test.sh] pubspec.lock 已归一化回 pub.dev（PUB_HOSTED_URL 只许影响下载源，不许留在 lock 里）"
fi

if [ "$MODE" = analyze ]; then
  # --fatal-infos：analysis_options.yaml 里开的 strict-casts 等报的是 info 级，
  # 不加这个参数等于白开。与 mobile.yml 同参数。
  echo "[test.sh] flutter analyze --fatal-infos"
  exec "${NOPROXY[@]}" flutter analyze --fatal-infos
fi

echo "[test.sh] flutter test ${ARGS[*]:-（全部）}"
# 展开写法必须是 ${ARGS[@]+"${ARGS[@]}"} 而不是 "${ARGS[@]:-}"：
# 后者在数组为空时会展开成**一个空字符串参数**，`flutter test ''` 会报「找不到测试文件」；
# 而 set -u 下又不能直接写 "${ARGS[@]}"（空数组在旧版 bash 上算未绑定变量）。
exec "${NOPROXY[@]}" flutter test ${ARGS[@]+"${ARGS[@]}"}
