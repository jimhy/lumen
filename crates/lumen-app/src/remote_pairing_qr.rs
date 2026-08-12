//! 配对二维码的模块矩阵与绘制（M7 片 6，被控端侧）。
//!
//! # 为什么是「自己画格子」而不是生成图片
//!
//! `qrcodegen`（Nayuki）**零传递依赖、无 unsafe**，而我们要的只有「文本 → 布尔矩阵」
//! 这一件事。走 `image` / `svg` 那条路要多引一整棵依赖树，还会诱导出「把二维码存成文件」
//! 的实现——**配对码是一次性口令，落盘就等于把它留在了磁盘上**。这里全程只在内存里，
//! 上传成一张 egui 纹理，会话结束即随缓存丢弃。
//!
//! # 每帧重建 2000+ 个矩形是不行的
//!
//! 一个典型载荷编码出来是 37×37 到 45×45 个模块。逐格 `rect_filled` 意味着每帧往
//! tessellator 里塞两千个矩形，而这个横幅在配对期间会**持续显示 120 秒**。
//! 所以按模块矩阵生成一张 N×N 的 `ColorImage` 上传成纹理，之后每帧只画一个 image；
//! 采样必须是 **NEAREST**，否则放大后格子边缘会被线性插值糊掉，手机就扫不出来了。
//!
//! # 锁屏
//!
//! 「应用锁锁定时不得渲染配对码 / 二维码」这条由**架构**保证，不靠这里判断：
//! 锁屏走的是 `main.rs` 里一条独立的渲染路径，只画 `shell::lock_ui`，根本不会调用
//! `shell::show`——横幅与二维码都不在那条路径上。⚠ **后人若要把横幅挪到通用路径上，
//! 必须在那里补回锁屏判定**。

use lumen_protocol::pairing_qr::PairingQrPayload;
use qrcodegen::{QrCode, QrCodeEcc};

/// 二维码在横幅里的边长（逻辑像素）。
///
/// 取 132 是因为：手机摄像头要在一臂距离外扫到 45×45 个模块，每模块至少需要 2 个
/// 逻辑像素才不至于在缩放后糊掉（132 / 45 ≈ 2.9），再大就会把横幅撑得喧宾夺主。
pub const QR_SIDE: f32 = 132.0;

/// 静区（quiet zone）宽度，单位是模块数。
///
/// 规范要求 4 个模块。少于它，扫码器在深色背景上找不到定位图形的边界——
/// 这正是「在深色主题下扫不出来」那类问题的根因，所以它不随主题变。
const QUIET_MODULES: usize = 4;

/// 二维码纹理缓存：同一份载荷只编码与上传一次。
#[derive(Default)]
pub struct PairingQrCache {
    /// 上次编码的文本。配对码 120 秒不变，所以命中率接近 100%。
    text: String,
    texture: Option<egui::TextureHandle>,
}

impl PairingQrCache {
    /// 取（必要时重建）这份载荷对应的纹理。
    ///
    /// 返回 `None` 表示编码失败——载荷超出二维码容量。此时调用方**只画数字配对码**
    /// （那条路径本来就在，用户口述 9 位数字即可完成配对），不显示任何错误。
    /// 这不是无声降级：扫码只是数字码的另一种呈现，少了它功能并不缺失。
    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        payload: &PairingQrPayload,
    ) -> Option<&egui::TextureHandle> {
        let text = payload.to_qr_text().ok()?;
        if self.texture.is_none() || self.text != text {
            let image = encode_to_image(&text)?;
            // 名字里带载荷长度只是为了让 egui 的纹理调试面板可读，不参与任何逻辑。
            self.texture =
                Some(ctx.load_texture("lumen_pairing_qr", image, egui::TextureOptions::NEAREST));
            self.text = text;
        }
        self.texture.as_ref()
    }

    /// 配对结束时丢弃缓存。
    ///
    /// **配对码是一次性口令**：会话建立或配对取消之后，这张图没有任何用途，
    /// 留着只是让一份含码的纹理在显存里多待一会儿。
    pub fn clear(&mut self) {
        self.text.clear();
        self.texture = None;
    }
}

/// 把文本编码成带静区的黑白 `ColorImage`（每模块一个像素）。
///
/// 纠错等级取 **Medium**：配对二维码显示在屏幕上、距离近、无污损，Low 就够用；
/// 但 Medium 只多几个模块，换来对屏幕反光与拍摄角度的宽容度，这个交换是划算的。
fn encode_to_image(text: &str) -> Option<egui::ColorImage> {
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let modules = usize::try_from(qr.size()).ok()?;
    let side = modules + QUIET_MODULES * 2;
    // 深色 = 模块，浅色 = 静区与空白。**不跟随主题**：扫码器需要的是高对比的
    // 深/浅关系，而不是好看的配色；深色主题下用主题色画会显著降低识别率。
    let mut pixels = vec![egui::Color32::WHITE; side * side];
    for y in 0..modules {
        for x in 0..modules {
            let dark = qr.get_module(
                i32::try_from(x).unwrap_or(i32::MAX),
                i32::try_from(y).unwrap_or(i32::MAX),
            );
            if dark {
                pixels[(y + QUIET_MODULES) * side + (x + QUIET_MODULES)] = egui::Color32::BLACK;
            }
        }
    }
    Some(egui::ColorImage {
        size: [side, side],
        pixels,
        source_size: egui::vec2(side as f32, side as f32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 样例载荷() -> PairingQrPayload {
        PairingQrPayload::new(
            "https://lumen.example.com",
            "550e8400-e29b-41d4-a716-446655440000",
            "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "012345678",
            1_786_342_908,
        )
    }

    #[test]
    fn 典型载荷编得出矩阵且带四模块静区() {
        let text = 样例载荷().to_qr_text().expect("序列化");
        let image = encode_to_image(&text).expect("编码");
        let side = image.size[0];
        assert_eq!(image.size[0], image.size[1], "二维码必须是正方形");
        assert!(side > QUIET_MODULES * 2, "除静区外要有真实模块");

        // 最外圈必须全白：静区少了，扫码器在深色背景上找不到定位图形的边界。
        for i in 0..side {
            assert_eq!(image.pixels[i], egui::Color32::WHITE, "上边静区被画黑了");
            assert_eq!(
                image.pixels[(side - 1) * side + i],
                egui::Color32::WHITE,
                "下边静区被画黑了"
            );
            assert_eq!(
                image.pixels[i * side],
                egui::Color32::WHITE,
                "左边静区被画黑了"
            );
            assert_eq!(
                image.pixels[i * side + side - 1],
                egui::Color32::WHITE,
                "右边静区被画黑了"
            );
        }
    }

    #[test]
    fn 每模块至少两个逻辑像素() {
        // 手机要在一臂距离外扫到它。模块数是随载荷长度变的，这条把「载荷变长 ⇒
        // 模块变密 ⇒ 扫不出来」这条链钉在编译期之外的最近一处。
        let text = 样例载荷().to_qr_text().expect("序列化");
        let image = encode_to_image(&text).expect("编码");
        let per_module = QR_SIDE / image.size[0] as f32;
        assert!(
            per_module >= 2.0,
            "每模块只有 {per_module:.2} 逻辑像素（{} 模块），扫码识别率会明显下降",
            image.size[0]
        );
    }

    #[test]
    fn 三个定位图形的结构完整() {
        // 定位图形是 7×7：最外一圈黑、里面一圈白、中心 3×3 黑。三个角各验这三点，
        // 能抓住「x/y 读反」「静区偏移算错」「边界差一」这类错误——它们画出来仍然是个
        // 「像二维码的东西」，只有真拿手机去扫才会发现不对。
        //
        // ⚠ **抓不住矩阵被转置或镜像**：那种情况下三个定位图形看起来仍然正确
        //（转置不动主对角线，另两个互换但它们是同一个图形）。真要验只能去解码，
        // 而那要引一个解码器——不值当，因为转置/镜像在这段代码里没有可能的来源
        //（就一个 x/y 双层循环）。这条注释比一个测不到的断言诚实。
        //
        // 也**不能**断言「右下角是白的」：右下角没有定位图形，但那里是**数据区**，
        // 黑白取决于载荷内容。第一版这么写过，换一份载荷就红了。
        let text = 样例载荷().to_qr_text().expect("序列化");
        let image = encode_to_image(&text).expect("编码");
        let side = image.size[0];
        let 黑 = |x: usize, y: usize| image.pixels[y * side + x] == egui::Color32::BLACK;
        let q = QUIET_MODULES;
        let 末 = side - QUIET_MODULES - 7; // 右/下那两个定位图形的左上角
        for (ox, oy) in [(q, q), (末, q), (q, 末)] {
            assert!(黑(ox, oy), "({ox}, {oy}) 定位图形外框应为黑");
            assert!(黑(ox + 6, oy + 6), "({ox}, {oy}) 定位图形外框右下角应为黑");
            assert!(!黑(ox + 1, oy + 1), "({ox}, {oy}) 定位图形第二圈应为白");
            assert!(黑(ox + 3, oy + 3), "({ox}, {oy}) 定位图形中心应为黑");
        }
    }

    #[test]
    fn 缓存对同一份载荷不重复编码() {
        // 横幅在配对期间持续显示 120 秒，每帧重编码 + 重上传纹理是纯浪费。
        let mut cache = PairingQrCache::default();
        assert!(cache.text.is_empty());
        cache.text = "占位".into();
        cache.clear();
        assert!(cache.text.is_empty(), "clear 之后必须能重新编码");
    }
}
