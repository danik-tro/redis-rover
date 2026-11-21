//! Edit a value in `$VISUAL` / `$EDITOR` / `vi` (`Shift-e` on a STRING).
//! No terminal handling here: the App suspends the TUI around [`edit`].
//!
//! The value goes into a file only its owner can read (`0600`, created
//! new, never over an existing file) that is removed on every way out: the
//! editor saving, failing, being killed, or rrover being told to stop.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// Largest value edited this way, as the yank warning (`MEMORY USAGE`
/// before, the edited text after).
pub const MAX_EDIT_BYTES: usize = 10 * 1024 * 1024;

/// How an edit ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Edited {
    /// Saved with this text (without the trailing newline most editors add,
    /// when the value had none).
    Changed(String),
    /// Saved as it was, or not saved.
    Unchanged,
    /// The editor failed, or the text is unusable; the reason.
    Failed(String),
    /// rrover got `SIGTERM` / `SIGHUP` meanwhile: the editor was stopped
    /// and rrover should quit.
    Stopped,
}

/// The editor command: `$VISUAL`, else `$EDITOR`, else `vi`.
#[must_use]
pub fn editor_command(visual: Option<String>, editor: Option<String>) -> String {
    [visual, editor]
        .into_iter()
        .flatten()
        .find(|cmd| !cmd.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned())
}

/// Write `text` to a private temp file, run `editor` on it and read it
/// back. The file is removed whatever happens.
pub async fn edit(text: &str, editor: &str) -> Edited {
    let file = match TempFile::create(text) {
        Ok(file) => file,
        Err(err) => return Edited::Failed(format!("can't create the temp file: {err}")),
    };
    let signals = SignalGuard::install();
    let status = run(editor, file.path(), &signals).await;
    drop(signals);
    match status {
        Run::Stopped => return Edited::Stopped,
        Run::Failed(reason) => return Edited::Failed(reason),
        Run::Done => {}
    }
    match file.read() {
        Ok(new) => compare(text, new),
        Err(err) => Edited::Failed(format!("can't read the edited file back: {err}")),
    }
}

/// The saved text against the value: most editors end the last line with
/// a newline the value didn't have, which isn't an edit.
fn compare(old: &str, mut new: String) -> Edited {
    if !old.ends_with('\n') && new.ends_with('\n') {
        new.pop();
        if !old.ends_with('\r') && new.ends_with('\r') {
            new.pop();
        }
    }
    if new.len() > MAX_EDIT_BYTES {
        return Edited::Failed(format!(
            "the edited value is over {} MiB",
            MAX_EDIT_BYTES / (1024 * 1024)
        ));
    }
    if new == old {
        Edited::Unchanged
    } else {
        Edited::Changed(new)
    }
}

enum Run {
    Done,
    Failed(String),
    Stopped,
}

/// Run `editor path` through the shell (so `EDITOR="code -w"` works) and
/// wait, checking for a stop signal.
async fn run(editor: &str, path: &Path, signals: &SignalGuard) -> Run {
    let mut command = shell_command(editor, path);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return Run::Failed(format!("can't start {editor}: {err}")),
    };
    loop {
        tokio::select! {
            status = child.wait() => {
                return match status {
                    Ok(status) if status.success() => Run::Done,
                    Ok(status) => Run::Failed(format!("{editor} exited with {}", describe(status))),
                    Err(err) => Run::Failed(format!("waiting for {editor} failed: {err}")),
                };
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {
                if signals.stop_requested() {
                    let _ = child.kill().await;
                    return Run::Stopped;
                }
            }
        }
    }
}

#[cfg(unix)]
fn shell_command(editor: &str, path: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    // `$1` is the path, quoted by the shell; the editor string is the
    // user's own, words and all.
    command
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("rrover-editor")
        .arg(path);
    command
}

#[cfg(not(unix))]
fn shell_command(editor: &str, path: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("cmd");
    command.arg("/C").arg(editor).arg(path);
    command
}

/// `code 1`, or `signal 9` for an editor that was killed.
fn describe(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        // A shell that stays as the editor's parent (dash) doesn't die of
        // the signal: it exits with 128 + the signal number.
        let from_shell = || {
            status
                .code()
                .filter(|code| (129..=192).contains(code))
                .map(|code| code - 128)
        };
        if let Some(signal) = status.signal().or_else(from_shell) {
            return format!("signal {signal}");
        }
    }
    status.code().map_or_else(
        || "an unknown status".to_owned(),
        |code| format!("code {code}"),
    )
}

/// The temp file, removed on drop (also when the edit panics).
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn create(text: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir();
        let mut last_err = None;
        // A name taken already (another rrover, a leftover) is never
        // opened: `create_new` fails and the next name is tried.
        for attempt in 0..16u32 {
            let path = dir.join(format!(
                "rrover-edit-{}-{}-{attempt}.txt",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos()),
            ));
            match private_new_file(&path) {
                Ok(mut file) => {
                    let guard = Self { path };
                    file.write_all(text.as_bytes())?;
                    file.sync_all()?;
                    return Ok(guard);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(err),
                Err(err) => return Err(err),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::other("no free temp file name")))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The file as the editor left it. Editors often save by writing a new
    /// file and renaming it over this one, so it is opened by name again.
    fn read(&self) -> std::io::Result<String> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len > (MAX_EDIT_BYTES as u64) * 2 {
            return Err(std::io::Error::other("the edited file is too large"));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("Couldn't remove the edit temp file: {err}");
            }
        }
    }
}

/// A new file readable and writable by its owner only.
fn private_new_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// While the editor runs, the terminal is out of raw mode: Ctrl-C sends
/// `SIGINT` to rrover as well as to the editor, and closing the terminal
/// `SIGHUP`. With the default actions rrover would die and leave the temp
/// file behind. For the edit only, these signals set a flag instead
/// (`SIGINT` / `SIGQUIT` belong to the editor and are otherwise ignored;
/// `SIGTERM` / `SIGHUP` stop the edit and quit); the old actions come back
/// on drop. A handler (unlike `SIG_IGN`) is reset by `exec`, so the editor
/// still gets the default actions.
struct SignalGuard {
    #[cfg(unix)]
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

#[cfg(unix)]
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(signal: libc::c_int) {
    // Async-signal-safe: one atomic store.
    if signal == libc::SIGTERM || signal == libc::SIGHUP {
        STOP.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl SignalGuard {
    #[cfg(unix)]
    fn install() -> Self {
        STOP.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut saved = Vec::new();
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: `sigaction` with a zeroed struct, an empty mask and a
            // handler that only stores an atomic; the old action is kept
            // and put back in `drop`.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&raw mut action.sa_mask);
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, &raw const action, &raw mut old) == 0 {
                    saved.push((signal, old));
                }
            }
        }
        Self { saved }
    }

    #[cfg(not(unix))]
    fn install() -> Self {
        Self {}
    }

    /// Takes the guard: the flag only means something while it lives.
    #[allow(clippy::unused_self)]
    fn stop_requested(&self) -> bool {
        #[cfg(unix)]
        {
            STOP.load(std::sync::atomic::Ordering::SeqCst)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (signal, old) in &self.saved {
            // SAFETY: restores the action saved in `install`.
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Temp files the edits of this test process left behind.
    fn leftovers() -> Vec<PathBuf> {
        let prefix = format!("rrover-edit-{}-", std::process::id());
        std::fs::read_dir(std::env::temp_dir())
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with(&prefix))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_editor_is_visual_then_editor_then_vi() {
        let s = |v: &str| Some(v.to_owned());
        assert_eq!(editor_command(s("code -w"), s("nano")), "code -w");
        assert_eq!(editor_command(None, s("nano")), "nano");
        assert_eq!(editor_command(s("  "), s("nano")), "nano");
        assert_eq!(editor_command(None, None), "vi");
    }

    #[test]
    fn a_trailing_newline_from_the_editor_is_not_an_edit() {
        assert_eq!(compare("abc", "abc\n".into()), Edited::Unchanged);
        assert_eq!(compare("abc", "abc\r\n".into()), Edited::Unchanged);
        assert_eq!(compare("abc\n", "abc\n".into()), Edited::Unchanged);
        assert_eq!(
            compare("abc", "abd\n".into()),
            Edited::Changed("abd".into())
        );
        assert!(matches!(
            compare("a", "x".repeat(MAX_EDIT_BYTES + 1)),
            Edited::Failed(_)
        ));
    }

    /// The file is the owner's only, holds the value, and is gone after a
    /// save, a failing editor, a killed editor and a `SIGTERM` to rrover.
    #[tokio::test]
    async fn the_temp_file_is_private_and_always_removed() {
        use std::os::unix::fs::PermissionsExt;

        // The "editor" checks the mode and the content, then edits. GNU form
        // first: GNU `stat -f` means filesystem status and prints to stdout
        // before failing, while BSD `stat -c` fails without printing.
        let check = "sh -c 'test \"$(stat -c %a \"$0\" 2>/dev/null || stat -f %Lp \"$0\")\" = 600 \
                     && grep -q hello \"$0\" && printf world > \"$0\"'";
        assert_eq!(edit("hello", check).await, Edited::Changed("world".into()));
        assert!(leftovers().is_empty(), "{:?}", leftovers());

        assert_eq!(edit("same", "true").await, Edited::Unchanged);

        match edit("x", "false").await {
            Edited::Failed(reason) => assert!(reason.contains("code 1"), "{reason}"),
            other => panic!("{other:?}"),
        }
        match edit("x", "sh -c 'kill -9 $$'").await {
            Edited::Failed(reason) => assert!(reason.contains("signal 9"), "{reason}"),
            other => panic!("{other:?}"),
        }
        match edit("x", "/nonexistent/editor").await {
            Edited::Failed(_) => {}
            other => panic!("{other:?}"),
        }
        assert!(leftovers().is_empty(), "{:?}", leftovers());

        let file = TempFile::create("v").expect("create");
        let mode = std::fs::metadata(file.path())
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(file);
        assert!(leftovers().is_empty());

        // In the same test: the stop flag and the handlers are process-wide.
        sigterm_during_the_edit_stops_it_and_cleans_up().await;
    }

    /// `SIGTERM` to rrover while the editor runs stops the editor, removes
    /// the file and asks rrover to quit; afterwards the old action is back.
    async fn sigterm_during_the_edit_stops_it_and_cleans_up() {
        let edit = tokio::spawn(edit("x", "sh -c 'sleep 30'"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        // SAFETY: signals this test process; the guard's handler is
        // installed, so it only sets the flag.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGTERM);
        }
        let ended = tokio::time::timeout(Duration::from_secs(5), edit)
            .await
            .expect("the edit stopped")
            .expect("no panic");
        assert_eq!(ended, Edited::Stopped);
        assert!(leftovers().is_empty(), "{:?}", leftovers());

        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: reads the current action only.
        unsafe {
            libc::sigaction(libc::SIGTERM, std::ptr::null(), &raw mut current);
        }
        assert_eq!(current.sa_sigaction, libc::SIG_DFL, "the default is back");
    }
}
