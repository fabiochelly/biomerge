//! Shared chrome: menu rows and text fields.

use std::path::{Path, PathBuf};

use egui::{
    Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Id, Margin, Rect, Response, Sense,
    Shadow, Stroke, StrokeKind, Ui, WidgetInfo, pos2, vec2,
};

use crate::fonts;
use crate::icons::{self, Icon};
use crate::theme::{Chrome, Look, UiPalette, alpha, mix};

pub const MENU_ITEM_HEIGHT: f32 = 32.0;

pub fn medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::UI_MEDIUM.into()))
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::UI_SEMIBOLD.into()))
}

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::UI_BOLD.into()))
}

pub fn menu_frame(palette: &UiPalette) -> Frame {
    let radius = match palette.chrome {
        Chrome::Verre => 12,
        Chrome::Porcelaine => 14,
        Chrome::Graphite => 8,
        Chrome::Ardoise | Chrome::Classic => 9,
    };
    Frame::new()
        .fill(palette.raised)
        .stroke(Stroke::new(1.0, palette.border))
        .corner_radius(radius)
        .inner_margin(Margin::same(6))
        .shadow(Shadow {
            offset: [0, 10],
            blur: 30,
            spread: 0,
            color: Color32::from_black_alpha(90),
        })
}

pub fn dialog_frame(palette: &UiPalette) -> Frame {
    let radius = match palette.chrome {
        Chrome::Verre => 16,
        Chrome::Porcelaine => 18,
        Chrome::Graphite => 10,
        Chrome::Ardoise | Chrome::Classic => 12,
    };
    menu_frame(palette).corner_radius(radius).inner_margin(Margin::same(22))
}

/// A menu's fixed content width: egui's would follow its widest row.
pub fn menu_width(ui: &mut Ui, width: f32) {
    ui.set_min_width(width);
    ui.set_max_width(width);
    ui.spacing_mut().item_spacing.y = 0.0;
}

/// A small title over a group of menu items.
pub fn menu_heading(ui: &mut Ui, palette: &UiPalette, text: &str) {
    let width = ui.available_width().max(ui.min_size().x);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    ui.painter().text(pos2(rect.left() + 10.0, rect.center().y), Align2::LEFT_CENTER, text, semibold(11.5), palette.text_faint);
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
    field(ui, palette, id, rect, text, hint, color, label, FontId::proportional(13.0), false, false)
}

/// A field whose text is shown as dots, with a red border while `invalid`.
#[allow(clippy::too_many_arguments)]
pub fn password_field(
    ui: &mut Ui,
    palette: &UiPalette,
    id: Id,
    rect: Rect,
    text: &mut String,
    hint: &str,
    label: &str,
    invalid: bool,
) -> Response {
    field(ui, palette, id, rect, text, hint, palette.text, label, FontId::proportional(14.0), invalid, true)
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
    field(ui, palette, id, rect, text, hint, color, label, FontId::monospace(12.5), invalid, false)
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
    password: bool,
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
            .password(password)
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
    /// The hue of the header's tile, where the theme has one.
    pub tint: Color32,
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

/// `name (role)` as the name and its role, as titles give them: "facture.rs (HEAD)".
fn split_role(title: &str) -> (&str, Option<&str>) {
    match title.strip_suffix(')').and_then(|rest| rest.rsplit_once(" (")) {
        Some((name, role)) if !name.is_empty() => (name, Some(role)),
        _ => (title, None),
    }
}

/// The folder of a path, the home folder written `~`.
fn folder_text(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    Some(
        home.as_ref()
            .and_then(|home| parent.strip_prefix(home).ok())
            .map_or_else(|| parent.display().to_string(), |rest| format!("~/{}", rest.display())),
    )
}

/// The header of a pane: what is compared there, and what can be done with it. A click on the
/// name makes the path editable. Each theme lays it out its own way, with the same functions.
pub fn path_bar(ui: &mut Ui, look: &Look, id: Id, rect: Rect, bar: PathBar<'_>) -> PathAction {
    let palette = &look.ui;
    let chrome = palette.chrome;
    match chrome {
        Chrome::Classic => {
            let radius = CornerRadius { nw: 9, ne: 9, sw: 0, se: 0 };
            ui.painter().rect_filled(rect, radius, palette.raised);
        }
        Chrome::Verre | Chrome::Graphite => {}
        Chrome::Ardoise | Chrome::Porcelaine => {}
    }
    if chrome != Chrome::Ardoise {
        let rule = ui.painter().round_to_pixel_center(rect.bottom() - 0.5);
        let color = match chrome {
            Chrome::Verre => alpha(0xffffff, 0.07),
            Chrome::Graphite => palette.card_border,
            // Barely there: it only parts the header from rows scrolled under it.
            Chrome::Porcelaine => alpha(0x000000, 0.05),
            _ => palette.border,
        };
        ui.painter().hline(rect.x_range(), rule, Stroke::new(1.0, color));
    }
    let inset = match chrome {
        Chrome::Classic => 8.0,
        Chrome::Ardoise => 2.0,
        Chrome::Porcelaine => 12.0,
        Chrome::Verre | Chrome::Graphite => 10.0,
    };

    let mut action = PathAction::None;
    const BUTTON: f32 = 28.0;
    let mut right = rect.right() - inset;
    let mut take = |width: f32| {
        let slot = Rect::from_center_size(pos2(right - width / 2.0, rect.center().y), vec2(width, BUTTON));
        right -= width + 2.0;
        slot
    };
    // The save button: an icon in the classic look, a word elsewhere, as the mockups have it.
    if bar.dirty {
        if chrome == Chrome::Classic {
            if icon_button(ui, palette, take(BUTTON), id.with("save"), Icon::Save, "Enregistrer ce côté", palette.warning).clicked() {
                action = PathAction::Save;
            }
        } else {
            let font = semibold(12.5);
            let width = text_size(ui, "Enregistrer", &font).x + if chrome == Chrome::Verre { 46.0 } else { 28.0 };
            let slot = take(width.round() + 6.0).shrink2(vec2(3.0, 0.0)).translate(vec2(-3.0, 0.0));
            if save_button(ui, look, slot, id.with("save")).clicked() {
                action = PathAction::Save;
            }
        }
    }
    if icon_button(ui, palette, take(BUTTON), id.with("copy"), Icon::Copy, "Copier le chemin", palette.text_dim).clicked() {
        ui.ctx().copy_text(bar.path.display().to_string());
    }
    if !bar.fixed && icon_button(ui, palette, take(BUTTON), id.with("browse"), Icon::FolderOpen, bar.browse_tip, palette.text_dim).clicked() {
        action = PathAction::Browse;
    }
    let text_right = right - 6.0;
    let area = Rect::from_min_max(pos2(rect.left() + inset, rect.top()), pos2(text_right, rect.bottom() - 1.0));

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
    let file_name = || bar.path.file_name().map_or_else(|| bar.path.display().to_string(), |n| n.to_string_lossy().into_owned());
    // A label stands for the whole path: the folder of a temporary file says nothing. A label
    // naming the file itself, with its role, keeps it.
    let real = bar.title.is_none_or(|title| split_role(title).0 == file_name());
    let folder = if real { folder_text(bar.path) } else { None };
    let mut x = area.left() + if chrome == Chrome::Classic { 8.0 } else { 4.0 };
    if chrome == Chrome::Classic {
        icons::paint(&painter, bar.icon, icons::square(pos2(x + 8.0, y), 16.0), palette.accent);
        x += 25.0;
        let name = bar.title.map_or_else(file_name, str::to_owned);
        x += painter.text(pos2(x, y), Align2::LEFT_CENTER, &name, medium(13.0), palette.text).width() + 6.0;
        x = after_name(&painter, palette, x, y, bar.dirty);
        x = chips(ui, &painter, look, id, area, x, y, bar.read_only, bar.badges);
        if let Some(folder) = folder {
            let font = FontId::proportional(12.0);
            let shown = elide_start(ui, &folder, &font, area.right() - x - 8.0);
            painter.text(pos2(x, y), Align2::LEFT_CENTER, shown, font, palette.text_dim);
        }
        return action;
    }

    // The name, its role and its folder: on two lines in Verre and Porcelaine, on one elsewhere.
    let (name, role) = match bar.title {
        Some(title) => {
            let (name, role) = split_role(title);
            (name.to_owned(), role.map(str::to_owned))
        }
        None => (file_name(), None),
    };
    // The role reads first; when room runs out, the folder gives up its start.
    let role_prefix = role.as_ref().filter(|_| folder.is_some()).map(|role| format!("{role} · "));
    let second = match (role, folder) {
        (Some(_), Some(folder)) => Some(folder),
        (role, folder) => role.or(folder),
    };
    match chrome {
        Chrome::Verre => {
            let tile = Rect::from_min_size(pos2(x, y - 16.0), vec2(32.0, 32.0));
            paint_tile(&painter, tile, bar.tint, bar.icon, palette.dark);
            x = tile.right() + 12.0;
        }
        Chrome::Ardoise => {
            icons::paint(&painter, bar.icon, icons::square(pos2(x + 8.0, y), 16.0), palette.text_dim);
            x += 26.0;
        }
        Chrome::Porcelaine => x += 6.0,
        _ => x += 6.0,
    }
    let two_lines = matches!(chrome, Chrome::Verre | Chrome::Porcelaine) && second.is_some();
    let (name_font, name_y) = match chrome {
        Chrome::Verre => (bold(14.0), if two_lines { y - 8.0 } else { y }),
        Chrome::Porcelaine => (bold(15.0), if two_lines { y - 9.0 } else { y }),
        Chrome::Graphite => (FontId::monospace(12.5), y),
        _ => (semibold(14.0), y),
    };
    let name_right = x + painter.text(pos2(x, name_y), Align2::LEFT_CENTER, &name, name_font, palette.text).width() + 8.0;
    let after = after_name(&painter, palette, name_right, name_y, bar.dirty);
    // Chips stand at the right end of the header, as in the mockups; what precedes them gives
    // way.
    let width = chips_width(ui, look, bar.read_only, bar.badges);
    let chips_left = area.right() - 4.0 - width;
    let mut start = chips_left.max(after);
    if let Some(second) = second {
        let font = FontId::proportional(if chrome == Chrome::Verre { 11.5 } else { 12.0 });
        let prefix = role_prefix.unwrap_or_default();
        let prefix_width = text_size(ui, &prefix, &font).x;
        if two_lines {
            let second_y = y + if chrome == Chrome::Porcelaine { 10.0 } else { 9.0 };
            let shown = elide_start(ui, &second, &font, chips_left - x - 10.0 - prefix_width);
            painter.text(pos2(x, second_y), Align2::LEFT_CENTER, format!("{prefix}{shown}"), font, palette.text_faint);
        } else {
            // On one line, the chips keep their place: the second text takes what is left.
            let room = chips_left - after - 10.0 - prefix_width;
            if room > 30.0 {
                let shown = elide_start(ui, &second, &font, room);
                let end = after + painter.text(pos2(after, y), Align2::LEFT_CENTER, format!("{prefix}{shown}"), font, palette.text_faint).width() + 10.0;
                start = start.max(end);
            }
        }
    }
    chips(ui, &painter, look, id, area, start, y, bar.read_only, bar.badges);
    action
}

/// The dot of unsaved changes after a name; returns where what follows starts.
fn after_name(painter: &egui::Painter, palette: &UiPalette, x: f32, y: f32, dirty: bool) -> f32 {
    if dirty {
        painter.circle_filled(pos2(x + 3.0, y), 3.0, palette.warning);
        x + 14.0
    } else {
        x + 4.0
    }
}

fn text_size(ui: &Ui, text: &str, font: &FontId) -> egui::Vec2 {
    ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE)).size()
}

/// A tinted square holding an icon: the tint at low opacity, a stronger border, the icon in a
/// light (or deep) version of the tint. As Biodrive's tiles.
pub fn paint_tile(painter: &egui::Painter, rect: Rect, tint: Color32, icon: Icon, dark: bool) {
    let [r, g, b, _] = tint.to_array();
    let fill = Color32::from_rgba_unmultiplied(r, g, b, 41);
    let border = Color32::from_rgba_unmultiplied(r, g, b, 100);
    painter.rect(rect, (rect.width() * 0.3).round(), fill, Stroke::new(1.0, border), StrokeKind::Inside);
    let glyph = if dark { mix(tint, Color32::WHITE, 0.5) } else { mix(tint, Color32::BLACK, 0.15) };
    icons::paint(painter, icon, icons::square(rect.center(), (rect.width() * 0.5).round()), glyph);
}

/// The "Enregistrer" button of a header with unsaved changes.
fn save_button(ui: &mut Ui, look: &Look, rect: Rect, id: Id) -> Response {
    let palette = &look.ui;
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, "Enregistrer ce côté"));
    let hovered = response.hovered();
    let painter = ui.painter();
    let (fill, border, text, radius) = match palette.chrome {
        Chrome::Verre => (
            alpha(0x34c77b, if hovered { 0.55 } else { 0.42 }),
            alpha(0x6ee7a8, 0.55),
            Color32::WHITE,
            rect.height() / 2.0,
        ),
        Chrome::Porcelaine => (if hovered { mix(palette.text, Color32::WHITE, 0.15) } else { palette.text }, Color32::TRANSPARENT, Color32::WHITE, rect.height() / 2.0),
        Chrome::Graphite => (if hovered { Color32::WHITE } else { palette.text }, Color32::TRANSPARENT, palette.background, 6.0),
        _ => (if hovered { Color32::WHITE } else { palette.text }, Color32::TRANSPARENT, palette.background, 8.0),
    };
    let rect = rect.shrink2(vec2(0.0, 1.0));
    painter.rect(rect, radius, fill, Stroke::new(1.0, border), StrokeKind::Inside);
    let font = semibold(12.5);
    if palette.chrome == Chrome::Verre {
        icons::paint(painter, Icon::Check, icons::square(pos2(rect.left() + 20.0, rect.center().y), 14.0), text);
        painter.text(pos2(rect.left() + 32.0, rect.center().y), Align2::LEFT_CENTER, "Enregistrer", font, text);
    } else {
        painter.text(rect.center(), Align2::CENTER_CENTER, "Enregistrer", font, text);
    }
    response.on_hover_text("Enregistrer ce côté (Ctrl+S)").on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The width the chips of a header take, gaps included.
fn chips_width(ui: &Ui, look: &Look, read_only: bool, badges: &[Badge]) -> f32 {
    let mut width = 0.0;
    if read_only {
        width += chip_size(ui, look, "lecture seule", true) + 8.0;
    }
    for badge in badges {
        width += chip_size(ui, look, &badge.text, false) + 8.0;
    }
    width
}

fn chip_size(ui: &Ui, look: &Look, text: &str, lock: bool) -> f32 {
    let extra = if look.ui.chrome == Chrome::Classic {
        if lock { 32.0 } else { 18.0 }
    } else {
        20.0
    };
    (text_size(ui, text, &medium(11.0)).x + extra).round()
}

/// The read-only chip and the badges, from `x`. Returns where they end.
#[allow(clippy::too_many_arguments)]
fn chips(ui: &Ui, painter: &egui::Painter, look: &Look, id: Id, area: Rect, mut x: f32, y: f32, read_only: bool, badges: &[Badge]) -> f32 {
    let palette = &look.ui;
    if read_only {
        if palette.chrome == Chrome::Classic {
            x += read_only_badge(painter, palette, pos2(x, y)) + 8.0;
        } else {
            let badge = Badge::new("lecture seule", "");
            x = paint_chip(painter, look, pos2(x, y), &badge).right() + 8.0;
        }
    }
    for (index, badge) in badges.iter().enumerate() {
        let rect = if palette.chrome == Chrome::Classic {
            paint_badge(painter, palette, pos2(x, y), badge)
        } else {
            paint_chip(painter, look, pos2(x, y), badge)
        };
        if rect.left() < area.right() {
            let hover = ui.interact(rect.intersect(area), id.with(("badge", index)), Sense::hover());
            hover.widget_info(|| WidgetInfo::labeled(egui::Role::Label, true, &badge.text));
            hover.on_hover_text(&badge.tip);
        }
        x = rect.right() + 8.0;
    }
    x
}

/// A chip in the style of the theme: glass, outlined, or a soft pill.
fn paint_chip(painter: &egui::Painter, look: &Look, left: egui::Pos2, badge: &Badge) -> Rect {
    let palette = &look.ui;
    let color = match badge.tone {
        Tone::Neutral => palette.text_dim,
        Tone::Warning => palette.warning,
        Tone::Error => palette.error,
    };
    let galley = painter.layout_no_wrap(badge.text.clone(), medium(11.0), color);
    let height = if palette.chrome == Chrome::Graphite { 20.0 } else { 22.0 };
    let rect = Rect::from_min_size(pos2(left.x, left.y - height / 2.0), vec2((galley.size().x + 20.0).round(), height));
    let tinted = badge.tone != Tone::Neutral;
    let [r, g, b, _] = color.to_array();
    let wash = |opacity: u8| Color32::from_rgba_unmultiplied(r, g, b, opacity);
    let (fill, border, radius) = match palette.chrome {
        Chrome::Verre => (if tinted { wash(30) } else { alpha(0xffffff, 0.06) }, if tinted { wash(90) } else { alpha(0xffffff, 0.08) }, 6.0),
        Chrome::Porcelaine => (if tinted { wash(26) } else { palette.background }, Color32::TRANSPARENT, height / 2.0),
        Chrome::Graphite => (Color32::TRANSPARENT, if tinted { wash(120) } else { palette.border }, 5.0),
        _ => (Color32::TRANSPARENT, if tinted { wash(120) } else { palette.border }, 6.0),
    };
    painter.rect(rect, radius, fill, Stroke::new(1.0, border), StrokeKind::Inside);
    painter.galley(pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0), galley, color);
    rect
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
    mask_corners(painter, card, radius, background, false);
}

/// The same for the top corners, or the bottom ones.
pub fn mask_corners(painter: &egui::Painter, card: Rect, radius: f32, background: Color32, top: bool) {
    const STEPS: usize = 8;
    let (left, right, vertical) = if top {
        (card.left_top(), card.right_top(), 1.0_f32)
    } else {
        (card.left_bottom(), card.right_bottom(), -1.0_f32)
    };
    for (corner, center, direction) in [
        (left, left + vec2(radius, vertical * radius), -1.0_f32),
        (right, right + vec2(-radius, vertical * radius), 1.0_f32),
    ] {
        let mut mesh = egui::Mesh::default();
        mesh.colored_vertex(corner, background);
        for step in 0..=STEPS {
            let angle = step as f32 / STEPS as f32 * std::f32::consts::FRAC_PI_2;
            let arc = center + vec2(direction * angle.cos(), -vertical * angle.sin()) * radius;
            mesh.colored_vertex(arc, background);
        }
        for step in 1..=STEPS as u32 {
            mesh.add_triangle(0, step, step + 1);
        }
        painter.add(egui::Shape::mesh(mesh));
    }
}
