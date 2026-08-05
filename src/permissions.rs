//! Filesystem-facing side of the permission system: load rules from
//! `.attini/{NAME}/permissions.jsonc` (session-local) and
//! `.attini/permissions.jsonc` (workspace-wide), and implement
//! `attini session grant` (safe text-preserving append).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nojson::RawJson;

use crate::sansio::permissions::{Rule, RuleDecision};
use crate::session::{SessionPaths, session_paths, session_root};

pub const PERMISSIONS_FILENAME: &str = "permissions.jsonc";

pub struct LoadedRules {
    pub session: Vec<Rule>,
    pub workspace: Vec<Rule>,
}

pub fn workspace_permissions_path() -> PathBuf {
    session_root().join(PERMISSIONS_FILENAME)
}

pub fn session_permissions_path(paths: &SessionPaths) -> PathBuf {
    paths.dir.join(PERMISSIONS_FILENAME)
}

/// Load session-local + workspace-wide rules. Missing files → empty.
/// Whole-file parse errors → empty for that scope + `warn` to stderr.
/// Individual rule validation failures → skip that rule + `warn`.
pub fn load(session_name: &str) -> io::Result<LoadedRules> {
    let paths = session_paths(session_name)?;
    let session = load_from_path(&session_permissions_path(&paths), "session");
    let workspace = load_from_path(&workspace_permissions_path(), "workspace");
    Ok(LoadedRules { session, workspace })
}

fn load_from_path(path: &Path, scope_label: &str) -> Vec<Rule> {
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
    let json = match RawJson::parse_jsonc(&text) {
        Ok((v, _comment_ranges)) => v,
        Err(e) => {
            eprintln!(
                "attini: {scope_label} permissions parse error ({}): {e}. Rules ignored.",
                path.display()
            );
            return Vec::new();
        }
    };
    let mut rules = Vec::new();
    let root = json.value();
    let array = match root.to_array() {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "attini: {scope_label} permissions {} is not a JSON array: {e}",
                path.display()
            );
            return Vec::new();
        }
    };
    for (i, item) in array.enumerate() {
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

fn parse_rule(value: nojson::RawJsonValue<'_, '_>) -> Result<Rule, String> {
    let prefix = required_string(value, "prefix").map_err(|e| format!("prefix: {e}"))?;
    if prefix.trim().is_empty() {
        return Err("prefix is empty or whitespace only".to_string());
    }
    if crate::sansio::permissions::tokenize(&prefix).is_empty() {
        return Err("prefix tokenizes to zero tokens".to_string());
    }
    let readonly = optional_bool(value, "readonly")
        .map_err(|e| format!("readonly: {e}"))?
        .unwrap_or(false);
    let network = optional_bool(value, "network")
        .map_err(|e| format!("network: {e}"))?
        .unwrap_or(true);
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
        prefix,
        readonly,
        network,
        decision,
    })
}

fn required_string(value: nojson::RawJsonValue<'_, '_>, key: &str) -> Result<String, String> {
    let v = value
        .to_member(key)
        .and_then(|m| m.required())
        .map_err(|e| e.to_string())?;
    v.to_unquoted_string_str()
        .map(|s| s.into_owned())
        .map_err(|e| e.to_string())
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

fn optional_bool(value: nojson::RawJsonValue<'_, '_>, key: &str) -> Result<Option<bool>, String> {
    let m = value.to_member(key).map_err(|e| e.to_string())?;
    let Some(v) = m.optional() else {
        return Ok(None);
    };
    v.try_into()
        .map(Some)
        .map_err(|e: nojson::JsonParseError| e.to_string())
}

// -------------------------------------------------------------------
// grant (safe text-preserving append)
// -------------------------------------------------------------------

pub enum GrantOutcome {
    Appended(PathBuf),
    AlreadyGranted(PathBuf),
}

pub enum GrantError {
    PrefixEmpty,
    SessionMissing(PathBuf),
    ParseError(PathBuf, String),
    ExistingDenyConflict(PathBuf, String),
    ExistingAttributeOnlyConflict(PathBuf, String),
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
            Self::PrefixEmpty => write!(f, "PREFIX is empty or tokenizes to zero tokens"),
            Self::SessionMissing(p) => write!(f, "session directory not found: {}", p.display()),
            Self::ParseError(p, e) => write!(
                f,
                "existing {} is not valid JSONC: {e}. Fix or delete it before granting.",
                p.display()
            ),
            Self::ExistingDenyConflict(p, prefix) => write!(
                f,
                "rule for prefix {prefix:?} already exists as `deny` in {}; edit manually to resolve",
                p.display()
            ),
            Self::ExistingAttributeOnlyConflict(p, prefix) => write!(
                f,
                "rule for prefix {prefix:?} already exists in {} without a `decision` field; append `\"decision\": \"approve\"` to it manually so evaluation order is preserved",
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

pub fn grant(scope: GrantScope<'_>, prefix: &str) -> Result<GrantOutcome, GrantError> {
    let prefix = prefix.trim().to_string();
    if prefix.is_empty() || crate::sansio::permissions::tokenize(&prefix).is_empty() {
        return Err(GrantError::PrefixEmpty);
    }
    let target = match scope {
        GrantScope::Session(name) => {
            let paths = session_paths(name).map_err(GrantError::Io)?;
            if !paths.dir.try_exists()? {
                return Err(GrantError::SessionMissing(paths.dir.clone()));
            }
            session_permissions_path(&paths)
        }
        GrantScope::Workspace => {
            let root = session_root();
            if !root.try_exists()? {
                fs::create_dir_all(&root)?;
            }
            workspace_permissions_path()
        }
    };

    // Read existing content (missing file → empty array template).
    let (existing_text, is_new_file) = match fs::read_to_string(&target) {
        Ok(s) => (s, false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => ("[]".to_string(), true),
        Err(e) => return Err(GrantError::Io(e)),
    };

    // Validate + look for existing rule with same prefix.
    let json = RawJson::parse_jsonc(&existing_text)
        .map_err(|e| GrantError::ParseError(target.clone(), e.to_string()))?
        .0;
    let root = json.value();
    let array = root
        .to_array()
        .map_err(|e| GrantError::ParseError(target.clone(), e.to_string()))?;
    for item in array {
        let existing_prefix = match required_string(item, "prefix") {
            Ok(p) => p,
            Err(_) => continue, // Malformed rule; treat as absent.
        };
        if existing_prefix != prefix {
            continue;
        }
        // Same prefix: branch on decision.
        let decision = optional_string(item, "decision").ok().flatten();
        return match decision.as_deref() {
            Some("approve") => Ok(GrantOutcome::AlreadyGranted(target)),
            Some("deny") => Err(GrantError::ExistingDenyConflict(target, prefix)),
            _ => Err(GrantError::ExistingAttributeOnlyConflict(target, prefix)),
        };
    }

    // No conflict → append. Determine insertion position from the
    // parsed root array's byte range (avoids naive `]` scanning).
    let array_start = root.position();
    let array_raw = root.as_raw_str();
    let closing_bracket_relative = array_raw
        .rfind(']')
        .ok_or_else(|| GrantError::ParseError(target.clone(), "root has no closing ']'".into()))?;
    let insertion_point = array_start + closing_bracket_relative;

    // Look at the last non-whitespace / non-comment char before the `]`.
    let head = &existing_text[..insertion_point];
    let last_significant = last_significant_char(head);

    let new_rule_text = format!(
        "{{ \"prefix\": {}, \"decision\": \"approve\" }}",
        json_string_literal(&prefix)
    );
    let insertion = match last_significant {
        Some(',') | Some('[') => format!("\n  {new_rule_text}\n"),
        Some(_) => format!(",\n  {new_rule_text}\n"),
        None => format!("\n  {new_rule_text}\n"),
    };

    let new_content = if is_new_file {
        format!("[\n  {new_rule_text}\n]\n")
    } else {
        let mut buf = String::with_capacity(existing_text.len() + insertion.len());
        buf.push_str(&existing_text[..insertion_point]);
        buf.push_str(&insertion);
        buf.push_str(&existing_text[insertion_point..]);
        buf
    };

    atomic_write(&target, new_content.as_bytes())?;
    Ok(GrantOutcome::Appended(target))
}

/// Scan `head` from the end, skipping whitespace, `// ...\n` line
/// comments, and `/* ... */` block comments. Return the first
/// non-comment / non-whitespace character encountered (as a char),
/// or `None` if the entire head is whitespace/comments.
fn last_significant_char(head: &str) -> Option<char> {
    let bytes = head.as_bytes();
    let mut i = bytes.len();
    loop {
        if i == 0 {
            return None;
        }
        let c = bytes[i - 1];
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
            i -= 1;
            continue;
        }
        // Try to skip a `/* ... */` block comment ending at position i.
        if i >= 2 && bytes[i - 2] == b'*' && bytes[i - 1] == b'/' {
            let mut j = i - 2;
            while j >= 2 && !(bytes[j - 2] == b'/' && bytes[j - 1] == b'*') {
                j -= 1;
            }
            if j >= 2 {
                i = j - 2;
                continue;
            }
            return Some('/'); // Malformed but return the '/' anyway.
        }
        // Try to skip a `// ...` line comment ending on this line.
        // Walk back to line start and check for `//`.
        let mut line_start = i;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        if let Some(pos) = head[line_start..i].find("//") {
            i = line_start + pos;
            continue;
        }
        return Some(c as char);
    }
}

fn json_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
