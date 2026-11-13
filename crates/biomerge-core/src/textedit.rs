//! Text editing on a [`Document`]: positions, movements and replacements.

use crate::document::{Document, Edit};

/// A position in a document: a line and a byte offset on a character boundary of that line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub line: u32,
    pub byte: usize,
}

impl TextPos {
    pub fn new(line: u32, byte: usize) -> Self {
        Self { line, byte }
    }
}

fn line_text(doc: &Document, line: u32) -> &str {
    doc.lines.get(line as usize).map_or("", |l| l.text.as_str())
}

/// The closest valid position.
pub fn clamp(doc: &Document, pos: TextPos) -> TextPos {
    if doc.is_empty() {
        return TextPos::default();
    }
    let line = pos.line.min(doc.len() as u32 - 1);
    let text = line_text(doc, line);
    let mut byte = pos.byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    TextPos { line, byte }
}

pub fn end_of_document(doc: &Document) -> TextPos {
    let line = doc.len().saturating_sub(1) as u32;
    TextPos::new(line, line_text(doc, line).len())
}

pub fn previous(doc: &Document, pos: TextPos) -> TextPos {
    if pos.byte > 0 {
        let text = line_text(doc, pos.line);
        let size = text[..pos.byte].chars().next_back().map_or(1, char::len_utf8);
        TextPos::new(pos.line, pos.byte - size)
    } else if pos.line > 0 {
        TextPos::new(pos.line - 1, line_text(doc, pos.line - 1).len())
    } else {
        pos
    }
}

pub fn next(doc: &Document, pos: TextPos) -> TextPos {
    let text = line_text(doc, pos.line);
    if let Some(c) = text[pos.byte.min(text.len())..].chars().next() {
        TextPos::new(pos.line, pos.byte + c.len_utf8())
    } else if (pos.line as usize) + 1 < doc.len() {
        TextPos::new(pos.line + 1, 0)
    } else {
        pos
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub fn word_left(doc: &Document, pos: TextPos) -> TextPos {
    if pos.byte == 0 {
        return previous(doc, pos);
    }
    let text = line_text(doc, pos.line);
    let mut chars = text[..pos.byte].char_indices().rev().peekable();
    let mut at = pos.byte;
    while let Some(&(index, c)) = chars.peek() {
        if !c.is_whitespace() {
            break;
        }
        at = index;
        chars.next();
    }
    if let Some(&(_, first)) = chars.peek() {
        let word = is_word(first);
        for (index, c) in chars {
            if c.is_whitespace() || is_word(c) != word {
                break;
            }
            at = index;
        }
    }
    TextPos::new(pos.line, at)
}

pub fn word_right(doc: &Document, pos: TextPos) -> TextPos {
    let text = line_text(doc, pos.line);
    if pos.byte >= text.len() {
        return next(doc, pos);
    }
    let mut chars = text[pos.byte..].char_indices().peekable();
    let mut at = text.len();
    if let Some(&(_, first)) = chars.peek() {
        if !first.is_whitespace() {
            let word = is_word(first);
            while let Some(&(_, c)) = chars.peek() {
                if c.is_whitespace() || is_word(c) != word {
                    break;
                }
                chars.next();
            }
        }
    }
    for (index, c) in chars {
        if !c.is_whitespace() {
            at = pos.byte + index;
            break;
        }
    }
    TextPos::new(pos.line, at)
}

/// The word under a position, for a double click.
pub fn word_at(doc: &Document, pos: TextPos) -> (TextPos, TextPos) {
    let text = line_text(doc, pos.line);
    let is_word_at = |index: usize| text[index..].chars().next().is_some_and(is_word);
    if !is_word_at(pos.byte.min(text.len())) {
        return (pos, next(doc, pos));
    }
    let start = text[..pos.byte]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(pos.byte, |(index, _)| index);
    let end = text[pos.byte..]
        .char_indices()
        .find(|&(_, c)| !is_word(c))
        .map_or(text.len(), |(index, _)| pos.byte + index);
    (TextPos::new(pos.line, start), TextPos::new(pos.line, end))
}

pub fn line_start_smart(doc: &Document, pos: TextPos) -> TextPos {
    let text = line_text(doc, pos.line);
    let indent = text.len() - text.trim_start().len();
    TextPos::new(pos.line, if pos.byte == indent { 0 } else { indent })
}

pub fn line_end(doc: &Document, pos: TextPos) -> TextPos {
    TextPos::new(pos.line, line_text(doc, pos.line).len())
}

/// The leading whitespace of a line, which a new line repeats.
pub fn indent_of(doc: &Document, line: u32) -> String {
    let text = line_text(doc, line);
    text[..text.len() - text.trim_start().len()].to_owned()
}

/// What the Tab key inserts: the document's own habit.
pub fn tab_text(doc: &Document) -> &'static str {
    if doc.lines.iter().any(|line| line.text.starts_with('\t')) {
        "\t"
    } else {
        "    "
    }
}

pub fn text_between(doc: &Document, start: TextPos, end: TextPos) -> String {
    let mut text = String::new();
    for line in start.line..=end.line {
        let content = line_text(doc, line);
        let from = if line == start.line { start.byte } else { 0 };
        let to = if line == end.line { end.byte } else { content.len() };
        text.push_str(&content[from.min(content.len())..to.min(content.len())]);
        if line != end.line {
            text.push('\n');
        }
    }
    text
}

/// Replaces what lies between two positions. Returns the edit and where the caret lands.
pub fn replace(doc: &mut Document, start: TextPos, end: TextPos, text: &str) -> (Edit, TextPos) {
    let prefix = line_text(doc, start.line)[..start.byte].to_owned();
    let suffix = line_text(doc, end.line)[end.byte..].to_owned();
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut pieces: Vec<String> = text.split('\n').map(str::to_owned).collect();
    let last = pieces.len() - 1;
    let caret_line = start.line + last as u32;
    let caret_byte = pieces[last].len() + if last == 0 { prefix.len() } else { 0 };
    pieces[0].insert_str(0, &prefix);
    pieces[last].push_str(&suffix);
    let range = if doc.is_empty() {
        0..0
    } else {
        start.line as usize..end.line as usize + 1
    };
    let edit = doc.edit_lines(range, pieces);
    (edit, TextPos::new(caret_line, caret_byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(doc: &Document) -> String {
        String::from_utf8(doc.encode().unwrap()).unwrap()
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut doc = Document::from_text("hello\nworld\n");
        let at = TextPos::new(0, 5);
        let (_, caret) = replace(&mut doc, at, at, "!");
        assert_eq!(text(&doc), "hello!\nworld\n");
        assert_eq!(caret, TextPos::new(0, 6));
    }

    #[test]
    fn a_new_line_splits_the_line() {
        let mut doc = Document::from_text("hello\n");
        let at = TextPos::new(0, 2);
        let (edit, caret) = replace(&mut doc, at, at, "\n");
        assert_eq!(text(&doc), "he\nllo\n");
        assert_eq!(caret, TextPos::new(1, 0));
        doc.apply(&edit.inverted());
        assert_eq!(text(&doc), "hello\n");
    }

    #[test]
    fn a_selection_is_replaced_across_lines() {
        let mut doc = Document::from_text("one\ntwo\nthree\n");
        let (_, caret) = replace(&mut doc, TextPos::new(0, 1), TextPos::new(2, 2), "X");
        assert_eq!(text(&doc), "oXree\n");
        assert_eq!(caret, TextPos::new(0, 2));
    }

    #[test]
    fn pasting_many_lines_lands_after_the_last() {
        let mut doc = Document::from_text("ab\n");
        let at = TextPos::new(0, 1);
        let (_, caret) = replace(&mut doc, at, at, "1\r\n2\r\n3");
        assert_eq!(text(&doc), "a1\n2\n3b\n");
        assert_eq!(caret, TextPos::new(2, 1));
    }

    #[test]
    fn deleting_joins_lines_and_keeps_accents_whole() {
        let mut doc = Document::from_text("été\nabc\n");
        let at = TextPos::new(1, 0);
        let before = previous(&doc, at);
        assert_eq!(before, TextPos::new(0, 5));
        replace(&mut doc, before, at, "");
        assert_eq!(text(&doc), "étéabc\n");
        assert_eq!(previous(&doc, TextPos::new(0, 2)), TextPos::new(0, 0));
    }

    #[test]
    fn typing_in_an_empty_document_works() {
        let mut doc = Document::from_text("");
        let (_, caret) = replace(&mut doc, TextPos::default(), TextPos::default(), "a\nb");
        assert_eq!(text(&doc), "a\nb");
        assert_eq!(caret, TextPos::new(1, 1));
    }

    #[test]
    fn movements_stop_at_the_ends() {
        let doc = Document::from_text("ab\n");
        assert_eq!(previous(&doc, TextPos::new(0, 0)), TextPos::new(0, 0));
        assert_eq!(next(&doc, TextPos::new(0, 2)), TextPos::new(0, 2));
    }

    #[test]
    fn word_movements_stop_at_word_starts() {
        let doc = Document::from_text("let value = compute(x);\n");
        let right = |byte| word_right(&doc, TextPos::new(0, byte)).byte;
        let left = |byte| word_left(&doc, TextPos::new(0, byte)).byte;
        assert_eq!(right(0), 4);
        assert_eq!(right(4), 10);
        assert_eq!(left(10), 4);
        assert_eq!(left(4), 0);
        assert_eq!(left(19), 12);
    }

    #[test]
    fn double_click_finds_the_word() {
        let doc = Document::from_text("let value = 1;\n");
        let (start, end) = word_at(&doc, TextPos::new(0, 6));
        assert_eq!((start.byte, end.byte), (4, 9));
    }

    #[test]
    fn home_goes_to_the_indent_then_to_the_start() {
        let doc = Document::from_text("    code\n");
        assert_eq!(line_start_smart(&doc, TextPos::new(0, 6)).byte, 4);
        assert_eq!(line_start_smart(&doc, TextPos::new(0, 4)).byte, 0);
    }

    #[test]
    fn the_tab_key_follows_the_document() {
        assert_eq!(tab_text(&Document::from_text("\tx\n")), "\t");
        assert_eq!(tab_text(&Document::from_text("    x\n")), "    ");
    }

    #[test]
    fn a_position_is_clamped_to_a_character_boundary() {
        let doc = Document::from_text("é\n");
        assert_eq!(clamp(&doc, TextPos::new(5, 1)), TextPos::new(0, 0));
    }

    #[test]
    fn text_between_spans_lines() {
        let doc = Document::from_text("one\ntwo\n");
        assert_eq!(text_between(&doc, TextPos::new(0, 1), TextPos::new(1, 2)), "ne\ntw");
    }
}
