//! `werkbank status <state-file>`: a one-line Inbox summary for Herdr's
//! `tab_bar_right` command entry. Reads the daemon's state file only, so it
//! is cheap enough to run every few seconds.

use std::path::Path;

use anyhow::{bail, Result};

use super::model::{kinds_of, AttentionKind};
use super::watcher::Snapshot;
use crate::core::unix_ms;
use crate::features::daemon::read_state;

/// A snapshot older than this means the daemon is not ticking.
const STALE_MS: u64 = 60_000;

const ORDER: [AttentionKind; 6] = [
    AttentionKind::Asking,
    AttentionKind::Blocked,
    AttentionKind::Stalled,
    AttentionKind::CiFailed,
    AttentionKind::DoneUnseen,
    AttentionKind::MergedCleanup,
];

/// Counts of Inbox rows by kind, e.g. `?1 ⏸2 ✗1`; empty when nothing needs you.
pub fn line(snapshot: &Snapshot, now_ms: u64) -> String {
    let mut counts = [0usize; ORDER.len()];
    let kinds = snapshot
        .agents
        .iter()
        .filter_map(|a| kinds_of(a).first().copied())
        .chain(snapshot.merged.iter().map(|m| m.kind));
    for kind in kinds {
        if let Some(i) = ORDER.iter().position(|k| *k == kind) {
            counts[i] += 1;
        }
    }
    let mut parts: Vec<String> = ORDER
        .iter()
        .zip(counts)
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| format!("{}{n}", k.glyph()))
        .collect();
    if now_ms.saturating_sub(snapshot.ticked_unix_ms) > STALE_MS {
        parts.insert(0, "⚠ watcher stale".into());
    }
    parts.join(" ")
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(path) = args.first() else {
        bail!("usage: werkbank status <state-file>");
    };
    match read_state(Path::new(path)) {
        Some(state) => println!("{}", line(&state.snapshot, unix_ms())),
        None => println!("⚠ watcher not running"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::herdr::AgentInfo;
    use crate::features::inbox::model::{AgentSnapshot, InboxItem};
    use crate::integrations::github::{Ci, PrInfo, PrState};
    use std::time::Duration;

    fn agent(status: &str) -> AgentSnapshot {
        AgentSnapshot {
            info: serde_json::from_value::<AgentInfo>(serde_json::json!({
                "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1",
                "agent": "mastracode", "agent_status": status,
            }))
            .unwrap(),
            workspace_label: "w".into(),
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

    fn pr(state: PrState, ci: Ci) -> PrInfo {
        PrInfo {
            number: 1,
            state,
            ci,
            url: String::new(),
            head_oid: String::new(),
            is_draft: false,
            review_decision: None,
            mergeable: None,
        }
    }

    fn snapshot(agents: Vec<AgentSnapshot>, merged: Vec<InboxItem>, ticked: u64) -> Snapshot {
        Snapshot {
            agents,
            merged,
            ticked_unix_ms: ticked,
            ..Default::default()
        }
    }

    #[test]
    fn counts_each_agent_once_at_its_top_kind_in_inbox_order() {
        let asking_and_stalled = AgentSnapshot {
            asking: true,
            stalled: Some(Duration::from_secs(400)),
            ..agent("working")
        };
        let stalled = AgentSnapshot {
            stalled: Some(Duration::from_secs(320)),
            ..agent("working")
        };
        let ci = AgentSnapshot {
            pr: Some(pr(PrState::Open, Ci::Fail)),
            ..agent("idle")
        };
        let merged = InboxItem::merged("w1", "w", "/wt", "b", pr(PrState::Merged, Ci::Pass));
        let s = snapshot(
            vec![
                agent("done"),
                stalled,
                agent("working"),
                ci,
                asking_and_stalled,
                agent("blocked"),
            ],
            vec![merged],
            1_000,
        );
        assert_eq!(line(&s, 1_000), "?1 !1 ⏸1 ✗1 ✓1 ⇣1");
    }

    #[test]
    fn a_merged_worktree_counts_even_while_its_agent_row_hides_it() {
        // User's choice: `⇣` counts every merged worktree; the Agents tab lists
        // the cleanup row only once the workspace's agent pane is gone.
        let merged = InboxItem::merged("w1", "w", "/wt", "b", pr(PrState::Merged, Ci::Pass));
        let s = snapshot(vec![agent("working")], vec![merged], 1_000);
        let rows = crate::features::inbox::model::rows(&s.agents, &s.merged);
        assert!(rows
            .iter()
            .all(|r| !matches!(r, crate::features::inbox::model::Row::Worktree(_))));
        assert_eq!(line(&s, 1_000), "⇣1");
    }

    #[test]
    fn nothing_to_do_is_empty_and_an_old_snapshot_is_flagged() {
        let s = snapshot(vec![agent("working")], Vec::new(), 1_000);
        assert_eq!(line(&s, 1_000 + STALE_MS), "");
        assert_eq!(line(&s, 2_000 + STALE_MS), "⚠ watcher stale");
    }
}
