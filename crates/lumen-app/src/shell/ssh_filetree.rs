//! Linux file tree for an active SSH session.
//!
//! Rendering is intentionally delegated to the shared file-tree primitives in
//! [`super::filetree`]. SSH paths remain opaque UTF-8 strings with `/`
//! separators and are never converted to a Windows `PathBuf`.

use std::time::Duration;

use egui::RichText;
use lumen_ssh::DirectoryEntryKind;

use super::filetree::{
    shared_filetree_panel, shared_filetree_root_label, shared_filetree_search_button,
    shared_tree_context_menu, shared_tree_placeholder_row, shared_tree_row,
    shared_tree_search_result_row, SharedTreeMenuAction, SharedTreeMenuSpec, SharedTreeRow,
    SharedTreeRowKind,
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
    depth: usize,
    expanded: bool,
    loading: bool,
    is_root: bool,
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
            let root_response =
                shared_filetree_root_label(ui, title, tree.map(|tree| tree.root.as_str()), pal);
            if let Some(tree) = tree {
                show_context_menu(
                    &root_response,
                    EntryView {
                        path: &tree.root,
                        name: title,
                        kind: DirectoryEntryKind::Directory,
                        size: 0,
                        depth: 0,
                        expanded: true,
                        loading: tree.loading,
                        is_root: true,
                    },
                    tree,
                    permissions.shell_idle,
                    permissions.can_paste,
                    output,
                );
            }
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
    let root_open_id = ui.make_persistent_id((
        "ssh_filetree_root_open",
        tree.session_id,
        tree.root.as_str(),
    ));
    let mut root_open = ui
        .data(|data| data.get_temp::<bool>(root_open_id))
        .unwrap_or(true);
    egui::ScrollArea::both()
        .id_salt(("ssh_filetree_scroll", tree.session_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().button_padding.x = 0.0;

            let root_name = linux_basename(&tree.root);
            if draw_entry(
                ui,
                tree,
                EntryView {
                    path: &tree.root,
                    name: root_name,
                    kind: DirectoryEntryKind::Directory,
                    size: 0,
                    depth: 0,
                    expanded: root_open,
                    loading: tree.rows.is_empty() && tree.loading,
                    is_root: true,
                },
                pal,
                permissions,
                selected_path,
                output,
            ) {
                root_open = !root_open;
            }
            if root_open {
                for row in &tree.rows {
                    let _ = draw_entry(
                        ui,
                        tree,
                        EntryView {
                            path: &row.path,
                            name: &row.name,
                            kind: row.kind,
                            size: row.size,
                            depth: row.depth.saturating_add(1),
                            expanded: row.expanded,
                            loading: row.loading,
                            is_root: false,
                        },
                        pal,
                        permissions,
                        selected_path,
                        output,
                    );
                }
            }
            if root_open {
                if tree.truncated {
                    shared_tree_placeholder_row(ui, 1, strings.filetree_truncated, pal);
                }
                if let Some(error) = &tree.error {
                    let response = ui.add(
                        egui::Label::new(
                            RichText::new(strings.filetree_unreadable)
                                .size(11.0)
                                .color(pal.fg_dim)
                                .italics(),
                        )
                        .wrap(),
                    );
                    response.on_hover_text(error);
                }
            }
        });
    ui.data_mut(|data| data.insert_temp(root_open_id, root_open));
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
                        depth: 0,
                        expanded: row.expanded,
                        loading: row.loading,
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

fn draw_entry(
    ui: &mut egui::Ui,
    tree: &crate::ssh_runtime::SshFileTreeView,
    entry: EntryView<'_>,
    pal: &Palette,
    permissions: TreePermissions,
    selected_path: &mut Option<String>,
    output: &mut Output,
) -> bool {
    let is_directory = entry.kind == DirectoryEntryKind::Directory;
    let response = shared_tree_row(
        ui,
        SharedTreeRow {
            id: egui::Id::new(("ssh_filetree_row", tree.session_id, entry.path)),
            depth: entry.depth,
            name: entry.name,
            path: entry.path,
            kind: if is_directory {
                SharedTreeRowKind::Directory {
                    open: entry.expanded,
                }
            } else {
                SharedTreeRowKind::File
            },
            selected: selected_path.as_deref() == Some(entry.path),
            loading: entry.loading,
        },
        pal,
    );

    if response
        .refresh
        .as_ref()
        .is_some_and(egui::Response::clicked)
    {
        output.intents.push(SshFileTreeIntent::RefreshDirectory {
            session_id: tree.session_id,
            path: entry.path.to_owned(),
        });
    } else if response
        .triangle
        .as_ref()
        .is_some_and(egui::Response::clicked)
    {
        output.intents.push(SshFileTreeIntent::ToggleDirectory {
            session_id: tree.session_id,
            path: entry.path.to_owned(),
        });
        show_context_menu(
            &response.row,
            entry,
            tree,
            permissions.shell_idle,
            permissions.can_paste,
            output,
        );
        return true;
    } else if response.row.double_clicked() {
        select_entry(
            tree.session_id,
            entry.path,
            is_directory,
            selected_path,
            output,
        );
        if is_directory {
            queue_change_directory(output, tree.session_id, entry.path, permissions.shell_idle);
        } else {
            output.intents.push(SshFileTreeIntent::OpenLocalCopy {
                session_id: tree.session_id,
                path: entry.path.to_owned(),
                name: entry.name.to_owned(),
                size: entry.size,
            });
        }
    } else if response.row.clicked() || response.row.secondary_clicked() {
        select_entry(
            tree.session_id,
            entry.path,
            is_directory,
            selected_path,
            output,
        );
    }
    show_context_menu(
        &response.row,
        entry,
        tree,
        permissions.shell_idle,
        permissions.can_paste,
        output,
    );
    false
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
                directory: entry.path.to_owned(),
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
        SharedTreeMenuAction::Reveal => {}
    }
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
            depth: 0,
            expanded: false,
            loading: false,
            is_root: false,
        };
        let directory = EntryView {
            path: "/srv/app",
            name: "app",
            kind: DirectoryEntryKind::Directory,
            size: 0,
            depth: 0,
            expanded: true,
            loading: false,
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
}
