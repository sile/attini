use std::process::ExitCode;

use attini::agent_cli::{self, AgentConfig, Continuation, DEFAULT_MAX_TURNS, RateLimit};
use attini::session_cmd;

const EXIT_USAGE: u8 = 2;
const EXIT_RUNTIME: u8 = 1;
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

// String forms of the tool-call cap defaults, exposed here because
// noargs' `default()` needs a `&'static str`. Kept in sync with the
// numeric constants in `attini::agent_cli` by
// `default_string_constants_stay_in_sync`.
const DEFAULT_TURN_TOOL_CALL_LIMIT_STR: &str = "20";
const DEFAULT_TOOL_CALL_RATE_STR: &str = "60/60";
const DEFAULT_SESSION_TOOL_CALL_MAX_STR: &str = "5000";

fn main() -> ExitCode {
    match run() {
        Ok(RunOutcome::Ok) => ExitCode::SUCCESS,
        Ok(RunOutcome::Exit(code)) => code,
        Err(RunError::Usage(err)) => {
            eprintln!("{err:?}");
            ExitCode::from(EXIT_USAGE)
        }
        Err(RunError::Runtime(msg)) => {
            eprintln!("attini: {msg}");
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

enum RunOutcome {
    Ok,
    Exit(ExitCode),
}

enum RunError {
    Usage(noargs::Error),
    Runtime(String),
}

impl From<noargs::Error> for RunError {
    fn from(err: noargs::Error) -> Self {
        Self::Usage(err)
    }
}

fn run() -> Result<RunOutcome, RunError> {
    let mut args = noargs::raw_args();
    args.metadata_mut().app_name = env!("CARGO_PKG_NAME");
    args.metadata_mut().app_description = "DeepSeek-based coding agent prototype.";

    if noargs::VERSION_FLAG.take(&mut args).is_present() {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(RunOutcome::Ok);
    }
    noargs::HELP_FLAG.take_help(&mut args);

    if let Some(exit) = try_run_agent(&mut args)? {
        return Ok(RunOutcome::Exit(exit));
    }
    if try_run_plan(&mut args)? {
        return Ok(RunOutcome::Ok);
    }
    if try_run_session(&mut args)? {
        return Ok(RunOutcome::Ok);
    }

    if let Some(help) = args.finish()? {
        print!("{help}");
    }
    Ok(RunOutcome::Ok)
}

fn try_run_agent(args: &mut noargs::RawArgs) -> Result<Option<ExitCode>, RunError> {
    if !noargs::cmd("agent")
        .doc("Run one turn of the sync CLI agent against a persistent session")
        .take(args)
        .is_present()
    {
        return Ok(None);
    }

    let model: String = noargs::opt("model")
        .ty("NAME")
        .doc("Model name")
        .default(DEFAULT_MODEL)
        .env("ATTINI_MODEL_NAME")
        .take(args)
        .then(|o| o.value().parse())?;
    let system: Option<String> = noargs::opt("system")
        .ty("TEXT")
        .doc("Optional system prompt prepended to the conversation")
        .take(args)
        .present_and_then(|o| o.value().parse())?;
    let show_reasoning = noargs::flag("show-reasoning")
        .doc("Print reasoning_content deltas to stderr")
        .take(args)
        .is_present();
    let approve = noargs::flag("approve")
        .doc("Resume the session by approving its pending tool call")
        .take(args)
        .is_present();
    let reject = noargs::flag("reject")
        .doc("Resume the session by rejecting its pending tool call")
        .take(args)
        .is_present();
    let local_only = noargs::flag("local-only")
        .doc("Local-only mode: auto-run commands matched by a `network: false` rule; leave others for approval")
        .take(args)
        .is_present();
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    // --read-path is repeatable; noargs' opt consumes one occurrence
    // per take(), so we loop until nothing is left.
    let mut read_paths: Vec<std::path::PathBuf> = Vec::new();
    loop {
        let taken = noargs::opt("read-path")
            .ty("PATH")
            .doc(
                "Extra workspace-external read-only path prefix for this invocation only. \
                 Repeatable. Persistent grants go through `attini session grant-read`.",
            )
            .take(args);
        if !taken.is_present() {
            break;
        }
        let s: String = taken.then(|o| o.value().parse())?;
        read_paths.push(std::path::PathBuf::from(s));
    }
    // --reference is repeatable; same loop pattern as --read-path.
    let mut reference_paths: Vec<std::path::PathBuf> = Vec::new();
    loop {
        let taken = noargs::opt("reference")
            .short('r')
            .ty("PATH")
            .doc(
                "File whose contents are inlined into the system prompt before the first turn. \
                 Repeatable. Relative paths resolve against the workspace root. Files larger \
                 than 32 KiB are not inlined; they are granted as read roots and referenced \
                 by absolute path instead.",
            )
            .take(args);
        if !taken.is_present() {
            break;
        }
        let s: String = taken.then(|o| o.value().parse())?;
        reference_paths.push(std::path::PathBuf::from(s));
    }
    let turn_tool_call_limit: usize = noargs::opt("turn-tool-call-limit")
        .ty("N")
        .doc(
            "Maximum tool calls admitted per model turn. Extras get a synthetic error \
             result and the loop advances to the next turn.",
        )
        .default(DEFAULT_TURN_TOOL_CALL_LIMIT_STR)
        .take(args)
        .then(|o| o.value().parse())?;
    let tool_call_rate_raw: String = noargs::opt("tool-call-rate")
        .ty("CALLS/SECS|none")
        .doc(
            "Sliding-window rate cap on admitted tool calls, formatted as \
             <calls>/<window_seconds>. Use `none` to disable.",
        )
        .default(DEFAULT_TOOL_CALL_RATE_STR)
        .take(args)
        .then(|o| o.value().parse())?;
    let session_tool_call_max_raw: String = noargs::opt("session-tool-call-max")
        .ty("N|none")
        .doc(
            "Invocation-scope backstop on admitted tool calls. Reaching it ends the \
             invocation with reason=session_tool_call_exhausted. Use `none` to disable.",
        )
        .default(DEFAULT_SESSION_TOOL_CALL_MAX_STR)
        .take(args)
        .then(|o| o.value().parse())?;
    let skill_name: Option<String> = noargs::opt("skill")
        .ty("NAME")
        .doc(
            "Load the named skill (directory under ~/.attini/skills or .attini/skills) \
             and prepend its SKILL.md body as a system message before PROMPT. \
             Cannot be combined with --approve or --reject.",
        )
        .take(args)
        .present_and_then(|o| o.value().parse())?;

    let prompt: Option<String> = noargs::arg("[PROMPT]")
        .doc("User prompt (required unless --approve or --reject is given)")
        .example("List the files in src/")
        .take(args)
        .present_and_then(|a| a.value().parse())?;

    if args.metadata().help_mode {
        return Ok(None);
    }

    let tool_call_rate = parse_tool_call_rate(&tool_call_rate_raw)?;
    let session_tool_call_max = parse_session_tool_call_max(&session_tool_call_max_raw)?;

    if approve && reject {
        return Err(RunError::Runtime(
            "--approve and --reject are mutually exclusive".to_string(),
        ));
    }
    if skill_name.is_some() && (approve || reject) {
        return Err(RunError::Runtime(
            "--skill cannot be combined with --approve or --reject".to_string(),
        ));
    }
    let cont = if approve {
        if prompt.is_some() {
            return Err(RunError::Runtime(
                "PROMPT must be omitted when using --approve".to_string(),
            ));
        }
        Continuation::Approve
    } else if reject {
        if prompt.is_some() {
            return Err(RunError::Runtime(
                "PROMPT must be omitted when using --reject".to_string(),
            ));
        }
        Continuation::Reject
    } else {
        match prompt {
            Some(p) => Continuation::Prompt(p),
            None => {
                return Err(RunError::Runtime(
                    "PROMPT is required unless --approve or --reject is given".to_string(),
                ));
            }
        }
    };

    let mode = if local_only {
        attini::sansio::permissions::Mode::LocalOnly
    } else {
        attini::sansio::permissions::Mode::Default
    };
    let authorization = attini::sansio::permissions::Authorization::PerTool;

    let workspace_root = std::env::current_dir()
        .map_err(|e| RunError::Runtime(format!("failed to read current dir: {e}")))?;
    // The `subagent_run` tool is only advertised when this process is
    // not itself a subagent (recursion guard). The env probe is done
    // here (only I/O spot in the CLI wiring) so
    // agent_cli::build_tool_defs stays pure.
    let subagent_available = std::env::var("ATTINI_IS_SUBAGENT").is_err();
    let cfg = AgentConfig {
        session_name,
        model,
        workspace_root,
        system_prompt: system,
        show_reasoning,
        max_turns: DEFAULT_MAX_TURNS,
        mode,
        extra_read_paths_cli: read_paths,
        reference_paths,
        turn_tool_call_limit,
        tool_call_rate,
        session_tool_call_max,
        skill_name,
        subagent_available,
        authorization,
    };
    match agent_cli::run(cfg, cont).map_err(|e| RunError::Runtime(e.to_string()))? {
        agent_cli::RunOutcome::Exit(code) => Ok(Some(code)),
    }
}

// -------------------------------------------------------------------
// `attini plan` command family
// -------------------------------------------------------------------

fn try_run_plan(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("plan")
        .doc("Show the current plan: latest assistant message + any pending tool call (read-only)")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_plan_view(&session_name).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn parse_tool_call_rate(raw: &str) -> Result<Option<RateLimit>, RunError> {
    if raw == "none" {
        return Ok(None);
    }
    let (calls_str, window_str) = raw.split_once('/').ok_or_else(|| {
        RunError::Runtime(format!(
            "--tool-call-rate must be <calls>/<window_seconds> or `none` (got {raw:?})"
        ))
    })?;
    let calls: usize = calls_str.parse().map_err(|e| {
        RunError::Runtime(format!(
            "--tool-call-rate calls part {calls_str:?} is not a non-negative integer: {e}"
        ))
    })?;
    let window_secs: u64 = window_str.parse().map_err(|e| {
        RunError::Runtime(format!(
            "--tool-call-rate window part {window_str:?} is not a non-negative integer: {e}"
        ))
    })?;
    if calls == 0 || window_secs == 0 {
        return Err(RunError::Runtime(
            "--tool-call-rate calls and window must both be positive (use `none` to disable)"
                .to_string(),
        ));
    }
    Ok(Some(RateLimit {
        calls,
        window: std::time::Duration::from_secs(window_secs),
    }))
}

fn parse_session_tool_call_max(raw: &str) -> Result<Option<usize>, RunError> {
    if raw == "none" {
        return Ok(None);
    }
    let n: usize = raw.parse().map_err(|e| {
        RunError::Runtime(format!(
            "--session-tool-call-max must be a non-negative integer or `none` (got {raw:?}): {e}"
        ))
    })?;
    if n == 0 {
        return Err(RunError::Runtime(
            "--session-tool-call-max must be positive (use `none` to disable)".to_string(),
        ));
    }
    Ok(Some(n))
}

fn try_run_session(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("session")
        .doc("Inspect and manage attini agent sessions under .attini/")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }

    if try_run_session_list(args)? {
        return Ok(true);
    }
    if try_run_session_show(args)? {
        return Ok(true);
    }
    if try_run_session_tail(args)? {
        return Ok(true);
    }
    if try_run_session_rm(args)? {
        return Ok(true);
    }
    if try_run_session_unlock(args)? {
        return Ok(true);
    }
    if try_run_session_grant(args)? {
        return Ok(true);
    }
    if try_run_session_compact(args)? {
        return Ok(true);
    }
    if try_run_session_prune(args)? {
        return Ok(true);
    }
    if try_run_session_metrics(args)? {
        return Ok(true);
    }
    if try_run_session_grant_read(args)? {
        return Ok(true);
    }

    if args.metadata().help_mode {
        return Ok(false);
    }
    Err(RunError::Runtime(
        "attini session requires a sub-command (list, show, tail, rm, unlock, grant, grant-read, compact, prune, metrics)"
            .to_string(),
    ))
}

fn try_run_session_list(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("list")
        .doc("List all sessions under .attini/ in the current directory")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_list().map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn try_run_session_show(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("show")
        .doc("Show a summary of one session (invocations, message counts, pending)")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let name: String = noargs::arg("<SESSION>")
        .doc("Session name; directory is .attini/<SESSION>/")
        .example("main")
        .take(args)
        .then(|a| a.value().parse())?;
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_show(&name).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn try_run_session_tail(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("tail")
        .doc("Print the tail of conversation.jsonl (LOCK not acquired)")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let follow = noargs::flag("follow")
        .short('f')
        .doc(
            "Poll the file every 500ms and print appended lines (like `tail -f`). \
              read-only observation mode; not related to `plan mode` permission preset.",
        )
        .take(args)
        .is_present();
    let lines: usize = noargs::opt("lines")
        .short('n')
        .ty("N")
        .doc("Number of trailing lines to print before following (default 20)")
        .default("20")
        .take(args)
        .then(|o| o.value().parse())?;
    let name: String = noargs::arg("<SESSION>")
        .doc("Session name")
        .example("main")
        .take(args)
        .then(|a| a.value().parse())?;
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_tail(&name, follow, lines).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn try_run_session_rm(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("rm")
        .doc("Remove a session directory (refuses if a live process is holding the LOCK)")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let yes = noargs::flag("yes")
        .short('y')
        .doc("Skip the confirmation prompt (required when stdin is not a TTY)")
        .take(args)
        .is_present();
    let name: String = noargs::arg("<SESSION>")
        .doc("Session name")
        .example("main")
        .take(args)
        .then(|a| a.value().parse())?;
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_rm(&name, yes).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn try_run_session_unlock(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("unlock")
        .doc("Remove the LOCK file. Refuses if the holder PID is alive unless --force is set")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let force = noargs::flag("force")
        .doc("Remove the LOCK even if the holder PID appears alive (PID reuse escape hatch)")
        .take(args)
        .is_present();
    let name: String = noargs::arg("<SESSION>")
        .doc("Session name")
        .example("main")
        .take(args)
        .then(|a| a.value().parse())?;
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_unlock(&name, force).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn try_run_session_grant_read(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("grant-read")
        .doc(
            "Append a workspace-external read-only path to permissions.json. \
             Read-only tools (list / read / search) will accept paths under this prefix.",
        )
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; writes to .attini/<NAME>/permissions.json")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    let workspace = noargs::flag("workspace")
        .doc(
            "Write to workspace-wide .attini/permissions.json instead of session-local \
             (mutually exclusive with -s / --session)",
        )
        .take(args)
        .is_present();
    let path: String = noargs::arg("<PATH>")
        .doc(
            "Read-only path prefix to grant. Workspace-relative or absolute; \
             stored as-given and canonicalised on load.",
        )
        .example("../shared-docs/")
        .take(args)
        .then(|a| a.value().parse())?;
    if args.metadata().help_mode {
        return Ok(false);
    }
    if workspace && session_name != "main" {
        return Err(RunError::Runtime(
            "-s / --session and --workspace are mutually exclusive".to_string(),
        ));
    }
    let scope = if workspace {
        attini::permissions::GrantScope::Workspace
    } else {
        attini::permissions::GrantScope::Session(&session_name)
    };
    match attini::permissions::grant_read(scope, &path) {
        Ok(attini::permissions::GrantReadOutcome::Appended(p)) => {
            eprintln!("granted read: appended to {}", p.display());
            Ok(true)
        }
        Ok(attini::permissions::GrantReadOutcome::AlreadyGranted(p)) => {
            eprintln!("already granted (no-op): {}", p.display());
            Ok(true)
        }
        Err(e) => Err(RunError::Runtime(e.to_string())),
    }
}

fn try_run_session_grant(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("grant")
        .doc(
            "Append an auto-approve permissions rule (argv_prefix) to permissions.json. \
             Positional args form the argv-prefix: `attini session grant cargo test` grants \
             any command whose argv starts with [\"cargo\", \"test\"].",
        )
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; writes to .attini/<NAME>/permissions.json")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    let workspace = noargs::flag("workspace")
        .doc("Write to workspace-wide .attini/permissions.json instead of session-local (mutually exclusive with -s / --session)")
        .take(args)
        .is_present();
    // argv-prefix as variadic positional args: read until args is exhausted.
    let head: String = noargs::arg("<ARG0>")
        .doc("First element of the argv_prefix to auto-approve (the program name).")
        .example("cargo")
        .take(args)
        .then(|a| a.value().parse())?;
    let mut argv_prefix: Vec<String> = vec![head];
    loop {
        let taken = noargs::arg("[ARG]")
            .doc("Further argv_prefix elements; repeat for a longer prefix.")
            .example("test")
            .take(args);
        if !taken.is_present() {
            break;
        }
        let s: String = taken.then(|a| a.value().parse())?;
        argv_prefix.push(s);
    }
    if args.metadata().help_mode {
        return Ok(false);
    }
    // -s explicitly given AND --workspace both present is ambiguous; we can't detect
    // the "explicit" -s from noargs (default fills in), so we only reject the pair when
    // --workspace is set and NAME is not the default. That's imperfect (user could set -s main
    // + --workspace and we'd accept) but matches user intent for the common case.
    if workspace && session_name != "main" {
        return Err(RunError::Runtime(
            "-s / --session and --workspace are mutually exclusive".to_string(),
        ));
    }
    let scope = if workspace {
        attini::permissions::GrantScope::Workspace
    } else {
        attini::permissions::GrantScope::Session(&session_name)
    };
    match attini::permissions::grant(scope, &argv_prefix) {
        Ok(attini::permissions::GrantOutcome::Appended(path)) => {
            eprintln!("granted: appended to {}", path.display());
            Ok(true)
        }
        Ok(attini::permissions::GrantOutcome::AlreadyGranted(path)) => {
            eprintln!("already granted (no-op): {}", path.display());
            Ok(true)
        }
        Err(e) => Err(RunError::Runtime(e.to_string())),
    }
}

fn try_run_session_metrics(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("metrics")
        .doc(
            "Aggregate metrics from session records. \
             Use -s NAME for a single session, --all for every session under .attini/.",
        )
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    let all = noargs::flag("all")
        .doc("Aggregate across every session under .attini/ (ignores -s)")
        .take(args)
        .is_present();
    let json = noargs::flag("json")
        .doc("Emit the aggregate as a JSON object instead of a human-readable table")
        .take(args)
        .is_present();
    if args.metadata().help_mode {
        return Ok(false);
    }
    let scope = if all {
        session_cmd::MetricsScope::All
    } else {
        session_cmd::MetricsScope::Single(&session_name)
    };
    session_cmd::run_metrics(scope, json).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn try_run_session_prune(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("prune")
        .doc(
            "Drop records before the last summary in conversation.jsonl. \
             Refuses if the session is held or missing.",
        )
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    let yes = noargs::flag("yes")
        .short('y')
        .doc("Skip the confirmation prompt (required when stdin is not a TTY)")
        .take(args)
        .is_present();
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_prune(&session_name, yes).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

fn try_run_session_compact(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("compact")
        .doc(
            "Summarise older conversation records and append a summary record. \
             Refuses if the session is held, is missing, or has pending.json.",
        )
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let model: String = noargs::opt("model")
        .ty("NAME")
        .doc("Model name used for the summariser")
        .default(DEFAULT_MODEL)
        .take(args)
        .then(|o| o.value().parse())?;
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    if args.metadata().help_mode {
        return Ok(false);
    }
    session_cmd::run_compact(&session_name, &model)
        .map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attini::agent_cli::{
        DEFAULT_SESSION_TOOL_CALL_MAX, DEFAULT_TOOL_CALL_RATE_CALLS,
        DEFAULT_TOOL_CALL_RATE_WINDOW_SECS, DEFAULT_TURN_TOOL_CALL_LIMIT,
    };

    #[test]
    fn default_string_constants_stay_in_sync() {
        // The `.default()` argument of noargs::opt requires a
        // `&'static str`, so the CLI mirrors the numeric defaults in
        // `agent_cli` with these string constants. This test guards
        // against them silently drifting apart.
        assert_eq!(
            DEFAULT_TURN_TOOL_CALL_LIMIT_STR,
            DEFAULT_TURN_TOOL_CALL_LIMIT.to_string()
        );
        assert_eq!(
            DEFAULT_TOOL_CALL_RATE_STR,
            format!(
                "{}/{}",
                DEFAULT_TOOL_CALL_RATE_CALLS, DEFAULT_TOOL_CALL_RATE_WINDOW_SECS
            )
        );
        assert_eq!(
            DEFAULT_SESSION_TOOL_CALL_MAX_STR,
            DEFAULT_SESSION_TOOL_CALL_MAX.to_string()
        );
    }

    #[test]
    fn tool_call_rate_none_disables() {
        match parse_tool_call_rate("none") {
            Ok(None) => {}
            other => panic!("expected Ok(None), got is_ok={}", other.is_ok()),
        }
    }

    #[test]
    fn tool_call_rate_valid_form_parses() {
        match parse_tool_call_rate("30/15") {
            Ok(Some(rl)) => {
                assert_eq!(rl.calls, 30);
                assert_eq!(rl.window, std::time::Duration::from_secs(15));
            }
            other => panic!("expected Ok(Some(...)), got is_ok={}", other.is_ok()),
        }
    }

    #[test]
    fn tool_call_rate_missing_slash_errors() {
        assert!(parse_tool_call_rate("30").is_err());
    }

    #[test]
    fn tool_call_rate_zero_parts_error() {
        assert!(parse_tool_call_rate("0/60").is_err());
        assert!(parse_tool_call_rate("60/0").is_err());
    }

    #[test]
    fn tool_call_rate_non_integer_errors() {
        assert!(parse_tool_call_rate("abc/60").is_err());
        assert!(parse_tool_call_rate("60/xyz").is_err());
    }

    #[test]
    fn session_tool_call_max_none_disables() {
        match parse_session_tool_call_max("none") {
            Ok(None) => {}
            other => panic!("expected Ok(None), got is_ok={}", other.is_ok()),
        }
    }

    #[test]
    fn session_tool_call_max_positive_parses() {
        match parse_session_tool_call_max("42") {
            Ok(Some(n)) => assert_eq!(n, 42),
            other => panic!("expected Ok(Some(42)), got is_ok={}", other.is_ok()),
        }
    }

    #[test]
    fn session_tool_call_max_zero_errors() {
        assert!(parse_session_tool_call_max("0").is_err());
    }

    #[test]
    fn session_tool_call_max_non_integer_errors() {
        assert!(parse_session_tool_call_max("abc").is_err());
    }
}
