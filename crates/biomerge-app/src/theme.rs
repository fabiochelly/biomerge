//! Interface and document palettes. One setting drives both: everything is dark or everything
//! is light.

use biomerge_core::highlight::SyntaxTheme;
use egui::epaint::FontColorTransferFunction;
use egui::{Color32, CornerRadius, Stroke, TextStyle};

pub const DEFAULT_CODE_SIZE: f32 = 13.0;
pub const MIN_CODE_SIZE: f32 = 9.0;
pub const MAX_CODE_SIZE: f32 = 26.0;
const LINE_HEIGHT_RATIO: f32 = 1.6;

/// Whole pixels, so that rows never fall between two pixels.
pub fn line_height(code_size: f32) -> f32 {
    (code_size * LINE_HEIGHT_RATIO).round()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "dark" => Some(Mode::Dark),
            "light" => Some(Mode::Light),
            _ => None,
        }
    }

    pub fn syntax(self) -> SyntaxTheme {
        match self {
            Mode::Dark => SyntaxTheme::Dark,
            Mode::Light => SyntaxTheme::Light,
        }
    }
}

const fn hex(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// Chrome colours: menus, tabs, status bar.
#[derive(Clone, Copy, Debug)]
pub struct UiPalette {
    pub mode: Mode,
    pub background: Color32,
    pub panel: Color32,
    pub raised: Color32,
    pub input: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub error: Color32,
    pub warning: Color32,
    pub status_top: Color32,
    pub status_bottom: Color32,
}

impl UiPalette {
    pub const fn get(mode: Mode) -> Self {
        match mode {
            Mode::Dark => Self {
                mode,
                background: hex(0x1a1b26),
                panel: hex(0x16161e),
                raised: hex(0x1f2335),
                input: hex(0x14141b),
                hover: hex(0x292e42),
                border: hex(0x2a2e42),
                text: hex(0xc0caf5),
                text_dim: hex(0x8089b3),
                accent: hex(0x7aa2f7),
                on_accent: hex(0x15161e),
                error: hex(0xf7768e),
                warning: hex(0xe0af68),
                status_top: hex(0x1a1b26),
                status_bottom: hex(0x14141c),
            },
            Mode::Light => Self {
                mode,
                background: hex(0xe6e7ed),
                panel: hex(0xf1f2f6),
                raised: hex(0xffffff),
                input: hex(0xffffff),
                hover: hex(0xdcdfe9),
                border: hex(0xc8cbd9),
                text: hex(0x343b58),
                text_dim: hex(0x636a88),
                accent: hex(0x2e7de9),
                on_accent: hex(0xffffff),
                error: hex(0xc5283d),
                warning: hex(0xb7791f),
                status_top: hex(0xf5f6f9),
                status_bottom: hex(0xe7e8ef),
            },
        }
    }
}

/// The colours of one kind of change.
#[derive(Clone, Copy, Debug)]
pub struct Tint {
    /// Background of the whole zone.
    pub zone: Color32,
    /// Background of the line-number gutter.
    pub gutter: Color32,
    /// Darker background behind the text itself.
    pub text: Color32,
    /// Saturated colour: thin position bars and overview marks.
    pub mark: Color32,
}

/// Compared-content colours.
#[derive(Clone, Copy, Debug)]
pub struct DocPalette {
    pub canvas: Color32,
    pub text: Color32,
    pub line_number: Color32,
    pub frame: Color32,
    pub selection: Color32,
    /// Behind what the filter found.
    pub found: Color32,
    pub control: Color32,
    pub control_text: Color32,
    pub insert: Tint,
    pub delete: Tint,
    pub change: Tint,
    /// What the ignore rules leave out: neutral, so that it reads as "seen, not counted".
    pub ignored: Tint,
    /// Files whose bytes differ but whose text or JSON value does not: a nuance of identical.
    pub equivalent: Tint,
    /// A three-way conflict nobody resolved yet: the one colour that must not be missed.
    pub conflict: Tint,
    /// A conflict once resolved: the same hue, quietened.
    pub resolved: Tint,
}

impl DocPalette {
    pub const fn get(mode: Mode) -> Self {
        match mode {
            Mode::Light => Self {
                canvas: hex(0xffffff),
                text: hex(0x1f2328),
                line_number: hex(0x6e7485),
                frame: hex(0x7f8290),
                selection: hex(0xb6d6fd),
                found: hex(0xffe08a),
                control: hex(0xffffff),
                control_text: hex(0x23262f),
                insert: Tint {
                    zone: hex(0xf0faf0),
                    gutter: hex(0xcdeccd),
                    text: hex(0xc2ecc5),
                    mark: hex(0x2ea043),
                },
                delete: Tint {
                    zone: hex(0xfef6ee),
                    gutter: hex(0xf9e0c8),
                    text: hex(0xf8dcc0),
                    mark: hex(0xe07b1f),
                },
                change: Tint {
                    zone: hex(0xf6f3fc),
                    gutter: hex(0xdfd5f1),
                    text: hex(0xe2d6f8),
                    mark: hex(0x8250df),
                },
                ignored: Tint {
                    zone: hex(0xf6f6f8),
                    gutter: hex(0xe8e9ee),
                    text: hex(0xe3e4ea),
                    mark: hex(0x9da2b3),
                },
                equivalent: Tint {
                    zone: hex(0xeff8f7),
                    gutter: hex(0xd3ecea),
                    text: hex(0xc9e9e6),
                    mark: hex(0x1b7f78),
                },
                conflict: Tint {
                    zone: hex(0xfdf0f3),
                    gutter: hex(0xf7d3dc),
                    text: hex(0xf8d6de),
                    mark: hex(0xd1365a),
                },
                resolved: Tint {
                    zone: hex(0xf9f6f8),
                    gutter: hex(0xede5eb),
                    text: hex(0xebe2e9),
                    mark: hex(0xa47b8d),
                },
            },
            Mode::Dark => Self {
                canvas: hex(0x1a1b26),
                text: hex(0xc0caf5),
                line_number: hex(0x636da0),
                frame: hex(0x5a6390),
                selection: hex(0x33467c),
                found: hex(0x453607),
                control: hex(0x24283b),
                control_text: hex(0xc0caf5),
                insert: Tint {
                    zone: hex(0x1b2925),
                    gutter: hex(0x263f37),
                    text: hex(0x1b3a2c),
                    mark: hex(0x9ece6a),
                },
                delete: Tint {
                    zone: hex(0x2c221c),
                    gutter: hex(0x452f20),
                    text: hex(0x452c18),
                    mark: hex(0xff9e64),
                },
                change: Tint {
                    zone: hex(0x211f34),
                    gutter: hex(0x322d54),
                    text: hex(0x352b5c),
                    mark: hex(0xbb9af7),
                },
                ignored: Tint {
                    zone: hex(0x1e1f2a),
                    gutter: hex(0x282a37),
                    text: hex(0x2b2d3b),
                    mark: hex(0x6b7089),
                },
                equivalent: Tint {
                    zone: hex(0x1a2a2d),
                    gutter: hex(0x213a3c),
                    text: hex(0x1f3b3d),
                    mark: hex(0x73daca),
                },
                conflict: Tint {
                    zone: hex(0x2e1c27),
                    gutter: hex(0x4b2536),
                    text: hex(0x4e2236),
                    mark: hex(0xf7768e),
                },
                resolved: Tint {
                    zone: hex(0x251f2d),
                    gutter: hex(0x382f44),
                    text: hex(0x372e42),
                    mark: hex(0x9d8299),
                },
            },
        }
    }
}

/// Blends `amount` of `over` on `base`.
pub fn mix(base: Color32, over: Color32, amount: f32) -> Color32 {
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color32::from_rgb(
        lerp(base.r(), over.r()),
        lerp(base.g(), over.g()),
        lerp(base.b(), over.b()),
    )
}

/// How glyph coverage becomes opacity, for 3K and 4K screens: unhinted greyscale glyphs, with
/// the slight weight of macOS text. Light-on-dark text takes the linear-light correction,
/// dark-on-light text a touch of weight.
fn transfer_function(mode: Mode) -> FontColorTransferFunction {
    match mode {
        Mode::Dark => FontColorTransferFunction::Gamma(1.0 / 2.2),
        Mode::Light => FontColorTransferFunction::Gamma(0.85),
    }
}

pub fn apply_style(ctx: &egui::Context, palette: &UiPalette) {
    ctx.set_theme(match palette.mode {
        Mode::Dark => egui::Theme::Dark,
        Mode::Light => egui::Theme::Light,
    });
    ctx.all_styles_mut(|style| {
        let mut visuals = match palette.mode {
            Mode::Dark => egui::Visuals::dark(),
            Mode::Light => egui::Visuals::light(),
        };
        // Text as the typeface was drawn: no snapping of stems to the pixel grid, glyphs at
        // their exact advances, greyscale coverage only.
        visuals.text_options.font_hinting = false;
        visuals.text_options.subpixel_binning = true;
        visuals.text_options.color_transfer_function = transfer_function(palette.mode);
        visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
        visuals.panel_fill = palette.panel;
        visuals.window_fill = palette.raised;
        visuals.window_stroke = Stroke::new(1.0, palette.border);
        visuals.window_corner_radius = CornerRadius::same(10);
        visuals.menu_corner_radius = CornerRadius::same(9);
        visuals.window_shadow = shadow(18, 50);
        visuals.popup_shadow = shadow(10, 30);
        visuals.extreme_bg_color = palette.input;
        visuals.faint_bg_color = palette.raised;
        visuals.text_edit_bg_color = Some(palette.input);
        visuals.override_text_color = None;
        visuals.weak_text_color = Some(palette.text_dim);
        visuals.selection.bg_fill = mix(palette.raised, palette.accent, 0.4);
        visuals.selection.stroke = Stroke::new(1.0, palette.accent);
        visuals.text_cursor.stroke = Stroke::new(1.5, palette.text);
        visuals.error_fg_color = palette.error;

        let widgets = &mut visuals.widgets;
        widgets.noninteractive.bg_fill = palette.raised;
        widgets.noninteractive.weak_bg_fill = palette.raised;
        widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.border);
        widgets.noninteractive.fg_stroke = Stroke::new(1.0, palette.text);
        for state in [&mut widgets.inactive, &mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
            state.corner_radius = CornerRadius::same(6);
            state.expansion = 0.0;
            state.bg_fill = palette.hover;
            state.weak_bg_fill = palette.hover;
            state.bg_stroke = Stroke::new(1.0, palette.border);
            state.fg_stroke = Stroke::new(1.0, palette.text);
        }
        widgets.inactive.weak_bg_fill = palette.raised;
        widgets.active.fg_stroke = Stroke::new(1.0, palette.accent);

        style.visuals = visuals;
        style.animation_time = 0.1;
        style.interaction.show_tooltips_only_when_still = true;
        let spacing = &mut style.spacing;
        spacing.item_spacing = egui::vec2(8.0, 6.0);
        spacing.button_padding = egui::vec2(14.0, 6.0);
        spacing.interact_size = egui::vec2(24.0, 30.0);
        spacing.menu_margin = egui::Margin::same(6);
        spacing.menu_spacing = 2.0;
        spacing.menu_width = 260.0;
        spacing.window_margin = egui::Margin::same(20);
        spacing.icon_width = 16.0;
        spacing.icon_spacing = 8.0;

        for (text_style, size) in [
            (TextStyle::Small, 11.0),
            (TextStyle::Body, 13.0),
            (TextStyle::Button, 13.0),
            (TextStyle::Heading, 16.0),
            (TextStyle::Monospace, 13.0),
        ] {
            if let Some(font) = style.text_styles.get_mut(&text_style) {
                font.size = size;
            }
        }
    });
}

fn shadow(offset: i8, blur: u8) -> egui::Shadow {
    egui::Shadow {
        offset: [0, offset],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(100),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color32) -> f32 {
        let linear = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn text_stays_readable_on_every_zone_in_both_themes() {
        for mode in [Mode::Light, Mode::Dark] {
            let doc = DocPalette::get(mode);
            let found = Tint { zone: doc.found, gutter: doc.found, text: doc.found, mark: doc.found };
            for tint in [doc.insert, doc.delete, doc.change, doc.ignored, doc.equivalent, doc.conflict, doc.resolved, found] {
                for background in [tint.zone, tint.gutter, tint.text] {
                    let ratio = contrast(doc.text, background);
                    assert!(ratio >= 7.0, "{mode:?}: contrast {ratio} on {background:?}");
                }
            }
            let ui = UiPalette::get(mode);
            assert!(contrast(ui.text, ui.panel) >= 7.0, "{mode:?}");
            assert!(contrast(ui.text_dim, ui.panel) >= 4.5, "{mode:?}");
        }
    }

    #[test]
    fn every_code_color_stays_readable_on_the_darkest_backgrounds() {
        for mode in [Mode::Light, Mode::Dark] {
            let doc = DocPalette::get(mode);
            for [r, g, b] in biomerge_core::highlight::theme_colors(mode.syntax()) {
                let color = Color32::from_rgb(r, g, b);
                for tint in [doc.insert, doc.delete, doc.change, doc.conflict, doc.resolved] {
                    let ratio = contrast(color, tint.text);
                    assert!(ratio >= 4.5, "{mode:?}: {color:?} at {ratio} on {:?}", tint.text);
                }
            }
        }
    }

    #[test]
    fn an_unresolved_conflict_stands_out_more_than_a_resolved_one() {
        // Hue matters here, not lightness alone: the distance from the canvas in RGB.
        let distance = |a: Color32, b: Color32| {
            let d = |x: u8, y: u8| (x as f32 - y as f32).powi(2);
            (d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())).sqrt()
        };
        for mode in [Mode::Light, Mode::Dark] {
            let doc = DocPalette::get(mode);
            for (conflict, resolved) in [(doc.conflict.zone, doc.resolved.zone), (doc.conflict.mark, doc.resolved.mark)] {
                assert!(distance(doc.canvas, conflict) > distance(doc.canvas, resolved) * 1.2, "{mode:?}");
            }
        }
    }

    #[test]
    fn darker_text_background_stands_out_from_its_zone() {
        for mode in [Mode::Light, Mode::Dark] {
            let doc = DocPalette::get(mode);
            for tint in [doc.insert, doc.delete, doc.change, doc.conflict] {
                assert!(contrast(tint.zone, tint.text) >= 1.2, "{mode:?}");
            }
        }
    }
}
