//! Copying and deleting entries of a folder comparison.
//!
//! These operations touch the user's real files, so they work in two steps. `plan` only reads:
//! it turns the selected entries into the exact list of what would be created, overwritten or
//! deleted, for a confirmation dialog. `execute` then applies that list and nothing else, and
//! looks at every file again first: whatever changed since the plan is reported, not overwritten.
//!
//! Only what the comparison shows is ever touched. A folder is copied or deleted through the
//! entries below it, never by walking the disk, so files hidden by a filter stay where they
//! are, and a folder that still holds some of them is kept.
//!
//! The two synchronisations are made of the same steps: a mirror copies one way and deletes
//! what only the destination holds; an update copies each way, the newer file over the older,
//! and never deletes. Their plans mix directions, each item saying which side it reads.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::Side;
use crate::dirscan::{Entry, EntryKind, Meta, Status};

const COPY_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FileOpError {
    #[error("{0} n'est pas un dossier")]
    NotADirectory(PathBuf),
    #[error("les deux côtés désignent le même dossier")]
    SameRoots,
    #[error("sélection invalide : la comparaison a changé, relancez-la")]
    InvalidSelection,
    #[error("{0}: chemin hors du dossier comparé, opération refusée")]
    EscapesRoot(PathBuf),
    #[error("{0}: ce dossier parent est un lien symbolique ou n'est pas un dossier, opération refusée")]
    UnsafeParent(PathBuf),
    #[error("{0}: le dossier parent n'existe pas")]
    MissingParent(PathBuf),
    #[error("{0}: la source et la destination sont le même fichier")]
    SameFile(PathBuf),
    #[error("{0}: un dossier et un fichier portent le même nom, copie refusée")]
    TypeConflict(PathBuf),
    #[error("{0}: type de fichier non pris en charge")]
    Unsupported(PathBuf),
    #[error("{0}: a disparu depuis la comparaison")]
    Vanished(PathBuf),
    #[error("{0}: modifié depuis la confirmation, laissé intact")]
    ChangedSincePlan(PathBuf),
    #[error("{0}: même date des deux côtés, contenus différents : rien n'est copié")]
    SameDateConflict(PathBuf),
    #[error("{0}: date inconnue, impossible de dire quel côté est le plus récent : rien n'est copié")]
    UnknownDateConflict(PathBuf),
    #[error("{path}: mise à la corbeille impossible ({reason})")]
    TrashFailed { path: PathBuf, reason: String },
    #[error("opération annulée")]
    Cancelled,
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMode {
    /// Moves to the system trash; fails, without deleting, where there is no trash.
    Trash,
    /// Removes for good.
    Permanent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Copies from `from` to the other side.
    Copy { from: Side },
    Delete { side: Side, mode: DeleteMode },
    /// Makes `to` identical to the other side: copies there what differs or is missing, and
    /// deletes what only `to` holds, in `mode`.
    Mirror { to: Side, mode: DeleteMode },
    /// Copies each pair that differs from its newer side to its older one, and what one side
    /// alone holds to the other. Deletes nothing; a pair whose newer file cannot be told is left
    /// alone and listed as blocked.
    Update,
}

/// What a file looked like when the plan was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    id: (u64, u64),
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        Self {
            kind,
            size: meta.len(),
            modified: meta.modified().ok(),
            id: file_id(meta),
        }
    }

    /// Folders change size and date whenever something moves inside them, so only their
    /// nature is compared.
    fn unchanged(&self, now: &Stamp) -> bool {
        self.kind == now.kind && (self.kind == EntryKind::Dir || self == now)
    }

    fn matches(&self, meta: &Meta) -> bool {
        self.kind == meta.kind && self.size == meta.size && self.modified == meta.modified
    }
}

#[cfg(unix)]
fn file_id(meta: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_id(_: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A folder missing on the destination side.
    CreateDir,
    /// A file or link missing on the destination side.
    Create,
    /// A file or link replacing the one on the destination side.
    Overwrite,
    /// A file or link to delete.
    Delete,
    /// A folder to delete once emptied; kept if something outside the plan remains in it.
    DeleteDir,
}

/// Which of the two files of an overwrite was modified last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Newer {
    Source,
    /// The copy would replace a more recent file.
    Destination,
    SameDate,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct PlanItem {
    /// Index of the entry in the comparison tree.
    pub index: u32,
    pub rel: PathBuf,
    pub action: Action,
    /// The side a copy reads, or the side a deletion empties.
    pub from: Side,
    /// The item read by a copy, or removed by a deletion.
    pub source: Stamp,
    /// The item an `Overwrite` replaces.
    pub dest: Option<Stamp>,
    /// Only meaningful for `Overwrite`.
    pub newer: Newer,
    /// For `DeleteDir`: direct children on disk that are not part of the plan (hidden by a
    /// filter, or created since the comparison). Not zero means the folder will be kept.
    pub unlisted: u64,
}

/// A selected entry the plan leaves alone, and why.
#[derive(Debug)]
pub struct Blocked {
    pub index: u32,
    pub rel: PathBuf,
    pub reason: FileOpError,
}

impl Totals {
    fn add(&mut self, other: &Totals) {
        self.create_dirs += other.create_dirs;
        self.creates += other.creates;
        self.overwrites += other.overwrites;
        self.overwrites_of_newer += other.overwrites_of_newer;
        self.deletes += other.deletes;
        self.delete_dirs += other.delete_dirs;
        self.dirs_kept += other.dirs_kept;
        self.identical += other.identical;
        self.blocked += other.blocked;
        self.bytes_to_copy += other.bytes_to_copy;
        self.bytes_replaced += other.bytes_replaced;
        self.bytes_to_delete += other.bytes_to_delete;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub create_dirs: usize,
    pub creates: usize,
    pub overwrites: usize,
    /// Overwrites that replace a more recent file.
    pub overwrites_of_newer: usize,
    pub deletes: usize,
    /// Folders expected to disappear.
    pub delete_dirs: usize,
    /// Folders that will remain because they hold items outside the plan.
    pub dirs_kept: usize,
    /// Files skipped by a copy because both sides are already identical.
    pub identical: usize,
    pub blocked: usize,
    pub bytes_to_copy: u64,
    /// Bytes of the destination files that overwrites replace.
    pub bytes_replaced: u64,
    pub bytes_to_delete: u64,
}

#[derive(Debug)]
pub struct Plan {
    pub operation: Operation,
    pub roots: [PathBuf; 2],
    /// Parents always come before what they contain.
    pub items: Vec<PlanItem>,
    pub blocked: Vec<Blocked>,
    pub totals: Totals,
}

impl Plan {
    /// True when executing would do nothing.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Every entry whose state may differ after `execute`, for `dirscan::recheck`.
    pub fn touched(&self) -> Vec<(u32, PathBuf)> {
        let items = self.items.iter().map(|item| (item.index, item.rel.clone()));
        let blocked = self.blocked.iter().map(|b| (b.index, b.rel.clone()));
        items.chain(blocked).collect()
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> FileOpError + '_ {
    move |source| FileOpError::Io {
        path: path.to_owned(),
        source,
    }
}

/// The state of `path` itself, never of what a link points to. `None` when nothing is there.
fn lstat(path: &Path) -> Result<Option<Stamp>, FileOpError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(Stamp::of(&meta))),
        Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(None),
        Err(error) => Err(io_error(path)(error)),
    }
}

/// A relative path may only go down: no `..`, no root, no drive.
fn safe_rel(rel: &Path) -> Result<(), FileOpError> {
    let mut components = rel.components().peekable();
    if components.peek().is_none() || !components.all(|c| matches!(c, Component::Normal(_))) {
        return Err(FileOpError::EscapesRoot(rel.to_owned()));
    }
    Ok(())
}

/// The entries above `index`, outermost first, checked against the path so that a tree that
/// does not describe `rel` is refused rather than trusted.
fn parents(entries: &[Entry], index: u32) -> Result<Vec<u32>, FileOpError> {
    let mut chain = Vec::new();
    let mut entry = &entries[index as usize];
    let mut path = entry.rel.parent().filter(|p| !p.as_os_str().is_empty());
    loop {
        match (entry.parent, path) {
            (None, None) => break,
            (Some(parent), Some(expected)) if parent < index => {
                entry = &entries[parent as usize];
                if entry.rel != expected {
                    return Err(FileOpError::InvalidSelection);
                }
                chain.push(parent);
                path = expected.parent().filter(|p| !p.as_os_str().is_empty());
            }
            _ => return Err(FileOpError::InvalidSelection),
        }
    }
    chain.reverse();
    Ok(chain)
}

struct Planner<'a> {
    entries: &'a [Entry],
    /// Root read by a copy, or emptied by a deletion.
    source_root: &'a Path,
    dest_root: &'a Path,
    source_side: Side,
    /// Folders already known to be real folders (or planned) where the operation writes.
    ready: HashSet<u32>,
    items: Vec<PlanItem>,
    totals: Totals,
}

impl Planner<'_> {
    fn push(&mut self, index: u32, action: Action, source: Stamp, dest: Option<Stamp>) {
        let newer = match (action, source.modified, dest.and_then(|d| d.modified)) {
            (Action::Overwrite, Some(a), Some(b)) if a > b => Newer::Source,
            (Action::Overwrite, Some(a), Some(b)) if a < b => Newer::Destination,
            (Action::Overwrite, Some(_), Some(_)) => Newer::SameDate,
            _ => Newer::Unknown,
        };
        self.items.push(PlanItem {
            index,
            rel: self.entries[index as usize].rel.clone(),
            action,
            from: self.source_side,
            source,
            dest,
            newer,
            unlisted: 0,
        });
    }

    /// Makes sure the folder of entry `index` will exist on the destination side, planning its
    /// creation when it is missing. Refuses to go through anything that is not a real folder.
    fn copy_dir(&mut self, index: u32) -> Result<(), FileOpError> {
        if self.ready.contains(&index) {
            return Ok(());
        }
        let entries = self.entries;
        let rel = &entries[index as usize].rel;
        let (source_path, dest_path) = (self.source_root.join(rel), self.dest_root.join(rel));
        let source = match lstat(&source_path)? {
            Some(stamp) if stamp.kind == EntryKind::Dir => stamp,
            Some(_) => return Err(FileOpError::UnsafeParent(source_path)),
            None => return Err(FileOpError::Vanished(source_path)),
        };
        match lstat(&dest_path)? {
            None => {
                self.push(index, Action::CreateDir, source, None);
                self.totals.create_dirs += 1;
            }
            Some(stamp) if stamp.kind == EntryKind::Dir => {}
            Some(stamp) if stamp.kind == EntryKind::Symlink => return Err(FileOpError::UnsafeParent(dest_path)),
            Some(_) => return Err(FileOpError::TypeConflict(dest_path)),
        }
        self.ready.insert(index);
        Ok(())
    }

    fn copy_entry(&mut self, index: u32) -> Result<(), FileOpError> {
        let entries = self.entries;
        let entry = &entries[index as usize];
        safe_rel(&entry.rel)?;
        for parent in parents(entries, index)? {
            self.copy_dir(parent)?;
        }
        let (source_path, dest_path) = (self.source_root.join(&entry.rel), self.dest_root.join(&entry.rel));
        let source = lstat(&source_path)?.ok_or_else(|| FileOpError::Vanished(source_path.clone()))?;
        match source.kind {
            EntryKind::Dir => return self.copy_dir(index),
            EntryKind::Other => return Err(FileOpError::Unsupported(source_path)),
            EntryKind::File | EntryKind::Symlink => {}
        }
        let dest = lstat(&dest_path)?;
        match dest {
            Some(stamp) if stamp.kind == EntryKind::Dir => return Err(FileOpError::TypeConflict(dest_path)),
            Some(stamp) if stamp.kind == EntryKind::Other => return Err(FileOpError::Unsupported(dest_path)),
            // Two names of one inode: copying would at best do nothing.
            Some(stamp) if cfg!(unix) && stamp.id == source.id => return Err(FileOpError::SameFile(dest_path)),
            _ => {}
        }
        // The comparison already read both files; they are skipped as long as neither moved.
        let (source_meta, dest_meta) = match self.source_side {
            Side::Left => (entry.left, entry.right),
            Side::Right => (entry.right, entry.left),
        };
        let compared = |stamp: Option<Stamp>, meta: Option<Meta>| stamp.zip(meta).is_some_and(|(s, m)| s.matches(&m));
        if entry.status == Status::Same && compared(Some(source), source_meta) && compared(dest, dest_meta) {
            self.totals.identical += 1;
            return Ok(());
        }
        if source.kind == EntryKind::File {
            self.totals.bytes_to_copy += source.size;
        }
        match dest {
            None => {
                self.totals.creates += 1;
                self.push(index, Action::Create, source, None);
            }
            Some(stamp) => {
                self.totals.overwrites += 1;
                self.totals.bytes_replaced += stamp.size;
                self.push(index, Action::Overwrite, source, dest);
                if self.items.last().is_some_and(|item| item.newer == Newer::Destination) {
                    self.totals.overwrites_of_newer += 1;
                }
            }
        }
        Ok(())
    }

    fn delete_entry(&mut self, index: u32) -> Result<(), FileOpError> {
        let entries = self.entries;
        let entry = &entries[index as usize];
        safe_rel(&entry.rel)?;
        for parent in parents(entries, index)? {
            if !self.ready.contains(&parent) {
                let path = self.source_root.join(&entries[parent as usize].rel);
                if !lstat(&path)?.is_some_and(|stamp| stamp.kind == EntryKind::Dir) {
                    return Err(FileOpError::UnsafeParent(path));
                }
                self.ready.insert(parent);
            }
        }
        let path = self.source_root.join(&entry.rel);
        let stamp = lstat(&path)?.ok_or(FileOpError::Vanished(path))?;
        if stamp.kind == EntryKind::Dir {
            self.push(index, Action::DeleteDir, stamp, None);
        } else {
            self.totals.deletes += 1;
            self.totals.bytes_to_delete += stamp.size;
            self.push(index, Action::Delete, stamp, None);
        }
        Ok(())
    }

    /// Counts what each folder holds besides the plan, and which folders will therefore stay.
    fn count_unlisted(&mut self) {
        let planned: HashSet<&Path> = self.items.iter().map(|item| item.rel.as_path()).collect();
        let unlisted: Vec<u64> = self
            .items
            .iter()
            .map(|item| {
                if item.action != Action::DeleteDir {
                    return 0;
                }
                let Ok(children) = fs::read_dir(self.source_root.join(&item.rel)) else {
                    return 0;
                };
                children
                    .filter(|child| match child {
                        Ok(child) => !planned.contains(item.rel.join(child.file_name()).as_path()),
                        Err(_) => true,
                    })
                    .count() as u64
            })
            .collect();
        drop(planned);
        let mut kept: HashSet<PathBuf> = HashSet::new();
        for (item, unlisted) in self.items.iter_mut().zip(unlisted).rev() {
            if item.action != Action::DeleteDir {
                continue;
            }
            item.unlisted = unlisted;
            if unlisted > 0 || kept.contains(&item.rel) {
                self.totals.dirs_kept += 1;
                if let Some(parent) = item.rel.parent() {
                    kept.insert(parent.to_owned());
                }
            } else {
                self.totals.delete_dirs += 1;
            }
        }
    }
}

/// Lists what `operation` would do to the selected entries and everything below them, without
/// touching anything. `entries` is the comparison tree and `selected` holds indices into it.
///
/// A copy takes the entries present on the source side: folders missing on the other side are
/// created, files are created or overwritten, files the comparison found identical are skipped,
/// and entries that only exist on the destination side are left alone. A deletion takes the
/// entries present on the given side. A mirror is the copy plus the deletion of the entries only
/// the destination holds; an update copies each pair from its newer side and each one-sided
/// entry to the other side.
pub fn plan(
    roots: &[PathBuf; 2],
    entries: &[Entry],
    selected: &[u32],
    operation: Operation,
) -> Result<Plan, FileOpError> {
    for root in roots {
        if !root.is_dir() {
            return Err(FileOpError::NotADirectory(root.clone()));
        }
    }
    if !matches!(operation, Operation::Delete { .. }) {
        let resolved = |root: &PathBuf| fs::canonicalize(root).map_err(io_error(root));
        if resolved(&roots[0])? == resolved(&roots[1])? {
            return Err(FileOpError::SameRoots);
        }
    }

    let mut chosen = vec![false; entries.len()];
    for &index in selected {
        let entry = entries.get(index as usize).ok_or(FileOpError::InvalidSelection)?;
        let end = (entry.subtree_end as usize).clamp(index as usize + 1, entries.len());
        chosen[index as usize..end].fill(true);
    }

    // One planner per side read: copies from it, or deletions on it.
    let planner = |side: Side| Planner {
        entries,
        source_root: &roots[side.index()],
        dest_root: &roots[side.other().index()],
        source_side: side,
        ready: HashSet::new(),
        items: Vec::new(),
        totals: Totals::default(),
    };
    let mut copiers = [planner(Side::Left), planner(Side::Right)];
    let mut deleter: Option<Planner> = None;
    let mut blocked = Vec::new();
    for index in (0..entries.len() as u32).filter(|&index| chosen[index as usize]) {
        let entry = &entries[index as usize];
        let present = |side: Side| match side {
            Side::Left => entry.left.is_some(),
            Side::Right => entry.right.is_some(),
        };
        let result = match operation {
            Operation::Copy { from } if present(from) => copiers[from.index()].copy_entry(index),
            Operation::Delete { side, .. } if present(side) => deleter.get_or_insert_with(|| planner(side)).delete_entry(index),
            Operation::Mirror { to, .. } if present(to.other()) => copiers[to.other().index()].copy_entry(index),
            Operation::Mirror { to, .. } if present(to) => deleter.get_or_insert_with(|| planner(to)).delete_entry(index),
            Operation::Update => match (present(Side::Left), present(Side::Right)) {
                (true, false) => copiers[0].copy_entry(index),
                (false, true) => copiers[1].copy_entry(index),
                (true, true) => match update_side(roots, entry) {
                    Ok(Some(newer)) => copiers[newer.index()].copy_entry(index),
                    Ok(None) => Ok(()),
                    Err(reason) => Err(reason),
                },
                (false, false) => Ok(()),
            },
            _ => Ok(()),
        };
        if let Err(reason) = result {
            blocked.push(Blocked {
                index,
                rel: entry.rel.clone(),
                reason,
            });
        }
    }
    if let Some(deleter) = &mut deleter {
        deleter.count_unlisted();
    }
    let mut items = Vec::new();
    let mut totals = Totals::default();
    for planner in copiers.into_iter().chain(deleter) {
        items.extend(planner.items);
        totals.add(&planner.totals);
    }
    // A folder created for a file deep in the tree is planned when that file is reached, which
    // can be after entries that sort before it; parents must still come first.
    items.sort_by_key(|item| item.index);
    totals.blocked = blocked.len();
    Ok(Plan {
        operation,
        roots: roots.clone(),
        items,
        blocked,
        totals,
    })
}

/// The side an update copies a pair present on both sides from: the newer one. `None` when
/// there is nothing to copy (identical files, two folders whose content decides). A tie or an
/// unknown date is a conflict: nobody can tell which version is wanted.
fn update_side(roots: &[PathBuf; 2], entry: &Entry) -> Result<Option<Side>, FileOpError> {
    safe_rel(&entry.rel)?;
    if entry.status == Status::Same {
        return Ok(None);
    }
    let [left, right] = [&roots[0], &roots[1]].map(|root| root.join(&entry.rel));
    let (Some(left_stamp), Some(right_stamp)) = (lstat(&left)?, lstat(&right)?) else {
        return Err(FileOpError::Vanished(left));
    };
    match (left_stamp.kind, right_stamp.kind) {
        (EntryKind::Dir, EntryKind::Dir) => return Ok(None),
        (EntryKind::Dir, _) | (_, EntryKind::Dir) => return Err(FileOpError::TypeConflict(left)),
        _ => {}
    }
    match (left_stamp.modified, right_stamp.modified) {
        (Some(a), Some(b)) if a > b => Ok(Some(Side::Left)),
        (Some(a), Some(b)) if a < b => Ok(Some(Side::Right)),
        (Some(_), Some(_)) => Err(FileOpError::SameDateConflict(left)),
        _ => Err(FileOpError::UnknownDateConflict(left)),
    }
}

/// Where deleted items go in `DeleteMode::Trash`. A trait so that tests never fill the real
/// trash, and so that a platform without one can say so.
pub trait Trasher {
    /// Moves `path` itself (never the target of a link) to the trash.
    fn trash(&self, path: &Path) -> Result<(), String>;
}

/// The trash of the desktop: FreeDesktop trash on Linux, Recycle Bin on Windows, Trash on macOS.
pub struct SystemTrash;

impl Trasher for SystemTrash {
    fn trash(&self, path: &Path) -> Result<(), String> {
        trash::delete(path).map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Progress<'a> {
    pub items_done: usize,
    pub items_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// Relative path of the item being processed.
    pub current: &'a Path,
}

#[derive(Debug)]
pub struct ItemError {
    pub index: u32,
    pub rel: PathBuf,
    pub error: FileOpError,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// Plan items carried out.
    pub completed: usize,
    pub bytes_copied: u64,
    /// One per item that failed; the others were still attempted.
    pub errors: Vec<ItemError>,
    /// Folders (relative) a deletion left in place because they are not empty.
    pub kept_dirs: Vec<PathBuf>,
    /// True when `cancel` stopped the run before the end of the plan.
    pub cancelled: bool,
}

/// Carries out a plan. Blocks until done; meant for a worker thread.
///
/// `progress` is called before each item and while large files are copied. `cancel` is checked
/// between items and between blocks of a large file, whose destination is then left as it was.
/// An item that fails is recorded in `Outcome::errors` and the run goes on with the next one.
/// `trasher` is only used by `DeleteMode::Trash`.
pub fn execute(
    plan: &Plan,
    trasher: &dyn Trasher,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&Progress<'_>),
) -> Outcome {
    let bytes_total: u64 = plan
        .items
        .iter()
        .filter(|item| item.source.kind == EntryKind::File)
        .map(|item| item.source.size)
        .sum();
    let mut run = Run {
        plan,
        cancel,
        progress,
        safe_dirs: HashSet::new(),
        items_done: 0,
        bytes_done: 0,
        bytes_total,
        outcome: Outcome::default(),
    };
    let all: Vec<&PlanItem> = plan.items.iter().collect();
    match plan.operation {
        Operation::Copy { .. } | Operation::Update => run.copy(&all),
        Operation::Delete { side, mode } => run.delete(&all, side, mode, trasher),
        Operation::Mirror { to, mode } => {
            // Copies first: a mirror interrupted halfway has lost nothing yet.
            let (deletions, copies): (Vec<&PlanItem>, Vec<&PlanItem>) =
                all.into_iter().partition(|item| matches!(item.action, Action::Delete | Action::DeleteDir));
            run.copy(&copies);
            if !run.cancelled() {
                run.delete(&deletions, to, mode, trasher);
            }
        }
    }
    run.report(Path::new(""));
    run.outcome
}

struct Run<'a> {
    plan: &'a Plan,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(&Progress<'_>),
    /// Folders already checked to be real folders, by full path.
    safe_dirs: HashSet<PathBuf>,
    items_done: usize,
    bytes_done: u64,
    bytes_total: u64,
    outcome: Outcome,
}

fn item_bytes(item: &PlanItem) -> u64 {
    if item.source.kind == EntryKind::File {
        item.source.size
    } else {
        0
    }
}

impl Run<'_> {
    fn report(&mut self, current: &Path) {
        (self.progress)(&Progress {
            items_done: self.items_done,
            items_total: self.plan.items.len(),
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            current,
        });
    }

    fn cancelled(&mut self) -> bool {
        self.outcome.cancelled |= self.cancel.load(Ordering::Relaxed);
        self.outcome.cancelled
    }

    /// Books the result of one item. `bytes_after` realigns the byte counter, which a failed
    /// copy leaves in the middle of a file.
    fn finish(&mut self, item: &PlanItem, result: Result<(), FileOpError>, bytes_after: u64) {
        match result {
            Ok(()) => self.outcome.completed += 1,
            Err(FileOpError::Cancelled) => {
                self.outcome.cancelled = true;
                return;
            }
            Err(error) => self.outcome.errors.push(ItemError {
                index: item.index,
                rel: item.rel.clone(),
                error,
            }),
        }
        self.items_done += 1;
        self.bytes_done = bytes_after;
    }

    /// Every folder between `root` and `rel` must be a real folder: going through a link could
    /// lead out of the compared tree.
    fn check_parents(&mut self, root: &Path, rel: &Path) -> Result<(), FileOpError> {
        safe_rel(rel)?;
        let mut path = root.to_path_buf();
        for component in rel.parent().into_iter().flat_map(Path::components) {
            path.push(component);
            if self.safe_dirs.contains(&path) {
                continue;
            }
            match lstat(&path)? {
                Some(stamp) if stamp.kind == EntryKind::Dir => self.safe_dirs.insert(path.clone()),
                Some(_) => return Err(FileOpError::UnsafeParent(path)),
                None => return Err(FileOpError::MissingParent(path)),
            };
        }
        Ok(())
    }

    fn copy(&mut self, items: &[&PlanItem]) {
        let plan = self.plan;
        let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
        let mut created: Vec<(PathBuf, PathBuf)> = Vec::new();
        for &item in items {
            if self.cancelled() {
                break;
            }
            let source_root = &plan.roots[item.from.index()];
            let dest_root = &plan.roots[item.from.other().index()];
            self.report(&item.rel);
            let bytes_after = self.bytes_done + item_bytes(item);
            let source = source_root.join(&item.rel);
            let dest = dest_root.join(&item.rel);
            let result = (|| {
                self.check_parents(source_root, &item.rel)?;
                self.check_parents(dest_root, &item.rel)?;
                let now = lstat(&source)?.ok_or_else(|| FileOpError::Vanished(source.clone()))?;
                if !item.source.unchanged(&now) {
                    return Err(FileOpError::ChangedSincePlan(source.clone()));
                }
                let dest_now = lstat(&dest)?;
                match (item.action, item.source.kind) {
                    (Action::CreateDir, _) => match dest_now {
                        None => {
                            fs::create_dir(&dest).map_err(io_error(&dest))?;
                            created.push((source.clone(), dest.clone()));
                            Ok(())
                        }
                        // Someone created it meanwhile: it is theirs, its attributes stay.
                        Some(stamp) if stamp.kind == EntryKind::Dir => Ok(()),
                        Some(_) => Err(FileOpError::ChangedSincePlan(dest.clone())),
                    },
                    (Action::Create | Action::Overwrite, kind) => {
                        let expected = match (item.dest, dest_now) {
                            (None, None) => true,
                            (Some(planned), Some(now)) => planned.kind != EntryKind::Dir && planned == now,
                            _ => false,
                        };
                        if !expected {
                            return Err(FileOpError::ChangedSincePlan(dest.clone()));
                        }
                        if kind == EntryKind::Symlink {
                            copy_symlink(&source, &dest)
                        } else {
                            self.copy_file(item, &source, &dest, &mut buffer)
                        }
                    }
                    (Action::Delete | Action::DeleteDir, _) => Err(FileOpError::InvalidSelection),
                }
            })();
            if result.is_ok() {
                self.outcome.bytes_copied += item_bytes(item);
            }
            self.finish(item, result, bytes_after);
        }
        // Attributes of new folders come last and deepest first: filling a folder changes its
        // date, and a read-only folder could not be filled. Best effort, like any attribute.
        for (source, dest) in created.iter().rev() {
            let Ok(meta) = fs::symlink_metadata(source) else {
                continue;
            };
            if let (Ok(modified), Ok(dir)) = (meta.modified(), File::open(dest)) {
                let _ = dir.set_modified(modified);
            }
            let _ = fs::set_permissions(dest, meta.permissions());
        }
    }

    /// Copies through a temporary file next to the destination, then renames it over the real
    /// name: an interrupted copy leaves the previous destination untouched.
    fn copy_file(
        &mut self,
        item: &PlanItem,
        source: &Path,
        dest: &Path,
        buffer: &mut [u8],
    ) -> Result<(), FileOpError> {
        let mut input = File::open(source).map_err(io_error(source))?;
        let meta = input.metadata().map_err(io_error(source))?;
        // Also catches a link slipped in place of the file after it was checked: the opened
        // file would then not be the planned one.
        if Stamp::of(&meta) != item.source {
            return Err(FileOpError::ChangedSincePlan(source.to_owned()));
        }
        let tmp = temporary_name(dest);
        let _ = fs::remove_file(&tmp);
        let result = (|| {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(io_error(&tmp))?;
            let mut copied = 0u64;
            loop {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(FileOpError::Cancelled);
                }
                let read = match input.read(buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(io_error(source)(error)),
                };
                output.write_all(&buffer[..read]).map_err(io_error(&tmp))?;
                copied += read as u64;
                self.bytes_done += read as u64;
                if copied < item.source.size {
                    self.report(&item.rel);
                }
            }
            let after = input.metadata().map_err(io_error(source))?;
            if copied != item.source.size || Stamp::of(&after) != item.source {
                return Err(FileOpError::ChangedSincePlan(source.to_owned()));
            }
            output.set_permissions(meta.permissions()).map_err(io_error(&tmp))?;
            if let Some(modified) = item.source.modified {
                output.set_modified(modified).map_err(io_error(&tmp))?;
            }
            output.sync_all().map_err(io_error(&tmp))?;
            drop(output);
            make_replaceable(dest);
            fs::rename(&tmp, dest).map_err(io_error(dest))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn delete(&mut self, items: &[&PlanItem], side: Side, mode: DeleteMode, trasher: &dyn Trasher) {
        let plan = self.plan;
        let root = &plan.roots[side.index()];
        let planned: HashMap<&Path, &PlanItem> = match mode {
            DeleteMode::Trash => items.iter().map(|item| (item.rel.as_path(), *item)).collect(),
            DeleteMode::Permanent => HashMap::new(),
        };
        let remove = |path: &Path, kind: EntryKind| match mode {
            DeleteMode::Trash => trasher.trash(path).map_err(|reason| FileOpError::TrashFailed {
                path: path.to_owned(),
                reason,
            }),
            DeleteMode::Permanent if kind == EntryKind::Dir => fs::remove_dir(path).map_err(io_error(path)),
            DeleteMode::Permanent => remove_file_or_link(path).map_err(io_error(path)),
        };

        // Files first, folders afterwards and deepest first, each removed only once empty, so
        // nothing outside the plan can ever go with them. The exception is the trash, where a
        // folder whose whole content is planned is moved in one piece, to be restorable so.
        let mut folders: Vec<&PlanItem> = Vec::new();
        let mut moved: Option<(&Path, bool)> = None;
        for &item in items {
            if self.cancelled() {
                break;
            }
            let bytes_after = self.bytes_done + item_bytes(item);
            if let Some((_, ok)) = moved.filter(|(dir, _)| item.rel.starts_with(dir)) {
                // Went (or failed) with the folder above it, which already told the story.
                if ok {
                    self.outcome.completed += 1;
                }
                self.items_done += 1;
                self.bytes_done = bytes_after;
                continue;
            }
            self.report(&item.rel);
            let path = root.join(&item.rel);
            let result = (|| {
                self.check_parents(root, &item.rel)?;
                let now = lstat(&path)?;
                match item.action {
                    Action::Delete => {
                        if now != Some(item.source) {
                            return Err(FileOpError::ChangedSincePlan(path.clone()));
                        }
                        remove(&path, item.source.kind)
                    }
                    Action::DeleteDir => {
                        if now.is_none_or(|stamp| stamp.kind != EntryKind::Dir) {
                            return Err(FileOpError::ChangedSincePlan(path.clone()));
                        }
                        if mode == DeleteMode::Trash && holds_only_planned(&path, &item.rel, &planned) {
                            let result = remove(&path, EntryKind::Dir);
                            moved = Some((item.rel.as_path(), result.is_ok()));
                            return result;
                        }
                        folders.push(item);
                        Ok(())
                    }
                    _ => Err(FileOpError::InvalidSelection),
                }
            })();
            // A folder put aside is booked when it is actually removed, below.
            let put_aside = result.is_ok() && folders.last().is_some_and(|last| std::ptr::eq(*last, item));
            if !put_aside {
                self.finish(item, result, bytes_after);
            }
        }
        for item in folders.into_iter().rev() {
            let path = root.join(&item.rel);
            // Cancelling keeps the folders, but those are still accounted for as kept.
            let empty = fs::read_dir(&path).is_ok_and(|mut children| children.next().is_none());
            if !empty || self.outcome.cancelled {
                self.outcome.kept_dirs.push(item.rel.clone());
                self.items_done += 1;
                continue;
            }
            let result = self
                .check_parents(root, &item.rel)
                .and_then(|()| remove(&path, EntryKind::Dir));
            let bytes = self.bytes_done;
            self.finish(item, result, bytes);
        }
        self.outcome.kept_dirs.reverse();
    }
}

/// True when everything under `dir` is part of the plan and still as planned, so that moving
/// the folder as a whole takes nothing else along.
fn holds_only_planned(dir: &Path, rel: &Path, planned: &HashMap<&Path, &PlanItem>) -> bool {
    let Ok(children) = fs::read_dir(dir) else {
        return false;
    };
    children.into_iter().all(|child| {
        let Ok(child) = child else {
            return false;
        };
        let child_rel = rel.join(child.file_name());
        let (Some(item), Ok(Some(now))) = (planned.get(child_rel.as_path()), lstat(&child.path())) else {
            return false;
        };
        item.source.unchanged(&now)
            && (now.kind != EntryKind::Dir || holds_only_planned(&child.path(), &child_rel, planned))
    })
}

fn temporary_name(dest: &Path) -> PathBuf {
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    dest.with_file_name(format!(".{name}.biomerge-{}.tmp", std::process::id()))
}

/// Recreates the link itself; what it points to is never read.
fn copy_symlink(source: &Path, dest: &Path) -> Result<(), FileOpError> {
    let target = fs::read_link(source).map_err(io_error(source))?;
    let tmp = temporary_name(dest);
    let _ = fs::remove_file(&tmp);
    make_symlink(&target, &tmp, source).map_err(io_error(&tmp))?;
    make_replaceable(dest);
    fs::rename(&tmp, dest).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        io_error(dest)(error)
    })
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path, _source: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path, source: &Path) -> io::Result<()> {
    if fs::metadata(source).is_ok_and(|meta| meta.is_dir()) {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_: &Path, _: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Windows refuses to rename over a read-only file; elsewhere only the folder's rights count.
#[cfg(windows)]
fn make_replaceable(dest: &Path) {
    if let Ok(meta) = fs::symlink_metadata(dest) {
        let mut permissions = meta.permissions();
        if meta.is_file() && permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(dest, permissions);
        }
    }
}

#[cfg(not(windows))]
fn make_replaceable(_: &Path) {}

/// Removes a file or the link itself. On Windows a link to a folder is removed as a folder.
fn remove_file_or_link(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(_) if cfg!(windows) && fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) => {
            fs::remove_dir(path)
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirscan::{self, NameFilter, ScanEvent, ScanOptions};
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    const LEFT_TO_RIGHT: Operation = Operation::Copy { from: Side::Left };
    const DELETE_LEFT: Operation = Operation::Delete {
        side: Side::Left,
        mode: DeleteMode::Permanent,
    };
    const TRASH_LEFT: Operation = Operation::Delete {
        side: Side::Left,
        mode: DeleteMode::Trash,
    };

    /// Two compared folders inside one temporary directory, plus room for an "outside".
    struct Fixture {
        dir: tempfile::TempDir,
        roots: [PathBuf; 2],
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let roots = [dir.path().join("left"), dir.path().join("right")];
            roots.iter().for_each(|root| fs::create_dir(root).unwrap());
            Self { dir, roots }
        }

        fn left(&self, rel: &str) -> PathBuf {
            self.roots[0].join(rel)
        }

        fn right(&self, rel: &str) -> PathBuf {
            self.roots[1].join(rel)
        }

        fn scan(&self) -> Vec<Entry> {
            self.scan_with(ScanOptions::default())
        }

        fn scan_with(&self, options: ScanOptions) -> Vec<Entry> {
            let (tx, rx) = mpsc::channel();
            let cancel = Arc::new(AtomicBool::new(false));
            dirscan::scan(self.roots[0].clone(), self.roots[1].clone(), options, cancel, tx, || {});
            let mut entries = Vec::new();
            for event in rx {
                match event {
                    ScanEvent::Tree { entries: tree, .. } => entries = tree,
                    ScanEvent::Progress(batch) => {
                        batch.into_iter().for_each(|(index, status)| entries[index as usize].status = status);
                    }
                    ScanEvent::Done { .. } => {}
                    ScanEvent::Failed(message) => panic!("{message}"),
                }
            }
            dirscan::aggregate(&mut entries);
            entries
        }

        fn plan(&self, entries: &[Entry], selected: &[&str], operation: Operation) -> Plan {
            let selected: Vec<u32> = selected.iter().map(|rel| index_of(entries, rel)).collect();
            plan(&self.roots, entries, &selected, operation).unwrap()
        }

        /// Plans the whole tree and executes it.
        fn run_all(&self, operation: Operation) -> (Plan, Outcome) {
            let entries = self.scan();
            let all: Vec<u32> = (0..entries.len() as u32).collect();
            let plan = plan(&self.roots, &entries, &all, operation).unwrap();
            let outcome = run(&plan);
            (plan, outcome)
        }
    }

    fn write(path: PathBuf, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn index_of(entries: &[Entry], rel: &str) -> u32 {
        entries
            .iter()
            .position(|e| e.rel == Path::new(rel))
            .unwrap_or_else(|| panic!("{rel} missing")) as u32
    }

    fn set_modified(path: &Path, seconds_ago: u64) -> SystemTime {
        let time = SystemTime::now() - Duration::from_secs(seconds_ago);
        File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
        fs::metadata(path).unwrap().modified().unwrap()
    }

    /// Fails the test if a deletion tries to use the trash.
    struct NoTrash;

    impl Trasher for NoTrash {
        fn trash(&self, path: &Path) -> Result<(), String> {
            panic!("trash used for {}", path.display());
        }
    }

    /// Records what it is given, and deletes it for real unless told to fail.
    #[derive(Default)]
    struct FakeTrash {
        calls: RefCell<Vec<PathBuf>>,
        fail: bool,
    }

    impl Trasher for FakeTrash {
        fn trash(&self, path: &Path) -> Result<(), String> {
            self.calls.borrow_mut().push(path.to_owned());
            if self.fail {
                return Err("pas de corbeille ici".into());
            }
            let removed = if fs::symlink_metadata(path).unwrap().is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            removed.map_err(|error| error.to_string())
        }
    }

    fn run(plan: &Plan) -> Outcome {
        execute(plan, &NoTrash, &AtomicBool::new(false), &mut |_| {})
    }

    fn actions(plan: &Plan) -> Vec<(&str, Action)> {
        plan.items.iter().map(|item| (item.rel.to_str().unwrap(), item.action)).collect()
    }

    /// Names left in a folder, to prove no temporary file stays behind.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|child| child.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn plan_describes_creations_and_overwrites_without_touching_anything() {
        let f = Fixture::new();
        write(f.left("new.txt"), b"fresh");
        write(f.left("older.txt"), b"left is older");
        write(f.right("older.txt"), b"right");
        write(f.left("newer.txt"), b"left");
        write(f.right("newer.txt"), b"right is older");
        write(f.left("same.txt"), b"equal");
        write(f.right("same.txt"), b"equal");
        write(f.left("sub/deep.txt"), b"deep");
        write(f.right("only-right.txt"), b"stays");
        let old = set_modified(&f.left("older.txt"), 3600);
        set_modified(&f.right("newer.txt"), 3600);

        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let plan = plan(&f.roots, &entries, &all, LEFT_TO_RIGHT).unwrap();
        assert_eq!(
            actions(&plan),
            [
                ("sub", Action::CreateDir),
                ("sub/deep.txt", Action::Create),
                ("new.txt", Action::Create),
                ("newer.txt", Action::Overwrite),
                ("older.txt", Action::Overwrite),
            ]
        );
        let item = |rel: &str| plan.items.iter().find(|item| item.rel == Path::new(rel)).unwrap();
        assert_eq!(item("newer.txt").newer, Newer::Source);
        assert_eq!(item("older.txt").newer, Newer::Destination);
        assert_eq!(item("older.txt").source.size, 13);
        assert_eq!(item("older.txt").source.modified, Some(old));
        assert_eq!(item("older.txt").dest.unwrap().size, 5);
        assert_eq!(
            plan.totals,
            Totals {
                create_dirs: 1,
                creates: 2,
                overwrites: 2,
                overwrites_of_newer: 1,
                identical: 1,
                bytes_to_copy: 4 + 5 + 4 + 13,
                bytes_replaced: 14 + 5,
                ..Totals::default()
            }
        );
        assert!(plan.blocked.is_empty());
        assert!(!f.right("new.txt").exists());
        assert_eq!(fs::read(f.right("older.txt")).unwrap(), b"right");
    }

    #[test]
    fn copy_overwrites_and_keeps_the_modification_time() {
        let f = Fixture::new();
        write(f.left("a.txt"), b"new content");
        write(f.right("a.txt"), b"old");
        let modified = set_modified(&f.left("a.txt"), 86_400);
        let (plan, outcome) = f.run_all(LEFT_TO_RIGHT);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!((outcome.completed, outcome.bytes_copied), (plan.items.len(), 11));
        assert_eq!(fs::read(f.right("a.txt")).unwrap(), b"new content");
        assert_eq!(fs::metadata(f.right("a.txt")).unwrap().modified().unwrap(), modified);
        assert_eq!(names(&f.roots[1]), ["a.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn copy_keeps_permissions_even_over_a_read_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        write(f.left("run.sh"), b"#!/bin/sh\n");
        write(f.right("run.sh"), b"old");
        fs::set_permissions(f.left("run.sh"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(f.right("run.sh"), fs::Permissions::from_mode(0o444)).unwrap();
        let (_, outcome) = f.run_all(LEFT_TO_RIGHT);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(fs::read(f.right("run.sh")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(fs::metadata(f.right("run.sh")).unwrap().permissions().mode() & 0o777, 0o750);
    }

    #[test]
    fn copying_a_folder_takes_its_whole_subtree_and_nothing_else() {
        let f = Fixture::new();
        write(f.left("pkg/a.txt"), b"a");
        write(f.left("pkg/in/deep/b.txt"), b"b");
        write(f.left("pkg/shared.txt"), b"left");
        write(f.right("pkg/shared.txt"), b"right!");
        write(f.right("pkg/only-right.txt"), b"keep me");
        write(f.left("other.txt"), b"not selected");
        fs::create_dir(f.left("pkg/empty")).unwrap();
        let folder_date = {
            let time = SystemTime::now() - Duration::from_secs(7200);
            File::open(f.left("pkg/in")).unwrap().set_modified(time).unwrap();
            fs::metadata(f.left("pkg/in")).unwrap().modified().unwrap()
        };

        let entries = f.scan();
        let plan = f.plan(&entries, &["pkg"], LEFT_TO_RIGHT);
        let outcome = run(&plan);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.completed, plan.items.len());
        assert_eq!(fs::read(f.right("pkg/a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(f.right("pkg/in/deep/b.txt")).unwrap(), b"b");
        assert_eq!(fs::read(f.right("pkg/shared.txt")).unwrap(), b"left");
        assert_eq!(fs::read(f.right("pkg/only-right.txt")).unwrap(), b"keep me");
        assert!(f.right("pkg/empty").is_dir());
        assert!(!f.right("other.txt").exists());
        assert_eq!(fs::metadata(f.right("pkg/in")).unwrap().modified().unwrap(), folder_date);
    }

    #[test]
    fn copying_one_deep_file_creates_its_missing_parent_folders() {
        let f = Fixture::new();
        write(f.left("a/b/c/file.txt"), b"x");
        write(f.left("a/b/sibling.txt"), b"not selected");
        let entries = f.scan();
        let plan = f.plan(&entries, &["a/b/c/file.txt"], LEFT_TO_RIGHT);
        assert_eq!(
            actions(&plan),
            [
                ("a", Action::CreateDir),
                ("a/b", Action::CreateDir),
                ("a/b/c", Action::CreateDir),
                ("a/b/c/file.txt", Action::Create),
            ]
        );
        assert!(run(&plan).errors.is_empty());
        assert_eq!(fs::read(f.right("a/b/c/file.txt")).unwrap(), b"x");
        assert!(!f.right("a/b/sibling.txt").exists());
    }

    #[test]
    fn selecting_a_folder_and_its_child_plans_each_item_once() {
        let f = Fixture::new();
        write(f.left("d/x.txt"), b"x");
        let entries = f.scan();
        let plan = f.plan(&entries, &["d/x.txt", "d", "d/x.txt"], LEFT_TO_RIGHT);
        assert_eq!(actions(&plan), [("d", Action::CreateDir), ("d/x.txt", Action::Create)]);
    }

    #[test]
    fn files_hidden_by_the_name_filter_are_not_copied() {
        let f = Fixture::new();
        write(f.left("d/code.rs"), b"fn main() {}");
        write(f.left("d/Cargo.lock"), b"lock");
        let options = ScanOptions {
            name_filter: NameFilter::parse("-*.lock").unwrap(),
            ..ScanOptions::default()
        };
        let entries = f.scan_with(options);
        let plan = f.plan(&entries, &["d"], LEFT_TO_RIGHT);
        assert!(run(&plan).errors.is_empty());
        assert_eq!(names(&f.right("d")), ["code.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_copied_as_links_and_never_followed() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = f.dir.path().join("outside");
        write(outside.join("secret.txt"), b"outside the tree");
        symlink(&outside, f.left("escape")).unwrap();
        symlink("missing-target", f.left("dangling")).unwrap();
        // On the right, a link where the left has a file: the link is replaced, not written through.
        write(f.left("config"), b"left config");
        symlink(outside.join("secret.txt"), f.right("config")).unwrap();

        let (plan, outcome) = f.run_all(LEFT_TO_RIGHT);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(plan.items.len(), 3);
        assert_eq!(fs::read_link(f.right("escape")).unwrap(), outside);
        assert_eq!(fs::read_link(f.right("dangling")).unwrap(), Path::new("missing-target"));
        assert!(fs::symlink_metadata(f.right("config")).unwrap().is_file());
        assert_eq!(fs::read(f.right("config")).unwrap(), b"left config");
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"outside the tree");
        assert_eq!(names(&outside), ["secret.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_folder_replaced_by_a_link_is_never_written_through() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = f.dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        write(f.left("d/a.txt"), b"a");
        write(f.right("d/b.txt"), b"b");
        let entries = f.scan();

        // Swapped before the plan: blocked there.
        fs::rename(f.right("d"), f.dir.path().join("moved")).unwrap();
        symlink(&outside, f.right("d")).unwrap();
        let plan = f.plan(&entries, &["d/a.txt"], LEFT_TO_RIGHT);
        assert!(plan.is_empty());
        assert!(matches!(plan.blocked[0].reason, FileOpError::UnsafeParent(_)));

        // Swapped between plan and execution: caught there.
        fs::remove_file(f.right("d")).unwrap();
        fs::create_dir(f.right("d")).unwrap();
        let plan = f.plan(&entries, &["d/a.txt"], LEFT_TO_RIGHT);
        assert_eq!(actions(&plan), [("d/a.txt", Action::Create)]);
        fs::remove_dir(f.right("d")).unwrap();
        symlink(&outside, f.right("d")).unwrap();
        let outcome = run(&plan);
        assert!(matches!(outcome.errors[0].error, FileOpError::UnsafeParent(_)));
        assert_eq!(outcome.completed, 0);
        assert!(names(&outside).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_destination_folder_fails_that_file_and_the_rest_goes_on() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        write(f.left("locked/a.txt"), b"new");
        write(f.right("locked/a.txt"), b"old");
        write(f.left("locked/b.txt"), b"new");
        write(f.left("open/c.txt"), b"new");
        write(f.left("z.txt"), b"new");
        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let plan = plan(&f.roots, &entries, &all, LEFT_TO_RIGHT).unwrap();
        fs::set_permissions(f.right("locked"), fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores permissions; the test would prove nothing.
        let enforced = File::create(f.right("locked/probe")).is_err();
        let outcome = run(&plan);
        fs::set_permissions(f.right("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if !enforced {
            return;
        }
        let failed: Vec<_> = outcome.errors.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(failed, ["locked/a.txt", "locked/b.txt"]);
        assert!(matches!(outcome.errors[0].error, FileOpError::Io { .. }));
        assert_eq!(outcome.completed, plan.items.len() - 2);
        assert!(!outcome.cancelled);
        assert_eq!(fs::read(f.right("locked/a.txt")).unwrap(), b"old");
        assert_eq!(names(&f.right("locked")), ["a.txt"]);
        assert_eq!(fs::read(f.right("open/c.txt")).unwrap(), b"new");
        assert_eq!(fs::read(f.right("z.txt")).unwrap(), b"new");
    }

    #[test]
    fn cancelling_stops_before_the_next_file_and_progress_is_reported() {
        let f = Fixture::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            write(f.left(name), b"12345");
        }
        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let plan = plan(&f.roots, &entries, &all, LEFT_TO_RIGHT).unwrap();
        let cancel = AtomicBool::new(false);
        let mut seen = Vec::new();
        let outcome = execute(&plan, &NoTrash, &cancel, &mut |progress| {
            seen.push((progress.items_done, progress.bytes_done, progress.current.to_owned()));
            assert_eq!((progress.items_total, progress.bytes_total), (3, 15));
            cancel.store(progress.items_done == 1, Ordering::Relaxed);
        });
        assert!(outcome.cancelled);
        assert_eq!((outcome.completed, outcome.bytes_copied), (1, 5));
        assert!(outcome.errors.is_empty());
        assert_eq!(names(&f.roots[1]), ["a.txt"]);
        assert_eq!(
            seen,
            [
                (0, 0, PathBuf::from("a.txt")),
                (1, 5, PathBuf::from("b.txt")),
                (1, 5, PathBuf::new()),
            ]
        );
    }

    #[test]
    fn cancelling_inside_a_large_file_leaves_the_destination_as_it_was() {
        let f = Fixture::new();
        write(f.left("big.bin"), &vec![1u8; COPY_BUFFER_BYTES * 3]);
        write(f.right("big.bin"), b"previous content");
        let (entries, cancel) = (f.scan(), AtomicBool::new(false));
        let plan = f.plan(&entries, &["big.bin"], LEFT_TO_RIGHT);
        let outcome = execute(&plan, &NoTrash, &cancel, &mut |progress| {
            cancel.store(progress.bytes_done > 0, Ordering::Relaxed);
        });
        assert!(outcome.cancelled);
        assert_eq!(outcome.completed, 0);
        assert!(outcome.errors.is_empty());
        assert_eq!(fs::read(f.right("big.bin")).unwrap(), b"previous content");
        assert_eq!(names(&f.roots[1]), ["big.bin"]);
    }

    #[test]
    fn files_changed_since_the_plan_are_reported_and_left_alone() {
        let f = Fixture::new();
        write(f.left("dest-changed.txt"), b"left");
        write(f.right("dest-changed.txt"), b"right");
        write(f.left("source-changed.txt"), b"left");
        write(f.right("source-changed.txt"), b"right");
        write(f.left("appeared.txt"), b"left");
        write(f.left("vanished.txt"), b"left");
        write(f.left("fine.txt"), b"left");
        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let plan = plan(&f.roots, &entries, &all, LEFT_TO_RIGHT).unwrap();
        assert_eq!(plan.items.len(), 5);

        write(f.right("dest-changed.txt"), b"edited meanwhile");
        write(f.left("source-changed.txt"), b"edited meanwhile");
        write(f.right("appeared.txt"), b"created meanwhile");
        fs::remove_file(f.left("vanished.txt")).unwrap();
        let outcome = run(&plan);

        let errors: Vec<_> = outcome.errors.iter().map(|e| (e.rel.to_str().unwrap(), &e.error)).collect();
        assert!(matches!(errors[0], ("appeared.txt", FileOpError::ChangedSincePlan(_))), "{errors:?}");
        assert!(matches!(errors[1], ("dest-changed.txt", FileOpError::ChangedSincePlan(_))));
        assert!(matches!(errors[2], ("source-changed.txt", FileOpError::ChangedSincePlan(_))));
        assert!(matches!(errors[3], ("vanished.txt", FileOpError::Vanished(_))));
        assert_eq!((errors.len(), outcome.completed), (4, 1));
        assert_eq!(fs::read(f.right("dest-changed.txt")).unwrap(), b"edited meanwhile");
        assert_eq!(fs::read(f.right("source-changed.txt")).unwrap(), b"right");
        assert_eq!(fs::read(f.right("appeared.txt")).unwrap(), b"created meanwhile");
        assert_eq!(fs::read(f.right("fine.txt")).unwrap(), b"left");
        assert!(errors[0].1.to_string().contains("modifié depuis la confirmation"));
    }

    #[test]
    fn a_file_that_vanished_before_the_plan_is_blocked_not_planned() {
        let f = Fixture::new();
        write(f.left("gone.txt"), b"x");
        let entries = f.scan();
        fs::remove_file(f.left("gone.txt")).unwrap();
        let plan = f.plan(&entries, &["gone.txt"], LEFT_TO_RIGHT);
        assert!(plan.is_empty());
        assert!(matches!(plan.blocked[0].reason, FileOpError::Vanished(_)));
        assert_eq!(plan.totals.blocked, 1);
        assert_eq!(plan.touched(), [(0, PathBuf::from("gone.txt"))]);
    }

    #[test]
    fn copying_a_folder_onto_itself_is_refused() {
        let f = Fixture::new();
        write(f.left("a.txt"), b"x");
        let entries = f.scan();
        let same = [f.roots[0].clone(), f.roots[0].join("../left")];
        let error = plan(&same, &entries, &[0], LEFT_TO_RIGHT).unwrap_err();
        assert!(matches!(error, FileOpError::SameRoots));
        assert_eq!(error.to_string(), "les deux côtés désignent le même dossier");
    }

    #[cfg(unix)]
    #[test]
    fn the_same_folder_reached_through_a_link_is_refused() {
        let f = Fixture::new();
        write(f.left("a.txt"), b"x");
        let alias = f.dir.path().join("alias");
        std::os::unix::fs::symlink(&f.roots[0], &alias).unwrap();
        let entries = f.scan();
        let error = plan(&[f.roots[0].clone(), alias], &entries, &[0], LEFT_TO_RIGHT).unwrap_err();
        assert!(matches!(error, FileOpError::SameRoots));
    }

    #[cfg(unix)]
    #[test]
    fn two_names_of_the_same_file_are_not_copied_onto_each_other() {
        let f = Fixture::new();
        write(f.left("linked.txt"), b"shared inode");
        fs::hard_link(f.left("linked.txt"), f.right("linked.txt")).unwrap();
        let mut entries = f.scan();
        // Even if the comparison had not noticed they are one file.
        entries[0].status = Status::Different;
        let plan = f.plan(&entries, &["linked.txt"], LEFT_TO_RIGHT);
        assert!(plan.is_empty());
        assert!(matches!(plan.blocked[0].reason, FileOpError::SameFile(_)));
        assert_eq!(fs::read(f.right("linked.txt")).unwrap(), b"shared inode");
    }

    #[test]
    fn paths_escaping_the_root_are_refused() {
        let f = Fixture::new();
        write(f.left("a.txt"), b"inside");
        write(f.dir.path().join("victim.txt"), b"outside");
        let absolute = f.dir.path().join("victim.txt");
        for (rel, operation) in [
            (PathBuf::from("../victim.txt"), LEFT_TO_RIGHT),
            (PathBuf::from("../victim.txt"), DELETE_LEFT),
            (PathBuf::from("./../left/a.txt"), DELETE_LEFT),
            (absolute.clone(), DELETE_LEFT),
            (absolute.clone(), LEFT_TO_RIGHT),
            (PathBuf::new(), DELETE_LEFT),
        ] {
            let mut entries = f.scan();
            entries[0].rel = rel.clone();
            let mut plan = plan(&f.roots, &entries, &[0], operation).unwrap();
            assert!(plan.is_empty(), "{rel:?}");
            assert!(matches!(plan.blocked[0].reason, FileOpError::EscapesRoot(_)), "{rel:?}");

            // A hand-made plan does not get further.
            let stamp = lstat(&absolute).unwrap().unwrap();
            let action = if operation == LEFT_TO_RIGHT { Action::Create } else { Action::Delete };
            plan.items.push(PlanItem {
                index: 0,
                rel: rel.clone(),
                action,
                from: Side::Left,
                source: stamp,
                dest: None,
                newer: Newer::Unknown,
                unlisted: 0,
            });
            let outcome = run(&plan);
            assert!(matches!(outcome.errors[0].error, FileOpError::EscapesRoot(_)), "{rel:?}");
            assert_eq!(outcome.completed, 0);
        }
        assert_eq!(fs::read(&absolute).unwrap(), b"outside");
        assert_eq!(fs::read(f.left("a.txt")).unwrap(), b"inside");
        assert_eq!(names(f.dir.path()), ["left", "right", "victim.txt"]);
    }

    #[test]
    fn a_file_is_never_copied_over_a_folder_nor_a_folder_over_a_file() {
        let f = Fixture::new();
        write(f.left("clash"), b"file on the left");
        write(f.right("clash/inner.txt"), b"folder on the right");
        write(f.left("other/inner.txt"), b"folder on the left");
        write(f.right("other"), b"file on the right");
        let (plan, outcome) = f.run_all(LEFT_TO_RIGHT);
        assert!(plan.is_empty());
        assert_eq!(plan.blocked.len(), 3);
        assert!(plan.blocked.iter().all(|b| matches!(b.reason, FileOpError::TypeConflict(_))));
        assert_eq!(outcome.completed, 0);
        assert_eq!(fs::read(f.right("clash/inner.txt")).unwrap(), b"folder on the right");
        assert_eq!(fs::read(f.right("other")).unwrap(), b"file on the right");
    }

    #[test]
    fn permanent_deletion_removes_a_subtree_on_one_side_only() {
        let f = Fixture::new();
        write(f.left("d/a.txt"), b"12345");
        write(f.left("d/in/b.txt"), b"123");
        write(f.right("d/a.txt"), b"right copy");
        write(f.left("keep.txt"), b"not selected");
        let entries = f.scan();
        let plan = f.plan(&entries, &["d"], DELETE_LEFT);
        assert_eq!(
            actions(&plan),
            [
                ("d", Action::DeleteDir),
                ("d/in", Action::DeleteDir),
                ("d/in/b.txt", Action::Delete),
                ("d/a.txt", Action::Delete),
            ]
        );
        assert_eq!(
            plan.totals,
            Totals {
                deletes: 2,
                delete_dirs: 2,
                bytes_to_delete: 8,
                ..Totals::default()
            }
        );
        assert!(f.left("d/a.txt").exists());

        let outcome = run(&plan);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.completed, 4);
        assert!(outcome.kept_dirs.is_empty());
        assert_eq!(names(&f.roots[0]), ["keep.txt"]);
        assert_eq!(fs::read(f.right("d/a.txt")).unwrap(), b"right copy");
    }

    #[test]
    fn deleting_a_folder_keeps_what_the_comparison_does_not_show() {
        let f = Fixture::new();
        write(f.left("d/code.rs"), b"shown");
        write(f.left("d/in/notes.rs"), b"shown");
        write(f.left("d/in/Cargo.lock"), b"hidden by the filter");
        write(f.left("gone/x.rs"), b"shown");
        let options = ScanOptions {
            name_filter: NameFilter::parse("-*.lock").unwrap(),
            ..ScanOptions::default()
        };
        let entries = f.scan_with(options);
        // Created after the comparison: not shown either.
        write(f.left("d/late.txt"), b"new");

        for (mode, trash) in [(DeleteMode::Trash, FakeTrash::default()), (DeleteMode::Permanent, FakeTrash::default())] {
            let operation = Operation::Delete { side: Side::Left, mode };
            let plan = f.plan(&entries, &["d"], operation);
            let unlisted: Vec<_> = plan.items.iter().map(|item| (item.rel.to_str().unwrap(), item.unlisted)).collect();
            assert_eq!(unlisted, [("d", 1), ("d/in", 1), ("d/in/notes.rs", 0), ("d/code.rs", 0)]);
            assert_eq!((plan.totals.deletes, plan.totals.delete_dirs, plan.totals.dirs_kept), (2, 0, 2));

            let outcome = execute(&plan, &trash, &AtomicBool::new(false), &mut |_| {});
            assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
            assert_eq!(outcome.completed, 2);
            assert_eq!(outcome.kept_dirs, [PathBuf::from("d"), PathBuf::from("d/in")]);
            assert_eq!(names(&f.left("d")), ["in", "late.txt"]);
            assert_eq!(names(&f.left("d/in")), ["Cargo.lock"]);
            let trashed = trash.calls.borrow().len();
            assert_eq!(trashed, if mode == DeleteMode::Trash { 2 } else { 0 });
            write(f.left("d/code.rs"), b"shown");
            write(f.left("d/in/notes.rs"), b"shown");
        }
        assert!(f.left("gone/x.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_link_never_touches_what_it_points_to() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = f.dir.path().join("outside");
        write(outside.join("precious.txt"), b"keep");
        symlink(&outside, f.left("link-to-dir")).unwrap();
        symlink(outside.join("precious.txt"), f.left("link-to-file")).unwrap();
        write(f.left("d/inner.txt"), b"x");
        symlink(&outside, f.left("d/nested-link")).unwrap();
        let (plan, outcome) = f.run_all(DELETE_LEFT);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.completed, plan.items.len());
        assert!(names(&f.roots[0]).is_empty());
        assert_eq!(fs::read(outside.join("precious.txt")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn deletion_refuses_to_go_through_a_folder_replaced_by_a_link() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let outside = f.dir.path().join("outside");
        write(outside.join("a.txt"), b"same name outside");
        write(f.left("d/a.txt"), b"same name outside");
        let entries = f.scan();
        let plan_before = f.plan(&entries, &["d/a.txt"], DELETE_LEFT);
        fs::remove_dir_all(f.left("d")).unwrap();
        symlink(&outside, f.left("d")).unwrap();

        let outcome = run(&plan_before);
        assert!(matches!(outcome.errors[0].error, FileOpError::UnsafeParent(_)));
        let plan_after = f.plan(&entries, &["d/a.txt"], DELETE_LEFT);
        assert!(plan_after.is_empty());
        assert!(matches!(plan_after.blocked[0].reason, FileOpError::UnsafeParent(_)));
        assert!(outside.join("a.txt").exists());
    }

    #[test]
    fn deletion_leaves_a_file_changed_since_the_plan() {
        let f = Fixture::new();
        write(f.left("d/a.txt"), b"old");
        write(f.left("d/b.txt"), b"old");
        let entries = f.scan();
        let plan = f.plan(&entries, &["d"], DELETE_LEFT);
        write(f.left("d/a.txt"), b"edited after the dialog");
        let outcome = run(&plan);
        assert_eq!(outcome.errors.len(), 1);
        assert!(matches!(outcome.errors[0].error, FileOpError::ChangedSincePlan(_)));
        assert_eq!(outcome.kept_dirs, [PathBuf::from("d")]);
        assert_eq!(names(&f.left("d")), ["a.txt"]);
    }

    #[test]
    fn trash_mode_goes_through_the_trasher_and_moves_whole_folders_at_once() {
        let f = Fixture::new();
        write(f.left("d/a.txt"), b"a");
        write(f.left("d/in/b.txt"), b"b");
        write(f.left("single.txt"), b"s");
        write(f.left("keep.txt"), b"k");
        let entries = f.scan();
        let plan = f.plan(&entries, &["d", "single.txt"], TRASH_LEFT);
        let trash = FakeTrash::default();
        let mut seen = Vec::new();
        let outcome = execute(&plan, &trash, &AtomicBool::new(false), &mut |progress| {
            seen.push(progress.current.to_owned());
        });
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.completed, plan.items.len());
        assert_eq!(*trash.calls.borrow(), [f.left("d"), f.left("single.txt")]);
        assert_eq!(names(&f.roots[0]), ["keep.txt"]);
        assert_eq!(seen, [PathBuf::from("d"), PathBuf::from("single.txt"), PathBuf::new()]);
    }

    #[test]
    fn a_failing_trash_deletes_nothing_and_says_so() {
        let f = Fixture::new();
        write(f.left("d/a.txt"), b"a");
        write(f.left("single.txt"), b"s");
        let entries = f.scan();
        let plan = f.plan(&entries, &["d", "single.txt"], TRASH_LEFT);
        let trash = FakeTrash {
            fail: true,
            ..FakeTrash::default()
        };
        let outcome = execute(&plan, &trash, &AtomicBool::new(false), &mut |_| {});
        assert_eq!(outcome.completed, 0);
        let failed: Vec<_> = outcome.errors.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(failed, ["d", "single.txt"]);
        assert!(matches!(outcome.errors[0].error, FileOpError::TrashFailed { .. }));
        assert!(outcome.errors[0].error.to_string().contains("mise à la corbeille impossible"));
        assert!(f.left("d/a.txt").exists() && f.left("single.txt").exists());
    }

    #[test]
    fn cancelling_a_deletion_keeps_the_remaining_files_and_their_folder() {
        let f = Fixture::new();
        write(f.left("d/a.txt"), b"a");
        write(f.left("d/b.txt"), b"b");
        let entries = f.scan();
        let plan = f.plan(&entries, &["d"], DELETE_LEFT);
        let cancel = AtomicBool::new(false);
        let outcome = execute(&plan, &NoTrash, &cancel, &mut |progress| {
            cancel.store(progress.current == Path::new("d/a.txt"), Ordering::Relaxed);
        });
        assert!(outcome.cancelled);
        assert_eq!(outcome.completed, 1);
        assert_eq!(outcome.kept_dirs, [PathBuf::from("d")]);
        assert_eq!(names(&f.left("d")), ["b.txt"]);
    }

    #[test]
    fn an_invalid_selection_or_root_is_an_error() {
        let f = Fixture::new();
        write(f.left("a.txt"), b"x");
        let entries = f.scan();
        assert!(matches!(
            plan(&f.roots, &entries, &[7], LEFT_TO_RIGHT),
            Err(FileOpError::InvalidSelection)
        ));
        let missing = [f.roots[0].clone(), f.dir.path().join("nope")];
        let error = plan(&missing, &entries, &[0], LEFT_TO_RIGHT).unwrap_err();
        assert!(error.to_string().ends_with("n'est pas un dossier"));
    }

    #[test]
    fn the_tree_is_patched_after_a_copy_and_a_deletion_without_rescanning() {
        let f = Fixture::new();
        write(f.left("d/a.txt"), b"left");
        write(f.right("d/a.txt"), b"right!");
        write(f.left("d/new.txt"), b"new");
        write(f.left("only/x.txt"), b"x");
        write(f.left("same.txt"), b"s");
        write(f.right("same.txt"), b"s");
        let mut entries = f.scan();
        let cancel = AtomicBool::new(false);
        let status = |entries: &[Entry], rel: &str| entries[index_of(entries, rel) as usize].status;
        assert_eq!(status(&entries, "d"), Status::Different);

        let plan = f.plan(&entries, &["d"], LEFT_TO_RIGHT);
        assert!(run(&plan).errors.is_empty());
        let updates = dirscan::recheck(&f.roots, &plan.touched(), &cancel);
        assert!(dirscan::apply_updates(&mut entries, &updates).is_empty());
        assert_eq!(status(&entries, "d/a.txt"), Status::Same);
        assert_eq!(status(&entries, "d/new.txt"), Status::Same);
        assert_eq!(status(&entries, "d"), Status::Same);
        assert_eq!(status(&entries, "only"), Status::LeftOnly);

        let plan = f.plan(&entries, &["only"], DELETE_LEFT);
        assert!(run(&plan).errors.is_empty());
        let removed_at = index_of(&entries, "only");
        let updates = dirscan::recheck(&f.roots, &plan.touched(), &cancel);
        let removed = dirscan::apply_updates(&mut entries, &updates);
        assert_eq!(removed, [removed_at, removed_at + 1]);
        let rels: Vec<_> = entries.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(rels, ["d", "d/a.txt", "d/new.txt", "same.txt"]);
        assert_eq!(entries[1].parent, Some(0));
        assert_eq!(entries[0].subtree_end, 3);
        assert_eq!(entries[3].parent, None);

        // Deleting one side of a pair leaves the entry, now present on the other side only.
        let plan = f.plan(&entries, &["same.txt"], DELETE_LEFT);
        assert!(run(&plan).errors.is_empty());
        let updates = dirscan::recheck(&f.roots, &plan.touched(), &cancel);
        assert!(dirscan::apply_updates(&mut entries, &updates).is_empty());
        assert_eq!(status(&entries, "same.txt"), Status::RightOnly);
        assert!(entries[3].left.is_none());
    }

    // --- Synchronisation --------------------------------------------------------------------

    const MIRROR_RIGHT: Operation = Operation::Mirror {
        to: Side::Right,
        mode: DeleteMode::Permanent,
    };

    #[test]
    fn a_mirror_copies_what_differs_and_deletes_what_only_the_destination_holds() {
        let f = Fixture::new();
        write(f.left("same.txt"), b"same");
        write(f.right("same.txt"), b"same");
        write(f.left("changed.txt"), b"left version");
        write(f.right("changed.txt"), b"right version, newer");
        set_modified(&f.left("changed.txt"), 3600);
        write(f.left("new/deep.txt"), b"new");
        write(f.right("extra.txt"), b"only right");
        write(f.right("old/a.txt"), b"a");
        write(f.right("old/sub/b.txt"), b"b");
        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let plan = plan(&f.roots, &entries, &all, MIRROR_RIGHT).unwrap();
        assert_eq!(plan.totals.creates, 1);
        assert_eq!(plan.totals.create_dirs, 1);
        assert_eq!(plan.totals.overwrites, 1);
        assert_eq!(plan.totals.overwrites_of_newer, 1, "the right one is newer: the dialog must say so");
        assert_eq!(plan.totals.deletes, 3);
        assert_eq!(plan.totals.delete_dirs, 2);
        assert_eq!(plan.totals.identical, 1);
        let directions: Vec<(&str, Side)> = plan.items.iter().map(|item| (item.rel.to_str().unwrap(), item.from)).collect();
        assert!(directions.contains(&("extra.txt", Side::Right)) && directions.contains(&("changed.txt", Side::Left)));
        let outcome = run(&plan);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(names(&f.roots[1]), ["changed.txt", "new", "same.txt"]);
        assert_eq!(fs::read(f.right("changed.txt")).unwrap(), b"left version");
        assert_eq!(fs::read(f.right("new/deep.txt")).unwrap(), b"new");
        assert_eq!(names(&f.roots[0]), ["changed.txt", "new", "same.txt"], "the source is untouched");
    }

    #[test]
    fn a_mirror_in_trash_mode_sends_destination_only_folders_to_the_trash_whole() {
        let f = Fixture::new();
        write(f.left("kept.txt"), b"k");
        write(f.right("old/a.txt"), b"a");
        write(f.right("stale.txt"), b"s");
        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let operation = Operation::Mirror { to: Side::Right, mode: DeleteMode::Trash };
        let plan = plan(&f.roots, &entries, &all, operation).unwrap();
        let trash = FakeTrash::default();
        let outcome = execute(&plan, &trash, &AtomicBool::new(false), &mut |_| {});
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(*trash.calls.borrow(), [f.right("old"), f.right("stale.txt")]);
        assert_eq!(names(&f.roots[1]), ["kept.txt"]);

        // Without a trash, nothing is deleted for good instead.
        write(f.right("again.txt"), b"x");
        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let again = super::plan(&f.roots, &entries, &all, operation).unwrap();
        let failing = FakeTrash { fail: true, ..FakeTrash::default() };
        let outcome = execute(&again, &failing, &AtomicBool::new(false), &mut |_| {});
        assert!(matches!(outcome.errors[0].error, FileOpError::TrashFailed { .. }));
        assert!(f.right("again.txt").exists());
    }

    #[test]
    fn a_mirror_of_a_selection_leaves_the_rest_alone() {
        let f = Fixture::new();
        write(f.left("a/x.txt"), b"x");
        write(f.right("a/stale.txt"), b"s");
        write(f.right("b/other.txt"), b"o");
        let entries = f.scan();
        let plan = f.plan(&entries, &["a"], MIRROR_RIGHT);
        assert!(run(&plan).errors.is_empty());
        assert_eq!(names(&f.right("a")), ["x.txt"]);
        assert_eq!(names(&f.right("b")), ["other.txt"], "outside the selection");
    }

    #[test]
    fn files_hidden_by_the_name_filter_are_neither_mirrored_nor_deleted() {
        let f = Fixture::new();
        write(f.left("d/code.rs"), b"fn main() {}");
        write(f.left("d/Cargo.lock"), b"left lock");
        write(f.right("d/Cargo.lock"), b"right lock");
        write(f.right("d/build.lock"), b"only right, hidden");
        let options = ScanOptions {
            name_filter: NameFilter::parse("-*.lock").unwrap(),
            ..ScanOptions::default()
        };
        let entries = f.scan_with(options);
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let plan = plan(&f.roots, &entries, &all, MIRROR_RIGHT).unwrap();
        assert!(run(&plan).errors.is_empty());
        assert_eq!(names(&f.right("d")), ["Cargo.lock", "build.lock", "code.rs"]);
        assert_eq!(fs::read(f.right("d/Cargo.lock")).unwrap(), b"right lock");
    }

    #[test]
    fn an_update_copies_each_pair_from_its_newer_side_and_deletes_nothing() {
        let f = Fixture::new();
        write(f.left("newer-left.txt"), b"left wins");
        write(f.right("newer-left.txt"), b"right loses");
        set_modified(&f.right("newer-left.txt"), 3600);
        write(f.left("newer-right.txt"), b"left loses");
        write(f.right("newer-right.txt"), b"right wins");
        set_modified(&f.left("newer-right.txt"), 3600);
        write(f.left("only-left/a.txt"), b"a");
        write(f.right("only-right.txt"), b"r");
        write(f.left("same.txt"), b"same");
        write(f.right("same.txt"), b"same");
        let (plan, outcome) = f.run_all(Operation::Update);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
        assert_eq!(plan.totals.overwrites_of_newer, 0, "an update never replaces a newer file");
        assert_eq!(plan.totals.deletes, 0);
        assert_eq!(fs::read(f.right("newer-left.txt")).unwrap(), b"left wins");
        assert_eq!(fs::read(f.left("newer-right.txt")).unwrap(), b"right wins");
        assert_eq!(fs::read(f.right("only-left/a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(f.left("only-right.txt")).unwrap(), b"r");
        assert_eq!(names(&f.roots[0]), names(&f.roots[1]));
    }

    #[test]
    fn an_update_blocks_a_pair_whose_newer_side_cannot_be_told() {
        let f = Fixture::new();
        write(f.left("tie.txt"), b"left");
        write(f.right("tie.txt"), b"right");
        let time = set_modified(&f.left("tie.txt"), 60);
        File::options().write(true).open(f.right("tie.txt")).unwrap().set_modified(time).unwrap();
        write(f.left("clash"), b"a file");
        write(f.right("clash/inside.txt"), b"a folder");
        let (plan, outcome) = f.run_all(Operation::Update);
        let reasons: Vec<(&str, &FileOpError)> = plan.blocked.iter().map(|b| (b.rel.to_str().unwrap(), &b.reason)).collect();
        assert!(matches!(reasons.iter().find(|(rel, _)| *rel == "tie.txt"), Some((_, FileOpError::SameDateConflict(_)))), "{reasons:?}");
        assert!(matches!(reasons.iter().find(|(rel, _)| *rel == "clash"), Some((_, FileOpError::TypeConflict(_)))), "{reasons:?}");
        let tie = plan.blocked.iter().find(|b| b.rel == Path::new("tie.txt")).unwrap();
        assert!(tie.reason.to_string().contains("rien n'est copié"), "{}", tie.reason);
        assert!(outcome.errors.is_empty());
        assert_eq!(fs::read(f.left("tie.txt")).unwrap(), b"left");
        assert_eq!(fs::read(f.right("tie.txt")).unwrap(), b"right");
        assert!(f.left("clash").is_file() && f.right("clash").is_dir());
    }

    #[test]
    fn cancelling_a_mirror_stops_between_files_and_deletes_nothing_yet() {
        let f = Fixture::new();
        for name in ["a.txt", "b.txt", "c.txt"] {
            write(f.left(name), b"12345");
        }
        write(f.right("stale.txt"), b"s");
        let entries = f.scan();
        let all: Vec<u32> = (0..entries.len() as u32).collect();
        let plan = plan(&f.roots, &entries, &all, MIRROR_RIGHT).unwrap();
        let cancel = AtomicBool::new(false);
        let outcome = execute(&plan, &NoTrash, &cancel, &mut |progress| {
            cancel.store(progress.items_done == 1, Ordering::Relaxed);
        });
        assert!(outcome.cancelled);
        assert_eq!(outcome.completed, 1);
        assert_eq!(names(&f.roots[1]), ["a.txt", "stale.txt"], "copies come first, deletions never started");
    }

    #[test]
    fn a_synchronisation_onto_the_same_folder_is_refused() {
        let f = Fixture::new();
        let roots = [f.roots[0].clone(), f.roots[0].clone()];
        for operation in [MIRROR_RIGHT, Operation::Update] {
            assert!(matches!(plan(&roots, &[], &[], operation), Err(FileOpError::SameRoots)));
        }
    }
}
