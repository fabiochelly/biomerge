//! JSON-aware comparison by projection.
//!
//! The line diff shows every line of both sides once and in order, so it cannot align two
//! objects whose keys are merely written in a different order. Instead of a second diff engine,
//! each file is parsed and re-emitted as a canonical text, one member or element per line, where
//! what JSON calls meaningless has been normalised: the follower side's keys follow the
//! reference side's order, and a value written differently but equal (`1.0` and `1`) takes the
//! reference's spelling. The ordinary line diff, block transfer and save then work on that text.
//!
//! Exactness comes first. Numbers and strings are kept as the spelling found in the file and
//! never go through `f64` or a decoded `String`, duplicate keys are kept as separate members,
//! and equality by value is decided on exact decimal digits. This is why the parser is written
//! here rather than borrowed: `serde_json` drops duplicate keys, forgets how values were spelled
//! and only keeps long numbers with a feature that changes its behaviour for the whole build.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::Range;
use std::path::Path;

use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};

use crate::Side;
use crate::diff::{DiffOptions, DiffResult, diff_lines_with};
use crate::ignore_rules::IgnoreRules;

/// Deeper nesting is refused: every walk here is recursive and must fit a worker's stack.
pub const MAX_DEPTH: usize = 256;
/// A summary longer than this is useless to read and costly to keep.
pub const MAX_SUMMARY_CHANGES: usize = 10_000;
const PREVIEW_CHARS: usize = 80;
const NONE: u32 = u32::MAX;
/// A canonical number is written in full up to this many zeros after its last other digit.
const MAX_PLAIN_ZEROS: i64 = 21;
/// Below this many key comparisons, pairing members by scanning beats building a hash map.
const SMALL_PAIRING: usize = 64;

// ---------------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------------

/// A JSON value exactly as written. Derived equality is therefore equality of spelling and
/// order; use [`equal`] for equality of value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    /// The number as spelled in the source, a valid JSON number of any length.
    Number(Box<str>),
    /// The text between the quotes, escapes untouched (a lone `\ud800` has no decoded form).
    String(Box<str>),
    Array(Vec<Value>),
    /// Members in source order, duplicate keys included.
    Object(Vec<Member>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The key between its quotes, escapes untouched.
    pub key: Box<str>,
    pub value: Value,
}

impl Value {
    /// The value on one line without any space, spelled as in the source.
    pub fn to_compact(&self) -> String {
        let options = JsonOptions::as_written();
        let mut emitter = Emitter::new(&options, None, Collect(Vec::new()));
        emitter.emit(self, None, 0);
        emitter.line
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// One value for the whole file.
    Json,
    /// JSON Lines (`.jsonl`, `.ndjson`): one value per line.
    JsonLines,
}

impl Format {
    /// The format announced by a file extension. JSONC and JSON5 are deliberately absent:
    /// their comments could not be written back.
    pub fn from_path(path: &Path) -> Option<Format> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "json" | "geojson" | "jsonld" | "webmanifest" | "ipynb" => Some(Format::Json),
            "jsonl" | "ndjson" => Some(Format::JsonLines),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indent {
    Spaces(u8),
    Tab,
}

impl Indent {
    fn unit(self) -> String {
        match self {
            Indent::Spaces(count) => " ".repeat(count as usize),
            Indent::Tab => "\t".to_owned(),
        }
    }
}

/// How a file was laid out, so that its projection keeps its habits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// One nesting level; two spaces when the file gives no clue (single line, flat).
    pub indent: Indent,
    pub final_newline: bool,
    /// JSON Lines only: `{"a": 1}` rather than `{"a":1}`.
    pub colon_space: bool,
    /// JSON Lines only: `[1, 2]` rather than `[1,2]`.
    pub comma_space: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            indent: Indent::Spaces(2),
            final_newline: true,
            colon_space: false,
            comma_space: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Why a text is not JSON, and where. Line and column are 1-based, the column in characters.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("ligne {line}, colonne {column} : {kind}")]
pub struct JsonError {
    pub line: u32,
    pub column: u32,
    pub kind: JsonErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JsonErrorKind {
    #[error("document vide")]
    Empty,
    #[error("fin de fichier inattendue")]
    UnexpectedEnd,
    #[error("caractère inattendu « {0} »")]
    UnexpectedChar(char),
    #[error("les commentaires ne font pas partie de JSON (fichier JSONC ou JSON5)")]
    Comment,
    #[error("virgule finale interdite en JSON (fichier JSONC ou JSON5)")]
    TrailingComma,
    #[error("nombre invalide")]
    InvalidNumber,
    #[error("séquence d'échappement invalide")]
    InvalidEscape,
    #[error("caractère de contrôle dans une chaîne")]
    ControlInString,
    #[error("chaîne non terminée")]
    UnterminatedString,
    #[error("contenu après la fin du document")]
    TrailingData,
    #[error("imbrication trop profonde (plus de {MAX_DEPTH} niveaux)")]
    TooDeep,
}

impl JsonErrorKind {
    /// The text is JSON with extensions (JSONC, JSON5) rather than broken JSON.
    pub fn is_dialect(&self) -> bool {
        matches!(self, JsonErrorKind::Comment | JsonErrorKind::TrailingComma)
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
    colon_space: Option<bool>,
    comma_space: Option<bool>,
}

type Parsed<T> = Result<T, JsonError>;

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            pos: 0,
            depth: 0,
            colon_space: None,
            comma_space: None,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    /// Positions are only computed for errors: the happy path never counts lines.
    fn error_at(&self, pos: usize, kind: JsonErrorKind) -> JsonError {
        let before = &self.text[..pos];
        let line_start = before.rfind('\n').map_or(0, |at| at + 1);
        JsonError {
            line: before.bytes().filter(|&byte| byte == b'\n').count() as u32 + 1,
            column: before[line_start..].chars().count() as u32 + 1,
            kind,
        }
    }

    fn error(&self, kind: JsonErrorKind) -> JsonError {
        self.error_at(self.pos, kind)
    }

    fn unexpected(&self) -> JsonError {
        let mut rest = self.text[self.pos..].chars();
        let kind = match (rest.next(), rest.next()) {
            (None, _) => JsonErrorKind::UnexpectedEnd,
            (Some('/'), Some('/' | '*')) => JsonErrorKind::Comment,
            (Some(other), _) => JsonErrorKind::UnexpectedChar(other),
        };
        self.error(kind)
    }

    fn document(&mut self) -> Parsed<Value> {
        self.skip_whitespace();
        if self.pos == self.bytes.len() {
            return Err(self.error(JsonErrorKind::Empty));
        }
        let value = self.value()?;
        self.skip_whitespace();
        if self.pos < self.bytes.len() {
            let error = self.unexpected();
            return Err(match error.kind {
                JsonErrorKind::Comment => error,
                _ => self.error(JsonErrorKind::TrailingData),
            });
        }
        Ok(value)
    }

    fn value(&mut self) -> Parsed<Value> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::String(self.string()?.into())),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') if self.literal("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.literal("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.literal("null") => Ok(Value::Null),
            _ => Err(self.unexpected()),
        }
    }

    fn literal(&mut self, word: &str) -> bool {
        let found = self.bytes[self.pos..].starts_with(word.as_bytes());
        if found {
            self.pos += word.len();
        }
        found
    }

    fn open(&mut self) -> Parsed<()> {
        if self.depth == MAX_DEPTH {
            return Err(self.error(JsonErrorKind::TooDeep));
        }
        self.depth += 1;
        self.pos += 1;
        self.skip_whitespace();
        Ok(())
    }

    /// Consumes a separating comma; one followed by the closing bracket is a dialect.
    fn comma(&mut self, close: u8) -> Parsed<()> {
        let at = self.pos;
        self.pos += 1;
        if self.comma_space.is_none() {
            self.comma_space = Some(self.peek() == Some(b' '));
        }
        self.skip_whitespace();
        if self.peek() == Some(close) {
            return Err(self.error_at(at, JsonErrorKind::TrailingComma));
        }
        Ok(())
    }

    fn array(&mut self) -> Parsed<Value> {
        self.open()?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
        } else {
            loop {
                items.push(self.value()?);
                self.skip_whitespace();
                match self.peek() {
                    Some(b',') => self.comma(b']')?,
                    Some(b']') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.unexpected()),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Array(items))
    }

    fn object(&mut self) -> Parsed<Value> {
        self.open()?;
        let mut members = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
        } else {
            loop {
                if self.peek() != Some(b'"') {
                    return Err(self.unexpected());
                }
                let key = self.string()?.into();
                self.skip_whitespace();
                if self.peek() != Some(b':') {
                    return Err(self.unexpected());
                }
                self.pos += 1;
                if self.colon_space.is_none() {
                    self.colon_space = Some(self.peek() == Some(b' '));
                }
                self.skip_whitespace();
                let value = self.value()?;
                members.push(Member { key, value });
                self.skip_whitespace();
                match self.peek() {
                    Some(b',') => self.comma(b'}')?,
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.unexpected()),
                }
            }
        }
        self.depth -= 1;
        Ok(Value::Object(members))
    }

    /// Validates a string and returns what stands between its quotes.
    fn string(&mut self) -> Parsed<&'a str> {
        let open = self.pos;
        self.pos += 1;
        loop {
            match self.peek() {
                Some(b'"') => break,
                Some(b'\\') => {
                    let valid = match self.bytes.get(self.pos + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => 2,
                        Some(b'u')
                            if self.bytes.get(self.pos + 2..self.pos + 6).is_some_and(|hex| {
                                hex.iter().all(u8::is_ascii_hexdigit)
                            }) =>
                        {
                            6
                        }
                        _ => return Err(self.error(JsonErrorKind::InvalidEscape)),
                    };
                    self.pos += valid;
                }
                Some(b'\n' | b'\r') | None => {
                    return Err(self.error_at(open, JsonErrorKind::UnterminatedString));
                }
                Some(control) if control < 0x20 => {
                    return Err(self.error(JsonErrorKind::ControlInString));
                }
                Some(_) => self.pos += 1,
            }
        }
        self.pos += 1;
        Ok(&self.text[open + 1..self.pos - 1])
    }

    fn digits(&mut self) -> bool {
        let start = self.pos;
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
        self.pos > start
    }

    fn number(&mut self) -> Parsed<Value> {
        let start = self.pos;
        let invalid = |parser: &Self| Err(parser.error_at(start, JsonErrorKind::InvalidNumber));
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return invalid(self),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !self.digits() {
                return invalid(self);
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            if !self.digits() {
                return invalid(self);
            }
        }
        // `01`, `0x1F`, `1.2.3`, `NaN`-like tails: name the number rather than a stray character.
        if let Some(b'0'..=b'9' | b'.' | b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'+' | b'-') = self.peek() {
            return invalid(self);
        }
        Ok(Value::Number(self.text[start..self.pos].into()))
    }
}

/// Splits on `\n`, `\r\n` and `\r`, without the phantom line after a final ending.
fn source_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let (mut start, mut at) = (0, 0);
    while at < bytes.len() {
        if bytes[at] == b'\n' || bytes[at] == b'\r' {
            lines.push(&text[start..at]);
            if bytes[at] == b'\r' && bytes.get(at + 1) == Some(&b'\n') {
                at += 1;
            }
            start = at + 1;
        }
        at += 1;
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// In a pretty-printed file the first indented line is one level deep.
fn detect_indent(lines: &[&str]) -> Indent {
    for line in lines {
        let content = line.trim_start_matches([' ', '\t']);
        let indent = &line[..line.len() - content.len()];
        if !indent.is_empty() && !content.is_empty() {
            return if indent.starts_with('\t') {
                Indent::Tab
            } else {
                Indent::Spaces(indent.len().min(16) as u8)
            };
        }
    }
    Indent::Spaces(2)
}

/// A parsed file: its value, and how it was laid out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonDoc {
    /// For [`Format::JsonLines`], an array holding one element per record.
    pub root: Value,
    pub format: Format,
    pub layout: Layout,
    canonical_layout: bool,
}

impl JsonDoc {
    /// Parses strict JSON (RFC 8259). A leading byte order mark is accepted.
    pub fn parse(text: &str, format: Format) -> Result<JsonDoc, JsonError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let lines = source_lines(text);
        let (root, colon_space, comma_space) = match format {
            Format::Json => {
                let mut parser = Parser::new(text);
                (parser.document()?, parser.colon_space, parser.comma_space)
            }
            Format::JsonLines => {
                let (mut colon_space, mut comma_space) = (None, None);
                let mut records = Vec::new();
                for (number, line) in lines.iter().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let mut parser = Parser::new(line);
                    records.push(parser.document().map_err(|error| JsonError {
                        line: number as u32 + 1,
                        ..error
                    })?);
                    colon_space = colon_space.or(parser.colon_space);
                    comma_space = comma_space.or(parser.comma_space);
                }
                (Value::Array(records), colon_space, comma_space)
            }
        };
        let mut doc = JsonDoc {
            root,
            format,
            layout: Layout {
                indent: detect_indent(&lines),
                final_newline: text.ends_with(['\n', '\r']),
                colon_space: colon_space.unwrap_or(false),
                comma_space: comma_space.unwrap_or(false),
            },
            canonical_layout: false,
        };
        let options = JsonOptions::as_written();
        let matches = Matches {
            expected: lines.iter().copied(),
            same: true,
        };
        let (mut matches, _) = doc.emit(None, &options, matches);
        doc.canonical_layout = matches.same && matches.expected.next().is_none();
        Ok(doc)
    }

    /// Parses a pane's lines. They do not say whether the file ends with a newline, which the
    /// `Document` tracks by itself: `layout.final_newline` is then simply `true`.
    pub fn parse_lines<L: AsRef<str>>(lines: &[L], format: Format) -> Result<JsonDoc, JsonError> {
        let mut text = String::with_capacity(lines.iter().map(|line| line.as_ref().len() + 1).sum());
        for line in lines {
            text.push_str(line.as_ref());
            text.push('\n');
        }
        JsonDoc::parse(&text, format)
    }

    /// The source was already laid out exactly as [`JsonDoc::lines`] writes it, so projecting it
    /// without normalisation and saving would leave the file as it is.
    pub fn has_canonical_layout(&self) -> bool {
        self.canonical_layout
    }

    /// This document alone in its own layout, nothing reordered or respelled.
    pub fn lines(&self) -> Vec<String> {
        self.emit(None, &JsonOptions::as_written(), Collect(Vec::new())).0.0
    }

    fn emit<S: Sink>(&self, peer: Option<&Value>, options: &JsonOptions, sink: S) -> (S, bool) {
        let pretty = (self.format == Format::Json).then_some(self.layout.indent);
        let mut emitter = Emitter::new(options, pretty, sink);
        if self.layout.colon_space {
            emitter.colon = ": ";
        }
        emitter.comma_space = self.layout.comma_space;
        match (self.format, &self.root) {
            // One record per line: the file must stay JSON Lines once saved.
            (Format::JsonLines, Value::Array(records)) => {
                let peer_records = match peer {
                    Some(Value::Array(records)) => Some(records.as_slice()),
                    _ => None,
                };
                let peers = peer_records.map_or(Peers::None, |p| element_peers(p, records, options));
                let order = match options.array_order {
                    ArrayOrder::Unordered => peers.follow_order(),
                    ArrayOrder::Ordered => None,
                };
                emitter.normalized |= order.is_some();
                for position in 0..records.len() {
                    let index = order.as_ref().map_or(position, |order| order[position] as usize);
                    let peer = peer_records.and_then(|p| peers.of(index).map(|at| &p[at]));
                    emitter.emit(&records[index], peer, 0);
                    emitter.flush();
                }
            }
            _ => {
                emitter.emit(&self.root, peer, 0);
                emitter.flush();
            }
        }
        (emitter.sink, emitter.normalized)
    }

    fn project(&self, peer: Option<&Value>, options: &JsonOptions) -> Projection {
        let (lines, normalized) = self.emit(peer, options, Collect(Vec::new()));
        Projection {
            lines: lines.0,
            final_newline: self.layout.final_newline,
            reformatted: !self.canonical_layout,
            normalized,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Equality by value
// ---------------------------------------------------------------------------------------------

/// The one spelling of a number's exact value: `1.0`, `1e0` and `10E-1` all give `1`.
///
/// Pure digit shuffling, so no precision is ever lost. A negative zero stays `-0`, distinct from
/// `0`, because parsers that read floats keep them apart.
pub fn canonical_number(raw: &str) -> Cow<'_, str> {
    // Without fraction or exponent a JSON number is an integer without leading zeros, canonical
    // unless it ends with more zeros than the plain form allows.
    let plain = !raw.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'));
    if plain && raw.len() - raw.trim_end_matches('0').len() <= MAX_PLAIN_ZEROS as usize {
        return Cow::Borrowed(raw);
    }
    let (negative, rest) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => (rest, "0"),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all = format!("{integer}{fraction}");
    let digits = all.trim_start_matches('0').trim_end_matches('0');
    let sign = if negative { "-" } else { "" };
    if digits.is_empty() {
        return Cow::Owned(format!("{sign}0"));
    }
    let dropped_zeros = (all.trim_start_matches('0').len() - digits.len()) as i64;
    // The value is `digits × 10^exponent`; an exponent too long for an `i64` cannot be
    // normalised, and the spelling is then only made comparable in case.
    let Some(exponent) = exponent
        .parse::<i64>()
        .ok()
        .and_then(|exponent| exponent.checked_sub(fraction.len() as i64))
        .and_then(|exponent| exponent.checked_add(dropped_zeros))
    else {
        return Cow::Owned(raw.replace('E', "e"));
    };
    let count = digits.len() as i64;
    let Some(point) = count.checked_add(exponent) else {
        return Cow::Owned(raw.replace('E', "e"));
    };
    // Plain digits as far as they stay readable, scientific beyond. Every integer of the
    // file's own digits stays plain, whatever its length.
    let text = if (0..=MAX_PLAIN_ZEROS).contains(&exponent) {
        format!("{sign}{digits}{}", "0".repeat(exponent as usize))
    } else if exponent < 0 && point > 0 {
        let (whole, part) = digits.split_at(point as usize);
        format!("{sign}{whole}.{part}")
    } else if exponent < 0 && point > -6 {
        format!("{sign}0.{}{digits}", "0".repeat(-point as usize))
    } else {
        let (first, others) = digits.split_at(1);
        let dot = if others.is_empty() { "" } else { "." };
        format!("{sign}{first}{dot}{others}e{}", point - 1)
    };
    Cow::Owned(text)
}

/// The one spelling of a string's exact content: escapes are resolved (`\u00e9` gives `é`,
/// `\/` gives `/`) except those JSON requires, written in their shortest lowercase form. A lone
/// surrogate has no character and stays an escape. Total: never fails on malformed input.
pub fn canonical_string(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\\') {
        return Cow::Borrowed(raw);
    }
    let unit = |text: &str| -> Option<u32> {
        let hex = text.strip_prefix("\\u")?.get(..4)?;
        u32::from_str_radix(hex, 16).ok().filter(|_| hex.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let mut chars = rest[1..].chars();
        let (decoded, length) = match chars.next() {
            Some('u') => match unit(rest) {
                Some(high @ 0xD800..=0xDBFF) => match unit(&rest[6..]) {
                    Some(low @ 0xDC00..=0xDFFF) => {
                        let scalar = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                        (char::from_u32(scalar), 12)
                    }
                    _ => (None, 6),
                },
                Some(code) => (char::from_u32(code), 6),
                None => (None, 2),
            },
            Some('b') => (Some('\u{8}'), 2),
            Some('f') => (Some('\u{c}'), 2),
            Some('n') => (Some('\n'), 2),
            Some('r') => (Some('\r'), 2),
            Some('t') => (Some('\t'), 2),
            Some(same @ ('"' | '\\' | '/')) => (Some(same), 2),
            Some(other) => (None, 1 + other.len_utf8()),
            None => (None, 1),
        };
        match decoded {
            Some('"') => out.push_str("\\\""),
            Some('\\') => out.push_str("\\\\"),
            Some('\u{8}') => out.push_str("\\b"),
            Some('\u{c}') => out.push_str("\\f"),
            Some('\n') => out.push_str("\\n"),
            Some('\r') => out.push_str("\\r"),
            Some('\t') => out.push_str("\\t"),
            Some(control) if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            Some(plain) => out.push(plain),
            // A lone surrogate, or an escape JSON does not know: kept, in lowercase.
            None => out.push_str(&rest[..length].to_ascii_lowercase().replace("\\U", "\\u")),
        }
        rest = &rest[length..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn number_key<'v>(raw: &'v str, options: &JsonOptions) -> Cow<'v, str> {
    if options.numbers_by_value {
        canonical_number(raw)
    } else {
        Cow::Borrowed(raw)
    }
}

fn string_key<'v>(raw: &'v str, options: &JsonOptions) -> Cow<'v, str> {
    if options.strings_by_value {
        canonical_string(raw)
    } else {
        Cow::Borrowed(raw)
    }
}

fn scalars_equal(left: &Value, right: &Value, options: &JsonOptions) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b || number_key(a, options) == number_key(b, options),
        (Value::String(a), Value::String(b)) => a == b || string_key(a, options) == string_key(b, options),
        _ => false,
    }
}

fn mix(hash: u64, word: u64) -> u64 {
    (hash.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95)
}

fn scramble(mut hash: u64) -> u64 {
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

fn hash_text(seed: u64, text: &str) -> u64 {
    let mut hash = mix(seed, text.len() as u64);
    for chunk in text.as_bytes().chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        hash = mix(hash, u64::from_le_bytes(word));
    }
    hash
}

/// A hash equal for values equal under `options`: member order never counts, element order
/// only when arrays are ordered. Used to pair array elements; a collision can only pair two
/// different elements, which the walk that follows then compares for real.
fn hash_value(value: &Value, options: &JsonOptions) -> u64 {
    scramble(match value {
        Value::Null => 1,
        Value::Bool(flag) => 2 + *flag as u64,
        Value::Number(raw) => hash_text(4, &number_key(raw, options)),
        Value::String(raw) => hash_text(5, &string_key(raw, options)),
        Value::Array(items) => {
            let hashes = items.iter().map(|item| hash_value(item, options));
            match options.array_order {
                ArrayOrder::Ordered => hashes.fold(6, mix),
                ArrayOrder::Unordered => mix(7, hashes.fold(items.len() as u64, u64::wrapping_add)),
            }
        }
        Value::Object(members) => {
            let hashes = members.iter().map(|member| {
                scramble(mix(hash_text(8, &canonical_string(&member.key)), hash_value(&member.value, options)))
            });
            mix(9, hashes.fold(members.len() as u64, u64::wrapping_add))
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyOrder {
    /// Every object keeps the order of its file: a reordered key shows as removed and added.
    AsWritten,
    /// The reference side keeps its order; on the other side, keys present on both sides follow
    /// the reference and the others stay behind the key they followed in their file.
    #[default]
    FollowReference,
    /// Both sides sorted by key, in code point order.
    Alphabetical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArrayOrder {
    /// Element order is data: the projection never moves an element.
    #[default]
    Ordered,
    /// The user declares arrays to be sets: the other side's elements follow the reference's
    /// order, matched by identity key or else by value. Saving that side reorders its arrays.
    Unordered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonOptions {
    pub key_order: KeyOrder,
    pub array_order: ArrayOrder,
    /// The side whose order and spellings the other one follows.
    pub reference: Side,
    /// `1.0`, `1` and `10e-1` are the same number.
    pub numbers_by_value: bool,
    /// `"\u00e9"` and `"é"` are the same string.
    pub strings_by_value: bool,
    /// Keys tried in turn to recognise the elements of an array of objects: the first one that
    /// every element of both sides carries once, with a string or number unique on its side.
    /// Empty: elements are matched by content only.
    pub identity_keys: Vec<String>,
}

impl Default for JsonOptions {
    fn default() -> Self {
        Self {
            key_order: KeyOrder::default(),
            array_order: ArrayOrder::default(),
            reference: Side::Left,
            numbers_by_value: true,
            strings_by_value: true,
            identity_keys: ["id", "_id", "uuid", "guid", "key", "name"].map(String::from).to_vec(),
        }
    }
}

impl JsonOptions {
    /// Nothing normalised: the projection only pretty-prints.
    pub fn as_written() -> Self {
        Self {
            key_order: KeyOrder::AsWritten,
            array_order: ArrayOrder::Ordered,
            reference: Side::Left,
            numbers_by_value: false,
            strings_by_value: false,
            identity_keys: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pairing members and elements
// ---------------------------------------------------------------------------------------------

/// For each child of the follower container, its counterpart in the reference container.
enum Peers {
    None,
    /// Same length, child `i` faces child `i`.
    Positional,
    Map(Vec<u32>),
}

impl Peers {
    fn from_map(map: Vec<u32>, reference_len: usize) -> Peers {
        let positional = map.len() == reference_len && map.iter().enumerate().all(|(at, &peer)| peer == at as u32);
        if positional { Peers::Positional } else { Peers::Map(map) }
    }

    fn of(&self, index: usize) -> Option<usize> {
        match self {
            Peers::None => None,
            Peers::Positional => Some(index),
            Peers::Map(map) => Some(map[index]).filter(|&peer| peer != NONE).map(|peer| peer as usize),
        }
    }

    /// For each reference child, its follower counterpart.
    fn inverse(&self, reference_len: usize) -> Vec<u32> {
        match self {
            Peers::None => vec![NONE; reference_len],
            Peers::Positional => (0..reference_len as u32).collect(),
            Peers::Map(map) => {
                let mut inverse = vec![NONE; reference_len];
                for (at, &peer) in map.iter().enumerate() {
                    if peer != NONE {
                        inverse[peer as usize] = at as u32;
                    }
                }
                inverse
            }
        }
    }

    /// The follower's children in the reference's order; a child the reference lacks stays
    /// right behind the one it followed. `None` when that is already the case.
    fn follow_order(&self) -> Option<Vec<u32>> {
        let Peers::Map(map) = self else { return None };
        let mut anchor = -1i64;
        let mut keyed: Vec<(i64, bool, u32)> = map
            .iter()
            .enumerate()
            .map(|(at, &peer)| {
                if peer != NONE {
                    anchor = peer as i64;
                }
                (anchor, peer == NONE, at as u32)
            })
            .collect();
        if keyed.is_sorted() {
            return None;
        }
        keyed.sort_unstable();
        Some(keyed.into_iter().map(|(.., at)| at).collect())
    }
}

/// Pairs members by key content; the n-th duplicate of a key faces the n-th duplicate.
fn member_peers(reference: &[Member], follower: &[Member]) -> Peers {
    if reference.len() == follower.len() && reference.iter().zip(follower).all(|(a, b)| a.key == b.key) {
        return Peers::Positional;
    }
    let mut map = vec![NONE; follower.len()];
    if reference.len() * follower.len() <= SMALL_PAIRING {
        let mut taken = [false; SMALL_PAIRING];
        for (slot, member) in map.iter_mut().zip(follower) {
            let key = canonical_string(&member.key);
            let found = reference
                .iter()
                .enumerate()
                .position(|(at, candidate)| !taken[at] && canonical_string(&candidate.key) == key);
            if let Some(at) = found {
                taken[at] = true;
                *slot = at as u32;
            }
        }
    } else {
        // `heads` gives the first unclaimed member of each key, `next` chains its duplicates.
        let mut heads: HashMap<Cow<'_, str>, u32> = HashMap::with_capacity(reference.len());
        let mut next = vec![NONE; reference.len()];
        for (at, member) in reference.iter().enumerate().rev() {
            if let Some(later) = heads.insert(canonical_string(&member.key), at as u32) {
                next[at] = later;
            }
        }
        for (slot, member) in map.iter_mut().zip(follower) {
            if let Some(head) = heads.get_mut(&canonical_string(&member.key)) {
                if *head != NONE {
                    *slot = *head;
                    *head = next[*head as usize];
                }
            }
        }
    }
    Peers::from_map(map, reference.len())
}

type Identity<'v> = (bool, Cow<'v, str>);

fn identity_of<'v>(value: &'v Value, key: &str, options: &JsonOptions) -> Option<Identity<'v>> {
    let Value::Object(members) = value else { return None };
    let mut carrying = members.iter().filter(|member| canonical_string(&member.key) == key);
    let (member, None) = (carrying.next()?, carrying.next()) else { return None };
    match &member.value {
        Value::String(raw) => Some((true, string_key(raw, options))),
        Value::Number(raw) => Some((false, number_key(raw, options))),
        _ => None,
    }
}

/// Pairs the elements of two arrays of objects by the first identity key that fits both.
fn identity_peers(reference: &[Value], follower: &[Value], options: &JsonOptions) -> Option<Vec<u32>> {
    if reference.is_empty() || follower.is_empty() {
        return None;
    }
    'keys: for key in &options.identity_keys {
        let mut index = HashMap::with_capacity(reference.len());
        for (at, item) in reference.iter().enumerate() {
            let Some(identity) = identity_of(item, key, options) else {
                continue 'keys;
            };
            if index.insert(identity, at as u32).is_some() {
                continue 'keys;
            }
        }
        let mut seen = HashSet::with_capacity(follower.len());
        let mut map = Vec::with_capacity(follower.len());
        for item in follower {
            let Some(identity) = identity_of(item, key, options) else { continue 'keys };
            map.push(index.get(&identity).copied().unwrap_or(NONE));
            if !seen.insert(identity) {
                continue 'keys;
            }
        }
        return Some(map);
    }
    None
}

struct Hashes<'a>(&'a [u64]);

impl<'a> TokenSource for Hashes<'a> {
    type Token = u64;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, u64>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

/// Pairs the elements of two arrays: by identity key when the arrays have one, else by content,
/// never in quadratic time. Elements left over face each other in order, as probable edits.
fn element_peers(reference: &[Value], follower: &[Value], options: &JsonOptions) -> Peers {
    if let Some(map) = identity_peers(reference, follower, options) {
        return Peers::from_map(map, reference.len());
    }
    let hashes = |items: &[Value]| -> Vec<u64> { items.iter().map(|item| hash_value(item, options)).collect() };
    let (before, after) = (hashes(reference), hashes(follower));
    if before == after {
        return Peers::Positional;
    }
    let mut map = vec![NONE; follower.len()];
    match options.array_order {
        ArrayOrder::Ordered => {
            // The line diff's algorithm, on one hash per element: near linear, and a changed
            // run is paired by position.
            let input = InternedInput::new(Hashes(&before), Hashes(&after));
            let diff = Diff::compute(Algorithm::Histogram, &input);
            let (mut a, mut b) = (0usize, 0usize);
            let mut pair_run = |a: &mut usize, b: &mut usize, a_end: usize, b_end: usize| {
                while *a < a_end && *b < b_end {
                    map[*b] = *a as u32;
                    *a += 1;
                    *b += 1;
                }
                (*a, *b) = (a_end, b_end);
            };
            for hunk in diff.hunks() {
                pair_run(&mut a, &mut b, hunk.before.start as usize, hunk.after.start as usize);
                pair_run(&mut a, &mut b, hunk.before.end as usize, hunk.after.end as usize);
            }
            pair_run(&mut a, &mut b, before.len(), after.len());
        }
        ArrayOrder::Unordered => {
            let mut heads: HashMap<u64, u32> = HashMap::with_capacity(before.len());
            let mut next = vec![NONE; before.len()];
            for (at, &hash) in before.iter().enumerate().rev() {
                if let Some(later) = heads.insert(hash, at as u32) {
                    next[at] = later;
                }
            }
            let mut taken = vec![false; before.len()];
            for (slot, hash) in map.iter_mut().zip(&after) {
                if let Some(head) = heads.get_mut(hash) {
                    if *head != NONE {
                        *slot = *head;
                        taken[*head as usize] = true;
                        *head = next[*head as usize];
                    }
                }
            }
            let mut free = (0..before.len() as u32).filter(|&at| !taken[at as usize]);
            for slot in map.iter_mut().filter(|slot| **slot == NONE) {
                let Some(at) = free.next() else { break };
                *slot = at;
            }
        }
    }
    Peers::from_map(map, reference.len())
}

// ---------------------------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------------------------

/// One side as the text to show, compare and save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    /// Lines without their ending. One member or element per line; one record per line for
    /// JSON Lines.
    pub lines: Vec<String>,
    pub final_newline: bool,
    /// The file was not laid out like this (other indentation habits, several values per line,
    /// single line…): saving the projection re-indents it, values untouched.
    pub reformatted: bool,
    /// Keys or elements were reordered, or values respelled after the reference: saving the
    /// projection writes that order and those spellings, values untouched.
    pub normalized: bool,
}

impl Projection {
    /// Saving this text unedited would not give back the file as it was: worth a warning.
    pub fn alters_file(&self) -> bool {
        self.reformatted || self.normalized
    }

    pub fn text(&self, eol: &str) -> String {
        let mut text = self.lines.join(eol);
        if self.final_newline && !self.lines.is_empty() {
            text.push_str(eol);
        }
        text
    }
}

/// Projects both sides for comparison, as `[left, right]` (index with `Side::index`).
///
/// The reference side is only pretty-printed (and sorted under [`KeyOrder::Alphabetical`]); the
/// other side is aligned on it as `options` allow. Each side keeps its own indentation: compare
/// the result with [`diff_json_lines`], which does not look at it.
pub fn project(left: &JsonDoc, right: &JsonDoc, options: &JsonOptions) -> [Projection; 2] {
    match options.reference {
        Side::Left => [left.project(None, options), right.project(Some(&left.root), options)],
        Side::Right => [left.project(Some(&right.root), options), right.project(None, options)],
    }
}

trait Sink {
    /// Takes a finished line and leaves the buffer empty.
    fn line(&mut self, line: &mut String);
}

struct Collect(Vec<String>);

impl Sink for Collect {
    fn line(&mut self, line: &mut String) {
        // An exact-size copy: taking the buffer would regrow it for every line.
        self.0.push(line.as_str().to_owned());
        line.clear();
    }
}

/// Compares the emitted lines with the source instead of storing them.
struct Matches<I> {
    expected: I,
    same: bool,
}

impl<'a, I: Iterator<Item = &'a str>> Sink for Matches<I> {
    fn line(&mut self, line: &mut String) {
        self.same = self.same && self.expected.next() == Some(line.as_str());
        line.clear();
    }
}

struct Emitter<'o, S> {
    options: &'o JsonOptions,
    /// One indentation level; `None` writes everything on the current line.
    unit: Option<String>,
    colon: &'static str,
    comma_space: bool,
    line: String,
    sink: S,
    normalized: bool,
}

impl<'o, S: Sink> Emitter<'o, S> {
    fn new(options: &'o JsonOptions, pretty: Option<Indent>, sink: S) -> Self {
        Self {
            options,
            unit: pretty.map(Indent::unit),
            colon: if pretty.is_some() { ": " } else { ":" },
            comma_space: false,
            line: String::new(),
            sink,
            normalized: false,
        }
    }

    fn flush(&mut self) {
        self.sink.line(&mut self.line);
    }

    fn break_line(&mut self, depth: usize) {
        if let Some(unit) = &self.unit {
            self.sink.line(&mut self.line);
            for _ in 0..depth {
                self.line.push_str(unit);
            }
        }
    }

    fn next_child(&mut self, position: usize, depth: usize) {
        if position > 0 {
            self.line.push(',');
            if self.unit.is_none() && self.comma_space {
                self.line.push(' ');
            }
        }
        self.break_line(depth);
    }

    /// Writes `value`; `peer` is what faces it on the reference side, if this is the follower.
    fn emit(&mut self, value: &Value, peer: Option<&Value>, depth: usize) {
        match value {
            Value::Null => self.line.push_str("null"),
            Value::Bool(flag) => self.line.push_str(if *flag { "true" } else { "false" }),
            Value::Number(raw) => {
                let text = match peer {
                    Some(Value::Number(theirs)) if theirs != raw && scalars_equal(value, peer.unwrap(), self.options) => {
                        self.normalized = true;
                        theirs
                    }
                    _ => raw,
                };
                self.line.push_str(text);
            }
            Value::String(raw) => {
                let text = match peer {
                    Some(Value::String(theirs)) if theirs != raw && scalars_equal(value, peer.unwrap(), self.options) => {
                        self.normalized = true;
                        theirs
                    }
                    _ => raw,
                };
                self.line.push('"');
                self.line.push_str(text);
                self.line.push('"');
            }
            Value::Array(items) if items.is_empty() => self.line.push_str("[]"),
            Value::Object(members) if members.is_empty() => self.line.push_str("{}"),
            Value::Array(items) => {
                let peer_items = match peer {
                    Some(Value::Array(items)) => Some(items.as_slice()),
                    _ => None,
                };
                let peers = peer_items.map_or(Peers::None, |p| element_peers(p, items, self.options));
                let order = match self.options.array_order {
                    ArrayOrder::Unordered => peers.follow_order(),
                    ArrayOrder::Ordered => None,
                };
                self.normalized |= order.is_some();
                self.line.push('[');
                for position in 0..items.len() {
                    let index = order.as_ref().map_or(position, |order| order[position] as usize);
                    self.next_child(position, depth + 1);
                    let peer = peer_items.and_then(|p| peers.of(index).map(|at| &p[at]));
                    self.emit(&items[index], peer, depth + 1);
                }
                self.break_line(depth);
                self.line.push(']');
            }
            Value::Object(members) => {
                let peer_members = match peer {
                    Some(Value::Object(members)) => Some(members.as_slice()),
                    _ => None,
                };
                let peers = peer_members.map_or(Peers::None, |p| member_peers(p, members));
                let order = match self.options.key_order {
                    KeyOrder::AsWritten => None,
                    KeyOrder::FollowReference => peers.follow_order(),
                    KeyOrder::Alphabetical => alphabetical_order(members),
                };
                self.normalized |= order.is_some();
                self.line.push('{');
                for position in 0..members.len() {
                    let index = order.as_ref().map_or(position, |order| order[position] as usize);
                    self.next_child(position, depth + 1);
                    let member = &members[index];
                    let peer = peer_members.and_then(|p| peers.of(index).map(|at| &p[at]));
                    // Paired keys have the same content: only their escapes can differ.
                    let key = match peer {
                        Some(theirs) if theirs.key != member.key && self.options.strings_by_value => {
                            self.normalized = true;
                            &theirs.key
                        }
                        _ => &member.key,
                    };
                    self.line.push('"');
                    self.line.push_str(key);
                    self.line.push('"');
                    self.line.push_str(self.colon);
                    self.emit(&member.value, peer.map(|theirs| &theirs.value), depth + 1);
                }
                self.break_line(depth);
                self.line.push('}');
            }
        }
    }
}

/// Member indices sorted by key content, duplicates in source order; `None` if sorted already.
fn alphabetical_order(members: &[Member]) -> Option<Vec<u32>> {
    let keys: Vec<Cow<'_, str>> = members.iter().map(|member| canonical_string(&member.key)).collect();
    if keys.is_sorted() {
        return None;
    }
    let mut order: Vec<u32> = (0..members.len() as u32).collect();
    order.sort_by(|&a, &b| keys[a as usize].cmp(&keys[b as usize]));
    Some(order)
}

// ---------------------------------------------------------------------------------------------
// Structural summary
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Present on the right only.
    Added,
    /// Present on the left only.
    Removed,
    /// A scalar with another value, or a value of another type.
    Changed,
    /// An element of an ordered array, recognised by its identity key at another rank. What
    /// changed inside it is reported separately.
    Moved,
}

/// One difference of value between the two documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    /// Where it stands on the left, like `$.servers[3].name`; `None` for an addition.
    pub left_path: Option<String>,
    /// Where it stands on the right; `None` for a removal.
    pub right_path: Option<String>,
    /// The left value on one line, shortened; `None` for an addition or a move.
    pub left: Option<String>,
    /// The right value on one line, shortened; `None` for a removal or a move.
    pub right: Option<String>,
}

impl Change {
    /// The path to show: the left one, or the right one for an addition.
    pub fn path(&self) -> &str {
        self.left_path.as_deref().or(self.right_path.as_deref()).unwrap_or("$")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// In left document order, the additions of a container after its other changes.
    pub changes: Vec<Change>,
    /// The limit was reached: there are more changes than listed.
    pub truncated: bool,
}

/// Lists the differences of value, by path. Key order never counts, whatever
/// `options.key_order` says about the display; number and string spellings count as `options`
/// say; element order counts unless arrays are unordered.
pub fn summarize(left: &JsonDoc, right: &JsonDoc, options: &JsonOptions) -> Summary {
    summarize_up_to(left, right, options, MAX_SUMMARY_CHANGES)
}

/// Like [`summarize`], stopping after `limit` changes.
pub fn summarize_up_to(left: &JsonDoc, right: &JsonDoc, options: &JsonOptions, limit: usize) -> Summary {
    let mut walker = Summarizer {
        options,
        limit,
        summary: Summary::default(),
        left: "$".to_owned(),
        right: "$".to_owned(),
    };
    walker.walk(&left.root, &right.root);
    walker.summary
}

/// Whether both documents hold the same value under `options`.
pub fn equal(left: &JsonDoc, right: &JsonDoc, options: &JsonOptions) -> bool {
    let summary = summarize_up_to(left, right, options, 1);
    summary.changes.is_empty() && !summary.truncated
}

struct Summarizer<'o> {
    options: &'o JsonOptions,
    limit: usize,
    summary: Summary,
    left: String,
    right: String,
}

impl Summarizer<'_> {
    fn report(&mut self, kind: ChangeKind, left: Option<&Value>, right: Option<&Value>) {
        if self.summary.changes.len() >= self.limit {
            self.summary.truncated = true;
            return;
        }
        self.summary.changes.push(Change {
            kind,
            left_path: (kind != ChangeKind::Added).then(|| self.left.clone()),
            right_path: (kind != ChangeKind::Removed).then(|| self.right.clone()),
            left: left.map(preview),
            right: right.map(preview),
        });
    }

    fn walk(&mut self, left: &Value, right: &Value) {
        match (left, right) {
            (Value::Object(a), Value::Object(b)) => {
                let peers = member_peers(a, b);
                let facing = peers.inverse(a.len());
                self.children(
                    a.len(),
                    b.len(),
                    &facing,
                    &peers,
                    None,
                    |path, at| push_key(path, &a[at].key),
                    |path, at| push_key(path, &b[at].key),
                    |at| &a[at].value,
                    |at| &b[at].value,
                );
            }
            (Value::Array(a), Value::Array(b)) => {
                let peers = element_peers(a, b, self.options);
                let facing = peers.inverse(a.len());
                let moved = match (&peers, self.options.array_order) {
                    (Peers::Map(map), ArrayOrder::Ordered) => outside_longest_run(map),
                    _ => None,
                };
                let index = |path: &mut String, at: usize| {
                    let _ = write!(path, "[{at}]");
                };
                self.children(a.len(), b.len(), &facing, &peers, moved.as_deref(), index, index, |at| &a[at], |at| &b[at]);
            }
            _ if scalars_equal(left, right, self.options) => {}
            _ => self.report(ChangeKind::Changed, Some(left), Some(right)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn children<'v>(
        &mut self,
        left_len: usize,
        right_len: usize,
        facing: &[u32],
        peers: &Peers,
        moved: Option<&[bool]>,
        left_segment: impl Fn(&mut String, usize),
        right_segment: impl Fn(&mut String, usize),
        left_child: impl Fn(usize) -> &'v Value,
        right_child: impl Fn(usize) -> &'v Value,
    ) {
        let (left_base, right_base) = (self.left.len(), self.right.len());
        for at in 0..left_len {
            if self.summary.truncated {
                return;
            }
            left_segment(&mut self.left, at);
            match facing[at] {
                NONE => self.report(ChangeKind::Removed, Some(left_child(at)), None),
                other => {
                    let other = other as usize;
                    right_segment(&mut self.right, other);
                    if moved.is_some_and(|moved| moved[other]) {
                        self.report(ChangeKind::Moved, None, None);
                    }
                    self.walk(left_child(at), right_child(other));
                    self.right.truncate(right_base);
                }
            }
            self.left.truncate(left_base);
        }
        for at in (0..right_len).filter(|&at| peers.of(at).is_none()) {
            if self.summary.truncated {
                return;
            }
            right_segment(&mut self.right, at);
            self.report(ChangeKind::Added, None, Some(right_child(at)));
            self.right.truncate(right_base);
        }
    }
}

/// Given, for each right element, the rank of its left counterpart: which ones break the
/// longest run in the same order, that is, the fewest elements to call moved. `None` if none.
fn outside_longest_run(map: &[u32]) -> Option<Vec<bool>> {
    let paired = || map.iter().filter(|&&peer| peer != NONE);
    if paired().is_sorted() {
        return None;
    }
    // Patience sorting: `tails[n]` is the element ending the best run of length n + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![usize::MAX; map.len()];
    for (at, &peer) in map.iter().enumerate().filter(|(_, peer)| **peer != NONE) {
        let rank = tails.partition_point(|&tail| map[tail] < peer);
        if rank > 0 {
            previous[at] = tails[rank - 1];
        }
        if rank == tails.len() {
            tails.push(at);
        } else {
            tails[rank] = at;
        }
    }
    let mut outside: Vec<bool> = map.iter().map(|&peer| peer != NONE).collect();
    let mut at = tails.last().copied().unwrap_or(usize::MAX);
    while at != usize::MAX {
        outside[at] = false;
        at = previous[at];
    }
    Some(outside)
}

/// A value on one line, cut short: enough to recognise it in a list of changes.
fn preview(value: &Value) -> String {
    fn write(out: &mut String, value: &Value) {
        if out.len() > PREVIEW_CHARS * 4 {
            return;
        }
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Value::Number(raw) => out.push_str(raw),
            Value::String(raw) => {
                out.push('"');
                out.push_str(raw);
                out.push('"');
            }
            Value::Array(items) => {
                out.push('[');
                for (at, item) in items.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    write(out, item);
                }
                out.push(']');
            }
            Value::Object(members) => {
                out.push('{');
                for (at, member) in members.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "\"{}\": ", member.key);
                    write(out, &member.value);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    write(&mut out, value);
    if let Some((cut, _)) = out.char_indices().nth(PREVIEW_CHARS) {
        out.truncate(cut);
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

/// Appends `.key`, or `["key"]` when the key would not read as a name.
fn push_key(path: &mut String, raw: &str) {
    let key = canonical_string(raw);
    let mut chars = key.chars();
    let plain = chars.next().is_some_and(|first| first == '_' || first.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric());
    if plain {
        path.push('.');
        path.push_str(&key);
    } else {
        let _ = write!(path, "[\"{key}\"]");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Root,
    /// Canonical key content, as [`canonical_string`] writes it.
    Key(Box<str>),
    Index(u32),
}

fn parse_path(path: &str) -> Option<Vec<Segment>> {
    let mut rest = path.strip_prefix('$')?;
    let mut segments = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("[\"") {
            let bytes = after.as_bytes();
            let mut end = 0;
            while *bytes.get(end)? != b'"' {
                end += if bytes[end] == b'\\' { 2 } else { 1 };
            }
            segments.push(Segment::Key(after.get(..end)?.into()));
            rest = after[end..].strip_prefix("\"]")?;
        } else if let Some(after) = rest.strip_prefix('[') {
            let (index, after) = after.split_once(']')?;
            segments.push(Segment::Index(index.parse().ok()?));
            rest = after;
        } else {
            let after = rest.strip_prefix('.')?;
            let end = after.find(['.', '[']).unwrap_or(after.len());
            segments.push(Segment::Key(after[..end].into()));
            rest = &after[end..];
        }
    }
    Some(segments)
}

#[derive(Clone, Debug)]
struct PathNode {
    parent: u32,
    segment: Segment,
    line: u32,
}

/// Which JSON path each line of a pane belongs to, for the status bar and for jumping from a
/// [`Change`] to its line.
///
/// Built by a scan that never fails: a pane being edited is often not valid JSON, and its lines
/// still deserve a path. Paths are stored as a tree, one small node per member or element.
#[derive(Clone, Debug)]
pub struct PathMap {
    nodes: Vec<PathNode>,
    lines: Vec<u32>,
}

struct Frame {
    node: u32,
    is_object: bool,
    next_index: u32,
    wants_key: bool,
    member: u32,
}

impl PathMap {
    /// Scans the text of a pane, projected or edited by hand, whatever its layout.
    pub fn scan<L: AsRef<str>>(lines: &[L], format: Format) -> PathMap {
        let mut nodes = vec![PathNode {
            parent: NONE,
            segment: Segment::Root,
            line: 0,
        }];
        let mut line_nodes = Vec::with_capacity(lines.len());
        let mut stack: Vec<Frame> = Vec::new();
        let (mut records, mut current) = (0u32, 0u32);
        for (number, line) in lines.iter().enumerate() {
            let line = line.as_ref();
            let number = number as u32;
            let mut add = |parent: u32, segment: Segment| {
                nodes.push(PathNode {
                    parent,
                    segment,
                    line: number,
                });
                nodes.len() as u32 - 1
            };
            // The node of a value about to start: the member awaiting it, or the next element.
            let mut value_node = |stack: &mut Vec<Frame>, add: &mut dyn FnMut(u32, Segment) -> u32| match stack.last_mut() {
                None if format == Format::JsonLines => {
                    records += 1;
                    add(0, Segment::Index(records - 1))
                }
                None => 0,
                Some(frame) if frame.is_object => {
                    let member = std::mem::replace(&mut frame.member, NONE);
                    if member == NONE { frame.node } else { member }
                }
                Some(frame) => {
                    frame.next_index += 1;
                    add(frame.node, Segment::Index(frame.next_index - 1))
                }
            };
            let mut first = None;
            for (token, range) in Lexer::new(line) {
                let node = match token {
                    Token::String if stack.last().is_some_and(|frame| frame.wants_key) => {
                        let frame = stack.last_mut().unwrap();
                        let inner = &line[range.start + 1..range.end];
                        let key = canonical_string(inner.strip_suffix('"').unwrap_or(inner));
                        frame.wants_key = false;
                        frame.member = add(frame.node, Segment::Key(key.into()));
                        frame.member
                    }
                    Token::String | Token::Scalar => value_node(&mut stack, &mut add),
                    Token::Open(is_object) => {
                        let node = value_node(&mut stack, &mut add);
                        stack.push(Frame {
                            node,
                            is_object,
                            next_index: 0,
                            wants_key: is_object,
                            member: NONE,
                        });
                        node
                    }
                    Token::Close => stack.pop().map_or(current, |frame| frame.node),
                    Token::Comma => {
                        if let Some(frame) = stack.last_mut() {
                            frame.wants_key = frame.is_object;
                            frame.member = NONE;
                        }
                        current
                    }
                    Token::Colon => current,
                };
                current = node;
                first.get_or_insert(node);
            }
            line_nodes.push(first.unwrap_or(current));
        }
        nodes[0].line = line_nodes.iter().position(|&node| node == 0).unwrap_or(0) as u32;
        PathMap {
            nodes,
            lines: line_nodes,
        }
    }

    /// The path of the member, element or bracket that starts `line` (0-based), like
    /// `$.servers[3].name`. A blank line takes the path of what precedes it.
    pub fn path_of_line(&self, line: usize) -> Option<String> {
        let mut segments = Vec::new();
        let mut node = *self.lines.get(line)?;
        while node != NONE {
            segments.push(&self.nodes[node as usize].segment);
            node = self.nodes[node as usize].parent;
        }
        let mut path = String::new();
        for segment in segments.into_iter().rev() {
            match segment {
                Segment::Root => path.push('$'),
                Segment::Key(key) => push_key(&mut path, key),
                Segment::Index(index) => {
                    let _ = write!(path, "[{index}]");
                }
            }
        }
        Some(path)
    }

    /// The line (0-based) where the value at `path` starts; with duplicate keys, the first.
    pub fn line_of_path(&self, path: &str) -> Option<usize> {
        let segments = parse_path(path)?;
        if segments.is_empty() {
            return Some(self.nodes[0].line as usize);
        }
        // Parents come before their children: one pass tells how much of the path each matches.
        let mut matched = vec![NONE; self.nodes.len()];
        matched[0] = 0;
        for (at, node) in self.nodes.iter().enumerate().skip(1) {
            let depth = matched[node.parent as usize];
            if depth != NONE && segments.get(depth as usize) == Some(&node.segment) {
                if depth as usize + 1 == segments.len() {
                    return Some(node.line as usize);
                }
                matched[at] = depth + 1;
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Working on the text of a pane
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    /// `{` (true) or `[`.
    Open(bool),
    Close,
    Comma,
    Colon,
    String,
    /// A number, a literal, or anything else that is not punctuation.
    Scalar,
}

/// Cuts one line into JSON tokens without judging it: text being typed must not stop it.
struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(line: &'a str) -> Self {
        Self {
            bytes: line.as_bytes(),
            pos: 0,
        }
    }
}

impl Iterator for Lexer<'_> {
    type Item = (Token, Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(b' ' | b'\t' | b'\r' | b'\n') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
        let start = self.pos;
        let token = match *self.bytes.get(start)? {
            b'{' => Token::Open(true),
            b'[' => Token::Open(false),
            b'}' | b']' => Token::Close,
            b',' => Token::Comma,
            b':' => Token::Colon,
            b'"' => {
                self.pos += 1;
                loop {
                    match self.bytes.get(self.pos) {
                        Some(b'"') | None => break,
                        Some(b'\\') => self.pos += 2,
                        Some(_) => self.pos += 1,
                    }
                }
                // A multi-byte character after a backslash: finish it, slices must stay valid.
                while self.bytes.get(self.pos).is_some_and(|&byte| byte & 0xC0 == 0x80) {
                    self.pos += 1;
                }
                self.pos = self.pos.min(self.bytes.len().saturating_sub(1));
                Token::String
            }
            _ => {
                while let Some(byte) = self.bytes.get(self.pos + 1) {
                    if matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'[' | b'}' | b']' | b',' | b':' | b'"') {
                        break;
                    }
                    self.pos += 1;
                }
                Token::Scalar
            }
        };
        self.pos += 1;
        Some((token, start..self.pos))
    }
}

/// Puts back the commas a block transfer broke: one is added between two values that follow
/// each other, and removed before a closing bracket, after an opening one or after another
/// comma. Returns the lines to replace, as `(line index, new text)`, in increasing order;
/// nothing for a text whose commas are right.
///
/// Only commas are touched, so this can neither change a value nor fix other damage.
pub fn repair_commas<L: AsRef<str>>(lines: &[L], format: Format) -> Vec<(usize, String)> {
    #[derive(PartialEq)]
    enum Previous {
        Nothing,
        Opening,
        Value,
        Colon,
        Comma,
    }
    // (line, byte offset, insert a comma there rather than remove the one there)
    let mut fixes: Vec<(usize, usize, bool)> = Vec::new();
    let mut previous = (Previous::Nothing, 0usize, 0usize);
    let mut depth = 0usize;
    for (number, line) in lines.iter().enumerate() {
        if format == Format::JsonLines {
            previous = (Previous::Nothing, 0, 0);
            depth = 0;
        }
        for (token, range) in Lexer::new(line.as_ref()) {
            let kind = match token {
                Token::String | Token::Scalar | Token::Open(_) => {
                    if previous.0 == Previous::Value && depth > 0 {
                        fixes.push((previous.1, previous.2, true));
                    }
                    if let Token::Open(_) = token {
                        depth += 1;
                        Previous::Opening
                    } else {
                        Previous::Value
                    }
                }
                Token::Close => {
                    if previous.0 == Previous::Comma {
                        fixes.push((previous.1, previous.2, false));
                    }
                    depth = depth.saturating_sub(1);
                    Previous::Value
                }
                Token::Comma => {
                    if matches!(previous.0, Previous::Comma | Previous::Opening | Previous::Nothing) {
                        fixes.push((number, range.start, false));
                        continue;
                    }
                    Previous::Comma
                }
                Token::Colon => Previous::Colon,
            };
            // An insertion goes after the token, a removal targets the comma itself.
            let offset = if kind == Previous::Comma { range.start } else { range.end };
            previous = (kind, number, offset);
        }
    }
    fixes.sort_unstable();
    let mut repaired: Vec<(usize, String)> = Vec::new();
    for &(number, offset, insert) in fixes.iter().rev() {
        if repaired.last().is_none_or(|(last, _)| *last != number) {
            repaired.push((number, lines[number].as_ref().to_owned()));
        }
        let text = &mut repaired.last_mut().unwrap().1;
        if insert {
            text.insert(offset, ',');
        } else {
            text.remove(offset);
        }
    }
    repaired.reverse();
    repaired
}

/// What of a line counts for the JSON comparison: neither its indentation nor the comma that
/// ends it, both dictated by the neighbours. Returns it with its byte offset in the line.
pub fn significant_part(line: &str) -> (&str, usize) {
    let content = line.trim_start_matches([' ', '\t']);
    let offset = line.len() - content.len();
    let trimmed = content.trim_end();
    (trimmed.strip_suffix(',').unwrap_or(content), offset)
}

/// [`diff_lines`] for two JSON panes: lines are compared without their indentation and without
/// the comma that ends them. Each side can so keep its own indentation, and adding a last
/// member does not mark the line before it, which merely gained a comma.
///
/// Rows, blocks and spans refer to the full lines given, exactly as [`diff_lines`] would.
pub fn diff_json_lines<L: AsRef<str> + Sync>(left: &[L], right: &[L], options: DiffOptions) -> DiffResult {
    diff_json_lines_with(left, right, options, &IgnoreRules::default())
}

/// [`diff_json_lines`] under ignore rules, as [`diff_lines_with`] applies them. Rules see the
/// significant part of each line, so a whole-line rule anchored with `^` matches after the
/// indentation.
pub fn diff_json_lines_with<L: AsRef<str> + Sync>(
    left: &[L],
    right: &[L],
    options: DiffOptions,
    rules: &IgnoreRules,
) -> DiffResult {
    fn parts<L: AsRef<str>>(lines: &[L]) -> (Vec<&str>, Vec<u32>) {
        lines
            .iter()
            .map(|line| {
                let (part, offset) = significant_part(line.as_ref());
                (part, offset as u32)
            })
            .unzip()
    }
    let (left_parts, left_offsets) = parts(left);
    let (right_parts, right_offsets) = parts(right);
    let mut result = diff_lines_with(&left_parts, &right_parts, options, rules);
    // Spans were measured in the trimmed lines: move them back into the real ones.
    for row in &result.rows {
        let (Some(pair), Some(l), Some(r)) = (row.pair, row.left, row.right) else { continue };
        let spans = &mut result.spans[pair as usize];
        for (spans, offset) in [(&mut spans.left, left_offsets[l as usize]), (&mut spans.right, right_offsets[r as usize])] {
            for span in spans.iter_mut() {
                span.range = span.range.start + offset..span.range.end + offset;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{BlockKind, diff_lines};
    use crate::ignore_rules::IgnoreRule;

    fn doc(text: &str) -> JsonDoc {
        JsonDoc::parse(text, Format::Json).unwrap()
    }

    fn error(text: &str) -> JsonError {
        JsonDoc::parse(text, Format::Json).unwrap_err()
    }

    fn projected(left: &str, right: &str, options: &JsonOptions) -> [Vec<String>; 2] {
        project(&doc(left), &doc(right), options).map(|projection| projection.lines)
    }

    fn changes(left: &str, right: &str, options: &JsonOptions) -> Vec<String> {
        summarize(&doc(left), &doc(right), options)
            .changes
            .iter()
            .map(|change| {
                let side = |text: &Option<String>| text.clone().unwrap_or_default();
                format!("{:?} {} {} -> {}", change.kind, change.path(), side(&change.left), side(&change.right))
            })
            .collect()
    }

    // ----- parsing -----

    #[test]
    fn parses_every_kind_of_value() {
        let parsed = doc(r#"{"a": [1, -2.5e3, "x", true, false, null, {}, []]}"#);
        let Value::Object(members) = &parsed.root else { panic!() };
        assert_eq!(&*members[0].key, "a");
        let Value::Array(items) = &members[0].value else { panic!() };
        assert_eq!(items.len(), 8);
        assert_eq!(items[1], Value::Number("-2.5e3".into()));
        assert_eq!(items[2], Value::String("x".into()));
        assert_eq!(items[6], Value::Object(vec![]));
    }

    #[test]
    fn a_scalar_alone_is_a_document() {
        assert_eq!(doc(" 42 \n").root, Value::Number("42".into()));
        assert_eq!(doc("\"x\"").lines(), ["\"x\""]);
    }

    #[test]
    fn keeps_duplicate_keys_as_separate_members() {
        let parsed = doc(r#"{"a": 1, "a": 2}"#);
        let Value::Object(members) = &parsed.root else { panic!() };
        assert_eq!(members.len(), 2);
        assert_eq!(parsed.lines(), ["{", "  \"a\": 1,", "  \"a\": 2", "}"]);
    }

    #[test]
    fn long_numbers_come_back_digit_for_digit() {
        let text = "[123456789012345678901234567890, 0.1000000000000000000000000001, 1E400, -0.0]";
        let lines = doc(text).lines();
        assert_eq!(lines[1], "  123456789012345678901234567890,");
        assert_eq!(lines[2], "  0.1000000000000000000000000001,");
        assert_eq!(lines[3], "  1E400,");
        assert_eq!(lines[4], "  -0.0");
    }

    #[test]
    fn strings_come_back_with_their_escapes() {
        let text = r#"["\u00e9", "é", "\ud83d\ude00", "\ud800", "a\/b", "tab\t"]"#;
        assert_eq!(doc(text).root.to_compact(), text.replace(", ", ","));
    }

    #[test]
    fn errors_give_line_and_column() {
        let broken = error("{\n  \"a\": 1,\n  \"é\": @\n}");
        assert_eq!((broken.line, broken.column), (3, 8));
        assert_eq!(broken.kind, JsonErrorKind::UnexpectedChar('@'));
        assert_eq!(broken.to_string(), "ligne 3, colonne 8 : caractère inattendu « @ »");
    }

    #[test]
    fn comments_are_refused_as_a_dialect() {
        for text in ["{\n  // note\n  \"a\": 1\n}", "[1, /* two */ 2]", "{} // end"] {
            let refused = error(text);
            assert_eq!(refused.kind, JsonErrorKind::Comment, "{text}");
            assert!(refused.kind.is_dialect());
        }
        assert_eq!(error("{\n  // note\n}").line, 2);
    }

    #[test]
    fn a_trailing_comma_is_refused_where_it_stands() {
        let refused = error("{\n  \"a\": [1, 2,],\n}");
        assert_eq!(refused.kind, JsonErrorKind::TrailingComma);
        assert_eq!((refused.line, refused.column), (2, 13));
        assert_eq!(error("{\"a\": 1,\n}").kind, JsonErrorKind::TrailingComma);
    }

    #[test]
    fn malformed_texts_are_refused() {
        use JsonErrorKind::*;
        let cases = [
            ("", Empty),
            ("  \n", Empty),
            ("[1, 2", UnexpectedEnd),
            ("{\"a\"}", UnexpectedChar('}')),
            ("{a: 1}", UnexpectedChar('a')),
            ("['a']", UnexpectedChar('\'')),
            ("[01]", InvalidNumber),
            ("[1.]", InvalidNumber),
            ("[-]", InvalidNumber),
            ("[1e]", InvalidNumber),
            ("[0x1F]", InvalidNumber),
            ("[+1]", UnexpectedChar('+')),
            ("[NaN]", UnexpectedChar('N')),
            ("[\"a\\x\"]", InvalidEscape),
            ("[\"\\u12G4\"]", InvalidEscape),
            ("[\"a\tb\"]", ControlInString),
            ("[\"abc]", UnterminatedString),
            ("[\"abc\n\"]", UnterminatedString),
            ("{} {}", TrailingData),
            ("[1] x", TrailingData),
            ("[,]", UnexpectedChar(',')),
            ("nul", UnexpectedChar('n')),
        ];
        for (text, kind) in cases {
            assert_eq!(error(text).kind, kind, "{text:?}");
        }
    }

    #[test]
    fn nesting_beyond_the_limit_is_an_error_not_a_crash() {
        let deep = |levels: usize| format!("{}{}", "[".repeat(levels), "]".repeat(levels));
        assert!(JsonDoc::parse(&deep(MAX_DEPTH), Format::Json).is_ok());
        assert_eq!(error(&deep(MAX_DEPTH + 1)).kind, JsonErrorKind::TooDeep);
        assert_eq!(error(&"[".repeat(100_000)).kind, JsonErrorKind::TooDeep);
    }

    #[test]
    fn a_byte_order_mark_is_accepted() {
        assert_eq!(doc("\u{feff}{\"a\": 1}").lines(), ["{", "  \"a\": 1", "}"]);
    }

    // ----- layout -----

    #[test]
    fn detects_indentation_and_final_newline() {
        assert_eq!(doc("{\n  \"a\": 1\n}\n").layout.indent, Indent::Spaces(2));
        assert_eq!(doc("{\n    \"a\": 1\n}").layout.indent, Indent::Spaces(4));
        assert_eq!(doc("{\n\t\"a\": {\n\t\t\"b\": 1\n\t}\n}").layout.indent, Indent::Tab);
        assert_eq!(doc("{\"a\": 1}").layout.indent, Indent::Spaces(2));
        assert!(doc("{}\n").layout.final_newline);
        assert!(doc("{}\r\n").layout.final_newline);
        assert!(!doc("{}").layout.final_newline);
    }

    #[test]
    fn a_pretty_printed_file_projects_to_itself() {
        for text in [
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    {}\n  ],\n  \"c\": []\n}\n",
            "{\n\t\"a\": {\n\t\t\"b\": null\n\t}\n}",
            "{\r\n    \"a\": 1\r\n}\r\n",
            "[]",
            "3.0\n",
        ] {
            let parsed = doc(text);
            assert!(parsed.has_canonical_layout(), "{text:?}");
            let [projection, _] = project(&parsed, &parsed, &JsonOptions::default());
            assert!(!projection.alters_file());
            let eol = if text.contains('\r') { "\r\n" } else { "\n" };
            assert_eq!(projection.text(eol), text);
        }
    }

    #[test]
    fn another_layout_is_reported_as_reformatted() {
        for text in ["{\"a\":1}", "{\n  \"a\": [1, 2]\n}", "{\n  \"a\" : 1\n}", "{\n  \"a\": 1\n\n}", "{ }"] {
            let parsed = doc(text);
            assert!(!parsed.has_canonical_layout(), "{text:?}");
            let [projection, _] = project(&parsed, &parsed, &JsonOptions::default());
            assert!(projection.reformatted && !projection.normalized);
        }
    }

    #[test]
    fn each_side_keeps_its_own_indentation() {
        let [left, right] = projected("{\n\t\"a\": [1]\n}", "{\n    \"a\": [1]\n}", &JsonOptions::default());
        assert_eq!(left, ["{", "\t\"a\": [", "\t\t1", "\t]", "}"]);
        assert_eq!(right, ["{", "    \"a\": [", "        1", "    ]", "}"]);
    }

    // ----- equality by value -----

    #[test]
    fn numbers_of_equal_value_share_one_canonical_spelling() {
        let cases = [
            ("1", "1"),
            ("1.0", "1"),
            ("1e0", "1"),
            ("10E-1", "1"),
            ("1e3", "1000"),
            ("1.5E+3", "1500"),
            ("-12.50", "-12.5"),
            ("0.000001", "0.000001"),
            ("1e-6", "0.000001"),
            ("1e-7", "1e-7"),
            ("0.00000012", "1.2e-7"),
            ("123456789012345678901234567890.0", "123456789012345678901234567890"),
            ("1e21", "1000000000000000000000"),
            ("1e22", "1e22"),
            ("12e21", "12000000000000000000000"),
            ("10000000000000000000000", "1e22"),
            ("1200000000000000000000000.0", "1.2e24"),
            ("120000000000000000000000", "1.2e23"),
            ("-5E+30", "-5e30"),
            ("0.0", "0"),
            ("0e99999999999999999999", "0"),
            ("-0.0", "-0"),
            ("-0", "-0"),
            ("100", "100"),
            ("1.10", "1.1"),
            ("0.1000000000000000000000000001", "0.1000000000000000000000000001"),
            ("1E99999999999999999999", "1e99999999999999999999"),
        ];
        for (raw, canonical) in cases {
            assert_eq!(canonical_number(raw), canonical, "{raw}");
        }
    }

    #[test]
    fn close_numbers_are_never_confused() {
        // Both pairs are equal once read as f64.
        assert_ne!(canonical_number("0.1"), canonical_number("0.1000000000000000055511151231257827"));
        assert_ne!(canonical_number("9007199254740993"), canonical_number("9007199254740992.0"));
        assert_ne!(canonical_number("0"), canonical_number("-0.0"));
    }

    #[test]
    fn strings_of_equal_content_share_one_canonical_spelling() {
        let cases = [
            ("plain", "plain"),
            (r"\u00e9", "é"),
            (r"\u00E9t\u00e9", "été"),
            (r"a\/b", "a/b"),
            (r"\ud83d\ude00", "😀"),
            (r"\uD800", r"\ud800"),
            (r"\ud83dx", r"\ud83dx"),
            (r"\ud83d\u0041", r"\ud83dA"),
            (r"\u000A\u0009", r"\n\t"),
            (r"\u0001", r"\u0001"),
            (r"\u0022\u005c", r#"\"\\"#),
            (r#"\"\\\n"#, r#"\"\\\n"#),
            (r"\b\f\r", r"\b\f\r"),
        ];
        for (raw, canonical) in cases {
            assert_eq!(canonical_string(raw), canonical, "{raw}");
        }
    }

    #[test]
    fn canonical_string_survives_malformed_escapes() {
        for raw in ["\\", "a\\", "\\u12", "\\é", "\\ud83d\\u", "\\x41", "\\u00zz"] {
            let _ = canonical_string(raw);
        }
        assert_eq!(canonical_string("\\é!"), "\\é!");
    }

    // ----- projection: keys -----

    #[test]
    fn the_follower_takes_the_key_order_of_the_reference() {
        let [left, right] = projected(r#"{"a": 1, "b": 2, "c": 3}"#, r#"{"c": 3, "b": 2, "a": 1}"#, &JsonOptions::default());
        assert_eq!(left, ["{", "  \"a\": 1,", "  \"b\": 2,", "  \"c\": 3", "}"]);
        assert_eq!(right, left);
    }

    #[test]
    fn keys_the_reference_lacks_stay_behind_their_predecessor() {
        let [_, right] = projected(
            r#"{"a": 1, "b": 2, "c": 3}"#,
            r#"{"first": 0, "c": 3, "x": 9, "a": 1, "y": 8}"#,
            &JsonOptions::default(),
        );
        assert_eq!(right, ["{", "  \"first\": 0,", "  \"a\": 1,", "  \"y\": 8,", "  \"c\": 3,", "  \"x\": 9", "}"]);
    }

    #[test]
    fn key_order_is_followed_at_every_depth() {
        let [left, right] = projected(
            r#"{"o": {"a": 1, "b": {"x": 1, "y": 2}}, "l": [{"p": 1, "q": 2}]}"#,
            r#"{"l": [{"q": 2, "p": 1}], "o": {"b": {"y": 2, "x": 1}, "a": 1}}"#,
            &JsonOptions::default(),
        );
        assert_eq!(left, right);
    }

    #[test]
    fn the_right_side_can_be_the_reference() {
        let options = JsonOptions {
            reference: Side::Right,
            ..JsonOptions::default()
        };
        let [left, right] = project(&doc(r#"{"a": 1, "b": 2}"#), &doc(r#"{"b": 2, "a": 1}"#), &options);
        assert_eq!(left.lines, ["{", "  \"b\": 2,", "  \"a\": 1", "}"]);
        assert_eq!(left.lines, right.lines);
        assert!(left.normalized && !right.normalized);
    }

    #[test]
    fn keys_as_written_are_left_alone() {
        let options = JsonOptions {
            key_order: KeyOrder::AsWritten,
            ..JsonOptions::default()
        };
        let [left, right] = project(&doc(r#"{"a": 1, "b": 2}"#), &doc(r#"{"b": 2, "a": 1}"#), &options);
        assert_eq!(right.lines, ["{", "  \"b\": 2,", "  \"a\": 1", "}"]);
        assert!(!left.normalized && !right.normalized);
    }

    #[test]
    fn alphabetical_order_sorts_both_sides() {
        let options = JsonOptions {
            key_order: KeyOrder::Alphabetical,
            ..JsonOptions::default()
        };
        let [left, right] = project(&doc(r#"{"b": 1, "a": {"z": 1, "y": 2}, "é": 0}"#), &doc(r#"{"a": {"y": 2, "z": 1}, "b": 1}"#), &options);
        assert_eq!(left.lines, ["{", "  \"a\": {", "    \"y\": 2,", "    \"z\": 1", "  },", "  \"b\": 1,", "  \"é\": 0", "}"]);
        assert_eq!(right.lines[..5], left.lines[..5]);
        assert!(left.normalized && !right.normalized);
    }

    #[test]
    fn duplicate_keys_are_paired_in_order_and_all_written() {
        let [left, right] = projected(r#"{"a": 1, "b": 0, "a": 2}"#, r#"{"a": 1, "a": 2, "a": 3, "b": 0}"#, &JsonOptions::default());
        assert_eq!(left, ["{", "  \"a\": 1,", "  \"b\": 0,", "  \"a\": 2", "}"]);
        assert_eq!(right, ["{", "  \"a\": 1,", "  \"b\": 0,", "  \"a\": 2,", "  \"a\": 3", "}"]);
    }

    #[test]
    fn large_objects_are_paired_like_small_ones() {
        let member = |n: usize| format!("\"k{n}\": {n}");
        let left = format!("{{{}}}", (0..300).map(member).collect::<Vec<_>>().join(","));
        let right = format!("{{{}}}", (0..300).rev().map(member).collect::<Vec<_>>().join(","));
        let [left, right] = projected(&left, &right, &JsonOptions::default());
        assert_eq!(left, right);
    }

    // ----- projection: spellings -----

    #[test]
    fn an_equal_number_takes_the_spelling_of_the_reference() {
        let [left, right] = project(&doc("[1.0, 2, 1e3]"), &doc("[1, 2.50, 1000]"), &JsonOptions::default());
        assert_eq!(left.lines, ["[", "  1.0,", "  2,", "  1e3", "]"]);
        assert_eq!(right.lines, ["[", "  1.0,", "  2.50,", "  1e3", "]"]);
        assert!(right.normalized && !left.normalized);
    }

    #[test]
    fn an_equal_string_or_key_takes_the_spelling_of_the_reference() {
        let [left, right] = projected(r#"{"\u00e9t\u00e9": "caf\u00e9", "b": "x"}"#, r#"{"b": "y", "été": "café"}"#, &JsonOptions::default());
        assert_eq!(left[1], r#"  "\u00e9t\u00e9": "caf\u00e9","#);
        assert_eq!(right[1], left[1]);
        assert_eq!(right[2], r#"  "b": "y""#);
    }

    #[test]
    fn spellings_are_kept_when_values_are_compared_as_written() {
        let options = JsonOptions {
            numbers_by_value: false,
            strings_by_value: false,
            ..JsonOptions::default()
        };
        let [_, right] = project(&doc(r#"{"a": 1.0, "b": "\u00e9"}"#), &doc(r#"{"a": 1, "b": "é"}"#), &options);
        assert_eq!(right.lines, ["{", "  \"a\": 1,", "  \"b\": \"é\"", "}"]);
        assert!(!right.normalized);
    }

    // ----- projection: arrays -----

    #[test]
    fn ordered_arrays_are_never_reordered() {
        let [_, right] = project(
            &doc(r#"[{"id": 1, "v": "a"}, {"id": 2, "v": "b"}]"#),
            &doc(r#"[{"id": 2, "v": "b"}, {"id": 1, "v": "a"}]"#),
            &JsonOptions::default(),
        );
        assert_eq!(right.lines[2], "    \"id\": 2,");
        assert!(!right.normalized);
    }

    #[test]
    fn identity_keys_pair_elements_whatever_their_rank() {
        // The inserted element shifts ranks; key order and spelling still follow the right peer.
        let [left, right] = projected(
            r#"[{"id": 1, "v": 1.0, "w": 0}, {"id": 2, "v": 2.0, "w": 0}]"#,
            r#"[{"w": 9, "id": 7}, {"w": 0, "v": 1, "id": 1}, {"w": 0, "v": 2, "id": 2}]"#,
            &JsonOptions::default(),
        );
        assert_eq!(right[1..5], ["  {", "    \"w\": 9,", "    \"id\": 7", "  },"]);
        assert_eq!(right[5..10], left[1..6]);
        assert_eq!(right[10..], left[6..]);
    }

    #[test]
    fn elements_without_identity_are_paired_by_content() {
        let [left, right] = projected(
            r#"[{"a": 1, "b": 1.0}, {"a": 2, "b": 2.0}, {"a": 3, "b": 3.0}]"#,
            r#"[{"b": 0, "a": 0}, {"b": 1, "a": 1}, {"b": 2, "a": 2}, {"b": 3, "a": 3}]"#,
            &JsonOptions::default(),
        );
        assert_eq!(right[1..5], ["  {", "    \"b\": 0,", "    \"a\": 0", "  },"]);
        assert_eq!(right[5..], left[1..]);
    }

    #[test]
    fn unordered_arrays_follow_the_reference_by_identity() {
        let options = JsonOptions {
            array_order: ArrayOrder::Unordered,
            ..JsonOptions::default()
        };
        let [left, right] = project(
            &doc(r#"[{"name": "a", "v": 1}, {"name": "b", "v": 2}, {"name": "c", "v": 3}]"#),
            &doc(r#"[{"name": "c", "v": 3}, {"name": "new", "v": 0}, {"name": "a", "v": 10}]"#),
            &options,
        );
        let names = |lines: &[String]| -> Vec<String> { lines.iter().filter(|line| line.contains("name")).cloned().collect() };
        assert_eq!(names(&left.lines), ["    \"name\": \"a\",", "    \"name\": \"b\",", "    \"name\": \"c\","]);
        assert_eq!(names(&right.lines), ["    \"name\": \"a\",", "    \"name\": \"c\",", "    \"name\": \"new\","]);
        assert!(right.normalized && !left.normalized);
    }

    #[test]
    fn unordered_arrays_follow_the_reference_by_value() {
        let options = JsonOptions {
            array_order: ArrayOrder::Unordered,
            ..JsonOptions::default()
        };
        let [left, right] = projected(r#"["x", "y", 1, "x", [1, 2]]"#, r#"[[2, 1], "z", "x", 1.0, "x", "y"]"#, &options);
        assert_eq!(left, ["[", "  \"x\",", "  \"y\",", "  1,", "  \"x\",", "  [", "    1,", "    2", "  ]", "]"]);
        assert_eq!(right, ["[", "  \"x\",", "  \"y\",", "  1,", "  \"x\",", "  [", "    1,", "    2", "  ],", "  \"z\"", "]"]);
    }

    #[test]
    fn projecting_never_changes_a_value() {
        let texts = [
            r#"{"b": [3, 1.0, {"id": 2, "k": "\u00e9", "z": null}, {"id": 1, "k": "x"}], "a": 1e2, "a": true}"#,
            r#"{"a": 100, "b": [{"k": "x", "id": 1}, 1, 3, {"z": null, "k": "é", "id": 2}], "c": {}}"#,
            r#"[{"name": "n", "v": [1, 2, [3]]}, "s", {"name": "m"}]"#,
            r#"{"a": {"b": {"c": [[], {}, [{}]]}}}"#,
        ];
        let strict = JsonOptions::as_written();
        let unordered = JsonOptions {
            array_order: ArrayOrder::Unordered,
            ..JsonOptions::as_written()
        };
        for key_order in [KeyOrder::AsWritten, KeyOrder::FollowReference, KeyOrder::Alphabetical] {
            for array_order in [ArrayOrder::Ordered, ArrayOrder::Unordered] {
                for by_value in [false, true] {
                    let options = JsonOptions {
                        key_order,
                        array_order,
                        numbers_by_value: by_value,
                        strings_by_value: by_value,
                        ..JsonOptions::default()
                    };
                    for left in texts {
                        for right in texts {
                            let sources = [doc(left), doc(right)];
                            let projections = project(&sources[0], &sources[1], &options);
                            for (source, projection) in sources.iter().zip(&projections) {
                                let back = JsonDoc::parse_lines(&projection.lines, Format::Json).unwrap();
                                // What may differ from the file is exactly what the options allow.
                                let allowed = JsonOptions {
                                    array_order,
                                    numbers_by_value: by_value,
                                    strings_by_value: by_value,
                                    ..JsonOptions::as_written()
                                };
                                assert!(equal(source, &back, &allowed), "{options:?}\n{left}\n{right}");
                                if !by_value {
                                    let exact = if array_order == ArrayOrder::Ordered { &strict } else { &unordered };
                                    assert!(equal(source, &back, exact));
                                }
                                assert!(back.has_canonical_layout());
                                if !projection.normalized {
                                    assert_eq!(back.root, source.root);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ----- JSON Lines -----

    #[test]
    fn json_lines_keep_one_record_per_line() {
        let left = JsonDoc::parse("{\"id\": 1, \"a\": [1, 2]}\n{\"id\": 2, \"a\": []}\n", Format::JsonLines).unwrap();
        let right = JsonDoc::parse("{\"a\":[1.0,2],\"id\":1}\n\n{\"a\":[],\"id\":3}", Format::JsonLines).unwrap();
        assert!(left.has_canonical_layout() && !right.has_canonical_layout());
        let [left, right] = project(&left, &right, &JsonOptions::default());
        assert_eq!(left.lines, ["{\"id\": 1, \"a\": [1, 2]}", "{\"id\": 2, \"a\": []}"]);
        assert_eq!(right.lines, ["{\"id\":1,\"a\":[1,2]}", "{\"a\":[],\"id\":3}"]);
        assert!(!left.alters_file() && right.reformatted && right.normalized);
        assert!(!right.final_newline);
    }

    #[test]
    fn json_lines_errors_name_the_record_line() {
        let broken = JsonDoc::parse("{\"a\": 1}\n\n{\"a\": }\n", Format::JsonLines).unwrap_err();
        assert_eq!((broken.line, broken.column), (3, 7));
        let two = JsonDoc::parse("{} {}\n", Format::JsonLines).unwrap_err();
        assert_eq!(two.kind, JsonErrorKind::TrailingData);
        assert_eq!(JsonDoc::parse("", Format::JsonLines).unwrap().root, Value::Array(vec![]));
    }

    #[test]
    fn extensions_select_the_format() {
        assert_eq!(Format::from_path(Path::new("a/b.JSON")), Some(Format::Json));
        assert_eq!(Format::from_path(Path::new("log.ndjson")), Some(Format::JsonLines));
        assert_eq!(Format::from_path(Path::new("tsconfig.jsonc")), None);
        assert_eq!(Format::from_path(Path::new("json")), None);
    }

    // ----- summary -----

    #[test]
    fn summary_lists_changes_by_path() {
        let found = changes(
            r#"{"a": 1, "b": {"c": [1, 2, 3], "old": true}, "we ird": "x"}"#,
            r#"{"b": {"c": [1, 3], "new": null}, "a": 2, "we ird": {"k": 1}}"#,
            &JsonOptions::default(),
        );
        assert_eq!(
            found,
            [
                "Changed $.a 1 -> 2",
                "Removed $.b.c[1] 2 -> ",
                "Removed $.b.old true -> ",
                "Added $.b.new  -> null",
                "Changed $[\"we ird\"] \"x\" -> {\"k\": 1}",
            ]
        );
    }

    #[test]
    fn key_order_and_spellings_are_not_changes() {
        let left = doc(r#"{"a": 1.0, "b": "\u00e9", "c": [1e2]}"#);
        let right = doc(r#"{"c": [100], "b": "é", "a": 1}"#);
        assert!(equal(&left, &right, &JsonOptions::default()));
        let exact = JsonOptions {
            numbers_by_value: false,
            ..JsonOptions::default()
        };
        assert!(!equal(&left, &right, &exact));
        assert_eq!(changes(r#"{"a": 1.0}"#, r#"{"a": 1}"#, &exact), ["Changed $.a 1.0 -> 1"]);
    }

    #[test]
    fn an_element_at_another_rank_is_reported_as_moved() {
        let summary = summarize(
            &doc(r#"[{"id": "a", "v": 1}, {"id": "b", "v": 2}, {"id": "c", "v": 3}]"#),
            &doc(r#"[{"id": "c", "v": 30}, {"id": "a", "v": 1}, {"id": "b", "v": 2}]"#),
            &JsonOptions::default(),
        );
        let [moved, changed] = &summary.changes[..] else { panic!("{summary:?}") };
        assert_eq!(moved.kind, ChangeKind::Moved);
        assert_eq!((moved.left_path.as_deref(), moved.right_path.as_deref()), (Some("$[2]"), Some("$[0]")));
        assert_eq!(changed.kind, ChangeKind::Changed);
        assert_eq!((changed.left_path.as_deref(), changed.right_path.as_deref()), (Some("$[2].v"), Some("$[0].v")));
        assert_eq!((changed.left.as_deref(), changed.right.as_deref()), (Some("3"), Some("30")));
    }

    #[test]
    fn unordered_arrays_are_equal_whatever_their_order() {
        let options = JsonOptions {
            array_order: ArrayOrder::Unordered,
            ..JsonOptions::default()
        };
        let left = r#"{"tags": ["a", "b", "a"], "l": [{"id": 1, "t": [1, 2]}, {"id": 2}]}"#;
        let right = r#"{"l": [{"id": 2}, {"t": [2, 1], "id": 1}], "tags": ["a", "a", "b"]}"#;
        assert!(equal(&doc(left), &doc(right), &options));
        assert!(!equal(&doc(left), &doc(right), &JsonOptions::default()));
        assert!(!equal(&doc(r#"["a", "a", "b"]"#), &doc(r#"["a", "b", "b"]"#), &options));
        assert_eq!(changes(r#"["a", "b"]"#, r#"["c", "a"]"#, &options), ["Changed $[1] \"b\" -> \"c\""]);
    }

    #[test]
    fn duplicate_keys_count_in_the_summary() {
        assert_eq!(changes(r#"{"a": 1, "a": 2}"#, r#"{"a": 1}"#, &JsonOptions::default()), ["Removed $.a 2 -> "]);
        assert!(equal(&doc(r#"{"a": 1, "b": 0, "a": 2}"#), &doc(r#"{"b": 0, "a": 1, "a": 2}"#), &JsonOptions::default()));
    }

    #[test]
    fn a_long_summary_is_cut_and_says_so() {
        let left = doc(&format!("[{}]", vec!["0"; 50].join(",")));
        let right = doc(&format!("[{}]", vec!["1"; 50].join(",")));
        let summary = summarize_up_to(&left, &right, &JsonOptions::default(), 10);
        assert_eq!(summary.changes.len(), 10);
        assert!(summary.truncated);
        assert!(!summarize(&left, &right, &JsonOptions::default()).truncated);
    }

    #[test]
    fn long_values_are_shortened_in_the_summary() {
        let long = format!("[{}]", vec!["\"éééééééééé\""; 1000].join(","));
        let summary = summarize(&doc("1"), &doc(&long), &JsonOptions::default());
        let shown = summary.changes[0].right.as_deref().unwrap();
        assert_eq!(shown.chars().count(), PREVIEW_CHARS + 1);
        assert!(shown.ends_with('…'));
    }

    // ----- paths -----

    #[test]
    fn every_projected_line_has_its_path() {
        let lines = doc(r#"{"a": 1, "b": {"c": [10, {"d e": null}], "f": []}, "g": "x"}"#).lines();
        let map = PathMap::scan(&lines, Format::Json);
        let paths: Vec<String> = (0..lines.len()).map(|line| map.path_of_line(line).unwrap()).collect();
        assert_eq!(
            paths,
            ["$", "$.a", "$.b", "$.b.c", "$.b.c[0]", "$.b.c[1]", "$.b.c[1][\"d e\"]", "$.b.c[1]", "$.b.c", "$.b.f", "$.b", "$.g", "$"]
        );
        assert_eq!(map.path_of_line(lines.len()), None);
        for (line, path) in paths.iter().enumerate() {
            let first = paths.iter().position(|other| other == path).unwrap();
            assert_eq!(map.line_of_path(path), Some(first), "{line} {path}");
        }
        assert_eq!(map.line_of_path("$.b.nope"), None);
        assert_eq!(map.line_of_path("not a path"), None);
    }

    #[test]
    fn summary_paths_lead_to_their_lines() {
        let (left, right) = (doc(r#"{"k": [{"id": 1, "é\"": 1}], "x y": {"z": 1}}"#), doc(r#"{"k": [{"id": 1, "é\"": 2}], "x y": {"z": 2}}"#));
        let lines = left.lines();
        let map = PathMap::scan(&lines, Format::Json);
        let summary = summarize(&left, &right, &JsonOptions::default());
        let found: Vec<(&str, usize)> = summary.changes.iter().map(|c| (c.path(), map.line_of_path(c.path()).unwrap())).collect();
        assert_eq!(found, [("$.k[0][\"é\\\"\"]", 4), ("$[\"x y\"].z", 8)]);
    }

    #[test]
    fn paths_survive_text_being_edited() {
        let lines = ["{", "  \"a\": [1, {\"b\": 2},", "", "  \"c\" 3", "  ]]] ,, \"unterminated", "  \"d\": \"\\"];
        let map = PathMap::scan(&lines, Format::Json);
        assert_eq!(map.path_of_line(1).as_deref(), Some("$.a"));
        assert_eq!(map.path_of_line(2).as_deref(), Some("$.a[1]"));
        for line in 0..lines.len() {
            assert!(map.path_of_line(line).is_some());
        }
        let _ = PathMap::scan(&["\"\\é", "\\", "\"", "}]", "\"a\\"], Format::Json);
    }

    #[test]
    fn json_lines_paths_count_records() {
        let lines = ["{\"a\": 1}", "[1, 2]", "", "3"];
        let map = PathMap::scan(&lines, Format::JsonLines);
        let paths: Vec<String> = (0..4).map(|line| map.path_of_line(line).unwrap()).collect();
        assert_eq!(paths, ["$[0]", "$[1]", "$[1]", "$[2]"]);
        assert_eq!(map.line_of_path("$[2]"), Some(3));
        assert_eq!(map.line_of_path("$[0].a"), Some(0));
    }

    // ----- commas -----

    #[test]
    fn valid_text_needs_no_comma_repair() {
        let lines = doc(r#"{"a": [1, {"b": "x,]"}, []], "c": {}, "d": "}"}"#).lines();
        assert!(repair_commas(&lines, Format::Json).is_empty());
        assert!(repair_commas(&["{\"a\": 1}", "{\"a\": 2}", "[1, 2]"], Format::JsonLines).is_empty());
    }

    #[test]
    fn a_transferred_block_gets_its_commas_back() {
        // A last member pushed into the middle, and a member pushed after the last one.
        let lines = ["{", "  \"a\": 1,", "  \"z\": [", "    1", "  ]", "  \"b\": 2", "  \"c\": {}", "}"];
        assert_eq!(
            repair_commas(&lines, Format::Json),
            [(4, "  ],".to_owned()), (5, "  \"b\": 2,".to_owned())]
        );
    }

    #[test]
    fn a_removed_block_leaves_no_trailing_comma() {
        let lines = ["{", "  \"a\": [", "    1,", "  ],", "}"];
        assert_eq!(repair_commas(&lines, Format::Json), [(2, "    1".to_owned()), (3, "  ]".to_owned())]);
        assert_eq!(repair_commas(&["[,1,,2,]"], Format::Json), [(0, "[1,2]".to_owned())]);
    }

    #[test]
    fn repaired_text_parses_again() {
        let lines = ["{", "  \"a\": 1", "  \"b\": [", "    1,", "    2,", "  ]", "  \"c\": {", "    \"d\": \"é\"", "    \"e\": null,", "  },", "}"];
        let mut text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        for (line, fixed) in repair_commas(&lines, Format::Json) {
            text[line] = fixed;
        }
        let parsed = JsonDoc::parse_lines(&text, Format::Json).unwrap();
        assert!(parsed.has_canonical_layout());
        assert_eq!(parsed.root.to_compact(), r#"{"a":1,"b":[1,2],"c":{"d":"é","e":null}}"#);
    }

    // ----- diff -----

    #[test]
    fn a_new_last_member_does_not_mark_the_previous_line() {
        let left = ["{", "  \"a\": 1", "}"];
        let right = ["{", "  \"a\": 1,", "  \"b\": 2", "}"];
        let diff = diff_json_lines(&left, &right, DiffOptions::default());
        assert_eq!(diff.blocks.len(), 1);
        assert_eq!(diff.blocks[0].kind, BlockKind::Insert);
        assert_eq!(diff.blocks[0].right, 2..3);
        assert_eq!(diff_lines(&left, &right, DiffOptions::default()).blocks[0].kind, BlockKind::Change);
    }

    #[test]
    fn ignore_rules_apply_to_the_significant_part_of_json_lines() {
        let left = ["{", "  \"built\": \"2024-03-15\",", "  \"version\": 1", "}"];
        let right = ["{", "    \"built\": \"2025-11-02\",", "    \"version\": 1", "}"];
        assert_eq!(diff_json_lines(&left, &right, DiffOptions::default()).blocks.len(), 1);
        let rules = IgnoreRules::new(&[IgnoreRule::whole_line("^\"built\"")]).unwrap();
        let diff = diff_json_lines_with(&left, &right, DiffOptions::default(), &rules);
        assert!(diff.blocks.is_empty());
        assert_eq!(diff.ignored_rows.len(), 1);
    }

    #[test]
    fn indentation_is_not_a_difference() {
        let left = doc("{\n\t\"a\": [\n\t\t1\n\t]\n}").lines();
        let right = doc("{\n    \"a\": [\n        1\n    ]\n}").lines();
        assert!(diff_json_lines(&left, &right, DiffOptions::default()).is_identical());
        assert!(!diff_lines(&left, &right, DiffOptions::default()).is_identical());
    }

    #[test]
    fn spans_point_into_the_full_lines() {
        let left = ["{", "        \"name\": \"alpha\",", "}"];
        let right = ["{", "\t\"name\": \"beta\"", "}"];
        let diff = diff_json_lines(&left, &right, DiffOptions::default());
        let row = diff.rows.iter().find(|row| row.pair.is_some()).unwrap();
        let spans = &diff.spans[row.pair.unwrap() as usize];
        let slice = |line: &str, range: &Range<u32>| line[range.start as usize..range.end as usize].to_owned();
        assert_eq!(slice(left[1], &spans.left[0].range), "alpha");
        assert_eq!(slice(right[1], &spans.right[0].range), "beta");
    }

    #[test]
    fn significant_part_drops_indentation_and_final_comma() {
        assert_eq!(significant_part("    \"a\": 1,"), ("\"a\": 1", 4));
        assert_eq!(significant_part("\t\t],  "), ("]", 2));
        assert_eq!(significant_part("\"a,\""), ("\"a,\"", 0));
        assert_eq!(significant_part("  x  "), ("x  ", 2));
        assert_eq!(significant_part(""), ("", 0));
    }

    #[test]
    fn files_differing_only_in_form_compare_identical() {
        let left = doc("{\n\t\"name\": \"caf\\u00e9\",\n\t\"size\": 1.50,\n\t\"tags\": [\"a\", \"b\"],\n\t\"meta\": {\"x\": 1, \"y\": 2}\n}\n");
        let right = doc(r#"{"meta":{"y":2,"x":1},"tags":["a","b"],"size":1.5,"name":"café"}"#);
        let [l, r] = project(&left, &right, &JsonOptions::default());
        assert!(diff_json_lines(&l.lines, &r.lines, DiffOptions::default()).is_identical());
        assert!(l.reformatted && !l.normalized && r.reformatted && r.normalized);
        assert!(equal(&left, &right, &JsonOptions::default()));
    }

    #[test]
    fn only_real_changes_show_between_reordered_files() {
        let left = doc(r#"{"name": "x", "deps": {"a": "1", "b": "2"}, "version": "1.0"}"#);
        let right = doc(r#"{"version": "1.1", "deps": {"b": "2", "c": "3", "a": "1"}, "name": "x"}"#);
        let [l, r] = project(&left, &right, &JsonOptions::default());
        let diff = diff_json_lines(&l.lines, &r.lines, DiffOptions::default());
        let kinds: Vec<(BlockKind, Range<u32>)> = diff.blocks.iter().map(|block| (block.kind, block.right.clone())).collect();
        assert_eq!(r.lines[3..6], ["    \"a\": \"1\",", "    \"b\": \"2\",", "    \"c\": \"3\""]);
        assert_eq!(kinds, [(BlockKind::Insert, 5..6), (BlockKind::Change, 7..8)]);
        assert_eq!(
            changes(&l.text("\n"), &r.text("\n"), &JsonOptions::default()),
            ["Added $.deps.c  -> \"3\"", "Changed $.version \"1.0\" -> \"1.1\""]
        );
    }

    // ----- size -----

    fn big_records(count: usize, shift: usize) -> String {
        let mut text = String::from("[\n");
        for n in 0..count {
            let id = (n + shift) % count;
            let _ = write!(
                text,
                "  {{\"id\": {id}, \"name\": \"record-{id}\", \"score\": {id}.50, \"tags\": [\"alpha\", \"beta\", {id}], \"nested\": {{\"flag\": true, \"note\": \"caf\\u00e9 {id}\"}}}}"
            );
            text.push_str(if n + 1 < count { ",\n" } else { "\n" });
        }
        text.push_str("]\n");
        text
    }

    #[test]
    fn large_arrays_pair_without_quadratic_work() {
        // 60 000 records, rotated on one side: by identity, then by content without identity.
        let (left, right) = (doc(&big_records(60_000, 0)), doc(&big_records(60_000, 7)));
        for identity_keys in [JsonOptions::default().identity_keys, Vec::new()] {
            let options = JsonOptions {
                identity_keys,
                ..JsonOptions::default()
            };
            let summary = summarize(&left, &right, &options);
            assert!(!summary.changes.is_empty());
            let unordered = JsonOptions {
                array_order: ArrayOrder::Unordered,
                ..options
            };
            assert!(equal(&left, &right, &unordered));
            let [l, r] = project(&left, &right, &unordered);
            assert_eq!(l.lines, r.lines);
        }
    }

    /// `cargo test -p biomerge-core json::tests::timing -- --ignored --nocapture`
    #[test]
    #[ignore = "timing report, not a check"]
    fn timing_on_a_large_file() {
        use std::time::Instant;
        let (left_text, right_text) = (big_records(200_000, 0), big_records(200_000, 3));
        let timed = |label: &str, start: Instant| eprintln!("{label}: {:?}", start.elapsed());
        eprintln!("size: {} MB per side", left_text.len() / 1_000_000);
        let start = Instant::now();
        let left = doc(&left_text);
        timed("parse one side", start);
        let right = doc(&right_text);
        let options = JsonOptions::default();
        let start = Instant::now();
        let [l, r] = project(&left, &right, &options);
        timed("project both sides", start);
        let start = Instant::now();
        let summary = summarize(&left, &right, &options);
        timed("summarize", start);
        let start = Instant::now();
        let map = PathMap::scan(&l.lines, Format::Json);
        timed("path map of one side", start);
        let start = Instant::now();
        let _ = repair_commas(&l.lines, Format::Json);
        timed("comma check of one side", start);
        let start = Instant::now();
        let diff = diff_json_lines(&l.lines, &r.lines, DiffOptions::default());
        timed("line diff", start);
        eprintln!("{} lines, {} changes, {} blocks, {:?}", l.lines.len(), summary.changes.len(), diff.blocks.len(), map.path_of_line(1_000_000));
    }
}
