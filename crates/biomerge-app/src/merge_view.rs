//! Three-way merge: the local version on the left, the editable result in the centre, the
//! remote version on the right. Each chunk of the merge is framed on the three panes and linked
//! across them as blocks are in the two-way view, in either display mode (`sync.rs`, or facing
//! lines kept level). The base, which is not a pane of its own, can take the centre's place on
//! request, read-only; each conflict also shows it in the tooltip of its action button.
//!
//! The result is edited through the engine (`biomerge_core::merge3`): typing goes through
//! `Merge::track_edit`, choosing a side through `Merge::set_resolution`, and each step keeps
//! the `MergeEdit` that undoes it, so one stack undoes both kinds of change.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use biomerge_core::diff::{ChangeSpan, DiffOptions};
use biomerge_core::document::{Document, Edit};
use biomerge_core::filter::{Filter, FilterOptions};
use biomerge_core::highlight::{self, Checkpoints, LineStyles, SyntaxTheme};
use biomerge_core::ignore_rules::IgnoreRules;
use biomerge_core::merge3::{Chunk, ChunkKind, Merge, MergeEdit, MergeOptions, Resolution, Stop, Version};
use biomerge_core::textedit::{self, TextPos};
use biomerge_core::wrap;
use egui::{
    Align2, CursorIcon, Event, FontFamily, FontId, ImeEvent, Key, Modifiers, Painter, PointerButton, Popup, Pos2,
    Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use rayon::prelude::*;

use crate::icons::Icon;
use crate::scroll::{Position, Scroller};
use crate::sync::SyncMap;
use crate::panes::{self, CODE_PADDING};
use crate::text_view::{
    ARROW_MARGIN, ARROW_SIZE, CARET_MARGIN_ROWS, CARET_WIDTH, CARET_BLINK_SECONDS, GUTTER_PADDING, LinePaint,
    SearchMode, SearchTarget, StatusItem, TEXT_PADDING, UNDO_MERGE_SECONDS, WRAP_MARGIN, block_button, caret_motion,
    deleted_range, line_at, paint_link, paint_text_line, snap,
};
use crate::theme::{DEFAULT_CODE_SIZE, DocPalette, LinkStyle, Look, Tint, UiPalette, line_height, mix};
use crate::widgets::{self, Badge, Item, PathAction, PathBar, menu_frame, separator};

/// The panes, left to right.
const LEFT: usize = 0;
const CENTRE: usize = 1;
const RIGHT: usize = 2;
/// A chunk this long on one side is not compared word by word: its frame says enough.
const MAX_SPAN_LINES: usize = 5_000;
/// Lines of the base shown in a conflict's tooltip.
const PEEK_LINES: usize = 14;

/// What a merge tab opens.
#[derive(Clone, Debug)]
pub enum MergeInputs {
    /// The four files of `git mergetool`, the first three possibly named by labels.
    Files {
        base: PathBuf,
        local: PathBuf,
        remote: PathBuf,
        merged: PathBuf,
        labels: [Option<String>; 3],
    },
    /// A working file git left with conflict markers, resolved in place.
    Conflicted(PathBuf),
}

impl MergeInputs {
    /// The file the result is written to.
    pub fn output(&self) -> &Path {
        match self {
            MergeInputs::Files { merged, .. } => merged,
            MergeInputs::Conflicted(path) => path,
        }
    }
}

/// The settings a merge tab shares with the text tabs.
#[derive(Clone)]
pub struct MergeSettings {
    pub options: DiffOptions,
    pub rules: IgnoreRules,
    pub wrap_lines: bool,
    pub aligned: bool,
    pub syntax_theme: SyntaxTheme,
}

enum Message {
    Loaded {
        merge: Box<Merge>,
        syntax: Option<String>,
        /// The result is not what the output file holds (git's markers, or no file yet).
        unsaved: bool,
    },
    Failed(String),
    Styles {
        generation: u64,
        slot: usize,
        start: usize,
        lines: Vec<LineStyles>,
    },
}

/// The four texts, in the order of the highlighting slots.
const VERSIONS: [Version; 4] = [Version::Base, Version::Left, Version::Right, Version::Result];

fn slot(version: Version) -> usize {
    match version {
        Version::Base => 0,
        Version::Left => 1,
        Version::Right => 2,
        Version::Result => 3,
    }
}

/// One undoable step: a resolution or an edit by hand, with the caret around it.
struct Step {
    edit: MergeEdit,
    /// Steps sharing a group are undone together: a burst of typing.
    group: u64,
    revision_before: u64,
    revision_after: u64,
    caret_before: Option<(usize, TextPos)>,
    caret_after: Option<(usize, TextPos)>,
    typed_at: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    pane: usize,
    anchor: TextPos,
    head: TextPos,
}

impl Selection {
    fn ordered(&self) -> (TextPos, TextPos) {
        let key = |p: &TextPos| (p.line, p.byte);
        if key(&self.anchor) <= key(&self.head) { (self.anchor, self.head) } else { (self.head, self.anchor) }
    }

    fn bytes_on(&self, pane: usize, line: u32, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if pane != self.pane || line < start.line || line > end.line || start == end {
            return None;
        }
        let from = if line == start.line { start.byte } else { 0 };
        let to = if line == end.line { end.byte } else { len };
        Some(from..to)
    }
}

/// One occurrence of the text searched for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Found {
    chunk: u32,
    pane: usize,
    line: u32,
    start: u32,
    end: u32,
}

impl Found {
    /// Reading order: chunk after chunk, left to right inside one.
    fn key(&self) -> (u32, usize, u32, u32) {
        (self.chunk, self.pane, self.line, self.start)
    }
}

/// A question asked before saving.
struct SaveQuestion {
    unresolved: usize,
    /// The result is empty because this side deleted the file.
    deleted_by: Option<&'static str>,
}

/// What a click in the view asked for, applied once painting is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkAction {
    Resolve(usize, Resolution),
    /// The conflict written with git's markers, to edit them by hand.
    Markers(usize),
}

#[derive(Default)]
struct Layout {
    valid: bool,
    wrap: bool,
    aligned: bool,
    columns: usize,
    line_height: f32,
    /// What the panes showed when the layout was made.
    versions: Option<[Version; 3]>,
    /// For each pane, the first visual line of each line; one more entry holds the total.
    offsets: [Vec<u32>; 3],
    /// When aligned: the first visual line of each row (one more entry: the total), the first
    /// row of each chunk, and the row of each line of each pane.
    row_offsets: Vec<u32>,
    chunk_rows: Vec<u32>,
    line_rows: [Vec<u32>; 3],
    /// The chunks framed as blocks, in order; their extents are the sync map's blocks.
    blocks: Vec<usize>,
    sync: SyncMap<3>,
    max_columns: usize,
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
    panes: [Pane; 3],
    cards: [Rect; 3],
    overview: Rect,
    line_height: f32,
    char_width: f32,
}

pub struct MergeTab {
    /// To merge again when a comparison option changes.
    ctx: egui::Context,
    inputs: MergeInputs,
    merge: Option<Box<Merge>>,
    error: Option<String>,
    pub message: Option<String>,
    loading: bool,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
    options: DiffOptions,
    rules: IgnoreRules,
    syntax: Option<String>,
    syntax_theme: SyntaxTheme,
    styles: [Vec<Option<LineStyles>>; 4],
    cancel: [Arc<AtomicBool>; 4],
    generation: [u64; 4],
    checkpoints: [Arc<Checkpoints>; 4],
    /// Word spans of each chunk's lines against the base, by chunk and version, worked out for
    /// the chunks in view.
    spans: HashMap<(usize, Version), Arc<Vec<Vec<ChangeSpan>>>>,
    /// The centre shows the base instead of the result.
    show_base: bool,
    layout: Layout,
    scroll: f64,
    pane_scroll: [f64; 3],
    pending_place: Option<(usize, f64)>,
    scroll_x: f32,
    scroller: Scroller,
    code_size: f32,
    current: Option<usize>,
    caret: Option<(usize, TextPos)>,
    selection: Option<Selection>,
    goal_column: Option<usize>,
    /// After an edit: this line of this pane stays at this height on screen.
    anchor: Option<(usize, u32, f64)>,
    caret_to_reveal: bool,
    center_caret: bool,
    undo: Vec<Step>,
    redo: Vec<Step>,
    next_group: u64,
    next_revision: u64,
    revision: u64,
    saved_revision: u64,
    /// Opened with a result that differs from the output file, and not saved since.
    unsaved_at_open: bool,
    /// The last save left no conflict unresolved: what `git mergetool` must hear.
    saved_clean: bool,
    question: Option<SaveQuestion>,
    pub filter_query: String,
    pub filter_options: FilterOptions,
    filter: Option<Filter>,
    filter_error: Option<String>,
    matches: Vec<Found>,
    /// The chunk the context menu was opened on.
    menu_chunk: Option<usize>,
    geometry: Option<Geometry>,
    jump_to_first: bool,
    reveal_current: bool,
}

impl MergeTab {
    pub fn new(ctx: &egui::Context, inputs: MergeInputs, settings: MergeSettings) -> Self {
        let (sender, receiver) = channel();
        let MergeSettings { options, rules, wrap_lines, aligned, syntax_theme } = settings;
        let mut tab = Self {
            ctx: ctx.clone(),
            inputs,
            merge: None,
            error: None,
            message: None,
            loading: false,
            sender,
            receiver,
            options,
            rules,
            syntax: None,
            syntax_theme,
            styles: Default::default(),
            cancel: Default::default(),
            generation: [0; 4],
            checkpoints: Default::default(),
            spans: HashMap::new(),
            show_base: false,
            layout: Layout { wrap: wrap_lines, aligned, line_height: line_height(DEFAULT_CODE_SIZE), ..Layout::default() },
            scroll: 0.0,
            pane_scroll: [0.0; 3],
            pending_place: None,
            scroll_x: 0.0,
            scroller: Scroller::default(),
            code_size: DEFAULT_CODE_SIZE,
            current: None,
            caret: None,
            selection: None,
            goal_column: None,
            anchor: None,
            caret_to_reveal: false,
            center_caret: false,
            undo: Vec::new(),
            redo: Vec::new(),
            next_group: 1,
            next_revision: 1,
            revision: 0,
            saved_revision: 0,
            unsaved_at_open: false,
            saved_clean: false,
            question: None,
            filter_query: String::new(),
            filter_options: FilterOptions::default(),
            filter: None,
            filter_error: None,
            matches: Vec::new(),
            menu_chunk: None,
            geometry: None,
            jump_to_first: true,
            reveal_current: false,
        };
        tab.reload(ctx);
        tab
    }

    // --- State --------------------------------------------------------------------------------

    pub fn title(&self) -> String {
        let path = self.inputs.output();
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        format!("{name} (fusion)")
    }

    pub fn is_ready(&self) -> bool {
        self.merge.is_some() && !self.loading
    }

    pub fn is_dirty(&self) -> bool {
        self.merge.is_some() && (self.revision != self.saved_revision || self.unsaved_at_open)
    }

    /// What `git mergetool` must hear when this tab goes: 0 only once the result was saved
    /// without a conflict left.
    pub fn exit_code(&self) -> i32 {
        if self.saved_clean { 0 } else { 1 }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn has_blocks(&self) -> bool {
        self.merge.as_ref().is_some_and(|merge| merge.next(None, Stop::Change).is_some())
    }

    #[cfg(test)]
    pub fn unresolved_count(&self) -> usize {
        self.merge.as_ref().map_or(0, |merge| merge.unresolved_count())
    }

    pub fn shows_base(&self) -> bool {
        self.show_base
    }

    /// Taking the base means something: the base of a conflicted file without base sections is
    /// unknown.
    pub fn base_known(&self) -> bool {
        self.merge.as_ref().is_some_and(|merge| merge.base_known())
    }

    pub fn has_caret(&self) -> bool {
        self.caret.is_some()
    }

    pub fn wants_dialog(&self) -> bool {
        self.question.is_some()
    }

    /// The chunk the keys and the menu act on, when it is a conflict that can be combined.
    pub fn current_combinable(&self) -> bool {
        self.current_chunk().is_some_and(|chunk| chunk.combinable)
    }

    fn current_chunk(&self) -> Option<&Chunk> {
        self.merge.as_ref()?.chunks().get(self.current?)
    }

    pub fn current_is_conflict(&self) -> bool {
        self.current_chunk().is_some_and(Chunk::is_conflict)
    }

    /// What a pane shows.
    fn version(&self, pane: usize) -> Version {
        match pane {
            LEFT => Version::Left,
            CENTRE if self.show_base => Version::Base,
            CENTRE => Version::Result,
            _ => Version::Right,
        }
    }

    fn versions(&self) -> [Version; 3] {
        [0, 1, 2].map(|pane| self.version(pane))
    }

    fn doc(&self, pane: usize) -> Option<&Document> {
        Some(self.merge.as_ref()?.document(self.version(pane)))
    }

    fn pane_name(&self, pane: usize) -> &'static str {
        match self.version(pane) {
            Version::Left => "La version locale",
            Version::Right => "La version distante",
            Version::Base => "L'ancêtre commun",
            Version::Result => "Le résultat",
        }
    }

    // --- Loading ------------------------------------------------------------------------------

    fn cancel_highlighting(&mut self, slot: usize) {
        self.cancel[slot].store(true, Ordering::Relaxed);
        self.cancel[slot] = Arc::new(AtomicBool::new(false));
        self.generation[slot] += 1;
    }

    pub fn reload(&mut self, ctx: &egui::Context) {
        for slot in 0..4 {
            self.cancel_highlighting(slot);
        }
        self.loading = true;
        self.error = None;
        self.merge = None;
        self.undo.clear();
        self.redo.clear();
        self.revision = 0;
        self.saved_revision = 0;
        self.unsaved_at_open = false;
        self.caret = None;
        self.selection = None;
        self.geometry = None;
        self.spans.clear();
        self.matches.clear();
        self.current = None;
        self.question = None;
        self.jump_to_first = true;
        let inputs = self.inputs.clone();
        let options = MergeOptions { diff: self.options, rules: self.rules.clone(), combine_adjacent: false };
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        // Opening a big merge takes a while: never on the interface's thread.
        std::thread::spawn(move || {
            let opened = match &inputs {
                MergeInputs::Files { base, local, remote, merged, .. } => Merge::open(base, local, remote, merged, options),
                MergeInputs::Conflicted(path) => Merge::open_conflicted(path, options),
            };
            let message = match opened {
                Ok(merge) => {
                    let unsaved = match &inputs {
                        MergeInputs::Files { merged, .. } => match Document::load(merged) {
                            Ok(disk) => disk.lines.iter().map(|l| &l.text).ne(merge.result.lines.iter().map(|l| &l.text)),
                            Err(_) => true,
                        },
                        MergeInputs::Conflicted(_) => false,
                    };
                    let first = |version: Version| merge.document(version).lines.first().map_or(String::new(), |l| l.text.clone());
                    let path = inputs.output().to_owned();
                    let syntax = highlight::detect_syntax(Some(&path), &first(Version::Result))
                        .or_else(|| highlight::detect_syntax(Some(&path), &first(Version::Left)));
                    Message::Loaded { merge: Box::new(merge), syntax, unsaved }
                }
                Err(error) => Message::Failed(error.to_string()),
            };
            let _ = sender.send(message);
            ctx.request_repaint();
        });
    }

    fn start_highlighting(&mut self, ctx: &egui::Context, slot: usize, changed_line: usize) {
        self.cancel_highlighting(slot);
        let (Some(merge), Some(syntax)) = (&self.merge, self.syntax.clone()) else {
            return;
        };
        let lines = merge.document(VERSIONS[slot]).lines.clone();
        let sender = self.sender.clone();
        let cancel = self.cancel[slot].clone();
        let checkpoints = self.checkpoints[slot].clone();
        let generation = self.generation[slot];
        let theme = self.syntax_theme;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            highlight::highlight_lines(&syntax, theme, &lines, changed_line, &checkpoints, &cancel, |start, styles| {
                let _ = sender.send(Message::Styles { generation, slot, start, lines: styles });
                ctx.request_repaint();
            });
        });
    }

    fn highlight_all(&mut self, ctx: &egui::Context) {
        for slot in 0..4 {
            self.checkpoints[slot].clear();
            self.start_highlighting(ctx, slot, 0);
        }
    }

    pub fn set_syntax_theme(&mut self, ctx: &egui::Context, theme: SyntaxTheme) {
        if self.syntax_theme != theme {
            self.syntax_theme = theme;
            for styles in &mut self.styles {
                styles.iter_mut().for_each(|style| *style = None);
            }
            self.highlight_all(ctx);
        }
    }

    /// True once every highlightable line has received its colours.
    #[cfg(test)]
    pub fn highlight_finished(&self) -> bool {
        self.syntax.is_none() || self.styles.iter().all(|styles| styles.iter().all(Option::is_some))
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Loaded { merge, syntax, unsaved } => {
                    self.styles = VERSIONS.map(|version| vec![None; merge.document(version).len()]);
                    self.merge = Some(merge);
                    self.syntax = syntax;
                    self.unsaved_at_open = unsaved;
                    self.loading = false;
                    self.layout.valid = false;
                    self.scroll = 0.0;
                    self.scroll_x = 0.0;
                    self.compute_matches();
                    self.highlight_all(ctx);
                }
                Message::Failed(error) => {
                    self.error = Some(error);
                    self.loading = false;
                }
                Message::Styles { generation, slot, start, lines } if generation == self.generation[slot] => {
                    let target = &mut self.styles[slot];
                    for (offset, line) in lines.into_iter().enumerate() {
                        if let Some(place) = target.get_mut(start + offset) {
                            *place = Some(line);
                        }
                    }
                }
                Message::Styles { .. } => {}
            }
        }
    }

    // --- Settings -----------------------------------------------------------------------------

    pub fn set_wrap(&mut self, wrap_lines: bool) {
        if self.layout.wrap != wrap_lines {
            self.layout.wrap = wrap_lines;
            self.layout.valid = false;
            self.scroll_x = 0.0;
        }
    }

    pub fn set_aligned(&mut self, aligned: bool) {
        if self.layout.aligned != aligned {
            self.keep_place();
            self.layout.aligned = aligned;
        }
    }

    /// The layout is made again around the place being read.
    fn keep_place(&mut self) {
        if self.layout.valid {
            let place = self.layout.sync.locate(self.scroll / self.layout.line_height as f64);
            self.pending_place.get_or_insert(place);
        }
        self.layout.valid = false;
    }

    /// New comparison options merge again, which only an untouched tab can do at once.
    pub fn set_options(&mut self, options: DiffOptions) {
        if self.options != options {
            self.options = options;
            self.merge_again();
        }
    }

    pub fn set_rules(&mut self, rules: IgnoreRules) {
        self.rules = rules;
        self.merge_again();
    }

    fn merge_again(&mut self) {
        if self.undo.is_empty() && !self.loading {
            let ctx = self.ctx.clone();
            self.reload(&ctx);
        } else {
            self.message = Some("Options de comparaison appliquées au prochain rechargement (F5)".to_owned());
        }
    }

    /// The centre shows the base, or the result again.
    pub fn toggle_base(&mut self) {
        self.show_base = !self.show_base;
        if self.caret.is_some_and(|(pane, _)| pane == CENTRE) {
            self.caret = None;
            self.selection = None;
        }
        self.keep_place();
        self.compute_matches();
    }

    fn set_code_size(&mut self, size: f32) {
        let height = line_height(size);
        self.code_size = size;
        let previous = self.layout.line_height;
        if height != previous {
            self.layout.line_height = height;
            self.scroll *= (height / previous) as f64;
            self.layout.valid = false;
        }
    }

    // --- Resolving ----------------------------------------------------------------------------

    /// Applies a resolution to a chunk, as one undoable step.
    pub fn resolve(&mut self, ctx: &egui::Context, chunk: usize, resolution: Resolution) {
        if self.show_base {
            self.message = Some("Le centre montre l'ancêtre commun : Alt+B revient au résultat".to_owned());
            return;
        }
        let Some(merge) = &mut self.merge else {
            return;
        };
        let Some(step) = merge.set_resolution(chunk, resolution) else {
            return;
        };
        self.current = Some(chunk);
        self.reveal_current = true;
        self.caret = None;
        self.selection = None;
        self.commit(ctx, step, None, false);
        self.copy_styles(chunk, resolution);
    }

    /// The conflict written with git's markers, to be edited by hand.
    pub fn edit_with_markers(&mut self, ctx: &egui::Context, chunk: usize) {
        if self.show_base {
            return;
        }
        let Some(merge) = &mut self.merge else {
            return;
        };
        let lines = merge.marker_lines(chunk);
        let step = merge.set_lines(chunk, lines);
        self.current = Some(chunk);
        self.reveal_current = true;
        let start = merge.chunks()[chunk].result.start;
        self.commit(ctx, step, None, false);
        self.set_caret(CENTRE, TextPos::new(start, 0), false);
    }

    pub fn apply(&mut self, ctx: &egui::Context, action: ChunkAction) {
        match action {
            ChunkAction::Resolve(chunk, resolution) => self.resolve(ctx, chunk, resolution),
            ChunkAction::Markers(chunk) => self.edit_with_markers(ctx, chunk),
        }
    }

    /// Ctrl+→ and Ctrl+← on the current chunk: what flows that way into the result. A chunk out
    /// of view is only shown, never changed unseen.
    pub fn take_current(&mut self, ctx: &egui::Context, resolution: Resolution) {
        let Some(chunk) = self.current else {
            return;
        };
        if self.chunk_in_view(chunk) {
            self.resolve(ctx, chunk, resolution);
        } else {
            self.select_chunk(chunk);
        }
    }

    /// The lines a resolution took keep their colours at once; a full pass follows.
    fn copy_styles(&mut self, chunk: usize, resolution: Resolution) {
        let Some(merge) = &self.merge else {
            return;
        };
        let chunk = &merge.chunks()[chunk];
        let sources: &[Version] = match resolution {
            Resolution::Left => &[Version::Left],
            Resolution::Right => &[Version::Right],
            Resolution::Base | Resolution::Unresolved => &[Version::Base],
            Resolution::LeftThenRight => &[Version::Left, Version::Right],
            Resolution::RightThenLeft => &[Version::Right, Version::Left],
            _ => &[],
        };
        let mut copied = Vec::new();
        for &version in sources {
            let range = chunk.range(version);
            copied.extend(self.styles[slot(version)].get(range.start as usize..range.end as usize).unwrap_or(&[]).iter().cloned());
        }
        let target = chunk.result.start as usize..chunk.result.end as usize;
        if copied.len() == target.len() {
            if let Some(styles) = self.styles[slot(Version::Result)].get_mut(target) {
                styles.clone_from_slice(&copied);
            }
        }
    }

    /// Records a step already applied to the merge, and lets every view follow.
    fn commit(&mut self, ctx: &egui::Context, edit: MergeEdit, caret: Option<TextPos>, typing: bool) {
        let now = ctx.input(|input| input.time);
        self.splice_styles(&edit.edit);
        let changed_line = edit.edit.at;
        let caret_before = self.caret;
        let caret_after = caret.map(|pos| (CENTRE, pos));
        let revision_before = self.revision;
        self.revision = self.next_revision;
        self.next_revision += 1;
        let single_line = edit.edit.removed.len() == 1 && edit.edit.inserted.len() == 1;
        // Keystrokes close together on one line are undone at once.
        let burst = self.undo.last().filter(|last| {
            typing
                && single_line
                && last.edit.edit.at == edit.edit.at
                && last.edit.edit.inserted.len() == 1
                && last.caret_after == caret_before
                && last.typed_at.is_some_and(|at| now - at < UNDO_MERGE_SECONDS)
        });
        let group = match burst {
            Some(last) => last.group,
            None => {
                self.next_group += 1;
                self.next_group
            }
        };
        self.undo.push(Step {
            edit,
            group,
            revision_before,
            revision_after: self.revision,
            caret_before,
            caret_after,
            typed_at: typing.then_some(now),
        });
        self.redo.clear();
        self.after_result_change();
        self.start_highlighting(ctx, slot(Version::Result), changed_line);
        if let Some(pos) = caret {
            self.goal_column = None;
            self.set_caret(CENTRE, pos, false);
        }
    }

    fn after_result_change(&mut self) {
        self.spans.retain(|(_, version), _| *version != Version::Result);
        self.layout.valid = false;
        self.compute_matches();
    }

    fn splice_styles(&mut self, edit: &Edit) {
        let styles = &mut self.styles[slot(Version::Result)];
        let at = (edit.at + edit.removed.len().min(edit.inserted.len())).min(styles.len());
        let end = (edit.at + edit.removed.len()).min(styles.len());
        let added = edit.inserted.len().saturating_sub(edit.removed.len());
        styles.splice(at..end.max(at), std::iter::repeat_n(None, added));
    }

    fn take_group(stack: &mut Vec<Step>) -> Vec<Step> {
        let Some(last) = stack.pop() else {
            return Vec::new();
        };
        let group = last.group;
        let mut steps = vec![last];
        while stack.last().is_some_and(|step| step.group == group) {
            steps.extend(stack.pop());
        }
        steps
    }

    pub fn undo(&mut self, ctx: &egui::Context) {
        let steps = Self::take_group(&mut self.undo);
        let (Some(first), Some(merge)) = (steps.last(), &mut self.merge) else {
            return;
        };
        let (revision, caret) = (first.revision_before, first.caret_before);
        let mut changed = usize::MAX;
        for step in &steps {
            merge.undo(&step.edit);
            changed = changed.min(step.edit.edit.at);
        }
        for step in &steps {
            self.splice_styles(&step.edit.edit.inverted());
        }
        self.finish_history(ctx, revision, caret, changed, &steps);
        self.redo.extend(steps);
    }

    pub fn redo(&mut self, ctx: &egui::Context) {
        let steps = Self::take_group(&mut self.redo);
        let (Some(last), Some(merge)) = (steps.last(), &mut self.merge) else {
            return;
        };
        let (revision, caret) = (last.revision_after, last.caret_after);
        let mut changed = usize::MAX;
        for step in &steps {
            merge.redo(&step.edit);
            changed = changed.min(step.edit.edit.at);
        }
        for step in &steps {
            self.splice_styles(&step.edit.edit);
        }
        self.finish_history(ctx, revision, caret, changed, &steps);
        self.undo.extend(steps);
    }

    fn finish_history(&mut self, ctx: &egui::Context, revision: u64, caret: Option<(usize, TextPos)>, changed: usize, steps: &[Step]) {
        self.revision = revision;
        self.selection = None;
        self.caret = None;
        // The chunk the step changed becomes the current one, in view.
        if let (Some(merge), Some(step)) = (&self.merge, steps.first()) {
            self.current = merge.chunk_at(Version::Result, step.edit.edit.at as u32).or(self.current);
        }
        self.reveal_current = true;
        self.after_result_change();
        self.start_highlighting(ctx, slot(Version::Result), changed);
        if let Some((pane, pos)) = caret.filter(|_| !self.show_base) {
            self.set_caret(pane, pos, false);
        }
    }

    // --- Saving -------------------------------------------------------------------------------

    /// Saves the result. Conflicts left unresolved, or a result emptied by a deletion, are
    /// asked about first: `Ok(false)` while the question is shown.
    pub fn save(&mut self) -> Result<bool, String> {
        let Some(merge) = &self.merge else {
            return Ok(true);
        };
        let unresolved = merge.unresolved_count();
        let deleted_by = (merge.result.is_empty() && unresolved == 0)
            .then(|| match (merge.is_missing(Version::Left), merge.is_missing(Version::Right)) {
                (true, _) => Some("la version locale"),
                (_, true) => Some("la version distante"),
                _ => None,
            })
            .flatten();
        if unresolved > 0 || deleted_by.is_some() {
            self.question = Some(SaveQuestion { unresolved, deleted_by });
            return Ok(false);
        }
        self.write(false).map(|()| true)
    }

    fn write(&mut self, force: bool) -> Result<(), String> {
        let Some(merge) = &mut self.merge else {
            return Ok(());
        };
        merge.save(force).map_err(|error| error.to_string())?;
        self.saved_clean = merge.unresolved_count() == 0;
        self.saved_revision = self.revision;
        self.unsaved_at_open = false;
        self.message = (!self.saved_clean).then(|| "Enregistré avec les marqueurs de conflit : le fichier reste en conflit".to_owned());
        Ok(())
    }

    /// Answers the save question, as its buttons do.
    pub fn answer_save(&mut self, confirmed: bool) -> Result<(), String> {
        let Some(question) = self.question.take() else {
            return Ok(());
        };
        if !confirmed {
            return Ok(());
        }
        self.write(question.unresolved > 0)
    }

    fn show_question(&mut self, ctx: &egui::Context, chrome: &UiPalette) {
        let Some(question) = &self.question else {
            return;
        };
        let mut lines = Vec::new();
        let title;
        let confirm;
        if question.unresolved > 0 {
            let n = question.unresolved;
            title = if n == 1 { "1 conflit non résolu".to_owned() } else { format!("{n} conflits non résolus") };
            lines.push(format!(
                "{} écrit{} avec les marqueurs de git (<<<<<<< ======= >>>>>>>) : le fichier restera en conflit pour git.",
                if n == 1 { "Il sera" } else { "Ils seront" },
                if n == 1 { "" } else { "s" }
            ));
            lines.push("F8 mène au prochain conflit non résolu.".to_owned());
            confirm = "Enregistrer avec les marqueurs de conflit";
        } else {
            title = "Résultat vide".to_owned();
            confirm = "Enregistrer le fichier vide";
        }
        if let Some(side) = question.deleted_by {
            lines.push(format!("Le résultat est vide : {side} a supprimé ce fichier. Un fichier vide sera écrit à sa place."));
        }
        let mut answer = None;
        let modal = egui::Modal::new(egui::Id::unique("merge-save-question")).frame(widgets::dialog_frame(chrome)).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.label(RichText::new(&title).font(widgets::semibold(15.0)));
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
        if let Some(confirmed) = answer {
            if let Err(error) = self.answer_save(confirmed) {
                self.message = Some(error);
            }
        }
    }

    // --- Navigation ---------------------------------------------------------------------------

    fn view_lines(&self) -> f64 {
        self.geometry.map_or(0.0, |g| (g.view.height() / g.line_height) as f64)
    }

    fn focal_line(&self) -> f64 {
        self.layout.sync.focal(self.scroll / self.layout.line_height as f64, self.view_lines())
    }

    fn update_pane_scroll(&mut self) {
        let line = self.layout.line_height as f64;
        self.pane_scroll = self.layout.sync.pane_scrolls(self.scroll / line, self.view_lines()).map(|lines| lines * line);
    }

    fn block_index(&self, chunk: usize) -> Option<usize> {
        self.layout.blocks.binary_search(&chunk).ok()
    }

    /// Brings a chunk where the panes agree, its frames level.
    fn select_chunk(&mut self, chunk: usize) {
        self.current = Some(chunk);
        let line = self.layout.line_height as f64;
        let extent = self.block_index(chunk).and_then(|block| self.layout.sync.blocks.get(block));
        if let Some(extent) = extent {
            self.scroll = self.layout.sync.scroll_showing(extent.start as f64, self.view_lines()) * line;
            self.update_pane_scroll();
        }
    }

    /// Screen position of the middle of a line's first visual line, in the last frame.
    #[cfg(test)]
    pub fn line_center(&self, pane: usize, line: u32) -> Option<Pos2> {
        let geometry = self.geometry?;
        let visual = *self.layout.offsets[pane].get(line as usize)? as f64;
        let y = geometry.view.top() as f64 + visual * geometry.line_height as f64 - self.pane_scroll[pane];
        Some(pos2(geometry.panes[pane].text_x + 40.0, y as f32 + geometry.line_height / 2.0))
    }

    /// Screen position of a chunk's action button, in the last frame.
    #[cfg(test)]
    pub fn menu_button_center(&self, chunk: usize) -> Option<Pos2> {
        let geometry = self.geometry?;
        let line = self.merge.as_ref()?.chunks()[chunk].result.start;
        let center = self.line_center(CENTRE, line)?;
        Some(pos2(geometry.panes[CENTRE].rect.right() - ARROW_SIZE.x / 2.0 - 7.0, center.y))
    }

    #[cfg(test)]
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    #[cfg(test)]
    pub fn pane_positions(&self) -> [f64; 3] {
        self.pane_scroll
    }

    #[cfg(test)]
    pub fn show_chunk(&mut self, chunk: usize) {
        self.select_chunk(chunk);
    }

    /// Some of the chunk's lines, or its rule, are on screen in one of the panes.
    fn chunk_in_view(&self, chunk: usize) -> bool {
        let Some(merge) = &self.merge else {
            return false;
        };
        let Some(chunk) = merge.chunks().get(chunk) else {
            return false;
        };
        let line = self.layout.line_height as f64;
        let view = self.view_lines() * line;
        (0..3).any(|pane| {
            let range = chunk.range(self.version(pane));
            let position = |index: u32| {
                self.layout.offsets[pane].get(index as usize).map_or(0.0, |&visual| visual as f64 * line - self.pane_scroll[pane])
            };
            position(range.end) >= 0.0 && position(range.start) <= view
        })
    }

    /// Ctrl+↓ and Ctrl+↑: the next or previous change, from the current one when it is in view,
    /// else from what is on screen.
    pub fn go_to_change(&mut self, forward: bool) {
        if self.layout.blocks.is_empty() {
            return;
        }
        self.caret = None;
        self.selection = None;
        let top = self.focal_line();
        let current = self.current.filter(|&chunk| self.chunk_in_view(chunk));
        let blocks = &self.layout.blocks;
        let starts = |index: usize| self.layout.sync.blocks.get(index).map_or(0.0, |b| b.start as f64);
        let target = match current.and_then(|chunk| self.block_index(chunk)) {
            Some(index) if forward => blocks.get(index + 1).or(blocks.get(index)).copied(),
            Some(index) => blocks.get(index.wrapping_sub(1)).or(blocks.get(index)).copied(),
            None if forward => (0..blocks.len()).find(|&i| starts(i) >= top).or(Some(blocks.len() - 1)).map(|i| blocks[i]),
            None => (0..blocks.len()).rev().find(|&i| starts(i) < top).or(Some(0)).map(|i| blocks[i]),
        };
        if let Some(target) = target {
            self.select_chunk(target);
        }
    }

    /// F8 and F7: the next or previous conflict left unresolved, around the ends of the file.
    pub fn go_to_unresolved(&mut self, forward: bool) {
        let Some(merge) = &self.merge else {
            return;
        };
        let from = self.current;
        let target = if forward {
            merge.next(from, Stop::Unresolved).or_else(|| merge.next(None, Stop::Unresolved))
        } else {
            merge.previous(from, Stop::Unresolved).or_else(|| merge.previous(None, Stop::Unresolved))
        };
        match target {
            Some(chunk) => {
                self.message = None;
                self.caret = None;
                self.selection = None;
                self.select_chunk(chunk);
            }
            None => self.message = Some("Aucun conflit non résolu".to_owned()),
        }
    }

    /// The chunk the keys and the menu act on.
    pub fn current_chunk_index(&self) -> Option<usize> {
        self.current
    }

    /// The current chunk, when it is a conflict.
    pub fn current_conflict(&self) -> Option<usize> {
        self.current.filter(|_| self.current_is_conflict())
    }

    pub fn scroll_lines(&mut self, lines: f64) {
        self.scroll += lines * self.layout.line_height as f64;
    }

    // --- Caret and editing --------------------------------------------------------------------

    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();
        let text = textedit::text_between(self.doc(selection.pane)?, start, end);
        (!text.is_empty()).then_some(text)
    }

    fn set_caret(&mut self, pane: usize, pos: TextPos, extend: bool) {
        let pos = match self.doc(pane) {
            Some(doc) => textedit::clamp(doc, pos),
            None => pos,
        };
        let anchor = match self.selection {
            Some(selection) if extend && selection.pane == pane => selection.anchor,
            _ => pos,
        };
        self.caret = Some((pane, pos));
        self.selection = Some(Selection { pane, anchor, head: pos });
        self.caret_to_reveal = true;
    }

    #[cfg(test)]
    pub fn put_caret(&mut self, pane: usize, line: u32, byte: usize) {
        self.set_caret(pane, TextPos::new(line, byte), false);
    }

    #[cfg(test)]
    pub fn caret_position(&self) -> Option<(usize, u32, usize)> {
        self.caret.map(|(pane, pos)| (pane, pos.line, pos.byte))
    }

    #[cfg(test)]
    pub fn merge(&self) -> &Merge {
        self.merge.as_ref().expect("loaded")
    }

    #[cfg(test)]
    pub fn result_text(&self) -> String {
        let merge = self.merge();
        merge.result.lines.iter().map(|line| format!("{}\n", line.text)).collect()
    }

    /// The chunk of a line, as the arrows will act on it.
    fn adopt_chunk_at(&mut self, pane: usize, line: u32) {
        let Some(merge) = &self.merge else {
            return;
        };
        if let Some(chunk) = merge.chunk_at(self.version(pane), line).filter(|&chunk| merge.chunks()[chunk].kind != ChunkKind::Unchanged) {
            self.current = Some(chunk);
        }
    }

    fn jump_to(&mut self, pane: usize, anchor: TextPos, head: TextPos) {
        self.goal_column = None;
        self.set_caret(pane, anchor, false);
        self.set_caret(pane, head, true);
        self.center_caret = true;
        self.adopt_chunk_at(pane, head.line);
    }

    /// The caret at the start of a line counted from 1, on the pane asked for or else the one
    /// in use, the result by default.
    pub fn go_to_line(&mut self, pane: Option<usize>, number: usize) {
        let pane = pane.or(self.caret.map(|(pane, _)| pane)).unwrap_or(CENTRE);
        let Some(doc) = self.doc(pane) else {
            return;
        };
        let last = doc.len().saturating_sub(1);
        let pos = TextPos::new(number.saturating_sub(1).min(last) as u32, 0);
        self.jump_to(pane, pos, pos);
    }

    pub fn caret_pane(&self) -> Option<usize> {
        self.caret.map(|(pane, _)| pane)
    }

    /// How many lines each pane holds, once loaded.
    pub fn line_counts(&self) -> Option<[usize; 3]> {
        self.merge.as_ref()?;
        Some([0, 1, 2].map(|pane| self.doc(pane).map_or(1, |doc| doc.len().max(1))))
    }

    fn refuse(&mut self, pane: usize) {
        self.message = Some(if pane == CENTRE && self.show_base {
            "L'ancêtre commun est en lecture seule : Alt+B revient au résultat".to_owned()
        } else {
            format!("{} est en lecture seule : seul le résultat, au centre, se modifie", self.pane_name(pane))
        });
    }

    fn replace_selection(&mut self, ctx: &egui::Context, text: &str, typing: bool) {
        let Some((pane, head)) = self.caret else {
            return;
        };
        let (start, end) = self.selection.filter(|s| s.pane == pane).map_or((head, head), |s| s.ordered());
        self.replace_range(ctx, pane, start, end, text, typing);
    }

    fn replace_range(&mut self, ctx: &egui::Context, pane: usize, start: TextPos, end: TextPos, text: &str, typing: bool) {
        if self.version(pane) != Version::Result {
            self.refuse(pane);
            return;
        }
        let line_pixels = self.layout.line_height as f64;
        let on_screen = self.layout.offsets[pane]
            .get(start.line as usize)
            .map(|&visual| visual as f64 * line_pixels - self.pane_scroll[pane]);
        let Some(merge) = &mut self.merge else {
            return;
        };
        let (start, end) = (textedit::clamp(&merge.result, start), textedit::clamp(&merge.result, end));
        let (edit, caret) = textedit::replace(&mut merge.result, start, end, text);
        let step = merge.track_edit(&edit);
        if let Some(chunk) = merge.chunk_at(Version::Result, caret.line).filter(|&c| merge.chunks()[c].kind != ChunkKind::Unchanged) {
            self.current = Some(chunk);
        }
        self.anchor = on_screen.map(|y| (pane, start.line, y));
        self.commit(ctx, step, Some(caret), typing);
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

    fn moved_vertically(&mut self, pane: usize, pos: TextPos, down: bool) -> TextPos {
        let ranges_of = |tab: &Self, text: &str| tab.visual_ranges(text);
        let Some(doc) = self.doc(pane) else {
            return pos;
        };
        let text = doc.lines.get(pos.line as usize).map_or("", |l| l.text.as_str()).to_owned();
        let count = doc.len();
        let ranges = ranges_of(self, &text);
        let row = ranges.iter().rposition(|range| range.start <= pos.byte).unwrap_or(0);
        let column = *self.goal_column.get_or_insert_with(|| wrap::column_of(&text, ranges[row].start, pos.byte));
        let inside = if down { row + 1 < ranges.len() } else { row > 0 };
        if inside {
            let target = if down { row + 1 } else { row - 1 };
            return TextPos::new(pos.line, wrap::offset_at_column(&text, ranges[target].clone(), column));
        }
        let line = if down { pos.line as usize + 1 } else { (pos.line as usize).wrapping_sub(1) };
        if line >= count {
            return pos;
        }
        let Some(doc) = self.doc(pane) else {
            return pos;
        };
        let text = doc.lines[line].text.clone();
        let ranges = ranges_of(self, &text);
        let target = if down { 0 } else { ranges.len() - 1 };
        TextPos::new(line as u32, wrap::offset_at_column(&text, ranges[target].clone(), column))
    }

    fn handle_key(&mut self, ctx: &egui::Context, key: Key, modifiers: Modifiers) {
        let Some((pane, head)) = self.caret else {
            return;
        };
        let extend = modifiers.shift;
        if matches!(key, Key::ArrowUp | Key::ArrowDown) {
            let target = self.moved_vertically(pane, head, key == Key::ArrowDown);
            self.set_caret(pane, target, extend);
            return;
        }
        let selected = self.selection.filter(|s| s.pane == pane && s.anchor != s.head).map(|s| s.ordered());
        let page = (self.view_lines() - 2.0).max(1.0) as u32;
        let Some(doc) = self.doc(pane) else {
            return;
        };
        if let Some(target) = caret_motion(doc, key, modifiers, head, selected, page) {
            self.goal_column = None;
            self.set_caret(pane, target, extend);
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
                    self.replace_range(ctx, pane, start, end, "", true);
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
                let end = textedit::end_of_document(doc);
                self.selection = Some(Selection { pane, anchor: TextPos::default(), head: end });
                self.caret = Some((pane, end));
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
                Event::Text(text) if !text.chars().any(char::is_control) => self.replace_selection(ctx, &text, true),
                Event::Ime(ImeEvent::Commit(text)) if !text.is_empty() => self.replace_selection(ctx, &text, true),
                Event::Paste(text) => self.replace_selection(ctx, &text, false),
                Event::Cut => {
                    if let Some(text) = self.selected_text() {
                        if self.caret.is_some_and(|(pane, _)| self.version(pane) == Version::Result) {
                            ctx.copy_text(text);
                        }
                        self.replace_selection(ctx, "", false);
                    }
                }
                Event::Key { key, pressed: true, modifiers, .. } => self.handle_key(ctx, key, modifiers),
                _ => {}
            }
        }
    }

    /// Puts the shared position where `pane` scrolls to `target` pixels.
    fn scroll_pane_to(&mut self, pane: usize, target: f64) {
        let line = self.layout.line_height as f64;
        let view = self.view_lines();
        let wanted = target / line;
        if wanted <= 0.0 {
            self.scroll = 0.0;
            self.update_pane_scroll();
            return;
        }
        let sync = &self.layout.sync;
        let (mut low, mut high) = (0.0, sync.max_scroll(view));
        for _ in 0..48 {
            let middle = (low + high) / 2.0;
            if sync.pane_scrolls(middle, view)[pane] < wanted {
                low = middle;
            } else {
                high = middle;
            }
        }
        self.scroll = high * line;
        self.update_pane_scroll();
    }

    /// The caret's pane, visual line and column.
    fn caret_place(&self) -> Option<(usize, u32, usize)> {
        let (pane, pos) = self.caret?;
        let text = self.doc(pane)?.lines.get(pos.line as usize)?.text.as_str();
        let ranges = self.visual_ranges(text);
        let row = ranges.iter().rposition(|range| range.start <= pos.byte).unwrap_or(0);
        let visual = self.layout.offsets[pane].get(pos.line as usize)? + row as u32;
        Some((pane, visual, wrap::column_of(text, ranges[row].start, pos.byte)))
    }

    #[cfg(test)]
    pub fn caret_in_view(&self) -> bool {
        let Some((pane, visual, _)) = self.caret_place() else {
            return false;
        };
        let line = self.layout.line_height as f64;
        let top = visual as f64 * line - self.pane_scroll[pane];
        top >= 0.0 && top + line <= self.view_lines() * line + 0.5
    }

    fn reveal_caret(&mut self) {
        let Some((pane, visual, column)) = self.caret_place() else {
            return;
        };
        let line = self.layout.line_height as f64;
        let view = self.view_lines() * line;
        let margin = (CARET_MARGIN_ROWS * line).min(view / 3.0);
        let top = visual as f64 * line;
        let scroll = self.pane_scroll[pane];
        let centered = std::mem::take(&mut self.center_caret);
        if centered && (top < scroll + margin || top + line > scroll + view - margin) {
            self.scroll_pane_to(pane, top - (view - line) / 2.0);
        } else if top < scroll + margin {
            self.scroll_pane_to(pane, top - margin);
        } else if top + line > scroll + view - margin {
            self.scroll_pane_to(pane, top + line + margin - view);
        }
        if let (false, Some(geometry)) = (self.layout.wrap, self.geometry) {
            let pane = geometry.panes[pane];
            let width = pane.text_right - pane.text_x;
            let x = column as f32 * geometry.char_width;
            if x < self.scroll_x {
                self.scroll_x = x;
            } else if x > self.scroll_x + width {
                self.scroll_x = x - width;
            }
        }
    }

    // --- Finding ------------------------------------------------------------------------------

    fn compute_matches(&mut self) {
        self.matches.clear();
        let (Some(filter), Some(merge)) = (&self.filter, &self.merge) else {
            return;
        };
        let versions = self.versions();
        let mut found: Vec<Found> = (0..3)
            .into_par_iter()
            .flat_map_iter(|pane| {
                let doc = merge.document(versions[pane]);
                let mut list = Vec::new();
                for (line, text) in doc.lines.iter().enumerate() {
                    for range in filter.find_ranges(&text.text) {
                        let chunk = merge.chunk_at(versions[pane], line as u32).unwrap_or(0);
                        // What the three texts share is counted once, in the centre.
                        let shared = merge.chunks()[chunk].kind == ChunkKind::Unchanged && merge.chunks()[chunk].resolution == Resolution::Merged;
                        if shared && pane != CENTRE {
                            continue;
                        }
                        list.push(Found { chunk: chunk as u32, pane, line: line as u32, start: range.start as u32, end: range.end as u32 });
                    }
                }
                list
            })
            .collect();
        found.sort_by_key(Found::key);
        self.matches = found;
    }

    fn reading_key(&self) -> (u32, usize, u32, u32) {
        let merge = self.merge.as_deref();
        let from_caret = self.caret.and_then(|(pane, pos)| {
            let start = self.selection.filter(|s| s.pane == pane).map_or(pos, |s| s.ordered().0);
            let chunk = merge?.chunk_at(self.version(pane), start.line)?;
            Some((chunk as u32, pane, start.line, start.byte as u32))
        });
        from_caret.unwrap_or_else(|| {
            let visual = (self.pane_scroll[CENTRE] / self.layout.line_height as f64) as u32;
            let line = line_at(&self.layout.offsets[CENTRE], visual) as u32;
            let chunk = merge.and_then(|merge| merge.chunk_at(self.version(CENTRE), line)).unwrap_or(0);
            (chunk as u32, 0, 0, 0)
        })
    }

    fn current_match(&self) -> Option<usize> {
        let selection = self.selection?;
        let (start, end) = selection.ordered();
        let index = self.matches.iter().position(|found| {
            found.pane == selection.pane && found.line == start.line && found.start as usize == start.byte
        })?;
        (start.line == end.line && self.matches[index].end as usize == end.byte).then_some(index)
    }

    fn show_match(&mut self, index: usize) {
        let found = self.matches[index];
        self.jump_to(found.pane, TextPos::new(found.line, found.start as usize), TextPos::new(found.line, found.end as usize));
    }

    // --- Layout -------------------------------------------------------------------------------

    fn ensure_layout(&mut self, columns: usize) {
        let versions = self.versions();
        let layout = &mut self.layout;
        if layout.valid && (!layout.wrap || layout.columns == columns) && layout.versions == Some(versions) {
            return;
        }
        let Some(merge) = &self.merge else {
            return;
        };
        let row_pixels = layout.line_height as f64;
        let place = self.pending_place.take().or_else(|| layout.valid.then(|| layout.sync.locate(self.scroll / row_pixels)));
        layout.valid = true;
        layout.columns = columns;
        layout.versions = Some(versions);
        let wrap_lines = layout.wrap;
        let docs = versions.map(|version| merge.document(version));
        let heights: Vec<Vec<u32>> = docs
            .par_iter()
            .map(|doc| doc.lines.par_iter().map(|l| if wrap_lines { wrap::visual_line_count(&l.text, columns) } else { 1 }).collect())
            .collect();
        layout.max_columns = if wrap_lines {
            0
        } else {
            docs.par_iter().map(|doc| doc.lines.par_iter().map(|l| wrap::display_width(&l.text)).max().unwrap_or(0)).max().unwrap_or(0)
        };
        let chunks = merge.chunks();
        layout.blocks = (0..chunks.len()).filter(|&index| chunks[index].kind != ChunkKind::Unchanged).collect();
        if layout.aligned {
            // A chunk takes as many rows as its longest pane; facing lines share a row, as tall
            // as the tallest of them.
            let mut row_heights = Vec::new();
            layout.chunk_rows = Vec::with_capacity(chunks.len() + 1);
            layout.line_rows = docs.map(|doc| Vec::with_capacity(doc.len()));
            for chunk in chunks {
                layout.chunk_rows.push(row_heights.len() as u32);
                let ranges = versions.map(|version| chunk.range(version));
                let count = ranges.iter().map(|range| range.len()).max().unwrap_or(0);
                for row in 0..count as u32 {
                    let height = (0..3)
                        .filter(|&pane| row < ranges[pane].len() as u32)
                        .map(|pane| heights[pane][(ranges[pane].start + row) as usize])
                        .max()
                        .unwrap_or(1);
                    for pane in 0..3 {
                        if row < ranges[pane].len() as u32 {
                            layout.line_rows[pane].push(row_heights.len() as u32);
                        }
                    }
                    row_heights.push(height);
                }
            }
            layout.chunk_rows.push(row_heights.len() as u32);
            layout.row_offsets = offsets_of(row_heights);
            let total = layout.row_offsets.last().copied().unwrap_or(0);
            layout.offsets = [0, 1, 2].map(|pane| {
                layout.line_rows[pane].iter().map(|&row| layout.row_offsets[row as usize]).chain(std::iter::once(total)).collect()
            });
            let rows = |chunk: usize| layout.chunk_rows[chunk]..layout.chunk_rows[chunk + 1];
            layout.sync = SyncMap::new(layout.blocks.iter().map(|&chunk| [rows(chunk), rows(chunk), rows(chunk)]), [&layout.row_offsets; 3]);
        } else {
            layout.row_offsets.clear();
            layout.chunk_rows.clear();
            layout.line_rows = Default::default();
            let mut heights = heights.into_iter();
            layout.offsets = [0, 1, 2].map(|_| offsets_of(heights.next().unwrap_or_default()));
            let offsets = &layout.offsets;
            layout.sync = SyncMap::new(
                layout.blocks.iter().map(|&chunk| versions.map(|version| chunks[chunk].range(version))),
                [&offsets[0], &offsets[1], &offsets[2]],
            );
        }
        if let Some(place) = place {
            self.scroll = layout.sync.position_of(place) * row_pixels;
        }
    }

    /// The visual lines a line itself takes, the gaps of the aligned mode excluded.
    fn line_extent(&self, pane: usize, line: usize) -> Range<u32> {
        let layout = &self.layout;
        let start = layout.offsets[pane][line];
        match layout.line_rows[pane].get(line) {
            Some(&row) if layout.aligned => start..layout.row_offsets[row as usize + 1],
            _ => start..layout.offsets[pane][line + 1],
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

    fn pane_at(&self, x: f32) -> Option<usize> {
        let geometry = self.geometry?;
        let cards = geometry.cards;
        Some(if x < (cards[0].right() + cards[1].left()) / 2.0 {
            LEFT
        } else if x < (cards[1].right() + cards[2].left()) / 2.0 {
            CENTRE
        } else {
            RIGHT
        })
    }

    fn hit_test(&self, pos: Pos2) -> Option<(usize, TextPos)> {
        let geometry = self.geometry?;
        let pane = self.pane_at(pos.x)?;
        let doc = self.doc(pane)?;
        if doc.is_empty() {
            return Some((pane, TextPos::default()));
        }
        let visual = ((pos.y - geometry.view.top()) as f64 + self.pane_scroll[pane]).max(0.0) / geometry.line_height as f64;
        let offsets = &self.layout.offsets[pane];
        let line = line_at(offsets, visual as u32).min(doc.len() - 1);
        let text = &doc.lines.get(line)?.text;
        let inside = (visual as u32).saturating_sub(offsets[line]);
        let range = self.visual_range(text, inside as usize);
        let column = ((pos.x - geometry.panes[pane].text_x + self.scroll_x) / geometry.char_width).max(0.0).round();
        Some((pane, TextPos { line: line as u32, byte: wrap::offset_at_column(text, range, column as usize) }))
    }

    /// The chunk under the pointer, framed or not.
    fn chunk_at_pos(&self, pos: Pos2) -> Option<usize> {
        let (pane, at) = self.hit_test(pos)?;
        self.merge.as_ref()?.chunk_at(self.version(pane), at.line)
    }

    fn place_caret(&mut self, pos: Pos2, extend: bool) {
        let Some((pane, at)) = self.hit_test(pos) else {
            return;
        };
        self.goal_column = None;
        self.set_caret(pane, at, extend);
        self.adopt_chunk_at(pane, at.line);
    }

    fn select_word(&mut self, pos: Pos2) {
        let Some((pane, at)) = self.hit_test(pos) else {
            return;
        };
        let Some(doc) = self.doc(pane) else {
            return;
        };
        let (start, end) = textedit::word_at(doc, at);
        self.goal_column = None;
        self.caret = Some((pane, end));
        self.selection = Some(Selection { pane, anchor: start, head: end });
    }

    fn extend_selection(&mut self, pos: Pos2) {
        let Some(selection) = self.selection else {
            return;
        };
        if let Some((pane, head)) = self.hit_test(pos).filter(|(pane, _)| *pane == selection.pane) {
            self.selection = Some(Selection { head, ..selection });
            self.caret = Some((pane, head));
            self.caret_to_reveal = true;
        }
    }

    /// Word spans against the base for the chunks in view, worked out once.
    fn prepare_spans(&mut self, geometry: Geometry) {
        let Some(merge) = &self.merge else {
            return;
        };
        let line = geometry.line_height as f64;
        for pane in 0..3 {
            let version = self.version(pane);
            if version == Version::Base {
                continue;
            }
            let doc = merge.document(version);
            if doc.is_empty() {
                continue;
            }
            let offsets = &self.layout.offsets[pane];
            let first = line_at(offsets, (self.pane_scroll[pane] / line) as u32).min(doc.len() - 1);
            let last = line_at(offsets, ((self.pane_scroll[pane] + geometry.view.height() as f64) / line) as u32).min(doc.len() - 1);
            let (Some(from), Some(to)) = (merge.chunk_at(version, first as u32), merge.chunk_at(version, last as u32)) else {
                continue;
            };
            for index in from..=to {
                let chunk = &merge.chunks()[index];
                let worth = chunk.kind != ChunkKind::Unchanged || chunk.resolution == Resolution::Edited;
                if !worth || self.spans.contains_key(&(index, version)) {
                    continue;
                }
                let range = chunk.range(version);
                let mut lines = vec![Vec::new(); range.len()];
                if range.len() <= MAX_SPAN_LINES && chunk.base.len() <= MAX_SPAN_LINES && !chunk.base.is_empty() {
                    let compared = merge.compare(index, Version::Base, version);
                    for row in &compared.rows {
                        if let (Some(at), Some(pair)) = (row.right, row.pair) {
                            if let (Some(slot), Some(spans)) = (lines.get_mut((at - range.start) as usize), compared.spans.get(pair as usize)) {
                                *slot = spans.right.clone();
                            }
                        }
                    }
                }
                self.spans.insert((index, version), Arc::new(lines));
            }
        }
    }

    // --- Painting -----------------------------------------------------------------------------

    /// The zone colour of a chunk in a pane: what changed is tinted where it shows, conflicts in
    /// their own colour, calmer once resolved.
    fn tint(&self, chunk: &Chunk, version: Version, doc: &DocPalette) -> Option<Tint> {
        let one_sided = |range: &Range<u32>| {
            if chunk.base.is_empty() {
                doc.insert
            } else if range.is_empty() {
                doc.delete
            } else {
                doc.change
            }
        };
        match chunk.kind {
            ChunkKind::Unchanged => (version == Version::Result && chunk.resolution != Resolution::Merged).then_some(doc.change),
            ChunkKind::Conflict => Some(if chunk.is_unresolved() { doc.conflict } else { doc.resolved }),
            kind => {
                let left = kind != ChunkKind::RightOnly;
                let right = kind != ChunkKind::LeftOnly;
                let side = if left { &chunk.left } else { &chunk.right };
                match version {
                    Version::Left if left => Some(one_sided(&chunk.left)),
                    Version::Right if right => Some(one_sided(&chunk.right)),
                    // The side that kept the base shows what the other one removes.
                    Version::Left | Version::Right => side.is_empty().then_some(doc.delete),
                    Version::Result if chunk.resolution == Resolution::Merged => Some(one_sided(side)),
                    Version::Result => Some(doc.change),
                    Version::Base => Some(one_sided(side)),
                }
            }
        }
    }

    fn paint_rows(&self, ui: &Ui, painter: &Painter, geometry: Geometry, look: &Look) {
        let Some(merge) = &self.merge else {
            return;
        };
        let doc = &look.doc;
        let above = (geometry.view.top() - geometry.rows.top()) as f64;
        let below = (geometry.rows.bottom() - geometry.view.bottom()) as f64;
        let view = geometry.view;
        let line_height = geometry.line_height;
        let number_font = FontId::new(self.code_size - 1.0, FontFamily::Monospace);
        let code_font = FontId::new(self.code_size, FontFamily::Monospace);
        let found_filter = self.filter.as_ref();
        for (pane_index, pane) in geometry.panes.iter().enumerate() {
            let version = self.version(pane_index);
            let document = merge.document(version);
            let clip_rect = Rect::from_x_y_ranges(pane.rect.x_range(), geometry.rows.y_range());
            let clip = painter.with_clip_rect(clip_rect);
            let text_clip = painter.with_clip_rect(Rect::from_min_max(
                pos2(pane.text_x - TEXT_PADDING, clip_rect.top()),
                pos2(pane.text_right + WRAP_MARGIN, clip_rect.bottom()),
            ));
            let scroll = self.pane_scroll[pane_index];
            let offsets = &self.layout.offsets[pane_index];
            let first = ((scroll - above).max(0.0) / line_height as f64) as u32;
            let last = ((scroll + view.height() as f64 + below) / line_height as f64) as u32 + 1;
            let styles = &self.styles[slot(version)];
            let mut chunk_index = merge.chunk_at(version, line_at(offsets, first) as u32).unwrap_or(0);
            for line in line_at(offsets, first)..document.len() {
                let start = offsets[line];
                if start >= last {
                    break;
                }
                while merge.chunks()[chunk_index].range(version).end as usize <= line {
                    chunk_index += 1;
                }
                let chunk = &merge.chunks()[chunk_index];
                let extent = self.line_extent(pane_index, line);
                let top = snap(painter, view.top() as f64 + start as f64 * line_height as f64 - scroll);
                let bottom = top + extent.len() as f32 * line_height;
                let rect = Rect::from_x_y_ranges(pane.rect.x_range(), top..=bottom);
                let gutter = Rect::from_x_y_ranges(pane.rect.left()..=pane.text_x - TEXT_PADDING, top..=bottom);
                let tint = self.tint(chunk, version, doc);
                if let Some(tint) = tint {
                    let current = self.current == Some(chunk_index);
                    clip.rect_filled(rect, 0.0, if current { tint.current } else { tint.zone });
                    if tint.gutter != tint.zone {
                        clip.rect_filled(gutter, 0.0, tint.gutter);
                    }
                    let bar_x = panes::line_bar_x(look, pane.rect, gutter.right() - GUTTER_PADDING);
                    panes::paint_line_bar(&clip, look, bar_x, top, bottom, tint.mark);
                }
                clip.text(
                    pos2(gutter.right() - GUTTER_PADDING, top + line_height / 2.0),
                    Align2::RIGHT_CENTER,
                    line + 1,
                    number_font.clone(),
                    if tint.is_some() { doc.line_number_on } else { doc.line_number },
                );
                let text = document.lines[line].text.as_str();
                let spans = self.spans.get(&(chunk_index, version));
                let spans = spans.and_then(|spans| spans.get(line - chunk.range(version).start as usize)).map_or(&[][..], |s| &s[..]);
                let found = found_filter.map(|filter| filter.find_ranges(text)).unwrap_or_default();
                let caret = self.caret.filter(|&(pane, pos)| pane == pane_index && pos.line == line as u32).map(|(_, pos)| pos.byte);
                paint_text_line(
                    ui,
                    &text_clip,
                    &LinePaint {
                        text,
                        styles: styles.get(line).and_then(Option::as_ref).map_or(&[][..], |s| &s[..]),
                        spans,
                        ignored: &[],
                        whole: None,
                        selected: self.selection.and_then(|s| s.bytes_on(pane_index, line as u32, text.len())),
                        found: &found,
                        caret,
                        wrap: self.layout.wrap.then_some(self.layout.columns),
                        scroll_x: self.scroll_x,
                        text_x: pane.text_x,
                        text_right: pane.text_right,
                        visible_right: pane.rect.right(),
                        top,
                        line_height,
                        char_width: geometry.char_width,
                        font: &code_font,
                        doc,
                    },
                );
            }
        }
    }

    /// Frames or ribbons, links, the arrows that take a side and the action button of the chunk
    /// being worked on, or of the one under the pointer. Returns what was clicked, and the
    /// action buttons whose menu may be open.
    fn paint_frames(&self, ui: &Ui, painter: &Painter, geometry: Geometry, look: &Look) -> (Option<ChunkAction>, Vec<(usize, egui::Response)>) {
        let doc = &look.doc;
        let mut clicked = None;
        let mut menus = Vec::new();
        let Some(merge) = &self.merge else {
            return (clicked, menus);
        };
        let view = geometry.view;
        let line_height = geometry.line_height as f64;
        let clip = painter.with_clip_rect(geometry.rows);
        let bottom_edge = clip.clip_rect().bottom();
        let versions = self.versions();
        let y_at = |pane: usize, visual: u32| snap(painter, view.top() as f64 + visual as f64 * line_height - self.pane_scroll[pane]);
        let y_of = |pane: usize, line: u32| y_at(pane, self.layout.offsets[pane].get(line as usize).copied().unwrap_or(0));
        let chunks = merge.chunks();
        let first = self.layout.blocks.partition_point(|&chunk| {
            (0..3).map(|pane| y_of(pane, chunks[chunk].range(versions[pane]).end)).fold(f32::MIN, f32::max) < geometry.rows.top()
        });
        let editing = !self.show_base;
        let ribbons = look.links == LinkStyle::Ribbons;
        let cards = geometry.cards;
        let gaps = [(cards[LEFT].right(), cards[CENTRE].left()), (cards[CENTRE].right(), cards[RIGHT].left())];
        let pointer = ui.ctx().pointer_hover_pos().filter(|_| ui.rect_contains_pointer(geometry.rows));
        for &index in &self.layout.blocks[first..] {
            let chunk = &chunks[index];
            let ranges = versions.map(|version| chunk.range(version));
            if (0..3).all(|pane| y_of(pane, ranges[pane].start) > bottom_edge) {
                break;
            }
            let current = self.current == Some(index);
            let unresolved = chunk.is_unresolved();
            let tint = self.tint(chunk, Version::Result, doc).unwrap_or(doc.change);
            let stroke = match (current, unresolved) {
                (true, _) => Stroke::new(1.5, tint.mark),
                (false, true) => Stroke::new(1.0, mix(doc.frame, doc.conflict.mark, 0.7)),
                (false, false) => Stroke::new(1.0, doc.frame),
            };
            let mut anchors = [0.0; 3];
            let mut spans = [(0.0, 0.0); 3];
            let mut tops = [0.0; 3];
            let mut hovered = false;
            for pane in 0..3 {
                let rect = geometry.panes[pane].rect;
                let frame_top = y_of(pane, ranges[pane].start);
                if ranges[pane].is_empty() {
                    // No line here: the chunk is a rule between two lines.
                    let visual = match self.layout.chunk_rows.get(index) {
                        Some(&row) if self.layout.aligned => y_at(pane, self.layout.row_offsets[row as usize]),
                        _ => frame_top,
                    };
                    let y = clip.round_to_pixel_center(visual);
                    let rule = if ribbons { Stroke::new(if current { 1.5 } else { 1.0 }, tint.mark) } else { stroke };
                    clip.hline(rect.x_range(), y, rule);
                    anchors[pane] = y;
                    spans[pane] = (y, y);
                    tops[pane] = (y - ARROW_SIZE.y / 2.0).max(view.top() + 2.0);
                    hovered |= pointer.is_some_and(|p| rect.x_range().contains(p.x) && (p.y - y).abs() <= 4.0);
                } else {
                    let last = ranges[pane].end as usize - 1;
                    let frame_bottom = y_at(pane, self.line_extent(pane, last).end);
                    let frame = Rect::from_x_y_ranges(rect.x_range(), frame_top..=frame_bottom);
                    if !ribbons {
                        clip.rect_stroke(frame, 0.0, stroke, StrokeKind::Inside);
                    }
                    let (low, high) = (frame_top.max(view.top()), frame_bottom.min(view.bottom()));
                    anchors[pane] = if low < high { (low + high) / 2.0 } else { (frame_top + frame_bottom) / 2.0 };
                    spans[pane] = (frame_top, frame_bottom);
                    let inset = (geometry.line_height - ARROW_SIZE.y) / 2.0;
                    tops[pane] = (frame_top + inset).max(view.top() + 2.0).min(frame_bottom - ARROW_SIZE.y - inset);
                    hovered |= pointer.is_some_and(|p| frame.contains(p));
                }
            }
            let mark = self.tint(chunk, Version::Result, &look.gap).unwrap_or(look.gap.change).mark;
            for (gap, (from, to)) in gaps.into_iter().zip([(LEFT, CENTRE), (CENTRE, RIGHT)]) {
                if ribbons {
                    panes::paint_ribbon(&clip, gap, spans[from], spans[to], mark, look.ui.ribbon, current);
                } else {
                    let link = if current || unresolved { stroke } else { Stroke::new(1.0, look.gap.frame) };
                    paint_link(&clip, pos2(gap.0, anchors[from]), pos2(gap.1, anchors[to]), link);
                }
                hovered |= pointer.is_some_and(|p| panes::ribbon_contains(gap, spans[from], spans[to], p));
            }
            if !editing {
                continue;
            }
            let left_rect = panes::arrow_rect(look, gaps[0], true, anchors[LEFT]);
            let right_rect = panes::arrow_rect(look, gaps[1], false, anchors[RIGHT]);
            let menu_rect = Rect::from_min_size(pos2(geometry.panes[CENTRE].rect.right() - ARROW_SIZE.x - 7.0, tops[CENTRE]), ARROW_SIZE);
            let combine_rect = menu_rect.translate(vec2(-(ARROW_SIZE.x + 4.0), 0.0));
            hovered |= pointer.is_some_and(|p| [left_rect, right_rect, menu_rect, combine_rect].iter().any(|rect| rect.contains(p)));
            // Buttons for the chunk being worked on, and for the one under the pointer.
            if !(current || hovered) {
                continue;
            }
            let fits = |rect: Rect| rect.top() >= view.top() && rect.bottom() <= view.bottom();
            let same = |version: Version| lines_equal(merge, chunk, version, Version::Result);
            // Taking a side is offered where it would change the result.
            if fits(left_rect) && !same(Version::Left) {
                let id = ui.scope_id().with(("take-left", index));
                let tip = "Prendre la gauche (Ctrl+→)";
                if panes::arrow_button(ui, &clip, look, left_rect, id, false, tip).on_hover_text(tip).clicked() {
                    clicked = Some(ChunkAction::Resolve(index, Resolution::Left));
                }
            }
            if fits(right_rect) && !same(Version::Right) {
                let id = ui.scope_id().with(("take-right", index));
                let tip = "Prendre la droite (Ctrl+←)";
                if panes::arrow_button(ui, &clip, look, right_rect, id, true, tip).on_hover_text(tip).clicked() {
                    clicked = Some(ChunkAction::Resolve(index, Resolution::Right));
                }
            }
            if fits(menu_rect) {
                let id = ui.scope_id().with(("chunk-menu", index));
                let response = block_button(ui, &clip, menu_rect, id, Icon::Ellipsis, false, "Actions du bloc", tint, doc);
                let response = response.on_hover_ui(|ui| peek(ui, merge, index, doc));
                menus.push((index, response));
                if unresolved && chunk.combinable {
                    let id = ui.scope_id().with(("combine", index));
                    let response = block_button(ui, &clip, combine_rect, id, Icon::Combine, false, "Combiner les deux côtés", tint, doc)
                        .on_hover_ui(|ui| proposal(ui, merge, index, doc));
                    if response.clicked() {
                        clicked = Some(ChunkAction::Resolve(index, Resolution::Merged));
                    }
                }
            }
        }
        (clicked, menus)
    }

    fn paint_overview(&mut self, ui: &mut Ui, painter: &Painter, geometry: Geometry, look: &Look) {
        let rect = geometry.overview;
        let total = (self.layout.sync.total() as f32).max(1.0);
        let scale = rect.height() / total;
        let mut marks = Vec::new();
        if let Some(merge) = &self.merge {
            for (&index, extent) in self.layout.blocks.iter().zip(&self.layout.sync.blocks) {
                let chunk = &merge.chunks()[index];
                let mark = self.tint(chunk, Version::Result, &look.gap).unwrap_or(look.gap.change).mark;
                marks.push((rect.top() + extent.start as f32 * scale, rect.top() + extent.end as f32 * scale, mark));
            }
        }
        let line = geometry.line_height;
        let thumb_top = rect.top() + (self.scroll as f32 / line) * scale;
        let thumb_height = (geometry.view.height() / line * scale).max(14.0);
        let thumb = Rect::from_min_size(pos2(rect.left(), thumb_top), vec2(rect.width(), thumb_height.min(rect.height())));
        panes::paint_overview(painter, look, rect, &marks, thumb);
        let response = ui.interact(rect, ui.scope_id().with("merge-overview"), Sense::click_and_drag());
        if let Some(pos) = response.interact_pointer_pos() {
            let ratio = ((pos.y - rect.top() - thumb_height / 2.0) / rect.height()).max(0.0);
            let max = self.layout.sync.max_scroll((geometry.view.height() / line) as f64) * line as f64;
            self.scroll = (ratio as f64 * total as f64 * line as f64).min(max);
        }
    }

    /// The change being worked on, counted from one, and how many there are.
    pub fn block_rank(&self) -> Option<(usize, usize)> {
        let current = self.current?;
        let rank = self.layout.blocks.binary_search(&current).ok()?;
        Some((rank + 1, self.layout.blocks.len()))
    }

    fn handle_scroll(&mut self, ui: &Ui, response: &egui::Response, geometry: Geometry) {
        let line = geometry.line_height as f64;
        let page = (geometry.view.height() as f64 - line * 2.0).max(line);
        let max_y = self.layout.sync.max_scroll(geometry.view.height() as f64 / line) * line;
        let pane = geometry.panes[0];
        let width = self.layout.max_columns as f32 * geometry.char_width;
        let max_x = if self.layout.wrap { 0.0 } else { (width - (pane.text_right - pane.text_x)).max(0.0) };
        self.scroller.update(
            ui,
            response.hovered() || ui.rect_contains_pointer(geometry.overview),
            Position { y: &mut self.scroll, x: &mut self.scroll_x, max_y, max_x, row_height: geometry.line_height },
        );
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

    fn headers(&mut self, ui: &mut Ui, look: &Look, headers: [Rect; 3]) -> bool {
        let hues = look.ui.hues;
        let mut save = false;
        let Some(merge) = &self.merge else {
            return save;
        };
        let dirty = self.is_dirty();
        let unresolved = merge.unresolved_count();
        for pane in 0..3 {
            let version = self.version(pane);
            let header = headers[pane];
            let (path, title) = self.pane_identity(merge, version);
            let mut badges = Vec::new();
            if merge.is_missing(version) {
                let tip = match version {
                    Version::Base => "Pas d'ancêtre commun : le fichier a été ajouté des deux côtés",
                    _ => "Ce fichier n'existe pas de ce côté : supprimé ici, ou ajouté de l'autre côté",
                };
                badges.push(Badge::new("absent", tip).warning());
            }
            if version == Version::Result && unresolved > 0 {
                let text = if unresolved == 1 { "1 conflit".to_owned() } else { format!("{unresolved} conflits") };
                badges.push(Badge::new(text, "Conflits non résolus : F8 mène au suivant").error());
            }
            if version == Version::Base && !merge.base_known() {
                badges.push(Badge::new("inconnu", "Le fichier en conflit n'a pas de section ||||||| : l'ancêtre n'est pas connu"));
            }
            let mut input = path.display().to_string();
            let tint = match version {
                Version::Left => hues.violet,
                Version::Result => hues.pink,
                Version::Base => hues.slate,
                Version::Right => hues.green,
            };
            let action = widgets::path_bar(
                ui,
                look,
                ui.scope_id().with(("merge-header", pane)),
                header,
                PathBar {
                    path: &path,
                    input: &mut input,
                    icon: if version == Version::Result { Icon::GitMerge } else if version == Version::Base { Icon::History } else { Icon::File },
                    tint,
                    dirty: version == Version::Result && dirty,
                    label: ["Version locale", if self.show_base { "Ancêtre commun" } else { "Résultat" }, "Version distante"][pane],
                    browse_tip: "",
                    title: Some(&title),
                    read_only: version != Version::Result,
                    badges: &badges,
                    fixed: true,
                },
            );
            save |= matches!(action, PathAction::Save);
        }
        save
    }

    /// The file a pane stands for and the name shown for it.
    fn pane_identity(&self, merge: &Merge, version: Version) -> (PathBuf, String) {
        let name_of = |path: &Path| path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        match &self.inputs {
            MergeInputs::Files { base, local, remote, merged, labels } => {
                let (path, label) = match version {
                    Version::Base => (base, &labels[0]),
                    Version::Left => (local, &labels[1]),
                    Version::Right => (remote, &labels[2]),
                    Version::Result => (merged, &None),
                };
                let title = match (label, version) {
                    (Some(label), _) => label.clone(),
                    (None, Version::Result) => format!("{} (résultat)", name_of(path)),
                    (None, Version::Base) => format!("{} (ancêtre commun)", name_of(path)),
                    (None, _) => name_of(path),
                };
                (path.clone(), title)
            }
            MergeInputs::Conflicted(path) => {
                let name = name_of(path);
                let marker = |label: &str, what: &str| if label.is_empty() { format!("{name} ({what})") } else { format!("{name} ({what} : {label})") };
                let title = match version {
                    Version::Base => marker(&merge.labels.base, "ancêtre commun"),
                    Version::Left => marker(&merge.labels.left, "locale"),
                    Version::Right => marker(&merge.labels.right, "distante"),
                    Version::Result => format!("{name} (résultat)"),
                };
                (path.clone(), title)
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui, look: &Look, code_size: f32) {
        let chrome = &look.ui;
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.set_code_size(code_size);
        let available = ui.available_rect_before_wrap();
        ui.allocate_rect(available, Sense::hover());
        let painter = ui.painter_at(available);
        painter.rect_filled(available, 0.0, chrome.background);

        let (laid, overview) = panes::layout(available, look, 3, true);
        let laid = [laid[0], laid[1], laid[2]];
        panes::paint_cards(&painter, look, &laid);
        let cards = laid.map(|pane| pane.card);
        if self.headers(ui, look, laid.map(|pane| pane.header)) {
            if let Err(error) = self.save() {
                self.message = Some(error);
            }
        }
        let rows = Rect::from_min_max(
            pos2(cards[0].left(), laid[0].code.top()),
            pos2(cards[2].right(), panes::row_clip(look, laid[0].code).bottom()),
        );
        let view = Rect::from_min_max(
            pos2(rows.left(), rows.top() + CODE_PADDING),
            pos2(rows.right(), laid[0].code.bottom() - CODE_PADDING),
        );
        let font = FontId::new(code_size, FontFamily::Monospace);
        let char_width = ctx.fonts_mut(|fonts| fonts.glyph_width(&font, '0')).max(1.0);
        let longest = (0..3).filter_map(|pane| self.doc(pane).map(Document::len)).max().unwrap_or(0);
        let digits = (longest.max(1).ilog10() as usize + 1).max(3);
        let gutter = digits as f32 * char_width + GUTTER_PADDING * 2.0 + if look.metrics.bar_at_edge { 4.0 } else { 0.0 };
        // The arrows taking a side stand in the gaps; the centre pane keeps room on its right
        // for the action buttons of its blocks.
        let make_pane = |pane: usize| {
            let rect = Rect::from_x_y_ranges(cards[pane].x_range(), view.y_range());
            let right_room = if pane == CENTRE { ARROW_MARGIN } else { WRAP_MARGIN };
            Pane { rect, text_x: rect.left() + gutter + TEXT_PADDING, text_right: rect.right() - right_room }
        };
        let panes = [make_pane(LEFT), make_pane(CENTRE), make_pane(RIGHT)];
        let geometry = Geometry {
            view,
            rows,
            panes,
            cards,
            overview,
            line_height: self.layout.line_height,
            char_width,
        };
        let columns = (((panes[1].text_right - panes[1].text_x) / char_width) as usize).max(8);
        self.geometry = Some(geometry);
        if self.merge.is_none() {
            panes::finish_cards(&painter, look, &laid);
            let (text, color) = match &self.error {
                Some(error) => (error.as_str(), chrome.error),
                None => ("Fusion des trois versions…", chrome.text_dim),
            };
            painter.text(view.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), color);
            return;
        }
        self.handle_editing(&ctx);
        self.ensure_layout(columns);
        if let Some((pane, line, on_screen)) = self.anchor.take() {
            if let Some(&visual) = self.layout.offsets[pane].get(line as usize) {
                self.scroll_pane_to(pane, visual as f64 * geometry.line_height as f64 - on_screen);
            }
        }
        if std::mem::take(&mut self.reveal_current) {
            self.update_pane_scroll();
            if let Some(chunk) = self.current.filter(|&chunk| !self.chunk_in_view(chunk)) {
                self.select_chunk(chunk);
            }
        }
        if std::mem::take(&mut self.jump_to_first) {
            self.update_pane_scroll();
            // Straight to work: the first conflict, or else the first change.
            let first = self.merge.as_ref().and_then(|merge| merge.next(None, Stop::Unresolved).or_else(|| merge.next(None, Stop::Change)));
            if let Some(chunk) = first {
                self.select_chunk(chunk);
            }
        }

        let response = ui.interact(view, ui.scope_id().with("merge-view"), Sense::click_and_drag());
        self.handle_scroll(ui, &response, geometry);
        self.update_pane_scroll();
        if let Some(pos) = response.interact_pointer_pos() {
            if response.secondary_clicked() {
                self.menu_chunk = self.chunk_at_pos(pos);
            } else if response.double_clicked() {
                self.select_word(pos);
            } else if response.drag_started_by(PointerButton::Primary) || response.clicked() {
                self.place_caret(pos, ui.input(|i| i.modifiers.shift));
            } else if response.dragged_by(PointerButton::Primary) {
                self.extend_selection(pos);
            }
        }
        if std::mem::take(&mut self.caret_to_reveal) {
            self.reveal_caret();
            self.update_pane_scroll();
        }
        if let Some((index, visual, column)) = self.caret_place() {
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
        if response.hovered() {
            ctx.set_cursor_icon(CursorIcon::Text);
        }

        self.prepare_spans(geometry);
        self.paint_rows(ui, &painter, geometry, look);
        let (mut clicked, menus) = self.paint_frames(ui, &painter, geometry, look);
        self.paint_overview(ui, &painter, geometry, look);
        panes::finish_cards(&painter, look, &laid);

        if let Some(merge) = &self.merge {
            for (chunk, button) in &menus {
                Popup::menu(button).frame(menu_frame(chrome)).show(|ui| {
                    if let Some(action) = chunk_menu(ui, chrome, merge, *chunk) {
                        clicked = Some(action);
                    }
                });
            }
            if let Some(chunk) = self.menu_chunk.filter(|&chunk| merge.chunks().get(chunk).is_some_and(|c| c.kind != ChunkKind::Unchanged) && !self.show_base) {
                Popup::context_menu(&response).frame(menu_frame(chrome)).show(|ui| {
                    if let Some(action) = chunk_menu(ui, chrome, merge, chunk) {
                        clicked = Some(action);
                    }
                });
            }
        }
        if let Some(action) = clicked {
            self.apply(&ctx, action);
        }
        if self.question.is_some() {
            self.show_question(&ctx, chrome);
        }
    }

    pub fn status_items(&self) -> Vec<StatusItem> {
        let Some(merge) = &self.merge else {
            return Vec::new();
        };
        let mut items = Vec::new();
        let (pane, line, column) = match self.caret {
            Some((pane, pos)) => {
                let text = self.doc(pane).and_then(|doc| doc.lines.get(pos.line as usize)).map_or("", |l| l.text.as_str());
                (pane, pos.line as usize, wrap::column_of(text, 0, pos.byte) + 1)
            }
            None => {
                let visual = self.pane_scroll[CENTRE] / self.layout.line_height as f64;
                (CENTRE, line_at(&self.layout.offsets[CENTRE], visual as u32), 1)
            }
        };
        let doc = merge.document(self.version(pane));
        if let Some(syntax) = &self.syntax {
            items.push(StatusItem::Pill(syntax.clone()));
        }
        items.push(StatusItem::Pill(merge.result.encoding.label().to_owned()));
        items.push(StatusItem::Text(format!("Ligne {} sur {} · Colonne {column}", line + 1, doc.len().max(1))));
        if let Some(index) = self.current {
            let chunk = &merge.chunks()[index];
            let blocks = &self.layout.blocks;
            let rank = blocks.binary_search(&index).map_or(String::new(), |rank| format!("Bloc {} sur {} : ", rank + 1, blocks.len()));
            items.push(StatusItem::Text(format!("{rank}{}", describe(chunk))));
        }
        let counts = merge.counts();
        let mut merged = Vec::new();
        if counts.left > 0 {
            merged.push(format!("{} de gauche", counts.left));
        }
        if counts.right > 0 {
            merged.push(format!("{} de droite", counts.right));
        }
        if counts.both > 0 {
            merged.push(format!("{} des deux", counts.both));
        }
        if !merged.is_empty() {
            items.push(StatusItem::Text(format!("Fusion auto : {}", merged.join(", "))));
        }
        items.push(StatusItem::Conflicts { unresolved: counts.unresolved, total: counts.conflicts });
        items
    }
}

/// How a chunk stands, for the status bar.
fn describe(chunk: &Chunk) -> String {
    let resolution = match chunk.resolution {
        Resolution::Unresolved => "non résolu",
        Resolution::Merged if chunk.is_conflict() => "résolu en combinant les deux côtés",
        Resolution::Merged => "fusionné automatiquement",
        Resolution::Base => "résolu par la base",
        Resolution::Left => "résolu par la gauche",
        Resolution::Right => "résolu par la droite",
        Resolution::LeftThenRight => "résolu par la gauche puis la droite",
        Resolution::RightThenLeft => "résolu par la droite puis la gauche",
        Resolution::Edited if chunk.has_markers => "édité, marqueurs restants",
        Resolution::Edited => "édité à la main",
    };
    let kind = match chunk.kind {
        ChunkKind::Conflict => "conflit",
        ChunkKind::LeftOnly => "changé à gauche",
        ChunkKind::RightOnly => "changé à droite",
        ChunkKind::Both => "même changement des deux côtés",
        ChunkKind::Unchanged => "inchangé",
    };
    format!("{kind}, {resolution}")
}

fn offsets_of(heights: Vec<u32>) -> Vec<u32> {
    let mut offsets = Vec::with_capacity(heights.len() + 1);
    let mut total = 0;
    for height in heights {
        offsets.push(total);
        total += height;
    }
    offsets.push(total);
    offsets
}

/// The chunk holds the same text in both versions.
fn lines_equal(merge: &Merge, chunk: &Chunk, a: Version, b: Version) -> bool {
    let lines = |version: Version| {
        let range = chunk.range(version);
        merge.document(version).lines[range.start as usize..range.end as usize].iter().map(|line| &line.text)
    };
    chunk.range(a).len() == chunk.range(b).len() && lines(a).eq(lines(b))
}

/// The actions of a chunk's menu. Returns the one picked.
fn chunk_menu(ui: &mut Ui, chrome: &UiPalette, merge: &Merge, index: usize) -> Option<ChunkAction> {
    widgets::menu_width(ui, 280.0);
    let chunk = &merge.chunks()[index];
    let item = |ui: &mut Ui, label: &str, keys: &str, resolution: Resolution, icon: Option<Icon>| {
        let mut entry = Item::new(label).keys(keys).checked(chunk.resolution == resolution && !chunk.has_markers);
        if let Some(icon) = icon {
            entry = entry.icon(icon);
        }
        let clicked = entry.show(ui, chrome).clicked();
        if clicked {
            ui.close();
        }
        clicked.then_some(ChunkAction::Resolve(index, resolution))
    };
    let mut picked = None;
    picked = picked.or(item(ui, "Prendre la gauche", "Ctrl+→", Resolution::Left, Some(Icon::ArrowRight)));
    picked = picked.or(item(ui, "Prendre la droite", "Ctrl+←", Resolution::Right, Some(Icon::ArrowLeft)));
    picked = picked.or(item(ui, "Prendre gauche puis droite", "", Resolution::LeftThenRight, Some(Icon::Align)));
    picked = picked.or(item(ui, "Prendre droite puis gauche", "", Resolution::RightThenLeft, Some(Icon::Align)));
    if merge.base_known() {
        picked = picked.or(item(ui, "Prendre la base", "", Resolution::Base, Some(Icon::History)));
    }
    if chunk.combinable {
        picked = picked.or(item(ui, "Combiner", "", Resolution::Merged, Some(Icon::Combine)));
    }
    separator(ui, chrome);
    if chunk.is_conflict() {
        if Item::new("Éditer avec marqueurs").icon(Icon::FileText).checked(chunk.has_markers).show(ui, chrome).clicked() {
            picked = Some(ChunkAction::Markers(index));
            ui.close();
        }
        picked = picked.or(item(ui, "Revenir à non résolu", "", Resolution::Unresolved, Some(Icon::Undo)));
    } else {
        picked = picked.or(item(ui, "Revenir à la fusion automatique", "", Resolution::Merged, Some(Icon::Undo)));
    }
    picked
}

/// A conflict's base, in the tooltip of its action button.
fn peek(ui: &mut Ui, merge: &Merge, index: usize, doc: &DocPalette) {
    let chunk = &merge.chunks()[index];
    let state = describe(chunk);
    let mut chars = state.chars();
    let state: String = chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default();
    ui.label(RichText::new(state).strong());
    if !merge.base_known() {
        ui.label("Ancêtre commun inconnu (le fichier n'a pas de section |||||||)");
        return;
    }
    let base = &merge.base.lines[chunk.base.start as usize..chunk.base.end as usize];
    if base.is_empty() {
        ui.label("Ancêtre commun : aucune ligne (ajout)");
    } else {
        ui.label(format!("Ancêtre commun, ligne{} {} :", if base.len() > 1 { "s" } else { "" }, chunk.base.start + 1));
        lines_block(ui, base.iter().map(|line| line.text.as_str()), base.len(), doc);
    }
    if chunk.combinable && chunk.is_unresolved() {
        ui.add_space(4.0);
        proposal(ui, merge, index, doc);
    }
}

/// What combining both sides would give.
fn proposal(ui: &mut Ui, merge: &Merge, index: usize, doc: &DocPalette) {
    let lines = merge.resolution_lines(index, Resolution::Merged);
    ui.label("Proposition combinant les deux côtés :");
    lines_block(ui, lines.iter().map(String::as_str), lines.len(), doc);
}

fn lines_block<'a>(ui: &mut Ui, lines: impl Iterator<Item = &'a str>, count: usize, doc: &DocPalette) {
    let mut text: Vec<&str> = lines.take(PEEK_LINES).collect();
    let more = count.saturating_sub(text.len());
    let shown;
    if more > 0 {
        shown = format!("… {more} ligne{} de plus", if more > 1 { "s" } else { "" });
        text.push(&shown);
    }
    egui::Frame::new().fill(doc.canvas).corner_radius(6).inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.label(RichText::new(text.join("\n")).font(FontId::monospace(12.0)).color(doc.text));
    });
}

impl SearchTarget for MergeTab {
    fn query(&mut self) -> &mut String {
        &mut self.filter_query
    }

    fn options(&mut self) -> &mut FilterOptions {
        &mut self.filter_options
    }

    fn search_mode(&self) -> SearchMode {
        SearchMode::Find
    }

    fn set_search_mode(&mut self, _: SearchMode) {}

    fn can_filter(&self) -> bool {
        false
    }

    fn apply_search(&mut self) {
        match Filter::phrase(&self.filter_query, self.filter_options) {
            Ok(filter) => {
                self.filter = filter;
                self.filter_error = None;
            }
            Err(error) => {
                self.filter = None;
                self.filter_error = Some(error);
            }
        }
        self.compute_matches();
        if !self.matches.is_empty() {
            let key = self.reading_key();
            let next = self.matches.partition_point(|found| found.key() < key);
            self.show_match(next % self.matches.len());
        }
    }

    fn find_step(&mut self, forward: bool) {
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

    fn filter_error(&self) -> Option<&str> {
        self.filter_error.as_deref()
    }

    fn filter_counts(&self) -> Option<(usize, usize)> {
        None
    }

    fn find_counts(&self) -> Option<(Option<usize>, usize)> {
        self.filter.as_ref().map(|_| (self.current_match().map(|index| index + 1), self.matches.len()))
    }

    fn selected_text(&self) -> Option<String> {
        MergeTab::selected_text(self)
    }
}

impl Drop for MergeTab {
    fn drop(&mut self) {
        for cancel in &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}
