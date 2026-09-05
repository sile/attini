//! Sans I/O agent core.
//!
//! [`AgentCore`] owns the conversation, the in-flight request, and the
//! transient display state; the surrounding I/O shell feeds it
//! [`Event`]s and executes the returned [`Action`]s. No transport,
//! terminal, filesystem, or subprocess is touched here.
//!
//! At most one model request is active at a time. Any events tagged
//! with a [`RequestId`] that is not the current one are silently
//! dropped, which lets the shell forward late arrivals from a
//! cancelled or completed request without corrupting state.
//!
//! Read-only tool calls are supported through an explicit tool loop
//! (see [`ReadOnlyTool`], `ToolRunning` phase,
//! [`Event::ToolCallDelta`], [`Event::ToolResult`]). File-editing and
//! command-execution tools, automatic retry, side-effect approvals,
//! and cross-turn reasoning carry-over remain out of scope.

use std::collections::BTreeMap;

use nojson::{Json, RawJsonValue};

use crate::metrics::Counter;
use crate::sansio::deepseek::{ChatMessage, ToolCall, ToolDef};

/// Maximum bytes of tool-call arguments (accumulated across streaming
/// fragments) the core will accept for a single tool call. Fragments
/// past this limit are dropped and the call is resolved to
/// `Err(ArgumentsTooLarge)` instead of being executed.
pub const ARGUMENTS_MAX_BYTES: usize = 64 * 1024;

/// Maximum number of tool calls the core will emit per user turn.
/// A "turn" spans [`Event::UserMessage`] acceptance to a return to
/// [`Status::Idle`], across any number of tool-loop iterations.
pub const TURN_TOOL_CALL_LIMIT: usize = 20;

/// Default upper bound on entries returned by [`ReadOnlyTool::List`].
pub const DEFAULT_LIST_MAX_ENTRIES: usize = 200;

/// Default upper bound on results returned by [`ReadOnlyTool::Search`].
pub const DEFAULT_SEARCH_MAX_RESULTS: usize = 50;

/// Maximum bytes read from a single file by [`ReadOnlyTool::Read`].
pub const READ_MAX_BYTES: usize = 1024 * 1024;

/// Maximum edits allowed in a single [`PatchInvocation`]. The 2-phase
/// applier assumes each edit maps to a unique target path, so the
/// upper bound doubles as an implicit cap on distinct target files
/// per call.
pub const PATCH_MAX_EDITS: usize = 20;

/// Maximum bytes for either the `content` of a [`PatchTool::Add`] or
/// the file targeted by a [`PatchTool::Update`]. Aligned with
/// [`READ_MAX_BYTES`] so the model cannot patch a file it cannot
/// read.
pub const PATCH_MAX_FILE_BYTES: usize = READ_MAX_BYTES;

/// Maximum bytes retained from either stdout or stderr of a running
/// command. Reaching this limit terminates the process group and
/// marks the tool result as `truncated`.
pub const COMMAND_MAX_STREAM_BYTES: usize = 256 * 1024;

/// Chunk size for a single non-blocking read on the child's stdout /
/// stderr pipe. Small enough to keep the TUI tail buffer responsive
/// under high-throughput output.
pub const COMMAND_STREAM_CHUNK_SIZE: usize = 4 * 1024;

/// A read-only tool the model can invoke while the agent is running.
///
/// Semantics and per-tool limits are defined in `src/tools.rs`; this
/// enum is only the Sans I/O contract that pairs a serialised
/// invocation (from the model) with an executor-supplied outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyTool {
    List {
        path: String,
        recursive: bool,
        max_entries: usize,
        include_hidden: bool,
    },
    Read {
        path: String,
        line_range: Option<(usize, usize)>,
    },
    Search {
        pattern: String,
        path_prefix: Option<String>,
        case_sensitive: bool,
        max_results: usize,
    },
}

impl ReadOnlyTool {
    /// OpenAI-compatible tool schemas advertised to the model as part
    /// of every [`crate::sansio::deepseek::ChatRequest`]. The enum is
    /// the source of truth; these definitions describe the wire shape
    /// the model must produce for a valid [`ToolCall`].
    pub fn definitions() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "list".to_string(),
                description: "List files and directories under a workspace-relative path. \
                     Returns a JSON array of {path, kind, size} entries; the \
                     result includes truncated:true when max_entries is hit."
                    .to_string(),
                parameters_json: LIST_PARAMS_SCHEMA.to_string(),
            },
            ToolDef {
                name: "read".to_string(),
                description: "Read a UTF-8 text file at a workspace-relative path. \
                     Optionally restrict to a 1-indexed inclusive [start, end] \
                     line range. Content is truncated to the first 1 MiB."
                    .to_string(),
                parameters_json: READ_PARAMS_SCHEMA.to_string(),
            },
            ToolDef {
                name: "search".to_string(),
                description: "Literal substring search across text files under an \
                     optional workspace-relative prefix. Returns \
                     {path, line, snippet} hits; binary or non-UTF-8 files \
                     are silently skipped."
                    .to_string(),
                parameters_json: SEARCH_PARAMS_SCHEMA.to_string(),
            },
        ]
    }

    /// Deserialise a model-supplied `function_name` + `arguments_json`
    /// pair into a concrete invocation.
    pub fn parse(function_name: &str, arguments_json: &str) -> Result<Self, ToolExecutionError> {
        match function_name {
            "list" => parse_list(arguments_json),
            "read" => parse_read(arguments_json),
            "search" => parse_search(arguments_json),
            _ => Err(ToolExecutionError::UnknownTool),
        }
    }
}

const LIST_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"path":{"type":"string","description":"Workspace-relative directory path (e.g. \".\" or \"src\")."},
"recursive":{"type":"boolean","default":false},
"max_entries":{"type":"integer","default":200,"minimum":1},
"include_hidden":{"type":"boolean","default":false}
},
"required":["path"]
}"#;

const READ_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"path":{"type":"string","description":"Workspace-relative file path."},
"line_range":{"type":"array","items":{"type":"integer","minimum":1},"minItems":2,"maxItems":2,"description":"1-indexed inclusive [start, end] range."}
},
"required":["path"]
}"#;

const SEARCH_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"pattern":{"type":"string","description":"Literal substring to match (no regex)."},
"path_prefix":{"type":"string","description":"Restrict to a workspace-relative subtree."},
"case_sensitive":{"type":"boolean","default":false},
"max_results":{"type":"integer","default":50,"minimum":1}
},
"required":["pattern"]
}"#;

fn parse_list(arguments_json: &str) -> Result<ReadOnlyTool, ToolExecutionError> {
    let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
    let root = json.value();
    let path = required_string(root, "path")?;
    let recursive = optional_bool(root, "recursive")?.unwrap_or(false);
    let max_entries = optional_usize(root, "max_entries")?.unwrap_or(DEFAULT_LIST_MAX_ENTRIES);
    let include_hidden = optional_bool(root, "include_hidden")?.unwrap_or(false);
    Ok(ReadOnlyTool::List {
        path,
        recursive,
        max_entries,
        include_hidden,
    })
}

fn parse_read(arguments_json: &str) -> Result<ReadOnlyTool, ToolExecutionError> {
    let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
    let root = json.value();
    let path = required_string(root, "path")?;
    let line_range = match root
        .to_member("line_range")
        .map_err(map_parse_err)?
        .optional()
    {
        None => None,
        Some(value) => {
            let mut iter = value.to_array().map_err(map_parse_err)?;
            let start = next_usize(&mut iter, "line_range")?;
            let end = next_usize(&mut iter, "line_range")?;
            if iter.next().is_some() {
                return Err(ToolExecutionError::ArgumentsParseFailed(
                    "line_range must have exactly 2 elements".to_string(),
                ));
            }
            Some((start, end))
        }
    };
    Ok(ReadOnlyTool::Read { path, line_range })
}

fn parse_search(arguments_json: &str) -> Result<ReadOnlyTool, ToolExecutionError> {
    let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
    let root = json.value();
    let pattern = required_string(root, "pattern")?;
    let path_prefix = optional_string(root, "path_prefix")?;
    let case_sensitive = optional_bool(root, "case_sensitive")?.unwrap_or(false);
    let max_results = optional_usize(root, "max_results")?.unwrap_or(DEFAULT_SEARCH_MAX_RESULTS);
    Ok(ReadOnlyTool::Search {
        pattern,
        path_prefix,
        case_sensitive,
        max_results,
    })
}

fn map_parse_err(err: nojson::JsonParseError) -> ToolExecutionError {
    ToolExecutionError::ArgumentsParseFailed(err.to_string())
}

fn next_usize<'text, 'raw, I>(iter: &mut I, field: &str) -> Result<usize, ToolExecutionError>
where
    I: Iterator<Item = RawJsonValue<'text, 'raw>>,
{
    iter.next()
        .ok_or_else(|| {
            ToolExecutionError::ArgumentsParseFailed(format!(
                "{field} must have exactly 2 elements"
            ))
        })?
        .try_into()
        .map_err(map_parse_err)
}

/// A single edit within a [`PatchInvocation`]. The 2-phase applier
/// requires each edit's target `path` to be unique inside the
/// containing invocation (see [`PatchInvocation::parse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchTool {
    /// Create a new file. Rejected if the target path already exists
    /// at apply time.
    Add { path: String, content: String },
    /// Replace the unique byte-for-byte occurrence of `before` inside
    /// the target file with `after`. Rejected if `before` matches
    /// zero times or more than once.
    Update {
        path: String,
        before: String,
        after: String,
    },
}

impl PatchTool {
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Update { path, .. } => path,
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::Update { .. } => "update",
        }
    }
}

/// A batch of [`PatchTool`] edits produced from a single `patch`
/// tool call. Parsed from the model-supplied JSON `arguments`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchInvocation {
    pub edits: Vec<PatchTool>,
}

impl PatchInvocation {
    /// Wire-format definition advertised to the model as one of the
    /// tools available to it in every [`crate::sansio::deepseek::ChatRequest`].
    pub fn definition() -> ToolDef {
        ToolDef {
            name: "patch".to_string(),
            description: "Apply a batch of file edits to the workspace. \
                          Each edit is either an add (create a new file) or \
                          an update (replace a unique substring). All edits \
                          in one call must target distinct paths. Every \
                          patch requires user approval before it touches \
                          the filesystem."
                .to_string(),
            parameters_json: PATCH_PARAMS_SCHEMA.to_string(),
        }
    }

    /// Parse the JSON `arguments` supplied by the model. Enforces the
    /// per-call limits declared in [`PATCH_MAX_EDITS`] /
    /// [`PATCH_MAX_FILE_BYTES`] and the unique-target-path invariant
    /// that the 2-phase applier depends on.
    pub fn parse(arguments_json: &str) -> Result<Self, ToolExecutionError> {
        let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
        let root = json.value();
        let edits_value = root
            .to_member("edits")
            .map_err(map_parse_err)?
            .required()
            .map_err(map_parse_err)?;

        let mut edits: Vec<PatchTool> = Vec::new();
        let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
        for item in edits_value.to_array().map_err(map_parse_err)? {
            let kind = required_string(item, "kind")?;
            let path = required_string(item, "path")?;
            if !seen_paths.insert(path.clone()) {
                return Err(ToolExecutionError::Patch(
                    PatchError::MultipleEditsSamePath { path },
                ));
            }
            let tool = match kind.as_str() {
                "add" => {
                    let content = required_string(item, "content")?;
                    if content.len() > PATCH_MAX_FILE_BYTES {
                        return Err(ToolExecutionError::Patch(PatchError::FileTooLarge { path }));
                    }
                    PatchTool::Add { path, content }
                }
                "update" => {
                    let before = required_string(item, "before")?;
                    let after = required_string(item, "after")?;
                    if after.len() > PATCH_MAX_FILE_BYTES {
                        return Err(ToolExecutionError::Patch(PatchError::FileTooLarge { path }));
                    }
                    PatchTool::Update {
                        path,
                        before,
                        after,
                    }
                }
                other => {
                    return Err(ToolExecutionError::ArgumentsParseFailed(format!(
                        "unknown edit kind: {other}"
                    )));
                }
            };
            edits.push(tool);
        }

        if edits.is_empty() {
            return Err(ToolExecutionError::ArgumentsParseFailed(
                "edits must not be empty".to_string(),
            ));
        }
        if edits.len() > PATCH_MAX_EDITS {
            return Err(ToolExecutionError::Patch(PatchError::TooManyEdits {
                count: edits.len() as u64,
            }));
        }
        Ok(Self { edits })
    }
}

const PATCH_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"edits":{"type":"array","minItems":1,"items":{
"type":"object",
"properties":{
"kind":{"type":"string","enum":["add","update"]},
"path":{"type":"string","description":"Workspace-relative target path. Must be unique across edits in one call."},
"content":{"type":"string","description":"Full contents for add."},
"before":{"type":"string","description":"For update: byte-exact substring to replace. Must match exactly once."},
"after":{"type":"string","description":"For update: replacement text."}
},
"required":["kind","path"]
}}
},
"required":["edits"]
}"#;

/// A single command the model wants to run. Parsed from the
/// `command` tool call's arguments and dispatched only after user
/// approval. `argv[0]` is exec'd directly (no shell); pipes /
/// redirects / globs must be handled by each program's native
/// flags or by explicitly invoking `["bash", "-c", "..."]` as
/// argv, which stays approval-gated unless a matching `bash -c`
/// rule pre-approves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    /// Command and arguments, exec'd directly without a shell. The
    /// first element names the program (PATH-resolved by
    /// `Command::new`); the rest are argv[1..].
    pub argv: Vec<String>,
}

impl CommandInvocation {
    /// Wire definition advertised to the model alongside
    /// [`ReadOnlyTool::definitions`] and [`PatchInvocation::definition`].
    pub fn definition() -> ToolDef {
        ToolDef {
            name: "command".to_string(),
            description: "Run a command in the workspace by executing argv[0] with argv[1..] \
                 directly (no shell). Every call requires user approval unless a matching \
                 argv_prefix rule pre-approves it. Output byte totals are capped; non-zero \
                 exit status is returned as a normal result (not an error). Runtime is not \
                 capped by attini; the user can interrupt a long-running command with Ctrl+C."
                .to_string(),
            parameters_json: COMMAND_PARAMS_SCHEMA.to_string(),
        }
    }

    /// Parse the JSON `arguments` supplied by the model.
    pub fn parse(arguments_json: &str) -> Result<Self, ToolExecutionError> {
        let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
        let root = json.value();
        let argv = required_string_array(root, "argv")?;
        if argv.is_empty() {
            return Err(ToolExecutionError::Command(CommandError::EmptyArgv));
        }
        Ok(Self { argv })
    }
}

const COMMAND_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"argv":{"type":"array","items":{"type":"string"},"minItems":1,"description":"Command and arguments to exec directly (no shell interpretation). Use each program's own flags for pipe / redirect / glob equivalents (for example --max-count instead of piping to head). For a shell pipe or chain, invoke it explicitly as [\"bash\", \"-c\", \"...\"]; that will still require user approval unless a matching argv_prefix rule pre-approves it."}
},
"required":["argv"]
}"#;

/// Request the shell to load a skill body by name and return its
/// contents verbatim as the tool result. The set of installable
/// skills is advertised at conversation start in an "Available
/// skills" system message. The shell handles filesystem resolution;
/// parsing / schema live here in sansio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLoadInvocation {
    /// Directory name of the skill under a skill root.
    pub name: String,
}

impl SkillLoadInvocation {
    /// Wire definition advertised to the model alongside
    /// [`ReadOnlyTool::definitions`], [`PatchInvocation::definition`],
    /// and [`CommandInvocation::definition`].
    pub fn definition() -> ToolDef {
        ToolDef {
            name: "skill_load".to_string(),
            description: "Load a skill body and follow its instructions. \
                 Skill names are listed in the 'Available skills' system \
                 message. The full SKILL.md body is returned verbatim as \
                 the tool result; if the skill needs arguments, the user \
                 provides them in the same turn's message."
                .to_string(),
            parameters_json: SKILL_LOAD_PARAMS_SCHEMA.to_string(),
        }
    }

    /// Parse the JSON `arguments` supplied by the model.
    pub fn parse(arguments_json: &str) -> Result<Self, ToolExecutionError> {
        let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
        let root = json.value();
        let name = required_string(root, "name")?;
        if name.trim().is_empty() {
            return Err(ToolExecutionError::ArgumentsParseFailed(
                "skill_load: name must not be empty".to_string(),
            ));
        }
        Ok(Self { name })
    }
}

const SKILL_LOAD_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"name":{"type":"string","description":"Skill name (directory name under a skill root)."}
},
"required":["name"]
}"#;

/// Terminal tool used by `plan create`'s planning mode: submit the
/// rendered plan components. attini renders the versioned actions
/// JSON block, the flat confirmation checklist, and the seal marker
/// deterministically; the model never writes markers or checkbox
/// lines itself. A successful submission ends the invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPlanInvocation {
    pub body_markdown: String,
    pub confirmations: Vec<SubmitConfirmation>,
    pub patches: Vec<SubmitPatch>,
    pub commands: Vec<SubmitCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitConfirmation {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitPatch {
    pub id: String,
    pub path: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitCommand {
    pub id: String,
    pub argv: Vec<String>,
    pub description: String,
}

impl SubmitPlanInvocation {
    pub fn definition() -> ToolDef {
        ToolDef {
            name: "submit_plan".to_string(),
            description: "Submit the plan you explored and finalize it. The plan body, \
                 confirmation items, approved patch paths, and approved command argv \
                 are rendered into a sealed Markdown plan file. This ends the planning \
                 invocation."
                .to_string(),
            parameters_json: SUBMIT_PLAN_PARAMS_SCHEMA.to_string(),
        }
    }

    /// Parse and validate the structured `submit_plan` arguments.
    /// Enforces the shared plan ID rules, per-kind counts, required
    /// non-empty fields, and uniqueness across all IDs. `all-ok` and
    /// the format version are reserved for attini, not the model.
    pub fn parse(arguments_json: &str) -> Result<Self, ToolExecutionError> {
        let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
        let root = json.value();
        let body_markdown = required_string(root, "body_markdown")?;
        if body_markdown.trim().is_empty() {
            return Err(ToolExecutionError::ArgumentsParseFailed(
                "submit_plan: body_markdown must not be empty".to_string(),
            ));
        }
        let confirmations = parse_confirmation_args(root)?;
        let patches = parse_patch_args(root)?;
        let commands = parse_command_args(root)?;

        if confirmations.len() > crate::plan::MAX_CONFIRMATIONS {
            return Err(ToolExecutionError::ArgumentsParseFailed(format!(
                "submit_plan: too many confirmations (max {})",
                crate::plan::MAX_CONFIRMATIONS
            )));
        }
        if patches.len() > crate::plan::MAX_ACTIONS || commands.len() > crate::plan::MAX_ACTIONS {
            return Err(ToolExecutionError::ArgumentsParseFailed(format!(
                "submit_plan: too many actions (max {})",
                crate::plan::MAX_ACTIONS
            )));
        }
        let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &confirmations {
            if c.id == crate::plan::RESERVED_CONFIRMATION_ID {
                return Err(ToolExecutionError::ArgumentsParseFailed(
                    "submit_plan: the all-ok confirmation is reserved".to_string(),
                ));
            }
            check_plan_id(&c.id, "confirmation", &mut ids)?;
            if c.description.trim().is_empty() {
                return Err(ToolExecutionError::ArgumentsParseFailed(
                    "submit_plan: confirmation description must not be empty".to_string(),
                ));
            }
        }
        for p in &patches {
            check_plan_id(&p.id, "patch", &mut ids)?;
            if p.path.trim().is_empty() {
                return Err(ToolExecutionError::ArgumentsParseFailed(
                    "submit_plan: patch path must not be empty".to_string(),
                ));
            }
            if p.description.trim().is_empty() {
                return Err(ToolExecutionError::ArgumentsParseFailed(
                    "submit_plan: patch description must not be empty".to_string(),
                ));
            }
        }
        let mut patch_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for p in &patches {
            if !patch_paths.insert(p.path.as_str()) {
                return Err(ToolExecutionError::ArgumentsParseFailed(format!(
                    "submit_plan: duplicate patch path {:?}",
                    p.path
                )));
            }
        }
        for c in &commands {
            check_plan_id(&c.id, "command", &mut ids)?;
            if c.argv.is_empty() {
                return Err(ToolExecutionError::ArgumentsParseFailed(
                    "submit_plan: command argv must have at least one element".to_string(),
                ));
            }
            if c.description.trim().is_empty() {
                return Err(ToolExecutionError::ArgumentsParseFailed(
                    "submit_plan: command description must not be empty".to_string(),
                ));
            }
        }
        let mut command_argv: std::collections::HashSet<&Vec<String>> =
            std::collections::HashSet::new();
        for c in &commands {
            if !command_argv.insert(&c.argv) {
                return Err(ToolExecutionError::ArgumentsParseFailed(format!(
                    "submit_plan: duplicate command argv {:?}",
                    c.argv
                )));
            }
        }
        Ok(Self {
            body_markdown,
            confirmations,
            patches,
            commands,
        })
    }
}

/// A non-terminal `plan` tool exposed in normal (non-planning)
/// sessions. The model drafts a sealed plan artifact for a change
/// that spans multiple `patch` calls or also needs `command` steps.
/// A human approves it with `attini plan ok <file>` and runs it with
/// `attini plan run <file>`. Unlike [`SubmitPlanInvocation`], calling
/// this tool does NOT end the invocation. It is a suggestion for
/// larger changes; if the change must be iterated step-by-step on
/// intermediate results, the model should keep using `patch` /
/// `command` directly instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanProposeInvocation {
    pub inner: SubmitPlanInvocation,
}

impl PlanProposeInvocation {
    pub fn definition() -> ToolDef {
        ToolDef {
            name: "plan".to_string(),
            description: "Draft a plan for a change that spans multiple `patch` calls or \
                 also needs `command` steps, and write it as a sealed Markdown plan file \
                 under the session's plans directory. A human must approve it with \
                 `attini plan ok <file>` before it can be run with `attini plan run <file>`. \
                 Use this when a change is too large to complete as a single patch/command \
                 sequence. This does NOT end the invocation, and is a suggestion, not a \
                 mandate: if the change must be iterated step-by-step on intermediate \
                 results, keep using `patch`/`command` directly."
                .to_string(),
            parameters_json: SUBMIT_PLAN_PARAMS_SCHEMA.to_string(),
        }
    }

    pub fn parse(arguments_json: &str) -> Result<Self, ToolExecutionError> {
        match SubmitPlanInvocation::parse(arguments_json) {
            Ok(inner) => Ok(Self { inner }),
            Err(err) => Err(rename_plan_error(err)),
        }
    }
}

/// Rewrite `submit_plan`-prefixed argument errors to `plan` so the
/// model sees a consistent tool name when `plan` reuses the shared
/// [`SubmitPlanInvocation`] validation.
fn rename_plan_error(err: ToolExecutionError) -> ToolExecutionError {
    match err {
        ToolExecutionError::ArgumentsParseFailed(msg) => {
            ToolExecutionError::ArgumentsParseFailed(msg.replace("submit_plan", "plan"))
        }
        other => other,
    }
}

fn check_plan_id(
    id: &str,
    kind: &str,
    ids: &mut std::collections::HashSet<String>,
) -> Result<(), ToolExecutionError> {
    crate::plan::validate_plan_id(id).map_err(|e| {
        ToolExecutionError::ArgumentsParseFailed(format!("submit_plan: {kind}: {e}"))
    })?;
    if !ids.insert(id.to_string()) {
        return Err(ToolExecutionError::ArgumentsParseFailed(format!(
            "submit_plan: duplicate {kind} id {id:?}"
        )));
    }
    Ok(())
}

fn parse_confirmation_args(
    root: RawJsonValue<'_, '_>,
) -> Result<Vec<SubmitConfirmation>, ToolExecutionError> {
    let mut out = Vec::new();
    let value = root
        .to_member("confirmations")
        .map_err(map_parse_err)?
        .optional();
    if let Some(arr) = value {
        for item in arr.to_array().map_err(map_parse_err)? {
            out.push(SubmitConfirmation {
                id: required_string(item, "id")?,
                description: required_string(item, "description")?,
            });
        }
    }
    Ok(out)
}

fn parse_patch_args(root: RawJsonValue<'_, '_>) -> Result<Vec<SubmitPatch>, ToolExecutionError> {
    let mut out = Vec::new();
    let value = root.to_member("patches").map_err(map_parse_err)?.optional();
    if let Some(arr) = value {
        for item in arr.to_array().map_err(map_parse_err)? {
            out.push(SubmitPatch {
                id: required_string(item, "id")?,
                path: required_string(item, "path")?,
                description: required_string(item, "description")?,
            });
        }
    }
    Ok(out)
}

fn parse_command_args(
    root: RawJsonValue<'_, '_>,
) -> Result<Vec<SubmitCommand>, ToolExecutionError> {
    let mut out = Vec::new();
    let value = root
        .to_member("commands")
        .map_err(map_parse_err)?
        .optional();
    if let Some(arr) = value {
        for item in arr.to_array().map_err(map_parse_err)? {
            out.push(SubmitCommand {
                id: required_string(item, "id")?,
                argv: required_string_array(item, "argv")?,
                description: required_string(item, "description")?,
            });
        }
    }
    Ok(out)
}

const SUBMIT_PLAN_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"body_markdown":{"type":"string","description":"Natural-language Markdown body: purpose, change approach, targets, and verification."},
"confirmations":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string","description":"Lowercase id ([a-z][a-z0-9-]{0,63}); the reserved all-ok id is added by attini."},"description":{"type":"string","description":"A user judgment item to approve."}},"required":["id","description"]}},
"patches":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"path":{"type":"string","description":"Exact workspace-relative path that may be patched."},"description":{"type":"string"}},"required":["id","path","description"]}},
"commands":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"argv":{"type":"array","items":{"type":"string"},"minItems":1,"description":"Exact argv that may be run (no shell)."},"description":{"type":"string"}},"required":["id","argv","description"]}}
},
"required":["body_markdown"]
}"#;

/// Run a child `attini agent` synchronously in a separate session and
/// block until it reaches a terminal state. Returns the child's
/// session name, terminal state, and latest assistant content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRunInvocation {
    pub prompt: String,
    pub session_name: Option<String>,
}

impl SubagentRunInvocation {
    pub fn definition() -> ToolDef {
        ToolDef {
            name: "subagent_run".to_string(),
            description: "Run a child attini agent synchronously in a separate session and \
                 block until it finishes. The child runs with its own session, permissions, \
                 and metrics, isolated from the parent's conversation and state."
                .to_string(),
            parameters_json: SUBAGENT_RUN_PARAMS_SCHEMA.to_string(),
        }
    }

    pub fn parse(arguments_json: &str) -> Result<Self, ToolExecutionError> {
        let json = nojson::RawJson::parse(arguments_json).map_err(map_parse_err)?;
        let root = json.value();
        let prompt = required_string(root, "prompt")?;
        if prompt.trim().is_empty() {
            return Err(ToolExecutionError::ArgumentsParseFailed(
                "subagent_run: prompt must not be empty".to_string(),
            ));
        }
        let session_name = optional_string(root, "session_name")?;
        if let Some(name) = &session_name
            && name.trim().is_empty()
        {
            return Err(ToolExecutionError::ArgumentsParseFailed(
                "subagent_run: session_name must not be empty".to_string(),
            ));
        }
        Ok(Self {
            prompt,
            session_name,
        })
    }
}

const SUBAGENT_RUN_PARAMS_SCHEMA: &str = r#"{
"type":"object",
"properties":{
"prompt":{"type":"string","description":"Initial user prompt for the child agent."},
"session_name":{"type":"string","description":"Optional session name for the child. Omit to auto-generate a `subagent-<timestamp>-<hex>` name. An idle existing session is reused."}
},
"required":["prompt"]
}"#;

fn required_string(root: RawJsonValue<'_, '_>, name: &str) -> Result<String, ToolExecutionError> {
    let value = root
        .to_member(name)
        .map_err(map_parse_err)?
        .required()
        .map_err(map_parse_err)?;
    value.try_into().map_err(map_parse_err)
}

fn required_string_array(
    root: RawJsonValue<'_, '_>,
    name: &str,
) -> Result<Vec<String>, ToolExecutionError> {
    let value = root
        .to_member(name)
        .map_err(map_parse_err)?
        .required()
        .map_err(map_parse_err)?;
    let array = value.to_array().map_err(map_parse_err)?;
    let mut out = Vec::new();
    for item in array {
        let s: String = item.try_into().map_err(map_parse_err)?;
        out.push(s);
    }
    Ok(out)
}

fn optional_string(
    root: RawJsonValue<'_, '_>,
    name: &str,
) -> Result<Option<String>, ToolExecutionError> {
    match root.to_member(name).map_err(map_parse_err)?.optional() {
        None => Ok(None),
        Some(value) => value.try_into().map_err(map_parse_err),
    }
}

fn optional_bool(
    root: RawJsonValue<'_, '_>,
    name: &str,
) -> Result<Option<bool>, ToolExecutionError> {
    match root.to_member(name).map_err(map_parse_err)?.optional() {
        None => Ok(None),
        Some(value) => Ok(Some(value.try_into().map_err(map_parse_err)?)),
    }
}

fn optional_usize(
    root: RawJsonValue<'_, '_>,
    name: &str,
) -> Result<Option<usize>, ToolExecutionError> {
    match root.to_member(name).map_err(map_parse_err)?.optional() {
        None => Ok(None),
        Some(value) => Ok(Some(value.try_into().map_err(map_parse_err)?)),
    }
}

/// Result of executing a [`ReadOnlyTool`] on behalf of the model.
///
/// Partial-success outcomes (truncated output, silently-skipped
/// binary files) are `Ok` with a JSON payload that includes
/// `truncated: true` or `skipped_binary: N` so the model can see what
/// happened. Only impossible-to-proceed situations become
/// [`Err`](ToolExecutionError).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    Ok(String),
    Err(ToolExecutionError),
}

/// Reasons a tool invocation could not succeed. Rendered into `Tool`
/// role message content for the model to reason about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecutionError {
    OutsideWorkspace,
    NotUtf8,
    Binary,
    IoError(String),
    ArgumentsParseFailed(String),
    ArgumentsTooLarge,
    UnknownTool,
    TurnToolCallLimitExceeded,
    /// Failure of a [`PatchTool`] invocation. Isolated in its own
    /// enum to keep the read-only error variants intact while
    /// letting patch introduce approval- and filesystem-write-
    /// specific failure modes.
    Patch(PatchError),
    /// Failure of a [`CommandInvocation`] before the child process
    /// produced meaningful output (rejected, spawn failed, arguments
    /// out of range). In-run terminations (cancel, output limit)
    /// are surfaced as `Ok` with a `termination_reason` instead.
    Command(CommandError),
}

/// Failure modes specific to [`PatchTool`] invocations. See the
/// polished `0007` design for the discipline: any failure that
/// prevents the workspace from being updated is surfaced here so
/// the model can decide whether to retry, split the batch, or
/// abandon the edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// The user actively rejected the preview.
    Rejected,
    /// The target file's SHA-256 changed between preview and apply.
    Conflict { path: String },
    /// `Update.before` matched zero times in the target file.
    NoMatch { path: String },
    /// `Update.before` matched more than once in the target file.
    AmbiguousMatch { path: String, match_count: u64 },
    /// `Add` target already exists at apply time.
    AddOnExistingFile { path: String },
    /// The parent directory of an `Add` target does not exist and is
    /// not auto-created.
    ParentDirMissing { path: String },
    /// `Update` target does not exist at apply time.
    UpdateOnMissingFile { path: String },
    /// The resolved canonical path escapes the workspace root.
    OutsideWorkspace { path: String },
    /// Layer 1 hardcoded reject: the target is a runtime-critical file
    /// (`.git/**`, `.attini/*/{LOCK,conversation.jsonl,...}`,
    /// `.attini/{permissions.json,memories.md}`) regardless of git
    /// tracking status. `reason` is a short human-readable classifier
    /// (`"git metadata"`, `"session runtime state"`, etc.) embedded in
    /// the message; it is not exposed as a separate JSON field.
    ExcludedPath { path: String, reason: String },
    /// Layer 3 Update reject: the target is not present in the
    /// startup-captured git tracked set (nor added by an earlier
    /// successful Add in the same invocation). Writing would be
    /// irrecoverable, so the write is refused.
    UntrackedTarget { path: String },
    /// Layer 3 Add reject: the parent directory of the new file is
    /// under a gitignored region, indicating an area the user has
    /// declared out-of-scope for the repo.
    IgnoredParent { path: String },
    /// Layer 4 reject: the workspace is not inside a git repository,
    /// so Layer 3 cannot judge. Layer 1/2 still apply; writes outside
    /// scratchpad are refused as the safe default.
    NotInGitRepo { path: String },
    /// `edits.len()` exceeded [`PATCH_MAX_EDITS`].
    TooManyEdits { count: u64 },
    /// Two edits within the same call named the same target path.
    MultipleEditsSamePath { path: String },
    /// The affected file (either new `content` on add, existing
    /// target on update, or the produced `after` on update) exceeds
    /// [`PATCH_MAX_FILE_BYTES`].
    FileTooLarge { path: String },
    /// Underlying filesystem I/O failed during preview or apply.
    IoError { path: String, message: String },
    /// `rename(2)` returned `EXDEV`. Not handled by fallback in the
    /// prototype scope.
    CrossDeviceRename { path: String },
}

impl PatchError {
    /// Machine-readable code paired with a human-readable message.
    /// The code is stable enough for the model to key on retry logic.
    pub fn to_code_and_message(&self) -> (&'static str, String) {
        match self {
            Self::Rejected => (
                "patch_rejected",
                "user rejected the patch preview".to_string(),
            ),
            Self::Conflict { path } => (
                "patch_conflict",
                format!("target file changed between preview and apply: {path}"),
            ),
            Self::NoMatch { path } => (
                "patch_no_match",
                format!("`before` did not match any content in {path}"),
            ),
            Self::AmbiguousMatch { path, match_count } => (
                "patch_ambiguous_match",
                format!("`before` matched {match_count} places in {path}; expected exactly 1"),
            ),
            Self::AddOnExistingFile { path } => (
                "patch_add_on_existing_file",
                format!("cannot add: file already exists at {path}"),
            ),
            Self::ParentDirMissing { path } => (
                "patch_parent_dir_missing",
                format!("parent directory does not exist for {path}"),
            ),
            Self::UpdateOnMissingFile { path } => (
                "patch_update_on_missing_file",
                format!("cannot update: file does not exist at {path}"),
            ),
            Self::OutsideWorkspace { path } => (
                "patch_outside_workspace",
                format!("target path {path} escapes the workspace root"),
            ),
            Self::ExcludedPath { path, reason } => (
                "patch_excluded_path",
                format!("path is runtime-critical ({reason}): {path}"),
            ),
            Self::UntrackedTarget { path } => (
                "patch_untracked_target",
                format!("cannot update git-untracked file (would be irrecoverable): {path}"),
            ),
            Self::IgnoredParent { path } => (
                "patch_ignored_parent",
                format!("cannot add into git-ignored directory: {path}"),
            ),
            Self::NotInGitRepo { path } => (
                "patch_not_in_git_repo",
                format!(
                    "workspace is not a git repository; patch refused outside scratchpad: {path}"
                ),
            ),
            Self::TooManyEdits { count } => (
                "patch_too_many_edits",
                format!("edits count {count} exceeded the {PATCH_MAX_EDITS} limit"),
            ),
            Self::MultipleEditsSamePath { path } => (
                "patch_multiple_edits_same_path",
                format!("more than one edit targets {path} within the same patch call"),
            ),
            Self::FileTooLarge { path } => (
                "patch_file_too_large",
                format!(
                    "target or new content for {path} exceeded the {PATCH_MAX_FILE_BYTES} byte limit"
                ),
            ),
            Self::IoError { path, message } => (
                "patch_io_error",
                format!("filesystem I/O failed for {path}: {message}"),
            ),
            Self::CrossDeviceRename { path } => (
                "patch_cross_device_rename",
                format!("cross-device rename not supported for {path}"),
            ),
        }
    }

    /// Actionable remediation advice for the model, surfaced as the
    /// `hint` member of the tool error JSON (`null` when absent).
    /// Returns `None` for policy/permission rejections where the only
    /// correct move is to choose a different target (or ask the user).
    pub fn to_hint(&self) -> Option<&'static str> {
        match self {
            Self::MultipleEditsSamePath { .. } => Some(
                "each path may appear in only one edit per call; split the edits into \
                 separate patch calls (one per target path) or merge this path's changes \
                 into a single update with one before/after pair",
            ),
            Self::NoMatch { .. } => Some(
                "`before` is not present verbatim in the file; read the file first, then \
                 copy an exact existing substring into `before`",
            ),
            Self::AmbiguousMatch { .. } => Some(
                "`before` matched more than one place; extend `before` with surrounding \
                 lines so it matches exactly once",
            ),
            Self::AddOnExistingFile { .. } => Some(
                "path already exists; use update with a before/after pair instead of add, \
                 or choose a different path",
            ),
            Self::UpdateOnMissingFile { .. } => {
                Some("path does not exist; use add to create it, or fix the path")
            }
            Self::ParentDirMissing { .. } => Some(
                "the parent directory does not exist; create it first (e.g. mkdir -p), \
                 then retry",
            ),
            Self::TooManyEdits { .. } => {
                Some("too many edits in one call; split across multiple patch calls")
            }
            Self::FileTooLarge { .. } => {
                Some("content is too large; reduce it or split across multiple patch calls")
            }
            Self::Conflict { .. } => {
                Some("target changed between preview and apply; re-read the file and retry")
            }
            _ => None,
        }
    }
}

/// Failure modes specific to [`CommandInvocation`]. In-run
/// terminations (cancel, output limit) do not appear here; they
/// surface as `Ok` with a `termination_reason` in the tool result
/// JSON so the model can still see the partial output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// User rejected the approval preview.
    Rejected,
    /// `exec` failed (`argv[0]` not on PATH, ENOMEM, EPERM, ...).
    SpawnFailed { message: String },
    /// `argv` was an empty array.
    EmptyArgv,
}

impl CommandError {
    pub fn to_code_and_message(&self) -> (&'static str, String) {
        match self {
            Self::Rejected => (
                "command_rejected",
                "user rejected the command preview".to_string(),
            ),
            Self::SpawnFailed { message } => (
                "command_spawn_failed",
                format!("failed to spawn command: {message}"),
            ),
            Self::EmptyArgv => ("command_empty_argv", "argv must not be empty".to_string()),
        }
    }
}

/// SHA-256 digest of a file captured at [`PatchTool::Update`] preview
/// time. `sha256` is `None` for [`PatchTool::Add`] paths (whose apply-
/// time check is "the file must NOT exist" rather than a hash match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewHash {
    pub path: String,
    pub sha256: Option<[u8; 32]>,
}

/// TUI-facing summary of an incoming patch, computed on the shell
/// side from the `PatchInvocation` and delivered to the core with
/// [`Event::PatchPreviewReady`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatchPreview {
    /// Sorted, deduplicated list of target paths.
    pub target_paths: Vec<String>,
    /// Number of `+` lines across all edits (unified-diff style).
    pub added_lines: u64,
    /// Number of `-` lines across all edits.
    pub removed_lines: u64,
    /// `invocation.edits.len()`.
    pub edit_count: u64,
}

/// Approval status of a tool call. Read-only tools always report
/// [`ApprovalState::NotRequired`]; patch and command tool calls flow
/// through `Pending` → (`Approved` or `Rejected`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalState {
    NotRequired,
    Pending,
    Approved,
    Rejected,
}

/// TUI-facing summary of an incoming command call, populated at
/// `on_finish` so the approval prompt has the full command text to
/// display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPreview {
    pub argv: Vec<String>,
    /// Directory the shell will spawn the child in. Copied from
    /// [`AgentCore::set_workspace_display`] so the pure Sans I/O core
    /// does not have to know the filesystem.
    pub working_directory: String,
}

/// Rolling tail of a running command's output plus running byte
/// totals. Updated on every [`Event::CommandOutputChunk`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutputTail {
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_bytes_total: u64,
    pub stderr_bytes_total: u64,
}

/// Which pipe an [`Event::CommandOutputChunk`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutputStream {
    Stdout,
    Stderr,
}

/// Maximum characters retained in [`CommandOutputTail::stdout_tail`]
/// / `stderr_tail` for TUI display. New chunks push the tail
/// forward; older content is dropped so the tail stays small.
const COMMAND_TAIL_CHARS: usize = 4 * 1024;

impl ToolExecutionError {
    /// Compact JSON representation suitable for a `Tool` role message
    /// body: `{"error":"CODE","message":"...","hint":"..."}`. `hint`
    /// is `null` unless there is actionable remediation, and is meant
    /// to guide the model's retry. Fields are stable enough for the
    /// model to key on.
    pub fn to_json_string(&self) -> String {
        let (code, message): (&str, String) = match self {
            Self::OutsideWorkspace => (
                "outside_workspace",
                "path escapes the workspace root".to_string(),
            ),
            Self::NotUtf8 => ("not_utf8", "file is not valid UTF-8".to_string()),
            Self::Binary => (
                "binary",
                "file contains binary data and cannot be read as text".to_string(),
            ),
            Self::IoError(msg) => ("io_error", msg.clone()),
            Self::ArgumentsParseFailed(msg) => ("arguments_parse_failed", msg.clone()),
            Self::ArgumentsTooLarge => (
                "arguments_too_large",
                format!(
                    "tool call arguments exceeded the {} byte limit",
                    ARGUMENTS_MAX_BYTES
                ),
            ),
            Self::UnknownTool => (
                "unknown_tool",
                "function_name does not match a known tool".to_string(),
            ),
            Self::TurnToolCallLimitExceeded => (
                "turn_tool_call_limit_exceeded",
                format!(
                    "this user turn exceeded the {} tool call limit",
                    TURN_TOOL_CALL_LIMIT
                ),
            ),
            Self::Patch(err) => err.to_code_and_message(),
            Self::Command(err) => err.to_code_and_message(),
        };
        let hint: Option<&'static str> = match self {
            Self::Patch(err) => err.to_hint(),
            _ => None,
        };
        Json(ToolErrorJson {
            code,
            message: &message,
            hint,
        })
        .to_string()
    }
}

struct ToolErrorJson<'a> {
    code: &'a str,
    message: &'a str,
    hint: Option<&'a str>,
}

impl nojson::DisplayJson for ToolErrorJson<'_> {
    fn fmt(&self, f: &mut nojson::JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("error", self.code)?;
            f.member("message", self.message)?;
            f.member("hint", self.hint)?;
            Ok(())
        })
    }
}

/// Opaque identifier for a model request tracked by the core.
///
/// The core assigns IDs internally; the surrounding shell receives
/// them via [`Action::StartRequest`] and tags subsequent events with
/// the same value. Tests and stubs can mint synthetic IDs with
/// [`Self::new`] to exercise stale-event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Coarse-grained runtime status of the core.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    /// No request is in flight and the shell can accept a new prompt.
    #[default]
    Idle,
    /// A request has been issued but no delta has been received yet.
    AwaitingModel,
    /// The current request has begun streaming content or reasoning.
    Streaming,
    /// The model finished with `finish_reason == "tool_calls"` and the
    /// shell is executing the requested tools; the core is awaiting
    /// [`Event::ToolResult`] for every outstanding call before
    /// auto-emitting the follow-up [`Action::StartRequest`].
    ToolRunning,
    /// At least one patch call is waiting for user approval. Coexists
    /// with in-flight read-only tool execution — the shell keeps
    /// running those in the background while the UI focus is on the
    /// approval prompt.
    AwaitingApproval,
}

/// Buffered pieces of the assistant response for the in-flight request.
///
/// `content` is the user-visible answer accumulated so far;
/// `reasoning` is the DeepSeek thinking-mode extension surfaced for
/// display only (not carried over between turns at this stage).
/// `finish_reason` becomes `Some` after the final chunk has been
/// observed but before the response is committed to the conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingResponse {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
}

/// Input event fed to the core by the surrounding I/O shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The user submitted a new prompt. Ignored while a request is
    /// already in flight.
    UserMessage(String),
    /// The user requested to abort the current request.
    Cancel,
    /// A content delta arrived from the transport.
    ContentDelta { request: RequestId, text: String },
    /// A reasoning-content delta arrived from the transport.
    ReasoningDelta { request: RequestId, text: String },
    /// A tool-call streaming fragment arrived. The core assembles
    /// fragments across chunks by matching on `index`; the first
    /// non-null `id` / `function_name` win, and `arguments_fragment`
    /// is concatenated in arrival order.
    ToolCallDelta {
        request: RequestId,
        index: u64,
        id: Option<String>,
        function_name: Option<String>,
        arguments_fragment: Option<String>,
    },
    /// The transport observed the terminating `[DONE]` or finish reason.
    Finish {
        request: RequestId,
        reason: Option<String>,
    },
    /// A tool execution completed (successfully or with an error).
    /// Accepted while the core is in either the [`Status::ToolRunning`]
    /// or [`Status::AwaitingApproval`] phase for the matching request.
    ToolResult {
        request: RequestId,
        call_id: String,
        outcome: ToolOutcome,
    },
    /// Shell has computed the target file hashes and diff summary for
    /// a patch call and is waiting for user approval. Transitions the
    /// core to [`Status::AwaitingApproval`].
    PatchPreviewReady {
        request: RequestId,
        call_id: String,
        preview_hashes: Vec<PreviewHash>,
        preview: PatchPreview,
    },
    /// User approved the approval-mode preview for `call_id`. The
    /// core dispatches the tool-specific action ([`Action::ApplyPatch`]
    /// for patch, [`Action::ExecuteCommand`] for command).
    ApproveToolCall { call_id: String },
    /// User rejected the approval-mode preview for `call_id`. The
    /// core synthesises an `Err(Rejected)` outcome for the call and
    /// continues the tool loop.
    RejectToolCall { call_id: String },
    /// Shell delivered a chunk of stdout or stderr from a running
    /// command. Only accepted while the corresponding pending tool
    /// result has `ApprovalState::Approved` and `outcome.is_none()`.
    CommandOutputChunk {
        request: RequestId,
        call_id: String,
        stream: CommandOutputStream,
        bytes: Vec<u8>,
    },
    /// The transport reported an unrecoverable error for this request.
    TransportError { request: RequestId, message: String },
    /// The request exceeded its allotted time.
    Timeout { request: RequestId },
}

/// Output action produced by the core for the surrounding I/O shell.
///
/// The shell renders the state after every batch that contains
/// [`Action::Redraw`]; other variants direct the transport or surface
/// a diagnostic to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hand `messages` to the transport and forward returned events
    /// back to the core tagged with `id`.
    StartRequest {
        id: RequestId,
        messages: Vec<ChatMessage>,
    },
    /// Ask the transport to abort the request identified by `id`. The
    /// shell may still receive late events for this ID; they will be
    /// dropped when forwarded back to the core.
    CancelRequest { id: RequestId },
    /// Run `invocation` on behalf of the model in the surrounding
    /// shell (typically on a blocking thread) and deliver the
    /// outcome back as [`Event::ToolResult`] tagged with the same
    /// `request` and `call_id`.
    ExecuteTool {
        request: RequestId,
        call_id: String,
        invocation: ReadOnlyTool,
    },
    /// Compute the target file SHA-256 hashes and diff summary for
    /// `invocation` and deliver them back as
    /// [`Event::PatchPreviewReady`] so the core can enter approval
    /// mode. The shell must not touch the filesystem yet.
    PreviewPatch {
        request: RequestId,
        call_id: String,
        invocation: PatchInvocation,
    },
    /// User approved the patch preview; apply the 2-phase writeback
    /// using `preview_hashes` to detect concurrent modifications
    /// between preview and apply. `invocation` is re-parsed from the
    /// tool call arguments so the shell does not need to cache it
    /// between preview and approval.
    ApplyPatch {
        request: RequestId,
        call_id: String,
        invocation: PatchInvocation,
        preview_hashes: Vec<PreviewHash>,
    },
    /// User approved the command preview; run the shell command,
    /// stream stdout/stderr back via [`Event::CommandOutputChunk`],
    /// and finish with an [`Event::ToolResult`] carrying the JSON
    /// result described in the polished `0008` design.
    ExecuteCommand {
        request: RequestId,
        call_id: String,
        invocation: CommandInvocation,
    },
    /// Abort any tool executions that were dispatched for `request`
    /// but have not yet reported an outcome. Emitted when the user
    /// cancels or the request times out while in
    /// [`Status::ToolRunning`] or [`Status::AwaitingApproval`].
    CancelToolExecution { request: RequestId },
    /// A diagnostic message the shell should surface to the user
    /// (transport failure, timeout, etc.). The core does not retain
    /// it; the shell owns any "sticky until dismissed" behaviour.
    ReportError { message: String },
    /// Something visible to the user changed; the shell should redraw.
    Redraw,
}

/// The Sans I/O agent core.
#[derive(Debug, Clone, Default)]
pub struct AgentCore {
    conversation: Vec<ChatMessage>,
    pending: Option<Pending>,
    status: Status,
    next_id: u64,
    /// Number of [`Action::ExecuteTool`] emitted since the current
    /// user turn began. Reset to 0 on every transition to
    /// [`Status::Idle`], including cancels, errors, and timeouts.
    tool_calls_this_turn: usize,
    /// Human-readable workspace root, copied into
    /// [`CommandPreview::working_directory`] so the approval prompt
    /// can show it. Set by the shell during startup via
    /// [`AgentCore::set_workspace_display`]; empty when unset.
    workspace_display: String,
    metrics: AgentMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    id: RequestId,
    phase: PendingPhase,
    response: PendingResponse,
    /// Partial tool calls being assembled from streaming fragments.
    /// The `u64` key is the `choices[0].delta.tool_calls[].index`.
    tool_call_slots: BTreeMap<u64, ToolCallSlot>,
    /// Tool results awaited before advancing to the next request.
    /// Populated on transition to `ToolRunning` phase; some
    /// entries may already carry a synthetic `Err` outcome for calls
    /// that hit the arguments-size or turn-tool-count limit.
    tool_results: Vec<PendingToolResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingPhase {
    /// Receiving deltas from the model's SSE stream.
    Streaming,
    /// Model finished with `finish_reason == "tool_calls"`; waiting on
    /// the shell to deliver [`Event::ToolResult`] for every call.
    ToolRunning,
    /// At least one patch call is waiting for user approval. Read-only
    /// tool results still land in this phase; `on_tool_result`'s gate
    /// accepts both `ToolRunning` and `AwaitingApproval`.
    AwaitingApproval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolCallSlot {
    id: Option<String>,
    function_name: Option<String>,
    arguments: String,
    /// Set once `arguments.len()` (after appending the current
    /// fragment) would exceed [`ARGUMENTS_MAX_BYTES`]; subsequent
    /// fragments for this index are dropped and the finalised call is
    /// resolved to `Err(ArgumentsTooLarge)` instead of being executed.
    over_limit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingToolResult {
    call_id: String,
    function_name: String,
    arguments_json: String,
    /// Approval state. Read-only tools are always
    /// [`ApprovalState::NotRequired`]. Patch tools stay in
    /// `NotRequired` until [`Event::PatchPreviewReady`] bumps them to
    /// `Pending`; command tools are `Pending` from `on_finish`.
    approval: ApprovalState,
    /// Populated when [`Event::PatchPreviewReady`] arrives so the TUI
    /// can render the diff summary. `None` for non-patch tools and
    /// for patch tools before the shell has produced a preview.
    patch_preview: Option<PatchPreview>,
    /// Populated together with `patch_preview`. Retained here so
    /// [`Event::ApproveToolCall`] can hand the same hashes back to
    /// the shell as [`Action::ApplyPatch`] without a round trip.
    preview_hashes: Vec<PreviewHash>,
    /// Populated at `on_finish` for command tool calls. Drives the
    /// approval-mode label content in the TUI.
    command_preview: Option<CommandPreview>,
    /// Populated on the first [`Event::CommandOutputChunk`] and
    /// updated on every subsequent one until the tool result lands.
    command_output_tail: Option<CommandOutputTail>,
    /// `None` while the shell is still executing the tool; `Some` once
    /// it has reported (or the core has synthesised) an outcome.
    outcome: Option<ToolOutcome>,
}

/// Read-only projection of a single tool call for TUI rendering.
///
/// Built from [`AgentCore::active_tool_calls`]; `is_streaming` is
/// `true` while the model is still emitting fragments and `false`
/// once the assistant turn has been committed and the shell is
/// executing (or has already resolved) the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveToolCall {
    pub call_id: String,
    pub function_name: String,
    pub arguments_json: String,
    pub outcome: Option<ToolOutcome>,
    pub is_streaming: bool,
    /// Approval status. [`ApprovalState::NotRequired`] for read-only
    /// tools; otherwise reflects the approval flow state.
    pub approval: ApprovalState,
    /// Diff summary from [`Event::PatchPreviewReady`]. `None` for
    /// non-patch tools or patch tools whose preview has not yet
    /// arrived.
    pub patch_preview: Option<PatchPreview>,
    /// SHA-256 hashes captured at preview time. Empty for non-patch
    /// tools; the TUI does not display them (they exist only so the
    /// core can hand them to [`Action::ApplyPatch`] on approval).
    pub preview_hashes: Vec<PreviewHash>,
    /// Approval-mode label content for command tool calls. `None`
    /// for other tool kinds.
    pub command_preview: Option<CommandPreview>,
    /// Rolling output tail while a command is running. `None` before
    /// the first chunk arrives, or for non-command tool kinds.
    pub command_output_tail: Option<CommandOutputTail>,
}

/// Cumulative counters for the branches taken by
/// [`AgentCore::handle_event`].
///
/// Each field increases exactly once per event that lands in the
/// corresponding branch; drop paths (stale event id, event received
/// while idle, etc.) also increment their own counter so the sum of a
/// pair (`*_accepted` + `*_rejected` / `*_appended` + `*_dropped_as_stale`)
/// tells you how many events of a given kind the core has seen.
///
/// Read via [`AgentCore::metrics`]. Fields use the shared
/// [`Counter`] wrapper so `let a = agent.metrics().clone();`
/// captures an independent snapshot for before/after diffs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMetrics {
    /// [`Event::UserMessage`] received while idle: the message was
    /// appended to the conversation and an [`Action::StartRequest`]
    /// was emitted.
    pub user_messages_accepted: Counter,
    /// [`Event::UserMessage`] received while another request was
    /// still in flight; the message was dropped.
    pub user_messages_rejected_while_active: Counter,
    /// [`Event::Cancel`] received while a request was in flight; the
    /// pending response was dropped and [`Action::CancelRequest`] was
    /// emitted.
    pub cancels_applied: Counter,
    /// [`Event::Cancel`] received while idle; no state changed.
    pub cancels_ignored_when_idle: Counter,
    /// [`Event::ContentDelta`] whose `request` matched the active
    /// [`RequestId`]; the text was appended to the pending response.
    pub content_deltas_appended: Counter,
    /// [`Event::ContentDelta`] dropped because no request was active
    /// or the `request` id did not match the active one.
    pub content_deltas_dropped_as_stale: Counter,
    /// [`Event::ReasoningDelta`] whose `request` matched the active
    /// [`RequestId`]; the text was appended to the pending reasoning
    /// buffer.
    pub reasoning_deltas_appended: Counter,
    /// [`Event::ReasoningDelta`] dropped because no request was
    /// active or the `request` id did not match the active one.
    pub reasoning_deltas_dropped_as_stale: Counter,
    /// [`Event::Finish`] whose `request` matched the active
    /// [`RequestId`]; the assistant message was committed to the
    /// conversation.
    pub finishes_committed: Counter,
    /// [`Event::Finish`] dropped because no request was active or the
    /// `request` id did not match the active one.
    pub finishes_dropped_as_stale: Counter,
    /// [`Event::TransportError`] whose `request` matched the active
    /// [`RequestId`]; an [`Action::ReportError`] was emitted.
    pub transport_errors_recorded: Counter,
    /// [`Event::TransportError`] dropped because no request was
    /// active or the `request` id did not match the active one.
    pub transport_errors_dropped_as_stale: Counter,
    /// [`Event::Timeout`] whose `request` matched the active
    /// [`RequestId`]; the request was cancelled and an
    /// [`Action::ReportError`] was emitted.
    pub timeouts_applied: Counter,
    /// [`Event::Timeout`] dropped because no request was active or
    /// the `request` id did not match the active one.
    pub timeouts_dropped_as_stale: Counter,
    /// [`Event::ToolCallDelta`] whose `request` matched the active
    /// [`RequestId`] and phase; the fragment was merged into the
    /// per-index tool-call slot.
    pub tool_call_deltas_appended: Counter,
    /// [`Event::ToolCallDelta`] dropped because no request was
    /// active, the id did not match, or the phase was not
    /// `Streaming` phase.
    pub tool_call_deltas_dropped_as_stale: Counter,
    /// A fragment was dropped because appending it would push the
    /// slot's accumulated `arguments` past [`ARGUMENTS_MAX_BYTES`].
    /// Counts every dropped fragment, not just the first one that
    /// tripped the limit.
    pub tool_call_arguments_fragments_dropped_over_limit: Counter,
    /// [`Event::ToolResult`] whose `request` matched the active
    /// [`RequestId`], phase was `ToolRunning` phase, and
    /// `call_id` matched an outstanding slot; the outcome was
    /// recorded.
    pub tool_results_committed: Counter,
    /// [`Event::ToolResult`] dropped because no request was active,
    /// the id did not match, the phase was not
    /// `ToolRunning` phase, or no outstanding call had the
    /// matching `call_id`.
    pub tool_results_dropped_as_stale: Counter,
    /// [`Action::ExecuteTool`] was emitted for a tool call (parsed
    /// invocation, within both the arguments-size and turn-count
    /// limits).
    pub tool_calls_executed: Counter,
    /// A tool call was resolved to a synthetic
    /// `Err(TurnToolCallLimitExceeded)` because the current user turn
    /// had already emitted [`TURN_TOOL_CALL_LIMIT`] executions.
    pub tool_calls_rejected_by_turn_limit: Counter,
    /// A tool call was resolved to a synthetic
    /// `Err(ArgumentsTooLarge)` because its accumulated arguments
    /// exceeded [`ARGUMENTS_MAX_BYTES`].
    pub tool_calls_rejected_by_arguments_limit: Counter,
    /// [`Action::PreviewPatch`] was emitted for a patch tool call
    /// (parsed successfully, within the shared turn/arguments limits).
    pub patch_calls_previewed: Counter,
    /// [`Event::PatchPreviewReady`] whose `call_id` matched an
    /// outstanding patch call; the diff summary and hashes were
    /// stored on the pending tool result approval bumped to `Pending`,
    /// and the phase transitioned to `AwaitingApproval`.
    pub patch_previews_committed: Counter,
    /// [`Event::PatchPreviewReady`] dropped because no request was
    /// active, the id did not match, no outstanding patch call had
    /// the matching `call_id`, or the call was already resolved.
    pub patch_previews_dropped_as_stale: Counter,
    /// [`Event::ApproveToolCall`] whose `call_id` matched an
    /// approval-pending call; the tool-specific apply/execute action
    /// was emitted.
    pub tool_call_approvals_committed: Counter,
    /// [`Event::ApproveToolCall`] dropped because no approval-pending
    /// call had the matching `call_id`.
    pub tool_call_approvals_dropped_as_stale: Counter,
    /// [`Event::RejectToolCall`] whose `call_id` matched an
    /// approval-pending call; a synthetic `Err(_::Rejected)` outcome
    /// was recorded for the target tool.
    pub tool_call_rejections_committed: Counter,
    /// [`Event::RejectToolCall`] dropped because no approval-pending
    /// call had the matching `call_id`.
    pub tool_call_rejections_dropped_as_stale: Counter,
    /// A command call was accepted by `on_finish` (parsed and added
    /// to the approval queue with `ApprovalState::Pending`).
    pub command_calls_dispatched: Counter,
    /// [`Action::ExecuteCommand`] was emitted for an approved
    /// command call.
    pub command_executions_started: Counter,
    /// [`Event::CommandOutputChunk`] whose `call_id` matched a
    /// running command; the chunk was folded into the tool result's
    /// output tail.
    pub command_output_chunks_appended: Counter,
    /// [`Event::CommandOutputChunk`] dropped because no request was
    /// active, the id did not match, or no running command call had
    /// the matching `call_id`.
    pub command_output_chunks_dropped_as_stale: Counter,
}

impl AgentCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a human-readable workspace root for display in the
    /// command approval prompt. Empty string means "not set" — the
    /// TUI falls back to omitting the cwd line in that case.
    pub fn set_workspace_display(&mut self, display: String) {
        self.workspace_display = display;
    }

    /// The committed user / assistant message history.
    pub fn conversation(&self) -> &[ChatMessage] {
        &self.conversation
    }

    /// The buffered response for the in-flight request, if any.
    pub fn pending_response(&self) -> Option<&PendingResponse> {
        self.pending.as_ref().map(|p| &p.response)
    }

    /// The ID of the in-flight request, if any.
    pub fn active_request(&self) -> Option<RequestId> {
        self.pending.as_ref().map(|p| p.id)
    }

    /// Call id of the first tool call awaiting user approval, if
    /// any. Returned as an owned `String` because callers typically
    /// need to move the id into an `Event`.
    pub fn pending_approval_call_id(&self) -> Option<String> {
        // Any tool call whose approval is `Pending` is ready to
        // accept the user's decision — patch bumps to `Pending` on
        // `PatchPreviewReady`, command bumps to `Pending` at
        // `on_finish`. No further per-tool gating is needed.
        self.pending
            .as_ref()?
            .tool_results
            .iter()
            .find(|r| r.approval == ApprovalState::Pending)
            .map(|r| r.call_id.clone())
    }

    /// Coarse runtime status suitable for a status line.
    pub fn status(&self) -> Status {
        self.status
    }

    /// Tool calls associated with the in-flight request, ordered by
    /// their streaming index so the TUI can render them stably.
    pub fn active_tool_calls(&self) -> Vec<ActiveToolCall> {
        let Some(pending) = self.pending.as_ref() else {
            return Vec::new();
        };
        match pending.phase {
            PendingPhase::Streaming => pending
                .tool_call_slots
                .iter()
                .map(|(index, slot)| ActiveToolCall {
                    call_id: slot
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("__pending_{index}")),
                    function_name: slot.function_name.clone().unwrap_or_default(),
                    arguments_json: slot.arguments.clone(),
                    outcome: None,
                    is_streaming: true,
                    approval: ApprovalState::NotRequired,
                    patch_preview: None,
                    preview_hashes: Vec::new(),
                    command_preview: None,
                    command_output_tail: None,
                })
                .collect(),
            PendingPhase::ToolRunning | PendingPhase::AwaitingApproval => pending
                .tool_results
                .iter()
                .map(|r| ActiveToolCall {
                    call_id: r.call_id.clone(),
                    function_name: r.function_name.clone(),
                    arguments_json: r.arguments_json.clone(),
                    outcome: r.outcome.clone(),
                    is_streaming: false,
                    approval: r.approval,
                    patch_preview: r.patch_preview.clone(),
                    preview_hashes: r.preview_hashes.clone(),
                    command_preview: r.command_preview.clone(),
                    command_output_tail: r.command_output_tail.clone(),
                })
                .collect(),
        }
    }

    /// Cumulative metrics for the branches taken by
    /// [`Self::handle_event`] over the lifetime of this instance.
    pub fn metrics(&self) -> &AgentMetrics {
        &self.metrics
    }

    /// Apply a single input event and return the resulting actions.
    pub fn handle_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::UserMessage(text) => self.on_user_message(text),
            Event::Cancel => self.on_cancel(),
            Event::ContentDelta { request, text } => self.on_content_delta(request, text),
            Event::ReasoningDelta { request, text } => self.on_reasoning_delta(request, text),
            Event::ToolCallDelta {
                request,
                index,
                id,
                function_name,
                arguments_fragment,
            } => self.on_tool_call_delta(request, index, id, function_name, arguments_fragment),
            Event::Finish { request, reason } => self.on_finish(request, reason),
            Event::ToolResult {
                request,
                call_id,
                outcome,
            } => self.on_tool_result(request, call_id, outcome),
            Event::PatchPreviewReady {
                request,
                call_id,
                preview_hashes,
                preview,
            } => self.on_patch_preview_ready(request, call_id, preview_hashes, preview),
            Event::ApproveToolCall { call_id } => self.on_approve_tool_call(call_id),
            Event::RejectToolCall { call_id } => self.on_reject_tool_call(call_id),
            Event::CommandOutputChunk {
                request,
                call_id,
                stream,
                bytes,
            } => self.on_command_output_chunk(request, call_id, stream, bytes),
            Event::TransportError { request, message } => self.on_transport_error(request, message),
            Event::Timeout { request } => self.on_timeout(request),
        }
    }

    fn on_user_message(&mut self, text: String) -> Vec<Action> {
        if self.pending.is_some() {
            self.metrics.user_messages_rejected_while_active.inc();
            return Vec::new();
        }
        self.conversation.push(ChatMessage::User(text));
        let id = self.mint_id();
        self.pending = Some(Pending {
            id,
            phase: PendingPhase::Streaming,
            response: PendingResponse::default(),
            tool_call_slots: BTreeMap::new(),
            tool_results: Vec::new(),
        });
        self.status = Status::AwaitingModel;
        self.tool_calls_this_turn = 0;
        self.metrics.user_messages_accepted.inc();
        vec![
            Action::StartRequest {
                id,
                messages: self.conversation.clone(),
            },
            Action::Redraw,
        ]
    }

    fn on_cancel(&mut self) -> Vec<Action> {
        let Some(pending) = self.pending.take() else {
            self.metrics.cancels_ignored_when_idle.inc();
            return Vec::new();
        };
        let cancel_action = match pending.phase {
            PendingPhase::Streaming => Action::CancelRequest { id: pending.id },
            PendingPhase::ToolRunning | PendingPhase::AwaitingApproval => {
                Action::CancelToolExecution {
                    request: pending.id,
                }
            }
        };
        self.status = Status::Idle;
        self.tool_calls_this_turn = 0;
        self.metrics.cancels_applied.inc();
        vec![cancel_action, Action::Redraw]
    }

    fn on_content_delta(&mut self, request: RequestId, text: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.content_deltas_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending.id != request || pending.phase != PendingPhase::Streaming {
            self.metrics.content_deltas_dropped_as_stale.inc();
            return Vec::new();
        }
        pending.response.content.push_str(&text);
        self.status = Status::Streaming;
        self.metrics.content_deltas_appended.inc();
        vec![Action::Redraw]
    }

    fn on_reasoning_delta(&mut self, request: RequestId, text: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.reasoning_deltas_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending.id != request || pending.phase != PendingPhase::Streaming {
            self.metrics.reasoning_deltas_dropped_as_stale.inc();
            return Vec::new();
        }
        pending.response.reasoning.push_str(&text);
        self.status = Status::Streaming;
        self.metrics.reasoning_deltas_appended.inc();
        vec![Action::Redraw]
    }

    fn on_tool_call_delta(
        &mut self,
        request: RequestId,
        index: u64,
        id: Option<String>,
        function_name: Option<String>,
        arguments_fragment: Option<String>,
    ) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.tool_call_deltas_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending.id != request || pending.phase != PendingPhase::Streaming {
            self.metrics.tool_call_deltas_dropped_as_stale.inc();
            return Vec::new();
        }
        let slot = pending
            .tool_call_slots
            .entry(index)
            .or_insert_with(|| ToolCallSlot {
                id: None,
                function_name: None,
                arguments: String::new(),
                over_limit: false,
            });
        if slot.id.is_none()
            && let Some(new_id) = id
        {
            slot.id = Some(new_id);
        }
        if slot.function_name.is_none()
            && let Some(new_name) = function_name
        {
            slot.function_name = Some(new_name);
        }
        if let Some(fragment) = arguments_fragment {
            if slot.over_limit {
                self.metrics
                    .tool_call_arguments_fragments_dropped_over_limit
                    .inc();
            } else if slot.arguments.len().saturating_add(fragment.len()) > ARGUMENTS_MAX_BYTES {
                slot.over_limit = true;
                self.metrics
                    .tool_call_arguments_fragments_dropped_over_limit
                    .inc();
            } else {
                slot.arguments.push_str(&fragment);
            }
        }
        self.status = Status::Streaming;
        self.metrics.tool_call_deltas_appended.inc();
        vec![Action::Redraw]
    }

    fn on_finish(&mut self, request: RequestId, reason: Option<String>) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            self.metrics.finishes_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending_ref.id != request || pending_ref.phase != PendingPhase::Streaming {
            self.metrics.finishes_dropped_as_stale.inc();
            return Vec::new();
        }
        let mut pending = self.pending.take().expect("checked above");
        pending.response.finish_reason = reason.clone();
        let reasoning_content = if pending.response.reasoning.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut pending.response.reasoning))
        };
        let content = std::mem::take(&mut pending.response.content);
        let is_tool_calls_reason = reason.as_deref() == Some("tool_calls");
        let has_slots = !pending.tool_call_slots.is_empty();

        if !is_tool_calls_reason || !has_slots {
            self.conversation.push(ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls: Vec::new(),
            });
            self.status = Status::Idle;
            self.tool_calls_this_turn = 0;
            self.metrics.finishes_committed.inc();
            return vec![Action::Redraw];
        }

        // Tool-calls finish: finalise slots, commit assistant with
        // tool_calls, and dispatch (or synthetically reject) each call.
        let request_id = pending.id;
        let (tool_calls, over_limit_ids) =
            finalize_tool_call_slots(std::mem::take(&mut pending.tool_call_slots));

        self.conversation.push(ChatMessage::Assistant {
            content,
            reasoning_content,
            tool_calls: tool_calls.clone(),
        });
        self.metrics.finishes_committed.inc();

        let mut actions = Vec::new();
        let mut pending_results: Vec<PendingToolResult> = Vec::with_capacity(tool_calls.len());
        for call in tool_calls.into_iter() {
            if over_limit_ids.contains(&call.id) {
                pending_results.push(synthetic_err_result(
                    call,
                    ToolExecutionError::ArgumentsTooLarge,
                ));
                self.metrics.tool_calls_rejected_by_arguments_limit.inc();
                continue;
            }
            if self.tool_calls_this_turn >= TURN_TOOL_CALL_LIMIT {
                pending_results.push(synthetic_err_result(
                    call,
                    ToolExecutionError::TurnToolCallLimitExceeded,
                ));
                self.metrics.tool_calls_rejected_by_turn_limit.inc();
                continue;
            }
            if call.function_name == "patch" {
                match PatchInvocation::parse(&call.arguments_json) {
                    Ok(invocation) => {
                        actions.push(Action::PreviewPatch {
                            request: request_id,
                            call_id: call.id.clone(),
                            invocation,
                        });
                        // Approval stays `NotRequired` until the
                        // preview arrives — user cannot see the diff
                        // yet, so exposing this call in the approval
                        // queue would prompt on an empty screen.
                        // `on_patch_preview_ready` bumps to `Pending`.
                        pending_results.push(patch_pending_result(call));
                        self.tool_calls_this_turn += 1;
                        self.metrics.patch_calls_previewed.inc();
                    }
                    Err(err) => {
                        pending_results.push(synthetic_err_result(call, err));
                    }
                }
            } else if call.function_name == "command" {
                match CommandInvocation::parse(&call.arguments_json) {
                    Ok(invocation) => {
                        let preview = CommandPreview {
                            argv: invocation.argv.clone(),
                            working_directory: self.workspace_display.clone(),
                        };
                        // No action emitted from on_finish; the shell
                        // waits for the user to approve. Action::ExecuteCommand
                        // will be produced by on_approve_tool_call.
                        pending_results.push(command_pending_result(call, preview));
                        self.tool_calls_this_turn += 1;
                        self.metrics.command_calls_dispatched.inc();
                    }
                    Err(err) => {
                        pending_results.push(synthetic_err_result(call, err));
                    }
                }
            } else {
                match ReadOnlyTool::parse(&call.function_name, &call.arguments_json) {
                    Ok(invocation) => {
                        actions.push(Action::ExecuteTool {
                            request: request_id,
                            call_id: call.id.clone(),
                            invocation,
                        });
                        pending_results.push(read_only_pending_result(call));
                        self.tool_calls_this_turn += 1;
                        self.metrics.tool_calls_executed.inc();
                    }
                    Err(err) => {
                        pending_results.push(synthetic_err_result(call, err));
                    }
                }
            }
        }

        let all_resolved = pending_results.iter().all(|r| r.outcome.is_some());
        if all_resolved {
            actions.extend(self.advance_to_next_request(pending_results));
            return actions;
        }

        pending.phase = PendingPhase::ToolRunning;
        pending.tool_results = pending_results;
        self.pending = Some(pending);
        self.status = Status::ToolRunning;
        // Command tool calls are already `Pending` approval as they
        // land here; bump the phase / status straight to
        // `AwaitingApproval` if any exists so the TUI does not have
        // to wait for a follow-up event before showing the prompt.
        self.recompute_phase_and_status();
        actions.push(Action::Redraw);
        actions
    }

    fn on_tool_result(
        &mut self,
        request: RequestId,
        call_id: String,
        outcome: ToolOutcome,
    ) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.tool_results_dropped_as_stale.inc();
            return Vec::new();
        };
        // Accept in both ToolRunning and AwaitingApproval phases so
        // read-only results can still land while a patch is waiting
        // for user approval.
        if pending.id != request || matches!(pending.phase, PendingPhase::Streaming) {
            self.metrics.tool_results_dropped_as_stale.inc();
            return Vec::new();
        }
        let Some(entry) = pending
            .tool_results
            .iter_mut()
            .find(|r| r.call_id == call_id && r.outcome.is_none())
        else {
            self.metrics.tool_results_dropped_as_stale.inc();
            return Vec::new();
        };
        entry.outcome = Some(outcome);
        self.metrics.tool_results_committed.inc();
        self.recompute_phase_and_status();

        self.maybe_advance_to_next_request()
    }

    fn on_patch_preview_ready(
        &mut self,
        request: RequestId,
        call_id: String,
        preview_hashes: Vec<PreviewHash>,
        preview: PatchPreview,
    ) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.patch_previews_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending.id != request || matches!(pending.phase, PendingPhase::Streaming) {
            self.metrics.patch_previews_dropped_as_stale.inc();
            return Vec::new();
        }
        let Some(entry) = pending.tool_results.iter_mut().find(|r| {
            r.call_id == call_id
                && r.approval == ApprovalState::NotRequired
                && r.outcome.is_none()
                && r.patch_preview.is_none()
        }) else {
            self.metrics.patch_previews_dropped_as_stale.inc();
            return Vec::new();
        };
        entry.patch_preview = Some(preview);
        entry.preview_hashes = preview_hashes;
        entry.approval = ApprovalState::Pending;
        self.metrics.patch_previews_committed.inc();
        self.recompute_phase_and_status();
        vec![Action::Redraw]
    }

    fn on_approve_tool_call(&mut self, call_id: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.tool_call_approvals_dropped_as_stale.inc();
            return Vec::new();
        };
        let request_id = pending.id;
        let Some(entry) = pending
            .tool_results
            .iter_mut()
            .find(|r| r.call_id == call_id && r.approval == ApprovalState::Pending)
        else {
            self.metrics.tool_call_approvals_dropped_as_stale.inc();
            return Vec::new();
        };
        entry.approval = ApprovalState::Approved;
        // Re-parse the arguments captured at on_finish. Parse cannot
        // fail here because on_finish already accepted it, but treat
        // an error as an immediate resolution to keep the loop
        // moving.
        let action = match entry.function_name.as_str() {
            "patch" => {
                let preview_hashes = entry.preview_hashes.clone();
                match PatchInvocation::parse(&entry.arguments_json) {
                    Ok(invocation) => Action::ApplyPatch {
                        request: request_id,
                        call_id: call_id.clone(),
                        invocation,
                        preview_hashes,
                    },
                    Err(err) => {
                        entry.outcome = Some(ToolOutcome::Err(err));
                        self.metrics.tool_call_approvals_committed.inc();
                        self.recompute_phase_and_status();
                        return self.maybe_advance_to_next_request();
                    }
                }
            }
            "command" => match CommandInvocation::parse(&entry.arguments_json) {
                Ok(invocation) => {
                    self.metrics.command_executions_started.inc();
                    Action::ExecuteCommand {
                        request: request_id,
                        call_id: call_id.clone(),
                        invocation,
                    }
                }
                Err(err) => {
                    entry.outcome = Some(ToolOutcome::Err(err));
                    self.metrics.tool_call_approvals_committed.inc();
                    self.recompute_phase_and_status();
                    return self.maybe_advance_to_next_request();
                }
            },
            other => {
                // Approval fired for a tool that never enters the
                // approval flow. Fail loudly through a synthetic Err.
                entry.outcome = Some(ToolOutcome::Err(ToolExecutionError::ArgumentsParseFailed(
                    format!("no approval flow for tool {other}"),
                )));
                self.metrics.tool_call_approvals_committed.inc();
                self.recompute_phase_and_status();
                return self.maybe_advance_to_next_request();
            }
        };
        self.metrics.tool_call_approvals_committed.inc();
        self.recompute_phase_and_status();
        vec![action, Action::Redraw]
    }

    fn on_reject_tool_call(&mut self, call_id: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.tool_call_rejections_dropped_as_stale.inc();
            return Vec::new();
        };
        let Some(entry) = pending
            .tool_results
            .iter_mut()
            .find(|r| r.call_id == call_id && r.approval == ApprovalState::Pending)
        else {
            self.metrics.tool_call_rejections_dropped_as_stale.inc();
            return Vec::new();
        };
        entry.approval = ApprovalState::Rejected;
        entry.outcome = Some(match entry.function_name.as_str() {
            "command" => ToolOutcome::Err(ToolExecutionError::Command(CommandError::Rejected)),
            // Patch is the only other tool that reaches this path; any
            // future approval-gated tool falls through to Patch::Rejected
            // by default, which is close enough for the model to see it
            // as "user did not approve".
            _ => ToolOutcome::Err(ToolExecutionError::Patch(PatchError::Rejected)),
        });
        self.metrics.tool_call_rejections_committed.inc();
        self.recompute_phase_and_status();
        self.maybe_advance_to_next_request()
    }

    fn on_command_output_chunk(
        &mut self,
        request: RequestId,
        call_id: String,
        stream: CommandOutputStream,
        bytes: Vec<u8>,
    ) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            self.metrics.command_output_chunks_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending.id != request || matches!(pending.phase, PendingPhase::Streaming) {
            self.metrics.command_output_chunks_dropped_as_stale.inc();
            return Vec::new();
        }
        let Some(entry) = pending.tool_results.iter_mut().find(|r| {
            r.call_id == call_id
                && r.approval == ApprovalState::Approved
                && r.outcome.is_none()
                && r.function_name == "command"
        }) else {
            self.metrics.command_output_chunks_dropped_as_stale.inc();
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&bytes);
        let byte_len = bytes.len() as u64;
        let tail = entry
            .command_output_tail
            .get_or_insert_with(CommandOutputTail::default);
        match stream {
            CommandOutputStream::Stdout => {
                append_bounded(&mut tail.stdout_tail, &text, COMMAND_TAIL_CHARS);
                tail.stdout_bytes_total = tail.stdout_bytes_total.saturating_add(byte_len);
            }
            CommandOutputStream::Stderr => {
                append_bounded(&mut tail.stderr_tail, &text, COMMAND_TAIL_CHARS);
                tail.stderr_bytes_total = tail.stderr_bytes_total.saturating_add(byte_len);
            }
        }
        self.metrics.command_output_chunks_appended.inc();
        vec![Action::Redraw]
    }

    /// After any state change to `tool_results`, adjust `pending.phase`
    /// and `self.status` to match: any Pending patch keeps us in
    /// `AwaitingApproval`, otherwise back to `ToolRunning`.
    fn recompute_phase_and_status(&mut self) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        // A patch is only "still waiting for approval" while its
        // outcome has not been committed. If the shell fails preview
        // and short-circuits to Err via ToolResult, the call is done
        // regardless of the `approval` field.
        let has_pending_approval = pending
            .tool_results
            .iter()
            .any(|r| r.approval == ApprovalState::Pending && r.outcome.is_none());
        match (pending.phase, has_pending_approval) {
            (PendingPhase::Streaming, _) => {}
            (_, true) => {
                pending.phase = PendingPhase::AwaitingApproval;
                self.status = Status::AwaitingApproval;
            }
            (_, false) => {
                pending.phase = PendingPhase::ToolRunning;
                self.status = Status::ToolRunning;
            }
        }
    }

    /// If every tool result is resolved, commit them and start the
    /// follow-up request; otherwise emit a redraw so the TUI reflects
    /// the state change.
    fn maybe_advance_to_next_request(&mut self) -> Vec<Action> {
        let Some(pending) = self.pending.as_ref() else {
            return Vec::new();
        };
        if pending.tool_results.iter().all(|r| r.outcome.is_some()) {
            let pending = self.pending.take().expect("checked above");
            self.advance_to_next_request(pending.tool_results)
        } else {
            vec![Action::Redraw]
        }
    }

    fn on_transport_error(&mut self, request: RequestId, message: String) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            self.metrics.transport_errors_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending_ref.id != request || pending_ref.phase != PendingPhase::Streaming {
            self.metrics.transport_errors_dropped_as_stale.inc();
            return Vec::new();
        }
        self.pending = None;
        self.status = Status::Idle;
        self.tool_calls_this_turn = 0;
        self.metrics.transport_errors_recorded.inc();
        vec![Action::ReportError { message }, Action::Redraw]
    }

    fn on_timeout(&mut self, request: RequestId) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            self.metrics.timeouts_dropped_as_stale.inc();
            return Vec::new();
        };
        if pending_ref.id != request || pending_ref.phase != PendingPhase::Streaming {
            self.metrics.timeouts_dropped_as_stale.inc();
            return Vec::new();
        }
        let id = pending_ref.id;
        self.pending = None;
        self.status = Status::Idle;
        self.tool_calls_this_turn = 0;
        self.metrics.timeouts_applied.inc();
        vec![
            Action::CancelRequest { id },
            Action::ReportError {
                message: "request timed out".to_string(),
            },
            Action::Redraw,
        ]
    }

    fn mint_id(&mut self) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// All tool results in the just-finished turn's tool loop are in;
    /// commit them as `Tool` role messages and issue a fresh
    /// [`Action::StartRequest`] under a new [`RequestId`] so the model
    /// can consume them.
    fn advance_to_next_request(&mut self, results: Vec<PendingToolResult>) -> Vec<Action> {
        for result in results {
            let outcome = result
                .outcome
                .expect("advance_to_next_request called with unresolved tool result");
            let content = match outcome {
                ToolOutcome::Ok(s) => s,
                ToolOutcome::Err(err) => err.to_json_string(),
            };
            self.conversation.push(ChatMessage::Tool {
                tool_call_id: result.call_id,
                content,
            });
        }
        let id = self.mint_id();
        self.pending = Some(Pending {
            id,
            phase: PendingPhase::Streaming,
            response: PendingResponse::default(),
            tool_call_slots: BTreeMap::new(),
            tool_results: Vec::new(),
        });
        self.status = Status::AwaitingModel;
        vec![
            Action::StartRequest {
                id,
                messages: self.conversation.clone(),
            },
            Action::Redraw,
        ]
    }
}

fn synthetic_err_result(call: ToolCall, err: ToolExecutionError) -> PendingToolResult {
    PendingToolResult {
        call_id: call.id,
        function_name: call.function_name,
        arguments_json: call.arguments_json,
        approval: ApprovalState::NotRequired,
        patch_preview: None,
        preview_hashes: Vec::new(),
        command_preview: None,
        command_output_tail: None,
        outcome: Some(ToolOutcome::Err(err)),
    }
}

fn read_only_pending_result(call: ToolCall) -> PendingToolResult {
    PendingToolResult {
        call_id: call.id,
        function_name: call.function_name,
        arguments_json: call.arguments_json,
        approval: ApprovalState::NotRequired,
        patch_preview: None,
        preview_hashes: Vec::new(),
        command_preview: None,
        command_output_tail: None,
        outcome: None,
    }
}

fn patch_pending_result(call: ToolCall) -> PendingToolResult {
    PendingToolResult {
        call_id: call.id,
        function_name: call.function_name,
        arguments_json: call.arguments_json,
        approval: ApprovalState::NotRequired,
        patch_preview: None,
        preview_hashes: Vec::new(),
        command_preview: None,
        command_output_tail: None,
        outcome: None,
    }
}

/// Append `text` to `tail` while keeping `tail.chars().count()` at
/// most `max_chars`. Older content is dropped from the front so the
/// most recent output stays visible.
fn append_bounded(tail: &mut String, text: &str, max_chars: usize) {
    tail.push_str(text);
    let count = tail.chars().count();
    if count > max_chars {
        let drop = count - max_chars;
        let split = tail
            .char_indices()
            .nth(drop)
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        tail.drain(..split);
    }
}

fn command_pending_result(call: ToolCall, preview: CommandPreview) -> PendingToolResult {
    PendingToolResult {
        call_id: call.id,
        function_name: call.function_name,
        arguments_json: call.arguments_json,
        approval: ApprovalState::Pending,
        patch_preview: None,
        preview_hashes: Vec::new(),
        command_preview: Some(preview),
        command_output_tail: None,
        outcome: None,
    }
}

/// Convert per-index streaming slots into the ordered [`ToolCall`] list
/// committed to the assistant turn. Returns the calls plus the set of
/// `id`s that were flagged `over_limit` during streaming and should be
/// resolved to `Err(ArgumentsTooLarge)` instead of being executed.
fn finalize_tool_call_slots(
    slots: BTreeMap<u64, ToolCallSlot>,
) -> (Vec<ToolCall>, std::collections::HashSet<String>) {
    let mut calls = Vec::with_capacity(slots.len());
    let mut over_limit = std::collections::HashSet::new();
    for (index, slot) in slots.into_iter() {
        let id = slot.id.unwrap_or_else(|| format!("__missing_id__{index}"));
        let function_name = slot.function_name.unwrap_or_default();
        if slot.over_limit {
            over_limit.insert(id.clone());
        }
        calls.push(ToolCall {
            id,
            function_name,
            arguments_json: slot.arguments,
        });
    }
    (calls, over_limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(core: &mut AgentCore, text: &str) -> Vec<Action> {
        core.handle_event(Event::UserMessage(text.to_string()))
    }

    fn last_start_id(actions: &[Action]) -> RequestId {
        for action in actions {
            if let Action::StartRequest { id, .. } = action {
                return *id;
            }
        }
        panic!("no StartRequest in {actions:?}");
    }

    #[test]
    fn user_message_from_idle_starts_request_and_appends_user_turn() {
        let mut core = AgentCore::new();
        let actions = user(&mut core, "hello");
        assert_eq!(core.status(), Status::AwaitingModel);
        assert_eq!(core.conversation().len(), 1);
        assert!(matches!(core.conversation()[0], ChatMessage::User(_)));
        assert!(matches!(actions[0], Action::StartRequest { .. }));
        assert!(actions.contains(&Action::Redraw));
        assert!(core.active_request().is_some());
    }

    #[test]
    fn user_message_while_active_is_ignored() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "first");
        let actions = user(&mut core, "second");
        assert!(actions.is_empty());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn content_delta_accumulates_and_flips_status_to_streaming() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = core.handle_event(Event::ContentDelta {
            request: id,
            text: "he".to_string(),
        });
        assert_eq!(actions, vec![Action::Redraw]);
        assert_eq!(core.status(), Status::Streaming);
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "llo".to_string(),
        });
        let pending = core.pending_response().expect("pending");
        assert_eq!(pending.content, "hello");
    }

    #[test]
    fn reasoning_delta_accumulates_separately_from_content() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ReasoningDelta {
            request: id,
            text: "let me think".to_string(),
        });
        let pending = core.pending_response().expect("pending");
        assert_eq!(pending.reasoning, "let me think");
        assert!(pending.content.is_empty());
    }

    #[test]
    fn finish_commits_assistant_turn_and_returns_to_idle() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "hello".to_string(),
        });
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("stop".to_string()),
        });
        assert_eq!(actions, vec![Action::Redraw]);
        assert_eq!(core.status(), Status::Idle);
        assert!(core.active_request().is_none());
        assert!(core.pending_response().is_none());
        assert_eq!(core.conversation().len(), 2);
        match &core.conversation()[1] {
            ChatMessage::Assistant { content, .. } => assert_eq!(content, "hello"),
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn cancel_drops_pending_response_and_asks_transport_to_cancel() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "partial".to_string(),
        });
        let actions = core.handle_event(Event::Cancel);
        assert!(actions.contains(&Action::CancelRequest { id }));
        assert!(actions.contains(&Action::Redraw));
        assert_eq!(core.status(), Status::Idle);
        assert!(core.pending_response().is_none());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn cancel_from_idle_is_no_op() {
        let mut core = AgentCore::new();
        assert!(core.handle_event(Event::Cancel).is_empty());
    }

    #[test]
    fn transport_error_ends_request_and_reports_message() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = core.handle_event(Event::TransportError {
            request: id,
            message: "boom".to_string(),
        });
        assert!(actions.contains(&Action::ReportError {
            message: "boom".to_string(),
        }));
        assert!(actions.contains(&Action::Redraw));
        assert_eq!(core.status(), Status::Idle);
        assert!(core.pending_response().is_none());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn timeout_cancels_transport_and_reports_error() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = core.handle_event(Event::Timeout { request: id });
        assert!(actions.contains(&Action::CancelRequest { id }));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::ReportError { .. })),
            "expected ReportError, got {actions:?}",
        );
        assert!(actions.contains(&Action::Redraw));
        assert!(core.pending_response().is_none());
    }

    #[test]
    fn stale_deltas_are_dropped_without_state_change() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: Some("stop".to_string()),
        });
        // Late delta from the finished request.
        let actions = core.handle_event(Event::ContentDelta {
            request: id,
            text: "late".to_string(),
        });
        assert!(actions.is_empty());
        assert_eq!(core.conversation().len(), 2);
    }

    #[test]
    fn events_tagged_with_unknown_id_are_dropped() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "hi");
        let actions = core.handle_event(Event::ContentDelta {
            request: RequestId::new(u64::MAX),
            text: "x".to_string(),
        });
        assert!(actions.is_empty());
        assert!(core.pending_response().expect("pending").content.is_empty());
    }

    #[test]
    fn each_start_request_gets_a_unique_id() {
        let mut core = AgentCore::new();
        let id1 = last_start_id(&user(&mut core, "first"));
        let _ = core.handle_event(Event::Finish {
            request: id1,
            reason: None,
        });
        let id2 = last_start_id(&user(&mut core, "second"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn late_finish_after_cancel_is_ignored() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Cancel);
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("stop".to_string()),
        });
        assert!(actions.is_empty());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn start_request_carries_full_conversation_snapshot() {
        let mut core = AgentCore::new();
        let id1 = last_start_id(&user(&mut core, "first"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id1,
            text: "one".to_string(),
        });
        let _ = core.handle_event(Event::Finish {
            request: id1,
            reason: None,
        });
        let actions = user(&mut core, "second");
        let messages = actions.iter().find_map(|a| match a {
            Action::StartRequest { messages, .. } => Some(messages.clone()),
            _ => None,
        });
        let messages = messages.expect("StartRequest present");
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[0], ChatMessage::User(_)));
        assert!(matches!(messages[1], ChatMessage::Assistant { .. }));
        match &messages[2] {
            ChatMessage::User(content) => assert_eq!(content, "second"),
            other => panic!("expected user, got {other:?}"),
        }
    }

    // -------------------------------------------------------------
    // metrics
    // -------------------------------------------------------------

    #[test]
    fn metrics_start_at_zero() {
        let core = AgentCore::new();
        assert_eq!(*core.metrics(), AgentMetrics::default());
    }

    #[test]
    fn user_message_accepted_and_rejected_counters() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "first");
        assert_eq!(core.metrics().user_messages_accepted.get(), 1);
        assert_eq!(core.metrics().user_messages_rejected_while_active.get(), 0);
        // Second user message while first is still active is rejected.
        let _ = user(&mut core, "second");
        assert_eq!(core.metrics().user_messages_accepted.get(), 1);
        assert_eq!(core.metrics().user_messages_rejected_while_active.get(), 1);
    }

    #[test]
    fn cancel_applied_and_ignored_counters() {
        let mut core = AgentCore::new();
        let _ = core.handle_event(Event::Cancel);
        assert_eq!(core.metrics().cancels_applied.get(), 0);
        assert_eq!(core.metrics().cancels_ignored_when_idle.get(), 1);
        let _ = user(&mut core, "hi");
        let _ = core.handle_event(Event::Cancel);
        assert_eq!(core.metrics().cancels_applied.get(), 1);
        assert_eq!(core.metrics().cancels_ignored_when_idle.get(), 1);
    }

    #[test]
    fn content_delta_appended_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "a".to_string(),
        });
        let _ = core.handle_event(Event::ContentDelta {
            request: RequestId::new(u64::MAX),
            text: "b".to_string(),
        });
        assert_eq!(core.metrics().content_deltas_appended.get(), 1);
        assert_eq!(core.metrics().content_deltas_dropped_as_stale.get(), 1);
    }

    #[test]
    fn reasoning_delta_appended_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ReasoningDelta {
            request: id,
            text: "think".to_string(),
        });
        // No pending: dropped
        let _ = core.handle_event(Event::Cancel);
        let _ = core.handle_event(Event::ReasoningDelta {
            request: id,
            text: "late".to_string(),
        });
        assert_eq!(core.metrics().reasoning_deltas_appended.get(), 1);
        assert_eq!(core.metrics().reasoning_deltas_dropped_as_stale.get(), 1);
    }

    #[test]
    fn finish_committed_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: None,
        });
        // Second finish for the (now completed) request is stale.
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: None,
        });
        assert_eq!(core.metrics().finishes_committed.get(), 1);
        assert_eq!(core.metrics().finishes_dropped_as_stale.get(), 1);
    }

    #[test]
    fn transport_error_recorded_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::TransportError {
            request: id,
            message: "boom".to_string(),
        });
        // No pending: dropped
        let _ = core.handle_event(Event::TransportError {
            request: id,
            message: "late".to_string(),
        });
        assert_eq!(core.metrics().transport_errors_recorded.get(), 1);
        assert_eq!(core.metrics().transport_errors_dropped_as_stale.get(), 1);
    }

    #[test]
    fn timeout_applied_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Timeout { request: id });
        // No pending: dropped
        let _ = core.handle_event(Event::Timeout { request: id });
        assert_eq!(core.metrics().timeouts_applied.get(), 1);
        assert_eq!(core.metrics().timeouts_dropped_as_stale.get(), 1);
    }

    #[test]
    fn other_counters_do_not_move_on_a_single_event() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "hi");
        let m = core.metrics();
        assert_eq!(m.user_messages_accepted.get(), 1);
        // Every other counter is zero.
        assert_eq!(m.user_messages_rejected_while_active.get(), 0);
        assert_eq!(m.cancels_applied.get(), 0);
        assert_eq!(m.cancels_ignored_when_idle.get(), 0);
        assert_eq!(m.content_deltas_appended.get(), 0);
        assert_eq!(m.content_deltas_dropped_as_stale.get(), 0);
        assert_eq!(m.reasoning_deltas_appended.get(), 0);
        assert_eq!(m.reasoning_deltas_dropped_as_stale.get(), 0);
        assert_eq!(m.finishes_committed.get(), 0);
        assert_eq!(m.finishes_dropped_as_stale.get(), 0);
        assert_eq!(m.transport_errors_recorded.get(), 0);
        assert_eq!(m.transport_errors_dropped_as_stale.get(), 0);
        assert_eq!(m.timeouts_applied.get(), 0);
        assert_eq!(m.timeouts_dropped_as_stale.get(), 0);
        assert_eq!(m.tool_call_deltas_appended.get(), 0);
        assert_eq!(m.tool_call_deltas_dropped_as_stale.get(), 0);
        assert_eq!(m.tool_call_arguments_fragments_dropped_over_limit.get(), 0);
        assert_eq!(m.tool_results_committed.get(), 0);
        assert_eq!(m.tool_results_dropped_as_stale.get(), 0);
        assert_eq!(m.tool_calls_executed.get(), 0);
        assert_eq!(m.tool_calls_rejected_by_turn_limit.get(), 0);
        assert_eq!(m.tool_calls_rejected_by_arguments_limit.get(), 0);
        assert_eq!(m.patch_calls_previewed.get(), 0);
        assert_eq!(m.patch_previews_committed.get(), 0);
        assert_eq!(m.patch_previews_dropped_as_stale.get(), 0);
        assert_eq!(m.tool_call_approvals_committed.get(), 0);
        assert_eq!(m.tool_call_approvals_dropped_as_stale.get(), 0);
        assert_eq!(m.tool_call_rejections_committed.get(), 0);
        assert_eq!(m.tool_call_rejections_dropped_as_stale.get(), 0);
        assert_eq!(m.command_calls_dispatched.get(), 0);
        assert_eq!(m.command_executions_started.get(), 0);
        assert_eq!(m.command_output_chunks_appended.get(), 0);
        assert_eq!(m.command_output_chunks_dropped_as_stale.get(), 0);
    }

    // -------------------------------------------------------------
    // tool loop
    // -------------------------------------------------------------

    fn tool_call_delta(
        request: RequestId,
        index: u64,
        id: Option<&str>,
        function_name: Option<&str>,
        arguments_fragment: Option<&str>,
    ) -> Event {
        Event::ToolCallDelta {
            request,
            index,
            id: id.map(str::to_string),
            function_name: function_name.map(str::to_string),
            arguments_fragment: arguments_fragment.map(str::to_string),
        }
    }

    fn drive_single_tool_call(
        core: &mut AgentCore,
        request: RequestId,
        call_id: &str,
        function_name: &str,
        arguments_json: &str,
    ) -> Vec<Action> {
        let _ = core.handle_event(tool_call_delta(
            request,
            0,
            Some(call_id),
            Some(function_name),
            Some(arguments_json),
        ));
        core.handle_event(Event::Finish {
            request,
            reason: Some("tool_calls".to_string()),
        })
    }

    #[test]
    fn tool_call_delta_merges_fragments_and_emits_execute_tool_on_finish() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(tool_call_delta(
            id,
            0,
            Some("call_1"),
            Some("read"),
            Some(r#"{"path":""#),
        ));
        let _ = core.handle_event(tool_call_delta(id, 0, None, None, Some(r#"src/main.rs"}"#)));
        assert_eq!(core.metrics().tool_call_deltas_appended.get(), 2);
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("tool_calls".to_string()),
        });
        assert_eq!(core.status(), Status::ToolRunning);
        assert_eq!(core.metrics().tool_calls_executed.get(), 1);
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::ExecuteTool {
                call_id, invocation: ReadOnlyTool::Read { path, .. }, ..
            } if call_id == "call_1" && path == "src/main.rs"
        )));
        // Assistant turn was committed with the tool_calls attached.
        match &core.conversation()[1] {
            ChatMessage::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].function_name, "read");
            }
            other => panic!("expected assistant with tool_calls, got {other:?}"),
        }
    }

    #[test]
    fn multiple_parallel_tool_calls_are_ordered_by_index() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        // Arrive out of order.
        let _ = core.handle_event(tool_call_delta(
            id,
            1,
            Some("b"),
            Some("list"),
            Some(r#"{"path":"src"}"#),
        ));
        let _ = core.handle_event(tool_call_delta(
            id,
            0,
            Some("a"),
            Some("read"),
            Some(r#"{"path":"README.md"}"#),
        ));
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("tool_calls".to_string()),
        });
        let execute_ids: Vec<String> = actions
            .iter()
            .filter_map(|a| match a {
                Action::ExecuteTool { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(execute_ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn tool_result_completes_slot_and_advances_to_next_request() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_tool_call(&mut core, id, "call_1", "list", r#"{"path":"."}"#);
        assert_eq!(core.status(), Status::ToolRunning);
        let actions = core.handle_event(Event::ToolResult {
            request: id,
            call_id: "call_1".to_string(),
            outcome: ToolOutcome::Ok(r#"[{"path":"a"}]"#.to_string()),
        });
        assert_eq!(core.metrics().tool_results_committed.get(), 1);
        // New request started with a fresh id, conversation now has
        // user + assistant(tool_calls) + tool + <no assistant yet>.
        let start = actions
            .iter()
            .find_map(|a| match a {
                Action::StartRequest { id, messages } => Some((*id, messages.clone())),
                _ => None,
            })
            .expect("StartRequest emitted");
        assert_ne!(start.0, id);
        assert_eq!(start.1.len(), 3);
        assert!(matches!(start.1[2], ChatMessage::Tool { .. }));
        assert_eq!(core.status(), Status::AwaitingModel);
    }

    #[test]
    fn tool_result_before_all_arrive_stays_in_tool_running() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(tool_call_delta(
            id,
            0,
            Some("a"),
            Some("read"),
            Some(r#"{"path":"a"}"#),
        ));
        let _ = core.handle_event(tool_call_delta(
            id,
            1,
            Some("b"),
            Some("read"),
            Some(r#"{"path":"b"}"#),
        ));
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: Some("tool_calls".to_string()),
        });
        let actions = core.handle_event(Event::ToolResult {
            request: id,
            call_id: "a".to_string(),
            outcome: ToolOutcome::Ok("ok".to_string()),
        });
        assert_eq!(core.status(), Status::ToolRunning);
        assert_eq!(actions, vec![Action::Redraw]);
    }

    #[test]
    fn tool_result_with_unknown_call_id_is_dropped_as_stale() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_tool_call(&mut core, id, "call_1", "list", r#"{"path":"."}"#);
        let actions = core.handle_event(Event::ToolResult {
            request: id,
            call_id: "does_not_exist".to_string(),
            outcome: ToolOutcome::Ok("ok".to_string()),
        });
        assert!(actions.is_empty());
        assert_eq!(core.metrics().tool_results_dropped_as_stale.get(), 1);
    }

    #[test]
    fn arguments_over_limit_yields_synthetic_arguments_too_large_err() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(tool_call_delta(
            id,
            0,
            Some("call_big"),
            Some("read"),
            Some(&"x".repeat(ARGUMENTS_MAX_BYTES + 1)),
        ));
        assert_eq!(
            core.metrics()
                .tool_call_arguments_fragments_dropped_over_limit
                .get(),
            1
        );
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("tool_calls".to_string()),
        });
        assert_eq!(
            core.metrics().tool_calls_rejected_by_arguments_limit.get(),
            1
        );
        assert_eq!(core.metrics().tool_calls_executed.get(), 0);
        // The synthetic Err resolves all results immediately; a new
        // StartRequest must have been emitted with the Tool message
        // carrying the arguments_too_large payload.
        let messages = actions
            .iter()
            .find_map(|a| match a {
                Action::StartRequest { messages, .. } => Some(messages.clone()),
                _ => None,
            })
            .expect("StartRequest emitted");
        let tool_msg = messages
            .iter()
            .find_map(|m| match m {
                ChatMessage::Tool { content, .. } => Some(content),
                _ => None,
            })
            .expect("Tool message present");
        assert!(tool_msg.contains("arguments_too_large"));
    }

    #[test]
    fn turn_tool_call_limit_forces_synthetic_err_for_extras() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        // 21 tool calls, indices 0..=20. Limit is 20.
        for i in 0..=TURN_TOOL_CALL_LIMIT as u64 {
            let call_id = format!("c{i}");
            let _ = core.handle_event(tool_call_delta(
                id,
                i,
                Some(&call_id),
                Some("list"),
                Some(r#"{"path":"."}"#),
            ));
        }
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: Some("tool_calls".to_string()),
        });
        assert_eq!(
            core.metrics().tool_calls_executed.get(),
            TURN_TOOL_CALL_LIMIT as u64
        );
        assert_eq!(core.metrics().tool_calls_rejected_by_turn_limit.get(), 1);
    }

    #[test]
    fn unknown_function_name_yields_synthetic_unknown_tool_err() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = drive_single_tool_call(&mut core, id, "call_1", "bogus", r#"{}"#);
        // Since it's synthetic Err (all resolved), next StartRequest is emitted.
        let messages = actions
            .iter()
            .find_map(|a| match a {
                Action::StartRequest { messages, .. } => Some(messages.clone()),
                _ => None,
            })
            .expect("StartRequest emitted");
        let tool_content = messages
            .iter()
            .find_map(|m| match m {
                ChatMessage::Tool { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("Tool message present");
        assert!(tool_content.contains("unknown_tool"));
        assert_eq!(core.metrics().tool_calls_executed.get(), 0);
    }

    #[test]
    fn cancel_in_tool_running_emits_cancel_tool_execution() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_tool_call(&mut core, id, "call_1", "list", r#"{"path":"."}"#);
        assert_eq!(core.status(), Status::ToolRunning);
        let actions = core.handle_event(Event::Cancel);
        assert!(actions.contains(&Action::CancelToolExecution { request: id }));
        assert_eq!(core.status(), Status::Idle);
        assert!(core.pending_response().is_none());
    }

    #[test]
    fn transport_error_dropped_in_tool_running_phase() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_tool_call(&mut core, id, "call_1", "list", r#"{"path":"."}"#);
        let actions = core.handle_event(Event::TransportError {
            request: id,
            message: "should be ignored".to_string(),
        });
        assert!(actions.is_empty());
        assert_eq!(core.metrics().transport_errors_dropped_as_stale.get(), 1);
        assert_eq!(core.status(), Status::ToolRunning);
    }

    #[test]
    fn tool_result_dropped_in_streaming_phase() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        // We are still in Streaming phase — no tool_calls finish yet.
        let actions = core.handle_event(Event::ToolResult {
            request: id,
            call_id: "call_1".to_string(),
            outcome: ToolOutcome::Ok("x".to_string()),
        });
        assert!(actions.is_empty());
        assert_eq!(core.metrics().tool_results_dropped_as_stale.get(), 1);
    }

    #[test]
    fn tool_calls_this_turn_resets_between_user_turns() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_tool_call(&mut core, id, "call_1", "list", r#"{"path":"."}"#);
        // Provide result to advance out of tool loop; also emit a
        // final stop-finish so we return to Idle.
        let next_id = {
            let actions = core.handle_event(Event::ToolResult {
                request: id,
                call_id: "call_1".to_string(),
                outcome: ToolOutcome::Ok("ok".to_string()),
            });
            last_start_id(&actions)
        };
        let _ = core.handle_event(Event::Finish {
            request: next_id,
            reason: Some("stop".to_string()),
        });
        assert_eq!(core.status(), Status::Idle);
        // A brand-new user turn should reset the counter and be able
        // to emit up to TURN_TOOL_CALL_LIMIT executions again.
        let id2 = last_start_id(&user(&mut core, "second"));
        let _ = drive_single_tool_call(&mut core, id2, "c2", "list", r#"{"path":"."}"#);
        assert_eq!(core.metrics().tool_calls_executed.get(), 2);
        assert_eq!(core.metrics().tool_calls_rejected_by_turn_limit.get(), 0);
    }

    #[test]
    fn finish_with_tool_calls_reason_but_no_slots_falls_through_to_idle() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("tool_calls".to_string()),
        });
        assert_eq!(actions, vec![Action::Redraw]);
        assert_eq!(core.status(), Status::Idle);
        assert_eq!(core.conversation().len(), 2);
    }

    #[test]
    fn readonly_tool_definitions_expose_the_three_tools() {
        let defs = ReadOnlyTool::definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["list", "read", "search"]);
    }

    #[test]
    fn readonly_tool_parse_list_defaults() {
        let tool = ReadOnlyTool::parse("list", r#"{"path":"src"}"#).expect("parses");
        assert_eq!(
            tool,
            ReadOnlyTool::List {
                path: "src".to_string(),
                recursive: false,
                max_entries: DEFAULT_LIST_MAX_ENTRIES,
                include_hidden: false,
            }
        );
    }

    #[test]
    fn readonly_tool_parse_read_with_line_range() {
        let tool = ReadOnlyTool::parse("read", r#"{"path":"src/main.rs","line_range":[10,20]}"#)
            .expect("parses");
        assert_eq!(
            tool,
            ReadOnlyTool::Read {
                path: "src/main.rs".to_string(),
                line_range: Some((10, 20)),
            }
        );
    }

    #[test]
    fn readonly_tool_parse_search_with_defaults() {
        let tool = ReadOnlyTool::parse("search", r#"{"pattern":"TODO"}"#).expect("parses");
        assert_eq!(
            tool,
            ReadOnlyTool::Search {
                pattern: "TODO".to_string(),
                path_prefix: None,
                case_sensitive: false,
                max_results: DEFAULT_SEARCH_MAX_RESULTS,
            }
        );
    }

    #[test]
    fn readonly_tool_parse_unknown_function_name() {
        let err = ReadOnlyTool::parse("foo", "{}").expect_err("unknown");
        assert_eq!(err, ToolExecutionError::UnknownTool);
    }

    #[test]
    fn readonly_tool_parse_invalid_json_is_reported() {
        let err = ReadOnlyTool::parse("list", "not-json").expect_err("invalid");
        assert!(matches!(err, ToolExecutionError::ArgumentsParseFailed(_)));
    }

    #[test]
    fn tool_execution_error_to_json_includes_code_and_message() {
        let s = ToolExecutionError::OutsideWorkspace.to_json_string();
        assert!(s.contains(r#""error":"outside_workspace""#));
        assert!(s.contains(r#""message""#));
    }

    // -------------------------------------------------------------
    // PatchInvocation
    // -------------------------------------------------------------

    #[test]
    fn patch_definition_advertises_the_patch_function_name() {
        let def = PatchInvocation::definition();
        assert_eq!(def.name, "patch");
        assert!(def.description.contains("approval"));
        assert!(def.parameters_json.contains("edits"));
    }

    #[test]
    fn patch_parse_add_and_update_edits() {
        let inv = PatchInvocation::parse(
            r#"{"edits":[
                {"kind":"add","path":"a.txt","content":"hello"},
                {"kind":"update","path":"b.txt","before":"foo","after":"bar"}
            ]}"#,
        )
        .expect("parses");
        assert_eq!(inv.edits.len(), 2);
        assert_eq!(
            inv.edits[0],
            PatchTool::Add {
                path: "a.txt".to_string(),
                content: "hello".to_string(),
            }
        );
        assert_eq!(
            inv.edits[1],
            PatchTool::Update {
                path: "b.txt".to_string(),
                before: "foo".to_string(),
                after: "bar".to_string(),
            }
        );
    }

    #[test]
    fn patch_parse_rejects_same_path_twice() {
        let err = PatchInvocation::parse(
            r#"{"edits":[
                {"kind":"update","path":"dup","before":"a","after":"b"},
                {"kind":"update","path":"dup","before":"c","after":"d"}
            ]}"#,
        )
        .expect_err("same-path rejected");
        assert_eq!(
            err,
            ToolExecutionError::Patch(PatchError::MultipleEditsSamePath {
                path: "dup".to_string(),
            })
        );
    }

    #[test]
    fn patch_parse_rejects_empty_edits() {
        let err = PatchInvocation::parse(r#"{"edits":[]}"#).expect_err("empty rejected");
        assert!(matches!(err, ToolExecutionError::ArgumentsParseFailed(_)));
    }

    #[test]
    fn patch_parse_rejects_too_many_edits() {
        let mut edits = String::from("[");
        for i in 0..(PATCH_MAX_EDITS + 1) {
            if i > 0 {
                edits.push(',');
            }
            edits.push_str(&format!(r#"{{"kind":"add","path":"f{i}","content":""}}"#));
        }
        edits.push(']');
        let json = format!(r#"{{"edits":{edits}}}"#);
        let err = PatchInvocation::parse(&json).expect_err("too many rejected");
        assert!(matches!(
            err,
            ToolExecutionError::Patch(PatchError::TooManyEdits { .. })
        ));
    }

    #[test]
    fn patch_parse_rejects_unknown_kind() {
        let err = PatchInvocation::parse(r#"{"edits":[{"kind":"delete","path":"x"}]}"#)
            .expect_err("unknown kind rejected");
        assert!(matches!(err, ToolExecutionError::ArgumentsParseFailed(_)));
    }

    #[test]
    fn patch_parse_rejects_add_over_size_limit() {
        let big = "x".repeat(PATCH_MAX_FILE_BYTES + 1);
        let json = format!(
            r#"{{"edits":[{{"kind":"add","path":"big","content":{}}}]}}"#,
            nojson::Json(&big),
        );
        let err = PatchInvocation::parse(&json).expect_err("too big rejected");
        assert!(matches!(
            err,
            ToolExecutionError::Patch(PatchError::FileTooLarge { .. })
        ));
    }

    #[test]
    fn patch_error_json_encodes_code_and_message() {
        let e = ToolExecutionError::Patch(PatchError::NoMatch {
            path: "a.txt".to_string(),
        });
        let s = e.to_json_string();
        assert!(s.contains(r#""error":"patch_no_match""#), "got {s}");
        assert!(s.contains("a.txt"));
    }

    #[test]
    fn patch_error_json_includes_hint_for_same_path() {
        let e = ToolExecutionError::Patch(PatchError::MultipleEditsSamePath {
            path: "dup".to_string(),
        });
        let s = e.to_json_string();
        assert!(
            s.contains(r#""error":"patch_multiple_edits_same_path""#),
            "got {s}"
        );
        assert!(s.contains(r#""hint":"#), "expected a hint member in {s}");
        assert!(s.contains("separate patch calls"), "got {s}");
    }

    #[test]
    fn patch_error_json_emits_null_hint_when_not_actionable() {
        let e = ToolExecutionError::Patch(PatchError::Rejected);
        let s = e.to_json_string();
        assert!(s.contains(r#""error":"patch_rejected""#), "got {s}");
        assert!(
            s.contains(r#""hint":null"#),
            "Rejected should emit null hint: {s}"
        );
    }

    // -------------------------------------------------------------
    // CommandInvocation
    // -------------------------------------------------------------

    #[test]
    fn command_definition_advertises_the_command_function_name() {
        let def = CommandInvocation::definition();
        assert_eq!(def.name, "command");
        assert!(def.description.contains("approval"));
        assert!(def.parameters_json.contains("argv"));
        assert!(!def.parameters_json.contains("timeout_seconds"));
    }

    #[test]
    fn command_parse_extracts_argv() {
        let inv = CommandInvocation::parse(r#"{"argv":["ls","-la"]}"#).expect("parses");
        assert_eq!(inv.argv, vec!["ls".to_string(), "-la".to_string()]);
    }

    #[test]
    fn command_parse_ignores_stray_timeout_seconds() {
        // The field is no longer part of the schema but old sessions
        // (or a confused model) may still emit it. Parsing must not
        // fail on extra keys.
        let inv = CommandInvocation::parse(r#"{"argv":["cargo","test"],"timeout_seconds":120}"#)
            .expect("parses");
        assert_eq!(inv.argv, vec!["cargo".to_string(), "test".to_string()]);
    }

    #[test]
    fn command_parse_rejects_empty_argv() {
        let err = CommandInvocation::parse(r#"{"argv":[]}"#).expect_err("empty rejected");
        assert_eq!(err, ToolExecutionError::Command(CommandError::EmptyArgv));
    }

    #[test]
    fn command_error_json_encodes_code_and_message() {
        let e = ToolExecutionError::Command(CommandError::SpawnFailed {
            message: "no such file".to_string(),
        });
        let s = e.to_json_string();
        assert!(s.contains(r#""error":"command_spawn_failed""#), "got {s}");
        assert!(s.contains("no such file"));
    }

    // -------------------------------------------------------------
    // patch approval loop
    // -------------------------------------------------------------

    fn drive_single_patch_call(
        core: &mut AgentCore,
        request: RequestId,
        call_id: &str,
        arguments_json: &str,
    ) -> Vec<Action> {
        let _ = core.handle_event(tool_call_delta(
            request,
            0,
            Some(call_id),
            Some("patch"),
            Some(arguments_json),
        ));
        core.handle_event(Event::Finish {
            request,
            reason: Some("tool_calls".to_string()),
        })
    }

    fn valid_add_patch_json() -> &'static str {
        r#"{"edits":[{"kind":"add","path":"new.txt","content":"hi"}]}"#
    }

    #[test]
    fn patch_finish_emits_preview_patch_and_stays_in_tool_running() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = drive_single_patch_call(&mut core, id, "p1", valid_add_patch_json());

        assert_eq!(core.status(), Status::ToolRunning);
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::PreviewPatch { call_id, .. } if call_id == "p1"
        )));
        assert_eq!(core.metrics().patch_calls_previewed.get(), 1);
        // no approval-visible call_id yet — preview not received
        assert_eq!(core.pending_approval_call_id(), None);
    }

    #[test]
    fn patch_preview_ready_transitions_to_awaiting_approval_and_publishes_call_id() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_patch_call(&mut core, id, "p1", valid_add_patch_json());

        let hash = PreviewHash {
            path: "new.txt".to_string(),
            sha256: None,
        };
        let preview = PatchPreview {
            target_paths: vec!["new.txt".to_string()],
            added_lines: 1,
            removed_lines: 0,
            edit_count: 1,
        };
        let actions = core.handle_event(Event::PatchPreviewReady {
            request: id,
            call_id: "p1".to_string(),
            preview_hashes: vec![hash],
            preview,
        });

        assert_eq!(actions, vec![Action::Redraw]);
        assert_eq!(core.status(), Status::AwaitingApproval);
        assert_eq!(core.pending_approval_call_id(), Some("p1".to_string()));
        assert_eq!(core.metrics().patch_previews_committed.get(), 1);

        let active = core.active_tool_calls();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].approval, ApprovalState::Pending);
        assert!(active[0].patch_preview.is_some());
    }

    #[test]
    fn patch_approve_emits_apply_patch_with_stored_hashes() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_patch_call(&mut core, id, "p1", valid_add_patch_json());
        let hash = PreviewHash {
            path: "new.txt".to_string(),
            sha256: Some([7u8; 32]),
        };
        let _ = core.handle_event(Event::PatchPreviewReady {
            request: id,
            call_id: "p1".to_string(),
            preview_hashes: vec![hash.clone()],
            preview: PatchPreview::default(),
        });

        let actions = core.handle_event(Event::ApproveToolCall {
            call_id: "p1".to_string(),
        });

        let apply = actions
            .iter()
            .find_map(|a| match a {
                Action::ApplyPatch {
                    call_id,
                    preview_hashes,
                    ..
                } => Some((call_id.clone(), preview_hashes.clone())),
                _ => None,
            })
            .expect("ApplyPatch emitted");
        assert_eq!(apply.0, "p1");
        assert_eq!(apply.1, vec![hash]);
        assert_eq!(core.metrics().tool_call_approvals_committed.get(), 1);
        // Approved but not yet resolved — approval left, phase now ToolRunning.
        assert_eq!(core.status(), Status::ToolRunning);
        assert!(core.pending_approval_call_id().is_none());
    }

    #[test]
    fn patch_reject_synthesizes_err_and_advances_when_last() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_patch_call(&mut core, id, "p1", valid_add_patch_json());
        let _ = core.handle_event(Event::PatchPreviewReady {
            request: id,
            call_id: "p1".to_string(),
            preview_hashes: Vec::new(),
            preview: PatchPreview::default(),
        });

        let actions = core.handle_event(Event::RejectToolCall {
            call_id: "p1".to_string(),
        });

        // Rejection is the only outstanding result → advance emits StartRequest.
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::StartRequest { .. }))
        );
        assert_eq!(core.metrics().tool_call_rejections_committed.get(), 1);
        // The synthetic Tool message contains the patch_rejected code.
        let last = core.conversation().last().expect("has tool message");
        match last {
            ChatMessage::Tool { content, .. } => {
                assert!(content.contains("patch_rejected"), "content={content}");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn patch_approve_dropped_if_no_pending_call() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "hi");
        let actions = core.handle_event(Event::ApproveToolCall {
            call_id: "nope".to_string(),
        });
        assert!(actions.is_empty());
        assert_eq!(core.metrics().tool_call_approvals_dropped_as_stale.get(), 1);
    }

    #[test]
    fn patch_preview_ready_dropped_if_call_id_unknown() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_patch_call(&mut core, id, "p1", valid_add_patch_json());
        let actions = core.handle_event(Event::PatchPreviewReady {
            request: id,
            call_id: "does_not_exist".to_string(),
            preview_hashes: Vec::new(),
            preview: PatchPreview::default(),
        });
        assert!(actions.is_empty());
        assert_eq!(core.metrics().patch_previews_dropped_as_stale.get(), 1);
    }

    #[test]
    fn read_only_result_lands_in_awaiting_approval_phase() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        // Two tool calls: one read-only, one patch. Patch first sets
        // approval pending after preview → phase AwaitingApproval;
        // read-only ToolResult must still land in that phase.
        let _ = core.handle_event(tool_call_delta(
            id,
            0,
            Some("r1"),
            Some("list"),
            Some(r#"{"path":"."}"#),
        ));
        let _ = core.handle_event(tool_call_delta(
            id,
            1,
            Some("p1"),
            Some("patch"),
            Some(valid_add_patch_json()),
        ));
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: Some("tool_calls".to_string()),
        });
        let _ = core.handle_event(Event::PatchPreviewReady {
            request: id,
            call_id: "p1".to_string(),
            preview_hashes: Vec::new(),
            preview: PatchPreview::default(),
        });
        assert_eq!(core.status(), Status::AwaitingApproval);

        let actions = core.handle_event(Event::ToolResult {
            request: id,
            call_id: "r1".to_string(),
            outcome: ToolOutcome::Ok("ok".to_string()),
        });
        assert_eq!(core.metrics().tool_results_committed.get(), 1);
        assert_eq!(actions, vec![Action::Redraw]);
        // Still in approval phase because patch is not yet resolved.
        assert_eq!(core.status(), Status::AwaitingApproval);
    }

    // -------------------------------------------------------------
    // command approval loop
    // -------------------------------------------------------------

    fn drive_single_command_call(
        core: &mut AgentCore,
        request: RequestId,
        call_id: &str,
        arguments_json: &str,
    ) -> Vec<Action> {
        let _ = core.handle_event(tool_call_delta(
            request,
            0,
            Some(call_id),
            Some("command"),
            Some(arguments_json),
        ));
        core.handle_event(Event::Finish {
            request,
            reason: Some("tool_calls".to_string()),
        })
    }

    fn valid_command_json() -> &'static str {
        r#"{"argv":["echo","hi"]}"#
    }

    #[test]
    fn command_finish_populates_command_preview_and_marks_pending() {
        let mut core = AgentCore::new();
        core.set_workspace_display("/tmp/wksp".to_string());
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = drive_single_command_call(&mut core, id, "c1", valid_command_json());

        // No ExecuteCommand yet — that only fires on approval.
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::ExecuteCommand { .. }))
        );
        assert_eq!(core.status(), Status::AwaitingApproval);
        assert_eq!(core.metrics().command_calls_dispatched.get(), 1);

        let active = core.active_tool_calls();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].approval, ApprovalState::Pending);
        let preview = active[0]
            .command_preview
            .as_ref()
            .expect("preview populated");
        assert_eq!(preview.argv, vec!["echo".to_string(), "hi".to_string()]);
        assert_eq!(preview.working_directory, "/tmp/wksp");
        assert_eq!(core.pending_approval_call_id(), Some("c1".to_string()));
    }

    #[test]
    fn command_approve_emits_execute_command_action() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_command_call(&mut core, id, "c1", valid_command_json());

        let actions = core.handle_event(Event::ApproveToolCall {
            call_id: "c1".to_string(),
        });

        assert_eq!(core.metrics().tool_call_approvals_committed.get(), 1);
        assert_eq!(core.metrics().command_executions_started.get(), 1);
        let matched = actions.iter().any(|a| {
            matches!(
                a,
                Action::ExecuteCommand { call_id, invocation, .. }
                    if call_id == "c1"
                        && invocation.argv == vec!["echo".to_string(), "hi".to_string()]
            )
        });
        assert!(matched, "expected ExecuteCommand, got {actions:?}");
        assert_eq!(core.status(), Status::ToolRunning);
    }

    #[test]
    fn command_reject_synthesizes_command_rejected_err() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_command_call(&mut core, id, "c1", valid_command_json());

        let actions = core.handle_event(Event::RejectToolCall {
            call_id: "c1".to_string(),
        });

        assert_eq!(core.metrics().tool_call_rejections_committed.get(), 1);
        // Advance emits StartRequest with the synthetic Tool message.
        let start = actions
            .iter()
            .find_map(|a| match a {
                Action::StartRequest { messages, .. } => Some(messages.clone()),
                _ => None,
            })
            .expect("StartRequest emitted");
        let tool_msg = start
            .iter()
            .rev()
            .find_map(|m| match m {
                ChatMessage::Tool { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("Tool message present");
        assert!(tool_msg.contains("command_rejected"), "content={tool_msg}");
    }

    #[test]
    fn command_output_chunk_appends_to_tail_when_approved() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_command_call(&mut core, id, "c1", valid_command_json());
        let _ = core.handle_event(Event::ApproveToolCall {
            call_id: "c1".to_string(),
        });

        let _ = core.handle_event(Event::CommandOutputChunk {
            request: id,
            call_id: "c1".to_string(),
            stream: CommandOutputStream::Stdout,
            bytes: b"line 1\n".to_vec(),
        });
        let _ = core.handle_event(Event::CommandOutputChunk {
            request: id,
            call_id: "c1".to_string(),
            stream: CommandOutputStream::Stderr,
            bytes: b"warn\n".to_vec(),
        });

        assert_eq!(core.metrics().command_output_chunks_appended.get(), 2);
        let active = core.active_tool_calls();
        let tail = active[0]
            .command_output_tail
            .as_ref()
            .expect("tail populated");
        assert_eq!(tail.stdout_tail, "line 1\n");
        assert_eq!(tail.stderr_tail, "warn\n");
        assert_eq!(tail.stdout_bytes_total, 7);
        assert_eq!(tail.stderr_bytes_total, 5);
    }

    #[test]
    fn command_output_chunk_dropped_if_not_approved() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = drive_single_command_call(&mut core, id, "c1", valid_command_json());
        // Still Pending, not yet Approved → chunk must drop as stale.

        let actions = core.handle_event(Event::CommandOutputChunk {
            request: id,
            call_id: "c1".to_string(),
            stream: CommandOutputStream::Stdout,
            bytes: b"early".to_vec(),
        });
        assert!(actions.is_empty());
        assert_eq!(
            core.metrics().command_output_chunks_dropped_as_stale.get(),
            1
        );
    }

    // -----------------------------------------------------------------
    // SubmitPlanInvocation
    // -----------------------------------------------------------------

    fn valid_submit_json() -> String {
        r#"{"body_markdown":"plan body","confirmations":[{"id":"migration","description":"run migration"}],"patches":[{"id":"fix-parser","path":"src/parser.rs","description":"fix parser"}],"commands":[{"id":"test","argv":["cargo","test"],"description":"run tests"}]}"#
            .to_string()
    }

    #[test]
    fn submit_plan_parses_valid_arguments() {
        let inv = SubmitPlanInvocation::parse(&valid_submit_json()).expect("parse");
        assert_eq!(inv.body_markdown, "plan body");
        assert_eq!(inv.confirmations.len(), 1);
        assert_eq!(inv.confirmations[0].id, "migration");
        assert_eq!(inv.patches[0].path, "src/parser.rs");
        assert_eq!(
            inv.commands[0].argv,
            vec!["cargo".to_string(), "test".to_string()]
        );
    }

    #[test]
    fn submit_plan_rejects_reserved_all_ok_id() {
        let json =
            r#"{"body_markdown":"b","confirmations":[{"id":"all-ok","description":"reserved"}]}"#;
        assert!(SubmitPlanInvocation::parse(json).is_err());
    }

    #[test]
    fn submit_plan_rejects_empty_body() {
        let json = r#"{"body_markdown":"   "}"#;
        assert!(SubmitPlanInvocation::parse(json).is_err());
    }

    #[test]
    fn submit_plan_rejects_invalid_id() {
        let json = r#"{"body_markdown":"b","confirmations":[{"id":"Bad_Id","description":"x"}]}"#;
        assert!(SubmitPlanInvocation::parse(json).is_err());
    }

    #[test]
    fn submit_plan_rejects_duplicate_ids_across_kinds() {
        let json = r#"{"body_markdown":"b","confirmations":[{"id":"dup","description":"c"}],"patches":[{"id":"dup","path":"src/a.rs","description":"p"}]}"#;
        assert!(SubmitPlanInvocation::parse(json).is_err());
    }

    #[test]
    fn submit_plan_rejects_duplicate_patch_path() {
        let json = r#"{"body_markdown":"b","patches":[{"id":"a","path":"src/x.rs","description":"p1"},{"id":"b","path":"src/x.rs","description":"p2"}]}"#;
        assert!(SubmitPlanInvocation::parse(json).is_err());
    }

    #[test]
    fn submit_plan_rejects_empty_command_argv() {
        let json = r#"{"body_markdown":"b","commands":[{"id":"a","argv":[],"description":"c"}]}"#;
        assert!(SubmitPlanInvocation::parse(json).is_err());
    }

    #[test]
    fn submit_plan_rejects_duplicate_command_argv() {
        let json = r#"{"body_markdown":"b","commands":[{"id":"a","argv":["ls"],"description":"c1"},{"id":"b","argv":["ls"],"description":"c2"}]}"#;
        assert!(SubmitPlanInvocation::parse(json).is_err());
    }

    // -----------------------------------------------------------------
    // PlanProposeInvocation (non-terminal `plan` tool)
    // -----------------------------------------------------------------

    #[test]
    fn plan_propose_parses_valid_arguments() {
        let inv = PlanProposeInvocation::parse(&valid_submit_json()).expect("parse");
        assert_eq!(inv.inner.body_markdown, "plan body");
        assert_eq!(inv.inner.confirmations.len(), 1);
        assert_eq!(inv.inner.confirmations[0].id, "migration");
        assert_eq!(inv.inner.commands[0].argv, vec!["cargo".to_string(), "test".to_string()]);
    }

    #[test]
    fn plan_propose_rewrites_error_prefix_to_plan() {
        let json = r#"{"body_markdown":"   "}"#;
        let err = PlanProposeInvocation::parse(json).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("plan:"), "expected a `plan:` prefix, got: {msg}");
        assert!(!msg.contains("submit_plan"), "stale prefix leaked: {msg}");
    }
}
