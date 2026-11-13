//! Shared chrome: menu rows and text fields.

use std::path::{Path, PathBuf};

use egui::{
    Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Id, Margin, Rect, Response, Sense,
    Shadow, Stroke, StrokeKind, Ui, WidgetInfo, pos2, vec2,
};

use crate::fonts;
use crate::icons::{self, Icon};
use crate::theme::{UiPalette, mix};

pub const MENU_ITEM_HEIGHT: f32 = 32.0;

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::UI_MEDIUM.into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::UI_SEMIBOLD.into()))
}

pub fn menu_frame(palette: &UiPalette) -> Frame {
    Frame::new()
        .fill(palette.raised)
        .stroke(Stroke::new(1.0, palette.border))
        .corner_radius(9)
        .inner_margin(Margin::same(6))
        .shadow(Shadow {
            offset: [0, 10],
            blur: 30,
            spread: 0,
            color: Color32::from_black_alpha(90),
        })
}

pub fn dialog_frame(palette: &UiPalette) -> Frame {
    menu_frame(palette).corner_radius(12).inner_margin(Margin::same(22))
}

/// A menu's fixed content width: egui's would follow its widest row.
pub fn menu_width(ui: &mut Ui, width: f32) {
    ui.set_min_width(width);
    ui.set_max_width(width);
    ui.spacing_mut().item_spacing.y = 0.0;
}

pub fn separator(ui: &mut Ui, palette: &UiPalette) {
    let width = ui.available_width().max(ui.min_size().x);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 11.0), Sense::hover());
    let y = ui.painter().round_to_pixel_center(rect.center().y);
    ui.painter().hline(rect.min.x + 4.0..=rect.max.x - 4.0, y, Stroke::new(1.0, palette.border));
}

pub struct Item<'a> {
    label: &'a str,
    icon: Option<Icon>,
    keys: &'a str,
    enabled: bool,
    checked: bool,
    submenu: bool,
    danger: bool,
}

impl<'a> Item<'a> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            icon: None,
            keys: "",
            enabled: true,
            checked: false,
            submenu: false,
            danger: false,
        }
    }

    /// An action that destroys something: its label and icon take the error colour.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn keys(mut self, keys: &'a str) -> Self {
        self.keys = keys;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    pub fn submenu(mut self) -> Self {
        self.submenu = true;
        self
    }

    /// Icon and label on the left; shortcut, tick or chevron on the right.
    pub fn show(self, ui: &mut Ui, palette: &UiPalette) -> Response {
        let width = ui.available_width().max(ui.min_size().x);
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(vec2(width, MENU_ITEM_HEIGHT), sense);
        response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, self.enabled, self.label));
        let hot = self.enabled && (response.hovered() || response.has_focus());
        let painter = ui.painter();
        if hot {
            painter.rect_filled(rect, 6.0, palette.hover);
        }
        let fade = |color: Color32| if self.enabled { color } else { color.gamma_multiply(0.45) };
        let foreground = fade(if self.danger { palette.error } else { palette.text });
        let y = rect.center().y;
        let mut x = rect.min.x + 10.0;
        if let Some(icon) = self.icon {
            let color = fade(match (self.danger, hot) {
                (true, _) => palette.error,
                (false, true) => palette.text,
                (false, false) => palette.text_dim,
            });
            icons::paint(painter, icon, Rect::from_min_size(pos2(x, y - 8.0), vec2(16.0, 16.0)), color);
            x += 26.0;
        }
        painter.text(pos2(x, y), Align2::LEFT_CENTER, self.label, FontId::proportional(13.0), foreground);
        let right = rect.max.x - 10.0;
        if self.submenu {
            icons::paint(painter, Icon::ChevronRight, icons::square(pos2(right - 6.0, y), 13.0), fade(palette.text_dim));
        } else if self.checked {
            icons::paint(painter, Icon::MenuCheck, icons::square(pos2(right - 7.0, y), 14.0), palette.accent);
        } else if !self.keys.is_empty() {
            painter.text(pos2(right, y), Align2::RIGHT_CENTER, self.keys, FontId::proportional(12.0), fade(palette.text_dim));
        }
        if self.enabled {
            response.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            response
        }
    }
}

/// A single-line text field filling `rect`, its text vertically centred.
#[allow(clippy::too_many_arguments)]
pub fn text_field(
    ui: &mut Ui,
    palette: &UiPalette,
    id: Id,
    rect: Rect,
    text: &mut String,
    hint: &str,
    color: Color32,
    label: &str,
) -> Response {
    field(ui, palette, id, rect, text, hint, color, label, FontId::proportional(13.0), false)
}

/// A field for code or patterns: monospace, with a red border while `invalid`.
#[allow(clippy::too_many_arguments)]
pub fn code_field(
    ui: &mut Ui,
    palette: &UiPalette,
    id: Id,
    rect: Rect,
    text: &mut String,
    hint: &str,
    color: Color32,
    label: &str,
    invalid: bool,
) -> Response {
    field(ui, palette, id, rect, text, hint, color, label, FontId::monospace(12.5), invalid)
}

#[allow(clippy::too_many_arguments)]
fn field(
    ui: &mut Ui,
    palette: &UiPalette,
    id: Id,
    rect: Rect,
    text: &mut String,
    hint: &str,
    color: Color32,
    label: &str,
    font: FontId,
    invalid: bool,
) -> Response {
    let focused = ui.memory(|memory| memory.has_focus(id));
    let hovered = ui.rect_contains_pointer(rect);
    let border = if invalid {
        palette.error
    } else if focused {
        palette.accent
    } else if hovered {
        mix(palette.border, palette.text_dim, 0.5)
    } else {
        palette.border
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(6),
        palette.input,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    let inner = rect.shrink2(vec2(11.0, 0.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    child.spacing_mut().interact_size.y = 0.0;
    let response = child.add(
        egui::TextEdit::singleline(text)
            .id(id)
            .font(font)
            .text_color(color)
            .hint_text(hint)
            .frame(Frame::NONE)
            .margin(Margin::ZERO)
            .desired_width(inner.width()),
    );
    ui.ctx().accesskit_node_builder(response.id, |builder| {
        builder.set_label(label);
    });
    response
}

/// A square icon button: quiet until hovered.
pub fn icon_button(
    ui: &mut Ui,
    palette: &UiPalette,
    rect: Rect,
    id: Id,
    icon: Icon,
    tip: &str,
    color: Color32,
) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, tip));
    let hovered = response.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 6.0, palette.hover);
    }
    let shown = if hovered { palette.text } else { color };
    icons::paint(ui.painter(), icon, icons::square(rect.center(), 15.0), shown);
    response.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// As much of the end of `text` as fits in `room`: the end of a path is what tells paths apart.
fn elide_start(ui: &Ui, text: &str, font: &FontId, room: f32) -> String {
    let width = |candidate: &str| {
        ui.fonts_mut(|fonts| fonts.layout_no_wrap(candidate.to_owned(), font.clone(), Color32::WHITE))
            .size()
            .x
    };
    if room <= 0.0 {
        return String::new();
    }
    if width(text) <= room {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut low, mut high) = (0, chars.len());
    while low < high {
        let keep = (low + high).div_ceil(2);
        let candidate: String = std::iter::once('…').chain(chars[chars.len() - keep..].iter().copied()).collect();
        if width(&candidate) <= room {
            low = keep;
        } else {
            high = keep - 1;
        }
    }
    std::iter::once('…').chain(chars[chars.len() - low..].iter().copied()).collect()
}

pub enum PathAction {
    None,
    Commit(PathBuf),
    Browse,
    Save,
}

/// How much a header badge asks for attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Warning,
    Error,
}

/// A short word after the name of a side, explained by its tooltip.
#[derive(Clone, Debug)]
pub struct Badge {
    pub text: String,
    pub tip: String,
    pub tone: Tone,
}

impl Badge {
    pub fn new(text: impl Into<String>, tip: impl Into<String>) -> Self {
        Self { text: text.into(), tip: tip.into(), tone: Tone::Neutral }
    }

    pub fn warning(mut self) -> Self {
        self.tone = Tone::Warning;
        self
    }

    pub fn error(mut self) -> Self {
        self.tone = Tone::Error;
        self
    }
}

pub struct PathBar<'a> {
    pub path: &'a Path,
    pub input: &'a mut String,
    pub icon: Icon,
    pub dirty: bool,
    pub label: &'a str,
    pub browse_tip: &'a str,
    /// Shown instead of the name and folder of `path`, which may be a temporary file.
    pub title: Option<&'a str>,
    /// A small "lecture seule" badge follows the name.
    pub read_only: bool,
    pub badges: &'a [Badge],
    /// The path cannot be changed here: no browsing, no editing it in place.
    pub fixed: bool,
}

/// The header of a pane: what is compared there, and what can be done with it. A click on the
/// path makes it editable.
pub fn path_bar(ui: &mut Ui, palette: &UiPalette, id: Id, rect: Rect, bar: PathBar<'_>) -> PathAction {
    let radius = CornerRadius { nw: 9, ne: 9, sw: 0, se: 0 };
    ui.painter().rect_filled(rect, radius, palette.raised);
    let rule = ui.painter().round_to_pixel_center(rect.bottom() - 0.5);
    ui.painter().hline(rect.x_range(), rule, Stroke::new(1.0, palette.border));

    let mut action = PathAction::None;
    const BUTTON: f32 = 28.0;
    let button_at = |slot: f32| {
        Rect::from_center_size(
            pos2(rect.right() - 8.0 - BUTTON / 2.0 - slot * (BUTTON + 2.0), rect.center().y),
            vec2(BUTTON, BUTTON),
        )
    };
    let mut slot = 0.0;
    if !bar.fixed {
        if icon_button(ui, palette, button_at(slot), id.with("browse"), Icon::FolderOpen, bar.browse_tip, palette.text_dim).clicked() {
            action = PathAction::Browse;
        }
        slot += 1.0;
    }
    if icon_button(ui, palette, button_at(slot), id.with("copy"), Icon::Copy, "Copier le chemin", palette.text_dim).clicked() {
        ui.ctx().copy_text(bar.path.display().to_string());
    }
    slot += 1.0;
    if bar.dirty {
        if icon_button(ui, palette, button_at(slot), id.with("save"), Icon::Save, "Enregistrer ce côté", palette.warning).clicked() {
            action = PathAction::Save;
        }
        slot += 1.0;
    }
    let text_right = button_at(slot - 1.0).left() - 8.0;
    let area = Rect::from_min_max(pos2(rect.left() + 8.0, rect.top()), pos2(text_right, rect.bottom() - 1.0));

    let editing_id = id.with("editing");
    let editing = ui.data(|data| data.get_temp::<bool>(editing_id)).unwrap_or(false);
    if editing {
        let field_id = id.with("field");
        let field = Rect::from_min_max(pos2(area.left(), rect.center().y - 15.0), pos2(area.right(), rect.center().y + 15.0));
        let response = text_field(ui, palette, field_id, field, bar.input, "", palette.text, bar.label);
        if ui.data_mut(|data| data.remove_temp::<bool>(id.with("just-opened"))).is_some() {
            response.request_focus();
        } else if response.lost_focus() {
            ui.data_mut(|data| data.insert_temp(editing_id, false));
            let entered = ui.input(|input| input.key_pressed(egui::Key::Enter));
            if entered {
                action = PathAction::Commit(PathBuf::from(bar.input.trim()));
            } else {
                *bar.input = bar.path.display().to_string();
            }
        }
        return action;
    }

    if bar.fixed {
        let response = ui.interact(area, id.with("path"), Sense::hover());
        response.widget_info(|| WidgetInfo::labeled(egui::Role::Label, true, bar.label));
        let _ = response.on_hover_text(bar.path.display().to_string());
    } else {
        let response = ui.interact(area, id.with("path"), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, bar.label));
        if response.hovered() {
            ui.painter().rect_filled(area.shrink2(vec2(0.0, 7.0)), 6.0, palette.hover);
        }
        if response.clicked() {
            *bar.input = bar.path.display().to_string();
            ui.data_mut(|data| {
                data.insert_temp(editing_id, true);
                data.insert_temp(id.with("just-opened"), true);
            });
        }
        let _ = response.on_hover_cursor(egui::CursorIcon::Text).on_hover_text("Modifier le chemin");
    }

    let y = rect.center().y;
    let painter = ui.painter().with_clip_rect(area);
    let mut x = area.left() + 8.0;
    icons::paint(&painter, bar.icon, icons::square(pos2(x + 8.0, y), 16.0), palette.accent);
    x += 25.0;
    let name = match bar.title {
        Some(title) => title.to_owned(),
        None => bar.path.file_name().map_or_else(|| bar.path.display().to_string(), |n| n.to_string_lossy().into_owned()),
    };
    let name_font = medium(13.0);
    let name_width = painter.text(pos2(x, y), Align2::LEFT_CENTER, &name, name_font, palette.text).width();
    x += name_width + 6.0;
    if bar.dirty {
        painter.circle_filled(pos2(x + 3.0, y), 3.0, palette.warning);
        x += 14.0;
    } else {
        x += 4.0;
    }
    if bar.read_only {
        x += read_only_badge(&painter, palette, pos2(x, y)) + 8.0;
    }
    for (index, badge) in bar.badges.iter().enumerate() {
        let rect = paint_badge(&painter, palette, pos2(x, y), badge);
        if rect.left() < area.right() {
            let hover = ui.interact(rect.intersect(area), id.with(("badge", index)), Sense::hover());
            hover.widget_info(|| WidgetInfo::labeled(egui::Role::Label, true, &badge.text));
            hover.on_hover_text(&badge.tip);
        }
        x = rect.right() + 8.0;
    }
    // A label stands for the whole path: the folder of a temporary file says nothing.
    if let Some(parent) = bar.path.parent().filter(|_| bar.title.is_none()) {
        let font = FontId::proportional(12.0);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let shown = home
            .as_ref()
            .and_then(|home| parent.strip_prefix(home).ok())
            .map_or_else(|| parent.display().to_string(), |rest| format!("~/{}", rest.display()));
        let shown = elide_start(ui, &shown, &font, area.right() - x - 8.0);
        painter.text(pos2(x, y), Align2::LEFT_CENTER, shown, font, palette.text_dim);
    }
    action
}

/// A pill with a lock, starting at `left` and centred on its height. Returns its width.
fn read_only_badge(painter: &egui::Painter, palette: &UiPalette, left: egui::Pos2) -> f32 {
    let font = medium(11.0);
    let galley = painter.layout_no_wrap("lecture seule".to_owned(), font, palette.text_dim);
    let width = (galley.size().x + 32.0).round();
    let rect = Rect::from_min_size(pos2(left.x, left.y - 10.0), vec2(width, 20.0));
    painter.rect(rect, 10.0, mix(palette.raised, palette.hover, 0.6), Stroke::new(1.0, palette.border), StrokeKind::Inside);
    icons::paint(painter, Icon::Lock, icons::square(pos2(rect.left() + 13.0, rect.center().y), 11.0), palette.text_dim);
    painter.galley(pos2(rect.left() + 22.0, rect.center().y - galley.size().y / 2.0), galley, palette.text_dim);
    width
}

/// A pill holding a word, tinted by its tone. Returns where it was painted.
fn paint_badge(painter: &egui::Painter, palette: &UiPalette, left: egui::Pos2, badge: &Badge) -> Rect {
    let color = match badge.tone {
        Tone::Neutral => palette.accent,
        Tone::Warning => palette.warning,
        Tone::Error => palette.error,
    };
    let galley = painter.layout_no_wrap(badge.text.clone(), medium(11.0), color);
    let rect = Rect::from_min_size(pos2(left.x, left.y - 10.0), vec2((galley.size().x + 18.0).round(), 20.0));
    painter.rect(rect, 10.0, mix(palette.raised, color, 0.12), Stroke::new(1.0, mix(palette.raised, color, 0.45)), StrokeKind::Inside);
    painter.galley(pos2(rect.left() + 9.0, rect.center().y - galley.size().y / 2.0), galley, color);
    rect
}

/// Asks for a file or a folder without blocking the interface.
pub fn pick_path(ctx: &egui::Context, folder: bool, picked: impl FnOnce(PathBuf) + Send + 'static) {
    // Tests must never open a window on the desktop.
    if cfg!(test) {
        return;
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let dialog = rfd::FileDialog::new();
        let path = if folder { dialog.pick_folder() } else { dialog.pick_file() };
        if let Some(path) = path {
            picked(path);
            ctx.request_repaint();
        }
    });
}

/// Hides what overflows the rounded bottom corners of a card: rows are clipped by a rectangle.
pub fn mask_bottom_corners(painter: &egui::Painter, card: Rect, radius: f32, background: Color32) {
    const STEPS: usize = 8;
    for (corner, center, direction) in [
        (card.left_bottom(), card.left_bottom() + vec2(radius, -radius), -1.0_f32),
        (card.right_bottom(), card.right_bottom() + vec2(-radius, -radius), 1.0_f32),
    ] {
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(corner, background);
        for step in 0..=STEPS {
            let angle = step as f32 / STEPS as f32 * std::f32::consts::FRAC_PI_2;
            let arc = center + vec2(direction * angle.cos(), angle.sin()) * radius;
            mesh.colored_vertex(arc, background);
        }
        for step in 1..=STEPS as u32 {
            mesh.add_triangle(0, step, step + 1);
        }
        painter.add(egui::Shape::mesh(mesh));
    }
}
