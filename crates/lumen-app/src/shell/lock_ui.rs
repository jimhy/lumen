//! 应用锁的独立全屏 UI。
//!
//! 本模块只负责绘制和产出动作，不读取任何终端、账户、设备或会话数据。锁屏使用
//! 完全不透明的全窗底色；唯一允许展示的运行态是泛化的“已授权远程控制仍在进行”，
//! 不包含对端名称、设备、会话或终端内容。

use std::time::Duration;

use zeroize::Zeroize;

use crate::i18n;

use super::theme::Palette;

/// 锁屏卡片宽度（逻辑像素）。
const CARD_WIDTH: f32 = 360.0;
/// 锁屏卡片基础高度（逻辑像素）。
const CARD_HEIGHT: f32 = 390.0;

/// 锁屏输入错误。存储错误由 [`LockUiInput::storage_error`] 提供，因为它属于持久化状态，
/// 而非一次密码提交的 UI 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockUiError {
    /// 密码校验失败。
    WrongPassword,
}

/// 锁屏的跨帧 UI 状态。
///
/// 密码仅存在于该缓冲和提交后的 [`LockUiOutput::unlock_password`] 中；调用 [`Self::clear`]
/// 或丢弃状态时会用 [`Zeroize::zeroize`] 覆写缓冲。
pub struct LockUiState {
    password: String,
    password_visible: bool,
    focus_password: bool,
    error: Option<LockUiError>,
}

impl Default for LockUiState {
    fn default() -> Self {
        Self {
            password: String::new(),
            password_visible: false,
            focus_password: true,
            error: None,
        }
    }
}

impl Drop for LockUiState {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl LockUiState {
    /// 清空所有敏感输入并恢复首次进入锁屏的 UI 状态。
    pub fn clear(&mut self) {
        self.password.zeroize();
        self.password_visible = false;
        self.focus_password = true;
        self.error = None;
    }

    /// 设置一次密码校验错误；旧输入先安全清空，下一帧重新聚焦密码框。
    pub fn set_error(&mut self, error: LockUiError) {
        self.password.zeroize();
        self.focus_password = true;
        self.error = Some(error);
    }

    /// 清除错误提示，不改变当前密码输入。
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Esc 只清空本次输入/错误，保留用户选择的密码显隐状态。
    fn clear_entry(&mut self) {
        self.password.zeroize();
        self.focus_password = true;
        self.error = None;
    }

    /// 把密码所有权移交给业务层；状态自身立即回到空缓冲。
    fn take_password(&mut self) -> Option<String> {
        if self.password.is_empty() {
            return None;
        }
        self.error = None;
        self.focus_password = false;
        Some(std::mem::take(&mut self.password))
    }
}

/// 锁屏一帧的只读输入。所有字段均为泛化状态，不得携带业务内容。
#[derive(Debug, Clone, Copy, Default)]
pub struct LockUiInput {
    /// 后台正在校验密码。
    pub busy: bool,
    /// 错误次数退避剩余时长；非零时禁用输入与提交。
    pub retry_remaining: Duration,
    /// 已授权远程控制仍在继续；只显示固定泛化文案。
    pub remote_active: bool,
    /// 当前 Caps Lock 状态。
    pub caps_lock: bool,
    /// 应用锁配置不可读/损坏；失败关闭，禁止尝试解锁并显示固定错误。
    pub storage_error: bool,
}

/// 锁屏一帧产出的业务动作。
#[derive(Default)]
pub struct LockUiOutput {
    /// 请求校验的密码。接收方应立即移入 `Zeroizing<String>`，不得记录或持久化明文。
    pub unlock_password: Option<String>,
    /// 请求最小化窗口。
    pub minimize: bool,
    /// 请求关闭窗口。
    pub close: bool,
}

/// 绘制完全不透明的全屏应用锁界面。
///
/// 调用方在锁定期间应以本函数替代普通 shell 绘制，并继续在后台泵 PTY 与已授权远程控制。
/// 本函数不会因 Esc 或点击背景关闭；Esc 仅安全清空密码输入。
pub fn show(
    root: &mut egui::Ui,
    st: &mut LockUiState,
    input: LockUiInput,
    pal: &Palette,
) -> LockUiOutput {
    if root.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
        st.clear_entry();
    }

    let mut out = LockUiOutput::default();
    let ctx = root.ctx().clone();
    let screen = ctx.content_rect();
    let opaque_bg = opaque(pal.bg_dark);
    let opaque_panel = opaque(pal.bg_panel);

    // Modal 负责吞掉下层点击/键盘；backdrop 与满窗 frame 都使用不透明实色，
    // 即使调用方误在普通业务 UI 之后绘制，也不会透出任何业务内容。
    egui::Modal::new(egui::Id::new("lumen_lock_screen"))
        .backdrop_color(opaque_bg)
        .frame(egui::Frame::new().fill(opaque_bg))
        .show(&ctx, |ui| {
            ui.set_min_size(screen.size());
            let full = ui.min_rect();
            ui.painter().rect_filled(full, 0.0, opaque_bg);

            window_controls(ui, full, pal, &mut out);

            let card_width = CARD_WIDTH.min((full.width() - 32.0).max(1.0));
            let card_height = CARD_HEIGHT.min((full.height() - 64.0).max(1.0));
            let card =
                egui::Rect::from_center_size(full.center(), egui::vec2(card_width, card_height));
            ui.painter().rect(
                card,
                egui::CornerRadius::same(12),
                opaque_panel,
                egui::Stroke::new(1.0_f32, pal.panel_outline),
                egui::StrokeKind::Inside,
            );

            // 极小窗口下按卡片尺寸收缩边距，避免固定 28/24 造成反向矩形。
            let content = card.shrink2(egui::vec2(
                (card.width() * 0.08).clamp(4.0, 28.0),
                (card.height() * 0.06).clamp(4.0, 24.0),
            ));
            let mut content_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(content)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            lock_card(&mut content_ui, st, input, pal, &mut out);
        });

    out
}

/// 锁屏中央卡片。这里只使用固定 i18n 文案和布尔/倒计时状态。
fn lock_card(
    ui: &mut egui::Ui,
    st: &mut LockUiState,
    input: LockUiInput,
    pal: &Palette,
    out: &mut LockUiOutput,
) {
    let s = i18n::strings();
    ui.set_width(ui.max_rect().width());

    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new("Lumen")
                .size(26.0)
                .strong()
                .color(pal.fg),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(s.lock_screen_title)
                .size(16.0)
                .strong()
                .color(pal.fg),
        );
    });
    ui.add_space(26.0);

    let retrying = !input.retry_remaining.is_zero();
    let editable = !input.busy && !retrying && !input.storage_error;
    let edit = ui
        .add_enabled_ui(editable, |ui| {
            super::secure_password_edit(
                ui,
                "lumen_lock_password",
                egui::TextEdit::singleline(&mut st.password)
                    .password(!st.password_visible)
                    .char_limit(128)
                    .hint_text(s.lock_screen_password_hint)
                    .desired_width(f32::INFINITY),
            )
        })
        .inner;
    if st.focus_password && editable {
        edit.request_focus();
        st.focus_password = false;
    }
    if edit.changed() {
        st.error = None;
    }
    let submitted_by_enter = editable
        && (edit.has_focus() || edit.lost_focus())
        && ui.input(|i| i.key_pressed(egui::Key::Enter));

    ui.add_space(4.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let label = if st.password_visible {
            s.lock_screen_hide_password
        } else {
            s.lock_screen_show_password
        };
        if ui.add_enabled(editable, egui::Button::new(label)).clicked() {
            st.password_visible = !st.password_visible;
            edit.request_focus();
        }
    });

    ui.add_space(12.0);
    if input.storage_error {
        status_label(ui, s.lock_screen_storage_error, pal.error);
    } else if retrying {
        status_label(
            ui,
            &i18n::fmt1(
                s.lock_screen_retry_fmt,
                retry_seconds(input.retry_remaining),
            ),
            pal.error,
        );
        // 秒级倒计时需要持续刷新，不依赖外部业务事件唤醒。
        ui.ctx().request_repaint_after(Duration::from_millis(200));
    } else if input.busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new(s.lock_screen_verifying)
                    .size(11.5)
                    .color(pal.fg_dim),
            );
        });
        ui.ctx().request_repaint();
    } else if st.error == Some(LockUiError::WrongPassword) {
        status_label(ui, s.lock_screen_wrong_password, pal.error);
    }

    if input.caps_lock {
        ui.add_space(6.0);
        status_label(ui, s.lock_screen_caps_lock, pal.warn);
    }
    if input.remote_active {
        ui.add_space(8.0);
        status_label(ui, s.lock_screen_remote_active, pal.info);
    }

    ui.add_space(18.0);
    let button_text = if input.busy {
        s.lock_screen_verifying
    } else {
        s.lock_screen_unlock
    };
    let unlock = egui::Button::new(
        egui::RichText::new(button_text)
            .size(13.0)
            .color(pal.accent_fg),
    )
    .fill(pal.accent)
    .min_size(egui::vec2(ui.available_width(), 34.0));
    let can_submit = editable && !st.password.is_empty();
    let clicked = ui.add_enabled(can_submit, unlock).clicked();
    if (clicked || submitted_by_enter) && can_submit {
        out.unlock_password = st.take_password();
    }
}

/// 固定文案状态行。
fn status_label(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.label(egui::RichText::new(text).size(11.5).color(color));
}

/// 锁屏仅保留最小化与关闭，避免进入普通标题栏或暴露其业务状态。
fn window_controls(ui: &mut egui::Ui, full: egui::Rect, pal: &Palette, out: &mut LockUiOutput) {
    let button_size = egui::vec2(38.0, 32.0);
    let close_rect = egui::Rect::from_min_size(
        egui::pos2(full.right() - button_size.x, full.top()),
        button_size,
    );
    let minimize_rect = close_rect.translate(egui::vec2(-button_size.x, 0.0));

    let minimize = ui.allocate_rect(minimize_rect, egui::Sense::click());
    if minimize.hovered() {
        ui.painter()
            .rect_filled(minimize_rect, 0.0, pal.bg_highlight);
    }
    ui.painter().hline(
        (minimize_rect.center().x - 5.0)..=(minimize_rect.center().x + 5.0),
        minimize_rect.center().y,
        egui::Stroke::new(1.4_f32, pal.fg),
    );
    if minimize
        .on_hover_text(i18n::strings().wc_minimize)
        .clicked()
    {
        out.minimize = true;
    }

    let close = ui.allocate_rect(close_rect, egui::Sense::click());
    if close.hovered() {
        ui.painter()
            .rect_filled(close_rect, 0.0, egui::Color32::from_rgb(0xc4, 0x2b, 0x1c));
    }
    let close_color = if close.hovered() {
        egui::Color32::WHITE
    } else {
        pal.fg
    };
    let center = close_rect.center();
    let radius = 4.5;
    let stroke = egui::Stroke::new(1.2_f32, close_color);
    ui.painter().line_segment(
        [
            center + egui::vec2(-radius, -radius),
            center + egui::vec2(radius, radius),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            center + egui::vec2(-radius, radius),
            center + egui::vec2(radius, -radius),
        ],
        stroke,
    );
    if close.on_hover_text(i18n::strings().wc_close).clicked() {
        out.close = true;
    }
}

/// Color32 可能来自未来自定义主题；锁屏强制丢弃 alpha，保证绝不透底。
fn opaque(color: egui::Color32) -> egui::Color32 {
    let [r, g, b, _] = color.to_srgba_unmultiplied();
    egui::Color32::from_rgb(r, g, b)
}

/// 倒计时向上取整，避免尚余不足一秒时显示“0 秒”却仍禁用。
fn retry_seconds(remaining: Duration) -> u64 {
    remaining
        .as_secs()
        .saturating_add(u64::from(remaining.subsec_nanos() > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_清空敏感状态并恢复首次聚焦() {
        let mut state = LockUiState {
            password: "敏感密码 123!".to_owned(),
            password_visible: true,
            focus_password: false,
            error: Some(LockUiError::WrongPassword),
        };
        state.clear();
        assert!(state.password.is_empty());
        assert!(!state.password_visible);
        assert!(state.focus_password);
        assert_eq!(state.error, None);
    }

    #[test]
    fn 密码错误会安全清空并等待重新聚焦() {
        let mut state = LockUiState {
            password: "wrong-password".to_owned(),
            password_visible: false,
            focus_password: false,
            error: None,
        };
        state.set_error(LockUiError::WrongPassword);
        assert!(state.password.is_empty());
        assert!(state.focus_password);
        assert_eq!(state.error, Some(LockUiError::WrongPassword));
    }

    #[test]
    fn 提交后状态不保留密码() {
        let mut state = LockUiState {
            password: "correct horse battery staple".to_owned(),
            password_visible: false,
            focus_password: true,
            error: Some(LockUiError::WrongPassword),
        };
        let password = state.take_password().expect("应产出密码");
        assert_eq!(password, "correct horse battery staple");
        assert!(state.password.is_empty());
        assert_eq!(state.error, None);
        assert!(!state.focus_password);
    }

    #[test]
    fn 重试秒数向上取整() {
        assert_eq!(retry_seconds(Duration::ZERO), 0);
        assert_eq!(retry_seconds(Duration::from_millis(1)), 1);
        assert_eq!(retry_seconds(Duration::from_secs(1)), 1);
        assert_eq!(retry_seconds(Duration::from_millis(1_001)), 2);
    }

    #[test]
    fn 锁屏颜色强制不透明() {
        // 选可被 8-bit 预乘表示精确还原的分量；极低 alpha 下颜色信息
        // 本就会量化丢失，不影响“输出 alpha 必须为 255”的安全属性。
        let color = opaque(egui::Color32::from_rgba_unmultiplied(128, 64, 32, 128));
        assert_eq!(color, egui::Color32::from_rgb(128, 64, 32));
        assert_eq!(color.a(), 255);
    }
}
