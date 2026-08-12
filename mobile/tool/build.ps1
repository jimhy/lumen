<#
.SYNOPSIS
    出 Lumen 移动端的 Android 包，并绕开本机常驻 http 代理与 PUB_HOSTED_URL 这两个坑。

.DESCRIPTION
    `tool/test.ps1` 已经把「清代理 + 把 PUB_HOSTED_URL 钉回源站」做了，但那道防线
    **只装在测试这条路上**。`flutter build` 同样会隐式跑一次 pub get —— 2026-08-12
    第一次出 APK 时就是这么把 118 处镜像 URL 写回 `pubspec.lock` 的，靠仓库自带的
    `test/pubspec_lock_test.dart` 才发现。

    两条理由与 test.ps1 完全相同，细节见那个文件的注释：

      1. 本机常驻代理会让 flutter 工具链的部分步骤卡死；
      2. 环境里的 PUB_HOSTED_URL 与 lock 里的 url 对不上时，pub 会**忽略 lock 重新解析
         整个依赖图**，顺手升一批包并把镜像地址写回 lock —— 而 CI 的
         `flutter pub get --enforce-lockfile` 不设这个变量，第一步就红。

    ⚠ release 需要 `android/key.properties`（模板见 `android/key.properties.example`）。
    没有它**不会失败**，而是回退成 debug 签名并在日志里打一行
    「⚠ 找不到 android/key.properties」。用 debug key 签的包不能上架、也不能覆盖安装
    正式版 —— 出正式包前先在日志里确认没有那一行。

.PARAMETER Release
    出 release 包（默认是 debug）。

.PARAMETER Bundle
    出 release AAB（Google Play 用），隐含 -Release。

.EXAMPLE
    .\tool\build.ps1
    .\tool\build.ps1 -Release
    .\tool\build.ps1 -Bundle
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Bundle
)

$ErrorActionPreference = 'Stop'

$repoMobile = Split-Path -Parent $PSScriptRoot
Push-Location $repoMobile
try {
    # 清代理：只改本进程的环境块，子进程继承改过的副本，不写注册表、不影响其它终端。
    $proxyVars = @(
        'http_proxy', 'https_proxy', 'all_proxy', 'ftp_proxy', 'no_proxy',
        'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'FTP_PROXY', 'NO_PROXY'
    )
    $cleared = @()
    foreach ($v in $proxyVars) {
        if (Test-Path "Env:\$v") {
            $cleared += $v
            Remove-Item "Env:\$v"
        }
    }
    if ($cleared.Count -gt 0) {
        Write-Host "[build.ps1] 已在本进程清除代理变量：$($cleared -join ', ')" -ForegroundColor DarkGray
    }

    # ★ 钉回源站。理由见 test.ps1 的「坑 4」——那里讲的是 analyze/test，这里是 build，
    # 机制完全一样：都会隐式 pub get。
    $env:PUB_HOSTED_URL = 'https://pub.dev'

    $target = if ($Bundle) { 'appbundle' } else { 'apk' }
    $mode = if ($Bundle -or $Release) { '--release' } else { '--debug' }

    # ── 签名降级的「有声」执行者（协作铁律 6）─────────────────────────────────
    #
    # build.gradle.kts 里那条 logger.warn **在 `flutter build` 下看不见**（实测：flutter
    # 过滤 gradle 输出）。所以这件事必须在脚本层喊，而且要喊两遍：开头那遍会被后面几百行
    # 构建日志淹掉，结尾那遍才是用户真正会看到的。
    $unsigned = ($mode -eq '--release') -and
                -not (Test-Path (Join-Path $repoMobile 'android/key.properties'))
    if ($unsigned) {
        Write-Host ''
        Write-Host '  ⚠ 找不到 android/key.properties —— 这个 release 包会用 **debug 密钥** 签名。' -ForegroundColor Yellow
        Write-Host '    它能装能跑，但【不能上架、也不能覆盖安装正式版】。' -ForegroundColor Yellow
        Write-Host '    正式出包请按 android/key.properties.example 放好密钥。' -ForegroundColor Yellow
        Write-Host ''
    }

    Write-Host "[build.ps1] flutter build $target $mode" -ForegroundColor Cyan
    & flutter build $target $mode
    $buildExit = $LASTEXITCODE

    # 兜底核对：即使上面钉了源站也再看一眼。成本是零，漏掉的代价是 CI 第一步就红，
    # 而 diff 看上去像「没变化」（只有 url 变了）。
    $lock = Join-Path $repoMobile 'pubspec.lock'
    $text = Get-Content $lock -Raw
    if ($text -match 'pub\.flutter-io\.cn') {
        Write-Host '[build.ps1] ⚠ pubspec.lock 里出现了镜像 URL，正在归一化回 pub.dev' -ForegroundColor Yellow
        ($text -replace 'https://pub\.flutter-io\.cn', 'https://pub.dev') |
            Set-Content -NoNewline -Encoding utf8 $lock
        Write-Host '[build.ps1] 已归一化。请把这次改动一并提交。' -ForegroundColor Yellow
    }

    # 结尾再喊一遍。这一遍才是真正会被看到的那一遍。
    if ($unsigned) {
        Write-Host ''
        Write-Host '  ⚠⚠ 提醒：刚出的这个 release 包是用 debug 密钥签的，不能上架、不能覆盖安装正式版。' -ForegroundColor Yellow
        Write-Host ''
    }

    exit $buildExit
}
finally {
    Pop-Location
}
