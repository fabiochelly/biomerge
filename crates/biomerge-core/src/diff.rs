//! Three-layer text comparison.
//!
//! 1. Lines: Histogram diff, then Git's indent heuristic to slide hunks on logical boundaries.
//! 2. Alignment: inside a hunk, similar lines are paired with a dynamic-programming alignment.
//! 3. Tokens: paired lines are diffed word by word, then cleaned up to avoid confetti.
//!
//! The result is a list of aligned rows, where a row may have a line on one side only: it says
//! which lines face each other. Displaying them is the interface's business.
//!
//! As in Araxis Merge, one hunk stays one framed block whatever it contains: a modification
//! block can mix paired lines, whole inserted lines and whole removed lines, and a paired line
//! can mix inserted, removed and replaced text. Text missing on one side is marked there by a
//! zero-width span, drawn as a thin bar.
//!
//! Ignore rules ([`IgnoreRules`]) hide noise without hiding text: every line still gets its row,
//! but a difference that only concerns ignored text makes no block.
//! - Text matched by a rule is compared as a placeholder: lines equal apart from it are equal,
//!   and the token diff does not highlight it.
//! - A line ignored as a whole keeps its own text as comparison key, so that it still anchors
//!   the alignment, and the rows of a hunk showing nothing but such lines leave the block: a
//!   pair of them, or one of them facing a filler when the other side has no such line. Facing
//!   a line that is not ignored, it stays in the block, as that line is a real difference.

use std::borrow::Cow;
use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};
use rayon::prelude::*;

use crate::ignore_rules::PLACEHOLDER;
pub use crate::ignore_rules::{IGNORE_PRESETS, IgnorePreset, IgnoreRule, IgnoreRules, IgnoreScope};

/// Largest `left * right` aligned by dynamic programming; beyond it lines pair by position.
const MAX_ALIGNMENT_CELLS: usize = 1_000_000;
const MIN_LINE_SIMILARITY: f32 = 0.45;
/// Below this share of common text, a whole line is more readable than scattered fragments.
const MIN_COMMON_TEXT: f32 = 0.3;
/// Punctuation-only islands up to this size are absorbed between two token changes.
const MAX_ISLAND_BYTES: usize = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Whitespace {
    #[default]
    Exact,
    IgnoreTrailing,
    IgnoreAll,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffOptions {
    pub whitespace: Whitespace,
    pub ignore_case: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Insert,
    Delete,
    Change,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub left: Range<u32>,
    pub right: Range<u32>,
    pub rows: Range<u32>,
}

/// One aligned row. `None` on a side: this line has no counterpart there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub left: Option<u32>,
    pub right: Option<u32>,
    pub block: Option<u32>,
    /// Index into [`DiffResult::spans`] for paired lines that differ.
    pub pair: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Insert,
    Delete,
    Replace,
}

/// A changed byte range inside a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSpan {
    pub range: Range<u32>,
    pub kind: SpanKind,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineSpans {
    pub left: Vec<ChangeSpan>,
    pub right: Vec<ChangeSpan>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub insertions: usize,
    pub deletions: usize,
    pub changes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct DiffResult {
    pub rows: Vec<Row>,
    pub blocks: Vec<Block>,
    pub spans: Vec<LineSpans>,
    pub stats: DiffStats,
    /// The rows, in increasing order, whose difference is hidden by the ignore rules: they are
    /// in no block although their two sides are not the same under the options alone. One of
    /// their sides can be a filler (a line ignored as a whole that exists on one side only).
    pub ignored_rows: Vec<u32>,
}

impl DiffResult {
    /// The row showing `line` of the given side.
    pub fn row_of_line(&self, left_side: bool, line: u32) -> Option<usize> {
        // Line numbers increase along rows, but fillers have none: search on the next numbered.
        let key = |row: &Row| if left_side { row.left } else { row.right };
        let mut low = 0;
        let mut high = self.rows.len();
        while low < high {
            let mid = (low + high) / 2;
            let mut probe = mid;
            while probe < high && key(&self.rows[probe]).is_none() {
                probe += 1;
            }
            match (probe < high).then(|| key(&self.rows[probe]).unwrap()) {
                Some(found) if found == line => return Some(probe),
                Some(found) if found < line => low = probe + 1,
                _ => high = mid,
            }
        }
        None
    }

    /// True when no difference is left to show; ignored ones may remain, see `ignored_rows`.
    pub fn is_identical(&self) -> bool {
        self.blocks.is_empty()
    }

    /// True when the ignore rules hide a difference on this row.
    pub fn is_row_ignored(&self, row: usize) -> bool {
        self.ignored_rows.binary_search(&(row as u32)).is_ok()
    }
}

/// Compares two line lists.
pub fn diff_lines<L: AsRef<str> + Sync>(
    left: &[L],
    right: &[L],
    options: DiffOptions,
) -> DiffResult {
    diff_lines_with(left, right, options, &IgnoreRules::default())
}

/// What the ignore rules found on a line.
const MARK_NONE: u8 = 0;
const MARK_TEXT: u8 = 1;
const MARK_LINE: u8 = 2;

/// Compares two line lists, leaving out of the blocks what the rules ignore.
pub fn diff_lines_with<L: AsRef<str> + Sync>(
    left: &[L],
    right: &[L],
    options: DiffOptions,
    rules: &IgnoreRules,
) -> DiffResult {
    let ignoring = !rules.is_empty();
    let (left_keys, left_marks) = comparison_keys(left, options, rules);
    let (right_keys, right_marks) = comparison_keys(right, options, rules);
    let hunks = line_hunks(&left_keys, &right_keys);

    let aligned: Vec<Vec<Pairing>> = hunks
        .par_iter()
        .map(|hunk| {
            align_hunk(
                &left_keys[hunk.before.start as usize..hunk.before.end as usize],
                &right_keys[hunk.after.start as usize..hunk.after.end as usize],
                hunk.before.start,
                hunk.after.start,
            )
        })
        .collect();

    let whole = |marks: &[u8], line: u32| marks[line as usize] == MARK_LINE;
    let neutral = |pairing: &Pairing| {
        ignoring
            && match *pairing {
                Pairing::Both(l, r) => whole(&left_marks, l) && whole(&right_marks, r),
                Pairing::LeftOnly(l) => whole(&left_marks, l),
                Pairing::RightOnly(r) => whole(&right_marks, r),
            }
    };
    let mut result = assemble(left.len() as u32, right.len() as u32, &hunks, aligned, neutral);

    if ignoring {
        // Only lines touched by a rule can hide a difference: the others are not read again.
        result.ignored_rows = result
            .rows
            .par_iter()
            .enumerate()
            .filter(|(_, row)| row.block.is_none())
            .filter(|(_, row)| match (row.left, row.right) {
                (Some(l), Some(r)) => {
                    (left_marks[l as usize] | right_marks[r as usize]) != MARK_NONE
                        && normalize(left[l as usize].as_ref(), options)
                            != normalize(right[r as usize].as_ref(), options)
                }
                _ => true,
            })
            .map(|(index, _)| index as u32)
            .collect();
    }

    let pair_rows: Vec<(usize, u32, u32)> = result
        .rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| Some((index, row.left?, row.right?)))
        .filter(|(index, ..)| result.rows[*index].block.is_some())
        .collect();
    let computed: Vec<(usize, LineSpans)> = pair_rows
        .par_iter()
        .map(|&(index, l, r)| {
            let (l, r) = (left[l as usize].as_ref(), right[r as usize].as_ref());
            let spans = diff_tokens_with(l, r, options, rules);
            (index, spans)
        })
        .collect();
    for (index, spans) in computed {
        result.rows[index].pair = Some(result.spans.len() as u32);
        result.spans.push(spans);
    }
    result
}

/// The comparison key of each line under the options and rules, and what the rules found on
/// it (no marks at all without rules, so that the keys cost what they always did). Shared with
/// the three-way merge, whose hunks must be the ones the two-way comparison shows.
pub(crate) fn comparison_keys<L: AsRef<str> + Sync>(
    lines: &[L],
    options: DiffOptions,
    rules: &IgnoreRules,
) -> (Vec<String>, Vec<u8>) {
    if rules.is_empty() {
        let keys = lines.par_iter().map(|line| normalize(line.as_ref(), options).into_owned()).collect();
        (keys, Vec::new())
    } else {
        lines.par_iter().map(|line| masked_key(line.as_ref(), options, rules)).unzip()
    }
}

/// The line hunks between two key lists: Histogram, then the indent heuristic.
pub(crate) fn line_hunks(left_keys: &[String], right_keys: &[String]) -> Vec<imara_diff::Hunk> {
    let input = InternedInput::new(LineSource(left_keys), LineSource(right_keys));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    diff.hunks().collect()
}

struct LineSource<'a>(&'a [String]);

impl<'a> TokenSource for LineSource<'a> {
    type Token = &'a str;
    type Tokenizer = std::iter::Map<std::slice::Iter<'a, String>, fn(&'a String) -> &'a str>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().map(String::as_str)
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

fn normalize(line: &str, options: DiffOptions) -> Cow<'_, str> {
    let text: Cow<'_, str> = match options.whitespace {
        Whitespace::Exact => Cow::Borrowed(line),
        Whitespace::IgnoreTrailing => Cow::Borrowed(line.trim_end()),
        Whitespace::IgnoreAll => {
            Cow::Owned(line.chars().filter(|c| !c.is_whitespace()).collect())
        }
    };
    if options.ignore_case {
        Cow::Owned(text.to_lowercase())
    } else {
        text
    }
}

/// The comparison key of a line under the rules, and what they found on it. Ignored text is
/// replaced before the options apply, so that trailing whitespace is still the line's.
fn masked_key(line: &str, options: DiffOptions, rules: &IgnoreRules) -> (String, u8) {
    if rules.ignores_line(line) {
        return (normalize(line, options).into_owned(), MARK_LINE);
    }
    let ignored = rules.ignored_ranges(line);
    if ignored.is_empty() {
        return (normalize(line, options).into_owned(), MARK_NONE);
    }
    let mut masked = String::with_capacity(line.len());
    let mut at = 0;
    for range in ignored {
        masked.push_str(&line[at..range.start]);
        masked.push(PLACEHOLDER);
        at = range.end;
    }
    masked.push_str(&line[at..]);
    (normalize(&masked, options).into_owned(), MARK_TEXT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pairing {
    Both(u32, u32),
    LeftOnly(u32),
    RightOnly(u32),
}

/// A sorted multiset of hashed character bigrams.
fn bigrams(text: &str) -> Vec<u32> {
    let mut grams = Vec::with_capacity(text.len());
    let mut previous = None;
    for c in text.trim().chars() {
        if let Some(p) = previous {
            grams.push((p as u32).wrapping_mul(0x9E37_79B1) ^ c as u32);
        }
        previous = Some(c);
    }
    grams.sort_unstable();
    grams
}

/// Sørensen–Dice coefficient of two bigram multisets.
fn similarity(a: &[u32], b: &[u32]) -> f32 {
    let total = a.len() + b.len();
    if total == 0 {
        return 0.0;
    }
    if 2.0 * a.len().min(b.len()) as f32 / (total as f32) < MIN_LINE_SIMILARITY {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2.0 * common as f32 / total as f32
}

fn align_hunk(left: &[String], right: &[String], left_start: u32, right_start: u32) -> Vec<Pairing> {
    let (n, m) = (left.len(), right.len());
    if n == 0 || m == 0 {
        return fill_gap(left_start..left_start + n as u32, right_start..right_start + m as u32);
    }
    if n * m > MAX_ALIGNMENT_CELLS {
        return fill_gap(left_start..left_start + n as u32, right_start..right_start + m as u32);
    }

    let left_grams: Vec<Vec<u32>> = left.iter().map(|l| bigrams(l)).collect();
    let right_grams: Vec<Vec<u32>> = right.iter().map(|l| bigrams(l)).collect();

    // Needleman–Wunsch: maximize the total similarity of an order-preserving pairing.
    let width = m + 1;
    let mut score = vec![0f32; (n + 1) * width];
    let mut matched = vec![false; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let similar = similarity(&left_grams[i], &right_grams[j]);
            let pair = if similar >= MIN_LINE_SIMILARITY {
                similar + score[(i + 1) * width + j + 1]
            } else {
                -1.0
            };
            let skip = score[(i + 1) * width + j].max(score[i * width + j + 1]);
            matched[i * width + j] = pair > skip;
            score[i * width + j] = pair.max(skip);
        }
    }

    let mut anchors = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if matched[i * width + j] {
            anchors.push((i, j));
            i += 1;
            j += 1;
        } else if score[(i + 1) * width + j] >= score[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }

    let mut pairs = Vec::with_capacity(n.max(m));
    let (mut next_left, mut next_right) = (0, 0);
    for (l, r) in anchors {
        pairs.extend(fill_gap(
            left_start + next_left as u32..left_start + l as u32,
            right_start + next_right as u32..right_start + r as u32,
        ));
        pairs.push(Pairing::Both(left_start + l as u32, right_start + r as u32));
        next_left = l + 1;
        next_right = r + 1;
    }
    pairs.extend(fill_gap(
        left_start + next_left as u32..left_start + n as u32,
        right_start + next_right as u32..right_start + m as u32,
    ));
    pairs
}

/// Between two anchors, opposite lines read as a modification, the rest as a removal/insertion.
fn fill_gap(left: Range<u32>, right: Range<u32>) -> Vec<Pairing> {
    let common = left.len().min(right.len()) as u32;
    let mut pairs = Vec::with_capacity(left.len().max(right.len()));
    for k in 0..common {
        pairs.push(Pairing::Both(left.start + k, right.start + k));
    }
    pairs.extend((left.start + common..left.end).map(Pairing::LeftOnly));
    pairs.extend((right.start + common..right.end).map(Pairing::RightOnly));
    pairs
}

/// Rows and blocks from the aligned hunks. A hunk is one block, unless some of its rows are
/// `neutral` (they show nothing but ignored lines): those are in no block, and split the hunk
/// into as many blocks as they leave runs of other rows.
fn assemble(
    left_len: u32,
    right_len: u32,
    hunks: &[imara_diff::Hunk],
    aligned: Vec<Vec<Pairing>>,
    neutral: impl Fn(&Pairing) -> bool,
) -> DiffResult {
    fn close(result: &mut DiffResult, open: Option<(u32, u32, u32)>, l: u32, r: u32) {
        let Some((first_row, left_start, right_start)) = open else {
            return;
        };
        let kind = match (left_start == l, right_start == r) {
            (true, _) => BlockKind::Insert,
            (_, true) => BlockKind::Delete,
            _ => BlockKind::Change,
        };
        match kind {
            BlockKind::Insert => result.stats.insertions += 1,
            BlockKind::Delete => result.stats.deletions += 1,
            BlockKind::Change => result.stats.changes += 1,
        }
        result.blocks.push(Block {
            kind,
            left: left_start..l,
            right: right_start..r,
            rows: first_row..result.rows.len() as u32,
        });
    }

    let mut result = DiffResult::default();
    result.rows.reserve(left_len.max(right_len) as usize);
    let (mut l, mut r) = (0u32, 0u32);

    for (hunk, pairs) in hunks.iter().zip(aligned) {
        while l < hunk.before.start {
            result.rows.push(Row {
                left: Some(l),
                right: Some(r),
                block: None,
                pair: None,
            });
            l += 1;
            r += 1;
        }
        // The block being filled: its first row, and where it starts on each side.
        let mut open: Option<(u32, u32, u32)> = None;
        for pairing in pairs {
            let (left, right) = match pairing {
                Pairing::Both(a, b) => (Some(a), Some(b)),
                Pairing::LeftOnly(a) => (Some(a), None),
                Pairing::RightOnly(b) => (None, Some(b)),
            };
            if neutral(&pairing) {
                close(&mut result, open.take(), l, r);
            } else if open.is_none() {
                open = Some((result.rows.len() as u32, l, r));
            }
            result.rows.push(Row {
                left,
                right,
                block: open.map(|_| result.blocks.len() as u32),
                pair: None,
            });
            l += left.is_some() as u32;
            r += right.is_some() as u32;
        }
        close(&mut result, open, l, r);
        debug_assert_eq!((l, r), (hunk.before.end, hunk.after.end));
    }
    while l < left_len {
        result.rows.push(Row {
            left: Some(l),
            right: Some(r),
            block: None,
            pair: None,
        });
        l += 1;
        r += 1;
    }
    debug_assert_eq!(r, right_len);
    result
}

/// Splits a line into words, whitespace runs and single punctuation characters.
fn tokenize(text: &str) -> Vec<Range<u32>> {
    #[derive(PartialEq, Clone, Copy)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut tokens: Vec<Range<u32>> = Vec::new();
    let mut previous = None;
    for (index, c) in text.char_indices() {
        let current = class(c);
        let end = (index + c.len_utf8()) as u32;
        match tokens.last_mut() {
            Some(token) if previous == Some(current) && current != Class::Other => token.end = end,
            _ => tokens.push(index as u32..end),
        }
        previous = Some(current);
    }
    tokens
}

/// As [`tokenize`], but each ignored range is one token whatever it contains.
fn tokenize_around(text: &str, ignored: &[Range<usize>]) -> Vec<Range<u32>> {
    if ignored.is_empty() {
        return tokenize(text);
    }
    fn between(tokens: &mut Vec<Range<u32>>, text: &str, from: usize, to: usize) {
        let shift = from as u32;
        tokens.extend(tokenize(&text[from..to]).into_iter().map(|t| t.start + shift..t.end + shift));
    }
    let mut tokens: Vec<Range<u32>> = Vec::new();
    let mut at = 0;
    for range in ignored {
        between(&mut tokens, text, at, range.start);
        tokens.push(range.start as u32..range.end as u32);
        at = range.end;
    }
    between(&mut tokens, text, at, text.len());
    tokens
}

/// True when `token`, made by [`tokenize_around`], is one of the ignored ranges.
fn is_ignored(ignored: &[Range<usize>], token: &Range<u32>) -> bool {
    ignored.binary_search_by_key(&(token.start as usize), |range| range.start).is_ok()
}

fn slice<'a>(text: &'a str, range: &Range<u32>) -> &'a str {
    &text[range.start as usize..range.end as usize]
}

fn whole_line(text: &str, kind: SpanKind) -> Vec<ChangeSpan> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let start = text.len() - text.trim_start().len();
    vec![ChangeSpan {
        range: start as u32..(start + trimmed.len()) as u32,
        kind,
    }]
}

struct TokenSlice<'a>(&'a [String]);

impl<'a> TokenSource for TokenSlice<'a> {
    type Token = &'a str;
    type Tokenizer = std::iter::Map<std::slice::Iter<'a, String>, fn(&'a String) -> &'a str>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().map(String::as_str)
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

/// Word-level comparison of two paired lines.
pub fn diff_tokens(left: &str, right: &str, options: DiffOptions) -> LineSpans {
    diff_tokens_with(left, right, options, &IgnoreRules::default())
}

/// Word-level comparison of two paired lines, where text ignored by the rules is equal to any
/// other ignored text and so is never highlighted. Whole-line rules play no part here: a line
/// reaching this point is shown as different.
pub fn diff_tokens_with(
    left: &str,
    right: &str,
    options: DiffOptions,
    rules: &IgnoreRules,
) -> LineSpans {
    if left == right {
        return LineSpans::default();
    }
    // Without rules these are empty vectors, which do not allocate.
    let left_ignored = rules.ignored_ranges(left);
    let right_ignored = rules.ignored_ranges(right);
    let left_tokens = tokenize_around(left, &left_ignored);
    let right_tokens = tokenize_around(right, &right_ignored);

    let key = |text: &str, ignored: &[Range<usize>], range: &Range<u32>| -> String {
        if is_ignored(ignored, range) {
            return PLACEHOLDER.to_string();
        }
        let token = slice(text, range);
        let spacing = options.whitespace == Whitespace::IgnoreAll && token.trim().is_empty();
        match (spacing, options.ignore_case) {
            (true, _) => String::new(),
            (_, true) => token.to_lowercase(),
            _ => token.to_owned(),
        }
    };
    let left_keys: Vec<String> = left_tokens.iter().map(|t| key(left, &left_ignored, t)).collect();
    let right_keys: Vec<String> = right_tokens.iter().map(|t| key(right, &right_ignored, t)).collect();

    let input = InternedInput::new(TokenSlice(&left_keys), TokenSlice(&right_keys));
    let diff = Diff::compute(Algorithm::MyersMinimal, &input);

    // Hunks in token coordinates, then absorbing trivial equal islands between them.
    let mut hunks: Vec<(Range<u32>, Range<u32>)> = Vec::new();
    for hunk in diff.hunks() {
        if let Some((before, after)) = hunks.last_mut() {
            let island = &left_tokens[before.end as usize..hunk.before.start as usize];
            // An ignored island is kept: absorbing it would highlight it.
            let ignored = island.iter().any(|token| is_ignored(&left_ignored, token));
            if !ignored && is_trivial_island(left, island) {
                before.end = hunk.before.end;
                after.end = hunk.after.end;
                continue;
            }
        }
        hunks.push((hunk.before, hunk.after));
    }

    let visible = |text: &str| text.chars().filter(|c| !c.is_whitespace()).count();
    let changed = |tokens: &[Range<u32>], text: &str, hunk: &Range<u32>| -> usize {
        tokens[hunk.start as usize..hunk.end as usize]
            .iter()
            .map(|t| visible(slice(text, t)))
            .sum()
    };
    let left_visible = visible(left);
    let right_visible = visible(right);
    let left_changed: usize = hunks.iter().map(|(b, _)| changed(&left_tokens, left, b)).sum();
    let common = left_visible.saturating_sub(left_changed) as f32;
    let longest = left_visible.max(right_visible).max(1) as f32;
    // With ignored text in a line, fragments are kept: a whole-line span would highlight it.
    let masked = !left_ignored.is_empty() || !right_ignored.is_empty();
    if !masked && common / longest < MIN_COMMON_TEXT {
        return LineSpans {
            left: whole_line(left, SpanKind::Replace),
            right: whole_line(right, SpanKind::Replace),
        };
    }

    // Byte range of a token hunk; an empty hunk gives the zero-width position of the gap.
    let bytes = |tokens: &[Range<u32>], text: &str, hunk: &Range<u32>| -> Range<u32> {
        if hunk.is_empty() {
            let at = tokens
                .get(hunk.start as usize)
                .map_or(text.len() as u32, |token| token.start);
            return at..at;
        }
        tokens[hunk.start as usize].start..tokens[hunk.end as usize - 1].end
    };
    let hide_spacing = options.whitespace == Whitespace::IgnoreAll;

    let mut spans = LineSpans::default();
    for (before, after) in hunks {
        let left_range = bytes(&left_tokens, left, &before);
        let right_range = bytes(&right_tokens, right, &after);
        if hide_spacing
            && slice(left, &left_range).trim().is_empty()
            && slice(right, &right_range).trim().is_empty()
        {
            continue;
        }
        let kind = match (before.is_empty(), after.is_empty()) {
            (false, false) => SpanKind::Replace,
            (false, true) => SpanKind::Delete,
            (true, false) => SpanKind::Insert,
            (true, true) => continue,
        };
        spans.left.push(ChangeSpan {
            range: left_range,
            kind,
        });
        spans.right.push(ChangeSpan {
            range: right_range,
            kind,
        });
    }
    spans
}

fn is_trivial_island(text: &str, tokens: &[Range<u32>]) -> bool {
    let bytes: usize = tokens.iter().map(|t| t.len()).sum();
    bytes <= MAX_ISLAND_BYTES
        && tokens
            .iter()
            .all(|t| !slice(text, t).chars().any(char::is_alphanumeric))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    fn diff(left: &str, right: &str) -> DiffResult {
        diff_lines(&lines(left), &lines(right), DiffOptions::default())
    }

    fn changed_text<'a>(text: &'a str, spans: &[ChangeSpan]) -> Vec<&'a str> {
        spans
            .iter()
            .filter(|s| !s.range.is_empty())
            .map(|s| slice(text, &s.range))
            .collect()
    }

    /// Rows must expose every line of both sides exactly once and in order.
    fn assert_complete(result: &DiffResult, left: usize, right: usize) {
        let l: Vec<u32> = result.rows.iter().filter_map(|r| r.left).collect();
        let r: Vec<u32> = result.rows.iter().filter_map(|r| r.right).collect();
        assert_eq!(l, (0..left as u32).collect::<Vec<_>>());
        assert_eq!(r, (0..right as u32).collect::<Vec<_>>());
    }

    #[test]
    fn identical_inputs_have_no_block() {
        let result = diff("a\nb\n", "a\nb\n");
        assert!(result.is_identical());
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn empty_sides_are_supported() {
        let result = diff("", "a\nb\n");
        assert_eq!(result.stats.insertions, 1);
        assert_complete(&result, 0, 2);
        let result = diff("a\n", "");
        assert_eq!(result.stats.deletions, 1);
        assert!(diff("", "").rows.is_empty());
    }

    #[test]
    fn insertion_produces_filler_rows_on_the_other_side() {
        let result = diff("a\nc\n", "a\nb\nc\n");
        assert_eq!(result.blocks.len(), 1);
        let block = &result.blocks[0];
        assert_eq!(block.kind, BlockKind::Insert);
        assert_eq!(block.left, 1..1);
        assert_eq!(block.right, 1..2);
        assert_eq!(result.rows[1].left, None);
        assert_eq!(result.rows[1].right, Some(1));
    }

    #[test]
    fn a_modification_block_keeps_inserted_lines_in_its_frame() {
        let left = "fn alpha(value: u32) -> u32 {\nlet total = compute(value);\n";
        let right = "fn alpha(value: u64) -> u64 {\n// brand new comment\nlet total = compute_fast(value);\n";
        let result = diff(left, right);
        assert_complete(&result, 2, 3);
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(result.blocks[0].kind, BlockKind::Change);
        assert_eq!(result.stats.changes, 1);
        let comment = result.rows.iter().find(|r| r.right == Some(1)).unwrap();
        assert_eq!(comment.left, None);
        assert_eq!(comment.block, Some(0));
        let total = result.rows.iter().find(|r| r.right == Some(2)).unwrap();
        assert_eq!(total.left, Some(1));
        assert!(total.pair.is_some());
    }

    #[test]
    fn a_modification_block_keeps_removed_lines_in_its_frame() {
        let left = "{\n    found = true;\n    current = action(current);\n}\nend\n";
        let right = "  found = true;\nend\n";
        let result = diff(left, right);
        assert_complete(&result, 5, 2);
        assert_eq!(result.blocks.len(), 1);
        let rows: Vec<_> = result.rows[..4].iter().map(|r| (r.left, r.right)).collect();
        assert_eq!(
            rows,
            [
                (Some(0), None),
                (Some(1), Some(0)),
                (Some(2), None),
                (Some(3), None)
            ]
        );
    }

    #[test]
    fn dissimilar_opposite_lines_read_as_a_modification() {
        let result = diff("first thing\n", "completely other\n");
        assert_eq!(result.blocks[0].kind, BlockKind::Change);
        assert_eq!(result.stats.changes, 1);
    }

    #[test]
    fn huge_hunks_fall_back_to_positional_pairing() {
        let left: Vec<String> = (0..1500).map(|i| format!("left {i}")).collect();
        let right: Vec<String> = (0..1500).map(|i| format!("right {i}")).collect();
        let result = diff_lines(&left, &right, DiffOptions::default());
        assert_complete(&result, 1500, 1500);
    }

    #[test]
    fn token_diff_highlights_only_the_changed_words() {
        let left = "const unichar* pchData = pEngine->getLineBegin(side);";
        let right = "const wchar_t* pchData = pEngine->getLineData(side);";
        let spans = diff_tokens(left, right, DiffOptions::default());
        assert_eq!(changed_text(left, &spans.left), ["unichar", "getLineBegin"]);
        assert_eq!(changed_text(right, &spans.right), ["wchar_t", "getLineData"]);
        assert!(spans.left.iter().all(|s| s.kind == SpanKind::Replace));
    }

    #[test]
    fn inserted_text_is_marked_by_a_bar_on_the_other_side() {
        let left = "let value = compute(input);";
        let right = "let value = compute(input, extra);";
        let spans = diff_tokens(left, right, DiffOptions::default());
        assert_eq!(changed_text(right, &spans.right), [", extra"]);
        assert_eq!(spans.right[0].kind, SpanKind::Insert);
        assert_eq!(spans.left[0].kind, SpanKind::Insert);
        let at = left.find(");").unwrap() as u32;
        assert_eq!(spans.left[0].range, at..at);
    }

    #[test]
    fn removed_text_is_marked_by_a_bar_on_the_other_side() {
        let spans = diff_tokens("    found = true;", "found = true;", DiffOptions::default());
        assert_eq!(spans.left[0].range, 0..4);
        assert_eq!(spans.left[0].kind, SpanKind::Delete);
        assert_eq!(spans.right[0].range, 0..0);
        assert_eq!(spans.right[0].kind, SpanKind::Delete);
    }

    #[test]
    fn one_line_can_mix_insertion_and_replacement() {
        let left = "for (engine::iterator<DiffLines> current(begin()); ok; next())";
        let right = "for (engine::iterator<DiffEngine<DiffLines> > current(rangeBegin()); ok; next())";
        let spans = diff_tokens(left, right, DiffOptions::default());
        let kinds: Vec<_> = spans.right.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SpanKind::Insert), "{spans:?}");
        assert!(kinds.contains(&SpanKind::Replace), "{spans:?}");
    }

    #[test]
    fn multibyte_text_keeps_valid_boundaries() {
        let left = "let é = \"déjà vu\";";
        let right = "let é = \"déjà lu\";";
        let spans = diff_tokens(left, right, DiffOptions::default());
        assert_eq!(changed_text(left, &spans.left), ["vu"]);
        assert_eq!(changed_text(right, &spans.right), ["lu"]);
    }

    #[test]
    fn unrelated_paired_lines_highlight_the_whole_text_without_indent() {
        let spans = diff_tokens("    alpha beta", "    gamma delta", DiffOptions::default());
        assert_eq!(changed_text("    alpha beta", &spans.left), ["alpha beta"]);
    }

    #[test]
    fn whitespace_and_case_options_hide_differences() {
        let ignore_all = DiffOptions {
            whitespace: Whitespace::IgnoreAll,
            ignore_case: false,
        };
        let trailing = DiffOptions {
            whitespace: Whitespace::IgnoreTrailing,
            ignore_case: false,
        };
        let case = DiffOptions {
            whitespace: Whitespace::Exact,
            ignore_case: true,
        };
        let run = |l: &str, r: &str, o| diff_lines(&lines(l), &lines(r), o).is_identical();
        assert!(run("a  b\n", "a b\n", ignore_all));
        assert!(run("a  \n", "a\n", trailing));
        assert!(!run("  a\n", "a\n", trailing));
        assert!(run("ABC\n", "abc\n", case));
        assert!(!run("ABC\n", "abd\n", case));
    }

    #[test]
    fn hunks_slide_to_function_boundaries() {
        let left = "fn a() {\n    one();\n}\n\nfn c() {\n    three();\n}\n";
        let right = "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n\nfn c() {\n    three();\n}\n";
        let result = diff(left, right);
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(result.blocks[0].right, 4..8);
    }

    #[test]
    fn line_lookup_skips_fillers() {
        let result = diff("a\nc\n", "a\nb\nc\n");
        assert_eq!(result.row_of_line(true, 1), Some(2));
        assert_eq!(result.row_of_line(false, 1), Some(1));
        assert_eq!(result.row_of_line(true, 9), None);
    }

    fn rules(rules: &[IgnoreRule]) -> IgnoreRules {
        IgnoreRules::new(rules).unwrap()
    }

    fn preset(name: &str) -> IgnoreRule {
        IGNORE_PRESETS.iter().find(|preset| preset.name == name).unwrap().rule()
    }

    fn diff_with(left: &str, right: &str, rules: &IgnoreRules) -> DiffResult {
        diff_lines_with(&lines(left), &lines(right), DiffOptions::default(), rules)
    }

    /// Blocks and rows must tell the same story, and stats must count the blocks.
    fn assert_consistent(result: &DiffResult) {
        let mut next_row = 0;
        for (index, block) in result.blocks.iter().enumerate() {
            assert!(block.rows.start >= next_row && !block.rows.is_empty(), "{block:?}");
            next_row = block.rows.end;
            let rows = &result.rows[block.rows.start as usize..block.rows.end as usize];
            assert!(rows.iter().all(|row| row.block == Some(index as u32)), "{block:?}");
            let l: Vec<u32> = rows.iter().filter_map(|row| row.left).collect();
            let r: Vec<u32> = rows.iter().filter_map(|row| row.right).collect();
            assert_eq!(l, block.left.clone().collect::<Vec<_>>(), "{block:?}");
            assert_eq!(r, block.right.clone().collect::<Vec<_>>(), "{block:?}");
            let kind = match (l.is_empty(), r.is_empty()) {
                (true, _) => BlockKind::Insert,
                (_, true) => BlockKind::Delete,
                _ => BlockKind::Change,
            };
            assert_eq!(block.kind, kind, "{block:?}");
        }
        let in_blocks: usize = result.blocks.iter().map(|block| block.rows.len()).sum();
        assert_eq!(result.rows.iter().filter(|row| row.block.is_some()).count(), in_blocks);
        let stats = result.stats;
        assert_eq!(stats.insertions + stats.deletions + stats.changes, result.blocks.len());
        assert!(result.ignored_rows.is_sorted_by(|a, b| a < b));
        for &row in &result.ignored_rows {
            assert_eq!(result.rows[row as usize].block, None);
            assert!(result.is_row_ignored(row as usize));
        }
    }

    #[test]
    fn without_rules_the_result_is_the_one_of_the_plain_comparison() {
        let left = "fn a() {\n    one(1);\n}\nshared\nold line\n";
        let right = "fn a() {\n    one(2);\n}\nnew first\nshared\n";
        let plain = diff(left, right);
        for none in [IgnoreRules::default(), rules(&[]), rules(&[IgnoreRule::text("")])] {
            let result = diff_with(left, right, &none);
            assert_eq!(result.rows, plain.rows);
            assert_eq!(result.blocks, plain.blocks);
            assert_eq!(result.spans, plain.spans);
            assert_eq!(result.stats, plain.stats);
            assert!(result.ignored_rows.is_empty());
        }
        assert!(!plain.is_identical());
        assert!(plain.ignored_rows.is_empty());
        assert_consistent(&plain);
    }

    #[test]
    fn lines_differing_only_by_ignored_text_are_paired_without_a_block() {
        let left = "start\nbuilt on 2024-03-15 by ci\nend\n";
        let right = "start\nbuilt on 2025-01-02 by ci\nend\n";
        assert_eq!(diff(left, right).blocks.len(), 1);
        let result = diff_with(left, right, &rules(&[preset("Date ISO")]));
        assert!(result.is_identical());
        assert_eq!(result.stats, DiffStats::default());
        assert_complete(&result, 3, 3);
        assert_eq!(result.rows[1], Row { left: Some(1), right: Some(1), block: None, pair: None });
        assert_eq!(result.ignored_rows, [1]);
        assert!(result.is_row_ignored(1));
        assert!(!result.is_row_ignored(0));
    }

    #[test]
    fn ignored_text_is_masked_not_removed() {
        let digits = rules(&[IgnoreRule::text("[0-9]+")]);
        // A number that is missing, or that became a word, is a real difference.
        assert!(!diff_with("id 12\n", "id \n", &digits).is_identical());
        assert!(!diff_with("id 12\n", "id abc\n", &digits).is_identical());
        assert!(!diff_with("id 12\n", "id 12 13\n", &digits).is_identical());
        // Its length is not.
        assert!(diff_with("id 12\n", "id 123456\n", &digits).is_identical());
        assert!(diff_with("12\n", "7\n", &digits).is_identical());
    }

    #[test]
    fn equal_lines_with_ignored_text_are_not_reported_as_ignored() {
        let digits = rules(&[IgnoreRule::text("[0-9]+")]);
        let result = diff_with("id 12\nid 13\nplain\n", "id 12\nid 14\nplain\n", &digits);
        assert!(result.is_identical());
        assert_eq!(result.ignored_rows, [1]);
    }

    #[test]
    fn a_real_difference_next_to_ignored_text_is_still_a_block() {
        let left = "keep\nrelease v1.2.3 is alpha quality\nkeep\n";
        let right = "keep\nrelease v1.2.4 is beta quality\nkeep\n";
        let result = diff_with(left, right, &rules(&[preset("Numéro de version")]));
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(result.stats.changes, 1);
        assert!(result.ignored_rows.is_empty());
        let spans = &result.spans[result.rows[1].pair.unwrap() as usize];
        assert_eq!(changed_text("release v1.2.3 is alpha quality", &spans.left), ["alpha"]);
        assert_eq!(changed_text("release v1.2.4 is beta quality", &spans.right), ["beta"]);
        assert_consistent(&result);
    }

    #[test]
    fn the_token_diff_does_not_highlight_ignored_text() {
        let digits = rules(&[IgnoreRule::text("[0-9]+")]);
        let options = DiffOptions::default();
        let (left, right) = ("port 8080 open since 12 days", "port 9090 closed since 3 days");
        let spans = diff_tokens_with(left, right, options, &digits);
        assert_eq!(changed_text(left, &spans.left), ["open"]);
        assert_eq!(changed_text(right, &spans.right), ["closed"]);
        // Without rules the numbers are highlighted too.
        let plain = diff_tokens(left, right, options);
        assert_eq!(changed_text(left, &plain.left), ["8080 open", "12"]);
        // Lines differing only by ignored text have nothing to highlight.
        assert_eq!(diff_tokens_with("id 1", "id 22", options, &digits), LineSpans::default());
    }

    #[test]
    fn unrelated_lines_with_ignored_text_keep_it_out_of_the_highlight() {
        let digits = rules(&[IgnoreRule::text("[0-9]+")]);
        let (left, right) = ("12 alpha beta", "345 gamma delta");
        let spans = diff_tokens_with(left, right, DiffOptions::default(), &digits);
        assert_eq!(changed_text(left, &spans.left), ["alpha beta"]);
        assert_eq!(changed_text(right, &spans.right), ["gamma delta"]);
    }

    #[test]
    fn ignored_punctuation_between_two_changes_is_not_absorbed() {
        let dash = rules(&[IgnoreRule::text("[-/]")]);
        let options = DiffOptions::default();
        let spans = diff_tokens_with("alpha-beta", "gamma/delta", options, &dash);
        assert_eq!(changed_text("alpha-beta", &spans.left), ["alpha", "beta"]);
        assert_eq!(changed_text("gamma/delta", &spans.right), ["gamma", "delta"]);
        let plain = diff_tokens("alpha-beta gamma", "delta-omega gamma", options);
        assert_eq!(changed_text("alpha-beta gamma", &plain.left), ["alpha-beta"]);
    }

    #[test]
    fn ignored_text_keeps_multibyte_boundaries() {
        let digits = rules(&[IgnoreRule::text("[0-9]+")]);
        let (left, right) = ("été 2023 : déjà vu", "été 2024 : déjà lu");
        let spans = diff_tokens_with(left, right, DiffOptions::default(), &digits);
        assert_eq!(changed_text(left, &spans.left), ["vu"]);
        assert_eq!(changed_text(right, &spans.right), ["lu"]);
        assert!(diff_with("été 2023 à midi\n", "été 2024 à midi\n", &digits).is_identical());
    }

    #[test]
    fn rules_combine_with_the_whitespace_and_case_options() {
        let digits = rules(&[IgnoreRule::text("[0-9]+")]);
        let run = |l: &str, r: &str, whitespace, ignore_case| {
            let options = DiffOptions { whitespace, ignore_case };
            diff_lines_with(&lines(l), &lines(r), options, &digits)
        };
        let result = run("Build  12 OK\n", "build 345 ok\n", Whitespace::IgnoreAll, true);
        assert!(result.is_identical());
        assert_eq!(result.ignored_rows, [0]);
        assert!(!run("Build 12 OK\n", "build 345 ok\n", Whitespace::Exact, false).is_identical());
        // Trailing whitespace is still trailing after an ignored match.
        assert!(run("total 12  \n", "total 7\n", Whitespace::IgnoreTrailing, false).is_identical());
        // What the options alone hide is not reported as ignored by the rules.
        let result = run("ABC 1\nDEF\n", "abc 1\ndef\n", Whitespace::Exact, true);
        assert!(result.is_identical());
        assert!(result.ignored_rows.is_empty());
    }

    #[test]
    fn several_rules_are_applied_together() {
        let both = rules(&[preset("Date ISO"), preset("Heure"), IgnoreRule { enabled: false, ..IgnoreRule::text("user") }]);
        let left = "2024-03-15 10:30:00 login user\n";
        let right = "2025-01-02 23:59:59 login user\n";
        assert!(diff_with(left, right, &both).is_identical());
        assert!(!diff_with(left, right, &rules(&[preset("Date ISO")])).is_identical());
        assert!(!diff_with(left, "2025-01-02 23:59:59 login admin\n", &both).is_identical());
    }

    #[test]
    fn ignored_lines_that_differ_are_paired_without_a_block() {
        let comments = rules(&[IgnoreRule::whole_line("^#")]);
        let result = diff_with("x\n# generated by A\ny\n", "x\n# made with B\ny\n", &comments);
        assert!(result.is_identical());
        assert_complete(&result, 3, 3);
        assert_eq!(result.rows[1], Row { left: Some(1), right: Some(1), block: None, pair: None });
        assert_eq!(result.ignored_rows, [1]);
        assert_eq!(result.stats, DiffStats::default());
    }

    #[test]
    fn an_ignored_line_on_one_side_only_faces_a_filler_without_a_block() {
        let comments = rules(&[IgnoreRule::whole_line("^#")]);
        let result = diff_with("x\ny\n", "x\n# note\ny\n", &comments);
        assert!(result.is_identical());
        assert_complete(&result, 2, 3);
        let rows: Vec<_> = result.rows.iter().map(|r| (r.left, r.right, r.block)).collect();
        assert_eq!(rows, [(Some(0), Some(0), None), (None, Some(1), None), (Some(1), Some(2), None)]);
        assert_eq!(result.ignored_rows, [1]);
        assert_eq!(result.row_of_line(true, 1), Some(2));
        let result = diff_with("# only here\n# and here\n", "", &comments);
        assert!(result.is_identical());
        assert_complete(&result, 2, 0);
        assert_eq!(result.ignored_rows, [0, 1]);
    }

    #[test]
    fn ignored_lines_leave_the_block_of_the_real_changes_around_them() {
        let comments = rules(&[IgnoreRule::whole_line("^#")]);
        let left = "alpha is one\n# comment one\nbeta is one\nend\n";
        let right = "alpha is two\n# comment two\nbeta is two\nend\n";
        assert_eq!(diff(left, right).blocks.len(), 1);
        let result = diff_with(left, right, &comments);
        assert_complete(&result, 4, 4);
        assert_consistent(&result);
        assert_eq!(result.stats, DiffStats { insertions: 0, deletions: 0, changes: 2 });
        let block = |left: Range<u32>, right: Range<u32>, rows: Range<u32>| Block { kind: BlockKind::Change, left, right, rows };
        assert_eq!(result.blocks, [block(0..1, 0..1, 0..1), block(2..3, 2..3, 2..3)]);
        assert_eq!(result.rows[1].block, None);
        assert_eq!(result.ignored_rows, [1]);
        assert!(result.rows[0].pair.is_some() && result.rows[2].pair.is_some());
        assert_eq!(result.spans.len(), 2);
    }

    #[test]
    fn an_added_ignored_line_leaves_the_block_of_the_added_lines() {
        let comments = rules(&[IgnoreRule::whole_line("^#")]);
        let result = diff_with("a\nb\n", "a\n# note\nnew line\nb\n", &comments);
        assert_complete(&result, 2, 4);
        assert_consistent(&result);
        assert_eq!(
            result.blocks,
            [Block { kind: BlockKind::Insert, left: 1..1, right: 2..3, rows: 2..3 }]
        );
        assert_eq!(result.stats, DiffStats { insertions: 1, deletions: 0, changes: 0 });
        assert_eq!(result.ignored_rows, [1]);
    }

    #[test]
    fn an_ignored_line_facing_a_real_line_stays_a_difference() {
        let comments = rules(&[IgnoreRule::whole_line("^#")]);
        let result = diff_with("keep\n# note\n", "keep\ncode();\n", &comments);
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(result.blocks[0].kind, BlockKind::Change);
        assert_eq!(result.rows[1].block, Some(0));
        assert!(result.ignored_rows.is_empty());
        assert_consistent(&result);
    }

    #[test]
    fn a_whole_line_rule_matches_anywhere_unless_anchored() {
        let anywhere = rules(&[IgnoreRule::whole_line("Copyright")]);
        assert!(diff_with("// Copyright 2023 A\n", "/* Copyright 2024 B */\n", &anywhere).is_identical());
        let blank = rules(&[IgnoreRule::whole_line(r"^\s*$")]);
        let result = diff_with("a\n\n\nb\n", "a\nb\n  \n", &blank);
        assert!(result.is_identical());
        assert_complete(&result, 4, 3);
        assert_eq!(result.ignored_rows.len(), 3);
        let case = rules(&[IgnoreRule { case_insensitive: true, ..IgnoreRule::whole_line("^rem ") }]);
        assert!(diff_with("REM one\n", "rem two\n", &case).is_identical());
    }

    #[test]
    fn rows_stay_complete_and_consistent_on_a_noisy_file() {
        let line = |i: usize, side: usize| match i % 7 {
            0 => format!("# generated {} at 10:{:02}:{:02}", i + side, i % 60, (i * 7 + side) % 60),
            1 => format!("2024-03-{:02} value_{i} = {}", 1 + (i + side) % 28, i * (side + 1)),
            2 if i % 5 == side => format!("only on side {side} number {i}"),
            3 => format!("id = 123e4567-e89b-12d3-a456-{:012x}", i * 31 + side),
            4 if (i / 7) % 9 == 0 => format!("changed text {} for {i}", ["left", "right"][side]),
            _ => format!("stable line {i}"),
        };
        let left: Vec<String> = (0..3000).map(|i| line(i, 0)).collect();
        let mut right: Vec<String> = (0..3000).map(|i| line(i, 1)).collect();
        right.retain(|text| !text.ends_with("0:07:07"));
        let all: Vec<IgnoreRule> = IGNORE_PRESETS
            .iter()
            .map(IgnorePreset::rule)
            .chain([IgnoreRule::whole_line("^#")])
            .collect();
        let options = DiffOptions::default();
        let plain = diff_lines(&left, &right, options);
        let result = diff_lines_with(&left, &right, options, &rules(&all));
        assert_complete(&result, left.len(), right.len());
        assert_consistent(&result);
        assert_consistent(&plain);
        assert!(!result.blocks.is_empty() && result.blocks.len() < plain.blocks.len());
        assert!(!result.ignored_rows.is_empty());
        // No block is made of ignored lines only, and every span stays inside its line.
        for block in &result.blocks {
            let rows = &result.rows[block.rows.start as usize..block.rows.end as usize];
            let real = |line: Option<u32>, lines: &[String]| line.is_some_and(|l| !lines[l as usize].starts_with('#'));
            assert!(rows.iter().any(|row| real(row.left, &left) || real(row.right, &right)));
        }
        for row in &result.rows {
            let Some(pair) = row.pair else { continue };
            let spans = &result.spans[pair as usize];
            let (l, r) = (&left[row.left.unwrap() as usize], &right[row.right.unwrap() as usize]);
            assert!(spans.left.iter().all(|s| l.get(s.range.start as usize..s.range.end as usize).is_some()));
            assert!(spans.right.iter().all(|s| r.get(s.range.start as usize..s.range.end as usize).is_some()));
        }
    }
}
