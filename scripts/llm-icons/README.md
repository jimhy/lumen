# 内置 LLM CLI 会话图标的生成链路

`icons/llm/*.png` 由这里的脚本从各家官方品牌 SVG 生成，**不要手改 PNG**——
要调整外观（配色、留白、圆角、新增一家 CLI），改脚本后重新生成。

## 为什么要内置这些图标

Lumen 的会话图标规则是「取会话内前台运行程序的 exe 关联图标」。这条规则对
LLM CLI 一个都不成立（2026-08-17 在 Windows 上逐个实跑实测）：

| CLI | 前台真实进程（shell 的直接子进程） | 抽到的图标 |
| --- | --- | --- |
| Claude Code | `node.exe`（npm 的 `claude.ps1` shim） | Node.js 图标 |
| Codex CLI | `node.exe`（同上） | Node.js 图标 |
| Gemini CLI | `node.exe`（同上） | Node.js 图标 |
| Kimi CLI | `kimi.exe`（Node 打包的原生二进制；经 `uvx` 安装则是 `python.exe`） | Node.js 图标 |

四个 CLI 顶着同一个 Node 绿六边形，彼此还分不开。Codex 更绕：真正的
`codex.exe` 是 `node.exe` 的**孙**进程（`pwsh → node.exe → codex.exe`），而取图标
只查直接子进程，够不到它；就算够得到，实测 `codex.exe` 也没嵌资源图标，抽出来
是 Windows 通用程序占位方块。

所以识别到受支持的 LLM CLI 时改用内置品牌图，见 `crates/lumen-app/src/llm_icon.rs`。

## 生成

需要 Python 3 + `pillow` + `numpy`：

```bash
cd scripts/llm-icons
python make_llm_icons.py ../../icons/llm 32
python make_llm_icons.py ../../icons/llm 64
```

`make_llm_icons.py` 默认按 `<名字>.png` 输出，两档尺寸需按
`<名字>-32.png` / `<名字>-64.png` 重命名后放进 `icons/llm/`
（Rust 侧用 `include_bytes!` 按这个命名嵌入）。

## 两档尺寸的由来

egui 的纹理采样是无 mipmap 的双线性，缩小倍率越大越糊。侧栏图标恒占 20 逻辑
像素，物理像素数随 DPI 缩放走（100% → 20px，200% → 40px），所以按
`pixels_per_point` 在 32/64 两档里挑更接近的那张。与窗口图标
`icons/lumen-icon-32.png` / `-64.png` 是同一套做法。

## 文件

- `svg/` —— 各家官方品牌符号的矢量原稿（取自 [simple-icons](https://github.com/simple-icons/simple-icons)，
  该项目本身 CC0；符号本身是各公司商标，此处仅用于标识「这个会话在跑哪个 CLI」）。
  - `claude.svg` → Claude Code
  - `openai.svg` → Codex CLI
  - `googlegemini.svg` → Gemini CLI
  - `kimi.svg` → Kimi CLI
- `svg_raster.py` —— 极简 SVG `<path>` 光栅化器（nonzero winding + 超采样抗锯齿）。
  只支持单 path 图标，够用即可，不追求完整 SVG 支持。
- `make_llm_icons.py` —— 圆角底 + 品牌符号的合成与配色。

## 配色取值

| CLI | 底色 | 符号 | 描边 |
| --- | --- | --- | --- |
| Claude Code | `#D97757`（Anthropic coral） | 白 | 白 16% |
| Codex CLI | `#0D0D0D`（OpenAI 黑） | 白 | 白 16% |
| Gemini CLI | `#26282D` | `#4285F4 → #9B72CB → #D96570` 渐变 | 白 16% |
| Kimi CLI | `#FFFFFF` | `#0B0B0B` | 黑 14% |

描边不是装饰：Lumen 深色主题侧栏是 `#232323`、浅色是 `#f5f5f5`，纯黑底的 Codex
和纯白底的 Kimi 各自会在其中一种主题里糊掉边界，统一加一圈描边比逐主题换图省事。
Codex 与 Kimi 一黑一白也是刻意的——两者都是单色标，同色底会难分辨。
