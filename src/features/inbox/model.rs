use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::core::herdr::AgentInfo;
use crate::integrations::github::{Ci, PrInfo, PrState};
use crate::integrations::mastracode::Recap;

pub const MASTRACODE: &str = "mastracode";

/// One Mastra Code agent as of the latest watcher tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub info: AgentInfo,
    pub workspace_label: String,
    pub branch: Option<String>,
    pub pr: Option<PrInfo>,
    pub stalled: Option<Duration>,
    /// The visible screen shows a pending `ask_user` question.
    pub asking: bool,
    /// The Mastra Code thread this pane runs, when known for sure.
    #[serde(default)]
    pub thread: Option<String>,
    /// The thread's recap from Mastra Code's observational memory.
    #[serde(default)]
    pub recap: Option<Recap>,
    /// The pending `ask_user` question, while asking.
    #[serde(default)]
    pub question: Option<String>,
    /// While stalled: the stall tracker's hash of the normalized screen, so a
    /// reply can check the screen has not moved since.
    #[serde(default)]
    pub stall_hash: Option<u64>,
    /// When the status/asking/stalled combination last changed, as unix ms.
    #[serde(default)]
    pub state_since_unix_ms: u64,
}

pub fn is_mastracode(agent: &AgentInfo) -> bool {
    agent.agent.as_deref() == Some(MASTRACODE)
}

/// `6m`, or seconds below a minute.
pub fn duration_text(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// `⏸ 6m`, or seconds below a minute.
pub fn stalled_text(quiet: Duration) -> String {
    format!("⏸ {}", duration_text(quiet))
}

/// Why an item needs attention, in Inbox priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AttentionKind {
    Asking,
    Blocked,
    Stalled,
    CiFailed,
    DoneUnseen,
    MergedCleanup,
}

impl AttentionKind {
    pub fn glyph(self) -> &'static str {
        match self {
            AttentionKind::Asking => "?",
            AttentionKind::Blocked => "!",
            AttentionKind::Stalled => "⏸",
            AttentionKind::CiFailed => "✗",
            AttentionKind::DoneUnseen => "✓",
            AttentionKind::MergedCleanup => "⇣",
        }
    }
}

/// A linked worktree whose PR merged and whose workspace has no agent left:
/// it is offered for cleanup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub kind: AttentionKind,
    pub workspace_id: String,
    pub workspace_label: String,
    /// The checkout path.
    pub title: String,
    pub detail: String,
    pub pr: Option<PrInfo>,
}

impl InboxItem {
    /// A linked worktree whose PR merged; it may have no agent pane at all.
    pub fn merged(
        workspace_id: &str,
        workspace_label: &str,
        checkout_path: &str,
        branch: &str,
        pr: PrInfo,
    ) -> Self {
        Self {
            kind: AttentionKind::MergedCleanup,
            workspace_id: workspace_id.to_string(),
            workspace_label: workspace_label.to_string(),
            title: checkout_path.to_string(),
            detail: format!("{}  {branch}  c cleanup", pr.token()),
            pr: Some(pr),
        }
    }
}

/// Every attention kind that applies to an agent, highest priority first.
/// Drives the tab-bar summary and notifications; the Agents tab uses [`Stage`].
pub fn kinds_of(a: &AgentSnapshot) -> Vec<AttentionKind> {
    let status = a.info.agent_status.as_str();
    let mut kinds = Vec::new();
    if a.asking {
        kinds.push(AttentionKind::Asking);
    }
    if status == "blocked" {
        kinds.push(AttentionKind::Blocked);
    }
    if a.stalled.is_some() {
        kinds.push(AttentionKind::Stalled);
    }
    if a.pr
        .as_ref()
        .is_some_and(|p| p.state == PrState::Open && p.ci == Ci::Fail)
    {
        kinds.push(AttentionKind::CiFailed);
    }
    if status == "done" && !a.asking {
        kinds.push(AttentionKind::DoneUnseen);
    }
    kinds
}

/// Whose move it is, in the Agents tab's display order: what went wrong, then
/// what waits on you, then what finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    NeedsFixing,
    NeedsYou,
    Done,
    Idle,
    Working,
    InReview,
    Merged,
}

impl Stage {
    pub const ALL: [Stage; 7] = [
        Stage::NeedsFixing,
        Stage::NeedsYou,
        Stage::Done,
        Stage::Idle,
        Stage::Working,
        Stage::InReview,
        Stage::Merged,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stage::NeedsYou => "Needs you",
            Stage::Done => "Done",
            Stage::NeedsFixing => "Needs fixing",
            Stage::Idle => "Idle",
            Stage::Working => "Working",
            Stage::InReview => "In review",
            Stage::Merged => "Merged",
        }
    }

    /// The stage's colour in the theme, as herdr-recap paints its stages.
    pub fn color(self) -> ratatui::style::Color {
        let t = crate::features::inbox::theme::theme();
        match self {
            Stage::NeedsYou => t.red,
            Stage::Done => t.teal,
            Stage::NeedsFixing => t.yellow,
            Stage::Idle => t.green,
            Stage::Working => t.peach,
            Stage::InReview => t.blue,
            Stage::Merged => t.mauve,
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Stage::NeedsYou => "?",
            Stage::Done => "✓",
            Stage::NeedsFixing => "✗",
            Stage::Idle => "○",
            Stage::Working => "●",
            Stage::InReview => "◕",
            Stage::Merged => "⇣",
        }
    }
}

/// What the agent is doing right now wins over its PR; first match wins.
pub fn stage_of(a: &AgentSnapshot) -> Stage {
    let status = a.info.agent_status.as_str();
    if a.asking || status == "blocked" {
        return Stage::NeedsYou;
    }
    if status == "done" {
        return Stage::Done;
    }
    if a.stalled.is_some() {
        return Stage::NeedsFixing;
    }
    if status == "working" {
        return Stage::Working;
    }
    match &a.pr {
        Some(p) if p.state == PrState::Open => {
            let failing = p.ci == Ci::Fail
                || p.mergeable.as_deref() == Some("CONFLICTING")
                || p.review_decision.as_deref() == Some("CHANGES_REQUESTED");
            if failing {
                Stage::NeedsFixing
            } else if p.is_draft {
                Stage::Idle
            } else {
                Stage::InReview
            }
        }
        Some(p) if p.state == PrState::Merged => Stage::Merged,
        _ => Stage::Idle,
    }
}

/// The row's own mark: the stage glyph, sharpened for blocked and stalled agents.
pub fn glyph_of(a: &AgentSnapshot) -> &'static str {
    match stage_of(a) {
        Stage::NeedsYou if !a.asking => "!",
        Stage::NeedsFixing if a.stalled.is_some() => "⏸",
        stage => stage.glyph(),
    }
}

/// The agent's thread title (Mastra Code's terminal title without its prefix).
pub fn title_of(a: &AgentSnapshot) -> String {
    match a.info.terminal_title_stripped.as_deref() {
        Some(t) => crate::integrations::mastracode::title_from_terminal(t)
            .unwrap_or(t)
            .to_string(),
        None => a.info.pane_id.clone(),
    }
}

/// `45s`, `6m`, `3h`, `2d`.
pub fn age_text(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// One row of the Agents tab.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Agent(Box<AgentSnapshot>),
    Worktree(Box<InboxItem>),
}

impl Row {
    pub fn stage(&self) -> Stage {
        match self {
            Row::Agent(a) => stage_of(a),
            Row::Worktree(_) => Stage::Merged,
        }
    }

    /// Stable identity across refreshes, so the selection can follow a row.
    pub fn key(&self) -> String {
        match self {
            Row::Agent(a) => format!("pane:{}", a.info.pane_id),
            Row::Worktree(w) => format!("ws:{}", w.workspace_id),
        }
    }

    pub fn pr(&self) -> Option<&PrInfo> {
        match self {
            Row::Agent(a) => a.pr.as_ref(),
            Row::Worktree(w) => w.pr.as_ref(),
        }
    }
}

/// Every agent by stage, longest waiting first (ties by title, then pane), then
/// merged worktrees under Merged by label. A worktree whose workspace still has
/// an agent is left out: cleanup refuses while an agent is there, and the
/// agent's row shows the merged PR already.
pub fn rows(agents: &[AgentSnapshot], merged: &[InboxItem]) -> Vec<Row> {
    let mut agent_rows: Vec<(Stage, &AgentSnapshot, String)> = agents
        .iter()
        .map(|a| (stage_of(a), a, title_of(a)))
        .collect();
    agent_rows.sort_by(|(sx, x, tx), (sy, y, ty)| {
        sx.cmp(sy)
            .then(x.state_since_unix_ms.cmp(&y.state_since_unix_ms))
            .then(tx.cmp(ty))
            .then(x.info.pane_id.cmp(&y.info.pane_id))
    });
    let mut worktrees: Vec<&InboxItem> = merged
        .iter()
        .filter(|m| !agents.iter().any(|a| a.info.workspace_id == m.workspace_id))
        .collect();
    worktrees.sort_by(|x, y| x.workspace_label.cmp(&y.workspace_label));
    // Merged sorts last, so the worktrees follow the merged agents.
    let mut out: Vec<Row> = agent_rows
        .into_iter()
        .map(|(_, a, _)| Row::Agent(Box::new(a.clone())))
        .collect();
    out.extend(
        worktrees
            .into_iter()
            .map(|w| Row::Worktree(Box::new(w.clone()))),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_text_uses_minutes_from_one_minute() {
        assert_eq!(stalled_text(Duration::from_secs(25)), "⏸ 25s");
        assert_eq!(stalled_text(Duration::from_secs(60)), "⏸ 1m");
        assert_eq!(stalled_text(Duration::from_secs(371)), "⏸ 6m");
    }

    fn agent(pane: &str, status: &str, seq: u64) -> AgentSnapshot {
        AgentSnapshot {
            info: serde_json::from_value(serde_json::json!({
                "pane_id": pane, "workspace_id": "w1", "tab_id": "w1:t1",
                "agent": "mastracode", "agent_status": status, "state_change_seq": seq,
            }))
            .unwrap(),
            workspace_label: "proj".into(),
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

    fn pr(state: PrState, ci: Ci) -> Option<PrInfo> {
        Some(PrInfo {
            number: 7,
            state,
            url: "https://x/7".into(),
            head_oid: String::new(),
            ci,
            is_draft: false,
            review_decision: None,
            mergeable: None,
        })
    }

    #[test]
    fn kind_mapping_per_status() {
        assert_eq!(
            kinds_of(&agent("p", "blocked", 1)),
            [AttentionKind::Blocked]
        );
        assert_eq!(
            kinds_of(&agent("p", "done", 1)),
            [AttentionKind::DoneUnseen]
        );
        assert!(kinds_of(&agent("p", "idle", 1)).is_empty());
        assert!(kinds_of(&agent("p", "working", 1)).is_empty());

        let mut asking = agent("p", "done", 1);
        asking.asking = true;
        assert_eq!(kinds_of(&asking), [AttentionKind::Asking]);

        let mut stalled = agent("p", "working", 1);
        stalled.stalled = Some(Duration::from_secs(400));
        assert_eq!(kinds_of(&stalled), [AttentionKind::Stalled]);

        let mut ci = agent("p", "idle", 1);
        ci.pr = pr(PrState::Open, Ci::Fail);
        assert_eq!(kinds_of(&ci), [AttentionKind::CiFailed]);
        ci.pr = pr(PrState::Merged, Ci::Fail);
        assert!(kinds_of(&ci).is_empty());
        ci.pr = pr(PrState::Open, Ci::Pending);
        assert!(kinds_of(&ci).is_empty());
    }

    fn with_pr(status: &str, state: PrState, ci: Ci) -> AgentSnapshot {
        let mut a = agent("p", status, 1);
        a.pr = pr(state, ci);
        a
    }

    #[test]
    fn stage_mapping_first_match_wins() {
        let mut asking = agent("p", "working", 1);
        asking.asking = true;
        let mut stalled = agent("p", "working", 1);
        stalled.stalled = Some(Duration::from_secs(400));
        let mut conflicting = with_pr("idle", PrState::Open, Ci::Pass);
        conflicting.pr.as_mut().unwrap().mergeable = Some("CONFLICTING".into());
        let mut changes = with_pr("idle", PrState::Open, Ci::Pending);
        changes.pr.as_mut().unwrap().review_decision = Some("CHANGES_REQUESTED".into());
        let mut draft = with_pr("idle", PrState::Open, Ci::Pass);
        draft.pr.as_mut().unwrap().is_draft = true;
        let mut failing_draft = with_pr("idle", PrState::Open, Ci::Fail);
        failing_draft.pr.as_mut().unwrap().is_draft = true;
        let mut done_with_pr = with_pr("done", PrState::Open, Ci::Fail);
        done_with_pr.stalled = None;
        let cases = [
            (asking, Stage::NeedsYou),
            (agent("p", "blocked", 1), Stage::NeedsYou),
            (done_with_pr, Stage::Done),
            (stalled, Stage::NeedsFixing),
            (agent("p", "working", 1), Stage::Working),
            // Doing beats the PR.
            (
                with_pr("working", PrState::Merged, Ci::Pass),
                Stage::Working,
            ),
            (with_pr("idle", PrState::Open, Ci::Fail), Stage::NeedsFixing),
            (conflicting, Stage::NeedsFixing),
            (changes, Stage::NeedsFixing),
            (failing_draft, Stage::NeedsFixing),
            (with_pr("idle", PrState::Open, Ci::Pending), Stage::InReview),
            (draft, Stage::Idle),
            (with_pr("idle", PrState::Merged, Ci::Fail), Stage::Merged),
            (with_pr("idle", PrState::Closed, Ci::Pass), Stage::Idle),
            (agent("p", "idle", 1), Stage::Idle),
            (agent("p", "unknown", 1), Stage::Idle),
        ];
        for (a, want) in cases {
            assert_eq!(stage_of(&a), want, "{:?} {:?}", a.info.agent_status, a.pr);
        }
    }

    #[test]
    fn row_glyphs_sharpen_blocked_and_stalled() {
        let mut stalled = agent("p", "working", 1);
        stalled.stalled = Some(Duration::from_secs(400));
        let mut asking = agent("p", "blocked", 1);
        asking.asking = true;
        assert_eq!(glyph_of(&stalled), "⏸");
        assert_eq!(glyph_of(&agent("p", "blocked", 1)), "!");
        assert_eq!(glyph_of(&asking), "?");
        assert_eq!(glyph_of(&with_pr("idle", PrState::Open, Ci::Fail)), "✗");
        assert_eq!(glyph_of(&agent("p", "done", 1)), "✓");
    }

    #[test]
    fn rows_sort_by_stage_then_longest_waiting() {
        let mut a = agent("a", "done", 1);
        a.state_since_unix_ms = 300;
        a.info.terminal_title_stripped = Some("Mastra Code - bbb".into());
        let mut b = agent("b", "done", 1);
        b.state_since_unix_ms = 100;
        let mut c = agent("c", "working", 1);
        c.state_since_unix_ms = 50;
        let mut d = agent("d", "idle", 1);
        d.asking = true;
        d.state_since_unix_ms = 900;
        // Same time: title, then pane id.
        let mut e = agent("e", "done", 1);
        e.state_since_unix_ms = 300;
        e.info.terminal_title_stripped = Some("Mastra Code - aaa".into());
        let mut f = agent("f", "working", 1);
        f.stalled = Some(Duration::from_secs(600));
        f.state_since_unix_ms = 950;
        let mut g = agent("g", "idle", 1);
        g.state_since_unix_ms = 10;
        let order: Vec<String> = rows(&[a, b, c, d, e, f, g], &[])
            .iter()
            .map(Row::key)
            .collect();
        assert_eq!(
            order,
            ["pane:f", "pane:d", "pane:b", "pane:e", "pane:a", "pane:g", "pane:c"],
            "needs fixing, needs you, done by age, idle, working"
        );
    }

    fn merged_item(ws: &str, label: &str) -> InboxItem {
        InboxItem::merged(
            ws,
            label,
            "/wt",
            "feat/x",
            pr(PrState::Merged, Ci::Pass).unwrap(),
        )
    }

    #[test]
    fn worktrees_list_under_merged_after_agents_unless_an_agent_is_there() {
        let merged_agent = with_pr("idle", PrState::Merged, Ci::Pass);
        let idle = agent("i", "idle", 1);
        let items = [merged_item("w9", "zeta"), merged_item("w8", "alpha")];
        let got: Vec<String> = rows(&[merged_agent.clone(), idle.clone()], &items)
            .iter()
            .map(Row::key)
            .collect();
        assert_eq!(got, ["pane:i", "pane:p", "ws:w8", "ws:w9"]);
        assert!(rows(&[], &items).iter().all(|r| r.stage() == Stage::Merged));
        // An agent of any stage in the worktree's workspace hides the row.
        for status in ["done", "working", "idle"] {
            let mut a = agent("x", status, 1);
            a.info.workspace_id = "w9".into();
            let got: Vec<String> = rows(&[a], &items).iter().map(Row::key).collect();
            assert_eq!(got, ["pane:x", "ws:w8"], "{status}");
        }
    }

    #[test]
    fn titles_drop_the_mastra_code_prefix_and_ages_scale() {
        let mut a = agent("w1:p3", "idle", 1);
        assert_eq!(title_of(&a), "w1:p3");
        a.info.terminal_title_stripped = Some("Mastra Code - Fix the build".into());
        assert_eq!(title_of(&a), "Fix the build");
        a.info.terminal_title_stripped = Some("zsh".into());
        assert_eq!(title_of(&a), "zsh");
        assert_eq!(age_text(59_999), "59s");
        assert_eq!(age_text(60_000), "1m");
        assert_eq!(age_text(3_600_000), "1h");
        assert_eq!(age_text(3 * 86_400_000), "3d");
    }
}
