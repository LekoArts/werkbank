//! The always-on watcher: one background `werkbank daemon <session>` per Herdr
//! session. A startup hook starts it, every Werkbank entry point revives it,
//! and the Inbox only views the state file it writes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::events::{self, Wake};
use crate::core::herdr::{Herdr, HerdrError};
use crate::core::runner::{RealRunner, Runner};
use crate::core::stop::Stopped;
use crate::core::{env, unix_ms};
use crate::features::inbox::watcher::{ObserverRecord, Settings, Snapshot, Watcher};
use crate::features::inbox::{open_lock_file, try_lock};
use crate::integrations::{linear, mastracode};

/// How often, and how patiently, the liveness thread asks Herdr whether the server is up.
const PROBE_EVERY: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Unreachable for this long without a break means the server is gone.
const GONE_AFTER: Duration = Duration::from_secs(30);
/// A freshly spawned daemon may race another caller's lock probe.
const SPAWN_WINDOW: Duration = Duration::from_secs(2);
/// A handover successor waits for its predecessor to release the lock.
const HANDOVER_WINDOW: Duration = Duration::from_secs(15);
const HANDOVER_RETRY_MS: u64 = 60_000;
/// The idle wait wakes this often to check for a refresh request or a stop.
const IDLE_STEP: Duration = Duration::from_millis(250);
/// The poll while Herdr's events wake the loop; screen checks still need it.
const EVENT_POLL: Duration = Duration::from_secs(10);
/// How long a lost event stream waits before it reconnects.
const EVENT_RETRY: Duration = Duration::from_secs(5);
const LOG_KEEP: u64 = 256 * 1024;

/// Per-session files in the plugin state dir.
#[derive(Debug, Clone)]
pub struct Paths {
    dir: PathBuf,
    session: String,
}

impl Paths {
    pub fn new(dir: impl Into<PathBuf>, session: &str) -> Self {
        Self {
            dir: dir.into(),
            session: session.to_string(),
        }
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    fn file(&self, name: &str, ext: &str) -> PathBuf {
        let safe = crate::core::safe_session(&self.session);
        self.dir.join(format!("{name}-{safe}{ext}"))
    }

    pub fn lock(&self) -> PathBuf {
        self.file("daemon", ".lock")
    }

    pub fn state(&self) -> PathBuf {
        self.file("state", ".json")
    }

    pub fn refresh(&self) -> PathBuf {
        self.file("refresh", "")
    }

    pub fn log(&self) -> PathBuf {
        self.file("daemon", ".log")
    }

    pub fn notify_log(&self) -> PathBuf {
        self.file("notifications", ".log")
    }

    /// The Inbox's cached PR lists.
    pub fn prs(&self) -> PathBuf {
        self.file("prs", ".json")
    }

    /// The Inbox's cached Linear issues.
    pub fn issues(&self) -> PathBuf {
        self.file("issues", ".json")
    }
}

/// `state-<session>.json`: the snapshot the Inbox renders, plus the observer
/// record only a handover successor reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    #[serde(flatten)]
    pub snapshot: Snapshot,
    #[serde(default)]
    pub observer: Option<ObserverRecord>,
}

/// Writes via a temp file in the same dir, fsync, then rename, so a reader
/// sees the old file or the new one, never a torn one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent dir", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        // A leftover temp file from a reused pid keeps its old mode otherwise.
        let _ = f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600));
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

pub fn write_state(path: &Path, state: &StateFile) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(state)?)
}

/// `None` when missing or malformed; the Inbox then waits for the next write.
pub fn read_state(path: &Path) -> Option<StateFile> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

pub fn read_generation(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The Inbox's `r`: the daemon ticks again as soon as it sees a higher number.
pub fn bump_generation(path: &Path) -> Result<()> {
    write_atomic(path, (read_generation(path) + 1).to_string().as_bytes())
}

/// Takes the lock, retrying for `window`; `None` when another process kept it.
pub fn acquire(path: &Path, window: Duration) -> Result<Option<File>> {
    let file = open_lock_file(path)?;
    let deadline = Instant::now() + window;
    loop {
        if try_lock(&file)? {
            return Ok(Some(file));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Whether some process holds the lock. A free lock is taken and dropped at once.
pub fn lock_held(path: &Path) -> bool {
    open_lock_file(path).is_ok_and(|f| matches!(try_lock(&f), Ok(false)))
}

fn write_record(lock: &mut File, pid: u32, started_unix_ms: u64) -> Result<()> {
    let record = serde_json::json!({ "pid": pid, "started_unix_ms": started_unix_ms });
    lock.set_len(0)?;
    lock.rewind()?;
    lock.write_all(record.to_string().as_bytes())?;
    lock.flush()?;
    Ok(())
}

/// `--session` wins, then the session Herdr runs us under (`core::herdr_session`:
/// `HERDR_SESSION`, else the owner of `HERDR_SOCKET_PATH`); there is no default, so a daemon
/// can never land on the wrong (e.g. the user's live) session.
pub fn resolve_session(arg: Option<&str>, env_session: &str) -> Option<String> {
    arg.filter(|s| !s.is_empty())
        .or(Some(env_session).filter(|s| !s.is_empty()))
        .map(str::to_string)
}

fn current_exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

fn log(msg: &str) {
    eprintln!("[{}] {msg}", unix_ms() / 1000);
}

unsafe extern "C" {
    fn setsid() -> i32;
}

/// Starts `exe daemon <session>` in its own session with stdio on the log, and
/// does not wait: the caller is about to exit, and init reaps the daemon.
fn spawn_daemon(exe: &Path, paths: &Paths, handover: Option<u32>) -> Result<()> {
    crate::core::private_dir(&paths.dir)?;
    let log = crate::core::open_private_append(&paths.log())?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").arg(&paths.session);
    if let Some(pid) = handover {
        cmd.arg("--handover").arg(pid.to_string());
    }
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            if setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    cmd.spawn()
        .with_context(|| format!("spawning {} daemon", exe.display()))?;
    Ok(())
}

/// `daemon-start` with its inputs injected: `Ok(true)` when it spawned one,
/// `Ok(false)` when a daemon already holds the lock.
pub fn start_with(
    session_arg: Option<&str>,
    env_session: &str,
    state_dir: &str,
    spawn: impl FnOnce(&Paths) -> Result<()>,
) -> Result<bool> {
    let Some(session) = resolve_session(session_arg, env_session) else {
        bail!("no session (pass --session or run under Herdr); not starting a daemon");
    };
    if state_dir.is_empty() {
        bail!("HERDR_PLUGIN_STATE_DIR is not set; not starting a daemon");
    }
    let paths = Paths::new(state_dir, &session);
    if lock_held(&paths.lock()) {
        return Ok(false);
    }
    spawn(&paths)?;
    Ok(true)
}

/// For short-lived callers (startup hook, actions, event hooks).
pub fn spawn_daemon_if_free(session_arg: Option<&str>) -> Result<bool> {
    start_with(
        session_arg,
        &crate::core::herdr_session(&RealRunner::default()),
        &env("HERDR_PLUGIN_STATE_DIR"),
        |paths| spawn_daemon(&current_exe()?, paths, None),
    )
}

/// Best effort from entry points: a failure only goes to stderr (the plugin log).
pub fn revive() {
    if let Err(err) = spawn_daemon_if_free(None) {
        eprintln!("werkbank: daemon not started: {err:#}");
    }
}

/// For long-lived callers (the Inbox): `daemon-start` runs as a waited-for
/// child, so the daemon is a grandchild and never a zombie of the caller.
pub fn ensure_daemon<R: Runner>(runner: &R, exe: &Path, session: &str) -> Result<()> {
    let exe = exe.to_string_lossy();
    let out = runner.run(&[&exe, "daemon-start", "--session", session], None, None)?;
    if !out.success() {
        bail!("daemon-start: {}", out.stderr.trim());
    }
    Ok(())
}

pub fn ensure_current(session: &str) -> Result<()> {
    ensure_daemon(&RealRunner::default(), &current_exe()?, session)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Alive,
    /// Herdr's `server_not_running` reply, as recorded in Baseline.
    Unreachable,
    /// A timeout or any other failure: proves nothing either way.
    Other,
}

pub fn classify<T>(result: &Result<T>) -> Probe {
    match result {
        Ok(_) => Probe::Alive,
        Err(err)
            if err
                .downcast_ref::<HerdrError>()
                .is_some_and(|e| e.code == "server_not_running") =>
        {
            Probe::Unreachable
        }
        Err(_) => Probe::Other,
    }
}

/// A run of unreachable probes; anything else breaks it.
#[derive(Debug)]
pub struct Streak {
    window_ms: u64,
    since_ms: Option<u64>,
}

impl Streak {
    pub fn new(window: Duration) -> Self {
        Self {
            window_ms: window.as_millis() as u64,
            since_ms: None,
        }
    }

    /// True once probes have been unreachable without a break for the window.
    pub fn observe(&mut self, probe: Probe, now_ms: u64) -> bool {
        if probe != Probe::Unreachable {
            self.since_ms = None;
            return false;
        }
        let since = *self.since_ms.get_or_insert(now_ms);
        now_ms.saturating_sub(since) >= self.window_ms
    }
}

fn sleep_unless(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while !stop.load(Ordering::SeqCst) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(IDLE_STEP));
    }
}

/// A tick that reached the server within this long already proves it is up.
const ALIVE_FRESH_MS: u64 = 2 * PROBE_EVERY.as_millis() as u64;

/// `Alive` without a subprocess while ticks keep reaching the server; once
/// they fail or stall, the real probe runs so server-gone detection keeps its bound.
pub fn probe_unless_ticking(alive_ms: u64, now_ms: u64, probe: impl FnOnce() -> Probe) -> Probe {
    if alive_ms != 0 && now_ms.saturating_sub(alive_ms) < ALIVE_FRESH_MS {
        Probe::Alive
    } else {
        probe()
    }
}

/// Probes until the server has been unreachable for `window`, then sets `stop`
/// and returns true; returns false if something else stopped first.
pub fn watch_liveness(
    mut probe: impl FnMut() -> Probe,
    every: Duration,
    window: Duration,
    stop: &AtomicBool,
) -> bool {
    let start = Instant::now();
    let mut streak = Streak::new(window);
    while !stop.load(Ordering::SeqCst) {
        if streak.observe(probe(), start.elapsed().as_millis() as u64) {
            stop.store(true, Ordering::SeqCst);
            return true;
        }
        sleep_unless(stop, every);
    }
    false
}

/// Whether to hand over to a fresh daemon because the binary changed.
#[derive(Debug)]
pub struct Handover {
    start: Option<SystemTime>,
    retry_at_ms: u64,
}

impl Handover {
    pub fn new(start: Option<SystemTime>) -> Self {
        Self {
            start,
            retry_at_ms: 0,
        }
    }

    pub fn due(&self, current: Option<SystemTime>, now_ms: u64) -> bool {
        matches!((self.start, current), (Some(a), Some(b)) if a != b) && now_ms >= self.retry_at_ms
    }

    /// A failed spawn keeps this daemon on the old binary; retry in a minute.
    pub fn failed(&mut self, now_ms: u64) {
        self.retry_at_ms = now_ms + HANDOVER_RETRY_MS;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Stopped,
    HandedOver,
}

pub struct LoopIo<'a> {
    pub exe_mtime: &'a dyn Fn() -> Option<SystemTime>,
    /// Set to the tick's start (`mono_ms`) after each tick that reached the server.
    pub alive_ms: &'a AtomicU64,
    /// Monotonic ms (never 0), so a wall-clock step can't fake a fresh tick.
    pub mono_ms: &'a dyn Fn() -> u64,
    pub spawn_successor: &'a mut dyn FnMut() -> Result<()>,
    pub now_ms: &'a dyn Fn() -> u64,
    /// Herdr events: wake the loop early, and allow a slower poll while live.
    pub events: &'a Wake,
}

/// Ticks, writes state, and sleeps until the poll is due, a refresh is
/// requested, a Herdr event arrives, or the stop flag is set. Never clears tokens: on handover the
/// successor keeps them, and on server exit they went with the server.
pub fn run_loop<R: Runner>(watcher: &mut Watcher<R>, paths: &Paths, pid: u32, io: LoopIo) -> Exit {
    let stop = watcher.stop_flag();
    let settings = watcher.settings();
    let mut handover = Handover::new((io.exe_mtime)());
    let mut last = Snapshot::default();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Exit::Stopped;
        }
        // A request that lands during the tick is seen after it.
        let seen = read_generation(&paths.refresh());
        let seen_events = io.events.generation();
        let now = Instant::now();
        let now_ms = (io.now_ms)();
        let mono = (io.mono_ms)();
        match watcher.tick(now) {
            Ok(snapshot) => {
                last = snapshot;
                io.alive_ms.store(mono, Ordering::SeqCst);
            }
            Err(err) if err.is::<Stopped>() => return Exit::Stopped,
            Err(err) => last.error = Some(format!("{err:#}")),
        }
        last.ticked_unix_ms = now_ms;
        last.daemon_pid = pid;
        last.stall_secs = settings.stall_threshold.as_secs();
        io.events
            .watch(last.agents.iter().map(|a| a.info.pane_id.clone()).collect());
        let poll = if io.events.connected() {
            settings.poll.max(EVENT_POLL)
        } else {
            settings.poll
        };
        last.poll_ms = poll.as_millis() as u64;
        let state = StateFile {
            snapshot: last.clone(),
            observer: Some(watcher.export(now, now_ms)),
        };
        if let Err(err) = write_state(&paths.state(), &state) {
            log(&format!("state: {err:#}"));
        }
        let now_ms = (io.now_ms)();
        if handover.due((io.exe_mtime)(), now_ms) {
            match (io.spawn_successor)() {
                Ok(()) => return Exit::HandedOver,
                Err(err) => {
                    log(&format!(
                        "handover failed, staying on the old binary: {err:#}"
                    ));
                    handover.failed(now_ms);
                }
            }
        }
        let wake = Instant::now() + poll;
        while !stop.load(Ordering::SeqCst)
            && read_generation(&paths.refresh()) <= seen
            && io.events.generation() <= seen_events
            && Instant::now() < wake
        {
            thread::sleep(IDLE_STEP.min(wake.saturating_duration_since(Instant::now())));
        }
    }
}

/// Keeps a log's newest 256 KiB; run only by the daemon that won the lock.
fn truncate_log(path: &Path) -> Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    if len <= LOG_KEEP {
        return Ok(());
    }
    f.seek(SeekFrom::Start(len - LOG_KEEP))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    f.set_len(0)?;
    f.rewind()?;
    f.write_all(&tail)?;
    Ok(())
}

fn parse_args(args: &[String]) -> Result<(String, Option<u32>)> {
    let session = args.first().map(String::as_str).unwrap_or_default();
    if session.is_empty() || session.starts_with('-') {
        bail!("missing session; refusing to start (usage: daemon <session> [--handover <pid>])");
    }
    let handover = match (args.get(1).map(String::as_str), args.get(2)) {
        (None, _) => None,
        (Some("--handover"), Some(pid)) => Some(pid.parse().context("--handover pid")?),
        _ => bail!("usage: daemon <session> [--handover <pid>]"),
    };
    Ok((session.to_string(), handover))
}

/// `werkbank daemon <session> [--handover <pid>]`.
pub fn run(args: &[String]) -> Result<()> {
    let (session, handover) = parse_args(args)?;
    let dir = env("HERDR_PLUGIN_STATE_DIR");
    if dir.is_empty() {
        bail!("HERDR_PLUGIN_STATE_DIR is not set");
    }
    let paths = Paths::new(&dir, &session);
    let window = if handover.is_some() {
        HANDOVER_WINDOW
    } else {
        SPAWN_WINDOW
    };
    let Some(mut lock) = acquire(&paths.lock(), window)? else {
        log(&format!("daemon {session}: another daemon holds the lock"));
        return Ok(());
    };
    // Every log in the state dir only grows; the lock winner trims them all.
    for path in [paths.log(), paths.notify_log(), paths.file("setup", ".log")] {
        let _ = truncate_log(&path);
    }
    let pid = std::process::id();
    write_record(&mut lock, pid, unix_ms())?;
    log(&match handover {
        Some(from) => format!("daemon {session} pid {pid} started (handover from {from})"),
        None => format!("daemon {session} pid {pid} started"),
    });

    let exe = current_exe()?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut watcher = Watcher::new(
        Herdr::new(RealRunner::default()).for_session(&session),
        Settings::from_env(),
        stop.clone(),
    )
    .with_linear_key(linear::load_key())
    .with_mastra_db(mastracode::db_path())
    .with_notify_log(paths.notify_log());
    if let Some(from) = handover {
        // Only the predecessor's own record: never state from another server.
        if let Some(record) = read_state(&paths.state())
            .filter(|s| s.snapshot.daemon_pid == from)
            .and_then(|s| s.observer)
        {
            watcher.seed(&record, Instant::now(), unix_ms());
            log("took over the predecessor's alerts and stall clocks");
        }
    }

    let gone = Arc::new(AtomicBool::new(false));
    let alive_ms = Arc::new(AtomicU64::new(0));
    let epoch = Instant::now();
    let mono_ms = move || epoch.elapsed().as_millis() as u64 + 1;
    {
        let probe = Herdr::new(RealRunner {
            timeout: PROBE_TIMEOUT,
        })
        .for_session(&session);
        let (stop, gone, alive_ms) = (stop.clone(), gone.clone(), alive_ms.clone());
        thread::spawn(move || {
            let is_gone = watch_liveness(
                || {
                    probe_unless_ticking(alive_ms.load(Ordering::SeqCst), mono_ms(), || {
                        classify(&probe.workspace_list())
                    })
                },
                PROBE_EVERY,
                GONE_AFTER,
                &stop,
            );
            gone.store(is_gone, Ordering::SeqCst);
        });
    }

    let events = Arc::new(Wake::default());
    {
        let herdr = Herdr::new(RealRunner {
            timeout: PROBE_TIMEOUT,
        })
        .for_session(&session);
        let (events, stop) = (events.clone(), stop.clone());
        thread::spawn(move || {
            events::run(|| herdr.socket_path(), &events, &stop, EVENT_RETRY, log);
        });
    }

    let exe_mtime = || fs::metadata(&exe).and_then(|m| m.modified()).ok();
    let mut spawn = || spawn_daemon(&exe, &paths, Some(pid));
    let exit = run_loop(
        &mut watcher,
        &paths,
        pid,
        LoopIo {
            exe_mtime: &exe_mtime,
            alive_ms: &alive_ms,
            mono_ms: &mono_ms,
            spawn_successor: &mut spawn,
            now_ms: &unix_ms,
            events: &events,
        },
    );
    match exit {
        Exit::HandedOver => log("binary changed; handed over to a successor"),
        Exit::Stopped if gone.load(Ordering::SeqCst) => {
            // A later server may reuse pane ids: leave it nothing to inherit.
            if let Some(mut state) = read_state(&paths.state()) {
                state.observer = None;
                let _ = write_state(&paths.state(), &state);
            }
            log("herdr server gone; exiting");
        }
        Exit::Stopped => log("stopped"),
    }
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::herdr::HerdrError;
    use crate::core::runner::{FakeRunner, Output};
    use crate::features::inbox::model::AttentionKind;
    use crate::features::inbox::stall::StallRecord;
    use crate::features::inbox::watcher::tests::{fake, PANE};
    use crate::features::inbox::watcher::{SentRecord, SinceRecord};

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("werkbank-daemon-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record() -> ObserverRecord {
        ObserverRecord {
            alerted: vec![(PANE.into(), AttentionKind::Stalled)],
            sent: vec![SentRecord {
                pane: PANE.into(),
                pr: Some("#1 ✗".into()),
                stalled: Some("⏸ 6m".into()),
                asking: None,
            }],
            stall: vec![StallRecord {
                pane: PANE.into(),
                hash: 7,
                since_ms: 1,
                last_read_ms: 2,
                reported: Some((3, 4)),
            }],
            since: vec![SinceRecord {
                pane: PANE.into(),
                status: "working".into(),
                asking: false,
                stalled: true,
                since_ms: 5,
            }],
        }
    }

    #[test]
    fn state_file_round_trips_snapshot_alerts_and_stall_entries() {
        let mut w = fake(Duration::from_secs(300));
        let mut snapshot = w.tick(Instant::now()).unwrap();
        snapshot.daemon_pid = 42;
        snapshot.ticked_unix_ms = 1_000;
        let state = StateFile {
            snapshot,
            observer: Some(record()),
        };
        let path = temp_dir("roundtrip").join("state-s.json");
        write_state(&path, &state).unwrap();
        assert_eq!(read_state(&path).unwrap(), state);
    }

    #[test]
    fn a_reader_racing_writes_never_sees_a_torn_file() {
        let dir = temp_dir("race");
        let path = dir.join("state-s.json");
        let mut state = StateFile {
            observer: Some(record()),
            ..StateFile::default()
        };
        state.snapshot.error = Some("x".repeat(20_000));
        write_state(&path, &state).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, done) = (path.clone(), done.clone());
            thread::spawn(move || {
                let mut reads = 0;
                while !done.load(Ordering::SeqCst) {
                    let text = fs::read_to_string(&path).unwrap();
                    serde_json::from_str::<StateFile>(&text).expect("whole file");
                    reads += 1;
                }
                reads
            })
        };
        for n in 0..200 {
            state.snapshot.ticked_unix_ms = n;
            write_state(&path, &state).unwrap();
        }
        done.store(true, Ordering::SeqCst);
        assert!(reader.join().unwrap() > 0);
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp files left");
    }

    #[test]
    fn missing_or_malformed_state_reads_as_none() {
        let dir = temp_dir("malformed");
        assert!(read_state(&dir.join("absent.json")).is_none());
        fs::write(dir.join("bad.json"), "{\"agents\":").unwrap();
        assert!(read_state(&dir.join("bad.json")).is_none());
    }

    #[test]
    fn a_held_lock_turns_an_ordinary_start_away() {
        let path = temp_dir("held").join("daemon-s.lock");
        let _held = acquire(&path, Duration::ZERO).unwrap().unwrap();
        assert!(lock_held(&path));
        let started = Instant::now();
        assert!(acquire(&path, Duration::from_millis(200))
            .unwrap()
            .is_none());
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn a_waiting_start_wins_a_lock_released_within_its_window() {
        let path = temp_dir("release").join("daemon-s.lock");
        let held = acquire(&path, Duration::ZERO).unwrap().unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        assert!(acquire(&path, SPAWN_WINDOW).unwrap().is_some());
        releaser.join().unwrap();
    }

    #[test]
    fn a_handover_successor_gives_up_when_a_third_daemon_won() {
        let path = temp_dir("third").join("daemon-s.lock");
        let old = acquire(&path, Duration::ZERO).unwrap().unwrap();
        drop(old);
        // A child forked by a parallel test briefly shares the dropped descriptor (and so
        // the flock) until its exec closes it, so the third daemon waits like a real one.
        let _third = acquire(&path, SPAWN_WINDOW).unwrap().unwrap();
        assert!(acquire(&path, Duration::from_millis(300))
            .unwrap()
            .is_none());
    }

    #[test]
    fn handover_is_due_only_on_a_changed_binary_and_retries_once_a_minute() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let t1 = t0 + Duration::from_secs(5);
        let mut h = Handover::new(Some(t0));
        assert!(!h.due(Some(t0), 0), "unchanged binary");
        assert!(!h.due(None, 0), "binary missing mid-replace");
        assert!(h.due(Some(t1), 10_000));
        h.failed(10_000);
        assert!(!h.due(Some(t1), 10_000 + 59_999));
        assert!(h.due(Some(t1), 10_000 + 60_000));
        assert!(!Handover::new(None).due(Some(t1), 0));
    }

    fn not_running() -> anyhow::Error {
        HerdrError {
            code: "server_not_running".into(),
            message: "no herdr server is running".into(),
        }
        .into()
    }

    #[test]
    fn a_recent_successful_tick_replaces_the_liveness_probe() {
        let calls = std::cell::Cell::new(0);
        let probe = || {
            calls.set(calls.get() + 1);
            Probe::Unreachable
        };
        assert_eq!(
            probe_unless_ticking(10_000, 10_000 + 3_999, probe),
            Probe::Alive
        );
        assert_eq!(calls.get(), 0, "no subprocess while ticks reach the server");
        assert_eq!(
            probe_unless_ticking(10_000, 14_000, probe),
            Probe::Unreachable
        );
        assert_eq!(probe_unless_ticking(0, 1_000, probe), Probe::Unreachable);
        assert_eq!(
            calls.get(),
            2,
            "stalled ticks and a fresh daemon probe for real"
        );
    }

    #[test]
    fn session_names_cannot_leave_the_state_dir() {
        let p = Paths::new("/state", "../../etc/x");
        assert_eq!(p.state(), PathBuf::from("/state/state-.._.._etc_x.json"));
        assert_eq!(p.state().parent(), Some(Path::new("/state")));
        let p = Paths::new("/state", "work-2.b_c");
        assert_eq!(p.lock(), PathBuf::from("/state/daemon-work-2.b_c.lock"));
        // Setup writes the notification log the daemon trims, so they agree.
        let setup = crate::features::setup::SetupEnv {
            state_dir: "/state".into(),
            session: "my session/x".into(),
            override_file: None,
            herdr_bin: "herdr".into(),
        };
        let p = Paths::new("/state", "my session/x");
        assert_eq!(setup.notify_log(), p.notify_log());
        assert_eq!(setup.notify_log().parent(), Some(Path::new("/state")));
    }

    #[test]
    fn only_a_tick_that_reached_the_server_counts_as_alive() {
        let mut w = fake(Duration::from_secs(3600));
        w.herdr.runner.respond(
            &["herdr", "agent", "list"],
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: r#"{"error":{"code":"server_not_running","message":"gone"}}"#.into(),
            },
        );
        // Pops the fake's good agent list; every later one is the failure.
        w.tick(Instant::now()).unwrap();
        let stop = w.stop_flag();
        let paths = Paths::new(temp_dir("alive"), "s");
        let alive = AtomicU64::new(0);
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            stop.store(true, Ordering::SeqCst);
        });
        let (mtime, mut spawn) = no_handover();
        run_loop(
            &mut w,
            &paths,
            1,
            LoopIo {
                exe_mtime: &mtime,
                alive_ms: &alive,
                mono_ms: &|| 7_000,
                spawn_successor: &mut spawn,
                now_ms: &|| 5_000,
                events: &Wake::default(),
            },
        );
        stopper.join().unwrap();
        assert!(read_state(&paths.state()).unwrap().snapshot.error.is_some());
        assert_eq!(
            alive.load(Ordering::SeqCst),
            0,
            "a failed tick proves nothing"
        );
    }

    #[test]
    fn the_recorded_server_not_running_reply_counts_as_unreachable() {
        // Verbatim from the Baseline probe ($PD/baseline-unreachable.txt).
        let reply = r#"{"id":"cli:workspace:list","error":{"code":"server_not_running","message":"no herdr server is running at /home/user/.config/herdr/sessions/wb-probe-1794435563/herdr.sock; run `herdr session attach wb-probe-1794435563` to start or attach it"}}"#;
        let h = Herdr::with_bin(FakeRunner::new(), "herdr").for_session("s");
        h.runner.respond(
            &["herdr", "--session", "s", "workspace", "list"],
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: reply.into(),
            },
        );
        assert_eq!(classify(&h.workspace_list()), Probe::Unreachable);
        assert_eq!(
            classify::<()>(&Err(anyhow!("x timed out after 2s"))),
            Probe::Other
        );
        assert_eq!(classify(&Ok(())), Probe::Alive);
        assert_eq!(classify::<()>(&Err(not_running())), Probe::Unreachable);
    }

    #[test]
    fn only_an_unbroken_30s_unreachable_streak_means_gone() {
        let mut s = Streak::new(GONE_AFTER);
        assert!(!s.observe(Probe::Unreachable, 0));
        assert!(!s.observe(Probe::Unreachable, 29_999));
        assert!(s.observe(Probe::Unreachable, 30_000));

        // unreachable → timeout → unreachable: the window restarts.
        let mut s = Streak::new(GONE_AFTER);
        s.observe(Probe::Unreachable, 0);
        s.observe(Probe::Other, 20_000);
        assert!(!s.observe(Probe::Unreachable, 22_000));
        assert!(!s.observe(Probe::Unreachable, 51_999));
        assert!(s.observe(Probe::Unreachable, 52_000));

        // A success breaks it too.
        let mut s = Streak::new(GONE_AFTER);
        s.observe(Probe::Unreachable, 0);
        s.observe(Probe::Alive, 10_000);
        assert!(!s.observe(Probe::Unreachable, 35_000));

        // A slow but alive server (every probe times out) is never gone.
        let mut s = Streak::new(GONE_AFTER);
        assert!((0..100).all(|n| !s.observe(Probe::Other, n * 2_000)));
    }

    fn no_handover() -> (impl Fn() -> Option<SystemTime>, impl FnMut() -> Result<()>) {
        let t = SystemTime::UNIX_EPOCH;
        (move || Some(t), || panic!("no handover expected"))
    }

    fn clears(w: &Watcher<FakeRunner>) -> usize {
        w.herdr
            .runner
            .argvs()
            .iter()
            .filter(|a| a.contains("--clear-token"))
            .count()
    }

    #[test]
    fn server_gone_stops_the_loop_mid_call_without_clearing_tokens() {
        let mut w = fake(Duration::from_secs(3600));
        // First tick (fast) sets the pr token, then every agent list is slow.
        w.tick(Instant::now()).unwrap();
        w.herdr.runner.respond_after(
            &["herdr", "agent", "list"],
            None,
            Output {
                status: 0,
                stdout: crate::features::inbox::watcher::tests::agents_json("working"),
                stderr: String::new(),
            },
            Duration::from_millis(400),
        );
        let stop = w.stop_flag();
        let paths = Paths::new(temp_dir("gone"), "s");
        let liveness = thread::spawn(move || {
            watch_liveness(
                || Probe::Unreachable,
                Duration::from_millis(10),
                Duration::from_millis(100),
                &stop,
            )
        });
        let (mtime, mut spawn) = no_handover();
        let started = Instant::now();
        let exit = run_loop(
            &mut w,
            &paths,
            1,
            LoopIo {
                exe_mtime: &mtime,
                alive_ms: &AtomicU64::new(0),
                mono_ms: &|| 7_000,
                spawn_successor: &mut spawn,
                now_ms: &unix_ms,
                events: &Wake::default(),
            },
        );
        assert_eq!(exit, Exit::Stopped);
        assert!(liveness.join().unwrap(), "liveness saw the server go");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(clears(&w), 0, "{:?}", w.herdr.runner.argvs());
    }

    #[test]
    fn a_changed_binary_hands_over_after_writing_state_without_clearing() {
        let mut w = fake(Duration::from_secs(3600));
        let paths = Paths::new(temp_dir("handover"), "s");
        let t0 = SystemTime::UNIX_EPOCH;
        let calls = std::cell::Cell::new(0);
        let mtime = || {
            calls.set(calls.get() + 1);
            Some(if calls.get() == 1 {
                t0
            } else {
                t0 + Duration::from_secs(1)
            })
        };
        let mut spawned = 0;
        let mut spawn = || {
            spawned += 1;
            Ok(())
        };
        let alive = AtomicU64::new(0);
        let exit = run_loop(
            &mut w,
            &paths,
            7,
            LoopIo {
                exe_mtime: &mtime,
                alive_ms: &alive,
                mono_ms: &|| 7_000,
                spawn_successor: &mut spawn,
                now_ms: &|| 5_000,
                events: &Wake::default(),
            },
        );
        assert_eq!(exit, Exit::HandedOver);
        assert_eq!(spawned, 1);
        assert_eq!(
            alive.load(Ordering::SeqCst),
            7_000,
            "a good tick marks the server alive, on the monotonic clock"
        );
        assert_eq!(clears(&w), 0);
        let state = read_state(&paths.state()).unwrap();
        assert_eq!(state.snapshot.daemon_pid, 7);
        assert!(
            state.observer.is_some(),
            "the successor's record is written first"
        );
    }

    #[test]
    fn a_failed_successor_spawn_keeps_the_daemon_running() {
        let mut w = fake(Duration::from_secs(3600));
        w.herdr.runner.respond(
            &["herdr", "agent", "list"],
            None,
            Output {
                status: 0,
                stdout: crate::features::inbox::watcher::tests::agents_json("working"),
                stderr: String::new(),
            },
        );
        let stop = w.stop_flag();
        let paths = Paths::new(temp_dir("spawnfail"), "s");
        let t0 = SystemTime::UNIX_EPOCH;
        let first = std::cell::Cell::new(true);
        let mtime = || {
            Some(if first.replace(false) {
                t0
            } else {
                t0 + Duration::from_secs(1)
            })
        };
        let mut attempts = 0;
        let mut spawn = || {
            attempts += 1;
            if attempts == 3 {
                stop.store(true, Ordering::SeqCst);
            }
            Err(anyhow!("exec failed"))
        };
        // Ten simulated seconds per tick; poll is 10ms.
        let ms = std::cell::Cell::new(0u64);
        let now_ms = || {
            ms.set(ms.get() + 5_000);
            ms.get()
        };
        let exit = run_loop(
            &mut w,
            &paths,
            1,
            LoopIo {
                exe_mtime: &mtime,
                alive_ms: &AtomicU64::new(0),
                mono_ms: &|| 7_000,
                spawn_successor: &mut spawn,
                now_ms: &now_ms,
                events: &Wake::default(),
            },
        );
        assert_eq!(exit, Exit::Stopped);
        assert_eq!(attempts, 3);
        let ticks = w
            .herdr
            .runner
            .argvs()
            .iter()
            .filter(|a| *a == "herdr agent list")
            .count();
        // One retry per simulated minute: 1 + 6 + 6 ticks, each 10 s apart.
        assert_eq!(ticks, 13, "kept ticking between rate-limited retries");
    }

    fn agent_lists(w: &Watcher<FakeRunner>) -> usize {
        w.herdr
            .runner
            .argvs()
            .iter()
            .filter(|a| *a == "herdr agent list")
            .count()
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Poke {
        Nothing,
        Refresh,
        Event,
    }

    /// Ticks once (300 ms), pokes the loop 100 ms in, stops at 1.3 s.
    fn run_poked(poke: Poke, events: Arc<Wake>) -> (usize, Paths) {
        let w = fake(Duration::from_secs(3600));
        w.herdr.runner.respond_after(
            &["herdr", "agent", "list"],
            None,
            Output {
                status: 0,
                stdout: crate::features::inbox::watcher::tests::agents_json("idle"),
                stderr: String::new(),
            },
            Duration::from_millis(300),
        );
        let mut w = w.with_poll(Duration::from_secs(60));
        let stop = w.stop_flag();
        let paths = Paths::new(temp_dir(&format!("poke-{}", poke as u8)), "s");
        let refresh = paths.refresh();
        let poker_events = events.clone();
        let poker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            match poke {
                Poke::Nothing => {}
                Poke::Refresh => bump_generation(&refresh).unwrap(),
                Poke::Event => poker_events.bump(),
            }
            thread::sleep(Duration::from_millis(1_200));
            stop.store(true, Ordering::SeqCst);
        });
        let (mtime, mut spawn) = no_handover();
        run_loop(
            &mut w,
            &paths,
            1,
            LoopIo {
                exe_mtime: &mtime,
                alive_ms: &AtomicU64::new(0),
                mono_ms: &|| 7_000,
                spawn_successor: &mut spawn,
                now_ms: &unix_ms,
                events: &events,
            },
        );
        poker.join().unwrap();
        (agent_lists(&w), paths)
    }

    #[test]
    fn a_refresh_or_an_event_during_a_slow_tick_causes_exactly_one_extra_tick() {
        let bumped = thread::spawn(|| run_poked(Poke::Refresh, Arc::default()).0);
        let evented = thread::spawn(|| run_poked(Poke::Event, Arc::new(Wake::live())));
        assert_eq!(run_poked(Poke::Nothing, Arc::default()).0, 1);
        assert_eq!(bumped.join().unwrap(), 2);
        let (ticks, paths) = evented.join().unwrap();
        assert_eq!(ticks, 2);
        let state = read_state(&paths.state()).unwrap();
        assert_eq!(state.snapshot.poll_ms, 60_000, "a slower poll is kept");
    }

    #[test]
    fn live_events_slow_the_poll_and_learn_the_agent_panes() {
        let events = Arc::new(Wake::live());
        let mut w = fake(Duration::from_secs(3600)).with_poll(Duration::from_secs(2));
        w.herdr.runner.ok(
            &["herdr", "agent", "list"],
            None,
            &crate::features::inbox::watcher::tests::agents_json("idle"),
        );
        let stop = w.stop_flag();
        stop.store(false, Ordering::SeqCst);
        let paths = Paths::new(temp_dir("events-poll"), "s");
        let stopper = {
            let stop = stop.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                stop.store(true, Ordering::SeqCst);
            })
        };
        let (mtime, mut spawn) = no_handover();
        run_loop(
            &mut w,
            &paths,
            1,
            LoopIo {
                exe_mtime: &mtime,
                alive_ms: &AtomicU64::new(0),
                mono_ms: &|| 7_000,
                spawn_successor: &mut spawn,
                now_ms: &unix_ms,
                events: &events,
            },
        );
        stopper.join().unwrap();
        let state = read_state(&paths.state()).unwrap();
        assert_eq!(state.snapshot.poll_ms, EVENT_POLL.as_millis() as u64);
        assert!(!events.watching().is_empty(), "subscribes to the agents");
        assert_eq!(
            events.watching(),
            state
                .snapshot
                .agents
                .iter()
                .map(|a| a.info.pane_id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn session_comes_from_the_flag_then_the_env_and_never_a_default() {
        assert_eq!(resolve_session(Some("a"), "b").as_deref(), Some("a"));
        assert_eq!(resolve_session(None, "b").as_deref(), Some("b"));
        assert_eq!(resolve_session(Some(""), "b").as_deref(), Some("b"));
        assert_eq!(resolve_session(None, ""), None);
    }

    #[test]
    fn start_without_a_session_fails_closed_and_spawns_nothing() {
        let dir = temp_dir("nosession");
        let err = start_with(None, "", dir.to_str().unwrap(), |_| {
            panic!("must not spawn")
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("no session"), "{err}");
        assert!(!err.contains("default"), "names no session: {err}");
        assert!(start_with(Some("s"), "", "", |_| panic!("must not spawn")).is_err());
    }

    #[test]
    fn start_spawns_only_when_the_lock_is_free() {
        let dir = temp_dir("start");
        let mut spawned = Vec::new();
        assert!(start_with(Some("s"), "x", dir.to_str().unwrap(), |p| {
            spawned.push(p.session().to_string());
            Ok(())
        })
        .unwrap());
        assert_eq!(spawned, ["s"]);
        let _held = acquire(&Paths::new(&dir, "s").lock(), Duration::ZERO)
            .unwrap()
            .unwrap();
        assert!(!start_with(Some("s"), "", dir.to_str().unwrap(), |_| panic!("held")).unwrap());
    }

    #[test]
    fn daemon_refuses_an_empty_session_argument() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["".into()]).is_err());
        assert!(parse_args(&["--handover".into(), "1".into()]).is_err());
        assert_eq!(
            parse_args(&["s".into(), "--handover".into(), "9".into()]).unwrap(),
            ("s".to_string(), Some(9))
        );
    }

    #[test]
    fn ensure_daemon_runs_daemon_start_for_the_session_and_waits() {
        let f = FakeRunner::new();
        f.ok(&["/bin/wb", "daemon-start", "--session", "s"], None, "");
        ensure_daemon(&f, Path::new("/bin/wb"), "s").unwrap();
        assert_eq!(f.argvs(), ["/bin/wb daemon-start --session s"]);
    }

    #[test]
    fn the_log_keeps_its_newest_256_kib() {
        let path = temp_dir("log").join("daemon-s.log");
        let mut text = vec![b'a'; 100];
        text.extend(vec![b'b'; LOG_KEEP as usize]);
        fs::write(&path, &text).unwrap();
        truncate_log(&path).unwrap();
        let kept = fs::read(&path).unwrap();
        assert_eq!(kept.len() as u64, LOG_KEEP);
        assert!(kept.iter().all(|b| *b == b'b'));
    }
}
