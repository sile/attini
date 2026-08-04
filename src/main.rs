use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use attini::agent_cli::{self, AgentConfig, Continuation, DEFAULT_MAX_TURNS};
use attini::deepseek::{DeepSeekClient, StreamEvent, TransportError};
use attini::sansio::deepseek::{ChatMessage, ChatRequest};
use attini::tui::{self, TuiConfig};
use tokio::sync::mpsc;

const EXIT_USAGE: u8 = 2;
const EXIT_RUNTIME: u8 = 1;
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
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

impl From<TransportError> for RunError {
    fn from(err: TransportError) -> Self {
        Self::Runtime(err.to_string())
    }
}

async fn run() -> Result<RunOutcome, RunError> {
    let mut args = noargs::raw_args();
    args.metadata_mut().app_name = env!("CARGO_PKG_NAME");
    args.metadata_mut().app_description = "DeepSeek-based coding agent prototype.";

    if noargs::VERSION_FLAG.take(&mut args).is_present() {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(RunOutcome::Ok);
    }
    noargs::HELP_FLAG.take_help(&mut args);

    try_run_chat(&mut args).await?;
    try_run_tui(&mut args).await?;
    if let Some(exit) = try_run_agent(&mut args)? {
        return Ok(RunOutcome::Exit(exit));
    }

    if let Some(help) = args.finish()? {
        print!("{help}");
    }
    Ok(RunOutcome::Ok)
}

async fn try_run_tui(args: &mut noargs::RawArgs) -> Result<(), RunError> {
    if !noargs::cmd("tui")
        .doc("Launch the interactive terminal UI")
        .take(args)
        .is_present()
    {
        return Ok(());
    }

    let model: String = noargs::opt("model")
        .ty("NAME")
        .doc("Model name")
        .default(DEFAULT_MODEL)
        .take(args)
        .then(|o| o.value().parse())?;
    let transcript_path: Option<PathBuf> = noargs::opt("transcript")
        .ty("PATH")
        .doc("Append session records as JSON Lines to this file")
        .take(args)
        .present_and_then(|o| o.value().parse::<PathBuf>())?;
    let metrics_snapshot_interval_secs: Option<u64> = noargs::opt("metrics-snapshot-interval")
        .ty("SECONDS")
        .doc("Emit a metrics_snapshot record to the transcript every N seconds (requires --transcript)")
        .take(args)
        .present_and_then(|o| o.value().parse::<u64>())?;

    if args.metadata().help_mode {
        return Ok(());
    }

    let metrics_snapshot_interval = match metrics_snapshot_interval_secs {
        Some(0) => {
            return Err(RunError::Runtime(
                "--metrics-snapshot-interval must be at least 1 second".to_string(),
            ));
        }
        Some(n) => {
            if transcript_path.is_none() {
                return Err(RunError::Runtime(
                    "--metrics-snapshot-interval requires --transcript".to_string(),
                ));
            }
            Some(std::time::Duration::from_secs(n))
        }
        None => None,
    };

    let client = DeepSeekClient::from_env()?;
    let config = TuiConfig {
        model,
        transcript_path,
        metrics_snapshot_interval,
    };
    tui::run(client, config)
        .await
        .map_err(|err| RunError::Runtime(err.to_string()))?;
    Ok(())
}

async fn try_run_chat(args: &mut noargs::RawArgs) -> Result<(), RunError> {
    if !noargs::cmd("chat")
        .doc("Send a single prompt to DeepSeek and stream the response to stdout")
        .take(args)
        .is_present()
    {
        return Ok(());
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
        .doc("Print reasoning_content deltas from thinking mode to stderr")
        .take(args)
        .is_present();
    let prompt: String = noargs::arg("<PROMPT>")
        .doc("User prompt to send")
        .example("Explain SSE decoding in one sentence.")
        .take(args)
        .then(|a| a.value().parse())?;

    if args.metadata().help_mode {
        return Ok(());
    }

    let mut messages = Vec::new();
    if let Some(text) = system {
        messages.push(ChatMessage::System(text));
    }
    messages.push(ChatMessage::User(prompt));
    let request = ChatRequest::new(model, messages);

    let client = DeepSeekClient::from_env()?;
    let mut rx = client.call(request);
    stream_response(&mut rx, show_reasoning).await?;
    Ok(())
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

    let workspace_root = std::env::current_dir()
        .map_err(|e| RunError::Runtime(format!("failed to read current dir: {e}")))?;
    let cfg = AgentConfig {
        session_name,
        model,
        workspace_root,
        system_prompt: system,
        show_reasoning,
        max_turns: DEFAULT_MAX_TURNS,
    };
    let exit = agent_cli::run(cfg, cont).map_err(|e| RunError::Runtime(e.to_string()))?;
    Ok(Some(exit))
}

async fn stream_response(
    rx: &mut mpsc::Receiver<Result<StreamEvent, TransportError>>,
    show_reasoning: bool,
) -> Result<(), TransportError> {
    let mut stdout = io::stdout();
    let mut wrote_content = false;
    while let Some(event) = rx.recv().await {
        match event? {
            StreamEvent::ContentDelta(text) => {
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
                wrote_content = true;
            }
            StreamEvent::ReasoningDelta(text) => {
                if show_reasoning {
                    let mut stderr = io::stderr();
                    let _ = stderr.write_all(text.as_bytes());
                    let _ = stderr.flush();
                }
            }
            StreamEvent::ToolCallDelta { .. } => {
                // The one-shot CLI does not run the tool loop; the
                // TUI shell wires tool_call fragments into AgentCore.
            }
            StreamEvent::Comment(_) => {}
            StreamEvent::Finish { .. } => {
                if wrote_content {
                    let _ = writeln!(stdout);
                }
            }
        }
    }
    Ok(())
}
