//! Read-only Linux file browser for an active SSH session.
//!
//! Linux paths remain UTF-8 strings with `/` separators all the way through
//! the UI. They are never converted to a Windows `PathBuf`.

use egui::{Color32, RichText};
use lumen_ssh::DirectoryEntryKind;

use super::theme::Palette;
use super::SshRuntimeAction;

#[derive(Default)]
pub struct Output {
    pub action: Option<SshRuntimeAction>,
    pub panel_width: Option<f32>,
    pub panel_rect: Option<egui::Rect>,
    pub hovered: bool,
    pub copy_text: Option<String>,
}

#[derive(Clone, Copy)]
enum ToolbarIcon {
    Hidden,
    Refresh,
}

pub fn show(
    root: &mut egui::Ui,
    tree: Option<&crate::ssh_runtime::SshFileTreeView>,
    visible: bool,
    pal: &Palette,
    width: f32,
) -> Output {
    if !visible {
        return Output::default();
    }

    let mut output = Output::default();
    let panel = egui::Panel::left("lumen_ssh_filetree")
        .default_size(width)
        .size_range(crate::settings::FILETREE_WIDTH_MIN..=crate::settings::FILETREE_WIDTH_MAX)
        .resizable(true)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(pal.filetree_fill)
                .inner_margin(egui::Margin::symmetric(6, 8)),
        )
        .show_inside(root, |ui| {
            draw_contents(ui, tree, pal, &mut output);
        });
    output.panel_width = Some(panel.response.rect.width());
    output.panel_rect = Some(panel.response.rect);
    output.hovered = panel.response.contains_pointer();
    output
}

fn draw_contents(
    ui: &mut egui::Ui,
    tree: Option<&crate::ssh_runtime::SshFileTreeView>,
    pal: &Palette,
    output: &mut Output,
) {
    let strings = crate::i18n::strings();
    // 与本地文件树使用同一套工具条规格：6px 面板横边距、24px
    // 图标热区、12px 根目录名。SSH 保留「刷新 / 显示隐藏项」两个
    // 远端专属动作，但不再另画一行完整路径，避免内容区起点错位。
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let refresh = toolbar_icon_button(
                ui,
                ToolbarIcon::Refresh,
                false,
                strings.remote_refresh_dir_tip,
                pal,
            );
            if refresh.clicked() {
                if let Some(tree) = tree {
                    output.action = Some(SshRuntimeAction::RefreshFileTree {
                        session_id: tree.session_id,
                    });
                }
            }
            let show_hidden = tree.is_some_and(|tree| tree.show_hidden);
            let hidden = toolbar_icon_button(
                ui,
                ToolbarIcon::Hidden,
                show_hidden,
                strings.remote_show_hidden,
                pal,
            );
            if hidden.clicked() {
                if let Some(tree) = tree {
                    output.action = Some(SshRuntimeAction::ToggleHiddenFiles {
                        session_id: tree.session_id,
                    });
                }
            }

            let title = tree.map_or(strings.filetree_root_placeholder, |tree| {
                linux_basename(&tree.root)
            });
            let root_response = ui.add(
                egui::Label::new(RichText::new(title).size(12.0).color(pal.fg))
                    .truncate()
                    .selectable(false),
            );
            if let Some(tree) = tree {
                root_response.on_hover_text(&tree.root).context_menu(|ui| {
                    if ui.button(strings.filetree_menu_copy_abs).clicked() {
                        output.copy_text = Some(tree.root.clone());
                        ui.close();
                    }
                });
            }
        });
    });

    let Some(tree) = tree else {
        ui.add_space(8.0);
        ui.label(
            RichText::new(strings.filetree_waiting_cwd)
                .size(11.0)
                .color(pal.fg_dim),
        );
        return;
    };

    ui.add_space(2.0);
    let selected_id = ui.make_persistent_id(("ssh_filetree_selected", tree.session_id));
    let mut selected_path = ui.data(|data| data.get_temp::<String>(selected_id));
    egui::ScrollArea::both()
        .id_salt(("ssh_filetree_scroll", tree.session_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 同本地/远程文件树：三角与名字仅留 2px，取消按钮的
            // 默认横向 padding，长文件名由行内 truncate 收口。
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().button_padding.x = 0.0;

            if tree.rows.is_empty() && tree.loading {
                placeholder_row(ui, strings.filetree_loading, pal);
            }

            for row in &tree.rows {
                ui.push_id(row.path.as_str(), |ui| {
                    draw_row(ui, tree, row, pal, &mut selected_path, output);
                });
            }

            if tree.truncated {
                placeholder_row(ui, strings.filetree_truncated, pal);
            }
            if let Some(error) = &tree.error {
                ui.add(
                    egui::Label::new(
                        RichText::new(strings.filetree_unreadable)
                            .size(11.0)
                            .color(pal.fg_dim)
                            .italics(),
                    )
                    .wrap(),
                )
                .on_hover_text(error);
            }
        });
    if let Some(selected_path) = selected_path {
        ui.data_mut(|data| data.insert_temp(selected_id, selected_path));
    }
}

fn draw_row(
    ui: &mut egui::Ui,
    tree: &crate::ssh_runtime::SshFileTreeView,
    row: &crate::ssh_runtime::SshFileTreeRow,
    pal: &Palette,
    selected_path: &mut Option<String>,
    output: &mut Output,
) {
    // 本地 ltreeview 的行高跟随 egui interact_size；不要另设 25px
    // 大行高，否则同一应用内两棵文件树的密度明显不同。
    let row_height = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_height),
        egui::Sense::click(),
    );
    let selected = selected_path.as_deref() == Some(row.path.as_str());
    if selected {
        ui.painter()
            .rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 2.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }

    #[allow(clippy::cast_precision_loss)]
    let indent = row.depth as f32 * 12.0;
    let mut x = rect.left() + indent;
    if row.kind == DirectoryEntryKind::Directory {
        let triangle_rect = egui::Rect::from_center_size(
            egui::pos2(x + 4.5, rect.center().y),
            egui::vec2(9.0, 16.0),
        );
        paint_triangle(
            ui.painter(),
            triangle_rect,
            row.expanded,
            if response.hovered() {
                pal.fg
            } else {
                pal.fg_dim
            },
        );
        x += 11.0;
    } else {
        // ltreeview 为叶节点保留 closer 槽，使同层文件名与目录名对齐。
        x += 11.0;
    }

    let right_padding = if row.loading { 20.0 } else { 3.0 };
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2(rect.right() - right_padding, rect.bottom()),
    );
    ui.put(
        text_rect,
        egui::Label::new(RichText::new(&row.name).color(pal.fg))
            .truncate()
            .selectable(false),
    );
    if row.loading {
        let spinner_rect = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 8.0, rect.center().y),
            egui::vec2(14.0, 14.0),
        );
        ui.put(spinner_rect, egui::Spinner::new().size(12.0));
    }

    if response.clicked() || response.secondary_clicked() {
        *selected_path = Some(row.path.clone());
    }
    if response.clicked() && row.kind == DirectoryEntryKind::Directory {
        output.action = Some(SshRuntimeAction::ToggleDirectory {
            session_id: tree.session_id,
            path: row.path.clone(),
        });
    }
    response.on_hover_text(&row.path).context_menu(|ui| {
        if ui
            .button(crate::i18n::strings().filetree_menu_copy_abs)
            .clicked()
        {
            output.copy_text = Some(row.path.clone());
            ui.close();
        }
    });
}

fn linux_basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path)
}

fn placeholder_row(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    ui.label(RichText::new(text).size(11.0).color(pal.fg_dim).italics());
}

fn toolbar_icon_button(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    active: bool,
    tooltip: &str,
    pal: &Palette,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(4), pal.bg_highlight);
    }
    let color = if active {
        pal.accent
    } else if response.hovered() {
        pal.fg
    } else {
        pal.fg_dim
    };
    match icon {
        ToolbarIcon::Hidden => paint_eye(ui.painter(), rect, color, active),
        ToolbarIcon::Refresh => paint_refresh(ui.painter(), rect, color),
    }
    response.on_hover_text(tooltip)
}

fn paint_triangle(painter: &egui::Painter, rect: egui::Rect, expanded: bool, color: Color32) {
    let center = rect.center();
    let radius = 3.5_f32;
    let points = if expanded {
        vec![
            egui::pos2(center.x - radius, center.y - radius * 0.5),
            egui::pos2(center.x + radius, center.y - radius * 0.5),
            egui::pos2(center.x, center.y + radius * 0.8),
        ]
    } else {
        vec![
            egui::pos2(center.x - radius * 0.5, center.y - radius),
            egui::pos2(center.x - radius * 0.5, center.y + radius),
            egui::pos2(center.x + radius * 0.8, center.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(
        points,
        color,
        egui::Stroke::NONE,
    ));
}

fn paint_eye(painter: &egui::Painter, rect: egui::Rect, color: Color32, active: bool) {
    let center = rect.center();
    let stroke = egui::Stroke::new(1.15_f32, color);
    painter.add(egui::Shape::line(
        vec![
            center + egui::vec2(-7.0, 0.0),
            center + egui::vec2(-3.5, -3.5),
            center + egui::vec2(0.0, -4.5),
            center + egui::vec2(3.5, -3.5),
            center + egui::vec2(7.0, 0.0),
            center + egui::vec2(3.5, 3.5),
            center + egui::vec2(0.0, 4.5),
            center + egui::vec2(-3.5, 3.5),
            center + egui::vec2(-7.0, 0.0),
        ],
        stroke,
    ));
    if active {
        painter.circle_filled(center, 2.0, color);
    } else {
        painter.circle_stroke(center, 2.0, stroke);
    }
}

fn paint_refresh(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    let stroke = egui::Stroke::new(1.2_f32, color);
    let radius = 6.0_f32;
    #[allow(clippy::cast_precision_loss)]
    let points = (0..=18)
        .map(|step| {
            let angle = -2.7_f32 + 4.7_f32 * step as f32 / 18.0_f32;
            center + egui::vec2(angle.cos(), angle.sin()) * radius
        })
        .collect::<Vec<_>>();
    painter.add(egui::Shape::line(points, stroke));
    let tip = center + egui::vec2(2.5, 5.4);
    painter.line_segment([tip, tip + egui::vec2(-4.0, -0.2)], stroke);
    painter.line_segment([tip, tip + egui::vec2(-0.8, -3.8)], stroke);
}
