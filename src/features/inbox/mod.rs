//! The Inbox: a pane in its own tab that views the always-on daemon's state.

pub mod asking;
pub mod issues;
pub mod model;
pub mod prs;
pub mod stall;
pub mod status;
pub mod theme;
pub mod ui;
pub mod watcher;

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::core::env;
use crate::core::herdr::{Herdr, Sound};
use crate::core::runner::Runner;
use crate::features::daemon::Paths;

/// The Inbox's session; outside Herdr it falls back to `default` for its own files.
fn session() -> String {
    match env("HERDR_SESSION") {
        s if s.is_empty() => "default".to_string(),
        s => s,
    }
}

/// The per-session lock file the Inbox holds for its lifetime.
fn inbox_lock_path() -> PathBuf {
    PathBuf::from(env("HERDR_PLUGIN_STATE_DIR")).join(format!(
        "inbox-{}.lock",
        crate::core::safe_session(&session())
    ))
}

pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        crate::core::private_dir(dir)?;
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?)
}

/// `Ok(true)` when this handle now holds the lock, `Ok(false)` when another
/// Inbox holds it. The OS drops the lock with the handle, even on a crash.
pub(crate) fn try_lock(file: &File) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err.into()),
    }
}

/// Runs the Inbox while holding the per-session lock; a second Inbox focuses
/// the first one and exits.
pub fn run<R: Runner>(herdr: &Herdr<R>) -> Result<()> {
    let mut file = open_lock_file(&inbox_lock_path())?;
    if !try_lock(&file)? {
        return focus_running_inbox(herdr, &mut file);
    }
    let record = serde_json::json!({
        "pid": std::process::id(),
        "pane_id": env("HERDR_PANE_ID"),
        "tab_id": env("HERDR_TAB_ID"),
        "session": env("HERDR_SESSION"),
    });
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(record.to_string().as_bytes())?;
    file.flush()?;
    let tab = env("HERDR_TAB_ID");
    if !tab.is_empty() {
        // Cosmetic: an unnamed tab still works as the Inbox.
        let _ = herdr.tab_rename(&tab, "Inbox");
    }
    let paths = Paths::new(env("HERDR_PLUGIN_STATE_DIR"), &session());
    let result = ui::run_inbox(paths);
    drop(file);
    result
}

pub fn open<R: Runner>(herdr: &Herdr<R>) -> Result<()> {
    open_inbox_at(herdr, &inbox_lock_path())
}

fn open_inbox_at<R: Runner>(herdr: &Herdr<R>, path: &Path) -> Result<()> {
    let mut file = open_lock_file(path)?;
    if try_lock(&file)? {
        drop(file);
        herdr.plugin_pane_open("inbox", &[])?;
        Ok(())
    } else {
        focus_running_inbox(herdr, &mut file)
    }
}

fn focus_running_inbox<R: Runner>(herdr: &Herdr<R>, file: &mut File) -> Result<()> {
    let mut record = String::new();
    file.read_to_string(&mut record)?;
    let record = serde_json::from_str::<serde_json::Value>(&record).unwrap_or_default();
    let field = |k: &str| record.get(k).and_then(|v| v.as_str()).map(str::to_string);
    match field("tab_id") {
        Some(tab) if herdr.tab_focus(&tab).is_ok() => Ok(()),
        _ => herdr
            .notify(
                "Inbox already open",
                field("pane_id").as_deref().unwrap_or("in this session"),
                Sound::None,
            )
            .map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::FakeRunner;

    fn lock_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("werkbank-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// Parallel tests fork subprocesses; a child shares the lock's open file
    /// description until its exec closes it (CLOEXEC), so a release can take a
    /// moment to become visible.
    fn locks_within_a_second(file: &File) -> bool {
        (0..100).any(|_| {
            try_lock(file).unwrap() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        })
    }

    #[test]
    fn second_handle_is_blocked_until_first_drops() {
        let path = lock_path("concurrent.lock");
        let first = open_lock_file(&path).unwrap();
        assert!(try_lock(&first).unwrap());
        let second = open_lock_file(&path).unwrap();
        assert!(!try_lock(&second).unwrap());
        drop(first);
        assert!(locks_within_a_second(&second));
    }

    #[test]
    fn open_inbox_focuses_running_inbox_instead_of_opening_another() {
        let path = lock_path("open.lock");
        let mut held = open_lock_file(&path).unwrap();
        assert!(try_lock(&held).unwrap());
        held.write_all(br#"{"pid":1,"pane_id":"w1:p9","tab_id":"w1:t3","session":"s"}"#)
            .unwrap();
        let f = FakeRunner::new();
        f.ok(
            &["herdr", "tab", "focus", "w1:t3"],
            None,
            r#"{"id":"x","result":{}}"#,
        )
        .otherwise(r#"{"id":"x","result":{}}"#);
        let herdr = Herdr::with_bin(f, "herdr");
        open_inbox_at(&herdr, &path).unwrap();
        open_inbox_at(&herdr, &path).unwrap();
        let calls = herdr.runner.argvs();
        assert_eq!(calls, vec!["herdr tab focus w1:t3"; 2]);
        drop(held);
        assert!(locks_within_a_second(&open_lock_file(&path).unwrap()));
        open_inbox_at(&herdr, &path).unwrap();
        let third = &herdr.runner.argvs()[2];
        assert!(
            third.starts_with("herdr plugin pane open"),
            "{:?}",
            herdr.runner.argvs()
        );
    }
}
