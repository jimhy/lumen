//! 内置 LLM CLI 品牌图标：会话前台跑的是受支持的 LLM CLI 时，用它顶替
//! [`crate::proc_icon`] 抽出来的「前台进程 exe 图标」。
//!
//! # 为什么这里必须破一次 F7② 的规则
//!
//! F7② 定的是「会话图标 = 会话内前台运行程序的 exe 图标」。这条规则对普通
//! 程序成立，对 LLM CLI **一个都不成立**（下表为 2026-08-17 在 Windows 上
//! 逐个实跑、查进程树 + 抽关联图标实测所得，不是推断）：
//!
//! | CLI | 前台真实进程（shell 的直接子进程） | 抽到的图标 |
//! |---|---|---|
//! | Claude Code | `node.exe`（npm 的 `claude.ps1` shim） | Node.js 图标 |
//! | Codex CLI | `node.exe`（同上） | Node.js 图标 |
//! | Gemini CLI | `node.exe`（同上） | Node.js 图标 |
//! | Kimi CLI | `kimi.exe`（Node 打包的原生二进制；经 `uvx` 安装则是 `python.exe`） | Node.js 图标 |
//!
//! 也就是四个 CLI 会顶着**同一个** Node 绿六边形，彼此还分不开。
//!
//! Codex 那行还多一层：真正的 `codex.exe` 是 `node.exe` 的**孙**进程
//! （`pwsh → node.exe → codex.exe`），而 [`crate::proc_icon`] 只查直接子进程，
//! 压根够不到它；就算够得到也没用——实测 `codex.exe` 没嵌资源图标，抽出来是
//! Windows 通用程序占位方块。
//!
//! 抽到的要么是个通用方块，要么是**运行时**的图标——都不回答用户看侧栏时真正
//! 要问的那个问题：这个会话在跑哪个 AI。所以识别到 CLI 时内置图优先；没识别到
//! 的会话完全走原路径，规则本身没变。
//!
//! # 为什么内置两档尺寸
//!
//! egui 的纹理采样是无 mipmap 的双线性，缩小倍率越大细节丢得越多（OpenAI 花结
//! 那种细线条最先糊）。侧栏图标恒占 [`SIDEBAR_ICON_PT`] 逻辑像素，物理像素数
//! 随 DPI 缩放走（100% → 20px，200% → 40px），故按 `pixels_per_point` 在 32/64
//! 两档里挑「不必放大、又不必大幅缩小」的那张。窗口图标
//! （`lumen-icon-32.png` / `-64.png`）本来就是这么做的，此处沿用。
//!
//! 图标由 `scripts/llm-icons/` 从各家官方 SVG 生成，外观要改走那条链路重生成，
//! 别手改 PNG。

use crate::llm_cli::LlmCliKind;
use crate::proc_icon::IconRgba;

/// 侧栏会话图标的逻辑边长，与 `shell` 里那个 20×20 的绘制框保持一致。
/// 只用于选尺寸档，画多大仍由渲染侧说了算。
const SIDEBAR_ICON_PT: f32 = 20.0;

/// 低 DPI 档的图标边长（像素）。也是远程上线固定使用的那一档。
const SMALL_PX: u32 = 32;

/// 内置图标的原始 PNG 字节。`hidpi` 为真取 64px 档，否则取 32px 档。
const fn png_bytes(kind: LlmCliKind, hidpi: bool) -> &'static [u8] {
    match (kind, hidpi) {
        (LlmCliKind::Claude, false) => include_bytes!("../../../icons/llm/claude-32.png"),
        (LlmCliKind::Claude, true) => include_bytes!("../../../icons/llm/claude-64.png"),
        (LlmCliKind::Codex, false) => include_bytes!("../../../icons/llm/codex-32.png"),
        (LlmCliKind::Codex, true) => include_bytes!("../../../icons/llm/codex-64.png"),
        (LlmCliKind::Gemini, false) => include_bytes!("../../../icons/llm/gemini-32.png"),
        (LlmCliKind::Gemini, true) => include_bytes!("../../../icons/llm/gemini-64.png"),
        (LlmCliKind::Kimi, false) => include_bytes!("../../../icons/llm/kimi-32.png"),
        (LlmCliKind::Kimi, true) => include_bytes!("../../../icons/llm/kimi-64.png"),
    }
}

/// 当前 DPI 缩放下该不该取 64px 档。
///
/// 判据是「宁可轻微缩小，也不要放大」：图标实际画成 `20 * pixels_per_point`
/// 个物理像素，只要它没超过 32px，32 档就已经够用（且比把 64 档缩下来更清晰）；
/// 超过 32px 才换 64 档，否则 32 档被放大会发虚。
///
/// 常见档位：100%/125%/150% 缩放 → 20/25/30 物理像素 → 32 档；
/// 175%/200% → 35/40 物理像素 → 64 档。
pub fn prefers_hidpi(pixels_per_point: f32) -> bool {
    SIDEBAR_ICON_PT * pixels_per_point > SMALL_PX as f32
}

/// 解码内置图标为 top-down RGBA8。
///
/// 返回类型与 [`crate::proc_icon::load_icon_rgba`] 一致，让「内置图标」和
/// 「exe 图标」两条来源共用上层同一套纹理缓存与远程位图管线。
/// 解码失败返回 `None`（上层回退自绘终端字形），不 panic——图标是视觉增强。
pub fn load_icon_rgba(kind: LlmCliKind, hidpi: bool) -> Option<IconRgba> {
    let bytes = png_bytes(kind, hidpi);
    match image::load_from_memory_with_format(bytes, image::ImageFormat::Png) {
        Ok(img) => {
            let img = img.into_rgba8();
            let (width, height) = img.dimensions();
            Some(IconRgba {
                width,
                height,
                rgba: img.into_raw(),
            })
        }
        Err(e) => {
            log::warn!(
                "内置 LLM 图标解码失败（{}），回退字形：{e}",
                kind.display_name()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [LlmCliKind; 4] = [
        LlmCliKind::Claude,
        LlmCliKind::Codex,
        LlmCliKind::Gemini,
        LlmCliKind::Kimi,
    ];

    #[test]
    fn 内置图标_两档全部可解码且尺寸自洽() {
        for kind in ALL {
            for (hidpi, expect) in [(false, 32u32), (true, 64u32)] {
                let icon = load_icon_rgba(kind, hidpi)
                    .unwrap_or_else(|| panic!("{} 的 {expect}px 档应能解码", kind.display_name()));
                assert_eq!(icon.width, expect, "{} 宽度", kind.display_name());
                assert_eq!(icon.height, expect, "{} 高度", kind.display_name());
                assert_eq!(
                    icon.rgba.len(),
                    (expect * expect * 4) as usize,
                    "{} 的 RGBA 长度必须与尺寸自洽——上层 ColorImage::from_rgba_unmultiplied \
                     对不上会 panic",
                    kind.display_name()
                );
            }
        }
    }

    #[test]
    fn 内置图标_四家各不相同() {
        // 防「复制粘贴 include_bytes! 时漏改文件名」——那种错编译不报，
        // 只会在侧栏上让两个 CLI 顶着同一张图。
        let bytes: Vec<&[u8]> = ALL.iter().map(|k| png_bytes(*k, false)).collect();
        for i in 0..bytes.len() {
            for j in (i + 1)..bytes.len() {
                assert_ne!(
                    bytes[i],
                    bytes[j],
                    "{} 与 {} 用了同一张图",
                    ALL[i].display_name(),
                    ALL[j].display_name()
                );
            }
        }
        // 同一家的两档也必须是不同文件（漏改尺寸后缀同样编译无声）。
        for kind in ALL {
            assert_ne!(
                png_bytes(kind, false),
                png_bytes(kind, true),
                "{} 的两档指向了同一张图",
                kind.display_name()
            );
        }
    }

    #[test]
    fn 选档_不放大优先_并在高dpi换大图() {
        assert!(!prefers_hidpi(1.0), "100% 缩放 20px，32 档够用");
        assert!(!prefers_hidpi(1.25), "125% 缩放 25px");
        assert!(!prefers_hidpi(1.5), "150% 缩放 30px，仍不该放大 32 档");
        assert!(prefers_hidpi(1.75), "175% 缩放 35px，超过 32 就换 64 档");
        assert!(prefers_hidpi(2.0), "200% 缩放 40px");
    }
}
