use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::integrations::mastracode::strip_wide;

/// Mastra Code's `ask_user` picker footer, from the real capture
/// `tests/fixtures/screens/mc-ask-user.txt`:
/// `▎ ↑↓ to navigate · Enter to select · Esc to skip`.
/// Neither marker appears on the idle (`mc-idle-done.txt`), answered
/// (`mc-answered-recent.txt`) or working (`mc-working-*.txt`) screens.
const SELECT_MARKER: &str = "Enter to select";
const SKIP_MARKER: &str = "Esc to skip";

/// Mastra Code's footer while a turn runs (`• working  7s · esc to interrupt`,
/// also `• thinking …`),
/// `mc-working-*.txt`). Herdr's hook-fed status can stay `idle` through a
/// turn it never heard start (goal-mode continuations), so the screen wins.
const BUSY_MARKER: &str = "esc to interrupt";

pub const ASK_READ_INTERVAL: Duration = Duration::from_secs(10);

/// True when the screen shows a pending `ask_user` question. Callers pass the
/// visible screen only, so an answered question in scrollback never counts.
pub fn is_asking(screen: &str) -> bool {
    screen
        .lines()
        .any(|l| l.contains(SELECT_MARKER) && l.contains(SKIP_MARKER))
}

/// True when the visible screen shows Mastra Code's running-turn footer.
pub fn is_busy(screen: &str) -> bool {
    screen
        .lines()
        .any(|l| l.trim_start().starts_with("• ") && l.contains(BUSY_MARKER))
}

/// The question of a pending `ask_user` picker: the first non-empty `▎ ` line of
/// the block that ends with the picker footer (`▎ Pick a color`).
pub fn question(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let footer = lines
        .iter()
        .rposition(|l| l.contains(SELECT_MARKER) && l.contains(SKIP_MARKER))?;
    let mut start = footer;
    while start > 0 && lines[start - 1].trim_start().starts_with('▎') {
        start -= 1;
    }
    lines[start..footer].iter().find_map(|l| {
        let text = l.trim_start().strip_prefix('▎')?;
        let text = strip_wide(text.trim());
        (!text.is_empty()).then_some(text)
    })
}

/// Per-pane cached ask verdicts, questions and busy footers, re-read at most every 10s.
#[derive(Debug, Default)]
pub struct AskTracker {
    panes: HashMap<String, Seen>,
}

#[derive(Debug)]
struct Seen {
    at: Instant,
    asking: bool,
    question: Option<String>,
    busy: bool,
}

impl AskTracker {
    pub fn read_due(&self, pane: &str, now: Instant) -> bool {
        self.panes
            .get(pane)
            .is_none_or(|seen| now.duration_since(seen.at) >= ASK_READ_INTERVAL)
    }

    /// Records a fresh visible-screen read.
    pub fn observe(&mut self, pane: &str, screen: &str, now: Instant) {
        let asking = is_asking(screen);
        let seen = Seen {
            at: now,
            asking,
            question: if asking { question(screen) } else { None },
            busy: is_busy(screen),
        };
        self.panes.insert(pane.to_string(), seen);
    }

    pub fn asking(&self, pane: &str) -> bool {
        self.panes.get(pane).is_some_and(|seen| seen.asking)
    }

    /// The last read showed a running turn.
    pub fn busy(&self, pane: &str) -> bool {
        self.panes.get(pane).is_some_and(|seen| seen.busy)
    }

    /// The pending question from the last read, kept between re-reads.
    pub fn question(&self, pane: &str) -> Option<String> {
        self.panes.get(pane).and_then(|seen| seen.question.clone())
    }

    pub fn forget(&mut self, pane: &str) {
        self.panes.remove(pane);
    }

    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.panes.retain(|p, _| keep(p));
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
    fn detects_recorded_ask_user_screen() {
        assert!(is_asking(&screen("mc-ask-user.txt")));
    }

    #[test]
    fn idle_working_answered_and_empty_screens_are_not_asking() {
        assert!(!is_asking(&screen("mc-idle-done.txt")));
        assert!(!is_asking(&screen("mc-answered-recent.txt")));
        assert!(!is_asking(&screen("mc-working-a.txt")));
        assert!(!is_asking(&screen("mc-working-b.txt")));
        assert!(!is_asking(""));
    }

    #[test]
    fn running_turn_footer_is_busy() {
        assert!(is_busy(&screen("mc-working-a.txt")));
        assert!(is_busy(&screen("mc-working-b.txt")));
        assert!(!is_busy(&screen("mc-idle-done.txt")));
        assert!(!is_busy(&screen("mc-answered-recent.txt")));
        assert!(!is_busy("a log line that says esc to interrupt"));
        assert!(is_busy("• thinking 1m3s · 116 tok/s · esc to interrupt"));
        let mut t = AskTracker::default();
        let t0 = Instant::now();
        t.observe("p", &screen("mc-working-a.txt"), t0);
        assert!(t.busy("p") && !t.asking("p"));
        t.observe(
            "p",
            &screen("mc-idle-done.txt"),
            t0 + Duration::from_secs(10),
        );
        assert!(!t.busy("p"));
    }

    #[test]
    fn tracker_rereads_every_10s() {
        let mut t = AskTracker::default();
        let t0 = Instant::now();
        assert!(t.read_due("p", t0));
        t.observe("p", &screen("mc-ask-user.txt"), t0);
        assert!(t.asking("p"));
        assert!(!t.read_due("p", t0 + Duration::from_secs(9)));
        assert!(t.read_due("p", t0 + Duration::from_secs(10)));
        t.forget("p");
        assert!(!t.asking("p"));
    }

    #[test]
    fn question_is_the_first_line_of_the_picker_block() {
        assert_eq!(
            question(&screen("mc-ask-user.txt")).as_deref(),
            Some("Pick a color")
        );
        for idle in [
            "mc-idle-done.txt",
            "mc-answered-recent.txt",
            "mc-working-a.txt",
            "mc-working-b.txt",
        ] {
            assert_eq!(question(&screen(idle)), None, "{idle}");
        }
    }

    #[test]
    fn tracker_keeps_the_question_between_reads_and_clears_it_with_asking() {
        let mut t = AskTracker::default();
        let t0 = Instant::now();
        t.observe("p", &screen("mc-ask-user.txt"), t0);
        let t1 = t0 + Duration::from_secs(4);
        assert!(!t.read_due("p", t1));
        assert_eq!(t.question("p").as_deref(), Some("Pick a color"));
        t.observe("p", &screen("mc-idle-done.txt"), t0 + ASK_READ_INTERVAL);
        assert!(!t.asking("p"));
        assert_eq!(t.question("p"), None);
    }
}
