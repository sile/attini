//! CLI handlers for the `attini plan` command family: create, check,
//! ok, run, and close. File I/O and lifecycle orchestration live
//! here; artifact parsing / rendering / sealing live in `crate::plan`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::agent_cli::{AgentConfig, Continuation, RunOutcome};
use crate::plan::{
    ApprovedPlan, ParsedPlan, PlanActions, PlanError, PlanSnapshot, RESERVED_CONFIRMATION_ID,
    confirmations_complete, parse, parse_snapshot, render_sealed, seal_hash, verify_seal,
};
use crate::sansio::permissions::Authorization;
use crate::session::{
    LockStatus, Session, SessionRecord, inspect_lock, now_unix_millis, session_paths,
};

impl From<PlanError> for io::Error {
    fn from(e: PlanError) -> Self {
        io::Error::other(e.to_string())
    }
}

/// Exit code for a structurally valid plan whose confirmation or
/// seal is incomplete.
pub const EXIT_PLAN_INCOMPLETE: u8 = 10;

/// Planner instruction prepended to the user prompt by `plan create`.
const PLANNER_INSTRUCTIONS: &str = "\
You are planning mode for a coding agent. Investigate the workspace and produce a Markdown \
plan that covers: purpose, change approach, target paths, and verification. List exact \
workspace-relative patch paths and exact command argv you will need approved in the plan \
actions. Confirmation items are for user judgment only, not implementation steps or TODOs. \
Do not emit markers, checkboxes, or the all-ok item yourself — end your reply by calling the \
submit_plan tool with the final structured plan.";

// -------------------------------------------------------------------
// create
// -------------------------------------------------------------------

/// Options shared by `plan create` and `plan run` agent invocations.
pub struct PlanAgentOptions {
    pub model: String,
    pub system_prompt: Option<String>,
    pub skill_name: Option<String>,
    pub show_reasoning: bool,
    pub read_paths: Vec<PathBuf>,
    pub turn_tool_call_limit: usize,
    pub tool_call_rate: Option<crate::agent_cli::RateLimit>,
    pub session_tool_call_max: Option<usize>,
}

/// Run the planning agent and write the rendered plan. Prints the
/// plan path to stdout on success; returns an exit code.
pub fn run_create(
    session_name: &str,
    output: Option<&Path>,
    options: &PlanAgentOptions,
    prompt: &str,
) -> io::Result<ExitCode> {
    let workspace_root = std::env::current_dir()?;
    let output_path = resolve_create_output(&workspace_root, session_name, output)?;
    if output_path.exists() {
        return Err(io::Error::other(format!(
            "refusing to overwrite existing file {}",
            output_path.display()
        )));
    }

    let cfg = AgentConfig {
        session_name: session_name.to_string(),
        model: options.model.clone(),
        workspace_root: workspace_root.clone(),
        system_prompt: options.system_prompt.clone(),
        show_reasoning: options.show_reasoning,
        max_turns: crate::agent_cli::DEFAULT_MAX_TURNS,
        mode: crate::sansio::permissions::Mode::Planning,
        extra_read_paths_cli: options.read_paths.clone(),
        turn_tool_call_limit: options.turn_tool_call_limit,
        tool_call_rate: options.tool_call_rate,
        session_tool_call_max: options.session_tool_call_max,
        skill_name: options.skill_name.clone(),
        subagent_available: std::env::var("ATTINI_IS_SUBAGENT").is_err(),
        authorization: Authorization::PerTool,
        plan_output_path: Some(output_path.clone()),
    };
    let user_prompt = format!("{PLANNER_INSTRUCTIONS}\n\nUser request: {prompt}");
    match crate::agent_cli::run(cfg, Continuation::Prompt(user_prompt))? {
        RunOutcome::PlanSubmitted(path) => {
            println!("{}", path.display());
            Ok(ExitCode::SUCCESS)
        }
        RunOutcome::Exit(code) => {
            eprintln!("attini: planning invocation ended without a valid submit_plan call");
            Ok(code)
        }
    }
}

fn resolve_create_output(
    workspace_root: &Path,
    session_name: &str,
    output: Option<&Path>,
) -> io::Result<PathBuf> {
    if let Some(out) = output {
        if out.extension().and_then(|e| e.to_str()) != Some("md") {
            return Err(io::Error::other(
                "plan output must have a .md extension".to_string(),
            ));
        }
        let resolved = workspace_abs(workspace_root, out)?;
        ensure_within_workspace(workspace_root, &resolved)?;
        return Ok(resolved);
    }
    let plans_dir = session_paths(session_name)?.dir.join("plans");
    for _ in 0..8 {
        let candidate = plans_dir.join(format!("plan-{}.md", crate::plan::plan_timestamp_suffix()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::other(
        "could not generate a unique managed plan name".to_string(),
    ))
}

fn workspace_abs(workspace_root: &Path, path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(workspace_root.join(path))
    }
}

fn ensure_within_workspace(workspace_root: &Path, path: &Path) -> io::Result<()> {
    let root_canon = workspace_root.canonicalize()?;
    let canonical = path
        .parent()
        .map(|p| p.canonicalize())
        .unwrap_or_else(|| Ok(workspace_root.to_path_buf()))?;
    if !canonical.starts_with(&root_canon) {
        return Err(io::Error::other(format!(
            "plan path {} escapes the workspace",
            path.display()
        )));
    }
    Ok(())
}

// -------------------------------------------------------------------
// check
// -------------------------------------------------------------------

/// Validate a plan and report its executability. Exit codes: 0 =
/// executable, 10 = structurally valid but confirmation / seal
/// incomplete, 1 = malformed.
pub fn run_check(plan_path: &Path) -> io::Result<ExitCode> {
    let text = read_plan(plan_path)?;
    let parsed = match parse(&text) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("attini: plan is malformed: {e}");
            return Ok(ExitCode::from(1));
        }
    };
    let hash = seal_hash(text.as_bytes())?;
    let seal_ok = verify_seal(&text)?;
    let complete = confirmations_complete(&parsed.confirmations);
    let unchecked: Vec<&str> = parsed
        .confirmations
        .iter()
        .filter(|c| !c.checked)
        .map(|c| c.id.as_str())
        .collect();
    println!("plan: {}", plan_path.display());
    println!("format version: {}", crate::plan::PLAN_FORMAT_VERSION);
    println!("plan_sha256: {hash}");
    println!("seal: {}", if seal_ok { "ok" } else { "mismatch" });
    println!(
        "confirmations: {} total, {} unchecked",
        parsed.confirmations.len(),
        unchecked.len()
    );
    if !unchecked.is_empty() {
        println!("unchecked: {}", unchecked.join(", "));
    }
    if !complete || !seal_ok {
        eprintln!(
            "attini: plan is not approved ({}); run `attini plan ok` to re-seal",
            if !seal_ok {
                "seal mismatch"
            } else {
                "confirmation incomplete"
            }
        );
        return Ok(ExitCode::from(EXIT_PLAN_INCOMPLETE));
    }
    Ok(ExitCode::SUCCESS)
}

// -------------------------------------------------------------------
// ok
// -------------------------------------------------------------------

/// Update confirmation checkboxes and re-seal the plan atomically.
pub fn run_ok(plan_path: &Path, all: bool, item_ids: &[String]) -> io::Result<ExitCode> {
    let text = read_plan(plan_path)?;
    let parsed = parse(&text)?;
    let updated = crate::plan::apply_confirmations(&parsed, all, item_ids)?;
    let re_sealed = render_sealed(&parsed.body_markdown, &parsed.actions, &updated)?;
    atomic_write(plan_path, re_sealed.as_bytes())?;
    let hash = seal_hash(re_sealed.as_bytes())?;
    let checked: Vec<&str> = updated
        .iter()
        .filter(|c| c.checked)
        .map(|c| c.id.as_str())
        .collect();
    println!("ok: {} checked ({})", checked.len(), checked.join(", "));
    println!("seal: {hash}");
    Ok(ExitCode::SUCCESS)
}

// -------------------------------------------------------------------
// run
// -------------------------------------------------------------------

/// Validate a sealed plan, snapshot it under the target session, and
/// execute the agent with the plan as invocation-limited
/// authorization. Exit codes follow the `check` contract for plan
/// rejection; agent failures map to 1.
pub fn run_run(
    session_name: Option<&str>,
    plan_path: &Path,
    options: &PlanAgentOptions,
) -> io::Result<ExitCode> {
    let workspace_root = std::env::current_dir()?;
    let session_name = match session_name {
        Some(s) => s.to_string(),
        None => infer_session(plan_path).unwrap_or_else(|| "main".to_string()),
    };

    let text = read_plan(plan_path)?;
    let parsed = parse(&text)?;
    let hash = seal_hash(text.as_bytes())?;
    if !verify_seal(&text)? {
        eprintln!("attini: plan seal mismatch; run `attini plan ok` to re-seal");
        return Ok(ExitCode::from(EXIT_PLAN_INCOMPLETE));
    }
    if !confirmations_complete(&parsed.confirmations) {
        eprintln!("attini: plan confirmations incomplete; run `attini plan ok`");
        return Ok(ExitCode::from(EXIT_PLAN_INCOMPLETE));
    }

    // Refuse to bypass a live session or an existing pending.
    let paths = session_paths(&session_name)?;
    if matches!(inspect_lock(&paths.lock), LockStatus::PidAlive(_)) {
        return Err(io::Error::other(format!(
            "session {session_name:?} is busy (LOCK held by a live process)"
        )));
    }
    if paths.pending.try_exists()? {
        return Err(io::Error::other(format!(
            "session {session_name:?} has a pending tool call; resolve it before plan run"
        )));
    }

    let snapshot_path = write_snapshot(&session_name, plan_path, &hash, &parsed)?;
    let approved = ApprovedPlan {
        plan_sha256: hash.clone(),
        actions: parsed.actions.clone(),
        snapshot_path: snapshot_path.clone(),
    };

    {
        let mut session = Session::open(&session_name)?;
        session.append(&SessionRecord::PlanApproved {
            ts: now_unix_millis(),
            path: plan_path.display().to_string(),
            plan_sha256: hash.clone(),
        })?;
        session.close()?;
    }
    let user_message = build_run_message(&parsed);
    let cfg = AgentConfig {
        session_name,
        model: options.model.clone(),
        workspace_root: workspace_root.clone(),
        system_prompt: options.system_prompt.clone(),
        show_reasoning: options.show_reasoning,
        max_turns: crate::agent_cli::DEFAULT_MAX_TURNS,
        mode: crate::sansio::permissions::Mode::Default,
        extra_read_paths_cli: options.read_paths.clone(),
        turn_tool_call_limit: options.turn_tool_call_limit,
        tool_call_rate: options.tool_call_rate,
        session_tool_call_max: options.session_tool_call_max,
        skill_name: options.skill_name.clone(),
        subagent_available: std::env::var("ATTINI_IS_SUBAGENT").is_err(),
        authorization: Authorization::ApprovedPlan(approved),
        plan_output_path: None,
    };
    eprintln!(
        "attini: plan run started ({}), snapshot {}",
        plan_path.display(),
        snapshot_path.display()
    );
    let outcome = crate::agent_cli::run(cfg, Continuation::Prompt(user_message))?;
    eprintln!(
        "attini: plan run finished; close the plan with `attini plan close {}`",
        plan_path.display()
    );
    match outcome {
        RunOutcome::Exit(code) => Ok(code),
        RunOutcome::PlanSubmitted(_) => Ok(ExitCode::from(1)),
    }
}

fn build_run_message(parsed: &ParsedPlan) -> String {
    let mut out = String::from("Execute the approved plan.\n\nApproved confirmations:\n");
    for c in parsed
        .confirmations
        .iter()
        .filter(|c| c.id != RESERVED_CONFIRMATION_ID)
    {
        out.push_str(&format!("- {} — {}\n", c.id, c.description));
    }
    out.push_str("\nPlan:\n");
    out.push_str(&parsed.body_markdown);
    out.push_str("\n\nApproved plan actions:\n");
    out.push_str(&actions_display(&parsed.actions));
    out
}

fn actions_display(actions: &PlanActions) -> String {
    let mut out = String::new();
    for p in &actions.patches {
        out.push_str(&format!(
            "- patch {}: path {:?} — {}\n",
            p.id, p.path, p.description
        ));
    }
    for c in &actions.commands {
        out.push_str(&format!(
            "- command {}: argv {:?} — {}\n",
            c.id, c.argv, c.description
        ));
    }
    out
}

// -------------------------------------------------------------------
// close
// -------------------------------------------------------------------

/// Move a managed plan into its session's `plans/closed/` directory.
pub fn run_close(plan_path: &Path) -> io::Result<ExitCode> {
    let session_name = infer_session(plan_path).ok_or_else(|| {
        io::Error::other(format!(
            "{} is not a managed plan under .attini/<SESSION>/plans/",
            plan_path.display()
        ))
    })?;
    if plan_path.components().any(|c| c.as_os_str() == "closed") {
        return Err(io::Error::other(format!(
            "{} is already under a closed/ directory",
            plan_path.display()
        )));
    }
    let paths = session_paths(&session_name)?;
    if matches!(inspect_lock(&paths.lock), LockStatus::PidAlive(_)) {
        return Err(io::Error::other(format!(
            "session {session_name:?} is busy; cannot close its plans"
        )));
    }
    let name = plan_path
        .file_name()
        .ok_or_else(|| io::Error::other("plan path has no file name"))?;
    let dest_dir = paths.dir.join("plans").join("closed");
    let dest = dest_dir.join(name);
    if dest.exists() {
        return Err(io::Error::other(format!(
            "destination {} already exists",
            dest.display()
        )));
    }
    fs::create_dir_all(&dest_dir)?;
    fs::rename(plan_path, &dest)?;
    println!("{}", dest.display());
    Ok(ExitCode::SUCCESS)
}

// -------------------------------------------------------------------
// shared helpers
// -------------------------------------------------------------------

fn read_plan(plan_path: &Path) -> io::Result<String> {
    let bytes = fs::read(plan_path).map_err(|e| {
        io::Error::other(format!("failed to read plan {}: {e}", plan_path.display()))
    })?;
    if bytes.len() > crate::plan::PLAN_MAX_BYTES {
        return Err(io::Error::other(format!(
            "plan {} exceeds {} bytes",
            plan_path.display(),
            crate::plan::PLAN_MAX_BYTES
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::other(format!("plan {} is not valid UTF-8", plan_path.display())))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp{}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn write_snapshot(
    session_name: &str,
    plan_path: &Path,
    hash: &str,
    parsed: &ParsedPlan,
) -> io::Result<PathBuf> {
    let dir = session_paths(session_name)?.dir.join("plan-runs");
    fs::create_dir_all(&dir)?;
    let snapshot_path = dir.join(format!("{hash}.json"));
    let snapshot = PlanSnapshot {
        snapshot_version: crate::plan::PLAN_SNAPSHOT_VERSION,
        plan_format_version: crate::plan::PLAN_FORMAT_VERSION,
        plan_sha256: hash.to_string(),
        canonical_path: plan_path.display().to_string(),
        body_markdown: parsed.body_markdown.clone(),
        actions: parsed.actions.clone(),
        confirmations: parsed.confirmations.clone(),
    };
    let json = snapshot.to_json();
    if snapshot_path.exists() {
        let existing = fs::read_to_string(&snapshot_path)?;
        let existing_parsed = parse_snapshot(&existing).map_err(|e| {
            io::Error::other(format!(
                "snapshot {} is unreadable: {e}",
                snapshot_path.display()
            ))
        })?;
        if existing_parsed.plan_sha256 != hash
            || existing_parsed.actions != parsed.actions
            || existing_parsed.body_markdown != parsed.body_markdown
        {
            return Err(io::Error::other(format!(
                "snapshot {} exists with different content (invariant violation)",
                snapshot_path.display()
            )));
        }
    } else {
        atomic_write(&snapshot_path, json.as_bytes())?;
    }
    Ok(snapshot_path)
}

/// Infer the session name from a managed plan path of the shape
/// `.attini/<SESSION>/plans/<name>.md`.
fn infer_session(plan_path: &Path) -> Option<String> {
    let mut components = plan_path.components();
    let first = components.next()?.as_os_str().to_str()?;
    if first != ".attini" {
        return None;
    }
    let session = components.next()?.as_os_str().to_str()?.to_string();
    let dir = components.next()?.as_os_str().to_str()?;
    if dir != "plans" {
        return None;
    }
    Some(session)
}

/// Audit helper used by tests to re-derive an approved plan from a
/// snapshot file.
#[allow(dead_code)]
pub(crate) fn snapshot_to_approved(path: &Path) -> io::Result<ApprovedPlan> {
    let text = fs::read_to_string(path)?;
    let snapshot = parse_snapshot(&text).map_err(|e| io::Error::other(format!("snapshot: {e}")))?;
    let mut approved = snapshot.into_approved_plan();
    approved.snapshot_path = path.to_path_buf();
    Ok(approved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Confirmation;

    #[test]
    fn infer_session_parses_managed_path() {
        let p = Path::new(".attini/planner/plans/plan-1.md");
        assert_eq!(infer_session(p).as_deref(), Some("planner"));
        assert_eq!(infer_session(Path::new("plans/plan-1.md")), None);
        assert_eq!(
            infer_session(Path::new(".attini/planner/other/plan-1.md")),
            None
        );
    }

    #[test]
    fn build_run_message_excludes_seal_and_all_ok() {
        let parsed = ParsedPlan {
            body_markdown: "# body".to_string(),
            actions: PlanActions {
                patches: vec![],
                commands: vec![],
            },
            confirmations: vec![
                Confirmation {
                    id: RESERVED_CONFIRMATION_ID.to_string(),
                    description: "all".to_string(),
                    checked: true,
                },
                Confirmation {
                    id: "migration".to_string(),
                    description: "run migration".to_string(),
                    checked: false,
                },
            ],
        };
        let msg = build_run_message(&parsed);
        assert!(msg.contains("migration — run migration"));
        assert!(!msg.contains("all-ok"));
        assert!(!msg.contains("ATTINI-CONFIRMATIONS"));
    }

    #[test]
    fn plan_error_display_is_stable() {
        assert!(!PlanError::MissingAllOk.to_string().is_empty());
        assert!(!PlanError::DuplicateAllOk.to_string().is_empty());
    }

    #[test]
    fn session_record_plan_variants_roundtrip() {
        use nojson::Json;
        let rec = SessionRecord::PlanCreated {
            ts: 1,
            path: "plans/plan-x.md".to_string(),
            plan_sha256: "ab".repeat(32),
        };
        let json = Json(&rec).to_string();
        assert!(json.contains("\"kind\":\"plan_created\""));
        assert!(json.contains("\"plan_sha256\""));
    }
}
