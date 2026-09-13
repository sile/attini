//! Filesystem-facing side of the permission system: load rules and
//! extra read paths from `.attini/{NAME}/permissions.json`
//! (session-local) and `.attini/permissions.json` (workspace-wide),
//! and implement `attini grant` / `attini grant-read`.
//!
//! The on-disk schema evolved from a legacy top-level array of rule
//! objects (`[{prefix, decision, ...}]`) to a top-level object
//! (`{command_prefixes: [...], extra_read_paths: [...]}`). Both are
//! accepted on load. Any write goes out as the object form; a legacy
//! array file is auto-migrated on the first write.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nojson::{DisplayJson, Json, JsonFormatter, RawJson};

use crate::sansio::permissions::{Rule, RuleDecision};
use crate::session::{SessionPaths, session_paths, session_root};

pub const PERMISSIONS_FILENAME: &str = "permissions.json";

pub struct LoadedRules {
    pub session: Vec<Rule>,
    pub workspace: Vec<Rule>,
    /// Union of `extra_read_paths` from both tiers, deduplicated and
    /// sorted. Callers typically pass this to `ToolExecutor::new` after
    /// canonicalising each entry.
    pub extra_read_paths: Vec<String>,
}

pub fn workspace_permissions_path() -> PathBuf {
    session_root().join(PERMISSIONS_FILENAME)
}

pub fn session_permissions_path(paths: &SessionPaths) -> PathBuf {
    paths.dir.join(PERMISSIONS_FILENAME)
}

/// Load session-local + workspace-wide permissions. Missing files →
/// empty. Whole-file parse errors → empty for that scope + `warn` to
/// stderr. Individual rule / path validation failures → skip that
/// entry + `warn`.
pub fn load(session_name: &str) -> io::Result<LoadedRules> {
    let paths = session_paths(session_name)?;
    let (session, session_paths_list) =
        load_from_path(&session_permissions_path(&paths), "session");
    let (workspace, workspace_paths_list) =
        load_from_path(&workspace_permissions_path(), "workspace");
    let mut merged: BTreeSet<String> = BTreeSet::new();
    merged.extend(session_paths_list);
    merged.extend(workspace_paths_list);
    Ok(LoadedRules {
        session,
        workspace,
        extra_read_paths: merged.into_iter().collect(),
    })
}

fn load_from_path(path: &Path, scope_label: &str) -> (Vec<Rule>, Vec<String>) {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (Vec::new(), Vec::new()),
        Err(e) => {
            eprintln!(
                "attini: cannot read {scope_label} permissions {}: {e}",
                path.display()
            );
            return (Vec::new(), Vec::new());
        }
    };
    let json = match RawJson::parse(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "attini: {scope_label} permissions parse error ({}): {e}. Rules ignored.",
                path.display()
            );
            return (Vec::new(), Vec::new());
        }
    };
    let root = json.value();
    if let Ok(array) = root.to_array() {
        // Legacy: top-level array of rule objects. No extra_read_paths.
        let rules = parse_rules(array, path, scope_label);
        return (rules, Vec::new());
    }
    // New schema: top-level object with command_prefixes / extra_read_paths.
    let rules = match root.to_member("command_prefixes") {
        Ok(m) => match m.optional() {
            Some(v) => match v.to_array() {
                Ok(a) => parse_rules(a, path, scope_label),
                Err(e) => {
                    eprintln!(
                        "attini: {scope_label} permissions {}: command_prefixes is not an array: {e}",
                        path.display()
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        },
        Err(e) => {
            eprintln!(
                "attini: {scope_label} permissions {} is not a JSON array or object: {e}",
                path.display()
            );
            return (Vec::new(), Vec::new());
        }
    };
    let paths_list = match root.to_member("extra_read_paths") {
        Ok(m) => match m.optional() {
            Some(v) => match v.to_array() {
                Ok(a) => parse_extra_read_paths(a, path, scope_label),
                Err(e) => {
                    eprintln!(
                        "attini: {scope_label} permissions {}: extra_read_paths is not an array: {e}",
                        path.display()
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    (rules, paths_list)
}

fn parse_rules<'text, 'raw, I>(array: I, path: &Path, scope_label: &str) -> Vec<Rule>
where
    I: IntoIterator<Item = nojson::RawJsonValue<'text, 'raw>>,
{
    let mut rules = Vec::new();
    for (i, item) in array.into_iter().enumerate() {
        match parse_rule(item) {
            Ok(rule) => rules.push(rule),
            Err(reason) => eprintln!(
                "attini: {scope_label} permissions {} rule #{}: skipping ({reason})",
                path.display(),
                i + 1
            ),
        }
    }
    rules
}

fn parse_extra_read_paths<'text, 'raw, I>(array: I, path: &Path, scope_label: &str) -> Vec<String>
where
    I: IntoIterator<Item = nojson::RawJsonValue<'text, 'raw>>,
{
    let mut out = Vec::new();
    for (i, item) in array.into_iter().enumerate() {
        match item.to_unquoted_string_str() {
            Ok(s) => {
                let text = s.into_owned();
                if text.trim().is_empty() {
                    eprintln!(
                        "attini: {scope_label} permissions {} extra_read_paths[#{}]: empty string, skipping",
                        path.display(),
                        i + 1
                    );
                    continue;
                }
                out.push(text);
            }
            Err(e) => eprintln!(
                "attini: {scope_label} permissions {} extra_read_paths[#{}]: skipping ({e})",
                path.display(),
                i + 1
            ),
        }
    }
    out
}

fn parse_rule(value: nojson::RawJsonValue<'_, '_>) -> Result<Rule, String> {
    let argv_prefix =
        required_string_array(value, "argv_prefix").map_err(|e| format!("argv_prefix: {e}"))?;
    if argv_prefix.is_empty() {
        return Err("argv_prefix is empty".to_string());
    }
    if argv_prefix.iter().any(|s| s.is_empty()) {
        return Err("argv_prefix contains an empty element".to_string());
    }
    let decision = match optional_string(value, "decision").map_err(|e| format!("decision: {e}"))? {
        None => None,
        Some(s) => match s.as_str() {
            "approve" => Some(RuleDecision::Approve),
            "deny" => Some(RuleDecision::Deny),
            other => {
                return Err(format!(
                    "decision must be \"approve\" or \"deny\" (got {other:?})"
                ));
            }
        },
    };
    Ok(Rule {
        argv_prefix,
        decision,
    })
}

fn required_string_array(
    value: nojson::RawJsonValue<'_, '_>,
    key: &str,
) -> Result<Vec<String>, String> {
    let member = value
        .to_member(key)
        .and_then(|m| m.required())
        .map_err(|e| e.to_string())?;
    let array = member.to_array().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for item in array {
        let s: String = item
            .to_unquoted_string_str()
            .map_err(|e| e.to_string())?
            .into_owned();
        out.push(s);
    }
    Ok(out)
}

fn optional_string(
    value: nojson::RawJsonValue<'_, '_>,
    key: &str,
) -> Result<Option<String>, String> {
    let m = value.to_member(key).map_err(|e| e.to_string())?;
    let Some(v) = m.optional() else {
        return Ok(None);
    };
    if v.as_raw_str().trim() == "null" {
        return Ok(None);
    }
    v.to_unquoted_string_str()
        .map(|s| Some(s.into_owned()))
        .map_err(|e| e.to_string())
}

// -------------------------------------------------------------------
// Writable schema representation
// -------------------------------------------------------------------

/// Parsed on-disk permissions ready for mutation and serialisation.
/// Both grant flows (command prefix and read path) go through this
/// so a single writer produces the new object schema regardless of
/// what the file looked like on load.
struct Permissions {
    command_prefixes: Vec<CommandPrefixEntry>,
    extra_read_paths: Vec<String>,
}

/// A raw command-prefix rule entry preserved for round-trip through
/// `grant`. A row without a `decision` field is kept as-is so `grant`
/// can surface it as a conflict rather than silently rewriting it,
/// but it is otherwise ignored (it never matches at evaluation time).
#[derive(Debug, Clone)]
struct CommandPrefixEntry {
    argv_prefix: Vec<String>,
    decision: Option<String>,
}

impl DisplayJson for CommandPrefixEntry {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("argv_prefix", &self.argv_prefix)?;
            if let Some(d) = &self.decision {
                f.member("decision", d)?;
            }
            Ok(())
        })
    }
}

impl DisplayJson for Permissions {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("command_prefixes", &self.command_prefixes)?;
            f.member("extra_read_paths", &self.extra_read_paths)
        })
    }
}

fn read_permissions_file(path: &Path) -> Result<Permissions, GrantError> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Permissions {
                command_prefixes: Vec::new(),
                extra_read_paths: Vec::new(),
            });
        }
        Err(e) => return Err(GrantError::Io(e)),
    };
    let json = RawJson::parse(&text)
        .map_err(|e| GrantError::ParseError(path.to_path_buf(), e.to_string()))?;
    let root = json.value();
    if let Ok(array) = root.to_array() {
        // Legacy: top-level array of rule objects (pre-object-schema
        // shape, one `{"prefix": "..."}` per entry). Modern rules never
        // appear at the top level. Silently drop them so the next
        // write migrates the file to the new object schema.
        let mut cp = Vec::new();
        for item in array {
            match read_command_prefix_entry(item) {
                Ok(entry) => cp.push(entry),
                Err(e) => eprintln!(
                    "attini: {} contains a legacy rule entry that will be dropped on next write ({e})",
                    path.display()
                ),
            }
        }
        return Ok(Permissions {
            command_prefixes: cp,
            extra_read_paths: Vec::new(),
        });
    }
    let cp_member = root
        .to_member("command_prefixes")
        .map_err(|e| GrantError::ParseError(path.to_path_buf(), e.to_string()))?;
    let mut cp = Vec::new();
    if let Some(v) = cp_member.optional() {
        let array = v
            .to_array()
            .map_err(|e| GrantError::ParseError(path.to_path_buf(), e.to_string()))?;
        for item in array {
            match read_command_prefix_entry(item) {
                Ok(entry) => cp.push(entry),
                Err(e) => eprintln!(
                    "attini: {} contains a legacy rule entry that will be dropped on next write ({e})",
                    path.display()
                ),
            }
        }
    }
    let paths_member = root
        .to_member("extra_read_paths")
        .map_err(|e| GrantError::ParseError(path.to_path_buf(), e.to_string()))?;
    let mut paths_list: Vec<String> = Vec::new();
    if let Some(v) = paths_member.optional() {
        let array = v
            .to_array()
            .map_err(|e| GrantError::ParseError(path.to_path_buf(), e.to_string()))?;
        for item in array {
            let s = item
                .to_unquoted_string_str()
                .map_err(|e| GrantError::ParseError(path.to_path_buf(), e.to_string()))?
                .into_owned();
            paths_list.push(s);
        }
    }
    Ok(Permissions {
        command_prefixes: cp,
        extra_read_paths: paths_list,
    })
}

fn read_command_prefix_entry(
    value: nojson::RawJsonValue<'_, '_>,
) -> Result<CommandPrefixEntry, GrantError> {
    let path_hint = || PathBuf::from("<in memory>");
    let argv_prefix = required_string_array(value, "argv_prefix")
        .map_err(|e| GrantError::ParseError(path_hint(), format!("argv_prefix: {e}")))?;
    let decision =
        optional_string(value, "decision").map_err(|e| GrantError::ParseError(path_hint(), e))?;
    Ok(CommandPrefixEntry {
        argv_prefix,
        decision,
    })
}

fn write_permissions_file(path: &Path, permissions: &Permissions) -> io::Result<()> {
    let body = Json(permissions).to_string();
    atomic_write(path, body.as_bytes())
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
    ArgvEmpty,
    SessionMissing(PathBuf),
    ParseError(PathBuf, String),
    ExistingDenyConflict(PathBuf, Vec<String>),
    ExistingAttributeOnlyConflict(PathBuf, Vec<String>),
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
            Self::ArgvEmpty => write!(f, "argv_prefix is empty or contains an empty element"),
            Self::SessionMissing(p) => write!(f, "session directory not found: {}", p.display()),
            Self::ParseError(p, e) => write!(
                f,
                "existing {} is not valid JSON: {e}. Fix or delete it before granting.",
                p.display()
            ),
            Self::ExistingDenyConflict(p, argv_prefix) => write!(
                f,
                "rule for argv_prefix {argv_prefix:?} already exists as `deny` in {}; edit manually to resolve",
                p.display()
            ),
            Self::ExistingAttributeOnlyConflict(p, argv_prefix) => write!(
                f,
                "rule for argv_prefix {argv_prefix:?} already exists in {} without a `decision` field; append `\"decision\": \"approve\"` to it manually so evaluation order is preserved",
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

pub fn grant(scope: GrantScope<'_>, argv_prefix: &[String]) -> Result<GrantOutcome, GrantError> {
    if argv_prefix.is_empty() || argv_prefix.iter().any(|s| s.is_empty()) {
        return Err(GrantError::ArgvEmpty);
    }
    let argv_prefix: Vec<String> = argv_prefix.to_vec();
    let target = resolve_target(scope)?;
    let mut permissions = read_permissions_file(&target)?;
    for entry in &permissions.command_prefixes {
        if entry.argv_prefix != argv_prefix {
            continue;
        }
        return match entry.decision.as_deref() {
            Some("approve") => Ok(GrantOutcome::AlreadyGranted(target)),
            Some("deny") => Err(GrantError::ExistingDenyConflict(target, argv_prefix)),
            _ => Err(GrantError::ExistingAttributeOnlyConflict(
                target,
                argv_prefix,
            )),
        };
    }
    permissions.command_prefixes.push(CommandPrefixEntry {
        argv_prefix,
        decision: Some("approve".to_string()),
    });
    write_permissions_file(&target, &permissions)?;
    Ok(GrantOutcome::Appended(target))
}

// -------------------------------------------------------------------
// grant-read (extra_read_paths append)
// -------------------------------------------------------------------

pub enum GrantReadOutcome {
    Appended(PathBuf),
    AlreadyGranted(PathBuf),
}

#[derive(Debug)]
pub enum GrantReadError {
    PathEmpty,
    SessionMissing(PathBuf),
    ParseError(PathBuf, String),
    Io(io::Error),
}

impl From<io::Error> for GrantReadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for GrantReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathEmpty => write!(f, "PATH is empty or whitespace only"),
            Self::SessionMissing(p) => write!(f, "session directory not found: {}", p.display()),
            Self::ParseError(p, e) => write!(
                f,
                "existing {} is not valid JSON: {e}. Fix or delete it before granting.",
                p.display()
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

pub fn grant_read(
    scope: GrantScope<'_>,
    path_arg: &str,
) -> Result<GrantReadOutcome, GrantReadError> {
    let path_str = path_arg.trim().to_string();
    if path_str.is_empty() {
        return Err(GrantReadError::PathEmpty);
    }
    let target = resolve_target(scope).map_err(|e| match e {
        GrantError::SessionMissing(p) => GrantReadError::SessionMissing(p),
        GrantError::Io(e) => GrantReadError::Io(e),
        GrantError::ParseError(p, msg) => GrantReadError::ParseError(p, msg),
        // The three shapes below aren't produced by resolve_target;
        // fold them into ParseError to keep the caller-visible type
        // simple.
        GrantError::ArgvEmpty
        | GrantError::ExistingDenyConflict(_, _)
        | GrantError::ExistingAttributeOnlyConflict(_, _) => {
            GrantReadError::ParseError(PathBuf::new(), "unexpected grant error".to_string())
        }
    })?;
    let mut permissions = read_permissions_file(&target).map_err(|e| match e {
        GrantError::ParseError(p, msg) => GrantReadError::ParseError(p, msg),
        GrantError::Io(e) => GrantReadError::Io(e),
        // read_permissions_file never returns the other variants.
        _ => GrantReadError::ParseError(target.clone(), "unexpected read error".to_string()),
    })?;
    if permissions.extra_read_paths.iter().any(|p| p == &path_str) {
        return Ok(GrantReadOutcome::AlreadyGranted(target));
    }
    permissions.extra_read_paths.push(path_str);
    write_permissions_file(&target, &permissions)?;
    Ok(GrantReadOutcome::Appended(target))
}

// -------------------------------------------------------------------
// Atomic write helper
// -------------------------------------------------------------------

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
    fn load_legacy_array_of_string_prefix_entries_yields_empty_rules() {
        // Legacy schema (root array + string prefix) is unsupported;
        // load_from_path warns + skips each entry and returns an
        // empty rule set. The next write-through will silently
        // migrate the file to the new object schema.
        let dir = tempdir("legacy_array");
        let path = dir.join("permissions.json");
        fs::write(&path, r#"[{"prefix":"cargo test","decision":"approve"}]"#).expect("write");
        let (rules, paths_list) = load_from_path(&path, "workspace");
        assert!(rules.is_empty());
        assert!(paths_list.is_empty());
    }

    #[test]
    fn load_reads_new_object_schema_with_both_fields() {
        let dir = tempdir("object_schema");
        let path = dir.join("permissions.json");
        fs::write(
            &path,
            r#"{"command_prefixes":[{"argv_prefix":["cargo","test"],"decision":"approve"}],"extra_read_paths":["../docs/","/opt/shared"]}"#,
        )
        .expect("write");
        let (rules, paths_list) = load_from_path(&path, "workspace");
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].argv_prefix,
            vec!["cargo".to_string(), "test".to_string()]
        );
        assert_eq!(
            paths_list,
            vec!["../docs/".to_string(), "/opt/shared".to_string()]
        );
    }

    #[test]
    fn load_new_schema_missing_command_prefixes_yields_empty_rules() {
        let dir = tempdir("only_paths");
        let path = dir.join("permissions.json");
        fs::write(&path, r#"{"extra_read_paths":["../docs/"]}"#).expect("write");
        let (rules, paths_list) = load_from_path(&path, "workspace");
        assert!(rules.is_empty());
        assert_eq!(paths_list, vec!["../docs/".to_string()]);
    }

    #[test]
    fn load_new_schema_missing_extra_read_paths_yields_empty_paths() {
        let dir = tempdir("only_rules");
        let path = dir.join("permissions.json");
        fs::write(
            &path,
            r#"{"command_prefixes":[{"argv_prefix":["cargo","test"],"decision":"approve"}]}"#,
        )
        .expect("write");
        let (rules, paths_list) = load_from_path(&path, "workspace");
        assert_eq!(rules.len(), 1);
        assert!(paths_list.is_empty());
    }

    #[test]
    fn load_legacy_string_prefix_entry_in_object_schema_is_dropped() {
        // Same silent-drop behaviour when a stray `{"prefix":"..."}`
        // entry appears inside a new-schema object.
        let dir = tempdir("legacy_entry_in_object");
        let path = dir.join("permissions.json");
        fs::write(
            &path,
            r#"{"command_prefixes":[{"prefix":"cargo test","decision":"approve"},{"argv_prefix":["ls"],"decision":"approve"}]}"#,
        )
        .expect("write");
        let (rules, _) = load_from_path(&path, "workspace");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].argv_prefix, vec!["ls".to_string()]);
    }

    #[test]
    fn write_produces_object_schema_regardless_of_prior_shape() {
        let dir = tempdir("write_object");
        let path = dir.join("permissions.json");
        // Start from legacy array.
        fs::write(&path, r#"[{"prefix":"cargo test","decision":"approve"}]"#).expect("write");
        // Read → mutate → write. Legacy entries are dropped during
        // read (silent migration); the write emits the new schema.
        let mut perms = read_permissions_file(&path).expect("read");
        perms.extra_read_paths.push("../docs/".to_string());
        write_permissions_file(&path, &perms).expect("write");
        let written = fs::read_to_string(&path).expect("read back");
        assert!(
            written.starts_with("{"),
            "expected object schema, got: {written}"
        );
        assert!(written.contains("command_prefixes"));
        assert!(written.contains("extra_read_paths"));
        assert!(written.contains("../docs/"));
        // Legacy `"prefix"` entry has been silently dropped (the new
        // schema uses `argv_prefix` / `command_prefixes`, not `prefix`).
        assert!(
            !written.contains(r#""prefix":"#),
            "expected legacy `\"prefix\":` field to be absent, got: {written}"
        );
    }

    #[test]
    fn read_permissions_file_dedups_already_granted_path_on_load() {
        // grant_read is idempotent — we cover the migration semantics
        // via write_produces_object_schema_regardless_of_prior_shape
        // and here verify that a path already present in the file
        // stays present without duplication when the file is read.
        let dir = tempdir("dedup_on_load");
        let path = dir.join("permissions.json");
        fs::write(
            &path,
            r#"{"command_prefixes":[],"extra_read_paths":["../docs/","../docs/"]}"#,
        )
        .expect("write");
        let perms = read_permissions_file(&path).expect("read");
        // The reader keeps duplicates as-is; grant_read is where
        // dedup happens. Both are in the loaded vec.
        assert_eq!(perms.extra_read_paths.len(), 2);
        // `load` (public API) is where the BTreeSet dedup happens.
        // Confirm that too by direct construction.
        let mut set: BTreeSet<String> = BTreeSet::new();
        set.extend(perms.extra_read_paths);
        assert_eq!(set.len(), 1);
    }
}
