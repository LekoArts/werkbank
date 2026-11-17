Werkbank is a personal Herdr plugin written in Rust. The daemon watches Mastra Code agents. The Inbox, the tab-bar status and the sidebar tokens all read the daemon's state file.

Commit straight to main with conventional commits (feat, fix, perf, refactor, docs), then push.

Before you commit, run: cargo fmt --check, cargo clippy --all-targets -- -D warnings, cargo test, cargo build --release. The release build also hands the running daemon over to the new binary, so a rebuild is how a change goes live. An open Inbox keeps the old binary until it is reopened.

Unit tests with FakeRunner and the recorded fixtures in tests/fixtures are the proof for almost every change. Keep cargo test under about 2 seconds: no real sleeps or timeouts longer than a few hundred milliseconds. Live Herdr proofs (.mastracode/plans/herdr-tower.proof/demo.sh <scenario>) are slow and block the user, so run one only when a change alters live Herdr behavior, and run only the scenario that covers it. Never run demo.sh all unless asked. In proofs, poll with wait_for rather than fixed sleeps.

To check the live session quickly, read ~/.local/state/herdr/plugins/werkbank/state-default.json or run target/release/werkbank status on it.

Ask before adding a crate. The current ones are anyhow, crossterm, ratatui, serde and serde_json.

Daemon and hook code must target an explicit Herdr session with Herdr::for_session. Never fall back to the default session.

Run every subprocess through core::runner. Keep LINEAR_API_KEY and other secrets out of argv, logs, notifications and fixtures, and strip home paths and usernames from fixtures.

Cleanup never uses --force, -D or reset --hard. When anything looks off, it refuses.

Anything that leaves the machine (Slack) or starts an agent shows the exact text and waits for y first.

Glyphs in the sidebar, tab bar and Inbox must be single-width text symbols such as ? ! ⏸ ✗ ✓ ⇣ ⊘. Emoji render double-width and break the alignment.

The user's Herdr config references the pane IDs, action IDs, subcommands and the $pr, $stalled and $asking token names, so don't rename them.

Edit ~/.config/herdr/config.toml only when the user asks. Back it up as config.toml.bak-werkbank-<timestamp> first, then run herdr config check and herdr server reload-config. Never touch ~/.mastracode/hooks.json.
