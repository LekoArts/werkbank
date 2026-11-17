//! Install/build commands for a checkout: a per-repo `setup.json` override,
//! else detection from repo files. Detected commands are argv vectors (never a
//! shell); only user-authored override strings run through `/bin/sh -c`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PACKAGE_MANAGERS: [&str; 4] = ["pnpm", "bun", "yarn", "npm"];
const LOCKFILES: [(&str, &str); 5] = [
    ("pnpm-lock.yaml", "pnpm"),
    ("bun.lockb", "bun"),
    ("bun.lock", "bun"),
    ("yarn.lock", "yarn"),
    ("package-lock.json", "npm"),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "cmd", rename_all = "lowercase")]
pub enum Cmd {
    Argv(Vec<String>),
    Shell(String),
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cmd::Argv(argv) => write!(f, "{}", argv.join(" ")),
            Cmd::Shell(s) => write!(f, "{s}"),
        }
    }
}

impl Cmd {
    /// How notifications name the command: detected argv as is; a user-authored
    /// override string only by position, since it may carry inline secrets.
    pub fn label(&self, index: usize) -> String {
        match self {
            Cmd::Argv(argv) => argv.join(" "),
            Cmd::Shell(_) => format!("setup.json command {}", index + 1),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Detected {
    pub commands: Vec<Cmd>,
    /// Shown in the setup log and notification (invalid override, odd package.json).
    pub warnings: Vec<String>,
}

/// `$WERKBANK_SETUP_CONFIG/setup.json` if that is set, else
/// `$HERDR_PLUGIN_CONFIG_DIR/setup.json`.
pub fn override_path(setup_config: &str, plugin_config_dir: &str) -> Option<PathBuf> {
    [setup_config, plugin_config_dir]
        .into_iter()
        .find(|d| !d.is_empty())
        .map(|d| Path::new(d).join("setup.json"))
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

const VALID: &str = "valid keys: repos.<abs repo root>.commands";

/// Strict parse: unknown keys are errors, so a typo is reported, not ignored.
fn parse_override(text: &str) -> Result<BTreeMap<PathBuf, Vec<String>>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let top = v.as_object().ok_or("top level is not an object")?;
    if let Some(k) = top.keys().find(|k| *k != "repos") {
        return Err(format!("unknown key `{k}`"));
    }
    let repos = top
        .get("repos")
        .and_then(Value::as_object)
        .ok_or("`repos` is missing or not an object")?;
    let mut out = BTreeMap::new();
    for (root, entry) in repos {
        let entry = entry
            .as_object()
            .ok_or_else(|| format!("repos[{root}] is not an object"))?;
        if let Some(k) = entry.keys().find(|k| *k != "commands") {
            return Err(format!("unknown key `{k}` in repos[{root}]"));
        }
        let commands = entry
            .get("commands")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("repos[{root}].commands is missing or not an array"))?
            .iter()
            .map(|c| c.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("repos[{root}].commands has a non-string entry"))?;
        out.insert(canon(Path::new(root)), commands);
    }
    Ok(out)
}

/// Commands for `checkout` of the repo at `repo_root`.
pub fn detect(repo_root: &Path, checkout: &Path, override_file: Option<&Path>) -> Detected {
    let mut warnings = Vec::new();
    if let Some(path) = override_file {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warnings.push(format!("setup.json invalid: {e} ({VALID})")),
            Ok(text) => match parse_override(&text) {
                Ok(repos) => {
                    if let Some(cmds) = repos.get(&canon(repo_root)) {
                        return Detected {
                            commands: cmds.iter().cloned().map(Cmd::Shell).collect(),
                            warnings,
                        };
                    }
                }
                Err(e) => warnings.push(format!("setup.json invalid: {e} ({VALID})")),
            },
        }
    }
    let mut detected = detect_files(checkout);
    warnings.append(&mut detected.warnings);
    detected.warnings = warnings;
    detected
}

fn argv(parts: &[&str]) -> Cmd {
    Cmd::Argv(parts.iter().map(|s| s.to_string()).collect())
}

fn detect_files(dir: &Path) -> Detected {
    let mut warnings = Vec::new();
    let pkg = dir.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&pkg) {
        let json: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warnings.push(format!("package.json invalid ({e}); no install detected"));
                return Detected {
                    commands: vec![],
                    warnings,
                };
            }
        };
        let declared = json.get("packageManager").and_then(Value::as_str);
        let named = declared.map(|d| d.split('@').next().unwrap_or(d));
        let pm = match named {
            Some(n) if PACKAGE_MANAGERS.contains(&n) => n,
            other => {
                if let Some(n) = other {
                    warnings.push(format!(
                        "packageManager `{n}` is not one of pnpm, bun, yarn, npm; using lockfiles"
                    ));
                }
                LOCKFILES
                    .iter()
                    .find(|(f, _)| dir.join(f).is_file())
                    .map_or("npm", |(_, pm)| pm)
            }
        };
        let mut commands = vec![argv(&[pm, "install"])];
        if json.pointer("/scripts/build").is_some_and(Value::is_string) {
            commands.push(argv(&[pm, "run", "build"]));
        }
        return Detected { commands, warnings };
    }
    if dir.join("Cargo.toml").is_file() {
        return Detected {
            commands: vec![argv(&["cargo", "build"])],
            warnings,
        };
    }
    Detected::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::setup::copy::tests::tmpdir;
    use std::fs;

    fn dir_with(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = tmpdir(tag);
        for (f, body) in files {
            fs::write(d.join(f), body).unwrap();
        }
        d
    }

    fn cmds(d: &Detected) -> Vec<String> {
        d.commands.iter().map(Cmd::to_string).collect()
    }

    fn files(d: &Path) -> Detected {
        detect(d, d, None)
    }

    #[test]
    fn each_lockfile_and_precedence() {
        for (lock, pm) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("bun.lockb", "bun"),
            ("bun.lock", "bun"),
            ("yarn.lock", "yarn"),
            ("package-lock.json", "npm"),
        ] {
            let d = dir_with("lock", &[("package.json", "{}"), (lock, "")]);
            assert_eq!(cmds(&files(&d)), vec![format!("{pm} install")], "{lock}");
        }
        let d = dir_with(
            "prec",
            &[
                ("package.json", "{}"),
                ("yarn.lock", ""),
                ("pnpm-lock.yaml", ""),
                ("bun.lockb", ""),
            ],
        );
        assert_eq!(cmds(&files(&d)), vec!["pnpm install"]);
        let d = dir_with("none", &[("package.json", "{}")]);
        assert_eq!(cmds(&files(&d)), vec!["npm install"]);
    }

    #[test]
    fn package_manager_field() {
        let d = dir_with(
            "pm",
            &[
                (
                    "package.json",
                    r#"{"packageManager":"pnpm@9.1.0","scripts":{"build":"turbo build"}}"#,
                ),
                ("yarn.lock", ""),
            ],
        );
        let got = files(&d);
        assert_eq!(cmds(&got), vec!["pnpm install", "pnpm run build"]);
        assert!(got.warnings.is_empty());
        let evil = r#"{"packageManager":"evil;rm -rf /@1"}"#;
        let d = dir_with("evil", &[("package.json", evil), ("pnpm-lock.yaml", "")]);
        let got = files(&d);
        assert_eq!(cmds(&got), vec!["pnpm install"]);
        assert!(got.warnings[0].contains("not one of"), "{got:?}");
        let d = dir_with("evil2", &[("package.json", evil)]);
        assert_eq!(cmds(&files(&d)), vec!["npm install"]);
        assert!(files(&d).commands.iter().all(|c| matches!(c, Cmd::Argv(_))));
    }

    #[test]
    fn malformed_package_json_detects_nothing() {
        let d = dir_with("bad", &[("package.json", "{"), ("Cargo.toml", "")]);
        let got = files(&d);
        assert!(got.commands.is_empty());
        assert!(got.warnings[0].contains("package.json invalid"));
    }

    #[test]
    fn build_script_present_absent_non_string() {
        let d = dir_with("b1", &[("package.json", r#"{"scripts":{"build":"tsc"}}"#)]);
        assert_eq!(cmds(&files(&d)), vec!["npm install", "npm run build"]);
        let d = dir_with("b2", &[("package.json", r#"{"scripts":{"test":"x"}}"#)]);
        assert_eq!(cmds(&files(&d)), vec!["npm install"]);
        let d = dir_with("b3", &[("package.json", r#"{"scripts":{"build":1}}"#)]);
        assert_eq!(cmds(&files(&d)), vec!["npm install"]);
    }

    #[test]
    fn cargo_and_nothing() {
        let d = dir_with("cargo", &[("Cargo.toml", "[package]")]);
        assert_eq!(cmds(&files(&d)), vec!["cargo build"]);
        let d = dir_with("empty", &[("README.md", "")]);
        assert_eq!(files(&d), Detected::default());
    }

    fn with_override(tag: &str, body: &str) -> (PathBuf, PathBuf) {
        let repo = dir_with(
            tag,
            &[(
                "package.json",
                r#"{"packageManager":"pnpm@9","scripts":{"build":"x"}}"#,
            )],
        );
        let cfg = tmpdir(&format!("{tag}-cfg"));
        fs::write(
            cfg.join("setup.json"),
            body.replace("ROOT", &repo.to_string_lossy()),
        )
        .unwrap();
        (repo, cfg.join("setup.json"))
    }

    #[test]
    fn override_wins_and_empty_means_none() {
        let (repo, cfg) = with_override(
            "ov",
            r#"{"repos":{"ROOT":{"commands":["make setup","exit 3"]}}}"#,
        );
        let got = detect(&repo, &repo, Some(&cfg));
        assert_eq!(
            got.commands,
            vec![Cmd::Shell("make setup".into()), Cmd::Shell("exit 3".into())]
        );
        let (repo, cfg) = with_override("ov0", r#"{"repos":{"ROOT":{"commands":[]}}}"#);
        assert_eq!(detect(&repo, &repo, Some(&cfg)), Detected::default());
        // Other repos fall through to detection.
        let other = dir_with("ov-other", &[("Cargo.toml", "")]);
        assert_eq!(
            cmds(&detect(&other, &other, Some(&cfg))),
            vec!["cargo build"]
        );
    }

    #[test]
    fn invalid_override_reports_and_falls_back() {
        for body in [
            r#"{"repos":"#,
            r#"{"repo":{}}"#,
            r#"{"repos":{"ROOT":{"commands":["a"],"cmd":1}}}"#,
        ] {
            let (repo, cfg) = with_override("inv", body);
            let got = detect(&repo, &repo, Some(&cfg));
            assert_eq!(cmds(&got), vec!["pnpm install", "pnpm run build"], "{body}");
            assert!(
                got.warnings[0].starts_with("setup.json invalid:"),
                "{got:?}"
            );
            assert!(got.warnings[0].contains("valid keys"), "{got:?}");
        }
        let missing = Path::new("/nonexistent/setup.json");
        let d = dir_with("miss", &[("Cargo.toml", "")]);
        assert!(detect(&d, &d, Some(missing)).warnings.is_empty());
    }

    #[test]
    fn override_keys_are_canonicalized() {
        // macOS: /tmp → /private/tmp. Use a symlinked alias wherever we are.
        let real = dir_with("canon", &[("README.md", "")]);
        let alias = tmpdir("canon-alias").join("link");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let cfg = tmpdir("canon-cfg").join("setup.json");
        let body = format!(
            r#"{{"repos":{{"{}":{{"commands":["echo hi"]}}}}}}"#,
            alias.display()
        );
        fs::write(&cfg, body).unwrap();
        assert_eq!(cmds(&detect(&real, &real, Some(&cfg))), vec!["echo hi"]);
        assert_eq!(cmds(&detect(&alias, &alias, Some(&cfg))), vec!["echo hi"]);
    }

    #[test]
    fn override_path_prefers_the_test_env() {
        assert_eq!(
            override_path("/a", "/b"),
            Some(PathBuf::from("/a/setup.json"))
        );
        assert_eq!(
            override_path("", "/b"),
            Some(PathBuf::from("/b/setup.json"))
        );
        assert_eq!(override_path("", ""), None);
    }
}
