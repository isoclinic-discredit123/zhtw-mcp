// MCP tool handler implementations.
//
// One tool exposed to the MCP client:
//   zhtw — unified lint / fix / gate for Traditional Chinese (Taiwan) text

use serde::Serialize;
use serde_json::{json, Value};

use super::prompts;
use super::resources;
use super::sampling::{refine_issues_with_sampling, SamplingBridge};
use super::types::{
    CallToolParams, CallToolResult, ClientCapabilities, InitializeParams, InitializeResult,
    JsonRpcRequest, JsonRpcResponse, PromptCapability, PromptGetParams, ResourceCapability,
    ResourceReadParams, ServerCapabilities, ServerInfo, ToolAnnotations, ToolCapability, ToolDef,
    ToolsListResult, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, MCP_PROTOCOL_VERSION,
    METHOD_NOT_FOUND, SERVER_NOT_INITIALIZED,
};
use crate::audit::Trace;
use crate::engine::excluded::ByteRange;
use crate::engine::s2t::S2TConverter;
use crate::engine::scan::{build_exclusions_for_content_type, ContentType, Scanner};
#[cfg(feature = "translate")]
use crate::engine::translate::calibrate_issues;
use crate::engine::zhtype::{detect_chinese_type, ChineseType};
use crate::fixer::{
    apply_fixes_with_context, remap_to_post_fix, suppress_convergent_issues, FixMode,
};
use crate::rules::loader::compute_ruleset_hash;
use crate::rules::ruleset::Ruleset;
use crate::rules::ruleset::{Issue, IssueType, PoliticalStance, Profile, Severity};
use crate::rules::store::{OverrideStore, PackStore, SuppressionStore};

/// The MCP tool server. Holds the compiled scanner, override/pack stores,
/// ruleset metadata, and client capability information.
pub struct Server {
    scanner: Scanner,
    /// SC→TC converter for auto-converting Simplified Chinese input.
    s2t: S2TConverter,
    suppression_store: SuppressionStore,
    ruleset_hash: String,
    /// Parsed client capabilities from the initialize handshake.
    client_capabilities: ClientCapabilities,
    /// Whether the client has completed the initialize handshake.
    initialized: bool,
    /// Client name from initialize handshake, used for auto-compact detection.
    client_name: Option<String>,
}

impl Server {
    /// Create a new server from the embedded ruleset + override/pack stores.
    pub fn new(
        store: OverrideStore,
        suppression_store: SuppressionStore,
        pack_store: PackStore,
        active_packs: Vec<String>,
    ) -> anyhow::Result<Self> {
        let base_ruleset = crate::rules::loader::load_embedded_ruleset()?;

        let (scanner, ruleset_hash) =
            Self::build_scanner(&base_ruleset, &store, &pack_store, &active_packs)?;

        Ok(Self {
            scanner,
            s2t: S2TConverter::new(),
            suppression_store,
            ruleset_hash,
            client_capabilities: ClientCapabilities::default(),
            initialized: false,
            client_name: None,
        })
    }

    /// Build a scanner from the base ruleset, overrides, and active packs.
    fn build_scanner(
        base_ruleset: &Ruleset,
        store: &OverrideStore,
        pack_store: &PackStore,
        active_packs: &[String],
    ) -> anyhow::Result<(Scanner, String)> {
        let (merged_spelling, merged_case) = crate::rules::store::build_merged_rules(
            &base_ruleset.spelling_rules,
            &base_ruleset.case_rules,
            store,
            pack_store,
            active_packs,
        );

        let ruleset_hash = compute_ruleset_hash(&merged_spelling, &merged_case);
        let scanner = Scanner::new(merged_spelling, merged_case);

        Ok((scanner, ruleset_hash))
    }

    /// Whether the client declared sampling support during initialization.
    pub(crate) fn supports_sampling(&self) -> bool {
        self.client_capabilities.sampling
    }

    /// Handle pre-initialization routing shared between sync and async transports.
    ///
    /// Returns `Some(response)` if the method was handled (initialize, ping,
    /// notifications, or rejection before init). Returns `None` if the caller
    /// should proceed with post-init method dispatch.
    pub(crate) fn dispatch_preinit(
        &mut self,
        req: &mut JsonRpcRequest,
    ) -> Option<Option<JsonRpcResponse>> {
        match req.method.as_str() {
            "initialize" => {
                if req.id.is_none() {
                    log::warn!("initialize sent as notification, ignoring");
                    return Some(None);
                }
                if self.initialized {
                    log::warn!("duplicate initialize request, rejecting");
                    return Some(Some(JsonRpcResponse::error(
                        req.id.clone(),
                        INVALID_REQUEST,
                        "already initialized".into(),
                    )));
                }
                Some(Some(self.handle_initialize(req)))
            }
            "initialized" | "notifications/initialized" | "notifications/cancelled" => {
                log::info!("{}", req.method);
                Some(None)
            }
            "ping" => {
                if req.id.is_some() {
                    Some(Some(JsonRpcResponse::success(
                        req.id.clone(),
                        serde_json::json!({}),
                    )))
                } else {
                    log::debug!("ping sent as notification, ignoring");
                    Some(None)
                }
            }
            _ if !self.initialized => {
                log::warn!("rejecting {} before initialization", req.method);
                Some(if req.id.is_some() {
                    Some(JsonRpcResponse::error(
                        req.id.clone(),
                        SERVER_NOT_INITIALIZED,
                        "server not initialized".into(),
                    ))
                } else {
                    None
                })
            }
            _ => None, // proceed to post-init dispatch
        }
    }

    /// Route a post-init method call (no sampling bridge).
    ///
    /// Shared between both transports for tools/list, resources, prompts, etc.
    /// tools/call is handled separately in the sync transport (needs bridge).
    pub(crate) fn dispatch_method(&mut self, req: &mut JsonRpcRequest) -> Option<JsonRpcResponse> {
        match req.method.as_str() {
            "tools/list" => Some(self.handle_tools_list(req)),
            "resources/list" => Some(self.handle_resources_list(req)),
            "resources/read" => Some(self.handle_resources_read(req)),
            "prompts/list" => Some(self.handle_prompts_list(req)),
            "prompts/get" => Some(self.handle_prompts_get(req)),
            _ => {
                log::debug!("unhandled method: {}", req.method);
                if req.id.is_some() {
                    Some(JsonRpcResponse::error(
                        req.id.clone(),
                        METHOD_NOT_FOUND,
                        format!("unknown method: {}", req.method),
                    ))
                } else {
                    None
                }
            }
        }
    }

    // MCP method handlers

    pub fn handle_initialize(&mut self, req: &mut JsonRpcRequest) -> JsonRpcResponse {
        let params: InitializeParams = match parse_params(req, "initialize") {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        // Store parsed client capabilities for later use (e.g. sampling).
        self.client_capabilities = ClientCapabilities::from(&params.capabilities);
        self.client_name = params.client_info.map(|ci| ci.name);
        self.initialized = true;

        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION,
            capabilities: ServerCapabilities {
                tools: ToolCapability {
                    list_changed: false,
                },
                resources: ResourceCapability {
                    list_changed: false,
                },
                prompts: PromptCapability {
                    list_changed: false,
                },
            },
            server_info: ServerInfo {
                name: "zhtw-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        };

        json_response(req.id.clone(), result)
    }

    pub fn handle_tools_list(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let tools = ToolsListResult {
            tools: tool_definitions(),
        };
        json_response(req.id.clone(), tools)
    }

    pub(crate) fn handle_tools_call(
        &mut self,
        req: &mut JsonRpcRequest,
        bridge: Option<&mut SamplingBridge<'_>>,
    ) -> JsonRpcResponse {
        let params: CallToolParams = match parse_params(req, "tools/call") {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        let result = match params.name.as_str() {
            "zhtw" => self.tool_check(&params.arguments, bridge),
            _ => CallToolResult::error(format!("unknown tool: {}", params.name)),
        };

        json_response(req.id.clone(), result)
    }

    // Tool implementation

    /// Maximum allowed size of the text field (256 KiB). Requests exceeding
    /// this trigger a structured error before any processing begins.
    const MAX_TEXT_BYTES: usize = 256 * 1024;

    fn tool_check(
        &self,
        args: &Value,
        mut bridge: Option<&mut SamplingBridge<'_>>,
    ) -> CallToolResult {
        let text = match require_str(args, "text") {
            Ok(t) => t,
            Err(r) => return r,
        };

        if text.len() > Self::MAX_TEXT_BYTES {
            return CallToolResult::error(format!(
                "text too large: {} bytes exceeds limit of {} bytes (256 KiB)",
                text.len(),
                Self::MAX_TEXT_BYTES,
            ));
        }

        // Auto-detect Simplified Chinese and convert to Traditional via S2T.
        let s2t_converted: Option<String> = if detect_chinese_type(text) == ChineseType::Simplified
        {
            Some(self.s2t.convert(text))
        } else {
            None
        };
        let text = s2t_converted.as_deref().unwrap_or(text);

        let fix_mode = match parse_fix_mode(args) {
            Ok(m) => m,
            Err(r) => return r,
        };
        let profile = match parse_profile(args) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let content_type = match parse_content_type(args) {
            Ok(ct) => ct,
            Err(r) => return r,
        };
        let stance = match parse_political_stance(args) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let max_errors = args.get("max_errors").and_then(|v| v.as_u64());
        let max_warnings = args.get("max_warnings").and_then(|v| v.as_u64());
        let ignore_terms = parse_ignore_terms(args);
        let ignore_set: std::collections::HashSet<&str> =
            ignore_terms.iter().map(String::as_str).collect();
        let explain = parse_explain(args);
        let output_mode =
            match parse_output_mode(args, default_output_mode(self.client_name.as_deref())) {
                Ok(m) => m,
                Err(r) => return r,
            };
        let fix_output = match parse_fix_output(args) {
            Ok(m) => m,
            Err(r) => return r,
        };
        #[cfg(feature = "translate")]
        let verify = parse_verify(args);

        let stance_name = stance.unwrap_or(PoliticalStance::RocCentric).name();

        match fix_mode {
            FixMode::None => {
                // Lint-only path.
                let output = self
                    .scanner
                    .scan_for_content_type(text, content_type, profile);
                let detected_script = if s2t_converted.is_some() {
                    "simplified"
                } else {
                    output.detected_script.name()
                };
                let mut issues = output.issues;
                if let Some(st) = stance {
                    filter_by_stance(&mut issues, st);
                }
                // Calibrate issues via Google Translate anchor matching.
                #[cfg(feature = "translate")]
                let calibrate_result = if verify {
                    Some(calibrate_issues(text, &mut issues))
                } else {
                    None
                };

                if let Some(b) = bridge.as_mut() {
                    refine_issues_with_sampling(&mut issues, b, text, 0.3, 0.8);
                }
                self.apply_suppressions(&mut issues);
                apply_ignore_set(&mut issues, &ignore_set);

                let trace =
                    Trace::new("zhtw", &self.ruleset_hash, text).with_issue_count(issues.len());

                build_check_output(&CheckOutputParams {
                    result_text: text,
                    issues: &issues,
                    applied_fixes: 0,
                    max_errors,
                    max_warnings,
                    profile,
                    stance_name,
                    detected_script,
                    s2t_applied: s2t_converted.is_some(),
                    trace: &trace,
                    explain,
                    output_mode,
                    has_fixes: s2t_converted.is_some(),
                    fix_output,
                    original_text: text,
                    fix_records: &[],
                    #[cfg(feature = "translate")]
                    calibrate_result,
                })
            }

            mode @ (FixMode::Orthographic | FixMode::LexicalSafe | FixMode::LexicalContextual) => {
                // Fix path: scan, apply fixes, re-scan for residual issues.
                let excluded = build_exclusions_for_content_type(text, content_type);
                let scan_out = self.scanner.scan_with_prebuilt_excluded(
                    text,
                    &excluded,
                    profile,
                    content_type,
                );
                let detected_script = if s2t_converted.is_some() {
                    "simplified"
                } else {
                    scan_out.detected_script.name()
                };
                let mut issues = scan_out.issues;
                if let Some(st) = stance {
                    filter_by_stance(&mut issues, st);
                }

                // Calibrate issues via Google Translate anchor matching.
                #[cfg(feature = "translate")]
                let calibrate_result = if verify {
                    Some(calibrate_issues(text, &mut issues))
                } else {
                    None
                };

                if let Some(b) = bridge.as_mut() {
                    refine_issues_with_sampling(&mut issues, b, text, 0.3, 0.8);
                }

                self.apply_suppressions(&mut issues);
                apply_ignore_set(&mut issues, &ignore_set);

                // Snapshot AFTER suppressions so restored severity reflects final state.
                struct PreservedState {
                    term: String,
                    orig_offset: usize,
                    length: usize,
                    english: Option<String>,
                    severity: Severity,
                    anchor_match: Option<bool>,
                    context: Option<String>,
                    suggestions: Vec<String>,
                }

                let preserved_states: Vec<PreservedState> = issues
                    .iter()
                    .map(|i| PreservedState {
                        term: i.found.clone(),
                        orig_offset: i.offset,
                        length: i.length,
                        english: i.english.clone(),
                        severity: i.severity,
                        anchor_match: i.anchor_match,
                        context: i.context.clone(),
                        suggestions: i.suggestions.clone(),
                    })
                    .collect();

                let excluded_pairs = to_offset_pairs(&excluded);
                let fix_result = apply_fixes_with_context(
                    text,
                    &issues,
                    mode,
                    &excluded_pairs,
                    Some(self.scanner.segmenter()),
                );

                // Re-scan after fixes.
                let mut remaining_issues = self
                    .scanner
                    .scan_for_content_type(&fix_result.text, content_type, profile)
                    .issues;
                if let Some(st) = stance {
                    filter_by_stance(&mut remaining_issues, st);
                }
                self.apply_suppressions(&mut remaining_issues);
                apply_ignore_set(&mut remaining_issues, &ignore_set);

                // Precompute remapped offsets once (O(M*F)) and index by
                // post-fix offset for O(1) lookup per remaining issue.
                use rustc_hash::FxHashMap;
                let mut state_by_offset: FxHashMap<usize, Vec<usize>> =
                    FxHashMap::with_capacity_and_hasher(preserved_states.len(), Default::default());
                for (idx, state) in preserved_states.iter().enumerate() {
                    let remapped = remap_to_post_fix(state.orig_offset, &fix_result.applied_fixes);
                    state_by_offset.entry(remapped).or_default().push(idx);
                }

                // Re-apply preserved states using identity-safe matching:
                // term + remapped offset + length + english must all match.
                for issue in &mut remaining_issues {
                    if let Some(candidates) = state_by_offset.get(&issue.offset) {
                        if let Some(&idx) = candidates.iter().find(|&&idx| {
                            let s = &preserved_states[idx];
                            s.term == issue.found
                                && s.length == issue.length
                                && s.english == issue.english
                        }) {
                            let state = &preserved_states[idx];
                            issue.severity = state.severity;
                            issue.anchor_match = state.anchor_match;
                            issue.context = state.context.clone();
                            issue.suggestions = state.suggestions.clone();
                        }
                    }
                }

                // Suppress convergent-chain noise: remove re-scan issues
                // whose offset falls within a byte range written by the fixer.
                suppress_convergent_issues(&mut remaining_issues, &fix_result.applied_fixes);

                let trace = Trace::new("zhtw", &self.ruleset_hash, text)
                    .with_issue_count(remaining_issues.len())
                    .with_output(&fix_result.text);

                build_check_output(&CheckOutputParams {
                    result_text: &fix_result.text,
                    issues: &remaining_issues,
                    applied_fixes: fix_result.applied,
                    max_errors,
                    max_warnings,
                    profile,
                    stance_name,
                    detected_script,
                    s2t_applied: s2t_converted.is_some(),
                    trace: &trace,
                    explain,
                    output_mode,
                    has_fixes: fix_result.applied > 0 || s2t_converted.is_some(),
                    fix_output,
                    original_text: text,
                    fix_records: &fix_result.applied_fixes,
                    #[cfg(feature = "translate")]
                    calibrate_result,
                })
            }
        }
    }

    /// Downgrade suppressed issues to Info severity.
    fn apply_suppressions(&self, issues: &mut [Issue]) {
        for issue in issues {
            if self.suppression_store.is_suppressed(&issue.found) {
                issue.severity = Severity::Info;
            }
        }
    }

    // -- Resource and prompt handlers -----------------------------------------

    pub fn handle_resources_list(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        json_response(req.id.clone(), resources::list_resources())
    }

    pub fn handle_resources_read(&self, req: &mut JsonRpcRequest) -> JsonRpcResponse {
        let params: ResourceReadParams = match parse_params(req, "resources/read") {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        match resources::read_resource(&params.uri, self.scanner.spelling_rules()) {
            Some(result) => json_response(req.id.clone(), result),
            None => JsonRpcResponse::error(
                req.id.clone(),
                INVALID_PARAMS,
                format!("unknown resource URI: {}", params.uri),
            ),
        }
    }

    pub fn handle_prompts_list(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let result = prompts::list_prompts();
        json_response(req.id.clone(), json!({ "prompts": result }))
    }

    pub fn handle_prompts_get(&self, req: &mut JsonRpcRequest) -> JsonRpcResponse {
        let params: PromptGetParams = match parse_params(req, "prompts/get") {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        let prompt_args: std::collections::HashMap<String, String> = params
            .arguments
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        match prompts::get_prompt(&params.name, &prompt_args) {
            Some(result) => json_response(req.id.clone(), result),
            None => JsonRpcResponse::error(
                req.id.clone(),
                INVALID_PARAMS,
                format!("unknown prompt: {}", params.name),
            ),
        }
    }
}

/// Serialize result to JSON and wrap in a success response, or return an
/// internal error response on serialization failure.
fn json_response(
    id: Option<super::types::RequestId>,
    result: impl serde::Serialize,
) -> JsonRpcResponse {
    match serde_json::to_value(result) {
        Ok(v) => JsonRpcResponse::success(id, v),
        Err(e) => {
            log::error!("failed to serialize response: {e}");
            JsonRpcResponse::error(id, INTERNAL_ERROR, "internal server error".into())
        }
    }
}

/// Convert excluded byte ranges to the (start, end) pairs expected by apply_fixes.
fn to_offset_pairs(ranges: &[ByteRange]) -> Vec<(usize, usize)> {
    ranges.iter().map(|r| (r.start, r.end)).collect()
}

/// Parse and take MCP request params, returning a typed struct or an error response.
#[allow(clippy::result_large_err)]
fn parse_params<T: serde::de::DeserializeOwned>(
    req: &mut JsonRpcRequest,
    method: &str,
) -> Result<T, JsonRpcResponse> {
    serde_json::from_value(std::mem::take(&mut req.params)).map_err(|e| {
        log::warn!("bad {method} params: {e}");
        JsonRpcResponse::error(
            req.id.clone(),
            INVALID_PARAMS,
            format!("invalid {method} parameters"),
        )
    })
}

/// Extract a required string field from a JSON object, returning a
/// CallToolResult::error on failure so callers can return Err(r).
fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, CallToolResult> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| CallToolResult::error(format!("missing '{field}' parameter")))
}

/// Parse the optional "fix_mode" field from tool arguments.
/// Returns an error for unrecognized values instead of silently defaulting.
fn parse_fix_mode(args: &Value) -> Result<FixMode, CallToolResult> {
    match args.get("fix_mode").and_then(|v| v.as_str()) {
        Some("orthographic") => Ok(FixMode::Orthographic),
        Some("lexical_safe") => Ok(FixMode::LexicalSafe),
        Some("lexical_contextual") => Ok(FixMode::LexicalContextual),
        None | Some("none") => Ok(FixMode::None),
        Some(other) => Err(CallToolResult::error(format!(
            "invalid 'fix_mode': '{other}' (expected 'none', 'orthographic', 'lexical_safe', or 'lexical_contextual')"
        ))),
    }
}

/// Parse the optional "content_type" field from tool arguments.
/// Returns an error for unrecognized values instead of silently defaulting.
fn parse_content_type(args: &Value) -> Result<ContentType, CallToolResult> {
    match args.get("content_type").and_then(|v| v.as_str()) {
        Some("markdown") => Ok(ContentType::Markdown),
        Some("markdown-scan-code") => Ok(ContentType::MarkdownScanCode),
        Some("yaml") => Ok(ContentType::Yaml),
        Some("plain") | None => Ok(ContentType::Plain),
        Some(other) => Err(CallToolResult::error(format!(
            "invalid 'content_type': '{other}' (expected 'plain', 'markdown', 'markdown-scan-code', or 'yaml')"
        ))),
    }
}

/// Parse the optional "profile" field from tool arguments.
/// Returns an error for unrecognized values instead of silently defaulting.
fn parse_profile(args: &Value) -> Result<Profile, CallToolResult> {
    match args.get("profile").and_then(|v| v.as_str()) {
        None => Ok(Profile::Default),
        Some(s) => Profile::from_str_strict(s).ok_or_else(|| {
            CallToolResult::error(format!(
                "invalid 'profile': '{s}' (expected 'default', 'strict_moe', or 'ui_strings')"
            ))
        }),
    }
}

/// Parse the optional "political_stance" field from tool arguments.
/// Returns an error for unrecognized values instead of silently defaulting.
fn parse_political_stance(args: &Value) -> Result<Option<PoliticalStance>, CallToolResult> {
    match args.get("political_stance").and_then(|v| v.as_str()) {
        None => Ok(None),
        Some(s) => PoliticalStance::from_str_strict(s).map(Some).ok_or_else(|| {
            CallToolResult::error(format!(
                "invalid 'political_stance': '{s}' (expected 'roc_centric', 'international', or 'neutral')"
            ))
        }),
    }
}

/// Fix output format: how corrected text is returned when fixes are applied.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FixOutputMode {
    /// Return the full corrected text (backward compat default).
    Full,
    /// Return search/replace blocks (LLM-friendly patching format).
    SearchReplace,
    /// Return a patches array with byte offsets into the original text.
    Patch,
}

impl FixOutputMode {
    fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::SearchReplace => "search_replace",
            Self::Patch => "patch",
        }
    }
}

/// Parse the optional "fix_output" parameter from tool arguments.
fn parse_fix_output(args: &Value) -> Result<FixOutputMode, CallToolResult> {
    match args.get("fix_output").and_then(|v| v.as_str()) {
        Some("full") | None => Ok(FixOutputMode::Full),
        Some("search_replace") => Ok(FixOutputMode::SearchReplace),
        Some("patch") => Ok(FixOutputMode::Patch),
        Some(other) => Err(CallToolResult::error(format!(
            "invalid 'fix_output': '{other}' (expected 'full', 'search_replace', or 'patch')"
        ))),
    }
}

/// Parse the optional "explain" boolean from tool arguments.
fn parse_explain(args: &Value) -> bool {
    args.get("explain")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Output mode for zhtw responses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Full,
    Compact,
    /// Header-once TSV format for LLM-facing responses.
    /// Eliminates JSON syntax tax (repeated keys, braces, quotes) that
    /// inflates BPE token count by 40-60% with zero semantic value.
    Tabular,
}

/// Parse the optional "output" mode from tool arguments.
/// When no explicit value is given, uses the provided default (which may
/// be auto-detected from the client identity).
fn parse_output_mode(args: &Value, default: OutputMode) -> Result<OutputMode, CallToolResult> {
    match args.get("output").and_then(|v| v.as_str()) {
        Some("compact") => Ok(OutputMode::Compact),
        Some("full") => Ok(OutputMode::Full),
        Some("tabular") => Ok(OutputMode::Tabular),
        None => Ok(default),
        Some(other) => Err(CallToolResult::error(format!(
            "invalid 'output': '{other}' (expected 'full', 'compact', or 'tabular')"
        ))),
    }
}

/// Known AI agent/CLI client names that benefit from compact output.
/// Matched as exact full-name against the lowercased `clientInfo.name`.
/// Only programmatic agents/CLIs — NOT desktop GUI apps like "Claude Desktop".
const AI_AGENT_CLIENTS: &[&str] = &[
    "claude-code",
    "claude code",
    "cursor",
    "cline",
    "continue",
    "zed",
    "windsurf",
    "copilot",
    "aider",
    "cody",
    "roo",
    "roo-code",
    "roo code",
];

/// Determine default output mode from client identity.
/// Uses exact full-name match only to avoid false positives on clients
/// like "Claude Desktop" that happen to share a token with an agent name.
/// Strips trailing version suffixes (`/1.0`, ` 1.0`) before matching,
/// since some clients embed version info in the name field.
fn default_output_mode(client_name: Option<&str>) -> OutputMode {
    match client_name {
        Some(name) => {
            let lower = name.to_ascii_lowercase();
            // Strip trailing version suffix: "Cursor/0.1.0" → "cursor", "cline 1.2" → "cline"
            let base = lower
                .split('/')
                .next()
                .unwrap_or(&lower)
                .trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')
                .trim();
            if AI_AGENT_CLIENTS.contains(&base) {
                OutputMode::Compact
            } else {
                OutputMode::Full
            }
        }
        None => OutputMode::Full,
    }
}

/// Parse the optional "verify" flag from tool arguments.
#[cfg(feature = "translate")]
fn parse_verify(args: &Value) -> bool {
    args.get("verify")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Generate a cultural/linguistic explanation for an issue.
///
/// Draws from the context, english, and rule_type fields to produce
/// a brief explanation useful for AI agents and educational applications.
fn build_explanation(issue: &Issue) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    match issue.rule_type {
        IssueType::CrossStrait => {
            if let Some(eng) = &issue.english {
                parts.push(format!(
                    "'{}' is a mainland Chinese term for '{}'; Taiwan uses '{}'.",
                    issue.found,
                    eng,
                    issue.suggestions.join(" / "),
                ));
            } else if !issue.suggestions.is_empty() {
                parts.push(format!(
                    "'{}' is a mainland Chinese expression; Taiwan standard: {}.",
                    issue.found,
                    issue.suggestions.join(" / "),
                ));
            }
        }
        IssueType::Confusable => {
            if let Some(eng) = &issue.english {
                parts.push(format!(
                    "'{}' is ambiguous across the strait. English anchor: '{}'. Taiwan form: {}.",
                    issue.found,
                    eng,
                    issue.suggestions.join(" / "),
                ));
            }
        }
        IssueType::PoliticalColoring => {
            parts.push(format!(
                "'{}' carries mainland political connotations; prefer {}.",
                issue.found,
                issue.suggestions.join(" / "),
            ));
        }
        IssueType::Variant => {
            parts.push(format!(
                "'{}' is a non-standard character variant; MoE standard form: {}.",
                issue.found,
                issue.suggestions.join(" / "),
            ));
        }
        IssueType::Typo => {
            parts.push(format!(
                "'{}' appears to be a typo; suggested: {}.",
                issue.found,
                issue.suggestions.join(" / "),
            ));
        }
        IssueType::Case => {
            parts.push(format!(
                "'{}' has incorrect casing; standard form: {}.",
                issue.found,
                issue.suggestions.join(" / "),
            ));
        }
        IssueType::Punctuation => {
            parts.push(format!(
                "'{}' should use the full-width equivalent {} in CJK prose per MoE standards.",
                issue.found,
                issue.suggestions.join(" / "),
            ));
        }
        IssueType::Grammar => {
            if let Some(ctx) = &issue.context {
                parts.push(format!(
                    "'{}' — {}. Suggested: {}.",
                    issue.found,
                    ctx,
                    issue.suggestions.join(" / "),
                ));
            } else {
                parts.push(format!(
                    "'{}' is a grammatical issue; suggested: {}.",
                    issue.found,
                    issue.suggestions.join(" / "),
                ));
            }
        }
    }

    // Grammar issues already embed context in the main explanation;
    // skip the shared Context: append to avoid duplication.
    if issue.rule_type != IssueType::Grammar {
        if let Some(ctx) = &issue.context {
            parts.push(format!("Context: {ctx}"));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Parse the optional "ignore_terms" array from tool arguments.
fn parse_ignore_terms(args: &Value) -> Vec<String> {
    args.get("ignore_terms")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Remove political_coloring issues that the given stance suppresses.
fn filter_by_stance(issues: &mut Vec<Issue>, stance: PoliticalStance) {
    issues.retain(|issue| {
        issue.rule_type != IssueType::PoliticalColoring || stance.allows_rule(&issue.found)
    });
}

/// Downgrade issues whose found term matches a pre-built ignore set to Info.
fn apply_ignore_set(issues: &mut [Issue], ignore_set: &std::collections::HashSet<&str>) {
    if ignore_set.is_empty() {
        return;
    }
    for issue in issues {
        if ignore_set.contains(issue.found.as_str()) {
            issue.severity = Severity::Info;
        }
    }
}

/// Issue severity summary counts.
#[derive(Serialize)]
struct IssueSummary {
    errors: usize,
    warnings: usize,
    info: usize,
}

/// Gate status in the tool response.
#[derive(Serialize)]
struct GateInfo {
    enabled: bool,
    max_errors: usize,
    residual_errors: usize,
    max_warnings: usize,
    residual_warnings: usize,
}

/// Anchor provenance for explain mode (borrowed).
#[derive(Serialize)]
struct AnchorProvenance<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_en: Option<&'a str>,
    anchor_match: Option<bool>,
}

/// Anchor provenance for compact mode (owned).
#[derive(Serialize)]
struct AnchorProvenanceOwned {
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_en: Option<String>,
    anchor_match: Option<bool>,
}

/// Issue with optional explain annotations, serialized directly without
/// intermediate Value allocation.
#[derive(Serialize)]
struct AnnotatedIssue<'a> {
    #[serde(flatten)]
    issue: &'a Issue,
    #[serde(skip_serializing_if = "Option::is_none")]
    explanation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_provenance: Option<AnchorProvenance<'a>>,
}

/// Issues list: either plain references or annotated wrappers.
#[derive(Serialize)]
#[serde(untagged)]
enum IssuesList<'a> {
    Plain(&'a [Issue]),
    Annotated(Vec<AnnotatedIssue<'a>>),
}

/// Location in compact mode.
#[derive(Serialize)]
struct CompactLocation {
    line: usize,
    col: usize,
}

/// Calibration stats from translation verification.
#[cfg(feature = "translate")]
#[derive(Serialize)]
struct VerifyStats {
    api_ok: bool,
    matched: usize,
    unmatched: usize,
    no_english: usize,
}

/// Full-detail tool response (serialized directly, no intermediate Value).
#[derive(Serialize)]
struct FullOutput<'a> {
    accepted: bool,
    text: &'a str,
    issues: IssuesList<'a>,
    applied_fixes: usize,
    summary: &'a IssueSummary,
    gate: GateInfo,
    profile: &'a str,
    political_stance: &'a str,
    detected_script: &'a str,
    s2t_applied: bool,
    trace: &'a Trace,
    /// Present when fix_output != "full": indicates the `text` field contains
    /// a diff representation (search_replace blocks or patch JSON) instead of
    /// the full corrected text.
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_output_mode: Option<&'a str>,
    #[cfg(feature = "translate")]
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<VerifyStats>,
}

/// Compact tool response (serialized directly, no intermediate Value).
#[derive(Serialize)]
struct CompactOutput<'a> {
    accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    issues: Vec<CompactGroup>,
    applied_fixes: usize,
    summary: &'a IssueSummary,
    gate: GateInfo,
    profile: &'a str,
    detected_script: &'a str,
    s2t_applied: bool,
    /// Present when fix_output != "full": indicates the `text` field contains
    /// a diff representation instead of the full corrected text.
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_output_mode: Option<&'a str>,
    #[cfg(feature = "translate")]
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<VerifyStats>,
}

/// Count issues by severity.
fn build_summary(issues: &[Issue]) -> IssueSummary {
    let mut s = IssueSummary {
        errors: 0,
        warnings: 0,
        info: 0,
    };
    for issue in issues {
        match issue.severity {
            Severity::Error => s.errors += 1,
            Severity::Warning => s.warnings += 1,
            Severity::Info => s.info += 1,
        }
    }
    s
}

/// Parameters for build_check_output.
struct CheckOutputParams<'a> {
    result_text: &'a str,
    issues: &'a [Issue],
    applied_fixes: usize,
    max_errors: Option<u64>,
    max_warnings: Option<u64>,
    profile: Profile,
    stance_name: &'a str,
    detected_script: &'a str,
    /// Whether S2T conversion was applied (input was Simplified Chinese).
    s2t_applied: bool,
    trace: &'a Trace,
    explain: bool,
    output_mode: OutputMode,
    has_fixes: bool,
    /// Fix output mode: full text, search/replace blocks, or patch array.
    fix_output: FixOutputMode,
    /// Original text before fixes (needed for search_replace and patch modes).
    original_text: &'a str,
    /// Applied fix records for patch/search_replace output.
    fix_records: &'a [crate::fixer::AppliedFix],
    #[cfg(feature = "translate")]
    calibrate_result: Option<crate::engine::translate::CalibrateResult>,
}

/// Build the unified zhtw JSON response and wrap it in a CallToolResult.
///
/// Both the lint-only and fix paths produce the same output shape; only the
/// concrete values differ. Compact mode omits text (in lint-only), trace,
/// byte offsets/lengths, and deduplicates repeated issues.
///
/// Serializes typed structs directly to avoid intermediate `serde_json::Value`
/// allocations. Uses compact JSON by default; set `ZHTW_PRETTY=1` env var
/// for indented output during debugging.
fn build_check_output(params: &CheckOutputParams<'_>) -> CallToolResult {
    let summary = build_summary(params.issues);

    let max_err = params.max_errors.unwrap_or(0) as usize;
    let max_warn = params.max_warnings.unwrap_or(0) as usize;
    let gate_enabled = params.max_errors.is_some() || params.max_warnings.is_some();
    let accepted = params.max_errors.is_none_or(|_| summary.errors <= max_err)
        && params
            .max_warnings
            .is_none_or(|_| summary.warnings <= max_warn);

    let gate = GateInfo {
        enabled: gate_enabled,
        max_errors: max_err,
        residual_errors: summary.errors,
        max_warnings: max_warn,
        residual_warnings: summary.warnings,
    };

    #[cfg(feature = "translate")]
    let verify = params.calibrate_result.as_ref().map(|cr| VerifyStats {
        api_ok: cr.api_ok,
        matched: cr.matched,
        unmatched: cr.unmatched,
        no_english: cr.no_english,
    });

    // When fix_output is not Full and fixes were applied, replace the text
    // field with a diff representation to save output tokens.
    let diff_text: Option<String> = if params.has_fixes
        && params.fix_output != FixOutputMode::Full
        && !params.fix_records.is_empty()
    {
        Some(build_fix_diff(
            params.original_text,
            params.fix_records,
            params.fix_output,
        ))
    } else {
        None
    };
    let effective_text = diff_text.as_deref().unwrap_or(params.result_text);

    let fix_mode_label = if diff_text.is_some() {
        Some(params.fix_output.name())
    } else {
        None
    };

    let serialize_result = match params.output_mode {
        OutputMode::Full => {
            let issues = build_issues_list(params.issues, params.explain);
            let output = FullOutput {
                accepted,
                text: effective_text,
                issues,
                applied_fixes: params.applied_fixes,
                summary: &summary,
                gate,
                profile: params.profile.name(),
                political_stance: params.stance_name,
                detected_script: params.detected_script,
                s2t_applied: params.s2t_applied,
                trace: params.trace,
                fix_output_mode: fix_mode_label,
                #[cfg(feature = "translate")]
                verify,
            };
            serialize_output(&output)
        }
        OutputMode::Compact => {
            let issues = build_compact_groups(params.issues, params.explain);
            let output = CompactOutput {
                accepted,
                text: if params.has_fixes {
                    Some(effective_text)
                } else {
                    None
                },
                issues,
                applied_fixes: params.applied_fixes,
                summary: &summary,
                gate,
                profile: params.profile.name(),
                detected_script: params.detected_script,
                s2t_applied: params.s2t_applied,
                fix_output_mode: fix_mode_label,
                #[cfg(feature = "translate")]
                verify,
            };
            serialize_output(&output)
        }
        OutputMode::Tabular => {
            let tsv = build_tabular_output(
                accepted,
                params.issues,
                params.applied_fixes,
                &summary,
                params.has_fixes,
                effective_text,
                params.explain,
                fix_mode_label,
            );
            Ok(tsv)
        }
    };

    match serialize_result {
        Ok(json_str) => {
            if accepted {
                CallToolResult::text(json_str)
            } else {
                CallToolResult::error(json_str)
            }
        }
        Err(e) => {
            log::error!("failed to serialize check output: {e}");
            CallToolResult::error("internal server error".into())
        }
    }
}

/// Serialize to compact JSON by default; pretty-print when `ZHTW_PRETTY=1`.
fn serialize_output(output: &impl serde::Serialize) -> serde_json::Result<String> {
    if std::env::var_os("ZHTW_PRETTY").is_some_and(|v| v == "1") {
        serde_json::to_string_pretty(output)
    } else {
        serde_json::to_string(output)
    }
}

/// Build issues list for full output mode: either plain references (no explain)
/// or annotated wrappers with explanation and anchor provenance.
fn build_issues_list<'a>(issues: &'a [Issue], explain: bool) -> IssuesList<'a> {
    if explain {
        let annotated: Vec<AnnotatedIssue<'a>> = issues
            .iter()
            .map(|issue| {
                let explanation = build_explanation(issue);
                let anchor_provenance = if issue.anchor_match.is_some() {
                    Some(AnchorProvenance {
                        anchor_en: issue.english.as_deref(),
                        anchor_match: issue.anchor_match,
                    })
                } else {
                    None
                };
                AnnotatedIssue {
                    issue,
                    explanation,
                    anchor_provenance,
                }
            })
            .collect();
        IssuesList::Annotated(annotated)
    } else {
        IssuesList::Plain(issues)
    }
}

/// Build compact deduplicated issues array.
///
/// Groups issues by (found, rule_type, suggestions, severity) key. Each group
/// becomes one entry with count and locations. Serialized directly via
/// `#[derive(Serialize)]` on `CompactGroup` — no intermediate `Value` per group.
fn build_compact_groups(issues: &[Issue], explain: bool) -> Vec<CompactGroup> {
    use std::collections::BTreeMap;

    // Key: (found, rule_type, suggestions_joined, severity)
    // Include severity so that sampling can produce mixed-severity occurrences
    // of the same term without silently inheriting the first occurrence's level.
    // Uses shared IssueType::name() and Severity::name() from ruleset.rs.
    // We use BTreeMap for deterministic ordering.
    let mut groups: BTreeMap<(&str, &str, String, &str), CompactGroup> = BTreeMap::new();

    for issue in issues {
        let rt = issue.rule_type.name();
        let sug_key = issue.suggestions.join("|");
        let sev_key = issue.severity.name();
        let key = (issue.found.as_str(), rt, sug_key, sev_key);

        let group = groups.entry(key).or_insert_with(|| CompactGroup {
            found: issue.found.clone(),
            suggestions: issue.suggestions.clone(),
            rule_type: rt.to_string(),
            severity: issue.severity.name().to_string(),
            context: issue.context.clone(),
            english: issue.english.clone(),
            explanation: if explain {
                build_explanation(issue)
            } else {
                None
            },
            anchor_provenance: if explain && issue.anchor_match.is_some() {
                Some(AnchorProvenanceOwned {
                    anchor_en: issue.english.clone(),
                    anchor_match: issue.anchor_match,
                })
            } else {
                None
            },
            count: 0,
            locations: Vec::new(),
        });
        group.count += 1;
        group.locations.push(CompactLocation {
            line: issue.line,
            col: issue.col,
        });
    }

    groups.into_values().collect()
}

/// Escape tab, newline, and carriage return in a TSV field to prevent
/// column/row injection.  Returns a borrowed reference when no escaping
/// is needed, avoiding allocation on the common path.
pub fn escape_tsv_field(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes()
        .any(|b| b == b'\\' || b == b'\t' || b == b'\n' || b == b'\r')
    {
        let mut out = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                _ => out.push(ch),
            }
        }
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Deduplicated issue group shared by MCP tabular output and CLI tabular format.
///
/// Groups issues by (found, rule_type, suggestions, severity) key. Each group
/// stores shared fields once and collects per-occurrence locations.
pub struct IssueGroup {
    pub suggestions: Vec<String>,
    pub count: usize,
    pub locs: Vec<(usize, usize)>,
    pub explanation: Option<String>,
}

/// Issue grouping key: (found, rule_type, suggestions_joined, severity).
pub type IssueGroupKey<'a> = (&'a str, &'a str, String, &'a str);

/// Group issues by (found, rule_type, suggestions, severity) into a BTreeMap
/// for deterministic ordering. Optionally generates explanations per group.
pub fn group_issues<'a>(
    issues: &'a [Issue],
    explain: bool,
) -> std::collections::BTreeMap<IssueGroupKey<'a>, IssueGroup> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<IssueGroupKey<'a>, IssueGroup> = BTreeMap::new();
    for issue in issues {
        let rt = issue.rule_type.name();
        let sug_key = issue.suggestions.join("|");
        let sev = issue.severity.name();
        let key: IssueGroupKey<'a> = (issue.found.as_str(), rt, sug_key, sev);
        let entry = groups.entry(key).or_insert_with(|| IssueGroup {
            suggestions: issue.suggestions.clone(),
            count: 0,
            locs: Vec::new(),
            explanation: if explain {
                build_explanation(issue)
            } else {
                None
            },
        });
        entry.count += 1;
        entry.locs.push((issue.line, issue.col));
    }
    groups
}

/// Map full severity name to single-letter code for tabular output.
pub fn shorten_severity(sev: &str) -> &str {
    match sev {
        "error" => "E",
        "warning" => "W",
        "info" => "I",
        _ => sev,
    }
}

/// Map full issue type name to abbreviated code for tabular output.
pub fn shorten_type(rt: &str) -> &str {
    match rt {
        "political_coloring" => "pol",
        "cross_strait" => "cs",
        "typo" => "typo",
        "confusable" => "cf",
        "case" => "case",
        "punctuation" => "punc",
        "variant" => "v",
        "grammar" => "gram",
        _ => rt,
    }
}

/// Compress a list of (line, col) locations into a compact string.
///
/// When all locations share the same column, emits "L1,L4,L7:C" instead of
/// the verbose "1:C,4:C,7:C" form -- saves tokens on repeated issues.
pub fn compress_locations(locs: &[(usize, usize)]) -> String {
    use std::fmt::Write;
    if locs.is_empty() {
        return String::new();
    }
    if locs.len() == 1 {
        return format!("{}:{}", locs[0].0, locs[0].1);
    }
    // Check if all columns are identical.
    let first_col = locs[0].1;
    if locs.iter().all(|(_, c)| *c == first_col) {
        let mut s = String::new();
        for (i, (line, _)) in locs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(s, "{line}");
        }
        let _ = write!(s, ":{first_col}");
        s
    } else {
        locs.iter()
            .map(|(l, c)| format!("{l}:{c}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Build header-once TSV output for LLM-facing responses.
///
/// Eliminates JSON syntax tax: no repeated keys, braces, or quotes per issue.
/// Header row defines column semantics; data rows are tab-separated.
/// Achieves >=50% token reduction vs compact JSON on typical responses.
#[allow(clippy::too_many_arguments)]
fn build_tabular_output(
    accepted: bool,
    issues: &[Issue],
    applied_fixes: usize,
    summary: &IssueSummary,
    has_fixes: bool,
    result_text: &str,
    explain: bool,
    fix_output_mode: Option<&str>,
) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(256);

    // Meta line: key=value pairs, omitting zero-count fields to save tokens.
    let _ = write!(out, "#ok={}", accepted);
    if summary.errors > 0 {
        let _ = write!(out, "\terr={}", summary.errors);
    }
    if summary.warnings > 0 {
        let _ = write!(out, "\twarn={}", summary.warnings);
    }
    if summary.info > 0 {
        let _ = write!(out, "\tinfo={}", summary.info);
    }
    if applied_fixes > 0 {
        let _ = write!(out, "\tfix={}", applied_fixes);
    }
    if has_fixes {
        let _ = write!(out, "\ttxt={}", result_text.len());
    }
    if let Some(mode) = fix_output_mode {
        let _ = write!(out, "\tfix_fmt={mode}");
    }
    out.push('\n');

    let groups = group_issues(issues, explain);

    // Header row.
    if explain {
        out.push_str("found\tsug\ttype\tsev\tn\tloc\texpl\n");
    } else {
        out.push_str("found\tsug\ttype\tsev\tn\tloc\n");
    }

    // Data rows.  Use abbreviated severity (E/W/I) and rule type codes
    // (cs/cf/v/pol/typo/punc/case/gram) to reduce token count.
    // Escape tab/newline in data fields to prevent TSV injection.
    for ((found, rt, _, sev), group) in &groups {
        let found_safe = escape_tsv_field(found);
        let suggestions_str = group
            .suggestions
            .iter()
            .map(|s| escape_tsv_field(s))
            .collect::<Vec<_>>()
            .join(",");

        // Map full group-key names to abbreviated codes directly,
        // avoiding an O(groups*issues) scan that could also mismatch
        // when the same found term appears in multiple groups.
        let short_rt = shorten_type(rt);
        let short_sev = shorten_severity(sev);

        // Compress locations: if all share the same column, emit
        // "L1,L4,L7:C" instead of "L1:C,L4:C,L7:C".
        let locs_str = compress_locations(&group.locs);

        let _ = write!(
            out,
            "{found_safe}\t{suggestions_str}\t{short_rt}\t{short_sev}\t{}\t{locs_str}",
            group.count,
        );
        if explain {
            out.push('\t');
            if let Some(expl) = &group.explanation {
                out.push_str(&escape_tsv_field(expl));
            }
        }
        out.push('\n');
    }

    // If fixes were applied, append the fixed text after a separator.
    if has_fixes {
        out.push_str("#text\n");
        out.push_str(result_text);
    }

    out
}

/// Build diff representation of fixes for token-efficient output.
///
/// For SearchReplace mode: emits <<<<<<< SEARCH / ======= REPLACE / >>>>>>> END
/// blocks that LLMs can parse reliably without byte arithmetic.
/// For Patch mode: emits a JSON patches array with byte offsets, sorted
/// descending by offset so clients can apply in order without index shifting.
fn build_fix_diff(
    original_text: &str,
    fix_records: &[crate::fixer::AppliedFix],
    mode: FixOutputMode,
) -> String {
    match mode {
        FixOutputMode::SearchReplace => {
            let mut out = String::with_capacity(fix_records.len() * 80);
            for fix in fix_records {
                // Safe slice: get() returns None if offset/end are out of
                // bounds or not on UTF-8 char boundaries.
                if let Some(found) = original_text.get(fix.offset..fix.offset + fix.old_len) {
                    out.push_str("<<<<<<< SEARCH\n");
                    out.push_str(found);
                    out.push_str("\n======= REPLACE\n");
                    out.push_str(&fix.replacement);
                    out.push_str("\n>>>>>>> END\n");
                }
            }
            out
        }
        FixOutputMode::Patch => {
            use std::fmt::Write;
            // TSV patch format: header-once, sorted descending by offset so
            // clients can apply in order without index shifting.
            let mut patches: Vec<(usize, usize, &str, &str)> = fix_records
                .iter()
                .filter_map(|fix| {
                    let found = original_text.get(fix.offset..fix.offset + fix.old_len)?;
                    Some((fix.offset, fix.old_len, found, fix.replacement.as_str()))
                })
                .collect();
            patches.sort_by(|a, b| b.0.cmp(&a.0));

            let mut out = String::with_capacity(patches.len() * 40);
            let _ = writeln!(out, "#patches={}", patches.len());
            out.push_str("offset\tlength\tfound\treplacement\n");
            for (offset, length, found, replacement) in &patches {
                let _ = writeln!(
                    out,
                    "{offset}\t{length}\t{}\t{}",
                    escape_tsv_field(found),
                    escape_tsv_field(replacement),
                );
            }
            out
        }
        FixOutputMode::Full => {
            // Should never reach here; caller guards.
            String::new()
        }
    }
}

/// Helper for compact mode issue grouping.
#[derive(Serialize)]
struct CompactGroup {
    found: String,
    suggestions: Vec<String>,
    rule_type: String,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    english: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    explanation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor_provenance: Option<AnchorProvenanceOwned>,
    count: usize,
    locations: Vec<CompactLocation>,
}

// Tool definitions (JSON Schema for zhtw)

fn tool_definitions() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "zhtw".into(),
        description: "Lint/fix/gate zh-TW text. Auto-converts Simplified Chinese to Traditional before applying rules. Use verify=true to calibrate issues via Google Translate anchor matching.".into(),
        input_schema: {
            let mut props = serde_json::Map::new();
            props.insert("text".into(), json!({ "type": "string" }));
            props.insert("fix_mode".into(), json!({
                "type": "string",
                "enum": ["none", "orthographic", "lexical_safe", "lexical_contextual"]
            }));
            props.insert("max_errors".into(), json!({ "type": "integer" }));
            props.insert("max_warnings".into(), json!({ "type": "integer" }));
            props.insert("profile".into(), json!({
                "type": "string",
                "enum": ["default", "strict_moe", "ui_strings"]
            }));
            props.insert("content_type".into(), json!({
                "type": "string",
                "enum": ["plain", "markdown", "markdown-scan-code", "yaml"]
            }));
            props.insert("political_stance".into(), json!({
                "type": "string",
                "enum": ["roc_centric", "international", "neutral"]
            }));
            props.insert("ignore_terms".into(), json!({
                "type": "array",
                "items": { "type": "string" }
            }));
            props.insert("explain".into(), json!({ "type": "boolean" }));
            props.insert("fix_output".into(), json!({
                "type": "string",
                "enum": ["full", "search_replace", "patch"],
                "description": "Fix output format: full text (default), search/replace blocks, or patch array with byte offsets"
            }));
            #[cfg(feature = "translate")]
            props.insert("verify".into(), json!({
                "type": "boolean",
                "description": "Anchor-verify issues via Google Translate"
            }));
            props.insert("output".into(), json!({
                "type": "string",
                "enum": ["full", "compact", "tabular"]
            }));
            json!({
                "type": "object",
                "properties": Value::Object(props),
                "required": ["text"]
            })
        },
        annotations: Some(ToolAnnotations {
            destructive: None,
            idempotent: Some(true),
            read_only: Some(true),
            open_world_hint: None,
        }),
    }]
}
