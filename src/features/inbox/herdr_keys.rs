//! The Herdr keys bound to Werkbank's actions, read from Herdr's config.toml so
//! the Inbox help shows the user's own bindings.

use std::sync::OnceLock;

use super::theme::config_path;

/// `(key, what the action does)` in config order.
pub type Bindings = Vec<(String, String)>;

/// Read once per Inbox process. Tests always get none.
pub fn herdr_keys() -> &'static Bindings {
    static KEYS: OnceLock<Bindings> = OnceLock::new();
    KEYS.get_or_init(|| {
        if cfg!(test) {
            return Vec::new();
        }
        let text = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        from_config(&text)
    })
}

/// Every `[[keys.command]]` that runs a `werkbank.*` plugin action, with
/// `prefix+` spelled out as the configured prefix. Line-based, like the theme.
pub fn from_config(toml: &str) -> Bindings {
    let mut prefix = "ctrl+b".to_string();
    let mut found: Vec<(Option<String>, Option<String>)> = Vec::new();
    let mut table = String::new();
    for line in toml.lines() {
        let line = line.split(" #").next().unwrap_or("").trim();
        if line.starts_with('[') {
            table = line.trim_matches(['[', ']']).trim().to_string();
            if line == "[[keys.command]]" {
                found.push((None, None));
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim().trim_matches('"').to_string());
        match (table.as_str(), key) {
            ("keys", "prefix") => prefix = value,
            ("keys.command", "key") => {
                if let Some(last) = found.last_mut() {
                    last.0 = Some(value);
                }
            }
            ("keys.command", "command") => {
                if let Some(last) = found.last_mut() {
                    last.1 = Some(value);
                }
            }
            _ => {}
        }
    }
    found
        .into_iter()
        .filter_map(|(key, command)| {
            let action = command?.strip_prefix("werkbank.")?.to_string();
            let key = key?.replace("prefix+", &format!("{prefix} "));
            Some((key, describe(&action)))
        })
        .collect()
}

fn describe(action: &str) -> String {
    match action {
        "open-inbox" => "open the Inbox",
        "launch-tab" => "new tab with Mastra Code",
        "launch-worktree" => "new worktree with Mastra Code",
        "setup-worktree" => "set up this worktree",
        other => other,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn werkbank_actions_are_listed_with_the_prefix_spelled_out() {
        let toml = r#"
[keys]
prefix = "ctrl+a" # mine
[[keys.command]]
key = "prefix+i"
type = "plugin_action"
command = "werkbank.open-inbox"

[[keys.command]]
key = "prefix+g"
type = "plugin_action"
command = "other.thing"

[ui]
key = "nope"

[[keys.command]]
command = "werkbank.launch-worktree"
key = "alt+y"
"#;
        assert_eq!(
            from_config(toml),
            vec![
                ("ctrl+a i".to_string(), "open the Inbox".to_string()),
                (
                    "alt+y".to_string(),
                    "new worktree with Mastra Code".to_string()
                ),
            ]
        );
    }

    #[test]
    fn the_default_prefix_is_ctrl_b_and_unknown_actions_show_their_id() {
        let toml = "[[keys.command]]\nkey = \"prefix+z\"\ncommand = \"werkbank.new-thing\"\n";
        assert_eq!(
            from_config(toml),
            vec![("ctrl+b z".to_string(), "new-thing".to_string())]
        );
        assert!(from_config("").is_empty());
    }
}
