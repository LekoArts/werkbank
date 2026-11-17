//! The Inbox's Issues tab: the viewer's open Linear issues, grouped by status
//! with work in flight first. Fetched by the Inbox worker only while shown.

use std::path::Path;

use anyhow::Result;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use serde::{Deserialize, Serialize};

use crate::features::daemon;
use crate::features::inbox::prs::Checkout;
use crate::features::inbox::theme::theme;
use crate::features::inbox::ui::{cut_spans, dim, fg, fit_spans, truncate, width_of};
use crate::integrations::linear::{MyIssue, MyIssues, TeamState, WorkflowState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    InReview,
    InProgress,
    Todo,
    Triage,
    Backlog,
}

impl Group {
    pub const ALL: [Group; 5] = [
        Group::InReview,
        Group::InProgress,
        Group::Todo,
        Group::Triage,
        Group::Backlog,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::InReview => "In review",
            Group::InProgress => "In progress",
            Group::Todo => "Todo",
            Group::Triage => "Triage",
            Group::Backlog => "Backlog",
        }
    }

    /// herdr-linear's colours: review mauve, progress yellow, todo blue.
    pub fn color(self) -> Color {
        let t = theme();
        match self {
            Group::InReview => t.mauve,
            Group::InProgress => t.yellow,
            Group::Todo => t.blue,
            Group::Triage => t.peach,
            Group::Backlog => t.overlay,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Group::InReview => "◕",
            Group::InProgress => "◔",
            Group::Todo => "○",
            Group::Triage => "◇",
            Group::Backlog => "◌",
        }
    }
}

/// `None` for a closed state: such an issue (e.g. just moved to Done) is not listed.
pub fn group_of(state: &WorkflowState) -> Option<Group> {
    match state.kind.as_str() {
        "started" if state.name.to_lowercase().contains("review") => Some(Group::InReview),
        "started" => Some(Group::InProgress),
        "unstarted" => Some(Group::Todo),
        "triage" => Some(Group::Triage),
        "backlog" => Some(Group::Backlog),
        _ => None,
    }
}

fn icon_style(state: &WorkflowState) -> Style {
    fg(group_of(state).map_or(theme().green, Group::color))
}

fn priority_color(p: u8) -> Color {
    let t = theme();
    match p {
        1 => t.red,
        2 => t.peach,
        3 => t.yellow,
        _ => t.overlay,
    }
}

pub fn icon_of(state: &WorkflowState) -> &'static str {
    group_of(state).map_or("✓", Group::icon)
}

pub fn priority_text(p: u8) -> Option<&'static str> {
    match p {
        1 => Some("urgent"),
        2 => Some("high"),
        3 => Some("medium"),
        4 => Some("low"),
        _ => None,
    }
}

/// Urgent first, no priority last.
fn priority_rank(p: u8) -> u8 {
    if p == 0 {
        u8::MAX
    } else {
        p
    }
}

/// What the Issues tab shows and caches in `issues-<session>.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IssueData {
    pub mine: MyIssues,
    pub checkouts: Vec<Checkout>,
    pub fetched_unix_ms: u64,
}

/// A cache from an older binary that no longer parses is deleted, not an error.
pub fn load_cache(path: &Path) -> Option<IssueData> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(d) => Some(d),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

pub fn save_cache(path: &Path, data: &IssueData) -> Result<()> {
    daemon::write_atomic(path, &serde_json::to_vec(data)?)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Item<'a> {
    Heading(Group, usize),
    Issue(&'a MyIssue),
}

/// Every group in order with its count (also 0), then its issues by priority,
/// ties by identifier.
pub fn items(mine: &MyIssues) -> Vec<Item<'_>> {
    let mut out = Vec::new();
    for g in Group::ALL {
        let mut list: Vec<&MyIssue> = mine
            .issues
            .iter()
            .filter(|i| group_of(&i.state) == Some(g))
            .collect();
        list.sort_by(|a, b| {
            priority_rank(a.priority)
                .cmp(&priority_rank(b.priority))
                .then(a.identifier.cmp(&b.identifier))
        });
        out.push(Item::Heading(g, list.len()));
        out.extend(list.into_iter().map(Item::Issue));
    }
    out
}

/// The issues in display order (headings dropped), for selection.
pub fn issues(mine: &MyIssues) -> Vec<&MyIssue> {
    items(mine)
        .into_iter()
        .filter_map(|i| match i {
            Item::Issue(i) => Some(i),
            Item::Heading(..) => None,
        })
        .collect()
}

/// `100+` when Linear has more than the first page.
pub fn header(mine: &MyIssues) -> String {
    let more = if mine.more { "+" } else { "" };
    format!("My issues {}{more}", issues(mine).len())
}

/// The open checkout on the issue's branch. An issue names no repo, so two
/// checkouts on that branch (in different repos) match none rather than a guess.
pub fn checkout_for<'a>(checkouts: &'a [Checkout], issue: &MyIssue) -> Option<&'a Checkout> {
    let branch = issue.branch_name.as_deref()?;
    let mut on_branch = checkouts.iter().filter(|c| c.branch == branch);
    let only = on_branch.next()?;
    on_branch.next().is_none().then_some(only)
}

/// Columns the title keeps before labels are dropped from a row.
const MIN_TITLE: usize = 30;

/// One line, the identifier first so it is always on the marker line.
pub fn row_line(issue: &MyIssue, checkouts: &[Checkout], width: usize) -> Line<'static> {
    let t = theme();
    let left = vec![
        (
            format!("{} ", icon_of(&issue.state)),
            icon_style(&issue.state),
        ),
        (format!("{} ", issue.identifier), fg(t.green)),
        (issue.title.clone(), fg(t.text).add_modifier(Modifier::BOLD)),
    ];
    let parts = |labels: bool| {
        let mut parts: Vec<(String, Style)> = Vec::new();
        if let Some(p) = priority_text(issue.priority) {
            parts.push((p.to_string(), fg(priority_color(issue.priority))));
        }
        if labels && !issue.labels.is_empty() {
            parts.push((issue.labels.join(", "), fg(t.mauve)));
        }
        if let Some(p) = &issue.project {
            parts.push((p.name.clone(), fg(t.subtext)));
        }
        if let Some(c) = checkout_for(checkouts, issue) {
            parts.push((format!("worktree {}", c.label), fg(t.teal)));
        }
        let mut right = Vec::new();
        for (i, part) in parts.into_iter().enumerate() {
            if i > 0 {
                right.push((" · ".to_string(), dim()));
            }
            right.push(part);
        }
        right
    };
    // Labels give way first, so the title keeps room to be read.
    let mut right = parts(true);
    let used: usize = right.iter().map(|(s, _)| width_of(s)).sum();
    if used + MIN_TITLE >= width {
        right = parts(false);
    }
    fit_spans(left, right, width)
}

/// The issue screen: the list row's data at once, the detail once it arrives.
pub fn detail_lines(
    listed: &MyIssue,
    detail: Option<&MyIssue>,
    loading: Option<&str>,
    checkouts: &[Checkout],
    width: usize,
) -> Vec<Line<'static>> {
    let i = detail.unwrap_or(listed);
    let cut = |s: String| truncate(&s, width);
    let mut lines = vec![Line::styled(
        cut(format!(
            "{} {} {}",
            icon_of(&i.state),
            i.identifier,
            i.title
        )),
        fg(theme().text).add_modifier(Modifier::BOLD),
    )];
    let mut status = vec![i.state.name.clone()];
    status.extend(priority_text(i.priority).map(String::from));
    status.extend(i.project.as_ref().map(|p| p.name.clone()));
    lines.push(Line::styled(cut(status.join(" · ")), icon_style(&i.state)));
    let who = i
        .assignee
        .as_ref()
        .map_or("unassigned".to_string(), |a| a.name.clone());
    lines.push(Line::from(cut(format!("assignee: {who}"))));
    if !i.labels.is_empty() {
        lines.push(Line::from(cut(format!("labels: {}", i.labels.join(", ")))));
    }
    let mut branch = format!("branch: {}", i.branch_name.as_deref().unwrap_or("(none)"));
    if let Some(c) = checkout_for(checkouts, i) {
        branch.push_str(&format!(" · worktree {}", c.label));
    }
    lines.push(Line::styled(cut(branch), dim()));
    lines.push(Line::from(""));
    match (detail, loading) {
        (_, Some(msg)) => lines.push(Line::styled(cut(msg.to_string()), dim())),
        (Some(d), None) => match d.description.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => {
                lines.extend(text.lines().map(|l| Line::from(l.trim_end().to_string())))
            }
            _ => lines.push(Line::styled("no description", dim())),
        },
        (None, None) => {}
    }
    lines
}

/// The `c` picker's rows: the team's states by position, the current one marked.
pub fn state_lines(states: &[TeamState], current: &str, width: usize) -> Vec<Line<'static>> {
    states
        .iter()
        .map(|s| {
            let state = WorkflowState {
                id: s.id.clone(),
                name: s.name.clone(),
                kind: s.kind.clone(),
            };
            let mut parts = vec![
                (format!("{} ", icon_of(&state)), icon_style(&state)),
                (s.name.clone(), fg(theme().text)),
            ];
            if s.id == current {
                parts.push((" (current)".to_string(), fg(theme().accent)));
            }
            Line::from(cut_spans(parts, width))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::linear::parse_my_issues;

    const MINE: &str = include_str!("../../../tests/fixtures/linear/my-issues.json");

    fn mine() -> MyIssues {
        parse_my_issues(MINE).unwrap()
    }

    fn state(kind: &str, name: &str) -> WorkflowState {
        WorkflowState {
            id: format!("st-{name}"),
            name: name.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn groups_and_icons() {
        let cases = [
            (state("started", "In Review"), Some(Group::InReview), "◕"),
            (state("started", "Code review"), Some(Group::InReview), "◕"),
            (
                state("started", "In Progress"),
                Some(Group::InProgress),
                "◔",
            ),
            (state("unstarted", "Todo"), Some(Group::Todo), "○"),
            (state("triage", "Triage"), Some(Group::Triage), "◇"),
            (state("backlog", "Backlog"), Some(Group::Backlog), "◌"),
            (state("completed", "Done"), None, "✓"),
            (state("canceled", "Canceled"), None, "✓"),
        ];
        for (s, g, icon) in cases {
            assert_eq!(group_of(&s), g, "{}", s.name);
            assert_eq!(icon_of(&s), icon, "{}", s.name);
        }
    }

    #[test]
    fn in_flight_first_then_by_priority() {
        let m = mine();
        let order: Vec<&str> = issues(&m).iter().map(|i| i.identifier.as_str()).collect();
        // review, progress, todo (urgent, low, none), triage, backlog
        assert_eq!(
            order,
            ["ENG-5", "ENG-4", "ENG-2", "ENG-3", "ENG-7", "ENG-6", "ENG-1"]
        );
        let headings: Vec<String> = items(&m)
            .into_iter()
            .filter_map(|i| match i {
                Item::Heading(g, n) => Some(format!("{} ({n})", g.label())),
                Item::Issue(_) => None,
            })
            .collect();
        assert_eq!(
            headings,
            [
                "In review (1)",
                "In progress (1)",
                "Todo (3)",
                "Triage (1)",
                "Backlog (1)"
            ]
        );
        assert_eq!(header(&m), "My issues 7");
        assert!(!m.more, "the fixture's pageInfo says no next page");
        let mut capped = m.clone();
        capped.more = true;
        assert_eq!(header(&capped), "My issues 7+");
    }

    #[test]
    fn a_branch_checked_out_in_two_repos_matches_no_checkout() {
        let m = mine();
        let i = m.issues.iter().find(|i| i.identifier == "ENG-1").unwrap();
        let at = |repo: &str| Checkout {
            workspace_id: repo.into(),
            label: repo.into(),
            path: format!("/{repo}"),
            repo: repo.into(),
            branch: "user/eng-1-x".into(),
            main: false,
        };
        assert_eq!(
            checkout_for(&[at("o/a")], i).map(|c| c.repo.as_str()),
            Some("o/a")
        );
        assert!(checkout_for(&[at("o/a"), at("o/b")], i).is_none());
    }

    #[test]
    fn empty_groups_still_head_and_closed_issues_drop_out() {
        let mut m = mine();
        m.issues.retain(|i| i.identifier == "ENG-1");
        m.issues[0].state = state("completed", "Done");
        let items = items(&m);
        assert_eq!(items.len(), 5, "five headings, no rows");
        assert!(items.iter().all(|i| matches!(i, Item::Heading(_, 0))));
    }

    #[test]
    fn row_puts_the_identifier_first() {
        let m = mine();
        let i = m.issues.iter().find(|i| i.identifier == "ENG-1").unwrap();
        let checkouts = [Checkout {
            workspace_id: "w1".into(),
            label: "eng-1".into(),
            path: "/x".into(),
            repo: "o/r".into(),
            branch: "user/eng-1-x".into(),
            main: false,
        }];
        let line = row_line(i, &checkouts, 100).to_string();
        assert!(line.starts_with("◌ ENG-1 Backlog item"), "{line}");
        assert!(
            line.ends_with("docs, good first issue · Documentation · worktree eng-1"),
            "{line}"
        );
        let tight = row_line(i, &checkouts, 70).to_string();
        assert!(tight.contains("Backlog item"), "{tight}");
        assert!(!tight.contains("docs"), "labels give way first: {tight}");
        assert!(tight.ends_with("Documentation · worktree eng-1"), "{tight}");
        let narrow = row_line(i, &checkouts, 20).to_string();
        assert!(narrow.starts_with("◌ ENG-1 "), "{narrow}");
        let urgent = m.issues.iter().find(|i| i.identifier == "ENG-2").unwrap();
        assert!(row_line(urgent, &[], 60).to_string().ends_with("urgent"));
    }

    #[test]
    fn detail_shows_description_once_loaded() {
        let m = mine();
        let listed = &m.issues[0];
        let lines = |d: Option<&MyIssue>, l: Option<&str>| -> Vec<String> {
            detail_lines(listed, d, l, &[], 60)
                .iter()
                .map(|l| l.to_string())
                .collect()
        };
        let loading = lines(None, Some("loading…"));
        assert_eq!(loading[0], "◌ ENG-1 Backlog item");
        assert_eq!(loading[1], "Backlog · Documentation");
        assert_eq!(loading[2], "assignee: User One");
        assert_eq!(loading.last().unwrap(), "loading…");
        let mut full = listed.clone();
        full.description = Some("Write it.\nTwice.".into());
        full.labels = vec!["docs".into()];
        let done = lines(Some(&full), None);
        assert!(done.contains(&"labels: docs".to_string()));
        assert_eq!(&done[done.len() - 2..], ["Write it.", "Twice."]);
        full.description = None;
        assert_eq!(lines(Some(&full), None).last().unwrap(), "no description");
    }

    #[test]
    fn cache_round_trips_and_a_bad_one_is_deleted() {
        let dir = std::env::temp_dir().join(format!("wb-issues-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("issues-s.json");
        let data = IssueData {
            mine: mine(),
            checkouts: Vec::new(),
            fetched_unix_ms: 7,
        };
        save_cache(&path, &data).unwrap();
        assert_eq!(load_cache(&path), Some(data));
        std::fs::write(&path, b"{\"old\":1}").unwrap();
        assert_eq!(load_cache(&path), None);
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_picker_marks_the_current_state() {
        let states = vec![
            TeamState {
                id: "a".into(),
                name: "Todo".into(),
                kind: "unstarted".into(),
                position: 1.0,
            },
            TeamState {
                id: "b".into(),
                name: "In Progress".into(),
                kind: "started".into(),
                position: 2.0,
            },
        ];
        let lines: Vec<String> = state_lines(&states, "b", 40)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines, ["○ Todo", "◔ In Progress (current)"]);
    }
}
