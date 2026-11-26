//! Messages an agent wrote for Factory, waiting for the user. `werkbank
//! factory draft` saves one; only the Inbox's Factory tab sends it, after the
//! user has read the exact text and pressed `y`.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::prompt::{Factory, SECTION_MAX};
use crate::core::herdr::Herdr;
use crate::core::runner::RealRunner;
use crate::features::daemon::write_atomic;

pub const USAGE: &str = "usage: werkbank factory draft < message.txt";

const HELP: &str = "\
werkbank factory draft < message.txt

Saves the message on stdin as a draft for Mastra Factory. Nothing is posted:
the draft waits in the Werkbank Inbox's Factory tab until the user reads it
and sends it to Slack, where it starts one Factory session.

Rules:
- One task per draft. Several tasks: run the command once per task.
- At most 3000 characters, counted the way Slack does: the bot mention is
  added in front, & < > count as &amp; &lt; &gt;, an emoji counts as two.
  A longer message is refused; shorten it and run again.
- Factory can't see this machine: name repo, branch, files and numbers, and
  link anything it needs. Plain URLs only; <url|text> links are not kept.
- Slack mrkdwn works: *bold*, _italic_, `code`, ``` blocks, - lists.
";

/// A saved draft; `id` is its file name without `.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    #[serde(skip)]
    pub id: String,
    pub created_unix_ms: u64,
    /// The checkout the agent ran in, so the user knows who wrote it.
    pub from: String,
    pub text: String,
}

pub fn dir(config_dir: &Path) -> PathBuf {
    config_dir.join("factory-drafts")
}

/// Checks `text` against the limit as it will be sent, then saves it.
/// Returns the file and the length Slack will count.
pub fn save(
    drafts: &Path,
    factory: &Factory,
    text: &str,
    from: &str,
    now_ms: u64,
) -> Result<(PathBuf, usize)> {
    let text = text.trim();
    if text.is_empty() {
        bail!("the message on stdin is empty");
    }
    let Some(message) = factory.compose_draft(text) else {
        bail!("SLACK_FACTORY_MENTION is not set, so a draft could never be sent; see the Werkbank README");
    };
    if let Some(why) = message.too_long() {
        bail!("{why}. Nothing was saved.");
    }
    crate::core::private_dir(drafts)?;
    let id = format!("{now_ms}-{}", std::process::id());
    let path = drafts.join(format!("{id}.json"));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let draft = Draft {
        id,
        created_unix_ms: now_ms,
        from: from.to_string(),
        text: text.to_string(),
    };
    write_atomic(&path, &serde_json::to_vec_pretty(&draft)?)?;
    Ok((path, message.slack_len()))
}

/// Every readable draft, oldest first; a file that doesn't parse is skipped.
pub fn list(drafts: &Path) -> Vec<Draft> {
    let Ok(entries) = std::fs::read_dir(drafts) else {
        return Vec::new();
    };
    let mut out: Vec<Draft> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json")?;
            if !valid_id(id) {
                return None;
            }
            let mut d: Draft = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            d.id = id.to_string();
            Some(d)
        })
        .collect();
    out.sort_by(|a, b| (a.created_unix_ms, &a.id).cmp(&(b.created_unix_ms, &b.id)));
    out
}

/// Deletes one draft; an id that isn't one of ours is refused.
pub fn remove(drafts: &Path, id: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("not a draft id: {id}");
    }
    let path = drafts.join(format!("{id}.json"));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// `<unix ms>-<pid>`: digits and one dash, so it can't leave the dir.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// `werkbank factory draft`, run by an agent from its own shell.
pub fn run(args: &[String]) -> Result<()> {
    match args {
        [cmd] if cmd == "draft" => {}
        [cmd, help] if cmd == "draft" && (help == "--help" || help == "-h") => {
            print!("{HELP}");
            return Ok(());
        }
        _ => bail!(USAGE),
    }
    // An agent's pane isn't a plugin context: ask Herdr where our config is,
    // so the `.env` and factory.json lookups below find it too.
    if std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").is_none() {
        let dir = Herdr::new(RealRunner::default()).plugin_config_dir("werkbank")?;
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", dir);
    }
    let config_dir = PathBuf::from(crate::core::env("HERDR_PLUGIN_CONFIG_DIR"));
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("reading the message from stdin")?;
    let from = std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let (_, len) = save(
        &dir(&config_dir),
        &Factory::load(),
        &text,
        &from,
        crate::core::unix_ms(),
    )?;
    println!(
        "Draft saved ({len}/{SECTION_MAX} characters). Nothing was posted: it waits in the Werkbank Inbox's Factory tab until the user sends it."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory() -> Factory {
        Factory::from_file(None, Some("U0FACTORY".into()))
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("werkbank-drafts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn saves_lists_oldest_first_and_removes() {
        let d = tmp("roundtrip");
        let (path, len) = save(&d, &factory(), "  second\n", "repo-b", 20).unwrap();
        assert_eq!(len, "<@U0FACTORY> second".len());
        save(&d, &factory(), "first & more", "repo-a", 10).unwrap();
        std::fs::write(d.join("junk.json"), "{}").unwrap();
        std::fs::write(d.join(".1-1.json.9.tmp"), "{}").unwrap();
        let all = list(&d);
        assert_eq!(
            all.iter()
                .map(|x| (x.from.as_str(), x.text.as_str()))
                .collect::<Vec<_>>(),
            [("repo-a", "first & more"), ("repo-b", "second")]
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d), 0o700);
        assert_eq!(mode(&path), 0o600);
        remove(&d, &all[0].id).unwrap();
        assert_eq!(list(&d).len(), 1);
        assert!(remove(&d, "../x").is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_too_long_empty_or_unsendable_and_saves_nothing() {
        let d = tmp("refuse");
        let room = SECTION_MAX - "<@U0FACTORY> ".len();
        let err = save(&d, &factory(), &"a".repeat(room + 1), "r", 1)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("3001") && err.contains("Nothing was saved"),
            "{err}"
        );
        assert!(save(&d, &factory(), " \n", "r", 1).is_err());
        let no_mention = Factory::from_file(None, None);
        let err = save(&d, &no_mention, "hi", "r", 1).unwrap_err().to_string();
        assert!(err.contains("SLACK_FACTORY_MENTION"), "{err}");
        assert!(!d.exists());
        assert!(save(&d, &factory(), &"a".repeat(room), "r", 1).is_ok());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_missing_dir_lists_nothing() {
        assert!(list(&tmp("missing")).is_empty());
    }
}
