//! 登录 / 注册界面（M3.5 / M5.1）：全屏覆盖层 + 居中卡片（与设置页同用 Modal）。
//!
//! **M5.1 真账户登录 + 显式注册**：卡片底部「登录 ↔ 注册」切换。
//! - 登录模式：邮箱 + 密码 → 登录；账号不存在提示「请先注册」（**不自动建号**）。
//! - 注册模式：邮箱 + 密码 + 确认密码 → 注册成功后**自动登录**；邮箱已注册提示去登录。
//!
//! 提交后在后台线程发 REST（[`crate::cloud`]），UI 显示进行中 spinner、不阻塞帧；
//! 结果回传后成功则携带 [`Profile`] 关闭。密码仅在提交瞬间过手、绝不落盘。
//!
//! 异步全封装在本模块内：[`show`] 在成功的那一帧才返回 `logged_in`，外层
//! shell / main 的消费路径无需改动。

use std::sync::mpsc::{self, Receiver};

use lumen_protocol::{DeviceInfo, LoginRequest};
use zeroize::{Zeroize, Zeroizing};

use crate::cloud::{self, CloudClient, CloudError};
use crate::i18n;
use crate::profile::{LoginError, Profile};

use super::theme::Palette;

/// 居中卡片宽度（逻辑像素）。
const CARD_WIDTH: f32 = 320.0;
const AUTH_WORKER_START_ERROR: &str = "无法启动登录任务，请稍后重试";

/// 登录 / 注册模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    /// 登录已有账户。
    Login,
    /// 注册新账户（成功后自动登录）。
    Register,
}

/// 后台鉴权失败的结构化原因（UI 线程映射为 i18n 文案）。
enum AuthErr {
    /// 登录：账号不存在（提示去注册）。
    UserNotFound,
    /// 注册：邮箱已注册（提示去登录）。
    EmailTaken,
    /// 登录：邮箱或密码错误。
    BadCredentials,
    /// 网络/其他：已是用户可见文案。
    Other(String),
}

/// 后台鉴权结果：成功 = 档案；失败 = 结构化原因。
type AuthResult = Result<Profile, AuthErr>;

/// 登录界面的跨帧状态。
#[derive(Default)]
pub struct LoginUiState {
    /// 是否打开（打开期间终端聚焦布尔置 false，键盘归 egui）。
    pub open: bool,
    /// 注册模式（false = 登录）。
    register_mode: bool,
    /// 邮箱输入缓冲。
    email: String,
    /// 密码输入缓冲（仅内存，关闭即清空，绝不落盘）。
    password: String,
    /// 确认密码输入缓冲（仅注册模式，仅内存）。
    password2: String,
    /// 本地校验失败枚举（F6）。
    error: Option<LoginError>,
    /// 服务端/网络/本地（密码不一致）错误文案。
    server_error: Option<String>,
    /// 刚打开：本帧把焦点交给邮箱输入框。
    focus_email: bool,
    /// 后台鉴权进行中（输入禁用 + spinner）。
    submitting: bool,
    /// 后台线程结果接收端。
    rx: Option<Receiver<AuthResult>>,
}

impl LoginUiState {
    /// 打开登录界面（清空上次残留，回到登录模式）。
    pub fn open_clean(&mut self) {
        self.reset();
        self.open = true;
        self.focus_email = true;
    }

    /// 本机进入锁屏时关闭登录层并清除所有凭据缓冲；已经发出的后台请求
    /// 允许自然结束，但其结果接收端会在此丢弃，解锁后不会复活旧登录。
    pub fn close_for_app_lock(&mut self) {
        self.reset();
        self.open = false;
    }

    /// 清空输入/错误/在途请求（关闭或成功后调用）。
    fn reset(&mut self) {
        self.register_mode = false;
        self.email.clear();
        self.password.zeroize();
        self.password2.zeroize();
        self.error = None;
        self.server_error = None;
        self.focus_email = false;
        self.submitting = false;
        self.rx = None;
    }
}

impl Drop for LoginUiState {
    fn drop(&mut self) {
        self.password.zeroize();
        self.password2.zeroize();
    }
}

/// 一帧登录界面的产出。
#[derive(Default)]
pub struct LoginOutput {
    /// 覆盖层应关闭（Esc / ✕ / 登录成功）。
    pub closed: bool,
    /// 登录成功的档案（main 写盘并更新全局登录态）。
    pub logged_in: Option<Profile>,
}

/// 绘制登录覆盖层。调用方保证 `st.open == true` 时才调用。
pub fn show(ctx: &egui::Context, st: &mut LoginUiState, pal: &Palette) -> LoginOutput {
    // —— 1. 先收后台结果（借用先于可变操作，避免借用冲突）——
    let polled = if st.submitting {
        st.rx.as_ref().map(Receiver::try_recv)
    } else {
        None
    };
    if let Some(res) = polled {
        match res {
            Ok(Ok(profile)) => {
                st.reset();
                return LoginOutput {
                    closed: true,
                    logged_in: Some(profile),
                };
            }
            Ok(Err(auth_err)) => {
                st.submitting = false;
                st.rx = None;
                let s = i18n::strings();
                st.server_error = Some(match auth_err {
                    AuthErr::UserNotFound => s.login_err_user_not_found.to_string(),
                    AuthErr::EmailTaken => s.login_err_email_taken.to_string(),
                    AuthErr::BadCredentials => s.login_err_bad_credentials.to_string(),
                    AuthErr::Other(m) => m,
                });
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                st.submitting = false;
                st.rx = None;
                st.server_error = Some("鉴权线程异常退出".to_string());
            }
        }
    }

    let mut out = LoginOutput::default();
    let modal = egui::Modal::new(egui::Id::new("lumen_login_modal"))
        .backdrop_color(egui::Color32::from_black_alpha(120))
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            ui.set_width(CARD_WIDTH);

            // 右上关闭：painter 画线 ✕（r=4.5、线宽 1.2，与设置页/顶栏窗控同款）。
            // 不用 RichText 字符，避免字体不含 ✕ 字形导致图标不可控。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let (close_rect, close_resp) =
                    ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
                {
                    let painter = ui.painter();
                    if close_resp.hovered() {
                        painter.rect_filled(
                            close_rect,
                            egui::CornerRadius::same(4),
                            pal.bg_highlight,
                        );
                    }
                    let fg = if close_resp.hovered() { pal.fg } else { pal.fg_dim };
                    let r = 4.5_f32;
                    let c = close_rect.center();
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
                if close_resp.clicked() {
                    out.closed = true;
                }
            });

            let s = i18n::strings();
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Lumen").size(22.0).strong().color(pal.fg));
                ui.add_space(2.0);
                ui.label(egui::RichText::new(s.login_subtitle).size(11.0).color(pal.fg_dim));
            });
            ui.add_space(16.0);

            let enabled = !st.submitting;
            let email_edit = ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut st.email)
                    .hint_text(s.login_email_hint)
                    .desired_width(f32::INFINITY),
            );
            if st.focus_email {
                email_edit.request_focus();
                st.focus_email = false;
            }
            ui.add_space(8.0);
            let pwd_edit = ui
                .add_enabled_ui(enabled, |ui| {
                    super::secure_password_edit(
                        ui,
                        "lumen_login_password",
                        egui::TextEdit::singleline(&mut st.password)
                            .password(true)
                            .hint_text(s.login_password_hint)
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner;

            // 注册模式：确认密码。
            let mut confirm_lost = false;
            if st.register_mode {
                ui.add_space(8.0);
                let c = ui
                    .add_enabled_ui(enabled, |ui| {
                        super::secure_password_edit(
                            ui,
                            "lumen_login_password_confirm",
                            egui::TextEdit::singleline(&mut st.password2)
                                .password(true)
                                .hint_text(s.login_password_confirm_hint)
                                .desired_width(f32::INFINITY),
                        )
                    })
                    .inner;
                confirm_lost = c.lost_focus();
            }
            ui.add_space(14.0);

            // 任一输入框内回车等同点击主按钮。
            let submitted_by_enter = enabled
                && (email_edit.lost_focus() || pwd_edit.lost_focus() || confirm_lost)
                && ui.input(|i| i.key_pressed(egui::Key::Enter));

            let btn_text = if st.register_mode {
                s.login_register_btn
            } else {
                s.login_btn
            };
            let main_btn = egui::Button::new(
                egui::RichText::new(btn_text).size(13.0).color(pal.accent_fg),
            )
            .fill(pal.accent)
            .min_size(egui::vec2(ui.available_width(), 32.0));
            let clicked = ui.add_enabled(enabled, main_btn).clicked();

            if st.submitting {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    let label = if st.register_mode { "注册中…" } else { "登录中…" };
                    ui.label(egui::RichText::new(label).size(11.5).color(pal.fg_dim));
                });
            }

            if (clicked || submitted_by_enter) && !st.submitting {
                start_auth(ctx, st);
            }

            // 错误显示（服务端/本地不一致 优先于本地校验枚举）。
            let err_text = st
                .server_error
                .clone()
                .or_else(|| st.error.map(|e| e.message().to_string()));
            if let Some(e) = err_text {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(e).size(11.5).color(pal.error));
            }

            // 登录 ↔ 注册 切换（提交中隐藏）。
            if !st.submitting {
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    let link = if st.register_mode {
                        s.login_to_login
                    } else {
                        s.login_to_register
                    };
                    if ui
                        .link(egui::RichText::new(link).size(11.0).color(pal.accent))
                        .clicked()
                    {
                        st.register_mode = !st.register_mode;
                        st.password2.clear();
                        st.error = None;
                        st.server_error = None;
                    }
                });
            }
        });

    // Esc / backdrop 点击 → 关闭（提交中也允许关闭：丢弃 rx，后台结果被忽略）。
    if modal.should_close() {
        out.closed = true;
    }
    if out.closed {
        st.reset();
    }
    out
}

/// 本地校验通过后启动后台鉴权线程。
fn start_auth(ctx: &egui::Context, st: &mut LoginUiState) {
    let email = st.email.trim().to_string();
    let password = Zeroizing::new(st.password.clone());
    // 本地校验：邮箱含 `@`（前后段非空）、密码非空。
    let valid_email = email
        .split_once('@')
        .is_some_and(|(u, h)| !u.is_empty() && !h.is_empty());
    if !valid_email {
        st.error = Some(LoginError::InvalidEmail);
        st.server_error = None;
        return;
    }
    if password.is_empty() {
        st.error = Some(LoginError::EmptyPassword);
        st.server_error = None;
        return;
    }
    // 注册模式：两次密码必须一致。
    if st.register_mode && password.as_str() != st.password2 {
        st.error = None;
        st.server_error = Some(i18n::strings().login_err_password_mismatch.to_string());
        return;
    }
    // 在点击/回车提交的主线程时刻捕获并验证 origin。之后即使用户立即把全局地址
    // 从 A 改为 B，此线程里的密码也只可能发往不可变的 A。
    let origin = match capture_auth_origin(&cloud::server_url()) {
        Ok(origin) => origin,
        Err(error) => {
            st.error = None;
            st.server_error = Some(error.user_message());
            return;
        }
    };
    st.error = None;
    st.server_error = None;

    let mode = if st.register_mode {
        AuthMode::Register
    } else {
        AuthMode::Login
    };
    let (tx, rx) = mpsc::channel();
    st.rx = Some(rx);
    st.submitting = true;
    let ctx2 = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("lumen-auth".to_owned())
        .spawn(move || {
            let result = do_auth(mode, &origin, &email, &password);
            // 发送失败（UI 已关闭丢弃 rx）忽略即可。
            let _ = tx.send(result);
            ctx2.request_repaint();
        });
    if spawned.is_err() {
        // spawn 失败会在返回前销毁未运行的闭包，闭包内 `Zeroizing<String>` 密码随即清零。
        rollback_auth_worker_start(st);
    }
}

fn rollback_auth_worker_start(st: &mut LoginUiState) {
    st.submitting = false;
    st.rx = None;
    st.error = None;
    st.server_error = Some(AUTH_WORKER_START_ERROR.to_owned());
}

fn capture_auth_origin(configured_origin: &str) -> Result<String, CloudError> {
    cloud::verified_server_origin(configured_origin)
}

fn should_send_origin_hardware_id(device_id: Option<&str>, legacy_identity: bool) -> bool {
    !(legacy_identity
        && device_id
            .map(str::trim)
            .is_some_and(|value| !value.is_empty()))
}

fn legacy_device_claim_confirmed(candidate: Option<&str>, server_device_id: &str) -> bool {
    candidate.is_some_and(|value| value == server_device_id)
}

/// 后台执行鉴权：注册模式先注册再登录；登录模式直接登录。构造 [`Profile`]。
fn do_auth(mode: AuthMode, origin: &str, email: &str, password: &str) -> AuthResult {
    let client = CloudClient::new(origin.to_owned());
    let previous_profile = Profile::load();
    let previous_origin = previous_profile
        .as_ref()
        .and_then(|profile| profile.auth_origin.as_deref());
    let previous_device_id = previous_profile
        .as_ref()
        .and_then(|profile| profile.device_id.as_deref());
    cloud::reconcile_device_id_for_origin(origin, previous_origin, previous_device_id);
    let persisted_device_id = cloud::load_device_id_for_origin(origin);
    let legacy_device_claimed = cloud::legacy_device_id_claimed_for_origin(origin);
    // v1.0.16 曾误删旧 profile。分区 id 尚不存在时，可先用保留的全局 id
    // 尝试登录；鉴权成功且服务端回传同 id 后才正式认领，错误密码/错账号不落标记。
    let legacy_device_candidate = persisted_device_id
        .is_none()
        .then(|| cloud::legacy_device_id_candidate_for_origin(origin))
        .flatten();
    let device_id = persisted_device_id.or_else(|| legacy_device_candidate.clone());
    let legacy_identity = legacy_device_claimed || legacy_device_candidate.is_some();
    let hw_id = should_send_origin_hardware_id(device_id.as_deref(), legacy_identity)
        .then(|| cloud::hardware_id_for_origin(origin))
        .flatten();
    let device = DeviceInfo {
        // 设备 id 按服务端 origin 分区；旧版全局 id 仅经上方一次性认领进入当前分区。
        device_id,
        // 旧设备行在 v2 服务端仍保存原始 hw_id；若同时发送新的 origin hash，服务端会
        // 误建新设备。已认领且 did 完整时只按 did 复用旧行；其余流程仍只发送 origin hash。
        hw_id,
        name: cloud::device_name(),
        os: std::env::consts::OS.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let req = LoginRequest {
        email: email.to_string(),
        password: password.to_string(),
        device,
    };

    if mode == AuthMode::Register {
        match client.register(email, password) {
            Ok(_) => {}
            Err(CloudError::Api { code, .. }) if code == "email_taken" => {
                return Err(AuthErr::EmailTaken)
            }
            Err(e) => return Err(AuthErr::Other(e.user_message())),
        }
    }

    match client.login(&req) {
        Ok(resp) => {
            if let Err(e) = cloud::save_device_id_for_origin(origin, &resp.device_id) {
                // 写盘失败不阻断本次登录（内存态已生效），但记警告——下次重登会回退 profile
                // 镜像 + hw_id 兜底，不会因此造出幽灵。
                log::warn!("持久化 device_id 失败: {e}");
            }
            if legacy_device_claim_confirmed(
                legacy_device_candidate.as_deref(),
                &resp.device_id,
            ) {
                cloud::claim_legacy_device_id_for_origin(
                    origin,
                    legacy_device_candidate.as_deref(),
                );
            }
            Ok(Profile::from_auth(resp, origin.to_owned()))
        }
        Err(CloudError::Api { code, .. }) if code == "user_not_found" => Err(AuthErr::UserNotFound),
        Err(CloudError::Api { code, .. }) if code == "invalid_credentials" => {
            Err(AuthErr::BadCredentials)
        }
        Err(e) => Err(AuthErr::Other(e.user_message())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 鉴权提交捕获不可变origin() {
        let captured = capture_auth_origin("https://a.example.com").expect("捕获 A");
        let later = capture_auth_origin("https://b.example.com").expect("捕获 B");
        assert_eq!(captured, "https://a.example.com");
        assert_ne!(captured, later);

        // 编译期回归：后台鉴权入口必须显式接收 origin，不能自行读取全局地址。
        let _requires_explicit_origin: fn(AuthMode, &str, &str, &str) -> AuthResult = do_auth;
    }

    #[test]
    fn 鉴权worker启动失败恢复提交状态() {
        let (_tx, rx) = mpsc::channel();
        let mut state = LoginUiState::default();
        state.submitting = true;
        state.rx = Some(rx);
        state.server_error = Some("old".to_owned());

        rollback_auth_worker_start(&mut state);

        assert!(!state.submitting);
        assert!(state.rx.is_none());
        assert_eq!(state.server_error.as_deref(), Some(AUTH_WORKER_START_ERROR));
    }

    #[test]
    fn 旧设备认领后优先按did复用v2服务端设备行() {
        assert!(!should_send_origin_hardware_id(
            Some("legacy-device"),
            true
        ));
        assert!(should_send_origin_hardware_id(None, true));
        assert!(should_send_origin_hardware_id(Some(""), true));
        assert!(should_send_origin_hardware_id(
            Some("current-device"),
            false
        ));
        assert!(legacy_device_claim_confirmed(
            Some("legacy-device"),
            "legacy-device"
        ));
        assert!(!legacy_device_claim_confirmed(
            Some("legacy-device"),
            "new-account-device"
        ));
        assert!(!legacy_device_claim_confirmed(None, "legacy-device"));
    }
}
