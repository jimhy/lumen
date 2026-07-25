//! Lumen 内置的远端文本文件编辑器。
//!
//! 文件读取/写回由 `main` 与对应传输层完成；本模块只持有编辑缓冲、
//! 编码/换行约定、未保存确认和 egui 交互。这样远程控制与 SSH 可以
//! 共用完全相同的编辑体验，同时不会把远端路径误当成本机 `PathBuf`。

use std::collections::HashMap;

use sha2::{Digest as _, Sha256};

use super::{
    text_editor_language::{CompletionKind, CompletionSet, Language, LanguageCache},
    text_editor_ops::{self, EditPlan, TypedCharDecision},
    theme::Palette,
};

/// 内置编辑器允许载入的最大文本大小。
///
/// 这不是文件传输上限；它只防止把超大日志或二进制文件塞进 egui
/// 的单个 `String` 后卡住 UI。双击文件仍可走下载后本机打开流程。
pub const MAX_TEXT_FILE_BYTES: usize = 1024 * 1024;

// 含 egui 垂直布局在相邻控件之间追加的 8px item spacing。
const EDITOR_STATUS_HEIGHT: f32 = 32.0;
const EDITOR_ERROR_HEIGHT: f32 = 30.0;
const EDITOR_INNER_MARGIN_X: f32 = 10.0;
const EDITOR_INNER_MARGIN_Y: f32 = 8.0;

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
enum CloseScope {
    Tab,
    Editor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CloseIntent {
    scope: CloseScope,
    token: u64,
}

#[derive(Clone, Debug)]
struct PendingSave {
    token: u64,
    text: String,
    bytes_sha256: [u8; 32],
}

#[derive(Clone)]
struct CompletionPopup {
    token: u64,
    fingerprint: [u8; 32],
    set: CompletionSet,
    selected: usize,
    caret_rect: egui::Rect,
}

#[derive(Clone)]
struct PendingCompletion {
    token: u64,
    fingerprint: [u8; 32],
    replace_chars: std::ops::Range<usize>,
    insertion: String,
    post_selection: Option<std::ops::Range<usize>>,
}

struct InjectedEditorEdit {
    post_selection: Option<std::ops::Range<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SmartNewlineEdit {
    replace_chars: std::ops::Range<usize>,
    insertion: String,
    cursor_after: usize,
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
    save_conflict: bool,
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
            save_conflict: false,
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
    documents: Vec<Document>,
    active_token: Option<u64>,
    next_token: u64,
    /// 文档仍在内存中、但中央工作区是否正在显示编辑器。
    ///
    /// 隐藏只切换此标志；所有标签、正文、脏状态和异步读写状态都保留。
    visible: bool,
    focus_editor: bool,
    find_open: bool,
    focus_find: bool,
    find_query: String,
    /// 查找栏的替换输入与替换行开关（Ctrl+H 或箭头按钮展开）。
    replace_query: String,
    find_replace_open: bool,
    /// 查找大小写敏感开关（Aa）。
    find_case_sensitive: bool,
    /// Ctrl+G 跳转到行的小输入条。
    goto_open: bool,
    goto_query: String,
    focus_goto: bool,
    /// Alt+Z 软换行开关（全局偏好，不随标签切换重置）。
    wrap: bool,
    /// 查找/跳转请求的目标字符下标；editor_body 消费并滚动到可见。
    pending_scroll: Option<(u64, usize)>,
    pending_close: Option<CloseIntent>,
    post_save_close: Option<CloseIntent>,
    /// 关闭整个编辑器时，用户已选择“不保存”的文档及其当时正文指纹。
    ///
    /// 异步保存另一个标签期间仍允许用户继续操作；若已放弃的标签后来
    /// 又被修改，指纹会失配，关闭流程必须再次询问，不能静默丢弃新编辑。
    close_editor_discarded: HashMap<u64, [u8; 32]>,
    language_caches: HashMap<u64, LanguageCache>,
    completion: Option<CompletionPopup>,
    pending_completion: Option<PendingCompletion>,
    ime_composing: bool,
}

impl TextEditorState {
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.documents.is_empty()
    }

    /// 编辑器既有打开文档，又正在占用中央工作区。
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible && self.is_open()
    }

    /// 暂时隐藏编辑器并把键盘/IME 交还工作区。
    ///
    /// 不关闭任何标签，也不改变正文、脏状态、关闭确认或异步保存状态。
    pub fn hide(&mut self) -> bool {
        if !self.is_visible() {
            return false;
        }
        self.visible = false;
        self.focus_editor = false;
        self.focus_find = false;
        self.focus_goto = false;
        self.dismiss_completion();
        true
    }

    /// 恢复仍有打开文档的编辑器，并在下一帧聚焦当前正文。
    pub fn restore(&mut self) -> bool {
        if !self.is_open() || self.visible {
            return false;
        }
        self.visible = true;
        self.focus_editor = true;
        self.focus_find = false;
        self.focus_goto = false;
        self.dismiss_completion();
        true
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.documents.iter().any(Document::dirty)
    }

    #[must_use]
    pub fn source_for_token(&self, token: u64) -> Option<&TextFileSource> {
        self.document(token).map(|document| &document.source)
    }

    pub fn sources(&self) -> impl Iterator<Item = &TextFileSource> {
        self.documents.iter().map(|document| &document.source)
    }

    /// 字体配置变更后清除已排版的语法高亮 Galley；正文与撤销历史不受影响。
    pub fn invalidate_visual_cache(&mut self) {
        self.language_caches.clear();
        self.completion = None;
    }

    /// 请求打开一个远端文件。已打开的来源只切换到既有标签；新来源
    /// 立即建立独立加载标签，不会替换或丢弃其他文档。
    pub fn request_open(&mut self, source: TextFileSource) -> Option<LoadRequest> {
        if let Some(token) = self
            .documents
            .iter()
            .find(|document| document.source == source && document.source_valid)
            .map(|document| document.token)
        {
            self.active_token = Some(token);
            self.visible = true;
            self.focus_editor = true;
            self.cancel_close_flow();
            self.dismiss_completion();
            return None;
        }
        self.cancel_close_flow();
        self.dismiss_completion();
        Some(self.begin_load(source))
    }

    fn begin_load(&mut self, source: TextFileSource) -> LoadRequest {
        let token = self.allocate_token();
        self.documents
            .push(Document::loading(source.clone(), token));
        self.active_token = Some(token);
        self.visible = true;
        self.find_open = false;
        self.focus_find = false;
        self.find_query.clear();
        self.replace_query.clear();
        self.find_replace_open = false;
        self.goto_open = false;
        self.goto_query.clear();
        self.focus_goto = false;
        self.pending_scroll = None;
        self.focus_editor = false;
        self.dismiss_completion();
        LoadRequest { token, source }
    }

    fn allocate_token(&mut self) -> u64 {
        loop {
            let token = self.next_token.max(1);
            self.next_token = token.checked_add(1).unwrap_or(1);
            if self
                .documents
                .iter()
                .all(|document| document.token != token)
            {
                return token;
            }
        }
    }

    fn document(&self, token: u64) -> Option<&Document> {
        self.documents
            .iter()
            .find(|document| document.token == token)
    }

    fn document_mut(&mut self, token: u64) -> Option<&mut Document> {
        self.documents
            .iter_mut()
            .find(|document| document.token == token)
    }

    fn active_document(&self) -> Option<&Document> {
        self.document(self.active_token?)
    }

    fn activate(&mut self, token: u64) -> bool {
        if self.document(token).is_none() {
            return false;
        }
        self.active_token = Some(token);
        self.focus_editor = true;
        self.dismiss_completion();
        true
    }

    fn activate_adjacent(&mut self, backwards: bool) -> bool {
        if self.documents.len() < 2 {
            return false;
        }
        let current = self
            .active_token
            .and_then(|token| {
                self.documents
                    .iter()
                    .position(|document| document.token == token)
            })
            .unwrap_or_default();
        let next = if backwards {
            current
                .checked_sub(1)
                .unwrap_or(self.documents.len().saturating_sub(1))
        } else {
            (current + 1) % self.documents.len()
        };
        self.activate(self.documents[next].token)
    }

    /// 应用异步读取结果。按 token 路由到对应标签；关闭后的陈旧回包
    /// 会被静默丢弃，后台标签加载完成也不会抢走当前编辑焦点。
    pub fn apply_loaded(&mut self, token: u64, result: Result<Vec<u8>, String>) -> bool {
        let is_active = self.active_token == Some(token);
        let Some(document) = self
            .document_mut(token)
            .filter(|document| document.source_valid)
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
                if is_active {
                    self.focus_editor = true;
                }
            }
            Err(error) => {
                document.state = LoadState::Error;
                document.error = Some(error);
            }
        }
        self.language_caches.remove(&token);
        if self
            .completion
            .as_ref()
            .is_some_and(|completion| completion.token == token)
        {
            self.dismiss_completion();
        }
        true
    }

    /// 标记保存请求已经交给传输层。若当前文档或 token 已变化则拒绝。
    pub fn mark_saving(&mut self, request: &SaveRequest) -> bool {
        let Some(document) = self.document_mut(request.token).filter(|doc| {
            doc.token == request.token && doc.source == request.source && doc.source_valid
        }) else {
            return false;
        };
        document.pending_save = Some(PendingSave {
            token: request.token,
            text: document.text.clone(),
            bytes_sha256: sha256(&request.bytes),
        });
        document.save_conflict = false;
        document.error = None;
        true
    }

    /// 应用异步保存结果。保存期间继续输入是允许的；成功时仅把已写入
    /// 的快照作为新基线，之后输入仍保持“未保存”状态。
    pub fn apply_saved(&mut self, token: u64, result: Result<(), SaveFailure>) -> bool {
        let Some(document) = self
            .document_mut(token)
            .filter(|document| document.source_valid)
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
        let (is_conflict, is_message_error) = match result {
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
                document.save_conflict = false;
                (false, false)
            }
            Err(SaveFailure::Conflict) => {
                document.error = None;
                document.pending_save = Some(pending);
                document.save_conflict = true;
                (true, false)
            }
            Err(SaveFailure::Message(message)) => {
                document.error = Some(message);
                document.save_conflict = false;
                (false, true)
            }
        };
        if is_conflict {
            self.active_token = Some(token);
        } else if is_message_error
            && self
                .post_save_close
                .is_some_and(|intent| intent.token == token)
        {
            self.pending_close = self.post_save_close.take();
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
        let message = message.into();
        let mut changed = false;
        for document in self
            .documents
            .iter_mut()
            .filter(|document| &document.source == source && document.source_valid)
        {
            document.source_valid = false;
            document.pending_save = None;
            document.save_conflict = false;
            document.error = Some(message.clone());
            if document.state == LoadState::Loading {
                document.state = LoadState::Error;
            }
            changed = true;
        }
        if changed {
            self.dismiss_completion();
        }
        changed
    }

    pub fn close_without_prompt(&mut self) {
        self.documents.clear();
        self.active_token = None;
        self.visible = false;
        self.pending_close = None;
        self.post_save_close = None;
        self.close_editor_discarded.clear();
        self.find_open = false;
        self.focus_find = false;
        self.find_query.clear();
        self.replace_query.clear();
        self.find_replace_open = false;
        self.goto_open = false;
        self.goto_query.clear();
        self.focus_goto = false;
        self.pending_scroll = None;
        self.language_caches.clear();
        self.dismiss_completion();
    }

    fn cancel_close_flow(&mut self) {
        self.pending_close = None;
        self.post_save_close = None;
        self.close_editor_discarded.clear();
    }

    fn dismiss_completion(&mut self) {
        self.completion = None;
        self.pending_completion = None;
        self.ime_composing = false;
    }

    fn remove_document(&mut self, token: u64) -> bool {
        let Some(index) = self
            .documents
            .iter()
            .position(|document| document.token == token)
        else {
            return false;
        };
        let was_active = self.active_token == Some(token);
        self.documents.remove(index);
        self.language_caches.remove(&token);
        if self
            .completion
            .as_ref()
            .is_some_and(|completion| completion.token == token)
            || self
                .pending_completion
                .as_ref()
                .is_some_and(|completion| completion.token == token)
        {
            self.dismiss_completion();
        }
        self.close_editor_discarded.remove(&token);
        if self.documents.is_empty() {
            self.active_token = None;
            self.visible = false;
            self.find_open = false;
            self.focus_find = false;
            self.find_query.clear();
            self.replace_query.clear();
            self.find_replace_open = false;
            self.goto_open = false;
            self.goto_query.clear();
            self.focus_goto = false;
            self.pending_scroll = None;
        } else if was_active {
            let next_index = index.min(self.documents.len() - 1);
            self.active_token = Some(self.documents[next_index].token);
            self.focus_editor = true;
        }
        true
    }

    fn needs_close_decision(document: &Document) -> bool {
        document.dirty() || document.pending_save.is_some()
    }

    fn request_close_tab(&mut self, token: u64, output: &mut Output) {
        let Some(document) = self.document(token) else {
            return;
        };
        if Self::needs_close_decision(document) {
            self.active_token = Some(token);
            self.pending_close = Some(CloseIntent {
                scope: CloseScope::Tab,
                token,
            });
            self.post_save_close = None;
            self.close_editor_discarded.clear();
        } else {
            self.remove_document(token);
            output.closed = self.documents.is_empty();
        }
    }

    fn request_close_editor(&mut self, output: &mut Output) {
        self.pending_close = None;
        self.post_save_close = None;
        self.close_editor_discarded.clear();
        self.advance_close_editor(output);
    }

    fn advance_close_editor(&mut self, output: &mut Output) {
        let next = self
            .documents
            .iter()
            .find(|document| {
                let discarded_unchanged = self
                    .close_editor_discarded
                    .get(&document.token)
                    .is_some_and(|fingerprint| *fingerprint == sha256(document.text.as_bytes()));
                Self::needs_close_decision(document) && !discarded_unchanged
            })
            .map(|document| document.token);
        if let Some(token) = next {
            self.active_token = Some(token);
            self.pending_close = Some(CloseIntent {
                scope: CloseScope::Editor,
                token,
            });
            self.focus_editor = true;
        } else {
            self.close_without_prompt();
            output.closed = true;
        }
    }

    fn conflict_token(&self) -> Option<u64> {
        self.active_document()
            .filter(|document| document.save_conflict)
            .map(|document| document.token)
            .or_else(|| {
                self.documents
                    .iter()
                    .find(|document| document.save_conflict)
                    .map(|document| document.token)
            })
    }

    fn build_save_request(&mut self, force: bool) -> Option<SaveRequest> {
        let token = self.active_token?;
        self.build_save_request_for(token, force)
    }

    fn build_save_request_for(&mut self, token: u64, force: bool) -> Option<SaveRequest> {
        let document = self.document_mut(token).filter(|doc| {
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
    pub hidden: bool,
}

/// 在中央工作区绘制编辑器。侧栏和目录树仍可见，终端继续在后台运行，
/// 但调用方应把键盘/IME 焦点交给 egui。
pub fn show(ui: &mut egui::Ui, state: &mut TextEditorState, pal: &Palette) -> Output {
    let mut output = Output::default();
    if !state.is_visible() {
        return output;
    }
    finish_deferred_action(state, &mut output);
    if output.closed {
        return output;
    }
    if state.active_document().is_none() {
        return output;
    }

    let previous_tab_shortcut = egui::KeyboardShortcut::new(
        egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        egui::Key::Tab,
    );
    let next_tab_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Tab);
    let close_tab_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::W);
    let previous_tab = ui.input_mut(|input| input.consume_shortcut(&previous_tab_shortcut));
    let next_tab =
        !previous_tab && ui.input_mut(|input| input.consume_shortcut(&next_tab_shortcut));
    let close_tab = ui.input_mut(|input| input.consume_shortcut(&close_tab_shortcut));
    if state.pending_close.is_none() && state.conflict_token().is_none() {
        if previous_tab || next_tab {
            state.activate_adjacent(previous_tab);
        }
        if close_tab {
            if let Some(token) = state.active_token {
                state.request_close_tab(token, &mut output);
            }
        }
    }
    if output.closed || !state.is_open() {
        return output;
    }

    let save_shortcut =
        ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::S));
    let find_shortcut =
        ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::F));
    let replace_shortcut =
        ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::H));
    let goto_shortcut =
        ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::G));
    let wrap_shortcut = ui.input(|input| input.modifiers.alt && input.key_pressed(egui::Key::Z));
    let active_state = state.active_document().map(|document| document.state);
    if active_state == Some(LoadState::Ready) {
        if find_shortcut || replace_shortcut {
            state.find_open = true;
            state.focus_find = true;
            state.find_replace_open |= replace_shortcut;
            state.goto_open = false;
            state.focus_goto = false;
            state.dismiss_completion();
        }
        if goto_shortcut {
            state.goto_open = true;
            state.focus_goto = true;
            state.find_open = false;
            state.focus_find = false;
            state.dismiss_completion();
        }
        if wrap_shortcut {
            state.wrap = !state.wrap;
        }
    }
    if state.pending_close.is_some() || state.conflict_token().is_some() {
        state.dismiss_completion();
    }

    egui::Frame::new()
        .fill(pal.bg_panel)
        .inner_margin(egui::Margin::same(0))
        .show(ui, |ui| {
            editor_header(ui, state, pal, &mut output);
            if output.closed || output.hidden || !state.is_open() {
                return;
            }
            if state.find_open {
                find_bar(ui, state, pal);
            }
            if state.goto_open {
                goto_bar(ui, state, pal);
            }
            ui.separator();

            let Some((token, load_state, source_valid, error, source)) =
                state.active_document().map(|document| {
                    (
                        document.token,
                        document.state,
                        document.source_valid,
                        document.error.clone(),
                        document.source.clone(),
                    )
                })
            else {
                return;
            };
            match load_state {
                LoadState::Loading => {
                    centered_status(ui, crate::i18n::strings().filetree_loading, pal)
                }
                LoadState::Error => {
                    let message = error
                        .as_deref()
                        .unwrap_or(crate::i18n::strings().ssh_status_error);
                    centered_error(ui, message, pal);
                    if source_valid {
                        ui.vertical_centered(|ui| {
                            if ui
                                .button(crate::i18n::strings().remote_refresh_dir_tip)
                                .clicked()
                            {
                                output.load = Some(LoadRequest { token, source });
                                if let Some(document) = state.document_mut(token) {
                                    document.state = LoadState::Loading;
                                    document.error = None;
                                }
                            }
                        });
                    }
                }
                LoadState::Ready => {
                    editor_body(ui, state, pal);
                }
            }
        });

    if output.hidden {
        return output;
    }
    if save_shortcut
        && output.save.is_none()
        && state.pending_close.is_none()
        && state.conflict_token().is_none()
    {
        output.save = state.build_save_request(false);
    }
    decision_modal(ui.ctx(), state, pal, &mut output);
    output
}

fn finish_deferred_action(state: &mut TextEditorState, output: &mut Output) {
    let intent = state.post_save_close.or(state.pending_close);
    let Some(intent) = intent else {
        return;
    };
    let Some(document) = state.document(intent.token) else {
        if state.post_save_close == Some(intent) {
            state.post_save_close = None;
        }
        if state.pending_close == Some(intent) {
            state.pending_close = None;
        }
        if intent.scope == CloseScope::Editor {
            state.advance_close_editor(output);
        }
        return;
    };
    if document.pending_save.is_some() || document.save_conflict {
        return;
    }
    if document.dirty() {
        if state.post_save_close == Some(intent) {
            state.post_save_close = None;
            state.pending_close = Some(intent);
        }
        return;
    }

    if state.post_save_close == Some(intent) {
        state.post_save_close = None;
    }
    if state.pending_close == Some(intent) {
        state.pending_close = None;
    }
    match intent.scope {
        CloseScope::Tab => {
            state.remove_document(intent.token);
            output.closed = !state.is_open();
        }
        CloseScope::Editor => state.advance_close_editor(output),
    }
}

#[derive(Clone)]
struct TabSnapshot {
    token: u64,
    name: String,
    path: String,
    dirty: bool,
    active: bool,
}

fn editor_hide_button(ui: &mut egui::Ui, pal: &Palette) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(4), pal.bg_highlight);
    }
    let color = if response.hovered() {
        pal.fg
    } else {
        pal.fg_dim
    };
    ui.painter().line_segment(
        [
            egui::pos2(rect.center().x - 5.0, rect.center().y + 3.0),
            egui::pos2(rect.center().x + 5.0, rect.center().y + 3.0),
        ],
        egui::Stroke::new(1.4_f32, color),
    );
    response
}

fn editor_header(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    pal: &Palette,
    output: &mut Output,
) {
    let tabs = state
        .documents
        .iter()
        .map(|document| TabSnapshot {
            token: document.token,
            name: document.source.display_name().to_owned(),
            path: document.source.path().to_owned(),
            dirty: document.dirty(),
            active: state.active_token == Some(document.token),
        })
        .collect::<Vec<_>>();
    let mut activate = None;
    let mut close_tab = None;
    let mut hide_editor = false;
    let mut close_editor = false;
    let close_editor_tip = if state.is_dirty() {
        crate::i18n::strings().text_editor_unsaved_title
    } else {
        crate::i18n::strings().menu_close
    };

    egui::Frame::new()
        .fill(pal.bg_dark)
        // 原顶部 3px 调整为 -2px，即 Tab 行整体上移 5px；
        // 底部同步补偿到 8px，保持标题栏总高度和下方布局不变。
        .inner_margin(egui::Margin {
            left: 4,
            right: 4,
            top: -2,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let tabs_width = (ui.available_width() - 60.0).max(96.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(tabs_width, 30.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("lumen_text_editor_tabs")
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    for tab in &tabs {
                                        egui::Frame::new()
                                            .fill(if tab.active {
                                                pal.bg_panel
                                            } else {
                                                pal.bg_dark
                                            })
                                            .stroke(egui::Stroke::new(
                                                1.0_f32,
                                                if tab.active {
                                                    pal.panel_outline
                                                } else {
                                                    egui::Color32::TRANSPARENT
                                                },
                                            ))
                                            .corner_radius(egui::CornerRadius::same(4))
                                            .inner_margin(egui::Margin::symmetric(7, 3))
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    let title = if tab.dirty {
                                                        format!("● {}", tab.name)
                                                    } else {
                                                        tab.name.clone()
                                                    };
                                                    if ui
                                                        .add(
                                                            egui::Button::new(
                                                                egui::RichText::new(title)
                                                                    .size(11.0)
                                                                    .color(if tab.active {
                                                                        pal.fg
                                                                    } else {
                                                                        pal.fg_dim
                                                                    }),
                                                            )
                                                            .frame(false),
                                                        )
                                                        .on_hover_text(&tab.path)
                                                        .clicked()
                                                    {
                                                        activate = Some(tab.token);
                                                    }
                                                    if ui
                                                        .add(
                                                            egui::Button::new(
                                                                egui::RichText::new("×")
                                                                    .size(12.0)
                                                                    .color(pal.fg_dim),
                                                            )
                                                            .frame(false),
                                                        )
                                                        .on_hover_text(
                                                            crate::i18n::strings().menu_close,
                                                        )
                                                        .clicked()
                                                    {
                                                        close_tab = Some(tab.token);
                                                    }
                                                });
                                            });
                                    }
                                });
                            });
                    },
                );
                if editor_hide_button(ui, pal)
                    .on_hover_text(crate::i18n::strings().text_editor_hide)
                    .clicked()
                {
                    hide_editor = true;
                }
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("×").size(14.0).color(pal.fg_dim))
                            .frame(false),
                    )
                    .on_hover_text(close_editor_tip)
                    .clicked()
                {
                    close_editor = true;
                }
            });
        });

    if let Some(token) = activate {
        state.activate(token);
    }
    if let Some(token) = close_tab {
        state.request_close_tab(token, output);
    }
    if hide_editor && state.hide() {
        output.hidden = true;
        ui.ctx().memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }
    if close_editor {
        state.request_close_editor(output);
    }
    if output.closed || output.hidden {
        return;
    }

    let Some(source) = state
        .active_document()
        .map(|document| document.source.clone())
    else {
        return;
    };
    let source_label = if source.is_ssh() { "SSH" } else { "REMOTE" };
    egui::Frame::new()
        .fill(pal.bg_panel)
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(source_label)
                        .monospace()
                        .size(10.0)
                        .color(pal.accent),
                );
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
        });
}

/// 设置编辑器选区并取回焦点（查找跳转、跳转行共用）。
fn select_editor_range(ctx: &egui::Context, editor_id: egui::Id, range: std::ops::Range<usize>) {
    let mut edit_state =
        egui::widgets::text_edit::TextEditState::load(ctx, editor_id).unwrap_or_default();
    edit_state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(range.start),
            egui::text::CCursor::new(range.end),
        )));
    edit_state.store(ctx, editor_id);
    ctx.memory_mut(|memory| memory.request_focus(editor_id));
}

fn editor_selection(ctx: &egui::Context, editor_id: egui::Id) -> std::ops::Range<usize> {
    egui::widgets::text_edit::TextEditState::load(ctx, editor_id)
        .and_then(|state| state.cursor.char_range())
        .map(|range| range.as_sorted_char_range())
        .unwrap_or(0..0)
}

/// 跳到下一个（next=true）或上一个匹配；越过文档端点时回绕。
fn find_jump(
    ctx: &egui::Context,
    state: &mut TextEditorState,
    token: u64,
    editor_id: egui::Id,
    next: bool,
) {
    let selection = editor_selection(ctx, editor_id);
    let target = {
        let TextEditorState {
            documents,
            language_caches,
            find_query,
            find_case_sensitive,
            ..
        } = state;
        let Some(document) = documents.iter().find(|document| document.token == token) else {
            return;
        };
        let matches = language_caches.entry(token).or_default().find_matches(
            &document.text,
            find_query,
            *find_case_sensitive,
        );
        if matches.is_empty() {
            None
        } else if next {
            matches
                .iter()
                .find(|m| m.start >= selection.end)
                .or_else(|| matches.first())
        } else {
            matches
                .iter()
                .rev()
                .find(|m| m.end <= selection.start)
                .or_else(|| matches.last())
        }
        .cloned()
    };
    if let Some(range) = target {
        select_editor_range(ctx, editor_id, range.clone());
        state.pending_scroll = Some((token, range.start));
    }
}

/// 替换当前选中的匹配；选区不是匹配时改为跳到下一个匹配。
fn replace_current(
    ctx: &egui::Context,
    state: &mut TextEditorState,
    token: u64,
    editor_id: egui::Id,
) {
    let selection = editor_selection(ctx, editor_id);
    let plan = {
        let TextEditorState {
            documents,
            language_caches,
            find_query,
            find_case_sensitive,
            replace_query,
            ..
        } = state;
        let Some(document) = documents.iter().find(|document| document.token == token) else {
            return;
        };
        let matches = language_caches.entry(token).or_default().find_matches(
            &document.text,
            find_query,
            *find_case_sensitive,
        );
        if matches.contains(&selection) {
            let cursor = selection.start + replace_query.chars().count();
            Some(EditPlan {
                replace_chars: selection,
                insertion: replace_query.clone(),
                selection_after: cursor..cursor,
            })
        } else {
            None
        }
    };
    if let Some(plan) = plan {
        queue_edit_plan(state, token, plan);
    } else {
        find_jump(ctx, state, token, editor_id, true);
    }
}

/// 全部替换：整个缓冲一次注入（单条可撤销）。
fn replace_all_matches(state: &mut TextEditorState, token: u64) {
    let plan = {
        let TextEditorState {
            documents,
            language_caches,
            find_query,
            find_case_sensitive,
            replace_query,
            ..
        } = state;
        let Some(document) = documents.iter().find(|document| document.token == token) else {
            return;
        };
        let matches = language_caches
            .entry(token)
            .or_default()
            .find_matches(&document.text, find_query, *find_case_sensitive)
            .to_vec();
        if matches.is_empty() {
            None
        } else {
            let new_text =
                text_editor_ops::replace_matches(&document.text, &matches, replace_query);
            let cursor = matches[0].start.min(text_editor_ops::char_count(&new_text));
            Some(EditPlan {
                replace_chars: 0..text_editor_ops::char_count(&document.text),
                insertion: new_text,
                selection_after: cursor..cursor,
            })
        }
    };
    if let Some(plan) = plan {
        queue_edit_plan(state, token, plan);
    }
}

fn find_bar(ui: &mut egui::Ui, state: &mut TextEditorState, pal: &Palette) {
    let Some(token) = state.active_token else {
        return;
    };
    let editor_id = ui.make_persistent_id(("lumen_text_editor", token));
    let mut jump_next = None;
    let mut do_replace = false;
    let mut do_replace_all = false;
    egui::Frame::new()
        .fill(pal.bg_dark)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let toggle = if state.find_replace_open {
                    "▾"
                } else {
                    "▸"
                };
                if ui.small_button(toggle).clicked() {
                    state.find_replace_open = !state.find_replace_open;
                }
                let response = ui.add_sized(
                    [220.0, 24.0],
                    egui::TextEdit::singleline(&mut state.find_query)
                        .hint_text(crate::i18n::strings().text_editor_find_hint)
                        .desired_width(220.0),
                );
                if state.focus_find {
                    response.request_focus();
                    state.focus_find = false;
                }
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    jump_next = Some(!ui.input(|input| input.modifiers.shift));
                    response.request_focus();
                }
                if (response.has_focus() || response.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Escape))
                {
                    state.find_open = false;
                    state.focus_find = false;
                }
                if ui
                    .selectable_label(state.find_case_sensitive, "Aa")
                    .on_hover_text(crate::i18n::strings().text_editor_case_sensitive)
                    .clicked()
                {
                    state.find_case_sensitive = !state.find_case_sensitive;
                }
                if ui
                    .small_button("↑")
                    .on_hover_text(crate::i18n::strings().text_editor_prev_match)
                    .clicked()
                {
                    jump_next = Some(false);
                }
                if ui
                    .small_button("↓")
                    .on_hover_text(crate::i18n::strings().text_editor_next_match)
                    .clicked()
                {
                    jump_next = Some(true);
                }
                // n/m：光标之后结束的第一个匹配算“当前”。
                let (current, total) = {
                    let TextEditorState {
                        documents,
                        language_caches,
                        find_query,
                        find_case_sensitive,
                        ..
                    } = state;
                    documents
                        .iter()
                        .find(|document| document.token == token)
                        .map(|document| {
                            let matches = language_caches.entry(token).or_default().find_matches(
                                &document.text,
                                find_query,
                                *find_case_sensitive,
                            );
                            let selection = editor_selection(ui.ctx(), editor_id);
                            let current = matches
                                .iter()
                                .position(|m| m.end > selection.start)
                                .map_or(0, |index| index + 1);
                            (current, matches.len())
                        })
                        .unwrap_or((0, 0))
                };
                ui.label(
                    egui::RichText::new(format!("{current}/{total}"))
                        .monospace()
                        .size(11.0)
                        .color(pal.fg_dim),
                );
                if ui.small_button("×").clicked() {
                    state.find_open = false;
                    state.focus_find = false;
                }
            });
            if state.find_replace_open {
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    let response = ui.add_sized(
                        [220.0, 24.0],
                        egui::TextEdit::singleline(&mut state.replace_query)
                            .hint_text(crate::i18n::strings().text_editor_replace)
                            .desired_width(220.0),
                    );
                    if response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        do_replace = true;
                        response.request_focus();
                    }
                    if (response.has_focus() || response.lost_focus())
                        && ui.input(|input| input.key_pressed(egui::Key::Escape))
                    {
                        state.find_open = false;
                        state.focus_find = false;
                    }
                    if ui
                        .button(crate::i18n::strings().text_editor_replace)
                        .clicked()
                    {
                        do_replace = true;
                    }
                    if ui
                        .button(crate::i18n::strings().text_editor_replace_all)
                        .clicked()
                    {
                        do_replace_all = true;
                    }
                });
            }
        });
    if let Some(next) = jump_next {
        find_jump(ui.ctx(), state, token, editor_id, next);
    }
    if do_replace {
        replace_current(ui.ctx(), state, token, editor_id);
    }
    if do_replace_all {
        replace_all_matches(state, token);
    }
}

/// Ctrl+G 跳转到行的小输入条。
fn goto_bar(ui: &mut egui::Ui, state: &mut TextEditorState, pal: &Palette) {
    let Some(token) = state.active_token else {
        return;
    };
    let editor_id = ui.make_persistent_id(("lumen_text_editor", token));
    let mut jump = false;
    egui::Frame::new()
        .fill(pal.bg_dark)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let response = ui.add_sized(
                    [120.0, 24.0],
                    egui::TextEdit::singleline(&mut state.goto_query)
                        .hint_text(crate::i18n::strings().text_editor_goto_hint)
                        .desired_width(120.0),
                );
                if state.focus_goto {
                    response.request_focus();
                    state.focus_goto = false;
                }
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    jump = true;
                }
                if (response.has_focus() || response.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Escape))
                {
                    state.goto_open = false;
                    state.focus_goto = false;
                }
                if ui.small_button("×").clicked() {
                    state.goto_open = false;
                    state.focus_goto = false;
                }
            });
        });
    if jump {
        if let Ok(line) = state.goto_query.trim().parse::<usize>() {
            if line > 0 {
                if let Some(document) = state.document(token) {
                    let char_idx = text_editor_ops::goto_line_start(&document.text, line);
                    select_editor_range(ui.ctx(), editor_id, char_idx..char_idx);
                    state.pending_scroll = Some((token, char_idx));
                }
            }
        }
        state.goto_open = false;
        state.focus_goto = false;
    }
}

/// editor_body 单帧采集的展示数据（文本块借用结束后供状态栏/覆盖层使用）。
struct EditorFrame {
    response: egui::Response,
    line_count: usize,
    bytes: usize,
    cursor_line: usize,
    cursor_column: usize,
    line_ending: LineEnding,
    utf8_bom: bool,
    language: Language,
    error: Option<String>,
    completion: Option<([u8; 32], CompletionSet, egui::Rect)>,
    viewport_rect: egui::Rect,
    viewport_clicked: bool,
    /// 本帧正文 galley 与其屏幕位置（行号/高亮定位用）。
    galley: std::sync::Arc<egui::Galley>,
    galley_pos: egui::Pos2,
    /// 行号栏宽度（含两侧留白）。
    gutter_width: f32,
    /// 等宽数字字宽与字号（缩进参考线/行号绘制用）。
    char_width: f32,
    font_size: f32,
    /// 当前选区（字符下标，升序）。
    cursor_range: Option<std::ops::Range<usize>>,
}

/// 字符区间的屏幕矩形（软换行跨视觉行时逐行切开）。
fn range_rects(
    galley: &egui::Galley,
    galley_pos: egui::Pos2,
    range: &std::ops::Range<usize>,
) -> Vec<egui::Rect> {
    let start = galley
        .pos_from_cursor(egui::text::CCursor::new(range.start))
        .translate(galley_pos.to_vec2());
    let end = galley
        .pos_from_cursor(egui::text::CCursor::new(range.end))
        .translate(galley_pos.to_vec2());
    if (start.min.y - end.min.y).abs() < 1.0 {
        let right = end.min.x.max(start.min.x + 2.0);
        return vec![egui::Rect::from_min_max(
            start.min,
            egui::pos2(right, start.max.y),
        )];
    }
    let mut out = Vec::new();
    for row in &galley.rows {
        let row_rect = row.rect().translate(galley_pos.to_vec2());
        if row_rect.max.y <= start.min.y || row_rect.min.y >= end.max.y {
            continue;
        }
        let left = if (row_rect.min.y - start.min.y).abs() < 1.0 {
            start.min.x
        } else {
            row_rect.min.x
        };
        let right = if (row_rect.min.y - end.min.y).abs() < 1.0 {
            end.min.x
        } else {
            row_rect.max.x
        };
        out.push(egui::Rect::from_min_max(
            egui::pos2(left, row_rect.min.y),
            egui::pos2(right.max(left + 2.0), row_rect.max.y),
        ));
    }
    out
}

/// 行号栏、当前行、缩进参考线与各类高亮的覆盖绘制。
///
/// 全部画在 TextEdit 之上（半透明）：galley 已含本帧滚动偏移。
/// 行号栏最后以不透明底绘制，遮住横向滚动滑到栏下的正文，保持栏位固定。
fn paint_editor_overlays(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    token: u64,
    frame: &EditorFrame,
    pal: &Palette,
) {
    let viewport = frame.viewport_rect;
    let painter = ui.painter().with_clip_rect(viewport);
    let galley = &frame.galley;
    let galley_pos = frame.galley_pos;
    let text_left = viewport.left() + frame.gutter_width;

    let TextEditorState {
        documents,
        language_caches,
        find_open,
        find_query,
        find_case_sensitive,
        ..
    } = state;
    let Some(document) = documents.iter().find(|document| document.token == token) else {
        return;
    };
    let cache = language_caches.entry(token).or_default();
    let text = &document.text;
    let cursor_line0 = frame.cursor_line.saturating_sub(1);

    // ── 沿 galley 行走可见行：行号、当前行、参考线共用一次遍历 ──
    let unit = detected_indent_unit(text, "", frame.language);
    let unit_cols = if unit == "\t" {
        4
    } else {
        unit.chars().count().max(1)
    };
    let line_starts = cache.line_starts(text).to_vec();
    let mut line_no = 0usize;
    let mut line_levels = 0usize;
    let mut current_line_rows: Vec<egui::Rect> = Vec::new();
    let mut gutter_rows: Vec<(usize, egui::Rect)> = Vec::new();
    let mut guides: Vec<(usize, egui::Rect)> = Vec::new();
    let mut first_visible_line = None;
    let mut last_visible_line = 0usize;
    for (row_idx, row) in galley.rows.iter().enumerate() {
        let is_line_start = row_idx == 0 || galley.rows[row_idx - 1].ends_with_newline;
        if is_line_start {
            let start = line_starts.get(line_no).copied().unwrap_or(usize::MAX);
            let end = line_starts
                .get(line_no + 1)
                .copied()
                .unwrap_or_else(|| text_editor_ops::char_count(text));
            let line_text = text_editor_ops::slice_chars(text, start..end);
            line_levels = text_editor_ops::indent_columns(line_text, unit_cols) / unit_cols;
        }
        let row_rect = row.rect().translate(galley_pos.to_vec2());
        if row_rect.min.y > viewport.bottom() {
            break;
        }
        if row_rect.max.y >= viewport.top() {
            first_visible_line.get_or_insert(line_no);
            last_visible_line = line_no;
            if line_no == cursor_line0 {
                current_line_rows.push(row_rect);
            }
            if is_line_start {
                gutter_rows.push((line_no, row_rect));
            }
            for level in 1..=line_levels.min(8) {
                guides.push((level, row_rect));
            }
        }
        if row.ends_with_newline {
            line_no += 1;
        }
    }

    // ── 当前行高亮（整条视口宽，行号栏部分稍后由栏底盖住）──
    for row_rect in &current_line_rows {
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(viewport.left(), row_rect.min.y),
                egui::pos2(viewport.right(), row_rect.max.y),
            ),
            0.0,
            pal.fg.gamma_multiply(0.05),
        );
    }

    // ── 可见字符区间：匹配类高亮只画可见部分 ──
    let visible_chars = first_visible_line.map_or(0..0, |first| {
        let start = line_starts.get(first).copied().unwrap_or(0);
        let end = line_starts
            .get(last_visible_line + 1)
            .copied()
            .unwrap_or_else(|| text_editor_ops::char_count(text));
        start..end
    });

    // ── 查找匹配高亮（当前选中的匹配更亮）──
    if *find_open && !find_query.is_empty() {
        let selected = frame.cursor_range.clone();
        let matches: Vec<std::ops::Range<usize>> = cache
            .find_matches(text, find_query, *find_case_sensitive)
            .iter()
            .filter(|m| visible_chars.contains(&m.start))
            .take(500)
            .cloned()
            .collect();
        for m in &matches {
            let fill = if selected.as_ref() == Some(m) {
                pal.accent.gamma_multiply(0.55)
            } else {
                pal.accent.gamma_multiply(0.28)
            };
            for rect in range_rects(galley, galley_pos, m) {
                painter.rect_filled(rect, egui::CornerRadius::same(2), fill);
            }
        }
    }

    // ── 选中词出现高亮 ──
    if let Some(sel) = frame
        .cursor_range
        .clone()
        .filter(|sel| !sel.is_empty() && sel.end - sel.start <= 64)
    {
        let word = text_editor_ops::slice_chars(text, sel.clone());
        if !word.is_empty()
            && !word.chars().any(char::is_whitespace)
            && word.chars().next().is_some_and(|ch| {
                super::text_editor_language::is_identifier_start(frame.language, ch)
            })
        {
            let occurrences: Vec<std::ops::Range<usize>> = cache
                .occurrences(text, word, frame.language)
                .iter()
                .filter(|m| visible_chars.contains(&m.start))
                .take(300)
                .cloned()
                .collect();
            for m in &occurrences {
                for rect in range_rects(galley, galley_pos, m) {
                    painter.rect_filled(
                        rect,
                        egui::CornerRadius::same(2),
                        pal.fg.gamma_multiply(0.16),
                    );
                }
            }
        }
    }

    // ── 括号匹配（失配用错误色）──
    if let Some(sel) = frame.cursor_range.clone().filter(|sel| sel.is_empty()) {
        if let Some((bracket, matched)) = cache.bracket_at(text, sel.start) {
            let fill = if matched.is_some() {
                pal.accent.gamma_multiply(0.40)
            } else {
                pal.error.gamma_multiply(0.55)
            };
            for idx in [bracket, matched.unwrap_or(bracket)] {
                for rect in range_rects(galley, galley_pos, &(idx..idx + 1)) {
                    painter.rect_filled(rect, egui::CornerRadius::same(2), fill);
                }
            }
        }
    }

    // ── 缩进参考线 ──
    for (level, row_rect) in &guides {
        let x = text_left + (level * unit_cols) as f32 * frame.char_width;
        if x >= viewport.right() {
            continue;
        }
        painter.line_segment(
            [egui::pos2(x, row_rect.min.y), egui::pos2(x, row_rect.max.y)],
            egui::Stroke::new(1.0_f32, pal.fg_dim.gamma_multiply(0.22)),
        );
    }

    // ── 行号栏（最后画，保持栏位固定）──
    let gutter_rect =
        egui::Rect::from_min_max(viewport.min, egui::pos2(text_left, viewport.bottom()));
    painter.rect_filled(gutter_rect, 0.0, pal.bg_dark);
    let font_id = egui::FontId::monospace(frame.font_size);
    for (line, row_rect) in &gutter_rows {
        let color = if *line == cursor_line0 {
            pal.fg
        } else {
            pal.fg_dim
        };
        painter.text(
            egui::pos2(text_left - 8.0, row_rect.center().y),
            egui::Align2::RIGHT_CENTER,
            (line + 1).to_string(),
            font_id.clone(),
            color,
        );
    }
    painter.line_segment(
        [
            egui::pos2(text_left - 0.5, viewport.top()),
            egui::pos2(text_left - 0.5, viewport.bottom()),
        ],
        egui::Stroke::new(1.0_f32, pal.panel_outline.gamma_multiply(0.6)),
    );
}

fn editor_body(ui: &mut egui::Ui, state: &mut TextEditorState, pal: &Palette) {
    let available = ui.available_size();
    let error_height = if state
        .active_document()
        .and_then(|document| document.error.as_ref())
        .is_some()
    {
        EDITOR_ERROR_HEIGHT
    } else {
        0.0
    };
    let editor_height = (available.y - EDITOR_STATUS_HEIGHT - error_height).max(0.0);
    let focus_editor = std::mem::take(&mut state.focus_editor);
    let Some(token) = state.active_token else {
        return;
    };
    let editor_id = ui.make_persistent_id(("lumen_text_editor", token));
    let input_language = state
        .active_document()
        .map(|document| Language::from_path_and_text(document.source.path(), &document.text))
        .unwrap_or(Language::PlainText);
    let (explicit_completion, injected_edit, suppress_completion) =
        prepare_completion_input(ui, state, token, editor_id, input_language);
    let editor_edit_applied = injected_edit.is_some();
    let had_completion = state
        .completion
        .as_ref()
        .is_some_and(|completion| completion.token == token);
    let ime_composing = state.ime_composing;
    let wrap = state.wrap;
    // 查找/跳转的滚动目标：只消费属于当前标签的，其他放回去。
    let pending_scroll = match state.pending_scroll.take() {
        Some((pending_token, char_idx)) if pending_token == token => Some(char_idx),
        other => {
            state.pending_scroll = other;
            None
        }
    };

    let frame = {
        let TextEditorState {
            documents,
            language_caches,
            ..
        } = state;
        let Some(document) = documents
            .iter_mut()
            .find(|document| document.token == token)
        else {
            return;
        };
        let language = Language::from_path_and_text(document.source.path(), &document.text);
        let cache = language_caches.entry(token).or_default();
        // 先固定分配编辑器视口，再让正文只在内部滚动。TextEdit 的
        // min_size.y 在 egui 0.34 不会约束高度，不能依赖正文控件自行撑开。
        let (editor_rect, editor_response) = ui.allocate_exact_size(
            egui::vec2(available.x.max(0.0), editor_height),
            egui::Sense::click(),
        );
        ui.painter().rect_filled(editor_rect, 0.0, pal.bg_panel);
        let inset_x = EDITOR_INNER_MARGIN_X.min(editor_rect.width().max(0.0) * 0.5);
        let inset_y = EDITOR_INNER_MARGIN_Y.min(editor_rect.height().max(0.0) * 0.5);
        let viewport_rect = editor_rect.shrink2(egui::vec2(inset_x, inset_y));
        let mut editor_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(viewport_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        let row_height = editor_ui
            .text_style_height(&egui::TextStyle::Monospace)
            .max(1.0);
        // TextEdit 默认上下各 2px 内边距；扣除后向下取整，避免空文档
        // 也因为一行的舍入误差常驻垂直滚动条。
        let desired_rows = ((viewport_rect.height() - 4.0).max(row_height) / row_height)
            .floor()
            .max(1.0) as usize;
        // 行号栏宽度：行数位数（至少 3 位）× 数字宽 + 两侧留白。
        let line_count = document.text.lines().count().max(1);
        let font_size = editor_ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Monospace)
            .map_or(13.0, |font| font.size);
        let char_width = editor_ui
            .fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap(
                        "0".to_owned(),
                        egui::FontId::monospace(font_size),
                        egui::Color32::WHITE,
                    )
                    .size()
                    .x
            })
            .max(1.0);
        let digits = line_count.max(999).to_string().len();
        let gutter_width =
            (digits as f32 * char_width + 16.0).min((viewport_rect.width() * 0.5).max(0.0));
        let text_width = (viewport_rect.width() - gutter_width).max(20.0);
        let scroll_output = egui::ScrollArea::both()
            .id_salt(("lumen_text_editor_scroll", token))
            .auto_shrink([false, false])
            .max_width(viewport_rect.width())
            .max_height(viewport_rect.height())
            .min_scrolled_width(viewport_rect.width())
            .min_scrolled_height(viewport_rect.height())
            .show(&mut editor_ui, |ui| {
                let edit_output = ui
                    .horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        // 行号栏预留的固定宽度；栏本身在滚动区外绘制，不随横向滚动。
                        ui.add_space(gutter_width);
                        let mut layouter =
                            |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap_width: f32| {
                                cache.layout(ui, buffer.as_str(), language, pal, wrap_width)
                            };
                        egui::TextEdit::multiline(&mut document.text)
                            .id(editor_id)
                            .font(egui::TextStyle::Monospace)
                            .code_editor()
                            .desired_width(if wrap { text_width } else { f32::INFINITY })
                            .desired_rows(desired_rows)
                            .min_size(egui::vec2(text_width, 0.0))
                            .lock_focus(true)
                            .layouter(&mut layouter)
                            .show(ui)
                    })
                    .inner;
                if let Some(target) = pending_scroll {
                    let caret = edit_output
                        .galley
                        .pos_from_cursor(egui::text::CCursor::new(target))
                        .translate(edit_output.galley_pos.to_vec2());
                    ui.scroll_to_rect(caret, Some(egui::Align::Center));
                }
                edit_output
            });
        let mut edit_output = scroll_output.inner;
        let visible_text_rect = scroll_output.inner_rect.intersect(viewport_rect);
        if edit_output.response.changed() {
            if let Some(post_selection) = injected_edit
                .as_ref()
                .and_then(|injected| injected.post_selection.clone())
            {
                let selection = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(post_selection.start),
                    egui::text::CCursor::new(post_selection.end),
                );
                edit_output.state.cursor.set_char_range(Some(selection));
                edit_output.state.clone().store(ui.ctx(), editor_id);
                edit_output.cursor_range = Some(selection);
            }
        }

        let cursor = edit_output.cursor_range.and_then(|range| range.single());
        let cursor_char = cursor.map_or(0, |cursor| cursor.index);
        let (cursor_line, cursor_column) = cursor_line_column(&document.text, cursor_char);
        let cursor_range = edit_output
            .cursor_range
            .map(|range| range.as_sorted_char_range());
        let should_complete = !ime_composing
            && !suppress_completion
            && !editor_edit_applied
            && (explicit_completion || edit_output.response.changed() || had_completion);
        let completion = if should_complete {
            cursor.and_then(|cursor| {
                let local_caret = edit_output.galley.pos_from_cursor(cursor);
                let caret_rect = local_caret.translate(edit_output.galley_pos.to_vec2());
                if !caret_rect.intersects(visible_text_rect) {
                    return None;
                }
                cache
                    .completions(&document.text, cursor.index, language, explicit_completion)
                    .map(|set| (sha256(document.text.as_bytes()), set, caret_rect))
            })
        } else {
            None
        };
        EditorFrame {
            response: edit_output.response.response,
            line_count,
            bytes: document.text.len(),
            cursor_line,
            cursor_column,
            line_ending: document.line_ending,
            utf8_bom: document.utf8_bom,
            language,
            error: document.error.clone(),
            completion,
            viewport_rect,
            viewport_clicked: editor_response.clicked(),
            galley: std::sync::Arc::clone(&edit_output.galley),
            galley_pos: edit_output.galley_pos,
            gutter_width,
            char_width,
            font_size,
            cursor_range,
        }
    };

    paint_editor_overlays(ui, state, token, &frame, pal);

    if focus_editor || frame.viewport_clicked {
        frame.response.request_focus();
    }
    if ime_composing || suppress_completion || editor_edit_applied {
        state.completion = None;
    } else if explicit_completion || frame.response.changed() || had_completion {
        state.completion = frame.completion.map(|(fingerprint, set, caret_rect)| {
            let selected = state
                .completion
                .as_ref()
                .filter(|completion| completion.token == token)
                .map_or(0, |completion| {
                    completion.selected.min(set.items.len().saturating_sub(1))
                });
            CompletionPopup {
                token,
                fingerprint,
                set,
                selected,
                caret_rect,
            }
        });
    }

    egui::Frame::new()
        .fill(pal.bg_dark)
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Ln {}, Col {}",
                        frame.cursor_line, frame.cursor_column
                    ))
                    .monospace()
                    .size(10.0)
                    .color(pal.fg_dim),
                );
                ui.separator();
                ui.label(
                    egui::RichText::new(crate::i18n::fmt2(
                        crate::i18n::strings().text_editor_stats_fmt,
                        frame.line_count,
                        frame.bytes,
                    ))
                    .monospace()
                    .size(10.0)
                    .color(pal.fg_dim),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(frame.language.label())
                            .monospace()
                            .size(10.0)
                            .color(pal.fg_dim),
                    )
                    .on_hover_text(crate::i18n::strings().text_editor_completion_hint);
                    ui.label(
                        egui::RichText::new(frame.line_ending.label())
                            .monospace()
                            .size(10.0)
                            .color(pal.fg_dim),
                    );
                    ui.label(
                        egui::RichText::new(if frame.utf8_bom { "UTF-8 BOM" } else { "UTF-8" })
                            .monospace()
                            .size(10.0)
                            .color(pal.fg_dim),
                    );
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(crate::i18n::strings().text_editor_wrap)
                                    .monospace()
                                    .size(10.0)
                                    .color(if state.wrap { pal.accent } else { pal.fg_dim }),
                            )
                            .frame(false),
                        )
                        .on_hover_text("Alt+Z")
                        .clicked()
                    {
                        state.wrap = !state.wrap;
                    }
                });
            });
        });
    if let Some(error) = &frame.error {
        ui.colored_label(pal.error, error);
    }

    let popup_snapshot = state.completion.clone();
    if let Some(popup) = popup_snapshot.as_ref() {
        let popup_output = completion_popup(ui.ctx(), popup, pal);
        if let Some(selected) = popup_output.hovered {
            if let Some(current) = state
                .completion
                .as_mut()
                .filter(|completion| completion.token == popup.token)
            {
                current.selected = selected;
            }
        }
        if let Some(selected) = popup_output.accepted {
            queue_completion(state, popup, selected);
            ui.ctx().request_repaint();
        } else if popup_output.clicked_outside_editor(frame.viewport_rect) {
            state.completion = None;
        }
    }
}

struct CompletionPopupOutput {
    accepted: Option<usize>,
    hovered: Option<usize>,
    rect: egui::Rect,
    pointer_clicked: bool,
    pointer_position: Option<egui::Pos2>,
}

impl CompletionPopupOutput {
    fn clicked_outside_editor(&self, editor_rect: egui::Rect) -> bool {
        self.pointer_clicked
            && self.pointer_position.is_some_and(|position| {
                !self.rect.contains(position) && !editor_rect.contains(position)
            })
    }
}

fn prepare_completion_input(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    token: u64,
    editor_id: egui::Id,
    language: Language,
) -> (bool, Option<InjectedEditorEdit>, bool) {
    let ime_frame = update_ime_state(ui, state);
    if ime_frame.composing || ime_frame.had_event {
        state.completion = None;
        state.pending_completion = None;
        return (false, None, true);
    }

    // 行操作 / 注释 / 缩进 / 括号自动闭合（仅编辑器聚焦时生效）。
    prepare_editor_ops(ui, state, token, editor_id, language);

    let explicit_shortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Space);
    let explicit = ui.input_mut(|input| input.consume_shortcut(&explicit_shortcut));

    let popup_matches = state
        .completion
        .as_ref()
        .is_some_and(|completion| completion.token == token);
    if popup_matches {
        let plain_modifiers = ui.input(|input| input.modifiers == egui::Modifiers::NONE);
        if plain_modifiers {
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                state.completion = None;
            } else if ui
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp))
            {
                if let Some(completion) = state.completion.as_mut() {
                    completion.selected = completion
                        .selected
                        .checked_sub(1)
                        .unwrap_or(completion.set.items.len().saturating_sub(1));
                }
            } else if ui
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
            {
                if let Some(completion) = state.completion.as_mut() {
                    completion.selected =
                        (completion.selected + 1) % completion.set.items.len().max(1);
                }
            } else {
                let accept_enter = ui
                    .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                let accept_tab = !accept_enter
                    && ui.input_mut(|input| {
                        input.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
                    });
                if accept_enter || accept_tab {
                    if let Some(completion) = state.completion.clone() {
                        queue_completion(state, &completion, completion.selected);
                    }
                }
            }
        }
    }

    let mut injected = inject_pending_completion(ui, state, token, editor_id);
    if injected.is_none() && queue_smart_newline(ui, state, token, editor_id, language) {
        injected = inject_pending_completion(ui, state, token, editor_id);
    }
    (explicit, injected, false)
}

/// 编辑器代码编辑操作：行操作/注释/缩进快捷键与括号自动闭合。
///
/// 需要改文本的操作统一转成 [`EditPlan`] 走 `pending_completion`
/// 单条 Paste 注入（一步可撤销）；只动光标的操作直接改写
/// `TextEditState`。在 TextEdit 处理输入之前调用。
fn prepare_editor_ops(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    token: u64,
    editor_id: egui::Id,
    language: Language,
) {
    if state.pending_close.is_some() || state.conflict_token().is_some() {
        return;
    }
    if !ui.memory(|memory| memory.has_focus(editor_id)) {
        return;
    }
    let Some(selection) = editor_char_range(ui, editor_id) else {
        return;
    };
    if let Some(plan) = shortcut_edit_plan(ui, state, token, language, &selection) {
        queue_edit_plan(state, token, plan);
        state.completion = None;
        return;
    }
    auto_close_input(ui, state, token, editor_id, &selection);
}

fn editor_char_range(ui: &egui::Ui, editor_id: egui::Id) -> Option<std::ops::Range<usize>> {
    egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
        .and_then(|state| state.cursor.char_range())
        .map(|range| range.as_sorted_char_range())
}

/// 把编辑计划排入单条注入队列（指纹取自当前文档，inject 时校验）。
fn queue_edit_plan(state: &mut TextEditorState, token: u64, plan: EditPlan) {
    let Some(document) = state.document(token) else {
        return;
    };
    state.pending_completion = Some(PendingCompletion {
        token,
        fingerprint: sha256(document.text.as_bytes()),
        replace_chars: plan.replace_chars,
        insertion: plan.insertion,
        post_selection: Some(plan.selection_after),
    });
}

/// 带修饰键的编辑快捷键：返回 Some 表示已消费按键并给出编辑计划。
fn shortcut_edit_plan(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    token: u64,
    language: Language,
    selection: &std::ops::Range<usize>,
) -> Option<EditPlan> {
    let document = state.document(token)?;
    let text = &document.text;
    let command = egui::Modifiers::COMMAND;
    let command_shift = command.plus(egui::Modifiers::SHIFT);
    let alt = egui::Modifiers::ALT;
    let alt_shift = alt.plus(egui::Modifiers::SHIFT);
    if ui.input_mut(|input| {
        input.consume_shortcut(&egui::KeyboardShortcut::new(command, egui::Key::Slash))
    }) {
        return language
            .comment_style()
            .and_then(|style| text_editor_ops::toggle_comment(text, selection, style));
    }
    if ui.input_mut(|input| {
        input.consume_shortcut(&egui::KeyboardShortcut::new(command_shift, egui::Key::K))
    }) {
        return Some(text_editor_ops::delete_lines(text, selection));
    }
    if ui.input_mut(|input| {
        input.consume_shortcut(&egui::KeyboardShortcut::new(alt, egui::Key::ArrowUp))
    }) {
        return text_editor_ops::move_lines(text, selection, true);
    }
    if ui.input_mut(|input| {
        input.consume_shortcut(&egui::KeyboardShortcut::new(alt, egui::Key::ArrowDown))
    }) {
        return text_editor_ops::move_lines(text, selection, false);
    }
    if ui.input_mut(|input| {
        input.consume_shortcut(&egui::KeyboardShortcut::new(
            alt_shift,
            egui::Key::ArrowDown,
        ))
    }) {
        return Some(text_editor_ops::duplicate(text, selection));
    }
    // 块缩进：弹窗开着时 Tab 让位给补全接受；空选区 Tab 保留 egui 默认插入。
    let popup_open = state
        .completion
        .as_ref()
        .is_some_and(|completion| completion.token == token);
    if !popup_open {
        if !selection.is_empty()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
        {
            let unit = detected_indent_unit(text, &line_indent_at(text, selection.start), language);
            return text_editor_ops::indent_lines(text, selection, &unit, false);
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)) {
            let unit = detected_indent_unit(text, &line_indent_at(text, selection.start), language);
            return text_editor_ops::indent_lines(text, selection, &unit, true);
        }
    }
    None
}

/// 括号/引号自动闭合、越过闭合符、成对退格删除。
fn auto_close_input(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    token: u64,
    editor_id: egui::Id,
    selection: &std::ops::Range<usize>,
) {
    // 成对退格删除：先判定、后消费（普通退格必须原样通过）。
    let pair_delete = state
        .document(token)
        .and_then(|document| text_editor_ops::backspace_pair_range(&document.text, selection));
    if let Some(range) = pair_delete {
        if consume_plain_key(ui, egui::Key::Backspace) {
            queue_edit_plan(
                state,
                token,
                EditPlan {
                    replace_chars: range.clone(),
                    insertion: String::new(),
                    selection_after: range.start..range.start,
                },
            );
            return;
        }
    }
    // 单字符 Text 事件：自动闭合 / 包裹 / 越过。
    let typed = ui.input(|input| {
        input
            .events
            .iter()
            .enumerate()
            .find_map(|(index, event)| match event {
                egui::Event::Text(text) if text.chars().count() == 1 => {
                    text.chars().next().map(|ch| (index, ch))
                }
                _ => None,
            })
    });
    let Some((event_index, ch)) = typed else {
        return;
    };
    let Some(document) = state.document(token) else {
        return;
    };
    match text_editor_ops::typed_char_decision(&document.text, selection, ch) {
        TypedCharDecision::Plain => {}
        TypedCharDecision::Pair { open, close } => {
            remove_event(ui, event_index);
            queue_edit_plan(
                state,
                token,
                EditPlan {
                    replace_chars: selection.clone(),
                    insertion: format!("{open}{close}"),
                    selection_after: selection.start + 1..selection.start + 1,
                },
            );
        }
        TypedCharDecision::Wrap { open, close } => {
            let inner = text_editor_ops::slice_chars(&document.text, selection.clone()).to_owned();
            remove_event(ui, event_index);
            queue_edit_plan(
                state,
                token,
                EditPlan {
                    replace_chars: selection.clone(),
                    insertion: format!("{open}{inner}{close}"),
                    selection_after: selection.start + 1..selection.end + 1,
                },
            );
        }
        TypedCharDecision::SkipCloser => {
            remove_event(ui, event_index);
            if let Some(mut edit_state) =
                egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
            {
                edit_state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(selection.start + 1),
                    )));
                edit_state.store(ui.ctx(), editor_id);
            }
        }
    }
}

/// 消费一个无修饰键的按下事件；没有该事件时不消费任何东西。
fn consume_plain_key(ui: &mut egui::Ui, key: egui::Key) -> bool {
    ui.input_mut(|input| {
        let mut consumed = false;
        input.events.retain(|event| {
            let hit = !consumed
                && matches!(
                    event,
                    egui::Event::Key {
                        key: event_key,
                        pressed: true,
                        modifiers,
                        ..
                    } if *event_key == key && *modifiers == egui::Modifiers::NONE
                );
            consumed |= hit;
            !hit
        });
        consumed
    })
}

fn remove_event(ui: &mut egui::Ui, index: usize) {
    ui.input_mut(|input| {
        if index < input.events.len() {
            input.events.remove(index);
        }
    });
}

/// 选区所在行的前导空白（缩进单位探测用）。
fn line_indent_at(text: &str, char_idx: usize) -> String {
    let line = text_editor_ops::line_index_at(text, char_idx);
    let range = text_editor_ops::line_range(text, line);
    text_editor_ops::slice_chars(text, range)
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .collect()
}

#[derive(Clone, Copy)]
struct ImeFrame {
    composing: bool,
    had_event: bool,
}

fn update_ime_state(ui: &egui::Ui, state: &mut TextEditorState) -> ImeFrame {
    let ime_events = ui.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Ime(event) => Some(event.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    });
    let had_event = !ime_events.is_empty();
    for event in ime_events {
        match event {
            egui::ImeEvent::Preedit(text) => state.ime_composing = !text.is_empty(),
            egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => {
                state.ime_composing = false;
            }
            egui::ImeEvent::Enabled => {}
        }
    }
    ImeFrame {
        composing: state.ime_composing,
        had_event,
    }
}

fn queue_completion(state: &mut TextEditorState, popup: &CompletionPopup, selected: usize) {
    let Some(item) = popup.set.items.get(selected) else {
        state.completion = None;
        return;
    };
    let post_selection = item.cursor_offset.map(|offset| {
        let cursor = popup.set.replace_chars.start + offset;
        cursor..cursor
    });
    state.pending_completion = Some(PendingCompletion {
        token: popup.token,
        fingerprint: popup.fingerprint,
        replace_chars: popup.set.replace_chars.clone(),
        insertion: item.insertion.clone(),
        post_selection,
    });
    state.completion = None;
}

fn inject_pending_completion(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    token: u64,
    editor_id: egui::Id,
) -> Option<InjectedEditorEdit> {
    let pending = state.pending_completion.take()?;
    let valid = pending.token == token
        && state.document(token).is_some_and(|document| {
            pending.fingerprint == sha256(document.text.as_bytes())
                && pending.replace_chars.end <= document.text.chars().count()
        });
    if !valid {
        return None;
    }

    let mut edit_state =
        egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id).unwrap_or_default();
    edit_state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(pending.replace_chars.start),
            egui::text::CCursor::new(pending.replace_chars.end),
        )));
    edit_state.store(ui.ctx(), editor_id);
    ui.memory_mut(|memory| memory.request_focus(editor_id));
    ui.input_mut(|input| {
        if pending.insertion.is_empty() {
            // egui 忽略空 Paste：删除统一改发 Backspace，选区内容作为
            // 一次普通删除进入撤销栈（同样一步可撤销）。
            input.events.push(egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: Some(egui::Key::Backspace),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
        } else {
            input.events.push(egui::Event::Paste(pending.insertion));
        }
    });
    Some(InjectedEditorEdit {
        post_selection: pending.post_selection,
    })
}

fn queue_smart_newline(
    ui: &mut egui::Ui,
    state: &mut TextEditorState,
    token: u64,
    editor_id: egui::Id,
    language: Language,
) -> bool {
    if !ui.memory(|memory| memory.has_focus(editor_id)) || !has_plain_enter(ui) {
        return false;
    }
    let Some(cursor_range) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
        .and_then(|edit_state| edit_state.cursor.char_range())
    else {
        return false;
    };
    let replace_chars = cursor_range.as_sorted_char_range();
    if !replace_chars.is_empty() {
        return false;
    }
    let Some((fingerprint, plan)) = state.document(token).and_then(|document| {
        smart_newline_edit(&document.text, replace_chars, language)
            .map(|plan| (sha256(document.text.as_bytes()), plan))
    }) else {
        return false;
    };
    if !consume_plain_enter(ui) {
        return false;
    }
    state.pending_completion = Some(PendingCompletion {
        token,
        fingerprint,
        replace_chars: plan.replace_chars,
        insertion: plan.insertion,
        post_selection: Some(plan.cursor_after..plan.cursor_after),
    });
    state.completion = None;
    true
}

fn has_plain_enter(ui: &egui::Ui) -> bool {
    ui.input(|input| {
        input.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::Enter,
                    pressed: true,
                    modifiers,
                    ..
                } if *modifiers == egui::Modifiers::NONE
            )
        })
    })
}

fn consume_plain_enter(ui: &mut egui::Ui) -> bool {
    ui.input_mut(|input| {
        let mut consumed = false;
        input.events.retain(|event| {
            let matches = !consumed
                && matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::Enter,
                        pressed: true,
                        modifiers,
                        ..
                    } if *modifiers == egui::Modifiers::NONE
                );
            consumed |= matches;
            !matches
        });
        consumed
    })
}

fn smart_newline_edit(
    text: &str,
    replace_chars: std::ops::Range<usize>,
    language: Language,
) -> Option<SmartNewlineEdit> {
    let char_count = text.chars().count();
    if !replace_chars.is_empty() || replace_chars.end > char_count {
        return None;
    }
    let cursor_byte = byte_index_from_char_index(text, replace_chars.start);
    let line_start_byte = text[..cursor_byte].rfind('\n').map_or(0, |index| index + 1);
    let line_start_char = text[..line_start_byte].chars().count();
    let line_before_cursor = &text[line_start_byte..cursor_byte];
    let suffix = &text[cursor_byte..];
    let base_indent: String = line_before_cursor
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .collect();
    let indent_unit = detected_indent_unit(text, &base_indent, language);
    let immediate_next = suffix.chars().next();

    // 光标位于只有缩进的闭合符前：保留当前内层空行，同时把闭合符
    // 回退一级。整次变化仍通过一条 Paste 事件进入撤销栈。
    if line_before_cursor
        .chars()
        .all(|ch| matches!(ch, ' ' | '\t'))
        && immediate_next.is_some_and(is_closer)
    {
        let outer_indent = remove_one_indent(&base_indent, &indent_unit);
        let insertion = format!("{base_indent}\n{outer_indent}");
        return Some(SmartNewlineEdit {
            replace_chars: line_start_char..replace_chars.end,
            cursor_after: line_start_char + base_indent.chars().count(),
            insertion,
        });
    }

    let code_tail = last_code_char(line_before_cursor, language);
    let opens_block = code_tail.is_some_and(|tail| {
        matches!(tail, '{' | '[' | '(')
            || (tail == ':'
                && matches!(language, Language::Python | Language::Json | Language::Yaml))
    });
    let inner_indent = if opens_block {
        format!("{base_indent}{indent_unit}")
    } else {
        base_indent.clone()
    };

    if code_tail
        .and_then(matching_closer)
        .is_some_and(|closer| immediate_next == Some(closer))
    {
        let insertion = format!("\n{inner_indent}\n{base_indent}");
        let cursor_after = replace_chars.start + 1 + inner_indent.chars().count();
        Some(SmartNewlineEdit {
            replace_chars,
            insertion,
            cursor_after,
        })
    } else {
        let insertion = format!("\n{inner_indent}");
        let cursor_after = replace_chars.start + insertion.chars().count();
        Some(SmartNewlineEdit {
            replace_chars,
            insertion,
            cursor_after,
        })
    }
}

fn byte_index_from_char_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn detected_indent_unit(text: &str, base_indent: &str, language: Language) -> String {
    if base_indent.ends_with('\t') {
        return "\t".to_owned();
    }
    let detected_spaces = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let spaces = line.chars().take_while(|ch| *ch == ' ').count();
            (spaces > 0 && spaces <= 8).then_some(spaces)
        })
        .min();
    let default_spaces = if matches!(
        language,
        Language::Json
            | Language::Yaml
            | Language::Toml
            | Language::Html
            | Language::Xml
            | Language::Css
    ) {
        2
    } else {
        4
    };
    " ".repeat(detected_spaces.unwrap_or(default_spaces))
}

fn remove_one_indent(indent: &str, unit: &str) -> String {
    if let Some(outer) = indent.strip_suffix(unit) {
        return outer.to_owned();
    }
    if let Some(outer) = indent.strip_suffix('\t') {
        return outer.to_owned();
    }
    let trailing_spaces = indent.chars().rev().take_while(|ch| *ch == ' ').count();
    let remove = trailing_spaces.min(unit.chars().count().max(1));
    indent
        .chars()
        .take(indent.chars().count().saturating_sub(remove))
        .collect()
}

fn matching_closer(opener: char) -> Option<char> {
    match opener {
        '{' => Some('}'),
        '[' => Some(']'),
        '(' => Some(')'),
        _ => None,
    }
}

fn is_closer(ch: char) -> bool {
    matches!(ch, '}' | ']' | ')')
}

fn last_code_char(line: &str, language: Language) -> Option<char> {
    let line_comment = match language {
        Language::Python
        | Language::Shell
        | Language::PowerShell
        | Language::Yaml
        | Language::Toml
        | Language::Dockerfile => Some("#"),
        Language::Sql => Some("--"),
        Language::Rust
        | Language::JavaScript
        | Language::TypeScript
        | Language::Go
        | Language::Java
        | Language::CLike
        | Language::Json => Some("//"),
        _ => None,
    };
    let mut quote = None;
    let mut escaped = false;
    let mut last = None;
    for (byte, ch) in line.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        if line_comment.is_some_and(|marker| line[byte..].starts_with(marker)) {
            break;
        }
        if matches!(ch, '"' | '\'' | '`') {
            quote = Some(ch);
        } else if !ch.is_whitespace() {
            last = Some(ch);
        }
    }
    last
}

fn completion_popup(
    ctx: &egui::Context,
    popup: &CompletionPopup,
    pal: &Palette,
) -> CompletionPopupOutput {
    let width = 320.0;
    let height = (34.0 + popup.set.items.len() as f32 * 25.0).min(270.0);
    let content = ctx.content_rect();
    let mut position = egui::pos2(popup.caret_rect.left(), popup.caret_rect.bottom() + 3.0);
    if position.y + height > content.bottom() {
        position.y = popup.caret_rect.top() - height - 3.0;
    }
    position.x = position.x.clamp(
        content.left(),
        (content.right() - width).max(content.left()),
    );
    position.y = position.y.clamp(
        content.top(),
        (content.bottom() - height).max(content.top()),
    );

    let mut accepted = None;
    let mut hovered = None;
    let area = egui::Area::new(egui::Id::new(("lumen_text_editor_completion", popup.token)))
        .order(egui::Order::Foreground)
        .fixed_pos(position)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(pal.bg_panel)
                .stroke(egui::Stroke::new(1.0_f32, pal.panel_outline))
                .corner_radius(egui::CornerRadius::same(5))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 5],
                    blur: 14,
                    spread: 0,
                    color: egui::Color32::from_black_alpha(100),
                })
                .inner_margin(egui::Margin::same(4))
                .show(ui, |ui| {
                    ui.set_width(width - 8.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(
                                crate::i18n::strings().text_editor_completion_title,
                            )
                            .strong()
                            .size(10.0)
                            .color(pal.fg),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(
                                    crate::i18n::strings().text_editor_completion_keys,
                                )
                                .size(9.0)
                                .color(pal.fg_dim),
                            );
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt(("lumen_text_editor_completion_list", popup.token))
                        .max_height(height - 34.0)
                        .show(ui, |ui| {
                            for (index, item) in popup.set.items.iter().enumerate() {
                                let (rect, response) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 24.0),
                                    egui::Sense::click(),
                                );
                                if index == popup.selected || response.hovered() {
                                    ui.painter().rect_filled(
                                        rect,
                                        egui::CornerRadius::same(3),
                                        pal.selection,
                                    );
                                }
                                if index == popup.selected {
                                    ui.painter().rect_filled(
                                        egui::Rect::from_min_max(
                                            rect.min,
                                            egui::pos2(rect.left() + 3.0, rect.bottom()),
                                        ),
                                        egui::CornerRadius::same(2),
                                        pal.accent,
                                    );
                                }
                                if response.hovered() {
                                    hovered = Some(index);
                                }
                                if response.clicked() {
                                    accepted = Some(index);
                                }
                                let kind = match item.kind {
                                    CompletionKind::Snippet => {
                                        crate::i18n::strings().text_editor_completion_snippet
                                    }
                                    CompletionKind::Keyword => {
                                        crate::i18n::strings().text_editor_completion_keyword
                                    }
                                    CompletionKind::Builtin => {
                                        crate::i18n::strings().text_editor_completion_builtin
                                    }
                                    CompletionKind::Document => {
                                        crate::i18n::strings().text_editor_completion_document
                                    }
                                };
                                ui.painter().text(
                                    egui::pos2(rect.left() + 6.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    &item.label,
                                    egui::FontId::monospace(12.0),
                                    pal.fg,
                                );
                                ui.painter().text(
                                    egui::pos2(rect.right() - 6.0, rect.center().y),
                                    egui::Align2::RIGHT_CENTER,
                                    kind,
                                    egui::FontId::proportional(10.0),
                                    pal.fg_dim,
                                );
                            }
                        });
                });
        });
    CompletionPopupOutput {
        accepted,
        hovered,
        rect: area.response.rect,
        pointer_clicked: ctx.input(|input| input.pointer.any_click()),
        pointer_position: ctx.input(|input| input.pointer.interact_pos()),
    }
}

fn cursor_line_column(text: &str, cursor_char: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for ch in text.chars().take(cursor_char) {
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn decision_modal(
    ctx: &egui::Context,
    state: &mut TextEditorState,
    pal: &Palette,
    output: &mut Output,
) {
    #[derive(Clone, Copy)]
    enum Dialog {
        Conflict(u64),
        Close(CloseIntent),
    }
    #[derive(Clone, Copy)]
    enum DialogAction {
        KeepEditing,
        Reload,
        Overwrite,
        Save,
        DontSave,
        Cancel,
    }

    let dialog = state
        .conflict_token()
        .map(Dialog::Conflict)
        .or_else(|| state.pending_close.map(Dialog::Close));
    let Some(dialog) = dialog else {
        return;
    };
    let token = match dialog {
        Dialog::Conflict(token) => token,
        Dialog::Close(intent) => intent.token,
    };
    let Some((name, path, can_save)) = state.document(token).map(|document| {
        (
            document.source.display_name().to_owned(),
            document.source.path().to_owned(),
            document.state == LoadState::Ready
                && document.source_valid
                && document.pending_save.is_none()
                && document.dirty(),
        )
    }) else {
        state.cancel_close_flow();
        return;
    };
    let mut action = None;
    egui::Modal::new(egui::Id::new(("lumen_text_editor_decision", token)))
        .backdrop_color(egui::Color32::from_black_alpha(140))
        .frame(
            egui::Frame::new()
                .fill(pal.bg_panel)
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::same(16)),
        )
        .show(ctx, |ui| {
            ui.set_min_width(390.0);
            match dialog {
                Dialog::Conflict(_) => {
                    ui.heading(crate::i18n::strings().text_editor_remote_changed_title);
                    ui.add_space(6.0);
                    ui.label(crate::i18n::strings().text_editor_remote_changed_body);
                    ui.label(
                        egui::RichText::new(&path)
                            .monospace()
                            .small()
                            .color(pal.fg_dim),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(crate::i18n::strings().text_editor_keep_editing)
                            .clicked()
                        {
                            action = Some(DialogAction::KeepEditing);
                        }
                        if ui
                            .button(crate::i18n::strings().text_editor_reload)
                            .clicked()
                        {
                            action = Some(DialogAction::Reload);
                        }
                        if ui
                            .button(
                                egui::RichText::new(crate::i18n::strings().text_editor_overwrite)
                                    .color(pal.error),
                            )
                            .clicked()
                        {
                            action = Some(DialogAction::Overwrite);
                        }
                    });
                }
                Dialog::Close(_) => {
                    ui.heading(crate::i18n::strings().text_editor_unsaved_title);
                    ui.add_space(6.0);
                    ui.label(crate::i18n::strings().text_editor_unsaved_body);
                    ui.label(egui::RichText::new(&name).strong().color(pal.fg));
                    ui.label(
                        egui::RichText::new(&path)
                            .monospace()
                            .small()
                            .color(pal.fg_dim),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                can_save,
                                egui::Button::new(crate::i18n::strings().ssh_save),
                            )
                            .clicked()
                        {
                            action = Some(DialogAction::Save);
                        }
                        if ui
                            .button(
                                egui::RichText::new(crate::i18n::strings().text_editor_dont_save)
                                    .color(pal.error),
                            )
                            .on_hover_text(crate::i18n::strings().text_editor_discard)
                            .clicked()
                        {
                            action = Some(DialogAction::DontSave);
                        }
                        if ui.button(crate::i18n::strings().ssh_cancel).clicked() {
                            action = Some(DialogAction::Cancel);
                        }
                    });
                }
            }
        });

    match (dialog, action) {
        (Dialog::Conflict(token), Some(DialogAction::KeepEditing)) => {
            if let Some(document) = state.document_mut(token) {
                document.pending_save = None;
                document.save_conflict = false;
            }
            state.cancel_close_flow();
        }
        (Dialog::Conflict(token), Some(DialogAction::Reload)) => {
            let request = state.document_mut(token).map(|document| {
                document.pending_save = None;
                document.save_conflict = false;
                document.state = LoadState::Loading;
                document.error = None;
                LoadRequest {
                    token,
                    source: document.source.clone(),
                }
            });
            state.cancel_close_flow();
            output.load = request;
        }
        (Dialog::Conflict(token), Some(DialogAction::Overwrite)) => {
            if let Some(document) = state.document_mut(token) {
                document.pending_save = None;
                document.save_conflict = false;
            }
            output.save = state.build_save_request_for(token, true);
        }
        (Dialog::Close(intent), Some(DialogAction::Save)) => {
            if let Some(save) = state.build_save_request_for(intent.token, false) {
                state.pending_close = None;
                state.post_save_close = Some(intent);
                output.save = Some(save);
            }
        }
        (Dialog::Close(intent), Some(DialogAction::DontSave)) => {
            state.pending_close = None;
            state.post_save_close = None;
            match intent.scope {
                CloseScope::Tab => {
                    state.remove_document(intent.token);
                    output.closed = !state.is_open();
                }
                CloseScope::Editor => {
                    if let Some(fingerprint) = state
                        .document(intent.token)
                        .map(|document| sha256(document.text.as_bytes()))
                    {
                        state
                            .close_editor_discarded
                            .insert(intent.token, fingerprint);
                    }
                    state.advance_close_editor(output);
                }
            }
        }
        (Dialog::Close(_), Some(DialogAction::Cancel)) => state.cancel_close_flow(),
        (_, None) => {}
        _ => {}
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

    fn remote(generation: u64, path: &str) -> TextFileSource {
        TextFileSource::Remote {
            generation,
            path: path.to_owned(),
        }
    }

    fn ssh(runtime_id: u64, session_id: u64, path: &str) -> TextFileSource {
        TextFileSource::Ssh {
            runtime_id,
            session_id,
            path: path.to_owned(),
        }
    }

    fn open_loaded(state: &mut TextEditorState, source: TextFileSource, text: &str) -> u64 {
        let request = state.request_open(source).expect("new load request");
        assert!(state.apply_loaded(request.token, Ok(text.as_bytes().to_vec())));
        request.token
    }

    fn edit(state: &mut TextEditorState, token: u64, text: &str) {
        state.document_mut(token).expect("document").text = text.to_owned();
    }

    #[test]
    fn stale_load_and_save_results_are_ignored() {
        let mut state = TextEditorState::default();
        let first = state.request_open(remote(1, "/tmp/a")).expect("load");
        state.close_without_prompt();
        assert!(!state.apply_loaded(first.token, Ok(b"old".to_vec())));

        let second = state.request_open(remote(1, "/tmp/b")).expect("load");
        assert!(state.apply_loaded(second.token, Ok(b"new".to_vec())));
        assert!(!state.apply_saved(first.token, Ok(())));
    }

    #[test]
    fn edit_during_save_remains_dirty_after_success() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, ssh(1, 7, "/tmp/a.txt"), "before");
        edit(&mut state, token, "saved");
        let save = state.build_save_request(false).expect("save");
        assert!(state.mark_saving(&save));
        edit(&mut state, token, "typed later");
        assert!(state.apply_saved(save.token, Ok(())));
        assert!(state.is_dirty());
        assert_eq!(state.document(token).expect("doc").saved_text, "saved");
        assert_eq!(state.document(token).expect("doc").text, "typed later");
    }

    #[test]
    fn opens_multiple_documents_and_reuses_existing_tab() {
        let mut state = TextEditorState::default();
        let first_source = remote(2, "/tmp/first.txt");
        let first = open_loaded(&mut state, first_source.clone(), "one");
        edit(&mut state, first, "one changed");
        let second = open_loaded(&mut state, remote(2, "/tmp/second.txt"), "two");

        assert_eq!(state.documents.len(), 2);
        assert_eq!(state.active_token, Some(second));
        assert!(state.request_open(first_source).is_none());
        assert_eq!(state.active_token, Some(first));
        assert_eq!(state.documents.len(), 2);
        assert_eq!(state.document(first).expect("first").text, "one changed");
    }

    #[test]
    fn hide_and_restore_preserve_tabs_dirty_buffers_and_pending_save() {
        let mut state = TextEditorState::default();
        let first = open_loaded(&mut state, remote(2, "/tmp/first.txt"), "one");
        edit(&mut state, first, "one changed");
        let second = open_loaded(&mut state, ssh(3, 4, "/tmp/second.txt"), "two");
        edit(&mut state, second, "two saving");
        let save = state.build_save_request(false).expect("save request");
        assert!(state.mark_saving(&save));
        state.find_open = true;
        state.find_query = "needle".to_owned();

        assert!(state.is_open());
        assert!(state.is_visible());
        assert!(state.hide());
        assert!(state.is_open());
        assert!(!state.is_visible());
        assert!(state.is_dirty());
        assert_eq!(state.documents.len(), 2);
        assert_eq!(state.active_token, Some(second));
        assert_eq!(state.document(first).expect("first").text, "one changed");
        assert_eq!(state.document(second).expect("second").text, "two saving");
        assert!(state
            .document(second)
            .expect("second")
            .pending_save
            .is_some());
        assert!(state.find_open);
        assert_eq!(state.find_query, "needle");
        assert!(!state.hide(), "重复隐藏不应报告状态变化");

        assert!(state.restore());
        assert!(state.is_visible());
        assert!(state.focus_editor);
        assert!(!state.restore(), "重复恢复不应报告状态变化");
        assert_eq!(state.documents.len(), 2);
        assert!(state.document(first).expect("first").dirty());
        assert!(state
            .document(second)
            .expect("second")
            .pending_save
            .is_some());
    }

    #[test]
    fn opening_a_file_restores_a_hidden_editor_without_losing_existing_tabs() {
        let mut state = TextEditorState::default();
        let first_source = remote(5, "/tmp/first.txt");
        let first = open_loaded(&mut state, first_source.clone(), "one");
        edit(&mut state, first, "one changed");
        assert!(state.hide());

        assert!(state.request_open(first_source).is_none());
        assert!(state.is_visible());
        assert_eq!(state.active_token, Some(first));
        assert_eq!(state.documents.len(), 1);
        assert_eq!(state.document(first).expect("first").text, "one changed");

        assert!(state.hide());
        let second = state
            .request_open(remote(5, "/tmp/second.txt"))
            .expect("new load request");
        assert!(state.is_visible());
        assert_eq!(state.active_token, Some(second.token));
        assert_eq!(state.documents.len(), 2);
        assert_eq!(state.document(first).expect("first").text, "one changed");
    }

    #[test]
    fn async_load_and_save_results_apply_without_restoring_hidden_editor() {
        let mut state = TextEditorState::default();
        let load = state
            .request_open(ssh(8, 9, "/tmp/async.txt"))
            .expect("load request");
        assert!(state.hide());
        assert!(state.apply_loaded(load.token, Ok(b"before".to_vec())));
        assert!(!state.is_visible());
        assert_eq!(state.document(load.token).expect("document").text, "before");

        assert!(state.restore());
        edit(&mut state, load.token, "after");
        let save = state.build_save_request(false).expect("save request");
        assert!(state.mark_saving(&save));
        assert!(state.hide());
        assert!(state.apply_saved(save.token, Ok(())));

        assert!(!state.is_visible());
        assert!(state.is_open());
        assert!(!state.is_dirty());
        assert_eq!(state.document(load.token).expect("document").text, "after");
        assert_eq!(
            state.document(load.token).expect("document").saved_text,
            "after"
        );
    }

    #[test]
    fn closing_the_last_tab_also_clears_editor_visibility() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, ssh(6, 7, "/tmp/only.txt"), "clean");
        let mut output = Output::default();

        state.request_close_tab(token, &mut output);

        assert!(output.closed);
        assert!(!state.is_open());
        assert!(!state.is_visible());
        assert!(!state.restore());
    }

    #[test]
    fn background_load_result_does_not_change_active_tab() {
        let mut state = TextEditorState::default();
        let first = state
            .request_open(remote(3, "/tmp/first.txt"))
            .expect("first load");
        let second = state
            .request_open(remote(3, "/tmp/second.txt"))
            .expect("second load");
        assert_eq!(state.active_token, Some(second.token));

        assert!(state.apply_loaded(first.token, Ok(b"background".to_vec())));
        assert_eq!(state.active_token, Some(second.token));
        assert_eq!(
            state.document(first.token).expect("first").text,
            "background"
        );
    }

    #[test]
    fn dirty_state_is_independent_per_tab() {
        let mut state = TextEditorState::default();
        let first = open_loaded(&mut state, remote(4, "/tmp/first.txt"), "one");
        let second = open_loaded(&mut state, remote(4, "/tmp/second.txt"), "two");
        edit(&mut state, first, "changed");

        assert!(state.document(first).expect("first").dirty());
        assert!(!state.document(second).expect("second").dirty());
        assert!(state.is_dirty());
    }

    #[test]
    fn same_path_in_a_new_remote_generation_is_a_different_document() {
        let mut state = TextEditorState::default();
        let first = state
            .request_open(remote(10, "/etc/app.conf"))
            .expect("first load");
        assert!(state.apply_loaded(first.token, Ok(b"old peer".to_vec())));

        let second = state
            .request_open(remote(11, "/etc/app.conf"))
            .expect("new generation must reload");
        assert_ne!(first.token, second.token);
        assert_eq!(second.source, remote(11, "/etc/app.conf"));
        assert_eq!(state.documents.len(), 2);
    }

    #[test]
    fn invalidation_finishes_loading_and_saving_without_losing_dirty_buffer() {
        let mut loading = TextEditorState::default();
        let source = ssh(4, 7, "/tmp/a.txt");
        let load = loading
            .request_open(source.clone())
            .expect("loading request");
        assert!(loading.invalidate_source(&source, "session changed"));
        let loading_document = loading.document(load.token).expect("document");
        assert_eq!(loading_document.state, LoadState::Error);
        assert!(!loading_document.source_valid);
        assert!(!loading.apply_loaded(load.token, Ok(b"stale".to_vec())));

        let mut saving = TextEditorState::default();
        let load = saving.request_open(source.clone()).expect("load");
        assert!(saving.apply_loaded(load.token, Ok(b"before".to_vec())));
        edit(&mut saving, load.token, "after");
        let save = saving.build_save_request(false).expect("save");
        assert!(saving.mark_saving(&save));
        assert!(saving.invalidate_source(&source, "session changed"));
        let saving_document = saving.document(load.token).expect("document");
        assert_eq!(saving_document.text, "after");
        assert!(saving_document.dirty());
        assert!(saving_document.pending_save.is_none());
        assert!(!saving_document.source_valid);
        assert!(saving.build_save_request(false).is_none());
    }

    #[test]
    fn save_before_close_waits_for_success_then_closes() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(2, "/tmp/a.txt"), "before");
        edit(&mut state, token, "after");
        let mut output = Output::default();
        state.request_close_tab(token, &mut output);
        let intent = state.pending_close.expect("close decision");
        let save = state.build_save_request(false).expect("save");
        state.pending_close = None;
        state.post_save_close = Some(intent);
        assert!(state.mark_saving(&save));

        finish_deferred_action(&mut state, &mut output);
        assert!(!output.closed);
        assert!(state.is_open());

        assert!(state.apply_saved(save.token, Ok(())));
        finish_deferred_action(&mut state, &mut output);
        assert!(output.closed);
        assert!(!state.is_open());
    }

    #[test]
    fn closing_one_clean_tab_keeps_the_other_document() {
        let mut state = TextEditorState::default();
        let first = open_loaded(&mut state, ssh(8, 9, "/tmp/a.txt"), "one");
        let second = open_loaded(&mut state, ssh(8, 9, "/tmp/b.txt"), "two");
        let mut output = Output::default();

        state.request_close_tab(second, &mut output);
        assert!(!output.closed);
        assert_eq!(state.documents.len(), 1);
        assert_eq!(state.active_token, Some(first));
        assert_eq!(state.document(first).expect("first").text, "one");
    }

    #[test]
    fn edits_during_close_save_return_to_the_close_prompt() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, ssh(8, 9, "/tmp/a.txt"), "before");
        edit(&mut state, token, "saved");
        let mut output = Output::default();
        state.request_close_tab(token, &mut output);
        let intent = state.pending_close.take().expect("close intent");
        let save = state.build_save_request(false).expect("save");
        state.post_save_close = Some(intent);
        assert!(state.mark_saving(&save));
        edit(&mut state, token, "typed later");

        assert!(state.apply_saved(save.token, Ok(())));
        finish_deferred_action(&mut state, &mut output);
        assert_eq!(state.pending_close, Some(intent));
        assert!(state.post_save_close.is_none());
        assert!(state.is_dirty());
        assert_eq!(state.document(token).expect("document").text, "typed later");
    }

    #[test]
    fn canceling_full_editor_close_preserves_all_dirty_buffers() {
        let mut state = TextEditorState::default();
        let first = open_loaded(&mut state, remote(7, "/tmp/a.txt"), "one");
        let second = open_loaded(&mut state, remote(7, "/tmp/b.txt"), "two");
        edit(&mut state, first, "one changed");
        edit(&mut state, second, "two changed");
        let mut output = Output::default();

        state.request_close_editor(&mut output);
        let first_intent = state.pending_close.take().expect("first decision");
        assert_eq!(first_intent.scope, CloseScope::Editor);
        let fingerprint = sha256(
            state
                .document(first_intent.token)
                .expect("first document")
                .text
                .as_bytes(),
        );
        state
            .close_editor_discarded
            .insert(first_intent.token, fingerprint);
        state.advance_close_editor(&mut output);
        assert!(state.pending_close.is_some());

        state.cancel_close_flow();
        assert_eq!(state.documents.len(), 2);
        assert_eq!(state.document(first).expect("first").text, "one changed");
        assert_eq!(state.document(second).expect("second").text, "two changed");
        assert!(state.document(first).expect("first").dirty());
        assert!(state.document(second).expect("second").dirty());
    }

    #[test]
    fn editing_a_discarded_tab_during_close_save_prompts_again() {
        let mut state = TextEditorState::default();
        let first = open_loaded(&mut state, remote(8, "/tmp/a.txt"), "one");
        let second = open_loaded(&mut state, remote(8, "/tmp/b.txt"), "two");
        edit(&mut state, first, "one changed");
        edit(&mut state, second, "two changed");
        let mut output = Output::default();

        state.request_close_editor(&mut output);
        let first_intent = state.pending_close.take().expect("first decision");
        let fingerprint = sha256(
            state
                .document(first_intent.token)
                .expect("first document")
                .text
                .as_bytes(),
        );
        state
            .close_editor_discarded
            .insert(first_intent.token, fingerprint);
        state.advance_close_editor(&mut output);

        let second_intent = state.pending_close.take().expect("second decision");
        assert_eq!(second_intent.token, second);
        let save = state
            .build_save_request_for(second, false)
            .expect("save request");
        state.post_save_close = Some(second_intent);
        assert!(state.mark_saving(&save));

        edit(&mut state, first, "one changed again");
        assert!(state.apply_saved(second, Ok(())));
        finish_deferred_action(&mut state, &mut output);

        assert_eq!(
            state.pending_close,
            Some(CloseIntent {
                scope: CloseScope::Editor,
                token: first,
            }),
            "已放弃标签后来又被编辑时必须重新询问"
        );
        assert_eq!(state.documents.len(), 2);
        assert_eq!(
            state.document(first).expect("first").text,
            "one changed again"
        );
    }

    #[test]
    fn saving_background_tab_routes_by_token() {
        let mut state = TextEditorState::default();
        let first = open_loaded(&mut state, remote(8, "/tmp/a.txt"), "one");
        let second = open_loaded(&mut state, remote(8, "/tmp/b.txt"), "two");
        edit(&mut state, first, "one saved");
        let save = state.build_save_request_for(first, false).expect("save");
        assert!(state.mark_saving(&save));
        assert_eq!(state.active_token, Some(second));

        assert!(state.apply_saved(first, Ok(())));
        assert_eq!(state.active_token, Some(second));
        assert!(!state.document(first).expect("first").dirty());
        assert_eq!(
            state.source_for_token(first),
            Some(&remote(8, "/tmp/a.txt"))
        );
    }

    fn egui_input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 500.0),
            )),
            events,
            ..Default::default()
        }
    }

    #[test]
    fn accepted_completion_is_one_undoable_text_edit() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(9, "/tmp/main.py"), "pri");
        state.pending_completion = Some(PendingCompletion {
            token,
            fingerprint: sha256(b"pri"),
            replace_chars: 0..3,
            insertion: "print".to_owned(),
            post_selection: None,
        });
        let ctx = egui::Context::default();
        let editor_id = egui::Id::new("completion_undo_editor");

        let _ = ctx.run_ui(egui_input(Vec::new()), |ui| {
            assert!(inject_pending_completion(ui, &mut state, token, editor_id).is_some());
            let document = state.document_mut(token).expect("document");
            let _ = egui::TextEdit::singleline(&mut document.text)
                .id(editor_id)
                .show(ui);
        });
        assert_eq!(state.document(token).expect("document").text, "print");

        let modifiers = egui::Modifiers::COMMAND;
        let undo = egui::Event::Key {
            key: egui::Key::Z,
            physical_key: Some(egui::Key::Z),
            pressed: true,
            repeat: false,
            modifiers,
        };
        let mut input = egui_input(vec![undo]);
        input.modifiers = modifiers;
        let _ = ctx.run_ui(input, |ui| {
            let document = state.document_mut(token).expect("document");
            let _ = egui::TextEdit::singleline(&mut document.text)
                .id(editor_id)
                .show(ui);
        });
        assert_eq!(
            state.document(token).expect("document").text,
            "pri",
            "补全接受必须能被一次 Ctrl+Z 完整撤销"
        );
    }

    #[test]
    fn snippet_completion_places_cursor_at_placeholder() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(9, "/tmp/main.py"), "pri");
        let popup = CompletionPopup {
            token,
            fingerprint: sha256(b"pri"),
            set: CompletionSet {
                replace_chars: 0..3,
                items: vec![crate::shell::text_editor_language::CompletionItem {
                    label: "print(…)".to_owned(),
                    filter_text: "print".to_owned(),
                    insertion: "print()".to_owned(),
                    cursor_offset: Some(6),
                    kind: CompletionKind::Snippet,
                }],
            },
            selected: 0,
            caret_rect: egui::Rect::NOTHING,
        };
        queue_completion(&mut state, &popup, 0);
        let ctx = egui::Context::default();
        let editor_id = egui::Id::new("snippet_cursor_editor");
        let _ = ctx.run_ui(egui_input(Vec::new()), |ui| {
            let injected = inject_pending_completion(ui, &mut state, token, editor_id)
                .expect("snippet injection");
            let document = state.document_mut(token).expect("document");
            let mut output = egui::TextEdit::singleline(&mut document.text)
                .id(editor_id)
                .show(ui);
            let post = injected.post_selection.expect("snippet cursor");
            let cursor = egui::text::CCursorRange::two(
                egui::text::CCursor::new(post.start),
                egui::text::CCursor::new(post.end),
            );
            output.state.cursor.set_char_range(Some(cursor));
            output.state.store(ui.ctx(), editor_id);
        });
        assert_eq!(state.document(token).expect("document").text, "print()");
        let cursor = egui::widgets::text_edit::TextEditState::load(&ctx, editor_id)
            .and_then(|edit_state| edit_state.cursor.char_range())
            .and_then(|range| range.single())
            .expect("snippet cursor");
        assert_eq!(cursor.index, 6);
    }

    #[test]
    fn ime_preedit_does_not_consume_completion_keys() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(9, "/tmp/main.py"), "pri");
        state.completion = Some(CompletionPopup {
            token,
            fingerprint: sha256(b"pri"),
            set: CompletionSet {
                replace_chars: 0..3,
                items: vec![crate::shell::text_editor_language::CompletionItem {
                    label: "print".to_owned(),
                    filter_text: "print".to_owned(),
                    insertion: "print".to_owned(),
                    cursor_offset: None,
                    kind: CompletionKind::Builtin,
                }],
            },
            selected: 0,
            caret_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1.0, 14.0)),
        });
        let events = vec![
            egui::Event::Ime(egui::ImeEvent::Preedit("中".to_owned())),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let ctx = egui::Context::default();
        let editor_id = egui::Id::new("completion_ime_editor");
        let mut enter_remains = false;
        let _ = ctx.run_ui(egui_input(events), |ui| {
            let (_, accepted, suppressed) =
                prepare_completion_input(ui, &mut state, token, editor_id, Language::Python);
            assert!(accepted.is_none());
            assert!(suppressed);
            enter_remains = ui.input(|input| input.key_pressed(egui::Key::Enter));
        });
        assert!(state.ime_composing);
        assert!(state.completion.is_none());
        assert!(
            enter_remains,
            "IME 组合期间 Enter 应交给输入法/TextEdit，而不是补全弹层"
        );
    }

    #[test]
    fn ime_commit_frame_does_not_consume_enter_for_auto_indent() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(9, "/tmp/main.py"), "if ready:");
        let events = vec![
            egui::Event::Ime(egui::ImeEvent::Commit("中".to_owned())),
            egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let ctx = egui::Context::default();
        let editor_id = egui::Id::new("indent_ime_commit_editor");
        let mut enter_remains = false;
        let _ = ctx.run_ui(egui_input(events), |ui| {
            ui.memory_mut(|memory| memory.request_focus(editor_id));
            let (_, injected, suppressed) =
                prepare_completion_input(ui, &mut state, token, editor_id, Language::Python);
            assert!(injected.is_none());
            assert!(suppressed);
            enter_remains = ui.input(|input| input.key_pressed(egui::Key::Enter));
        });
        assert!(enter_remains);
    }

    #[test]
    fn smart_newline_inherits_indent_and_expands_matching_pair() {
        let inherited = smart_newline_edit("    value", 9..9, Language::Python).expect("plan");
        assert_eq!(inherited.insertion, "\n    ");
        assert_eq!(inherited.cursor_after, 14);

        let pair = smart_newline_edit("if ready {\n    {}", 16..16, Language::Rust).expect("plan");
        assert_eq!(pair.insertion, "\n        \n    ");
        assert_eq!(pair.cursor_after, 25);

        let tabs = smart_newline_edit("\tif ready:", 10..10, Language::Python).expect("plan");
        assert_eq!(tabs.insertion, "\n\t\t");
    }

    #[test]
    fn smart_newline_dedents_closer_and_ignores_comment_or_string_openers() {
        let closer =
            smart_newline_edit("fn main() {\n    }", 16..16, Language::Rust).expect("plan");
        assert_eq!(closer.replace_chars, 12..16);
        assert_eq!(closer.insertion, "    \n");
        assert_eq!(closer.cursor_after, 16);

        let comment = smart_newline_edit("// {", 4..4, Language::Rust).expect("plan");
        assert_eq!(comment.insertion, "\n");
        let string = smart_newline_edit("print(\"{\")", 10..10, Language::Python).expect("plan");
        assert_eq!(string.insertion, "\n");
    }

    #[test]
    fn smart_newline_is_one_undoable_text_edit_and_places_cursor() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(10, "/tmp/main.py"), "if ready:");
        let ctx = egui::Context::default();
        let editor_id = egui::Id::new("smart_newline_undo_editor");
        let original_len = "if ready:".chars().count();

        let _ = ctx.run_ui(egui_input(Vec::new()), |ui| {
            let document = state.document_mut(token).expect("document");
            let mut output = egui::TextEdit::multiline(&mut document.text)
                .id(editor_id)
                .code_editor()
                .show(ui);
            let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(original_len));
            output.state.cursor.set_char_range(Some(cursor));
            output.state.store(ui.ctx(), editor_id);
            ui.memory_mut(|memory| memory.request_focus(editor_id));
        });

        let enter = egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: Some(egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let _ = ctx.run_ui(egui_input(vec![enter]), |ui| {
            let (_, injected, suppressed) =
                prepare_completion_input(ui, &mut state, token, editor_id, Language::Python);
            assert!(!suppressed);
            let injected = injected.expect("smart newline injection");
            let document = state.document_mut(token).expect("document");
            let mut output = egui::TextEdit::multiline(&mut document.text)
                .id(editor_id)
                .code_editor()
                .show(ui);
            if let Some(post_selection) = injected.post_selection {
                let cursor = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(post_selection.start),
                    egui::text::CCursor::new(post_selection.end),
                );
                output.state.cursor.set_char_range(Some(cursor));
                output.state.store(ui.ctx(), editor_id);
            }
        });
        assert_eq!(
            state.document(token).expect("document").text,
            "if ready:\n    "
        );
        let cursor = egui::widgets::text_edit::TextEditState::load(&ctx, editor_id)
            .and_then(|edit_state| edit_state.cursor.char_range())
            .and_then(|range| range.single())
            .expect("post cursor");
        assert_eq!(cursor.index, 14);

        let modifiers = egui::Modifiers::COMMAND;
        let undo = egui::Event::Key {
            key: egui::Key::Z,
            physical_key: Some(egui::Key::Z),
            pressed: true,
            repeat: false,
            modifiers,
        };
        let mut input = egui_input(vec![undo]);
        input.modifiers = modifiers;
        let _ = ctx.run_ui(input, |ui| {
            let document = state.document_mut(token).expect("document");
            let _ = egui::TextEdit::multiline(&mut document.text)
                .id(editor_id)
                .code_editor()
                .show(ui);
        });
        assert_eq!(
            state.document(token).expect("document").text,
            "if ready:",
            "自动缩进必须能被一次 Ctrl+Z 完整撤销"
        );
    }

    #[test]
    fn editor_viewport_height_does_not_follow_document_length() {
        fn rendered_extent(text: &str, with_error: bool) -> (f32, f32) {
            let mut state = TextEditorState::default();
            let token = open_loaded(&mut state, remote(11, "/tmp/layout.rs"), text);
            if with_error {
                state.document_mut(token).expect("document").error = Some("save failed".to_owned());
            }
            let ctx = egui::Context::default();
            let mut consumed_height = 0.0;
            let mut used_width = 0.0;
            let _ = ctx.run_ui(egui_input(Vec::new()), |ui| {
                let start_y = ui.cursor().min.y;
                editor_body(ui, &mut state, &crate::shell::theme::DARK);
                consumed_height = ui.cursor().min.y - start_y;
                used_width = ui.min_rect().width();
            });
            (consumed_height, used_width)
        }

        let short = rendered_extent("", false);
        let many_lines = (0..400)
            .map(|index| format!("let value_{index} = {index};"))
            .collect::<Vec<_>>()
            .join("\n");
        let long = rendered_extent(&many_lines, false);
        let wide = rendered_extent(&"x".repeat(20_000), false);
        let error = rendered_extent("dirty", true);

        assert!(
            (short.0 - long.0).abs() < 1.0 && (short.0 - wide.0).abs() < 1.0,
            "编辑器外框高度必须由内容区决定：short={short:?}, long={long:?}, wide={wide:?}"
        );
        assert!(
            (short.1 - wide.1).abs() < 1.0,
            "超长单行不得把编辑区撑宽：short={short:?}, wide={wide:?}"
        );
        assert!(short.0 <= 500.0, "编辑器不得把根内容区撑高：{short:?}");
        assert!(error.0 <= 500.0, "错误提示也不得把内容区撑高：{error:?}");
    }

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// 建文档 + 首帧建立 TextEditState/焦点/光标，返回 (ctx, editor_id)。
    fn focused_editor(
        state: &mut TextEditorState,
        token: u64,
        cursor: usize,
        id: &str,
    ) -> (egui::Context, egui::Id) {
        let ctx = egui::Context::default();
        let editor_id = egui::Id::new(id);
        let _ = ctx.run_ui(egui_input(Vec::new()), |ui| {
            let document = state.document_mut(token).expect("document");
            let mut output = egui::TextEdit::multiline(&mut document.text)
                .id(editor_id)
                .code_editor()
                .show(ui);
            output
                .state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
            output.state.store(ui.ctx(), editor_id);
            ui.memory_mut(|memory| memory.request_focus(editor_id));
        });
        (ctx, editor_id)
    }

    /// 跑一帧输入事件 + prepare_completion_input + TextEdit，应用注入后选区。
    fn drive_ops_frame(
        ctx: &egui::Context,
        state: &mut TextEditorState,
        token: u64,
        editor_id: egui::Id,
        language: Language,
        input: egui::RawInput,
    ) -> Option<InjectedEditorEdit> {
        let mut injected_out = None;
        let _ = ctx.run_ui(input, |ui| {
            let (_, injected, _) = prepare_completion_input(ui, state, token, editor_id, language);
            let document = state.document_mut(token).expect("document");
            let mut output = egui::TextEdit::multiline(&mut document.text)
                .id(editor_id)
                .code_editor()
                .show(ui);
            if let Some(post_selection) = injected.as_ref().and_then(|i| i.post_selection.clone()) {
                output
                    .state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(post_selection.start),
                        egui::text::CCursor::new(post_selection.end),
                    )));
                output.state.store(ui.ctx(), editor_id);
            }
            injected_out = injected;
        });
        injected_out
    }

    fn undo_once(
        ctx: &egui::Context,
        state: &mut TextEditorState,
        token: u64,
        editor_id: egui::Id,
    ) {
        let modifiers = egui::Modifiers::COMMAND;
        let mut input = egui_input(vec![key_event(egui::Key::Z, modifiers)]);
        input.modifiers = modifiers;
        let _ = ctx.run_ui(input, |ui| {
            let document = state.document_mut(token).expect("document");
            let _ = egui::TextEdit::multiline(&mut document.text)
                .id(editor_id)
                .code_editor()
                .show(ui);
        });
    }

    #[test]
    fn comment_toggle_injects_one_undoable_edit() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(12, "/tmp/main.py"), "a = 1\nb = 2");
        let (ctx, editor_id) = focused_editor(&mut state, token, 1, "ops_comment");

        let modifiers = egui::Modifiers::COMMAND;
        let mut input = egui_input(vec![key_event(egui::Key::Slash, modifiers)]);
        input.modifiers = modifiers;
        let injected = drive_ops_frame(&ctx, &mut state, token, editor_id, Language::Python, input);
        assert!(injected.is_some(), "Ctrl+/ 应排队注入");
        assert_eq!(
            state.document(token).expect("document").text,
            "# a = 1\nb = 2"
        );
        let cursor = egui::widgets::text_edit::TextEditState::load(&ctx, editor_id)
            .and_then(|edit_state| edit_state.cursor.char_range())
            .and_then(|range| range.single())
            .expect("cursor");
        assert_eq!(cursor.index, 8, "空选区注释后光标移到下一行行首");

        undo_once(&ctx, &mut state, token, editor_id);
        assert_eq!(
            state.document(token).expect("document").text,
            "a = 1\nb = 2",
            "注释切换必须能被一次 Ctrl+Z 完整撤销"
        );
    }

    #[test]
    fn typed_open_bracket_auto_closes_pair() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(13, "/tmp/a.rs"), "a ");
        let (ctx, editor_id) = focused_editor(&mut state, token, 2, "ops_autoclose");

        let injected = drive_ops_frame(
            &ctx,
            &mut state,
            token,
            editor_id,
            Language::Rust,
            egui_input(vec![egui::Event::Text("(".to_owned())]),
        );
        assert!(injected.is_some(), "开括号应注入成对符号");
        assert_eq!(state.document(token).expect("document").text, "a ()");
        let cursor = egui::widgets::text_edit::TextEditState::load(&ctx, editor_id)
            .and_then(|edit_state| edit_state.cursor.char_range())
            .and_then(|range| range.single())
            .expect("cursor");
        assert_eq!(cursor.index, 3, "光标落在成对符号中间");
    }

    #[test]
    fn typed_closer_skips_existing_closer() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(14, "/tmp/a.rs"), "()");
        let (ctx, editor_id) = focused_editor(&mut state, token, 1, "ops_skip");

        let injected = drive_ops_frame(
            &ctx,
            &mut state,
            token,
            editor_id,
            Language::Rust,
            egui_input(vec![egui::Event::Text(")".to_owned())]),
        );
        assert!(injected.is_none(), "越过闭合符不产生文本编辑");
        assert_eq!(state.document(token).expect("document").text, "()");
        let cursor = egui::widgets::text_edit::TextEditState::load(&ctx, editor_id)
            .and_then(|edit_state| edit_state.cursor.char_range())
            .and_then(|range| range.single())
            .expect("cursor");
        assert_eq!(cursor.index, 2, "光标右移越过已有闭合符");
    }

    #[test]
    fn backspace_deletes_empty_pair() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(15, "/tmp/a.rs"), "()");
        let (ctx, editor_id) = focused_editor(&mut state, token, 1, "ops_pair_bs");

        let injected = drive_ops_frame(
            &ctx,
            &mut state,
            token,
            editor_id,
            Language::Rust,
            egui_input(vec![key_event(egui::Key::Backspace, egui::Modifiers::NONE)]),
        );
        assert!(injected.is_some(), "空对中间退格应注入整对删除");
        assert_eq!(state.document(token).expect("document").text, "");
    }

    #[test]
    fn delete_line_shortcut_removes_current_line() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(16, "/tmp/a.rs"), "keep\ndel\nkeep2");
        let (ctx, editor_id) = focused_editor(&mut state, token, 6, "ops_del_line");

        let modifiers = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        let mut input = egui_input(vec![key_event(egui::Key::K, modifiers)]);
        input.modifiers = modifiers;
        let injected = drive_ops_frame(&ctx, &mut state, token, editor_id, Language::Rust, input);
        assert!(injected.is_some());
        assert_eq!(state.document(token).expect("document").text, "keep\nkeep2");
    }

    #[test]
    fn move_line_up_swaps_with_previous() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(17, "/tmp/a.rs"), "one\ntwo");
        let (ctx, editor_id) = focused_editor(&mut state, token, 5, "ops_move");

        let modifiers = egui::Modifiers::ALT;
        let mut input = egui_input(vec![key_event(egui::Key::ArrowUp, modifiers)]);
        input.modifiers = modifiers;
        let injected = drive_ops_frame(&ctx, &mut state, token, editor_id, Language::Rust, input);
        assert!(injected.is_some());
        assert_eq!(state.document(token).expect("document").text, "two\none");
    }

    #[test]
    fn replace_all_rewrites_document_as_one_undoable_edit() {
        let mut state = TextEditorState::default();
        let token = open_loaded(&mut state, remote(18, "/tmp/a.txt"), "foo bar foo");
        state.find_query = "foo".to_owned();
        state.replace_query = "baz".to_owned();
        replace_all_matches(&mut state, token);
        assert!(state.pending_completion.is_some(), "全部替换应排队注入");

        let ctx = egui::Context::default();
        let editor_id = egui::Id::new("ops_replace_all");
        let _ = ctx.run_ui(egui_input(Vec::new()), |ui| {
            assert!(inject_pending_completion(ui, &mut state, token, editor_id).is_some());
            let document = state.document_mut(token).expect("document");
            let _ = egui::TextEdit::multiline(&mut document.text)
                .id(editor_id)
                .code_editor()
                .show(ui);
        });
        assert_eq!(state.document(token).expect("document").text, "baz bar baz");

        undo_once(&ctx, &mut state, token, editor_id);
        assert_eq!(
            state.document(token).expect("document").text,
            "foo bar foo",
            "全部替换必须能被一次 Ctrl+Z 完整撤销"
        );
    }
}
