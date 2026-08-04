//! JSON-RPC 2.0 envelope types for the line-delimited transport.
//!
//! Outgoing (server → client) types (`SuccessResponse`,
//! `ErrorResponse`, `Notification`) implement `DisplayJson`;
//! [`server::write_line`] writes them followed by a newline.
//!
//! Incoming (client → server) is one line per message. The parser
//! ([`parse_incoming`]) classifies a line into
//! [`Incoming::Request`] or [`Incoming::Notification`] and hands
//! back the `params` object as a JSON substring so per-method
//! handlers can decode with a method-specific type.
//!
//! `id` is limited to non-negative integers in this prototype
//! (the JSON-RPC 2.0 spec allows string / number / null; string
//! and null are rejected as `Invalid Request`).
//!
//! [`server::write_line`]: crate::rpc::server

use nojson::{DisplayJson, JsonFormatter, JsonParseError, RawJson};

/// Standard JSON-RPC 2.0 error codes (subset used by this crate).
pub const CODE_PARSE_ERROR: i64 = -32700;
pub const CODE_INVALID_REQUEST: i64 = -32600;
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
pub const CODE_INVALID_PARAMS: i64 = -32602;
pub const CODE_INTERNAL_ERROR: i64 = -32603;

/// Parsed incoming message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// Client expects a matching response.
    Request {
        id: u64,
        method: String,
        params_json: String,
    },
    /// Fire-and-forget from the client.
    Notification { method: String, params_json: String },
}

/// Parse one line from the client into an [`Incoming`].
///
/// Returns `Err` when the line is not valid JSON, is missing
/// required fields, or uses an `id` shape this prototype does not
/// support. Callers should send back an
/// [`ErrorResponse::parse_error`] or
/// [`ErrorResponse::invalid_request`] with `id: None` in that
/// case, and continue reading the next line.
pub fn parse_incoming(line: &str) -> Result<Incoming, JsonParseError> {
    let json: RawJson = RawJson::parse(line)?;
    let value = json.value();

    let jsonrpc = value
        .to_member("jsonrpc")?
        .required()?
        .to_unquoted_string_str()?;
    if jsonrpc.as_ref() != "2.0" {
        return Err(value.invalid(format!("expected jsonrpc = \"2.0\", got {jsonrpc:?}")));
    }

    let method_value = value.to_member("method")?.required()?;
    let method = method_value.to_unquoted_string_str()?.into_owned();

    let params_json = match value.to_member("params")?.optional() {
        Some(params) => params.as_raw_str().to_string(),
        None => "null".to_string(),
    };

    match value.to_member("id")?.optional() {
        Some(id_value) => {
            let id: u64 = id_value
                .try_into()
                .map_err(|_e| id_value.invalid("id must be a u64"))?;
            Ok(Incoming::Request {
                id,
                method,
                params_json,
            })
        }
        None => Ok(Incoming::Notification {
            method,
            params_json,
        }),
    }
}

/// Success response envelope. `result` must implement
/// [`DisplayJson`] so callers can supply any per-method result
/// type without materialising an intermediate JSON string.
pub struct SuccessResponse<R: DisplayJson> {
    pub id: u64,
    pub result: R,
}

impl<R: DisplayJson> DisplayJson for SuccessResponse<R> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("jsonrpc", "2.0")?;
            f.member("id", self.id)?;
            f.member("result", &self.result)
        })
    }
}

/// Error response envelope. `id` is `None` when the incoming
/// message could not be parsed far enough to know its id.
pub struct ErrorResponse {
    pub id: Option<u64>,
    pub error: RpcError,
}

impl ErrorResponse {
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self {
            id: None,
            error: RpcError {
                code: CODE_PARSE_ERROR,
                message: message.into(),
            },
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            id: None,
            error: RpcError {
                code: CODE_INVALID_REQUEST,
                message: message.into(),
            },
        }
    }

    pub fn method_not_found(id: u64, method: &str) -> Self {
        Self {
            id: Some(id),
            error: RpcError {
                code: CODE_METHOD_NOT_FOUND,
                message: format!("method not found: {method}"),
            },
        }
    }

    pub fn invalid_params(id: u64, message: impl Into<String>) -> Self {
        Self {
            id: Some(id),
            error: RpcError {
                code: CODE_INVALID_PARAMS,
                message: message.into(),
            },
        }
    }

    pub fn internal_error(id: u64, message: impl Into<String>) -> Self {
        Self {
            id: Some(id),
            error: RpcError {
                code: CODE_INTERNAL_ERROR,
                message: message.into(),
            },
        }
    }
}

impl DisplayJson for ErrorResponse {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("jsonrpc", "2.0")?;
            f.member("id", self.id)?;
            f.member("error", &self.error)
        })
    }
}

/// Error object body. `data` is intentionally omitted (not
/// used by any server-side error at this time).
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl DisplayJson for RpcError {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("code", self.code)?;
            f.member("message", &self.message)
        })
    }
}

/// Server → client notification envelope. `params` implements
/// [`DisplayJson`] so per-event payload types are inlined
/// directly without extra JSON round-tripping.
pub struct Notification<P: DisplayJson> {
    pub method: String,
    pub params: P,
}

impl<P: DisplayJson> DisplayJson for Notification<P> {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
        f.object(|f| {
            f.member("jsonrpc", "2.0")?;
            f.member("method", &self.method)?;
            f.member("params", &self.params)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nojson::Json;

    #[test]
    fn parse_request_with_object_params() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"submit_prompt","params":{"text":"hi"}}"#;
        let msg = parse_incoming(line).expect("parse");
        assert_eq!(
            msg,
            Incoming::Request {
                id: 1,
                method: "submit_prompt".into(),
                params_json: r#"{"text":"hi"}"#.into(),
            }
        );
    }

    #[test]
    fn parse_request_without_params_uses_null() {
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"cancel"}"#;
        let msg = parse_incoming(line).expect("parse");
        assert_eq!(
            msg,
            Incoming::Request {
                id: 2,
                method: "cancel".into(),
                params_json: "null".into(),
            }
        );
    }

    #[test]
    fn parse_notification_when_id_missing() {
        let line = r#"{"jsonrpc":"2.0","method":"event/finish","params":{}}"#;
        let msg = parse_incoming(line).expect("parse");
        assert_eq!(
            msg,
            Incoming::Notification {
                method: "event/finish".into(),
                params_json: "{}".into(),
            }
        );
    }

    #[test]
    fn parse_rejects_wrong_jsonrpc_version() {
        let line = r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#;
        assert!(parse_incoming(line).is_err());
    }

    #[test]
    fn parse_rejects_string_id() {
        let line = r#"{"jsonrpc":"2.0","id":"abc","method":"x"}"#;
        assert!(parse_incoming(line).is_err());
    }

    #[test]
    fn success_response_serialises_result() {
        struct R;
        impl DisplayJson for R {
            fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
                f.object(|f| f.member("accepted", true))
            }
        }
        let resp = SuccessResponse { id: 3, result: R };
        let s = Json(&resp).to_string();
        assert_eq!(s, r#"{"jsonrpc":"2.0","id":3,"result":{"accepted":true}}"#);
    }

    #[test]
    fn error_response_serialises_with_null_id_when_none() {
        let resp = ErrorResponse::parse_error("bad line");
        let s = Json(&resp).to_string();
        assert_eq!(
            s,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"bad line"}}"#
        );
    }

    #[test]
    fn notification_serialises_with_no_id() {
        struct P;
        impl DisplayJson for P {
            fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> std::fmt::Result {
                f.object(|f| f.member("kind", "cancel"))
            }
        }
        let notif = Notification {
            method: "event/cancel".to_string(),
            params: P,
        };
        let s = Json(&notif).to_string();
        assert_eq!(
            s,
            r#"{"jsonrpc":"2.0","method":"event/cancel","params":{"kind":"cancel"}}"#
        );
    }
}
