//! DeepSeek Chat Completions request / response types.
//!
//! The Chat Completions API is compatible with the OpenAI schema. This
//! module models the subset that the prototype currently needs:
//!
//! - Serialising a streaming request with an optional `system` prompt,
//!   one or more chat messages, and optional tool definitions
//! - Decoding the streamed response chunks into content deltas,
//!   reasoning-content deltas (DeepSeek's thinking mode extension),
//!   tool-call deltas, and finish reasons
//! - Recognising the `[DONE]` sentinel that terminates the stream
//!
//! Everything in this module is Sans I/O: types are constructed and
//! validated purely from strings.

use nojson::{DisplayJson, Json, JsonFormatter, JsonParseError, RawJsonValue};

/// A single message in a chat conversation.
///
/// Modelled as an enum per role because OpenAI-compatible messages
/// have very different shapes: user / system carry plain text,
/// assistant may carry text plus tool calls plus thinking-mode
/// reasoning, and tool result messages carry the id of the call they
/// answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMessage {
    System(String),
    User(String),
    Assistant {
        content: String,
        reasoning_content: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl ChatMessage {
    /// Convenience constructor for a plain assistant text turn.
    pub fn assistant_text(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: content.into(),
            reasoning_content: None,
            tool_calls: Vec::new(),
        }
    }
}

/// A single tool invocation requested by the model.
///
/// Constructed while streaming from `choices[0].delta.tool_calls[]`
/// fragments (see [`StreamToolCallDelta`]) and finalised on the
/// terminating chunk when `finish_reason == "tool_calls"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments_json: String,
}

impl DisplayJson for ChatMessage {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        match self {
            ChatMessage::System(content) => f.object(|f| {
                f.member("role", "system")?;
                f.member("content", content)
            }),
            ChatMessage::User(content) => f.object(|f| {
                f.member("role", "user")?;
                f.member("content", content)
            }),
            ChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => f.object(|f| {
                f.member("role", "assistant")?;
                if tool_calls.is_empty() {
                    f.member("content", content)?;
                } else {
                    // OpenAI schema: content may be null when the turn
                    // consists only of tool_calls.
                    if content.is_empty() {
                        f.member("content", Option::<&str>::None)?;
                    } else {
                        f.member("content", content)?;
                    }
                    f.member("tool_calls", tool_calls)?;
                }
                if let Some(reasoning) = reasoning_content {
                    f.member("reasoning_content", reasoning)?;
                }
                Ok(())
            }),
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => f.object(|f| {
                f.member("role", "tool")?;
                f.member("tool_call_id", tool_call_id)?;
                f.member("content", content)
            }),
        }
    }
}

impl DisplayJson for ToolCall {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        // OpenAI wire format uses `{"id": ..., "type": "function",
        // "function": {"name": ..., "arguments": <json string>}}`.
        f.object(|f| {
            f.member("id", &self.id)?;
            f.member("type", "function")?;
            f.member(
                "function",
                &FunctionOnWire {
                    name: &self.function_name,
                    arguments: &self.arguments_json,
                },
            )
        })
    }
}

struct FunctionOnWire<'a> {
    name: &'a str,
    arguments: &'a str,
}

impl DisplayJson for FunctionOnWire<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("name", self.name)?;
            f.member("arguments", self.arguments)
        })
    }
}

/// A tool definition sent to the model as part of a [`ChatRequest`].
///
/// `parameters_json` is a JSON schema string (typically an object with
/// `type: "object"` and a `properties` map) that describes the
/// arguments the model is allowed to synthesise for the function call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters_json: String,
}

impl DisplayJson for ToolDef {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("type", "function")?;
            f.member(
                "function",
                &ToolDefFunction {
                    name: &self.name,
                    description: &self.description,
                    parameters_json: &self.parameters_json,
                },
            )
        })
    }
}

struct ToolDefFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters_json: &'a str,
}

impl DisplayJson for ToolDefFunction<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        // `parameters_json` is already-serialised JSON; wrap it in
        // `RawJsonSlice` so nojson emits it verbatim instead of
        // re-quoting it as a string.
        f.object(|f| {
            f.member("name", self.name)?;
            f.member("description", self.description)?;
            f.member("parameters", RawJsonSlice(self.parameters_json))
        })
    }
}

struct RawJsonSlice<'a>(&'a str);

impl DisplayJson for RawJsonSlice<'_> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.inner_mut().write_str(self.0)
    }
}

/// A streaming Chat Completions request.
///
/// The wire representation always sets `"stream": true` because that
/// is the only mode the prototype uses. `tools` is omitted from the
/// wire body when empty so requests without tool support look
/// identical to a plain messages-only request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = tools;
        self
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
            f.member("stream", true)?;
            f.member("stream_options", &StreamOptions)?;
            if !self.tools.is_empty() {
                f.member("tools", &self.tools)?;
            }
            Ok(())
        })
    }
}

struct StreamOptions;

impl DisplayJson for StreamOptions {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| f.member("include_usage", true))
    }
}

/// A fragment of a tool call arriving in one streaming chunk.
///
/// The model sends the id, function name, and arguments in pieces
/// across chunks. `index` is the identity used to reassemble them:
/// same `index` = same tool call. See [`StreamChunk::tool_call_deltas`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamToolCallDelta {
    pub index: u64,
    pub id: Option<String>,
    pub function_name: Option<String>,
    pub arguments_fragment: Option<String>,
}

/// A single decoded chunk from a streaming Chat Completions response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamChunk {
    pub content_delta: Option<String>,
    pub reasoning_delta: Option<String>,
    pub tool_call_deltas: Vec<StreamToolCallDelta>,
    pub finish_reason: Option<String>,
    /// Token usage counters. OpenAI-compatible APIs return these only
    /// on the final `usage`-only chunk (typically with an empty
    /// `choices` array). Requested by setting `stream_options.include_usage`.
    pub usage: Option<Usage>,
}

/// Token usage counters returned by an OpenAI-compatible chat
/// completions response.
///
/// All fields are optional because different models and different
/// modes populate different subsets. `prompt_tokens` is what
/// compaction triggers on; the DeepSeek-specific cache hit / miss
/// breakdown is preserved for observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub prompt_cache_hit_tokens: Option<u64>,
    pub prompt_cache_miss_tokens: Option<u64>,
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
        let usage = decode_optional_usage(value)?;
        let choices = value.to_member("choices")?.optional();
        let choice = match choices {
            Some(choices) => choices.to_array()?.next(),
            None => None,
        };
        // `choices: []` plus a top-level `usage` is how OpenAI-compatible
        // APIs deliver the terminating usage chunk when `include_usage`
        // is enabled. Treat the absence of a choice as a usage-only
        // chunk instead of a parse error.
        let Some(choice) = choice else {
            return Ok(StreamChunk {
                usage,
                ..Default::default()
            });
        };
        let delta = choice.to_member("delta")?.optional();
        let (content_delta, reasoning_delta, tool_call_deltas) = match delta {
            Some(delta) => (
                optional_string(delta, "content")?,
                optional_string(delta, "reasoning_content")?,
                decode_tool_call_deltas(delta)?,
            ),
            None => (None, None, Vec::new()),
        };
        let finish_reason = optional_string(choice, "finish_reason")?;
        Ok(StreamChunk {
            content_delta,
            reasoning_delta,
            tool_call_deltas,
            finish_reason,
            usage,
        })
    }
}

fn decode_optional_usage(value: RawJsonValue<'_, '_>) -> Result<Option<Usage>, JsonParseError> {
    let Some(usage_value) = value.to_member("usage")?.optional() else {
        return Ok(None);
    };
    if usage_value.as_raw_str().trim() == "null" {
        return Ok(None);
    }
    Ok(Some(Usage {
        prompt_tokens: optional_u64(usage_value, "prompt_tokens")?,
        completion_tokens: optional_u64(usage_value, "completion_tokens")?,
        total_tokens: optional_u64(usage_value, "total_tokens")?,
        prompt_cache_hit_tokens: optional_u64(usage_value, "prompt_cache_hit_tokens")?,
        prompt_cache_miss_tokens: optional_u64(usage_value, "prompt_cache_miss_tokens")?,
    }))
}

fn optional_u64(parent: RawJsonValue<'_, '_>, name: &str) -> Result<Option<u64>, JsonParseError> {
    match parent.to_member(name)?.optional() {
        Some(v) if v.as_raw_str().trim() == "null" => Ok(None),
        Some(v) => v.try_into().map(Some),
        None => Ok(None),
    }
}

fn decode_tool_call_deltas<'text, 'raw>(
    delta: RawJsonValue<'text, 'raw>,
) -> Result<Vec<StreamToolCallDelta>, JsonParseError> {
    let tool_calls = match delta.to_member("tool_calls")?.optional() {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    let mut deltas = Vec::new();
    for item in tool_calls.to_array()? {
        let index: u64 = item
            .to_member("index")?
            .required()?
            .try_into()
            .map_err(|_e| item.invalid("tool_calls[].index must be a u64"))?;
        let id = optional_string(item, "id")?;
        let function = item.to_member("function")?.optional();
        let (function_name, arguments_fragment) = match function {
            Some(function) => (
                optional_string(function, "name")?,
                optional_string(function, "arguments")?,
            ),
            None => (None, None),
        };
        deltas.push(StreamToolCallDelta {
            index,
            id,
            function_name,
            arguments_fragment,
        });
    }
    Ok(deltas)
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
                ChatMessage::System("You are a helpful assistant.".to_string()),
                ChatMessage::User("Hello".to_string()),
            ],
        );
        assert_eq!(
            request.to_json_string(),
            r#"{"model":"deepseek-v4-flash","messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Hello"}],"stream":true,"stream_options":{"include_usage":true}}"#
        );
    }

    #[test]
    fn chat_request_escapes_special_characters() {
        let request = ChatRequest::new(
            "m",
            vec![ChatMessage::User("line1\nline2\t\"quoted\"".to_string())],
        );
        let json = request.to_json_string();
        assert!(json.contains(r#""content":"line1\nline2\t\"quoted\"""#));
    }

    #[test]
    fn chat_request_omits_tools_when_empty() {
        let request = ChatRequest::new("m", vec![ChatMessage::User("hi".to_string())]);
        let json = request.to_json_string();
        assert!(!json.contains("\"tools\""), "unexpected tools: {json}");
    }

    #[test]
    fn chat_request_serialises_tools_when_present() {
        let request =
            ChatRequest::new("m", vec![ChatMessage::User("hi".to_string())]).with_tools(vec![
                ToolDef {
                    name: "list".to_string(),
                    description: "List entries".to_string(),
                    parameters_json: r#"{"type":"object","properties":{}}"#.to_string(),
                },
            ]);
        let json = request.to_json_string();
        assert!(json.contains(r#""tools":[{"type":"function","function":{"name":"list","description":"List entries","parameters":{"type":"object","properties":{}}}}]"#), "unexpected: {json}");
    }

    #[test]
    fn assistant_message_without_tool_calls_serialises_content() {
        let msg = ChatMessage::assistant_text("hello");
        let json = Json(&msg).to_string();
        assert_eq!(json, r#"{"role":"assistant","content":"hello"}"#);
    }

    #[test]
    fn assistant_message_with_tool_calls_uses_null_content_when_empty() {
        let msg = ChatMessage::Assistant {
            content: String::new(),
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                function_name: "list".to_string(),
                arguments_json: r#"{"path":"."}"#.to_string(),
            }],
        };
        let json = Json(&msg).to_string();
        assert_eq!(
            json,
            r#"{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"list","arguments":"{\"path\":\".\"}"}}]}"#
        );
    }

    #[test]
    fn assistant_message_with_reasoning_serialises_field() {
        let msg = ChatMessage::Assistant {
            content: "answer".to_string(),
            reasoning_content: Some("thinking...".to_string()),
            tool_calls: Vec::new(),
        };
        let json = Json(&msg).to_string();
        assert_eq!(
            json,
            r#"{"role":"assistant","content":"answer","reasoning_content":"thinking..."}"#
        );
    }

    #[test]
    fn tool_message_serialises_role_and_call_id() {
        let msg = ChatMessage::Tool {
            tool_call_id: "call_1".to_string(),
            content: "[]".to_string(),
        };
        let json = Json(&msg).to_string();
        assert_eq!(
            json,
            r#"{"role":"tool","tool_call_id":"call_1","content":"[]"}"#
        );
    }

    #[test]
    fn decodes_content_delta_chunk() {
        let payload = r#"{"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                content_delta: Some("hi".to_string()),
                ..Default::default()
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
                reasoning_delta: Some("thinking...".to_string()),
                ..Default::default()
            })
        );
    }

    #[test]
    fn decodes_tool_call_deltas() {
        let payload = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"list","arguments":"{\"pa"}},{"index":1,"function":{"arguments":"th"}}]},"finish_reason":null}]}"#;
        let got = decode_stream_payload(payload).expect("decode");
        let chunk = match got {
            StreamPayload::Chunk(c) => c,
            _ => panic!("expected chunk"),
        };
        assert_eq!(chunk.tool_call_deltas.len(), 2);
        assert_eq!(chunk.tool_call_deltas[0].index, 0);
        assert_eq!(chunk.tool_call_deltas[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            chunk.tool_call_deltas[0].function_name.as_deref(),
            Some("list")
        );
        assert_eq!(
            chunk.tool_call_deltas[0].arguments_fragment.as_deref(),
            Some(r#"{"pa"#)
        );
        assert_eq!(chunk.tool_call_deltas[1].index, 1);
        assert!(chunk.tool_call_deltas[1].id.is_none());
        assert!(chunk.tool_call_deltas[1].function_name.is_none());
        assert_eq!(
            chunk.tool_call_deltas[1].arguments_fragment.as_deref(),
            Some("th")
        );
    }

    #[test]
    fn decodes_finish_reason_on_terminating_chunk() {
        let payload = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                finish_reason: Some("stop".to_string()),
                ..Default::default()
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
    fn empty_choices_array_with_usage_yields_usage_only_chunk() {
        // OpenAI-compatible APIs deliver the terminating usage payload
        // as a chunk with an empty `choices` array plus a top-level
        // `usage` object. Historically we rejected this shape as
        // malformed; enabling `stream_options.include_usage` now makes
        // it the normal way to receive token counters.
        let payload = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13,"prompt_cache_hit_tokens":8,"prompt_cache_miss_tokens":2}}"#;
        let got = decode_stream_payload(payload).expect("usage-only chunk parses");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                usage: Some(Usage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(3),
                    total_tokens: Some(13),
                    prompt_cache_hit_tokens: Some(8),
                    prompt_cache_miss_tokens: Some(2),
                }),
                ..Default::default()
            })
        );
    }

    #[test]
    fn empty_choices_array_without_usage_yields_empty_chunk() {
        let payload = r#"{"choices":[]}"#;
        let got = decode_stream_payload(payload).expect("empty chunk parses");
        assert_eq!(got, StreamPayload::Chunk(StreamChunk::default()));
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
    fn empty_tool_calls_array_yields_no_deltas() {
        // Previously covered by `ignores_unknown_top_level_fields`; now
        // we explicitly assert that an empty tool_calls array parses
        // into an empty `tool_call_deltas` vector so callers can rely
        // on the absence of any delta. The top-level `usage` in this
        // payload is also decoded when present.
        let payload = r#"{"choices":[{"index":0,"delta":{"content":"x","tool_calls":[]},"finish_reason":null,"logprobs":null}],"usage":{"total_tokens":1}}"#;
        let got = decode_stream_payload(payload).expect("decode");
        assert_eq!(
            got,
            StreamPayload::Chunk(StreamChunk {
                content_delta: Some("x".to_string()),
                usage: Some(Usage {
                    total_tokens: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
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
