// End-to-end MCP protocol test.
//
// Spawns the zhtw-mcp binary, sends JSON-RPC messages over stdin, and
// verifies the stdout responses match expected structure and content.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Send a JSON-RPC request to the child process and read the response.
fn send_recv(stdin: &mut impl Write, stdout: &mut impl BufRead, request: &Value) -> Value {
    let msg = serde_json::to_string(request).unwrap();
    writeln!(stdin, "{}", msg).unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).expect("response should be valid JSON")
}

/// Send a notification (no response expected).
fn send_notification(stdin: &mut impl Write, request: &Value) {
    let msg = serde_json::to_string(request).unwrap();
    writeln!(stdin, "{}", msg).unwrap();
    stdin.flush().unwrap();
}

/// Build the binary path. In cargo test, the binary is in target/debug/.
fn binary_path() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    // test binary is in target/debug/deps/e2e_mcp-<hash>
    // the main binary is in target/debug/zhtw-mcp
    path.pop(); // remove test binary name
    if path.ends_with("deps") {
        path.pop(); // remove deps/
    }
    path.push("zhtw-mcp");
    path
}

#[test]
fn e2e_initialize_and_tools_list() {
    let bin = binary_path();
    if !bin.exists() {
        panic!("binary not found at {:?}; run `cargo build` first", bin);
    }

    let tmp_dir = tempfile::tempdir().expect("create temp dir");
    let overrides_path = tmp_dir.path().join("overrides.json");
    let suppressions_path = tmp_dir.path().join("suppressions.json");

    let mut child = Command::new(&bin)
        .args([
            "--overrides",
            overrides_path.to_str().unwrap(),
            "--suppressions",
            suppressions_path.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn zhtw-mcp");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // 0. Pre-init: tools/list before initialize should be rejected
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": 0,
            "params": {}
        }),
    );
    assert_eq!(resp["id"], 0);
    assert!(
        resp["error"].is_object(),
        "tools/list before initialize should return error"
    );
    assert_eq!(resp["error"]["code"], -32002); // SERVER_NOT_INITIALIZED
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not initialized"));

    // 1. Initialize
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "id": 1,
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }
        }),
    );
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["capabilities"]["tools"].is_object());
    assert!(resp["result"]["capabilities"]["resources"].is_object());
    assert!(resp["result"]["capabilities"]["prompts"].is_object());
    assert_eq!(resp["result"]["serverInfo"]["name"], "zhtw-mcp");

    // 2. Notifications/initialized (no response)
    send_notification(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    );

    // 3. Tools list — 1 tool: zhtw
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "id": 2,
            "params": {}
        }),
    );
    assert_eq!(resp["id"], 2);
    let tools = resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(tool_names.contains(&"zhtw"));

    // Verify tool annotations (MCP spec compliance)
    let zhtw = tools.iter().find(|t| t["name"] == "zhtw").unwrap();
    assert_eq!(zhtw["annotations"]["readOnly"], true);
    assert_eq!(zhtw["annotations"]["idempotent"], true);
    assert!(zhtw["annotations"].get("destructive").is_none());

    // Verify zhtw schema has expected parameters
    let props = &zhtw["inputSchema"]["properties"];
    assert!(props.get("text").is_some());
    assert!(props.get("fix_mode").is_some());
    assert!(props.get("max_errors").is_some());
    assert!(props.get("ignore_terms").is_some());
    assert!(props.get("profile").is_some());
    assert!(props.get("content_type").is_some());
    assert!(props.get("political_stance").is_some());

    // 4. zhtw lint-only (fix_mode absent = none) — detect 軟件
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 3,
            "params": {
                "name": "zhtw",
                "arguments": { "text": "這個軟件很好用" }
            }
        }),
    );
    assert_eq!(resp["id"], 3);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(output["accepted"], true);
    assert_eq!(output["applied_fixes"], 0);
    assert_eq!(output["gate"]["enabled"], false);
    let issues = output["issues"].as_array().unwrap();
    assert!(!issues.is_empty());
    assert_eq!(issues[0]["found"], "軟件");
    assert!(issues[0]["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s == "軟體"));
    // text field returns original (no fixes)
    assert_eq!(output["text"], "這個軟件很好用");

    // 5. zhtw gate-pass — clean text + max_errors: 0 + fix_mode: safe
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 4,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "這個軟體很好用",
                    "fix_mode": "safe",
                    "max_errors": 0
                }
            }
        }),
    );
    assert_eq!(resp["id"], 4);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(output["accepted"], true);
    assert_eq!(output["gate"]["enabled"], true);
    assert_eq!(output["gate"]["residual_errors"], 0);

    // 6. zhtw gate-fix — dirty text + fix_mode: safe, verify fixes
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 5,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "這個軟件用了很多內存",
                    "fix_mode": "safe"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 5);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(output["accepted"], true);
    let fixed_text = output["text"].as_str().unwrap();
    assert!(fixed_text.contains("軟體"));
    assert!(fixed_text.contains("記憶體"));
    assert!(output["applied_fixes"].as_u64().unwrap() > 0);

    // 7. zhtw with ignore_terms — 軟件 downgraded to info
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 6,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "這個軟件很好用",
                    "ignore_terms": ["軟件"]
                }
            }
        }),
    );
    assert_eq!(resp["id"], 6);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    let issues = output["issues"].as_array().unwrap();
    assert!(!issues.is_empty());
    let software_issue = issues.iter().find(|i| i["found"] == "軟件").unwrap();
    assert_eq!(software_issue["severity"], "info");
    // Summary should count it as info, not error
    assert_eq!(output["summary"]["info"].as_u64().unwrap(), 1);

    // 8. resources/list
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "resources/list",
            "id": 10,
            "params": {}
        }),
    );
    assert_eq!(resp["id"], 10);
    let resources = resp["result"]["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2);
    assert_eq!(resources[0]["uri"], "zh-tw://style-guide/moe");
    assert_eq!(resources[1]["uri"], "zh-tw://dictionary/ambiguous");

    // 9. resources/read — style guide
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "resources/read",
            "id": 11,
            "params": { "uri": "zh-tw://style-guide/moe" }
        }),
    );
    assert_eq!(resp["id"], 11);
    let contents = resp["result"]["contents"].as_array().unwrap();
    assert!(contents[0]["text"]
        .as_str()
        .unwrap()
        .contains("Punctuation"));

    // 10. prompts/list
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "prompts/list",
            "id": 12,
            "params": {}
        }),
    );
    assert_eq!(resp["id"], 12);
    let prompts = resp["result"]["prompts"].as_array().unwrap();
    assert!(!prompts.is_empty());
    assert_eq!(prompts[0]["name"], "normalize_tone");

    // 11. prompts/get
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "prompts/get",
            "id": 13,
            "params": { "name": "normalize_tone" }
        }),
    );
    assert_eq!(resp["id"], 13);
    let messages = resp["result"]["messages"].as_array().unwrap();
    assert!(!messages.is_empty());
    assert!(messages[0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("Traditional Chinese"));

    // -- E2E: content_type: "markdown" -- code inside fences excluded --

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 20,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "這個軟件很好\n\n```\n軟件 is ok in code\n```\n\n另一個軟件",
                    "content_type": "markdown"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 20);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    let issues = output["issues"].as_array().unwrap();
    // "軟件" in fenced code block should be excluded; only prose occurrences flagged
    let software_issues: Vec<_> = issues.iter().filter(|i| i["found"] == "軟件").collect();
    assert_eq!(
        software_issues.len(),
        2,
        "code block 軟件 should be excluded"
    );

    // -- E2E: profile: "strict_moe" -- variant rules fire --

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 21,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "裏面有線索",
                    "profile": "strict_moe"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 21);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(output["profile"], "strict_moe");
    let issues = output["issues"].as_array().unwrap();
    // strict_moe should catch 裏→裡 variant
    let variant_issue = issues.iter().find(|i| i["found"] == "裏");
    assert!(variant_issue.is_some(), "strict_moe should flag 裏 variant");
    assert!(variant_issue.unwrap()["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s == "裡"));

    // -- E2E: gate rejection (accepted: false, max_errors exceeded) --
    // "內地" is political_coloring → Severity::Error, which the gate counts.

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 22,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "回到內地出差",
                    "max_errors": 0
                }
            }
        }),
    );
    assert_eq!(resp["id"], 22);
    // Gate rejection: isError=true on the result, output JSON has accepted=false
    let result = &resp["result"];
    assert_eq!(
        result["isError"], true,
        "gate rejection should set isError=true"
    );
    let output_text = result["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(output_text).unwrap();
    assert_eq!(output["accepted"], false);
    assert_eq!(output["gate"]["enabled"], true);
    assert!(output["gate"]["residual_errors"].as_u64().unwrap() > 0);

    // -- E2E: fix_mode: "aggressive" --

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 23,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "這個軟件和視頻很好",
                    "fix_mode": "aggressive"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 23);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    let fixed = output["text"].as_str().unwrap();
    assert!(fixed.contains("軟體"), "aggressive should fix 軟件→軟體");
    assert!(fixed.contains("影片"), "aggressive should fix 視頻→影片");
    assert!(output["applied_fixes"].as_u64().unwrap() >= 2);

    // -- E2E: oversized request rejected by MAX_TEXT_BYTES --

    // Exactly 256 KiB should pass (boundary).
    let boundary_text = "a".repeat(256 * 1024);
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 24,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": boundary_text
                }
            }
        }),
    );
    assert_eq!(resp["id"], 24);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !content_text.contains("text too large"),
        "exactly 256 KiB should be accepted"
    );

    // 256 KiB + 1 byte should be rejected.
    let over_text = "a".repeat(256 * 1024 + 1);
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 25,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": over_text
                }
            }
        }),
    );
    assert_eq!(resp["id"], 25);
    let content = &resp["result"]["content"][0];
    let error_text = content["text"].as_str().unwrap();
    assert!(
        error_text.contains("text too large"),
        "256 KiB + 1 should be rejected"
    );

    // -- E2E: invalid arguments (missing text field) --

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 26,
            "params": {
                "name": "zhtw",
                "arguments": {}
            }
        }),
    );
    assert_eq!(resp["id"], 26);
    let content = &resp["result"]["content"][0];
    let error_text = content["text"].as_str().unwrap();
    assert!(
        error_text.contains("missing") && error_text.contains("text"),
        "missing text field should return error"
    );

    // -- E2E: invalid content_type rejected --

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 27,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "測試",
                    "content_type": "html"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 27);
    let result = &resp["result"];
    assert_eq!(result["isError"], true);
    let error_text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        error_text.contains("invalid") && error_text.contains("content_type"),
        "unknown content_type should be rejected: {error_text}"
    );

    // -- E2E: output: "compact" — deduplicated issues, no text/trace fields --

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 30,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "視頻和視頻都是視頻",
                    "output": "compact"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 30);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    assert_eq!(output["accepted"], true);
    // Compact omits text field when no fixes applied.
    assert!(
        output.get("text").is_none(),
        "compact without fixes should omit text"
    );
    // Compact omits trace field.
    assert!(output.get("trace").is_none(), "compact should omit trace");
    // Issues should be deduplicated: 3x 視頻 collapses to 1 group.
    let issues = output["issues"].as_array().unwrap();
    let video_groups: Vec<_> = issues.iter().filter(|i| i["found"] == "視頻").collect();
    assert_eq!(
        video_groups.len(),
        1,
        "compact should deduplicate identical issues into one group"
    );
    assert_eq!(
        video_groups[0]["count"].as_u64().unwrap(),
        3,
        "deduplicated group should have count=3"
    );
    // Locations array should have 3 entries.
    assert_eq!(
        video_groups[0]["locations"].as_array().unwrap().len(),
        3,
        "locations should list all 3 occurrences"
    );
    // Compact uses shared IssueType::name() for rule_type field.
    assert_eq!(
        video_groups[0]["rule_type"], "cross_strait",
        "rule_type should use snake_case name"
    );

    // -- E2E: output: "compact" with fix_mode — text included when fixes applied --

    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 31,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "這個軟件很好",
                    "output": "compact",
                    "fix_mode": "safe"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 31);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    assert!(output["applied_fixes"].as_u64().unwrap() > 0);
    // Compact includes text when fixes were applied.
    assert!(
        output.get("text").is_some(),
        "compact with fixes should include text"
    );
    assert!(
        output["text"].as_str().unwrap().contains("軟體"),
        "fixed text should contain 軟體"
    );

    // -- E2E: output: "compact" token reduction vs full --

    let test_text = "軟件用了視頻功能，視頻品質好。並行計算很快。";
    let resp_full = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 32,
            "params": {
                "name": "zhtw",
                "arguments": { "text": test_text, "output": "full" }
            }
        }),
    );
    let resp_compact = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 33,
            "params": {
                "name": "zhtw",
                "arguments": { "text": test_text, "output": "compact" }
            }
        }),
    );
    let full_len = resp_full["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .len();
    let compact_len = resp_compact["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .len();
    let reduction = 1.0 - (compact_len as f64 / full_len as f64);
    assert!(
        reduction >= 0.30,
        "MCP compact should achieve ≥30% reduction vs full: full={full_len} compact={compact_len} reduction={reduction:.2}"
    );

    // Close stdin to let the child exit gracefully.
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
    // tmp_dir auto-cleaned on drop
}

/// AI agent clients (e.g. claude-code) should auto-default to compact output
/// without explicitly passing `"output": "compact"`.
#[test]
fn e2e_auto_compact_for_ai_clients() {
    let bin = binary_path();
    let tmp_dir = tempfile::tempdir().expect("create temp dir");
    let overrides_path = tmp_dir.path().join("overrides.json");
    let suppressions_path = tmp_dir.path().join("suppressions.json");

    let mut child = Command::new(&bin)
        .args([
            "--overrides",
            overrides_path.to_str().unwrap(),
            "--suppressions",
            suppressions_path.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn zhtw-mcp");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // Initialize with clientInfo.name = "claude-code" (AI agent).
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "id": 1,
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "claude-code", "version": "1.0" }
            }
        }),
    );
    assert_eq!(resp["id"], 1);

    send_notification(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    );

    // Call zhtw WITHOUT explicit "output" field — should auto-compact.
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 2,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "視頻和視頻都是視頻"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 2);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();

    // Compact signature: no "text" field (no fixes applied), no "trace" field.
    assert!(
        output.get("text").is_none(),
        "auto-compact for AI client should omit text: {output}"
    );
    assert!(
        output.get("trace").is_none(),
        "auto-compact for AI client should omit trace: {output}"
    );
    // Issues should be deduplicated (compact grouping).
    let issues = output["issues"].as_array().unwrap();
    let video_groups: Vec<_> = issues.iter().filter(|i| i["found"] == "視頻").collect();
    assert_eq!(
        video_groups.len(),
        1,
        "auto-compact should deduplicate issues"
    );
    assert_eq!(
        video_groups[0]["count"].as_u64().unwrap(),
        3,
        "should show count=3 for 3 occurrences"
    );

    // Explicit "output": "full" should override auto-compact.
    let resp = send_recv(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "id": 3,
            "params": {
                "name": "zhtw",
                "arguments": {
                    "text": "這個軟件",
                    "output": "full"
                }
            }
        }),
    );
    assert_eq!(resp["id"], 3);
    let content_text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let output: Value = serde_json::from_str(content_text).unwrap();
    // Full mode: text and trace fields present.
    assert!(
        output.get("text").is_some(),
        "explicit full should include text"
    );
    assert!(
        output.get("trace").is_some(),
        "explicit full should include trace"
    );

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
}
