//! Herdr's event stream (`events.subscribe` on the session socket). It only
//! wakes the daemon: every tick still reads the full state through the CLI, so
//! a missed or dropped event costs latency, never correctness.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Pane-set and lifecycle events; status changes are subscribed per pane.
const GLOBAL: &[&str] = &[
    "workspace.created",
    "workspace.closed",
    "worktree.created",
    "worktree.removed",
    "tab.created",
    "tab.closed",
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.agent_detected",
];

/// How often a blocked read checks for stop and a new pane set.
const READ_STEP: Duration = Duration::from_millis(250);

/// Shared between the event thread and the daemon loop.
#[derive(Default)]
pub struct Wake {
    generation: AtomicU64,
    connected: AtomicBool,
    panes: Mutex<Vec<String>>,
}

impl Wake {
    /// Bumped on every event; the loop ticks when it moved.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// True while a subscription is live, so the loop may poll slowly.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// The agent panes whose status changes should wake the loop.
    pub fn watch(&self, mut panes: Vec<String>) {
        panes.sort();
        panes.dedup();
        *self.panes.lock().unwrap() = panes;
    }

    fn panes(&self) -> Vec<String> {
        self.panes.lock().unwrap().clone()
    }

    pub fn bump(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub fn live() -> Self {
        let wake = Self::default();
        wake.connected.store(true, Ordering::SeqCst);
        wake
    }

    #[cfg(test)]
    pub fn watching(&self) -> Vec<String> {
        self.panes()
    }
}

pub fn subscribe_request(panes: &[String]) -> String {
    let subs: Vec<Value> = GLOBAL
        .iter()
        .map(|t| json!({ "type": t }))
        .chain(
            panes
                .iter()
                .map(|p| json!({ "type": "pane.agent_status_changed", "pane_id": p })),
        )
        .collect();
    json!({
        "id": "werkbank:events",
        "method": "events.subscribe",
        "params": { "subscriptions": subs },
    })
    .to_string()
}

#[derive(Debug, PartialEq)]
enum Line {
    Started,
    Event,
    Failed(String),
    Other,
}

fn classify(line: &[u8]) -> Line {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return Line::Other;
    };
    if v.get("event").is_some() {
        return Line::Event;
    }
    if let Some(err) = v.get("error") {
        let code = err["code"].as_str().unwrap_or("error");
        return Line::Failed(format!("events.subscribe: {code}"));
    }
    if v["result"]["type"] == "subscription_started" {
        return Line::Started;
    }
    Line::Other
}

/// Keeps one subscription for the current pane set until `stop`, resubscribing
/// when the set changes and retrying `retry` after a failure. `log` hears
/// connects and the first failure after each, so a dead server stays quiet.
pub fn run(
    socket: impl Fn() -> Result<PathBuf>,
    wake: &Wake,
    stop: &AtomicBool,
    retry: Duration,
    log: impl Fn(&str),
) {
    let mut quiet = false;
    while !stop.load(Ordering::SeqCst) {
        let panes = wake.panes();
        if let Err(err) = stream(&socket, &panes, wake, stop, &log) {
            if wake.connected.swap(false, Ordering::SeqCst) || !quiet {
                log(&format!("herdr events: {err:#}; polling until it is back"));
                quiet = true;
            }
            let until = Instant::now() + retry;
            while !stop.load(Ordering::SeqCst) && Instant::now() < until && wake.panes() == panes {
                std::thread::sleep(READ_STEP.min(until - Instant::now()));
            }
        }
    }
    wake.connected.store(false, Ordering::SeqCst);
}

/// One connection. `Ok` when stopped or the pane set changed.
fn stream(
    socket: &impl Fn() -> Result<PathBuf>,
    panes: &[String],
    wake: &Wake,
    stop: &AtomicBool,
    log: &impl Fn(&str),
) -> Result<()> {
    let mut conn = UnixStream::connect(socket()?)?;
    conn.set_read_timeout(Some(READ_STEP))?;
    writeln!(conn, "{}", subscribe_request(panes))?;
    let mut reader = BufReader::new(conn);
    let mut line = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) || wake.panes() != panes {
            return Ok(());
        }
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => bail!("socket closed"),
            Ok(_) if line.ends_with(b"\n") => {
                match classify(&line) {
                    Line::Started => {
                        if !wake.connected.swap(true, Ordering::SeqCst) {
                            log("herdr events: subscribed");
                        }
                        // Catch up on anything between the last tick and now.
                        wake.bump();
                    }
                    Line::Event => wake.bump(),
                    Line::Failed(msg) => bail!(msg),
                    Line::Other => {}
                }
                line.clear();
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    fn wait_for(what: &str, f: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while !f() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_request_subscribes_to_lifecycle_and_each_panes_status() {
        let req: Value = serde_json::from_str(&subscribe_request(&["w1:p2".into()])).unwrap();
        assert_eq!(req["method"], "events.subscribe");
        let subs = req["params"]["subscriptions"].as_array().unwrap();
        assert!(subs.contains(&json!({ "type": "pane.agent_detected" })));
        assert!(subs.contains(&json!({ "type": "pane.closed" })));
        assert_eq!(
            subs.last().unwrap(),
            &json!({ "type": "pane.agent_status_changed", "pane_id": "w1:p2" })
        );
    }

    #[test]
    fn lines_are_classified() {
        assert_eq!(
            classify(br#"{"id":"x","result":{"type":"subscription_started"}}"#),
            Line::Started
        );
        assert_eq!(
            classify(br#"{"data":{"type":"tab_closed"},"event":"tab_closed"}"#),
            Line::Event
        );
        assert_eq!(
            classify(br#"{"id":"x","error":{"code":"pane_not_found","message":"pane w1:p9"}}"#),
            Line::Failed("events.subscribe: pane_not_found".into())
        );
        assert_eq!(classify(b"not json"), Line::Other);
    }

    /// A fake Herdr socket: each connection reports its subscribe line, then
    /// the test writes replies to it.
    struct Server {
        path: PathBuf,
        conns: std::sync::mpsc::Receiver<(String, UnixStream)>,
    }

    impl Server {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("werkbank-ev-{name}-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).unwrap();
            let (tx, conns) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let conn = conn.unwrap();
                    let mut req = String::new();
                    BufReader::new(conn.try_clone().unwrap())
                        .read_line(&mut req)
                        .unwrap();
                    if tx.send((req, conn)).is_err() {
                        return;
                    }
                }
            });
            Self { path, conns }
        }

        fn next(&self) -> (String, UnixStream) {
            self.conns.recv_timeout(Duration::from_secs(2)).unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn spawn(
        server: &Server,
        wake: &Arc<Wake>,
        stop: &Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        let (path, wake, stop) = (server.path.clone(), wake.clone(), stop.clone());
        std::thread::spawn(move || {
            let logs = Mutex::new(Vec::new());
            run(
                || Ok(path.clone()),
                &wake,
                &stop,
                Duration::from_millis(20),
                |m| logs.lock().unwrap().push(m.to_string()),
            );
            logs.into_inner().unwrap()
        })
    }

    #[test]
    fn events_wake_and_a_new_pane_set_resubscribes() {
        let server = Server::new("wake");
        let (wake, stop) = (Arc::new(Wake::default()), Arc::new(AtomicBool::new(false)));
        wake.watch(vec!["w1:p1".into()]);
        let handle = spawn(&server, &wake, &stop);

        let (req, mut conn) = server.next();
        assert!(req.contains(r#""pane_id":"w1:p1""#), "{req}");
        assert!(!wake.connected());
        conn.write_all(
            b"{\"id\":\"werkbank:events\",\"result\":{\"type\":\"subscription_started\"}}\n",
        )
        .unwrap();
        wait_for("connected", || wake.connected());
        let after_start = wake.generation();
        assert_eq!(after_start, 1, "a fresh subscription catches up once");

        // An event split across writes still counts once.
        conn.write_all(b"{\"data\":{},\"event\":\"pane_agent_")
            .unwrap();
        conn.write_all(b"status_changed\"}\n").unwrap();
        wait_for("event", || wake.generation() == 2);

        wake.watch(vec!["w1:p2".into(), "w1:p1".into()]);
        let (req, mut conn) = server.next();
        assert!(req.contains("w1:p1") && req.contains("w1:p2"), "{req}");
        conn.write_all(
            b"{\"id\":\"werkbank:events\",\"result\":{\"type\":\"subscription_started\"}}\n",
        )
        .unwrap();
        wait_for("resubscribed", || wake.generation() == 3);

        stop.store(true, Ordering::SeqCst);
        assert_eq!(handle.join().unwrap(), ["herdr events: subscribed"]);
        assert!(!wake.connected());
    }

    #[test]
    fn a_failure_falls_back_to_polling_and_reconnects() {
        let server = Server::new("retry");
        let (wake, stop) = (Arc::new(Wake::default()), Arc::new(AtomicBool::new(false)));
        wake.watch(vec!["w1:gone".into()]);
        let handle = spawn(&server, &wake, &stop);

        let (_, mut conn) = server.next();
        conn.write_all(b"{\"id\":\"werkbank:events\",\"error\":{\"code\":\"pane_not_found\",\"message\":\"x\"}}\n")
            .unwrap();
        // It retries; the second try succeeds, then the server goes away.
        let (_, mut conn) = server.next();
        conn.write_all(
            b"{\"id\":\"werkbank:events\",\"result\":{\"type\":\"subscription_started\"}}\n",
        )
        .unwrap();
        wait_for("connected", || wake.connected());
        drop(conn);
        wait_for("disconnected", || !wake.connected());
        let (_, _conn) = server.next();

        stop.store(true, Ordering::SeqCst);
        let logs = handle.join().unwrap();
        assert_eq!(
            logs,
            [
                "herdr events: events.subscribe: pane_not_found; polling until it is back",
                "herdr events: subscribed",
                "herdr events: socket closed; polling until it is back",
            ]
        );
    }
}
