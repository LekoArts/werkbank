//! Launch presets: a new Mastra Code tab, and a new worktree + Mastra Code
//! (a branch, PR number or PR URL typed into the `worktree-prompt` popup).
//! Neither sends a prompt.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;
use serde::Deserialize;

use crate::core::herdr::{Herdr, Sound, WorktreeInfo};
use crate::core::runner::Runner;
use crate::features::inbox::prs;
use crate::features::launch::{issue, pr};
use crate::features::setup;

/// The parts of `HERDR_PLUGIN_CONTEXT_JSON` the presets use. Herdr only adds
/// `worktree` for repos it has made worktrees in (see the recorded fixtures).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Context {
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub workspace_cwd: Option<String>,
    #[serde(default)]
    pub focused_pane_cwd: Option<String>,
    #[serde(default)]
    pub worktree: Option<WorktreeInfo>,
}

impl Context {
    pub fn parse(json: &str) -> Result<Context> {
        serde_json::from_str(json).map_err(|e| anyhow!("bad HERDR_PLUGIN_CONTEXT_JSON: {e}"))
    }

    pub fn from_env() -> Result<Context> {
        Context::parse(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").unwrap_or_default())
    }

    /// The focused pane's cwd, else the workspace's.
    pub fn cwd(&self) -> Option<&str> {
        self.focused_pane_cwd
            .as_deref()
            .or(self.workspace_cwd.as_deref())
            .filter(|c| !c.is_empty())
    }
}

/// New tab labelled `mc` in the current workspace, at the focused pane's cwd,
/// running `mastracode`. Returns the new pane id.
pub fn launch_tab<R: Runner>(herdr: &Herdr<R>, ctx: &Context, workspace: &str) -> Result<String> {
    let workspace = if workspace.is_empty() {
        ctx.workspace_id.as_str()
    } else {
        workspace
    };
    if workspace.is_empty() {
        bail!("no workspace in the plugin context");
    }
    let cwd = ctx
        .cwd()
        .ok_or_else(|| anyhow!("no cwd in the plugin context"))?;
    let pane = herdr.tab_create(workspace, cwd, "mc", true)?;
    herdr.pane_run(&pane, "mastracode")?;
    Ok(pane)
}

pub fn launch_worktree<R: Runner>(herdr: &Herdr<R>) -> Result<()> {
    herdr.plugin_pane_open("worktree-prompt", &[]).map(|_| ())
}

/// The main repo root for the focused workspace: Herdr's `worktree.repo_root`,
/// else git from the focused cwd (the parent of the common `.git` dir, so a
/// linked worktree resolves to its main checkout too).
pub fn repo_root<R: Runner>(runner: &R, ctx: &Context) -> Result<String> {
    if let Some(wt) = &ctx.worktree {
        return Ok(wt.repo_root.clone());
    }
    let not_git = || anyhow!("the focused workspace is not a git checkout");
    let cwd = ctx.cwd().ok_or_else(not_git)?;
    let out = runner.run(
        &[
            "git",
            "-C",
            cwd,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ],
        None,
        None,
    )?;
    if !out.success() {
        return Err(not_git());
    }
    let common = Path::new(out.stdout.trim());
    match (common.file_name(), common.parent()) {
        (Some(name), Some(root)) if name == ".git" => Ok(root.to_string_lossy().into_owned()),
        _ => Err(not_git()),
    }
}

/// Focused worktree on `branch` (an existing local branch is checked out) with
/// `mastracode` started in its root pane. No prompt is sent.
/// Waits briefly for the `worktree.created` hook's copy (never for install/build).
pub fn create_worktree<R: Runner>(herdr: &Herdr<R>, repo_root: &str, branch: &str) -> Result<()> {
    let since = std::time::SystemTime::now();
    let wt = herdr.worktree_create(repo_root, branch, true)?;
    if let Some(notice) = setup::after_create(&wt.checkout_path, branch, since) {
        let _ = herdr.notify("Werkbank setup", &notice, Sound::None);
    }
    herdr.pane_run(&wt.root_pane_id, "mastracode")
}

/// Focused worktree of PR `number` of the repo at `repo_root`, made by
/// `gh pr checkout`, with `mastracode` started in it; an open one is focused.
/// `url_repo` is the repo a pasted URL names: it must be this one.
pub fn create_pr_worktree<R: Runner>(
    herdr: &Herdr<R>,
    repo_root: &str,
    number: u64,
    url_repo: Option<&str>,
    dirs: (&Path, pr::Setup),
) -> Result<()> {
    let repo = issue::origin_repo(&herdr.runner, repo_root)
        .ok_or_else(|| anyhow!("{repo_root} has no GitHub origin"))?;
    if let Some(u) = url_repo.filter(|u| !u.eq_ignore_ascii_case(&repo)) {
        bail!("that PR is in {u}; this repo is {repo}");
    }
    let no_guard = || Ok(());
    let head = pr::view(&herdr.runner, &repo, number)?;
    let checkouts = prs::checkouts(herdr, &no_guard)?;
    match pr::checkout(herdr, &checkouts, repo_root, &repo, &head, dirs, &no_guard)? {
        pr::PrCheckout::Open(c) => herdr.workspace_focus(&c.workspace_id),
        pr::PrCheckout::Made { pane, setup, .. } => {
            if let Some(notice) = setup {
                let _ = herdr.notify(
                    "Werkbank setup",
                    &format!("#{number}: {notice}"),
                    Sound::None,
                );
            }
            herdr.pane_run(&pane, "mastracode")
        }
    }
}

struct Prompt {
    repo: Result<String, String>,
    input: String,
    error: Option<String>,
}

/// What a key press asks the popup to do.
#[derive(Debug, PartialEq)]
enum Step {
    Stay,
    Exit,
    Create(pr::Target),
}

impl Prompt {
    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> Step {
        if self.repo.is_err() {
            return Step::Exit;
        }
        match code {
            KeyCode::Esc => Step::Exit,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => Step::Exit,
            KeyCode::Enter => match pr::target(&self.input) {
                Ok(target) => Step::Create(target),
                Err(e) => {
                    self.error = Some(e.to_string());
                    Step::Stay
                }
            },
            KeyCode::Backspace => {
                self.input.pop();
                Step::Stay
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.error = None;
                Step::Stay
            }
            _ => Step::Stay,
        }
    }

    fn render(&self, frame: &mut Frame) {
        let block = Block::bordered().title(" New worktree + Mastra Code ");
        let area = block.inner(frame.area());
        frame.render_widget(block, frame.area());
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(area);
        match &self.repo {
            Err(e) => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled(e.as_str(), Style::new().fg(Color::Red)),
                        Line::raw("press any key to close"),
                    ])
                    .wrap(Wrap { trim: true }),
                    area,
                );
            }
            Ok(repo) => {
                frame.render_widget(Line::raw(format!("repo: {repo}")), rows[0]);
                frame.render_widget(Line::raw(format!("branch or PR: {}_", self.input)), rows[1]);
                let help = match &self.error {
                    Some(e) => Line::styled(e.as_str(), Style::new().fg(Color::Red)),
                    None => Line::raw("⏎ create worktree + start Mastra Code · Esc cancel"),
                };
                frame.render_widget(Paragraph::new(help).wrap(Wrap { trim: true }), rows[2]);
            }
        }
    }
}

/// The `worktree-prompt` popup: one input line; exits (closing the popup) once
/// the worktree is created, on Esc, or after showing why there is no repo.
pub fn run_prompt<R: Runner>(herdr: &Herdr<R>) -> Result<()> {
    let repo = Context::from_env()
        .and_then(|ctx| repo_root(&herdr.runner, &ctx))
        .map_err(|e| e.to_string());
    let mut prompt = Prompt {
        repo,
        input: String::new(),
        error: None,
    };
    let mut terminal = ratatui::init();
    let result = loop {
        if let Err(e) = terminal.draw(|f| prompt.render(f)) {
            break Err(e.into());
        }
        match event::poll(Duration::from_millis(250)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(e) => break Err(e.into()),
        }
        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match prompt.key(key.code, key.modifiers) {
            Step::Stay => {}
            Step::Exit => break Ok(()),
            Step::Create(target) => {
                let repo = prompt.repo.clone().unwrap_or_default();
                prompt.error = Some(format!("creating {}…", prompt.input.trim()));
                let _ = terminal.draw(|f| prompt.render(f));
                let made = match &target {
                    pr::Target::Branch(branch) => create_worktree(herdr, &repo, branch),
                    pr::Target::Pr { number, repo: url } => create_pr_worktree(
                        herdr,
                        &repo,
                        *number,
                        url.as_deref(),
                        (&pr::worktrees_dir(), setup::after_open),
                    ),
                };
                match made {
                    Ok(()) => break Ok(()),
                    Err(e) => prompt.error = Some(format!("{e:#}")),
                }
            }
        }
    };
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::FakeRunner;

    const ACTION: &str = include_str!("../../../tests/fixtures/plugin-context-action.json");
    const ACTION_NO_WT: &str =
        include_str!("../../../tests/fixtures/plugin-context-action-no-worktree.json");
    const POPUP: &str = include_str!("../../../tests/fixtures/plugin-context-popup.json");

    #[test]
    fn parses_recorded_contexts() {
        let a = Context::parse(ACTION).unwrap();
        assert_eq!(a.workspace_id, "w1");
        assert!(a.cwd().unwrap().ends_with("-repo"));
        let wt = a.worktree.unwrap();
        assert!(!wt.is_linked_worktree);
        assert!(wt.repo_root.ends_with("-repo"));
        let plain = Context::parse(ACTION_NO_WT).unwrap();
        assert!(plain.worktree.is_none());
        assert_eq!(plain.focused_pane_cwd, plain.workspace_cwd);
        let popup = Context::parse(POPUP).unwrap();
        assert!(popup.cwd().is_some());
    }

    #[test]
    fn cwd_prefers_focused_pane_then_workspace() {
        let ctx = Context {
            focused_pane_cwd: Some("/p".into()),
            workspace_cwd: Some("/w".into()),
            ..Context::default()
        };
        assert_eq!(ctx.cwd(), Some("/p"));
        let ctx = Context {
            workspace_cwd: Some("/w".into()),
            ..Context::default()
        };
        assert_eq!(ctx.cwd(), Some("/w"));
        assert_eq!(Context::default().cwd(), None);
    }

    #[test]
    fn launch_tab_creates_mc_tab_and_starts_mastracode_without_prompt() {
        let f = FakeRunner::new();
        f.ok(
            &[
                "herdr", "tab", "create", "--workspace", "w1", "--cwd", "/repo", "--label", "mc",
                "--focus",
            ],
            None,
            r#"{"id":"x","result":{"type":"tab_created","root_pane":{"pane_id":"w1:p7"},"tab":{"tab_id":"w1:t4"}}}"#,
        )
        .ok(&["herdr", "pane", "run", "w1:p7", "mastracode"], None, "");
        let herdr = Herdr::with_bin(f, "herdr");
        let ctx = Context {
            workspace_id: "w9".into(),
            focused_pane_cwd: Some("/repo".into()),
            ..Context::default()
        };
        assert_eq!(launch_tab(&herdr, &ctx, "w1").unwrap(), "w1:p7");
        assert_eq!(
            herdr.runner.argvs(),
            vec![
                "herdr tab create --workspace w1 --cwd /repo --label mc --focus",
                "herdr pane run w1:p7 mastracode",
            ]
        );
    }

    #[test]
    fn launch_tab_falls_back_to_context_workspace_and_needs_a_cwd() {
        let f = FakeRunner::new();
        f.otherwise(
            r#"{"id":"x","result":{"root_pane":{"pane_id":"w9:p1"},"tab":{"tab_id":"w9:t1"}}}"#,
        );
        let herdr = Herdr::with_bin(f, "herdr");
        let ctx = Context {
            workspace_id: "w9".into(),
            workspace_cwd: Some("/w".into()),
            ..Context::default()
        };
        launch_tab(&herdr, &ctx, "").unwrap();
        assert!(herdr.runner.argvs()[0].contains("--workspace w9 --cwd /w"));
        let none = Context {
            workspace_id: "w9".into(),
            ..Context::default()
        };
        assert!(launch_tab(&herdr, &none, "").is_err());
        assert_eq!(herdr.runner.argvs().len(), 2, "no tab without a cwd");
    }

    #[test]
    fn launch_worktree_opens_the_popup() {
        let f = FakeRunner::new();
        f.otherwise(r#"{"id":"x","result":{"type":"plugin_pane_opened"}}"#);
        let herdr = Herdr::with_bin(f, "herdr");
        launch_worktree(&herdr).unwrap();
        assert_eq!(
            herdr.runner.argvs(),
            vec!["herdr plugin pane open --plugin werkbank --entrypoint worktree-prompt"]
        );
    }

    #[test]
    fn repo_root_uses_herdr_metadata_then_git() {
        let f = FakeRunner::new();
        let ctx = Context::parse(ACTION).unwrap();
        assert!(repo_root(&f, &ctx).unwrap().ends_with("-repo"));
        assert!(f.argvs().is_empty(), "metadata needs no git call");

        let git = |cwd: &'static str| {
            [
                "git",
                "-C",
                cwd,
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            ]
        };
        let f = FakeRunner::new();
        f.ok(&git("/repo/sub"), None, "/repo/.git\n")
            .ok(&git("/wt/linked"), None, "/main/.git\n")
            .respond(
                &git("/tmp"),
                None,
                crate::core::runner::Output {
                    status: 128,
                    stdout: String::new(),
                    stderr: "fatal: not a git repository".into(),
                },
            );
        let at = |cwd: &str| Context {
            focused_pane_cwd: Some(cwd.into()),
            ..Context::default()
        };
        assert_eq!(repo_root(&f, &at("/repo/sub")).unwrap(), "/repo");
        assert_eq!(repo_root(&f, &at("/wt/linked")).unwrap(), "/main");
        let err = repo_root(&f, &at("/tmp")).unwrap_err().to_string();
        assert!(err.contains("not a git checkout"), "{err}");
        assert!(repo_root(&f, &Context::default()).is_err());
    }

    #[test]
    fn create_worktree_starts_mastracode_in_its_root_pane_and_never_prompts() {
        let f = FakeRunner::new();
        f.ok(
            &[
                "herdr", "worktree", "create", "--cwd", "/repo", "--branch", "feat/x", "--focus",
            ],
            None,
            include_str!("../../../tests/fixtures/worktree-create.json"),
        )
        .otherwise("");
        let herdr = Herdr::with_bin(f, "herdr");
        create_worktree(&herdr, "/repo", "feat/x").unwrap();
        let calls = herdr.runner.argvs();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].starts_with("herdr pane run ") && calls[1].ends_with(" mastracode"));
        assert!(calls.iter().all(|c| !c.contains("agent prompt")));
    }

    fn pr_runner(origin: &str) -> FakeRunner {
        let f = FakeRunner::new();
        f.ok(
            &["git", "-C", "/c/main", "remote", "get-url", "origin"],
            None,
            origin,
        );
        f
    }

    #[test]
    fn a_pr_url_from_another_repo_is_refused_before_any_lookup() {
        let herdr = Herdr::with_bin(pr_runner("git@github.com:o/r.git\n"), "herdr");
        let dirs = (Path::new("/wt"), (|_| None) as pr::Setup);
        let err = create_pr_worktree(&herdr, "/c/main", 7, Some("x/y"), dirs)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "that PR is in x/y; this repo is o/r");
        assert_eq!(herdr.runner.argvs().len(), 1, "only the origin read");
    }

    #[test]
    fn a_pr_number_makes_its_worktree_and_starts_mastracode_there() {
        let f = pr_runner("https://github.com/O/R.git\n");
        let view = pr::view_argv("O/R", 7);
        let view: Vec<&str> = view.iter().map(String::as_str).collect();
        f.ok(
            &view,
            None,
            r#"{"headRefName":"feat/7","isCrossRepository":true,"number":7}"#,
        )
        .ok(
            &["herdr", "workspace", "list"],
            None,
            r#"{"result":{"workspaces":[]}}"#,
        );
        let gh = pr::checkout_argv("O/R", 7, "/wt/main/feat-7");
        let gh: Vec<&str> = gh.iter().map(String::as_str).collect();
        f.ok(&gh, Some("/c/main"), "")
            .ok(
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
            )
            .otherwise(r#"{"result":{}}"#);
        let herdr = Herdr::with_bin(f, "herdr");
        let dirs = (
            Path::new("/wt"),
            (|_| panic!("forks skip setup")) as pr::Setup,
        );
        create_pr_worktree(&herdr, "/c/main", 7, Some("o/r"), dirs).unwrap();
        let calls = herdr.runner.argvs();
        assert!(
            calls.iter().any(|c| c.starts_with(
                "herdr notification show Werkbank setup --body #7: fork, setup skipped"
            )),
            "{calls:?}"
        );
        assert_eq!(
            calls.last().unwrap(),
            "herdr pane run w3:p1 mastracode",
            "{calls:?}"
        );
    }

    #[test]
    fn prompt_keys_refuse_empty_then_create_trimmed_branch() {
        let mut p = Prompt {
            repo: Ok("/repo".into()),
            input: String::new(),
            error: None,
        };
        assert_eq!(p.key(KeyCode::Enter, KeyModifiers::NONE), Step::Stay);
        assert!(p.error.is_some());
        for c in " ab".chars() {
            p.key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        p.key(KeyCode::Backspace, KeyModifiers::NONE);
        p.key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            p.key(KeyCode::Enter, KeyModifiers::NONE),
            Step::Create(pr::Target::Branch("ax".into()))
        );
        p.input = "#12".into();
        assert_eq!(
            p.key(KeyCode::Enter, KeyModifiers::NONE),
            Step::Create(pr::Target::Pr {
                number: 12,
                repo: None
            })
        );
        assert_eq!(p.key(KeyCode::Esc, KeyModifiers::NONE), Step::Exit);
    }

    #[test]
    fn prompt_without_repo_shows_error_and_any_key_exits() {
        let mut p = Prompt {
            repo: Err("the focused workspace is not a git checkout".into()),
            input: String::new(),
            error: None,
        };
        let backend = ratatui::backend::TestBackend::new(70, 8);
        let mut t = ratatui::Terminal::new(backend).unwrap();
        t.draw(|f| p.render(f)).unwrap();
        let text: String = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("not a git checkout"), "{text}");
        assert_eq!(p.key(KeyCode::Char('a'), KeyModifiers::NONE), Step::Exit);
    }
}
