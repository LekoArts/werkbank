//! Copies git-ignored per-checkout files (env/secret files, local agent/editor
//! settings) from the main checkout into a new worktree. Never overwrites,
//! never follows symlinks, and reports paths only, never contents.
//!
//! Threat model: the source is the user's own checkout and the target a fresh
//! worktree. Components are checked right before each operation and the
//! published path is re-canonicalized; an adversary swapping parent
//! directories mid-copy is out of scope (that would need `openat`/`linkat`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Result};

use crate::core::runner::Runner;

pub const MAX_FILE: u64 = 1024 * 1024;
pub const DIR_CAP_BYTES: u64 = 2 * 1024 * 1024;
pub const DIR_CAP_FILES: usize = 200;
const SKIP_SUFFIXES: [&str; 5] = [".example", ".sample", ".template", ".dist", ".tpl"];
/// Wholly ignored root directories whose files are all local settings.
const WALKED_DIRS: [&str; 2] = [".vscode", ".idea"];
const LS_FILES_BATCH: usize = 256;

#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "arm")))]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "aarch64", target_arch = "arm"))
))]
const O_NOFOLLOW: i32 = 0o400000;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub copied: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
}

impl Report {
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self.copied.iter().map(|p| format!("copied: {p}")).collect();
        out.extend(
            self.skipped
                .iter()
                .map(|(p, r)| format!("skipped: {p} ({r})")),
        );
        out.extend(
            self.failed
                .iter()
                .map(|(p, r)| format!("failed: {p} ({r})")),
        );
        if out.is_empty() {
            out.push("no local files to copy".to_string());
        }
        out
    }

    pub fn summary(&self) -> String {
        format!(
            "{} copied, {} skipped, {} failed",
            self.copied.len(),
            self.skipped.len(),
            self.failed.len()
        )
    }
}

/// Whether a file path (relative to the repo root) names per-checkout config.
pub fn matches(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let env_like = name == ".env"
        || name.starts_with(".env.")
        || name == ".envrc"
        || name == ".dev.vars"
        || name.starts_with(".dev.vars.")
        || name == ".npmrc"
        || name == ".yarnrc.yml"
        || name == "local.properties"
        || name.ends_with(".tfvars")
        || name.ends_with(".tfvars.json");
    let rails = rel == "config/master.key"
        || rel
            .strip_prefix("config/credentials/")
            .is_some_and(|f| !f.contains('/') && f.ends_with(".key"));
    let settings = rel == ".claude/settings.local.json"
        || rel.ends_with("/.claude/settings.local.json")
        || rel == "CLAUDE.local.md"
        || walked_dir(rel).is_some();
    env_like || rails || settings
}

/// `.vscode` / `.idea` when `rel` lies under one of them at the root.
fn walked_dir(rel: &str) -> Option<&'static str> {
    WALKED_DIRS
        .into_iter()
        .find(|d| rel.strip_prefix(d).is_some_and(|r| r.starts_with('/')))
}

fn example(rel: &str) -> bool {
    SKIP_SUFFIXES.iter().any(|s| rel.ends_with(s))
}

/// Rejects anything but plain relative components.
fn relative(rel: &str) -> Result<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("not a plain relative path");
    }
    Ok(p.to_path_buf())
}

fn git<R: Runner>(runner: &R, root: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
    let root = root.to_string_lossy();
    let mut argv = vec!["git", "-C", root.as_ref()];
    argv.extend_from_slice(args);
    let out = runner.run(&argv, stdin, None)?;
    // check-ignore exits 1 when nothing matched; that is an answer, not an error.
    let no_match = args.first() == Some(&"check-ignore") && out.status == 1;
    if !out.success() && !no_match {
        bail!("git {}: {}", args.join(" "), out.stderr.trim());
    }
    Ok(out.stdout)
}

fn nul_list(s: &str) -> impl Iterator<Item = &str> {
    s.split('\0').filter(|e| !e.is_empty())
}

/// What a walk of one of `WALKED_DIRS` has collected so far.
#[derive(Default)]
struct Budget {
    files: usize,
    bytes: u64,
}

impl Budget {
    fn over(&self) -> bool {
        self.files > DIR_CAP_FILES || self.bytes > DIR_CAP_BYTES
    }
}

/// Regular files under `dir` (relative to `root`), without following symlinks.
/// Stops as soon as `budget` is over the cap, so a huge directory costs little.
fn walk(root: &Path, dir: &str, budget: &mut Budget, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(root.join(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        if budget.over() {
            return;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        let rel = format!("{dir}/{}", entry.file_name().to_string_lossy());
        if meta.is_dir() {
            walk(root, &rel, budget, out);
        } else if meta.is_file() {
            budget.files += 1;
            budget.bytes += meta.len();
            out.push(rel);
        }
    }
}

fn cap_skip(report: &mut Report, dir: &str, detail: String) {
    report.skipped.push((
        format!("{dir}/"),
        format!("{detail}: over the {DIR_CAP_FILES}-file / 2 MiB cap"),
    ));
}

/// Candidate files relative to `root`, plus directories skipped as a whole.
pub fn discover<R: Runner>(runner: &R, root: &Path) -> Result<(Vec<String>, Report)> {
    let listing = git(
        runner,
        root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
        None,
    )?;
    let mut direct = BTreeSet::new();
    let mut probed = BTreeSet::new();
    let mut budgets: BTreeMap<&'static str, Budget> = BTreeMap::new();
    for entry in nul_list(&listing) {
        let Some(dir) = entry.strip_suffix('/') else {
            if matches(entry) {
                direct.insert(entry.to_string());
            }
            continue;
        };
        let base = dir.rsplit('/').next().unwrap_or(dir);
        if base == ".claude" {
            probed.insert(format!("{dir}/settings.local.json"));
        } else if let Some(top) = WALKED_DIRS
            .into_iter()
            .find(|d| *d == dir)
            .or_else(|| walked_dir(dir))
        {
            // Root `.vscode/` or `.idea/`, or a wholly ignored subdirectory of one.
            let mut files = Vec::new();
            walk(root, dir, budgets.entry(top).or_default(), &mut files);
            probed.extend(files);
        } else if dir == "config/credentials" {
            if let Ok(entries) = fs::read_dir(root.join(dir)) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.ends_with(".key") {
                        probed.insert(format!("{dir}/{name}"));
                    }
                }
            }
        }
        // Every other wholly ignored directory (node_modules/, .mastracode/, …) is never entered.
    }
    let mut report = Report::default();
    let over: Vec<&str> = budgets
        .iter()
        .filter(|(_, b)| b.over())
        .map(|(d, _)| *d)
        .collect();
    for dir in &over {
        cap_skip(&mut report, dir, "more files or bytes".into());
        direct.retain(|f| walked_dir(f) != Some(*dir));
    }
    // Probed paths came from walking, not from git: keep only ignored, untracked files.
    let probed: Vec<String> = probed
        .into_iter()
        .filter(|p| walked_dir(p).is_none_or(|d| !over.contains(&d)))
        .filter(|p| root.join(p).symlink_metadata().is_ok())
        .collect();
    if !probed.is_empty() {
        let stdin: Vec<u8> = probed
            .iter()
            .flat_map(|p| [p.as_bytes(), b"\0"].concat())
            .collect();
        let ignored = git(
            runner,
            root,
            &["check-ignore", "-z", "--stdin"],
            Some(&stdin),
        )?;
        let ignored: BTreeSet<&str> = nul_list(&ignored).collect();
        // Batched so a long list never hits the argv limit.
        let mut tracked_out = String::new();
        for chunk in probed.chunks(LS_FILES_BATCH) {
            let mut args = vec!["ls-files", "-z", "--"];
            args.extend(chunk.iter().map(String::as_str));
            tracked_out.push_str(&git(runner, root, &args, None)?);
            tracked_out.push('\0');
        }
        let tracked: BTreeSet<&str> = nul_list(&tracked_out).collect();
        direct.extend(
            probed
                .iter()
                .filter(|p| ignored.contains(p.as_str()) && !tracked.contains(p.as_str()))
                .cloned(),
        );
    }
    let mut files: Vec<String> = direct.into_iter().collect();
    for dir in WALKED_DIRS.into_iter().filter(|d| !over.contains(d)) {
        let inside: Vec<&String> = files
            .iter()
            .filter(|f| walked_dir(f) == Some(dir))
            .collect();
        let bytes: u64 = inside
            .iter()
            .filter_map(|f| root.join(f).symlink_metadata().ok())
            .map(|m| m.len())
            .sum();
        if inside.len() > DIR_CAP_FILES || bytes > DIR_CAP_BYTES {
            cap_skip(
                &mut report,
                dir,
                format!("{} files, {bytes} bytes", inside.len()),
            );
            files.retain(|f| walked_dir(f) != Some(dir));
        }
    }
    Ok((files, report))
}

/// Every component strictly between `root` and the leaf must be a real directory.
fn parents_are_dirs(root: &Path, rel: &Path) -> bool {
    let mut at = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        at.push(c);
        match at.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            _ => return false,
        }
    }
    true
}

/// Creates missing target parents one component at a time, refusing symlinks.
fn make_parents(root: &Path, rel: &Path) -> Result<PathBuf> {
    let mut at = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        at.push(c);
        match at.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("target parent is a symlink or not a directory"),
            Err(e) if e.kind() == ErrorKind::NotFound => match fs::create_dir(&at) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if !at.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                        bail!("target parent is a symlink or not a directory");
                    }
                }
                Err(e) => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        }
    }
    Ok(at)
}

enum Outcome {
    Copied,
    Skipped(String),
}

/// Reads the source without following any symlink, checking it is still the
/// regular file of the size seen.
fn read_source(src_root: &Path, rel: &Path) -> Result<Result<(Vec<u8>, u32), String>> {
    if !parents_are_dirs(src_root, rel) {
        return Ok(Err("source parent is a symlink".into()));
    }
    let path = src_root.join(rel);
    let meta = match path.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Err("source gone".into())),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Ok(Err("symlink".into()));
    }
    if !meta.is_file() {
        return Ok(Err("not a regular file".into()));
    }
    if meta.len() > MAX_FILE {
        return Ok(Err("larger than 1 MiB".into()));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(&path)?;
    let fmeta = file.metadata()?;
    if !fmeta.is_file() || fmeta.len() != meta.len() {
        return Ok(Err("changed while copying".into()));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    (&mut file).take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != meta.len() {
        return Ok(Err("changed while copying".into()));
    }
    Ok(Ok((bytes, fmeta.permissions().mode() & 0o777)))
}

fn tmp_name(dir: &Path, rel: &Path) -> PathBuf {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    dir.join(format!(
        ".{name}.werkbank-{}-{nanos}.tmp",
        std::process::id()
    ))
}

fn copy_one(
    src_root: &Path,
    dst_root: &Path,
    rel: &str,
    before_publish: &dyn Fn(&Path),
) -> Result<Outcome> {
    let rel = relative(rel)?;
    let (bytes, mode) = match read_source(src_root, &rel)? {
        Ok(v) => v,
        Err(reason) => return Ok(Outcome::Skipped(reason)),
    };
    let dir = match make_parents(dst_root, &rel) {
        Ok(d) => d,
        Err(e) => return Ok(Outcome::Skipped(format!("{e}"))),
    };
    let final_path = dst_root.join(&rel);
    if final_path.symlink_metadata().is_ok() {
        return Ok(Outcome::Skipped("exists".into()));
    }
    let tmp = tmp_name(&dir, &rel);
    let result = (|| -> Result<Outcome> {
        let mut f: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        before_publish(&final_path);
        // link() refuses any existing entry, dangling symlinks included, so a
        // file created concurrently is never overwritten.
        match fs::hard_link(&tmp, &final_path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Ok(Outcome::Skipped("exists".into()))
            }
            Err(e) => return Err(e.into()),
        }
        let canon = final_path.canonicalize()?;
        if !canon.starts_with(dst_root) {
            let _ = fs::remove_file(&final_path);
            bail!("published path left the worktree");
        }
        Ok(Outcome::Copied)
    })();
    let _ = fs::remove_file(&tmp);
    result
}

/// Copies every discovered file from `repo_root` into `checkout`.
pub fn copy_local_files<R: Runner>(
    runner: &R,
    repo_root: &Path,
    checkout: &Path,
) -> Result<Report> {
    copy_with(runner, repo_root, checkout, &|_| {})
}

fn copy_with<R: Runner>(
    runner: &R,
    repo_root: &Path,
    checkout: &Path,
    before_publish: &dyn Fn(&Path),
) -> Result<Report> {
    let src = repo_root
        .canonicalize()
        .map_err(|e| anyhow!("repo root {}: {e}", repo_root.display()))?;
    let dst = checkout
        .canonicalize()
        .map_err(|e| anyhow!("checkout {}: {e}", checkout.display()))?;
    if src == dst {
        bail!("the checkout is the main checkout itself");
    }
    let (files, mut report) = discover(runner, &src)?;
    for rel in files {
        if example(&rel) {
            report.skipped.push((rel, "example/template".into()));
            continue;
        }
        match copy_one(&src, &dst, &rel, before_publish) {
            Ok(Outcome::Copied) => report.copied.push(rel),
            Ok(Outcome::Skipped(reason)) => report.skipped.push((rel, reason)),
            Err(e) => report.failed.push((rel, format!("{e:#}"))),
        }
    }
    Ok(report)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::runner::RealRunner;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    /// A fresh temp dir per test (no tempfile crate).
    pub(crate) fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("wb-setup-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    pub(crate) fn sh(dir: &Path, script: &str) {
        let ok = Command::new("/bin/sh")
            .args(["-c", script])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap()
            .success();
        assert!(ok, "{script}");
    }

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// A git repo with `.gitignore` and the given tracked files committed.
    pub(crate) fn repo(tag: &str, ignore: &str, tracked: &[(&str, &str)]) -> PathBuf {
        let root = tmpdir(tag).join("repo");
        fs::create_dir_all(&root).unwrap();
        put(&root, ".gitignore", ignore);
        for (rel, body) in tracked {
            put(&root, rel, body);
        }
        sh(
            &root,
            "git init -q -b main && git add -A && git -c user.email=t@t -c user.name=t commit -qm init",
        );
        root
    }

    fn worktree(root: &Path) -> PathBuf {
        let wt = root.parent().unwrap().join("wt");
        fs::create_dir_all(&wt).unwrap();
        wt
    }

    fn copy(root: &Path, wt: &Path) -> Report {
        copy_local_files(&RealRunner::default(), root, wt).unwrap()
    }

    const IGNORE: &str = ".env\n.env.*\n.envrc\n.claude/\n.vscode/settings.json\n.vscode/big/\nnode_modules/\n.mastracode/\n*.big\nconfig/master.key\nconfig/credentials/\n.idea/\n";

    #[test]
    fn patterns() {
        for yes in [
            ".env",
            "apps/web/.env.local",
            ".envrc",
            ".dev.vars",
            ".dev.vars.prod",
            ".npmrc",
            ".yarnrc.yml",
            "android/local.properties",
            "infra/prod.tfvars",
            "infra/a.tfvars.json",
            "config/master.key",
            "config/credentials/production.key",
            ".claude/settings.local.json",
            "pkg/.claude/settings.local.json",
            "CLAUDE.local.md",
            ".vscode/settings.json",
            ".idea/workspace.xml",
        ] {
            assert!(matches(yes), "{yes}");
        }
        for no in [
            "README.md",
            "env",
            "sub/CLAUDE.local.md",
            "pkg/.vscode/settings.json",
            ".claude/settings.json",
            "config/credentials/a/b.key",
            "lib/master.key",
        ] {
            assert!(!matches(no), "{no}");
        }
        assert!(relative("../x").is_err());
        assert!(relative("/etc/passwd").is_err());
        assert!(relative("a/./b").is_ok());
    }

    #[test]
    fn copies_matching_ignored_files_and_skips_the_rest() {
        let root = repo("basic", IGNORE, &[("package.json", "{}")]);
        put(&root, ".env", "A=main-value");
        put(&root, "apps/web/.env.local", "W=1");
        put(&root, ".env.example", "A=");
        put(&root, ".env.big", &"x".repeat(MAX_FILE as usize + 1));
        put(&root, "node_modules/x/.env", "NM=1");
        put(&root, ".mastracode/plans/p.md", "plan");
        put(&root, ".claude/settings.local.json", "{}");
        put(&root, ".claude/other.json", "{}");
        let wt = worktree(&root);
        let report = copy(&root, &wt);
        assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "A=main-value");
        assert!(wt.join("apps/web/.env.local").is_file());
        assert!(wt.join(".claude/settings.local.json").is_file());
        assert!(!wt.join(".claude/other.json").exists());
        assert!(!wt.join(".env.example").exists());
        assert!(!wt.join(".env.big").exists());
        assert!(
            !wt.join("node_modules").exists(),
            "wholly ignored dir never entered"
        );
        assert!(!wt.join(".mastracode").exists());
        assert!(report
            .skipped
            .contains(&(".env.example".into(), "example/template".into())));
        assert!(report
            .skipped
            .contains(&(".env.big".into(), "larger than 1 MiB".into())));
        assert!(report.failed.is_empty(), "{report:?}");
    }

    #[test]
    fn report_never_contains_contents() {
        let root = repo("contents", IGNORE, &[]);
        put(&root, ".env", "SECRET=hunter2-sentinel");
        let wt = worktree(&root);
        put(&wt, ".env.local", "x");
        put(&root, ".env.local", "OTHER=sentinel-two");
        let text = copy(&root, &wt).lines().join("\n");
        assert!(text.contains("copied: .env"), "{text}");
        assert!(text.contains("skipped: .env.local (exists)"), "{text}");
        assert!(
            !text.contains("hunter2") && !text.contains("sentinel"),
            "{text}"
        );
    }

    #[test]
    fn never_overwrites_files_or_dangling_symlinks() {
        let root = repo("overwrite", IGNORE, &[]);
        put(&root, ".env", "A=main");
        put(&root, ".envrc", "R=main");
        let wt = worktree(&root);
        put(&wt, ".env", "A=worktree");
        symlink(wt.join("nowhere"), wt.join(".envrc")).unwrap();
        let report = copy(&root, &wt);
        assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "A=worktree");
        assert!(wt
            .join(".envrc")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            !wt.join("nowhere").exists(),
            "dangling symlink not followed"
        );
        assert_eq!(report.skipped.len(), 2, "{report:?}");
        assert!(report.copied.is_empty());
    }

    #[test]
    fn a_file_created_right_before_publish_wins() {
        let root = repo("race", IGNORE, &[]);
        put(&root, ".env", "A=main");
        let wt = worktree(&root);
        let report = copy_with(&RealRunner::default(), &root, &wt, &|final_path| {
            fs::write(final_path, "A=agent").unwrap();
        })
        .unwrap();
        assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "A=agent");
        assert_eq!(report.skipped, vec![(".env".into(), "exists".into())]);
        let leftovers: Vec<_> = fs::read_dir(&wt)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "no tmp file left: {leftovers:?}");
    }

    #[test]
    fn symlinked_sources_and_parents_are_skipped() {
        let root = repo("symlink", IGNORE, &[]);
        let outside = root.parent().unwrap().join("outside.txt");
        fs::write(&outside, "OUTSIDE-SENTINEL").unwrap();
        symlink(&outside, root.join(".env.link")).unwrap();
        let elsewhere = root.parent().unwrap().join("elsewhere");
        put(&elsewhere, ".env.local", "E=1");
        symlink(&elsewhere, root.join("apps")).unwrap();
        // git reports apps as a symlink entry, not a dir; test the parent check directly too.
        assert!(!parents_are_dirs(&root, Path::new("apps/.env.local")));
        let wt = worktree(&root);
        let report = copy(&root, &wt);
        assert!(!wt.join(".env.link").exists());
        assert!(!wt.join("apps").exists());
        assert!(
            report
                .skipped
                .contains(&(".env.link".into(), "symlink".into())),
            "{report:?}"
        );
    }

    #[test]
    fn symlinked_target_parent_is_skipped() {
        let root = repo("tparent", IGNORE, &[]);
        put(&root, "apps/web/.env.local", "W=1");
        let wt = worktree(&root);
        let elsewhere = root.parent().unwrap().join("target-elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        symlink(&elsewhere, wt.join("apps")).unwrap();
        let report = copy(&root, &wt);
        assert!(
            fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing written through the symlink"
        );
        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert!(report.skipped[0].1.contains("symlink"), "{report:?}");
    }

    #[test]
    fn walks_root_vscode_with_cap_and_skips_tracked_files() {
        let root = repo("vscode", ".vscode/\n.idea/\n", &[]);
        // .vscode wholly ignored, but one file force-tracked.
        put(&root, ".vscode/extensions.json", "[]");
        sh(&root, "git add -f .vscode/extensions.json && git -c user.email=t@t -c user.name=t commit -qm ext");
        put(&root, ".vscode/settings.json", "{}");
        put(&root, ".vscode/sub/launch.json", "{}");
        for i in 0..=DIR_CAP_FILES {
            put(&root, &format!(".idea/f{i}.xml"), "x");
        }
        let wt = worktree(&root);
        let report = copy(&root, &wt);
        assert!(wt.join(".vscode/settings.json").is_file());
        assert!(wt.join(".vscode/sub/launch.json").is_file());
        assert!(
            !wt.join(".vscode/extensions.json").exists(),
            "tracked file not copied"
        );
        assert!(!wt.join(".idea").exists(), "over cap → whole dir skipped");
        assert!(
            report
                .skipped
                .iter()
                .any(|(p, r)| p == ".idea/" && r.contains("cap")),
            "{report:?}"
        );
    }

    #[test]
    fn walk_stops_once_over_the_cap() {
        let root = repo("walkcap", IGNORE, &[]);
        for i in 0..DIR_CAP_FILES * 3 {
            put(&root, &format!(".idea/f{i}.xml"), "x");
        }
        let mut budget = Budget::default();
        let mut out = Vec::new();
        walk(&root, ".idea", &mut budget, &mut out);
        assert_eq!(out.len(), DIR_CAP_FILES + 1);
        assert!(budget.over());
    }

    #[test]
    fn oversized_settings_dir_does_not_block_other_files() {
        let root = repo("bigidea", IGNORE, &[]);
        put(&root, ".env", "A=1");
        for i in 0..DIR_CAP_FILES * 3 {
            put(&root, &format!(".idea/d{}/f{i}.xml", i % 7), "x");
        }
        let wt = worktree(&root);
        let report = copy(&root, &wt);
        assert!(wt.join(".env").is_file(), "{report:?}");
        assert!(!wt.join(".idea").exists());
        assert!(
            report
                .skipped
                .iter()
                .any(|(p, r)| p == ".idea/" && r.contains("cap")),
            "{report:?}"
        );
    }

    #[test]
    fn rails_probes() {
        let root = repo("rails", IGNORE, &[]);
        put(&root, "config/master.key", "m");
        put(&root, "config/credentials/production.key", "p");
        put(&root, "config/credentials/production.yml.enc", "enc");
        let wt = worktree(&root);
        copy(&root, &wt);
        assert!(wt.join("config/master.key").is_file());
        assert!(wt.join("config/credentials/production.key").is_file());
        assert!(!wt.join("config/credentials/production.yml.enc").exists());
    }

    #[test]
    fn a_write_failure_leaves_no_partial_target_or_tmp() {
        let root = repo("rofail", IGNORE, &[]);
        put(&root, "apps/.env", "A=1");
        let wt = worktree(&root);
        fs::create_dir_all(wt.join("apps")).unwrap();
        fs::set_permissions(wt.join("apps"), fs::Permissions::from_mode(0o555)).unwrap();
        let report = copy(&root, &wt);
        fs::set_permissions(wt.join("apps"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(fs::read_dir(wt.join("apps")).unwrap().count(), 0);
    }

    #[test]
    fn copied_files_keep_the_source_mode() {
        let root = repo("mode", IGNORE, &[]);
        put(&root, ".env", "A=1");
        fs::set_permissions(root.join(".env"), fs::Permissions::from_mode(0o640)).unwrap();
        let wt = worktree(&root);
        copy(&root, &wt);
        let mode = wt.join(".env").metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }
}
