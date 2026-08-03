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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    id: RequestId,
    response: PendingResponse,
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

    /// Coarse runtime status suitable for a status line.
    pub fn status(&self) -> Status {
        self.status
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
            return Vec::new();
        };
        self.status = Status::Idle;
        vec![Action::CancelRequest { id: pending.id }, Action::Redraw]
    }

    fn on_content_delta(&mut self, request: RequestId, text: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            return Vec::new();
        };
        if pending.id != request {
            return Vec::new();
        }
        pending.response.content.push_str(&text);
        self.status = Status::Streaming;
        vec![Action::Redraw]
    }

    fn on_reasoning_delta(&mut self, request: RequestId, text: String) -> Vec<Action> {
        let Some(pending) = self.pending.as_mut() else {
            return Vec::new();
        };
        if pending.id != request {
            return Vec::new();
        }
        pending.response.reasoning.push_str(&text);
        self.status = Status::Streaming;
        vec![Action::Redraw]
    }

    fn on_finish(&mut self, request: RequestId, reason: Option<String>) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            return Vec::new();
        };
        if pending_ref.id != request {
            return Vec::new();
        }
        let mut pending = self.pending.take().expect("checked above");
        pending.response.finish_reason = reason;
        self.conversation.push(ChatMessage {
            role: Role::Assistant,
            content: pending.response.content,
        });
        self.status = Status::Idle;
        vec![Action::Redraw]
    }

    fn on_transport_error(&mut self, request: RequestId, message: String) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            return Vec::new();
        };
        if pending_ref.id != request {
            return Vec::new();
        }
        self.pending = None;
        self.status = Status::Idle;
        vec![Action::ReportError { message }, Action::Redraw]
    }

    fn on_timeout(&mut self, request: RequestId) -> Vec<Action> {
        let Some(pending_ref) = self.pending.as_ref() else {
            return Vec::new();
        };
        if pending_ref.id != request {
            return Vec::new();
        }
        let id = pending_ref.id;
        self.pending = None;
        self.status = Status::Idle;
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
}
