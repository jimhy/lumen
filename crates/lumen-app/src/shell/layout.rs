//! 终端分屏布局引擎（F5 / M3.7）：固定均分，纯函数无状态。
//!
//! 布局规则（海风哥拍板）：1=满屏、2=左右、3=左中右、4=上2下2、
//! 5=上3下2、6=上3下3；两排时上排在前（返回顺序先上排自左向右、
//! 再下排）。窗格之间留 [`PANE_GAP`] 逻辑像素间隙（露出底色作分隔
//! 线）；窗格内不留额外边距（终端自身有 padding）。

/// 窗格间隙（逻辑像素）。
pub const PANE_GAP: f32 = 2.0;

/// 计算 n 个窗格在 `area` 内的矩形（egui 逻辑点坐标，未做像素对齐
/// ——调用方按 DPI round_to_pixels）。
///
/// n=0 返回空；n>6 防御性按 6 计算（调用方维护上限不变量，见
/// session::MAX_PANES）。各行等高、行内各列等宽，浮点均分（±半像素
/// 的差异由调用方的像素对齐吸收）。
pub fn pane_rects(n: usize, area: egui::Rect) -> Vec<egui::Rect> {
    // 每排的列数：上排在前。
    let rows: &[usize] = match n {
        0 => return Vec::new(),
        1 => &[1],
        2 => &[2],
        3 => &[3],
        4 => &[2, 2],
        5 => &[3, 2],
        _ => &[3, 3], // 6 及以上（防御截断到 6）
    };
    let nrows = rows.len() as f32;
    let row_h = (area.height() - PANE_GAP * (nrows - 1.0)) / nrows;
    let mut out = Vec::with_capacity(n.min(6));
    for (ri, &cols) in rows.iter().enumerate() {
        let y0 = area.min.y + ri as f32 * (row_h + PANE_GAP);
        let col_w = (area.width() - PANE_GAP * (cols as f32 - 1.0)) / cols as f32;
        for ci in 0..cols {
            let x0 = area.min.x + ci as f32 * (col_w + PANE_GAP);
            out.push(egui::Rect::from_min_size(
                egui::pos2(x0, y0),
                egui::vec2(col_w, row_h),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试区域：x 0..304、y 0..202（宽度对 3 列、高度对 2 排都能
    /// 整除：列宽 (304-4)/3=100、(304-2)/2=151；排高 (202-2)/2=100）。
    fn area() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(304.0, 202.0))
    }

    fn assert_rect(r: egui::Rect, x: f32, y: f32, w: f32, h: f32) {
        let eps = 0.01;
        assert!(
            (r.min.x - x).abs() < eps
                && (r.min.y - y).abs() < eps
                && (r.width() - w).abs() < eps
                && (r.height() - h).abs() < eps,
            "矩形不符：得到 {r:?}，期望 min=({x},{y}) size=({w},{h})"
        );
    }

    #[test]
    fn 零与超限() {
        assert!(pane_rects(0, area()).is_empty());
        // n>6 防御性按 6 计算（调用方维护上限）。
        assert_eq!(pane_rects(9, area()).len(), 6);
    }

    #[test]
    fn 一格满屏() {
        let r = pane_rects(1, area());
        assert_eq!(r.len(), 1);
        assert_rect(r[0], 0.0, 0.0, 304.0, 202.0);
    }

    #[test]
    fn 两格左右() {
        let r = pane_rects(2, area());
        assert_eq!(r.len(), 2);
        assert_rect(r[0], 0.0, 0.0, 151.0, 202.0);
        assert_rect(r[1], 153.0, 0.0, 151.0, 202.0);
    }

    #[test]
    fn 三格左中右() {
        let r = pane_rects(3, area());
        assert_eq!(r.len(), 3);
        assert_rect(r[0], 0.0, 0.0, 100.0, 202.0);
        assert_rect(r[1], 102.0, 0.0, 100.0, 202.0);
        assert_rect(r[2], 204.0, 0.0, 100.0, 202.0);
    }

    #[test]
    fn 四格上2下2() {
        let r = pane_rects(4, area());
        assert_eq!(r.len(), 4);
        // 上排在前：0/1 上排，2/3 下排。
        assert_rect(r[0], 0.0, 0.0, 151.0, 100.0);
        assert_rect(r[1], 153.0, 0.0, 151.0, 100.0);
        assert_rect(r[2], 0.0, 102.0, 151.0, 100.0);
        assert_rect(r[3], 153.0, 102.0, 151.0, 100.0);
    }

    #[test]
    fn 五格上3下2() {
        let r = pane_rects(5, area());
        assert_eq!(r.len(), 5);
        // 上排 3 个窄列。
        assert_rect(r[0], 0.0, 0.0, 100.0, 100.0);
        assert_rect(r[1], 102.0, 0.0, 100.0, 100.0);
        assert_rect(r[2], 204.0, 0.0, 100.0, 100.0);
        // 下排 2 个宽列。
        assert_rect(r[3], 0.0, 102.0, 151.0, 100.0);
        assert_rect(r[4], 153.0, 102.0, 151.0, 100.0);
    }

    #[test]
    fn 六格上3下3() {
        let r = pane_rects(6, area());
        assert_eq!(r.len(), 6);
        assert_rect(r[0], 0.0, 0.0, 100.0, 100.0);
        assert_rect(r[2], 204.0, 0.0, 100.0, 100.0);
        assert_rect(r[3], 0.0, 102.0, 100.0, 100.0);
        assert_rect(r[5], 204.0, 102.0, 100.0, 100.0);
    }

    #[test]
    fn 区域偏移与边界() {
        // 非零原点（真实终端区在侧栏/顶栏右下方）：矩形跟随原点，
        // 且全部窗格都落在区域内、最后一格右下角贴齐区域边界。
        let a = egui::Rect::from_min_size(egui::pos2(180.0, 36.0), egui::vec2(304.0, 202.0));
        for n in 1..=6 {
            let rects = pane_rects(n, a);
            assert_eq!(rects.len(), n);
            for r in &rects {
                assert!(a.contains_rect(*r), "n={n} 窗格 {r:?} 超出区域 {a:?}");
            }
            let last = rects[rects.len() - 1];
            assert!(
                (last.max.x - a.max.x).abs() < 0.01 && (last.max.y - a.max.y).abs() < 0.01,
                "n={n} 末格右下角 {last:?} 未贴齐区域 {a:?}"
            );
        }
    }
}
