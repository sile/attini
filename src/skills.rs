//! Filesystem-facing side of the skill system: enumerate available
//! skills from `~/.attini/skills/{NAME}/SKILL.md` (global) and
//! `.attini/skills/{NAME}/SKILL.md` (project), load a skill body with
//! byte-cap enforcement, extract the `description:` line from the
//! YAML-shaped frontmatter (attini only reads that one field), and
//! substitute `$ARGUMENTS` in the body.
//!
//! The pure `SkillLoadInvocation` (tool schema + JSON parse) lives in
//! `crate::sansio::agent` alongside the other tool definitions.
//! `crate::agent_cli` glues the two sides together in the `drive`
//! tool-call loop.
//!
//! Resolution order: project (`.attini/skills`) first, then global
//! (`~/.attini/skills`). Project wins on same-name collision.
//! `HOME` unset → global tier silently skipped (memories.rs falls
//! back to `.` and double-reads the project file, but skills do not
//! to keep listings clean).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::session::session_root;

pub const SKILL_MAX_BYTES: usize = 256 * 1024;

const SKILL_FILENAME: &str = "SKILL.md";
const SKILLS_SUBDIR: &str = "skills";

#[derive(Debug)]
pub enum SkillLoadError {
    /// No SKILL.md found under any known root.
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
                "no SKILL.md found under any skill root".to_string(),
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

/// One entry in the discovery listing that gets prepended to the
/// system prompt so the model knows which skills are available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    /// Extracted from the frontmatter's `description:` line; `None`
    /// if the field is missing, empty, or uses an unsupported YAML
    /// form (block scalar).
    pub description: Option<String>,
    /// Absolute path of the skill directory (contains SKILL.md).
    pub path: PathBuf,
    /// `true` if SKILL.md size exceeds [`SKILL_MAX_BYTES`]; discovery
    /// still lists it (with a warning marker) so users can spot the
    /// bad skill, but `skill_load` will error out.
    pub oversized: bool,
    /// `true` if the SKILL.md size probe failed (missing / permission
    /// error). Listed with a note so the user is aware.
    pub broken: bool,
}

/// Roots consulted in order. First = global, last = project (project
/// wins on collision because we insert-overwrite in `discover_all`).
fn skill_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = crate::memories::home_dir() {
        roots.push(home.join(".attini").join(SKILLS_SUBDIR));
    }
    roots.push(session_root().join(SKILLS_SUBDIR));
    roots
}

/// Return the directory of the named skill, honoring project-over-
/// global precedence. Used by both `skill_load` (fs I/O side) and
/// discovery to guarantee they resolve the same file.
pub fn resolve_skill_dir(name: &str) -> Option<PathBuf> {
    resolve_skill_dir_in(&skill_roots(), name)
}

/// Explicit-roots variant of [`resolve_skill_dir`], useful for tests
/// that need isolation from `HOME` and CWD. `roots` is interpreted
/// low-precedence-first (the last entry wins on same-name collision,
/// matching the default `[global, project]` order).
pub fn resolve_skill_dir_in(roots: &[PathBuf], name: &str) -> Option<PathBuf> {
    for root in roots.iter().rev() {
        let candidate = root.join(name);
        if candidate.join(SKILL_FILENAME).is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Enumerate every skill dir under any known root, deduped by name
/// with project winning over global. Returns entries sorted by name.
pub fn discover_all() -> Vec<SkillEntry> {
    discover_all_in(&skill_roots())
}

/// Explicit-roots variant of [`discover_all`], useful for tests.
/// `roots` is interpreted low-precedence-first: entries from later
/// roots overwrite entries with the same name from earlier ones.
pub fn discover_all_in(roots: &[PathBuf]) -> Vec<SkillEntry> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<String, SkillEntry> = BTreeMap::new();
    for root in roots {
        let read_dir = match fs::read_dir(root) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let skill_md = path.join(SKILL_FILENAME);
            if !skill_md.is_file() {
                continue;
            }
            let (description, oversized, broken) = probe_skill(&skill_md);
            by_name.insert(
                name.to_string(),
                SkillEntry {
                    name: name.to_string(),
                    description,
                    path: path.clone(),
                    oversized,
                    broken,
                },
            );
        }
    }
    by_name.into_values().collect()
}

fn probe_skill(skill_md: &Path) -> (Option<String>, bool, bool) {
    let metadata = match fs::metadata(skill_md) {
        Ok(m) => m,
        Err(_) => return (None, false, true),
    };
    let size = metadata.len();
    if size > SKILL_MAX_BYTES as u64 {
        return (None, true, false);
    }
    match fs::read_to_string(skill_md) {
        Ok(text) => (extract_description(&text), false, false),
        Err(_) => (None, false, true),
    }
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

/// Extract the `description:` value from the YAML-shaped frontmatter
/// of a SKILL.md body. Only handles the flat single-line form; block
/// scalars (`description: |` / `description: >`) resolve to `None`.
///
/// Rules: the first non-empty line must be exactly `---`; scan lines
/// up to the closing `---` for the first `description:` prefix at
/// column 0 (case-sensitive); trim the value, strip a matched outer
/// quote pair, and return `None` on empty / block-scalar / missing.
pub fn extract_description(text: &str) -> Option<String> {
    // Step 2: identify the frontmatter block. The first non-empty
    // line must be exactly "---" (no surrounding whitespace).
    let mut lines = text.lines();
    let first_non_empty = loop {
        match lines.next() {
            Some(line) if line.trim().is_empty() => continue,
            Some(line) => break line,
            None => return None,
        }
    };
    if first_non_empty != "---" {
        return None;
    }
    // Collect lines until the closing "---".
    let mut frontmatter: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        frontmatter.push(line);
    }
    if !closed {
        return None;
    }
    // Step 3: find the first line starting with `description:` at
    // column 0, case-sensitive.
    let raw_value = frontmatter
        .iter()
        .find_map(|line| line.strip_prefix("description:"))?;
    // Step 4-6: trim whitespace, reject block scalars, strip an
    // outermost matching quote pair.
    let trimmed = raw_value.trim();
    if trimmed.starts_with('|') || trimmed.starts_with('>') {
        return None;
    }
    let unquoted = strip_matched_quotes(trimmed);
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

fn strip_matched_quotes(s: &str) -> &str {
    for quote in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(quote) && s.ends_with(quote) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Substitute `$ARGUMENTS` in the body with `args`. Uses plain
/// `str::replace`, so word boundaries are not guaranteed —
/// `$ARGUMENTSFOO` would also match. Skill authors are responsible
/// for keeping the marker unambiguous.
pub fn substitute_arguments(body: &str, args: &str) -> String {
    body.replace("$ARGUMENTS", args)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // extract_description
    // -----------------------------------------------------------------

    #[test]
    fn extract_description_returns_value_from_flat_frontmatter() {
        let text = "---\nname: foo\ndescription: hello world\n---\n\nbody\n";
        assert_eq!(extract_description(text), Some("hello world".to_string()));
    }

    #[test]
    fn extract_description_none_when_no_frontmatter() {
        let text = "# Just body\n\ndescription: not here\n";
        assert_eq!(extract_description(text), None);
    }

    #[test]
    fn extract_description_tolerates_leading_blank_lines_before_frontmatter() {
        let text = "\n\n---\ndescription: hi\n---\nbody";
        assert_eq!(extract_description(text), Some("hi".to_string()));
    }

    #[test]
    fn extract_description_none_when_frontmatter_unclosed() {
        let text = "---\ndescription: hi\n(no closing marker)\nbody";
        assert_eq!(extract_description(text), None);
    }

    #[test]
    fn extract_description_none_for_empty_value() {
        let text = "---\ndescription:\n---\nbody";
        assert_eq!(extract_description(text), None);
    }

    #[test]
    fn extract_description_strips_matched_double_quotes() {
        let text = "---\ndescription: \"quoted value\"\n---\nbody";
        assert_eq!(extract_description(text), Some("quoted value".to_string()));
    }

    #[test]
    fn extract_description_strips_matched_single_quotes() {
        let text = "---\ndescription: 'quoted value'\n---\nbody";
        assert_eq!(extract_description(text), Some("quoted value".to_string()));
    }

    #[test]
    fn extract_description_does_not_strip_mismatched_quotes() {
        let text = "---\ndescription: \"mismatched'\n---\nbody";
        assert_eq!(extract_description(text), Some("\"mismatched'".to_string()));
    }

    #[test]
    fn extract_description_none_for_block_scalar_pipe() {
        let text = "---\ndescription: |\n  multi\n  line\n---\nbody";
        assert_eq!(extract_description(text), None);
    }

    #[test]
    fn extract_description_none_for_block_scalar_gt() {
        let text = "---\ndescription: >\n  folded\n---\nbody";
        assert_eq!(extract_description(text), None);
    }

    #[test]
    fn extract_description_ignores_capitalised_key() {
        let text = "---\nDescription: nope\n---\nbody";
        assert_eq!(extract_description(text), None);
    }

    #[test]
    fn extract_description_ignores_indented_key() {
        let text = "---\n  description: indented\n---\nbody";
        assert_eq!(extract_description(text), None);
    }

    #[test]
    fn extract_description_takes_first_of_multiple_occurrences() {
        let text = "---\ndescription: first\ndescription: second\n---\nbody";
        assert_eq!(extract_description(text), Some("first".to_string()));
    }

    // -----------------------------------------------------------------
    // substitute_arguments
    // -----------------------------------------------------------------

    #[test]
    fn substitute_arguments_replaces_marker() {
        assert_eq!(
            substitute_arguments("Run: $ARGUMENTS please", "cargo test"),
            "Run: cargo test please".to_string()
        );
    }

    #[test]
    fn substitute_arguments_uses_empty_string_when_args_empty() {
        assert_eq!(
            substitute_arguments("A $ARGUMENTS B", ""),
            "A  B".to_string()
        );
    }

    #[test]
    fn substitute_arguments_no_op_when_marker_absent() {
        assert_eq!(
            substitute_arguments("plain body", "ignored"),
            "plain body".to_string()
        );
    }

    #[test]
    fn substitute_arguments_replaces_all_occurrences() {
        assert_eq!(
            substitute_arguments("$ARGUMENTS and $ARGUMENTS", "x"),
            "x and x".to_string()
        );
    }

    #[test]
    fn substitute_arguments_touches_prefix_collision_by_design() {
        // Documented behavior: word boundary is not guaranteed;
        // `$ARGUMENTSX` starts with the marker and is (mis-)touched.
        assert_eq!(
            substitute_arguments("$ARGUMENTSX", "Y"),
            "YX".to_string(),
            "prefix collision is intentional per skill spec"
        );
    }

    // -----------------------------------------------------------------
    // load_body
    // -----------------------------------------------------------------

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

    // -----------------------------------------------------------------
    // to_code_and_message
    // -----------------------------------------------------------------

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
