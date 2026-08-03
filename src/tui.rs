//! Terminal UI for the coding agent prototype.
//!
//! The UI splits work between two tasks:
//!
//! - **Blocking task**: owns the [`Terminal`], polls terminal events
//!   with [`Terminal::poll_event`], forwards keys and resize signals
//!   over an mpsc channel, and draws snapshots of the UI state sent by
//!   the async task.
//! - **Async task** (this function): owns [`AgentCore`], the draft
//!   prompt buffer, and the currently-streaming transport receiver. It
//!   multiplexes terminal events and stream events with
//!   [`tokio::select!`] and pushes a rendering snapshot to the
//!   blocking task after each state change.
//!
//! Splitting this way avoids a subtle Unix gotcha: putting the input
//! fd into non-blocking mode (as [`tokio::io::unix::AsyncFd`] requires)
//! also flips stdout into non-blocking mode when both share a single
//! open file description, which then makes [`Terminal::draw`] fail
//! with `EAGAIN`. `poll_event` keeps everything in blocking mode and
//! sidesteps the issue.
//!
//! Character widths follow Unicode Standard Annex #11 via
//! [`unicode_width`], so CJK characters and other wide glyphs occupy
//! two columns as expected. Long lines are currently clipped at the
//! frame edge rather than wrapped, and there is no scrollback; both
//! are deferred.

use std::fmt::Write as _;
use std::io;
use std::time::Duration;

use tokio::sync::mpsc;
use tuinix::{
    EstimateCharWidth, KeyCode, KeyInput, Terminal, TerminalColor, TerminalEvent, TerminalFrame,
    TerminalInput, TerminalPosition, TerminalSize, TerminalStyle,
};
use unicode_width::UnicodeWidthChar;

use crate::deepseek::{DeepSeekClient, StreamEvent, TransportError};
use crate::sansio::agent::{Action, AgentCore, Event, PendingResponse, RequestId, Status};
use crate::sansio::deepseek::{ChatMessage, ChatRequest, Role};

/// Runtime configuration for the TUI.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub model: String,
}

const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run the TUI event loop until the user quits.
pub async fn run(client: DeepSeekClient, config: TuiConfig) -> io::Result<()> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TerminalUpdate>();
    let (render_tx, render_rx) = mpsc::unbounded_channel::<RenderData>();

    let terminal_handle = tokio::task::spawn_blocking(move || terminal_loop(event_tx, render_rx));

    let mut ui = UiState::new(config);
    let mut agent = AgentCore::new();
    let mut stream: Option<Stream> = None;
    let mut error_banner: Option<String> = None;
    let mut should_quit = false;

    let _ = render_tx.send(build_render(&ui, &agent, error_banner.as_deref()));

    while !should_quit {
        tokio::select! {
            biased;
            update = event_rx.recv() => {
                match update {
                    Some(TerminalUpdate::Key(key)) => {
                        handle_key(
                            key,
                            &mut ui,
                            &mut agent,
                            &client,
                            &mut stream,
                            &mut error_banner,
                            &mut should_quit,
                        );
                    }
                    Some(TerminalUpdate::Resize) => {
                        // The blocking task tracks size internally; a
                        // fresh render snapshot below is enough for it
                        // to redraw with the new dimensions.
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
        let _ = render_tx.send(build_render(&ui, &agent, error_banner.as_deref()));
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

#[derive(Debug, Clone)]
struct UiState {
    model: String,
    draft: String,
}

impl UiState {
    fn new(config: TuiConfig) -> Self {
        Self {
            model: config.model,
            draft: String::new(),
        }
    }
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

fn handle_key(
    key: KeyInput,
    ui: &mut UiState,
    agent: &mut AgentCore,
    client: &DeepSeekClient,
    stream: &mut Option<Stream>,
    error_banner: &mut Option<String>,
    should_quit: &mut bool,
) {
    let actions = match (key.ctrl, key.code) {
        (true, KeyCode::Char('c')) => {
            if agent.active_request().is_some() {
                agent.handle_event(Event::Cancel)
            } else {
                *should_quit = true;
                Vec::new()
            }
        }
        (true, KeyCode::Char('d')) => {
            *should_quit = true;
            Vec::new()
        }
        (false, KeyCode::Escape) => {
            if agent.active_request().is_some() {
                agent.handle_event(Event::Cancel)
            } else {
                Vec::new()
            }
        }
        (false, KeyCode::Enter) => {
            if ui.draft.is_empty() || agent.active_request().is_some() {
                Vec::new()
            } else {
                *error_banner = None;
                let text = std::mem::take(&mut ui.draft);
                agent.handle_event(Event::UserMessage(text))
            }
        }
        (false, KeyCode::Backspace) => {
            if agent.active_request().is_none() {
                ui.draft.pop();
            }
            Vec::new()
        }
        (_, KeyCode::Char(c)) if !key.ctrl && !key.alt => {
            if agent.active_request().is_none() {
                ui.draft.push(c);
            }
            Vec::new()
        }
        _ => Vec::new(),
    };
    apply_actions(ui, actions, client, stream, error_banner);
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
                let request = ChatRequest::new(ui.model.clone(), messages);
                let rx = client.call(request);
                *stream = Some(Stream { id, rx });
            }
            Action::CancelRequest { .. } => {
                *stream = None;
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
        StreamEvent::Comment(_) => Vec::new(),
        StreamEvent::Finish { reason } => vec![Event::Finish { request, reason }],
    }
}

/// Snapshot of everything the blocking task needs to draw the UI.
#[derive(Debug, Clone)]
struct RenderData {
    model: String,
    status: Status,
    active: bool,
    draft: String,
    conversation: Vec<ChatMessage>,
    pending: Option<PendingResponse>,
    error_banner: Option<String>,
}

fn build_render(ui: &UiState, agent: &AgentCore, error_banner: Option<&str>) -> RenderData {
    RenderData {
        model: ui.model.clone(),
        status: agent.status(),
        active: agent.active_request().is_some(),
        draft: ui.draft.clone(),
        conversation: agent.conversation().to_vec(),
        pending: agent.pending_response().cloned(),
        error_banner: error_banner.map(String::from),
    }
}

/// Character width estimator backed by `unicode-width`.
///
/// tuinix's default estimator treats every non-control character as
/// one column, which mis-aligns CJK glyphs and emoji. This estimator
/// consults Unicode Standard Annex #11 via [`UnicodeWidthChar`].
struct UnicodeCharWidth;

impl EstimateCharWidth for UnicodeCharWidth {
    fn estimate_char_width(&self, c: char) -> usize {
        UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

fn terminal_loop(
    event_tx: mpsc::UnboundedSender<TerminalUpdate>,
    mut render_rx: mpsc::UnboundedReceiver<RenderData>,
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
        if let Some(data) = latest {
            let frame = render_frame(size, &data);
            terminal.draw(frame)?;
        }
        if render_closed {
            break;
        }

        match terminal.poll_event(&[], &[], Some(TERMINAL_POLL_INTERVAL))? {
            Some(TerminalEvent::Input(TerminalInput::Key(key))) => {
                if event_tx.send(TerminalUpdate::Key(key)).is_err() {
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

fn render_frame(size: TerminalSize, data: &RenderData) -> TerminalFrame<UnicodeCharWidth> {
    let mut main = TerminalFrame::with_char_width_estimator(size, UnicodeCharWidth);
    let bold = TerminalStyle::new().bold();
    let reset = TerminalStyle::RESET;
    let dim = TerminalStyle::new().fg_color(TerminalColor::BRIGHT_BLACK);
    let user_style = TerminalStyle::new().bold().fg_color(TerminalColor::CYAN);
    let assistant_style = TerminalStyle::new().fg_color(TerminalColor::GREEN);
    let error_style = TerminalStyle::new().bold().fg_color(TerminalColor::RED);
    let reasoning_style = TerminalStyle::new().fg_color(TerminalColor::BRIGHT_BLACK);

    let status_label = match data.status {
        Status::Idle => "idle",
        Status::AwaitingModel => "waiting",
        Status::Streaming => "streaming",
    };
    let hint = if data.active {
        "Esc / Ctrl-C: cancel"
    } else {
        "Enter: send   Ctrl-D: quit"
    };

    write_single_line(
        &mut main,
        TerminalPosition::ZERO,
        size.cols,
        format_args!(
            "{bold}attini{reset}  model={}  status={status_label}",
            data.model
        ),
    );

    if size.rows >= 1 {
        write_single_line(
            &mut main,
            TerminalPosition::row_col(size.rows - 1, 0),
            size.cols,
            format_args!("{bold}>{reset} {}   {dim}[{hint}]{reset}", data.draft),
        );
    }

    let mut reserved_bottom_rows = 1;
    if let Some(err) = data.error_banner.as_deref()
        && size.rows >= 2
    {
        write_single_line(
            &mut main,
            TerminalPosition::row_col(size.rows - 2, 0),
            size.cols,
            format_args!("{error_style}error:{reset} {err}"),
        );
        reserved_bottom_rows = 2;
    }

    let body_top: usize = 1;
    let body_rows = size.rows.saturating_sub(body_top + reserved_bottom_rows);
    if body_rows > 0 {
        let mut body_text = String::new();
        for message in &data.conversation {
            let (label, style) = match message.role {
                Role::User => ("user", user_style),
                Role::Assistant => ("assistant", assistant_style),
                Role::System => ("system", dim),
            };
            let _ = writeln!(body_text, "{style}{label}:{reset} {}", message.content);
        }
        if let Some(pending) = data.pending.as_ref() {
            if !pending.reasoning.is_empty() {
                let _ = writeln!(
                    body_text,
                    "{reasoning_style}[thinking] {}{reset}",
                    pending.reasoning,
                );
            }
            if !pending.content.is_empty() {
                let _ = writeln!(
                    body_text,
                    "{assistant_style}assistant:{reset} {}",
                    pending.content,
                );
            }
        }

        let lines: Vec<&str> = body_text.lines().collect();
        let start = lines.len().saturating_sub(body_rows);
        let body_size = TerminalSize::rows_cols(body_rows, size.cols);
        let mut body = TerminalFrame::with_char_width_estimator(body_size, UnicodeCharWidth);
        for line in &lines[start..] {
            let _ = writeln!(body, "{line}");
        }
        main.draw(TerminalPosition::row_col(body_top, 0), &body);
    }

    main
}

fn write_single_line(
    frame: &mut TerminalFrame<UnicodeCharWidth>,
    position: TerminalPosition,
    cols: usize,
    args: std::fmt::Arguments<'_>,
) {
    let mut line = TerminalFrame::with_char_width_estimator(
        TerminalSize::rows_cols(1, cols),
        UnicodeCharWidth,
    );
    let _ = line.write_fmt(args);
    frame.draw(position, &line);
}
