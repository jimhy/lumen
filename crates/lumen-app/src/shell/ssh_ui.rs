//! SSH 服务器列表与编辑表单。
//!
//! 本模块只负责展示和收集用户意图。它不持有 [`SshInventory`] 的可变引用，
//! 也不直接写磁盘或发起连接；调用方应依次处理 [`SshUiOutput::actions`]。
//! 表单有意不包含密码、口令、私钥正文或本机私钥路径。

use std::collections::HashSet;

use egui::{Color32, RichText};

use super::theme::Palette;
use crate::ssh::{
    AuthMethod, GroupId, NewSshProfile, ProfileId, SshGroup, SshInventory, SshProfile,
};

const ROW_HEIGHT: f32 = 30.0;
const SECTION_GAP: f32 = 8.0;

/// SSH 页面文案。
///
/// 目前由调用方提供，便于在不改动 i18n 模块的前提下独立接入。所有字段都对应
/// 一个预期的 i18n 字段，默认值仅用于开发预览与测试。
#[derive(Debug, Clone, Copy)]
pub struct SshUiText {
    pub title: &'static str,
    pub search_hint: &'static str,
    pub new_profile: &'static str,
    pub new_group: &'static str,
    pub ungrouped: &'static str,
    pub empty_group: &'static str,
    pub no_search_results: &'static str,
    pub edit: &'static str,
    pub delete: &'static str,
    pub connect: &'static str,
    pub rename_group: &'static str,
    pub delete_group: &'static str,
    pub create_group_title: &'static str,
    pub rename_group_title: &'static str,
    pub delete_group_title: &'static str,
    pub delete_group_message: &'static str,
    pub delete_profile_title: &'static str,
    pub delete_profile_message: &'static str,
    pub group_name: &'static str,
    pub create_profile_title: &'static str,
    pub edit_profile_title: &'static str,
    pub profile_name: &'static str,
    pub host: &'static str,
    pub port: &'static str,
    pub username: &'static str,
    pub auth_method: &'static str,
    pub auth_password: &'static str,
    pub auth_private_key: &'static str,
    pub auth_agent: &'static str,
    pub group: &'static str,
    pub initial_directory: &'static str,
    pub connect_timeout: &'static str,
    pub keep_alive: &'static str,
    pub keep_alive_disabled: &'static str,
    pub monitor_enabled: &'static str,
    pub seconds: &'static str,
    pub save: &'static str,
    pub create: &'static str,
    pub cancel: &'static str,
    pub confirm_delete: &'static str,
}

impl Default for SshUiText {
    fn default() -> Self {
        Self {
            title: "SSH 服务器",
            search_hint: "搜索服务器",
            new_profile: "新建服务器",
            new_group: "新建组",
            ungrouped: "未分组",
            empty_group: "暂无服务器",
            no_search_results: "没有匹配的服务器",
            edit: "编辑",
            delete: "删除",
            connect: "连接",
            rename_group: "重命名",
            delete_group: "删除组",
            create_group_title: "新建 SSH 分组",
            rename_group_title: "重命名 SSH 分组",
            delete_group_title: "删除 SSH 分组",
            delete_group_message: "删除分组后，其中的服务器将移到“未分组”。",
            delete_profile_title: "删除 SSH 服务器",
            delete_profile_message: "此操作将删除服务器配置。",
            group_name: "组名称",
            create_profile_title: "新建 SSH 服务器",
            edit_profile_title: "编辑 SSH 服务器",
            profile_name: "名称",
            host: "主机",
            port: "端口",
            username: "用户名",
            auth_method: "认证方式",
            auth_password: "密码",
            auth_private_key: "私钥",
            auth_agent: "SSH Agent",
            group: "分组",
            initial_directory: "初始目录",
            connect_timeout: "连接超时",
            keep_alive: "Keepalive",
            keep_alive_disabled: "关闭",
            monitor_enabled: "显示服务器监控信息",
            seconds: "秒",
            save: "保存",
            create: "创建",
            cancel: "取消",
            confirm_delete: "确认删除",
        }
    }
}

impl SshUiText {
    /// 从当前语言表构造 SSH 页面文案。
    pub fn localized() -> Self {
        let strings = crate::i18n::strings();
        Self {
            title: strings.ssh_title,
            search_hint: strings.ssh_search_hint,
            new_profile: strings.ssh_new_profile,
            new_group: strings.ssh_new_group,
            ungrouped: strings.ssh_ungrouped,
            empty_group: strings.ssh_empty_group,
            no_search_results: strings.ssh_no_search_results,
            edit: strings.ssh_edit,
            delete: strings.ssh_delete,
            connect: strings.ssh_connect,
            rename_group: strings.ssh_rename_group,
            delete_group: strings.ssh_delete_group,
            create_group_title: strings.ssh_create_group_title,
            rename_group_title: strings.ssh_rename_group_title,
            delete_group_title: strings.ssh_delete_group_title,
            delete_group_message: strings.ssh_delete_group_message,
            delete_profile_title: strings.ssh_delete_profile_title,
            delete_profile_message: strings.ssh_delete_profile_message,
            group_name: strings.ssh_group_name,
            create_profile_title: strings.ssh_create_profile_title,
            edit_profile_title: strings.ssh_edit_profile_title,
            profile_name: strings.ssh_profile_name,
            host: strings.ssh_host,
            port: strings.ssh_port,
            username: strings.ssh_username,
            auth_method: strings.ssh_auth_method,
            auth_password: strings.ssh_auth_password,
            auth_private_key: strings.ssh_auth_private_key,
            auth_agent: strings.ssh_auth_agent,
            group: strings.ssh_group,
            initial_directory: strings.ssh_initial_directory,
            connect_timeout: strings.ssh_connect_timeout,
            keep_alive: strings.ssh_keep_alive,
            keep_alive_disabled: strings.ssh_keep_alive_disabled,
            monitor_enabled: strings.ssh_monitor_enabled,
            seconds: strings.ssh_seconds,
            save: strings.ssh_save,
            create: strings.ssh_create,
            cancel: strings.ssh_cancel,
            confirm_delete: strings.ssh_confirm_delete,
        }
    }
}

/// 单帧只读输入。
pub struct SshUiInput<'a> {
    pub inventory: &'a SshInventory,
    pub text: &'a SshUiText,
}

/// UI 向存储与连接层发送的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshUiAction {
    CreateGroup {
        name: String,
    },
    RenameGroup {
        id: GroupId,
        name: String,
    },
    DeleteGroup {
        id: GroupId,
    },
    CreateProfile {
        draft: NewSshProfile,
    },
    UpdateProfile {
        id: ProfileId,
        draft: NewSshProfile,
    },
    DeleteProfile {
        id: ProfileId,
    },
    MoveProfile {
        id: ProfileId,
        target_group_id: Option<GroupId>,
        target_index: usize,
    },
    ConnectProfile {
        id: ProfileId,
    },
}

/// 单帧输出。动作按用户在本帧的操作顺序排列。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SshUiOutput {
    pub actions: Vec<SshUiAction>,
}

#[derive(Debug, Clone)]
enum Dialog {
    CreateGroup { name: String },
    RenameGroup { id: GroupId, name: String },
    DeleteGroup { id: GroupId, name: String },
    EditProfile(ProfileForm),
    DeleteProfile { id: ProfileId, name: String },
}

#[derive(Debug, Clone)]
struct ProfileForm {
    editing_id: Option<ProfileId>,
    name: String,
    host: String,
    port: u16,
    username: String,
    auth_method: AuthMethod,
    group_id: Option<GroupId>,
    initial_directory: String,
    connect_timeout_secs: u32,
    keep_alive_enabled: bool,
    keep_alive_secs: u32,
    monitor_enabled: bool,
    trusted_host_key: Option<crate::ssh::HostKeyTrust>,
}

impl ProfileForm {
    fn create() -> Self {
        Self::from_draft(None, NewSshProfile::default())
    }

    fn edit(profile: &SshProfile) -> Self {
        Self::from_draft(
            Some(profile.id.clone()),
            NewSshProfile {
                name: profile.name.clone(),
                host: profile.host.clone(),
                port: profile.port,
                username: profile.username.clone(),
                auth_method: profile.auth_method,
                group_id: profile.group_id.clone(),
                initial_directory: profile.initial_directory.clone(),
                connect_timeout_secs: profile.connect_timeout_secs,
                keep_alive_secs: profile.keep_alive_secs,
                monitor_enabled: profile.monitor_enabled,
                trusted_host_key: profile.trusted_host_key.clone(),
            },
        )
    }

    fn from_draft(editing_id: Option<ProfileId>, draft: NewSshProfile) -> Self {
        Self {
            editing_id,
            name: draft.name,
            host: draft.host,
            port: draft.port,
            username: draft.username,
            auth_method: draft.auth_method,
            group_id: draft.group_id,
            initial_directory: draft.initial_directory.unwrap_or_default(),
            connect_timeout_secs: draft.connect_timeout_secs,
            keep_alive_enabled: draft.keep_alive_secs.is_some(),
            keep_alive_secs: draft.keep_alive_secs.unwrap_or(30).max(1),
            monitor_enabled: draft.monitor_enabled,
            trusted_host_key: draft.trusted_host_key,
        }
    }

    fn valid(&self, inventory: &SshInventory) -> bool {
        !self.name.trim().is_empty()
            && !self.host.trim().is_empty()
            && !self.username.trim().is_empty()
            && self.port != 0
            && self.connect_timeout_secs != 0
            && (!self.keep_alive_enabled || self.keep_alive_secs != 0)
            && self
                .group_id
                .as_deref()
                .is_none_or(|id| inventory.group(id).is_some())
    }

    fn to_draft(&self) -> NewSshProfile {
        NewSshProfile {
            name: self.name.trim().to_owned(),
            host: self.host.trim().to_owned(),
            port: self.port,
            username: self.username.trim().to_owned(),
            auth_method: self.auth_method,
            group_id: self.group_id.clone(),
            initial_directory: optional_trimmed(&self.initial_directory),
            connect_timeout_secs: self.connect_timeout_secs,
            keep_alive_secs: self.keep_alive_enabled.then_some(self.keep_alive_secs),
            monitor_enabled: self.monitor_enabled,
            trusted_host_key: self.trusted_host_key.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DropTarget {
    group_id: Option<GroupId>,
    /// 插入位置，以源服务器已经从原列表中移除后的列表为准。
    index: usize,
}

/// SSH 页面跨帧状态。
#[derive(Debug, Default)]
pub struct SshUiState {
    search: String,
    selected_profile_id: Option<ProfileId>,
    collapsed_groups: HashSet<GroupId>,
    ungrouped_collapsed: bool,
    dialog: Option<Dialog>,
    dragged_profile_id: Option<ProfileId>,
    drop_target: Option<DropTarget>,
}

impl SshUiState {
    #[cfg(test)]
    pub fn set_search(&mut self, search: impl Into<String>) {
        self.search = search.into();
        if !self.search.trim().is_empty() {
            self.cancel_drag();
        }
    }

    pub fn selected_profile_id(&self) -> Option<&str> {
        self.selected_profile_id.as_deref()
    }

    pub fn select_profile(&mut self, id: Option<ProfileId>) {
        self.selected_profile_id = id;
    }

    /// 应用上锁时清理所有可能悬浮于解锁界面之上的瞬时 UI。
    pub fn close_for_app_lock(&mut self) {
        self.dialog = None;
        self.cancel_drag();
    }

    fn cancel_drag(&mut self) {
        self.dragged_profile_id = None;
        self.drop_target = None;
    }

    fn reconcile(&mut self, inventory: &SshInventory) {
        if self
            .selected_profile_id
            .as_deref()
            .is_some_and(|id| inventory.profile(id).is_none())
        {
            self.selected_profile_id = None;
        }
        if self
            .dragged_profile_id
            .as_deref()
            .is_some_and(|id| inventory.profile(id).is_none())
        {
            self.cancel_drag();
        }
        self.collapsed_groups
            .retain(|id| inventory.group(id).is_some());
    }
}

/// 在调用方给定的侧栏区域内绘制 SSH 服务器栏与弹窗。
pub fn show(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    input: SshUiInput<'_>,
    pal: &Palette,
) -> SshUiOutput {
    state.reconcile(input.inventory);
    let mut out = SshUiOutput::default();

    if ui.input(|events| events.key_pressed(egui::Key::Escape)) {
        if state.dragged_profile_id.is_some() {
            state.cancel_drag();
        } else {
            state.dialog = None;
        }
    }

    ui.visuals_mut().override_text_color = Some(pal.fg);
    draw_header(ui, state, input.text, pal);
    ui.add_space(SECTION_GAP);
    draw_search(ui, state, input.text, pal);
    ui.add_space(SECTION_GAP);

    if !state.search.trim().is_empty() {
        state.cancel_drag();
    } else if state.dragged_profile_id.is_some() {
        state.drop_target = None;
    }

    let query = normalized_query(&state.search);
    let mut any_visible = false;
    egui::ScrollArea::vertical()
        .id_salt("ssh_server_list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut groups: Vec<_> = input.inventory.groups().iter().collect();
            groups.sort_by_key(|group| group.sort_order);
            for group in groups {
                let all_profiles = input.inventory.profiles_in_group(Some(&group.id));
                let profiles = filtered_profiles(&all_profiles, group, &query);
                if !query.is_empty() && profiles.is_empty() {
                    continue;
                }
                any_visible = true;
                draw_group_section(
                    ui,
                    state,
                    input.inventory,
                    group,
                    &profiles,
                    input.text,
                    pal,
                    &mut out,
                );
                ui.add_space(4.0);
            }

            // “未分组”始终在所有用户分组之后。
            let all_ungrouped = input.inventory.profiles_in_group(None);
            let ungrouped: Vec<_> = all_ungrouped
                .iter()
                .copied()
                .filter(|profile| profile_matches(profile, &query))
                .collect();
            if query.is_empty() || !ungrouped.is_empty() {
                any_visible = true;
                draw_ungrouped_section(
                    ui,
                    state,
                    input.inventory,
                    &ungrouped,
                    input.text,
                    pal,
                    &mut out,
                );
            }
        });

    if !any_visible {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(input.text.no_search_results).color(pal.fg_dim));
        });
    }

    finish_drop(ui, state, input.inventory, &mut out);
    draw_dialog(ui.ctx(), state, input, pal, &mut out);
    out
}

fn draw_header(ui: &mut egui::Ui, state: &mut SshUiState, text: &SshUiText, pal: &Palette) {
    ui.horizontal(|ui| {
        ui.heading(RichText::new(text.title).size(16.0).color(pal.fg));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if small_button(ui, "+", text.new_profile, pal).clicked() {
                state.dialog = Some(Dialog::EditProfile(ProfileForm::create()));
            }
            if small_button(ui, "▦", text.new_group, pal).clicked() {
                state.dialog = Some(Dialog::CreateGroup {
                    name: String::new(),
                });
            }
        });
    });
}

fn draw_search(ui: &mut egui::Ui, state: &mut SshUiState, text: &SshUiText, pal: &Palette) {
    let response = ui.add(
        egui::TextEdit::singleline(&mut state.search)
            .hint_text(text.search_hint)
            .desired_width(f32::INFINITY)
            .text_color(pal.fg)
            .background_color(pal.extreme_bg),
    );
    if response.changed() && !state.search.trim().is_empty() {
        state.cancel_drag();
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_group_section(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    group: &SshGroup,
    profiles: &[&SshProfile],
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    let collapsed = state.collapsed_groups.contains(&group.id);
    let header = draw_group_header(ui, &group.name, collapsed, pal, |ui| {
        if small_button(ui, "✎", text.rename_group, pal).clicked() {
            state.dialog = Some(Dialog::RenameGroup {
                id: group.id.clone(),
                name: group.name.clone(),
            });
        }
        if small_button(ui, "×", text.delete_group, pal).clicked() {
            state.dialog = Some(Dialog::DeleteGroup {
                id: group.id.clone(),
                name: group.name.clone(),
            });
        }
    });
    if header.clicked() {
        if collapsed {
            state.collapsed_groups.remove(&group.id);
        } else {
            state.collapsed_groups.insert(group.id.clone());
        }
    }
    register_group_drop_target(state, inventory, &header, Some(&group.id), profiles.len());

    if !collapsed {
        if profiles.is_empty() {
            empty_row(ui, text.empty_group, pal);
        } else {
            draw_profile_rows(
                ui,
                state,
                inventory,
                Some(&group.id),
                profiles,
                text,
                pal,
                out,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ungrouped_section(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    profiles: &[&SshProfile],
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    let header = draw_group_header(ui, text.ungrouped, state.ungrouped_collapsed, pal, |_| {});
    if header.clicked() {
        state.ungrouped_collapsed = !state.ungrouped_collapsed;
    }
    register_group_drop_target(state, inventory, &header, None, profiles.len());

    if !state.ungrouped_collapsed {
        if profiles.is_empty() {
            empty_row(ui, text.empty_group, pal);
        } else {
            draw_profile_rows(ui, state, inventory, None, profiles, text, pal, out);
        }
    }
}

fn draw_group_header(
    ui: &mut egui::Ui,
    name: &str,
    collapsed: bool,
    pal: &Palette,
    trailing: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let response = egui::Frame::NONE
        .fill(pal.bg_panel)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.set_min_height(ROW_HEIGHT - 2.0);
            ui.horizontal(|ui| {
                let arrow = if collapsed { "▸" } else { "▾" };
                ui.label(RichText::new(arrow).color(pal.fg_dim));
                ui.label(RichText::new(name).strong().color(pal.fg));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
            });
        })
        .response
        .interact(egui::Sense::click());

    if response.hovered() {
        ui.painter().rect_stroke(
            response.rect,
            4.0,
            egui::Stroke::new(1.0_f32, pal.bg_highlight),
            egui::StrokeKind::Inside,
        );
    }
    response
}

#[allow(clippy::too_many_arguments)]
fn draw_profile_rows(
    ui: &mut egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    group_id: Option<&str>,
    profiles: &[&SshProfile],
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    for (visible_index, profile) in profiles.iter().enumerate() {
        let selected = state.selected_profile_id.as_deref() == Some(&profile.id);
        let fill = if selected {
            pal.selection
        } else {
            Color32::TRANSPARENT
        };
        let row = egui::Frame::NONE
            .fill(fill)
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(6, 2))
            .show(ui, |ui| {
                ui.set_min_height(ROW_HEIGHT);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&profile.name).color(pal.fg));
                        ui.label(
                            RichText::new(format!(
                                "{}@{}:{}",
                                profile.username, profile.host, profile.port
                            ))
                            .small()
                            .color(pal.fg_dim),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if small_button(ui, "×", text.delete, pal).clicked() {
                            state.dialog = Some(Dialog::DeleteProfile {
                                id: profile.id.clone(),
                                name: profile.name.clone(),
                            });
                        }
                        if small_button(ui, "✎", text.edit, pal).clicked() {
                            state.dialog = Some(Dialog::EditProfile(ProfileForm::edit(profile)));
                        }
                        if small_button(ui, "▶", text.connect, pal).clicked() {
                            out.actions.push(SshUiAction::ConnectProfile {
                                id: profile.id.clone(),
                            });
                        }
                    });
                });
            })
            .response
            .interact(if state.search.trim().is_empty() {
                egui::Sense::click_and_drag()
            } else {
                egui::Sense::click()
            });

        if row.clicked() {
            state.selected_profile_id = Some(profile.id.clone());
        }
        if row.double_clicked() {
            out.actions.push(SshUiAction::ConnectProfile {
                id: profile.id.clone(),
            });
        }
        if state.search.trim().is_empty() && row.drag_started() {
            state.dragged_profile_id = Some(profile.id.clone());
            state.drop_target = None;
        }
        register_row_drop_target(
            state,
            inventory,
            &row,
            group_id,
            visible_index,
            profiles.len(),
        );

        if state.dragged_profile_id.as_deref() == Some(&profile.id) && row.dragged() {
            ui.painter().rect_stroke(
                row.rect,
                4.0,
                egui::Stroke::new(1.0_f32, pal.accent),
                egui::StrokeKind::Inside,
            );
        }
    }
}

fn register_group_drop_target(
    state: &mut SshUiState,
    inventory: &SshInventory,
    response: &egui::Response,
    group_id: Option<&str>,
    displayed_len: usize,
) {
    if state.dragged_profile_id.is_none() || !response.hovered() {
        return;
    }
    let source_id = state.dragged_profile_id.as_deref().expect("已检查");
    state.drop_target = normalized_drop_target(inventory, source_id, group_id, displayed_len);
}

fn register_row_drop_target(
    state: &mut SshUiState,
    inventory: &SshInventory,
    response: &egui::Response,
    group_id: Option<&str>,
    visible_index: usize,
    displayed_len: usize,
) {
    if state.dragged_profile_id.is_none() || !response.hovered() {
        return;
    }
    let after = response
        .hover_pos()
        .is_some_and(|position| position.y >= response.rect.center().y);
    let boundary = (visible_index + usize::from(after)).min(displayed_len);
    let source_id = state.dragged_profile_id.as_deref().expect("已检查");
    state.drop_target = normalized_drop_target(inventory, source_id, group_id, boundary);
}

fn normalized_drop_target(
    inventory: &SshInventory,
    source_id: &str,
    target_group_id: Option<&str>,
    boundary_index: usize,
) -> Option<DropTarget> {
    let source = inventory.profile(source_id)?;
    let target_profiles = inventory.profiles_in_group(target_group_id);
    let boundary_index = boundary_index.min(target_profiles.len());
    let source_index = target_profiles
        .iter()
        .position(|profile| profile.id == source_id);
    let index = match source_index {
        Some(index) if index < boundary_index => boundary_index.saturating_sub(1),
        _ => boundary_index,
    };

    if source.group_id.as_deref() == target_group_id && source_index == Some(index) {
        return None;
    }
    Some(DropTarget {
        group_id: target_group_id.map(ToOwned::to_owned),
        index,
    })
}

fn finish_drop(
    ui: &egui::Ui,
    state: &mut SshUiState,
    inventory: &SshInventory,
    out: &mut SshUiOutput,
) {
    if state.dragged_profile_id.is_none() {
        return;
    }
    let cancelled = ui.input(|input| input.key_pressed(egui::Key::Escape));
    let released = ui.input(|input| input.pointer.any_released());
    if cancelled {
        state.cancel_drag();
    } else if released {
        let source_id = state.dragged_profile_id.take().expect("已检查");
        let target = state.drop_target.take();
        if inventory.profile(&source_id).is_some() {
            if let Some(target) = target {
                out.actions.push(SshUiAction::MoveProfile {
                    id: source_id,
                    target_group_id: target.group_id,
                    target_index: target.index,
                });
            }
        }
    }
}

fn draw_dialog(
    ctx: &egui::Context,
    state: &mut SshUiState,
    input: SshUiInput<'_>,
    pal: &Palette,
    out: &mut SshUiOutput,
) {
    let Some(mut dialog) = state.dialog.take() else {
        return;
    };
    let mut keep_open = true;
    let title = match &dialog {
        Dialog::CreateGroup { .. } => input.text.create_group_title,
        Dialog::RenameGroup { .. } => input.text.rename_group_title,
        Dialog::DeleteGroup { .. } => input.text.delete_group_title,
        Dialog::EditProfile(form) if form.editing_id.is_some() => input.text.edit_profile_title,
        Dialog::EditProfile(_) => input.text.create_profile_title,
        Dialog::DeleteProfile { .. } => input.text.delete_profile_title,
    };

    egui::Window::new(title)
        .id(egui::Id::new("ssh_ui_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(pal.bg_panel)
                .stroke(egui::Stroke::new(1.0_f32, pal.panel_outline)),
        )
        .show(ctx, |ui| match &mut dialog {
            Dialog::CreateGroup { name } => {
                keep_open = group_editor(ui, name, None, input.text, pal, out);
            }
            Dialog::RenameGroup { id, name } => {
                keep_open = group_editor(ui, name, Some(id), input.text, pal, out);
            }
            Dialog::DeleteGroup { id, name } => {
                ui.label(
                    RichText::new(format!("{}：{name}", input.text.delete_group_message))
                        .color(pal.fg),
                );
                keep_open = confirm_row(
                    ui,
                    input.text,
                    pal,
                    SshUiAction::DeleteGroup { id: id.clone() },
                    out,
                );
            }
            Dialog::EditProfile(form) => {
                keep_open = profile_editor(ui, form, input, pal, out);
            }
            Dialog::DeleteProfile { id, name } => {
                ui.label(
                    RichText::new(format!("{}：{name}", input.text.delete_profile_message))
                        .color(pal.fg),
                );
                keep_open = confirm_row(
                    ui,
                    input.text,
                    pal,
                    SshUiAction::DeleteProfile { id: id.clone() },
                    out,
                );
            }
        });

    if keep_open {
        state.dialog = Some(dialog);
    }
}

fn group_editor(
    ui: &mut egui::Ui,
    name: &mut String,
    editing_id: Option<&GroupId>,
    text: &SshUiText,
    pal: &Palette,
    out: &mut SshUiOutput,
) -> bool {
    ui.label(RichText::new(text.group_name).color(pal.fg));
    let response = ui.add(
        egui::TextEdit::singleline(name)
            .desired_width(320.0)
            .char_limit(50)
            .text_color(pal.fg)
            .background_color(pal.extreme_bg),
    );
    response.request_focus();
    let valid = !name.trim().is_empty() && name.trim().chars().count() <= 50;
    let submit_from_keyboard =
        valid && response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));

    let mut keep_open = true;
    ui.horizontal(|ui| {
        if ui.button(text.cancel).clicked() {
            keep_open = false;
        }
        let submit = ui
            .add_enabled(valid, egui::Button::new(text.save))
            .clicked()
            || submit_from_keyboard;
        if submit {
            let name = name.trim().to_owned();
            let action = match editing_id {
                Some(id) => SshUiAction::RenameGroup {
                    id: id.clone(),
                    name,
                },
                None => SshUiAction::CreateGroup { name },
            };
            out.actions.push(action);
            keep_open = false;
        }
    });
    keep_open
}

fn confirm_row(
    ui: &mut egui::Ui,
    text: &SshUiText,
    pal: &Palette,
    action: SshUiAction,
    out: &mut SshUiOutput,
) -> bool {
    let mut keep_open = true;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button(text.cancel).clicked() {
            keep_open = false;
        }
        if ui
            .add(
                egui::Button::new(RichText::new(text.confirm_delete).color(pal.accent_fg))
                    .fill(pal.error),
            )
            .clicked()
        {
            out.actions.push(action);
            keep_open = false;
        }
    });
    keep_open
}

fn profile_editor(
    ui: &mut egui::Ui,
    form: &mut ProfileForm,
    input: SshUiInput<'_>,
    pal: &Palette,
    out: &mut SshUiOutput,
) -> bool {
    egui::Grid::new("ssh_profile_form_grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            form_text_row(ui, input.text.profile_name, &mut form.name, pal);
            form_text_row(ui, input.text.host, &mut form.host, pal);
            form_number_row(ui, input.text.port, &mut form.port, "");
            form_text_row(ui, input.text.username, &mut form.username, pal);

            ui.label(input.text.auth_method);
            egui::ComboBox::from_id_salt("ssh_profile_auth_method")
                .selected_text(auth_method_text(form.auth_method, input.text))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut form.auth_method,
                        AuthMethod::Password,
                        input.text.auth_password,
                    );
                    ui.selectable_value(
                        &mut form.auth_method,
                        AuthMethod::PrivateKey,
                        input.text.auth_private_key,
                    );
                    ui.selectable_value(
                        &mut form.auth_method,
                        AuthMethod::Agent,
                        input.text.auth_agent,
                    );
                });
            ui.end_row();

            ui.label(input.text.group);
            egui::ComboBox::from_id_salt("ssh_profile_group")
                .selected_text(group_name(
                    input.inventory,
                    form.group_id.as_deref(),
                    input.text.ungrouped,
                ))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut form.group_id, None, input.text.ungrouped);
                    let mut groups: Vec<_> = input.inventory.groups().iter().collect();
                    groups.sort_by_key(|group| group.sort_order);
                    for group in groups {
                        ui.selectable_value(
                            &mut form.group_id,
                            Some(group.id.clone()),
                            &group.name,
                        );
                    }
                });
            ui.end_row();

            form_text_row(
                ui,
                input.text.initial_directory,
                &mut form.initial_directory,
                pal,
            );
            form_number_row(
                ui,
                input.text.connect_timeout,
                &mut form.connect_timeout_secs,
                input.text.seconds,
            );

            ui.label(input.text.keep_alive);
            ui.horizontal(|ui| {
                ui.checkbox(&mut form.keep_alive_enabled, "");
                if form.keep_alive_enabled {
                    ui.add(egui::DragValue::new(&mut form.keep_alive_secs).range(1..=86_400));
                    ui.label(input.text.seconds);
                } else {
                    ui.label(RichText::new(input.text.keep_alive_disabled).color(pal.fg_dim));
                }
            });
            ui.end_row();

            ui.label(input.text.monitor_enabled);
            ui.checkbox(&mut form.monitor_enabled, "");
            ui.end_row();
        });

    let mut keep_open = true;
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button(input.text.cancel).clicked() {
            keep_open = false;
        }
        let button_text = if form.editing_id.is_some() {
            input.text.save
        } else {
            input.text.create
        };
        if ui
            .add_enabled(form.valid(input.inventory), egui::Button::new(button_text))
            .clicked()
        {
            let draft = form.to_draft();
            let action = match &form.editing_id {
                Some(id) => SshUiAction::UpdateProfile {
                    id: id.clone(),
                    draft,
                },
                None => SshUiAction::CreateProfile { draft },
            };
            out.actions.push(action);
            keep_open = false;
        }
    });
    keep_open
}

fn form_text_row(ui: &mut egui::Ui, label: &str, value: &mut String, pal: &Palette) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(260.0)
            .text_color(pal.fg)
            .background_color(pal.extreme_bg),
    );
    ui.end_row();
}

fn form_number_row<T>(ui: &mut egui::Ui, label: &str, value: &mut T, suffix: &str)
where
    T: egui::emath::Numeric,
{
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(egui::DragValue::new(value).range(1..=86_400));
        if !suffix.is_empty() {
            ui.label(suffix);
        }
    });
    ui.end_row();
}

fn auth_method_text(method: AuthMethod, text: &SshUiText) -> &'static str {
    match method {
        AuthMethod::Password => text.auth_password,
        AuthMethod::PrivateKey => text.auth_private_key,
        AuthMethod::Agent => text.auth_agent,
    }
}

fn group_name<'a>(
    inventory: &'a SshInventory,
    group_id: Option<&str>,
    ungrouped: &'a str,
) -> &'a str {
    group_id
        .and_then(|id| inventory.group(id))
        .map_or(ungrouped, |group| group.name.as_str())
}

fn filtered_profiles<'a>(
    profiles: &[&'a SshProfile],
    group: &SshGroup,
    query: &str,
) -> Vec<&'a SshProfile> {
    if query.is_empty() || normalized_query(&group.name).contains(query) {
        profiles.to_vec()
    } else {
        profiles
            .iter()
            .copied()
            .filter(|profile| profile_matches(profile, query))
            .collect()
    }
}

fn profile_matches(profile: &SshProfile, query: &str) -> bool {
    query.is_empty()
        || normalized_query(&profile.name).contains(query)
        || normalized_query(&profile.host).contains(query)
        || normalized_query(&profile.username).contains(query)
}

fn normalized_query(value: &str) -> String {
    value.trim().to_lowercase()
}

fn optional_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn empty_row(ui: &mut egui::Ui, message: &str, pal: &Palette) {
    ui.add_sized(
        [ui.available_width(), ROW_HEIGHT],
        egui::Label::new(RichText::new(message).small().color(pal.fg_dim)),
    );
}

fn small_button(ui: &mut egui::Ui, glyph: &str, tooltip: &str, pal: &Palette) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(glyph).color(pal.fg))
            .fill(Color32::TRANSPARENT)
            .corner_radius(3.0)
            .min_size(egui::vec2(24.0, 24.0)),
    )
    .on_hover_text(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> SshInventory {
        let mut inventory = SshInventory::default();
        let group = inventory.create_group("生产").unwrap();
        for name in ["A", "B", "C"] {
            inventory
                .create_profile(NewSshProfile {
                    name: name.into(),
                    host: format!("{name}.example.com"),
                    username: "root".into(),
                    group_id: Some(group.clone()),
                    ..NewSshProfile::default()
                })
                .unwrap();
        }
        inventory
    }

    #[test]
    fn 同组向下拖动会换算为移除源行后的索引() {
        let inventory = inventory();
        let group_id = inventory.groups()[0].id.as_str();
        let profiles = inventory.profiles_in_group(Some(group_id));
        let source_id = &profiles[0].id;

        let target = normalized_drop_target(&inventory, source_id, Some(group_id), 2).unwrap();
        assert_eq!(target.index, 1);
    }

    #[test]
    fn 原位置落点不产生动作() {
        let inventory = inventory();
        let group_id = inventory.groups()[0].id.as_str();
        let profiles = inventory.profiles_in_group(Some(group_id));
        let source_id = &profiles[1].id;

        assert_eq!(
            normalized_drop_target(&inventory, source_id, Some(group_id), 1),
            None
        );
        assert_eq!(
            normalized_drop_target(&inventory, source_id, Some(group_id), 2),
            None
        );
    }

    #[test]
    fn 搜索与上锁都会清理拖放和弹窗() {
        let mut state = SshUiState {
            dialog: Some(Dialog::CreateGroup {
                name: "临时".into(),
            }),
            dragged_profile_id: Some("ssh_example".into()),
            drop_target: Some(DropTarget {
                group_id: None,
                index: 0,
            }),
            ..SshUiState::default()
        };

        state.set_search("prod");
        assert!(state.dragged_profile_id.is_none());
        assert!(state.drop_target.is_none());
        assert!(state.dialog.is_some());

        state.close_for_app_lock();
        assert!(state.dialog.is_none());
        assert!(state.dragged_profile_id.is_none());
        assert!(state.drop_target.is_none());
    }

    #[test]
    fn 表单只生成可同步的连接元数据() {
        let form = ProfileForm::create();
        let draft = form.to_draft();
        assert_eq!(draft.port, 22);
        assert_eq!(draft.auth_method, AuthMethod::Password);
        assert_eq!(draft.keep_alive_secs, Some(30));
        assert!(draft.trusted_host_key.is_none());
    }
}
