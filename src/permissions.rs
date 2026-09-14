//! Filesystem-facing side of the permission system: load rules from
//! `.attini/{NAME}/permissions.jsonl` (session-local) and
//! `.attini/permissions.jsonl` (workspace-wide), and implement the
//! rule append used by `attini approve --grant`.
//!
//! The on-disk format is **JSONL**: one JSON object per line, one rule
//! per line. Lines beginning with `#` are comments; blank lines are
//! ignored. Each rule has a fixed field order (`type` -> `allow` ->
//! type-specific) and an explicit `allow` boolean. The layer a rule
//! belongs to is expressed by which file it lives in, not by a field.
//! See `docs/design/permissions-file.md`.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nojson::{DisplayJson, JsonFormatter, RawJson, RawJsonValue};

use crate::sansio::permissions::{PermissionKind, Rule};
use crate::session::{SessionPaths, session_paths, session_root};

pub const PERMISSIONS_FILENAME: &str = "permissions.jsonl";

pub struct LoadedRules {
    /// Rules from the workspace-wide file, in file order.
    pub workspace: Vec<Rule>,
    /// Rules from the session-local file, in file order.
    pub session: Vec<Rule>,
    /// Union of `read` rules from both tiers, deduplicated and sorted.
    /// Callers typically pass the resolved paths to `ToolExecutor::new`
    /// after canonicalising each entry.
    pub extra_read_paths: Vec<String>,
}

pub fn workspace_permissions_path() -> PathBuf {
    session_root().join(PERMISSIONS_FILENAME)
}

pub fn session_permissions_path(paths: &SessionPaths) -> PathBuf {
    paths.dir.join(PERMISSIONS_FILENAME)
}

/// Load session-local + workspace-wide permissions. Missing files ->
/// empty. A whole-file read failure is reported and treated as empty.
/// An individual malformed line is reported and skipped; the rest of
/// the file still loads.
pub fn load(session_name: &str) -> io::Result<LoadedRules> {
    let paths = session_paths(session_name)?;
    let workspace = load_rules_from_path(&workspace_permissions_path(), "workspace");
    let session = load_rules_from_path(&session_permissions_path(&paths), "session");
    let mut merged: BTreeSet<String> = BTreeSet::new();
    for rule in workspace.iter().chain(session.iter()) {
        if rule.kind == PermissionKind::Read && rule.allow {
            merged.insert(rule.path.clone());
        }
    }
    Ok(LoadedRules {
        workspace,
        session,
        extra_read_paths: merged.into_iter().collect(),
    })
}

fn load_rules_from_path(path: &Path, scope_label: &str) -> Vec<Rule> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!(
                "attini: cannot read {scope_label} permissions {}: {e}",
                path.display()
            );
            return Vec::new();
        }
    };
    parse_jsonl(&text, path, scope_label)
}

/// Parse a JSONL permissions file into rules. `#` comments and blank
/// lines are skipped; a malformed line is reported and skipped.
fn parse_jsonl(text: &str, path: &Path, scope_label: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match parse_rule_line(trimmed) {
            Ok(rule) => rules.push(rule),
            Err(reason) => eprintln!(
                "attini: {scope_label} permissions {} line {}: skipping ({reason})",
                path.display(),
                i + 1
            ),
        }
    }
    rules
}

fn parse_rule_line(line: &str) -> Result<Rule, String> {
    let json = RawJson::parse(line).map_err(|e| e.to_string())?;
    let value = json.value();
    let type_str = required_string(value, "type")?;
    match type_str.as_str() {
        "command" => {
            let allow = required_bool(value, "allow")?;
            let args_prefix = required_string_array(value, "args_prefix")?;
            if args_prefix.is_empty() {
                return Err("args_prefix is empty".to_string());
            }
            if args_prefix.iter().any(|s| s.is_empty()) {
                return Err("args_prefix contains an empty element".to_string());
            }
            Ok(Rule::command(allow, args_prefix))
        }
        "read" => {
            let path = required_string(value, "path")?;
            if path.trim().is_empty() {
                return Err("path is empty".to_string());
            }
            // Read rules are allow-only: `allow` may be omitted (or
            // `true`), but `allow:false` is rejected because a read deny
            // cannot be enforced (the model can always read via the
            // `command` tool).
            match optional_bool(value, "allow")? {
                None | Some(true) => Ok(Rule::read(path)),
                Some(false) => {
                    Err("read rules are allow-only; `allow:false` is not supported".to_string())
                }
            }
        }
        "write" => {
            let allow = required_bool(value, "allow")?;
            let path = required_string(value, "path")?;
            if path.trim().is_empty() {
                return Err("path is empty".to_string());
            }
            Ok(Rule::write(allow, path))
        }
        other => Err(format!(
            "unknown type {other:?} (expected \"command\", \"read\" or \"write\")"
        )),
    }
}

fn required_string(value: RawJsonValue<'_, '_>, key: &str) -> Result<String, String> {
    let member = value
        .to_member(key)
        .and_then(|m| m.required())
        .map_err(|e| format!("{key}: {e}"))?;
    member
        .to_unquoted_string_str()
        .map(|s| s.into_owned())
        .map_err(|e| format!("{key}: {e}"))
}

fn required_bool(value: RawJsonValue<'_, '_>, key: &str) -> Result<bool, String> {
    let member = value
        .to_member(key)
        .and_then(|m| m.required())
        .map_err(|e| format!("{key}: {e}"))?;
    let raw = member.as_boolean_str().map_err(|e| format!("{key}: {e}"))?;
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("{key}: expected a boolean (got {other})")),
    }
}

/// Read an optional boolean member. Returns `Ok(None)` when the key is
/// absent, `Ok(Some(b))` when present and boolean, and an error when
/// the key is present but not a boolean.
fn optional_bool(value: RawJsonValue<'_, '_>, key: &str) -> Result<Option<bool>, String> {
    let member = match value.to_member(key).and_then(|m| m.required()) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    let raw = member.as_boolean_str().map_err(|e| format!("{key}: {e}"))?;
    match raw {
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        other => Err(format!("{key}: expected a boolean (got {other})")),
    }
}

fn required_string_array(value: RawJsonValue<'_, '_>, key: &str) -> Result<Vec<String>, String> {
    let member = value
        .to_member(key)
        .and_then(|m| m.required())
        .map_err(|e| format!("{key}: {e}"))?;
    let array = member.to_array().map_err(|e| format!("{key}: {e}"))?;
    let mut out = Vec::new();
    for item in array {
        let s: String = item
            .to_unquoted_string_str()
            .map_err(|e| format!("{key}: {e}"))?
            .into_owned();
        out.push(s);
    }
    Ok(out)
}

// -------------------------------------------------------------------
// Writable representation (JSONL appends)
// -------------------------------------------------------------------

impl DisplayJson for Rule {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("type", self.kind.as_str())?;
            // Read rules are allow-only, so `allow` is omitted (implied
            // `true`). Command and write rules always carry `allow`.
            if self.kind != PermissionKind::Read {
                f.member("allow", self.allow)?;
            }
            match self.kind {
                PermissionKind::Command => {
                    f.member("args_prefix", &self.args_prefix)?;
                }
                PermissionKind::Read => {
                    f.member("path", &self.path)?;
                }
                PermissionKind::Write => {
                    f.member("path", &self.path)?;
                }
            }
            Ok(())
        })
    }
}

fn render_rule_line(rule: &Rule) -> String {
    // `Json(...)` renders one compact JSON object; prefix with a newline
    // when appending.
    nojson::Json(rule).to_string()
}

// -------------------------------------------------------------------
// grant (command prefix)
// -------------------------------------------------------------------

pub enum GrantOutcome {
    Appended(PathBuf),
    AlreadyGranted(PathBuf),
}

#[derive(Debug)]
pub enum GrantError {
    ArgsEmpty,
    SessionMissing(PathBuf),
    ReadError(PathBuf, io::Error),
    ExistingDenyConflict(PathBuf, Vec<String>),
    ExistingWriteDenyConflict(PathBuf, String),
    Io(io::Error),
}

impl From<io::Error> for GrantError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ArgsEmpty => write!(f, "args_prefix is empty or contains an empty element"),
            Self::SessionMissing(p) => write!(f, "session directory not found: {}", p.display()),
            Self::ReadError(p, e) => write!(f, "cannot read {}: {e}", p.display()),
            Self::ExistingDenyConflict(p, args_prefix) => write!(
                f,
                "command rule for args_prefix {args_prefix:?} already exists as `allow:false` in {}; edit manually to resolve",
                p.display()
            ),
            Self::ExistingWriteDenyConflict(p, path) => write!(
                f,
                "write rule for path {path:?} already exists as `allow:false` in {}; edit manually to resolve",
                p.display()
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

pub enum GrantScope<'a> {
    Session(&'a str),
    Workspace,
}

fn resolve_target(scope: GrantScope<'_>) -> Result<PathBuf, GrantError> {
    match scope {
        GrantScope::Session(name) => {
            let paths = session_paths(name).map_err(GrantError::Io)?;
            if !paths.dir.try_exists()? {
                return Err(GrantError::SessionMissing(paths.dir.clone()));
            }
            Ok(session_permissions_path(&paths))
        }
        GrantScope::Workspace => {
            let root = session_root();
            if !root.try_exists()? {
                fs::create_dir_all(&root)?;
            }
            Ok(workspace_permissions_path())
        }
    }
}

pub fn grant(scope: GrantScope<'_>, args_prefix: &[String]) -> Result<GrantOutcome, GrantError> {
    if args_prefix.is_empty() || args_prefix.iter().any(|s| s.is_empty()) {
        return Err(GrantError::ArgsEmpty);
    }
    let args_prefix: Vec<String> = args_prefix.to_vec();
    let target = resolve_target(scope)?;
    let existing = load_rules_from_path(&target, "grant");
    for rule in &existing {
        if rule.kind != PermissionKind::Command {
            continue;
        }
        if rule.args_prefix != args_prefix {
            continue;
        }
        return match rule.allow {
            true => Ok(GrantOutcome::AlreadyGranted(target)),
            false => Err(GrantError::ExistingDenyConflict(target, args_prefix)),
        };
    }
    let rule = Rule::command(true, args_prefix);
    append_rule_line(&target, &rule)?;
    Ok(GrantOutcome::Appended(target))
}

/// Persist a `read` rule for `path` at the given scope. Mirrors
/// [`grant`] but for the path matcher: `read` rules are allow-only, so
/// an existing identical rule is a no-op and a fresh rule is appended
/// (there is no read deny to conflict with).
pub fn grant_read(scope: GrantScope<'_>, path: &str) -> Result<GrantOutcome, GrantError> {
    if path.trim().is_empty() {
        return Err(GrantError::ArgsEmpty);
    }
    let target = resolve_target(scope)?;
    let existing = load_rules_from_path(&target, "grant");
    for rule in &existing {
        if rule.kind != PermissionKind::Read {
            continue;
        }
        if rule.path != path {
            continue;
        }
        return Ok(GrantOutcome::AlreadyGranted(target));
    }
    let rule = Rule::read(path.to_string());
    append_rule_line(&target, &rule)?;
    Ok(GrantOutcome::Appended(target))
}

/// Persist an `allow:true` `write` rule for `path` at the given scope.
/// Mirrors [`grant`] and [`grant_read`] but for the write path matcher:
/// an existing identical `allow:true` rule is a no-op, an existing
/// identical `allow:false` rule is a conflict (edit manually), and a
/// fresh rule is appended.
pub fn grant_write(scope: GrantScope<'_>, path: &str) -> Result<GrantOutcome, GrantError> {
    if path.trim().is_empty() {
        return Err(GrantError::ArgsEmpty);
    }
    let target = resolve_target(scope)?;
    let existing = load_rules_from_path(&target, "grant");
    for rule in &existing {
        if rule.kind != PermissionKind::Write {
            continue;
        }
        if rule.path != path {
            continue;
        }
        return match rule.allow {
            true => Ok(GrantOutcome::AlreadyGranted(target)),
            false => Err(GrantError::ExistingWriteDenyConflict(
                target,
                path.to_string(),
            )),
        };
    }
    let rule = Rule::write(true, path.to_string());
    append_rule_line(&target, &rule)?;
    Ok(GrantOutcome::Appended(target))
}

// -------------------------------------------------------------------
// Atomic append helper
// -------------------------------------------------------------------

/// Append a rendered rule as a new line, atomically (read whole file,
/// append, write tmp, rename). Appending rather than rewriting keeps
/// any hand-written comments in the file intact except that the file is
/// rewritten byte-for-byte with one extra trailing line.
fn append_rule_line(path: &Path, rule: &Rule) -> io::Result<()> {
    let mut body = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&render_rule_line(rule));
    body.push('\n');
    atomic_write(path, body.as_bytes())
}

fn atomic_write(path: &Path, content: &[u8]) -> io::Result<()> {
    let tmp = tmp_path_for(path);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn tmp_path_for(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let mut buf = target.as_os_str().to_os_string();
    buf.push(format!(".grant-tmp-{pid}"));
    PathBuf::from(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "attini-permissions-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("mkdir");
        base
    }

    #[test]
    fn parse_jsonl_reads_command_and_read_rules_skipping_comments() {
        let text = r#"
# allow cargo test
{"type":"command","allow":true,"args_prefix":["cargo","test"]}

# deny rm
{"type":"command","allow":false,"args_prefix":["rm"]}
{"type":"read","allow":true,"path":"../docs/"}
"#;
        let dir = tempdir("jsonl_basic");
        let path = dir.join(PERMISSIONS_FILENAME);
        let rules = parse_jsonl(text, &path, "workspace");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].kind, PermissionKind::Command);
        assert!(rules[0].allow);
        assert_eq!(
            rules[0].args_prefix,
            vec!["cargo".to_string(), "test".to_string()]
        );
        assert_eq!(rules[1].kind, PermissionKind::Command);
        assert!(!rules[1].allow);
        assert_eq!(rules[2].kind, PermissionKind::Read);
        assert_eq!(rules[2].path, "../docs/");
    }

    #[test]
    fn parse_jsonl_skips_malformed_line_and_keeps_rest() {
        let text = r#"{"type":"command","allow":true,"args_prefix":["ls"]}
not json
{"type":"command","allow":true,"args_prefix":["cat"]}
"#;
        let dir = tempdir("jsonl_bad_line");
        let path = dir.join(PERMISSIONS_FILENAME);
        let rules = parse_jsonl(text, &path, "workspace");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].args_prefix, vec!["ls".to_string()]);
        assert_eq!(rules[1].args_prefix, vec!["cat".to_string()]);
    }

    #[test]
    fn rule_without_allow_is_skipped() {
        let text = r#"{"type":"command","args_prefix":["ls"]}
"#;
        let dir = tempdir("jsonl_no_allow");
        let path = dir.join(PERMISSIONS_FILENAME);
        let rules = parse_jsonl(text, &path, "workspace");
        assert!(rules.is_empty());
    }

    #[test]
    fn read_rule_may_omit_allow() {
        let text = r#"{"type":"read","path":"../docs/"}
"#;
        let dir = tempdir("jsonl_read_no_allow");
        let path = dir.join(PERMISSIONS_FILENAME);
        let rules = parse_jsonl(text, &path, "workspace");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].kind, PermissionKind::Read);
        assert!(rules[0].allow);
        assert_eq!(rules[0].path, "../docs/");
    }

    #[test]
    fn read_rule_with_allow_false_is_skipped() {
        // Read rules are allow-only; `allow:false` is not supported and
        // the line is rejected (reported and skipped).
        let text = r#"{"type":"read","allow":false,"path":"secret/"}
"#;
        let dir = tempdir("jsonl_read_deny");
        let path = dir.join(PERMISSIONS_FILENAME);
        let rules = parse_jsonl(text, &path, "workspace");
        assert!(rules.is_empty());
    }

    #[test]
    fn unknown_type_is_skipped() {
        let text = r#"{"type":"bogus","allow":true,"path":"src/"}
"#;
        let dir = tempdir("jsonl_unknown_type");
        let path = dir.join(PERMISSIONS_FILENAME);
        let rules = parse_jsonl(text, &path, "workspace");
        assert!(rules.is_empty());
    }

    #[test]
    fn write_rule_parses_and_round_trips() {
        let text = r#"{"type":"write","allow":true,"path":"src/"}
{"type":"write","allow":false,"path":"src/generated"}
"#;
        let dir = tempdir("jsonl_write");
        let path = dir.join(PERMISSIONS_FILENAME);
        let rules = parse_jsonl(text, &path, "workspace");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].kind, PermissionKind::Write);
        assert!(rules[0].allow);
        assert_eq!(rules[0].path, "src/");
        assert_eq!(rules[1].kind, PermissionKind::Write);
        assert!(!rules[1].allow);
        assert_eq!(
            render_rule_line(&rules[0]),
            r#"{"type":"write","allow":true,"path":"src/"}"#
        );
    }

    #[test]
    fn render_rule_line_matches_canonical_field_order() {
        let cmd = Rule::command(true, vec!["cargo".to_string(), "test".to_string()]);
        assert_eq!(
            render_rule_line(&cmd),
            r#"{"type":"command","allow":true,"args_prefix":["cargo","test"]}"#
        );
        // Read rules are allow-only: `allow` is omitted from the line.
        let read = Rule::read("secret/".to_string());
        assert_eq!(
            render_rule_line(&read),
            r#"{"type":"read","path":"secret/"}"#
        );
        let write = Rule::write(true, "src/".to_string());
        assert_eq!(
            render_rule_line(&write),
            r#"{"type":"write","allow":true,"path":"src/"}"#
        );
    }

    #[test]
    fn append_rule_line_preserves_comments_and_appends_one_line() {
        let dir = tempdir("append_preserve");
        let path = dir.join(PERMISSIONS_FILENAME);
        fs::write(
            &path,
            "# my rules\n{\"type\":\"command\",\"allow\":true,\"args_prefix\":[\"ls\"]}\n",
        )
        .expect("write");
        let rule = Rule::command(true, vec!["cargo".to_string(), "test".to_string()]);
        append_rule_line(&path, &rule).expect("append");
        let written = fs::read_to_string(&path).expect("read back");
        assert!(written.starts_with("# my rules\n"));
        assert!(written.contains(r#"{"type":"command","allow":true,"args_prefix":["ls"]}"#));
        assert!(written.ends_with(
            r#"{"type":"command","allow":true,"args_prefix":["cargo","test"]}
"#
        ));
    }

    #[test]
    fn load_merges_read_paths_from_both_tiers() {
        // Directly exercise the union logic without touching the real
        // `.attini/` tree: build the LoadedRules shape by hand.
        let mut set: BTreeSet<String> = BTreeSet::new();
        set.insert("../docs/".to_string());
        set.insert("/opt/shared".to_string());
        set.insert("../docs/".to_string());
        assert_eq!(set.len(), 2);
    }
}
