//! Lumen 内置的远端文本文件编辑器。
//!
//! 文件读取/写回由 `main` 与对应传输层完成；本模块只持有编辑缓冲、
//! 编码/换行约定、未保存确认和 egui 交互。这样远程控制与 SSH 可以
//! 共用完全相同的编辑体验，同时不会把远端路径误当成本机 `PathBuf`。

use sha2::{Digest as _, Sha256};

use super::theme::Palette;

/// 内置编辑器允许载入的最大文本大小。
///
/// 这不是文件传输上限；它只防止把超大日志或二进制文件塞进 egui
/// 的单个 `String` 后卡住 UI。双击文件仍可走下载后本机打开流程。
pub const MAX_TEXT_FILE_BYTES: usize = 1024 * 1024;

/// 编辑器所指向的远端文件。路径始终是不透明的远端 UTF-8 字符串。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TextFileSource {
    Remote {
        generation: u64,
        path: String,
    },
    Ssh {
        runtime_id: crate::ssh_runtime::SshRuntimeId,
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
    },
}

impl TextFileSource {
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Remote { path, .. } | Self::Ssh { path, .. } => path,
        }
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        self.path()
            .rsplit(['/', '\\'])
            .find(|segment| !segment.is_empty())
            .unwrap_or(self.path())
    }

    #[must_use]
    pub const fn is_ssh(&self) -> bool {
        matches!(self, Self::Ssh { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl LineEnding {
    const fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::CrLf => "CRLF",
        }
    }
}

/// 传输层开始读取文件时携带的稳定请求标识。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadRequest {
    pub token: u64,
    pub source: TextFileSource,
}

/// 保存请求。`expected_sha256` 是打开/上次成功保存时的远端内容摘要；
/// 后端能做条件写时应据此阻止静默覆盖其他客户端的修改。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveRequest {
    pub token: u64,
    pub source: TextFileSource,
    pub bytes: Vec<u8>,
    pub expected_len: u64,
    pub expected_sha256: [u8; 32],
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveFailure {
    /// 远端内容已被其他程序修改，普通保存必须停下等待用户选择。
    Conflict,
    Message(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadState {
    Loading,
    Ready,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingDecisionKind {
    Close,
    Switch,
    OverwriteConflict,
}

#[derive(Clone, Debug)]
struct PendingSave {
    token: u64,
    text: String,
    bytes_sha256: [u8; 32],
}

#[derive(Clone, Debug)]
struct Document {
    source: TextFileSource,
    token: u64,
    text: String,
    saved_text: String,
    expected_sha256: [u8; 32],
    expected_len: u64,
    line_ending: LineEnding,
    utf8_bom: bool,
    state: LoadState,
    error: Option<String>,
    pending_save: Option<PendingSave>,
    saved_flash_until: Option<std::time::Instant>,
    source_valid: bool,
}

impl Document {
    fn loading(source: TextFileSource, token: u64) -> Self {
        Self {
            source,
            token,
            text: String::new(),
            saved_text: String::new(),
            expected_sha256: [0; 32],
            expected_len: 0,
            line_ending: LineEnding::Lf,
            utf8_bom: false,
            state: LoadState::Loading,
            error: None,
            pending_save: None,
            saved_flash_until: None,
            source_valid: true,
        }
    }

    fn dirty(&self) -> bool {
        self.text != self.saved_text
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.text.zeroize();
        self.saved_text.zeroize();
        if let Some(pending) = self.pending_save.as_mut() {
            pending.text.zeroize();
        }
    }
}

/// 跨帧编辑器状态。由 [`super::ShellState`] 持有。
#[derive(Default)]
pub struct TextEditorState {
    document: Option<Document>,
    next_token: u64,
    focus_editor: bool,
    find_open: bool,
    focus_find: bool,
    find_query: String,
    pending_source: Option<TextFileSource>,
    pending_decision: Option<PendingDecisionKind>,
    post_save_action: Option<PendingDecisionKind>,
}

impl TextEditorState {
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.document.is_some()
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.document.as_ref().is_some_and(Document::dirty)
    }

    #[must_use]
    pub fn source(&self) -> Option<&TextFileSource> {
        self.document.as_ref().map(|document| &document.source)
    }

    /// 请求打开一个远端文件。当前文档无未保存修改时立即返回读取请求；
    /// 否则先在编辑器内弹确认，确认丢弃后由 [`show`] 返回读取请求。
    pub fn request_open(&mut self, source: TextFileSource) -> Option<LoadRequest> {
        if self
            .document
            .as_ref()
            .is_some_and(|document| document.source == source && document.source_valid)
        {
            self.focus_editor = true;
            return None;
        }
        if self.is_dirty()
            || self
                .document
                .as_ref()
                .is_some_and(|d| d.pending_save.is_some())
        {
            self.pending_source = Some(source);
            self.pending_decision = Some(PendingDecisionKind::Switch);
            return None;
        }
        Some(self.begin_load(source))
    }

    fn begin_load(&mut self, source: TextFileSource) -> LoadRequest {
        let token = self.allocate_token();
        self.document = Some(Document::loading(source.clone(), token));
        self.find_open = false;
        self.focus_find = false;
        self.find_query.clear();
        self.focus_editor = false;
        LoadRequest { token, source }
    }

    fn allocate_token(&mut self) -> u64 {
        let token = self.next_token.max(1);
        self.next_token = token.checked_add(1).unwrap_or(1);
        token
    }

    /// 应用异步读取结果。陈旧 token（切换/关闭后的回包）会被静默丢弃。
    pub fn apply_loaded(&mut self, token: u64, result: Result<Vec<u8>, String>) -> bool {
        let Some(document) = self
            .document
            .as_mut()
            .filter(|doc| doc.token == token && doc.source_valid)
        else {
            return false;
        };
        match result.and_then(decode_text_file) {
            Ok(decoded) => {
                document.text.clone_from(&decoded.text);
                document.saved_text = decoded.text;
                document.expected_sha256 = decoded.sha256;
                document.expected_len = decoded.original_len;
                document.line_ending = decoded.line_ending;
                document.utf8_bom = decoded.utf8_bom;
                document.state = LoadState::Ready;
                document.error = None;
                self.focus_editor = true;
            }
            Err(error) => {
                document.state = LoadState::Error;
                document.error = Some(error);
            }
        }
        true
    }

    /// 标记保存请求已经交给传输层。若当前文档或 token 已变化则拒绝。
    pub fn mark_saving(&mut self, request: &SaveRequest) -> bool {
        let Some(document) = self.document.as_mut().filter(|doc| {
            doc.token == request.token && doc.source == request.source && doc.source_valid
        }) else {
            return false;
        };
        document.pending_save = Some(PendingSave {
            token: request.token,
            text: document.text.clone(),
            bytes_sha256: sha256(&request.bytes),
        });
        document.error = None;
        true
    }

    /// 应用异步保存结果。保存期间继续输入是允许的；成功时仅把已写入
    /// 的快照作为新基线，之后输入仍保持“未保存”状态。
    pub fn apply_saved(&mut self, token: u64, result: Result<(), SaveFailure>) -> bool {
        let Some(document) = self
            .document
            .as_mut()
            .filter(|doc| doc.token == token && doc.source_valid)
        else {
            return false;
        };
        let Some(pending) = document
            .pending_save
            .take()
            .filter(|pending| pending.token == token)
        else {
            return false;
        };
        match result {
            Ok(()) => {
                document.saved_text = pending.text;
                document.expected_sha256 = pending.bytes_sha256;
                document.expected_len = u64::try_from(
                    encode_text_file(
                        &document.saved_text,
                        document.line_ending,
                        document.utf8_bom,
                    )
                    .len(),
                )
                .unwrap_or(u64::MAX);
                document.error = None;
                document.saved_flash_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
            }
            Err(SaveFailure::Conflict) => {
                document.error = None;
                document.pending_save = Some(pending);
                self.pending_decision = Some(PendingDecisionKind::OverwriteConflict);
            }
            Err(SaveFailure::Message(message)) => {
                document.error = Some(message);
                self.post_save_action = None;
            }
        }
        true
    }

    /// 令指定来源失效。加载中的文档转为错误；保存中的文档结束等待但保留
    /// 当前缓冲与 dirty 状态；已加载文档继续可读/复制，但不能再写回旧来源。
    pub fn invalidate_source(
        &mut self,
        source: &TextFileSource,
        message: impl Into<String>,
    ) -> bool {
        let Some(document) = self
            .document
            .as_mut()
            .filter(|document| &document.source == source && document.source_valid)
        else {
            return false;
        };
        document.source_valid = false;
        document.pending_save = None;
        document.saved_flash_until = None;
        document.error = Some(message.into());
        if document.state == LoadState::Loading {
            document.state = LoadState::Error;
        }
        if self.pending_decision == Some(PendingDecisionKind::OverwriteConflict) {
            self.pending_decision = None;
        }
        self.post_save_action = None;
        true
    }

    pub fn close_without_prompt(&mut self) {
        self.document = None;
        self.pending_source = None;
        self.pending_decision = None;
        self.post_save_action = None;
        self.find_open = false;
        self.focus_find = false;
        self.find_query.clear();
    }

    fn request_close(&mut self) -> bool {
        if self.is_dirty()
            || self
                .document
                .as_ref()
                .is_some_and(|d| d.pending_save.is_some())
        {
            self.pending_decision = Some(PendingDecisionKind::Close);
            false
        } else {
            self.close_without_prompt();
            true
        }
    }

    fn build_save_request(&mut self, force: bool) -> Option<SaveRequest> {
        let document = self.document.as_mut().filter(|doc| {
            doc.state == LoadState::Ready && doc.pending_save.is_none() && doc.source_valid
        })?;
        if !force && !document.dirty() {
            return None;
        }
        let bytes = encode_text_file(&document.text, document.line_ending, document.utf8_bom);
        if bytes.len() > MAX_TEXT_FILE_BYTES {
            document.error = Some(crate::i18n::fmt1(
                crate::i18n::strings().text_editor_save_too_large_fmt,
                MAX_TEXT_FILE_BYTES / (1024 * 1024),
            ));
            return None;
        }
        Some(SaveRequest {
            token: document.token,
            source: document.source.clone(),
            bytes,
            expected_len: document.expected_len,
            expected_sha256: document.expected_sha256,
            force,
        })
    }
}

#[derive(Default)]
pub struct Output {
    pub load: Option<LoadRequest>,
    pub save: Option<SaveRequest>,
    pub closed: bool,
}

/// 在中央工作区绘制编辑器。侧栏和目录树仍可见，终端继续在后台运行，
/// 但调用方应把键盘/IME 焦点交给 egui。
pub fn show(ui: &mut egui::Ui, state: &mut TextEditorState, pal: &Palette) -> Output {
    let mut output = Output::default();
    finish_deferred_action(state, &mut output);
    if output.closed || output.load.is_some() {
        return output;
    }
    let Some(document) = state.document.as_mut() else {
        return output;
    };

    let save_shortcut =
        ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::S));
    let find_shortcut =
        ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::F));
    if find_shortcut && document.state == LoadState::Ready {
        state.find_open = true;
        state.focus_find = true;
    }

    egui::Frame::new()
        .fill(pal.bg_panel)
        .inner_margin(egui::Margin::same(0))
        .show(ui, |ui| {
            editor_header(ui, state, pal, &mut output);
            if state.find_open {
                find_bar(ui, state, pal);
            }
            ui.separator();

            let Some(document) = state.document.as_mut() else {
                return;
            };
            match document.state {
                LoadState::Loading => {
                    centered_status(ui, crate::i18n::strings().filetree_loading, pal)
                }
                LoadState::Error => {
                    let message = document
                        .error
                        .as_deref()
                        .unwrap_or(crate::i18n::strings().ssh_status_error);
                    centered_error(ui, message, pal);
                    if document.source_valid {
                        ui.vertical_centered(|ui| {
                            if ui
                                .button(crate::i18n::strings().remote_refresh_dir_tip)
                                .clicked()
                            {
                                output.load = Some(LoadRequest {
                                    token: document.token,
                                    source: document.source.clone(),
                                });
                                document.state = LoadState::Loading;
                                document.error = None;
                            }
                        });
                    }
                }
                LoadState::Ready => {
                    editor_body(ui, state, pal);
                }
            }
        });

    if save_shortcut && output.save.is_none() {
        output.save = state.build_save_request(false);
    }
    decision_modal(ui.ctx(), state, pal, &mut output);
    output
}

fn finish_deferred_action(state: &mut TextEditorState, output: &mut Output) {
    let post_save = state.post_save_action;
    let clean_pending_decision = state.pending_decision.filter(|kind| {
        matches!(
            kind,
            PendingDecisionKind::Close | PendingDecisionKind::Switch
        ) && state
            .document
            .as_ref()
            .is_some_and(|document| document.pending_save.is_none() && !document.dirty())
    });
    let Some(action) = post_save.or(clean_pending_decision) else {
        return;
    };
    let Some(document) = state.document.as_ref() else {
        state.post_save_action = None;
        state.pending_decision = None;
        return;
    };
    if document.pending_save.is_some() {
        return;
    }
    if document.dirty() {
        if post_save.is_some() {
            state.post_save_action = None;
            state.pending_decision = Some(action);
        }
        return;
    }

    state.post_save_action = None;
    state.pending_decision = None;
    match action {
        PendingDecisionKind::Close => {
            state.close_without_prompt();
            output.closed = true;
        }
        PendingDecisionKind::Switch => {
            if let Some(source) = state.pending_source.take() {
                output.load = Some(state.begin_load(source));
            }
        }
        PendingDecisionKind::OverwriteConflict => {}
    }
}

fn editor_header(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    pal: &Palette,
    output: &mut Output,
) {
    let Some(document) = state.document.as_ref() else {
        return;
    };
    let source = document.source.clone();
    let dirty = document.dirty();
    let saving = document.pending_save.is_some();
    let can_save = document.state == LoadState::Ready && document.source_valid && dirty && !saving;
    let saved_recent = document
        .saved_flash_until
        .is_some_and(|until| until > std::time::Instant::now());
    let title = format!("{}{}", if dirty { "● " } else { "" }, source.display_name());
    let path = source.path().to_owned();
    let source_label = if source.is_ssh() { "SSH" } else { "REMOTE" };

    egui::Frame::new()
        .fill(pal.bg_dark)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).strong().color(pal.fg))
                    .on_hover_text(path);
                ui.label(
                    egui::RichText::new(source_label)
                        .monospace()
                        .size(10.0)
                        .color(pal.accent),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(crate::i18n::strings().menu_close).clicked() {
                        output.closed = state.request_close();
                    }
                    if ui
                        .add_enabled(can_save, egui::Button::new(crate::i18n::strings().ssh_save))
                        .on_hover_text("Ctrl+S")
                        .clicked()
                    {
                        output.save = state.build_save_request(false);
                    }
                    let status = if saving {
                        crate::i18n::strings().text_editor_saving
                    } else if saved_recent {
                        crate::i18n::strings().text_editor_saved
                    } else if dirty {
                        crate::i18n::strings().text_editor_unsaved
                    } else {
                        crate::i18n::strings().text_editor_saved
                    };
                    ui.label(egui::RichText::new(status).size(11.0).color(if dirty {
                        pal.fg
                    } else {
                        pal.fg_dim
                    }));
                });
            });
            ui.add(
                egui::Label::new(
                    egui::RichText::new(source.path())
                        .monospace()
                        .size(10.0)
                        .color(pal.fg_dim),
                )
                .truncate()
                .selectable(true),
            );
        });
}

fn find_bar(ui: &mut egui::Ui, state: &mut TextEditorState, pal: &Palette) {
    let matches = state
        .document
        .as_ref()
        .map_or(0, |document| match_count(&document.text, &state.find_query));
    egui::Frame::new()
        .fill(pal.bg_dark)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let response = ui.add_sized(
                    [260.0, 24.0],
                    egui::TextEdit::singleline(&mut state.find_query)
                        .hint_text(crate::i18n::strings().text_editor_find_hint)
                        .desired_width(260.0),
                );
                if state.focus_find {
                    response.request_focus();
                    state.focus_find = false;
                }
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    state.find_open = false;
                    state.focus_find = false;
                }
                ui.label(
                    egui::RichText::new(format!("{matches}"))
                        .monospace()
                        .size(11.0)
                        .color(pal.fg_dim),
                );
                if ui.small_button("×").clicked() {
                    state.find_open = false;
                    state.focus_find = false;
                }
            });
        });
}

fn editor_body(ui: &mut egui::Ui, state: &mut TextEditorState, pal: &Palette) {
    let available = ui.available_size();
    let status_height = 24.0;
    let editor_height = (available.y - status_height).max(80.0);
    let Some(document) = state.document.as_mut() else {
        return;
    };
    let response = egui::Frame::new()
        .fill(pal.bg_panel)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.add_sized(
                [ui.available_width(), editor_height],
                egui::TextEdit::multiline(&mut document.text)
                    .id_salt(("lumen_text_editor", document.token))
                    .font(egui::TextStyle::Monospace)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .lock_focus(true),
            )
        })
        .inner;
    if state.focus_editor {
        response.request_focus();
        state.focus_editor = false;
    }

    let line_count = document.text.lines().count().max(1);
    let bytes = document.text.len();
    egui::Frame::new()
        .fill(pal.bg_dark)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(crate::i18n::fmt2(
                        crate::i18n::strings().text_editor_stats_fmt,
                        line_count,
                        bytes,
                    ))
                    .monospace()
                    .size(10.0)
                    .color(pal.fg_dim),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(document.line_ending.label())
                            .monospace()
                            .size(10.0)
                            .color(pal.fg_dim),
                    );
                    ui.label(
                        egui::RichText::new(if document.utf8_bom {
                            "UTF-8 BOM"
                        } else {
                            "UTF-8"
                        })
                        .monospace()
                        .size(10.0)
                        .color(pal.fg_dim),
                    );
                });
            });
        });
    if let Some(error) = &document.error {
        ui.colored_label(pal.error, error);
    }
}

fn decision_modal(
    ctx: &egui::Context,
    state: &mut TextEditorState,
    pal: &Palette,
    output: &mut Output,
) {
    let Some(kind) = state.pending_decision else {
        return;
    };
    let mut keep = true;
    egui::Modal::new(egui::Id::new("lumen_text_editor_decision"))
        .backdrop_color(egui::Color32::from_black_alpha(140))
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(16)),
        )
        .show(ctx, |ui| {
            ui.set_min_width(390.0);
            match kind {
                PendingDecisionKind::OverwriteConflict => {
                    ui.heading(crate::i18n::strings().text_editor_remote_changed_title);
                    ui.add_space(6.0);
                    ui.label(crate::i18n::strings().text_editor_remote_changed_body);
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(crate::i18n::strings().text_editor_keep_editing)
                            .clicked()
                        {
                            if let Some(document) = state.document.as_mut() {
                                document.pending_save = None;
                            }
                            state.post_save_action = None;
                            keep = false;
                        }
                        if ui
                            .button(crate::i18n::strings().text_editor_reload)
                            .clicked()
                        {
                            if let Some(document) = state.document.as_mut() {
                                document.pending_save = None;
                                document.state = LoadState::Loading;
                                output.load = Some(LoadRequest {
                                    token: document.token,
                                    source: document.source.clone(),
                                });
                            }
                            state.post_save_action = None;
                            keep = false;
                        }
                        if ui
                            .button(
                                egui::RichText::new(crate::i18n::strings().text_editor_overwrite)
                                    .color(pal.error),
                            )
                            .clicked()
                        {
                            if let Some(document) = state.document.as_mut() {
                                document.pending_save = None;
                            }
                            output.save = state.build_save_request(true);
                            keep = false;
                        }
                    });
                }
                PendingDecisionKind::Close | PendingDecisionKind::Switch => {
                    ui.heading(crate::i18n::strings().text_editor_unsaved_title);
                    ui.add_space(6.0);
                    ui.label(crate::i18n::strings().text_editor_unsaved_body);
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(crate::i18n::strings().text_editor_keep_editing)
                            .clicked()
                        {
                            state.pending_source = None;
                            keep = false;
                        }
                        let can_save = state.document.as_ref().is_some_and(|document| {
                            document.state == LoadState::Ready
                                && document.source_valid
                                && document.pending_save.is_none()
                                && document.dirty()
                        });
                        if ui
                            .add_enabled(
                                can_save,
                                egui::Button::new(crate::i18n::strings().ssh_save),
                            )
                            .clicked()
                        {
                            if let Some(save) = state.build_save_request(false) {
                                state.post_save_action = Some(kind);
                                output.save = Some(save);
                                keep = false;
                            }
                        }
                        if ui
                            .button(
                                egui::RichText::new(crate::i18n::strings().text_editor_discard)
                                    .color(pal.error),
                            )
                            .clicked()
                        {
                            match kind {
                                PendingDecisionKind::Close => {
                                    state.close_without_prompt();
                                    output.closed = true;
                                }
                                PendingDecisionKind::Switch => {
                                    if let Some(source) = state.pending_source.take() {
                                        output.load = Some(state.begin_load(source));
                                    }
                                }
                                PendingDecisionKind::OverwriteConflict => {}
                            }
                            keep = false;
                        }
                    });
                }
            }
        });
    if !keep {
        state.pending_decision = None;
    }
}

fn centered_status(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    ui.with_layout(
        egui::Layout::top_down(egui::Align::Center).with_main_align(egui::Align::Center),
        |ui| {
            ui.spinner();
            ui.label(egui::RichText::new(text).color(pal.fg_dim));
        },
    );
}

fn centered_error(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    ui.with_layout(
        egui::Layout::top_down(egui::Align::Center).with_main_align(egui::Align::Center),
        |ui| {
            ui.label(egui::RichText::new(text).color(pal.error));
        },
    );
}

fn match_count(text: &str, query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    text.match_indices(query).count()
}

struct DecodedText {
    text: String,
    line_ending: LineEnding,
    utf8_bom: bool,
    sha256: [u8; 32],
    original_len: u64,
}

fn decode_text_file(bytes: Vec<u8>) -> Result<DecodedText, String> {
    if bytes.len() > MAX_TEXT_FILE_BYTES {
        return Err(crate::i18n::fmt1(
            crate::i18n::strings().text_editor_open_too_large_fmt,
            MAX_TEXT_FILE_BYTES / (1024 * 1024),
        ));
    }
    if bytes.contains(&0) {
        return Err(crate::i18n::strings().text_editor_binary_error.to_owned());
    }
    let digest = sha256(&bytes);
    let original_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let (utf8_bom, payload) = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .map_or((false, bytes.as_slice()), |payload| (true, payload));
    let raw = std::str::from_utf8(payload).map_err(|_| {
        crate::i18n::strings()
            .text_editor_utf8_only_error
            .to_owned()
    })?;
    let crlf = raw.matches("\r\n").count();
    let lone_lf = raw.matches('\n').count().saturating_sub(crlf);
    if (crlf > 0 && lone_lf > 0) || raw.replace("\r\n", "").contains('\r') {
        return Err(crate::i18n::strings()
            .text_editor_mixed_eol_error
            .to_owned());
    }
    let line_ending = if crlf > 0 {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    let text = raw.replace("\r\n", "\n");
    Ok(DecodedText {
        text,
        line_ending,
        utf8_bom,
        sha256: digest,
        original_len,
    })
}

fn encode_text_file(text: &str, line_ending: LineEnding, utf8_bom: bool) -> Vec<u8> {
    let estimated = text
        .len()
        .saturating_add(if utf8_bom { 3 } else { 0 })
        .saturating_add(if line_ending == LineEnding::CrLf {
            text.matches('\n').count()
        } else {
            0
        });
    let mut bytes = Vec::with_capacity(estimated);
    if utf8_bom {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    match line_ending {
        LineEnding::Lf => bytes.extend_from_slice(text.as_bytes()),
        LineEnding::CrLf => {
            for (index, part) in text.split('\n').enumerate() {
                if index > 0 {
                    bytes.extend_from_slice(b"\r\n");
                }
                bytes.extend_from_slice(part.as_bytes());
            }
        }
    }
    bytes
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_bom_and_crlf_round_trip() {
        let original = b"\xEF\xBB\xBFone\r\ntwo\r\n".to_vec();
        let decoded = decode_text_file(original.clone()).expect("valid text");
        assert!(decoded.utf8_bom);
        assert_eq!(decoded.line_ending, LineEnding::CrLf);
        assert_eq!(decoded.text, "one\ntwo\n");
        assert_eq!(
            encode_text_file(&decoded.text, decoded.line_ending, decoded.utf8_bom),
            original
        );
    }

    #[test]
    fn rejects_binary_and_invalid_utf8() {
        assert!(decode_text_file(vec![b'a', 0, b'b']).is_err());
        assert!(decode_text_file(vec![0xFF]).is_err());
    }

    #[test]
    fn stale_load_and_save_results_are_ignored() {
        let mut state = TextEditorState::default();
        let first = state
            .request_open(TextFileSource::Remote {
                generation: 1,
                path: "/tmp/a".to_owned(),
            })
            .expect("load");
        state.close_without_prompt();
        assert!(!state.apply_loaded(first.token, Ok(b"old".to_vec())));

        let second = state
            .request_open(TextFileSource::Remote {
                generation: 1,
                path: "/tmp/b".to_owned(),
            })
            .expect("load");
        assert!(state.apply_loaded(second.token, Ok(b"new".to_vec())));
        assert!(!state.apply_saved(first.token, Ok(())));
    }

    #[test]
    fn edit_during_save_remains_dirty_after_success() {
        let mut state = TextEditorState::default();
        let load = state
            .request_open(TextFileSource::Ssh {
                runtime_id: 1,
                session_id: 7,
                path: "/tmp/a.txt".to_owned(),
            })
            .expect("load");
        state
            .apply_loaded(load.token, Ok(b"before".to_vec()))
            .then_some(())
            .expect("applied");
        state.document.as_mut().expect("doc").text = "saved".to_owned();
        let save = state.build_save_request(false).expect("save");
        assert!(state.mark_saving(&save));
        state.document.as_mut().expect("doc").text = "typed later".to_owned();
        assert!(state.apply_saved(save.token, Ok(())));
        assert!(state.is_dirty());
        assert_eq!(state.document.as_ref().expect("doc").saved_text, "saved");
    }

    #[test]
    fn same_path_in_a_new_remote_generation_is_a_different_document() {
        let mut state = TextEditorState::default();
        let first = state
            .request_open(TextFileSource::Remote {
                generation: 10,
                path: "/etc/app.conf".to_owned(),
            })
            .expect("first load");
        assert!(state.apply_loaded(first.token, Ok(b"old peer".to_vec())));

        let second = state
            .request_open(TextFileSource::Remote {
                generation: 11,
                path: "/etc/app.conf".to_owned(),
            })
            .expect("new generation must reload");
        assert_ne!(first.token, second.token);
        assert_eq!(
            second.source,
            TextFileSource::Remote {
                generation: 11,
                path: "/etc/app.conf".to_owned(),
            }
        );
    }

    #[test]
    fn invalidation_finishes_loading_and_saving_without_losing_dirty_buffer() {
        let mut loading = TextEditorState::default();
        let source = TextFileSource::Ssh {
            runtime_id: 4,
            session_id: 7,
            path: "/tmp/a.txt".to_owned(),
        };
        let load = loading
            .request_open(source.clone())
            .expect("loading request");
        assert!(loading.invalidate_source(&source, "session changed"));
        let loading_document = loading.document.as_ref().expect("document");
        assert_eq!(loading_document.state, LoadState::Error);
        assert!(!loading_document.source_valid);
        assert!(!loading.apply_loaded(load.token, Ok(b"stale".to_vec())));

        let mut saving = TextEditorState::default();
        let load = saving.request_open(source.clone()).expect("load");
        assert!(saving.apply_loaded(load.token, Ok(b"before".to_vec())));
        saving.document.as_mut().expect("document").text = "after".to_owned();
        let save = saving.build_save_request(false).expect("save");
        assert!(saving.mark_saving(&save));
        assert!(saving.invalidate_source(&source, "session changed"));
        let saving_document = saving.document.as_ref().expect("document");
        assert_eq!(saving_document.text, "after");
        assert!(saving_document.dirty());
        assert!(saving_document.pending_save.is_none());
        assert!(!saving_document.source_valid);
        assert!(saving.build_save_request(false).is_none());
    }

    #[test]
    fn save_before_close_waits_for_success_then_closes() {
        let mut state = TextEditorState::default();
        let source = TextFileSource::Remote {
            generation: 2,
            path: "/tmp/a.txt".to_owned(),
        };
        let load = state.request_open(source).expect("load");
        assert!(state.apply_loaded(load.token, Ok(b"before".to_vec())));
        state.document.as_mut().expect("document").text = "after".to_owned();
        let save = state.build_save_request(false).expect("save");
        state.post_save_action = Some(PendingDecisionKind::Close);
        assert!(state.mark_saving(&save));

        let mut output = Output::default();
        finish_deferred_action(&mut state, &mut output);
        assert!(!output.closed);
        assert!(state.is_open());

        assert!(state.apply_saved(save.token, Ok(())));
        finish_deferred_action(&mut state, &mut output);
        assert!(output.closed);
        assert!(!state.is_open());
    }

    #[test]
    fn edits_made_during_save_prevent_deferred_switch_from_discarding_them() {
        let mut state = TextEditorState::default();
        let source = TextFileSource::Ssh {
            runtime_id: 8,
            session_id: 9,
            path: "/tmp/a.txt".to_owned(),
        };
        let target = TextFileSource::Ssh {
            runtime_id: 8,
            session_id: 9,
            path: "/tmp/b.txt".to_owned(),
        };
        let load = state.request_open(source).expect("load");
        assert!(state.apply_loaded(load.token, Ok(b"before".to_vec())));
        state.document.as_mut().expect("document").text = "saved".to_owned();
        let save = state.build_save_request(false).expect("save");
        state.pending_source = Some(target);
        state.post_save_action = Some(PendingDecisionKind::Switch);
        assert!(state.mark_saving(&save));
        state.document.as_mut().expect("document").text = "typed later".to_owned();

        assert!(state.apply_saved(save.token, Ok(())));
        let mut output = Output::default();
        finish_deferred_action(&mut state, &mut output);
        assert!(output.load.is_none());
        assert_eq!(state.pending_decision, Some(PendingDecisionKind::Switch));
        assert!(state.is_dirty());
        assert_eq!(
            state.document.as_ref().expect("document").text,
            "typed later"
        );
    }
}
