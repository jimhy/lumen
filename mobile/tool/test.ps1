<#
.SYNOPSIS
    跑 Lumen 移动端的 flutter test，并绕开本机常驻 http 代理造成的卡死。

.DESCRIPTION
    本机（作者的 Windows 开发机）有一个常驻 http 代理（`all_proxy=http://127.0.0.1:7897`）。
    它对 flutter 工具链有三个已知影响，本脚本包掉其中两个：

      1. **flutter test 会卡死**（无输出、不超时、Ctrl-C 才停）。
         `flutter test` 起一条本机 socket 让测试宿主与被测 isolate 通信，代理环境变量
         被继承进去之后这条**本机**连接也去走代理，于是永远等不到握手。
         ⇒ 本脚本在**当前进程**里删掉全部代理变量再拉起 flutter（只影响本进程与其子进程，
            不动系统设置、不动用户环境变量）。
      2. **pub 要走镜像**：`flutter pub get` 直连 pub.dev 会卡。见 -Pub 开关。
      3. curl 打本机服务要 `--noproxy "*"`（与本脚本无关，记在 README 里）。

    ⚠ **CI 上没有代理，也没有 PowerShell 的这套包装**：`.github/workflows/mobile.yml`
    直接跑 `flutter test`。两边都成立是刻意的——本脚本删的是「本来就不该存在的变量」，
    在没有代理的机器上删它们是无操作，所以不存在「本地能过 CI 不能过」的分叉。

.PARAMETER Pub
    跑 flutter test 之前先跑一次 `flutter pub get`（走 pub.flutter-io.cn 镜像）。
    第一次 clone 仓库、或改过 pubspec.yaml 之后需要。

.PARAMETER Analyze
    只跑 `flutter analyze --fatal-infos`，不跑测试。与 CI 的第一道门同参数。

.PARAMETER Coverage
    带 --coverage 跑，产物落 mobile/coverage/（已被 .gitignore 挡住）。

.EXAMPLE
    .\tool\test.ps1
    .\tool\test.ps1 -Pub
    .\tool\test.ps1 -Analyze
    .\tool\test.ps1 test/protocol/golden_corpus_path_test.dart
#>
[CmdletBinding()]
param(
    [switch]$Pub,
    [switch]$Analyze,
    [switch]$Coverage,
    # 透传给 flutter test 的其余参数（测试文件路径、--name 过滤等）。
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'

# 脚本在 mobile/tool/ 下，包根是它的上一级。用脚本自身位置定位而不是依赖调用者的 cwd：
# 语料测试按相对路径 ../crates/... 读文件，工作目录错了会报「找不到语料目录」。
$repoMobile = Split-Path -Parent $PSScriptRoot
Push-Location $repoMobile
try {
    # ── 坑 1：清掉全部代理变量 ────────────────────────────────────────────────
    # 大小写各一份都要删：Dart 的 HttpClient.findProxyFromEnvironment 同时认
    # http_proxy / HTTP_PROXY；all_proxy 虽然 Dart 不认，但 flutter 工具链里的
    # 其它组件（gradle / curl 风格的下载器）认，一并清掉省事。
    # Remove-Item Env:\ 只改本 PowerShell 进程的环境块，子进程继承的是改过的副本，
    # **不写注册表、不影响其它终端**。
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
        Write-Host "[test.ps1] 已在本进程清除代理变量：$($cleared -join ', ')" -ForegroundColor DarkGray
    }

    if ($Pub) {
        # ── 坑 2：pub 走镜像 ─────────────────────────────────────────────────
        # 只在本进程设，不污染用户环境；FLUTTER_STORAGE_BASE_URL 是给 flutter 自身
        # 下载引擎产物用的，与 pub 包是两个源，缺哪个都会在对应步骤上卡。
        $env:PUB_HOSTED_URL = 'https://pub.flutter-io.cn'
        $env:FLUTTER_STORAGE_BASE_URL = 'https://storage.flutter-io.cn'
        Write-Host '[test.ps1] flutter pub get（镜像：pub.flutter-io.cn）' -ForegroundColor Cyan
        & flutter pub get
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

        # ── ★ 归一化回写：把 lock 里的 hosted url 改回 pub.dev ────────────────
        # pub 会把当前 PUB_HOSTED_URL **写进 pubspec.lock 的每一条 url**，而 CI 跑的是
        # `flutter pub get --enforce-lockfile` 且刻意不设 PUB_HOSTED_URL。pub 把 hosted
        # URL 当作依赖身份的一部分 ⇒ url 不同即视为不同来源 ⇒ 锁文件与解析结果对不上 ⇒
        # `Unable to satisfy pubspec.yaml using pubspec.lock` ⇒ mobile.yml 第一步就红。
        #
        # **这是本地与 CI 之间唯一真实的分叉点**：它不报错、不改版本号、只改 URL，
        # diff 看上去像「没变化」。不归一化就必红，而且要等推上去才发现。
        # sha256 不用动——镜像与源站的归档字节一致。
        # 另有 test/pubspec_lock_test.dart 把这条纪律钉成断言（本地跑测试就会红）。
        $lock = Join-Path $repoMobile 'pubspec.lock'
        (Get-Content $lock -Raw) -replace 'https://pub\.flutter-io\.cn', 'https://pub.dev' |
            Set-Content -NoNewline -Encoding utf8 $lock
        Write-Host '[test.ps1] pubspec.lock 已归一化回 pub.dev（PUB_HOSTED_URL 只许影响下载源，不许留在 lock 里）' -ForegroundColor DarkGray
    }

    if ($Analyze) {
        # --fatal-infos：与 mobile.yml 同参数。analysis_options.yaml 里开的
        # strict-casts / strict-inference / strict-raw-types 报的是 info 级，
        # 不加这个参数等于白开。
        Write-Host '[test.ps1] flutter analyze --fatal-infos' -ForegroundColor Cyan
        & flutter analyze --fatal-infos
        exit $LASTEXITCODE
    }

    # 变量名刻意不叫 $args：那是 PowerShell 的自动变量（脚本未绑定参数），
    # 覆盖它能跑但语义混乱，读的人会以为在改调用方传进来的东西。
    $flutterArgs = @('test')
    if ($Coverage) { $flutterArgs += '--coverage' }
    if ($Rest) { $flutterArgs += $Rest }

    Write-Host "[test.ps1] flutter $($flutterArgs -join ' ')" -ForegroundColor Cyan
    & flutter @flutterArgs
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
