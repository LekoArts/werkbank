use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::runner::Runner;

pub const OPEN_REFRESH: Duration = Duration::from_secs(30);
pub const CLOSED_REFRESH: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ci {
    Fail,
    Pending,
    Pass,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub state: PrState,
    pub url: String,
    pub head_oid: String,
    pub ci: Ci,
    #[serde(default)]
    pub is_draft: bool,
    /// GitHub's `reviewDecision`, e.g. `APPROVED` or `CHANGES_REQUESTED`.
    #[serde(default)]
    pub review_decision: Option<String>,
    /// GitHub's `mergeable`: `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    #[serde(default)]
    pub mergeable: Option<String>,
}

impl PrInfo {
    pub fn token(&self) -> String {
        let n = self.number;
        match (self.state, self.ci) {
            (PrState::Merged, _) => format!("#{n} ⇣"),
            (PrState::Closed, _) => format!("#{n} ⊘"),
            (PrState::Open, Ci::Fail) => format!("#{n} ✗"),
            (PrState::Open, Ci::Pending) => format!("#{n} …"),
            (PrState::Open, Ci::Pass) => format!("#{n} ✓"),
            (PrState::Open, Ci::None) => format!("#{n}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Fail,
    Pending,
    Pass,
}

fn classify(check: &Value) -> Check {
    let s = |k: &str| check.get(k).and_then(Value::as_str);
    let kind = s("__typename").unwrap_or_else(|| {
        if check.get("status").is_some() || check.get("conclusion").is_some() {
            "CheckRun"
        } else if check.get("state").is_some() {
            "StatusContext"
        } else {
            ""
        }
    });
    match kind {
        "CheckRun" => {
            let conclusion = s("conclusion").unwrap_or_default();
            match conclusion {
                "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
                    Check::Fail
                }
                "" => Check::Pending,
                _ if s("status") != Some("COMPLETED") => Check::Pending,
                _ => Check::Pass,
            }
        }
        "StatusContext" => match s("state") {
            Some("FAILURE" | "ERROR") => Check::Fail,
            Some("SUCCESS") => Check::Pass,
            _ => Check::Pending,
        },
        _ => Check::Pending,
    }
}

/// Fail beats pending beats pass; no checks at all is `None`.
pub fn rollup(checks: &[Value]) -> Ci {
    let kinds: Vec<Check> = checks.iter().map(classify).collect();
    if kinds.contains(&Check::Fail) {
        Ci::Fail
    } else if kinds.contains(&Check::Pending) {
        Ci::Pending
    } else if kinds.is_empty() {
        Ci::None
    } else {
        Ci::Pass
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    state: PrState,
    url: String,
    head_ref_oid: String,
    #[serde(default)]
    status_check_rollup: Vec<Value>,
}

/// Parses `gh pr list --json number,state,url,headRefOid,statusCheckRollup` output.
pub fn parse_pr_list(stdout: &str) -> Result<Option<PrInfo>> {
    let prs: Vec<GhPr> = serde_json::from_str(stdout).context("gh pr list: invalid JSON")?;
    Ok(prs.into_iter().next().map(|p| PrInfo {
        ci: rollup(&p.status_check_rollup),
        number: p.number,
        state: p.state,
        url: p.url,
        head_oid: p.head_ref_oid,
        is_draft: false,
        review_decision: None,
        mergeable: None,
    }))
}

/// A checkout's repo toplevel and current branch, when it is on a PR-able branch.
/// `repo` is the git common dir, shared by a main checkout and all its worktrees.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BranchKey {
    pub toplevel: String,
    pub branch: String,
    pub repo: String,
}

pub const BRANCH_ARGS: [&str; 6] = [
    "rev-parse",
    "--show-toplevel",
    "--abbrev-ref",
    "HEAD",
    "--path-format=absolute",
    "--git-common-dir",
];

/// The checkout's toplevel (`None` outside git) and its branch key, which is
/// `None` for detached HEAD, `main` and `master`.
pub fn probe_branch<R: Runner>(runner: &R, cwd: &str) -> Option<(String, Option<BranchKey>)> {
    let mut argv = vec!["git", "-C", cwd];
    argv.extend(BRANCH_ARGS);
    let out = runner.run(&argv, None, None).ok()?;
    if !out.success() {
        return None;
    }
    let mut lines = out.stdout.lines();
    let toplevel = lines.next()?.trim().to_string();
    let branch = lines.next()?.trim().to_string();
    let repo = lines.next()?.trim().to_string();
    let key = match branch.as_str() {
        "" | "HEAD" | "main" | "master" => None,
        _ => Some(BranchKey {
            toplevel: toplevel.clone(),
            branch,
            repo,
        }),
    };
    Some((toplevel, key))
}

fn gh_error(out: &crate::core::runner::Output) -> anyhow::Error {
    let msg = out.stderr.lines().next().unwrap_or("").trim().to_string();
    anyhow!("gh: {msg}")
}

/// An uncached single-branch lookup, run in `toplevel`.
pub fn gh_lookup<R: Runner>(runner: &R, toplevel: &str, branch: &str) -> Result<Option<PrInfo>> {
    let out = runner.run(
        &[
            "gh",
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "1",
            "--json",
            "number,state,url,headRefOid,statusCheckRollup",
        ],
        None,
        Some(Path::new(toplevel)),
    )?;
    if !out.success() {
        return Err(gh_error(&out));
    }
    parse_pr_list(&out.stdout)
}

/// Branches per batched GraphQL call.
pub const BATCH: usize = 25;

const PR_NODES: &str = "nodes{number state url headRefOid isDraft reviewDecision mergeable commits(last:1){nodes{commit{\
statusCheckRollup{contexts(first:100){nodes{__typename \
...on CheckRun{status conclusion} ...on StatusContext{state}}}}}}}}";

/// One `gh api graphql` call for many branches of the same repo: alias `b<i>` is the
/// newest PR (any state) whose head is `branches[i]`, like `gh pr list --head`.
/// Branch names travel as `-f` variables, never inside the query text.
pub fn batch_argv(branches: &[&str]) -> Vec<String> {
    let mut vars = String::from("$owner:String!,$name:String!");
    let mut fields = String::new();
    for i in 0..branches.len() {
        vars.push_str(&format!(",$b{i}:String!"));
        fields.push_str(&format!(
            "b{i}:pullRequests(headRefName:$b{i},states:[OPEN,MERGED,CLOSED],first:1,\
orderBy:{{field:CREATED_AT,direction:DESC}}){{{PR_NODES}}} "
        ));
    }
    let mut argv: Vec<String> = [
        "gh",
        "api",
        "graphql",
        "-F",
        "owner={owner}",
        "-F",
        "name={repo}",
        "-f",
    ]
    .map(String::from)
    .to_vec();
    argv.push(format!(
        "query=query({vars}){{repository(owner:$owner,name:$name){{{fields}}}}}"
    ));
    for (i, b) in branches.iter().enumerate() {
        argv.push("-f".into());
        argv.push(format!("b{i}={b}"));
    }
    argv
}

fn parse_node(node: &Value) -> Result<PrInfo> {
    let s = |k: &str| {
        node.get(k)
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("gh api graphql: PR without {k}"))
    };
    let checks = node
        .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(PrInfo {
        number: node
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("gh api graphql: PR without number"))?,
        state: serde_json::from_value(Value::String(s("state")?))
            .context("gh api graphql: unknown PR state")?,
        url: s("url")?,
        head_oid: s("headRefOid")?,
        ci: rollup(&checks),
        is_draft: node.get("isDraft").and_then(Value::as_bool) == Some(true),
        review_decision: s("reviewDecision").ok(),
        mergeable: s("mergeable").ok(),
    })
}

/// Parses `batch_argv` output into one result per requested branch, in order.
pub fn parse_batch(stdout: &str, n: usize) -> Result<Vec<Option<PrInfo>>> {
    let v: Value = serde_json::from_str(stdout).context("gh api graphql: invalid JSON")?;
    let Some(repo) = v.pointer("/data/repository").filter(|r| r.is_object()) else {
        let msg = v
            .pointer("/errors/0/message")
            .and_then(Value::as_str)
            .unwrap_or("no repository in reply");
        return Err(anyhow!("gh api graphql: {msg}"));
    };
    (0..n)
        .map(|i| {
            let conn = repo
                .get(format!("b{i}"))
                .filter(|c| c.is_object())
                .ok_or_else(|| anyhow!("gh api graphql: missing b{i}"))?;
            conn.pointer("/nodes/0").map(parse_node).transpose()
        })
        .collect()
}

/// Looks up the PRs of `branches` (all in the repo checked out at `toplevel`) in one call.
pub fn gh_lookup_batch<R: Runner>(
    runner: &R,
    toplevel: &str,
    branches: &[&str],
) -> Result<Vec<Option<PrInfo>>> {
    let argv = batch_argv(branches);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = runner.run(&argv, None, Some(Path::new(toplevel)))?;
    if !out.success() {
        return Err(gh_error(&out));
    }
    parse_batch(&out.stdout, branches.len())
}

struct Cached {
    at: Instant,
    pr: Option<PrInfo>,
}

/// Per (toplevel, branch) PR results: open/missing refresh every 30s, merged/closed every 5 min.
#[derive(Default)]
pub struct PrCache {
    entries: HashMap<BranchKey, Cached>,
    /// When a checkout last reported this branch.
    seen: HashMap<BranchKey, Instant>,
}

/// A cache entry no checkout has reported for this long is dropped. Long enough
/// that a few failed probes (pane read errors, git hiccups) never lose live data.
pub const EVICT_AFTER: Duration = Duration::from_secs(600);

impl PrCache {
    pub fn due(&self, key: &BranchKey, now: Instant) -> bool {
        match self.entries.get(key) {
            None => true,
            Some(c) => {
                let every = match c.pr.as_ref().map(|p| p.state) {
                    Some(PrState::Merged | PrState::Closed) => CLOSED_REFRESH,
                    _ => OPEN_REFRESH,
                };
                now.duration_since(c.at) >= every
            }
        }
    }

    pub fn store(&mut self, key: BranchKey, pr: Option<PrInfo>, now: Instant) {
        self.seen.insert(key.clone(), now);
        self.entries.insert(key, Cached { at: now, pr });
    }

    /// A failed lookup waits a full interval before retrying, like a success,
    /// and keeps the last known PR rather than dropping it.
    pub fn store_failure(&mut self, key: BranchKey, now: Instant) {
        self.seen.insert(key.clone(), now);
        self.entries
            .entry(key)
            .and_modify(|c| c.at = now)
            .or_insert(Cached { at: now, pr: None });
    }

    pub fn get(&self, key: &BranchKey) -> Option<&PrInfo> {
        self.entries.get(key)?.pr.as_ref()
    }

    /// Marks `active` as seen now and drops entries no checkout has reported for
    /// `EVICT_AFTER`, so the cache holds the working set, not every branch ever seen.
    pub fn evict_inactive(&mut self, active: &HashSet<BranchKey>, now: Instant) {
        for key in active {
            self.seen.insert(key.clone(), now);
        }
        self.seen
            .retain(|_, at| now.duration_since(*at) < EVICT_AFTER);
        let seen = &self.seen;
        self.entries.retain(|k, _| seen.contains_key(k));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// ---- The Inbox's PRs tab: the viewer's open PRs and review requests ----

/// Commits a list row looks back through for "your review · N new commits".
pub const LIST_WINDOW: usize = 30;
/// The same look-back on the PR screen.
pub const DETAIL_WINDOW: usize = 100;
/// Results per search; the header says when there are more.
pub const SEARCH_FIRST: usize = 50;

const LIST_FIELDS: &str = "id number title url isDraft author{__typename login} \
repository{nameWithOwner} headRefName headRepository{nameWithOwner} reviewDecision \
mergeStateStatus mergeable updatedAt viewerLatestReview{state commit{oid}} \
latestReviews(last:20){nodes{author{__typename login} state commit{oid}}}";

/// One reviewer: `state` is GitHub's review state, or `REQUESTED` for a
/// requested review not given yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewer {
    pub login: String,
    pub bot: bool,
    pub state: String,
}

/// The viewer's own latest review and what happened since.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct YourReview {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPr {
    /// GraphQL node id, for mutations.
    pub id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    pub author: String,
    pub author_bot: bool,
    /// `owner/name`.
    pub repo: String,
    pub head_ref: String,
    /// `owner/name` of the head repo; differs from `repo` for a fork, `None` if deleted.
    pub head_repo: Option<String>,
    pub review_decision: Option<String>,
    pub merge_state: Option<String>,
    pub mergeable: Option<String>,
    pub updated_at: String,
    pub ci: Ci,
    pub reviewers: Vec<Reviewer>,
    pub your_review: Option<YourReview>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLists {
    pub viewer: String,
    pub mine: Vec<ListPr>,
    pub assigned: Vec<ListPr>,
    pub mine_total: u64,
    pub assigned_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDetail {
    pub pr: ListPr,
    pub body: String,
    pub base_ref: String,
    /// Names of failing checks.
    pub failing: Vec<String>,
    pub pending: usize,
}

fn commits_field(window: usize) -> String {
    format!("commits(last:{window}){{totalCount nodes{{commit{{oid}}}}}}")
}

fn search(alias: &str, q: &str) -> String {
    format!(
        "{alias}:search(query:\"{q}\",type:ISSUE,first:{SEARCH_FIRST}){{issueCount nodes{{...on PullRequest{{\
{LIST_FIELDS} head:commits(last:1){{nodes{{commit{{statusCheckRollup{{state}}}}}}}} {}}}}}}}",
        commits_field(LIST_WINDOW)
    )
}

/// One `gh api graphql` read: the viewer's open PRs, authored or assigned.
pub fn lists_argv() -> Vec<String> {
    let query = format!(
        "query{{viewer{{login}} {} {}}}",
        search("mine", "is:pr is:open author:@me archived:false"),
        search("assigned", "is:pr is:open assignee:@me archived:false"),
    );
    ["gh", "api", "graphql", "-f"]
        .map(String::from)
        .into_iter()
        .chain([format!("query={query}")])
        .collect()
}

/// `owner/name` repo, PR number: one read with the PR screen's extra fields.
pub fn detail_argv(repo: &str, number: u64) -> Result<Vec<String>> {
    let (owner, name) = repo
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty() && !o.starts_with('@'))
        .ok_or_else(|| anyhow!("not an owner/name repo: {repo}"))?;
    let query = format!(
        "query($owner:String!,$name:String!,$number:Int!){{\
repository(owner:$owner,name:$name){{pullRequest(number:$number){{{LIST_FIELDS} body baseRefName \
reviewRequests(first:20){{nodes{{requestedReviewer{{__typename ...on User{{login}} ...on Bot{{login}} \
...on Team{{name}}}}}}}} head:commits(last:1){{nodes{{commit{{statusCheckRollup{{state \
contexts(first:100){{nodes{{__typename ...on CheckRun{{name status conclusion}} \
...on StatusContext{{context state}}}}}}}}}}}}}} {}}}}}}}",
        commits_field(DETAIL_WINDOW)
    );
    Ok([
        "gh".to_string(),
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("owner={owner}"),
        "-f".into(),
        format!("name={name}"),
        "-F".into(),
        format!("number={number}"),
        "-f".into(),
        format!("query={query}"),
    ]
    .to_vec())
}

fn review_word(state: &str) -> String {
    match state {
        "APPROVED" => "approved".into(),
        "CHANGES_REQUESTED" => "changes requested".into(),
        "COMMENTED" => "commented".into(),
        "DISMISSED" => "dismissed".into(),
        other => other.to_lowercase(),
    }
}

/// "Your review" from the viewer's review `state` at `oid` and the PR's last
/// `commits` (oldest first) out of `total`, looked up in a `window` of commits.
/// Without the reviewed commit in the window the count is unknown, so it never
/// claims one.
pub fn your_review(
    state: &str,
    oid: Option<&str>,
    commits: &[String],
    total: u64,
    window: usize,
) -> YourReview {
    let word = review_word(state);
    match oid.and_then(|o| commits.iter().position(|c| c == o)) {
        Some(at) => {
            let n = commits.len() - 1 - at;
            YourReview {
                text: match n {
                    0 => word,
                    1 => format!("{word} · 1 new commit"),
                    _ => format!("{word} · {n} new commits"),
                },
            }
        }
        None if total <= window as u64 => YourReview {
            text: format!("{word} · reviewed an earlier head"),
        },
        None => YourReview {
            text: format!("{word} · reviewed an earlier head ({window}+ commits)"),
        },
    }
}

/// GitHub's rolled-up check state.
fn ci_of_state(state: Option<&str>) -> Ci {
    match state {
        Some("SUCCESS") => Ci::Pass,
        Some("FAILURE" | "ERROR") => Ci::Fail,
        Some(_) => Ci::Pending,
        None => Ci::None,
    }
}

fn parse_list_pr(node: &Value, window: usize) -> Result<ListPr> {
    let s = |p: &str| node.pointer(p).and_then(Value::as_str).map(String::from);
    let need = |p: &str| s(p).ok_or_else(|| anyhow!("gh api graphql: PR without {p}"));
    let reviews: Vec<&Value> = node
        .pointer("/latestReviews/nodes")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let reviewers = reviews
        .iter()
        .filter_map(|r| {
            Some(Reviewer {
                login: r.pointer("/author/login")?.as_str()?.to_string(),
                bot: r.pointer("/author/__typename").and_then(Value::as_str) == Some("Bot"),
                state: r.get("state")?.as_str()?.to_string(),
            })
        })
        .filter(|r| r.state != "PENDING")
        .collect();
    let commits: Vec<String> = node
        .pointer("/commits/nodes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| c.pointer("/commit/oid")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let total = node
        .pointer("/commits/totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(commits.len() as u64);
    // GitHub's own pointer to the viewer's latest review, so it never drops out
    // of the 20 reviews listed for the reviewers line.
    let mine = node.get("viewerLatestReview").filter(|r| {
        r.get("state")
            .and_then(Value::as_str)
            .is_some_and(|s| s != "PENDING")
    });
    let your_review = mine.map(|r| {
        your_review(
            r.get("state").and_then(Value::as_str).unwrap_or(""),
            r.pointer("/commit/oid").and_then(Value::as_str),
            &commits,
            total,
            window,
        )
    });
    Ok(ListPr {
        id: need("/id")?,
        number: node
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("gh api graphql: PR without number"))?,
        title: need("/title")?,
        url: need("/url")?,
        is_draft: node.get("isDraft").and_then(Value::as_bool) == Some(true),
        author: s("/author/login").unwrap_or_else(|| "ghost".into()),
        author_bot: s("/author/__typename").as_deref() == Some("Bot"),
        repo: need("/repository/nameWithOwner")?,
        head_ref: need("/headRefName")?,
        head_repo: s("/headRepository/nameWithOwner"),
        review_decision: s("/reviewDecision"),
        merge_state: s("/mergeStateStatus"),
        mergeable: s("/mergeable"),
        updated_at: s("/updatedAt").unwrap_or_default(),
        ci: ci_of_state(
            node.pointer("/head/nodes/0/commit/statusCheckRollup/state")
                .and_then(Value::as_str),
        ),
        reviewers,
        your_review,
    })
}

fn graphql_data(stdout: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(stdout).context("gh api graphql: invalid JSON")?;
    match v.get("data").filter(|d| d.is_object()) {
        Some(d) => Ok(d.clone()),
        None => Err(anyhow!(
            "gh api graphql: {}",
            v.pointer("/errors/0/message")
                .and_then(Value::as_str)
                .unwrap_or("no data in reply")
        )),
    }
}

pub fn parse_lists(stdout: &str) -> Result<PrLists> {
    let data = graphql_data(stdout)?;
    let viewer = data
        .pointer("/viewer/login")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("gh api graphql: no viewer"))?
        .to_string();
    let list = |alias: &str| -> Result<(Vec<ListPr>, u64)> {
        let s = data
            .get(alias)
            .ok_or_else(|| anyhow!("gh api graphql: no {alias} search"))?;
        let prs = s
            .pointer("/nodes")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            // A search node that isn't a PR comes back as `{}`.
            .filter(|n| n.get("number").is_some())
            .map(|n| parse_list_pr(n, LIST_WINDOW))
            .collect::<Result<_>>()?;
        Ok((
            prs,
            s.get("issueCount").and_then(Value::as_u64).unwrap_or(0),
        ))
    };
    let (mine, mine_total) = list("mine")?;
    let (assigned, assigned_total) = list("assigned")?;
    Ok(PrLists {
        viewer,
        mine,
        assigned,
        mine_total,
        assigned_total,
    })
}

pub fn parse_detail(stdout: &str) -> Result<PrDetail> {
    let data = graphql_data(stdout)?;
    let node = data
        .pointer("/repository/pullRequest")
        .filter(|n| n.is_object())
        .ok_or_else(|| anyhow!("gh api graphql: no such PR"))?;
    let mut pr = parse_list_pr(node, DETAIL_WINDOW)?;
    let requested = node
        .pointer("/reviewRequests/nodes")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let who = r.get("requestedReviewer")?;
            let login = who.get("login").or_else(|| who.get("name"))?.as_str()?;
            Some(Reviewer {
                login: login.to_string(),
                bot: who.get("__typename").and_then(Value::as_str) == Some("Bot"),
                state: "REQUESTED".into(),
            })
        })
        .filter(|r| !pr.reviewers.iter().any(|x| x.login == r.login))
        .collect::<Vec<_>>();
    pr.reviewers.extend(requested);
    let contexts: Vec<Value> = node
        .pointer("/head/nodes/0/commit/statusCheckRollup/contexts/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let name = |c: &Value| {
        c.get("name")
            .or_else(|| c.get("context"))
            .and_then(Value::as_str)
            .unwrap_or("check")
            .to_string()
    };
    let mut failing: Vec<String> = Vec::new();
    let mut pending = 0;
    for c in &contexts {
        match classify(c) {
            Check::Fail => {
                let n = name(c);
                if !failing.contains(&n) {
                    failing.push(n);
                }
            }
            Check::Pending => pending += 1,
            Check::Pass => {}
        }
    }
    Ok(PrDetail {
        pr,
        body: node
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        base_ref: node
            .get("baseRefName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        failing,
        pending,
    })
}

fn gh_json<R: Runner>(runner: &R, argv: &[String]) -> Result<String> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = runner.run(&argv, None, None)?;
    if !out.success() {
        return Err(gh_error(&out));
    }
    Ok(out.stdout)
}

pub fn fetch_pr_lists<R: Runner>(runner: &R) -> Result<PrLists> {
    parse_lists(&gh_json(runner, &lists_argv())?)
}

pub fn fetch_pr_detail<R: Runner>(runner: &R, repo: &str, number: u64) -> Result<PrDetail> {
    parse_detail(&gh_json(runner, &detail_argv(repo, number)?)?)
}

/// The branch's newest Actions runs, newest first.
pub fn runs_argv(repo: &str, head: &str) -> Vec<String> {
    [
        "gh",
        "run",
        "list",
        &format!("--repo={repo}"),
        &format!("--branch={head}"),
        "--limit=30",
        "--json=databaseId,headSha,conclusion",
    ]
    .map(String::from)
    .to_vec()
}

/// The failed runs of the newest commit with runs; older commits' failures
/// are history, not this PR's state.
pub fn parse_failed_runs(json: &str) -> Result<Vec<u64>> {
    let runs: Vec<Value> = serde_json::from_str(json)?;
    let newest = runs.first().and_then(|r| r.get("headSha")).cloned();
    Ok(runs
        .iter()
        .filter(|r| newest.is_some() && r.get("headSha") == newest.as_ref())
        .filter(|r| {
            matches!(
                r.get("conclusion").and_then(Value::as_str),
                Some("failure" | "timed_out" | "cancelled" | "startup_failure")
            )
        })
        .filter_map(|r| r.get("databaseId").and_then(Value::as_u64))
        .collect())
}

pub fn rerun_argv(repo: &str, run: u64) -> Vec<String> {
    [
        "gh".to_string(),
        "run".into(),
        "rerun".into(),
        run.to_string(),
        "--failed".into(),
        format!("--repo={repo}"),
    ]
    .to_vec()
}

/// Reruns the failed jobs of the PR head's failed Actions runs; returns how
/// many runs. Checks from other CI systems can't be rerun from here.
pub fn rerun_failed<R: Runner>(runner: &R, repo: &str, head: &str) -> Result<usize> {
    let failed = parse_failed_runs(&gh_json(runner, &runs_argv(repo, head))?)?;
    for run in &failed {
        gh_json(runner, &rerun_argv(repo, *run))?;
    }
    Ok(failed.len())
}

/// `owner/name` from a GitHub remote URL (https, scp-like or ssh form).
pub fn repo_of_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::FakeRunner;
    use serde_json::json;

    fn gh_fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/gh/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn parse(name: &str) -> PrInfo {
        parse_pr_list(&gh_fixture(name)).unwrap().unwrap()
    }

    #[test]
    fn recorded_rollups() {
        let fail = parse("pr-open-fail.json");
        assert_eq!((fail.state, fail.ci), (PrState::Open, Ci::Fail));
        assert_eq!(fail.number, 29923);
        assert_eq!(parse("pr-open-pending.json").ci, Ci::Pending);
        assert_eq!(parse("pr-open-pass.json").ci, Ci::Pass);
        let merged = parse("pr-merged.json");
        assert_eq!(merged.state, PrState::Merged);
        assert_eq!(merged.head_oid.len(), 40);
        assert_eq!(parse_pr_list(&gh_fixture("pr-none.json")).unwrap(), None);
    }

    #[test]
    fn recorded_fixtures_mix_check_shapes() {
        let raw: Value = serde_json::from_str(&gh_fixture("pr-open-pass.json")).unwrap();
        let kinds: Vec<&str> = raw[0]["statusCheckRollup"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["__typename"].as_str())
            .collect();
        assert!(kinds.contains(&"CheckRun") && kinds.contains(&"StatusContext"));
    }

    fn ctx(state: &str) -> Value {
        json!({"__typename": "StatusContext", "context": "x", "state": state})
    }

    fn run(status: &str, conclusion: &str) -> Value {
        json!({"__typename": "CheckRun", "name": "x", "status": status, "conclusion": conclusion})
    }

    #[test]
    fn status_context_states() {
        assert_eq!(rollup(&[ctx("SUCCESS")]), Ci::Pass);
        assert_eq!(rollup(&[ctx("FAILURE")]), Ci::Fail);
        assert_eq!(rollup(&[ctx("ERROR")]), Ci::Fail);
        assert_eq!(rollup(&[ctx("PENDING")]), Ci::Pending);
        assert_eq!(rollup(&[ctx("EXPECTED")]), Ci::Pending);
    }

    #[test]
    fn check_run_states() {
        assert_eq!(rollup(&[run("IN_PROGRESS", "")]), Ci::Pending);
        assert_eq!(rollup(&[run("COMPLETED", "SUCCESS")]), Ci::Pass);
        assert_eq!(rollup(&[run("COMPLETED", "SKIPPED")]), Ci::Pass);
        assert_eq!(rollup(&[run("COMPLETED", "NEUTRAL")]), Ci::Pass);
        for c in [
            "FAILURE",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STARTUP_FAILURE",
        ] {
            assert_eq!(rollup(&[run("COMPLETED", c)]), Ci::Fail, "{c}");
        }
    }

    #[test]
    fn rollup_priority_and_shapes() {
        assert_eq!(rollup(&[]), Ci::None);
        assert_eq!(rollup(&[ctx("SUCCESS"), run("QUEUED", "")]), Ci::Pending);
        assert_eq!(
            rollup(&[ctx("PENDING"), run("COMPLETED", "FAILURE")]),
            Ci::Fail
        );
        // Shape inferred without __typename; unknown shapes never pass.
        assert_eq!(rollup(&[json!({"state": "SUCCESS"})]), Ci::Pass);
        assert_eq!(
            rollup(&[json!({"status": "COMPLETED", "conclusion": "FAILURE"})]),
            Ci::Fail
        );
        assert_eq!(rollup(&[json!({"name": "mystery"})]), Ci::Pending);
    }

    fn info(state: PrState, ci: Ci) -> PrInfo {
        PrInfo {
            number: 12,
            state,
            url: String::new(),
            head_oid: String::new(),
            ci,
            is_draft: false,
            review_decision: None,
            mergeable: None,
        }
    }

    #[test]
    fn token_text() {
        assert_eq!(info(PrState::Open, Ci::Fail).token(), "#12 ✗");
        assert_eq!(info(PrState::Open, Ci::Pass).token(), "#12 ✓");
        assert_eq!(info(PrState::Open, Ci::Pending).token(), "#12 …");
        assert_eq!(info(PrState::Open, Ci::None).token(), "#12");
        assert_eq!(info(PrState::Merged, Ci::Fail).token(), "#12 ⇣");
        assert_eq!(info(PrState::Closed, Ci::Pass).token(), "#12 ⊘");
    }

    fn key() -> BranchKey {
        BranchKey {
            toplevel: "/r".into(),
            branch: "feat/x".into(),
            repo: "/r/.git".into(),
        }
    }

    #[test]
    fn cache_refreshes_open_after_30s_and_merged_after_5min() {
        let mut c = PrCache::default();
        let t0 = Instant::now();
        assert!(c.due(&key(), t0));
        c.store(key(), Some(info(PrState::Open, Ci::Pass)), t0);
        assert!(!c.due(&key(), t0 + Duration::from_secs(29)));
        assert!(c.due(&key(), t0 + Duration::from_secs(30)));
        c.store(key(), Some(info(PrState::Merged, Ci::Pass)), t0);
        assert!(!c.due(&key(), t0 + Duration::from_secs(299)));
        assert!(c.due(&key(), t0 + Duration::from_secs(300)));
        c.store(key(), None, t0);
        assert!(c.due(&key(), t0 + Duration::from_secs(30)));
    }

    #[test]
    fn entries_unreported_for_ten_minutes_are_evicted() {
        let mut c = PrCache::default();
        let t0 = Instant::now();
        let branch = |b: &str| BranchKey {
            toplevel: "/r".into(),
            branch: b.into(),
            repo: "/r/.git".into(),
        };
        for i in 0..1000 {
            c.store(branch(&format!("old-{i}")), None, t0);
        }
        c.store(branch("live"), Some(info(PrState::Open, Ci::Pass)), t0);
        let live: HashSet<BranchKey> = [branch("live")].into_iter().collect();
        let none = HashSet::new();
        // A tick where the live checkout failed to report (e.g. a pane read error)
        // must not lose its PR.
        c.evict_inactive(&none, t0 + Duration::from_secs(400));
        assert_eq!(c.len(), 1001, "nothing goes before EVICT_AFTER");
        c.evict_inactive(&live, t0 + Duration::from_secs(500));
        c.evict_inactive(&none, t0 + EVICT_AFTER);
        assert_eq!(c.len(), 1, "unreported for 10 min: evicted");
        assert!(c.get(&branch("live")).is_some(), "seen at +500s: kept");
        c.evict_inactive(&none, t0 + Duration::from_secs(500) + EVICT_AFTER);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn probe_branch_skips_detached_and_default_branches() {
        let f = FakeRunner::new();
        let mut argv = vec!["git", "-C", "/r"];
        argv.extend(BRANCH_ARGS);
        f.ok(&argv, None, "/r\nfeat/x\n/r/.git\n")
            .ok(&argv, None, "/r\nHEAD\n/r/.git\n")
            .ok(&argv, None, "/r\nmain\n/r/.git\n")
            .ok(&argv, None, "/r\nmaster\n/r/.git\n");
        assert_eq!(probe_branch(&f, "/r"), Some(("/r".into(), Some(key()))));
        assert_eq!(
            probe_branch(&f, "/r"),
            Some(("/r".into(), None)),
            "detached"
        );
        assert_eq!(probe_branch(&f, "/r"), Some(("/r".into(), None)));
        assert_eq!(probe_branch(&f, "/r"), Some(("/r".into(), None)));
    }

    #[test]
    fn gh_lookup_runs_in_checkout_and_reports_errors() {
        let f = FakeRunner::new();
        let argv = [
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
        f.ok(&argv, Some("/r"), &gh_fixture("pr-open-fail.json"));
        assert_eq!(gh_lookup(&f, "/r", "feat/x").unwrap().unwrap().ci, Ci::Fail);
        let f = FakeRunner::new();
        f.respond(
            &argv,
            Some("/r"),
            crate::core::runner::Output {
                status: 1,
                stdout: String::new(),
                stderr: "To get started with GitHub CLI, please run:  gh auth login\n".into(),
            },
        );
        let err = gh_lookup(&f, "/r", "feat/x").unwrap_err().to_string();
        assert!(err.contains("gh auth login"), "{err}");
    }

    #[test]
    fn a_failed_lookup_keeps_the_last_pr_and_waits_an_interval() {
        let key = BranchKey {
            toplevel: "/r".into(),
            branch: "feat/x".into(),
            repo: "/r/.git".into(),
        };
        let mut cache = PrCache::default();
        let t0 = Instant::now();
        cache.store(key.clone(), Some(parse("pr-open-fail.json")), t0);
        let t1 = t0 + OPEN_REFRESH;
        assert!(cache.due(&key, t1));
        cache.store_failure(key.clone(), t1);
        assert!(!cache.due(&key, t1 + Duration::from_secs(5)));
        assert!(cache.get(&key).is_some(), "last known PR kept");
        let fresh = BranchKey {
            toplevel: "/r".into(),
            branch: "feat/y".into(),
            repo: "/r/.git".into(),
        };
        cache.store_failure(fresh.clone(), t0);
        assert!(!cache.due(&fresh, t0 + Duration::from_secs(1)));
        assert!(cache.get(&fresh).is_none());
    }
    /// Recorded from mastra-ai/mastra: open (failing), open (passing),
    /// merged, and a branch with no PR. `gh pr list --head` agreed on all four.
    #[test]
    fn recorded_batch_reply() {
        let prs = parse_batch(&gh_fixture("batch.json"), 4).unwrap();
        let got: Vec<_> = prs
            .iter()
            .map(|p| p.as_ref().map(|p| (p.number, p.state, p.ci)))
            .collect();
        assert_eq!(
            got,
            vec![
                Some((30146, PrState::Open, Ci::Fail)),
                Some((29925, PrState::Open, Ci::Pass)),
                Some((29923, PrState::Merged, Ci::Fail)), // a CANCELLED check
                None,
            ]
        );
        let first = prs[0].as_ref().unwrap();
        assert_eq!(first.head_oid.len(), 40);
        assert!(first.url.ends_with("/pull/30146"), "{}", first.url);
        let review: Vec<_> = prs
            .iter()
            .flatten()
            .map(|p| {
                (
                    p.is_draft,
                    p.review_decision.as_deref(),
                    p.mergeable.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            review,
            [
                (false, Some("CHANGES_REQUESTED"), Some("MERGEABLE")),
                (false, Some("APPROVED"), Some("MERGEABLE")),
                (false, Some("APPROVED"), Some("UNKNOWN")),
            ]
        );
        let draft = r#"{"data":{"repository":{"b0":{"nodes":[{"number":5,"state":"OPEN","url":"u","headRefOid":"h","isDraft":true,"reviewDecision":null}]}}}}"#;
        let pr = parse_batch(draft, 1).unwrap()[0].clone().unwrap();
        assert!(pr.is_draft);
        assert_eq!((pr.review_decision, pr.mergeable), (None, None));
    }

    #[test]
    fn batch_reply_errors() {
        let no_repo = r#"{"data":{"repository":null},"errors":[{"message":"Could not resolve to a Repository"}]}"#;
        let err = parse_batch(no_repo, 1).unwrap_err().to_string();
        assert!(err.contains("Could not resolve"), "{err}");
        let missing = r#"{"data":{"repository":{"b0":{"nodes":[]}}}}"#;
        assert!(
            parse_batch(missing, 2).is_err(),
            "a missing alias fails the batch"
        );
        let no_checks = r#"{"data":{"repository":{"b0":{"nodes":[{"number":5,"state":"OPEN","url":"u","headRefOid":"h","commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}]}}}}"#;
        assert_eq!(
            parse_batch(no_checks, 1).unwrap()[0].as_ref().unwrap().ci,
            Ci::None
        );
        assert!(parse_batch("nope", 1).is_err());
    }

    #[test]
    fn batch_argv_passes_branches_as_variables_only() {
        let argv = batch_argv(&["feat/x", "a\"}){evil}"]);
        let query = argv.iter().find(|a| a.starts_with("query=")).unwrap();
        assert!(
            !query.contains("feat/x") && !query.contains("evil"),
            "{query}"
        );
        assert!(query.contains("$b0:String!,$b1:String!"), "{query}");
        assert!(query.contains("b1:pullRequests(headRefName:$b1,states:[OPEN,MERGED,CLOSED]"));
        let tail: Vec<&str> = argv[argv.len() - 4..].iter().map(String::as_str).collect();
        assert_eq!(tail, ["-f", "b0=feat/x", "-f", "b1=a\"}){evil}"]);
        assert_eq!(
            &argv[..7],
            [
                "gh",
                "api",
                "graphql",
                "-F",
                "owner={owner}",
                "-F",
                "name={repo}"
            ]
        );
    }

    #[test]
    fn batch_lookup_runs_in_checkout_and_reports_errors() {
        let argv = batch_argv(&["feat/x"]);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let reply = r#"{"data":{"repository":{"b0":{"nodes":[]}}}}"#;
        let f = FakeRunner::new();
        f.ok(&argv, Some("/r"), reply);
        assert_eq!(gh_lookup_batch(&f, "/r", &["feat/x"]).unwrap(), vec![None]);
        let f = FakeRunner::new();
        f.respond(
            &argv,
            Some("/r"),
            crate::core::runner::Output {
                status: 1,
                stdout: String::new(),
                stderr: "gh auth login required\n".into(),
            },
        );
        let err = gh_lookup_batch(&f, "/r", &["feat/x"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("gh auth login"), "{err}");
    }

    const LISTS: &str = include_str!("../../tests/fixtures/gh/pr-lists.json");
    const DETAIL: &str = include_str!("../../tests/fixtures/gh/pr-detail.json");

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn pr_lists_parse_both_searches_with_totals() {
        let l = parse_lists(LISTS).unwrap();
        assert_eq!(l.viewer, "me");
        assert_eq!((l.mine.len(), l.mine_total), (2, 2));
        assert_eq!((l.assigned.len(), l.assigned_total), (6, 70));
        let own = &l.mine[0];
        assert_eq!((own.number, own.ci), (40001, Ci::Fail));
        assert_eq!(own.id, "PR_mine1");
        let r = &l.assigned[0];
        assert_eq!(r.repo, "mastra-ai/mastra");
        assert_eq!(r.head_repo.as_deref(), Some("mastra-ai/mastra"));
        assert_eq!(r.mergeable.as_deref(), Some("CONFLICTING"));
        assert_eq!(r.review_decision.as_deref(), Some("APPROVED"));
        assert!(r.reviewers.iter().any(|x| x.bot && x.login == "review-bot"));
        assert_eq!(l.assigned[5].head_repo.as_deref(), Some("user99/mastra"));
    }

    #[test]
    fn your_review_on_rows_counts_new_commits_or_says_earlier_head() {
        let l = parse_lists(LISTS).unwrap();
        // Reviewed the third-newest commit: two since.
        let found = l.assigned[0].your_review.clone().unwrap();
        assert_eq!(found.text, "approved · 2 new commits");
        // The reviewed commit is gone and the history fits the window: rewritten.
        let gone = l.assigned[1].your_review.clone().unwrap();
        assert_eq!(gone.text, "changes requested · reviewed an earlier head");
        // Reviewed the head itself.
        let current = l.assigned[2].your_review.clone().unwrap();
        assert_eq!(current.text, "commented");
        // No viewer review at all.
        assert_eq!(l.assigned[3].your_review, None);
    }

    #[test]
    fn your_review_comes_from_viewer_latest_review_not_the_last_twenty() {
        let mut v: Value = serde_json::from_str(LISTS).unwrap();
        let node = &mut v["data"]["assigned"]["nodes"][0];
        // More reviewers than the 20 listed: the viewer's review is not among them.
        node["latestReviews"]["nodes"] = serde_json::json!([]);
        let l = parse_lists(&v.to_string()).unwrap();
        let r = l.assigned[0].your_review.clone().unwrap();
        assert_eq!(r.text, "approved · 2 new commits");
        // A pending (unsubmitted) review is not a review yet.
        let mut v: Value = serde_json::from_str(LISTS).unwrap();
        v["data"]["assigned"]["nodes"][0]["viewerLatestReview"]["state"] = "PENDING".into();
        assert_eq!(
            parse_lists(&v.to_string()).unwrap().assigned[0].your_review,
            None
        );
    }

    #[test]
    fn your_review_never_claims_a_count_beyond_the_window() {
        let commits: Vec<String> = (0..30).map(|i| format!("c{i}")).collect();
        let r = your_review("APPROVED", Some("old"), &commits, 120, 30);
        assert_eq!(r.text, "approved · reviewed an earlier head (30+ commits)");
        let r = your_review("APPROVED", Some("c28"), &commits, 120, 30);
        assert_eq!(r.text, "approved · 1 new commit");
        let r = your_review("COMMENTED", None, &commits, 30, 30);
        assert_eq!(r.text, "commented · reviewed an earlier head");
    }

    #[test]
    fn pr_detail_parses_body_checks_and_reviewers_with_bots() {
        let d = parse_detail(DETAIL).unwrap();
        assert_eq!(d.pr.number, 30148);
        assert!(d.body.contains("widget report"));
        assert_eq!(d.base_ref, "main");
        assert_eq!(d.failing, vec!["Docs E2E tests".to_string()]);
        assert_eq!(d.pr.ci, Ci::Fail);
        let requested: Vec<&str> =
            d.pr.reviewers
                .iter()
                .filter(|r| r.state == "REQUESTED")
                .map(|r| r.login.as_str())
                .collect();
        assert!(requested.contains(&"core"), "{requested:?}");
        assert!(d
            .pr
            .reviewers
            .iter()
            .any(|r| r.bot && r.state != "REQUESTED"));
    }

    #[test]
    fn lists_and_detail_are_reads_with_variables() {
        let argv = lists_argv();
        assert_eq!(strs(&argv)[..4], ["gh", "api", "graphql", "-f"]);
        assert!(argv[4].starts_with("query=query{"));
        assert!(!argv[4].contains("mutation"));
        assert!(argv[4].contains("assignee:@me"));
        let argv = detail_argv("mastra-ai/mastra", 7).unwrap();
        assert!(argv.contains(&"owner=mastra-ai".to_string()));
        assert!(argv.contains(&"number=7".to_string()));
        assert!(detail_argv("@etc/x", 1).is_err());
        assert!(detail_argv("nope", 1).is_err());
    }

    #[test]
    fn only_the_newest_commits_failed_runs_are_rerun() {
        let runs = r#"[
            {"databaseId":3,"headSha":"new","conclusion":"failure"},
            {"databaseId":2,"headSha":"new","conclusion":"success"},
            {"databaseId":5,"headSha":"new","conclusion":"timed_out"},
            {"databaseId":1,"headSha":"old","conclusion":"failure"}
        ]"#;
        assert_eq!(parse_failed_runs(runs).unwrap(), [3, 5]);
        assert!(parse_failed_runs("[]").unwrap().is_empty());

        let f = FakeRunner::new();
        f.ok(&strs(&runs_argv("o/r", "feat/7")), None, runs)
            .ok(&strs(&rerun_argv("o/r", 3)), None, "")
            .ok(&strs(&rerun_argv("o/r", 5)), None, "");
        assert_eq!(rerun_failed(&f, "o/r", "feat/7").unwrap(), 2);
        let calls = f.argvs();
        assert_eq!(calls[0], "gh run list --repo=o/r --branch=feat/7 --limit=30 --json=databaseId,headSha,conclusion");
        assert_eq!(
            calls[1..],
            [
                "gh run rerun 3 --failed --repo=o/r",
                "gh run rerun 5 --failed --repo=o/r"
            ]
        );
    }

    #[test]
    fn remotes_parse_in_all_three_forms() {
        for url in [
            "https://github.com/Mastra-AI/mastra.git",
            "https://github.com/Mastra-AI/mastra",
            "git@github.com:Mastra-AI/mastra.git",
            "ssh://git@github.com/Mastra-AI/mastra.git\n",
        ] {
            assert_eq!(
                repo_of_remote(url).as_deref(),
                Some("Mastra-AI/mastra"),
                "{url}"
            );
        }
        assert_eq!(repo_of_remote("https://gitlab.com/a/b.git"), None);
        assert_eq!(repo_of_remote("git@github.com:a"), None);
    }
}
