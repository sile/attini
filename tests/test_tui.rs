//! Unit tests for `attini::sansio::tui`.
//!
//! Covers layout, style, overflow, key bindings, and CJK-width
//! passthrough. The tests build [`RenderState`] snapshots directly
//! (bypassing `AgentCore`) so each case is minimal and focused.

use attini::sansio::agent::{AgentCore, Event, PendingResponse, Status};
use attini::sansio::deepseek::ChatMessage;
use attini::sansio::tui::{
    AgentView, Color, KeyCode, KeyEffect, KeyInput, RenderState, StyledLine, StyledSpan, UiState,
    build_render_state, handle_key, render,
};

fn empty_state(model: &str) -> RenderState {
    RenderState {
        model: model.to_string(),
        status: Status::Idle,
        active: false,
        awaiting_approval: false,
        draft: String::new(),
        conversation: Vec::new(),
        pending: None,
        active_tool_calls: Vec::new(),
        error_banner: None,
    }
}

fn user_msg(content: &str) -> ChatMessage {
    ChatMessage::User(content.to_string())
}

fn assistant_msg(content: &str) -> ChatMessage {
    ChatMessage::assistant_text(content)
}

fn find_span(line: &StyledLine, needle: &str) -> Option<StyledSpan> {
    line.spans.iter().find(|s| s.text.contains(needle)).cloned()
}

// -----------------------------------------------------------------
// layout
// -----------------------------------------------------------------

#[test]
fn header_is_placed_at_row_zero() {
    let grid = render(&empty_state("m"), (10, 40));
    assert_eq!(grid.header.top, 0);
    assert_eq!(grid.header.lines.len(), 1);
}

#[test]
fn prompt_is_placed_at_last_row() {
    let grid = render(&empty_state("m"), (10, 40));
    assert_eq!(grid.prompt.top, 9);
    assert_eq!(grid.prompt.lines.len(), 1);
}

#[test]
fn body_sits_between_header_and_prompt_when_no_error() {
    let grid = render(&empty_state("m"), (10, 40));
    assert_eq!(grid.body.top, 1);
    assert!(grid.error.is_none());
}

#[test]
fn error_banner_appears_above_prompt_when_present() {
    let mut state = empty_state("m");
    state.error_banner = Some("boom".to_string());
    let grid = render(&state, (10, 40));
    let error = grid.error.expect("error region");
    assert_eq!(error.top, 8);
    assert_eq!(grid.prompt.top, 9);
    // Body must shrink by one row to make room for the error banner.
    // header(1) + body(N) + error(1) + prompt(1) = 10 => body_max = 7.
    // With no content, body has zero lines.
    assert_eq!(grid.body.top, 1);
}

// -----------------------------------------------------------------
// style
// -----------------------------------------------------------------

#[test]
fn header_label_span_is_bold() {
    let grid = render(&empty_state("m"), (10, 40));
    let attini_span = find_span(&grid.header.lines[0], "attini").expect("attini span");
    assert!(attini_span.style.bold);
}

#[test]
fn user_message_label_span_is_bold_and_cyan() {
    let mut state = empty_state("m");
    state.conversation.push(user_msg("hello"));
    let grid = render(&state, (10, 40));
    let label = find_span(&grid.body.lines[0], "user:").expect("user label");
    assert!(label.style.bold);
    assert_eq!(label.style.fg, Some(Color::Cyan));
}

#[test]
fn assistant_message_label_span_is_green() {
    let mut state = empty_state("m");
    state.conversation.push(assistant_msg("hi"));
    let grid = render(&state, (10, 40));
    let label = find_span(&grid.body.lines[0], "assistant:").expect("assistant label");
    assert_eq!(label.style.fg, Some(Color::Green));
    assert!(!label.style.bold);
}

#[test]
fn error_banner_label_span_is_bold_and_red() {
    let mut state = empty_state("m");
    state.error_banner = Some("boom".to_string());
    let grid = render(&state, (10, 40));
    let error = grid.error.expect("error region");
    let label = find_span(&error.lines[0], "error:").expect("error label");
    assert!(label.style.bold);
    assert_eq!(label.style.fg, Some(Color::Red));
}

#[test]
fn reasoning_span_uses_dim_bright_black() {
    let mut state = empty_state("m");
    state.pending = Some(PendingResponse {
        content: String::new(),
        reasoning: "thinking...".to_string(),
        finish_reason: None,
    });
    let grid = render(&state, (10, 40));
    let span = find_span(&grid.body.lines[0], "[thinking]").expect("reasoning marker");
    assert_eq!(span.style.fg, Some(Color::BrightBlack));
}

#[test]
fn prompt_hint_span_uses_dim_style() {
    let grid = render(&empty_state("m"), (10, 40));
    let hint = find_span(&grid.prompt.lines[0], "[Enter:").expect("prompt hint");
    assert!(hint.style.dim);
    assert_eq!(hint.style.fg, Some(Color::BrightBlack));
}

// -----------------------------------------------------------------
// overflow
// -----------------------------------------------------------------

#[test]
fn body_is_not_truncated_when_content_fits() {
    let mut state = empty_state("m");
    state.conversation.push(user_msg("a"));
    state.conversation.push(assistant_msg("b"));
    let grid = render(&state, (20, 40));
    assert!(!grid.body_truncated);
    assert_eq!(grid.body.lines.len(), 2);
}

#[test]
fn body_truncates_oldest_lines_when_overflowing() {
    let mut state = empty_state("m");
    for i in 0..30 {
        state.conversation.push(user_msg(&format!("msg {i}")));
    }
    let grid = render(&state, (10, 40));
    assert!(grid.body_truncated);
    // header(1) + body + prompt(1) = 10 => body_max = 8
    assert_eq!(grid.body.lines.len(), 8);
    // Newest message ("msg 29") must still be visible.
    let has_newest = grid
        .body
        .lines
        .iter()
        .any(|line| line.spans.iter().any(|s| s.text.contains("msg 29")));
    assert!(has_newest, "newest message should survive truncation");
    // Prompt must still occupy the last row.
    assert_eq!(grid.prompt.top, 9);
}

#[test]
fn multiline_message_content_produces_multiple_body_lines() {
    let mut state = empty_state("m");
    state
        .conversation
        .push(assistant_msg("line1\nline2\nline3"));
    let grid = render(&state, (20, 40));
    assert_eq!(grid.body.lines.len(), 3);
    // The label only appears on the first line.
    assert!(find_span(&grid.body.lines[0], "assistant:").is_some());
    assert!(find_span(&grid.body.lines[1], "assistant:").is_none());
    assert!(find_span(&grid.body.lines[2], "assistant:").is_none());
}

// -----------------------------------------------------------------
// input binding
// -----------------------------------------------------------------

fn key(code: KeyCode) -> KeyInput {
    KeyInput {
        ctrl: false,
        alt: false,
        code,
    }
}

fn ctrl(code: KeyCode) -> KeyInput {
    KeyInput {
        ctrl: true,
        alt: false,
        code,
    }
}

fn idle_view() -> AgentView {
    AgentView {
        has_active_request: false,
        pending_approval_call_id: None,
    }
}

fn active_view() -> AgentView {
    AgentView {
        has_active_request: true,
        pending_approval_call_id: None,
    }
}

#[test]
fn char_key_appends_to_draft_when_idle() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(key(KeyCode::Char('a')), &mut ui, idle_view());
    assert_eq!(ui.draft, "a");
    assert!(outcome.events.is_empty());
    assert_eq!(outcome.effect, KeyEffect::None);
}

#[test]
fn char_key_is_ignored_during_active_request() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: "prev".to_string(),
    };
    let _ = handle_key(key(KeyCode::Char('x')), &mut ui, active_view());
    assert_eq!(ui.draft, "prev");
}

#[test]
fn backspace_pops_last_char_when_idle() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: "hello".to_string(),
    };
    let _ = handle_key(key(KeyCode::Backspace), &mut ui, idle_view());
    assert_eq!(ui.draft, "hell");
}

#[test]
fn enter_emits_user_message_and_clears_draft() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: "hi".to_string(),
    };
    let outcome = handle_key(key(KeyCode::Enter), &mut ui, idle_view());
    assert_eq!(outcome.events, vec![Event::UserMessage("hi".to_string())]);
    assert_eq!(ui.draft, "");
    assert_eq!(outcome.effect, KeyEffect::None);
}

#[test]
fn enter_on_empty_draft_is_a_no_op() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(key(KeyCode::Enter), &mut ui, idle_view());
    assert!(outcome.events.is_empty());
    assert_eq!(outcome.effect, KeyEffect::None);
}

#[test]
fn enter_is_ignored_during_active_request() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: "hi".to_string(),
    };
    let outcome = handle_key(key(KeyCode::Enter), &mut ui, active_view());
    assert!(outcome.events.is_empty());
    assert_eq!(ui.draft, "hi");
}

#[test]
fn ctrl_c_cancels_when_active() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(ctrl(KeyCode::Char('c')), &mut ui, active_view());
    assert_eq!(outcome.events, vec![Event::Cancel]);
    assert_eq!(outcome.effect, KeyEffect::None);
}

#[test]
fn ctrl_c_quits_when_idle() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(ctrl(KeyCode::Char('c')), &mut ui, idle_view());
    assert!(outcome.events.is_empty());
    assert_eq!(outcome.effect, KeyEffect::Quit);
}

#[test]
fn ctrl_d_always_quits() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: "buf".to_string(),
    };
    let outcome = handle_key(ctrl(KeyCode::Char('d')), &mut ui, idle_view());
    assert_eq!(outcome.effect, KeyEffect::Quit);
    let outcome_active = handle_key(ctrl(KeyCode::Char('d')), &mut ui, active_view());
    assert_eq!(outcome_active.effect, KeyEffect::Quit);
}

#[test]
fn escape_cancels_when_active_and_is_noop_when_idle() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let active = handle_key(key(KeyCode::Escape), &mut ui, active_view());
    assert_eq!(active.events, vec![Event::Cancel]);
    let idle = handle_key(key(KeyCode::Escape), &mut ui, idle_view());
    assert!(idle.events.is_empty());
    assert_eq!(idle.effect, KeyEffect::None);
}

#[test]
fn other_key_is_ignored() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: "buf".to_string(),
    };
    let outcome = handle_key(key(KeyCode::Other), &mut ui, idle_view());
    assert!(outcome.events.is_empty());
    assert_eq!(outcome.effect, KeyEffect::None);
    assert_eq!(ui.draft, "buf");
}

// -----------------------------------------------------------------
// CJK / unicode passthrough
// -----------------------------------------------------------------

#[test]
fn cjk_draft_survives_handle_key_and_render() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    for c in "あいうえお".chars() {
        let _ = handle_key(key(KeyCode::Char(c)), &mut ui, idle_view());
    }
    assert_eq!(ui.draft, "あいうえお");
    let state = RenderState {
        model: ui.model.clone(),
        status: Status::Idle,
        active: false,
        awaiting_approval: false,
        draft: ui.draft.clone(),
        conversation: Vec::new(),
        pending: None,
        active_tool_calls: Vec::new(),
        error_banner: None,
    };
    let grid = render(&state, (10, 40));
    let joined: String = grid.prompt.lines[0]
        .spans
        .iter()
        .map(|s| s.text.as_str())
        .collect();
    assert!(joined.contains("あいうえお"), "prompt line: {joined:?}");
}

#[test]
fn cjk_message_content_flows_into_body_span() {
    let mut state = empty_state("m");
    state.conversation.push(user_msg("こんにちは"));
    let grid = render(&state, (10, 40));
    let span = grid.body.lines[0]
        .spans
        .iter()
        .find(|s| s.text.contains("こんにちは"))
        .expect("content span");
    // Character width follows Unicode Standard Annex #11 (each CJK
    // codepoint = 2 columns), so 5 CJK chars would occupy 10 columns
    // when drawn by tuinix's UnicodeCharWidth estimator. Sans I/O
    // side only stores the string as-is; width validation is left to
    // tuinix's frame layer.
    assert_eq!(span.text.chars().count(), 5);
}

// -----------------------------------------------------------------
// build_render_state
// -----------------------------------------------------------------

#[test]
fn build_render_state_captures_agent_and_ui_state() {
    let mut agent = AgentCore::new();
    let _ = agent.handle_event(Event::UserMessage("hi".to_string()));
    let ui = UiState {
        model: "deepseek-v4-flash".to_string(),
        draft: "draft".to_string(),
    };
    let state = build_render_state(&ui, &agent, Some("recent-error"));
    assert_eq!(state.model, "deepseek-v4-flash");
    assert_eq!(state.draft, "draft");
    assert!(state.active);
    assert_eq!(state.status, Status::AwaitingModel);
    assert_eq!(state.conversation.len(), 1);
    assert_eq!(state.error_banner.as_deref(), Some("recent-error"));
}

#[test]
fn agent_view_reflects_active_request() {
    let mut agent = AgentCore::new();
    assert!(!agent.view().has_active_request);
    let _ = agent.handle_event(Event::UserMessage("hi".to_string()));
    assert!(agent.view().has_active_request);
}

// -----------------------------------------------------------------
// active_tool_calls rendering
// -----------------------------------------------------------------

#[test]
fn active_tool_call_appears_in_body_with_yellow_label_and_state_word() {
    let mut state = empty_state("m");
    state
        .active_tool_calls
        .push(attini::sansio::agent::ActiveToolCall {
            call_id: "call_1".to_string(),
            function_name: "list".to_string(),
            arguments_json: r#"{"path":"src"}"#.to_string(),
            outcome: None,
            is_streaming: false,
            approval: attini::sansio::agent::ApprovalState::NotRequired,
            patch_preview: None,
            preview_hashes: Vec::new(),
        });
    state.status = Status::ToolRunning;
    state.active = true;
    let grid = render(&state, (10, 60));
    let joined: String = grid
        .body
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.text.as_str())
        .collect();
    assert!(joined.contains("[tool: list"), "body: {joined}");
    assert!(joined.contains("running"), "body: {joined}");

    let label_span = grid.body.lines[0]
        .spans
        .iter()
        .find(|s| s.text.starts_with("[tool:"))
        .expect("label span");
    assert_eq!(label_span.style.fg, Some(Color::Yellow));
    assert!(label_span.style.bold);
}

#[test]
fn completed_ok_tool_call_shows_done_and_summary() {
    let mut state = empty_state("m");
    state
        .active_tool_calls
        .push(attini::sansio::agent::ActiveToolCall {
            call_id: "c".to_string(),
            function_name: "read".to_string(),
            arguments_json: r#"{"path":"a"}"#.to_string(),
            outcome: Some(attini::sansio::agent::ToolOutcome::Ok(
                r#"{"content":"hi","truncated":false}"#.to_string(),
            )),
            is_streaming: false,
            approval: attini::sansio::agent::ApprovalState::NotRequired,
            patch_preview: None,
            preview_hashes: Vec::new(),
        });
    let grid = render(&state, (10, 60));
    let joined: String = grid
        .body
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.text.as_str())
        .collect();
    assert!(joined.contains("done"), "body: {joined}");
    assert!(joined.contains("content"), "body: {joined}");
}

#[test]
fn errored_tool_call_shows_error_state_in_red() {
    let mut state = empty_state("m");
    state
        .active_tool_calls
        .push(attini::sansio::agent::ActiveToolCall {
            call_id: "c".to_string(),
            function_name: "read".to_string(),
            arguments_json: r#"{"path":"a"}"#.to_string(),
            outcome: Some(attini::sansio::agent::ToolOutcome::Err(
                attini::sansio::agent::ToolExecutionError::OutsideWorkspace,
            )),
            is_streaming: false,
            approval: attini::sansio::agent::ApprovalState::NotRequired,
            patch_preview: None,
            preview_hashes: Vec::new(),
        });
    let grid = render(&state, (10, 60));
    let error_span = grid.body.lines[0]
        .spans
        .iter()
        .find(|s| s.text.contains("error"))
        .expect("error span");
    assert_eq!(error_span.style.fg, Some(Color::Red));
}

// -----------------------------------------------------------------
// patch approval rendering + key handling
// -----------------------------------------------------------------

fn approval_view(call_id: &str) -> AgentView {
    AgentView {
        has_active_request: true,
        pending_approval_call_id: Some(call_id.to_string()),
    }
}

#[test]
fn approval_mode_prompt_hint_switches_to_y_n() {
    let mut state = empty_state("m");
    state.awaiting_approval = true;
    state.active = true;
    state.status = Status::AwaitingApproval;
    let grid = render(&state, (10, 60));
    let hint = grid.prompt.lines[0]
        .spans
        .iter()
        .find(|s| s.text.starts_with("[Y:"))
        .expect("approval hint");
    assert!(hint.text.contains("approve"));
    assert!(hint.text.contains("reject"));
}

#[test]
fn approval_pending_patch_shows_awaiting_label_with_diff_stats() {
    let mut state = empty_state("m");
    state.awaiting_approval = true;
    state.active = true;
    state.status = Status::AwaitingApproval;
    state
        .active_tool_calls
        .push(attini::sansio::agent::ActiveToolCall {
            call_id: "p1".to_string(),
            function_name: "patch".to_string(),
            arguments_json: String::new(),
            outcome: None,
            is_streaming: false,
            approval: attini::sansio::agent::ApprovalState::Pending,
            patch_preview: Some(attini::sansio::agent::PatchPreview {
                target_paths: vec!["src/main.rs".to_string()],
                added_lines: 3,
                removed_lines: 1,
                edit_count: 1,
            }),
            preview_hashes: Vec::new(),
        });
    let grid = render(&state, (10, 80));
    let body: String = grid
        .body
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.text.as_str())
        .collect();
    assert!(body.contains("[patch approval]"), "body: {body}");
    assert!(body.contains("src/main.rs"), "body: {body}");
    assert!(body.contains("(+3/-1)"), "body: {body}");
    assert!(body.contains("awaiting"), "body: {body}");
}

#[test]
fn approval_mode_y_key_emits_approve_patch_event() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(key(KeyCode::Char('y')), &mut ui, approval_view("p1"));
    assert_eq!(outcome.effect, KeyEffect::None);
    assert_eq!(outcome.events.len(), 1);
    match &outcome.events[0] {
        attini::sansio::agent::Event::ApprovePatch { call_id } => assert_eq!(call_id, "p1"),
        other => panic!("expected ApprovePatch, got {other:?}"),
    }
}

#[test]
fn approval_mode_n_key_emits_reject_patch_event() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(key(KeyCode::Char('n')), &mut ui, approval_view("p1"));
    match &outcome.events[..] {
        [attini::sansio::agent::Event::RejectPatch { call_id }] => assert_eq!(call_id, "p1"),
        other => panic!("expected RejectPatch, got {other:?}"),
    }
}

#[test]
fn approval_mode_esc_key_also_rejects_patch() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(key(KeyCode::Escape), &mut ui, approval_view("p1"));
    match &outcome.events[..] {
        [attini::sansio::agent::Event::RejectPatch { call_id }] => assert_eq!(call_id, "p1"),
        other => panic!("expected RejectPatch from Esc, got {other:?}"),
    }
}

#[test]
fn approval_mode_ignores_draft_edit_keys() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: "existing".to_string(),
    };
    let _ = handle_key(key(KeyCode::Char('a')), &mut ui, approval_view("p1"));
    let _ = handle_key(key(KeyCode::Backspace), &mut ui, approval_view("p1"));
    let outcome = handle_key(key(KeyCode::Enter), &mut ui, approval_view("p1"));
    assert_eq!(ui.draft, "existing", "approval mode must not mutate draft");
    assert!(outcome.events.is_empty());
}

#[test]
fn approval_mode_ctrl_c_still_emits_cancel() {
    let mut ui = UiState {
        model: "m".to_string(),
        draft: String::new(),
    };
    let outcome = handle_key(ctrl(KeyCode::Char('c')), &mut ui, approval_view("p1"));
    match &outcome.events[..] {
        [attini::sansio::agent::Event::Cancel] => {}
        other => panic!("expected Cancel, got {other:?}"),
    }
}
