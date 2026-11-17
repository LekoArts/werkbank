mod core;
mod features;
mod integrations;

use std::process::ExitCode;

use crate::core::env;
use crate::core::herdr::Herdr;
use crate::core::runner::RealRunner;
use crate::features::{daemon, inbox, launch::presets, setup};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_default();
    // Every entry point but the daemon's own revives a missing daemon.
    if matches!(
        cmd.as_str(),
        "open-inbox" | "launch-tab" | "launch-worktree" | "worktree-created" | "setup-worktree"
    ) {
        daemon::revive();
    }
    let result = match cmd.as_str() {
        "daemon" => daemon::run(&args[1..]),
        "daemon-start" => daemon_start(&args[1..]),
        "inbox" => inbox::run(&Herdr::new(RealRunner::default())),
        "status" => inbox::status::run(&args[1..]),
        "open-inbox" => inbox::open(&Herdr::new(RealRunner::default())),
        "launch-tab" => {
            let herdr = Herdr::new(RealRunner::default());
            presets::Context::from_env()
                .and_then(|ctx| presets::launch_tab(&herdr, &ctx, &env("HERDR_WORKSPACE_ID")))
                .map(|_| ())
        }
        "launch-worktree" => presets::launch_worktree(&Herdr::new(RealRunner::default())),
        "worktree-prompt" => presets::run_prompt(&Herdr::new(RealRunner::default())),
        "worktree-created" => setup::run_hook(),
        "setup-worktree" => setup::run_action(),
        "setup-run" => setup::run_setup(&args[1..]),
        _ => {
            eprintln!(
                "usage: werkbank <inbox|open-inbox|status|launch-tab|launch-worktree|worktree-prompt|daemon|daemon-start|worktree-created|setup-worktree|setup-run>"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("werkbank {cmd}: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `daemon-start [--session <s>]`: spawns a daemon unless one runs, then returns.
fn daemon_start(args: &[String]) -> anyhow::Result<()> {
    let session = match args {
        [] => None,
        [flag, s] if flag == "--session" => Some(s.as_str()),
        _ => anyhow::bail!("usage: daemon-start [--session <session>]"),
    };
    daemon::spawn_daemon_if_free(session).map(|_| ())
}

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_declares_every_pane_and_action_on_the_release_binary() {
        let toml = include_str!("../herdr-plugin.toml");
        assert!(toml.contains("id = \"werkbank\""));
        for (table, id) in [
            ("panes", "inbox"),
            ("panes", "worktree-prompt"),
            ("actions", "open-inbox"),
            ("actions", "launch-tab"),
            ("actions", "launch-worktree"),
            ("actions", "setup-worktree"),
        ] {
            let entry = format!("[[{table}]]\nid = \"{id}\"");
            assert!(toml.contains(&entry), "missing {entry}");
            let section = toml.split(&entry).nth(1).unwrap();
            let section = section.split("[[").next().unwrap();
            let cmd = section
                .lines()
                .find(|l| l.starts_with("command = "))
                .unwrap_or_else(|| panic!("{id} has no command"));
            let args: Vec<String> =
                serde_json::from_str(cmd.trim_start_matches("command = ")).unwrap();
            assert_eq!(args[0], "target/release/werkbank", "{id}");
            assert_eq!(args[1], id, "subcommand matches id");
        }
        let startup = toml.split("[[startup]]").nth(1).expect("a startup hook");
        let startup = startup.split("[[").next().unwrap();
        assert!(
            startup.contains(r#"command = ["target/release/werkbank", "daemon-start"]"#),
            "{startup}"
        );
        let setup = toml.split("id = \"setup-worktree\"").nth(1).unwrap();
        let setup = setup.split("[[").next().unwrap();
        assert!(setup.contains(r#"contexts = ["workspace"]"#), "{setup}");
        let events = toml.split("[[events]]").nth(1).expect("an event hook");
        let events = events.split("[[").next().unwrap();
        assert!(events.contains(r#"on = "worktree.created""#), "{events}");
        assert!(
            events.contains(r#"command = ["target/release/werkbank", "worktree-created"]"#),
            "{events}"
        );
        // Every manifest command is a subcommand main() dispatches.
        let known = [
            "inbox",
            "open-inbox",
            "launch-tab",
            "launch-worktree",
            "worktree-prompt",
            "daemon-start",
            "worktree-created",
            "setup-worktree",
        ];
        for line in toml
            .lines()
            .filter(|l| l.starts_with("command = [\"target/release"))
        {
            let args: Vec<String> =
                serde_json::from_str(line.trim_start_matches("command = ")).unwrap();
            assert!(known.contains(&args[1].as_str()), "{line}");
            assert!(
                include_str!("main.rs").contains(&format!("\"{}\" =>", args[1])),
                "{line}"
            );
        }
        assert!(toml.contains("placement = \"tab\""));
        assert!(toml.contains("placement = \"popup\""));
    }
}
