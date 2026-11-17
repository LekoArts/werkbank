use std::collections::BTreeMap;
use std::fmt;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::runner::{Output, Runner};

pub const SOURCE_ID: &str = "werkbank";
pub const PLUGIN_ID: &str = "werkbank";

/// A structured `{"error":{"code","message"}}` reply from the herdr CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "herdr {}: {}", self.code, self.message)
    }
}

impl std::error::Error for HerdrError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WorktreeInfo {
    pub checkout_path: String,
    pub repo_root: String,
    #[serde(default)]
    pub is_linked_worktree: bool,
    #[serde(default)]
    pub repo_name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub worktree: Option<WorktreeInfo>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorktree {
    pub workspace_id: String,
    pub checkout_path: String,
    pub root_pane_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadSource {
    /// The current screen only.
    Visible,
    /// Recent scrollback, including lines that scrolled off.
    Recent,
}

impl ReadSource {
    fn as_arg(self) -> &'static str {
        match self {
            ReadSource::Visible => "visible",
            ReadSource::Recent => "recent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    None,
    Request,
}

impl Sound {
    fn as_arg(self) -> &'static str {
        match self {
            Sound::None => "none",
            Sound::Request => "request",
        }
    }
}

/// Typed wrappers over the `herdr` CLI (at `$HERDR_BIN_PATH`) and its JSON replies.
pub struct Herdr<R: Runner> {
    pub runner: R,
    bin: String,
    /// When set, every call targets this session explicitly (`herdr --session <s> …`).
    session: Option<String>,
}

impl<R: Runner> Herdr<R> {
    pub fn new(runner: R) -> Self {
        let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
        Self::with_bin(runner, bin)
    }

    pub fn with_bin(runner: R, bin: impl Into<String>) -> Self {
        Self {
            runner,
            bin: bin.into(),
            session: None,
        }
    }

    pub fn for_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    fn argv<'a>(&'a self, args: &[&'a str]) -> Vec<&'a str> {
        let mut argv = vec![self.bin.as_str()];
        if let Some(s) = &self.session {
            argv.extend_from_slice(&["--session", s.as_str()]);
        }
        argv.extend_from_slice(args);
        argv
    }

    fn raw(&self, args: &[&str]) -> Result<Output> {
        let out = self.runner.run(&self.argv(args), None, None)?;
        if let Some(err) = parse_error(&out.stdout).or_else(|| parse_error(&out.stderr)) {
            return Err(err.into());
        }
        if !out.success() {
            return Err(anyhow!(
                "herdr {} exited {}: {}",
                args.join(" "),
                out.status,
                out.stderr.trim()
            ));
        }
        Ok(out)
    }

    fn json(&self, args: &[&str]) -> Result<Value> {
        let out = self.raw(args)?;
        let value: Value = serde_json::from_str(&out.stdout)
            .with_context(|| format!("herdr {}: invalid JSON", args.join(" ")))?;
        value
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("herdr {}: reply has no result", args.join(" ")))
    }

    /// This session's API socket. Refuses without an explicit session.
    pub fn socket_path(&self) -> Result<std::path::PathBuf> {
        let Some(session) = &self.session else {
            bail!("no Herdr session to find a socket for");
        };
        let out = self.raw(&["session", "list", "--json"])?;
        super::socket_for_session(&out.stdout, session)
            .map(Into::into)
            .ok_or_else(|| anyhow!("herdr session {session} is not running"))
    }

    pub fn agent_list(&self) -> Result<Vec<AgentInfo>> {
        field(self.json(&["agent", "list"])?, "agents")
    }

    pub fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>> {
        field(self.json(&["workspace", "list"])?, "workspaces")
    }

    pub fn pane_list(&self) -> Result<Vec<PaneInfo>> {
        field(self.json(&["pane", "list"])?, "panes")
    }

    /// The argv of each foreground process in the pane.
    pub fn pane_process_info(&self, pane: &str) -> Result<Vec<Vec<String>>> {
        let v = self.json(&["pane", "process-info", "--pane", pane])?;
        let procs = v
            .pointer("/process_info/foreground_processes")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("herdr pane process-info: no foreground_processes"))?;
        Ok(procs
            .iter()
            .map(|p| {
                p.get("argv")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect())
    }

    pub fn pane_read(&self, pane: &str, source: ReadSource, lines: u32) -> Result<String> {
        let lines = lines.to_string();
        let out = self.raw(&[
            "pane",
            "read",
            pane,
            "--source",
            source.as_arg(),
            "--lines",
            &lines,
        ])?;
        Ok(out.stdout)
    }

    pub fn set_tokens(&self, pane: &str, tokens: &[(&str, &str)], ttl_ms: u64) -> Result<()> {
        let pairs: Vec<String> = tokens.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let ttl = ttl_ms.to_string();
        let mut args = vec!["pane", "report-metadata", pane, "--source", SOURCE_ID];
        for pair in &pairs {
            args.push("--token");
            args.push(pair);
        }
        args.push("--ttl-ms");
        args.push(&ttl);
        self.raw(&args).map(|_| ())
    }

    pub fn clear_tokens(&self, pane: &str, names: &[&str]) -> Result<()> {
        let mut args = vec!["pane", "report-metadata", pane, "--source", SOURCE_ID];
        for name in names {
            args.push("--clear-token");
            args.push(name);
        }
        self.raw(&args).map(|_| ())
    }

    /// Returns the raw result object so callers can log whether it was shown.
    pub fn notify(&self, title: &str, body: &str, sound: Sound) -> Result<Value> {
        self.json(&[
            "notification",
            "show",
            title,
            "--body",
            body,
            "--sound",
            sound.as_arg(),
        ])
    }

    pub fn focus_agent(&self, pane: &str) -> Result<()> {
        self.json(&["agent", "focus", pane]).map(|_| ())
    }

    /// `agent focus` rejects panes without an agent (`agent_not_found`), so
    /// non-agent panes such as the Inbox are reached through their tab.
    pub fn tab_focus(&self, tab: &str) -> Result<()> {
        self.json(&["tab", "focus", tab]).map(|_| ())
    }

    pub fn tab_rename(&self, tab: &str, label: &str) -> Result<()> {
        self.json(&["tab", "rename", tab, label]).map(|_| ())
    }

    /// Creates a tab and returns its root pane id.
    pub fn tab_create(
        &self,
        workspace: &str,
        cwd: &str,
        label: &str,
        focus: bool,
    ) -> Result<String> {
        self.tab_create_ids(workspace, cwd, label, focus)
            .map(|(_, pane)| pane)
    }

    /// Creates a tab and returns `(tab_id, root pane_id)`.
    pub fn tab_create_ids(
        &self,
        workspace: &str,
        cwd: &str,
        label: &str,
        focus: bool,
    ) -> Result<(String, String)> {
        let focus = if focus { "--focus" } else { "--no-focus" };
        let result = self.json(&[
            "tab",
            "create",
            "--workspace",
            workspace,
            "--cwd",
            cwd,
            "--label",
            label,
            focus,
        ])?;
        Ok((
            str_at(&result, &["tab", "tab_id"])?,
            str_at(&result, &["root_pane", "pane_id"])?,
        ))
    }

    pub fn tab_close(&self, tab: &str) -> Result<()> {
        self.raw(&["tab", "close", tab]).map(|_| ())
    }

    pub fn pane_run(&self, pane: &str, cmd: &str) -> Result<()> {
        self.raw(&["pane", "run", pane, cmd]).map(|_| ())
    }

    pub fn worktree_create(&self, cwd: &str, branch: &str, focus: bool) -> Result<CreatedWorktree> {
        check_branch(branch)?;
        let focus_flag = if focus { "--focus" } else { "--no-focus" };
        let result = self.json(&[
            "worktree", "create", "--cwd", cwd, "--branch", branch, focus_flag,
        ])?;
        let workspace_id = str_at(&result, &["workspace", "workspace_id"])?;
        let checkout_path = str_at(&result, &["worktree", "path"])
            .or_else(|_| str_at(&result, &["workspace", "worktree", "checkout_path"]))?;
        let root_pane_id = match str_at(&result, &["root_pane", "pane_id"]) {
            Ok(id) => id,
            Err(_) => self
                .pane_list()?
                .into_iter()
                .find(|p| p.workspace_id == workspace_id)
                .map(|p| p.pane_id)
                .ok_or_else(|| anyhow!("no pane in new workspace {workspace_id}"))?,
        };
        Ok(CreatedWorktree {
            workspace_id,
            checkout_path,
            root_pane_id,
        })
    }

    /// `worktree_create` from `base` with a workspace `label`.
    pub fn worktree_create_at(
        &self,
        cwd: &str,
        branch: &str,
        base: &str,
        label: &str,
        focus: bool,
    ) -> Result<CreatedWorktree> {
        check_branch(branch)?;
        check_branch(base)?;
        let focus_flag = if focus { "--focus" } else { "--no-focus" };
        let result = self.json(&[
            "worktree", "create", "--cwd", cwd, "--branch", branch, "--base", base, "--label",
            label, focus_flag,
        ])?;
        Ok(CreatedWorktree {
            workspace_id: str_at(&result, &["workspace", "workspace_id"])?,
            checkout_path: str_at(&result, &["worktree", "path"])
                .or_else(|_| str_at(&result, &["workspace", "worktree", "checkout_path"]))?,
            root_pane_id: str_at(&result, &["root_pane", "pane_id"]).unwrap_or_default(),
        })
    }

    pub fn workspace_focus(&self, workspace: &str) -> Result<()> {
        self.raw(&["workspace", "focus", workspace]).map(|_| ())
    }

    pub fn worktree_remove(&self, workspace: &str) -> Result<Value> {
        self.json(&["worktree", "remove", "--workspace", workspace])
    }

    /// Submits a prompt and waits until the agent starts working. Errors are
    /// rebuilt like `agent_send`'s, since the prompt carries issue text.
    pub fn agent_prompt(&self, pane: &str, text: &str, timeout_ms: u64) -> Result<Value> {
        let timeout = timeout_ms.to_string();
        let args = [
            "agent",
            "prompt",
            pane,
            text,
            "--wait",
            "--until",
            "working",
            "--timeout",
            &timeout,
        ];
        let out = match self.runner.run(&self.argv(&args), None, None) {
            Ok(out) => out,
            Err(err) if err.to_string().contains(" timed out after ") => {
                return Err(anyhow!("prompt to {pane} timed out"))
            }
            Err(_) => return Err(anyhow!("prompt to {pane} failed")),
        };
        if let Some(err) = parse_error(&out.stdout).or_else(|| parse_error(&out.stderr)) {
            return Err(anyhow!("prompt to {pane} refused: {}", err.code));
        }
        if !out.success() {
            return Err(anyhow!("prompt to {pane} refused: exit {}", out.status));
        }
        serde_json::from_str::<Value>(&out.stdout)
            .ok()
            .and_then(|v| v.get("result").cloned())
            .ok_or_else(|| anyhow!("prompt to {pane}: unreadable herdr reply"))
    }

    /// Submits a reply without waiting; Herdr itself refuses a blocked agent.
    /// The text goes in the positional slot with no `--`: Herdr takes it
    /// verbatim even when it starts with `-`, and does not accept `--` there
    /// (probed: `-- "-x hi"` fails with "unknown option").
    /// Every error is rebuilt from scratch so the text never reaches a log or
    /// the footer: the runner's own errors quote the whole argv.
    pub fn agent_send(&self, pane: &str, text: &str) -> Result<()> {
        let out = match self
            .runner
            .run(&self.argv(&["agent", "prompt", pane, text]), None, None)
        {
            Ok(out) => out,
            Err(err) if err.to_string().contains(" timed out after ") => {
                return Err(anyhow!("reply to {pane} timed out"))
            }
            Err(_) => return Err(anyhow!("reply to {pane} failed")),
        };
        if let Some(err) = parse_error(&out.stdout).or_else(|| parse_error(&out.stderr)) {
            return Err(anyhow!("reply to {pane} refused: {}", err.code));
        }
        if !out.success() {
            return Err(anyhow!("reply to {pane} refused: exit {}", out.status));
        }
        Ok(())
    }

    pub fn plugin_pane_open(&self, entrypoint: &str, env: &[(&str, &str)]) -> Result<Value> {
        let pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut args = vec![
            "plugin",
            "pane",
            "open",
            "--plugin",
            PLUGIN_ID,
            "--entrypoint",
            entrypoint,
        ];
        for pair in &pairs {
            args.push("--env");
            args.push(pair);
        }
        self.json(&args)
    }
}

/// Git's ref-name rules (`git check-ref-format --branch`), so a branch name
/// from Linear, GitHub or the keyboard can't become an option or a path that
/// climbs out of the worktree dir.
pub fn check_branch(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@"
        || name.split('/').any(|c| c.starts_with('.'))
        || name
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c));
    if bad {
        bail!("not a usable branch name: {name:?}");
    }
    Ok(())
}

fn parse_error(text: &str) -> Option<HerdrError> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    let err = value.get("error")?;
    Some(HerdrError {
        code: err.get("code")?.as_str()?.to_string(),
        message: err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn field<T: DeserializeOwned>(mut result: Value, name: &str) -> Result<T> {
    let value = result
        .get_mut(name)
        .map(Value::take)
        .ok_or_else(|| anyhow!("herdr reply has no {name}"))?;
    Ok(serde_json::from_value(value)?)
}

fn str_at(value: &Value, path: &[&str]) -> Result<String> {
    let mut cur = value;
    for key in path {
        cur = cur
            .get(key)
            .ok_or_else(|| anyhow!("herdr reply has no {}", path.join(".")))?;
    }
    cur.as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("herdr reply {} is not a string", path.join(".")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::FakeRunner;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn herdr() -> Herdr<FakeRunner> {
        Herdr::with_bin(FakeRunner::new(), "herdr")
    }

    const OK: &str = r#"{"id":"x","result":{"type":"ok"}}"#;

    #[test]
    fn parses_agent_list_fixture() {
        let h = herdr();
        h.runner.ok(
            &["herdr", "agent", "list"],
            None,
            &fixture("agent-list.json"),
        );
        let agents = h.agent_list().unwrap();
        assert!(!agents.is_empty());
        let mc = agents
            .iter()
            .find(|a| a.agent.as_deref() == Some("mastracode"))
            .expect("a mastracode agent");
        assert!(mc.pane_id.contains(":p"));
        assert!(!mc.agent_status.is_empty());
        assert!(mc.foreground_cwd.is_some());
        assert!(mc.state_change_seq > 0);
        assert!(
            agents.iter().any(|a| a.tokens.contains_key("pr")),
            "fixture records a pane with a werkbank token"
        );
    }

    #[test]
    fn parses_pane_process_info_fixture() {
        let h = herdr();
        h.runner.ok(
            &["herdr", "pane", "process-info", "--pane", "w1:p1"],
            None,
            &fixture("mastracode/process-info-resumed.json"),
        );
        let argvs = h.pane_process_info("w1:p1").unwrap();
        assert_eq!(argvs.len(), 2);
        assert_eq!(argvs[0], ["caffeinate", "-i", "-m"]);
        assert_eq!(
            argvs[1][2..],
            ["--thread", "11111111-2222-4333-8444-555555555555"]
        );
    }

    #[test]
    fn parses_workspace_list_fixture() {
        let h = herdr();
        h.runner.ok(
            &["herdr", "workspace", "list"],
            None,
            &fixture("workspace-list.json"),
        );
        let ws = h.workspace_list().unwrap();
        let main = ws
            .iter()
            .find(|w| w.worktree.as_ref().is_some_and(|t| !t.is_linked_worktree))
            .expect("main checkout");
        let linked = ws
            .iter()
            .find(|w| w.worktree.as_ref().is_some_and(|t| t.is_linked_worktree))
            .expect("linked worktree");
        let (m, l) = (
            main.worktree.as_ref().unwrap(),
            linked.worktree.as_ref().unwrap(),
        );
        assert_eq!(m.repo_root, l.repo_root);
        assert_ne!(l.checkout_path, l.repo_root);
        assert!(!l.repo_name.is_empty());
    }

    #[test]
    fn parses_plugin_pane_fixtures() {
        let h = herdr();
        h.runner.ok(
            &[
                "herdr",
                "plugin",
                "pane",
                "open",
                "--plugin",
                "werkbank",
                "--entrypoint",
                "inbox",
            ],
            None,
            &fixture("plugin-pane-open.json"),
        );
        let opened = h.plugin_pane_open("inbox", &[]).unwrap();
        assert_eq!(opened["type"], "plugin_pane_opened");
        assert!(opened["plugin_pane"]["pane"]["pane_id"]
            .as_str()
            .unwrap()
            .contains(":p"));
        let ctx: Value = serde_json::from_str(&fixture("plugin-context-pane.json")).unwrap();
        for key in [
            "workspace_id",
            "tab_id",
            "focused_pane_cwd",
            "workspace_cwd",
        ] {
            assert!(ctx[key].is_string(), "context has {key}");
        }
    }

    #[test]
    fn parses_worktree_create_fixture() {
        let h = herdr();
        let argv = [
            "herdr",
            "worktree",
            "create",
            "--cwd",
            "/repo",
            "--branch",
            "b",
            "--no-focus",
        ];
        h.runner.ok(&argv, None, &fixture("worktree-create.json"));
        let created = h.worktree_create("/repo", "b", false).unwrap();
        assert!(created.workspace_id.starts_with('w'));
        assert!(created.checkout_path.contains("/.herdr/worktrees/"));
        assert!(created.root_pane_id.starts_with(&created.workspace_id));
    }

    #[test]
    fn worktree_create_falls_back_to_pane_list_for_root_pane() {
        let h = herdr();
        let argv = [
            "herdr", "worktree", "create", "--cwd", "/r", "--branch", "b", "--focus",
        ];
        h.runner.ok(
            &argv,
            None,
            r#"{"result":{"workspace":{"workspace_id":"w9"},"worktree":{"path":"/wt"}}}"#,
        );
        h.runner.ok(
            &["herdr", "pane", "list"],
            None,
            r#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1"},{"pane_id":"w9:p3","workspace_id":"w9","tab_id":"w9:t1"}]}}"#,
        );
        let created = h.worktree_create("/r", "b", true).unwrap();
        assert_eq!(created.root_pane_id, "w9:p3");
        assert_eq!(created.checkout_path, "/wt");
    }

    #[test]
    fn error_reply_maps_to_herdr_error_with_code() {
        let h = herdr();
        h.runner.respond(
            &["herdr", "worktree", "remove", "--workspace", "w1"],
            None,
            Output {
                status: 1,
                stdout: fixture("error-not-linked.json"),
                stderr: String::new(),
            },
        );
        let err = h.worktree_remove("w1").unwrap_err();
        let herdr_err = err.downcast_ref::<HerdrError>().expect("HerdrError");
        assert_eq!(herdr_err.code, "not_linked_worktree");
    }

    #[test]
    fn error_on_stderr_is_also_recognised() {
        let h = herdr();
        h.runner.respond(
            &["herdr", "agent", "focus", "w1:p9"],
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: r#"{"error":{"code":"pane_not_found","message":"no pane"}}"#.into(),
            },
        );
        let err = h.focus_agent("w1:p9").unwrap_err();
        assert_eq!(
            err.downcast_ref::<HerdrError>().unwrap().code,
            "pane_not_found"
        );
    }

    #[test]
    fn set_tokens_argv() {
        let h = herdr();
        let argv = [
            "herdr",
            "pane",
            "report-metadata",
            "w1:p1",
            "--source",
            "werkbank",
            "--token",
            "pr=#12 ✗",
            "--token",
            "stalled=⏸ 6m",
            "--ttl-ms",
            "90000",
        ];
        // `pane report-metadata` prints nothing on success.
        h.runner.ok(&argv, None, "");
        h.set_tokens("w1:p1", &[("pr", "#12 ✗"), ("stalled", "⏸ 6m")], 90_000)
            .unwrap();
        assert_eq!(h.runner.argvs(), vec![argv.join(" ")]);
    }

    #[test]
    fn clear_tokens_argv() {
        let h = herdr();
        let argv = [
            "herdr",
            "pane",
            "report-metadata",
            "w1:p1",
            "--source",
            "werkbank",
            "--clear-token",
            "pr",
            "--clear-token",
            "stalled",
        ];
        h.runner.ok(&argv, None, "");
        h.clear_tokens("w1:p1", &["pr", "stalled"]).unwrap();
        assert_eq!(h.runner.argvs(), vec![argv.join(" ")]);
    }

    #[test]
    fn pane_read_passes_requested_source() {
        let h = herdr();
        let visible = [
            "herdr", "pane", "read", "w1:p1", "--source", "visible", "--lines", "60",
        ];
        let recent = [
            "herdr", "pane", "read", "w1:p1", "--source", "recent", "--lines", "60",
        ];
        h.runner.ok(&visible, None, &fixture("pane-read-plain.txt"));
        h.runner.ok(&recent, None, "scrollback");
        let screen = h.pane_read("w1:p1", ReadSource::Visible, 60).unwrap();
        assert!(screen.contains("mastracode"));
        assert_eq!(
            h.pane_read("w1:p1", ReadSource::Recent, 60).unwrap(),
            "scrollback"
        );
        assert_eq!(h.runner.argvs(), vec![visible.join(" "), recent.join(" ")]);
    }

    #[test]
    fn simple_wrapper_argvs() {
        let h = herdr();
        let cases: Vec<Vec<&str>> = vec![
            vec![
                "herdr",
                "notification",
                "show",
                "Stalled: a/b",
                "--body",
                "x",
                "--sound",
                "request",
            ],
            vec!["herdr", "agent", "focus", "w1:p1"],
            vec!["herdr", "tab", "focus", "w1:t2"],
            vec![
                "herdr",
                "agent",
                "prompt",
                "w1:p1",
                "hello",
                "--wait",
                "--until",
                "working",
                "--timeout",
                "15000",
            ],
            vec![
                "herdr",
                "plugin",
                "pane",
                "open",
                "--plugin",
                "werkbank",
                "--entrypoint",
                "inbox",
                "--env",
                "WERKBANK_STALL_SECS=20",
            ],
        ];
        for argv in &cases {
            h.runner.ok(argv, None, OK);
        }
        // `pane run` prints nothing on success.
        h.runner
            .ok(&["herdr", "pane", "run", "w1:p1", "mastracode"], None, "");
        h.runner.ok(
            &[
                "herdr",
                "tab",
                "create",
                "--workspace",
                "w1",
                "--cwd",
                "/r",
                "--label",
                "mc",
                "--focus",
            ],
            None,
            r#"{"result":{"root_pane":{"pane_id":"w1:p7"},"tab":{"tab_id":"w1:t7"}}}"#,
        );
        h.notify("Stalled: a/b", "x", Sound::Request).unwrap();
        h.focus_agent("w1:p1").unwrap();
        h.tab_focus("w1:t2").unwrap();
        h.pane_run("w1:p1", "mastracode").unwrap();
        h.agent_prompt("w1:p1", "hello", 15_000).unwrap();
        h.plugin_pane_open("inbox", &[("WERKBANK_STALL_SECS", "20")])
            .unwrap();
        assert_eq!(h.tab_create("w1", "/r", "mc", true).unwrap(), "w1:p7");
        let mut expected: Vec<String> = cases.iter().map(|a| a.join(" ")).collect();
        expected.insert(3, "herdr pane run w1:p1 mastracode".into());
        expected.push("herdr tab create --workspace w1 --cwd /r --label mc --focus".into());
        assert_eq!(h.runner.argvs(), expected);
    }

    #[test]
    fn agent_send_submits_without_waiting() {
        let h = herdr();
        h.runner.ok(
            &["herdr", "agent", "prompt", "w1:p1", "hi there"],
            None,
            r#"{"result":{}}"#,
        );
        h.agent_send("w1:p1", "hi there").unwrap();
        assert_eq!(h.runner.argvs(), ["herdr agent prompt w1:p1 hi there"]);
    }

    #[test]
    fn agent_send_passes_dash_leading_text_as_the_text_argument() {
        let h = herdr();
        h.runner.ok(
            &["herdr", "agent", "prompt", "w1:p1", "--wait"],
            None,
            r#"{"result":{}}"#,
        );
        h.agent_send("w1:p1", "--wait").unwrap();
        assert_eq!(
            h.runner.calls()[0].argv,
            ["herdr", "agent", "prompt", "w1:p1", "--wait"]
        );
    }

    #[test]
    fn agent_send_errors_never_quote_the_text() {
        let secret = "my secret reply";
        let argv = ["herdr", "agent", "prompt", "w1:p1", secret];
        let h = herdr();
        h.runner.fail(&argv, None);
        let err = format!("{:#}", h.agent_send("w1:p1", secret).unwrap_err());
        assert_eq!(err, "reply to w1:p1 timed out");

        let h = herdr();
        h.runner.respond(
            &argv,
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: format!(r#"{{"error":{{"code":"agent_blocked","message":"{secret}"}}}}"#),
            },
        );
        let err = format!("{:#}", h.agent_send("w1:p1", secret).unwrap_err());
        assert_eq!(err, "reply to w1:p1 refused: agent_blocked");

        let h = herdr();
        h.runner.respond(
            &argv,
            None,
            Output {
                status: 2,
                stdout: String::new(),
                stderr: format!("bad: {secret}"),
            },
        );
        let err = format!("{:#}", h.agent_send("w1:p1", secret).unwrap_err());
        assert_eq!(err, "reply to w1:p1 refused: exit 2");
    }

    #[test]
    fn tab_create_without_focus_passes_no_focus() {
        let h = herdr();
        h.runner
            .otherwise(r#"{"result":{"root_pane":{"pane_id":"w1:p8"},"tab":{"tab_id":"w1:t8"}}}"#);
        assert_eq!(h.tab_create("w1", "/r", "setup", false).unwrap(), "w1:p8");
        assert_eq!(
            h.runner.argvs(),
            ["herdr tab create --workspace w1 --cwd /r --label setup --no-focus"]
        );
    }

    #[test]
    fn a_session_herdr_targets_that_session_on_every_call() {
        let h = Herdr::with_bin(FakeRunner::new(), "herdr").for_session("s1");
        h.runner.otherwise(
            r#"{"id":"x","result":{"agents":[],"workspaces":[],"panes":[],"shown":true}}"#,
        );
        h.agent_list().unwrap();
        h.workspace_list().unwrap();
        h.pane_read("w1:p1", ReadSource::Visible, 60).unwrap();
        h.set_tokens("w1:p1", &[("pr", "#1")], 1).unwrap();
        h.clear_tokens("w1:p1", &["pr"]).unwrap();
        h.notify("t", "b", Sound::None).unwrap();
        for argv in h.runner.argvs() {
            assert!(argv.starts_with("herdr --session s1 "), "{argv}");
        }
        assert_eq!(h.runner.argvs().len(), 6);
    }

    #[test]
    fn the_socket_is_the_sessions_own_and_needs_a_session() {
        let h = Herdr::with_bin(FakeRunner::new(), "herdr").for_session("work");
        h.runner.ok(
            &["herdr", "--session", "work", "session", "list", "--json"],
            None,
            r#"{"sessions":[{"name":"default","running":true,"socket_path":"/h/herdr.sock"},
                {"name":"work","running":true,"socket_path":"/h/work.sock"}]}"#,
        );
        assert_eq!(
            h.socket_path().unwrap(),
            std::path::Path::new("/h/work.sock")
        );
        assert!(herdr().socket_path().is_err(), "never the default session");
    }

    #[test]
    fn nonzero_exit_without_error_json_is_an_error() {
        let h = herdr();
        h.runner.respond(
            &["herdr", "agent", "list"],
            None,
            Output {
                status: 2,
                stdout: String::new(),
                stderr: "boom".into(),
            },
        );
        assert!(h.agent_list().unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn agent_prompt_errors_never_carry_the_prompt() {
        let secret = "issue body with private details";
        let argv = [
            "herdr",
            "agent",
            "prompt",
            "w1:p1",
            secret,
            "--wait",
            "--until",
            "working",
            "--timeout",
            "5",
        ];
        let h = herdr();
        h.runner.fail(&argv, None);
        let err = h.agent_prompt("w1:p1", secret, 5).unwrap_err().to_string();
        assert!(!err.contains(secret), "{err}");
        let h = herdr();
        h.runner.respond(
            &argv,
            None,
            Output {
                status: 2,
                stdout: String::new(),
                stderr: secret.into(),
            },
        );
        let err = h.agent_prompt("w1:p1", secret, 5).unwrap_err().to_string();
        assert!(!err.contains(secret), "{err}");
        let h = herdr();
        h.runner.ok(&argv, None, "not json");
        let err = h.agent_prompt("w1:p1", secret, 5).unwrap_err().to_string();
        assert!(!err.contains(secret), "{err}");
    }

    #[test]
    fn worktree_branches_follow_git_ref_rules() {
        for ok in ["feat/x", "user/eng-12-fix-it", "origin/feat/x", "a.b"] {
            assert!(check_branch(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "--upload-pack=x",
            "../x",
            "a/../b",
            "/abs",
            "a/",
            "a//b",
            ".x",
            "a/.x",
            "a.lock",
            "a.",
            "a b",
            "a\tb",
            "a\u{1b}b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "a@{1}",
            "@",
        ] {
            assert!(check_branch(bad).is_err(), "{bad:?}");
        }
        let h = herdr();
        assert!(h.worktree_create("/r", "../../etc", true).is_err());
        assert!(h.runner.calls().is_empty(), "refused before herdr runs");
    }
}
