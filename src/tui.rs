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

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tuinix::{
    EstimateCharWidth, Terminal, TerminalColor, TerminalFrame, TerminalInput, TerminalPosition,
    TerminalSize, TerminalStyle, try_nonblocking,
};
use unicode_width::UnicodeWidthChar;

use crate::deepseek::{DeepSeekClient, StreamEvent, TransportError};
use crate::sansio::agent::{Action, AgentCore, Event, RequestId, ToolOutcome};
use crate::sansio::deepseek::ChatRequest;
use crate::sansio::tui::{
    self, Color, KeyCode, KeyEffect, KeyInput, Region, RenderedGrid, Style, StyledLine, UiState,
};
use crate::tools::ToolExecutor;

/// Runtime configuration for the TUI.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub model: String,
}

/// Run the TUI event loop until the user quits.
pub async fn run(client: DeepSeekClient, config: TuiConfig) -> io::Result<()> {
    let mut terminal = Terminal::new()?;
    // `set_input_nonblocking` opens a fresh fd on the tty device and
    // returns that; O_NONBLOCK on the new fd does NOT propagate to
    // the stdout fd. Applying O_NONBLOCK directly to `input_fd()`
    // instead would flip stdout to non-blocking too (same open file
    // description) and make `terminal.draw()` fail with EAGAIN once
    // output exceeds ~1 KiB.
    let input_fd = terminal.set_input_nonblocking()?;
    let signal_fd = terminal.signal_fd();
    tuinix::set_nonblocking(signal_fd)?;

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
    let mut shell = Shell {
        stream: None,
        error_banner: None,
        tool_handles: HashMap::new(),
        tool_executor,
        event_tx,
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
}

#[derive(Debug)]
enum ToolFeedback {
    Result {
        request: RequestId,
        call_id: String,
        outcome: ToolOutcome,
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
                let request = ChatRequest::new(ui.model.clone(), messages)
                    .with_tools(crate::sansio::agent::ReadOnlyTool::definitions());
                let rx = client.call(request);
                shell.stream = Some(Stream { id, rx });
            }
            Action::CancelRequest { .. } => {
                shell.stream = None;
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
            }
            Action::PreviewPatch { .. } | Action::ApplyPatch { .. } => {
                // Patch executor wiring lands in a follow-up commit;
                // without it, a turn containing a patch call stays
                // parked in `AwaitingApproval` forever. Guarded by
                // the fact that PatchInvocation::definition() is not
                // yet advertised on the wire, so the model cannot
                // produce a patch tool call in the meantime.
            }
            Action::ReportError { message } => {
                shell.error_banner = Some(message);
            }
            Action::Redraw => {}
        }
    }
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
