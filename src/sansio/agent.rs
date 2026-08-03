//! Sans I/O agent core.
//!
//! [`AgentCore`] owns the conversation, the in-flight request, and the
//! transient display state; the surrounding I/O shell feeds it
//! [`Event`]s and executes the returned [`Action`]s. No transport,
//! terminal, filesystem, or subprocess is touched here.
//!
//! At most one model request is active at a time. Any events tagged
//! with a [`RequestId`] that is not the current one are silently
//! dropped, which lets the shell forward late arrivals from a
//! cancelled or completed request without corrupting state.
//!
//! Tool calls, reasoning-content carry-over across turns, automatic
//! retry, and side-effect approvals are intentionally out of scope for
//! this issue; they land on top of these primitives in later work.

use crate::sansio::deepseek::{ChatMessage, Role};
use crate::sansio::tui::AgentView;

/// Opaque identifier for a model request tracked by the core.
///
/// The core assigns IDs internally; the surrounding shell receives
/// them via [`Action::StartRequest`] and tags subsequent events with
/// the same value. Tests and stubs can mint synthetic IDs with
/// [`Self::new`] to exercise stale-event handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Coarse-grained runtime status of the core.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Status {
    /// No request is in flight and the shell can accept a new prompt.
    #[default]
    Idle,
    /// A request has been issued but no delta has been received yet.
    AwaitingModel,
    /// The current request has begun streaming content or reasoning.
    Streaming,
}

/// Buffered pieces of the assistant response for the in-flight request.
///
/// `content` is the user-visible answer accumulated so far;
/// `reasoning` is the DeepSeek thinking-mode extension surfaced for
/// display only (not carried over between turns at this stage).
/// `finish_reason` becomes `Some` after the final chunk has been
/// observed but before the response is committed to the conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingResponse {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
}

/// Input event fed to the core by the surrounding I/O shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The user submitted a new prompt. Ignored while a request is
    /// already in flight.
    UserMessage(String),
    /// The user requested to abort the current request.
    Cancel,
    /// A content delta arrived from the transport.
    ContentDelta { request: RequestId, text: String },
    /// A reasoning-content delta arrived from the transport.
    ReasoningDelta { request: RequestId, text: String },
    /// The transport observed the terminating `[DONE]` or finish reason.
    Finish {
        request: RequestId,
        reason: Option<String>,
    },
    /// The transport reported an unrecoverable error for this request.
    TransportError { request: RequestId, message: String },
    /// The request exceeded its allotted time.
    Timeout { request: RequestId },
}

/// Output action produced by the core for the surrounding I/O shell.
///
/// The shell renders the state after every batch that contains
/// [`Action::Redraw`]; other variants direct the transport or surface
/// a diagnostic to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hand `messages` to the transport and forward returned events
    /// back to the core tagged with `id`.
    StartRequest {
        id: RequestId,
        messages: Vec<ChatMessage>,
    },
    /// Ask the transport to abort the request identified by `id`. The
    /// shell may still receive late events for this ID; they will be
    /// dropped when forwarded back to the core.
    CancelRequest { id: RequestId },
    /// A diagnostic message the shell should surface to the user
    /// (transport failure, timeout, etc.). The core does not retain
    /// it; the shell owns any "sticky until dismissed" behaviour.
    ReportError { message: String },
    /// Something visible to the user changed; the shell should redraw.
    Redraw,
}

/// The Sans I/O agent core.
#[derive(Debug, Clone, Default)]
pub struct AgentCore {
    conversation: Vec<ChatMessage>,
    pending: Option<Pending>,
    status: Status,
    next_id: u64,
    metrics: AgentMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    id: RequestId,
    response: PendingResponse,
}

/// Cumulative counters for the branches taken by
/// [`AgentCore::handle_event`].
///
/// Each field increases exactly once per event that lands in the
/// corresponding branch; drop paths (stale event id, event received
/// while idle, etc.) also increment their own counter so the sum of a
/// pair (`*_accepted` + `*_rejected` / `*_appended` + `*_dropped_as_stale`)
/// tells you how many events of a given kind the core has seen.
/// All counters use [`u64::saturating_add`] so overflow saturates at
/// [`u64::MAX`] rather than wrapping.
///
/// Read via [`AgentCore::metrics`]. There is no reset API; take
/// snapshots by cloning if you need to compute deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMetrics {
    /// [`Event::UserMessage`] received while idle: the message was
    /// appended to the conversation and an [`Action::StartRequest`]
    /// was emitted.
    pub user_messages_accepted: u64,
    /// [`Event::UserMessage`] received while another request was
    /// still in flight; the message was dropped.
    pub user_messages_rejected_while_active: u64,
    /// [`Event::Cancel`] received while a request was in flight; the
    /// pending response was dropped and [`Action::CancelRequest`] was
    /// emitted.
    pub cancels_applied: u64,
    /// [`Event::Cancel`] received while idle; no state changed.
    pub cancels_ignored_when_idle: u64,
    /// [`Event::ContentDelta`] whose `request` matched the active
    /// [`RequestId`]; the text was appended to the pending response.
    pub content_deltas_appended: u64,
    /// [`Event::ContentDelta`] dropped because no request was active
    /// or the `request` id did not match the active one.
    pub content_deltas_dropped_as_stale: u64,
    /// [`Event::ReasoningDelta`] whose `request` matched the active
    /// [`RequestId`]; the text was appended to the pending reasoning
    /// buffer.
    pub reasoning_deltas_appended: u64,
    /// [`Event::ReasoningDelta`] dropped because no request was
    /// active or the `request` id did not match the active one.
    pub reasoning_deltas_dropped_as_stale: u64,
    /// [`Event::Finish`] whose `request` matched the active
    /// [`RequestId`]; the assistant message was committed to the
    /// conversation.
    pub finishes_committed: u64,
    /// [`Event::Finish`] dropped because no request was active or the
    /// `request` id did not match the active one.
    pub finishes_dropped_as_stale: u64,
    /// [`Event::TransportError`] whose `request` matched the active
    /// [`RequestId`]; an [`Action::ReportError`] was emitted.
    pub transport_errors_recorded: u64,
    /// [`Event::TransportError`] dropped because no request was
    /// active or the `request` id did not match the active one.
    pub transport_errors_dropped_as_stale: u64,
    /// [`Event::Timeout`] whose `request` matched the active
    /// [`RequestId`]; the request was cancelled and an
    /// [`Action::ReportError`] was emitted.
    pub timeouts_applied: u64,
    /// [`Event::Timeout`] dropped because no request was active or
    /// the `request` id did not match the active one.
    pub timeouts_dropped_as_stale: u64,
}

fn inc(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

impl AgentCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The committed user / assistant message history.
    pub fn conversation(&self) -> &[ChatMessage] {
        &self.conversation
    }

    /// The buffered response for the in-flight request, if any.
    pub fn pending_response(&self) -> Option<&PendingResponse> {
        self.pending.as_ref().map(|p| &p.response)
    }

    /// The ID of the in-flight request, if any.
    pub fn active_request(&self) -> Option<RequestId> {
        self.pending.as_ref().map(|p| p.id)
    }

    /// Read-only projection consumed by pure functions such as
    /// [`crate::sansio::tui::handle_key`].
    pub fn view(&self) -> AgentView {
        AgentView {
            has_active_request: self.pending.is_some(),
        }
    }

    /// Coarse runtime status suitable for a status line.
    pub fn status(&self) -> Status {
        self.status
    }

    /// Cumulative metrics for the branches taken by
    /// [`Self::handle_event`] over the lifetime of this instance.
    pub fn metrics(&self) -> &AgentMetrics {
        &self.metrics
    }

    /// Apply a single input event and return the resulting actions.
    pub fn handle_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::UserMessage(text) => self.on_user_message(text),
            Event::Cancel => self.on_cancel(),
            Event::ContentDelta { request, text } => self.on_content_delta(request, text),
            Event::ReasoningDelta { request, text } => self.on_reasoning_delta(request, text),
            Event::Finish { request, reason } => self.on_finish(request, reason),
            Event::TransportError { request, message } => self.on_transport_error(request, message),
            Event::Timeout { request } => self.on_timeout(request),
        }
    }

    fn on_user_message(&mut self, text: String) -> Vec<Action> {
        if self.pending.is_some() {
            inc(&mut self.metrics.user_messages_rejected_while_active);
            return Vec::new();
        }
        self.conversation.push(ChatMessage {
            role: Role::User,
            content: text,
        });
        let id = self.mint_id();
        self.pending = Some(Pending {
            id,
            response: PendingResponse::default(),
        });
        self.status = Status::AwaitingModel;
        inc(&mut self.metrics.user_messages_accepted);
        vec![
            Action::StartRequest {
                id,
                messages: self.conversation.clone(),
            },
            Action::Redraw,
        ]
    }

    fn on_cancel(&mut self) -> Vec<Action> {
        let Some(pending) = self.pending.take() else {
            inc(&mut self.metrics.cancels_ignored_when_idle);
            return Vec::new();
        };
        self.status = Status::Idle;
        inc(&mut self.metrics.cancels_applied);
        vec![Action::CancelRequest { id: pending.id }, Action::Redraw]
    }

    fn on_content_delta(&mut self, request: RequestId, text: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            inc(&mut self.metrics.content_deltas_dropped_as_stale);
            return Vec::new();
        };
        if pending.id != request {
            inc(&mut self.metrics.content_deltas_dropped_as_stale);
            return Vec::new();
        }
        pending.response.content.push_str(&text);
        self.status = Status::Streaming;
        inc(&mut self.metrics.content_deltas_appended);
        vec![Action::Redraw]
    }

    fn on_reasoning_delta(&mut self, request: RequestId, text: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            inc(&mut self.metrics.reasoning_deltas_dropped_as_stale);
            return Vec::new();
        };
        if pending.id != request {
            inc(&mut self.metrics.reasoning_deltas_dropped_as_stale);
            return Vec::new();
        }
        pending.response.reasoning.push_str(&text);
        self.status = Status::Streaming;
        inc(&mut self.metrics.reasoning_deltas_appended);
        vec![Action::Redraw]
    }

    fn on_finish(&mut self, request: RequestId, reason: Option<String>) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            inc(&mut self.metrics.finishes_dropped_as_stale);
            return Vec::new();
        };
        if pending_ref.id != request {
            inc(&mut self.metrics.finishes_dropped_as_stale);
            return Vec::new();
        }
        let mut pending = self.pending.take().expect("checked above");
        pending.response.finish_reason = reason;
        self.conversation.push(ChatMessage {
            role: Role::Assistant,
            content: pending.response.content,
        });
        self.status = Status::Idle;
        inc(&mut self.metrics.finishes_committed);
        vec![Action::Redraw]
    }

    fn on_transport_error(&mut self, request: RequestId, message: String) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            inc(&mut self.metrics.transport_errors_dropped_as_stale);
            return Vec::new();
        };
        if pending_ref.id != request {
            inc(&mut self.metrics.transport_errors_dropped_as_stale);
            return Vec::new();
        }
        self.pending = None;
        self.status = Status::Idle;
        inc(&mut self.metrics.transport_errors_recorded);
        vec![Action::ReportError { message }, Action::Redraw]
    }

    fn on_timeout(&mut self, request: RequestId) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            inc(&mut self.metrics.timeouts_dropped_as_stale);
            return Vec::new();
        };
        if pending_ref.id != request {
            inc(&mut self.metrics.timeouts_dropped_as_stale);
            return Vec::new();
        }
        let id = pending_ref.id;
        self.pending = None;
        self.status = Status::Idle;
        inc(&mut self.metrics.timeouts_applied);
        vec![
            Action::CancelRequest { id },
            Action::ReportError {
                message: "request timed out".to_string(),
            },
            Action::Redraw,
        ]
    }

    fn mint_id(&mut self) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(core: &mut AgentCore, text: &str) -> Vec<Action> {
        core.handle_event(Event::UserMessage(text.to_string()))
    }

    fn last_start_id(actions: &[Action]) -> RequestId {
        for action in actions {
            if let Action::StartRequest { id, .. } = action {
                return *id;
            }
        }
        panic!("no StartRequest in {actions:?}");
    }

    #[test]
    fn user_message_from_idle_starts_request_and_appends_user_turn() {
        let mut core = AgentCore::new();
        let actions = user(&mut core, "hello");
        assert_eq!(core.status(), Status::AwaitingModel);
        assert_eq!(core.conversation().len(), 1);
        assert_eq!(core.conversation()[0].role, Role::User);
        assert!(matches!(actions[0], Action::StartRequest { .. }));
        assert!(actions.contains(&Action::Redraw));
        assert!(core.active_request().is_some());
    }

    #[test]
    fn user_message_while_active_is_ignored() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "first");
        let actions = user(&mut core, "second");
        assert!(actions.is_empty());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn content_delta_accumulates_and_flips_status_to_streaming() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = core.handle_event(Event::ContentDelta {
            request: id,
            text: "he".to_string(),
        });
        assert_eq!(actions, vec![Action::Redraw]);
        assert_eq!(core.status(), Status::Streaming);
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "llo".to_string(),
        });
        let pending = core.pending_response().expect("pending");
        assert_eq!(pending.content, "hello");
    }

    #[test]
    fn reasoning_delta_accumulates_separately_from_content() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ReasoningDelta {
            request: id,
            text: "let me think".to_string(),
        });
        let pending = core.pending_response().expect("pending");
        assert_eq!(pending.reasoning, "let me think");
        assert!(pending.content.is_empty());
    }

    #[test]
    fn finish_commits_assistant_turn_and_returns_to_idle() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "hello".to_string(),
        });
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("stop".to_string()),
        });
        assert_eq!(actions, vec![Action::Redraw]);
        assert_eq!(core.status(), Status::Idle);
        assert!(core.active_request().is_none());
        assert!(core.pending_response().is_none());
        assert_eq!(core.conversation().len(), 2);
        assert_eq!(core.conversation()[1].role, Role::Assistant);
        assert_eq!(core.conversation()[1].content, "hello");
    }

    #[test]
    fn cancel_drops_pending_response_and_asks_transport_to_cancel() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "partial".to_string(),
        });
        let actions = core.handle_event(Event::Cancel);
        assert!(actions.contains(&Action::CancelRequest { id }));
        assert!(actions.contains(&Action::Redraw));
        assert_eq!(core.status(), Status::Idle);
        assert!(core.pending_response().is_none());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn cancel_from_idle_is_no_op() {
        let mut core = AgentCore::new();
        assert!(core.handle_event(Event::Cancel).is_empty());
    }

    #[test]
    fn transport_error_ends_request_and_reports_message() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = core.handle_event(Event::TransportError {
            request: id,
            message: "boom".to_string(),
        });
        assert!(actions.contains(&Action::ReportError {
            message: "boom".to_string(),
        }));
        assert!(actions.contains(&Action::Redraw));
        assert_eq!(core.status(), Status::Idle);
        assert!(core.pending_response().is_none());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn timeout_cancels_transport_and_reports_error() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let actions = core.handle_event(Event::Timeout { request: id });
        assert!(actions.contains(&Action::CancelRequest { id }));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::ReportError { .. })),
            "expected ReportError, got {actions:?}",
        );
        assert!(actions.contains(&Action::Redraw));
        assert!(core.pending_response().is_none());
    }

    #[test]
    fn stale_deltas_are_dropped_without_state_change() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: Some("stop".to_string()),
        });
        // Late delta from the finished request.
        let actions = core.handle_event(Event::ContentDelta {
            request: id,
            text: "late".to_string(),
        });
        assert!(actions.is_empty());
        assert_eq!(core.conversation().len(), 2);
    }

    #[test]
    fn events_tagged_with_unknown_id_are_dropped() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "hi");
        let actions = core.handle_event(Event::ContentDelta {
            request: RequestId::new(u64::MAX),
            text: "x".to_string(),
        });
        assert!(actions.is_empty());
        assert!(core.pending_response().expect("pending").content.is_empty());
    }

    #[test]
    fn each_start_request_gets_a_unique_id() {
        let mut core = AgentCore::new();
        let id1 = last_start_id(&user(&mut core, "first"));
        let _ = core.handle_event(Event::Finish {
            request: id1,
            reason: None,
        });
        let id2 = last_start_id(&user(&mut core, "second"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn late_finish_after_cancel_is_ignored() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Cancel);
        let actions = core.handle_event(Event::Finish {
            request: id,
            reason: Some("stop".to_string()),
        });
        assert!(actions.is_empty());
        assert_eq!(core.conversation().len(), 1);
    }

    #[test]
    fn start_request_carries_full_conversation_snapshot() {
        let mut core = AgentCore::new();
        let id1 = last_start_id(&user(&mut core, "first"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id1,
            text: "one".to_string(),
        });
        let _ = core.handle_event(Event::Finish {
            request: id1,
            reason: None,
        });
        let actions = user(&mut core, "second");
        let messages = actions.iter().find_map(|a| match a {
            Action::StartRequest { messages, .. } => Some(messages.clone()),
            _ => None,
        });
        let messages = messages.expect("StartRequest present");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::User);
        assert_eq!(messages[2].content, "second");
    }

    // -------------------------------------------------------------
    // metrics
    // -------------------------------------------------------------

    #[test]
    fn metrics_start_at_zero() {
        let core = AgentCore::new();
        assert_eq!(*core.metrics(), AgentMetrics::default());
    }

    #[test]
    fn user_message_accepted_and_rejected_counters() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "first");
        assert_eq!(core.metrics().user_messages_accepted, 1);
        assert_eq!(core.metrics().user_messages_rejected_while_active, 0);
        // Second user message while first is still active is rejected.
        let _ = user(&mut core, "second");
        assert_eq!(core.metrics().user_messages_accepted, 1);
        assert_eq!(core.metrics().user_messages_rejected_while_active, 1);
    }

    #[test]
    fn cancel_applied_and_ignored_counters() {
        let mut core = AgentCore::new();
        let _ = core.handle_event(Event::Cancel);
        assert_eq!(core.metrics().cancels_applied, 0);
        assert_eq!(core.metrics().cancels_ignored_when_idle, 1);
        let _ = user(&mut core, "hi");
        let _ = core.handle_event(Event::Cancel);
        assert_eq!(core.metrics().cancels_applied, 1);
        assert_eq!(core.metrics().cancels_ignored_when_idle, 1);
    }

    #[test]
    fn content_delta_appended_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ContentDelta {
            request: id,
            text: "a".to_string(),
        });
        let _ = core.handle_event(Event::ContentDelta {
            request: RequestId::new(u64::MAX),
            text: "b".to_string(),
        });
        assert_eq!(core.metrics().content_deltas_appended, 1);
        assert_eq!(core.metrics().content_deltas_dropped_as_stale, 1);
    }

    #[test]
    fn reasoning_delta_appended_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::ReasoningDelta {
            request: id,
            text: "think".to_string(),
        });
        // No pending: dropped
        let _ = core.handle_event(Event::Cancel);
        let _ = core.handle_event(Event::ReasoningDelta {
            request: id,
            text: "late".to_string(),
        });
        assert_eq!(core.metrics().reasoning_deltas_appended, 1);
        assert_eq!(core.metrics().reasoning_deltas_dropped_as_stale, 1);
    }

    #[test]
    fn finish_committed_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: None,
        });
        // Second finish for the (now completed) request is stale.
        let _ = core.handle_event(Event::Finish {
            request: id,
            reason: None,
        });
        assert_eq!(core.metrics().finishes_committed, 1);
        assert_eq!(core.metrics().finishes_dropped_as_stale, 1);
    }

    #[test]
    fn transport_error_recorded_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::TransportError {
            request: id,
            message: "boom".to_string(),
        });
        // No pending: dropped
        let _ = core.handle_event(Event::TransportError {
            request: id,
            message: "late".to_string(),
        });
        assert_eq!(core.metrics().transport_errors_recorded, 1);
        assert_eq!(core.metrics().transport_errors_dropped_as_stale, 1);
    }

    #[test]
    fn timeout_applied_and_dropped_counters() {
        let mut core = AgentCore::new();
        let id = last_start_id(&user(&mut core, "hi"));
        let _ = core.handle_event(Event::Timeout { request: id });
        // No pending: dropped
        let _ = core.handle_event(Event::Timeout { request: id });
        assert_eq!(core.metrics().timeouts_applied, 1);
        assert_eq!(core.metrics().timeouts_dropped_as_stale, 1);
    }

    #[test]
    fn other_counters_do_not_move_on_a_single_event() {
        let mut core = AgentCore::new();
        let _ = user(&mut core, "hi");
        let m = core.metrics();
        assert_eq!(m.user_messages_accepted, 1);
        // Every other counter is zero.
        assert_eq!(m.user_messages_rejected_while_active, 0);
        assert_eq!(m.cancels_applied, 0);
        assert_eq!(m.cancels_ignored_when_idle, 0);
        assert_eq!(m.content_deltas_appended, 0);
        assert_eq!(m.content_deltas_dropped_as_stale, 0);
        assert_eq!(m.reasoning_deltas_appended, 0);
        assert_eq!(m.reasoning_deltas_dropped_as_stale, 0);
        assert_eq!(m.finishes_committed, 0);
        assert_eq!(m.finishes_dropped_as_stale, 0);
        assert_eq!(m.transport_errors_recorded, 0);
        assert_eq!(m.transport_errors_dropped_as_stale, 0);
        assert_eq!(m.timeouts_applied, 0);
        assert_eq!(m.timeouts_dropped_as_stale, 0);
    }

    #[test]
    fn counters_saturate_at_u64_max() {
        let mut counter: u64 = u64::MAX;
        inc(&mut counter);
        assert_eq!(counter, u64::MAX);
    }
}
