<#
.SYNOPSIS
    协议对齐守卫的本地一键跑：先跑 Rust 侧 golden，再跑 Dart 侧 golden。

.DESCRIPTION
    `crates/lumen-protocol/tests/golden/mobile/*.json` 是 Rust 与 Dart **两端共用的唯一事实源**。
    两条断言合起来才把线格式钉死：

      - Rust 侧 `cargo test -p lumen-protocol --test mobile_golden`：
        断言「语料 ⇄ `LlmFrame`」一致。改变体名 / 加必填字段 → 语料对不上 → 红。
      - Dart 侧 `flutter test test/protocol/`：
        断言「语料 ⇄ Dart 模型」一致。Dart 少实现一个变体 → 红。

    **只跑一边等于没做**（README 原话）。所以本脚本按顺序跑两边，任一失败即停：
    Rust 侧红时先修 Rust，此时跑 Dart 侧只会得到一堆同源的误导性失败。

    ⚠ 现状（M7 片 0b）：Dart 侧只有 `golden_corpus_path_test.dart`（语料可达性 + 信封形状），
    真正的「语料 ⇄ Dart 模型」断言是**片 1-Dart** 的 `golden_test.dart`，尚未落地。
    在它落地之前，本脚本的 Dart 半边只能证明语料还在原地——**不要因为它绿就以为两端对齐了**。

    ⚠ cargo 很贵：lumen-app 是超大 crate。本脚本刻意用 `-p lumen-protocol --test mobile_golden`
    精确到单个测试目标，不跑 `--workspace`。

.PARAMETER SkipRust
    只跑 Dart 半边（刚跑过 Rust 侧、只在调 Dart 模型时用）。

.EXAMPLE
    .\tool\gen_golden_check.ps1
    .\tool\gen_golden_check.ps1 -SkipRust
#>
[CmdletBinding()]
param([switch]$SkipRust)

$ErrorActionPreference = 'Stop'

$mobileDir = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $mobileDir

if (-not $SkipRust) {
    Write-Host '[golden] Rust 侧：cargo test -p lumen-protocol --test mobile_golden' -ForegroundColor Cyan
    Push-Location $repoRoot
    try {
        & cargo test -p lumen-protocol --test mobile_golden
        if ($LASTEXITCODE -ne 0) {
            Write-Host '[golden] Rust 侧红了。先修 Rust——此时跑 Dart 侧只会得到一堆同源的误导性失败。' -ForegroundColor Red
            Write-Host '[golden] 详细输出：cargo test -p lumen-protocol --test mobile_golden -- --nocapture' -ForegroundColor Yellow
            Write-Host '[golden] 注意：语料**没有** UPDATE_GOLDEN=1 自动重写，reserialized 要手工粘贴（见语料 README §6）。' -ForegroundColor Yellow
            exit $LASTEXITCODE
        }
    }
    finally { Pop-Location }
}

Write-Host '[golden] Dart 侧：flutter test test/protocol/（经 tool/test.ps1 绕代理）' -ForegroundColor Cyan
& (Join-Path $PSScriptRoot 'test.ps1') 'test/protocol/'
exit $LASTEXITCODE
