//! DeepSeek Chat Completions request / response types.
//!
//! The Chat Completions API is compatible with the OpenAI schema. This
//! module models only the subset that the prototype currently needs:
//!
//! - Serialising a single streaming request with a `system` prompt and
//!   one or more chat messages.
//! - Decoding the streamed response chunks into content deltas,
//!   reasoning-content deltas (DeepSeek's thinking mode extension), and
//!   finish reasons.
//! - Recognising the `[DONE]` sentinel that terminates the stream.
//!
//! Tool calls, `usage` accounting, multi-choice responses, and every
//! other field are out of scope for the initial prototype and are
//! intentionally ignored on decode.
//!
//! Everything in this module is Sans I/O: types are constructed and
//! validated purely from strings.

use nojson::{DisplayJson, Json, JsonFormatter, JsonParseError, RawJsonValue};

/// The role attached to a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// A single message in a chat conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

impl DisplayJson for ChatMessage {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("role", self.role.as_str())?;
            f.member("content", &self.content)
        })
    }
}

/// A streaming Chat Completions request.
///
/// The wire representation always sets `"stream": true` because that is
/// the only mode the prototype uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
        }
    }

    /// Serialise the request as compact JSON suitable for the request
    /// body.
    pub fn to_json_string(&self) -> String {
        Json(self).to_string()
    }
}

impl DisplayJson for ChatRequest {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("model", &self.model)?;
            f.member("messages", &self.messages)?;
            f.member("stream", true)
        })
    }
}

/// A single decoded chunk from a streaming Chat Completions response.
///
/// The three fields are independent: any combination may be `Some` in
/// a given frame. In practice DeepSeek emits either a content delta or
/// a reasoning delta per chunk, and populates `finish_reason` on the
/// terminating chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamChunk {
    pub content_delta: Option<String>,
    pub reasoning_delta: Option<String>,
    pub finish_reason: Option<String>,
}

/// Payload of one SSE `data:` frame from the streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamPayload {
    /// A regular streaming chunk.
    Chunk(StreamChunk),
    /// The `[DONE]` sentinel that terminates the stream.
    Done,
}

/// Decode one SSE data payload into a [`StreamPayload`].
///
/// The value passed in is the accumulated `data:` field from a single
/// SSE event (see [`crate::sansio::sse`]), stripped of leading and
/// trailing whitespace before the `[DONE]` comparison so keep-alive
/// artifacts do not confuse the sentinel check.
pub fn decode_stream_payload(data: &str) -> Result<StreamPayload, JsonParseError> {
    if data.trim() == "[DONE]" {
        return Ok(StreamPayload::Done);
    }
    let parsed: Json<StreamChunk> = data.parse()?;
    Ok(StreamPayload::Chunk(parsed.0))
}

impl<'text, 'raw> TryFrom<RawJsonValue<'text, 'raw>> for StreamChunk {
    type Error = JsonParseError;

    fn try_from(value: RawJsonValue<'text, 'raw>) -> Result<Self, Self::Error> {
        let choice = match value.to_member("choices")?.optional() {
            Some(choices) => choices
                .to_array()?
                .next()
                .ok_or_else(|| value.invalid("choices array is empty"))?,
            None => return Ok(StreamChunk::default()),
        };
        let delta = choice.to_member("delta")?.optional();
        let (content_delta, reasoning_delta) = match delta {
            Some(delta) => (
                optional_string(delta, "content")?,
                optional_string(delta, "reasoning_content")?,
            ),
            None => (None, None),
        };
        let finish_reason = optional_string(choice, "finish_reason")?;
        Ok(StreamChunk {
            content_delta,
            reasoning_delta,
            finish_reason,
        })
    }
}

fn optional_string<'text, 'raw>(
    parent: RawJsonValue<'text, 'raw>,
    name: &str,
) -> Result<Option<String>, JsonParseError> {
    match parent.to_member(name)?.optional() {
        Some(v) => v.try_into(),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_serialises_model_messages_and_stream_flag() {
        let request = ChatRequest::new(
            "deepseek-v4-flash",
            vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are a helpful assistant.".to_string(),
                },
                ChatMessage {
                    role: Role::User,
                    content: "Hello".to_string(),
                },
            ],
        );
        assert_eq!(
            request.to_json_string(),
            r#"{"model":"deepseek-v4-flash","messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Hello"}],"stream":true}"#
        );
    }

    #[test]
    fn chat_request_escapes_special_characters() {
        let request = ChatRequest::new(
            "m",
            vec![ChatMessage {
                role: Role::User,
                content: "line1\nline2\t\"quoted\"".to_string(),
            }],
        );
        let json = request.to_json_string();
        assert!(json.contains(r#""content":"line1\nline2\t\"quoted\"""#));
    }

    #[test]
    fn decodes_content_delta_chunk() {
        let payload = r#"{"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                content_delta: Some("hi".to_string()),
                reasoning_delta: None,
                finish_reason: None,
            })
        );
    }

    #[test]
    fn decodes_reasoning_delta_chunk() {
        let payload = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"thinking..."},"finish_reason":null}]}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                content_delta: None,
                reasoning_delta: Some("thinking...".to_string()),
                finish_reason: None,
            })
        );
    }

    #[test]
    fn decodes_finish_reason_on_terminating_chunk() {
        let payload = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                content_delta: None,
                reasoning_delta: None,
                finish_reason: Some("stop".to_string()),
            })
        );
    }

    #[test]
    fn missing_delta_yields_empty_deltas() {
        let payload = r#"{"choices":[{"index":0,"finish_reason":null}]}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(got, StreamPayload::Chunk(StreamChunk::default()));
    }

    #[test]
    fn missing_choices_yields_empty_chunk() {
        let payload = r#"{"id":"c","object":"chat.completion.chunk"}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(got, StreamPayload::Chunk(StreamChunk::default()));
    }

    #[test]
    fn empty_choices_array_reports_error() {
        let payload = r#"{"choices":[]}"#;
        let err = decode_stream_payload(payload).expect_err("empty array is an error");
        let msg = err.to_string();
        assert!(msg.contains("choices"), "unexpected error: {msg}");
    }

    #[test]
    fn done_sentinel_is_recognised() {
        assert_eq!(
            decode_stream_payload("[DONE]").expect("decode"),
            StreamPayload::Done
        );
        assert_eq!(
            decode_stream_payload("  [DONE]\n").expect("decode with whitespace"),
            StreamPayload::Done
        );
    }

    #[test]
    fn ignores_unknown_top_level_fields() {
        let payload = r#"{"choices":[{"index":0,"delta":{"content":"x","tool_calls":[]},"finish_reason":null,"logprobs":null}],"usage":{"total_tokens":1}}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                content_delta: Some("x".to_string()),
                reasoning_delta: None,
                finish_reason: None,
            })
        );
    }

    #[test]
    fn malformed_json_returns_parse_error() {
        let err = decode_stream_payload("{not json").expect_err("malformed json");
        // Just verify we get a JsonParseError back rather than a panic; the
        // exact message is nojson's concern and may evolve.
        let _ = err.to_string();
    }
}
