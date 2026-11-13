//! One window for every comparison: a second `biomerge A B` hands its request to the running
//! window, which opens a tab, and exits. With `--wait` it first waits until that tab is closed,
//! which is what `git difftool` needs: git removes its temporary files as soon as the tool
//! returns. Nothing here can lose a request: whatever fails, the launch opens its own window.
//!
//! The protocol is a few lines on a Unix socket, one `key value` per line, an empty line to end:
//!
//! ```text
//! biomerge 1
//! left /abs/path        (with right: the pair to compare; neither: only raise the window)
//! right /abs/path
//! label-left name       (optional)
//! label-right name
//! wait                  (optional)
//! ```
//!
//! A three-way merge (`git mergetool`) sends `base`, `local`, `remote` and `merged` instead of
//! `left` and `right`, and may name the first three with `label-base`, `label-local` and
//! `label-remote`. `head /abs/path` asks for the file against its committed version, and
//! `path /abs/path` for one path alone (a file in conflict is merged, anything else waits in
//! the comparison dialog for its other side). A window older than these keys reads them as a
//! request to raise it.
//!
//! The window answers `ok` once the tab is open (or `error <message>`), then, for `wait`,
//! `closed` when the tab goes, followed by an exit code for a merge (`closed 0`: saved without
//! conflict). A window that dies closes the socket, which ends the wait too. Values are bytes
//! with `\` and line breaks escaped, so any Unix path goes through.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::Receiver;

/// The four files of a three-way merge, as `git mergetool` passes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeRequest {
    pub base: PathBuf,
    pub local: PathBuf,
    pub remote: PathBuf,
    /// Where the result is written.
    pub merged: PathBuf,
    /// Names shown for the base, the local and the remote version instead of git's temporary
    /// files.
    pub labels: [Option<String>; 3],
}

impl MergeRequest {
    pub fn paths_mut(&mut self) -> [&mut PathBuf; 4] {
        [&mut self.base, &mut self.local, &mut self.remote, &mut self.merged]
    }
}

/// What a launch asks of the window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub paths: Option<[PathBuf; 2]>,
    /// Names shown instead of the files' own, e.g. for the temporary files of git.
    pub labels: [Option<String>; 2],
    /// A three-way merge instead of a comparison.
    pub merge: Option<MergeRequest>,
    /// `--head`: the file against its version of the last commit.
    pub head: Option<PathBuf>,
    /// One path alone: a file git left in conflict is merged, anything else is the left side
    /// of the comparison dialog.
    pub single: Option<PathBuf>,
    pub wait: bool,
}

/// A request received by the window, with the way back to the launch that sent it.
pub struct Incoming {
    pub request: Request,
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
}

/// Held by the tab a waiting launch opened: dropping it, when the tab closes, lets the launch
/// exit, with the code a merge tab gave it. The launch that is the window itself waits through
/// a shared code instead, read when the window closes.
pub struct Waiter {
    #[cfg(unix)]
    stream: Option<std::os::unix::net::UnixStream>,
    local: Option<Arc<AtomicI32>>,
    code: Option<i32>,
}

impl Waiter {
    /// For the tab the window's own launch opened: the code lands in `outcome`.
    pub fn local(outcome: Arc<AtomicI32>) -> Self {
        Self {
            #[cfg(unix)]
            stream: None,
            local: Some(outcome),
            code: None,
        }
    }

    /// What the launch exits with. A merge tab keeps it up to date, so that closing the tab or
    /// the whole window gives the right one.
    pub fn set_code(&mut self, code: i32) {
        self.code = Some(code);
        if let Some(local) = &self.local {
            local.store(code, Ordering::Relaxed);
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(stream) = &mut self.stream {
            use std::io::Write;
            let line = match self.code {
                Some(code) => format!("closed {code}\n"),
                None => "closed\n".to_owned(),
            };
            let _ = stream.write_all(line.as_bytes());
        }
    }
}

impl Incoming {
    /// Tells the launch its tab is open. For a waiting launch, the returned `Waiter` must live
    /// as long as the tab.
    pub fn accept(self) -> Option<Waiter> {
        #[cfg(unix)]
        {
            use std::io::Write;
            let mut stream = self.stream;
            if stream.write_all(b"ok\n").is_err() {
                return None;
            }
            self.request.wait.then_some(Waiter { stream: Some(stream), local: None, code: None })
        }
        #[cfg(not(unix))]
        None
    }

    /// Tells the launch what went wrong; it prints it and exits.
    pub fn refuse(self, message: &str) {
        #[cfg(unix)]
        {
            use std::io::Write;
            let mut stream = self.stream;
            let _ = stream.write_all(format!("error {}\n", message.replace('\n', " ")).as_bytes());
        }
        #[cfg(not(unix))]
        let _ = message;
    }
}

/// How a hand-over ended.
pub enum Handed {
    /// The window has the request; `Waiting` still has to wait for the tab to close.
    Done,
    #[cfg(unix)]
    Waiting(std::io::BufReader<std::os::unix::net::UnixStream>),
    /// The window could not open the files and says why.
    Refused(String),
}

impl Handed {
    /// Blocks until the window closes the tab, or goes away. The code the tab closed with (0
    /// when it gave none), `None` when the window went away without a word.
    pub fn wait(self) -> Option<i32> {
        #[cfg(unix)]
        if let Handed::Waiting(mut reader) = self {
            use std::io::BufRead;
            let _ = reader.get_ref().set_read_timeout(None);
            let mut line = Vec::new();
            // "closed", "closed 1", or nothing at all when the window quits.
            let _ = reader.read_until(b'\n', &mut line);
            let line = String::from_utf8_lossy(&line);
            let rest = line.trim_end().strip_prefix("closed")?;
            return Some(rest.trim().parse().unwrap_or(0));
        }
        Some(0)
    }
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    use super::{Handed, Incoming, MergeRequest, Request};

    const MAGIC: &str = "biomerge 1";
    /// How long each side waits for the other to speak. A window that accepts the connection
    /// but never answers (stopped, wedged) does not swallow the request: the launch opens its
    /// own window instead.
    const ANSWER_WAIT: Duration = Duration::from_secs(2);

    unsafe extern "C" {
        safe fn getuid() -> u32;
    }

    /// `$XDG_RUNTIME_DIR/biomerge-<uid>.sock`, or the temporary folder without one.
    pub fn default_socket_path() -> PathBuf {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        dir.join(format!("biomerge-{}.sock", getuid()))
    }

    fn escape(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match byte {
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                byte => out.push(byte),
            }
        }
        out
    }

    fn unescape(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        let mut iter = bytes.iter();
        while let Some(&byte) = iter.next() {
            match (byte, iter.as_slice().first()) {
                (b'\\', Some(b'n')) => {
                    out.push(b'\n');
                    iter.next();
                }
                (b'\\', Some(b'\\')) => {
                    out.push(b'\\');
                    iter.next();
                }
                (byte, _) => out.push(byte),
            }
        }
        out
    }

    pub(super) fn encode(request: &Request) -> Vec<u8> {
        let mut out = Vec::new();
        let mut line = |key: &str, value: &[u8]| {
            out.extend_from_slice(key.as_bytes());
            out.push(b' ');
            out.extend_from_slice(&escape(value));
            out.push(b'\n');
        };
        line("biomerge", b"1");
        if let Some([left, right]) = &request.paths {
            line("left", left.as_os_str().as_bytes());
            line("right", right.as_os_str().as_bytes());
        }
        if let Some(merge) = &request.merge {
            for (key, path) in ["base", "local", "remote", "merged"].into_iter().zip([&merge.base, &merge.local, &merge.remote, &merge.merged]) {
                line(key, path.as_os_str().as_bytes());
            }
            for (key, label) in ["label-base", "label-local", "label-remote"].into_iter().zip(&merge.labels) {
                if let Some(label) = label {
                    line(key, label.as_bytes());
                }
            }
        }
        if let Some(head) = &request.head {
            line("head", head.as_os_str().as_bytes());
        }
        if let Some(single) = &request.single {
            line("path", single.as_os_str().as_bytes());
        }
        for (key, label) in ["label-left", "label-right"].into_iter().zip(&request.labels) {
            if let Some(label) = label {
                line(key, label.as_bytes());
            }
        }
        if request.wait {
            out.extend_from_slice(b"wait\n");
        }
        out.push(b'\n');
        out
    }

    /// The request in `lines`, or `None` when they are not one (another program, a launch cut
    /// short).
    pub(super) fn decode(lines: &[Vec<u8>]) -> Option<Request> {
        let (first, rest) = lines.split_first()?;
        if first.as_slice() != MAGIC.as_bytes() {
            return None;
        }
        let mut request = Request::default();
        let (mut left, mut right) = (None, None);
        let mut merge: [Option<PathBuf>; 4] = Default::default();
        let mut merge_labels: [Option<String>; 3] = Default::default();
        for line in rest {
            let (key, value) = match line.iter().position(|&b| b == b' ') {
                Some(at) => (&line[..at], unescape(&line[at + 1..])),
                None => (&line[..], Vec::new()),
            };
            let text = || String::from_utf8_lossy(&value).into_owned();
            let path = || PathBuf::from(std::ffi::OsString::from_vec(value.clone()));
            match key {
                b"base" => merge[0] = Some(path()),
                b"local" => merge[1] = Some(path()),
                b"remote" => merge[2] = Some(path()),
                b"merged" => merge[3] = Some(path()),
                b"label-base" => merge_labels[0] = Some(text()),
                b"label-local" => merge_labels[1] = Some(text()),
                b"label-remote" => merge_labels[2] = Some(text()),
                b"left" => left = Some(path()),
                b"right" => right = Some(path()),
                b"head" => request.head = Some(path()),
                b"path" => request.single = Some(path()),
                b"label-left" => request.labels[0] = Some(text()),
                b"label-right" => request.labels[1] = Some(text()),
                b"wait" => request.wait = true,
                // Newer launches may say more: what is not understood is left aside.
                _ => {}
            }
        }
        request.paths = match (left, right) {
            (Some(left), Some(right)) => Some([left, right]),
            (None, None) => None,
            _ => return None,
        };
        request.merge = match merge {
            [Some(base), Some(local), Some(remote), Some(merged)] => Some(MergeRequest { base, local, remote, merged, labels: merge_labels }),
            [None, None, None, None] => None,
            _ => return None,
        };
        Some(request)
    }

    /// Sends the request to the window listening on `socket`. `None`: nobody took it.
    pub fn hand_over(socket: &Path, request: &Request) -> Option<Handed> {
        let mut stream = UnixStream::connect(socket).ok()?;
        stream.set_write_timeout(Some(ANSWER_WAIT)).ok()?;
        stream.set_read_timeout(Some(ANSWER_WAIT)).ok()?;
        stream.write_all(&encode(request)).ok()?;
        let mut reader = BufReader::new(stream);
        let mut answer = Vec::new();
        reader.read_until(b'\n', &mut answer).ok()?;
        let answer = String::from_utf8_lossy(&answer);
        let answer = answer.trim_end();
        if answer == "ok" {
            Some(if request.wait { Handed::Waiting(reader) } else { Handed::Done })
        } else {
            answer.strip_prefix("error ").map(|message| Handed::Refused(message.to_owned()))
        }
    }

    /// A socket this process listens on. The file goes when the listener is dropped.
    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// Takes `socket` over, unless a window still answers there. A file nobody listens to
        /// is what a crashed window leaves behind: it is replaced.
        pub fn bind(socket: &Path) -> Option<Self> {
            if UnixStream::connect(socket).is_ok() {
                return None;
            }
            let _ = std::fs::remove_file(socket);
            let listener = UnixListener::bind(socket).ok()?;
            Some(Self { listener, path: socket.to_owned() })
        }

        /// Reads requests on a thread; each arrives on the channel and `wake` is called so that
        /// the interface picks it up.
        pub fn serve(&self, wake: impl Fn() + Send + 'static) -> Option<Receiver<Incoming>> {
            let listener = self.listener.try_clone().ok()?;
            let (sender, receiver) = channel();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    // A client that connects and goes quiet must not block the next ones.
                    let _ = stream.set_read_timeout(Some(ANSWER_WAIT));
                    let Ok(reading) = stream.try_clone() else {
                        continue;
                    };
                    let mut reader = BufReader::new(reading);
                    let mut lines = Vec::new();
                    let complete = loop {
                        let mut line = Vec::new();
                        match reader.read_until(b'\n', &mut line) {
                            Ok(0) | Err(_) => break false,
                            Ok(_) => {}
                        }
                        if line.pop() != Some(b'\n') {
                            break false;
                        }
                        if line.is_empty() {
                            break true;
                        }
                        lines.push(line);
                    };
                    let Some(request) = decode(&lines).filter(|_| complete) else {
                        continue;
                    };
                    let _ = stream.set_read_timeout(None);
                    if sender.send(Incoming { request, stream }).is_err() {
                        break;
                    }
                    wake();
                }
            });
            Some(receiver)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// After a hand-over: brings the window to the front. Wayland lets a window raise itself
    /// only with an activation token it does not have here, so on Hyprland the compositor is
    /// asked, as csvfab does.
    pub fn raise() {
        if cfg!(test) || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            return;
        }
        let selector = r"class:^(biomerge)$";
        let lua = format!("hl.dsp.focus({{ window = \"{}\" }})", selector.replace('\\', "\\\\"));
        let ok = std::process::Command::new("hyprctl")
            .args(["dispatch", &lua])
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "ok");
        if !ok {
            let _ = std::process::Command::new("hyprctl").args(["dispatch", "focuswindow", selector]).output();
        }
    }
}

/// Elsewhere every launch opens its own window.
#[cfg(not(unix))]
mod fallback {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::Receiver;

    use super::{Handed, Incoming, Request};

    pub fn default_socket_path() -> PathBuf {
        PathBuf::new()
    }

    pub fn hand_over(_: &Path, _: &Request) -> Option<Handed> {
        None
    }

    pub struct Listener;

    impl Listener {
        pub fn bind(_: &Path) -> Option<Self> {
            None
        }

        pub fn serve(&self, _: impl Fn() + Send + 'static) -> Option<Receiver<Incoming>> {
            None
        }
    }

    pub fn raise() {}
}

#[cfg(not(unix))]
pub use fallback::*;

/// The channel the window reads requests from.
pub type Requests = Receiver<Incoming>;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn requests_survive_the_trip_with_any_path() {
        let odd = PathBuf::from(OsString::from_vec(b"/tmp/a\\b\nc\xff.txt".to_vec()));
        let request = Request {
            paths: Some([odd, PathBuf::from("/tmp/x y.rs")]),
            labels: [Some("src/x.rs (base)".to_owned()), None],
            wait: true,
            ..Request::default()
        };
        let bytes = encode(&request);
        let mut lines: Vec<Vec<u8>> = bytes.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
        // The message ends with an empty line; what follows it is not part of it.
        let end = lines.iter().position(Vec::is_empty).unwrap();
        lines.truncate(end);
        assert_eq!(decode(&lines), Some(request));
    }

    fn lines_of(request: &Request) -> Vec<Vec<u8>> {
        let bytes = encode(request);
        let mut lines: Vec<Vec<u8>> = bytes.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
        let end = lines.iter().position(Vec::is_empty).unwrap();
        lines.truncate(end);
        lines
    }

    #[test]
    fn a_merge_request_carries_its_four_files_and_three_labels() {
        let request = Request {
            merge: Some(MergeRequest {
                base: PathBuf::from("/repo/x_BASE_1.rs"),
                local: PathBuf::from("/repo/x_LOCAL_1.rs"),
                remote: PathBuf::from("/repo/x_REMOTE_1.rs"),
                merged: PathBuf::from("/repo/x.rs"),
                labels: [Some("x.rs (base)".to_owned()), Some("x.rs (local)".to_owned()), None],
            }),
            wait: true,
            ..Request::default()
        };
        assert_eq!(decode(&lines_of(&request)), Some(request.clone()));
        // Three files of four are not a merge.
        let mut lines = lines_of(&request);
        lines.retain(|line| !line.starts_with(b"merged "));
        assert_eq!(decode(&lines), None);
    }

    #[test]
    fn a_head_request_and_a_single_path_survive_the_trip() {
        let odd = PathBuf::from(OsString::from_vec(b"/repo/a\\b\nc\xff.txt".to_vec()));
        let head = Request { head: Some(odd), wait: true, ..Request::default() };
        assert_eq!(decode(&lines_of(&head)), Some(head.clone()));
        assert!(lines_of(&head).contains(&b"head /repo/a\\\\b\\nc\xff.txt".to_vec()));
        let single = Request { single: Some(PathBuf::from("/repo/x y.rs")), ..Request::default() };
        assert_eq!(decode(&lines_of(&single)), Some(single.clone()));
        assert!(lines_of(&single).contains(&b"path /repo/x y.rs".to_vec()));
    }

    #[test]
    fn the_keys_of_older_launches_still_mean_the_same() {
        let lines = ["biomerge 1", "left /a", "right /b", "label-left A", "wait"].map(|line| line.as_bytes().to_vec());
        let request = decode(&lines).unwrap();
        assert_eq!(request.paths, Some([PathBuf::from("/a"), PathBuf::from("/b")]));
        assert_eq!(request.labels[0].as_deref(), Some("A"));
        assert!(request.wait && request.head.is_none() && request.single.is_none());
    }

    #[test]
    fn a_waiting_launch_hears_the_code_its_tab_closed_with() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("code.sock");
        let listener = Listener::bind(&socket).unwrap();
        let requests = listener.serve(|| {}).unwrap();
        for code in [None, Some(0), Some(1)] {
            let client = socket.clone();
            let launch = std::thread::spawn(move || {
                let request = Request { wait: true, ..Request::default() };
                hand_over(&client, &request).expect("the window answers").wait()
            });
            let incoming = requests.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let mut waiter = incoming.accept().expect("a waiting launch");
            if let Some(code) = code {
                waiter.set_code(code);
            }
            drop(waiter);
            assert_eq!(launch.join().unwrap(), Some(code.unwrap_or(0)));
        }
        // The window's own launch reads the code from memory.
        let outcome = Arc::new(AtomicI32::new(1));
        let mut waiter = Waiter::local(outcome.clone());
        waiter.set_code(0);
        assert_eq!(outcome.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn half_a_pair_or_another_program_is_not_a_request() {
        assert_eq!(decode(&[b"biomerge 1".to_vec(), b"left /a".to_vec()]), None);
        assert_eq!(decode(&[b"GET / HTTP/1.1".to_vec()]), None);
        let raise = decode(&[b"biomerge 1".to_vec()]).unwrap();
        assert_eq!(raise, Request::default());
    }

    #[test]
    fn a_stale_socket_is_replaced_and_a_live_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");
        // What a crashed window leaves: a socket file nobody listens to.
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        assert!(hand_over(&socket, &Request::default()).is_none());
        let listener = Listener::bind(&socket).expect("the stale file is taken over");
        assert!(Listener::bind(&socket).is_none(), "a live window keeps its socket");
        drop(listener);
        assert!(!socket.exists(), "the socket goes with its window");
    }

    #[test]
    fn a_window_that_never_answers_does_not_swallow_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mute.sock");
        let _mute = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let start = std::time::Instant::now();
        assert!(hand_over(&socket, &Request::default()).is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }
}
