//! Linux file tree for an active SSH session.
//!
//! Rendering is intentionally delegated to the shared file-tree primitives in
//! [`super::filetree`]. SSH paths remain opaque UTF-8 strings with `/`
//! separators and are never converted to a Windows `PathBuf`.

use std::cell::RefCell;
use std::time::Duration;

use egui::RichText;
use egui_ltreeview::{Action, NodeBuilder, TreeView, TreeViewBuilder, TreeViewState};
use lumen_ssh::DirectoryEntryKind;

use super::filetree::{
    merge_double_click_activation, shared_filetree_panel, shared_filetree_root_label,
    shared_filetree_search_button, shared_ltree_dir_label, shared_ltree_leaf_label,
    shared_tree_context_menu, shared_tree_placeholder_row, shared_tree_search_result_row,
    SharedTreeMenuAction, SharedTreeMenuSpec,
};
use super::theme::Palette;

const SEARCH_MIN_CHARS: usize = 2;
const SEARCH_DEBOUNCE_SECONDS: f64 = 0.25;

/// SSH 文件树的完整交互意图。
///
/// `action` 字段仍保留旧的展开/整树刷新兼容链路；新后端应消费这里的
/// intent，以便目录级刷新、文件传输和编辑不再挤进终端输入通道。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SshFileTreeIntent {
    Select {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
        is_directory: bool,
    },
    ClearSelection {
        session_id: crate::ssh_runtime::SshSessionId,
    },
    ToggleDirectory {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
    },
    RefreshDirectory {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
    },
    ChangeDirectory {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
    },
    OpenLocalCopy {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
        name: String,
        size: u64,
    },
    Edit {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
        name: String,
        size: u64,
    },
    CopyFiles {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
        name: String,
        is_directory: bool,
        size: u64,
    },
    PasteInto {
        session_id: crate::ssh_runtime::SshSessionId,
        directory: String,
    },
    MoveEntry {
        session_id: crate::ssh_runtime::SshSessionId,
        source_path: String,
        source_is_directory: bool,
        target_directory: String,
    },
    CreateEntry {
        session_id: crate::ssh_runtime::SshSessionId,
        directory: String,
        is_directory: bool,
    },
    Delete {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
        name: String,
        is_directory: bool,
    },
    /// 右键「重命名」：shell 据此开改名对话框（预填原名），确认后走 SFTP rename（同目录）。
    Rename {
        session_id: crate::ssh_runtime::SshSessionId,
        path: String,
        name: String,
        is_directory: bool,
    },
    Search {
        session_id: crate::ssh_runtime::SshSessionId,
        query: String,
    },
}

#[derive(Default)]
pub struct Output {
    pub intents: Vec<SshFileTreeIntent>,
    pub panel_width: Option<f32>,
    pub panel_rect: Option<egui::Rect>,
    pub hovered: bool,
    pub focused: bool,
    pub copy_text: Option<String>,
    /// 目录激活时 SSH shell 正忙，未向终端注入 `cd`。
    pub busy_hint: bool,
}

#[derive(Clone, Default)]
struct SearchUiState {
    open: bool,
    query: String,
    focus: bool,
    changed_at: Option<f64>,
}

#[derive(Clone, Copy)]
struct EntryView<'a> {
    path: &'a str,
    name: &'a str,
    kind: DirectoryEntryKind,
    size: u64,
    is_root: bool,
}

#[derive(Clone)]
struct OwnedEntry {
    path: String,
    name: String,
    kind: DirectoryEntryKind,
    size: u64,
    depth: usize,
    expanded: bool,
    loading: bool,
    is_root: bool,
}

impl OwnedEntry {
    fn as_view(&self) -> EntryView<'_> {
        EntryView {
            path: &self.path,
            name: &self.name,
            kind: self.kind,
            size: self.size,
            is_root: self.is_root,
        }
    }
}

#[derive(Default)]
struct DeferredTreeOutput {
    refresh: Option<String>,
    menu: Option<(SharedTreeMenuAction, OwnedEntry)>,
    context_selection: Option<OwnedEntry>,
}

struct LtreeBuildCtx<'a> {
    entries: &'a [OwnedEntry],
    tree: &'a crate::ssh_runtime::SshFileTreeView,
    pal: &'a Palette,
    permissions: TreePermissions,
    deferred: &'a RefCell<DeferredTreeOutput>,
}

#[derive(Clone, Copy)]
struct TreePermissions {
    shell_idle: bool,
    can_paste: bool,
}

pub fn show(
    root: &mut egui::Ui,
    tree: Option<&crate::ssh_runtime::SshFileTreeView>,
    visible: bool,
    pal: &Palette,
    width: f32,
    shell_idle: bool,
    can_paste: bool,
) -> Output {
    if !visible {
        return Output::default();
    }

    let mut output = Output::default();
    let panel = shared_filetree_panel(root, pal, width, |ui| {
        draw_contents(ui, tree, pal, shell_idle, can_paste, &mut output);
    });
    output.panel_width = Some(panel.width);
    output.panel_rect = Some(panel.rect);
    output.hovered = panel.hovered;
    output
}

fn draw_contents(
    ui: &mut egui::Ui,
    tree: Option<&crate::ssh_runtime::SshFileTreeView>,
    pal: &Palette,
    shell_idle: bool,
    can_paste: bool,
    output: &mut Output,
) {
    let strings = crate::i18n::strings();
    let permissions = TreePermissions {
        shell_idle,
        can_paste,
    };
    let session_key = tree.map_or(0, |tree| tree.session_id);
    let root_key = tree.map_or("", |tree| tree.root.as_str());
    let search_id = ui.make_persistent_id(("ssh_filetree_search", session_key, root_key));
    let mut search = ui
        .data(|data| data.get_temp::<SearchUiState>(search_id))
        .unwrap_or_default();

    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if shared_filetree_search_button(ui, search.open, pal).clicked() {
                search.open = !search.open;
                search.focus = search.open;
                if !search.open {
                    search.query.clear();
                    search.changed_at = None;
                    if let Some(tree) = tree {
                        output.intents.push(SshFileTreeIntent::Search {
                            session_id: tree.session_id,
                            query: String::new(),
                        });
                    }
                }
            }
            let title = tree.map_or(strings.filetree_root_placeholder, |tree| {
                linux_basename(&tree.root)
            });
            // 与本地目录树一致：标题只显示根名；根目录右键菜单挂在
            // 下方原生 TreeView 的根节点上。
            shared_filetree_root_label(ui, title, tree.map(|tree| tree.root.as_str()), pal);
        });
    });

    if search.open {
        let response = ui.add(
            egui::TextEdit::singleline(&mut search.query)
                .hint_text(strings.filetree_search_hint)
                .desired_width(f32::INFINITY),
        );
        if search.focus {
            response.request_focus();
            search.focus = false;
        }
        if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            search.open = false;
            search.query.clear();
            search.changed_at = None;
            if let Some(tree) = tree {
                output.intents.push(SshFileTreeIntent::Search {
                    session_id: tree.session_id,
                    query: String::new(),
                });
            }
        } else if response.changed() {
            if search.query.trim().chars().count() >= SEARCH_MIN_CHARS {
                search.changed_at = Some(ui.input(|input| input.time));
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            } else {
                search.changed_at = None;
                if let Some(tree) = tree {
                    output.intents.push(SshFileTreeIntent::Search {
                        session_id: tree.session_id,
                        query: String::new(),
                    });
                }
            }
        }
    }

    if let (Some(tree), Some(changed_at)) = (tree, search.changed_at) {
        let now = ui.input(|input| input.time);
        if now - changed_at >= SEARCH_DEBOUNCE_SECONDS {
            output.intents.push(SshFileTreeIntent::Search {
                session_id: tree.session_id,
                query: search.query.trim().to_owned(),
            });
            search.changed_at = None;
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }

    let Some(tree) = tree else {
        ui.add_space(8.0);
        ui.label(
            RichText::new(strings.filetree_waiting_cwd)
                .size(11.0)
                .color(pal.fg_dim),
        );
        ui.data_mut(|data| data.insert_temp(search_id, search));
        return;
    };

    ui.add_space(2.0);
    let selected_id =
        ui.make_persistent_id(("ssh_filetree_selected", tree.session_id, tree.root.as_str()));
    let mut selected_path = ui.data(|data| data.get_temp::<String>(selected_id));
    let searching = search.open && search.query.trim().chars().count() >= SEARCH_MIN_CHARS;
    if searching {
        draw_search_results(
            ui,
            tree,
            search.query.trim(),
            pal,
            permissions,
            &mut selected_path,
            output,
        );
    } else {
        draw_tree(ui, tree, pal, permissions, &mut selected_path, output);
    }
    if let Some(selected_path) = selected_path {
        ui.data_mut(|data| data.insert_temp(selected_id, selected_path));
    } else {
        ui.data_mut(|data| data.remove::<String>(selected_id));
    }
    ui.data_mut(|data| data.insert_temp(search_id, search));
}

fn draw_tree(
    ui: &mut egui::Ui,
    tree: &crate::ssh_runtime::SshFileTreeView,
    pal: &Palette,
    permissions: TreePermissions,
    selected_path: &mut Option<String>,
    output: &mut Output,
) {
    let strings = crate::i18n::strings();
    let state_id =
        ui.make_persistent_id(("lumen_ssh_file_tree", tree.session_id, tree.root.as_str()));
    let mut state = TreeViewState::<String>::load(ui, state_id).unwrap_or_default();
    let root_entry = OwnedEntry {
        path: tree.root.clone(),
        name: linux_basename(&tree.root).to_owned(),
        kind: DirectoryEntryKind::Directory,
        size: 0,
        depth: 0,
        expanded: true,
        loading: tree.rows.is_empty() && tree.loading,
        is_root: true,
    };
    let entries = tree
        .rows
        .iter()
        .map(|row| OwnedEntry {
            path: row.path.clone(),
            name: row.name.clone(),
            kind: row.kind,
            size: row.size,
            depth: row.depth,
            expanded: row.expanded,
            loading: row.loading,
            is_root: false,
        })
        .collect::<Vec<_>>();

    // SSH runtime owns the directory cache/open state. Seed ltreeview from that
    // snapshot before drawing, then turn any closer changes back into runtime
    // intents after drawing.
    for entry in &entries {
        if entry.kind == DirectoryEntryKind::Directory {
            state.set_openness(entry.path.clone(), entry.expanded);
        }
    }
    if let Some(path) = selected_path.as_ref() {
        state.set_one_selected(path.clone());
    }

    let deferred = RefCell::new(DeferredTreeOutput::default());
    egui::ScrollArea::both()
        .id_salt(("ssh_filetree_scroll", tree.session_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let (response, actions) = TreeView::new(state_id)
                .allow_multi_selection(false)
                .allow_drag_and_drop(true)
                .show_state(ui, &mut state, |builder| {
                    let root_open =
                        add_ltree_entry(builder, &root_entry, tree, pal, permissions, &deferred);
                    if root_open {
                        if root_entry.loading {
                            add_ltree_placeholder(
                                builder,
                                "\0ssh:root-loading".to_owned(),
                                strings.filetree_loading,
                                pal,
                            );
                        }
                        let mut next = 0;
                        let build = LtreeBuildCtx {
                            entries: &entries,
                            tree,
                            pal,
                            permissions,
                            deferred: &deferred,
                        };
                        add_ltree_rows(builder, &mut next, 0, &build);
                        if tree.truncated {
                            add_ltree_placeholder(
                                builder,
                                "\0ssh:truncated".to_owned(),
                                strings.filetree_truncated,
                                pal,
                            );
                        }
                        if let Some(error) = &tree.error {
                            add_ltree_error_placeholder(
                                builder,
                                "\0ssh:error".to_owned(),
                                strings.filetree_unreadable,
                                error,
                                pal,
                            );
                        }
                    }
                    builder.close_dir();
                });
            if response.clicked() || response.secondary_clicked() {
                response.request_focus();
            }
            output.focused = response.has_focus();

            let mut activated = Vec::new();
            let mut selected_now = None;
            let mut move_intent = None;
            for action in actions {
                match action {
                    Action::Activate(action) => activated.extend(action.selected),
                    Action::SetSelected(selected) => selected_now = Some(selected),
                    Action::Move(action) => {
                        let Some(source) = action.source.first() else {
                            continue;
                        };
                        move_intent = move_entry_intent(
                            tree.session_id,
                            &root_entry,
                            &entries,
                            source,
                            &action.target,
                        );
                    }
                    Action::Drag(action) => {
                        let allowed = action.source.first().is_some_and(|source| {
                            move_entry_intent(
                                tree.session_id,
                                &root_entry,
                                &entries,
                                source,
                                &action.target,
                            )
                            .is_some()
                        });
                        if !allowed {
                            action.remove_drop_marker(ui);
                        }
                    }
                    Action::DragExternal(_) | Action::MoveExternal(_) => {}
                }
            }

            let selected_for_activation = selected_now.clone();
            if let Some(selected) = selected_now {
                if let Some(entry) = selected
                    .last()
                    .and_then(|id| entry_for_path(&root_entry, &entries, id))
                {
                    if selected_path.as_deref() != Some(entry.path.as_str()) {
                        select_entry(
                            tree.session_id,
                            &entry.path,
                            entry.kind == DirectoryEntryKind::Directory,
                            selected_path,
                            output,
                        );
                    }
                } else if selected.is_empty() && selected_path.take().is_some() {
                    output.intents.push(SshFileTreeIntent::ClearSelection {
                        session_id: tree.session_id,
                    });
                }
            }

            for id in merge_double_click_activation(
                activated,
                response.double_clicked(),
                selected_for_activation,
            ) {
                let Some(entry) = entry_for_path(&root_entry, &entries, &id) else {
                    continue;
                };
                if selected_path.as_deref() != Some(entry.path.as_str()) {
                    select_entry(
                        tree.session_id,
                        &entry.path,
                        entry.kind == DirectoryEntryKind::Directory,
                        selected_path,
                        output,
                    );
                }
                if entry.kind == DirectoryEntryKind::Directory {
                    queue_change_directory(
                        output,
                        tree.session_id,
                        &entry.path,
                        permissions.shell_idle,
                    );
                } else {
                    output.intents.push(SshFileTreeIntent::OpenLocalCopy {
                        session_id: tree.session_id,
                        path: entry.path.clone(),
                        name: entry.name.clone(),
                        size: entry.size,
                    });
                }
            }

            if let Some(intent) = move_intent {
                state.set_selected(Vec::new());
                *selected_path = None;
                output.intents.push(intent);
            }
        });

    for entry in &entries {
        if entry.kind != DirectoryEntryKind::Directory {
            continue;
        }
        let open = state.is_open(&entry.path).unwrap_or(entry.expanded);
        if open != entry.expanded {
            output.intents.push(SshFileTreeIntent::ToggleDirectory {
                session_id: tree.session_id,
                path: entry.path.clone(),
            });
        }
    }
    state.store(ui, state_id);

    let deferred = deferred.into_inner();
    if let Some(entry) = deferred.context_selection {
        let is_directory = entry.kind == DirectoryEntryKind::Directory;
        if selected_path.as_deref() != Some(entry.path.as_str()) {
            select_entry(
                tree.session_id,
                &entry.path,
                is_directory,
                selected_path,
                output,
            );
        }
    }
    if let Some(path) = deferred.refresh {
        output.intents.push(SshFileTreeIntent::RefreshDirectory {
            session_id: tree.session_id,
            path,
        });
    }
    if let Some((action, entry)) = deferred.menu {
        apply_menu_action(
            action,
            entry.as_view(),
            tree,
            permissions.shell_idle,
            output,
        );
    }
}

fn draw_search_results(
    ui: &mut egui::Ui,
    tree: &crate::ssh_runtime::SshFileTreeView,
    query: &str,
    pal: &Palette,
    permissions: TreePermissions,
    selected_path: &mut Option<String>,
    output: &mut Output,
) {
    let strings = crate::i18n::strings();
    let needle = query.to_lowercase();
    let backend_matches_query = tree.search_query.as_deref() == Some(query);
    let matches = if backend_matches_query {
        tree.search_rows.iter().collect::<Vec<_>>()
    } else {
        tree.rows
            .iter()
            .filter(|row| row.path.to_lowercase().contains(&needle))
            .collect::<Vec<_>>()
    };
    egui::ScrollArea::both()
        .id_salt(("ssh_filetree_search_scroll", tree.session_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if matches.is_empty() {
                let placeholder = if tree.search_loading || !backend_matches_query {
                    strings.filetree_searching
                } else if tree.search_error.is_some() {
                    strings.filetree_unreadable
                } else {
                    strings.filetree_no_results
                };
                shared_tree_placeholder_row(ui, 0, placeholder, pal);
                if let Some(error) = &tree.search_error {
                    ui.label(egui::RichText::new(error).size(10.0).color(pal.fg_dim));
                }
                return;
            }
            for row in matches {
                let is_directory = row.kind == DirectoryEntryKind::Directory;
                let relative = relative_linux_path(&row.path, &tree.root);
                let text = if is_directory {
                    format!("{relative}/")
                } else {
                    relative
                };
                let response = shared_tree_search_result_row(
                    ui,
                    egui::Id::new(("ssh_filetree_search_result", tree.session_id, &row.path)),
                    &text,
                    &row.path,
                    selected_path.as_deref() == Some(row.path.as_str()),
                    pal,
                );
                if response.clicked() || response.secondary_clicked() {
                    response.request_focus();
                }
                output.focused |= response.has_focus();
                if response.double_clicked() {
                    select_entry(
                        tree.session_id,
                        &row.path,
                        is_directory,
                        selected_path,
                        output,
                    );
                    if is_directory {
                        queue_change_directory(
                            output,
                            tree.session_id,
                            &row.path,
                            permissions.shell_idle,
                        );
                    } else {
                        output.intents.push(SshFileTreeIntent::OpenLocalCopy {
                            session_id: tree.session_id,
                            path: row.path.clone(),
                            name: row.name.clone(),
                            size: row.size,
                        });
                    }
                } else if response.clicked() || response.secondary_clicked() {
                    select_entry(
                        tree.session_id,
                        &row.path,
                        is_directory,
                        selected_path,
                        output,
                    );
                }
                show_context_menu(
                    &response,
                    EntryView {
                        path: &row.path,
                        name: &row.name,
                        kind: row.kind,
                        size: row.size,
                        is_root: false,
                    },
                    tree,
                    permissions.shell_idle,
                    permissions.can_paste,
                    output,
                );
            }
            if tree.search_truncated {
                shared_tree_placeholder_row(ui, 0, strings.filetree_truncated, pal);
            }
        });
}

fn add_ltree_rows(
    builder: &mut TreeViewBuilder<'_, String>,
    next: &mut usize,
    depth: usize,
    build: &LtreeBuildCtx<'_>,
) {
    while let Some(entry) = build.entries.get(*next) {
        if entry.depth < depth {
            return;
        }
        if entry.depth > depth {
            // A malformed snapshot must not escape its missing parent. Runtime
            // snapshots are normally contiguous, so this is only a defensive skip.
            *next += 1;
            continue;
        }

        let entry_index = *next;
        *next += 1;
        let open = add_ltree_entry(
            builder,
            entry,
            build.tree,
            build.pal,
            build.permissions,
            build.deferred,
        );
        if entry.kind != DirectoryEntryKind::Directory {
            continue;
        }

        if open {
            if entry.loading || !entry.expanded {
                add_ltree_placeholder(
                    builder,
                    format!("\0ssh:loading:{entry_index}:{}", entry.path),
                    crate::i18n::strings().filetree_loading,
                    build.pal,
                );
            }
            add_ltree_rows(builder, next, depth.saturating_add(1), build);
        } else {
            while build
                .entries
                .get(*next)
                .is_some_and(|child| child.depth > depth)
            {
                *next += 1;
            }
        }
        builder.close_dir();
    }
}

fn add_ltree_entry(
    builder: &mut TreeViewBuilder<'_, String>,
    entry: &OwnedEntry,
    tree: &crate::ssh_runtime::SshFileTreeView,
    pal: &Palette,
    permissions: TreePermissions,
    deferred: &RefCell<DeferredTreeOutput>,
) -> bool {
    let is_directory = entry.kind == DirectoryEntryKind::Directory;
    let id = entry.path.clone();
    let label = entry.name.clone();
    let menu_entry = entry.clone();
    let menu = deferred;
    let menu_spec = SharedTreeMenuSpec {
        is_directory,
        is_root: entry.is_root,
        can_paste: permissions.can_paste,
        can_reveal: false,
        can_edit: !is_directory,
        can_delete: true,
        // 同目录改名走 SFTP rename（服务端原子操作，撞名回 Conflict、不覆盖）。
        can_rename: true,
        permanent_delete: true,
    };
    if !is_directory {
        builder.node(
            NodeBuilder::leaf(id)
                .label_ui(move |ui| shared_ltree_leaf_label(ui, &label))
                .context_menu(move |ui| {
                    menu.borrow_mut().context_selection = Some(menu_entry.clone());
                    if let Some(action) = shared_tree_context_menu(ui, menu_spec) {
                        menu.borrow_mut().menu = Some((action, menu_entry.clone()));
                    }
                }),
        );
        return false;
    }

    let refresh_path = entry.path.clone();
    let refresh_id = egui::Id::new(("lumen_ssh_rf", tree.session_id, entry.path.as_str()));
    let refresh = deferred;
    let fg_dim = pal.fg_dim;
    builder.node(
        NodeBuilder::dir(id)
            .activatable(true)
            .default_open(entry.is_root)
            .drop_allowed(true)
            .label_ui(move |ui| {
                if shared_ltree_dir_label(ui, &label, refresh_id, fg_dim) {
                    refresh.borrow_mut().refresh = Some(refresh_path.clone());
                }
            })
            .context_menu(move |ui| {
                menu.borrow_mut().context_selection = Some(menu_entry.clone());
                if let Some(action) = shared_tree_context_menu(ui, menu_spec) {
                    menu.borrow_mut().menu = Some((action, menu_entry.clone()));
                }
            }),
    )
}

fn add_ltree_placeholder(
    builder: &mut TreeViewBuilder<'_, String>,
    id: String,
    text: &str,
    pal: &Palette,
) {
    builder.node(
        NodeBuilder::leaf(id).activatable(false).label(
            egui::RichText::new(text)
                .size(11.0)
                .color(pal.fg_dim)
                .italics(),
        ),
    );
}

fn add_ltree_error_placeholder(
    builder: &mut TreeViewBuilder<'_, String>,
    id: String,
    text: &str,
    error: &str,
    pal: &Palette,
) {
    let text = text.to_owned();
    let error = error.to_owned();
    let fg_dim = pal.fg_dim;
    builder.node(
        NodeBuilder::leaf(id)
            .activatable(false)
            .label_ui(move |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&text)
                            .size(11.0)
                            .color(fg_dim)
                            .italics(),
                    )
                    .selectable(false),
                )
                .on_hover_text(&error);
            }),
    );
}

fn entry_for_path<'a>(
    root: &'a OwnedEntry,
    entries: &'a [OwnedEntry],
    path: &str,
) -> Option<&'a OwnedEntry> {
    if path == root.path {
        Some(root)
    } else {
        entries.iter().find(|entry| entry.path == path)
    }
}

fn select_entry(
    session_id: crate::ssh_runtime::SshSessionId,
    path: &str,
    is_directory: bool,
    selected_path: &mut Option<String>,
    output: &mut Output,
) {
    *selected_path = Some(path.to_owned());
    output.intents.push(SshFileTreeIntent::Select {
        session_id,
        path: path.to_owned(),
        is_directory,
    });
}

fn show_context_menu(
    response: &egui::Response,
    entry: EntryView<'_>,
    tree: &crate::ssh_runtime::SshFileTreeView,
    shell_idle: bool,
    can_paste: bool,
    output: &mut Output,
) {
    let is_directory = entry.kind == DirectoryEntryKind::Directory;
    egui::Popup::context_menu(response).show(|ui| {
        let Some(action) = shared_tree_context_menu(
            ui,
            SharedTreeMenuSpec {
                is_directory,
                is_root: entry.is_root,
                can_paste,
                can_reveal: false,
                can_edit: !is_directory,
                can_delete: true,
                // 同上：同目录改名走 SFTP rename。
                can_rename: true,
                permanent_delete: true,
            },
        ) else {
            return;
        };
        apply_menu_action(action, entry, tree, shell_idle, output);
    });
}

fn apply_menu_action(
    action: SharedTreeMenuAction,
    entry: EntryView<'_>,
    tree: &crate::ssh_runtime::SshFileTreeView,
    shell_idle: bool,
    output: &mut Output,
) {
    let session_id = tree.session_id;
    let is_directory = entry.kind == DirectoryEntryKind::Directory;
    let parent = linux_parent(entry.path).unwrap_or(entry.path);
    let target_directory = if is_directory { entry.path } else { parent };
    match action {
        SharedTreeMenuAction::EnterDirectory => {
            queue_change_directory(output, session_id, entry.path, shell_idle);
        }
        SharedTreeMenuAction::Edit => {
            output.intents.push(SshFileTreeIntent::Edit {
                session_id,
                path: entry.path.to_owned(),
                name: entry.name.to_owned(),
                size: entry.size,
            });
        }
        SharedTreeMenuAction::CopyFiles => {
            output.intents.push(SshFileTreeIntent::CopyFiles {
                session_id,
                path: entry.path.to_owned(),
                name: entry.name.to_owned(),
                is_directory,
                size: entry.size,
            });
        }
        SharedTreeMenuAction::Paste => {
            output.intents.push(SshFileTreeIntent::PasteInto {
                session_id,
                directory: target_directory.to_owned(),
            });
        }
        SharedTreeMenuAction::NewFile | SharedTreeMenuAction::NewDirectory => {
            output.intents.push(SshFileTreeIntent::CreateEntry {
                session_id,
                directory: target_directory.to_owned(),
                is_directory: action == SharedTreeMenuAction::NewDirectory,
            });
        }
        SharedTreeMenuAction::CopyAbsolutePath => {
            output.copy_text = Some(entry.path.to_owned());
        }
        SharedTreeMenuAction::CopyRelativePath => {
            output.copy_text = Some(relative_linux_path(entry.path, &tree.root));
        }
        SharedTreeMenuAction::Delete => {
            output.intents.push(SshFileTreeIntent::Delete {
                session_id,
                path: entry.path.to_owned(),
                name: entry.name.to_owned(),
                is_directory,
            });
        }
        SharedTreeMenuAction::Rename => {
            output.intents.push(SshFileTreeIntent::Rename {
                session_id,
                path: entry.path.to_owned(),
                name: entry.name.to_owned(),
                is_directory,
            });
        }
        SharedTreeMenuAction::Reveal => {}
    }
}

fn move_entry_intent(
    session_id: crate::ssh_runtime::SshSessionId,
    root: &OwnedEntry,
    entries: &[OwnedEntry],
    source_path: &str,
    target_directory: &str,
) -> Option<SshFileTreeIntent> {
    let source = entry_for_path(root, entries, source_path)?;
    let target = entry_for_path(root, entries, target_directory)?;
    if source.is_root || target.kind != DirectoryEntryKind::Directory {
        return None;
    }

    let source_parent = linux_parent(&source.path)?;
    if source_parent == target.path {
        return None;
    }
    let source_is_directory = source.kind == DirectoryEntryKind::Directory;
    if source_is_directory && linux_path_is_same_or_descendant(&target.path, &source.path) {
        return None;
    }

    Some(SshFileTreeIntent::MoveEntry {
        session_id,
        source_path: source.path.clone(),
        source_is_directory,
        target_directory: target.path.clone(),
    })
}

fn linux_path_is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn queue_change_directory(
    output: &mut Output,
    session_id: crate::ssh_runtime::SshSessionId,
    path: &str,
    shell_idle: bool,
) {
    if shell_idle {
        output.intents.push(SshFileTreeIntent::ChangeDirectory {
            session_id,
            path: path.to_owned(),
        });
    } else {
        output.busy_hint = true;
    }
}

fn linux_basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
}

fn linux_parent(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    let (parent, _) = trimmed.rsplit_once('/')?;
    Some(if parent.is_empty() { "/" } else { parent })
}

fn relative_linux_path(path: &str, root: &str) -> String {
    path.strip_prefix(root)
        .map(|relative| relative.trim_start_matches('/').to_owned())
        .filter(|relative| !relative.is_empty())
        .unwrap_or_else(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tree() -> crate::ssh_runtime::SshFileTreeView {
        crate::ssh_runtime::SshFileTreeView {
            session_id: 7,
            profile_id: "ssh_test".to_owned(),
            root: "/srv/app".to_owned(),
            rows: Vec::new(),
            loading: false,
            truncated: false,
            error: None,
            search_query: None,
            search_rows: Vec::new(),
            search_loading: false,
            search_truncated: false,
            search_error: None,
        }
    }

    #[test]
    fn linux_paths_stay_opaque_and_relative_to_the_tree_root() {
        assert_eq!(
            relative_linux_path("/home/alice/project/src/main.rs", "/home/alice/project"),
            "src/main.rs"
        );
        assert_eq!(linux_parent("/home/alice/file.txt"), Some("/home/alice"));
        assert_eq!(linux_parent("/file.txt"), Some("/"));
        assert_eq!(linux_basename("/home/alice/"), "alice");
    }

    #[test]
    fn new_entry_targets_a_files_parent_but_a_directory_itself() {
        let file = EntryView {
            path: "/srv/app/main.rs",
            name: "main.rs",
            kind: DirectoryEntryKind::File,
            size: 12,
            is_root: false,
        };
        let directory = EntryView {
            path: "/srv/app",
            name: "app",
            kind: DirectoryEntryKind::Directory,
            size: 0,
            is_root: false,
        };
        assert_eq!(linux_parent(file.path), Some("/srv/app"));
        assert_eq!(
            if directory.kind == DirectoryEntryKind::Directory {
                directory.path
            } else {
                linux_parent(directory.path).unwrap()
            },
            "/srv/app"
        );
    }

    #[test]
    fn change_directory_is_blocked_while_the_ssh_shell_is_busy() {
        let mut output = Output::default();
        queue_change_directory(&mut output, 7, "/srv/app", false);
        assert!(output.busy_hint);
        assert!(output.intents.is_empty());

        let mut output = Output::default();
        queue_change_directory(&mut output, 7, "/srv/app", true);
        assert!(!output.busy_hint);
        assert_eq!(
            output.intents,
            vec![SshFileTreeIntent::ChangeDirectory {
                session_id: 7,
                path: "/srv/app".to_owned(),
            }]
        );
    }

    #[test]
    fn paste_on_a_file_targets_its_parent_directory() {
        let tree = test_tree();
        let file = EntryView {
            path: "/srv/app/.env",
            name: ".env",
            kind: DirectoryEntryKind::File,
            size: 12,
            is_root: false,
        };
        let mut output = Output::default();
        apply_menu_action(SharedTreeMenuAction::Paste, file, &tree, true, &mut output);
        assert_eq!(
            output.intents,
            vec![SshFileTreeIntent::PasteInto {
                session_id: 7,
                directory: "/srv/app".to_owned(),
            }]
        );
    }

    fn owned_entry(path: &str, kind: DirectoryEntryKind, is_root: bool) -> OwnedEntry {
        OwnedEntry {
            path: path.to_owned(),
            name: linux_basename(path).to_owned(),
            kind,
            size: 0,
            depth: usize::from(!is_root),
            expanded: false,
            loading: false,
            is_root,
        }
    }

    #[test]
    fn tree_drag_moves_entries_only_into_safe_different_directories() {
        let root = owned_entry("/srv", DirectoryEntryKind::Directory, true);
        let entries = vec![
            owned_entry("/srv/app", DirectoryEntryKind::Directory, false),
            owned_entry("/srv/app/src", DirectoryEntryKind::Directory, false),
            owned_entry("/srv/apple", DirectoryEntryKind::Directory, false),
            owned_entry("/srv/archive", DirectoryEntryKind::Directory, false),
            owned_entry("/srv/readme.txt", DirectoryEntryKind::File, false),
        ];

        assert_eq!(
            move_entry_intent(7, &root, &entries, "/srv/readme.txt", "/srv/archive"),
            Some(SshFileTreeIntent::MoveEntry {
                session_id: 7,
                source_path: "/srv/readme.txt".to_owned(),
                source_is_directory: false,
                target_directory: "/srv/archive".to_owned(),
            })
        );
        assert_eq!(
            move_entry_intent(7, &root, &entries, "/srv/app", "/srv/archive"),
            Some(SshFileTreeIntent::MoveEntry {
                session_id: 7,
                source_path: "/srv/app".to_owned(),
                source_is_directory: true,
                target_directory: "/srv/archive".to_owned(),
            })
        );

        assert!(
            move_entry_intent(7, &root, &entries, "/srv", "/srv/archive").is_none(),
            "文件树根节点不可移动"
        );
        assert!(
            move_entry_intent(7, &root, &entries, "/srv/readme.txt", "/srv").is_none(),
            "拖到原父目录只是无意义的同目录重排"
        );
        assert!(
            move_entry_intent(7, &root, &entries, "/srv/app", "/srv/app").is_none(),
            "目录不可移入自身"
        );
        assert!(
            move_entry_intent(7, &root, &entries, "/srv/app", "/srv/app/src").is_none(),
            "目录不可移入自己的子树"
        );
        assert!(
            move_entry_intent(7, &root, &entries, "/srv/readme.txt", "/srv/readme.txt").is_none(),
            "文件节点不是合法落点"
        );
        assert!(
            move_entry_intent(7, &root, &entries, "/srv/app", "/srv/apple").is_some(),
            "相同字符串前缀但不同路径组件不得被误判为子树"
        );
    }
}
