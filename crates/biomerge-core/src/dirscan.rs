//! Parallel folder comparison.
//!
//! Both trees are walked at once with the `ignore` crate's parallel walker (the one behind
//! ripgrep). The merged tree is published immediately, then same-sized files are compared byte
//! by byte on a thread pool, with an early exit on the first difference. Hashing would have to
//! read both files entirely, which is never faster when the goal is only "equal or not".

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};
use rayon::prelude::*;

const COMPARE_BUFFER_BYTES: usize = 256 * 1024;
const PROGRESS_BATCH: usize = 64;
const MAX_COMPARE_THREADS: usize = 32;
const ROTATIONAL_COMPARE_THREADS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    file_id: (u64, u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Same,
    Different,
    LeftOnly,
    RightOnly,
    TypeMismatch,
    Error,
}

impl Status {
    pub fn is_difference(self) -> bool {
        !matches!(self, Status::Same | Status::Pending)
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub rel: PathBuf,
    pub depth: u32,
    pub parent: Option<u32>,
    /// One past the index of this entry's last descendant.
    pub subtree_end: u32,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub status: Status,
}

impl Entry {
    pub fn name(&self) -> &OsStr {
        self.rel.file_name().unwrap_or(self.rel.as_os_str())
    }

    pub fn is_dir(&self) -> bool {
        [self.left, self.right]
            .iter()
            .flatten()
            .any(|m| m.kind == EntryKind::Dir)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameFilterError {
    #[error("motif « {pattern} » invalide : {reason}")]
    InvalidPattern { pattern: String, reason: String },
    #[error("motif vide après « {0} »")]
    EmptyPattern(String),
    #[error("guillemet non refermé")]
    UnclosedQuote,
}

/// Globs of one polarity, split by what they are matched against so that a name-only pattern
/// never has to look at the whole path.
#[derive(Clone, Debug)]
struct PatternSet {
    names: GlobSet,
    dir_names: GlobSet,
    paths: GlobSet,
    dir_paths: GlobSet,
    len: usize,
}

impl Default for PatternSet {
    fn default() -> Self {
        Self {
            names: GlobSet::empty(),
            dir_names: GlobSet::empty(),
            paths: GlobSet::empty(),
            dir_paths: GlobSet::empty(),
            len: 0,
        }
    }
}

impl PatternSet {
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn matches(&self, rel: &Path, is_dir: bool) -> bool {
        let name = rel.file_name().unwrap_or(rel.as_os_str());
        self.names.is_match(name)
            || self.paths.is_match(rel)
            || (is_dir && (self.dir_names.is_match(name) || self.dir_paths.is_match(rel)))
    }
}

/// Include and exclude patterns typed by the user in a single field.
///
/// Syntax, one pattern per word (words are separated by spaces or `;`, and a word may be put in
/// double quotes to contain spaces):
/// - `*.rs` includes; `-*.lock` or `!*.lock` excludes;
/// - a trailing `/` restricts the pattern to folders: `-target/`;
/// - a pattern without any other `/` is matched against the name, at any depth; a pattern with
///   a `/` is matched against the path relative to the compared roots: `docs/*.md`, `/build/`
///   (top level only), `**/gen/*.rs`;
/// - globs are `*`, `?`, `[a-z]`, `{a,b}` and `**`; matching ignores case.
///
/// Semantics: an excluded item is dropped and an excluded folder is not walked at all. Without
/// include patterns everything else is kept. With include patterns, a file is kept when it, or
/// one of the folders above it, matches one of them; other folders are still walked but only
/// shown when something below them is kept. Exclusion always wins over inclusion.
#[derive(Clone, Default)]
pub struct NameFilter {
    source: String,
    include: PatternSet,
    exclude: PatternSet,
}

/// The compiled globs print as pages of automaton states; the text says it all.
impl std::fmt::Debug for NameFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NameFilter").field(&self.source).finish()
    }
}

impl NameFilter {
    pub fn parse(text: &str) -> Result<Self, NameFilterError> {
        let mut include = PatternSetBuilder::default();
        let mut exclude = PatternSetBuilder::default();
        for word in split_words(text)? {
            let (builder, pattern) = match word.strip_prefix(['-', '!']) {
                Some(rest) => (&mut exclude, rest),
                None => (&mut include, word.strip_prefix('+').unwrap_or(&word)),
            };
            builder.add(&word, pattern)?;
        }
        Ok(Self {
            source: text.trim().to_owned(),
            include: include.build()?,
            exclude: exclude.build()?,
        })
    }

    /// True when nothing is filtered, as with `NameFilter::default()`.
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// The text this filter was parsed from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the item at `rel` (relative to a compared root) belongs to the comparison.
    /// Folders that are only walked to reach included files answer `false`.
    pub fn accepts(&self, rel: &Path, is_dir: bool) -> bool {
        let above = || rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty());
        if self.exclude.matches(rel, is_dir) || above().any(|a| self.exclude.matches(a, true)) {
            return false;
        }
        self.include.is_empty()
            || self.include.matches(rel, is_dir)
            || above().any(|a| self.include.matches(a, true))
    }
}

#[derive(Default)]
struct PatternSetBuilder {
    /// Indexed like the fields of `PatternSet`: names, folder names, paths, folder paths.
    sets: [Option<GlobSetBuilder>; 4],
    len: usize,
}

impl PatternSetBuilder {
    fn add(&mut self, word: &str, pattern: &str) -> Result<(), NameFilterError> {
        let (pattern, dir_only) = match pattern.strip_suffix('/') {
            Some(rest) => (rest, true),
            None => (pattern, false),
        };
        // The roots are the anchor of every path pattern, so a leading `/` only says "path".
        let anchored = pattern.contains('/');
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        if pattern.is_empty() {
            return Err(NameFilterError::EmptyPattern(word.to_owned()));
        }
        let glob = GlobBuilder::new(pattern)
            .case_insensitive(true)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| NameFilterError::InvalidPattern {
                pattern: word.to_owned(),
                reason: glob_error_reason(error.kind()),
            })?;
        let slot = usize::from(dir_only) + 2 * usize::from(anchored);
        self.sets[slot].get_or_insert_with(GlobSetBuilder::new).add(glob);
        self.len += 1;
        Ok(())
    }

    fn build(self) -> Result<PatternSet, NameFilterError> {
        let len = self.len;
        let mut sets = self.sets.into_iter().map(|set| match set {
            Some(builder) => builder.build().map_err(|error| NameFilterError::InvalidPattern {
                pattern: error.glob().unwrap_or_default().to_owned(),
                reason: glob_error_reason(error.kind()),
            }),
            None => Ok(GlobSet::empty()),
        });
        let mut next = || sets.next().unwrap();
        Ok(PatternSet {
            names: next()?,
            dir_names: next()?,
            paths: next()?,
            dir_paths: next()?,
            len,
        })
    }
}

fn glob_error_reason(kind: &globset::ErrorKind) -> String {
    use globset::ErrorKind as Kind;
    match kind {
        Kind::InvalidRecursive => "« ** » doit occuper un niveau de chemin entier".into(),
        Kind::UnclosedClass => "crochet « [ » non refermé".into(),
        Kind::InvalidRange(from, to) => format!("intervalle « {from}-{to} » inversé"),
        Kind::UnopenedAlternates => "accolade « } » sans « { »".into(),
        Kind::UnclosedAlternates => "accolade « { » non refermée".into(),
        Kind::NestedAlternates => "accolades imbriquées".into(),
        Kind::DanglingEscape => "« \\ » en fin de motif".into(),
        other => format!("motif trop complexe ({other})"),
    }
}

/// Splits on whitespace and `;`, keeping quoted spans together and dropping the quotes.
fn split_words(text: &str) -> Result<Vec<String>, NameFilterError> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if !quoted && (c.is_whitespace() || c == ';') => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if quoted {
        return Err(NameFilterError::UnclosedQuote);
    }
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub respect_gitignore: bool,
    pub skip_vcs_dirs: bool,
    /// Overrides the automatic number of comparison threads.
    pub compare_threads: Option<usize>,
    /// Names to keep or leave out; empty by default.
    pub name_filter: NameFilter,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: false,
            skip_vcs_dirs: true,
            compare_threads: None,
            name_filter: NameFilter::default(),
        }
    }
}

#[derive(Debug)]
pub enum ScanEvent {
    /// The merged tree; statuses of files that need reading are still `Pending`.
    Tree {
        entries: Vec<Entry>,
        walk_errors: Vec<String>,
        walk_time: Duration,
    },
    Progress(Vec<(u32, Status)>),
    Done {
        total_time: Duration,
        bytes_compared: u64,
        threads: usize,
    },
    Failed(String),
}

#[derive(Debug)]
struct Walked {
    rel: PathBuf,
    meta: Meta,
    /// A folder the name filter emptied, or only walked to reach included files: it is shown
    /// only if something below it is.
    hollow: bool,
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

fn to_meta(meta: &fs::Metadata) -> Meta {
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
    Meta {
        kind,
        size: meta.len(),
        modified: meta.modified().ok(),
        file_id: file_id(meta),
    }
}

/// Flushes a walker thread's private buffer into the shared result when the thread ends.
struct ThreadSink<'a> {
    local: Vec<Walked>,
    errors: Vec<String>,
    /// Folders in which the name filter excluded something.
    filtered_in: Vec<PathBuf>,
    shared: &'a Mutex<(Vec<Walked>, Vec<String>, HashSet<PathBuf>)>,
}

impl Drop for ThreadSink<'_> {
    fn drop(&mut self) {
        let mut shared = self.shared.lock().unwrap();
        shared.0.append(&mut self.local);
        shared.1.append(&mut self.errors);
        shared.2.extend(self.filtered_in.drain(..));
    }
}

fn walk(root: &Path, options: &ScanOptions, cancel: &AtomicBool) -> (Vec<Walked>, Vec<String>) {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .hidden(false)
        .parents(false)
        .follow_links(false)
        .threads(std::thread::available_parallelism().map_or(4, usize::from));
    if options.respect_gitignore {
        builder
            .git_ignore(true)
            .git_exclude(true)
            .ignore(true)
            .require_git(false);
    }

    let shared = Mutex::new((Vec::new(), Vec::new(), HashSet::new()));
    builder.build_parallel().run(|| {
        let mut sink = ThreadSink {
            local: Vec::with_capacity(1024),
            errors: Vec::new(),
            filtered_in: Vec::new(),
            shared: &shared,
        };
        let skip_vcs = options.skip_vcs_dirs;
        let filter = &options.name_filter;
        Box::new(move |result| {
            if cancel.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    sink.errors.push(error.to_string());
                    return WalkState::Continue;
                }
            };
            if entry.depth() == 0 {
                return WalkState::Continue;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            if skip_vcs && is_dir && matches!(entry.file_name().to_str(), Some(".git" | ".hg" | ".svn")) {
                return WalkState::Skip;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                return WalkState::Continue;
            };
            let mut hollow = false;
            if filter.exclude.matches(rel, is_dir) {
                // Skipping here is what prunes an excluded folder: it is never read.
                if let Some(parent) = rel.parent() {
                    if sink.filtered_in.last().is_none_or(|last| last != parent) {
                        sink.filtered_in.push(parent.to_owned());
                    }
                }
                return WalkState::Skip;
            }
            if !filter.include.is_empty() {
                let included = filter.include.matches(rel, is_dir)
                    || rel
                        .ancestors()
                        .skip(1)
                        .any(|above| !above.as_os_str().is_empty() && filter.include.matches(above, true));
                if !included && !is_dir {
                    return WalkState::Continue;
                }
                hollow = !included;
            }
            match entry.metadata() {
                Ok(meta) => sink.local.push(Walked {
                    rel: rel.to_owned(),
                    meta: to_meta(&meta),
                    hollow,
                }),
                Err(error) => sink.errors.push(error.to_string()),
            }
            WalkState::Continue
        })
    });
    let (mut walked, errors, filtered_in) = shared.into_inner().unwrap();
    if !filtered_in.is_empty() {
        for item in &mut walked {
            item.hollow |= item.meta.kind == EntryKind::Dir && filtered_in.contains(&item.rel);
        }
    }
    (walked, errors)
}

/// Directory-first, case-insensitive ordering that still keeps each subtree contiguous.
fn tree_order(a: (&Path, bool), b: (&Path, bool)) -> CmpOrdering {
    let mut left = a.0.components();
    let mut right = b.0.components();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return CmpOrdering::Equal,
            (None, Some(_)) => return CmpOrdering::Less,
            (Some(_), None) => return CmpOrdering::Greater,
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => {
                let left_dir = a.1 || left.clone().next().is_some();
                let right_dir = b.1 || right.clone().next().is_some();
                return right_dir
                    .cmp(&left_dir)
                    .then_with(|| case_folded(x).cmp(&case_folded(y)))
                    .then_with(|| x.cmp(&y));
            }
        }
    }
}

fn case_folded(component: Component<'_>) -> String {
    component.as_os_str().to_string_lossy().to_lowercase()
}

fn merge_trees(mut left: Vec<Walked>, mut right: Vec<Walked>) -> Vec<Entry> {
    left.par_sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    right.par_sort_unstable_by(|a, b| a.rel.cmp(&b.rel));

    let mut merged: Vec<(PathBuf, Option<Meta>, Option<Meta>, bool)> =
        Vec::with_capacity(left.len().max(right.len()));
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    loop {
        let order = match (left.peek(), right.peek()) {
            (Some(a), Some(b)) => a.rel.cmp(&b.rel),
            (Some(_), None) => CmpOrdering::Less,
            (None, Some(_)) => CmpOrdering::Greater,
            (None, None) => break,
        };
        match order {
            CmpOrdering::Less => {
                let a = left.next().unwrap();
                merged.push((a.rel, Some(a.meta), None, a.hollow));
            }
            CmpOrdering::Greater => {
                let b = right.next().unwrap();
                merged.push((b.rel, None, Some(b.meta), b.hollow));
            }
            CmpOrdering::Equal => {
                let a = left.next().unwrap();
                let b = right.next().unwrap();
                merged.push((a.rel, Some(a.meta), Some(b.meta), a.hollow || b.hollow));
            }
        }
    }

    let is_dir = |m: &Option<Meta>| m.is_some_and(|m| m.kind == EntryKind::Dir);
    merged.par_sort_unstable_by(|a, b| {
        tree_order(
            (&a.0, is_dir(&a.1) || is_dir(&a.2)),
            (&b.0, is_dir(&b.1) || is_dir(&b.2)),
        )
    });

    // Folders the name filter left without anything to show are dropped, deepest first, and
    // so are the folders that only held such folders. A folder that is hollow on one side only
    // is still dropped when the merged tree has nothing under it; a genuinely empty folder is
    // never hollow and stays.
    if merged.iter().any(|item| item.3) {
        // Per depth: whether the folder being closed kept a child, and whether it lost one.
        let mut below: Vec<(bool, bool)> = Vec::new();
        let mut keep = vec![true; merged.len()];
        for (index, (rel, left, right, hollow)) in merged.iter().enumerate().rev() {
            let depth = rel.components().count();
            if below.len() < depth + 2 {
                below.resize(depth + 2, (false, false));
            }
            let (has_content, lost_content) = std::mem::take(&mut below[depth + 1]);
            let only_dirs = [left, right].into_iter().flatten().all(|m| m.kind == EntryKind::Dir);
            keep[index] = has_content || !(*hollow || lost_content) || !only_dirs;
            below[depth].0 |= keep[index];
            below[depth].1 |= !keep[index];
        }
        let mut keep = keep.into_iter();
        merged.retain(|_| keep.next().unwrap());
    }

    let mut entries: Vec<Entry> = merged
        .into_iter()
        .map(|(rel, left, right, _)| Entry {
            depth: rel.components().count() as u32,
            parent: None,
            subtree_end: 0,
            status: initial_status(&left, &right),
            rel,
            left,
            right,
        })
        .collect();
    link_tree(&mut entries);
    aggregate(&mut entries);
    entries
}

/// Rebuilds `parent` and `subtree_end` from the depths of a preordered tree.
fn link_tree(entries: &mut [Entry]) {
    let total = entries.len() as u32;
    let mut stack: Vec<u32> = Vec::new();
    for index in 0..total {
        let depth = entries[index as usize].depth;
        while stack
            .last()
            .is_some_and(|&top| entries[top as usize].depth >= depth)
        {
            let top = stack.pop().unwrap();
            entries[top as usize].subtree_end = index;
        }
        entries[index as usize].parent = stack.last().copied();
        entries[index as usize].subtree_end = index + 1;
        stack.push(index);
    }
    for top in stack {
        entries[top as usize].subtree_end = total;
    }
}

fn initial_status(left: &Option<Meta>, right: &Option<Meta>) -> Status {
    match (left, right) {
        (Some(_), None) => Status::LeftOnly,
        (None, Some(_)) => Status::RightOnly,
        (Some(a), Some(b)) if a.kind != b.kind => Status::TypeMismatch,
        (Some(a), Some(b)) if a.kind == EntryKind::File && a.size != b.size => Status::Different,
        (Some(a), Some(b)) if a.kind == EntryKind::File && a.size == 0 && b.size == 0 => Status::Same,
        (Some(a), Some(_)) if a.kind == EntryKind::Dir => Status::Same,
        _ => Status::Pending,
    }
}

fn initial_status_or_gone(left: &Option<Meta>, right: &Option<Meta>) -> Status {
    match (left, right) {
        (None, None) => Status::Same,
        _ => initial_status(left, right),
    }
}

/// What one entry looks like on disk now; produced by `recheck`, consumed by `apply_updates`.
#[derive(Clone, Debug)]
pub struct EntryUpdate {
    pub index: u32,
    pub rel: PathBuf,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub status: Status,
}

/// Looks again at the given entries (index in the tree, relative path) after files were copied
/// or deleted, so that the tree can be patched without walking both folders again. Reads and
/// compares files, so it belongs on a worker thread. A cancelled comparison leaves `Pending`.
pub fn recheck(roots: &[PathBuf; 2], targets: &[(u32, PathBuf)], cancel: &AtomicBool) -> Vec<EntryUpdate> {
    let stat = |root: &Path, rel: &Path| match fs::symlink_metadata(root.join(rel)) {
        Ok(meta) => Ok(Some(to_meta(&meta))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        // A parent that became a file makes the path "not a directory": gone as well.
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(None),
        Err(error) => Err(error),
    };
    targets
        .par_iter()
        .map_init(
            || (vec![0u8; COMPARE_BUFFER_BYTES], vec![0u8; COMPARE_BUFFER_BYTES]),
            |buffers, (index, rel)| {
                let mut update = EntryUpdate {
                    index: *index,
                    rel: rel.clone(),
                    left: None,
                    right: None,
                    status: Status::Error,
                };
                let (Ok(left), Ok(right)) = (stat(&roots[0], rel), stat(&roots[1], rel)) else {
                    return update;
                };
                update.left = left;
                update.right = right;
                update.status = initial_status_or_gone(&left, &right);
                if update.status == Status::Pending {
                    let entry = Entry {
                        rel: rel.clone(),
                        depth: 0,
                        parent: None,
                        subtree_end: 0,
                        left,
                        right,
                        status: Status::Pending,
                    };
                    let (status, _) = compare_pending(&entry, &roots[0], &roots[1], buffers, cancel);
                    if !cancel.load(Ordering::Relaxed) {
                        update.status = status;
                    }
                }
                update
            },
        )
        .collect()
}

/// Patches the tree with the result of `recheck` and recomputes folder statuses. No I/O.
///
/// Entries that no longer exist on either side are removed together with everything below
/// them; their former indices are returned in ascending order so that the caller can drop the
/// same positions from any array it keeps parallel to `entries`. When the result is not empty,
/// every index held by the caller (selection, parents) is stale. Updates whose path no longer
/// matches the entry at their index (the tree was rescanned meanwhile) are ignored.
pub fn apply_updates(entries: &mut Vec<Entry>, updates: &[EntryUpdate]) -> Vec<u32> {
    for update in updates {
        if let Some(entry) = entries.get_mut(update.index as usize) {
            if entry.rel == update.rel {
                entry.left = update.left;
                entry.right = update.right;
                entry.status = update.status;
            }
        }
    }
    let mut gone = vec![false; entries.len()];
    let mut removed = Vec::new();
    for index in 0..entries.len() {
        let entry = &entries[index];
        gone[index] = (entry.left.is_none() && entry.right.is_none())
            || entry.parent.is_some_and(|parent| gone[parent as usize]);
        if gone[index] {
            removed.push(index as u32);
        }
    }
    if !removed.is_empty() {
        let mut gone = gone.into_iter();
        entries.retain(|_| !gone.next().unwrap());
        link_tree(entries);
    }
    aggregate(entries);
    removed
}

/// Recomputes folder statuses from their children. Parents always precede their children.
pub fn aggregate(entries: &mut [Entry]) {
    #[derive(Clone, Copy, Default)]
    struct Summary {
        pending: bool,
        different: bool,
    }
    let mut summaries = vec![Summary::default(); entries.len()];
    for index in (0..entries.len()).rev() {
        let both_sides_dir = entries[index]
            .left
            .zip(entries[index].right)
            .is_some_and(|(a, b)| a.kind == EntryKind::Dir && b.kind == EntryKind::Dir);
        if both_sides_dir {
            let summary = summaries[index];
            entries[index].status = if summary.different {
                Status::Different
            } else if summary.pending {
                Status::Pending
            } else {
                Status::Same
            };
        }
        if let Some(parent) = entries[index].parent {
            let status = entries[index].status;
            let summary = &mut summaries[parent as usize];
            summary.pending |= status == Status::Pending;
            summary.different |= status.is_difference();
        }
    }
}

fn read_full(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Compares two files with an early exit. Returns the verdict and the bytes read per side.
fn files_equal(
    left: &Path,
    right: &Path,
    buffers: &mut (Vec<u8>, Vec<u8>),
    cancel: &AtomicBool,
) -> io::Result<(bool, u64)> {
    let mut left = File::open(left)?;
    let mut right = File::open(right)?;
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok((false, total));
        }
        let a = read_full(&mut left, &mut buffers.0)?;
        let b = read_full(&mut right, &mut buffers.1)?;
        total += (a + b) as u64;
        if buffers.0[..a] != buffers.1[..b] {
            return Ok((false, total));
        }
        if a < buffers.0.len() {
            return Ok((true, total));
        }
    }
}

fn compare_pending(
    entry: &Entry,
    left_root: &Path,
    right_root: &Path,
    buffers: &mut (Vec<u8>, Vec<u8>),
    cancel: &AtomicBool,
) -> (Status, u64) {
    let left = left_root.join(&entry.rel);
    let right = right_root.join(&entry.rel);
    let (Some(left_meta), Some(right_meta)) = (entry.left, entry.right) else {
        return (Status::Error, 0);
    };
    match left_meta.kind {
        EntryKind::File => {
            // Hard links of one inode are necessarily equal.
            if cfg!(unix) && left_meta.file_id == right_meta.file_id {
                return (Status::Same, 0);
            }
            match files_equal(&left, &right, buffers, cancel) {
                Ok((true, bytes)) => (Status::Same, bytes),
                Ok((false, bytes)) => (Status::Different, bytes),
                Err(_) => (Status::Error, 0),
            }
        }
        EntryKind::Symlink => match (fs::read_link(&left), fs::read_link(&right)) {
            (Ok(a), Ok(b)) if a == b => (Status::Same, 0),
            (Ok(_), Ok(_)) => (Status::Different, 0),
            _ => (Status::Error, 0),
        },
        EntryKind::Other => (Status::Same, 0),
        EntryKind::Dir => (Status::Same, 0),
    }
}

/// True when `path` is on a spinning disk, where parallel reads only add seeks.
#[cfg(target_os = "linux")]
fn is_rotational(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let dev = meta.dev();
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    let device = format!("/sys/dev/block/{major}:{minor}");
    [
        format!("{device}/queue/rotational"),
        format!("{device}/../queue/rotational"),
    ]
    .iter()
    .find_map(|file| fs::read_to_string(file).ok())
    .is_some_and(|value| value.trim() == "1")
}

#[cfg(not(target_os = "linux"))]
fn is_rotational(_: &Path) -> bool {
    false
}

fn compare_threads(left: &Path, right: &Path) -> usize {
    if is_rotational(left) || is_rotational(right) {
        ROTATIONAL_COMPARE_THREADS
    } else {
        std::thread::available_parallelism()
            .map_or(8, |n| usize::from(n) * 2)
            .min(MAX_COMPARE_THREADS)
    }
}

/// Runs a full comparison, streaming events to `events`. Blocks until done or cancelled.
pub fn scan(
    left_root: PathBuf,
    right_root: PathBuf,
    options: ScanOptions,
    cancel: Arc<AtomicBool>,
    events: Sender<ScanEvent>,
    notify: impl Fn() + Sync,
) {
    let started = Instant::now();
    for root in [&left_root, &right_root] {
        if !root.is_dir() {
            let _ = events.send(ScanEvent::Failed(format!(
                "{} n'est pas un dossier",
                root.display()
            )));
            notify();
            return;
        }
    }

    let ((left, mut left_errors), (right, right_errors)) = rayon::join(
        || walk(&left_root, &options, &cancel),
        || walk(&right_root, &options, &cancel),
    );
    left_errors.extend(right_errors);
    let entries = merge_trees(left, right);
    let walk_time = started.elapsed();

    let pending: Vec<(u32, Entry)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == Status::Pending)
        .map(|(index, e)| (index as u32, e.clone()))
        .collect();
    let _ = events.send(ScanEvent::Tree {
        entries,
        walk_errors: left_errors,
        walk_time,
    });
    notify();

    let threads = options
        .compare_threads
        .unwrap_or_else(|| compare_threads(&left_root, &right_root));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("biomerge-compare-{i}"))
        .build();
    let run = || {
        pending
            .par_chunks(PROGRESS_BATCH)
            .map_init(
                || {
                    (
                        vec![0u8; COMPARE_BUFFER_BYTES],
                        vec![0u8; COMPARE_BUFFER_BYTES],
                    )
                },
                |buffers, chunk| {
                    if cancel.load(Ordering::Relaxed) {
                        return 0;
                    }
                    let mut bytes = 0;
                    let mut batch = Vec::with_capacity(chunk.len());
                    for (index, entry) in chunk {
                        let (status, read) =
                            compare_pending(entry, &left_root, &right_root, buffers, &cancel);
                        bytes += read;
                        batch.push((*index, status));
                    }
                    if !cancel.load(Ordering::Relaxed) {
                        let _ = events.send(ScanEvent::Progress(batch));
                        notify();
                    }
                    bytes
                },
            )
            .sum::<u64>()
    };
    let bytes_compared = match pool {
        Ok(pool) => pool.install(run),
        Err(_) => run(),
    };
    if !cancel.load(Ordering::Relaxed) {
        let _ = events.send(ScanEvent::Done {
            total_time: started.elapsed(),
            bytes_compared,
            threads,
        });
        notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(left: &Path, right: &Path, options: ScanOptions) -> Vec<Entry> {
        let (tx, rx) = mpsc::channel();
        scan(
            left.to_owned(),
            right.to_owned(),
            options,
            Arc::new(AtomicBool::new(false)),
            tx,
            || {},
        );
        let mut entries = Vec::new();
        let mut done = false;
        for event in rx {
            match event {
                ScanEvent::Tree { entries: tree, .. } => entries = tree,
                ScanEvent::Progress(batch) => {
                    for (index, status) in batch {
                        entries[index as usize].status = status;
                    }
                }
                ScanEvent::Done { .. } => done = true,
                ScanEvent::Failed(message) => panic!("{message}"),
            }
        }
        assert!(done);
        aggregate(&mut entries);
        entries
    }

    fn status(entries: &[Entry], rel: &str) -> Status {
        entries
            .iter()
            .find(|e| e.rel == Path::new(rel))
            .unwrap_or_else(|| panic!("{rel} missing"))
            .status
    }

    #[test]
    fn classifies_every_kind_of_difference() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "same.txt", b"content");
        write(r, "same.txt", b"content");
        write(l, "size.txt", b"short");
        write(r, "size.txt", b"much longer");
        write(l, "bytes.txt", b"abcd");
        write(r, "bytes.txt", b"abXd");
        write(l, "only-left/x.txt", b"x");
        write(r, "only-right.txt", b"y");
        write(l, "clash", b"file");
        write(r, "clash/inner.txt", b"dir");
        write(l, "empty.txt", b"");
        write(r, "empty.txt", b"");

        let entries = run(l, r, ScanOptions::default());
        assert_eq!(status(&entries, "same.txt"), Status::Same);
        assert_eq!(status(&entries, "empty.txt"), Status::Same);
        assert_eq!(status(&entries, "size.txt"), Status::Different);
        assert_eq!(status(&entries, "bytes.txt"), Status::Different);
        assert_eq!(status(&entries, "only-left"), Status::LeftOnly);
        assert_eq!(status(&entries, "only-left/x.txt"), Status::LeftOnly);
        assert_eq!(status(&entries, "only-right.txt"), Status::RightOnly);
        assert_eq!(status(&entries, "clash"), Status::TypeMismatch);
    }

    #[test]
    fn folder_status_summarizes_its_content() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "clean/deep/a.txt", b"same");
        write(r, "clean/deep/a.txt", b"same");
        write(l, "dirty/deep/a.txt", b"one");
        write(r, "dirty/deep/a.txt", b"two");
        let entries = run(l, r, ScanOptions::default());
        assert_eq!(status(&entries, "clean/deep"), Status::Same);
        assert_eq!(status(&entries, "clean"), Status::Same);
        assert_eq!(status(&entries, "dirty/deep"), Status::Different);
        assert_eq!(status(&entries, "dirty"), Status::Different);
    }

    #[test]
    fn differences_beyond_the_first_buffer_are_found() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        let content = vec![7u8; COMPARE_BUFFER_BYTES * 2];
        let mut changed = content.clone();
        *changed.last_mut().unwrap() = 8;
        write(l, "tail.bin", &content);
        write(r, "tail.bin", &changed);
        write(l, "exact.bin", &content);
        write(r, "exact.bin", &content);
        let entries = run(l, r, ScanOptions::default());
        assert_eq!(status(&entries, "tail.bin"), Status::Different);
        assert_eq!(status(&entries, "exact.bin"), Status::Same);
    }

    #[test]
    fn tree_is_preordered_with_folders_first() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "b.txt", b"");
        write(l, "A.txt", b"");
        write(l, "z/inner.txt", b"");
        write(r, "m/inner.txt", b"");
        let entries = run(l, r, ScanOptions::default());
        let order: Vec<_> = entries.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(
            order,
            ["m", "m/inner.txt", "z", "z/inner.txt", "A.txt", "b.txt"]
        );
        let z = entries.iter().position(|e| e.rel == Path::new("z")).unwrap();
        assert_eq!(entries[z].subtree_end as usize, z + 2);
        assert_eq!(entries[z + 1].parent, Some(z as u32));
        assert_eq!(entries[z + 1].depth, 2);
    }

    #[test]
    fn vcs_folders_are_skipped_but_hidden_files_are_kept() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, ".git/config", b"x");
        write(l, ".env", b"x");
        let entries = run(l, r, ScanOptions::default());
        let names: Vec<_> = entries.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(names, [".env"]);
    }

    #[test]
    fn gitignore_is_opt_in() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, ".gitignore", b"target/\n");
        write(l, "target/out.bin", b"x");
        let has_target = |entries: &[Entry]| entries.iter().any(|e| e.rel.starts_with("target"));
        assert!(has_target(&run(l, r, ScanOptions::default())));
        let options = ScanOptions {
            respect_gitignore: true,
            ..ScanOptions::default()
        };
        assert!(!has_target(&run(l, r, options)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_compare_by_target_and_are_never_followed() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        std::os::unix::fs::symlink("target-one", l.join("same")).unwrap();
        std::os::unix::fs::symlink("target-one", r.join("same")).unwrap();
        std::os::unix::fs::symlink("target-one", l.join("diff")).unwrap();
        std::os::unix::fs::symlink("target-two", r.join("diff")).unwrap();
        let entries = run(l, r, ScanOptions::default());
        assert_eq!(status(&entries, "same"), Status::Same);
        assert_eq!(status(&entries, "diff"), Status::Different);
    }

    fn filtered(left: &Path, right: &Path, filter: &str) -> Vec<String> {
        let options = ScanOptions {
            name_filter: NameFilter::parse(filter).unwrap(),
            ..ScanOptions::default()
        };
        let entries = run(left, right, options);
        entries.iter().map(|e| e.rel.to_str().unwrap().to_owned()).collect()
    }

    #[test]
    fn default_options_filter_no_name() {
        assert!(ScanOptions::default().name_filter.is_empty());
        assert!(NameFilter::parse("  ; ").unwrap().is_empty());
        assert!(NameFilter::default().accepts(Path::new("any/thing.lock"), false));
    }

    #[test]
    fn include_patterns_keep_matching_files_and_only_the_folders_leading_to_them() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "src/main.rs", b"");
        write(l, "src/deep/mod.RS", b"");
        write(l, "src/notes.txt", b"");
        write(l, "docs/guide.md", b"");
        write(r, "docs/img/logo.png", b"");
        write(l, "Cargo.toml", b"");
        write(l, "Cargo.lock", b"");
        assert_eq!(
            filtered(l, r, "*.rs *.toml"),
            ["src", "src/deep", "src/deep/mod.RS", "src/main.rs", "Cargo.toml"]
        );
        assert_eq!(filtered(l, r, "*.rs;+*.toml"), filtered(l, r, "*.rs *.toml"));
    }

    #[test]
    fn including_a_folder_keeps_everything_below_it() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "docs/guide.md", b"");
        write(l, "docs/img/logo.png", b"");
        write(l, "docs/img/logo.bak", b"");
        write(l, "src/docs/inner.txt", b"");
        write(l, "src/main.rs", b"");
        fs::create_dir(l.join("docs/empty")).unwrap();
        assert_eq!(
            filtered(l, r, "docs/ -*.bak"),
            [
                "docs",
                "docs/empty",
                "docs/img",
                "docs/img/logo.png",
                "docs/guide.md",
                "src",
                "src/docs",
                "src/docs/inner.txt"
            ]
        );
        assert_eq!(
            filtered(l, r, "/docs/ -*.bak"),
            ["docs", "docs/empty", "docs/img", "docs/img/logo.png", "docs/guide.md"]
        );
    }

    #[test]
    fn exclusion_wins_and_emptied_folders_are_dropped_but_empty_ones_stay() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "target/debug/app", b"");
        write(l, "src/target/x.rs", b"");
        write(l, "src/main.rs", b"");
        write(l, "locks/deep/Cargo.lock", b"");
        write(l, "mixed/Cargo.lock", b"");
        write(r, "mixed/kept.txt", b"");
        fs::create_dir(l.join("really-empty")).unwrap();
        let names = filtered(l, r, "-target/ -*.lock");
        assert_eq!(names, ["mixed", "mixed/kept.txt", "really-empty", "src", "src/main.rs"]);
        // Emptied on the left, not on the right: the folder exists on both sides.
        let options = ScanOptions {
            name_filter: NameFilter::parse("-*.lock").unwrap(),
            ..ScanOptions::default()
        };
        assert_eq!(status(&run(l, r, options), "mixed"), Status::Different);
        // An exclusion beats an inclusion of the same file.
        assert_eq!(filtered(l, r, "*.rs -main.*"), ["src", "src/target", "src/target/x.rs"]);
    }

    #[test]
    fn path_patterns_are_anchored_at_the_roots_and_names_ignore_case() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "build/out.o", b"");
        write(l, "sub/build/out.o", b"");
        write(l, "sub/gen/a.rs", b"");
        write(l, "sub/My File.TXT", b"");
        write(l, "photo.JPG", b"");
        assert_eq!(
            filtered(l, r, "-/build/ -**/gen/*.rs -*.jpg"),
            ["sub", "sub/build", "sub/build/out.o", "sub/My File.TXT"]
        );
        assert_eq!(filtered(l, r, "\"my file.txt\""), ["sub", "sub/My File.TXT"]);
        assert_eq!(filtered(l, r, "sub/*.txt"), ["sub", "sub/My File.TXT"]);
        assert_eq!(filtered(l, r, "!sub !build photo.{png,jpg}"), ["photo.JPG"]);
        // A trailing slash never matches a file.
        assert_eq!(filtered(l, r, "photo.jpg/"), [] as [&str; 0]);
    }

    #[cfg(unix)]
    #[test]
    fn an_excluded_folder_is_pruned_from_the_walk_not_filtered_afterwards() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "target/locked/x.bin", b"");
        write(dir.path(), "kept.txt", b"");
        let locked = dir.path().join("target/locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let walk_with = |filter: &str| {
            let options = ScanOptions {
                name_filter: NameFilter::parse(filter).unwrap(),
                ..ScanOptions::default()
            };
            walk(dir.path(), &options, &AtomicBool::new(false))
        };
        let (unfiltered, errors_without) = walk_with("");
        let (walked, errors) = walk_with("-target/");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let names: Vec<_> = walked.iter().map(|w| w.rel.to_str().unwrap()).collect();
        assert_eq!(names, ["kept.txt"]);
        // Reading the unreadable folder fails (except for root): never entering it cannot.
        assert!(errors.is_empty(), "{errors:?}");
        assert!(unfiltered.len() >= 3);
        if fs::read_dir(&locked).is_ok() {
            return;
        }
        assert_eq!(errors_without.len(), 1);
    }

    #[test]
    fn invalid_patterns_are_explained_in_french() {
        let message = |text: &str| NameFilter::parse(text).unwrap_err().to_string();
        assert_eq!(message("*.rs [a-"), "motif « [a- » invalide : crochet « [ » non refermé");
        assert_eq!(message("-{a,b"), "motif « -{a,b » invalide : accolade « { » non refermée");
        assert_eq!(message("x[z-a]"), "motif « x[z-a] » invalide : intervalle « z-a » inversé");
        assert_eq!(message("*.rs -"), "motif vide après « - »");
        assert_eq!(message("-/"), "motif vide après « -/ »");
        assert_eq!(message("\"open"), "guillemet non refermé");
        assert_eq!(NameFilter::parse(" *.rs  -x ").unwrap().source(), "*.rs  -x");
    }

    #[test]
    fn accepts_gives_the_same_answer_as_the_walk() {
        let filter = NameFilter::parse("src/ *.toml -*.lock -target/").unwrap();
        let accepts = |rel: &str, is_dir: bool| filter.accepts(Path::new(rel), is_dir);
        assert!(accepts("src", true));
        assert!(accepts("src/deep/main.rs", false));
        assert!(accepts("Cargo.toml", false));
        assert!(!accepts("README.md", false));
        assert!(!accepts("docs", true));
        assert!(!accepts("src/Cargo.lock", false));
        assert!(!accepts("src/target/debug/app", false));
        assert!(!accepts("target", true));
        assert!(accepts("src/target", false));
    }

    #[test]
    fn rechecked_entries_are_patched_and_vanished_ones_removed() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (l, r) = (a.path(), b.path());
        write(l, "d/a.txt", b"one");
        write(r, "d/a.txt", b"two");
        write(l, "d/gone/x.txt", b"x");
        write(l, "z.txt", b"z");
        let roots = [l.to_owned(), r.to_owned()];
        let mut entries = run(l, r, ScanOptions::default());
        assert_eq!(status(&entries, "d"), Status::Different);

        write(r, "d/a.txt", b"one");
        fs::remove_dir_all(l.join("d/gone")).unwrap();
        fs::remove_file(l.join("z.txt")).unwrap();
        write(r, "z.txt", b"moved to the right");
        let position = |entries: &[Entry], rel: &str| entries.iter().position(|e| e.rel == Path::new(rel)).unwrap() as u32;
        let targets: Vec<(u32, PathBuf)> = ["d/a.txt", "d/gone", "z.txt"]
            .iter()
            .map(|rel| (position(&entries, rel), PathBuf::from(rel)))
            .collect();
        let gone = position(&entries, "d/gone");
        let mut updates = recheck(&roots, &targets, &AtomicBool::new(false));
        // An update that does not describe the entry at its index is ignored.
        updates.push(EntryUpdate {
            index: 0,
            rel: PathBuf::from("not/the/entry"),
            left: None,
            right: None,
            status: Status::Error,
        });
        // The child of the vanished folder was not rechecked; it goes with its parent.
        assert_eq!(apply_updates(&mut entries, &updates), [gone, gone + 1]);
        let order: Vec<_> = entries.iter().map(|e| e.rel.to_str().unwrap()).collect();
        assert_eq!(order, ["d", "d/a.txt", "z.txt"]);
        assert_eq!(status(&entries, "d/a.txt"), Status::Same);
        assert_eq!(status(&entries, "d"), Status::Same);
        assert_eq!(status(&entries, "z.txt"), Status::RightOnly);
        assert_eq!((entries[0].subtree_end, entries[1].parent, entries[2].parent), (2, Some(0), None));
    }

    #[test]
    fn missing_root_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        scan(
            dir.path().join("nope"),
            dir.path().to_owned(),
            ScanOptions::default(),
            Arc::new(AtomicBool::new(false)),
            tx,
            || {},
        );
        assert!(matches!(rx.recv().unwrap(), ScanEvent::Failed(_)));
    }

    #[test]
    fn cancelling_emits_no_completion() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(a.path(), "x.txt", b"x");
        write(b.path(), "x.txt", b"y");
        let (tx, rx) = mpsc::channel();
        scan(
            a.path().to_owned(),
            b.path().to_owned(),
            ScanOptions::default(),
            Arc::new(AtomicBool::new(true)),
            tx,
            || {},
        );
        assert!(!rx.iter().any(|e| matches!(e, ScanEvent::Done { .. })));
    }
}
