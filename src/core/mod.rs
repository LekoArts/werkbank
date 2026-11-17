//! Shared plumbing every feature builds on: process running and the Herdr CLI.

pub mod events;
pub mod herdr;
pub mod runner;
pub mod stop;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Creates `dir` and makes it owner-only: the state dir holds issue text,
/// agent output and logs. Tightening is best-effort, so a filesystem without
/// modes doesn't stop the daemon.
pub fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Opens `path` for appending as an owner-only file, tightening one that
/// already existed.
pub fn open_private_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    Ok(f)
}

pub fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// `name` from the environment, else from `$HERDR_PLUGIN_CONFIG_DIR/.env`.
pub fn env_or_dotenv(name: &str) -> Option<String> {
    value_from(
        name,
        std::env::var(name).ok(),
        std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(Into::into),
    )
}

fn value_from(
    name: &str,
    env: Option<String>,
    config_dir: Option<std::path::PathBuf>,
) -> Option<String> {
    if let Some(v) = env.filter(|v| !v.trim().is_empty()) {
        return Some(v.trim().to_string());
    }
    let text = std::fs::read_to_string(config_dir?.join(".env")).ok()?;
    text.lines().find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let value = line.strip_prefix(name)?.strip_prefix('=')?.trim();
        let value = value.trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// `session` as a file-name part: session names come from the environment,
/// so anything that could step out of the state dir becomes `_`.
pub fn safe_session(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The Herdr session this process runs under. `HERDR_SESSION` when set; Herdr
/// leaves it unset for the unnamed default session, so otherwise the session
/// is the one whose socket is `HERDR_SOCKET_PATH`. Empty when neither resolves.
pub fn herdr_session<R: runner::Runner>(runner: &R) -> String {
    let named = env("HERDR_SESSION");
    if !named.is_empty() {
        return named;
    }
    let socket = env("HERDR_SOCKET_PATH");
    if socket.is_empty() {
        return String::new();
    }
    let bin = Some(env("HERDR_BIN_PATH"))
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "herdr".into());
    runner
        .run(&[&bin, "session", "list", "--json"], None, None)
        .ok()
        .filter(|o| o.status == 0)
        .and_then(|o| session_for_socket(&o.stdout, &socket))
        .unwrap_or_default()
}

/// The name of the session in `herdr session list --json` whose socket is `socket`.
pub fn session_for_socket(list_json: &str, socket: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(list_json).ok()?;
    v["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["socket_path"].as_str() == Some(socket))
        .and_then(|s| s["name"].as_str())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// The socket of `session` in `herdr session list --json`, if it is running.
pub fn socket_for_session(list_json: &str, session: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(list_json).ok()?;
    v["sessions"]
        .as_array()?
        .iter()
        .find(|s| s["name"].as_str() == Some(session) && s["running"] != false)
        .and_then(|s| s["socket_path"].as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// 64-bit FNV-1a: stable across builds and Rust versions, unlike `DefaultHasher`,
/// so values can be persisted and compared by a later binary.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `at` as unix ms, given one `now` observed on both clocks.
pub fn to_unix_ms(at: Instant, now: Instant, now_ms: u64) -> u64 {
    now_ms.saturating_sub(now.saturating_duration_since(at).as_millis() as u64)
}

/// The inverse of `to_unix_ms`; a time before the monotonic clock's origin becomes `now`.
pub fn from_unix_ms(ms: u64, now: Instant, now_ms: u64) -> Instant {
    now.checked_sub(Duration::from_millis(now_ms.saturating_sub(ms)))
        .unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_comes_from_env_then_config_env_file() {
        let dir = std::env::temp_dir().join(format!("tower-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".env"),
            "OTHER=1\nLINEAR_API_KEY_X=wrong\nexport LINEAR_API_KEY=\"from-file\"",
        )
        .unwrap();
        let k = "LINEAR_API_KEY";
        assert_eq!(
            value_from(k, Some("from-env".into()), Some(dir.clone())).as_deref(),
            Some("from-env")
        );
        assert_eq!(
            value_from(k, Some("  ".into()), Some(dir.clone())).as_deref(),
            Some("from-file")
        );
        assert_eq!(value_from(k, None, None), None);
        std::fs::write(dir.join(".env"), "OTHER=1\n").unwrap();
        assert_eq!(value_from(k, None, Some(dir.clone())), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn socket_is_found_by_session() {
        let list = r#"{"sessions":[
            {"default":true,"name":"default","running":true,"socket_path":"/h/herdr.sock"},
            {"default":false,"name":"work","running":false,"socket_path":"/h/sessions/work/herdr.sock"}]}"#;
        assert_eq!(
            socket_for_session(list, "default").as_deref(),
            Some("/h/herdr.sock")
        );
        assert_eq!(socket_for_session(list, "work"), None, "not running");
        assert_eq!(socket_for_session(list, "nope"), None);
    }

    #[test]
    fn session_is_found_by_socket() {
        let list = r#"{"sessions":[
            {"default":true,"name":"default","running":true,"socket_path":"/h/herdr.sock"},
            {"default":false,"name":"work","running":true,"socket_path":"/h/sessions/work/herdr.sock"}]}"#;
        assert_eq!(
            session_for_socket(list, "/h/herdr.sock").as_deref(),
            Some("default")
        );
        assert_eq!(
            session_for_socket(list, "/h/sessions/work/herdr.sock").as_deref(),
            Some("work")
        );
        assert_eq!(session_for_socket(list, "/elsewhere.sock"), None);
        assert_eq!(session_for_socket("not json", "/h/herdr.sock"), None);
    }

    #[test]
    fn fnv1a_matches_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn instants_round_trip_through_unix_ms() {
        let now = Instant::now();
        let earlier = now - Duration::from_secs(42);
        let ms = to_unix_ms(earlier, now, 1_000_000);
        assert_eq!(ms, 958_000);
        let back = from_unix_ms(ms, now, 1_000_000);
        assert_eq!(now.duration_since(back), Duration::from_secs(42));
    }

    #[test]
    fn state_files_and_dirs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = std::env::temp_dir().join(format!("werkbank-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let log = dir.join("x.log");
        std::fs::write(&log, "old\n").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_private_append(&log).unwrap();
        assert_eq!(mode(&log), 0o600, "an existing log is tightened");
        let state = dir.join("state.json");
        std::fs::write(&state, "{}").unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
        crate::features::daemon::write_atomic(&state, b"{}").unwrap();
        assert_eq!(mode(&state), 0o600, "a rewrite replaces it owner-only");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
