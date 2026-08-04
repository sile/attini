//! Terminal UI shell for the coding agent prototype.
//!
//! Rendering, input decoding, and per-key state transitions live in
//! [`crate::sansio::tui`]. This module keeps only the I/O side:
//!
//! - Reading terminal input asynchronously via
//!   [`Terminal::set_input_nonblocking`] + [`tokio::io::unix::AsyncFd`]
//!   so the tokio runtime can wait on it alongside the transport
//!   stream and tool-executor feedback
//! - Reading resize signals via the same async pattern on
//!   [`Terminal::signal_fd`]
//! - Turning [`RenderedGrid`] snapshots into
//!   [`tuinix::TerminalFrame`]s and drawing them
//! - Spawning transport / tool-executor tasks and merging their output
//!   into the [`AgentCore`] state machine

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tuinix::{
    EstimateCharWidth, Terminal, TerminalColor, TerminalFrame, TerminalInput, TerminalPosition,
    TerminalSize, TerminalStyle, try_nonblocking,
};
use unicode_width::UnicodeWidthChar;

use crate::deepseek::{DeepSeekClient, StreamEvent, TransportError};
use crate::sansio::agent::{
    Action, AgentCore, CommandInvocation, CommandOutputStream, Event, PatchInvocation,
    PatchPreview, PreviewHash, ReadOnlyTool, RequestId, ToolExecutionError, ToolOutcome,
};
use crate::sansio::deepseek::ChatRequest;
use crate::sansio::tui::{
    self, Color, KeyCode, KeyEffect, KeyInput, Region, RenderedGrid, Style, StyledLine, UiState,
};
use crate::tools::ToolExecutor;
use crate::tools::command::{CommandOutputMessage, run_command};

/// Runtime configuration for the TUI.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub model: String,
}

/// Run the TUI event loop until the user quits.
pub async fn run(client: DeepSeekClient, config: TuiConfig) -> io::Result<()> {
    let mut terminal = Terminal::new()?;
    let input_fd = terminal.set_input_nonblocking()?;
    let signal_fd = terminal.set_signal_nonblocking()?;

    // Terminal owns both fds and outlives the AsyncFd wrappers (the
    // whole function is one scope), so a bare RawFd newtype that does
    // not close on drop is what we want.
    let input_async = AsyncFd::with_interest(BorrowedFd(input_fd), Interest::READABLE)?;
    let signal_async = AsyncFd::with_interest(BorrowedFd(signal_fd), Interest::READABLE)?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ToolFeedback>();

    let workspace = std::env::current_dir()?;
    let tool_executor = Arc::new(
        ToolExecutor::new(&workspace)
            .map_err(|e| io::Error::other(format!("workspace {workspace:?}: {e}")))?,
    );

    let mut ui = UiState {
        model: config.model,
        draft: String::new(),
    };
    let mut agent = AgentCore::new();
    agent.set_workspace_display(workspace.display().to_string());
    let mut shell = Shell {
        stream: None,
        error_banner: None,
        tool_handles: HashMap::new(),
        tool_executor,
        event_tx,
        workspace: workspace.clone(),
        active_command: None,
        pending_commands: VecDeque::new(),
    };
    let mut size = terminal.size();
    let mut should_quit = false;

    draw(&mut terminal, size, &ui, &agent, &shell)?;

    while !should_quit {
        tokio::select! {
            biased;
            input_ready = input_async.readable() => {
                let mut guard = input_ready?;
                drain_input(&mut terminal, &mut ui, &mut agent, &client, &mut shell, &mut should_quit)?;
                guard.clear_ready();
            }
            signal_ready = signal_async.readable() => {
                let mut guard = signal_ready?;
                if let Some(new_size) = try_nonblocking(terminal.wait_for_resize())? {
                    size = new_size;
                }
                guard.clear_ready();
            }
            feedback = event_rx.recv() => {
                match feedback {
                    Some(ToolFeedback::Result { request, call_id, outcome }) => {
                        shell.tool_handles.remove(&(request, call_id.clone()));
                        on_command_finished(&mut shell, &call_id);
                        let actions = agent.handle_event(Event::ToolResult {
                            request,
                            call_id,
                            outcome,
                        });
                        apply_actions(&ui, actions, &client, &mut shell);
                    }
                    Some(ToolFeedback::CommandOutputChunk { request, call_id, stream, bytes }) => {
                        let actions = agent.handle_event(Event::CommandOutputChunk {
                            request,
                            call_id,
                            stream,
                            bytes,
                        });
                        apply_actions(&ui, actions, &client, &mut shell);
                    }
                    Some(ToolFeedback::PatchPreviewReady { request, call_id, preview_hashes, preview }) => {
                        shell.tool_handles.remove(&(request, call_id.clone()));
                        let actions = agent.handle_event(Event::PatchPreviewReady {
                            request,
                            call_id,
                            preview_hashes,
                            preview,
                        });
                        apply_actions(&ui, actions, &client, &mut shell);
                    }
                    Some(ToolFeedback::PatchPreviewFailed { request, call_id, outcome }) => {
                        // Preview itself failed (e.g. workspace boundary,
                        // no match). Feed it back as an immediate tool
                        // result so the model sees the error without
                        // going through approval.
                        shell.tool_handles.remove(&(request, call_id.clone()));
                        let actions = agent.handle_event(Event::ToolResult {
                            request,
                            call_id,
                            outcome,
                        });
                        apply_actions(&ui, actions, &client, &mut shell);
                    }
                    None => {
                        // All senders (only the tool executor tasks) dropped
                        // without the user quitting. In practice this cannot
                        // happen because `shell` still owns a sender; treat
                        // it as a safety net.
                        should_quit = true;
                    }
                }
            }
            recv = recv_stream(&mut shell.stream) => {
                let request = match shell.stream.as_ref() {
                    Some(s) => s.id,
                    None => continue,
                };
                match recv {
                    None => {
                        shell.stream = None;
                    }
                    Some(Ok(event)) => {
                        for core_event in translate_stream_event(event, request) {
                            let actions = agent.handle_event(core_event);
                            apply_actions(&ui, actions, &client, &mut shell);
                        }
                    }
                    Some(Err(err)) => {
                        let actions = agent.handle_event(Event::TransportError {
                            request,
                            message: err.to_string(),
                        });
                        apply_actions(&ui, actions, &client, &mut shell);
                        shell.stream = None;
                    }
                }
            }
        }
        draw(&mut terminal, size, &ui, &agent, &shell)?;
    }

    for (_, handle) in shell.tool_handles.drain() {
        handle.abort();
    }
    Ok(())
}

fn drain_input(
    terminal: &mut Terminal,
    ui: &mut UiState,
    agent: &mut AgentCore,
    client: &DeepSeekClient,
    shell: &mut Shell,
    should_quit: &mut bool,
) -> io::Result<()> {
    // `read_input` returns `Ok(None)` when its internal buffer is
    // empty; `try_nonblocking` maps the outer `EWOULDBLOCK` I/O error
    // to `Ok(None)`. Either outer-`None` or inner-`None` means "no
    // more input right now".
    while let Some(Some(input)) = try_nonblocking(terminal.read_input())? {
        match input {
            TerminalInput::Key(key) => {
                let outcome = tui::handle_key(to_sansio_key(key), ui, agent.view());
                *should_quit |= matches!(outcome.effect, KeyEffect::Quit);
                for event in outcome.events {
                    let actions = agent.handle_event(event);
                    apply_actions(ui, actions, client, shell);
                }
            }
            TerminalInput::Mouse(_) => {}
        }
    }
    Ok(())
}

fn draw(
    terminal: &mut Terminal,
    size: TerminalSize,
    ui: &UiState,
    agent: &AgentCore,
    shell: &Shell,
) -> io::Result<()> {
    let state = tui::build_render_state(ui, agent, shell.error_banner.as_deref());
    let grid = tui::render(&state, (size.rows, size.cols));
    let frame = render_grid_to_frame(size, &grid);
    terminal.draw(frame)
}

/// Non-owning wrapper: `AsyncFd::new` requires `T: AsRawFd`, and we
/// need to make sure `Drop` does not close the fd (the [`Terminal`]
/// keeps ownership). A bare `RawFd` newtype without a custom `Drop`
/// satisfies both.
#[derive(Debug, Clone, Copy)]
struct BorrowedFd(RawFd);

impl AsRawFd for BorrowedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

struct Shell {
    stream: Option<Stream>,
    error_banner: Option<String>,
    tool_handles: HashMap<(RequestId, String), JoinHandle<()>>,
    tool_executor: Arc<ToolExecutor>,
    event_tx: mpsc::UnboundedSender<ToolFeedback>,
    /// Workspace directory child processes are spawned in.
    workspace: PathBuf,
    /// The command currently running, if any. Additional
    /// `ExecuteCommand` actions are queued in `pending_commands`
    /// until this one drains.
    active_command: Option<ActiveCommand>,
    /// FIFO queue of command executions waiting on `active_command`.
    pending_commands: VecDeque<PendingCommand>,
}

struct ActiveCommand {
    request: RequestId,
    call_id: String,
    cancel: Arc<Notify>,
    _task: JoinHandle<()>,
}

struct PendingCommand {
    request: RequestId,
    call_id: String,
    invocation: CommandInvocation,
}

#[derive(Debug)]
enum ToolFeedback {
    Result {
        request: RequestId,
        call_id: String,
        outcome: ToolOutcome,
    },
    PatchPreviewReady {
        request: RequestId,
        call_id: String,
        preview_hashes: Vec<PreviewHash>,
        preview: PatchPreview,
    },
    PatchPreviewFailed {
        request: RequestId,
        call_id: String,
        outcome: ToolOutcome,
    },
    CommandOutputChunk {
        request: RequestId,
        call_id: String,
        stream: CommandOutputStream,
        bytes: Vec<u8>,
    },
}

struct Stream {
    id: RequestId,
    rx: mpsc::Receiver<Result<StreamEvent, TransportError>>,
}

async fn recv_stream(stream: &mut Option<Stream>) -> Option<Result<StreamEvent, TransportError>> {
    match stream.as_mut() {
        Some(s) => s.rx.recv().await,
        None => std::future::pending().await,
    }
}

fn apply_actions(ui: &UiState, actions: Vec<Action>, client: &DeepSeekClient, shell: &mut Shell) {
    for action in actions {
        match action {
            Action::StartRequest { id, messages } => {
                // Clearing the banner here (rather than inside handle_key)
                // keeps handle_key pure and pins the reset to "a new
                // request just started" instead of "Enter was pressed".
                shell.error_banner = None;
                let mut tools = ReadOnlyTool::definitions();
                tools.push(PatchInvocation::definition());
                tools.push(CommandInvocation::definition());
                let request = ChatRequest::new(ui.model.clone(), messages).with_tools(tools);
                let rx = client.call(request);
                shell.stream = Some(Stream { id, rx });
            }
            Action::CancelRequest { .. } => {
                shell.stream = None;
                shell.error_banner = Some("cancelled".to_string());
            }
            Action::ExecuteTool {
                request,
                call_id,
                invocation,
            } => {
                let exec = shell.tool_executor.clone();
                let tx = shell.event_tx.clone();
                let key = (request, call_id.clone());
                let handle = tokio::task::spawn_blocking(move || {
                    let outcome = exec.execute(invocation);
                    let _ = tx.send(ToolFeedback::Result {
                        request,
                        call_id,
                        outcome,
                    });
                });
                shell.tool_handles.insert(key, handle);
            }
            Action::CancelToolExecution { request } => {
                shell.tool_handles.retain(|(req, _), h| {
                    if *req == request {
                        h.abort();
                        false
                    } else {
                        true
                    }
                });
                cancel_commands_for_request(shell, request);
                shell.error_banner = Some("cancelled".to_string());
            }
            Action::PreviewPatch {
                request,
                call_id,
                invocation,
            } => {
                spawn_preview_patch(shell, request, call_id, invocation);
            }
            Action::ApplyPatch {
                request,
                call_id,
                invocation,
                preview_hashes,
            } => {
                spawn_apply_patch(shell, request, call_id, invocation, preview_hashes);
            }
            Action::ExecuteCommand {
                request,
                call_id,
                invocation,
            } => {
                if shell.active_command.is_some() {
                    shell.pending_commands.push_back(PendingCommand {
                        request,
                        call_id,
                        invocation,
                    });
                } else {
                    start_command(shell, request, call_id, invocation);
                }
            }
            Action::ReportError { message } => {
                shell.error_banner = Some(message);
            }
            Action::Redraw => {}
        }
    }
}

fn spawn_preview_patch(
    shell: &mut Shell,
    request: RequestId,
    call_id: String,
    invocation: PatchInvocation,
) {
    let exec = shell.tool_executor.clone();
    let tx = shell.event_tx.clone();
    let key = (request, call_id.clone());
    let handle = tokio::task::spawn_blocking(move || match exec.preview_patch(&invocation) {
        Ok((preview_hashes, preview)) => {
            let _ = tx.send(ToolFeedback::PatchPreviewReady {
                request,
                call_id,
                preview_hashes,
                preview,
            });
        }
        Err(err) => {
            let _ = tx.send(ToolFeedback::PatchPreviewFailed {
                request,
                call_id,
                outcome: ToolOutcome::Err(ToolExecutionError::Patch(err)),
            });
        }
    });
    shell.tool_handles.insert(key, handle);
}

fn spawn_apply_patch(
    shell: &mut Shell,
    request: RequestId,
    call_id: String,
    invocation: PatchInvocation,
    preview_hashes: Vec<PreviewHash>,
) {
    let exec = shell.tool_executor.clone();
    let tx = shell.event_tx.clone();
    let key = (request, call_id.clone());
    let handle = tokio::task::spawn_blocking(move || {
        let outcome = match exec.apply_patch(&invocation, &preview_hashes) {
            Ok(_) => ToolOutcome::Ok(r#"{"applied":true}"#.to_string()),
            Err(err) => ToolOutcome::Err(ToolExecutionError::Patch(err)),
        };
        let _ = tx.send(ToolFeedback::Result {
            request,
            call_id,
            outcome,
        });
    });
    shell.tool_handles.insert(key, handle);
}

fn start_command(
    shell: &mut Shell,
    request: RequestId,
    call_id: String,
    invocation: CommandInvocation,
) {
    let cancel = Arc::new(Notify::new());
    let cancel_task = cancel.clone();
    let cwd = shell.workspace.clone();
    let tx = shell.event_tx.clone();
    let call_id_for_stream = call_id.clone();
    let call_id_for_result = call_id.clone();

    // Forward the CommandOutputMessage stream from run_command into
    // our ToolFeedback::CommandOutputChunk stream, then await the
    // final result and send a single ToolFeedback::Result.
    let task = tokio::spawn(async move {
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<CommandOutputMessage>();
        let tx_forward = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(msg) = chunk_rx.recv().await {
                let _ = tx_forward.send(ToolFeedback::CommandOutputChunk {
                    request,
                    call_id: call_id_for_stream.clone(),
                    stream: msg.stream,
                    bytes: msg.bytes,
                });
            }
        });
        let result = run_command(cwd, invocation, cancel_task, chunk_tx).await;
        // Dropping chunk_tx above (goes out of scope with run_command
        // returning) lets the forwarder loop exit; wait for it before
        // reporting the result so ordering stays "chunks → result".
        let _ = forwarder.await;
        let outcome = match result {
            Ok(res) => ToolOutcome::Ok(res.to_json_string()),
            Err(err) => ToolOutcome::Err(ToolExecutionError::Command(
                crate::sansio::agent::CommandError::SpawnFailed {
                    message: err.to_string(),
                },
            )),
        };
        let _ = tx.send(ToolFeedback::Result {
            request,
            call_id: call_id_for_result,
            outcome,
        });
    });

    shell.active_command = Some(ActiveCommand {
        request,
        call_id: call_id.clone(),
        cancel,
        _task: task,
    });
    // Also track in tool_handles so that generic CancelToolExecution
    // for the wider request can find and remove the entry.
    // The task itself is owned by ActiveCommand; use a dummy no-op
    // handle here.
    // (No JoinHandle stored here — real join lives in ActiveCommand.)
    let _ = call_id;
}

fn on_command_finished(shell: &mut Shell, call_id: &str) {
    if shell
        .active_command
        .as_ref()
        .is_some_and(|c| c.call_id == call_id)
    {
        shell.active_command = None;
        if let Some(next) = shell.pending_commands.pop_front() {
            start_command(shell, next.request, next.call_id, next.invocation);
        }
    }
}

fn cancel_commands_for_request(shell: &mut Shell, request: RequestId) {
    if let Some(active) = shell.active_command.as_ref()
        && active.request == request
    {
        active.cancel.notify_one();
    }
    // Discard any queued commands whose request no longer matters.
    // Their absence will naturally surface as "no ToolResult for this
    // call_id" — but core's CancelToolExecution already dropped the
    // entire pending, so they will not be looked up.
    shell.pending_commands.retain(|p| p.request != request);
}

fn translate_stream_event(event: StreamEvent, request: RequestId) -> Vec<Event> {
    match event {
        StreamEvent::ContentDelta(text) => vec![Event::ContentDelta { request, text }],
        StreamEvent::ReasoningDelta(text) => vec![Event::ReasoningDelta { request, text }],
        StreamEvent::ToolCallDelta {
            index,
            id,
            function_name,
            arguments_fragment,
        } => vec![Event::ToolCallDelta {
            request,
            index,
            id,
            function_name,
            arguments_fragment,
        }],
        StreamEvent::Comment(_) => Vec::new(),
        StreamEvent::Finish { reason } => vec![Event::Finish { request, reason }],
    }
}

fn to_sansio_key(key: tuinix::KeyInput) -> KeyInput {
    let code = match key.code {
        tuinix::KeyCode::Enter => KeyCode::Enter,
        tuinix::KeyCode::Escape => KeyCode::Escape,
        tuinix::KeyCode::Backspace => KeyCode::Backspace,
        tuinix::KeyCode::Char(c) => KeyCode::Char(c),
        _ => KeyCode::Other,
    };
    KeyInput {
        ctrl: key.ctrl,
        alt: key.alt,
        code,
    }
}

struct UnicodeCharWidth;

impl EstimateCharWidth for UnicodeCharWidth {
    fn estimate_char_width(&self, c: char) -> usize {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

fn render_grid_to_frame(
    size: TerminalSize,
    grid: &RenderedGrid,
) -> TerminalFrame<UnicodeCharWidth> {
    let mut main = TerminalFrame::with_char_width_estimator(size, UnicodeCharWidth);
    draw_region(&mut main, size, &grid.header);
    draw_region(&mut main, size, &grid.body);
    if let Some(error) = grid.error.as_ref() {
        draw_region(&mut main, size, error);
    }
    draw_region(&mut main, size, &grid.prompt);
    main
}

fn draw_region(main: &mut TerminalFrame<UnicodeCharWidth>, size: TerminalSize, region: &Region) {
    for (offset, line) in region.lines.iter().enumerate() {
        let row = region.top + offset;
        if row >= size.rows {
            break;
        }
        let sub_size = TerminalSize::rows_cols(1, size.cols);
        let mut sub = TerminalFrame::with_char_width_estimator(sub_size, UnicodeCharWidth);
        write_styled_line(&mut sub, line);
        main.draw(TerminalPosition::row_col(row, 0), &sub);
    }
}

fn write_styled_line(frame: &mut TerminalFrame<UnicodeCharWidth>, line: &StyledLine) {
    for span in &line.spans {
        let style = to_terminal_style(&span.style);
        let _ = write!(frame, "{style}{}{}", span.text, TerminalStyle::RESET);
    }
}

fn to_terminal_style(style: &Style) -> TerminalStyle {
    let mut ts = TerminalStyle::new();
    if style.bold {
        ts = ts.bold();
    }
    if style.dim {
        ts = ts.dim();
    }
    if style.italic {
        ts = ts.italic();
    }
    if style.underline {
        ts = ts.underline();
    }
    if let Some(fg) = style.fg {
        ts = ts.fg_color(to_terminal_color(fg));
    }
    if let Some(bg) = style.bg {
        ts = ts.bg_color(to_terminal_color(bg));
    }
    ts
}

fn to_terminal_color(color: Color) -> TerminalColor {
    match color {
        Color::Cyan => TerminalColor::CYAN,
        Color::Green => TerminalColor::GREEN,
        Color::Red => TerminalColor::RED,
        Color::Yellow => TerminalColor::YELLOW,
        Color::BrightBlack => TerminalColor::BRIGHT_BLACK,
    }
}
