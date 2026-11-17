use std::fmt;

use anyhow::Result;

/// Checked before every side effect, so a stop never waits on more than one call.
pub type Guard<'a> = &'a dyn Fn() -> Result<()>;

/// Returned by work that noticed the stop flag before its next side effect.
#[derive(Debug)]
pub struct Stopped;

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("watcher stopped")
    }
}

impl std::error::Error for Stopped {}
