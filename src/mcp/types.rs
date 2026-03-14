// MCP JSON-RPC 2.0 types for stdio transport.
//
// Covers the MCP protocol: initialize (with capability negotiation),
// tools/list, tools/call, resources/list, resources/read, prompts/list,
// prompts/get.

use std::string::FromUtf8Error;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// JSON-RPC base types

/// A JSON-RPC 2.0 request ID (integer or string).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

/// Incoming JSON-RPC request (method call or notification).
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<RequestId>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Outgoing JSON-RPC response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: Option<RequestId>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<RequestId>, code: i64, message: String) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// MCP protocol types

/// Initialize request params.
#[derive(Debug, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ClientCapabilitiesRaw,
    #[serde(rename = "clientInfo", default)]
    pub client_info: Option<ClientInfo>,
}

/// Raw client capabilities from the initialize request.
/// Each field indicates whether the client supports that MCP feature.
#[derive(Debug, Default, Deserialize)]
pub struct ClientCapabilitiesRaw {
    #[serde(default)]
    pub sampling: Option<Value>,
    #[serde(default)]
    pub roots: Option<Value>,
}

/// Parsed client capabilities stored by the server.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientCapabilities {
    /// Client supports sampling/createMessage (server -> client requests).
    pub sampling: bool,
    /// Client supports roots/list.
    pub roots: bool,
}

impl From<&ClientCapabilitiesRaw> for ClientCapabilities {
    fn from(raw: &ClientCapabilitiesRaw) -> Self {
        Self {
            sampling: raw.sampling.is_some(),
            roots: raw.roots.is_some(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Initialize response result.
#[derive(Debug, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'static str,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

/// Server capabilities declared to the client during initialization.
#[derive(Debug, Serialize)]
pub struct ServerCapabilities {
    pub tools: ToolCapability,
    pub resources: ResourceCapability,
    pub prompts: PromptCapability,
}

#[derive(Debug, Serialize)]
pub struct ToolCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Serialize)]
pub struct ResourceCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Serialize)]
pub struct PromptCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// A tool definition returned by tools/list.
#[derive(Debug, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

/// MCP tool annotations (hints for clients about tool behavior).
#[derive(Debug, Serialize)]
pub struct ToolAnnotations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<bool>,
    #[serde(rename = "readOnly", skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(rename = "openWorldHint", skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

/// Result of tools/list.
#[derive(Debug, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDef>,
}

/// Parameters for tools/call.
#[derive(Debug, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// A content block in a tool result.
#[derive(Debug, Serialize)]
pub struct Content {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// Result of tools/call.
#[derive(Debug, Serialize)]
pub struct CallToolResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl CallToolResult {
    pub fn text(text: String) -> Self {
        Self {
            content: vec![Content {
                content_type: "text".into(),
                text,
            }],
            is_error: None,
        }
    }

    pub fn error(message: String) -> Self {
        Self {
            content: vec![Content {
                content_type: "text".into(),
                text: message,
            }],
            is_error: Some(true),
        }
    }
}

// MCP Resources types

/// A resource definition returned by resources/list.
#[derive(Debug, Serialize)]
pub struct ResourceDef {
    pub uri: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

/// Result of resources/list.
#[derive(Debug, Serialize)]
pub struct ResourcesListResult {
    pub resources: Vec<ResourceDef>,
}

/// Parameters for resources/read.
#[derive(Debug, Deserialize)]
pub struct ResourceReadParams {
    pub uri: String,
}

/// A resource content item returned by resources/read.
#[derive(Debug, Serialize)]
pub struct ResourceContent {
    pub uri: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub text: String,
}

/// Result of resources/read.
#[derive(Debug, Serialize)]
pub struct ResourceReadResult {
    pub contents: Vec<ResourceContent>,
}

// MCP Prompts types

/// A prompt definition returned by prompts/list.
#[derive(Debug, Serialize)]
pub struct PromptDef {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<PromptArgDef>>,
}

/// An argument definition for a prompt.
#[derive(Debug, Serialize)]
pub struct PromptArgDef {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// Parameters for prompts/get.
#[derive(Debug, Deserialize)]
pub struct PromptGetParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

/// A message in a prompt result.
#[derive(Debug, Serialize)]
pub struct PromptMessage {
    pub role: String,
    pub content: PromptContent,
}

/// Content of a prompt message.
#[derive(Debug, Serialize)]
pub struct PromptContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// Result of prompts/get.
#[derive(Debug, Serialize)]
pub struct PromptGetResult {
    pub description: String,
    pub messages: Vec<PromptMessage>,
}

// Transport error types

/// Structured transport error for distinguishing failure modes in the
/// dispatch loop. Maps to specific JSON-RPC error codes:
///   - Closed → graceful exit (break loop)
///   - Parse → -32700 (PARSE_ERROR)
///   - InvalidUtf8 → -32700 (PARSE_ERROR)
///   - InvalidRequest → -32600 (INVALID_REQUEST)
///   - Io → -32000 (server error) with log
#[derive(Debug)]
pub enum TransportError {
    /// Underlying I/O failure (read/write error).
    Io(std::io::Error),
    /// Transport closed (EOF or channel dropped).
    Closed,
    /// Input is not valid JSON (malformed syntax).
    Parse(serde_json::Error),
    /// Input contains invalid UTF-8 sequences.
    InvalidUtf8(FromUtf8Error),
    /// Valid JSON but not a valid JSON-RPC request (missing method, wrong
    /// version, response-shaped message, etc.).
    InvalidRequest(String),
}

impl TransportError {
    /// Whether this error represents a closed/EOF condition.
    pub fn is_closed(&self) -> bool {
        matches!(self, TransportError::Closed)
    }

    /// JSON-RPC error code for this transport error, if applicable.
    /// Returns None for Closed (no error response should be sent).
    pub fn error_code(&self) -> Option<i64> {
        match self {
            TransportError::Io(_) => Some(-32000),
            TransportError::Closed => None,
            TransportError::Parse(_) => Some(PARSE_ERROR),
            TransportError::InvalidUtf8(_) => Some(PARSE_ERROR),
            TransportError::InvalidRequest(_) => Some(INVALID_REQUEST),
        }
    }

    /// Human-readable error message for JSON-RPC error responses.
    pub fn error_message(&self) -> String {
        match self {
            TransportError::Io(e) => format!("transport I/O error: {e}"),
            TransportError::Closed => "transport closed".into(),
            TransportError::Parse(e) => format!("parse error: {e}"),
            TransportError::InvalidUtf8(_) => {
                "request contains malformed UTF-8 character(s)".into()
            }
            TransportError::InvalidRequest(msg) => format!("invalid request: {msg}"),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "transport I/O: {e}"),
            TransportError::Closed => write!(f, "transport closed"),
            TransportError::Parse(e) => write!(f, "JSON parse: {e}"),
            TransportError::InvalidUtf8(e) => write!(f, "invalid UTF-8: {e}"),
            TransportError::InvalidRequest(msg) => write!(f, "invalid request: {msg}"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::Io(e) => Some(e),
            TransportError::Parse(e) => Some(e),
            TransportError::InvalidUtf8(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        TransportError::Io(e)
    }
}

impl From<serde_json::Error> for TransportError {
    fn from(e: serde_json::Error) -> Self {
        TransportError::Parse(e)
    }
}

impl From<FromUtf8Error> for TransportError {
    fn from(e: FromUtf8Error) -> Self {
        TransportError::InvalidUtf8(e)
    }
}

impl TransportError {
    /// Build a JSON-RPC error response for this transport error, if one
    /// should be sent. Returns None for Closed (no response).
    ///
    /// Accepts an optional request id so that InvalidRequest errors (where
    /// the JSON was valid enough to extract an id) can be correlated by
    /// the client.
    pub fn into_response(self, id: Option<RequestId>) -> Option<JsonRpcResponse> {
        let code = self.error_code()?;
        let message = self.error_message();
        Some(JsonRpcResponse::error(id, code, message))
    }
}

/// Parse a raw JSON line into a validated JsonRpcRequest.
///
/// Returns TransportError variants that preserve the distinction between
/// malformed JSON (Parse → -32700) and valid-JSON-but-invalid-JSON-RPC
/// (InvalidRequest → -32600).
pub fn parse_jsonrpc_line(line: &str) -> Result<JsonRpcRequest, TransportError> {
    // Step 1: parse as generic JSON.
    let obj: serde_json::Value = serde_json::from_str(line).map_err(TransportError::Parse)?;

    // Step 2: discard response-shaped messages (has result/error, no method).
    if obj.is_object() && obj.get("method").is_none() {
        return Err(TransportError::InvalidRequest(
            "response-shaped message (no method field)".into(),
        ));
    }

    // Step 3: convert to typed request.
    let req: JsonRpcRequest =
        serde_json::from_value(obj).map_err(|e| TransportError::InvalidRequest(e.to_string()))?;

    // Step 4: validate JSON-RPC version.
    if req.jsonrpc != JSONRPC_VERSION {
        return Err(TransportError::InvalidRequest(format!(
            "expected jsonrpc \"{JSONRPC_VERSION}\", got \"{}\"",
            req.jsonrpc
        )));
    }

    Ok(req)
}

// Protocol constants

pub const JSONRPC_VERSION: &str = "2.0";
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// Standard JSON-RPC error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const SERVER_NOT_INITIALIZED: i64 = -32002;
