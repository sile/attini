use std::process::ExitCode;

use attini::agent_cli::{self, AgentConfig, Continuation, DEFAULT_MAX_TURNS};
use attini::session_cmd;

const EXIT_USAGE: u8 = 2;
const EXIT_RUNTIME: u8 = 1;
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

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
    let plan_mode = noargs::flag("plan")
        .doc("Plan mode: hide the patch tool; only run commands matched by a `readonly: true` rule, reject everything else")
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
    let prompt: Option<String> = noargs::arg("[PROMPT]")
        .doc("User prompt (required unless --approve or --reject is given)")
        .example("List the files in src/")
        .take(args)
        .present_and_then(|a| a.value().parse())?;

    if args.metadata().help_mode {
        return Ok(None);
    }

    if approve && reject {
        return Err(RunError::Runtime(
            "--approve and --reject are mutually exclusive".to_string(),
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

    if plan_mode && local_only {
        return Err(RunError::Runtime(
            "--plan and --local-only are mutually exclusive".to_string(),
        ));
    }
    let mode = if plan_mode {
        attini::sansio::permissions::Mode::Plan
    } else if local_only {
        attini::sansio::permissions::Mode::LocalOnly
    } else {
        attini::sansio::permissions::Mode::Default
    };

    let workspace_root = std::env::current_dir()
        .map_err(|e| RunError::Runtime(format!("failed to read current dir: {e}")))?;
    let cfg = AgentConfig {
        session_name,
        model,
        workspace_root,
        system_prompt: system,
        show_reasoning,
        max_turns: DEFAULT_MAX_TURNS,
        mode,
    };
    let exit = agent_cli::run(cfg, cont).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(Some(exit))
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

    if args.metadata().help_mode {
        return Ok(false);
    }
    Err(RunError::Runtime(
        "attini session requires a sub-command (list, show, tail, rm, unlock, grant)".to_string(),
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

fn try_run_session_grant(args: &mut noargs::RawArgs) -> Result<bool, RunError> {
    if !noargs::cmd("grant")
        .doc("Append an auto-approve permissions rule to permissions.jsonc")
        .take(args)
        .is_present()
    {
        return Ok(false);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; writes to .attini/<NAME>/permissions.jsonc")
        .default("main")
        .take(args)
        .then(|o| o.value().parse())?;
    let workspace = noargs::flag("workspace")
        .doc("Write to workspace-wide .attini/permissions.jsonc instead of session-local (mutually exclusive with -s / --session)")
        .take(args)
        .is_present();
    let prefix: String = noargs::arg("<PREFIX>")
        .doc("Command prefix to auto-approve (word-boundary match; single-quote to preserve whitespace)")
        .example("cargo test")
        .take(args)
        .then(|a| a.value().parse())?;
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
    match attini::permissions::grant(scope, &prefix) {
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
