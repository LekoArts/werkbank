use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Every side effect (herdr, git, gh, curl) goes through this trait so logic can be
/// driven by recorded fixtures in tests.
pub trait Runner: Send + Sync {
    fn run(&self, argv: &[&str], stdin: Option<&[u8]>, cwd: Option<&Path>) -> Result<Output>;
}

pub struct RealRunner {
    pub timeout: Duration,
}

impl Default for RealRunner {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl Runner for RealRunner {
    fn run(&self, argv: &[&str], stdin: Option<&[u8]>, cwd: Option<&Path>) -> Result<Output> {
        run_with_timeout(argv, stdin, cwd, self.timeout)
    }
}

/// Runs `argv` and kills it once `timeout` expires, so a hung child can never wedge a caller.
pub fn run_with_timeout(
    argv: &[&str],
    stdin: Option<&[u8]>,
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Output> {
    let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty argv"))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A user shell with CLICOLOR_FORCE makes gh colourise its --json output.
        .env_remove("CLICOLOR_FORCE")
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // Its own process group, so a timeout can kill descendants that hold the pipes.
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;
    let group = child.id() as i32;

    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    // Readers report over channels so collecting output stays under the deadline:
    // a descendant that inherits the pipes can keep them open after the child exits.
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        let _ = out_tx.send(buf);
    });
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        let _ = err_tx.send(buf);
    });

    // Killing the group closes every write end, so the reader threads hit EOF and exit
    // instead of lingering on a pipe a descendant kept open.
    // Group members that escaped with setsid/setpgid are out of reach; their readers
    // are abandoned after this grace period rather than waited on.
    let reap = |rxs: [&mpsc::Receiver<Vec<u8>>; 2]| {
        kill_group(group);
        let grace = Instant::now() + Duration::from_secs(1);
        for rx in rxs {
            let _ = rx.recv_timeout(grace.saturating_duration_since(Instant::now()));
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            // Signal the group while its leader is unreaped, so the id cannot have been reused.
            reap([&out_rx, &err_rx]);
            let _ = child.wait();
            return Err(anyhow!(
                "{} timed out after {}s",
                argv.join(" "),
                timeout.as_secs_f32()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let collect = |rx: &mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    };
    let (Some(stdout), Some(stderr)) = (collect(&out_rx), collect(&err_rx)) else {
        reap([&out_rx, &err_rx]);
        return Err(anyhow!(
            "{} timed out after {}s (output still open after exit)",
            argv.join(" "),
            timeout.as_secs_f32()
        ));
    };
    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

/// Sends `sig` to `pid` (a negative pid targets that process group); signal 0
/// only probes. True when the process exists (zombies included).
pub fn signal(pid: i32, sig: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: kill(2) takes plain integers and has no memory effects.
    unsafe { kill(pid, sig) == 0 }
}

// The group is the child's own, created by `process_group(0)` above.
fn kill_group(group: i32) {
    signal(-group, SIGKILL);
}

#[cfg(test)]
pub use fake::FakeRunner;

#[cfg(test)]
mod fake {
    use super::{Output, Runner};
    use anyhow::{anyhow, Result};
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Call {
        pub argv: Vec<String>,
        pub cwd: Option<PathBuf>,
        pub stdin: Option<Vec<u8>>,
    }

    type Key = (String, Option<PathBuf>);

    /// Canned responses keyed by (argv joined with spaces, cwd). Successive identical calls
    /// pop successive responses; the last one repeats.
    #[derive(Default)]
    pub struct FakeRunner {
        responses: Mutex<HashMap<Key, VecDeque<(Output, Duration)>>>,
        fallback: Mutex<Option<Output>>,
        failures: Mutex<std::collections::HashSet<Key>>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn respond(&self, argv: &[&str], cwd: Option<&str>, output: Output) -> &Self {
            self.respond_after(argv, cwd, output, Duration::ZERO)
        }

        /// Like `respond`, but the call blocks for `delay` before returning.
        pub fn respond_after(
            &self,
            argv: &[&str],
            cwd: Option<&str>,
            output: Output,
            delay: Duration,
        ) -> &Self {
            self.responses
                .lock()
                .unwrap()
                .entry((argv.join(" "), cwd.map(PathBuf::from)))
                .or_default()
                .push_back((output, delay));
            self
        }

        pub fn ok(&self, argv: &[&str], cwd: Option<&str>, stdout: &str) -> &Self {
            self.respond(
                argv,
                cwd,
                Output {
                    status: 0,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                },
            )
        }

        /// Replaces any queued responses for this call with one successful `stdout`.
        pub fn set(&self, argv: &[&str], cwd: Option<&str>, stdout: &str) -> &Self {
            self.responses
                .lock()
                .unwrap()
                .remove(&(argv.join(" "), cwd.map(PathBuf::from)));
            self.ok(argv, cwd, stdout)
        }

        /// Successful `stdout` for any call without a canned response.
        pub fn otherwise(&self, stdout: &str) -> &Self {
            *self.fallback.lock().unwrap() = Some(Output {
                status: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            });
            self
        }

        /// Makes this call return `Err` the way the real runner's timeout does,
        /// with the argv quoted in the message.
        pub fn fail(&self, argv: &[&str], cwd: Option<&str>) -> &Self {
            self.failures
                .lock()
                .unwrap()
                .insert((argv.join(" "), cwd.map(PathBuf::from)));
            self
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        pub fn argvs(&self) -> Vec<String> {
            self.calls().iter().map(|c| c.argv.join(" ")).collect()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, argv: &[&str], stdin: Option<&[u8]>, cwd: Option<&Path>) -> Result<Output> {
            self.calls.lock().unwrap().push(Call {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                cwd: cwd.map(Path::to_path_buf),
                stdin: stdin.map(<[u8]>::to_vec),
            });
            let key = (argv.join(" "), cwd.map(Path::to_path_buf));
            if self.failures.lock().unwrap().contains(&key) {
                return Err(anyhow!("{} timed out after 20s", key.0));
            }
            let (output, delay) = {
                let mut responses = self.responses.lock().unwrap();
                match responses.get_mut(&key) {
                    Some(queue) if queue.len() > 1 => queue.pop_front().unwrap(),
                    Some(queue) => queue.front().cloned().unwrap(),
                    None => self
                        .fallback
                        .lock()
                        .unwrap()
                        .clone()
                        .map(|o| (o, Duration::ZERO))
                        .ok_or_else(|| {
                            anyhow!("no fake response for {:?} in {:?}", key.0, key.1)
                        })?,
                }
            };
            std::thread::sleep(delay);
            Ok(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_runner_returns_queued_outputs_and_records_calls() {
        let fake = FakeRunner::new();
        fake.ok(&["git", "status"], Some("/repo"), "first").ok(
            &["git", "status"],
            Some("/repo"),
            "second",
        );
        let cwd = Path::new("/repo");
        let a = fake
            .run(&["git", "status"], Some(b"in"), Some(cwd))
            .unwrap();
        let b = fake.run(&["git", "status"], None, Some(cwd)).unwrap();
        let c = fake.run(&["git", "status"], None, Some(cwd)).unwrap();
        assert_eq!(a.stdout, "first");
        assert_eq!(b.stdout, "second");
        assert_eq!(c.stdout, "second", "last response repeats");
        let calls = fake.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].stdin.as_deref(), Some(&b"in"[..]));
        assert_eq!(calls[0].cwd.as_deref(), Some(cwd));
        assert!(fake.run(&["git", "status"], None, None).is_err());
    }

    #[test]
    fn real_runner_captures_output_and_stdin() {
        let out = RealRunner::default()
            .run(
                &["sh", "-c", "cat; echo err >&2; exit 3"],
                Some(b"hi"),
                None,
            )
            .unwrap();
        assert_eq!(out.stdout, "hi");
        assert_eq!(out.stderr, "err\n");
        assert_eq!(out.status, 3);
    }

    #[test]
    fn real_runner_strips_forced_colour_env() {
        let out = RealRunner::default()
            .run(
                &[
                    "sh",
                    "-c",
                    "echo ${CLICOLOR_FORCE:-unset} ${NO_COLOR:-unset}",
                ],
                None,
                None,
            )
            .unwrap();
        assert_eq!(out.stdout, "unset 1\n");
    }

    #[test]
    fn real_runner_timeout_covers_pipes_held_by_a_descendant() {
        let start = Instant::now();
        let runner = RealRunner {
            timeout: Duration::from_millis(200),
        };
        let err = runner
            .run(&["sh", "-c", "sleep 5 & exit 0"], None, None)
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn real_runner_timeout_kills_descendants_holding_the_pipes() {
        let dir = std::env::temp_dir().join(format!("tower-runner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, script) in [
            ("exits", "sleep 30 & echo $! > \"$0\"; exit 0"),
            ("hangs", "sleep 30 & echo $! > \"$0\"; sleep 30"),
        ] {
            let pid_file = dir.join(name);
            let start = Instant::now();
            let runner = RealRunner {
                timeout: Duration::from_millis(200),
            };
            let err = runner
                .run(
                    &["sh", "-c", script, pid_file.to_str().unwrap()],
                    None,
                    None,
                )
                .unwrap_err();
            assert!(err.to_string().contains("timed out"), "{name}: {err}");
            assert!(start.elapsed() < Duration::from_secs(4), "{name}");
            let pid = std::fs::read_to_string(&pid_file).unwrap();
            // Gone or a zombie both mean it was killed; `kill -0` can't tell them apart.
            let stat = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", pid.trim()])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&stat.stdout).trim().to_string();
            assert!(
                stat.is_empty() || stat.starts_with('Z'),
                "{name}: grandchild {} still running ({stat})",
                pid.trim()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_runner_kills_on_timeout() {
        let start = Instant::now();
        let runner = RealRunner {
            timeout: Duration::from_millis(200),
        };
        let err = runner.run(&["sleep", "5"], None, None).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
