//! 顶栏（M3.5 / M3.8 / 问题1 修复 / R8 / F12 批1）：
//! - 左端（LTR）：本地/远程视图 tab（M5.2）
//! - 右端（RTL）：关闭 / 最大化还原 / 最小化 / 头像
//! - 中间剩余空白：拖动区（drag / 双击 / 右键语义不变）
//! - 标题文字已删除（R8 海风哥点名去掉路径标题）
//!
//! F12 批1 变更：原左端①侧栏 ②文件树 ③还原窗格三按钮与右端「＋」
//! 新增窗格按钮迁往标题栏之下的应用工具栏（见 [`super::toolbar`]，
//! 图标绘制函数一并迁走）；顶栏只留拖动区 + 头像 + 窗控三按钮，
//! 拖动/双击/右键/Snap Layouts 热区语义不变。
//!
//! 问题1 修复（RTL 布局 cursor().min bug）：
//! - 原实现用 `allocate_rect(Rect::from_min_size(cursor().min, …))` 手工构造矩形，
//!   RTL 布局下 cursor().min 是剩余区域左上角而非当前放置位置，导致三个窗控
//!   按钮全部叠画在面板左端、顶栏内容消失。
//! - 修复：一律用 `allocate_exact_size(vec2(W, H), Sense::click())` 让布局引擎
//!   自动放置（RTL 下靠右排、左移 cursor），painter 按返回 rect 画图。
//!
//! M3.8 变更：
//! - 窗口无边框后，窗控三按钮并入顶栏右端（Warp/VSCode 形态）。
//! - 标题文字左侧空白区兼作拖动手柄（`drag_title_bar` 信号）。
//! - 双击空白区 toggle 最大化；右键空白区弹系统窗口菜单。
//! - 新参数 `is_maximized: bool` 控制最大化/还原图标切换。
//!
//! 规格（docs/M3应用外壳设计.md §4）：未登录头像为占位人形图标，已
//! 登录为强调色圆底 + 展示名首字母；点击弹下拉菜单——已登录：展示名
//! （灰字不可点）/ Settings / Keyboard shortcuts / Documentation
//! （灰显占位）/ 分隔线 / Log out；未登录：Log in / Settings /
//! Keyboard shortcuts。UI 只产出动作（[`TopbarOutput`]），登录/登出
//! 与设置页打开/窗口操作由上层执行。

use crate::i18n;
use crate::profile::Profile;

use super::theme::Palette;

/// 顶栏高度（逻辑像素）。加入后终端区高度变化走既有的
/// 「矩形变化 → 重建离屏纹理 + 全会话 resize」链路。
pub const HEIGHT: f32 = 34.0;
/// 窗控按钮热区宽度（逻辑像素，参考 Win11 约 46 × 34）。
const WC_BTN_W: f32 = 46.0;
/// 视图 tab 热区高度（逻辑像素，R8：26）。
const VIEW_BTN_H: f32 = 26.0;
/// 左端内容前缘内边距。
const LEFT_GROUP_MARGIN: f32 = 10.0;
/// 左端内容间距（R8.2：2→4，增强分组感）。
const VIEW_BTN_GAP: f32 = 4.0;
/// 头像直径。
const AVATAR_SIZE: f32 = 24.0;
/// 下拉菜单宽度（set_min_width 强撑生效后 304 偏宽，海风哥反馈砍半）。
const MENU_WIDTH: f32 = 152.0;

/// 一帧顶栏 UI 的产出。
#[derive(Default)]
pub struct TopbarOutput {
    /// 点击了 Log in（打开登录覆盖层）。
    pub open_login: bool,
    /// 点击了 Settings（打开设置页）。
    pub open_settings: bool,
    /// 点击了 Keyboard shortcuts（打开设置页并定位该分类）。
    pub open_shortcuts: bool,
    /// 头像菜单：点击了「检查更新」（无就绪更新时）→ main 起手动检查。
    pub check_update: bool,
    /// 头像菜单：点击了「更新到 vX」（有就绪更新时）→ main 显示更新弹窗。
    pub open_update: bool,
    /// 头像菜单：点击了「更新日志」→ main 打开 GitHub Releases。
    pub open_whats_new: bool,
    /// 头像菜单：点击了「文档」→ main 打开 GitHub 仓库 README。
    pub open_documentation: bool,
    /// 头像菜单：点击了「反馈」→ main 打开 GitHub Issues。
    pub open_feedback: bool,
    /// 点击了 Log out。
    pub log_out: bool,
    // ── M3.8 窗口控制信号 ──────────────────────────────────────────────
    /// 拖动了顶栏空白区——main 调 window.drag_window()。
    pub drag_title_bar: bool,
    /// 最小化窗口。
    pub minimize_window: bool,
    /// 切换最大化/还原。
    pub toggle_maximize_window: bool,
    /// 关闭窗口——走 CloseRequested 同路径（落盘再退）。
    pub close_window: bool,
    /// 右键空白区弹系统窗口菜单，坐标为 egui 逻辑点。
    pub show_window_menu_at: Option<(f32, f32)>,
    /// 最大化/还原按钮本帧的 egui 逻辑坐标矩形（M3.8 批2 Snap Layouts
    /// 子类化用）：main 换算为屏幕物理像素后写入 snap_layouts 原子。
    /// 按钮不可见（极端情况）时为 None，main 跳过本帧更新。
    pub maximize_btn_rect: Option<egui::Rect>,
    // ── 视图切换信号 ─────────────────────────────────────────────────
    /// 切换本地/远程视图（点击顶栏「本地/远程」tab，M5.2）。
    /// None = 未点击，Some(false) = 本地，Some(true) = 远程。
    pub toggle_view_mode: Option<bool>,
}

/// 顶栏额外状态（打包传入 [`show`]，避免参数列表超过 clippy 7 参数限制）。
pub struct ViewState {
    /// 头像菜单更新项：Some(版本号) = 有就绪更新（显示「更新到 vX」强调项），
    /// None = 无更新（显示「检查更新」）。
    pub update_version: Option<String>,
    /// 当前视图（M5.2）：false = 本地，true = 远程。
    pub current_view: bool,
    /// 登录态已过期需重新登录（token 过期）：头像叠红色感叹号角标 + 菜单出红字「登录过期」。
    /// main 据 `profile.token_expires_at` 判定（自动续期之外的兜底，如关闭 >7 天再开）。
    pub need_relogin: bool,
}

/// 绘制顶栏（全宽窄条；须先于工具栏/侧栏加入面板布局才能横贯整窗
/// 且居于最顶）。
///
/// # 参数
/// - `title`：激活会话标题（R8 已不显示，仅保留参数兼容，未来可用于 OS 窗口标题）。
/// - `is_maximized`：窗口当前是否最大化（切换最大化/还原图标）。
/// - `view`：头像菜单/视图 tab 的当前状态。
pub fn show(
    root: &mut egui::Ui,
    title: &str,
    profile: Option<&Profile>,
    pal: &Palette,
    is_maximized: bool,
    view: ViewState,
) -> TopbarOutput {
    let _ = title; // R8：不显示标题，保留参数供上层传 OS 窗口标题用
    let mut out = TopbarOutput::default();
    egui::Panel::top("lumen_topbar")
        .exact_size(HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(pal.bg_dark)
                .inner_margin(egui::Margin::symmetric(0, 0)),
        )
        .show_inside(root, |ui| {
            // 布局策略（R8 / F12 批1 后）：
            //   右端：RTL 布局分配窗控三按钮 + 头像。
            //   左端：在余下空间内用 LTR 子布局放本地/远程视图 tab。
            //   中间：剩余矩形作为拖动区（drag/双击/右键语义不变）。
            //
            // 关键：一律用 allocate_exact_size(vec2(W, H), Sense::click()) 让布局
            // 引擎自动放置——RTL 下布局引擎从右向左分配，自动靠右排列并左移
            // cursor，painter 按返回 rect 画图，不依赖 cursor().min。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let s = i18n::strings();

                // ── 窗控三按钮（最右，从右到左：关闭 → 最大化/还原 → 最小化）
                // ──────────────────────────────────────────────────────────────
                // macOS 用原生装饰（交通灯），此处不画自绘窗控，避免双套按钮；
                // Windows/Linux 无边框，由自绘顶栏承担窗控。
                if !cfg!(target_os = "macos") {

                // 关闭按钮（悬停红底 #c42b1c 白字，Win11 惯例）
                let (close_rect, close_resp) =
                    ui.allocate_exact_size(egui::vec2(WC_BTN_W, HEIGHT), egui::Sense::click());
                {
                    let painter = ui.painter();
                    let c = close_rect.center();
                    if close_resp.hovered() {
                        // 悬停红底（Win11 关闭按钮惯例色 #c42b1c）
                        painter.rect_filled(
                            close_rect,
                            0.0,
                            egui::Color32::from_rgb(0xc4, 0x2b, 0x1c),
                        );
                    }
                    // ✕ 画线风格（不赌字体覆盖，与窗格标题栏 ✕ 同款）
                    let r = 4.5;
                    let fg = if close_resp.hovered() {
                        egui::Color32::WHITE
                    } else {
                        pal.fg_dim
                    };
                    let stroke = egui::Stroke::new(1.2_f32, fg);
                    painter.line_segment(
                        [egui::pos2(c.x - r, c.y - r), egui::pos2(c.x + r, c.y + r)],
                        stroke,
                    );
                    painter.line_segment(
                        [egui::pos2(c.x - r, c.y + r), egui::pos2(c.x + r, c.y - r)],
                        stroke,
                    );
                }
                if close_resp.on_hover_text(s.wc_close).clicked() {
                    out.close_window = true;
                }

                // 最大化/还原按钮
                let (maxrst_rect, maxrst_resp) =
                    ui.allocate_exact_size(egui::vec2(WC_BTN_W, HEIGHT), egui::Sense::click());
                {
                    let painter = ui.painter();
                    let c = maxrst_rect.center();
                    if maxrst_resp.hovered() {
                        painter.rect_filled(maxrst_rect, 0.0, pal.bg_highlight);
                    }
                    let fg = if maxrst_resp.hovered() {
                        pal.fg
                    } else {
                        pal.fg_dim
                    };
                    let stroke = egui::Stroke::new(1.2_f32, fg);
                    if is_maximized {
                        // 还原图标 ⧉：错位双矩形（与窗格最大化态图标同款画法）
                        let r = 3.0;
                        let back = egui::Rect::from_center_size(
                            c + egui::vec2(1.5, -1.5),
                            egui::vec2(2.0 * r, 2.0 * r),
                        );
                        painter.line_segment(
                            [
                                egui::pos2(back.min.x + 2.0, back.min.y),
                                egui::pos2(back.max.x, back.min.y),
                            ],
                            stroke,
                        );
                        painter.line_segment(
                            [
                                egui::pos2(back.max.x, back.min.y),
                                egui::pos2(back.max.x, back.max.y - 2.0),
                            ],
                            stroke,
                        );
                        let front = egui::Rect::from_center_size(
                            c + egui::vec2(-1.0, 1.0),
                            egui::vec2(2.0 * r, 2.0 * r),
                        );
                        painter.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Middle);
                    } else {
                        // 最大化图标 □：单矩形描边
                        let r = 4.5;
                        painter.rect_stroke(
                            egui::Rect::from_center_size(c, egui::vec2(2.0 * r, 2.0 * r)),
                            0.0,
                            stroke,
                            egui::StrokeKind::Middle,
                        );
                    }
                }
                let tip = if is_maximized {
                    s.wc_restore
                } else {
                    s.wc_maximize
                };
                if maxrst_resp.on_hover_text(tip).clicked() {
                    out.toggle_maximize_window = true;
                }
                // M3.8 批2：记录最大化按钮的逻辑矩形，供 main 换算为
                // 屏幕物理像素后写入 snap_layouts 原子（WM_NCHITTEST 命中用）。
                out.maximize_btn_rect = Some(maxrst_rect);

                // 最小化按钮
                let (min_rect, min_resp) =
                    ui.allocate_exact_size(egui::vec2(WC_BTN_W, HEIGHT), egui::Sense::click());
                {
                    let painter = ui.painter();
                    let c = min_rect.center();
                    if min_resp.hovered() {
                        painter.rect_filled(min_rect, 0.0, pal.bg_highlight);
                    }
                    let fg = if min_resp.hovered() {
                        pal.fg
                    } else {
                        pal.fg_dim
                    };
                    // 「—」横线
                    painter.line_segment(
                        [egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 5.0, c.y)],
                        egui::Stroke::new(1.5_f32, fg),
                    );
                }
                if min_resp.on_hover_text(s.wc_minimize).clicked() {
                    out.minimize_window = true;
                }

                } // end if !macOS（窗控三按钮）

                // ── 头像（紧贴窗控左侧，加右内边距 10px）──────────────────
                ui.add_space(10.0);
                let resp = avatar_button(
                    ui,
                    profile,
                    pal,
                    view.update_version.is_some(),
                    view.need_relogin,
                );
                let update_version = view.update_version.as_deref();
                let need_relogin = view.need_relogin;
                let _ = egui::Popup::menu(&resp)
                    .align(egui::RectAlign::BOTTOM_END)
                    .width(MENU_WIDTH)
                    .show(|ui| menu_ui(ui, profile, pal, update_version, need_relogin, &mut out));
                ui.add_space(6.0);

                // ── 左端本地/远程 tab（M5.2；F12 批1 后左端仅剩它们，
                // 原三视图按钮组已迁往应用工具栏）。中间剩余空白区作为
                // 拖动区。RTL 布局在此处 cursor 已经是右端按钮左侧；用
                // available_rect_before_wrap 取整个余下区域，然后在里面画两层：
                //   1. LTR 子 ui 在左端放视图 tab
                //   2. interact 覆盖整个余下矩形作为拖动区
                let remaining = ui.available_rect_before_wrap();

                // 拖动区（双击/右键/拖动感知）——覆盖余下整个空白，含视图 tab 之间区域
                let drag_resp = ui.interact(
                    remaining,
                    ui.id().with("topbar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if drag_resp.drag_started_by(egui::PointerButton::Primary) {
                    out.drag_title_bar = true;
                }
                if drag_resp.double_clicked() {
                    out.toggle_maximize_window = true;
                }
                if drag_resp.secondary_clicked() {
                    if let Some(pos) = drag_resp.interact_pointer_pos() {
                        out.show_window_menu_at = Some((pos.x, pos.y));
                    }
                }

                // LTR 子布局：在余下区域左端放本地/远程视图 tab
                let mut left_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(remaining)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                left_ui.add_space(LEFT_GROUP_MARGIN);
                if draw_view_tab(&mut left_ui, s.topbar_tab_local, !view.current_view, pal)
                    .clicked()
                {
                    out.toggle_view_mode = Some(false);
                }
                left_ui.add_space(VIEW_BTN_GAP);
                if draw_view_tab(&mut left_ui, s.topbar_tab_remote, view.current_view, pal)
                    .clicked()
                {
                    out.toggle_view_mode = Some(true);
                }
            });
        });
    out
}

// 注：原 R8.2 三个图标绘制函数（①侧栏 ②文件树 ③田字复位）已随
// 按钮迁往 [`super::toolbar`]（F12 批1），此处不再保留副本。

/// 本地/远程 tab 按钮（M5.2）：文字 pill。active = accent 字 + bg_highlight 底；
/// hover = fg 字 + 半透底；常态 = fg_dim 字。返回点击 Response。
fn draw_view_tab(ui: &mut egui::Ui, text: &str, active: bool, pal: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(40.0, VIEW_BTN_H), egui::Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 4.0, pal.bg_highlight);
    } else if resp.hovered() {
        painter.rect_filled(rect, 4.0, pal.bg_highlight.gamma_multiply(0.5));
    }
    let color = if active {
        pal.accent
    } else if resp.hovered() {
        pal.fg
    } else {
        pal.fg_dim
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(12.5),
        color,
    );
    resp
}

/// 圆形头像按钮：已登录 = 强调色圆底 + 首字母；未登录 = 占位人形。
/// `need_relogin` 为真时右上角叠红色感叹号角标（登录过期，最优先）；否则 `has_update` 为真时叠小红点。
fn avatar_button(
    ui: &mut egui::Ui,
    profile: Option<&Profile>,
    pal: &Palette,
    has_update: bool,
    need_relogin: bool,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(AVATAR_SIZE, AVATAR_SIZE), egui::Sense::click());
    let center = rect.center();
    let radius = AVATAR_SIZE / 2.0;
    match profile {
        Some(p) => {
            // 已登录头像：accent 实底 + 反相首字母（深色主题白底黑字，
            // 浅色主题近黑底白字——M3.7b 去蓝，与 CTA 按钮同形态）。
            ui.painter().circle_filled(center, radius, pal.accent);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                p.avatar_letter(),
                egui::FontId::proportional(13.0),
                pal.accent_fg,
            );
        }
        None => {
            ui.painter().circle_filled(center, radius, pal.bg_highlight);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                "👤",
                egui::FontId::proportional(13.0),
                pal.fg_dim,
            );
        }
    }
    // 悬停反馈：外圈描边（圆形按钮没有 egui 默认的底色 hover 效果）。
    if resp.hovered() {
        ui.painter()
            .circle_stroke(center, radius, egui::Stroke::new(1.5_f32, pal.fg_dim));
    }
    // 右上角角标：登录过期（红圆 + 白「!」，最优先）> 有更新（小红点）。先垫一圈顶栏底色，
    // 确保在 accent 头像底 / 顶栏底上都清晰可辨。
    let red = egui::Color32::from_rgb(0xE5, 0x48, 0x4D);
    let badge = egui::pos2(center.x + radius * 0.66, center.y - radius * 0.66);
    if need_relogin {
        let r = 5.0;
        ui.painter().circle_filled(badge, r + 1.2, pal.bg_dark);
        ui.painter().circle_filled(badge, r, red);
        ui.painter().text(
            badge,
            egui::Align2::CENTER_CENTER,
            "!",
            egui::FontId::proportional(8.5),
            egui::Color32::WHITE,
        );
    } else if has_update {
        let dot_r = 3.5;
        ui.painter().circle_filled(badge, dot_r + 1.2, pal.bg_dark);
        ui.painter().circle_filled(badge, dot_r, red);
    }
    // 悬停提示：过期时提示重新登录。
    let hover = if need_relogin {
        i18n::strings().topbar_session_expired.to_owned()
    } else {
        profile.map_or_else(
            || i18n::strings().topbar_not_logged_in.to_owned(),
            |p| p.email.clone(),
        )
    };
    resp.on_hover_text(hover)
}

/// 头像下拉菜单（对齐 Warp 分组样式）：
/// 用户名 ┊ 更新组（更新到 vX / 检查更新 + 更新日志）┊ 设置组（设置 +
/// 键盘快捷键）┊ 资源组（文档 + 反馈）┊ 账号组（登录 / 退出登录）。
fn menu_ui(
    ui: &mut egui::Ui,
    profile: Option<&Profile>,
    pal: &Palette,
    update_version: Option<&str>,
    need_relogin: bool,
    out: &mut TopbarOutput,
) {
    let s = i18n::strings();
    // 强制菜单内容区最小宽度：`Popup::menu().width()` 仅作建议，内容窄时
    // 菜单仍按内容收窄（海风哥反馈菜单太窄即此因），这里硬撑到 MENU_WIDTH。
    ui.set_min_width(MENU_WIDTH);
    // 顶部：已登录展示名（灰字不可点）+ 分隔线。
    if let Some(p) = profile {
        ui.add_enabled(
            false,
            egui::Button::new(egui::RichText::new(&p.display_name).color(pal.fg_dim)),
        );
        ui.separator();
    }
    // 更新组：有就绪更新 →「更新到 vX」(强调色，打开更新弹窗)；否则
    // →「检查更新」(手动检查)。再加「更新日志」。
    if let Some(ver) = update_version {
        if ui
            .button(egui::RichText::new(i18n::fmt1(s.menu_update_to_fmt, ver)).color(pal.accent))
            .clicked()
        {
            out.open_update = true;
            ui.close();
        }
    } else if ui.button(s.menu_check_update).clicked() {
        out.check_update = true;
        ui.close();
    }
    if ui.button(s.menu_whats_new).clicked() {
        out.open_whats_new = true;
        ui.close();
    }
    ui.separator();
    // 设置组。
    if ui.button(s.menu_settings).clicked() {
        out.open_settings = true;
        ui.close();
    }
    if ui.button(s.menu_keyboard_shortcuts).clicked() {
        out.open_shortcuts = true;
        ui.close();
    }
    ui.separator();
    // 资源组：文档 / 反馈（打开 GitHub）。
    if ui.button(s.menu_documentation).clicked() {
        out.open_documentation = true;
        ui.close();
    }
    if ui.button(s.menu_feedback).clicked() {
        out.open_feedback = true;
        ui.close();
    }
    ui.separator();
    // 账号组：登录过期 → 红字「登录过期」（点此重登）置顶；再「登录 / 退出登录」。
    if need_relogin
        && profile.is_some()
        && ui
            .button(egui::RichText::new(s.menu_session_expired).color(pal.error))
            .clicked()
    {
        out.open_login = true;
        ui.close();
    }
    if profile.is_some() {
        if ui.button(s.menu_log_out).clicked() {
            out.log_out = true;
            ui.close();
        }
    } else if ui.button(s.menu_log_in).clicked() {
        out.open_login = true;
        ui.close();
    }
}

#[cfg(test)]
mod topbar_layout_tests {
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

    fn test_view() -> ViewState {
        ViewState {
            update_version: None,
            current_view: false,
            need_relogin: false,
        }
    }

    /// RTL 布局哨兵（fb2610a 修复回归防线）：无头 egui 跑一帧顶栏布局，
    /// 断言窗控按钮分配在面板右端——cursor().min 类手工矩形 bug 重现时此测试失败。
    /// macOS 用原生装饰、不画自绘窗控（见 show 内 cfg），故本测试不适用。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn 顶栏_最大化按钮分配在右端() {
        let ctx = egui::Context::default();
        let pal = test_palette();
        let mut got: Option<egui::Rect> = None;
        let _ = ctx.run_ui(test_input(), |ui| {
            let tb = show(ui, "诊断标题", None, &pal, false, test_view());
            got = Some(tb.maximize_btn_rect.unwrap_or(egui::Rect::NOTHING));
        });
        let r = got.expect("应跑过一帧");
        // 关闭按钮占最右 46px，最大化按钮应在其左：x ∈ [1200-92, 1200-46]
        assert!(
            r.max.x > 1100.0 && r.min.x > 1050.0,
            "最大化按钮不在右端：{r:?}"
        );
        assert!(r.height() > 0.0, "按钮矩形退化：{r:?}");
    }

    /// 绘制哨兵：一帧的绘制图元里右端区域（x>1050）应存在窗控按钮的
    /// 线段图元（✕/—/□ 画线）——按钮绘制被条件分支意外跳过时此测试失败。
    /// macOS 不画自绘窗控，故本测试不适用。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn 顶栏_右端绘制图元存在() {
        let ctx = egui::Context::default();
        let pal = test_palette();
        let full = ctx.run_ui(test_input(), |ui| {
            let _ = show(ui, "诊断标题", None, &pal, false, test_view());
        });
        fn walk(s: &egui::epaint::Shape) -> usize {
            use egui::epaint::Shape;
            match s {
                Shape::LineSegment { points, .. } => {
                    usize::from(points[0].x > 1050.0 || points[1].x > 1050.0)
                }
                Shape::Vec(v) => v.iter().map(walk).sum(),
                _ => 0,
            }
        }
        let segs: usize = full.shapes.iter().map(|cs| walk(&cs.shape)).sum();
        // 窗控三按钮至少 4 条线段（✕ 两条 + — 一条 + □ 矩形另算）
        assert!(segs >= 3, "右端线段图元过少：{segs} 条——按钮没画");
    }

    /// F12 批1 后左端哨兵：三视图按钮已迁工具栏，顶栏左端只剩
    /// 本地/远程 tab——一帧的绘制图元里左端区域（x<250）应存在
    /// 至少 2 个文字图元（「本地」「远程」）。
    #[test]
    fn 顶栏_左端本地远程tab图元存在() {
        let ctx = egui::Context::default();
        let pal = test_palette();
        let full = ctx.run_ui(test_input(), |ui| {
            let _ = show(ui, "标题", None, &pal, false, test_view());
        });
        fn walk_text(s: &egui::epaint::Shape) -> usize {
            use egui::epaint::Shape;
            match s {
                Shape::Text(t) => usize::from(t.pos.x < 250.0),
                Shape::Vec(v) => v.iter().map(walk_text).sum(),
                _ => 0,
            }
        }
        let texts: usize = full.shapes.iter().map(|cs| walk_text(&cs.shape)).sum();
        assert!(
            texts >= 2,
            "左端文字图元过少：{texts} 个——本地/远程 tab 没画到左端"
        );
    }
}
