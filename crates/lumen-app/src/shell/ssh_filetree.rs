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
        .size_range(
            crate::settings::FILETREE_WIDTH_MIN..=crate::settings::FILETREE_WIDTH_MAX,
        )
        .resizable(true)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(pal.filetree_fill)
                .inner_margin(egui::Margin::symmetric(7, 9)),
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
    let (header_rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    let mut header_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(header_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    header_ui.label(
        RichText::new(strings.filetree_root_placeholder)
            .size(12.0)
            .color(pal.fg_dim),
    );
    header_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                    profile_id: tree.profile_id.clone(),
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
                    profile_id: tree.profile_id.clone(),
                });
            }
        }
    });

    let Some(tree) = tree else {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(strings.filetree_waiting_cwd)
                    .small()
                    .color(pal.fg_dim),
            );
        });
        return;
    };

    let root_response = ui.add(
        egui::Label::new(
            RichText::new(&tree.root)
                .monospace()
                .small()
                .color(pal.fg),
        )
        .truncate(),
    );
    root_response
        .on_hover_text(&tree.root)
        .context_menu(|ui| {
            if ui.button(strings.filetree_menu_copy_abs).clicked() {
                output.copy_text = Some(tree.root.clone());
                ui.close();
            }
        });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(3.0);

    egui::ScrollArea::vertical()
        .id_salt(("ssh_filetree_scroll", tree.profile_id.as_str()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if tree.rows.is_empty() && tree.loading {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(13.0));
                    ui.label(
                        RichText::new(strings.filetree_loading)
                            .small()
                            .color(pal.fg_dim),
                    );
                });
            }

            for row in &tree.rows {
                draw_row(ui, tree, row, pal, output);
            }

            if tree.truncated {
                ui.add_space(5.0);
                ui.label(
                    RichText::new(strings.filetree_truncated)
                        .small()
                        .color(pal.warn),
                );
            }
            if let Some(error) = &tree.error {
                ui.add_space(5.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(strings.filetree_unreadable)
                            .small()
                            .color(pal.warn),
                    )
                    .wrap(),
                )
                .on_hover_text(error);
            }
        });
}

fn draw_row(
    ui: &mut egui::Ui,
    tree: &crate::ssh_runtime::SshFileTreeView,
    row: &crate::ssh_runtime::SshFileTreeRow,
    pal: &Palette,
    output: &mut Output,
) {
    const ROW_HEIGHT: f32 = 25.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    if response.hovered() {
        ui.painter().rect_filled(rect, 2.0, pal.bg_highlight);
    }

    #[allow(clippy::cast_precision_loss)]
    let indent = row.depth as f32 * 13.0;
    let mut x = rect.left() + 2.0 + indent;
    if row.kind == DirectoryEntryKind::Directory {
        let chevron_rect = egui::Rect::from_center_size(
            egui::pos2(x + 6.0, rect.center().y),
            egui::vec2(12.0, 16.0),
        );
        paint_chevron(ui.painter(), chevron_rect, row.expanded, pal.fg_dim);
        x += 14.0;
    } else {
        x += 14.0;
    }

    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(x + 7.0, rect.center().y),
        egui::vec2(15.0, 15.0),
    );
    paint_entry_icon(ui.painter(), icon_rect, row.kind, pal.info);
    x += 19.0;

    let right_padding = if row.loading { 20.0 } else { 3.0 };
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2(rect.right() - right_padding, rect.bottom()),
    );
    ui.put(
        text_rect,
        egui::Label::new(
            RichText::new(&row.name)
                .size(12.0)
                .color(if response.hovered() {
                    pal.fg
                } else {
                    pal.fg_dim
                }),
        )
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

    if response.clicked() && row.kind == DirectoryEntryKind::Directory {
        output.action = Some(SshRuntimeAction::ToggleDirectory {
            profile_id: tree.profile_id.clone(),
            path: row.path.clone(),
        });
    }
    response
        .on_hover_text(&row.path)
        .context_menu(|ui| {
            if ui.button(crate::i18n::strings().filetree_menu_copy_abs).clicked() {
                output.copy_text = Some(row.path.clone());
                ui.close();
            }
        });
}

fn toolbar_icon_button(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    active: bool,
    tooltip: &str,
    pal: &Palette,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(24.0, 22.0), egui::Sense::click());
    if response.hovered() || active {
        ui.painter().rect_filled(
            rect,
            3.0,
            if active {
                pal.selection
            } else {
                pal.bg_highlight
            },
        );
    }
    let color = if response.hovered() || active {
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

fn paint_chevron(
    painter: &egui::Painter,
    rect: egui::Rect,
    expanded: bool,
    color: Color32,
) {
    let center = rect.center();
    let stroke = egui::Stroke::new(1.2_f32, color);
    let points = if expanded {
        [
            center + egui::vec2(-4.0, -2.0),
            center + egui::vec2(0.0, 2.0),
            center + egui::vec2(4.0, -2.0),
        ]
    } else {
        [
            center + egui::vec2(-2.0, -4.0),
            center + egui::vec2(2.0, 0.0),
            center + egui::vec2(-2.0, 4.0),
        ]
    };
    painter.line_segment([points[0], points[1]], stroke);
    painter.line_segment([points[1], points[2]], stroke);
}

fn paint_entry_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    kind: DirectoryEntryKind,
    color: Color32,
) {
    let stroke = egui::Stroke::new(1.1_f32, color);
    if kind == DirectoryEntryKind::Directory {
        let left = rect.left() + 1.0;
        let right = rect.right() - 1.0;
        let top = rect.top() + 3.0;
        let bottom = rect.bottom() - 2.0;
        painter.add(egui::Shape::line(
            vec![
                egui::pos2(left, bottom),
                egui::pos2(left, top + 2.0),
                egui::pos2(left + 5.0, top + 2.0),
                egui::pos2(left + 7.0, top + 4.0),
                egui::pos2(right, top + 4.0),
                egui::pos2(right, bottom),
                egui::pos2(left, bottom),
            ],
            stroke,
        ));
    } else {
        let body = rect.shrink2(egui::vec2(2.5, 1.5));
        painter.rect_stroke(body, 1.0, stroke, egui::StrokeKind::Middle);
        painter.line_segment(
            [
                egui::pos2(body.left() + 3.0, body.center().y),
                egui::pos2(body.right() - 3.0, body.center().y),
            ],
            stroke,
        );
    }
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
