//! Background syntax highlighting with syntect's pure-Rust regex engine.

use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use syntect::highlighting::{
    Color, FontStyle, HighlightIterator, HighlightState, Highlighter as ThemeHighlighter,
    ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use crate::document::Line;

/// A longer line is left uncoloured: regex grammars can take seconds on minified files.
const MAX_LINE_BYTES: usize = 16 * 1024;
const CHUNK_LINES: usize = 512;

/// Code colours follow the document theme, independently of the interface theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyntaxTheme {
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleRun {
    pub start: u32,
    pub end: u32,
    pub color: [u8; 3],
    pub italic: bool,
}

pub type LineStyles = Arc<[StyleRun]>;

/// `(scope selector, colour, italic)`, most generic first: later rules win on equal specificity.
type ThemeRules = [(&'static str, u32, bool)];

/// Saturated tones that stay readable on the tinted change zones.
const LIGHT_RULES: &ThemeRules = &[
    ("comment, punctuation.definition.comment", 0x59616c, true),
    ("string, string punctuation.definition.string", 0x0a6836, false),
    ("constant.numeric, constant.language, constant.character", 0x933c08, false),
    ("constant.character.escape, constant.other", 0x0a6072, false),
    ("keyword, storage.modifier", 0x7a1fa2, false),
    ("keyword.operator", 0x4a5568, false),
    ("storage.type", 0x7a1fa2, false),
    ("entity.name.function, support.function, variable.function", 0x1657a6, false),
    ("entity.name.type, entity.name.class, entity.name.struct, support.type, support.class", 0x784e00, false),
    ("entity.name.tag", 0xa8231b, false),
    ("entity.other.attribute-name", 0x784e00, false),
    ("variable.parameter", 0x8a2f10, false),
    ("variable.language", 0xa8231b, false),
    ("punctuation", 0x4a5568, false),
    ("markup.heading", 0x1657a6, false),
    ("markup.italic", 0x7a1fa2, true),
];

/// Tokyo Night.
const DARK_RULES: &ThemeRules = &[
    ("comment, punctuation.definition.comment", 0x9aa2c8, true),
    ("string, string punctuation.definition.string", 0x9ece6a, false),
    ("constant.numeric, constant.language, constant.character", 0xff9e64, false),
    ("constant.character.escape, constant.other", 0x89ddff, false),
    ("keyword, storage.modifier", 0xbb9af7, false),
    ("keyword.operator", 0x89ddff, false),
    ("storage.type", 0xbb9af7, false),
    ("entity.name.function, support.function, variable.function", 0x7aa2f7, false),
    ("entity.name.type, entity.name.class, entity.name.struct, support.type, support.class", 0x2ac3de, false),
    ("entity.name.tag", 0xf7768e, false),
    ("entity.other.attribute-name", 0xbb9af7, false),
    ("variable.parameter", 0xe0af68, false),
    ("variable.language", 0xf7768e, false),
    ("punctuation", 0x9aa5ce, false),
    ("markup.heading", 0x7aa2f7, false),
    ("markup.italic", 0xbb9af7, true),
];

fn rgb(value: u32) -> Color {
    Color {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
        a: 0xff,
    }
}

fn build_theme(name: &str, foreground: u32, rules: &ThemeRules) -> Theme {
    Theme {
        name: Some(name.to_owned()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(rgb(foreground)),
            ..ThemeSettings::default()
        },
        scopes: rules
            .iter()
            .map(|&(selector, color, italic)| ThemeItem {
                scope: ScopeSelectors::from_str(selector).expect("selectors are constants"),
                style: StyleModifier {
                    foreground: Some(rgb(color)),
                    background: None,
                    font_style: italic.then_some(FontStyle::ITALIC),
                },
            })
            .collect(),
    }
}

/// Every foreground colour of a theme, so the interface can verify its contrast.
pub fn theme_colors(theme: SyntaxTheme) -> Vec<[u8; 3]> {
    let rules = match theme {
        SyntaxTheme::Light => LIGHT_RULES,
        SyntaxTheme::Dark => DARK_RULES,
    };
    rules
        .iter()
        .map(|&(_, color, _)| {
            let color = rgb(color);
            [color.r, color.g, color.b]
        })
        .collect()
}

struct Assets {
    syntaxes: SyntaxSet,
    light: Theme,
    dark: Theme,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| Assets {
        syntaxes: two_face::syntax::extra_newlines(),
        light: build_theme("BioMerge Light", 0x1f2328, LIGHT_RULES),
        dark: build_theme("Tokyo Night", 0xc0caf5, DARK_RULES),
    })
}

/// Loads grammars ahead of time, so the first comparison does not pay for it.
pub fn preload() {
    assets();
}

/// Name of the grammar for a file, from its name then from its first line (shebang, modeline).
pub fn detect_syntax(path: Option<&Path>, first_line: &str) -> Option<String> {
    let syntaxes = &assets().syntaxes;
    let by_path = path.and_then(|p| {
        let name = p.file_name()?.to_str()?;
        let extension = p.extension().and_then(|e| e.to_str());
        syntaxes
            .find_syntax_by_extension(name)
            .or_else(|| syntaxes.find_syntax_by_extension(extension?))
    });
    by_path
        .or_else(|| syntaxes.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
        .map(|s| s.name.clone())
}

struct Checkpoint {
    line: usize,
    parse: ParseState,
    highlight: HighlightState,
}

/// Grammar states kept along a file. After an edit, highlighting resumes from the last one before
/// it instead of from the top: while typing, the first lines of a pass are the ones that count.
#[derive(Default)]
pub struct Checkpoints(Mutex<Vec<Checkpoint>>);

impl Checkpoints {
    pub fn clear(&self) {
        if let Ok(mut checkpoints) = self.0.lock() {
            checkpoints.clear();
        }
    }

    /// Forgets what a change at `line` invalidates and returns the state to resume from.
    fn resume_before(&self, line: usize) -> Option<Checkpoint> {
        let mut checkpoints = self.0.lock().ok()?;
        checkpoints.retain(|checkpoint| checkpoint.line <= line);
        let last = checkpoints.last()?;
        Some(Checkpoint {
            line: last.line,
            parse: last.parse.clone(),
            highlight: last.highlight.clone(),
        })
    }

    fn record(&self, line: usize, parse: &ParseState, highlight: &HighlightState) {
        if let Ok(mut checkpoints) = self.0.lock() {
            if checkpoints.last().is_none_or(|last| last.line < line) {
                checkpoints.push(Checkpoint {
                    line,
                    parse: parse.clone(),
                    highlight: highlight.clone(),
                });
            }
        }
    }
}

/// Highlights `lines` from the last checkpoint before `changed_line`, calling
/// `emit(first_line, styles)` for each chunk.
pub fn highlight_lines(
    syntax_name: &str,
    theme: SyntaxTheme,
    lines: &[Line],
    changed_line: usize,
    checkpoints: &Checkpoints,
    cancel: &AtomicBool,
    mut emit: impl FnMut(usize, Vec<LineStyles>),
) {
    let Assets {
        syntaxes,
        light,
        dark,
    } = assets();
    let theme = match theme {
        SyntaxTheme::Light => light,
        SyntaxTheme::Dark => dark,
    };
    let Some(syntax) = syntaxes.find_syntax_by_name(syntax_name) else {
        return;
    };
    let highlighter = ThemeHighlighter::new(theme);
    let (first, mut parse, mut state) = match checkpoints.resume_before(changed_line) {
        Some(checkpoint) => (checkpoint.line, checkpoint.parse, checkpoint.highlight),
        None => (
            0,
            ParseState::new(syntax),
            HighlightState::new(&highlighter, ScopeStack::new()),
        ),
    };
    let mut text = String::new();

    for (chunk_index, chunk) in lines.get(first..).unwrap_or(&[]).chunks(CHUNK_LINES).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let chunk_start = first + chunk_index * CHUNK_LINES;
        checkpoints.record(chunk_start, &parse, &state);
        let mut styles = Vec::with_capacity(chunk.len());
        for line in chunk {
            if line.text.len() > MAX_LINE_BYTES {
                styles.push(Arc::from(Vec::new()));
                continue;
            }
            text.clear();
            text.push_str(&line.text);
            text.push('\n');
            let Ok(ops) = parse.parse_line(&text, syntaxes) else {
                styles.push(Arc::from(Vec::new()));
                continue;
            };
            let mut runs = Vec::new();
            let mut start = 0u32;
            for (style, piece) in HighlightIterator::new(&mut state, &ops, &text, &highlighter) {
                let end = (start as usize + piece.len()).min(line.text.len()) as u32;
                if end > start {
                    runs.push(StyleRun {
                        start,
                        end,
                        color: [style.foreground.r, style.foreground.g, style.foreground.b],
                        italic: style.font_style.contains(FontStyle::ITALIC),
                    });
                }
                start = end;
            }
            styles.push(Arc::from(runs));
        }
        emit(chunk_start, styles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn run_with(theme: SyntaxTheme, syntax: &str, text: &str) -> Vec<LineStyles> {
        let doc = Document::from_text(text);
        let mut all = Vec::new();
        highlight_lines(
            syntax,
            theme,
            &doc.lines,
            0,
            &Checkpoints::default(),
            &AtomicBool::new(false),
            |_, chunk| all.extend(chunk),
        );
        all
    }

    fn run(syntax: &str, text: &str) -> Vec<LineStyles> {
        run_with(SyntaxTheme::Light, syntax, text)
    }

    #[test]
    fn each_document_theme_has_its_own_colors() {
        let text = "fn main() { let s = 1; }\n";
        let light = run_with(SyntaxTheme::Light, "Rust", text);
        let dark = run_with(SyntaxTheme::Dark, "Rust", text);
        assert_ne!(light[0][0].color, dark[0][0].color);
    }

    #[test]
    fn languages_are_detected_from_names_and_shebangs() {
        let detect = |name: &str| detect_syntax(Some(Path::new(name)), "");
        assert_eq!(detect("main.rs").as_deref(), Some("Rust"));
        assert_eq!(detect("a.cpp").as_deref(), Some("C++"));
        assert_eq!(detect("app.ts").as_deref(), Some("TypeScript"));
        assert_eq!(detect("Cargo.toml").as_deref(), Some("TOML"));
        assert_eq!(detect("notes.unknownext"), None);
        assert!(detect_syntax(None, "#!/usr/bin/env python3").is_some());
    }

    #[test]
    fn keywords_and_strings_get_distinct_colors() {
        let styles = run("Rust", "fn main() { let s = \"hi\"; }\n");
        assert_eq!(styles.len(), 1);
        let colors: std::collections::HashSet<_> = styles[0].iter().map(|r| r.color).collect();
        assert!(colors.len() >= 3, "{colors:?}");
    }

    #[test]
    fn runs_cover_the_line_without_the_added_newline() {
        let text = "let é = \"déjà\";";
        let styles = run("Rust", text);
        let runs = &styles[0];
        assert_eq!(runs[0].start, 0);
        assert_eq!(runs.last().unwrap().end as usize, text.len());
        assert!(runs.windows(2).all(|w| w[0].end == w[1].start));
    }

    #[test]
    fn multi_line_state_is_kept_across_chunks() {
        let mut text = String::from("/*\n");
        text.push_str(&"inside\n".repeat(CHUNK_LINES + 3));
        text.push_str("*/\nfn x() {}\n");
        let styles = run("Rust", &text);
        let inside_early = styles[1][0].color;
        let inside_late = styles[CHUNK_LINES + 2][0].color;
        assert_eq!(inside_early, inside_late);
        assert_ne!(styles.last().unwrap()[0].color, inside_early);
    }

    #[test]
    fn highlighting_resumes_near_an_edit_with_the_right_state() {
        let mut text = String::from("/*\n");
        text.push_str(&"inside\n".repeat(CHUNK_LINES * 2 + 10));
        text.push_str("*/\nfn x() {}\n");
        let doc = Document::from_text(&text);
        let checkpoints = Checkpoints::default();
        let cancel = AtomicBool::new(false);
        let mut full = Vec::new();
        highlight_lines("Rust", SyntaxTheme::Light, &doc.lines, 0, &checkpoints, &cancel, |start, chunk| {
            full.push((start, chunk.len()));
        });
        assert_eq!(full[0], (0, CHUNK_LINES));

        let changed = CHUNK_LINES * 2 + 3;
        let mut resumed = Vec::new();
        highlight_lines("Rust", SyntaxTheme::Light, &doc.lines, changed, &checkpoints, &cancel, |start, chunk| {
            resumed.push((start, chunk));
        });
        assert_eq!(resumed[0].0, CHUNK_LINES * 2, "only the last chunk is redone");
        let comment = run("Rust", &text)[1][0].color;
        assert_eq!(resumed[0].1[1][0].color, comment, "still inside the comment");
    }

    #[test]
    fn an_empty_document_highlights_nothing() {
        let doc = Document::from_text("");
        let mut chunks = 0;
        highlight_lines(
            "Rust",
            SyntaxTheme::Light,
            &doc.lines,
            0,
            &Checkpoints::default(),
            &AtomicBool::new(false),
            |_, _| chunks += 1,
        );
        assert_eq!(chunks, 0);
    }

    #[test]
    fn cancellation_stops_the_worker() {
        let doc = Document::from_text("a\n");
        let mut chunks = 0;
        highlight_lines(
            "Rust",
            SyntaxTheme::Light,
            &doc.lines,
            0,
            &Checkpoints::default(),
            &AtomicBool::new(true),
            |_, _| chunks += 1,
        );
        assert_eq!(chunks, 0);
    }
}
