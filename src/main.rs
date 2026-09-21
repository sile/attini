use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use attini::session_cmd;
use attini::tell_cli::{self, Continuation, RateLimit, TellConfig};

const EXIT_USAGE: u8 = 2;
const EXIT_RUNTIME: u8 = 1;
const DEFAULT_MODEL: &str = "deepseek-flash";

/// Environment variable that supplies a default session name when
/// `-s/--session` (or a positional `<SESSION>`) is omitted.
const SESSION_ENV: &str = "ATTINI_SESSION_NAME";
/// Environment variable that supplies a default model name when
/// `--model` is omitted.
const MODEL_ENV: &str = "ATTINI_MODEL_NAME";
/// Environment variable that supplies a default completion-token cap when
/// `--max-tokens` is omitted.
const MAX_TOKENS_ENV: &str = "ATTINI_MAX_TOKENS";
/// Environment variable that supplies a default sampling temperature when
/// `--temperature` is omitted.
const TEMPERATURE_ENV: &str = "ATTINI_TEMPERATURE";
/// Environment variable that supplies a default `command` tool timeout
/// (seconds) when `--command-timeout` is omitted.
const COMMAND_TIMEOUT_ENV: &str = "ATTINI_COMMAND_TIMEOUT_SECONDS";
/// Environment variable that supplies the agent-loop turn cap when
/// `--max-turns` is omitted.
const MAX_TURNS_ENV: &str = "ATTINI_MAX_TURNS";

/// Cap on how many bytes `--stdin` may contribute to the prompt, to avoid
/// bloating the user message with an unbounded paste.
const MAX_STDIN_BYTES: usize = 1024 * 1024;

// String forms of the tool-call cap defaults, exposed here because
// noargs' `default()` needs a `&'static str`. Kept in sync with the
// numeric constants in `attini::tell_cli` by
// `default_string_constants_stay_in_sync`.
const DEFAULT_TURN_TOOL_CALL_LIMIT_STR: &str = "20";
// String form of `tell_cli::DEFAULT_MAX_TURNS`, for the same reason.
const DEFAULT_MAX_TURNS_STR: &str = "20";
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

/// Result of probing one top-level command. Lets `run()` short-circuit
/// once a command is matched, so sibling commands are not recorded as
/// subcommands in help output.
#[derive(Debug, Clone, Copy)]
enum CommandOutcome {
    /// This top-level command was not the one named on the CLI.
    NotHandled,
    /// Matched and completed; stop.
    Done,
    /// Matched and requested a process exit; stop with this code.
    Exit(ExitCode),
    /// Matched but the user asked for help; stop probing siblings so
    /// `args.finish()` renders only this command's help.
    Help,
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

/// Append standard-input auxiliary content to the prompt, wrapped in an
/// unambiguous marker block so the model can tell where the pasted data
/// begins.
fn append_stdin_aux(prompt: String, stdin_text: &str) -> String {
    format!("{prompt}\n\n--- stdin ---\n{stdin_text}\n--- end stdin ---")
}

/// Read the `--stdin` auxiliary prompt content. Reads until EOF, so a
/// terminal caller is told to finish with Ctrl+D (or Ctrl+C to cancel).
/// Errors when input exceeds [`MAX_STDIN_BYTES`]. Returns `None` (and
/// warns) when input is empty.
fn read_stdin_auxiliary() -> Result<Option<String>, RunError> {
    if std::io::stdin().is_terminal() {
        eprintln!(
            "note: --stdin: reading from the terminal; type your text and press EOF (Ctrl+D) to \
             send, or Ctrl+C to cancel"
        );
    }
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| RunError::Runtime(format!("failed to read standard input: {e}")))?;
    if buf.len() > MAX_STDIN_BYTES {
        return Err(RunError::Runtime(format!(
            "standard input exceeded {MAX_STDIN_BYTES} bytes; paste a smaller fragment"
        )));
    }
    let s = String::from_utf8(buf)
        .map_err(|e| RunError::Runtime(format!("standard input is not UTF-8: {e}")))?;
    if s.is_empty() {
        eprintln!("warning: --stdin produced empty input; continuing without it");
        Ok(None)
    } else {
        Ok(Some(s))
    }
}

/// Report an error if any raw argument was left unconsumed after parsing.
/// This is the non-consuming equivalent of noargs' `finish()` leftover check,
/// callable from handlers that receive `&mut RawArgs` (where `finish()`, which
/// takes ownership, cannot be called).
fn check_unconsumed_args(args: &noargs::RawArgs) -> Result<(), RunError> {
    if let Some((_, raw)) = args.remaining_args().next() {
        return Err(RunError::Usage(noargs::Error::other(
            args,
            format!("unexpected argument '{raw}' found"),
        )));
    }
    Ok(())
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

    match try_run_tell(&mut args)? {
        CommandOutcome::NotHandled => {}
        CommandOutcome::Done => return Ok(RunOutcome::Ok),
        CommandOutcome::Exit(exit) => return Ok(RunOutcome::Exit(exit)),
        CommandOutcome::Help => {
            if let Some(help) = args.finish()? {
                print!("{help}");
            }
            return Ok(RunOutcome::Ok);
        }
    }
    match try_run_approve(&mut args)? {
        CommandOutcome::NotHandled => {}
        CommandOutcome::Done => return Ok(RunOutcome::Ok),
        CommandOutcome::Exit(exit) => return Ok(RunOutcome::Exit(exit)),
        CommandOutcome::Help => {
            if let Some(help) = args.finish()? {
                print!("{help}");
            }
            return Ok(RunOutcome::Ok);
        }
    }
    match try_run_ask(&mut args)? {
        CommandOutcome::NotHandled => {}
        CommandOutcome::Done => return Ok(RunOutcome::Ok),
        CommandOutcome::Exit(_) => unreachable!("attini ask never exits"),
        CommandOutcome::Help => {
            if let Some(help) = args.finish()? {
                print!("{help}");
            }
            return Ok(RunOutcome::Ok);
        }
    }
    match try_run_status(&mut args)? {
        CommandOutcome::NotHandled => {}
        CommandOutcome::Done => return Ok(RunOutcome::Ok),
        CommandOutcome::Exit(exit) => return Ok(RunOutcome::Exit(exit)),
        CommandOutcome::Help => {
            if let Some(help) = args.finish()? {
                print!("{help}");
            }
            return Ok(RunOutcome::Ok);
        }
    }

    if let Some(help) = args.finish()? {
        print!("{help}");
    }
    Ok(RunOutcome::Ok)
}

fn try_run_tell(args: &mut noargs::RawArgs) -> Result<CommandOutcome, RunError> {
    if !noargs::cmd("tell")
        .doc("Tell the agent what to do; it runs one turn against a persistent session")
        .take(args)
        .is_present()
    {
        return Ok(CommandOutcome::NotHandled);
    }

    let model: String = noargs::opt("model")
        .ty("NAME")
        .doc("Model name")
        .default(DEFAULT_MODEL)
        .env(MODEL_ENV)
        .take(args)
        .then(|o| o.value().parse())?;
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .env(SESSION_ENV)
        .take(args)
        .then(|o| o.value().parse())?;
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
    let max_turns: usize = noargs::opt("max-turns")
        .ty("N")
        .doc(
            "Maximum agent-loop turns for this invocation. Reaching it stops the \
             session so it can be resumed with `attini approve`.",
        )
        .default(DEFAULT_MAX_TURNS_STR)
        .env(MAX_TURNS_ENV)
        .take(args)
        .then(|o| o.value().parse())?;
    let max_tokens: Option<u64> = noargs::opt("max-tokens")
        .ty("N")
        .doc("Maximum completion tokens per model call; `none` uses the model default")
        .env(MAX_TOKENS_ENV)
        .take(args)
        .present_and_then(|o| o.value().parse::<u64>())?;

    let temperature: Option<f64> = noargs::opt("temperature")
        .short('t')
        .ty("N")
        .doc(
            "Sampling temperature for model calls; 0 is deterministic. Default 0 for code editing.",
        )
        .env(TEMPERATURE_ENV)
        .take(args)
        .present_and_then(|o| o.value().parse::<f64>())?;

    let command_timeout_seconds: Option<u64> = noargs::opt("command-timeout")
        .ty("N")
        .doc(
            "Wall-clock cap in seconds on a single `command` tool call; the child is killed \
             (SIGTERM, then SIGKILL) on expiry. Default 180. `0` disables the cap.",
        )
        .env(COMMAND_TIMEOUT_ENV)
        .take(args)
        .present_and_then(|o| o.value().parse::<u64>())?;

    // No `.env()` here on purpose: the env var
    // (`ATTINI_COMPACTION_TRIGGER_TOKENS_KB`) is read separately by
    // `resolve_compaction_trigger_tokens`, which *warns and falls back*
    // on a bad value rather than aborting. Routing it through noargs'
    // `.env()` would make a typo a hard usage error, which is the right
    // call for the flag but not for the environment.
    let compaction_trigger_kb: Option<u64> = noargs::opt("compaction-trigger-kb")
        .ty("N")
        .doc(
            "Auto-compaction threshold in kilobytes (1 KB = 1024 tokens); when the previous \
             prompt exceeds it the next turn summarises first. Default 16 (16384 tokens). \
             Overrides ATTINI_COMPACTION_TRIGGER_TOKENS_KB.",
        )
        .take(args)
        .present_and_then(|o| o.value().parse::<u64>())?;

    let use_stdin = noargs::flag("stdin")
        .short('I')
        .doc(
            "Read standard input and append it to the prompt as auxiliary content \
             (not a file). Reads until EOF (from a terminal, press Ctrl+D); caps at 1 MiB.",
        )
        .take(args)
        .is_present();

    let prompt: Option<String> = noargs::arg("[PROMPT]")
        .doc("User prompt for this turn")
        .example("List the files in src/")
        .take(args)
        .present_and_then(|a| a.value().parse())?;

    if args.metadata().help_mode {
        return Ok(CommandOutcome::Help);
    }
    // Validate that no unexpected arguments remain before running the agent.
    // Without this, `attini tell hello world` silently dropped `world`.
    check_unconsumed_args(args)?;

    let tool_call_rate = parse_tool_call_rate(&tool_call_rate_raw)?;
    let session_tool_call_max = parse_session_tool_call_max(&session_tool_call_max_raw)?;

    let p = match prompt {
        Some(p) => p,
        None => {
            return Err(RunError::Runtime(
                "PROMPT is required (to approve a pending call, use `attini approve`)".to_string(),
            ));
        }
    };
    let p = if use_stdin {
        match read_stdin_auxiliary()? {
            Some(text) => append_stdin_aux(p, &text),
            None => p,
        }
    } else {
        p
    };
    let cont = Continuation::Prompt(p);

    let authorization = attini::sansio::permissions::Authorization::PerTool;

    let workspace_root = std::env::current_dir()
        .map_err(|e| RunError::Runtime(format!("failed to read current dir: {e}")))?;
    let cfg = TellConfig {
        session_name,
        model,
        max_tokens,
        workspace_root,
        max_turns,
        turn_tool_call_limit,
        tool_call_rate,
        session_tool_call_max,
        authorization,
        temperature,
        grant_request: tell_cli::GrantRequest::None,
        command_timeout_seconds,
        compaction_trigger_kb,
        follow_session_model: false,
    };
    match tell_cli::run(cfg, cont).map_err(|e| RunError::Runtime(e.to_string()))? {
        tell_cli::TellOutcome::Exit(code) => Ok(CommandOutcome::Exit(code)),
    }
}

// -------------------------------------------------------------------
// `attini approve` command
// -------------------------------------------------------------------

/// Resume a stopped session. Three cases, one human act — "yes, go on":
///
/// - the session has a pending tool call: it is approved and executed;
/// - the previous invocation ended in a transport failure before any
///   assistant output (e.g. connection reset): the identical request is
///   re-issued;
/// - the session stopped at `max_turns`: a fixed continuation message is
///   appended and the turn continues.
///
/// `--grant` only applies to the pending-call case and is ignored when
/// there is nothing to approve.
///
/// This is a dedicated subcommand rather than a `--approve` flag on
/// `tell`: `tell` keeps an optional positional `<PROMPT>`, so a
/// mistyped flag (`--approv`) is silently absorbed as the prompt and
/// starts an unintended model turn. A subcommand has no positional, so
/// an unknown flag fails cleanly. See `docs/design/approve-command.md`.
fn try_run_approve(args: &mut noargs::RawArgs) -> Result<CommandOutcome, RunError> {
    if !noargs::cmd("approve")
        .doc("Resume a stopped session: approve its pending tool call(s), or continue at max_turns")
        .take(args)
        .is_present()
    {
        return Ok(CommandOutcome::NotHandled);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .env(SESSION_ENV)
        .take(args)
        .then(|o| o.value().parse())?;
    // `approve` follows the session's own model (its plan and summariser
    // calls must match the model the conversation has been using), so
    // there is deliberately no `--model` option here.
    // --grant <SCOPE>: fold a persistent auto-approve rule into the
    // approval, so the rule is written without editing permissions.jsonl.
    let grant: tell_cli::GrantRequest = match noargs::opt("grant")
        .ty("SCOPE")
        .doc(
            "Also persist an auto-approve rule for the approved command: \
             `oneshot` (approve only, persist nothing — the default), \
             `session` (append the args-prefix to the session permissions.jsonl), or \
             `workspace` (append to the workspace-wide permissions.jsonl).",
        )
        .take(args)
        .present_and_then(|o| o.value().parse::<String>())?
    {
        Some(s) => match s.as_str() {
            "oneshot" => tell_cli::GrantRequest::Oneshot,
            "session" => tell_cli::GrantRequest::Session,
            "workspace" => tell_cli::GrantRequest::Workspace,
            other => {
                return Err(RunError::Runtime(format!(
                    "--grant must be 'oneshot', 'session', or 'workspace', got '{other}'"
                )));
            }
        },
        None => tell_cli::GrantRequest::None,
    };
    let command_timeout_seconds: Option<u64> = noargs::opt("command-timeout")
        .ty("N")
        .doc(
            "Wall-clock cap in seconds on a single `command` tool call; the child is killed \
             (SIGTERM, then SIGKILL) on expiry. Default 180. `0` disables the cap.",
        )
        .env(COMMAND_TIMEOUT_ENV)
        .take(args)
        .present_and_then(|o| o.value().parse::<u64>())?;
    // Resume honours the same turn cap as `tell` so a session cut off at
    // `max_turns` and continued here uses the limit the user chose (flag,
    // then `ATTINI_MAX_TURNS`, then the default).
    let max_turns: usize = noargs::opt("max-turns")
        .ty("N")
        .doc(
            "Maximum agent-loop turns for this invocation. Reaching it stops the \
             session so it can be resumed with `attini approve`.",
        )
        .default(DEFAULT_MAX_TURNS_STR)
        .env(MAX_TURNS_ENV)
        .take(args)
        .then(|o| o.value().parse())?;

    if args.metadata().help_mode {
        return Ok(CommandOutcome::Help);
    }
    // No positional: an unknown token (e.g. `--approv`) is a hard error,
    // not silently absorbed.
    check_unconsumed_args(args)?;

    let workspace_root = std::env::current_dir()
        .map_err(|e| RunError::Runtime(format!("failed to read current dir: {e}")))?;
    let cfg = TellConfig {
        session_name,
        model: DEFAULT_MODEL.to_string(),
        max_tokens: None,
        workspace_root,
        max_turns,
        turn_tool_call_limit: DEFAULT_TURN_TOOL_CALL_LIMIT_STR
            .parse()
            .map_err(|e| RunError::Runtime(format!("bad default turn limit: {e}")))?,
        tool_call_rate: parse_tool_call_rate(DEFAULT_TOOL_CALL_RATE_STR)?,
        session_tool_call_max: parse_session_tool_call_max(DEFAULT_SESSION_TOOL_CALL_MAX_STR)?,
        authorization: attini::sansio::permissions::Authorization::PerTool,
        temperature: None,
        grant_request: grant,
        command_timeout_seconds,
        compaction_trigger_kb: None,
        follow_session_model: true,
    };
    match tell_cli::run(cfg, Continuation::Approve).map_err(|e| RunError::Runtime(e.to_string()))? {
        tell_cli::TellOutcome::Exit(code) => Ok(CommandOutcome::Exit(code)),
    }
}

// -------------------------------------------------------------------
// `attini ask` command family
// -------------------------------------------------------------------

fn try_run_ask(args: &mut noargs::RawArgs) -> Result<CommandOutcome, RunError> {
    if !noargs::cmd("ask")
        .doc("Ask the model about the current state of a session (read-only)")
        .take(args)
        .is_present()
    {
        return Ok(CommandOutcome::NotHandled);
    }
    let session_name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .env(SESSION_ENV)
        .take(args)
        .then(|o| o.value().parse())?;
    let model: String = noargs::opt("model")
        .ty("NAME")
        .doc("Model name used for the summariser")
        .default(DEFAULT_MODEL)
        .env(MODEL_ENV)
        .take(args)
        .then(|o| o.value().parse())?;
    let limit: Option<usize> = noargs::opt("limit")
        .ty("N")
        .doc("Only summarise the most recent N conversation records")
        .take(args)
        .present_and_then(|o| o.value().parse::<usize>())?;
    let all = noargs::flag("all")
        .doc("Summarise the entire conversation, ignoring the last summary cutoff")
        .take(args)
        .is_present();
    let max_tokens: Option<u64> = noargs::opt("max-tokens")
        .ty("N")
        .doc("Maximum tokens for the summariser response")
        .env(MAX_TOKENS_ENV)
        .take(args)
        .present_and_then(|o| o.value().parse::<u64>())?;
    let question: Option<String> = noargs::arg("[QUESTION]")
        .doc("Optional question to focus the model's answer on the current state")
        .example("What is the model currently working on?")
        .take(args)
        .present_and_then(|a| a.value().parse())?;
    if args.metadata().help_mode {
        return Ok(CommandOutcome::Help);
    }
    // Validate that no unexpected arguments remain (e.g. a stray trailing token).
    check_unconsumed_args(args)?;
    session_cmd::run_ask(
        &session_name,
        question.as_deref(),
        &model,
        limit,
        all,
        max_tokens,
    )
    .map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(CommandOutcome::Done)
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

fn try_run_status(args: &mut noargs::RawArgs) -> Result<CommandOutcome, RunError> {
    if !noargs::cmd("status")
        .doc("Show one session's current state (lock, summary, pending) and aggregate metrics.")
        .take(args)
        .is_present()
    {
        return Ok(CommandOutcome::NotHandled);
    }
    let name: String = noargs::opt("session")
        .short('s')
        .ty("NAME")
        .doc("Session name; directory is .attini/<NAME>/")
        .default("main")
        .env(SESSION_ENV)
        .take(args)
        .then(|o| o.value().parse())?;
    let json = noargs::flag("json")
        .doc("Emit the whole overview as a JSON object")
        .take(args)
        .is_present();
    if args.metadata().help_mode {
        return Ok(CommandOutcome::Help);
    }
    session_cmd::run_status(&name, json).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(CommandOutcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attini::tell_cli::{
        DEFAULT_MAX_TURNS, DEFAULT_SESSION_TOOL_CALL_MAX, DEFAULT_TOOL_CALL_RATE_CALLS,
        DEFAULT_TOOL_CALL_RATE_WINDOW_SECS, DEFAULT_TURN_TOOL_CALL_LIMIT,
    };

    #[test]
    fn default_string_constants_stay_in_sync() {
        // The `.default()` argument of noargs::opt requires a
        // `&'static str`, so the CLI mirrors the numeric defaults in
        // `tell_cli` with these string constants. This test guards
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
        assert_eq!(DEFAULT_MAX_TURNS_STR, DEFAULT_MAX_TURNS.to_string());
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

    #[test]
    fn unconsumed_args_catch_extra_positions() {
        // `attini tell hello world` used to silently drop `world`; the
        // leftover check must now reject it as a usage error.
        let mut args = noargs::RawArgs::new(
            ["attini", "tell", "hello", "world"]
                .iter()
                .map(|s| s.to_string()),
        );
        noargs::cmd("tell").take(&mut args);
        noargs::arg("[PROMPT]").take(&mut args);
        assert!(check_unconsumed_args(&args).is_err());
    }

    #[test]
    fn unconsumed_args_ok_when_all_consumed() {
        let mut args =
            noargs::RawArgs::new(["attini", "tell", "hello"].iter().map(|s| s.to_string()));
        noargs::cmd("tell").take(&mut args);
        noargs::arg("[PROMPT]").take(&mut args);
        assert!(check_unconsumed_args(&args).is_ok());
    }

    #[test]
    fn append_stdin_aux_wraps_content() {
        let out = append_stdin_aux("summarise".to_string(), "the quick brown fox");
        assert_eq!(
            out,
            "summarise\n\n--- stdin ---\nthe quick brown fox\n--- end stdin ---"
        );
    }
}
