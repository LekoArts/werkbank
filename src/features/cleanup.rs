use std::fmt;
use std::path::Path;

use anyhow::{anyhow, bail, Result};

use crate::core::herdr::{Herdr, WorkspaceInfo};
use crate::core::runner::Runner;
use crate::core::stop::Guard;
use crate::integrations::github::{gh_lookup, PrInfo, PrState};

/// Git state files that mean a merge, rebase, cherry-pick, revert or bisect is in progress.
/// Not REBASE_HEAD: git can leave it behind after a rebase finishes, and git itself
/// only treats rebase-merge/rebase-apply as a rebase in progress.
const GIT_OPS: [&str; 6] = [
    "MERGE_HEAD",
    "rebase-merge",
    "rebase-apply",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
];

/// What the confirm dialog shows and what `execute` acts on. Captured once, at plan time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPlan {
    pub workspace_id: String,
    pub workspace_label: String,
    pub checkout_path: String,
    pub repo_root: String,
    pub branch: String,
    pub head_sha: String,
    pub pr_number: u64,
    pub panes: Vec<String>,
    pub delete_branch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    IdentityChanged(String),
    CommitsAfterMerge { head: String, merged: String },
    NotLinked,
    Dirty(usize),
    Unpushed(usize),
    AgentPresent(String),
    GitOperation(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::IdentityChanged(why) => write!(f, "Identity changed: {why}"),
            Refusal::CommitsAfterMerge { head, merged } => write!(
                f,
                "Commits after merge: HEAD {} is not the merged head {}",
                short(head),
                short(merged)
            ),
            Refusal::NotLinked => write!(f, "Not linked: this is the main checkout"),
            Refusal::Dirty(n) => write!(f, "Dirty: {n} uncommitted or untracked paths"),
            Refusal::Unpushed(n) => write!(f, "Unpushed: {n} commits on no remote"),
            Refusal::AgentPresent(pane) => write!(f, "Agent present in {pane} — exit it first"),
            Refusal::GitOperation(op) => write!(f, "Git operation in progress: {op}"),
        }
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// The plan and the refusals found when it was made, for the confirm dialog.
/// `serial` lets the UI tell a fresh plan from one it already showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupView {
    pub serial: u64,
    pub plan: CleanupPlan,
    pub refusals: Vec<Refusal>,
}

/// Linked worktree workspaces only; the main checkout is never a candidate.
pub fn linked_worktrees(workspaces: &[WorkspaceInfo]) -> Vec<&WorkspaceInfo> {
    workspaces
        .iter()
        .filter(|w| {
            w.worktree
                .as_ref()
                .is_some_and(|t| t.is_linked_worktree && t.checkout_path != t.repo_root)
        })
        .collect()
}

fn git<R: Runner>(runner: &R, dir: &str, args: &[&str]) -> Result<String> {
    let mut argv = vec!["git", "-C", dir];
    argv.extend_from_slice(args);
    let out = runner.run(&argv, None, None)?;
    if !out.success() {
        bail!("git {}: {}", args.join(" "), out.stderr.trim());
    }
    Ok(out.stdout.trim().to_string())
}

/// The current branch, or `None` when HEAD is detached.
fn current_branch<R: Runner>(runner: &R, dir: &str) -> Result<Option<String>> {
    let b = git(runner, dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok((b != "HEAD" && !b.is_empty()).then_some(b))
}

fn count_lines(s: &str) -> usize {
    s.lines().filter(|l| !l.trim().is_empty()).count()
}

/// Builds the fixed plan for a workspace from fresh state.
pub fn build_plan<R: Runner>(
    herdr: &Herdr<R>,
    workspace_id: &str,
    guard: Guard,
) -> Result<CleanupPlan> {
    guard()?;
    let ws = herdr
        .workspace_list()?
        .into_iter()
        .find(|w| w.workspace_id == workspace_id)
        .ok_or_else(|| anyhow!("workspace {workspace_id} is gone"))?;
    let wt = ws
        .worktree
        .clone()
        .ok_or_else(|| anyhow!("{} is not a git checkout", ws.label))?;
    guard()?;
    let branch = current_branch(&herdr.runner, &wt.checkout_path)?
        .ok_or_else(|| anyhow!("{} has a detached HEAD", wt.checkout_path))?;
    guard()?;
    let head_sha = git(&herdr.runner, &wt.checkout_path, &["rev-parse", "HEAD"])?;
    guard()?;
    let pr = merged_pr(herdr, &wt.checkout_path, &branch)?
        .ok_or_else(|| anyhow!("{branch} has no merged PR"))?;
    guard()?;
    let panes = herdr
        .pane_list()?
        .into_iter()
        .filter(|p| p.workspace_id == workspace_id)
        .map(|p| p.pane_id)
        .collect();
    Ok(CleanupPlan {
        workspace_id: ws.workspace_id,
        workspace_label: ws.label,
        checkout_path: wt.checkout_path,
        repo_root: wt.repo_root,
        branch,
        head_sha,
        pr_number: pr.number,
        panes,
        delete_branch: false,
    })
}

/// An uncached lookup; only a MERGED PR counts.
fn merged_pr<R: Runner>(herdr: &Herdr<R>, checkout: &str, branch: &str) -> Result<Option<PrInfo>> {
    Ok(gh_lookup(&herdr.runner, checkout, branch)?.filter(|p| p.state == PrState::Merged))
}

/// Re-reads live state and lists every reason not to remove the worktree.
pub fn check<R: Runner>(
    herdr: &Herdr<R>,
    plan: &CleanupPlan,
    guard: Guard,
) -> Result<Vec<Refusal>> {
    let mut refusals = Vec::new();
    let identity = |why: String| Refusal::IdentityChanged(why);

    guard()?;
    let Some(ws) = herdr
        .workspace_list()?
        .into_iter()
        .find(|w| w.workspace_id == plan.workspace_id)
    else {
        return Ok(vec![identity("the workspace no longer exists".into())]);
    };
    let Some(wt) = ws.worktree else {
        return Ok(vec![identity(
            "the workspace is no longer a git checkout".into(),
        )]);
    };
    if wt.checkout_path != plan.checkout_path {
        return Ok(vec![identity(format!(
            "checkout moved to {}",
            wt.checkout_path
        ))]);
    }
    if !wt.is_linked_worktree || wt.checkout_path == wt.repo_root {
        refusals.push(Refusal::NotLinked);
    }

    let dir = plan.checkout_path.as_str();
    guard()?;
    match current_branch(&herdr.runner, dir)? {
        Some(b) if b == plan.branch => {}
        Some(b) => refusals.push(identity(format!("branch is now {b}, not {}", plan.branch))),
        None => refusals.push(identity("HEAD is detached".into())),
    }
    guard()?;
    let head = git(&herdr.runner, dir, &["rev-parse", "HEAD"])?;
    if head != plan.head_sha {
        refusals.push(identity(format!(
            "HEAD moved from {} to {}",
            short(&plan.head_sha),
            short(&head)
        )));
    }
    guard()?;
    let mut head_in_pr = false;
    match merged_pr(herdr, dir, &plan.branch)? {
        Some(pr) if pr.number == plan.pr_number => {
            head_in_pr = head == pr.head_oid;
            if !head_in_pr {
                refusals.push(Refusal::CommitsAfterMerge {
                    head: head.clone(),
                    merged: pr.head_oid,
                });
            }
        }
        _ => refusals.push(identity(format!(
            "PR #{} is no longer the merged PR for {}",
            plan.pr_number, plan.branch
        ))),
    }

    guard()?;
    let dirty = count_lines(&git(&herdr.runner, dir, &["status", "--porcelain"])?);
    if dirty > 0 {
        refusals.push(Refusal::Dirty(dirty));
    }
    // HEAD being exactly the merged PR's last commit means GitHub keeps every
    // commit in the PR, even after a squash merge deleted the remote branch.
    // Otherwise compare against every remote-tracking ref, so no upstream is
    // needed; with no remotes at all every commit counts and cleanup is refused.
    if !head_in_pr {
        guard()?;
        let unpushed = count_lines(&git(
            &herdr.runner,
            dir,
            &["rev-list", "HEAD", "--not", "--remotes"],
        )?);
        if unpushed > 0 {
            refusals.push(Refusal::Unpushed(unpushed));
        }
    }

    // Herdr's agent status is not reliable enough to tell an idle agent from a
    // busy or asking one, so any detected agent refuses.
    guard()?;
    for a in herdr.agent_list()? {
        if a.workspace_id == plan.workspace_id && a.agent.is_some() {
            refusals.push(Refusal::AgentPresent(a.pane_id));
        }
    }

    for op in GIT_OPS {
        guard()?;
        let path = git(&herdr.runner, dir, &["rev-parse", "--git-path", op])?;
        let path = Path::new(dir).join(path);
        if path.exists() {
            refusals.push(Refusal::GitOperation(op.to_string()));
        }
    }
    Ok(refusals)
}

/// Re-checks, then removes the worktree (never forced) and, only if asked,
/// deletes the merged branch with `branch -d`. Returns a footer summary.
pub fn execute<R: Runner>(herdr: &Herdr<R>, plan: &CleanupPlan, guard: Guard) -> Result<String> {
    let refusals = check(herdr, plan, guard)?;
    if !refusals.is_empty() {
        let reasons: Vec<String> = refusals.iter().map(Refusal::to_string).collect();
        bail!("cleanup aborted: {}", reasons.join("; "));
    }
    guard()?;
    // Computed while the checkout still exists, so the path canonicalizes as the hook saw it.
    let state_dir = crate::core::env("HERDR_PLUGIN_STATE_DIR");
    let marker = (!state_dir.is_empty())
        .then(|| crate::features::setup::marker_path(Path::new(&state_dir), &plan.checkout_path));
    let removed = herdr.worktree_remove(&plan.workspace_id)?;
    if let Some(marker) = marker {
        let _ = std::fs::remove_file(marker);
    }
    let kind = removed
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("worktree_removed");
    let mut summary = format!("{kind}: {}", plan.checkout_path);
    if plan.delete_branch {
        guard()?;
        match git(
            &herdr.runner,
            &plan.repo_root,
            &["branch", "-d", &plan.branch],
        ) {
            Ok(_) => summary.push_str(&format!(" · branch {} deleted", plan.branch)),
            Err(err) => summary.push_str(&format!(" · branch kept: {err:#}")),
        }
    } else {
        summary.push_str(&format!(" · branch {} kept", plan.branch));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::{FakeRunner, Output};

    const WT: &str = "/wt/feat-x";
    const ROOT: &str = "/repo";
    const SHA: &str = "543b8ddc19ae9a9ae0fb81242f28d8e0ce756ccc";
    const OTHER: &str = "1111111111111111111111111111111111111111";

    fn no_stop() -> Result<()> {
        Ok(())
    }

    fn workspaces(linked: bool, checkout: &str) -> String {
        format!(
            r#"{{"id":"x","result":{{"type":"workspace_list","workspaces":[
              {{"workspace_id":"w2","label":"feat-x","focused":false,
                "worktree":{{"checkout_path":"{checkout}","repo_root":"{ROOT}","is_linked_worktree":{linked},"repo_name":"repo"}}}}]}}}}"#
        )
    }

    fn agents(list: &str) -> String {
        format!(r#"{{"id":"x","result":{{"type":"agent_list","agents":[{list}]}}}}"#)
    }

    fn gh(state: &str, oid: &str) -> String {
        format!(
            r#"[{{"number":42,"state":"{state}","url":"https://github.com/o/r/pull/42","headRefOid":"{oid}","statusCheckRollup":[]}}]"#
        )
    }

    const GH: [&str; 11] = [
        "gh",
        "pr",
        "list",
        "--head",
        "feat/x",
        "--state",
        "all",
        "--limit",
        "1",
        "--json",
        "number,state,url,headRefOid,statusCheckRollup",
    ];

    fn g(args: &[&str]) -> Vec<String> {
        let mut v = vec!["git".to_string(), "-C".into(), WT.into()];
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }

    fn ok(f: &FakeRunner, argv: &[String], out: &str) {
        let a: Vec<&str> = argv.iter().map(String::as_str).collect();
        f.set(&a, None, out);
    }

    /// A clean, linked, merged, pushed worktree with no agent and no git operation.
    fn clean() -> FakeRunner {
        let f = FakeRunner::new();
        f.ok(&["herdr", "workspace", "list"], None, &workspaces(true, WT))
            .ok(&["herdr", "agent", "list"], None, &agents(""))
            .ok(
                &["herdr", "pane", "list"],
                None,
                r#"{"id":"x","result":{"type":"pane_list","panes":[
                   {"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1"},
                   {"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1"}]}}"#,
            )
            .ok(&GH, Some(WT), &gh("MERGED", SHA));
        ok(&f, &g(&["rev-parse", "--abbrev-ref", "HEAD"]), "feat/x\n");
        ok(&f, &g(&["rev-parse", "HEAD"]), &format!("{SHA}\n"));
        ok(&f, &g(&["status", "--porcelain"]), "");
        ok(&f, &g(&["rev-list", "HEAD", "--not", "--remotes"]), "");
        for op in GIT_OPS {
            ok(
                &f,
                &g(&["rev-parse", "--git-path", op]),
                &format!("/nonexistent-werkbank/{op}\n"),
            );
        }
        f.ok(
            &["herdr", "worktree", "remove", "--workspace", "w2"],
            None,
            r#"{"id":"x","result":{"type":"worktree_removed","forced":false}}"#,
        );
        f
    }

    fn herdr(f: FakeRunner) -> Herdr<FakeRunner> {
        Herdr::with_bin(f, "herdr")
    }

    fn plan() -> CleanupPlan {
        build_plan(&herdr(clean()), "w2", &no_stop).unwrap()
    }

    #[test]
    fn builds_plan_from_fresh_state() {
        let p = plan();
        assert_eq!(p.branch, "feat/x");
        assert_eq!(p.head_sha, SHA);
        assert_eq!(p.pr_number, 42);
        assert_eq!(p.panes, vec!["w2:p1".to_string()]);
        assert_eq!(p.repo_root, ROOT);
        assert!(!p.delete_branch, "branch is kept by default");
    }

    #[test]
    fn clean_plan_passes() {
        assert_eq!(check(&herdr(clean()), &plan(), &no_stop).unwrap(), vec![]);
    }

    fn refusals(setup: impl FnOnce(&FakeRunner)) -> Vec<Refusal> {
        let f = clean();
        setup(&f);
        check(&herdr(f), &plan(), &no_stop).unwrap()
    }

    // Plans are built on a separate clean runner; the mutated runner only
    // serves the re-check, as if state changed while the dialog was open.

    #[test]
    fn refuses_dirty() {
        let r = refusals(|f| ok(f, &g(&["status", "--porcelain"]), "?? junk.txt\n M a.rs\n"));
        assert_eq!(r, vec![Refusal::Dirty(2)]);
        assert!(r[0].to_string().starts_with("Dirty"));
    }

    #[test]
    fn refuses_unpushed_commits_beyond_the_merged_head() {
        // HEAD moved past the PR's last commit and the new commits are on no remote
        // (with no remotes, `--not --remotes` excludes nothing and lists every commit).
        let f = clean();
        ok(&f, &g(&["rev-parse", "HEAD"]), &format!("{OTHER}\n"));
        ok(
            &f,
            &g(&["rev-list", "HEAD", "--not", "--remotes"]),
            "a\nb\nc\n",
        );
        let mut p = plan();
        p.head_sha = OTHER.into();
        let r = check(&herdr(f), &p, &no_stop).unwrap();
        assert!(r.contains(&Refusal::Unpushed(3)), "{r:?}");
        assert!(r
            .iter()
            .any(|x| matches!(x, Refusal::CommitsAfterMerge { .. })));
    }

    #[test]
    fn squash_merged_branch_with_deleted_remote_passes() {
        // After a squash merge the remote branch is gone, so the branch's commits
        // are on no remote-tracking ref; HEAD is the PR's last commit, which
        // GitHub keeps in the PR, so nothing is lost and the unpushed probe is skipped.
        let f = clean();
        ok(
            &f,
            &g(&["rev-list", "HEAD", "--not", "--remotes"]),
            "a\nb\nc\n",
        );
        let h = herdr(f);
        assert!(check(&h, &plan(), &no_stop).unwrap().is_empty());
        assert!(!h.runner.argvs().iter().any(|a| a.contains("rev-list")));
    }

    #[test]
    fn refuses_main_checkout() {
        let f = clean();
        f.set(
            &["herdr", "workspace", "list"],
            None,
            &workspaces(false, WT),
        );
        let mut p = plan();
        p.checkout_path = WT.into();
        assert_eq!(
            check(&herdr(f), &p, &no_stop).unwrap(),
            vec![Refusal::NotLinked]
        );
        // Same path as the repo root also counts as the main checkout.
        let mut ws: Vec<WorkspaceInfo> = serde_json::from_value(
            serde_json::from_str::<serde_json::Value>(&workspaces(true, ROOT)).unwrap()["result"]
                ["workspaces"]
                .clone(),
        )
        .unwrap();
        assert!(linked_worktrees(&ws).is_empty());
        ws[0].worktree.as_mut().unwrap().is_linked_worktree = false;
        assert!(linked_worktrees(&ws).is_empty());
    }

    #[test]
    fn idle_or_done_agent_in_workspace_refuses() {
        for status in ["idle", "done", "working"] {
            let r = refusals(|f| {
                f.set(
                    &["herdr", "agent", "list"],
                    None,
                    &agents(&format!(
                        r#"{{"pane_id":"w2:p1","workspace_id":"w2","tab_id":"w2:t1","agent":"mastracode","agent_status":"{status}"}},
                           {{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","agent":"mastracode","agent_status":"working"}}"#
                    )),
                );
            });
            assert_eq!(r, vec![Refusal::AgentPresent("w2:p1".into())], "{status}");
            assert_eq!(r[0].to_string(), "Agent present in w2:p1 — exit it first");
        }
    }

    #[test]
    fn refuses_git_operation_in_progress() {
        let dir = std::env::temp_dir().join(format!("werkbank-gitop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let merge_head = dir.join("MERGE_HEAD");
        std::fs::write(&merge_head, "x").unwrap();
        let r = refusals(|f| {
            ok(
                f,
                &g(&["rev-parse", "--git-path", "MERGE_HEAD"]),
                merge_head.to_str().unwrap(),
            )
        });
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(r, vec![Refusal::GitOperation("MERGE_HEAD".into())]);
    }

    #[test]
    fn a_leftover_rebase_head_passes_but_a_rebase_in_progress_refuses() {
        let dir = std::env::temp_dir().join(format!("werkbank-rebase-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("rebase-merge")).unwrap();
        std::fs::write(dir.join("REBASE_HEAD"), "x").unwrap();
        let r = refusals(|f| {
            ok(
                f,
                &g(&["rev-parse", "--git-path", "rebase-merge"]),
                dir.join("rebase-merge").to_str().unwrap(),
            )
        });
        std::fs::remove_dir(dir.join("rebase-merge")).unwrap();
        let leftover = refusals(|f| {
            ok(
                f,
                &g(&["rev-parse", "--git-path", "REBASE_HEAD"]),
                dir.join("REBASE_HEAD").to_str().unwrap(),
            )
        });
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(r, vec![Refusal::GitOperation("rebase-merge".into())]);
        assert!(leftover.is_empty(), "{leftover:?}");
    }

    #[test]
    fn commits_after_merge_refuses_and_exact_merged_head_passes() {
        // HEAD has one more pushed commit than the merged PR head; PR still MERGED.
        let f = clean();
        ok(&f, &g(&["rev-parse", "HEAD"]), &format!("{OTHER}\n"));
        let mut p = build_plan(&herdr(clean()), "w2", &no_stop).unwrap();
        p.head_sha = OTHER.into();
        let r = check(&herdr(f), &p, &no_stop).unwrap();
        assert_eq!(
            r,
            vec![Refusal::CommitsAfterMerge {
                head: OTHER.into(),
                merged: SHA.into()
            }]
        );
        assert!(check(&herdr(clean()), &plan(), &no_stop)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn detached_head_is_not_plannable_and_refuses_on_recheck() {
        let f = clean();
        f.set(
            &["git", "-C", WT, "rev-parse", "--abbrev-ref", "HEAD"],
            None,
            "HEAD\n",
        );
        let err = build_plan(&herdr(f), "w2", &no_stop).unwrap_err();
        assert!(err.to_string().contains("detached"), "{err}");

        let r = refusals(|f| ok(f, &g(&["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD\n"));
        assert_eq!(r, vec![Refusal::IdentityChanged("HEAD is detached".into())]);
    }

    fn assert_execute_aborts(setup: impl FnOnce(&FakeRunner), expect: &str) {
        let f = clean();
        let p = build_plan(&herdr(clean()), "w2", &no_stop).unwrap();
        setup(&f);
        let h = herdr(f);
        let err = execute(&h, &p, &no_stop).unwrap_err().to_string();
        assert!(err.contains(expect), "{err}");
        let argvs = h.runner.argvs();
        assert!(
            !argvs.iter().any(|a| a.contains("worktree remove")),
            "{argvs:?}"
        );
    }

    #[test]
    fn execute_rechecks_and_aborts_when_state_changed() {
        assert_execute_aborts(
            |f| ok(f, &g(&["status", "--porcelain"]), "?? new\n"),
            "Dirty",
        );
        // Switched to a clean, pushed, unmerged branch.
        assert_execute_aborts(
            |f| {
                ok(f, &g(&["rev-parse", "--abbrev-ref", "HEAD"]), "other\n");
                let mut gh_other = GH.to_vec();
                gh_other[4] = "other";
                f.ok(&gh_other, Some(WT), "[]");
            },
            "branch is now other",
        );
        assert_execute_aborts(
            |f| ok(f, &g(&["rev-parse", "HEAD"]), &format!("{OTHER}\n")),
            "HEAD moved",
        );
        assert_execute_aborts(
            |f| {
                f.set(&GH, Some(WT), &gh("OPEN", SHA));
            },
            "no longer the merged PR",
        );
        assert_execute_aborts(
            |f| {
                f.set(
                    &["herdr", "workspace", "list"],
                    None,
                    r#"{"id":"x","result":{"type":"workspace_list","workspaces":[]}}"#,
                );
            },
            "no longer exists",
        );
        assert_execute_aborts(
            |f| {
                f.set(
                    &["herdr", "agent", "list"],
                    None,
                    &agents(r#"{"pane_id":"w2:p3","workspace_id":"w2","tab_id":"w2:t1","agent":"mastracode","agent_status":"idle"}"#),
                );
            },
            "Agent present in w2:p3",
        );
    }

    #[test]
    fn execute_removes_without_force_and_keeps_branch_by_default() {
        let h = herdr(clean());
        let summary = execute(&h, &plan(), &no_stop).unwrap();
        assert_eq!(
            summary,
            format!("worktree_removed: {WT} · branch feat/x kept")
        );
        let argvs = h.runner.argvs();
        assert!(argvs.contains(&"herdr worktree remove --workspace w2".to_string()));
        assert!(!argvs
            .iter()
            .any(|a| a.contains("--force") || a.contains(" -D")));
        assert!(!argvs.iter().any(|a| a.contains("branch")), "{argvs:?}");
    }

    #[test]
    fn delete_branch_uses_lowercase_d_only_when_toggled() {
        let f = clean();
        f.ok(
            &["git", "-C", ROOT, "branch", "-d", "feat/x"],
            None,
            "Deleted\n",
        );
        let h = herdr(f);
        let mut p = plan();
        p.delete_branch = true;
        let summary = execute(&h, &p, &no_stop).unwrap();
        assert!(summary.ends_with("branch feat/x deleted"), "{summary}");
        let argvs = h.runner.argvs();
        assert!(argvs.contains(&format!("git -C {ROOT} branch -d feat/x")));
        assert!(!argvs
            .iter()
            .any(|a| a.contains("--force") || a.contains(" -D")));
    }

    #[test]
    fn failed_branch_delete_reports_branch_kept_without_retry() {
        let f = clean();
        f.respond(
            &["git", "-C", ROOT, "branch", "-d", "feat/x"],
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: "error: the branch 'feat/x' is not fully merged.".into(),
            },
        );
        let h = herdr(f);
        let mut p = plan();
        p.delete_branch = true;
        let summary = execute(&h, &p, &no_stop).unwrap();
        assert!(
            summary.contains("branch kept: git branch -d feat/x: error: the branch"),
            "{summary}"
        );
        let deletes = h
            .runner
            .argvs()
            .iter()
            .filter(|a| a.contains("branch -d"))
            .count();
        assert_eq!(deletes, 1);
    }

    #[test]
    fn stop_flag_prevents_removal() {
        let h = herdr(clean());
        let stopped = || -> Result<()> { Err(anyhow!("stopped")) };
        assert!(execute(&h, &plan(), &stopped).is_err());
        assert!(h.runner.argvs().is_empty());
    }
}
