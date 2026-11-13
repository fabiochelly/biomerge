//! Text documents: lossless decoding, line storage, reversible edits and safe saving.

use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use encoding_rs::{UTF_16BE, UTF_16LE, WINDOWS_1252};

const BINARY_PROBE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: fichier binaire")]
    Binary(PathBuf),
    #[error("{0}: le fichier a changé sur le disque depuis son ouverture")]
    ChangedOnDisk(PathBuf),
    #[error("{path}: le texte contient des caractères impossibles à écrire en {encoding}")]
    Unmappable { path: PathBuf, encoding: String },
    #[error("{0}: décodage avec pertes, sauvegarde refusée")]
    LossyDecode(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    None,
    Lf,
    CrLf,
    Cr,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::None => "",
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
            Eol::Cr => "\r",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Eol::None | Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
            Eol::Cr => "CR",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub eol: Eol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl TextEncoding {
    pub fn label(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf8Bom => "UTF-8 BOM",
            TextEncoding::Utf16Le => "UTF-16 LE",
            TextEncoding::Utf16Be => "UTF-16 BE",
            TextEncoding::Windows1252 => "Windows-1252",
        }
    }
}

/// A reversible replacement of consecutive lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub at: usize,
    pub removed: Vec<Line>,
    pub inserted: Vec<Line>,
}

impl Edit {
    pub fn inverted(&self) -> Edit {
        Edit {
            at: self.at,
            removed: self.inserted.clone(),
            inserted: self.removed.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DiskStamp {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct Document {
    pub path: Option<PathBuf>,
    /// Shared with background workers; `Arc::make_mut` keeps their snapshot valid.
    pub lines: Arc<Vec<Line>>,
    pub encoding: TextEncoding,
    pub lossy: bool,
    stamp: Option<DiskStamp>,
}

impl Document {
    pub fn from_text(text: &str) -> Self {
        Self {
            path: None,
            lines: Arc::new(split_lines(text)),
            encoding: TextEncoding::Utf8,
            lossy: false,
            stamp: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self, DocumentError> {
        let io = |source| DocumentError::Io {
            path: path.to_owned(),
            source,
        };
        let bytes = fs::read(path).map_err(io)?;
        let stamp = disk_stamp(path).map_err(io)?;
        let (text, encoding, lossy) =
            decode(&bytes).ok_or_else(|| DocumentError::Binary(path.to_owned()))?;
        Ok(Self {
            path: Some(path.to_owned()),
            lines: Arc::new(split_lines(&text)),
            encoding,
            lossy,
            stamp: Some(stamp),
        })
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The most frequent line ending, used for lines coming from the other side.
    pub fn dominant_eol(&self) -> Eol {
        let (mut lf, mut crlf, mut cr) = (0usize, 0usize, 0usize);
        for line in self.lines.iter() {
            match line.eol {
                Eol::Lf => lf += 1,
                Eol::CrLf => crlf += 1,
                Eol::Cr => cr += 1,
                Eol::None => {}
            }
        }
        if crlf > lf && crlf >= cr {
            Eol::CrLf
        } else if cr > lf {
            Eol::Cr
        } else {
            Eol::Lf
        }
    }

    /// Replaces `range` by `texts`, keeping this document's line-ending conventions.
    pub fn replace_lines(&mut self, range: Range<usize>, texts: Vec<String>) -> Edit {
        let eol = self.dominant_eol();
        let len = self.len();
        let touches_end = range.end == len;
        let had_final_eol = self.lines.last().is_none_or(|l| l.eol != Eol::None);

        let mut at = range.start;
        let mut removed_end = range.end;
        let mut inserted: Vec<Line> = texts
            .into_iter()
            .map(|text| Line { text, eol })
            .collect();

        // Appending after an unterminated last line: that line now needs an ending.
        if range.is_empty() && at == len && at > 0 && !had_final_eol && !inserted.is_empty() {
            let previous = self.lines[at - 1].text.clone();
            inserted.insert(0, Line { text: previous, eol });
            at -= 1;
            removed_end = len;
        }
        if touches_end && !had_final_eol {
            if let Some(last) = inserted.last_mut() {
                last.eol = Eol::None;
            }
        }

        let edit = Edit {
            at,
            removed: self.lines[at..removed_end].to_vec(),
            inserted,
        };
        self.apply(&edit);
        edit
    }

    /// Replaces `range` by `texts` as typing does: every other line keeps its ending, and the
    /// last new line takes the ending of the last replaced one, so a file's end does not change.
    pub fn edit_lines(&mut self, range: Range<usize>, texts: Vec<String>) -> Edit {
        let eol = self.dominant_eol();
        let last_eol = range.end.checked_sub(1).and_then(|last| self.lines.get(last)).map(|l| l.eol);
        let mut inserted: Vec<Line> = texts.into_iter().map(|text| Line { text, eol }).collect();
        if let (Some(line), Some(last_eol)) = (inserted.last_mut(), last_eol) {
            line.eol = last_eol;
        } else if self.lines.is_empty() {
            if let Some(line) = inserted.last_mut() {
                line.eol = Eol::None;
            }
        }
        let edit = Edit {
            at: range.start,
            removed: self.lines[range].to_vec(),
            inserted,
        };
        self.apply(&edit);
        edit
    }

    pub fn apply(&mut self, edit: &Edit) {
        let lines = Arc::make_mut(&mut self.lines);
        lines.splice(
            edit.at..edit.at + edit.removed.len(),
            edit.inserted.iter().cloned(),
        );
    }

    pub fn encode(&self) -> Result<Vec<u8>, DocumentError> {
        let path = || self.path.clone().unwrap_or_default();
        if self.lossy {
            return Err(DocumentError::LossyDecode(path()));
        }
        let mut text = String::with_capacity(self.lines.iter().map(|l| l.text.len() + 2).sum());
        for line in self.lines.iter() {
            text.push_str(&line.text);
            text.push_str(line.eol.as_str());
        }
        Ok(match self.encoding {
            TextEncoding::Utf8 => text.into_bytes(),
            TextEncoding::Utf8Bom => {
                let mut out = vec![0xEF, 0xBB, 0xBF];
                out.extend_from_slice(text.as_bytes());
                out
            }
            TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
                let le = self.encoding == TextEncoding::Utf16Le;
                let mut out = Vec::with_capacity(text.len() * 2 + 2);
                out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
                for unit in text.encode_utf16() {
                    out.extend_from_slice(&if le {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                out
            }
            TextEncoding::Windows1252 => {
                let (bytes, _, unmappable) = WINDOWS_1252.encode(&text);
                if unmappable {
                    return Err(DocumentError::Unmappable {
                        path: path(),
                        encoding: self.encoding.label().to_owned(),
                    });
                }
                bytes.into_owned()
            }
        })
    }

    /// Atomically writes the document. A file modified behind our back is never overwritten.
    pub fn save(&mut self) -> Result<(), DocumentError> {
        let path = self.path.clone().unwrap_or_default();
        let io = |source| DocumentError::Io {
            path: path.clone(),
            source,
        };
        let bytes = self.encode()?;
        // Writing through a symlink must update its target, not replace the link.
        let target = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if let (Some(stamp), Ok(current)) = (self.stamp, disk_stamp(&target)) {
            if stamp != current {
                return Err(DocumentError::ChangedOnDisk(path.clone()));
            }
        }
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let tmp = target.with_file_name(format!(".{name}.biomerge-{}.tmp", std::process::id()));
        let result = (|| {
            // A file that existed on one side only can live in a folder missing on the other.
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            if let Ok(meta) = fs::metadata(&target) {
                fs::set_permissions(&tmp, meta.permissions())?;
            }
            fs::rename(&tmp, &target)
        })();
        if let Err(source) = result {
            let _ = fs::remove_file(&tmp);
            return Err(io(source));
        }
        self.stamp = Some(disk_stamp(&target).map_err(io)?);
        Ok(())
    }
}

fn disk_stamp(path: &Path) -> std::io::Result<DiskStamp> {
    let meta = fs::metadata(path)?;
    Ok(DiskStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Returns `None` for binary content.
fn decode(bytes: &[u8]) -> Option<(String, TextEncoding, bool)> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let text = String::from_utf8_lossy(rest);
        let lossy = matches!(text, std::borrow::Cow::Owned(_));
        return Some((text.into_owned(), TextEncoding::Utf8Bom, lossy));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let (text, lossy) = UTF_16LE.decode_without_bom_handling(rest);
        return Some((text.into_owned(), TextEncoding::Utf16Le, lossy));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let (text, lossy) = UTF_16BE.decode_without_bom_handling(rest);
        return Some((text.into_owned(), TextEncoding::Utf16Be, lossy));
    }
    if bytes[..bytes.len().min(BINARY_PROBE_BYTES)].contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some((text.to_owned(), TextEncoding::Utf8, false)),
        Err(_) => {
            let (text, lossy) = WINDOWS_1252.decode_without_bom_handling(bytes);
            Some((text.into_owned(), TextEncoding::Windows1252, lossy))
        }
    }
}

fn split_lines(text: &str) -> Vec<Line> {
    let bytes = text.as_bytes();
    let mut lines = Vec::with_capacity(bytes.iter().filter(|&&b| b == b'\n').count() + 1);
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let eol = match bytes[i] {
            b'\n' => Some((Eol::Lf, 1)),
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => Some((Eol::CrLf, 2)),
            b'\r' => Some((Eol::Cr, 1)),
            _ => None,
        };
        if let Some((eol, width)) = eol {
            lines.push(Line {
                text: text[start..i].to_owned(),
                eol,
            });
            i += width;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() {
        lines.push(Line {
            text: text[start..].to_owned(),
            eol: Eol::None,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(bytes: &[u8], encoding: TextEncoding) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, bytes).unwrap();
        let mut doc = Document::load(&path).unwrap();
        assert_eq!(doc.encoding, encoding);
        doc.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn saving_unchanged_documents_preserves_every_byte() {
        round_trip(b"a\nb\r\nc\rd", TextEncoding::Utf8);
        round_trip(b"a\nb\n", TextEncoding::Utf8);
        round_trip(b"\xEF\xBB\xBFh\xC3\xA9\r\n", TextEncoding::Utf8Bom);
        round_trip(b"caf\xE9 \x81\n", TextEncoding::Windows1252);
        round_trip(b"\xFF\xFEa\0\n\0", TextEncoding::Utf16Le);
        round_trip(b"\xFE\xFF\0a\0\n", TextEncoding::Utf16Be);
    }

    #[test]
    fn binary_files_are_rejected() {
        assert!(decode(b"abc\0def").is_none());
    }

    #[test]
    fn replacing_adopts_the_target_line_endings_and_can_be_undone() {
        let mut doc = Document::from_text("a\r\nb\r\nc\r\n");
        let before = doc.clone();
        let edit = doc.replace_lines(1..2, vec!["x".into(), "y".into()]);
        assert_eq!(doc.lines[1].eol, Eol::CrLf);
        assert_eq!(doc.len(), 4);
        doc.apply(&edit.inverted());
        assert_eq!(doc.lines, before.lines);
    }

    #[test]
    fn appending_after_unterminated_line_keeps_the_file_unterminated() {
        let mut doc = Document::from_text("a\nb");
        let before = doc.clone();
        let edit = doc.replace_lines(2..2, vec!["c".into()]);
        assert_eq!(doc.encode().unwrap(), b"a\nb\nc");
        doc.apply(&edit.inverted());
        assert_eq!(doc.lines, before.lines);
    }

    #[test]
    fn typing_keeps_the_ending_of_each_line() {
        let mut doc = Document::from_text("a\r\nb\nc\nd");
        doc.edit_lines(0..1, vec!["x".into()]);
        assert_eq!(doc.encode().unwrap(), b"x\r\nb\nc\nd");
        doc.edit_lines(3..4, vec!["d1".into(), "d2".into()]);
        assert_eq!(doc.encode().unwrap(), b"x\r\nb\nc\nd1\nd2");
    }

    #[test]
    fn typing_in_an_empty_document_leaves_no_ending() {
        let mut doc = Document::from_text("");
        doc.edit_lines(0..0, vec!["hello".into()]);
        assert_eq!(doc.encode().unwrap(), b"hello");
    }

    #[test]
    fn save_refuses_a_file_changed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, "one\n").unwrap();
        let mut doc = Document::load(&path).unwrap();
        fs::write(&path, "changed elsewhere\n").unwrap();
        assert!(matches!(doc.save(), Err(DocumentError::ChangedOnDisk(_))));
        assert_eq!(fs::read_to_string(&path).unwrap(), "changed elsewhere\n");
    }

    #[test]
    fn save_refuses_text_the_encoding_cannot_represent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"caf\xE9\n").unwrap();
        let mut doc = Document::load(&path).unwrap();
        doc.replace_lines(0..1, vec!["日本語".into()]);
        assert!(matches!(doc.save(), Err(DocumentError::Unmappable { .. })));
        assert_eq!(fs::read(&path).unwrap(), b"caf\xE9\n");
    }

    #[test]
    fn save_creates_missing_parent_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/deep/f.txt");
        let mut doc = Document::from_text("a\n");
        doc.path = Some(path.clone());
        doc.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\n");
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_through_symlinks_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        fs::write(&real, "a\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut doc = Document::load(&link).unwrap();
        doc.replace_lines(0..1, vec!["b".into()]);
        doc.save().unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "b\n");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
