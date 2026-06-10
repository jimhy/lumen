//! 应用外壳 UI（egui）：侧栏 + 终端工作区布局。
//!
//! M3.2 起侧栏是真功能的会话 tab 列表：条目（标题 + 未读点 + 激活
//! 高亮）点击切换、右键菜单重命名/关闭、底部新建。UI 只产出动作
//! （[`ShellOutput`]），会话增删切换由 main.rs 执行。

pub mod theme;

/// 左侧会话栏宽度（逻辑像素）。
pub const SIDEBAR_WIDTH: f32 = 180.0;

/// 一条会话在侧栏的展示数据（由 main.rs 按帧构造）。
pub struct SessionEntry {
    pub id: u64,
    /// 展示标题（自定义名 > OSC 标题 > 默认名，已做空回退）。
    pub title: String,
    pub active: bool,
    /// 后台期间有未读输出（条目右侧小圆点）。
    pub unseen: bool,
}

/// 跨帧保留的外壳 UI 状态。
#[derive(Default)]
pub struct ShellState {
    /// 进行中的重命名：(会话 id, 编辑中文本)。编辑期间键盘归 egui。
    pub renaming: Option<(u64, String)>,
    /// 重命名刚开始，下一帧把焦点交给编辑框。
    rename_focus: bool,
}

/// 一帧外壳 UI 的产出。
pub struct ShellOutput {
    /// 终端工作区矩形（egui 逻辑点坐标）。
    pub term_rect: egui::Rect,
    /// 本帧用户点击了终端区（焦点交还终端）。
    pub term_clicked: bool,
    /// 点击了某会话条目（切换激活）。
    pub activate: Option<u64>,
    /// 请求关闭某会话（右键菜单）。
    pub close: Option<u64>,
    /// 提交的重命名：(会话 id, 新名字)。空字符串 = 清除自定义名。
    pub rename: Option<(u64, String)>,
    /// 点击了「新建会话」。
    pub new_session: bool,
}

/// 绘制整个外壳：左侧会话栏 + 中央终端纹理。
pub fn show(
    root: &mut egui::Ui,
    term_tex: egui::TextureId,
    sessions: &[SessionEntry],
    st: &mut ShellState,
) -> ShellOutput {
    let mut out = ShellOutput {
        term_rect: egui::Rect::NOTHING,
        term_clicked: false,
        activate: None,
        close: None,
        rename: None,
        new_session: false,
    };
    // 重命名目标可能已被关闭（进程退出等）：清掉孤儿编辑态，
    // 否则编辑框永不渲染、也永不失焦，键盘焦点会卡在 egui 侧。
    if st
        .renaming
        .as_ref()
        .is_some_and(|(id, _)| !sessions.iter().any(|e| e.id == *id))
    {
        st.renaming = None;
    }

    egui::Panel::left("lumen_sidebar")
        .exact_size(SIDEBAR_WIDTH)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(theme::BG_DARK)
                .inner_margin(egui::Margin::symmetric(8, 10)),
        )
        .show_inside(root, |ui| sidebar_ui(ui, sessions, st, &mut out));

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show_inside(root, |ui| {
            let rect = ui.available_rect_before_wrap();
            ui.put(
                rect,
                egui::Image::new(egui::load::SizedTexture::new(term_tex, rect.size())),
            );
            // 点击终端区 → 焦点交还终端。选区/块点击/滚轮仍走
            // window_event 按终端区矩形路由（见 main.rs）。
            let resp = ui.interact(rect, ui.id().with("terminal_area"), egui::Sense::click());
            out.term_clicked = resp.clicked();
            out.term_rect = rect;
        });
    out
}

/// 侧栏内容：会话条目列表 + 底部新建按钮。
fn sidebar_ui(
    ui: &mut egui::Ui,
    sessions: &[SessionEntry],
    st: &mut ShellState,
    out: &mut ShellOutput,
) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new("会话").size(11.0).color(theme::FG_DIM));
    ui.add_space(4.0);

    for entry in sessions {
        // 重命名中的条目：行内编辑框替代按钮。Enter 提交、Esc 或
        // 点击别处取消（egui 的 TextEdit 在这三种情况都会失焦）。
        let is_renaming = st.renaming.as_ref().is_some_and(|(id, _)| *id == entry.id);
        if is_renaming {
            if let Some((_, buf)) = st.renaming.as_mut() {
                let resp = ui.add(
                    egui::TextEdit::singleline(buf).desired_width(f32::INFINITY),
                );
                if st.rename_focus {
                    resp.request_focus();
                    st.rename_focus = false;
                }
                if resp.lost_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        out.rename = Some((entry.id, buf.trim().to_owned()));
                    }
                    st.renaming = None;
                }
            }
            continue;
        }

        let fill = if entry.active {
            theme::BG_HIGHLIGHT
        } else {
            egui::Color32::TRANSPARENT
        };
        let btn = egui::Button::new(
            egui::RichText::new(format!("● {}", entry.title)).color(theme::FG),
        )
        .fill(fill)
        .wrap_mode(egui::TextWrapMode::Truncate)
        .min_size(egui::vec2(ui.available_width(), 30.0));
        let resp = ui.add(btn);
        if resp.clicked() {
            out.activate = Some(entry.id);
        }
        resp.context_menu(|ui| {
            if ui.button("重命名").clicked() {
                st.renaming = Some((entry.id, entry.title.clone()));
                st.rename_focus = true;
                ui.close();
            }
            if ui.button("关闭").clicked() {
                out.close = Some(entry.id);
                ui.close();
            }
        });
        // 未读小圆点（后台有新输出，切换到该 tab 时清除）。
        if entry.unseen {
            let center = egui::pos2(resp.rect.right() - 10.0, resp.rect.center().y);
            ui.painter().circle_filled(center, 3.0, theme::ACCENT);
        }
    }

    // 底部「＋」新建会话（继承当前 shell 配置，见 Session::spawn）。
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
        ui.add_space(2.0);
        let plus = egui::Button::new(egui::RichText::new("＋ 新建会话").color(theme::FG_DIM))
            .min_size(egui::vec2(ui.available_width(), 28.0));
        if ui.add(plus).clicked() {
            out.new_session = true;
        }
    });
}
