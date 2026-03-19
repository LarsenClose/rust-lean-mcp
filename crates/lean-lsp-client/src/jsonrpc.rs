//! JSON-RPC 2.0 message types for the LSP protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC 2.0 request message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    /// Protocol version, always "2.0".
    pub jsonrpc: String,
    /// Unique identifier for the request.
    pub id: i64,
    /// The method to invoke.
    pub method: String,
    /// Optional parameters for the method.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    /// Protocol version, always "2.0".
    pub jsonrpc: String,
    /// The id of the request this response corresponds to.
    pub id: Option<i64>,
    /// The result of a successful request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error if the request failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseError {
    /// A number indicating the error type.
    pub code: i64,
    /// A short description of the error.
    pub message: String,
    /// Optional additional data about the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 notification message (no id, no response expected).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    /// Protocol version, always "2.0".
    pub jsonrpc: String,
    /// The method being notified.
    pub method: String,
    /// Optional parameters for the notification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Create a new JSON-RPC request.
    pub fn new(id: i64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }
}

impl Notification {
    /// Create a new JSON-RPC notification.
    pub fn new(method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        }
    }
}

/// A parsed JSON-RPC message, which can be a request, response, or notification.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A request from client to server (has id and method).
    Request(Request),
    /// A response from server to client (has id, result or error).
    Response(Response),
    /// A notification (has method but no id).
    Notification(Notification),
}

impl Message {
    /// Parse a JSON value into a typed JSON-RPC message.
    ///
    /// Dispatch logic:
    /// - Has "method" and "id" -> Request
    /// - Has "method" but no "id" -> Notification
    /// - Has "id" but no "method" -> Response
    pub fn from_value(value: Value) -> Result<Self, serde_json::Error> {
        let has_method = value.get("method").is_some();
        let has_id = value.get("id").is_some();

        if has_method && has_id {
            let request: Request = serde_json::from_value(value)?;
            Ok(Message::Request(request))
        } else if has_method {
            let notification: Notification = serde_json::from_value(value)?;
            Ok(Message::Notification(notification))
        } else {
            let response: Response = serde_json::from_value(value)?;
            Ok(Message::Response(response))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn test_request_new() {
        let req = Request::new(1, "initialize", Some(json!({"rootUri": "file:///tmp"})));
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, 1);
        assert_eq!(req.method, "initialize");
        assert_eq!(req.params, Some(json!({"rootUri": "file:///tmp"})));
    }

    #[test]
    fn test_request_new_without_params() {
        let req = Request::new(42, "shutdown", None);
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, 42);
        assert_eq!(req.method, "shutdown");
        assert_eq!(req.params, None);
    }

    #[test]
    fn test_request_serialization() {
        let req = Request::new(
            1,
            "textDocument/hover",
            Some(json!({"position": {"line": 0, "character": 5}})),
        );
        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["jsonrpc"], "2.0");
        assert_eq!(serialized["id"], 1);
        assert_eq!(serialized["method"], "textDocument/hover");
        assert!(serialized.get("params").is_some());
    }

    #[test]
    fn test_request_serialization_omits_none_params() {
        let req = Request::new(1, "shutdown", None);
        let serialized = serde_json::to_string(&req).unwrap();
        assert!(!serialized.contains("params"));
    }

    #[test]
    fn test_request_deserialization() {
        let json_str =
            r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"rootUri":"file:///tmp"}}"#;
        let req: Request = serde_json::from_str(json_str).unwrap();
        assert_eq!(req.id, 3);
        assert_eq!(req.method, "initialize");
        assert_eq!(req.params, Some(json!({"rootUri": "file:///tmp"})));
    }

    #[test]
    fn test_request_roundtrip() {
        let original = Request::new(7, "textDocument/completion", Some(json!({"line": 10})));
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: Request = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_response_with_result() {
        let resp = Response {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            result: Some(json!({"capabilities": {}})),
            error: None,
        };
        let serialized = serde_json::to_value(&resp).unwrap();
        assert_eq!(serialized["id"], 1);
        assert!(serialized.get("result").is_some());
        assert!(serialized.get("error").is_none());
    }

    #[test]
    fn test_response_with_error() {
        let resp = Response {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            result: None,
            error: Some(ResponseError {
                code: -32601,
                message: "Method not found".to_string(),
                data: None,
            }),
        };
        let serialized = serde_json::to_value(&resp).unwrap();
        assert_eq!(serialized["error"]["code"], -32601);
        assert_eq!(serialized["error"]["message"], "Method not found");
        assert!(serialized.get("result").is_none());
    }

    #[test]
    fn test_response_error_with_data() {
        let err = ResponseError {
            code: -32600,
            message: "Invalid request".to_string(),
            data: Some(json!({"details": "missing field"})),
        };
        let serialized = serde_json::to_value(&err).unwrap();
        assert_eq!(serialized["data"]["details"], "missing field");
    }

    #[test]
    fn test_response_deserialization() {
        let json_str =
            r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"hoverProvider":true}}}"#;
        let resp: Response = serde_json::from_str(json_str).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_notification_new() {
        let notif = Notification::new("initialized", Some(json!({})));
        assert_eq!(notif.jsonrpc, "2.0");
        assert_eq!(notif.method, "initialized");
        assert_eq!(notif.params, Some(json!({})));
    }

    #[test]
    fn test_notification_serialization_omits_none_params() {
        let notif = Notification::new("exit", None);
        let serialized = serde_json::to_string(&notif).unwrap();
        assert!(!serialized.contains("params"));
    }

    #[test]
    fn test_notification_roundtrip() {
        let original = Notification::new(
            "textDocument/didOpen",
            Some(json!({"uri": "file:///a.lean"})),
        );
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: Notification = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_message_from_value_request() {
        let value = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let msg = Message::from_value(value).unwrap();
        assert!(matches!(msg, Message::Request(_)));
        if let Message::Request(req) = msg {
            assert_eq!(req.id, 1);
            assert_eq!(req.method, "initialize");
        }
    }

    #[test]
    fn test_message_from_value_response() {
        let value = json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}});
        let msg = Message::from_value(value).unwrap();
        assert!(matches!(msg, Message::Response(_)));
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, Some(1));
        }
    }

    #[test]
    fn test_message_from_value_notification() {
        let value =
            json!({"jsonrpc":"2.0","method":"window/logMessage","params":{"message":"hello"}});
        let msg = Message::from_value(value).unwrap();
        assert!(matches!(msg, Message::Notification(_)));
        if let Message::Notification(notif) = msg {
            assert_eq!(notif.method, "window/logMessage");
        }
    }

    #[test]
    fn test_message_from_value_error_response() {
        let value =
            json!({"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"Method not found"}});
        let msg = Message::from_value(value).unwrap();
        assert!(matches!(msg, Message::Response(_)));
        if let Message::Response(resp) = msg {
            assert_eq!(resp.id, Some(5));
            assert!(resp.error.is_some());
            assert_eq!(resp.error.unwrap().code, -32601);
        }
    }
}
