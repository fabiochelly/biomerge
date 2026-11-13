//! Monospace line layout. Counting and painting share this code, so row heights always match
//! what is painted and the height of millions of rows can be computed without shaping text.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

pub const TAB_WIDTH: usize = 4;

/// Display columns of a character at column `col`.
pub fn char_columns(c: char, col: usize) -> usize {
    match c {
        '\t' => TAB_WIDTH - col % TAB_WIDTH,
        _ => c.width().unwrap_or(1),
    }
}

pub fn display_width(text: &str) -> usize {
    if text.is_ascii() && !text.contains('\t') {
        return text.len();
    }
    text.chars().fold(0, |col, c| col + char_columns(c, col))
}

/// Calls `emit` with the byte range of each visual line of `text`. Empty text gives one line.
pub fn for_each_visual_line(text: &str, columns: usize, mut emit: impl FnMut(Range<usize>)) {
    let columns = columns.max(8);
    let mut line_start = 0;
    let mut line_col = 0;
    // Logical column of the whole line: tab stops do not move when a line wraps.
    let mut col = 0;
    let mut break_at: Option<(usize, usize)> = None;

    for (index, c) in text.char_indices() {
        let width = char_columns(c, col);
        if col + width - line_col > columns && index > line_start {
            match break_at.filter(|&(at, _)| at > line_start) {
                Some((at, at_col)) => {
                    emit(line_start..at);
                    line_start = at;
                    line_col = at_col;
                }
                None => {
                    emit(line_start..index);
                    line_start = index;
                    line_col = col;
                }
            }
            break_at = None;
        }
        col += width;
        if c.is_whitespace() {
            break_at = Some((index + c.len_utf8(), col));
        }
    }
    emit(line_start..text.len());
}

pub fn visual_line_count(text: &str, columns: usize) -> u32 {
    if display_width(text) <= columns.max(8) {
        return 1;
    }
    let mut count = 0;
    for_each_visual_line(text, columns, |_| count += 1);
    count
}

/// Column where the byte offset `at` of `text` starts, relative to the visual line at `start`.
pub fn column_of(text: &str, start: usize, at: usize) -> usize {
    let mut col = 0;
    let mut visual = 0;
    for (index, c) in text.char_indices() {
        if index >= at {
            break;
        }
        let width = char_columns(c, col);
        if index >= start {
            visual += width;
        }
        col += width;
    }
    visual
}

/// Byte offset closest to `target` columns after the visual line starting at `range.start`.
pub fn offset_at_column(text: &str, range: Range<usize>, target: usize) -> usize {
    let mut col = 0;
    let mut visual = 0;
    for (index, c) in text.char_indices() {
        if index >= range.end {
            break;
        }
        let width = char_columns(c, col);
        if index >= range.start {
            if visual + width.div_ceil(2) > target {
                return index;
            }
            visual += width;
        }
        col += width;
    }
    range.end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visual(text: &str, columns: usize) -> Vec<&str> {
        let mut parts = Vec::new();
        for_each_visual_line(text, columns, |r| parts.push(&text[r]));
        parts
    }

    #[test]
    fn short_and_empty_lines_stay_on_one_row() {
        assert_eq!(visual("", 10), [""]);
        assert_eq!(visual("abc", 10), ["abc"]);
        assert_eq!(visual_line_count("", 10), 1);
    }

    #[test]
    fn wrapping_prefers_word_boundaries() {
        assert_eq!(visual("alpha beta gamma", 11), ["alpha beta ", "gamma"]);
    }

    #[test]
    fn a_comma_stays_with_its_following_space() {
        assert_eq!(visual("alpha, beta", 8), ["alpha, ", "beta"]);
    }

    #[test]
    fn long_words_are_cut() {
        assert_eq!(visual("abcdefghijklmnopqrst", 8), ["abcdefgh", "ijklmnop", "qrst"]);
    }

    #[test]
    fn exact_fit_does_not_wrap() {
        assert_eq!(visual("abcdefgh", 8), ["abcdefgh"]);
    }

    #[test]
    fn wide_characters_and_tabs_are_counted() {
        assert_eq!(display_width("\tx"), 5);
        assert_eq!(display_width("a\tb"), 5);
        assert_eq!(display_width("日本"), 4);
        assert_eq!(visual("日本語日本語", 8), ["日本語日", "本語"]);
    }

    #[test]
    fn count_always_matches_the_painted_lines() {
        let samples = [
            "",
            "plain",
            "a much longer line, with (some) punctuation; and words",
            "\t\tdeeply\tindented\ttext with tabs everywhere\tand more",
            "日本語のテキストと mixed ASCII の組み合わせ テスト",
            "éééééééééééééééééééééééééééééééééééééé",
        ];
        for text in samples {
            for columns in 8..40 {
                assert_eq!(
                    visual_line_count(text, columns) as usize,
                    visual(text, columns).len(),
                    "{text:?} at {columns}"
                );
            }
        }
    }

    #[test]
    fn all_bytes_are_covered_without_overlap() {
        let text = "\tsome text, 日本語 and more words to wrap around here";
        let mut next = 0;
        for_each_visual_line(text, 9, |r| {
            assert_eq!(r.start, next);
            next = r.end;
        });
        assert_eq!(next, text.len());
    }

    #[test]
    fn hit_testing_inverts_column_lookup() {
        let text = "ab\tc日d";
        for (offset, _) in text.char_indices() {
            let col = column_of(text, 0, offset);
            assert_eq!(offset_at_column(text, 0..text.len(), col), offset);
        }
    }
}
