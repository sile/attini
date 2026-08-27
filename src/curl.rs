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
        Some(s) if s.is_empty() => {
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
    pub reasoning_content: Option<String>,
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
            reasoning_content: self.reasoning_content,
            tool_calls: self.tool_calls,
        }
    }
}

pub struct ProgressSinks<'a> {
    pub content: &'a mut dyn Write,
    pub reasoning: Option<&'a mut dyn Write>,
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
    let assembly = decode_sse_stream(stdout, sinks)?;

    let mut stderr = child
        .stderr
        .take()
        .expect("stderr was piped when spawning curl");
    let mut stderr_buf = String::new();
    let _ = stderr.read_to_string(&mut stderr_buf);

    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "curl exited with {}: {}",
            status,
            stderr_buf.trim()
        )));
    }
    Ok(assembly)
}

fn decode_sse_stream<R: Read>(reader: R, sinks: &mut ProgressSinks<'_>) -> io::Result<CallResult> {
    let mut br = BufReader::new(reader);
    let mut decoder = SseDecoder::new();
    let mut assembly = Assembly::default();

    loop {
        let filled = br.fill_buf()?;
        if filled.is_empty() {
            break;
        }
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
                        StreamPayload::Done => return Ok(assembly.finish()),
                    }
                }
                SseEvent::Comment(_) => {}
            }
        }
    }
    Ok(assembly.finish())
}

#[derive(Default)]
struct Assembly {
    content: String,
    reasoning: String,
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
        if let Some(delta) = chunk.reasoning_delta {
            if let Some(sink) = sinks.reasoning.as_deref_mut() {
                let _ = sink.write_all(delta.as_bytes());
                let _ = sink.flush();
            }
            self.reasoning.push_str(&delta);
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
        let reasoning = if self.reasoning.is_empty() {
            None
        } else {
            Some(self.reasoning)
        };
        CallResult {
            content: self.content,
            reasoning_content: reasoning,
            tool_calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::chat_completions_url;

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
}
