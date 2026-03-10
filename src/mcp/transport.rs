// Synchronous stdio transport for MCP JSON-RPC 2.0.
//
// Reads newline-delimited JSON from stdin via a background reader thread,
// dispatches to the tool server, writes JSON responses to stdout.
//
// The reader thread enables non-blocking reads with timeout, which is
// required for sampling support: the server can send a sampling/createMessage
// request and wait for the client's response without blocking the entire
// transport indefinitely.
//
// During a sampling window, the bridge may consume messages from the shared
// channel that belong to the main dispatch loop (notifications, other
// requests).  These are returned as "spillover" and re-processed by the main
// loop after the bridge is dropped.

use std::collections::VecDeque;
use std::io::{self, BufRead, Read, Write};
use std::string::FromUtf8Error;
use std::sync::mpsc;
use std::thread;

use anyhow::Result;

use super::sampling::{SamplingBridge, DEFAULT_SAMPLING_BUDGET, DEFAULT_SAMPLING_TIMEOUT};
use super::tools::Server;
use super::types::{
    JsonRpcRequest, JsonRpcResponse, INVALID_REQUEST, JSONRPC_VERSION, PARSE_ERROR,
};

/// Maximum line length we'll accept from stdin (4 MiB payload).
/// Prevents memory exhaustion from malformed input.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// Messages from the stdin reader thread.
#[derive(Debug)]
pub(crate) enum StdinMsg {
    /// A complete, non-empty line of text.
    Line(String),
    /// A line that exceeded the maximum allowed size.
    TooLong,
    /// A line that contains malformed UTF-8 text
    MalformedUtf8(FromUtf8Error),
}

/// Result of a bounded line read (internal to the reader thread).
enum ReadLine {
    Line,
    Eof,
    TooLong,
    MalformedUtf8(FromUtf8Error),
}

/// Serialize a response as JSON and write it to stdout, followed by a newline.
fn send(out: &mut impl Write, resp: &JsonRpcResponse) -> Result<()> {
    let json = serde_json::to_string(resp)?;
    writeln!(out, "{json}")?;
    out.flush()?;
    Ok(())
}

/// Read a single line from reader, bounded to MAX_LINE_BYTES.
///
/// Uses read_until on raw bytes so that a multi-byte UTF-8 sequence
/// straddling the MAX_LINE_BYTES boundary does not cause an InvalidData
/// error (which would kill the reader thread). If the completed line
/// contains invalid UTF-8, returns `ReadLine::MalformedUtf8` so the
/// caller can emit a proper error response.
fn read_bounded_line(reader: &mut impl BufRead, buf: &mut String) -> io::Result<ReadLine> {
    let mut raw: Vec<u8> = Vec::new();
    let n = reader
        .by_ref()
        .take(MAX_LINE_BYTES + 1)
        .read_until(b'\n', &mut raw)?;

    if n == 0 {
        return Ok(ReadLine::Eof);
    }

    if !raw.ends_with(b"\n") && n as u64 > MAX_LINE_BYTES {
        drain_until_newline(reader)?;
        return Ok(ReadLine::TooLong);
    }

    match String::from_utf8(raw) {
        Ok(text) => {
            *buf = text;
            Ok(ReadLine::Line)
        }
        Err(e) => Ok(ReadLine::MalformedUtf8(e)),
    }
}

/// Consume and discard bytes from reader until a newline or EOF.
fn drain_until_newline(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            reader.consume(pos + 1);
            break;
        }
        let len = available.len();
        reader.consume(len);
    }
    Ok(())
}

/// Run the MCP server on stdin/stdout until EOF.
///
/// Spawns a background thread to read stdin, enabling timeout-based reads
/// for sampling support.  The main thread processes requests and writes
/// responses to stdout.
///
/// Messages consumed by the sampling bridge during a tools/call dispatch
/// are returned as spillover and re-processed before reading new input.
pub fn run_stdio(server: &mut Server) -> Result<()> {
    let (line_tx, line_rx) = mpsc::channel::<StdinMsg>();

    // Background stdin reader: reads bounded lines and sends them to the channel.
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut buf = String::new();
        loop {
            match read_bounded_line(&mut reader, &mut buf) {
                Ok(ReadLine::Eof) => break,
                Ok(ReadLine::Line) => {
                    let trimmed = buf.trim().to_string();
                    if !trimmed.is_empty() && line_tx.send(StdinMsg::Line(trimmed)).is_err() {
                        break; // receiver dropped
                    }
                }
                Ok(ReadLine::TooLong) => {
                    if line_tx.send(StdinMsg::TooLong).is_err() {
                        break;
                    }
                }
                Ok(ReadLine::MalformedUtf8(e)) => {
                    if line_tx.send(StdinMsg::MalformedUtf8(e)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("stdin reader: {e}");
                    break;
                }
            }
        }
    });

    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut pending: VecDeque<StdinMsg> = VecDeque::new();

    loop {
        // Drain spillover before blocking on the channel.
        let Some(msg) = pending.pop_front().or_else(|| line_rx.recv().ok()) else {
            break; // channel closed = EOF
        };

        let line = match msg {
            StdinMsg::Line(l) => l,
            StdinMsg::TooLong => {
                log::warn!("line too long, returning error");
                let resp =
                    JsonRpcResponse::error(None, INVALID_REQUEST, "request too large".into());
                send(&mut writer, &resp)?;
                continue;
            }
            StdinMsg::MalformedUtf8(e) => {
                log::warn!("line contains malformed UTF-8 character(s), returning error: {e}");
                let resp = JsonRpcResponse::error(
                    None,
                    PARSE_ERROR,
                    "request contains malformed UTF-8 character(s)".into(),
                );
                send(&mut writer, &resp)?;
                continue;
            }
        };

        // Parse once as a generic Value; reuse the result for both the
        // response-shape check and the typed JsonRpcRequest conversion.
        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("invalid JSON-RPC: {e}");
                let resp =
                    JsonRpcResponse::error(None, PARSE_ERROR, "invalid JSON-RPC request".into());
                send(&mut writer, &resp)?;
                continue;
            }
        };

        // Silently discard response-shaped messages (no method field).
        // These can arrive when a stale sampling response from a previous
        // bridge lifetime reaches the channel after the bridge was dropped.
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
                send(&mut writer, &resp)?;
                continue;
            }
        };

        if req.jsonrpc != JSONRPC_VERSION {
            log::warn!("invalid jsonrpc version: {}", req.jsonrpc);
            let resp = JsonRpcResponse::error(
                req.id.clone(),
                INVALID_REQUEST,
                "unsupported JSON-RPC version".into(),
            );
            send(&mut writer, &resp)?;
            continue;
        }

        // Notifications (no id) get no response per JSON-RPC spec.
        let (resp, spillover) = dispatch(server, &mut req, &line_rx, &mut writer);
        if let Some(resp) = resp {
            send(&mut writer, &resp)?;
        }
        for msg in spillover {
            pending.push_back(msg);
        }
    }

    Ok(())
}

fn dispatch(
    server: &mut Server,
    req: &mut JsonRpcRequest,
    line_rx: &mpsc::Receiver<StdinMsg>,
    writer: &mut impl Write,
) -> (Option<JsonRpcResponse>, Vec<StdinMsg>) {
    // Pre-init routing (initialize, ping, notifications, rejection).
    if let Some(resp) = server.dispatch_preinit(req) {
        return (resp, Vec::new());
    }

    // tools/call needs a sampling bridge; all others delegate to dispatch_method.
    if req.method == "tools/call" {
        let mut bridge = if server.supports_sampling() {
            Some(SamplingBridge::new(
                writer,
                line_rx,
                DEFAULT_SAMPLING_TIMEOUT,
                DEFAULT_SAMPLING_BUDGET,
            ))
        } else {
            None
        };
        let resp = server.handle_tools_call(req, bridge.as_mut());
        let spillover = bridge.map_or_else(Vec::new, SamplingBridge::into_spillover);
        (Some(resp), spillover)
    } else {
        (server.dispatch_method(req), Vec::new())
    }
}
