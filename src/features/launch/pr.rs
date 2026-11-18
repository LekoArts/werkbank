//! A worktree for any PR of a repo, made by `gh pr checkout --worktree` and
//! opened in Herdr. Fork PRs work too: gh fetches `refs/pull/N/head` and points
//! the branch's push at the fork.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use crate::core::herdr::{check_branch, Herdr};
use crate::core::runner::Runner;
use crate::core::stop::Guard;
use crate::features::inbox::prs::{self, Checkout};
use crate::features::setup;

/// What the `prefix+y` popup was given.
#[derive(Debug, PartialEq)]
pub enum Target {
    Branch(String),
    /// A PR number, with the `owner/name` of its repo when typed as a URL.
    Pr {
        number: u64,
        repo: Option<String>,
    },
}

/// `123`, `#123` or a `github.com/<owner>/<repo>/pull/123` URL is a PR;
/// anything else is a branch name.
pub fn target(raw: &str) -> Result<Target> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("type a branch, PR number or PR URL");
    }
    if let Ok(number) = s.strip_prefix('#').unwrap_or(s).parse::<u64>() {
        return Ok(Target::Pr { number, repo: None });
    }
    let path = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let path = path.strip_prefix("www.").unwrap_or(path);
    if let Some(rest) = path.strip_prefix("github.com/") {
        let parts: Vec<&str> = rest.split(['/', '?', '#']).collect();
        return match parts.as_slice() {
            [owner, name, "pull", n, ..] if !owner.is_empty() && !name.is_empty() => {
                let number = n.parse().map_err(|_| anyhow!("not a PR URL: {s}"))?;
                Ok(Target::Pr {
                    number,
                    repo: Some(format!("{owner}/{name}")),
                })
            }
            _ => bail!("not a PR URL: {s}"),
        };
    }
    Ok(Target::Branch(s.to_string()))
}

/// The PR's head branch and whether it comes from a fork.
#[derive(Debug, Clone, PartialEq)]
pub struct PrHead {
    pub number: u64,
    pub head: String,
    pub fork: bool,
}

pub fn view_argv(repo: &str, number: u64) -> Vec<String> {
    [
        "gh",
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        repo,
        "--json",
        "number,headRefName,isCrossRepository",
    ]
    .map(String::from)
    .to_vec()
}

/// Looks the PR up on GitHub.
pub fn view<R: Runner>(runner: &R, repo: &str, number: u64) -> Result<PrHead> {
    let argv = view_argv(repo, number);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = runner.run(&argv, None, None)?;
    if !out.success() {
        bail!("#{number}: {}", first_line(&out.stderr));
    }
    let v: Value =
        serde_json::from_str(&out.stdout).map_err(|_| anyhow!("#{number}: gh gave bad JSON"))?;
    let head = v["headRefName"]
        .as_str()
        .ok_or_else(|| anyhow!("#{number}: no head branch"))?;
    Ok(PrHead {
        number,
        head: head.to_string(),
        fork: v["isCrossRepository"].as_bool().unwrap_or(true),
    })
}

/// A PR's worktree: already open in a workspace, or just made.
#[derive(Debug)]
pub enum PrCheckout {
    Open(Checkout),
    /// `setup` is a notice about setup, if there is anything to say.
    Made {
        pane: String,
        path: String,
        setup: Option<String>,
    },
}

/// Runs setup (copy local files, install, build) on a fresh worktree; returns a
/// notice to show. A parameter so tests never start a real setup.
pub type Setup = fn(&setup::Target) -> Option<String>;

/// The open checkout of the PR's branch. A fork's branch only counts in a
/// linked worktree: the main checkout's same-named branch is someone else's.
pub fn open_checkout<'a>(
    checkouts: &'a [Checkout],
    repo: &str,
    pr: &PrHead,
) -> Option<&'a Checkout> {
    if !pr.fork {
        return prs::checkout_of(checkouts, repo, Some(repo), &pr.head);
    }
    checkouts
        .iter()
        .find(|c| !c.main && c.repo.eq_ignore_ascii_case(repo) && c.branch == pr.head)
}

/// Where Herdr would put this branch's worktree: `<dir>/<repo name>/<branch, / → ->`.
pub fn worktree_path(dir: &Path, main: &str, head: &str) -> PathBuf {
    let name = Path::new(main)
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned());
    dir.join(name).join(head.replace('/', "-"))
}

pub fn checkout_argv(repo: &str, number: u64, path: &str) -> Vec<String> {
    [
        "gh",
        "pr",
        "checkout",
        &number.to_string(),
        "--repo",
        repo,
        "--worktree",
        path,
    ]
    .map(String::from)
    .to_vec()
}

/// The PR's open checkout, else `gh pr checkout --worktree` from `main` (a
/// checkout of `repo`) and `herdr worktree open`. Fork worktrees get no
/// automatic setup: it would run the fork's install scripts beside copied
/// local files.
pub fn checkout<R: Runner>(
    herdr: &Herdr<R>,
    checkouts: &[Checkout],
    main: &str,
    repo: &str,
    pr: &PrHead,
    dirs: (&Path, Setup),
    guard: Guard,
) -> Result<PrCheckout> {
    let (dir, setup) = dirs;
    let number = pr.number;
    if check_branch(&pr.head).is_err() {
        bail!("#{number}: unusable branch name");
    }
    if let Some(c) = open_checkout(checkouts, repo, pr) {
        return Ok(PrCheckout::Open(c.clone()));
    }
    let path = worktree_path(dir, main, &pr.head);
    if path.exists() {
        bail!("#{number}: {} already exists", path.display());
    }
    let path = path.to_string_lossy().into_owned();
    guard()?;
    let argv = checkout_argv(repo, number, &path);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = herdr.runner.run(&argv, None, Some(Path::new(main)))?;
    if !out.success() {
        bail!("#{number}: gh pr checkout: {}", checkout_error(&out.stderr));
    }
    guard()?;
    let wt = herdr.worktree_open(main, &path, &format!("#{number}"), true)?;
    let skipped = "fork, setup skipped: run setup-worktree once you trust it";
    let setup = if pr.fork {
        Some(skipped.to_string())
    } else {
        setup(&setup::Target {
            workspace_id: wt.workspace_id.clone(),
            repo_root: main.to_string(),
            checkout: wt.checkout_path.clone(),
            branch: pr.head.clone(),
        })
    };
    Ok(PrCheckout::Made {
        pane: wt.root_pane_id,
        path: wt.checkout_path,
        setup,
    })
}

/// git's `fatal:` line if there is one, else the last line; a single-branch
/// clone gets a hint, since gh needs `origin/<branch>` to track.
fn checkout_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let line = lines
        .iter()
        .find(|l| l.starts_with("fatal:"))
        .or(lines.last())
        .copied()
        .unwrap_or("failed");
    if line.contains("is not a branch") {
        return format!(
            "{line} (origin fetches one branch only? git config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*')"
        );
    }
    line.to_string()
}

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("failed")
}

/// Herdr's `[worktrees] directory`, default `~/.herdr/worktrees`.
pub fn worktrees_dir() -> PathBuf {
    let text = crate::features::inbox::theme::config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    dir_from_config(&text, &std::env::var("HOME").unwrap_or_default())
}

fn dir_from_config(toml: &str, home: &str) -> PathBuf {
    let mut table = "";
    let mut dir = "~/.herdr/worktrees".to_string();
    for line in toml.lines() {
        let line = line.split(" #").next().unwrap_or("").trim();
        if line.starts_with('[') {
            table = if line == "[worktrees]" {
                "worktrees"
            } else {
                ""
            };
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if table == "worktrees" && k.trim() == "directory" {
                dir = v.trim().trim_matches('"').to_string();
            }
        }
    }
    match dir.strip_prefix("~/") {
        Some(rest) => Path::new(home).join(rest),
        None => PathBuf::from(dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::FakeRunner;

    fn no_guard() -> Result<()> {
        Ok(())
    }

    fn no_setup(_: &setup::Target) -> Option<String> {
        None
    }

    fn noted_setup(t: &setup::Target) -> Option<String> {
        Some(format!("setup {} in {}", t.branch, t.workspace_id))
    }

    #[test]
    fn numbers_hashes_and_pr_urls_are_prs_anything_else_a_branch() {
        let pr = |number, repo: Option<&str>| Target::Pr {
            number,
            repo: repo.map(String::from),
        };
        assert_eq!(target(" 123 ").unwrap(), pr(123, None));
        assert_eq!(target("#45").unwrap(), pr(45, None));
        assert_eq!(
            target("https://github.com/o/r/pull/7/files?x=1").unwrap(),
            pr(7, Some("o/r"))
        );
        assert_eq!(
            target("github.com/o/r/pull/8#top").unwrap(),
            pr(8, Some("o/r"))
        );
        assert_eq!(target("feat/x").unwrap(), Target::Branch("feat/x".into()));
        assert!(target("https://github.com/o/r/issues/7").is_err());
        assert!(target("  ").is_err());
    }

    #[test]
    fn worktrees_go_where_herdr_puts_its_own() {
        assert_eq!(
            dir_from_config("", "/h"),
            PathBuf::from("/h/.herdr/worktrees")
        );
        assert_eq!(
            dir_from_config(
                "[ui]\ndirectory = \"x\"\n[worktrees]\ndirectory = \"/wt\" # mine\n",
                "/h"
            ),
            PathBuf::from("/wt")
        );
        assert_eq!(
            worktree_path(Path::new("/wt"), "/c/main", "you/eng-1"),
            PathBuf::from("/wt/main/you-eng-1")
        );
    }

    #[test]
    fn view_reads_the_head_and_whether_it_is_a_fork() {
        let r = FakeRunner::new();
        let argv = view_argv("o/r", 7);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        r.ok(
            &argv,
            None,
            r#"{"headRefName":"feat/7","isCrossRepository":true,"number":7}"#,
        );
        assert_eq!(
            view(&r, "o/r", 7).unwrap(),
            PrHead {
                number: 7,
                head: "feat/7".into(),
                fork: true
            }
        );
        assert!(view(&r, "o/r", 8).is_err());
    }

    fn main_checkout() -> Checkout {
        Checkout {
            workspace_id: "w1".into(),
            label: "r".into(),
            path: "/c/main".into(),
            repo: "o/r".into(),
            branch: "feat/7".into(),
            main: true,
        }
    }

    fn pr(fork: bool) -> PrHead {
        PrHead {
            number: 7,
            head: "feat/7".into(),
            fork,
        }
    }

    fn made(fork: bool, setup: Setup) -> (Herdr<FakeRunner>, Result<PrCheckout>) {
        let h = Herdr::with_bin(FakeRunner::new(), "herdr");
        let gh = checkout_argv("o/r", 7, "/wt/main/feat-7");
        let gh: Vec<&str> = gh.iter().map(String::as_str).collect();
        h.runner.ok(&gh, Some("/c/main"), "");
        h.runner.ok(
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
        let got = checkout(
            &h,
            &[],
            "/c/main",
            "o/r",
            &pr(fork),
            (Path::new("/wt"), setup),
            &no_guard,
        );
        (h, got)
    }

    #[test]
    fn a_same_repo_pr_is_checked_out_by_gh_opened_in_herdr_then_set_up() {
        let (h, got) = made(false, noted_setup);
        let Ok(PrCheckout::Made { pane, path, setup }) = got else {
            panic!("{got:?}")
        };
        assert_eq!((pane.as_str(), path.as_str()), ("w3:p1", "/wt/r/feat-7"));
        assert_eq!(setup.as_deref(), Some("setup feat/7 in w3"));
        let calls = h.runner.argvs();
        assert!(
            calls[0].starts_with("gh pr checkout 7 --repo o/r --worktree"),
            "{calls:?}"
        );
        assert!(calls[1].starts_with("herdr worktree open"), "{calls:?}");
        assert!(!calls
            .iter()
            .any(|c| c.contains("--force") || c.contains(" -f")));
    }

    #[test]
    fn a_fork_pr_is_checked_out_but_never_set_up_automatically() {
        let (_, got) = made(true, |_| panic!("no setup for forks"));
        let Ok(PrCheckout::Made { setup, .. }) = got else {
            panic!("{got:?}")
        };
        assert_eq!(
            setup.as_deref(),
            Some("fork, setup skipped: run setup-worktree once you trust it")
        );
    }

    #[test]
    fn an_open_checkout_is_returned_without_any_call() {
        let h = Herdr::with_bin(FakeRunner::new(), "herdr");
        let mut linked = main_checkout();
        linked.main = false;
        linked.path = "/c/wt".into();
        let both = [main_checkout(), linked];
        let dirs = (Path::new("/wt"), no_setup as Setup);
        let got = checkout(&h, &both, "/c/main", "o/r", &pr(false), dirs, &no_guard).unwrap();
        assert!(matches!(got, PrCheckout::Open(c) if c.path == "/c/wt"));
        // A fork's branch never matches the main checkout's same-named one.
        assert!(open_checkout(&both[..1], "o/r", &pr(true)).is_none());
        assert!(open_checkout(&both, "o/r", &pr(true)).is_some());
        assert!(h.runner.argvs().is_empty());
    }

    #[test]
    fn a_gh_failure_opens_nothing_and_says_why() {
        let h = Herdr::with_bin(FakeRunner::new(), "herdr");
        let gh = checkout_argv("o/r", 7, "/wt/main/feat-7");
        let gh: Vec<&str> = gh.iter().map(String::as_str).collect();
        h.runner.respond(
            &gh,
            Some("/c/main"),
            crate::core::runner::Output {
                status: 1,
                stdout: String::new(),
                stderr: "Preparing worktree (new branch 'feat/7')\nfatal: cannot set up tracking information; starting point 'origin/feat/7' is not a branch\nfailed to run git: exit status 255\n".into(),
            },
        );
        let err = checkout(
            &h,
            &[],
            "/c/main",
            "o/r",
            &pr(false),
            (Path::new("/wt"), no_setup),
            &no_guard,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.starts_with("#7: gh pr checkout: fatal: cannot set up tracking"),
            "{err}"
        );
        assert!(err.contains("remote.origin.fetch"), "{err}");
        assert_eq!(h.runner.argvs().len(), 1, "no herdr call");
        let bad = PrHead {
            head: "-x".into(),
            ..pr(false)
        };
        assert!(checkout(
            &h,
            &[],
            "/c/main",
            "o/r",
            &bad,
            (Path::new("/wt"), no_setup),
            &no_guard
        )
        .is_err());
        assert_eq!(h.runner.argvs().len(), 1, "a bad branch runs nothing");
    }
}
