use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::core::{fnv1a, from_unix_ms, to_unix_ms};

/// Minimum gap between screen reads of one working pane.
pub const READ_EVERY: Duration = Duration::from_secs(15);

/// Strips what changes on a Mastra Code screen while nothing really happens: braille
/// spinner glyphs (and logo), the `• working/thinking … tok/s · esc to interrupt`
/// status line, the footer's context counter (`56.5/120k↓ 47%`), `tok/s` rates and
/// elapsed-time counters like `7s` / `1.5m`. Whitespace is collapsed.
pub fn normalize(screen: &str) -> String {
    let lines: Vec<String> = screen
        .lines()
        .filter(|line| !is_status_line(line))
        .map(strip_counters)
        .collect();
    let no_braille: String = lines
        .join("\n")
        .chars()
        .filter(|c| !('\u{2800}'..='\u{28FF}').contains(c))
        .collect();
    let no_status = no_braille
        .replace("esc to interrupt", " ")
        .replace("working", " ");
    let no_durations = strip_durations(&no_status);
    no_durations
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Mastra Code's live status line, e.g. ` • working  21m51s · no output for 6m · 148 tok/s · esc to interrupt`.
fn is_status_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with('•') && line.contains("esc to interrupt")
}

/// Drops `<n> tok/s` rates, and on ` · `-separated footer lines the context counter
/// (`0/120k`, `56.5/120k↓`) with the percentage after it.
fn strip_counters(line: &str) -> String {
    let footer = line.contains(" · ");
    let words: Vec<&str> = line.split(' ').collect();
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if is_number(w) && words.get(i + 1) == Some(&"tok/s") {
            i += 2;
            continue;
        }
        if footer && is_context_counter(w) {
            i += 1;
            if words
                .get(i)
                .is_some_and(|p| is_number(p.trim_end_matches('%')) && p.ends_with('%'))
            {
                i += 1;
            }
            continue;
        }
        out.push(w);
        i += 1;
    }
    out.join(" ")
}

fn is_number(w: &str) -> bool {
    !w.is_empty()
        && w.chars().all(|c| c.is_ascii_digit() || c == '.')
        && w.chars().any(|c| c.is_ascii_digit())
}

fn is_context_counter(w: &str) -> bool {
    let w = w.trim_end_matches('↓');
    let Some((used, total)) = w.split_once('/') else {
        return false;
    };
    let used = used.trim_end_matches('k');
    let total = total.trim_end_matches('k');
    is_number(used) && is_number(total)
}

/// Mastra Code's own quiet timer: `no output for <duration>` on its last status line.
pub fn reported_quiet(screen: &str) -> Option<Duration> {
    const MARKER: &str = "no output for ";
    let line = screen.lines().rev().find(|l| is_status_line(l))?;
    let rest = &line[line.find(MARKER)? + MARKER.len()..];
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
        .collect();
    parse_duration(&token)
}

/// Parses `6m`, `45s`, `1h`, `6m30s`, `1.5m`, `250ms`.
fn parse_duration(token: &str) -> Option<Duration> {
    let chars: Vec<char> = token.chars().collect();
    let mut i = 0;
    let mut total = 0.0f64;
    while i < chars.len() {
        let end = duration_end(&chars, i)?;
        let text: String = chars[i..end].iter().collect();
        let unit_at = text.find(|c: char| c.is_ascii_alphabetic())?;
        let value: f64 = text[..unit_at].parse().ok()?;
        total += value
            * match &text[unit_at..] {
                "ms" => 0.001,
                "s" => 1.0,
                "m" => 60.0,
                _ => 3600.0,
            };
        i = end;
    }
    // Screen text is untrusted: an absurd value must not panic the daemon.
    if i == 0 {
        return None;
    }
    Duration::try_from_secs_f64(total).ok()
}

/// Removes `\b(\d+(\.\d+)?(ms|s|m|h))+\b` tokens, e.g. `7s`, `1.5m`, `6m53s`.
fn strip_durations(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || !word(chars[i - 1]);
        if at_boundary && chars[i].is_ascii_digit() {
            let mut end = i;
            while let Some(next) = duration_end(&chars, end) {
                end = next;
            }
            if end > i && (end == chars.len() || !word(chars[end])) {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn duration_end(chars: &[char], start: usize) -> Option<usize> {
    let digits = |mut i: usize| {
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let mut i = digits(start);
    if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
        i = digits(i + 1);
    }
    match (chars.get(i), chars.get(i + 1)) {
        (Some('m'), Some('s')) => Some(i + 2),
        (Some('s' | 'm' | 'h'), _) => Some(i + 1),
        _ => None,
    }
}

/// Stable across binaries, so a persisted entry still matches after a daemon handover.
fn hash(text: &str) -> u64 {
    fnv1a(text.as_bytes())
}

/// One pane's stall clock as unix ms, for the daemon's state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallRecord {
    pub pane: String,
    pub hash: u64,
    pub since_ms: u64,
    pub last_read_ms: u64,
    /// Mastra Code's own `no output for` value (ms) and when it was read (unix ms).
    pub reported: Option<(u64, u64)>,
}

struct Entry {
    hash: u64,
    since: Instant,
    last_read: Instant,
    /// Mastra Code's own `no output for` value at the last read, and when it was read.
    reported: Option<(Duration, Instant)>,
}

/// Tracks, per working pane, how long it has been quiet: the longer of Mastra Code's
/// own `no output for` timer and how long its normalized screen has stayed the same.
#[derive(Default)]
pub struct StallTracker {
    panes: HashMap<String, Entry>,
}

impl StallTracker {
    /// Whether a working pane should be read again now.
    pub fn read_due(&self, pane: &str, now: Instant) -> bool {
        self.panes
            .get(pane)
            .is_none_or(|e| now.duration_since(e.last_read) >= READ_EVERY)
    }

    /// Records a pane's status and, when it was read this tick, its raw screen.
    /// Any status other than `working` forgets the pane, so the clock restarts.
    pub fn observe(&mut self, pane: &str, status: &str, screen: Option<&str>, now: Instant) {
        if status != "working" {
            self.panes.remove(pane);
            return;
        }
        let Some(screen) = screen else { return };
        let h = hash(&normalize(screen));
        let reported = reported_quiet(screen).map(|quiet| (quiet, now));
        match self.panes.get_mut(pane) {
            Some(e) if e.hash == h => {
                e.last_read = now;
                e.reported = reported;
            }
            _ => {
                self.panes.insert(
                    pane.to_string(),
                    Entry {
                        hash: h,
                        since: now,
                        last_read: now,
                        reported,
                    },
                );
            }
        }
    }

    pub fn stalled(&self, pane: &str, now: Instant, threshold: Duration) -> Option<Duration> {
        let e = self.panes.get(pane)?;
        let screen_quiet = now.duration_since(e.since);
        let reported_quiet = e.reported.map_or(Duration::ZERO, |(quiet, at)| {
            quiet.saturating_add(now.duration_since(at))
        });
        let quiet = screen_quiet.max(reported_quiet);
        (quiet >= threshold).then_some(quiet)
    }

    /// The hash of the pane's last normalized screen, as `fnv1a(normalize(screen))`.
    pub fn screen_hash(&self, pane: &str) -> Option<u64> {
        self.panes.get(pane).map(|e| e.hash)
    }

    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.panes.retain(|pane, _| keep(pane));
    }

    pub fn export(&self, now: Instant, now_ms: u64) -> Vec<StallRecord> {
        let mut records: Vec<StallRecord> = self
            .panes
            .iter()
            .map(|(pane, e)| StallRecord {
                pane: pane.clone(),
                hash: e.hash,
                since_ms: to_unix_ms(e.since, now, now_ms),
                last_read_ms: to_unix_ms(e.last_read, now, now_ms),
                reported: e
                    .reported
                    .map(|(quiet, at)| (quiet.as_millis() as u64, to_unix_ms(at, now, now_ms))),
            })
            .collect();
        records.sort_by(|a, b| a.pane.cmp(&b.pane));
        records
    }

    pub fn seed(&mut self, records: &[StallRecord], now: Instant, now_ms: u64) {
        for r in records {
            self.panes.insert(
                r.pane.clone(),
                Entry {
                    hash: r.hash,
                    since: from_unix_ms(r.since_ms, now, now_ms),
                    last_read: from_unix_ms(r.last_read_ms, now, now_ms),
                    reported: r.reported.map(|(quiet, at)| {
                        (Duration::from_millis(quiet), from_unix_ms(at, now, now_ms))
                    }),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/screens/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn recorded_working_screens_normalize_equal() {
        let (a, b) = (screen("mc-working-a.txt"), screen("mc-working-b.txt"));
        assert_ne!(a, b, "fixtures differ by timer");
        assert_eq!(normalize(&a), normalize(&b));
        assert!(normalize(&a).contains("Sleeping for 40 seconds"));
    }

    #[test]
    fn normalize_strips_timers_spinners_and_status_words() {
        assert_eq!(
            normalize("⠏ Running tests 13s\n • working  15s · esc to interrupt"),
            "Running tests"
        );
        assert_eq!(normalize("took 1.5m and 250ms, 2h"), "took and ,");
        assert_eq!(normalize("took 6m53s, 1h2m"), "took ,");
    }

    #[test]
    fn normalize_keeps_non_duration_numbers() {
        assert_eq!(
            normalize("0/120k 0% v0.49.0 sha1s 40 seconds 3x"),
            "0/120k 0% v0.49.0 sha1s 40 seconds 3x"
        );
    }

    #[test]
    fn real_output_change_survives_normalization() {
        assert_ne!(normalize("step 1 7s"), normalize("step 2 9s"));
    }

    const T: Duration = Duration::from_secs(300);

    #[test]
    fn flags_stall_after_threshold() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        t.observe("p", "working", Some("same"), t0);
        assert_eq!(t.stalled("p", t0 + Duration::from_secs(299), T), None);
        t.observe("p", "working", Some("same"), t0 + Duration::from_secs(200));
        assert_eq!(
            t.stalled("p", t0 + Duration::from_secs(301), T),
            Some(Duration::from_secs(301))
        );
    }

    #[test]
    fn resets_on_screen_change() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        t.observe("p", "working", Some("a"), t0);
        t.observe("p", "working", Some("b"), t0 + Duration::from_secs(250));
        assert_eq!(t.stalled("p", t0 + Duration::from_secs(400), T), None);
        assert!(t.stalled("p", t0 + Duration::from_secs(551), T).is_some());
    }

    #[test]
    fn resets_on_non_working_status() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        t.observe("p", "working", Some("a"), t0);
        t.observe("p", "idle", None, t0 + Duration::from_secs(100));
        t.observe("p", "working", Some("a"), t0 + Duration::from_secs(101));
        assert_eq!(t.stalled("p", t0 + Duration::from_secs(350), T), None);
    }

    #[test]
    fn read_due_every_fifteen_seconds() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        assert!(t.read_due("p", t0));
        t.observe("p", "working", Some("a"), t0);
        assert!(!t.read_due("p", t0 + Duration::from_secs(14)));
        assert!(t.read_due("p", t0 + Duration::from_secs(15)));
    }

    // Status and footer lines as seen on a live Mastra Code pane (2026-11-11), 20s apart.
    const QUIET_A: &str = "│ ⠋ Verifying checks 6m53s │\n • working  21m51s · no output for 6m · 148 tok/s · esc to interrupt\n build · anthropic/claude-opus-5-5 · 56.5/120k↓ 47% · Firegoat status update";
    const QUIET_B: &str = "│ ⠙ Verifying checks 6m54s │\n • working  22m11s · no output for 6m · 133 tok/s · esc to interrupt\n build · anthropic/claude-opus-5-5 · 60/120k↓ 50% · Firegoat status update";

    #[test]
    fn status_line_and_counters_do_not_count_as_output() {
        assert_eq!(normalize(QUIET_A), normalize(QUIET_B));
        assert_eq!(
            normalize(QUIET_A),
            "│ Verifying checks │ build · anthropic/claude-opus-5-5 · · Firegoat status update"
        );
    }

    #[test]
    fn reads_mastra_codes_own_quiet_timer() {
        assert_eq!(reported_quiet(QUIET_A), Some(Duration::from_secs(360)));
        assert_eq!(
            reported_quiet(" • working 9m · no output for 6m30s · esc to interrupt"),
            Some(Duration::from_secs(390))
        );
        assert_eq!(reported_quiet(&screen("mc-working-a.txt")), None);
        // Only the status line counts, not output that happens to say it.
        assert_eq!(reported_quiet("log: no output for 9m"), None);
    }

    #[test]
    fn an_absurd_quiet_timer_does_not_panic() {
        let huge = " • working 9m · no output for 99999999999999999999999h · esc to interrupt";
        assert_eq!(reported_quiet(huge), None);
        let max = " • working 9m · no output for 1000000000000000h · esc to interrupt";
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        t.observe("p", "working", Some(max), t0);
        assert!(t.stalled("p", t0 + Duration::from_secs(5), T).is_some());
    }

    #[test]
    fn mastra_codes_quiet_timer_flags_a_stall_on_the_first_read() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        let five_m = " • working 9m · no output for 5m · esc to interrupt";
        t.observe("p", "working", Some(five_m), t0);
        assert_eq!(t.stalled("p", t0, T), Some(T));
        // Real output: Mastra Code drops the marker and the screen changes.
        t.observe("p", "working", Some("step 2"), t0 + Duration::from_secs(15));
        assert_eq!(t.stalled("p", t0 + Duration::from_secs(16), T), None);
    }

    #[test]
    fn quiet_timer_keeps_running_between_reads() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        t.observe(
            "p",
            "working",
            Some(" • working · no output for 4m · esc to interrupt"),
            t0,
        );
        assert_eq!(t.stalled("p", t0 + Duration::from_secs(59), T), None);
        assert!(t.stalled("p", t0 + Duration::from_secs(60), T).is_some());
    }

    #[test]
    fn exported_entries_seed_a_new_tracker_with_the_same_clock() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        t.observe("p", "working", Some("frozen"), t0);
        // Read again 5s before the export: same screen, so quiet-since stays t0.
        t.observe(
            "p",
            "working",
            Some("frozen"),
            t0 + Duration::from_secs(195),
        );
        let now = t0 + Duration::from_secs(200);
        let records = t.export(now, 10_000_000);
        assert_eq!(records[0].since_ms, 10_000_000 - 200_000);
        assert_eq!(records[0].hash, fnv1a(b"frozen"));

        // A later process: its own monotonic clock, the same wall clock.
        let mut seeded = StallTracker::default();
        let later = Instant::now();
        seeded.seed(&records, later, 10_000_000);
        assert!(!seeded.read_due("p", later), "last read carried over");
        assert_eq!(
            seeded.stalled("p", later + Duration::from_secs(100), T),
            Some(T)
        );
        // The unchanged screen keeps the original quiet-since.
        seeded.observe(
            "p",
            "working",
            Some("frozen"),
            later + Duration::from_secs(15),
        );
        assert_eq!(
            seeded.stalled("p", later + Duration::from_secs(100), T),
            Some(T)
        );
    }

    #[test]
    fn a_stall_known_only_from_the_reported_timer_survives_a_restart() {
        let mut t = StallTracker::default();
        let t0 = Instant::now();
        t.observe(
            "p",
            "working",
            Some(" • working 9m · no output for 6m · esc to interrupt"),
            t0,
        );
        assert!(t.stalled("p", t0, T).is_some(), "stalled only via reported");
        let records = t.export(t0, 5_000_000);
        assert_eq!(records[0].reported, Some((360_000, 5_000_000)));
        let mut seeded = StallTracker::default();
        let later = Instant::now();
        seeded.seed(&records, later, 5_000_000);
        assert_eq!(
            seeded.stalled("p", later, T),
            Some(Duration::from_secs(360))
        );
    }
}
