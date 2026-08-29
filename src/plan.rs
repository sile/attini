//! Plan artifact parsing, rendering, validation, and sealing for the
//! `attini plan` command family.
//!
//! A plan is a UTF-8 Markdown file with a fixed layout:
//!
//! ```text
//! <natural-language Markdown body>
//!
//! <!-- ATTINI-ACTIONS:BEGIN -->
//! <fenced JSON actions block>
//! <!-- ATTINI-ACTIONS:END -->
//!
//! <!-- ATTINI-CONFIRMATIONS:BEGIN -->
//! <flat confirmation checkbox list>
//! <!-- ATTINI-CONFIRMATIONS-SHA256: <64hex> -->
//! <!-- ATTINI-CONFIRMATIONS:END -->
//! ```
//!
//! The seal is a SHA-256 over the raw UTF-8 bytes of the file with the
//! seal marker line removed, so any edit (including whitespace) breaks
//! the seal until `plan ok` re-approves it. Parsing, rendering, and
//! hashing here are pure and never touch the filesystem.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nojson::{DisplayJson, Json, RawJson};

use sha2::{Digest, Sha256};

/// Maximum file size of a plan artifact (bytes).
pub const PLAN_MAX_BYTES: usize = 64 * 1024;
/// Maximum number of confirmation items (including `all-ok`).
pub const MAX_CONFIRMATIONS: usize = 32;
/// Maximum number of patch or command actions.
pub const MAX_ACTIONS: usize = 128;
/// Reserved confirmation ID emitted by attini itself.
pub const RESERVED_CONFIRMATION_ID: &str = "all-ok";
/// Description rendered for the reserved `all-ok` confirmation.
pub const ALL_OK_DESCRIPTION: &str = "Approve all confirmation items";
/// Initial plan format version.
pub const PLAN_FORMAT_VERSION: u64 = 1;
/// Version of the serialised snapshot payload.
pub const PLAN_SNAPSHOT_VERSION: u64 = 1;

const ACTIONS_BEGIN: &str = "<!-- ATTINI-ACTIONS:BEGIN -->";
const ACTIONS_END: &str = "<!-- ATTINI-ACTIONS:END -->";
const CONFIRMATIONS_BEGIN: &str = "<!-- ATTINI-CONFIRMATIONS:BEGIN -->";
const CONFIRMATIONS_END: &str = "<!-- ATTINI-CONFIRMATIONS:END -->";
const SEAL_PREFIX: &str = "<!-- ATTINI-CONFIRMATIONS-SHA256:";
const SEAL_SUFFIX: &str = "-->";
const CONFIRM_PREFIX: &str = "ATTINI-CONFIRM:";

/// A sealed plan approved for execution in one invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedPlan {
    pub plan_sha256: String,
    pub actions: PlanActions,
    /// Absolute path of the content-addressed snapshot this
    /// authorization is derived from. Passed to child subagents so
    /// they validate the same snapshot instead of the live plan file.
    pub snapshot_path: PathBuf,
}

/// One approved patch action from a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPatchAction {
    pub id: String,
    /// Exact normalized workspace-relative path.
    pub path: String,
    pub description: String,
}

/// One approved command action from a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCommandAction {
    pub id: String,
    /// Exact argv, matched element-for-element.
    pub argv: Vec<String>,
    pub description: String,
}

/// The machine-checkable action manifest of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanActions {
    pub patches: Vec<PlanPatchAction>,
    pub commands: Vec<PlanCommandAction>,
}

impl PlanActions {
    /// Look up a patch action by exact path.
    pub fn patch_by_path(&self, path: &str) -> Option<&PlanPatchAction> {
        self.patches.iter().find(|a| a.path == path)
    }

    /// Look up a command action by exact argv.
    pub fn command_by_argv(&self, argv: &[String]) -> Option<&PlanCommandAction> {
        self.commands.iter().find(|a| a.argv == argv)
    }

    /// Validate the action manifest (IDs, counts, uniqueness, and
    /// required fields). Used by the renderer and by the `submit_plan`
    /// argument parser before rendering.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.patches.len() > MAX_ACTIONS {
            return Err(PlanError::TooManyActions {
                kind: "patch",
                count: self.patches.len(),
            });
        }
        if self.commands.len() > MAX_ACTIONS {
            return Err(PlanError::TooManyActions {
                kind: "command",
                count: self.commands.len(),
            });
        }
        let mut action_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for action in &self.patches {
            validate_id(&action.id, "action id")?;
            if !action_ids.insert(&action.id) {
                return Err(PlanError::DuplicateId(action.id.clone()));
            }
            if action.path.is_empty() {
                return Err(PlanError::EmptyField("patch path".to_string()));
            }
            if action.description.trim().is_empty() {
                return Err(PlanError::EmptyField("action description".to_string()));
            }
        }
        for action in &self.commands {
            validate_id(&action.id, "action id")?;
            if !action_ids.insert(&action.id) {
                return Err(PlanError::DuplicateId(action.id.clone()));
            }
            if action.argv.is_empty() {
                return Err(PlanError::EmptyField("command argv".to_string()));
            }
            if action.description.trim().is_empty() {
                return Err(PlanError::EmptyField("action description".to_string()));
            }
        }
        let mut paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for action in &self.patches {
            if !paths.insert(&action.path) {
                return Err(PlanError::DuplicatePatchPath(action.path.clone()));
            }
        }
        let mut argv_set: std::collections::HashSet<&Vec<String>> =
            std::collections::HashSet::new();
        for action in &self.commands {
            if !argv_set.insert(&action.argv) {
                return Err(PlanError::DuplicateCommandArgv(action.argv.join(" ")));
            }
        }
        Ok(())
    }
}

/// One confirmation item. `checked` reflects the current checkbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub id: String,
    pub description: String,
    pub checked: bool,
}

impl Confirmation {
    pub fn unchecked(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            checked: false,
        }
    }
}

/// A parsed and structurally validated plan file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPlan {
    /// Natural-language Markdown body (before the actions block).
    pub body_markdown: String,
    pub actions: PlanActions,
    pub confirmations: Vec<Confirmation>,
}

/// Errors raised while parsing, rendering, or sealing a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The file is not valid UTF-8.
    NotUtf8,
    /// The file exceeds [`PLAN_MAX_BYTES`].
    TooLarge {
        size: usize,
    },
    /// A required marker is missing or duplicated.
    MissingMarker(&'static str),
    DuplicateMarker(&'static str),
    /// The confirmation block is not at the end of the file.
    ContentAfterConfirmations,
    /// A forbidden marker appears in the Markdown body.
    MarkerInBody(&'static str),
    /// The fenced JSON actions block is missing or malformed.
    ActionsBlockMissing,
    /// The actions JSON failed to parse.
    ActionsJsonParse(String),
    /// The actions JSON is valid JSON but has a wrong shape.
    ActionsSchema(String),
    /// The plan format version is missing or unsupported.
    UnsupportedVersion(u64),
    /// A confirmation ID is not unique.
    DuplicateConfirmationId(String),
    /// A patch path is not unique.
    DuplicatePatchPath(String),
    /// A command argv is not unique.
    DuplicateCommandArgv(String),
    /// An action or confirmation ID is duplicated.
    DuplicateId(String),
    /// Too many confirmations.
    TooManyConfirmations {
        count: usize,
    },
    /// Too many actions of one kind.
    TooManyActions {
        kind: &'static str,
        count: usize,
    },
    /// An ID failed the `[a-z][a-z0-9-]{0,63}` rule.
    InvalidId(String),
    /// A required field is empty.
    EmptyField(String),
    /// `all-ok` must appear exactly once.
    MissingAllOk,
    DuplicateAllOk,
    /// A confirmation checkbox line is malformed.
    MalformedConfirmation(String),
    /// A confirmations item ID is unknown.
    UnknownConfirmationId(String),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUtf8 => f.write_str("plan is not valid UTF-8"),
            Self::TooLarge { size } => write!(
                f,
                "plan exceeds {PLAN_MAX_BYTES} bytes (actual {size} bytes)"
            ),
            Self::MissingMarker(m) => write!(f, "missing {m}"),
            Self::DuplicateMarker(m) => write!(f, "duplicate {m}"),
            Self::ContentAfterConfirmations => {
                f.write_str("content appears after the confirmations block")
            }
            Self::MarkerInBody(m) => write!(f, "marker {m} must not appear in the plan body"),
            Self::ActionsBlockMissing => f.write_str("actions fenced JSON block is missing"),
            Self::ActionsJsonParse(e) => write!(f, "actions JSON parse error: {e}"),
            Self::ActionsSchema(e) => write!(f, "actions JSON schema error: {e}"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "unsupported plan version {v} (only version {PLAN_FORMAT_VERSION} is supported)"
            ),
            Self::DuplicateConfirmationId(id) => {
                write!(f, "duplicate confirmation id {id:?}")
            }
            Self::DuplicatePatchPath(p) => write!(f, "duplicate patch path {p:?}"),
            Self::DuplicateCommandArgv(a) => write!(f, "duplicate command argv {a:?}"),
            Self::DuplicateId(id) => write!(f, "duplicate action id {id:?}"),
            Self::TooManyConfirmations { count } => write!(
                f,
                "too many confirmations ({count}; max {MAX_CONFIRMATIONS})"
            ),
            Self::TooManyActions { kind, count } => {
                write!(f, "too many {kind} actions ({count}; max {MAX_ACTIONS})")
            }
            Self::InvalidId(id) => {
                write!(f, "invalid id {id:?} (must match [a-z][a-z0-9-]{{0,63}})")
            }
            Self::EmptyField(field) => write!(f, "{field} must not be empty"),
            Self::MissingAllOk => {
                write!(f, "confirmation {RESERVED_CONFIRMATION_ID:?} is required")
            }
            Self::DuplicateAllOk => write!(
                f,
                "confirmation {RESERVED_CONFIRMATION_ID:?} must appear exactly once"
            ),
            Self::MalformedConfirmation(line) => {
                write!(f, "malformed confirmation line: {line:?}")
            }
            Self::UnknownConfirmationId(id) => {
                write!(f, "unknown confirmation id {id:?}")
            }
        }
    }
}

/// Compute the SHA-256 of `raw` with the seal marker line removed.
/// Returns the 64-char lowercase hex digest.
pub fn seal_hash(raw: &[u8]) -> Result<String, PlanError> {
    let text = std::str::from_utf8(raw).map_err(|_| PlanError::NotUtf8)?;
    let without_marker = remove_seal_line(text);
    let digest = Sha256::digest(without_marker.as_bytes());
    Ok(hex(digest.as_slice()))
}

fn remove_seal_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if is_seal_line(line.trim_end_matches(['\n', '\r'])) {
            continue;
        }
        out.push_str(line);
    }
    out
}

fn is_seal_line(line: &str) -> bool {
    line.starts_with(SEAL_PREFIX)
}

/// Validate and parse a plan file's text. Structural rules from the
/// design: exactly one actions and one confirmations block, the
/// confirmation block at EOF, strict JSON actions, bounded counts,
/// and valid IDs. The seal is not verified here (see [`seal_hash`]).
pub fn parse(text: &str) -> Result<ParsedPlan, PlanError> {
    let raw = text.as_bytes();
    if raw.len() > PLAN_MAX_BYTES {
        return Err(PlanError::TooLarge { size: raw.len() });
    }

    let actions_begin = find_marker(text, ACTIONS_BEGIN)?;
    let actions_end = find_marker(text, ACTIONS_END)?;
    if actions_begin >= actions_end {
        return Err(PlanError::ActionsBlockMissing);
    }

    let body = &text[..actions_begin];
    for marker in [
        ACTIONS_BEGIN,
        ACTIONS_END,
        CONFIRMATIONS_BEGIN,
        CONFIRMATIONS_END,
        SEAL_PREFIX,
    ] {
        if body.contains(marker) {
            return Err(PlanError::MarkerInBody(marker));
        }
    }

    let actions_block = &text[actions_begin + ACTIONS_BEGIN.len()..actions_end];
    let actions_json = extract_fenced_json(actions_block).ok_or(PlanError::ActionsBlockMissing)?;
    let actions = parse_actions_json(actions_json)?;

    let confirmations_begin = find_marker(text, CONFIRMATIONS_BEGIN)?;
    let confirmations_end = find_marker(text, CONFIRMATIONS_END)?;
    if confirmations_begin <= actions_end {
        return Err(PlanError::MissingMarker(CONFIRMATIONS_BEGIN));
    }
    let after = &text[confirmations_end + CONFIRMATIONS_END.len()..];
    if !after.trim().is_empty() {
        return Err(PlanError::ContentAfterConfirmations);
    }

    let confirmations_block =
        &text[confirmations_begin + CONFIRMATIONS_BEGIN.len()..confirmations_end];
    let confirmations = parse_confirmations(confirmations_block)?;

    Ok(ParsedPlan {
        body_markdown: body.trim().to_string(),
        actions,
        confirmations,
    })
}

/// Render a plan file from its components and seal it. The seal
/// marker line is inserted with the hash of everything else.
pub fn render_sealed(
    body: &str,
    actions: &PlanActions,
    confirmations: &[Confirmation],
) -> Result<String, PlanError> {
    actions.validate()?;
    let unsealed = render_unsealed(body, actions, confirmations)?;
    let hash = seal_hash(unsealed.as_bytes())?;
    Ok(replace_or_insert_seal(unsealed, &hash))
}

/// Render the plan text without a seal marker line.
fn render_unsealed(
    body: &str,
    actions: &PlanActions,
    confirmations: &[Confirmation],
) -> Result<String, PlanError> {
    let mut out = String::new();
    out.push_str(body.trim());
    out.push_str("\n\n");
    out.push_str(ACTIONS_BEGIN);
    out.push_str("\n\n## Attini approved actions\n\n```json\n");
    let _ = writeln!(out, "{}", Json(PlanActionsJson(actions)));
    out.push_str("```\n\n");
    out.push_str(ACTIONS_END);
    out.push_str("\n\n");
    out.push_str(CONFIRMATIONS_BEGIN);
    out.push_str("\n\n## Attini confirmations\n\n");
    for c in confirmations {
        let mark = if c.checked { "x" } else { " " };
        out.push_str(&format!(
            "- [{mark}] {CONFIRM_PREFIX} {} — {}\n",
            c.id, c.description
        ));
    }
    out.push('\n');
    out.push_str(SEAL_PREFIX);
    out.push(' ');
    out.push_str(&"0".repeat(64));
    out.push(' ');
    out.push_str(SEAL_SUFFIX);
    out.push('\n');
    out.push_str(CONFIRMATIONS_END);
    Ok(out)
}

fn replace_or_insert_seal(text: String, hash: &str) -> String {
    let seal_line = format!("{SEAL_PREFIX} {hash} {SEAL_SUFFIX}");
    let mut found = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if is_seal_line(line.trim_end_matches(['\n', '\r'])) {
            if found {
                out.push_str(line);
            } else {
                found = true;
                out.push_str(&seal_line);
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    if !found {
        out.push_str(&seal_line);
        out.push('\n');
    }
    out
}

/// Verify a plan's stored seal against its current bytes. Returns
/// `Ok(true)` when the seal is present and matches, `Ok(false)` when
/// it is absent or mismatched.
pub fn verify_seal(raw: &str) -> Result<bool, PlanError> {
    let stored = extract_stored_seal(raw);
    let computed = seal_hash(raw.as_bytes())?;
    Ok(stored.as_deref() == Some(computed.as_str()))
}

fn extract_stored_seal(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(SEAL_PREFIX) {
            let rest = rest.trim();
            let hex = rest
                .strip_suffix(SEAL_SUFFIX)
                .map(str::trim)
                .unwrap_or(rest);
            if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(hex.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Apply the confirmations of `plan_ok`: when `--all` is set, check
/// `all-ok` and uncheck every other item; when specific IDs are
/// given, check those and uncheck `all-ok`; when neither is given,
/// keep the current checkbox state. Unknown IDs are rejected before
/// any mutation.
pub fn apply_confirmations(
    plan: &ParsedPlan,
    all: bool,
    item_ids: &[String],
) -> Result<Vec<Confirmation>, PlanError> {
    if all {
        return Ok(plan
            .confirmations
            .iter()
            .map(|c| Confirmation {
                checked: c.id == RESERVED_CONFIRMATION_ID,
                ..c.clone()
            })
            .collect());
    }
    if item_ids.is_empty() {
        return Ok(plan.confirmations.clone());
    }
    let known: std::collections::HashSet<&str> =
        plan.confirmations.iter().map(|c| c.id.as_str()).collect();
    for id in item_ids {
        if *id == RESERVED_CONFIRMATION_ID || !known.contains(id.as_str()) {
            return Err(PlanError::UnknownConfirmationId(id.clone()));
        }
    }
    Ok(plan
        .confirmations
        .iter()
        .map(|c| Confirmation {
            checked: item_ids.iter().any(|id| id == &c.id),
            ..c.clone()
        })
        .collect())
}

/// Whether the confirmations are complete: `all-ok` checked, or every
/// individual item checked.
pub fn confirmations_complete(confirmations: &[Confirmation]) -> bool {
    let all_ok = confirmations
        .iter()
        .find(|c| c.id == RESERVED_CONFIRMATION_ID);
    match all_ok {
        Some(c) if c.checked => true,
        _ => confirmations
            .iter()
            .filter(|c| c.id != RESERVED_CONFIRMATION_ID)
            .all(|c| c.checked),
    }
}

fn find_marker(text: &str, marker: &'static str) -> Result<usize, PlanError> {
    let mut first: Option<usize> = None;
    let mut count = 0;
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find(marker) {
        count += 1;
        if first.is_none() {
            first = Some(search_from + idx);
        }
        search_from += idx + marker.len();
    }
    match (first, count) {
        (Some(idx), 1) => Ok(idx),
        (None, _) => Err(PlanError::MissingMarker(marker)),
        (_, _) => Err(PlanError::DuplicateMarker(marker)),
    }
}

fn extract_fenced_json(block: &str) -> Option<&str> {
    let start = block.find("```json")?;
    let content_start = start + "```json".len();
    let rest = &block[content_start..];
    let end = rest.find("```")?;
    Some(rest[..end].trim())
}

fn parse_actions_json(json: &str) -> Result<PlanActions, PlanError> {
    let parsed = RawJson::parse(json).map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?;
    let root = parsed.value();
    let version: u64 = root
        .to_member("version")
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
        .required()
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
        .try_into()
        .map_err(|e: nojson::JsonParseError| PlanError::ActionsJsonParse(e.to_string()))?;
    if version != PLAN_FORMAT_VERSION {
        return Err(PlanError::UnsupportedVersion(version));
    }

    let mut patches = Vec::new();
    let patches_value = root
        .to_member("patches")
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
        .optional();
    if let Some(arr) = patches_value {
        for item in arr
            .to_array()
            .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
        {
            let id = read_action_field(item, "id")?;
            let path = read_action_field(item, "path")?;
            let description = read_action_field(item, "description")?;
            patches.push(PlanPatchAction {
                id,
                path,
                description,
            });
        }
    }

    let mut commands = Vec::new();
    let commands_value = root
        .to_member("commands")
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
        .optional();
    if let Some(arr) = commands_value {
        for item in arr
            .to_array()
            .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
        {
            let id = read_action_field(item, "id")?;
            let description = read_action_field(item, "description")?;
            let argv = read_argv(item)?;
            commands.push(PlanCommandAction {
                id,
                argv,
                description,
            });
        }
    }

    let actions = PlanActions { patches, commands };
    actions.validate()?;
    Ok(actions)
}

fn read_action_field(item: nojson::RawJsonValue<'_, '_>, field: &str) -> Result<String, PlanError> {
    item.to_member(field)
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
        .required()
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
        .to_unquoted_string_str()
        .map(|s| s.into_owned())
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))
}

fn read_argv(item: nojson::RawJsonValue<'_, '_>) -> Result<Vec<String>, PlanError> {
    let value = item
        .to_member("argv")
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
        .required()
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))?;
    let mut out = Vec::new();
    for element in value
        .to_array()
        .map_err(|e| PlanError::ActionsSchema(e.to_string()))?
    {
        let s = element
            .to_unquoted_string_str()
            .map(|s| s.into_owned())
            .map_err(|e| PlanError::ActionsSchema(e.to_string()))?;
        out.push(s);
    }
    Ok(out)
}

fn parse_confirmations(block: &str) -> Result<Vec<Confirmation>, PlanError> {
    let mut confirmations = Vec::new();
    let mut seen_all_ok = false;
    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_seal_line(trimmed) {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("- [") else {
            continue;
        };
        let (mark, rest) = match rest.chars().next() {
            Some(' ') => (false, &rest[1..]),
            Some('x') | Some('X') => (true, &rest[1..]),
            _ => continue,
        };
        let Some(rest) = rest.strip_prefix("] ") else {
            continue;
        };
        let Some(body) = rest.strip_prefix(CONFIRM_PREFIX) else {
            continue;
        };
        let body = body.trim();
        let (id, description) = match body.split_once("—") {
            Some((id, desc)) => (id.trim().to_string(), desc.trim().to_string()),
            None => (body.trim().to_string(), String::new()),
        };
        validate_id(&id, "confirmation id")?;
        if id == RESERVED_CONFIRMATION_ID {
            if seen_all_ok {
                return Err(PlanError::DuplicateAllOk);
            }
            seen_all_ok = true;
        }
        if confirmations.len() >= MAX_CONFIRMATIONS {
            return Err(PlanError::TooManyConfirmations {
                count: confirmations.len() + 1,
            });
        }
        confirmations.push(Confirmation {
            id,
            description,
            checked: mark,
        });
    }
    if !seen_all_ok {
        return Err(PlanError::MissingAllOk);
    }
    let mut ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for c in &confirmations {
        if !ids.insert(c.id.as_str()) {
            return Err(PlanError::DuplicateConfirmationId(c.id.clone()));
        }
    }
    Ok(confirmations)
}

fn validate_id(id: &str, label: &str) -> Result<(), PlanError> {
    let mut chars = id.chars();
    let mut count = 0usize;
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => count += 1,
        _ => return Err(PlanError::InvalidId(format!("{label} {id:?}"))),
    }
    for c in chars {
        count += 1;
        if count > 64 || !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(PlanError::InvalidId(format!("{label} {id:?}")));
        }
    }
    Ok(())
}

/// Public ID validation for the `submit_plan` argument parser.
pub fn validate_plan_id(id: &str) -> Result<(), PlanError> {
    validate_id(id, "id")
}

/// `YYYYMMDD-HHMMSS-<6hex>` timestamp suffix for managed plan names.
/// Pure UTC breakdown of the current Unix time; the hex part comes
/// from subsecond nanoseconds so rapid successive calls differ.
pub fn plan_timestamp_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    let hex = now.subsec_nanos() & 0x00FF_FFFF;
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{hex:06x}")
}

fn unix_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let day = secs / 86_400;
    let time_of_day = (secs % 86_400) as u32;
    let h = time_of_day / 3600;
    let mi = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    let day = day as i64 + 719_468;
    let era = if day >= 0 { day } else { day - 146_096 } / 146_097;
    let doe = (day - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = y + if m <= 2 { 1 } else { 0 };
    (y as u32, m, d, h, mi, s)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// -------------------------------------------------------------------
// JSON serialisation of the actions block and the plan snapshot
// -------------------------------------------------------------------

struct PlanActionsJson<'a>(&'a PlanActions);

impl DisplayJson for PlanActionsJson<'_> {
    fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("version", PLAN_FORMAT_VERSION)?;
            f.member(
                "patches",
                self.0
                    .patches
                    .iter()
                    .map(PlanPatchJson)
                    .collect::<Vec<_>>()
                    .as_slice(),
            )?;
            f.member(
                "commands",
                self.0
                    .commands
                    .iter()
                    .map(PlanCommandJson)
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
        })
    }
}

struct PlanPatchJson<'a>(&'a PlanPatchAction);
struct PlanCommandJson<'a>(&'a PlanCommandAction);

impl DisplayJson for PlanPatchJson<'_> {
    fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("id", &self.0.id)?;
            f.member("path", &self.0.path)?;
            f.member("description", &self.0.description)
        })
    }
}

impl DisplayJson for PlanCommandJson<'_> {
    fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("id", &self.0.id)?;
            f.member("argv", &self.0.argv)?;
            f.member("description", &self.0.description)
        })
    }
}

/// Serialised content-addressed snapshot payload stored under the run
/// session's `plan-runs/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSnapshot {
    pub snapshot_version: u64,
    pub plan_format_version: u64,
    pub plan_sha256: String,
    pub canonical_path: String,
    pub body_markdown: String,
    pub actions: PlanActions,
    pub confirmations: Vec<Confirmation>,
}

impl PlanSnapshot {
    pub fn to_json(&self) -> String {
        Json(PlanSnapshotJson(self)).to_string()
    }

    /// Build the invocation authorization this snapshot represents.
    pub fn into_approved_plan(self) -> ApprovedPlan {
        ApprovedPlan {
            plan_sha256: self.plan_sha256,
            actions: self.actions,
            snapshot_path: PathBuf::new(),
        }
    }
}

/// Parse a serialised snapshot JSON document.
pub fn parse_snapshot(text: &str) -> Result<PlanSnapshot, PlanError> {
    let json = RawJson::parse(text).map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?;
    let root = json.value();
    let read_u64 = |name: &str| -> Result<u64, PlanError> {
        root.to_member(name)
            .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
            .required()
            .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
            .try_into()
            .map_err(|e: nojson::JsonParseError| PlanError::ActionsJsonParse(e.to_string()))
    };
    let read_string = |name: &str| -> Result<String, PlanError> {
        root.to_member(name)
            .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
            .required()
            .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
            .to_unquoted_string_str()
            .map(|s| s.into_owned())
            .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))
    };
    let snapshot_version = read_u64("snapshot_version")?;
    let plan_format_version = read_u64("plan_format_version")?;
    if snapshot_version != PLAN_SNAPSHOT_VERSION {
        return Err(PlanError::UnsupportedVersion(snapshot_version));
    }
    if plan_format_version != PLAN_FORMAT_VERSION {
        return Err(PlanError::UnsupportedVersion(plan_format_version));
    }
    let plan_sha256 = read_string("plan_sha256")?;
    let canonical_path = read_string("canonical_path")?;
    let body_markdown = read_string("body_markdown")?;

    let actions_raw = root
        .to_member("actions")
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
        .required()
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?;
    let actions = parse_actions_json(actions_raw.as_raw_str())?;

    let mut confirmations = Vec::new();
    if let Some(arr) = root
        .to_member("confirmations")
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
        .optional()
    {
        for item in arr
            .to_array()
            .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
        {
            let id = read_member_string(item, "id")?;
            let description = read_member_string(item, "description")?;
            let checked: bool = item
                .to_member("checked")
                .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
                .required()
                .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
                .try_into()
                .map_err(|e: nojson::JsonParseError| PlanError::ActionsJsonParse(e.to_string()))?;
            confirmations.push(Confirmation {
                id,
                description,
                checked,
            });
        }
    }

    Ok(PlanSnapshot {
        snapshot_version,
        plan_format_version,
        plan_sha256,
        canonical_path,
        body_markdown,
        actions,
        confirmations,
    })
}

fn read_member_string(item: nojson::RawJsonValue<'_, '_>, name: &str) -> Result<String, PlanError> {
    item.to_member(name)
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
        .required()
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))?
        .to_unquoted_string_str()
        .map(|s| s.into_owned())
        .map_err(|e| PlanError::ActionsJsonParse(e.to_string()))
}

struct PlanSnapshotJson<'a>(&'a PlanSnapshot);

impl DisplayJson for PlanSnapshotJson<'_> {
    fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("snapshot_version", self.0.snapshot_version)?;
            f.member("plan_format_version", self.0.plan_format_version)?;
            f.member("plan_sha256", &self.0.plan_sha256)?;
            f.member("canonical_path", &self.0.canonical_path)?;
            f.member("body_markdown", &self.0.body_markdown)?;
            f.member("actions", PlanActionsJson(&self.0.actions))?;
            f.member(
                "confirmations",
                self.0
                    .confirmations
                    .iter()
                    .map(ConfirmationJson)
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
        })
    }
}

struct ConfirmationJson<'a>(&'a Confirmation);

impl DisplayJson for ConfirmationJson<'_> {
    fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("id", &self.0.id)?;
            f.member("description", &self.0.description)?;
            f.member("checked", self.0.checked)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_actions() -> PlanActions {
        PlanActions {
            patches: vec![PlanPatchAction {
                id: "update-parser".to_string(),
                path: "src/parser.rs".to_string(),
                description: "Update the parser".to_string(),
            }],
            commands: vec![PlanCommandAction {
                id: "run-tests".to_string(),
                argv: vec!["cargo".to_string(), "test".to_string()],
                description: "Run the test suite".to_string(),
            }],
        }
    }

    fn sample_confirmations() -> Vec<Confirmation> {
        vec![
            Confirmation::unchecked(RESERVED_CONFIRMATION_ID, ALL_OK_DESCRIPTION),
            Confirmation::unchecked("public-api", "Allow public API changes"),
        ]
    }

    #[test]
    fn seal_hash_ignores_seal_marker_line() {
        let plan =
            render_sealed("# Plan\n\nbody", &sample_actions(), &sample_confirmations()).unwrap();
        let hash = seal_hash(plan.as_bytes()).unwrap();
        assert_eq!(hash.len(), 64);
        assert!(verify_seal(&plan).unwrap());
    }

    #[test]
    fn seal_breaks_on_any_edit() {
        let plan =
            render_sealed("# Plan\n\nbody", &sample_actions(), &sample_confirmations()).unwrap();
        let edited = plan.replace("body", "body edited");
        assert!(!verify_seal(&edited).unwrap());
    }

    #[test]
    fn render_parse_roundtrip() {
        let text = render_sealed(
            "# Plan\n\nSome body text.",
            &sample_actions(),
            &sample_confirmations(),
        )
        .unwrap();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.body_markdown, "# Plan\n\nSome body text.");
        assert_eq!(parsed.actions, sample_actions());
        assert_eq!(parsed.confirmations, sample_confirmations());
    }

    #[test]
    fn parse_rejects_marker_in_body() {
        let mut text =
            render_sealed("# Plan\n\nbody", &sample_actions(), &sample_confirmations()).unwrap();
        text = text.replace("body", &format!("body {CONFIRMATIONS_BEGIN}"));
        assert!(matches!(parse(&text), Err(PlanError::MarkerInBody(_))));
    }

    #[test]
    fn parse_rejects_unknown_version() {
        let mut text =
            render_sealed("# Plan\n\nbody", &sample_actions(), &sample_confirmations()).unwrap();
        text = text.replace("\"version\":1", "\"version\":2");
        assert!(matches!(
            parse(&text),
            Err(PlanError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn parse_rejects_invalid_id() {
        let actions = PlanActions {
            patches: vec![PlanPatchAction {
                id: "Bad-ID".to_string(),
                path: "src/x.rs".to_string(),
                description: "bad id".to_string(),
            }],
            ..PlanActions::default()
        };
        // Rendering validates the id, so the error surfaces there.
        assert!(matches!(
            render_sealed("# Plan", &actions, &sample_confirmations()),
            Err(PlanError::InvalidId(_))
        ));
    }

    #[test]
    fn apply_confirmations_all_checks_all_ok_only() {
        let plan = ParsedPlan {
            body_markdown: String::new(),
            actions: PlanActions::default(),
            confirmations: sample_confirmations(),
        };
        let out = apply_confirmations(&plan, true, &[]).unwrap();
        assert!(out.iter().find(|c| c.id == "all-ok").unwrap().checked);
        assert!(!out.iter().find(|c| c.id == "public-api").unwrap().checked);
    }

    #[test]
    fn apply_confirmations_item_ids_checks_those_only() {
        let plan = ParsedPlan {
            body_markdown: String::new(),
            actions: PlanActions::default(),
            confirmations: sample_confirmations(),
        };
        let out = apply_confirmations(&plan, false, &["public-api".to_string()]).unwrap();
        assert!(!out.iter().find(|c| c.id == "all-ok").unwrap().checked);
        assert!(out.iter().find(|c| c.id == "public-api").unwrap().checked);
    }

    #[test]
    fn apply_confirmations_unknown_id_errors() {
        let plan = ParsedPlan {
            body_markdown: String::new(),
            actions: PlanActions::default(),
            confirmations: sample_confirmations(),
        };
        assert!(matches!(
            apply_confirmations(&plan, false, &["nope".to_string()]),
            Err(PlanError::UnknownConfirmationId(_))
        ));
    }

    #[test]
    fn confirmations_complete_rules() {
        let all_ok_unchecked = vec![
            Confirmation::unchecked("all-ok", ALL_OK_DESCRIPTION),
            Confirmation::unchecked("a", "A"),
        ];
        assert!(!confirmations_complete(&all_ok_unchecked));
        let all_ok_checked = vec![
            Confirmation {
                id: "all-ok".into(),
                description: ALL_OK_DESCRIPTION.into(),
                checked: true,
            },
            Confirmation::unchecked("a", "A"),
        ];
        assert!(confirmations_complete(&all_ok_checked));
        let individual = vec![
            Confirmation::unchecked("all-ok", ALL_OK_DESCRIPTION),
            Confirmation {
                id: "a".into(),
                description: "A".into(),
                checked: true,
            },
        ];
        assert!(confirmations_complete(&individual));
        let none_checked = vec![
            Confirmation::unchecked("all-ok", ALL_OK_DESCRIPTION),
            Confirmation::unchecked("a", "A"),
        ];
        assert!(!confirmations_complete(&none_checked));
    }

    #[test]
    fn id_validation_bounds() {
        assert!(validate_id("a", "id").is_ok());
        assert!(validate_id("abc-123", "id").is_ok());
        assert!(validate_id("A", "id").is_err());
        assert!(validate_id("1abc", "id").is_err());
        assert!(validate_id("a_", "id").is_err());
        assert!(validate_id("", "id").is_err());
        let long = format!("a{}", "b".repeat(63));
        assert!(validate_id(&long, "id").is_ok());
        let too_long = format!("a{}", "b".repeat(64));
        assert!(validate_id(&too_long, "id").is_err());
    }

    #[test]
    fn snapshot_json_roundtrips_fields() {
        let plan_sha256 = "ab".repeat(32);
        let snapshot = PlanSnapshot {
            snapshot_version: PLAN_SNAPSHOT_VERSION,
            plan_format_version: PLAN_FORMAT_VERSION,
            plan_sha256: plan_sha256.clone(),
            canonical_path: "plans/plan-x.md".to_string(),
            body_markdown: "# body".to_string(),
            actions: sample_actions(),
            confirmations: sample_confirmations(),
        };
        let json = snapshot.to_json();
        assert!(json.contains("\"snapshot_version\":1"));
        assert!(json.contains(&format!("\"plan_sha256\":\"{plan_sha256}\"")));
        assert!(json.contains("\"canonical_path\":\"plans/plan-x.md\""));
    }
}
