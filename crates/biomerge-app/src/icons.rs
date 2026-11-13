//! SVG icons, rasterized at the exact pixel size they are shown at and drawn 1:1 on the pixel
//! grid, so they stay crisp at any scale. The files are white: the colour tints them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{Color32, ColorImage, Context, Id, Painter, Rect, SizeHint, TextureHandle, TextureOptions, Vec2, pos2};

macro_rules! icons {
    ($($variant:ident = $file:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Icon { $($variant),* }

        impl Icon {
            fn svg(self) -> &'static [u8] {
                match self { $(Icon::$variant => include_bytes!(concat!("../assets/icons/", $file, ".svg"))),* }
            }

            #[cfg(test)]
            const ALL: &[Icon] = &[$(Icon::$variant),*];
        }
    };
}

icons! {
    AccentE = "accent-e", AccentMark = "accent-mark", Align = "align", ArrowDown = "arrow-down", ArrowLeft = "arrow-left", ArrowRight = "arrow-right",
    ArrowUp = "arrow-up", Braces = "braces", Case = "case", ChevronRight = "chevron-right", ColorScale = "color-scale",
    Combine = "combine", Command = "command", Compare = "compare", Copy = "copy", CopyRight = "copy-right", Cross = "cross", Ellipsis = "ellipsis", EyeOff = "eye-off", File = "file", FileText = "file-text", Folder = "folder",
    FolderOpen = "folder-open",
    Fold = "fold", Font = "font", Funnel = "funnel", GitBranch = "git-branch", GitMerge = "git-merge", Goto = "goto", Hamburger = "hamburger", History = "history", Ignore = "ignore", Info = "info",
    List = "list", Lock = "lock", Magnifier = "magnifier", MenuCheck = "menu-check", Mirror = "mirror", Open = "open", PushRight = "push-right", Quit = "quit", Redo = "redo", Regex = "regex", Reload = "reload",
    Save = "save", Swap = "swap", Sync = "sync", Trash = "trash", Undo = "undo", Warning = "warning", WholeWord = "whole-word", Whitespace = "whitespace",
    Wrap = "wrap",
}

type Cache = Arc<Mutex<HashMap<(Icon, [u32; 2]), TextureHandle>>>;

fn rasterize(icon: Icon, pixels: [u32; 2]) -> Option<ColorImage> {
    let hint = SizeHint::Size {
        width: pixels[0],
        height: pixels[1],
        maintain_aspect_ratio: true,
    };
    egui_extras::image::load_svg_bytes_with_size(icon.svg(), hint, &Default::default()).ok()
}

fn texture(ctx: &Context, icon: Icon, pixels: [u32; 2]) -> Option<TextureHandle> {
    let id = Id::unique("biomerge-icons");
    let cache: Cache = ctx.data_mut(|data| data.get_temp_mut_or_default::<Cache>(id).clone());
    let mut cache = cache.lock().ok()?;
    if let Some(handle) = cache.get(&(icon, pixels)) {
        return Some(handle.clone());
    }
    let image = rasterize(icon, pixels)?;
    let handle = ctx.load_texture(format!("icon-{icon:?}-{pixels:?}"), image, TextureOptions::NEAREST);
    cache.insert((icon, pixels), handle.clone());
    Some(handle)
}

fn draw(painter: &Painter, icon: Icon, rect: Rect, color: Color32, mirrored: bool) {
    let scale = painter.pixels_per_point();
    let size = (rect.size() * scale).round();
    let pixels = [size.x as u32, size.y as u32];
    if pixels.contains(&0) {
        return;
    }
    let Some(handle) = texture(painter.ctx(), icon, pixels) else {
        return;
    };
    let shown = handle.size_vec2();
    // On whole pixels, so that each texel lands on exactly one pixel.
    let min = ((rect.center().to_vec2() * scale - shown / 2.0).round() / scale).to_pos2();
    let uv = if mirrored {
        Rect::from_min_max(pos2(1.0, 0.0), pos2(0.0, 1.0))
    } else {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
    };
    painter.image(handle.id(), Rect::from_min_size(min, shown / scale), uv, color);
}

pub fn paint(painter: &Painter, icon: Icon, rect: Rect, color: Color32) {
    draw(painter, icon, rect, color, false);
}

/// Left-pointing variant of an icon drawn pointing right.
pub fn paint_mirrored(painter: &Painter, icon: Icon, rect: Rect, color: Color32) {
    draw(painter, icon, rect, color, true);
}

pub fn square(center: egui::Pos2, side: f32) -> Rect {
    Rect::from_center_size(center, Vec2::splat(side))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_rasterizes_at_the_requested_size() {
        for &icon in Icon::ALL {
            let image = rasterize(icon, [28, 28]).unwrap_or_else(|| panic!("{icon:?}"));
            assert_eq!(image.size, [28, 28], "{icon:?}");
            assert!(image.pixels.iter().any(|p| p.a() > 0), "{icon:?} is empty");
        }
    }
}
