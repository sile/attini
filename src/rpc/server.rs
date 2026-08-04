//! RPC server: TCP listener + single-client session driving
//! [`AgentCore`] over the wire.
//!
//! Binds `RpcConfig::listen_addr`, prints one JSON line to stdout
//! with the actually-bound address (so `--listen 127.0.0.1:0`
//! round-trips through `read line → parse → get addr`), accepts
//! exactly one client, runs the session until the client
//! disconnects or sends `quit`, and returns. Subsequent
//! concurrent connections while a session is in flight are
//! closed immediately.
//!
//! Notification emission is buffered through an unbounded mpsc
//! channel so the main event-loop arms never `await` on the TCP
//! write. A dedicated writer task drains the channel to the
//! socket.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use nojson::{DisplayJson, Json, JsonFormatter, RawJson};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::deepseek::{DeepSeekClient, StreamEvent, TransportError};
use crate::rpc::RpcConfig;
use crate::rpc::wire::{self, ErrorResponse, Incoming, Notification, SuccessResponse};
use crate::sansio::agent::{
    Action, AgentCore, CommandInvocation, CommandOutputStream, Event, PatchInvocation,
    PatchPreview, PreviewHash, ReadOnlyTool, RequestId, Status, ToolExecutionError, ToolOutcome,
};
use crate::sansio::deepseek::{ChatMessage, ChatRequest, ToolCall};
use crate::sansio::tui;
use crate::tools::ToolExecutor;
use crate::tools::command::{CommandOutputMessage, run_command};
use crate::tui::transcript::{
    ApprovalDecision, AssistantToolCall, CommandStream, MetricsCounters, SessionEndReason,
    ToolKind, TranscriptRecord, TranscriptWriter, now_unix_millis,
};

/// Run the RPC server. Binds, announces the bound address on
/// stdout, accepts one client, runs the session, returns.
pub async fn run(client: DeepSeekClient, config: RpcConfig) -> io::Result<()> {
    let listener = TcpListener::bind(config.listen_addr).await?;
    let local = listener.local_addr()?;
    print_bound_addr(local).await?;

    let (socket, _peer) = listener.accept().await?;
    run_session(listener, socket, client, config).await
}

async fn print_bound_addr(addr: std::net::SocketAddr) -> io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let line = format!("{{\"rpc_bound_addr\":\"{addr}\"}}\n");
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await
}

async fn run_session(
    listener: TcpListener,
    socket: tokio::net::TcpStream,
    client: DeepSeekClient,
    config: RpcConfig,
) -> io::Result<()> {
    let (read_half, write_half) = socket.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Writer task drains outgoing lines to the socket. Buffering
    // means the main loop never `await`s on socket write.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let writer_join = tokio::spawn(writer_task(out_rx, write_half));

    let workspace = std::env::current_dir()?;
    let tool_executor = Arc::new(
        ToolExecutor::new(&workspace)
            .map_err(|e| io::Error::other(format!("workspace {workspace:?}: {e}")))?,
    );

    let (transcript, mut transcript_err_rx) = match config.transcript_path.as_ref() {
        Some(path) => {
            let (writer, err_rx) = TranscriptWriter::open(
                path,
                env!("CARGO_PKG_VERSION").to_string(),
                config.model.clone(),
                workspace.display().to_string(),
            )
            .await
            .map_err(|e| io::Error::other(format!("transcript {path:?}: {e}")))?;
            (Some(writer), Some(err_rx))
        }
        None => (None, None),
    };

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ToolFeedback>();

    let mut agent = AgentCore::new();
    agent.set_workspace_display(workspace.display().to_string());

    let mut session = Session {
        model: config.model.clone(),
        stream: None,
        tool_handles: HashMap::new(),
        tool_executor,
        event_tx,
        workspace: workspace.clone(),
        active_command: None,
        pending_commands: VecDeque::new(),
        transcript,
        conversation_len: 0,
        last_pending_approval: None,
        tool_call_kinds: HashMap::new(),
        out_tx,
    };

    // Server hello: emit session_start notification (and transcript,
    // if enabled — TranscriptWriter already seeded its own
    // session_start on open, so we do not duplicate).
    session.emit_session_start();

    let mut metrics_interval = config.metrics_snapshot_interval.map(tokio::time::interval);
    let mut should_quit = false;
    let mut session_end_reason = SessionEndReason::UserQuit;

    while !should_quit {
        tokio::select! {
            biased;
            line = lines.next_line() => {
                match line? {
                    Some(line) => handle_client_line(&mut session, &mut agent, &client, &line, &mut should_quit),
                    None => {
                        // Client disconnected.
                        session_end_reason = SessionEndReason::Eof;
                        break;
                    }
                }
            }
            accepted = listener.accept() => {
                // Reject 2nd concurrent client.
                let (extra, _peer) = accepted?;
                drop(extra);
            }
            feedback = event_rx.recv() => {
                match feedback {
                    Some(ToolFeedback::Result { request, call_id, outcome }) => {
                        session.tool_handles.remove(&(request, call_id.clone()));
                        on_command_finished(&mut session, &call_id);
                        dispatch(&mut session, &mut agent, &client, Event::ToolResult { request, call_id, outcome });
                    }
                    Some(ToolFeedback::CommandOutputChunk { request, call_id, stream, bytes }) => {
                        emit_command_output_chunk(&mut session, &call_id, stream, &bytes);
                        dispatch(&mut session, &mut agent, &client, Event::CommandOutputChunk { request, call_id, stream, bytes });
                    }
                    Some(ToolFeedback::PatchPreviewReady { request, call_id, preview_hashes, preview }) => {
                        session.tool_handles.remove(&(request, call_id.clone()));
                        dispatch(&mut session, &mut agent, &client, Event::PatchPreviewReady { request, call_id, preview_hashes, preview });
                    }
                    Some(ToolFeedback::PatchPreviewFailed { request, call_id, outcome }) => {
                        session.tool_handles.remove(&(request, call_id.clone()));
                        dispatch(&mut session, &mut agent, &client, Event::ToolResult { request, call_id, outcome });
                    }
                    None => {
                        session_end_reason = SessionEndReason::Eof;
                        break;
                    }
                }
            }
            recv = recv_stream(&mut session.stream) => {
                let request = match session.stream.as_ref() {
                    Some(s) => s.id,
                    None => continue,
                };
                match recv {
                    None => { session.stream = None; }
                    Some(Ok(event)) => {
                        for core_event in translate_stream_event(event, request) {
                            dispatch(&mut session, &mut agent, &client, core_event);
                        }
                    }
                    Some(Err(err)) => {
                        dispatch(&mut session, &mut agent, &client, Event::TransportError {
                            request,
                            message: err.to_string(),
                        });
                        session.stream = None;
                    }
                }
            }
            Some(msg) = wait_transcript_err(&mut transcript_err_rx) => {
                // Transcript writer failed; surface via a notification
                // (no error_banner concept on the RPC side).
                let _ = &msg;
            }
            _ = tick_metrics(&mut metrics_interval) => {
                if let Some(writer) = session.transcript.as_ref() {
                    writer.send(TranscriptRecord::MetricsSnapshot {
                        ts: now_unix_millis(),
                        counters: MetricsCounters::from_agent_metrics(agent.metrics()),
                    });
                }
            }
        }
    }

    // Emit session_end notification + transcript record + shutdown.
    session.emit_session_end(session_end_reason);
    for (_, handle) in session.tool_handles.drain() {
        handle.abort();
    }
    if let Some(writer) = session.transcript.take() {
        writer.shutdown(session_end_reason).await;
    }
    drop(session.out_tx);
    let _ = writer_join.await;
    Ok(())
}

async fn writer_task(
    mut rx: mpsc::UnboundedReceiver<String>,
    mut write: tokio::net::tcp::OwnedWriteHalf,
) {
    while let Some(line) = rx.recv().await {
        if write.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
    let _ = write.shutdown().await;
}

// -------------------------------------------------------------------
// Session state
// -------------------------------------------------------------------

struct Session {
    model: String,
    stream: Option<Stream>,
    tool_handles: HashMap<(RequestId, String), JoinHandle<()>>,
    tool_executor: Arc<ToolExecutor>,
    event_tx: mpsc::UnboundedSender<ToolFeedback>,
    workspace: std::path::PathBuf,
    active_command: Option<ActiveCommand>,
    pending_commands: VecDeque<PendingCommand>,
    transcript: Option<TranscriptWriter>,
    conversation_len: usize,
    last_pending_approval: Option<String>,
    tool_call_kinds: HashMap<String, ToolKind>,
    out_tx: mpsc::UnboundedSender<String>,
}

struct ActiveCommand {
    request: RequestId,
    call_id: String,
    cancel: Arc<Notify>,
    _task: JoinHandle<()>,
}

struct PendingCommand {
    request: RequestId,
    call_id: String,
    invocation: CommandInvocation,
}

#[derive(Debug)]
enum ToolFeedback {
    Result {
        request: RequestId,
        call_id: String,
        outcome: ToolOutcome,
    },
    PatchPreviewReady {
        request: RequestId,
        call_id: String,
        preview_hashes: Vec<PreviewHash>,
        preview: PatchPreview,
    },
    PatchPreviewFailed {
        request: RequestId,
        call_id: String,
        outcome: ToolOutcome,
    },
    CommandOutputChunk {
        request: RequestId,
        call_id: String,
        stream: CommandOutputStream,
        bytes: Vec<u8>,
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

async fn wait_transcript_err(rx: &mut Option<oneshot::Receiver<String>>) -> Option<String> {
    let Some(receiver) = rx.as_mut() else {
        return std::future::pending().await;
    };
    let result = receiver.await.ok();
    *rx = None;
    result
}

async fn tick_metrics(interval: &mut Option<tokio::time::Interval>) -> tokio::time::Instant {
    match interval.as_mut() {
        Some(i) => i.tick().await,
        None => std::future::pending().await,
    }
}

// -------------------------------------------------------------------
// Client line dispatch
// -------------------------------------------------------------------

fn handle_client_line(
    session: &mut Session,
    agent: &mut AgentCore,
    client: &DeepSeekClient,
    line: &str,
    should_quit: &mut bool,
) {
    let incoming = match wire::parse_incoming(line) {
        Ok(v) => v,
        Err(e) => {
            session.send_json(&ErrorResponse::parse_error(e.to_string()));
            return;
        }
    };
    match incoming {
        Incoming::Request {
            id,
            method,
            params_json,
        } => handle_request(
            session,
            agent,
            client,
            id,
            &method,
            &params_json,
            should_quit,
        ),
        Incoming::Notification { .. } => {
            // The server does not consume client notifications.
        }
    }
}

fn handle_request(
    session: &mut Session,
    agent: &mut AgentCore,
    client: &DeepSeekClient,
    id: u64,
    method: &str,
    params_json: &str,
    should_quit: &mut bool,
) {
    match method {
        "submit_prompt" => {
            let text = match parse_string_field(params_json, "text") {
                Ok(v) => v,
                Err(e) => {
                    session.send_json(&ErrorResponse::invalid_params(id, e));
                    return;
                }
            };
            let was_idle = matches!(agent.status(), Status::Idle);
            dispatch(session, agent, client, Event::UserMessage(text));
            let now_active = !matches!(agent.status(), Status::Idle);
            let accepted = was_idle && now_active;
            session.send_json(&SuccessResponse {
                id,
                result: AcceptedResult { accepted },
            });
        }
        "approve" => {
            let call_id = match parse_string_field(params_json, "call_id") {
                Ok(v) => v,
                Err(e) => {
                    session.send_json(&ErrorResponse::invalid_params(id, e));
                    return;
                }
            };
            let applied = agent.view().pending_approval_call_id.as_deref() == Some(&call_id);
            if applied {
                dispatch(
                    session,
                    agent,
                    client,
                    Event::ApproveToolCall {
                        call_id: call_id.clone(),
                    },
                );
            }
            session.send_json(&SuccessResponse {
                id,
                result: AppliedResult { applied },
            });
        }
        "reject" => {
            let call_id = match parse_string_field(params_json, "call_id") {
                Ok(v) => v,
                Err(e) => {
                    session.send_json(&ErrorResponse::invalid_params(id, e));
                    return;
                }
            };
            let applied = agent.view().pending_approval_call_id.as_deref() == Some(&call_id);
            if applied {
                dispatch(
                    session,
                    agent,
                    client,
                    Event::RejectToolCall {
                        call_id: call_id.clone(),
                    },
                );
            }
            session.send_json(&SuccessResponse {
                id,
                result: AppliedResult { applied },
            });
        }
        "cancel" => {
            dispatch(session, agent, client, Event::Cancel);
            session.send_json(&SuccessResponse {
                id,
                result: EmptyResult,
            });
        }
        "get_state" => {
            let phase = status_str(agent.status());
            let pending = agent.view().pending_approval_call_id.clone();
            let active_cmd = session.active_command.as_ref().map(|c| c.call_id.clone());
            session.send_json(&SuccessResponse {
                id,
                result: StateResult {
                    phase,
                    pending_approval_call_id: pending,
                    conversation_length: agent.conversation().len() as u64,
                    active_command_call_id: active_cmd,
                },
            });
        }
        "get_metrics" => {
            let counters = MetricsCounters::from_agent_metrics(agent.metrics());
            session.send_json(&SuccessResponse {
                id,
                result: MetricsResult { counters },
            });
        }
        "get_rendered_grid" => {
            let (rows, cols) = match parse_rows_cols(params_json) {
                Ok(v) => v,
                Err(e) => {
                    session.send_json(&ErrorResponse::invalid_params(id, e));
                    return;
                }
            };
            let ui = tui::UiState {
                model: session.model.clone(),
                draft: String::new(),
            };
            let state = tui::build_render_state(&ui, agent, None);
            let grid = tui::render(&state, (rows, cols));
            session.send_json(&SuccessResponse { id, result: grid });
        }
        "quit" => {
            session.send_json(&SuccessResponse {
                id,
                result: EmptyResult,
            });
            *should_quit = true;
        }
        _ => {
            session.send_json(&ErrorResponse::method_not_found(id, method));
        }
    }
}

fn parse_string_field(params_json: &str, key: &str) -> Result<String, String> {
    let json = RawJson::parse(params_json).map_err(|e| e.to_string())?;
    let value = json.value();
    let m = value
        .to_member(key)
        .map_err(|e| e.to_string())?
        .required()
        .map_err(|e| e.to_string())?;
    let s = m.to_unquoted_string_str().map_err(|e| e.to_string())?;
    Ok(s.into_owned())
}

fn parse_rows_cols(params_json: &str) -> Result<(usize, usize), String> {
    let json = RawJson::parse(params_json).map_err(|e| e.to_string())?;
    let value = json.value();
    let rows: u64 = value
        .to_member("rows")
        .map_err(|e| e.to_string())?
        .required()
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "rows must be a u64".to_string())?;
    let cols: u64 = value
        .to_member("cols")
        .map_err(|e| e.to_string())?
        .required()
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "cols must be a u64".to_string())?;
    Ok((rows as usize, cols as usize))
}

fn status_str(s: Status) -> &'static str {
    match s {
        Status::Idle => "idle",
        Status::AwaitingModel => "awaiting_model",
        Status::Streaming => "streaming",
        Status::ToolRunning => "tool_running",
        Status::AwaitingApproval => "awaiting_approval",
    }
}

// -------------------------------------------------------------------
// Response result payload types (per-method result shapes)
// -------------------------------------------------------------------

struct EmptyResult;
impl DisplayJson for EmptyResult {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|_| Ok(()))
    }
}

struct AcceptedResult {
    accepted: bool,
}
impl DisplayJson for AcceptedResult {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| f.member("accepted", self.accepted))
    }
}

struct AppliedResult {
    applied: bool,
}
impl DisplayJson for AppliedResult {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| f.member("applied", self.applied))
    }
}

struct StateResult {
    phase: &'static str,
    pending_approval_call_id: Option<String>,
    conversation_length: u64,
    active_command_call_id: Option<String>,
}
impl DisplayJson for StateResult {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("phase", self.phase)?;
            f.member("pending_approval_call_id", &self.pending_approval_call_id)?;
            f.member("conversation_length", self.conversation_length)?;
            f.member("active_command_call_id", &self.active_command_call_id)
        })
    }
}

struct MetricsResult {
    counters: MetricsCounters,
}
impl DisplayJson for MetricsResult {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| f.member("counters", &self.counters))
    }
}

// -------------------------------------------------------------------
// Session methods: notification emission + apply_actions
// -------------------------------------------------------------------

impl Session {
    fn send_json<T: DisplayJson>(&self, msg: &T) {
        let mut line = Json(msg).to_string();
        line.push('\n');
        let _ = self.out_tx.send(line);
    }

    fn emit_notification(&self, method: &str, record: &TranscriptRecord) {
        // The transcript record body IS the notification params.
        let notif = Notification {
            method: method.to_string(),
            params: record,
        };
        self.send_json(&notif);
    }

    fn transcript_send(&self, record: TranscriptRecord) {
        if let Some(w) = self.transcript.as_ref() {
            w.send(record);
        }
    }

    fn emit_session_start(&self) {
        let record = TranscriptRecord::SessionStart {
            ts: now_unix_millis(),
            attini_version: env!("CARGO_PKG_VERSION").to_string(),
            model: self.model.clone(),
            workspace: self.workspace.display().to_string(),
        };
        self.emit_notification("event/session_start", &record);
        // TranscriptWriter::open already wrote its own session_start;
        // do not double-write to transcript.
    }

    fn emit_session_end(&self, reason: SessionEndReason) {
        let record = TranscriptRecord::SessionEnd {
            ts: now_unix_millis(),
            reason,
        };
        self.emit_notification("event/session_end", &record);
        // TranscriptWriter::shutdown will append its own session_end.
    }
}

fn emit_command_output_chunk(
    session: &mut Session,
    call_id: &str,
    stream: CommandOutputStream,
    bytes: &[u8],
) {
    let record = TranscriptRecord::CommandOutputChunk {
        ts: now_unix_millis(),
        call_id: call_id.to_string(),
        stream: to_transcript_stream(stream),
        bytes_len: bytes.len() as u64,
        preview: TranscriptRecord::command_preview_from_bytes(bytes),
    };
    session.emit_notification("event/command_output_chunk", &record);
    session.transcript_send(record);
}

fn to_transcript_stream(stream: CommandOutputStream) -> CommandStream {
    match stream {
        CommandOutputStream::Stdout => CommandStream::Stdout,
        CommandOutputStream::Stderr => CommandStream::Stderr,
    }
}

fn dispatch(session: &mut Session, agent: &mut AgentCore, client: &DeepSeekClient, event: Event) {
    if let Some((method, record)) = pre_event_record(&event) {
        session.emit_notification(method, &record);
        session.transcript_send(record);
    }
    let actions = agent.handle_event(event);
    emit_conversation_records(session, agent);
    emit_pending_approval(session, agent);
    apply_actions(session, agent, client, actions);
}

fn pre_event_record(event: &Event) -> Option<(&'static str, TranscriptRecord)> {
    let ts = now_unix_millis();
    match event {
        Event::Cancel => Some(("event/cancel", TranscriptRecord::Cancel { ts })),
        Event::TransportError { message, .. } => Some((
            "event/transport_error",
            TranscriptRecord::TransportError {
                ts,
                message: message.clone(),
            },
        )),
        Event::Finish { reason, .. } => Some((
            "event/finish",
            TranscriptRecord::Finish {
                ts,
                reason: reason.clone(),
            },
        )),
        Event::PatchPreviewReady {
            call_id, preview, ..
        } => Some((
            "event/patch_preview_ready",
            TranscriptRecord::PatchPreviewReady {
                ts,
                call_id: call_id.clone(),
                target_paths: preview.target_paths.clone(),
                added_lines: preview.added_lines,
                removed_lines: preview.removed_lines,
                edit_count: preview.edit_count,
            },
        )),
        Event::ApproveToolCall { call_id } => Some((
            "event/tool_approval",
            TranscriptRecord::ToolApproval {
                ts,
                call_id: call_id.clone(),
                decision: ApprovalDecision::Approve,
            },
        )),
        Event::RejectToolCall { call_id } => Some((
            "event/tool_approval",
            TranscriptRecord::ToolApproval {
                ts,
                call_id: call_id.clone(),
                decision: ApprovalDecision::Reject,
            },
        )),
        _ => None,
    }
}

fn emit_conversation_records(session: &mut Session, agent: &AgentCore) {
    let conv = agent.conversation();
    while session.conversation_len < conv.len() {
        let msg = &conv[session.conversation_len];
        session.conversation_len += 1;
        let (method, record) = match msg {
            ChatMessage::User(text) => (
                "event/user_message",
                TranscriptRecord::UserMessage {
                    ts: now_unix_millis(),
                    text: text.clone(),
                },
            ),
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                // Track function_name → ToolKind for later approval
                // records (patch / command tool_kind resolution).
                for tc in tool_calls {
                    if let Some(kind) = to_tool_kind(&tc.function_name) {
                        session.tool_call_kinds.insert(tc.id.clone(), kind);
                    }
                }
                (
                    "event/assistant_message",
                    TranscriptRecord::AssistantMessage {
                        ts: now_unix_millis(),
                        content: content.clone(),
                        reasoning: reasoning_content.clone(),
                        tool_calls: tool_calls.iter().map(from_wire_tool_call).collect(),
                    },
                )
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => (
                "event/tool_result",
                TranscriptRecord::ToolResult {
                    ts: now_unix_millis(),
                    call_id: tool_call_id.clone(),
                    content: content.clone(),
                },
            ),
            ChatMessage::System(_) => continue,
        };
        session.emit_notification(method, &record);
        session.transcript_send(record);
    }
}

fn emit_pending_approval(session: &mut Session, agent: &AgentCore) {
    let current = agent.view().pending_approval_call_id.clone();
    if session.last_pending_approval == current {
        return;
    }
    session.last_pending_approval = current.clone();
    if let Some(call_id) = current {
        let tool_kind = session
            .tool_call_kinds
            .get(&call_id)
            .copied()
            .unwrap_or(ToolKind::Patch);
        let record = TranscriptRecord::ToolApprovalRequired {
            ts: now_unix_millis(),
            call_id,
            tool_kind,
        };
        session.emit_notification("event/tool_approval_required", &record);
        session.transcript_send(record);
    }
}

fn to_tool_kind(function_name: &str) -> Option<ToolKind> {
    match function_name {
        "patch" => Some(ToolKind::Patch),
        "command" => Some(ToolKind::Command),
        _ => None,
    }
}

fn from_wire_tool_call(tc: &ToolCall) -> AssistantToolCall {
    AssistantToolCall {
        id: tc.id.clone(),
        function_name: tc.function_name.clone(),
        arguments_json: tc.arguments_json.clone(),
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

// -------------------------------------------------------------------
// apply_actions (parallel to src/tui.rs::apply_actions, adapted for
// RPC — no error_banner concept, no draw)
// -------------------------------------------------------------------

fn apply_actions(
    session: &mut Session,
    _agent: &mut AgentCore,
    client: &DeepSeekClient,
    actions: Vec<Action>,
) {
    for action in actions {
        match action {
            Action::StartRequest { id, messages } => {
                let mut tools = ReadOnlyTool::definitions();
                tools.push(PatchInvocation::definition());
                tools.push(CommandInvocation::definition());
                let request = ChatRequest::new(session.model.clone(), messages).with_tools(tools);
                let rx = client.call(request);
                session.stream = Some(Stream { id, rx });
            }
            Action::CancelRequest { .. } => {
                session.stream = None;
            }
            Action::ExecuteTool {
                request,
                call_id,
                invocation,
            } => {
                let exec = session.tool_executor.clone();
                let tx = session.event_tx.clone();
                let key = (request, call_id.clone());
                let handle = tokio::task::spawn_blocking(move || {
                    let outcome = exec.execute(invocation);
                    let _ = tx.send(ToolFeedback::Result {
                        request,
                        call_id,
                        outcome,
                    });
                });
                session.tool_handles.insert(key, handle);
            }
            Action::CancelToolExecution { request } => {
                session.tool_handles.retain(|(req, _), h| {
                    if *req == request {
                        h.abort();
                        false
                    } else {
                        true
                    }
                });
                cancel_commands_for_request(session, request);
            }
            Action::PreviewPatch {
                request,
                call_id,
                invocation,
            } => spawn_preview_patch(session, request, call_id, invocation),
            Action::ApplyPatch {
                request,
                call_id,
                invocation,
                preview_hashes,
            } => spawn_apply_patch(session, request, call_id, invocation, preview_hashes),
            Action::ExecuteCommand {
                request,
                call_id,
                invocation,
            } => {
                if session.active_command.is_some() {
                    session.pending_commands.push_back(PendingCommand {
                        request,
                        call_id,
                        invocation,
                    });
                } else {
                    start_command(session, request, call_id, invocation);
                }
            }
            Action::ReportError { message: _ } => {
                // No error banner on RPC side. TransportError /
                // Timeout events already flow through pre_event_record.
            }
            Action::Redraw => {}
        }
    }
}

fn spawn_preview_patch(
    session: &mut Session,
    request: RequestId,
    call_id: String,
    invocation: PatchInvocation,
) {
    let exec = session.tool_executor.clone();
    let tx = session.event_tx.clone();
    let key = (request, call_id.clone());
    let handle = tokio::task::spawn_blocking(move || match exec.preview_patch(&invocation) {
        Ok((preview_hashes, preview)) => {
            let _ = tx.send(ToolFeedback::PatchPreviewReady {
                request,
                call_id,
                preview_hashes,
                preview,
            });
        }
        Err(err) => {
            let _ = tx.send(ToolFeedback::PatchPreviewFailed {
                request,
                call_id,
                outcome: ToolOutcome::Err(ToolExecutionError::Patch(err)),
            });
        }
    });
    session.tool_handles.insert(key, handle);
}

fn spawn_apply_patch(
    session: &mut Session,
    request: RequestId,
    call_id: String,
    invocation: PatchInvocation,
    preview_hashes: Vec<PreviewHash>,
) {
    let exec = session.tool_executor.clone();
    let tx = session.event_tx.clone();
    let key = (request, call_id.clone());
    let handle = tokio::task::spawn_blocking(move || {
        let outcome = match exec.apply_patch(&invocation, &preview_hashes) {
            Ok(_) => ToolOutcome::Ok(r#"{"applied":true}"#.to_string()),
            Err(err) => ToolOutcome::Err(ToolExecutionError::Patch(err)),
        };
        let _ = tx.send(ToolFeedback::Result {
            request,
            call_id,
            outcome,
        });
    });
    session.tool_handles.insert(key, handle);
}

fn start_command(
    session: &mut Session,
    request: RequestId,
    call_id: String,
    invocation: CommandInvocation,
) {
    let cancel = Arc::new(Notify::new());
    let cancel_task = cancel.clone();
    let cwd = session.workspace.clone();
    let tx = session.event_tx.clone();
    let call_id_for_stream = call_id.clone();
    let call_id_for_result = call_id.clone();

    let task = tokio::spawn(async move {
        let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<CommandOutputMessage>();
        let tx_forward = tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(msg) = chunk_rx.recv().await {
                let _ = tx_forward.send(ToolFeedback::CommandOutputChunk {
                    request,
                    call_id: call_id_for_stream.clone(),
                    stream: msg.stream,
                    bytes: msg.bytes,
                });
            }
        });
        let result = run_command(cwd, invocation, cancel_task, chunk_tx).await;
        let _ = forwarder.await;
        let outcome = match result {
            Ok(res) => ToolOutcome::Ok(res.to_json_string()),
            Err(err) => ToolOutcome::Err(ToolExecutionError::Command(
                crate::sansio::agent::CommandError::SpawnFailed {
                    message: err.to_string(),
                },
            )),
        };
        let _ = tx.send(ToolFeedback::Result {
            request,
            call_id: call_id_for_result,
            outcome,
        });
    });

    session.active_command = Some(ActiveCommand {
        request,
        call_id,
        cancel,
        _task: task,
    });
}

fn on_command_finished(session: &mut Session, call_id: &str) {
    if session
        .active_command
        .as_ref()
        .is_some_and(|c| c.call_id == call_id)
    {
        session.active_command = None;
        if let Some(next) = session.pending_commands.pop_front() {
            start_command(session, next.request, next.call_id, next.invocation);
        }
    }
}

fn cancel_commands_for_request(session: &mut Session, request: RequestId) {
    if let Some(active) = session.active_command.as_ref()
        && active.request == request
    {
        active.cancel.notify_one();
    }
    session.pending_commands.retain(|p| p.request != request);
}
