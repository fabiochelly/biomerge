//! What the comparison views share, styled by the theme: the cards of the panes, the links or
//! ribbons between them, the arrows of a block and the overview of the changes.

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::icons::{self, Icon};
use crate::theme::{Chrome, Look, RibbonStyle, alpha, ribbon_band};

/// Empty space inside a code area, above its first row and below its last one.
pub const CODE_PADDING: f32 = 10.0;
/// Points sampled along each edge of a ribbon: smooth at any height.
const RIBBON_SAMPLES: usize = 24;

/// One pane: its whole card, its header and the area of its code.
#[derive(Clone, Copy, Debug)]
pub struct PaneRects {
    pub card: Rect,
    pub header: Rect,
    pub code: Rect,
}

/// Lays out `count` panes side by side in `area`, the overview strip on the right if wanted.
pub fn layout(area: Rect, look: &Look, count: usize, overview: bool) -> (Vec<PaneRects>, Rect) {
    let metrics = look.metrics;
    let content = Rect::from_min_max(
        pos2(area.left() + metrics.margin.x, area.top() + metrics.margin_top),
        pos2(area.right() - metrics.margin.x, area.bottom() - metrics.margin.y),
    );
    let strip = if overview { metrics.overview_width + metrics.overview_margin } else { 0.0 };
    let gaps = metrics.gap * count.saturating_sub(1) as f32;
    let width = ((content.width() - gaps - strip) / count as f32).max(120.0).floor();
    let panes: Vec<PaneRects> = (0..count)
        .map(|index| {
            let x = content.left() + index as f32 * (width + metrics.gap);
            let card = Rect::from_min_size(pos2(x, content.top()), vec2(width, content.height()));
            let header = Rect::from_min_size(card.min, vec2(width, metrics.header_height));
            let code = Rect::from_min_max(pos2(card.left(), header.bottom()), card.max);
            PaneRects { card, header, code }
        })
        .collect();
    let right = panes.last().map_or(content.left(), |pane| pane.card.right());
    let (top, bottom) = match look.ui.chrome {
        Chrome::Ardoise | Chrome::Porcelaine => (content.top() + metrics.header_height, content.bottom()),
        _ => (content.top(), content.bottom()),
    };
    let strip = Rect::from_min_max(
        pos2(right + metrics.overview_margin, top),
        pos2(right + metrics.overview_margin + metrics.overview_width, bottom),
    );
    (panes, strip)
}

/// The corners of the code area: all of them when its header stands outside, the bottom ones
/// otherwise.
fn code_radius(look: &Look) -> CornerRadius {
    let radius = look.metrics.card_radius;
    if look.metrics.header_outside {
        CornerRadius::same(radius)
    } else {
        CornerRadius { nw: 0, ne: 0, sw: radius, se: radius }
    }
}

/// Cards and code areas, before anything is painted in them.
pub fn paint_cards(painter: &Painter, look: &Look, panes: &[PaneRects]) {
    let ui = &look.ui;
    let radius = look.metrics.card_radius;
    for pane in panes {
        match ui.chrome {
            Chrome::Classic => {
                painter.add(shadow(0, 3, 14, 46).as_shape(pane.card, radius));
            }
            Chrome::Porcelaine => {
                painter.add(shadow(0, 10, 30, 18).as_shape(pane.card, radius));
                painter.add(shadow(0, 1, 2, 13).as_shape(pane.card, radius));
            }
            Chrome::Ardoise => {
                painter.add(shadow(0, 12, 32, 115).as_shape(pane.code, radius));
            }
            Chrome::Verre | Chrome::Graphite => {}
        }
        painter.rect_filled(pane.card, radius, ui.card);
        if pane.code.height() > 0.0 && look.doc.canvas != ui.card && look.doc.canvas.a() > 0 {
            painter.rect_filled(pane.code, code_radius(look), look.doc.canvas);
        }
    }
}

/// The rectangle rows may paint in: the code area, short of rounded corners nothing can mask.
pub fn row_clip(look: &Look, code: Rect) -> Rect {
    let radius = look.metrics.card_radius as f32;
    if radius > 0.0 && look.ui.solid_behind_cards().is_none() {
        code.with_max_y(code.bottom() - radius * 0.6)
    } else {
        code
    }
}

/// Over what was painted in the cards: the corners rows overflowed, then the borders.
pub fn finish_cards(painter: &Painter, look: &Look, panes: &[PaneRects]) {
    let ui = &look.ui;
    let radius = look.metrics.card_radius;
    for pane in panes {
        if let Some(behind) = ui.solid_behind_cards().filter(|_| radius > 0) {
            crate::widgets::mask_bottom_corners(painter, pane.code, radius as f32, behind);
            if look.metrics.header_outside {
                crate::widgets::mask_corners(painter, pane.code, radius as f32, behind, true);
            }
        }
        let outline = if look.metrics.header_outside { pane.code } else { pane.card };
        match ui.chrome {
            Chrome::Graphite => {
                for x in [pane.card.left(), pane.card.right()] {
                    painter.vline(painter.round_to_pixel_center(x), pane.card.y_range(), Stroke::new(1.0, ui.card_border));
                }
            }
            _ => {
                painter.rect_stroke(outline, radius, Stroke::new(1.0, ui.card_border), StrokeKind::Inside);
            }
        }
    }
}

fn shadow(x: i8, y: i8, blur: u8, opacity: u8) -> egui::Shadow {
    egui::Shadow { offset: [x, y], blur, spread: 0, color: Color32::from_black_alpha(opacity) }
}

/// A translucent band joining a block's range on one side of a gap to its range on the other,
/// with a slightly stronger edge. `current`: the block being worked on, more strongly tinted.
pub fn paint_ribbon(painter: &Painter, x: (f32, f32), left: (f32, f32), right: (f32, f32), mark: Color32, style: RibbonStyle, current: bool) {
    let band = ribbon_band(x.0, x.1, left, right, RIBBON_SAMPLES);
    let [r, g, b, _] = mark.to_array();
    let tinted = |opacity: f32| Color32::from_rgba_unmultiplied(r, g, b, (opacity * 255.0).round() as u8);
    let fill = tinted(if current { style.current_fill } else { style.fill });
    let mut mesh = egui::Mesh::default();
    for (index, (top, bottom)) in band.iter().enumerate() {
        mesh.colored_vertex(*top, fill);
        mesh.colored_vertex(*bottom, fill);
        if index > 0 {
            let base = (index as u32 - 1) * 2;
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base + 1, base + 3, base + 2);
        }
    }
    painter.add(Shape::mesh(mesh));
    let stroke = if current { Stroke::new(1.6, mark) } else { Stroke::new(1.0, tinted(style.edge)) };
    let mut outline: Vec<Pos2> = band.iter().map(|(top, _)| *top).collect();
    outline.extend(band.iter().rev().map(|(_, bottom)| *bottom));
    painter.add(Shape::closed_line(outline, stroke));
}

/// Whether `pos` falls on the band of a ribbon, with a little tolerance for a thin one.
pub fn ribbon_contains(x: (f32, f32), left: (f32, f32), right: (f32, f32), pos: Pos2) -> bool {
    if pos.x < x.0 || pos.x > x.1 {
        return false;
    }
    let band = ribbon_band(x.0, x.1, left, right, RIBBON_SAMPLES);
    crate::theme::ribbon_span_at(&band, pos.x).is_some_and(|(top, bottom)| pos.y >= top - 4.0 && pos.y <= bottom + 4.0)
}

/// Where the arrow of a block stands in a gap: against the pane it sends from, on that side's
/// part of the block.
pub fn arrow_rect(look: &Look, gap: (f32, f32), from_left: bool, y: f32) -> Rect {
    let size = look.ui.arrow.size;
    let x = if from_left { gap.0 + 1.0 + size / 2.0 } else { gap.1 - 1.0 - size / 2.0 };
    Rect::from_center_size(pos2(x, y), vec2(size, size))
}

/// An arrow's colours from its block's colour: the same hue made opaque, so the button sits
/// on the ribbon or the link instead of letting it show through, and a glyph that reads on it.
pub fn arrow_colours(tint: Color32) -> (Color32, Color32, Color32, Color32) {
    let [r, g, b, _] = tint.to_srgba_unmultiplied();
    let fill = Color32::from_rgb(r, g, b);
    let hover = crate::theme::mix(fill, Color32::WHITE, 0.18);
    let border = crate::theme::mix(fill, Color32::WHITE, 0.35);
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    let icon = if luma > 160.0 { Color32::from_rgb(0x14, 0x16, 0x1c) } else { Color32::WHITE };
    (fill, hover, border, icon)
}

/// The button sending a block across, in the theme's shape and the block's colour. `mirrored`
/// points it left. Paint it after every ribbon and link, so that it stays on top of them.
#[allow(clippy::too_many_arguments)]
pub fn arrow_button(ui: &Ui, painter: &Painter, look: &Look, rect: Rect, id: egui::Id, mirrored: bool, tip: &str, tint: Color32) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tip));
    let style = look.ui.arrow;
    let hovered = response.hovered();
    let (base, lighter, border, icon) = arrow_colours(tint);
    let fill = if hovered { lighter } else { base };
    if style.glow {
        let [r, g, b, _] = base.to_array();
        let opacity = if hovered { 150 } else { 110 };
        let halo = Color32::from_rgba_unmultiplied(r, g, b, opacity);
        painter.add(egui::Shadow { offset: [0, 0], blur: 14, spread: 0, color: halo }.as_shape(rect, style.radius as u8));
    } else {
        painter.add(shadow(0, 3, 10, if look.ui.dark { 110 } else { 46 }).as_shape(rect, style.radius as u8));
    }
    painter.rect(rect, style.radius, fill, Stroke::new(1.0, border), StrokeKind::Inside);
    let glyph = icons::square(rect.center(), (style.size * 0.5).round());
    if mirrored {
        icons::paint_mirrored(painter, Icon::ArrowRight, glyph, icon);
    } else {
        icons::paint(painter, Icon::ArrowRight, glyph, icon);
    }
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// The overview strip: its track, a mark per block and where the view stands.
pub fn paint_overview(painter: &Painter, look: &Look, rect: Rect, marks: &[(f32, f32, Color32)], thumb: Rect) {
    let ui = &look.ui;
    let (inset, round, min_height) = match ui.chrome {
        Chrome::Verre => (2.0, 3.0, 4.0),
        Chrome::Graphite => (3.0, 0.0, 3.0),
        Chrome::Ardoise | Chrome::Porcelaine => (1.0, 3.0, 4.0),
        Chrome::Classic => (0.0, 0.0, 2.0),
    };
    match ui.chrome {
        Chrome::Verre => {
            painter.rect_filled(rect, 5.0, alpha(0xffffff, 0.05));
        }
        Chrome::Graphite => {
            painter.vline(painter.round_to_pixel_center(rect.left()), rect.y_range(), Stroke::new(1.0, ui.card_border));
        }
        Chrome::Ardoise => {
            painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(0x19, 0x1c, 0x22));
        }
        Chrome::Porcelaine => {
            painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(0xe6, 0xe6, 0xe0));
        }
        Chrome::Classic => {
            painter.rect_filled(rect, 4.0, ui.panel);
        }
    }
    let x_range = rect.left() + inset..=rect.right() - inset;
    let mut last_bottom = f32::MIN;
    for &(top, bottom, color) in marks {
        let bottom = bottom.max(top + min_height);
        if bottom <= last_bottom {
            continue;
        }
        let mark = Rect::from_x_y_ranges(x_range.clone(), top.max(last_bottom)..=bottom);
        if ui.chrome == Chrome::Verre {
            let [r, g, b, _] = color.to_array();
            let glow = Color32::from_rgba_unmultiplied(r, g, b, 90);
            painter.add(egui::Shadow { offset: [0, 0], blur: 6, spread: 0, color: glow }.as_shape(mark, round as u8));
        }
        painter.rect_filled(mark, round, color);
        last_bottom = bottom + if round > 0.0 { 1.0 } else { 0.0 };
    }
    if ui.chrome == Chrome::Verre {
        painter.rect_stroke(rect.expand(2.0), 6.0, Stroke::new(1.0, alpha(0xffffff, 0.18)), StrokeKind::Inside);
    }
    let [r, g, b, _] = ui.text.to_array();
    let veil = Color32::from_rgba_unmultiplied(r, g, b, if ui.dark { 26 } else { 20 });
    let edge = Color32::from_rgba_unmultiplied(r, g, b, if ui.dark { 90 } else { 80 });
    painter.rect(thumb.expand2(vec2(if ui.chrome == Chrome::Verre { 1.0 } else { 0.0 }, 0.0)), 3.0, veil, Stroke::new(1.0, edge), StrokeKind::Inside);
}

/// The bar beside a changed line, in the colour of its change.
pub fn paint_line_bar(painter: &Painter, look: &Look, x: f32, top: f32, bottom: f32, color: Color32) {
    let round = match look.ui.chrome {
        Chrome::Verre | Chrome::Porcelaine => 1.5,
        _ => 0.0,
    };
    painter.rect_filled(Rect::from_min_max(pos2(x, top), pos2(x + 3.0, bottom)), round, color);
}

/// Where the bar of a changed line goes in a pane whose numbers end at `numbers_right`.
pub fn line_bar_x(look: &Look, pane: Rect, numbers_right: f32) -> f32 {
    if look.metrics.bar_at_edge { pane.left() } else { numbers_right + 8.0 }
}
