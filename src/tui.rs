//! Terminal UI shell for the coding agent prototype.
//!
//! Rendering, input decoding, and per-key state transitions live in
//! [`crate::sansio::tui`]. This module keeps only the I/O side:
//!
//! - Reading terminal events with [`Terminal::poll_event`]
//! - Turning [`RenderedGrid`] snapshots into
//!   [`tuinix::TerminalFrame`]s and drawing them
//! - Running the transport `tokio::spawn` for a request and forwarding
//!   its [`StreamEvent`]s through the async loop
//! - Splitting the terminal loop into a `spawn_blocking` task so
//!   [`Terminal::draw`] never has to compete with a non-blocking
//!   stdout (see tuinix issue on `set_nonblocking` propagation).

use std::fmt::Write as _;
use std::io;
use std::time::Duration;

use tokio::sync::mpsc;
use tuinix::{
    EstimateCharWidth, Terminal, TerminalColor, TerminalEvent, TerminalFrame, TerminalInput,
    TerminalPosition, TerminalSize, TerminalStyle,
};
use unicode_width::UnicodeWidthChar;

use crate::deepseek::{DeepSeekClient, StreamEvent, TransportError};
use crate::sansio::agent::{Action, AgentCore, Event, RequestId};
use crate::sansio::deepseek::ChatRequest;
use crate::sansio::tui::{
    self, Color, KeyCode, KeyEffect, KeyInput, Region, RenderState, RenderedGrid, Style,
    StyledLine, UiState,
};

/// Runtime configuration for the TUI.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub model: String,
}

const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run the TUI event loop until the user quits.
pub async fn run(client: DeepSeekClient, config: TuiConfig) -> io::Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TerminalUpdate>();
    let (render_tx, render_rx) = mpsc::unbounded_channel::<RenderState>();

    let terminal_handle = tokio::task::spawn_blocking(move || terminal_loop(event_tx, render_rx));

    let mut ui = UiState {
        model: config.model,
        draft: String::new(),
    };
    let mut agent = AgentCore::new();
    let mut stream: Option<Stream> = None;
    let mut error_banner: Option<String> = None;
    let mut should_quit = false;

    let _ = render_tx.send(tui::build_render_state(
        &ui,
        &agent,
        error_banner.as_deref(),
    ));

    while !should_quit {
        tokio::select! {
            biased;
            update = event_rx.recv() => {
                match update {
                    Some(TerminalUpdate::Key(key)) => {
                        let outcome = tui::handle_key(key, &mut ui, agent.view());
                        should_quit |= matches!(outcome.effect, KeyEffect::Quit);
                        for event in outcome.events {
                            let actions = agent.handle_event(event);
                            apply_actions(&ui, actions, &client, &mut stream, &mut error_banner);
                        }
                    }
                    Some(TerminalUpdate::Resize) => {
                        // Blocking task tracks size internally; a fresh
                        // render snapshot below picks up the new size.
                    }
                    None => {
                        should_quit = true;
                    }
                }
            }
            recv = recv_stream(&mut stream) => {
                let request = match stream.as_ref() {
                    Some(s) => s.id,
                    None => continue,
                };
                match recv {
                    None => {
                        stream = None;
                    }
                    Some(Ok(event)) => {
                        for core_event in translate_stream_event(event, request) {
                            let actions = agent.handle_event(core_event);
                            apply_actions(&ui, actions, &client, &mut stream, &mut error_banner);
                        }
                    }
                    Some(Err(err)) => {
                        let actions = agent.handle_event(Event::TransportError {
                            request,
                            message: err.to_string(),
                        });
                        apply_actions(&ui, actions, &client, &mut stream, &mut error_banner);
                        stream = None;
                    }
                }
            }
        }
        let _ = render_tx.send(tui::build_render_state(
            &ui,
            &agent,
            error_banner.as_deref(),
        ));
    }

    drop(render_tx);
    match terminal_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(join_err) => Err(io::Error::other(join_err.to_string())),
    }
}

#[derive(Debug)]
enum TerminalUpdate {
    Key(KeyInput),
    Resize,
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

fn apply_actions(
    ui: &UiState,
    actions: Vec<Action>,
    client: &DeepSeekClient,
    stream: &mut Option<Stream>,
    error_banner: &mut Option<String>,
) {
    for action in actions {
        match action {
            Action::StartRequest { id, messages } => {
                // Clearing the banner here (rather than inside handle_key)
                // keeps handle_key pure and pins the reset to "a new
                // request just started" instead of "Enter was pressed".
                *error_banner = None;
                let request = ChatRequest::new(ui.model.clone(), messages);
                let rx = client.call(request);
                *stream = Some(Stream { id, rx });
            }
            Action::CancelRequest { .. } => {
                *stream = None;
            }
            Action::ExecuteTool { .. } | Action::CancelToolExecution { .. } => {
                // Tool executor wiring lands in a follow-up commit; for
                // now these actions have no side effect in the shell,
                // which effectively wedges any tool-loop turn until the
                // wiring is complete. Guarded by the compile-time
                // absence of code that emits StreamEvent::ToolCallDelta.
            }
            Action::ReportError { message } => {
                *error_banner = Some(message);
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

fn terminal_loop(
    event_tx: mpsc::UnboundedSender<TerminalUpdate>,
    mut render_rx: mpsc::UnboundedReceiver<RenderState>,
) -> io::Result<()> {
    let mut terminal = Terminal::new()?;
    let mut size = terminal.size();

    loop {
        let mut latest = None;
        let mut render_closed = false;
        loop {
            match render_rx.try_recv() {
                Ok(data) => latest = Some(data),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    render_closed = true;
                    break;
                }
            }
        }
        if let Some(state) = latest {
            let grid = tui::render(&state, (size.rows, size.cols));
            let frame = render_grid_to_frame(size, &grid);
            terminal.draw(frame)?;
        }
        if render_closed {
            break;
        }

        match terminal.poll_event(&[], &[], Some(TERMINAL_POLL_INTERVAL))? {
            Some(TerminalEvent::Input(TerminalInput::Key(key))) => {
                if event_tx
                    .send(TerminalUpdate::Key(to_sansio_key(key)))
                    .is_err()
                {
                    break;
                }
            }
            Some(TerminalEvent::Input(TerminalInput::Mouse(_))) => {}
            Some(TerminalEvent::Resize(new_size)) => {
                size = new_size;
                if event_tx.send(TerminalUpdate::Resize).is_err() {
                    break;
                }
            }
            Some(TerminalEvent::FdReady { .. }) => {}
            None => {}
        }
    }
    Ok(())
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
        Color::BrightBlack => TerminalColor::BRIGHT_BLACK,
    }
}
