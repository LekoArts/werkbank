use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::core::herdr::Herdr;
use crate::core::runner::{RealRunner, Runner};
use crate::core::unix_ms;
use crate::features::cleanup::CleanupView;
use crate::features::daemon::{self, Paths};
use crate::features::factory::drafts::{self, Draft};
use crate::features::factory::prompt::{Factory, Message, Prompt, Role};
use crate::features::inbox::herdr_keys::herdr_keys;
use crate::features::inbox::issues;
use crate::features::inbox::model::{
    self, age_text, glyph_of, stage_of, title_of, AgentSnapshot, Row, Stage,
};
use crate::features::inbox::prs::{self, Item};
use crate::features::inbox::theme::theme;
use crate::features::inbox::watcher::{
    Actions, Command, LaunchView, Snapshot, WatchState, WatcherStatus,
};
use crate::integrations::github::{Ci, ListPr, PrInfo, PrState};
use crate::integrations::linear::{self, MyIssue, TeamState, WorkflowState};
use crate::integrations::slack::Slack;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Agents,
    Prs,
    Issues,
    Factory,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Agents, Tab::Prs, Tab::Issues, Tab::Factory];

    fn label(self) -> &'static str {
        match self {
            Tab::Agents => "Agents",
            Tab::Prs => "PRs",
            Tab::Issues => "Issues",
            Tab::Factory => "Factory",
        }
    }

    fn step(self, by: isize) -> Tab {
        let at = Tab::ALL.iter().position(|t| *t == self).unwrap_or(0) as isize;
        Tab::ALL[(at + by).rem_euclid(Tab::ALL.len() as isize) as usize]
    }
}

/// The merged-worktree cleanup confirm dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanupModal {
    pub view: CleanupView,
    pub input: String,
}

impl CleanupModal {
    /// Only with no refusals and `DELETE` typed exactly.
    pub fn can_confirm(&self) -> bool {
        self.view.refusals.is_empty() && self.input == "DELETE"
    }
}

/// The `n` dialog: paste an issue id, then pick the repo to launch it in.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchModal {
    Input(String),
    Pick { view: LaunchView, selected: usize },
}

/// The `tab` box: a reply to one agent, or why it can't take one.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyBox {
    pub pane: String,
    pub title: String,
    pub text: String,
    /// Why this agent can't take typed text; Enter then goes to its pane.
    pub refusal: Option<String>,
    pub stall_hash: Option<u64>,
    /// Sent to the worker, waiting for its answer.
    pub sending: bool,
}

pub const REPLY_MAX: usize = 4000;

/// The `f`/`s` confirm: the exact text, shown before it leaves.
#[derive(Debug, Clone, PartialEq)]
pub struct FactoryConfirm {
    pub message: Message,
    pub scroll: u16,
    /// Sent from the Issues marks, which clear once it posts.
    pub batch: bool,
    /// `Some`: a local agent on this PR gets the body; `None`: Slack.
    pub local: Option<PrRef>,
    /// The agent draft this is, deleted once it posts; `d` discards it.
    pub draft: Option<String>,
}

/// The PR a prompt is about, and where its branch lives.
#[derive(Debug, Clone, PartialEq)]
pub struct PrRef {
    pub repo: String,
    pub head_repo: Option<String>,
    pub head: String,
    pub number: u64,
    pub url: String,
}

/// The PRs tab's `f`/`s` picker: the PR prompts, then a typed message.
#[derive(Debug, Clone, PartialEq)]
pub struct PrPick {
    pub pr: PrRef,
    /// `s`: a local agent; else Factory over Slack.
    pub local: bool,
    pub selected: usize,
}

/// The Factory tab's text box: a URL for a prompt with `{ref}`, or, with no
/// prompt, a whole message typed as-is. With `about`, the message is about
/// that PR (and goes to a local agent when its flag is set).
#[derive(Debug, Clone, PartialEq)]
pub struct FactoryInput {
    pub prompt: Option<Prompt>,
    pub text: String,
    pub about: Option<(PrRef, bool)>,
}

impl FactoryInput {
    fn max(&self) -> usize {
        if self.prompt.is_some() {
            FACTORY_REF_MAX
        } else {
            REPLY_MAX
        }
    }

    /// Adds typed or pasted text up to the limit; a URL stays on one line.
    fn push(&mut self, text: &str) {
        let multiline = self.prompt.is_none();
        for c in text.replace("\r\n", "\n").chars() {
            if self.text.chars().count() >= self.max() {
                break;
            }
            match c {
                '\n' | '\r' if multiline => self.text.push('\n'),
                c if c.is_control() => {}
                c => self.text.push(c),
            }
        }
    }
}

/// A Factory tab row: agent drafts, the free-form message, then each
/// sendable prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FactoryRow<'a> {
    Draft(&'a Draft),
    Message,
    Prompt(&'a Prompt),
}

pub const FACTORY_REF_MAX: usize = 200;

/// The `c` picker: the issue's team states by position.
#[derive(Debug, Clone, PartialEq)]
pub struct StatePicker {
    pub issue_id: String,
    pub identifier: String,
    pub current: String,
    pub states: Vec<TeamState>,
    pub selected: usize,
}

impl ReplyBox {
    fn for_agent(a: &AgentSnapshot) -> Self {
        let status = a.info.agent_status.as_str();
        let refusal = if a.asking || status == "blocked" {
            Some("it's asking — answer it in the pane; ⏎ goes there".to_string())
        } else if matches!(status, "idle" | "done") || a.stalled.is_some() {
            None
        } else {
            Some(format!("it's {status} — ⏎ goes there"))
        };
        Self {
            pane: a.info.pane_id.clone(),
            title: title_of(a),
            text: String::new(),
            refusal,
            stall_hash: a.stall_hash.filter(|_| a.stalled.is_some()),
            sending: false,
        }
    }
}

pub struct App {
    pub state: WatchState,
    pub tab: Tab,
    /// Index into the Agents tab's visible rows.
    pub selected: usize,
    /// The selected row's key, so the selection follows it when rows move.
    selected_key: Option<String>,
    /// The Agents stage tab; `None` is All.
    pub stage: Option<Stage>,
    pub filter: String,
    /// The `/` filter line has the keyboard.
    pub filter_open: bool,
    pub modal: Option<CleanupModal>,
    /// The newest plan serial already shown, so a plan opens the dialog once.
    seen_plan: u64,
    /// Shown while a cleanup step runs on the worker.
    pub pending: Option<String>,
    pub launch: Option<LaunchModal>,
    /// The newest launch view already shown, so it opens the picker once.
    seen_launch: u64,
    /// The issue id whose lookup the open input dialog is waiting for. Only that
    /// lookup's result may open the picker; Esc or editing the id drops it.
    awaiting: Option<String>,
    seen_fetch: u64,
    pub reply: Option<ReplyBox>,
    seen_reply: u64,
    /// Index into the PRs tab's PRs (headings excluded).
    pub pr_selected: usize,
    /// The selected PR's node id, so the selection follows it across refetches.
    pr_key: Option<String>,
    /// The open PR screen: (PR id, detail request serial).
    pub pr_open: Option<(String, u64)>,
    pr_serial: u64,
    /// Index into the Issues tab's issues (headings excluded).
    pub issue_selected: usize,
    /// The selected issue's id, so the selection follows it when it regroups.
    issue_key: Option<String>,
    /// The open issue screen: (issue id, detail request serial).
    pub issue_open: Option<(String, u64)>,
    /// First line shown in an open PR or issue screen; `jk` scroll it.
    pub detail_scroll: u16,
    /// The last line `detail_scroll` can reach, as the last draw measured it.
    detail_max: std::cell::Cell<u16>,
    issue_serial: u64,
    pub picker: Option<StatePicker>,
    seen_states: u64,
    /// `s` is waiting for its re-read: (issue id, identifier).
    starting: Option<(String, String)>,
    /// A one-key note for the footer (e.g. why `d` did nothing); cleared by the next key.
    pub flash: Option<String>,
    pub factory: Factory,
    /// `SLACK_USER_TOKEN` and `SLACK_CHANNEL` are set.
    pub slack_ready: bool,
    /// Issue ids marked with Space, for one batched `f`.
    pub marks: Vec<String>,
    pub factory_selected: usize,
    pub factory_input: Option<FactoryInput>,
    pub factory_confirm: Option<FactoryConfirm>,
    pub pr_pick: Option<PrPick>,
    /// The post in flight came from the marks.
    factory_batch: bool,
    seen_factory: u64,
    /// Where agents' `werkbank factory draft` files land; `None` in tests
    /// that don't use them.
    drafts_dir: Option<PathBuf>,
    pub drafts: Vec<Draft>,
    /// The draft the pending post sends, deleted once it went out.
    factory_draft: Option<String>,
    /// The `?` screen is open, scrolled to this line.
    pub help: Option<u16>,
    /// The last line `help` can scroll to, as the last draw measured it.
    help_max: std::cell::Cell<u16>,
}

impl App {
    pub fn new() -> Self {
        Self {
            state: WatchState::default(),
            tab: Tab::Agents,
            selected: 0,
            selected_key: None,
            stage: None,
            filter: String::new(),
            filter_open: false,
            modal: None,
            seen_plan: 0,
            pending: None,
            launch: None,
            seen_launch: 0,
            awaiting: None,
            seen_fetch: 0,
            reply: None,
            seen_reply: 0,
            pr_selected: 0,
            pr_key: None,
            pr_open: None,
            pr_serial: 0,
            issue_selected: 0,
            issue_key: None,
            issue_open: None,
            detail_scroll: 0,
            detail_max: std::cell::Cell::new(0),
            issue_serial: 0,
            picker: None,
            seen_states: 0,
            starting: None,
            flash: None,
            factory: Factory::default(),
            slack_ready: false,
            marks: Vec::new(),
            factory_selected: 0,
            factory_input: None,
            factory_confirm: None,
            pr_pick: None,
            factory_batch: false,
            seen_factory: 0,
            drafts_dir: None,
            drafts: Vec::new(),
            factory_draft: None,
            help: None,
            help_max: std::cell::Cell::new(0),
        }
    }

    pub fn with_factory(mut self, factory: Factory, slack_ready: bool) -> Self {
        self.factory = factory;
        self.slack_ready = slack_ready;
        self
    }

    /// Opens the confirm with the exact message, or flashes what's missing.
    fn open_factory(&mut self, prompt: &Prompt, refs: &[String], batch: bool) -> Option<Command> {
        if self.factory_ready() {
            let message = self.factory.compose(prompt, refs);
            self.confirm_slack(message, batch, None);
        }
        None
    }

    /// The Slack confirm for `message`, unless Slack would refuse its length.
    fn confirm_slack(&mut self, message: Option<Message>, batch: bool, draft: Option<String>) {
        let Some(message) = message else { return };
        if let Some(why) = message.too_long() {
            self.flash = Some(why);
            return;
        }
        self.factory_confirm = Some(FactoryConfirm {
            message,
            scroll: 0,
            batch,
            local: None,
            draft,
        });
    }

    pub fn with_drafts(mut self, dir: PathBuf) -> Self {
        self.drafts_dir = Some(dir);
        self.load_drafts();
        self
    }

    /// Re-reads the drafts dir; true when the list changed.
    pub fn load_drafts(&mut self) -> bool {
        let Some(dir) = &self.drafts_dir else {
            return false;
        };
        let drafts = drafts::list(dir);
        if drafts == self.drafts {
            return false;
        }
        self.drafts = drafts;
        let last = self.factory_rows().len().saturating_sub(1);
        self.factory_selected = self.factory_selected.min(last);
        true
    }

    fn remove_draft(&mut self, id: &str) {
        if let Some(dir) = &self.drafts_dir {
            if let Err(err) = drafts::remove(dir, id) {
                self.flash = Some(format!("draft: {err:#}"));
            }
        }
        self.load_drafts();
    }

    /// Opens the confirm for a local agent on `pr` with `body` as its prompt.
    fn open_local(&mut self, pr: PrRef, body: String) {
        self.factory_confirm = Some(FactoryConfirm {
            message: Message {
                body,
                signature: None,
            },
            scroll: 0,
            batch: false,
            local: Some(pr),
            draft: None,
        });
    }

    fn on_pr_pick_key(&mut self, code: KeyCode) -> Option<Command> {
        let pick = self.pr_pick.as_mut()?;
        let last = self.factory.pr_prompts().len();
        match code {
            KeyCode::Esc => self.pr_pick = None,
            KeyCode::Char('j') | KeyCode::Down => pick.selected = (pick.selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => pick.selected = pick.selected.saturating_sub(1),
            KeyCode::Enter => {
                let pick = self.pr_pick.take()?;
                let prompt = self
                    .factory
                    .pr_prompts()
                    .get(pick.selected)
                    .map(|p| (*p).clone());
                match (prompt, pick.local) {
                    (Some(p), true) => {
                        let body = p.fill(std::slice::from_ref(&pick.pr.url));
                        self.open_local(pick.pr, body);
                    }
                    (Some(p), false) => return self.open_factory(&p, &[pick.pr.url], false),
                    (None, local) => {
                        self.factory_input = Some(FactoryInput {
                            prompt: None,
                            text: String::new(),
                            about: Some((pick.pr, local)),
                        });
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// `f`/`s` on a PR: the picker, once Slack is set for `f`.
    fn pick_for_pr(&mut self, local: bool) -> Option<Command> {
        let pr = self.selected_pr()?;
        if !local && !self.factory_ready() {
            return None;
        }
        self.pr_pick = Some(PrPick {
            pr: PrRef {
                repo: pr.repo,
                head_repo: pr.head_repo,
                head: pr.head_ref,
                number: pr.number,
                url: pr.url,
            },
            local,
            selected: 0,
        });
        None
    }

    fn factory_ready(&mut self) -> bool {
        let missing = if !self.slack_ready {
            "SLACK_USER_TOKEN or SLACK_CHANNEL"
        } else if self.factory.mention.is_none() {
            "SLACK_FACTORY_MENTION"
        } else {
            return true;
        };
        self.flash = Some(format!("{missing} not set — see README"));
        false
    }

    /// `f` on Issues: the marks in display order, else the issue on screen.
    fn factory_issues(&mut self) -> Option<Command> {
        let marked: Vec<String> = if self.issue_open.is_some() {
            Vec::new()
        } else {
            self.issue_rows()
                .into_iter()
                .filter(|i| self.marks.contains(&i.id))
                .map(|i| i.url.clone())
                .collect()
        };
        let (role, refs) = match marked.len() {
            0 => (Role::Issue, vec![self.selected_issue()?.url]),
            1 => (Role::Issue, marked),
            _ => (Role::IssueBatch, marked),
        };
        let batch = !self.marks.is_empty() && self.issue_open.is_none();
        let prompt = self.factory.for_role(role).clone();
        self.open_factory(&prompt, &refs, batch)
    }

    fn on_factory_confirm_key(&mut self, code: KeyCode) -> Option<Command> {
        let confirm = self.factory_confirm.as_mut()?;
        match code {
            KeyCode::Char('y') => {
                let c = self.factory_confirm.take()?;
                if let Some(pr) = c.local {
                    self.pending = Some(format!("starting an agent on #{}…", pr.number));
                    return Some(Command::StartPr {
                        repo: pr.repo,
                        head_repo: pr.head_repo,
                        head: pr.head,
                        number: pr.number,
                        prompt: c.message.body,
                    });
                }
                self.factory_batch = c.batch;
                self.factory_draft = c.draft;
                self.pending = Some("posting to Slack…".into());
                Some(Command::FactorySend { message: c.message })
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.factory_confirm = None;
                None
            }
            KeyCode::Char('d') if confirm.draft.is_some() => {
                let id = self.factory_confirm.take()?.draft?;
                self.remove_draft(&id);
                self.flash = self.flash.take().or(Some("draft discarded".into()));
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let max = confirm.message.preview().len() as u16;
                confirm.scroll = confirm.scroll.saturating_add(1).min(max);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                confirm.scroll = confirm.scroll.saturating_sub(1);
                None
            }
            _ => None,
        }
    }

    fn on_factory_input_key(&mut self, key: KeyEvent) -> Option<Command> {
        let input = self.factory_input.as_mut()?;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let newline = (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT))
            || (ctrl && key.code == KeyCode::Char('j'));
        match key.code {
            KeyCode::Esc => self.factory_input = None,
            _ if newline => input.push("\n"),
            KeyCode::Enter => {
                let text = input.text.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                let input = self.factory_input.take()?;
                return match (input.prompt, input.about) {
                    (_, Some((pr, true))) => {
                        let body = format!("{text}\n{}", pr.url);
                        self.open_local(pr, body);
                        None
                    }
                    (_, Some((pr, false))) => {
                        if self.factory_ready() {
                            let message = self.factory.compose_about(&text, &pr.url);
                            self.confirm_slack(message, false, None);
                        }
                        None
                    }
                    (Some(prompt), None) => self.open_factory(&prompt, &[text], false),
                    (None, None) => {
                        if self.factory_ready() {
                            let message = self.factory.compose_message(&text);
                            self.confirm_slack(message, false, None);
                        }
                        None
                    }
                };
            }
            KeyCode::Backspace => {
                input.text.pop();
            }
            KeyCode::Char(c) if !ctrl => input.push(&c.to_string()),
            _ => {}
        }
        None
    }

    /// Bracketed paste: text goes into the box that has the keyboard, never
    /// as keys, so a pasted newline can't submit and a pasted `y` can't confirm.
    pub fn on_paste(&mut self, text: &str) {
        if let Some(input) = self.factory_input.as_mut() {
            input.push(text);
        } else if let Some(reply) = self
            .reply
            .as_mut()
            .filter(|r| !r.sending && r.refusal.is_none())
        {
            for c in text.replace("\r\n", "\n").chars() {
                if reply.text.chars().count() >= REPLY_MAX {
                    break;
                }
                if c == '\n' || !c.is_control() {
                    reply.text.push(c);
                }
            }
        } else if self.filter_open
            || self.modal.is_some()
            || matches!(self.launch, Some(LaunchModal::Input(_)))
        {
            for c in text.chars().filter(|c| !c.is_control()) {
                self.on_key(KeyCode::Char(c));
            }
        }
    }

    /// The Factory tab's rows: a typed message, then every prompt it can
    /// send. The batch prompt needs marked issues, so it only lives on Issues.
    pub fn factory_rows(&self) -> Vec<FactoryRow<'_>> {
        self.drafts
            .iter()
            .map(FactoryRow::Draft)
            .chain(std::iter::once(FactoryRow::Message))
            .chain(
                self.factory
                    .prompts
                    .iter()
                    .filter(|p| !p.needs_refs())
                    .map(FactoryRow::Prompt),
            )
            .collect()
    }

    fn on_factory_key(&mut self, code: KeyCode) -> Option<Command> {
        let last = self.factory_rows().len().saturating_sub(1);
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.factory_selected = (self.factory_selected + 1).min(last);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.factory_selected = self.factory_selected.saturating_sub(1);
                None
            }
            KeyCode::Enter => {
                let prompt = match self.factory_rows().get(self.factory_selected)? {
                    FactoryRow::Draft(d) => {
                        let (id, text) = (d.id.clone(), d.text.clone());
                        if self.factory_ready() {
                            let message = self.factory.compose_draft(&text);
                            self.confirm_slack(message, false, Some(id));
                        }
                        return None;
                    }
                    FactoryRow::Message => None,
                    FactoryRow::Prompt(p) => Some((*p).clone()),
                };
                match prompt {
                    Some(p) if !p.needs_ref() => self.open_factory(&p, &[], false),
                    prompt => {
                        if self.factory_ready() {
                            self.factory_input = Some(FactoryInput {
                                prompt,
                                text: String::new(),
                                about: None,
                            });
                        }
                        None
                    }
                }
            }
            _ => None,
        }
    }

    /// A dialog has the keyboard, so `q` types instead of quitting.
    pub fn has_modal(&self) -> bool {
        self.modal.is_some()
            || self.launch.is_some()
            || self.filter_open
            || self.reply.is_some()
            || self.picker.is_some()
            || self.factory_input.is_some()
            || self.factory_confirm.is_some()
            || self.pr_pick.is_some()
            || self.help.is_some()
    }

    /// The issues in display order.
    pub fn issue_rows(&self) -> Vec<&MyIssue> {
        self.state
            .issue_list
            .data
            .as_ref()
            .map(|d| issues::issues(&d.mine))
            .unwrap_or_default()
    }

    fn selected_issue(&self) -> Option<MyIssue> {
        let id = match &self.issue_open {
            Some((id, _)) => id.clone(),
            None => self.issue_key.clone()?,
        };
        if let Some(i) = self.issue_rows().into_iter().find(|i| i.id == id) {
            return Some(i.clone());
        }
        // Gone from the list (e.g. moved to Done) but still on screen.
        let d = self.state.issue_detail.as_ref()?.detail.as_ref()?;
        (d.id == id).then(|| (**d).clone())
    }

    /// Keeps the issue selection on the same issue, else clamps it.
    fn reselect_issue(&mut self) {
        let ids: Vec<String> = self.issue_rows().iter().map(|i| i.id.clone()).collect();
        if let Some(at) = self
            .issue_key
            .as_ref()
            .and_then(|k| ids.iter().position(|i| i == k))
        {
            self.issue_selected = at;
        }
        self.issue_selected = self.issue_selected.min(ids.len().saturating_sub(1));
        self.issue_key = ids.get(self.issue_selected).cloned();
    }

    fn on_issue_key(&mut self, code: KeyCode) -> Option<Command> {
        match code {
            KeyCode::Char('j') | KeyCode::Down if self.issue_open.is_none() => {
                self.issue_selected += 1;
                self.issue_key = None;
                self.reselect_issue();
                None
            }
            KeyCode::Char('k') | KeyCode::Up if self.issue_open.is_none() => {
                self.issue_selected = self.issue_selected.saturating_sub(1);
                self.issue_key = None;
                self.reselect_issue();
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.detail_scroll = self
                    .detail_scroll
                    .saturating_add(1)
                    .min(self.detail_max.get());
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                // Clamp first: after a resize the stored value can sit past the end.
                self.detail_scroll = self
                    .detail_scroll
                    .min(self.detail_max.get())
                    .saturating_sub(1);
                None
            }
            KeyCode::Esc => {
                self.issue_open = None;
                None
            }
            KeyCode::Enter if self.issue_open.is_none() => {
                let issue = self.selected_issue()?;
                self.issue_serial += 1;
                self.issue_open = Some((issue.id.clone(), self.issue_serial));
                self.detail_scroll = 0;
                Some(Command::FetchIssueDetail {
                    serial: self.issue_serial,
                    id: issue.id,
                })
            }
            KeyCode::Char('o') => Some(Command::OpenUrl(self.selected_issue()?.url)),
            KeyCode::Char('c') => {
                let issue = self.selected_issue()?;
                self.pending = Some(format!("{}: loading states…", issue.identifier));
                Some(Command::FetchTeamStates {
                    issue_id: issue.id,
                    identifier: issue.identifier,
                    team_id: issue.team.id,
                    current: issue.state.id,
                })
            }
            KeyCode::Char('f') => self.factory_issues(),
            KeyCode::Char(' ') if self.issue_open.is_none() => {
                let id = self.issue_key.clone()?;
                match self.marks.iter().position(|m| *m == id) {
                    Some(at) => {
                        self.marks.remove(at);
                    }
                    None => self.marks.push(id),
                }
                None
            }
            KeyCode::Char('s') => {
                let issue = self.selected_issue()?;
                self.launch = Some(LaunchModal::Input(issue.identifier.clone()));
                self.awaiting = Some(issue.identifier.clone());
                self.pending = Some(format!("checking {}…", issue.identifier));
                self.starting = Some((issue.id.clone(), issue.identifier.clone()));
                Some(Command::PrepareStart {
                    id: issue.id,
                    identifier: issue.identifier,
                })
            }
            _ => None,
        }
    }

    fn on_picker_key(&mut self, code: KeyCode) -> Option<Command> {
        let picker = self.picker.as_mut()?;
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                picker.selected = (picker.selected + 1).min(picker.states.len().saturating_sub(1));
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
                None
            }
            KeyCode::Esc => {
                self.picker = None;
                None
            }
            KeyCode::Enter => {
                let p = self.picker.take()?;
                let s = p.states.get(p.selected)?;
                if s.id == p.current {
                    return None;
                }
                self.pending = Some(format!("{} → {}…", p.identifier, s.name));
                Some(Command::SetIssueState {
                    issue_id: p.issue_id,
                    identifier: p.identifier,
                    state: WorkflowState {
                        id: s.id.clone(),
                        name: s.name.clone(),
                        kind: s.kind.clone(),
                    },
                })
            }
            _ => None,
        }
    }

    /// The PRs in display order.
    pub fn pr_rows(&self) -> Vec<&ListPr> {
        self.state
            .prs
            .data
            .as_ref()
            .map(|d| prs::prs(&d.lists))
            .unwrap_or_default()
    }

    fn selected_pr(&self) -> Option<ListPr> {
        let id = match &self.pr_open {
            Some((id, _)) => id.clone(),
            None => self.pr_key.clone()?,
        };
        if let Some(p) = self.pr_rows().into_iter().find(|p| p.id == id) {
            return Some(p.clone());
        }
        // Gone from the list but still on screen: the detail's copy.
        let d = self.state.pr_detail.as_ref()?.detail.as_ref()?;
        (d.pr.id == id).then(|| d.pr.clone())
    }

    /// Keeps the PR selection on the same PR, else clamps it to a PR row.
    fn reselect_pr(&mut self) {
        let ids: Vec<String> = self.pr_rows().iter().map(|p| p.id.clone()).collect();
        if let Some(at) = self
            .pr_key
            .as_ref()
            .and_then(|k| ids.iter().position(|i| i == k))
        {
            self.pr_selected = at;
        }
        self.pr_selected = self.pr_selected.min(ids.len().saturating_sub(1));
        self.pr_key = ids.get(self.pr_selected).cloned();
    }

    fn switch_tab(&mut self, tab: Tab) -> Option<Command> {
        let changed = tab != self.tab;
        self.tab = tab;
        changed.then_some(Command::TabVisible(tab))
    }

    fn on_pr_key(&mut self, code: KeyCode) -> Option<Command> {
        match code {
            KeyCode::Char('j') | KeyCode::Down if self.pr_open.is_none() => {
                self.pr_selected += 1;
                self.pr_key = None;
                self.reselect_pr();
                None
            }
            KeyCode::Char('k') | KeyCode::Up if self.pr_open.is_none() => {
                self.pr_selected = self.pr_selected.saturating_sub(1);
                self.pr_key = None;
                self.reselect_pr();
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.detail_scroll = self
                    .detail_scroll
                    .saturating_add(1)
                    .min(self.detail_max.get());
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                // Clamp first: after a resize the stored value can sit past the end.
                self.detail_scroll = self
                    .detail_scroll
                    .min(self.detail_max.get())
                    .saturating_sub(1);
                None
            }
            KeyCode::Esc => {
                self.pr_open = None;
                None
            }
            KeyCode::Enter if self.pr_open.is_none() => {
                let pr = self.selected_pr()?;
                self.pr_serial += 1;
                self.pr_open = Some((pr.id.clone(), self.pr_serial));
                self.detail_scroll = 0;
                Some(Command::FetchPrDetail {
                    serial: self.pr_serial,
                    repo: pr.repo,
                    number: pr.number,
                })
            }
            KeyCode::Char('o') => Some(Command::OpenUrl(self.selected_pr()?.url)),
            KeyCode::Char('f') => self.pick_for_pr(false),
            KeyCode::Char('s') => self.pick_for_pr(true),
            KeyCode::Char('R') => {
                let pr = self.selected_pr()?;
                if pr.ci != Ci::Fail {
                    self.flash = Some(format!("#{}: no failing checks", pr.number));
                    return None;
                }
                self.pending = Some(format!("rerunning #{}…", pr.number));
                Some(Command::RerunFailed {
                    repo: pr.repo,
                    head: pr.head_ref,
                    number: pr.number,
                })
            }
            KeyCode::Char('w') => {
                let pr = self.selected_pr()?;
                self.pending = Some(format!("worktree for #{}…", pr.number));
                Some(Command::OpenPrWorktree {
                    repo: pr.repo,
                    head_repo: pr.head_repo,
                    head: pr.head_ref,
                    number: pr.number,
                })
            }
            _ => None,
        }
    }

    /// Every Agents row the filter keeps, across all stages.
    fn filtered(&self) -> Vec<Row> {
        let words: Vec<String> = self
            .filter
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        model::rows(&self.state.agents, &self.state.merged)
            .into_iter()
            .filter(|r| {
                let hay = haystack(r, &self.state).to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect()
    }

    /// The Agents rows shown: the filter's, in the current stage.
    pub fn rows(&self) -> Vec<Row> {
        self.filtered()
            .into_iter()
            .filter(|r| self.stage.is_none_or(|s| r.stage() == s))
            .collect()
    }

    fn selected_row(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.selected)
    }

    /// All, then every stage that has rows.
    fn stage_choices(&self) -> Vec<Option<Stage>> {
        let filtered = self.filtered();
        std::iter::once(None)
            .chain(
                Stage::ALL
                    .into_iter()
                    .filter(|s| filtered.iter().any(|r| r.stage() == *s))
                    .map(Some),
            )
            .collect()
    }

    fn step_stage(&mut self, by: isize) {
        let choices = self.stage_choices();
        let at = choices.iter().position(|c| *c == self.stage).unwrap_or(0) as isize;
        self.stage = choices[(at + by).rem_euclid(choices.len() as isize) as usize];
        self.reselect();
    }

    /// Keeps the selection on the same row when rows move, else clamps it.
    fn reselect(&mut self) {
        let rows = self.rows();
        if let Some(at) = self
            .selected_key
            .as_ref()
            .and_then(|k| rows.iter().position(|r| &r.key() == k))
        {
            self.selected = at;
        }
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        self.selected_key = rows.get(self.selected).map(Row::key);
    }

    fn move_selection(&mut self, by: isize) {
        let last = self.rows().len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + by).clamp(0, last) as usize;
        self.selected_key = None;
        self.reselect();
    }

    fn on_filter_key(&mut self, code: KeyCode) -> Option<Command> {
        match code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filter_open = false;
            }
            KeyCode::Enter => self.filter_open = false,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) if self.filter.chars().count() < 64 => self.filter.push(c),
            _ => {}
        }
        self.reselect();
        None
    }

    /// Takes a worker update; a new cleanup plan opens the dialog.
    pub fn apply(&mut self, state: WatchState) {
        if let Some(view) = &state.cleanup {
            if view.serial > self.seen_plan {
                self.seen_plan = view.serial;
                self.modal = Some(CleanupModal {
                    view: view.clone(),
                    input: String::new(),
                });
                self.pending = None;
            }
        }
        if let Some(view) = &state.launch {
            if view.serial > self.seen_launch {
                self.seen_launch = view.serial;
                let expected = self.awaiting.as_deref() == Some(view.issue.identifier.as_str());
                if expected && matches!(self.launch, Some(LaunchModal::Input(_))) {
                    self.awaiting = None;
                    self.launch = Some(LaunchModal::Pick {
                        view: view.clone(),
                        selected: 0,
                    });
                }
            }
        }
        // The awaited lookup finished without opening the picker (it failed):
        // let Enter retry it.
        if let Some((serial, id)) = &state.fetched {
            if *serial > self.seen_fetch {
                self.seen_fetch = *serial;
                if self.awaiting.as_ref() == Some(id) {
                    self.awaiting = None;
                }
            }
        }
        if let Some((serial, sent)) = state.replied {
            if serial > self.seen_reply {
                self.seen_reply = serial;
                if self.reply.as_ref().is_some_and(|r| r.sending) {
                    // Sent: done. Refused: keep the text so it can be retried.
                    if sent {
                        self.reply = None;
                    } else if let Some(r) = &mut self.reply {
                        r.sending = false;
                    }
                }
            }
        }
        if let Some(view) = &state.states {
            if view.serial > self.seen_states {
                self.seen_states = view.serial;
                self.pending = None;
                self.picker = Some(StatePicker {
                    issue_id: view.issue_id.clone(),
                    identifier: view.identifier.clone(),
                    current: view.current.clone(),
                    selected: view
                        .states
                        .iter()
                        .position(|s| s.id == view.current)
                        .unwrap_or(0),
                    states: view.states.clone(),
                });
            }
        }
        if state.notice != self.state.notice && !state.launching {
            self.pending = None;
        }
        if let Some((serial, sent)) = state.factory_sent {
            if serial > self.seen_factory {
                self.seen_factory = serial;
                self.pending = None;
                if sent && self.factory_batch {
                    self.marks.clear();
                }
                self.factory_batch = false;
                if let Some(id) = self.factory_draft.take().filter(|_| sent) {
                    self.remove_draft(&id);
                }
            }
        }
        self.state = state;
        self.reselect();
        self.reselect_pr();
        self.reselect_issue();
        if self.state.issue_list.data.is_some() {
            let ids: Vec<String> = self.issue_rows().iter().map(|i| i.id.clone()).collect();
            self.marks.retain(|m| ids.contains(m));
        }
    }

    fn on_modal_key(&mut self, code: KeyCode) -> Option<Command> {
        let modal = self.modal.as_mut()?;
        match code {
            KeyCode::Esc => {
                self.modal = None;
                None
            }
            KeyCode::Enter => {
                if !modal.can_confirm() {
                    return None;
                }
                let plan = modal.view.plan.clone();
                self.modal = None;
                self.pending = Some(format!("removing {}…", plan.checkout_path));
                Some(Command::ExecuteCleanup(plan))
            }
            KeyCode::Backspace => {
                modal.input.pop();
                None
            }
            // `DELETE` has no lowercase b, so this never collides with typing it.
            KeyCode::Char('b') => {
                modal.view.plan.delete_branch = !modal.view.plan.delete_branch;
                None
            }
            KeyCode::Char(c) if modal.view.refusals.is_empty() && modal.input.len() < 16 => {
                modal.input.push(c);
                None
            }
            _ => None,
        }
    }

    fn on_launch_key(&mut self, code: KeyCode) -> Option<Command> {
        let modal = self.launch.as_mut()?;
        match (modal, code) {
            (_, KeyCode::Esc) => {
                self.launch = None;
                self.pending = None;
                self.awaiting = None;
                self.starting = None;
                None
            }
            (LaunchModal::Input(input), KeyCode::Enter) => {
                let id = linear::parse_issue_id(input)?;
                if self.awaiting.as_ref() == Some(&id) {
                    return None;
                }
                if let Some((issue_id, identifier)) = self.starting.clone() {
                    self.awaiting = Some(identifier.clone());
                    self.pending = Some(format!("checking {identifier}…"));
                    return Some(Command::PrepareStart {
                        id: issue_id,
                        identifier,
                    });
                }
                self.awaiting = Some(id.clone());
                self.pending = Some(format!("fetching {id}…"));
                Some(Command::FetchIssue(id))
            }
            (LaunchModal::Input(_), KeyCode::Backspace | KeyCode::Char(_))
                if self.starting.is_some() =>
            {
                None
            }
            (LaunchModal::Input(input), KeyCode::Backspace) => {
                input.pop();
                self.awaiting = None;
                None
            }
            (LaunchModal::Input(input), KeyCode::Char(c)) if input.len() < 24 => {
                input.push(c);
                self.awaiting = None;
                None
            }
            (LaunchModal::Pick { view, selected }, KeyCode::Char('j') | KeyCode::Down) => {
                *selected = (*selected + 1).min(view.repos.len().saturating_sub(1));
                None
            }
            (LaunchModal::Pick { selected, .. }, KeyCode::Char('k') | KeyCode::Up) => {
                *selected = selected.saturating_sub(1);
                None
            }
            (LaunchModal::Pick { view, selected }, KeyCode::Enter) => {
                let repo = view.repos.get(*selected)?.clone();
                let issue = view.issue.clone();
                let start = view.start.clone();
                self.launch = None;
                self.starting = None;
                if let Some(start) = start {
                    self.pending =
                        Some(format!("starting {} in {}…", issue.identifier, repo.label));
                    return Some(Command::Start {
                        issue: start,
                        repo_root: repo.repo_root,
                    });
                }
                self.pending = Some(format!("launching {} in {}…", issue.identifier, repo.label));
                Some(Command::Launch {
                    issue,
                    repo_root: repo.repo_root,
                })
            }
            _ => None,
        }
    }

    /// Applies a key event; only the reply box needs its modifiers.
    pub fn on_key_event(&mut self, key: KeyEvent) -> Option<Command> {
        if self.reply.is_some() && self.modal.is_none() && self.launch.is_none() {
            return self.on_reply_key(key);
        }
        if self.factory_input.is_some() && self.factory_confirm.is_none() {
            return self.on_factory_input_key(key);
        }
        // Ctrl+<letter> is not the letter: Ctrl+C must not plan a cleanup.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return None;
        }
        self.on_key(key.code)
    }

    /// `q` closes the Inbox unless something is being typed; Ctrl+C always does.
    pub fn quits(&self, key: KeyEvent) -> bool {
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        ctrl_c || (key.code == KeyCode::Char('q') && key.modifiers.is_empty() && !self.has_modal())
    }

    fn on_reply_key(&mut self, key: KeyEvent) -> Option<Command> {
        let reply = self.reply.as_mut()?;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let newline = (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::ALT))
            || (ctrl && key.code == KeyCode::Char('j'));
        if key.code == KeyCode::Esc {
            self.reply = None;
            return None;
        }
        if reply.sending {
            return None;
        }
        if reply.refusal.is_some() {
            if key.code == KeyCode::Enter {
                let pane = reply.pane.clone();
                self.reply = None;
                return Some(Command::Focus(pane));
            }
            return None;
        }
        let room = reply.text.chars().count() < REPLY_MAX;
        match key.code {
            _ if newline => {
                if room {
                    reply.text.push('\n');
                }
            }
            KeyCode::Enter => {
                if reply.text.trim().is_empty() {
                    return None;
                }
                reply.sending = true;
                return Some(Command::Reply {
                    pane: reply.pane.clone(),
                    text: reply.text.clone(),
                    stall_hash: reply.stall_hash,
                });
            }
            KeyCode::Backspace => {
                reply.text.pop();
            }
            KeyCode::Char(c) if !ctrl && room => reply.text.push(c),
            _ => {}
        }
        None
    }

    fn on_help_key(&mut self, code: KeyCode) -> Option<Command> {
        let at = self.help.as_mut()?;
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                *at = at.saturating_add(1).min(self.help_max.get());
            }
            KeyCode::Char('k') | KeyCode::Up => {
                *at = (*at).min(self.help_max.get()).saturating_sub(1);
            }
            KeyCode::Char('?' | 'q') | KeyCode::Esc => self.help = None,
            _ => {}
        }
        None
    }

    /// Applies a key; returns a command for the worker, if any.
    pub fn on_key(&mut self, code: KeyCode) -> Option<Command> {
        if self.help.is_some() {
            return self.on_help_key(code);
        }
        if self.modal.is_some() {
            return self.on_modal_key(code);
        }
        if self.launch.is_some() {
            return self.on_launch_key(code);
        }
        if self.filter_open {
            return self.on_filter_key(code);
        }
        if self.picker.is_some() {
            return self.on_picker_key(code);
        }
        if self.factory_confirm.is_some() {
            return self.on_factory_confirm_key(code);
        }
        if self.factory_input.is_some() {
            return self.on_factory_input_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        if self.pr_pick.is_some() {
            return self.on_pr_pick_key(code);
        }
        self.flash = None;
        match code {
            KeyCode::Char('?') => {
                self.help = Some(0);
                None
            }
            KeyCode::Char(c @ '1'..='4') => self.switch_tab(Tab::ALL[c as usize - '1' as usize]),
            KeyCode::Char(']') => self.switch_tab(self.tab.step(1)),
            KeyCode::Char('[') => self.switch_tab(self.tab.step(-1)),
            KeyCode::Char('r') if self.tab == Tab::Prs => Some(Command::FetchPrs),
            KeyCode::Char('r') if self.tab == Tab::Issues => Some(Command::FetchIssues),
            KeyCode::Char('r') => Some(Command::Refresh),
            KeyCode::Char('n') if self.tab == Tab::Issues && self.issue_open.is_none() => {
                self.launch = Some(LaunchModal::Input(String::new()));
                None
            }
            _ if self.tab == Tab::Prs => self.on_pr_key(code),
            _ if self.tab == Tab::Issues => self.on_issue_key(code),
            _ if self.tab == Tab::Factory => self.on_factory_key(code),
            _ if self.tab != Tab::Agents => None,
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                None
            }
            KeyCode::Right => {
                self.step_stage(1);
                None
            }
            KeyCode::Left => {
                self.step_stage(-1);
                None
            }
            KeyCode::Esc => {
                if self.stage.is_some() {
                    self.stage = None;
                } else {
                    self.filter.clear();
                }
                self.reselect();
                None
            }
            KeyCode::Char('/') => {
                self.filter_open = true;
                None
            }
            KeyCode::Enter => match self.selected_row()? {
                Row::Agent(a) => Some(Command::Focus(a.info.pane_id)),
                Row::Worktree(_) => None,
            },
            KeyCode::Tab => {
                if let Row::Agent(a) = self.selected_row()? {
                    self.reply = Some(ReplyBox::for_agent(&a));
                }
                None
            }
            KeyCode::Char('o') => self
                .selected_row()?
                .pr()
                .filter(|p| !p.url.is_empty())
                .map(|p| Command::OpenUrl(p.url.clone())),
            KeyCode::Char('c') => {
                let Row::Worktree(item) = self.selected_row()? else {
                    return None;
                };
                self.pending = Some(format!("planning cleanup of {}…", item.workspace_label));
                Some(Command::PlanCleanup(item.workspace_id))
            }
            _ => None,
        }
    }
}

/// Longest workspace label shown before it is cut with `…`. Worktree workspaces are
/// named after their (often very long) branch.
const PLACE_MAX: usize = 24;

fn place(workspace_label: &str) -> String {
    if workspace_label.chars().count() > PLACE_MAX {
        let cut: String = workspace_label.chars().take(PLACE_MAX - 1).collect();
        format!("{cut}…")
    } else {
        workspace_label.to_string()
    }
}

/// What the `/` filter matches a row against.
fn haystack(row: &Row, state: &WatchState) -> String {
    let mut parts: Vec<String> = vec![row.stage().label().to_string()];
    match row {
        Row::Agent(a) => {
            parts.push(title_of(a));
            parts.push(a.workspace_label.clone());
            parts.extend(a.branch.clone());
            parts.extend(a.question.clone());
            if let Some(r) = &a.recap {
                parts.extend(r.lines.iter().cloned());
                parts.extend(r.now.clone());
                parts.extend(r.judge.clone());
            }
            if let Some(id) = a.branch.as_deref().and_then(linear::issue_id_from_branch) {
                if let Some(i) = state.issues.get(&id) {
                    parts.push(i.title.clone());
                    parts.push(i.state_name().to_string());
                }
                parts.push(id);
            }
            if let Some(pr) = &a.pr {
                parts.push(format!("#{}", pr.number));
            }
        }
        Row::Worktree(w) => {
            parts.push(w.workspace_label.clone());
            parts.push(w.title.clone());
            parts.push(w.detail.clone());
        }
    }
    parts.join("\n")
}

pub(crate) fn width_of(s: &str) -> usize {
    Span::raw(s).width()
}

/// `s` cut with `…` to at most `max` columns.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if width_of(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = width_of(c.encode_utf8(&mut [0; 4]));
        if used + w + 1 > max {
            break;
        }
        used += w;
        out.push(c);
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// A PR token's colour: merged mauve, closed dim, then by CI.
fn pr_color(pr: &PrInfo) -> Color {
    let t = theme();
    match (pr.state, pr.ci) {
        (PrState::Merged, _) => t.mauve,
        (PrState::Closed, _) => t.overlay,
        (_, Ci::Fail) => t.red,
        (_, Ci::Pending) => t.yellow,
        (_, Ci::Pass) => t.green,
        (_, Ci::None) => t.blue,
    }
}

/// Secondary text: the theme's overlay colour.
pub(crate) fn dim() -> Style {
    Style::new().fg(theme().overlay)
}

pub(crate) fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

/// The selected row: the theme's surface behind it, its colours kept.
pub(crate) fn selected_style() -> Style {
    Style::new().bg(theme().surface)
}

/// A chosen tab or stage: accent ground.
fn chosen_style() -> Style {
    Style::new()
        .bg(theme().accent)
        .fg(theme().surface)
        .add_modifier(Modifier::BOLD)
}

/// Styled parts cut like `truncate`; the `…` keeps the style of the part it cuts.
pub(crate) fn cut_spans(parts: Vec<(String, Style)>, max: usize) -> Vec<Span<'static>> {
    let total: usize = parts.iter().map(|(t, _)| width_of(t)).sum();
    if total <= max {
        return parts
            .into_iter()
            .filter(|(t, _)| !t.is_empty())
            .map(|(t, st)| Span::styled(t, st))
            .collect();
    }
    let mut out = Vec::new();
    let mut used = 0;
    for (t, st) in parts {
        let mut kept = String::new();
        for c in t.chars() {
            let w = width_of(c.encode_utf8(&mut [0; 4]));
            if used + w + 1 > max {
                if !kept.is_empty() {
                    out.push(Span::styled(kept, st));
                }
                if max > 0 {
                    out.push(Span::styled("…", st));
                }
                return out;
            }
            used += w;
            kept.push(c);
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, st));
        }
    }
    out
}

/// Styled `left` and `right` on one line of `width` columns, `right` flush
/// right; the left side is cut first.
pub(crate) fn fit_spans(
    left: Vec<(String, Style)>,
    right: Vec<(String, Style)>,
    width: usize,
) -> Line<'static> {
    let rw: usize = right.iter().map(|(t, _)| width_of(t)).sum();
    if rw + 2 >= width {
        return Line::from(cut_spans(left, width));
    }
    let mut spans = cut_spans(left, width - rw - 1);
    let lw: usize = spans.iter().map(|s| s.width()).sum();
    spans.push(Span::raw(" ".repeat(width - lw - rw)));
    spans.extend(right.into_iter().map(|(t, st)| Span::styled(t, st)));
    Line::from(spans)
}

/// A `· `-joined meta line, each part in its own colour.
pub(crate) fn meta_spans(indent: &str, parts: Vec<(String, Style)>, width: usize) -> Line<'static> {
    let mut all = vec![(indent.to_string(), Style::new())];
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            all.push((" · ".to_string(), dim()));
        }
        all.push(part);
    }
    Line::from(cut_spans(all, width))
}

/// The lines of one Agents row, `width` columns wide.
fn row_lines(row: &Row, state: &WatchState, width: usize, now_ms: u64) -> Vec<Line<'static>> {
    let indent = |s: &str| truncate(&format!("  {s}"), width);
    match row {
        Row::Agent(a) => {
            let t = theme();
            let stage = fg(stage_of(a).color());
            let mut right = vec![(place(&a.workspace_label), fg(t.subtext))];
            if a.state_since_unix_ms > 0 {
                let age = age_text(now_ms.saturating_sub(a.state_since_unix_ms));
                right.push((format!(" · {age}"), dim()));
            }
            let mut lines = vec![fit_spans(
                vec![
                    (format!("{} ", glyph_of(a)), stage),
                    (
                        title_of(a).to_string(),
                        fg(t.text).add_modifier(Modifier::BOLD),
                    ),
                ],
                right,
                width,
            )];
            if a.asking {
                let q = a.question.as_deref().unwrap_or("waiting for an answer");
                lines.push(Line::styled(indent(&format!("? {q}")), fg(t.red)));
            }
            let now = a.recap.as_ref().and_then(|r| r.now.as_deref());
            if let Some(n) = now {
                lines.push(Line::from(vec![
                    Span::styled("  now  ", dim()),
                    Span::styled(truncate(n, width.saturating_sub(7)), fg(t.subtext)),
                ]));
            }
            if let Some(r) = a.recap.as_ref().filter(|r| r.goal_runs.is_some()) {
                let (used, max) = r.goal_runs.unwrap_or_default();
                let mut goal = format!("goal {used}/{max}");
                if let Some(j) = &r.judge {
                    goal.push_str(&format!(" · {j}"));
                }
                lines.push(Line::styled(indent(&goal), fg(t.teal)));
            }
            // The memory recap is history: only shown when there is no live line.
            match a
                .recap
                .as_ref()
                .filter(|r| !r.lines.is_empty() && now.is_none())
            {
                Some(r) => {
                    let style = if r.stale {
                        let age = r.observed_unix_ms.map_or("a while".to_string(), |t| {
                            format!("{} ago", age_text(now_ms.saturating_sub(t)))
                        });
                        lines.push(Line::styled(indent(&format!("recap from {age}")), dim()));
                        dim()
                    } else {
                        fg(t.subtext)
                    };
                    for (i, l) in r.lines.iter().take(2).enumerate() {
                        // A partial recap's window started mid-group: say text came before.
                        let l = if i == 0 && r.partial {
                            format!("… {l}")
                        } else {
                            l.clone()
                        };
                        lines.push(Line::styled(indent(&l), style));
                    }
                }
                None if now.is_none() => lines.push(Line::styled(indent("no recap"), dim())),
                None => {}
            }
            let mut meta: Vec<(String, Style)> = Vec::new();
            let issue = a.branch.as_deref().and_then(linear::issue_id_from_branch);
            // The issue names the branch already, so the branch only shows without one.
            match &issue {
                Some(id) => match state.issues.get(id) {
                    Some(i) => meta.push((format!("{id} {}", i.state_name()), fg(t.green))),
                    None => meta.push((id.clone(), fg(t.green))),
                },
                None => meta.extend(a.branch.clone().map(|b| (b, fg(t.mauve)))),
            }
            if let Some(r) = a.recap.as_ref().filter(|r| r.tasks_total > 0) {
                meta.push((
                    format!("{}/{} tasks", r.tasks_done, r.tasks_total),
                    fg(t.teal),
                ));
            }
            if let Some(pr) = &a.pr {
                meta.push((pr.token(), fg(pr_color(pr))));
            }
            if !meta.is_empty() {
                lines.push(meta_spans("  ", meta, width));
            }
            lines
        }
        Row::Worktree(w) => vec![
            fit_spans(
                vec![
                    (
                        format!("{} ", Stage::Merged.glyph()),
                        fg(Stage::Merged.color()),
                    ),
                    (
                        w.workspace_label.clone(),
                        fg(theme().text).add_modifier(Modifier::BOLD),
                    ),
                ],
                vec![("no agent".to_string(), dim())],
                width,
            ),
            Line::styled(indent(&format!("{}  {}", w.detail, w.title)), dim()),
        ],
    }
}

/// `key label` pills; the key stands out.
fn pills(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (key, label)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::new()
                .fg(theme().accent)
                .bg(theme().surface)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}"), fg(theme().subtext)));
    }
    Line::from(spans)
}

fn draw_agents(frame: &mut Frame, app: &App, area: Rect, now_ms: u64) {
    let show_filter = app.filter_open || !app.filter.is_empty();
    let [strip, filter, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(show_filter)),
        Constraint::Min(1),
    ])
    .areas(area);

    let filtered = app.filtered();
    let mut spans = Vec::new();
    for (i, choice) in app.stage_choices().into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        let (label, n) = match choice {
            None => ("All", filtered.len()),
            Some(s) => (
                s.label(),
                filtered.iter().filter(|r| r.stage() == s).count(),
            ),
        };
        let style = if choice == app.stage {
            chosen_style()
        } else {
            fg(choice.map_or(theme().text, Stage::color))
        };
        spans.push(Span::styled(format!("{label} {n}"), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), strip);
    if show_filter {
        let cursor = if app.filter_open { "_" } else { "" };
        frame.render_widget(Paragraph::new(format!("/ {}{cursor}", app.filter)), filter);
    }

    let rows = app.rows();
    if rows.is_empty() {
        let text = if app.state.agents.is_empty() && app.state.merged.is_empty() {
            "no Mastra Code agents"
        } else {
            "no agents match"
        };
        frame.render_widget(Paragraph::new(text), list);
        return;
    }
    // The `› ` marker takes two columns on every row.
    let width = usize::from(list.width.saturating_sub(2));
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| ListItem::new(row_lines(r, &app.state, width, now_ms)))
        .collect();
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(selected_style())
            .highlight_symbol("› "),
        list,
        &mut state,
    );
}

/// The PRs tab: a header with the counts and fetch state, then the grouped
/// list or one PR's screen. Renders only cached state: no Runner calls.
fn draw_prs(frame: &mut Frame, app: &App, area: Rect, now_ms: u64) {
    let view = &app.state.prs;
    let Some(data) = &view.data else {
        let text = match &view.error {
            Some(err) => format!("couldn't fetch pull requests: {err}"),
            None => "fetching pull requests…".to_string(),
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
        return;
    };
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let mut top = prs::header(&data.lists);
    if view.updating {
        top.push_str(" · updating…");
    }
    if let Some(err) = &view.error {
        top.push_str(&format!(" · stale · {err}"));
    }
    frame.render_widget(
        Paragraph::new(Line::styled(truncate(&top, usize::from(head.width)), dim())),
        head,
    );
    let width = usize::from(body.width);
    if let Some((id, serial)) = &app.pr_open {
        let Some(listed) = app.selected_pr() else {
            frame.render_widget(Paragraph::new("that PR is gone — esc"), body);
            return;
        };
        let reply = app.state.pr_detail.as_ref().filter(|d| d.serial == *serial);
        let detail = reply
            .and_then(|d| d.detail.as_deref())
            .filter(|d| &d.pr.id == id);
        let loading = match reply {
            None => Some("loading…".to_string()),
            Some(d) => d.error.as_ref().map(|e| format!("couldn't load: {e}")),
        };
        let lines = prs::detail_lines(
            &listed,
            detail,
            loading.as_deref(),
            &data.checkouts,
            width,
            now_ms,
        );
        draw_detail(frame, app, body, lines);
        return;
    }
    let agent_ws: Vec<&str> = app
        .state
        .agents
        .iter()
        .map(|a| a.info.workspace_id.as_str())
        .collect();
    // The `› ` marker takes two columns on every row.
    let inner = width.saturating_sub(2);
    let mut selected = None;
    let mut seen = 0;
    let items: Vec<ListItem> = prs::items(&data.lists)
        .into_iter()
        .enumerate()
        .map(|(i, item)| match item {
            Item::Heading(g, n) => ListItem::new(Line::styled(
                format!("{} ({n})", g.label()),
                fg(g.color()).add_modifier(Modifier::BOLD),
            )),
            Item::Pr(p) => {
                if seen == app.pr_selected {
                    selected = Some(i);
                }
                seen += 1;
                ListItem::new(prs::row_lines(p, &data.checkouts, &agent_ws, inner, now_ms))
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(selected_style())
            .highlight_symbol("› "),
        body,
        &mut state,
    );
}

/// The Issues tab: a header with the count and fetch state, then the grouped
/// list or one issue's screen. Renders only cached state: no Runner calls.
fn draw_issues(frame: &mut Frame, app: &App, area: Rect) {
    let view = &app.state.issue_list;
    let Some(data) = &view.data else {
        let text = match &view.error {
            Some(err) => format!("couldn't fetch Linear issues: {err}"),
            None => "fetching Linear issues…".to_string(),
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
        return;
    };
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let mut top = issues::header(&data.mine);
    if !app.marks.is_empty() {
        top.push_str(&format!(" · {} marked", app.marks.len()));
    }
    if view.updating {
        top.push_str(" · updating…");
    }
    if let Some(err) = &view.error {
        top.push_str(&format!(" · stale · {err}"));
    }
    frame.render_widget(
        Paragraph::new(Line::styled(truncate(&top, usize::from(head.width)), dim())),
        head,
    );
    let width = usize::from(body.width);
    if let Some((id, serial)) = &app.issue_open {
        let Some(listed) = app.selected_issue() else {
            frame.render_widget(Paragraph::new("that issue is gone — esc"), body);
            return;
        };
        let reply = app
            .state
            .issue_detail
            .as_ref()
            .filter(|d| d.serial == *serial);
        let detail = reply
            .and_then(|d| d.detail.as_deref())
            .filter(|d| &d.id == id);
        let loading = match reply {
            None => Some("loading…".to_string()),
            Some(d) => d.error.as_ref().map(|e| format!("couldn't load: {e}")),
        };
        let lines =
            issues::detail_lines(&listed, detail, loading.as_deref(), &data.checkouts, width);
        draw_detail(frame, app, body, lines);
        return;
    }
    let inner = width.saturating_sub(2);
    let mut selected = None;
    let mut seen = 0;
    let items: Vec<ListItem> = issues::items(&data.mine)
        .into_iter()
        .enumerate()
        .map(|(i, item)| match item {
            issues::Item::Heading(g, n) => ListItem::new(Line::styled(
                format!("{} ({n})", g.label()),
                fg(g.color()).add_modifier(Modifier::BOLD),
            )),
            issues::Item::Issue(issue) => {
                if seen == app.issue_selected {
                    selected = Some(i);
                }
                seen += 1;
                if app.marks.is_empty() {
                    return ListItem::new(issues::row_line(issue, &data.checkouts, inner));
                }
                // The mark column only exists while something is marked.
                let mark = if app.marks.contains(&issue.id) {
                    "• "
                } else {
                    "  "
                };
                let mut line = issues::row_line(issue, &data.checkouts, inner.saturating_sub(2));
                line.spans.insert(0, Span::styled(mark, fg(theme().accent)));
                ListItem::new(line)
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(selected_style())
            .highlight_symbol("› "),
        body,
        &mut state,
    );
}

fn draw_picker(frame: &mut Frame, area: Rect, p: &StatePicker) {
    frame.render_widget(Clear, area);
    let inner = usize::from(area.width).saturating_sub(4);
    let items: Vec<ListItem> = issues::state_lines(&p.states, &p.current, inner)
        .into_iter()
        .map(ListItem::new)
        .collect();
    let mut state = ListState::default().with_selected(Some(p.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .title(format!(" Move {} to ", p.identifier)),
            )
            .highlight_style(selected_style())
            .highlight_symbol("› "),
        area,
        &mut state,
    );
}

/// An open PR or issue screen, scrolled by `jk`. It wraps the lines itself so the
/// row count that bounds the scroll is exactly what is drawn.
fn draw_detail(frame: &mut Frame, app: &App, body: Rect, lines: Vec<Line<'static>>) {
    let rows: Vec<Line<'static>> = lines
        .into_iter()
        .flat_map(|l| wrap_line(l, usize::from(body.width.max(1))))
        .collect();
    let max = rows.len().saturating_sub(usize::from(body.height));
    app.detail_max.set(u16::try_from(max).unwrap_or(u16::MAX));
    let scroll = app.detail_scroll.min(app.detail_max.get());
    frame.render_widget(Paragraph::new(rows).scroll((scroll, 0)), body);
}

/// Word-wraps one styled line to `width` columns, breaking inside a word only when
/// it alone is wider than a row.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| {
            let style = line.style.patch(s.style);
            s.content.chars().map(move |c| (c, style))
        })
        .collect();
    let w = |c: char| Span::raw(c.to_string()).width();
    let mut rows = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let (mut end, mut used, mut space) = (start, 0, None);
        while end < cells.len() && used + w(cells[end].0) <= width {
            if cells[end].0 == ' ' {
                space = Some(end);
            }
            used += w(cells[end].0);
            end += 1;
        }
        if end == start {
            end += 1; // a glyph wider than the row still makes progress
        } else if end < cells.len() && cells[end].0 != ' ' {
            if let Some(sp) = space.filter(|&sp| sp > start) {
                end = sp + 1;
            }
        }
        let row = &cells[start..end];
        let mut spans: Vec<Span<'static>> = Vec::new();
        for &(c, style) in row {
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
        }
        rows.push(Line::from(spans));
        start = end;
    }
    if rows.is_empty() {
        rows.push(Line::default());
    }
    rows
}

/// The selected agent's goal, else what it is doing now.
fn hint_line(app: &App) -> String {
    let Some(Row::Agent(a)) = app.selected_row() else {
        return String::new();
    };
    let Some(r) = &a.recap else {
        return String::new();
    };
    match (&r.goal, &r.active_task) {
        (Some(g), _) => format!("goal: {g}"),
        (None, Some(t)) => format!("doing: {t}"),
        (None, None) => String::new(),
    }
}

pub fn draw(frame: &mut Frame, app: &App, now_ms: u64) {
    let [header, body, hint, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let mut spans = vec![Span::styled(
        "werkbank  ",
        fg(theme().accent).add_modifier(Modifier::BOLD),
    )];
    for (i, t) in Tab::ALL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", dim()));
        }
        let style = if app.tab == t {
            chosen_style()
        } else {
            fg(theme().subtext)
        };
        let count = match (t, app.drafts.len()) {
            (Tab::Factory, n) if n > 0 => format!(" ({n})"),
            _ => String::new(),
        };
        spans.push(Span::styled(
            format!(" {} {}{count} ", i + 1, t.label()),
            style,
        ));
    }
    spans.push(Span::styled(
        match app.state.agents.len() {
            1 => "  · 1 agent".to_string(),
            n => format!("  · {n} agents"),
        },
        dim(),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), header);

    match app.tab {
        Tab::Agents => draw_agents(frame, app, body, now_ms),
        Tab::Prs => draw_prs(frame, app, body, now_ms),
        Tab::Issues => draw_issues(frame, app, body),
        Tab::Factory => draw_factory(frame, app, body),
    }
    if let Some(p) = &app.picker {
        draw_picker(frame, body, p);
    }

    if let Some(modal) = &app.modal {
        draw_cleanup(frame, body, modal);
    }
    if let Some(modal) = &app.launch {
        draw_launch(frame, body, modal);
    }
    if let Some(reply) = &app.reply {
        draw_reply(frame, body, reply);
    }
    if let Some(pick) = &app.pr_pick {
        draw_pr_pick(frame, body, app, pick);
    }
    if let Some(input) = &app.factory_input {
        draw_factory_input(frame, body, input);
    }
    if let Some(confirm) = &app.factory_confirm {
        draw_factory_confirm(frame, body, confirm);
    }
    if let Some(at) = app.help {
        draw_help(frame, body, app, at);
    }

    if app.tab == Tab::Agents {
        let text = truncate(&hint_line(app), usize::from(hint.width));
        frame.render_widget(Paragraph::new(Line::styled(text, dim())), hint);
    }

    let mut status = match app.state.watcher {
        WatcherStatus::Running => format!(
            "updated {}s ago",
            app.state
                .ticked_unix_ms
                .map_or(0, |t| now_ms.saturating_sub(t) / 1000)
        ),
        WatcherStatus::Busy(secs) => format!("watcher busy ({secs}s)"),
        WatcherStatus::Starting => "watcher not running — starting…".to_string(),
        WatcherStatus::Waiting => "waiting for watcher…".to_string(),
    };
    // The error goes first so a narrow pane still shows it; the line is cut with
    // `…` at the pane width.
    if let Some(err) = &app.state.error {
        status.push_str(&format!(" · {err}"));
    }
    if let Some(f) = &app.flash {
        status.push_str(&format!(" · {f}"));
    }
    if let Some(p) = &app.pending {
        status.push_str(&format!(" · working… {p}"));
    } else if let Some(n) = &app.state.notice {
        status.push_str(&format!(" · {n}"));
    }
    if app.state.linear_missing_key {
        status.push_str(" · LINEAR_API_KEY not set — see README");
    }
    if let Some(w) = &app.factory.warning {
        status.push_str(&format!(" · {w}"));
    }
    if app.tab == Tab::Factory && !app.slack_ready {
        status.push_str(" · SLACK_USER_TOKEN or SLACK_CHANNEL not set — see README");
    }
    // The daemon's threshold, which is what actually flags stalls.
    if let Some(secs) = app.state.stall_secs {
        status.push_str(&format!(" · stall {secs}s"));
    }
    // Keys get their own line so a long notice can't push them out of view.
    // Row keys only show where they apply, so `q quit` stays on a 100-column line.
    let selected = app.selected_row();
    let agent_keys: Vec<(&str, &str)> = [("jk", "move"), ("←→", "stage"), ("/", "filter")]
        .into_iter()
        .chain(match &selected {
            Some(Row::Agent(_)) => vec![("⏎", "focus"), ("tab", "reply")],
            Some(Row::Worktree(_)) => vec![("c", "cleanup")],
            None => vec![],
        })
        .chain(selected.as_ref().and_then(Row::pr).map(|_| ("o", "PR")))
        .chain([("r", "refresh"), ("?", "help"), ("q", "quit")])
        .collect();
    let keys: &[(&str, &str)] = match app.tab {
        _ if app.help.is_some() => &[("jk", "scroll"), ("esc", "close")],
        _ if app
            .modal
            .as_ref()
            .is_some_and(|m| !m.view.refusals.is_empty()) =>
        {
            &[("esc", "close")]
        }
        _ if app.modal.is_some() => &[("⏎", "remove"), ("b", "branch too"), ("esc", "cancel")],
        _ if matches!(app.launch, Some(LaunchModal::Pick { .. })) => {
            &[("jk", "pick"), ("⏎", "go"), ("esc", "cancel")]
        }
        _ if app.launch.is_some() => &[("⏎", "look up"), ("esc", "cancel")],
        _ if app.filter_open => &[("⏎", "keep filter"), ("esc", "clear"), ("⌫", "erase")],
        _ if app.reply.as_ref().is_some_and(|r| r.refusal.is_some()) => {
            &[("⏎", "go to pane"), ("esc", "close")]
        }
        _ if app.reply.is_some() => &[("⏎", "send"), ("alt+⏎", "newline"), ("esc", "drop")],
        _ if app.picker.is_some() => &[("jk", "move"), ("⏎", "set"), ("esc", "cancel")],
        _ if app
            .factory_confirm
            .as_ref()
            .is_some_and(|c| c.draft.is_some()) =>
        {
            &[
                ("y", "send"),
                ("d", "discard"),
                ("n", "keep"),
                ("jk", "scroll"),
            ]
        }
        _ if app.factory_confirm.is_some() => &[("y", "send"), ("n", "cancel"), ("jk", "scroll")],
        _ if app.pr_pick.is_some() => &[("jk", "pick"), ("⏎", "next"), ("esc", "cancel")],
        _ if app
            .factory_input
            .as_ref()
            .is_some_and(|i| i.prompt.is_none()) =>
        {
            &[("⏎", "next"), ("alt+⏎", "newline"), ("esc", "cancel")]
        }
        _ if app.factory_input.is_some() => &[("⏎", "next"), ("esc", "cancel"), ("⌫", "erase")],
        Tab::Agents => &agent_keys,
        Tab::Prs if app.pr_open.is_some() => &[
            ("esc", "back"),
            ("jk", "scroll"),
            ("s", "start"),
            ("f", "factory"),
            ("R", "rerun"),
            ("w", "worktree"),
            ("o", "open"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Prs => &[
            ("jk", "move"),
            ("⏎", "open"),
            ("s", "start"),
            ("f", "factory"),
            ("R", "rerun"),
            ("w", "worktree"),
            ("o", "open"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Issues if app.issue_open.is_some() => &[
            ("esc", "back"),
            ("jk", "scroll"),
            ("c", "status"),
            ("s", "start"),
            ("f", "factory"),
            ("o", "open"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Issues => &[
            ("jk", "move"),
            ("⏎", "open"),
            ("c", "status"),
            ("s", "start"),
            ("f", "factory"),
            ("spc", "mark"),
            ("o", "open"),
            ("n", "by id"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Factory => &[
            ("jk", "move"),
            ("⏎", "use"),
            ("1-4", "tabs"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(truncate(&status, usize::from(footer.width))),
            pills(keys),
        ]),
        footer,
    );
}

/// `(key or glyph, what it means)` rows of the help screen.
type HelpRows = &'static [(&'static str, &'static str)];

/// What the current tab shows, its glyphs and every key, then the keys that work everywhere.
pub fn help_lines(tab: Tab, herdr: &[(String, String)]) -> Vec<Line<'static>> {
    let (about, legend, keys): (&[&str], HelpRows, HelpRows) = match tab {
        Tab::Agents => (
            &[
                "Every Mastra Code agent in this Herdr session, plus merged worktrees",
                "waiting for cleanup. Rows are grouped by stage, oldest first, and show",
                "what the agent is doing now and how far its goal is.",
            ],
            &[
                (
                    "✗",
                    "needs fixing: stalled (⏸), failing CI, conflicts or changes requested",
                ),
                ("?", "asks you a question"),
                ("!", "blocked, waiting for a permission"),
                ("✓", "done"),
                ("○", "idle"),
                ("●", "working"),
                ("◕", "its PR is in review"),
                ("⇣", "PR merged, the worktree can be cleaned up"),
            ],
            &[
                ("j k", "move"),
                ("← →", "show one stage at a time"),
                ("/", "filter by text"),
                ("esc", "clear the stage, then the filter"),
                ("⏎", "focus the agent's pane"),
                ("tab", "reply to the agent without leaving the Inbox"),
                ("o", "open the agent's PR"),
                ("c", "clean up a merged worktree (asks you to type DELETE)"),
                ("r", "refresh now"),
            ],
        ),
        Tab::Prs => (
            &[
                "Your open PRs and the ones assigned to you. ⏎ opens the details:",
                "checks, reviews and the description.",
            ],
            &[
                ("●", "ready for review"),
                ("◌", "draft"),
                ("✓ ✗ ●", "checks pass, fail or are running"),
                ("!", "conflicts or behind the base branch"),
            ],
            &[
                ("j k", "move, or scroll the details"),
                ("⏎", "open the details"),
                ("esc", "back to the list"),
                (
                    "s",
                    "start a local agent in the PR's worktree with a prompt",
                ),
                ("f", "send a prompt about the PR to Factory on Slack"),
                ("R", "rerun the failed GitHub Actions jobs"),
                ("w", "open the PR's worktree, creating it if needed"),
                ("o", "open in the browser"),
                ("r", "fetch again"),
            ],
        ),
        Tab::Issues => (
            &[
                "Your open Linear issues, grouped by status and sorted by priority.",
                "⏎ opens the description and comments.",
            ],
            &[],
            &[
                ("j k", "move, or scroll the details"),
                ("⏎", "open the details"),
                ("esc", "back to the list"),
                ("c", "change the status"),
                (
                    "s",
                    "start locally: worktree, Mastra Code and the issue prompt",
                ),
                ("f", "send to Factory on Slack; marked issues go as one PR"),
                ("space", "mark or unmark an issue for f"),
                ("n", "start any issue by its ID"),
                ("o", "open in Linear"),
                ("r", "fetch again"),
            ],
        ),
        Tab::Factory => (
            &[
                "Prompts for Factory, posted to Slack as you. Drafts come first:",
                "messages agents saved with `werkbank factory draft`. Message lets",
                "you write your own. Prompts about an issue or PR ask for its URL.",
                "Every post shows the exact text first and only goes out on y.",
            ],
            &[],
            &[
                ("j k", "move"),
                ("⏎", "open the draft or use the prompt"),
                ("y n", "send or cancel in the preview"),
                ("d", "discard the draft in its preview"),
            ],
        ),
    };
    let heading = |t: &str| {
        Line::styled(
            t.to_string(),
            fg(theme().accent).add_modifier(Modifier::BOLD),
        )
    };
    let entry = |k: &str, text: &str| {
        Line::from(vec![
            Span::styled(
                format!(" {k:<6}"),
                fg(theme().accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {text}"), fg(theme().subtext)),
        ])
    };
    let mut lines = vec![heading(tab.label())];
    lines.extend(about.iter().map(|l| Line::raw(l.to_string())));
    if !legend.is_empty() {
        lines.push(Line::raw(""));
        lines.push(heading("Glyphs"));
        lines.extend(legend.iter().map(|(k, t)| entry(k, t)));
    }
    lines.push(Line::raw(""));
    lines.push(heading("Keys"));
    lines.extend(keys.iter().map(|(k, t)| entry(k, t)));
    lines.push(Line::raw(""));
    lines.push(heading("Everywhere"));
    lines.extend(
        [
            ("1-4", "switch tabs, or [ and ]"),
            ("?", "this help, for the tab you are on"),
            ("q", "quit (Ctrl+C always quits)"),
        ]
        .iter()
        .map(|(k, t)| entry(k, t)),
    );
    lines.push(Line::raw(""));
    lines.push(heading("In Herdr"));
    if herdr.is_empty() {
        lines.push(Line::styled(
            "No Werkbank actions are bound in Herdr's config.toml; see the README.",
            fg(theme().subtext),
        ));
    }
    let width = herdr
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    lines.extend(herdr.iter().map(|(k, t)| {
        Line::from(vec![
            Span::styled(
                format!(" {k:<width$}"),
                fg(theme().accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {t}"), fg(theme().subtext)),
        ])
    }));
    lines
}

fn draw_help(frame: &mut Frame, body: Rect, app: &App, at: u16) {
    let lines = help_lines(app.tab, herdr_keys());
    let inner = body.height.saturating_sub(2);
    let max = (lines.len() as u16).saturating_sub(inner);
    app.help_max.set(max);
    frame.render_widget(Clear, body);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((at.min(max), 0))
            .block(Block::new().borders(Borders::ALL).title(" help ")),
        body,
    );
}

/// The fixed plan: what goes, what stays, and every refusal.
pub fn cleanup_lines(modal: &CleanupModal) -> Vec<String> {
    let p = &modal.view.plan;
    let mut lines = vec![
        format!(
            "Remove worktree {} ({})",
            p.checkout_path, p.workspace_label
        ),
        format!(
            "Close workspace {} and its panes: {}",
            p.workspace_id,
            p.panes.join(", ")
        ),
        format!(
            "PR #{} merged · HEAD {}",
            p.pr_number,
            &p.head_sha[..p.head_sha.len().min(8)]
        ),
        if p.delete_branch {
            format!(
                "Delete branch {} with git branch -d  [b: keep it]",
                p.branch
            )
        } else {
            format!(
                "Keep branch {}  [b: delete it with git branch -d]",
                p.branch
            )
        },
        format!("Keep the main checkout {}", p.repo_root),
        String::new(),
    ];
    if modal.view.refusals.is_empty() {
        let state = if modal.can_confirm() {
            "ready — ⏎ confirm"
        } else {
            "type DELETE"
        };
        lines.push(format!(
            "Confirm: {}_   ({state} · Esc cancel)",
            modal.input
        ));
    } else {
        lines.push("Refused:".to_string());
        lines.extend(modal.view.refusals.iter().map(|r| format!("  ✗ {r}")));
        lines.push("Confirm disabled · Esc cancel".to_string());
    }
    lines
}

pub fn launch_lines(modal: &LaunchModal) -> Vec<String> {
    match modal {
        LaunchModal::Input(input) => vec![
            format!("Linear issue ID: {input}_"),
            String::new(),
            "⏎ fetch · Esc cancel".to_string(),
        ],
        LaunchModal::Pick { view, selected } => {
            let i = &view.issue;
            let mut lines = vec![
                format!("{} [{}]  {}", i.identifier, i.state_name(), i.title),
                format!("branch {}", i.branch_name.as_deref().unwrap_or("—")),
                String::new(),
                "Repo:".to_string(),
            ];
            lines.extend(view.repos.iter().enumerate().map(|(n, r)| {
                let mark = if n == *selected { "›" } else { " " };
                format!("{mark} {}", r.label)
            }));
            lines.push(String::new());
            lines.push(if view.start.is_some() {
                "jk pick · ⏎ worktree → claim in Linear → issue prompt · Esc cancel".to_string()
            } else {
                "jk pick · ⏎ worktree + Mastra Code + issue prompt · Esc cancel".to_string()
            });
            lines
        }
    }
}

pub fn reply_lines(reply: &ReplyBox) -> Vec<String> {
    if let Some(why) = &reply.refusal {
        return vec![format!("Can't reply: {why}")];
    }
    let mut lines: Vec<String> = reply.text.split('\n').map(String::from).collect();
    if reply.sending {
        lines.push("sending…".to_string());
    } else if let Some(last) = lines.last_mut() {
        last.push('_');
    }
    lines
}

/// The reply box sits at the bottom of the list, growing up to fit the text.
fn draw_reply(frame: &mut Frame, body: Rect, reply: &ReplyBox) {
    let lines = reply_lines(reply);
    let height = (lines.len() as u16 + 2).min(body.height);
    let area = Rect {
        y: body.y + body.height - height,
        height,
        ..body
    };
    frame.render_widget(Clear, area);
    // Show the end of a long reply: that's where typing happens.
    let text: Vec<Line> = lines
        .iter()
        .skip(
            lines
                .len()
                .saturating_sub(usize::from(height.saturating_sub(2))),
        )
        .map(|l| Line::from(l.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::new()
                .borders(Borders::ALL)
                .title(format!(" Reply to {} ", reply.title)),
        ),
        area,
    );
}

/// The Factory tab: the loaded prompts, and the selected one's text.
fn draw_factory(frame: &mut Frame, app: &App, area: Rect) {
    let f = &app.factory;
    let rows = app.factory_rows();
    let [head, list, text] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(rows.len() as u16 + 1),
        Constraint::Min(1),
    ])
    .areas(area);
    let top = match &f.mention {
        Some(m) => format!("posts to Slack mentioning {m}"),
        None => "SLACK_FACTORY_MENTION not set".to_string(),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(truncate(&top, usize::from(head.width)), dim())),
        head,
    );
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            if let FactoryRow::Draft(d) = row {
                let first = d.text.lines().next().unwrap_or_default();
                return ListItem::new(Line::from(vec![
                    Span::styled("Draft ", fg(theme().accent).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("from {}  ", d.from), dim()),
                    Span::styled(first.to_string(), fg(theme().text)),
                ]));
            }
            let (name, note) = match row {
                FactoryRow::Draft(_) => unreachable!("drawn above"),
                FactoryRow::Message => ("Message", "  type anything"),
                FactoryRow::Prompt(p) if p.needs_ref() => (p.name.as_str(), "  asks for a URL"),
                FactoryRow::Prompt(p) => (p.name.as_str(), ""),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    name.to_string(),
                    fg(theme().text).add_modifier(Modifier::BOLD),
                ),
                Span::styled(note, dim()),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.factory_selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(selected_style())
            .highlight_symbol("› "),
        list,
        &mut state,
    );
    if let Some(row) = rows.get(app.factory_selected) {
        let preview = match row {
            FactoryRow::Draft(d) => d.text.as_str(),
            FactoryRow::Message => "Your own words, sent as-is with the mention and signature.",
            FactoryRow::Prompt(p) => p.text.as_str(),
        };
        frame.render_widget(
            Paragraph::new(preview.to_string())
                .style(fg(theme().subtext))
                .wrap(Wrap { trim: false }),
            text,
        );
    }
}

fn draw_factory_input(frame: &mut Frame, body: Rect, input: &FactoryInput) {
    let Some(prompt) = &input.prompt else {
        let title = match &input.about {
            Some((pr, true)) => format!(" prompt for a local agent on #{} ", pr.number),
            Some((pr, false)) => format!(" message to Factory about #{} ", pr.number),
            None => " message to Factory ".to_string(),
        };
        return draw_factory_message(frame, body, &input.text, &title);
    };
    let height = 3.min(body.height);
    let area = Rect {
        y: body.y + body.height - height,
        height,
        ..body
    };
    let name = &prompt.name;
    frame.render_widget(Clear, area);
    // Show the end of a long URL: that's where typing happens.
    let room = usize::from(area.width.saturating_sub(3));
    let typed: Vec<char> = input.text.chars().collect();
    let shown: String = typed[typed.len().saturating_sub(room)..].iter().collect();
    frame.render_widget(
        Paragraph::new(format!("{shown}_")).block(
            Block::new()
                .borders(Borders::ALL)
                .title(format!(" {name}: URL ")),
        ),
        area,
    );
}

/// The free-form box grows up from the bottom like the reply box.
fn draw_factory_message(frame: &mut Frame, body: Rect, text: &str, title: &str) {
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    if let Some(last) = lines.last_mut() {
        last.push('_');
    }
    let height = (lines.len() as u16 + 2).max(5).min(body.height);
    let area = Rect {
        y: body.y + body.height - height,
        height,
        ..body
    };
    frame.render_widget(Clear, area);
    let shown: Vec<Line> = lines
        .iter()
        .skip(
            lines
                .len()
                .saturating_sub(usize::from(height.saturating_sub(2))),
        )
        .map(|l| Line::from(l.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(shown)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::ALL).title(title.to_string())),
        area,
    );
}

pub fn pr_pick_lines(app: &App, pick: &PrPick) -> Vec<String> {
    let prompts = app.factory.pr_prompts();
    let names = prompts
        .iter()
        .map(|p| p.name.as_str())
        .chain(["Write a message"]);
    names
        .enumerate()
        .map(|(i, name)| {
            let cursor = if i == pick.selected { "›" } else { " " };
            format!("{cursor} {name}")
        })
        .collect()
}

fn draw_pr_pick(frame: &mut Frame, body: Rect, app: &App, pick: &PrPick) {
    let lines = pr_pick_lines(app, pick);
    let height = (lines.len() as u16 + 2).min(body.height);
    let area = Rect {
        y: body.y + body.height - height,
        height,
        ..body
    };
    let title = if pick.local {
        format!(" local agent on #{} ", pick.pr.number)
    } else {
        format!(" factory (slack) · #{} ", pick.pr.number)
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .block(Block::new().borders(Borders::ALL).title(title)),
        area,
    );
}

pub fn factory_confirm_lines(confirm: &FactoryConfirm) -> Vec<String> {
    let mut lines = confirm.message.preview();
    lines.push(String::new());
    lines.push(match &confirm.local {
        Some(pr) => format!("y start an agent on #{} · n cancel", pr.number),
        None => "y post to Slack · n cancel".to_string(),
    });
    lines
}

fn draw_factory_confirm(frame: &mut Frame, area: Rect, confirm: &FactoryConfirm) {
    frame.render_widget(Clear, area);
    let text: Vec<Line> = factory_confirm_lines(confirm)
        .into_iter()
        .map(Line::from)
        .collect();
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((confirm.scroll, 0))
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .title(match &confirm.local {
                        Some(pr) => format!(" local agent on #{} → ", pr.number),
                        None => " factory (slack) → ".to_string(),
                    }),
            ),
        area,
    );
}

fn draw_launch(frame: &mut Frame, area: Rect, modal: &LaunchModal) {
    frame.render_widget(Clear, area);
    let text: Vec<Line> = launch_lines(modal).into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" Launch Linear issue "),
        ),
        area,
    );
}

fn draw_cleanup(frame: &mut Frame, area: Rect, modal: &CleanupModal) {
    frame.render_widget(Clear, area);
    let text: Vec<Line> = cleanup_lines(modal).into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::new()
                .borders(Borders::ALL)
                .title(" Clean up merged worktree "),
        ),
        area,
    );
}

/// The daemon is free to start at most this often from one Inbox.
const ENSURE_EVERY: Duration = Duration::from_secs(10);
/// How often the Inbox looks at the state file and the daemon lock.
const VIEW_POLL: Duration = Duration::from_millis(500);

/// Judges the daemon by its lock, and its state's age only to say it is busy.
#[derive(Debug, Default)]
pub struct Liveness {
    last_ensure: Option<Instant>,
}

impl Liveness {
    /// The footer status, and whether to start a daemon now.
    pub fn judge(
        &mut self,
        lock_held: bool,
        snapshot: Option<&Snapshot>,
        now: Instant,
        now_ms: u64,
    ) -> (WatcherStatus, bool) {
        if !lock_held {
            let ensure = self
                .last_ensure
                .is_none_or(|at| now.duration_since(at) >= ENSURE_EVERY);
            if ensure {
                self.last_ensure = Some(now);
            }
            return (WatcherStatus::Starting, ensure);
        }
        let Some(s) = snapshot else {
            return (WatcherStatus::Waiting, false);
        };
        let age_ms = now_ms.saturating_sub(s.ticked_unix_ms);
        if age_ms > (3 * s.poll_ms).max(30_000) {
            (WatcherStatus::Busy(age_ms / 1000), false)
        } else {
            (WatcherStatus::Running, false)
        }
    }
}

/// The daemon's snapshot plus this Inbox's dialog state.
pub fn compose<R: Runner>(
    snapshot: Option<&Snapshot>,
    watcher: WatcherStatus,
    actions: &Actions<R>,
) -> WatchState {
    let s = snapshot.cloned().unwrap_or_default();
    WatchState {
        agents: s.agents,
        merged: s.merged,
        ticked_unix_ms: snapshot.map(|s| s.ticked_unix_ms),
        watcher,
        stall_secs: snapshot.map(|s| s.stall_secs),
        error: actions.last_error.clone().or(s.error),
        cleanup: actions.cleanup.clone(),
        notice: actions.notice.clone(),
        issues: s.issues,
        linear_missing_key: s.linear_missing_key,
        launch: actions.launch.clone(),
        launching: false,
        fetched: actions.fetched.clone(),
        replied: actions.replied,
        factory_sent: actions.factory_sent,
        prs: actions.prs.clone(),
        pr_detail: actions.pr_detail.clone(),
        issue_list: actions.issue_list.clone(),
        issue_detail: actions.issue_detail.clone(),
        states: actions.states.clone(),
    }
}

/// Where the Inbox worker reads the daemon's state and pokes it.
pub struct ViewerIo {
    pub snapshot: Box<dyn FnMut() -> Option<Snapshot> + Send>,
    pub lock_held: Box<dyn Fn() -> bool + Send>,
    pub ensure: Box<dyn FnMut() + Send>,
    pub refresh: Box<dyn FnMut() + Send>,
}

/// Re-reads the state file only when its mtime or size changed.
fn state_reader(path: PathBuf) -> impl FnMut() -> Option<Snapshot> + Send {
    let mut seen: Option<(SystemTime, u64)> = None;
    let mut cached: Option<Snapshot> = None;
    move || {
        let key = std::fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if key != seen {
            seen = key;
            cached = key
                .and_then(|_| daemon::read_state(&path))
                .map(|s| s.snapshot);
        }
        cached.clone()
    }
}

/// The Inbox worker: views the daemon's state every 500 ms and runs commands.
/// Every Runner call happens here, never on the UI thread.
/// `writing` is true while a command that changes something runs, so quitting
/// waits for it and never for a read.
pub fn run_worker<R: Runner>(
    mut actions: Actions<R>,
    mut io: ViewerIo,
    tx: Sender<WatchState>,
    commands: Receiver<Command>,
    writing: Arc<AtomicBool>,
) {
    let mut liveness = Liveness::default();
    let mut last: Option<WatchState> = None;
    loop {
        let snapshot = (io.snapshot)();
        let (status, ensure) = liveness.judge(
            (io.lock_held)(),
            snapshot.as_ref(),
            Instant::now(),
            unix_ms(),
        );
        if ensure {
            (io.ensure)();
        }
        let state = compose(snapshot.as_ref(), status, &actions);
        if last.as_ref() != Some(&state) {
            if tx.send(state.clone()).is_err() {
                return;
            }
            last = Some(state);
        }
        // The UI drops its sender on quit, which wakes this at once.
        match commands.recv_timeout(VIEW_POLL) {
            Ok(Command::Refresh) => (io.refresh)(),
            Ok(cmd) => {
                if matches!(cmd, Command::Launch { .. }) {
                    // A launch can take a minute; show it is in flight.
                    let mut busy = compose(snapshot.as_ref(), status, &actions);
                    busy.launching = true;
                    busy.notice = Some("launching…".into());
                    let _ = tx.send(busy);
                }
                let writes = !cmd.read_only();
                writing.store(writes, Ordering::SeqCst);
                // Pairs with quit's `stop` then `writing` (both SeqCst): either quit
                // sees this write and waits for it, or this sees `stop` and skips it.
                if writes && actions.stopped() {
                    return;
                }
                actions.handle(cmd);
                writing.store(false, Ordering::SeqCst);
                // Answer at once so a dialog never waits for the next poll.
                let state = compose(snapshot.as_ref(), status, &actions);
                if tx.send(state.clone()).is_err() {
                    return;
                }
                last = Some(state);
            }
            // Fetch only when no command waited, so a keypress never queues behind it.
            Err(RecvTimeoutError::Timeout) if actions.prs_due(Instant::now()) => {
                actions.start_prs_fetch();
                let _ = tx.send(compose(snapshot.as_ref(), status, &actions));
                actions.fetch_prs();
                let state = compose(snapshot.as_ref(), status, &actions);
                if tx.send(state.clone()).is_err() {
                    return;
                }
                last = Some(state);
            }
            Err(RecvTimeoutError::Timeout) if actions.issues_due(Instant::now()) => {
                actions.start_issues_fetch();
                let _ = tx.send(compose(snapshot.as_ref(), status, &actions));
                actions.fetch_issues();
                let state = compose(snapshot.as_ref(), status, &actions);
                if tx.send(state.clone()).is_err() {
                    return;
                }
                last = Some(state);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Views the daemon for `paths`' session; quitting leaves the daemon and its
/// sidebar tokens running.
pub fn run_inbox(paths: Paths) -> Result<()> {
    let slack = Slack::load();
    let stop = Arc::new(AtomicBool::new(false));
    // Act on the session whose state is on screen, never an ambient one.
    let actions = Actions::new(
        Herdr::new(RealRunner::default()).for_session(paths.session()),
        stop.clone(),
    )
    .with_linear_key(linear::load_key())
    .with_slack(slack.clone())
    .with_pr_cache(paths.prs())
    .with_issue_cache(paths.issues());
    let io = {
        let (lock, refresh) = (paths.lock(), paths.refresh());
        let session = paths.session().to_string();
        ViewerIo {
            snapshot: Box::new(state_reader(paths.state())),
            lock_held: Box::new(move || daemon::lock_held(&lock)),
            // A failure shows as the footer staying on "starting…".
            ensure: Box::new(move || {
                let _ = daemon::ensure_current(&session);
            }),
            refresh: Box::new(move || {
                let _ = daemon::bump_generation(&refresh);
            }),
        }
    };
    let (tx, rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let writing = Arc::new(AtomicBool::new(false));
    let worker = {
        let writing = writing.clone();
        thread::spawn(move || run_worker(actions, io, tx, cmd_rx, writing))
    };

    let mut terminal = ratatui::init();
    // A paste arrives as one event, so its newlines and letters are text, not keys.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    let mut app = App::new().with_factory(Factory::load(), slack.is_some());
    if let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR") {
        app = app.with_drafts(drafts::dir(std::path::Path::new(&dir)));
    }
    let result = (|| -> Result<()> {
        // Redraw on new state, input, resize, or when the footer's seconds tick over.
        let mut dirty = true;
        let mut drawn_at = Instant::now();
        let mut drafts_at = Instant::now();
        loop {
            while let Ok(state) = rx.try_recv() {
                app.apply(state);
                dirty = true;
            }
            if drafts_at.elapsed() >= Duration::from_secs(1) {
                dirty |= app.load_drafts();
                drafts_at = Instant::now();
            }
            if dirty || drawn_at.elapsed() >= Duration::from_secs(1) {
                terminal.draw(|f| draw(f, &app, unix_ms()))?;
                dirty = false;
                drawn_at = Instant::now();
            }
            if event::poll(Duration::from_millis(200))? {
                dirty = true;
                match event::read()? {
                    Event::Paste(text) => app.on_paste(&text),
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if app.quits(key) {
                            return Ok(());
                        }
                        if let Some(cmd) = app.on_key_event(key) {
                            let _ = cmd_tx.send(cmd);
                        }
                    }
                    _ => {}
                }
            }
        }
    })();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    // Quitting only closes this viewer: the daemon keeps the tokens current.
    // A running command stops before its next Runner call.
    stop.store(true, Ordering::SeqCst);
    // Dropping the sender wakes an idle worker at once instead of after its poll.
    drop(cmd_tx);
    // Wait out a write (a half-done start or draft toggle would surprise);
    // a read in flight is abandoned so `q` is instant. A write picked up after
    // this check sees `stop` and never starts (see `run_worker`).
    if writing.load(Ordering::SeqCst) || worker.is_finished() {
        let _ = worker.join();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::herdr::AgentInfo;
    use crate::core::runner::FakeRunner;
    use crate::features::cleanup::{CleanupPlan, Refusal};
    use crate::features::inbox::model::InboxItem;
    use crate::features::inbox::watcher::{IssueView, StatesView};
    use crate::features::launch::issue;
    use crate::integrations::github::{Ci, PrInfo, PrState};
    use crate::integrations::mastracode::Recap;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn agent(pane: &str, label: &str, status: &str, title: &str) -> AgentSnapshot {
        let info: AgentInfo = serde_json::from_value(serde_json::json!({
            "pane_id": pane, "workspace_id": "w1", "tab_id": format!("{pane}-t"),
            "agent": "mastracode", "agent_status": status,
            "terminal_title_stripped": title, "state_change_seq": 1
        }))
        .unwrap();
        AgentSnapshot {
            info,
            workspace_label: label.into(),
            branch: None,
            pr: None,
            stalled: None,
            asking: false,
            thread: None,
            recap: None,
            question: None,
            stall_hash: None,
            state_since_unix_ms: 0,
        }
    }

    fn app() -> App {
        let mut stalled = agent("a", "alpha", "working", "fix ci");
        stalled.stalled = Some(Duration::from_secs(360));
        stalled.pr = Some(PrInfo {
            number: 12,
            state: PrState::Open,
            url: "https://github.com/o/r/pull/12".into(),
            head_oid: String::new(),
            ci: Ci::Fail,
            is_draft: false,
            review_decision: None,
            mergeable: None,
        });
        let mut asking = agent("c", "gamma", "idle", "question");
        asking.asking = true;
        let mut app = App::new();
        app.state = WatchState {
            agents: vec![
                agent("d", "delta", "done", "finished"),
                stalled,
                agent("q", "quiet", "working", "busy"),
                asking,
            ],
            ticked_unix_ms: Some(unix_ms()),
            watcher: WatcherStatus::Running,
            stall_secs: Some(300),
            error: Some("gh: not logged in".into()),
            ..Default::default()
        };
        app
    }

    fn render(app: &App) -> Vec<String> {
        render_h(app, 8)
    }

    fn render_h(app: &App, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
        terminal.draw(|f| draw(f, app, unix_ms())).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn recap(lines: &[&str], stale: bool) -> Recap {
        Recap {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            tasks_done: 2,
            tasks_total: 5,
            goal: Some("Ship the agents tab".into()),
            observed_unix_ms: Some(unix_ms() - 20 * 60_000),
            stale,
            ..Default::default()
        }
    }

    #[test]
    fn a_live_now_line_and_goal_progress_replace_the_memory_recap() {
        let mut app = app();
        let mut r = recap(&["✓ (10:02) wrote the parser"], false);
        r.now = Some("I'm checking whether the dashboard has to be saved.".into());
        r.goal_runs = Some((2, 30));
        r.judge = Some("Phase 1 verified as passing".into());
        app.state.agents[0].recap = Some(r);
        app.apply(app.state.clone());
        let lines = render_h(&app, 24);
        assert!(lines[8].starts_with("  ✓ finished"), "{lines:?}");
        assert_eq!(
            lines[9].trim(),
            "now  I'm checking whether the dashboard has to be saved."
        );
        assert_eq!(lines[10].trim(), "goal 2/30 · Phase 1 verified as passing");
        assert!(
            !lines.iter().any(|l| l.contains("wrote the parser")),
            "{lines:?}"
        );
        app.filter = "dashboard".into();
        app.apply(app.state.clone());
        let lines = render_h(&app, 24);
        assert!(
            lines.iter().any(|l| l.contains("finished"))
                && !lines.iter().any(|l| l.contains("busy")),
            "now is searchable: {lines:?}"
        );
    }

    #[test]
    fn renders_agents_by_stage_with_question_recap_and_meta() {
        let mut app = app();
        app.state.agents[3].question = Some("Pick a color".into());
        app.state.agents[0].recap = Some(recap(&["✓ (10:02) wrote the parser"], false));
        app.state.agents[0].branch = Some("user/eng-325-widget".into());
        app.state.agents[0].state_since_unix_ms = unix_ms() - 6 * 60_000;
        app.state.issues = [("ENG-325".to_string(), linear_issue())].into();
        app.apply(app.state.clone());
        let lines = render_h(&app, 24);
        assert!(lines[0].contains("1 Agents") && lines[0].contains("2 PRs"));
        assert!(!lines[0].contains("Today"), "{lines:?}");
        assert!(
            lines[1].starts_with("All 4 · Needs fixing 1 · Needs you 1 · Done 1 · Working 1"),
            "{lines:?}"
        );
        assert!(lines[2].starts_with("› ⏸ fix ci"), "{lines:?}");
        assert_eq!(lines[4].trim(), "#12 ✗");
        assert!(lines[5].starts_with("  ? question"), "{lines:?}");
        assert!(lines[5].trim_end().ends_with("gamma"), "{lines:?}");
        assert_eq!(lines[6].trim(), "? Pick a color");
        assert_eq!(lines[7].trim(), "no recap");
        assert!(lines[8].starts_with("  ✓ finished"), "{lines:?}");
        assert!(lines[8].trim_end().ends_with("delta · 6m"), "{lines:?}");
        assert_eq!(lines[9].trim(), "✓ (10:02) wrote the parser");
        assert_eq!(lines[10].trim(), "ENG-325 Done · 2/5 tasks");
        assert!(lines[11].starts_with("  ● busy"), "{lines:?}");
        assert!(lines[22].contains("gh: not logged in"));
        assert!(lines[23].contains("q quit"), "keys keep their own line");
    }

    #[test]
    fn a_stale_recap_says_how_old_it_is_and_the_hint_shows_the_goal() {
        let mut app = app();
        app.state.agents[3].recap = Some(recap(&["• (09:10) a", "• (09:20) b", "✓ c"], true));
        app.apply(app.state.clone());
        app.on_key(KeyCode::Char('j'));
        let lines = render_h(&app, 24);
        assert_eq!(lines[6].trim(), "? waiting for an answer");
        assert_eq!(lines[7].trim(), "recap from 20m ago");
        assert_eq!(lines[8].trim(), "• (09:10) a");
        assert_eq!(lines[9].trim(), "• (09:20) b", "two recap lines at most");
        assert_eq!(lines[10].trim(), "2/5 tasks");
        assert_eq!(lines[21].trim(), "goal: Ship the agents tab");
        app.state.agents[3].recap.as_mut().unwrap().partial = true;
        app.apply(app.state.clone());
        let partial = render_h(&app, 24);
        assert_eq!(partial[8].trim(), "… • (09:10) a", "a cut window says so");
        assert_eq!(partial[9].trim(), "• (09:20) b");
        app.state.agents[3].recap.as_mut().unwrap().partial = false;
        app.state.agents[3].recap.as_mut().unwrap().goal = None;
        app.state.agents[3].recap.as_mut().unwrap().active_task = Some("Writing tests".into());
        assert_eq!(render_h(&app, 24)[21].trim(), "doing: Writing tests");
    }

    #[test]
    fn keys_move_focus_and_open_pr() {
        let mut app = app();
        // Rows: a (needs fixing), c (needs you), d (done), q (working).
        assert_eq!(app.on_key(KeyCode::Enter), Some(Command::Focus("a".into())));
        assert_eq!(
            app.on_key(KeyCode::Char('o')),
            Some(Command::OpenUrl("https://github.com/o/r/pull/12".into()))
        );
        app.on_key(KeyCode::Char('k'));
        assert_eq!(app.selected, 0, "k clamps at the top");
        app.on_key(KeyCode::Char('j'));
        app.on_key(KeyCode::Down);
        assert_eq!(app.on_key(KeyCode::Enter), Some(Command::Focus("d".into())));
        app.on_key(KeyCode::Down);
        app.on_key(KeyCode::Down);
        assert_eq!(app.selected, 3, "j clamps at the bottom");
        assert_eq!(app.on_key(KeyCode::Char('o')), None, "no PR");
        app.on_key(KeyCode::Up);
        assert_eq!(app.selected, 2);
        assert_eq!(app.on_key(KeyCode::Char('c')), None, "not a worktree row");
        assert_eq!(app.on_key(KeyCode::Char('r')), Some(Command::Refresh));
    }

    #[test]
    fn the_marker_follows_the_selection_once_per_row() {
        let mut app = app();
        app.on_key(KeyCode::Char('j'));
        app.on_key(KeyCode::Char('j'));
        let lines = render_h(&app, 24);
        let marked: Vec<&String> = lines.iter().filter(|l| l.starts_with("› ")).collect();
        assert_eq!(marked.len(), 1, "{lines:?}");
        assert!(marked[0].starts_with("› ✓ finished"), "{lines:?}");
        assert!(
            lines[9].starts_with("    no recap"),
            "second line padded: {lines:?}"
        );
    }

    #[test]
    fn the_selection_follows_its_row_when_rows_move() {
        let mut app = app();
        app.on_key(KeyCode::Char('j'));
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.on_key(KeyCode::Enter), Some(Command::Focus("d".into())));
        // a is fixed and back at work, so d moves up one row.
        let mut state = app.state.clone();
        state.agents[1].stalled = None;
        state.agents[1].pr = None;
        app.apply(state);
        assert_eq!(app.selected, 1);
        assert_eq!(app.on_key(KeyCode::Enter), Some(Command::Focus("d".into())));
        // d goes away: the selection clamps onto a row.
        let mut state = app.state.clone();
        state.agents.retain(|x| x.info.pane_id != "d");
        app.apply(state);
        assert_eq!(app.selected, 1);
        assert!(app.on_key(KeyCode::Enter).is_some());
    }

    #[test]
    fn stage_tabs_cycle_through_non_empty_stages_and_esc_returns_to_all() {
        let mut app = app();
        app.on_key(KeyCode::Right);
        assert_eq!(app.stage, Some(Stage::NeedsFixing));
        let lines = render_h(&app, 24);
        assert!(lines[2].starts_with("› ⏸ fix ci"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("busy")), "one stage only");
        assert_eq!(app.on_key(KeyCode::Enter), Some(Command::Focus("a".into())));
        app.on_key(KeyCode::Right);
        assert_eq!(app.stage, Some(Stage::NeedsYou));
        app.on_key(KeyCode::Right);
        assert_eq!(app.stage, Some(Stage::Done));
        app.on_key(KeyCode::Right);
        assert_eq!(app.stage, Some(Stage::Working), "Idle is empty: skipped");
        app.on_key(KeyCode::Right);
        assert_eq!(app.stage, None, "wraps to All");
        app.on_key(KeyCode::Left);
        assert_eq!(app.stage, Some(Stage::Working));
        app.on_key(KeyCode::Esc);
        assert_eq!(app.stage, None);
    }

    #[test]
    fn question_mark_shows_the_tabs_help_and_closes_without_quitting() {
        let mut app = app();
        assert_eq!(app.on_key(KeyCode::Char('?')), None);
        assert!(app.has_modal(), "q closes the help, not the Inbox");
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.quits(q));
        let lines = render_h(&app, 40);
        let screen = lines.join("\n");
        assert!(lines[1].contains(" help "), "{lines:?}");
        assert!(lines[2].contains("Agents"), "{lines:?}");
        for want in ["⏸", "reply to the agent", "Everywhere", "this help"] {
            assert!(screen.contains(want), "{want}: {lines:?}");
        }
        assert!(lines[39].contains("esc close"), "{lines:?}");
        // Keys go to the help, not the list behind it.
        assert_eq!(app.on_key(KeyCode::Enter), None);
        assert_eq!(app.on_key_event(q), None);
        assert!(app.help.is_none());
        // Each tab explains itself.
        for (key, want) in [
            ('2', "rerun the failed"),
            ('3', "mark or unmark"),
            ('4', "only goes out on y"),
        ] {
            app.on_key(KeyCode::Char(key));
            app.on_key(KeyCode::Char('?'));
            assert!(render_h(&app, 40).join("\n").contains(want), "{want}");
            app.on_key(KeyCode::Esc);
            assert!(app.help.is_none());
        }
        app.on_key(KeyCode::Char('?'));
        app.on_key(KeyCode::Char('?'));
        assert!(app.help.is_none(), "? toggles");
    }

    #[test]
    fn help_scrolls_within_its_lines_and_fits_the_width() {
        for tab in Tab::ALL {
            for line in help_lines(tab, &[]) {
                assert!(line.width() <= 98, "{tab:?}: {line}");
            }
        }
        let mut app = app();
        app.on_key(KeyCode::Char('?'));
        render_h(&app, 12);
        for _ in 0..100 {
            app.on_key(KeyCode::Char('j'));
        }
        let max = app.help.unwrap();
        assert_eq!(
            usize::from(max),
            help_lines(Tab::Agents, &[]).len() - (12 - 4 - 2)
        );
        assert!(
            render_h(&app, 12)[7].contains("see the README"),
            "the last line shows"
        );
        app.on_key(KeyCode::Char('k'));
        assert_eq!(app.help, Some(max - 1));
    }

    #[test]
    fn help_lists_the_herdr_keys_bound_to_werkbank() {
        let text = |lines: Vec<Line>| {
            lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let none = text(help_lines(Tab::Prs, &[]));
        assert!(
            none.contains("In Herdr\nNo Werkbank actions are bound"),
            "{none}"
        );
        let keys = vec![
            ("ctrl+b i".to_string(), "open the Inbox".to_string()),
            (
                "alt+y".to_string(),
                "new worktree with Mastra Code".to_string(),
            ),
        ];
        let shown = text(help_lines(Tab::Issues, &keys));
        assert!(
            shown.ends_with(
                "In Herdr\n ctrl+b i  open the Inbox\n alt+y     new worktree with Mastra Code"
            ),
            "{shown}"
        );
    }

    #[test]
    fn question_mark_types_into_the_filter() {
        let mut app = app();
        app.on_key(KeyCode::Char('/'));
        type_text(&mut app, "?");
        assert_eq!(app.filter, "?");
        assert!(app.help.is_none());
    }

    #[test]
    fn filter_matches_every_word_and_q_types_into_it() {
        let mut app = app();
        app.state.agents[1].recap = Some(recap(&["• (10:00) flaky snapshot test"], false));
        app.apply(app.state.clone());
        app.on_key(KeyCode::Char('/'));
        assert!(app.has_modal(), "q must not quit while filtering");
        type_text(&mut app, "q");
        assert_eq!(app.filter, "q");
        app.on_key(KeyCode::Backspace);
        type_text(&mut app, "FLAKY alpha");
        assert_eq!(app.rows().len(), 1);
        let lines = render_h(&app, 24);
        assert!(lines[1].starts_with("All 1 · Needs fixing 1"), "{lines:?}");
        assert_eq!(lines[2].trim(), "/ FLAKY alpha_");
        assert!(lines[3].starts_with("› ⏸ fix ci"), "{lines:?}");
        assert!(lines[23].contains("keep filter"), "{lines:?}");
        // Enter keeps the filter and gives the keys back.
        assert_eq!(app.on_key(KeyCode::Enter), None);
        assert!(!app.has_modal());
        assert_eq!(app.on_key(KeyCode::Enter), Some(Command::Focus("a".into())));
        assert_eq!(render_h(&app, 24)[2].trim(), "/ FLAKY alpha");
        // Other fields match too; no match says so.
        app.on_key(KeyCode::Char('/'));
        type_text(&mut app, " nothing-like-this");
        assert!(render_h(&app, 24)[3].contains("no agents match"));
        // Esc clears it.
        app.on_key(KeyCode::Esc);
        assert_eq!((app.filter.as_str(), app.filter_open), ("", false));
        assert_eq!(app.rows().len(), 4);
        for (word, n) in [("#12", 1), ("needs", 2), ("gamma", 1), ("working", 1)] {
            app.filter = word.into();
            assert_eq!(app.rows().len(), n, "{word}");
        }
    }

    #[test]
    fn rows_fit_a_40_column_pane() {
        let mut app = app();
        app.state.agents[1].workspace_label = "user-eng-622-widget-sync-loops".into();
        app.state.agents[1].info.terminal_title_stripped =
            Some("Mastra Code - Fix the flaky CI job that keeps timing out".into());
        app.state.agents[1].state_since_unix_ms = unix_ms() - 3 * 3_600_000;
        app.state.agents[1].recap = Some(recap(
            &["• (10:00) a very long recap line that cannot fit"],
            false,
        ));
        app.apply(app.state.clone());
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        terminal.draw(|f| draw(f, &app, unix_ms())).unwrap();
        let buf = terminal.backend().buffer();
        let lines: Vec<String> = (0..24)
            .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let row = lines.iter().position(|l| l.starts_with("› ⏸")).unwrap();
        assert_eq!(lines[row], "› ⏸ Fix t… user-eng-622-widget-syn… · 3h");
        assert_eq!(lines[row + 1], "    • (10:00) a very long recap line th…");
    }

    #[test]
    fn a_narrow_status_line_shows_the_error_first_and_ends_in_an_ellipsis() {
        let mut app = app();
        app.state.error = Some("gh: rate limited".into());
        app.state.notice = Some("a long notice that will never fit in forty columns".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| draw(f, &app, unix_ms())).unwrap();
        let buf = terminal.backend().buffer();
        let status: String = (0..40).map(|x| buf[(x, 10)].symbol()).collect();
        assert!(status.contains("gh: rate limited"), "{status}");
        assert!(status.trim_end().ends_with('…'), "{status}");
    }

    #[test]
    fn top_tabs_switch_with_numbers_and_brackets_and_keep_the_selection() {
        let mut app = app();
        app.on_key(KeyCode::Char('j'));
        app.on_key(KeyCode::Char('2'));
        assert_eq!(app.tab, Tab::Prs);
        assert_eq!(app.on_key(KeyCode::Enter), None, "agent keys are off");
        assert!(render_h(&app, 24)[1].contains("pull requests"));
        app.on_key(KeyCode::Char(']'));
        assert_eq!(app.tab, Tab::Issues);
        app.on_key(KeyCode::Char(']'));
        assert_eq!(app.tab, Tab::Factory);
        app.on_key(KeyCode::Char(']'));
        assert_eq!(app.tab, Tab::Agents);
        app.on_key(KeyCode::Char('['));
        assert_eq!(app.tab, Tab::Factory);
        app.on_key(KeyCode::Char('['));
        assert_eq!(app.tab, Tab::Issues);
        assert_eq!(
            app.on_key(KeyCode::Char('4')),
            Some(Command::TabVisible(Tab::Factory))
        );
        app.on_key(KeyCode::Char('1'));
        assert_eq!(app.on_key(KeyCode::Enter), Some(Command::Focus("c".into())));
        assert_eq!(app.on_key(KeyCode::Tab), None, "Tab no longer switches");
        assert_eq!(app.tab, Tab::Agents);
    }

    #[test]
    fn a_linear_key_notice_and_an_empty_list_render() {
        let mut app = App::new();
        app.apply(WatchState {
            linear_missing_key: true,
            ticked_unix_ms: Some(unix_ms()),
            watcher: WatcherStatus::Running,
            ..Default::default()
        });
        let text = render_h(&app, 12).join("\n");
        assert!(text.contains("no Mastra Code agents"), "{text}");
        assert!(
            text.contains("LINEAR_API_KEY not set — see README"),
            "{text}"
        );
        assert_eq!(app.on_key(KeyCode::Enter), None);
        assert_eq!(app.on_key(KeyCode::Char('j')), None);
    }

    fn merged_pr() -> PrInfo {
        PrInfo {
            number: 42,
            state: PrState::Merged,
            url: "https://github.com/o/r/pull/42".into(),
            head_oid: "543b8ddc".into(),
            ci: Ci::Pass,
            is_draft: false,
            review_decision: None,
            mergeable: None,
        }
    }

    fn view(serial: u64, refusals: Vec<Refusal>) -> CleanupView {
        CleanupView {
            serial,
            plan: CleanupPlan {
                workspace_id: "w2".into(),
                workspace_label: "feat-x".into(),
                checkout_path: "/wt/feat-x".into(),
                repo_root: "/repo".into(),
                branch: "feat/x".into(),
                head_sha: "543b8ddc19ae".into(),
                pr_number: 42,
                panes: vec!["w2:p1".into()],
                delete_branch: false,
            },
            refusals,
        }
    }

    fn with_merged() -> App {
        let mut app = App::new();
        app.apply(WatchState {
            agents: vec![agent("d", "delta", "done", "finished")],
            merged: vec![InboxItem::merged(
                "w2",
                "feat-x",
                "/wt/feat-x",
                "feat/x",
                merged_pr(),
            )],
            ticked_unix_ms: Some(unix_ms()),
            watcher: WatcherStatus::Running,
            stall_secs: Some(300),
            ..Default::default()
        });
        app
    }

    #[test]
    fn merged_row_renders_last_and_c_plans_only_merged_rows() {
        let mut app = with_merged();
        let lines = render_h(&app, 14);
        assert!(
            lines[1].starts_with("All 2 · Done 1 · Merged 1"),
            "{lines:?}"
        );
        assert!(lines[2].starts_with("› ✓ finished"), "{lines:?}");
        assert!(lines[4].starts_with("  ⇣ feat-x"), "{lines:?}");
        assert!(lines[4].trim_end().ends_with("no agent"), "{lines:?}");
        assert_eq!(lines[5].trim(), "#42 ⇣  feat/x  c cleanup  /wt/feat-x");
        assert_eq!(app.on_key(KeyCode::Char('c')), None, "not on a merged row");
        app.on_key(KeyCode::Char('j'));
        assert_eq!(app.on_key(KeyCode::Enter), None, "merged rows have no pane");
        assert_eq!(
            app.on_key(KeyCode::Char('o')),
            Some(Command::OpenUrl("https://github.com/o/r/pull/42".into()))
        );
        assert_eq!(
            app.on_key(KeyCode::Char('c')),
            Some(Command::PlanCleanup("w2".into()))
        );
        let lines = render_h(&app, 14);
        assert!(lines[12].contains("working… planning cleanup of feat-x"));
        assert!(
            lines[13].contains("c cleanup"),
            "a notice never hides the keys"
        );
    }

    fn open_modal(refusals: Vec<Refusal>) -> App {
        let mut app = with_merged();
        let mut state = app.state.clone();
        state.cleanup = Some(view(1, refusals));
        app.apply(state);
        assert!(app.modal.is_some());
        app
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(KeyCode::Char(c));
        }
    }

    #[test]
    fn confirm_stays_disabled_until_delete_is_typed_exactly() {
        let mut app = open_modal(vec![]);
        assert_eq!(app.on_key(KeyCode::Enter), None);
        type_text(&mut app, "DELET");
        assert_eq!(app.on_key(KeyCode::Enter), None);
        type_text(&mut app, "EX");
        assert_eq!(app.on_key(KeyCode::Enter), None, "DELETEX is not DELETE");
        app.on_key(KeyCode::Backspace);
        assert_eq!(app.on_key(KeyCode::Char('q')), None, "q is typed, not quit");
        app.on_key(KeyCode::Backspace);
        assert!(app.modal.as_ref().unwrap().can_confirm());
        let lines = render_h(&app, 20).join("\n");
        assert!(lines.contains("Keep branch feat/x"), "{lines}");
        assert!(lines.contains("ready"), "{lines}");
        match app.on_key(KeyCode::Enter) {
            Some(Command::ExecuteCleanup(plan)) => assert!(!plan.delete_branch),
            other => panic!("{other:?}"),
        }
        assert!(app.modal.is_none());
    }

    #[test]
    fn b_toggles_branch_deletion() {
        let mut app = open_modal(vec![]);
        app.on_key(KeyCode::Char('b'));
        assert!(render_h(&app, 20)
            .join("\n")
            .contains("Delete branch feat/x with git branch -d"));
        type_text(&mut app, "DELETE");
        match app.on_key(KeyCode::Enter) {
            Some(Command::ExecuteCleanup(plan)) => assert!(plan.delete_branch),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn confirm_is_disabled_while_there_are_refusals() {
        let mut app = open_modal(vec![Refusal::Dirty(1)]);
        type_text(&mut app, "DELETE");
        assert_eq!(app.modal.as_ref().unwrap().input, "", "input is disabled");
        assert_eq!(app.on_key(KeyCode::Enter), None);
        let lines = render_h(&app, 20).join("\n");
        assert!(lines.contains("Dirty: 1"), "{lines}");
        assert!(lines.contains("Confirm disabled"), "{lines}");
        app.on_key(KeyCode::Esc);
        assert!(app.modal.is_none());
        // The same plan does not reopen on the next tick.
        let state = app.state.clone();
        app.apply(state);
        assert!(app.modal.is_none());
    }

    fn linear_issue() -> linear::Issue {
        serde_json::from_value(serde_json::json!({
            "identifier": "ENG-325", "title": "Widget export", "description": null,
            "url": "https://linear.app/x/issue/ENG-325",
            "branchName": "user/eng-325-widget-export-cli",
            "state": {"name": "Done"}
        }))
        .unwrap()
    }

    #[test]
    fn n_flow_fetches_then_picks_a_repo_then_launches() {
        let mut app = with_merged();
        assert_eq!(app.on_key(KeyCode::Char('n')), None);
        assert!(!app.has_modal(), "n is an Issues key, not an Agents one");
        app.on_key(KeyCode::Char('3'));
        assert_eq!(app.on_key(KeyCode::Char('n')), None);
        assert!(app.has_modal());
        type_text(&mut app, "q eng-325");
        assert_eq!(
            app.on_key(KeyCode::Enter),
            None,
            "not an id is never fetched"
        );
        let mut app2 = with_merged();
        app2.on_key(KeyCode::Char('3'));
        app2.on_key(KeyCode::Char('n'));
        type_text(&mut app2, "eng-325");
        assert_eq!(
            app2.on_key(KeyCode::Enter),
            Some(Command::FetchIssue("ENG-325".into()))
        );
        let repos = vec![
            issue::RepoChoice {
                repo_root: "/r/mastra".into(),
                label: "mastra — mastra".into(),
            },
            issue::RepoChoice {
                repo_root: "/r/docs".into(),
                label: "docs — docs".into(),
            },
        ];
        let mut state = app2.state.clone();
        state.notice = Some("ENG-325: Widget export".into());
        state.launch = Some(LaunchView {
            serial: 1,
            issue: linear_issue(),
            repos,
            start: None,
        });
        app2.apply(state.clone());
        let lines = render_h(&app2, 14).join("\n");
        assert!(lines.contains("ENG-325 [Done]  Widget export"), "{lines}");
        assert!(lines.contains("› mastra — mastra"), "{lines}");
        app2.on_key(KeyCode::Char('j'));
        assert_eq!(
            app2.on_key(KeyCode::Enter),
            Some(Command::Launch {
                issue: linear_issue(),
                repo_root: "/r/docs".into(),
            })
        );
        assert!(app2.launch.is_none());
        // The same view arriving again does not reopen the picker.
        app2.apply(state);
        assert!(app2.launch.is_none());
    }

    #[test]
    fn a_cancelled_lookup_never_opens_a_later_dialog() {
        let view = |serial| LaunchView {
            serial,
            issue: linear_issue(),
            repos: vec![issue::RepoChoice {
                repo_root: "/r/mastra".into(),
                label: "mastra — mastra".into(),
            }],
            start: None,
        };
        let mut app = with_merged();
        app.on_key(KeyCode::Char('3'));
        app.on_key(KeyCode::Char('n'));
        type_text(&mut app, "eng-325");
        assert!(app.on_key(KeyCode::Enter).is_some());
        assert_eq!(app.on_key(KeyCode::Enter), None, "no duplicate lookup");
        app.on_key(KeyCode::Esc);
        // A new dialog for another issue; the old lookup's answer lands now.
        app.on_key(KeyCode::Char('3'));
        app.on_key(KeyCode::Char('n'));
        type_text(&mut app, "abc-1");
        let mut state = app.state.clone();
        state.launch = Some(view(1));
        app.apply(state.clone());
        assert_eq!(app.launch, Some(LaunchModal::Input("abc-1".into())));
        // Editing the id after Enter also drops the pending lookup.
        let mut app = with_merged();
        app.on_key(KeyCode::Char('3'));
        app.on_key(KeyCode::Char('n'));
        type_text(&mut app, "eng-325");
        app.on_key(KeyCode::Enter);
        app.on_key(KeyCode::Backspace);
        state.launch = Some(view(2));
        app.apply(state.clone());
        assert_eq!(app.launch, Some(LaunchModal::Input("eng-32".into())));
        // The awaited id opens the picker.
        type_text(&mut app, "5");
        assert!(app.on_key(KeyCode::Enter).is_some());
        state.launch = Some(view(3));
        app.apply(state);
        assert!(matches!(app.launch, Some(LaunchModal::Pick { .. })));
    }

    #[test]
    fn a_failed_lookup_can_be_retried_with_enter() {
        let mut app = with_merged();
        app.on_key(KeyCode::Char('3'));
        app.on_key(KeyCode::Char('n'));
        type_text(&mut app, "eng-325");
        let fetch = Some(Command::FetchIssue("ENG-325".into()));
        assert_eq!(app.on_key(KeyCode::Enter), fetch);
        assert_eq!(app.on_key(KeyCode::Enter), None, "still in flight");
        let mut state = app.state.clone();
        state.notice = Some("linear: network down".into());
        state.fetched = Some((1, "ENG-325".into()));
        app.apply(state.clone());
        assert_eq!(app.launch, Some(LaunchModal::Input("eng-325".into())));
        assert_eq!(app.on_key(KeyCode::Enter), fetch);
        // A repeat of the same finished lookup does not clear the new request.
        app.apply(state);
        assert_eq!(app.on_key(KeyCode::Enter), None);
    }

    #[test]
    fn esc_closes_the_launch_dialog_and_q_is_typed_not_quit() {
        let mut app = with_merged();
        app.on_key(KeyCode::Char('3'));
        app.on_key(KeyCode::Char('n'));
        app.on_key(KeyCode::Char('q'));
        assert_eq!(app.launch, Some(LaunchModal::Input("q".into())));
        assert!(app.has_modal());
        app.on_key(KeyCode::Esc);
        assert!(!app.has_modal());
    }

    fn snap(ticked_unix_ms: u64) -> Snapshot {
        Snapshot {
            ticked_unix_ms,
            stall_secs: 42,
            poll_ms: 2000,
            ..Default::default()
        }
    }

    #[test]
    fn liveness_judges_by_lock_and_ensures_at_most_every_10s() {
        let mut l = Liveness::default();
        let t0 = Instant::now();
        let now_ms = 1_000_000;
        assert_eq!(
            l.judge(false, None, t0, now_ms),
            (WatcherStatus::Starting, true)
        );
        assert_eq!(
            l.judge(false, None, t0 + Duration::from_secs(9), now_ms),
            (WatcherStatus::Starting, false)
        );
        assert_eq!(
            l.judge(false, None, t0 + Duration::from_secs(10), now_ms),
            (WatcherStatus::Starting, true)
        );
        // Held lock: never ensure, whatever the state's age.
        let t = t0 + Duration::from_secs(100);
        assert_eq!(
            l.judge(true, None, t, now_ms),
            (WatcherStatus::Waiting, false)
        );
        let fresh = snap(now_ms - 5_000);
        assert_eq!(
            l.judge(true, Some(&fresh), t, now_ms),
            (WatcherStatus::Running, false)
        );
        let stale = snap(now_ms - 31_000);
        assert_eq!(
            l.judge(true, Some(&stale), t, now_ms),
            (WatcherStatus::Busy(31), false)
        );
    }

    #[test]
    fn footer_shows_the_daemons_stall_threshold_and_status() {
        let actions = Actions::new(
            Herdr::with_bin(FakeRunner::new(), "herdr"),
            Arc::new(AtomicBool::new(false)),
        );
        let now = unix_ms();
        let mut app = App::new();
        app.apply(compose(Some(&snap(now)), WatcherStatus::Running, &actions));
        let text = render(&app).join("\n");
        assert!(text.contains("stall 42s"), "{text}");
        assert!(text.contains("updated 0s ago"), "{text}");
        app.apply(compose(None, WatcherStatus::Starting, &actions));
        let text = render(&app).join("\n");
        assert!(text.contains("watcher not running — starting…"), "{text}");
        assert!(
            !text.contains("stall "),
            "no threshold without a daemon: {text}"
        );
    }

    #[test]
    fn a_write_picked_up_after_quit_never_starts() {
        let stop = Arc::new(AtomicBool::new(false));
        // No canned replies: a started write would fail, not hang.
        let actions = Actions::new(Herdr::with_bin(FakeRunner::new(), "herdr"), stop.clone());
        let io = ViewerIo {
            snapshot: Box::new(|| Some(snap(unix_ms()))),
            lock_held: Box::new(|| true),
            ensure: Box::new(|| {}),
            refresh: Box::new(|| {}),
        };
        let (tx, rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let writing = Arc::new(AtomicBool::new(false));
        let handle = {
            let writing = writing.clone();
            thread::spawn(move || run_worker(actions, io, tx, cmd_rx, writing))
        };
        rx.recv_timeout(Duration::from_secs(5))
            .expect("first state");
        stop.store(true, Ordering::SeqCst);
        cmd_tx
            .send(Command::Reply {
                pane: "w1:p1".into(),
                text: "hi".into(),
                stall_hash: None,
            })
            .unwrap();
        // The worker leaves without handling it, though the sender is still open.
        handle.join().unwrap();
        while let Ok(state) = rx.try_recv() {
            assert!(state.notice.is_none() && state.error.is_none(), "{state:?}");
        }
        drop(cmd_tx);
    }

    #[test]
    fn viewer_worker_wakes_at_once_and_quitting_clears_no_tokens() {
        use std::sync::atomic::AtomicUsize;
        let actions = Actions::new(
            Herdr::with_bin(FakeRunner::new(), "herdr"),
            Arc::new(AtomicBool::new(false)),
        );
        let refreshes = Arc::new(AtomicUsize::new(0));
        let ensures = Arc::new(AtomicUsize::new(0));
        let io = {
            let (r, e) = (refreshes.clone(), ensures.clone());
            ViewerIo {
                snapshot: Box::new(|| Some(snap(unix_ms()))),
                lock_held: Box::new(|| true),
                ensure: Box::new(move || {
                    e.fetch_add(1, Ordering::SeqCst);
                }),
                refresh: Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }),
            }
        };
        let (tx, rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            run_worker(actions, io, tx, cmd_rx, Arc::new(AtomicBool::new(false)))
        });
        let first = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first state");
        assert_eq!(first.watcher, WatcherStatus::Running);
        let asked = Instant::now();
        cmd_tx.send(Command::Refresh).unwrap();
        while refreshes.load(Ordering::SeqCst) == 0 {
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "refresh not handled"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // Dropping the sender ends the 500ms wait at once.
        let quit = Instant::now();
        drop(cmd_tx);
        handle.join().unwrap();
        assert!(quit.elapsed() < Duration::from_secs(2));
        assert_eq!(ensures.load(Ordering::SeqCst), 0, "held lock: no ensure");
    }

    fn select(app: &mut App, pane: &str) {
        for _ in 0..app.rows().len() {
            app.on_key(KeyCode::Char('k'));
        }
        for _ in 0..app.rows().len() {
            if matches!(app.selected_row(), Some(Row::Agent(a)) if a.info.pane_id == pane) {
                return;
            }
            app.on_key(KeyCode::Char('j'));
        }
        panic!("no row for {pane}");
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_reply(app: &mut App, text: &str) {
        for c in text.chars() {
            assert_eq!(app.on_key_event(key(KeyCode::Char(c))), None);
        }
    }

    #[test]
    fn tab_opens_a_reply_box_only_on_agent_rows() {
        let mut app = with_merged();
        select(&mut app, "d");
        app.on_key_event(key(KeyCode::Tab));
        let reply = app.reply.clone().expect("box on an agent row");
        assert_eq!((reply.pane.as_str(), reply.refusal), ("d", None));
        assert!(app.has_modal());
        app.on_key_event(key(KeyCode::Esc));
        assert!(app.reply.is_none());
        for _ in 0..app.rows().len() {
            app.on_key(KeyCode::Char('j'));
        }
        assert!(matches!(app.selected_row(), Some(Row::Worktree(_))));
        app.on_key_event(key(KeyCode::Tab));
        assert!(app.reply.is_none(), "no box on a worktree row");
    }

    #[test]
    fn reply_box_types_every_key_and_sends_the_exact_text() {
        let mut app = app();
        select(&mut app, "d");
        app.on_key_event(key(KeyCode::Tab));
        type_reply(&mut app, "q1[]x");
        app.on_key_event(key(KeyCode::Backspace));
        app.on_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        app.on_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_reply(&mut app, "ok");
        app.on_paste(" pasted\r\nline");
        assert_eq!(app.tab, Tab::Agents, "1 and [ ] typed, not switched");
        assert!(app.has_modal(), "q types instead of quitting");
        assert_eq!(
            app.on_key_event(key(KeyCode::Enter)),
            Some(Command::Reply {
                pane: "d".into(),
                text: "q1[]\n\nok pasted\nline".into(),
                stall_hash: None,
            })
        );
        let screen = render_h(&app, 14).join("\n");
        assert!(screen.contains("Reply to finished"), "{screen}");
        assert!(screen.contains("sending…"), "{screen}");
        assert_eq!(
            app.on_key_event(key(KeyCode::Enter)),
            None,
            "no double send"
        );
    }

    #[test]
    fn reply_box_ignores_an_empty_reply_and_caps_its_length() {
        let mut app = app();
        select(&mut app, "d");
        app.on_key_event(key(KeyCode::Tab));
        type_reply(&mut app, "  ");
        assert_eq!(app.on_key_event(key(KeyCode::Enter)), None);
        type_reply(&mut app, &"x".repeat(REPLY_MAX));
        assert_eq!(app.reply.as_ref().unwrap().text.chars().count(), REPLY_MAX);
    }

    #[test]
    fn reply_box_closes_when_sent_and_keeps_the_text_when_refused() {
        let mut app = app();
        select(&mut app, "d");
        app.on_key_event(key(KeyCode::Tab));
        type_reply(&mut app, "hi");
        app.on_key_event(key(KeyCode::Enter));
        let mut state = app.state.clone();
        state.replied = Some((1, false));
        state.notice = Some("not sent: d is working".into());
        app.apply(state.clone());
        let reply = app.reply.clone().expect("kept after a refusal");
        assert_eq!((reply.text.as_str(), reply.sending), ("hi", false));
        app.on_key_event(key(KeyCode::Enter));
        state.replied = Some((2, true));
        app.apply(state);
        assert!(app.reply.is_none());
    }

    #[test]
    fn reply_box_refuses_busy_or_asking_agents_and_enter_goes_there() {
        let mut app = app();
        for (pane, why) in [("c", "it's asking"), ("q", "it's working")] {
            select(&mut app, pane);
            app.on_key_event(key(KeyCode::Tab));
            let refusal = app.reply.as_ref().unwrap().refusal.clone().unwrap();
            assert!(refusal.starts_with(why), "{pane}: {refusal}");
            let screen = render_h(&app, 14).join("\n");
            assert!(screen.contains("Can't reply"), "{screen}");
            type_reply(&mut app, "x");
            assert_eq!(app.reply.as_ref().unwrap().text, "");
            assert_eq!(
                app.on_key_event(key(KeyCode::Enter)),
                Some(Command::Focus(pane.into()))
            );
            assert!(app.reply.is_none());
        }
    }

    #[test]
    fn a_stalled_agent_takes_a_reply_with_its_stall_hash() {
        let mut app = app();
        app.state.agents[1].stall_hash = Some(42);
        select(&mut app, "a");
        app.on_key_event(key(KeyCode::Tab));
        type_reply(&mut app, "go on");
        assert_eq!(
            app.on_key_event(key(KeyCode::Enter)),
            Some(Command::Reply {
                pane: "a".into(),
                text: "go on".into(),
                stall_hash: Some(42),
            })
        );
    }

    // ---- PRs tab ----

    use crate::features::inbox::prs::PrData;
    use crate::features::inbox::watcher::{DetailView, PrView};
    use crate::integrations::github::{parse_detail, parse_lists};

    const PR_LISTS: &str = include_str!("../../../tests/fixtures/gh/pr-lists.json");
    const PR_DETAIL: &str = include_str!("../../../tests/fixtures/gh/pr-detail.json");

    fn pr_view() -> PrView {
        PrView {
            data: Some(Arc::new(PrData {
                lists: parse_lists(PR_LISTS).unwrap(),
                checkouts: vec![],
                fetched_unix_ms: 1,
            })),
            updating: false,
            error: None,
        }
    }

    fn pr_app() -> App {
        let mut a = App::new();
        assert_eq!(
            a.on_key(KeyCode::Char('2')),
            Some(Command::TabVisible(Tab::Prs))
        );
        a.apply(WatchState {
            prs: pr_view(),
            watcher: WatcherStatus::Running,
            ticked_unix_ms: Some(unix_ms()),
            ..Default::default()
        });
        a
    }

    #[test]
    fn ctrl_c_quits_and_never_acts_as_c() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut a = issue_app();
        assert!(a.quits(ctrl_c));
        assert_eq!(a.on_key_event(ctrl_c), None, "not FetchTeamStates");
        let mut a = app();
        a.on_key(KeyCode::Char('/'));
        assert!(a.has_modal());
        assert!(a.quits(ctrl_c), "Ctrl+C quits even while typing");
        assert!(!a.quits(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    }

    fn marked(lines: &[String]) -> Vec<&String> {
        lines.iter().filter(|l| l.starts_with("› ")).collect()
    }

    #[test]
    fn prs_render_header_headings_and_one_marker_on_a_pr() {
        let a = pr_app();
        let screen = render_h(&a, 30);
        assert!(
            screen[1].starts_with("Mine 2 · Assigned 6 of 70"),
            "{screen:?}"
        );
        assert!(screen[2].starts_with("  Needs fixing (5)"), "{screen:?}");
        for g in ["Ready to merge (1)", "Waiting (2)", "Drafts (0)"] {
            assert!(
                screen.iter().any(|l| l.trim_start().starts_with(g)),
                "{g} {screen:?}"
            );
        }
        let m = marked(&screen);
        assert_eq!(m.len(), 1, "{screen:?}");
        assert!(m[0].starts_with("› ● #"), "{m:?}");
        assert!(screen
            .last()
            .unwrap()
            .starts_with("jk move  ⏎ open  s start  f factory  R rerun  w worktree"));
    }

    #[test]
    fn a_40_column_pr_row_still_shows_your_review() {
        let a = pr_app();
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| draw(f, &a, unix_ms())).unwrap();
        let buf = terminal.backend().buffer();
        let screen: Vec<String> = (0..12)
            .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(screen[3].starts_with("› ● #30151 "), "{screen:?}");
        assert!(screen[4].contains("your review: approved"), "{screen:?}");
        assert!(screen[4].ends_with('…'), "{screen:?}");
    }

    #[test]
    fn j_and_k_skip_headings_and_clamp_at_both_ends() {
        let mut a = pr_app();
        let n = a.pr_rows().len();
        a.on_key(KeyCode::Char('k'));
        assert_eq!(a.pr_selected, 0);
        for _ in 0..n + 3 {
            a.on_key(KeyCode::Char('j'));
            let screen = render_h(&a, 60);
            let m = marked(&screen);
            assert_eq!(m.len(), 1);
            assert!(
                m[0].starts_with("› ● #") || m[0].starts_with("› ◌ #"),
                "{m:?}"
            );
        }
        assert_eq!(a.pr_selected, n - 1);
    }

    #[test]
    fn an_empty_list_has_no_selection_and_keys_do_nothing() {
        let mut a = App::new();
        a.on_key(KeyCode::Char('2'));
        let mut view = pr_view();
        view.data = Some(Arc::new(PrData::default()));
        a.apply(WatchState {
            prs: view,
            ..Default::default()
        });
        for k in ['j', 'k', 'd', 'w', 'o'] {
            assert_eq!(a.on_key(KeyCode::Char(k)), None);
        }
        assert_eq!(a.on_key(KeyCode::Enter), None);
        assert!(marked(&render_h(&a, 20)).is_empty());
        assert!(render_h(&a, 20)[2]
            .trim_start()
            .starts_with("Needs fixing (0)"));
    }

    #[test]
    fn a_refetch_keeps_the_selected_pr_or_clamps_onto_a_pr_row() {
        let mut a = pr_app();
        a.on_key(KeyCode::Char('j'));
        a.on_key(KeyCode::Char('j'));
        let id = a.selected_pr().unwrap().id;
        // Same PR, list reordered by an older PR going away.
        let mut view = pr_view();
        let data = Arc::make_mut(view.data.as_mut().unwrap());
        let first = a.pr_rows()[0].id.clone();
        data.lists.mine.retain(|p| p.id != first);
        data.lists.assigned.retain(|p| p.id != first);
        a.apply(WatchState {
            prs: view.clone(),
            ..Default::default()
        });
        assert_eq!(a.selected_pr().unwrap().id, id);
        // The selected PR itself goes: the selection lands on a PR, never a heading.
        let data = Arc::make_mut(view.data.as_mut().unwrap());
        data.lists.mine.retain(|p| p.id != id);
        data.lists.assigned.retain(|p| p.id != id);
        a.apply(WatchState {
            prs: view,
            ..Default::default()
        });
        assert!(a.selected_pr().is_some());
        let screen = render_h(&a, 60);
        assert_eq!(marked(&screen).len(), 1);
        assert!(marked(&screen)[0].contains(" #"), "{screen:?}");
    }

    #[test]
    fn each_tab_keeps_its_own_selection_and_tab_keys_say_which_is_shown() {
        let mut a = pr_app();
        a.on_key(KeyCode::Char('j'));
        let pr = a.pr_selected;
        assert_eq!(
            a.on_key(KeyCode::Char('1')),
            Some(Command::TabVisible(Tab::Agents))
        );
        assert_eq!(a.on_key(KeyCode::Char('1')), None, "already shown");
        assert_eq!(
            a.on_key(KeyCode::Char(']')),
            Some(Command::TabVisible(Tab::Prs))
        );
        assert_eq!(a.pr_selected, pr);
        assert_eq!(a.on_key(KeyCode::Char('r')), Some(Command::FetchPrs));
        assert_eq!(
            a.on_key(KeyCode::Char('[')),
            Some(Command::TabVisible(Tab::Agents))
        );
        assert_eq!(a.on_key(KeyCode::Char('r')), Some(Command::Refresh));
    }

    #[test]
    fn cached_lists_show_at_once_marked_updating_or_stale() {
        let mut a = pr_app();
        let mut view = pr_view();
        view.updating = true;
        a.apply(WatchState {
            prs: view.clone(),
            ..Default::default()
        });
        assert!(render_h(&a, 20)[1].contains("· updating…"));
        view.updating = false;
        view.error = Some("gh: HTTP 502".into());
        a.apply(WatchState {
            prs: view,
            ..Default::default()
        });
        assert!(render_h(&a, 20)[1].contains("· stale · gh: HTTP 502"));
    }

    #[test]
    fn rendering_the_prs_tab_runs_nothing() {
        // The worker composes from cached state; drawing never touches a Runner.
        let actions = Actions::new(
            Herdr::with_bin(FakeRunner::new(), "herdr"),
            Arc::new(AtomicBool::new(false)),
        );
        let mut a = pr_app();
        let mut state = compose(None, WatcherStatus::Running, &actions);
        state.prs = pr_view();
        a.apply(state);
        render_h(&a, 40);
        assert!(actions.herdr.runner.calls().is_empty());
    }

    #[test]
    fn enter_opens_the_pr_screen_and_only_its_own_reply_fills_it() {
        let mut a = pr_app();
        let detail = parse_detail(PR_DETAIL).unwrap();
        let at = a
            .pr_rows()
            .iter()
            .position(|p| p.id == detail.pr.id)
            .unwrap();
        for _ in 0..at {
            a.on_key(KeyCode::Char('j'));
        }
        let cmd = a.on_key(KeyCode::Enter).unwrap();
        let Command::FetchPrDetail { serial, number, .. } = cmd else {
            panic!("{cmd:?}");
        };
        assert_eq!(number, 30148);
        let screen = render_h(&a, 30);
        assert!(screen[2].starts_with("● #30148 "), "{screen:?}");
        assert!(screen.iter().any(|l| l.starts_with("merge: ")));
        assert!(screen.iter().any(|l| l.starts_with("loading…")));
        // A late reply for an earlier request is dropped.
        let reply = |serial| DetailView {
            serial,
            number: 30148,
            detail: Some(Arc::new(detail.clone())),
            error: None,
        };
        a.apply(WatchState {
            prs: pr_view(),
            pr_detail: Some(reply(serial - 1)),
            ..Default::default()
        });
        assert!(render_h(&a, 30).iter().any(|l| l.starts_with("loading…")));
        a.apply(WatchState {
            prs: pr_view(),
            pr_detail: Some(reply(serial)),
            ..Default::default()
        });
        let screen = render_h(&a, 40);
        assert!(
            screen.iter().any(|l| l.contains("widget report")),
            "{screen:?}"
        );
        assert!(screen.iter().any(|l| l.starts_with("reviewers: ")));
        assert_eq!(a.on_key(KeyCode::Esc), None);
        assert_eq!(a.pr_open, None);
    }

    #[test]
    fn wrap_line_breaks_on_words_and_keeps_styles() {
        let red = Style::new().fg(Color::Red);
        let line = Line::from(vec![
            Span::raw("aaaaaa "),
            Span::styled("bbbbbb cccccc", red),
        ]);
        let rows = wrap_line(line, 10);
        let text: Vec<String> = rows.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, ["aaaaaa ", "bbbbbb ", "cccccc"]);
        assert_eq!(rows[2].spans[0].style, red);
        let long = wrap_line(Line::raw("x".repeat(25)), 10);
        assert_eq!(long.len(), 3, "a word wider than the row is split");
        assert_eq!(wrap_line(Line::raw(""), 10).len(), 1, "blank lines stay");
    }

    #[test]
    fn a_wrapped_description_scrolls_to_its_last_line_and_k_works_after_a_resize() {
        let mut a = pr_app();
        let mut detail = parse_detail(PR_DETAIL).unwrap();
        // Lines wider than the 100-column body that wrap on word boundaries.
        detail.body = (1..=30)
            .map(|i| format!("{} end{i}\n", "wordy words here ".repeat(7)))
            .collect();
        let at = a
            .pr_rows()
            .iter()
            .position(|p| p.id == detail.pr.id)
            .unwrap();
        for _ in 0..at {
            a.on_key(KeyCode::Char('j'));
        }
        let Some(Command::FetchPrDetail { serial, .. }) = a.on_key(KeyCode::Enter) else {
            panic!()
        };
        a.apply(WatchState {
            prs: pr_view(),
            pr_detail: Some(DetailView {
                serial,
                number: 30148,
                detail: Some(Arc::new(detail)),
                error: None,
            }),
            ..Default::default()
        });
        for _ in 0..300 {
            a.on_key(KeyCode::Char('j'));
            render_h(&a, 20);
        }
        let screen = render_h(&a, 20);
        assert!(screen.iter().any(|l| l.contains("end30")), "{screen:?}");
        // A taller terminal lowers the end; k must move at once, not after the
        // stale overshoot is used up.
        render_h(&a, 40);
        let max = a.detail_max.get();
        assert!(a.detail_scroll > max);
        a.on_key(KeyCode::Char('k'));
        assert_eq!(a.detail_scroll, max - 1);
    }

    #[test]
    fn jk_scroll_a_long_pr_description_and_stop_at_its_end() {
        let mut a = pr_app();
        let mut detail = parse_detail(PR_DETAIL).unwrap();
        detail.body = (1..=60).map(|i| format!("body line {i}\n")).collect();
        let at = a
            .pr_rows()
            .iter()
            .position(|p| p.id == detail.pr.id)
            .unwrap();
        for _ in 0..at {
            a.on_key(KeyCode::Char('j'));
        }
        let Some(Command::FetchPrDetail { serial, .. }) = a.on_key(KeyCode::Enter) else {
            panic!()
        };
        a.apply(WatchState {
            prs: pr_view(),
            pr_detail: Some(DetailView {
                serial,
                number: 30148,
                detail: Some(Arc::new(detail)),
                error: None,
            }),
            ..Default::default()
        });
        assert!(!render_h(&a, 20).iter().any(|l| l.contains("body line 60")));
        for _ in 0..200 {
            a.on_key(KeyCode::Char('j'));
            render_h(&a, 20);
        }
        let screen = render_h(&a, 20);
        assert!(
            screen.iter().any(|l| l.contains("body line 60")),
            "{screen:?}"
        );
        let at_end = a.detail_scroll;
        assert!(at_end < 200, "the scroll stops at the end");
        a.on_key(KeyCode::Char('k'));
        assert_eq!(a.detail_scroll, at_end - 1, "one k scrolls back at once");
        assert!(a.pr_open.is_some(), "jk don't leave the screen");
    }

    #[test]
    fn w_and_o_act_on_the_selected_pr() {
        let mut a = pr_app();
        let pr = a.selected_pr().unwrap();
        assert_eq!(
            a.on_key(KeyCode::Char('w')),
            Some(Command::OpenPrWorktree {
                repo: pr.repo.clone(),
                head_repo: pr.head_repo.clone(),
                head: pr.head_ref.clone(),
                number: pr.number,
            })
        );
        assert_eq!(a.on_key(KeyCode::Char('o')), Some(Command::OpenUrl(pr.url)));
    }

    // --- Issues tab ---

    fn issue_view() -> IssueView {
        let mine = linear::parse_my_issues(include_str!(
            "../../../tests/fixtures/linear/my-issues.json"
        ))
        .unwrap();
        IssueView {
            data: Some(Arc::new(issues::IssueData {
                mine,
                checkouts: vec![],
                fetched_unix_ms: 1,
            })),
            updating: false,
            error: None,
        }
    }

    fn issue_app() -> App {
        let mut a = App::new();
        assert_eq!(
            a.on_key(KeyCode::Char('3')),
            Some(Command::TabVisible(Tab::Issues))
        );
        a.apply(WatchState {
            issue_list: issue_view(),
            watcher: WatcherStatus::Running,
            ticked_unix_ms: Some(unix_ms()),
            ..Default::default()
        });
        a
    }

    fn team_states() -> Vec<TeamState> {
        linear::parse_team_states(include_str!(
            "../../../tests/fixtures/linear/team-states.json"
        ))
        .unwrap()
    }

    #[test]
    fn issues_render_grouped_with_one_marker_on_an_issue() {
        let a = issue_app();
        let screen = render_h(&a, 30);
        let text = screen.join("\n");
        for heading in [
            "In review (1)",
            "In progress (1)",
            "Todo (3)",
            "Triage (1)",
            "Backlog (1)",
        ] {
            assert!(text.contains(heading), "{heading}: {text}");
        }
        let marked = marked(&screen);
        assert_eq!(marked.len(), 1, "{screen:?}");
        assert!(marked[0].contains("ENG-5 "), "{marked:?}");
    }

    #[test]
    fn issue_selection_skips_headings_clamps_and_follows_a_regrouped_issue() {
        let mut a = issue_app();
        a.on_key(KeyCode::Char('k'));
        assert_eq!(a.selected_issue().unwrap().identifier, "ENG-5");
        a.on_key(KeyCode::Char('j'));
        assert_eq!(a.selected_issue().unwrap().identifier, "ENG-4");
        // ENG-4 moves to Backlog: the selection stays on it.
        let mut view = issue_view();
        let data = Arc::make_mut(view.data.as_mut().unwrap());
        let i = data
            .mine
            .issues
            .iter_mut()
            .find(|i| i.id == "issue-4")
            .unwrap();
        i.state = WorkflowState {
            id: "st-backlog".into(),
            name: "Backlog".into(),
            kind: "backlog".into(),
        };
        a.apply(WatchState {
            issue_list: view,
            ..Default::default()
        });
        assert_eq!(a.selected_issue().unwrap().identifier, "ENG-4");
        let marked: Vec<String> = marked(&render_h(&a, 30)).into_iter().cloned().collect();
        assert!(marked[0].contains("ENG-4 "), "{marked:?}");
        for _ in 0..20 {
            a.on_key(KeyCode::Char('j'));
        }
        assert_eq!(a.issue_selected, 6);
    }

    #[test]
    fn enter_opens_the_issue_screen_and_esc_returns() {
        let mut a = issue_app();
        assert_eq!(
            a.on_key(KeyCode::Enter),
            Some(Command::FetchIssueDetail {
                serial: 1,
                id: "issue-5".into(),
            })
        );
        assert!(render_h(&a, 30).join("\n").contains("loading…"));
        assert_eq!(a.on_key(KeyCode::Esc), None);
        assert!(a.issue_open.is_none());
        assert_eq!(
            a.on_key(KeyCode::Char('o')),
            Some(Command::OpenUrl(a.selected_issue().unwrap().url))
        );
    }

    #[test]
    fn the_state_picker_marks_clamps_cancels_and_sets() {
        let mut a = issue_app();
        let open = |a: &mut App, serial| {
            assert_eq!(
                a.on_key(KeyCode::Char('c')),
                Some(Command::FetchTeamStates {
                    issue_id: "issue-5".into(),
                    identifier: "ENG-5".into(),
                    team_id: "team-1111".into(),
                    current: "st-review".into(),
                })
            );
            let mut state = a.state.clone();
            state.states = Some(StatesView {
                serial,
                issue_id: "issue-5".into(),
                identifier: "ENG-5".into(),
                current: "st-review".into(),
                states: team_states(),
            });
            a.apply(state);
        };
        open(&mut a, 1);
        let p = a.picker.as_ref().expect("picker");
        assert_eq!(
            p.states[p.selected].id, "st-review",
            "starts on the current state"
        );
        assert!(a.has_modal());
        assert_eq!(a.on_key(KeyCode::Char('q')), None);
        for _ in 0..20 {
            a.on_key(KeyCode::Down);
        }
        let p = a.picker.as_ref().unwrap();
        assert_eq!(p.selected, p.states.len() - 1);
        for _ in 0..20 {
            a.on_key(KeyCode::Up);
        }
        assert_eq!(a.picker.as_ref().unwrap().selected, 0);
        assert_eq!(a.on_key(KeyCode::Esc), None);
        assert!(a.picker.is_none());
        open(&mut a, 2);
        // Walk from the top to Todo.
        let todo = a
            .picker
            .as_ref()
            .unwrap()
            .states
            .iter()
            .position(|s| s.id == "st-todo")
            .unwrap();
        a.picker.as_mut().unwrap().selected = 0;
        for _ in 0..todo {
            a.on_key(KeyCode::Char('j'));
        }
        assert_eq!(
            a.on_key(KeyCode::Enter),
            Some(Command::SetIssueState {
                issue_id: "issue-5".into(),
                identifier: "ENG-5".into(),
                state: WorkflowState {
                    id: "st-todo".into(),
                    name: "Todo".into(),
                    kind: "unstarted".into(),
                },
            })
        );
        assert!(a.picker.is_none());
    }

    #[test]
    fn s_asks_the_worker_to_check_the_issue_then_picks_a_repo() {
        let mut a = issue_app();
        assert_eq!(
            a.on_key(KeyCode::Char('s')),
            Some(Command::PrepareStart {
                id: "issue-5".into(),
                identifier: "ENG-5".into(),
            })
        );
        assert!(a.has_modal());
        assert_eq!(
            a.on_key(KeyCode::Char('x')),
            None,
            "no typing while checking"
        );
        let first = linear::parse_my_issues(include_str!(
            "../../../tests/fixtures/linear/my-issues.json"
        ))
        .unwrap()
        .issues
        .into_iter()
        .find(|i| i.id == "issue-5")
        .unwrap();
        let mut state = a.state.clone();
        state.launch = Some(LaunchView {
            serial: 1,
            issue: first.as_issue(),
            repos: vec![issue::RepoChoice {
                repo_root: "/r/repo".into(),
                label: "repo — repo".into(),
            }],
            start: Some(Box::new(first.clone())),
        });
        a.apply(state);
        assert!(render_h(&a, 30).join("\n").contains("› repo — repo"));
        assert_eq!(
            a.on_key(KeyCode::Enter),
            Some(Command::Start {
                issue: Box::new(first),
                repo_root: "/r/repo".into(),
            })
        );
        assert!(a.launch.is_none());
    }

    fn factory_ready(mut a: App) -> App {
        a.factory = Factory::from_file(None, Some("U0FACTORY".into()));
        a.slack_ready = true;
        a
    }

    fn sent(a: &mut App, serial: u64, ok: bool) {
        let state = WatchState {
            factory_sent: Some((serial, ok)),
            ..a.state.clone()
        };
        a.apply(state);
    }

    #[test]
    fn f_without_slack_config_flashes_and_posts_nothing() {
        let mut a = issue_app();
        assert_eq!(a.on_key(KeyCode::Char('f')), None);
        assert!(a.factory_confirm.is_none());
        let text = render_h(&a, 30).join("\n");
        assert!(
            text.contains("SLACK_USER_TOKEN or SLACK_CHANNEL not set"),
            "{text}"
        );
        a.slack_ready = true;
        assert_eq!(a.on_key(KeyCode::Char('f')), None);
        assert!(a.factory_confirm.is_none());
        assert!(a
            .flash
            .as_deref()
            .unwrap()
            .contains("SLACK_FACTORY_MENTION"));
    }

    #[test]
    fn f_on_an_issue_confirms_the_exact_text_then_posts_it() {
        let mut a = factory_ready(issue_app());
        let issue = a.selected_issue().unwrap();
        let want = a
            .factory
            .compose(
                a.factory.for_role(Role::Issue),
                std::slice::from_ref(&issue.url),
            )
            .unwrap();
        assert_eq!(
            a.on_key(KeyCode::Char('f')),
            None,
            "nothing leaves before y"
        );
        assert_eq!(a.factory_confirm.as_ref().unwrap().message, want);
        assert!(want.body.starts_with("<@U0FACTORY> "), "{want:?}");
        assert!(want.body.contains(&issue.url), "{want:?}");
        let screen = render_h(&a, 30).join("\n");
        assert!(screen.contains("factory (slack) →"), "{screen}");
        assert!(screen.contains("Sent via werkbank"), "{screen}");
        assert!(a.has_modal(), "q must not quit from the confirm");
        assert_eq!(
            a.on_key(KeyCode::Char('2')),
            None,
            "tabs don't switch under it"
        );
        assert_eq!(a.tab, Tab::Issues);
        assert_eq!(
            a.on_key(KeyCode::Char('y')),
            Some(Command::FactorySend { message: want })
        );
        assert!(a.factory_confirm.is_none());
        assert_eq!(a.pending.as_deref(), Some("posting to Slack…"));
        sent(&mut a, 1, true);
        assert!(a.pending.is_none());
    }

    #[test]
    fn cancelling_the_factory_confirm_posts_nothing() {
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            let mut a = factory_ready(issue_app());
            a.on_key(KeyCode::Char('f'));
            assert_eq!(a.on_key(key), None);
            assert!(a.factory_confirm.is_none());
            assert_eq!(a.on_key(KeyCode::Char('j')), None);
        }
    }

    #[test]
    fn s_still_starts_locally_and_f_is_the_only_way_to_factory() {
        let mut a = factory_ready(issue_app());
        assert!(matches!(
            a.on_key(KeyCode::Char('s')),
            Some(Command::PrepareStart { .. })
        ));
        assert!(a.factory_confirm.is_none());
    }

    #[test]
    fn marked_issues_batch_in_display_order_and_clear_once_sent() {
        let mut a = factory_ready(issue_app());
        // ENG-5 shows first, ENG-4 second; mark them in reverse.
        a.on_key(KeyCode::Char('j'));
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Char('k'));
        a.on_key(KeyCode::Char(' '));
        assert_eq!(a.marks, vec!["issue-4".to_string(), "issue-5".to_string()]);
        let screen = render_h(&a, 30);
        assert!(screen[1].contains("2 marked"), "{screen:?}");
        assert!(screen[29].contains("f factory"), "{screen:?}");
        assert!(
            screen[29].contains("q quit"),
            "keys fit 100 columns: {screen:?}"
        );
        assert!(
            screen
                .iter()
                .any(|l| l.contains("• ") && l.contains("ENG-4 ")),
            "{screen:?}"
        );
        a.on_key(KeyCode::Char('f'));
        let text = a.factory_confirm.as_ref().unwrap().message.body.clone();
        let (five, four) = (
            text.find("issue/ENG-5/").unwrap(),
            text.find("issue/ENG-4/").unwrap(),
        );
        assert!(five < four, "display order: {text}");
        assert!(text.contains("single PR"), "the batch prompt: {text}");
        a.on_key(KeyCode::Char('y'));
        sent(&mut a, 1, false);
        assert_eq!(a.marks.len(), 2, "a failed post keeps the marks");
        a.on_key(KeyCode::Char('f'));
        a.on_key(KeyCode::Char('y'));
        sent(&mut a, 2, true);
        assert!(a.marks.is_empty());
    }

    #[test]
    fn space_unmarks_and_marks_drop_with_their_rows() {
        let mut a = issue_app();
        a.on_key(KeyCode::Char(' '));
        a.on_key(KeyCode::Char(' '));
        assert!(a.marks.is_empty());
        a.on_key(KeyCode::Char(' '));
        assert_eq!(a.marks, vec!["issue-5".to_string()]);
        let mut view = issue_view();
        Arc::make_mut(view.data.as_mut().unwrap())
            .mine
            .issues
            .retain(|i| i.id != "issue-5");
        a.apply(WatchState {
            issue_list: view,
            ..Default::default()
        });
        assert!(a.marks.is_empty());
    }

    #[test]
    fn f_on_a_pr_confirms_a_review_request_with_its_url() {
        let mut a = factory_ready(pr_app());
        let url = a.selected_pr().unwrap().url;
        a.on_key(KeyCode::Char('f'));
        let pick = render_h(&a, 30).join("\n");
        for name in ["Review PR", "Fix CI", "Address review", "Write a message"] {
            assert!(pick.contains(name), "{name}: {pick}");
        }
        assert!(a.has_modal(), "q can't quit the picker");
        assert_eq!(a.on_key(KeyCode::Char('q')), None);
        assert_eq!(a.on_key(KeyCode::Enter), None);
        let text = a.factory_confirm.as_ref().unwrap().message.body.clone();
        assert!(text.contains("Review") && text.contains(&url), "{text}");
        assert!(matches!(
            a.on_key(KeyCode::Char('y')),
            Some(Command::FactorySend { .. })
        ));
    }

    #[test]
    fn s_on_a_pr_starts_a_local_agent_with_the_picked_prompt_after_a_confirm() {
        // No Slack config: a local start never needs it.
        let mut a = pr_app();
        let pr = a.selected_pr().unwrap();
        a.on_key(KeyCode::Char('s'));
        a.on_key(KeyCode::Char('j'));
        assert_eq!(a.on_key(KeyCode::Enter), None, "a confirm comes first");
        let c = a.factory_confirm.clone().unwrap();
        assert!(c.message.body.contains("CI"), "{:?}", c.message);
        assert!(c.message.body.contains(&pr.url), "unescaped URL locally");
        assert_eq!(c.message.signature, None, "no Slack signature locally");
        let screen = render_h(&a, 30).join("\n");
        assert!(
            screen.contains(&format!("local agent on #{}", pr.number)),
            "{screen}"
        );
        assert_eq!(
            a.on_key(KeyCode::Char('y')),
            Some(Command::StartPr {
                repo: pr.repo,
                head_repo: pr.head_repo,
                head: pr.head_ref,
                number: pr.number,
                prompt: c.message.body,
            })
        );
    }

    #[test]
    fn a_typed_pr_message_carries_the_url_and_esc_drops_the_picker() {
        let mut a = factory_ready(pr_app());
        let url = a.selected_pr().unwrap().url;
        a.on_key(KeyCode::Char('f'));
        assert_eq!(a.on_key(KeyCode::Esc), None);
        assert!(a.pr_pick.is_none() && !a.has_modal());
        a.on_key(KeyCode::Char('s'));
        for _ in 0..5 {
            a.on_key(KeyCode::Char('j'));
        }
        a.on_key(KeyCode::Enter);
        a.on_paste("rebase it\nthen push");
        a.on_key(KeyCode::Enter);
        let c = a.factory_confirm.clone().unwrap();
        assert_eq!(c.message.body, format!("rebase it\nthen push\n{url}"));
        assert!(c.local.is_some());
        a.on_key(KeyCode::Char('n'));

        a.on_key(KeyCode::Char('f'));
        for _ in 0..5 {
            a.on_key(KeyCode::Char('j'));
        }
        a.on_key(KeyCode::Enter);
        a.on_paste("look at the flake");
        a.on_key(KeyCode::Enter);
        let c = a.factory_confirm.clone().unwrap();
        assert!(c.local.is_none());
        assert!(
            c.message
                .body
                .starts_with("<@U0FACTORY> look at the flake\n"),
            "{:?}",
            c.message
        );
    }

    #[test]
    fn capital_r_reruns_only_a_pr_with_failing_checks() {
        let mut a = pr_app();
        let pr = a.selected_pr().unwrap();
        let cmd = a.on_key(KeyCode::Char('R'));
        if pr.ci == Ci::Fail {
            assert_eq!(
                cmd,
                Some(Command::RerunFailed {
                    repo: pr.repo,
                    head: pr.head_ref,
                    number: pr.number
                })
            );
        } else {
            assert_eq!(cmd, None);
            assert!(a.flash.as_deref().unwrap().ends_with("no failing checks"));
        }
    }

    #[test]
    fn every_issue_or_pr_prompt_on_the_factory_tab_asks_for_its_url() {
        let mut a = factory_ready(App::new());
        // Written without `{ref}`: the URL still has to come from somewhere.
        a.factory = Factory::from_file(
            Some(
                r#"{"prompts":[{"name":"Fix","role":"pr","text":"Fix it."},
                {"name":"Take","role":"issue","text":"Take it."}]}"#,
            ),
            Some("U0FACTORY".into()),
        );
        a.on_key(KeyCode::Char('4'));
        let rows = a.factory_rows().len();
        for i in 1..rows {
            a.factory_selected = i;
            assert_eq!(a.on_key(KeyCode::Enter), None);
            assert!(a.factory_confirm.is_none(), "row {i} sent without a URL");
            assert!(a.factory_input.is_some(), "row {i}");
            a.on_key(KeyCode::Esc);
        }
    }

    #[test]
    fn f_on_a_pr_without_slack_flashes_and_opens_nothing() {
        let mut a = pr_app();
        assert_eq!(a.on_key(KeyCode::Char('f')), None);
        assert!(a.pr_pick.is_none());
        assert!(a.flash.as_deref().unwrap().contains("not set"));
    }

    #[test]
    fn the_factory_tab_asks_for_a_url_then_confirms() {
        let mut a = factory_ready(App::new());
        a.on_key(KeyCode::Char('4'));
        let screen = render_h(&a, 30).join("\n");
        assert!(screen.contains("Fix CI"), "{screen}");
        assert!(screen.contains("Address review"), "{screen}");
        assert!(
            !screen.contains("Linear issues → one PR"),
            "the batch prompt only lives on Issues: {screen}"
        );
        assert!(screen.contains("Message"), "{screen}");
        assert!(a
            .factory_rows()
            .iter()
            .all(|r| !matches!(r, FactoryRow::Prompt(p) if p.role == Role::IssueBatch)));
        for _ in 0..10 {
            a.on_key(KeyCode::Char('j'));
        }
        assert_eq!(a.factory_selected, 4, "j stops at the last listed prompt");
        a.factory_selected = a
            .factory_rows()
            .iter()
            .position(|r| matches!(r, FactoryRow::Prompt(p) if p.role == Role::Pr))
            .unwrap();
        a.on_key(KeyCode::Enter);
        assert!(a.has_modal());
        for c in "https://github.com/o/r/issues/1".chars() {
            assert_eq!(a.on_key(KeyCode::Char(c)), None);
        }
        assert_eq!(a.tab, Tab::Factory, "typing 1 doesn't switch tabs");
        a.on_key(KeyCode::Enter);
        assert!(a.factory_input.is_none());
        let message = a.factory_confirm.as_ref().unwrap().message.clone();
        assert!(
            message.body.contains("https://github.com/o/r/issues/1"),
            "{message:?}"
        );
        assert_eq!(
            a.on_key(KeyCode::Char('y')),
            Some(Command::FactorySend { message })
        );
    }

    #[test]
    fn the_factory_ref_box_is_bounded() {
        let mut a = factory_ready(App::new());
        a.on_key(KeyCode::Char('4'));
        a.factory_selected = a
            .factory_rows()
            .iter()
            .position(|r| matches!(r, FactoryRow::Prompt(p) if p.role == Role::Pr))
            .unwrap();
        a.on_key(KeyCode::Enter);
        assert_eq!(a.on_key(KeyCode::Enter), None, "empty ref goes nowhere");
        for _ in 0..FACTORY_REF_MAX + 10 {
            a.on_key(KeyCode::Char('x'));
        }
        assert_eq!(
            a.factory_input.as_ref().unwrap().text.len(),
            FACTORY_REF_MAX
        );
    }

    #[test]
    fn the_factory_tab_sends_a_typed_message_after_confirm() {
        let mut a = factory_ready(App::new());
        a.on_key(KeyCode::Char('4'));
        assert_eq!(a.factory_selected, 0, "Message is first");
        a.on_key(KeyCode::Enter);
        assert!(a.has_modal());
        for c in "hi {ref} 1".chars() {
            assert_eq!(a.on_key(KeyCode::Char(c)), None);
        }
        assert_eq!(a.tab, Tab::Factory, "typing 1 doesn't switch tabs");
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(a.on_key_event(alt_enter), None);
        a.on_paste("second\r\nyes y\n");
        assert!(
            a.factory_confirm.is_none(),
            "a pasted newline doesn't submit"
        );
        assert_eq!(
            a.factory_input.as_ref().unwrap().text,
            "hi {ref} 1\nsecond\nyes y\n"
        );
        let screen = render_h(&a, 30).join("\n");
        assert!(screen.contains("message to Factory"), "{screen}");
        assert!(screen.contains("alt+⏎ newline"), "{screen}");
        assert_eq!(a.on_key(KeyCode::Enter), None, "nothing leaves before y");
        let want = Message {
            body: "<@U0FACTORY> hi {ref} 1\nsecond\nyes y".into(),
            signature: Some("Sent via werkbank".into()),
        };
        assert_eq!(a.factory_confirm.as_ref().unwrap().message, want);
        let screen = render_h(&a, 30).join("\n");
        assert!(screen.contains("— Sent via werkbank"), "{screen}");
        a.on_paste("y");
        assert!(a.factory_confirm.is_some(), "a pasted y doesn't confirm");
        assert_eq!(
            a.on_key(KeyCode::Char('y')),
            Some(Command::FactorySend { message: want })
        );
    }

    #[test]
    fn an_empty_message_or_missing_config_sends_nothing() {
        let mut a = App::new();
        a.on_key(KeyCode::Char('4'));
        a.on_key(KeyCode::Enter);
        assert!(a.factory_input.is_none());
        assert!(a.flash.as_deref().unwrap().contains("SLACK_USER_TOKEN"));
        let mut a = factory_ready(App::new());
        a.on_key(KeyCode::Char('4'));
        a.on_key(KeyCode::Enter);
        a.on_paste("   \n ");
        assert_eq!(a.on_key(KeyCode::Enter), None);
        assert!(a.factory_confirm.is_none());
    }

    #[test]
    fn a_url_box_keeps_a_paste_on_one_line() {
        let mut a = factory_ready(App::new());
        a.on_key(KeyCode::Char('4'));
        a.on_key(KeyCode::Char('j'));
        a.on_key(KeyCode::Enter);
        a.on_paste("https://x/1\n");
        assert_eq!(a.factory_input.as_ref().unwrap().text, "https://x/1");
        assert!(a.factory_confirm.is_none());
    }

    /// External titles reach spans unfiltered; ratatui drops control
    /// characters, so an ESC or OSC sequence can't reach the terminal.
    #[test]
    fn control_characters_never_reach_the_screen() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        let hostile = "a\u{1b}]52;c;ZXZpbA==\u{7}b\u{1b}[31mc\r\nd\u{9b}e";
        ratatui::widgets::Widget::render(
            Paragraph::new(Line::from(Span::raw(hostile))),
            area,
            &mut buf,
        );
        let shown: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
    }

    fn draft_app(name: &str) -> (App, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("werkbank-ui-drafts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = Factory::from_file(None, Some("U0FACTORY".into()));
        drafts::save(
            &dir,
            &f,
            "Shrink *core* & ship\nsecond line",
            "explore-sizes",
            10,
        )
        .unwrap();
        let mut a = factory_ready(App::new()).with_drafts(dir.clone());
        a.on_key(KeyCode::Char('4'));
        (a, dir)
    }

    #[test]
    fn a_draft_is_listed_first_and_counted_in_the_tab() {
        let (a, dir) = draft_app("list");
        assert!(matches!(a.factory_rows()[0], FactoryRow::Draft(_)));
        let text = render_h(&a, 20).join("\n");
        assert!(text.contains("4 Factory (1)"), "{text}");
        assert!(
            text.contains("Draft from explore-sizes  Shrink *core* & ship"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_draft_posts_escaped_after_y_and_is_deleted_only_once_sent() {
        let (mut a, dir) = draft_app("send");
        assert_eq!(a.on_key(KeyCode::Enter), None);
        let message = a.factory_confirm.as_ref().unwrap().message.clone();
        assert_eq!(
            message.body,
            "<@U0FACTORY> Shrink *core* &amp; ship\nsecond line"
        );
        let text = render_h(&a, 20).join("\n");
        assert!(
            text.contains("Shrink *core* & ship"),
            "preview unescapes: {text}"
        );
        assert!(text.contains("discard"), "{text}");
        assert_eq!(
            a.on_key(KeyCode::Char('y')),
            Some(Command::FactorySend { message })
        );
        sent(&mut a, 1, false);
        assert_eq!(a.drafts.len(), 1, "a failed post keeps the draft");
        assert_eq!(drafts::list(&dir).len(), 1);
        a.on_key(KeyCode::Enter);
        a.on_key(KeyCode::Char('y'));
        sent(&mut a, 2, true);
        assert!(a.drafts.is_empty());
        assert!(drafts::list(&dir).is_empty());
        assert_eq!(a.factory_rows()[0], FactoryRow::Message);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn d_in_a_draft_preview_discards_it_without_posting() {
        let (mut a, dir) = draft_app("discard");
        a.on_key(KeyCode::Enter);
        assert_eq!(a.on_key(KeyCode::Char('d')), None);
        assert!(a.factory_confirm.is_none());
        assert!(drafts::list(&dir).is_empty());
        assert_eq!(a.flash.as_deref(), Some("draft discarded"));
        // `d` means nothing in an ordinary confirm.
        a.on_key(KeyCode::Enter);
        for c in "hi".chars() {
            a.on_key(KeyCode::Char(c));
        }
        a.on_key(KeyCode::Enter);
        assert_eq!(a.on_key(KeyCode::Char('d')), None);
        assert!(a.factory_confirm.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_over_slacks_limit_never_reaches_the_confirm() {
        let mut a = factory_ready(App::new());
        a.on_key(KeyCode::Char('4'));
        a.on_key(KeyCode::Enter);
        a.on_paste(&"a".repeat(3500));
        assert_eq!(a.on_key(KeyCode::Enter), None);
        assert!(a.factory_confirm.is_none());
        let flash = a.flash.clone().unwrap();
        assert!(flash.contains("the limit is 3000"), "{flash}");
    }
}
