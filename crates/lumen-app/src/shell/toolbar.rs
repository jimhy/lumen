//! 应用工具栏（F12 批1，海风哥 2026-07-16 需求）：标题栏之下的
//! 全宽横条，统一收纳原散落在标题栏上的功能按钮。
//! **左端**（视图开关组）：
//! - ①侧栏（会话列表）显隐开关（与设置 sidebar_visible 同状态源）
//! - ②文件树显隐开关（与 Ctrl+B 同状态源）
//!
//! **右端**（窗格操作组，海风哥 2026-07-17 验收反馈迁至最右）：
//! - ③还原窗格大小（pane_count > 1 才可用，恢复均分）
//! - ④「＋」新增窗格（居最右；满 MAX_PANES 禁用 + 悬停提示）
//!
//! 标题栏（topbar）自此只留拖动区 + 头像 + 窗控三按钮。
//! 工具栏**不是**窗口拖动区：空白处不响应拖动/双击最大化/右键
//! 系统菜单（不做任何 drag 交互即天然如此，勿在此加 Sense::drag）。
//!
//! 图标绘制函数自 topbar 迁入（R8.2 精绘规格原样保留）：视觉盒
//! ~18×14 逻辑 px 居中于 28×26 热区；线宽 1.2；常态 fg_dim、hover
//! 圆角底 bg_highlight（圆角 4）。「＋」改为同规格自绘十字（原
//! topbar 为文字按钮，入栏后与其余图标视觉语言统一；点击/禁用/
//! 悬停提示语义不变）。底部 1px 分隔线用 pal.panel_outline（与
//! 侧栏/文件树面板轮廓同语义色，像素对齐防分数 DPI 模糊）。
//!
//! UI 只产出动作（[`ToolbarOutput`]），可见性写盘、窗格复位与新增
//! 由上层（shell/mod.rs → main.rs）执行。后续新增按钮按既有热区/
//! 间距规格（组内 [`BTN_GAP`]）追加：左组向右接、右组向左接。

use crate::i18n;
use crate::session::MAX_PANES;

use super::theme::Palette;

/// 工具栏高度（逻辑像素）。加入后终端区高度变化走既有的
/// 「矩形变化 → 重建离屏纹理 + 全会话 resize」链路。
pub const HEIGHT: f32 = 32.0;
/// 图标按钮热区宽 × 高（逻辑像素，沿用 topbar R8 规格 28×26）。
const BTN_W: f32 = 28.0;
const BTN_H: f32 = 26.0;
/// 左端按钮组前缘内边距（与 topbar 左组一致）。
const LEFT_MARGIN: f32 = 10.0;
/// 右端按钮组后缘内边距（与左缘对称）。
const RIGHT_MARGIN: f32 = 10.0;
/// 组内按钮间距（R8.2 规格 4）。
const BTN_GAP: f32 = 4.0;

/// 一帧工具栏 UI 的产出。
#[derive(Default)]
pub struct ToolbarOutput {
    /// 切换会话栏显示/隐藏（点击①按钮）。None = 未点击，Some(v) = 新可见值。
    pub toggle_sidebar: Option<bool>,
    /// 切换文件树显示/隐藏（点击②按钮，与 Ctrl+B 同状态源）。None = 未点击，Some(v) = 新可见值。
    pub toggle_filetree: Option<bool>,
    /// 点击了③还原窗格大小按钮（当前 tab 全部窗格比例恢复均分）。
    pub reset_layout: bool,
    /// 点击了④「＋」：焦点 tab 内新增窗格（同 Ctrl+Shift+D，F5）。
    pub new_pane: bool,
}

/// 工具栏按钮的当前状态（打包传入 [`show`]，仿 topbar::ViewState 模式）。
pub struct ViewState {
    /// 会话栏（①）当前是否可见。
    pub sidebar_visible: bool,
    /// 文件树（②）当前是否可见（与 Ctrl+B 同状态源）。
    pub filetree_visible: bool,
}

/// 绘制应用工具栏（全宽横条；须在 topbar 之后、侧栏之前加入面板
/// 布局——egui 顶部面板按声明顺序自上而下堆叠，即紧贴标题栏下方）。
///
/// # 参数
/// - `pane_count`：激活 tab 当前窗格数（③复位可用判定、④「＋」满额禁用判定）。
/// - `view`：①②开关按钮的当前可见态。
pub fn show(
    root: &mut egui::Ui,
    pane_count: usize,
    pal: &Palette,
    view: ViewState,
) -> ToolbarOutput {
    let mut out = ToolbarOutput::default();
    egui::Panel::top("lumen_toolbar")
        .exact_size(HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(pal.bg_dark)
                .inner_margin(egui::Margin::symmetric(0, 0)),
        )
        .show_inside(root, |ui| {
            // 底部 1px 分隔线（panel_outline 语义色，像素对齐——与侧栏/
            // 文件树面板轮廓同款画法，防分数 DPI 下模糊/双粗）。
            {
                use egui::emath::GuiRounding as _;
                let ppp = ui.pixels_per_point();
                let r = ui.max_rect().round_to_pixels(ppp);
                let hw = 0.5 / ppp;
                ui.painter().line_segment(
                    [
                        egui::pos2(r.min.x, r.max.y - hw),
                        egui::pos2(r.max.x, r.max.y - hw),
                    ],
                    egui::Stroke::new(1.0 / ppp, pal.panel_outline),
                );
            }

            // 双端布局（同 topbar 迁移前的模式）：外层 RTL 先画右端
            // 窗格操作组（先加的在最右），余下空间内层切回 LTR 画左端
            // 视图开关组。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let s = i18n::strings();
                ui.add_space(RIGHT_MARGIN);

                // ④「＋」新增窗格（F5，居最右）：满 MAX_PANES 禁用 + 悬停提示。
                {
                    let enabled = pane_count < MAX_PANES;
                    let (plus_rect, plus_resp) =
                        ui.allocate_exact_size(egui::vec2(BTN_W, BTN_H), egui::Sense::click());
                    draw_icon_plus(ui, plus_rect, enabled, pal);
                    let tip = if enabled {
                        s.topbar_new_pane_tip.to_owned()
                    } else {
                        i18n::fmt1(s.topbar_max_panes_fmt, MAX_PANES)
                    };
                    if plus_resp.on_hover_text(tip).clicked() && enabled {
                        out.new_pane = true;
                    }
                }

                ui.add_space(BTN_GAP);

                // ③ 还原窗格大小（田字格图标；单窗格无可复位，禁用态）
                {
                    let enabled = pane_count > 1;
                    let (reset_rect, reset_resp) =
                        ui.allocate_exact_size(egui::vec2(BTN_W, BTN_H), egui::Sense::click());
                    draw_icon_grid(ui, reset_rect, enabled, pal);
                    let tip = if enabled {
                        s.topbar_reset_layout_tip
                    } else {
                        s.topbar_reset_layout_disabled_tip
                    };
                    if reset_resp.on_hover_text(tip).clicked() && enabled {
                        out.reset_layout = true;
                    }
                }

                // 余下空间：左端视图开关组（LTR）。
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add_space(LEFT_MARGIN);

                    // ① 显示/隐藏会话栏（codicon layout-sidebar-left 风格）
                    {
                        let (sb_rect, sb_resp) =
                            ui.allocate_exact_size(egui::vec2(BTN_W, BTN_H), egui::Sense::click());
                        draw_icon_sidebar(ui, sb_rect, view.sidebar_visible, pal);
                        let tip = if view.sidebar_visible {
                            s.topbar_sidebar_hide_tip
                        } else {
                            s.topbar_sidebar_show_tip
                        };
                        if sb_resp.on_hover_text(tip).clicked() {
                            out.toggle_sidebar = Some(!view.sidebar_visible);
                        }
                    }

                    ui.add_space(BTN_GAP);

                    // ② 显示/隐藏文件树（codicon list-tree 风格）
                    {
                        let (ft_rect, ft_resp) =
                            ui.allocate_exact_size(egui::vec2(BTN_W, BTN_H), egui::Sense::click());
                        draw_icon_filetree(ui, ft_rect, view.filetree_visible, pal);
                        let tip = if view.filetree_visible {
                            s.topbar_filetree_hide_tip
                        } else {
                            s.topbar_filetree_show_tip
                        };
                        if ft_resp.on_hover_text(tip).clicked() {
                            out.toggle_filetree = Some(!view.filetree_visible);
                        }
                    }
                });
            });
        });
    out
}

// ── 图标绘制子函数（自 topbar 迁入；R8.2 精绘规格原样保留）────────────────
// 视觉盒 ~18×14 逻辑 px 居中于 28×26 热区；线宽 1.2；
// 颜色常态 fg_dim，hover fg；hover 圆角底 bg_highlight（圆角 4）。

/// ① 侧栏切换图标（codicon layout-sidebar-left 风格）：
/// 圆角外框 18×14（圆角 2.5）+ 距左 1/3 处竖分隔线；
/// 侧栏可见态左舱填充（fg_dim 40% 透明度），隐藏态仅线框。
fn draw_icon_sidebar(ui: &egui::Ui, rect: egui::Rect, visible: bool, pal: &Palette) {
    let painter = ui.painter();
    // 悬停底色
    if ui.rect_contains_pointer(rect) {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    }
    let fg = if visible { pal.fg } else { pal.fg_dim };
    let stroke = egui::Stroke::new(1.2_f32, fg);
    let c = rect.center();
    // 外框 18×14，圆角 2.5，像素对齐
    let bw = 18.0_f32;
    let bh = 14.0_f32;
    let ox = (c.x - bw / 2.0 + 0.5).floor() - 0.5; // round to 0.5
    let oy = (c.y - bh / 2.0 + 0.5).floor() - 0.5;
    let frame = egui::Rect::from_min_size(egui::pos2(ox, oy), egui::vec2(bw, bh));
    painter.rect_stroke(frame, 2.5, stroke, egui::StrokeKind::Middle);
    // 左 1/3 竖分隔线（距左缘约 bw/3）
    let div_x = (ox + bw / 3.0 + 0.5).floor() - 0.5;
    painter.line_segment(
        [
            egui::pos2(div_x, oy + 1.0),
            egui::pos2(div_x, oy + bh - 1.0),
        ],
        stroke,
    );
    // 可见态：左舱填充（fg_dim 40% 透明度的 rect）
    if visible {
        let fill_color = egui::Color32::from_rgba_unmultiplied(
            pal.fg_dim.r(),
            pal.fg_dim.g(),
            pal.fg_dim.b(),
            (pal.fg_dim.a() as f32 * 0.4) as u8,
        );
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(ox + 1.5, oy + 1.5),
                egui::pos2(div_x - 0.5, oy + bh - 1.5),
            ),
            1.5,
            fill_color,
        );
    }
}

/// ② 文件树切换图标（codicon list-tree 风格）：
/// 无外框；左侧竖干线（高 14）+ 向右三条横枝（y 均分，长度 9/6.5/4）。
/// 层次差拉大（9/6.5/4）使高 DPI 下层次可辨；可见态 fg，隐藏态 fg_dim。
fn draw_icon_filetree(ui: &egui::Ui, rect: egui::Rect, visible: bool, pal: &Palette) {
    let painter = ui.painter();
    if ui.rect_contains_pointer(rect) {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    }
    let fg = if visible { pal.fg } else { pal.fg_dim };
    let stroke = egui::Stroke::new(1.2_f32, fg);
    let c = rect.center();
    // 树形高度 14，竖干偏左 8px
    let tree_h = 14.0_f32;
    let trunk_x = (c.x - 8.0 + 0.5).floor() - 0.5; // 竖干 x，像素对齐
    let top_y = (c.y - tree_h / 2.0 + 0.5).floor() - 0.5;
    let bot_y = top_y + tree_h;
    // 竖干
    painter.line_segment(
        [egui::pos2(trunk_x, top_y), egui::pos2(trunk_x, bot_y)],
        stroke,
    );
    // 三条横枝（y 均分于 top+2/top+7/top+12；长度差拉大 9/6.5/4）
    let branches: [(f32, f32); 3] = [(top_y + 2.0, 9.0), (top_y + 7.0, 6.5), (top_y + 12.0, 4.0)];
    for (by, branch_len) in branches {
        let by = (by + 0.5).floor() - 0.5;
        painter.line_segment(
            [
                egui::pos2(trunk_x, by),
                egui::pos2(trunk_x + branch_len, by),
            ],
            stroke,
        );
    }
}

/// ③ 还原窗格图标（田字格风格）：
/// 圆角外框 16×14（圆角 2.5）+ 内部横竖中线十字分隔（2×2 田字）。
/// 不画四个分离小方块（避免显碎）。禁用态 fg_dim 再压暗 40%。
fn draw_icon_grid(ui: &egui::Ui, rect: egui::Rect, enabled: bool, pal: &Palette) {
    let painter = ui.painter();
    let hovered = ui.rect_contains_pointer(rect);
    if hovered && enabled {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    }
    let fg = if !enabled {
        pal.fg_dim.gamma_multiply(0.4)
    } else if hovered {
        pal.fg
    } else {
        pal.fg_dim
    };
    let stroke = egui::Stroke::new(1.2_f32, fg);
    let c = rect.center();
    // 16×14，比例对齐侧栏/树形框
    let bw = 16.0_f32;
    let bh = 14.0_f32;
    let ox = (c.x - bw / 2.0 + 0.5).floor() - 0.5;
    let oy = (c.y - bh / 2.0 + 0.5).floor() - 0.5;
    let frame = egui::Rect::from_min_size(egui::pos2(ox, oy), egui::vec2(bw, bh));
    // 圆角外框
    painter.rect_stroke(frame, 2.5, stroke, egui::StrokeKind::Middle);
    // 内部横中线
    let mid_y = (oy + bh / 2.0 + 0.5).floor() - 0.5;
    painter.line_segment(
        [
            egui::pos2(ox + 1.5, mid_y),
            egui::pos2(ox + bw - 1.5, mid_y),
        ],
        stroke,
    );
    // 内部竖中线
    let mid_x = (ox + bw / 2.0 + 0.5).floor() - 0.5;
    painter.line_segment(
        [
            egui::pos2(mid_x, oy + 1.5),
            egui::pos2(mid_x, oy + bh - 1.5),
        ],
        stroke,
    );
}

/// ④ 新增窗格图标（自绘「＋」十字）：臂长 6（视觉盒 12×12，与
/// 18×14 系图标观感均衡）；禁用态 fg_dim 再压暗 40%（同③风格）。
/// 原 topbar 为文字按钮「＋」，入栏后改自绘线条与其余图标统一
/// （点击/禁用/悬停提示语义不变）。
fn draw_icon_plus(ui: &egui::Ui, rect: egui::Rect, enabled: bool, pal: &Palette) {
    let painter = ui.painter();
    let hovered = ui.rect_contains_pointer(rect);
    if hovered && enabled {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    }
    let fg = if !enabled {
        pal.fg_dim.gamma_multiply(0.4)
    } else if hovered {
        pal.fg
    } else {
        pal.fg_dim
    };
    let stroke = egui::Stroke::new(1.2_f32, fg);
    let c = rect.center();
    // 十字中心像素对齐（round to 0.5，与其余图标同法）
    let cx = (c.x + 0.5).floor() - 0.5;
    let cy = (c.y + 0.5).floor() - 0.5;
    let r = 6.0;
    painter.line_segment([egui::pos2(cx - r, cy), egui::pos2(cx + r, cy)], stroke);
    painter.line_segment([egui::pos2(cx, cy - r), egui::pos2(cx, cy + r)], stroke);
}

#[cfg(test)]
mod toolbar_layout_tests {
    use super::*;
    use crate::shell::theme;

    fn test_palette() -> Palette {
        let info = lumen_renderer::themes::find_or_default("lumen-dark");
        theme::shell_palette(info)
    }

    fn test_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1200.0, 700.0),
            )),
            ..Default::default()
        }
    }

    /// 布局哨兵：工具栏占高恰为 HEIGHT——面板加入后余下区域的 y 起点
    /// 应下移 HEIGHT（工作区 y 起点接线依赖此值，偏差即布局断裂）。
    #[test]
    fn 工具栏_占高等于常量() {
        let ctx = egui::Context::default();
        let pal = test_palette();
        let _ = ctx.run_ui(test_input(), |ui| {
            let _ = show(
                ui,
                1,
                &pal,
                ViewState {
                    sidebar_visible: true,
                    filetree_visible: true,
                },
            );
            let rest = ui.available_rect_before_wrap();
            assert!(
                (rest.min.y - HEIGHT).abs() < 0.5,
                "工具栏后余下区域 y 起点应为 HEIGHT={HEIGHT}，实为 {}",
                rest.min.y
            );
        });
    }

    /// 绘制哨兵：一帧的绘制图元里，左端区域（x<200）应存在①②图标的
    /// 线段图元（①框内分隔线 ②竖干+三横枝），右端区域（x>1000，屏宽
    /// 1200）应存在③④图标的线段图元（③田字两中线 ④十字两线）——
    /// 按钮绘制被条件分支意外跳过、或左右分组错位时此测试失败。
    #[test]
    fn 工具栏_左右两端按钮图元存在() {
        let ctx = egui::Context::default();
        let pal = test_palette();
        let full = ctx.run_ui(test_input(), |ui| {
            let _ = show(
                ui,
                2,
                &pal,
                ViewState {
                    sidebar_visible: true,
                    filetree_visible: false,
                },
            );
        });
        fn count_segs(s: &egui::epaint::Shape, x_pred: &dyn Fn(f32) -> bool) -> usize {
            use egui::epaint::Shape;
            match s {
                Shape::LineSegment { points, .. } => {
                    usize::from(x_pred(points[0].x) && points[0].y < HEIGHT)
                }
                Shape::Vec(v) => v.iter().map(|s| count_segs(s, x_pred)).sum(),
                _ => 0,
            }
        }
        let left: usize = full
            .shapes
            .iter()
            .map(|cs| count_segs(&cs.shape, &|x| x < 200.0))
            .sum();
        let right: usize = full
            .shapes
            .iter()
            .map(|cs| count_segs(&cs.shape, &|x| x > 1000.0))
            .sum();
        // 左端：①分隔线 + ②竖干与三横枝 ≥ 5 条。
        assert!(left >= 5, "左端线段图元过少：{left} 条——视图开关按钮没画");
        // 右端：③田字两中线 + ④十字两线 ≥ 4 条。
        assert!(
            right >= 4,
            "右端线段图元过少：{right} 条——窗格操作按钮没画或没靠右"
        );
    }

    /// 满额哨兵：pane_count = MAX_PANES 时「＋」为禁用态，一帧渲染
    /// 不 panic 且无点击也不产出任何动作（Output 全默认）。
    #[test]
    fn 工具栏_满额帧无动作产出() {
        let ctx = egui::Context::default();
        let pal = test_palette();
        let mut got_default = false;
        let _ = ctx.run_ui(test_input(), |ui| {
            let out = show(
                ui,
                MAX_PANES,
                &pal,
                ViewState {
                    sidebar_visible: false,
                    filetree_visible: true,
                },
            );
            got_default = !out.new_pane
                && !out.reset_layout
                && out.toggle_sidebar.is_none()
                && out.toggle_filetree.is_none();
        });
        assert!(got_default, "无输入的一帧不应产出任何动作");
    }
}
