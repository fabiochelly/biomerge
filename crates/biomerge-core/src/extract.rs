//! Text extraction from word-processing documents (.docx and .odt families), for comparison.
//!
//! The output is meant for the line diff, not for rendering: one paragraph is one line, never
//! hard-wrapped, so that the word-level diff inside paired lines shows exactly what changed in a
//! paragraph. A light Markdown-like structure (headings, list items, pipe tables, notes) keeps
//! the document navigable and gets Markdown colouring for free.
//!
//! Everything that would make two equal texts look different is left out or made deterministic:
//! ordered items all start with `1.` (a real number would renumber every following item after
//! an insertion), notes are inlined where they are referenced (no numbering to shift), page and
//! section breaks are dropped, automatic numbering is not reproduced. What is left out is
//! counted in [`DocFacts`], so the interface can say what was not compared.
//!
//! Both formats are zip archives of XML parts. Each part is read under a size cap, parsed into a
//! small tree whose names are rewritten with canonical namespace prefixes (`w:p`, `text:p`),
//! then walked into format-independent blocks rendered by one common function.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use unicode_normalization::{UnicodeNormalization, is_nfc};
use zip::ZipArchive;
use zip::result::ZipError;

/// A path with this name gives the right grammar to `highlight::detect_syntax`.
pub const SYNTAX_HINT: &str = "document.md";

/// Decompressed size cap of one XML part: a zip bomb is refused rather than read.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;
/// A flat .fodt embeds its images as base64, hence a larger cap for the single file.
const MAX_FLAT_BYTES: u64 = 256 * 1024 * 1024;
/// Elements per part: bounds the memory of the tree whatever the markup density.
const MAX_ELEMENTS: usize = 3_000_000;
/// Nesting depth: the walk is recursive, this keeps it far from the end of the stack.
const MAX_DEPTH: usize = 200;
/// How far an OLE container is searched for the marker of an encrypted package.
const OLE_PROBE_BYTES: u64 = 8 * 1024 * 1024;
const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Inside paragraph text: a line break (`w:br`, `text:line-break`).
const BREAK: char = '\n';
/// Inside paragraph text: a paragraph boundary restored from tracked deletions.
const SPLIT: char = '\u{2029}';
/// Joins the paragraphs of a table cell or of a note, which must stay on one line.
const PARAGRAPH_JOIN: &str = " ¶ ";

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: ni une archive zip ni un document XML, document Word ou OpenDocument attendu")]
    NotZip(PathBuf),
    #[error("{0}: document chiffré ou protégé par mot de passe")]
    Encrypted(PathBuf),
    #[error("{0}: ancien format binaire (Word 97-2003), non pris en charge")]
    LegacyBinary(PathBuf),
    #[error("{0}: ce n'est pas un document texte Word ou OpenDocument")]
    Unsupported(PathBuf),
    #[error("{path}: partie « {part} » absente, document incomplet")]
    MissingPart { path: PathBuf, part: String },
    #[error("{path}: XML mal formé dans « {part} » ({detail})")]
    Xml {
        path: PathBuf,
        part: String,
        detail: String,
    },
    #[error("{path}: « {part} » dépasse la taille admise ({limit_mb} Mo décompressés)")]
    TooLarge {
        path: PathBuf,
        part: String,
        limit_mb: u64,
    },
    #[error("{path}: archive endommagée, « {part} » illisible ({detail})")]
    Corrupt {
        path: PathBuf,
        part: String,
        detail: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DocFormat {
    /// Office Open XML: .docx, .docm, .dotx, .dotm.
    Docx,
    /// OpenDocument Text: .odt, .ott, flat .fodt.
    Odt,
}

impl DocFormat {
    pub fn label(self) -> &'static str {
        match self {
            DocFormat::Docx => "Word",
            DocFormat::Odt => "OpenDocument",
        }
    }

    /// The format announced by the file name. The content decides when extracting.
    pub fn from_extension(path: &Path) -> Option<DocFormat> {
        let name = path.file_name()?.to_str()?;
        // Word's owner files (`~$report.docx`) carry the extension without being documents.
        if name.starts_with("~$") {
            return None;
        }
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "docx" | "docm" | "dotx" | "dotm" => Some(DocFormat::Docx),
            "odt" | "ott" | "fodt" => Some(DocFormat::Odt),
            _ => None,
        }
    }
}

/// Which side of the tracked changes is extracted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Revision {
    /// All changes accepted: insertions kept, deletions dropped.
    #[default]
    Final,
    /// All changes rejected: insertions dropped, deletions restored.
    Original,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NotePlacement {
    /// `^[note text]` where the note is referenced: no numbering, so adding a note does not
    /// touch the others, and a changed note shows inside its paragraph.
    #[default]
    Inline,
    /// `[^1]` in the text, `[^1]: note text` lines after the body, numbered by appearance.
    End,
    Omit,
}

/// Typographic noise removed before comparing. Each rule is separate: the difference between a
/// straight and a curly quote is sometimes exactly what is being looked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Normalization {
    /// Unicode NFC: `e` + combining accent equals `é`.
    pub nfc: bool,
    /// Non-breaking, thin, em... spaces become a space; zero-width spaces disappear.
    pub exotic_spaces: bool,
    /// Soft hyphens disappear, non-breaking hyphens become `-`.
    pub hyphens: bool,
    /// Curly quotes and apostrophes become straight, guillemets become `"` without their
    /// inner spaces.
    pub fold_quotes: bool,
    /// En and em dashes become `-`, the ellipsis character becomes `...`.
    pub fold_dashes: bool,
    /// `ﬁ`, `ﬂ`... become their letters.
    pub fold_ligatures: bool,
    /// Runs of spaces become one space.
    pub collapse_spaces: bool,
    /// Spaces and tabs at the end of a paragraph disappear.
    pub trim_trailing: bool,
}

impl Default for Normalization {
    /// Invisible differences are removed, visible ones (quotes, dashes) are kept.
    fn default() -> Self {
        Self {
            nfc: true,
            exotic_spaces: true,
            hyphens: true,
            fold_quotes: false,
            fold_dashes: false,
            fold_ligatures: true,
            collapse_spaces: true,
            trim_trailing: true,
        }
    }
}

impl Normalization {
    /// The text exactly as stored in the document.
    pub fn none() -> Self {
        Self {
            nfc: false,
            exotic_spaces: false,
            hyphens: false,
            fold_quotes: false,
            fold_dashes: false,
            fold_ligatures: false,
            collapse_spaces: false,
            trim_trailing: false,
        }
    }

    /// Everything folded: only the words are compared.
    pub fn aggressive() -> Self {
        Self {
            fold_quotes: true,
            fold_dashes: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExtractOptions {
    pub revision: Revision,
    pub notes: NotePlacement,
    /// `[text](url)` when a link's address is not already its text; the text alone otherwise.
    pub link_targets: bool,
    /// `[image: alternative text]` where a picture, `[objet]` where a chart or an embedded
    /// object is anchored.
    pub images: bool,
    /// Comments as `[commentaire de …] …` lines after the body.
    pub comments: bool,
    /// Headers and footers as `[en-tête] …` and `[pied de page] …` lines after the body.
    pub headers_footers: bool,
    /// Empty paragraphs as empty lines. Off, vertical spacing made of empty paragraphs is not
    /// compared.
    pub keep_empty_paragraphs: bool,
    pub normalization: Normalization,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            revision: Revision::Final,
            notes: NotePlacement::Inline,
            link_targets: true,
            images: true,
            comments: false,
            headers_footers: false,
            keep_empty_paragraphs: false,
            normalization: Normalization::default(),
        }
    }
}

/// What the document contains besides its text, so the interface can say what is not compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocFacts {
    pub images: usize,
    /// Charts, diagrams, embedded (OLE) objects.
    pub objects: usize,
    pub equations: usize,
    pub tables: usize,
    pub text_boxes: usize,
    pub footnotes: usize,
    pub endnotes: usize,
    pub comments: usize,
    /// Tracked-change marks, not changed words: one inserted paragraph can count twice (its
    /// text and its paragraph mark).
    pub tracked_insertions: usize,
    pub tracked_deletions: usize,
    /// At least one header or footer has text.
    pub has_headers_footers: bool,
    pub has_macros: bool,
}

impl DocFacts {
    pub fn has_tracked_changes(&self) -> bool {
        self.tracked_insertions + self.tracked_deletions > 0
    }

    /// Short French sentences for a badge or a tooltip: what is in the file that the extracted
    /// text does not show, or shows on one side only of a choice.
    pub fn notices(&self, options: &ExtractOptions) -> Vec<String> {
        let count = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        let mut notices = Vec::new();
        if self.has_tracked_changes() {
            notices.push(match options.revision {
                Revision::Final => "modifications suivies : texte final affiché".to_owned(),
                Revision::Original => "modifications suivies : texte d'origine affiché".to_owned(),
            });
        }
        if self.images > 0 {
            notices.push(count(self.images, "image non comparée", "images non comparées"));
        }
        if self.objects > 0 {
            notices.push(count(
                self.objects,
                "objet incorporé non comparé",
                "objets incorporés non comparés",
            ));
        }
        if self.equations > 0 {
            notices.push(count(
                self.equations,
                "équation réduite à son texte",
                "équations réduites à leur texte",
            ));
        }
        if self.comments > 0 && !options.comments {
            notices.push(count(self.comments, "commentaire ignoré", "commentaires ignorés"));
        }
        let notes = self.footnotes + self.endnotes;
        if notes > 0 && options.notes == NotePlacement::Omit {
            notices.push(count(notes, "note ignorée", "notes ignorées"));
        }
        if self.has_headers_footers && !options.headers_footers {
            notices.push("en-têtes et pieds de page ignorés".to_owned());
        }
        if self.has_macros {
            notices.push("macros non comparées".to_owned());
        }
        notices
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extracted {
    pub format: DocFormat,
    /// Lines ended by `\n`, one per paragraph, table row or note.
    pub text: String,
    pub facts: DocFacts,
}

/// Cheap test by file name, for the interface and for folder comparison.
pub fn is_supported(path: &Path) -> bool {
    DocFormat::from_extension(path).is_some()
}

/// Extracts the text of a document. The format is recognised from the content, whatever the
/// extension.
pub fn extract(path: &Path, options: &ExtractOptions) -> Result<Extracted, ExtractError> {
    let file = File::open(path).map_err(|source| ExtractError::Io {
        path: path.to_owned(),
        source,
    })?;
    extract_from(BufReader::new(file), path, options)
}

/// Same as [`extract`] for a document already in memory; `path` only names it in errors.
pub fn extract_bytes(
    bytes: &[u8],
    path: &Path,
    options: &ExtractOptions,
) -> Result<Extracted, ExtractError> {
    extract_from(Cursor::new(bytes), path, options)
}

/// The extracted text alone.
pub fn extract_text(path: &Path, options: &ExtractOptions) -> Result<String, ExtractError> {
    extract(path, options).map(|extracted| extracted.text)
}

/// Whether two documents have the same text, however different their bytes are.
pub fn same_text(
    left: &Path,
    right: &Path,
    options: &ExtractOptions,
) -> Result<bool, ExtractError> {
    Ok(extract_text(left, options)? == extract_text(right, options)?)
}

/// A 64-bit hash of the extracted text, stable across runs: a cache key for folder comparison.
/// Equal hashes make equal texts very likely, not certain; [`same_text`] is the proof.
pub fn text_hash(path: &Path, options: &ExtractOptions) -> Result<u64, ExtractError> {
    extract_text(path, options).map(|text| hash_text(&text))
}

/// The hash used by [`text_hash`] (FNV-1a).
pub fn hash_text(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Applies the typographic rules to a text, line by line. Extraction already does it; this is
/// for texts coming from elsewhere that must compare equal to an extracted one.
pub fn normalize_text(text: &str, rules: &Normalization) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&normalize_line(line, rules));
    }
    out
}

fn is_space_like(c: char) -> bool {
    matches!(c, ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

fn normalize_line(line: &str, rules: &Normalization) -> String {
    let mut out = String::with_capacity(line.len());
    // French guillemets come with inner spaces that straight quotes do not have.
    let mut after_opening_guillemet = false;
    for c in line.chars() {
        if after_opening_guillemet && is_space_like(c) {
            continue;
        }
        after_opening_guillemet = false;
        match c {
            '\u{200b}' | '\u{2060}' | '\u{feff}' if rules.exotic_spaces => {}
            c if c != ' ' && is_space_like(c) && rules.exotic_spaces => out.push(' '),
            '\u{ad}' if rules.hyphens => {}
            '\u{2010}' | '\u{2011}' if rules.hyphens => out.push('-'),
            '‘' | '’' | '‚' | '‛' | '′' if rules.fold_quotes => out.push('\''),
            '“' | '”' | '„' | '‟' | '″' if rules.fold_quotes => out.push('"'),
            '«' if rules.fold_quotes => {
                out.push('"');
                after_opening_guillemet = true;
            }
            '»' if rules.fold_quotes => {
                out.truncate(out.trim_end_matches(is_space_like).len());
                out.push('"');
            }
            '–' | '—' | '―' | '−' if rules.fold_dashes => out.push('-'),
            '…' if rules.fold_dashes => out.push_str("..."),
            'ﬀ' if rules.fold_ligatures => out.push_str("ff"),
            'ﬁ' if rules.fold_ligatures => out.push_str("fi"),
            'ﬂ' if rules.fold_ligatures => out.push_str("fl"),
            'ﬃ' if rules.fold_ligatures => out.push_str("ffi"),
            'ﬄ' if rules.fold_ligatures => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' if rules.fold_ligatures => out.push_str("st"),
            c => out.push(c),
        }
    }
    if rules.nfc && !is_nfc(&out) {
        out = out.nfc().collect();
    }
    if rules.collapse_spaces && out.contains("  ") {
        let mut collapsed = String::with_capacity(out.len());
        for c in out.chars() {
            if c != ' ' || !collapsed.ends_with(' ') {
                collapsed.push(c);
            }
        }
        out = collapsed;
    }
    if rules.trim_trailing {
        out.truncate(out.trim_end_matches([' ', '\t']).len());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// XML tree
// ---------------------------------------------------------------------------------------------

/// An element with names rewritten to canonical prefixes, whatever prefixes the file uses.
#[derive(Debug, Default)]
struct Elem {
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
}

#[derive(Debug)]
enum Node {
    Elem(Elem),
    Text(String),
}

impl Elem {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn elems(&self) -> impl Iterator<Item = &Elem> {
        self.kids.iter().filter_map(|kid| match kid {
            Node::Elem(elem) => Some(elem),
            Node::Text(_) => None,
        })
    }

    fn child(&self, name: &str) -> Option<&Elem> {
        self.elems().find(|elem| elem.name == name)
    }

    /// First descendant with this name, in document order.
    fn find(&self, name: &str) -> Option<&Elem> {
        self.elems()
            .find_map(|elem| if elem.name == name { Some(elem) } else { elem.find(name) })
    }

    /// Text directly inside the element.
    fn push_text(&self, buf: &mut String) {
        for kid in &self.kids {
            if let Node::Text(text) = kid {
                buf.push_str(text);
            }
        }
    }

    fn text(&self) -> String {
        let mut text = String::new();
        self.push_text(&mut text);
        text
    }
}

enum XmlFailure {
    Malformed(String),
    TooLarge,
}

/// The prefix this module uses for a namespace. Transitional and Strict OOXML share prefixes.
fn canonical_prefix(uri: &str) -> Option<&'static str> {
    Some(match uri {
        "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
        | "http://purl.oclc.org/ooxml/wordprocessingml/main" => "w",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
        | "http://purl.oclc.org/ooxml/officeDocument/relationships" => "r",
        "http://schemas.openxmlformats.org/package/2006/relationships" => "rel",
        "http://schemas.openxmlformats.org/markup-compatibility/2006" => "mc",
        "http://schemas.openxmlformats.org/officeDocument/2006/math"
        | "http://purl.oclc.org/ooxml/officeDocument/math" => "m",
        "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
        | "http://purl.oclc.org/ooxml/drawingml/wordprocessingDrawing" => "wp",
        "http://schemas.openxmlformats.org/drawingml/2006/picture"
        | "http://purl.oclc.org/ooxml/drawingml/picture" => "pic",
        "http://schemas.openxmlformats.org/drawingml/2006/chart"
        | "http://purl.oclc.org/ooxml/drawingml/chart" => "c",
        "http://schemas.openxmlformats.org/drawingml/2006/diagram"
        | "http://purl.oclc.org/ooxml/drawingml/diagram" => "dgm",
        "urn:schemas-microsoft-com:vml" => "v",
        "urn:schemas-microsoft-com:office:office" => "o",
        "urn:oasis:names:tc:opendocument:xmlns:office:1.0" => "office",
        "urn:oasis:names:tc:opendocument:xmlns:text:1.0" => "text",
        "urn:oasis:names:tc:opendocument:xmlns:table:1.0" => "table",
        "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" => "draw",
        "urn:oasis:names:tc:opendocument:xmlns:style:1.0" => "style",
        "urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" => "svg",
        "http://www.w3.org/1999/xlink" => "xlink",
        "http://purl.org/dc/elements/1.1/" => "dc",
        _ => return None,
    })
}

/// `prefix:local` with the canonical prefix; `~prefix:local` in a namespace this module does
/// not know, so that it can never be mistaken for a known name.
fn qualify(raw: &str, scopes: &[(String, Option<&'static str>)], element: bool) -> String {
    let (prefix, local) = raw.split_once(':').unwrap_or(("", raw));
    // Unprefixed attributes are in no namespace.
    if (prefix.is_empty() && !element) || prefix == "xml" {
        return raw.to_owned();
    }
    match scopes.iter().rev().find(|(declared, _)| declared == prefix) {
        Some((_, Some(canonical))) => format!("{canonical}:{local}"),
        None if prefix.is_empty() => local.to_owned(),
        _ => format!("~{prefix}:{local}"),
    }
}

fn unescape_attribute(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(';') else { break };
        match resolve_entity(&rest[1..end]) {
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn resolve_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code).filter(|c| *c != '\0')
        }
    }
}

/// XML parts are UTF-8 in practice; OOXML also allows UTF-16.
fn decode_xml(bytes: &[u8]) -> Result<std::borrow::Cow<'_, str>, XmlFailure> {
    let utf16 = match bytes {
        [0xFF, 0xFE, ..] => Some(encoding_rs::UTF_16LE),
        [0xFE, 0xFF, ..] => Some(encoding_rs::UTF_16BE),
        _ => None,
    };
    if let Some(encoding) = utf16 {
        let (text, _, _) = encoding.decode(bytes);
        return Ok(text);
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    std::str::from_utf8(bytes)
        .map(std::borrow::Cow::Borrowed)
        .map_err(|_| XmlFailure::Malformed("UTF-8 invalide".to_owned()))
}

fn parse_xml(bytes: &[u8]) -> Result<Elem, XmlFailure> {
    let malformed = |detail: &dyn std::fmt::Display| XmlFailure::Malformed(detail.to_string());
    let text = decode_xml(bytes)?;
    let mut reader = Reader::from_str(&text);
    // Open elements, each with the number of namespace declarations in scope before it.
    let mut stack: Vec<(Elem, usize)> = Vec::new();
    let mut scopes: Vec<(String, Option<&'static str>)> = Vec::new();
    let mut root: Option<Elem> = None;
    let mut elements = 0usize;

    let open = |start: &BytesStart,
                scopes: &mut Vec<(String, Option<&'static str>)>|
     -> Result<Elem, XmlFailure> {
        fn utf8(bytes: &[u8]) -> Result<&str, XmlFailure> {
            std::str::from_utf8(bytes).map_err(|e| XmlFailure::Malformed(e.to_string()))
        }
        let mut attrs = Vec::new();
        for attr in start.attributes() {
            let attr = attr.map_err(|e| malformed(&e))?;
            let key = utf8(attr.key.as_ref())?;
            let value = unescape_attribute(utf8(&attr.value)?);
            if key == "xmlns" {
                scopes.push((String::new(), canonical_prefix(&value)));
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                scopes.push((prefix.to_owned(), canonical_prefix(&value)));
            } else {
                attrs.push((key.to_owned(), value));
            }
        }
        for (key, _) in &mut attrs {
            *key = qualify(key, scopes, false);
        }
        Ok(Elem {
            name: qualify(utf8(start.name().as_ref())?, scopes, true),
            attrs,
            kids: Vec::new(),
        })
    };
    let close = |elem: Elem, stack: &mut Vec<(Elem, usize)>, root: &mut Option<Elem>| {
        match stack.last_mut() {
            Some((parent, _)) => parent.kids.push(Node::Elem(elem)),
            None => *root = root.take().or(Some(elem)),
        }
    };
    let push_text = |text: &str, stack: &mut Vec<(Elem, usize)>| {
        let Some((parent, _)) = stack.last_mut() else { return };
        // Embedded pictures of flat documents: megabytes of base64 nobody reads.
        if parent.name == "office:binary-data" {
            return;
        }
        match parent.kids.last_mut() {
            Some(Node::Text(previous)) => previous.push_str(text),
            _ => parent.kids.push(Node::Text(text.to_owned())),
        }
    };

    loop {
        match reader.read_event().map_err(|e| malformed(&e))? {
            Event::Start(start) => {
                elements += 1;
                if elements > MAX_ELEMENTS {
                    return Err(XmlFailure::TooLarge);
                }
                if stack.len() >= MAX_DEPTH {
                    return Err(XmlFailure::Malformed("imbrication trop profonde".to_owned()));
                }
                let before = scopes.len();
                let elem = open(&start, &mut scopes)?;
                stack.push((elem, before));
            }
            Event::Empty(start) => {
                elements += 1;
                if elements > MAX_ELEMENTS {
                    return Err(XmlFailure::TooLarge);
                }
                let before = scopes.len();
                let elem = open(&start, &mut scopes)?;
                scopes.truncate(before);
                close(elem, &mut stack, &mut root);
            }
            Event::End(_) => {
                let Some((elem, before)) = stack.pop() else {
                    return Err(XmlFailure::Malformed("balise fermante en trop".to_owned()));
                };
                scopes.truncate(before);
                close(elem, &mut stack, &mut root);
            }
            Event::Text(text) => push_text(&text.decode().map_err(|e| malformed(&e))?, &mut stack),
            Event::CData(text) => {
                push_text(&text.decode().map_err(|e| malformed(&e))?, &mut stack)
            }
            Event::GeneralRef(reference) => {
                let name = reference.decode().map_err(|e| malformed(&e))?;
                if let Some(c) = resolve_entity(&name) {
                    push_text(c.encode_utf8(&mut [0; 4]), &mut stack);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(XmlFailure::Malformed("élément non fermé".to_owned()));
    }
    root.ok_or_else(|| XmlFailure::Malformed("aucun élément".to_owned()))
}

fn parse_part(path: &Path, part: &str, bytes: &[u8], limit: u64) -> Result<Elem, ExtractError> {
    parse_xml(bytes).map_err(|failure| match failure {
        XmlFailure::Malformed(detail) => ExtractError::Xml {
            path: path.to_owned(),
            part: part.to_owned(),
            detail,
        },
        XmlFailure::TooLarge => too_large(path, part, limit),
    })
}

fn too_large(path: &Path, part: &str, limit: u64) -> ExtractError {
    ExtractError::TooLarge {
        path: path.to_owned(),
        part: part.to_owned(),
        limit_mb: limit / (1024 * 1024),
    }
}

// ---------------------------------------------------------------------------------------------
// Container
// ---------------------------------------------------------------------------------------------

struct Package<'p, R> {
    zip: ZipArchive<R>,
    path: &'p Path,
}

impl<R: Read + Seek> Package<'_, R> {
    fn has(&self, name: &str) -> bool {
        self.zip.index_for_name(name).is_some()
    }

    /// The decompressed part, `None` when the archive has no such entry.
    fn read(&mut self, name: &str, limit: u64) -> Result<Option<Vec<u8>>, ExtractError> {
        let path = self.path;
        let corrupt = |detail: String| ExtractError::Corrupt {
            path: path.to_owned(),
            part: name.to_owned(),
            detail,
        };
        let mut file = match self.zip.by_name(name) {
            Ok(file) => file,
            Err(ZipError::FileNotFound) => return Ok(None),
            Err(ZipError::InvalidPassword) => return Err(ExtractError::Encrypted(path.to_owned())),
            Err(ZipError::UnsupportedArchive(why)) if why.to_lowercase().contains("password") => {
                return Err(ExtractError::Encrypted(path.to_owned()));
            }
            Err(error) => return Err(corrupt(error.to_string())),
        };
        if file.size() > limit {
            return Err(too_large(path, name, limit));
        }
        // The declared size can lie: the cap is enforced on what is really decompressed.
        let mut bytes = Vec::new();
        (&mut file)
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| corrupt(e.to_string()))?;
        if bytes.len() as u64 > limit {
            return Err(too_large(path, name, limit));
        }
        Ok(Some(bytes))
    }

    fn xml(&mut self, name: &str) -> Result<Option<Elem>, ExtractError> {
        match self.read(name, MAX_PART_BYTES)? {
            Some(bytes) => parse_part(self.path, name, &bytes, MAX_PART_BYTES).map(Some),
            None => Ok(None),
        }
    }

    fn missing(&self, part: &str) -> ExtractError {
        ExtractError::MissingPart {
            path: self.path.to_owned(),
            part: part.to_owned(),
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn extract_from<R: Read + Seek>(
    mut reader: R,
    path: &Path,
    options: &ExtractOptions,
) -> Result<Extracted, ExtractError> {
    let io = |source| ExtractError::Io {
        path: path.to_owned(),
        source,
    };
    let mut head = Vec::new();
    (&mut reader).take(8).read_to_end(&mut head).map_err(io)?;
    reader.seek(SeekFrom::Start(0)).map_err(io)?;

    if head.starts_with(b"PK") {
        let zip = ZipArchive::new(reader).map_err(|_| ExtractError::NotZip(path.to_owned()))?;
        return extract_package(Package { zip, path }, options);
    }
    if head == OLE_MAGIC {
        // Word encrypts a .docx by wrapping it in an OLE container, the format of old .doc
        // files: the stream name tells them apart.
        let mut bytes = Vec::new();
        reader.take(OLE_PROBE_BYTES).read_to_end(&mut bytes).map_err(io)?;
        let marker: Vec<u8> = "EncryptedPackage".encode_utf16().flat_map(u16::to_le_bytes).collect();
        return Err(if contains_bytes(&bytes, &marker) {
            ExtractError::Encrypted(path.to_owned())
        } else {
            ExtractError::LegacyBinary(path.to_owned())
        });
    }
    let after_bom = head
        .strip_prefix(&[0xEF, 0xBB, 0xBF])
        .unwrap_or(&head)
        .trim_ascii_start();
    let utf16 = head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]);
    if !utf16 && !after_bom.starts_with(b"<") {
        return Err(ExtractError::NotZip(path.to_owned()));
    }
    // A flat OpenDocument file: the same XML as in the archive, in one document.
    let mut bytes = Vec::new();
    reader.take(MAX_FLAT_BYTES + 1).read_to_end(&mut bytes).map_err(io)?;
    let part = "document XML";
    if bytes.len() as u64 > MAX_FLAT_BYTES {
        return Err(too_large(path, part, MAX_FLAT_BYTES));
    }
    let root = parse_part(path, part, &bytes, MAX_FLAT_BYTES)?;
    if root.name != "office:document" {
        return Err(ExtractError::Unsupported(path.to_owned()));
    }
    extract_odt(&root, Some(&root), options).ok_or_else(|| ExtractError::Unsupported(path.to_owned()))
}

fn extract_package<R: Read + Seek>(
    mut package: Package<'_, R>,
    options: &ExtractOptions,
) -> Result<Extracted, ExtractError> {
    let unsupported = || ExtractError::Unsupported(package.path.to_owned());
    let odt = match package.read("mimetype", 4096)? {
        Some(mime) => {
            let mime = String::from_utf8_lossy(&mime);
            if !mime.trim().starts_with("application/vnd.oasis.opendocument.text") {
                return Err(unsupported());
            }
            true
        }
        None if package.has("[Content_Types].xml") || package.has("word/document.xml") => false,
        None if package.has("content.xml") => true,
        None => return Err(unsupported()),
    };
    if !odt {
        return extract_docx(&mut package, options);
    }
    // An encrypted OpenDocument keeps its zip structure; the manifest lists what is encrypted.
    if let Some(manifest) = package.read("META-INF/manifest.xml", MAX_PART_BYTES)?
        && contains_bytes(&manifest, b"encryption-data")
    {
        return Err(ExtractError::Encrypted(package.path.to_owned()));
    }
    let content = package
        .xml("content.xml")?
        .ok_or_else(|| package.missing("content.xml"))?;
    let styles = package.xml("styles.xml")?;
    extract_odt(&content, styles.as_ref(), options).ok_or_else(unsupported)
}

// ---------------------------------------------------------------------------------------------
// Blocks: what both formats are reduced to
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Bullet,
    Number,
    /// A further paragraph of an item, aligned with its text.
    Continuation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Plain,
    Heading(u8),
    Item { level: u8, mark: Mark },
}

#[derive(Debug)]
enum Block {
    Para {
        kind: Kind,
        /// May contain [`BREAK`] and [`SPLIT`].
        text: String,
        /// The paragraph mark does not exist in the extracted revision: the text continues in
        /// the next paragraph.
        joins_next: bool,
    },
    /// Rows of cells of blocks.
    Table(Vec<Vec<Vec<Block>>>),
}

fn plain(text: String) -> Block {
    Block::Para {
        kind: Kind::Plain,
        text,
        joins_next: false,
    }
}

/// Joins each paragraph whose mark is tracked away with the paragraph that follows, which
/// gives its style to the result, as in the word processor.
fn merge_joined(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    let mut carry: Option<String> = None;
    for block in blocks {
        match block {
            Block::Para { kind, mut text, joins_next } => {
                if let Some(mut carried) = carry.take() {
                    carried.push_str(&text);
                    text = carried;
                }
                if joins_next {
                    carry = Some(text);
                } else {
                    out.push(Block::Para { kind, text, joins_next });
                }
            }
            table => {
                out.extend(carry.take().map(plain));
                out.push(table);
            }
        }
    }
    out.extend(carry.map(plain));
    out
}

/// Blocks on one line: for table cells, notes and comments.
fn flatten(blocks: &[Block]) -> String {
    let mut pieces: Vec<String> = Vec::new();
    for block in blocks {
        let piece = match block {
            Block::Para { kind, text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                let marker = match kind {
                    Kind::Item { mark: Mark::Bullet, .. } => "- ",
                    Kind::Item { mark: Mark::Number, .. } => "1. ",
                    _ => "",
                };
                let mut piece = String::from(marker);
                for c in text.trim().chars() {
                    match c {
                        BREAK => piece.push(' '),
                        SPLIT => piece.push_str(PARAGRAPH_JOIN),
                        c => piece.push(c),
                    }
                }
                piece
            }
            Block::Table(rows) => rows
                .iter()
                .map(|row| row.iter().map(|cell| flatten(cell)).collect::<Vec<_>>().join(" | "))
                .collect::<Vec<_>>()
                .join(PARAGRAPH_JOIN),
        };
        if !piece.trim().is_empty() {
            pieces.push(piece);
        }
    }
    pieces.join(PARAGRAPH_JOIN)
}

struct Output<'a> {
    lines: Vec<String>,
    options: &'a ExtractOptions,
    /// A blank line is due before the next line, if there is one.
    gap: bool,
}

impl Output<'_> {
    /// Asks for one blank line between what precedes and what follows. Deterministic, so two
    /// documents with the same structure get the same blank lines.
    fn gap(&mut self) {
        self.gap = !self.lines.is_empty();
    }

    fn line(&mut self, prefix: &str, text: &str) {
        let text = normalize_line(text, &self.options.normalization);
        if text.trim().is_empty() {
            if self.options.keep_empty_paragraphs && prefix.is_empty() {
                self.lines.push(String::new());
                self.gap = false;
            }
            return;
        }
        if self.gap && self.lines.last().is_some_and(|last| !last.is_empty()) {
            self.lines.push(String::new());
        }
        self.gap = false;
        self.lines.push(format!("{prefix}{text}"));
    }

    fn blocks(&mut self, blocks: &[Block]) {
        for block in blocks {
            match block {
                Block::Para { kind: Kind::Heading(level), text, .. } => {
                    self.gap();
                    let prefix = format!("{} ", "#".repeat(usize::from(*level)));
                    self.line(&prefix, text.replace([BREAK, SPLIT], " ").trim());
                }
                Block::Para { kind: Kind::Plain, text, .. } => {
                    for piece in text.split([BREAK, SPLIT]) {
                        self.line("", piece);
                    }
                }
                Block::Para { kind: Kind::Item { level, mark }, text, .. } => {
                    let indent = "  ".repeat(usize::from(*level));
                    let (first, rest) = match mark {
                        Mark::Bullet => ("- ", "  "),
                        Mark::Number => ("1. ", "   "),
                        Mark::Continuation => ("  ", "  "),
                    };
                    for (index, piece) in text.split([BREAK, SPLIT]).enumerate() {
                        let marker = if index == 0 { first } else { rest };
                        self.line(&format!("{indent}{marker}"), piece.trim_start());
                    }
                }
                Block::Table(rows) => {
                    self.gap();
                    for (index, row) in rows.iter().enumerate() {
                        let cells: Vec<String> = row.iter().map(|cell| flatten(cell)).collect();
                        self.line("", &format!("| {} |", cells.join(" | ")));
                        // The delimiter row makes it a Markdown table, coloured as such.
                        if index == 0 {
                            self.line("", &format!("|{}", "---|".repeat(cells.len().max(1))));
                        }
                    }
                    self.gap();
                }
            }
        }
    }

    fn group(&mut self, lines: &[String]) {
        self.gap();
        for line in lines {
            self.line("", line);
        }
    }

    fn finish(self) -> String {
        let mut text = self.lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        text
    }
}

/// What a walk accumulates besides the body blocks.
#[derive(Default)]
struct Collected {
    facts: DocFacts,
    /// `[^n]: …` lines, for [`NotePlacement::End`].
    notes: Vec<String>,
    comments: Vec<String>,
    /// Header and footer lines, without repetition.
    furniture: Vec<String>,
    in_note: bool,
}

impl Collected {
    fn note(&mut self, text: &str, options: &ExtractOptions, buf: &mut String) {
        let text = text.trim();
        match options.notes {
            NotePlacement::Inline => {
                buf.push_str("^[");
                buf.push_str(text);
                buf.push(']');
            }
            NotePlacement::End => {
                let number = self.notes.len() + 1;
                buf.push_str(&format!("[^{number}]"));
                self.notes.push(format!("[^{number}]: {text}"));
            }
            NotePlacement::Omit => {}
        }
    }

    fn comment(&mut self, author: &str, text: &str) {
        let author = author.trim();
        self.comments.push(if author.is_empty() {
            format!("[commentaire] {text}")
        } else {
            format!("[commentaire de {author}] {text}")
        });
    }

    fn furniture(&mut self, label: &str, blocks: &[Block], options: &ExtractOptions) {
        let text = flatten(blocks);
        if text.is_empty() {
            return;
        }
        self.facts.has_headers_footers = true;
        let line = format!("[{label}] {text}");
        // First-page, odd and even variants usually repeat the same text.
        if options.headers_footers && !self.furniture.contains(&line) {
            self.furniture.push(line);
        }
    }

    fn image(&mut self, alt: Option<&str>, options: &ExtractOptions, buf: &mut String) {
        self.facts.images += 1;
        if !options.images {
            return;
        }
        let alt = alt.map(|alt| alt.split_whitespace().collect::<Vec<_>>().join(" "));
        match alt.filter(|alt| !alt.is_empty()) {
            Some(alt) => buf.push_str(&format!("[image: {alt}]")),
            None => buf.push_str("[image]"),
        }
    }

    fn object(&mut self, options: &ExtractOptions, buf: &mut String) {
        self.facts.objects += 1;
        if options.images {
            buf.push_str("[objet]");
        }
    }

    fn finish(self, format: DocFormat, body: &[Block], options: &ExtractOptions) -> Extracted {
        let mut output = Output {
            lines: Vec::new(),
            options,
            gap: false,
        };
        output.blocks(body);
        output.group(&self.notes);
        output.group(&self.comments);
        output.group(&self.furniture);
        Extracted {
            format,
            text: output.finish(),
            facts: self.facts,
        }
    }
}

/// Turns `buf[start..]` into `[text](url)`, unless the text already says where the link goes.
fn wrap_link(buf: &mut String, start: usize, url: &str, options: &ExtractOptions) {
    fn bare(address: &str) -> String {
        let address = address.trim();
        let address = ["mailto:", "https://", "http://"]
            .iter()
            .find_map(|scheme| address.strip_prefix(scheme))
            .unwrap_or(address);
        address.trim_end_matches('/').to_lowercase()
    }
    if !options.link_targets || url.is_empty() || !buf.is_char_boundary(start) {
        return;
    }
    let text = &buf[start..];
    if text.trim().is_empty() || bare(text) == bare(url) {
        return;
    }
    let link = format!("[{text}]({url})");
    buf.truncate(start);
    buf.push_str(&link);
}

// ---------------------------------------------------------------------------------------------
// .docx
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Style {
    name: String,
    based_on: Option<String>,
    outline: Option<u8>,
    num_id: Option<String>,
    level: Option<u8>,
}

struct Relationship {
    id: String,
    /// Last segment of the type URI: `hyperlink`, `footnotes`, `header`...
    kind: String,
    target: String,
    external: bool,
}

struct Docx<'a> {
    options: &'a ExtractOptions,
    /// Relationship id to external address.
    links: HashMap<String, String>,
    styles: HashMap<String, Style>,
    /// List id to the marker of each level; `None` for levels without marker.
    numbering: HashMap<String, HashMap<u8, Option<Mark>>>,
    footnotes: HashMap<String, Elem>,
    endnotes: HashMap<String, Elem>,
}

/// An open complex field: its instruction, then where its cached result starts.
struct Field {
    instruction: String,
    result_start: Option<usize>,
}

struct DocxWalk {
    collected: Collected,
    /// Fields open in the current paragraph.
    fields: Vec<Field>,
}

fn val<'e>(parent: &'e Elem, child: &str) -> Option<&'e str> {
    parent.child(child)?.attr("w:val")
}

/// `a/b/` + `../c.xml` gives `a/c.xml`; a leading `/` restarts from the archive root.
fn resolve_target(base_dir: &str, target: &str) -> String {
    let mut segments: Vec<&str> = match target.strip_prefix('/') {
        Some(_) => Vec::new(),
        None => base_dir.split('/').filter(|s| !s.is_empty()).collect(),
    };
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    segments.join("/")
}

fn part_dir(part: &str) -> &str {
    part.rfind('/').map_or("", |slash| &part[..=slash])
}

fn relationships<R: Read + Seek>(
    package: &mut Package<'_, R>,
    part: &str,
) -> Result<Vec<Relationship>, ExtractError> {
    let dir = part_dir(part);
    let rels_name = format!("{dir}_rels/{}.rels", &part[dir.len()..]);
    let Some(root) = package.xml(&rels_name)? else {
        return Ok(Vec::new());
    };
    Ok(root
        .elems()
        .filter(|rel| rel.name == "rel:Relationship")
        .filter_map(|rel| {
            let external = rel.attr("TargetMode") == Some("External");
            let target = rel.attr("Target")?;
            Some(Relationship {
                id: rel.attr("Id")?.to_owned(),
                kind: rel.attr("Type")?.rsplit('/').next()?.to_owned(),
                target: if external { target.to_owned() } else { resolve_target(dir, target) },
                external,
            })
        })
        .collect())
}

/// Heading level announced by a style name or identifier: `heading 2`, `Titre2`, `Title`.
fn heading_of_name(name: &str) -> Option<u8> {
    let name: String = name.to_lowercase().split_whitespace().collect();
    if name == "title" || name == "titre" {
        return Some(1);
    }
    ["heading", "titre", "überschrift", "berschrift"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .and_then(|level| level.parse::<u8>().ok())
        .filter(|level| (1..=9).contains(level))
}

fn parse_styles(root: &Elem) -> HashMap<String, Style> {
    let mut styles = HashMap::new();
    for style in root.elems().filter(|e| e.name == "w:style") {
        let Some(id) = style.attr("w:styleId") else { continue };
        let ppr = style.child("w:pPr");
        let num = ppr.and_then(|p| p.child("w:numPr"));
        styles.insert(
            id.to_owned(),
            Style {
                name: val(style, "w:name").unwrap_or_default().to_owned(),
                based_on: val(style, "w:basedOn").map(str::to_owned),
                outline: ppr.and_then(|p| val(p, "w:outlineLvl")).and_then(|v| v.parse().ok()),
                num_id: num.and_then(|n| val(n, "w:numId")).map(str::to_owned),
                level: num.and_then(|n| val(n, "w:ilvl")).and_then(|v| v.parse().ok()),
            },
        );
    }
    styles
}

fn parse_numbering(root: &Elem) -> HashMap<String, HashMap<u8, Option<Mark>>> {
    fn levels(parent: &Elem, into: &mut HashMap<u8, Option<Mark>>) {
        for level in parent.elems().filter(|e| e.name == "w:lvl") {
            let Some(index) = level.attr("w:ilvl").and_then(|v| v.parse().ok()) else { continue };
            let mark = match val(level, "w:numFmt") {
                Some("bullet") => Some(Mark::Bullet),
                Some("none") => None,
                _ => Some(Mark::Number),
            };
            into.insert(index, mark);
        }
    }
    let mut abstracts: HashMap<&str, HashMap<u8, Option<Mark>>> = HashMap::new();
    for definition in root.elems().filter(|e| e.name == "w:abstractNum") {
        let Some(id) = definition.attr("w:abstractNumId") else { continue };
        levels(definition, abstracts.entry(id).or_default());
    }
    let mut numbering = HashMap::new();
    for num in root.elems().filter(|e| e.name == "w:num") {
        let Some(id) = num.attr("w:numId") else { continue };
        let mut marks = val(num, "w:abstractNumId")
            .and_then(|id| abstracts.get(id))
            .cloned()
            .unwrap_or_default();
        for over in num.elems().filter(|e| e.name == "w:lvlOverride") {
            levels(over, &mut marks);
        }
        numbering.insert(id.to_owned(), marks);
    }
    numbering
}

/// Notes by id, without the separator pseudo-notes.
fn parse_notes(root: Option<Elem>, name: &str) -> HashMap<String, Elem> {
    let mut notes = HashMap::new();
    for kid in root.map(|root| root.kids).unwrap_or_default() {
        let Node::Elem(note) = kid else { continue };
        let real = matches!(note.attr("w:type"), None | Some("normal"));
        if note.name == name && real && let Some(id) = note.attr("w:id") {
            notes.insert(id.to_owned(), note);
        }
    }
    notes
}

/// The branch of an `mc:AlternateContent` to read: both hold the same content, for old and new
/// readers.
fn alternate(elem: &Elem) -> Option<&Elem> {
    elem.child("mc:Choice").or_else(|| elem.child("mc:Fallback"))
}

impl Docx<'_> {
    fn is_final(&self) -> bool {
        self.options.revision == Revision::Final
    }

    fn kind(&self, ppr: Option<&Elem>) -> Kind {
        let direct_num = ppr.and_then(|p| p.child("w:numPr"));
        let mut outline: Option<u8> =
            ppr.and_then(|p| val(p, "w:outlineLvl")).and_then(|v| v.parse().ok());
        let mut num_id = direct_num.and_then(|n| val(n, "w:numId"));
        let mut level: Option<u8> =
            direct_num.and_then(|n| val(n, "w:ilvl")).and_then(|v| v.parse().ok());
        let mut named = None;

        // Paragraph properties are inherited along the style chain.
        let mut style_id = ppr.and_then(|p| val(p, "w:pStyle"));
        for _ in 0..16 {
            let Some(id) = style_id else { break };
            let Some(style) = self.styles.get(id) else {
                named = named.or_else(|| heading_of_name(id));
                break;
            };
            outline = outline.or(style.outline);
            named = named.or_else(|| heading_of_name(&style.name));
            num_id = num_id.or(style.num_id.as_deref());
            level = level.or(style.level);
            style_id = style.based_on.as_deref();
        }

        // Outline level 9 means body text.
        let heading = match outline {
            Some(outline) if outline < 9 => Some(outline + 1),
            _ => named,
        };
        if let Some(heading) = heading {
            return Kind::Heading(heading.min(6));
        }
        match num_id {
            Some(id) if id != "0" => {
                let level = level.unwrap_or(0);
                let mark = self.numbering.get(id).and_then(|marks| marks.get(&level)).copied();
                match mark.unwrap_or(Some(Mark::Bullet)) {
                    Some(mark) => Kind::Item { level: level.min(8), mark },
                    None => Kind::Plain,
                }
            }
            _ => Kind::Plain,
        }
    }

    fn blocks(&self, walk: &mut DocxWalk, parent: &Elem) -> Vec<Block> {
        let mut out = Vec::new();
        self.blocks_into(walk, parent, &mut out);
        merge_joined(out)
    }

    fn blocks_into(&self, walk: &mut DocxWalk, parent: &Elem, out: &mut Vec<Block>) {
        for elem in parent.elems() {
            match elem.name.as_str() {
                "w:p" => self.paragraph(walk, elem, out),
                "w:tbl" => {
                    walk.collected.facts.tables += 1;
                    let mut rows = Vec::new();
                    self.rows(walk, elem, &mut rows);
                    out.push(Block::Table(rows));
                }
                "w:ins" | "w:moveTo" if self.is_final() => self.blocks_into(walk, elem, out),
                "w:del" | "w:moveFrom" if !self.is_final() => self.blocks_into(walk, elem, out),
                "mc:AlternateContent" => {
                    if let Some(branch) = alternate(elem) {
                        self.blocks_into(walk, branch, out);
                    }
                }
                "w:sdt" | "w:sdtContent" | "w:customXml" | "w:smartTag" => {
                    self.blocks_into(walk, elem, out)
                }
                _ => {}
            }
        }
    }

    fn rows(&self, walk: &mut DocxWalk, parent: &Elem, rows: &mut Vec<Vec<Vec<Block>>>) {
        for elem in parent.elems() {
            match elem.name.as_str() {
                "w:tr" => {
                    let properties = elem.child("w:trPr");
                    let inserted = properties.is_some_and(|p| p.child("w:ins").is_some());
                    let deleted = properties.is_some_and(|p| p.child("w:del").is_some());
                    walk.collected.facts.tracked_insertions += usize::from(inserted);
                    walk.collected.facts.tracked_deletions += usize::from(deleted);
                    if (self.is_final() && deleted) || (!self.is_final() && inserted) {
                        continue;
                    }
                    let mut cells = Vec::new();
                    self.cells(walk, elem, &mut cells);
                    rows.push(cells);
                }
                "w:sdt" | "w:sdtContent" | "w:customXml" => self.rows(walk, elem, rows),
                _ => {}
            }
        }
    }

    fn cells(&self, walk: &mut DocxWalk, parent: &Elem, cells: &mut Vec<Vec<Block>>) {
        for elem in parent.elems() {
            match elem.name.as_str() {
                "w:tc" => cells.push(self.blocks(walk, elem)),
                "w:sdt" | "w:sdtContent" | "w:customXml" => self.cells(walk, elem, cells),
                _ => {}
            }
        }
    }

    fn paragraph(&self, walk: &mut DocxWalk, paragraph: &Elem, out: &mut Vec<Block>) {
        let properties = paragraph.child("w:pPr");
        // A tracked change on the paragraph mark itself: the paragraph was split or joined.
        let mark = properties.and_then(|p| p.child("w:rPr"));
        let has = |a: &str, b: &str| {
            mark.is_some_and(|m| m.child(a).is_some() || m.child(b).is_some())
        };
        let inserted = has("w:ins", "w:moveTo");
        let deleted = has("w:del", "w:moveFrom");
        walk.collected.facts.tracked_insertions += usize::from(inserted);
        walk.collected.facts.tracked_deletions += usize::from(deleted);

        let mut text = String::new();
        let mut boxes = Vec::new();
        let outer_fields = std::mem::take(&mut walk.fields);
        self.inline(walk, paragraph, &mut text, &mut boxes);
        walk.fields = outer_fields;
        out.push(Block::Para {
            kind: self.kind(properties),
            text,
            joins_next: if self.is_final() { deleted } else { inserted },
        });
        out.append(&mut boxes);
    }

    /// Paragraph content. `boxes` receives the blocks of text boxes anchored there.
    fn inline(&self, walk: &mut DocxWalk, parent: &Elem, buf: &mut String, boxes: &mut Vec<Block>) {
        for elem in parent.elems() {
            match elem.name.as_str() {
                "w:r" | "m:r" => self.run(walk, elem, buf, boxes),
                "w:hyperlink" => {
                    let start = buf.len();
                    self.inline(walk, elem, buf, boxes);
                    if let Some(url) = elem.attr("r:id").and_then(|id| self.links.get(id)) {
                        wrap_link(buf, start, url, self.options);
                    }
                }
                "w:ins" | "w:moveTo" => {
                    walk.collected.facts.tracked_insertions += 1;
                    if self.is_final() {
                        self.inline(walk, elem, buf, boxes);
                    }
                }
                "w:del" | "w:moveFrom" => {
                    walk.collected.facts.tracked_deletions += 1;
                    if !self.is_final() {
                        self.inline(walk, elem, buf, boxes);
                    }
                }
                "mc:AlternateContent" => {
                    if let Some(branch) = alternate(elem) {
                        self.inline(walk, branch, buf, boxes);
                    }
                }
                "m:oMath" => {
                    walk.collected.facts.equations += 1;
                    self.inline(walk, elem, buf, boxes);
                }
                // Properties hold no text, and their change marks are not content changes.
                name if name.ends_with("Pr") => {}
                // Content controls, smart tags, simple fields (cached result), math structures.
                _ => self.inline(walk, elem, buf, boxes),
            }
        }
    }

    fn run(&self, walk: &mut DocxWalk, run: &Elem, buf: &mut String, boxes: &mut Vec<Block>) {
        for elem in run.elems() {
            match elem.name.as_str() {
                "w:t" | "m:t" | "w:delText" => elem.push_text(buf),
                "w:tab" | "w:ptab" => buf.push('\t'),
                // Page and column breaks are layout, not text.
                "w:br" | "w:cr" => {
                    if !matches!(elem.attr("w:type"), Some("page" | "column")) {
                        buf.push(BREAK);
                    }
                }
                "w:noBreakHyphen" => buf.push('\u{2011}'),
                "w:softHyphen" => buf.push('\u{ad}'),
                "w:sym" => {
                    let symbol = elem
                        .attr("w:char")
                        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                        .and_then(char::from_u32);
                    // Symbol fonts live in the private use area: meaningless without the font.
                    if let Some(c) = symbol && !('\u{e000}'..='\u{f8ff}').contains(&c) {
                        buf.push(c);
                    }
                }
                "w:footnoteReference" => self.note(walk, &self.footnotes, elem, buf),
                "w:endnoteReference" => self.note(walk, &self.endnotes, elem, buf),
                "w:fldChar" => self.field_mark(walk, elem, buf),
                "w:instrText" => {
                    if let Some(field) = walk.fields.last_mut() && field.result_start.is_none() {
                        elem.push_text(&mut field.instruction);
                    }
                }
                "w:drawing" | "w:pict" => {
                    let alt = elem
                        .find("wp:docPr")
                        .and_then(|p| p.attr("descr").filter(|d| !d.is_empty()).or(p.attr("title")))
                        .or_else(|| elem.find("v:shape").and_then(|shape| shape.attr("alt")));
                    self.graphic(walk, elem, alt, buf, boxes);
                }
                "w:object" => walk.collected.object(self.options, buf),
                "w:ruby" => {
                    if let Some(base) = elem.child("w:rubyBase") {
                        self.inline(walk, base, buf, boxes);
                    }
                }
                "mc:AlternateContent" => {
                    if let Some(branch) = alternate(elem) {
                        self.run(walk, branch, buf, boxes);
                    }
                }
                _ => {}
            }
        }
    }

    /// Complex fields: the instruction is skipped, the cached result is ordinary text. A
    /// `HYPERLINK` instruction turns the result into a link.
    fn field_mark(&self, walk: &mut DocxWalk, mark: &Elem, buf: &mut String) {
        match mark.attr("w:fldCharType") {
            Some("begin") => walk.fields.push(Field {
                instruction: String::new(),
                result_start: None,
            }),
            Some("separate") => {
                if let Some(field) = walk.fields.last_mut() {
                    field.result_start = Some(buf.len());
                }
            }
            Some("end") => {
                let Some(field) = walk.fields.pop() else { return };
                let Some(start) = field.result_start.filter(|start| *start <= buf.len()) else {
                    return;
                };
                let instruction = field.instruction.trim();
                let Some(arguments) = instruction.strip_prefix("HYPERLINK") else { return };
                // `\l` points inside the document: not an address.
                let arguments = arguments.trim_start();
                if let Some(quoted) = arguments.strip_prefix('"')
                    && let Some(end) = quoted.find('"')
                {
                    wrap_link(buf, start, &quoted[..end], self.options);
                }
            }
            _ => {}
        }
    }

    fn note(&self, walk: &mut DocxWalk, notes: &HashMap<String, Elem>, reference: &Elem, buf: &mut String) {
        let Some(body) = reference.attr("w:id").and_then(|id| notes.get(id)) else { return };
        if walk.collected.in_note || self.options.notes == NotePlacement::Omit {
            return;
        }
        walk.collected.in_note = true;
        let text = flatten(&self.blocks(walk, body));
        walk.collected.in_note = false;
        walk.collected.note(&text, self.options, buf);
    }

    fn graphic(
        &self,
        walk: &mut DocxWalk,
        parent: &Elem,
        alt: Option<&str>,
        buf: &mut String,
        boxes: &mut Vec<Block>,
    ) {
        for elem in parent.elems() {
            match elem.name.as_str() {
                "w:txbxContent" => {
                    walk.collected.facts.text_boxes += 1;
                    let outer_fields = std::mem::take(&mut walk.fields);
                    boxes.extend(self.blocks(walk, elem));
                    walk.fields = outer_fields;
                }
                "pic:pic" | "v:imagedata" => walk.collected.image(alt, self.options, buf),
                "c:chart" | "dgm:relIds" | "o:OLEObject" => walk.collected.object(self.options, buf),
                "mc:AlternateContent" => {
                    if let Some(branch) = alternate(elem) {
                        self.graphic(walk, branch, alt, buf, boxes);
                    }
                }
                _ => self.graphic(walk, elem, alt, buf, boxes),
            }
        }
    }
}

fn extract_docx<R: Read + Seek>(
    package: &mut Package<'_, R>,
    options: &ExtractOptions,
) -> Result<Extracted, ExtractError> {
    let main = relationships(package, "")?
        .into_iter()
        .find(|rel| rel.kind == "officeDocument" && !rel.external)
        .map_or_else(|| "word/document.xml".to_owned(), |rel| rel.target);
    let root = package.xml(&main)?.ok_or_else(|| package.missing(&main))?;
    // Workbooks and presentations are packages too.
    if root.name != "w:document" {
        return Err(ExtractError::Unsupported(package.path.to_owned()));
    }
    let rels = relationships(package, &main)?;
    let dir = part_dir(&main).to_owned();
    // Without relationships, the conventional names still work.
    let part_of = |kind: &str| {
        rels.iter()
            .find(|rel| rel.kind == kind && !rel.external)
            .map_or_else(|| format!("{dir}{kind}.xml"), |rel| rel.target.clone())
    };

    let styles = package.xml(&part_of("styles"))?;
    let numbering = package.xml(&part_of("numbering"))?;
    let footnotes = package.xml(&part_of("footnotes"))?;
    let endnotes = package.xml(&part_of("endnotes"))?;
    let docx = Docx {
        options,
        links: rels
            .iter()
            .filter(|rel| rel.kind == "hyperlink" && rel.external)
            .map(|rel| (rel.id.clone(), rel.target.clone()))
            .collect(),
        styles: styles.as_ref().map(parse_styles).unwrap_or_default(),
        numbering: numbering.as_ref().map(parse_numbering).unwrap_or_default(),
        footnotes: parse_notes(footnotes, "w:footnote"),
        endnotes: parse_notes(endnotes, "w:endnote"),
    };
    let mut walk = DocxWalk {
        collected: Collected::default(),
        fields: Vec::new(),
    };
    walk.collected.facts.footnotes = docx.footnotes.len();
    walk.collected.facts.endnotes = docx.endnotes.len();
    walk.collected.facts.has_macros =
        rels.iter().any(|rel| rel.kind == "vbaProject") || package.has("word/vbaProject.bin");

    let body = match root.child("w:body") {
        Some(body) => docx.blocks(&mut walk, body),
        None => Vec::new(),
    };

    if let Some(comments) = package.xml(&part_of("comments"))? {
        for comment in comments.elems().filter(|e| e.name == "w:comment") {
            walk.collected.facts.comments += 1;
            if options.comments {
                let text = flatten(&docx.blocks(&mut walk, comment));
                walk.collected.comment(comment.attr("w:author").unwrap_or_default(), &text);
            }
        }
    }
    for rel in rels.iter().filter(|rel| !rel.external) {
        let label = match rel.kind.as_str() {
            "header" => "en-tête",
            "footer" => "pied de page",
            _ => continue,
        };
        if let Some(part) = package.xml(&rel.target)? {
            let blocks = docx.blocks(&mut walk, &part);
            walk.collected.furniture(label, &blocks, options);
        }
    }
    Ok(walk.collected.finish(DocFormat::Docx, &body, options))
}

// ---------------------------------------------------------------------------------------------
// .odt
// ---------------------------------------------------------------------------------------------

enum Change<'a> {
    Insertion,
    /// The removed content, kept by the file as paragraphs.
    Deletion(&'a Elem),
    Format,
}

struct Odt<'a> {
    options: &'a ExtractOptions,
    /// List style name to the marker of each level, from level 1.
    lists: HashMap<&'a str, Vec<Mark>>,
    changes: HashMap<&'a str, Change<'a>>,
}

#[derive(Default)]
struct OdtWalk {
    collected: Collected,
    /// Inside a tracked insertion while extracting the original text.
    skipping: bool,
    in_deletion: bool,
}

/// Position in nested lists: depth from 1, and the list style in force.
#[derive(Clone, Copy)]
struct ListContext<'a> {
    level: u8,
    style: Option<&'a str>,
}

fn collect_list_styles<'a>(root: &'a Elem, lists: &mut HashMap<&'a str, Vec<Mark>>) {
    let sections = root
        .elems()
        .filter(|e| matches!(e.name.as_str(), "office:automatic-styles" | "office:styles"));
    for style in sections.flat_map(Elem::elems).filter(|e| e.name == "text:list-style") {
        let Some(name) = style.attr("style:name") else { continue };
        let mut marks = Vec::new();
        for level in style.elems() {
            let mark = match level.name.as_str() {
                // An empty number format is how outlines without numbers are stored.
                "text:list-level-style-number" => match level.attr("style:num-format") {
                    Some("") => Mark::Continuation,
                    _ => Mark::Number,
                },
                "text:list-level-style-bullet" | "text:list-level-style-image" => Mark::Bullet,
                _ => continue,
            };
            let index: usize = level.attr("text:level").and_then(|v| v.parse().ok()).unwrap_or(1);
            if (1..=10).contains(&index) {
                if marks.len() < index {
                    marks.resize(index, Mark::Bullet);
                }
                marks[index - 1] = mark;
            }
        }
        lists.entry(name).or_insert(marks);
    }
}

/// OpenDocument text is whitespace-collapsed: line feeds and indentation of the XML are not
/// content, real repeated spaces are `text:s` elements.
fn push_collapsed(buf: &mut String, text: &str) {
    for c in text.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            if !buf.is_empty() && !buf.ends_with([' ', BREAK, SPLIT]) {
                buf.push(' ');
            }
        } else {
            buf.push(c);
        }
    }
}

impl<'a> Odt<'a> {
    fn is_final(&self) -> bool {
        self.options.revision == Revision::Final
    }

    fn blocks(&self, walk: &mut OdtWalk, parent: &'a Elem, list: Option<ListContext<'a>>) -> Vec<Block> {
        let mut out = Vec::new();
        self.blocks_into(walk, parent, list, &mut out);
        merge_joined(out)
    }

    fn blocks_into(
        &self,
        walk: &mut OdtWalk,
        parent: &'a Elem,
        list: Option<ListContext<'a>>,
        out: &mut Vec<Block>,
    ) {
        let item = |context: ListContext| Kind::Item {
            level: (context.level - 1).min(8),
            mark: Mark::Continuation,
        };
        for elem in parent.elems() {
            match elem.name.as_str() {
                "text:p" => {
                    let kind = match list {
                        Some(context) => item(context),
                        None if elem.attr("text:style-name") == Some("Title") => Kind::Heading(1),
                        None => Kind::Plain,
                    };
                    self.paragraph(walk, elem, kind, out);
                }
                "text:h" => {
                    let level: u8 = elem
                        .attr("text:outline-level")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1);
                    self.paragraph(walk, elem, Kind::Heading(level.clamp(1, 6)), out);
                }
                "text:list" => {
                    let context = ListContext {
                        level: list.map_or(1, |outer| outer.level.saturating_add(1)),
                        style: elem.attr("text:style-name").or(list.and_then(|outer| outer.style)),
                    };
                    let mark = context
                        .style
                        .and_then(|style| self.lists.get(style))
                        .and_then(|marks| marks.get(usize::from(context.level) - 1))
                        .copied()
                        .unwrap_or(Mark::Bullet);
                    for entry in elem.elems() {
                        let header = entry.name == "text:list-header";
                        if entry.name != "text:list-item" && !header {
                            continue;
                        }
                        let first = out.len();
                        self.blocks_into(walk, entry, Some(context), out);
                        // The marker goes to the item's own first paragraph; an item made of
                        // a nested list only has none.
                        if !header
                            && let Some(Block::Para { kind, .. }) = out.get_mut(first)
                            && *kind == item(context)
                        {
                            *kind = Kind::Item { level: (context.level - 1).min(8), mark };
                        }
                    }
                }
                "table:table" => {
                    walk.collected.facts.tables += 1;
                    let mut rows = Vec::new();
                    self.rows(walk, elem, &mut rows);
                    out.push(Block::Table(rows));
                }
                "text:section" | "text:index-body" | "text:index-title" | "text:numbered-paragraph" => {
                    self.blocks_into(walk, elem, list, out)
                }
                // Tables of contents and indexes: the generated body, not its template.
                "text:table-of-content" | "text:illustration-index" | "text:table-index"
                | "text:object-index" | "text:user-index" | "text:alphabetical-index"
                | "text:bibliography" => {
                    if let Some(body) = elem.child("text:index-body") {
                        self.blocks_into(walk, body, list, out);
                    }
                }
                // Frames and shapes anchored to the page rather than to a paragraph.
                name if name.starts_with("draw:") => {
                    let mut text = String::new();
                    let mut boxes = Vec::new();
                    self.drawing(walk, elem, &mut text, &mut boxes);
                    out.push(plain(text));
                    out.append(&mut boxes);
                }
                _ => {}
            }
        }
    }

    fn rows(&self, walk: &mut OdtWalk, parent: &'a Elem, rows: &mut Vec<Vec<Vec<Block>>>) {
        for elem in parent.elems() {
            match elem.name.as_str() {
                "table:table-row" => rows.push(
                    elem.elems()
                        .filter_map(|cell| match cell.name.as_str() {
                            "table:table-cell" => Some(self.blocks(walk, cell, None)),
                            // Hidden under a merged cell: kept empty so columns stay aligned.
                            "table:covered-table-cell" => Some(Vec::new()),
                            _ => None,
                        })
                        .collect(),
                ),
                "table:table-header-rows" | "table:table-rows" | "table:table-row-group" => {
                    self.rows(walk, elem, rows)
                }
                _ => {}
            }
        }
    }

    fn paragraph(&self, walk: &mut OdtWalk, paragraph: &'a Elem, kind: Kind, out: &mut Vec<Block>) {
        let mut text = String::new();
        let mut boxes = Vec::new();
        self.inline(walk, paragraph, &mut text, &mut boxes);
        out.push(Block::Para {
            kind,
            text,
            // An insertion still open at the end of the paragraph inserted the paragraph break.
            joins_next: walk.skipping,
        });
        out.append(&mut boxes);
    }

    fn inline(&self, walk: &mut OdtWalk, parent: &'a Elem, buf: &mut String, boxes: &mut Vec<Block>) {
        for kid in &parent.kids {
            let elem = match kid {
                Node::Text(text) => {
                    if !walk.skipping {
                        push_collapsed(buf, text);
                    }
                    continue;
                }
                Node::Elem(elem) => elem,
            };
            match elem.name.as_str() {
                "text:s" if !walk.skipping => {
                    let count: usize = elem.attr("text:c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    buf.extend(std::iter::repeat_n(' ', count.min(1000)));
                }
                "text:tab" if !walk.skipping => buf.push('\t'),
                "text:line-break" if !walk.skipping => buf.push(BREAK),
                "text:s" | "text:tab" | "text:line-break" => {}
                "text:a" => {
                    let start = buf.len();
                    self.inline(walk, elem, buf, boxes);
                    // `#name` points inside the document: not an address.
                    if let Some(url) = elem.attr("xlink:href").filter(|url| !url.starts_with('#')) {
                        wrap_link(buf, start, url, self.options);
                    }
                }
                "text:note" => self.note(walk, elem, buf),
                "office:annotation" => {
                    walk.collected.facts.comments += 1;
                    if self.options.comments && !walk.skipping {
                        let author = elem.child("dc:creator").map(Elem::text).unwrap_or_default();
                        let text = flatten(&self.blocks(walk, elem, None));
                        walk.collected.comment(&author, &text);
                    }
                }
                "text:change-start" | "text:change-end" => {
                    let insertion = matches!(self.change(elem), Some(Change::Insertion));
                    if insertion && !self.is_final() {
                        walk.skipping = elem.name == "text:change-start";
                    }
                }
                "text:change" => {
                    if let Some(Change::Deletion(removed)) = self.change(elem)
                        && !self.is_final()
                        && !walk.skipping
                        && !walk.in_deletion
                    {
                        walk.in_deletion = true;
                        self.deleted(walk, removed, buf, boxes);
                        walk.in_deletion = false;
                    }
                }
                name if name.starts_with("draw:") => self.drawing(walk, elem, buf, boxes),
                // Ruby annotations, note numbers and descriptions are not the running text.
                "text:note-citation" | "text:ruby-text" | "svg:title" | "svg:desc"
                | "text:tracked-changes" | "office:event-listeners" | "office:annotation-end" => {}
                // Spans, fields (their cached text), bookmarks, ruby bases.
                _ => self.inline(walk, elem, buf, boxes),
            }
        }
    }

    fn change(&self, mark: &Elem) -> Option<&Change<'a>> {
        self.changes.get(mark.attr("text:change-id")?)
    }

    /// Puts removed content back where its mark is. Its first paragraph continues the current
    /// one; each further paragraph is a paragraph boundary that was removed with it.
    fn deleted(&self, walk: &mut OdtWalk, removed: &'a Elem, buf: &mut String, boxes: &mut Vec<Block>) {
        let mut first = true;
        for elem in removed.elems() {
            let block = !matches!(elem.name.as_str(), "text:p" | "text:h");
            if block && !matches!(elem.name.as_str(), "text:list" | "table:table" | "text:section") {
                continue;
            }
            if !first {
                buf.push(SPLIT);
            }
            first = false;
            if block {
                // Removed lists and tables have no line of their own to go back to.
                let mut out = Vec::new();
                if elem.name == "table:table" {
                    let mut rows = Vec::new();
                    self.rows(walk, elem, &mut rows);
                    out.push(Block::Table(rows));
                } else {
                    self.blocks_into(walk, elem, None, &mut out);
                }
                buf.push_str(&flatten(&out));
            } else {
                self.inline(walk, elem, buf, boxes);
            }
        }
    }

    fn note(&self, walk: &mut OdtWalk, note: &'a Elem, buf: &mut String) {
        if walk.skipping {
            return;
        }
        match note.attr("text:note-class") {
            Some("endnote") => walk.collected.facts.endnotes += 1,
            _ => walk.collected.facts.footnotes += 1,
        }
        let Some(body) = note.child("text:note-body") else { return };
        if walk.collected.in_note || self.options.notes == NotePlacement::Omit {
            return;
        }
        walk.collected.in_note = true;
        let text = flatten(&self.blocks(walk, body, None));
        walk.collected.in_note = false;
        walk.collected.note(&text, self.options, buf);
    }

    /// Frames (pictures, text boxes, embedded objects) and drawing shapes carrying text.
    fn drawing(&self, walk: &mut OdtWalk, elem: &'a Elem, buf: &mut String, boxes: &mut Vec<Block>) {
        if walk.skipping {
            return;
        }
        if elem.name != "draw:frame" {
            let own = self.blocks(walk, elem, None);
            if !flatten(&own).is_empty() {
                walk.collected.facts.text_boxes += 1;
                boxes.extend(own);
            }
            // Groups hold further shapes.
            for inner in elem.elems().filter(|e| e.name.starts_with("draw:")) {
                self.drawing(walk, inner, buf, boxes);
            }
            return;
        }
        let is_object = |e: &&Elem| {
            matches!(e.name.as_str(), "draw:object" | "draw:object-ole" | "draw:plugin" | "draw:applet")
        };
        for text_box in elem.elems().filter(|e| e.name == "draw:text-box") {
            walk.collected.facts.text_boxes += 1;
            boxes.extend(self.blocks(walk, text_box, None));
        }
        // An embedded object comes with a picture of itself: one object, not one image more.
        if elem.elems().any(|e| is_object(&e)) {
            walk.collected.object(self.options, buf);
        } else if elem.child("draw:image").is_some() {
            let alt = elem
                .child("svg:title")
                .or_else(|| elem.child("svg:desc"))
                .map(Elem::text);
            walk.collected.image(alt.as_deref(), self.options, buf);
        }
    }
}

/// `content` holds the body, `styles` the named list styles and the headers and footers; they
/// are the same element for a flat document. `None`: not a text document.
fn extract_odt(content: &Elem, styles: Option<&Elem>, options: &ExtractOptions) -> Option<Extracted> {
    let body = content.child("office:body")?.child("office:text")?;
    let mut odt = Odt {
        options,
        lists: HashMap::new(),
        changes: HashMap::new(),
    };
    collect_list_styles(content, &mut odt.lists);
    if let Some(styles) = styles {
        collect_list_styles(styles, &mut odt.lists);
    }
    let mut walk = OdtWalk::default();
    let regions = body.child("text:tracked-changes").into_iter().flat_map(Elem::elems);
    for region in regions.filter(|e| e.name == "text:changed-region") {
        let Some(id) = region.attr("text:id").or_else(|| region.attr("xml:id")) else { continue };
        let change = match region.elems().next() {
            Some(change) if change.name == "text:insertion" => {
                walk.collected.facts.tracked_insertions += 1;
                Change::Insertion
            }
            Some(change) if change.name == "text:deletion" => {
                walk.collected.facts.tracked_deletions += 1;
                Change::Deletion(change)
            }
            _ => Change::Format,
        };
        odt.changes.insert(id, change);
    }

    let blocks = odt.blocks(&mut walk, body, None);
    walk.skipping = false;

    let pages = styles
        .and_then(|styles| styles.child("office:master-styles"))
        .into_iter()
        .flat_map(Elem::elems)
        .filter(|e| e.name == "style:master-page");
    for furniture in pages.flat_map(Elem::elems) {
        // Also the `-left` and `-first` variants.
        let label = if furniture.name.starts_with("style:header") {
            "en-tête"
        } else if furniture.name.starts_with("style:footer") {
            "pied de page"
        } else {
            continue;
        };
        let blocks = odt.blocks(&mut walk, furniture, None);
        walk.collected.furniture(label, &blocks, options);
    }
    Some(walk.collected.finish(DocFormat::Odt, &blocks, options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml""#;
    const ODF: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:dc="http://purl.org/dc/elements/1.1/""#;
    const ODT_MIME: &str = "application/vnd.oasis.opendocument.text";

    fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in parts {
            writer.start_file(*name, SimpleFileOptions::default()).unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn document_xml(body: &str) -> String {
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W}><w:body>{body}<w:sectPr/></w:body></w:document>"#)
    }

    /// A .docx with the given body and extra parts (`word/…` names).
    fn docx_with(body: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let document = document_xml(body);
        let mut parts = vec![
            ("[Content_Types].xml", r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#,
            ),
            ("word/document.xml", document.as_str()),
        ];
        parts.extend_from_slice(extra);
        zip_of(&parts)
    }

    fn docx(body: &str) -> Vec<u8> {
        docx_with(body, &[])
    }

    fn content_xml(body: &str, styles: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {ODF}><office:automatic-styles>{styles}</office:automatic-styles><office:body><office:text>{body}</office:text></office:body></office:document-content>"#
        )
    }

    fn odt_with(body: &str, styles: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let content = content_xml(body, styles);
        let mut parts = vec![("mimetype", ODT_MIME), ("content.xml", content.as_str())];
        parts.extend_from_slice(extra);
        zip_of(&parts)
    }

    fn odt(body: &str) -> Vec<u8> {
        odt_with(body, "", &[])
    }

    fn run(bytes: &[u8], options: &ExtractOptions) -> Extracted {
        extract_bytes(bytes, Path::new("test"), options).unwrap()
    }

    fn text_of(bytes: &[u8]) -> String {
        run(bytes, &ExtractOptions::default()).text
    }

    fn original() -> ExtractOptions {
        ExtractOptions {
            revision: Revision::Original,
            ..ExtractOptions::default()
        }
    }

    fn fail(bytes: &[u8]) -> ExtractError {
        extract_bytes(bytes, Path::new("test"), &ExtractOptions::default()).unwrap_err()
    }

    fn p(text: &str) -> String {
        format!(r#"<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#)
    }

    // --- .docx --------------------------------------------------------------------------------

    #[test]
    fn each_docx_paragraph_is_one_line_however_long() {
        let long = "mot ".repeat(400);
        let text = text_of(&docx(&format!("{}{}", p(&long), p("Fin."))));
        assert_eq!(text, format!("{}\nFin.\n", long.trim_end()));
    }

    #[test]
    fn runs_split_in_the_middle_of_a_word_are_joined() {
        let body = r#"<w:p><w:r><w:t>Bon</w:t></w:r><w:proofErr w:type="spellStart"/><w:r><w:rPr><w:b/></w:rPr><w:t>jour</w:t></w:r><w:r><w:t xml:space="preserve"> le monde</w:t></w:r></w:p>"#;
        assert_eq!(text_of(&docx(body)), "Bonjour le monde\n");
    }

    #[test]
    fn entities_and_character_references_are_decoded() {
        let body = p("Dupont &amp; fils &lt;sarl&gt; caf&#233; &#x2014; fin");
        assert_eq!(text_of(&docx(&body)), "Dupont & fils <sarl> café — fin\n");
    }

    #[test]
    fn tabs_are_kept_and_line_breaks_start_a_new_line_but_page_breaks_vanish() {
        let body = r#"<w:p><w:r><w:t>Nom</w:t><w:tab/><w:t>Valeur</w:t><w:br/><w:t>Suite</w:t><w:br w:type="page"/><w:t>!</w:t></w:r></w:p>"#;
        assert_eq!(text_of(&docx(body)), "Nom\tValeur\nSuite!\n");
    }

    #[test]
    fn empty_paragraphs_are_dropped_unless_asked_for() {
        let body = format!("{}<w:p/><w:p><w:r><w:t xml:space=\"preserve\">  </w:t></w:r></w:p>{}", p("a"), p("b"));
        assert_eq!(text_of(&docx(&body)), "a\nb\n");
        let keep = ExtractOptions {
            keep_empty_paragraphs: true,
            ..ExtractOptions::default()
        };
        assert_eq!(run(&docx(&body), &keep).text, "a\n\n\nb\n");
    }

    const STYLES: &str = r#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
        <w:style w:type="paragraph" w:styleId="Titre1"><w:name w:val="heading 1"/><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style>
        <w:style w:type="paragraph" w:styleId="Titre2"><w:name w:val="heading 2"/></w:style>
        <w:style w:type="paragraph" w:styleId="Chapitre"><w:name w:val="Chapitre"/><w:basedOn w:val="Titre1"/></w:style>
        <w:style w:type="paragraph" w:styleId="Puce"><w:name w:val="List Bullet"/><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>
    </w:styles>"#;

    fn styled(style: &str, text: &str) -> String {
        format!(r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#)
    }

    #[test]
    fn headings_come_from_the_outline_level_the_style_name_or_the_parent_style() {
        let body = format!(
            "{}{}{}{}{}",
            styled("Titre1", "Un"),
            p("Texte."),
            styled("Titre2", "Deux"),
            styled("Chapitre", "Hérité"),
            r#"<w:p><w:pPr><w:outlineLvl w:val="2"/></w:pPr><w:r><w:t>Direct</w:t></w:r></w:p>"#,
        );
        let text = text_of(&docx_with(&body, &[("word/styles.xml", STYLES)]));
        assert_eq!(text, "# Un\nTexte.\n\n## Deux\n\n# Hérité\n\n### Direct\n");
    }

    #[test]
    fn heading_style_identifiers_work_without_a_styles_part() {
        let body = format!("{}{}{}", styled("Heading2", "Anglais"), styled("Titre3", "Français"), styled("Title", "Titre"));
        assert_eq!(text_of(&docx(&body)), "## Anglais\n\n### Français\n\n# Titre\n");
    }

    const NUMBERING: &str = r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
        <w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum>
        <w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/></w:lvl></w:abstractNum>
        <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
        <w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>
    </w:numbering>"#;

    fn item(num: u8, level: u8, text: &str) -> String {
        format!(
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{num}"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
        )
    }

    #[test]
    fn nested_lists_get_markers_and_indentation_and_ordered_items_are_never_renumbered() {
        let body = format!(
            "{}{}{}{}{}{}",
            item(1, 0, "pomme"),
            item(1, 1, "reinette"),
            item(2, 0, "premier"),
            item(2, 1, "sous-point"),
            item(2, 0, "second"),
            styled("Puce", "par le style"),
        );
        let parts = [("word/numbering.xml", NUMBERING), ("word/styles.xml", STYLES)];
        assert_eq!(
            text_of(&docx_with(&body, &parts)),
            "- pomme\n  - reinette\n1. premier\n  1. sous-point\n1. second\n- par le style\n"
        );
    }

    #[test]
    fn a_line_break_inside_a_list_item_continues_under_its_text() {
        let body = r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="2"/></w:numPr></w:pPr><w:r><w:t>un</w:t><w:br/><w:t>suite</w:t></w:r></w:p>"#;
        assert_eq!(text_of(&docx_with(body, &[("word/numbering.xml", NUMBERING)])), "1. un\n   suite\n");
    }

    fn cell(content: &str) -> String {
        format!("<w:tc><w:tcPr/>{content}</w:tc>")
    }

    #[test]
    fn a_table_row_is_one_line_and_cell_paragraphs_stay_in_their_cell() {
        let nested = format!("<w:tbl><w:tr>{}{}</w:tr></w:tbl>", cell(&p("x")), cell(&p("y")));
        let body = format!(
            "{}<w:tbl><w:tblPr/><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl>{}",
            p("Avant"),
            cell(&p("Nom")),
            cell(&p("Adresse")),
            cell(&p("Ada")),
            cell(&format!("{}{}", p("1 rue A"), p("Paris"))),
            cell("<w:p/>"),
            cell(&format!("{}<w:p/>", nested)),
            p("Après"),
        );
        let extracted = run(&docx(&body), &ExtractOptions::default());
        assert_eq!(
            extracted.text,
            "Avant\n\n| Nom | Adresse |\n|---|---|\n| Ada | 1 rue A ¶ Paris |\n| | x | y |\n\nAprès\n"
        );
        assert_eq!(extracted.facts.tables, 2);
    }

    const FOOTNOTES: &str = r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
        <w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote>
        <w:footnote w:id="2"><w:p><w:r><w:footnoteRef/></w:r><w:r><w:t xml:space="preserve"> Voir le rapport.</w:t></w:r></w:p><w:p><w:r><w:t>Page 3.</w:t></w:r></w:p></w:footnote>
        <w:footnote w:id="3"><w:p><w:r><w:t>Autre note.</w:t></w:r></w:p></w:footnote>
    </w:footnotes>"#;
    const NOTED: &str = r#"<w:p><w:r><w:t>Un fait</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r><w:r><w:t xml:space="preserve"> établi</w:t></w:r><w:r><w:footnoteReference w:id="3"/></w:r><w:r><w:t>.</w:t></w:r></w:p>"#;

    #[test]
    fn footnotes_are_inlined_where_referenced_by_default() {
        let extracted = run(&docx_with(NOTED, &[("word/footnotes.xml", FOOTNOTES)]), &ExtractOptions::default());
        assert_eq!(extracted.text, "Un fait^[Voir le rapport. ¶ Page 3.] établi^[Autre note.].\n");
        assert_eq!(extracted.facts.footnotes, 2);
    }

    #[test]
    fn footnotes_can_be_listed_after_the_body_or_omitted() {
        let bytes = docx_with(NOTED, &[("word/footnotes.xml", FOOTNOTES)]);
        let end = ExtractOptions { notes: NotePlacement::End, ..ExtractOptions::default() };
        assert_eq!(
            run(&bytes, &end).text,
            "Un fait[^1] établi[^2].\n\n[^1]: Voir le rapport. ¶ Page 3.\n[^2]: Autre note.\n"
        );
        let omit = ExtractOptions { notes: NotePlacement::Omit, ..ExtractOptions::default() };
        let omitted = run(&bytes, &omit);
        assert_eq!(omitted.text, "Un fait établi.\n");
        assert_eq!(omitted.facts.notices(&omit), ["2 notes ignorées"]);
    }

    const TRACKED: &str = r#"<w:p><w:r><w:t xml:space="preserve">Le chat </w:t></w:r><w:del w:id="1" w:author="A"><w:r><w:delText>noir</w:delText></w:r></w:del><w:ins w:id="2" w:author="A"><w:r><w:t>blanc</w:t></w:r></w:ins><w:r><w:t xml:space="preserve"> dort.</w:t></w:r></w:p>"#;

    #[test]
    fn tracked_changes_give_the_final_text_by_default_and_the_original_on_request() {
        let bytes = docx(TRACKED);
        let extracted = run(&bytes, &ExtractOptions::default());
        assert_eq!(extracted.text, "Le chat blanc dort.\n");
        assert_eq!((extracted.facts.tracked_insertions, extracted.facts.tracked_deletions), (1, 1));
        assert!(extracted.facts.has_tracked_changes());
        assert_eq!(run(&bytes, &original()).text, "Le chat noir dort.\n");
        assert_eq!(
            extracted.facts.notices(&ExtractOptions::default()),
            ["modifications suivies : texte final affiché"]
        );
    }

    #[test]
    fn a_tracked_paragraph_mark_joins_or_splits_paragraphs() {
        // "Un. Deux." was split in two by inserting a paragraph mark after "Un.".
        let split = r#"<w:p><w:pPr><w:rPr><w:ins w:id="1"/></w:rPr></w:pPr><w:r><w:t xml:space="preserve">Un. </w:t></w:r></w:p><w:p><w:r><w:t>Deux.</w:t></w:r></w:p>"#;
        assert_eq!(text_of(&docx(split)), "Un.\nDeux.\n");
        assert_eq!(run(&docx(split), &original()).text, "Un. Deux.\n");
        // "Un." and "Deux." were joined by deleting the mark between them.
        let joined = r#"<w:p><w:pPr><w:rPr><w:del w:id="1"/></w:rPr></w:pPr><w:r><w:t xml:space="preserve">Un. </w:t></w:r></w:p><w:p><w:r><w:t>Deux.</w:t></w:r></w:p>"#;
        assert_eq!(text_of(&docx(joined)), "Un. Deux.\n");
        assert_eq!(run(&docx(joined), &original()).text, "Un.\nDeux.\n");
    }

    #[test]
    fn a_wholly_inserted_paragraph_and_a_deleted_table_row_exist_on_one_side_only() {
        let body = format!(
            r#"{}<w:p><w:pPr><w:rPr><w:ins w:id="1"/></w:rPr></w:pPr><w:ins w:id="2"><w:r><w:t>Nouveau.</w:t></w:r></w:ins></w:p>{}<w:tbl><w:tr>{}</w:tr><w:tr><w:trPr><w:del w:id="3"/></w:trPr>{}</w:tr></w:tbl>"#,
            p("Avant."),
            p("Après."),
            cell(&p("gardée")),
            cell(r#"<w:p><w:del w:id="4"><w:r><w:delText>retirée</w:delText></w:r></w:del></w:p>"#),
        );
        assert_eq!(text_of(&docx(&body)), "Avant.\nNouveau.\nAprès.\n\n| gardée |\n|---|\n");
        assert_eq!(
            run(&docx(&body), &original()).text,
            "Avant.\nAprès.\n\n| gardée |\n|---|\n| retirée |\n"
        );
    }

    const LINK_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
        <Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org/doc?a=1&amp;b=2" TargetMode="External"/>
        <Relationship Id="rId6" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org/" TargetMode="External"/>
    </Relationships>"#;
    const LINKS: &str = r#"<w:p><w:r><w:t xml:space="preserve">Voir </w:t></w:r><w:hyperlink r:id="rId5"><w:r><w:t>la doc</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> ou </w:t></w:r><w:hyperlink r:id="rId6"><w:r><w:t>example.org</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> ou </w:t></w:r><w:hyperlink w:anchor="_Toc1"><w:r><w:t>plus bas</w:t></w:r></w:hyperlink></w:p>"#;

    #[test]
    fn a_link_shows_its_address_only_when_the_text_does_not_already_say_it() {
        let bytes = docx_with(LINKS, &[("word/_rels/document.xml.rels", LINK_RELS)]);
        assert_eq!(
            text_of(&bytes),
            "Voir [la doc](https://example.org/doc?a=1&b=2) ou example.org ou plus bas\n"
        );
        let bare = ExtractOptions { link_targets: false, ..ExtractOptions::default() };
        assert_eq!(run(&bytes, &bare).text, "Voir la doc ou example.org ou plus bas\n");
    }

    #[test]
    fn fields_give_their_cached_result_and_never_their_instruction() {
        let body = r#"<w:p><w:r><w:t xml:space="preserve">Page </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGE \* MERGEFORMAT </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>7</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:t xml:space="preserve">, </w:t></w:r><w:fldSimple w:instr=" DATE "><w:r><w:t>13/11/2026</w:t></w:r></w:fldSimple><w:r><w:t xml:space="preserve">, </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> HYPERLINK "https://example.org/x" </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>le site</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>"#;
        assert_eq!(text_of(&docx(body)), "Page 7, 13/11/2026, [le site](https://example.org/x)\n");
    }

    const PICTURE: &str = r#"<w:p><w:r><w:t xml:space="preserve">Figure : </w:t></w:r><w:r><w:drawing><wp:inline><wp:docPr id="1" name="Image 1" descr="Courbe de
        croissance"/><a:graphic><a:graphicData><pic:pic><pic:blipFill><a:blip r:embed="rId9"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"#;

    #[test]
    fn images_leave_a_placeholder_with_their_alternative_text_and_are_counted() {
        let extracted = run(&docx(PICTURE), &ExtractOptions::default());
        assert_eq!(extracted.text, "Figure : [image: Courbe de croissance]\n");
        assert_eq!(extracted.facts.images, 1);
        assert_eq!(extracted.facts.notices(&ExtractOptions::default()), ["1 image non comparée"]);
        let hidden = ExtractOptions { images: false, ..ExtractOptions::default() };
        let extracted = run(&docx(PICTURE), &hidden);
        assert_eq!(extracted.text, "Figure :\n");
        assert_eq!(extracted.facts.images, 1);
    }

    #[test]
    fn a_text_box_is_read_once_although_the_file_stores_it_twice() {
        let content = format!("<w:txbxContent>{}</w:txbxContent>", p("Dans le cadre"));
        let body = format!(
            r#"<w:p><w:r><w:t>Ancre</w:t></w:r><w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><wp:anchor><wps:wsp xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:txbx>{content}</wps:txbx></wps:wsp></wp:anchor></w:drawing></mc:Choice><mc:Fallback><w:pict><v:shape><v:textbox>{content}</v:textbox></v:shape></w:pict></mc:Fallback></mc:AlternateContent></w:r></w:p>{}"#,
            p("Suite")
        );
        let extracted = run(&docx(&body), &ExtractOptions::default());
        assert_eq!(extracted.text, "Ancre\nDans le cadre\nSuite\n");
        assert_eq!(extracted.facts.text_boxes, 1);
    }

    const COMMENTS: &str = r#"<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:comment w:id="0" w:author="Ada"><w:p><w:r><w:t>À vérifier.</w:t></w:r></w:p></w:comment></w:comments>"#;
    const HEADER: &str = r#"<w:hdr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:t>Rapport confidentiel</w:t></w:r></w:p></w:hdr>"#;
    const FURNITURE_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
        <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>
        <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/>
        <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="/word/header2.xml"/>
    </Relationships>"#;

    #[test]
    fn comments_and_headers_are_reported_as_facts_and_shown_only_on_request() {
        let body = r#"<w:p><w:commentRangeStart w:id="0"/><w:r><w:t>Texte</w:t></w:r><w:commentRangeEnd w:id="0"/><w:r><w:commentReference w:id="0"/></w:r></w:p>"#;
        let bytes = docx_with(
            body,
            &[
                ("word/_rels/document.xml.rels", FURNITURE_RELS),
                ("word/comments.xml", COMMENTS),
                ("word/header1.xml", HEADER),
                ("word/header2.xml", HEADER),
                ("word/vbaProject.bin", "x"),
            ],
        );
        let defaults = ExtractOptions::default();
        let extracted = run(&bytes, &defaults);
        assert_eq!(extracted.text, "Texte\n");
        assert_eq!(extracted.facts.comments, 1);
        assert!(extracted.facts.has_headers_footers && extracted.facts.has_macros);
        assert_eq!(
            extracted.facts.notices(&defaults),
            ["1 commentaire ignoré", "en-têtes et pieds de page ignorés", "macros non comparées"]
        );
        let all = ExtractOptions { comments: true, headers_footers: true, ..defaults };
        let extracted = run(&bytes, &all);
        assert_eq!(
            extracted.text,
            "Texte\n\n[commentaire de Ada] À vérifier.\n\n[en-tête] Rapport confidentiel\n"
        );
        assert_eq!(extracted.facts.notices(&all), ["macros non comparées"]);
    }

    #[test]
    fn unusual_prefixes_and_strict_namespaces_are_read_like_the_usual_ones() {
        let document = r#"<x:document xmlns:x="http://purl.oclc.org/ooxml/wordprocessingml/main"><x:body><x:p><x:r><x:t>Strict</x:t></x:r></x:p></x:body></x:document>"#;
        let bytes = zip_of(&[("[Content_Types].xml", "<Types/>"), ("word/document.xml", document)]);
        let extracted = run(&bytes, &ExtractOptions::default());
        assert_eq!((extracted.format, extracted.text.as_str()), (DocFormat::Docx, "Strict\n"));
    }

    #[test]
    fn a_utf16_part_is_decoded() {
        let document = document_xml(&p("Été")).replace("UTF-8", "UTF-16");
        let mut encoded = vec![0xFF, 0xFE];
        encoded.extend(document.encode_utf16().flat_map(u16::to_le_bytes));
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file("[Content_Types].xml", SimpleFileOptions::default()).unwrap();
        writer.start_file("word/document.xml", SimpleFileOptions::default()).unwrap();
        writer.write_all(&encoded).unwrap();
        assert_eq!(text_of(&writer.finish().unwrap().into_inner()), "Été\n");
    }

    // --- .odt ---------------------------------------------------------------------------------

    #[test]
    fn odt_paragraphs_headings_spaces_tabs_and_line_breaks() {
        let body = r#"<text:h text:outline-level="2">Titre</text:h>
            <text:p>Un <text:span>mot cou</text:span><text:span>pé</text:span>,<text:s text:c="3"/>trois espaces,<text:tab/>tab<text:line-break/>suite</text:p>
            <text:p text:style-name="Title">Grand titre</text:p>
            <text:p>
                indenté
                dans le XML
            </text:p>"#;
        let none = ExtractOptions { normalization: Normalization::none(), ..ExtractOptions::default() };
        let extracted = run(&odt(body), &none);
        assert_eq!(extracted.format, DocFormat::Odt);
        assert_eq!(
            extracted.text,
            "## Titre\nUn mot coupé,   trois espaces,\ttab\nsuite\n\n# Grand titre\nindenté dans le XML \n"
        );
    }

    const LIST_STYLES: &str = r#"<text:list-style style:name="L1"><text:list-level-style-bullet text:level="1"/><text:list-level-style-bullet text:level="2"/></text:list-style>
        <text:list-style style:name="L2"><text:list-level-style-number text:level="1" style:num-format="1"/><text:list-level-style-number text:level="2" style:num-format="a"/></text:list-style>"#;

    #[test]
    fn odt_nested_lists_follow_their_list_style() {
        let body = r#"<text:list text:style-name="L1"><text:list-item><text:p>pomme</text:p><text:p>second paragraphe</text:p><text:list><text:list-item><text:p>reinette</text:p></text:list-item></text:list></text:list-item></text:list>
            <text:list text:style-name="L2"><text:list-item><text:p>premier</text:p><text:list><text:list-item><text:p>sous-point</text:p></text:list-item></text:list></text:list-item><text:list-item><text:p>second</text:p></text:list-item></text:list>
            <text:list><text:list-item><text:p>sans style</text:p></text:list-item></text:list>"#;
        assert_eq!(
            text_of(&odt_with(body, LIST_STYLES, &[])),
            "- pomme\n  second paragraphe\n  - reinette\n1. premier\n  1. sous-point\n1. second\n- sans style\n"
        );
    }

    #[test]
    fn odt_tables_notes_links_and_images() {
        let body = r##"<table:table><table:table-header-rows><table:table-row><table:table-cell><text:p>Nom</text:p></table:table-cell><table:table-cell><text:p>Ville</text:p></table:table-cell></table:table-row></table:table-header-rows>
            <table:table-row><table:table-cell><text:p>Ada</text:p><text:p>Lovelace</text:p></table:table-cell><table:covered-table-cell/></table:table-row></table:table>
            <text:p>Fait<text:note text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>Source.</text:p></text:note-body></text:note> et <text:a xlink:href="https://example.org/a">lien</text:a>, <text:a xlink:href="#ancre">interne</text:a>.</text:p>
            <text:p><draw:frame><draw:image xlink:href="Pictures/1.png"/><svg:title>Logo</svg:title></draw:frame><draw:frame><draw:object xlink:href="./Object 1"/><draw:image xlink:href="./ObjectReplacements/Object 1"/></draw:frame></text:p>
            <text:p>Cadre<draw:frame><draw:text-box><text:p>Dans le cadre</text:p></draw:text-box></draw:frame></text:p>"##;
        let extracted = run(&odt(body), &ExtractOptions::default());
        assert_eq!(
            extracted.text,
            "| Nom | Ville |\n|---|---|\n| Ada ¶ Lovelace | |\n\nFait^[Source.] et [lien](https://example.org/a), interne.\n[image: Logo][objet]\nCadre\nDans le cadre\n"
        );
        let facts = extracted.facts;
        assert_eq!(
            (facts.tables, facts.footnotes, facts.images, facts.objects, facts.text_boxes),
            (1, 1, 1, 1, 1)
        );
    }

    const ODT_TRACKED: &str = r#"<text:tracked-changes>
            <text:changed-region text:id="c1"><text:insertion><office:change-info><dc:creator>A</dc:creator></office:change-info></text:insertion></text:changed-region>
            <text:changed-region text:id="c2"><text:deletion><office:change-info><dc:creator>A</dc:creator></office:change-info><text:p>noir</text:p></text:deletion></text:changed-region>
            <text:changed-region text:id="c3"><text:deletion><office:change-info/><text:p> Fin du premier.</text:p><text:p>Début du second, </text:p></text:deletion></text:changed-region>
            <text:changed-region text:id="c4"><text:insertion><office:change-info/></text:insertion></text:changed-region>
        </text:tracked-changes>
        <text:p>Le chat <text:change text:change-id="c2"/><text:change-start text:change-id="c1"/>blanc<text:change-end text:change-id="c1"/> dort.</text:p>
        <text:p>Un.<text:change text:change-id="c3"/>deux.</text:p>
        <text:p>Trois.<text:change-start text:change-id="c4"/> Ajout.</text:p>
        <text:p>Encore.<text:change-end text:change-id="c4"/>Quatre.</text:p>"#;

    #[test]
    fn odt_tracked_changes_give_the_final_text_or_the_original_one() {
        let bytes = odt(ODT_TRACKED);
        let extracted = run(&bytes, &ExtractOptions::default());
        assert_eq!(
            extracted.text,
            "Le chat blanc dort.\nUn.deux.\nTrois. Ajout.\nEncore.Quatre.\n"
        );
        assert_eq!((extracted.facts.tracked_insertions, extracted.facts.tracked_deletions), (2, 2));
        assert_eq!(
            run(&bytes, &original()).text,
            "Le chat noir dort.\nUn. Fin du premier.\nDébut du second, deux.\nTrois.Quatre.\n"
        );
    }

    const ODT_STYLES: &str = r#"<office:document-styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"><office:master-styles><style:master-page style:name="Standard"><style:header><text:p>Société X</text:p></style:header><style:footer><text:p>Page <text:page-number>1</text:page-number></text:p></style:footer></style:master-page></office:master-styles></office:document-styles>"#;

    #[test]
    fn odt_comments_headers_and_footers_are_facts_shown_only_on_request() {
        let body = r#"<text:p>Texte<office:annotation><dc:creator>Ada</dc:creator><dc:date>2026-01-01</dc:date><text:p>À revoir.</text:p></office:annotation> annoté.</text:p>"#;
        let bytes = odt_with(body, "", &[("styles.xml", ODT_STYLES)]);
        let extracted = run(&bytes, &ExtractOptions::default());
        assert_eq!(extracted.text, "Texte annoté.\n");
        assert_eq!(extracted.facts.comments, 1);
        assert!(extracted.facts.has_headers_footers);
        let all = ExtractOptions { comments: true, headers_footers: true, ..ExtractOptions::default() };
        assert_eq!(
            run(&bytes, &all).text,
            "Texte annoté.\n\n[commentaire de Ada] À revoir.\n\n[en-tête] Société X\n[pied de page] Page 1\n"
        );
    }

    #[test]
    fn a_flat_fodt_is_read_and_its_embedded_pictures_are_not_loaded() {
        let flat = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <office:document {ODF} office:mimetype="{ODT_MIME}"><office:body><office:text>
                <text:p>Plat<draw:frame><draw:image><office:binary-data>{}</office:binary-data></draw:image></draw:frame></text:p>
            </office:text></office:body></office:document>"#,
            "iVBORw0KGgo=".repeat(100)
        );
        let extracted = run(flat.as_bytes(), &ExtractOptions::default());
        assert_eq!((extracted.format, extracted.text.as_str()), (DocFormat::Odt, "Plat[image]\n"));
    }

    // --- Normalisation ------------------------------------------------------------------------

    #[test]
    fn invisible_typographic_differences_are_removed_by_default() {
        let line = "Prix\u{a0}: 10\u{202f}€  net, coo\u{ad}pé\u{200b}ration, e\u{301}te\u{301}, ﬁn ef\u{fb01}cace, porte\u{2011}clé \t";
        assert_eq!(
            normalize_line(line, &Normalization::default()),
            "Prix : 10 € net, coopération, été, fin efficace, porte-clé"
        );
        assert_eq!(normalize_line(line, &Normalization::none()), line);
    }

    #[test]
    fn quotes_and_dashes_are_kept_unless_folding_is_asked_for() {
        let line = "L’avis « très net » de “Bob” – enfin… presque";
        assert_eq!(normalize_line(line, &Normalization::default()), line);
        assert_eq!(
            normalize_line("L’avis «\u{a0}très net\u{a0}» de “Bob” – enfin… presque", &Normalization::aggressive()),
            "L'avis \"très net\" de \"Bob\" - enfin... presque"
        );
    }

    #[test]
    fn normalize_text_works_line_by_line_and_keeps_line_ends() {
        assert_eq!(normalize_text("a\u{a0} b \nc  d\n", &Normalization::default()), "a b\nc d\n");
    }

    #[test]
    fn two_documents_differing_only_by_typography_extract_to_the_same_text() {
        let left = docx(&p("Il a dit\u{a0}: «\u{a0}non\u{a0}».  Fin "));
        let right = odt("<text:p>Il a dit : « non ». Fin</text:p>");
        assert_eq!(text_of(&left), text_of(&right));
        let exact = ExtractOptions { normalization: Normalization::none(), ..ExtractOptions::default() };
        assert_ne!(run(&left, &exact).text, run(&right, &exact).text);
    }

    // --- Files, detection, errors -------------------------------------------------------------

    #[test]
    fn supported_files_are_recognised_by_extension_except_lock_files() {
        for name in ["a.docx", "a.DOCX", "b.docm", "c.dotx", "d.odt", "e.ott", "f.fodt"] {
            assert!(is_supported(Path::new(name)), "{name}");
        }
        for name in ["a.doc", "a.txt", "docx", "~$a.docx", ".~lock.a.odt#", "a.ods"] {
            assert!(!is_supported(Path::new(name)), "{name}");
        }
        assert_eq!(DocFormat::from_extension(Path::new("x/y.ott")), Some(DocFormat::Odt));
    }

    #[test]
    fn the_content_decides_the_format_whatever_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        let misnamed = dir.path().join("lettre.docx");
        std::fs::write(&misnamed, odt("<text:p>Bonjour</text:p>")).unwrap();
        let other = dir.path().join("lettre.odt");
        std::fs::write(&other, docx(&p("Bonjour\u{a0}"))).unwrap();
        let options = ExtractOptions::default();
        assert_eq!(extract(&misnamed, &options).unwrap().format, DocFormat::Odt);
        assert_eq!(extract(&other, &options).unwrap().format, DocFormat::Docx);
        assert!(same_text(&misnamed, &other, &options).unwrap());
        assert_eq!(extract_text(&other, &options).unwrap(), "Bonjour\n");
        assert_eq!(text_hash(&misnamed, &options).unwrap(), hash_text("Bonjour\n"));
        assert_ne!(hash_text("Bonjour\n"), hash_text("Bonsoir\n"));
    }

    #[test]
    fn a_missing_file_is_an_io_error_naming_the_path() {
        let error = extract(Path::new("/nonexistent/x.docx"), &ExtractOptions::default()).unwrap_err();
        assert!(matches!(error, ExtractError::Io { .. }));
        assert!(error.to_string().starts_with("/nonexistent/x.docx: "));
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_refused() {
        assert!(matches!(fail(b"Just some text, not a document."), ExtractError::NotZip(_)));
        assert!(matches!(fail(b""), ExtractError::NotZip(_)));
        assert!(matches!(fail(b"PK\x03\x04 truncated"), ExtractError::NotZip(_)));
        assert!(fail(b"x").to_string().starts_with("test: ni une archive zip"));
    }

    #[test]
    fn password_protected_documents_are_reported_as_encrypted() {
        // Word: an OLE container with an EncryptedPackage stream.
        let mut ole = OLE_MAGIC.to_vec();
        ole.extend([0u8; 600]);
        ole.extend("EncryptedPackage".encode_utf16().flat_map(u16::to_le_bytes));
        assert!(matches!(fail(&ole), ExtractError::Encrypted(_)));
        // The same container without it: an old .doc.
        assert!(matches!(fail(&ole[..600]), ExtractError::LegacyBinary(_)));
        // OpenDocument: the manifest declares the encryption.
        let manifest = r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="content.xml"><manifest:encryption-data/></manifest:file-entry></manifest:manifest>"#;
        let bytes = zip_of(&[("mimetype", ODT_MIME), ("META-INF/manifest.xml", manifest), ("content.xml", "\u{1}\u{2}")]);
        let error = fail(&bytes);
        assert!(matches!(error, ExtractError::Encrypted(_)));
        assert_eq!(error.to_string(), "test: document chiffré ou protégé par mot de passe");
    }

    #[test]
    fn a_missing_main_part_is_reported_with_its_name() {
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
        let error = fail(&zip_of(&[("[Content_Types].xml", "<Types/>"), ("_rels/.rels", rels)]));
        assert!(matches!(&error, ExtractError::MissingPart { part, .. } if part == "word/document.xml"));
        assert_eq!(error.to_string(), "test: partie « word/document.xml » absente, document incomplet");
        let error = fail(&zip_of(&[("mimetype", ODT_MIME)]));
        assert!(matches!(&error, ExtractError::MissingPart { part, .. } if part == "content.xml"));
    }

    #[test]
    fn malformed_xml_is_reported_with_the_part_name() {
        for broken in ["<w:p><w:r></w:p>", "<w:p>", "</w:p>", "<w:p a=1/>"] {
            let document = document_xml(broken);
            let bytes = zip_of(&[("[Content_Types].xml", "<Types/>"), ("word/document.xml", &document)]);
            let error = fail(&bytes);
            assert!(
                matches!(&error, ExtractError::Xml { part, .. } if part == "word/document.xml"),
                "{broken}: {error}"
            );
        }
        let error = fail(&zip_of(&[("mimetype", ODT_MIME), ("content.xml", "<a><b></a>")]));
        assert!(error.to_string().starts_with("test: XML mal formé dans « content.xml » ("));
        let deep = format!("{}{}", "<a>".repeat(MAX_DEPTH + 1), "</a>".repeat(MAX_DEPTH + 1));
        assert!(matches!(fail(&zip_of(&[("content.xml", &deep)])), ExtractError::Xml { .. }));
    }

    #[test]
    fn other_zip_archives_and_other_office_documents_are_unsupported() {
        assert!(matches!(fail(&zip_of(&[("readme.txt", "hello")])), ExtractError::Unsupported(_)));
        let sheet = zip_of(&[("mimetype", "application/vnd.oasis.opendocument.spreadsheet"), ("content.xml", "<a/>")]);
        assert!(matches!(fail(&sheet), ExtractError::Unsupported(_)));
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
        let workbook = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#;
        let xlsx = zip_of(&[("[Content_Types].xml", "<Types/>"), ("_rels/.rels", rels), ("xl/workbook.xml", workbook)]);
        assert!(matches!(fail(&xlsx), ExtractError::Unsupported(_)));
        assert!(matches!(fail(b"<html><body>page</body></html>"), ExtractError::Unsupported(_)));
    }

    #[test]
    fn a_part_inflating_beyond_the_cap_is_refused_before_being_parsed() {
        // 64 MiB of spaces deflate to a few dozen kilobytes: the shape of a zip bomb.
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file("[Content_Types].xml", SimpleFileOptions::default()).unwrap();
        writer.start_file("word/document.xml", SimpleFileOptions::default()).unwrap();
        let chunk = vec![b' '; 1024 * 1024];
        for _ in 0..=MAX_PART_BYTES / chunk.len() as u64 {
            writer.write_all(&chunk).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        assert!(bytes.len() < 1024 * 1024);
        let error = fail(&bytes);
        assert!(matches!(&error, ExtractError::TooLarge { limit_mb: 64, .. }), "{error}");
    }

    #[test]
    fn the_syntax_hint_selects_markdown_colouring() {
        let syntax = crate::highlight::detect_syntax(Some(Path::new(SYNTAX_HINT)), "");
        assert_eq!(syntax.as_deref(), Some("Markdown"));
    }

    #[test]
    fn extracted_text_feeds_the_line_diff_with_one_changed_paragraph_as_one_paired_line() {
        let left = text_of(&docx(&format!("{}{}{}", p("Premier paragraphe."), p("Le chat noir dort sur le canapé."), p("Fin."))));
        let right = text_of(&docx(&format!("{}{}{}", p("Premier paragraphe."), p("Le chat blanc dort sur le canapé."), p("Fin."))));
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let result = crate::diff::diff_lines(&lines(&left), &lines(&right), crate::diff::DiffOptions::default());
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(result.spans.len(), 1);
        let span = &result.spans[0].left[0];
        assert_eq!(&lines(&left)[1][span.range.start as usize..span.range.end as usize], "noir");
    }
}
