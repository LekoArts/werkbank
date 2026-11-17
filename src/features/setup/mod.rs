//! Zero-config worktree setup: on `worktree.created` (and the `setup-worktree`
//! action) copy local files from the main checkout, then run detected
//! install/build commands in a background `setup` tab.

pub mod copy;
pub mod detect;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::herdr::{Herdr, Sound};
use crate::core::runner::{RealRunner, Runner};
use crate::core::{env, fnv1a, unix_ms};
use crate::features::launch::presets;
use detect::Cmd;

/// How long Werkbank's own launchers wait for the hook's copy before starting the agent.
pub const COPY_WAIT: Duration = Duration::from_secs(2);
const COPY_POLL: Duration = Duration::from_millis(100);
const CLOSE_DELAY: Duration = Duration::from_secs(3);
/// How long one setup command may run before it is stopped.
pub const COMMAND_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How long a timed-out command gets after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(5);
const TITLE: &str = "Werkbank setup";

/// The copy marker for a checkout, shared by the hook, the launchers and cleanup.
/// FNV-1a of the canonical path, so every binary agrees on the name.
pub fn marker_path(state_dir: &Path, checkout: &str) -> PathBuf {
    let canon = Path::new(checkout)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(checkout));
    let id = fnv1a(canon.as_os_str().to_string_lossy().as_bytes());
    state_dir.join(format!("copied-{id:016x}"))
}

/// Waits up to `timeout` for a copy marker written after `since`. With no
/// state dir there is no hook to wait for, so it returns at once.
pub fn await_copy(
    state_dir: &str,
    checkout: &str,
    since: SystemTime,
    timeout: Duration,
    interval: Duration,
) -> bool {
    if state_dir.is_empty() {
        return true;
    }
    let marker = marker_path(Path::new(state_dir), checkout);
    let deadline = Instant::now() + timeout;
    loop {
        let fresh = fs::metadata(&marker)
            .and_then(|m| m.modified())
            .is_ok_and(|m| m >= since);
        if fresh {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}

pub fn copy_notice(branch: &str) -> String {
    format!("local files not copied yet for {branch} — run setup-worktree if missing")
}

/// Waits for the copy after a Werkbank launcher created `checkout`; on timeout
/// returns the notice to show. The launch itself always proceeds.
pub fn after_create(checkout: &str, branch: &str, since: SystemTime) -> Option<String> {
    let copied = await_copy(
        &env("HERDR_PLUGIN_STATE_DIR"),
        checkout,
        since,
        COPY_WAIT,
        COPY_POLL,
    );
    (!copied).then(|| copy_notice(branch))
}

/// `'…'` with embedded quotes as `'\''`: nothing reaches the shell unquoted.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What setup acts on.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub workspace_id: String,
    pub repo_root: String,
    pub checkout: String,
    pub branch: String,
}

/// The `worktree.created` payload, as recorded in `tests/fixtures/event-worktree-created.json`.
pub fn parse_event(json: &str) -> Result<Option<Target>> {
    let v: Value = serde_json::from_str(json).context("bad HERDR_PLUGIN_EVENT_JSON")?;
    let data = v.get("data").ok_or_else(|| anyhow!("event has no data"))?;
    let s = |ptr: &str| {
        data.pointer(ptr)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    if data.pointer("/worktree/is_linked_worktree") == Some(&Value::Bool(false)) {
        return Ok(None);
    }
    let missing = |what: &str| anyhow!("event has no {what}");
    Ok(Some(Target {
        workspace_id: s("/workspace/workspace_id")
            .or_else(|| s("/worktree/open_workspace_id"))
            .ok_or_else(|| missing("workspace id"))?,
        repo_root: s("/workspace/worktree/repo_root").ok_or_else(|| missing("repo_root"))?,
        checkout: s("/worktree/path").ok_or_else(|| missing("worktree path"))?,
        branch: s("/worktree/branch").unwrap_or_else(|| "(detached)".to_string()),
    }))
}

/// Where setup writes, and which server it talks to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupEnv {
    pub state_dir: PathBuf,
    pub session: String,
    pub override_file: Option<PathBuf>,
    pub herdr_bin: String,
}

impl SetupEnv {
    pub fn from_env() -> Result<SetupEnv> {
        let state_dir = env("HERDR_PLUGIN_STATE_DIR");
        let session = crate::core::herdr_session(&RealRunner::default());
        if state_dir.is_empty() || session.is_empty() {
            bail!("HERDR_PLUGIN_STATE_DIR and a Herdr session must be set (run under Herdr)");
        }
        Ok(SetupEnv {
            state_dir: PathBuf::from(state_dir),
            session,
            override_file: detect::override_path(
                &env("WERKBANK_SETUP_CONFIG"),
                &env("HERDR_PLUGIN_CONFIG_DIR"),
            ),
            herdr_bin: Some(env("HERDR_BIN_PATH"))
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| "herdr".into()),
        })
    }

    pub fn setup_log(&self) -> PathBuf {
        self.state_dir.join(format!(
            "setup-{}.log",
            crate::core::safe_session(&self.session)
        ))
    }

    pub fn notify_log(&self) -> PathBuf {
        self.state_dir.join(format!(
            "notifications-{}.log",
            crate::core::safe_session(&self.session)
        ))
    }

    fn jobs_dir(&self) -> PathBuf {
        self.state_dir.join("setup-jobs")
    }
}

/// Everything `setup-run` needs; the pane's shell inherits no hook env.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub branch: String,
    pub checkout: String,
    pub repo_root: String,
    pub copy_report: Vec<String>,
    pub commands: Vec<Cmd>,
    pub warning: Option<String>,
    pub env: SetupEnv,
}

fn append(path: &Path, line: &str) {
    let written = crate::core::open_private_append(path).and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = written {
        eprintln!("werkbank: {}: {e}", path.display());
    }
}

fn log_notification(path: &Path, branch: &str, body: &str, result: &Result<Value>) {
    let mut line = serde_json::json!({
        "ts": unix_ms() / 1000,
        "kind": "setup",
        "branch": branch,
        "title": TITLE,
        "body": body,
    });
    match result {
        Ok(v) => line["result"] = v.clone(),
        Err(e) => line["error"] = format!("{e:#}").into(),
    }
    append(path, &line.to_string());
}

fn notify<R: Runner>(
    herdr: &Herdr<R>,
    env: &SetupEnv,
    branch: &str,
    body: &str,
    sound: Sound,
) -> Result<Value> {
    let result = herdr.notify(TITLE, body, sound);
    log_notification(&env.notify_log(), branch, body, &result);
    result
}

fn job_id() -> String {
    let seed = format!(
        "{}-{:?}-{}",
        std::process::id(),
        SystemTime::now(),
        unix_ms()
    );
    format!("{:016x}", fnv1a(seed.as_bytes()))
}

fn write_job(env: &SetupEnv, job: &Job) -> Result<PathBuf> {
    fs::create_dir_all(env.jobs_dir())?;
    prune_jobs(&env.jobs_dir(), SystemTime::now());
    let path = env.jobs_dir().join(format!("{}.json", job_id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(serde_json::to_string_pretty(job)?.as_bytes())?;
    f.sync_all()?;
    Ok(path)
}

/// A job whose setup tab was closed before it ran is never deleted by `setup-run`;
/// drop any older than a day so the directory can't grow forever.
fn prune_jobs(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60));
        let is_job = entry.path().extension().is_some_and(|e| e == "json");
        if old && is_job {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub fn read_job(path: &Path) -> Result<Job> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// What `prepare` did: the copy outcome line and, if there is something to run, the job file.
pub struct Prepared {
    pub copy: String,
    pub job: Option<PathBuf>,
}

/// Copy, then detect.
pub fn prepare<R: Runner>(runner: &R, env: &SetupEnv, t: &Target) -> Result<Prepared> {
    crate::core::private_dir(&env.state_dir)?;
    let log = env.setup_log();
    let marker = marker_path(&env.state_dir, &t.checkout);
    let _ = fs::remove_file(&marker);
    append(
        &log,
        &format!("[{}] setup {} ({})", unix_ms() / 1000, t.branch, t.checkout),
    );
    let copied = copy::copy_local_files(runner, Path::new(&t.repo_root), Path::new(&t.checkout));
    let copy_report = match &copied {
        Ok(report) => {
            let mut lines = vec![format!("copy: {}", report.summary())];
            lines.extend(report.lines());
            lines
        }
        Err(e) => vec![format!("copy failed: {e:#}")],
    };
    for line in &copy_report {
        println!("{line}");
        append(&log, line);
    }
    // No marker after a failed copy, so a waiting launcher shows its "not copied" notice.
    if copied.is_ok() {
        fs::write(&marker, format!("{} {}\n", t.workspace_id, unix_ms()))?;
    }
    let detected = detect::detect(
        Path::new(&t.repo_root),
        Path::new(&t.checkout),
        env.override_file.as_deref(),
    );
    for w in &detected.warnings {
        println!("{w}");
        append(&log, w);
    }
    let copy_line = copy_report[0].clone();
    if detected.commands.is_empty() {
        append(&log, &format!("no setup commands: {}", t.checkout));
        return Ok(Prepared {
            copy: copy_line,
            job: None,
        });
    }
    let job = Job {
        branch: t.branch.clone(),
        checkout: t.checkout.clone(),
        repo_root: t.repo_root.clone(),
        copy_report,
        commands: detected.commands,
        warning: (!detected.warnings.is_empty()).then(|| detected.warnings.join("; ")),
        env: env.clone(),
    };
    Ok(Prepared {
        copy: copy_line,
        job: Some(write_job(env, &job)?),
    })
}

/// The exact command line typed into the setup pane.
pub fn run_line(exe: &Path, job: &Path) -> String {
    format!(
        "{} setup-run {}",
        sh_quote(&exe.to_string_lossy()),
        sh_quote(&job.to_string_lossy())
    )
}

/// Opens the `setup` tab running `setup-run <job>`. On failure the job is
/// deleted and the user is told setup could not start.
pub fn dispatch<R: Runner>(
    herdr: &Herdr<R>,
    env: &SetupEnv,
    t: &Target,
    job: &Path,
    exe: &Path,
    focus: bool,
) -> Result<()> {
    let started = herdr
        .tab_create_ids(&t.workspace_id, &t.checkout, "setup", focus)
        .and_then(|(tab, pane)| {
            herdr.pane_run(&pane, &run_line(exe, job)).inspect_err(|_| {
                // Don't leave an empty setup tab behind.
                let _ = herdr.tab_close(&tab);
            })
        });
    if let Err(e) = &started {
        let msg = format!("setup could not start: {} ({e:#})", t.branch);
        eprintln!("{msg}");
        append(&env.setup_log(), &msg);
        let _ = notify(
            herdr,
            env,
            &t.branch,
            &format!("setup could not start: {}", t.branch),
            Sound::Request,
        );
        let _ = fs::remove_file(job);
    }
    started
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok,
    Exit(i32),
    Signal(i32),
    NotFound,
    TimedOut(Duration),
    Error(String),
}

impl Outcome {
    fn reason(&self) -> String {
        match self {
            Outcome::Ok => "ok".into(),
            Outcome::Exit(n) => format!("exit {n}"),
            Outcome::Signal(s) => format!("signal {s}"),
            Outcome::NotFound => "not found".into(),
            Outcome::TimedOut(d) => match d.as_secs() {
                s if s >= 60 && s % 60 == 0 => format!("timed out after {}m", s / 60),
                s => format!("timed out after {s}s"),
            },
            Outcome::Error(e) => e.clone(),
        }
    }
}

pub fn outcome_line(cmd: &dyn std::fmt::Display, outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok => format!("ok: {cmd}"),
        other => format!("failed: {cmd} ({})", other.reason()),
    }
}

/// Secrets Werkbank reads, kept from repo-supplied setup scripts.
const SECRET_ENV: [&str; 2] = ["LINEAR_API_KEY", "SLACK_USER_TOKEN"];

/// The process for `cmd`, without Werkbank's secrets in its environment.
fn command(cmd: &Cmd) -> Option<Command> {
    let mut c = match cmd {
        Cmd::Argv(argv) => {
            let (program, args) = argv.split_first()?;
            let mut c = Command::new(program);
            c.args(args);
            c
        }
        Cmd::Shell(s) => {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", s]);
            c
        }
    };
    for k in SECRET_ENV {
        c.env_remove(k);
    }
    Some(c)
}

/// Runs one command in `cwd` with inherited stdio, so the tab shows its output.
pub fn exec(cmd: &Cmd, cwd: &Path) -> Outcome {
    exec_within(cmd, cwd, COMMAND_LIMIT)
}

/// [`exec`] with a time limit. The command stays in the tab's process group so
/// Ctrl+C in the setup tab still reaches it; on timeout it and its descendants
/// get SIGTERM, then SIGKILL after a grace period.
pub fn exec_within(cmd: &Cmd, cwd: &Path, limit: Duration) -> Outcome {
    let Some(mut c) = command(cmd) else {
        return Outcome::Error("empty command".into());
    };
    let mut child = match c.current_dir(cwd).spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Outcome::NotFound,
        Err(e) => return Outcome::Error(e.to_string()),
    };
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(200)),
            Ok(None) => {
                stop_tree(&mut child);
                return Outcome::TimedOut(limit);
            }
            Err(e) => return Outcome::Error(e.to_string()),
        }
    };
    match (status.code(), status.signal()) {
        (Some(0), _) => Outcome::Ok,
        (Some(n), _) => Outcome::Exit(n),
        (None, Some(s)) => Outcome::Signal(s),
        (None, None) => Outcome::Error("unknown exit".into()),
    }
}

/// `pid` and every descendant still attached to it, parents first.
fn process_tree(pid: i32) -> Vec<i32> {
    let mut tree = vec![pid];
    let mut i = 0;
    while i < tree.len() && tree.len() < 4096 {
        let p = tree[i].to_string();
        if let Ok(out) = RealRunner::default().run(&["pgrep", "-P", &p], None, None) {
            tree.extend(
                out.stdout
                    .lines()
                    .filter_map(|l| l.trim().parse::<i32>().ok()),
            );
        }
        i += 1;
    }
    tree
}

/// SIGTERM to the command and its descendants, SIGKILL to whatever is left
/// after the grace, then reaps the command.
fn stop_tree(child: &mut std::process::Child) {
    use crate::core::runner::{signal, SIGKILL, SIGTERM};
    let tree = process_tree(child.id() as i32);
    for &p in &tree {
        signal(p, SIGTERM);
    }
    let until = Instant::now() + TERM_GRACE;
    let mut root_done = false;
    while Instant::now() < until {
        root_done = root_done || matches!(child.try_wait(), Ok(Some(_)));
        // Descendants orphaned by the root's exit no longer show under it, so
        // the original pids are probed directly.
        if root_done && !tree[1..].iter().any(|&p| signal(p, 0)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for &p in &tree {
        signal(p, SIGKILL);
    }
    let _ = child.wait();
}

/// `setup-run <job>`: runs the job's commands in order, stops at the first
/// failure, notifies, and on success deletes the job and closes its own tab.
pub fn run_job<R: Runner>(
    herdr: &Herdr<R>,
    job_path: &Path,
    tab: Option<&str>,
    exec: &mut dyn FnMut(&Cmd, &Path) -> Outcome,
    close_delay: Duration,
) -> Result<bool> {
    let job = read_job(job_path)?;
    let log = job.env.setup_log();
    println!("werkbank setup for {} ({})", job.branch, job.checkout);
    for line in &job.copy_report {
        println!("  {line}");
    }
    if let Some(w) = &job.warning {
        println!("warning: {w}");
    }
    let mut failure = None;
    for (i, cmd) in job.commands.iter().enumerate() {
        println!("\n$ {cmd}");
        let outcome = exec(cmd, Path::new(&job.checkout));
        println!("{}", outcome_line(cmd, &outcome));
        // The log, like the notification, names override commands only by position.
        append(
            &log,
            &format!("[{}] {}", job.branch, outcome_line(&cmd.label(i), &outcome)),
        );
        if outcome != Outcome::Ok {
            failure = Some((cmd.label(i), outcome));
            break;
        }
    }
    let mut body = match &failure {
        None => format!("setup done: {}", job.branch),
        Some((label, outcome)) => format!(
            "setup failed: {} — {label} ({})",
            job.branch,
            outcome.reason()
        ),
    };
    if let Some(w) = &job.warning {
        body.push_str(&format!(" · {w}"));
    }
    let sound = if failure.is_some() {
        Sound::Request
    } else {
        Sound::None
    };
    if let Err(e) = notify(herdr, &job.env, &job.branch, &body, sound) {
        println!("notification failed: {e:#}");
    }
    println!("\n{body}");
    if failure.is_some() {
        println!(
            "job kept: {} — rerun with: werkbank setup-run {}",
            job_path.display(),
            sh_quote(&job_path.to_string_lossy())
        );
        return Ok(false);
    }
    let _ = fs::remove_file(job_path);
    if let Some(tab) = tab.filter(|t| !t.is_empty()) {
        std::thread::sleep(close_delay);
        herdr.tab_close(tab)?;
    }
    Ok(true)
}

/// `setup-worktree` target: context metadata when Herdr has it, else git on the focused cwd.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Linked { repo_root: String, checkout: String },
    NotLinked(String),
}

pub fn resolve_target<R: Runner>(runner: &R, ctx: &presets::Context) -> Result<Resolved> {
    if let Some(wt) = &ctx.worktree {
        return Ok(if wt.is_linked_worktree {
            Resolved::Linked {
                repo_root: wt.repo_root.clone(),
                checkout: wt.checkout_path.clone(),
            }
        } else {
            Resolved::NotLinked(wt.checkout_path.clone())
        });
    }
    let cwd = ctx
        .cwd()
        .ok_or_else(|| anyhow!("no cwd in the plugin context"))?;
    let out = runner.run(
        &[
            "git",
            "-C",
            cwd,
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
        None,
        None,
    )?;
    let lines: Vec<&str> = out.stdout.lines().collect();
    let [top, git_dir, common] = lines[..] else {
        return Ok(Resolved::NotLinked(cwd.to_string()));
    };
    if !out.success() || git_dir == common {
        return Ok(Resolved::NotLinked(top.to_string()));
    }
    let repo_root = Path::new(common)
        .parent()
        .ok_or_else(|| anyhow!("no parent for {common}"))?;
    Ok(Resolved::Linked {
        repo_root: repo_root.to_string_lossy().into_owned(),
        checkout: top.to_string(),
    })
}

fn current_branch<R: Runner>(runner: &R, checkout: &str) -> String {
    runner
        .run(
            &["git", "-C", checkout, "branch", "--show-current"],
            None,
            None,
        )
        .ok()
        .map(|o| o.stdout.trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "(detached)".to_string())
}

fn exe() -> Result<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

/// The `worktree-created` event hook.
pub fn run_hook() -> Result<()> {
    let env = SetupEnv::from_env()?;
    let Some(target) = parse_event(&crate::core::env("HERDR_PLUGIN_EVENT_JSON"))? else {
        println!("not a linked worktree; nothing to set up");
        return Ok(());
    };
    let herdr = Herdr::with_bin(RealRunner::default(), env.herdr_bin.clone())
        .for_session(env.session.clone());
    if let Some(job) = prepare(&herdr.runner, &env, &target)?.job {
        dispatch(&herdr, &env, &target, &job, &exe()?, false)?;
    }
    Ok(())
}

/// The `setup-worktree` action: same flow, on the focused linked worktree, tab focused.
pub fn run_action() -> Result<()> {
    let env = SetupEnv::from_env()?;
    let herdr = Herdr::with_bin(RealRunner::default(), env.herdr_bin.clone())
        .for_session(env.session.clone());
    let ctx = presets::Context::from_env()?;
    let (repo_root, checkout) = match resolve_target(&herdr.runner, &ctx)? {
        Resolved::Linked {
            repo_root,
            checkout,
        } => (repo_root, checkout),
        Resolved::NotLinked(path) => {
            let body = format!("setup-worktree: {path} is not a linked worktree; nothing done");
            let _ = herdr.notify(TITLE, &body, Sound::None);
            println!("{body}");
            return Ok(());
        }
    };
    let workspace_id = Some(ctx.workspace_id.clone())
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| crate::core::env("HERDR_WORKSPACE_ID"));
    let target = Target {
        branch: current_branch(&herdr.runner, &checkout),
        workspace_id,
        repo_root,
        checkout,
    };
    let prepared = prepare(&herdr.runner, &env, &target)?;
    match prepared.job {
        Some(job) => dispatch(&herdr, &env, &target, &job, &exe()?, true),
        None => {
            let body = format!(
                "setup-worktree: {} — {}; no setup commands",
                target.branch, prepared.copy
            );
            let _ = notify(&herdr, &env, &target.branch, &body, Sound::None);
            Ok(())
        }
    }
}

/// `setup-run <job>` in the setup pane.
pub fn run_setup(args: &[String]) -> Result<()> {
    let [job] = args else {
        bail!("usage: setup-run <job file>");
    };
    let job_path = Path::new(job);
    let parsed = read_job(job_path)?;
    let bin = Some(crate::core::env("HERDR_BIN_PATH"))
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| parsed.env.herdr_bin.clone());
    let herdr = Herdr::with_bin(RealRunner::default(), bin).for_session(parsed.env.session.clone());
    let tab = crate::core::env("HERDR_TAB_ID");
    run_job(&herdr, job_path, Some(&tab), &mut exec, CLOSE_DELAY).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::{FakeRunner, Output};
    use copy::tests::{repo, sh, tmpdir};

    const EVENT: &str = include_str!("../../../tests/fixtures/event-worktree-created.json");

    #[test]
    fn parses_the_recorded_event() {
        let t = parse_event(EVENT).unwrap().unwrap();
        assert_eq!(
            t,
            Target {
                workspace_id: "w2".into(),
                repo_root: "/tmp/repo".into(),
                checkout: "/home/user/.herdr/worktrees/repo/probe-br".into(),
                branch: "probe-br".into(),
            }
        );
        let mut main: Value = serde_json::from_str(EVENT).unwrap();
        main["data"]["worktree"]["is_linked_worktree"] = Value::Bool(false);
        assert_eq!(parse_event(&main.to_string()).unwrap(), None);
        assert!(parse_event("{").is_err());
    }

    #[test]
    fn quoting_survives_spaces_and_quotes() {
        let line = run_line(Path::new("/a b/it's/werkbank"), Path::new("/s d/jo'b.json"));
        assert_eq!(
            line,
            r"'/a b/it'\''s/werkbank' setup-run '/s d/jo'\''b.json'"
        );
        let out = Command::new("/bin/sh")
            .args([
                "-c",
                &format!("printf '%s\\n' {}", line.replacen(" setup-run ", " ", 1)),
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "/a b/it's/werkbank\n/s d/jo'b.json\n"
        );
    }

    #[test]
    fn marker_names_are_stable_and_canonical() {
        let dir = tmpdir("marker");
        let alias = tmpdir("marker-alias").join("l");
        std::os::unix::fs::symlink(&dir, &alias).unwrap();
        let a = marker_path(Path::new("/state"), &dir.to_string_lossy());
        let b = marker_path(Path::new("/state"), &alias.to_string_lossy());
        assert_eq!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("copied-") && name.len() == "copied-".len() + 16,
            "{name}"
        );
    }

    #[test]
    fn launcher_wait_needs_a_fresh_marker() {
        let state = tmpdir("wait");
        let checkout = tmpdir("wait-co");
        let co = checkout.to_string_lossy().into_owned();
        let marker = marker_path(&state, &co);
        fs::write(&marker, "old").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let since = SystemTime::now();
        let st = state.to_string_lossy().into_owned();
        let t0 = Instant::now();
        assert!(
            !await_copy(
                &st,
                &co,
                since,
                Duration::from_millis(300),
                Duration::from_millis(20)
            ),
            "old marker ignored"
        );
        assert!(t0.elapsed() >= Duration::from_millis(300));
        let writer = {
            let marker = marker.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                fs::write(marker, "new").unwrap();
            })
        };
        let since = SystemTime::now();
        let t0 = Instant::now();
        assert!(await_copy(
            &st,
            &co,
            since,
            Duration::from_secs(2),
            Duration::from_millis(20)
        ));
        assert!(t0.elapsed() < Duration::from_secs(1));
        writer.join().unwrap();
        assert!(
            await_copy("", &co, SystemTime::now(), COPY_WAIT, COPY_POLL),
            "no state dir → no wait"
        );
        assert_eq!(
            copy_notice("feat/x"),
            "local files not copied yet for feat/x — run setup-worktree if missing"
        );
    }

    fn setup_env(tag: &str, override_file: Option<PathBuf>) -> SetupEnv {
        SetupEnv {
            state_dir: tmpdir(tag).join("state"),
            session: "s1".into(),
            override_file,
            herdr_bin: "herdr".into(),
        }
    }

    #[test]
    fn prune_jobs_drops_only_day_old_job_files() {
        let dir = std::env::temp_dir().join(format!("werkbank-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in ["a.json", "keep.txt"] {
            fs::write(dir.join(name), "{}").unwrap();
        }
        prune_jobs(&dir, SystemTime::now());
        assert!(dir.join("a.json").exists(), "a fresh job stays");
        prune_jobs(&dir, SystemTime::now() + Duration::from_secs(25 * 60 * 60));
        assert!(!dir.join("a.json").exists(), "a day-old job goes");
        assert!(dir.join("keep.txt").exists(), "only job files are touched");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        let root = repo(
            tag,
            ".env\nnode_modules/\n",
            &[(
                "package.json",
                r#"{"packageManager":"npm@10","scripts":{"build":"x"}}"#,
            )],
        );
        let wt = root.parent().unwrap().join("wt dir");
        sh(
            &root,
            &format!(
                "git worktree add -q -b feat {}",
                sh_quote(&wt.to_string_lossy())
            ),
        );
        fs::write(root.join(".env"), "A=1").unwrap();
        (root, wt.canonicalize().unwrap())
    }

    fn target(root: &Path, wt: &Path) -> Target {
        Target {
            workspace_id: "w9".into(),
            repo_root: root.to_string_lossy().into(),
            checkout: wt.to_string_lossy().into(),
            branch: "feat".into(),
        }
    }

    #[test]
    fn prepare_copies_marks_and_writes_a_private_job() {
        let (root, wt) = scratch("prep");
        let env = setup_env("prep", None);
        fs::create_dir_all(&env.state_dir).unwrap();
        let marker = marker_path(&env.state_dir, &wt.to_string_lossy());
        fs::write(&marker, "stale").unwrap();
        let job = prepare(&RealRunner::default(), &env, &target(&root, &wt))
            .unwrap()
            .job
            .unwrap();
        assert!(wt.join(".env").is_file());
        assert_ne!(
            fs::read_to_string(&marker).unwrap(),
            "stale",
            "old marker replaced"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&job).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let parsed = read_job(&job).unwrap();
        assert_eq!(
            parsed.commands,
            vec![
                Cmd::Argv(vec!["npm".into(), "install".into()]),
                Cmd::Argv(vec!["npm".into(), "run".into(), "build".into()])
            ]
        );
        assert_eq!(parsed.env, env, "job carries session, state dir and logs");
        assert!(parsed.copy_report.iter().any(|l| l == "copied: .env"));
        let log = fs::read_to_string(env.setup_log()).unwrap();
        assert!(log.contains("copied: .env"), "{log}");
        assert!(!log.contains("A=1"));
    }

    #[test]
    fn empty_override_logs_no_setup_commands() {
        let (root, wt) = scratch("noop");
        let cfg = tmpdir("noop-cfg").join("setup.json");
        fs::write(
            &cfg,
            format!(r#"{{"repos":{{"{}":{{"commands":[]}}}}}}"#, root.display()),
        )
        .unwrap();
        let env = setup_env("noop", Some(cfg));
        let prepared = prepare(&RealRunner::default(), &env, &target(&root, &wt)).unwrap();
        assert_eq!(prepared.job, None);
        assert!(prepared.copy.starts_with("copy: "), "{}", prepared.copy);
        let log = fs::read_to_string(env.setup_log()).unwrap();
        assert!(
            log.contains(&format!("no setup commands: {}", wt.display())),
            "{log}"
        );
        assert!(wt.join(".env").is_file(), "copy still runs");
    }

    #[test]
    fn override_commands_are_tagged_shell() {
        let (root, wt) = scratch("shell");
        let cfg = tmpdir("shell-cfg").join("setup.json");
        fs::write(
            &cfg,
            format!(
                r#"{{"repos":{{"{}":{{"commands":["exit 3"]}}}}}}"#,
                root.display()
            ),
        )
        .unwrap();
        let env = setup_env("shell", Some(cfg));
        let job = prepare(&RealRunner::default(), &env, &target(&root, &wt))
            .unwrap()
            .job
            .unwrap();
        assert_eq!(
            read_job(&job).unwrap().commands,
            vec![Cmd::Shell("exit 3".into())]
        );
    }

    const TAB_REPLY: &str =
        r#"{"result":{"root_pane":{"pane_id":"w9:p7"},"tab":{"tab_id":"w9:t3"}}}"#;
    const SHOWN: &str = r#"{"result":{"shown":true}}"#;

    fn job_file(env: &SetupEnv, commands: Vec<Cmd>, warning: Option<&str>) -> PathBuf {
        let job = Job {
            branch: "feat".into(),
            checkout: env.state_dir.to_string_lossy().into(),
            repo_root: "/r".into(),
            copy_report: vec!["copy: 1 copied".into()],
            commands,
            warning: warning.map(str::to_string),
            env: env.clone(),
        };
        fs::create_dir_all(&env.state_dir).unwrap();
        write_job(env, &job).unwrap()
    }

    #[test]
    fn dispatch_opens_a_background_tab_with_a_quoted_command() {
        let env = setup_env("dispatch", None);
        let job = job_file(&env, vec![Cmd::Shell("true".into())], None);
        let fake = FakeRunner::new();
        fake.ok(
            &[
                "herdr",
                "--session",
                "s1",
                "tab",
                "create",
                "--workspace",
                "w9",
                "--cwd",
                "/co it's",
                "--label",
                "setup",
                "--no-focus",
            ],
            None,
            TAB_REPLY,
        );
        fake.otherwise("");
        let herdr = Herdr::with_bin(fake, "herdr").for_session("s1");
        let mut t = target(Path::new("/r"), Path::new("/co it's"));
        t.checkout = "/co it's".into();
        let exe = Path::new("/bin dir/we'rk");
        dispatch(&herdr, &env, &t, &job, exe, false).unwrap();
        let calls = herdr.runner.calls();
        let run = calls
            .iter()
            .find(|c| c.argv.get(3).map(String::as_str) == Some("pane"))
            .unwrap();
        assert_eq!(
            run.argv[..6],
            ["herdr", "--session", "s1", "pane", "run", "w9:p7"].map(String::from)
        );
        assert_eq!(run.argv[6], run_line(exe, &job));
        assert!(job.exists());
    }

    #[test]
    fn pane_run_failure_closes_the_new_tab() {
        let env = setup_env("prfail", None);
        let job = job_file(&env, vec![Cmd::Shell("true".into())], None);
        let exe = Path::new("/x");
        let fake = FakeRunner::new();
        let tab_argv = [
            "herdr",
            "--session",
            "s1",
            "tab",
            "create",
            "--workspace",
            "w9",
            "--cwd",
            "/co",
            "--label",
            "setup",
            "--no-focus",
        ];
        fake.ok(&tab_argv, None, TAB_REPLY);
        let line = run_line(exe, &job);
        fake.respond(
            &["herdr", "--session", "s1", "pane", "run", "w9:p7", &line],
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: "boom".into(),
            },
        );
        fake.otherwise(SHOWN);
        let herdr = Herdr::with_bin(fake, "herdr").for_session("s1");
        let t = target(Path::new("/r"), Path::new("/co"));
        assert!(dispatch(&herdr, &env, &t, &job, exe, false).is_err());
        assert!(herdr
            .runner
            .argvs()
            .iter()
            .any(|a| a == "herdr --session s1 tab close w9:t3"));
        assert!(!job.exists());
    }

    #[test]
    fn failed_copy_writes_no_marker() {
        let (_, wt) = scratch("nocopy");
        let not_git = tmpdir("nocopy-src");
        let env = setup_env("nocopy", None);
        fs::create_dir_all(&env.state_dir).unwrap();
        let marker = marker_path(&env.state_dir, &wt.to_string_lossy());
        let prepared = prepare(&RealRunner::default(), &env, &target(&not_git, &wt)).unwrap();
        assert!(
            prepared.copy.starts_with("copy failed"),
            "{}",
            prepared.copy
        );
        assert!(!marker.exists());
        let log = fs::read_to_string(env.setup_log()).unwrap();
        assert!(log.contains("copy failed"), "{log}");
    }

    #[test]
    fn dispatch_failure_deletes_the_job_and_says_so() {
        let env = setup_env("dfail", None);
        let job = job_file(&env, vec![Cmd::Shell("true".into())], None);
        let fake = FakeRunner::new();
        fake.respond(
            &[
                "herdr",
                "--session",
                "s1",
                "tab",
                "create",
                "--workspace",
                "w9",
                "--cwd",
                "/co",
                "--label",
                "setup",
                "--no-focus",
            ],
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: "boom".into(),
            },
        );
        fake.otherwise(SHOWN);
        let herdr = Herdr::with_bin(fake, "herdr").for_session("s1");
        assert!(dispatch(
            &herdr,
            &env,
            &target(Path::new("/r"), Path::new("/co")),
            &job,
            Path::new("/x"),
            false
        )
        .is_err());
        assert!(!job.exists());
        let log = fs::read_to_string(env.notify_log()).unwrap();
        assert!(log.contains("setup could not start: feat"), "{log}");
        assert!(herdr
            .runner
            .argvs()
            .iter()
            .any(|a| a.starts_with("herdr --session s1 notification show")));
    }

    fn run_with(
        env: &SetupEnv,
        commands: Vec<Cmd>,
        outcomes: Vec<Outcome>,
        notify_ok: bool,
        warning: Option<&str>,
    ) -> (bool, PathBuf, Vec<String>) {
        let job = job_file(env, commands, warning);
        let fake = FakeRunner::new();
        if notify_ok {
            fake.otherwise(SHOWN);
        } else {
            fake.respond(
                &[
                    "herdr",
                    "--session",
                    "s1",
                    "notification",
                    "show",
                    TITLE,
                    "--body",
                    "setup done: feat",
                    "--sound",
                    "none",
                ],
                None,
                Output {
                    status: 1,
                    stdout: String::new(),
                    stderr: "no".into(),
                },
            );
            fake.otherwise("");
        }
        let herdr = Herdr::with_bin(fake, "herdr").for_session("s1");
        let mut queue = outcomes.into_iter();
        let ok = run_job(
            &herdr,
            &job,
            Some("w9:t3"),
            &mut |_, _| queue.next().unwrap(),
            Duration::ZERO,
        )
        .unwrap();
        (ok, job, herdr.runner.argvs())
    }

    #[test]
    fn setup_run_success_notifies_deletes_the_job_and_closes_its_tab() {
        let env = setup_env("ok", None);
        let cmds = vec![
            Cmd::Argv(vec!["npm".into(), "install".into()]),
            Cmd::Argv(vec!["npm".into(), "run".into(), "build".into()]),
        ];
        let (ok, job, argvs) = run_with(
            &env,
            cmds,
            vec![Outcome::Ok, Outcome::Ok],
            true,
            Some("setup.json invalid: x"),
        );
        assert!(ok);
        assert!(!job.exists());
        assert!(
            argvs
                .iter()
                .any(|a| a == "herdr --session s1 tab close w9:t3"),
            "{argvs:?}"
        );
        let note: Value = serde_json::from_str(
            fs::read_to_string(env.notify_log())
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(note["kind"], "setup");
        assert_eq!(note["result"]["shown"], true);
        assert_eq!(note["body"], "setup done: feat · setup.json invalid: x");
        let log = fs::read_to_string(env.setup_log()).unwrap();
        assert!(
            log.contains("[feat] ok: npm install") && log.contains("[feat] ok: npm run build"),
            "{log}"
        );
    }

    #[test]
    fn setup_run_failures_keep_the_job_and_the_tab() {
        for (outcome, reason) in [
            (Outcome::Exit(3), "exit 3"),
            (Outcome::Signal(9), "signal 9"),
            (Outcome::NotFound, "not found"),
        ] {
            let env = setup_env("fail", None);
            let cmds = vec![Cmd::Shell("exit 3".into()), Cmd::Shell("never".into())];
            let (ok, job, argvs) = run_with(&env, cmds, vec![outcome], true, None);
            assert!(!ok);
            assert!(job.exists(), "job kept for a rerun");
            assert!(!argvs.iter().any(|a| a.contains("tab close")), "{argvs:?}");
            let log = fs::read_to_string(env.setup_log()).unwrap();
            assert!(
                log.contains(&format!("failed: setup.json command 1 ({reason})")),
                "{log}"
            );
            assert!(!log.contains("command 2"), "stops at the first failure");
            let notes = fs::read_to_string(env.notify_log()).unwrap();
            assert!(
                notes.contains(&format!(
                    "setup failed: feat — setup.json command 1 ({reason})"
                )),
                "{notes}"
            );
            assert!(!log.contains("exit 3 (") && !notes.contains("exit 3 ("));
        }
    }

    #[test]
    fn setup_run_still_closes_when_notifying_fails() {
        let env = setup_env("nofail", None);
        let (ok, job, argvs) = run_with(
            &env,
            vec![Cmd::Shell("true".into())],
            vec![Outcome::Ok],
            false,
            None,
        );
        assert!(ok && !job.exists());
        assert!(argvs
            .iter()
            .any(|a| a == "herdr --session s1 tab close w9:t3"));
        assert!(fs::read_to_string(env.notify_log())
            .unwrap()
            .contains("\"error\""));
    }

    #[test]
    fn setup_commands_never_see_werkbank_secrets() {
        for cmd in [Cmd::Argv(vec!["true".into()]), Cmd::Shell("true".into())] {
            let c = command(&cmd).unwrap();
            let removed: Vec<_> = c
                .get_envs()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
            assert_eq!(removed, SECRET_ENV, "{cmd:?}");
        }
        assert!(command(&Cmd::Argv(vec![])).is_none());
    }

    #[test]
    fn a_command_over_its_limit_is_stopped_with_its_children() {
        let dir = tmpdir("exec-limit");
        let pidfile = dir.join("child.pid");
        let cmd = Cmd::Shell(format!(
            "sleep 30 & echo $! > '{}'; wait",
            pidfile.display()
        ));
        let start = Instant::now();
        let out = exec_within(&cmd, &dir, Duration::from_millis(400));
        assert_eq!(out, Outcome::TimedOut(Duration::from_millis(400)));
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
        let child: i32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            !crate::core::runner::signal(child, 0)
        });
        assert!(gone, "child {child} survived");
        assert_eq!(
            outcome_line(&cmd.label(0), &Outcome::TimedOut(COMMAND_LIMIT)),
            format!("failed: {} (timed out after 10m)", cmd.label(0))
        );
    }

    #[test]
    fn real_exec_outcomes() {
        let dir = tmpdir("exec");
        assert_eq!(exec(&Cmd::Shell("true".into()), &dir), Outcome::Ok);
        assert_eq!(exec(&Cmd::Shell("exit 3".into()), &dir), Outcome::Exit(3));
        assert_eq!(
            exec(&Cmd::Shell("kill -9 $$".into()), &dir),
            Outcome::Signal(9)
        );
        assert_eq!(
            exec(&Cmd::Argv(vec!["wb-no-such-tool".into()]), &dir),
            Outcome::NotFound
        );
        assert_eq!(
            outcome_line(&Cmd::Shell("exit 3".into()), &Outcome::Exit(3)),
            "failed: exit 3 (exit 3)"
        );
        assert_eq!(
            outcome_line(
                &Cmd::Argv(vec!["npm".into(), "install".into()]),
                &Outcome::Ok
            ),
            "ok: npm install"
        );
    }

    fn ctx(json: &str) -> presets::Context {
        presets::Context::parse(json).unwrap()
    }

    #[test]
    fn setup_worktree_targets() {
        let fake = FakeRunner::new();
        let linked = ctx(
            r#"{"workspace_id":"w1","worktree":{"repo_root":"/r","checkout_path":"/wt","is_linked_worktree":true}}"#,
        );
        assert_eq!(
            resolve_target(&fake, &linked).unwrap(),
            Resolved::Linked {
                repo_root: "/r".into(),
                checkout: "/wt".into()
            }
        );
        let main = ctx(
            r#"{"workspace_id":"w1","worktree":{"repo_root":"/r","checkout_path":"/r","is_linked_worktree":false}}"#,
        );
        assert_eq!(
            resolve_target(&fake, &main).unwrap(),
            Resolved::NotLinked("/r".into())
        );

        let (root, wt) = scratch("resolve");
        let plain = ctx(&format!(
            r#"{{"workspace_id":"w1","focused_pane_cwd":"{}"}}"#,
            wt.display()
        ));
        assert_eq!(
            resolve_target(&RealRunner::default(), &plain).unwrap(),
            Resolved::Linked {
                repo_root: root.to_string_lossy().into(),
                checkout: wt.to_string_lossy().into()
            }
        );
        let on_main = ctx(&format!(
            r#"{{"workspace_id":"w1","focused_pane_cwd":"{}"}}"#,
            root.display()
        ));
        assert_eq!(
            resolve_target(&RealRunner::default(), &on_main).unwrap(),
            Resolved::NotLinked(root.to_string_lossy().into())
        );
        assert_eq!(
            current_branch(&RealRunner::default(), &wt.to_string_lossy()),
            "feat"
        );
    }
}
