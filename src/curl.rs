//! Sync HTTP transport for `attini agent` via a `curl` subprocess.
//!
//! Endpoint and credentials:
//! - `DEEPSEEK_API_KEY` (required)
//! - `DEEPSEEK_BASE_URL` (optional OpenAI-compatible base; default
//!   `https://api.deepseek.com`; `/chat/completions` is appended)

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use crate::sansio::deepseek::{
    ChatMessage, ChatRequest, StreamChunk, StreamPayload, StreamToolCallDelta, ToolCall, Usage,
    decode_stream_payload,
};
use crate::sansio::sse::{SseDecoder, SseEvent};

/// Default OpenAI-compatible API base (no trailing path).
/// The chat completions path is appended by [`chat_completions_url`].
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
const BASE_URL_ENV: &str = "DEEPSEEK_BASE_URL";
const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// Build the chat-completions endpoint from an optional base URL.
///
/// `base` should be an OpenAI-compatible root such as
/// `https://api.deepseek.com` or `http://host:8888/v1`. A trailing slash
/// is stripped before `/chat/completions` is appended. When `base` is
/// `None`, [`DEFAULT_BASE_URL`] is used.
fn chat_completions_url(base: Option<&str>) -> Result<String, io::Error> {
    let raw = match base {
        None => DEFAULT_BASE_URL,
        Some("") => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{BASE_URL_ENV} is empty"),
            ));
        }
        Some(s) => s,
    };
    let trimmed = raw.trim_end_matches('/');
    Ok(format!("{trimmed}{CHAT_COMPLETIONS_PATH}"))
}

fn resolve_chat_completions_url() -> io::Result<String> {
    match std::env::var(BASE_URL_ENV) {
        Ok(value) => chat_completions_url(Some(&value)),
        Err(std::env::VarError::NotPresent) => chat_completions_url(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{BASE_URL_ENV} is not valid Unicode"),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    /// Token counters from the response's terminating usage chunk.
    /// `None` when the model or server did not emit one (e.g. the API
    /// silently ignored `stream_options.include_usage`).
    pub usage: Option<Usage>,
}

impl CallResult {
    pub fn into_assistant(self) -> ChatMessage {
        ChatMessage::Assistant {
            content: self.content,
            tool_calls: self.tool_calls,
        }
    }
}

pub struct ProgressSinks<'a> {
    pub content: &'a mut dyn Write,
}

pub fn call(request: &ChatRequest, sinks: &mut ProgressSinks<'_>) -> io::Result<CallResult> {
    let api_key = std::env::var(API_KEY_ENV).map_err(|_| {
        io::Error::new(io::ErrorKind::NotFound, format!("{API_KEY_ENV} is not set"))
    })?;
    if api_key.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{API_KEY_ENV} is empty"),
        ));
    }

    let url = resolve_chat_completions_url()?;
    let body = request.to_json_string();

    let mut child = Command::new("curl")
        .arg("-sS")
        .arg("-N")
        // Fail on HTTP 4xx/5xx while still writing the response body to
        // stdout so we can surface the API error message.
        .arg("--fail-with-body")
        .arg("-H")
        .arg(format!("Authorization: Bearer {api_key}"))
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-H")
        .arg("Accept: text/event-stream")
        .arg("--data-binary")
        .arg("@-")
        .arg(&url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::other(format!("failed to spawn curl: {e}")))?;

    drop(api_key);

    let mut stdin = child
        .stdin
        .take()
        .expect("stdin was piped when spawning curl");
    stdin.write_all(body.as_bytes())?;
    stdin.flush()?;
    drop(stdin);

    let stdout = child
        .stdout
        .take()
        .expect("stdout was piped when spawning curl");
    let (assembly, raw_body) = decode_sse_stream(stdout, sinks)?;

    let mut stderr = child
        .stderr
        .take()
        .expect("stderr was piped when spawning curl");
    let mut stderr_buf = String::new();
    let _ = stderr.read_to_string(&mut stderr_buf);

    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format_curl_failure(
            status,
            &raw_body,
            stderr_buf.trim(),
        )));
    }
    Ok(assembly)
}

/// Prefer the OpenAI-style `error.message` from the response body;
/// fall back to curl's stderr / exit status.
fn format_curl_failure(status: std::process::ExitStatus, body: &[u8], stderr: &str) -> String {
    let body_text = String::from_utf8_lossy(body);
    if let Some(message) = extract_api_error_message(&body_text) {
        return format!("API request failed: {message}");
    }
    let body_text = body_text.trim();
    if !body_text.is_empty() {
        return format!("curl exited with {status}: {body_text}");
    }
    if !stderr.is_empty() {
        return format!("curl exited with {status}: {stderr}");
    }
    format!("curl exited with {status}")
}

/// Pull `error.message` from an OpenAI-compatible error JSON body.
fn extract_api_error_message(body: &str) -> Option<String> {
    let json = nojson::RawJson::parse(body.trim()).ok()?;
    let error = json.value().to_member("error").ok()?.required().ok()?;
    let message: String = error
        .to_member("message")
        .ok()?
        .required()
        .ok()?
        .try_into()
        .ok()?;
    if message.is_empty() {
        None
    } else {
        Some(message)
    }
}

fn decode_sse_stream<R: Read>(
    reader: R,
    sinks: &mut ProgressSinks<'_>,
) -> io::Result<(CallResult, Vec<u8>)> {
    let mut br = BufReader::new(reader);
    let mut decoder = SseDecoder::new();
    let mut assembly = Assembly::default();
    let mut raw_body = Vec::new();

    loop {
        let filled = br.fill_buf()?;
        if filled.is_empty() {
            break;
        }
        raw_body.extend_from_slice(filled);
        decoder.feed(filled);
        let len = filled.len();
        br.consume(len);

        while let Some(event) = decoder
            .next_event()
            .map_err(|e| io::Error::other(format!("sse decode: {e}")))?
        {
            match event {
                SseEvent::Message { data } => {
                    let payload = decode_stream_payload(&data)
                        .map_err(|e| io::Error::other(format!("stream payload: {e}")))?;
                    match payload {
                        StreamPayload::Chunk(chunk) => assembly.absorb_chunk(chunk, sinks),
                        StreamPayload::Done => return Ok((assembly.finish(), raw_body)),
                    }
                }
                SseEvent::Comment(_) => {}
            }
        }
    }
    Ok((assembly.finish(), raw_body))
}

#[derive(Default)]
struct Assembly {
    content: String,
    tool_slots: Vec<ToolSlot>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

#[derive(Default)]
struct ToolSlot {
    index: u64,
    id: String,
    function_name: String,
    arguments_json: String,
}

impl Assembly {
    fn absorb_chunk(&mut self, chunk: StreamChunk, sinks: &mut ProgressSinks<'_>) {
        if let Some(delta) = chunk.content_delta {
            let _ = sinks.content.write_all(delta.as_bytes());
            let _ = sinks.content.flush();
            self.content.push_str(&delta);
        }
        for tc in chunk.tool_call_deltas {
            self.absorb_tool_call(tc);
        }
        if let Some(reason) = chunk.finish_reason {
            self.finish_reason = Some(reason);
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage);
        }
    }

    fn absorb_tool_call(&mut self, delta: StreamToolCallDelta) {
        let slot = match self.tool_slots.iter_mut().find(|s| s.index == delta.index) {
            Some(s) => s,
            None => {
                self.tool_slots.push(ToolSlot {
                    index: delta.index,
                    ..Default::default()
                });
                self.tool_slots
                    .last_mut()
                    .expect("just pushed a slot, so last_mut must succeed")
            }
        };
        if let Some(id) = delta.id
            && slot.id.is_empty()
        {
            slot.id = id;
        }
        if let Some(name) = delta.function_name
            && slot.function_name.is_empty()
        {
            slot.function_name = name;
        }
        if let Some(fragment) = delta.arguments_fragment {
            slot.arguments_json.push_str(&fragment);
        }
    }

    fn finish(self) -> CallResult {
        let tool_calls = self
            .tool_slots
            .into_iter()
            .map(|s| ToolCall {
                id: s.id,
                function_name: s.function_name,
                arguments_json: s.arguments_json,
            })
            .collect();
        CallResult {
            content: self.content,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{chat_completions_url, extract_api_error_message, format_curl_failure};

    #[test]
    fn default_base_url_appends_chat_completions() {
        let url = chat_completions_url(None).expect("default base must succeed");
        assert_eq!(url, "https://api.deepseek.com/chat/completions");
    }

    #[test]
    fn custom_base_url_appends_chat_completions() {
        let url = chat_completions_url(Some("http://100.114.199.83:8888/v1"))
            .expect("custom base must succeed");
        assert_eq!(url, "http://100.114.199.83:8888/v1/chat/completions");
    }

    #[test]
    fn trailing_slash_on_base_is_stripped() {
        let url = chat_completions_url(Some("http://127.0.0.1:8888/v1/"))
            .expect("base with trailing slash must succeed");
        assert_eq!(url, "http://127.0.0.1:8888/v1/chat/completions");
    }

    #[test]
    fn empty_base_url_is_rejected() {
        let err = chat_completions_url(Some("")).expect_err("empty base must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("DEEPSEEK_BASE_URL"));
    }

    #[test]
    fn extracts_openai_style_error_message() {
        let body = r#"{"error":{"message":"The model `deepseek-v4-flash` does not exist.","type":"NotFoundError","code":404}}"#;
        assert_eq!(
            extract_api_error_message(body).as_deref(),
            Some("The model `deepseek-v4-flash` does not exist.")
        );
    }

    #[test]
    fn extract_api_error_message_ignores_non_error_json() {
        assert!(extract_api_error_message(r#"{"id":"x"}"#).is_none());
        assert!(extract_api_error_message("not json").is_none());
        assert!(extract_api_error_message("").is_none());
    }

    #[test]
    fn format_curl_failure_prefers_api_message() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(22 << 8);
        let body = br#"{"error":{"message":"The model `x` does not exist."}}"#;
        let msg = format_curl_failure(status, body, "The requested URL returned error: 404");
        assert_eq!(msg, "API request failed: The model `x` does not exist.");
    }
}
