//! Fonts, found through fontconfig: the interface in Inter, the code in a monospace face. The
//! weights of the interface are real faces (or a variable font at that weight), never synthesized.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use egui::epaint::text::{FontTweak, VariationCoords};
use egui::{FontData, FontDefinitions, FontFamily};

/// First found wins, as in csvfab.
const CODE_STACK: &[&str] = &[
    "MonoLisa", "MonolisaNerd", "MonoLisa Nerd Font", "MonoLisaCode locked", "Cascadia Code", "Monaco", "JetBrains Mono",
    "JetBrainsMono Nerd Font", "Cascadia Mono", "Consolas", "DejaVu Sans Mono",
];
const UI_STACK: &[&str] = &["Inter", "Roboto", "Noto Sans", "Liberation Sans", "DejaVu Sans"];

pub const UI_MEDIUM: &str = "ui-medium";
pub const UI_SEMIBOLD: &str = "ui-semibold";

const FONTCONFIG_REGULAR: f32 = 80.0;
const FONTCONFIG_MEDIUM: f32 = 100.0;
const FONTCONFIG_SEMIBOLD: f32 = 180.0;

#[derive(Clone, Debug, PartialEq)]
struct Face {
    family: String,
    path: PathBuf,
    index: u32,
    weight: (f32, f32),
    italic: bool,
    declared_mono: bool,
}

impl Face {
    fn is_variable(&self) -> bool {
        self.weight.0 != self.weight.1
    }
}

fn parse_weight(text: &str) -> Option<(f32, f32)> {
    let text = text.trim().trim_start_matches('[').trim_end_matches(']');
    let mut parts = text.split_whitespace();
    let low: f32 = parts.next()?.parse().ok()?;
    let high = parts.next().map_or(Some(low), |p| p.parse().ok())?;
    Some((low, high))
}

fn parse_face(line: &str) -> Option<Face> {
    let mut fields = line.split('\t');
    let family = fields.next()?.to_owned();
    let path = PathBuf::from(fields.next()?);
    // Above 2^16: a named instance of a variable font.
    let index = fields.next()?.parse::<u32>().ok()? & 0xFFFF;
    let weight = parse_weight(fields.next()?)?;
    let slant = fields.next()?;
    let spacing = fields.next().unwrap_or("");
    Some(Face {
        family,
        path,
        index,
        weight,
        italic: slant.trim() != "0",
        declared_mono: spacing.trim() == "100",
    })
}

fn catalogue() -> Vec<Face> {
    let output = Command::new("fc-list")
        .args(["--format", "%{family[0]}\t%{file}\t%{index}\t%{weight}\t%{slant}\t%{spacing}\n"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout).lines().filter_map(parse_face).collect()
}

/// The face of `family` closest to `weight`, on fontconfig's scale.
fn best<'a>(faces: &'a [Face], family: &str, weight: f32) -> Option<&'a Face> {
    let distance = |face: &Face| {
        if weight < face.weight.0 {
            face.weight.0 - weight
        } else if weight > face.weight.1 {
            weight - face.weight.1
        } else {
            0.0
        }
    };
    faces
        .iter()
        .filter(|face| !face.italic && face.family.eq_ignore_ascii_case(family))
        .min_by(|a, b| {
            let key = |face: &Face| (distance(face), face.is_variable());
            key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn css_weight(fontconfig: f32) -> f32 {
    if fontconfig >= FONTCONFIG_SEMIBOLD {
        600.0
    } else if fontconfig >= FONTCONFIG_MEDIUM {
        500.0
    } else {
        400.0
    }
}

fn font_data(face: &Face, weight: f32, files: &mut BTreeMap<PathBuf, Arc<Vec<u8>>>) -> Option<FontData> {
    let bytes = match files.get(&face.path) {
        Some(bytes) => bytes.clone(),
        None => {
            let bytes = Arc::new(std::fs::read(&face.path).ok()?);
            files.insert(face.path.clone(), bytes.clone());
            bytes
        }
    };
    let mut data = FontData::from_blob(bytes, face.index);
    if face.is_variable() {
        data = data.tweak(FontTweak {
            coords: VariationCoords::new([(b"wght", css_weight(weight))]),
            ..FontTweak::default()
        });
    }
    Some(data)
}

/// Installs the interface and code fonts. `code_family`: a family from [`list_monospace`];
/// when absent or no longer installed, the first of the usual ones.
pub fn install(ctx: &egui::Context, code_family: Option<&str>) {
    let faces = catalogue();
    let mut definitions = FontDefinitions::default();
    let mut files = BTreeMap::new();
    let code_base = definitions.families[&FontFamily::Monospace].clone();
    let ui_base = definitions.families[&FontFamily::Proportional].clone();

    let code = code_family
        .into_iter()
        .chain(CODE_STACK.iter().copied())
        .find_map(|family| best(&faces, family, FONTCONFIG_REGULAR));
    let ui_family = UI_STACK.iter().find(|family| best(&faces, family, FONTCONFIG_REGULAR).is_some());

    let mut add = |name: &str, face: Option<&Face>, weight: f32| -> bool {
        let Some(data) = face.and_then(|face| font_data(face, weight, &mut files)) else {
            return false;
        };
        definitions.font_data.insert(name.to_owned(), Arc::new(data));
        true
    };
    let has_code = add("code", code, FONTCONFIG_REGULAR);
    let ui_face = |weight| ui_family.and_then(|family| best(&faces, family, weight));
    let has_ui = add("ui", ui_face(FONTCONFIG_REGULAR), FONTCONFIG_REGULAR);
    let has_medium = add("ui-medium-face", ui_face(FONTCONFIG_MEDIUM), FONTCONFIG_MEDIUM);
    let has_semibold = add("ui-semibold-face", ui_face(FONTCONFIG_SEMIBOLD), FONTCONFIG_SEMIBOLD);

    let chain = |first: &[(bool, &str)], base: &[String]| -> Vec<String> {
        first
            .iter()
            .filter(|(present, _)| *present)
            .map(|(_, name)| name.to_string())
            .chain(base.iter().cloned())
            .collect()
    };
    let families = [
        (FontFamily::Monospace, chain(&[(has_code, "code"), (has_ui, "ui")], &code_base)),
        (FontFamily::Proportional, chain(&[(has_ui, "ui"), (has_code, "code")], &ui_base)),
        (
            FontFamily::Name(UI_MEDIUM.into()),
            chain(&[(has_medium, "ui-medium-face"), (has_ui, "ui")], &ui_base),
        ),
        (
            FontFamily::Name(UI_SEMIBOLD.into()),
            chain(&[(has_semibold, "ui-semibold-face"), (has_ui, "ui")], &ui_base),
        ),
    ];
    for (family, list) in families {
        definitions.families.insert(family, list);
    }
    ctx.set_fonts(definitions);
}

const STYLE_WORDS: &[&str] = &[
    "Thin", "ExtraLight", "Light", "Regular", "Medium", "SemiBold", "Bold", "ExtraBold", "Black",
];

/// True when the Latin letters, digits and punctuation of a font all share one advance:
/// a font's own claim of being monospace is not trusted.
fn is_fixed_width(data: &[u8], index: u32) -> bool {
    let Ok(face) = ttf_parser::Face::parse(data, index) else {
        return false;
    };
    let advance = |c: char| face.glyph_index(c).and_then(|glyph| face.glyph_hor_advance(glyph));
    let Some(reference) = advance('0') else {
        return false;
    };
    "iMWl.,0 _".chars().all(|c| advance(c) == Some(reference))
}

/// The installed monospace families, verified on their glyphs. Slow: not on the interface thread.
pub fn list_monospace() -> Vec<String> {
    let faces = catalogue();
    let looks_mono = |face: &Face| {
        let name = face.family.to_lowercase();
        face.declared_mono || ["mono", "code", "consol", "courier", "term"].iter().any(|w| name.contains(w))
    };
    let mut checked = HashSet::new();
    let mut families = Vec::new();
    for face in faces.iter().filter(|face| !face.italic && looks_mono(face)) {
        if checked.insert(face.family.clone())
            && std::fs::read(&face.path).is_ok_and(|data| is_fixed_width(&data, face.index))
        {
            families.push(face.family.clone());
        }
    }
    // "Aptos Mono Bold" is a weight of "Aptos Mono", not a family of its own.
    let bases: HashSet<String> = families.iter().cloned().collect();
    families.retain(|family| {
        family
            .rsplit_once(' ')
            .is_none_or(|(base, style)| !STYLE_WORDS.contains(&style) || !bases.contains(base))
    });
    families.sort_by_key(|family| family.to_lowercase());
    families
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(family: &str, weight: (f32, f32), italic: bool) -> Face {
        Face {
            family: family.to_owned(),
            path: PathBuf::from(format!("/{family}-{}-{}", weight.0, weight.1)),
            index: 0,
            weight,
            italic,
            declared_mono: false,
        }
    }

    #[test]
    fn fontconfig_lines_are_parsed() {
        let parsed = parse_face("Inter\t/f/Inter.ttc\t65537\t80\t0\t").unwrap();
        assert_eq!(parsed.family, "Inter");
        assert_eq!(parsed.index, 1);
        assert_eq!(parsed.weight, (80.0, 80.0));
        assert!(!parsed.italic && !parsed.declared_mono);
        let variable = parse_face("Code\t/f/c.ttf\t0\t[50 210]\t100\t100").unwrap();
        assert_eq!(variable.weight, (50.0, 210.0));
        assert!(variable.italic && variable.declared_mono && variable.is_variable());
    }

    #[test]
    fn the_exact_upright_face_wins_over_a_variable_one() {
        let faces = [
            face("Inter", (50.0, 210.0), false),
            face("Inter", (80.0, 80.0), true),
            face("Inter", (100.0, 100.0), false),
            face("Inter", (80.0, 80.0), false),
        ];
        assert_eq!(best(&faces, "inter", 80.0), Some(&faces[3]));
        assert_eq!(best(&faces, "Inter", 100.0), Some(&faces[2]));
        assert_eq!(best(&faces, "Inter", 180.0), Some(&faces[0]));
        assert_eq!(best(&faces, "Other", 80.0), None);
    }

    #[test]
    fn a_proportional_font_is_never_listed_as_monospace() {
        let definitions = FontDefinitions::default();
        let bytes = |name: &str| (*definitions.font_data[name].font).as_ref().to_vec();
        assert!(is_fixed_width(&bytes("Hack"), 0));
        assert!(!is_fixed_width(&bytes("Ubuntu-Light"), 0));
    }
}
