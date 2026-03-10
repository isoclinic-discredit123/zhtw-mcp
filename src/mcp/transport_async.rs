// Async stdio transport for MCP JSON-RPC 2.0 (feature: async-transport).
//
// Replaces the thread+mpsc synchronous transport with a tokio-based event
// loop. The Server struct remains synchronous (Aho-Corasick is CPU-bound);
// async wraps transport I/O only.
//
// Sampling support uses tokio::time::timeout instead of mpsc::recv_timeout.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::tools::Server;
use super::types::{
    JsonRpcRequest, JsonRpcResponse, INVALID_REQUEST, JSONRPC_VERSION, PARSE_ERROR,
};

/// Maximum line length (4 MiB), matching the sync transport.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Run the MCP server on async stdin/stdout until EOF.
///
/// Uses a single-threaded tokio runtime. The Server is borrowed mutably
/// and all tool calls block the event loop (CPU-bound scan is fast enough
/// that this is acceptable for stdio; true concurrency requires
/// RwLock<Scanner> which is deferred).
pub fn run_async_stdio(server: &mut Server) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_stdio_loop(server))
}

async fn async_stdio_loop(server: &mut Server) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut raw_buf: Vec<u8> = Vec::new();

    loop {
        raw_buf.clear();
        // Bounded read: read raw bytes up to the limit to avoid InvalidData
        // errors when the cap splits a multi-byte UTF-8 codepoint.
        let n = {
            let limited = (&mut reader).take((MAX_LINE_BYTES + 1) as u64);
            tokio::pin!(limited);
            limited.read_until(b'\n', &mut raw_buf).await?
        };
        if n == 0 {
            break; // EOF
        }
        if raw_buf.len() > MAX_LINE_BYTES {
            // Drain the remainder of the oversized line so the next
            // iteration starts at a fresh line boundary.
            if raw_buf.last() != Some(&b'\n') {
                let _ = reader.read_until(b'\n', &mut raw_buf).await;
            }
            let resp = JsonRpcResponse::error(None, INVALID_REQUEST, "request too large".into());
            send_async(&mut stdout, &resp).await?;
            continue;
        }
        let Ok(line) = str::from_utf8(&raw_buf).map(str::trim) else {
            let resp = JsonRpcResponse::error(
                None,
                PARSE_ERROR,
                "request contains malformed UTF-8 character(s)".into(),
            );
            send_async(&mut stdout, &resp).await?;
            continue;
        };

        if line.is_empty() {
            continue;
        }

        // Parse once; reuse for response-shape check and typed conversion.
        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("invalid JSON-RPC: {e}");
                let resp = JsonRpcResponse::error(None, PARSE_ERROR, format!("parse error: {e}"));
                send_async(&mut stdout, &resp).await?;
                continue;
            }
        };

        // Discard response-shaped messages (stale sampling responses).
        if obj.is_object() && obj.get("method").is_none() {
            log::debug!("discarding response-shaped message");
            continue;
        }

        let mut req: JsonRpcRequest = match serde_json::from_value(obj) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("invalid JSON-RPC request: {e}");
                let resp =
                    JsonRpcResponse::error(None, INVALID_REQUEST, format!("invalid request: {e}"));
                send_async(&mut stdout, &resp).await?;
                continue;
            }
        };

        if req.jsonrpc != JSONRPC_VERSION {
            let resp = JsonRpcResponse::error(
                req.id.clone(),
                INVALID_REQUEST,
                format!(
                    "expected jsonrpc \"{JSONRPC_VERSION}\", got \"{}\"",
                    req.jsonrpc
                ),
            );
            send_async(&mut stdout, &resp).await?;
            continue;
        }

        // Dispatch synchronously. For tools/call with sampling, we fall back
        // to the synchronous SamplingBridge since the Server is not async.
        // The sampling bridge needs synchronous stdin access, so we use a
        // simple non-sampling dispatch for the async path.
        let resp = dispatch_async(server, &mut req);
        if let Some(resp) = resp {
            send_async(&mut stdout, &resp).await?;
        }
    }

    Ok(())
}

/// Dispatch a request. Sampling is not supported in the async transport
/// (sampling requires synchronous channel access; a full async sampling
/// bridge is a future enhancement).
fn dispatch_async(server: &mut Server, req: &mut JsonRpcRequest) -> Option<JsonRpcResponse> {
    // Pre-init routing (initialize, ping, notifications, rejection).
    if let Some(resp) = server.dispatch_preinit(req) {
        return resp;
    }

    // Async path: tools/call without sampling bridge, all others shared.
    if req.method == "tools/call" {
        Some(server.handle_tools_call(req, None))
    } else {
        server.dispatch_method(req)
    }
}

async fn send_async(writer: &mut tokio::io::Stdout, resp: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(resp)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
