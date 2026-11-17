use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::core::runner::Runner;
use crate::integrations::github::EVICT_AFTER;

const ENDPOINT: &str = "https://api.linear.app/graphql";
const FIELDS: &str = "identifier title description url branchName state { name }";
pub const ISSUE_TTL: Duration = Duration::from_secs(300);
/// After a failed fetch, wait this long before calling Linear again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueState {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub url: String,
    #[serde(default, rename = "branchName")]
    pub branch_name: Option<String>,
    #[serde(default)]
    pub state: Option<IssueState>,
}

impl Issue {
    pub fn state_name(&self) -> &str {
        self.state.as_ref().map_or("", |s| s.name.as_str())
    }
}

/// `[a-z][a-z0-9]{1,9}-[0-9]+` (any case), uppercased, if the whole input is one.
pub fn parse_issue_id(input: &str) -> Option<String> {
    let s = input.trim();
    match match_id_at(s.as_bytes(), 0) {
        Some(end) if end == s.len() => Some(s.to_ascii_uppercase()),
        _ => None,
    }
}

/// The first issue id in a branch name that starts at the beginning or after
/// `/`, `_` or `-`, and ends at the end or before one of them.
pub fn issue_id_from_branch(branch: &str) -> Option<String> {
    let b = branch.as_bytes();
    let sep = |c: u8| matches!(c, b'/' | b'_' | b'-');
    (0..b.len())
        .filter(|&i| i == 0 || sep(b[i - 1]))
        .find_map(|i| {
            let end = match_id_at(b, i)?;
            (end == b.len() || sep(b[end])).then(|| branch[i..end].to_ascii_uppercase())
        })
}

/// Matches the id pattern at `start`; returns the end of the digit run.
/// Prefix and digit runs are maximal, which is all the regex could match here.
fn match_id_at(b: &[u8], start: usize) -> Option<usize> {
    if !b.get(start)?.is_ascii_alphabetic() {
        return None;
    }
    let mut i = start;
    while i < b.len() && b[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if !(2..=10).contains(&(i - start)) || b.get(i) != Some(&b'-') {
        return None;
    }
    let digits = i + 1;
    let mut j = digits;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    (j > digits).then_some(j)
}

/// `LINEAR_API_KEY` from the environment, else from `$HERDR_PLUGIN_CONFIG_DIR/.env`.
pub fn load_key() -> Option<String> {
    crate::core::env_or_dotenv("LINEAR_API_KEY")
}

/// One aliased query; ids travel as variables, never spliced into the query text.
pub fn build_request(ids: &[String]) -> String {
    let params: Vec<String> = (0..ids.len()).map(|i| format!("$a{i}: String!")).collect();
    let fields: Vec<String> = (0..ids.len())
        .map(|i| format!("a{i}: issue(id: $a{i}) {{ {FIELDS} }}"))
        .collect();
    let vars: serde_json::Map<String, Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (format!("a{i}"), Value::String(id.clone())))
        .collect();
    json!({
        "query": format!("query({}) {{ {} }}", params.join(", "), fields.join(" ")),
        "variables": vars,
    })
    .to_string()
}

#[derive(Debug, Default, PartialEq)]
struct Parsed {
    found: HashMap<usize, Issue>,
    /// Aliases Linear reported as not found.
    missing: Vec<usize>,
}

fn parse_response(text: &str) -> Result<Parsed> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| anyhow!("Linear: invalid JSON reply: {e}"))?;
    let alias = |s: &str| s.strip_prefix('a').and_then(|n| n.parse::<usize>().ok());
    let mut parsed = Parsed::default();
    if let Some(data) = value.get("data").and_then(Value::as_object) {
        for (key, v) in data {
            if let (Some(i), false) = (alias(key), v.is_null()) {
                parsed.found.insert(i, serde_json::from_value(v.clone())?);
            }
        }
    }
    let mut other = Vec::new();
    for err in value
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let at = err
            .get("path")
            .and_then(|p| p.get(0))
            .and_then(Value::as_str)
            .and_then(alias);
        match at {
            Some(i) => parsed.missing.push(i),
            None => other.push(
                err.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            ),
        }
    }
    if !other.is_empty() {
        bail!("Linear: {}", other.join("; "));
    }
    Ok(parsed)
}

/// Looks up issues by id. Ids Linear doesn't know are left out. One unknown
/// id nulls the whole reply, so the query is retried once without them.
pub fn fetch_issues<R: Runner>(
    runner: &R,
    key: &str,
    ids: &[String],
) -> Result<HashMap<String, Issue>> {
    let mut ids: Vec<String> = ids.to_vec();
    let mut out = HashMap::new();
    for _ in 0..2 {
        if ids.is_empty() {
            break;
        }
        let parsed = parse_response(&post(runner, key, &build_request(&ids))?)?;
        for (i, issue) in parsed.found {
            out.insert(ids[i].clone(), issue);
        }
        if parsed.missing.is_empty() || !out.is_empty() {
            break;
        }
        ids = ids
            .iter()
            .enumerate()
            .filter(|(i, _)| !parsed.missing.contains(i))
            .map(|(_, id)| id.clone())
            .collect();
    }
    Ok(out)
}

const POST: [&str; 7] = [
    "curl",
    "-sS",
    ENDPOINT,
    "-H",
    "Content-Type: application/json",
    "-H",
    "@-",
];

/// The key goes to curl as a header on stdin (`-H @-`), so it never shows in `ps`.
fn post<R: Runner>(runner: &R, key: &str, body: &str) -> Result<String> {
    let header = format!("Authorization: {key}\n");
    let mut argv = POST.to_vec();
    argv.extend(["--data-binary", body]);
    let out = runner.run(&argv, Some(header.as_bytes()), None)?;
    if !out.success() {
        bail!("Linear: curl exited {}: {}", out.status, out.stderr.trim());
    }
    Ok(out.stdout)
}

/// The argv `post` runs for `body`, for FakeRunner replies.
#[cfg(test)]
pub fn post_argv(body: &str) -> Vec<String> {
    POST.iter()
        .copied()
        .chain(["--data-binary", body])
        .map(String::from)
        .collect()
}

/// Issues by id, refreshed every 5 minutes; unknown ids are cached as misses.
#[derive(Debug, Default)]
pub struct IssueCache {
    entries: HashMap<String, (Option<Issue>, Instant)>,
    failed_at: Option<Instant>,
    /// When an agent's branch last named this id.
    seen: HashMap<String, Instant>,
}

impl IssueCache {
    /// Ids that need a lookup now, or none while backing off after an error.
    pub fn due(&self, ids: &[String], now: Instant) -> Vec<String> {
        if self
            .failed_at
            .is_some_and(|t| now.duration_since(t) < RETRY_AFTER)
        {
            return Vec::new();
        }
        ids.iter()
            .filter(|id| {
                self.entries
                    .get(*id)
                    .is_none_or(|(_, at)| now.duration_since(*at) >= ISSUE_TTL)
            })
            .cloned()
            .collect()
    }

    pub fn store(&mut self, asked: &[String], found: HashMap<String, Issue>, now: Instant) {
        self.failed_at = None;
        let mut found = found;
        for id in asked {
            self.seen.insert(id.clone(), now);
            self.entries.insert(id.clone(), (found.remove(id), now));
        }
    }

    pub fn failed(&mut self, now: Instant) {
        self.failed_at = Some(now);
    }

    pub fn get(&self, id: &str) -> Option<&Issue> {
        self.entries.get(id).and_then(|(i, _)| i.as_ref())
    }

    /// Marks `active` as seen now and drops issues no agent's branch has named
    /// for `EVICT_AFTER`.
    pub fn evict_inactive(&mut self, active: &[String], now: Instant) {
        for id in active {
            self.seen.insert(id.clone(), now);
        }
        self.seen
            .retain(|_, at| now.duration_since(*at) < EVICT_AFTER);
        let seen = &self.seen;
        self.entries.retain(|id, _| seen.contains_key(id));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

const MY_FIELDS: &str = "id identifier title url branchName priority \
     state { id name type } team { id key } project { name } assignee { id name } \
     labels { nodes { name } }";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    /// `triage`, `backlog`, `unstarted`, `started`, `completed`, `canceled`, `duplicate`.
    #[serde(rename = "type")]
    pub kind: String,
}

impl WorkflowState {
    pub fn closed(&self) -> bool {
        matches!(self.kind.as_str(), "completed" | "canceled" | "duplicate")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Team {
    pub id: String,
    pub key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Named {
    #[serde(default)]
    pub id: String,
    pub name: String,
}

/// An issue as the Issues tab shows it; `description` only from the detail read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MyIssue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub branch_name: Option<String>,
    /// 0 none, 1 urgent … 4 low.
    #[serde(default)]
    pub priority: u8,
    pub state: WorkflowState,
    pub team: Team,
    #[serde(default)]
    pub project: Option<Named>,
    #[serde(default)]
    pub assignee: Option<Named>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "label_names")]
    pub labels: Vec<String>,
}

fn label_names<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Labels {
        Graph { nodes: Vec<Named> },
        Plain(Vec<String>),
    }
    Ok(match Option::<Labels>::deserialize(d)? {
        Some(Labels::Graph { nodes }) => nodes.into_iter().map(|n| n.name).collect(),
        Some(Labels::Plain(v)) => v,
        None => Vec::new(),
    })
}

impl MyIssue {
    pub fn as_issue(&self) -> Issue {
        Issue {
            identifier: self.identifier.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            url: self.url.clone(),
            branch_name: self.branch_name.clone(),
            state: Some(IssueState {
                name: self.state.name.clone(),
            }),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MyIssues {
    pub viewer_id: String,
    pub issues: Vec<MyIssue>,
    /// Linear has more than the first 100.
    #[serde(default)]
    pub more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamState {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub position: f64,
}

/// `data` of a GraphQL reply, or every error message.
fn data_of(text: &str) -> Result<Value> {
    let mut value: Value =
        serde_json::from_str(text).map_err(|e| anyhow!("Linear: invalid JSON reply: {e}"))?;
    let errors: Vec<String> = value
        .get("errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string()
        })
        .collect();
    if !errors.is_empty() {
        bail!("Linear: {}", errors.join("; "));
    }
    match value.get_mut("data").map(Value::take) {
        Some(d) if !d.is_null() => Ok(d),
        _ => bail!("Linear: reply without data"),
    }
}

fn request(query: &str, variables: Value) -> String {
    json!({ "query": query, "variables": variables }).to_string()
}

pub fn my_issues_request() -> String {
    request(
        &format!(
            "query {{ viewer {{ id assignedIssues(first: 100, filter: {{ state: {{ type: {{ \
             nin: [\"completed\", \"canceled\", \"duplicate\"] }} }} }}) {{ pageInfo {{ hasNextPage }} \
             nodes {{ {MY_FIELDS} }} }} }} }}"
        ),
        json!({}),
    )
}

pub fn parse_my_issues(text: &str) -> Result<MyIssues> {
    let d = data_of(text)?;
    let viewer = &d["viewer"];
    Ok(MyIssues {
        viewer_id: viewer["id"].as_str().unwrap_or_default().to_string(),
        issues: serde_json::from_value(viewer["assignedIssues"]["nodes"].clone())?,
        more: viewer["assignedIssues"]["pageInfo"]["hasNextPage"].as_bool() == Some(true),
    })
}

/// The viewer's open issues (not completed, canceled or duplicate).
pub fn fetch_my_issues<R: Runner>(runner: &R, key: &str) -> Result<MyIssues> {
    parse_my_issues(&post(runner, key, &my_issues_request())?)
}

pub fn detail_request(id: &str) -> String {
    request(
        &format!(
            "query($id: String!) {{ viewer {{ id name }} issue(id: $id) {{ {MY_FIELDS} description }} }}"
        ),
        json!({ "id": id }),
    )
}

/// An issue's full read plus who is asking, for `s`'s "someone else's" check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueDetail {
    pub issue: MyIssue,
    pub viewer_id: String,
    pub viewer_name: String,
}

pub fn parse_detail(text: &str) -> Result<IssueDetail> {
    let d = data_of(text)?;
    if d["issue"].is_null() {
        bail!("Linear: issue not found");
    }
    Ok(IssueDetail {
        issue: serde_json::from_value(d["issue"].clone())?,
        viewer_id: d["viewer"]["id"].as_str().unwrap_or_default().to_string(),
        viewer_name: d["viewer"]["name"].as_str().unwrap_or_default().to_string(),
    })
}

/// One issue with description and labels; also the fresh read `s` re-checks.
pub fn fetch_issue_detail<R: Runner>(runner: &R, key: &str, id: &str) -> Result<IssueDetail> {
    parse_detail(&post(runner, key, &detail_request(id))?)
}

pub fn team_states_request(team_id: &str) -> String {
    request(
        "query($id: String!) { team(id: $id) { states { nodes { id name type position } } } }",
        json!({ "id": team_id }),
    )
}

pub fn parse_team_states(text: &str) -> Result<Vec<TeamState>> {
    let d = data_of(text)?;
    let mut states: Vec<TeamState> = serde_json::from_value(d["team"]["states"]["nodes"].clone())?;
    states.sort_by(|a, b| a.position.total_cmp(&b.position));
    Ok(states)
}

/// The team's workflow states, by position.
pub fn team_states<R: Runner>(runner: &R, key: &str, team_id: &str) -> Result<Vec<TeamState>> {
    parse_team_states(&post(runner, key, &team_states_request(team_id))?)
}

/// `issueUpdate` with exactly `input`; ids travel as variables.
pub fn update_request(issue_id: &str, input: Value) -> String {
    request(
        "mutation($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success } }",
        json!({ "id": issue_id, "input": input }),
    )
}

fn update<R: Runner>(runner: &R, key: &str, issue_id: &str, input: Value) -> Result<()> {
    let d = data_of(&post(runner, key, &update_request(issue_id, input))?)?;
    if d["issueUpdate"]["success"].as_bool() != Some(true) {
        bail!("Linear: issueUpdate did not succeed");
    }
    Ok(())
}

pub fn set_state<R: Runner>(runner: &R, key: &str, issue_id: &str, state_id: &str) -> Result<()> {
    update(runner, key, issue_id, json!({ "stateId": state_id }))
}

/// Moves to `state_id` and/or assigns `assignee_id`; sends nothing when both are `None`.
pub fn claim<R: Runner>(
    runner: &R,
    key: &str,
    issue_id: &str,
    state_id: Option<&str>,
    assignee_id: Option<&str>,
) -> Result<()> {
    let mut input = serde_json::Map::new();
    if let Some(s) = state_id {
        input.insert("stateId".into(), s.into());
    }
    if let Some(a) = assignee_id {
        input.insert("assigneeId".into(), a.into());
    }
    if input.is_empty() {
        return Ok(());
    }
    update(runner, key, issue_id, Value::Object(input))
}

pub fn first_prompt(issue: &Issue) -> String {
    let description = issue
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("(no description)");
    format!(
        "Linear issue {}: {}\n{}\n\n{}",
        issue.identifier, issue.title, issue.url, description
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::{FakeRunner, Output};

    const ISSUES: &str = include_str!("../../tests/fixtures/linear/issues.json");
    const NOT_FOUND: &str = include_str!("../../tests/fixtures/linear/issues-not-found.json");
    const BRANCH: &str = "user/eng-325-widget-export-cli";
    const KEY: &str = "lin_api_TESTKEYDONOTUSE";

    fn issue() -> Issue {
        Issue {
            identifier: "ENG-325".into(),
            title: "\"Widget export\": CLI flag".into(),
            description: Some("New CLI capabilities".into()),
            url: "https://linear.app/example/issue/ENG-325/widget-export-cli".into(),
            branch_name: Some(BRANCH.into()),
            state: Some(IssueState {
                name: "Done".into(),
            }),
        }
    }

    #[test]
    fn branch_parsing_table() {
        let cases = [
            (BRANCH, Some("ENG-325")),
            ("feat/ABC-12", Some("ABC-12")),
            ("abc-12-x", Some("ABC-12")),
            ("main", None),
            ("fix/abc", None),
            // Matches the pattern; Linear then reports it unknown and it stays unmapped.
            ("release-2024", Some("RELEASE-2024")),
            ("x-1", None),
            ("abcdefghijk-1", None),
            ("feat/abc-12x", None),
            ("team_eng-7_fix", Some("ENG-7")),
            ("2024-01/eng-3", Some("ENG-3")),
        ];
        for (branch, want) in cases {
            assert_eq!(
                issue_id_from_branch(branch).as_deref(),
                want,
                "branch {branch}"
            );
        }
    }

    #[test]
    fn pasted_ids_are_validated_and_uppercased() {
        assert_eq!(parse_issue_id(" eng-325 ").as_deref(), Some("ENG-325"));
        assert_eq!(parse_issue_id("ENG-325"), Some("ENG-325".into()));
        assert_eq!(parse_issue_id("ENG-325 x"), None);
        assert_eq!(parse_issue_id("\") { x }"), None);
        assert_eq!(parse_issue_id(""), None);
    }

    #[test]
    fn query_is_aliased_with_ids_as_variables() {
        let body: Value =
            serde_json::from_str(&build_request(&["ENG-325".into(), "ENG-7".into()])).unwrap();
        let q = body["query"].as_str().unwrap();
        assert!(q.starts_with("query($a0: String!, $a1: String!) {"), "{q}");
        assert!(q.contains(
            "a0: issue(id: $a0) { identifier title description url branchName state { name } }"
        ));
        assert!(q.contains("a1: issue(id: $a1)"));
        assert!(!q.contains("ENG-325"));
        assert_eq!(body["variables"]["a0"], "ENG-325");
        assert_eq!(body["variables"]["a1"], "ENG-7");
    }

    fn curl_argv(body: &str) -> Vec<&str> {
        vec![
            "curl",
            "-sS",
            ENDPOINT,
            "-H",
            "Content-Type: application/json",
            "-H",
            "@-",
            "--data-binary",
            body,
        ]
    }

    #[test]
    fn key_goes_on_stdin_never_in_argv() {
        let f = FakeRunner::new();
        let ids = vec!["ENG-325".to_string()];
        let body = build_request(&ids);
        f.ok(&curl_argv(&body), None, ISSUES);
        let got = fetch_issues(&f, KEY, &ids).unwrap();
        assert_eq!(got["ENG-325"], issue());
        let calls = f.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].argv.iter().all(|a| !a.contains(KEY)));
        assert_eq!(
            calls[0].stdin.as_deref(),
            Some(format!("Authorization: {KEY}\n").as_bytes())
        );
    }

    #[test]
    fn parses_recorded_reply_and_retries_without_unknown_ids() {
        let f = FakeRunner::new();
        let both = vec!["ENG-325".to_string(), "ZZZ-999999".to_string()];
        let one = vec!["ENG-325".to_string()];
        f.ok(&curl_argv(&build_request(&both)), None, NOT_FOUND).ok(
            &curl_argv(&build_request(&one)),
            None,
            ISSUES,
        );
        let got = fetch_issues(&f, KEY, &both).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got["ENG-325"].branch_name.as_deref(), Some(BRANCH));
        assert_eq!(got["ENG-325"].state_name(), "Done");
        assert_eq!(f.calls().len(), 2);
    }

    #[test]
    fn non_alias_errors_and_curl_failures_are_errors() {
        let auth = r#"{"errors":[{"message":"Authentication required, not authenticated"}]}"#;
        assert!(parse_response(auth)
            .unwrap_err()
            .to_string()
            .contains("Authentication required"));
        let f = FakeRunner::new();
        let ids = vec!["ENG-325".to_string()];
        f.respond(
            &curl_argv(&build_request(&ids)),
            None,
            Output {
                status: 6,
                stdout: String::new(),
                stderr: "Could not resolve host".into(),
            },
        );
        let err = fetch_issues(&f, KEY, &ids).unwrap_err().to_string();
        assert!(err.contains("curl exited 6"), "{err}");
    }

    #[test]
    fn cache_refreshes_after_five_minutes_and_backs_off_after_errors() {
        let t0 = Instant::now();
        let ids = vec!["ENG-325".to_string(), "ZZZ-1".to_string()];
        let mut cache = IssueCache::default();
        assert_eq!(cache.due(&ids, t0), ids);
        cache.store(&ids, HashMap::from([("ENG-325".to_string(), issue())]), t0);
        assert!(cache.due(&ids, t0 + Duration::from_secs(299)).is_empty());
        assert_eq!(cache.get("ENG-325"), Some(&issue()));
        assert_eq!(cache.get("ZZZ-1"), None);
        assert_eq!(cache.due(&ids, t0 + ISSUE_TTL), ids);
        let t1 = t0 + ISSUE_TTL;
        cache.failed(t1);
        assert!(cache.due(&ids, t1 + Duration::from_secs(29)).is_empty());
        assert_eq!(cache.due(&ids, t1 + RETRY_AFTER), ids);
    }

    #[test]
    fn issues_no_branch_names_for_ten_minutes_are_evicted() {
        let t0 = Instant::now();
        let old: Vec<String> = (0..500).map(|i| format!("OLD-{i}")).collect();
        let mut cache = IssueCache::default();
        cache.store(&old, HashMap::new(), t0);
        let live = vec!["ENG-325".to_string()];
        cache.store(&live, HashMap::from([("ENG-325".to_string(), issue())]), t0);
        cache.evict_inactive(&[], t0 + Duration::from_secs(400));
        assert_eq!(cache.len(), 501, "a tick without the agent loses nothing");
        cache.evict_inactive(&live, t0 + Duration::from_secs(500));
        cache.evict_inactive(&[], t0 + EVICT_AFTER);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("ENG-325"), Some(&issue()), "recently named: kept");
    }

    #[test]
    fn first_prompt_format() {
        assert_eq!(
            first_prompt(&issue()),
            "Linear issue ENG-325: \"Widget export\": CLI flag\n\
             https://linear.app/example/issue/ENG-325/widget-export-cli\n\
             \n\
             New CLI capabilities"
        );
        let mut bare = issue();
        bare.description = None;
        assert!(first_prompt(&bare).ends_with("\n\n(no description)"));
    }

    const MINE: &str = include_str!("../../tests/fixtures/linear/my-issues.json");
    const DETAIL: &str = include_str!("../../tests/fixtures/linear/issue-detail.json");
    const STATES: &str = include_str!("../../tests/fixtures/linear/team-states.json");
    const OK: &str = r#"{"data":{"issueUpdate":{"success":true}}}"#;

    fn fake(body: &str, reply: &str) -> FakeRunner {
        let r = FakeRunner::new();
        let argv = post_argv(body);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        r.ok(&argv, None, reply);
        r
    }

    fn assert_key_only_on_stdin(r: &FakeRunner) {
        for c in r.calls() {
            assert!(c.argv.iter().all(|a| !a.contains(KEY)), "key in argv");
            let stdin = String::from_utf8(c.stdin.clone().unwrap()).unwrap();
            assert_eq!(stdin, format!("Authorization: {KEY}\n"));
        }
    }

    #[test]
    fn my_issues_parse_the_recorded_shape() {
        let r = fake(&my_issues_request(), MINE);
        let mine = fetch_my_issues(&r, KEY).unwrap();
        assert_eq!(mine.viewer_id, "user-viewer");
        assert_eq!(mine.issues.len(), 7);
        let first = &mine.issues[0];
        assert_eq!(first.identifier, "ENG-1");
        assert_eq!(first.state.kind, "backlog");
        assert_eq!(first.team.id, "team-1111");
        assert_eq!(first.project.as_ref().unwrap().name, "Documentation");
        assert_eq!(first.assignee.as_ref().unwrap().id, "user-viewer");
        assert_eq!(first.labels, ["docs", "good first issue"]);
        assert!(mine.issues[1].labels.is_empty());
        assert_eq!(mine.issues[5].branch_name, None);
        assert!(
            my_issues_request().contains(r#"nin: [\"completed\", \"canceled\", \"duplicate\"]"#)
        );
        assert_key_only_on_stdin(&r);
    }

    #[test]
    fn detail_reads_description_and_labels() {
        let r = fake(&detail_request("issue-1"), DETAIL);
        let d = fetch_issue_detail(&r, KEY, "issue-1").unwrap();
        assert_eq!(d.viewer_id, "user-viewer");
        let d = d.issue;
        assert!(d.description.unwrap().starts_with("Write the guide."));
        assert_eq!(d.labels, ["docs", "good first issue"]);
        assert!(detail_request("issue-1").contains(r#""variables":{"id":"issue-1"}"#));
        assert!(parse_detail(r#"{"data":{"issue":null}}"#).is_err());
        assert!(parse_detail(r#"{"errors":[{"message":"nope"}]}"#)
            .unwrap_err()
            .to_string()
            .contains("nope"));
    }

    #[test]
    fn team_states_come_by_position() {
        let r = fake(&team_states_request("team-1111"), STATES);
        let s = team_states(&r, KEY, "team-1111").unwrap();
        let names: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Backlog",
                "Todo",
                "In Progress",
                "Done",
                "Canceled",
                "Duplicate",
                "In Review"
            ]
        );
    }

    #[test]
    fn mutations_carry_ids_as_variables() {
        let body = update_request("issue-1", json!({ "stateId": "st-todo" }));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v["query"].as_str().unwrap().starts_with("mutation("));
        assert!(!v["query"].as_str().unwrap().contains("issue-1"));
        assert_eq!(v["variables"]["id"], "issue-1");
        assert_eq!(v["variables"]["input"], json!({ "stateId": "st-todo" }));

        let r = fake(&body, OK);
        set_state(&r, KEY, "issue-1", "st-todo").unwrap();
        assert_key_only_on_stdin(&r);

        let both = update_request(
            "issue-1",
            json!({ "stateId": "st-prog", "assigneeId": "user-viewer" }),
        );
        let r = fake(&both, OK);
        claim(&r, KEY, "issue-1", Some("st-prog"), Some("user-viewer")).unwrap();
        assert_eq!(r.calls().len(), 1);

        let r = FakeRunner::new();
        claim(&r, KEY, "issue-1", None, None).unwrap();
        assert!(r.calls().is_empty(), "nothing to claim sends nothing");

        let r = fake(&body, r#"{"data":{"issueUpdate":{"success":false}}}"#);
        assert!(set_state(&r, KEY, "issue-1", "st-todo").is_err());
    }
}
