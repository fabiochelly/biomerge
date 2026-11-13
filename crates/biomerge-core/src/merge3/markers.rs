//! Git conflict markers: the blocks written for unresolved conflicts, and the parser that
//! rebuilds the three versions of a working file git left conflicted.
//!
//! A marker is exactly seven `<`, `|`, `=` or `>` at the start of a line, alone or followed by
//! a space and a label. Longer runs (git's inner markers of a recursive merge are longer) and
//! runs glued to text are ordinary text. Outside a conflict only `<<<<<<<` means anything, so a
//! reStructuredText underline of `=======` is text; inside the right section, the only marker
//! expected is the closing one.

use std::ops::Range;

use crate::document::Line;

/// How unresolved conflicts are written, as git's `merge.conflictStyle`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarkerStyle {
    /// `<<<<<<<` left `=======` right `>>>>>>>`.
    #[default]
    Merge,
    /// With the base between `|||||||` and `=======`. As conflicts are already trimmed of the
    /// lines both sides share, this is what git calls `zdiff3`, which any diff3 reader accepts.
    Diff3,
}

/// The labels after the markers; empty writes a bare marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerLabels {
    pub left: String,
    pub base: String,
    pub right: String,
}

impl Default for MarkerLabels {
    /// Git's labels for `git checkout --conflict`.
    fn default() -> Self {
        Self {
            left: "ours".into(),
            base: "base".into(),
            right: "theirs".into(),
        }
    }
}

/// The label of a marker line made of `mark`, or `None` when the line is not one.
pub(super) fn marker(text: &str, mark: u8) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.len() < 7 || bytes[..7].iter().any(|&b| b != mark) {
        return None;
    }
    match bytes.get(7) {
        None => Some(""),
        Some(b' ') => Some(&text[8..]),
        Some(_) => None,
    }
}

pub(super) fn marker_line(mark: char, label: &str) -> String {
    let mut line: String = std::iter::repeat_n(mark, 7).collect();
    if !label.is_empty() {
        line.push(' ');
        line.push_str(label);
    }
    line
}

/// A part of a conflicted file, in line indices of that file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Region {
    Common(Range<usize>),
    Conflict {
        left: Range<usize>,
        base: Option<Range<usize>>,
        right: Range<usize>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Parsed {
    pub regions: Vec<Region>,
    /// `Diff3` when a conflict has a base section.
    pub style: MarkerStyle,
    /// Those of the first conflict, to write the file back as it came.
    pub labels: MarkerLabels,
    pub conflicts: usize,
}

/// Splits a conflicted file into common text and conflicts. A malformed conflict (nested
/// opening marker, separator or closing marker missing) is an error giving its 1-based line.
pub(super) fn parse(lines: &[Line]) -> Result<Parsed, usize> {
    enum State {
        Out,
        Left,
        Base,
        Right,
    }
    let mut parsed = Parsed {
        regions: Vec::new(),
        style: MarkerStyle::Merge,
        labels: MarkerLabels { left: String::new(), base: String::new(), right: String::new() },
        conflicts: 0,
    };
    let mut state = State::Out;
    let (mut common, mut opened) = (0, 0);
    let (mut left, mut base, mut right) = (0..0, None::<Range<usize>>, 0..0);
    for (index, line) in lines.iter().enumerate() {
        let text = line.text.as_str();
        let first = parsed.conflicts == 0;
        match state {
            State::Out => {
                if let Some(label) = marker(text, b'<') {
                    if common < index {
                        parsed.regions.push(Region::Common(common..index));
                    }
                    if first {
                        parsed.labels.left = label.to_owned();
                    }
                    opened = index;
                    left = index + 1..index + 1;
                    base = None;
                    state = State::Left;
                }
            }
            State::Left | State::Base if marker(text, b'<').is_some() || marker(text, b'>').is_some() => return Err(index + 1),
            State::Left => {
                if let Some(label) = marker(text, b'|') {
                    if first {
                        parsed.labels.base = label.to_owned();
                    }
                    left.end = index;
                    base = Some(index + 1..index + 1);
                    state = State::Base;
                } else if marker(text, b'=').is_some() {
                    left.end = index;
                    right = index + 1..index + 1;
                    state = State::Right;
                }
            }
            State::Base => {
                if marker(text, b'=').is_some() {
                    base.as_mut().unwrap().end = index;
                    right = index + 1..index + 1;
                    state = State::Right;
                }
            }
            State::Right => {
                if let Some(label) = marker(text, b'>') {
                    if first {
                        parsed.labels.right = label.to_owned();
                    }
                    right.end = index;
                    if base.is_some() {
                        parsed.style = MarkerStyle::Diff3;
                    }
                    parsed.regions.push(Region::Conflict { left: left.clone(), base: base.take(), right: right.clone() });
                    parsed.conflicts += 1;
                    common = index + 1;
                    state = State::Out;
                } else if marker(text, b'<').is_some() {
                    return Err(index + 1);
                }
            }
        }
    }
    if !matches!(state, State::Out) {
        return Err(opened + 1);
    }
    if common < lines.len() {
        parsed.regions.push(Region::Common(common..lines.len()));
    }
    Ok(parsed)
}

/// True when the lines hold at least one well-formed git conflict: the file can be opened as a
/// merge to resolve.
pub fn has_conflict_markers(lines: &[Line]) -> bool {
    // Cheap rejection first: most files have no opening marker at all.
    lines.iter().any(|line| marker(&line.text, b'<').is_some()) && parse(lines).is_ok_and(|parsed| parsed.conflicts > 0)
}
