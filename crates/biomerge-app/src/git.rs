//! The committed version of a file, read with the `git` program. Slow enough (a process per
//! question) to belong on a worker thread.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use biomerge_core::document::Document;

/// Runs `git` in `dir`, quietly: never a prompt, never a pager, no lock taken on the index.
fn git(dir: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| match error.kind() {
            ErrorKind::NotFound => "Git est introuvable : installe-le, ou ajoute-le au PATH".to_owned(),
            _ => format!("Git n'a pas pu être lancé : {error}"),
        })
}

/// What git said on failure, on one line, for the end of a message.
fn complaint(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim().to_owned()
}

/// The file as of the last commit, decoded as the working file would be.
pub fn head_version(path: &Path) -> Result<Document, String> {
    let shown = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    if !path.is_file() {
        return Err(format!("{shown} n'est pas un fichier"));
    }
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    let inside = git(dir, &["rev-parse", "--is-inside-work-tree"])?;
    if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        return Err(format!("{shown} n'est pas dans un dépôt Git"));
    }
    if !git(dir, &["ls-files", "--error-unmatch", "--", &name])?.status.success() {
        return Err(format!("{shown} n'est pas suivi par Git"));
    }
    if !git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"])?.status.success() {
        return Err("Le dépôt n'a encore aucun commit".to_owned());
    }
    // `./` makes the path relative to `dir`, not to the top of the work tree.
    let show = git(dir, &["show", &format!("HEAD:./{name}")])?;
    if !show.status.success() {
        let why = complaint(&show);
        return Err(if why.contains("exists on disk, but not in") {
            format!("{shown} n'existe pas dans HEAD : il a été ajouté depuis le dernier commit")
        } else {
            format!("Git n'a pas pu lire {shown} dans HEAD : {why}")
        });
    }
    decode(show.stdout, &shown)
}

/// Text in any encoding the working file could have. The decoder of `Document` reads files, so
/// what is not UTF-8 takes a short trip through a private temporary file.
fn decode(bytes: Vec<u8>, shown: &str) -> Result<Document, String> {
    let bytes = match String::from_utf8(bytes) {
        Ok(text) => return Ok(Document::from_text(&text)),
        Err(error) => error.into_bytes(),
    };
    let dir = temporary_dir().map_err(|e| format!("Fichier temporaire impossible : {e}"))?;
    let file = dir.join("head");
    let result = std::fs::write(&file, &bytes)
        .map_err(|e| format!("Fichier temporaire impossible : {e}"))
        .and_then(|()| Document::load(&file).map_err(|_| format!("{shown} est un fichier binaire dans HEAD")));
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn temporary_dir() -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "biomerge-head-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Runs git in a throwaway repository, away from the user's configuration and hooks.
    pub fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    /// A repository with `tracked.txt` committed then modified, and `new.txt` never added.
    pub fn repository(dir: &Path) {
        run(dir, &["init", "-q", "."]);
        std::fs::write(dir.join("tracked.txt"), "un\ndeux\ntrois\n").unwrap();
        std::fs::write(dir.join("latin1.txt"), b"caf\xe9\n").unwrap();
        run(dir, &["add", "tracked.txt", "latin1.txt"]);
        run(dir, &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("tracked.txt"), "un\n2\ntrois\n").unwrap();
        std::fs::write(dir.join("new.txt"), "nouveau\n").unwrap();
    }

    #[test]
    fn the_committed_text_comes_back_and_errors_say_why_in_french() {
        let dir = tempfile::tempdir().unwrap();
        repository(dir.path());
        let head = head_version(&dir.path().join("tracked.txt")).unwrap();
        let lines: Vec<&str> = head.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(lines, ["un", "deux", "trois"]);
        let latin = head_version(&dir.path().join("latin1.txt")).unwrap();
        assert_eq!(latin.lines[0].text, "café");

        let untracked = head_version(&dir.path().join("new.txt")).unwrap_err();
        assert_eq!(untracked, "new.txt n'est pas suivi par Git");
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("alone.txt"), "x").unwrap();
        let error = head_version(&outside.path().join("alone.txt")).unwrap_err();
        assert_eq!(error, "alone.txt n'est pas dans un dépôt Git");

        run(dir.path(), &["add", "new.txt"]);
        let added = head_version(&dir.path().join("new.txt")).unwrap_err();
        assert!(added.contains("ajouté depuis le dernier commit"), "{added}");
    }
}
