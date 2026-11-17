//! The Inbox's PRs tab: the viewer's open PRs (authored or assigned), grouped
//! by whose move it is. Fetched by the Inbox worker only while the tab is shown.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use serde::{Deserialize, Serialize};

use crate::core::herdr::Herdr;
use crate::core::runner::Runner;
use crate::core::stop::Guard;
use crate::features::daemon;
use crate::features::inbox::model::age_text;
use crate::features::inbox::theme::theme;
use crate::features::inbox::ui::{dim, fg, fit_spans, meta_spans, truncate};
use crate::features::launch::issue::origin_repo;
use crate::integrations::github::{Ci, ListPr, PrDetail, PrLists};
use crate::integrations::mastracode::iso_unix_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    NeedsFixing,
    ReadyToMerge,
    Waiting,
    Drafts,
}

impl Group {
    pub fn color(self) -> Color {
        let t = theme();
        match self {
            Group::NeedsFixing => t.red,
            Group::ReadyToMerge => t.green,
            Group::Waiting => t.blue,
            Group::Drafts => t.overlay,
        }
    }

    pub const ALL: [Group; 4] = [
        Group::NeedsFixing,
        Group::ReadyToMerge,
        Group::Waiting,
        Group::Drafts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::NeedsFixing => "Needs fixing",
            Group::ReadyToMerge => "Ready to merge",
            Group::Waiting => "Waiting",
            Group::Drafts => "Drafts",
        }
    }
}

/// First match wins: the viewer's move when it needs fixing or can merge.
pub fn group_of(pr: &ListPr) -> Group {
    if pr.is_draft {
        return Group::Drafts;
    }
    let decision = pr.review_decision.as_deref();
    if pr.ci == Ci::Fail
        || pr.mergeable.as_deref() == Some("CONFLICTING")
        || decision == Some("CHANGES_REQUESTED")
    {
        Group::NeedsFixing
    } else if decision == Some("APPROVED") || (decision.is_none() && pr.ci == Ci::Pass) {
        Group::ReadyToMerge
    } else {
        Group::Waiting
    }
}

/// An open Herdr workspace's checkout, for the worktree/agent hints and `w`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub workspace_id: String,
    pub label: String,
    pub path: String,
    /// `owner/name` of its `origin`.
    pub repo: String,
    pub branch: String,
    /// The repo's main checkout, not a linked worktree.
    pub main: bool,
}

/// What the PRs tab shows and caches in `prs-<session>.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PrData {
    pub lists: PrLists,
    pub checkouts: Vec<Checkout>,
    pub fetched_unix_ms: u64,
}

/// A cached list from an older binary that no longer parses is deleted, not an error.
pub fn load_cache(path: &Path) -> Option<PrData> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(d) => Some(d),
        Err(_) => {
            let _ = std::fs::remove_file(path);
            None
        }
    }
}

pub fn save_cache(path: &Path, data: &PrData) -> Result<()> {
    daemon::write_atomic(path, &serde_json::to_vec(data)?)
}

/// `(toplevel, branch, main)` of the checkout at `path`, in one git call.
fn head_of<R: Runner>(runner: &R, path: &str) -> Option<(String, String, bool)> {
    let out = runner
        .run(
            &[
                "git",
                "-C",
                path,
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
                "--abbrev-ref",
                "HEAD",
            ],
            None,
            None,
        )
        .ok()
        .filter(|o| o.success())?;
    let mut lines = out.stdout.lines().map(str::trim);
    let (top, git_dir, common, branch) =
        (lines.next()?, lines.next()?, lines.next()?, lines.next()?);
    Some((top.to_string(), branch.to_string(), git_dir == common))
}

/// Every open workspace's checkout: one `workspace list`, then per checkout
/// one branch read and one `origin` read, memoized by path for this call.
pub fn checkouts<R: Runner>(herdr: &Herdr<R>, guard: Guard) -> Result<Vec<Checkout>> {
    guard()?;
    let workspaces = herdr.workspace_list()?;
    let mut panes = None;
    let mut memo: HashMap<String, Option<(String, String, bool, String)>> = HashMap::new();
    let mut out = Vec::new();
    for ws in &workspaces {
        let path = match &ws.worktree {
            Some(wt) => Some(wt.checkout_path.clone()),
            None => {
                if panes.is_none() {
                    guard()?;
                    panes = Some(herdr.pane_list()?);
                }
                panes.iter().flatten().find_map(|p| {
                    (p.workspace_id == ws.workspace_id)
                        .then(|| p.foreground_cwd.clone().or(p.cwd.clone()))
                        .flatten()
                })
            }
        };
        let Some(path) = path else { continue };
        if !memo.contains_key(&path) {
            guard()?;
            let found = head_of(&herdr.runner, &path).and_then(|(top, branch, main)| {
                let repo = origin_repo(&herdr.runner, &top)?;
                Some((top, branch, main, repo))
            });
            memo.insert(path.clone(), found);
        }
        if let Some((top, branch, main, repo)) = memo[&path].clone() {
            out.push(Checkout {
                workspace_id: ws.workspace_id.clone(),
                label: ws.label.clone(),
                path: top,
                repo,
                branch,
                main,
            });
        }
    }
    Ok(out)
}

/// The open checkout of this PR's head branch in the same repo (a fork's
/// same-named branch never matches); a linked worktree before the main checkout.
pub fn checkout_for<'a>(checkouts: &'a [Checkout], pr: &ListPr) -> Option<&'a Checkout> {
    checkout_of(checkouts, &pr.repo, pr.head_repo.as_deref(), &pr.head_ref)
}

pub fn checkout_of<'a>(
    checkouts: &'a [Checkout],
    repo: &str,
    head_repo: Option<&str>,
    head: &str,
) -> Option<&'a Checkout> {
    if !head_repo.is_some_and(|h| h.eq_ignore_ascii_case(repo)) {
        return None;
    }
    checkouts
        .iter()
        .filter(|c| c.repo.eq_ignore_ascii_case(repo) && c.branch == head)
        .min_by_key(|c| c.main)
}

/// One line of the PRs list: a group heading, or a PR.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Item<'a> {
    Heading(Group, usize),
    Pr(&'a ListPr),
}

/// Own PRs, then assigned PRs not already listed as own.
fn all(l: &PrLists) -> Vec<&ListPr> {
    let assigned = l
        .assigned
        .iter()
        .filter(|a| !l.mine.iter().any(|m| m.id == a.id));
    l.mine.iter().chain(assigned).collect()
}

/// `(own rows, assigned rows)` as rendered.
pub fn counts(l: &PrLists) -> (usize, usize) {
    let mine = l.mine.len();
    (mine, all(l).len() - mine)
}

/// Every group in order, each heading with its count (also 0), then its PRs
/// newest first, ties by number.
pub fn items(l: &PrLists) -> Vec<Item<'_>> {
    let rows = all(l);
    let mut out = Vec::new();
    for g in Group::ALL {
        let mut prs: Vec<&ListPr> = rows.iter().copied().filter(|p| group_of(p) == g).collect();
        prs.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then(a.number.cmp(&b.number))
        });
        out.push(Item::Heading(g, prs.len()));
        out.extend(prs.into_iter().map(Item::Pr));
    }
    out
}

/// The PRs in display order (headings dropped), for selection.
pub fn prs(l: &PrLists) -> Vec<&ListPr> {
    items(l)
        .into_iter()
        .filter_map(|i| match i {
            Item::Pr(p) => Some(p),
            Item::Heading(..) => None,
        })
        .collect()
}

pub fn header(l: &PrLists) -> String {
    let (mine, assigned) = counts(l);
    // A search past its first page says how many there are.
    let of = |shown: usize, total: u64| {
        if total > shown as u64 {
            format!(" of {total}")
        } else {
            String::new()
        }
    };
    format!(
        "Mine {mine}{} · Assigned {assigned}{}",
        of(l.mine.len(), l.mine_total),
        of(l.assigned.len(), l.assigned_total)
    )
}

fn mark(pr: &ListPr) -> &'static str {
    if pr.is_draft {
        "◌"
    } else {
        "●"
    }
}

fn short_repo(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

fn updated(pr: &ListPr, now_ms: u64) -> Option<String> {
    iso_unix_ms(&pr.updated_at).map(|t| age_text(now_ms.saturating_sub(t)))
}

fn ci_color(ci: Ci) -> Color {
    let t = theme();
    match ci {
        Ci::Pass => t.green,
        Ci::Fail => t.red,
        Ci::Pending => t.yellow,
        Ci::None => t.overlay,
    }
}

fn decision_color(d: &str) -> Color {
    let t = theme();
    match d {
        "approved" => t.green,
        "changes requested" => t.red,
        _ => t.yellow,
    }
}

fn decision_text(d: Option<&str>) -> Option<&'static str> {
    match d? {
        "APPROVED" => Some("approved"),
        "CHANGES_REQUESTED" => Some("changes requested"),
        "REVIEW_REQUIRED" => Some("review required"),
        _ => None,
    }
}

fn checks_text(ci: Ci) -> Option<&'static str> {
    match ci {
        Ci::Pass => Some("✓ checks"),
        Ci::Fail => Some("✗ checks"),
        Ci::Pending => Some("● checks"),
        Ci::None => None,
    }
}

/// Two lines; line 2 leads with what matters so truncation drops the tail.
pub fn row_lines(
    pr: &ListPr,
    checkouts: &[Checkout],
    agent_workspaces: &[&str],
    width: usize,
    now_ms: u64,
) -> Vec<Line<'static>> {
    let t = theme();
    let left = vec![
        (format!("{} ", mark(pr)), fg(group_of(pr).color())),
        (format!("#{} ", pr.number), fg(t.green)),
        (pr.title.clone(), fg(t.text).add_modifier(Modifier::BOLD)),
    ];
    let mut right = vec![(
        format!("{} · {}", pr.author, short_repo(&pr.repo)),
        fg(t.subtext),
    )];
    if let Some(age) = updated(pr, now_ms) {
        right.push((format!(" · {age}"), dim()));
    }
    let mut meta: Vec<(String, Style)> = Vec::new();
    if pr.merge_state.as_deref() == Some("DIRTY") || pr.mergeable.as_deref() == Some("CONFLICTING")
    {
        meta.push(("! conflicts".into(), fg(t.red)));
    } else if pr.merge_state.as_deref() == Some("BEHIND") {
        meta.push(("! behind base".into(), fg(t.yellow)));
    }
    if let Some(r) = &pr.your_review {
        meta.push((format!("your review: {}", r.text), fg(t.yellow)));
    }
    meta.extend(checks_text(pr.ci).map(|c| (c.to_string(), fg(ci_color(pr.ci)))));
    meta.extend(
        decision_text(pr.review_decision.as_deref())
            .map(|d| (d.to_string(), fg(decision_color(d)))),
    );
    meta.push((pr.head_ref.clone(), fg(t.mauve)));
    if let Some(c) = checkout_for(checkouts, pr) {
        meta.push((format!("worktree {}", c.label), fg(t.teal)));
        if agent_workspaces.contains(&c.workspace_id.as_str()) {
            meta.push(("agent".into(), fg(t.peach)));
        }
    }
    vec![fit_spans(left, right, width), meta_spans("  ", meta, width)]
}

/// GitHub's merge gate, then why it is held.
pub fn merge_line(pr: &ListPr) -> String {
    let gate = match pr.merge_state.as_deref() {
        Some("CLEAN" | "HAS_HOOKS") => "ready",
        Some("UNSTABLE") => "possible, checks not passing",
        Some("BLOCKED") => "blocked",
        Some("BEHIND") => "behind base",
        Some("DIRTY") => "conflicts",
        Some("DRAFT") => "draft",
        _ => "unknown",
    };
    let mut parts = vec![format!("merge: {gate}")];
    match pr.ci {
        Ci::Fail => parts.push("checks failing".into()),
        Ci::Pending => parts.push("checks running".into()),
        _ => {}
    }
    if pr.mergeable.as_deref() == Some("CONFLICTING") && gate != "conflicts" {
        parts.push("conflicts".into());
    }
    parts.extend(
        decision_text(pr.review_decision.as_deref())
            .filter(|d| *d != "approved")
            .map(String::from),
    );
    parts.join(" · ")
}

fn reviewer_glyph(state: &str) -> &'static str {
    match state {
        "APPROVED" => "✓",
        "CHANGES_REQUESTED" => "✗",
        "REQUESTED" => "○",
        "DISMISSED" => "⊘",
        _ => "•",
    }
}

/// The PR screen: the list row's data at once, the detail once it arrives.
pub fn detail_lines(
    listed: &ListPr,
    detail: Option<&PrDetail>,
    loading: Option<&str>,
    checkouts: &[Checkout],
    width: usize,
    now_ms: u64,
) -> Vec<Line<'static>> {
    let pr = detail.map_or(listed, |d| &d.pr);
    let cut = |s: String| truncate(&s, width);
    let mut title = format!("{} #{} {}", mark(pr), pr.number, pr.title);
    if pr.is_draft {
        title.push_str(" · draft");
    }
    let mut lines = vec![Line::styled(
        cut(title),
        fg(theme().text).add_modifier(Modifier::BOLD),
    )];
    let mut by = format!("{} · {}", pr.author, pr.repo);
    if let Some(age) = updated(pr, now_ms) {
        by.push_str(&format!(" · updated {age}"));
    }
    lines.push(Line::styled(cut(by), dim()));
    let merge = merge_line(pr);
    let style = if pr.ci == Ci::Fail || merge.contains("conflicts") {
        fg(theme().red)
    } else if merge == "merge: ready" {
        fg(theme().green)
    } else {
        fg(theme().yellow)
    };
    lines.push(Line::styled(cut(merge), style));
    if let Some(d) = detail {
        if !d.failing.is_empty() {
            lines.push(Line::styled(
                cut(format!("failing: {}", d.failing.join(", "))),
                fg(theme().red),
            ));
        }
        if d.pending > 0 {
            lines.push(Line::from(cut(format!("{} checks running", d.pending))));
        }
    }
    if !pr.reviewers.is_empty() {
        let who: Vec<String> = pr
            .reviewers
            .iter()
            .map(|r| {
                let bot = if r.bot { " (bot)" } else { "" };
                format!("{} {}{bot}", reviewer_glyph(&r.state), r.login)
            })
            .collect();
        lines.push(Line::from(cut(format!("reviewers: {}", who.join("  ")))));
    }
    if let Some(r) = &pr.your_review {
        lines.push(Line::styled(
            cut(format!("your review: {}", r.text)),
            fg(theme().yellow),
        ));
    }
    let mut branch = pr.head_ref.clone();
    if let Some(d) = detail.filter(|d| !d.base_ref.is_empty()) {
        branch = format!("{branch} → {}", d.base_ref);
    }
    if let Some(c) = checkout_for(checkouts, pr) {
        branch.push_str(&format!(" · worktree {}", c.label));
    }
    lines.push(Line::styled(cut(branch), fg(theme().mauve)));
    lines.push(Line::from(""));
    match (detail, loading) {
        (_, Some(msg)) => lines.push(Line::styled(cut(msg.to_string()), dim())),
        (Some(d), None) if !d.body.trim().is_empty() => {
            lines.extend(d.body.lines().map(|l| Line::from(l.trim_end().to_string())));
        }
        (Some(_), None) => lines.push(Line::styled("no description", dim())),
        (None, None) => {}
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::FakeRunner;
    use crate::integrations::github::{parse_lists, YourReview};

    const LISTS: &str = include_str!("../../../tests/fixtures/gh/pr-lists.json");

    fn pr(n: u64) -> ListPr {
        ListPr {
            id: format!("PR_{n}"),
            number: n,
            title: format!("PR {n}"),
            url: format!("https://github.com/o/r/pull/{n}"),
            is_draft: false,
            author: "user1".into(),
            author_bot: false,
            repo: "o/r".into(),
            head_ref: format!("feat/{n}"),
            head_repo: Some("o/r".into()),
            review_decision: None,
            merge_state: None,
            mergeable: None,
            updated_at: "2026-11-12T10:00:00Z".into(),
            ci: Ci::None,
            reviewers: vec![],
            your_review: None,
        }
    }

    #[test]
    fn groups_follow_whose_move_it_is() {
        let draft = ListPr {
            is_draft: true,
            ci: Ci::Fail,
            ..pr(1)
        };
        assert_eq!(group_of(&draft), Group::Drafts);
        let failing = ListPr {
            ci: Ci::Fail,
            review_decision: Some("APPROVED".into()),
            ..pr(1)
        };
        assert_eq!(group_of(&failing), Group::NeedsFixing);
        let conflict = ListPr {
            mergeable: Some("CONFLICTING".into()),
            ..pr(1)
        };
        assert_eq!(group_of(&conflict), Group::NeedsFixing);
        let changes = ListPr {
            review_decision: Some("CHANGES_REQUESTED".into()),
            ..pr(1)
        };
        assert_eq!(group_of(&changes), Group::NeedsFixing);
        let approved = ListPr {
            review_decision: Some("APPROVED".into()),
            ci: Ci::Pending,
            ..pr(1)
        };
        assert_eq!(group_of(&approved), Group::ReadyToMerge);
        let green = ListPr {
            ci: Ci::Pass,
            ..pr(1)
        };
        assert_eq!(group_of(&green), Group::ReadyToMerge);
        let required = ListPr {
            ci: Ci::Pass,
            review_decision: Some("REVIEW_REQUIRED".into()),
            ..pr(1)
        };
        assert_eq!(group_of(&required), Group::Waiting);
        assert_eq!(group_of(&pr(1)), Group::Waiting);
    }

    #[test]
    fn a_pr_both_authored_and_assigned_is_listed_once() {
        let mut l = PrLists {
            viewer: "me".into(),
            mine: vec![ListPr {
                ci: Ci::Fail,
                ..pr(1)
            }],
            assigned: vec![pr(1), pr(2)],
            mine_total: 1,
            assigned_total: 2,
        };
        assert_eq!(counts(&l), (1, 1));
        let listed = prs(&l);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].ci, Ci::Fail, "the own copy is kept");
        l.mine.clear();
        l.mine_total = 0;
        assert_eq!(counts(&l), (0, 2));
        assert_eq!(header(&l), "Mine 0 · Assigned 2");
        l.mine = vec![pr(3)];
        l.mine_total = 60;
        l.assigned_total = 70;
        assert_eq!(header(&l), "Mine 1 of 60 · Assigned 2 of 70");
    }

    #[test]
    fn every_heading_renders_with_its_count_and_prs_sort_newest_first() {
        let mut a = pr(5);
        a.updated_at = "2026-11-12T09:00:00Z".into();
        let b = pr(9);
        let c = pr(3);
        let l = PrLists {
            viewer: "me".into(),
            assigned: vec![a, b, c],
            assigned_total: 3,
            ..Default::default()
        };
        let it = items(&l);
        let headings: Vec<(Group, usize)> = it
            .iter()
            .filter_map(|i| match i {
                Item::Heading(g, n) => Some((*g, *n)),
                _ => None,
            })
            .collect();
        assert_eq!(
            headings,
            vec![
                (Group::NeedsFixing, 0),
                (Group::ReadyToMerge, 0),
                (Group::Waiting, 3),
                (Group::Drafts, 0)
            ]
        );
        let order: Vec<u64> = prs(&l).iter().map(|p| p.number).collect();
        assert_eq!(order, vec![3, 9, 5], "same time: by number; older last");
    }

    #[test]
    fn the_fixture_groups_by_whose_move_it_is() {
        let l = parse_lists(LISTS).unwrap();
        let group = |n: u64| group_of(prs(&l).into_iter().find(|p| p.number == n).unwrap());
        assert_eq!(group(40001), Group::NeedsFixing);
        assert_eq!(group(40002), Group::ReadyToMerge);
    }

    fn checkout(repo: &str, branch: &str, main: bool) -> Checkout {
        Checkout {
            workspace_id: format!("w-{repo}-{branch}-{main}"),
            label: branch.into(),
            path: format!("/tmp/{repo}"),
            repo: repo.into(),
            branch: branch.into(),
            main,
        }
    }

    #[test]
    fn association_needs_the_same_repo_and_branch_and_skips_forks() {
        let p = pr(7);
        let other = checkout("x/y", "feat/7", false);
        assert_eq!(checkout_for(std::slice::from_ref(&other), &p), None);
        let main = checkout("O/R", "feat/7", true);
        let linked = checkout("o/r", "feat/7", false);
        let cs = vec![other, main, linked.clone()];
        assert_eq!(checkout_for(&cs, &p), Some(&linked));
        let fork = ListPr {
            head_repo: Some("someone/r".into()),
            ..pr(7)
        };
        assert_eq!(checkout_for(&cs, &fork), None);
    }

    #[test]
    fn checkouts_read_each_path_once() {
        let f = FakeRunner::new();
        let herdr = Herdr::with_bin(f, "herdr");
        let ws = serde_json::json!({"result":{"workspaces":[
            {"workspace_id":"w1","label":"a","worktree":{"checkout_path":"/c/a","repo_root":"/c/a","is_linked_worktree":false,"repo_name":"a"}},
            {"workspace_id":"w2","label":"a again","worktree":{"checkout_path":"/c/a","repo_root":"/c/a","is_linked_worktree":false,"repo_name":"a"}}
        ]}});
        herdr
            .runner
            .ok(&["herdr", "workspace", "list"], None, &ws.to_string());
        herdr.runner.ok(
            &[
                "git",
                "-C",
                "/c/a",
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
                "--abbrev-ref",
                "HEAD",
            ],
            None,
            "/c/a\n/c/a/.git\n/c/a/.git\nfeat/7\n",
        );
        herdr.runner.ok(
            &["git", "-C", "/c/a", "remote", "get-url", "origin"],
            None,
            "git@github.com:o/r.git\n",
        );
        let cs = checkouts(&herdr, &|| Ok(())).unwrap();
        assert_eq!(cs.len(), 2);
        assert!(cs
            .iter()
            .all(|c| c.repo == "o/r" && c.branch == "feat/7" && c.main));
        let git = herdr
            .runner
            .argvs()
            .iter()
            .filter(|a| a.starts_with("git"))
            .count();
        assert_eq!(git, 2, "one branch read and one origin read for /c/a");
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn rows_put_your_review_first_and_still_show_it_at_40_columns() {
        let p = ListPr {
            your_review: Some(YourReview {
                text: "approved · 2 new commits".into(),
            }),
            ci: Ci::Fail,
            review_decision: Some("APPROVED".into()),
            ..pr(12)
        };
        let now = iso_unix_ms("2026-11-12T13:00:00Z").unwrap();
        let wide = text(&row_lines(&p, &[], &[], 100, now));
        assert!(wide[0].starts_with("● #12 PR 12"), "{wide:?}");
        assert!(wide[0].ends_with("user1 · r · 3h"), "{wide:?}");
        assert_eq!(
            wide[1].trim_end(),
            "  your review: approved · 2 new commits · ✗ checks · approved · feat/12"
        );
        let narrow = text(&row_lines(&p, &[], &[], 38, now));
        assert!(narrow[1].starts_with("  your review"), "{narrow:?}");
        assert!(narrow.iter().all(|l| Line::from(l.as_str()).width() <= 38));
    }

    #[test]
    fn rows_name_the_worktree_and_its_agent() {
        let p = pr(7);
        let c = checkout("o/r", "feat/7", false);
        let lines = text(&row_lines(
            &p,
            std::slice::from_ref(&c),
            &[c.workspace_id.as_str()],
            120,
            0,
        ));
        assert!(
            lines[1].ends_with("feat/7 · worktree feat/7 · agent"),
            "{lines:?}"
        );
        let draft = ListPr {
            is_draft: true,
            ..pr(7)
        };
        assert!(text(&row_lines(&draft, &[], &[], 80, 0))[0].starts_with("◌ #7"));
    }

    #[test]
    fn conflicts_and_behind_lead_the_second_line() {
        let dirty = ListPr {
            merge_state: Some("DIRTY".into()),
            ..pr(7)
        };
        assert!(text(&row_lines(&dirty, &[], &[], 80, 0))[1].starts_with("  ! conflicts"));
        let behind = ListPr {
            merge_state: Some("BEHIND".into()),
            ..pr(7)
        };
        assert!(text(&row_lines(&behind, &[], &[], 80, 0))[1].starts_with("  ! behind base"));
        assert!(!text(&row_lines(&pr(7), &[], &[], 80, 0))[1].contains('!'));
    }

    #[test]
    fn the_detail_shows_author_merge_gate_reviewers_and_description() {
        let d = crate::integrations::github::parse_detail(include_str!(
            "../../../tests/fixtures/gh/pr-detail.json"
        ))
        .unwrap();
        let lines = text(&detail_lines(&d.pr, Some(&d), None, &[], 200, 0));
        assert!(lines[0].starts_with("● #30148 "), "{lines:?}");
        assert!(lines[1].starts_with(&format!("{} · mastra-ai/mastra", d.pr.author)));
        assert!(lines[2].starts_with("merge: "), "{lines:?}");
        assert!(lines[2].contains("checks failing"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "failing: Docs E2E tests"));
        let reviewers = lines.iter().find(|l| l.starts_with("reviewers: ")).unwrap();
        assert!(reviewers.contains("(bot)") && reviewers.contains("○ core"));
        assert!(lines.iter().any(|l| l.contains("widget report")));
        // Before the detail arrives: the row's data and a loading line.
        let early = text(&detail_lines(&d.pr, None, Some("loading…"), &[], 200, 0));
        assert!(early[2].starts_with("merge: "));
        assert_eq!(early.last().unwrap(), "loading…");
        let draft = ListPr {
            is_draft: true,
            ..d.pr.clone()
        };
        assert!(text(&detail_lines(&draft, None, None, &[], 200, 0))[0].ends_with(" · draft"));
    }

    #[test]
    fn the_cache_round_trips_and_an_unreadable_one_is_deleted() {
        let dir = std::env::temp_dir().join(format!("werkbank-prs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prs-s.json");
        let data = PrData {
            lists: parse_lists(LISTS).unwrap(),
            checkouts: vec![checkout("o/r", "feat/7", true)],
            fetched_unix_ms: 42,
        };
        save_cache(&path, &data).unwrap();
        assert_eq!(load_cache(&path), Some(data));
        std::fs::write(&path, b"{\"lists\": 3}").unwrap();
        assert_eq!(load_cache(&path), None);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
