//! Property-based tests for `attini::sansio::agent::AgentCore`.
//!
//! Generates arbitrary event sequences (mixing real active-request IDs
//! with stale / synthetic ones) and checks that the core's public
//! contract holds after every step.

use attini::sansio::agent::{
    Action, AgentCore, Event, RequestId, Status, ToolExecutionError, ToolOutcome,
};
use attini::sansio::deepseek::ChatMessage;

const ITERATIONS: usize = 256;
const SEED_ENV: &str = "ATTINI_PBT_SEED";
const STEPS: usize = 60;

fn sample_string(ctx: &mut noprop::TestCaseContext) -> String {
    let len = noprop::sample_usize_in(ctx, 0..=8);
    noprop::sample_ascii_printable_string(ctx, len)
}

fn sample_request_id(ctx: &mut noprop::TestCaseContext, core: &AgentCore) -> RequestId {
    // Bias toward the current active request so that non-stale events are
    // exercised, but keep a healthy stream of synthetic IDs to verify
    // stale-event handling.
    if let Some(active) = core.active_request()
        && noprop::sample_ratio(ctx, 3, 5)
    {
        return active;
    }
    RequestId::new(noprop::sample_usize_in(ctx, 0..=8) as u64)
}

fn sample_call_id(ctx: &mut noprop::TestCaseContext, core: &AgentCore) -> String {
    // Prefer a call_id from the currently-tracked tool call so that
    // tool_results are occasionally committed (not always dropped as
    // stale). Otherwise emit a synthetic id.
    let active = core.active_tool_calls();
    if !active.is_empty() && noprop::sample_ratio(ctx, 3, 5) {
        let idx = noprop::sample_usize_in(ctx, 0..=(active.len() - 1));
        return active[idx].call_id.clone();
    }
    format!("synth_{}", noprop::sample_usize_in(ctx, 0..=8))
}

fn sample_finish_reason(ctx: &mut noprop::TestCaseContext) -> Option<String> {
    // Enrich the reason distribution so the tool-loop branch on
    // "tool_calls" is exercised at meaningful frequency.
    let kind = noprop::sample_choice(ctx, &["tool_calls", "stop", "length", "custom", "none"]);
    match kind {
        "tool_calls" => Some("tool_calls".to_string()),
        "stop" => Some("stop".to_string()),
        "length" => Some("length".to_string()),
        "custom" => Some(sample_string(ctx)),
        _ => None,
    }
}

fn sample_tool_outcome(ctx: &mut noprop::TestCaseContext) -> ToolOutcome {
    if noprop::sample_bool(ctx) {
        ToolOutcome::Ok(sample_string(ctx))
    } else {
        ToolOutcome::Err(
            match noprop::sample_choice(
                ctx,
                &[
                    "outside_workspace",
                    "not_utf8",
                    "binary",
                    "io_error",
                    "parse_failed",
                    "unknown_tool",
                ],
            ) {
                "outside_workspace" => ToolExecutionError::OutsideWorkspace,
                "not_utf8" => ToolExecutionError::NotUtf8,
                "binary" => ToolExecutionError::Binary,
                "io_error" => ToolExecutionError::IoError(sample_string(ctx)),
                "parse_failed" => ToolExecutionError::ArgumentsParseFailed(sample_string(ctx)),
                _ => ToolExecutionError::UnknownTool,
            },
        )
    }
}

fn sample_event(ctx: &mut noprop::TestCaseContext, core: &AgentCore) -> Event {
    let kind = noprop::sample_choice(
        ctx,
        &[
            "user_msg",
            "cancel",
            "content_delta",
            "reasoning_delta",
            "tool_call_delta",
            "finish",
            "tool_result",
            "transport_error",
            "timeout",
        ],
    );
    match kind {
        "user_msg" => Event::UserMessage(sample_string(ctx)),
        "cancel" => Event::Cancel,
        "content_delta" => Event::ContentDelta {
            request: sample_request_id(ctx, core),
            text: sample_string(ctx),
        },
        "reasoning_delta" => Event::ReasoningDelta {
            request: sample_request_id(ctx, core),
            text: sample_string(ctx),
        },
        "tool_call_delta" => Event::ToolCallDelta {
            request: sample_request_id(ctx, core),
            index: noprop::sample_usize_in(ctx, 0..=3) as u64,
            id: if noprop::sample_bool(ctx) {
                Some(format!("call_{}", noprop::sample_usize_in(ctx, 0..=4)))
            } else {
                None
            },
            function_name: if noprop::sample_bool(ctx) {
                Some(noprop::sample_choice(ctx, &["list", "read", "search", "bogus"]).to_string())
            } else {
                None
            },
            arguments_fragment: if noprop::sample_bool(ctx) {
                Some(sample_string(ctx))
            } else {
                None
            },
        },
        "finish" => Event::Finish {
            request: sample_request_id(ctx, core),
            reason: sample_finish_reason(ctx),
        },
        "tool_result" => Event::ToolResult {
            request: sample_request_id(ctx, core),
            call_id: sample_call_id(ctx, core),
            outcome: sample_tool_outcome(ctx),
        },
        "transport_error" => Event::TransportError {
            request: sample_request_id(ctx, core),
            message: sample_string(ctx),
        },
        "timeout" => Event::Timeout {
            request: sample_request_id(ctx, core),
        },
        other => panic!("unhandled kind {other}"),
    }
}

fn assert_getter_consistency(core: &AgentCore) {
    match core.active_request() {
        Some(_) => {
            assert!(
                core.pending_response().is_some(),
                "active request without pending response"
            );
            assert_ne!(
                core.status(),
                Status::Idle,
                "active request but status is Idle"
            );
        }
        None => {
            assert!(
                core.pending_response().is_none(),
                "no active request yet pending response exists"
            );
            assert_eq!(
                core.status(),
                Status::Idle,
                "no active request but status is not Idle"
            );
        }
    }
}

fn count_assistant(core: &AgentCore) -> usize {
    core.conversation()
        .iter()
        .filter(|m| matches!(m, ChatMessage::Assistant { .. }))
        .count()
}

fn count_user(core: &AgentCore) -> usize {
    core.conversation()
        .iter()
        .filter(|m| matches!(m, ChatMessage::User(_)))
        .count()
}

fn count_tool(core: &AgentCore) -> usize {
    core.conversation()
        .iter()
        .filter(|m| matches!(m, ChatMessage::Tool { .. }))
        .count()
}

#[test]
fn public_contract_holds_across_random_event_sequences() -> noprop::Result<()> {
    let seed = noprop::seed_from_env_or_time(SEED_ENV).expect("valid seed");
    noprop::Runner::new(seed, ITERATIONS).run(|ctx| {
        let mut core = AgentCore::new();
        assert_getter_consistency(&core);

        let mut previous_conv_len: usize = 0;
        let mut previous_assistant: usize = 0;
        let mut events_handled: u64 = 0;

        for _ in 0..STEPS {
            let event = sample_event(ctx, &core);

            let actions = core.handle_event(event);
            events_handled += 1;

            assert_getter_consistency(&core);
            assert!(
                core.conversation().len() >= previous_conv_len,
                "conversation shrunk from {previous_conv_len} to {}",
                core.conversation().len()
            );
            assert!(
                count_assistant(&core) >= previous_assistant,
                "assistant message count decreased",
            );

            // Every assistant turn is bracketed by a preceding user or
            // tool message: the first assistant in a turn answers a
            // user prompt; every subsequent one (in the tool loop)
            // answers a batch of tool results. So the count is bounded
            // by user + tool.
            assert!(
                count_user(&core) + count_tool(&core) >= count_assistant(&core),
                "user({}) + tool({}) < assistant({})",
                count_user(&core),
                count_tool(&core),
                count_assistant(&core),
            );
            // finishes_committed is the number of assistant turns
            // materialised via a finish event; it is exactly the
            // number of assistant messages in the conversation.
            assert_eq!(
                count_assistant(&core) as u64,
                core.metrics().finishes_committed.get(),
                "assistant count {} != finishes_committed {}",
                count_assistant(&core),
                core.metrics().finishes_committed.get(),
            );

            let start_requests = actions
                .iter()
                .filter(|a| matches!(a, Action::StartRequest { .. }))
                .count();
            assert!(
                start_requests <= 1,
                "single handle emitted {start_requests} StartRequest actions",
            );

            let m = core.metrics();
            // Every input event lands in exactly one of these bins.
            // `tool_call_arguments_fragments_dropped_over_limit` and
            // the *_rejected_by_* / tool_calls_executed counters are
            // per-slot/per-call bookkeeping, not per-event totals, so
            // they are not summed here.
            let total = m.user_messages_accepted.get()
                + m.user_messages_rejected_while_active.get()
                + m.cancels_applied.get()
                + m.cancels_ignored_when_idle.get()
                + m.content_deltas_appended.get()
                + m.content_deltas_dropped_as_stale.get()
                + m.reasoning_deltas_appended.get()
                + m.reasoning_deltas_dropped_as_stale.get()
                + m.tool_call_deltas_appended.get()
                + m.tool_call_deltas_dropped_as_stale.get()
                + m.finishes_committed.get()
                + m.finishes_dropped_as_stale.get()
                + m.tool_results_committed.get()
                + m.tool_results_dropped_as_stale.get()
                + m.patch_previews_committed.get()
                + m.patch_previews_dropped_as_stale.get()
                + m.patch_approvals_committed.get()
                + m.patch_approvals_dropped_as_stale.get()
                + m.patch_rejections_committed.get()
                + m.patch_rejections_dropped_as_stale.get()
                + m.transport_errors_recorded.get()
                + m.transport_errors_dropped_as_stale.get()
                + m.timeouts_applied.get()
                + m.timeouts_dropped_as_stale.get();
            assert_eq!(
                total, events_handled,
                "metrics counter total {total} != events fed {events_handled}",
            );
            assert!(m.finishes_committed.get() <= events_handled);
            assert!(m.user_messages_accepted.get() <= events_handled);

            previous_conv_len = core.conversation().len();
            previous_assistant = count_assistant(&core);
        }
        Ok(())
    })?;
    Ok(())
}
