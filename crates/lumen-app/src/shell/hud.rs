//! LLM CLI HUD：焦点窗格运行受支持的 LLM 时，锚定窗格右下角展示。
//!
//! 信息架构参考 claude-hud Full：项目/Git、模型/状态、上下文、额度、
//! token、工具、子代理、任务、配置与会话时间。终端画面提供即时核心
//! 数据，transcript/Git 等较重采集在后台线程完成。

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::theme::Palette;
use crate::llm_cli::{HudContext, HudContextKind, HudUsageWindow, LlmCliKind};
use crate::llm_hud::{
    CliNativeDetails, CollectRequest, ConfigCounts, GitStatus, HudDetails, ToolSummary,
};

const MARGIN: f32 = 12.0;
const DEFAULT_WIDTH: f32 = 460.0;
const DEFAULT_HEIGHT: f32 = 390.0;
// 窄窗格下允许内容自行折行并由滚动区承载，不能用桌面宽卡片的固定下限
// 阻止用户继续缩小。
const MIN_WIDTH: f32 = 220.0;
const MIN_HEIGHT: f32 = 160.0;
const MAX_WIDTH: f32 = 920.0;
const MAX_HEIGHT: f32 = 760.0;
const COLLAPSED_WIDTH: f32 = 54.0;
const COLLAPSED_HEIGHT: f32 = 28.0;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// main.rs 按帧提供的 HUD 即时数据。
pub struct HudView {
    pub session_id: u64,
    pub kind: LlmCliKind,
    pub model: Option<String>,
    pub context: Option<HudContext>,
    /// 从状态行刮到的额度窗口。Claude 只有这一条通路。
    pub usage: Vec<HudUsageWindow>,
    pub project_path: Option<String>,
    pub foreground_pid: Option<u32>,
    pub busy: bool,
    pub session_elapsed: Duration,
    /// 焦点窗格底部输入框高度（egui 逻辑像素）。
    pub bottom_inset: f32,
}

/// HUD 的跨帧开关、异步采集与缓存状态。
pub struct HudState {
    session_id: Option<u64>,
    open: bool,
    /// 用户**显式**点过关闭的会话。
    ///
    /// # 为什么必须单独记一份，而不是只靠 [`Self::open`]
    /// [`HudState::sync_session`] 在 `session_id` 变化时会把 `open` 重置成默认值，
    /// 而「切换视图」正好会让它变两次：焦点挪到别的窗格 ⇒ `view` 变成 `None` ⇒
    /// `session_id` 变 `None`；切回来 ⇒ 又变回 `Some(id)`。老实现在这一步无条件
    /// `open = next.is_some()`，于是**用户关掉的 HUD 每切一次视图就自己弹回来一次**
    /// （海风哥 2026-08-11 报）。
    ///
    /// 关闭是用户的显式意图，它必须活得比「焦点在哪个窗格」更久；而「新会话第一次
    /// 显示 HUD」也仍然是产品意图——两者的分界正是这份集合：**只有被显式关过的会话
    /// 才默认收起**。
    ///
    /// # 不做清理的取舍
    /// 会话 id 单调递增且**不复用**（`session::SessionId` 的不变量），所以残留条目
    /// 不会误伤任何新会话，只是白占内存——一条 8 字节，反复开关上万次也才几十 KB。
    /// 为它引一套「会话已消失」的通知反而要在 `main.rs` 与这里之间多一条耦合。
    dismissed: std::collections::HashSet<u64>,
    details: Option<HudDetails>,
    details_rx: Option<mpsc::Receiver<HudDetails>>,
    last_refresh: Option<Instant>,
    size: egui::Vec2,
    resize_start: Option<(egui::Vec2, egui::Pos2)>,
}

impl Default for HudState {
    fn default() -> Self {
        Self {
            session_id: None,
            open: false,
            dismissed: std::collections::HashSet::new(),
            details: None,
            details_rx: None,
            last_refresh: None,
            size: egui::vec2(DEFAULT_WIDTH, DEFAULT_HEIGHT),
            resize_start: None,
        }
    }
}

impl HudState {
    /// HUD 正在拖动尺寸时保持鼠标捕获，避免指针暂时越出面板后事件穿透到终端。
    pub fn captures_pointer(&self) -> bool {
        self.resize_start.is_some()
    }

    fn sync_session(&mut self, view: Option<&HudView>) {
        let next = view.map(|view| view.session_id);
        if next != self.session_id {
            self.session_id = next;
            // 默认展开，但**用户关过的会话保持收起**——切换视图会让 session_id 变两次
            // （挪走变 None、切回变 Some），不查这一份记忆就会每切一次弹一次。
            self.open = next.is_some_and(|id| !self.dismissed.contains(&id));
            self.details = None;
            self.details_rx = None;
            self.last_refresh = None;
            self.resize_start = None;
        }
    }

    fn poll_details(&mut self, ctx: &egui::Context, view: &HudView) {
        let mut disconnected = false;
        if let Some(rx) = &self.details_rx {
            match rx.try_recv() {
                Ok(details) => {
                    if details.session_id == view.session_id {
                        self.details = Some(details);
                    }
                    self.details_rx = None;
                    self.last_refresh = Some(Instant::now());
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => disconnected = true,
            }
        }
        if disconnected {
            self.details_rx = None;
            self.last_refresh = Some(Instant::now());
        }

        let due = self
            .last_refresh
            .is_none_or(|last| last.elapsed() >= REFRESH_INTERVAL);
        if self.open && self.details_rx.is_none() && due {
            let request = CollectRequest {
                session_id: view.session_id,
                kind: view.kind,
                cwd: view.project_path.as_deref().map(PathBuf::from),
                foreground_pid: view.foreground_pid,
                previous: self.details.clone(),
            };
            let (tx, rx) = mpsc::sync_channel(1);
            self.details_rx = Some(rx);
            self.last_refresh = Some(Instant::now());
            let _ = std::thread::Builder::new()
                .name(format!("lumen-hud-{}", view.session_id))
                .spawn(move || {
                    let _ = tx.send(crate::llm_hud::collect(request));
                });
        }

        // 会话时间、活动动画和后台采集结果都需要周期刷新，但不制造
        // 全速重绘循环。
        if self.open {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// 绘制 HUD。`pane_rect` 是焦点窗格的终端内容矩形。
pub fn show(
    ctx: &egui::Context,
    state: &mut HudState,
    view: Option<&HudView>,
    pane_rect: egui::Rect,
    pal: &Palette,
    blocked: bool,
) {
    state.sync_session(view);
    let Some(view) = view else {
        return;
    };
    state.poll_details(ctx, view);
    if blocked || !pane_rect.is_positive() || pane_rect.width() < 120.0 {
        return;
    }

    let bottom = (pane_rect.bottom() - view.bottom_inset - MARGIN)
        .max(pane_rect.top() + COLLAPSED_HEIGHT + MARGIN);
    let right = pane_rect.right() - MARGIN;

    if !state.open {
        collapsed_button(ctx, state, view, right, bottom, pal);
        return;
    }

    let max_width = (pane_rect.width() - MARGIN * 2.0).max(120.0);
    let max_height = (bottom - pane_rect.top() - MARGIN).max(120.0);
    let width = state.size.x.clamp(MIN_WIDTH.min(max_width), max_width);
    let height = state.size.y.clamp(MIN_HEIGHT.min(max_height), max_height);
    let pos = egui::pos2(
        right - width,
        (bottom - height).max(pane_rect.top() + MARGIN),
    );
    let details = state.details.clone();
    let details = details.as_ref();

    egui::Area::new(egui::Id::new(("lumen_llm_hud", view.session_id)))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(pal.bg_panel)
                .stroke(egui::Stroke::new(1.0_f32, pal.panel_outline))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    let inner_size = egui::vec2(width - 20.0, height - 20.0);
                    ui.set_min_size(inner_size);
                    ui.set_max_size(inner_size);
                    header(ui, state, view, details, egui::vec2(width, height), pal);
                    separator(ui, pal);
                    let content_height = ui.available_height().max(72.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("llm_hud_scroll", view.session_id))
                        .max_height(content_height)
                        .min_scrolled_height(content_height)
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                        )
                        .show(ui, |ui| {
                            ui.set_width((inner_size.x - 4.0).max(100.0));
                            project_line(ui, view, details, pal);
                            ui.add_space(8.0);
                            context_section(ui, view, details, pal);
                            usage_section(ui, view, details, pal);
                            if let Some(details) = details {
                                token_section(ui, details, pal);
                                tool_section(ui, details, pal);
                                agent_section(ui, details, pal);
                                todo_section(ui, details, pal);
                                native_section(ui, view.kind, details, pal);
                                metadata_section(ui, details, pal);
                            } else {
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(
                                        egui::RichText::new(crate::i18n::strings().hud_loading)
                                            .size(10.5)
                                            .color(pal.fg_dim),
                                    );
                                });
                            }
                        });
                });
        });
}

fn collapsed_button(
    ctx: &egui::Context,
    state: &mut HudState,
    view: &HudView,
    right: f32,
    bottom: f32,
    pal: &Palette,
) {
    let pos = egui::pos2(right - COLLAPSED_WIDTH, bottom - COLLAPSED_HEIGHT);
    let response = egui::Area::new(egui::Id::new(("lumen_llm_hud_collapsed", view.session_id)))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(COLLAPSED_WIDTH, COLLAPSED_HEIGHT),
                egui::Sense::CLICK,
            );
            let fill = if response.hovered() {
                pal.bg_highlight
            } else {
                pal.bg_panel
            };
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(6), fill);
            ui.painter().rect_stroke(
                rect,
                egui::CornerRadius::same(6),
                egui::Stroke::new(1.0_f32, pal.panel_outline),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "HUD",
                egui::FontId::proportional(11.0),
                if response.hovered() {
                    pal.fg
                } else {
                    pal.fg_dim
                },
            );
            response
        })
        .inner;
    if response.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if response
        .on_hover_text(crate::i18n::strings().hud_open_tip)
        .clicked()
    {
        state.open = true;
        // 重新点开 = 撤销上次的关闭意图，之后切换视图要跟着保持展开。
        state.dismissed.remove(&view.session_id);
        state.last_refresh = None;
    }
}

fn header(
    ui: &mut egui::Ui,
    state: &mut HudState,
    view: &HudView,
    details: Option<&HudDetails>,
    displayed_size: egui::Vec2,
    pal: &Palette,
) {
    // 头部占满整行，并把关闭按钮钉在面板右边。原先把 right-to-left
    // 布局嵌进自然宽度 horizontal，面板变窄时关闭按钮会跟内容一起向左漂。
    let header_width = ui.available_width();
    let (header_rect, _) =
        ui.allocate_exact_size(egui::vec2(header_width, 20.0), egui::Sense::hover());
    let close_rect = egui::Rect::from_min_size(
        egui::pos2(header_rect.right() - 20.0, header_rect.top()),
        egui::vec2(20.0, 20.0),
    );
    let content_rect = egui::Rect::from_min_max(
        header_rect.min,
        egui::pos2(
            (close_rect.left() - 4.0).max(header_rect.left()),
            header_rect.bottom(),
        ),
    );
    let mut content_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(content_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    resize_handle(&mut content_ui, state, displayed_size, pal);
    {
        let status_color = if view.busy { pal.warn } else { pal.success };
        content_ui.painter().circle_filled(
            content_ui.cursor().min + egui::vec2(4.0, 9.0),
            3.0,
            status_color,
        );
        content_ui.add_space(10.0);
        content_ui.label(egui::RichText::new("HUD").strong().size(12.0).color(pal.fg));
        content_ui.add_space(5.0);
        content_ui.add(
            egui::Label::new(
                egui::RichText::new(view.kind.display_name())
                    .size(10.0)
                    .color(pal.fg_dim),
            )
            .truncate(),
        );
    }
    let close = ui
        .put(
            close_rect,
            egui::Button::new(egui::RichText::new("×").size(15.0).color(pal.fg_dim))
                .frame(false)
                .sense(egui::Sense::CLICK),
        )
        .on_hover_text(crate::i18n::strings().hud_close_tip);
    if close.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if close.clicked() {
        state.open = false;
        // 记住这次关闭：否则切走窗格再切回来，sync_session 会把它当成「新会话」重新展开。
        state.dismissed.insert(view.session_id);
    }
    ui.add_space(3.0);
    ui.horizontal_wrapped(|ui| {
        let s = crate::i18n::strings();
        let status = if view.busy {
            s.hud_status_working
        } else {
            s.hud_status_ready
        };
        ui.label(egui::RichText::new(status).size(10.0).color(if view.busy {
            pal.warn
        } else {
            pal.success
        }));
        dot(ui, pal);
        let elapsed = details
            .and_then(|details| details.session_started_ms)
            .and_then(elapsed_since_ms)
            .unwrap_or(view.session_elapsed);
        ui.label(
            egui::RichText::new(format!("{} {}", s.hud_session, format_duration(elapsed)))
                .monospace()
                .size(9.5)
                .color(pal.fg_dim),
        );
        let model = view
            .model
            .as_deref()
            .or_else(|| details.and_then(|details| details.model.as_deref()));
        if let Some(model) = model {
            dot(ui, pal);
            badge(ui, model, pal.accent, pal.accent_fg);
        }
        if let Some(effort) = details.and_then(|details| details.effort.as_deref()) {
            badge(ui, effort, pal.btn_bg, pal.fg_dim);
        }
        if let Some(name) = details.and_then(|details| details.session_name.as_deref()) {
            dot(ui, pal);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(name)
                        .monospace()
                        .size(9.5)
                        .color(pal.fg_dim),
                )
                .truncate(),
            )
            .on_hover_text(name);
        }
    });
}

fn resize_handle(
    ui: &mut egui::Ui,
    state: &mut HudState,
    displayed_size: egui::Vec2,
    pal: &Palette,
) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(13.0, 17.0), egui::Sense::drag());
    let stroke = egui::Stroke::new(
        1.0_f32,
        if response.hovered() || response.dragged() {
            pal.accent
        } else {
            pal.fg_dim
        },
    );
    let corner = rect.left_top() + egui::vec2(2.0, 2.0);
    ui.painter().line_segment(
        [corner + egui::vec2(0.0, 8.0), corner + egui::vec2(8.0, 0.0)],
        stroke,
    );
    ui.painter().line_segment(
        [
            corner + egui::vec2(0.0, 12.0),
            corner + egui::vec2(12.0, 0.0),
        ],
        stroke,
    );
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
    }
    if response.drag_started() {
        let pointer = response
            .interact_pointer_pos()
            .unwrap_or_else(|| rect.center());
        // 窗格本身可能已把 HUD 压小，拖动必须从屏幕上真实显示尺寸起算，
        // 不能从历史期望尺寸起算，否则第一下就会跳变。
        state.resize_start = Some((displayed_size, pointer));
    }
    if response.dragged() {
        if let (Some((start, origin)), Some(pointer)) =
            (state.resize_start, response.interact_pointer_pos())
        {
            let delta = pointer - origin;
            state.size = egui::vec2(
                (start.x - delta.x).clamp(MIN_WIDTH, MAX_WIDTH),
                (start.y - delta.y).clamp(MIN_HEIGHT, MAX_HEIGHT),
            );
        }
    }
    if response.drag_stopped() {
        state.resize_start = None;
    }
    response.on_hover_text(crate::i18n::strings().hud_resize_tip);
}

fn project_line(ui: &mut egui::Ui, view: &HudView, details: Option<&HudDetails>, pal: &Palette) {
    let s = crate::i18n::strings();
    ui.horizontal_wrapped(|ui| {
        section_label(ui, s.hud_project, pal);
        let full = view.project_path.as_deref().unwrap_or(s.hud_waiting_cli);
        let compact = compact_project_path(full);
        ui.add(
            egui::Label::new(
                egui::RichText::new(compact)
                    .monospace()
                    .size(10.5)
                    .color(pal.fg),
            )
            .truncate(),
        )
        .on_hover_text(full);
        if let Some(git) = details.and_then(|details| details.git.as_ref()) {
            let git_text = format_git(git);
            badge(
                ui,
                &git_text,
                if git.dirty {
                    pal.bg_highlight
                } else {
                    pal.btn_bg
                },
                if git.dirty { pal.warn } else { pal.fg_dim },
            );
        }
    });
}

fn context_section(ui: &mut egui::Ui, view: &HudView, details: Option<&HudDetails>, pal: &Palette) {
    let s = crate::i18n::strings();
    let context = view.context.or_else(|| {
        let details = details?;
        let used = details.context_tokens?;
        let window = details.context_window?;
        (window > 0).then(|| HudContext {
            percent: ((used.saturating_mul(100) / window).min(100)) as u8,
            kind: HudContextKind::Used,
        })
    });
    ui.horizontal(|ui| {
        section_label(ui, s.hud_context, pal);
        let label = context.map_or_else(
            || s.hud_waiting_cli.to_owned(),
            |context| match context.kind {
                HudContextKind::Used => crate::i18n::fmt1(s.hud_context_used_fmt, context.percent),
                HudContextKind::Remaining => {
                    crate::i18n::fmt1(s.hud_context_remaining_fmt, context.percent)
                }
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(label)
                    .monospace()
                    .size(10.0)
                    .color(context_color(context, pal)),
            );
        });
    });
    ui.add_space(3.0);
    progress_bar(
        ui,
        context.map_or(0.0, |context| {
            f32::from(match context.kind {
                HudContextKind::Used => context.percent,
                HudContextKind::Remaining => 100_u8.saturating_sub(context.percent),
            })
        }),
        context_color(context, pal),
        pal,
    );
}

/// 额度窗口的两条来源：后台采集的 JSON（Codex/Kimi）与状态行刮取
/// （Claude 只有这一条）。JSON 是结构化原值，优先；画面刮取只在没有
/// JSON 时兜底，免得一次误匹配盖掉一份准确数据。
fn usage_windows(
    view: &HudView,
    details: Option<&HudDetails>,
) -> Vec<(String, f32, Option<Duration>)> {
    let from_json: Vec<_> = details
        .map(|details| {
            details
                .usage_windows
                .iter()
                .map(|window| {
                    (
                        window.label.clone(),
                        window.used_percent,
                        window.resets_at_ms.and_then(remaining_until_ms),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if !from_json.is_empty() {
        return from_json;
    }
    view.usage
        .iter()
        .map(|window| {
            (
                window.label.to_owned(),
                window.used_percent,
                window.resets_in,
            )
        })
        .collect()
}

fn usage_section(ui: &mut egui::Ui, view: &HudView, details: Option<&HudDetails>, pal: &Palette) {
    let windows = usage_windows(view, details);
    if windows.is_empty() {
        return;
    }
    ui.add_space(9.0);
    section_label(ui, crate::i18n::strings().hud_usage, pal);
    ui.add_space(3.0);
    for (label, used_percent, reset) in windows {
        let compact = ui.available_width() < 300.0;
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(&label)
                    .monospace()
                    .size(9.5)
                    .color(pal.fg_dim),
            );
            let color = percent_color(used_percent, pal);
            ui.add_space(3.0);
            let reset_inline = reset.is_some() && !compact;
            let reserved = if reset_inline { 116.0 } else { 38.0 };
            let width = (ui.available_width() - reserved).max(32.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
            paint_progress(ui, rect, used_percent, color, pal);
            ui.label(
                egui::RichText::new(format!("{used_percent:.0}%"))
                    .monospace()
                    .size(9.5)
                    .color(color),
            );
            if let Some(reset) = reset.filter(|_| reset_inline) {
                ui.label(
                    egui::RichText::new(format!("↻ {}", format_duration(reset)))
                        .monospace()
                        .size(9.0)
                        .color(pal.fg_dim),
                );
            }
        });
        if let Some(reset) = reset.filter(|_| compact) {
            ui.horizontal(|ui| {
                ui.add_space(25.0);
                ui.label(
                    egui::RichText::new(format!("↻ {}", format_duration(reset)))
                        .monospace()
                        .size(9.0)
                        .color(pal.fg_dim),
                );
            });
        }
        ui.add_space(2.0);
    }
}

fn token_section(ui: &mut egui::Ui, details: &HudDetails, pal: &Palette) {
    let Some(tokens) = details.tokens else {
        return;
    };
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        section_label(ui, crate::i18n::strings().hud_tokens, pal);
        metric(ui, "in", tokens.input, pal);
        metric(ui, "out", tokens.output, pal);
        if tokens.cached > 0 {
            metric(ui, "cache", tokens.cached, pal);
        }
        if tokens.cache_write > 0 {
            metric(ui, "write", tokens.cache_write, pal);
        }
        if tokens.reasoning > 0 {
            metric(ui, "reason", tokens.reasoning, pal);
        }
        metric(ui, "Σ", tokens.total(), pal);
    });
}

fn tool_section(ui: &mut egui::Ui, details: &HudDetails, pal: &Palette) {
    if details.tools.is_empty() {
        return;
    }
    ui.add_space(9.0);
    section_label(ui, crate::i18n::strings().hud_tools, pal);
    ui.add_space(3.0);
    ui.horizontal_wrapped(|ui| {
        for tool in &details.tools {
            tool_badge(ui, tool, pal);
        }
    });
}

fn agent_section(ui: &mut egui::Ui, details: &HudDetails, pal: &Palette) {
    if details.agents.is_empty() {
        return;
    }
    ui.add_space(9.0);
    section_label(ui, crate::i18n::strings().hud_agents, pal);
    ui.add_space(3.0);
    for agent in details.agents.iter().take(3) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(if agent.running { "◐" } else { "✓" })
                    .size(10.5)
                    .color(if agent.running { pal.warn } else { pal.success }),
            );
            ui.label(
                egui::RichText::new(&agent.name)
                    .monospace()
                    .size(10.0)
                    .color(pal.fg),
            );
            if let Some(model) = agent.model.as_deref() {
                badge(ui, model, pal.btn_bg, pal.fg_dim);
            }
            if let Some(description) = agent.description.as_deref() {
                ui.add(
                    egui::Label::new(egui::RichText::new(description).size(9.5).color(pal.fg_dim))
                        .truncate(),
                )
                .on_hover_text(description);
            }
        });
    }
}

fn todo_section(ui: &mut egui::Ui, details: &HudDetails, pal: &Palette) {
    if details.todos.total == 0 {
        return;
    }
    ui.add_space(9.0);
    ui.horizontal(|ui| {
        section_label(ui, crate::i18n::strings().hud_tasks, pal);
        let percent = details.todos.completed as f32 * 100.0 / details.todos.total as f32;
        let width = (ui.available_width() - 88.0).max(70.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
        paint_progress(ui, rect, percent, pal.success, pal);
        ui.label(
            egui::RichText::new(format!(
                "{}/{}",
                details.todos.completed, details.todos.total
            ))
            .monospace()
            .size(9.5)
            .color(pal.success),
        );
    });
    if let Some(active) = details.todos.active.as_deref() {
        ui.horizontal(|ui| {
            ui.add_space(48.0);
            ui.label(egui::RichText::new("▸").size(10.0).color(pal.warn));
            ui.add(
                egui::Label::new(egui::RichText::new(active).size(9.5).color(pal.fg_dim))
                    .truncate(),
            )
            .on_hover_text(active);
        });
    }
}

fn native_section(ui: &mut egui::Ui, kind: LlmCliKind, details: &HudDetails, pal: &Palette) {
    let Some(native) = details.native.as_ref() else {
        return;
    };
    ui.add_space(9.0);
    ui.horizontal_wrapped(|ui| {
        section_label(ui, kind.display_name(), pal);
        match native {
            CliNativeDetails::Claude(stats) => {
                native_count_badge(ui, "turns", stats.user_turns, pal);
                native_count_badge(ui, "responses", stats.assistant_messages, pal);
                native_count_badge(ui, "attachments", stats.attachments, pal);
                native_count_badge(ui, "queue", stats.queue_operations, pal);
                native_count_badge(ui, "server tools", stats.server_tool_calls, pal);
                native_text_badge(ui, "permission", stats.permission_mode.as_deref(), pal);
                native_text_badge(ui, "tier", stats.service_tier.as_deref(), pal);
                native_text_badge(ui, "speed", stats.speed.as_deref(), pal);
            }
            CliNativeDetails::Codex(stats) => {
                native_text_badge(ui, "provider", stats.provider.as_deref(), pal);
                native_text_badge(ui, "origin", stats.originator.as_deref(), pal);
                native_text_badge(ui, "source", stats.thread_source.as_deref(), pal);
                native_text_badge(ui, "approval", stats.approval_policy.as_deref(), pal);
                native_text_badge(ui, "sandbox", stats.sandbox_policy.as_deref(), pal);
                native_count_badge(ui, "recent replies", stats.recent_messages, pal);
                native_count_badge(ui, "reasoning", stats.recent_reasoning, pal);
            }
            CliNativeDetails::Kimi(stats) => {
                native_text_badge(ui, "provider", stats.provider.as_deref(), pal);
                native_text_badge(ui, "profile", stats.profile.as_deref(), pal);
                native_text_badge(ui, "permission", stats.permission_mode.as_deref(), pal);
                native_text_badge(ui, "protocol", stats.protocol_version.as_deref(), pal);
                native_count_badge(ui, "turns", stats.turns, pal);
                native_count_badge(ui, "steps", stats.steps, pal);
                native_count_badge(ui, "thinking", stats.thinking_blocks, pal);
                native_count_badge(ui, "active tools", stats.active_tools, pal);
                native_count_badge(ui, "approvals", stats.approvals, pal);
                if stats.background_tasks > 0 {
                    badge(
                        ui,
                        &format!("tasks {}/{}", stats.running_tasks, stats.background_tasks),
                        pal.btn_bg,
                        if stats.running_tasks > 0 {
                            pal.warn
                        } else {
                            pal.fg_dim
                        },
                    );
                }
                if let Some(yolo) = stats.yolo {
                    badge(
                        ui,
                        if yolo { "YOLO on" } else { "YOLO off" },
                        pal.btn_bg,
                        if yolo { pal.warn } else { pal.fg_dim },
                    );
                }
            }
            CliNativeDetails::Gemini(stats) => {
                native_count_badge(ui, "turns", stats.user_turns, pal);
                native_count_badge(ui, "responses", stats.responses, pal);
                native_count_badge(ui, "info", stats.info_messages, pal);
                native_count_badge(ui, "errors", stats.errors, pal);
                if stats.thought_tokens > 0 {
                    badge(
                        ui,
                        &format!("thought {}", format_count(stats.thought_tokens)),
                        pal.btn_bg,
                        pal.fg_dim,
                    );
                }
                if stats.tool_tokens > 0 {
                    badge(
                        ui,
                        &format!("tool {}", format_count(stats.tool_tokens)),
                        pal.btn_bg,
                        pal.fg_dim,
                    );
                }
                if stats.has_summary {
                    badge(ui, "summary", pal.btn_bg, pal.success);
                }
            }
        }
    });
}

fn native_count_badge(ui: &mut egui::Ui, label: &str, value: u32, pal: &Palette) {
    if value > 0 {
        badge(ui, &format!("{label} ×{value}"), pal.btn_bg, pal.fg_dim);
    }
}

fn native_text_badge(ui: &mut egui::Ui, label: &str, value: Option<&str>, pal: &Palette) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        badge(ui, &format!("{label} {value}"), pal.btn_bg, pal.fg_dim);
    }
}

fn metadata_section(ui: &mut egui::Ui, details: &HudDetails, pal: &Palette) {
    let has_config = details.config.is_some_and(|config| {
        config.instructions
            + config.rules
            + config.mcp
            + config.hooks
            + config.skills
            + config.plugins
            + config.extensions
            > 0
    });
    if !has_config
        && details.compactions == 0
        && details.cli_version.is_none()
        && details.last_response_ms.is_none()
    {
        return;
    }
    ui.add_space(9.0);
    ui.horizontal_wrapped(|ui| {
        if let Some(config) = details.config.filter(|_| has_config) {
            config_badges(ui, config, pal);
        }
        if details.compactions > 0 {
            badge(
                ui,
                &format!(
                    "{} ×{}",
                    crate::i18n::strings().hud_compactions,
                    details.compactions
                ),
                pal.btn_bg,
                pal.fg_dim,
            );
        }
        if let Some(version) = details.cli_version.as_deref() {
            badge(ui, &format!("v{version}"), pal.btn_bg, pal.fg_dim);
        }
        if let Some(last) = details.last_response_ms.and_then(elapsed_since_ms) {
            badge(
                ui,
                &format!("last {}", format_duration(last)),
                pal.btn_bg,
                pal.fg_dim,
            );
        }
    });
}

fn config_badges(ui: &mut egui::Ui, config: ConfigCounts, pal: &Palette) {
    section_label(ui, crate::i18n::strings().hud_config, pal);
    for (label, value) in [
        ("MD", config.instructions),
        ("rules", config.rules),
        ("MCP", config.mcp),
        ("hooks", config.hooks),
        ("skills", config.skills),
        ("plugins", config.plugins),
        ("ext", config.extensions),
    ] {
        if value > 0 {
            badge(ui, &format!("{label} ×{value}"), pal.btn_bg, pal.fg_dim);
        }
    }
}

fn tool_badge(ui: &mut egui::Ui, tool: &ToolSummary, pal: &Palette) {
    let (symbol, color) = if tool.running > 0 {
        ("◐", pal.warn)
    } else if tool.errors > 0 {
        ("!", pal.error)
    } else {
        ("✓", pal.success)
    };
    let count = tool
        .completed
        .saturating_add(tool.errors)
        .saturating_add(tool.running);
    let text = if count > 1 {
        format!("{symbol} {} ×{count}", tool.name)
    } else {
        format!("{symbol} {}", tool.name)
    };
    let response = badge_response(ui, &text, pal.btn_bg, color);
    if let Some(target) = tool.active_target.as_deref() {
        response.on_hover_text(target);
    }
}

fn metric(ui: &mut egui::Ui, label: &str, value: u64, pal: &Palette) {
    badge(
        ui,
        &format!("{label} {}", format_count(value)),
        pal.btn_bg,
        pal.fg_dim,
    );
}

fn badge(ui: &mut egui::Ui, text: &str, fill: egui::Color32, color: egui::Color32) {
    badge_response(ui, text, fill, color);
}

/// 单个原子控件才能在 `horizontal_wrapped` 中整体换行；复合 Frame 会在剩余窄空间
/// 内先压缩文字，最终出现 `reasoning ×8` 逐字竖排的异常。
fn badge_response(
    ui: &mut egui::Ui,
    text: &str,
    fill: egui::Color32,
    color: egui::Color32,
) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).monospace().size(9.5).color(color))
            .fill(fill)
            .stroke(egui::Stroke::NONE)
            .corner_radius(egui::CornerRadius::same(4))
            .sense(egui::Sense::hover())
            .wrap_mode(egui::TextWrapMode::Extend),
    )
}

fn section_label(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    ui.label(egui::RichText::new(text).size(9.5).color(pal.fg_dim));
}

fn progress_bar(ui: &mut egui::Ui, percent: f32, color: egui::Color32, pal: &Palette) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 7.0), egui::Sense::hover());
    paint_progress(ui, rect, percent, color, pal);
}

fn paint_progress(
    ui: &egui::Ui,
    rect: egui::Rect,
    percent: f32,
    color: egui::Color32,
    pal: &Palette,
) {
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(3), pal.btn_bg);
    let fraction = (percent / 100.0).clamp(0.0, 1.0);
    if fraction > 0.0 {
        let fill = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.left() + rect.width() * fraction, rect.bottom()),
        );
        ui.painter()
            .rect_filled(fill, egui::CornerRadius::same(3), color);
    }
}

fn separator(ui: &mut egui::Ui, pal: &Palette) {
    ui.add_space(5.0);
    let rect = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), rect.top()),
            egui::pos2(rect.right(), rect.top()),
        ],
        egui::Stroke::new(1.0_f32, pal.bg_highlight),
    );
    ui.add_space(6.0);
}

fn dot(ui: &mut egui::Ui, pal: &Palette) {
    ui.label(egui::RichText::new("·").size(10.0).color(pal.fg_dim));
}

fn context_color(context: Option<HudContext>, pal: &Palette) -> egui::Color32 {
    let Some(context) = context else {
        return pal.fg_dim;
    };
    let used = match context.kind {
        HudContextKind::Used => context.percent,
        HudContextKind::Remaining => 100_u8.saturating_sub(context.percent),
    };
    percent_color(f32::from(used), pal)
}

fn percent_color(percent: f32, pal: &Palette) -> egui::Color32 {
    if percent >= 85.0 {
        pal.error
    } else if percent >= 70.0 {
        pal.warn
    } else {
        pal.success
    }
}

fn format_git(git: &GitStatus) -> String {
    let mut text = format!("git:({}", git.branch);
    if git.dirty {
        text.push('*');
    }
    text.push(')');
    if git.ahead > 0 {
        text.push_str(&format!(" ↑{}", git.ahead));
    }
    if git.behind > 0 {
        text.push_str(&format!(" ↓{}", git.behind));
    }
    let changed = git
        .modified
        .saturating_add(git.added)
        .saturating_add(git.deleted)
        .saturating_add(git.untracked);
    if changed > 0 {
        text.push_str(&format!(
            " !{} +{} −{} ?{}",
            git.modified, git.added, git.deleted, git.untracked
        ));
    }
    text
}

fn format_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    if total >= 86_400 {
        format!("{}d {}h", total / 86_400, total % 86_400 / 3_600)
    } else if total >= 3_600 {
        format!("{}h {}m", total / 3_600, total % 3_600 / 60)
    } else if total >= 60 {
        format!("{}m {}s", total / 60, total % 60)
    } else {
        format!("{total}s")
    }
}

fn elapsed_since_ms(started_ms: u64) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    Some(Duration::from_millis(
        now.as_millis()
            .min(u128::from(u64::MAX))
            .saturating_sub(u128::from(started_ms)) as u64,
    ))
}

fn remaining_until_ms(reset_ms: u64) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let now_ms = now.as_millis().min(u128::from(u64::MAX)) as u64;
    Some(Duration::from_millis(reset_ms.saturating_sub(now_ms)))
}

fn compact_project_path(path: &str) -> String {
    let path = Path::new(path);
    let Some(name) = path.file_name().and_then(|part| part.to_str()) else {
        return truncate_tail(path.to_string_lossy().as_ref(), 42);
    };
    let parent = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|part| part.to_str());
    let compact = parent.map_or_else(
        || name.to_owned(),
        |parent| format!("{parent}{}{name}", std::path::MAIN_SEPARATOR),
    );
    truncate_tail(&compact, 42)
}

fn truncate_tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let tail = text
        .chars()
        .skip(count.saturating_sub(max_chars.saturating_sub(1)))
        .collect::<String>();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份最小的 HUD 即时数据（只有 `session_id` 参与本组测试的判定）。
    fn view(session_id: u64) -> HudView {
        HudView {
            session_id,
            kind: LlmCliKind::Claude,
            model: None,
            context: None,
            usage: Vec::new(),
            project_path: None,
            foreground_pid: None,
            busy: false,
            session_elapsed: Duration::ZERO,
            bottom_inset: 0.0,
        }
    }

    /// 用户点关闭按钮（等价于 [`header`] 里那两行）。
    fn user_closes(state: &mut HudState, session_id: u64) {
        state.open = false;
        state.dismissed.insert(session_id);
    }

    /// 用户点折叠按钮重新打开（等价于 [`collapsed_button`] 里那三行）。
    fn user_reopens(state: &mut HudState, session_id: u64) {
        state.open = true;
        state.dismissed.remove(&session_id);
        state.last_refresh = None;
    }

    #[test]
    fn new_llm_session_reopens_hud_and_clears_details() {
        let mut state = HudState::default();
        let first = view(1);
        state.sync_session(Some(&first));
        assert!(state.open);
        state.open = false;
        state.sync_session(Some(&first));
        assert!(!state.open);
        let second = view(2);
        state.sync_session(Some(&second));
        assert!(state.open);
        assert!(state.details.is_none());
        state.sync_session(None);
        assert!(!state.open);
    }

    /// **回归**（海风哥 2026-08-11 报）：关掉 HUD 之后，切换视图它又自己弹回来。
    ///
    /// 根因是 `sync_session` 在 `session_id` 变化时无条件 `open = next.is_some()`，
    /// 而「切走再切回」恰好让它变**两次**：`Some(1)` → `None`（焦点窗格没跑 LLM）
    /// → `Some(1)`。老测试只走了 `1→1` / `1→2` / `2→None` 三条路，正好绕开了它。
    #[test]
    fn 关闭后切换视图不再自己弹回来() {
        let mut state = HudState::default();
        let first = view(1);
        state.sync_session(Some(&first));
        assert!(state.open, "首次遇到会话应当展开");

        user_closes(&mut state, 1);

        // 切到别的窗格（那里没跑 LLM），再切回来。
        state.sync_session(None);
        assert!(!state.open);
        state.sync_session(Some(&first));
        assert!(!state.open, "切回同一个会话不该把用户关掉的 HUD 重新展开");

        // 新会话仍然默认展开——那是产品意图，不是这次要修的东西。
        let second = view(2);
        state.sync_session(Some(&second));
        assert!(state.open);

        // 在两个会话之间来回切，被关掉的那个始终保持收起。
        state.sync_session(Some(&first));
        assert!(!state.open);
        state.sync_session(Some(&second));
        assert!(state.open);
    }

    /// 重新点开要**撤销**关闭意图，否则用户只能关不能开。
    #[test]
    fn 重新点开之后切换视图保持展开() {
        let mut state = HudState::default();
        let first = view(1);
        state.sync_session(Some(&first));
        user_closes(&mut state, 1);
        user_reopens(&mut state, 1);

        state.sync_session(None);
        state.sync_session(Some(&first));
        assert!(state.open, "撤销关闭之后，切换视图要跟着保持展开");
    }

    /// 关闭状态下**不采集**：后台采集线程与 2 秒轮询都挂在 `open` 上。
    ///
    /// 这条不是装饰——收起时仍然每 2 秒起一次线程去读 transcript / 跑 git，
    /// 是用户看不见的持续开销，而他刚刚明确表示不想要这个面板。
    #[test]
    fn 收起时不再起后台采集() {
        let mut state = HudState::default();
        let first = view(1);
        state.sync_session(Some(&first));
        user_closes(&mut state, 1);

        let ctx = egui::Context::default();
        state.poll_details(&ctx, &first);
        assert!(state.details_rx.is_none(), "收起状态不该起采集线程");
    }

    #[test]
    fn project_path_keeps_last_two_segments() {
        let value = compact_project_path(r"C:\work\lumen");
        assert!(value.ends_with(r"work\lumen"));
    }

    #[test]
    fn compact_counts_and_durations_are_readable() {
        assert_eq!(format_count(1_250), "1.2k");
        assert_eq!(format_count(2_000_000), "2.0m");
        assert_eq!(format_duration(Duration::from_secs(3_725)), "1h 2m");
    }
}
