use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::herdr::{Herdr, HerdrError, ReadSource, Sound, WorkspaceInfo};
use crate::core::runner::Runner;
use crate::core::stop::{Guard, Stopped};
use crate::features::cleanup::{self, linked_worktrees, CleanupPlan, CleanupView};
use crate::features::inbox::asking::{self, AskTracker};
use crate::features::inbox::issues::{self as issue_list, IssueData};
use crate::features::inbox::model::{
    duration_text, is_mastracode, kinds_of, stalled_text, AgentSnapshot, AttentionKind, InboxItem,
};
use crate::features::inbox::prs::{self, PrData};
use crate::features::inbox::stall::{self, StallRecord, StallTracker};
use crate::features::inbox::ui::Tab;
use crate::features::launch::issue::{self, ReadyWait, RepoChoice};
use crate::features::launch::pr::{self, PrCheckout};
use crate::features::setup;
use crate::integrations::github::{
    self, gh_lookup_batch, probe_branch, BranchKey, PrCache, PrState, BATCH,
};
use crate::integrations::linear::{
    self, Issue, IssueCache, MyIssue, Named, TeamState, WorkflowState,
};
use crate::integrations::mastracode::{self, Recap};
use crate::integrations::slack::Slack;

pub const TOKEN_TTL_MS: u64 = 90_000;
pub const TOKEN_REFRESH: Duration = Duration::from_secs(30);
const STALL_READ_LINES: u32 = 60;
const ASK_READ_LINES: u32 = 60;

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub stall_threshold: Duration,
    pub poll: Duration,
}

impl Settings {
    pub fn from_env() -> Self {
        let num = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        Self {
            stall_threshold: Duration::from_secs(num("WERKBANK_STALL_SECS", 300)),
            poll: Duration::from_millis(num("WERKBANK_POLL_MS", 2000).max(200)),
        }
    }
}

/// What the daemon observed in one tick; written to `state-<session>.json` for the Inbox.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub agents: Vec<AgentSnapshot>,
    /// Linked worktrees whose PR merged.
    pub merged: Vec<InboxItem>,
    /// Linear issues for the agents' branches, by issue id.
    pub issues: HashMap<String, Issue>,
    /// Last gh or herdr error, for the footer.
    pub error: Option<String>,
    /// No `LINEAR_API_KEY` was found.
    pub linear_missing_key: bool,
    pub ticked_unix_ms: u64,
    pub daemon_pid: u32,
    pub stall_secs: u64,
    pub poll_ms: u64,
}

/// Whether the Inbox can see a daemon keeping its state fresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WatcherStatus {
    /// The daemon holds its lock but has not written state yet.
    #[default]
    Waiting,
    /// No daemon holds the lock; the Inbox is starting one.
    Starting,
    /// The daemon holds its lock but its state is this many seconds old.
    Busy(u64),
    Running,
}

/// Everything the Inbox renders: the daemon's latest snapshot plus the
/// Inbox's own dialog state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WatchState {
    pub agents: Vec<AgentSnapshot>,
    /// Linked worktrees whose PR merged.
    pub merged: Vec<InboxItem>,
    /// When the daemon last ticked, as unix ms.
    pub ticked_unix_ms: Option<u64>,
    pub watcher: WatcherStatus,
    /// The daemon's stall threshold, when known.
    pub stall_secs: Option<u64>,
    /// Last gh or herdr error, for the footer.
    pub error: Option<String>,
    /// The latest cleanup plan, for the confirm dialog.
    pub cleanup: Option<CleanupView>,
    /// The result of the last cleanup step, for the footer.
    pub notice: Option<String>,
    /// Linear issues for the agents' branches, by issue id.
    pub issues: HashMap<String, Issue>,
    /// No `LINEAR_API_KEY` was found.
    pub linear_missing_key: bool,
    /// The fetched issue and repo choices for the launch dialog.
    pub launch: Option<LaunchView>,
    /// A launch is running; it can take up to a minute.
    pub launching: bool,
    /// The last finished issue lookup (serial, id), success or failure, so the
    /// dialog that asked for it can accept Enter again.
    pub fetched: Option<(u64, String)>,
    /// The last finished reply (serial, sent), so the reply box knows to close.
    pub replied: Option<(u64, bool)>,
    /// The last finished Factory post (serial, sent), so marks clear on success.
    pub factory_sent: Option<(u64, bool)>,
    pub prs: PrView,
    pub pr_detail: Option<DetailView>,
    pub issue_list: IssueView,
    pub issue_detail: Option<IssueDetailView>,
    pub states: Option<StatesView>,
}

/// The Issues tab's list: cached or fetched, and how the last fetch went.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueView {
    pub data: Option<Arc<IssueData>>,
    pub updating: bool,
    pub error: Option<String>,
}

/// One issue screen's fetch, tagged with the UI's request serial.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueDetailView {
    pub serial: u64,
    pub id: String,
    pub detail: Option<Arc<MyIssue>>,
    pub error: Option<String>,
}

/// The `c` picker's data: a team's states for one issue. A new serial opens it.
#[derive(Debug, Clone, PartialEq)]
pub struct StatesView {
    pub serial: u64,
    pub issue_id: String,
    pub identifier: String,
    pub current: String,
    pub states: Vec<TeamState>,
}

/// The PRs tab's lists: cached or fetched, and how the last fetch went.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrView {
    pub data: Option<Arc<PrData>>,
    /// A fetch is running; the shown data is the previous one.
    pub updating: bool,
    /// The last fetch failed; the shown data is older.
    pub error: Option<String>,
}

/// One PR screen's fetch, tagged with the UI's request serial so a late reply
/// for a PR no longer shown is dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailView {
    pub serial: u64,
    pub number: u64,
    pub detail: Option<Arc<github::PrDetail>>,
    pub error: Option<String>,
}

/// How often the PRs tab refetches while shown.
pub const PRS_EVERY: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq)]
pub struct LaunchView {
    pub serial: u64,
    pub issue: Issue,
    pub repos: Vec<RepoChoice>,
    /// `s` from the Issues tab: picking a repo starts this issue (claim
    /// included) instead of only launching it.
    pub start: Option<Box<MyIssue>>,
}

fn is_pane_gone(err: &anyhow::Error) -> bool {
    err.downcast_ref::<HerdrError>()
        .is_some_and(|e| e.code == "pane_not_found")
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Sent {
    pr: Option<String>,
    stalled: Option<String>,
    asking: Option<String>,
    at: Option<Instant>,
}

/// The `$asking` token value.
pub const ASKING_TOKEN: &str = "?";

/// Tokens last written to one pane, for the daemon's state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentRecord {
    pub pane: String,
    pub pr: Option<String>,
    pub stalled: Option<String>,
    #[serde(default)]
    pub asking: Option<String>,
}

/// The observer state a daemon hands to its successor, so a binary change
/// neither re-notifies nor restarts stall clocks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserverRecord {
    pub alerted: Vec<(String, AttentionKind)>,
    pub sent: Vec<SentRecord>,
    pub stall: Vec<StallRecord>,
    #[serde(default)]
    pub since: Vec<SinceRecord>,
}

/// When a pane's status/asking/stalled combination last changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinceRecord {
    pub pane: String,
    pub status: String,
    pub asking: bool,
    pub stalled: bool,
    pub since_ms: u64,
}

/// How often a pane's process is re-checked for its `--thread` without a title change.
const PROCESS_INFO_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often recaps are re-read while nothing changes.
const RECAP_EVERY: Duration = Duration::from_secs(20);
/// After a failed memory read, the next attempt comes this soon instead of a
/// full `RECAP_EVERY` later.
const RECAP_RETRY: Duration = Duration::from_secs(10);

/// What a pane's process said about its thread.
struct ThreadProbe {
    /// The terminal title when the process was checked.
    title: Option<String>,
    at: Instant,
    /// The `--thread` argument; `None` means the title decides.
    argv: Option<String>,
}

/// Which thread each pane runs, and the recaps of those threads.
#[derive(Default)]
struct ThreadMap {
    probes: HashMap<String, ThreadProbe>,
    /// The resolved thread per pane, as of the last recap refresh.
    threads: HashMap<String, String>,
    recaps: HashMap<String, Recap>,
    /// (pane, status, title) of every agent at the last refresh.
    seen: Vec<(String, String, Option<String>)>,
    refreshed: Option<Instant>,
}

/// Observes Mastra Code agents each tick: stall, ask, PR/CI, sidebar tokens,
/// notifications, merged worktrees and branch issues. Runs in the daemon.
pub struct Watcher<R: Runner> {
    pub herdr: Herdr<R>,
    settings: Settings,
    stop: Arc<AtomicBool>,
    stall: StallTracker,
    prs: PrCache,
    sent: HashMap<String, Sent>,
    ask: AskTracker,
    /// Alerts already sent for a condition that still holds; cleared to re-arm.
    alerted: HashSet<(String, AttentionKind)>,
    notify_log: Option<PathBuf>,
    last_error: Option<String>,
    linear_key: Option<String>,
    issues: IssueCache,
    /// Branch per checkout, valid for one tick; agents and worktrees often share one.
    branches: HashMap<String, Option<BranchKey>>,
    /// Branch per checkout across ticks, reused while its HEAD file reads the same.
    heads: HashMap<String, HeadMemo>,
    /// The current tick's `now`.
    now: Instant,
    /// Mastra Code's database; `None` turns recaps off.
    mastra_db: Option<PathBuf>,
    threads: ThreadMap,
    /// Per pane: (status, asking, stalled) and since when, as unix ms.
    since: HashMap<String, ((String, bool, bool), u64)>,
}

/// How long a directory git did not recognise is taken at its word.
const NOT_GIT_TTL: Duration = Duration::from_secs(30);

enum HeadMemo {
    /// A `git rev-parse` result and the HEAD file it depends on. `head` is the
    /// file's content read *before* that probe, so a switch during the probe is
    /// seen next tick; `None` until a probe ran with a known path.
    Git {
        path: PathBuf,
        head: Option<String>,
        key: Option<BranchKey>,
    },
    /// `git rev-parse` failed here (not a checkout, or git itself failed).
    NotGit { at: Instant },
}

/// `<toplevel>/.git/HEAD`, or the HEAD of a linked worktree's gitdir.
fn head_path(toplevel: &str) -> Option<PathBuf> {
    let dot = Path::new(toplevel).join(".git");
    if dot.is_dir() {
        return Some(dot.join("HEAD"));
    }
    let text = fs::read_to_string(&dot).ok()?;
    let gitdir = text.strip_prefix("gitdir:")?.trim();
    Some(Path::new(toplevel).join(gitdir).join("HEAD"))
}

impl<R: Runner> Watcher<R> {
    pub fn new(herdr: Herdr<R>, settings: Settings, stop: Arc<AtomicBool>) -> Self {
        Self {
            herdr,
            settings,
            stop,
            stall: StallTracker::default(),
            prs: PrCache::default(),
            sent: HashMap::new(),
            ask: AskTracker::default(),
            alerted: HashSet::new(),
            notify_log: None,
            last_error: None,
            linear_key: None,
            issues: IssueCache::default(),
            branches: HashMap::new(),
            heads: HashMap::new(),
            now: Instant::now(),
            mastra_db: None,
            threads: ThreadMap::default(),
            since: HashMap::new(),
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn export(&self, now: Instant, now_ms: u64) -> ObserverRecord {
        let mut alerted: Vec<_> = self.alerted.iter().cloned().collect();
        alerted.sort();
        let mut sent: Vec<SentRecord> = self
            .sent
            .iter()
            .map(|(pane, s)| SentRecord {
                pane: pane.clone(),
                pr: s.pr.clone(),
                stalled: s.stalled.clone(),
                asking: s.asking.clone(),
            })
            .collect();
        sent.sort_by(|a, b| a.pane.cmp(&b.pane));
        let mut since: Vec<SinceRecord> = self
            .since
            .iter()
            .map(
                |(pane, ((status, asking, stalled), since_ms))| SinceRecord {
                    pane: pane.clone(),
                    status: status.clone(),
                    asking: *asking,
                    stalled: *stalled,
                    since_ms: *since_ms,
                },
            )
            .collect();
        since.sort_by(|a, b| a.pane.cmp(&b.pane));
        ObserverRecord {
            alerted,
            sent,
            stall: self.stall.export(now, now_ms),
            since,
        }
    }

    /// Takes over a predecessor's observer state. Tokens are re-sent on the
    /// first tick, since when they were last sent is not carried over.
    pub fn seed(&mut self, record: &ObserverRecord, now: Instant, now_ms: u64) {
        self.alerted.extend(record.alerted.iter().cloned());
        for s in &record.sent {
            self.sent.insert(
                s.pane.clone(),
                Sent {
                    pr: s.pr.clone(),
                    stalled: s.stalled.clone(),
                    asking: s.asking.clone(),
                    at: None,
                },
            );
        }
        self.stall.seed(&record.stall, now, now_ms);
        for r in &record.since {
            self.since.insert(
                r.pane.clone(),
                ((r.status.clone(), r.asking, r.stalled), r.since_ms),
            );
        }
    }

    /// Reads recaps from this Mastra Code database.
    pub fn with_mastra_db(mut self, db: Option<PathBuf>) -> Self {
        self.mastra_db = db;
        self
    }

    pub fn with_linear_key(mut self, key: Option<String>) -> Self {
        self.linear_key = key;
        self
    }

    /// Appends one JSON line per notification to `path`.
    pub fn with_notify_log(mut self, path: PathBuf) -> Self {
        self.notify_log = Some(path);
        self
    }

    #[cfg(test)]
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.settings.poll = poll;
        self
    }

    /// Checked before every Runner call so a stop never waits on more than one call.
    fn guard(&self) -> Result<()> {
        if self.stop.load(Ordering::SeqCst) {
            Err(Stopped.into())
        } else {
            Ok(())
        }
    }

    pub fn tick(&mut self, now: Instant) -> Result<Snapshot> {
        self.now = now;
        self.branches.clear();
        self.guard()?;
        let agents: Vec<_> = self
            .herdr
            .agent_list()?
            .into_iter()
            .filter(is_mastracode)
            .collect();
        self.guard()?;
        let workspaces = self.herdr.workspace_list()?;
        let labels: HashMap<String, String> = workspaces
            .iter()
            .map(|w| (w.workspace_id.clone(), w.label.clone()))
            .collect();

        self.stall.retain(|p| agents.iter().any(|a| a.pane_id == p));
        self.ask.retain(|p| agents.iter().any(|a| a.pane_id == p));
        self.since
            .retain(|p, _| agents.iter().any(|a| &a.pane_id == p));
        self.threads
            .probes
            .retain(|p, _| agents.iter().any(|a| &a.pane_id == p));
        self.threads
            .threads
            .retain(|p, _| agents.iter().any(|a| &a.pane_id == p));
        self.alerted
            .retain(|(p, _)| agents.iter().any(|a| &a.pane_id == p));
        let gone: Vec<String> = self
            .sent
            .keys()
            .filter(|p| !agents.iter().any(|a| &a.pane_id == *p))
            .cloned()
            .collect();
        for pane in gone {
            self.guard()?;
            self.clear_pane(&pane);
        }

        let mut keys = Vec::new();
        for info in &agents {
            if let Some(cwd) = info.foreground_cwd.as_deref().or(info.cwd.as_deref()) {
                keys.extend(self.branch(cwd)?);
            }
        }
        for ws in linked_worktrees(&workspaces) {
            if let Some(wt) = &ws.worktree {
                keys.extend(self.branch(&wt.checkout_path)?);
            }
        }
        self.refresh_prs(&keys, now)?;
        self.refresh_recaps(&agents, now)?;

        let mut snapshots = Vec::new();
        for info in agents {
            match self.watch_agent(&info, &labels, now) {
                Ok(snap) => snapshots.push(snap),
                Err(err) if err.is::<Stopped>() => return Err(err),
                Err(err) if is_pane_gone(&err) => {
                    self.stall.retain(|p| p != info.pane_id);
                    self.ask.forget(&info.pane_id);
                    self.sent.remove(&info.pane_id);
                    self.since.remove(&info.pane_id);
                    self.threads.probes.remove(&info.pane_id);
                    self.threads.threads.remove(&info.pane_id);
                }
                Err(err) => self.last_error = Some(format!("{}: {err:#}", info.pane_id)),
            }
        }
        self.notify_transitions(&snapshots)?;
        let merged = self.discover_merged(&workspaces)?;
        let active: HashSet<BranchKey> = self.branches.values().flatten().cloned().collect();
        self.prs.evict_inactive(&active, now);
        let branches = &self.branches;
        self.heads.retain(|cwd, _| branches.contains_key(cwd));
        let issues = self.branch_issues(&snapshots, now)?;
        Ok(Snapshot {
            agents: snapshots,
            merged,
            issues,
            error: self.last_error.clone(),
            linear_missing_key: self.linear_key.is_none(),
            stall_secs: self.settings.stall_threshold.as_secs(),
            poll_ms: self.settings.poll.as_millis() as u64,
            ..Snapshot::default()
        })
    }

    fn branch(&mut self, cwd: &str) -> Result<Option<BranchKey>> {
        if let Some(key) = self.branches.get(cwd) {
            return Ok(key.clone());
        }
        let mut before = None;
        let mut known = None;
        match self.heads.get(cwd) {
            Some(HeadMemo::Git { path, head, key }) => {
                before = fs::read_to_string(path).ok();
                if before.is_some() && before == *head {
                    let key = key.clone();
                    self.branches.insert(cwd.to_string(), key.clone());
                    return Ok(key);
                }
                known = Some(path.clone());
            }
            Some(HeadMemo::NotGit { at })
                if self.now.saturating_duration_since(*at) < NOT_GIT_TTL =>
            {
                self.branches.insert(cwd.to_string(), None);
                return Ok(None);
            }
            _ => {}
        }
        self.guard()?;
        let probe = probe_branch(&self.herdr.runner, cwd);
        let memo = match &probe {
            None => Some(HeadMemo::NotGit { at: self.now }),
            Some((top, key)) => head_path(top).map(|path| HeadMemo::Git {
                head: before.filter(|_| known.as_ref() == Some(&path)),
                path,
                key: key.clone(),
            }),
        };
        let key = probe.and_then(|(_, key)| key);
        match memo {
            Some(memo) => self.heads.insert(cwd.to_string(), memo),
            None => self.heads.remove(cwd),
        };
        self.branches.insert(cwd.to_string(), key.clone());
        Ok(key)
    }

    /// Looks up every due branch with one `gh` call per repo (per `BATCH` branches),
    /// instead of one per branch. A failed call backs off all of its branches.
    fn refresh_prs(&mut self, keys: &[BranchKey], now: Instant) -> Result<()> {
        // repo → branch → every due checkout on it, in first-seen order.
        let mut by_repo: BTreeMap<&str, Vec<(&str, Vec<&BranchKey>)>> = BTreeMap::new();
        for key in keys.iter().filter(|k| self.prs.due(k, now)) {
            let branches = by_repo.entry(&key.repo).or_default();
            match branches.iter_mut().find(|(b, _)| *b == key.branch) {
                Some((_, ks)) if ks.contains(&key) => {}
                Some((_, ks)) => ks.push(key),
                None => branches.push((&key.branch, vec![key])),
            }
        }
        for branches in by_repo.values() {
            for chunk in branches.chunks(BATCH) {
                let names: Vec<&str> = chunk.iter().map(|(b, _)| *b).collect();
                self.guard()?;
                let toplevel = &chunk[0].1[0].toplevel;
                match gh_lookup_batch(&self.herdr.runner, toplevel, &names) {
                    Ok(prs) => {
                        for ((_, ks), pr) in chunk.iter().zip(prs) {
                            for key in ks {
                                self.prs.store((*key).clone(), pr.clone(), now);
                            }
                        }
                    }
                    Err(err) => {
                        for key in chunk.iter().flat_map(|(_, ks)| ks) {
                            self.prs.store_failure((*key).clone(), now);
                        }
                        self.last_error = Some(format!("{err:#}"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Issues named by the agents' branches; lookups are cached 5 min.
    fn branch_issues(
        &mut self,
        snapshots: &[AgentSnapshot],
        now: Instant,
    ) -> Result<HashMap<String, Issue>> {
        let mut ids: Vec<String> = snapshots
            .iter()
            .filter_map(|s| s.branch.as_deref().and_then(linear::issue_id_from_branch))
            .collect();
        ids.sort();
        ids.dedup();
        self.issues.evict_inactive(&ids, now);
        if let Some(key) = &self.linear_key {
            let due = self.issues.due(&ids, now);
            if !due.is_empty() {
                self.guard()?;
                match linear::fetch_issues(&self.herdr.runner, key, &due) {
                    Ok(found) => self.issues.store(&due, found, now),
                    Err(err) => {
                        self.issues.failed(now);
                        self.last_error = Some(format!("{err:#}"));
                    }
                }
            }
        }
        Ok(ids
            .into_iter()
            .filter_map(|id| self.issues.get(&id).map(|i| (id, i.clone())))
            .collect())
    }

    /// Linked worktrees on a branch whose PR merged; detached HEADs and the
    /// main checkout are never candidates. PRs come from `refresh_prs`.
    fn discover_merged(&mut self, workspaces: &[WorkspaceInfo]) -> Result<Vec<InboxItem>> {
        let mut items = Vec::new();
        for ws in linked_worktrees(workspaces) {
            let Some(wt) = &ws.worktree else { continue };
            let Some(key) = self.branch(&wt.checkout_path)? else {
                continue;
            };
            if let Some(pr) = self.prs.get(&key).filter(|p| p.state == PrState::Merged) {
                items.push(InboxItem::merged(
                    &ws.workspace_id,
                    &ws.label,
                    &wt.checkout_path,
                    &key.branch,
                    pr.clone(),
                ));
            }
        }
        Ok(items)
    }

    /// Re-resolves which thread each pane runs and re-reads their recaps: at most
    /// once per tick, when an agent's status or title changed or a minute passed.
    /// A `--thread` argument is authoritative; other panes are matched by title
    /// every refresh, and lose their thread when the match is not unique.
    fn refresh_recaps(
        &mut self,
        agents: &[crate::core::herdr::AgentInfo],
        now: Instant,
    ) -> Result<()> {
        let Some(db) = self.mastra_db.clone() else {
            return Ok(());
        };
        let mut thread_changed = false;
        for info in agents {
            let title = info.terminal_title_stripped.clone();
            let due =
                self.threads.probes.get(&info.pane_id).is_none_or(|p| {
                    p.title != title || now.duration_since(p.at) >= PROCESS_INFO_EVERY
                });
            if !due {
                continue;
            }
            self.guard()?;
            let argv = match self.herdr.pane_process_info(&info.pane_id) {
                Ok(procs) => procs.iter().find_map(|a| mastracode::thread_from_argv(a)),
                Err(err) if is_pane_gone(&err) => continue,
                Err(err) => {
                    self.last_error = Some(format!("{}: {err:#}", info.pane_id));
                    None
                }
            };
            let before = self.threads.probes.get(&info.pane_id).map(|p| &p.argv);
            thread_changed |= before.is_some_and(|b| *b != argv);
            self.threads.probes.insert(
                info.pane_id.clone(),
                ThreadProbe {
                    title,
                    at: now,
                    argv,
                },
            );
        }

        let mut seen: Vec<_> = agents
            .iter()
            .map(|a| {
                (
                    a.pane_id.clone(),
                    a.agent_status.clone(),
                    a.terminal_title_stripped.clone(),
                )
            })
            .collect();
        seen.sort();
        let fresh = self
            .threads
            .refreshed
            .is_some_and(|at| now.duration_since(at) < RECAP_EVERY);
        if fresh && !thread_changed && seen == self.threads.seen {
            return Ok(());
        }
        self.threads.seen = seen;
        self.threads.refreshed = Some(now);

        let mut threads = HashMap::new();
        let mut fallback = Vec::new();
        for info in agents {
            let Some(probe) = self.threads.probes.get(&info.pane_id) else {
                continue;
            };
            if let Some(id) = &probe.argv {
                threads.insert(info.pane_id.clone(), id.clone());
                continue;
            }
            let title = info
                .terminal_title_stripped
                .as_deref()
                .and_then(mastracode::title_from_terminal);
            let cwd = info.foreground_cwd.as_deref().or(info.cwd.as_deref());
            if let (Some(title), Some(cwd)) = (title, cwd) {
                fallback.push((info.pane_id.clone(), (title.to_string(), cwd.to_string())));
            }
        }
        let read = (|| -> Result<HashMap<String, Recap>> {
            if !fallback.is_empty() {
                self.guard()?;
                let asks: Vec<_> = fallback.iter().map(|(_, q)| q.clone()).collect();
                let found = mastracode::lookup_by_titles(&self.herdr.runner, &db, &asks)?;
                let claimed: Vec<String> = threads.values().cloned().collect();
                for ((pane, _), id) in fallback.iter().zip(found) {
                    if let Some(id) = id.filter(|id| !claimed.contains(id)) {
                        threads.insert(pane.clone(), id);
                    }
                }
            }
            let ids: Vec<String> = threads.values().cloned().collect();
            if ids.is_empty() {
                return Ok(HashMap::new());
            }
            self.guard()?;
            let rows = mastracode::fetch_recaps(&self.herdr.runner, &db, &ids)?;
            Ok(rows
                .iter()
                .map(|(id, row)| (id.clone(), Recap::from_row(row)))
                .collect())
        })();
        match read {
            Ok(recaps) => {
                self.threads.threads = threads;
                self.threads.recaps = recaps;
            }
            Err(err) if err.is::<Stopped>() => return Err(err),
            Err(err) => {
                self.last_error = Some(format!("mastra code memory: {err:#}"));
                self.threads.refreshed = now.checked_sub(RECAP_EVERY - RECAP_RETRY);
            }
        }
        Ok(())
    }

    fn watch_agent(
        &mut self,
        info: &crate::core::herdr::AgentInfo,
        labels: &HashMap<String, String>,
        now: Instant,
    ) -> Result<AgentSnapshot> {
        let pane = info.pane_id.as_str();
        // A pending ask_user question has been seen reported as idle, done and
        // working, so every non-blocked status gets a visible-screen check.
        if matches!(info.agent_status.as_str(), "idle" | "done" | "working") {
            if self.ask.read_due(pane, now) {
                self.guard()?;
                let screen = self
                    .herdr
                    .pane_read(pane, ReadSource::Visible, ASK_READ_LINES)?;
                self.ask.observe(pane, &screen, now);
            }
        } else {
            self.ask.forget(pane);
        }
        let asking = self.ask.asking(pane);
        // Herdr's status comes from Mastra Code's hook, which can miss a turn
        // starting (goal-mode continuations stay `idle`); the running-turn
        // footer on screen corrects it.
        let mut info = info.clone();
        if matches!(info.agent_status.as_str(), "idle" | "done") && self.ask.busy(pane) {
            info.agent_status = "working".to_string();
        }
        let info = &info;
        // Waiting on the user is not a stall.
        let working = info.agent_status == "working" && !asking;
        let screen = if working && self.stall.read_due(pane, now) {
            self.guard()?;
            Some(
                self.herdr
                    .pane_read(pane, ReadSource::Recent, STALL_READ_LINES)?,
            )
        } else {
            None
        };
        let status = if asking { "asking" } else { &info.agent_status };
        self.stall.observe(pane, status, screen.as_deref(), now);
        let stalled = self.stall.stalled(pane, now, self.settings.stall_threshold);

        let cwd = info.foreground_cwd.as_deref().or(info.cwd.as_deref());
        let key = match cwd {
            Some(cwd) => self.branch(cwd)?,
            None => None,
        };
        let pr = key.as_ref().and_then(|k| self.prs.get(k)).cloned();

        // A failed token write must not hide the agent from the Inbox.
        if let Err(err) = self.sync_tokens(
            pane,
            Tokens {
                pr: pr.as_ref().map(|p| p.token()),
                stalled: stalled.map(stalled_text),
                asking: asking.then(|| ASKING_TOKEN.to_string()),
            },
            now,
        ) {
            if err.is::<Stopped>() || is_pane_gone(&err) {
                return Err(err);
            }
            self.last_error = Some(format!("{pane}: {err:#}"));
        }
        let state = (info.agent_status.clone(), asking, stalled.is_some());
        let state_since_unix_ms = match self.since.get(pane) {
            Some((s, at)) if *s == state => *at,
            _ => {
                let at = crate::core::unix_ms();
                self.since.insert(pane.to_string(), (state, at));
                at
            }
        };
        let thread = self.threads.threads.get(pane).cloned();
        Ok(AgentSnapshot {
            info: info.clone(),
            workspace_label: labels.get(&info.workspace_id).cloned().unwrap_or_default(),
            branch: key.map(|k| k.branch),
            pr,
            stalled,
            asking,
            recap: thread
                .as_ref()
                .and_then(|t| self.threads.recaps.get(t))
                .cloned(),
            thread,
            question: if asking {
                self.ask.question(pane)
            } else {
                None
            },
            stall_hash: stalled.and_then(|_| self.stall.screen_hash(pane)),
            state_since_unix_ms,
        })
    }

    /// Notifies once when an agent becomes Stalled or CI-failed, and re-arms
    /// when the condition clears. Other kinds never notify; Herdr covers them.
    fn notify_transitions(&mut self, snapshots: &[AgentSnapshot]) -> Result<()> {
        for a in snapshots {
            let kinds = kinds_of(a);
            for kind in [AttentionKind::Stalled, AttentionKind::CiFailed] {
                let key = (a.info.pane_id.clone(), kind);
                if !kinds.contains(&kind) {
                    self.alerted.remove(&key);
                    continue;
                }
                if self.alerted.contains(&key) {
                    continue;
                }
                let (title, body) = match kind {
                    AttentionKind::Stalled => (
                        format!("Stalled: {}/{}", a.workspace_label, a.info.tab_id),
                        format!(
                            "{} — no output for {}",
                            a.info
                                .terminal_title_stripped
                                .as_deref()
                                .unwrap_or(&a.info.pane_id),
                            duration_text(a.stalled.unwrap_or_default())
                        ),
                    ),
                    _ => (
                        format!("CI failed: #{}", a.pr.as_ref().map_or(0, |p| p.number)),
                        format!(
                            "{} {}",
                            a.workspace_label,
                            a.branch.as_deref().unwrap_or_default()
                        ),
                    ),
                };
                self.guard()?;
                let result = self.herdr.notify(&title, &body, Sound::Request);
                if let Err(err) = &result {
                    self.last_error = Some(format!("notify: {err:#}"));
                }
                self.log_notification(kind, &a.info.pane_id, &title, &body, &result);
                self.alerted.insert(key);
            }
        }
        Ok(())
    }

    fn log_notification(
        &mut self,
        kind: AttentionKind,
        pane: &str,
        title: &str,
        body: &str,
        result: &Result<serde_json::Value>,
    ) {
        let Some(path) = &self.notify_log else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut line = serde_json::json!({
            "ts": ts,
            "kind": match kind { AttentionKind::Stalled => "stalled", _ => "ci_failed" },
            "pane": pane,
            "title": title,
            "body": body,
        });
        match result {
            Ok(v) => line["result"] = v.clone(),
            Err(err) => line["error"] = format!("{err:#}").into(),
        }
        let written =
            crate::core::open_private_append(path).and_then(|mut f| writeln!(f, "{line}"));
        if let Err(err) = written {
            self.last_error = Some(format!("notification log: {err}"));
        }
    }

    /// Clears tokens that went away at once, and (re)sends present ones when they
    /// change or every 30s so their 90s TTL never lapses while the daemon runs.
    fn sync_tokens(&mut self, pane: &str, tokens: Tokens, now: Instant) -> Result<()> {
        let prev = self.sent.get(pane).cloned().unwrap_or_default();
        let pairs = [
            ("pr", &prev.pr, &tokens.pr),
            ("stalled", &prev.stalled, &tokens.stalled),
            ("asking", &prev.asking, &tokens.asking),
        ];
        let clear: Vec<&str> = pairs
            .iter()
            .filter(|(_, old, new)| old.is_some() && new.is_none())
            .map(|(name, _, _)| *name)
            .collect();
        if !clear.is_empty() {
            self.guard()?;
            self.herdr.clear_tokens(pane, &clear)?;
        }
        let set: Vec<(&str, &str)> = pairs
            .iter()
            .filter_map(|(name, _, new)| new.as_deref().map(|v| (*name, v)))
            .collect();
        // A token that only went away was handled by the clear above.
        let changed = pairs
            .iter()
            .any(|(_, old, new)| new.is_some() && old != new);
        let stale = prev
            .at
            .is_none_or(|at| now.duration_since(at) >= TOKEN_REFRESH);
        let mut at = prev.at;
        if !set.is_empty() && (changed || stale) {
            self.guard()?;
            self.herdr.set_tokens(pane, &set, TOKEN_TTL_MS)?;
            at = Some(now);
        }
        if tokens.pr.is_none() && tokens.stalled.is_none() && tokens.asking.is_none() {
            self.sent.remove(pane);
        } else {
            self.sent.insert(
                pane.to_string(),
                Sent {
                    pr: tokens.pr,
                    stalled: tokens.stalled,
                    asking: tokens.asking,
                    at,
                },
            );
        }
        Ok(())
    }

    fn clear_pane(&mut self, pane: &str) {
        if let Some(sent) = self.sent.remove(pane) {
            let names: Vec<&str> = [
                ("pr", &sent.pr),
                ("stalled", &sent.stalled),
                ("asking", &sent.asking),
            ]
            .into_iter()
            .filter(|(_, v)| v.is_some())
            .map(|(n, _)| n)
            .collect();
            // The pane may already be gone; its tokens then went with it.
            let _ = self.herdr.clear_tokens(pane, &names);
        }
    }
}

/// The sidebar tokens one pane should carry.
struct Tokens {
    pr: Option<String>,
    stalled: Option<String>,
    asking: Option<String>,
}

/// Requests from the Inbox UI thread; every Runner call happens on its worker.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Refresh,
    Focus(String),
    OpenUrl(String),
    /// Build and check a cleanup plan for a workspace.
    PlanCleanup(String),
    /// Re-check, then remove the planned worktree.
    ExecuteCleanup(CleanupPlan),
    /// Look up a pasted Linear issue id for the launch dialog.
    FetchIssue(String),
    /// Create the issue's worktree in a repo, start Mastra Code, send the issue.
    Launch {
        issue: Issue,
        repo_root: String,
    },
    /// Re-check the agent, then send it `text`. `stall_hash` is the snapshot's,
    /// so a stalled agent only gets the reply while its screen is unchanged.
    Reply {
        pane: String,
        text: String,
        stall_hash: Option<u64>,
    },
    /// The top tab shown: PR lists are fetched only while theirs is.
    TabVisible(Tab),
    /// Refetch the PR lists (`r` on the PRs tab).
    FetchPrs,
    /// The PR screen's data; `serial` is the UI's request number.
    FetchPrDetail {
        serial: u64,
        repo: String,
        number: u64,
    },
    /// Start Mastra Code on the PR's worktree (made if needed) with `prompt`.
    StartPr {
        repo: String,
        head_repo: Option<String>,
        head: String,
        number: u64,
        prompt: String,
    },
    /// Rerun the failed jobs of the PR head's failed Actions runs.
    RerunFailed {
        repo: String,
        head: String,
        number: u64,
    },
    /// Focus the PR's worktree, or fetch its branch and create one.
    OpenPrWorktree {
        repo: String,
        head_repo: Option<String>,
        head: String,
        number: u64,
    },
    /// Refetch the issue list (`r` on the Issues tab).
    FetchIssues,
    /// The issue screen's data; `serial` is the UI's request number.
    FetchIssueDetail {
        serial: u64,
        id: String,
    },
    /// `c`: the issue's team states, for the picker.
    FetchTeamStates {
        issue_id: String,
        identifier: String,
        team_id: String,
        current: String,
    },
    /// Move the issue to `state`.
    SetIssueState {
        issue_id: String,
        identifier: String,
        state: WorkflowState,
    },
    /// `s`: re-read the issue, refuse a closed or someone else's, offer repos.
    PrepareStart {
        id: String,
        identifier: String,
    },
    /// Worktree → re-read and re-check → claim → first prompt.
    Start {
        issue: Box<MyIssue>,
        repo_root: String,
    },
    /// Post the composed message to the Factory Slack channel.
    FactorySend {
        message: crate::features::factory::prompt::Message,
    },
}

impl Command {
    /// Only reads: quitting may abandon it mid-way. Anything that changes a
    /// worktree, an agent, GitHub, Linear or Slack is waited for.
    pub fn read_only(&self) -> bool {
        matches!(
            self,
            Command::Refresh
                | Command::Focus(_)
                | Command::OpenUrl(_)
                | Command::PlanCleanup(_)
                | Command::FetchIssue(_)
                | Command::TabVisible(_)
                | Command::FetchPrs
                | Command::FetchPrDetail { .. }
                | Command::FetchIssues
                | Command::FetchIssueDetail { .. }
                | Command::FetchTeamStates { .. }
                | Command::PrepareStart { .. }
        )
    }
}

/// Runs the Inbox's interactive commands. Lives in the Inbox process, since
/// they need the user present; observation lives in the daemon.
pub struct Actions<R: Runner> {
    pub herdr: Herdr<R>,
    stop: Arc<AtomicBool>,
    linear_key: Option<String>,
    slack: Option<Slack>,
    ready_wait: ReadyWait,
    pub last_error: Option<String>,
    pub cleanup: Option<CleanupView>,
    plan_serial: u64,
    pub notice: Option<String>,
    pub launch: Option<LaunchView>,
    launch_serial: u64,
    pub fetched: Option<(u64, String)>,
    pub replied: Option<(u64, bool)>,
    pub factory_sent: Option<(u64, bool)>,
    pub prs: PrView,
    pr_cache: Option<PathBuf>,
    visible: Tab,
    prs_wanted: bool,
    prs_fetched_at: Option<Instant>,
    pub pr_detail: Option<DetailView>,
    pub issue_list: IssueView,
    issue_cache: Option<PathBuf>,
    issues_wanted: bool,
    issues_fetched_at: Option<Instant>,
    pub issue_detail: Option<IssueDetailView>,
    pub states: Option<StatesView>,
    /// Where PR worktrees go, and the setup run on a fresh same-repo one.
    worktrees: (PathBuf, pr::Setup),
}

fn stop_guard(stop: Arc<AtomicBool>) -> impl Fn() -> Result<()> {
    move || {
        if stop.load(Ordering::SeqCst) {
            Err(Stopped.into())
        } else {
            Ok(())
        }
    }
}

/// Why `s` won't start this issue: it is closed, or someone else's.
fn start_refusal(issue: &MyIssue, viewer_id: &str) -> Option<String> {
    if issue.state.closed() {
        return Some(format!("it is {}", issue.state.name));
    }
    match &issue.assignee {
        Some(a) if a.id != viewer_id => Some(format!("it is assigned to {}", a.name)),
        _ => None,
    }
}

impl<R: Runner> Actions<R> {
    /// Whether the Inbox is quitting.
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn new(herdr: Herdr<R>, stop: Arc<AtomicBool>) -> Self {
        Self {
            herdr,
            stop,
            linear_key: None,
            slack: None,
            ready_wait: ReadyWait::DEFAULT,
            last_error: None,
            cleanup: None,
            plan_serial: 0,
            notice: None,
            launch: None,
            launch_serial: 0,
            fetched: None,
            replied: None,
            factory_sent: None,
            prs: PrView::default(),
            pr_cache: None,
            visible: Tab::Agents,
            prs_wanted: false,
            prs_fetched_at: None,
            pr_detail: None,
            issue_list: IssueView::default(),
            issue_cache: None,
            issues_wanted: false,
            issues_fetched_at: None,
            issue_detail: None,
            states: None,
            worktrees: if cfg!(test) {
                (PathBuf::from("/wt"), |_| None)
            } else {
                (pr::worktrees_dir(), setup::after_open)
            },
        }
    }

    /// Shows the last fetched issues at once, before any fetch.
    pub fn with_issue_cache(mut self, path: PathBuf) -> Self {
        self.issue_list.data = issue_list::load_cache(&path).map(Arc::new);
        self.issue_cache = Some(path);
        self
    }

    /// An issue fetch is due: the tab is shown and it was asked for or is 120 s old.
    pub fn issues_due(&self, now: Instant) -> bool {
        self.visible == Tab::Issues
            && (self.issues_wanted
                || self
                    .issues_fetched_at
                    .is_none_or(|t| now.duration_since(t) >= PRS_EVERY))
    }

    pub fn start_issues_fetch(&mut self) {
        self.issue_list.updating = true;
    }

    /// Fetches the viewer's open issues plus the open checkouts; a failure
    /// keeps the previous list and says so.
    pub fn fetch_issues(&mut self) {
        let guard = stop_guard(self.stop.clone());
        self.issues_wanted = false;
        self.issues_fetched_at = Some(Instant::now());
        let fetched = match self.linear_key.as_deref() {
            None => Err(anyhow::anyhow!("LINEAR_API_KEY not set — see README")),
            Some(key) => guard()
                .and_then(|()| linear::fetch_my_issues(&self.herdr.runner, key))
                .map(|mine| IssueData {
                    mine,
                    checkouts: prs::checkouts(&self.herdr, &guard).unwrap_or_default(),
                    fetched_unix_ms: crate::core::unix_ms(),
                }),
        };
        self.issue_list.updating = false;
        match fetched {
            Ok(data) => {
                self.issue_list.error = None;
                self.save_issues(&data);
                self.issue_list.data = Some(Arc::new(data));
            }
            Err(err) => self.issue_list.error = Some(format!("{err:#}")),
        }
    }

    fn save_issues(&mut self, data: &IssueData) {
        if let Some(path) = &self.issue_cache {
            if let Err(err) = issue_list::save_cache(path, data) {
                self.last_error = Some(format!("{err:#}"));
            }
        }
    }

    /// Applies a state and/or assignee Linear accepted to the list, the cache
    /// and the open issue screen, without a refetch that could read a stale value.
    fn patch_issue(&mut self, id: &str, state: Option<&WorkflowState>, assignee: Option<&Named>) {
        let patch = |i: &mut MyIssue| {
            if let Some(s) = state {
                i.state = s.clone();
            }
            if let Some(a) = assignee {
                i.assignee = Some(a.clone());
            }
        };
        if let Some(data) = &mut self.issue_list.data {
            let data = Arc::make_mut(data);
            data.mine
                .issues
                .iter_mut()
                .filter(|i| i.id == id)
                .for_each(patch);
            let data = data.clone();
            self.save_issues(&data);
        }
        if let Some(d) = self.issue_detail.as_mut().and_then(|v| v.detail.as_mut()) {
            let d = Arc::make_mut(d);
            if d.id == id {
                patch(d);
            }
        }
    }

    fn key(&self) -> Result<String> {
        self.linear_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("LINEAR_API_KEY not set — see README"))
    }

    /// `s`'s first read: refuses a closed or someone else's issue, then the
    /// repo choices for the picker.
    fn prepare_start(&mut self, id: &str, guard: Guard) -> Result<LaunchView> {
        let key = self.key()?;
        guard()?;
        let read = linear::fetch_issue_detail(&self.herdr.runner, &key, id)?;
        if let Some(why) = start_refusal(&read.issue, &read.viewer_id) {
            anyhow::bail!("{why}; not starting");
        }
        issue::branch_of(&read.issue.as_issue())?;
        guard()?;
        let workspaces = self.herdr.workspace_list()?;
        let repos = issue::repo_choices(&self.herdr, &workspaces, guard)?;
        if repos.is_empty() {
            anyhow::bail!("no open workspace on a main git checkout to start in");
        }
        self.launch_serial += 1;
        Ok(LaunchView {
            serial: self.launch_serial,
            issue: read.issue.as_issue(),
            repos,
            start: Some(Box::new(read.issue)),
        })
    }

    /// herdr-linear's order: worktree → re-read and re-check (same branch and
    /// team, still open, still ours) → claim → first prompt. A failure stops
    /// the later steps and the notice says what did and didn't happen.
    fn start_issue(&mut self, first: &MyIssue, repo_root: &str, guard: Guard) -> String {
        let name = &first.identifier;
        let key = match self.key() {
            Ok(k) => k,
            Err(err) => return format!("start {name}: {err:#}"),
        };
        let prepared = match issue::prepare_worktree(
            &self.herdr,
            &first.as_issue(),
            repo_root,
            self.ready_wait,
            guard,
        ) {
            Ok(p) => p,
            Err(err) => {
                return format!("start {name}: {err:#} — Linear not updated, no prompt sent")
            }
        };
        let not_claimed = "worktree created, Linear not updated, no prompt sent";
        let fresh = match guard()
            .and_then(|()| linear::fetch_issue_detail(&self.herdr.runner, &key, &first.id))
        {
            Ok(d) => d,
            Err(err) => return format!("start {name}: re-read failed: {err:#} — {not_claimed}"),
        };
        let changed = start_refusal(&fresh.issue, &fresh.viewer_id).or_else(|| {
            if fresh.issue.branch_name != first.branch_name {
                Some("its branch changed".to_string())
            } else if fresh.issue.team.id != first.team.id {
                Some("it moved to another team".to_string())
            } else {
                None
            }
        });
        if let Some(why) = changed {
            return format!(
                "start {name}: issue changed while preparing ({why}): worktree kept, Linear not updated, no prompt sent"
            );
        }
        let started = if fresh.issue.state.kind == "started" {
            None
        } else {
            let states = guard()
                .and_then(|()| linear::team_states(&self.herdr.runner, &key, &fresh.issue.team.id));
            match states.map(|s| s.into_iter().find(|s| s.kind == "started")) {
                Ok(Some(s)) => Some(WorkflowState {
                    id: s.id,
                    name: s.name,
                    kind: s.kind,
                }),
                Ok(None) => {
                    return format!("start {name}: the team has no started state — {not_claimed}")
                }
                Err(err) => return format!("start {name}: {err:#} — {not_claimed}"),
            }
        };
        let assignee = fresh
            .issue
            .assignee
            .is_none()
            .then(|| fresh.viewer_id.clone());
        let claimed = guard().and_then(|()| {
            linear::claim(
                &self.herdr.runner,
                &key,
                &first.id,
                started.as_ref().map(|s| s.id.as_str()),
                assignee.as_deref(),
            )
        });
        if let Err(err) = claimed {
            return format!("start {name}: claim failed: {err:#} — {not_claimed}");
        }
        let claimed_by = assignee.map(|id| Named {
            id,
            name: fresh.viewer_name.clone(),
        });
        if started.is_some() || claimed_by.is_some() {
            self.patch_issue(&first.id, started.as_ref(), claimed_by.as_ref());
        }
        match issue::send_first_prompt(&self.herdr, &fresh.issue.as_issue(), &prepared, guard) {
            Ok(done) => done,
            Err(err) => {
                format!("start {name}: worktree created, Linear updated, prompt failed: {err:#}")
            }
        }
    }

    /// Shows the last fetched PR lists at once, before any fetch.
    pub fn with_pr_cache(mut self, path: PathBuf) -> Self {
        self.prs.data = prs::load_cache(&path).map(Arc::new);
        self.pr_cache = Some(path);
        self
    }

    /// A PR fetch is due: the tab is shown and it was asked for or is 120 s old.
    pub fn prs_due(&self, now: Instant) -> bool {
        self.visible == Tab::Prs
            && (self.prs_wanted
                || self
                    .prs_fetched_at
                    .is_none_or(|t| now.duration_since(t) >= PRS_EVERY))
    }

    /// Marks the shown lists as being refreshed, for the UI before a fetch.
    pub fn start_prs_fetch(&mut self) {
        self.prs.updating = true;
    }

    /// Fetches both PR searches plus the open checkouts; a failure keeps the
    /// previous lists and says so.
    pub fn fetch_prs(&mut self) {
        let stop = self.stop.clone();
        let guard = move || -> Result<()> {
            if stop.load(Ordering::SeqCst) {
                Err(Stopped.into())
            } else {
                Ok(())
            }
        };
        self.prs_wanted = false;
        self.prs_fetched_at = Some(Instant::now());
        let fetched = guard()
            .and_then(|()| github::fetch_pr_lists(&self.herdr.runner))
            .map(|lists| PrData {
                lists,
                checkouts: prs::checkouts(&self.herdr, &guard).unwrap_or_default(),
                fetched_unix_ms: crate::core::unix_ms(),
            });
        self.prs.updating = false;
        match fetched {
            Ok(data) => {
                self.prs.error = None;
                self.save_prs(&data);
                self.prs.data = Some(Arc::new(data));
            }
            Err(err) => self.prs.error = Some(format!("{err:#}")),
        }
    }

    fn save_prs(&mut self, data: &PrData) {
        if let Some(path) = &self.pr_cache {
            if let Err(err) = prs::save_cache(path, data) {
                self.last_error = Some(format!("{err:#}"));
            }
        }
    }

    /// Focuses the open worktree of the PR's branch, else creates one.
    fn open_pr_worktree(
        &self,
        repo: &str,
        head_repo: Option<&str>,
        head: &str,
        number: u64,
        guard: Guard,
    ) -> Result<String> {
        match self.pr_checkout(repo, head_repo, head, number, guard)? {
            PrCheckout::Open(c) => {
                guard()?;
                self.herdr.workspace_focus(&c.workspace_id)?;
                Ok(format!("#{number}: went to {}", c.label))
            }
            PrCheckout::Made { path, setup, .. } => Ok(match setup {
                Some(n) => format!("#{number}: worktree {path} · {n}"),
                None => format!("#{number}: worktree {path}"),
            }),
        }
    }

    /// Starts Mastra Code on the PR's worktree with `prompt`: a new tab in an
    /// open worktree, so a busy pane there is never typed into, else the root
    /// pane of a fresh one.
    fn start_pr(
        &self,
        pr: (&str, Option<&str>, &str, u64),
        prompt: &str,
        guard: Guard,
    ) -> Result<String> {
        let (repo, head_repo, head, number) = pr;
        let (pane, path) = match self.pr_checkout(repo, head_repo, head, number, guard)? {
            PrCheckout::Open(c) => {
                guard()?;
                let pane =
                    self.herdr
                        .tab_create(&c.workspace_id, &c.path, &format!("#{number}"), true)?;
                (pane, c.path)
            }
            PrCheckout::Made { pane, path, .. } => (pane, path),
        };
        issue::start_agent(&self.herdr, &pane, &path, self.ready_wait, guard)?;
        guard()?;
        self.herdr
            .agent_prompt(&pane, prompt, issue::PROMPT_WAIT_MS)?;
        Ok(format!("#{number}: agent started in {path} ({pane})"))
    }

    /// The open worktree of the PR's branch, else `gh pr checkout --worktree`
    /// from the repo's main checkout. A missing `head_repo` (deleted fork)
    /// counts as a fork.
    fn pr_checkout(
        &self,
        repo: &str,
        head_repo: Option<&str>,
        head: &str,
        number: u64,
        guard: Guard,
    ) -> Result<PrCheckout> {
        let pr = pr::PrHead {
            number,
            head: head.to_string(),
            fork: !head_repo.is_some_and(|h| h.eq_ignore_ascii_case(repo)),
        };
        let checkouts = prs::checkouts(&self.herdr, guard)?;
        if let Some(c) = pr::open_checkout(&checkouts, repo, &pr) {
            return Ok(PrCheckout::Open(c.clone()));
        }
        let main = checkouts
            .iter()
            .find(|c| c.main && c.repo.eq_ignore_ascii_case(repo))
            .ok_or_else(|| {
                anyhow::anyhow!("#{number}: no open workspace has a checkout of {repo}")
            })?;
        let (dir, setup) = &self.worktrees;
        pr::checkout(
            &self.herdr,
            &checkouts,
            &main.path,
            repo,
            &pr,
            (dir, *setup),
            guard,
        )
    }

    pub fn with_linear_key(mut self, key: Option<String>) -> Self {
        self.linear_key = key;
        self
    }

    pub fn with_slack(mut self, slack: Option<Slack>) -> Self {
        self.slack = slack;
        self
    }

    /// Handles one command; `Refresh` is the caller's (it pokes the daemon).
    pub fn handle(&mut self, cmd: Command) {
        let stop = self.stop.clone();
        let guard = move || -> Result<()> {
            if stop.load(Ordering::SeqCst) {
                Err(Stopped.into())
            } else {
                Ok(())
            }
        };
        let result = match &cmd {
            Command::Refresh => Ok(()),
            Command::PlanCleanup(workspace) => {
                let planned =
                    cleanup::build_plan(&self.herdr, workspace, &guard).and_then(|plan| {
                        let refusals = cleanup::check(&self.herdr, &plan, &guard)?;
                        Ok((plan, refusals))
                    });
                match planned {
                    Ok((plan, refusals)) => {
                        self.plan_serial += 1;
                        self.cleanup = Some(CleanupView {
                            serial: self.plan_serial,
                            plan,
                            refusals,
                        });
                        self.notice = None;
                    }
                    Err(err) => self.notice = Some(format!("cleanup: {err:#}")),
                }
                Ok(())
            }
            Command::ExecuteCleanup(plan) => {
                self.notice = Some(match cleanup::execute(&self.herdr, plan, &guard) {
                    Ok(summary) => summary,
                    Err(err) => format!("{err:#}"),
                });
                Ok(())
            }
            Command::FetchIssue(id) => {
                self.launch = None;
                let serial = self.fetched.as_ref().map_or(0, |f| f.0) + 1;
                self.fetched = Some((serial, id.clone()));
                self.notice = Some(match self.fetch_for_launch(id, &guard) {
                    Ok(view) => {
                        let line = format!("{}: {}", view.issue.identifier, view.issue.title);
                        self.launch = Some(view);
                        line
                    }
                    Err(err) => format!("linear: {err:#}"),
                });
                Ok(())
            }
            Command::Launch { issue, repo_root } => {
                self.launch = None;
                self.notice = Some(
                    match issue::launch(&self.herdr, issue, repo_root, self.ready_wait, &guard) {
                        Ok(summary) => summary,
                        Err(err) => format!("launch: {err:#}"),
                    },
                );
                Ok(())
            }
            Command::Reply {
                pane,
                text,
                stall_hash,
            } => {
                let result = self.reply(pane, text, *stall_hash, &guard);
                let serial = self.replied.map_or(0, |r| r.0) + 1;
                self.replied = Some((serial, result.is_ok()));
                // Never the text: every error here is built without it.
                self.notice = Some(match result {
                    Ok(()) => format!("reply sent to {pane}"),
                    Err(err) => format!("{err:#}"),
                });
                Ok(())
            }
            Command::TabVisible(tab) => {
                self.visible = *tab;
                match tab {
                    Tab::Prs => self.prs_wanted = true,
                    Tab::Issues => self.issues_wanted = true,
                    Tab::Agents | Tab::Factory => {}
                }
                Ok(())
            }
            Command::FactorySend { message } => {
                let result = match &self.slack {
                    None => Err(anyhow::anyhow!(
                        "SLACK_USER_TOKEN or SLACK_CHANNEL not set — see README"
                    )),
                    Some(slack) => guard().and_then(|()| {
                        slack.post_message(
                            &self.herdr.runner,
                            &message.body,
                            message.signature.as_deref(),
                        )
                    }),
                };
                let serial = self.factory_sent.map_or(0, |r| r.0) + 1;
                self.factory_sent = Some((serial, result.is_ok()));
                // Never the text or the token: Slack errors carry neither.
                self.notice = Some(match result {
                    Ok(()) => "sent to Factory in Slack".to_string(),
                    Err(err) => format!("factory: {err:#}"),
                });
                Ok(())
            }
            Command::FetchIssues => {
                self.issues_wanted = true;
                Ok(())
            }
            Command::FetchIssueDetail { serial, id } => {
                let fetched = self.key().and_then(|key| {
                    guard()?;
                    linear::fetch_issue_detail(&self.herdr.runner, &key, id)
                });
                self.issue_detail = Some(match fetched {
                    Ok(d) => IssueDetailView {
                        serial: *serial,
                        id: id.clone(),
                        detail: Some(Arc::new(d.issue)),
                        error: None,
                    },
                    Err(err) => IssueDetailView {
                        serial: *serial,
                        id: id.clone(),
                        detail: None,
                        error: Some(format!("{err:#}")),
                    },
                });
                Ok(())
            }
            Command::FetchTeamStates {
                issue_id,
                identifier,
                team_id,
                current,
            } => {
                let fetched = self.key().and_then(|key| {
                    guard()?;
                    linear::team_states(&self.herdr.runner, &key, team_id)
                });
                match fetched {
                    Ok(states) => {
                        let serial = self.states.as_ref().map_or(0, |s| s.serial) + 1;
                        self.states = Some(StatesView {
                            serial,
                            issue_id: issue_id.clone(),
                            identifier: identifier.clone(),
                            current: current.clone(),
                            states,
                        });
                    }
                    Err(err) => self.notice = Some(format!("status {identifier}: {err:#}")),
                }
                Ok(())
            }
            Command::SetIssueState {
                issue_id,
                identifier,
                state,
            } => {
                let set = self.key().and_then(|key| {
                    guard()?;
                    linear::set_state(&self.herdr.runner, &key, issue_id, &state.id)
                });
                self.notice = Some(match set {
                    Ok(()) => {
                        self.patch_issue(issue_id, Some(state), None);
                        format!("{identifier} → {}", state.name)
                    }
                    Err(err) => format!("status {identifier}: {err:#}"),
                });
                Ok(())
            }
            Command::PrepareStart { id, identifier } => {
                self.launch = None;
                let serial = self.fetched.as_ref().map_or(0, |f| f.0) + 1;
                self.fetched = Some((serial, identifier.clone()));
                self.notice = Some(match self.prepare_start(id, &guard) {
                    Ok(view) => {
                        self.launch = Some(view);
                        format!("start {identifier}: pick a repo")
                    }
                    Err(err) => format!("start {identifier}: {err:#}"),
                });
                Ok(())
            }
            Command::Start { issue, repo_root } => {
                self.launch = None;
                self.notice = Some(self.start_issue(issue, repo_root, &guard));
                Ok(())
            }
            Command::FetchPrs => {
                self.prs_wanted = true;
                Ok(())
            }
            Command::FetchPrDetail {
                serial,
                repo,
                number,
            } => {
                let fetched = guard()
                    .and_then(|()| github::fetch_pr_detail(&self.herdr.runner, repo, *number));
                self.pr_detail = Some(match fetched {
                    Ok(d) => DetailView {
                        serial: *serial,
                        number: *number,
                        detail: Some(Arc::new(d)),
                        error: None,
                    },
                    Err(err) => DetailView {
                        serial: *serial,
                        number: *number,
                        detail: None,
                        error: Some(format!("{err:#}")),
                    },
                });
                Ok(())
            }
            Command::OpenPrWorktree {
                repo,
                head_repo,
                head,
                number,
            } => {
                self.notice = Some(
                    match self.open_pr_worktree(repo, head_repo.as_deref(), head, *number, &guard) {
                        Ok(done) => done,
                        Err(err) => format!("{err:#}"),
                    },
                );
                Ok(())
            }
            Command::StartPr {
                repo,
                head_repo,
                head,
                number,
                prompt,
            } => {
                let pr = (repo.as_str(), head_repo.as_deref(), head.as_str(), *number);
                // Never the prompt: every error here is built without it.
                self.notice = Some(match self.start_pr(pr, prompt, &guard) {
                    Ok(done) => done,
                    Err(err) => format!("start #{number}: {err:#}"),
                });
                Ok(())
            }
            Command::RerunFailed { repo, head, number } => {
                let rerun = guard()
                    .and_then(|()| crate::core::herdr::check_branch(head))
                    .and_then(|()| github::rerun_failed(&self.herdr.runner, repo, head));
                self.notice = Some(match rerun {
                    Ok(0) => format!("#{number}: no failed Actions runs to rerun"),
                    Ok(1) => format!("#{number}: rerunning 1 failed run"),
                    Ok(n) => format!("#{number}: rerunning {n} failed runs"),
                    Err(err) => format!("rerun #{number}: {err:#}"),
                });
                Ok(())
            }
            Command::Focus(pane) => self.herdr.focus_agent(pane),
            Command::OpenUrl(url) => {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                self.herdr
                    .runner
                    .run(&[opener, url], None, None)
                    .and_then(|o| {
                        if o.success() {
                            Ok(())
                        } else {
                            Err(anyhow::anyhow!("{opener} {url}: {}", o.stderr.trim()))
                        }
                    })
            }
        };
        if let Err(err) = result {
            self.last_error = Some(format!("{err:#}"));
        }
    }

    /// Sends a reply only if a fresh look agrees it is safe: not asking on the
    /// visible screen (a picker would swallow the text), and idle or done, or
    /// still stalled on exactly the screen the snapshot saw.
    fn reply(&self, pane: &str, text: &str, stall_hash: Option<u64>, guard: Guard) -> Result<()> {
        guard()?;
        let mut status = self
            .herdr
            .agent_list()?
            .into_iter()
            .find(|a| a.pane_id == pane)
            .ok_or_else(|| anyhow::anyhow!("not sent: {pane} is gone"))?
            .agent_status;
        guard()?;
        let visible = self
            .herdr
            .pane_read(pane, ReadSource::Visible, ASK_READ_LINES)?;
        if asking::is_asking(&visible) {
            anyhow::bail!("not sent: {pane} is asking — answer it in the pane");
        }
        // Same correction as the daemon: a running turn on screen is working.
        if matches!(status.as_str(), "idle" | "done") && asking::is_busy(&visible) {
            status = "working".to_string();
        }
        match status.as_str() {
            "idle" | "done" => {}
            "working" => {
                let Some(expected) = stall_hash else {
                    anyhow::bail!("not sent: {pane} is working");
                };
                guard()?;
                let recent = self
                    .herdr
                    .pane_read(pane, ReadSource::Recent, STALL_READ_LINES)?;
                if crate::core::fnv1a(stall::normalize(&recent).as_bytes()) != expected {
                    anyhow::bail!("not sent: {pane} is working again");
                }
            }
            other => anyhow::bail!("not sent: {pane} is {other}"),
        }
        guard()?;
        self.herdr.agent_send(pane, text)
    }

    fn fetch_for_launch(&mut self, id: &str, guard: Guard) -> Result<LaunchView> {
        let id =
            linear::parse_issue_id(id).ok_or_else(|| anyhow::anyhow!("not an issue id: {id:?}"))?;
        let key = self
            .linear_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("LINEAR_API_KEY not set — see README"))?;
        guard()?;
        let issue = linear::fetch_issues(&self.herdr.runner, key, std::slice::from_ref(&id))?
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("{id} not found in Linear"))?;
        if issue
            .branch_name
            .as_deref()
            .is_none_or(|b| b.trim().is_empty())
        {
            anyhow::bail!("{id} has no branchName in Linear");
        }
        guard()?;
        let workspaces = self.herdr.workspace_list()?;
        let repos = issue::repo_choices(&self.herdr, &workspaces, guard)?;
        if repos.is_empty() {
            anyhow::bail!("no open workspace on a main git checkout to launch in");
        }
        self.launch_serial += 1;
        Ok(LaunchView {
            serial: self.launch_serial,
            issue,
            repos,
            start: None,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::runner::{FakeRunner, Output};
    use std::thread;

    pub(crate) const PANE: &str = "w1:p1";

    /// The Inbox's executor over the same fake, for the commands it now owns.
    fn into_actions(w: Watcher<FakeRunner>) -> Actions<FakeRunner> {
        Actions::new(w.herdr, w.stop).with_linear_key(w.linear_key)
    }

    pub(crate) fn agents_json(status: &str) -> String {
        format!(
            r#"{{"id":"x","result":{{"type":"agent_list","agents":[
              {{"pane_id":"{PANE}","workspace_id":"w1","tab_id":"w1:t1","agent":"mastracode",
                "agent_status":"{status}","cwd":"/r","foreground_cwd":"/r","tokens":{{}},"state_change_seq":4}},
              {{"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t1","agent":"claude",
                "agent_status":"working","cwd":"/r","tokens":{{}},"state_change_seq":2}}]}}}}"#
        )
    }

    const WORKSPACES: &str = r#"{"id":"x","result":{"type":"workspace_list","workspaces":[
        {"workspace_id":"w1","label":"proj","focused":true}]}}"#;
    const GIT: [&str; 9] = [
        "git",
        "-C",
        "/r",
        "rev-parse",
        "--show-toplevel",
        "--abbrev-ref",
        "HEAD",
        "--path-format=absolute",
        "--git-common-dir",
    ];

    fn git_at(dir: &'static str) -> [&'static str; 9] {
        let mut argv = GIT;
        argv[2] = dir;
        argv
    }

    /// The batched PR query for `branches`, as the FakeRunner key.
    fn gh_for(branches: &[&str]) -> Vec<&'static str> {
        crate::integrations::github::batch_argv(branches)
            .into_iter()
            .map(|s| &*Box::leak(s.into_boxed_str()))
            .collect()
    }

    /// A recorded `gh pr list` reply re-shaped as the batched reply for one branch.
    fn as_batch(list: &str) -> String {
        let prs: Vec<serde_json::Value> = serde_json::from_str(list).unwrap();
        let nodes: Vec<serde_json::Value> = prs
            .iter()
            .map(|p| {
                serde_json::json!({
                    "number": p["number"], "state": p["state"], "url": p["url"],
                    "headRefOid": p["headRefOid"],
                    "commits": {"nodes": [{"commit": {"statusCheckRollup":
                        {"contexts": {"nodes": p["statusCheckRollup"]}}}}]}
                })
            })
            .collect();
        serde_json::json!({"data": {"repository": {"b0": {"nodes": nodes}}}}).to_string()
    }
    const READ: [&str; 8] = [
        "herdr", "pane", "read", PANE, "--source", "recent", "--lines", "60",
    ];

    const OK_REPLY: &str = r#"{"id":"x","result":{"shown":true}}"#;
    const VISIBLE: [&str; 8] = [
        "herdr", "pane", "read", PANE, "--source", "visible", "--lines", "60",
    ];

    fn gh_fail() -> String {
        as_batch(
            &std::fs::read_to_string(format!(
                "{}/tests/fixtures/gh/pr-open-fail.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap(),
        )
    }

    fn set_argv(tokens: &[&str]) -> String {
        let mut s = format!("herdr pane report-metadata {PANE} --source werkbank");
        for t in tokens {
            s.push_str(&format!(" --token {t}"));
        }
        s + " --ttl-ms 90000"
    }

    fn clear_argv(names: &[&str]) -> String {
        let mut s = format!("herdr pane report-metadata {PANE} --source werkbank");
        for n in names {
            s.push_str(&format!(" --clear-token {n}"));
        }
        s
    }

    /// A fake with one working mastracode pane on feat/x whose PR has failing CI.
    pub(crate) fn fake(threshold: Duration) -> Watcher<FakeRunner> {
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, &agents_json("working"))
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .ok(
                &READ,
                None,
                "⠏ Sleeping 7s\n • working 7s · esc to interrupt",
            )
            .ok(&GIT, None, "/r\nfeat/x\n/r/.git\n")
            .ok(&gh_for(&["feat/x"]), Some("/r"), &gh_fail())
            .otherwise(OK_REPLY);
        let settings = Settings {
            stall_threshold: threshold,
            poll: Duration::from_millis(10),
        };
        Watcher::new(
            Herdr::with_bin(f, "herdr"),
            settings,
            Arc::new(AtomicBool::new(false)),
        )
    }

    const LINKED: &str = r#"{"id":"x","result":{"type":"workspace_list","workspaces":[
        {"workspace_id":"w1","label":"main","worktree":{"checkout_path":"/repo","repo_root":"/repo","is_linked_worktree":false,"repo_name":"repo"}},
        {"workspace_id":"w2","label":"feat-x","worktree":{"checkout_path":"/wt/x","repo_root":"/repo","is_linked_worktree":true,"repo_name":"repo"}},
        {"workspace_id":"w3","label":"detached","worktree":{"checkout_path":"/wt/d","repo_root":"/repo","is_linked_worktree":true,"repo_name":"repo"}}]}}"#;

    fn merged_fake() -> Watcher<FakeRunner> {
        let merged = std::fs::read_to_string(format!(
            "{}/tests/fixtures/gh/pr-merged.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let gh = gh_for(&["feat/merged"]);
        let f = FakeRunner::new();
        f.ok(
            &["herdr", "agent", "list"],
            None,
            r#"{"id":"x","result":{"type":"agent_list","agents":[]}}"#,
        )
        .ok(&["herdr", "workspace", "list"], None, LINKED)
        .ok(&git_at("/wt/x"), None, "/wt/x\nfeat/merged\n/repo/.git\n")
        .ok(&git_at("/wt/d"), None, "/wt/d\nHEAD\n/repo/.git\n")
        .ok(&gh, Some("/wt/x"), &as_batch(&merged));
        Watcher::new(
            Herdr::with_bin(f, "herdr"),
            Settings {
                stall_threshold: Duration::from_secs(300),
                poll: Duration::from_millis(10),
            },
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn discovers_merged_linked_worktrees_only_and_caches_5_min() {
        let mut w = merged_fake();
        let t0 = Instant::now();
        let state = w.tick(t0).unwrap();
        assert_eq!(state.merged.len(), 1, "{:?}", state.merged);
        let item = &state.merged[0];
        assert_eq!(item.kind, AttentionKind::MergedCleanup);
        assert_eq!(item.workspace_id, "w2");
        assert!(item.detail.contains("feat/merged"), "{}", item.detail);
        let argvs = w.herdr.runner.argvs();
        assert!(
            !argvs.iter().any(|a| a.contains("-C /repo")),
            "main checkout skipped"
        );
        assert_eq!(
            argvs.iter().filter(|a| a.starts_with("gh ")).count(),
            1,
            "detached skipped"
        );

        w.tick(t0 + Duration::from_secs(299)).unwrap();
        let gh_calls = |w: &Watcher<FakeRunner>| {
            w.herdr
                .runner
                .argvs()
                .iter()
                .filter(|a| a.starts_with("gh "))
                .count()
        };
        assert_eq!(gh_calls(&w), 1);
        w.tick(t0 + Duration::from_secs(300)).unwrap();
        assert_eq!(gh_calls(&w), 2);
    }

    #[test]
    fn a_checkout_shared_by_an_agent_and_a_worktree_is_probed_once_per_tick() {
        let w = merged_fake();
        w.herdr.runner.set(
            &["herdr", "agent", "list"],
            None,
            r#"{"id":"x","result":{"type":"agent_list","agents":[
                {"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":"mastracode",
                 "agent_status":"blocked","cwd":"/wt/x","state_change_seq":1}]}}"#,
        );
        let mut w = w;
        let t0 = Instant::now();
        let probes = |w: &Watcher<FakeRunner>| {
            w.herdr
                .runner
                .argvs()
                .iter()
                .filter(|a| a.starts_with("git -C /wt/x rev-parse"))
                .count()
        };
        let state = w.tick(t0).unwrap();
        assert_eq!(state.agents[0].branch.as_deref(), Some("feat/merged"));
        assert_eq!(state.merged.len(), 1);
        assert_eq!(probes(&w), 1, "memoized within a tick");
        w.herdr
            .runner
            .set(&git_at("/wt/x"), None, "/wt/x\nfeat/other\n/repo/.git\n");
        let state = w.tick(t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(probes(&w), 2, "re-probed next tick");
        assert_eq!(
            state.agents[0].branch.as_deref(),
            Some("feat/other"),
            "a branch switch shows up on the next tick"
        );
        assert!(state.merged.is_empty(), "no longer the merged branch");
    }

    #[test]
    fn a_dir_git_does_not_recognise_is_re_probed_every_30s() {
        let w = fake(Duration::from_secs(300));
        let mut argv = GIT.to_vec();
        argv[2] = "/plain";
        w.herdr.runner.respond(
            &argv,
            None,
            Output {
                status: 128,
                stdout: String::new(),
                stderr: "fatal: not a git repository".into(),
            },
        );
        let mut w = w;
        let probes = |w: &Watcher<FakeRunner>| {
            let n = w.herdr.runner.argvs();
            n.iter()
                .filter(|a| a.starts_with("git -C /plain rev-parse"))
                .count()
        };
        let t0 = Instant::now();
        for (secs, want) in [(0, 1), (2, 1), (29, 1), (30, 2), (32, 2)] {
            w.now = t0 + Duration::from_secs(secs);
            w.branches.clear();
            assert_eq!(w.branch("/plain").unwrap(), None);
            assert_eq!(probes(&w), want, "at +{secs}s");
        }
    }

    #[test]
    fn a_checkout_is_re_probed_only_when_its_head_file_changes() {
        let root = std::env::temp_dir().join(format!("werkbank-head-{}", std::process::id()));
        let main = root.join("main");
        let linked = root.join("linked");
        let gitdir = main.join(".git/worktrees/linked");
        fs::create_dir_all(&gitdir).unwrap();
        fs::create_dir_all(&linked).unwrap();
        fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        let (m, l) = (main.to_str().unwrap(), linked.to_str().unwrap());

        let w = fake(Duration::from_secs(300));
        let mut argv = GIT.to_vec();
        for (dir, out) in [
            (m, format!("{m}\nmain\n{m}/.git\n")),
            (l, format!("{l}\nfeat/x\n{m}/.git\n")),
        ] {
            argv[2] = dir;
            w.herdr.runner.set(&argv, None, &out);
        }
        let mut w = w;
        let probes = |w: &Watcher<FakeRunner>, dir: &str| {
            let prefix = format!("git -C {dir} rev-parse");
            w.herdr
                .runner
                .argvs()
                .iter()
                .filter(|a| a.starts_with(&prefix))
                .count()
        };
        let tick = |w: &mut Watcher<FakeRunner>| {
            for dir in [m, l] {
                w.branch(dir).unwrap();
            }
            w.branches.clear();
        };
        tick(&mut w);
        tick(&mut w);
        assert_eq!(
            (probes(&w, m), probes(&w, l)),
            (2, 2),
            "learns HEAD on the 2nd probe"
        );
        tick(&mut w);
        tick(&mut w);
        assert_eq!(
            (probes(&w, m), probes(&w, l)),
            (2, 2),
            "unchanged HEAD: no git"
        );
        assert_eq!(w.branch(l).unwrap().unwrap().branch, "feat/x");
        w.branches.clear();

        fs::write(gitdir.join("HEAD"), "ref: refs/heads/feat/y\n").unwrap();
        argv[2] = l;
        w.herdr
            .runner
            .set(&argv, None, &format!("{l}\nfeat/y\n{m}/.git\n"));
        assert_eq!(
            w.branch(l).unwrap().unwrap().branch,
            "feat/y",
            "switch seen at once"
        );
        w.branches.clear();
        tick(&mut w);
        assert_eq!(
            (probes(&w, m), probes(&w, l)),
            (2, 3),
            "only the switched checkout"
        );

        fs::remove_dir_all(&root).unwrap();
        tick(&mut w);
        assert_eq!(
            (probes(&w, m), probes(&w, l)),
            (3, 4),
            "unreadable HEAD: probe again"
        );
    }

    #[test]
    fn plan_command_reports_view_and_failed_plan_reports_notice() {
        let mut w = into_actions(merged_fake());
        // build_plan needs HEAD; no response for it makes planning fail cleanly.
        w.handle(Command::PlanCleanup("w2".into()));
        assert!(w.cleanup.is_none());
        let notice = w.notice.clone().unwrap();
        assert!(notice.starts_with("cleanup: "), "{notice}");
        assert!(
            !w.herdr
                .runner
                .argvs()
                .iter()
                .any(|a| a.contains("worktree remove")),
            "planning never removes"
        );
    }

    fn token_calls(w: &Watcher<FakeRunner>) -> Vec<String> {
        w.herdr
            .runner
            .argvs()
            .into_iter()
            .filter(|a| a.contains("report-metadata"))
            .collect()
    }

    #[test]
    fn tick_sets_pr_and_stalled_tokens_for_mastracode_only() {
        let mut w = fake(Duration::ZERO);
        let t0 = Instant::now();
        let state = w.tick(t0).unwrap();
        assert_eq!(state.agents.len(), 1);
        let a = &state.agents[0];
        assert_eq!(a.workspace_label, "proj");
        assert_eq!(a.branch.as_deref(), Some("feat/x"));
        assert_eq!(a.pr.as_ref().unwrap().number, 29923);
        assert_eq!(
            w.herdr.runner.argvs(),
            vec![
                "herdr agent list".to_string(),
                "herdr workspace list".into(),
                GIT.join(" "),
                gh_for(&["feat/x"]).join(" "),
                VISIBLE.join(" "),
                READ.join(" "),
                set_argv(&["pr=#29923 ✗", "stalled=⏸ 0s"]),
                "herdr notification show Stalled: proj/w1:t1 --body w1:p1 — no output for 0s --sound request".into(),
                "herdr notification show CI failed: #29923 --body proj feat/x --sound request".into(),
            ]
        );
    }

    #[test]
    fn unchanged_tokens_are_resent_only_every_30s() {
        let mut w = fake(Duration::from_secs(3600));
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.tick(t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(token_calls(&w), vec![set_argv(&["pr=#29923 ✗"])]);
        w.tick(t0 + Duration::from_secs(31)).unwrap();
        assert_eq!(token_calls(&w).len(), 2);
    }

    #[test]
    fn pr_token_cleared_when_branch_becomes_main() {
        let mut w = fake(Duration::from_secs(3600));
        w.herdr.runner.ok(&GIT, None, "/r\nmain\n/r/.git\n");
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        let state = w.tick(t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(state.agents[0].pr, None);
        assert_eq!(
            token_calls(&w),
            vec![set_argv(&["pr=#29923 ✗"]), clear_argv(&["pr"])]
        );
    }

    #[test]
    fn pr_token_cleared_when_lookup_finds_no_pr() {
        let mut w = fake(Duration::from_secs(3600));
        w.herdr
            .runner
            .ok(&gh_for(&["feat/x"]), Some("/r"), &as_batch("[]"));
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.tick(t0 + Duration::from_secs(30)).unwrap();
        assert_eq!(
            token_calls(&w),
            vec![set_argv(&["pr=#29923 ✗"]), clear_argv(&["pr"])]
        );
    }

    #[test]
    fn stalled_token_cleared_once_output_moves() {
        let mut w = fake(Duration::from_secs(20));
        let same = "⠏ Sleeping 9s\n • working 9s · esc to interrupt";
        w.herdr
            .runner
            .ok(&READ, None, same)
            .ok(&READ, None, "step two");
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        // 15s later the screen only differs by its timer: still the same work.
        w.tick(t0 + Duration::from_secs(15)).unwrap();
        w.tick(t0 + Duration::from_secs(21)).unwrap();
        // At 30s the screen really changed, so the stall clears.
        w.tick(t0 + Duration::from_secs(30)).unwrap();
        assert_eq!(
            token_calls(&w),
            vec![
                set_argv(&["pr=#29923 ✗"]),
                set_argv(&["pr=#29923 ✗", "stalled=⏸ 21s"]),
                clear_argv(&["stalled"]),
            ]
        );
    }

    #[test]
    fn stall_reads_use_recent_source_and_only_while_working() {
        let mut w = fake(Duration::from_secs(3600));
        w.herdr
            .runner
            .ok(&["herdr", "agent", "list"], None, &agents_json("idle"));
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.tick(t0 + Duration::from_secs(20)).unwrap();
        let reads: Vec<String> = w
            .herdr
            .runner
            .argvs()
            .into_iter()
            .filter(|a| a.contains("--source recent"))
            .collect();
        assert_eq!(reads, vec![READ.join(" ")]);
    }

    #[test]
    fn pane_gone_mid_tick_is_dropped_not_fatal() {
        let mut w = fake(Duration::ZERO);
        let gone = Output {
            status: 1,
            stdout: String::new(),
            stderr: r#"{"error":{"code":"pane_not_found","message":"pane w1:p1 not found"}}"#
                .into(),
        };
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, &agents_json("working"))
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .respond(&READ, None, gone)
            .otherwise("");
        w.herdr = Herdr::with_bin(f, "herdr");
        let state = w.tick(Instant::now()).unwrap();
        assert!(state.agents.is_empty());
        assert_eq!(state.error, None);
    }

    #[test]
    fn token_write_failure_keeps_agent_listed() {
        let mut w = fake(Duration::ZERO);
        w.herdr.runner.respond(
            &[
                "herdr",
                "pane",
                "report-metadata",
                PANE,
                "--source",
                "werkbank",
                "--token",
                "pr=#29923 ✗",
                "--token",
                "stalled=⏸ 0s",
                "--ttl-ms",
                "90000",
            ],
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: "socket hiccup".into(),
            },
        );
        let state = w.tick(Instant::now()).unwrap();
        assert_eq!(state.agents.len(), 1);
        assert!(state.error.unwrap().contains("socket hiccup"));
    }

    #[test]
    fn gh_errors_set_no_token_and_surface_in_state() {
        let mut w = fake(Duration::from_secs(3600));
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, &agents_json("idle"))
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .ok(&GIT, None, "/r\nfeat/x\n/r/.git\n")
            .respond(
                &gh_for(&["feat/x"]),
                Some("/r"),
                Output {
                    status: 1,
                    stdout: String::new(),
                    stderr: "no git remotes found\n".into(),
                },
            )
            .otherwise("");
        w.herdr = Herdr::with_bin(f, "herdr");
        let state = w.tick(Instant::now()).unwrap();
        assert_eq!(state.agents[0].pr, None);
        assert!(state.error.unwrap().contains("no git remotes"));
        assert!(token_calls(&w).is_empty());
    }

    #[test]
    fn prs_are_fetched_with_one_call_per_repo() {
        let agents = r#"{"id":"x","result":{"type":"agent_list","agents":[
            {"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent":"mastracode","agent_status":"blocked","cwd":"/wt/a","state_change_seq":1},
            {"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t1","agent":"mastracode","agent_status":"blocked","cwd":"/wt/b","state_change_seq":1},
            {"pane_id":"w1:p3","workspace_id":"w1","tab_id":"w1:t1","agent":"mastracode","agent_status":"blocked","cwd":"/wt/b2","state_change_seq":1},
            {"pane_id":"w1:p4","workspace_id":"w1","tab_id":"w1:t1","agent":"mastracode","agent_status":"blocked","cwd":"/other","state_change_seq":1}]}}"#;
        let pr = |n: u64| {
            format!(
                r#"{{"nodes":[{{"number":{n},"state":"OPEN","url":"u","headRefOid":"h","commits":{{"nodes":[{{"commit":{{"statusCheckRollup":null}}}}]}}}}]}}"#
            )
        };
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, agents)
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .ok(&git_at("/wt/a"), None, "/wt/a\nfeat/a\n/repo/.git\n")
            .ok(&git_at("/wt/b"), None, "/wt/b\nfeat/b\n/repo/.git\n")
            // A second checkout on the same branch is looked up once.
            .ok(&git_at("/wt/b2"), None, "/wt/b2\nfeat/b\n/repo/.git\n")
            .ok(&git_at("/other"), None, "/other\nfeat/c\n/other/.git\n")
            .ok(
                &gh_for(&["feat/a", "feat/b"]),
                Some("/wt/a"),
                &format!(
                    r#"{{"data":{{"repository":{{"b0":{},"b1":{}}}}}}}"#,
                    pr(1),
                    pr(2)
                ),
            )
            .ok(
                &gh_for(&["feat/c"]),
                Some("/other"),
                r#"{"data":{"repository":{"b0":{"nodes":[]}}}}"#,
            )
            .otherwise(OK_REPLY);
        let mut w = fake(Duration::from_secs(3600));
        w.herdr = Herdr::with_bin(f, "herdr");
        let t0 = Instant::now();
        let state = w.tick(t0).unwrap();
        let numbers: Vec<Option<u64>> = state
            .agents
            .iter()
            .map(|a| a.pr.as_ref().map(|p| p.number))
            .collect();
        assert_eq!(numbers, vec![Some(1), Some(2), Some(2), None]);
        let gh_calls = |w: &Watcher<FakeRunner>| {
            w.herdr
                .runner
                .argvs()
                .into_iter()
                .filter(|a| a.starts_with("gh "))
                .count()
        };
        assert_eq!(gh_calls(&w), 2, "one call per repo");
        w.tick(t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(gh_calls(&w), 2, "cached inside the refresh interval");
        w.tick(t0 + crate::integrations::github::OPEN_REFRESH)
            .unwrap();
        assert_eq!(gh_calls(&w), 4);
    }

    #[test]
    fn failed_gh_lookups_wait_the_refresh_interval_before_retrying() {
        let mut w = fake(Duration::from_secs(3600));
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, &agents_json("idle"))
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .ok(&GIT, None, "/r\nfeat/x\n/r/.git\n")
            .respond(
                &gh_for(&["feat/x"]),
                Some("/r"),
                Output {
                    status: 1,
                    stdout: String::new(),
                    stderr: "gh auth login required\n".into(),
                },
            )
            .otherwise("");
        w.herdr = Herdr::with_bin(f, "herdr");
        let gh_calls = |w: &Watcher<FakeRunner>| {
            w.herdr
                .runner
                .argvs()
                .iter()
                .filter(|a| a.starts_with("gh "))
                .count()
        };
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        assert_eq!(gh_calls(&w), 1);
        w.tick(t0 + Duration::from_secs(2)).unwrap();
        w.tick(t0 + Duration::from_secs(20)).unwrap();
        assert_eq!(gh_calls(&w), 1, "no retry inside the interval");
        w.tick(t0 + Duration::from_secs(31)).unwrap();
        assert_eq!(gh_calls(&w), 2);
    }

    #[test]
    fn a_tick_without_the_agent_keeps_its_last_known_pr() {
        let mut w = fake(Duration::ZERO);
        let t0 = Instant::now();
        let state = w.tick(t0).unwrap();
        assert_eq!(state.agents[0].pr.as_ref().unwrap().number, 29923);
        // Past the 5 min refresh the agent briefly drops out of `agent list`…
        let r = &w.herdr.runner;
        r.set(
            &["herdr", "agent", "list"],
            None,
            r#"{"id":"x","result":{"type":"agent_list","agents":[]}}"#,
        );
        w.tick(t0 + Duration::from_secs(400)).unwrap();
        // …then returns while gh is failing: the last known PR must still be there.
        let r = &w.herdr.runner;
        r.set(&["herdr", "agent", "list"], None, &agents_json("working"));
        r.set(&gh_for(&["feat/x"]), Some("/r"), "not json");
        let state = w.tick(t0 + Duration::from_secs(402)).unwrap();
        assert!(state.error.is_some(), "gh failure surfaced");
        assert_eq!(state.agents[0].pr.as_ref().unwrap().number, 29923);
    }

    #[test]
    fn stop_during_slow_call_returns_promptly_without_clearing() {
        let slow = Output {
            status: 0,
            stdout: agents_json("working"),
            stderr: String::new(),
        };
        let mut w = fake(Duration::from_secs(3600));
        w.herdr.runner.respond_after(
            &["herdr", "agent", "list"],
            None,
            slow,
            Duration::from_millis(400),
        );
        let stop = w.stop.clone();
        // First tick (fast agent list) sets the pr token.
        w.tick(Instant::now()).unwrap();
        let handle = thread::spawn(move || {
            let result = w.tick(Instant::now());
            (w, result)
        });
        // The second tick blocks in the slow `agent list`; stop while it runs.
        thread::sleep(Duration::from_millis(100));
        let stopped_at = Instant::now();
        stop.store(true, Ordering::SeqCst);
        let (w, result) = handle.join().unwrap();
        assert!(stopped_at.elapsed() < Duration::from_millis(1000));
        assert!(
            result.unwrap_err().is::<Stopped>(),
            "a stopped tick is no state"
        );
        let calls = w.herdr.runner.argvs();
        assert_eq!(calls.last().unwrap(), "herdr agent list");
        // Tokens belong to whichever daemon runs next; stopping never clears them.
        assert!(
            calls.iter().all(|c| !c.contains("--clear-token")),
            "{calls:?}"
        );
    }

    fn notify_calls(w: &Watcher<FakeRunner>) -> Vec<String> {
        w.herdr
            .runner
            .argvs()
            .into_iter()
            .filter(|a| a.contains("notification show"))
            .collect()
    }

    fn screen(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/screens/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn notifications_fire_once_per_transition_and_rearm() {
        let mut w = fake(Duration::from_secs(20));
        let same = "⠏ Sleeping 9s\n • working 9s · esc to interrupt";
        w.herdr.runner.ok(&READ, None, same).ok(&READ, None, same);
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.tick(t0 + Duration::from_secs(21)).unwrap();
        w.tick(t0 + Duration::from_secs(25)).unwrap();
        let stalled: Vec<_> = notify_calls(&w)
            .into_iter()
            .filter(|a| a.contains("Stalled"))
            .collect();
        assert_eq!(
            stalled,
            ["herdr notification show Stalled: proj/w1:t1 --body w1:p1 — no output for 21s --sound request"]
        );
        // Output moves: the stall clears and re-arms, then fires again later.
        w.herdr.runner.ok(&READ, None, "step two");
        // Queued reads: t40 still `same`, t55 `step two` (clears), t76 unchanged (stalls again).
        w.tick(t0 + Duration::from_secs(40)).unwrap();
        w.tick(t0 + Duration::from_secs(55)).unwrap();
        w.tick(t0 + Duration::from_secs(76)).unwrap();
        let stalled = notify_calls(&w)
            .into_iter()
            .filter(|a| a.contains("Stalled"))
            .count();
        assert_eq!(stalled, 2);
        // CI failure stayed failing throughout: exactly one alert.
        let ci = notify_calls(&w)
            .into_iter()
            .filter(|a| a.contains("CI failed: #29923"))
            .count();
        assert_eq!(ci, 1);
    }

    #[test]
    fn a_running_turn_on_screen_overrides_an_idle_or_done_status() {
        for status in ["idle", "done"] {
            let mut w = fake(Duration::from_secs(3600));
            w.herdr
                .runner
                .set(&["herdr", "agent", "list"], None, &agents_json(status));
            w.herdr
                .runner
                .set(&VISIBLE, None, &screen("mc-working-a.txt"));
            let t0 = Instant::now();
            let a = w.tick(t0).unwrap().agents.remove(0);
            assert_eq!(a.info.agent_status, "working", "{status}");
            assert_eq!(
                crate::features::inbox::model::stage_of(&a),
                crate::features::inbox::model::Stage::Working
            );
            // The turn ends: the next visible read (after the 10s cache) restores it.
            w.herdr
                .runner
                .set(&VISIBLE, None, &screen("mc-idle-done.txt"));
            let a = w
                .tick(t0 + Duration::from_secs(11))
                .unwrap()
                .agents
                .remove(0);
            assert_eq!(a.info.agent_status, status);
        }
    }

    #[test]
    fn asking_token_set_and_cleared() {
        let mut w = fake(Duration::from_secs(3600));
        w.herdr
            .runner
            .set(&VISIBLE, None, &screen("mc-ask-user.txt"));
        let t0 = Instant::now();
        assert!(w.tick(t0).unwrap().agents[0].asking);
        let calls = token_calls(&w);
        assert!(
            calls.iter().any(|c| c.contains("--token asking=?")),
            "{calls:?}"
        );
        // Answered: the next visible read (after the 10s cache) clears it.
        w.herdr
            .runner
            .set(&VISIBLE, None, &screen("mc-working-a.txt"));
        assert!(!w.tick(t0 + Duration::from_secs(11)).unwrap().agents[0].asking);
        let calls = token_calls(&w);
        assert!(
            calls.last().unwrap().contains("--clear-token asking"),
            "{calls:?}"
        );
    }

    #[test]
    fn a_seeded_observer_keeps_a_stall_without_renotifying() {
        let same = "⠏ Sleeping 9s\n • working 9s · esc to interrupt";
        let mut w = fake(Duration::from_secs(20));
        w.herdr.runner.set(&READ, None, same);
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.tick(t0 + Duration::from_secs(21)).unwrap();
        assert_eq!(
            notify_calls(&w).len(),
            2,
            "stalled and CI failed, once each"
        );
        let record = w.export(t0 + Duration::from_secs(21), 5_000_000);

        // The successor: a fresh process, one second later on the wall clock.
        let mut next = fake(Duration::from_secs(20));
        next.herdr.runner.set(&READ, None, same);
        let later = Instant::now();
        next.seed(&record, later, 5_001_000);
        let first = next.tick(later).unwrap();
        assert!(
            first.agents[0].stalled.is_some(),
            "stalled from the first tick"
        );
        assert!(
            token_calls(&next)
                .iter()
                .any(|c| c.contains("--token stalled=")),
            "{:?}",
            token_calls(&next)
        );
        // A full threshold later, still frozen and still failing: no repeat alerts.
        next.tick(later + Duration::from_secs(25)).unwrap();
        next.tick(later + Duration::from_secs(45)).unwrap();
        assert!(notify_calls(&next).is_empty(), "{:?}", notify_calls(&next));
    }

    #[test]
    fn no_notifications_for_asking_blocked_or_done() {
        for status in ["blocked", "done", "idle"] {
            let f = FakeRunner::new();
            f.ok(&["herdr", "agent", "list"], None, &agents_json(status))
                .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
                .ok(&VISIBLE, None, &screen("mc-ask-user.txt"))
                .ok(&GIT, None, "/r\nmain\n/r/.git\n")
                .otherwise(OK_REPLY);
            let mut w = fake(Duration::ZERO);
            w.herdr = Herdr::with_bin(f, "herdr");
            let t0 = Instant::now();
            let state = w.tick(t0).unwrap();
            w.tick(t0 + Duration::from_secs(30)).unwrap();
            assert_eq!(state.agents[0].asking, status != "blocked", "{status}");
            assert!(notify_calls(&w).is_empty(), "{status}");
        }
    }

    #[test]
    fn ask_detection_reads_visible_screen_only() {
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, &agents_json("done"))
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .ok(&VISIBLE, None, &screen("mc-idle-done.txt"))
            .ok(&READ, None, &screen("mc-answered-recent.txt"))
            .ok(&GIT, None, "/r\nmain\n/r/.git\n")
            .otherwise(OK_REPLY);
        let mut w = fake(Duration::ZERO);
        w.herdr = Herdr::with_bin(f, "herdr");
        let t0 = Instant::now();
        assert!(!w.tick(t0).unwrap().agents[0].asking);
        // Cached for 10s, then read again from the visible screen.
        w.tick(t0 + Duration::from_secs(5)).unwrap();
        w.tick(t0 + Duration::from_secs(10)).unwrap();
        let reads: Vec<String> = w
            .herdr
            .runner
            .argvs()
            .into_iter()
            .filter(|a| a.contains("pane read"))
            .collect();
        assert_eq!(reads, vec![VISIBLE.join(" "); 2]);
    }

    #[test]
    fn asking_while_reported_working_is_not_a_stall() {
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, &agents_json("working"))
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .ok(&VISIBLE, None, &screen("mc-ask-user.txt"))
            .ok(&GIT, None, "/r\nmain\n/r/.git\n")
            .otherwise(OK_REPLY);
        let mut w = fake(Duration::ZERO);
        w.herdr = Herdr::with_bin(f, "herdr");
        let t0 = Instant::now();
        let a = w.tick(t0).unwrap().agents.remove(0);
        let b = w
            .tick(t0 + Duration::from_secs(60))
            .unwrap()
            .agents
            .remove(0);
        assert!(a.asking && b.asking);
        assert_eq!(b.stalled, None);
        assert!(notify_calls(&w).is_empty());
        assert!(!w
            .herdr
            .runner
            .argvs()
            .iter()
            .any(|a| a.contains("--source recent")));
    }

    #[test]
    fn notification_log_lines_are_json_with_result() {
        let dir = std::env::temp_dir().join(format!("werkbank-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("notifications-s.log");
        let _ = std::fs::remove_file(&log);
        let mut w = fake(Duration::ZERO).with_notify_log(log.clone());
        w.tick(Instant::now()).unwrap();
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "stalled");
        assert_eq!(lines[0]["pane"], PANE);
        assert_eq!(lines[0]["title"], "Stalled: proj/w1:t1");
        assert_eq!(lines[0]["result"]["shown"], true);
        assert!(lines[0]["ts"].as_u64().unwrap() > 0);
        assert_eq!(lines[1]["kind"], "ci_failed");
        assert_eq!(lines[1]["title"], "CI failed: #29923");
        assert_eq!(lines[1]["body"], "proj feat/x");
    }

    fn curl(ids: &[&str]) -> Vec<String> {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        [
            "curl",
            "-sS",
            "https://api.linear.app/graphql",
            "-H",
            "Content-Type: application/json",
            "-H",
            "@-",
            "--data-binary",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([linear::build_request(&ids)])
        .collect()
    }

    fn curl_count(w: &Watcher<FakeRunner>) -> usize {
        w.herdr
            .runner
            .argvs()
            .iter()
            .filter(|a| a.starts_with("curl "))
            .count()
    }

    const ISSUE_REPLY: &str = include_str!("../../../tests/fixtures/linear/issues.json");

    #[test]
    fn branch_issues_are_fetched_with_the_key_and_cached() {
        let mut w = fake(Duration::from_secs(300)).with_linear_key(Some("k".into()));
        w.herdr
            .runner
            .set(&GIT, None, "/r\nuser/eng-325-widget-export-cli\n/r/.git\n");
        let argv = curl(&["ENG-325"]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        w.herdr.runner.ok(&argv, None, ISSUE_REPLY);
        let t0 = Instant::now();
        let state = w.tick(t0).unwrap();
        assert!(!state.linear_missing_key);
        assert_eq!(state.issues["ENG-325"].title, "\"Widget export\": CLI flag");
        let state = w.tick(t0 + Duration::from_secs(60)).unwrap();
        assert!(state.issues.contains_key("ENG-325"));
        assert_eq!(curl_count(&w), 1, "cached for 5 min");
        w.tick(t0 + linear::ISSUE_TTL).unwrap();
        assert_eq!(curl_count(&w), 2);
    }

    #[test]
    fn no_key_means_no_linear_calls() {
        let mut w = fake(Duration::from_secs(300));
        w.herdr
            .runner
            .set(&GIT, None, "/r\nuser/eng-325-widget-export-cli\n/r/.git\n");
        let state = w.tick(Instant::now()).unwrap();
        assert!(state.linear_missing_key);
        assert!(state.issues.is_empty());
        assert_eq!(curl_count(&w), 0);
        let mut w = into_actions(w);
        w.handle(Command::FetchIssue("ENG-325".into()));
        assert!(w
            .notice
            .as_deref()
            .unwrap()
            .contains("LINEAR_API_KEY not set"));
        assert!(w.launch.is_none());
    }

    #[test]
    fn fetch_issue_offers_main_checkouts_or_explains_why_not() {
        let w = fake(Duration::from_secs(300)).with_linear_key(Some("k".into()));
        let argv = curl(&["ENG-325"]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        w.herdr.runner.ok(&argv, None, ISSUE_REPLY);
        // The only workspace is not a git checkout.
        w.herdr.runner.ok(
            &["herdr", "pane", "list"],
            None,
            r#"{"id":"x","result":{"type":"pane_list","panes":[
              {"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","cwd":"/scratch"}]}}"#,
        );
        let mut w = into_actions(w);
        w.handle(Command::FetchIssue("ENG-325".into()));
        assert!(w.launch.is_none());
        assert_eq!(w.fetched, Some((1, "ENG-325".into())), "failure reported");
        assert!(
            w.notice.as_deref().unwrap().contains("no open workspace"),
            "{:?}",
            w.notice
        );
        w.herdr.runner.set(
            &["herdr", "workspace", "list"],
            None,
            r#"{"id":"x","result":{"type":"workspace_list","workspaces":[
              {"workspace_id":"w1","label":"proj","worktree":{"checkout_path":"/r","repo_root":"/r","is_linked_worktree":false,"repo_name":"r"}}]}}"#,
        );
        w.handle(Command::FetchIssue("eng-325".into()));
        let view = w.launch.clone().expect("launch view");
        assert_eq!(view.serial, 1);
        assert_eq!(view.issue.identifier, "ENG-325");
        assert_eq!(view.repos[0].label, "r — proj");
        assert!(w
            .herdr
            .runner
            .argvs()
            .iter()
            .all(|a| !a.contains("worktree create") && !a.contains("agent prompt")));
    }

    // --- Mastra Code memory ---

    const RESUMED: &str = "11111111-2222-4333-8444-555555555555";
    const BY_TITLE: &str = "66666666-7777-4888-9999-000000000000";
    const WIDGETS: &str = "/home/user/code/widgets";
    const DB: &str = "/db/mastra.db";

    fn mc_fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/mastracode/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// Mastra Code agents as `(pane, status, title)`, all in `/home/user/code/widgets`.
    fn mc_agents(agents: &[(&str, &str, &str)]) -> String {
        let list: Vec<serde_json::Value> = agents
            .iter()
            .map(|(pane, status, title)| {
                serde_json::json!({
                    "pane_id": pane, "workspace_id": "w1", "tab_id": "w1:t1",
                    "agent": "mastracode", "agent_status": status, "cwd": WIDGETS,
                    "terminal_title_stripped": format!("Mastra Code - {title}"),
                    "state_change_seq": 1
                })
            })
            .collect();
        serde_json::json!({"id": "x", "result": {"type": "agent_list", "agents": list}}).to_string()
    }

    fn process_info(pane: &str) -> [String; 5] {
        ["herdr", "pane", "process-info", "--pane", pane].map(String::from)
    }

    fn refs(argv: &[String]) -> Vec<&str> {
        argv.iter().map(String::as_str).collect()
    }

    fn recaps_key(ids: &[&str]) -> Vec<String> {
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        mastracode::recaps_argv(Path::new(DB), &ids)
            .unwrap()
            .unwrap()
    }

    fn titles_key(titles: &[&str]) -> Vec<String> {
        let panes: Vec<(String, String)> = titles
            .iter()
            .map(|t| (t.to_string(), WIDGETS.to_string()))
            .collect();
        mastracode::titles_argv(Path::new(DB), &panes)
            .unwrap()
            .unwrap()
    }

    /// `w1:p1` runs `--thread RESUMED`; `w1:p2` is a session-init pane titled
    /// "Dark mode toggle", which the title lookup resolves to `BY_TITLE`.
    fn mc_fake(agents: &[(&str, &str, &str)]) -> Watcher<FakeRunner> {
        let f = FakeRunner::new();
        f.ok(&["herdr", "agent", "list"], None, &mc_agents(agents))
            .ok(&["herdr", "workspace", "list"], None, WORKSPACES)
            .ok(
                &refs(&process_info("w1:p1")),
                None,
                &mc_fixture("process-info-resumed.json"),
            )
            .ok(
                &refs(&process_info("w1:p2")),
                None,
                &mc_fixture("process-info-session-init.json"),
            )
            .ok(
                &refs(&titles_key(&["Dark mode toggle"])),
                None,
                &mc_fixture("title-lookup.json"),
            )
            .otherwise(OK_REPLY);
        let rows = mc_fixture("recap-rows.json");
        for ids in [vec![RESUMED], vec![BY_TITLE], vec![RESUMED, BY_TITLE]] {
            f.ok(&refs(&recaps_key(&ids)), None, &rows);
        }
        Watcher::new(
            Herdr::with_bin(f, "herdr"),
            Settings {
                stall_threshold: Duration::from_secs(300),
                poll: Duration::from_millis(10),
            },
            Arc::new(AtomicBool::new(false)),
        )
        .with_mastra_db(Some(PathBuf::from(DB)))
    }

    fn count(w: &Watcher<FakeRunner>, prefix: &str) -> usize {
        w.herdr
            .runner
            .argvs()
            .iter()
            .filter(|a| a.starts_with(prefix))
            .count()
    }

    fn agent<'a>(s: &'a Snapshot, pane: &str) -> &'a AgentSnapshot {
        s.agents.iter().find(|a| a.info.pane_id == pane).unwrap()
    }

    #[test]
    fn panes_resolve_by_thread_argument_or_unique_title_with_recaps() {
        let mut w = mc_fake(&[
            ("w1:p1", "idle", "Resumed"),
            ("w1:p2", "idle", "Dark mode toggle"),
        ]);
        let s = w.tick(Instant::now()).unwrap();
        assert_eq!(count(&w, "herdr pane process-info"), 2);
        assert_eq!(count(&w, "sqlite3"), 2, "one title lookup, one recap read");
        let a = agent(&s, "w1:p1");
        assert_eq!(a.thread.as_deref(), Some(RESUMED));
        let recap = a.recap.as_ref().unwrap();
        assert_eq!(recap.lines.len(), 2);
        assert_eq!((recap.tasks_done, recap.tasks_total), (1, 3));
        let b = agent(&s, "w1:p2");
        assert_eq!(b.thread.as_deref(), Some(BY_TITLE));
        assert!(b.recap.is_some());
        assert_eq!(s.error, None);
    }

    #[test]
    fn unchanged_ticks_skip_process_info_and_sqlite_until_due() {
        let mut w = mc_fake(&[
            ("w1:p1", "idle", "Resumed"),
            ("w1:p2", "idle", "Dark mode toggle"),
        ]);
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.tick(t0 + Duration::from_secs(2)).unwrap();
        w.tick(t0 + Duration::from_secs(19)).unwrap();
        assert_eq!(count(&w, "herdr pane process-info"), 2);
        assert_eq!(count(&w, "sqlite3"), 2);
        w.tick(t0 + Duration::from_secs(21)).unwrap();
        assert_eq!(count(&w, "herdr pane process-info"), 2);
        assert_eq!(count(&w, "sqlite3"), 4, "20 s later: one refresh");
        w.tick(t0 + Duration::from_secs(5 * 60)).unwrap();
        assert_eq!(
            count(&w, "herdr pane process-info"),
            4,
            "re-checked every 5 min"
        );
    }

    #[test]
    fn a_status_change_reads_recaps_once() {
        let mut w = mc_fake(&[("w1:p1", "working", "Resumed")]);
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        assert_eq!(count(&w, "sqlite3"), 1, "no fallback pane, no title lookup");
        w.herdr.runner.set(
            &["herdr", "agent", "list"],
            None,
            &mc_agents(&[("w1:p1", "done", "Resumed")]),
        );
        w.tick(t0 + Duration::from_secs(2)).unwrap();
        w.tick(t0 + Duration::from_secs(4)).unwrap();
        assert_eq!(count(&w, "sqlite3"), 2);
        assert_eq!(count(&w, "herdr pane process-info"), 1);
    }

    #[test]
    fn a_title_change_rechecks_that_pane_and_re_resolves() {
        let mut w = mc_fake(&[
            ("w1:p1", "idle", "Resumed"),
            ("w1:p2", "idle", "Dark mode toggle"),
        ]);
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.herdr.runner.set(
            &["herdr", "agent", "list"],
            None,
            &mc_agents(&[("w1:p1", "idle", "Resumed"), ("w1:p2", "idle", "Renamed")]),
        );
        w.herdr
            .runner
            .ok(&refs(&titles_key(&["Renamed"])), None, "");
        let s = w.tick(t0 + Duration::from_secs(2)).unwrap();
        let argvs = w.herdr.runner.argvs();
        let infos: Vec<_> = argvs
            .iter()
            .filter(|a| a.contains("process-info"))
            .collect();
        assert_eq!(infos.len(), 3);
        assert!(infos[2].ends_with("w1:p2"));
        assert_eq!(
            agent(&s, "w1:p2").thread,
            None,
            "no thread has the new title"
        );
        assert_eq!(agent(&s, "w1:p2").recap, None);
        assert_eq!(agent(&s, "w1:p1").thread.as_deref(), Some(RESUMED));
    }

    #[test]
    fn a_title_match_that_turns_ambiguous_drops_the_thread() {
        let mut w = mc_fake(&[("w1:p2", "idle", "Dark mode toggle")]);
        let t0 = Instant::now();
        let s = w.tick(t0).unwrap();
        assert_eq!(agent(&s, "w1:p2").thread.as_deref(), Some(BY_TITLE));
        w.herdr.runner.set(
            &refs(&titles_key(&["Dark mode toggle"])),
            None,
            &mc_fixture("title-lookup-ambiguous.json"),
        );
        let s = w.tick(t0 + Duration::from_secs(61)).unwrap();
        assert_eq!(agent(&s, "w1:p2").thread, None);
        assert_eq!(agent(&s, "w1:p2").recap, None);
    }

    #[test]
    fn a_vanished_pane_is_evicted() {
        let mut w = mc_fake(&[
            ("w1:p1", "idle", "Resumed"),
            ("w1:p2", "idle", "Dark mode toggle"),
        ]);
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.herdr.runner.set(
            &["herdr", "agent", "list"],
            None,
            &mc_agents(&[("w1:p1", "idle", "Resumed")]),
        );
        w.tick(t0 + Duration::from_secs(2)).unwrap();
        assert!(!w.threads.probes.contains_key("w1:p2"));
        assert!(!w.threads.threads.contains_key("w1:p2"));
        assert!(!w.since.contains_key("w1:p2"));
    }

    #[test]
    fn a_failed_read_keeps_the_previous_recaps_and_reports_it() {
        let mut w = mc_fake(&[("w1:p1", "idle", "Resumed")]);
        let t0 = Instant::now();
        let before = agent(&w.tick(t0).unwrap(), "w1:p1").recap.clone();
        assert!(before.is_some());
        // No canned reply for this path: sqlite3's "output" is not JSON rows.
        w.mastra_db = Some(PathBuf::from("/db/other.db"));
        w.herdr.runner.set(
            &["herdr", "agent", "list"],
            None,
            &mc_agents(&[("w1:p1", "done", "Resumed")]),
        );
        let s = w.tick(t0 + Duration::from_secs(2)).unwrap();
        assert_eq!(count(&w, "sqlite3"), 2);
        assert_eq!(agent(&s, "w1:p1").recap, before);
        assert_eq!(agent(&s, "w1:p1").thread.as_deref(), Some(RESUMED));
        assert!(s.error.unwrap().starts_with("mastra code memory:"));
        w.tick(t0 + Duration::from_secs(10)).unwrap();
        assert_eq!(count(&w, "sqlite3"), 2, "no retry before RECAP_RETRY");
        w.tick(t0 + Duration::from_secs(13)).unwrap();
        assert_eq!(count(&w, "sqlite3"), 3, "retried ~10 s after the failure");
    }

    #[test]
    fn quitting_waits_only_for_commands_that_change_something() {
        let reads = [
            Command::FetchPrs,
            Command::FetchIssues,
            Command::TabVisible(Tab::Prs),
            Command::PrepareStart {
                id: "i".into(),
                identifier: "ENG-1".into(),
            },
        ];
        assert!(reads.iter().all(Command::read_only));
        let writes = [
            Command::Reply {
                pane: "w1:p1".into(),
                text: "hi".into(),
                stall_hash: None,
            },
            Command::StartPr {
                repo: "o/r".into(),
                head_repo: None,
                head: "b".into(),
                number: 1,
                prompt: "p".into(),
            },
            Command::RerunFailed {
                repo: "o/r".into(),
                head: "b".into(),
                number: 1,
            },
            Command::OpenPrWorktree {
                repo: "o/r".into(),
                head_repo: None,
                head: "b".into(),
                number: 1,
            },
            Command::FactorySend { message: msg("hi") },
        ];
        assert!(!writes.iter().any(Command::read_only));
    }

    fn msg(body: &str) -> crate::features::factory::prompt::Message {
        crate::features::factory::prompt::Message {
            body: body.into(),
            signature: Some("sig".into()),
        }
    }

    fn slack_actions() -> Actions<FakeRunner> {
        Actions::new(
            Herdr::with_bin(FakeRunner::new(), "herdr"),
            Arc::new(AtomicBool::new(false)),
        )
        .with_slack(Some(Slack {
            token: "xoxp-test".into(),
            channel: "C0000000001".into(),
        }))
    }

    #[test]
    fn factory_send_posts_once_and_reports_success() {
        let mut a = slack_actions();
        let argv = crate::integrations::slack::post_argv("C0000000001", "hi", Some("sig"));
        a.herdr.runner.ok(
            &refs(&argv),
            None,
            include_str!("../../../tests/fixtures/slack/ok.json"),
        );
        a.handle(Command::FactorySend { message: msg("hi") });
        assert_eq!(a.factory_sent, Some((1, true)));
        assert_eq!(a.notice.as_deref(), Some("sent to Factory in Slack"));
        assert_eq!(a.herdr.runner.calls().len(), 1);
    }

    #[test]
    fn factory_send_failures_and_missing_config_say_why_without_the_text() {
        let mut a = slack_actions();
        let argv =
            crate::integrations::slack::post_argv("C0000000001", "secret-ish text", Some("sig"));
        a.herdr.runner.ok(
            &refs(&argv),
            None,
            include_str!("../../../tests/fixtures/slack/error.json"),
        );
        a.handle(Command::FactorySend {
            message: msg("secret-ish text"),
        });
        assert_eq!(a.factory_sent, Some((1, false)));
        assert_eq!(a.notice.as_deref(), Some("factory: Slack: not_in_channel"));

        let mut a = Actions::new(
            Herdr::with_bin(FakeRunner::new(), "herdr"),
            Arc::new(AtomicBool::new(false)),
        );
        a.handle(Command::FactorySend { message: msg("x") });
        assert_eq!(a.factory_sent, Some((1, false)));
        assert!(a.notice.unwrap().contains("SLACK_USER_TOKEN"));
        assert!(a.herdr.runner.calls().is_empty());
    }

    #[test]
    fn a_re_probe_that_finds_a_new_thread_refreshes_at_once() {
        let mut w = mc_fake(&[("w1:p1", "idle", "Resumed")]);
        let t0 = Instant::now();
        w.tick(t0).unwrap();
        w.tick(t0 + Duration::from_secs(4 * 60 + 58)).unwrap();
        assert_eq!(count(&w, "sqlite3"), 2);
        // The pane now runs a different thread (seen by the 5-minute re-probe).
        let other = mc_fixture("process-info-resumed.json").replace(RESUMED, BY_TITLE);
        w.herdr
            .runner
            .set(&refs(&process_info("w1:p1")), None, &other);
        let s = w.tick(t0 + Duration::from_secs(5 * 60)).unwrap();
        assert_eq!(count(&w, "herdr pane process-info"), 2);
        assert_eq!(count(&w, "sqlite3"), 3, "refreshed in the same tick");
        assert_eq!(agent(&s, "w1:p1").thread.as_deref(), Some(BY_TITLE));
    }

    #[test]
    fn without_a_database_no_process_info_or_sqlite_runs() {
        let mut w = mc_fake(&[("w1:p1", "idle", "Resumed")]).with_mastra_db(None);
        let s = w.tick(Instant::now()).unwrap();
        assert_eq!(count(&w, "herdr pane process-info"), 0);
        assert_eq!(count(&w, "sqlite3"), 0);
        assert_eq!(agent(&s, "w1:p1").thread, None);
    }

    #[test]
    fn the_asking_question_is_in_the_snapshot() {
        let mut w = mc_fake(&[("w1:p1", "working", "Resumed")]);
        let visible = [
            "herdr", "pane", "read", "w1:p1", "--source", "visible", "--lines", "60",
        ];
        w.herdr
            .runner
            .ok(&visible, None, &screen("mc-ask-user.txt"));
        let s = w.tick(Instant::now()).unwrap();
        let a = agent(&s, "w1:p1");
        assert!(a.asking);
        assert_eq!(a.question.as_deref(), Some("Pick a color"));
    }

    #[test]
    fn a_stalled_snapshot_carries_its_screen_hash() {
        let screen = "⠏ Sleeping 7s\n • working 7s · esc to interrupt";
        let want = crate::core::fnv1a(crate::features::inbox::stall::normalize(screen).as_bytes());
        let mut w = fake(Duration::ZERO);
        let s = w.tick(Instant::now()).unwrap();
        assert!(s.agents[0].stalled.is_some());
        assert_eq!(s.agents[0].stall_hash, Some(want));
        let mut w = fake(Duration::from_secs(300));
        let s = w.tick(Instant::now()).unwrap();
        assert!(s.agents[0].stalled.is_none());
        assert_eq!(s.agents[0].stall_hash, None);
    }

    #[test]
    fn state_since_moves_only_when_status_asking_or_stalled_changes() {
        let mut w = mc_fake(&[("w1:p1", "idle", "Resumed")]);
        let t0 = Instant::now();
        let first = agent(&w.tick(t0).unwrap(), "w1:p1").state_since_unix_ms;
        assert!(first > 0);
        thread::sleep(Duration::from_millis(5));
        let same =
            agent(&w.tick(t0 + Duration::from_secs(2)).unwrap(), "w1:p1").state_since_unix_ms;
        assert_eq!(same, first);
        w.herdr.runner.set(
            &["herdr", "agent", "list"],
            None,
            &mc_agents(&[("w1:p1", "done", "Resumed")]),
        );
        let moved =
            agent(&w.tick(t0 + Duration::from_secs(4)).unwrap(), "w1:p1").state_since_unix_ms;
        assert!(moved > first);

        // A successor seeded from the record keeps it.
        let record = w.export(t0, crate::core::unix_ms());
        let mut next = mc_fake(&[("w1:p1", "done", "Resumed")]);
        next.seed(&record, t0, crate::core::unix_ms());
        thread::sleep(Duration::from_millis(5));
        let kept = agent(&next.tick(t0).unwrap(), "w1:p1").state_since_unix_ms;
        assert_eq!(kept, moved);
    }

    #[test]
    fn old_state_without_the_new_fields_still_reads() {
        let old = r#"{"info":{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"t"},
            "workspace_label":"w","branch":null,"pr":null,"stalled":null,"asking":false}"#;
        let a: AgentSnapshot = serde_json::from_str(old).unwrap();
        assert_eq!(
            (a.thread, a.recap, a.question, a.stall_hash),
            (None, None, None, None)
        );
        assert_eq!(a.state_since_unix_ms, 0);
        let rec: ObserverRecord =
            serde_json::from_str(r#"{"alerted":[],"sent":[],"stall":[]}"#).unwrap();
        assert!(rec.since.is_empty());
    }

    const SEND: [&str; 5] = ["herdr", "agent", "prompt", PANE, "keep going"];

    fn reply_actions(status: &str, visible: &str, recent: &str) -> Actions<FakeRunner> {
        let runner = FakeRunner::new();
        runner.ok(&["herdr", "agent", "list"], None, &agents_json(status));
        runner.ok(&VISIBLE, None, visible);
        runner.ok(&READ, None, recent);
        runner.ok(&SEND, None, r#"{"id":"x","result":{}}"#);
        Actions::new(
            Herdr::with_bin(runner, "herdr"),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn reply(actions: &mut Actions<FakeRunner>, stall_hash: Option<u64>) {
        actions.handle(Command::Reply {
            pane: PANE.into(),
            text: "keep going".into(),
            stall_hash,
        });
    }

    fn sends(actions: &Actions<FakeRunner>) -> usize {
        actions
            .herdr
            .runner
            .argvs()
            .iter()
            .filter(|a| a.starts_with("herdr agent prompt"))
            .count()
    }

    fn hash_of(screen: &str) -> u64 {
        crate::core::fnv1a(stall::normalize(screen).as_bytes())
    }

    #[test]
    fn reply_to_an_idle_agent_sends_once_without_waiting() {
        let mut a = reply_actions("idle", &screen("mc-idle-done.txt"), "");
        reply(&mut a, None);
        assert_eq!(
            a.herdr.runner.argvs(),
            [
                "herdr agent list".to_string(),
                VISIBLE.join(" "),
                SEND.join(" "),
            ],
            "fresh status, visible-screen ask check, then one prompt without --wait"
        );
        assert_eq!(a.replied, Some((1, true)));
        assert_eq!(a.notice.as_deref(), Some("reply sent to w1:p1"));
    }

    #[test]
    fn reply_is_refused_when_an_idle_agent_shows_a_running_turn() {
        let busy = screen("mc-working-a.txt");
        let mut a = reply_actions("idle", &busy, &busy);
        reply(&mut a, None);
        assert_eq!(sends(&a), 0);
        assert!(a.notice.as_deref().unwrap().contains("is working"));
    }

    #[test]
    fn reply_to_a_done_agent_is_sent() {
        let mut a = reply_actions("done", &screen("mc-idle-done.txt"), "");
        reply(&mut a, None);
        assert_eq!(sends(&a), 1);
    }

    #[test]
    fn reply_is_refused_when_the_visible_screen_asks_whatever_the_status() {
        for status in ["idle", "done", "working"] {
            let ask = screen("mc-ask-user.txt");
            let mut a = reply_actions(status, &ask, &ask);
            reply(&mut a, Some(hash_of(&ask)));
            assert_eq!(sends(&a), 0, "{status}");
            assert_eq!(a.replied, Some((1, false)));
            assert!(
                a.notice.as_deref().unwrap().contains("is asking"),
                "{status}"
            );
        }
    }

    #[test]
    fn reply_to_a_stalled_agent_needs_the_same_screen() {
        let stuck = screen("mc-working-a.txt");
        let mut a = reply_actions("working", &stuck, &stuck);
        reply(&mut a, Some(hash_of(&stuck)));
        assert_eq!(sends(&a), 1, "still stalled on the same screen");
        assert!(a.herdr.runner.argvs().contains(&READ.join(" ")));

        let moved = format!("{stuck}\nwrote src/new_file.rs");
        let mut a = reply_actions("working", &stuck, &moved);
        reply(&mut a, Some(hash_of(&stuck)));
        assert_eq!(sends(&a), 0, "the screen moved: working again");
        assert_eq!(
            a.notice.as_deref(),
            Some("not sent: w1:p1 is working again")
        );

        let mut a = reply_actions("working", &stuck, &stuck);
        reply(&mut a, None);
        assert_eq!(sends(&a), 0, "idle in the snapshot, working now: no hash");
        assert_eq!(a.notice.as_deref(), Some("not sent: w1:p1 is working"));
    }

    #[test]
    fn an_answered_picker_in_scrollback_does_not_block_a_reply() {
        // The stall read is Recent and still holds the old picker; only the
        // visible screen decides asking.
        let old = screen("mc-ask-user.txt");
        let mut a = reply_actions("working", &screen("mc-working-a.txt"), &old);
        reply(&mut a, Some(hash_of(&old)));
        assert_eq!(sends(&a), 1);
    }

    #[test]
    fn reply_to_a_blocked_or_missing_agent_is_refused() {
        let mut a = reply_actions("blocked", "", "");
        reply(&mut a, None);
        assert_eq!(sends(&a), 0);
        assert_eq!(a.notice.as_deref(), Some("not sent: w1:p1 is blocked"));

        let mut a = reply_actions("idle", "", "");
        a.handle(Command::Reply {
            pane: "w9:p9".into(),
            text: "keep going".into(),
            stall_hash: None,
        });
        assert_eq!(sends(&a), 0);
        assert_eq!(a.notice.as_deref(), Some("not sent: w9:p9 is gone"));
    }

    #[test]
    fn reply_errors_never_carry_the_text() {
        let a_fail = reply_actions("idle", "", "");
        a_fail.herdr.runner.fail(&SEND, None);
        let mut a = a_fail;
        reply(&mut a, None);
        assert_eq!(a.replied, Some((1, false)));
        assert_eq!(a.notice.as_deref(), Some("reply to w1:p1 timed out"));

        let runner = FakeRunner::new();
        runner.ok(&["herdr", "agent", "list"], None, &agents_json("idle"));
        runner.ok(&VISIBLE, None, "");
        runner.respond(
            &SEND,
            None,
            Output {
                status: 1,
                stdout: r#"{"error":{"code":"agent_blocked","message":"keep going"}}"#.into(),
                stderr: "keep going".into(),
            },
        );
        let mut a = Actions::new(
            Herdr::with_bin(runner, "herdr"),
            Arc::new(AtomicBool::new(false)),
        );
        reply(&mut a, None);
        let shown = format!("{:?} {:?}", a.notice, a.last_error);
        assert!(!shown.contains("keep going"), "{shown}");
        assert_eq!(
            a.notice.as_deref(),
            Some("reply to w1:p1 refused: agent_blocked")
        );
    }

    // ---- PRs tab ----

    const PR_LISTS: &str = include_str!("../../../tests/fixtures/gh/pr-lists.json");
    const PR_DETAIL: &str = include_str!("../../../tests/fixtures/gh/pr-detail.json");
    const REV_PARSE: [&str; 10] = [
        "git",
        "-C",
        "/c/main",
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-dir",
        "--git-common-dir",
        "--abbrev-ref",
        "HEAD",
    ];

    fn joined(argv: &[String]) -> Vec<&str> {
        argv.iter().map(String::as_str).collect()
    }

    /// One main checkout of `o/r` at /c/main on `main`, plus an optional
    /// linked worktree of `o/r` on `wt_branch`.
    fn pr_actions(wt_branch: Option<&str>) -> Actions<FakeRunner> {
        let runner = FakeRunner::new();
        let mut workspaces = vec![serde_json::json!({"workspace_id":"w1","label":"repo",
            "worktree":{"checkout_path":"/c/main","repo_root":"/c/main","is_linked_worktree":false,"repo_name":"r"}})];
        runner.ok(
            &REV_PARSE,
            None,
            "/c/main\n/c/main/.git\n/c/main/.git\nmain\n",
        );
        runner.ok(
            &["git", "-C", "/c/main", "remote", "get-url", "origin"],
            None,
            "https://github.com/O/R.git\n",
        );
        if let Some(b) = wt_branch {
            workspaces.push(serde_json::json!({"workspace_id":"w2","label":"wt",
                "worktree":{"checkout_path":"/c/wt","repo_root":"/c/main","is_linked_worktree":true,"repo_name":"r"}}));
            let mut rp = REV_PARSE;
            rp[2] = "/c/wt";
            runner.ok(
                &rp,
                None,
                &format!("/c/wt\n/c/main/.git/worktrees/wt\n/c/main/.git\n{b}\n"),
            );
            runner.ok(
                &["git", "-C", "/c/wt", "remote", "get-url", "origin"],
                None,
                "git@github.com:o/r.git\n",
            );
        }
        runner.ok(
            &["herdr", "workspace", "list"],
            None,
            &serde_json::json!({"result":{"workspaces":workspaces}}).to_string(),
        );
        runner.ok(&joined(&github::lists_argv()), None, PR_LISTS);
        Actions::new(
            Herdr::with_bin(runner, "herdr"),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn gh_calls(a: &Actions<FakeRunner>) -> usize {
        a.herdr
            .runner
            .argvs()
            .iter()
            .filter(|c| c.starts_with("gh "))
            .count()
    }

    #[test]
    fn the_prs_tab_fetches_only_while_shown_and_every_two_minutes() {
        let mut a = pr_actions(None);
        let now = Instant::now();
        assert!(!a.prs_due(now), "Agents is shown at start");
        a.handle(Command::TabVisible(Tab::Prs));
        assert!(a.prs_due(now));
        a.fetch_prs();
        assert_eq!(gh_calls(&a), 1);
        let data = a.prs.data.clone().unwrap();
        assert_eq!(data.lists.assigned.len(), 6);
        assert_eq!(data.checkouts.len(), 1);
        assert!(!a.prs_due(Instant::now()));
        assert!(a.prs_due(Instant::now() + PRS_EVERY));
        a.handle(Command::TabVisible(Tab::Agents));
        assert!(
            !a.prs_due(Instant::now() + PRS_EVERY * 3),
            "hidden: no cadence"
        );
        a.handle(Command::FetchPrs);
        assert!(!a.prs_due(Instant::now()), "r only counts while shown");
    }

    #[test]
    fn repeated_fetch_requests_coalesce_into_one() {
        let mut a = pr_actions(None);
        a.handle(Command::TabVisible(Tab::Prs));
        a.handle(Command::FetchPrs);
        a.handle(Command::TabVisible(Tab::Prs));
        a.handle(Command::FetchPrs);
        while a.prs_due(Instant::now()) {
            a.fetch_prs();
        }
        assert_eq!(gh_calls(&a), 1);
    }

    #[test]
    fn a_failed_fetch_keeps_the_previous_lists_and_says_why() {
        let mut a = pr_actions(None);
        // Queued behind the fixture: the first fetch succeeds, the second fails.
        a.herdr.runner.respond(
            &joined(&github::lists_argv()),
            None,
            crate::core::runner::Output {
                status: 1,
                stdout: String::new(),
                stderr: "HTTP 502".into(),
            },
        );
        a.handle(Command::TabVisible(Tab::Prs));
        a.fetch_prs();
        let before = a.prs.data.clone();
        assert!(before.is_some());
        a.handle(Command::FetchPrs);
        a.fetch_prs();
        assert_eq!(a.prs.data, before);
        assert_eq!(a.prs.error.as_deref(), Some("gh: HTTP 502"));
    }

    #[test]
    fn the_cache_is_written_and_read_back() {
        let dir = std::env::temp_dir().join(format!("werkbank-prcache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prs-s.json");
        let mut a = pr_actions(None).with_pr_cache(path.clone());
        assert_eq!(a.prs.data, None);
        a.handle(Command::TabVisible(Tab::Prs));
        a.fetch_prs();
        let b = pr_actions(None).with_pr_cache(path.clone());
        assert_eq!(b.prs.data, a.prs.data, "shown before any fetch");
        assert_eq!(gh_calls(&b), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_detail_answers_with_the_requests_serial() {
        let mut a = pr_actions(None);
        let argv = github::detail_argv("mastra-ai/mastra", 30148).unwrap();
        a.herdr.runner.ok(&joined(&argv), None, PR_DETAIL);
        a.handle(Command::FetchPrDetail {
            serial: 7,
            repo: "mastra-ai/mastra".into(),
            number: 30148,
        });
        let d = a.pr_detail.clone().unwrap();
        assert_eq!((d.serial, d.number), (7, 30148));
        assert_eq!(d.detail.unwrap().pr.number, 30148);
    }

    fn open_wt(a: &mut Actions<FakeRunner>, head_repo: &str) {
        a.handle(Command::OpenPrWorktree {
            repo: "o/r".into(),
            head_repo: Some(head_repo.into()),
            head: "feat/7".into(),
            number: 7,
        });
    }

    /// `gh pr checkout` and `herdr worktree open` answer for PR #7.
    fn gh_made(a: &Actions<FakeRunner>) {
        let gh = pr::checkout_argv("o/r", 7, "/wt/main/feat-7");
        let gh: Vec<&str> = gh.iter().map(String::as_str).collect();
        a.herdr.runner.ok(&gh, Some("/c/main"), "").ok(
            &[
                "herdr",
                "worktree",
                "open",
                "--cwd",
                "/c/main",
                "--path",
                "/wt/main/feat-7",
                "--label",
                "#7",
                "--focus",
            ],
            None,
            include_str!("../../../tests/fixtures/worktree-open.json"),
        );
    }

    #[test]
    fn a_fork_pr_gets_a_worktree_but_no_automatic_setup() {
        let mut a = pr_actions(None);
        gh_made(&a);
        open_wt(&mut a, "someone/r");
        assert_eq!(
            a.notice.as_deref(),
            Some("#7: worktree /wt/r/feat-7 · fork, setup skipped: run setup-worktree once you trust it")
        );
    }

    #[test]
    fn worktree_needs_an_open_checkout_of_the_repo() {
        let mut a = pr_actions(None);
        a.handle(Command::OpenPrWorktree {
            repo: "x/y".into(),
            head_repo: Some("x/y".into()),
            head: "feat/7".into(),
            number: 7,
        });
        assert_eq!(
            a.notice.as_deref(),
            Some("#7: no open workspace has a checkout of x/y")
        );
        assert!(!a
            .herdr
            .runner
            .argvs()
            .iter()
            .any(|c| c.contains("worktree create")));
    }

    #[test]
    fn an_existing_worktree_is_focused_not_created() {
        let mut a = pr_actions(Some("feat/7"));
        a.herdr.runner.ok(
            &["herdr", "workspace", "focus", "w2"],
            None,
            r#"{"result":{}}"#,
        );
        open_wt(&mut a, "O/R");
        assert_eq!(a.notice.as_deref(), Some("#7: went to wt"));
        let calls = a.herdr.runner.argvs();
        assert!(calls.contains(&"herdr workspace focus w2".to_string()));
        assert!(!calls
            .iter()
            .any(|c| c.contains("worktree create") || c.contains("fetch")));
    }

    #[test]
    fn a_new_worktree_comes_from_gh_pr_checkout_then_herdr_opens_it() {
        let mut a = pr_actions(None);
        gh_made(&a);
        open_wt(&mut a, "o/r");
        assert_eq!(a.notice.as_deref(), Some("#7: worktree /wt/r/feat-7"));
        let calls = a.herdr.runner.argvs();
        let gh = calls.iter().position(|c| c.starts_with("gh pr checkout 7"));
        let open = calls
            .iter()
            .position(|c| c.starts_with("herdr worktree open"));
        assert!(gh.is_some() && gh < open, "{calls:?}");
        assert!(!calls
            .iter()
            .any(|c| c.contains("fetch") || c.contains("worktree create")));
    }

    fn start_pr(a: &mut Actions<FakeRunner>, pane: &str) {
        a.ready_wait = ReadyWait {
            attempts: 2,
            interval: Duration::ZERO,
        };
        a.herdr
            .runner
            .ok(&["herdr", "pane", "run", pane, "mastracode"], None, "")
            .ok(
                &["herdr", "agent", "list"],
                None,
                &format!(
                    r#"{{"result":{{"type":"agent_list","agents":[{{"pane_id":"{pane}","workspace_id":"w2","tab_id":"w2:t1","agent":"mastracode","agent_status":"idle"}}]}}}}"#
                ),
            )
            .ok(
                &[
                    "herdr", "agent", "prompt", pane, "secret prompt", "--wait", "--until",
                    "working", "--timeout", "15000",
                ],
                None,
                r#"{"result":{}}"#,
            );
        a.handle(Command::StartPr {
            repo: "o/r".into(),
            head_repo: Some("O/R".into()),
            head: "feat/7".into(),
            number: 7,
            prompt: "secret prompt".into(),
        });
    }

    #[test]
    fn s_on_an_open_worktree_starts_in_a_new_tab_never_its_busy_pane() {
        let mut a = pr_actions(Some("feat/7"));
        a.herdr.runner.ok(
            &[
                "herdr",
                "tab",
                "create",
                "--workspace",
                "w2",
                "--cwd",
                "/c/wt",
                "--label",
                "#7",
                "--focus",
            ],
            None,
            r#"{"result":{"root_pane":{"pane_id":"w2:p9"},"tab":{"tab_id":"w2:t9"}}}"#,
        );
        start_pr(&mut a, "w2:p9");
        assert_eq!(
            a.notice.as_deref(),
            Some("#7: agent started in /c/wt (w2:p9)")
        );
        let calls = a.herdr.runner.argvs();
        let tab = calls.iter().position(|c| c.contains("tab create")).unwrap();
        let run = calls.iter().position(|c| c.contains("pane run")).unwrap();
        let prompt = calls
            .iter()
            .position(|c| c.contains("agent prompt"))
            .unwrap();
        assert!(tab < run && run < prompt, "{calls:?}");
        assert!(!calls.iter().any(|c| c.contains("worktree create")));
    }

    #[test]
    fn s_without_a_worktree_makes_one_and_starts_in_its_root_pane() {
        let mut a = pr_actions(None);
        gh_made(&a);
        start_pr(&mut a, "w3:p1");
        let notice = a.notice.clone().unwrap();
        assert!(notice.starts_with("#7: agent started in "), "{notice}");
        assert!(!a
            .herdr
            .runner
            .argvs()
            .iter()
            .any(|c| c.contains("tab create")));
    }

    #[test]
    fn r_reruns_failed_runs_and_says_how_many() {
        let mut a = pr_actions(None);
        a.herdr.runner.ok(
            &joined(&github::runs_argv("o/r", "feat/7")),
            None,
            r#"[{"databaseId":4,"headSha":"s","conclusion":"failure"}]"#,
        );
        a.herdr
            .runner
            .ok(&joined(&github::rerun_argv("o/r", 4)), None, "");
        let rerun = |head: &str| Command::RerunFailed {
            repo: "o/r".into(),
            head: head.into(),
            number: 7,
        };
        a.handle(rerun("feat/7"));
        assert_eq!(a.notice.as_deref(), Some("#7: rerunning 1 failed run"));
        let before = a.herdr.runner.argvs().len();
        a.handle(rerun("-x"));
        assert!(a.notice.as_deref().unwrap().starts_with("rerun #7: "));
        assert_eq!(
            a.herdr.runner.argvs().len(),
            before,
            "a bad branch runs nothing"
        );
    }

    #[test]
    fn a_failed_pr_start_never_shows_the_prompt() {
        let mut a = pr_actions(Some("feat/7"));
        a.handle(Command::StartPr {
            repo: "o/r".into(),
            head_repo: Some("o/r".into()),
            head: "feat/7".into(),
            number: 7,
            prompt: "secret prompt".into(),
        });
        let notice = a.notice.clone().unwrap();
        assert!(notice.starts_with("start #7: "), "{notice}");
        assert!(!notice.contains("secret"), "{notice}");
    }

    // --- Issues tab ---

    const ISSUE_DETAIL: &str = include_str!("../../../tests/fixtures/linear/issue-detail.json");
    const TEAM_STATES: &str = include_str!("../../../tests/fixtures/linear/team-states.json");
    const UPDATED: &str = r#"{"data":{"issueUpdate":{"success":true}}}"#;
    const ISSUE_CREATE: [&str; 8] = [
        "herdr",
        "worktree",
        "create",
        "--cwd",
        "/repo",
        "--branch",
        "user/eng-1-x",
        "--focus",
    ];

    fn issue_actions() -> Actions<FakeRunner> {
        let mut a = Actions::new(
            Herdr::with_bin(FakeRunner::new(), "herdr"),
            Arc::new(AtomicBool::new(false)),
        )
        .with_linear_key(Some("k".into()));
        a.ready_wait = ReadyWait {
            attempts: 2,
            interval: Duration::ZERO,
        };
        a
    }

    fn linear_reply(a: &Actions<FakeRunner>, body: &str, reply: &str) {
        let argv = linear::post_argv(body);
        a.herdr.runner.ok(&joined(&argv), None, reply);
    }

    /// The recorded detail read, with `edit` applied to its issue.
    fn detail_with(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut v: serde_json::Value = serde_json::from_str(ISSUE_DETAIL).unwrap();
        edit(&mut v["data"]["issue"]);
        v.to_string()
    }

    fn read_of(detail: &str) -> MyIssue {
        linear::parse_detail(detail).unwrap().issue
    }

    /// Worktree create, `mastracode` started, the agent reports idle.
    fn worktree_ready(a: &Actions<FakeRunner>) {
        a.herdr
            .runner
            .ok(
                &ISSUE_CREATE,
                None,
                include_str!("../../../tests/fixtures/worktree-create.json"),
            )
            .ok(&["herdr", "pane", "run", "w2:p1", "mastracode"], None, "")
            .ok(
                &["herdr", "agent", "list"],
                None,
                r#"{"id":"x","result":{"type":"agent_list","agents":[{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":"mastracode","agent_status":"idle"}]}}"#,
            );
    }

    fn prompt_argv(issue: &MyIssue) -> Vec<String> {
        [
            "herdr",
            "agent",
            "prompt",
            "w2:p1",
            &linear::first_prompt(&issue.as_issue()),
            "--wait",
            "--until",
            "working",
            "--timeout",
            "15000",
        ]
        .map(String::from)
        .to_vec()
    }

    fn start(a: &mut Actions<FakeRunner>, first: MyIssue) -> String {
        a.handle(Command::Start {
            issue: Box::new(first),
            repo_root: "/repo".into(),
        });
        a.notice.clone().unwrap()
    }

    fn position(calls: &[String], needle: &str) -> usize {
        calls
            .iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("no call with {needle}: {calls:?}"))
    }

    #[test]
    fn start_goes_worktree_then_reread_then_claim_then_prompt() {
        let mut a = issue_actions();
        let fresh = read_of(ISSUE_DETAIL);
        worktree_ready(&a);
        linear_reply(&a, &linear::detail_request("issue-1"), ISSUE_DETAIL);
        linear_reply(&a, &linear::team_states_request("team-1111"), TEAM_STATES);
        // Backlog and already ours: move to the first started state, no assign.
        let claim = linear::update_request("issue-1", serde_json::json!({"stateId": "st-prog"}));
        linear_reply(&a, &claim, UPDATED);
        a.herdr.runner.ok(
            &joined(&prompt_argv(&fresh)),
            None,
            r#"{"id":"x","result":{}}"#,
        );
        let notice = start(&mut a, fresh.clone());
        assert!(notice.starts_with("launched ENG-1 in "), "{notice}");
        let calls = a.herdr.runner.argvs();
        let order = [
            position(&calls, "worktree create"),
            position(&calls, "viewer { id name }"),
            position(&calls, "team(id"),
            position(&calls, "issueUpdate"),
            position(&calls, "agent prompt"),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?} {calls:?}");
        assert_eq!(
            calls.iter().filter(|c| c.contains("issueUpdate")).count(),
            1
        );
        assert!(!calls.iter().any(|c| c.contains("assigneeId")));
        assert!(calls
            .iter()
            .all(|c| !c.contains(" k ") && !c.ends_with(" k")));
    }

    #[test]
    fn start_assigns_an_unassigned_issue_to_the_viewer() {
        let mut a = issue_actions();
        let detail = detail_with(|i| i["assignee"] = serde_json::Value::Null);
        worktree_ready(&a);
        linear_reply(&a, &linear::detail_request("issue-1"), &detail);
        linear_reply(&a, &linear::team_states_request("team-1111"), TEAM_STATES);
        let claim = linear::update_request(
            "issue-1",
            serde_json::json!({"stateId": "st-prog", "assigneeId": "user-viewer"}),
        );
        linear_reply(&a, &claim, UPDATED);
        let fresh = read_of(&detail);
        a.herdr.runner.ok(
            &joined(&prompt_argv(&fresh)),
            None,
            r#"{"id":"x","result":{}}"#,
        );
        with_issue_list(&mut a);
        let listed = |a: &Actions<FakeRunner>| {
            let data = a.issue_list.data.as_ref().unwrap();
            let i = data.mine.issues.iter().find(|i| i.id == "issue-1").unwrap();
            i.assignee.clone()
        };
        Arc::make_mut(a.issue_list.data.as_mut().unwrap())
            .mine
            .issues
            .iter_mut()
            .filter(|i| i.id == "issue-1")
            .for_each(|i| i.assignee = None);
        let notice = start(&mut a, fresh);
        assert!(notice.starts_with("launched ENG-1"), "{notice}");
        let calls = a.herdr.runner.argvs();
        assert!(calls.iter().any(|c| c.contains("assigneeId")), "{calls:?}");
        let who = listed(&a).expect("the list shows the claim at once");
        assert_eq!(
            (who.id.as_str(), who.name.as_str()),
            ("user-viewer", "User Viewer")
        );
    }

    #[test]
    fn start_keeps_an_already_started_state_and_sends_no_mutation() {
        let mut a = issue_actions();
        let detail = detail_with(|i| {
            i["state"] = serde_json::json!({"id":"st-review","name":"In Review","type":"started"})
        });
        worktree_ready(&a);
        linear_reply(&a, &linear::detail_request("issue-1"), &detail);
        let fresh = read_of(&detail);
        a.herdr.runner.ok(
            &joined(&prompt_argv(&fresh)),
            None,
            r#"{"id":"x","result":{}}"#,
        );
        let notice = start(&mut a, fresh);
        assert!(notice.starts_with("launched ENG-1"), "{notice}");
        let calls = a.herdr.runner.argvs();
        assert!(
            !calls.iter().any(|c| c.contains("issueUpdate")),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| c.contains("team(id")), "{calls:?}");
        assert!(calls.iter().any(|c| c.contains("agent prompt")));
    }

    #[test]
    fn start_refuses_closed_or_someone_elses_issue_before_any_worktree() {
        for (edit, why) in [
            (
                serde_json::json!({"state": {"id":"st-done","name":"Done","type":"completed"}}),
                "it is Done",
            ),
            (
                serde_json::json!({"state": {"id":"st-dup","name":"Duplicate","type":"duplicate"}}),
                "it is Duplicate",
            ),
            (
                serde_json::json!({"state": {"id":"st-c","name":"Canceled","type":"canceled"}}),
                "it is Canceled",
            ),
            (
                serde_json::json!({"assignee": {"id":"user-other","name":"Someone Else"}}),
                "assigned to Someone Else",
            ),
        ] {
            let mut a = issue_actions();
            let detail = detail_with(|i| {
                for (k, v) in edit.as_object().unwrap() {
                    i[k] = v.clone();
                }
            });
            linear_reply(&a, &linear::detail_request("issue-1"), &detail);
            a.handle(Command::PrepareStart {
                id: "issue-1".into(),
                identifier: "ENG-1".into(),
            });
            let notice = a.notice.clone().unwrap();
            assert!(
                notice.contains(why) && notice.contains("not starting"),
                "{notice}"
            );
            assert!(a.launch.is_none());
            let calls = a.herdr.runner.argvs();
            assert!(
                calls
                    .iter()
                    .all(|c| !c.contains("worktree") && !c.contains("workspace list")),
                "{calls:?}"
            );
        }
    }

    #[test]
    fn prepare_start_offers_the_repo_picker_with_the_first_read() {
        let mut a = issue_actions();
        linear_reply(&a, &linear::detail_request("issue-1"), ISSUE_DETAIL);
        a.herdr.runner.ok(
            &["herdr", "workspace", "list"],
            None,
            r#"{"id":"x","result":{"type":"workspace_list","workspaces":[{"workspace_id":"w1","label":"repo","worktree":{"checkout_path":"/repo","repo_root":"/repo","is_linked_worktree":false,"repo_name":"repo"}}]}}"#,
        );
        a.handle(Command::PrepareStart {
            id: "issue-1".into(),
            identifier: "ENG-1".into(),
        });
        let view = a.launch.as_ref().expect("picker");
        assert_eq!(view.repos.len(), 1);
        assert_eq!(view.start.as_deref(), Some(&read_of(ISSUE_DETAIL)));
        assert!(!a
            .herdr
            .runner
            .argvs()
            .iter()
            .any(|c| c.contains("worktree create")));
    }

    #[test]
    fn start_stops_when_the_issue_changed_while_preparing() {
        for edit in [
            serde_json::json!({"assignee": {"id":"user-other","name":"Someone Else"}}),
            serde_json::json!({"branchName": "user/eng-1-renamed"}),
            serde_json::json!({"team": {"id":"team-2222","key":"OPS"}}),
            serde_json::json!({"state": {"id":"st-done","name":"Done","type":"completed"}}),
        ] {
            let mut a = issue_actions();
            worktree_ready(&a);
            let detail = detail_with(|i| {
                for (k, v) in edit.as_object().unwrap() {
                    i[k] = v.clone();
                }
            });
            linear_reply(&a, &linear::detail_request("issue-1"), &detail);
            let notice = start(&mut a, read_of(ISSUE_DETAIL));
            assert!(
                notice.contains("issue changed while preparing")
                    && notice.contains("worktree kept, Linear not updated, no prompt sent"),
                "{notice}"
            );
            let calls = a.herdr.runner.argvs();
            assert!(calls.iter().any(|c| c.contains("worktree create")));
            assert!(
                calls
                    .iter()
                    .all(|c| !c.contains("issueUpdate") && !c.contains("agent prompt")),
                "{calls:?}"
            );
        }
    }

    #[test]
    fn a_failed_claim_sends_no_prompt() {
        let mut a = issue_actions();
        worktree_ready(&a);
        linear_reply(&a, &linear::detail_request("issue-1"), ISSUE_DETAIL);
        linear_reply(&a, &linear::team_states_request("team-1111"), TEAM_STATES);
        let claim = linear::update_request("issue-1", serde_json::json!({"stateId": "st-prog"}));
        linear_reply(&a, &claim, r#"{"errors":[{"message":"forbidden"}]}"#);
        let notice = start(&mut a, read_of(ISSUE_DETAIL));
        assert!(
            notice.contains("claim failed") && notice.contains("no prompt sent"),
            "{notice}"
        );
        assert!(!a
            .herdr
            .runner
            .argvs()
            .iter()
            .any(|c| c.contains("agent prompt")));
    }

    fn with_issue_list(a: &mut Actions<FakeRunner>) {
        let mine: linear::MyIssues = linear::parse_my_issues(include_str!(
            "../../../tests/fixtures/linear/my-issues.json"
        ))
        .unwrap();
        a.issue_list.data = Some(Arc::new(IssueData {
            mine,
            checkouts: vec![],
            fetched_unix_ms: 1,
        }));
    }

    fn listed_state(a: &Actions<FakeRunner>, id: &str) -> String {
        let data = a.issue_list.data.as_ref().unwrap();
        let i = data.mine.issues.iter().find(|i| i.id == id).unwrap();
        i.state.id.clone()
    }

    #[test]
    fn a_status_change_patches_the_list_or_keeps_the_old_state_on_error() {
        let mut a = issue_actions();
        with_issue_list(&mut a);
        let id = a.issue_list.data.as_ref().unwrap().mine.issues[0]
            .id
            .clone();
        let before = listed_state(&a, &id);
        let target = WorkflowState {
            id: "st-target".into(),
            name: "Target".into(),
            kind: "unstarted".into(),
        };
        let body = linear::update_request(&id, serde_json::json!({"stateId": "st-target"}));
        linear_reply(&a, &body, r#"{"errors":[{"message":"nope"}]}"#);
        let set = Command::SetIssueState {
            issue_id: id.clone(),
            identifier: "X-1".into(),
            state: target.clone(),
        };
        a.handle(set.clone());
        assert_eq!(listed_state(&a, &id), before);
        assert!(a.notice.as_deref().unwrap().starts_with("status X-1:"));
        let argv = linear::post_argv(&body);
        a.herdr.runner.set(&joined(&argv), None, UPDATED);
        a.handle(set);
        assert_eq!(a.notice.as_deref(), Some("X-1 → Target"));
        assert_eq!(listed_state(&a, &id), "st-target");
        assert_eq!(a.notice.as_deref(), Some("X-1 → Target"));
    }

    #[test]
    fn the_issues_tab_drives_its_own_fetch_cadence() {
        let mut a = issue_actions();
        let now = Instant::now();
        assert!(!a.issues_due(now));
        a.handle(Command::TabVisible(Tab::Issues));
        assert!(a.issues_due(now));
        linear_reply(
            &a,
            &linear::my_issues_request(),
            include_str!("../../../tests/fixtures/linear/my-issues.json"),
        );
        a.herdr.runner.ok(
            &["herdr", "workspace", "list"],
            None,
            r#"{"id":"x","result":{"type":"workspace_list","workspaces":[]}}"#,
        );
        a.fetch_issues();
        assert!(a.issue_list.error.is_none(), "{:?}", a.issue_list.error);
        assert!(!a.issues_due(Instant::now()));
        assert!(a.issues_due(Instant::now() + PRS_EVERY));
        a.handle(Command::FetchIssues);
        a.handle(Command::FetchIssues);
        assert!(a.issues_due(Instant::now()));
        a.handle(Command::TabVisible(Tab::Agents));
        assert!(!a.issues_due(Instant::now() + PRS_EVERY));
    }

    #[test]
    fn a_failed_issue_fetch_keeps_the_list_and_says_so() {
        let mut a = issue_actions();
        with_issue_list(&mut a);
        linear_reply(
            &a,
            &linear::my_issues_request(),
            r#"{"errors":[{"message":"down"}]}"#,
        );
        a.fetch_issues();
        assert!(a.issue_list.data.is_some());
        assert!(a.issue_list.error.as_deref().unwrap().contains("down"));
    }
}
