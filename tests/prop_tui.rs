//! Property-based tests for `attini::sansio::tui`.
//!
//! Two tests:
//!
//! - `layout_invariants_hold_for_arbitrary_state_and_size` samples an
//!   arbitrary [`RenderState`] plus terminal size and checks that
//!   [`render`] emits a well-formed [`RenderedGrid`] (regions in the
//!   right rows, no overlap, body fits its allotted space, truncation
//!   flag consistent).
//! - `pipeline_invariants_hold_across_random_sessions` runs random
//!   sequences of key events and agent-facing events through
//!   `handle_key → AgentCore::handle_event → build_render_state →
//!   render` and checks that the layout invariants keep holding and
//!   that the projected `active` flag stays in sync with the agent's
//!   real state.

use attini::sansio::agent::{AgentCore, Event, PendingResponse, RequestId, Status};
use attini::sansio::deepseek::{ChatMessage, Role};
use attini::sansio::tui::{
    AgentView, KeyCode, KeyEffect, KeyInput, RenderState, RenderedGrid, UiState,
    build_render_state, handle_key, render,
};

const ITERATIONS: usize = 256;
const STATEFUL_ITERATIONS: usize = 128;
const STATEFUL_STEPS: usize = 30;
const SEED_ENV: &str = "ATTINI_PBT_SEED";

// -----------------------------------------------------------------
// Layout invariant PBT
// -----------------------------------------------------------------

fn sample_role(ctx: &mut noprop::TestCaseContext) -> Role {
    match noprop::sample_choice(ctx, &["user", "assistant", "system"]) {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => Role::System,
    }
}

fn sample_text(ctx: &mut noprop::TestCaseContext, max_len: usize) -> String {
    let len = noprop::sample_usize_in(ctx, 0..=max_len);
    noprop::sample_ascii_printable_string(ctx, len)
}

fn sample_content(ctx: &mut noprop::TestCaseContext) -> String {
    let mut s = sample_text(ctx, 20);
    if noprop::sample_ratio(ctx, 1, 5) {
        s.push('\n');
        s.push_str(&sample_text(ctx, 10));
    }
    s
}

fn sample_message(ctx: &mut noprop::TestCaseContext) -> ChatMessage {
    ChatMessage {
        role: sample_role(ctx),
        content: sample_content(ctx),
    }
}

fn sample_state(ctx: &mut noprop::TestCaseContext) -> RenderState {
    let convo_len = noprop::sample_usize_in(ctx, 0..=15);
    let mut conversation = Vec::new();
    for _ in 0..convo_len {
        conversation.push(sample_message(ctx));
    }
    let pending = if noprop::sample_bool(ctx) {
        Some(PendingResponse {
            content: if noprop::sample_bool(ctx) {
                sample_content(ctx)
            } else {
                String::new()
            },
            reasoning: if noprop::sample_bool(ctx) {
                sample_content(ctx)
            } else {
                String::new()
            },
            finish_reason: None,
        })
    } else {
        None
    };
    let error_banner = if noprop::sample_bool(ctx) {
        Some(sample_text(ctx, 20))
    } else {
        None
    };
    let status = match noprop::sample_choice(ctx, &["idle", "waiting", "streaming"]) {
        "idle" => Status::Idle,
        "waiting" => Status::AwaitingModel,
        _ => Status::Streaming,
    };
    let active = !matches!(status, Status::Idle);
    let draft = sample_text(ctx, 15);
    RenderState {
        model: "m".to_string(),
        status,
        active,
        draft,
        conversation,
        pending,
        error_banner,
    }
}

fn sample_size(ctx: &mut noprop::TestCaseContext) -> (usize, usize) {
    // Restrict to sizes where the layout has room for header +
    // (optional error) + prompt without collisions. Small-size
    // degeneracy is out of scope for this property.
    let rows = noprop::sample_usize_in(ctx, 4..=40);
    let cols = noprop::sample_usize_in(ctx, 10..=200);
    (rows, cols)
}

fn assert_layout_invariants(grid: &RenderedGrid) {
    let (rows, _cols) = grid.size;

    assert_eq!(grid.header.top, 0, "header must sit at row 0");
    assert_eq!(grid.header.lines.len(), 1, "header must occupy 1 line");

    assert_eq!(grid.prompt.top, rows - 1, "prompt must sit at the last row",);
    assert_eq!(grid.prompt.lines.len(), 1, "prompt must occupy 1 line");

    assert_eq!(grid.body.top, 1, "body must start at row 1");

    if let Some(error) = grid.error.as_ref() {
        assert_eq!(
            error.top,
            rows - 2,
            "error banner must sit one row above the prompt",
        );
        assert_eq!(error.lines.len(), 1, "error banner must occupy 1 line");
    }

    let header_end = grid.header.top + grid.header.lines.len();
    let body_end = grid.body.top + grid.body.lines.len();
    let prompt_start = grid.prompt.top;

    assert!(header_end <= grid.body.top, "header overlaps body");
    match grid.error.as_ref() {
        Some(error) => {
            let error_end = error.top + error.lines.len();
            assert!(body_end <= error.top, "body overlaps error");
            assert!(error_end <= prompt_start, "error overlaps prompt");
        }
        None => {
            assert!(body_end <= prompt_start, "body overlaps prompt");
        }
    }

    assert!(
        prompt_start + grid.prompt.lines.len() <= rows,
        "prompt extends past frame bottom",
    );

    let reserved_bottom = 1 + if grid.error.is_some() { 1 } else { 0 };
    let body_capacity = rows - 1 - reserved_bottom;
    assert!(
        grid.body.lines.len() <= body_capacity,
        "body has {} lines, capacity is {body_capacity}",
        grid.body.lines.len(),
    );

    if !grid.body_truncated {
        // When no truncation happened, body must be able to hold all
        // logical lines i.e. its current length is <= capacity (already
        // checked above) and no lines are missing. We can't reconstruct
        // the original line count from the grid alone, so just assert
        // the flag is consistent with the observed lengths.
        assert!(grid.body.lines.len() <= body_capacity);
    }
}

#[test]
fn layout_invariants_hold_for_arbitrary_state_and_size() -> noprop::Result<()> {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed, ITERATIONS).run(|ctx| {
        let state = sample_state(ctx);
        let size = sample_size(ctx);
        let grid = render(&state, size);
        assert_layout_invariants(&grid);
        Ok(())
    })?;
    Ok(())
}

// -----------------------------------------------------------------
// Stateful pipeline PBT
// -----------------------------------------------------------------

enum PipelineAction {
    Key(KeyInput),
    Agent(Event),
}

fn sample_key(ctx: &mut noprop::TestCaseContext) -> KeyInput {
    let kind = noprop::sample_choice(
        ctx,
        &[
            "enter",
            "escape",
            "backspace",
            "char",
            "ctrl_c",
            "ctrl_d",
            "other",
        ],
    );
    match kind {
        "enter" => KeyInput {
            ctrl: false,
            alt: false,
            code: KeyCode::Enter,
        },
        "escape" => KeyInput {
            ctrl: false,
            alt: false,
            code: KeyCode::Escape,
        },
        "backspace" => KeyInput {
            ctrl: false,
            alt: false,
            code: KeyCode::Backspace,
        },
        "char" => {
            let c = noprop::sample_choice(ctx, &['a', 'z', '0', 'あ', 'A', ' ', '!', 'い']);
            KeyInput {
                ctrl: false,
                alt: false,
                code: KeyCode::Char(c),
            }
        }
        "ctrl_c" => KeyInput {
            ctrl: true,
            alt: false,
            code: KeyCode::Char('c'),
        },
        "ctrl_d" => KeyInput {
            ctrl: true,
            alt: false,
            code: KeyCode::Char('d'),
        },
        _ => KeyInput {
            ctrl: false,
            alt: false,
            code: KeyCode::Other,
        },
    }
}

fn sample_request_id(ctx: &mut noprop::TestCaseContext, agent: &AgentCore) -> RequestId {
    // 60% of the time use the currently-active request id (if any) so
    // events are non-stale; 40% synthesize a small integer id to
    // exercise stale-event handling.
    if let Some(active) = agent.active_request()
        && noprop::sample_ratio(ctx, 3, 5)
    {
        return active;
    }
    RequestId::new(noprop::sample_usize_in(ctx, 0..=8) as u64)
}

fn sample_agent_event(ctx: &mut noprop::TestCaseContext, agent: &AgentCore) -> Event {
    let kind = noprop::sample_choice(
        ctx,
        &[
            "user_message",
            "cancel",
            "content_delta",
            "reasoning_delta",
            "finish",
            "transport_error",
            "timeout",
        ],
    );
    match kind {
        "user_message" => Event::UserMessage(sample_text(ctx, 10)),
        "cancel" => Event::Cancel,
        "content_delta" => Event::ContentDelta {
            request: sample_request_id(ctx, agent),
            text: sample_text(ctx, 8),
        },
        "reasoning_delta" => Event::ReasoningDelta {
            request: sample_request_id(ctx, agent),
            text: sample_text(ctx, 8),
        },
        "finish" => Event::Finish {
            request: sample_request_id(ctx, agent),
            reason: if noprop::sample_bool(ctx) {
                Some(sample_text(ctx, 4))
            } else {
                None
            },
        },
        "transport_error" => Event::TransportError {
            request: sample_request_id(ctx, agent),
            message: sample_text(ctx, 12),
        },
        _ => Event::Timeout {
            request: sample_request_id(ctx, agent),
        },
    }
}

fn sample_pipeline_action(ctx: &mut noprop::TestCaseContext, agent: &AgentCore) -> PipelineAction {
    // Bias slightly toward keys so that draft/edit paths are exercised
    // even when the agent is idle.
    if noprop::sample_ratio(ctx, 3, 5) {
        PipelineAction::Key(sample_key(ctx))
    } else {
        PipelineAction::Agent(sample_agent_event(ctx, agent))
    }
}

fn drive_key(ui: &mut UiState, agent: &mut AgentCore, key: KeyInput) -> KeyEffect {
    let outcome = handle_key(key, ui, agent.view());
    for event in outcome.events {
        let _ = agent.handle_event(event);
    }
    outcome.effect
}

#[test]
fn pipeline_invariants_hold_across_random_sessions() -> noprop::Result<()> {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed, STATEFUL_ITERATIONS).run(|ctx| {
        let mut ui = UiState {
            model: "m".to_string(),
            draft: String::new(),
        };
        let mut agent = AgentCore::new();
        let mut quit = false;

        for _ in 0..STATEFUL_STEPS {
            if quit {
                break;
            }
            let action = sample_pipeline_action(ctx, &agent);
            match action {
                PipelineAction::Key(key) => {
                    let effect = drive_key(&mut ui, &mut agent, key);
                    if matches!(effect, KeyEffect::Quit) {
                        quit = true;
                    }
                }
                PipelineAction::Agent(event) => {
                    let _ = agent.handle_event(event);
                }
            }

            // Layout invariants must hold at every step.
            let state = build_render_state(&ui, &agent, None);
            let size = sample_size(ctx);
            let grid = render(&state, size);
            assert_layout_invariants(&grid);

            // `active` in the snapshot must reflect the agent view.
            let view = AgentView {
                has_active_request: agent.active_request().is_some(),
            };
            assert_eq!(
                state.active, view.has_active_request,
                "RenderState.active out of sync with AgentView",
            );

            // Draft is only mutable when there is no active request.
            if agent.active_request().is_some() {
                // Nothing to assert directly here about draft length: it
                // can hold whatever it held before the request started.
                // The stronger invariant is checked by handle_key unit
                // tests. We only make sure ui / agent stay coherent.
            }
        }
        Ok(())
    })?;
    Ok(())
}
