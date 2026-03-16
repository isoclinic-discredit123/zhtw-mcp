# Internals

## Script detection

The scanner detects Traditional vs. Simplified Chinese by counting exclusive characters. Variant rules (裏→裡, 着→著) are skipped for Simplified input. When detection is `Unknown`, variant rules still fire (conservative default).

## Processing pipeline

1. NFC normalization with byte-offset mapping
2. Content-type dispatch: Markdown (pulldown-cmark), YAML (key token exclusion), plain text (regex exclusion). `MarkdownScanCode` variant also lints inside fenced code blocks.
3. Inline suppression markers (`<!-- zhtw:ignore-next-line -->`, `<!-- zhtw:ignore-block/end-ignore -->`)
4. Spelling pass: dual Aho-Corasick automata (leftmost-longest for spelling, case-insensitive for case rules); context-clue AC pre-scan for rules with `context_clues` or `negative_context_clues`
5. Punctuation pass: full-width conversion, CN curly quotes, enumeration comma, quote hierarchy, CJK spacing
6. Variant pass: character variant normalization with exception phrase checking
7. Overlap resolution: longer match wins, higher severity on tie
8. Profile filtering (e.g., `臺`/`台` only in `strict_moe`)
9. Sampling (optional): ambiguous terms escalated to host LLM

## Design decisions

- No async runtime by default. Synchronous stdio with background thread + mpsc for timeout-bounded sampling. Optional `--features async-transport` for tokio.
- Pure Rust, no C/C++ dependencies. MMSEG segmenter builds its dictionary from ruleset vocabulary at construction time.
- Byte-safe edits: positions from pulldown-cmark event ranges map back to original byte offsets.
- JSON ruleset (`assets/ruleset.json`) embedded via `include_str!`. Runtime overrides in platform config directory.
- SHA-256 trace IDs for reproducibility. No `uuid` crate dependency.
- Small release binary (~3 MB on x86-64 Linux, LTO + strip).
- Sampling (step 9) only activates when running as an MCP server inside an AI assistant. The standalone CLI skips sampling and keeps ambiguous issues at their original severity.
- Incremental scan cache (BLAKE3-keyed, 24h TTL, 2000-entry cap) skips re-scanning unchanged files in lint-only CLI mode. Disabled for `--fix`, `--verify`, and stdin. MCP path does not use the cache (stateless by design).
- Built-in SC→TC converter (`s2t.rs` + `s2t_data.rs`) eliminates the OpenCC runtime dependency for the `convert` subcommand.
- Anchor calibration (`translate.rs`) annotates ambiguous issues with `anchor_match: Option<bool>` (confirmed/unconfirmed/no-signal) via synonym table and LCP stem matching. Fails open on API error (severity preserved).

## Testing

```bash
cargo test                             # all tests
cargo test engine::scan                # specific module
cargo test --test scanner_integration  # integration tests (scanner behavior)
cargo test --test e2e_mcp              # E2E: JSON-RPC round-trip
cargo test --test vocabulary_expansion # political nouns, IT terms, context clues
cargo test --test cli_lint             # CLI: exit codes, formats, fix, SARIF, baseline
cargo test --test anchor_benchmark -- --ignored  # anchor calibration (requires network)
cargo test --test fix_tier_benchmark   # fix tier coverage
cargo clippy                           # must be warning-free
cargo fmt --check
```
