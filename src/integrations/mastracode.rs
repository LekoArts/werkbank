//! Read-only access to Mastra Code's local memory: which thread a pane runs, and
//! a recap of that thread from its observational memory and task/goal state.
//!
//! The database is opened only by `sqlite3 -readonly` on a `mode=ro` URI; nothing
//! here writes to it, checkpoints it, or reads `mastra_messages`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use ratatui::text::Span;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::runner::Runner;

pub const DB_ENV: &str = "WERKBANK_MASTRA_DB";

/// A recap lists at most this many observation lines.
pub const RECAP_LINES: usize = 2;
const GOAL_TITLE_CHARS: usize = 80;
/// Thread updates this long after the last observation make the recap stale.
const STALE_AFTER_MS: u64 = 5 * 60 * 1000;
const MAX_ID_LEN: usize = 64;

/// `WERKBANK_MASTRA_DB`, else Mastra Code's macOS default; `None` when the file is missing.
pub fn db_path() -> Option<PathBuf> {
    let path = match std::env::var_os(DB_ENV) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME")?)
            .join("Library/Application Support/mastracode/mastra.db"),
    };
    path.is_file().then_some(path)
}

/// Thread ids are UUIDs or `<ms timestamp>-<base36>`; anything outside lowercase
/// alphanumerics and `-` (and `session-init`) is refused before it reaches SQL.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id != "session-init"
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The `--thread <id>` a resumed Mastra Code was started with.
pub fn thread_from_argv(argv: &[String]) -> Option<String> {
    let at = argv.iter().position(|a| a == "--thread")?;
    argv.get(at + 1).filter(|id| valid_id(id)).cloned()
}

/// `Mastra Code - <title>` → `<title>`.
pub fn title_from_terminal(stripped: &str) -> Option<&str> {
    let title = stripped.strip_prefix("Mastra Code - ")?.trim();
    (!title.is_empty()).then_some(title)
}

/// A single-quoted SQL string literal.
pub fn sql_str(s: &str) -> Result<String> {
    if s.contains('\0') {
        bail!("NUL in SQL text");
    }
    Ok(format!("'{}'", s.replace('\'', "''")))
}

/// `file:<db>?mode=ro`, with the characters that end a URI path escaped.
fn uri(db: &Path) -> String {
    let path = db
        .to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    format!("file:{path}?mode=ro")
}

/// The one argv every query runs with.
fn argv(db: &Path, sql: &str) -> Vec<String> {
    vec![
        "sqlite3".into(),
        "-readonly".into(),
        "-json".into(),
        uri(db),
        sql.into(),
    ]
}

fn query<R: Runner>(runner: &R, argv: &[String]) -> Result<Vec<Value>> {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = runner.run(&refs, None, None)?;
    if !out.success() {
        return Err(anyhow!(
            "sqlite3 exited {}: {}",
            out.status,
            out.stderr.lines().next().unwrap_or("").trim()
        ));
    }
    // `-json` prints nothing at all for an empty result.
    if out.stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&out.stdout)?)
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Resolves each `(title, project dir)` to a thread, in one query: the one
/// thread with that title in that project, else (Mastra Code renames threads,
/// the terminal title can lag) the project's only thread, when no other pane
/// works in the same dir. Zero or several candidates give `None`.
pub fn lookup_by_titles<R: Runner>(
    runner: &R,
    db: &Path,
    panes: &[(String, String)],
) -> Result<Vec<Option<String>>> {
    let Some(argv) = titles_argv(db, panes)? else {
        return Ok(Vec::new());
    };
    let rows = query(runner, &argv)?;
    let rows: Vec<(String, String, PathBuf)> = rows
        .iter()
        .filter_map(|r| {
            Some((
                r.get("id")?.as_str()?.to_string(),
                r.get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                canonical(r.get("project")?.as_str()?),
            ))
        })
        .collect();
    let projects: Vec<PathBuf> = panes.iter().map(|(_, p)| canonical(p)).collect();
    let only = |mut hits: std::vec::IntoIter<&String>| match (hits.next(), hits.next()) {
        (Some(id), None) if valid_id(id) => Some(id.clone()),
        _ => None,
    };
    Ok(panes
        .iter()
        .zip(&projects)
        .map(|((title, _), project)| {
            let in_project: Vec<_> = rows.iter().filter(|(_, _, p)| p == project).collect();
            let titled: Vec<&String> = in_project
                .iter()
                .filter(|(_, t, _)| t == title)
                .map(|(id, _, _)| id)
                .collect();
            if !titled.is_empty() {
                return only(titled.into_iter());
            }
            let shared = projects.iter().filter(|p| *p == project).count() > 1;
            if shared {
                return None;
            }
            only(
                in_project
                    .iter()
                    .map(|(id, _, _)| id)
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        })
        .collect())
}

/// The lookup's argv: threads with one of the titles or in one of the project
/// dirs (as given and canonical); `None` when there is nothing to look up.
pub(crate) fn titles_argv(db: &Path, panes: &[(String, String)]) -> Result<Option<Vec<String>>> {
    if panes.is_empty() {
        return Ok(None);
    }
    let list = |mut v: Vec<String>| -> Result<String> {
        v.sort_unstable();
        v.dedup();
        Ok(v.iter()
            .map(|t| sql_str(t))
            .collect::<Result<Vec<_>>>()?
            .join(","))
    };
    let titles = list(panes.iter().map(|(t, _)| t.clone()).collect())?;
    let dirs = list(
        panes
            .iter()
            .flat_map(|(_, p)| [p.clone(), canonical(p).to_string_lossy().into_owned()])
            .collect(),
    )?;
    let sql = format!(
        "select id, title, json_extract(json(metadata),'$.projectPath') project \
         from mastra_threads where title in ({titles}) \
         or json_extract(json(metadata),'$.projectPath') in ({dirs})"
    );
    Ok(Some(argv(db, &sql)))
}

/// One thread's memory as the recap query returns it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRow {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// The last 2,000 characters of the observations.
    #[serde(default)]
    pub obs: Option<String>,
    #[serde(default)]
    pub last_observed_at: Option<String>,
    /// The last 2,000 characters of the newest buffered observation chunk:
    /// Mastra Code observes in the background into chunks and only folds them
    /// into the active observations once enough message tokens piled up, so
    /// the newest observations usually live here.
    #[serde(default)]
    pub buf: Option<String>,
    #[serde(default)]
    pub buf_observed_at: Option<String>,
    /// The task list, as JSON text.
    #[serde(default)]
    pub tasks: Option<String>,
    /// The first 600 characters of the goal's markdown objective.
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub goal_status: Option<String>,
    #[serde(default)]
    pub goal_runs: Option<u32>,
    #[serde(default)]
    pub goal_max: Option<u32>,
    #[serde(default)]
    pub goal_started: Option<u64>,
    /// The first 400 characters of the newest assistant message's last text
    /// or reasoning part.
    #[serde(default)]
    pub now: Option<String>,
    #[serde(default)]
    pub now_at: Option<String>,
    /// The first 600 characters of the newest `[Goal attempt …]` signal.
    #[serde(default)]
    pub judge: Option<String>,
    #[serde(default)]
    pub judge_at: Option<String>,
}

/// Every valid id's memory row, in one query.
pub fn fetch_recaps<R: Runner>(
    runner: &R,
    db: &Path,
    ids: &[String],
) -> Result<HashMap<String, MemoryRow>> {
    let Some(argv) = recaps_argv(db, ids)? else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    for row in query(runner, &argv)? {
        let row: MemoryRow = serde_json::from_value(row)?;
        out.insert(row.id.clone(), row);
    }
    Ok(out)
}

/// The recap query's argv over the valid ids; `None` when none is valid.
pub(crate) fn recaps_argv(db: &Path, ids: &[String]) -> Result<Option<Vec<String>>> {
    let mut ids: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| valid_id(id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(None);
    }
    let list = ids
        .iter()
        .map(|id| sql_str(id))
        .collect::<Result<Vec<_>>>()?
        .join(",");
    let sql = format!(
        "select t.id, t.title, t.updatedAt, \
         json_extract(json(t.metadata),'$.projectPath') project, \
         substr(o.activeObservations,-2000) obs, o.lastObservedAt, \
         case when json_valid(o.bufferedObservationChunks) then \
         substr(json_extract(o.bufferedObservationChunks,'$[#-1].observations'),-2000) end buf, \
         case when json_valid(o.bufferedObservationChunks) then \
         json_extract(o.bufferedObservationChunks,'$[#-1].lastObservedAt') end bufObservedAt, \
         (select value from mastra_thread_state s where s.threadId=t.id and s.type='task') tasks, \
         substr((select json_extract(value,'$.objective') from mastra_thread_state s \
         where s.threadId=t.id and s.type='goal'),1,600) goal, \
         g.status goalStatus, g.runs goalRuns, g.max goalMax, g.started goalStarted, \
         (select (select substr(coalesce(json_extract(p.value,'$.text'), \
         json_extract(p.value,'$.reasoning')),1,400) \
         from json_each(case when json_valid(m.content) then m.content else '{{}}' end,'$.parts') p \
         where json_extract(p.value,'$.type') in ('text','reasoning') \
         and trim(coalesce(json_extract(p.value,'$.text'),json_extract(p.value,'$.reasoning'),''))<>'' \
         order by p.key desc limit 1) \
         from mastra_messages m where m.thread_id=t.id and m.role='assistant' \
         order by m.createdAt desc limit 1) now, \
         (select m.createdAt from mastra_messages m where m.thread_id=t.id \
         and m.role='assistant' order by m.createdAt desc limit 1) nowAt, \
         (select substr(case when json_valid(m.content) then json_extract(m.content,'$.parts[0].text') end,1,600) \
         from mastra_messages m where m.thread_id=t.id and m.role='signal' \
         and m.content like '%[Goal attempt %' order by m.createdAt desc limit 1) judge, \
         (select m.createdAt from mastra_messages m where m.thread_id=t.id and m.role='signal' \
         and m.content like '%[Goal attempt %' order by m.createdAt desc limit 1) judgeAt \
         from mastra_threads t \
         left join (select threadId, json_extract(value,'$.status') status, \
         json_extract(value,'$.runsUsed') runs, json_extract(value,'$.maxRuns') max, \
         json_extract(value,'$.startedAt') started from mastra_thread_state \
         where type='goal' and json_valid(value)) g on g.threadId=t.id left join mastra_observational_memory o \
         on o.lookupKey='thread:'||t.id and o.supersededBy is null \
         where t.id in ({list})"
    );
    Ok(Some(argv(db, &sql)))
}

/// What the Inbox shows about a thread, derived without any LLM call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recap {
    /// Up to two `• …` (important) / `✓ …` (done) lines from the latest
    /// observation group, newest last.
    #[serde(default)]
    pub lines: Vec<String>,
    /// The observation window held no group start, so the lines may lack context.
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub tasks_done: u32,
    #[serde(default)]
    pub tasks_total: u32,
    /// The in-progress task's `activeForm`.
    #[serde(default)]
    pub active_task: Option<String>,
    /// The goal's first heading (or first line), ≤ 80 chars.
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub observed_unix_ms: Option<u64>,
    /// The thread moved on more than 5 minutes after its last observation.
    #[serde(default)]
    pub stale: bool,
    /// The first sentence of the agent's newest message: what it is doing now.
    #[serde(default)]
    pub now: Option<String>,
    #[serde(default)]
    pub now_unix_ms: Option<u64>,
    /// `(runs used, max runs)` of an active goal.
    #[serde(default)]
    pub goal_runs: Option<(u32, u32)>,
    /// The first sentence of the goal judge's latest feedback, for this goal.
    #[serde(default)]
    pub judge: Option<String>,
}

#[derive(Deserialize)]
struct Task {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "activeForm")]
    active_form: String,
}

impl Recap {
    pub fn from_row(row: &MemoryRow) -> Self {
        // Active observations first, the newest buffered chunk after them, so
        // the newest section with marker lines wins.
        let obs = match (row.obs.as_deref(), row.buf.as_deref()) {
            (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
            (a, b) => a.or(b).map(str::to_string),
        };
        let (lines, partial) = obs.as_deref().map(recap_lines).unwrap_or_default();
        let tasks: Vec<Task> = row
            .tasks
            .as_deref()
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default();
        let active_task = tasks
            .iter()
            .find(|t| t.status == "in_progress")
            .map(|t| strip_wide(t.active_form.trim()))
            .filter(|t| !t.is_empty());
        let observed = [&row.last_observed_at, &row.buf_observed_at]
            .into_iter()
            .filter_map(|t| t.as_deref().and_then(iso_unix_ms))
            .max();
        let updated = row.updated_at.as_deref().and_then(iso_unix_ms);
        let active = row.goal_status.as_deref() == Some("active");
        // Only feedback on this goal: a signal from before it started judged another one.
        let judge = row
            .judge
            .as_deref()
            .filter(|_| active)
            .filter(|_| {
                match (
                    row.judge_at.as_deref().and_then(iso_unix_ms),
                    row.goal_started,
                ) {
                    (Some(at), Some(started)) => at >= started,
                    _ => false,
                }
            })
            .and_then(|t| t.split_once("Judge feedback:").map(|(_, f)| f));
        Recap {
            lines,
            partial,
            tasks_done: tasks.iter().filter(|t| t.status == "completed").count() as u32,
            tasks_total: tasks.len() as u32,
            active_task,
            // A finished goal is history; older rows have no status.
            goal: row
                .goal
                .as_deref()
                .filter(|_| row.goal_status.is_none() || active)
                .and_then(goal_title),
            observed_unix_ms: observed,
            stale: matches!((observed, updated), (Some(o), Some(u)) if u > o + STALE_AFTER_MS),
            now: row
                .now
                .as_deref()
                .and_then(|t| first_sentence(t, &[". ", "! ", "? "])),
            now_unix_ms: row.now_at.as_deref().and_then(iso_unix_ms),
            goal_runs: active.then_some(row.goal_runs.zip(row.goal_max)).flatten(),
            judge: judge.and_then(|t| first_sentence(t, &[". ", ": ", "; "])),
        }
    }
}

/// `text` on one line without markdown emphasis, cut after the first sentence
/// end; `None` when nothing is left.
fn first_sentence(text: &str, ends: &[&str]) -> Option<String> {
    let flat = strip_wide(&text.replace(['`', '*'], ""))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cut = ends
        .iter()
        .filter_map(|e| flat.find(e).map(|i| i + e.len() - 1))
        .min()
        .unwrap_or(flat.len());
    let s = flat[..cut].trim_end_matches([':', ';', ' ']).to_string();
    (!s.is_empty()).then_some(s)
}

const SEPARATORS: [&str; 2] = ["<observation-group", "--- message boundary"];

/// The marker lines of the newest observation section that has any.
fn recap_lines(obs: &str) -> (Vec<String>, bool) {
    let mut starts: Vec<usize> = SEPARATORS
        .iter()
        .flat_map(|s| obs.match_indices(s).map(|(i, _)| i))
        .collect();
    starts.sort_unstable();
    // Section 0 is whatever precedes the first separator; its first line may be
    // cut by the 2,000-char window, so it is dropped.
    let mut bounds = vec![0];
    bounds.extend(&starts);
    bounds.push(obs.len());
    for i in (0..bounds.len() - 1).rev() {
        let section = &obs[bounds[i]..bounds[i + 1]];
        // A separator's own line, or the possibly cut first line, is not content.
        let body = section.split_once('\n').map_or("", |(_, rest)| rest);
        let lines: Vec<String> = body.lines().filter_map(marker_line).collect();
        if !lines.is_empty() {
            let keep = lines.len().saturating_sub(RECAP_LINES);
            return (lines[keep..].to_vec(), i == 0);
        }
    }
    (Vec::new(), false)
}

/// `* 🔴 (10:30) text` → `• (10:30) text`, `* ✅ …` → `✓ …`; other lines are dropped.
fn marker_line(line: &str) -> Option<String> {
    let line = line.trim_start();
    let line = line.strip_prefix("* ").unwrap_or(line);
    let (glyph, rest) = if let Some(rest) = line.strip_prefix('🔴') {
        ("•", rest)
    } else if let Some(rest) = line.strip_prefix('✅') {
        ("✓", rest)
    } else {
        return None;
    };
    let text = strip_wide(rest.trim_start_matches('\u{FE0F}').trim());
    (!text.is_empty()).then(|| format!("{glyph} {text}"))
}

fn goal_title(objective: &str) -> Option<String> {
    let line = objective
        .lines()
        .find_map(|l| l.trim().strip_prefix("# "))
        .or_else(|| objective.lines().map(str::trim).find(|l| !l.is_empty()))?;
    let title = strip_wide(line.trim());
    if title.is_empty() {
        return None;
    }
    if title.chars().count() <= GOAL_TITLE_CHARS {
        return Some(title);
    }
    let cut: String = title.chars().take(GOAL_TITLE_CHARS - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// Drops every character that does not render exactly one cell wide (emoji, CJK,
/// variation selectors, controls), so recap text cannot break the Inbox's columns.
pub fn strip_wide(s: &str) -> String {
    let mut buf = [0u8; 4];
    let out: String = s
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|&c| c != '\u{FE0F}' && Span::raw(&*c.encode_utf8(&mut buf)).width() == 1)
        .collect();
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `2026-11-11T10:30:10.045Z` → unix ms (UTC only, as Mastra Code stores it).
pub fn iso_unix_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    let frac = frac.get(..frac.len().min(3))?;
    let ms: i64 = format!("{frac:0<3}").parse().ok()?;
    let time_ok = (0..24).contains(&hh) && (0..60).contains(&mm) && (0..=60).contains(&ss);
    if !time_ok || !(1..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = ((days * 24 + hh) * 60 + mm) * 60 + ss;
    u64::try_from(total * 1000 + ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runner::{FakeRunner, Output};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/mastracode/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn rows() -> Vec<MemoryRow> {
        serde_json::from_str(&fixture("recap-rows.json")).unwrap()
    }

    fn argv_of(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    const DB: &str = "/db/mastra.db";

    #[test]
    fn thread_ids_come_only_from_a_valid_thread_argument() {
        let resumed = argv_of(&[
            "node",
            "mc",
            "--thread",
            "11111111-2222-4333-8444-555555555555",
        ]);
        assert_eq!(
            thread_from_argv(&resumed).as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
        let ts = argv_of(&["node", "mc", "--thread", "1794445290640-k3j9x0abz"]);
        assert_eq!(
            thread_from_argv(&ts).as_deref(),
            Some("1794445290640-k3j9x0abz")
        );
        assert_eq!(
            thread_from_argv(&argv_of(&["mc", "--thread", "session-init"])),
            None
        );
        assert_eq!(thread_from_argv(&argv_of(&["mc", "--thread"])), None);
        assert_eq!(thread_from_argv(&argv_of(&["mc", "idle"])), None);
        for bad in [
            "x'; drop", "ABC-1", "a b", "a.b", "a\0b", "-lead", "a/b", "",
        ] {
            assert_eq!(
                thread_from_argv(&argv_of(&["mc", "--thread", bad])),
                None,
                "{bad:?}"
            );
        }
        assert!(valid_id(&"a".repeat(64)));
        assert!(!valid_id(&"a".repeat(65)));
    }

    #[test]
    fn recorded_process_info_yields_the_thread_or_nothing() {
        for (file, want) in [
            (
                "process-info-resumed.json",
                Some("11111111-2222-4333-8444-555555555555"),
            ),
            ("process-info-session-init.json", None),
        ] {
            let v: Value = serde_json::from_str(&fixture(file)).unwrap();
            let procs = v["result"]["process_info"]["foreground_processes"]
                .as_array()
                .unwrap();
            let found = procs.iter().find_map(|p| {
                let argv: Vec<String> = serde_json::from_value(p["argv"].clone()).unwrap();
                thread_from_argv(&argv)
            });
            assert_eq!(found.as_deref(), want, "{file}");
        }
    }

    #[test]
    fn terminal_titles_lose_the_mastra_code_prefix() {
        assert_eq!(
            title_from_terminal("Mastra Code - Dark mode"),
            Some("Dark mode")
        );
        assert_eq!(title_from_terminal("Mastra Code - "), None);
        assert_eq!(title_from_terminal("zsh"), None);
    }

    #[test]
    fn sql_strings_double_quotes_and_refuse_nul() {
        assert_eq!(sql_str("it's").unwrap(), "'it''s'");
        assert_eq!(sql_str("''").unwrap(), "''''''");
        assert!(sql_str("a\0b").is_err());
    }

    fn sql_of(argv: &[String]) -> &str {
        argv.last().unwrap()
    }

    #[test]
    fn every_query_is_read_only_with_one_sql_argument() {
        let r = FakeRunner::new();
        r.otherwise("[]");
        let ids = vec![
            "11111111-2222-4333-8444-555555555555".to_string(),
            "x' or 1=1 --".to_string(),
            "1794445290640-k3j9x0abz".to_string(),
        ];
        fetch_recaps(&r, Path::new("/a b/mastra.db"), &ids).unwrap();
        lookup_by_titles(&r, Path::new(DB), &[("it's".into(), "/p".into())]).unwrap();
        let calls = r.calls();
        assert_eq!(calls.len(), 2);
        for c in &calls {
            assert_eq!(c.argv.len(), 5);
            assert_eq!(c.argv[..3], ["sqlite3", "-readonly", "-json"]);
            assert!(c.argv[3].starts_with("file:") && c.argv[3].ends_with("?mode=ro"));
            assert!(c.stdin.is_none());
            assert!(!sql_of(&c.argv).to_lowercase().contains("pragma"));
        }
        // Messages are read only by the recap query, one newest row at a time.
        assert!(!sql_of(&calls[1].argv).contains("mastra_messages"));
        let recap = sql_of(&calls[0].argv);
        assert_eq!(
            recap.matches("from mastra_messages").count(),
            recap.matches("order by m.createdAt desc limit 1").count()
        );
        assert_eq!(calls[0].argv[3], "file:/a b/mastra.db?mode=ro");
        assert!(sql_of(&calls[0].argv)
            .ends_with("in ('11111111-2222-4333-8444-555555555555','1794445290640-k3j9x0abz')"));
        assert!(sql_of(&calls[1].argv).contains("title in ('it''s')"));
        assert!(sql_of(&calls[1].argv).ends_with("in ('/p')"));
    }

    #[test]
    fn uri_escapes_characters_that_end_the_path() {
        assert_eq!(
            uri(Path::new("/x/a?b#c%d.db")),
            "file:/x/a%3fb%23c%25d.db?mode=ro"
        );
    }

    #[test]
    fn no_ids_or_titles_means_no_query() {
        let r = FakeRunner::new();
        assert!(fetch_recaps(&r, Path::new(DB), &[]).unwrap().is_empty());
        assert!(fetch_recaps(&r, Path::new(DB), &["session-init".into()])
            .unwrap()
            .is_empty());
        assert!(lookup_by_titles(&r, Path::new(DB), &[]).unwrap().is_empty());
        assert!(r.calls().is_empty());
    }

    fn title_runner(file: &str) -> FakeRunner {
        let r = FakeRunner::new();
        r.otherwise(&fixture(file));
        r
    }

    #[test]
    fn a_title_resolves_only_to_exactly_one_thread_in_the_pane_project() {
        let pane = vec![(
            "Dark mode toggle".to_string(),
            "/home/user/code/widgets".to_string(),
        )];
        let one = lookup_by_titles(&title_runner("title-lookup.json"), Path::new(DB), &pane);
        assert_eq!(
            one.unwrap(),
            [Some("66666666-7777-4888-9999-000000000000".to_string())]
        );
        let two = lookup_by_titles(
            &title_runner("title-lookup-ambiguous.json"),
            Path::new(DB),
            &pane,
        );
        assert_eq!(two.unwrap(), [None]);
        let other = vec![(
            "Dark mode toggle".to_string(),
            "/home/user/code/other".to_string(),
        )];
        let zero = lookup_by_titles(&title_runner("title-lookup.json"), Path::new(DB), &other);
        assert_eq!(zero.unwrap(), [None]);
        let r = FakeRunner::new();
        r.otherwise("");
        assert_eq!(lookup_by_titles(&r, Path::new(DB), &pane).unwrap(), [None]);
    }

    #[test]
    fn a_renamed_thread_is_found_as_the_only_one_in_its_dir() {
        let r = FakeRunner::new();
        r.otherwise(
            r#"[{"id":"143680e2-4051-49a5-a7db-51058a084a29","title":"Observability Alerts","project":"/w/docs-alerts"},
                {"id":"66666666-7777-4888-9999-000000000000","title":"x","project":"/w/main"},
                {"id":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee","title":"y","project":"/w/main"}]"#,
        );
        let ask = |t: &str, p: &str| (t.to_string(), p.to_string());
        let got = lookup_by_titles(
            &r,
            Path::new(DB),
            &[ask("docs-alerts", "/w/docs-alerts"), ask("z", "/w/main")],
        )
        .unwrap();
        assert_eq!(
            got,
            [
                Some("143680e2-4051-49a5-a7db-51058a084a29".to_string()),
                None
            ],
            "several threads in a dir stay ambiguous"
        );
        let shared = lookup_by_titles(
            &r,
            Path::new(DB),
            &[ask("a", "/w/docs-alerts"), ask("b", "/w/docs-alerts")],
        )
        .unwrap();
        assert_eq!(
            shared,
            [None, None],
            "two panes in one dir can't both own it"
        );
        let titled = lookup_by_titles(&r, Path::new(DB), &[ask("x", "/w/main")]).unwrap();
        assert_eq!(
            titled,
            [Some("66666666-7777-4888-9999-000000000000".to_string())]
        );
    }

    #[test]
    fn project_paths_compare_canonically() {
        let dir = std::env::temp_dir().join(format!("wb-mc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let _ = std::fs::remove_file(dir.join("link"));
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let r = FakeRunner::new();
        let real = canonical(dir.join("real").to_str().unwrap());
        r.otherwise(&format!(
            r#"[{{"id":"abc-1","title":"T","project":"{}"}}]"#,
            real.display()
        ));
        let got = lookup_by_titles(
            &r,
            Path::new(DB),
            &[("T".into(), dir.join("link").to_string_lossy().into())],
        )
        .unwrap();
        assert_eq!(got, [Some("abc-1".to_string())]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sql_failures_are_errors_without_the_sql() {
        let r = FakeRunner::new();
        r.respond(
            &argv(Path::new(DB), "x")
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            None,
            Output {
                status: 1,
                stdout: String::new(),
                stderr: "Error: no such table\nmore".into(),
            },
        );
        let err = query(&r, &argv(Path::new(DB), "x"))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "sqlite3 exited 1: Error: no such table");
    }

    #[test]
    fn fetch_recaps_parses_the_recorded_rows() {
        let r = FakeRunner::new();
        r.otherwise(&fixture("recap-rows.json"));
        let ids = vec!["11111111-2222-4333-8444-555555555555".to_string()];
        let got = fetch_recaps(&r, Path::new(DB), &ids).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got["66666666-7777-4888-9999-000000000000"].tasks, None);
    }

    #[test]
    fn now_and_the_judge_come_from_the_newest_messages_of_an_active_goal() {
        let mut row = MemoryRow {
            now: Some("I'm **weighing** whether to keep\nthe `MDX` entities. Then more.".into()),
            now_at: Some("2026-11-12T21:08:05.505Z".into()),
            goal: Some("# Document alerts".into()),
            goal_status: Some("active".into()),
            goal_runs: Some(2),
            goal_max: Some(30),
            goal_started: iso_unix_ms("2026-11-12T20:00:00.000Z"),
            judge: Some(
                "[Goal attempt 2/30] The goal is not yet complete. Judge feedback: Phase 1 verified as passing: both pages exist.".into(),
            ),
            judge_at: Some("2026-11-12T20:58:19.554Z".into()),
            ..Default::default()
        };
        let r = Recap::from_row(&row);
        assert_eq!(
            r.now.as_deref(),
            Some("I'm weighing whether to keep the MDX entities.")
        );
        assert_eq!(r.now_unix_ms, iso_unix_ms("2026-11-12T21:08:05.505Z"));
        assert_eq!(r.goal_runs, Some((2, 30)));
        assert_eq!(r.judge.as_deref(), Some("Phase 1 verified as passing"));
        assert_eq!(r.goal.as_deref(), Some("Document alerts"));

        row.judge_at = Some("2026-11-12T19:00:00.000Z".into());
        assert_eq!(
            Recap::from_row(&row).judge,
            None,
            "feedback on an older goal"
        );
        row.goal_status = Some("done".into());
        let done = Recap::from_row(&row);
        assert_eq!((done.goal, done.goal_runs), (None, None));
        assert_eq!(first_sentence("  \n ", &[". "]), None);
    }

    #[test]
    fn recap_maps_markers_from_the_latest_group() {
        let rows = rows();
        let recap = Recap::from_row(&rows[0]);
        assert_eq!(
            recap.lines,
            [
                "✓ (10:14) Fixed heading rendering: a group shows its most common page once. Added a regression test.",
                "• (10:30) User requested deployment (\"deploy\"). Deployment has not yet been run.",
            ]
        );
        assert!(!recap.partial);
        assert_eq!((recap.tasks_done, recap.tasks_total), (1, 3));
        assert_eq!(
            recap.active_task.as_deref(),
            Some("Rendering grouped headings")
        );
        assert_eq!(
            recap.goal.as_deref(),
            Some("Widget report outcomes and connected knowledge")
        );
        assert!(recap.stale, "updated 10 min after the last observation");
        assert_eq!(
            recap.observed_unix_ms,
            iso_unix_ms("2026-11-11T10:30:10.045Z")
        );

        let second = Recap::from_row(&rows[1]);
        assert!(!second.stale);
        assert!(second.lines.len() <= RECAP_LINES && !second.lines.is_empty());
        assert_eq!((second.tasks_done, second.tasks_total), (0, 0));
        assert_eq!(second.goal, None);
        for line in recap.lines.iter().chain(&second.lines) {
            assert_eq!(Span::raw(line.as_str()).width(), line.chars().count());
        }
    }

    #[test]
    fn a_window_without_a_group_start_drops_its_cut_first_line() {
        let row = MemoryRow {
            obs: Some(
                "🔴 (09:00) cut in the midd\n* 🟡 (09:01) noise\n* 🔴 (09:02) kept one\n  * -> detail\n"
                    .into(),
            ),
            ..MemoryRow::default()
        };
        let recap = Recap::from_row(&row);
        assert_eq!(recap.lines, ["• (09:02) kept one"]);
        assert!(recap.partial);
    }

    #[test]
    fn only_the_last_two_marker_lines_are_kept_and_empty_groups_fall_back() {
        let obs = "<observation-group id=\"a\">\n* ✅ one\n* 🔴 two\n* ✅ three\n</observation-group>\n\n--- message boundary (x) ---\n";
        let recap = Recap::from_row(&MemoryRow {
            obs: Some(obs.into()),
            ..MemoryRow::default()
        });
        assert_eq!(recap.lines, ["• two", "✓ three"]);
        assert!(!recap.partial);
        assert_eq!(Recap::from_row(&MemoryRow::default()), Recap::default());
    }

    #[test]
    fn the_newest_buffered_chunk_wins_over_older_active_observations() {
        let active = "<observation-group id=\"a\">\n* ✅ (00:54) old work\n</observation-group>\n";
        let chunk =
            "<observation-group id=\"b\">\n* 🔴 (09:12) new question\n</observation-group>\n";
        let row = MemoryRow {
            obs: Some(active.into()),
            last_observed_at: Some("2026-11-12T07:52:03.742Z".into()),
            buf: Some(chunk.into()),
            buf_observed_at: Some("2026-11-12T16:12:40.901Z".into()),
            updated_at: Some("2026-11-12T16:13:00.000Z".into()),
            ..MemoryRow::default()
        };
        let recap = Recap::from_row(&row);
        assert_eq!(recap.lines, ["• (09:12) new question"]);
        assert_eq!(
            recap.observed_unix_ms,
            iso_unix_ms("2026-11-12T16:12:40.901Z")
        );
        assert!(!recap.stale);
        // A chunk with no marker lines falls back to the active observations.
        let quiet = MemoryRow {
            buf: Some("<observation-group id=\"c\">\n* 🟡 noise\n</observation-group>\n".into()),
            ..row
        };
        assert_eq!(Recap::from_row(&quiet).lines, ["✓ (00:54) old work"]);
    }

    #[test]
    fn the_recap_query_reads_the_last_buffered_chunk_safely() {
        let argv = recaps_argv(
            Path::new("/db"),
            &["a3d2e70e-857e-4341-a868-bfe004eb3e6f".into()],
        )
        .unwrap()
        .unwrap();
        let sql = argv.last().unwrap();
        assert!(sql.contains("json_valid(o.bufferedObservationChunks)"));
        assert!(sql.contains("'$[#-1].observations'"));
        assert!(sql.contains("'$[#-1].lastObservedAt'"));
    }

    #[test]
    fn a_long_objective_yields_a_short_goal_title() {
        let goal = rows()[0].goal.clone().unwrap();
        assert!(goal.chars().count() >= 600);
        assert!(goal_title(&goal).unwrap().chars().count() <= GOAL_TITLE_CHARS);
        let long = format!("# {}", "word ".repeat(40));
        let t = goal_title(&long).unwrap();
        assert_eq!(t.chars().count(), GOAL_TITLE_CHARS);
        assert!(t.ends_with('…'));
        assert_eq!(
            goal_title("\n plain first line \nmore").as_deref(),
            Some("plain first line")
        );
    }

    #[test]
    fn malformed_timestamps_are_none_not_a_panic() {
        assert_eq!(iso_unix_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(iso_unix_ms("2026-11-11T10:30:10.0éZ"), None);
        assert_eq!(iso_unix_ms("99999999999999-01-01T00:00:00Z"), None);
        assert_eq!(iso_unix_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(iso_unix_ms("2026-01-01T9999999999999999:00:00Z"), None);
    }

    #[test]
    fn strip_wide_leaves_only_single_width_text() {
        let s = strip_wide("ok 🔴 red ✅ done ❤\u{FE0F} 漢字 tab\there é");
        assert_eq!(Span::raw(s.as_str()).width(), s.chars().count());
        assert_eq!(s, "ok red done ❤ tab here é");
    }

    #[test]
    fn iso_times_convert_to_unix_ms() {
        assert_eq!(iso_unix_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            iso_unix_ms("2026-11-11T10:30:10.045Z"),
            Some(1_794_393_010_045)
        );
        assert_eq!(iso_unix_ms("2000-03-01T00:00:01Z"), Some(951_868_801_000));
        assert_eq!(iso_unix_ms("garbage"), None);
    }
}
