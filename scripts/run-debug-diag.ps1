<#
.SYNOPSIS
    用正式版的登录态运行 debug 版 Lumen，抓 P2P 直连诊断日志。

.DESCRIPTION
    直接双击 debug 版 exe 是**抓不到远程日志**的：debug 构建会把数据目录隔离到
    %LOCALAPPDATA%\Lumen-dev\（见 crates/lumen-app/src/paths.rs 的「debug 自动隔离」约定），
    那是一份全新的空配置——启动后是「未登录」状态，连不上服务器，也就走不到 P2P 那一步。

    本脚本用逃生口 LUMEN_DATA_DIR 把数据目录指回正式版的 %LOCALAPPDATA%\Lumen，
    于是登录态、device_id、配对信任、设备列表全部照旧，远程会话可以直接建。

    代价：debug 版与正式版**不能同时跑**——同一份 settings.json / sessions.json 会互相覆盖，
    而且服务端见到同一个 device_id 会把先连的那条 WS 顶替掉。脚本会先检查并要求关掉正式版。

    日志同时输出到控制台窗口和 %LOCALAPPDATA%\Lumen\lumen.log（超 8 MiB 轮转为 lumen.log.old）。

.PARAMETER Exe
    debug 版 lumen.exe 路径。默认取本仓库的 target\debug\lumen.exe。
    拷到另一台机器用时，把 exe 和本脚本一起拷过去，再用 -Exe 指定路径。

.PARAMETER Force
    检测到 Lumen 正在运行时自动结束它，而不是报错退出。

.PARAMETER LogFilter
    RUST_LOG 过滤级别。默认 info（已足够看全 P2P 打洞链路）。
    要更细可以传 "info,lumen_app::p2p=debug"。

.EXAMPLE
    .\scripts\run-debug-diag.ps1

.EXAMPLE
    .\scripts\run-debug-diag.ps1 -Exe D:\lumen-debug\lumen.exe -Force
#>
[CmdletBinding()]
param(
    [string]$Exe,
    [switch]$Force,
    [string]$LogFilter = "info"
)

$ErrorActionPreference = "Stop"

# 默认取仓库内的 debug 产物（脚本在 <仓库>\scripts\ 下，故上翻一层）。
if (-not $Exe) {
    $repo = Split-Path -Parent $PSScriptRoot
    $Exe = Join-Path $repo "target\debug\lumen.exe"
}

if (-not (Test-Path $Exe)) {
    Write-Error "找不到 debug 版 exe：$Exe`n先编译：cargo build -p lumen-app"
}

$dataDir = Join-Path $env:LOCALAPPDATA "Lumen"
if (-not (Test-Path $dataDir)) {
    Write-Error "正式版数据目录不存在：$dataDir`n这台机器还没装过/登录过正式版 Lumen，请先装正式版并登录一次。"
}

# 同一数据目录 + 同一 device_id 不能两个实例并存，先清场。
$running = Get-Process -Name "lumen" -ErrorAction SilentlyContinue
if ($running) {
    if ($Force) {
        Write-Host "发现 Lumen 正在运行（PID $($running.Id -join ', ')），按 -Force 结束它…" -ForegroundColor Yellow
        $running | Stop-Process -Force
        Start-Sleep -Seconds 2
    }
    else {
        Write-Error "Lumen 正在运行（PID $($running.Id -join ', ')）。请先退出正式版，或加 -Force 让脚本替你关掉。`n原因：debug 版要复用同一份数据目录和 device_id，两个实例并存会互相覆盖配置、并在服务端互相顶替 WS 连接。"
    }
}

$logPath = Join-Path $dataDir "lumen.log"

Write-Host ""
Write-Host "启动 debug 版 Lumen（诊断模式）" -ForegroundColor Cyan
Write-Host "  exe      : $Exe"
Write-Host "  数据目录 : $dataDir  （复用正式版登录态）"
Write-Host "  日志文件 : $logPath"
Write-Host "  RUST_LOG : $LogFilter"
Write-Host ""
Write-Host "接下来：建一次远程会话，然后把上面那个日志文件发出来。" -ForegroundColor Green
# 这个脚本的整个用途就是「把 lumen.log 发我看看」，那就得在这里把代价说清楚 ——
# 清单与 crates/lumen-app/src/applog.rs 的「这份文件里有什么个人信息」一节同源。
Write-Host "  ⚠ 发出去之前请知悉：日志里含本端与对端的公网 IP:端口、本机 LAN IP、" -ForegroundColor Yellow
Write-Host "    登录用的显示名与邮箱，以及本机路径（Windows 用户名常是真名）。" -ForegroundColor Yellow
Write-Host "    不含证书私钥、鉴权 token、你敲的任何字符与终端输出。" -ForegroundColor Yellow
Write-Host "关键行 —— 搜 'P2P' 即可：" -ForegroundColor Green
Write-Host "  P2P 引擎启动 / P2P 未启动             ← 引擎起没起"
Write-Host "  P2P STUN 探测无应答                   ← 本端没有公网候选，跨网必挂"
Write-Host "  P2P STUN 反射回不可路由地址           ← 自托管同机部署会踩，等于也没有公网候选"
Write-Host "  P2P 本端候选端点                      ← 本端发给对端的候选"
Write-Host "  P2P 开始打洞：…对端候选 […]           ← 对端候选里有没有公网地址（能断定问题在哪端）"
Write-Host "  P2P connect <地址> 未建立: <原因>     ← 逐候选的失败原因"
Write-Host "  P2P 打洞成功 / P2P 打洞失败           ← 结论（失败行带「拒绝连入 N 次」）"
Write-Host ""
Write-Host "特别留意这个组合 —— 它坐实一个已知的设计缺陷：" -ForegroundColor Yellow
Write-Host "  同时出现「accept 来自 X 的连入握手成功」和「P2P 打洞失败」"
Write-Host "  = 直连其实已经打通了，但因为方向与角色不匹配被两端各自丢弃。"
Write-Host ""

$env:LUMEN_DATA_DIR = $dataDir
$env:RUST_LOG = $LogFilter

# 前台运行：debug 构建没有 windows_subsystem="windows"，日志会直接打在本控制台窗口里，
# 同时由 applog 落到 lumen.log。关掉窗口即结束进程。
& $Exe
