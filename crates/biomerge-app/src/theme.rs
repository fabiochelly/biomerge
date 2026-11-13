//! Themes: the interface (chrome) and the code area (documents) each have their palette, so that
//! a dark interface can show light code. One setting picks the theme; two more pick the code's
//! tone and how blocks are linked between panes.

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

/// The theme setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    /// Translucent glass blurred by the compositor, as Biodrive.
    Verre,
    /// A dark interface around light code on paper.
    Ardoise,
    /// Light and soft, with a floating dock.
    Porcelaine,
    /// Dense and nearly monochrome.
    Graphite,
    /// The theme BioMerge started with.
    TokyoNight,
    Clair,
}

impl ThemeKind {
    pub const ALL: [ThemeKind; 6] = [
        ThemeKind::Verre,
        ThemeKind::Ardoise,
        ThemeKind::Porcelaine,
        ThemeKind::Graphite,
        ThemeKind::TokyoNight,
        ThemeKind::Clair,
    ];

    pub fn key(self) -> &'static str {
        match self {
            ThemeKind::Verre => "verre",
            ThemeKind::Ardoise => "ardoise",
            ThemeKind::Porcelaine => "porcelaine",
            ThemeKind::Graphite => "graphite",
            ThemeKind::TokyoNight => "tokyo-night",
            ThemeKind::Clair => "clair",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ThemeKind::Verre => "Verre",
            ThemeKind::Ardoise => "Ardoise et papier",
            ThemeKind::Porcelaine => "Porcelaine",
            ThemeKind::Graphite => "Graphite",
            ThemeKind::TokyoNight => "Tokyo Night",
            ThemeKind::Clair => "Clair",
        }
    }

    /// The stored setting. The old one only knew "dark" and "light", and saved "dark" on every
    /// exit even when nobody chose it, so "dark" means "the default" and gets Verre; "light"
    /// was a real choice and keeps its look.
    pub fn from_stored(value: Option<&str>) -> Self {
        match value {
            Some("dark") => ThemeKind::Verre,
            Some("light") => ThemeKind::Clair,
            Some(key) => Self::ALL.into_iter().find(|kind| kind.key() == key).unwrap_or(ThemeKind::Verre),
            None => ThemeKind::Verre,
        }
    }

    pub fn chrome(self) -> Chrome {
        match self {
            ThemeKind::Verre => Chrome::Verre,
            ThemeKind::Ardoise => Chrome::Ardoise,
            ThemeKind::Porcelaine => Chrome::Porcelaine,
            ThemeKind::Graphite => Chrome::Graphite,
            ThemeKind::TokyoNight | ThemeKind::Clair => Chrome::Classic,
        }
    }

    fn dark_chrome(self) -> bool {
        !matches!(self, ThemeKind::Porcelaine | ThemeKind::Clair)
    }

    /// The code's tone when it follows the theme.
    fn dark_code(self) -> bool {
        matches!(self, ThemeKind::Verre | ThemeKind::Graphite | ThemeKind::TokyoNight)
    }
}

/// Code light or dark whatever the theme, or as the theme has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeTone {
    Theme,
    Light,
    Dark,
}

impl CodeTone {
    pub const ALL: [CodeTone; 3] = [CodeTone::Theme, CodeTone::Light, CodeTone::Dark];

    pub fn key(self) -> &'static str {
        match self {
            CodeTone::Theme => "theme",
            CodeTone::Light => "light",
            CodeTone::Dark => "dark",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tone| tone.key() == key)
    }

    pub fn name(self) -> &'static str {
        match self {
            CodeTone::Theme => "Code : comme le thème",
            CodeTone::Light => "Code : clair",
            CodeTone::Dark => "Code : sombre",
        }
    }
}

/// How a block is joined to its counterpart across the gap between two panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkStyle {
    /// A thin S curve between the frames of the block.
    Lines,
    /// A translucent band from the block's lines on one side to those on the other.
    Ribbons,
}

impl LinkStyle {
    pub fn key(self) -> &'static str {
        match self {
            LinkStyle::Lines => "lines",
            LinkStyle::Ribbons => "ribbons",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "lines" => Some(LinkStyle::Lines),
            "ribbons" => Some(LinkStyle::Ribbons),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            LinkStyle::Lines => "Liaisons : lignes fines",
            LinkStyle::Ribbons => "Liaisons : rubans",
        }
    }
}

/// The layout of the chrome: tabs, pane headers, status bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chrome {
    Verre,
    Ardoise,
    Porcelaine,
    Graphite,
    /// Tokyo Night and Clair, as BioMerge first looked.
    Classic,
}

/// Sizes that change with the chrome.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub band_height: f32,
    pub status_height: f32,
    /// Around the panes, inside the central area.
    pub margin: egui::Vec2,
    pub margin_top: f32,
    /// Between two panes, where blocks are linked and their arrows stand.
    pub gap: f32,
    pub header_height: f32,
    pub card_radius: u8,
    pub overview_width: f32,
    pub overview_margin: f32,
    /// The header stands above the code card instead of inside it.
    pub header_outside: bool,
    /// The bar of a changed line runs along the pane's left edge rather than after its number.
    pub bar_at_edge: bool,
    pub window_radius: u8,
}

impl Metrics {
    fn of(chrome: Chrome) -> Self {
        let base = Metrics {
            band_height: 48.0,
            status_height: 40.0,
            margin: egui::vec2(12.0, 12.0),
            margin_top: 12.0,
            gap: 60.0,
            header_height: 44.0,
            card_radius: 10,
            overview_width: 10.0,
            overview_margin: 12.0,
            header_outside: false,
            bar_at_edge: false,
            window_radius: 0,
        };
        match chrome {
            Chrome::Classic => base,
            Chrome::Verre => Metrics {
                band_height: 60.0,
                status_height: 60.0,
                margin: egui::vec2(16.0, 0.0),
                margin_top: 0.0,
                header_height: 56.0,
                card_radius: 16,
                overview_margin: 10.0,
                window_radius: 14,
                ..base
            },
            Chrome::Ardoise => Metrics {
                band_height: 52.0,
                status_height: 48.0,
                margin: egui::vec2(16.0, 4.0),
                margin_top: 14.0,
                header_height: 46.0,
                card_radius: 12,
                overview_width: 8.0,
                header_outside: true,
                ..base
            },
            Chrome::Porcelaine => Metrics {
                band_height: 64.0,
                status_height: 96.0,
                margin: egui::vec2(22.0, 0.0),
                margin_top: 4.0,
                header_height: 58.0,
                card_radius: 18,
                overview_width: 8.0,
                ..base
            },
            Chrome::Graphite => Metrics {
                band_height: 48.0,
                status_height: 34.0,
                margin: egui::vec2(0.0, 0.0),
                margin_top: 0.0,
                gap: 48.0,
                header_height: 40.0,
                card_radius: 0,
                overview_width: 14.0,
                overview_margin: 0.0,
                bar_at_edge: true,
                ..base
            },
        }
    }
}

const fn hex(value: u32) -> Color32 {
    Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// A colour with an opacity, from its hex value.
pub fn alpha(value: u32, opacity: f32) -> Color32 {
    let [r, g, b, _] = hex(value).to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (opacity * 255.0).round() as u8)
}

/// What `over` looks like painted on an opaque `under`.
pub fn over(top: Color32, under: Color32) -> Color32 {
    let keep = 1.0 - top.a() as f32 / 255.0;
    let channel = |t: u8, u: u8| (t as f32 + u as f32 * keep).round().min(255.0) as u8;
    Color32::from_rgb(channel(top.r(), under.r()), channel(top.g(), under.g()), channel(top.b(), under.b()))
}

/// The tinted squares of tabs and pane headers, one hue per kind of thing.
#[derive(Clone, Copy, Debug)]
pub struct Hues {
    pub violet: Color32,
    pub blue: Color32,
    pub pink: Color32,
    pub amber: Color32,
    pub teal: Color32,
    pub green: Color32,
    pub slate: Color32,
}

/// The buttons that send a block across, or take a side in a merge.
#[derive(Clone, Copy, Debug)]
pub struct ArrowStyle {
    pub fill: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub icon: Color32,
    /// A coloured halo, as the glowing dots of Biodrive.
    pub glow: bool,
    pub size: f32,
    /// Half the size gives a circle.
    pub radius: f32,
}

/// How strongly the ribbons between panes are tinted.
#[derive(Clone, Copy, Debug)]
pub struct RibbonStyle {
    pub fill: f32,
    pub edge: f32,
    pub current_fill: f32,
}

/// Chrome colours: menus, tabs, status bar, dialogs.
#[derive(Clone, Copy, Debug)]
pub struct UiPalette {
    pub chrome: Chrome,
    pub dark: bool,
    /// The window's own fill, under everything: translucent glass for Verre.
    pub window: Color32,
    pub window_border: Color32,
    pub background: Color32,
    pub panel: Color32,
    pub raised: Color32,
    pub input: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    /// Second lines and hints: still readable.
    pub text_faint: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub error: Color32,
    pub warning: Color32,
    pub status_top: Color32,
    pub status_bottom: Color32,
    /// The card of a pane, around its header and its code.
    pub card: Color32,
    pub card_border: Color32,
    /// A lit option: fill, border and icon.
    pub lit: Color32,
    pub lit_border: Color32,
    pub lit_text: Color32,
    pub hues: Hues,
    pub arrow: ArrowStyle,
    pub ribbon: RibbonStyle,
}

impl UiPalette {
    /// The interface of a theme, apart from its code.
    pub fn of(kind: ThemeKind, translucent: bool) -> Self {
        let dark_hues = Hues {
            violet: hex(0x8b5cf6),
            blue: hex(0x3b82f6),
            pink: hex(0xec4899),
            amber: hex(0xf59e0b),
            teal: hex(0x14b8a6),
            green: hex(0x34c77b),
            slate: hex(0x94a3b8),
        };
        let light_hues = Hues {
            violet: hex(0x7c3aed),
            blue: hex(0x2563eb),
            pink: hex(0xdb2777),
            amber: hex(0xb45309),
            teal: hex(0x0f766e),
            green: hex(0x15803d),
            slate: hex(0x64748b),
        };
        let circle = |fill: Color32, hover: Color32, icon: Color32| ArrowStyle {
            fill,
            hover,
            border: Color32::TRANSPARENT,
            icon,
            glow: false,
            size: 26.0,
            radius: 13.0,
        };
        match kind {
            ThemeKind::Verre => {
                let glass = alpha(0x10141a, 0.62);
                let window = if translucent { glass } else { over(glass, VERRE_WALLPAPER) };
                let ink = 0xeef1f5;
                Self {
                    chrome: Chrome::Verre,
                    dark: true,
                    window,
                    window_border: alpha(0xffffff, 0.08),
                    background: Color32::TRANSPARENT,
                    panel: Color32::TRANSPARENT,
                    raised: hex(0x1a1f28),
                    input: alpha(0xffffff, 0.045),
                    hover: alpha(0xffffff, 0.08),
                    border: alpha(0xffffff, 0.10),
                    text: hex(ink),
                    text_dim: alpha(ink, 0.78),
                    text_faint: alpha(ink, 0.70),
                    accent: hex(0xa78bfa),
                    on_accent: hex(0xffffff),
                    error: hex(0xff7b80),
                    warning: hex(0xf5b941),
                    status_top: Color32::TRANSPARENT,
                    status_bottom: Color32::TRANSPARENT,
                    card: alpha(0xffffff, 0.05),
                    card_border: alpha(0xffffff, 0.08),
                    lit: alpha(0x8b5cf6, 0.25),
                    lit_border: alpha(0x8b5cf6, 0.45),
                    lit_text: hex(0xd6c8ff),
                    hues: dark_hues,
                    arrow: ArrowStyle {
                        fill: alpha(0x8b5cf6, 0.55),
                        hover: alpha(0x8b5cf6, 0.8),
                        border: alpha(0xc4b0ff, 0.6),
                        icon: hex(0xffffff),
                        glow: true,
                        size: 26.0,
                        radius: 13.0,
                    },
                    ribbon: RibbonStyle { fill: 0.2, edge: 0.58, current_fill: 0.32 },
                }
            }
            ThemeKind::Ardoise => Self {
                chrome: Chrome::Ardoise,
                dark: true,
                window: hex(0x0f1115),
                window_border: Color32::TRANSPARENT,
                background: hex(0x0f1115),
                panel: hex(0x0f1115),
                raised: hex(0x171a20),
                input: hex(0x171a20),
                hover: hex(0x1d2129),
                border: hex(0x2c323c),
                text: hex(0xe8ebf0),
                text_dim: hex(0xa8b0bd),
                text_faint: hex(0x8e97a6),
                accent: hex(0xa78bfa),
                on_accent: hex(0x0f1115),
                error: hex(0xf87171),
                warning: hex(0xf5b941),
                status_top: hex(0x0f1115),
                status_bottom: hex(0x0f1115),
                card: Color32::TRANSPARENT,
                card_border: alpha(0xffffff, 0.06),
                lit: hex(0x2a2340),
                lit_border: hex(0x2a2340),
                lit_text: hex(0xc4b5fd),
                hues: Hues { blue: hex(0x5aa9ff), ..dark_hues },
                arrow: circle(hex(0x7c3aed), hex(0x8b5cf6), hex(0xffffff)),
                ribbon: RibbonStyle { fill: 0.22, edge: 0.6, current_fill: 0.4 },
            },
            ThemeKind::Porcelaine => Self {
                chrome: Chrome::Porcelaine,
                dark: false,
                window: hex(0xf2f2ee),
                window_border: Color32::TRANSPARENT,
                background: hex(0xf2f2ee),
                panel: hex(0xf2f2ee),
                raised: hex(0xffffff),
                input: hex(0xffffff),
                hover: hex(0xe6e6e0),
                border: hex(0xdcdcd5),
                text: hex(0x1c1f24),
                text_dim: hex(0x4b5059),
                text_faint: hex(0x5f6570),
                accent: hex(0x2f5bd3),
                on_accent: hex(0xffffff),
                error: hex(0xc5283d),
                warning: hex(0xb45309),
                status_top: hex(0xf2f2ee),
                status_bottom: hex(0xf2f2ee),
                card: hex(0xffffff),
                card_border: alpha(0x000000, 0.05),
                lit: hex(0x1c1f24),
                lit_border: hex(0x1c1f24),
                lit_text: hex(0xffffff),
                hues: light_hues,
                arrow: circle(hex(0x1c1f24), hex(0x3a3f47), hex(0xffffff)),
                ribbon: RibbonStyle { fill: 0.14, edge: 0.32, current_fill: 0.26 },
            },
            ThemeKind::Graphite => Self {
                chrome: Chrome::Graphite,
                dark: true,
                window: hex(0x0c0c0d),
                window_border: Color32::TRANSPARENT,
                background: hex(0x0c0c0d),
                panel: hex(0x0c0c0d),
                raised: hex(0x1c1c1e),
                input: hex(0x141415),
                hover: hex(0x1f1f21),
                border: hex(0x2a2a2d),
                text: hex(0xededed),
                text_dim: hex(0xa1a1a6),
                text_faint: hex(0x8f8f94),
                accent: hex(0x93c5fd),
                on_accent: hex(0x0c0c0d),
                error: hex(0xf87171),
                warning: hex(0xfbbf24),
                status_top: hex(0x0c0c0d),
                status_bottom: hex(0x0c0c0d),
                card: hex(0x111112),
                card_border: hex(0x1f1f21),
                lit: hex(0x262628),
                lit_border: hex(0x262628),
                lit_text: hex(0xededed),
                hues: dark_hues,
                arrow: ArrowStyle {
                    fill: hex(0x1c1c1e),
                    hover: hex(0x2a2a2d),
                    border: hex(0x3b3b3f),
                    icon: hex(0xededed),
                    glow: false,
                    size: 22.0,
                    radius: 6.0,
                },
                ribbon: RibbonStyle { fill: 0.16, edge: 0.45, current_fill: 0.32 },
            },
            ThemeKind::TokyoNight => Self {
                chrome: Chrome::Classic,
                dark: true,
                window: hex(0x1a1b26),
                window_border: Color32::TRANSPARENT,
                background: hex(0x1a1b26),
                panel: hex(0x16161e),
                raised: hex(0x1f2335),
                input: hex(0x14141b),
                hover: hex(0x292e42),
                border: hex(0x2a2e42),
                text: hex(0xc0caf5),
                text_dim: hex(0x8089b3),
                text_faint: hex(0x8089b3),
                accent: hex(0x7aa2f7),
                on_accent: hex(0x15161e),
                error: hex(0xf7768e),
                warning: hex(0xe0af68),
                status_top: hex(0x1a1b26),
                status_bottom: hex(0x14141c),
                card: hex(0x1f2335),
                card_border: hex(0x2a2e42),
                lit: over(alpha(0x7aa2f7, 0.2), hex(0x1a1b26)),
                lit_border: over(alpha(0x7aa2f7, 0.55), hex(0x1a1b26)),
                lit_text: hex(0x7aa2f7),
                hues: dark_hues,
                arrow: ArrowStyle {
                    fill: hex(0x24283b),
                    hover: hex(0x2f3550),
                    border: hex(0x5a6390),
                    icon: hex(0xc0caf5),
                    glow: false,
                    size: 24.0,
                    radius: 6.0,
                },
                ribbon: RibbonStyle { fill: 0.2, edge: 0.55, current_fill: 0.34 },
            },
            ThemeKind::Clair => Self {
                chrome: Chrome::Classic,
                dark: false,
                window: hex(0xe6e7ed),
                window_border: Color32::TRANSPARENT,
                background: hex(0xe6e7ed),
                panel: hex(0xf1f2f6),
                raised: hex(0xffffff),
                input: hex(0xffffff),
                hover: hex(0xdcdfe9),
                border: hex(0xc8cbd9),
                text: hex(0x343b58),
                text_dim: hex(0x585f7c),
                text_faint: hex(0x585f7c),
                accent: hex(0x2e7de9),
                on_accent: hex(0xffffff),
                error: hex(0xc5283d),
                warning: hex(0xb7791f),
                status_top: hex(0xf5f6f9),
                status_bottom: hex(0xe7e8ef),
                card: hex(0xffffff),
                card_border: hex(0xc8cbd9),
                lit: over(alpha(0x2e7de9, 0.14), hex(0xf5f6f9)),
                lit_border: over(alpha(0x2e7de9, 0.5), hex(0xf5f6f9)),
                lit_text: hex(0x1d5bbf),
                hues: light_hues,
                arrow: ArrowStyle {
                    fill: hex(0xffffff),
                    hover: hex(0xeef0f5),
                    border: hex(0x7f8290),
                    icon: hex(0x23262f),
                    glow: false,
                    size: 24.0,
                    radius: 6.0,
                },
                ribbon: RibbonStyle { fill: 0.16, edge: 0.42, current_fill: 0.3 },
            },
        }
    }

    /// The colour behind the panes, when it is solid: what hides the overflow of rounded corners.
    pub fn solid_behind_cards(&self) -> Option<Color32> {
        [self.background, self.window].into_iter().find(|color| color.is_opaque())
    }
}

/// What a nominal wallpaper looks like through Verre's blur: the mockup's dark blue. Used to
/// make the window solid when transparency is off, and to check contrasts.
pub const VERRE_WALLPAPER: Color32 = hex(0x1c2636);

/// The colours of one kind of change.
#[derive(Clone, Copy, Debug)]
pub struct Tint {
    /// Background of the whole zone.
    pub zone: Color32,
    /// Background of the line-number gutter.
    pub gutter: Color32,
    /// Stronger background behind the changed text itself.
    pub text: Color32,
    /// Saturated colour: position bars, overview marks, ribbons.
    pub mark: Color32,
    /// The zone of the block being worked on.
    pub current: Color32,
}

/// Compared-content colours: everything painted inside a code area comes from here.
#[derive(Clone, Copy, Debug)]
pub struct DocPalette {
    /// May be transparent: the card behind shows through.
    pub canvas: Color32,
    /// The solid colour the canvas is painted on, as far as it can be known: for contrasts and
    /// for the few colours that must be solid.
    pub backdrop: Color32,
    pub dark: bool,
    pub text: Color32,
    pub line_number: Color32,
    /// Numbers of changed lines stand out a little.
    pub line_number_on: Color32,
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

/// A kind of change, as the mockups give it: the hue of its tints and the colour of its bar.
#[derive(Clone, Copy)]
struct Kind {
    base: u32,
    mark: u32,
}

/// A code palette made of translucent tints over its canvas.
struct CodeSpec {
    canvas: Color32,
    dark: bool,
    text: u32,
    number: Color32,
    number_on: Color32,
    change: Kind,
    insert: Kind,
    delete: Kind,
    conflict: Kind,
    resolved: Kind,
    equivalent: Kind,
    ignored: Kind,
    zone: f32,
    current: f32,
    word: f32,
    selection: Color32,
    found: Color32,
    frame: Color32,
    control: Color32,
}

impl CodeSpec {
    fn palette(&self, backdrop: Color32) -> DocPalette {
        let tint = |kind: Kind| Tint {
            zone: alpha(kind.base, self.zone),
            gutter: alpha(kind.base, self.zone),
            text: alpha(kind.base, self.word),
            mark: hex(kind.mark),
            current: alpha(kind.base, self.current),
        };
        DocPalette {
            canvas: self.canvas,
            backdrop,
            dark: self.dark,
            text: hex(self.text),
            line_number: self.number,
            line_number_on: self.number_on,
            frame: self.frame,
            selection: self.selection,
            found: self.found,
            control: self.control,
            control_text: hex(self.text),
            insert: tint(self.insert),
            delete: tint(self.delete),
            change: tint(self.change),
            ignored: Tint { text: alpha(self.ignored.base, self.word * 0.7), ..tint(self.ignored) },
            equivalent: tint(self.equivalent),
            conflict: Tint { zone: alpha(self.conflict.base, self.zone * 1.4), ..tint(self.conflict) },
            resolved: Tint { zone: alpha(self.resolved.base, self.zone * 0.8), ..tint(self.resolved) },
        }
    }
}

/// Verre's code: pastel tints on the glass card.
fn glass_code() -> CodeSpec {
    let ink = 0xeef1f5;
    CodeSpec {
        canvas: Color32::TRANSPARENT,
        dark: true,
        text: ink,
        number: alpha(ink, 0.7),
        number_on: alpha(ink, 0.88),
        change: Kind { base: 0x8b5cf6, mark: 0xa78bfa },
        insert: Kind { base: 0x34c77b, mark: 0x34c77b },
        delete: Kind { base: 0xf97316, mark: 0xf97316 },
        conflict: Kind { base: 0xf2555a, mark: 0xff6b70 },
        resolved: Kind { base: 0x8f7a8c, mark: 0x977f93 },
        equivalent: Kind { base: 0x14b8a6, mark: 0x2dd4bf },
        ignored: Kind { base: 0x94a3b8, mark: 0x94a3b8 },
        zone: 0.09,
        current: 0.20,
        word: 0.26,
        selection: alpha(0x5aa9ff, 0.28),
        found: alpha(0xf5b941, 0.24),
        frame: alpha(0xffffff, 0.3),
        control: alpha(0xffffff, 0.08),
    }
}

/// Light code on paper: Ardoise et papier, and Verre's light code.
fn paper_code(canvas: u32) -> CodeSpec {
    CodeSpec {
        canvas: hex(canvas),
        dark: false,
        text: 0x1f2328,
        number: hex(0x656a73),
        number_on: hex(0x24292f),
        change: Kind { base: 0x7c3aed, mark: 0x7c3aed },
        insert: Kind { base: 0x1a7f37, mark: 0x1a7f37 },
        delete: Kind { base: 0xd9560b, mark: 0xd9560b },
        conflict: Kind { base: 0xd1365a, mark: 0xd1365a },
        resolved: Kind { base: 0xa47b8d, mark: 0xa47b8d },
        equivalent: Kind { base: 0x1b7f78, mark: 0x1b7f78 },
        ignored: Kind { base: 0x9da2b3, mark: 0x9da2b3 },
        zone: 0.07,
        current: 0.14,
        word: 0.2,
        selection: hex(0xb6d6fd),
        found: hex(0xffe08a),
        frame: hex(0x7f8290),
        control: hex(0xffffff),
    }
}

/// Dark code for the themes whose code is light by default.
fn slate_code() -> CodeSpec {
    CodeSpec {
        canvas: hex(0x16191f),
        dark: true,
        text: 0xe8ebf0,
        number: hex(0x8a93a3),
        number_on: hex(0xd5dae2),
        change: Kind { base: 0xa78bfa, mark: 0xa78bfa },
        insert: Kind { base: 0x4ade80, mark: 0x4ade80 },
        delete: Kind { base: 0xfb923c, mark: 0xfb923c },
        conflict: Kind { base: 0xf87171, mark: 0xf87171 },
        resolved: Kind { base: 0x8f7a8c, mark: 0x977f93 },
        equivalent: Kind { base: 0x2dd4bf, mark: 0x2dd4bf },
        ignored: Kind { base: 0x94a3b8, mark: 0x94a3b8 },
        zone: 0.09,
        current: 0.17,
        word: 0.24,
        selection: hex(0x33467c),
        found: hex(0x4a3a0a),
        frame: hex(0x5b6475),
        control: hex(0x1f232b),
    }
}

/// Graphite: blue changes on near black.
fn mono_code() -> CodeSpec {
    CodeSpec {
        canvas: hex(0x111112),
        dark: true,
        text: 0xededed,
        number: hex(0x8a8a90),
        number_on: hex(0xededed),
        change: Kind { base: 0x3b82f6, mark: 0x60a5fa },
        insert: Kind { base: 0x22c55e, mark: 0x4ade80 },
        delete: Kind { base: 0xf97316, mark: 0xfb923c },
        conflict: Kind { base: 0xf43f5e, mark: 0xfb7185 },
        resolved: Kind { base: 0x7d7d85, mark: 0x85858d },
        equivalent: Kind { base: 0x14b8a6, mark: 0x2dd4bf },
        ignored: Kind { base: 0x8f8f94, mark: 0x8f8f94 },
        zone: 0.07,
        current: 0.15,
        word: 0.24,
        selection: hex(0x2b3b5c),
        found: hex(0x45360a),
        frame: hex(0x4a4a50),
        control: hex(0x1c1c1e),
    }
}

fn mono_light_code() -> CodeSpec {
    CodeSpec {
        canvas: hex(0xfafafa),
        dark: false,
        text: 0x18181b,
        number: hex(0x66666c),
        number_on: hex(0x18181b),
        change: Kind { base: 0x2563eb, mark: 0x2563eb },
        insert: Kind { base: 0x16a34a, mark: 0x15803d },
        delete: Kind { base: 0xea580c, mark: 0xc2410c },
        conflict: Kind { base: 0xe11d48, mark: 0xbe123c },
        resolved: Kind { base: 0x8b8b93, mark: 0x8b8b93 },
        equivalent: Kind { base: 0x0f766e, mark: 0x0f766e },
        ignored: Kind { base: 0x9a9aa0, mark: 0x9a9aa0 },
        zone: 0.07,
        current: 0.14,
        word: 0.2,
        selection: hex(0xbfdbfe),
        found: hex(0xfde68a),
        frame: hex(0x8a8a90),
        control: hex(0xffffff),
    }
}

/// The first palettes of BioMerge, solid colours throughout.
fn classic_code(dark: bool) -> DocPalette {
    let tint = |zone: u32, gutter: u32, text: u32, mark: u32, current: u32| Tint {
        zone: hex(zone),
        gutter: hex(gutter),
        text: hex(text),
        mark: hex(mark),
        current: hex(current),
    };
    if dark {
        DocPalette {
            canvas: hex(0x1a1b26),
            backdrop: hex(0x1a1b26),
            dark: true,
            text: hex(0xc0caf5),
            line_number: hex(0x7d87ba),
            line_number_on: hex(0xa9b1d6),
            frame: hex(0x5a6390),
            selection: hex(0x283760),
            found: hex(0x453607),
            control: hex(0x24283b),
            control_text: hex(0xc0caf5),
            insert: tint(0x1b2925, 0x263f37, 0x1b3a2c, 0x9ece6a, 0x1f3329),
            delete: tint(0x2c221c, 0x452f20, 0x452c18, 0xff9e64, 0x38291f),
            change: tint(0x211f34, 0x322d54, 0x352b5c, 0xbb9af7, 0x2a2645),
            ignored: tint(0x1e1f2a, 0x282a37, 0x2b2d3b, 0x6b7089, 0x232533),
            equivalent: tint(0x1a2a2d, 0x213a3c, 0x1f3b3d, 0x73daca, 0x1d3236),
            conflict: tint(0x2e1c27, 0x4b2536, 0x4e2236, 0xf7768e, 0x3a2030),
            resolved: tint(0x251f2d, 0x382f44, 0x372e42, 0x9d8299, 0x2d2637),
        }
    } else {
        DocPalette {
            canvas: hex(0xffffff),
            backdrop: hex(0xffffff),
            dark: false,
            text: hex(0x1f2328),
            line_number: hex(0x6e7485),
            line_number_on: hex(0x3d4352),
            frame: hex(0x7f8290),
            selection: hex(0xb6d6fd),
            found: hex(0xffe08a),
            control: hex(0xffffff),
            control_text: hex(0x23262f),
            insert: tint(0xf0faf0, 0xcdeccd, 0xc2ecc5, 0x2ea043, 0xe2f5e3),
            delete: tint(0xfef6ee, 0xf9e0c8, 0xf8dcc0, 0xe07b1f, 0xfcebda),
            change: tint(0xf6f3fc, 0xdfd5f1, 0xe2d6f8, 0x8250df, 0xede6fa),
            ignored: tint(0xf6f6f8, 0xe8e9ee, 0xe3e4ea, 0x9da2b3, 0xeeeef2),
            equivalent: tint(0xeff8f7, 0xd3ecea, 0xc9e9e6, 0x1b7f78, 0xe1f2f0),
            conflict: tint(0xfdf0f3, 0xf7d3dc, 0xf8d6de, 0xd1365a, 0xfae3e9),
            resolved: tint(0xf9f6f8, 0xede5eb, 0xebe2e9, 0xa47b8d, 0xf2ecf0),
        }
    }
}

/// The code palette of a theme in a tone, painted on `card` over `under`.
fn code_palette(kind: ThemeKind, dark: bool, card: Color32, under: Color32) -> (DocPalette, SyntaxTheme) {
    let solid_card = over(card, under);
    let spec = match (kind, dark) {
        (ThemeKind::TokyoNight | ThemeKind::Clair, _) => {
            return (classic_code(dark), if dark { SyntaxTheme::Dark } else { SyntaxTheme::Light });
        }
        (ThemeKind::Verre, true) => (glass_code(), SyntaxTheme::Glass),
        (ThemeKind::Verre | ThemeKind::Ardoise, false) => (paper_code(0xfbfbf9), SyntaxTheme::Paper),
        (ThemeKind::Porcelaine, false) => (paper_code(0xffffff), SyntaxTheme::Paper),
        (ThemeKind::Ardoise | ThemeKind::Porcelaine, true) => (slate_code(), SyntaxTheme::Slate),
        (ThemeKind::Graphite, true) => (mono_code(), SyntaxTheme::Mono),
        (ThemeKind::Graphite, false) => (mono_light_code(), SyntaxTheme::MonoLight),
    };
    (spec.0.palette(solid_card), spec.1)
}

/// Everything a theme decides, for the settings in force.
#[derive(Clone, Copy, Debug)]
pub struct Look {
    pub ui: UiPalette,
    /// The code areas.
    pub doc: DocPalette,
    /// The kinds of change as the chrome shows them, between and beside the panes: ribbons,
    /// overview, counts. A dark interface keeps its own colours around light code.
    pub gap: DocPalette,
    pub syntax: SyntaxTheme,
    pub metrics: Metrics,
    pub links: LinkStyle,
    /// The window lets the desktop show through.
    pub translucent: bool,
}

impl Look {
    pub fn new(kind: ThemeKind, tone: CodeTone, transparent: bool, links: LinkStyle) -> Self {
        Self::on_wallpaper(kind, tone, transparent, links, VERRE_WALLPAPER)
    }

    /// As [`Look::new`], with what the glass of a translucent window shows behind it.
    pub fn on_wallpaper(kind: ThemeKind, tone: CodeTone, transparent: bool, links: LinkStyle, wallpaper: Color32) -> Self {
        let translucent = transparent && kind == ThemeKind::Verre;
        let ui = UiPalette::of(kind, translucent);
        let dark_code = match tone {
            CodeTone::Theme => kind.dark_code(),
            CodeTone::Light => false,
            CodeTone::Dark => true,
        };
        let behind = over(ui.background, over(ui.window, wallpaper));
        let (doc, syntax) = code_palette(kind, dark_code, ui.card, behind);
        let (gap, _) = code_palette(kind, kind.dark_chrome(), ui.card, behind);
        Self { ui, doc, gap, syntax, metrics: Metrics::of(kind.chrome()), links, translucent }
    }
}

/// Blends `amount` of `over` on `base`, opacity included: two solid colours give a solid one.
pub fn mix(base: Color32, over: Color32, amount: f32) -> Color32 {
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color32::from_rgba_premultiplied(
        lerp(base.r(), over.r()),
        lerp(base.g(), over.g()),
        lerp(base.b(), over.b()),
        lerp(base.a(), over.a()),
    )
}

/// How glyph coverage becomes opacity, for 3K and 4K screens: unhinted greyscale glyphs, with
/// the slight weight of macOS text. Light-on-dark text takes the linear-light correction,
/// dark-on-light text a touch of weight.
fn transfer_function(dark: bool) -> FontColorTransferFunction {
    if dark {
        FontColorTransferFunction::Gamma(1.0 / 2.2)
    } else {
        FontColorTransferFunction::Gamma(0.85)
    }
}

pub fn apply_style(ctx: &egui::Context, look: &Look) {
    let palette = &look.ui;
    ctx.set_theme(if palette.dark { egui::Theme::Dark } else { egui::Theme::Light });
    let radius = match palette.chrome {
        Chrome::Verre => 12,
        Chrome::Porcelaine => 14,
        Chrome::Graphite => 8,
        Chrome::Ardoise | Chrome::Classic => 10,
    };
    ctx.all_styles_mut(|style| {
        let mut visuals = if palette.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        // Text as the typeface was drawn: no snapping of stems to the pixel grid, glyphs at
        // their exact advances, greyscale coverage only.
        visuals.text_options.font_hinting = false;
        visuals.text_options.subpixel_binning = true;
        visuals.text_options.color_transfer_function = transfer_function(palette.dark);
        visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
        visuals.panel_fill = palette.panel;
        visuals.window_fill = palette.raised;
        visuals.window_stroke = Stroke::new(1.0, palette.border);
        visuals.window_corner_radius = CornerRadius::same(radius);
        visuals.menu_corner_radius = CornerRadius::same(radius.saturating_sub(1));
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
            state.corner_radius = CornerRadius::same((radius / 2 + 1).max(5));
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

/// The band a ribbon fills between two panes: its upper and lower edges sampled at the same
/// abscissas from `x0` to `x1`, `left` and `right` being the block's vertical ranges on either
/// side. Both edges are cubic curves leaving and reaching the panes horizontally, with their
/// control points halfway across, as in the mockups' SVG paths; a side without lines gives a
/// range of zero height, which the band narrows to.
pub fn ribbon_band(x0: f32, x1: f32, left: (f32, f32), right: (f32, f32), samples: usize) -> Vec<(egui::Pos2, egui::Pos2)> {
    let samples = samples.max(2);
    let middle = (x0 + x1) / 2.0;
    let cubic = |t: f32, a: f32, b: f32, c: f32, d: f32| {
        let u = 1.0 - t;
        u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d
    };
    (0..samples)
        .map(|index| {
            let t = index as f32 / (samples - 1) as f32;
            let x = cubic(t, x0, middle, middle, x1);
            let top = cubic(t, left.0, left.0, right.0, right.0);
            let bottom = cubic(t, left.1, left.1, right.1, right.1);
            (egui::pos2(x, top), egui::pos2(x, bottom))
        })
        .collect()
}

/// The vertical extent of a band at `x`, if `x` is within it.
pub fn ribbon_span_at(band: &[(egui::Pos2, egui::Pos2)], x: f32) -> Option<(f32, f32)> {
    band.windows(2).find_map(|pair| {
        let (a, b) = (pair[0], pair[1]);
        if x < a.0.x || x > b.0.x {
            return None;
        }
        let t = if b.0.x > a.0.x { (x - a.0.x) / (b.0.x - a.0.x) } else { 0.0 };
        Some((a.0.y + (b.0.y - a.0.y) * t, a.1.y + (b.1.y - a.1.y) * t))
    })
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

    /// Every look: each theme with its code in both tones, Verre also opaque, and a translucent
    /// Verre over a light grey wallpaper as well as the dark one.
    fn looks() -> Vec<(String, Look)> {
        let mut looks = Vec::new();
        for kind in ThemeKind::ALL {
            for tone in [CodeTone::Light, CodeTone::Dark] {
                for transparent in [true, false] {
                    if !transparent && kind != ThemeKind::Verre {
                        continue;
                    }
                    let name = format!("{} {tone:?} transparent={transparent}", kind.name());
                    looks.push((name, Look::new(kind, tone, transparent, LinkStyle::Ribbons)));
                }
            }
        }
        looks
    }

    /// The text of the interface stays readable even on a mid-grey wallpaper.
    fn interface_looks() -> Vec<(String, Look)> {
        let mut looks = looks();
        for tone in [CodeTone::Light, CodeTone::Dark] {
            let look = Look::on_wallpaper(ThemeKind::Verre, tone, true, LinkStyle::Ribbons, hex(0x808080));
            looks.push((format!("Verre {tone:?} on grey"), look));
        }
        looks
    }

    fn canvas(doc: &DocPalette) -> Color32 {
        over(doc.canvas, doc.backdrop)
    }

    fn tints(doc: &DocPalette) -> [Tint; 7] {
        [doc.insert, doc.delete, doc.change, doc.ignored, doc.equivalent, doc.conflict, doc.resolved]
    }

    /// Every contrast below `minimum`, named, so that one run lists all that must change.
    struct Checks(Vec<String>);

    impl Checks {
        fn need(&mut self, minimum: f32, text: Color32, background: Color32, what: impl Fn() -> String) {
            let ratio = contrast(text, background);
            if ratio < minimum {
                self.0.push(format!("{}: {ratio:.2} < {minimum} ({text:?} on {background:?})", what()));
            }
        }

        fn done(self) {
            assert!(self.0.is_empty(), "{}", self.0.join("\n"));
        }
    }

    #[test]
    fn text_stays_readable_on_every_zone_of_every_theme() {
        let mut checks = Checks(Vec::new());
        for (name, look) in looks() {
            let doc = look.doc;
            let base = canvas(&doc);
            let mut backgrounds = vec![("canvas".to_owned(), base), ("found".to_owned(), over(doc.found, base)), ("selection".to_owned(), over(doc.selection, base))];
            let kinds = ["insert", "delete", "change", "ignored", "equivalent", "conflict", "resolved"];
            for (kind, tint) in kinds.into_iter().zip(tints(&doc)) {
                for (part, color) in [("zone", tint.zone), ("gutter", tint.gutter), ("word", tint.text), ("current", tint.current)] {
                    backgrounds.push((format!("{kind} {part}"), over(color, base)));
                }
            }
            for (what, background) in backgrounds {
                checks.need(7.0, doc.text, background, || format!("{name} text on {what}"));
            }
        }
        checks.done();
    }

    #[test]
    fn line_numbers_stay_readable_on_their_rows() {
        let mut checks = Checks(Vec::new());
        for (name, look) in interface_looks() {
            let doc = look.doc;
            let base = canvas(&doc);
            checks.need(4.5, over(doc.line_number, base), base, || format!("{name} number"));
            for tint in tints(&doc) {
                for zone in [tint.zone, tint.current] {
                    let row = over(zone, base);
                    checks.need(4.5, over(doc.line_number_on, row), row, || format!("{name} number on a change"));
                }
            }
        }
        checks.done();
    }

    #[test]
    fn interface_text_and_faint_labels_stay_readable() {
        let mut checks = Checks(Vec::new());
        for (name, look) in interface_looks() {
            let ui = look.ui;
            let behind = over(ui.background, over(ui.window, look.doc.backdrop));
            let card = over(ui.card, behind);
            for (surface_name, surface) in [("window", behind), ("card", card), ("raised", ui.raised)] {
                checks.need(7.0, over(ui.text, surface), surface, || format!("{name} text on {surface_name}"));
                checks.need(4.5, over(ui.text_dim, surface), surface, || format!("{name} dim on {surface_name}"));
                checks.need(4.5, over(ui.text_faint, surface), surface, || format!("{name} faint on {surface_name}"));
            }
            let lit = over(ui.lit, behind);
            checks.need(4.5, over(ui.lit_text, lit), lit, || format!("{name} lit"));
        }
        checks.done();
    }

    #[test]
    fn every_code_color_stays_readable_on_the_darkest_backgrounds() {
        let mut checks = Checks(Vec::new());
        for (name, look) in looks() {
            let doc = look.doc;
            let base = canvas(&doc);
            for [r, g, b] in biomerge_core::highlight::theme_colors(look.syntax) {
                let color = Color32::from_rgb(r, g, b);
                for (index, tint) in [doc.insert, doc.delete, doc.change, doc.conflict, doc.resolved].into_iter().enumerate() {
                    for background in [tint.text, tint.current] {
                        let background = over(background, base);
                        checks.need(4.5, color, background, || format!("{name} {:?} code on {}", look.syntax, ["insert", "delete", "change", "conflict", "resolved"][index]));
                    }
                }
            }
        }
        checks.done();
    }

    #[test]
    fn light_code_under_a_dark_interface_takes_its_own_colours() {
        let look = Look::new(ThemeKind::Ardoise, CodeTone::Theme, true, LinkStyle::Ribbons);
        assert!(look.ui.dark && !look.doc.dark);
        assert_eq!(look.syntax, SyntaxTheme::Paper);
        // What surrounds the code keeps the interface's tone.
        assert!(look.gap.dark);
        let dark = Look::new(ThemeKind::Ardoise, CodeTone::Dark, true, LinkStyle::Ribbons);
        assert!(dark.doc.dark && dark.syntax != look.syntax);
        let verre = Look::new(ThemeKind::Verre, CodeTone::Light, true, LinkStyle::Ribbons);
        assert!(!verre.doc.dark && verre.doc.canvas.is_opaque());
    }

    #[test]
    fn an_unresolved_conflict_stands_out_more_than_a_resolved_one() {
        // Hue matters here, not lightness alone: the distance from the canvas in RGB.
        let distance = |a: Color32, b: Color32| {
            let d = |x: u8, y: u8| (x as f32 - y as f32).powi(2);
            (d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())).sqrt()
        };
        for (name, look) in looks() {
            let doc = look.doc;
            let base = canvas(&doc);
            let shown = |color: Color32| over(color, base);
            for (conflict, resolved) in [(doc.conflict.zone, doc.resolved.zone), (doc.conflict.mark, doc.resolved.mark)] {
                let (strong, calm) = (distance(base, shown(conflict)), distance(base, shown(resolved)));
                assert!(strong > calm * 1.2, "{name}: {strong} against {calm} ({conflict:?}, {resolved:?})");
            }
        }
    }

    #[test]
    fn darker_text_background_and_current_block_stand_out_from_their_zone() {
        for (name, look) in looks() {
            let doc = look.doc;
            let base = canvas(&doc);
            for tint in [doc.insert, doc.delete, doc.change, doc.conflict] {
                let zone = over(tint.zone, base);
                let (word, current) = (contrast(zone, over(tint.text, base)), contrast(zone, over(tint.current, base)));
                assert!(word >= 1.15 && current >= 1.05, "{name}: {word} {current} {:?}", tint.mark);
            }
        }
    }

    #[test]
    fn the_old_default_becomes_verre_and_the_old_light_choice_keeps_its_look() {
        assert_eq!(ThemeKind::from_stored(None), ThemeKind::Verre);
        assert_eq!(ThemeKind::from_stored(Some("dark")), ThemeKind::Verre);
        assert_eq!(ThemeKind::from_stored(Some("light")), ThemeKind::Clair);
        assert_eq!(ThemeKind::from_stored(Some("something else")), ThemeKind::Verre);
        for kind in ThemeKind::ALL {
            assert_eq!(ThemeKind::from_stored(Some(kind.key())), kind);
        }
    }

    #[test]
    fn only_verre_is_translucent_and_only_when_asked() {
        for kind in ThemeKind::ALL {
            let look = Look::new(kind, CodeTone::Theme, true, LinkStyle::Ribbons);
            assert_eq!(look.translucent, kind == ThemeKind::Verre, "{kind:?}");
            assert_eq!(look.ui.window.is_opaque(), kind != ThemeKind::Verre, "{kind:?}");
            assert!(Look::new(kind, CodeTone::Theme, false, LinkStyle::Ribbons).ui.window.is_opaque());
        }
    }

    #[test]
    fn a_ribbon_joins_the_two_ranges_and_never_crosses_itself() {
        let band = ribbon_band(100.0, 160.0, (10.0, 54.0), (80.0, 80.0), 17);
        let (first, last) = (band[0], band[band.len() - 1]);
        assert_eq!((first.0.x, first.0.y, first.1.y), (100.0, 10.0, 54.0));
        assert_eq!((last.0.x, last.0.y, last.1.y), (160.0, 80.0, 80.0));
        // Halfway across, each edge is halfway between its two ends.
        let middle = band[8];
        assert!((middle.0.x - 130.0).abs() < 1e-3);
        assert!((middle.0.y - 45.0).abs() < 1e-3 && (middle.1.y - 67.0).abs() < 1e-3);
        for (top, bottom) in &band {
            assert!(top.y <= bottom.y);
        }
        assert!(band.windows(2).all(|pair| pair[0].0.x < pair[1].0.x && pair[0].0.y <= pair[1].0.y));
        // It leaves and reaches the panes horizontally.
        assert!((band[1].0.y - 10.0).abs() < 1.0 && (band[15].0.y - 80.0).abs() < 1.0);
        let (top, bottom) = ribbon_span_at(&band, 130.0).unwrap();
        assert!((top - 45.0).abs() < 0.5 && (bottom - 67.0).abs() < 0.5);
        assert_eq!(ribbon_span_at(&band, 90.0), None);
    }

    #[test]
    fn mixing_keeps_solid_colours_solid_and_blends_opacity() {
        let solid = mix(hex(0x000000), hex(0xffffff), 0.5);
        assert!(solid.is_opaque());
        assert_eq!(solid.r(), 128);
        let half = mix(Color32::TRANSPARENT, hex(0xffffff), 0.5);
        assert_eq!(half.a(), 128);
    }
}
