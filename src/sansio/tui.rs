//! Sans I/O rendering and input decoding for the TUI.
//!
//! The types and functions in this module do not touch the terminal,
//! the network, or any other I/O. The surrounding shell (`src/tui.rs`)
//! is responsible for:
//!
//! - Reading raw input from the terminal
//! - Translating [`tuinix::KeyInput`] to the mirror [`KeyInput`]
//!   defined here
//! - Feeding key events into [`handle_key`] and any resulting agent
//!   [`Event`]s into [`AgentCore::handle_event`]
//! - Building a [`RenderState`] snapshot with [`build_render_state`]
//!   and turning the resulting [`RenderedGrid`] into a
//!   [`tuinix::TerminalFrame`] just before drawing
//!
//! Keeping every rendering and key-decoding decision in this module
//! lets `tests/test_tui.rs` cover layout, styling, overflow, key
//! bindings, and CJK-width behaviour without a real terminal.

use crate::sansio::agent::{AgentCore, Event, PendingResponse, Status};
use crate::sansio::deepseek::{ChatMessage, Role};

/// Runtime UI state owned by the shell.
///
/// The shell holds a [`UiState`] alongside the [`AgentCore`]. Both
/// [`handle_key`] and [`build_render_state`] read (and, for
/// [`handle_key`], mutate) this state as pure functions.
#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub model: String,
    pub draft: String,
}

/// Read-only projection of [`AgentCore`] required by [`handle_key`].
///
/// Passing the whole `AgentCore` would give the pure function too much
/// authority; this projection contains only the flags [`handle_key`]
/// actually branches on. Constructed via [`AgentCore::view`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentView {
    pub has_active_request: bool,
}

/// Snapshot of everything [`render`] needs to draw the UI.
///
/// The shell rebuilds this from `UiState` + `AgentCore` + the
/// error-banner slot on every render cycle. It is intentionally
/// self-contained so `sansio::tui` never has to reach back into
/// mutable state.
#[derive(Debug, Clone, Default)]
pub struct RenderState {
    pub model: String,
    pub status: Status,
    pub active: bool,
    pub draft: String,
    pub conversation: Vec<ChatMessage>,
    pub pending: Option<PendingResponse>,
    pub error_banner: Option<String>,
}

/// Outcome of [`handle_key`].
///
/// `events` are handed to [`AgentCore::handle_event`] one by one.
/// `effect` carries shell-level directives that have no matching
/// [`Event`] variant (currently just `Quit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOutcome {
    pub events: Vec<Event>,
    pub effect: KeyEffect,
}

/// Side-effect the shell should perform in addition to consuming
/// [`KeyOutcome::events`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyEffect {
    #[default]
    None,
    Quit,
}

/// Mirror of [`tuinix::KeyInput`] that does not pull the tuinix
/// terminal machinery into `sansio`.
///
/// The shell converts a tuinix `KeyInput` into this type before
/// calling [`handle_key`]. Any tuinix key variant not represented
/// here maps to [`KeyCode::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub ctrl: bool,
    pub alt: bool,
    pub code: KeyCode,
}

/// Key variants recognised by [`handle_key`]. `Other` catches every
/// tuinix key the current TUI ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Enter,
    Escape,
    Backspace,
    Char(char),
    Other,
}

/// Structured rendering output. The shell converts this into a
/// [`tuinix::TerminalFrame`] just before calling `terminal.draw`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedGrid {
    pub size: (usize, usize),
    pub header: Region,
    pub body: Region,
    pub error: Option<Region>,
    pub prompt: Region,
    pub cursor: Option<(usize, usize)>,
    pub body_truncated: bool,
}

/// A vertical block of styled lines placed at `top` in the parent
/// frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub top: usize,
    pub lines: Vec<StyledLine>,
}

/// One line's worth of styled content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledLine {
    pub spans: Vec<StyledSpan>,
}

/// A run of characters that share a [`Style`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    pub style: Style,
}

/// Style attributes applied to a [`StyledSpan`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Style {
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
        }
    }

    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }
}

/// Colours currently used by the TUI. Extend as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Cyan,
    Green,
    Red,
    BrightBlack,
}

/// Build a [`RenderState`] snapshot from the live shell state.
pub fn build_render_state(
    ui: &UiState,
    agent: &AgentCore,
    error_banner: Option<&str>,
) -> RenderState {
    RenderState {
        model: ui.model.clone(),
        status: agent.status(),
        active: agent.active_request().is_some(),
        draft: ui.draft.clone(),
        conversation: agent.conversation().to_vec(),
        pending: agent.pending_response().cloned(),
        error_banner: error_banner.map(String::from),
    }
}

/// Consume one key event and return the resulting [`KeyOutcome`].
///
/// The function mutates `ui.draft` for edit keys and reads
/// `view.has_active_request` to gate destructive actions during a live
/// request. It never touches [`AgentCore`] directly and never
/// performs I/O.
pub fn handle_key(key: KeyInput, ui: &mut UiState, view: AgentView) -> KeyOutcome {
    let mut events = Vec::new();
    let mut effect = KeyEffect::None;
    match (key.ctrl, key.code) {
        (true, KeyCode::Char('c')) => {
            if view.has_active_request {
                events.push(Event::Cancel);
            } else {
                effect = KeyEffect::Quit;
            }
        }
        (true, KeyCode::Char('d')) => {
            effect = KeyEffect::Quit;
        }
        (false, KeyCode::Escape) => {
            if view.has_active_request {
                events.push(Event::Cancel);
            }
        }
        (false, KeyCode::Enter) => {
            if !ui.draft.is_empty() && !view.has_active_request {
                let text = std::mem::take(&mut ui.draft);
                events.push(Event::UserMessage(text));
            }
        }
        (false, KeyCode::Backspace) => {
            if !view.has_active_request {
                ui.draft.pop();
            }
        }
        (_, KeyCode::Char(c)) if !key.ctrl && !key.alt && !view.has_active_request => {
            ui.draft.push(c);
        }
        _ => {}
    }
    KeyOutcome { events, effect }
}

const HEADER_TOP: usize = 0;
const HEADER_ROWS: usize = 1;
const PROMPT_ROWS: usize = 1;

/// Produce a [`RenderedGrid`] laid out to `size = (rows, cols)`.
///
/// Layout:
///
/// - Row 0: header (`attini  model=...  status=...`)
/// - Rows 1..(rows - 1 - error_row_count): body (conversation +
///   pending). Body content that does not fit is truncated from the
///   top, keeping the newest lines visible; `body_truncated` is set
///   when this happens.
/// - Row (rows - 2): error banner if `error_banner` is `Some` and
///   `rows >= 2`
/// - Row (rows - 1): prompt (`> {draft}   [hint]`)
pub fn render(state: &RenderState, size: (usize, usize)) -> RenderedGrid {
    let (rows, cols) = size;

    let header_line = build_header_line(state);
    let header = Region {
        top: HEADER_TOP,
        lines: vec![header_line],
    };

    let prompt_line = build_prompt_line(state);
    let prompt_top = rows.saturating_sub(PROMPT_ROWS);
    let prompt = Region {
        top: prompt_top,
        lines: vec![prompt_line],
    };

    // Error banner sits at `rows - 2`, one row above the prompt. Skip
    // it entirely when `rows < 3` to avoid colliding with the header
    // at row 0.
    let (error, error_rows) = if let Some(message) = state.error_banner.as_deref() {
        if rows >= 3 {
            let region = Region {
                top: rows - 2,
                lines: vec![build_error_line(message)],
            };
            (Some(region), 1usize)
        } else {
            (None, 0)
        }
    } else {
        (None, 0)
    };

    let reserved_bottom = PROMPT_ROWS + error_rows;
    let body_top = HEADER_TOP + HEADER_ROWS;
    let body_rows = rows.saturating_sub(body_top + reserved_bottom);

    let mut body_lines = build_body_lines(state);
    let body_truncated = body_lines.len() > body_rows;
    if body_truncated {
        let excess = body_lines.len() - body_rows;
        body_lines.drain(..excess);
    }
    let body = Region {
        top: body_top,
        lines: body_lines,
    };

    let _ = cols;
    RenderedGrid {
        size: (rows, cols),
        header,
        body,
        error,
        prompt,
        cursor: None,
        body_truncated,
    }
}

fn header_style_bold() -> Style {
    Style::new().bold()
}

fn dim_style() -> Style {
    Style::new().fg(Color::BrightBlack).dim()
}

fn user_style() -> Style {
    Style::new().bold().fg(Color::Cyan)
}

fn assistant_style() -> Style {
    Style::new().fg(Color::Green)
}

fn error_style() -> Style {
    Style::new().bold().fg(Color::Red)
}

fn reasoning_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

fn build_header_line(state: &RenderState) -> StyledLine {
    let status_label = match state.status {
        Status::Idle => "idle",
        Status::AwaitingModel => "waiting",
        Status::Streaming => "streaming",
    };
    StyledLine {
        spans: vec![
            StyledSpan {
                text: "attini".to_string(),
                style: header_style_bold(),
            },
            StyledSpan {
                text: format!("  model={}  status={}", state.model, status_label,),
                style: Style::default(),
            },
        ],
    }
}

fn build_prompt_line(state: &RenderState) -> StyledLine {
    let hint = if state.active {
        "Esc / Ctrl-C: cancel"
    } else {
        "Enter: send   Ctrl-D: quit"
    };
    StyledLine {
        spans: vec![
            StyledSpan {
                text: ">".to_string(),
                style: header_style_bold(),
            },
            StyledSpan {
                text: format!(" {}   ", state.draft),
                style: Style::default(),
            },
            StyledSpan {
                text: format!("[{hint}]"),
                style: dim_style(),
            },
        ],
    }
}

fn build_error_line(message: &str) -> StyledLine {
    StyledLine {
        spans: vec![
            StyledSpan {
                text: "error:".to_string(),
                style: error_style(),
            },
            StyledSpan {
                text: format!(" {message}"),
                style: Style::default(),
            },
        ],
    }
}

fn build_body_lines(state: &RenderState) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    for message in &state.conversation {
        push_labeled_message(&mut lines, message);
    }
    if let Some(pending) = state.pending.as_ref() {
        if !pending.reasoning.is_empty() {
            push_reasoning(&mut lines, &pending.reasoning);
        }
        if !pending.content.is_empty() {
            push_pending_assistant(&mut lines, &pending.content);
        }
    }
    lines
}

fn push_labeled_message(lines: &mut Vec<StyledLine>, message: &ChatMessage) {
    let (label, style) = match message.role {
        Role::User => ("user", user_style()),
        Role::Assistant => ("assistant", assistant_style()),
        Role::System => ("system", dim_style()),
    };
    push_labeled_multiline(lines, label, style, &message.content);
}

fn push_pending_assistant(lines: &mut Vec<StyledLine>, content: &str) {
    push_labeled_multiline(lines, "assistant", assistant_style(), content);
}

fn push_reasoning(lines: &mut Vec<StyledLine>, reasoning: &str) {
    let style = reasoning_style();
    for (idx, part) in reasoning.split('\n').enumerate() {
        let spans = if idx == 0 {
            vec![
                StyledSpan {
                    text: "[thinking] ".to_string(),
                    style,
                },
                StyledSpan {
                    text: part.to_string(),
                    style,
                },
            ]
        } else {
            vec![StyledSpan {
                text: part.to_string(),
                style,
            }]
        };
        lines.push(StyledLine { spans });
    }
}

fn push_labeled_multiline(
    lines: &mut Vec<StyledLine>,
    label: &str,
    label_style: Style,
    content: &str,
) {
    let mut parts = content.split('\n');
    let first = parts.next().unwrap_or("");
    lines.push(StyledLine {
        spans: vec![
            StyledSpan {
                text: format!("{label}: "),
                style: label_style,
            },
            StyledSpan {
                text: first.to_string(),
                style: Style::default(),
            },
        ],
    });
    for part in parts {
        lines.push(StyledLine {
            spans: vec![StyledSpan {
                text: part.to_string(),
                style: Style::default(),
            }],
        });
    }
}
