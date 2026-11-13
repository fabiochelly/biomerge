//! Mirrored folder trees, updated live while files are compared in the background.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use biomerge_core::Side;
use biomerge_core::dirscan::{self, Entry, EntryUpdate, Meta, NameFilter, ScanEvent, ScanOptions, Status};
use biomerge_core::extract::{self, ExtractOptions};
use biomerge_core::fileops::{self, Action, DeleteMode, Newer, Operation, Outcome, Plan, Trasher};
use biomerge_core::json::{self, Format, JsonDoc, JsonOptions};
use rayon::prelude::*;
use chrono::{DateTime, Local};
use egui::{
    Align2, Button, Color32, CornerRadius, CursorIcon, FontId, Key, Modifiers, Painter, Popup, Pos2, Rect,
    RichText, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::icons::{self, Icon};
use crate::scroll::{Position, Scroller};
use crate::text_view::StatusItem;
use crate::theme::{DocPalette, Tint, UiPalette, mix};
use crate::widgets::{self, Item, PathAction, PathBar, dialog_frame, menu_frame, menu_width, separator};

const OUTER_MARGIN: f32 = 12.0;
const HEADER_HEIGHT: f32 = 44.0;
const CENTER_GAP: f32 = 36.0;
const SWAP_BUTTON: f32 = 28.0;
const ROW_HEIGHT: f32 = 28.0;
const INDENT: f32 = 20.0;
const SIZE_WIDTH: f32 = 90.0;
const DATE_WIDTH: f32 = 128.0;
const CARD_PADDING: f32 = 8.0;
const MIN_WIDTH_FOR_DATE: f32 = 460.0;
const CARD_RADIUS: u8 = 10;
const REFRESH_EVERY: Duration = Duration::from_millis(80);
const PROGRESS_SIZE: egui::Vec2 = vec2(460.0, 68.0);
const DIALOG_WIDTH: f32 = 560.0;
const LIST_ROW: f32 = 26.0;
const LIST_HEIGHT: f32 = 230.0;

/// What a row asks of the application.
pub enum FolderAction {
    /// The pair of files to compare in a new tab.
    Open([PathBuf; 2]),
}

/// Where a copy or a deletion stands. Only one runs at a time per tab.
enum OpState {
    Idle,
    /// `plan` reads every selected file: it runs beside the interface.
    Planning {
        operation: Operation,
        receiver: Receiver<Result<Plan, String>>,
    },
    /// The dialog lists the plan and waits for a deliberate answer.
    Confirm(Box<Plan>),
    Running {
        operation: Operation,
        cancel: Arc<AtomicBool>,
        progress: Arc<Mutex<Snapshot>>,
        receiver: Receiver<Finished>,
    },
}

/// The last progress the worker reported.
#[derive(Clone, Default)]
struct Snapshot {
    items_done: usize,
    items_total: usize,
    bytes_done: u64,
    bytes_total: u64,
    current: PathBuf,
}

struct Finished {
    plan: Plan,
    outcome: Outcome,
    updates: Vec<EntryUpdate>,
}

/// What an operation left behind that deserves more than a line in the status bar.
pub struct Report {
    title: String,
    summary: String,
    pub cancelled: bool,
    pub errors: Vec<String>,
    kept_dirs: Vec<PathBuf>,
}

/// Refuses every deletion: under test, nothing may reach the real trash.
struct NoTrash;

impl Trasher for NoTrash {
    fn trash(&self, path: &Path) -> Result<(), String> {
        Err(format!("{} : corbeille désactivée pendant les tests", path.display()))
    }
}

fn default_trasher() -> Arc<dyn Trasher + Send + Sync> {
    if cfg!(test) { Arc::new(NoTrash) } else { Arc::new(fileops::SystemTrash) }
}

/// Why two files whose bytes differ are still equal for the reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Equivalence {
    /// Two word-processing documents holding the same text.
    Text,
    /// Two JSON files holding the same value under the JSON options.
    Json,
}

impl Equivalence {
    fn label(self) -> &'static str {
        match self {
            Equivalence::Text => "même texte",
            Equivalence::Json => "même valeur",
        }
    }
}

/// The check that looks, beside the interface, for pairs that differ only in bytes.
struct EquivalenceJob {
    cancel: Arc<AtomicBool>,
    receiver: Receiver<Vec<(PathBuf, Equivalence)>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    same: usize,
    /// Different bytes, same text or same JSON value.
    equivalent: usize,
    different: usize,
    left_only: usize,
    right_only: usize,
    errors: usize,
    pending: usize,
}

pub struct FolderTab {
    paths: [PathBuf; 2],
    /// Names shown instead of the folders' own (git's temporary folders).
    labels: [Option<String>; 2],
    inputs: [String; 2],
    entries: Vec<Entry>,
    expanded: Vec<bool>,
    visible: Vec<u32>,
    receiver: Receiver<ScanEvent>,
    picked: (Sender<(usize, PathBuf)>, Receiver<(usize, PathBuf)>),
    cancel: Arc<AtomicBool>,
    options: ScanOptions,
    differences_only: bool,
    scroll: f64,
    scroller: Scroller,
    /// Entry indices, sorted. Stale once entries are removed, so cleared then.
    selected: Vec<u32>,
    /// Where a Shift+click range starts.
    anchor: Option<u32>,
    /// The pane clicked last: the side a deletion targets when the rows exist on both.
    focus_side: Side,
    op: OpState,
    trasher: Arc<dyn Trasher + Send + Sync>,
    /// The last operation, in a few words, for the status bar.
    last_action: Option<String>,
    /// Errors, folders kept or a cancellation, waiting to be shown in a dialog.
    pub report: Option<Report>,
    /// Absolute paths an operation wrote or removed, for the text tabs to check.
    touched_paths: Vec<PathBuf>,
    statuses_changed: bool,
    visible_changed: bool,
    last_refresh: Instant,
    counts: Counts,
    summary: Option<String>,
    walk_time: Option<Duration>,
    walk_errors: usize,
    error: Option<String>,
    scanning: bool,
    /// Pairs found equal by text or by JSON value, by relative path. Only read for entries
    /// whose status is still `Different`: an entry that changes status leaves it by itself.
    equivalent: HashMap<PathBuf, Equivalence>,
    equivalence_options: (ExtractOptions, JsonOptions),
    equivalence_job: Option<EquivalenceJob>,
    /// The check must run (again) on these paths, or on every candidate for `Some(None)`.
    equivalence_due: Option<Option<Vec<PathBuf>>>,
}

impl FolderTab {
    pub fn new(ctx: &egui::Context, paths: [PathBuf; 2], options: ScanOptions, differences_only: bool) -> Self {
        let (_, receiver) = channel();
        let mut tab = Self {
            inputs: paths.clone().map(|p| p.display().to_string()),
            paths,
            labels: [None, None],
            entries: Vec::new(),
            expanded: Vec::new(),
            visible: Vec::new(),
            receiver,
            picked: channel(),
            cancel: Arc::new(AtomicBool::new(false)),
            options,
            differences_only,
            scroll: 0.0,
            scroller: Scroller::default(),
            selected: Vec::new(),
            anchor: None,
            focus_side: Side::Left,
            op: OpState::Idle,
            trasher: default_trasher(),
            last_action: None,
            report: None,
            touched_paths: Vec::new(),
            statuses_changed: false,
            visible_changed: false,
            last_refresh: Instant::now(),
            counts: Counts::default(),
            summary: None,
            walk_time: None,
            walk_errors: 0,
            error: None,
            scanning: false,
            equivalent: HashMap::new(),
            equivalence_options: (ExtractOptions::default(), JsonOptions::default()),
            equivalence_job: None,
            equivalence_due: None,
        };
        tab.rescan(ctx);
        tab
    }

    pub fn title(&self) -> String {
        let name = |side: usize| match &self.labels[side] {
            Some(label) => label.clone(),
            None => {
                let path = &self.paths[side];
                path.file_name()
                    .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
            }
        };
        format!("{}, {}", name(0), name(1))
    }

    pub fn set_labels(&mut self, labels: [Option<String>; 2]) {
        self.labels = labels;
    }

    pub fn rescan(&mut self, ctx: &egui::Context) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.entries.clear();
        self.expanded.clear();
        self.visible.clear();
        self.selected.clear();
        self.anchor = None;
        self.scroll = 0.0;
        self.counts = Counts::default();
        self.summary = None;
        self.walk_time = None;
        self.walk_errors = 0;
        self.error = None;
        self.scanning = true;
        self.equivalent.clear();
        self.equivalence_due = None;
        self.cancel_equivalence();

        let (sender, receiver) = channel();
        self.receiver = receiver;
        let [left, right] = self.paths.clone();
        let options = self.options.clone();
        let cancel = self.cancel.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            dirscan::scan(left, right, options, cancel, sender, move || ctx.request_repaint());
        });
    }

    /// The options "même texte" and "même valeur" are decided with. A change checks again.
    pub fn set_equivalence_options(&mut self, extract: ExtractOptions, json: JsonOptions) {
        if self.equivalence_options != (extract, json.clone()) {
            self.equivalence_options = (extract, json);
            if !self.scanning {
                self.equivalence_due = Some(None);
            }
        }
    }

    fn cancel_equivalence(&mut self) {
        if let Some(job) = self.equivalence_job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Looks for documents with the same text and JSON files with the same value among the
    /// pairs whose bytes differ, on the worker pool. `only`: the paths an operation touched.
    fn start_equivalence(&mut self, ctx: &egui::Context, only: Option<Vec<PathBuf>>) {
        self.cancel_equivalence();
        let concerned = |rel: &Path| only.as_ref().is_none_or(|only| only.iter().any(|path| rel.starts_with(path)));
        if only.is_none() {
            self.equivalent.clear();
        } else {
            self.equivalent.retain(|rel, _| !concerned(rel));
        }
        let candidates: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|entry| entry.status == Status::Different && !entry.is_dir() && is_candidate(&entry.rel))
            .filter(|entry| concerned(&entry.rel))
            .map(|entry| entry.rel.clone())
            .collect();
        if candidates.is_empty() {
            self.statuses_changed = true;
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = channel();
        let roots = self.paths.clone();
        let (extract_options, json_options) = self.equivalence_options.clone();
        let (worker_cancel, ctx) = (cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let found: Vec<(PathBuf, Equivalence)> = candidates
                .into_par_iter()
                .filter_map(|rel| {
                    if worker_cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let (left, right) = (roots[0].join(&rel), roots[1].join(&rel));
                    equivalence(&left, &right, &extract_options, &json_options).map(|kind| (rel, kind))
                })
                .collect();
            if !worker_cancel.load(Ordering::Relaxed) {
                let _ = sender.send(found);
                ctx.request_repaint();
            }
        });
        self.equivalence_job = Some(EquivalenceJob { cancel, receiver });
    }

    #[cfg(test)]
    pub fn equivalence_of(&self, rel: &str) -> Option<Equivalence> {
        self.equivalence(self.entries.iter().find(|e| e.rel == Path::new(rel))?)
    }

    /// The equivalence an entry shows: only while its bytes are known to differ.
    fn equivalence(&self, entry: &Entry) -> Option<Equivalence> {
        if entry.status != Status::Different || self.equivalent.is_empty() {
            return None;
        }
        self.equivalent.get(&entry.rel).copied()
    }

    pub fn set_respect_gitignore(&mut self, ctx: &egui::Context, respect: bool) {
        if self.options.respect_gitignore != respect {
            self.options.respect_gitignore = respect;
            self.rescan(ctx);
        }
    }

    /// Rescans when the patterns are not the ones in force.
    pub fn set_name_filter(&mut self, ctx: &egui::Context, filter: &NameFilter) {
        if self.options.name_filter.source() != filter.source() {
            self.options.name_filter = filter.clone();
            self.rescan(ctx);
        }
    }

    pub fn set_differences_only(&mut self, value: bool) {
        self.differences_only = value;
        self.visible_changed = true;
    }

    pub fn set_all_expanded(&mut self, expanded: bool) {
        self.expanded.iter_mut().for_each(|e| *e = expanded);
        self.visible_changed = true;
    }

    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        !self.scanning
            && self.equivalence_job.is_none()
            && self.equivalence_due.is_none()
            && matches!(self.op, OpState::Idle | OpState::Confirm(_))
    }

    #[cfg(test)]
    pub fn set_trasher(&mut self, trasher: Arc<dyn Trasher + Send + Sync>) {
        self.trasher = trasher;
    }

    #[cfg(test)]
    pub fn select_paths(&mut self, rels: &[&str]) {
        self.selected = rels
            .iter()
            .filter_map(|rel| self.entries.iter().position(|e| e.rel == Path::new(rel)))
            .map(|index| index as u32)
            .collect();
        self.selected.sort_unstable();
    }

    #[cfg(test)]
    pub fn entry_status(&self, rel: &str) -> Option<Status> {
        self.entries.iter().find(|e| e.rel == Path::new(rel)).map(|e| e.status)
    }

    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn selection(&self) -> Vec<String> {
        self.selected.iter().map(|&i| self.entries[i as usize].rel.display().to_string()).collect()
    }

    #[cfg(test)]
    pub fn pending_plan(&self) -> Option<&Plan> {
        match &self.op {
            OpState::Confirm(plan) => Some(plan),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn last_action(&self) -> Option<&str> {
        self.last_action.as_deref()
    }

    /// Screen position of a visible row in a pane, for clicks in tests.
    #[cfg(test)]
    pub fn row_center(&self, rel: &str, side: Side, view: Rect) -> Option<Pos2> {
        let index = self.entries.iter().position(|e| e.rel == Path::new(rel))? as u32;
        let row = self.visible.iter().position(|&i| i == index)?;
        let content = view.shrink(OUTER_MARGIN);
        let pane_width = ((content.width() - CENTER_GAP) / 2.0).max(160.0);
        let left = content.left() + side.index() as f32 * (pane_width + CENTER_GAP);
        let top = content.top() + HEADER_HEIGHT + CARD_PADDING;
        Some(pos2(left + pane_width / 2.0, top + (row as f32 + 0.5) * ROW_HEIGHT - self.scroll as f32))
    }

    /// True while the dialog of an operation must show: planning, confirming or reporting.
    pub fn wants_dialog(&self) -> bool {
        matches!(self.op, OpState::Planning { .. } | OpState::Confirm(_)) || self.report.is_some()
    }

    /// Copies and deletions need a finished comparison, a selection, and nothing else running.
    pub fn can_operate(&self) -> bool {
        self.can_sync() && !self.selected.is_empty()
    }

    /// A synchronisation takes the selection, or the whole comparison without one.
    pub fn can_sync(&self) -> bool {
        !self.scanning && self.error.is_none() && !self.entries.is_empty() && matches!(self.op, OpState::Idle)
    }

    pub fn is_operating(&self) -> bool {
        !matches!(self.op, OpState::Idle)
    }

    /// The side a deletion acts on: the only one holding the selection, or the pane clicked last.
    pub fn delete_side(&self) -> Side {
        let holds = |side: Side| {
            self.selected.iter().any(|&index| {
                let entry = &self.entries[index as usize];
                if side == Side::Left { entry.left.is_some() } else { entry.right.is_some() }
            })
        };
        match (holds(Side::Left), holds(Side::Right)) {
            (true, false) => Side::Left,
            (false, true) => Side::Right,
            _ => self.focus_side,
        }
    }

    /// Lists what `operation` would do, beside the interface; the dialog then asks.
    pub fn start_operation(&mut self, ctx: &egui::Context, operation: Operation) {
        let sync = matches!(operation, Operation::Mirror { .. } | Operation::Update);
        if !(if sync { self.can_sync() } else { self.can_operate() }) {
            return;
        }
        ctx.data_mut(|d| d.remove::<bool>(egui::Id::unique("folder-operation").with("permanent")));
        let (sender, receiver) = channel();
        let roots = self.paths.clone();
        let entries = self.entries.clone();
        // Without a selection, a synchronisation covers the whole comparison: its top entries.
        let selected = if self.selected.is_empty() {
            (0..self.entries.len() as u32).filter(|&index| self.entries[index as usize].parent.is_none()).collect()
        } else {
            self.selected.clone()
        };
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let plan = fileops::plan(&roots, &entries, &selected, operation).map_err(|e| e.to_string());
            let _ = sender.send(plan);
            ctx.request_repaint();
        });
        self.op = OpState::Planning { operation, receiver };
    }

    /// The paths an operation changed, once: the caller looks for tabs showing them.
    pub fn take_touched_paths(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.touched_paths)
    }

    /// Picks up the plan or the result of a worker; called every frame, for every tab.
    pub fn poll_operation(&mut self, ctx: &egui::Context) {
        match &self.op {
            OpState::Planning { operation, receiver } => match receiver.try_recv() {
                Ok(Ok(plan)) => self.op = OpState::Confirm(Box::new(plan)),
                Ok(Err(error)) => {
                    self.report = Some(Report {
                        title: operation_title(*operation),
                        summary: "Impossible de préparer l'opération".to_owned(),
                        cancelled: false,
                        errors: vec![error],
                        kept_dirs: Vec::new(),
                    });
                    self.op = OpState::Idle;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.op = OpState::Idle,
            },
            OpState::Running { receiver, operation, .. } => match receiver.try_recv() {
                Ok(finished) => {
                    let operation = *operation;
                    self.op = OpState::Idle;
                    self.finish_operation(operation, finished);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(REFRESH_EVERY),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.op = OpState::Idle,
            },
            OpState::Idle | OpState::Confirm(_) => {}
        }
    }

    /// Runs the confirmed plan on a worker, then looks again at what it touched.
    fn execute(&mut self, ctx: &egui::Context, mut plan: Box<Plan>, permanent: bool) {
        let mode = if permanent { DeleteMode::Permanent } else { DeleteMode::Trash };
        match plan.operation {
            Operation::Delete { side, .. } => plan.operation = Operation::Delete { side, mode },
            Operation::Mirror { to, .. } => plan.operation = Operation::Mirror { to, mode },
            Operation::Copy { .. } | Operation::Update => {}
        }
        let operation = plan.operation;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(Snapshot::default()));
        let (sender, receiver) = channel();
        let trasher = self.trasher.clone();
        // The rescan flag, not the operation's: a cancelled operation still needs its result
        // checked, but a closed tab does not.
        let recheck_cancel = self.cancel.clone();
        let (worker_cancel, worker_progress, ctx) = (cancel.clone(), progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut last_repaint = Instant::now();
            let outcome = fileops::execute(&plan, &*trasher, &worker_cancel, &mut |step| {
                if let Ok(mut snapshot) = worker_progress.lock() {
                    *snapshot = Snapshot {
                        items_done: step.items_done,
                        items_total: step.items_total,
                        bytes_done: step.bytes_done,
                        bytes_total: step.bytes_total,
                        current: step.current.to_owned(),
                    };
                }
                if last_repaint.elapsed() >= REFRESH_EVERY {
                    last_repaint = Instant::now();
                    ctx.request_repaint();
                }
            });
            let updates = dirscan::recheck(&plan.roots, &plan.touched(), &recheck_cancel);
            let _ = sender.send(Finished { plan: *plan, outcome, updates });
            ctx.request_repaint();
        });
        self.op = OpState::Running { operation, cancel, progress, receiver };
    }

    fn finish_operation(&mut self, operation: Operation, finished: Finished) {
        let Finished { plan, outcome, updates } = finished;
        let removed = dirscan::apply_updates(&mut self.entries, &updates);
        if !removed.is_empty() {
            let mut gone = removed.iter().peekable();
            let mut index = 0u32;
            self.expanded.retain(|_| {
                let drop = gone.next_if_eq(&&index).is_some();
                index += 1;
                !drop
            });
            self.selected.clear();
            self.anchor = None;
        }
        self.count();
        // Indices changed: the rows shown must follow before anything is painted.
        self.rebuild_visible();
        for item in &plan.items {
            for root in &plan.roots {
                self.touched_paths.push(root.join(&item.rel));
            }
        }
        // What the operation touched is checked again for "même texte".
        let touched: Vec<PathBuf> = plan.items.iter().map(|item| item.rel.clone()).collect();
        if !touched.is_empty() {
            self.equivalence_due = Some(Some(touched));
        }
        let summary = outcome_summary(operation, &outcome);
        self.last_action = Some(summary.clone());
        if outcome.cancelled || !outcome.errors.is_empty() || !outcome.kept_dirs.is_empty() {
            self.report = Some(Report {
                title: operation_title(operation),
                summary,
                cancelled: outcome.cancelled,
                errors: outcome.errors.iter().map(|e| e.error.to_string()).collect(),
                kept_dirs: outcome.kept_dirs,
            });
        }
    }

    /// Stops a running operation between two items; the rest of the plan is left alone.
    pub fn cancel_operation(&mut self) {
        if let OpState::Running { cancel, .. } = &self.op {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn status_items(&self) -> Vec<StatusItem> {
        let counts = self.counts;
        let mut items = Vec::new();
        if let Some(action) = &self.last_action {
            items.push(StatusItem::Text(action.clone()));
        }
        if self.scanning {
            items.push(StatusItem::Text(match self.walk_time {
                Some(_) => format!("Comparaison… {} restants", counts.pending),
                None => "Parcours des dossiers…".to_owned(),
            }));
        } else if let Some(summary) = &self.summary {
            items.push(StatusItem::Text(summary.clone()));
        }
        if self.walk_errors > 0 {
            items.push(StatusItem::Pill(format!("{} inaccessibles", self.walk_errors)));
        }
        if counts.errors > 0 {
            items.push(StatusItem::Pill(format!("{} erreurs", counts.errors)));
        }
        if self.equivalence_job.is_some() {
            items.push(StatusItem::Text("Comparaison des textes…".to_owned()));
        }
        if counts.equivalent > 0 {
            items.push(StatusItem::Pill(format!("{} même texte ou valeur", counts.equivalent)));
        }
        let plural = if counts.same > 1 { "s" } else { "" };
        items.push(StatusItem::Pill(format!("{} identique{plural}", counts.same)));
        items.push(StatusItem::Counts {
            deletions: counts.left_only,
            insertions: counts.right_only,
            changes: counts.different,
        });
        items
    }

    fn poll(&mut self) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                ScanEvent::Tree {
                    entries,
                    walk_errors,
                    walk_time,
                } => {
                    self.expanded = vec![false; entries.len()];
                    self.entries = entries;
                    self.walk_errors = walk_errors.len();
                    self.walk_time = Some(walk_time);
                    self.statuses_changed = true;
                }
                ScanEvent::Progress(batch) => {
                    for (index, status) in batch {
                        if let Some(entry) = self.entries.get_mut(index as usize) {
                            entry.status = status;
                        }
                    }
                    self.statuses_changed = true;
                }
                ScanEvent::Done {
                    total_time,
                    bytes_compared,
                    threads,
                } => {
                    self.scanning = false;
                    self.equivalence_due = Some(None);
                    self.statuses_changed = true;
                    self.summary = Some(format!(
                        "{} éléments en {} ({} lus, {threads} threads)",
                        self.entries.len(),
                        format_duration(total_time),
                        format_size(bytes_compared),
                    ));
                }
                ScanEvent::Failed(error) => {
                    self.scanning = false;
                    self.error = Some(error);
                }
            }
        }
        if let Some(job) = &self.equivalence_job {
            match job.receiver.try_recv() {
                Ok(found) => {
                    self.equivalent.extend(found);
                    self.equivalence_job = None;
                    self.statuses_changed = true;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.equivalence_job = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let due = !self.scanning || self.last_refresh.elapsed() >= REFRESH_EVERY;
        if self.statuses_changed && due {
            self.statuses_changed = false;
            self.visible_changed = true;
            self.last_refresh = Instant::now();
            dirscan::aggregate(&mut self.entries);
            self.count();
        }
        if self.visible_changed {
            self.visible_changed = false;
            self.rebuild_visible();
        }
    }

    fn count(&mut self) {
        let mut counts = Counts::default();
        for entry in self.entries.iter().filter(|e| !e.is_dir()) {
            match entry.status {
                Status::Same => counts.same += 1,
                Status::Different if self.equivalence(entry).is_some() => counts.equivalent += 1,
                Status::Different | Status::TypeMismatch => counts.different += 1,
                Status::LeftOnly => counts.left_only += 1,
                Status::RightOnly => counts.right_only += 1,
                Status::Error => counts.errors += 1,
                Status::Pending => counts.pending += 1,
            }
        }
        self.counts = counts;
    }

    fn rebuild_visible(&mut self) {
        self.visible.clear();
        let mut index = 0;
        while index < self.entries.len() {
            let entry = &self.entries[index];
            if self.differences_only && entry.status == Status::Same {
                index = entry.subtree_end as usize;
                continue;
            }
            self.visible.push(index as u32);
            let collapsed = entry.is_dir() && !self.expanded[index];
            index = if collapsed {
                entry.subtree_end as usize
            } else {
                index + 1
            };
        }
    }

    /// Returns what a row asks for: a pair of files to open.
    pub fn show(&mut self, ui: &mut Ui, chrome: &UiPalette, doc: &DocPalette) -> Option<FolderAction> {
        let ctx = ui.ctx().clone();
        self.poll_operation(&ctx);
        self.poll();
        if let Some(only) = self.equivalence_due.take() {
            self.start_equivalence(&ctx, only);
        }
        if self.scanning {
            ctx.request_repaint_after(REFRESH_EVERY);
        }
        let available = ui.available_rect_before_wrap();
        ui.allocate_rect(available, Sense::hover());
        let painter = ui.painter_at(available);
        painter.rect_filled(available, 0.0, chrome.background);

        let content = available.shrink(OUTER_MARGIN);
        let pane_width = ((content.width() - CENTER_GAP) / 2.0).max(160.0);
        let lefts = [content.left(), content.left() + pane_width + CENTER_GAP];
        let cards = lefts.map(|x| {
            Rect::from_min_size(pos2(x, content.top()), vec2(pane_width, content.height()))
        });
        for card in cards {
            painter.add(egui::Shadow {
                offset: [0, 3],
                blur: 14,
                spread: 0,
                color: Color32::from_black_alpha(46),
            }.as_shape(card, CARD_RADIUS));
            painter.rect_filled(card, CARD_RADIUS, doc.canvas);
        }

        let mut rescan = false;
        while let Ok((side, path)) = self.picked.1.try_recv() {
            self.inputs[side] = path.display().to_string();
            self.paths[side] = path;
            self.labels[side] = None;
            rescan = true;
        }
        for side in 0..2 {
            let header = Rect::from_min_size(cards[side].min, vec2(pane_width, HEADER_HEIGHT));
            let action = widgets::path_bar(
                ui,
                chrome,
                ui.scope_id().with(("header", side)),
                header,
                PathBar {
                    path: &self.paths[side],
                    input: &mut self.inputs[side],
                    icon: Icon::Folder,
                    dirty: false,
                    label: ["Chemin de gauche", "Chemin de droite"][side],
                    browse_tip: "Choisir un autre dossier",
                    title: self.labels[side].as_deref(),
                    read_only: false,
                    badges: &[],
                    fixed: false,
                },
            );
            match action {
                PathAction::Commit(path) if path != self.paths[side] => {
                    self.paths[side] = path;
                    self.labels[side] = None;
                    rescan = true;
                }
                PathAction::Browse => {
                    let sender = self.picked.0.clone();
                    widgets::pick_path(&ctx, true, move |path| {
                        let _ = sender.send((side, path));
                    });
                }
                _ => {}
            }
        }
        let swap = icons::square(
            pos2((cards[0].right() + cards[1].left()) / 2.0, content.top() + HEADER_HEIGHT / 2.0),
            SWAP_BUTTON,
        );
        let swap_id = ui.scope_id().with("swap");
        if widgets::icon_button(ui, chrome, swap, swap_id, Icon::Swap, "Échanger les deux côtés", chrome.text_dim).clicked() {
            self.paths.swap(0, 1);
            self.inputs.swap(0, 1);
            self.labels.swap(0, 1);
            rescan = true;
        }
        if rescan {
            self.rescan(&ctx);
        }

        let view = Rect::from_min_max(
            pos2(content.left(), content.top() + HEADER_HEIGHT + CARD_PADDING),
            content.max - vec2(0.0, CARD_PADDING),
        );
        let panes = lefts.map(|x| Rect::from_min_size(pos2(x, view.top()), vec2(pane_width, view.height())));
        let border = |painter: &Painter| {
            for card in cards {
                widgets::mask_bottom_corners(painter, card, CARD_RADIUS as f32, chrome.background);
                painter.rect_stroke(card, CARD_RADIUS, Stroke::new(1.0, chrome.border), egui::StrokeKind::Inside);
            }
        };
        border(&painter);
        if let Some(error) = &self.error {
            painter.text(view.center(), Align2::CENTER_CENTER, error, FontId::proportional(14.0), chrome.error);
            return None;
        }
        if self.entries.is_empty() {
            let text = if self.scanning {
                "Parcours des dossiers…"
            } else {
                "Dossiers vides"
            };
            painter.text(view.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), chrome.text_dim);
            return None;
        }

        let total = self.visible.len() as f64 * ROW_HEIGHT as f64;
        let response = ui.interact(view, ui.scope_id().with("tree"), Sense::click());
        let max_scroll = (total - view.height() as f64).max(0.0);
        let mut no_sideways = 0.0;
        self.scroller.update(
            ui,
            response.hovered(),
            Position {
                y: &mut self.scroll,
                x: &mut no_sideways,
                max_y: max_scroll,
                max_x: 0.0,
                row_height: ROW_HEIGHT,
            },
        );
        if !ctx.egui_wants_keyboard_input() {
            let page = view.height() as f64 - ROW_HEIGHT as f64;
            ui.input(|i| {
                if i.key_pressed(egui::Key::PageDown) {
                    self.scroll += page;
                }
                if i.key_pressed(egui::Key::PageUp) {
                    self.scroll -= page;
                }
            });
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);

        let mut open = None;
        if let Some(pos) = response.interact_pointer_pos() {
            let row = ((pos.y - view.top()) as f64 + self.scroll) / ROW_HEIGHT as f64;
            let side = if pos.x < panes[1].left() - CENTER_GAP / 2.0 { Side::Left } else { Side::Right };
            let modifiers = ui.input(|i| i.modifiers);
            match self.visible.get(row as usize).copied() {
                Some(index) if response.secondary_clicked() => {
                    // The menu acts on the selection; a row outside it becomes the selection.
                    self.focus_side = side;
                    if self.selected.binary_search(&index).is_err() {
                        self.selected = vec![index];
                        self.anchor = Some(index);
                    }
                }
                Some(index) if response.clicked() || response.double_clicked() => {
                    self.focus_side = side;
                    if modifiers.command {
                        match self.selected.binary_search(&index) {
                            Ok(at) => drop(self.selected.remove(at)),
                            Err(at) => self.selected.insert(at, index),
                        }
                        self.anchor = Some(index);
                    } else if let (true, Some(anchor)) = (modifiers.shift, self.anchor) {
                        self.select_range(anchor, index);
                    } else {
                        self.selected = vec![index];
                        self.anchor = Some(index);
                        let entry = &self.entries[index as usize];
                        if entry.is_dir() {
                            if response.clicked() {
                                self.expanded[index as usize] ^= true;
                                self.visible_changed = true;
                            }
                        } else if response.double_clicked() {
                            open = Some(FolderAction::Open([
                                self.paths[0].join(&entry.rel),
                                self.paths[1].join(&entry.rel),
                            ]));
                        }
                    }
                }
                None if response.clicked() => {
                    self.selected.clear();
                    self.anchor = None;
                }
                _ => {}
            }
        }
        if response.hovered() {
            ctx.set_cursor_icon(CursorIcon::Default);
        }
        if !ctx.egui_wants_keyboard_input() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
            self.selected = self.visible.clone();
            self.selected.sort_unstable();
        }
        let menu_choice = self.context_menu(&response, chrome);
        match menu_choice {
            Some(MenuChoice::Open) => {
                if let [index] = self.selected[..] {
                    let rel = &self.entries[index as usize].rel;
                    open = Some(FolderAction::Open([self.paths[0].join(rel), self.paths[1].join(rel)]));
                }
            }
            Some(MenuChoice::Operate(operation)) => self.start_operation(&ctx, operation),
            None => {}
        }

        let first = (self.scroll / ROW_HEIGHT as f64) as usize;
        let count = (view.height() / ROW_HEIGHT) as usize + 2;
        let clip = panes.map(|pane| painter.with_clip_rect(pane));
        for (offset, &index) in self.visible.iter().skip(first).take(count).enumerate() {
            let y = view.top() as f64 + (first + offset) as f64 * ROW_HEIGHT as f64 - self.scroll;
            let entry = &self.entries[index as usize];
            let selected = self.selected.binary_search(&index).is_ok();
            let expanded = self.expanded[index as usize];
            for side in 0..2 {
                let meta = if side == 0 { entry.left } else { entry.right };
                let row = Rect::from_min_size(pos2(panes[side].left(), y as f32), vec2(pane_width, ROW_HEIGHT));
                // The pane clicked last, which a deletion targets, holds the stronger selection.
                let selection = match (selected, side == self.focus_side.index()) {
                    (false, _) => None,
                    (true, true) => Some(doc.selection),
                    (true, false) => Some(mix(doc.canvas, doc.selection, 0.55)),
                };
                paint_row(&clip[side], row, entry, meta, expanded, selection, self.equivalence(entry), doc);
            }
        }
        border(&painter);
        self.paint_progress(ui, view, chrome);
        open
    }

    /// Every row between two entries, in the order they are shown.
    fn select_range(&mut self, from: u32, to: u32) {
        let position = |index: u32| self.visible.iter().position(|&i| i == index);
        let (Some(a), Some(b)) = (position(from), position(to)) else {
            self.selected = vec![to];
            return;
        };
        self.selected = self.visible[a.min(b)..=a.max(b)].to_vec();
        self.selected.sort_unstable();
    }

    fn context_menu(&self, response: &egui::Response, chrome: &UiPalette) -> Option<MenuChoice> {
        let mut choice = None;
        let enabled = self.can_operate();
        let single_file = matches!(self.selected[..], [index] if !self.entries[index as usize].is_dir());
        let side = self.delete_side();
        Popup::context_menu(response).frame(menu_frame(chrome)).show(|ui| {
            menu_width(ui, 300.0);
            let mut go = |ui: &mut Ui, item: Item<'_>, picked: MenuChoice| {
                if item.show(ui, chrome).clicked() {
                    choice = Some(picked);
                    ui.close();
                }
            };
            go(ui, Item::new("Comparer les deux fichiers").icon(Icon::Compare).keys("Double-clic").enabled(single_file), MenuChoice::Open);
            separator(ui, chrome);
            for (operation, name, icon) in SYNC_CHOICES {
                go(ui, Item::new(name).icon(icon).enabled(self.can_sync()), MenuChoice::Operate(operation));
            }
            separator(ui, chrome);
            go(
                ui,
                Item::new("Copier vers la droite").icon(Icon::CopyRight).keys("Ctrl+→").enabled(enabled),
                MenuChoice::Operate(Operation::Copy { from: Side::Left }),
            );
            go(
                ui,
                Item::new("Copier vers la gauche").icon(Icon::ArrowLeft).keys("Ctrl+←").enabled(enabled),
                MenuChoice::Operate(Operation::Copy { from: Side::Right }),
            );
            separator(ui, chrome);
            for (which, name) in [(Side::Left, "Supprimer à gauche (corbeille)"), (Side::Right, "Supprimer à droite (corbeille)")] {
                let keys = if which == side { "Suppr" } else { "" };
                go(
                    ui,
                    Item::new(name).icon(Icon::Trash).keys(keys).danger().enabled(enabled),
                    MenuChoice::Operate(Operation::Delete { side: which, mode: DeleteMode::Trash }),
                );
            }
        });
        choice
    }

    /// A small card over the bottom of the trees while files are copied or deleted.
    fn paint_progress(&mut self, ui: &mut Ui, view: Rect, chrome: &UiPalette) {
        let OpState::Running { operation, progress, cancel, .. } = &self.op else {
            return;
        };
        let snapshot = progress.lock().map(|s| s.clone()).unwrap_or_default();
        let cancelling = cancel.load(Ordering::Relaxed);
        let card = Rect::from_center_size(
            pos2(view.center().x, view.bottom() - PROGRESS_SIZE.y / 2.0 - 14.0),
            vec2(PROGRESS_SIZE.x.min(view.width() - 24.0), PROGRESS_SIZE.y),
        );
        let painter = ui.painter();
        painter.add(egui::Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(70) }.as_shape(card, 10));
        painter.rect(card, 10.0, chrome.raised, Stroke::new(1.0, chrome.border), StrokeKind::Inside);
        let inner = card.shrink2(vec2(16.0, 12.0));
        let verb = match operation {
            Operation::Copy { .. } => "Copie",
            Operation::Delete { .. } => "Suppression",
            Operation::Mirror { .. } | Operation::Update => "Synchronisation",
        };
        let title = if cancelling {
            "Arrêt après l'élément en cours…".to_owned()
        } else {
            format!("{verb}… {} / {}", snapshot.items_done, snapshot.items_total)
        };
        painter.text(pos2(inner.left(), inner.top() + 8.0), Align2::LEFT_CENTER, title, widgets::medium(12.5), chrome.text);
        let button = Rect::from_min_size(pos2(inner.right() - 84.0, inner.top()), vec2(84.0, 26.0));
        let current = snapshot.current.display().to_string();
        painter.with_clip_rect(Rect::from_min_max(pos2(inner.left(), inner.top() + 18.0), pos2(button.left() - 10.0, inner.top() + 36.0))).text(
            pos2(inner.left(), inner.top() + 27.0),
            Align2::LEFT_CENTER,
            current,
            FontId::proportional(11.5),
            chrome.text_dim,
        );
        let share = if snapshot.bytes_total > 0 {
            snapshot.bytes_done as f32 / snapshot.bytes_total as f32
        } else {
            snapshot.items_done as f32 / snapshot.items_total.max(1) as f32
        };
        let bar = Rect::from_min_size(pos2(inner.left(), inner.bottom() - 3.0), vec2(inner.width(), 3.0));
        painter.rect_filled(bar, 1.5, chrome.hover);
        painter.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * share.clamp(0.0, 1.0), 3.0)), 1.5, chrome.accent);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(button));
        if child.add_enabled(!cancelling, Button::new("Annuler").min_size(button.size())).clicked() {
            self.cancel_operation();
        }
    }

    /// The confirmation of a planned operation, or the report of a finished one. Returns false
    /// once nothing is left to show.
    pub fn show_dialog(&mut self, ctx: &egui::Context, chrome: &UiPalette) {
        if let Some(report) = &self.report {
            let mut close = false;
            let modal = egui::Modal::new(egui::Id::unique("folder-report")).frame(dialog_frame(chrome)).show(ctx, |ui| {
                ui.set_width(DIALOG_WIDTH);
                ui.spacing_mut().item_spacing.y = 6.0;
                ui.label(RichText::new(&report.title).font(widgets::semibold(16.0)));
                let color = if report.errors.is_empty() { chrome.text_dim } else { chrome.error };
                ui.label(RichText::new(&report.summary).color(color));
                if report.cancelled {
                    ui.label(RichText::new("Annulé : le reste de la liste n'a pas été touché.").color(chrome.warning));
                }
                let mut lines: Vec<(String, Color32)> = report.errors.iter().map(|e| (e.clone(), chrome.error)).collect();
                lines.extend(report.kept_dirs.iter().map(|dir| {
                    (format!("{} : dossier conservé, il contient des éléments non affichés", dir.display()), chrome.text_dim)
                }));
                if !lines.is_empty() {
                    ui.add_space(4.0);
                    list_frame(chrome).show(ui, |ui| {
                        egui::ScrollArea::vertical().max_height(LIST_HEIGHT).auto_shrink([false, true]).show_rows(
                            ui,
                            LIST_ROW,
                            lines.len(),
                            |ui, range| {
                                for (text, color) in &lines[range] {
                                    ui.add_sized([ui.available_width(), LIST_ROW], egui::Label::new(RichText::new(text).size(12.0).color(*color)).truncate());
                                }
                            },
                        );
                    });
                }
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(primary_button(chrome, "Fermer")).clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.should_close() {
                self.report = None;
            }
            return;
        }
        let mut answer = None;
        let modal = match &self.op {
            OpState::Planning { operation, .. } => {
                let operation = *operation;
                egui::Modal::new(egui::Id::unique("folder-operation")).frame(dialog_frame(chrome)).show(ctx, |ui| {
                    ui.set_width(DIALOG_WIDTH);
                    ui.label(RichText::new(operation_title(operation)).font(widgets::semibold(16.0)));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Examen des éléments choisis…").color(chrome.text_dim));
                    });
                    ui.add_space(10.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Annuler").clicked() {
                            answer = Some(Answer::Cancel);
                        }
                    });
                })
            }
            OpState::Confirm(plan) => {
                let id = egui::Id::unique("folder-operation");
                let permanent = ctx.data(|d| d.get_temp::<bool>(id.with("permanent"))).unwrap_or(false);
                egui::Modal::new(id).frame(dialog_frame(chrome)).show(ctx, |ui| {
                    answer = confirm_contents(ui, chrome, &self.paths, plan, permanent);
                })
            }
            _ => return,
        };
        if modal.should_close() && answer.is_none() {
            answer = Some(Answer::Cancel);
        }
        let permanent_key = egui::Id::unique("folder-operation").with("permanent");
        match answer {
            None => {}
            Some(Answer::Cancel) => {
                ctx.data_mut(|d| d.remove::<bool>(permanent_key));
                self.op = OpState::Idle;
            }
            Some(Answer::AskPermanent) => {
                ctx.data_mut(|d| d.insert_temp(permanent_key, true));
            }
            Some(Answer::Go { permanent }) => {
                ctx.data_mut(|d| d.remove::<bool>(permanent_key));
                if let OpState::Confirm(plan) = std::mem::replace(&mut self.op, OpState::Idle) {
                    self.execute(ctx, plan, permanent);
                }
            }
        }
    }

    /// Opens the delete dialog straight on permanent deletion (Shift+Delete). The dialog still
    /// has to be confirmed.
    pub fn ask_permanent(ctx: &egui::Context) {
        ctx.data_mut(|d| d.insert_temp(egui::Id::unique("folder-operation").with("permanent"), true));
    }

    /// Answers the open dialog as its buttons would, for tests.
    #[cfg(test)]
    pub fn confirm_pending(&mut self, ctx: &egui::Context, permanent: bool) {
        if let OpState::Confirm(plan) = std::mem::replace(&mut self.op, OpState::Idle) {
            self.execute(ctx, plan, permanent);
        }
    }
}

enum MenuChoice {
    Open,
    Operate(Operation),
}

#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Cancel,
    /// The secondary button of a deletion: the dialog now says it is for good.
    AskPermanent,
    Go { permanent: bool },
}

/// The synchronisations, as menus offer them.
pub const SYNC_CHOICES: [(Operation, &str, Icon); 3] = [
    (Operation::Mirror { to: Side::Right, mode: DeleteMode::Trash }, "Rendre la droite identique à la gauche", Icon::Mirror),
    (Operation::Mirror { to: Side::Left, mode: DeleteMode::Trash }, "Rendre la gauche identique à la droite", Icon::Mirror),
    (Operation::Update, "Mettre à jour dans les deux sens", Icon::Sync),
];

fn operation_title(operation: Operation) -> String {
    match operation {
        Operation::Copy { from: Side::Left } => "Copier vers la droite",
        Operation::Copy { from: Side::Right } => "Copier vers la gauche",
        Operation::Delete { side: Side::Left, .. } => "Supprimer à gauche",
        Operation::Delete { side: Side::Right, .. } => "Supprimer à droite",
        Operation::Mirror { to: Side::Right, .. } => "Rendre la droite identique à la gauche",
        Operation::Mirror { to: Side::Left, .. } => "Rendre la gauche identique à la droite",
        Operation::Update => "Mettre à jour dans les deux sens",
    }
    .to_owned()
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count > 1 { many } else { one })
}

fn outcome_summary(operation: Operation, outcome: &Outcome) -> String {
    let done = outcome.completed;
    let mut text = match operation {
        Operation::Copy { from } => format!(
            "{} vers la {} ({})",
            plural(done, "élément copié", "éléments copiés"),
            if from == Side::Left { "droite" } else { "gauche" },
            format_size(outcome.bytes_copied)
        ),
        Operation::Delete { side, mode } => format!(
            "{} à {}",
            if mode == DeleteMode::Trash {
                plural(done, "élément mis à la corbeille", "éléments mis à la corbeille")
            } else {
                plural(done, "élément supprimé définitivement", "éléments supprimés définitivement")
            },
            if side == Side::Left { "gauche" } else { "droite" }
        ),
        Operation::Mirror { to, .. } => format!(
            "{} à {} ({} copiés)",
            plural(done, "élément synchronisé", "éléments synchronisés"),
            if to == Side::Left { "gauche" } else { "droite" },
            format_size(outcome.bytes_copied)
        ),
        Operation::Update => format!(
            "{} dans les deux sens ({} copiés)",
            plural(done, "élément mis à jour", "éléments mis à jour"),
            format_size(outcome.bytes_copied)
        ),
    };
    if !outcome.errors.is_empty() {
        text.push_str(&format!(", {}", plural(outcome.errors.len(), "échec", "échecs")));
    }
    if outcome.cancelled {
        text.push_str(", annulé");
    }
    text
}

fn list_frame(chrome: &UiPalette) -> egui::Frame {
    egui::Frame::new()
        .fill(chrome.input)
        .stroke(Stroke::new(1.0, chrome.border))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 6))
}

pub fn primary_button<'a>(chrome: &UiPalette, text: &'a str) -> Button<'a> {
    Button::new(RichText::new(text).color(chrome.on_accent))
        .fill(chrome.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(6))
        .min_size(vec2(96.0, 32.0))
}

fn danger_button<'a>(chrome: &UiPalette, text: &'a str) -> Button<'a> {
    Button::new(RichText::new(text).color(chrome.on_accent))
        .fill(chrome.error)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(6))
        .min_size(vec2(96.0, 32.0))
}

/// A count with its icon, on one line of the summary; nothing for zero.
fn total_line(ui: &mut Ui, icon: Icon, color: Color32, text: String) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        icons::paint(ui.painter(), icon, icons::square(rect.center(), 15.0), color);
        ui.label(RichText::new(text).color(color));
    });
}

fn home_relative(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.as_ref()
        .and_then(|home| path.strip_prefix(home).ok())
        .map_or_else(|| path.display().to_string(), |rest| format!("~/{}", rest.display()))
}

/// The plan, counted then listed, and the buttons. Whether files go to the trash or are gone
/// for good is said in words, next to the button that does it.
fn confirm_contents(ui: &mut Ui, chrome: &UiPalette, roots: &[PathBuf; 2], plan: &Plan, permanent: bool) -> Option<Answer> {
    let mut answer = None;
    let totals = plan.totals;
    ui.set_width(DIALOG_WIDTH);
    ui.spacing_mut().item_spacing.y = 6.0;
    ui.label(RichText::new(operation_title(plan.operation)).font(widgets::semibold(16.0)));
    let sync = matches!(plan.operation, Operation::Mirror { .. } | Operation::Update);
    let deleting = match plan.operation {
        Operation::Mirror { to, .. } => {
            ui.label(
                RichText::new(format!("De {}\nvers {}", home_relative(&roots[to.other().index()]), home_relative(&roots[to.index()])))
                    .size(12.0)
                    .color(chrome.text_dim),
            );
            (totals.deletes + totals.delete_dirs + totals.dirs_kept > 0).then_some(to)
        }
        Operation::Update => {
            ui.label(
                RichText::new(format!("Entre {}\net {}", home_relative(&roots[0]), home_relative(&roots[1])))
                    .size(12.0)
                    .color(chrome.text_dim),
            );
            None
        }
        Operation::Copy { from } => {
            let to = from.other();
            ui.label(
                RichText::new(format!("De {}\nvers {}", home_relative(&roots[from.index()]), home_relative(&roots[to.index()])))
                    .size(12.0)
                    .color(chrome.text_dim),
            );
            None
        }
        Operation::Delete { side, .. } => {
            ui.label(RichText::new(format!("Dans {}", home_relative(&roots[side.index()]))).size(12.0).color(chrome.text_dim));
            Some(side)
        }
    };
    ui.add_space(6.0);
    if deleting.is_some() {
        if permanent {
            total_line(ui, Icon::Warning, chrome.error, "Suppression définitive : rien ne passera par la corbeille.".to_owned());
        } else {
            total_line(ui, Icon::Trash, chrome.text, "Les éléments iront dans la corbeille, d'où ils pourront être restaurés.".to_owned());
        }
    }
    let size = |bytes: u64| format_size(bytes);
    if sync {
        // Each direction on its own line: what goes right, what goes left.
        for from in [Side::Left, Side::Right] {
            let count = |action: Action| plan.items.iter().filter(|item| item.from == from && item.action == action).count();
            let mut parts = Vec::new();
            for (action, one, many) in [
                (Action::Create, "fichier créé", "fichiers créés"),
                (Action::Overwrite, "fichier remplacé", "fichiers remplacés"),
                (Action::CreateDir, "dossier créé", "dossiers créés"),
            ] {
                if count(action) > 0 {
                    parts.push(plural(count(action), one, many));
                }
            }
            if !parts.is_empty() {
                let side = if from == Side::Left { "Vers la droite" } else { "Vers la gauche" };
                total_line(ui, if from == Side::Left { Icon::ArrowRight } else { Icon::ArrowLeft }, chrome.text, format!("{side} : {}", parts.join(", ")));
            }
        }
    }
    if totals.creates > 0 && !sync {
        total_line(ui, Icon::File, chrome.text, plural(totals.creates, "fichier créé", "fichiers créés"));
    }
    if totals.overwrites > 0 && !sync {
        total_line(ui, Icon::Copy, chrome.text, format!("{} ({} écrasés)", plural(totals.overwrites, "fichier remplacé", "fichiers remplacés"), size(totals.bytes_replaced)));
    }
    if totals.overwrites_of_newer > 0 {
        total_line(
            ui,
            Icon::Warning,
            chrome.warning,
            format!("dont {} : la copie écrasera une version plus récente", plural(totals.overwrites_of_newer, "fichier plus récent", "fichiers plus récents")),
        );
    }
    if totals.create_dirs > 0 && !sync {
        total_line(ui, Icon::Folder, chrome.text, plural(totals.create_dirs, "dossier créé", "dossiers créés"));
    }
    if totals.bytes_to_copy > 0 {
        total_line(ui, Icon::CopyRight, chrome.text_dim, format!("{} à copier", size(totals.bytes_to_copy)));
    }
    if totals.deletes > 0 {
        total_line(ui, Icon::Trash, chrome.text, format!("{} ({})", plural(totals.deletes, "fichier supprimé", "fichiers supprimés"), size(totals.bytes_to_delete)));
    }
    if totals.delete_dirs > 0 {
        total_line(ui, Icon::Folder, chrome.text, plural(totals.delete_dirs, "dossier supprimé", "dossiers supprimés"));
    }
    if totals.dirs_kept > 0 {
        total_line(ui, Icon::Folder, chrome.text_dim, format!("{} : ils contiennent des éléments non affichés", plural(totals.dirs_kept, "dossier conservé", "dossiers conservés")));
    }
    if totals.identical > 0 {
        total_line(ui, Icon::MenuCheck, chrome.text_dim, plural(totals.identical, "fichier déjà identique, ignoré", "fichiers déjà identiques, ignorés"));
    }
    if totals.blocked > 0 {
        total_line(ui, Icon::Cross, chrome.error, format!("{} (voir la liste)", plural(totals.blocked, "élément laissé de côté", "éléments laissés de côté")));
    }
    if sync {
        ui.label(
            RichText::new("Seuls les éléments de cette comparaison sont concernés : ce que le filtre des noms ou .gitignore masque n'est ni copié ni supprimé.")
                .size(12.0)
                .color(chrome.text_dim),
        );
    }

    // One list for both: what will be done, then what will not, with why.
    let rows = plan.items.len() + plan.blocked.len();
    if rows > 0 {
        ui.add_space(4.0);
        list_frame(chrome).show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(LIST_HEIGHT).auto_shrink([false, true]).show_rows(ui, LIST_ROW, rows, |ui, range| {
                for row in range {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW), Sense::hover());
                    let y = rect.center().y;
                    let painter = ui.painter_at(rect);
                    let (icon, color, rel, tag) = match plan.items.get(row) {
                        Some(item) => {
                            let (tag, color) = match item.action {
                                // A synchronisation goes both ways: each line says which.
                                _ if sync && !matches!(item.action, Action::Delete | Action::DeleteDir) => {
                                    let arrow = if item.from == Side::Left { "→" } else { "←" };
                                    let (word, color) = match item.action {
                                        Action::CreateDir => ("dossier créé", chrome.text_dim),
                                        Action::Create => ("créé", chrome.text_dim),
                                        _ if item.newer == Newer::Destination => ("remplace une version plus récente", chrome.warning),
                                        _ => ("remplacé", chrome.text_dim),
                                    };
                                    (format!("{arrow} {word}"), color)
                                }
                                Action::CreateDir => ("dossier créé".to_owned(), chrome.text_dim),
                                Action::Create => ("créé".to_owned(), chrome.text_dim),
                                Action::Overwrite if item.newer == Newer::Destination => ("remplace une version plus récente".to_owned(), chrome.warning),
                                Action::Overwrite => ("remplacé".to_owned(), chrome.text_dim),
                                Action::Delete => ("supprimé".to_owned(), chrome.text_dim),
                                Action::DeleteDir if item.unlisted > 0 => ("dossier conservé".to_owned(), chrome.text_dim),
                                Action::DeleteDir => ("dossier supprimé".to_owned(), chrome.text_dim),
                            };
                            let icon = if matches!(item.action, Action::CreateDir | Action::DeleteDir) { Icon::Folder } else { Icon::File };
                            (icon, color, &item.rel, tag)
                        }
                        None => {
                            let blocked = &plan.blocked[row - plan.items.len()];
                            // The path is already on the line: the reason alone fits beside it.
                            let reason = blocked.reason.to_string();
                            let reason = match reason.split_once(": ") {
                                Some((head, rest)) if Path::new(head).ends_with(&blocked.rel) => rest.to_owned(),
                                _ => reason,
                            };
                            (Icon::Cross, chrome.error, &blocked.rel, reason)
                        }
                    };
                    icons::paint(&painter, icon, icons::square(pos2(rect.left() + 8.0, y), 14.0), color);
                    let tag_font = FontId::proportional(11.5);
                    // The tag takes what the path leaves, but never less than 40 % of the line.
                    let name_width = painter.layout_no_wrap(rel.display().to_string(), FontId::proportional(12.5), chrome.text).size().x;
                    let room = (rect.width() - 22.0 - name_width - 16.0).max(rect.width() * 0.4);
                    let tag_width = painter.layout_no_wrap(tag.clone(), tag_font.clone(), color).size().x.min(room);
                    let tag_left = rect.right() - tag_width;
                    painter.with_clip_rect(Rect::from_min_max(rect.min, pos2(tag_left - 12.0, rect.bottom()))).text(
                        pos2(rect.left() + 22.0, y),
                        Align2::LEFT_CENTER,
                        rel.display().to_string(),
                        FontId::proportional(12.5),
                        chrome.text,
                    );
                    painter.with_clip_rect(Rect::from_min_max(pos2(tag_left, rect.top()), rect.max)).text(
                        pos2(rect.right(), y),
                        Align2::RIGHT_CENTER,
                        tag,
                        tag_font,
                        color,
                    );
                }
            });
        });
    }
    ui.add_space(12.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if plan.is_empty() {
            ui.label(RichText::new("Rien à faire").color(chrome.text_dim));
            if ui.add(primary_button(chrome, "Fermer")).clicked() {
                answer = Some(Answer::Cancel);
            }
            return;
        }
        let confirm = match (sync, deleting, permanent) {
            (true, Some(_), true) => "Synchroniser et supprimer définitivement",
            (true, _, _) => "Synchroniser",
            (false, None, _) => "Copier",
            (false, Some(_), false) => "Mettre à la corbeille",
            (false, Some(_), true) => "Supprimer définitivement",
        };
        let button = if deleting.is_some() && permanent { danger_button(chrome, confirm) } else { primary_button(chrome, confirm) };
        if ui.add(button).clicked() {
            answer = Some(Answer::Go { permanent: deleting.is_some() && permanent });
        }
        if ui.button("Annuler").clicked() {
            answer = Some(Answer::Cancel);
        }
        if deleting.is_some() && !permanent {
            // At the far left, away from the buttons that confirm.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let text = RichText::new("Supprimer définitivement…").color(chrome.error).size(12.5);
                if ui.add(Button::new(text).frame(false)).on_hover_text("Sans passer par la corbeille").clicked() {
                    answer = Some(Answer::AskPermanent);
                }
            });
        }
    });
    answer
}

impl Drop for FolderTab {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel_equivalence();
    }
}

/// A pair worth a look past its bytes: documents compared by text, JSON by value.
fn is_candidate(rel: &Path) -> bool {
    extract::is_supported(rel) || Format::from_path(rel).is_some()
}

/// Whether two files that differ in bytes are equal for the reader. A file that cannot be read
/// or parsed is simply not equivalent: the pair stays "différent".
fn equivalence(left: &Path, right: &Path, extract_options: &ExtractOptions, json_options: &JsonOptions) -> Option<Equivalence> {
    if extract::is_supported(left) && extract::is_supported(right) {
        return extract::same_text(left, right, extract_options).ok()?.then_some(Equivalence::Text);
    }
    let format = Format::from_path(left)?;
    let parse = |path: &Path| JsonDoc::parse(&std::fs::read_to_string(path).ok()?, format).ok();
    let (left, right) = (parse(left)?, parse(right)?);
    json::equal(&left, &right, json_options).then_some(Equivalence::Json)
}

fn status_tint<'a>(status: Status, doc: &'a DocPalette) -> Option<&'a Tint> {
    match status {
        Status::Different | Status::TypeMismatch | Status::Error => Some(&doc.change),
        Status::LeftOnly => Some(&doc.delete),
        Status::RightOnly => Some(&doc.insert),
        Status::Same | Status::Pending => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    painter: &Painter,
    row: Rect,
    entry: &Entry,
    meta: Option<Meta>,
    expanded: bool,
    selection: Option<Color32>,
    equivalence: Option<Equivalence>,
    doc: &DocPalette,
) {
    let tint = match equivalence {
        Some(_) => Some(&doc.equivalent),
        None => status_tint(entry.status, doc),
    };
    let is_dir = entry.is_dir();
    if let Some(selection) = selection {
        painter.rect_filled(row, 0.0, selection);
    } else if let Some(tint) = tint {
        let fill = match (is_dir, meta.is_some()) {
            // A folder that merely contains differences stays discreet.
            (true, _) if entry.left.is_some() && entry.right.is_some() => mix(doc.canvas, tint.zone, 0.5),
            (_, false) => mix(doc.canvas, tint.zone, 0.5),
            _ => tint.zone,
        };
        painter.rect_filled(row, 0.0, fill);
    }
    let center_y = row.center().y;
    let x = row.left() + 12.0 + (entry.depth.saturating_sub(1)) as f32 * INDENT;
    let dim = doc.line_number;
    let Some(meta) = meta else {
        return;
    };
    if is_dir {
        paint_disclosure(painter, pos2(x, center_y), expanded, dim);
    }
    let icon_x = x + 12.0;
    let icon_color = tint.map_or(dim, |t| t.mark);
    let icon = if is_dir { Icon::Folder } else { Icon::File };
    icons::paint(painter, icon, icons::square(pos2(icon_x, center_y), 16.0), icon_color);
    let right = row.right() - 12.0;
    // In a narrow pane the name matters more than the date.
    let show_date = row.width() >= MIN_WIDTH_FOR_DATE;
    let date_right = right;
    let size_right = if show_date { date_right - DATE_WIDTH } else { right };
    let mut name_right = (size_right - SIZE_WIDTH).max(row.left() + 80.0);
    if let Some(equivalence) = equivalence {
        // Said in words, as the tint alone is a nuance.
        let label = painter.text(
            pos2(size_right - SIZE_WIDTH + 18.0, center_y),
            Align2::RIGHT_CENTER,
            equivalence.label(),
            FontId::proportional(11.5),
            doc.equivalent.mark,
        );
        name_right = name_right.min(label.left() - 10.0).max(row.left() + 80.0);
    }
    let name_clip = Rect::from_min_max(pos2(row.left(), row.top()), pos2(name_right, row.bottom()));
    painter.with_clip_rect(name_clip.intersect(painter.clip_rect())).text(
        pos2(icon_x + 15.0, center_y),
        Align2::LEFT_CENTER,
        entry.name().to_string_lossy(),
        FontId::proportional(13.0),
        if entry.status == Status::Pending {
            dim
        } else {
            doc.text
        },
    );
    if !is_dir {
        painter.text(
            pos2(size_right, center_y),
            Align2::RIGHT_CENTER,
            format_size(meta.size),
            FontId::proportional(12.0),
            dim,
        );
    }
    if let Some(modified) = meta.modified.filter(|_| show_date) {
        painter.text(
            pos2(date_right, center_y),
            Align2::RIGHT_CENTER,
            format_date(modified),
            FontId::proportional(12.0),
            dim,
        );
    }
}

fn paint_disclosure(painter: &Painter, center: Pos2, expanded: bool, color: Color32) {
    let points = if expanded {
        vec![center + vec2(-4.0, -2.0), center + vec2(4.0, -2.0), center + vec2(0.0, 3.0)]
    } else {
        vec![center + vec2(-2.0, -4.0), center + vec2(3.0, 0.0), center + vec2(-2.0, 4.0)]
    };
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Ko", "Mo", "Go", "To"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} o")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_date(time: SystemTime) -> String {
    let date: DateTime<Local> = time.into();
    date.format("%d/%m/%Y %H:%M").to_string()
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 1 {
        format!("{:.2} s", duration.as_secs_f64())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 o");
        assert_eq!(format_size(1023), "1023 o");
        assert_eq!(format_size(1536), "1.5 Ko");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 Mo");
    }
}
