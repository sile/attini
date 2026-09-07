//! Filesystem-facing side of the skill system: load a skill body
//! (SKILL.md) from a user-supplied path with byte-cap enforcement and
//! UTF-8 validation.
//!
//! Skills are selected explicitly via `attini agent --skill <PATH>`.
//! There is no filesystem discovery, no "Available skills" listing, and
//! no model-driven `skill_load` tool. Context enters only because the
//! human asked for it, at invocation start — mirroring attini's
//! explicit / controllable philosophy.
//!
//! `crate::agent_cli` resolves `--skill <PATH>` (relative paths against
//! the workspace root) and calls [`load_from_path`] at the start of a
//! fresh invocation.

use std::fs;
use std::io;
use std::path::Path;

pub const SKILL_MAX_BYTES: usize = 256 * 1024;

const SKILL_FILENAME: &str = "SKILL.md";

#[derive(Debug)]
pub enum SkillLoadError {
    /// No SKILL.md at the given path (or the path does not exist).
    NotFound,
    /// SKILL.md size exceeds [`SKILL_MAX_BYTES`].
    TooLarge { bytes: u64 },
    /// I/O error opening or reading SKILL.md.
    Io(io::Error),
    /// SKILL.md is not valid UTF-8.
    NotUtf8,
}

impl SkillLoadError {
    /// Split into `(error_code, human_message)` for
    /// `tool_error_json` (mirrors [`crate::sansio::agent::CommandError::to_code_and_message`]).
    pub fn to_code_and_message(&self) -> (&'static str, String) {
        match self {
            Self::NotFound => (
                "skill_not_found",
                "no SKILL.md found at the given path".to_string(),
            ),
            Self::TooLarge { bytes } => (
                "skill_too_large",
                format!("SKILL.md is {bytes} bytes (max {SKILL_MAX_BYTES})"),
            ),
            Self::Io(e) => ("skill_load_failed", format!("failed to read SKILL.md: {e}")),
            Self::NotUtf8 => (
                "skill_load_failed",
                "SKILL.md is not valid UTF-8".to_string(),
            ),
        }
    }
}

impl std::fmt::Display for SkillLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (_, msg) = self.to_code_and_message();
        f.write_str(&msg)
    }
}

/// Resolve a `--skill <PATH>` value against `base_dir` (the workspace
/// root) and load its SKILL.md.
///
/// If `path` is a directory, `SKILL.md` inside it is used; if `path`
/// is a file, the file is used directly.
pub fn load_from_path(base_dir: &Path, path: &Path) -> Result<String, SkillLoadError> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    let skill_md = if resolved.is_dir() {
        resolved.join(SKILL_FILENAME)
    } else {
        resolved
    };
    load_body(&skill_md)
}

/// Load SKILL.md as UTF-8 text after verifying it fits within
/// [`SKILL_MAX_BYTES`]. Takes the SKILL.md file path (not the
/// directory).
pub fn load_body(skill_md: &Path) -> Result<String, SkillLoadError> {
    let metadata = fs::metadata(skill_md).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            SkillLoadError::NotFound
        } else {
            SkillLoadError::Io(e)
        }
    })?;
    let size = metadata.len();
    if size > SKILL_MAX_BYTES as u64 {
        return Err(SkillLoadError::TooLarge { bytes: size });
    }
    let bytes = fs::read(skill_md).map_err(SkillLoadError::Io)?;
    String::from_utf8(bytes).map_err(|_| SkillLoadError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("attini-skills-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    #[test]
    fn load_body_reads_utf8_content() {
        let dir = tempdir("load_body_ok");
        let path = dir.join("SKILL.md");
        fs::write(&path, "---\ndescription: hi\n---\nbody").expect("write");
        let body = load_body(&path).expect("load ok");
        assert!(body.contains("body"));
    }

    #[test]
    fn load_from_path_accepts_a_directory_containing_skill_md() {
        let dir = tempdir("load_path_dir");
        fs::write(dir.join("SKILL.md"), "body-dir").expect("write");
        let parent = dir.parent().unwrap().to_path_buf();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let body = load_from_path(&parent, Path::new(&name)).expect("load");
        assert_eq!(body, "body-dir");
    }

    #[test]
    fn load_from_path_accepts_a_skill_md_file_directly() {
        let dir = tempdir("load_path_file");
        let file = dir.join("custom.md");
        fs::write(&file, "body-file").expect("write");
        let body = load_from_path(&dir, Path::new("custom.md")).expect("load");
        assert_eq!(body, "body-file");
    }

    #[test]
    fn load_from_path_resolves_absolute_paths_without_base() {
        let dir = tempdir("load_path_abs");
        let file = dir.join("SKILL.md");
        fs::write(&file, "body-abs").expect("write");
        // Pass an absolute path; base is ignored.
        let body = load_from_path(Path::new("/nonexistent-base"), &file).expect("load");
        assert_eq!(body, "body-abs");
    }

    #[test]
    fn load_from_path_missing_path_returns_not_found() {
        let dir = tempdir("load_path_missing");
        match load_from_path(&dir, Path::new("nope")) {
            Err(SkillLoadError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn load_body_rejects_files_over_the_byte_cap() {
        let dir = tempdir("load_body_toolarge");
        let path = dir.join("SKILL.md");
        let big = vec![b'a'; SKILL_MAX_BYTES + 1];
        fs::write(&path, big).expect("write");
        match load_body(&path) {
            Err(SkillLoadError::TooLarge { bytes }) => {
                assert!(bytes as usize > SKILL_MAX_BYTES)
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn load_body_admits_files_exactly_at_the_byte_cap() {
        let dir = tempdir("load_body_atcap");
        let path = dir.join("SKILL.md");
        let at_cap = vec![b'a'; SKILL_MAX_BYTES];
        fs::write(&path, at_cap).expect("write");
        assert!(load_body(&path).is_ok());
    }

    #[test]
    fn load_body_missing_returns_not_found() {
        let dir = tempdir("load_body_missing");
        let path = dir.join("SKILL.md");
        match load_body(&path) {
            Err(SkillLoadError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn load_body_non_utf8_returns_not_utf8() {
        let dir = tempdir("load_body_notutf8");
        let path = dir.join("SKILL.md");
        fs::write(&path, [0xFFu8, 0xFEu8, 0x00u8]).expect("write");
        match load_body(&path) {
            Err(SkillLoadError::NotUtf8) => {}
            other => panic!("expected NotUtf8, got {other:?}"),
        }
    }

    #[test]
    fn to_code_and_message_maps_variants_to_stable_error_codes() {
        assert_eq!(
            SkillLoadError::NotFound.to_code_and_message().0,
            "skill_not_found"
        );
        assert_eq!(
            SkillLoadError::TooLarge { bytes: 0 }
                .to_code_and_message()
                .0,
            "skill_too_large"
        );
        assert_eq!(
            SkillLoadError::NotUtf8.to_code_and_message().0,
            "skill_load_failed"
        );
        assert_eq!(
            SkillLoadError::Io(io::Error::other("x"))
                .to_code_and_message()
                .0,
            "skill_load_failed"
        );
    }
}
