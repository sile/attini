//! Property-based tests for `attini::sansio::agent::AgentCore`.
//!
//! Generates arbitrary event sequences (mixing real active-request IDs
//! with stale / synthetic ones) and checks that the core's public
//! contract holds after every step.

use attini::sansio::agent::{Action, AgentCore, Event, RequestId, Status};
use attini::sansio::deepseek::Role;

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

fn sample_event(ctx: &mut noprop::TestCaseContext, core: &AgentCore) -> Event {
    let kind = noprop::sample_choice(
        ctx,
        &[
            "user_msg",
            "cancel",
            "content_delta",
            "reasoning_delta",
            "finish",
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
        "finish" => Event::Finish {
            request: sample_request_id(ctx, core),
            reason: if noprop::sample_bool(ctx) {
                Some(sample_string(ctx))
            } else {
                None
            },
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
        .filter(|m| m.role == Role::Assistant)
        .count()
}

fn count_user(core: &AgentCore) -> usize {
    core.conversation()
        .iter()
        .filter(|m| m.role == Role::User)
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
        let mut successful_finishes: usize = 0;
        let mut events_handled: u64 = 0;

        for _ in 0..STEPS {
            let previous_active = core.active_request();
            let event = sample_event(ctx, &core);
            let is_matching_finish = matches!(
                &event,
                Event::Finish { request, .. } if Some(*request) == previous_active
            );

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

            if is_matching_finish {
                successful_finishes += 1;
            }
            assert!(
                count_assistant(&core) <= successful_finishes,
                "assistant count {} exceeds successful finish count {}",
                count_assistant(&core),
                successful_finishes,
            );
            assert!(
                count_user(&core) >= count_assistant(&core),
                "user turns {} < assistant turns {}",
                count_user(&core),
                count_assistant(&core),
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
            let total = m.user_messages_accepted.get()
                + m.user_messages_rejected_while_active.get()
                + m.cancels_applied.get()
                + m.cancels_ignored_when_idle.get()
                + m.content_deltas_appended.get()
                + m.content_deltas_dropped_as_stale.get()
                + m.reasoning_deltas_appended.get()
                + m.reasoning_deltas_dropped_as_stale.get()
                + m.finishes_committed.get()
                + m.finishes_dropped_as_stale.get()
                + m.transport_errors_recorded.get()
                + m.transport_errors_dropped_as_stale.get()
                + m.timeouts_applied.get()
                + m.timeouts_dropped_as_stale.get();
            assert_eq!(
                total, events_handled,
                "metrics counter total {total} != events fed {events_handled}",
            );
            // Success-only counters must never exceed their event-total
            // partners (accepted <= accepted + rejected, and so on).
            assert!(m.finishes_committed.get() <= events_handled);
            assert!(m.user_messages_accepted.get() <= events_handled);

            previous_conv_len = core.conversation().len();
            previous_assistant = count_assistant(&core);
        }
        Ok(())
    })?;
    Ok(())
}
