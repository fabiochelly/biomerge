//! Three-way merge: a base (the common ancestor), a left side (ours, git's LOCAL) and a right
//! side (theirs, git's REMOTE) make an editable result (git's MERGED), as `git mergetool` needs.
//!
//! - Chunks: the line diffs base→left and base→right are the ones the two-way comparison shows
//!   (same keys under the options and text rules, Histogram and indent heuristic). Walking them
//!   together splits the three texts into [`Chunk`]s: unchanged, changed on one side, changed
//!   identically on both, or conflicting. Conflicts are trimmed of the lines both sides share,
//!   and those whose sides changed different base lines carry a proposal combining both.
//! - Result: a [`Document`] built from the chunks, each chunk owning a range of its lines. A
//!   [`Resolution`] says what a chunk shows; changing it, or typing in the result, returns a
//!   [`MergeEdit`] that undoes and redoes the text and the chunk states together.
//! - Typing never re-runs the diffs: base, left and right do not change, so only the result
//!   ranges move (by index, O(chunks)), and the chunks the edit touched become `Edited`.
//! - Conflict markers: unresolved conflicts are written as git markers when saving is forced,
//!   and a working file holding markers can be opened as a merge ([`Merge::open_conflicted`]).
//!
//! Text policy. Under options that hide differences (whitespace, case, text ignore rules),
//! lines equal under them are equal for the merge, and the text kept must still be a real one:
//! in an unchanged chunk each line is the left one, unless only the right side changed it (it
//! is then the right one); a chunk changed identically on both sides keeps the left text; a
//! one-sided change keeps the text of the side that changed. Whole-line ignore rules are not
//! applied: hiding a line that exists on one side only would drop or duplicate real text.
//!
//! Line endings and encoding follow the three-way rule (what one side changed wins; when both
//! changed, the left wins), an empty or missing text having no say: the encoding (BOM
//! included), the dominant line ending and the final newline. Lines coming from a text whose
//! dominant ending is the result's keep their own ending (a mixed file stays mixed), the others
//! take the result's.

mod chunks;
mod markers;
#[cfg(test)]
mod tests;

use std::fs;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use markers::{MarkerLabels, MarkerStyle, has_conflict_markers};

use crate::diff::{DiffOptions, DiffResult, IgnoreRules, Whitespace, comparison_keys, diff_lines_with};
use crate::document::{Document, DocumentError, Edit, Eol, Line, TextEncoding};
use chunks::{Keys, chunk_of, conflict_chunks};
use markers::{Region, marker, marker_line};

/// One of the four texts of a merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Version {
    Base,
    Left,
    Right,
    Result,
}

impl Version {
    /// Its name in messages.
    pub fn label(self) -> &'static str {
        match self {
            Version::Base => "ancêtre commun",
            Version::Left => "version locale",
            Version::Right => "version distante",
            Version::Result => "résultat",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkKind {
    /// The three texts agree (under the options).
    Unchanged,
    /// Only the left side changed the base here.
    LeftOnly,
    /// Only the right side changed the base here.
    RightOnly,
    /// Both sides made the same change: not a conflict.
    Both,
    Conflict,
}

/// What a chunk's result lines are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// A conflict not resolved yet. Its result lines are the base text (empty when both sides
    /// inserted at the same place), editable; a forced save writes markers in their place.
    Unresolved,
    /// Both sides' changes applied: the automatic resolution of every chunk that is not a
    /// conflict, and for a `combinable` conflict the proposal combining its two sides.
    Merged,
    Base,
    Left,
    Right,
    LeftThenRight,
    RightThenLeft,
    /// Typed by hand, or set with [`Merge::set_lines`].
    Edited,
}

/// A range of lines of one input text, a part of a chunk's content.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Piece {
    version: Version,
    range: Range<u32>,
}

/// A part of the merge, with its lines in each of the four texts. In each text the ranges of
/// successive chunks follow one another without gap; a range can be empty (a deletion, an
/// insertion on the other side, a conflict resolved to nothing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub kind: ChunkKind,
    pub base: Range<u32>,
    pub left: Range<u32>,
    pub right: Range<u32>,
    pub result: Range<u32>,
    pub resolution: Resolution,
    /// A conflict whose two sides changed different base lines (adjacent ones, which git
    /// reports as a conflict): [`Resolution::Merged`] applies both changes. Only a proposal,
    /// unless [`MergeOptions::combine_adjacent`] applied it.
    pub combinable: bool,
    /// Edited by hand and still holding a `<<<<<<<` or `>>>>>>>` marker line: counted as an
    /// unresolved conflict.
    pub has_markers: bool,
    /// The content of the proposal of a combinable conflict.
    combined: Vec<Piece>,
}

impl Chunk {
    pub fn range(&self, version: Version) -> Range<u32> {
        match version {
            Version::Base => self.base.clone(),
            Version::Left => self.left.clone(),
            Version::Right => self.right.clone(),
            Version::Result => self.result.clone(),
        }
    }

    pub fn is_conflict(&self) -> bool {
        self.kind == ChunkKind::Conflict
    }

    /// An unresolved conflict, or a chunk edited by hand that still holds markers.
    pub fn is_unresolved(&self) -> bool {
        (self.is_conflict() && self.resolution == Resolution::Unresolved) || self.has_markers
    }
}

#[derive(Clone, Debug, Default)]
pub struct MergeOptions {
    pub diff: DiffOptions,
    /// Only the text rules apply, see the module documentation.
    pub rules: IgnoreRules,
    /// Resolve conflicts whose sides changed different (adjacent) lines with the combination of
    /// both, instead of leaving them to the user as git does. Off by default.
    pub combine_adjacent: bool,
}

/// Where navigation stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Every chunk that is not unchanged.
    Change,
    Conflict,
    /// What [`Chunk::is_unresolved`] says.
    Unresolved,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeCounts {
    pub conflicts: usize,
    /// Unresolved conflicts plus chunks edited by hand that still hold markers.
    pub unresolved: usize,
    /// Conflicts offering a combination of both sides.
    pub combinable: usize,
    /// Changes merged automatically from the left side only, the right side only, both.
    pub left: usize,
    pub right: usize,
    pub both: usize,
    /// Conflicts resolved by the combination of both sides.
    pub combined: usize,
    pub edited: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error("{path} ({}) : fichier binaire, fusion impossible", version.label())]
    Binary { path: PathBuf, version: Version },
    #[error("{path} ({}) : texte indécodable, fusion impossible", version.label())]
    Undecodable { path: PathBuf, version: Version },
    #[error("{}", unresolved_message(*.0))]
    Unresolved(usize),
    #[error("{path} : marqueurs de conflit mal formés à la ligne {line}")]
    Markers { path: PathBuf, line: usize },
}

fn unresolved_message(count: usize) -> String {
    match count {
        1 => "1 conflit non résolu : enregistrement refusé".to_owned(),
        n => format!("{n} conflits non résolus : enregistrement refusé"),
    }
}

/// The state of a chunk that an edit changes, to restore it on undo or redo.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChunkState {
    result: Range<u32>,
    resolution: Resolution,
    has_markers: bool,
}

/// One undoable step: the edit of the result text, and the chunk states before and after it.
/// Keep it on the undo stack and give it back to [`Merge::undo`] and [`Merge::redo`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeEdit {
    pub edit: Edit,
    first: usize,
    before: Vec<ChunkState>,
    after: Vec<ChunkState>,
}

#[derive(Clone, Debug)]
pub struct Merge {
    pub base: Document,
    pub left: Document,
    pub right: Document,
    /// The result. Edit it through the merge, or report each edit to [`Merge::track_edit`].
    pub result: Document,
    /// How a forced save writes unresolved conflicts; a conflicted file keeps its own.
    pub marker_style: MarkerStyle,
    pub labels: MarkerLabels,
    chunks: Vec<Chunk>,
    options: MergeOptions,
    /// Base, left, right: the file did not exist (git passes no file or `/dev/null`).
    missing: [bool; 3],
    /// False for a conflicted file written without base sections: its base is unknown.
    base_known: bool,
    /// The ending of new lines, and which inputs keep their own endings.
    eol: Eol,
    own_eol: [bool; 3],
    /// Whether the result ends with a line ending, used while it is empty.
    final_eol: bool,
}

/// The comparison keys of the three inputs, computed side by side.
fn keys_of(docs: [&Document; 3], options: &MergeOptions) -> [Vec<String>; 3] {
    let keys = |doc: &Document| {
        let texts: Vec<&str> = doc.lines.iter().map(|line| line.text.as_str()).collect();
        comparison_keys(&texts, options.diff, &options.rules).0
    };
    let (base, (left, right)) = rayon::join(|| keys(docs[0]), || rayon::join(|| keys(docs[1]), || keys(docs[2])));
    [base, left, right]
}

/// The three-way rule: what one side changed wins, and the left when both did. `None` has no
/// say (an empty or missing text).
fn three_way<T: PartialEq + Copy>(base: Option<T>, left: Option<T>, right: Option<T>) -> Option<T> {
    match (base, left, right) {
        (Some(b), Some(l), Some(r)) => Some(if l == b { r } else { l }),
        (_, left, right) => left.or(right).or(base),
    }
}

fn is_null_device(path: &Path) -> bool {
    path == Path::new("/dev/null") || path.as_os_str().eq_ignore_ascii_case("nul")
}

/// Loads one input; a path that does not exist (or the null device) is an empty missing text.
fn load_input(path: &Path, version: Version) -> Result<(Document, bool), MergeError> {
    match fs::metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((Document::from_text(""), true)),
        _ if is_null_device(path) => return Ok((Document::from_text(""), true)),
        _ => {}
    }
    match Document::load(path) {
        Ok(doc) if doc.lossy => Err(MergeError::Undecodable { path: path.to_owned(), version }),
        Ok(doc) => Ok((doc, false)),
        Err(DocumentError::Binary(path)) => Err(MergeError::Binary { path, version }),
        Err(error) => Err(error.into()),
    }
}

impl Merge {
    /// Merges three texts. The result has no path yet: set `result.path` before saving.
    pub fn new(base: Document, left: Document, right: Document, options: MergeOptions) -> Result<Merge, MergeError> {
        for (doc, version) in [(&base, Version::Base), (&left, Version::Left), (&right, Version::Right)] {
            if doc.lossy {
                let path = doc.path.clone().unwrap_or_default();
                return Err(MergeError::Undecodable { path, version });
            }
        }
        let output = Document::from_text("");
        Ok(Self::from_inputs(base, left, right, output, [false; 3], options))
    }

    /// Merges three files into `output`, as a mergetool: `git mergetool` passes `$BASE $LOCAL
    /// $REMOTE $MERGED`. A missing input (or `/dev/null`) is an empty text flagged missing. The
    /// output's current content (git's markers) is ignored, but its disk state is recorded so
    /// that saving never overwrites a later change.
    pub fn open(base: &Path, left: &Path, right: &Path, output: &Path, options: MergeOptions) -> Result<Merge, MergeError> {
        let (base, base_missing) = load_input(base, Version::Base)?;
        let (left, left_missing) = load_input(left, Version::Left)?;
        let (right, right_missing) = load_input(right, Version::Right)?;
        let mut result = Document::load(output).unwrap_or_else(|_| Document::from_text(""));
        result.path = Some(output.to_owned());
        Ok(Self::from_inputs(base, left, right, result, [base_missing, left_missing, right_missing], options))
    }

    fn from_inputs(base: Document, left: Document, right: Document, mut output: Document, missing: [bool; 3], options: MergeOptions) -> Merge {
        let keys = keys_of([&base, &left, &right], &options);
        let chunks = chunks::chunks(&Keys { base: &keys[0], left: &keys[1], right: &keys[2] });
        drop(keys);
        let vote = |doc: &Document| (!doc.is_empty()).then_some(doc.encoding);
        output.encoding = three_way(vote(&base), vote(&left), vote(&right)).unwrap_or(TextEncoding::Utf8);
        Self::assemble(base, left, right, output, chunks, missing, true, options)
    }

    /// Opens a working file git left with conflict markers, in either style: its common text
    /// and the sections of each conflict rebuild the left, right and base texts (the base is
    /// unknown without `|||||||` sections, see [`Merge::base_known`]). Each conflict stays a
    /// conflict, trimmed and given a proposal as in a merge; nothing else is merged again. The
    /// result is that file, and keeps its marker style and labels for a forced save.
    pub fn open_conflicted(path: &Path, options: MergeOptions) -> Result<Merge, MergeError> {
        let doc = Document::load(path).map_err(|error| match error {
            DocumentError::Binary(path) => MergeError::Binary { path, version: Version::Result },
            error => error.into(),
        })?;
        if doc.lossy {
            return Err(MergeError::Undecodable { path: path.to_owned(), version: Version::Result });
        }
        Self::from_conflicted(doc, options)
    }

    /// As [`Merge::open_conflicted`], from a document already loaded.
    pub fn from_conflicted(doc: Document, options: MergeOptions) -> Result<Merge, MergeError> {
        let parsed = markers::parse(&doc.lines).map_err(|line| MergeError::Markers {
            path: doc.path.clone().unwrap_or_default(),
            line,
        })?;
        let mut versions: [Vec<Line>; 3] = Default::default();
        // Per region, where it starts in base, left and right.
        let mut starts = Vec::with_capacity(parsed.regions.len());
        let take = |lines: &mut Vec<Line>, range: &Range<usize>| lines.extend_from_slice(&doc.lines[range.clone()]);
        let mut base_known = true;
        for region in &parsed.regions {
            starts.push(versions.each_ref().map(|lines| lines.len() as u32));
            match region {
                Region::Common(range) => versions.iter_mut().for_each(|lines| take(lines, range)),
                Region::Conflict { left, base, right } => {
                    match base {
                        Some(base) => take(&mut versions[0], base),
                        None => base_known = false,
                    }
                    take(&mut versions[1], left);
                    take(&mut versions[2], right);
                }
            }
        }
        // Each text ends as the file does.
        let final_eol = doc.lines.last().is_none_or(|line| line.eol != Eol::None);
        let [base, left, right] = versions.map(|mut lines| {
            if let Some(last) = lines.last_mut() {
                last.eol = if final_eol && last.eol == Eol::None { doc.dominant_eol() } else if final_eol { last.eol } else { Eol::None };
            }
            let mut version = Document::from_text("");
            version.lines = Arc::new(lines);
            version
        });

        let keys = keys_of([&base, &left, &right], &options);
        let keys = Keys { base: &keys[0], left: &keys[1], right: &keys[2] };
        let mut chunks = Vec::new();
        for (index, region) in parsed.regions.iter().enumerate() {
            let [b, l, r] = starts[index];
            let next = starts.get(index + 1).copied().unwrap_or([base.len() as u32, left.len() as u32, right.len() as u32]);
            match region {
                Region::Common(_) => chunks.push(chunk_of(ChunkKind::Unchanged, b..next[0], l..next[1], r..next[2])),
                Region::Conflict { .. } => conflict_chunks(&keys, b..next[0], l..next[1], r..next[2], &mut chunks),
            }
        }
        if chunks.is_empty() {
            chunks.push(chunk_of(ChunkKind::Unchanged, 0..0, 0..0, 0..0));
        }
        let mut merge = Self::assemble(base, left, right, doc, chunks, [false; 3], base_known, options);
        merge.marker_style = parsed.style;
        merge.labels = parsed.labels;
        Ok(merge)
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        base: Document,
        left: Document,
        right: Document,
        mut output: Document,
        mut chunks: Vec<Chunk>,
        missing: [bool; 3],
        base_known: bool,
        options: MergeOptions,
    ) -> Merge {
        let docs = [&base, &left, &right];
        let dominant = |doc: &Document| (!doc.is_empty()).then(|| doc.dominant_eol());
        let eol = three_way(dominant(&base), dominant(&left), dominant(&right)).unwrap_or(Eol::Lf);
        let own_eol = docs.map(|doc| dominant(doc) == Some(eol));
        let ending = |doc: &Document| doc.lines.last().map(|line| line.eol != Eol::None);
        let final_eol = three_way(ending(&base), ending(&left), ending(&right)).unwrap_or(true);
        for chunk in &mut chunks {
            if chunk.is_conflict() {
                chunk.resolution = if options.combine_adjacent && chunk.combinable { Resolution::Merged } else { Resolution::Unresolved };
            }
        }
        output.lines = Arc::new(Vec::new());
        output.lossy = false;
        let mut merge = Merge {
            base,
            left,
            right,
            result: output,
            marker_style: MarkerStyle::default(),
            labels: MarkerLabels::default(),
            chunks,
            options,
            missing,
            base_known,
            eol,
            own_eol,
            final_eol,
        };
        let mut lines = Vec::with_capacity(merge.left.len().max(merge.right.len()));
        for index in 0..merge.chunks.len() {
            let start = lines.len() as u32;
            let chunk = &merge.chunks[index];
            lines.extend(merge.lines_of(&merge.pieces(chunk, chunk.resolution)));
            merge.chunks[index].result = start..lines.len() as u32;
        }
        if let Some(last) = lines.last_mut() {
            last.eol = if !final_eol { Eol::None } else if last.eol == Eol::None { eol } else { last.eol };
        }
        merge.result.lines = Arc::new(lines);
        merge
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    pub fn options(&self) -> &MergeOptions {
        &self.options
    }

    pub fn document(&self, version: Version) -> &Document {
        match version {
            Version::Base => &self.base,
            Version::Left => &self.left,
            Version::Right => &self.right,
            Version::Result => &self.result,
        }
    }

    /// True when that input did not exist: deleted on that side, or added on the others.
    pub fn is_missing(&self, version: Version) -> bool {
        match version {
            Version::Base => self.missing[0],
            Version::Left => self.missing[1],
            Version::Right => self.missing[2],
            Version::Result => false,
        }
    }

    /// False for a conflicted file without base sections: base ranges of conflicts are empty
    /// and taking the base means nothing, so do not offer it.
    pub fn base_known(&self) -> bool {
        self.base_known
    }

    /// What a resolution makes of a chunk.
    fn pieces(&self, chunk: &Chunk, resolution: Resolution) -> Vec<Piece> {
        let piece = |version: Version| Piece { version, range: chunk.range(version) };
        match resolution {
            Resolution::Unresolved | Resolution::Base => vec![piece(Version::Base)],
            Resolution::Left => vec![piece(Version::Left)],
            Resolution::Right => vec![piece(Version::Right)],
            Resolution::LeftThenRight => vec![piece(Version::Left), piece(Version::Right)],
            Resolution::RightThenLeft => vec![piece(Version::Right), piece(Version::Left)],
            Resolution::Edited => vec![piece(Version::Result)],
            Resolution::Merged => match chunk.kind {
                ChunkKind::LeftOnly | ChunkKind::Both => vec![piece(Version::Left)],
                ChunkKind::RightOnly => vec![piece(Version::Right)],
                ChunkKind::Conflict => chunk.combined.clone(),
                ChunkKind::Unchanged => self.unchanged_pieces(chunk),
            },
        }
    }

    /// Lines equal under the options may still differ: each is the left one, unless only the
    /// right side changed it.
    fn unchanged_pieces(&self, chunk: &Chunk) -> Vec<Piece> {
        let exact = self.options.diff.whitespace == Whitespace::Exact && !self.options.diff.ignore_case && self.options.rules.is_empty();
        if exact {
            return vec![Piece { version: Version::Left, range: chunk.left.clone() }];
        }
        let mut pieces: Vec<Piece> = Vec::new();
        for offset in 0..chunk.base.len() as u32 {
            let text = |version: Version| {
                let line = chunk.range(version).start + offset;
                self.document(version).lines[line as usize].text.as_str()
            };
            let base = text(Version::Base);
            let right_only = text(Version::Left) == base && text(Version::Right) != base;
            let (version, start) = if right_only { (Version::Right, chunk.right.start) } else { (Version::Left, chunk.left.start) };
            match pieces.last_mut() {
                Some(last) if last.version == version => last.range.end += 1,
                _ => pieces.push(Piece { version, range: start + offset..start + offset + 1 }),
            }
        }
        pieces
    }

    fn lines_of(&self, pieces: &[Piece]) -> Vec<Line> {
        let mut lines = Vec::with_capacity(pieces.iter().map(|piece| piece.range.len()).sum());
        for piece in pieces {
            let own = match piece.version {
                Version::Base => self.own_eol[0],
                Version::Left => self.own_eol[1],
                Version::Right => self.own_eol[2],
                Version::Result => true,
            };
            let source = &self.document(piece.version).lines[piece.range.start as usize..piece.range.end as usize];
            lines.extend(source.iter().map(|line| Line {
                text: line.text.clone(),
                eol: if own && line.eol != Eol::None { line.eol } else { self.eol },
            }));
        }
        lines
    }

    /// The lines a resolution would give a chunk, without applying it (for a preview).
    pub fn resolution_lines(&self, chunk: usize, resolution: Resolution) -> Vec<String> {
        let pieces = self.pieces(&self.chunks[chunk], resolution);
        self.lines_of(&pieces).into_iter().map(|line| line.text).collect()
    }

    /// Changes what a chunk shows. `None` when nothing changes or the resolution does not apply:
    /// `Unresolved` to a chunk that is not a conflict, `Merged` to a conflict that is not
    /// combinable, `Edited` (use [`Merge::set_lines`]). Any chunk can take a side, its base or
    /// both sides, not only a conflict.
    pub fn set_resolution(&mut self, chunk: usize, resolution: Resolution) -> Option<MergeEdit> {
        let current = &self.chunks[chunk];
        let applies = match resolution {
            Resolution::Unresolved => current.is_conflict(),
            Resolution::Merged => !current.is_conflict() || current.combinable,
            Resolution::Edited => false,
            _ => true,
        };
        if !applies || (current.resolution == resolution && !current.has_markers) {
            return None;
        }
        let lines = self.lines_of(&self.pieces(current, resolution));
        Some(self.replace_chunk(chunk, lines, resolution))
    }

    /// Replaces a chunk's result lines by `texts`, as an edit by hand: the conflict is resolved
    /// unless the text holds markers.
    pub fn set_lines(&mut self, chunk: usize, texts: Vec<String>) -> MergeEdit {
        let lines = texts.into_iter().map(|text| Line { text, eol: self.eol }).collect();
        self.replace_chunk(chunk, lines, Resolution::Edited)
    }

    /// The unresolved conflict written with markers, in the merge's style and labels, for an
    /// edit by hand of the markers ([`Merge::set_lines`]).
    pub fn marker_lines(&self, chunk: usize) -> Vec<String> {
        let chunk = &self.chunks[chunk];
        let section = |doc: &Document, range: &Range<u32>| -> Vec<String> {
            doc.lines[range.start as usize..range.end as usize].iter().map(|line| line.text.clone()).collect()
        };
        let mut lines = vec![marker_line('<', &self.labels.left)];
        lines.extend(section(&self.left, &chunk.left));
        if self.marker_style == MarkerStyle::Diff3 && self.base_known {
            lines.push(marker_line('|', &self.labels.base));
            lines.extend(section(&self.base, &chunk.base));
        }
        lines.push(marker_line('=', ""));
        lines.extend(section(&self.right, &chunk.right));
        lines.push(marker_line('>', &self.labels.right));
        lines
    }

    fn state(&self, chunk: usize) -> ChunkState {
        let chunk = &self.chunks[chunk];
        ChunkState { result: chunk.result.clone(), resolution: chunk.resolution, has_markers: chunk.has_markers }
    }

    fn shift_after(&mut self, last: usize, delta: i64) {
        if delta == 0 {
            return;
        }
        let at = |line: u32| (line as i64 + delta) as u32;
        for chunk in &mut self.chunks[last + 1..] {
            chunk.result = at(chunk.result.start)..at(chunk.result.end);
        }
    }

    fn holds_markers(&self, chunk: usize) -> bool {
        let range = &self.chunks[chunk].result;
        self.result.lines[range.start as usize..range.end as usize]
            .iter()
            .any(|line| marker(&line.text, b'<').is_some() || marker(&line.text, b'>').is_some())
    }

    fn replace_chunk(&mut self, index: usize, lines: Vec<Line>, resolution: Resolution) -> MergeEdit {
        let before = vec![self.state(index)];
        let range = self.chunks[index].result.clone();
        let count = lines.len() as u32;
        let edit = self.splice(range.start as usize..range.end as usize, lines);
        let chunk = &mut self.chunks[index];
        chunk.result = range.start..range.start + count;
        chunk.resolution = resolution;
        self.chunks[index].has_markers = resolution == Resolution::Edited && self.holds_markers(index);
        self.shift_after(index, count as i64 - range.len() as i64);
        MergeEdit { edit, first: index, before, after: vec![self.state(index)] }
    }

    /// Replaces result lines, keeping whether the result ends with a line ending (as
    /// [`Document::replace_lines`] does): a line that becomes or stops being the last one is
    /// part of the edit for its ending.
    fn splice(&mut self, range: Range<usize>, mut inserted: Vec<Line>) -> Edit {
        let lines = &self.result.lines;
        let len = lines.len();
        let final_eol = lines.last().map_or(self.final_eol, |line| line.eol != Eol::None);
        let mut at = range.start;
        if range.end == len {
            if let Some(last) = inserted.last_mut() {
                last.eol = if !final_eol { Eol::None } else if last.eol == Eol::None { self.eol } else { last.eol };
                if at == len && at > 0 && lines[at - 1].eol == Eol::None {
                    inserted.insert(0, Line { text: lines[at - 1].text.clone(), eol: self.eol });
                    at -= 1;
                }
            } else if at > 0 && at < len && !final_eol {
                inserted.push(Line { text: lines[at - 1].text.clone(), eol: Eol::None });
                at -= 1;
            }
        }
        let edit = Edit { at, removed: lines[at..range.end].to_vec(), inserted };
        self.result.apply(&edit);
        edit
    }

    /// Records an edit already applied to `self.result` (typing, pasting, any
    /// [`crate::textedit`] function), and returns the step to keep for undo. Chunks after the
    /// edit move; the chunks it touched become `Edited` when their text changed. When it spans
    /// several chunks, the first one takes the new lines, the last one keeps its lines after
    /// the edit, and those between become empty. A pure insertion (typing in an empty result)
    /// goes to an empty chunk standing at that line, else to the chunk holding it.
    pub fn track_edit(&mut self, edit: &Edit) -> MergeEdit {
        let (at, removed, inserted) = (edit.at as u32, edit.removed.len() as u32, edit.inserted.len() as u32);
        let delta = inserted as i64 - removed as i64;
        let chunks = &self.chunks;
        let (first, last) = if removed == 0 {
            let from = chunks.partition_point(|chunk| chunk.result.end < at);
            let found = chunks[from..]
                .iter()
                .position(|chunk| (chunk.result.is_empty() && chunk.result.start == at) || chunk.result.end > at);
            let index = found.map_or(chunks.len() - 1, |offset| from + offset);
            (index, index)
        } else {
            let first = chunks.partition_point(|chunk| chunk.result.end <= at);
            let last = chunks.partition_point(|chunk| chunk.result.start < at + removed) - 1;
            (first, last)
        };
        let before: Vec<ChunkState> = (first..=last).map(|index| self.state(index)).collect();
        let changed = edit.removed.iter().map(|line| &line.text).ne(edit.inserted.iter().map(|line| &line.text));
        if changed {
            let moved = |line: u32| (line as i64 + delta) as u32;
            if first == last {
                let chunk = &mut self.chunks[first];
                chunk.result.end = moved(chunk.result.end);
            } else {
                let split = at + inserted;
                self.chunks[first].result.end = split;
                for chunk in &mut self.chunks[first + 1..last] {
                    chunk.result = split..split;
                }
                let end = moved(self.chunks[last].result.end);
                self.chunks[last].result = split..end;
            }
            for index in first..=last {
                self.chunks[index].resolution = Resolution::Edited;
                self.chunks[index].has_markers = self.holds_markers(index);
            }
            self.shift_after(last, delta);
        }
        let after = (first..=last).map(|index| self.state(index)).collect();
        MergeEdit { edit: edit.clone(), first, before, after }
    }

    pub fn undo(&mut self, step: &MergeEdit) {
        self.result.apply(&step.edit.inverted());
        self.restore(step.first, &step.before, step.edit.removed.len() as i64 - step.edit.inserted.len() as i64);
    }

    pub fn redo(&mut self, step: &MergeEdit) {
        self.result.apply(&step.edit);
        self.restore(step.first, &step.after, step.edit.inserted.len() as i64 - step.edit.removed.len() as i64);
    }

    fn restore(&mut self, first: usize, states: &[ChunkState], delta: i64) {
        for (offset, state) in states.iter().enumerate() {
            let chunk = &mut self.chunks[first + offset];
            chunk.result = state.result.clone();
            chunk.resolution = state.resolution;
            chunk.has_markers = state.has_markers;
        }
        self.shift_after(first + states.len() - 1, delta);
    }

    pub fn counts(&self) -> MergeCounts {
        let mut counts = MergeCounts::default();
        for chunk in &self.chunks {
            let merged = chunk.resolution == Resolution::Merged;
            counts.conflicts += chunk.is_conflict() as usize;
            counts.unresolved += chunk.is_unresolved() as usize;
            counts.combinable += chunk.combinable as usize;
            counts.edited += (chunk.resolution == Resolution::Edited) as usize;
            match chunk.kind {
                ChunkKind::LeftOnly => counts.left += merged as usize,
                ChunkKind::RightOnly => counts.right += merged as usize,
                ChunkKind::Both => counts.both += merged as usize,
                ChunkKind::Conflict => counts.combined += merged as usize,
                ChunkKind::Unchanged => {}
            }
        }
        counts
    }

    pub fn unresolved_count(&self) -> usize {
        self.chunks.iter().filter(|chunk| chunk.is_unresolved()).count()
    }

    fn stops(&self, chunk: &Chunk, stop: Stop) -> bool {
        match stop {
            Stop::Change => chunk.kind != ChunkKind::Unchanged,
            Stop::Conflict => chunk.is_conflict(),
            Stop::Unresolved => chunk.is_unresolved(),
        }
    }

    /// The first chunk after `after` (from the start with `None`) where `stop` stops.
    pub fn next(&self, after: Option<usize>, stop: Stop) -> Option<usize> {
        let from = after.map_or(0, |index| index + 1);
        (from..self.chunks.len()).find(|&index| self.stops(&self.chunks[index], stop))
    }

    /// The last chunk before `before` (from the end with `None`) where `stop` stops.
    pub fn previous(&self, before: Option<usize>, stop: Stop) -> Option<usize> {
        let to = before.unwrap_or(self.chunks.len()).min(self.chunks.len());
        (0..to).rev().find(|&index| self.stops(&self.chunks[index], stop))
    }

    /// The chunk holding `line` of a text, `None` past its end.
    pub fn chunk_at(&self, version: Version, line: u32) -> Option<usize> {
        let index = self.chunks.partition_point(|chunk| chunk.range(version).end <= line);
        (index < self.chunks.len()).then_some(index)
    }

    /// The two-way comparison of a chunk between two texts (base and left, base and right,
    /// left and right…): row pairing and token spans, from [`diff_lines_with`] under the
    /// merge's options. Line numbers are those of the whole texts.
    pub fn compare(&self, chunk: usize, from: Version, to: Version) -> DiffResult {
        let chunk = &self.chunks[chunk];
        let (from_range, to_range) = (chunk.range(from), chunk.range(to));
        let slice = |version: Version, range: &Range<u32>| -> Vec<&str> {
            self.document(version).lines[range.start as usize..range.end as usize].iter().map(|line| line.text.as_str()).collect()
        };
        let mut result = diff_lines_with(&slice(from, &from_range), &slice(to, &to_range), self.options.diff, &self.options.rules);
        let (left, right) = (from_range.start, to_range.start);
        for row in &mut result.rows {
            row.left = row.left.map(|line| line + left);
            row.right = row.right.map(|line| line + right);
        }
        for block in &mut result.blocks {
            block.left = block.left.start + left..block.left.end + left;
            block.right = block.right.start + right..block.right.end + right;
        }
        result
    }

    /// The result with every unresolved conflict written as git markers in place of its lines.
    pub fn marked_lines(&self) -> Vec<Line> {
        let mut lines = Vec::with_capacity(self.result.len());
        for (index, chunk) in self.chunks.iter().enumerate() {
            if chunk.is_conflict() && chunk.resolution == Resolution::Unresolved {
                lines.extend(self.marker_lines(index).into_iter().map(|text| Line { text, eol: self.eol }));
            } else {
                lines.extend_from_slice(&self.result.lines[chunk.result.start as usize..chunk.result.end as usize]);
            }
        }
        let final_eol = self.result.lines.last().map_or(self.final_eol, |line| line.eol != Eol::None);
        let count = lines.len();
        for (index, line) in lines.iter_mut().enumerate() {
            if index + 1 == count && !final_eol {
                line.eol = Eol::None;
            } else if line.eol == Eol::None {
                line.eol = self.eol;
            }
        }
        lines
    }

    /// Saves the result. While conflicts are unresolved this is refused, unless `force`: the
    /// unresolved ones are then written as markers (the result itself keeps their lines).
    pub fn save(&mut self, force: bool) -> Result<(), MergeError> {
        let unresolved = self.unresolved_count();
        if unresolved > 0 && !force {
            return Err(MergeError::Unresolved(unresolved));
        }
        if !self.chunks.iter().any(|chunk| chunk.is_conflict() && chunk.resolution == Resolution::Unresolved) {
            return Ok(self.result.save()?);
        }
        let marked = Arc::new(self.marked_lines());
        let kept = std::mem::replace(&mut self.result.lines, marked);
        let saved = self.result.save();
        self.result.lines = kept;
        Ok(saved?)
    }
}
