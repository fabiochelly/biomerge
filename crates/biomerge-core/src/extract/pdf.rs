//! PDF text, made comparable.
//!
//! pdf_oxide gives each page as lines in reading order, and nothing more: no paragraphs, running
//! headers and page numbers in the middle of the text, words cut at line ends, and garbage where
//! a font has no usable character map. What follows rebuilds what the line diff needs, as for
//! the word-processing formats: one paragraph per line, nothing that changes when only the
//! pagination does.
//!
//! In order: the pages are read under guard (a PDF must neither crash nor hang the application),
//! each page's text is checked to be made of words, running lines are dropped, lines are joined
//! into paragraphs with their line-end hyphens resolved, then the [`Normalization`] applies.
//!
//! [`Normalization`]: super::Normalization

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::LazyLock;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use pdf_oxide::extractors::auto::PageKind;
use pdf_oxide::{Error as PdfError, PdfDocument};
use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use super::{
    DocFacts, DocFormat, ExtractError, ExtractOptions, Extracted, PageStart, PdfParagraphs, hash_text,
    normalize_line,
};

/// How much reading a PDF may take. Past `pages` or `time`, the remaining pages are left unread
/// and counted; past `hard`, a page that never ends is abandoned with its thread.
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    pub pages: usize,
    pub time: Duration,
    pub hard: Duration,
}

const BUDGET: Budget = Budget {
    pages: 20_000,
    time: Duration::from_secs(30),
    hard: Duration::from_secs(90),
};

/// The reader recurses through nested objects: a large stack keeps a deep file from the stack
/// overflow, which unlike a panic would take the whole application down.
const READER_STACK: usize = 64 * 1024 * 1024;

/// Lines this close to the top or bottom of a page are candidates for running headers and
/// footers.
const EDGE_LINES: usize = 3;
/// Below this width (in characters) lines are a form or a list, never prose to rejoin.
const MIN_PROSE_WIDTH: usize = 25;
/// A page needs this many lines to measure its own line width; fewer, the document's is used.
const MIN_LINES_FOR_WIDTH: usize = 8;

/// One page as the reader gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RawPage {
    Text(String),
    /// No text: an image (a scan, no OCR is done) or a blank page.
    Empty { scanned: bool },
    /// The reader failed on this page.
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RawPdf {
    pub pages: Vec<RawPage>,
    /// Pages past the budget, after `pages`.
    pub skipped: usize,
}

/// Why a PDF gave no text, before a path is attached to it.
#[derive(Debug)]
enum Failure {
    PasswordRequired,
    WrongPassword,
    /// An encryption the reader cannot open, whatever the password.
    Encrypted,
    Broken(&'static str),
    TooSlow,
}

impl Failure {
    fn into_error(self, path: &Path) -> ExtractError {
        let path = path.to_owned();
        match self {
            Failure::PasswordRequired => ExtractError::PasswordRequired(path),
            Failure::WrongPassword => ExtractError::WrongPassword(path),
            Failure::Encrypted => ExtractError::Encrypted(path),
            Failure::Broken(detail) => ExtractError::Pdf {
                path,
                detail: detail.to_owned(),
            },
            Failure::TooSlow => ExtractError::TooSlow(path),
        }
    }
}

pub(super) fn extract_pdf(
    bytes: Vec<u8>,
    path: &Path,
    options: &ExtractOptions,
    password: Option<&str>,
) -> Result<Extracted, ExtractError> {
    let raw = read_guarded(bytes, password, BUDGET).map_err(|failure| failure.into_error(path))?;
    Ok(assemble(&raw, options))
}

/// Reads on a thread of its own: a panic is caught there, and a page that never ends leaves its
/// thread behind rather than the caller waiting forever.
fn read_guarded(bytes: Vec<u8>, password: Option<&str>, budget: Budget) -> Result<RawPdf, Failure> {
    const INTERNAL: &str = "erreur interne du lecteur PDF";
    let (sender, receiver) = mpsc::channel();
    let password = password.map(str::to_owned);
    let spawned = std::thread::Builder::new()
        .name("pdf".to_owned())
        .stack_size(READER_STACK)
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| read_pages(bytes, password.as_deref(), budget)));
            let _ = sender.send(outcome.unwrap_or(Err(Failure::Broken(INTERNAL))));
        });
    if spawned.is_err() {
        return Err(Failure::Broken(INTERNAL));
    }
    match receiver.recv_timeout(budget.hard) {
        Ok(outcome) => outcome,
        Err(RecvTimeoutError::Timeout) => Err(Failure::TooSlow),
        Err(RecvTimeoutError::Disconnected) => Err(Failure::Broken(INTERNAL)),
    }
}

fn read_pages(bytes: Vec<u8>, password: Option<&str>, budget: Budget) -> Result<RawPdf, Failure> {
    let doc = PdfDocument::from_bytes(bytes).map_err(|error| broken(&error))?;
    // Unauthenticated, the reader returns empty pages as a success: the state is checked first.
    // Opening already tried the empty user password; a password given is only tried when that
    // was not enough.
    if doc.is_encrypted() && !doc.is_authenticated() {
        match doc.authenticate(password.unwrap_or("").as_bytes()) {
            Ok(true) => {}
            Ok(false) if password.is_some() => return Err(Failure::WrongPassword),
            Ok(false) => return Err(Failure::PasswordRequired),
            Err(_) => return Err(Failure::Encrypted),
        }
    }
    let count = doc.page_count().map_err(|error| broken(&error))?;
    let started = Instant::now();
    let mut raw = RawPdf::default();
    for index in 0..count {
        if index >= budget.pages || started.elapsed() > budget.time {
            raw.skipped = count - index;
            break;
        }
        let text = catch_unwind(AssertUnwindSafe(|| doc.extract_text(index)));
        raw.pages.push(match text {
            Ok(Ok(text)) if text.chars().any(|c| !c.is_whitespace()) => RawPage::Text(text),
            Ok(Ok(_)) => RawPage::Empty {
                scanned: is_scanned(&doc, index),
            },
            Ok(Err(PdfError::EncryptedPdf)) => return Err(Failure::PasswordRequired),
            _ => RawPage::Failed,
        });
    }
    Ok(raw)
}

/// Only asked of pages without text: the classification reads the page again.
fn is_scanned(doc: &PdfDocument, index: usize) -> bool {
    let kind = catch_unwind(AssertUnwindSafe(|| doc.classify_page(index)));
    matches!(kind, Ok(Ok(page)) if matches!(page.kind, PageKind::Scanned | PageKind::ImageText | PageKind::Mixed))
}

fn broken(error: &PdfError) -> Failure {
    Failure::Broken(match error {
        PdfError::EncryptedPdf => return Failure::PasswordRequired,
        PdfError::InvalidHeader(_) => "en-tête %PDF- invalide",
        PdfError::UnsupportedVersion(_) => "version non prise en charge",
        PdfError::InvalidXref => "table des objets endommagée",
        PdfError::UnexpectedEof => "fichier tronqué",
        PdfError::Unsupported(_) | PdfError::UnsupportedFilter(_) => "fonction non prise en charge",
        PdfError::Io(_) => "lecture impossible",
        _ => "structure endommagée",
    })
}

// ---------------------------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Line {
    /// Trimmed; empty for a blank line, which ends a paragraph.
    text: String,
    /// Width in characters, runs of spaces counted as one.
    width: usize,
    /// Columns padded with spaces or tabs, or pipes: a table row, never joined.
    table: bool,
    /// Stands for an illegible page: alone on its line, never joined, never a running line.
    placeholder: bool,
}

impl Line {
    fn new(raw: &str) -> Self {
        let raw: String = raw.chars().filter(|&c| c == '\t' || !c.is_control()).collect();
        let text = raw.trim().to_owned();
        Line {
            width: text.split_whitespace().map(|word| word.chars().count() + 1).sum::<usize>().saturating_sub(1),
            table: text.contains('\t') || text.contains("   ") || text.contains(" | "),
            text,
            placeholder: false,
        }
    }

    fn placeholder(text: String) -> Self {
        Line {
            width: text.chars().count(),
            text,
            table: false,
            placeholder: true,
        }
    }

    fn is_text(&self) -> bool {
        !self.text.is_empty() && !self.placeholder
    }
}

struct Page {
    /// From 1.
    number: u32,
    lines: Vec<Line>,
}

pub(super) fn assemble(raw: &RawPdf, options: &ExtractOptions) -> Extracted {
    let mut facts = DocFacts {
        pages: raw.pages.len() + raw.skipped,
        unread_pages: raw.skipped,
        ..DocFacts::default()
    };
    let mut pages = Vec::with_capacity(raw.pages.len());
    for (index, page) in raw.pages.iter().enumerate() {
        let lines = match page {
            // Diffing garbage is noise. A fingerprint stands in for it: the same garbage still
            // compares equal, other garbage still differs, so "same text" never lies.
            RawPage::Text(text) if is_illegible(text) => {
                facts.illegible_pages += 1;
                vec![Line::placeholder(format!(
                    "[page illisible : texte non comparé, empreinte {:08x}]",
                    hash_text(text) as u32
                ))]
            }
            // The reader sometimes ends a line with a form feed instead of a newline.
            RawPage::Text(text) => text.split(['\n', '\r', '\x0c']).map(Line::new).collect(),
            RawPage::Empty { scanned } => {
                facts.pages_without_text += 1;
                facts.scanned_pages += usize::from(*scanned);
                Vec::new()
            }
            RawPage::Failed => {
                facts.unread_pages += 1;
                Vec::new()
            }
        };
        pages.push(Page {
            number: index as u32 + 1,
            lines,
        });
    }
    if !options.pdf_headers_footers {
        facts.headers_footers_removed = remove_running_lines(&mut pages);
    }

    let mut text = String::new();
    let mut page_starts = Vec::new();
    let mut line = 0;
    for (page, paragraph) in join_paragraphs(&pages, options.pdf_paragraphs) {
        let paragraph = normalize_line(&paragraph, &options.normalization);
        if paragraph.trim().is_empty() {
            continue;
        }
        if page_starts.last().is_none_or(|start: &PageStart| start.page != page) {
            page_starts.push(PageStart { line, page });
        }
        text.push_str(&paragraph);
        text.push('\n');
        line += 1;
    }
    Extracted {
        format: DocFormat::Pdf,
        text,
        facts,
        page_starts,
    }
}

// ---------------------------------------------------------------------------------------------
// Illegible pages
// ---------------------------------------------------------------------------------------------

/// Whether a page's text is not words: a font without a usable character map gives letters
/// shifted, glyph numbers or punctuation, a damaged file gives bytes. Cheap signals only:
/// replacement, control and private-use characters; symbols outnumbering letters and digits
/// (runs of one character, dot leaders, count once); a long page without a single word, only
/// stray letters and digits (other scripts aside); then, over Latin words, the share of vowels
/// (French and English prose stay well above 30 %, shifted letters fall under 25 %) and the
/// share of words no language writes (symbols between letters, a capital after a small letter,
/// no vowel).
fn is_illegible(text: &str) -> bool {
    let (mut visible, mut bad, mut symbols, mut other_scripts) = (0usize, 0usize, 0usize, 0usize);
    let mut previous = ' ';
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if c == previous && !c.is_alphanumeric() {
            continue;
        }
        previous = c;
        visible += 1;
        if c == '\u{fffd}' || c.is_control() || is_private_use(c) {
            bad += 1;
        } else if !c.is_alphanumeric() {
            symbols += 1;
        } else if c.is_alphabetic() && !is_latin_letter(c) {
            other_scripts += 1;
        }
    }
    if visible > 0 && bad * 10 >= visible {
        return true;
    }
    if visible >= 40 && symbols * 2 > visible {
        return true;
    }
    let (mut words, mut odd, mut letters, mut vowels) = (0usize, 0usize, 0usize, 0usize);
    for token in text.split_whitespace() {
        let core = token.trim_matches(|c: char| !c.is_alphanumeric());
        let length = core.chars().count();
        if length < 3 || !core.chars().any(is_latin_letter) || core.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        // Acronyms (TTC, CSG, SNCF) have no vowels to count.
        if length <= 5 && !core.chars().any(char::is_lowercase) {
            continue;
        }
        words += 1;
        let mut has_vowel = false;
        let mut previous_lower = false;
        let mut strange = false;
        for c in core.chars() {
            if c.is_alphabetic() {
                letters += 1;
                if is_vowel(c) {
                    vowels += 1;
                    has_vowel = true;
                }
                strange |= previous_lower && c.is_uppercase();
                previous_lower = c.is_lowercase();
            } else if !matches!(c, '\'' | '’' | '-' | '.' | '\u{ad}') {
                strange = true;
            }
        }
        if strange || (!has_vowel && length >= 4) {
            odd += 1;
        }
    }
    if words == 0 && visible >= 300 && other_scripts * 10 < visible {
        return true;
    }
    words >= 8 && (odd * 2 > words || vowels * 4 < letters)
}

fn is_private_use(c: char) -> bool {
    matches!(c, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..='\u{10ffff}')
}

fn is_latin_letter(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, '\u{c0}'..='\u{24f}' if c.is_alphabetic())
}

fn is_vowel(c: char) -> bool {
    let base = c.to_lowercase().nfd().next().unwrap_or(c);
    matches!(base, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'æ' | 'œ' | 'ø')
}

// ---------------------------------------------------------------------------------------------
// Running headers and footers
// ---------------------------------------------------------------------------------------------

/// A line made of a page number only: "3", "- 3 -", "Page 3", "3 / 12", "page 3 sur 12".
static PAGE_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(page\s*)?[-–—]?\s*\d{1,4}\s*[-–—]?\s*((/|sur|of|de)\s*\d{1,4})?\s*$").unwrap()
});
/// A line that names its page ("Confidentiel - page 3", "p. 3"): its numbers change from page
/// to page, the rest repeats.
static NAMES_ITS_PAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bpage\s*\d|\bp\.\s*\d").unwrap());
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
/// A short line ending with its page number ("Confidentiel - page 1"): furniture even on a
/// document of one page, where nothing repeats.
static ENDS_WITH_PAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bpage\s*\d{1,4}(\s*(/|sur|of|de)\s*\d{1,4})?\s*$").unwrap());

fn is_page_marker(text: &str) -> bool {
    PAGE_NUMBER.is_match(text) || (text.chars().count() <= 40 && ENDS_WITH_PAGE.is_match(text))
}

/// What two running lines have in common. Digits are only wildcarded in page numbering:
/// elsewhere a table row with a date or an amount at the edge of each page would match the
/// rows of other pages and be dropped as a header.
fn running_key(text: &str) -> String {
    let key = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    if NAMES_ITS_PAGE.is_match(&key) || PAGE_NUMBER.is_match(&key) {
        DIGITS.replace_all(&key, "#").into_owned()
    } else {
        key
    }
}

/// Drops, near the top and bottom of pages, the lines that repeat on at least half the pages
/// (and on two at least), and page numbers. Returns how many were dropped.
fn remove_running_lines(pages: &mut [Page]) -> usize {
    let repeating = pages.iter().filter(|page| page.lines.iter().any(Line::is_text)).count() >= 2;
    let edges = |page: &Page| -> Vec<usize> {
        let text: Vec<usize> = (0..page.lines.len()).filter(|&i| page.lines[i].is_text()).collect();
        let mut edge: Vec<usize> = text.iter().take(EDGE_LINES).chain(text.iter().rev().take(EDGE_LINES)).copied().collect();
        edge.sort_unstable();
        edge.dedup();
        edge
    };
    let with_text: Vec<usize> = (0..pages.len()).filter(|&i| pages[i].lines.iter().any(Line::is_text)).collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for &index in with_text.iter().filter(|_| repeating) {
        let page = &pages[index];
        let keys: HashSet<String> = edges(page).into_iter().map(|i| running_key(&page.lines[i].text)).collect();
        for key in keys {
            *counts.entry(key).or_default() += 1;
        }
    }
    let need = with_text.len().div_ceil(2).max(2);
    let mut removed = 0;
    for &index in &with_text {
        let page = &mut pages[index];
        let drop: HashSet<usize> = edges(page)
            .into_iter()
            .filter(|&i| {
                let text = &page.lines[i].text;
                counts.get(&running_key(text)).is_some_and(|&n| n >= need) || is_page_marker(text)
            })
            .collect();
        removed += drop.len();
        let mut position = 0;
        page.lines.retain(|_| {
            position += 1;
            !drop.contains(&(position - 1))
        });
    }
    removed
}

// ---------------------------------------------------------------------------------------------
// Paragraphs
// ---------------------------------------------------------------------------------------------

/// A line about to be joined, with what the decision needs to know about its surroundings.
struct Candidate<'a> {
    line: &'a Line,
    /// The usual width of a full line on its page.
    full: usize,
    /// Its block (lines between blank lines, on one page) reads as running prose.
    prose: bool,
}

/// An item marker opens a new paragraph: bullets, dashes, "1.", "a)".
static ITEM_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([•◦▪‣·∙○■□➢►–—-]|\d{1,3}[.)]|[a-zA-Z][.)])\s").unwrap());

/// Hyphens that only cut a word: soft hyphen, and the Unicode hyphens readers emit for it.
const SOFT_HYPHENS: [char; 3] = ['\u{ad}', '\u{2010}', '\u{2011}'];

/// Words after which a line-end hyphen is a compound's own ("après-midi", "porte-monnaie").
const COMPOUND_FIRST: [&str; 10] = ["après", "avant", "arrière", "porte", "garde", "demi", "semi", "mi", "vice", "sans"];
/// Words before which it is ("celle-ci", "c'est-à-dire", "dis-moi", "a-t-il").
const COMPOUND_SECOND: [&str; 23] = [
    "ci", "là", "même", "mêmes", "dire", "vis", "midi", "t", "il", "ils", "elle", "elles", "on", "je", "tu", "nous",
    "vous", "moi", "toi", "lui", "leur", "en", "y",
];

fn percentile_90(mut widths: Vec<usize>) -> usize {
    if widths.is_empty() {
        return 0;
    }
    widths.sort_unstable();
    widths[(widths.len() * 9 / 10).min(widths.len() - 1)]
}

/// Whether a block of lines is running prose: mostly letters, wide enough, and its lines but
/// the last one nearly full. Forms, payslips and statements are short lines or numbers: joined,
/// their diffs got worse.
fn is_prose(block: &[&Line], full: usize) -> bool {
    if full < MIN_PROSE_WIDTH || block.is_empty() {
        return false;
    }
    let (mut visible, mut letters) = (0usize, 0usize);
    for c in block.iter().flat_map(|line| line.text.chars()).filter(|c| !c.is_whitespace()) {
        visible += 1;
        letters += usize::from(c.is_alphabetic());
    }
    if letters * 10 < visible * 6 || block.iter().filter(|line| line.table).count() * 2 > block.len() {
        return false;
    }
    // The lines that should be full if this is prose: not the last, not one before an item,
    // not a short one closing a sentence (paragraphs without blank lines between them).
    let body: Vec<&&Line> = block
        .iter()
        .zip(block.iter().skip(1))
        .filter(|(line, next)| {
            let closes = line.text.ends_with(['.', '!', '?', '…']) && line.width * 10 < full * 6;
            !closes && !ITEM_START.is_match(&next.text)
        })
        .map(|(line, _)| line)
        .collect();
    // No such line: a lone line, the end of a paragraph begun on the page before, or a heading;
    // the lines around it decide.
    body.is_empty() || body.iter().filter(|line| line.width * 10 >= full * 6).count() * 2 >= body.len()
}

/// The decision to continue `prev`'s paragraph with `next`.
fn joins(prev: &Candidate<'_>, next: &Candidate<'_>, mode: PdfParagraphs) -> bool {
    if prev.line.table || next.line.table || ITEM_START.is_match(&next.line.text) {
        return false;
    }
    let Some(first) = next.line.text.chars().next() else {
        return false;
    };
    match mode {
        PdfParagraphs::Never => false,
        PdfParagraphs::Always => true,
        PdfParagraphs::Auto => {
            let cut_word = first.is_alphabetic();
            if prev.line.text.ends_with(SOFT_HYPHENS) && cut_word {
                return true;
            }
            if !prev.prose || !next.prose {
                return false;
            }
            let (width, full) = (prev.line.width * 100, prev.full);
            let at_least = |percent: usize| width >= full * percent;
            match prev.line.text.chars().last() {
                Some('-') if cut_word => at_least(50),
                Some(c) if c.is_alphabetic() || matches!(c, ',' | ';' | '\'' | '’' | '«' | '(' | '"' | '“') => {
                    if first.is_lowercase() { at_least(40) } else { at_least(85) }
                }
                // A sentence ends: only a line that filled its width goes on.
                Some('.' | '!' | '?' | '…' | '»' | '”') => {
                    if first.is_lowercase() { at_least(60) } else { at_least(92) }
                }
                // Digits, amounts, percentages, ")" and ":" end rows and labels, not lines of
                // prose.
                _ => false,
            }
        }
    }
}

/// A blank line ends a paragraph, unless the line before stops mid-sentence at full width and
/// the next goes on in lowercase: the bottom of a column, then the top of the next one.
fn resumes_after_gap(prev: &Candidate<'_>, next: &Line) -> bool {
    let stops_mid_sentence = prev.line.text.ends_with(|c: char| c.is_alphabetic() || c == ',');
    prev.prose
        && !prev.line.table
        && !next.table
        && !next.placeholder
        && stops_mid_sentence
        && prev.line.width * 100 >= prev.full * 85
        && next.text.starts_with(char::is_lowercase)
}

/// The words of the document, for telling a compound's hyphen from a line-end cut.
fn vocabulary(pages: &[Page]) -> HashSet<String> {
    pages
        .iter()
        .flat_map(|page| page.lines.iter())
        .filter(|line| line.is_text())
        .flat_map(|line| line.text.split_whitespace())
        .map(|word| clean_word(word).to_lowercase())
        .filter(|word| !word.is_empty())
        .collect()
}

fn clean_word(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
        .trim_end_matches('-')
        .chars()
        .filter(|&c| c != '\u{ad}')
        .collect()
}

/// Whether the hyphen ending `left` (a word ending in "-") belongs to the word, `right` being
/// the first word of the next line.
fn keeps_hyphen(left: &str, right: &str, words: &HashSet<String>) -> bool {
    let stem = left.trim_end_matches('-').trim_start_matches(|c: char| !c.is_alphanumeric());
    let right = right.trim_end_matches(|c: char| !c.is_alphanumeric());
    let (stem_lower, right_lower) = (stem.to_lowercase(), right.to_lowercase());
    if words.contains(&format!("{stem_lower}-{right_lower}")) {
        return true;
    }
    if words.contains(&format!("{stem_lower}{right_lower}")) {
        return false;
    }
    let first_part = stem_lower.rsplit(['-', '\'', '’']).next().unwrap_or("");
    let second_part = right_lower.split('-').next().unwrap_or("");
    right.starts_with(char::is_uppercase)
        || stem.contains('-')
        || COMPOUND_FIRST.contains(&first_part)
        || COMPOUND_SECOND.contains(&second_part)
}

/// Appends `next` to the paragraph, resolving a hyphen at the end of the paragraph so far.
fn append(paragraph: &mut String, next: &str, words: &HashSet<String>) {
    let without_soft = paragraph.trim_end_matches(SOFT_HYPHENS).len();
    if without_soft < paragraph.len() {
        paragraph.truncate(without_soft);
        paragraph.push_str(next);
        return;
    }
    let cut = paragraph.ends_with('-')
        && paragraph[..paragraph.len() - 1].ends_with(char::is_alphabetic)
        && next.starts_with(char::is_alphabetic);
    if cut {
        let left = paragraph.rsplit(char::is_whitespace).next().unwrap_or("");
        let right = next.split_whitespace().next().unwrap_or("");
        if !keeps_hyphen(left, right, words) {
            paragraph.pop();
        }
        paragraph.push_str(next);
    } else {
        paragraph.push(' ');
        paragraph.push_str(next);
    }
}

/// The paragraphs of the document, each with the page it starts on. Page breaks do not end a
/// paragraph by themselves; blank lines do.
fn join_paragraphs(pages: &[Page], mode: PdfParagraphs) -> Vec<(u32, String)> {
    let all_widths = |page: &Page| page.lines.iter().filter(|line| line.is_text()).map(|line| line.width).collect::<Vec<_>>();
    let document_full = percentile_90(pages.iter().flat_map(all_widths).collect());
    let words = vocabulary(pages);

    let mut paragraphs = Vec::new();
    let mut current: Option<(u32, String)> = None;
    let mut previous: Option<Candidate<'_>> = None;
    for page in pages {
        let widths = all_widths(page);
        // Few lines say little about the page's width; but they are a bound when the page is
        // narrower than the rest (a portrait page after a landscape one).
        let full = if widths.len() >= MIN_LINES_FOR_WIDTH {
            percentile_90(widths)
        } else {
            widths.iter().copied().max().unwrap_or(0).min(document_full)
        };
        // Blank lines end a paragraph only between texts of one page: at a page's top or
        // bottom they are what is left around removed headers and footers.
        let first_text = page.lines.iter().position(|line| !line.text.is_empty());
        let last_text = page.lines.iter().rposition(|line| !line.text.is_empty());
        let (Some(first_text), Some(last_text)) = (first_text, last_text) else {
            continue;
        };
        let mut start = first_text;
        while start <= last_text {
            if page.lines[start].text.is_empty() {
                let next = page.lines[start..=last_text].iter().find(|line| !line.text.is_empty());
                let resumes = mode == PdfParagraphs::Auto
                    && previous.as_ref().zip(next).is_some_and(|(prev, next)| resumes_after_gap(prev, next));
                if !resumes {
                    paragraphs.extend(current.take());
                    previous = None;
                }
                start += 1;
                continue;
            }
            let end = (start..=last_text).find(|&i| page.lines[i].text.is_empty()).unwrap_or(last_text + 1);
            let block = &page.lines[start..end];
            start = end;
            let text_lines: Vec<&Line> = block.iter().filter(|line| !line.placeholder).collect();
            let prose = is_prose(&text_lines, full);
            for line in block {
                if line.placeholder {
                    paragraphs.extend(current.take());
                    paragraphs.push((page.number, line.text.clone()));
                    previous = None;
                    continue;
                }
                let candidate = Candidate { line, full, prose };
                match (&mut current, &previous) {
                    (Some((_, paragraph)), Some(prev)) if joins(prev, &candidate, mode) => {
                        append(paragraph, &line.text, &words);
                    }
                    _ => {
                        paragraphs.extend(current.take());
                        current = Some((page.number, line.text.clone()));
                    }
                }
                previous = Some(candidate);
            }
        }
    }
    paragraphs.extend(current);
    paragraphs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pages: &[&str]) -> RawPdf {
        RawPdf {
            pages: pages.iter().map(|text| RawPage::Text((*text).to_owned())).collect(),
            skipped: 0,
        }
    }

    fn text(pages: &[&str]) -> String {
        assemble(&raw(pages), &ExtractOptions::default()).text
    }

    fn with(mode: PdfParagraphs, pages: &[&str]) -> String {
        let options = ExtractOptions {
            pdf_paragraphs: mode,
            ..ExtractOptions::default()
        };
        assemble(&raw(pages), &options).text
    }

    /// A paragraph of running prose cut into lines of about 70 characters.
    const PROSE: &str = "Le comité de direction s'est réuni le mardi matin pour examiner les\n\
                         résultats du trimestre écoulé. Les ventes ont progressé dans toutes les\n\
                         régions, à l'exception du secteur nord où la concurrence est restée\n\
                         très vive.";
    const PROSE_JOINED: &str = "Le comité de direction s'est réuni le mardi matin pour examiner les résultats du \
                                trimestre écoulé. Les ventes ont progressé dans toutes les régions, à l'exception du \
                                secteur nord où la concurrence est restée très vive.\n";

    #[test]
    fn running_prose_is_joined_into_one_line_per_paragraph() {
        assert_eq!(text(&[PROSE]), PROSE_JOINED);
        let two = format!("{PROSE}\n\n{PROSE}");
        assert_eq!(text(&[&two]), format!("{PROSE_JOINED}{PROSE_JOINED}"), "a blank line ends a paragraph");
    }

    #[test]
    fn forms_statements_and_tables_keep_their_lines() {
        let form = "Nom : Dupont\nPrénom : Jean\nAdresse : 12 rue des Lilas\nVille : Paris";
        assert_eq!(text(&[form]), format!("{form}\n"));
        let statement = "Le solde de votre compte au 31 mars s'élève maintenant à la somme de 1 234,56 €\n\
                         Les frais de tenue de compte prélevés ce trimestre représentent environ 12 %\n\
                         Les opérations du mois sont détaillées au verso de ce relevé (page 2)\n\
                         Pour toute question, votre conseiller se tient à votre disposition :\n\
                         par téléphone ou à l'agence, du lundi au vendredi.";
        assert_eq!(text(&[statement]).lines().count(), 5, "no join after €, %, ) or :");
        let table = "Article      Quantité      Prix\nClavier sans fil      2      45,00 €\nSouris optique      3      19,90 €";
        assert_eq!(text(&[table]).lines().count(), 3);
    }

    #[test]
    fn a_sentence_end_goes_on_only_when_its_line_is_full() {
        let short_end = "Le comité de direction s'est réuni le mardi matin pour examiner les\n\
                         résultats du trimestre écoulé.\n\
                         Les ventes ont progressé dans toutes les régions, à l'exception du nord.";
        assert_eq!(text(&[short_end]).lines().count(), 2);
        let full_end = "Le comité de direction s'est réuni le mardi matin pour examiner tout.\n\
                        Les ventes ont progressé dans toutes les régions, à l'exception du nord\n\
                        où la concurrence est restée très vive.";
        assert_eq!(text(&[full_end]).lines().count(), 1);
    }

    #[test]
    fn the_mode_can_force_or_forbid_joining() {
        let form = "Nom : Dupont\nPrénom : Jean";
        assert_eq!(with(PdfParagraphs::Always, &[form]), "Nom : Dupont Prénom : Jean\n");
        assert_eq!(with(PdfParagraphs::Never, &[PROSE]), format!("{PROSE}\n"));
        let items = "• Premier point assez long pour remplir une ligne entière de texte courant\n• Second point";
        assert_eq!(with(PdfParagraphs::Always, &[items]).lines().count(), 2, "an item starts a paragraph");
    }

    #[test]
    fn line_end_hyphens_are_dropped_unless_the_word_is_a_compound() {
        let soft = "Le comité de direction s'est réuni le mardi matin pour un examen extraor\u{ad}\u{2010}\n\
                    dinaire des résultats du trimestre écoulé, dans toutes les régions du pays.";
        assert!(text(&[soft]).contains("examen extraordinaire des"), "{}", text(&[soft]));
        let cut = "Le comité de direction s'est réuni le mardi matin pour un examen extraor-\n\
                   dinaire des résultats du trimestre écoulé, dans toutes les régions du pays.";
        assert!(text(&[cut]).contains("examen extraordinaire des"));
        let seen = "Le porte-monnaie était resté sur la table du salon depuis la veille au soir, et le porte-\n\
                    monnaie de sa sœur avait disparu du tiroir de la commode de l'entrée hier.";
        assert!(text(&[seen]).contains("le porte-monnaie de sa sœur"), "{}", text(&[seen]));
        let compound = "Il faut relire la note avant de signer la convention, c'est-à-\n\
                        dire avant la réunion du conseil d'administration prévue la semaine prochaine.";
        assert!(text(&[compound]).contains("c'est-à-dire avant"));
        let name = "Le contrat a été signé au nom de la société par son gérant, monsieur Jean-\n\
                    Pierre Martin, en présence des deux associés et du notaire chargé du dossier.";
        assert!(text(&[name]).contains("Jean-Pierre Martin"));
        let pattern = "Les enfants de six à dix ans sont accueillis chaque mercredi après-\n\
                       midi par les animateurs de la bibliothèque municipale du centre-ville.";
        assert!(text(&[pattern]).contains("après-midi par"));
    }

    #[test]
    fn running_headers_footers_and_page_numbers_are_dropped_but_not_table_rows() {
        let page = |number: u32, body: &str, row: &str| {
            format!("Société Exemple - Rapport interne\n\n{body}\n\n{row}\n\nPage {number} / 3")
        };
        let pages = [
            page(1, PROSE, "01/03/2026 Prélèvement électricité 45,00"),
            page(2, "Chaque collaborateur recevra une note détaillée.", "15/03/2026 Prélèvement électricité 47,10"),
            page(3, "La rénovation de la façade commencera au printemps.", "29/03/2026 Prélèvement électricité 46,20"),
        ];
        let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
        let extracted = assemble(&raw(&pages), &ExtractOptions::default());
        assert!(!extracted.text.contains("Société Exemple"));
        assert!(!extracted.text.contains("Page "));
        assert_eq!(extracted.text.matches("Prélèvement électricité").count(), 3, "rows differ by their numbers");
        assert_eq!(extracted.facts.headers_footers_removed, 6);
        let kept = ExtractOptions {
            pdf_headers_footers: true,
            ..ExtractOptions::default()
        };
        assert_eq!(assemble(&raw(&pages), &kept).text.matches("Société Exemple").count(), 3);
        // Alone on its page, "page N" still names a footer; a lone number at an edge too.
        assert_eq!(text(&[&format!("{PROSE}\n\nConfidentiel - page 1")]), PROSE_JOINED);
        assert_eq!(text(&[&format!("- 4 -\n{PROSE}")]), PROSE_JOINED);
    }

    #[test]
    fn a_page_break_does_not_end_a_paragraph_and_each_page_start_is_mapped() {
        let first = "Le comité de direction s'est réuni le mardi matin pour examiner les\n\
                     résultats du trimestre écoulé. Les ventes ont progressé dans toutes les";
        let second = "régions, à l'exception du secteur nord où la concurrence est restée\n\
                      très vive.\n\n\
                      Chaque collaborateur recevra une note détaillée.";
        let third = "Les justificatifs devront être conservés pendant trois ans au moins.";
        let extracted = assemble(&raw(&[first, second, third]), &ExtractOptions::default());
        assert_eq!(extracted.text.lines().next().unwrap(), PROSE_JOINED.trim_end());
        assert_eq!(extracted.text.lines().count(), 3);
        // The paragraph straddling pages 1 and 2 belongs to page 1; page 2's first own line
        // is the second line.
        assert_eq!(
            extracted.page_starts,
            [PageStart { line: 0, page: 1 }, PageStart { line: 1, page: 2 }, PageStart { line: 2, page: 3 }]
        );
    }

    #[test]
    fn a_page_moved_by_a_page_break_gives_the_same_text() {
        let early = ["Premier paragraphe.\n\nDeuxième paragraphe.", "Troisième paragraphe."];
        let late = ["Premier paragraphe.", "Deuxième paragraphe.\n\nTroisième paragraphe."];
        assert_eq!(text(&early), text(&late));
    }

    #[test]
    fn garbage_is_told_from_words() {
        let shifted = "5 DSSRUW LQWHUQH VXU ODFWLYLWH 6HFWLRQ 5HVXOWDWV /H FRPLWH GH GLUHFWLRQ VH VWUHXQLOH \
                       PDUGL PDWLQ SRXU HADPLQHU OHV UHVXOWDWV GX WULPHVWUH HFRXOH /HV YHQWHV RQW";
        assert!(is_illegible(shifted));
        assert!(is_illegible(&"!\" # $ %!& ’$#!’ ($ )&#&$ (&*) + ,- . /- 9:;< %% $# %!& # &=$ ( > %’& #? @\n".repeat(3)));
        assert!(is_illegible(&"\u{e001}\u{e002}\u{e003} abc ".repeat(20)));
        assert!(!is_illegible(PROSE));
        let payslip = "SALAIRE DE BASE 151,67 11,6500 1 766,96\nCSG DEDUCTIBLE 1 735,50 6,800 118,01\n\
                       URSSAF MALADIE 1 766,96 7,000 123,69\nNET A PAYER AVANT IMPOT 1 391,23\nTTC HT TVA 20 %";
        assert!(!is_illegible(payslip));
        assert!(!is_illegible("Introduction ........................................ 3\nChapitre premier ........................... 7"));
        assert!(!is_illegible("今天天气很好，我们去公园散步吧。東京は日本の首都です。桜の季節はとても美しい。"));
    }

    #[test]
    fn an_illegible_page_becomes_one_fingerprinted_line() {
        let garbage = "5 DSSRUW LQWHUQH VXU ODFWLYLWH 6HFWLRQ 5HVXOWDWV /H FRPLWH GH GLUHFWLRQ VH VWUHXQLOH \
                       PDUGL PDWLQ SRXU HADPLQHU OHV UHVXOWDWV GX WULPHVWUH HFRXOH /HV YHQWHV RQW";
        let extracted = assemble(&raw(&[PROSE, garbage]), &ExtractOptions::default());
        assert_eq!(extracted.facts.illegible_pages, 1);
        let placeholder = extracted.text.lines().nth(1).unwrap();
        assert!(placeholder.starts_with("[page illisible : texte non comparé, empreinte "), "{placeholder}");
        assert_eq!(extracted.page_starts[1], PageStart { line: 1, page: 2 });
        let other = assemble(&raw(&[PROSE, &garbage.replace("5 ", "6 ")]), &ExtractOptions::default());
        assert_ne!(other.text, extracted.text, "other garbage, another fingerprint");
        assert_eq!(assemble(&raw(&[PROSE, garbage]), &ExtractOptions::default()).text, extracted.text);
    }

    #[test]
    fn pages_past_the_budget_are_counted_unread() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pdf-corpus/synth/h01_paragraphs_pages/doc.pdf");
        let budget = Budget {
            pages: 1,
            ..BUDGET
        };
        let read = read_guarded(std::fs::read(path).unwrap(), None, budget).unwrap();
        assert_eq!((read.pages.len(), read.skipped), (1, 2));
        let extracted = assemble(&read, &ExtractOptions::default());
        assert_eq!(extracted.facts.unread_pages, 2);
        assert!(extracted.facts.notices(&ExtractOptions::default()).contains(&"2 pages non lues (endommagées, ou limite de temps atteinte)".to_owned()));
    }
}
