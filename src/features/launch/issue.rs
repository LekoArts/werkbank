use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::core::herdr::{Herdr, WorkspaceInfo};
use crate::core::runner::Runner;
use crate::core::stop::Guard;
use crate::features::setup;
use crate::integrations::linear::{first_prompt, Issue};

pub const PROMPT_WAIT_MS: u64 = 15_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoChoice {
    pub repo_root: String,
    pub label: String,
}

/// Main checkouts of open workspaces, one per repo. Herdr only reports
/// `worktree` for repos it has made worktrees in, so other workspaces are
/// resolved with git from a pane's cwd; linked worktrees are skipped either way.
pub fn repo_choices<R: Runner>(
    herdr: &Herdr<R>,
    workspaces: &[WorkspaceInfo],
    guard: Guard,
) -> Result<Vec<RepoChoice>> {
    let mut panes = None;
    let mut out: Vec<(String, RepoChoice)> = Vec::new();
    for ws in workspaces {
        let found = match &ws.worktree {
            Some(wt) if wt.is_linked_worktree => None,
            Some(wt) => Some((wt.repo_root.clone(), wt.repo_name.clone())),
            None => {
                if panes.is_none() {
                    guard()?;
                    panes = Some(herdr.pane_list()?);
                }
                let cwd = panes.iter().flatten().find_map(|p| {
                    (p.workspace_id == ws.workspace_id)
                        .then(|| p.foreground_cwd.clone().or(p.cwd.clone()))
                        .flatten()
                });
                match cwd {
                    Some(cwd) => {
                        guard()?;
                        main_checkout_of(&herdr.runner, &cwd)
                    }
                    None => None,
                }
            }
        };
        let Some((root, name)) = found else { continue };
        let key = std::fs::canonicalize(&root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| root.clone());
        if out.iter().any(|(k, _)| *k == key) {
            continue;
        }
        out.push((
            key,
            RepoChoice {
                repo_root: root,
                label: format!("{name} — {}", ws.label),
            },
        ));
    }
    Ok(out.into_iter().map(|(_, c)| c).collect())
}

/// `(toplevel, name)` when `cwd` is inside a main checkout (not a linked worktree).
pub(crate) fn main_checkout_of<R: Runner>(runner: &R, cwd: &str) -> Option<(String, String)> {
    let out = runner
        .run(
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
        )
        .ok()
        .filter(|o| o.success())?;
    let mut lines = out.stdout.lines();
    let (top, git_dir, common) = (lines.next()?, lines.next()?, lines.next()?);
    if git_dir != common {
        return None;
    }
    let name = Path::new(top).file_name()?.to_string_lossy().into_owned();
    Some((top.to_string(), name))
}

/// The `owner/name` GitHub repo of `checkout`'s `origin` remote.
pub fn origin_repo<R: Runner>(runner: &R, checkout: &str) -> Option<String> {
    let out = runner
        .run(
            &["git", "-C", checkout, "remote", "get-url", "origin"],
            None,
            None,
        )
        .ok()
        .filter(|o| o.success())?;
    crate::integrations::github::repo_of_remote(&out.stdout)
}

/// How long `launch` waits for Mastra Code to report in: `attempts` polls `interval` apart.
#[derive(Debug, Clone, Copy)]
pub struct ReadyWait {
    pub attempts: u32,
    pub interval: Duration,
}

impl ReadyWait {
    pub const DEFAULT: ReadyWait = ReadyWait {
        attempts: 60,
        interval: Duration::from_secs(1),
    };
}

/// Creates a worktree on the issue's branch, starts Mastra Code in its root
/// pane and, once that agent reports idle, sends the issue as the first prompt.
/// The prompt only ever goes to the pane this function started.
pub fn launch<R: Runner>(
    herdr: &Herdr<R>,
    issue: &Issue,
    repo_root: &str,
    wait: ReadyWait,
    guard: Guard,
) -> Result<String> {
    let prepared = prepare_worktree(herdr, issue, repo_root, wait, guard)?;
    send_first_prompt(herdr, issue, &prepared, guard)
}

/// A worktree with Mastra Code started and idle in its root pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub pane: String,
    pub checkout_path: String,
    pub copy_notice: Option<String>,
}

/// The issue's branch, or an error naming the issue.
pub fn branch_of(issue: &Issue) -> Result<&str> {
    issue
        .branch_name
        .as_deref()
        .filter(|b| !b.trim().is_empty())
        .ok_or_else(|| anyhow!("{} has no branchName in Linear", issue.identifier))
}

/// Worktree on the issue's branch, setup copy wait, `mastracode` in its root
/// pane, then waits for that agent to report idle. Sends nothing.
pub fn prepare_worktree<R: Runner>(
    herdr: &Herdr<R>,
    issue: &Issue,
    repo_root: &str,
    wait: ReadyWait,
    guard: Guard,
) -> Result<Prepared> {
    let branch = branch_of(issue)?;
    guard()?;
    let since = std::time::SystemTime::now();
    let wt = herdr.worktree_create(repo_root, branch, true)?;
    let pane = wt.root_pane_id;
    let copy_notice = setup::after_create(&wt.checkout_path, branch, since);
    start_agent(herdr, &pane, &wt.checkout_path, wait, guard)?;
    Ok(Prepared {
        pane,
        checkout_path: wt.checkout_path,
        copy_notice,
    })
}

/// Runs `mastracode` in `pane` and waits for it to report idle.
pub fn start_agent<R: Runner>(
    herdr: &Herdr<R>,
    pane: &str,
    checkout_path: &str,
    wait: ReadyWait,
    guard: Guard,
) -> Result<()> {
    guard()?;
    herdr.pane_run(pane, "mastracode")?;
    for attempt in 0..wait.attempts {
        if attempt > 0 {
            std::thread::sleep(wait.interval);
        }
        guard()?;
        if herdr.agent_list()?.iter().any(|a| {
            a.pane_id == pane
                && a.agent.as_deref() == Some("mastracode")
                && matches!(a.agent_status.as_str(), "idle" | "done")
        }) {
            return Ok(());
        }
    }
    bail!(
        "Mastra Code not ready in {pane} after {}s; no prompt sent ({checkout_path})",
        (wait.interval * wait.attempts).as_secs(),
    )
}

/// Sends the issue as the first prompt to the pane `prepare_worktree` started.
pub fn send_first_prompt<R: Runner>(
    herdr: &Herdr<R>,
    issue: &Issue,
    prepared: &Prepared,
    guard: Guard,
) -> Result<String> {
    guard()?;
    let pane = &prepared.pane;
    herdr.agent_prompt(pane, &first_prompt(issue), PROMPT_WAIT_MS)?;
    let launched = format!(
        "launched {} in {} ({pane})",
        issue.identifier, prepared.checkout_path
    );
    Ok(match &prepared.copy_notice {
        Some(n) => format!("{launched} · {n}"),
        None => launched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::{FakeRunner, Output};
    use crate::core::stop::Stopped;
    use crate::integrations::linear::IssueState;
    use std::cell::Cell;

    const WORKTREE: &str = include_str!("../../../tests/fixtures/worktree-create.json");
    const BRANCH: &str = "user/eng-325-widget-export-cli";

    fn no_stop() -> Result<()> {
        Ok(())
    }

    fn issue() -> Issue {
        Issue {
            identifier: "ENG-325".into(),
            title: "\"Widget export\": CLI flag".into(),
            description: Some("New CLI capabilities".into()),
            url: "https://linear.app/example/issue/ENG-325/widget-export-cli".into(),
            branch_name: Some(BRANCH.into()),
            state: Some(IssueState {
                name: "Done".into(),
            }),
        }
    }

    fn git_dirs(cwd: &str) -> [&str; 8] {
        [
            "git",
            "-C",
            cwd,
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ]
    }

    #[test]
    fn repo_choices_are_main_checkouts_deduped_by_repo() {
        let ws: Vec<WorkspaceInfo> = serde_json::from_str(
            r#"[
              {"workspace_id":"w1","label":"mastra","worktree":{"checkout_path":"/r/mastra","repo_root":"/r/mastra","is_linked_worktree":false,"repo_name":"mastra"}},
              {"workspace_id":"w2","label":"mastra 2","worktree":{"checkout_path":"/r/mastra","repo_root":"/r/mastra","is_linked_worktree":false,"repo_name":"mastra"}},
              {"workspace_id":"w3","label":"feat","worktree":{"checkout_path":"/wt/feat","repo_root":"/r/mastra","is_linked_worktree":true,"repo_name":"mastra"}},
              {"workspace_id":"w4","label":"scratch"},
              {"workspace_id":"w5","label":"docs","worktree":{"checkout_path":"/r/docs","repo_root":"/r/docs","is_linked_worktree":false,"repo_name":"docs"}},
              {"workspace_id":"w6","label":"tower"},
              {"workspace_id":"w7","label":"hand-made wt"},
              {"workspace_id":"w8","label":"no panes"}
            ]"#,
        )
        .unwrap();
        let f = FakeRunner::new();
        f.ok(
            &["herdr", "pane", "list"],
            None,
            r#"{"id":"x","result":{"type":"pane_list","panes":[
              {"pane_id":"w4:p1","workspace_id":"w4","tab_id":"w4:t1","cwd":"/tmp"},
              {"pane_id":"w6:p1","workspace_id":"w6","tab_id":"w6:t1","cwd":"/r","foreground_cwd":"/r/tower/src"},
              {"pane_id":"w7:p1","workspace_id":"w7","tab_id":"w7:t1","cwd":"/wt/hand"}]}}"#,
        )
        .respond(
            &git_dirs("/tmp"),
            None,
            Output {
                status: 128,
                stdout: String::new(),
                stderr: "fatal: not a git repository".into(),
            },
        )
        .ok(
            &git_dirs("/r/tower/src"),
            None,
            "/r/tower\n/r/tower/.git\n/r/tower/.git\n",
        )
        .ok(
            &git_dirs("/wt/hand"),
            None,
            "/wt/hand\n/r/mastra/.git/worktrees/hand\n/r/mastra/.git\n",
        );
        let herdr = Herdr::with_bin(f, "herdr");
        let choices = repo_choices(&herdr, &ws, &no_stop).unwrap();
        let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["mastra — mastra", "docs — docs", "tower — tower"]);
        assert_eq!(choices[2].repo_root, "/r/tower");
        let argvs = herdr.runner.argvs();
        assert_eq!(argvs.iter().filter(|a| *a == "herdr pane list").count(), 1);
    }

    fn agents(status: &str, agent: &str) -> String {
        format!(
            r#"{{"id":"x","result":{{"type":"agent_list","agents":[{{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":{agent},"agent_status":"{status}"}}]}}}}"#
        )
    }

    const CREATE: [&str; 8] = [
        "herdr", "worktree", "create", "--cwd", "/repo", "--branch", BRANCH, "--focus",
    ];

    fn launch_runner() -> FakeRunner {
        let f = FakeRunner::new();
        f.ok(&CREATE, None, WORKTREE)
            .ok(&["herdr", "pane", "run", "w2:p1", "mastracode"], None, "")
            .ok(
                &["herdr", "agent", "list"],
                None,
                &agents("unknown", "null"),
            )
            .ok(
                &["herdr", "agent", "list"],
                None,
                &agents("idle", "\"mastracode\""),
            );
        f
    }

    const WAIT: ReadyWait = ReadyWait {
        attempts: 3,
        interval: Duration::ZERO,
    };

    #[test]
    fn launch_creates_worktree_starts_mastracode_waits_then_prompts() {
        let f = launch_runner();
        let prompt = first_prompt(&issue());
        let prompt_argv = [
            "herdr",
            "agent",
            "prompt",
            "w2:p1",
            &prompt,
            "--wait",
            "--until",
            "working",
            "--timeout",
            "15000",
        ];
        f.ok(
            &prompt_argv,
            None,
            r#"{"id":"x","result":{"type":"agent_prompted"}}"#,
        );
        let herdr = Herdr::with_bin(f, "herdr");
        let summary = launch(&herdr, &issue(), "/repo", WAIT, &no_stop).unwrap();
        assert!(summary.starts_with("launched ENG-325 in "), "{summary}");
        let argvs = herdr.runner.argvs();
        assert_eq!(
            argvs,
            [
                CREATE.join(" "),
                "herdr pane run w2:p1 mastracode".to_string(),
                "herdr agent list".to_string(),
                "herdr agent list".to_string(),
                prompt_argv.join(" "),
            ]
        );
    }

    #[test]
    fn no_prompt_when_mastracode_never_reports_ready() {
        let f = FakeRunner::new();
        f.ok(&CREATE, None, WORKTREE)
            .ok(&["herdr", "pane", "run", "w2:p1", "mastracode"], None, "")
            .ok(
                &["herdr", "agent", "list"],
                None,
                &agents("unknown", "null"),
            );
        let herdr = Herdr::with_bin(f, "herdr");
        let err = launch(&herdr, &issue(), "/repo", WAIT, &no_stop)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no prompt sent"), "{err}");
        let argvs = herdr.runner.argvs();
        assert_eq!(argvs.iter().filter(|a| *a == "herdr agent list").count(), 3);
        assert!(argvs.iter().all(|a| !a.contains("agent prompt")));
    }

    #[test]
    fn stopping_during_the_poll_sends_no_prompt() {
        let herdr = Herdr::with_bin(launch_runner(), "herdr");
        let calls = Cell::new(0);
        // Allow worktree create and pane run, then stop during the poll.
        let guard = || -> Result<()> {
            calls.set(calls.get() + 1);
            if calls.get() > 3 {
                Err(Stopped.into())
            } else {
                Ok(())
            }
        };
        let err = launch(&herdr, &issue(), "/repo", WAIT, &guard).unwrap_err();
        assert!(err.is::<Stopped>());
        assert!(herdr
            .runner
            .argvs()
            .iter()
            .all(|a| !a.contains("agent prompt")));
    }

    #[test]
    fn issue_without_branch_name_creates_nothing() {
        let herdr = Herdr::with_bin(FakeRunner::new(), "herdr");
        let mut bare = issue();
        bare.branch_name = None;
        let err = launch(&herdr, &bare, "/repo", WAIT, &no_stop)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no branchName"), "{err}");
        assert!(herdr.runner.calls().is_empty());
    }
}
