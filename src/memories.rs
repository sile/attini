//! Persistent user / project / session memory loaded into every
//! `attini agent` invocation's system prompt.
//!
//! Three tiers (broadest → narrowest) live at:
//! - `~/.attini/memories.md` — user-wide
//! - `.attini/memories.md` — workspace (CWD)-wide
//! - `.attini/{NAME}/memories.md` — session-local
//!
//! On every `attini agent` invocation, all tiers that exist are
//! concatenated into a single system message and prepended to the
//! conversation (before any `--system` prompt). Missing files are
//! silently skipped.

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::session::{session_paths, session_root};

pub const MEMORIES_FILENAME: &str = "memories.md";

/// Load and concatenate the three memory tiers into a single
/// system-message body. Returns `None` if no memory file exists.
pub fn load(session_name: &str) -> io::Result<Option<String>> {
    let mut out = String::new();
    for (label, path) in tiered_paths(session_name)? {
        let text = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                eprintln!("attini: cannot read {label} memory {}: {e}", path.display());
                continue;
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n---\n\n");
        }
        out.push_str(&format!("# {label} memory\n\n"));
        out.push_str(text.trim_end());
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn tiered_paths(session_name: &str) -> io::Result<Vec<(&'static str, PathBuf)>> {
    let session = session_paths(session_name)?;
    Ok(vec![
        ("Global", global_path()),
        ("Project", session_root().join(MEMORIES_FILENAME)),
        ("Session", session.dir.join(MEMORIES_FILENAME)),
    ])
}

fn global_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".attini")
        .join(MEMORIES_FILENAME)
}

/// Best-effort home directory lookup via `$HOME`. Falls back to
/// `None` if the variable is unset or empty; callers substitute
/// `.` so a missing home just means the global tier is a no-op
/// against `./.attini/memories.md` (which the project tier already
/// looks at, so effectively skipped).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}
