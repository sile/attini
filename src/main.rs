use std::io::{self, Write};
use std::process::ExitCode;

use attini::deepseek::{DeepSeekClient, StreamEvent, TransportError};
use attini::sansio::deepseek::{ChatMessage, ChatRequest, Role};
use attini::tui::{self, TuiConfig};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const EXIT_USAGE: u8 = 2;
const EXIT_RUNTIME: u8 = 1;
const DEFAULT_MODEL: &str = "deepseek-v4-flash";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    init_tracing();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
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

async fn run() -> Result<(), RunError> {
    let mut args = noargs::raw_args();
    args.metadata_mut().app_name = env!("CARGO_PKG_NAME");
    args.metadata_mut().app_description = "DeepSeek-based coding agent prototype.";

    if noargs::VERSION_FLAG.take(&mut args).is_present() {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    noargs::HELP_FLAG.take_help(&mut args);

    try_run_chat(&mut args).await?;
    try_run_tui(&mut args).await?;

    if let Some(help) = args.finish()? {
        print!("{help}");
    }
    Ok(())
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

    if args.metadata().help_mode {
        return Ok(());
    }

    let client = DeepSeekClient::from_env()?;
    let config = TuiConfig { model };
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
        messages.push(ChatMessage {
            role: Role::System,
            content: text,
        });
    }
    messages.push(ChatMessage {
        role: Role::User,
        content: prompt,
    });
    let request = ChatRequest::new(model, messages);

    let client = DeepSeekClient::from_env()?;
    let mut rx = client.call(request);
    stream_response(&mut rx, show_reasoning).await?;
    Ok(())
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
            StreamEvent::Comment(comment) => {
                tracing::trace!(comment = %comment, "sse comment");
            }
            StreamEvent::Finish { reason } => {
                if wrote_content {
                    let _ = writeln!(stdout);
                }
                tracing::info!(?reason, "stream finished");
            }
        }
    }
    Ok(())
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("attini=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .init();
}
