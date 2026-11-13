//! Side-by-side text comparison. Everything is painted by hand so that only visible rows cost
//! anything, whatever the size of the files.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use biomerge_core::Side;
use biomerge_core::diff::{BlockKind, ChangeSpan, DiffOptions, DiffResult, LineSpans, Row, SpanKind, diff_lines_with};
use biomerge_core::ignore_rules::IgnoreRules;
use biomerge_core::document::{Document, DocumentError, Edit, Line};
use biomerge_core::extract::{self, DocFacts, DocFormat, ExtractOptions};
use biomerge_core::json::{self, Format, JsonDoc, JsonOptions, PathMap, Summary};
use biomerge_core::merge3::has_conflict_markers;
use biomerge_core::filter::{Filter, FilterOptions};
use biomerge_core::highlight::{self, Checkpoints, LineStyles, StyleRun, SyntaxTheme};
use biomerge_core::textedit::{self, TextPos};
use biomerge_core::wrap;
use egui::epaint::CubicBezierShape;
use egui::epaint::text::ByteIndex;
use egui::text::{LayoutJob, LayoutSection, TextFormat};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Event, FontFamily, FontId, ImeEvent, Key, Modifiers,
    Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use rayon::prelude::*;

use crate::icons::{self, Icon};
use crate::scroll::{Position, Scroller};
use crate::sync::SyncMap;
use crate::formats;
use crate::panes::{self, CODE_PADDING};
use crate::theme::{Chrome, DEFAULT_CODE_SIZE, DocPalette, LinkStyle, Look, Tint, UiPalette, line_height, mix};
use crate::widgets::{self, Badge, PathAction, PathBar};

pub(crate) const GUTTER_PADDING: f32 = 12.0;
pub(crate) const TEXT_PADDING: f32 = 12.0;
/// Reserved on the right of a pane for the action buttons of a merge's blocks and the wrap marks.
pub(crate) const ARROW_MARGIN: f32 = 58.0;
/// Reserved on the right of a pane for the wrap marks alone: the arrows stand in the gaps.
pub(crate) const WRAP_MARGIN: f32 = 26.0;
pub(crate) const ARROW_SIZE: egui::Vec2 = vec2(24.0, 18.0);
pub(crate) const ARROW_ICON: f32 = 13.0;
pub(crate) const WRAP_ICON: f32 = 12.0;
const SWAP_BUTTON: f32 = 28.0;
pub(crate) const CARET_WIDTH: f32 = 1.6;
pub(crate) const CARET_BLINK_SECONDS: f64 = 0.53;
/// Rows kept between the caret and the edges of the view.
pub(crate) const CARET_MARGIN_ROWS: f64 = 2.0;
/// Keystrokes closer than this are undone together.
pub(crate) const UNDO_MERGE_SECONDS: f64 = 1.0;
/// How far the horizontal ends of a link reach towards each other, as a share of the gap.
pub(crate) const LINK_CURVE: f32 = 0.55;
/// Up to this many lines, the JSON paths of a pane are scanned at once after an edit; beyond,
/// on a worker, the status bar keeping the previous paths meanwhile.
const PATH_SCAN_INLINE_LINES: usize = 50_000;
const SUMMARY_ROW: f32 = 30.0;

enum Message {
    Loaded {
        docs: Box<[Document; 2]>,
        diff: DiffResult,
        syntax: [Option<String>; 2],
        documents: [Option<DocumentSide>; 2],
        /// The texts as read, before any projection.
        originals: [Arc<Vec<Line>>; 2],
        /// The JSON projection made while loading (its edits and which sides it altered), or
        /// why each side could not be parsed.
        projection: Option<Result<ProjectionSteps, [Option<String>; 2]>>,
    },
    /// A projection asked for after loading, from the text the panes held at `revisions`.
    Projected {
        job: u64,
        revisions: [u64; 2],
        outcome: Result<Projected, [Option<String>; 2]>,
    },
    Paths {
        side: Side,
        revision: u64,
        map: PathMap,
    },
    Summary {
        job: u64,
        outcome: Result<Summary, String>,
    },
    Failed(String),
    Picked(usize, PathBuf),
    Styles {
        generation: u64,
        side: Side,
        start: usize,
        lines: Vec<LineStyles>,
    },
}

/// How the comparison is shown and computed: the settings every text tab shares.
#[derive(Clone)]
pub struct TextSettings {
    pub options: DiffOptions,
    /// Compiled once by the application, whenever the rules change.
    pub rules: IgnoreRules,
    pub wrap_lines: bool,
    pub aligned: bool,
    pub context: Option<u32>,
    pub syntax_theme: SyntaxTheme,
    /// What a JSON tab normalises, and whether it opens compared structurally.
    pub json: JsonOptions,
    pub json_structural: bool,
    /// How word-processing documents are turned into text.
    pub extract: ExtractOptions,
}

/// What one side holds besides its path. The default is a file read from disk and editable.
#[derive(Clone, Debug, Default)]
pub struct SideSource {
    /// Shown instead of the file's name and folder: the path may be a temporary file, or the
    /// side a version that has no file of its own.
    pub label: Option<String>,
    /// Text already read elsewhere (a Git revision, an extracted document), shown instead of
    /// reading the path. The path then only names the side, for its syntax and its header.
    pub contents: Option<Document>,
    /// No typing, no transfer into this side, no saving. Copying from it stays possible.
    pub read_only: bool,
    /// A word-processing document whose text is extracted with these options, on the loader's
    /// worker and again on F5. Such a side is always read-only.
    pub extract: Option<ExtractOptions>,
}

/// What a side extracted from a document knows about it, for its badge.
#[derive(Clone, Copy, Debug)]
pub struct DocumentSide {
    pub format: DocFormat,
    pub facts: DocFacts,
}

/// Both panes after a projection: the new documents, the edit that turned each old text into
/// the new one (`None` when nothing changed) and the comparison of the new texts.
struct Projected {
    docs: Box<[Document; 2]>,
    edits: [Option<Edit>; 2],
    /// Saving that side would not give back its file as it was.
    alters: [bool; 2],
    diff: DiffResult,
}

type ProjectionSteps = ([Option<Edit>; 2], [bool; 2]);

/// The JSON side of a tab whose files are JSON.
struct JsonState {
    format: Format,
    options: JsonOptions,
    /// The user wants the structural comparison (the default, or the toggle).
    wanted: bool,
    /// It is in force: projected texts, JSON diff, comma repair after a transfer.
    structural: bool,
    /// Why a side could not be compared structurally, at the last attempt.
    errors: [Option<String>; 2],
    /// The paths of each pane's lines, for the revision they were scanned at.
    paths: [Option<(u64, PathMap)>; 2],
    /// A worker is scanning that side's paths.
    scanning: [bool; 2],
    /// Saving a reformatted side was confirmed once: the question is not asked again.
    save_confirmed: [bool; 2],
    /// The projection running on a worker, by number.
    pending: Option<u64>,
    next_job: u64,
}

/// The "Changements JSON" dialog.
struct SummaryDialog {
    job: u64,
    outcome: Option<Result<Summary, String>>,
}

/// A question the tab asks before going on.
enum Question {
    /// Re-projecting rewrites panes that hold edits of the user.
    Reproject { options: JsonOptions },
    /// Saving this side writes reformatted JSON, or text that is not JSON.
    Save {
        side: Side,
        reformatted: bool,
        invalid: Option<String>,
        /// Then the other modified side, as Ctrl+S saves both.
        all: bool,
    },
}

/// What a tab shows in the status bar.
pub enum StatusItem {
    Text(String),
    /// A JSON path: a click copies it.
    Path(String),
    Pill(String),
    Counts {
        deletions: usize,
        insertions: usize,
        changes: usize,
    },
    /// A merge's conflicts: those left unresolved, in the conflict colour, out of all of them.
    Conflicts {
        unresolved: usize,
        total: usize,
    },
}

struct Step {
    side: Side,
    edit: Edit,
    /// Steps sharing a group other than 0 are undone and redone together.
    group: u64,
    /// A projection that made this side differ from its file in layout or order.
    alters: bool,
    revision_before: u64,
    revision_after: u64,
    caret_before: Option<(Side, TextPos)>,
    caret_after: Option<(Side, TextPos)>,
    typed_at: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    side: Side,
    anchor: TextPos,
    head: TextPos,
}

impl Selection {
    fn ordered(&self) -> (TextPos, TextPos) {
        let key = |p: &TextPos| (p.line, p.byte);
        if key(&self.anchor) <= key(&self.head) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn bytes_on(&self, side: Side, line: u32, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if side != self.side || line < start.line || line > end.line || start == end {
            return None;
        }
        let from = if line == start.line { start.byte } else { 0 };
        let to = if line == end.line { end.byte } else { len };
        Some(from..to)
    }
}

/// What the field of the band does with what is typed in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    /// Every occurrence in the two texts is highlighted and can be stepped through.
    Find,
    /// Only the blocks holding the words stay, everything else folds.
    Filter,
}

/// One occurrence of the text searched for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Match {
    row: u32,
    side: Side,
    line: u32,
    start: u32,
    end: u32,
    /// An unchanged row holds it on both sides: it counts once, kept on the left one.
    shared: bool,
}

impl Match {
    /// The order of reading: down the rows, the left side first.
    fn key(&self) -> (u32, usize, u32) {
        (self.row, self.side.index(), self.start)
    }
}

/// What the filter and the context do to a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fold {
    Shown,
    /// First of a hidden run: it stands for the `n` rows of that run.
    Separator(u32),
    Hidden,
}

#[derive(Default)]
struct RowLayout {
    columns: usize,
    wrap: bool,
    /// For each side, the first visual line of each line; one more entry holds the total.
    offsets: [Vec<u32>; 2],
    /// For each side, the row of the comparison each line belongs to.
    line_rows: [Vec<u32>; 2],
    folds: [Vec<Fold>; 2],
    /// For each row, whether it shows. Empty when nothing folds.
    shown_rows: Vec<bool>,
    sync: SyncMap,
    /// Facing lines stay level, a gap standing for what the other side lacks.
    aligned: bool,
    /// When aligned: the first visual line of each row, with one more entry holding the total.
    row_offsets: Vec<u32>,
    /// Widest row in columns, which bounds the horizontal scroll without wrapping.
    max_columns: usize,
    line_height: f32,
    valid: bool,
}

#[derive(Clone, Copy)]
struct Pane {
    rect: Rect,
    text_x: f32,
    text_right: f32,
}

#[derive(Clone, Copy)]
struct Geometry {
    view: Rect,
    /// Where rows may paint: the view and the padding around it.
    rows: Rect,
    panes: [Pane; 2],
    /// The gap between the panes, from left to right.
    gap: (f32, f32),
    overview: Rect,
    line_height: f32,
    char_width: f32,
}

pub struct TextTab {
    paths: [PathBuf; 2],
    inputs: [String; 2],
    allow_missing: bool,
    sources: [SideSource; 2],
    /// A folder operation replaced or removed the file under unsaved edits: saving that side
    /// would silently undo it.
    changed_on_disk: [bool; 2],
    docs: Option<Box<[Document; 2]>>,
    diff: DiffResult,
    styles: [Vec<Option<LineStyles>>; 2],
    syntax: [Option<String>; 2],
    error: Option<String>,
    pub message: Option<String>,
    loading: bool,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    cancel: [Arc<AtomicBool>; 2],
    generation: [u64; 2],
    checkpoints: [Arc<Checkpoints>; 2],
    options: DiffOptions,
    rules: IgnoreRules,
    /// The options or the rules changed while the files were being read: the comparison the
    /// loader made is out of date.
    stale_diff: bool,
    syntax_theme: SyntaxTheme,
    layout: RowLayout,
    /// The shared position, in pixels of the virtual document (`sync.rs`).
    scroll: f64,
    /// Where it puts each pane, in pixels.
    pane_scroll: [f64; 2],
    /// A place to come back to once the layout is rebuilt.
    pending_place: Option<(usize, f64)>,
    scroll_x: f32,
    scroller: Scroller,
    code_size: f32,
    pub filter_query: String,
    pub filter_options: FilterOptions,
    search_mode: SearchMode,
    /// What the query compiles to, in either mode: it is what gets highlighted.
    filter: Option<Filter>,
    filter_error: Option<String>,
    /// For each block, whether the filter keeps it. Empty without a filter.
    kept: Vec<bool>,
    /// When finding: every occurrence, in reading order.
    matches: Vec<Match>,
    /// Unchanged text folds, but for this many lines around each block.
    context: Option<u32>,
    /// Rows unfolded by a click on their separator.
    unfolded: Vec<Range<u32>>,
    /// In the last frame: the side and line of the separator under the pointer.
    hovered_separator: Option<(usize, usize)>,
    current_block: Option<usize>,
    caret: Option<(Side, TextPos)>,
    selection: Option<Selection>,
    /// The column up and down aim for across shorter lines.
    goal_column: Option<usize>,
    /// After an edit: this line must stay at this height on screen (`side`, `line`, pixels).
    anchor: Option<(usize, u32, f64)>,
    caret_to_reveal: bool,
    /// The caret was sent far away: out of view, it comes to the middle rather than the edge.
    center_caret: bool,
    undo: Vec<Step>,
    redo: Vec<Step>,
    next_revision: u64,
    revision: [u64; 2],
    /// The revision whose text is the one on disk.
    saved_revision: [u64; 2],
    /// Revisions holding nothing the user would lose: the one on disk, and a projection made
    /// from it. A side is dirty at any other.
    clean: [Vec<u64>; 2],
    next_group: u64,
    json: Option<JsonState>,
    /// Each side's text as it is on disk. A side without edits is projected again from it, so
    /// that a new option sees the file's own order rather than the previous projection's.
    originals: [Option<Arc<Vec<Line>>>; 2],
    /// The JSON options and mode a file chosen later in this tab starts with.
    json_defaults: (JsonOptions, bool),
    /// Sides extracted from word-processing documents.
    documents: [Option<DocumentSide>; 2],
    extract_options: ExtractOptions,
    /// Document tabs wrap by themselves, one paragraph being one line, whatever the setting.
    own_wrap: bool,
    question: Option<Question>,
    summary: Option<SummaryDialog>,
    geometry: Option<Geometry>,
    jump_to_first: bool,
    /// After a transfer: the block the arrows now act on must be in view.
    reveal_current: bool,
    /// The arrows painted in the last frame: their block, where they send, where they stand.
    arrow_rects: Vec<(usize, Side, Rect)>,
    /// Sides whose file, as read, holds git's conflict markers.
    conflicted: [bool; 2],
}

impl TextTab {
    pub fn new(
        ctx: &egui::Context,
        paths: [PathBuf; 2],
        allow_missing: bool,
        settings: TextSettings,
        sources: [SideSource; 2],
    ) -> Self {
        let TextSettings { options, rules, wrap_lines, aligned, context, syntax_theme, json, json_structural, extract } = settings;
        let (sender, receiver) = channel();
        let own_wrap = sources.iter().any(|source| source.extract.is_some());
        let json_defaults = (json, json_structural);
        let json = json_format(&paths, &sources)
            .map(|format| JsonState::new(format, json_defaults.0.clone(), json_defaults.1));
        let mut tab = Self {
            inputs: paths.clone().map(|p| p.display().to_string()),
            paths,
            allow_missing,
            sources,
            changed_on_disk: [false, false],
            docs: None,
            diff: DiffResult::default(),
            styles: [Vec::new(), Vec::new()],
            syntax: [None, None],
            error: None,
            message: None,
            loading: false,
            sender,
            receiver,
            cancel: [Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false))],
            generation: [0, 0],
            checkpoints: [Arc::default(), Arc::default()],
            options,
            rules,
            stale_diff: false,
            syntax_theme,
            layout: RowLayout {
                wrap: wrap_lines || own_wrap,
                aligned,
                line_height: line_height(DEFAULT_CODE_SIZE),
                ..RowLayout::default()
            },
            scroll: 0.0,
            pane_scroll: [0.0, 0.0],
            pending_place: None,
            scroll_x: 0.0,
            scroller: Scroller::default(),
            code_size: DEFAULT_CODE_SIZE,
            filter_query: String::new(),
            filter_options: FilterOptions::default(),
            search_mode: SearchMode::Find,
            filter: None,
            filter_error: None,
            kept: Vec::new(),
            matches: Vec::new(),
            context,
            unfolded: Vec::new(),
            hovered_separator: None,
            current_block: None,
            caret: None,
            selection: None,
            goal_column: None,
            anchor: None,
            caret_to_reveal: false,
            center_caret: false,
            undo: Vec::new(),
            redo: Vec::new(),
            next_revision: 1,
            revision: [0, 0],
            saved_revision: [0, 0],
            clean: [vec![0], vec![0]],
            next_group: 1,
            originals: [None, None],
            json,
            json_defaults,
            documents: [None, None],
            extract_options: extract,
            own_wrap,
            question: None,
            summary: None,
            geometry: None,
            jump_to_first: true,
            reveal_current: false,
            arrow_rects: Vec::new(),
            conflicted: [false, false],
        };
        tab.reload(ctx);
        tab
    }

    pub fn title(&self) -> String {
        let name = |side: usize| match &self.sources[side].label {
            Some(label) => label.clone(),
            None => {
                let path = &self.paths[side];
                path.file_name()
                    .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
            }
        };
        let (left, right) = (name(0), name(1));
        if left == right {
            left
        } else {
            format!("{left}, {right}")
        }
    }

    /// The file of a side that git left with conflict markers, which a three-way merge can
    /// resolve. The side being edited first.
    pub fn conflicted_path(&self) -> Option<&Path> {
        let preferred = self.caret_side().into_iter().chain([Side::Left, Side::Right]);
        preferred.filter(|side| self.conflicted[side.index()]).map(|side| self.paths[side.index()].as_path()).next()
    }

    /// True when this side refuses edits, transfers into it and saving.
    pub fn is_read_only(&self, side: Side) -> bool {
        let source = &self.sources[side.index()];
        source.read_only || source.extract.is_some()
    }

    // Part of the read-only API, for sides that show extracted documents.
    #[allow(dead_code)]
    pub fn set_read_only(&mut self, side: Side, read_only: bool) {
        self.sources[side.index()].read_only = read_only;
    }

    /// The name shown for a side instead of its file name; `None` goes back to the file name.
    #[allow(dead_code)]
    pub fn set_label(&mut self, side: Side, label: Option<String>) {
        self.sources[side.index()].label = label;
    }

    /// Paths read from disk, by side: what a folder operation can change under the tab.
    pub fn disk_paths(&self) -> [Option<&Path>; 2] {
        [0, 1].map(|side| self.sources[side].contents.is_none().then_some(self.paths[side].as_path()))
    }

    /// Someone else wrote or removed the file of `side`. Without unsaved edits the tab simply
    /// reads both files again; with some, the side is marked so that saving cannot erase what
    /// happened on disk.
    pub fn changed_on_disk(&mut self, ctx: &egui::Context, side: Side) {
        if !self.is_dirty() {
            self.reload(ctx);
        } else {
            self.changed_on_disk[side.index()] = true;
            self.message = Some(format!(
                "{} a changé sur le disque : F5 le relit (tes modifications seront perdues)",
                self.side_name(side)
            ));
        }
    }

    fn side_name(&self, side: Side) -> String {
        let index = side.index();
        self.sources[index].label.clone().unwrap_or_else(|| {
            self.paths[index].file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        })
    }

    fn refuse_read_only(&mut self, side: Side) {
        self.message = Some(if self.sources[side.index()].extract.is_some() {
            format!("{} est comparé par son texte, en lecture seule", self.side_name(side))
        } else {
            format!("{} est en lecture seule", self.side_name(side))
        });
    }

    pub fn filter_error(&self) -> Option<&str> {
        self.filter_error.as_deref()
    }

    /// `(kept, total)` while a filter is in force.
    pub fn filter_counts(&self) -> Option<(usize, usize)> {
        self.filter
            .as_ref()
            .filter(|_| self.search_mode == SearchMode::Filter)
            .map(|_| (self.kept.iter().filter(|&&kept| kept).count(), self.kept.len()))
    }

    /// While finding: the rank of the occurrence the selection sits on, and how many there are.
    pub fn find_counts(&self) -> Option<(Option<usize>, usize)> {
        self.filter
            .as_ref()
            .filter(|_| self.search_mode == SearchMode::Find)
            .map(|_| (self.current_match().map(|index| index + 1), self.matches.len()))
    }

    pub fn set_search_mode(&mut self, mode: SearchMode) {
        if self.search_mode != mode {
            self.search_mode = mode;
            self.apply_search();
        }
    }

    fn block_kept(&self, block: usize) -> bool {
        self.kept.get(block).copied().unwrap_or(true)
    }

    /// Reads the query as a block filter. The interface goes through `set_search_mode`.
    #[cfg(test)]
    pub fn apply_filter(&mut self) {
        self.search_mode = SearchMode::Filter;
        self.apply_search();
    }

    /// Reads the query and its options again, for what the mode does with them.
    pub fn apply_search(&mut self) {
        let finding = self.search_mode == SearchMode::Find;
        let built = if finding {
            Filter::phrase(&self.filter_query, self.filter_options)
        } else {
            Filter::new(&self.filter_query, self.filter_options)
        };
        match built {
            Ok(filter) => {
                self.filter = filter;
                self.filter_error = None;
            }
            Err(error) => {
                self.filter = None;
                self.filter_error = Some(error);
            }
        }
        let was_filtering = !self.kept.is_empty();
        self.compute_kept();
        self.compute_matches();
        if !finding {
            self.unfolded.clear();
            self.layout.valid = false;
            self.scroll = 0.0;
            self.current_block = None;
            self.jump_to_first = true;
            return;
        }
        if was_filtering {
            // The blocks the filter hid come back around the one in use.
            self.unfolded.clear();
            self.layout.valid = false;
            self.reveal_current = true;
        }
        // Found as it is typed: the first occurrence from where the reader is.
        if !self.matches.is_empty() {
            let key = self.reading_key();
            let next = self.matches.partition_point(|found| found.key() < key);
            self.show_match(next % self.matches.len());
        }
    }

    fn compute_kept(&mut self) {
        self.kept.clear();
        let (SearchMode::Filter, Some(filter), Some(docs)) = (self.search_mode, &self.filter, &self.docs) else {
            return;
        };
        self.kept = self
            .diff
            .blocks
            .par_iter()
            .map(|block| {
                let text = |side: usize, range: &Range<u32>| {
                    docs[side].lines[range.start as usize..range.end as usize]
                        .iter()
                        .map(|line| line.text.as_str())
                };
                filter.matches_lines(text(0, &block.left).chain(text(1, &block.right)))
            })
            .collect();
    }

    fn compute_matches(&mut self) {
        self.matches.clear();
        let (SearchMode::Find, Some(filter), Some(docs)) = (self.search_mode, &self.filter, &self.docs) else {
            return;
        };
        self.matches = self
            .diff
            .rows
            .par_iter()
            .enumerate()
            .flat_map_iter(|(index, row)| {
                let shared = shared_row(row, docs);
                let sides = [(Side::Left, row.left), (Side::Right, row.right)];
                let count = if shared { 1 } else { 2 };
                sides.into_iter().take(count).flat_map(move |(side, line)| {
                    let text = line.and_then(|line| docs[side.index()].lines.get(line as usize));
                    let ranges = text.map_or_else(Vec::new, |line| filter.find_ranges(&line.text));
                    ranges.into_iter().map(move |range| Match {
                        row: index as u32,
                        side,
                        line: line.unwrap_or(0),
                        start: range.start as u32,
                        end: range.end as u32,
                        shared,
                    })
                })
            })
            .collect();
    }

    /// Where a place of the text falls among the occurrences.
    fn match_key(&self, side: Side, pos: TextPos) -> Option<(u32, usize, u32)> {
        let row = *self.layout.line_rows[side.index()].get(pos.line as usize)?;
        let shared = shared_row(self.diff.rows.get(row as usize)?, self.docs.as_ref()?);
        Some((row, if shared { 0 } else { side.index() }, pos.byte as u32))
    }

    /// Where the reader is: the start of the selection, or else the top of the view.
    fn reading_key(&self) -> (u32, usize, u32) {
        let from_caret = self.caret.and_then(|(side, pos)| {
            let start = self.selection.filter(|s| s.side == side).map_or(pos, |s| s.ordered().0);
            self.match_key(side, start)
        });
        from_caret.unwrap_or_else(|| {
            let visual = (self.pane_scroll[0] / self.layout.line_height as f64) as u32;
            let line = line_at(&self.layout.offsets[0], visual);
            (self.layout.line_rows[0].get(line).copied().unwrap_or(0), 0, 0)
        })
    }

    /// The occurrence the selection sits on, when it does.
    fn current_match(&self) -> Option<usize> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();
        let key = self.match_key(selection.side, start)?;
        let index = self.matches.binary_search_by_key(&key, Match::key).ok()?;
        (start.line == end.line && self.matches[index].end as usize == end.byte).then_some(index)
    }

    /// To the next or the previous occurrence, around the ends of the text. Under a filter the
    /// results are blocks: the same keys step through them.
    pub fn find_step(&mut self, forward: bool) {
        if self.search_mode == SearchMode::Filter {
            if self.filter.is_some() {
                self.go_to_block(forward);
            }
            return;
        }
        let count = self.matches.len();
        if count == 0 {
            return;
        }
        let step = |from: usize| if forward { (from + 1) % count } else { (from + count - 1) % count };
        let target = match self.current_match() {
            Some(current) => step(current),
            None => {
                let key = self.reading_key();
                let next = self.matches.partition_point(|found| found.key() < key);
                if forward { next % count } else { step(next % count) }
            }
        };
        self.show_match(target);
    }

    /// Selects an occurrence, which also brings it in view on its side.
    fn show_match(&mut self, index: usize) {
        let found = self.matches[index];
        // What both sides hold is shown on the one in use.
        let other = self.diff.rows.get(found.row as usize).and_then(|row| row.right);
        let (side, line) = match (self.caret, other) {
            (Some((Side::Right, _)), Some(line)) if found.shared => (Side::Right, line),
            _ => (found.side, found.line),
        };
        self.jump_to(side, TextPos::new(line, found.start as usize), TextPos::new(line, found.end as usize));
    }

    /// Sends the caret to a place asked for by name, selecting from `anchor` to `head`.
    fn jump_to(&mut self, side: Side, anchor: TextPos, head: TextPos) {
        self.goal_column = None;
        self.set_caret(side, anchor, false);
        self.set_caret(side, head, true);
        self.center_caret = true;
        self.adopt_block_at(side, head.line);
    }

    /// Puts the caret at the start of a line counted from 1, on the side asked for or else the
    /// one in use. A number past the last line means the last line.
    pub fn go_to_line(&mut self, side: Option<Side>, number: usize) {
        let Some(docs) = &self.docs else {
            return;
        };
        let side = side.or(self.caret_side()).unwrap_or(Side::Left);
        let last = docs[side.index()].len().saturating_sub(1);
        let pos = TextPos::new(number.saturating_sub(1).min(last) as u32, 0);
        self.jump_to(side, pos, pos);
    }

    /// How many lines each side has, once loaded.
    pub fn line_counts(&self) -> Option<[usize; 2]> {
        self.docs.as_ref().map(|docs| [docs[0].len().max(1), docs[1].len().max(1)])
    }

    pub fn caret_side(&self) -> Option<Side> {
        self.caret.map(|(side, _)| side)
    }

    /// `Some(n)`: unchanged text folds, but for `n` lines around each block.
    pub fn set_context(&mut self, context: Option<u32>) {
        if self.context != context {
            self.context = context;
            self.unfolded.clear();
            self.reveal_current |= self.current_block.is_some_and(|block| self.block_in_view(block));
            self.caret_to_reveal |= self.caret.is_some();
            self.relayout_in_place();
        }
    }

    /// The folds have to be worked out again, around the place being read.
    fn relayout_in_place(&mut self) {
        if self.layout.valid {
            let place = self.layout.sync.locate(self.scroll / self.layout.line_height as f64);
            self.pending_place.get_or_insert(place);
            self.layout.valid = false;
        }
    }

    /// The block of a line becomes the one the arrows act on.
    fn adopt_block_at(&mut self, side: Side, line: u32) {
        let row = self.layout.line_rows[side.index()].get(line as usize);
        let block = row.and_then(|&row| self.diff.rows.get(row as usize)).and_then(|row| row.block);
        if let Some(block) = block {
            self.current_block = Some(block as usize);
        }
    }

    /// The unfolded rows as lines of one side, which a new comparison does not renumber as long
    /// as the edit is on the other one.
    fn unfolded_lines(&self, side: Side) -> Vec<Range<u32>> {
        let line = |row: &Row| if side == Side::Left { row.left } else { row.right };
        let lines = |range: &Range<u32>| {
            let rows = self.diff.rows.get(range.start as usize..range.end as usize)?;
            Some(rows.iter().find_map(line)?..rows.iter().rev().find_map(line)? + 1)
        };
        self.unfolded.iter().filter_map(lines).collect()
    }

    fn restore_unfolded(&mut self, side: Side, lines: Vec<Range<u32>>) {
        let line = |row: &Row| if side == Side::Left { row.left } else { row.right };
        let rows = &self.diff.rows;
        self.unfolded = lines
            .into_iter()
            .filter_map(|range| {
                let start = rows.iter().position(|row| line(row).is_some_and(|l| l >= range.start))?;
                let end = rows.iter().rposition(|row| line(row).is_some_and(|l| l < range.end))?;
                (start <= end).then(|| start as u32..end as u32 + 1)
            })
            .collect();
    }

    pub fn has_caret(&self) -> bool {
        self.caret.is_some()
    }

    pub fn is_ready(&self) -> bool {
        self.docs.is_some() && !self.loading && self.json.as_ref().is_none_or(|json| json.pending.is_none())
    }

    pub fn is_dirty(&self) -> bool {
        self.side_dirty(Side::Left) || self.side_dirty(Side::Right)
    }

    fn side_dirty(&self, side: Side) -> bool {
        !self.clean[side.index()].contains(&self.revision[side.index()])
    }

    /// The text of this side is a projection its file was not written as, and was not saved
    /// since: saving writes the new layout or order.
    fn altered(&self, side: Side) -> bool {
        let saved = self.saved_revision[side.index()];
        self.undo.iter().any(|step| step.side == side && step.alters && step.revision_after > saved)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn has_blocks(&self) -> bool {
        !self.diff.blocks.is_empty()
    }

    /// Puts a block in the middle of the view, as the F8 key does.
    #[cfg(test)]
    pub fn show_block(&mut self, block: usize) {
        self.select_block(block);
    }

    #[cfg(test)]
    pub fn pane_positions(&self) -> [f64; 2] {
        self.pane_scroll
    }

    #[cfg(test)]
    pub fn put_caret(&mut self, side: Side, line: u32, byte: usize) {
        self.set_caret(side, TextPos::new(line, byte), false);
    }

    #[cfg(test)]
    pub fn line_text(&self, side: Side, line: usize) -> String {
        self.docs.as_ref().unwrap()[side.index()].lines[line].text.clone()
    }

    #[cfg(test)]
    pub fn caret_position(&self) -> Option<(u32, usize)> {
        self.caret.map(|(_, pos)| (pos.line, pos.byte))
    }

    #[cfg(test)]
    pub fn line_count(&self, side: Side) -> usize {
        self.docs.as_ref().unwrap()[side.index()].len()
    }

    #[cfg(test)]
    pub fn hidden_lines(&self, side: Side) -> usize {
        self.layout.folds[side.index()].iter().filter(|&&fold| fold != Fold::Shown).count()
    }

    #[cfg(test)]
    pub fn line_is_shown(&self, side: Side, line: usize) -> bool {
        self.layout.folds[side.index()][line] == Fold::Shown
    }

    /// Screen position of the first separator of a side in the last painted frame.
    #[cfg(test)]
    pub fn first_separator(&self, side: Side) -> Option<Pos2> {
        let geometry = self.geometry?;
        let index = side.index();
        let line = self.layout.folds[index].iter().position(|fold| matches!(fold, Fold::Separator(_)))?;
        let top = self.layout.offsets[index][line] as f64 * geometry.line_height as f64 - self.pane_scroll[index];
        Some(pos2(
            geometry.panes[index].rect.center().x,
            geometry.view.top() + top as f32 + geometry.line_height / 2.0,
        ))
    }

    /// True when the caret is on a line the last frame painted inside the view.
    #[cfg(test)]
    pub fn caret_in_view(&self) -> bool {
        let Some((index, visual, _)) = self.caret_place() else {
            return false;
        };
        let line = self.layout.line_height as f64;
        let top = visual as f64 * line - self.pane_scroll[index];
        top >= 0.0 && top + line <= self.view_lines() * line + 0.5
    }

    #[cfg(test)]
    pub fn selection_text(&self) -> Option<(Side, String)> {
        Some((self.selection?.side, self.selected_text()?))
    }

    #[cfg(test)]
    pub fn selected_block(&self) -> Option<usize> {
        self.current_block
    }

    #[cfg(test)]
    pub fn scroll_position(&self) -> f64 {
        self.scroll
    }

    #[cfg(test)]
    pub fn block_count(&self) -> usize {
        self.diff.blocks.len()
    }

    #[cfg(test)]
    pub fn syntax_theme(&self) -> SyntaxTheme {
        self.syntax_theme
    }

    #[cfg(test)]
    pub fn block_start(&self, block: usize, side: Side) -> u32 {
        let block = &self.diff.blocks[block];
        if side == Side::Left { block.left.start } else { block.right.start }
    }

    /// Screen position of the middle of a line's first visual line, in the last frame.
    #[cfg(test)]
    pub fn line_center(&self, side: Side, line: u32) -> Option<Pos2> {
        let geometry = self.geometry?;
        let index = side.index();
        let visual = *self.layout.offsets[index].get(line as usize)? as f64;
        let y = geometry.view.top() as f64 + visual * geometry.line_height as f64 - self.pane_scroll[index];
        Some(pos2(geometry.panes[index].text_x + 40.0, y as f32 + geometry.line_height / 2.0))
    }

    /// Screen position of a block's arrow in the last painted frame.
    #[cfg(test)]
    pub fn arrow_center(&self, block: usize, target: Side) -> Option<Pos2> {
        self.arrow_rects.iter().find(|(b, t, _)| *b == block && *t == target).map(|(_, _, rect)| rect.center())
    }

    #[cfg(test)]
    pub fn badge_texts(&self, side: Side) -> Vec<String> {
        self.badges(side).into_iter().map(|badge| badge.text).collect()
    }

    /// The "Changements JSON" dialog has its list, or its error.
    #[cfg(test)]
    pub fn summary_ready(&self) -> bool {
        self.summary.as_ref().is_some_and(|summary| summary.outcome.is_some())
    }

    #[cfg(test)]
    pub fn summary_paths(&self) -> Vec<String> {
        match self.summary.as_ref().and_then(|summary| summary.outcome.as_ref()) {
            Some(Ok(found)) => found.changes.iter().map(|change| change.path().to_owned()).collect(),
            _ => Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn has_question(&self) -> bool {
        self.question.is_some()
    }

    /// True once every highlightable line has received its colours.
    #[cfg(test)]
    pub fn highlight_finished(&self) -> bool {
        [Side::Left, Side::Right].iter().all(|side| {
            self.syntax[side.index()].is_none()
                || self.styles[side.index()].iter().all(Option::is_some)
        })
    }

    fn cancel_highlighting(&mut self, side: Side) {
        let index = side.index();
        self.cancel[index].store(true, Ordering::Relaxed);
        self.cancel[index] = Arc::new(AtomicBool::new(false));
        self.generation[index] += 1;
    }

    pub fn reload(&mut self, ctx: &egui::Context) {
        self.cancel_highlighting(Side::Left);
        self.cancel_highlighting(Side::Right);
        self.loading = true;
        self.error = None;
        self.docs = None;
        self.undo.clear();
        self.redo.clear();
        self.revision = [0, 0];
        self.saved_revision = [0, 0];
        self.clean = [vec![0], vec![0]];
        self.selection = None;
        self.caret = None;
        self.geometry = None;
        self.jump_to_first = true;
        self.stale_diff = false;
        self.changed_on_disk = [false, false];
        self.question = None;
        self.summary = None;
        self.documents = [None, None];
        if let Some(json) = &mut self.json {
            json.reset();
        }
        // What the projection needs, when the tab opens compared structurally.
        let projection = self
            .json
            .as_ref()
            .filter(|json| json.structural)
            .map(|json| (json.format, json.options.clone()));

        let paths = self.paths.clone();
        let sender = self.sender.clone();
        let options = self.options;
        let rules = self.rules.clone();
        let allow_missing = self.allow_missing;
        let sources = self.sources.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            type Loaded = (Document, Option<DocumentSide>);
            let load = |path: &Path, source: &SideSource| -> Result<Loaded, String> {
                if let Some(options) = &source.extract {
                    let extracted = extract::extract(path, options).map_err(|e| e.to_string())?;
                    let mut doc = Document::from_text(&extracted.text);
                    doc.path = Some(path.to_owned());
                    Ok((doc, Some(DocumentSide { format: extracted.format, facts: extracted.facts })))
                } else if let Some(doc) = &source.contents {
                    let mut doc = doc.clone();
                    doc.path = Some(path.to_owned());
                    Ok((doc, None))
                } else if allow_missing && !path.exists() {
                    let mut doc = Document::from_text("");
                    doc.path = Some(path.to_owned());
                    Ok((doc, None))
                } else {
                    Document::load(path).map(|doc| (doc, None)).map_err(|e: DocumentError| e.to_string())
                }
            };
            let (left, right) = rayon::join(|| load(&paths[0], &sources[0]), || load(&paths[1], &sources[1]));
            let message = match (left, right) {
                (Ok((left, left_document)), Ok((right, right_document))) => {
                    let documents = [left_document, right_document];
                    let syntax = [(&left, &documents[0]), (&right, &documents[1])].map(|(doc, document)| {
                        let first = doc.lines.first().map_or("", |l| l.text.as_str());
                        // Extracted text is Markdown-like, whatever the file's extension says.
                        let path = match document {
                            Some(_) => Some(Path::new(extract::SYNTAX_HINT)),
                            None => doc.path.as_deref(),
                        };
                        highlight::detect_syntax(path, first)
                    });
                    let originals = [left.lines.clone(), right.lines.clone()];
                    let projected = projection.map(|(format, json_options)| {
                        project_documents([left.clone(), right.clone()], [None, None], format, &json_options, options, &rules)
                    });
                    let (docs, diff, projection) = match projected {
                        Some(Ok(Projected { docs, edits, alters, diff })) => (docs, diff, Some(Ok((edits, alters)))),
                        failed => {
                            let diff = compute_diff(&left, &right, options, &rules, false);
                            let errors = match failed {
                                Some(Err(errors)) => Some(Err(errors)),
                                _ => None,
                            };
                            (Box::new([left, right]), diff, errors)
                        }
                    };
                    Message::Loaded {
                        docs,
                        diff,
                        syntax,
                        documents,
                        originals,
                        projection,
                    }
                }
                (Err(error), _) | (_, Err(error)) => Message::Failed(error),
            };
            let _ = sender.send(message);
            ctx.request_repaint();
        });
    }

    /// Colours `side` again from the last checkpoint before `changed_line`.
    fn start_highlighting(&mut self, ctx: &egui::Context, side: Side, changed_line: usize) {
        self.cancel_highlighting(side);
        let index = side.index();
        let (Some(docs), Some(syntax)) = (&self.docs, self.syntax[index].clone()) else {
            return;
        };
        let lines = docs[index].lines.clone();
        let sender = self.sender.clone();
        let cancel = self.cancel[index].clone();
        let checkpoints = self.checkpoints[index].clone();
        let generation = self.generation[index];
        let theme = self.syntax_theme;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            highlight::highlight_lines(&syntax, theme, &lines, changed_line, &checkpoints, &cancel, |start, styles| {
                let _ = sender.send(Message::Styles {
                    generation,
                    side,
                    start,
                    lines: styles,
                });
                ctx.request_repaint();
            });
        });
    }

    fn highlight_both(&mut self, ctx: &egui::Context) {
        self.checkpoints.iter().for_each(|checkpoints| checkpoints.clear());
        self.start_highlighting(ctx, Side::Left, 0);
        self.start_highlighting(ctx, Side::Right, 0);
    }

    pub fn set_syntax_theme(&mut self, ctx: &egui::Context, theme: SyntaxTheme) {
        if self.syntax_theme != theme {
            self.syntax_theme = theme;
            for side in 0..2 {
                self.styles[side].iter_mut().for_each(|s| *s = None);
            }
            self.highlight_both(ctx);
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Loaded { docs, diff, syntax, documents, originals, projection } => {
                    self.conflicted = [0, 1].map(|side| {
                        let source = &self.sources[side];
                        source.contents.is_none() && source.extract.is_none() && has_conflict_markers(&originals[side])
                    });
                    self.originals = originals.map(Some);
                    self.styles = [vec![None; docs[0].len()], vec![None; docs[1].len()]];
                    self.docs = Some(docs);
                    self.diff = diff;
                    self.syntax = syntax;
                    self.documents = documents;
                    match projection {
                        Some(Ok((edits, alters))) => self.record_projection(edits, alters),
                        // Not JSON after all: the files are compared as text, and told why.
                        Some(Err(errors)) => {
                            if let Some(json) = &mut self.json {
                                json.structural = false;
                            }
                            self.json_failed(errors);
                        }
                        None => {}
                    }
                    self.loading = false;
                    self.layout.valid = false;
                    self.scroll = 0.0;
                    self.scroll_x = 0.0;
                    self.current_block = None;
                    self.unfolded.clear();
                    self.compute_kept();
                    self.compute_matches();
                    self.highlight_both(ctx);
                    if std::mem::take(&mut self.stale_diff) {
                        self.recompute(Side::Left);
                    }
                }
                Message::Failed(error) => {
                    self.error = Some(error);
                    self.loading = false;
                }
                Message::Projected { job, revisions, outcome } => self.projected(ctx, job, revisions, outcome),
                Message::Paths { side, revision, map } => {
                    if let Some(json) = &mut self.json {
                        json.scanning[side.index()] = false;
                        let newer = json.paths[side.index()].as_ref().is_none_or(|(known, _)| *known <= revision);
                        if newer {
                            json.paths[side.index()] = Some((revision, map));
                        }
                    }
                }
                Message::Summary { job, outcome } => {
                    if let Some(summary) = self.summary.as_mut().filter(|summary| summary.job == job) {
                        summary.outcome = Some(outcome);
                    }
                }
                Message::Picked(side, path) => self.change_path(ctx, side, path),
                Message::Styles {
                    generation,
                    side,
                    start,
                    lines,
                } if generation == self.generation[side.index()] => {
                    let target = &mut self.styles[side.index()];
                    for (offset, line) in lines.into_iter().enumerate() {
                        if let Some(slot) = target.get_mut(start + offset) {
                            *slot = Some(line);
                        }
                    }
                }
                Message::Styles { .. } => {}
            }
        }
    }

    /// Puts the projection made by the loader on the undo stack: Ctrl+Z gives back the files'
    /// own layout. A projection of an untouched side leaves it clean, as nothing typed would be
    /// lost; saving then writes nothing unless the user edits.
    fn record_projection(&mut self, edits: [Option<Edit>; 2], alters: [bool; 2]) {
        let group = self.next_group;
        self.next_group += 1;
        for (index, edit) in edits.into_iter().enumerate() {
            let Some(edit) = edit else {
                continue;
            };
            let side = if index == 0 { Side::Left } else { Side::Right };
            let revision_before = self.revision[index];
            let revision_after = self.next_revision;
            self.next_revision += 1;
            self.revision[index] = revision_after;
            if self.clean[index].contains(&revision_before) {
                self.clean[index].push(revision_after);
            }
            self.undo.push(Step {
                side,
                edit,
                group,
                alters: alters[index] && !self.is_read_only(side),
                revision_before,
                revision_after,
                caret_before: None,
                caret_after: None,
                typed_at: None,
            });
        }
        self.redo.clear();
    }

    /// The sides that are not JSON: the comparison stays as it is, the reasons are shown.
    fn json_failed(&mut self, errors: [Option<String>; 2]) {
        let names = [self.side_name(Side::Left), self.side_name(Side::Right)];
        if let Some(json) = &mut self.json {
            self.message = formats::json_failure(&errors, &names);
            json.errors = errors;
        }
    }

    /// Asks for a new projection with `options`. Panes holding edits are only rewritten once the
    /// user agrees; without the structural comparison, the options are only kept.
    pub fn request_projection(&mut self, ctx: &egui::Context, options: JsonOptions) {
        let Some(json) = &mut self.json else {
            return;
        };
        if !json.wanted {
            json.options = options;
            return;
        }
        if self.is_dirty() {
            self.question = Some(Question::Reproject { options });
        } else {
            self.start_projection(ctx, options);
        }
    }

    /// Projects both panes again on a worker, from what they hold now.
    fn start_projection(&mut self, ctx: &egui::Context, options: JsonOptions) {
        let (Some(json), Some(docs)) = (&mut self.json, &self.docs) else {
            return;
        };
        json.options = options.clone();
        json.wanted = true;
        let job = json.next_job;
        json.next_job += 1;
        json.pending = Some(job);
        let format = json.format;
        let docs = [docs[0].clone(), docs[1].clone()];
        // A side without edits starts again from its file.
        let dirty = [self.side_dirty(Side::Left), self.side_dirty(Side::Right)];
        let sources = [0, 1].map(|index| self.originals[index].clone().filter(|_| !dirty[index]));
        let revisions = self.revision;
        let (diff_options, rules) = (self.options, self.rules.clone());
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let outcome = project_documents(docs, sources, format, &options, diff_options, &rules);
            let _ = sender.send(Message::Projected { job, revisions, outcome });
            ctx.request_repaint();
        });
    }

    fn projected(
        &mut self,
        ctx: &egui::Context,
        job: u64,
        revisions: [u64; 2],
        outcome: Result<Projected, [Option<String>; 2]>,
    ) {
        let Some(json) = self.json.as_mut().filter(|json| json.pending == Some(job)) else {
            return;
        };
        json.pending = None;
        if revisions != self.revision {
            self.message = Some("Le texte a changé pendant le réalignement : relance « Réaligner »".to_owned());
            return;
        }
        let Projected { docs, edits, alters, diff } = match outcome {
            Ok(projected) => projected,
            Err(errors) => return self.json_failed(errors),
        };
        let Some(json) = &mut self.json else {
            return;
        };
        let was_structural = std::mem::replace(&mut json.structural, true);
        json.errors = [None, None];
        let changed = edits.each_ref().map(Option::is_some);
        self.docs = Some(docs);
        self.diff = diff;
        self.record_projection(edits, alters);
        self.message = None;
        if changed == [false, false] && was_structural {
            return;
        }
        self.selection = None;
        self.caret = None;
        self.layout.valid = false;
        self.scroll = 0.0;
        self.scroll_x = 0.0;
        self.current_block = None;
        self.unfolded.clear();
        self.jump_to_first = true;
        self.compute_kept();
        self.compute_matches();
        for side in [Side::Left, Side::Right] {
            if changed[side.index()] {
                let len = self.docs.as_ref().map_or(0, |docs| docs[side.index()].len());
                self.styles[side.index()] = vec![None; len];
                self.checkpoints[side.index()].clear();
                self.start_highlighting(ctx, side, 0);
            }
        }
    }

    /// Back to the plain line comparison, or to the structural one.
    pub fn set_structural(&mut self, ctx: &egui::Context, structural: bool) {
        let Some(json) = &mut self.json else {
            return;
        };
        if structural {
            let options = json.options.clone();
            json.wanted = true;
            if self.is_dirty() {
                self.question = Some(Question::Reproject { options });
            } else {
                self.start_projection(ctx, options);
            }
            return;
        }
        json.wanted = false;
        json.structural = false;
        if self.is_dirty() {
            // Edits are kept as they are; the layout made for the comparison can be undone.
            self.recompute(Side::Left);
            self.message = Some("Comparaison en texte simple : Ctrl+Z défait la mise en forme JSON".to_owned());
        } else {
            self.reload(ctx);
        }
    }

    /// Re-projects both panes with the options in force: what "Réaligner" does.
    pub fn realign(&mut self, ctx: &egui::Context) {
        if let Some(options) = self.json.as_ref().map(|json| json.options.clone()) {
            if let Some(json) = &mut self.json {
                json.wanted = true;
            }
            self.request_projection(ctx, options);
        }
    }

    /// A tab whose files are JSON, whichever comparison is in force.
    pub fn is_json(&self) -> bool {
        self.json.is_some()
    }

    /// The structural comparison is in force (or being made).
    pub fn is_structural(&self) -> bool {
        self.json.as_ref().is_some_and(|json| json.structural || json.pending.is_some())
    }

    pub fn json_options(&self) -> Option<&JsonOptions> {
        self.json.as_ref().map(|json| &json.options)
    }

    fn structural_format(&self) -> Option<Format> {
        self.json.as_ref().filter(|json| json.structural).map(|json| json.format)
    }

    /// Document tabs keep their own wrapping, one paragraph being one line.
    pub fn owns_wrap(&self) -> bool {
        self.own_wrap
    }

    pub fn wraps(&self) -> bool {
        self.layout.wrap
    }

    /// New extraction options: the documents are read again, unless edits would be lost.
    pub fn set_extract_options(&mut self, ctx: &egui::Context, options: ExtractOptions) {
        if self.extract_options == options {
            return;
        }
        self.extract_options = options;
        let mut any = false;
        for source in &mut self.sources {
            if let Some(extract) = &mut source.extract {
                *extract = options;
                any = true;
            }
        }
        if !any {
            return;
        }
        if self.is_dirty() {
            self.message = Some("Enregistre d'abord : F5 relira les documents avec ces options".to_owned());
        } else {
            self.reload(ctx);
        }
    }

    /// A dialog of this tab is open: the application keeps its shortcuts to itself.
    pub fn wants_dialog(&self) -> bool {
        self.question.is_some() || self.summary.is_some()
    }

    /// Lists the differences of value on a worker, in the "Changements JSON" dialog.
    pub fn open_json_summary(&mut self, ctx: &egui::Context) {
        let names = [self.side_name(Side::Left), self.side_name(Side::Right)];
        let (Some(json), Some(docs)) = (&mut self.json, &self.docs) else {
            return;
        };
        let job = json.next_job;
        json.next_job += 1;
        self.summary = Some(SummaryDialog { job, outcome: None });
        let lines = [docs[0].lines.clone(), docs[1].lines.clone()];
        let (format, options) = (json.format, json.options.clone());
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let parse = |lines: &[Line]| {
                let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
                JsonDoc::parse_lines(&texts, format)
            };
            let (left, right) = rayon::join(|| parse(&lines[0]), || parse(&lines[1]));
            let outcome = match (left, right) {
                (Ok(left), Ok(right)) => Ok(json::summarize(&left, &right, &options)),
                (left, right) => {
                    let errors = [left.err(), right.err()].map(|error| error.map(|e| formats::describe_json_error(&e)));
                    Err(formats::json_failure(&errors, &names).unwrap_or_default())
                }
            };
            let _ = sender.send(Message::Summary { job, outcome });
            ctx.request_repaint();
        });
    }

    /// The JSON paths of a side, scanned again when its text changed: at once for an ordinary
    /// file, on a worker for a big one.
    fn refresh_paths(&mut self, ctx: &egui::Context, side: Side) {
        let index = side.index();
        let revision = self.revision[index];
        let (Some(json), Some(docs)) = (&mut self.json, &self.docs) else {
            return;
        };
        if json.paths[index].as_ref().is_some_and(|(known, _)| *known == revision) || json.scanning[index] {
            return;
        }
        let format = json.format;
        let lines = docs[index].lines.clone();
        if lines.len() <= PATH_SCAN_INLINE_LINES {
            let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
            json.paths[index] = Some((revision, PathMap::scan(&texts, format)));
            return;
        }
        json.scanning[index] = true;
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
            let map = PathMap::scan(&texts, format);
            let _ = sender.send(Message::Paths { side, revision, map });
            ctx.request_repaint();
        });
    }

    /// The JSON path of the line the caret is on, or of the top line of the left pane.
    pub fn caret_json_path(&self) -> Option<String> {
        let json = self.json.as_ref()?;
        let (side, line) = match self.caret {
            Some((side, pos)) => (side, pos.line as usize),
            None => {
                let visual = self.pane_scroll[0] / self.layout.line_height as f64;
                (Side::Left, line_at(&self.layout.offsets[0], visual as u32))
            }
        };
        json.paths[side.index()].as_ref()?.1.path_of_line(line)
    }

    /// Sends the caret to where `path` starts on `side`.
    fn jump_to_path(&mut self, side: Side, path: &str) -> bool {
        let (Some(json), Some(docs)) = (&self.json, &self.docs) else {
            return false;
        };
        let texts: Vec<&str> = docs[side.index()].lines.iter().map(|line| line.text.as_str()).collect();
        let Some(line) = PathMap::scan(&texts, json.format).line_of_path(path) else {
            return false;
        };
        let pos = TextPos::new(line as u32, 0);
        self.jump_to(side, pos, pos);
        true
    }

    pub fn set_options(&mut self, options: DiffOptions) {
        if self.options != options {
            self.options = options;
            self.stale_diff |= self.loading;
            self.recompute(Side::Left);
        }
    }

    /// New ignore rules: no line moves, only blocks appear or disappear.
    pub fn set_rules(&mut self, rules: IgnoreRules) {
        self.rules = rules;
        self.stale_diff |= self.loading;
        self.recompute(Side::Left);
    }

    /// How many rows the ignore rules keep out of the blocks.
    #[cfg(test)]
    pub fn ignored_row_count(&self) -> usize {
        self.diff.ignored_rows.len()
    }

    /// Compares again. `stable`: a side whose lines kept their numbers.
    fn recompute(&mut self, stable: Side) {
        let unfolded = self.unfolded_lines(stable);
        if let Some(docs) = &self.docs {
            let structural = self.json.as_ref().is_some_and(|json| json.structural);
            self.diff = compute_diff(&docs[0], &docs[1], self.options, &self.rules, structural);
            self.layout.valid = false;
            self.restore_unfolded(stable, unfolded);
            self.compute_kept();
            self.compute_matches();
            // The block that took this place, or the last one.
            let blocks = self.diff.blocks.len();
            self.current_block = self.current_block.and_then(|current| {
                (current..blocks)
                    .find(|&block| self.block_kept(block))
                    .or_else(|| (0..current.min(blocks)).rev().find(|&block| self.block_kept(block)))
            });
        }
    }

    pub fn set_wrap(&mut self, wrap_lines: bool) {
        if self.layout.wrap != wrap_lines {
            self.layout.wrap = wrap_lines;
            self.layout.valid = false;
            self.scroll_x = 0.0;
        }
    }

    pub fn set_aligned(&mut self, aligned: bool) {
        if self.layout.aligned != aligned {
            let place = self.layout.sync.locate(self.scroll / self.layout.line_height as f64);
            self.layout.aligned = aligned;
            self.layout.valid = false;
            self.pending_place = Some(place);
        }
    }

    /// Lines that only changed keep their colours until the new ones arrive: no flash while typing.
    fn splice_styles(&mut self, side: Side, edit: &Edit) {
        let styles = &mut self.styles[side.index()];
        let at = (edit.at + edit.removed.len().min(edit.inserted.len())).min(styles.len());
        let end = (edit.at + edit.removed.len()).min(styles.len());
        let added = edit.inserted.len().saturating_sub(edit.removed.len());
        styles.splice(at..end.max(at), std::iter::repeat_n(None, added));
    }

    /// Applies edits of an undo or a redo, in order, then compares once.
    fn apply_edits(&mut self, ctx: &egui::Context, edits: &[(Side, Edit)]) {
        let mut first_changed: [Option<usize>; 2] = [None, None];
        for (side, edit) in edits {
            if let Some(docs) = &mut self.docs {
                docs[side.index()].apply(edit);
            }
            self.splice_styles(*side, edit);
            let first = &mut first_changed[side.index()];
            *first = Some(first.map_or(edit.at, |at| at.min(edit.at)));
        }
        self.selection = None;
        self.caret = None;
        self.reveal_current = true;
        // A side no edit touched kept its line numbers.
        let stable = if first_changed[0].is_none() { Side::Left } else { Side::Right };
        self.recompute(stable);
        for side in [Side::Left, Side::Right] {
            if let Some(at) = first_changed[side.index()] {
                self.start_highlighting(ctx, side, at);
            }
        }
    }

    /// Copies a block to the opposite side.
    pub fn push_block(&mut self, ctx: &egui::Context, block: usize, target: Side) {
        if self.is_read_only(target) {
            self.refuse_read_only(target);
            return;
        }
        let Some(docs) = &mut self.docs else {
            return;
        };
        let Some(block) = self.diff.blocks.get(block).cloned() else {
            return;
        };
        let range = |r: &Range<u32>| r.start as usize..r.end as usize;
        let (source_range, target_range) = match target {
            Side::Right => (range(&block.left), range(&block.right)),
            Side::Left => (range(&block.right), range(&block.left)),
        };
        // The pushed lines keep their colours at once; a full pass follows in the background.
        let source_styles: Vec<Option<LineStyles>> = self.styles[target.other().index()]
            .get(source_range.clone())
            .map_or_else(Vec::new, <[_]>::to_vec);
        let texts: Vec<String> = docs[target.other().index()].lines[source_range]
            .iter()
            .map(|l| l.text.clone())
            .collect();
        let edit = docs[target.index()].replace_lines(target_range, texts);
        // Already applied by `replace_lines`: only views have to follow.
        let mut changed_line = edit.at;
        let styles = &mut self.styles[target.index()];
        let end = (edit.at + edit.removed.len()).min(styles.len());
        let unstyled = edit.inserted.len().saturating_sub(source_styles.len());
        let inserted = std::iter::repeat_n(None, unstyled).chain(source_styles);
        styles.splice(edit.at.min(end)..end, inserted);
        // The transfer and the commas it calls for are undone together.
        let group = self.next_group;
        self.next_group += 1;
        self.push_step(target, edit, group);
        if let Some(edit) = self.repair_commas(target) {
            changed_line = changed_line.min(edit.at);
            self.splice_styles(target, &edit);
            self.push_step(target, edit, group);
        }
        self.redo.clear();
        self.selection = None;
        self.caret = None;
        self.reveal_current = true;
        self.recompute(target.other());
        self.start_highlighting(ctx, target, changed_line);
    }

    /// Records an edit already applied to `side`, as a step of `group`.
    fn push_step(&mut self, side: Side, edit: Edit, group: u64) {
        let revision_before = self.revision[side.index()];
        let revision_after = self.next_revision;
        self.next_revision += 1;
        self.revision[side.index()] = revision_after;
        self.undo.push(Step {
            side,
            edit,
            group,
            alters: false,
            revision_before,
            revision_after,
            caret_before: None,
            caret_after: None,
            typed_at: None,
        });
    }

    /// Under the structural comparison, puts back the commas a transfer broke on `side`, in one
    /// edit spanning the lines repaired.
    fn repair_commas(&mut self, side: Side) -> Option<Edit> {
        let format = self.structural_format()?;
        let doc = &mut self.docs.as_mut()?[side.index()];
        let repairs = {
            let texts: Vec<&str> = doc.lines.iter().map(|line| line.text.as_str()).collect();
            json::repair_commas(&texts, format)
        };
        let range = repairs.first()?.0..repairs.last()?.0 + 1;
        let mut lines: Vec<String> = doc.lines[range.clone()].iter().map(|line| line.text.clone()).collect();
        for (at, text) in repairs {
            lines[at - range.start] = text;
        }
        Some(doc.edit_lines(range, lines))
    }

    pub fn scroll_lines(&mut self, lines: f64) {
        self.scroll += lines * self.layout.line_height as f64;
    }

    pub fn push_current(&mut self, ctx: &egui::Context, target: Side) {
        let Some(block) = self.current_block else {
            return;
        };
        // Nothing is sent unseen: a block out of view is shown first.
        if self.block_in_view(block) {
            self.push_block(ctx, block, target);
        } else {
            self.select_block(block);
        }
    }

    /// The last step of a stack, with the steps of its group.
    fn take_group(stack: &mut Vec<Step>) -> Vec<Step> {
        let Some(last) = stack.pop() else {
            return Vec::new();
        };
        let group = last.group;
        let mut steps = vec![last];
        while group != 0 && stack.last().is_some_and(|step| step.group == group) {
            steps.extend(stack.pop());
        }
        steps
    }

    pub fn undo(&mut self, ctx: &egui::Context) {
        let steps = Self::take_group(&mut self.undo);
        let Some(first) = steps.last() else {
            return;
        };
        let caret = first.caret_before;
        let edits: Vec<(Side, Edit)> = steps.iter().map(|step| (step.side, step.edit.inverted())).collect();
        self.apply_edits(ctx, &edits);
        for step in &steps {
            self.revision[step.side.index()] = step.revision_before;
        }
        self.restore_caret(caret);
        self.redo.extend(steps);
    }

    pub fn redo(&mut self, ctx: &egui::Context) {
        let steps = Self::take_group(&mut self.redo);
        let Some(last) = steps.last() else {
            return;
        };
        let caret = last.caret_after;
        let edits: Vec<(Side, Edit)> = steps.iter().map(|step| (step.side, step.edit.clone())).collect();
        self.apply_edits(ctx, &edits);
        for step in &steps {
            self.revision[step.side.index()] = step.revision_after;
        }
        self.restore_caret(caret);
        self.undo.extend(steps);
    }

    fn restore_caret(&mut self, caret: Option<(Side, TextPos)>) {
        if let Some((side, pos)) = caret {
            self.set_caret(side, pos, false);
        }
    }

    /// Saves the modified sides. Stops at the first failure, which is returned, or at a side
    /// that needs the user's word first: `Ok(false)`, the question being shown.
    pub fn save(&mut self) -> Result<bool, String> {
        for side in [Side::Left, Side::Right] {
            if self.side_dirty(side) {
                self.check_saveable(side)?;
                if let Some(question) = self.save_question(side, true) {
                    self.question = Some(question);
                    return Ok(false);
                }
                self.write_side(side)?;
            }
        }
        self.message = None;
        Ok(true)
    }

    fn write_side(&mut self, side: Side) -> Result<(), String> {
        let index = side.index();
        let Some(docs) = &mut self.docs else {
            return Ok(());
        };
        docs[index].save().map_err(|e| e.to_string())?;
        self.originals[index] = Some(docs[index].lines.clone());
        self.saved_revision[index] = self.revision[index];
        self.clean[index] = vec![self.revision[index]];
        Ok(())
    }

    /// Before writing JSON: once for a side shown reformatted, every time for invalid JSON.
    fn save_question(&self, side: Side, all: bool) -> Option<Question> {
        let json = self.json.as_ref()?;
        let docs = self.docs.as_ref()?;
        let reformatted = self.altered(side) && !json.save_confirmed[side.index()];
        let texts: Vec<&str> = docs[side.index()].lines.iter().map(|line| line.text.as_str()).collect();
        let invalid = JsonDoc::parse_lines(&texts, json.format).err().map(|error| formats::describe_json_error(&error));
        (reformatted || invalid.is_some()).then_some(Question::Save { side, reformatted, invalid, all })
    }

    /// Read-only sides and files changed behind the tab's back are never written.
    fn check_saveable(&self, side: Side) -> Result<(), String> {
        if self.is_read_only(side) {
            return Err(format!("{} est en lecture seule", self.side_name(side)));
        }
        if self.changed_on_disk[side.index()] {
            return Err(format!(
                "{} a changé sur le disque depuis son ouverture : enregistrement refusé",
                self.side_name(side)
            ));
        }
        Ok(())
    }

    pub fn go_to_block(&mut self, forward: bool) {
        let count = self.diff.blocks.len();
        if count == 0 {
            return;
        }
        // Moving between blocks leaves the text: typing must not land out of view.
        self.caret = None;
        self.selection = None;
        let top = self.focal_line();
        // Scrolled away from the current block, the arrows start from what is on screen.
        let current = self.current_block.filter(|&block| self.block_in_view(block));
        let kept: Vec<usize> = (0..count).filter(|&block| self.block_kept(block)).collect();
        let starts = |block: &usize| self.layout.sync.blocks.get(*block).map_or(0.0, |b| b.start as f64);
        let target = match current {
            Some(current) if forward => kept.iter().find(|&&b| b > current).copied().or(Some(current)),
            Some(current) => kept.iter().rev().find(|&&b| b < current).copied().or(Some(current)),
            None if forward => kept.iter().find(|b| starts(b) >= top).or(kept.last()).copied(),
            None => kept.iter().rev().find(|b| starts(b) < top).or(kept.first()).copied(),
        };
        if let Some(target) = target {
            self.select_block(target);
        }
    }

    fn block_in_view(&self, block: usize) -> bool {
        if !self.block_kept(block) {
            return false;
        }
        let Some(block) = self.diff.blocks.get(block) else {
            return false;
        };
        let line = self.layout.line_height as f64;
        let view = self.view_lines() * line;
        [&block.left, &block.right].iter().enumerate().any(|(side, range)| {
            let offsets = &self.layout.offsets[side];
            let scroll = self.pane_scroll[side];
            let position = |line_index: u32| {
                offsets.get(line_index as usize).map_or(0.0, |&visual| visual as f64 * line - scroll)
            };
            position(range.end) >= 0.0 && position(range.start) <= view
        })
    }

    fn view_lines(&self) -> f64 {
        self.geometry.map_or(0.0, |g| (g.view.height() / g.line_height) as f64)
    }

    fn focal_line(&self) -> f64 {
        let line = self.layout.line_height as f64;
        self.layout.sync.focal(self.scroll / line, self.view_lines())
    }

    /// Brings a block where the two panes agree, so that its two frames are level.
    fn select_block(&mut self, block: usize) {
        self.current_block = Some(block);
        if let Some(extent) = self.layout.sync.blocks.get(block) {
            let line = self.layout.line_height as f64;
            self.scroll = self.layout.sync.scroll_showing(extent.start as f64, self.view_lines()) * line;
            self.update_pane_scroll();
        }
    }

    fn update_pane_scroll(&mut self) {
        let line = self.layout.line_height as f64;
        self.pane_scroll = self
            .layout
            .sync
            .pane_scrolls(self.scroll / line, self.view_lines())
            .map(|lines| lines * line);
    }

    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let docs = self.docs.as_ref()?;
        let (start, end) = selection.ordered();
        let text = textedit::text_between(&docs[selection.side.index()], start, end);
        (!text.is_empty()).then_some(text)
    }

    fn set_caret(&mut self, side: Side, pos: TextPos, extend: bool) {
        let pos = match &self.docs {
            Some(docs) => textedit::clamp(&docs[side.index()], pos),
            None => pos,
        };
        let anchor = match self.selection {
            Some(selection) if extend && selection.side == side => selection.anchor,
            _ => pos,
        };
        self.caret = Some((side, pos));
        self.selection = Some(Selection { side, anchor, head: pos });
        self.caret_to_reveal = true;
        // A caret never sits on a folded line: the folds are worked out again around it.
        let fold = self.layout.folds[side.index()].get(pos.line as usize);
        if fold.is_some_and(|&fold| fold != Fold::Shown) {
            self.relayout_in_place();
        }
    }

    /// Replaces the selection, or inserts at the caret. Every edit goes through here.
    fn replace_selection(&mut self, ctx: &egui::Context, text: &str, typing: bool) {
        let Some((side, head)) = self.caret else {
            return;
        };
        let (start, end) = self
            .selection
            .filter(|selection| selection.side == side)
            .map_or((head, head), |selection| selection.ordered());
        self.replace_range(ctx, side, start, end, text, typing);
    }

    fn replace_range(
        &mut self,
        ctx: &egui::Context,
        side: Side,
        start: TextPos,
        end: TextPos,
        text: &str,
        typing: bool,
    ) {
        if self.is_read_only(side) {
            self.refuse_read_only(side);
            return;
        }
        let index = side.index();
        let line_pixels = self.layout.line_height as f64;
        let on_screen = self.layout.offsets[index]
            .get(start.line as usize)
            .map(|&visual| visual as f64 * line_pixels - self.pane_scroll[index]);
        let Some(docs) = &mut self.docs else {
            return;
        };
        let doc = &mut docs[index];
        if doc.lossy {
            self.message = Some("Ce fichier n'a pas pu être décodé sans pertes : lecture seule".to_owned());
            return;
        }
        let (start, end) = (textedit::clamp(doc, start), textedit::clamp(doc, end));
        let (edit, caret) = textedit::replace(doc, start, end, text);
        self.anchor = on_screen.map(|y| (index, start.line, y));
        self.commit_edit(ctx, side, edit, caret, typing);
    }

    fn commit_edit(&mut self, ctx: &egui::Context, side: Side, edit: Edit, caret: TextPos, typing: bool) {
        let index = side.index();
        let caret_before = self.caret;
        let caret_after = Some((side, caret));
        let now = ctx.input(|input| input.time);
        self.splice_styles(side, &edit);
        let changed_line = edit.at;
        let revision_before = self.revision[index];
        let revision_after = self.next_revision;
        self.next_revision += 1;
        self.revision[index] = revision_after;

        let same_line = edit.removed.len() == 1 && edit.inserted.len() == 1;
        let burst = self.undo.last_mut().filter(|last| {
            typing
                && same_line
                && last.side == side
                && last.edit.at == edit.at
                && last.edit.inserted.len() == 1
                && last.caret_after == caret_before
                && last.typed_at.is_some_and(|at| now - at < UNDO_MERGE_SECONDS)
        });
        if let Some(last) = burst {
            last.edit.inserted = edit.inserted;
            last.revision_after = revision_after;
            last.caret_after = caret_after;
            last.typed_at = Some(now);
        } else {
            self.undo.push(Step {
                side,
                edit,
                group: 0,
                alters: false,
                revision_before,
                revision_after,
                caret_before,
                caret_after,
                typed_at: typing.then_some(now),
            });
        }
        self.redo.clear();
        self.goal_column = None;
        self.set_caret(side, caret, false);
        self.recompute(side.other());
        self.start_highlighting(ctx, side, changed_line);
    }

    fn visual_ranges(&self, text: &str) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        if self.layout.wrap {
            wrap::for_each_visual_line(text, self.layout.columns, |range| ranges.push(range));
        } else {
            ranges.push(0..text.len());
        }
        ranges
    }

    /// Up or down one visual line, keeping the column, across the lines the filter hides.
    fn moved_vertically(&mut self, side: Side, pos: TextPos, down: bool) -> TextPos {
        let index = side.index();
        let Some(docs) = &self.docs else {
            return pos;
        };
        let doc = &docs[index];
        let text = doc.lines.get(pos.line as usize).map_or("", |l| l.text.as_str());
        let ranges = self.visual_ranges(text);
        let row = ranges
            .iter()
            .rposition(|range| range.start <= pos.byte)
            .unwrap_or(0);
        let column = *self
            .goal_column
            .get_or_insert_with(|| wrap::column_of(text, ranges[row].start, pos.byte));
        let inside = if down { row + 1 < ranges.len() } else { row > 0 };
        if inside {
            let target = if down { row + 1 } else { row - 1 };
            return TextPos::new(pos.line, wrap::offset_at_column(text, ranges[target].clone(), column));
        }
        let mut line = pos.line as i64;
        loop {
            line += if down { 1 } else { -1 };
            if line < 0 || line >= doc.len() as i64 {
                return pos;
            }
            if self.layout.folds[index].get(line as usize) == Some(&Fold::Shown) {
                break;
            }
        }
        let text = doc.lines[line as usize].text.as_str();
        let ranges = self.visual_ranges(text);
        let target = if down { 0 } else { ranges.len() - 1 };
        TextPos::new(line as u32, wrap::offset_at_column(text, ranges[target].clone(), column))
    }

    fn handle_key(&mut self, ctx: &egui::Context, key: Key, modifiers: Modifiers) {
        let Some((side, head)) = self.caret else {
            return;
        };
        let extend = modifiers.shift;
        if matches!(key, Key::ArrowUp | Key::ArrowDown) {
            let target = self.moved_vertically(side, head, key == Key::ArrowDown);
            self.set_caret(side, target, extend);
            return;
        }
        let Some(docs) = &self.docs else {
            return;
        };
        let doc = &docs[side.index()];
        let selected = self
            .selection
            .filter(|selection| selection.side == side && selection.anchor != selection.head)
            .map(|selection| selection.ordered());
        let page = (self.view_lines() - 2.0).max(1.0) as u32;
        if let Some(target) = caret_motion(doc, key, modifiers, head, selected, page) {
            self.goal_column = None;
            self.set_caret(side, target, extend);
            return;
        }
        match key {
            Key::Backspace | Key::Delete => {
                if selected.is_some() {
                    self.replace_selection(ctx, "", false);
                    return;
                }
                let (start, end) = deleted_range(doc, key, modifiers, head);
                if start != end {
                    self.replace_range(ctx, side, start, end, "", true);
                }
            }
            Key::Enter => {
                let indent = textedit::indent_of(doc, head.line);
                self.replace_selection(ctx, &format!("\n{indent}"), false);
            }
            Key::Tab => {
                let text = textedit::tab_text(doc);
                self.replace_selection(ctx, text, false);
            }
            Key::A if modifiers.command => {
                let start = TextPos::default();
                let end = textedit::end_of_document(doc);
                self.selection = Some(Selection { side, anchor: start, head: end });
                self.caret = Some((side, end));
            }
            _ => {}
        }
    }

    /// Everything typed since the last frame. Arrows with Ctrl were taken by the shortcuts.
    fn handle_editing(&mut self, ctx: &egui::Context) {
        if self.caret.is_none() || ctx.egui_wants_keyboard_input() {
            return;
        }
        for event in ctx.input(|input| input.events.clone()) {
            match event {
                Event::Text(text) => {
                    if !text.chars().any(char::is_control) {
                        self.replace_selection(ctx, &text, true);
                    }
                }
                Event::Ime(ImeEvent::Commit(text)) if !text.is_empty() => {
                    self.replace_selection(ctx, &text, true);
                }
                Event::Paste(text) => self.replace_selection(ctx, &text, false),
                Event::Cut => {
                    if let Some(text) = self.selected_text() {
                        ctx.copy_text(text);
                        self.replace_selection(ctx, "", false);
                    }
                }
                Event::Key { key, pressed: true, modifiers, .. } => self.handle_key(ctx, key, modifiers),
                _ => {}
            }
        }
    }

    /// Puts the shared position where `side` scrolls to `target` pixels.
    fn scroll_pane_to(&mut self, side: usize, target: f64) {
        let line = self.layout.line_height as f64;
        let view = self.view_lines();
        let wanted = target / line;
        if wanted <= 0.0 {
            // The top exactly, not the bisection's last approximation of it.
            self.scroll = 0.0;
            self.update_pane_scroll();
            return;
        }
        let sync = &self.layout.sync;
        let (mut low, mut high) = (0.0, sync.max_scroll(view));
        for _ in 0..48 {
            let middle = (low + high) / 2.0;
            if sync.pane_scrolls(middle, view)[side] < wanted {
                low = middle;
            } else {
                high = middle;
            }
        }
        self.scroll = high * line;
        self.update_pane_scroll();
    }

    /// Where the caret is drawn: its visual line in the pane and its column there.
    fn caret_place(&self) -> Option<(usize, u32, usize)> {
        let (side, pos) = self.caret?;
        let index = side.index();
        let docs = self.docs.as_ref()?;
        let text = docs[index].lines.get(pos.line as usize)?.text.as_str();
        let ranges = self.visual_ranges(text);
        let row = ranges.iter().rposition(|range| range.start <= pos.byte).unwrap_or(0);
        let visual = self.layout.offsets[index].get(pos.line as usize)? + row as u32;
        Some((index, visual, wrap::column_of(text, ranges[row].start, pos.byte)))
    }

    fn reveal_caret(&mut self) {
        let Some((index, visual, column)) = self.caret_place() else {
            return;
        };
        let line = self.layout.line_height as f64;
        let view = self.view_lines() * line;
        let margin = (CARET_MARGIN_ROWS * line).min(view / 3.0);
        let top = visual as f64 * line;
        let scroll = self.pane_scroll[index];
        let centered = std::mem::take(&mut self.center_caret);
        if centered && (top < scroll + margin || top + line > scroll + view - margin) {
            self.scroll_pane_to(index, top - (view - line) / 2.0);
        } else if top < scroll + margin {
            self.scroll_pane_to(index, top - margin);
        } else if top + line > scroll + view - margin {
            self.scroll_pane_to(index, top + line + margin - view);
        }
        if let (false, Some(geometry)) = (self.layout.wrap, self.geometry) {
            let pane = geometry.panes[index];
            let width = pane.text_right - pane.text_x;
            let x = column as f32 * geometry.char_width;
            if x < self.scroll_x {
                self.scroll_x = x;
            } else if x > self.scroll_x + width {
                self.scroll_x = x - width;
            }
        }
    }

    pub fn status_items(&self) -> Vec<StatusItem> {
        let Some(docs) = &self.docs else {
            return Vec::new();
        };
        let (side, line, column) = match self.caret {
            Some((side, pos)) => {
                let text = docs[side.index()]
                    .lines
                    .get(pos.line as usize)
                    .map_or("", |l| l.text.as_str());
                (side, pos.line as usize, wrap::column_of(text, 0, pos.byte) + 1)
            }
            None => {
                let visual = self.pane_scroll[0] / self.layout.line_height as f64;
                (Side::Left, line_at(&self.layout.offsets[0], visual as u32), 1)
            }
        };
        let doc = &docs[side.index()];
        let mut items = Vec::new();
        if let Some(path) = self.caret_json_path() {
            items.push(StatusItem::Path(path));
        }
        if let Some(syntax) = &self.syntax[side.index()] {
            items.push(StatusItem::Pill(syntax.clone()));
        }
        items.push(StatusItem::Pill(doc.encoding.label().to_owned()));
        items.push(StatusItem::Pill(doc.dominant_eol().label().to_owned()));
        if let Some((current, total)) = self.block_rank() {
            items.push(StatusItem::Text(format!("Bloc {current} sur {total}")));
        }
        items.push(StatusItem::Text(format!(
            "Ligne {} sur {} · Colonne {column}",
            line + 1,
            doc.len().max(1)
        )));
        let ignored = self.diff.ignored_rows.len();
        if ignored > 0 {
            let plural = if ignored > 1 { "s" } else { "" };
            items.push(StatusItem::Pill(format!("{ignored} ligne{plural} ignorée{plural}")));
        }
        let stats = self.diff.stats;
        items.push(if self.diff.is_identical() {
            StatusItem::Text("Identiques".to_owned())
        } else {
            StatusItem::Counts {
                deletions: stats.deletions,
                insertions: stats.insertions,
                changes: stats.changes,
            }
        });
        items
    }

    fn ensure_layout(&mut self, columns: usize) {
        let layout = &mut self.layout;
        if layout.valid && (!layout.wrap || layout.columns == columns) {
            return;
        }
        let Some(docs) = &self.docs else {
            return;
        };
        // Keep the same place when only the wrapping changes.
        let row_pixels = layout.line_height as f64;
        let place = self
            .pending_place
            .take()
            .or_else(|| layout.valid.then(|| layout.sync.locate(self.scroll / row_pixels)));
        layout.columns = columns;
        layout.valid = true;
        let wrap_lines = layout.wrap;
        let heights_of = |doc: &Document| -> Vec<u32> {
            doc.lines
                .par_iter()
                .map(|l| if wrap_lines { wrap::visual_line_count(&l.text, columns) } else { 1 })
                .collect()
        };
        let offsets_of = |heights: Vec<u32>| {
            let mut offsets = Vec::with_capacity(heights.len() + 1);
            let mut total = 0;
            for height in heights {
                offsets.push(total);
                total += height;
            }
            offsets.push(total);
            offsets
        };
        let [left, right] = &**docs;
        let mut heights = [heights_of(left), heights_of(right)];
        layout.max_columns = if wrap_lines {
            0
        } else {
            [left, right]
                .par_iter()
                .map(|doc| {
                    doc.lines
                        .par_iter()
                        .map(|l| wrap::display_width(&l.text))
                        .max()
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
        };
        layout.line_rows = [vec![0; left.len()], vec![0; right.len()]];
        for (index, row) in self.diff.rows.iter().enumerate() {
            if let Some(line) = row.left {
                layout.line_rows[0][line as usize] = index as u32;
            }
            if let Some(line) = row.right {
                layout.line_rows[1][line as usize] = index as u32;
            }
        }
        // The line being edited stays in view, even if the edit makes its block leave the filter
        // or if it is far from any block.
        let pinned = self.caret.and_then(|(side, pos)| {
            layout.line_rows[side.index()].get(pos.line as usize).copied()
        });
        if let Some(block) = pinned.and_then(|row| self.diff.rows[row as usize].block) {
            self.current_block = Some(block as usize);
        }
        layout.shown_rows = visible_rows(&self.diff.rows, &self.kept, self.context, pinned, &self.unfolded);
        layout.folds = fold_lines(&self.diff.rows, &layout.shown_rows, [left.len(), right.len()]);
        for side in 0..2 {
            for (height, fold) in heights[side].iter_mut().zip(&layout.folds[side]) {
                match fold {
                    Fold::Shown => {}
                    Fold::Separator(_) => *height = 1,
                    Fold::Hidden => *height = 0,
                }
            }
        }
        if layout.aligned {
            // A row is as tall as its taller side; a line then starts where its row starts.
            let row_height = |row: &biomerge_core::diff::Row| {
                let height = |side: usize, line: Option<u32>| {
                    line.map_or(0, |line| heights[side][line as usize])
                };
                height(0, row.left).max(height(1, row.right))
            };
            layout.row_offsets =
                offsets_of(self.diff.rows.iter().map(row_height).collect());
            let row_offsets = &layout.row_offsets;
            let total = row_offsets.last().copied().unwrap_or(0);
            let aligned_side = |rows: &[u32]| -> Vec<u32> {
                rows.iter()
                    .map(|&row| row_offsets[row as usize])
                    .chain(std::iter::once(total))
                    .collect()
            };
            layout.offsets = [aligned_side(&layout.line_rows[0]), aligned_side(&layout.line_rows[1])];
        } else {
            layout.row_offsets.clear();
            let [left, right] = heights;
            layout.offsets = [offsets_of(left), offsets_of(right)];
        }
        layout.sync = if layout.aligned {
            // Both sides follow the rows, which makes them scroll as one.
            SyncMap::new(
                self.diff.blocks.iter().map(|b| [b.rows.clone(), b.rows.clone()]),
                [&layout.row_offsets, &layout.row_offsets],
            )
        } else {
            SyncMap::new(
                self.diff.blocks.iter().map(|b| [b.left.clone(), b.right.clone()]),
                [&layout.offsets[0], &layout.offsets[1]],
            )
        };
        if let Some(place) = place {
            self.scroll = layout.sync.position_of(place) * row_pixels;
        }
    }

    /// The visual lines a line itself takes, gaps excluded.
    fn line_extent(&self, side: usize, line: usize) -> Range<u32> {
        let layout = &self.layout;
        let start = layout.offsets[side][line];
        if layout.aligned {
            let row = layout.line_rows[side][line] as usize;
            start..layout.row_offsets[row + 1]
        } else {
            start..layout.offsets[side][line + 1]
        }
    }

    fn visual_range(&self, text: &str, index: usize) -> Range<usize> {
        if !self.layout.wrap {
            return 0..text.len();
        }
        let mut found = text.len()..text.len();
        let mut current = 0;
        wrap::for_each_visual_line(text, self.layout.columns, |range| {
            if current == index {
                found = range.clone();
            } else if current < index {
                found = range.end..range.end;
            }
            current += 1;
        });
        found
    }

    fn hit_test(&self, pos: Pos2) -> Option<(Side, TextPos)> {
        let geometry = self.geometry?;
        let docs = self.docs.as_ref()?;
        let side = if pos.x < (geometry.gap.0 + geometry.gap.1) / 2.0 {
            Side::Left
        } else {
            Side::Right
        };
        let pane = geometry.panes[side.index()];
        let index = side.index();
        let visual = ((pos.y - geometry.view.top()) as f64 + self.pane_scroll[index]).max(0.0)
            / geometry.line_height as f64;
        let offsets = &self.layout.offsets[index];
        if docs[index].is_empty() {
            return Some((side, TextPos::default()));
        }
        // Below the last line: the end of the document.
        let line = line_at(offsets, visual as u32).min(docs[index].len() - 1);
        if self.layout.folds[index].get(line) != Some(&Fold::Shown) {
            return None;
        }
        let text = &docs[index].lines.get(line)?.text;
        let inside = (visual as u32).saturating_sub(offsets[line]);
        let range = self.visual_range(text, inside as usize);
        let column = ((pos.x - pane.text_x + self.scroll_x) / geometry.char_width).max(0.0).round();
        Some((
            side,
            TextPos {
                line: line as u32,
                byte: wrap::offset_at_column(text, range, column as usize),
            },
        ))
    }

    /// The hidden run whose separator is at `pos`: its side, its line there and its rows.
    fn separator_at(&self, pos: Pos2) -> Option<(usize, usize, Range<u32>)> {
        let geometry = self.geometry?;
        let index = geometry.panes.iter().position(|pane| pane.rect.x_range().contains(pos.x))?;
        let visual = ((pos.y - geometry.view.top()) as f64 + self.pane_scroll[index]) / geometry.line_height as f64;
        let offsets = &self.layout.offsets[index];
        if visual < 0.0 || visual as u32 >= *offsets.last()? {
            return None;
        }
        let line = line_at(offsets, visual as u32);
        if !matches!(self.layout.folds[index].get(line)?, Fold::Separator(_)) {
            return None;
        }
        let row = *self.layout.line_rows[index].get(line)? as usize;
        let hidden = |row: &usize| self.layout.shown_rows.get(*row) == Some(&false);
        let start = (0..=row).rev().take_while(hidden).last()?;
        let end = (row..self.layout.shown_rows.len()).take_while(hidden).last()?;
        Some((index, line, start as u32..end as u32 + 1))
    }

    /// Shows a hidden run again, its first line taking the place of the separator.
    fn unfold(&mut self, side: usize, line: usize, rows: Range<u32>) {
        let top = self.layout.offsets[side][line] as f64 * self.layout.line_height as f64;
        self.anchor = Some((side, line as u32, top - self.pane_scroll[side]));
        self.unfolded.push(rows);
        self.layout.valid = false;
    }

    /// Applies a new code size, keeping the same place.
    fn set_code_size(&mut self, size: f32) {
        let height = line_height(size);
        self.code_size = size;
        let previous = self.layout.line_height;
        if height != previous {
            self.layout.line_height = height;
            self.scroll *= (height / previous) as f64;
        }
    }

    pub fn show(&mut self, ui: &mut Ui, look: &Look, code_size: f32) {
        let chrome = &look.ui;
        let doc = &look.doc;
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.set_code_size(code_size);
        let available = ui.available_rect_before_wrap();
        ui.allocate_rect(available, Sense::hover());
        let painter = ui.painter_at(available);
        painter.rect_filled(available, 0.0, chrome.background);

        let (cards, overview) = panes::layout(available, look, 2, true);
        let cards = [cards[0], cards[1]];
        panes::paint_cards(&painter, look, &cards);
        self.pane_headers(ui, &ctx, look, cards);

        let rows = Rect::from_min_max(
            pos2(cards[0].card.left(), cards[0].code.top()),
            pos2(cards[1].card.right(), panes::row_clip(look, cards[0].code).bottom()),
        );
        let view = Rect::from_min_max(
            pos2(rows.left(), rows.top() + CODE_PADDING),
            pos2(rows.right(), cards[0].code.bottom() - CODE_PADDING),
        );
        let font = FontId::new(code_size, FontFamily::Monospace);
        let char_width = ctx.fonts_mut(|fonts| fonts.glyph_width(&font, '0')).max(1.0);
        let line_digits = self
            .docs
            .as_ref()
            .map_or(4, |d| d[0].len().max(d[1].len()).max(1).ilog10() as usize + 1)
            .max(3);
        let gutter = line_digits as f32 * char_width + GUTTER_PADDING * 2.0 + if look.metrics.bar_at_edge { 4.0 } else { 0.0 };
        let make_pane = |card: Rect| {
            let rect = Rect::from_x_y_ranges(card.x_range(), view.y_range());
            Pane {
                rect,
                text_x: rect.left() + gutter + TEXT_PADDING,
                text_right: rect.right() - WRAP_MARGIN,
            }
        };
        let panes = [make_pane(cards[0].card), make_pane(cards[1].card)];
        let geometry = Geometry {
            view,
            rows,
            panes,
            gap: (cards[0].card.right(), cards[1].card.left()),
            overview,
            line_height: self.layout.line_height,
            char_width,
        };
        let columns = (((panes[0].text_right - panes[0].text_x) / char_width) as usize).max(8);
        self.geometry = Some(geometry);

        if let Some(error) = &self.error {
            panes::finish_cards(&painter, look, &cards);
            painter.text(
                view.center(),
                Align2::CENTER_CENTER,
                error,
                FontId::proportional(14.0),
                chrome.error,
            );
            return;
        }
        if self.docs.is_none() {
            panes::finish_cards(&painter, look, &cards);
            let extracting = self.sources.iter().any(|source| source.extract.is_some());
            painter.text(
                view.center(),
                Align2::CENTER_CENTER,
                if extracting { "Extraction du texte…" } else { "Chargement…" },
                FontId::proportional(14.0),
                chrome.text_dim,
            );
            return;
        }
        self.handle_editing(&ctx);
        if self.json.is_some() {
            self.refresh_paths(&ctx, self.caret_side().unwrap_or(Side::Left));
        }
        self.ensure_layout(columns);
        if let Some((side, line, on_screen)) = self.anchor.take() {
            let row_pixels = geometry.line_height as f64;
            if let Some(&visual) = self.layout.offsets[side].get(line as usize) {
                self.scroll_pane_to(side, visual as f64 * row_pixels - on_screen);
            }
        }
        if std::mem::take(&mut self.reveal_current) {
            self.update_pane_scroll();
            if let Some(block) = self.current_block.filter(|&block| !self.block_in_view(block)) {
                self.select_block(block);
            }
        }
        if self.jump_to_first {
            self.jump_to_first = false;
            self.update_pane_scroll();
            if let Some(block) = (0..self.diff.blocks.len()).find(|&block| self.block_kept(block)) {
                self.select_block(block);
            }
        }

        let response = ui.interact(view, ui.scope_id().with("compare-view"), Sense::click_and_drag());
        self.handle_scroll(ui, &response, geometry);
        self.update_pane_scroll();
        let separator = response.hover_pos().and_then(|pos| self.separator_at(pos));
        self.hovered_separator = separator.as_ref().map(|(side, line, _)| (*side, *line));
        if let Some(pos) = response.interact_pointer_pos() {
            if let (true, Some((side, line, rows))) = (response.clicked(), separator) {
                self.unfold(side, line, rows);
                ctx.request_repaint();
            } else if response.double_clicked() {
                self.select_word(pos);
            } else if response.drag_started() || response.clicked() {
                self.place_caret(pos, ui.input(|i| i.modifiers.shift));
            } else if response.dragged() {
                self.extend_selection(pos);
            }
        }
        if std::mem::take(&mut self.caret_to_reveal) {
            self.reveal_caret();
            self.update_pane_scroll();
        }
        if let Some((index, visual, column)) = self.caret_place() {
            // Blink, and tell the system where composed text (dead keys) appears.
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(CARET_BLINK_SECONDS));
            let pane = geometry.panes[index];
            let x = pane.text_x + column as f32 * char_width - self.scroll_x;
            let y = view.top() + (visual as f64 * geometry.line_height as f64 - self.pane_scroll[index]) as f32;
            let cursor_rect = Rect::from_min_size(pos2(x, y), vec2(CARET_WIDTH, geometry.line_height));
            ctx.output_mut(|output| {
                output.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect: pane.rect,
                    cursor_rect,
                    should_interrupt_composition: false,
                });
            });
        }
        if self.hovered_separator.is_some() {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        } else if response.hovered() {
            ctx.set_cursor_icon(CursorIcon::Text);
        }

        self.paint_rows(ui, &painter, geometry, look);
        self.paint_frames_and_arrows(ui, &ctx, &painter, geometry, look);
        self.paint_overview(ui, &painter, geometry, look);
        if self.filter_counts().is_some_and(|(kept, _)| kept == 0) {
            painter.text(
                view.center(),
                Align2::CENTER_CENTER,
                "Aucun bloc ne contient ce filtre",
                FontId::proportional(14.0),
                chrome.text_dim,
            );
        }
        if self.json.as_ref().is_some_and(|json| json.pending.is_some()) {
            ctx.set_cursor_icon(CursorIcon::Progress);
        }
        panes::finish_cards(&painter, look, &cards);
        if self.question.is_some() {
            self.show_question(&ctx, chrome);
        }
        if self.summary.is_some() {
            self.show_summary(&ctx, chrome, doc);
        }
    }

    fn show_question(&mut self, ctx: &egui::Context, chrome: &UiPalette) {
        let Some(question) = &self.question else {
            return;
        };
        let (title, lines, confirm) = match question {
            Question::Reproject { .. } => (
                "Réaligner le JSON ?".to_owned(),
                vec![
                    "Les deux côtés vont être réécrits dans l'ordre et la mise en forme de la comparaison.".to_owned(),
                    "Tes modifications sont gardées, leur mise en forme non ; Ctrl+Z revient en arrière.".to_owned(),
                ],
                "Réaligner",
            ),
            Question::Save { side, reformatted, invalid, .. } => {
                let mut lines = Vec::new();
                if *reformatted {
                    lines.push(
                        "Ce côté est montré réorganisé pour la comparaison : l'enregistrer écrit ce JSON réorganisé \
                         (ordre des clés, mise en forme), valeurs inchangées."
                            .to_owned(),
                    );
                }
                if let Some(error) = invalid {
                    lines.push(format!("Ce texte n'est pas du JSON valide : {error}"));
                }
                (format!("Enregistrer {} ?", self.side_name(*side)), lines, "Enregistrer quand même")
            }
        };
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::unique("text-tab-question")).frame(widgets::dialog_frame(chrome)).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(egui::RichText::new(&title).font(widgets::semibold(15.0)));
            ui.add_space(6.0);
            for line in &lines {
                ui.label(line);
            }
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(crate::folder_view::primary_button(chrome, confirm)).clicked() {
                    answer = Some(true);
                }
                if ui.button("Annuler").clicked() {
                    answer = Some(false);
                }
            });
        });
        if modal.should_close() {
            answer = answer.or(Some(false));
        }
        let Some(confirmed) = answer else {
            return;
        };
        let Some(question) = self.question.take() else {
            return;
        };
        if !confirmed {
            return;
        }
        match question {
            Question::Reproject { options } => self.start_projection(ctx, options),
            Question::Save { side, reformatted, all, .. } => {
                if let (true, Some(json)) = (reformatted, &mut self.json) {
                    json.save_confirmed[side.index()] = true;
                }
                match self.write_side(side) {
                    Ok(()) if all => {
                        if let Err(error) = self.save() {
                            self.message = Some(error);
                        }
                    }
                    Ok(()) => self.message = None,
                    Err(error) => self.message = Some(error),
                }
            }
        }
    }

    /// The "Changements JSON" dialog: each difference of value, a click going to its line.
    fn show_summary(&mut self, ctx: &egui::Context, chrome: &UiPalette, doc: &DocPalette) {
        let Some(summary) = &self.summary else {
            return;
        };
        let mut jump: Option<(Side, String)> = None;
        let mut close = false;
        let id = egui::Id::unique("json-summary");
        let modal = egui::Modal::new(id).frame(widgets::dialog_frame(chrome)).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.label(egui::RichText::new("Changements JSON").font(widgets::semibold(16.0)));
            ui.add_space(4.0);
            match &summary.outcome {
                None => {
                    ui.label(egui::RichText::new("Comparaison des valeurs…").color(chrome.text_dim));
                }
                Some(Err(error)) => {
                    ui.label(egui::RichText::new(error).color(chrome.error));
                }
                Some(Ok(found)) if found.changes.is_empty() => {
                    ui.label(egui::RichText::new("Aucune différence de valeur : seuls l'ordre ou l'écriture diffèrent.").color(chrome.text_dim));
                }
                Some(Ok(found)) => {
                    let count = found.changes.len();
                    let head = if found.truncated {
                        format!("{count} premiers changements (liste tronquée)")
                    } else if count == 1 {
                        "1 changement".to_owned()
                    } else {
                        format!("{count} changements")
                    };
                    ui.label(egui::RichText::new(head).color(chrome.text_dim).size(12.0));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, true]).show_rows(
                        ui,
                        SUMMARY_ROW,
                        count,
                        |ui, rows| {
                            for change in &found.changes[rows] {
                                if summary_row(ui, chrome, doc, change).clicked() {
                                    jump = match (&change.left_path, &change.right_path) {
                                        (Some(path), _) => Some((Side::Left, path.clone())),
                                        (None, Some(path)) => Some((Side::Right, path.clone())),
                                        (None, None) => None,
                                    };
                                }
                            }
                        },
                    );
                }
            }
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(crate::folder_view::primary_button(chrome, "Fermer")).clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.summary = None;
        }
        if let Some((side, path)) = jump {
            if self.jump_to_path(side, &path) {
                self.summary = None;
            } else {
                self.message = Some(format!("{path} introuvable dans le texte"));
            }
        }
    }

    fn change_path(&mut self, ctx: &egui::Context, side: usize, path: PathBuf) {
        if path == self.paths[side] {
            return;
        }
        if self.is_dirty() {
            self.inputs[side] = self.paths[side].display().to_string();
            self.message = Some("Enregistre ou annule tes modifications d'abord".to_owned());
        } else {
            self.inputs[side] = path.display().to_string();
            // Another file is an ordinary file, whatever the side showed before, unless it is a
            // document whose text is compared.
            self.sources[side] = SideSource {
                extract: extract::is_supported(&path).then_some(self.extract_options),
                ..SideSource::default()
            };
            self.paths[side] = path;
            if self.sources.iter().any(|source| source.extract.is_some()) {
                self.own_wrap = true;
                self.set_wrap(true);
            }
            let (options, wanted) = match &self.json {
                Some(json) => (json.options.clone(), json.wanted),
                None => self.json_defaults.clone(),
            };
            self.json = json_format(&self.paths, &self.sources).map(|format| JsonState::new(format, options, wanted));
            self.reload(ctx);
        }
    }

    fn pane_headers(&mut self, ui: &mut Ui, ctx: &egui::Context, look: &Look, cards: [panes::PaneRects; 2]) {
        let chrome = &look.ui;
        for side in 0..2 {
            let header = cards[side].header;
            let which = if side == 0 { Side::Left } else { Side::Right };
            let badges = self.badges(which);
            let dirty = self.side_dirty(which);
            let read_only = self.is_read_only(which);
            let tint = match (read_only, which) {
                (true, _) => chrome.hues.slate,
                (false, Side::Left) => chrome.hues.violet,
                (false, Side::Right) => chrome.hues.green,
            };
            // A comparison with HEAD: the other side is the working copy.
            let head_label = |index: usize| self.sources[index].label.as_deref().is_some_and(|label| label.ends_with("(HEAD)"));
            let working = (self.sources[side].label.is_none() && head_label(1 - side)).then(|| {
                let name = self.paths[side].file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                format!("{name} (copie de travail)")
            });
            let title = self.sources[side].label.clone().or(working);
            let action = widgets::path_bar(
                ui,
                look,
                ui.scope_id().with(("header", side)),
                header,
                PathBar {
                    path: &self.paths[side],
                    input: &mut self.inputs[side],
                    icon: Icon::File,
                    tint,
                    dirty,
                    label: ["Chemin de gauche", "Chemin de droite"][side],
                    browse_tip: "Choisir un autre fichier",
                    title: title.as_deref(),
                    read_only,
                    badges: &badges,
                    fixed: false,
                },
            );
            match action {
                PathAction::None => {}
                PathAction::Commit(path) => self.change_path(ctx, side, path),
                PathAction::Browse => {
                    let sender = self.sender.clone();
                    widgets::pick_path(ctx, false, move |path| {
                        let _ = sender.send(Message::Picked(side, path));
                    });
                }
                PathAction::Save => self.save_side(which),
            }
        }
        let center = pos2(
            (cards[0].card.right() + cards[1].card.left()) / 2.0,
            cards[0].header.center().y,
        );
        let id = ui.scope_id().with("swap");
        let rect = icons::square(center, SWAP_BUTTON);
        if widgets::icon_button(ui, chrome, rect, id, Icon::Swap, "Échanger les deux côtés", chrome.text_dim).clicked() {
            if self.is_dirty() {
                self.message = Some("Enregistre ou annule tes modifications d'abord".to_owned());
            } else {
                self.paths.swap(0, 1);
                self.inputs.swap(0, 1);
                self.sources.swap(0, 1);
                self.reload(ctx);
            }
        }
    }

    /// What the header of a side says about it besides its name.
    fn badges(&self, side: Side) -> Vec<Badge> {
        let index = side.index();
        let mut badges = Vec::new();
        if let Some(document) = &self.documents[index] {
            let mut tip = vec!["Texte extrait du document : la mise en forme n'est pas comparée".to_owned()];
            tip.extend(document.facts.notices(&self.extract_options));
            badges.push(Badge::new(document.format.label(), tip.join("\n")));
        }
        if let Some(json) = &self.json {
            if let Some(error) = &json.errors[index] {
                badges.push(Badge::new("JSON invalide", format!("Comparé en texte simple : {error}")).error());
            } else if self.altered(side) && !self.is_read_only(side) {
                let tip = "Ce côté est montré réorganisé pour la comparaison (ordre des clés, mise en forme).\n\
                           L'enregistrer écrit ce JSON réorganisé, valeurs inchangées ; Ctrl+Z revient au texte du fichier.";
                badges.push(Badge::new("reformaté", tip).warning());
            }
        }
        badges
    }

    fn save_side(&mut self, side: Side) {
        if let Err(error) = self.check_saveable(side) {
            self.message = Some(error);
            return;
        }
        if let Some(question) = self.save_question(side, false) {
            self.question = Some(question);
            return;
        }
        self.message = self.write_side(side).err();
    }

    fn handle_scroll(&mut self, ui: &Ui, response: &egui::Response, geometry: Geometry) {
        let line = geometry.line_height as f64;
        let page = (geometry.view.height() as f64 - line * 2.0).max(line);
        let max_y = self.layout.sync.max_scroll((geometry.view.height() as f64) / line) * line;
        let pane = geometry.panes[0];
        let width = self.layout.max_columns as f32 * geometry.char_width;
        let max_x = if self.layout.wrap {
            0.0
        } else {
            (width - (pane.text_right - pane.text_x)).max(0.0)
        };
        self.scroller.update(
            ui,
            response.hovered() || ui.rect_contains_pointer(geometry.overview),
            Position {
                y: &mut self.scroll,
                x: &mut self.scroll_x,
                max_y,
                max_x,
                row_height: geometry.line_height,
            },
        );
        // With a caret, these keys move it instead.
        if !ui.ctx().egui_wants_keyboard_input() && self.caret.is_none() {
            ui.input(|i| {
                if i.key_pressed(Key::ArrowDown) {
                    self.scroll += line;
                }
                if i.key_pressed(Key::ArrowUp) {
                    self.scroll -= line;
                }
                if i.key_pressed(Key::ArrowRight) {
                    self.scroll_x = (self.scroll_x + geometry.char_width * 4.0).min(max_x);
                }
                if i.key_pressed(Key::ArrowLeft) {
                    self.scroll_x = (self.scroll_x - geometry.char_width * 4.0).max(0.0);
                }
                if i.key_pressed(Key::PageDown) {
                    self.scroll += page;
                }
                if i.key_pressed(Key::PageUp) {
                    self.scroll -= page;
                }
                if i.modifiers.ctrl && i.key_pressed(Key::Home) {
                    self.scroll = 0.0;
                }
                if i.modifiers.ctrl && i.key_pressed(Key::End) {
                    self.scroll = f64::MAX;
                }
            });
        }
        self.scroll = self.scroll.clamp(0.0, max_y);
    }

    fn place_caret(&mut self, pos: Pos2, extend: bool) {
        let Some((side, text_pos)) = self.hit_test(pos) else {
            return;
        };
        self.goal_column = None;
        self.set_caret(side, text_pos, extend);
        self.adopt_block_at(side, text_pos.line);
    }

    fn select_word(&mut self, pos: Pos2) {
        let (Some((side, text_pos)), Some(docs)) = (self.hit_test(pos), &self.docs) else {
            return;
        };
        let (start, end) = textedit::word_at(&docs[side.index()], text_pos);
        self.goal_column = None;
        self.caret = Some((side, end));
        self.selection = Some(Selection { side, anchor: start, head: end });
    }

    fn extend_selection(&mut self, pos: Pos2) {
        let Some(selection) = self.selection else {
            return;
        };
        if let Some((side, head)) = self.hit_test(pos) {
            if side == selection.side {
                self.selection = Some(Selection { head, ..selection });
                self.caret = Some((side, head));
                self.caret_to_reveal = true;
            }
        }
    }

    fn paint_rows(&self, ui: &Ui, painter: &Painter, geometry: Geometry, look: &Look) {
        let Some(docs) = &self.docs else {
            return;
        };
        let doc = &look.doc;
        let view = geometry.view;
        let line_height = geometry.line_height;
        let number_font = FontId::new(self.code_size - 1.0, FontFamily::Monospace);
        let code_font = FontId::new(self.code_size, FontFamily::Monospace);
        let above = (view.top() - geometry.rows.top()) as f64;
        let below = (geometry.rows.bottom() - view.bottom()) as f64;
        // The first look paints whole inserted or removed lines darker; the newer ones leave the
        // zone and its bar to say it, as their mockups do.
        let classic = look.ui.chrome == Chrome::Classic;

        for (index, pane) in geometry.panes.iter().enumerate() {
            let side = if index == 0 { Side::Left } else { Side::Right };
            // Lines run to the edges of the code area, past the padding of a resting first line.
            let clip_rect = Rect::from_x_y_ranges(pane.rect.x_range(), geometry.rows.y_range());
            let clip = painter.with_clip_rect(clip_rect);
            let text_clip = painter.with_clip_rect(Rect::from_min_max(
                pos2(pane.text_x - TEXT_PADDING, clip_rect.top()),
                pos2(pane.rect.right(), clip_rect.bottom()),
            ));
            let scroll = self.pane_scroll[index];
            let offsets = &self.layout.offsets[index];
            let first = ((scroll - above).max(0.0) / line_height as f64) as u32;
            let last = ((scroll + view.height() as f64 + below) / line_height as f64) as u32 + 1;

            for line in line_at(offsets, first)..docs[index].len() {
                let start = offsets[line];
                if start >= last {
                    break;
                }
                let fold = self.layout.folds[index][line];
                if fold == Fold::Hidden {
                    continue;
                }
                let extent = self.line_extent(index, line);
                let top = snap(painter, view.top() as f64 + start as f64 * line_height as f64 - scroll);
                let bottom = top + extent.len() as f32 * line_height;
                if let Fold::Separator(rows) = fold {
                    let hovered = self.hovered_separator == Some((index, line));
                    paint_separator(&clip, pane.rect, top, line_height, rows, hovered, doc);
                    continue;
                }
                let row_index = self.layout.line_rows[index][line] as usize;
                let row = &self.diff.rows[row_index];
                let ignored_row = self.diff.is_row_ignored(row_index);
                let current = row.block.is_some() && row.block.map(|b| b as usize) == self.current_block;
                // A gap between two lines of one block keeps the colour of what fills it across.
                if let Some(block) = row.block {
                    let block = &self.diff.blocks[block as usize];
                    let lines = if index == 0 { &block.left } else { &block.right };
                    let gap = offsets[line + 1] - extent.end;
                    if gap > 0 && (line as u32 + 1) < lines.end {
                        let tint = if index == 0 { doc.insert.zone } else { doc.delete.zone };
                        let hole = Rect::from_x_y_ranges(
                            pane.rect.x_range(),
                            bottom..=bottom + gap as f32 * line_height,
                        );
                        clip.rect_filled(hole, 0.0, mix(Color32::TRANSPARENT, tint, 0.5));
                    }
                }
                let rect = Rect::from_x_y_ranges(pane.rect.x_range(), top..=bottom);
                let gutter = Rect::from_x_y_ranges(
                    pane.rect.left()..=pane.text_x - TEXT_PADDING,
                    top..=bottom,
                );
                let tint = row_tint(row, doc).or(ignored_row.then_some(&doc.ignored));
                if let Some(tint) = tint {
                    clip.rect_filled(rect, 0.0, if current { tint.current } else { tint.zone });
                    if tint.gutter != tint.zone {
                        clip.rect_filled(gutter, 0.0, tint.gutter);
                    }
                    let bar_x = panes::line_bar_x(look, pane.rect, gutter.right() - GUTTER_PADDING);
                    panes::paint_line_bar(&clip, look, bar_x, top, bottom, tint.mark);
                }
                // What the rules leave out is marked where it matters: in a difference, or where
                // it hides one. Unchanged text matching a rule stays plain.
                let text = &docs[index].lines[line].text;
                let ignored = if (row.block.is_some() || ignored_row) && !self.rules.is_empty() {
                    self.rules.ignored_ranges(text)
                } else {
                    Vec::new()
                };
                clip.text(
                    pos2(gutter.right() - GUTTER_PADDING, top + line_height / 2.0),
                    Align2::RIGHT_CENTER,
                    line + 1,
                    number_font.clone(),
                    if tint.is_some() { doc.line_number_on } else { doc.line_number },
                );
                let spans = row.pair.and_then(|p| self.diff.spans.get(p as usize));
                let whole = match (row.left, row.right, row.block) {
                    (Some(_), None, Some(_)) if classic => Some(doc.delete.text),
                    (None, Some(_), Some(_)) if classic => Some(doc.insert.text),
                    _ => None,
                };
                self.paint_line(
                    ui,
                    &text_clip,
                    *pane,
                    top,
                    side,
                    &docs[index].lines[line],
                    line as u32,
                    spans.map_or(&[], |s| side_of(s, side)),
                    &ignored,
                    whole,
                    geometry,
                    &code_font,
                    doc,
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_line(
        &self,
        ui: &Ui,
        painter: &Painter,
        pane: Pane,
        top: f32,
        side: Side,
        line: &Line,
        line_index: u32,
        spans: &[ChangeSpan],
        ignored: &[Range<usize>],
        whole: Option<Color32>,
        geometry: Geometry,
        font: &FontId,
        doc: &DocPalette,
    ) {
        let text = line.text.as_str();
        let styles = self.styles[side.index()]
            .get(line_index as usize)
            .and_then(Option::as_ref)
            .map_or(&[][..], |s| &s[..]);
        let selected = self
            .selection
            .and_then(|s| s.bytes_on(side, line_index, text.len()));
        let found = self.filter.as_ref().map(|filter| filter.find_ranges(text)).unwrap_or_default();
        let caret = self.caret.filter(|&(caret_side, pos)| caret_side == side && pos.line == line_index).map(|(_, pos)| pos.byte);
        paint_text_line(
            ui,
            painter,
            &LinePaint {
                text,
                styles,
                spans,
                ignored,
                whole,
                selected,
                found: &found,
                caret,
                wrap: self.layout.wrap.then_some(self.layout.columns),
                scroll_x: self.scroll_x,
                text_x: pane.text_x,
                text_right: pane.text_right,
                visible_right: pane.rect.right(),
                top,
                line_height: geometry.line_height,
                char_width: geometry.char_width,
                font,
                doc,
            },
        );
    }

    /// The frames or the ribbons of the blocks in view, the links between them, and the arrows
    /// of the block being worked on, or of the one under the pointer.
    fn paint_frames_and_arrows(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        painter: &Painter,
        geometry: Geometry,
        look: &Look,
    ) {
        let doc = &look.doc;
        let view = geometry.view;
        let line_height = geometry.line_height as f64;
        let clip = painter.with_clip_rect(geometry.rows);
        let bottom_edge = clip.clip_rect().bottom();
        let pane_scroll = self.pane_scroll;
        let offsets = &self.layout.offsets;
        let y_at = |side: usize, visual: u32| {
            snap(painter, view.top() as f64 + visual as f64 * line_height - pane_scroll[side])
        };
        let y_of = |side: usize, line: u32| y_at(side, offsets[side][line as usize]);
        let first = self.diff.blocks.partition_point(|block| {
            y_of(0, block.left.end).max(y_of(1, block.right.end)) < geometry.rows.top()
        });
        let ribbons = look.links == LinkStyle::Ribbons;
        let pointer = ui.ctx().pointer_hover_pos().filter(|_| ui.rect_contains_pointer(geometry.rows));
        let mut pushed = None;
        let mut arrows = Vec::new();
        let read_only = [Side::Left, Side::Right].map(|side| self.is_read_only(side));
        for block_index in first..self.diff.blocks.len() {
            if !self.block_kept(block_index) {
                continue;
            }
            let block = &self.diff.blocks[block_index];
            let ranges = [&block.left, &block.right];
            if y_of(0, block.left.start) > bottom_edge && y_of(1, block.right.start) > bottom_edge {
                break;
            }
            let current = self.current_block == Some(block_index);
            let tint = block_tint(block.kind, doc);
            let stroke = if current {
                Stroke::new(1.5, tint.mark)
            } else {
                Stroke::new(1.0, doc.frame)
            };
            let mut anchors = [0.0; 2];
            let mut spans = [(0.0, 0.0); 2];
            let mut hovered = false;
            for side in 0..2 {
                let pane = geometry.panes[side];
                let frame_top = y_of(side, ranges[side].start);
                if ranges[side].is_empty() {
                    // No line on this side: the block is a rule between two lines.
                    let visual = match self.layout.row_offsets.get(block.rows.start as usize) {
                        Some(&row_top) => y_at(side, row_top),
                        None => frame_top,
                    };
                    let y = clip.round_to_pixel_center(visual);
                    let rule = if ribbons { Stroke::new(if current { 1.5 } else { 1.0 }, tint.mark) } else { stroke };
                    clip.hline(pane.rect.x_range(), y, rule);
                    anchors[side] = y;
                    spans[side] = (y, y);
                    hovered |= pointer.is_some_and(|p| pane.rect.x_range().contains(p.x) && (p.y - y).abs() <= 4.0);
                } else {
                    let last = ranges[side].end as usize - 1;
                    let frame_bottom = y_at(side, self.line_extent(side, last).end);
                    let rect = Rect::from_x_y_ranges(pane.rect.x_range(), frame_top..=frame_bottom);
                    if !ribbons {
                        clip.rect_stroke(rect, 0.0, stroke, StrokeKind::Inside);
                    }
                    // The middle of what can be seen of the frame, so that a link stays in view
                    // along a block taller than the window.
                    let (low, high) = (frame_top.max(view.top()), frame_bottom.min(view.bottom()));
                    anchors[side] = if low < high { (low + high) / 2.0 } else { (frame_top + frame_bottom) / 2.0 };
                    spans[side] = (frame_top, frame_bottom);
                    hovered |= pointer.is_some_and(|p| rect.contains(p));
                }
            }
            if ribbons {
                let mark = block_tint(block.kind, &look.gap).mark;
                panes::paint_ribbon(&clip, geometry.gap, spans[0], spans[1], mark, look.ui.ribbon, current);
            } else {
                let link = if current { stroke } else { Stroke::new(1.0, look.gap.frame) };
                paint_link(&clip, pos2(geometry.gap.0, anchors[0]), pos2(geometry.gap.1, anchors[1]), link);
            }
            hovered |= pointer.is_some_and(|p| panes::ribbon_contains(geometry.gap, spans[0], spans[1], p));
            let rects = [
                (0, Side::Right, panes::arrow_rect(look, geometry.gap, true, anchors[0])),
                (1, Side::Left, panes::arrow_rect(look, geometry.gap, false, anchors[1])),
            ];
            hovered |= pointer.is_some_and(|p| rects.iter().any(|(_, _, rect)| rect.contains(p)));
            // Arrows for the block being worked on, and for the one under the pointer: the
            // others stay quiet.
            if !(current || hovered) {
                continue;
            }
            for (side, target, rect) in rects {
                // Nothing may be sent into a read-only side: no arrow offers it.
                if read_only[target.index()] {
                    continue;
                }
                if rect.bottom() > view.bottom() || rect.top() < view.top() {
                    continue;
                }
                let tip = if target == Side::Right { "Envoyer ce bloc à droite" } else { "Envoyer ce bloc à gauche" };
                let id = ui.scope_id().with(("arrow", block_index, side));
                let response = panes::arrow_button(ui, &clip, look, rect, id, target == Side::Left, tip).on_hover_text(tip);
                arrows.push((block_index, target, rect));
                if response.clicked() {
                    pushed = Some((block_index, target));
                }
            }
        }
        self.arrow_rects = arrows;
        if let Some((block, target)) = pushed {
            self.push_block(ctx, block, target);
        }
    }

    fn paint_overview(&mut self, ui: &mut Ui, painter: &Painter, geometry: Geometry, look: &Look) {
        let rect = geometry.overview;
        let total = (self.layout.sync.total() as f32).max(1.0);
        let scale = rect.height() / total;
        let marks: Vec<(f32, f32, Color32)> = self
            .diff
            .blocks
            .iter()
            .zip(&self.layout.sync.blocks)
            .filter(|(_, extent)| !extent.is_empty())
            .map(|(block, extent)| {
                let top = rect.top() + extent.start as f32 * scale;
                let bottom = rect.top() + extent.end as f32 * scale;
                (top, bottom, block_tint(block.kind, &look.gap).mark)
            })
            .collect();
        let line = geometry.line_height;
        let thumb_top = rect.top() + (self.scroll as f32 / line) * scale;
        let thumb_height = (geometry.view.height() / line * scale).max(14.0);
        let thumb = Rect::from_min_size(
            pos2(rect.left(), thumb_top),
            vec2(rect.width(), thumb_height.min(rect.height())),
        );
        panes::paint_overview(painter, look, rect, &marks, thumb);
        let response = ui.interact(rect, ui.scope_id().with("overview"), Sense::click_and_drag());
        if let Some(pos) = response.interact_pointer_pos() {
            let ratio = ((pos.y - rect.top() - thumb_height / 2.0) / rect.height()).max(0.0);
            let max = self.layout.sync.max_scroll((geometry.view.height() / line) as f64) * line as f64;
            self.scroll = (ratio as f64 * total as f64 * line as f64).min(max);
        }
    }

    /// The block being worked on, counted from one, and how many there are.
    pub fn block_rank(&self) -> Option<(usize, usize)> {
        let current = self.current_block?;
        Some((current + 1, self.diff.blocks.len()))
    }
}

/// One change of the JSON summary: a dot of its kind, its path, then what it was and became.
fn summary_row(ui: &mut Ui, chrome: &UiPalette, doc: &DocPalette, change: &json::Change) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), SUMMARY_ROW), Sense::click());
    let path = change.path();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, path));
    if response.hovered() {
        ui.painter().rect_filled(rect, 6.0, chrome.hover);
    }
    let (color, kind) = match change.kind {
        json::ChangeKind::Added => (doc.insert.mark, "ajouté"),
        json::ChangeKind::Removed => (doc.delete.mark, "retiré"),
        json::ChangeKind::Changed => (doc.change.mark, "modifié"),
        json::ChangeKind::Moved => (doc.frame, "déplacé"),
    };
    let y = rect.center().y;
    let painter = ui.painter().with_clip_rect(rect);
    painter.circle_filled(pos2(rect.left() + 12.0, y), 3.5, color);
    painter.text(pos2(rect.left() + 24.0, y), Align2::LEFT_CENTER, kind, FontId::proportional(12.0), chrome.text_dim);
    let path_end = painter
        .text(pos2(rect.left() + 92.0, y), Align2::LEFT_CENTER, path, FontId::monospace(12.5), chrome.text)
        .right();
    let values = match (&change.left, &change.right) {
        (Some(left), Some(right)) => format!("{left} → {right}"),
        (Some(left), None) => left.clone(),
        (None, Some(right)) => right.clone(),
        (None, None) => change.right_path.clone().map_or_else(String::new, |to| format!("→ {to}")),
    };
    let x = (path_end + 16.0).max(rect.left() + 340.0);
    painter.text(pos2(x, y), Align2::LEFT_CENTER, values, FontId::monospace(12.0), chrome.text_dim);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// On a whole physical pixel, so that text and frames stay crisp at any scroll position.
pub(crate) fn snap(painter: &Painter, y: f64) -> f32 {
    let scale = painter.pixels_per_point() as f64;
    ((y * scale).round() / scale) as f32
}

/// Which rows show. The filter decides which blocks stay; the context, how much unchanged text
/// stays around them: `None` shows it all unless a filter is in force, which hides it all. The
/// row being edited always shows, and so does what was unfolded by hand. Empty: nothing folds.
fn visible_rows(
    rows: &[Row],
    kept: &[bool],
    context: Option<u32>,
    pinned: Option<u32>,
    unfolded: &[Range<u32>],
) -> Vec<bool> {
    if kept.is_empty() && context.is_none() {
        return Vec::new();
    }
    let in_kept_block = |row: &Row| {
        row.block.is_some_and(|block| kept.get(block as usize).copied().unwrap_or(true))
    };
    let mut shown: Vec<bool> = rows.iter().map(in_kept_block).collect();
    if let Some(shown) = pinned.and_then(|row| shown.get_mut(row as usize)) {
        *shown = true;
    }
    if let Some(context) = context.filter(|&context| context > 0) {
        // Context is unchanged text only: it stops at a block the filter hides.
        let seeds = shown.clone();
        let mut spread = |indices: &mut dyn Iterator<Item = usize>| {
            let mut budget = 0;
            for index in indices {
                if seeds[index] {
                    budget = context;
                } else if rows[index].block.is_none() && budget > 0 {
                    shown[index] = true;
                    budget -= 1;
                } else {
                    budget = 0;
                }
            }
        };
        spread(&mut (0..rows.len()));
        spread(&mut (0..rows.len()).rev());
    }
    for row in unfolded.iter().flat_map(|range| range.clone()) {
        if let Some(shown) = shown.get_mut(row as usize) {
            *shown = true;
        }
    }
    shown
}

/// The folds of each line. Each hidden run leaves one line on each side, which becomes its
/// separator.
fn fold_lines(rows: &[Row], shown: &[bool], lines: [usize; 2]) -> [Vec<Fold>; 2] {
    if shown.is_empty() {
        return lines.map(|count| vec![Fold::Shown; count]);
    }
    let mut folds = lines.map(|count| vec![Fold::Hidden; count]);
    let mut run = 0;
    while run < rows.len() {
        let shown = |index: usize| shown[index];
        if shown(run) {
            if let Some(line) = rows[run].left {
                folds[0][line as usize] = Fold::Shown;
            }
            if let Some(line) = rows[run].right {
                folds[1][line as usize] = Fold::Shown;
            }
            run += 1;
            continue;
        }
        let end = (run..rows.len()).find(|&index| shown(index)).unwrap_or(rows.len());
        let hidden = (end - run) as u32;
        if let Some(line) = rows[run..end].iter().find_map(|row| row.left) {
            folds[0][line as usize] = Fold::Separator(hidden);
        }
        if let Some(line) = rows[run..end].iter().find_map(|row| row.right) {
            folds[1][line as usize] = Fold::Separator(hidden);
        }
        run = end;
    }
    folds
}

/// A click shows the run again: under the pointer, the label says so by lighting up.
fn paint_separator(painter: &Painter, pane: Rect, top: f32, height: f32, rows: u32, hovered: bool, doc: &DocPalette) {
    let y = top + height / 2.0;
    let text = format!("{rows} ligne{} masquée{}", plural(rows), plural(rows));
    let center = pos2(pane.center().x, y);
    let color = if hovered { doc.text } else { doc.line_number };
    let label = painter.text(center, Align2::CENTER_CENTER, text, FontId::proportional(11.5), color);
    let stroke = Stroke::new(1.0, color.gamma_multiply(0.35));
    let rule = painter.round_to_pixel_center(y);
    painter.hline(pane.left() + 14.0..=label.left() - 12.0, rule, stroke);
    painter.hline(label.right() + 12.0..=pane.right() - 14.0, rule, stroke);
}

fn plural(count: u32) -> &'static str {
    if count > 1 { "s" } else { "" }
}

fn block_tint(kind: BlockKind, doc: &DocPalette) -> Tint {
    match kind {
        BlockKind::Insert => doc.insert,
        BlockKind::Delete => doc.delete,
        BlockKind::Change => doc.change,
    }
}

/// The thin link between two frames: leaves and reaches each frame horizontally, the two curves
/// joining along the slanted middle. Frames at the same level get a straight line.
pub(crate) fn paint_link(painter: &Painter, from: Pos2, to: Pos2, stroke: Stroke) {
    if (from.y - to.y).abs() < 0.5 {
        painter.hline(from.x..=to.x, painter.round_to_pixel_center(from.y), stroke);
        return;
    }
    let reach = (to.x - from.x) * LINK_CURVE;
    painter.add(CubicBezierShape::from_points_stroke(
        [from, from + vec2(reach, 0.0), to - vec2(reach, 0.0), to],
        false,
        Color32::TRANSPARENT,
        stroke,
    ));
}

impl Drop for TextTab {
    fn drop(&mut self) {
        for cancel in &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// The line comparison, or under the structural JSON comparison the one that sees neither
/// indentation nor final commas.
fn compute_diff(left: &Document, right: &Document, options: DiffOptions, rules: &IgnoreRules, json: bool) -> DiffResult {
    fn text(doc: &Document) -> Vec<&str> {
        doc.lines.iter().map(|l| l.text.as_str()).collect()
    }
    if json {
        json::diff_json_lines_with(&text(left), &text(right), options, rules)
    } else {
        diff_lines_with(&text(left), &text(right), options, rules)
    }
}

/// The JSON dialect of a pair of files, when they are JSON. Documents are never.
fn json_format(paths: &[PathBuf; 2], sources: &[SideSource; 2]) -> Option<Format> {
    if sources.iter().any(|source| source.extract.is_some()) {
        return None;
    }
    Format::from_path(&paths[0]).or_else(|| Format::from_path(&paths[1]))
}

impl JsonState {
    fn new(format: Format, options: JsonOptions, wanted: bool) -> Self {
        Self {
            format,
            options,
            wanted,
            structural: wanted,
            errors: [None, None],
            paths: [None, None],
            scanning: [false, false],
            save_confirmed: [false, false],
            pending: None,
            next_job: 1,
        }
    }

    /// The files are read again: what was known of the old texts goes.
    fn reset(&mut self) {
        self.structural = self.wanted;
        self.errors = [None, None];
        self.paths = [None, None];
        self.scanning = [false, false];
        self.save_confirmed = [false, false];
        self.pending = None;
    }
}

/// Parses both panes (or the given source texts) and replaces each pane's text by its
/// projection, on a worker. Each edit turns the pane's text into the new one, so that a single
/// undo step gives it back.
fn project_documents(
    mut docs: [Document; 2],
    sources: [Option<Arc<Vec<Line>>>; 2],
    format: Format,
    json_options: &JsonOptions,
    options: DiffOptions,
    rules: &IgnoreRules,
) -> Result<Projected, [Option<String>; 2]> {
    let parse = |index: usize| {
        let lines = sources[index].as_ref().unwrap_or(&docs[index].lines);
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        JsonDoc::parse_lines(&texts, format)
    };
    let (left, right) = rayon::join(|| parse(0), || parse(1));
    let (left, right) = match (left, right) {
        (Ok(left), Ok(right)) => (left, right),
        (left, right) => {
            return Err([left.err(), right.err()].map(|error| error.map(|e| formats::describe_json_error(&e))));
        }
    };
    let projections = json::project(&left, &right, json_options);
    drop((left, right));
    let mut edits = [None, None];
    let mut alters = [false, false];
    for (index, projection) in projections.into_iter().enumerate() {
        let doc = &mut docs[index];
        let same = doc.lines.len() == projection.lines.len()
            && doc.lines.iter().zip(&projection.lines).all(|(line, projected)| line.text == *projected);
        if !same {
            alters[index] = projection.alters_file();
            let len = doc.len();
            edits[index] = Some(doc.replace_lines(0..len, projection.lines));
        }
    }
    let diff = compute_diff(&docs[0], &docs[1], options, rules, true);
    Ok(Projected { docs: Box::new(docs), edits, alters, diff })
}

/// True for an unchanged row whose two lines are the same text: what is found there is found
/// once, not once per side.
fn shared_row(row: &Row, docs: &[Document; 2]) -> bool {
    match (row.block, row.left, row.right) {
        (None, Some(left), Some(right)) => {
            let text = |side: usize, line: u32| docs[side].lines.get(line as usize).map(|l| l.text.as_str());
            text(0, left) == text(1, right)
        }
        _ => false,
    }
}

pub(crate) fn line_at(offsets: &[u32], visual: u32) -> usize {
    offsets.partition_point(|&offset| offset <= visual).saturating_sub(1)
}

fn side_of(spans: &LineSpans, side: Side) -> &[ChangeSpan] {
    match side {
        Side::Left => &spans.left,
        Side::Right => &spans.right,
    }
}

/// One frame can hold paired, inserted and removed lines: the zone colour belongs to the row.
fn row_tint<'a>(row: &Row, doc: &'a DocPalette) -> Option<&'a Tint> {
    row.block?;
    Some(match (row.left, row.right) {
        (Some(_), Some(_)) => &doc.change,
        (Some(_), None) => &doc.delete,
        _ => &doc.insert,
    })
}

pub(crate) fn span_tint(kind: SpanKind, doc: &DocPalette) -> &Tint {
    match kind {
        SpanKind::Insert => &doc.insert,
        SpanKind::Delete => &doc.delete,
        SpanKind::Replace => &doc.change,
    }
}

/// Where a key sends the caret, the vertical arrows aside (they depend on the layout): by
/// character, word, line, page or to an end of the document. `selected`: the ordered ends of a
/// selection, which plain left and right collapse to.
pub(crate) fn caret_motion(
    doc: &Document,
    key: Key,
    modifiers: Modifiers,
    head: TextPos,
    selected: Option<(TextPos, TextPos)>,
    page: u32,
) -> Option<TextPos> {
    let extend = modifiers.shift;
    match key {
        Key::ArrowLeft if !extend && selected.is_some() => selected.map(|(start, _)| start),
        Key::ArrowRight if !extend && selected.is_some() => selected.map(|(_, end)| end),
        Key::ArrowLeft if modifiers.alt => Some(textedit::word_left(doc, head)),
        Key::ArrowRight if modifiers.alt => Some(textedit::word_right(doc, head)),
        Key::ArrowLeft => Some(textedit::previous(doc, head)),
        Key::ArrowRight => Some(textedit::next(doc, head)),
        Key::PageUp => Some(textedit::clamp(doc, TextPos::new(head.line.saturating_sub(page), head.byte))),
        Key::PageDown => Some(textedit::clamp(doc, TextPos::new(head.line + page, head.byte))),
        Key::Home if modifiers.command => Some(TextPos::default()),
        Key::End if modifiers.command => Some(textedit::end_of_document(doc)),
        Key::Home => Some(textedit::line_start_smart(doc, head)),
        Key::End => Some(textedit::line_end(doc, head)),
        _ => None,
    }
}

/// What Backspace or Delete removes from the caret, a word at a time with Alt or Ctrl.
pub(crate) fn deleted_range(doc: &Document, key: Key, modifiers: Modifiers, head: TextPos) -> (TextPos, TextPos) {
    match (key, modifiers.alt || modifiers.command) {
        (Key::Backspace, true) => (textedit::word_left(doc, head), head),
        (Key::Backspace, false) => (textedit::previous(doc, head), head),
        (_, true) => (head, textedit::word_right(doc, head)),
        (_, false) => (head, textedit::next(doc, head)),
    }
}

/// The field of the band finds in any text tab; only a two-way comparison also filters blocks.
pub trait SearchTarget {
    fn query(&mut self) -> &mut String;
    fn options(&mut self) -> &mut FilterOptions;
    fn search_mode(&self) -> SearchMode;
    fn set_search_mode(&mut self, mode: SearchMode);
    fn can_filter(&self) -> bool;
    fn apply_search(&mut self);
    fn find_step(&mut self, forward: bool);
    fn filter_error(&self) -> Option<&str>;
    fn filter_counts(&self) -> Option<(usize, usize)>;
    fn find_counts(&self) -> Option<(Option<usize>, usize)>;
    fn selected_text(&self) -> Option<String>;
}

impl SearchTarget for TextTab {
    fn query(&mut self) -> &mut String {
        &mut self.filter_query
    }

    fn options(&mut self) -> &mut FilterOptions {
        &mut self.filter_options
    }

    fn search_mode(&self) -> SearchMode {
        self.search_mode
    }

    fn set_search_mode(&mut self, mode: SearchMode) {
        TextTab::set_search_mode(self, mode);
    }

    fn can_filter(&self) -> bool {
        true
    }

    fn apply_search(&mut self) {
        TextTab::apply_search(self);
    }

    fn find_step(&mut self, forward: bool) {
        TextTab::find_step(self, forward);
    }

    fn filter_error(&self) -> Option<&str> {
        TextTab::filter_error(self)
    }

    fn filter_counts(&self) -> Option<(usize, usize)> {
        TextTab::filter_counts(self)
    }

    fn find_counts(&self) -> Option<(Option<usize>, usize)> {
        TextTab::find_counts(self)
    }

    fn selected_text(&self) -> Option<String> {
        TextTab::selected_text(self)
    }
}

/// The small button of a block, in the colour of the block when hovered. `mirrored` turns the
/// icon to point left. `tip` names it for accessibility; the caller shows what it wants on hover.
#[allow(clippy::too_many_arguments)]
pub(crate) fn block_button(
    ui: &Ui,
    painter: &Painter,
    rect: Rect,
    id: egui::Id,
    icon: Icon,
    mirrored: bool,
    tip: &str,
    tint: Tint,
    doc: &DocPalette,
) -> egui::Response {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, tip));
    let hovered = response.hovered();
    let (fill, color) = if hovered { (tint.mark, crate::theme::over(doc.canvas, doc.backdrop)) } else { (doc.control, doc.control_text) };
    painter.rect(
        rect,
        CornerRadius::same(5),
        fill,
        Stroke::new(1.0, if hovered { tint.mark } else { doc.frame }),
        StrokeKind::Inside,
    );
    let glyph = icons::square(rect.center(), ARROW_ICON);
    if mirrored {
        icons::paint_mirrored(painter, icon, glyph, color);
    } else {
        icons::paint(painter, icon, glyph, color);
    }
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

/// What [`paint_text_line`] needs to draw one line of a pane.
pub(crate) struct LinePaint<'a> {
    pub(crate) text: &'a str,
    pub(crate) styles: &'a [StyleRun],
    pub(crate) spans: &'a [ChangeSpan],
    pub(crate) ignored: &'a [Range<usize>],
    pub(crate) whole: Option<Color32>,
    pub(crate) selected: Option<Range<usize>>,
    pub(crate) found: &'a [Range<usize>],
    /// The byte the caret stands before, when it is on this line.
    pub(crate) caret: Option<usize>,
    /// The columns a wrapped line breaks at; `None` scrolls sideways instead.
    pub(crate) wrap: Option<usize>,
    pub(crate) scroll_x: f32,
    pub(crate) text_x: f32,
    /// Where the wrap marks go.
    pub(crate) text_right: f32,
    /// The pane's right edge: unwrapped text is laid out up to there only.
    pub(crate) visible_right: f32,
    pub(crate) top: f32,
    pub(crate) line_height: f32,
    pub(crate) char_width: f32,
    pub(crate) font: &'a FontId,
    pub(crate) doc: &'a DocPalette,
}

/// One line of a pane, each of its visual lines with its syntax and change colours, the caret
/// when it blinks there, the bars of text missing on the other side and the wrap marks.
pub(crate) fn paint_text_line(ui: &Ui, painter: &Painter, line: &LinePaint<'_>) {
    let LinePaint { text, styles, spans, ignored, whole, found, char_width, line_height, doc, .. } = *line;
    let mut ranges = Vec::new();
    match line.wrap {
        Some(columns) => wrap::for_each_visual_line(text, columns, |range| ranges.push(range)),
        None => {
            // Only lay out what is visible: a minified line can weigh megabytes.
            let first = (line.scroll_x / char_width) as usize;
            let visible = ((line.visible_right - line.text_x) / char_width) as usize + 2;
            let start = wrap::offset_at_column(text, 0..text.len(), first);
            let end = wrap::offset_at_column(text, 0..text.len(), first + visible);
            ranges.push(start..end);
        }
    }
    let count = ranges.len();
    for (index, range) in ranges.into_iter().enumerate() {
        let y = line.top + index as f32 * line_height;
        let job = build_job(&JobInput {
            text,
            range: range.clone(),
            styles,
            spans,
            ignored,
            whole,
            selected: line.selected.clone(),
            found,
            font: line.font,
            line_height,
            doc,
        });
        let first_col = match line.wrap {
            Some(_) => 0.0,
            None => wrap::column_of(text, 0, range.start) as f32 * char_width - line.scroll_x,
        };
        let x = line.text_x + first_col;
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        painter.galley(pos2(x, y), galley, doc.text);

        if let Some(byte) = line.caret {
            let here = range.contains(&byte) || (byte == range.end && index + 1 == count);
            let time = ui.input(|input| input.time);
            if here && (time / CARET_BLINK_SECONDS) as u64 % 2 == 0 {
                let caret_x = x + wrap::column_of(text, range.start, byte) as f32 * char_width;
                painter.rect_filled(Rect::from_min_size(pos2(caret_x, y), vec2(CARET_WIDTH, line_height)), 0.0, doc.text);
            }
        }

        // Thin bar: text exists at this position on the other side only.
        for span in spans.iter().filter(|s| s.range.is_empty()) {
            let at = span.range.start as usize;
            let on_this_row = range.contains(&at) || (at == range.end && (index + 1 == count || range.is_empty()));
            if on_this_row {
                let bar_x = x + wrap::column_of(text, range.start, at) as f32 * char_width;
                painter.rect_filled(
                    Rect::from_min_size(pos2(bar_x - 1.0, y), vec2(2.0, line_height)),
                    0.0,
                    span_tint(span.kind, doc).mark,
                );
            }
        }
        if index + 1 < count {
            icons::paint(
                painter,
                Icon::Wrap,
                icons::square(pos2(line.text_right + 10.0, y + line_height / 2.0), WRAP_ICON),
                doc.line_number,
            );
        }
    }
}

pub(crate) struct JobInput<'a> {
    pub(crate) text: &'a str,
    pub(crate) range: Range<usize>,
    pub(crate) styles: &'a [StyleRun],
    pub(crate) spans: &'a [ChangeSpan],
    /// Byte ranges the ignore rules mask: shown, but faded.
    pub(crate) ignored: &'a [Range<usize>],
    pub(crate) whole: Option<Color32>,
    pub(crate) selected: Option<Range<usize>>,
    pub(crate) found: &'a [Range<usize>],
    pub(crate) font: &'a FontId,
    pub(crate) line_height: f32,
    pub(crate) doc: &'a DocPalette,
}

/// Builds one visual line: syntax colours in the foreground, change colours in the background.
pub(crate) fn build_job(input: &JobInput<'_>) -> LayoutJob {
    let JobInput {
        text,
        range,
        styles,
        spans,
        ignored,
        whole,
        selected,
        found,
        font,
        line_height,
        doc,
    } = input;
    let mut cuts = vec![range.start, range.end];
    // Colours can lag behind an edit by a frame or two, so their offsets may fall anywhere.
    let inside = |at: usize| at > range.start && at < range.end && text.is_char_boundary(at);
    for run in styles.iter() {
        cuts.extend([run.start as usize, run.end as usize].into_iter().filter(|&c| inside(c)));
    }
    for span in spans.iter() {
        cuts.extend(
            [span.range.start as usize, span.range.end as usize]
                .into_iter()
                .filter(|&c| inside(c)),
        );
    }
    for range in found.iter().chain(ignored.iter()) {
        cuts.extend([range.start, range.end].into_iter().filter(|&c| inside(c)));
    }
    if let Some(selected) = selected {
        cuts.extend([selected.start, selected.end].into_iter().filter(|&c| inside(c)));
    }
    cuts.sort_unstable();
    cuts.dedup();

    let mut job = LayoutJob {
        break_on_newline: false,
        ..LayoutJob::default()
    };
    job.wrap.max_width = f32::INFINITY;
    let mut column = wrap::display_width(&text[..range.start]);
    for piece in cuts.windows(2) {
        let (start, end) = (piece[0], piece[1]);
        let section_start = job.text.len();
        for c in text[start..end].chars() {
            let width = wrap::char_columns(c, column);
            match c {
                '\t' => job.text.extend(std::iter::repeat_n(' ', width)),
                c if c.is_control() => job.text.push('\u{FFFD}'),
                c => job.text.push(c),
            }
            column += width;
        }
        let at = start as u32;
        let run = styles.iter().find(|r| r.start <= at && at < r.end);
        let change = spans.iter().find(|s| s.range.contains(&at));
        let masked = ignored.iter().any(|range| range.contains(&start));
        let background = if selected.as_ref().is_some_and(|s| s.contains(&start)) {
            doc.selection
        } else if found.iter().any(|range| range.contains(&start)) {
            doc.found
        } else if let Some(change) = change {
            span_tint(change.kind, doc).text
        } else if masked {
            doc.ignored.text
        } else {
            whole.unwrap_or(Color32::TRANSPARENT)
        };
        let color = run.map_or(doc.text, |r| Color32::from_rgb(r.color[0], r.color[1], r.color[2]));
        job.sections.push(LayoutSection {
            leading_space: 0.0,
            byte_range: ByteIndex(section_start)..ByteIndex(job.text.len()),
            format: TextFormat {
                font_id: (*font).clone(),
                color: if masked { mix(color, background, 0.4) } else { color },
                background,
                italics: run.is_some_and(|r| r.italic),
                line_height: Some(*line_height),
                ..TextFormat::default()
            },
        });
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{CodeTone, LinkStyle, Look, ThemeKind};

    fn light() -> crate::theme::DocPalette {
        Look::new(ThemeKind::Clair, CodeTone::Theme, false, LinkStyle::Ribbons).doc
    }

    fn job(text: &str, range: Range<usize>, spans: &[ChangeSpan]) -> LayoutJob {
        let doc = light();
        let font = FontId::monospace(14.0);
        build_job(&JobInput {
            text,
            range,
            styles: &[],
            spans,
            ignored: &[],
            whole: None,
            selected: None,
            found: &[],
            font: &font,
            line_height: 21.0,
            doc: &doc,
        })
    }

    #[test]
    fn tabs_expand_from_the_logical_column_even_after_a_wrap() {
        assert_eq!(job("\tab", 0..3, &[]).text, "    ab");
        assert_eq!(job("ab\tc", 2..4, &[]).text, "  c");
    }

    #[test]
    fn change_spans_become_dark_backgrounds() {
        let doc = light();
        let spans = [
            ChangeSpan {
                range: 2..4,
                kind: SpanKind::Insert,
            },
            ChangeSpan {
                range: 5..6,
                kind: SpanKind::Delete,
            },
        ];
        let job = job("abcdefgh", 0..8, &spans);
        let backgrounds: Vec<_> = job.sections.iter().map(|s| s.format.background).collect();
        assert_eq!(
            backgrounds,
            [
                Color32::TRANSPARENT,
                doc.insert.text,
                Color32::TRANSPARENT,
                doc.delete.text,
                Color32::TRANSPARENT
            ]
        );
    }

    #[test]
    fn zero_width_markers_do_not_paint_a_background() {
        let spans = [ChangeSpan {
            range: 3..3,
            kind: SpanKind::Insert,
        }];
        let job = job("abcdef", 0..6, &spans);
        assert!(job.sections.iter().all(|s| s.format.background == Color32::TRANSPARENT));
    }

    #[test]
    fn selection_spans_lines_and_orders_its_ends() {
        let selection = Selection {
            side: Side::Left,
            anchor: TextPos::new(2, 1),
            head: TextPos::new(0, 3),
        };
        assert_eq!(selection.bytes_on(Side::Left, 0, 5), Some(3..5));
        assert_eq!(selection.bytes_on(Side::Left, 1, 4), Some(0..4));
        assert_eq!(selection.bytes_on(Side::Left, 2, 6), Some(0..1));
        assert_eq!(selection.bytes_on(Side::Right, 1, 4), None);
        assert_eq!(selection.bytes_on(Side::Left, 3, 4), None);
    }

    #[test]
    fn a_filter_keeps_the_blocks_and_leaves_one_separator_per_hidden_run() {
        let row = |left: Option<u32>, right: Option<u32>, block: Option<u32>| Row {
            left,
            right,
            block,
            pair: None,
        };
        let rows = [
            row(Some(0), Some(0), None),
            row(Some(1), Some(1), Some(0)),
            row(Some(2), Some(2), None),
            row(Some(3), None, Some(1)),
            row(Some(4), Some(3), None),
            row(None, Some(4), Some(2)),
        ];
        let shown = visible_rows(&rows, &[true, false, true], None, None, &[]);
        let [left, right] = fold_lines(&rows, &shown, [5, 5]);
        assert_eq!(
            left,
            [Fold::Separator(1), Fold::Shown, Fold::Separator(3), Fold::Hidden, Fold::Hidden]
        );
        assert_eq!(
            right,
            [Fold::Separator(1), Fold::Shown, Fold::Separator(3), Fold::Hidden, Fold::Shown]
        );
    }

    fn row(left: Option<u32>, right: Option<u32>, block: Option<u32>) -> Row {
        Row { left, right, block, pair: None }
    }

    /// Ten rows facing each other, rows 4 and 8 being one block each.
    fn two_blocks() -> Vec<Row> {
        (0..10)
            .map(|i| row(Some(i), Some(i), match i { 4 => Some(0), 8 => Some(1), _ => None }))
            .collect()
    }

    fn shown(rows: &[bool]) -> String {
        rows.iter().map(|&shown| if shown { '#' } else { '.' }).collect()
    }

    #[test]
    fn context_keeps_a_few_unchanged_rows_around_each_block() {
        let rows = two_blocks();
        assert_eq!(shown(&visible_rows(&rows, &[], Some(1), None, &[])), "...###.###");
        assert_eq!(shown(&visible_rows(&rows, &[], Some(0), None, &[])), "....#...#.");
        assert_eq!(shown(&visible_rows(&rows, &[], Some(3), None, &[])), ".#########");
        let [left, _] = fold_lines(&rows, &visible_rows(&rows, &[], Some(1), None, &[]), [10, 10]);
        assert_eq!(left[0..4], [Fold::Separator(3), Fold::Hidden, Fold::Hidden, Fold::Shown]);
        assert_eq!(left[6], Fold::Separator(1));
    }

    #[test]
    fn the_filter_picks_the_blocks_and_the_context_what_shows_around_them() {
        let rows = two_blocks();
        assert_eq!(shown(&visible_rows(&rows, &[false, true], Some(1), None, &[])), ".......###");
        // Context never crosses a block the filter hides.
        assert_eq!(shown(&visible_rows(&rows, &[false, true], Some(9), None, &[])), ".....#####");
        assert_eq!(shown(&visible_rows(&rows, &[false, true], None, None, &[])), "........#.");
    }

    #[test]
    fn the_edited_row_and_unfolded_runs_show_whatever_folds() {
        let rows = two_blocks();
        assert_eq!(shown(&visible_rows(&rows, &[], Some(0), Some(1), &[])), ".#..#...#.");
        assert_eq!(shown(&visible_rows(&rows, &[], Some(1), Some(1), &[])), "######.###");
        assert_eq!(shown(&visible_rows(&rows, &[false, true], None, None, &[0..3])), "###.....#.");
    }

    #[test]
    fn no_filter_shows_everything() {
        let folds = fold_lines(&[], &visible_rows(&[], &[], None, None, &[]), [2, 3]);
        assert_eq!(folds[0], [Fold::Shown; 2]);
        assert_eq!(folds[1], [Fold::Shown; 3]);
    }

    #[test]
    fn colours_lagging_behind_an_edit_never_split_a_character() {
        let doc = light();
        let font = FontId::monospace(14.0);
        let stale = [StyleRun { start: 1, end: 3, color: [1, 2, 3], italic: false }];
        let job = build_job(&JobInput {
            text: "été",
            range: 0..5,
            styles: &stale,
            spans: &[],
            ignored: &[],
            whole: None,
            selected: None,
            found: &[],
            font: &font,
            line_height: 21.0,
            doc: &doc,
        });
        assert_eq!(job.text, "été");
    }

    #[test]
    fn ignored_text_is_faded_on_its_own_background() {
        let doc = Look::new(ThemeKind::TokyoNight, CodeTone::Theme, false, LinkStyle::Ribbons).doc;
        let font = FontId::monospace(14.0);
        let job = build_job(&JobInput {
            text: "at 2024-03-15 ok",
            range: 0..16,
            styles: &[],
            spans: &[],
            ignored: &[3..13],
            whole: None,
            selected: None,
            found: &[],
            font: &font,
            line_height: 21.0,
            doc: &doc,
        });
        let backgrounds: Vec<_> = job.sections.iter().map(|s| s.format.background).collect();
        assert_eq!(backgrounds, [Color32::TRANSPARENT, doc.ignored.text, Color32::TRANSPARENT]);
        assert_ne!(job.sections[1].format.color, doc.text);
    }

    #[test]
    fn found_text_takes_over_the_change_colour() {
        let doc = light();
        let font = FontId::monospace(14.0);
        let spans = [ChangeSpan { range: 0..6, kind: SpanKind::Replace }];
        let job = build_job(&JobInput {
            text: "abcdef",
            range: 0..6,
            styles: &[],
            spans: &spans,
            ignored: &[],
            whole: None,
            selected: None,
            found: &[2..4],
            font: &font,
            line_height: 21.0,
            doc: &doc,
        });
        let backgrounds: Vec<_> = job.sections.iter().map(|s| s.format.background).collect();
        assert_eq!(backgrounds, [doc.change.text, doc.found, doc.change.text]);
    }

    #[test]
    fn line_lookup_handles_wrapped_lines() {
        let offsets = [0, 1, 4, 5];
        assert_eq!(line_at(&offsets, 0), 0);
        assert_eq!(line_at(&offsets, 1), 1);
        assert_eq!(line_at(&offsets, 3), 1);
        assert_eq!(line_at(&offsets, 4), 2);
    }
}
