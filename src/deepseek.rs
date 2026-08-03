//! HTTP/1.1 + TLS transport for the DeepSeek streaming Chat Completions
//! API.
//!
//! This module owns the async I/O side that surrounds the Sans I/O
//! parsers in [`crate::sansio`]. A caller builds a [`DeepSeekClient`],
//! submits a [`ChatRequest`], and drains events from the returned
//! [`tokio::sync::mpsc::Receiver`] until it closes.
//!
//! The prototype opens a fresh TLS connection per request and sends
//! `Connection: close`. Persistent connection reuse and automatic retry
//! are deferred to later issues.

use std::sync::Arc;

use rustls_platform_verifier::BuilderVerifierExt;
use shiguredo_http11::{BodyProgress, Method, Request as HttpRequest, ResponseDecoder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{self, ClientConfig, pki_types::ServerName};

use crate::sansio::deepseek::{ChatRequest, StreamPayload, decode_stream_payload};
use crate::sansio::sse::{SseDecoder, SseError, SseEvent};

const DEEPSEEK_HOST: &str = "api.deepseek.com";
const DEEPSEEK_PORT: u16 = 443;
const DEEPSEEK_PATH: &str = "/chat/completions";
const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
const CHANNEL_CAPACITY: usize = 32;
const READ_CHUNK_SIZE: usize = 8192;

/// A discrete event emitted while draining the streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    Comment(String),
    Finish { reason: Option<String> },
}

/// Errors produced by the transport layer or bubbling up from the
/// underlying HTTP / TLS / SSE / JSON decoders.
#[derive(Debug)]
pub enum TransportError {
    MissingApiKey,
    Config(String),
    Io(std::io::Error),
    Tls(String),
    InvalidHost(String),
    HttpEncode(String),
    HttpDecode(String),
    UnexpectedStatus { status: u16 },
    Sse(SseError),
    Payload(nojson::JsonParseError),
    ChannelClosed,
    ServerClosedBeforeDone,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::MissingApiKey => write!(
                f,
                "environment variable {API_KEY_ENV} is not set or is empty"
            ),
            TransportError::Config(msg) => write!(f, "TLS configuration failed: {msg}"),
            TransportError::Io(err) => write!(f, "I/O error: {err}"),
            TransportError::Tls(msg) => write!(f, "TLS handshake failed: {msg}"),
            TransportError::InvalidHost(msg) => write!(f, "invalid host: {msg}"),
            TransportError::HttpEncode(msg) => write!(f, "failed to encode HTTP request: {msg}"),
            TransportError::HttpDecode(msg) => write!(f, "failed to decode HTTP response: {msg}"),
            TransportError::UnexpectedStatus { status } => {
                write!(f, "server returned unexpected HTTP status {status}")
            }
            TransportError::Sse(err) => write!(f, "SSE decode error: {err}"),
            TransportError::Payload(err) => write!(f, "streaming payload decode error: {err}"),
            TransportError::ChannelClosed => f.write_str("the receiver dropped the stream channel"),
            TransportError::ServerClosedBeforeDone => {
                f.write_str("the server closed the connection before the stream completed")
            }
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::Io(err) => Some(err),
            TransportError::Sse(err) => Some(err),
            TransportError::Payload(err) => Some(err),
            _ => None,
        }
    }
}

/// A secret string whose value never appears in [`std::fmt::Debug`]
/// output.
#[derive(Clone)]
struct SecretString(Arc<String>);

impl SecretString {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Client that speaks the streaming Chat Completions API against
/// `api.deepseek.com`.
#[derive(Clone, Debug)]
pub struct DeepSeekClient {
    api_key: SecretString,
    host: String,
    port: u16,
    path: String,
    tls_config: Arc<ClientConfig>,
    server_name: ServerName<'static>,
}

impl DeepSeekClient {
    /// Build a client using the API key from `DEEPSEEK_API_KEY` and
    /// the default DeepSeek endpoint.
    ///
    /// Returns [`TransportError::MissingApiKey`] when the variable is
    /// unset or empty. The raw value never appears in the error or in
    /// any log output.
    pub fn from_env() -> Result<Self, TransportError> {
        let api_key = match std::env::var(API_KEY_ENV) {
            Ok(value) if !value.is_empty() => SecretString(Arc::new(value)),
            _ => return Err(TransportError::MissingApiKey),
        };
        let tls_config = build_client_config()?;
        let server_name = ServerName::try_from(DEEPSEEK_HOST)
            .map_err(|err| TransportError::InvalidHost(err.to_string()))?
            .to_owned();
        Ok(Self {
            api_key,
            host: DEEPSEEK_HOST.to_string(),
            port: DEEPSEEK_PORT,
            path: DEEPSEEK_PATH.to_string(),
            tls_config: Arc::new(tls_config),
            server_name,
        })
    }

    /// Send the request and return a channel that yields events until
    /// the stream terminates or an error occurs.
    ///
    /// The returned receiver closes cleanly on `[DONE]`; any error is
    /// delivered as the final `Err` before the channel closes. Dropping
    /// the receiver causes the background task to stop reading from
    /// the connection on its next send.
    pub fn call(
        &self,
        request: ChatRequest,
    ) -> mpsc::Receiver<Result<StreamEvent, TransportError>> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let client = self.clone();
        tokio::spawn(async move {
            if let Err(err) = run_stream(client, request, &tx).await {
                let _ = tx.send(Err(err)).await;
            }
        });
        rx
    }
}

fn build_client_config() -> Result<ClientConfig, TransportError> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| TransportError::Config(err.to_string()))?
        .with_platform_verifier()
        .map_err(|err| TransportError::Config(err.to_string()))
        .map(|builder| builder.with_no_client_auth())
}

async fn run_stream(
    client: DeepSeekClient,
    request: ChatRequest,
    tx: &mpsc::Sender<Result<StreamEvent, TransportError>>,
) -> Result<(), TransportError> {
    let body = request.to_json_string();
    let host_header = if client.port == 443 {
        client.host.clone()
    } else {
        format!("{}:{}", client.host, client.port)
    };
    let auth = format!("Bearer {}", client.api_key.expose());
    let http_request = HttpRequest::new(Method::POST, client.path.as_str())
        .and_then(|r| r.header("Host", host_header))
        .and_then(|r| r.header("Authorization", auth))
        .and_then(|r| r.header("Accept", "text/event-stream"))
        .and_then(|r| r.header("Content-Type", "application/json"))
        .and_then(|r| r.header("Connection", "close"))
        .map_err(|err| TransportError::HttpEncode(err.to_string()))?
        .body(body.into_bytes());
    let request_bytes = http_request
        .encode()
        .map_err(|err| TransportError::HttpEncode(err.to_string()))?;

    let tcp = TcpStream::connect((client.host.as_str(), client.port))
        .await
        .map_err(TransportError::Io)?;
    let connector = TlsConnector::from(client.tls_config.clone());
    let mut tls = connector
        .connect(client.server_name.clone(), tcp)
        .await
        .map_err(|err| TransportError::Tls(err.to_string()))?;

    tls.write_all(&request_bytes)
        .await
        .map_err(TransportError::Io)?;
    tls.flush().await.map_err(TransportError::Io)?;

    let mut http_decoder = ResponseDecoder::new();
    let mut sse = SseDecoder::new();
    let mut headers_parsed = false;
    let mut body_complete = false;
    let mut stream_done = false;
    let mut read_buf = vec![0u8; READ_CHUNK_SIZE];

    loop {
        if stream_done {
            break;
        }
        let n = tls.read(&mut read_buf).await.map_err(TransportError::Io)?;
        if n == 0 {
            if !body_complete {
                return Err(TransportError::ServerClosedBeforeDone);
            }
            break;
        }
        http_decoder
            .feed(&read_buf[..n])
            .map_err(|err| TransportError::HttpDecode(err.to_string()))?;

        if !headers_parsed {
            match http_decoder
                .decode_headers()
                .map_err(|err| TransportError::HttpDecode(err.to_string()))?
            {
                Some((head, _body_kind)) => {
                    if head.status_code() != 200 {
                        return Err(TransportError::UnexpectedStatus {
                            status: head.status_code(),
                        });
                    }
                    headers_parsed = true;
                }
                None => continue,
            }
        }

        // Move as much body as available into the SSE decoder, then
        // drain any completed SSE events.
        loop {
            if let Some(chunk) = http_decoder.peek_body()
                && !chunk.is_empty()
            {
                sse.feed(chunk);
                let len = chunk.len();
                http_decoder
                    .consume_body(len)
                    .map_err(|err| TransportError::HttpDecode(err.to_string()))?;
                continue;
            }
            match http_decoder
                .progress()
                .map_err(|err| TransportError::HttpDecode(err.to_string()))?
            {
                BodyProgress::Complete { .. } => {
                    body_complete = true;
                    break;
                }
                BodyProgress::Advanced => continue,
                BodyProgress::NeedData => break,
            }
        }

        while let Some(event) = sse.next_event().map_err(TransportError::Sse)? {
            for outgoing in translate_sse_event(event, &mut stream_done)? {
                if tx.send(Ok(outgoing)).await.is_err() {
                    return Err(TransportError::ChannelClosed);
                }
            }
            if stream_done {
                break;
            }
        }

        if body_complete && !stream_done {
            return Err(TransportError::ServerClosedBeforeDone);
        }
    }

    Ok(())
}

fn translate_sse_event(
    event: SseEvent,
    stream_done: &mut bool,
) -> Result<Vec<StreamEvent>, TransportError> {
    match event {
        SseEvent::Comment(content) => Ok(vec![StreamEvent::Comment(content)]),
        SseEvent::Message { data } => {
            let payload = decode_stream_payload(&data).map_err(TransportError::Payload)?;
            match payload {
                StreamPayload::Done => {
                    *stream_done = true;
                    Ok(Vec::new())
                }
                StreamPayload::Chunk(chunk) => {
                    let mut out = Vec::new();
                    if let Some(content) = chunk.content_delta.filter(|s| !s.is_empty()) {
                        out.push(StreamEvent::ContentDelta(content));
                    }
                    if let Some(reasoning) = chunk.reasoning_delta.filter(|s| !s.is_empty()) {
                        out.push(StreamEvent::ReasoningDelta(reasoning));
                    }
                    if chunk.finish_reason.is_some() {
                        out.push(StreamEvent::Finish {
                            reason: chunk.finish_reason,
                        });
                    }
                    Ok(out)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_string_debug_output_redacts_value() {
        let key = SecretString(Arc::new("sk-super-secret".to_string()));
        let debug = format!("{key:?}");
        assert!(
            !debug.contains("sk-super-secret"),
            "raw secret leaked: {debug}"
        );
        assert_eq!(debug, "<redacted>");
    }

    #[test]
    fn translate_content_delta_emits_stream_event() {
        let mut done = false;
        let out = translate_sse_event(
            SseEvent::Message {
                data: r#"{"choices":[{"delta":{"content":"hi"}}]}"#.to_string(),
            },
            &mut done,
        )
        .expect("translate");
        assert_eq!(out, vec![StreamEvent::ContentDelta("hi".to_string())]);
        assert!(!done);
    }

    #[test]
    fn translate_reasoning_delta_emits_stream_event() {
        let mut done = false;
        let out = translate_sse_event(
            SseEvent::Message {
                data: r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#.to_string(),
            },
            &mut done,
        )
        .expect("translate");
        assert_eq!(out, vec![StreamEvent::ReasoningDelta("think".to_string())]);
        assert!(!done);
    }

    #[test]
    fn translate_finish_reason_emits_finish() {
        let mut done = false;
        let out = translate_sse_event(
            SseEvent::Message {
                data: r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string(),
            },
            &mut done,
        )
        .expect("translate");
        assert_eq!(
            out,
            vec![StreamEvent::Finish {
                reason: Some("stop".to_string())
            }]
        );
    }

    #[test]
    fn translate_done_sets_stream_done_flag() {
        let mut done = false;
        let out = translate_sse_event(
            SseEvent::Message {
                data: "[DONE]".to_string(),
            },
            &mut done,
        )
        .expect("translate");
        assert!(out.is_empty());
        assert!(done);
    }

    #[test]
    fn translate_comment_is_surfaced() {
        let mut done = false;
        let out = translate_sse_event(SseEvent::Comment("ping".to_string()), &mut done)
            .expect("translate");
        assert_eq!(out, vec![StreamEvent::Comment("ping".to_string())]);
    }

    #[test]
    fn translate_empty_content_delta_is_dropped() {
        // The first streamed chunk from OpenAI-compatible APIs typically
        // carries an empty content string alongside the assistant role;
        // there is no user-visible token to emit in that case.
        let mut done = false;
        let payload = r#"{"choices":[{"delta":{"content":""}}]}"#.to_string();
        let out =
            translate_sse_event(SseEvent::Message { data: payload }, &mut done).expect("translate");
        assert!(out.is_empty(), "unexpected events: {out:?}");
    }
}
