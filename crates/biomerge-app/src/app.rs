//! Application shell: hamburger menu, tabs, status bar, palette, dialogs and settings.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use biomerge_core::Side;
use biomerge_core::diff::{DiffOptions, Whitespace};
use biomerge_core::dirscan::{NameFilter, ScanOptions};
use biomerge_core::document::Document;
use biomerge_core::extract::{self, ExtractOptions, NotePlacement, Revision};
use biomerge_core::fileops::{DeleteMode, Operation};
use biomerge_core::ignore_rules::{IgnoreRule, IgnoreRules};
use biomerge_core::json::{ArrayOrder, JsonOptions, KeyOrder};
use egui::{
    Align2, Button, Color32, FontId, Frame, Key, Margin, Modifiers, Panel, Popup, Rect,
    RichText, Sense, Stroke, Ui, ViewportCommand, WidgetInfo, pos2, vec2,
};

use crate::folder_view::{FolderAction, FolderTab, SYNC_CHOICES};
use crate::fonts;
use crate::formats::{self, Typography};
use crate::icons::{self, Icon};
use crate::instance::{Incoming, Listener, MergeRequest, Request, Requests, Waiter};
use crate::merge_view::{MergeInputs, MergeSettings, MergeTab};
use crate::palette::{Entry, Palette};
use crate::text_view::{SearchMode, SearchTarget, SideSource, StatusItem, TextSettings, TextTab};
use biomerge_core::merge3::{Resolution, has_conflict_markers};
use crate::{git, rules_editor};
use crate::theme::{self, Chrome, CodeTone, DEFAULT_CODE_SIZE, LinkStyle, Look, MAX_CODE_SIZE, MIN_CODE_SIZE, ThemeKind, UiPalette, mix};
use crate::widgets::{self, Item, dialog_frame, menu_frame, menu_width, separator};

const BAND_PADDING: f32 = 10.0;
const BAND_GAP: f32 = 8.0;
const MENU_BUTTON: f32 = 32.0;
const TAB_HEIGHT: f32 = 32.0;
const TAB_MIN_WIDTH: f32 = 90.0;
const TAB_MAX_WIDTH: f32 = 260.0;
const TAB_GAP: f32 = 2.0;
const STATUS_PADDING: f32 = 18.0;
const PILL_HEIGHT: f32 = 22.0;
const PILL_PADDING: f32 = 22.0;
const TOGGLE_SIZE: egui::Vec2 = vec2(28.0, 24.0);
const MENU_WIDTH: f32 = 300.0;
const SUBMENU_WIDTH: f32 = 260.0;
const FIELD_HEIGHT: f32 = 34.0;
const FILTER_HEIGHT: f32 = 32.0;
const FILTER_WIDTH: f32 = 420.0;
const FILTER_MIN_WIDTH: f32 = 200.0;
const FILTER_TOGGLE: f32 = 26.0;
const PROMPT_WIDTH: f32 = 440.0;
const SIDE_BUTTON: egui::Vec2 = vec2(66.0, 26.0);
const DEFAULT_CONTEXT_LINES: u32 = 3;
const NOTICE_SECONDS: f64 = 2.5;
/// A pause in typing a folder filter long enough to start the new comparison.
const FOLDER_FILTER_DELAY: f64 = 0.5;
const FOLDER_FILTER_HELP: &str = "Motifs séparés par des espaces :\n\
    *.rs *.toml  garde ces fichiers\n\
    -target/  écarte ce dossier (/ final : dossiers seulement)\n\
    -*.lock ou !*.lock  écarte ces fichiers\n\
    docs/*.md  un motif avec / part de la racine comparée\n\
    \"mon dossier/\"  guillemets pour les espaces";
/// The amounts of context offered in the menu and the palette.
const JSON_KEY_ORDERS: [(KeyOrder, &str); 3] = [
    (KeyOrder::FollowReference, "Clés dans l'ordre de la référence"),
    (KeyOrder::Alphabetical, "Clés par ordre alphabétique"),
    (KeyOrder::AsWritten, "Clés dans l'ordre des fichiers"),
];
const DOC_NOTES: [(NotePlacement, &str); 3] = [
    (NotePlacement::Inline, "Notes dans le texte"),
    (NotePlacement::End, "Notes à la fin"),
    (NotePlacement::Omit, "Notes ignorées"),
];
const DOC_TYPOGRAPHY: [(Typography, &str); 3] = [
    (Typography::Default, "Typographie : espaces invisibles ignorés"),
    (Typography::Aggressive, "Typographie : guillemets et tirets ignorés aussi"),
    (Typography::None, "Typographie : texte exact"),
];
const CONTEXT_CHOICES: [(u32, &str); 3] = [
    (0, "Contexte : aucune ligne"),
    (3, "Contexte : 3 lignes"),
    (10, "Contexte : 10 lignes"),
];

enum Tab {
    Text(Box<TextTab>),
    Folder(Box<FolderTab>),
    Merge(Box<MergeTab>),
}

impl Tab {
    fn title(&self) -> String {
        match self {
            Tab::Text(tab) => tab.title(),
            Tab::Folder(tab) => tab.title(),
            Tab::Merge(tab) => tab.title(),
        }
    }

    fn is_dirty(&self) -> bool {
        match self {
            Tab::Text(tab) => tab.is_dirty(),
            Tab::Folder(_) => false,
            Tab::Merge(tab) => tab.is_dirty(),
        }
    }

    /// Saving means something: edits to write, or a merge's result.
    fn can_save(&self) -> bool {
        match self {
            Tab::Merge(tab) => tab.is_ready(),
            tab => tab.is_dirty(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Command {
    OpenPalette,
    Find,
    FindNext,
    FindPrevious,
    FocusFilter,
    GoToLine,
    OpenDialog,
    Reload,
    Save,
    CloseTab,
    Quit,
    Undo,
    Redo,
    Copy,
    NextBlock,
    PreviousBlock,
    PushRight,
    PushLeft,
    ScrollLines(f64),
    ToggleWrap,
    ToggleAligned,
    ToggleContext,
    SetContextLines(u32),
    SetWhitespace(Whitespace),
    ToggleCase,
    ToggleGitignore,
    ToggleDifferencesOnly,
    SetAllExpanded(bool),
    /// Copies the selected entries of a folder comparison from this side to the other.
    FolderCopy(Side),
    /// Deletes the selection on the side it belongs to, or the pane clicked last.
    FolderDelete { permanent: bool },
    FolderDeleteOn(Side),
    /// A mirror or an update of the selection, or of the whole comparison without one.
    FolderSync(Operation),
    FocusFolderFilter,
    OpenIgnoreRules,
    CompareWithHead,
    SetTheme(ThemeKind),
    SetCodeTone(CodeTone),
    SetLinks(LinkStyle),
    /// Verre's window lets the desktop show through, or not.
    ToggleTransparency,
    OpenFontDialog,
    ChangeCodeSize(f32),
    ResetCodeSize,
    ActivateTab(usize),
    About,
    /// JSON tabs: the structural comparison, or the plain line one.
    ToggleJsonStructural,
    SetJsonKeyOrder(KeyOrder),
    ToggleJsonUnorderedArrays,
    ToggleJsonNumbersByValue,
    ToggleJsonStringsByValue,
    SetJsonReference(Side),
    /// Projects both sides again with the options in force.
    Realign,
    JsonSummary,
    SetDocRevision(Revision),
    SetDocNotes(NotePlacement),
    ToggleDocLinks,
    ToggleDocImages,
    ToggleDocComments,
    ToggleDocHeadersFooters,
    SetDocTypography(Typography),
    /// The dialog naming the four files of a three-way merge.
    NewMerge,
    /// The text tab's file holds conflict markers: resolve them in a merge tab.
    ResolveConflicts,
    /// Merge tabs: the base in place of the result, or the result again.
    ToggleBase,
    NextConflict,
    PreviousConflict,
    /// What the current chunk of a merge becomes.
    Resolve(Resolution),
    EditWithMarkers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterConfirm {
    CloseTab(usize),
    Reload(usize),
    Quit,
}

#[derive(Default)]
struct OpenDialog {
    paths: [String; 2],
    error: Option<String>,
    /// A single file given, holding conflict markers: a merge is offered for it.
    conflicted: Option<PathBuf>,
    /// Opened for a launch with one path: the right field takes the keyboard.
    focus_right: bool,
    /// That launch, if it waits: it goes with the tab the dialog opens, or is let go.
    waiter: Option<Waiter>,
}

/// The committed version of a file read on a worker, with the launch waiting for its tab.
type HeadJob = (PathBuf, Result<Document, String>, Option<Waiter>);

/// The four files of a three-way merge, as typed: base, local, remote, result.
#[derive(Default)]
struct MergeDialog {
    paths: [String; 4],
    error: Option<String>,
}

/// The prompt of Ctrl+G.
#[derive(Default)]
struct GoToLine {
    text: String,
    /// The pane clicked in the prompt, by index. A `g:` or `d:` typed before the number wins
    /// over it.
    side: Option<usize>,
    invalid: bool,
    focus_done: bool,
}

/// `120`, or `g:120` and `d:120` to name the side. Digits may be grouped as people write them.
#[cfg(test)]
fn parse_line_target(text: &str) -> Option<(Option<Side>, usize)> {
    let (pane, number) = parse_pane_target(text, &['g', 'd'])?;
    Some((pane.map(|pane| if pane == 0 { Side::Left } else { Side::Right }), number))
}

/// A line number, after the letter of one of `panes` (`g`, `d`, and `r` for a merge's result)
/// naming the pane by its index.
fn parse_pane_target(text: &str, panes: &[char]) -> Option<(Option<usize>, usize)> {
    let text = text.trim();
    let first = text.chars().next()?.to_ascii_lowercase();
    let (pane, number) = match panes.iter().position(|&letter| letter == first) {
        Some(pane) => (Some(pane), &text[1..]),
        None => (None, text),
    };
    let number = number.trim_start().strip_prefix(':').unwrap_or(number);
    let digits: String = number.chars().filter(|c| !c.is_whitespace() && *c != '_').collect();
    Some((pane, digits.parse().ok()?))
}

struct FontDialog {
    families: Option<Vec<String>>,
    receiver: Receiver<Vec<String>>,
    query: String,
}

impl FontDialog {
    fn new(ctx: &egui::Context) -> Self {
        let (sender, receiver) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(fonts::list_monospace());
            ctx.request_repaint();
        });
        Self {
            families: None,
            receiver,
            query: String::new(),
        }
    }
}

pub struct App {
    tabs: Vec<Tab>,
    active: usize,
    theme: ThemeKind,
    code_tone: CodeTone,
    links: LinkStyle,
    /// Verre only: the compositor blurs the desktop behind the window.
    transparent: bool,
    /// What the four settings above give, worked out when one of them changes.
    look: Look,
    /// Ctrl+wheel steps the code size; a touchpad gives fractions of a step.
    zoom_scroll: f32,
    code_family: Option<String>,
    code_size: f32,
    wrap_lines: bool,
    aligned_lines: bool,
    diff_options: DiffOptions,
    scan_options: ScanOptions,
    differences_only: bool,
    /// Text comparisons fold what did not change, but for `context_lines` around each block.
    context_only: bool,
    context_lines: u32,
    palette: Option<Palette>,
    focus_search: bool,
    go_to_line: Option<GoToLine>,
    open_dialog: Option<OpenDialog>,
    merge_dialog: Option<MergeDialog>,
    merge_picker: (Sender<(usize, PathBuf)>, Receiver<(usize, PathBuf)>),
    font_dialog: Option<FontDialog>,
    picker: (Sender<(usize, PathBuf)>, Receiver<(usize, PathBuf)>),
    confirm: Option<AfterConfirm>,
    about: bool,
    close_allowed: bool,
    message: Option<String>,
    /// The rules as edited, kept in the settings; `compiled_rules` is what the tabs use.
    ignore_rules: Vec<IgnoreRule>,
    compiled_rules: IgnoreRules,
    rules_editor: bool,
    /// The text of the folder filter field; what is in force is `scan_options.name_filter`.
    folder_filter: String,
    folder_filter_error: Option<String>,
    /// When the field was last typed in, until its patterns are applied.
    folder_filter_edited: Option<f64>,
    /// Parallel to `tabs`: the launch waiting for that tab to close (`--wait`).
    waiters: Vec<Option<Waiter>>,
    /// Requests of later launches (single instance), and the socket they come through.
    requests: Option<Requests>,
    listener: Option<Listener>,
    git_jobs: (Sender<HeadJob>, Receiver<HeadJob>),
    git_pending: usize,
    /// What new JSON tabs normalise, and whether they open compared structurally.
    json_options: JsonOptions,
    json_structural: bool,
    /// A passing word in the status bar (something was copied), with when it was said.
    notice: Option<(String, f64)>,
    /// How documents are turned into text, in text tabs and folder comparisons.
    extract_options: ExtractOptions,
}

impl App {
    pub fn new(
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
        initial: Option<Request>,
    ) -> Self {
        std::thread::spawn(biomerge_core::highlight::preload);
        // Ctrl+plus and Ctrl+minus size the code, not the whole window.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        let load = |key: &str| storage.and_then(|s| s.get_string(key));
        let theme = ThemeKind::from_stored(load("theme").as_deref());
        let code_tone = load("code_tone").and_then(|v| CodeTone::from_key(&v)).unwrap_or(CodeTone::Theme);
        let links = load("link_style").and_then(|v| LinkStyle::from_key(&v)).unwrap_or(LinkStyle::Ribbons);
        let transparent = load("transparent_window").is_none_or(|v| v == "true");
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            theme,
            code_tone,
            links,
            transparent,
            look: Look::new(theme, code_tone, transparent, links),
            zoom_scroll: 0.0,
            code_family: load("code_family").filter(|family| !family.is_empty()),
            code_size: load("code_size")
                .and_then(|size| size.parse().ok())
                .map_or(DEFAULT_CODE_SIZE, |size: f32| size.clamp(MIN_CODE_SIZE, MAX_CODE_SIZE)),
            wrap_lines: load("wrap_lines").is_none_or(|v| v == "true"),
            aligned_lines: load("aligned_lines").is_some_and(|v| v == "true"),
            diff_options: DiffOptions::default(),
            scan_options: ScanOptions::default(),
            differences_only: false,
            context_only: load("context_only").is_some_and(|v| v == "true"),
            context_lines: load("context_lines").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_CONTEXT_LINES),
            palette: None,
            focus_search: false,
            go_to_line: None,
            open_dialog: None,
            merge_dialog: None,
            merge_picker: channel(),
            font_dialog: None,
            picker: channel(),
            confirm: None,
            about: false,
            close_allowed: false,
            message: None,
            ignore_rules: load("ignore_rules").map(|text| rules_editor::decode(&text)).unwrap_or_default(),
            compiled_rules: IgnoreRules::default(),
            rules_editor: false,
            folder_filter: load("folder_filter").unwrap_or_default(),
            folder_filter_error: None,
            folder_filter_edited: None,
            waiters: Vec::new(),
            requests: None,
            listener: None,
            git_jobs: channel(),
            git_pending: 0,
            json_options: JsonOptions::default(),
            json_structural: true,
            notice: None,
            extract_options: ExtractOptions::default(),
        };
        (app.json_options, app.json_structural) = formats::load_json(&load);
        app.extract_options = formats::load_extract(&load);
        app.scan_extras_changed();
        app.compiled_rules = rules_editor::compile(&app.ignore_rules);
        match NameFilter::parse(&app.folder_filter) {
            Ok(filter) => app.scan_options.name_filter = filter,
            Err(error) => app.folder_filter_error = Some(error.to_string()),
        }
        app.install_look(ctx);
        if let Some(request) = initial {
            if let Some(merge) = request.merge {
                app.open_merge(ctx, merge_inputs(merge), None);
            } else if let Some(path) = request.head {
                app.compare_with_head(ctx, path, None);
            } else if let Some(path) = request.single {
                app.open_single(ctx, path, None);
            } else if let Some(paths) = request.paths {
                if let Err(error) = app.open_labelled(ctx, paths, request.labels) {
                    app.message = Some(error);
                }
            }
        }
        app
    }

    /// The launch that opened the window waits on its tab (a merge from `git mergetool`, or a
    /// single file in conflict). False when that tab is not a merge: there is no code to wait
    /// for.
    pub fn wait_on_first_tab(&mut self, mut waiter: Waiter) -> bool {
        if let (Some(Tab::Merge(tab)), Some(slot)) = (self.tabs.first(), self.waiters.first_mut()) {
            waiter.set_code(tab.exit_code());
            *slot = Some(waiter);
            return true;
        }
        false
    }

    /// One path alone: a file git left in conflict is merged in place, anything else is the
    /// left side of the comparison dialog, which waits for the right one.
    fn open_single(&mut self, ctx: &egui::Context, path: PathBuf, waiter: Option<Waiter>) {
        let conflicted = path.is_file() && Document::load(&path).is_ok_and(|doc| has_conflict_markers(&doc.lines));
        if conflicted {
            self.open_dialog = None;
            self.open_merge(ctx, MergeInputs::Conflicted(path), waiter);
        } else {
            self.open_dialog = Some(OpenDialog {
                paths: [path.to_string_lossy().into_owned(), String::new()],
                focus_right: true,
                waiter,
                ..OpenDialog::default()
            });
        }
    }

    fn merge_settings(&self) -> MergeSettings {
        MergeSettings {
            options: self.diff_options,
            rules: self.compiled_rules.clone(),
            wrap_lines: self.wrap_lines,
            aligned: self.aligned_lines,
            syntax_theme: self.look.syntax,
        }
    }

    /// A merge tab: it reads and merges the files on a worker, and shows why if it cannot.
    fn open_merge(&mut self, ctx: &egui::Context, inputs: MergeInputs, waiter: Option<Waiter>) {
        let tab = MergeTab::new(ctx, inputs, self.merge_settings());
        self.push_tab(Tab::Merge(Box::new(tab)), waiter);
    }

    /// Takes the requests of later launches from now on. The socket goes with the window.
    pub fn listen(&mut self, ctx: &egui::Context, listener: Listener) {
        let ctx = ctx.clone();
        self.requests = listener.serve(move || ctx.request_repaint());
        self.listener = Some(listener);
    }

    fn install_look(&self, ctx: &egui::Context) {
        fonts::install(ctx, self.code_family.as_deref());
        theme::apply_style(ctx, &self.look);
    }

    /// Works the look out again after a change of one of its settings: styles, and the code
    /// colours of every tab.
    fn refresh_look(&mut self, ctx: &egui::Context) {
        let syntax = self.look.syntax;
        self.look = Look::new(self.theme, self.code_tone, self.transparent, self.links);
        theme::apply_style(ctx, &self.look);
        if self.look.syntax != syntax {
            let syntax = self.look.syntax;
            for tab in &mut self.tabs {
                match tab {
                    Tab::Text(tab) => tab.set_syntax_theme(ctx, syntax),
                    Tab::Merge(tab) => tab.set_syntax_theme(ctx, syntax),
                    Tab::Folder(_) => {}
                }
            }
        }
    }

    fn open(&mut self, ctx: &egui::Context, paths: [PathBuf; 2]) -> Result<(), String> {
        self.open_labelled(ctx, paths, [None, None])
    }

    /// Opens a comparison whose sides may be shown under other names than their files'.
    fn open_labelled(&mut self, ctx: &egui::Context, paths: [PathBuf; 2], labels: [Option<String>; 2]) -> Result<(), String> {
        let tab = match (paths[0].is_dir(), paths[1].is_dir()) {
            (true, true) => {
                let mut tab = FolderTab::new(ctx, paths, self.scan_options.clone(), self.differences_only);
                tab.set_labels(labels);
                tab.set_equivalence_options(self.extract_options, self.json_options.clone());
                Tab::Folder(Box::new(tab))
            }
            (false, false) => {
                for path in &paths {
                    if !path.is_file() {
                        return Err(format!("{} introuvable", path.display()));
                    }
                }
                self.file_tab(ctx, paths, labels, false)
            }
            _ => return Err("Choisis deux fichiers ou deux dossiers".to_owned()),
        };
        self.push_tab(tab, None);
        Ok(())
    }

    /// Every new tab comes through here, so that `waiters` stays parallel to `tabs`.
    fn push_tab(&mut self, tab: Tab, mut waiter: Option<Waiter>) {
        // A merge's launch hears "unresolved" until the tab says otherwise.
        if let (Tab::Merge(merge), Some(waiter)) = (&tab, &mut waiter) {
            waiter.set_code(merge.exit_code());
        }
        self.tabs.push(tab);
        self.waiters.push(waiter);
        self.active = self.tabs.len() - 1;
    }

    fn text_settings(&self) -> TextSettings {
        TextSettings {
            options: self.diff_options,
            rules: self.compiled_rules.clone(),
            wrap_lines: self.wrap_lines,
            aligned: self.aligned_lines,
            context: self.context(),
            syntax_theme: self.look.syntax,
            json: self.json_options.clone(),
            json_structural: self.json_structural,
            extract: self.extract_options,
        }
    }

    /// A tab on two files. A word-processing document is compared by its text, read-only.
    fn file_tab(&self, ctx: &egui::Context, paths: [PathBuf; 2], labels: [Option<String>; 2], allow_missing: bool) -> Tab {
        let mut index = 0;
        let sources = labels.map(|label| {
            let path = &paths[index];
            index += 1;
            let document = extract::is_supported(path) && (path.exists() || !allow_missing);
            SideSource {
                label,
                read_only: document,
                extract: document.then_some(self.extract_options),
                ..SideSource::default()
            }
        });
        self.text_tab_with(ctx, paths, allow_missing, sources)
    }

    /// Folder tabs follow the options their "même texte" check depends on.
    fn scan_extras_changed(&mut self) {
        let (extract, json) = (self.extract_options, self.json_options.clone());
        for tab in &mut self.tabs {
            if let Tab::Folder(tab) = tab {
                tab.set_equivalence_options(extract, json.clone());
            }
        }
    }

    /// The JSON options shown and changed: those of the active JSON tab, or the defaults.
    fn current_json_options(&self) -> JsonOptions {
        match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) => tab.json_options().cloned().unwrap_or_else(|| self.json_options.clone()),
            _ => self.json_options.clone(),
        }
    }

    /// Applies a change of JSON option: kept for new tabs, and the active JSON tab is projected
    /// again with it (after asking, if it holds edits).
    fn change_json(&mut self, ctx: &egui::Context, change: impl FnOnce(&mut JsonOptions)) {
        let mut options = self.current_json_options();
        change(&mut options);
        self.json_options = options.clone();
        if let Some(tab) = self.active_text().filter(|tab| tab.is_json()) {
            tab.request_projection(ctx, options);
        }
        self.scan_extras_changed();
    }

    /// Applies a change of extraction option to every document tab and folder comparison.
    fn change_extract(&mut self, ctx: &egui::Context, change: impl FnOnce(&mut ExtractOptions)) {
        change(&mut self.extract_options);
        let options = self.extract_options;
        for tab in &mut self.tabs {
            if let Tab::Text(tab) = tab {
                tab.set_extract_options(ctx, options);
            }
        }
        self.scan_extras_changed();
    }

    fn text_tab_with(&self, ctx: &egui::Context, paths: [PathBuf; 2], allow_missing: bool, sources: [SideSource; 2]) -> Tab {
        Tab::Text(Box::new(TextTab::new(ctx, paths, allow_missing, self.text_settings(), sources)))
    }

    /// A request from another launch: a tab, then the window to the front.
    fn handle_incoming(&mut self, ctx: &egui::Context, incoming: Incoming) {
        if let Some(merge) = incoming.request.merge.clone() {
            let waiter = incoming.accept();
            self.open_merge(ctx, merge_inputs(merge), waiter);
        } else if let Some(path) = incoming.request.head.clone() {
            // Git answers later; what goes wrong is told in the window, as for the menu.
            let waiter = incoming.accept();
            self.compare_with_head(ctx, path, waiter);
        } else if let Some(path) = incoming.request.single.clone() {
            let waiter = incoming.accept();
            self.open_single(ctx, path, waiter);
        } else if let Some(paths) = incoming.request.paths.clone() {
            match self.open_labelled(ctx, paths, incoming.request.labels.clone()) {
                Ok(()) => {
                    let waiter = incoming.accept();
                    if let Some(slot) = self.waiters.last_mut() {
                        *slot = waiter;
                    }
                }
                Err(error) => {
                    incoming.refuse(&error);
                    self.message = Some(error);
                }
            }
        } else {
            incoming.accept();
        }
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    /// The file a "compare with HEAD" is about: the side being edited in the active text tab,
    /// else its right side, as long as it is a real file.
    fn head_candidate(&self) -> Option<PathBuf> {
        let Some(Tab::Text(tab)) = self.tabs.get(self.active) else {
            return None;
        };
        let paths = tab.disk_paths();
        let preferred = tab.caret_side().into_iter().chain([Side::Right, Side::Left]);
        preferred
            .filter_map(|side| paths[side.index()])
            .find(|path| path.is_file())
            .map(Path::to_path_buf)
    }

    /// Reads the committed version beside the interface; a tab opens when it arrives, and the
    /// waiting launch, if any, goes with it.
    fn compare_with_head(&mut self, ctx: &egui::Context, path: PathBuf, waiter: Option<Waiter>) {
        let sender = self.git_jobs.0.clone();
        let ctx = ctx.clone();
        self.git_pending += 1;
        std::thread::spawn(move || {
            let result = git::head_version(&path);
            let _ = sender.send((path, result, waiter));
            ctx.request_repaint();
        });
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        let incoming: Vec<Incoming> = self.requests.as_ref().map_or_else(Vec::new, |r| r.try_iter().collect());
        for incoming in incoming {
            self.handle_incoming(ctx, incoming);
        }
        while let Ok((path, result, waiter)) = self.git_jobs.1.try_recv() {
            self.git_pending -= 1;
            match result {
                Ok(head) => {
                    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    let head_side = SideSource {
                        label: Some(format!("{name} (HEAD)")),
                        contents: Some(head),
                        read_only: true,
                        ..SideSource::default()
                    };
                    let tab = self.text_tab_with(ctx, [path.clone(), path], false, [head_side, SideSource::default()]);
                    self.push_tab(tab, waiter);
                    self.message = None;
                }
                Err(error) => self.message = Some(error),
            }
        }
        // Copies and deletions finish even in a tab not shown; text tabs on what they touched
        // must know.
        let mut touched = Vec::new();
        for tab in &mut self.tabs {
            if let Tab::Folder(tab) = tab {
                tab.poll_operation(ctx);
                touched.extend(tab.take_touched_paths());
            }
        }
        if !touched.is_empty() {
            for tab in &mut self.tabs {
                if let Tab::Text(tab) = tab {
                    let sides = tab.disk_paths().map(|path| path.is_some_and(|p| touched.iter().any(|t| t == p)));
                    for side in [Side::Left, Side::Right] {
                        if sides[side.index()] {
                            tab.changed_on_disk(ctx, side);
                        }
                    }
                }
            }
        }
        if let Some(edited) = self.folder_filter_edited {
            let waited = ctx.input(|i| i.time) - edited;
            if waited >= FOLDER_FILTER_DELAY {
                self.apply_folder_filter(ctx);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(FOLDER_FILTER_DELAY - waited));
            }
        }
    }

    /// The patterns of the field become the ones every folder comparison uses.
    fn apply_folder_filter(&mut self, ctx: &egui::Context) {
        self.folder_filter_edited = None;
        match NameFilter::parse(&self.folder_filter) {
            Ok(filter) => {
                self.folder_filter_error = None;
                for tab in &mut self.tabs {
                    if let Tab::Folder(tab) = tab {
                        tab.set_name_filter(ctx, &filter);
                    }
                }
                self.scan_options.name_filter = filter;
            }
            Err(error) => self.folder_filter_error = Some(error.to_string()),
        }
    }

    /// New rules are compiled once, then every text tab compares again with them.
    fn rules_changed(&mut self) {
        self.compiled_rules = rules_editor::compile(&self.ignore_rules);
        for tab in &mut self.tabs {
            match tab {
                Tab::Text(tab) => tab.set_rules(self.compiled_rules.clone()),
                Tab::Merge(tab) => tab.set_rules(self.compiled_rules.clone()),
                Tab::Folder(_) => {}
            }
        }
    }

    fn context(&self) -> Option<u32> {
        self.context_only.then_some(self.context_lines)
    }

    fn broadcast_context(&mut self) {
        let context = self.context();
        for tab in &mut self.tabs {
            if let Tab::Text(tab) = tab {
                tab.set_context(context);
            }
        }
    }

    /// Puts the keyboard in the field of the band, for one of its two uses.
    fn focus_search(&mut self, mode: SearchMode) {
        if self.active_folder().is_some() {
            // A folder comparison has one field: the filter of the names.
            self.focus_search = true;
            return;
        }
        let Some(tab) = self.active_search() else {
            return;
        };
        tab.set_search_mode(mode);
        // As editors do: finding starts from the words that are selected.
        let selected = tab.selected_text().filter(|text| !text.contains('\n') && text.len() <= 200);
        if let (SearchMode::Find, Some(text)) = (mode, selected) {
            if *tab.query() != text {
                *tab.query() = text;
                tab.apply_search();
            }
        }
        self.focus_search = true;
    }

    /// The tab the field of the band finds in.
    fn active_search(&mut self) -> Option<&mut dyn SearchTarget> {
        match self.tabs.get_mut(self.active) {
            Some(Tab::Text(tab)) => Some(tab.as_mut()),
            Some(Tab::Merge(tab)) => Some(tab.as_mut()),
            _ => None,
        }
    }

    fn active_merge(&mut self) -> Option<&mut MergeTab> {
        match self.tabs.get_mut(self.active) {
            Some(Tab::Merge(tab)) => Some(tab),
            _ => None,
        }
    }

    fn active_text(&mut self) -> Option<&mut TextTab> {
        match self.tabs.get_mut(self.active) {
            Some(Tab::Text(tab)) => Some(tab),
            _ => None,
        }
    }

    fn active_folder(&mut self) -> Option<&mut FolderTab> {
        match self.tabs.get_mut(self.active) {
            Some(Tab::Folder(tab)) => Some(tab),
            _ => None,
        }
    }

    fn run(&mut self, ctx: &egui::Context, command: Command) {
        match command {
            Command::OpenPalette => self.palette = Some(Palette::default()),
            Command::Find => self.focus_search(SearchMode::Find),
            Command::FocusFilter => self.focus_search(SearchMode::Filter),
            Command::FindNext => {
                if let Some(tab) = self.active_search() {
                    tab.find_step(true);
                }
            }
            Command::FindPrevious => {
                if let Some(tab) = self.active_search() {
                    tab.find_step(false);
                }
            }
            Command::GoToLine => {
                let ready = match self.tabs.get(self.active) {
                    Some(Tab::Text(tab)) => tab.is_ready(),
                    Some(Tab::Merge(tab)) => tab.is_ready(),
                    _ => false,
                };
                if ready {
                    self.go_to_line = Some(GoToLine::default());
                }
            }
            Command::OpenDialog => self.open_dialog = Some(OpenDialog::default()),
            Command::Reload => {
                if self.tabs.get(self.active).is_some_and(Tab::is_dirty) {
                    self.confirm = Some(AfterConfirm::Reload(self.active));
                } else {
                    self.reload(ctx, self.active);
                }
            }
            Command::Save => self.save_tab(self.active),
            Command::CloseTab => self.request_close(self.active),
            Command::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
            Command::Undo => match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.undo(ctx),
                Some(Tab::Merge(tab)) => tab.undo(ctx),
                _ => {}
            },
            Command::Redo => match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.redo(ctx),
                Some(Tab::Merge(tab)) => tab.redo(ctx),
                _ => {}
            },
            Command::Copy => {
                if let Some(text) = self.active_search().and_then(|tab| tab.selected_text()) {
                    ctx.copy_text(text);
                }
            }
            // In a merge, Ctrl+↑ and Ctrl+↓ go from change to change, conflicts or not.
            Command::NextBlock => match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.go_to_block(true),
                Some(Tab::Merge(tab)) => tab.go_to_change(true),
                _ => {}
            },
            Command::PreviousBlock => match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.go_to_block(false),
                Some(Tab::Merge(tab)) => tab.go_to_change(false),
                _ => {}
            },
            // The same keys send a block in a text and copy entries in a folder comparison. In a
            // merge, text flows the way the arrow points, as in a comparison: Ctrl+→ takes the
            // left side into the result, Ctrl+← the right side.
            Command::PushRight => match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.push_current(ctx, Side::Right),
                Some(Tab::Folder(_)) => self.run(ctx, Command::FolderCopy(Side::Left)),
                Some(Tab::Merge(tab)) => tab.take_current(ctx, Resolution::Left),
                None => {}
            },
            Command::PushLeft => match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.push_current(ctx, Side::Left),
                Some(Tab::Folder(_)) => self.run(ctx, Command::FolderCopy(Side::Right)),
                Some(Tab::Merge(tab)) => tab.take_current(ctx, Resolution::Right),
                None => {}
            },
            Command::Resolve(resolution) => {
                if let Some(tab) = self.active_merge() {
                    tab.take_current(ctx, resolution);
                }
            }
            Command::EditWithMarkers => {
                if let Some(tab) = self.active_merge() {
                    if let Some(chunk) = tab.current_conflict() {
                        tab.edit_with_markers(ctx, chunk);
                    }
                }
            }
            Command::NextConflict => {
                if let Some(tab) = self.active_merge() {
                    tab.go_to_unresolved(true);
                }
            }
            Command::PreviousConflict => {
                if let Some(tab) = self.active_merge() {
                    tab.go_to_unresolved(false);
                }
            }
            Command::ToggleBase => {
                if let Some(tab) = self.active_merge() {
                    tab.toggle_base();
                }
            }
            Command::NewMerge => self.merge_dialog = Some(MergeDialog::default()),
            Command::ResolveConflicts => {
                let path = match self.tabs.get(self.active) {
                    Some(Tab::Text(tab)) => tab.conflicted_path().map(Path::to_path_buf),
                    _ => None,
                };
                if let Some(path) = path {
                    self.open_merge(ctx, MergeInputs::Conflicted(path), None);
                }
            }
            Command::FolderCopy(from) => {
                if let Some(tab) = self.active_folder() {
                    tab.start_operation(ctx, Operation::Copy { from });
                }
            }
            Command::FolderDelete { permanent } => {
                if let Some(tab) = self.active_folder() {
                    let side = tab.delete_side();
                    tab.start_operation(ctx, Operation::Delete { side, mode: DeleteMode::Trash });
                    if permanent && tab.is_operating() {
                        FolderTab::ask_permanent(ctx);
                    }
                }
            }
            Command::FolderDeleteOn(side) => {
                if let Some(tab) = self.active_folder() {
                    tab.start_operation(ctx, Operation::Delete { side, mode: DeleteMode::Trash });
                }
            }
            Command::FolderSync(operation) => {
                if let Some(tab) = self.active_folder() {
                    tab.start_operation(ctx, operation);
                }
            }
            Command::FocusFolderFilter => self.focus_search = true,
            Command::OpenIgnoreRules => self.rules_editor = true,
            Command::CompareWithHead => match self.head_candidate() {
                Some(path) => self.compare_with_head(ctx, path, None),
                None => self.message = Some("Aucun fichier à comparer avec HEAD dans cet onglet".to_owned()),
            },
            Command::ScrollLines(lines) => match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.scroll_lines(lines),
                Some(Tab::Merge(tab)) => tab.scroll_lines(lines),
                _ => {}
            },
            // A document tab wraps on its own: the key acts on it alone.
            Command::ToggleWrap if self.active_text().is_some_and(|tab| tab.owns_wrap()) => {
                if let Some(tab) = self.active_text() {
                    let wrap = !tab.wraps();
                    tab.set_wrap(wrap);
                }
            }
            Command::ToggleWrap => {
                self.wrap_lines = !self.wrap_lines;
                let wrap_lines = self.wrap_lines;
                for tab in &mut self.tabs {
                    match tab {
                        Tab::Text(tab) if !tab.owns_wrap() => tab.set_wrap(wrap_lines),
                        Tab::Merge(tab) => tab.set_wrap(wrap_lines),
                        _ => {}
                    }
                }
            }
            Command::ToggleAligned => {
                self.aligned_lines = !self.aligned_lines;
                let aligned = self.aligned_lines;
                for tab in &mut self.tabs {
                    match tab {
                        Tab::Text(tab) => tab.set_aligned(aligned),
                        Tab::Merge(tab) => tab.set_aligned(aligned),
                        Tab::Folder(_) => {}
                    }
                }
            }
            Command::ToggleContext => {
                self.context_only = !self.context_only;
                self.broadcast_context();
            }
            // Choosing an amount of context is asking for the mode.
            Command::SetContextLines(lines) => {
                self.context_lines = lines;
                self.context_only = true;
                self.broadcast_context();
            }
            Command::SetWhitespace(whitespace) => {
                self.diff_options.whitespace = whitespace;
                self.broadcast_options();
            }
            Command::ToggleCase => {
                self.diff_options.ignore_case = !self.diff_options.ignore_case;
                self.broadcast_options();
            }
            Command::ToggleGitignore => {
                self.scan_options.respect_gitignore = !self.scan_options.respect_gitignore;
                let respect = self.scan_options.respect_gitignore;
                for tab in &mut self.tabs {
                    if let Tab::Folder(tab) = tab {
                        tab.set_respect_gitignore(ctx, respect);
                    }
                }
            }
            Command::ToggleDifferencesOnly => {
                self.differences_only = !self.differences_only;
                let value = self.differences_only;
                for tab in &mut self.tabs {
                    if let Tab::Folder(tab) = tab {
                        tab.set_differences_only(value);
                    }
                }
            }
            Command::SetAllExpanded(expanded) => {
                if let Some(tab) = self.active_folder() {
                    tab.set_all_expanded(expanded);
                }
            }
            Command::SetTheme(kind) => {
                self.theme = kind;
                self.refresh_look(ctx);
            }
            Command::SetCodeTone(tone) => {
                self.code_tone = tone;
                self.refresh_look(ctx);
            }
            Command::SetLinks(links) => {
                self.links = links;
                self.refresh_look(ctx);
            }
            Command::ToggleTransparency => {
                self.transparent = !self.transparent;
                self.refresh_look(ctx);
            }
            Command::OpenFontDialog => self.font_dialog = Some(FontDialog::new(ctx)),
            Command::ChangeCodeSize(delta) => {
                self.code_size = (self.code_size + delta).clamp(MIN_CODE_SIZE, MAX_CODE_SIZE);
            }
            Command::ResetCodeSize => self.code_size = DEFAULT_CODE_SIZE,
            Command::ActivateTab(index) => {
                if index < self.tabs.len() {
                    self.active = index;
                }
            }
            Command::About => self.about = true,
            Command::ToggleJsonStructural => match self.active_text().filter(|tab| tab.is_json()) {
                Some(tab) => {
                    let structural = !tab.is_structural();
                    tab.set_structural(ctx, structural);
                    self.json_structural = structural;
                }
                None => self.json_structural = !self.json_structural,
            },
            Command::SetJsonKeyOrder(order) => self.change_json(ctx, |options| options.key_order = order),
            Command::ToggleJsonUnorderedArrays => self.change_json(ctx, |options| {
                options.array_order = match options.array_order {
                    ArrayOrder::Ordered => ArrayOrder::Unordered,
                    ArrayOrder::Unordered => ArrayOrder::Ordered,
                };
            }),
            Command::ToggleJsonNumbersByValue => self.change_json(ctx, |options| options.numbers_by_value ^= true),
            Command::ToggleJsonStringsByValue => self.change_json(ctx, |options| options.strings_by_value ^= true),
            Command::SetJsonReference(side) => self.change_json(ctx, |options| options.reference = side),
            Command::Realign => {
                if let Some(tab) = self.active_text() {
                    tab.realign(ctx);
                }
            }
            Command::JsonSummary => {
                if let Some(tab) = self.active_text().filter(|tab| tab.is_json() && tab.is_ready()) {
                    tab.open_json_summary(ctx);
                }
            }
            Command::SetDocRevision(revision) => self.change_extract(ctx, |options| options.revision = revision),
            Command::SetDocNotes(notes) => self.change_extract(ctx, |options| options.notes = notes),
            Command::ToggleDocLinks => self.change_extract(ctx, |options| options.link_targets ^= true),
            Command::ToggleDocImages => self.change_extract(ctx, |options| options.images ^= true),
            Command::ToggleDocComments => self.change_extract(ctx, |options| options.comments ^= true),
            Command::ToggleDocHeadersFooters => self.change_extract(ctx, |options| options.headers_footers ^= true),
            Command::SetDocTypography(typography) => {
                self.change_extract(ctx, |options| options.normalization = typography.rules());
            }
        }
    }

    fn broadcast_options(&mut self) {
        let options = self.diff_options;
        for tab in &mut self.tabs {
            match tab {
                Tab::Text(tab) => tab.set_options(options),
                Tab::Merge(tab) => tab.set_options(options),
                Tab::Folder(_) => {}
            }
        }
    }

    fn reload(&mut self, ctx: &egui::Context, index: usize) {
        match self.tabs.get_mut(index) {
            Some(Tab::Text(tab)) => tab.reload(ctx),
            Some(Tab::Folder(tab)) => tab.rescan(ctx),
            Some(Tab::Merge(tab)) => tab.reload(ctx),
            None => {}
        }
    }

    fn save_tab(&mut self, index: usize) {
        let saved = match self.tabs.get_mut(index) {
            Some(Tab::Text(tab)) => tab.save(),
            Some(Tab::Merge(tab)) => tab.save(),
            _ => return,
        };
        // Saved, or a question of the tab is shown: it shows on that tab.
        if matches!(saved, Ok(false)) {
            self.active = index;
        }
        self.message = saved.err();
        self.merge_saved(index);
    }

    /// A merge's launch hears how its tab stands after each save.
    fn merge_saved(&mut self, index: usize) {
        if let (Some(Tab::Merge(tab)), Some(Some(waiter))) = (self.tabs.get(index), self.waiters.get_mut(index)) {
            waiter.set_code(tab.exit_code());
        }
    }

    fn request_close(&mut self, index: usize) {
        if self.tabs.get(index).is_some_and(Tab::is_dirty) {
            self.confirm = Some(AfterConfirm::CloseTab(index));
        } else {
            self.close_tab(index);
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.tabs.remove(index);
            // A launch waiting on this tab (`--wait`) may now return.
            self.waiters.remove(index);
            if self.active > index || self.active >= self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        }
    }

    /// Ctrl+wheel sizes the code, a notch a step; a touchpad's points add up to steps.
    fn zoom_with_wheel(&mut self, ctx: &egui::Context) {
        let delta: f32 = ctx.input(|input| {
            input
                .events
                .iter()
                .map(|event| match event {
                    egui::Event::MouseWheel { unit, delta, modifiers, .. } if modifiers.command => match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Point => delta.y / 40.0,
                        egui::MouseWheelUnit::Page => delta.y * 3.0,
                    },
                    _ => 0.0,
                })
                .sum()
        });
        if delta == 0.0 {
            return;
        }
        self.zoom_scroll += delta;
        while self.zoom_scroll.abs() >= 1.0 {
            let step = self.zoom_scroll.signum();
            self.zoom_scroll -= step;
            self.run(ctx, Command::ChangeCodeSize(step));
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        // Tab belongs to the text: before any widget of this pass claims the keyboard focus.
        let in_text = match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) => tab.has_caret(),
            Some(Tab::Merge(tab)) => tab.has_caret(),
            _ => false,
        };
        // F7 and F8 step through blocks; in a merge, through the conflicts left to resolve.
        let merging = matches!(self.tabs.get(self.active), Some(Tab::Merge(_)));
        let (previous, next) = if merging {
            (Command::PreviousConflict, Command::NextConflict)
        } else {
            (Command::PreviousBlock, Command::NextBlock)
        };
        if in_text && !typing && ctx.input(|input| input.key_pressed(Key::Tab)) {
            ctx.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
        }
        let none = Modifiers::NONE;
        let ctrl = Modifiers::COMMAND;
        // egui lets a plain key match while Alt or Shift is held: those keys must come first.
        let shortcuts = [
            (ctrl | Modifiers::SHIFT, Key::F, Command::FocusFilter, true),
            (Modifiers::SHIFT, Key::F3, Command::FindPrevious, true),
            (Modifiers::ALT, Key::ArrowUp, Command::ScrollLines(-1.0), false),
            (Modifiers::ALT, Key::ArrowDown, Command::ScrollLines(1.0), false),
            (ctrl, Key::ArrowUp, Command::PreviousBlock, false),
            (ctrl, Key::ArrowDown, Command::NextBlock, false),
            (ctrl, Key::P, Command::OpenPalette, true),
            (ctrl, Key::F, Command::Find, true),
            (ctrl, Key::G, Command::GoToLine, true),
            (none, Key::F3, Command::FindNext, true),
            (ctrl, Key::O, Command::OpenDialog, true),
            (ctrl, Key::S, Command::Save, true),
            (ctrl, Key::W, Command::CloseTab, true),
            (ctrl, Key::Q, Command::Quit, true),
            (none, Key::F5, Command::Reload, true),
            (none, Key::F7, previous, true),
            (none, Key::F8, next, true),
            (ctrl, Key::Plus, Command::ChangeCodeSize(1.0), true),
            (ctrl, Key::Equals, Command::ChangeCodeSize(1.0), true),
            (ctrl, Key::Minus, Command::ChangeCodeSize(-1.0), true),
            (ctrl, Key::Num0, Command::ResetCodeSize, true),
            (Modifiers::ALT, Key::Z, Command::ToggleWrap, false),
            (Modifiers::ALT, Key::L, Command::ToggleAligned, false),
            (Modifiers::ALT, Key::D, Command::ToggleContext, false),
            (Modifiers::ALT, Key::B, Command::ToggleBase, false),
            (ctrl, Key::Z, Command::Undo, false),
            (ctrl, Key::Y, Command::Redo, false),
        ];
        for (modifiers, key, command, while_typing) in shortcuts {
            if (while_typing || !typing) && ctx.input_mut(|i| i.consume_key(modifiers, key)) {
                self.run(ctx, command);
            }
        }
        // Deleting entries of a folder comparison; in a text, Delete belongs to the editing.
        if !typing && matches!(self.tabs.get(self.active), Some(Tab::Folder(_))) {
            if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Delete)) {
                self.run(ctx, Command::FolderDelete { permanent: true });
            } else if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete)) {
                self.run(ctx, Command::FolderDelete { permanent: false });
            }
        }
        // One press, one block: a key held down must not send block after block.
        for (key, command) in [(Key::ArrowRight, Command::PushRight), (Key::ArrowLeft, Command::PushLeft)] {
            if typing {
                continue;
            }
            let first_press = ctx.input(|i| {
                i.events.iter().any(|event| {
                    matches!(event, egui::Event::Key { key: pressed_key, pressed: true, repeat: false, modifiers, .. }
                        if *pressed_key == key && modifiers.command)
                })
            });
            if ctx.input_mut(|i| i.consume_key(ctrl, key)) && first_press {
                self.run(ctx, command);
            }
        }
        // The window system delivers Ctrl+C as a copy event, not as a key press.
        if !typing && ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
            self.run(ctx, Command::Copy);
        }
    }

    fn main_menu(&self, ui: &mut Ui, chrome: &UiPalette, commands: &mut Vec<Command>) {
        menu_width(ui, MENU_WIDTH);
        let tab = self.tabs.get(self.active);
        let text = match tab {
            Some(Tab::Text(tab)) => Some(tab.as_ref()),
            _ => None,
        };
        let merge = match tab {
            Some(Tab::Merge(tab)) => Some(tab.as_ref()),
            _ => None,
        };
        let is_folder = matches!(tab, Some(Tab::Folder(_)));
        let has_blocks = text.is_some_and(|t| t.is_ready() && t.has_blocks()) || merge.is_some_and(|m| m.is_ready() && m.has_blocks());
        let can_undo = text.is_some_and(TextTab::can_undo) || merge.is_some_and(MergeTab::can_undo);
        let can_redo = text.is_some_and(TextTab::can_redo) || merge.is_some_and(MergeTab::can_redo);
        let dirty = tab.is_some_and(Tab::can_save);
        let searchable = text.is_some() || merge.is_some();
        let lines_ready = text.is_some_and(TextTab::is_ready) || merge.is_some_and(MergeTab::is_ready);
        let folder = match tab {
            Some(Tab::Folder(tab)) => Some(tab.as_ref()),
            _ => None,
        };
        let can_operate = folder.is_some_and(FolderTab::can_operate);
        let delete_keys = |side: Side| if folder.is_some_and(|f| f.delete_side() == side) { "Suppr" } else { "" };

        let go = |ui: &mut Ui, item: Item<'_>, command: Command, commands: &mut Vec<Command>| {
            if item.show(ui, chrome).clicked() {
                commands.push(command);
                ui.close();
            }
        };

        go(ui, Item::new("Palette de commandes…").icon(Icon::Command).keys("Ctrl+P"), Command::OpenPalette, commands);
        separator(ui, chrome);
        go(ui, Item::new("Nouvelle comparaison…").icon(Icon::Open).keys("Ctrl+O"), Command::OpenDialog, commands);
        go(ui, Item::new("Nouvelle fusion à 3 voies…").icon(Icon::GitMerge), Command::NewMerge, commands);
        if text.is_some_and(|tab| tab.conflicted_path().is_some()) {
            go(ui, Item::new("Résoudre les conflits en fusion à 3 voies").icon(Icon::GitMerge), Command::ResolveConflicts, commands);
        }
        go(ui, Item::new("Comparer avec HEAD").icon(Icon::GitBranch).enabled(self.head_candidate().is_some()), Command::CompareWithHead, commands);
        go(ui, Item::new("Enregistrer").icon(Icon::Save).keys("Ctrl+S").enabled(dirty), Command::Save, commands);
        go(ui, Item::new("Recharger").icon(Icon::Reload).keys("F5").enabled(tab.is_some()), Command::Reload, commands);
        separator(ui, chrome);
        go(ui, Item::new("Annuler").icon(Icon::Undo).keys("Ctrl+Z").enabled(can_undo), Command::Undo, commands);
        go(ui, Item::new("Rétablir").icon(Icon::Redo).keys("Ctrl+Y").enabled(can_redo), Command::Redo, commands);
        separator(ui, chrome);
        go(ui, Item::new("Rechercher…").icon(Icon::Magnifier).keys("Ctrl+F").enabled(searchable), Command::Find, commands);
        go(ui, Item::new("Aller à la ligne…").icon(Icon::Goto).keys("Ctrl+G").enabled(lines_ready), Command::GoToLine, commands);
        separator(ui, chrome);
        go(ui, Item::new("Différence suivante").icon(Icon::ArrowDown).keys("Ctrl+↓").enabled(has_blocks), Command::NextBlock, commands);
        go(ui, Item::new("Différence précédente").icon(Icon::ArrowUp).keys("Ctrl+↑").enabled(has_blocks), Command::PreviousBlock, commands);
        if merge.is_none() {
            go(ui, Item::new("Envoyer le bloc à droite").icon(Icon::PushRight).keys("Ctrl+→").enabled(has_blocks), Command::PushRight, commands);
            go(ui, Item::new("Envoyer le bloc à gauche").icon(Icon::ArrowLeft).keys("Ctrl+←").enabled(has_blocks), Command::PushLeft, commands);
        }
        separator(ui, chrome);

        let response = Item::new("Fusion").icon(Icon::GitMerge).submenu().show(ui, chrome);
        egui::containers::menu::SubMenu::new().config(submenu_config(chrome)).show(ui, &response, |ui| {
            menu_width(ui, SUBMENU_WIDTH + 60.0);
            let ready = merge.is_some_and(MergeTab::is_ready);
            let current = merge.is_some_and(|m| m.current_chunk_index().is_some());
            let conflict = merge.is_some_and(MergeTab::current_is_conflict);
            go(ui, Item::new("Nouvelle fusion à 3 voies…").icon(Icon::GitMerge), Command::NewMerge, commands);
            separator(ui, chrome);
            go(ui, Item::new("Conflit non résolu suivant").icon(Icon::ArrowDown).keys("F8").enabled(ready), Command::NextConflict, commands);
            go(ui, Item::new("Conflit non résolu précédent").icon(Icon::ArrowUp).keys("F7").enabled(ready), Command::PreviousConflict, commands);
            separator(ui, chrome);
            go(ui, Item::new("Prendre la gauche").icon(Icon::ArrowRight).keys("Ctrl+→").enabled(current), Command::Resolve(Resolution::Left), commands);
            go(ui, Item::new("Prendre la droite").icon(Icon::ArrowLeft).keys("Ctrl+←").enabled(current), Command::Resolve(Resolution::Right), commands);
            go(ui, Item::new("Prendre gauche puis droite").icon(Icon::Align).enabled(current), Command::Resolve(Resolution::LeftThenRight), commands);
            go(ui, Item::new("Prendre droite puis gauche").icon(Icon::Align).enabled(current), Command::Resolve(Resolution::RightThenLeft), commands);
            if merge.is_none_or(MergeTab::base_known) {
                go(ui, Item::new("Prendre la base").icon(Icon::History).enabled(current), Command::Resolve(Resolution::Base), commands);
            }
            go(ui, Item::new("Combiner").icon(Icon::Combine).enabled(merge.is_some_and(MergeTab::current_combinable)), Command::Resolve(Resolution::Merged), commands);
            go(ui, Item::new("Éditer avec marqueurs").icon(Icon::FileText).enabled(conflict), Command::EditWithMarkers, commands);
            go(ui, Item::new("Revenir à non résolu").icon(Icon::Undo).enabled(conflict), Command::Resolve(Resolution::Unresolved), commands);
            separator(ui, chrome);
            let base = merge.is_some_and(MergeTab::shows_base);
            go(ui, Item::new("Afficher l'ancêtre commun").icon(Icon::History).keys(if base { "" } else { "Alt+B" }).checked(base).enabled(ready), Command::ToggleBase, commands);
        });

        let response = Item::new("Comparaison").icon(Icon::Compare).submenu().show(ui, chrome);
        egui::containers::menu::SubMenu::new().config(submenu_config(chrome)).show(ui, &response, |ui| {
            menu_width(ui, SUBMENU_WIDTH);
            let whitespace = self.diff_options.whitespace;
            go(ui, Item::new("Espaces stricts").checked(whitespace == Whitespace::Exact), Command::SetWhitespace(Whitespace::Exact), commands);
            go(ui, Item::new("Ignorer les espaces de fin").checked(whitespace == Whitespace::IgnoreTrailing), Command::SetWhitespace(Whitespace::IgnoreTrailing), commands);
            go(ui, Item::new("Ignorer tous les espaces").checked(whitespace == Whitespace::IgnoreAll), Command::SetWhitespace(Whitespace::IgnoreAll), commands);
            separator(ui, chrome);
            go(ui, Item::new("Ignorer la casse").checked(self.diff_options.ignore_case), Command::ToggleCase, commands);
            separator(ui, chrome);
            go(ui, Item::new("Règles d'ignorance…").icon(Icon::Ignore), Command::OpenIgnoreRules, commands);
        });
        let response = Item::new("Dossiers").icon(Icon::Folder).submenu().show(ui, chrome);
        egui::containers::menu::SubMenu::new().config(submenu_config(chrome)).show(ui, &response, |ui| {
            menu_width(ui, SUBMENU_WIDTH + 60.0);
            go(ui, Item::new("Différences seulement").checked(self.differences_only), Command::ToggleDifferencesOnly, commands);
            go(ui, Item::new("Respecter .gitignore").checked(self.scan_options.respect_gitignore), Command::ToggleGitignore, commands);
            separator(ui, chrome);
            go(ui, Item::new("Tout déplier").enabled(is_folder), Command::SetAllExpanded(true), commands);
            go(ui, Item::new("Tout replier").enabled(is_folder), Command::SetAllExpanded(false), commands);
            go(ui, Item::new("Filtrer les noms…").icon(Icon::Funnel).keys("Ctrl+Maj+F").enabled(is_folder), Command::FocusFolderFilter, commands);
            separator(ui, chrome);
            go(ui, Item::new("Copier vers la droite").icon(Icon::CopyRight).keys("Ctrl+→").enabled(can_operate), Command::FolderCopy(Side::Left), commands);
            go(ui, Item::new("Copier vers la gauche").icon(Icon::ArrowLeft).keys("Ctrl+←").enabled(can_operate), Command::FolderCopy(Side::Right), commands);
            go(ui, Item::new("Supprimer à gauche (corbeille)").icon(Icon::Trash).keys(delete_keys(Side::Left)).danger().enabled(can_operate), Command::FolderDeleteOn(Side::Left), commands);
            go(ui, Item::new("Supprimer à droite (corbeille)").icon(Icon::Trash).keys(delete_keys(Side::Right)).danger().enabled(can_operate), Command::FolderDeleteOn(Side::Right), commands);
            separator(ui, chrome);
            let can_sync = folder.is_some_and(FolderTab::can_sync);
            for (operation, name, icon) in SYNC_CHOICES {
                go(ui, Item::new(name).icon(icon).enabled(can_sync), Command::FolderSync(operation), commands);
            }
        });
        let is_json = text.is_some_and(TextTab::is_json);
        let json_ready = is_json && text.is_some_and(TextTab::is_ready);
        let json = self.current_json_options();
        let structural = text.filter(|tab| tab.is_json()).map_or(self.json_structural, TextTab::is_structural);
        let response = Item::new("JSON").icon(Icon::Braces).submenu().show(ui, chrome);
        egui::containers::menu::SubMenu::new().config(submenu_config(chrome)).show(ui, &response, |ui| {
            menu_width(ui, SUBMENU_WIDTH + 40.0);
            go(ui, Item::new("Comparaison structurelle").checked(structural), Command::ToggleJsonStructural, commands);
            go(ui, Item::new("Changements JSON…").icon(Icon::List).enabled(json_ready), Command::JsonSummary, commands);
            go(ui, Item::new("Réaligner").icon(Icon::Align).enabled(json_ready), Command::Realign, commands);
            separator(ui, chrome);
            for (order, name) in JSON_KEY_ORDERS {
                go(ui, Item::new(name).checked(json.key_order == order), Command::SetJsonKeyOrder(order), commands);
            }
            separator(ui, chrome);
            go(ui, Item::new("Tableaux sans ordre").checked(json.array_order == ArrayOrder::Unordered), Command::ToggleJsonUnorderedArrays, commands);
            go(ui, Item::new("Nombres comparés par valeur").checked(json.numbers_by_value), Command::ToggleJsonNumbersByValue, commands);
            go(ui, Item::new("Chaînes comparées par valeur").checked(json.strings_by_value), Command::ToggleJsonStringsByValue, commands);
            separator(ui, chrome);
            go(ui, Item::new("Référence : gauche").checked(json.reference == Side::Left), Command::SetJsonReference(Side::Left), commands);
            go(ui, Item::new("Référence : droite").checked(json.reference == Side::Right), Command::SetJsonReference(Side::Right), commands);
        });
        let documents = self.extract_options;
        let typography = Typography::of(&documents.normalization);
        let response = Item::new("Documents").icon(Icon::FileText).submenu().show(ui, chrome);
        egui::containers::menu::SubMenu::new().config(submenu_config(chrome)).show(ui, &response, |ui| {
            menu_width(ui, SUBMENU_WIDTH + 40.0);
            go(ui, Item::new("Modifications suivies : texte final").checked(documents.revision == Revision::Final), Command::SetDocRevision(Revision::Final), commands);
            go(ui, Item::new("Modifications suivies : texte d'origine").checked(documents.revision == Revision::Original), Command::SetDocRevision(Revision::Original), commands);
            separator(ui, chrome);
            for (notes, name) in DOC_NOTES {
                go(ui, Item::new(name).checked(documents.notes == notes), Command::SetDocNotes(notes), commands);
            }
            separator(ui, chrome);
            go(ui, Item::new("Adresses des liens").checked(documents.link_targets), Command::ToggleDocLinks, commands);
            go(ui, Item::new("Repères d'images").checked(documents.images), Command::ToggleDocImages, commands);
            go(ui, Item::new("Commentaires").checked(documents.comments), Command::ToggleDocComments, commands);
            go(ui, Item::new("En-têtes et pieds de page").checked(documents.headers_footers), Command::ToggleDocHeadersFooters, commands);
            separator(ui, chrome);
            for (level, name) in DOC_TYPOGRAPHY {
                go(ui, Item::new(name).checked(typography == level), Command::SetDocTypography(level), commands);
            }
        });
        let response = Item::new("Apparence").icon(Icon::ColorScale).submenu().show(ui, chrome);
        egui::containers::menu::SubMenu::new().config(submenu_config(chrome)).show(ui, &response, |ui| {
            menu_width(ui, SUBMENU_WIDTH);
            for kind in ThemeKind::ALL {
                go(ui, Item::new(kind.name()).checked(self.theme == kind), Command::SetTheme(kind), commands);
            }
            separator(ui, chrome);
            for tone in CodeTone::ALL {
                go(ui, Item::new(tone.name()).checked(self.code_tone == tone), Command::SetCodeTone(tone), commands);
            }
            separator(ui, chrome);
            for links in [LinkStyle::Lines, LinkStyle::Ribbons] {
                go(ui, Item::new(links.name()).checked(self.links == links), Command::SetLinks(links), commands);
            }
            let verre = self.theme == ThemeKind::Verre;
            go(ui, Item::new("Fenêtre transparente").checked(verre && self.transparent).enabled(verre), Command::ToggleTransparency, commands);
            separator(ui, chrome);
            go(ui, Item::new("Police du texte…"), Command::OpenFontDialog, commands);
            let wrap = self.shown_wrap();
            go(ui, Item::new("Retour à la ligne").keys(if wrap { "" } else { "Alt+Z" }).checked(wrap), Command::ToggleWrap, commands);
            go(ui, Item::new("Aligner les lignes").keys(if self.aligned_lines { "" } else { "Alt+L" }).checked(self.aligned_lines), Command::ToggleAligned, commands);
            separator(ui, chrome);
            go(ui, Item::new("Différences seules").keys(if self.context_only { "" } else { "Alt+D" }).checked(self.context_only), Command::ToggleContext, commands);
            for (lines, name) in CONTEXT_CHOICES {
                go(ui, Item::new(name).checked(self.context_lines == lines), Command::SetContextLines(lines), commands);
            }
        });
        separator(ui, chrome);
        go(ui, Item::new("À propos de BioMerge").icon(Icon::Info), Command::About, commands);
        go(ui, Item::new("Quitter").icon(Icon::Quit).keys("Ctrl+Q"), Command::Quit, commands);
    }

    fn palette_entries(&self) -> Vec<Entry<Command>> {
        let tab = self.tabs.get(self.active);
        let text = match tab {
            Some(Tab::Text(tab)) => Some(tab.as_ref()),
            _ => None,
        };
        let merge = match tab {
            Some(Tab::Merge(tab)) => Some(tab.as_ref()),
            _ => None,
        };
        let is_folder = matches!(tab, Some(Tab::Folder(_)));
        let can_operate = matches!(tab, Some(Tab::Folder(f)) if f.can_operate());
        let has_blocks = text.is_some_and(|t| t.is_ready() && t.has_blocks()) || merge.is_some_and(|m| m.is_ready() && m.has_blocks());
        let searchable = text.is_some() || merge.is_some();
        let has_head = self.head_candidate().is_some();
        let mut entries = Vec::new();
        let mut add = |enabled: bool, name: &str, group, keys, icon, checked, command| {
            if enabled {
                entries.push(Entry { name: name.to_owned(), group, keys, icon, checked, command });
            }
        };
        let whitespace = self.diff_options.whitespace;
        add(true, "Nouvelle comparaison…", "Fichier", "Ctrl+O", Some(Icon::Open), false, Command::OpenDialog);
        add(true, "Nouvelle fusion à 3 voies…", "Fichier", "", Some(Icon::GitMerge), false, Command::NewMerge);
        let conflicted = text.is_some_and(|tab| tab.conflicted_path().is_some());
        add(conflicted, "Résoudre les conflits en fusion à 3 voies", "Fusion", "", Some(Icon::GitMerge), false, Command::ResolveConflicts);
        if let Some(merge) = merge.filter(|merge| merge.is_ready()) {
            let current = merge.current_chunk_index().is_some();
            let conflict = merge.current_is_conflict();
            add(true, "Conflit non résolu suivant", "Fusion", "F8", Some(Icon::ArrowDown), false, Command::NextConflict);
            add(true, "Conflit non résolu précédent", "Fusion", "F7", Some(Icon::ArrowUp), false, Command::PreviousConflict);
            add(current, "Prendre la gauche", "Fusion", "Ctrl+→", Some(Icon::ArrowRight), false, Command::Resolve(Resolution::Left));
            add(current, "Prendre la droite", "Fusion", "Ctrl+←", Some(Icon::ArrowLeft), false, Command::Resolve(Resolution::Right));
            add(current, "Prendre gauche puis droite", "Fusion", "", Some(Icon::GitMerge), false, Command::Resolve(Resolution::LeftThenRight));
            add(current, "Prendre droite puis gauche", "Fusion", "", Some(Icon::GitMerge), false, Command::Resolve(Resolution::RightThenLeft));
            add(current && merge.base_known(), "Prendre la base", "Fusion", "", Some(Icon::History), false, Command::Resolve(Resolution::Base));
            add(merge.current_combinable(), "Combiner les deux côtés", "Fusion", "", Some(Icon::Combine), false, Command::Resolve(Resolution::Merged));
            add(conflict, "Éditer avec marqueurs", "Fusion", "", Some(Icon::GitMerge), false, Command::EditWithMarkers);
            add(conflict, "Revenir à non résolu", "Fusion", "", Some(Icon::Undo), false, Command::Resolve(Resolution::Unresolved));
            add(true, "Afficher l'ancêtre commun", "Fusion", "Alt+B", Some(Icon::History), merge.shows_base(), Command::ToggleBase);
        }
        add(has_head, "Comparer avec HEAD", "Fichier", "", Some(Icon::GitBranch), false, Command::CompareWithHead);
        add(true, "Règles d'ignorance…", "Comparaison", "", Some(Icon::Ignore), !self.compiled_rules.is_empty(), Command::OpenIgnoreRules);
        add(is_folder, "Filtrer les noms…", "Dossiers", "Ctrl+Maj+F", Some(Icon::Funnel), false, Command::FocusFolderFilter);
        add(can_operate, "Copier vers la droite", "Dossiers", "Ctrl+→", Some(Icon::CopyRight), false, Command::FolderCopy(Side::Left));
        add(can_operate, "Copier vers la gauche", "Dossiers", "Ctrl+←", Some(Icon::ArrowLeft), false, Command::FolderCopy(Side::Right));
        add(can_operate, "Supprimer à gauche (corbeille)", "Dossiers", "", Some(Icon::Trash), false, Command::FolderDeleteOn(Side::Left));
        add(can_operate, "Supprimer à droite (corbeille)", "Dossiers", "", Some(Icon::Trash), false, Command::FolderDeleteOn(Side::Right));
        let can_sync = matches!(tab, Some(Tab::Folder(f)) if f.can_sync());
        for (operation, name, icon) in SYNC_CHOICES {
            add(can_sync, name, "Dossiers", "", Some(icon), false, Command::FolderSync(operation));
        }
        add(searchable, "Rechercher dans le texte…", "Aller", "Ctrl+F", Some(Icon::Magnifier), false, Command::Find);
        add(searchable, "Occurrence suivante", "Aller", "F3", Some(Icon::ArrowDown), false, Command::FindNext);
        add(searchable, "Occurrence précédente", "Aller", "Maj+F3", Some(Icon::ArrowUp), false, Command::FindPrevious);
        add(text.is_some(), "Filtrer les blocs…", "Aller", "Ctrl+Maj+F", Some(Icon::Funnel), false, Command::FocusFilter);
        add(text.is_some_and(TextTab::is_ready) || merge.is_some_and(MergeTab::is_ready), "Aller à la ligne…", "Aller", "Ctrl+G", Some(Icon::Goto), false, Command::GoToLine);
        add(tab.is_some_and(Tab::can_save), "Enregistrer", "Fichier", "Ctrl+S", Some(Icon::Save), false, Command::Save);
        add(tab.is_some(), "Recharger", "Fichier", "F5", Some(Icon::Reload), false, Command::Reload);
        add(tab.is_some(), "Fermer l'onglet", "Fichier", "Ctrl+W", Some(Icon::Cross), false, Command::CloseTab);
        add(has_blocks, "Différence suivante", "Aller", "Ctrl+↓", Some(Icon::ArrowDown), false, Command::NextBlock);
        add(has_blocks, "Différence précédente", "Aller", "Ctrl+↑", Some(Icon::ArrowUp), false, Command::PreviousBlock);
        add(has_blocks && merge.is_none(), "Envoyer le bloc à droite", "Édition", "Ctrl+→", Some(Icon::PushRight), false, Command::PushRight);
        add(has_blocks && merge.is_none(), "Envoyer le bloc à gauche", "Édition", "Ctrl+←", Some(Icon::ArrowLeft), false, Command::PushLeft);
        add(text.is_some_and(TextTab::can_undo) || merge.is_some_and(MergeTab::can_undo), "Annuler", "Édition", "Ctrl+Z", Some(Icon::Undo), false, Command::Undo);
        add(text.is_some_and(TextTab::can_redo) || merge.is_some_and(MergeTab::can_redo), "Rétablir", "Édition", "Ctrl+Y", Some(Icon::Redo), false, Command::Redo);
        add(true, "Espaces stricts", "Comparaison", "", Some(Icon::Whitespace), whitespace == Whitespace::Exact, Command::SetWhitespace(Whitespace::Exact));
        add(true, "Ignorer les espaces de fin", "Comparaison", "", Some(Icon::Whitespace), whitespace == Whitespace::IgnoreTrailing, Command::SetWhitespace(Whitespace::IgnoreTrailing));
        add(true, "Ignorer tous les espaces", "Comparaison", "", Some(Icon::Whitespace), whitespace == Whitespace::IgnoreAll, Command::SetWhitespace(Whitespace::IgnoreAll));
        add(true, "Ignorer la casse", "Comparaison", "", Some(Icon::Case), self.diff_options.ignore_case, Command::ToggleCase);
        add(true, "Différences seulement", "Dossiers", "", Some(Icon::Folder), self.differences_only, Command::ToggleDifferencesOnly);
        add(true, "Respecter .gitignore", "Dossiers", "", Some(Icon::Folder), self.scan_options.respect_gitignore, Command::ToggleGitignore);
        add(is_folder, "Tout déplier", "Dossiers", "", Some(Icon::Folder), false, Command::SetAllExpanded(true));
        add(is_folder, "Tout replier", "Dossiers", "", Some(Icon::Folder), false, Command::SetAllExpanded(false));
        for kind in ThemeKind::ALL {
            let name = format!("Thème : {}", kind.name());
            add(true, &name, "Apparence", "", Some(Icon::ColorScale), self.theme == kind, Command::SetTheme(kind));
        }
        for tone in CodeTone::ALL {
            let icon = if tone == CodeTone::Dark { Icon::Moon } else { Icon::Sun };
            add(true, tone.name(), "Apparence", "", Some(icon), self.code_tone == tone, Command::SetCodeTone(tone));
        }
        for links in [LinkStyle::Lines, LinkStyle::Ribbons] {
            add(true, links.name(), "Apparence", "", Some(Icon::Ribbon), self.links == links, Command::SetLinks(links));
        }
        let verre = self.theme == ThemeKind::Verre;
        add(verre, "Fenêtre transparente", "Apparence", "", Some(Icon::ColorScale), self.transparent, Command::ToggleTransparency);
        add(true, "Police du texte…", "Apparence", "", Some(Icon::Font), false, Command::OpenFontDialog);
        add(true, "Agrandir le texte", "Apparence", "Ctrl++", Some(Icon::Font), false, Command::ChangeCodeSize(1.0));
        add(true, "Réduire le texte", "Apparence", "Ctrl+-", Some(Icon::Font), false, Command::ChangeCodeSize(-1.0));
        add(true, "Taille de texte par défaut", "Apparence", "Ctrl+0", Some(Icon::Font), false, Command::ResetCodeSize);
        add(true, "Retour à la ligne", "Apparence", "Alt+Z", Some(Icon::Wrap), self.shown_wrap(), Command::ToggleWrap);
        add(true, "Aligner les lignes", "Apparence", "Alt+L", Some(Icon::Align), self.aligned_lines, Command::ToggleAligned);
        add(true, "Différences seules", "Apparence", "Alt+D", Some(Icon::Fold), self.context_only, Command::ToggleContext);
        for (lines, name) in CONTEXT_CHOICES {
            add(true, name, "Apparence", "", Some(Icon::Fold), self.context_lines == lines, Command::SetContextLines(lines));
        }
        let is_json = text.is_some_and(TextTab::is_json);
        let json_ready = is_json && text.is_some_and(TextTab::is_ready);
        let json = self.current_json_options();
        let structural = text.filter(|tab| tab.is_json()).map_or(self.json_structural, TextTab::is_structural);
        add(true, "Comparaison structurelle JSON", "JSON", "", Some(Icon::Braces), structural, Command::ToggleJsonStructural);
        add(json_ready, "Changements JSON…", "JSON", "", Some(Icon::List), false, Command::JsonSummary);
        add(json_ready, "Réaligner le JSON", "JSON", "", Some(Icon::Align), false, Command::Realign);
        for (order, name) in JSON_KEY_ORDERS {
            add(true, name, "JSON", "", Some(Icon::Braces), json.key_order == order, Command::SetJsonKeyOrder(order));
        }
        add(true, "Tableaux sans ordre", "JSON", "", Some(Icon::Braces), json.array_order == ArrayOrder::Unordered, Command::ToggleJsonUnorderedArrays);
        add(true, "Nombres comparés par valeur", "JSON", "", Some(Icon::Braces), json.numbers_by_value, Command::ToggleJsonNumbersByValue);
        add(true, "Chaînes comparées par valeur", "JSON", "", Some(Icon::Braces), json.strings_by_value, Command::ToggleJsonStringsByValue);
        add(true, "Référence JSON : gauche", "JSON", "", Some(Icon::Braces), json.reference == Side::Left, Command::SetJsonReference(Side::Left));
        add(true, "Référence JSON : droite", "JSON", "", Some(Icon::Braces), json.reference == Side::Right, Command::SetJsonReference(Side::Right));
        let documents = self.extract_options;
        let typography = Typography::of(&documents.normalization);
        add(true, "Modifications suivies : texte final", "Documents", "", Some(Icon::FileText), documents.revision == Revision::Final, Command::SetDocRevision(Revision::Final));
        add(true, "Modifications suivies : texte d'origine", "Documents", "", Some(Icon::FileText), documents.revision == Revision::Original, Command::SetDocRevision(Revision::Original));
        for (notes, name) in DOC_NOTES {
            add(true, name, "Documents", "", Some(Icon::FileText), documents.notes == notes, Command::SetDocNotes(notes));
        }
        add(true, "Adresses des liens", "Documents", "", Some(Icon::FileText), documents.link_targets, Command::ToggleDocLinks);
        add(true, "Repères d'images", "Documents", "", Some(Icon::FileText), documents.images, Command::ToggleDocImages);
        add(true, "Commentaires des documents", "Documents", "", Some(Icon::FileText), documents.comments, Command::ToggleDocComments);
        add(true, "En-têtes et pieds de page", "Documents", "", Some(Icon::FileText), documents.headers_footers, Command::ToggleDocHeadersFooters);
        for (level, name) in DOC_TYPOGRAPHY {
            add(true, name, "Documents", "", Some(Icon::FileText), typography == level, Command::SetDocTypography(level));
        }
        add(true, "À propos de BioMerge", "Aide", "", Some(Icon::Info), false, Command::About);
        add(true, "Quitter", "Fichier", "Ctrl+Q", Some(Icon::Quit), false, Command::Quit);
        for (index, tab) in self.tabs.iter().enumerate() {
            let name = format!("Onglet : {}", tab.title());
            add(index != self.active, &name, "Onglets", "", Some(Icon::File), false, Command::ActivateTab(index));
        }
        entries
    }

    fn band(&mut self, ui: &mut Ui, look: &Look, commands: &mut Vec<Command>) {
        let chrome = &look.ui;
        let style = chrome.chrome;
        let full = ui.max_rect();
        let padding = match style {
            Chrome::Verre | Chrome::Ardoise => 16.0,
            Chrome::Porcelaine => 22.0,
            Chrome::Graphite => 12.0,
            Chrome::Classic => BAND_PADDING,
        };
        let bar = full.shrink2(vec2(padding, 0.0));
        let rule = match style {
            Chrome::Classic => Some(chrome.border),
            Chrome::Ardoise => Some(Color32::from_rgb(0x1e, 0x22, 0x2a)),
            Chrome::Graphite => Some(Color32::from_rgb(0x22, 0x22, 0x24)),
            Chrome::Verre | Chrome::Porcelaine => None,
        };
        if let Some(rule) = rule {
            ui.painter().hline(full.x_range(), ui.painter().round_to_pixel_center(full.bottom() - 0.5), Stroke::new(1.0, rule));
        }
        let center_y = bar.center().y;
        let size = match style {
            Chrome::Verre | Chrome::Porcelaine => 38.0,
            Chrome::Ardoise => 34.0,
            Chrome::Graphite | Chrome::Classic => MENU_BUTTON,
        };
        let button = Rect::from_min_size(pos2(bar.left(), center_y - size / 2.0), vec2(size, size));
        let response = ui.interact(button, ui.scope_id().with("menu-button"), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, "Menu"));
        let open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&response));
        let active = response.hovered() || open;
        let painter = ui.painter();
        match style {
            Chrome::Verre => {
                let fill = if active { theme::alpha(0xffffff, 0.1) } else { theme::alpha(0xffffff, 0.05) };
                painter.rect(button, 11.0, fill, Stroke::new(1.0, theme::alpha(0xffffff, 0.08)), egui::StrokeKind::Inside);
            }
            Chrome::Porcelaine => {
                painter.add(egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(18) }.as_shape(button, 12));
                let fill = if active { chrome.hover } else { chrome.raised };
                painter.rect(button, 12.0, fill, Stroke::new(1.0, theme::alpha(0x000000, 0.05)), egui::StrokeKind::Inside);
            }
            _ if active => {
                painter.rect_filled(button, 8.0, chrome.hover);
            }
            _ => {}
        }
        let glyph = if active || matches!(style, Chrome::Verre | Chrome::Porcelaine) { chrome.text } else { chrome.text_dim };
        icons::paint(painter, Icon::Hamburger, icons::square(button.center(), 18.0), glyph);
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        Popup::menu(&response).frame(menu_frame(chrome)).show(|ui| {
            self.main_menu(ui, chrome, commands);
        });
        let gap = if style == Chrome::Porcelaine { 10.0 } else { BAND_GAP + 2.0 };
        let tabs_left = button.right() + gap;
        // Text tabs find and filter blocks there; folder tabs filter names.
        let has_field = self.tabs.get(self.active).is_some();
        let (field_width, field_height) = match style {
            Chrome::Verre => (340.0, 40.0),
            Chrome::Ardoise => (320.0, 34.0),
            Chrome::Porcelaine => (300.0, 38.0),
            Chrome::Graphite => (520.0, 32.0),
            Chrome::Classic => (FILTER_WIDTH, FILTER_HEIGHT),
        };
        let room = bar.right() - tabs_left;
        let field_width = if has_field { (room * 0.45).clamp(FILTER_MIN_WIDTH.min(room), field_width.max(380.0)) } else { 0.0 };
        // Graphite centres its field, the language and the encoding on its right.
        let field = if style == Chrome::Graphite {
            Rect::from_center_size(pos2(full.center().x, center_y), vec2(field_width.min(room * 0.5), field_height))
        } else {
            Rect::from_min_size(pos2(bar.right() - field_width, center_y - field_height / 2.0), vec2(field_width, field_height))
        };
        let tabs_right = if has_field { field.left() - BAND_GAP * 2.0 } else { bar.right() };
        self.tab_strip(ui, Rect::from_min_max(pos2(tabs_left, bar.top()), pos2(tabs_right, bar.bottom())), look);
        if style == Chrome::Graphite && has_field {
            let items = self.active_status_items();
            let words: Vec<String> = items.iter().filter_map(|item| match item {
                StatusItem::Pill(text) => Some(text.clone()),
                _ => None,
            }).collect();
            let text = words.join("  ·  ");
            let painter = ui.painter_at(Rect::from_min_max(pos2(field.right() + 16.0, bar.top()), bar.max));
            painter.text(pos2(bar.right(), center_y), Align2::RIGHT_CENTER, text, FontId::proportional(12.5), chrome.text_faint);
        }
        if has_field {
            if matches!(self.tabs.get(self.active), Some(Tab::Folder(_))) {
                self.folder_filter_group(ui, field, look);
            } else {
                self.search_group(ui, field, look);
            }
        }
    }

    /// The patterns that keep or leave out names in folder comparisons, in the frame of the
    /// text search: applied after a pause in typing, or at once with Enter.
    fn folder_filter_group(&mut self, ui: &mut Ui, group: Rect, look: &Look) {
        let chrome = &look.ui;
        let focus = std::mem::take(&mut self.focus_search);
        let id = ui.scope_id().with("folder-filter");
        let focused = ui.memory(|memory| memory.has_focus(id));
        let hovered = ui.rect_contains_pointer(group);
        field_frame(ui, look, group, focused, hovered, self.folder_filter_error.is_some());
        let center_y = group.center().y;
        let active = !self.scan_options.name_filter.is_empty();
        let funnel = icons::square(pos2(group.left() + 17.0, center_y), 15.0);
        icons::paint(ui.painter(), Icon::Funnel, funnel, if active { chrome.accent } else { chrome.text_dim });
        let _ = ui
            .interact(funnel.expand(4.0), ui.scope_id().with("folder-filter-help"), Sense::hover())
            .on_hover_text(FOLDER_FILTER_HELP);
        let mut text_right = group.right() - 8.0;
        let mut apply_now = false;
        if !self.folder_filter.is_empty() {
            let clear = icons::square(pos2(text_right - 9.0, center_y), 20.0);
            let clear_id = ui.scope_id().with("folder-filter-clear");
            if widgets::icon_button(ui, chrome, clear, clear_id, Icon::Cross, "Effacer le filtre des noms", chrome.text_dim).clicked() {
                self.folder_filter.clear();
                apply_now = true;
            }
            text_right = clear.left() - 2.0;
        }
        let inner = Rect::from_min_max(pos2(group.left() + 33.0, group.top()), pos2(text_right, group.bottom()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.set_clip_rect(inner.intersect(ui.clip_rect()));
        child.spacing_mut().interact_size.y = 0.0;
        let response = child.add(
            egui::TextEdit::singleline(&mut self.folder_filter)
                .id(id)
                .font(FontId::proportional(13.0))
                .hint_text("Filtrer les noms : *.rs -target/")
                .frame(Frame::NONE)
                .margin(Margin::ZERO)
                .desired_width(inner.width()),
        );
        ui.ctx().accesskit_node_builder(response.id, |builder| builder.set_label("Filtrer les noms"));
        if focus {
            response.request_focus();
        }
        if response.changed() {
            // The error shows as it is typed; the comparison waits for a pause.
            self.folder_filter_error = NameFilter::parse(&self.folder_filter).err().map(|e| e.to_string());
            self.folder_filter_edited = Some(ui.input(|i| i.time));
        }
        if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.folder_filter.clear();
            apply_now = true;
        }
        if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            apply_now = true;
        }
        if apply_now {
            self.apply_folder_filter(ui.ctx());
        }
        if let Some(error) = &self.folder_filter_error {
            let _ = response.on_hover_text(error.clone());
        }
    }

    /// Finding in the text and filtering the blocks share one field and its options in one frame,
    /// as csvfab's search: the lit funnel tells which of the two the field is doing.
    fn search_group(&mut self, ui: &mut Ui, group: Rect, look: &Look) {
        let chrome = &look.ui;
        let focus = std::mem::take(&mut self.focus_search);
        let tab: &mut dyn SearchTarget = match self.tabs.get_mut(self.active) {
            Some(Tab::Text(tab)) => tab.as_mut(),
            Some(Tab::Merge(tab)) => tab.as_mut(),
            _ => return,
        };
        let filtering = tab.search_mode() == SearchMode::Filter;
        let id = ui.scope_id().with("block-filter");
        let focused = ui.memory(|memory| memory.has_focus(id));
        let hovered = ui.rect_contains_pointer(group);
        let invalid = tab.filter_error().is_some();
        field_frame(ui, look, group, focused, hovered, invalid);
        let center_y = group.center().y;
        icons::paint(ui.painter(), Icon::Magnifier, icons::square(pos2(group.left() + 17.0, center_y), 15.0), chrome.text_dim);

        // A merge only finds: its field has no funnel.
        let can_filter = tab.can_filter();
        let toggle_count = if can_filter { 4.0 } else { 3.0 };
        let toggles_width = FILTER_TOGGLE * toggle_count + 3.0;
        let toggles_left = group.right() - toggles_width;
        let mut text_right = toggles_left - 4.0;
        if !tab.query().is_empty() {
            let clear = icons::square(pos2(text_right - 9.0, center_y), 20.0);
            let clear_id = ui.scope_id().with("filter-clear");
            let tip = if filtering { "Effacer le filtre" } else { "Effacer la recherche" };
            if widgets::icon_button(ui, chrome, clear, clear_id, Icon::Cross, tip, chrome.text_dim).clicked() {
                tab.query().clear();
                tab.apply_search();
            }
            text_right = clear.left() - 2.0;
        }
        // Blocks kept out of all of them, or the occurrence in use out of all of them.
        let counter = match (tab.filter_counts(), tab.find_counts()) {
            (Some((kept, total)), _) => Some((format!("{kept} / {total}"), kept == 0)),
            (None, Some((Some(current), total))) => Some((format!("{current} / {total}"), false)),
            (None, Some((None, total))) => Some((total.to_string(), total == 0)),
            (None, None) => None,
        };
        if let Some((counter, none)) = counter {
            let width = text_width(ui, &counter, widgets::medium(11.5));
            let color = if none { chrome.error } else { chrome.accent };
            ui.painter().text(
                pos2(text_right - 2.0, center_y),
                Align2::RIGHT_CENTER,
                counter,
                widgets::medium(11.5),
                color,
            );
            text_right -= width + 8.0;
        }
        let inner = Rect::from_min_max(pos2(group.left() + 33.0, group.top()), pos2(text_right, group.bottom()));
        let mut child = ui.new_child(
            egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        child.set_clip_rect(inner.intersect(ui.clip_rect()));
        child.spacing_mut().interact_size.y = 0.0;
        let response = child.add(
            egui::TextEdit::singleline(tab.query())
                .id(id)
                .font(FontId::proportional(13.0))
                .hint_text(if filtering { "Filtrer les blocs…" } else { "Rechercher…" })
                .frame(Frame::NONE)
                .margin(Margin::ZERO)
                .desired_width(inner.width()),
        );
        ui.ctx().accesskit_node_builder(response.id, |builder| {
            builder.set_label(if filtering { "Filtrer les blocs" } else { "Rechercher dans le texte" });
        });
        if focus {
            // What is already there is selected, ready to be typed over.
            response.request_focus();
            let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            let end = egui::text::CCursor::new(tab.query().chars().count());
            state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
            state.store(ui.ctx(), id);
        }
        if response.changed() {
            tab.apply_search();
        }
        if response.lost_focus() && ui.input(|input| input.key_pressed(Key::Escape)) {
            tab.query().clear();
            tab.apply_search();
        }
        if response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
            // Enter steps through the results and stays in the field. The key is taken away:
            // the text, which has the caret, would read it as a new line.
            let backwards = ui.input_mut(|input| {
                let shift = input.modifiers.shift;
                input.consume_key(Modifiers::SHIFT, Key::Enter);
                input.consume_key(Modifiers::NONE, Key::Enter);
                shift
            });
            tab.find_step(!backwards);
            response.request_focus();
        }
        if let Some(error) = tab.filter_error() {
            let error = error.to_owned();
            let _ = response.on_hover_text(error);
        }

        let options = *tab.options();
        let toggles = [
            (Icon::Funnel, filtering, "Filtrer les blocs (Ctrl+Maj+F)"),
            (Icon::AccentE, options.match_case, "Respecter les accents et la casse"),
            (Icon::Regex, options.regex, "Expression régulière"),
            (Icon::WholeWord, options.whole_words, "Mots entiers"),
        ];
        let skipped = if can_filter { 0 } else { 1 };
        for (index, (icon, on, tip)) in toggles.into_iter().enumerate().skip(skipped) {
            let rect = Rect::from_min_size(
                pos2(toggles_left + (index - skipped) as f32 * FILTER_TOGGLE, center_y - FILTER_TOGGLE / 2.0),
                vec2(FILTER_TOGGLE, FILTER_TOGGLE),
            );
            let response = ui.interact(rect, ui.scope_id().with(("filter-option", index)), Sense::click());
            response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, tip));
            let color = if on {
                chrome.accent
            } else if response.hovered() {
                chrome.text
            } else {
                chrome.text_dim
            };
            let glyph = icons::square(rect.center(), 18.0);
            icons::paint(ui.painter(), icon, glyph, color);
            if icon == Icon::AccentE {
                let mark = if on { chrome.error } else { chrome.text_dim.gamma_multiply(0.6) };
                icons::paint(ui.painter(), Icon::AccentMark, glyph, mark);
            }
            let response = response.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                let options = tab.options();
                match index {
                    0 => {}
                    1 => options.match_case = !options.match_case,
                    2 => options.regex = !options.regex,
                    _ => options.whole_words = !options.whole_words,
                }
                if index == 0 {
                    tab.set_search_mode(if filtering { SearchMode::Find } else { SearchMode::Filter });
                } else {
                    tab.apply_search();
                }
            }
        }
    }

    /// The kind of a tab: the icon and hue of its tile or square.
    fn tab_kind(&self, tab: &Tab, look: &Look) -> (Icon, Color32) {
        let hues = look.ui.hues;
        match tab {
            Tab::Text(tab) if tab.is_json() => (Icon::Braces, hues.amber),
            Tab::Text(tab) if tab.owns_wrap() => (Icon::FileText, hues.teal),
            Tab::Text(_) => (Icon::TextCompare, hues.violet),
            Tab::Folder(_) => (Icon::Folder, hues.blue),
            Tab::Merge(_) => (Icon::MergeTab, hues.pink),
        }
    }

    fn tab_strip(&mut self, ui: &mut Ui, strip: Rect, look: &Look) {
        let chrome = &look.ui;
        let style = chrome.chrome;
        let painter = ui.painter_at(strip);
        let center_y = strip.center().y;
        let titles: Vec<String> = self.tabs.iter().map(Tab::title).collect();
        let dirty: Vec<bool> = self.tabs.iter().map(Tab::is_dirty).collect();
        let kinds: Vec<(Icon, Color32)> = self.tabs.iter().map(|tab| self.tab_kind(tab, look)).collect();
        let (height, font, active_font, lead, gap) = match style {
            Chrome::Verre => (40.0, widgets::medium(13.5), widgets::bold(13.5), 40.0, 6.0),
            Chrome::Ardoise => (strip.height(), widgets::medium(13.5), widgets::semibold(13.5), 31.0, 4.0),
            Chrome::Porcelaine => (32.0, widgets::medium(13.0), widgets::semibold(13.0), 14.0, 4.0),
            Chrome::Graphite => (30.0, widgets::medium(13.0), widgets::medium(13.0), 12.0, 2.0),
            Chrome::Classic => (TAB_HEIGHT, widgets::medium(12.5), widgets::medium(12.5), 12.0, TAB_GAP),
        };
        // Tabs fit their title, then shrink together when they run out of room.
        let natural: Vec<f32> = titles
            .iter()
            .enumerate()
            .map(|(index, title)| {
                let font = if index == self.active { active_font.clone() } else { font.clone() };
                let text = ui.fonts_mut(|f| f.layout_no_wrap(title.clone(), font, chrome.text));
                (text.size().x + lead + 8.0 + 16.0 + 8.0 + 14.0).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH + lead)
            })
            .collect();
        let inset = if style == Chrome::Porcelaine { 4.0 } else { 0.0 };
        let gaps = gap * titles.len().saturating_sub(1) as f32 + inset * 2.0;
        let wanted: f32 = natural.iter().sum();
        let scale = ((strip.width() - gaps) / wanted.max(1.0)).min(1.0);
        if style == Chrome::Porcelaine && !titles.is_empty() {
            // The segmented control holding the tabs.
            let width = natural.iter().map(|w| (w * scale).floor()).sum::<f32>() + gaps;
            let tray = Rect::from_min_size(pos2(strip.left(), center_y - height / 2.0 - inset), vec2(width, height + inset * 2.0));
            painter.rect_filled(tray, 14.0, Color32::from_rgb(0xe6, 0xe6, 0xe0));
        }
        let mut close = None;
        let mut x = strip.left() + inset;
        for (index, title) in titles.iter().enumerate() {
            let width = (natural[index] * scale).floor();
            let rect = Rect::from_min_size(pos2(x, center_y - height / 2.0), vec2(width, height));
            x += width + gap;
            let response = ui.interact(rect, ui.scope_id().with(("tab", index)), Sense::click());
            response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, title));
            let active = index == self.active;
            let hovered = response.hovered();
            let (icon, hue) = kinds[index];
            let [r, g, b, _] = hue.to_array();
            let tinted = |opacity: u8| Color32::from_rgba_unmultiplied(r, g, b, opacity);
            match style {
                Chrome::Verre => {
                    if active {
                        painter.rect(rect, 12.0, tinted(51), Stroke::new(1.0, tinted(89)), egui::StrokeKind::Inside);
                    } else if hovered {
                        painter.rect_filled(rect, 12.0, theme::alpha(0xffffff, 0.05));
                    }
                    let tile = Rect::from_min_size(pos2(rect.left() + 6.0, center_y - 14.0), vec2(28.0, 28.0));
                    widgets::paint_tile(&painter, tile, hue, icon, true);
                }
                Chrome::Ardoise => {
                    if active {
                        let line = Rect::from_min_max(pos2(rect.left(), strip.bottom() - 2.0), pos2(rect.right(), strip.bottom()));
                        painter.rect_filled(line, 0.0, chrome.accent);
                    } else if hovered {
                        painter.rect_filled(rect.shrink2(vec2(0.0, 9.0)), 8.0, chrome.hover);
                    }
                    painter.rect_filled(Rect::from_center_size(pos2(rect.left() + 18.0, center_y), vec2(8.0, 8.0)), 2.0, hue);
                }
                Chrome::Porcelaine => {
                    if active {
                        painter.add(egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(20) }.as_shape(rect, 10));
                        painter.rect_filled(rect, 10.0, chrome.raised);
                    } else if hovered {
                        painter.rect_filled(rect, 10.0, theme::alpha(0xffffff, 0.5));
                    }
                }
                Chrome::Graphite => {
                    if active {
                        painter.rect(rect, 7.0, chrome.raised, Stroke::new(1.0, chrome.border), egui::StrokeKind::Inside);
                    } else if hovered {
                        painter.rect_filled(rect, 7.0, chrome.hover);
                    }
                }
                Chrome::Classic => {
                    if active {
                        painter.rect_filled(rect, 7.0, chrome.raised);
                    } else if hovered {
                        painter.rect_filled(rect, 7.0, chrome.hover);
                    }
                }
            }
            let color = if active { chrome.text } else { chrome.text_dim };
            let close_rect = icons::square(pos2(rect.right() - 16.0, center_y), 18.0);
            let mut text_x = rect.left() + lead;
            let title_font = if active { active_font.clone() } else { font.clone() };
            let shown = painter.with_clip_rect(rect.with_max_x(close_rect.left() - 2.0));
            let title_rect = shown.text(pos2(text_x, center_y), Align2::LEFT_CENTER, title, title_font, color);
            text_x = title_rect.right() + 8.0;
            if dirty[index] {
                let dot = pos2((text_x + 3.0).min(close_rect.left() - 4.0), center_y);
                let warning = if style == Chrome::Graphite { chrome.text } else { chrome.warning };
                if style == Chrome::Verre {
                    let [r, g, b, _] = warning.to_array();
                    painter.add(egui::Shadow { offset: [0, 0], blur: 8, spread: 0, color: Color32::from_rgba_unmultiplied(r, g, b, 150) }.as_shape(Rect::from_center_size(dot, vec2(7.0, 7.0)), 4));
                }
                painter.circle_filled(dot, if style == Chrome::Verre { 3.5 } else { 3.0 }, warning);
            }
            let close_response = ui.interact(close_rect, ui.scope_id().with(("tab-close", index)), Sense::click());
            close_response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, "Fermer l'onglet"));
            if close_response.hovered() {
                painter.rect_filled(close_rect, 4.0, chrome.hover);
            }
            if hovered || close_response.hovered() || active {
                icons::paint(&painter, Icon::Cross, icons::square(close_rect.center(), 12.0), chrome.text_dim);
            }
            if close_response.clicked() || response.middle_clicked() {
                close = Some(index);
            } else if response.clicked() {
                self.active = index;
            }
        }
        if let Some(index) = close {
            self.request_close(index);
        }
    }

    fn active_status_items(&self) -> Vec<StatusItem> {
        match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) => tab.status_items(),
            Some(Tab::Folder(tab)) => tab.status_items(),
            Some(Tab::Merge(tab)) => tab.status_items(),
            None => Vec::new(),
        }
    }

    /// The block being worked on and how many there are, for the dock and the status bar.
    fn block_rank(&self) -> Option<(usize, usize)> {
        match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) => tab.block_rank(),
            Some(Tab::Merge(tab)) => tab.block_rank(),
            _ => None,
        }
    }

    /// The message of the moment, if any, for the left of the status bar.
    fn status_message(&mut self, ui: &mut Ui, chrome: &UiPalette, commands: &mut Vec<Command>) {
        let tab_message = match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) => tab.message.clone(),
            Some(Tab::Merge(tab)) => tab.message.clone(),
            _ => None,
        };
        let conflicted = matches!(self.tabs.get(self.active), Some(Tab::Text(tab)) if tab.conflicted_path().is_some());
        if let Some(message) = self.message.as_ref().or(tab_message.as_ref()) {
            ui.label(RichText::new(message).color(chrome.error).size(12.0));
        } else if conflicted {
            // A file git left in conflict is better resolved than compared.
            let link = ui
                .add(egui::Label::new(RichText::new("Ce fichier contient des conflits : résoudre en fusion à 3 voies").color(chrome.accent).size(12.0)).sense(Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if link.clicked() {
                commands.push(Command::ResolveConflicts);
            }
        } else if let Some((notice, at)) = &self.notice {
            let age = ui.input(|i| i.time) - at;
            if age < NOTICE_SECONDS {
                ui.label(RichText::new(notice).color(chrome.text_dim).size(12.0));
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(NOTICE_SECONDS - age));
            }
        }
    }

    fn status_bar(&mut self, ui: &mut Ui, look: &Look, commands: &mut Vec<Command>) {
        let chrome = &look.ui;
        let bar = ui.max_rect();
        match chrome.chrome {
            Chrome::Classic => {
                let mut gradient = egui::Mesh::default();
                gradient.colored_vertex(bar.left_top(), chrome.status_top);
                gradient.colored_vertex(bar.right_top(), chrome.status_top);
                gradient.colored_vertex(bar.right_bottom(), chrome.status_bottom);
                gradient.colored_vertex(bar.left_bottom(), chrome.status_bottom);
                gradient.add_triangle(0, 1, 2);
                gradient.add_triangle(0, 2, 3);
                ui.painter().add(egui::Shape::mesh(gradient));
                ui.painter().hline(bar.x_range(), ui.painter().round_to_pixel_center(bar.top() + 0.5), Stroke::new(1.0, chrome.border));
            }
            Chrome::Graphite => {
                ui.painter().hline(bar.x_range(), ui.painter().round_to_pixel_center(bar.top() + 0.5), Stroke::new(1.0, Color32::from_rgb(0x22, 0x22, 0x24)));
            }
            Chrome::Porcelaine => return self.dock(ui, look, commands),
            Chrome::Verre | Chrome::Ardoise => {}
        }
        let mut items = self.active_status_items();
        if chrome.chrome == Chrome::Verre {
            // One chip for the language, the encoding and the line ends, as in the mockup.
            let mut merged: Vec<StatusItem> = Vec::new();
            for item in items {
                match (merged.last_mut(), item) {
                    (Some(StatusItem::Pill(previous)), StatusItem::Pill(text)) => {
                        previous.push_str(" · ");
                        previous.push_str(&text);
                    }
                    (_, item) => merged.push(item),
                }
            }
            items = merged;
        }
        let mut copied = None;
        let padding = match chrome.chrome {
            Chrome::Verre | Chrome::Ardoise => 16.0,
            Chrome::Graphite => 12.0,
            _ => STATUS_PADDING,
        };
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add_space(padding - 4.0);
            self.option_toggles(ui, look, commands);
            ui.add_space(if chrome.chrome == Chrome::Graphite { 12.0 } else { 8.0 });
            if chrome.chrome == Chrome::Ardoise {
                code_tone_button(ui, look, commands);
                ui.add_space(4.0);
            }
            size_control(ui, look, self.code_size, commands);
            ui.add_space(12.0);
            self.status_message(ui, chrome, commands);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add_space(padding - 8.0);
                // From the right, most useful first: what no longer fits is left out.
                let separated = chrome.chrome == Chrome::Ardoise;
                let mut first = true;
                for item in items.iter().rev() {
                    if chrome.chrome == Chrome::Graphite && matches!(item, StatusItem::Pill(_)) {
                        // In the band, beside the field.
                        continue;
                    }
                    let needed = match item {
                        StatusItem::Text(text) => text_width(ui, text, FontId::proportional(12.5)),
                        StatusItem::Path(text) => text_width(ui, &elide_path(text), FontId::monospace(11.5)),
                        StatusItem::Pill(text) => text_width(ui, text, widgets::medium(11.5)) + PILL_PADDING,
                        StatusItem::Counts { .. } => 120.0,
                        StatusItem::Conflicts { unresolved, .. } => text_width(ui, &conflicts_text(*unresolved), widgets::medium(11.5)) + PILL_PADDING,
                    };
                    if needed + 8.0 > ui.available_width() {
                        break;
                    }
                    if separated && !std::mem::take(&mut first) {
                        ui.label(RichText::new("·").color(Color32::from_rgb(0x5a, 0x61, 0x6e)).size(12.5));
                    }
                    match item {
                        StatusItem::Text(text) => {
                            ui.label(RichText::new(text).color(chrome.text_dim).size(12.5));
                        }
                        StatusItem::Pill(text) => pill(ui, look, text),
                        StatusItem::Path(path) => {
                            let response = ui
                                .add(
                                    egui::Label::new(RichText::new(elide_path(path)).font(FontId::monospace(11.5)).color(chrome.accent))
                                        .sense(Sense::click()),
                                )
                                .on_hover_text(format!("{path}\nCliquer pour copier le chemin JSON"))
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if response.clicked() {
                                ui.ctx().copy_text(path.clone());
                                copied = Some(path.clone());
                            }
                        }
                        StatusItem::Counts { deletions, insertions, changes } => {
                            counts(ui, look, [*deletions, *insertions, *changes]);
                        }
                        StatusItem::Conflicts { unresolved, total } => conflicts_pill(ui, look, *unresolved, *total),
                    }
                    if !separated {
                        ui.add_space(6.0);
                    }
                }
                if chrome.chrome == Chrome::Graphite {
                    let hints: &[(&str, &str)] = match self.tabs.get(self.active) {
                        Some(Tab::Merge(_)) => &[("F8", "conflit suivant"), ("Ctrl →", "prendre la gauche")],
                        Some(Tab::Folder(_)) => &[("Ctrl →", "copier à droite")],
                        Some(Tab::Text(_)) => &[("Ctrl ↓", "bloc suivant"), ("Ctrl →", "envoyer")],
                        None => &[],
                    };
                    ui.label(RichText::new("|").color(Color32::from_rgb(0x3a, 0x3a, 0x3d)).size(12.0));
                    for (keys, what) in hints.iter().rev() {
                        if ui.available_width() < 160.0 {
                            break;
                        }
                        ui.label(RichText::new(*what).color(chrome.text_dim).size(12.0));
                        ui.label(RichText::new(*keys).font(FontId::monospace(11.5)).color(chrome.text));
                        ui.add_space(6.0);
                    }
                }
            });
        });
        if let Some(path) = copied {
            self.notice = Some((format!("Copié : {path}"), ui.input(|i| i.time)));
        }
    }

    /// Porcelaine's floating dock: block navigation, counts, the main options and the code size,
    /// in one pill at the bottom centre. What the status bar says otherwise sits in the corners.
    fn dock(&mut self, ui: &mut Ui, look: &Look, commands: &mut Vec<Command>) {
        let chrome = &look.ui;
        let bar = ui.max_rect();
        let items = self.active_status_items();
        let toggles = self.toggle_list();
        const BUTTON: f32 = 36.0;
        let rank = self.block_rank().map(|(current, total)| format!("{current} sur {total}"));
        let rank_width = rank.as_ref().map_or(0.0, |rank| text_width(ui, rank, widgets::semibold(12.5)).max(48.0) + 16.0);
        let counts_values = items.iter().find_map(|item| match item {
            StatusItem::Counts { deletions, insertions, changes } => Some([*changes, *insertions, *deletions]),
            _ => None,
        });
        let counts_width = counts_values.map_or(0.0, |values| {
            values.iter().map(|value| text_width(ui, &value.to_string(), widgets::semibold(12.5)) + 27.0).sum::<f32>()
        });
        let size_text = format!("{}", self.code_size.round());
        let separator = 9.0;
        let has_blocks = rank.is_some() || matches!(self.tabs.get(self.active), Some(Tab::Text(_) | Tab::Merge(_)));
        let mut width = 16.0;
        if has_blocks {
            width += BUTTON * 2.0 + rank_width + separator;
        }
        if counts_values.is_some() {
            width += counts_width + separator;
        }
        width += toggles.len() as f32 * (BUTTON + 2.0) + separator;
        width += BUTTON * 2.0 + 30.0;
        let dock = Rect::from_center_size(pos2(bar.center().x, bar.bottom() - 22.0 - 26.0), vec2(width, 52.0));
        let painter = ui.painter();
        painter.add(egui::Shadow { offset: [0, 12], blur: 36, spread: 0, color: Color32::from_black_alpha(36) }.as_shape(dock, 26));
        painter.rect(dock, 26.0, theme::alpha(0xffffff, 0.94), Stroke::new(1.0, theme::alpha(0x000000, 0.06)), egui::StrokeKind::Inside);
        let y = dock.center().y;
        let mut x = dock.left() + 8.0;
        let rule = |painter: &egui::Painter, x: &mut f32| {
            painter.vline(*x + separator / 2.0, y - 12.0..=y + 12.0, Stroke::new(1.0, Color32::from_rgb(0xe3, 0xe3, 0xdd)));
            *x += separator;
        };
        let round_button = |ui: &mut Ui, x: &mut f32, id: &str, icon: Option<Icon>, text: Option<(&str, f32)>, lit: bool, tip: &str| -> bool {
            let rect = Rect::from_min_size(pos2(*x, y - BUTTON / 2.0), vec2(BUTTON, BUTTON));
            *x += BUTTON + 2.0;
            let response = ui.interact(rect, ui.scope_id().with(("dock", id)), Sense::click());
            response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, tip));
            let hovered = response.hovered();
            let (fill, color) = if lit { (chrome.lit, chrome.lit_text) } else if hovered { (chrome.hover, chrome.text) } else { (Color32::TRANSPARENT, chrome.text_dim) };
            ui.painter().rect_filled(rect, BUTTON / 2.0, fill);
            if let Some(icon) = icon {
                icons::paint(ui.painter(), icon, icons::square(rect.center(), 16.0), color);
            }
            if let Some((text, size)) = text {
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, widgets::bold(size), chrome.text);
            }
            response.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
        };
        if has_blocks {
            if round_button(ui, &mut x, "previous", Some(Icon::ChevronUp), None, false, "Différence précédente (Ctrl+↑)") {
                commands.push(Command::PreviousBlock);
            }
            if let Some(rank) = &rank {
                ui.painter().text(pos2(x + rank_width / 2.0 - 1.0, y), Align2::CENTER_CENTER, rank, widgets::semibold(12.5), chrome.text);
            }
            x += rank_width;
            if round_button(ui, &mut x, "next", Some(Icon::ChevronDown), None, false, "Différence suivante (Ctrl+↓)") {
                commands.push(Command::NextBlock);
            }
            rule(ui.painter(), &mut x);
        }
        if let Some(values) = counts_values {
            let colors = [look.gap.change.mark, look.gap.insert.mark, look.gap.delete.mark];
            let words = ["modifications", "ajouts", "retraits"];
            for index in 0..3 {
                let text = values[index].to_string();
                let start = x + 6.0;
                ui.painter().circle_filled(pos2(start + 4.5, y), 4.5, colors[index]);
                let label = ui.painter().text(pos2(start + 15.0, y), Align2::LEFT_CENTER, &text, widgets::semibold(12.5), chrome.text);
                let hover = ui.interact(Rect::from_min_max(pos2(start, y - 10.0), pos2(label.right(), y + 10.0)), ui.scope_id().with(("dock-count", index)), Sense::hover());
                hover.on_hover_text(format!("{text} {}", words[index]));
                x = label.right() + 6.0;
            }
            x = x.max(dock.left());
            rule(ui.painter(), &mut x);
        }
        for toggle in &toggles {
            if round_button(ui, &mut x, toggle.tip, Some(toggle.icon), None, toggle.lit, toggle.tip) {
                commands.push(toggle.command);
            }
        }
        rule(ui.painter(), &mut x);
        if round_button(ui, &mut x, "smaller", None, Some(("A−", 12.0)), false, "Réduire le texte") {
            commands.push(Command::ChangeCodeSize(-1.0));
        }
        let size_rect = Rect::from_min_size(pos2(x - 2.0, y - 12.0), vec2(28.0, 24.0));
        ui.painter().text(size_rect.center(), Align2::CENTER_CENTER, &size_text, widgets::semibold(12.5), chrome.text);
        let size_response = ui.interact(size_rect, ui.scope_id().with("dock-size"), Sense::click());
        size_response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, "Taille du code"));
        if size_response.on_hover_text("Taille du code : un clic revient à la taille par défaut").clicked() {
            commands.push(Command::ResetCodeSize);
        }
        x += 28.0;
        if round_button(ui, &mut x, "larger", None, Some(("A+", 15.0)), false, "Agrandir le texte") {
            commands.push(Command::ChangeCodeSize(1.0));
        }
        // The corners: the message of the moment on the left, the facts of the file on the right.
        let left = Rect::from_min_max(pos2(bar.left() + 26.0, dock.top()), pos2(dock.left() - 16.0, dock.bottom()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.set_clip_rect(left);
        self.status_message(&mut child, chrome, commands);
        let right = Rect::from_min_max(pos2(dock.right() + 16.0, dock.top()), pos2(bar.right() - 26.0, dock.bottom()));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::right_to_left(egui::Align::Center)));
        child.set_clip_rect(right);
        for item in &items {
            if let StatusItem::Conflicts { unresolved, total } = item {
                conflicts_pill(&mut child, look, *unresolved, *total);
            }
        }
        // The facts of the file in their order, as many as fit.
        let font = FontId::proportional(12.0);
        let room = child.available_width();
        let mut line = String::new();
        for item in &items {
            if let StatusItem::Text(text) | StatusItem::Pill(text) = item {
                let longer = if line.is_empty() { text.clone() } else { format!("{line} · {text}") };
                if text_width(&child, &longer, font.clone()) > room {
                    break;
                }
                line = longer;
            }
        }
        child.label(RichText::new(line).font(font).color(chrome.text_faint));
    }

    /// The comparison options shown lit when active: in the status bar or Porcelaine's dock.
    fn toggle_list(&self) -> Vec<Toggle> {
        let whitespace = self.diff_options.whitespace;
        let (next, tip) = match whitespace {
            Whitespace::Exact => (Whitespace::IgnoreTrailing, "Espaces : tous comparés"),
            Whitespace::IgnoreTrailing => (Whitespace::IgnoreAll, "Espaces : fins de ligne ignorées"),
            Whitespace::IgnoreAll => (Whitespace::Exact, "Espaces : tous ignorés"),
        };
        let mut toggles = vec![
            Toggle { icon: Icon::Whitespace, short: "Espaces", lit: whitespace != Whitespace::Exact, tip, command: Command::SetWhitespace(next) },
            Toggle { icon: Icon::Case, short: "Casse", lit: self.diff_options.ignore_case, tip: "Ignorer la casse", command: Command::ToggleCase },
            Toggle { icon: Icon::Ignore, short: "Règles", lit: !self.compiled_rules.is_empty(), tip: "Règles d'ignorance…", command: Command::OpenIgnoreRules },
            Toggle { icon: Icon::Wrap, short: "Retour ligne", lit: self.shown_wrap(), tip: "Retour à la ligne (Alt+Z)", command: Command::ToggleWrap },
            Toggle { icon: Icon::Align, short: "Aligner", lit: self.aligned_lines, tip: "Aligner les lignes (Alt+L)", command: Command::ToggleAligned },
            Toggle { icon: Icon::Fold, short: "Différences seules", lit: self.context_only, tip: "Différences seules (Alt+D)", command: Command::ToggleContext },
            Toggle { icon: Icon::Funnel, short: "Dossiers : différences", lit: self.differences_only, tip: "Dossiers : différences seulement", command: Command::ToggleDifferencesOnly },
            Toggle { icon: Icon::EyeOff, short: ".gitignore", lit: self.scan_options.respect_gitignore, tip: "Dossiers : respecter .gitignore", command: Command::ToggleGitignore },
        ];
        // Only where it means something: a JSON tab, a merge.
        if let Some(Tab::Text(tab)) = self.tabs.get(self.active).filter(|tab| matches!(tab, Tab::Text(t) if t.is_json())) {
            toggles.push(Toggle { icon: Icon::Braces, short: "JSON", lit: tab.is_structural(), tip: "Comparaison structurelle JSON", command: Command::ToggleJsonStructural });
        }
        if let Some(Tab::Merge(tab)) = self.tabs.get(self.active) {
            toggles.push(Toggle { icon: Icon::History, short: "Ancêtre", lit: tab.shows_base(), tip: "Afficher l'ancêtre commun (Alt+B)", command: Command::ToggleBase });
        }
        toggles
    }

    /// The main comparison options, lit when active.
    fn option_toggles(&self, ui: &mut Ui, look: &Look, commands: &mut Vec<Command>) {
        let toggles = self.toggle_list();
        let mut show = |ui: &mut Ui| {
            for toggle in &toggles {
                if option_toggle(ui, look, toggle).clicked() {
                    commands.push(toggle.command);
                }
            }
        };
        if look.ui.chrome == Chrome::Verre {
            glass_tray(ui, look, show);
        } else {
            show(ui);
        }
    }

    /// The wrapping the toggle shows: the active document tab's own, or the setting.
    fn shown_wrap(&self) -> bool {
        match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) if tab.owns_wrap() => tab.wraps(),
            _ => self.wrap_lines,
        }
    }

    fn welcome(&mut self, ui: &mut Ui, chrome: &UiPalette, commands: &mut Vec<Command>) {
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, chrome.background);
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(rect.height() * 0.34);
                ui.label(RichText::new("BioMerge").font(widgets::semibold(26.0)).color(chrome.text));
                ui.add_space(4.0);
                ui.label(RichText::new("Compare des fichiers et des dossiers").color(chrome.text_dim));
                ui.add_space(20.0);
                if ui.add_sized([210.0, 34.0], primary_button(chrome, "Nouvelle comparaison")).clicked() {
                    commands.push(Command::OpenDialog);
                }
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context, chrome: &UiPalette, commands: &mut Vec<Command>) {
        while let Ok((side, path)) = self.picker.1.try_recv() {
            if let Some(dialog) = &mut self.open_dialog {
                dialog.paths[side] = path.display().to_string();
            }
        }
        while let Ok((slot, path)) = self.merge_picker.1.try_recv() {
            if let Some(dialog) = &mut self.merge_dialog {
                dialog.paths[slot] = path.display().to_string();
            }
        }
        if self.merge_dialog.is_some() {
            self.show_merge_dialog(ctx, chrome);
        }
        if self.palette.is_some() {
            self.show_palette(ctx, chrome, commands);
        }
        if self.go_to_line.is_some() {
            self.show_go_to_line(ctx, chrome);
        }
        if self.open_dialog.is_some() {
            self.show_open_dialog(ctx, chrome);
        }
        if self.font_dialog.is_some() {
            self.show_font_dialog(ctx, chrome);
        }
        if let Some(action) = self.confirm {
            self.show_confirm(ctx, chrome, action);
        }
        if self.rules_editor {
            let (changed, close) = rules_editor::show(ctx, chrome, &mut self.ignore_rules);
            if changed {
                self.rules_changed();
            }
            if close {
                self.rules_editor = false;
            }
        }
        if let Some(Tab::Folder(tab)) = self.tabs.get_mut(self.active) {
            if tab.wants_dialog() {
                tab.show_dialog(ctx, chrome);
            }
        }
        if self.about {
            let modal = egui::Modal::new(egui::Id::unique("about")).frame(dialog_frame(chrome)).show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(RichText::new("BioMerge").font(widgets::semibold(18.0)));
                ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(chrome.text_dim));
                ui.add_space(10.0);
                ui.label("Comparaison de fichiers et de dossiers, écrite en Rust.");
            });
            if modal.should_close() {
                self.about = false;
            }
        }
    }

    fn show_palette(&mut self, ctx: &egui::Context, chrome: &UiPalette, commands: &mut Vec<Command>) {
        let entries = self.palette_entries();
        let Some(palette) = &mut self.palette else {
            return;
        };
        let id = egui::Id::unique("command-palette");
        let area = egui::Modal::default_area(id).anchor(egui::Align2::CENTER_TOP, vec2(0.0, 90.0));
        let mut outcome = (None, false);
        let modal = egui::Modal::new(id)
            .area(area)
            .frame(menu_frame(chrome).inner_margin(Margin::ZERO))
            .show(ctx, |ui| outcome = palette.show(ui, chrome, &entries));
        let (pick, close) = outcome;
        if let Some(index) = pick {
            commands.push(entries[index].command);
        }
        if pick.is_some() || close || modal.should_close() {
            self.palette = None;
        }
    }

    /// A line number to type, in the place and the look of the palette.
    fn show_go_to_line(&mut self, ctx: &egui::Context, chrome: &UiPalette) {
        // The panes of the tab: their buttons, their letters, their line counts and the one in use.
        type Panes = (&'static [(&'static str, &'static str)], &'static [char], Vec<usize>, usize);
        let panes: Option<Panes> = match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) => tab.line_counts().map(|counts| {
                let buttons: &[(&str, &str)] = &[("Gauche", "Côté gauche"), ("Droite", "Côté droit")];
                (buttons, &['g', 'd'][..], counts.to_vec(), tab.caret_side().unwrap_or(Side::Left).index())
            }),
            Some(Tab::Merge(tab)) => tab.line_counts().map(|counts| {
                let buttons: &[(&str, &str)] = &[("Gauche", "Version locale"), ("Résultat", "Résultat"), ("Droite", "Version distante")];
                (buttons, &['g', 'r', 'd'][..], counts.to_vec(), tab.caret_pane().unwrap_or(1))
            }),
            _ => None,
        };
        let (Some(prompt), Some((buttons, letters, counts, in_use))) = (&mut self.go_to_line, panes) else {
            self.go_to_line = None;
            return;
        };
        let typed = parse_pane_target(&prompt.text, letters);
        // The side named before the number, or the one clicked, or the one in use.
        let side = typed.and_then(|(side, _)| side).or(prompt.side).unwrap_or(in_use);
        let id = egui::Id::unique("go-to-line");
        let area = egui::Modal::default_area(id).anchor(egui::Align2::CENTER_TOP, vec2(0.0, 90.0));
        let (mut submit, mut close) = (false, false);
        let width = PROMPT_WIDTH + (buttons.len() as f32 - 2.0) * (SIDE_BUTTON.x + 2.0);
        let modal = egui::Modal::new(id).area(area).frame(menu_frame(chrome).inner_margin(Margin::ZERO)).show(ctx, |ui| {
            (submit, close) = ui.input_mut(|input| {
                (input.consume_key(Modifiers::NONE, Key::Enter), input.consume_key(Modifiers::NONE, Key::Escape))
            });
            ui.set_width(width);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let (field, _) = ui.allocate_exact_size(vec2(width, 46.0), Sense::hover());
            let center_y = field.center().y;
            icons::paint(ui.painter(), Icon::Goto, icons::square(pos2(field.min.x + 22.0, center_y), 16.0), chrome.text_dim);

            let sides_left = field.max.x - 10.0 - (SIDE_BUTTON.x + 2.0) * buttons.len() as f32 + 2.0;
            for (index, &(name, label)) in buttons.iter().enumerate() {
                let rect = Rect::from_min_size(
                    pos2(sides_left + index as f32 * (SIDE_BUTTON.x + 2.0), center_y - SIDE_BUTTON.y / 2.0),
                    SIDE_BUTTON,
                );
                let response = ui.interact(rect, ui.scope_id().with(("go-to-side", index)), Sense::click());
                response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, label));
                let lit = side == index;
                if lit {
                    let border = mix(chrome.raised, chrome.accent, 0.55);
                    ui.painter().rect(rect, 6.0, mix(chrome.raised, chrome.accent, 0.2), Stroke::new(1.0, border), egui::StrokeKind::Inside);
                } else if response.hovered() {
                    ui.painter().rect_filled(rect, 6.0, chrome.hover);
                }
                let color = if lit { chrome.accent } else { chrome.text_dim };
                ui.painter().text(rect.center(), Align2::CENTER_CENTER, name, widgets::medium(12.0), color);
                if response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    prompt.side = Some(index);
                    // The click wins over a side typed earlier.
                    if let Some((Some(_), number)) = typed {
                        prompt.text = number.to_string();
                    }
                }
            }

            let inner = Rect::from_min_max(pos2(field.min.x + 42.0, field.min.y), pos2(sides_left - 10.0, field.max.y));
            let mut child = ui.new_child(
                egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            child.spacing_mut().interact_size.y = 0.0;
            let response = child.add(
                egui::TextEdit::singleline(&mut prompt.text)
                    .id(ui.scope_id().with("go-to-line-number"))
                    .font(FontId::proportional(14.0))
                    .hint_text("Aller à la ligne…")
                    .frame(Frame::NONE)
                    .margin(Margin::ZERO)
                    .desired_width(inner.width()),
            );
            ui.ctx().accesskit_node_builder(response.id, |builder| {
                builder.set_label("Numéro de ligne");
            });
            if !prompt.focus_done || !response.has_focus() {
                prompt.focus_done = true;
                response.request_focus();
            }
            if response.changed() {
                prompt.invalid = false;
            }
            ui.painter().hline(field.x_range(), ui.painter().round_to_pixel_center(field.max.y), Stroke::new(1.0, chrome.border));

            let (hint, _) = ui.allocate_exact_size(vec2(width, 34.0), Sense::hover());
            let (text, color) = if prompt.invalid {
                ("Tape un numéro de ligne".to_owned(), chrome.error)
            } else {
                (format!("De 1 à {}", counts[side.min(counts.len() - 1)]), chrome.text_dim)
            };
            ui.painter().text(pos2(hint.min.x + 16.0, hint.center().y), Align2::LEFT_CENTER, text, FontId::proportional(12.0), color);
            let letters_hint = if buttons.len() == 3 { "g:12, r:12 ou d:12 choisit le volet" } else { "g:12 ou d:12 choisit le côté" };
            ui.painter().text(
                pos2(hint.max.x - 16.0, hint.center().y),
                Align2::RIGHT_CENTER,
                letters_hint,
                FontId::proportional(12.0),
                chrome.text_dim,
            );
        });
        if submit {
            match parse_pane_target(&prompt.text, letters) {
                Some((typed_side, number)) => {
                    let pane = typed_side.or(prompt.side);
                    match self.tabs.get_mut(self.active) {
                        Some(Tab::Text(tab)) => tab.go_to_line(pane.map(|pane| if pane == 0 { Side::Left } else { Side::Right }), number),
                        Some(Tab::Merge(tab)) => tab.go_to_line(pane, number),
                        _ => {}
                    }
                    close = true;
                }
                None if prompt.text.trim().is_empty() => close = true,
                None => prompt.invalid = true,
            }
        }
        if close || modal.should_close() {
            self.go_to_line = None;
        }
    }

    fn show_open_dialog(&mut self, ctx: &egui::Context, chrome: &UiPalette) {
        let picker = self.picker.0.clone();
        let mut submit = false;
        let mut cancel = false;
        let mut head = false;
        let mut resolve = false;
        let Some(dialog) = &mut self.open_dialog else {
            return;
        };
        let modal = egui::Modal::new(egui::Id::unique("open-comparison")).frame(dialog_frame(chrome)).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new("Nouvelle comparaison").font(widgets::semibold(16.0)));
            ui.add_space(6.0);
            for (side, label) in ["Gauche", "Droite"].into_iter().enumerate() {
                ui.label(RichText::new(label).color(chrome.text_dim).size(12.0));
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(414.0, FIELD_HEIGHT), Sense::hover());
                    let field = widgets::text_field(
                        ui,
                        chrome,
                        ui.scope_id().with(("open", side)),
                        rect,
                        &mut dialog.paths[side],
                        "Chemin d'un fichier ou d'un dossier",
                        chrome.text,
                        label,
                    );
                    if side == 1 && std::mem::take(&mut dialog.focus_right) {
                        field.request_focus();
                    }
                    for (text, folder) in [("Fichier…", false), ("Dossier…", true)] {
                        if ui.add_sized([84.0, FIELD_HEIGHT], Button::new(text)).clicked() {
                            let picker = picker.clone();
                            widgets::pick_path(ctx, folder, move |path| {
                                let _ = picker.send((side, path));
                            });
                        }
                    }
                });
                ui.add_space(4.0);
            }
            if let Some(error) = &dialog.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            if dialog.conflicted.is_some() {
                // One file holding git's markers: what the user needs is a merge.
                Frame::new()
                    .fill(mix(chrome.raised, chrome.accent, 0.1))
                    .stroke(Stroke::new(1.0, mix(chrome.raised, chrome.accent, 0.4)))
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.set_min_height(32.0);
                            let (icon, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                            icons::paint(ui.painter(), Icon::GitMerge, icons::square(icon.center(), 16.0), chrome.accent);
                            ui.label(RichText::new("Ce fichier contient des conflits : résoudre en fusion à 3 voies").color(chrome.text));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(primary_button(chrome, "Résoudre")).clicked() {
                                    resolve = true;
                                }
                            });
                        });
                    });
            }
            ui.add_space(6.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(primary_button(chrome, "Comparer")).clicked() {
                    submit = true;
                }
                if ui.button("Annuler").clicked() {
                    cancel = true;
                }
                ui.add_space((ui.available_width() - 160.0).max(0.0));
                let response = ui.add(Button::new("Comparer avec HEAD").min_size(vec2(0.0, 32.0)));
                if response.on_hover_text("Le premier fichier indiqué, contre sa version du dernier commit Git").clicked() {
                    head = true;
                }
            });
            submit |= ui.input(|i| i.key_pressed(Key::Enter));
        });
        cancel |= modal.should_close();
        // A single file is compared with nothing: if git left it in conflict, a merge is offered.
        let given: Vec<PathBuf> = dialog.paths.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).map(PathBuf::from).collect();
        if submit && given.len() == 1 && given[0].is_file() {
            submit = false;
            match Document::load(&given[0]) {
                Ok(doc) if has_conflict_markers(&doc.lines) => {
                    dialog.conflicted = Some(given[0].clone());
                    dialog.error = None;
                }
                _ => dialog.error = Some("Indique un second fichier à comparer".to_owned()),
            }
        }
        if cancel {
            self.open_dialog = None;
        } else if let (true, Some(path)) = (resolve, dialog.conflicted.clone()) {
            let waiter = self.close_open_dialog();
            self.open_merge(ctx, MergeInputs::Conflicted(path), waiter);
        } else if head {
            let path = dialog.paths.iter().map(|p| p.trim()).find(|p| !p.is_empty()).map(PathBuf::from);
            match path.filter(|path| path.is_file()) {
                Some(path) => {
                    let waiter = self.close_open_dialog();
                    self.compare_with_head(ctx, path, waiter);
                }
                None => dialog.error = Some("Indique un fichier pour le comparer avec HEAD".to_owned()),
            }
        } else if submit {
            let paths = dialog.paths.clone().map(|p| PathBuf::from(p.trim()));
            match self.open(ctx, paths) {
                Ok(()) => {
                    if let (Some(waiter), Some(slot)) = (self.close_open_dialog(), self.waiters.last_mut()) {
                        *slot = Some(waiter);
                    }
                }
                Err(error) => {
                    if let Some(dialog) = &mut self.open_dialog {
                        dialog.error = Some(error);
                    }
                }
            }
        }
    }

    /// Closes the comparison dialog. A launch that was waiting on it (one path given) waits on
    /// the tab it opened instead, which the caller hands it to.
    fn close_open_dialog(&mut self) -> Option<Waiter> {
        self.open_dialog.take().and_then(|dialog| dialog.waiter)
    }

    /// The four files of a merge, in the look of the comparison dialog.
    fn show_merge_dialog(&mut self, ctx: &egui::Context, chrome: &UiPalette) {
        let picker = self.merge_picker.0.clone();
        let (mut submit, mut cancel) = (false, false);
        let Some(dialog) = &mut self.merge_dialog else {
            return;
        };
        let fields = [
            ("Ancêtre commun (base)", "Version d'où partent les deux autres ; vide s'il n'y en a pas"),
            ("Locale (gauche)", "Ta version"),
            ("Distante (droite)", "La version à fusionner"),
            ("Résultat", "Fichier où écrire la fusion"),
        ];
        let modal = egui::Modal::new(egui::Id::unique("new-merge")).frame(dialog_frame(chrome)).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new("Nouvelle fusion à 3 voies").font(widgets::semibold(16.0)));
            ui.add_space(6.0);
            for (slot, (label, hint)) in fields.into_iter().enumerate() {
                ui.label(RichText::new(label).color(chrome.text_dim).size(12.0));
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(506.0, FIELD_HEIGHT), Sense::hover());
                    widgets::text_field(ui, chrome, ui.scope_id().with(("merge", slot)), rect, &mut dialog.paths[slot], hint, chrome.text, label);
                    if ui.add_sized([84.0, FIELD_HEIGHT], Button::new("Fichier…")).clicked() {
                        let picker = picker.clone();
                        widgets::pick_path(ctx, false, move |path| {
                            let _ = picker.send((slot, path));
                        });
                    }
                });
                ui.add_space(2.0);
            }
            if let Some(error) = &dialog.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.add_space(6.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(primary_button(chrome, "Fusionner")).clicked() {
                    submit = true;
                }
                if ui.button("Annuler").clicked() {
                    cancel = true;
                }
            });
            submit |= ui.input(|i| i.key_pressed(Key::Enter));
        });
        if cancel || modal.should_close() {
            self.merge_dialog = None;
            return;
        }
        if !submit {
            return;
        }
        let [base, local, remote, merged] = dialog.paths.clone().map(|p| PathBuf::from(p.trim()));
        let error = if local.as_os_str().is_empty() || remote.as_os_str().is_empty() || merged.as_os_str().is_empty() {
            Some("Indique au moins la version locale, la distante et le résultat".to_owned())
        } else if !local.is_file() && !remote.is_file() {
            Some("Ni la version locale ni la distante n'existent".to_owned())
        } else if merged.is_dir() {
            Some(format!("{} est un dossier", merged.display()))
        } else {
            None
        };
        if let Some(error) = error {
            dialog.error = Some(error);
            return;
        }
        self.merge_dialog = None;
        // No base: both sides added the file, which an empty missing base says.
        let base = if base.as_os_str().is_empty() { PathBuf::from("/dev/null") } else { base };
        let inputs = MergeInputs::Files { base, local, remote, merged, labels: Default::default() };
        self.open_merge(ctx, inputs, None);
    }

    fn show_font_dialog(&mut self, ctx: &egui::Context, chrome: &UiPalette) {
        let mut pick: Option<Option<String>> = None;
        let mut close = false;
        let mut size = self.code_size;
        let current = self.code_family.clone();
        let Some(dialog) = &mut self.font_dialog else {
            return;
        };
        if let Ok(families) = dialog.receiver.try_recv() {
            dialog.families = Some(families);
        }
        let modal = egui::Modal::new(egui::Id::unique("code-font")).frame(dialog_frame(chrome)).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.label(RichText::new("Police du texte").font(widgets::semibold(16.0)));
            ui.label(RichText::new("Seules les polices à chasse fixe sont proposées.").color(chrome.text_dim).size(12.0));
            ui.add_space(4.0);
            Frame::new()
                .fill(chrome.input)
                .stroke(Stroke::new(1.0, chrome.border))
                .corner_radius(8)
                .inner_margin(Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let font = FontId::monospace(size);
                    ui.label(RichText::new("fn compare(left: &str) -> bool { 0O Il1 }").font(font.clone()).color(chrome.text));
                    ui.label(RichText::new("{}[]()<> => != 12 345,67 € @#%& éàçô").font(font).color(chrome.accent));
                });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Taille").color(chrome.text_dim));
                ui.spacing_mut().slider_width = 300.0;
                ui.add(egui::Slider::new(&mut size, MIN_CODE_SIZE..=MAX_CODE_SIZE).step_by(0.5).suffix(" px"));
            });
            ui.add_space(4.0);
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), FIELD_HEIGHT), Sense::hover());
            widgets::text_field(
                ui,
                chrome,
                ui.scope_id().with("font-query"),
                rect,
                &mut dialog.query,
                "Rechercher une police…",
                chrome.text,
                "Rechercher une police",
            );
            let query = dialog.query.trim().to_lowercase();
            egui::ScrollArea::vertical().max_height(280.0).auto_shrink([false, false]).show(ui, |ui| {
                menu_width(ui, 462.0);
                if query.is_empty()
                    && Item::new("Par défaut").checked(current.is_none()).show(ui, chrome).clicked()
                {
                    pick = Some(None);
                }
                match &dialog.families {
                    None => {
                        ui.label(RichText::new("Recherche des polices installées…").color(chrome.text_dim));
                    }
                    Some(families) => {
                        for family in families.iter().filter(|f| f.to_lowercase().contains(&query)) {
                            let on = current.as_deref() == Some(family);
                            if Item::new(family).checked(on).show(ui, chrome).clicked() {
                                pick = Some(Some(family.clone()));
                            }
                        }
                    }
                }
            });
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(primary_button(chrome, "Fermer")).clicked() {
                    close = true;
                }
            });
        });
        self.code_size = size;
        if let Some(family) = pick {
            self.code_family = family;
            self.install_look(ctx);
        }
        if close || modal.should_close() {
            self.font_dialog = None;
        }
    }

    fn show_confirm(&mut self, ctx: &egui::Context, chrome: &UiPalette, action: AfterConfirm) {
        #[derive(PartialEq)]
        enum Answer {
            Save,
            Discard,
            Cancel,
        }
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("confirm")).frame(dialog_frame(chrome)).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(RichText::new("Modifications non enregistrées").font(widgets::semibold(15.0)));
            ui.add_space(4.0);
            ui.label("Les enregistrer avant de continuer ?");
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(primary_button(chrome, "Enregistrer")).clicked() {
                    answer = Some(Answer::Save);
                }
                if ui.button("Ne pas enregistrer").clicked() {
                    answer = Some(Answer::Discard);
                }
                if ui.button("Annuler").clicked() {
                    answer = Some(Answer::Cancel);
                }
            });
        });
        let Some(answer) = answer else {
            return;
        };
        self.confirm = None;
        if answer == Answer::Cancel {
            return;
        }
        if answer == Answer::Save {
            let targets: Vec<usize> = match action {
                AfterConfirm::CloseTab(index) | AfterConfirm::Reload(index) => vec![index],
                AfterConfirm::Quit => (0..self.tabs.len()).collect(),
            };
            for index in targets {
                self.save_tab(index);
                if self.tabs.get(index).is_some_and(Tab::is_dirty) {
                    // The failure is displayed in the status bar; nothing may be lost.
                    return;
                }
            }
        }
        match action {
            AfterConfirm::CloseTab(index) => self.close_tab(index),
            AfterConfirm::Reload(index) => self.reload(ctx, index),
            AfterConfirm::Quit => {
                self.close_allowed = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    pub fn root(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let look = self.look;
        let chrome = look.ui;
        let metrics = look.metrics;
        let mut commands = Vec::new();

        if ctx.input(|i| i.viewport().close_requested())
            && !self.close_allowed
            && self.tabs.iter().any(Tab::is_dirty)
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.confirm = Some(AfterConfirm::Quit);
        }
        self.poll_background(&ctx);
        let modal_open = self.confirm.is_some()
            || self.open_dialog.is_some()
            || self.merge_dialog.is_some()
            || matches!(self.tabs.get(self.active), Some(Tab::Merge(tab)) if tab.wants_dialog())
            || self.palette.is_some()
            || self.go_to_line.is_some()
            || self.font_dialog.is_some()
            || self.rules_editor
            || matches!(self.tabs.get(self.active), Some(Tab::Folder(tab)) if tab.wants_dialog())
            || matches!(self.tabs.get(self.active), Some(Tab::Text(tab)) if tab.wants_dialog());
        if !modal_open {
            self.handle_shortcuts(&ctx);
            self.zoom_with_wheel(&ctx);
        }

        // The window itself, under everything: Verre's glass lets the compositor's blur show,
        // with Biodrive's rounded corners and light edge; other themes are solid.
        let window = ui.max_rect();
        let radius = if look.translucent { metrics.window_radius } else { 0 };
        ui.painter().rect(window, radius, chrome.window, Stroke::new(1.0, chrome.window_border), egui::StrokeKind::Inside);

        Panel::top("band")
            .frame(Frame::new().fill(chrome.panel))
            .exact_size(metrics.band_height)
            .show_separator_line(false)
            .show(ui, |ui| self.band(ui, &look, &mut commands));
        Panel::bottom("status-bar")
            .frame(Frame::new().fill(chrome.panel))
            .exact_size(metrics.status_height)
            .show_separator_line(false)
            .show(ui, |ui| self.status_bar(ui, &look, &mut commands));
        let mut open_files = None;
        let code_size = self.code_size;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(chrome.background))
            .show(ui, |ui| match self.tabs.get_mut(self.active) {
                Some(Tab::Text(tab)) => tab.show(ui, &look, code_size),
                Some(Tab::Folder(tab)) => open_files = tab.show(ui, &look).map(|FolderAction::Open(paths)| paths),
                Some(Tab::Merge(tab)) => tab.show(ui, &look, code_size),
                None => self.welcome(ui, &chrome, &mut commands),
            });
        // Saves happen inside a merge tab too (its header, its question): its launch must know.
        self.merge_saved(self.active);
        if let Some(paths) = open_files {
            let tab = self.file_tab(&ctx, paths, [None, None], true);
            self.push_tab(tab, None);
        }
        self.dialogs(&ctx, &chrome, &mut commands);
        for command in commands {
            self.run(&ctx, command);
        }
    }

    #[cfg(test)]
    pub fn set_theme(&mut self, ctx: &egui::Context, kind: ThemeKind) {
        self.run(ctx, Command::SetTheme(kind));
    }

    #[cfg(test)]
    pub fn is_settled(&self) -> bool {
        self.git_pending == 0 && self.tabs.iter().all(|tab| match tab {
            Tab::Text(tab) => tab.is_ready() && tab.highlight_finished(),
            Tab::Folder(tab) => tab.is_finished(),
            Tab::Merge(tab) => tab.is_ready() && tab.highlight_finished(),
        })
    }

    /// What the status bar lists for the active tab, as plain text.
    #[cfg(test)]
    pub fn status_items_text(&self) -> String {
        let items = match self.tabs.get(self.active) {
            Some(Tab::Text(tab)) => tab.status_items(),
            Some(Tab::Folder(tab)) => tab.status_items(),
            Some(Tab::Merge(tab)) => tab.status_items(),
            None => Vec::new(),
        };
        let words: Vec<String> = items
            .into_iter()
            .map(|item| match item {
                StatusItem::Text(text) | StatusItem::Pill(text) | StatusItem::Path(text) => text,
                StatusItem::Counts { deletions, insertions, changes } => format!("{deletions}/{insertions}/{changes}"),
                StatusItem::Conflicts { unresolved, .. } => conflicts_text(unresolved),
            })
            .collect();
        words.join(" · ")
    }

    #[cfg(test)]
    pub fn text_tab_mut(&mut self) -> &mut TextTab {
        self.active_text().expect("a text tab is active")
    }

    #[cfg(test)]
    pub fn merge_tab_mut(&mut self) -> &mut MergeTab {
        self.active_merge().expect("a merge tab is active")
    }
}

/// What a launch asks to merge, as a tab opens it.
fn merge_inputs(request: MergeRequest) -> MergeInputs {
    let MergeRequest { base, local, remote, merged, labels } = request;
    MergeInputs::Files { base, local, remote, merged, labels }
}

/// A long JSON path keeps its end, which tells where the line is.
fn elide_path(path: &str) -> String {
    const MAX: usize = 64;
    let count = path.chars().count();
    if count <= MAX {
        return path.to_owned();
    }
    let tail: String = path.chars().skip(count - (MAX - 1)).collect();
    format!("…{tail}")
}

fn text_width(ui: &Ui, text: &str, font: FontId) -> f32 {
    ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.to_owned(), font, egui::Color32::WHITE))
        .size()
        .x
}

/// One option of the status bar: its icon, its short name for Graphite's words, and its tip,
/// which is also its accessible name.
struct Toggle {
    icon: Icon,
    short: &'static str,
    lit: bool,
    tip: &'static str,
    command: Command,
}

/// The rounded tray Verre gathers buttons in.
fn glass_tray(ui: &mut Ui, look: &Look, add: impl FnOnce(&mut Ui)) {
    tray(ui, theme::alpha(0xffffff, 0.045), Stroke::new(1.0, look.ui.card_border), 14.0, 4.0, 38.0, add);
}

/// A rounded tray behind buttons laid out in a row: as tall as `height`, as wide as what `add`
/// lays out, `margin` around it. Painted underneath once the width is known.
fn tray(ui: &mut Ui, fill: Color32, stroke: Stroke, radius: f32, margin: f32, height: f32, add: impl FnOnce(&mut Ui)) {
    let slot = ui.painter().add(egui::Shape::Noop);
    let left = ui.cursor().min.x;
    ui.add_space(margin);
    let spacing = ui.spacing().item_spacing.x;
    add(ui);
    let right = ui.cursor().min.x - spacing + margin;
    ui.add_space(margin);
    let y = ui.max_rect().center().y;
    let rect = Rect::from_min_max(pos2(left, y - height / 2.0), pos2(right, y + height / 2.0));
    ui.painter().set(slot, egui::epaint::RectShape::new(rect, radius, fill, stroke, egui::StrokeKind::Inside));
}

fn option_toggle(ui: &mut Ui, look: &Look, toggle: &Toggle) -> egui::Response {
    let chrome = &look.ui;
    let lit = toggle.lit;
    if chrome.chrome == Chrome::Graphite {
        // Graphite names its options in words.
        let font = FontId::proportional(12.0);
        let width = text_width(ui, toggle.short, font.clone()) + 16.0;
        let (rect, response) = ui.allocate_exact_size(vec2(width, 24.0), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, toggle.tip));
        let hovered = response.hovered();
        if lit || hovered {
            ui.painter().rect_filled(rect, 5.0, if lit { chrome.lit } else { chrome.hover });
        }
        let color = if lit || hovered { chrome.text } else { chrome.text_dim };
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, toggle.short, font, color);
        return response.on_hover_text(toggle.tip).on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    let (size, radius) = match chrome.chrome {
        Chrome::Verre => (vec2(34.0, 30.0), 10.0),
        Chrome::Ardoise => (vec2(32.0, 28.0), 8.0),
        _ => (TOGGLE_SIZE, 6.0),
    };
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, toggle.tip));
    let hovered = response.hovered();
    let painter = ui.painter();
    if lit {
        let fill = if hovered { mix(chrome.lit, chrome.lit_text, 0.12) } else { chrome.lit };
        painter.rect(rect, radius, fill, Stroke::new(1.0, chrome.lit_border), egui::StrokeKind::Inside);
    } else if hovered {
        painter.rect_filled(rect, radius, chrome.hover);
    }
    let color = if lit {
        chrome.lit_text
    } else if hovered {
        chrome.text
    } else {
        chrome.text_dim
    };
    icons::paint(painter, toggle.icon, icons::square(rect.center(), 16.0), color);
    response.on_hover_text(toggle.tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A− 13 A+: the code size at a glance, a click away in every tab. A click on the number goes
/// back to the default size.
fn size_control(ui: &mut Ui, look: &Look, size: f32, commands: &mut Vec<Command>) {
    let chrome = &look.ui;
    let style = chrome.chrome;
    let number = match style {
        Chrome::Verre | Chrome::Graphite => format!("{} px", size.round()),
        _ => format!("{}", size.round()),
    };
    let mut body = |ui: &mut Ui| {
        let (button, height) = match style {
            Chrome::Verre => (vec2(30.0, 30.0), 30.0),
            Chrome::Graphite => (vec2(24.0, 24.0), 24.0),
            _ => (vec2(30.0, 26.0), 26.0),
        };
        let step = |ui: &mut Ui, text: &str, glyph: f32, tip: &str| -> bool {
            let (rect, response) = ui.allocate_exact_size(button, Sense::click());
            response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, tip));
            let hovered = response.hovered();
            if style == Chrome::Graphite {
                ui.painter().rect(rect, 5.0, if hovered { chrome.hover } else { Color32::TRANSPARENT }, Stroke::new(1.0, chrome.border), egui::StrokeKind::Inside);
            } else if hovered {
                ui.painter().rect_filled(rect, if style == Chrome::Verre { 10.0 } else { 6.0 }, chrome.hover);
            }
            let color = if hovered { chrome.text } else { mix(chrome.text_dim, chrome.text, 0.4) };
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, widgets::bold(glyph), color);
            response.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
        };
        let (smaller, larger) = if style == Chrome::Graphite { ("−", "+") } else { ("A−", "A+") };
        if step(ui, smaller, 12.0, "Réduire le texte (Ctrl+-)") {
            commands.push(Command::ChangeCodeSize(-1.0));
        }
        let font = if style == Chrome::Graphite { FontId::monospace(12.0) } else { widgets::semibold(12.5) };
        let width = text_width(ui, &number, font.clone()).max(if style == Chrome::Verre { 44.0 } else { 26.0 }) + 8.0;
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, "Taille du code"));
        if style == Chrome::Ardoise {
            for x in [rect.left(), rect.right()] {
                ui.painter().vline(ui.painter().round_to_pixel_center(x), rect.y_range(), Stroke::new(1.0, chrome.border));
            }
        }
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, &number, font, chrome.text);
        if response.on_hover_text("Taille du code : un clic revient à la taille par défaut (Ctrl+0)").clicked() {
            commands.push(Command::ResetCodeSize);
        }
        if step(ui, larger, 15.0, "Agrandir le texte (Ctrl++)") {
            commands.push(Command::ChangeCodeSize(1.0));
        }
    };
    match style {
        Chrome::Verre => glass_tray(ui, look, body),
        Chrome::Ardoise => tray(ui, Color32::TRANSPARENT, Stroke::new(1.0, chrome.border), 8.0, 0.0, 28.0, body),
        _ => body(ui),
    }
}

/// Ardoise et papier's switch between light and dark code, in its status bar.
fn code_tone_button(ui: &mut Ui, look: &Look, commands: &mut Vec<Command>) {
    let chrome = &look.ui;
    let (icon, text, other) = if look.doc.dark {
        (Icon::Moon, "Code sombre", CodeTone::Light)
    } else {
        (Icon::Sun, "Code clair", CodeTone::Dark)
    };
    let font = widgets::semibold(12.0);
    let width = text_width(ui, text, font.clone()) + 41.0;
    let (rect, response) = ui.allocate_exact_size(vec2(width, 28.0), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, "Code sombre ou clair"));
    let fill = if response.hovered() { chrome.hover } else { Color32::TRANSPARENT };
    ui.painter().rect(rect, 8.0, fill, Stroke::new(1.0, chrome.border), egui::StrokeKind::Inside);
    icons::paint(ui.painter(), icon, icons::square(pos2(rect.left() + 17.0, rect.center().y), 14.0), chrome.text_dim);
    ui.painter().text(pos2(rect.left() + 31.0, rect.center().y), Align2::LEFT_CENTER, text, font, mix(chrome.text_dim, chrome.text, 0.5));
    if response.on_hover_text("Code sombre ou clair").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        commands.push(Command::SetCodeTone(other));
    }
}

/// The frame of the band's field, in the theme's style.
fn field_frame(ui: &Ui, look: &Look, rect: Rect, focused: bool, hovered: bool, invalid: bool) {
    let chrome = &look.ui;
    let radius = match chrome.chrome {
        Chrome::Verre => 11.0,
        Chrome::Ardoise => 9.0,
        Chrome::Porcelaine => 12.0,
        Chrome::Graphite => 8.0,
        Chrome::Classic => 7.0,
    };
    let rest = match chrome.chrome {
        Chrome::Verre => theme::alpha(0xffffff, 0.12),
        Chrome::Ardoise => Color32::from_rgb(0x26, 0x2b, 0x34),
        Chrome::Porcelaine => theme::alpha(0x000000, 0.06),
        _ => chrome.border,
    };
    let border = if invalid {
        chrome.error
    } else if focused {
        chrome.accent
    } else if hovered {
        mix(rest, chrome.text_dim, 0.4)
    } else {
        rest
    };
    let painter = ui.painter();
    if focused {
        painter.rect_stroke(rect, radius, Stroke::new(3.0, chrome.accent.gamma_multiply(0.22)), egui::StrokeKind::Outside);
    }
    if chrome.chrome == Chrome::Porcelaine {
        painter.add(egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(15) }.as_shape(rect, radius as u8));
    }
    painter.rect(rect, radius, chrome.input, Stroke::new(1.0, border), egui::StrokeKind::Inside);
}

fn pill(ui: &mut Ui, look: &Look, text: &str) {
    let chrome = &look.ui;
    if chrome.chrome == Chrome::Ardoise {
        ui.label(RichText::new(text).color(chrome.text_dim).size(12.5));
        return;
    }
    let font = widgets::medium(11.5);
    let color = if chrome.chrome == Chrome::Verre { chrome.text_dim } else { chrome.text_dim };
    let galley = ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.to_owned(), font, color));
    let height = if chrome.chrome == Chrome::Verre { 24.0 } else { PILL_HEIGHT };
    let (rect, _) = ui.allocate_exact_size(vec2((galley.size().x + PILL_PADDING).round(), height), Sense::hover());
    let painter = ui.painter();
    match chrome.chrome {
        Chrome::Verre => {
            painter.rect_filled(rect, 8.0, theme::alpha(0xffffff, 0.06));
        }
        _ => {
            painter.rect(rect, PILL_HEIGHT / 2.0, mix(chrome.status_top, chrome.hover, 0.5), Stroke::new(1.0, chrome.border), egui::StrokeKind::Inside);
        }
    }
    let position = rect.center() - galley.size() / 2.0;
    painter.galley(position, galley, color);
}

fn conflicts_text(unresolved: usize) -> String {
    match unresolved {
        0 => "Aucun conflit non résolu".to_owned(),
        1 => "1 conflit non résolu".to_owned(),
        n => format!("{n} conflits non résolus"),
    }
}

/// The conflicts left to resolve, in the conflict colour; calm once there are none.
fn conflicts_pill(ui: &mut Ui, look: &Look, unresolved: usize, total: usize) {
    let chrome = &look.ui;
    let mark = look.gap.conflict.mark;
    let font = widgets::medium(11.5);
    let [r, g, b, _] = mark.to_array();
    let (fill, border, color) = if unresolved > 0 {
        (Color32::from_rgba_unmultiplied(r, g, b, 40), Color32::from_rgba_unmultiplied(r, g, b, 150), mark)
    } else {
        (chrome.hover, chrome.border, chrome.text_dim)
    };
    let galley = ui.fonts_mut(|fonts| fonts.layout_no_wrap(conflicts_text(unresolved), font, color));
    let (rect, response) = ui.allocate_exact_size(vec2((galley.size().x + PILL_PADDING).round(), PILL_HEIGHT), Sense::hover());
    ui.painter().rect(rect, PILL_HEIGHT / 2.0, fill, Stroke::new(1.0, border), egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    let tip = match total {
        0 => "Aucun conflit : tout s'est fusionné de lui-même".to_owned(),
        _ => format!("{} conflit{} en tout ; F8 mène au prochain non résolu", total, if total > 1 { "s" } else { "" }),
    };
    response.on_hover_text(tip);
}

/// Modifications, insertions and removals, each after a dot of its colour: glowing in Verre's
/// pill, square in Ardoise et papier and Graphite.
fn counts(ui: &mut Ui, look: &Look, values: [usize; 3]) {
    let chrome = &look.ui;
    let style = chrome.chrome;
    // Shown left to right: modifications, insertions, removals; `values` holds them the other
    // way round.
    let shown = [values[2], values[1], values[0]];
    let colors = [look.gap.change.mark, look.gap.insert.mark, look.gap.delete.mark];
    let words = ["modifications", "ajouts", "retraits"];
    let label = |index: usize, value: usize| -> String {
        if style != Chrome::Verre {
            return value.to_string();
        }
        let plural = if value > 1 { "s" } else { "" };
        match index {
            0 => format!("{value} modifié{plural}"),
            1 => format!("{value} ajout{plural}"),
            _ => format!("{value} retrait{plural}"),
        }
    };
    let font = if style == Chrome::Verre { widgets::semibold(12.5) } else { widgets::medium(12.0) };
    let texts: Vec<String> = (0..3).map(|index| label(index, shown[index])).collect();
    let widths: Vec<f32> = texts.iter().map(|text| text_width(ui, text, font.clone())).collect();
    let (dot, gap, pad) = if style == Chrome::Verre { (8.0, 7.0, 14.0) } else { (8.0, 5.0, 0.0) };
    let spacing = if style == Chrome::Verre { 14.0 } else { 9.0 };
    let width = widths.iter().map(|w| dot + gap + w).sum::<f32>() + spacing * 2.0 + pad * 2.0;
    let height = if style == Chrome::Verre { 36.0 } else { 20.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(width.ceil(), height), Sense::hover());
    let painter = ui.painter();
    if style == Chrome::Verre {
        painter.rect(rect, 18.0, theme::alpha(0xffffff, 0.045), Stroke::new(1.0, chrome.card_border), egui::StrokeKind::Inside);
    }
    let y = rect.center().y;
    let mut x = rect.left() + pad;
    for index in 0..3 {
        let square = Rect::from_center_size(pos2(x + dot / 2.0, y), vec2(dot, dot));
        match style {
            Chrome::Verre => {
                let [r, g, b, _] = colors[index].to_array();
                painter.add(egui::Shadow { offset: [0, 0], blur: 8, spread: 0, color: Color32::from_rgba_unmultiplied(r, g, b, 160) }.as_shape(square, 4));
                painter.circle_filled(square.center(), 4.0, colors[index]);
            }
            Chrome::Ardoise => {
                painter.rect_filled(square, 2.0, colors[index]);
            }
            Chrome::Graphite => {
                painter.rect_filled(square, 0.0, colors[index]);
            }
            _ => {
                painter.circle_filled(square.center(), 3.5, colors[index]);
            }
        }
        x += dot + gap;
        painter.text(pos2(x, y), Align2::LEFT_CENTER, &texts[index], font.clone(), chrome.text);
        let hover = Rect::from_min_max(pos2(square.left(), y - 10.0), pos2(x + widths[index], y + 10.0));
        let _ = ui.interact(hover, ui.scope_id().with(("count", index)), Sense::hover()).on_hover_text(format!("{} {}", shown[index], words[index]));
        x += widths[index] + spacing;
    }
}

fn submenu_config(chrome: &UiPalette) -> egui::containers::menu::MenuConfig {
    let frame = menu_frame(chrome);
    egui::containers::menu::MenuConfig::new().style(egui::style::StyleModifier::new(move |style| {
        style.visuals.window_fill = frame.fill;
    }))
}

fn primary_button<'a>(chrome: &UiPalette, text: &'a str) -> Button<'a> {
    crate::folder_view::primary_button(chrome, text)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.root(ui);
    }

    /// Fully transparent: the app paints the whole window itself, as glass or solid, so the
    /// theme can change while running without recreating the window.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string("theme", self.theme.key().to_owned());
        storage.set_string("code_tone", self.code_tone.key().to_owned());
        storage.set_string("link_style", self.links.key().to_owned());
        storage.set_string("transparent_window", self.transparent.to_string());
        storage.set_string("wrap_lines", self.wrap_lines.to_string());
        storage.set_string("aligned_lines", self.aligned_lines.to_string());
        storage.set_string("context_only", self.context_only.to_string());
        storage.set_string("context_lines", self.context_lines.to_string());
        storage.set_string("code_family", self.code_family.clone().unwrap_or_default());
        storage.set_string("code_size", self.code_size.to_string());
        storage.set_string("ignore_rules", rules_editor::encode(&self.ignore_rules));
        storage.set_string("folder_filter", self.folder_filter.clone());
        for (key, value) in formats::store(&self.json_options, self.json_structural, &self.extract_options) {
            storage.set_string(key, value);
        }
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod merge_tests;

#[cfg(test)]
mod visual_tests {
    use super::*;
    use egui_kittest::Harness;
    use std::path::Path;
    use std::time::Duration;

    const LEFT: &str = r#"#include <string>

/// Finds the next occurrence of a text in a range.
template<class Action>
bool doFind(engine::DiffTextLines* pEngine, int side, const AString& strText, bool bMatchCase, bool bMatchWholeWord, const engine::PositionRange& inRange, Action& matchAction)
{
    bool bFound(false), bStop(false);
    const size_t nSearchLen(strText.length());

    for (engine::textiterator<engine::DiffTextLines> current(inRange.begin(), pEngine, side); !bStop && current < inRange.end(); ++current)
    {
        const size_t nLength(pEngine->getPrunedLineLength(side, current.line()));
        const unichar* pchData = pEngine->getLineBegin(side, current.line());

        if (current.column() + nSearchLen <= nLength && matches(pchData, nLength, current.column(), strText))
        {
            bFound = true;
            current = matchAction(current, bStop);
        }
    }

    return bFound;
}

// This doesn't need to deal with text replacement.
int unchanged = 42;
"#;
    const RIGHT: &str = r#"#include <string>

/// Finds the next occurrence of a text in a range.
template<class Action>
bool doFind(engine::DiffTextLines* pEngine, int side, const AString& strText, bool bMatchCase, bool bMatchWholeWord, const engine::PositionRange& inRange, Action& matchAction)
{
    _ASSERTE(pEngine);

    bool bFound(false), bStop(false);
    const size_t nSearchLen(strText.length());

    for (engine::textiterator<engine::DiffEngine<engine::DiffTextLines> > current(inRange.begin(), pEngine, side); !bStop && current < inRange.end(); ++current)
    {
        const int nLength(pEngine->getPrunedLineLength(side, current.line()));
        const wchar_t* pchOtherData = pEngine->getLineData(1 - side, line());
        const wchar_t* pchData = pEngine->getLineData(side, current.line());

        if (current.column() + nSearchLen <= nLength && matches(pchData, nLength, current.column(), strText))
            bFound = true;
    }

    return bFound;
}

// This doesnt need to deal with text replacement.
int unchanged = 42;
"#;

    /// Something for Verre's glass to show, as the mockup's coloured blobs: the harness renders
    /// on an opaque background, where the compositor's blur would be. The other themes cover it.
    pub(super) fn paint_wallpaper(ui: &Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, Color32::from_rgb(0x14, 0x1a, 0x24));
        let blobs = [
            (pos2(rect.left() + 240.0, rect.top() + 150.0), 330.0, Color32::from_rgb(0x2a, 0x6b, 0x73)),
            (pos2(rect.right() - 220.0, rect.top() + 420.0), 360.0, Color32::from_rgb(0x4b, 0x3a, 0x8c)),
            (pos2(rect.left() + 790.0, rect.bottom() + 20.0), 380.0, Color32::from_rgb(0x1f, 0x4f, 0x8a)),
        ];
        for (center, radius, color) in blobs {
            // A soft disc: rings fading out, as a blur would leave it.
            for step in 0..24 {
                let t = step as f32 / 24.0;
                let [r, g, b, _] = color.to_array();
                painter.circle_filled(center, radius * (1.0 - t * 0.85), Color32::from_rgba_unmultiplied(r, g, b, (14.0 * (0.4 + t)) as u8));
            }
        }
    }

    pub(super) fn screenshot_dir() -> Option<std::path::PathBuf> {
        std::env::var_os("BIOMERGE_SHOTS").map(std::path::PathBuf::from)
    }

    pub(super) fn settle(harness: &mut Harness<'_, App>) {
        for _ in 0..600 {
            harness.step();
            if harness.state().is_settled() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(harness.state().is_settled(), "background work did not finish");
        harness.run_steps(3);
    }

    fn harness_for(paths: [std::path::PathBuf; 2]) -> Harness<'static, App> {
        let ctx = egui::Context::default();
        let app = App::new(&ctx, None, None);
        let mut pending = Some(paths);
        Harness::builder()
            .with_size(vec2(1440.0, 760.0))
            .with_pixels_per_point(2.0)
            .with_max_steps(20_000)
            .allow_missing_glyphs()
            .wgpu()
            .build_ui_state(
                move |ui, app: &mut App| {
                    if let Some(paths) = pending.take() {
                        app.install_look(ui.ctx());
                        app.open(ui.ctx(), paths).unwrap();
                    }
                    paint_wallpaper(ui);
                    app.root(ui);
                },
                app,
            )
    }

    pub(super) fn save(harness: &mut Harness<'_, App>, name: &str) {
        let Some(dir) = screenshot_dir() else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();
        harness
            .render()
            .expect("wgpu rendering")
            .save(dir.join(name))
            .unwrap();
    }

    fn write_fixture(dir: &Path) -> [std::path::PathBuf; 2] {
        let left = dir.join("Test1.cpp");
        let right = dir.join("Test2.cpp");
        std::fs::write(&left, LEFT).unwrap();
        std::fs::write(&right, RIGHT).unwrap();
        [left, right]
    }

    /// The file of the mockups, before and after: one change of value, a field added, a call
    /// rewritten, a function removed and two more changes.
    const FACTURE_LEFT: &str = "use std::fmt;\n\npub const TAX_RATE: f64 = 0.20;\n\n#[derive(Debug, Clone)]\npub struct Line {\n    pub label: String,\n    pub price: f64,\n    pub quantity: u32,\n}\n\npub fn subtotal(lines: &[Line]) -> f64 {\n    lines.iter().map(|l| l.price * l.quantity as f64).sum()\n}\n\n#[deprecated]\npub fn round2(v: f64) -> f64 {\n    (v * 100.0).round() / 100.0\n}\n\npub fn tax(amount: f64) -> f64 {\n    amount * TAX_RATE\n}\n\nimpl fmt::Display for Line {\n    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {\n        // Two decimals, as on the invoice.\n        write!(f, \"{} : {:.2}\", self.label, self.price)\n    }\n}\n";
    const FACTURE_RIGHT: &str = "use std::fmt;\n\npub const TAX_RATE: f64 = 0.21;\n\n#[derive(Debug, Clone)]\npub struct Line {\n    pub label: String,\n    pub price: f64,\n    pub quantity: u32,\n    pub discount: f64,\n}\n\npub fn subtotal(lines: &[Line]) -> f64 {\n    lines.iter().map(|l| l.price * f64::from(l.quantity)).sum()\n}\n\npub fn tax(amount: f64) -> f64 {\n    (amount * TAX_RATE).max(0.0)\n}\n\nimpl fmt::Display for Line {\n    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {\n        // Two decimals, as on the invoice.\n        write!(f, \"{} : {:.2} €\", self.label, self.price)\n    }\n}\n";

    fn write_facture(dir: &Path) -> [std::path::PathBuf; 2] {
        let (left, right) = (dir.join("left"), dir.join("right"));
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();
        std::fs::write(left.join("facture.rs"), FACTURE_LEFT).unwrap();
        std::fs::write(right.join("facture.rs"), FACTURE_RIGHT).unwrap();
        [left.join("facture.rs"), right.join("facture.rs")]
    }

    fn set_display(harness: &mut Harness<'_, App>, links: LinkStyle, aligned: bool) {
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::SetLinks(links));
        if harness.state().aligned_lines != aligned {
            harness.state_mut().run(&ctx, Command::ToggleAligned);
        }
    }

    #[test]
    fn every_theme_renders_both_panes_and_folders_in_both_link_styles_and_display_modes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_facture(dir.path());
        let mut harness = harness_for(paths.clone());
        harness.step();
        let ctx = harness.ctx.clone();
        for kind in ThemeKind::ALL {
            harness.state_mut().set_theme(&ctx, kind);
            for links in [LinkStyle::Ribbons, LinkStyle::Lines] {
                for aligned in [false, true] {
                    set_display(&mut harness, links, aligned);
                    settle(&mut harness);
                    if !aligned {
                        let style = if links == LinkStyle::Ribbons { "" } else { "-lines" };
                        save(&mut harness, &format!("theme-{}{style}.png", kind.key()));
                    }
                }
            }
            set_display(&mut harness, LinkStyle::Ribbons, false);
        }
        // Light code under dark glass, dark code on Ardoise's dark interface.
        for (kind, tone) in [(ThemeKind::Verre, CodeTone::Light), (ThemeKind::Ardoise, CodeTone::Dark)] {
            harness.state_mut().set_theme(&ctx, kind);
            harness.state_mut().run(&ctx, Command::SetCodeTone(tone));
            settle(&mut harness);
            save(&mut harness, &format!("theme-{}-code-{}.png", kind.key(), tone.key()));
        }
        harness.state_mut().run(&ctx, Command::SetCodeTone(CodeTone::Theme));

        // The folders of the two files. The text tab goes: hidden, it would never finish
        // colouring its code again for each theme.
        harness.state_mut().run(&ctx, Command::CloseTab);
        let folders = [paths[0].parent().unwrap().to_owned(), paths[1].parent().unwrap().to_owned()];
        std::fs::write(folders[0].join("left-only.txt"), "x").unwrap();
        std::fs::write(folders[1].join("right-only.txt"), "y").unwrap();
        std::fs::write(folders[0].join("README.md"), "same").unwrap();
        std::fs::write(folders[1].join("README.md"), "same").unwrap();
        harness.state_mut().open(&ctx, folders).unwrap();
        for kind in ThemeKind::ALL {
            harness.state_mut().set_theme(&ctx, kind);
            settle(&mut harness);
            save(&mut harness, &format!("folders-{}.png", kind.key()));
        }
    }

    /// The settings a previous run left, as eframe keeps them.
    #[derive(Default)]
    struct Stored(std::collections::HashMap<String, String>);

    impl eframe::Storage for Stored {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }

        fn flush(&mut self) {}
    }

    fn app_with(pairs: &[(&str, &str)]) -> App {
        let stored = Stored(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        App::new(&egui::Context::default(), Some(&stored), None)
    }

    #[test]
    fn the_old_theme_setting_keeps_its_look_and_a_first_run_gets_verre_with_ribbons() {
        let fresh = app_with(&[]);
        assert_eq!(fresh.theme, ThemeKind::Verre);
        assert_eq!(fresh.links, LinkStyle::Ribbons);
        assert!(fresh.transparent && fresh.look.translucent);
        assert_eq!(app_with(&[("theme", "dark")]).theme, ThemeKind::Verre);
        assert_eq!(app_with(&[("theme", "light")]).theme, ThemeKind::Clair);
        // What is saved reads back the same.
        let mut app = app_with(&[("theme", "graphite"), ("code_tone", "light"), ("link_style", "lines"), ("transparent_window", "false")]);
        assert_eq!((app.theme, app.code_tone, app.links, app.transparent), (ThemeKind::Graphite, CodeTone::Light, LinkStyle::Lines, false));
        let mut stored = Stored::default();
        eframe::App::save(&mut app, &mut stored);
        let again = App::new(&egui::Context::default(), Some(&stored), None);
        assert_eq!((again.theme, again.code_tone, again.links, again.transparent), (ThemeKind::Graphite, CodeTone::Light, LinkStyle::Lines, false));
    }

    #[test]
    fn forcing_the_code_tone_changes_the_code_colours_but_not_the_interface() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_facture(dir.path()));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Ardoise);
        let paper = harness.state().look;
        assert!(paper.ui.dark && !paper.doc.dark);
        assert_eq!(harness.state_mut().text_tab_mut().syntax_theme(), paper.syntax);
        harness.state_mut().run(&ctx, Command::SetCodeTone(CodeTone::Dark));
        settle(&mut harness);
        let slate = harness.state().look;
        assert!(slate.ui.dark && slate.doc.dark);
        assert_eq!(slate.ui.window, paper.ui.window);
        assert_ne!(slate.syntax, paper.syntax);
        assert_eq!(harness.state_mut().text_tab_mut().syntax_theme(), slate.syntax);
        // Transparency belongs to Verre alone.
        harness.state_mut().run(&ctx, Command::ToggleTransparency);
        assert!(!harness.state().look.translucent);
        harness.state_mut().set_theme(&ctx, ThemeKind::Verre);
        assert!(!harness.state().look.translucent, "turned off, it stays off");
        harness.state_mut().run(&ctx, Command::ToggleTransparency);
        assert!(harness.state().look.translucent);
    }

    #[test]
    fn the_size_buttons_change_the_code_size_in_every_theme() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_facture(dir.path()));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        for kind in ThemeKind::ALL {
            harness.state_mut().set_theme(&ctx, kind);
            harness.run_steps(2);
            let before = harness.state().code_size;
            let larger = if kind == ThemeKind::Porcelaine { "Agrandir le texte" } else { "Agrandir le texte (Ctrl++)" };
            let smaller = if kind == ThemeKind::Porcelaine { "Réduire le texte" } else { "Réduire le texte (Ctrl+-)" };
            harness.get_by_label(larger).click();
            harness.run_steps(3);
            assert_eq!(harness.state().code_size, before + 1.0, "{kind:?}");
            harness.get_by_label(smaller).click();
            harness.get_by_label(smaller).click();
            harness.run_steps(3);
            assert_eq!(harness.state().code_size, before - 1.0, "{kind:?}");
            harness.get_by_label("Taille du code").click();
            harness.run_steps(3);
            assert_eq!(harness.state().code_size, DEFAULT_CODE_SIZE, "{kind:?}");
        }
    }

    #[test]
    fn ctrl_and_the_wheel_size_the_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_facture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.hover_at(pos2(400.0, 400.0));
        let wheel = |delta: f32| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: vec2(0.0, delta),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::COMMAND,
            source: egui::MouseWheelSource::Unknown,
        };
        let before = harness.state().code_size;
        harness.event(wheel(1.0));
        harness.run_steps(2);
        assert_eq!(harness.state().code_size, before + 1.0);
        harness.event(wheel(-2.0));
        harness.run_steps(2);
        assert_eq!(harness.state().code_size, before - 1.0);
        assert_eq!(harness.state_mut().text_tab_mut().scroll_position(), 0.0, "Ctrl+wheel does not scroll");
    }

    #[test]
    fn arrows_show_for_the_current_block_and_for_the_one_under_the_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_facture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.remove_cursor();
        harness.run_steps(2);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.selected_block(), Some(0));
        assert!(tab.arrow_center(0, Side::Right).is_some());
        assert!(tab.arrow_center(1, Side::Right).is_none(), "only the current block has arrows");
        // Under the pointer, another block shows its arrows, and they work. The second block is
        // a line added on the right.
        let line = tab.block_start(1, Side::Right);
        let at = tab.line_center(Side::Right, line).unwrap();
        harness.hover_at(at);
        harness.run_steps(2);
        let center = harness.state_mut().text_tab_mut().arrow_center(1, Side::Right).expect("hovered block shows its arrows");
        harness.hover_at(center);
        harness.run_steps(2);
        harness.drag_at(center);
        harness.run_steps(1);
        harness.drop_at(center);
        settle(&mut harness);
        assert!(harness.state_mut().text_tab_mut().is_dirty(), "the hovered block's arrow sends it");
    }

    #[test]
    fn pushing_a_block_then_undoing_restores_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fixture(dir.path());
        let mut harness = harness_for(paths.clone());
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().text_tab_mut().push_block(&ctx, 0, Side::Left);
        settle(&mut harness);
        assert!(harness.state_mut().text_tab_mut().is_dirty());
        harness.state_mut().text_tab_mut().undo(&ctx);
        settle(&mut harness);
        assert!(!harness.state_mut().text_tab_mut().is_dirty());
        assert!(harness.state_mut().text_tab_mut().can_redo());
    }

    #[test]
    fn saved_push_reaches_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fixture(dir.path());
        let mut harness = harness_for(paths.clone());
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().text_tab_mut().push_block(&ctx, 0, Side::Left);
        harness.state_mut().text_tab_mut().save().unwrap();
        assert!(std::fs::read_to_string(&paths[0]).unwrap().contains("_ASSERTE"));
        assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), RIGHT);
    }

    #[test]
    fn clicking_an_arrow_copies_the_block_and_the_menus_open() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let blocks_before = harness.state_mut().text_tab_mut().block_count();

        // The first block is selected on opening: its left arrow sits on the right pane.
        let ctx = harness.ctx.clone();
        let center = harness.state_mut().text_tab_mut().arrow_center(0, Side::Left).unwrap();
        ctx.request_repaint();
        harness.hover_at(center);
        harness.run_steps(2);
        harness.drag_at(center);
        harness.run_steps(1);
        harness.drop_at(center);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.is_dirty(), "the arrow click did nothing");
        assert_eq!(tab.block_count(), blocks_before - 1);

        harness.remove_cursor();
        harness.get_by_label("Menu").click();
        harness.run_steps(4);
        save(&mut harness, "menu.png");
        harness.get_by_label("Palette de commandes…");
        harness.get_by_label("Apparence").hover();
        harness.run_steps(6);
        save(&mut harness, "submenu.png");
        harness.get_by_label("Clair");
    }

    #[test]
    fn the_palette_finds_and_runs_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::P);
        harness.run_steps(3);
        assert!(harness.state().palette.is_some());
        save(&mut harness, "palette.png");
        harness.event(egui::Event::Text("ignorer casse".to_owned()));
        harness.run_steps(3);
        harness.key_press(Key::Enter);
        harness.run_steps(3);
        assert!(harness.state().palette.is_none());
        assert!(harness.state().diff_options.ignore_case);
    }

    #[test]
    fn a_status_bar_toggle_lights_up_and_applies() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.get_by_label("Ignorer la casse").click();
        harness.run_steps(3);
        assert!(harness.state().diff_options.ignore_case);
        harness.get_by_label("Espaces : tous comparés").click();
        harness.run_steps(3);
        assert_eq!(harness.state().diff_options.whitespace, Whitespace::IgnoreTrailing);
        save(&mut harness, "status-options.png");
    }

    #[test]
    fn wheel_and_touchpad_scroll_several_rows_quickly() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("a.txt");
        let right = dir.path().join("b.txt");
        let text: String = (0..500).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&left, &text).unwrap();
        std::fs::write(&right, &text).unwrap();
        let mut harness = harness_for([left, right]);
        harness.step();
        settle(&mut harness);
        harness.hover_at(pos2(300.0, 400.0));
        harness.run_steps(2);

        let wheel = |unit, delta| egui::Event::MouseWheel {
            unit,
            delta,
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
            source: egui::MouseWheelSource::Unknown,
        };
        harness.event(wheel(egui::MouseWheelUnit::Line, vec2(0.0, -1.0)));
        harness.run_steps(12);
        let after_notch = harness.state_mut().text_tab_mut().scroll_position();
        assert!(after_notch >= 60.0, "one notch moved {after_notch} px");

        harness.event(wheel(egui::MouseWheelUnit::Point, vec2(0.0, -100.0)));
        harness.step();
        let after_touchpad = harness.state_mut().text_tab_mut().scroll_position();
        assert_eq!(after_touchpad, after_notch + 100.0, "a touchpad applies at once");
    }

    #[test]
    fn the_code_size_changes_rows_and_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::Equals);
        harness.run_steps(3);
        assert_eq!(harness.state().code_size, DEFAULT_CODE_SIZE + 1.0);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::Num0);
        harness.run_steps(3);
        assert_eq!(harness.state().code_size, DEFAULT_CODE_SIZE);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::ChangeCodeSize(500.0));
        assert_eq!(harness.state().code_size, MAX_CODE_SIZE);
    }

    #[test]
    fn the_font_dialog_lists_monospace_families() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::OpenFontDialog);
        for _ in 0..500 {
            harness.step();
            let ready = harness.state().font_dialog.as_ref().is_some_and(|d| d.families.is_some());
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        harness.run_steps(3);
        save(&mut harness, "font-dialog.png");
        let dialog = harness.state().font_dialog.as_ref().unwrap();
        assert!(dialog.families.as_ref().is_some_and(|f| !f.is_empty()));
    }

    #[test]
    fn closing_a_modified_tab_asks_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().text_tab_mut().push_block(&ctx, 0, Side::Left);
        harness.state_mut().request_close(0);
        harness.run_steps(3);
        assert_eq!(harness.state().tabs.len(), 1);
        assert_eq!(harness.state().confirm, Some(AfterConfirm::CloseTab(0)));
    }

    /// Two files where the right one holds a block of 60 extra lines.
    fn write_big_block_fixture(dir: &Path) -> [std::path::PathBuf; 2] {
        let line = |i: usize| format!("    let value_{i} = compute(input_{i}, {i});\n");
        let head: String = (0..40).map(line).collect();
        let block: String = (0..60).map(|i| format!("    extra_call_{i}(argument);\n")).collect();
        let tail: String = (40..90).map(line).collect();
        let left = dir.join("left.rs");
        let right = dir.join("right.rs");
        std::fs::write(&left, format!("{head}{tail}")).unwrap();
        std::fs::write(&right, format!("{head}{block}{tail}")).unwrap();
        [left, right]
    }

    #[test]
    fn a_big_block_leaves_no_blank_and_the_panes_meet_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_big_block_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        tab.show_block(0);
        let [left, right] = tab.pane_positions();
        assert_eq!(left, right, "the block is brought where the panes agree");
        harness.run_steps(3);
        save(&mut harness, "big-block.png");

        // Past the block, the right pane stays exactly its 60 lines ahead.
        harness.hover_at(pos2(300.0, 400.0));
        harness.run_steps(2);
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -2000.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
            source: egui::MouseWheelSource::Unknown,
        });
        harness.run_steps(3);
        let [left, right] = harness.state_mut().text_tab_mut().pane_positions();
        let row = theme::line_height(DEFAULT_CODE_SIZE) as f64;
        assert!(right > left, "{left} {right}");
        assert!((right - left - 60.0 * row).abs() < 1.0 || right - left < 60.0 * row, "{left} {right}");

        // On request, facing lines stay level instead.
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::ToggleAligned);
        settle(&mut harness);
        harness.state_mut().text_tab_mut().show_block(0);
        harness.run_steps(3);
        save(&mut harness, "big-block-aligned.png");
        let [left, right] = harness.state_mut().text_tab_mut().pane_positions();
        assert_eq!(left, right, "aligned panes scroll together");
    }

    #[test]
    fn ctrl_arrows_navigate_and_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().selected_block(), Some(0));

        harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowDown);
        harness.run_steps(3);
        assert_eq!(harness.state_mut().text_tab_mut().selected_block(), Some(1));
        harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowUp);
        harness.run_steps(3);
        assert_eq!(harness.state_mut().text_tab_mut().selected_block(), Some(0));

        let blocks = harness.state_mut().text_tab_mut().block_count();
        harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowLeft);
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), blocks - 1);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), blocks);
        assert!(!harness.state_mut().text_tab_mut().is_dirty());
    }

    #[test]
    fn plain_arrows_scroll_without_a_caret() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_big_block_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let before = harness.state_mut().text_tab_mut().scroll_position();
        harness.key_press(Key::ArrowDown);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.scroll_position() > before);
        assert_eq!(tab.block_count(), 1);
        assert!(!tab.is_dirty());
    }

    #[test]
    fn typing_edits_the_text_and_the_comparison_follows() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fixture(dir.path());
        let mut harness = harness_for(paths.clone());
        harness.step();
        settle(&mut harness);
        let blocks = harness.state_mut().text_tab_mut().block_count();

        // Line 1 is identical on both sides: typing there creates a difference.
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 0, 0);
        harness.run_steps(2);
        harness.event(egui::Event::Text("// ".to_owned()));
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Left, 0), "// #include <string>");
        assert_eq!(tab.block_count(), blocks + 1, "the new difference appears");
        assert!(tab.is_dirty());
        assert_eq!(tab.caret_position(), Some((0, 3)));

        // Erasing it makes the two sides equal again.
        for _ in 0..3 {
            harness.key_press(Key::Backspace);
            harness.run_steps(2);
        }
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Left, 0), "#include <string>");
        assert_eq!(tab.block_count(), blocks);

        harness.event(egui::Event::Text("x".to_owned()));
        harness.run_steps(2);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::S);
        harness.run_steps(2);
        assert!(std::fs::read_to_string(&paths[0]).unwrap().starts_with("x#include"));
    }

    #[test]
    fn arrows_move_the_caret_and_enter_keeps_the_indent() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 6, 4);
        harness.run_steps(2);
        harness.key_press(Key::ArrowRight);
        harness.run_steps(2);
        assert_eq!(harness.state_mut().text_tab_mut().caret_position(), Some((6, 5)));
        harness.key_press(Key::ArrowDown);
        harness.run_steps(2);
        assert_eq!(harness.state_mut().text_tab_mut().caret_position(), Some((7, 5)));
        let blocks = harness.state_mut().text_tab_mut().block_count();
        assert_eq!(blocks, harness.state_mut().text_tab_mut().block_count(), "no transfer");

        harness.key_press(Key::End);
        harness.run_steps(2);
        harness.key_press(Key::Enter);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Left, 8), "    ");
        assert_eq!(tab.caret_position(), Some((8, 4)));
    }

    #[test]
    fn a_burst_of_typing_is_undone_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.state_mut().text_tab_mut().put_caret(Side::Right, 0, 0);
        harness.run_steps(2);
        for letter in ["a", "b", "c"] {
            harness.event(egui::Event::Text(letter.to_owned()));
            harness.run_steps(2);
        }
        assert!(harness.state_mut().text_tab_mut().line_text(Side::Right, 0).starts_with("abc"));
        harness.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Right, 0), "#include <string>");
        assert!(!tab.is_dirty());
        assert_eq!(tab.caret_position(), Some((0, 0)));
    }

    #[test]
    fn the_tab_key_stays_in_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 0, 0);
        harness.run_steps(2);
        harness.key_press(Key::Tab);
        harness.run_steps(3);
        harness.event(egui::Event::Text("x".to_owned()));
        harness.run_steps(3);
        assert!(harness.state_mut().text_tab_mut().line_text(Side::Left, 0).starts_with("    x"));
    }

    #[test]
    fn pasting_and_selecting_replace_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 1, 0);
        harness.run_steps(2);
        harness.event(egui::Event::Paste("one\ntwo".to_owned()));
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Left, 1), "one");
        assert_eq!(tab.line_text(Side::Left, 2), "two");
        assert_eq!(tab.caret_position(), Some((2, 3)));
    }

    #[test]
    fn the_line_being_edited_stays_visible_under_a_filter() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        tab.filter_query = "unichar".to_owned();
        tab.apply_filter();
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        let line = (0..20).find(|&l| tab.line_text(Side::Left, l).contains("unichar")).unwrap();
        tab.put_caret(Side::Left, line as u32, 0);
        harness.run_steps(2);
        // The block no longer holds the word, yet typing goes on.
        for _ in 0..3 {
            harness.key_press(Key::Delete);
            harness.run_steps(1);
        }
        harness.run_steps(3);
        assert!(harness.state_mut().text_tab_mut().has_caret());
    }

    #[test]
    fn alt_arrows_scroll_without_leaving_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_big_block_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let before = harness.state_mut().text_tab_mut().scroll_position();
        harness.key_press_modifiers(Modifiers::ALT, Key::ArrowDown);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.scroll_position() > before);
        assert_eq!(tab.selected_block(), Some(0));
        assert_eq!(tab.block_count(), 1);
    }

    #[test]
    fn a_held_arrow_transfers_only_one_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let blocks = harness.state_mut().text_tab_mut().block_count();
        // The key stays down: the window system then sends repeats of the same press.
        harness.key_down_modifiers(Modifiers::COMMAND, Key::ArrowLeft);
        settle(&mut harness);
        for _ in 0..3 {
            harness.key_down_modifiers(Modifiers::COMMAND, Key::ArrowLeft);
            harness.run_steps(2);
        }
        harness.key_up_modifiers(Modifiers::COMMAND, Key::ArrowLeft);
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), blocks - 1);
    }

    #[test]
    fn an_arrow_first_shows_a_block_that_is_out_of_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_big_block_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.hover_at(pos2(300.0, 400.0));
        harness.run_steps(2);
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, 100_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
            source: egui::MouseWheelSource::Unknown,
        });
        harness.run_steps(3);
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -100_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: Modifiers::NONE,
            source: egui::MouseWheelSource::Unknown,
        });
        harness.run_steps(3);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowLeft);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.block_count(), 1, "nothing is sent unseen");
        assert!(!tab.is_dirty());
        harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowLeft);
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 0);
    }

    #[test]
    fn the_filter_keeps_only_matching_blocks_and_navigation_follows() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let total = harness.state_mut().text_tab_mut().block_count();

        harness.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::F);
        harness.run_steps(3);
        harness.event(egui::Event::Text("getLine".to_owned()));
        harness.run_steps(4);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.filter_counts(), Some((1, total)));
        let only = tab.selected_block();
        assert!(only.is_some());
        save(&mut harness, "filter.png");

        harness.remove_cursor();
        harness.key_press(Key::Escape);
        harness.run_steps(4);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.filter_counts(), None, "Escape clears the filter");

        harness.get_by_label("Mots entiers").click();
        harness.run_steps(3);
        assert!(harness.state_mut().text_tab_mut().filter_options.whole_words);
    }

    #[test]
    fn an_invalid_regex_shows_an_error_and_filters_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        tab.filter_options.regex = true;
        tab.filter_query = "(".to_owned();
        tab.apply_filter();
        assert!(tab.filter_error().is_some());
        assert_eq!(tab.filter_counts(), None);
    }

    #[test]
    fn the_filter_survives_a_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        let tab = harness.state_mut().text_tab_mut();
        tab.filter_query = "current".to_owned();
        tab.apply_filter();
        let (kept, _) = tab.filter_counts().unwrap();
        assert!(kept >= 2);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        let block = tab.selected_block().unwrap();
        tab.push_block(&ctx, block, Side::Left);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.filter_counts().unwrap().0, kept - 1);
        assert!(tab.selected_block().is_some());
    }

    pub(super) fn type_text(harness: &mut Harness<'_, App>, text: &str) {
        harness.event(egui::Event::Text(text.to_owned()));
        harness.run_steps(4);
    }

    fn line_holding(tab: &TextTab, side: Side, needle: &str) -> u32 {
        (0..tab.line_count(side)).find(|&line| tab.line_text(side, line).contains(needle)).unwrap() as u32
    }

    #[test]
    fn finding_steps_through_the_occurrences_of_either_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::F);
        harness.run_steps(3);
        // Only the right side holds this word, twice, in text the filter would not look at alone.
        type_text(&mut harness, "wchar_t");
        let tab = harness.state_mut().text_tab_mut();
        let first = line_holding(tab, Side::Right, "pchOtherData");
        assert_eq!(tab.filter_counts(), None, "finding folds nothing");
        assert_eq!(tab.hidden_lines(Side::Right), 0);
        assert_eq!(tab.find_counts(), Some((Some(1), 2)));
        assert_eq!(tab.selection_text(), Some((Side::Right, "wchar_t".to_owned())));
        assert_eq!(tab.caret_position().unwrap().0, first);
        save(&mut harness, "find.png");

        // Enter goes on and stays in the field: the text must not take it as a new line.
        harness.key_press(Key::Enter);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.find_counts(), Some((Some(2), 2)));
        assert_eq!(tab.caret_position().unwrap().0, first + 1);
        assert!(!tab.is_dirty());
        type_text(&mut harness, "*");
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.filter_query, "wchar_t*", "the field kept the keyboard");
        assert_eq!(tab.find_counts(), Some((Some(2), 2)));

        // F3 wraps around the end, Shift+F3 comes back, Shift+Enter too.
        harness.key_press(Key::F3);
        harness.run_steps(3);
        assert_eq!(harness.state_mut().text_tab_mut().find_counts(), Some((Some(1), 2)));
        harness.key_press_modifiers(Modifiers::SHIFT, Key::F3);
        harness.run_steps(3);
        assert_eq!(harness.state_mut().text_tab_mut().find_counts(), Some((Some(2), 2)));
        harness.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.find_counts(), Some((Some(1), 2)));
        assert_eq!(tab.selection_text(), Some((Side::Right, "wchar_t*".to_owned())));
        assert!(!tab.is_dirty());

        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        settle(&mut harness);
        save(&mut harness, "find-light.png");
    }

    #[test]
    fn finding_reaches_unchanged_text_far_away_and_counts_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_big_block_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::F);
        harness.run_steps(3);
        // A phrase, spaces included, on a line both sides hold: one occurrence, not two.
        type_text(&mut harness, "value_85 = COMPUTE");
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.find_counts(), Some((Some(1), 1)));
        assert_eq!(tab.selection_text(), Some((Side::Left, "value_85 = compute".to_owned())));
        assert_eq!(tab.caret_position().unwrap().0, 85);
        assert!(tab.scroll_position() > 0.0);
        assert!(tab.caret_in_view(), "the view follows the occurrence");

        // From the right pane, the same occurrence is shown there, 60 lines further down.
        tab.put_caret(Side::Right, 0, 0);
        harness.run_steps(2);
        harness.key_press(Key::F3);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.selection_text(), Some((Side::Right, "value_85 = compute".to_owned())));
        assert_eq!(tab.caret_position().unwrap().0, 145);
        assert!(tab.caret_in_view());

        // Nothing found: the counter says so and the caret stays.
        type_text(&mut harness, "zzz");
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.find_counts(), Some((None, 0)));
        assert_eq!(tab.caret_position().unwrap().0, 145);
    }

    #[test]
    fn one_field_finds_or_filters_and_its_funnel_switches() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let total = harness.state_mut().text_tab_mut().block_count();
        harness.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::F);
        harness.run_steps(3);
        type_text(&mut harness, "getLine");
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.filter_counts(), Some((1, total)));
        assert_eq!(tab.find_counts(), None);
        assert!(tab.hidden_lines(Side::Left) > 0);

        harness.get_by_label("Filtrer les blocs (Ctrl+Maj+F)").click();
        harness.run_steps(4);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.search_mode(), SearchMode::Find);
        assert_eq!(tab.filter_counts(), None);
        assert_eq!(tab.hidden_lines(Side::Left), 0, "the same words are now only looked for");
        assert!(tab.find_counts().is_some_and(|(current, count)| current == Some(1) && count >= 3));

        // Ctrl+F from the text looks for what is selected there.
        harness.key_press(Key::Escape);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        let line = line_holding(tab, Side::Left, "nSearchLen(");
        tab.put_caret(Side::Left, line, 4);
        harness.run_steps(2);
        harness.key_press_modifiers(Modifiers::SHIFT, Key::End);
        harness.run_steps(2);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::F);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.filter_query, "const size_t nSearchLen(strText.length());");
        assert_eq!(tab.find_counts(), Some((Some(1), 1)));
    }

    #[test]
    fn ctrl_g_goes_to_a_line_of_the_side_asked_for() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_big_block_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::G);
        harness.run_steps(3);
        assert!(harness.state().go_to_line.is_some());
        type_text(&mut harness, "d:120");
        save(&mut harness, "go-to-line.png");
        harness.key_press(Key::Enter);
        harness.run_steps(4);
        assert!(harness.state().go_to_line.is_none());
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.caret_side(), Some(Side::Right));
        assert_eq!(tab.caret_position(), Some((119, 0)));
        assert!(tab.caret_in_view());
        let [left, right] = tab.pane_positions();
        assert!(left > 0.0 && right > left, "the left pane follows: {left} {right}");

        // Without a side: the one holding the caret. Past the end: the last line.
        harness.key_press_modifiers(Modifiers::COMMAND, Key::G);
        harness.run_steps(3);
        type_text(&mut harness, "99 999");
        harness.key_press(Key::Enter);
        harness.run_steps(4);
        let tab = harness.state_mut().text_tab_mut();
        let last = tab.line_count(Side::Right) as u32 - 1;
        assert_eq!((tab.caret_side(), tab.caret_position()), (Some(Side::Right), Some((last, 0))));
        assert!(tab.caret_in_view());

        // The side can be clicked; what is not a number keeps the prompt open.
        harness.key_press_modifiers(Modifiers::COMMAND, Key::G);
        harness.run_steps(3);
        type_text(&mut harness, "x");
        harness.key_press(Key::Enter);
        harness.run_steps(3);
        assert!(harness.state().go_to_line.as_ref().is_some_and(|prompt| prompt.invalid));
        harness.key_press(Key::Backspace);
        harness.run_steps(2);
        type_text(&mut harness, "5");
        harness.get_by_label("Côté gauche").click();
        harness.run_steps(3);
        harness.key_press(Key::Enter);
        harness.run_steps(4);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!((tab.caret_side(), tab.caret_position()), (Some(Side::Left), Some((4, 0))));
        assert_eq!(tab.pane_positions(), [0.0, 0.0]);
        assert!(!tab.is_dirty());

        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::G);
        harness.run_steps(3);
        save(&mut harness, "go-to-line-light.png");
        harness.key_press(Key::Escape);
        harness.run_steps(3);
        assert!(harness.state().go_to_line.is_none());
        assert!(harness.state().palette_entries().iter().any(|entry| entry.command == Command::GoToLine));
        harness.get_by_label("Menu").click();
        harness.run_steps(4);
        harness.get_by_label("Aller à la ligne…");
        harness.get_by_label("Rechercher…");
    }

    #[test]
    fn line_targets_name_a_side_or_leave_it_open() {
        assert_eq!(parse_line_target(" 120 "), Some((None, 120)));
        assert_eq!(parse_line_target("d:120"), Some((Some(Side::Right), 120)));
        assert_eq!(parse_line_target("G 1 204"), Some((Some(Side::Left), 1204)));
        assert_eq!(parse_line_target("d12"), Some((Some(Side::Right), 12)));
        assert_eq!(parse_line_target("d:"), None);
        assert_eq!(parse_line_target("douze"), None);
        assert_eq!(parse_line_target(""), None);
    }

    #[test]
    fn differences_only_folds_unchanged_text_but_never_the_line_being_edited() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_big_block_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.key_press_modifiers(Modifiers::ALT, Key::D);
        harness.run_steps(3);
        assert!(harness.state().context_only);
        let tab = harness.state_mut().text_tab_mut();
        // 90 lines on the left, 3 kept on each side of the block.
        assert_eq!(tab.hidden_lines(Side::Left), 84);
        assert_eq!(tab.hidden_lines(Side::Right), 84);
        assert!(tab.line_is_shown(Side::Left, 37) && !tab.line_is_shown(Side::Left, 36));
        assert!(tab.line_is_shown(Side::Right, 102) && !tab.line_is_shown(Side::Right, 103));
        assert_eq!(tab.selected_block(), Some(0));
        save(&mut harness, "context.png");

        // A folded line asked for by its number comes back with its own context, and stays
        // while it is typed in, even when the edit is undone by hand.
        tab_go(&mut harness, Side::Left, 80);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.line_is_shown(Side::Left, 79) && tab.line_is_shown(Side::Left, 76));
        assert!(!tab.line_is_shown(Side::Left, 75));
        assert!(tab.caret_in_view());
        type_text(&mut harness, "x");
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.line_text(Side::Left, 79).starts_with('x'));
        assert_eq!(tab.block_count(), 2);
        assert!(tab.line_is_shown(Side::Left, 79) && tab.caret_in_view());
        harness.key_press(Key::Backspace);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.block_count(), 1);
        assert!(tab.has_caret() && tab.line_is_shown(Side::Left, 79) && tab.caret_in_view());

        // A click on a separator unfolds its run, which then survives an edit.
        tab_go(&mut harness, Side::Right, 1);
        let tab = harness.state_mut().text_tab_mut();
        let before = tab.hidden_lines(Side::Left);
        let separator = tab.first_separator(Side::Left).unwrap();
        harness.hover_at(separator);
        harness.run_steps(2);
        harness.drag_at(separator);
        harness.run_steps(1);
        harness.drop_at(separator);
        harness.run_steps(4);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.hidden_lines(Side::Left) < before, "the click unfolded nothing");
        assert!(tab.line_is_shown(Side::Left, 20));
        harness.remove_cursor();
        type_text(&mut harness, "y");
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.line_text(Side::Right, 0).starts_with('y'));
        assert!(tab.line_is_shown(Side::Left, 20) && tab.line_is_shown(Side::Right, 20));

        // No context at all: only the blocks and the line being edited.
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::SetContextLines(0));
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert!(!tab.line_is_shown(Side::Left, 39) && !tab.line_is_shown(Side::Left, 20));
        assert!(tab.line_is_shown(Side::Right, 0) && tab.line_is_shown(Side::Right, 40));

        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        harness.state_mut().run(&ctx, Command::SetContextLines(3));
        settle(&mut harness);
        save(&mut harness, "context-light.png");
        harness.get_by_label("Différences seules (Alt+D)").click();
        harness.run_steps(3);
        assert!(!harness.state().context_only);
        assert_eq!(harness.state_mut().text_tab_mut().hidden_lines(Side::Left), 0);
    }

    fn tab_go(harness: &mut Harness<'_, App>, side: Side, number: usize) {
        harness.state_mut().text_tab_mut().go_to_line(Some(side), number);
        harness.run_steps(3);
    }

    #[test]
    fn the_filter_picks_the_blocks_and_the_context_shows_their_surroundings() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_fixture(dir.path()));
        harness.step();
        settle(&mut harness);
        let total = harness.state_mut().text_tab_mut().block_count();
        let tab = harness.state_mut().text_tab_mut();
        tab.filter_query = "getLine".to_owned();
        tab.apply_filter();
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        let filtered = [tab.hidden_lines(Side::Left), tab.hidden_lines(Side::Right)];
        let kept_line = line_holding(tab, Side::Right, "pchOtherData") as usize;
        let other_block = line_holding(tab, Side::Right, "_ASSERTE") as usize;
        assert!(tab.line_is_shown(Side::Right, kept_line));
        assert!(!tab.line_is_shown(Side::Right, kept_line + 3), "a filter alone shows no context");

        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::ToggleContext);
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.filter_counts(), Some((1, total)), "the filter still decides the blocks");
        assert!(tab.line_is_shown(Side::Right, kept_line) && tab.line_is_shown(Side::Right, kept_line + 3));
        assert!(!tab.line_is_shown(Side::Right, other_block), "a block the filter hides stays hidden");
        assert!(tab.hidden_lines(Side::Left) < filtered[0] && tab.hidden_lines(Side::Right) < filtered[1]);
        assert!(tab.selected_block().is_some());
        save(&mut harness, "filter-context.png");

        // Without the filter the other blocks come back, each with its context.
        let tab = harness.state_mut().text_tab_mut();
        tab.filter_query.clear();
        tab.apply_filter();
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.line_is_shown(Side::Right, other_block));
        assert!(!tab.line_is_shown(Side::Right, 0), "text far from any block is still folded");
    }

    /// Thousands of random edits on accented code, in every display mode: nothing may panic.
    fn torture(seed: u64, steps: usize, aligned: bool, wrap: bool, filter: &str, context: bool) {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("a.php");
        let right = dir.path().join("b.php");
        let line = |i: usize| match i % 6 {
            0 => format!("    public static array $list_{i} = [1 => 'Chèque', 2 => 'Prélèvement'];\n"),
            1 => format!("    // Facture créée à l'échéance numéro {i}, déjà réglée\n"),
            2 => "\n".to_owned(),
            3 => format!("    $été = \"日本語 {i}\"; /* commentaire accentué */\n"),
            4 => format!("\t\tif ($montant > {i}) {{ return 'prépayé'; }}\n"),
            _ => format!("{}\n", "é".repeat(90)),
        };
        let left_text: String = (0..60).map(line).collect();
        let right_text: String = (0..60).filter(|i| i % 7 != 3).map(|i| line(i + (i % 5 == 0) as usize)).collect();
        std::fs::write(&left, left_text).unwrap();
        std::fs::write(&right, right_text).unwrap();
        let mut harness = harness_for([left, right]);
        harness.set_size(vec2(900.0, 500.0));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        if aligned {
            harness.state_mut().run(&ctx, Command::ToggleAligned);
        }
        if !wrap {
            harness.state_mut().run(&ctx, Command::ToggleWrap);
        }
        harness.state_mut().text_tab_mut().filter_query = filter.to_owned();
        if context {
            // Folds everywhere, and occurrences to find again after each edit.
            harness.state_mut().run(&ctx, Command::ToggleContext);
            harness.state_mut().text_tab_mut().apply_search();
        } else {
            harness.state_mut().text_tab_mut().apply_filter();
        }
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 3, 0);
        harness.step();

        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut random = move |limit: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % limit
        };
        let keys = [
            Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End,
            Key::Backspace, Key::Delete, Key::Enter, Key::Tab, Key::PageUp, Key::PageDown,
        ];
        for _ in 0..steps {
            match random(12) {
                0..=3 => {
                    let text = ["é", "a", "日", " ", "çà", "x"][random(6) as usize];
                    harness.event(egui::Event::Text(text.to_owned()));
                }
                4..=8 => harness.key_press(keys[random(keys.len() as u64) as usize]),
                // Tab can reach a tab's close button, and Enter press it: that session is over.
                _ if harness.state().tabs.is_empty() => return,
                9 => harness.key_press_modifiers(Modifiers::SHIFT, keys[random(6) as usize]),
                10 => match random(7) {
                    0 => harness.key_press_modifiers(Modifiers::COMMAND, Key::Z),
                    1 => harness.key_press_modifiers(Modifiers::COMMAND, Key::Y),
                    2 => harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowDown),
                    3 => harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowLeft),
                    4 => harness.event(egui::Event::Paste("deux\nlignes é\n".to_owned())),
                    5 => harness.event(egui::Event::Cut),
                    _ => harness.key_press_modifiers(Modifiers::COMMAND, Key::A),
                },
                _ => {
                    if !matches!(harness.state().tabs.get(harness.state().active), Some(Tab::Text(_))) {
                        return;
                    }
                    if random(2) == 0 {
                        let side = if random(2) == 0 { Side::Left } else { Side::Right };
                        harness.state_mut().text_tab_mut().put_caret(side, random(70) as u32, 0);
                    } else {
                        // A real click, wherever it lands.
                        let at = pos2(random(900) as f32, 112.0 + random(350) as f32);
                        harness.hover_at(at);
                        harness.step();
                        harness.drag_at(at);
                        harness.step();
                        harness.drop_at(at);
                    }
                }
            }
            harness.step();
        }
    }

    /// `BIOMERGE_TORTURE_SEEDS=40` runs a longer session of each mode.
    fn seeds() -> u64 {
        std::env::var("BIOMERGE_TORTURE_SEEDS").ok().and_then(|n| n.parse().ok()).unwrap_or(1)
    }

    #[test]
    fn random_editing_never_panics() {
        (0..seeds()).for_each(|seed| torture(1 + seed * 10, 1500, false, true, "", false));
    }

    #[test]
    fn random_editing_never_panics_aligned() {
        (0..seeds()).for_each(|seed| torture(2 + seed * 10, 1000, true, true, "", false));
    }

    #[test]
    fn random_editing_never_panics_without_wrapping() {
        (0..seeds()).for_each(|seed| torture(3 + seed * 10, 1000, false, false, "", false));
    }

    #[test]
    fn random_editing_never_panics_under_a_filter() {
        (0..seeds()).for_each(|seed| torture(4 + seed * 10, 1000, false, true, "facture", false));
    }

    #[test]
    fn random_editing_never_panics_with_differences_only_and_a_search() {
        (0..seeds()).for_each(|seed| torture(5 + seed * 10, 1000, false, true, "facture", true));
        (0..seeds()).for_each(|seed| torture(6 + seed * 10, 600, true, false, "é", true));
    }

    #[test]
    fn a_narrow_status_bar_never_overlaps_its_icons() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(a.path().join("left-only.txt"), "x").unwrap();
        std::fs::write(b.path().join("right-only.txt"), "y").unwrap();
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.set_size(vec2(560.0, 420.0));
        harness.step();
        settle(&mut harness);
        save(&mut harness, "narrow.png");
    }

    #[test]
    fn folder_comparison_renders() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for (root, content) in [(a.path(), LEFT), (b.path(), RIGHT)] {
            std::fs::create_dir_all(root.join("src/engine")).unwrap();
            std::fs::write(root.join("src/engine/diff.cpp"), content).unwrap();
            std::fs::write(root.join("README.md"), "same").unwrap();
        }
        std::fs::write(a.path().join("left-only.txt"), "x").unwrap();
        std::fs::write(b.path().join("right-only.txt"), "y").unwrap();
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);
        match harness.state_mut().tabs.get_mut(0) {
            Some(Tab::Folder(tab)) => tab.set_all_expanded(true),
            _ => panic!("a folder tab is expected"),
        }
        harness.run_steps(3);
        save(&mut harness, "folders.png");
    }

    // --- Folder actions ---------------------------------------------------------------------

    fn folder_tab<'h>(harness: &'h mut Harness<'_, App>) -> &'h mut FolderTab {
        let app = harness.state_mut();
        match app.tabs.get_mut(app.active) {
            Some(Tab::Folder(tab)) => tab,
            _ => panic!("a folder tab is expected"),
        }
    }

    /// A click with modifiers held, as the window system sends it.
    fn click_with(harness: &mut Harness<'_, App>, pos: egui::Pos2, modifiers: Modifiers, button: egui::PointerButton) {
        harness.event(egui::Event::ModifiersChanged(modifiers));
        harness.hover_at(pos);
        harness.step();
        harness.event(egui::Event::PointerButton { pos, button, pressed: true, modifiers });
        harness.step();
        harness.event(egui::Event::PointerButton { pos, button, pressed: false, modifiers });
        harness.step();
        harness.event(egui::Event::ModifiersChanged(Modifiers::NONE));
        harness.remove_cursor();
        harness.run_steps(2);
    }

    /// Steps until the folder tab has a plan waiting for its answer.
    fn wait_for_plan(harness: &mut Harness<'_, App>) {
        for _ in 0..500 {
            harness.step();
            if folder_tab(harness).pending_plan().is_some() {
                harness.run_steps(3);
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no plan came");
    }

    /// Left: `same.txt`, `both.txt` (older), `left-only.txt`, `docs/guide.md`.
    /// Right: `same.txt`, `both.txt` (newer, different), `right-only.txt`.
    fn write_folders(left: &Path, right: &Path) {
        std::fs::create_dir_all(left.join("docs")).unwrap();
        std::fs::write(left.join("docs/guide.md"), "# Guide\n").unwrap();
        for root in [left, right] {
            std::fs::write(root.join("same.txt"), "pareil\n").unwrap();
        }
        std::fs::write(left.join("both.txt"), "ancienne version\n").unwrap();
        std::fs::write(right.join("both.txt"), "version plus récente\n").unwrap();
        std::fs::write(left.join("left-only.txt"), "à gauche\n").unwrap();
        std::fs::write(right.join("right-only.txt"), "à droite\n").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(left.join("both.txt")).unwrap().set_modified(old).unwrap();
    }

    #[test]
    fn the_folder_filter_keeps_matching_names_and_shows_its_errors() {
        use egui_kittest::kittest::Queryable;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for root in [a.path(), b.path()] {
            std::fs::create_dir_all(root.join("target/debug")).unwrap();
            std::fs::write(root.join("target/debug/out.rs"), "généré").unwrap();
            std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
            std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
            std::fs::write(root.join("Cargo.lock"), "# lock").unwrap();
        }
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);
        assert_eq!(folder_tab(&mut harness).entry_count(), 6);

        harness.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::F);
        harness.run_steps(3);
        type_text(&mut harness, "*.rs *.toml -target/");
        harness.key_press(Key::Enter);
        harness.run_steps(2);
        settle(&mut harness);
        let tab = folder_tab(&mut harness);
        assert_eq!(tab.entry_status("main.rs"), Some(biomerge_core::dirscan::Status::Same));
        assert!(tab.entry_status("Cargo.toml").is_some());
        assert!(tab.entry_status("Cargo.lock").is_none(), "not included");
        assert!(tab.entry_status("target").is_none(), "excluded folder");
        assert_eq!(harness.state().scan_options.name_filter.source(), "*.rs *.toml -target/");
        save(&mut harness, "folder-filter.png");

        // A pattern still being typed: the error shows, the filter in force stays.
        harness.get_by_label("Filtrer les noms").click();
        harness.run_steps(2);
        type_text(&mut harness, " [a-");
        let app = harness.state();
        assert!(app.folder_filter_error.as_deref().is_some_and(|e| e.contains("crochet")), "{:?}", app.folder_filter_error);
        assert_eq!(app.scan_options.name_filter.source(), "*.rs *.toml -target/");
        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        harness.run_steps(3);
        save(&mut harness, "folder-filter-error-light.png");

        // Escape empties the field and shows everything again.
        harness.key_press(Key::Escape);
        harness.run_steps(2);
        settle(&mut harness);
        assert!(harness.state().scan_options.name_filter.is_empty());
        assert_eq!(folder_tab(&mut harness).entry_count(), 6);
    }

    #[test]
    fn clicks_select_rows_and_copying_to_the_right_plans_confirms_and_refreshes() {
        use biomerge_core::dirscan::Status;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write_folders(a.path(), b.path());
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);

        // A click, then Ctrl+click adds, and Shift+click takes the rows between.
        let row = |harness: &mut Harness<'_, App>, rel: &str| folder_tab(harness).row_center(rel, Side::Left).unwrap();
        let at = row(&mut harness, "both.txt");
        click_with(&mut harness, at, Modifiers::NONE, egui::PointerButton::Primary);
        assert_eq!(folder_tab(&mut harness).selection(), ["both.txt"]);
        let at = row(&mut harness, "left-only.txt");
        click_with(&mut harness, at, Modifiers::COMMAND, egui::PointerButton::Primary);
        assert_eq!(folder_tab(&mut harness).selection(), ["both.txt", "left-only.txt"]);
        let at = row(&mut harness, "same.txt");
        click_with(&mut harness, at, Modifiers::SHIFT, egui::PointerButton::Primary);
        assert_eq!(folder_tab(&mut harness).selection(), ["left-only.txt", "right-only.txt", "same.txt"]);
        let at = row(&mut harness, "both.txt");
        click_with(&mut harness, at, Modifiers::NONE, egui::PointerButton::Primary);
        let at = row(&mut harness, "left-only.txt");
        click_with(&mut harness, at, Modifiers::COMMAND, egui::PointerButton::Primary);
        assert_eq!(folder_tab(&mut harness).selection(), ["both.txt", "left-only.txt"]);

        // The right button opens the menu on the selection.
        let at = row(&mut harness, "both.txt");
        click_with(&mut harness, at, Modifiers::NONE, egui::PointerButton::Secondary);
        harness.run_steps(3);
        save(&mut harness, "folder-menu.png");
        {
            use egui_kittest::kittest::Queryable;
            harness.get_by_label("Copier vers la gauche");
            harness.get_by_label("Supprimer à droite (corbeille)");
        }
        harness.key_press(Key::Escape);
        harness.run_steps(3);

        harness.key_press_modifiers(Modifiers::COMMAND, Key::ArrowRight);
        wait_for_plan(&mut harness);
        let plan = folder_tab(&mut harness).pending_plan().unwrap();
        assert_eq!(plan.totals.creates, 1);
        assert_eq!(plan.totals.overwrites, 1);
        assert_eq!(plan.totals.overwrites_of_newer, 1, "the right one is newer");
        save(&mut harness, "folder-copy-dialog.png");
        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        harness.run_steps(3);
        save(&mut harness, "folder-copy-dialog-light.png");

        {
            use egui_kittest::kittest::Queryable;
            harness.get_by_label("Copier").click();
        }
        harness.run_steps(2);
        settle(&mut harness);
        assert_eq!(std::fs::read_to_string(b.path().join("both.txt")).unwrap(), "ancienne version\n");
        assert_eq!(std::fs::read_to_string(b.path().join("left-only.txt")).unwrap(), "à gauche\n");
        let tab = folder_tab(&mut harness);
        assert_eq!(tab.entry_status("both.txt"), Some(Status::Same));
        assert_eq!(tab.entry_status("left-only.txt"), Some(Status::Same));
        assert_eq!(tab.entry_status("right-only.txt"), Some(Status::RightOnly), "untouched");
        assert!(tab.last_action().is_some_and(|a| a.starts_with("2 éléments copiés vers la droite")), "{:?}", tab.last_action());
        assert!(tab.report.is_none());
        save(&mut harness, "folder-after-copy-light.png");
    }

    #[test]
    fn deleting_for_good_takes_the_secondary_button_and_the_tree_follows() {
        use egui_kittest::kittest::Queryable;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write_folders(a.path(), b.path());
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);
        folder_tab(&mut harness).select_paths(&["docs", "left-only.txt"]);
        harness.key_press(Key::Delete);
        wait_for_plan(&mut harness);
        assert_eq!(folder_tab(&mut harness).delete_side(), Side::Left, "only the left holds them");
        save(&mut harness, "folder-delete-dialog.png");
        harness.get_by_label("Mettre à la corbeille");

        harness.get_by_label("Supprimer définitivement…").click();
        harness.run_steps(3);
        save(&mut harness, "folder-delete-permanent.png");
        harness.get_by_label("Supprimer définitivement").click();
        harness.run_steps(2);
        settle(&mut harness);
        assert!(!a.path().join("left-only.txt").exists());
        assert!(!a.path().join("docs").exists());
        let tab = folder_tab(&mut harness);
        assert!(tab.entry_status("left-only.txt").is_none(), "gone from the tree");
        assert!(tab.entry_status("docs/guide.md").is_none());
        assert!(tab.selection().is_empty(), "stale indices are dropped");
        assert!(tab.last_action().is_some_and(|a| a.contains("supprimés définitivement")));
        assert_eq!(tab.entry_status("both.txt"), Some(biomerge_core::dirscan::Status::Different));
    }

    /// Moves what it is given into its own folder, slowly enough to be cancelled.
    struct FakeTrash {
        bin: std::path::PathBuf,
        delay: Duration,
    }

    impl biomerge_core::fileops::Trasher for FakeTrash {
        fn trash(&self, path: &Path) -> Result<(), String> {
            std::thread::sleep(self.delay);
            let target = self.bin.join(path.file_name().unwrap());
            std::fs::rename(path, target).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn the_trash_is_never_the_real_one_and_a_cancel_stops_between_items() {
        let (a, b, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for i in 0..40 {
            std::fs::write(a.path().join(format!("f{i:02}.txt")), "x").unwrap();
        }
        std::fs::write(b.path().join("other.txt"), "y").unwrap();
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);

        // Without a trash of its own, a test refuses to delete rather than use the real one.
        let all: Vec<String> = (0..40).map(|i| format!("f{i:02}.txt")).collect();
        let names: Vec<&str> = all.iter().map(String::as_str).collect();
        folder_tab(&mut harness).select_paths(&names[..1]);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::FolderDeleteOn(Side::Left));
        wait_for_plan(&mut harness);
        folder_tab(&mut harness).confirm_pending(&ctx, false);
        settle(&mut harness);
        assert!(a.path().join("f00.txt").exists());
        let report = folder_tab(&mut harness).report.take().expect("the failure is reported");
        assert!(report.errors[0].contains("corbeille désactivée"), "{:?}", report.errors);

        folder_tab(&mut harness).set_trasher(std::sync::Arc::new(FakeTrash {
            bin: bin.path().to_owned(),
            delay: Duration::from_millis(20),
        }));
        folder_tab(&mut harness).select_paths(&names);
        harness.state_mut().run(&ctx, Command::FolderDeleteOn(Side::Left));
        wait_for_plan(&mut harness);
        folder_tab(&mut harness).confirm_pending(&ctx, false);
        harness.run_steps(4);
        std::thread::sleep(Duration::from_millis(100));
        harness.run_steps(2);
        save(&mut harness, "folder-progress.png");
        folder_tab(&mut harness).cancel_operation();
        settle(&mut harness);
        let moved = std::fs::read_dir(bin.path()).unwrap().count();
        let left = std::fs::read_dir(a.path()).unwrap().count();
        assert!(moved > 0 && left > 0, "moved {moved}, left {left}");
        assert_eq!(moved + left, 40);
        let tab = folder_tab(&mut harness);
        let report = tab.report.as_ref().expect("a cancellation is reported");
        assert!(report.cancelled);
        assert_eq!(tab.entry_count(), left + 1, "the tree lost exactly what went");
        save(&mut harness, "folder-cancelled.png");
    }

    #[test]
    fn a_text_tab_learns_that_a_folder_operation_replaced_its_file() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write_folders(a.path(), b.path());
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        // A text tab on the pair, edited and not saved.
        harness.state_mut().open(&ctx, [a.path().join("both.txt"), b.path().join("both.txt")]).unwrap();
        settle(&mut harness);
        harness.state_mut().text_tab_mut().put_caret(Side::Right, 0, 0);
        harness.run_steps(2);
        type_text(&mut harness, "edit ");
        assert!(harness.state_mut().text_tab_mut().is_dirty());

        harness.state_mut().active = 0;
        folder_tab(&mut harness).select_paths(&["both.txt"]);
        harness.state_mut().run(&ctx, Command::FolderCopy(Side::Left));
        wait_for_plan(&mut harness);
        folder_tab(&mut harness).confirm_pending(&ctx, false);
        settle(&mut harness);
        harness.state_mut().active = 1;
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.message.as_deref().is_some_and(|m| m.contains("a changé sur le disque")));
        let error = tab.save().unwrap_err();
        assert!(error.contains("enregistrement refusé"), "{error}");
        assert_eq!(std::fs::read_to_string(b.path().join("both.txt")).unwrap(), "ancienne version\n");
    }

    #[test]
    fn mirroring_makes_the_right_identical_and_asks_before_deleting_for_good() {
        use egui_kittest::kittest::Queryable;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write_folders(a.path(), b.path());
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);
        assert!(folder_tab(&mut harness).selection().is_empty(), "no selection: the whole comparison");
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::FolderSync(Operation::Mirror { to: Side::Right, mode: DeleteMode::Trash }));
        wait_for_plan(&mut harness);
        let plan = folder_tab(&mut harness).pending_plan().unwrap();
        assert_eq!(plan.totals.creates, 2, "left-only.txt and docs/guide.md");
        assert_eq!(plan.totals.create_dirs, 1);
        assert_eq!(plan.totals.overwrites_of_newer, 1, "both.txt is newer on the right");
        assert_eq!(plan.totals.deletes, 1, "right-only.txt");
        harness.get_by_label("Synchroniser");
        save(&mut harness, "folder-mirror-dialog.png");
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        harness.run_steps(3);
        save(&mut harness, "folder-mirror-dialog-light.png");

        // Deleting for good is a deliberate second step, as for a deletion.
        harness.get_by_label("Supprimer définitivement…").click();
        harness.run_steps(3);
        save(&mut harness, "folder-mirror-permanent-light.png");
        harness.get_by_label("Synchroniser et supprimer définitivement").click();
        harness.run_steps(2);
        settle(&mut harness);
        assert!(!b.path().join("right-only.txt").exists());
        assert_eq!(std::fs::read_to_string(b.path().join("both.txt")).unwrap(), "ancienne version\n");
        assert_eq!(std::fs::read_to_string(b.path().join("docs/guide.md")).unwrap(), "# Guide\n");
        assert!(a.path().join("left-only.txt").exists(), "the source is untouched");
        let tab = folder_tab(&mut harness);
        assert!(tab.entry_status("right-only.txt").is_none());
        for rel in ["both.txt", "left-only.txt", "docs", "docs/guide.md", "same.txt"] {
            assert_eq!(tab.entry_status(rel), Some(biomerge_core::dirscan::Status::Same), "{rel}");
        }
        assert!(tab.last_action().is_some_and(|a| a.contains("synchronisés à droite")), "{:?}", tab.last_action());
    }

    #[test]
    fn updating_both_ways_takes_the_newer_file_and_leaves_a_tie_alone() {
        use egui_kittest::kittest::Queryable;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write_folders(a.path(), b.path());
        std::fs::write(a.path().join("tie.txt"), "gauche\n").unwrap();
        std::fs::write(b.path().join("tie.txt"), "droite\n").unwrap();
        let time = std::time::SystemTime::now() - Duration::from_secs(60);
        for root in [a.path(), b.path()] {
            std::fs::File::options().write(true).open(root.join("tie.txt")).unwrap().set_modified(time).unwrap();
        }
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::FolderSync(Operation::Update));
        wait_for_plan(&mut harness);
        let plan = folder_tab(&mut harness).pending_plan().unwrap();
        assert_eq!(plan.totals.deletes, 0);
        assert_eq!(plan.totals.overwrites_of_newer, 0);
        assert_eq!(plan.blocked.len(), 1);
        assert!(plan.blocked[0].reason.to_string().contains("même date des deux côtés"));
        save(&mut harness, "folder-update-dialog.png");
        harness.get_by_label("Synchroniser").click();
        harness.run_steps(2);
        settle(&mut harness);
        assert_eq!(std::fs::read_to_string(a.path().join("both.txt")).unwrap(), "version plus récente\n");
        assert_eq!(std::fs::read_to_string(a.path().join("right-only.txt")).unwrap(), "à droite\n");
        assert_eq!(std::fs::read_to_string(b.path().join("left-only.txt")).unwrap(), "à gauche\n");
        assert_eq!(std::fs::read_to_string(b.path().join("docs/guide.md")).unwrap(), "# Guide\n");
        assert_eq!(std::fs::read_to_string(a.path().join("tie.txt")).unwrap(), "gauche\n", "nobody can tell which one is wanted");
        assert_eq!(std::fs::read_to_string(b.path().join("tie.txt")).unwrap(), "droite\n");
        let tab = folder_tab(&mut harness);
        assert_eq!(tab.entry_status("tie.txt"), Some(biomerge_core::dirscan::Status::Different));
        assert_eq!(tab.entry_status("right-only.txt"), Some(biomerge_core::dirscan::Status::Same));
    }

    // --- Ignore rules -------------------------------------------------------------------------

    fn write_dated(dir: &Path) -> [std::path::PathBuf; 2] {
        let left = dir.join("report-a.log");
        let right = dir.join("report-b.log");
        std::fs::write(&left, "build 2024-03-15 ok\nsteps: 12\nsame line\nend 2024-03-15\n").unwrap();
        std::fs::write(&right, "build 2025-11-02 ok\nsteps: 14\nsame line\nend 2025-11-02\n").unwrap();
        [left, right]
    }

    #[test]
    fn ignored_rows_are_shown_but_not_counted_as_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_dated(dir.path()));
        harness.step();
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 2);

        let ctx = harness.ctx.clone();
        let app = harness.state_mut();
        app.ignore_rules = vec![IgnoreRule::text(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")];
        app.rules_changed();
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.block_count(), 1, "only the steps still differ");
        assert_eq!(tab.ignored_row_count(), 2);
        assert!(tab.status_items().iter().any(|item| matches!(item, StatusItem::Pill(text) if text == "2 lignes ignorées")));
        save(&mut harness, "ignored-rows.png");
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        settle(&mut harness);
        save(&mut harness, "ignored-rows-light.png");

        // A whole-line rule hides the last difference; a disabled rule does nothing.
        let app = harness.state_mut();
        app.ignore_rules.push(IgnoreRule { enabled: false, ..IgnoreRule::whole_line("^steps") });
        app.rules_changed();
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 1);
        let app = harness.state_mut();
        app.ignore_rules[1].enabled = true;
        app.rules_changed();
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.block_count(), 0);
        assert_eq!(tab.ignored_row_count(), 3);
    }

    #[test]
    fn the_rules_editor_applies_as_rules_change_and_offers_presets() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_dated(dir.path()));
        harness.step();
        settle(&mut harness);
        harness.get_by_label("Règles d'ignorance…").click();
        harness.run_steps(3);
        assert!(harness.state().rules_editor);
        save(&mut harness, "rules-empty.png");

        harness.get_by_label("Ajouter un préréglage").click();
        harness.run_steps(3);
        save(&mut harness, "rules-presets.png");
        harness.get_by_label("Date ISO").click();
        harness.run_steps(3);
        assert_eq!(harness.state().ignore_rules.len(), 1);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 1, "applied at once");

        // A rule typed by hand: its error shows under it and the others keep working.
        harness.get_by_label("Ajouter une règle").click();
        harness.run_steps(3);
        type_text(&mut harness, "steps: (");
        let app = harness.state();
        assert_eq!(app.ignore_rules[1].pattern, "steps: (");
        assert!(app.ignore_rules[1].check().is_err());
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 1);
        save(&mut harness, "rules-error.png");
        harness.key_press(Key::Backspace);
        harness.run_steps(2);
        type_text(&mut harness, "[0-9]+");
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 0, "steps: 12 and 14 now match");
        harness.get_all_by_label("Ligne entière").nth(1).unwrap().click();
        harness.run_steps(2);
        assert_eq!(harness.state().ignore_rules[1].scope, biomerge_core::ignore_rules::IgnoreScope::WholeLine);
        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        harness.run_steps(3);
        save(&mut harness, "rules-light.png");

        // Kept in the settings as they were.
        let stored = rules_editor::encode(&harness.state().ignore_rules);
        assert_eq!(rules_editor::decode(&stored), harness.state().ignore_rules);
        harness.get_by_label("Fermer").click();
        harness.run_steps(3);
        assert!(!harness.state().rules_editor);
    }

    // --- Git and the read-only side ---------------------------------------------------------

    pub(super) fn wait_for_tabs(harness: &mut Harness<'_, App>, count: usize) {
        for _ in 0..600 {
            harness.step();
            if harness.state().tabs.len() >= count {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(harness.state().tabs.len(), count, "{:?}", harness.state().message);
        settle(harness);
    }

    #[test]
    fn the_head_side_refuses_edits_and_transfers_into_it() {
        use egui_kittest::kittest::Queryable;
        let repo = tempfile::tempdir().unwrap();
        git::tests::repository(repo.path());
        let tracked = repo.path().join("tracked.txt");
        let mut harness = harness_for([tracked.clone(), tracked.clone()]);
        harness.step();
        settle(&mut harness);
        assert!(harness.state().palette_entries().iter().any(|e| e.command == Command::CompareWithHead));
        harness.state_mut().text_tab_mut().put_caret(Side::Right, 0, 0);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::CompareWithHead);
        wait_for_tabs(&mut harness, 2);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.title(), "tracked.txt (HEAD), tracked.txt");
        assert!(tab.is_read_only(Side::Left) && !tab.is_read_only(Side::Right));
        assert_eq!(tab.line_text(Side::Left, 1), "deux");
        assert_eq!(tab.line_text(Side::Right, 1), "2");
        assert_eq!(tab.block_count(), 1);
        harness.get_by_label("Chemin de gauche");
        save(&mut harness, "head.png");

        // Typing in it, pasting, sending a block into it: refused, with a word why.
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 0, 0);
        harness.run_steps(2);
        type_text(&mut harness, "x");
        harness.event(egui::Event::Paste("collé".to_owned()));
        harness.run_steps(2);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Left, 0), "un");
        assert!(tab.message.as_deref().is_some_and(|m| m.contains("lecture seule")));
        tab.push_block(&ctx, 0, Side::Left);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert!(!tab.is_dirty());
        assert_eq!(tab.block_count(), 1);

        // From it, a block goes as anywhere else; the working file is the one saved.
        tab.push_block(&ctx, 0, Side::Right);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.block_count(), 0);
        tab.save().unwrap();
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "un\ndeux\ntrois\n");
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        settle(&mut harness);
        save(&mut harness, "head-light.png");
    }

    #[test]
    fn an_untracked_file_explains_why_there_is_no_head() {
        let repo = tempfile::tempdir().unwrap();
        git::tests::repository(repo.path());
        let new = repo.path().join("new.txt");
        let mut harness = harness_for([new.clone(), new.clone()]);
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::CompareWithHead);
        settle(&mut harness);
        assert_eq!(harness.state().tabs.len(), 1);
        assert_eq!(harness.state().message.as_deref(), Some("new.txt n'est pas suivi par Git"));
    }

    // --- JSON -------------------------------------------------------------------------------

    /// Laid out as the projection writes it: one member or element per line.
    const JSON_LEFT: &str = "{\n  \"name\": \"biomerge\",\n  \"version\": 2,\n  \"tags\": [\n    \"diff\",\n    \"json\"\n  ],\n  \"build\": {\n    \"jobs\": 4\n  }\n}\n";
    /// The same value: keys in another order, another spelling of 2, all on one line.
    const JSON_RIGHT: &str = "{\"build\": {\"jobs\": 4}, \"tags\": [\"diff\", \"json\"], \"version\": 2.0, \"name\": \"biomerge\"}\n";

    fn write_json(dir: &Path, left: &str, right: &str) -> [std::path::PathBuf; 2] {
        let paths = [dir.join("left.json"), dir.join("right.json")];
        std::fs::write(&paths[0], left).unwrap();
        std::fs::write(&paths[1], right).unwrap();
        paths
    }

    #[test]
    fn a_json_tab_with_reordered_keys_shows_no_block_and_says_the_side_was_reformatted() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let paths = write_json(dir.path(), JSON_LEFT, JSON_RIGHT);
        let mut harness = harness_for(paths.clone());
        harness.step();
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.is_structural());
        assert_eq!(tab.block_count(), 0, "same value, nothing to show");
        assert!(!tab.is_dirty(), "a projection is not an edit");
        assert_eq!(tab.line_text(Side::Right, 1), "  \"name\": \"biomerge\",");
        assert_eq!(tab.badge_texts(Side::Right), ["reformaté"]);
        assert!(tab.badge_texts(Side::Left).is_empty(), "the reference was already laid out so");
        harness.get_by_label("reformaté");
        save(&mut harness, "json.png");

        // Ctrl+Z gives the file's own text back, still clean.
        harness.key_press_modifiers(Modifiers::COMMAND, Key::Z);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Right, 0), JSON_RIGHT.trim_end());
        assert!(!tab.is_dirty());
        assert!(tab.block_count() > 0);
        harness.key_press_modifiers(Modifiers::COMMAND, Key::Y);
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 0);

        // Saving an edit there asks once, as the file gets the new layout.
        harness.state_mut().text_tab_mut().put_caret(Side::Right, 2, 14);
        harness.run_steps(2);
        type_text(&mut harness, "0");
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 1, "the comparison follows the keystroke");
        harness.key_press_modifiers(Modifiers::COMMAND, Key::S);
        harness.run_steps(3);
        assert!(harness.state_mut().text_tab_mut().has_question());
        assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), JSON_RIGHT, "nothing written before the answer");
        save(&mut harness, "json-save-question.png");
        harness.get_by_label("Enregistrer quand même").click();
        harness.run_steps(3);
        let written = std::fs::read_to_string(&paths[1]).unwrap();
        assert!(written.starts_with("{\n  \"name\": \"biomerge\",\n  \"version\": 20,"), "{written}");
        assert!(!harness.state_mut().text_tab_mut().is_dirty());
        assert!(harness.state_mut().text_tab_mut().badge_texts(Side::Right).is_empty());

        // The options live in the "JSON" submenu of the hamburger.
        harness.get_by_label("Menu").click();
        harness.run_steps(4);
        harness.get_by_label("JSON").hover();
        harness.run_steps(6);
        harness.get_by_label("Clés par ordre alphabétique");
        harness.get_by_label("Changements JSON…");
        save(&mut harness, "json-menu.png");
    }

    #[test]
    fn a_json_option_projects_again_in_one_step_that_undo_reverts() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_json(dir.path(), JSON_LEFT, JSON_RIGHT));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::SetJsonKeyOrder(KeyOrder::AsWritten));
        settle(&mut harness);
        assert_eq!(harness.state().json_options.key_order, KeyOrder::AsWritten, "kept for the next tabs");
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.json_options().unwrap().key_order, KeyOrder::AsWritten);
        assert_eq!(tab.line_text(Side::Right, 1), "  \"build\": {", "the file's own order, not the previous projection's");
        assert!(tab.block_count() > 0);
        assert!(!tab.is_dirty());

        harness.state_mut().text_tab_mut().undo(&ctx);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.block_count(), 0, "both sides came back at once");
        assert_eq!(tab.line_text(Side::Right, 1), "  \"name\": \"biomerge\",");

        // With an edit in a pane, the user is asked before the panes are rewritten.
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 2, 14);
        harness.run_steps(2);
        type_text(&mut harness, "1");
        harness.state_mut().run(&ctx, Command::SetJsonKeyOrder(KeyOrder::Alphabetical));
        harness.run_steps(3);
        assert!(harness.state_mut().text_tab_mut().has_question());
        save(&mut harness, "json-reproject-question.png");
        {
            use egui_kittest::kittest::Queryable;
            harness.get_by_label("Réaligner").click();
        }
        harness.run_steps(2);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Left, 1), "  \"build\": {", "sorted");
        assert_eq!(tab.line_text(Side::Left, 9), "  \"version\": 21", "the edit is kept");
    }

    #[test]
    fn invalid_json_is_compared_as_text_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_json(dir.path(), JSON_LEFT, "{\"name\": \"biomerge\", \"version\": }\n"));
        harness.step();
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert!(!tab.is_structural());
        assert_eq!(tab.line_text(Side::Left, 0), "{", "nothing was rewritten");
        let message = tab.message.clone().unwrap_or_default();
        assert!(message.contains("JSON invalide") && message.contains("right.json : ligne 1, colonne"), "{message}");
        assert_eq!(tab.badge_texts(Side::Right), ["JSON invalide"]);
        save(&mut harness, "json-invalid.png");
        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        settle(&mut harness);
        save(&mut harness, "json-invalid-light.png");

        // JSONC is named for what it is.
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_json(dir.path(), "{\n  // réglage\n  \"a\": 1\n}\n", "{\"a\": 1}\n"));
        harness.step();
        settle(&mut harness);
        let message = harness.state_mut().text_tab_mut().message.clone().unwrap_or_default();
        assert!(message.contains("JSONC ou JSON5"), "{message}");
    }

    #[test]
    fn a_transferred_member_gets_its_comma_in_the_same_undo_step() {
        let dir = tempfile::tempdir().unwrap();
        let left = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let right = "{\n  \"a\": 1\n}\n";
        let paths = write_json(dir.path(), left, right);
        let mut harness = harness_for(paths.clone());
        harness.step();
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 1, "the comma alone is no difference");
        let ctx = harness.ctx.clone();
        harness.state_mut().text_tab_mut().push_block(&ctx, 0, Side::Right);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Right, 1), "  \"a\": 1,", "repaired");
        assert_eq!(tab.line_text(Side::Right, 2), "  \"b\": 2");
        assert_eq!(tab.block_count(), 0);
        tab.save().unwrap();
        assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), left);
        harness.state_mut().text_tab_mut().undo(&ctx);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_count(Side::Right), 3);
        assert_eq!(tab.line_text(Side::Right, 1), "  \"a\": 1", "transfer and comma undone together");
    }

    #[test]
    fn the_status_bar_shows_the_json_path_of_the_caret_and_copies_it() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let mut harness = harness_for(write_json(dir.path(), JSON_LEFT, JSON_RIGHT));
        harness.step();
        settle(&mut harness);
        harness.state_mut().text_tab_mut().put_caret(Side::Right, 8, 4);
        harness.run_steps(3);
        assert_eq!(harness.state_mut().text_tab_mut().caret_json_path().as_deref(), Some("$.build.jobs"));
        let path = harness.get_by_label("$.build.jobs");
        path.click();
        harness.run_steps(2);
        assert!(harness.state().notice.as_ref().is_some_and(|(text, _)| text.contains("$.build.jobs")));
        save(&mut harness, "json-path.png");

        // Typing moves what follows: the paths follow the keystroke.
        harness.state_mut().text_tab_mut().put_caret(Side::Right, 3, 11);
        harness.run_steps(2);
        harness.key_press(Key::Enter);
        harness.run_steps(2);
        type_text(&mut harness, "  \"beta\",");
        harness.state_mut().text_tab_mut().put_caret(Side::Right, 5, 4);
        harness.run_steps(3);
        assert_eq!(harness.state_mut().text_tab_mut().caret_json_path().as_deref(), Some("$.tags[1]"));
    }

    #[test]
    fn a_change_of_the_json_summary_jumps_to_its_line() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let right = "{\"build\": {\"jobs\": 8}, \"tags\": [\"diff\"], \"version\": 2, \"name\": \"biomerge\", \"new\": true}\n";
        let mut harness = harness_for(write_json(dir.path(), JSON_LEFT, right));
        harness.step();
        settle(&mut harness);
        let ctx = harness.ctx.clone();
        harness.state_mut().run(&ctx, Command::JsonSummary);
        for _ in 0..500 {
            harness.step();
            if harness.state_mut().text_tab_mut().summary_ready() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        harness.run_steps(3);
        let paths = harness.state_mut().text_tab_mut().summary_paths();
        assert!(paths.contains(&"$.build.jobs".to_owned()) && paths.contains(&"$.new".to_owned()), "{paths:?}");
        save(&mut harness, "json-summary.png");
        let ctx2 = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx2, ThemeKind::Clair);
        harness.run_steps(3);
        save(&mut harness, "json-summary-light.png");
        harness.get_by_label("$.build.jobs").click();
        harness.run_steps(3);
        let tab = harness.state_mut().text_tab_mut();
        assert!(!tab.wants_dialog(), "the dialog closes on the jump");
        assert_eq!(tab.caret_side(), Some(Side::Left));
        assert_eq!(tab.caret_position(), Some((8, 0)), "the line of \"jobs\" on the left");
    }

    // --- Documents --------------------------------------------------------------------------

    fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in parts {
            writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// A .docx whose paragraphs are given as raw runs (`<w:r>…</w:r>` content of each).
    fn docx(paragraphs: &[&str]) -> Vec<u8> {
        const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
        let body: String = paragraphs.iter().map(|runs| format!("<w:p>{runs}</w:p>")).collect();
        let document = format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W}><w:body>{body}<w:sectPr/></w:body></w:document>"#);
        zip_of(&[
            ("[Content_Types].xml", r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
            ),
            ("word/document.xml", &document),
        ])
    }

    fn run(text: &str) -> String {
        format!(r#"<w:r><w:t xml:space="preserve">{text}</w:t></w:r>"#)
    }

    fn odt(paragraphs: &[&str]) -> Vec<u8> {
        const ODF: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0""#;
        let body: String = paragraphs.iter().map(|text| format!("<text:p>{text}</text:p>")).collect();
        let content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {ODF}><office:body><office:text>{body}</office:text></office:body></office:document-content>"#
        );
        zip_of(&[("mimetype", "application/vnd.oasis.opendocument.text"), ("content.xml", &content)])
    }

    #[test]
    fn a_word_and_an_opendocument_file_compare_by_their_text_read_only() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("rapport.docx");
        let right = dir.path().join("rapport.odt");
        let long = "Une phrase assez longue pour qu'un paragraphe entier tienne sur une seule ligne. ".repeat(4);
        let first = format!("{}{}", run("Le rapport annuel "), run(long.trim_end()));
        std::fs::write(&left, docx(&[&first, &run("Conclusion provisoire.")])).unwrap();
        std::fs::write(&right, odt(&[&format!("Le rapport annuel {}", long.trim_end()), "Conclusion définitive."])).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx, None, None);
        app.wrap_lines = false;
        let mut pending = Some([left.clone(), right.clone()]);
        let mut harness = Harness::builder()
            .with_size(vec2(1440.0, 760.0))
            .with_pixels_per_point(2.0)
            .with_max_steps(20_000)
            .allow_missing_glyphs()
            .wgpu()
            .build_ui_state(
                move |ui, app: &mut App| {
                    if let Some(paths) = pending.take() {
                        app.install_look(ui.ctx());
                        app.open(ui.ctx(), paths).unwrap();
                    }
                    paint_wallpaper(ui);
                    app.root(ui);
                },
                app,
            );
        harness.step();
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.is_read_only(Side::Left) && tab.is_read_only(Side::Right));
        assert_eq!(tab.line_count(Side::Left), 2, "one paragraph, one line");
        assert_eq!(tab.line_text(Side::Right, 1), "Conclusion définitive.");
        assert_eq!(tab.block_count(), 1);
        assert!(tab.wraps(), "a document tab wraps by itself");
        assert_eq!(tab.badge_texts(Side::Left), ["Word"]);
        assert_eq!(tab.badge_texts(Side::Right), ["OpenDocument"]);
        assert!(!harness.state().wrap_lines, "the setting is left alone");
        harness.get_by_label("Word");
        harness.get_by_label("OpenDocument");
        assert!(harness.state().status_items_text().contains("Markdown"), "{}", harness.state().status_items_text());
        save(&mut harness, "documents.png");
        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        settle(&mut harness);
        save(&mut harness, "documents-light.png");

        // Alt+Z acts on this tab alone.
        harness.key_press_modifiers(Modifiers::ALT, Key::Z);
        harness.run_steps(2);
        assert!(!harness.state_mut().text_tab_mut().wraps());
        assert!(!harness.state().wrap_lines);

        // F5 extracts again: the file changed meanwhile.
        std::fs::write(&left, docx(&[&first, &run("Conclusion définitive.")])).unwrap();
        harness.key_press(Key::F5);
        harness.run_steps(2);
        settle(&mut harness);
        assert_eq!(harness.state_mut().text_tab_mut().block_count(), 0);
    }

    #[test]
    fn nothing_can_be_typed_or_sent_into_a_document_side() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("notes.docx");
        let right = dir.path().join("notes.md");
        std::fs::write(&left, docx(&[&run("Premier point."), &run("Second point.")])).unwrap();
        std::fs::write(&right, "Premier point.\n").unwrap();
        let mut harness = harness_for([left.clone(), right.clone()]);
        harness.step();
        settle(&mut harness);
        let before = std::fs::read(&left).unwrap();
        harness.state_mut().text_tab_mut().put_caret(Side::Left, 0, 0);
        harness.run_steps(2);
        type_text(&mut harness, "x");
        let ctx = harness.ctx.clone();
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.line_text(Side::Left, 0), "Premier point.");
        assert!(tab.message.as_deref().is_some_and(|m| m.contains("comparé par son texte, en lecture seule")), "{:?}", tab.message);
        tab.push_block(&ctx, 0, Side::Left);
        settle(&mut harness);
        assert!(!harness.state_mut().text_tab_mut().is_dirty());

        // The other way is allowed: the text side is an ordinary file.
        harness.state_mut().text_tab_mut().push_block(&ctx, 0, Side::Right);
        settle(&mut harness);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.block_count(), 0);
        tab.save().unwrap();
        assert_eq!(std::fs::read_to_string(&right).unwrap(), "Premier point.\nSecond point.\n");
        assert_eq!(std::fs::read(&left).unwrap(), before, "the document is never written");
    }

    #[test]
    fn folders_tell_documents_and_json_with_the_same_content_from_different_ones() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // Same text, other bytes: the runs are split differently.
        std::fs::write(a.path().join("contrat.docx"), docx(&[&run("Bonjour le monde")])).unwrap();
        std::fs::write(b.path().join("contrat.docx"), docx(&[&format!("{}{}", run("Bonjour "), run("le monde"))])).unwrap();
        std::fs::write(a.path().join("devis.docx"), docx(&[&run("Total : 100 €")])).unwrap();
        std::fs::write(b.path().join("devis.docx"), docx(&[&run("Total : 120 €")])).unwrap();
        std::fs::write(a.path().join("config.json"), JSON_LEFT).unwrap();
        std::fs::write(b.path().join("config.json"), JSON_RIGHT).unwrap();
        std::fs::write(a.path().join("notes.txt"), "un").unwrap();
        std::fs::write(b.path().join("notes.txt"), "deux").unwrap();
        let mut harness = harness_for([a.path().to_owned(), b.path().to_owned()]);
        harness.step();
        settle(&mut harness);
        use crate::folder_view::Equivalence;
        use biomerge_core::dirscan::Status;
        let tab = folder_tab(&mut harness);
        assert_eq!(tab.entry_status("contrat.docx"), Some(Status::Different), "the bytes do differ");
        assert_eq!(tab.equivalence_of("contrat.docx"), Some(Equivalence::Text));
        assert_eq!(tab.equivalence_of("devis.docx"), None);
        assert_eq!(tab.equivalence_of("config.json"), Some(Equivalence::Json));
        assert_eq!(tab.equivalence_of("notes.txt"), None);
        assert!(tab.status_items().iter().any(|item| matches!(item, StatusItem::Pill(text) if text == "2 même texte ou valeur")));
        save(&mut harness, "folder-same-text.png");
        let ctx = harness.ctx.clone();
        harness.state_mut().set_theme(&ctx, ThemeKind::Clair);
        harness.run_steps(3);
        save(&mut harness, "folder-same-text-light.png");

        // A double click opens the extracted texts.
        let at = folder_tab(&mut harness).row_center("contrat.docx", Side::Left).unwrap();
        harness.hover_at(at);
        harness.step();
        // A step lasts a quarter of a second: each click goes in one frame to stay a double one.
        for _ in 0..2 {
            for pressed in [true, false] {
                let event = egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
                harness.input_mut().events.push(event);
            }
            harness.step();
        }
        wait_for_tabs(&mut harness, 2);
        let tab = harness.state_mut().text_tab_mut();
        assert!(tab.is_read_only(Side::Left) && tab.is_read_only(Side::Right));
        assert_eq!(tab.line_text(Side::Right, 0), "Bonjour le monde");
        assert_eq!(tab.block_count(), 0);
    }

    // --- Single instance --------------------------------------------------------------------

    pub(super) fn empty_harness() -> Harness<'static, App> {
        let ctx = egui::Context::default();
        let app = App::new(&ctx, None, None);
        Harness::builder()
            .with_size(vec2(1440.0, 760.0))
            .with_pixels_per_point(2.0)
            .with_max_steps(20_000)
            .allow_missing_glyphs()
            .wgpu()
            .build_ui_state(|ui, app: &mut App| app.root(ui), app)
    }

    #[test]
    fn a_second_launch_opens_a_labelled_tab_and_waits_until_it_closes() {
        let dir = tempfile::tempdir().unwrap();
        let [left, right] = write_fixture(dir.path());
        let socket = dir.path().join("instance.sock");
        let mut harness = empty_harness();
        let ctx = harness.ctx.clone();
        let listener = crate::instance::Listener::bind(&socket).unwrap();
        harness.state_mut().listen(&ctx, listener);

        let request = Request {
            paths: Some([left, right]),
            labels: [Some("src/Test.cpp (base)".to_owned()), Some("src/Test.cpp".to_owned())],
            wait: true,
            ..Request::default()
        };
        let (done, finished) = channel();
        let client_socket = socket.clone();
        std::thread::spawn(move || {
            let handed = crate::instance::hand_over(&client_socket, &request).expect("the window answers");
            let _ = done.send("ok");
            handed.wait();
            let _ = done.send("closed");
        });
        wait_for_tabs(&mut harness, 1);
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok("ok"));
        assert_eq!(harness.state().tabs[0].title(), "src/Test.cpp (base), src/Test.cpp");
        std::thread::sleep(Duration::from_millis(100));
        assert!(finished.try_recv().is_err(), "the launch waits while the tab is open");
        save(&mut harness, "labels.png");

        harness.state_mut().request_close(0);
        harness.run_steps(2);
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok("closed"));

        // A launch with what cannot be opened hears why.
        let client_socket = socket.clone();
        let missing = [dir.path().join("missing.txt"), dir.path().join("Test2.cpp")];
        let handle = std::thread::spawn(move || {
            let request = Request { paths: Some(missing), ..Request::default() };
            match crate::instance::hand_over(&client_socket, &request) {
                Some(crate::instance::Handed::Refused(message)) => Some(message),
                _ => None,
            }
        });
        while !handle.is_finished() {
            harness.step();
            std::thread::sleep(Duration::from_millis(5));
        }
        let message = handle.join().unwrap().expect("refused with a reason");
        assert!(message.contains("missing.txt introuvable"), "{message}");
        assert!(harness.state().tabs.is_empty());
        drop(harness);
        assert!(!socket.exists(), "the socket goes with the window");
    }

    /// A window taking the requests of later launches on `socket`.
    pub(super) fn listening_harness(socket: &Path) -> Harness<'static, App> {
        let mut harness = empty_harness();
        let ctx = harness.ctx.clone();
        let listener = crate::instance::Listener::bind(socket).unwrap();
        harness.state_mut().listen(&ctx, listener);
        harness
    }

    /// A later launch handing `request` over: the channel hears "ok", then for a waiting launch
    /// the code its tab closed with.
    pub(super) fn launch(socket: &Path, request: Request) -> Receiver<Option<i32>> {
        let client = socket.to_owned();
        let (done, finished) = channel();
        std::thread::spawn(move || {
            let handed = crate::instance::hand_over(&client, &request).expect("the window answers");
            let _ = done.send(None);
            let _ = done.send(handed.wait());
        });
        finished
    }

    /// Steps the window until the launch hears that its request was taken.
    pub(super) fn answered(harness: &mut Harness<'_, App>, finished: &Receiver<Option<i32>>) {
        for _ in 0..600 {
            harness.step();
            if let Ok(heard) = finished.try_recv() {
                assert_eq!(heard, None, "the launch heard the window take its request");
                harness.run_steps(3);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the window never answered");
    }

    #[test]
    fn a_head_launch_opens_the_committed_version_beside_the_working_file_and_waits() {
        let repo = tempfile::tempdir().unwrap();
        git::tests::repository(repo.path());
        let socket = repo.path().join("instance.sock");
        let mut harness = listening_harness(&socket);
        let finished = launch(&socket, Request { head: Some(repo.path().join("tracked.txt")), wait: true, ..Request::default() });
        answered(&mut harness, &finished);
        wait_for_tabs(&mut harness, 1);
        let tab = harness.state_mut().text_tab_mut();
        assert_eq!(tab.title(), "tracked.txt (HEAD), tracked.txt");
        assert!(tab.is_read_only(Side::Left) && !tab.is_read_only(Side::Right));
        assert_eq!(tab.line_text(Side::Left, 1), "deux");
        assert_eq!(tab.line_text(Side::Right, 1), "2");
        std::thread::sleep(Duration::from_millis(100));
        assert!(finished.try_recv().is_err(), "the launch waits while the tab is open");
        harness.state_mut().request_close(0);
        harness.run_steps(2);
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(0)));

        // A file git does not follow: the window says why, as the menu does, and lets the
        // launch go.
        let finished = launch(&socket, Request { head: Some(repo.path().join("new.txt")), wait: true, ..Request::default() });
        answered(&mut harness, &finished);
        settle(&mut harness);
        assert!(harness.state().tabs.is_empty());
        assert_eq!(harness.state().message.as_deref(), Some("new.txt n'est pas suivi par Git"));
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(0)));
    }

    #[test]
    fn a_single_ordinary_file_opens_the_dialog_with_the_right_side_to_fill() {
        use egui_kittest::kittest::Queryable;
        let dir = tempfile::tempdir().unwrap();
        let [left, right] = write_fixture(dir.path());
        let socket = dir.path().join("instance.sock");
        let mut harness = listening_harness(&socket);
        let finished = launch(&socket, Request { single: Some(left.clone()), wait: true, ..Request::default() });
        answered(&mut harness, &finished);
        let dialog = harness.state().open_dialog.as_ref().expect("the comparison dialog");
        assert_eq!(dialog.paths, [left.display().to_string(), String::new()]);
        assert!(harness.state().tabs.is_empty());
        assert!(harness.query_all_by_label("Droite").any(|node| node.is_focused()), "the right field takes the keyboard");
        save(&mut harness, "single-path.png");

        // The right side typed, then Enter: the launch now waits on the tab.
        type_text(&mut harness, &right.display().to_string());
        harness.key_press(Key::Enter);
        harness.run_steps(3);
        assert!(harness.state().open_dialog.is_none());
        assert_eq!(harness.state().tabs.len(), 1);
        assert_eq!(harness.state().tabs[0].title(), "Test1.cpp, Test2.cpp");
        std::thread::sleep(Duration::from_millis(100));
        assert!(finished.try_recv().is_err(), "the launch waits while the tab is open");
        settle(&mut harness);
        harness.state_mut().request_close(0);
        harness.run_steps(2);
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(0)));

        // A folder alone is a left side too; cancelling lets its launch go.
        let finished = launch(&socket, Request { single: Some(dir.path().to_owned()), wait: true, ..Request::default() });
        answered(&mut harness, &finished);
        assert_eq!(harness.state().open_dialog.as_ref().unwrap().paths[0], dir.path().display().to_string());
        harness.key_press(Key::Escape);
        harness.run_steps(3);
        assert!(harness.state().open_dialog.is_none());
        assert_eq!(finished.recv_timeout(Duration::from_secs(5)), Ok(Some(0)));
    }
}
