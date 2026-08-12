#!/usr/bin/env bash
# Lumen 移动端出包包装（bash 版；Windows 上的等价物是 tool/build.ps1）。
#
# ## 为什么出包也要包装（片 12 踩出来的）
#
# `tool/test.sh` 已经把「摘代理 + 把 PUB_HOSTED_URL 钉回源站」这两件事做了，但那道防线
# **只装在测试这条路上**。`flutter build` 同样会隐式跑一次 pub get —— 2026-08-12 第一次
# 出 APK 时就是这么把 118 处镜像 URL 写回 `pubspec.lock` 的，靠仓库自带的
# `test/pubspec_lock_test.dart` 才发现。
#
# 两条理由与 test.sh 完全相同，不再重复，细节见那个文件的注释：
# 1. 本机常驻 http 代理会让 flutter 工具链的部分步骤卡死；
# 2. 环境里的 PUB_HOSTED_URL 与 lock 里的 url 对不上时，pub 会**忽略 lock 重新解析整个
#    依赖图**，顺手升级一批包并把镜像地址写回 lock —— 而 CI 的
#    `flutter pub get --enforce-lockfile` 不设这个变量，第一步就红。
#
# 用法：
#   ./tool/build.sh                 # debug APK（debug 签名，装了就能用）
#   ./tool/build.sh --release       # release APK
#   ./tool/build.sh --bundle        # release AAB（Google Play）
#
# ⚠ release 需要 android/key.properties（模板见 android/key.properties.example）。
# 没有它**不会失败**，而是回退成 debug 签名并在日志里打一行「⚠ 找不到 android/key.properties」。
# 用 debug key 签的包不能上架、也不能覆盖安装正式版 —— 出正式包前先在日志里确认没有那一行。
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

NOPROXY=(env -u http_proxy -u https_proxy -u all_proxy -u ftp_proxy -u no_proxy
         -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u FTP_PROXY -u NO_PROXY)
PUB_SOURCE=(PUB_HOSTED_URL=https://pub.dev)

TARGET=apk
MODE=--debug
for a in "$@"; do
  case "$a" in
    --release) MODE=--release ;;
    --debug)   MODE=--debug ;;
    --bundle)  TARGET=appbundle; MODE=--release ;;
    *)         echo "未知参数：$a" >&2; exit 2 ;;
  esac
done

# ── 签名降级的「有声」执行者（协作铁律 6）─────────────────────────────────────
#
# `build.gradle.kts` 里那条 `logger.warn` **在 `flutter build` 下看不见**（实测：flutter
# 过滤 gradle 输出）。所以这件事必须在脚本层喊——而且要喊两遍：开头一遍容易被后面几百行
# 构建日志淹掉，结尾那遍才是用户真正会看到的。
UNSIGNED=0
if [ "$MODE" = --release ] && [ ! -f android/key.properties ]; then
  UNSIGNED=1
  echo "" >&2
  echo "  ⚠ 找不到 android/key.properties —— 这个 release 包会用 **debug 密钥** 签名。" >&2
  echo "    它能装能跑，但【不能上架、也不能覆盖安装正式版】。" >&2
  echo "    正式出包请按 android/key.properties.example 放好密钥。" >&2
  echo "" >&2
fi

echo "[build.sh] flutter build $TARGET $MODE"
"${NOPROXY[@]}" "${PUB_SOURCE[@]}" flutter build "$TARGET" "$MODE"

# 兜底核对：即使上面钉了源站，也再看一眼 lock —— 这条检查的成本是零，
# 而漏掉它的代价是 CI 第一步就红，且 diff 看上去像「没变化」（只有 url 变了）。
if grep -q 'pub\.flutter-io\.cn' pubspec.lock; then
  echo "[build.sh] ⚠ pubspec.lock 里出现了镜像 URL，正在归一化回 pub.dev" >&2
  sed -i 's#https://pub\.flutter-io\.cn#https://pub.dev#g' pubspec.lock
  echo "[build.sh] 已归一化。请把这次改动一并提交。" >&2
fi

# 结尾再喊一遍。这一遍才是真正会被看到的那一遍。
if [ "$UNSIGNED" = 1 ]; then
  echo "" >&2
  echo "  ⚠⚠ 提醒：刚出的这个 release 包是用 debug 密钥签的，不能上架、不能覆盖安装正式版。" >&2
  echo "" >&2
fi
