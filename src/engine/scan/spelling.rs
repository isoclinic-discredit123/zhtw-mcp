// Spelling rule scan using Aho-Corasick.
//
// Uses daachorse's CharwiseDoubleArrayAhoCorasick when available (charwise
// transitions reduce state count ~3x for CJK patterns).  Falls back to
// BurntSushi's bytewise Aho-Corasick otherwise.
//
// Context-clue checking uses a pre-scan approach: a separate bytewise AC
// automaton (built from all unique context_clue and negative_context_clue
// strings) is run once over the full text before the spelling scan.  Each
// spelling match then checks clue presence via O(log H) binary search in
// the pre-scan hit list, eliminating per-match MMSEG segmentation.
//
// The clue AC uses MatchKind::Standard with overlapping iteration so that
// substring clues (e.g. "下拉" inside "下拉菜單") are all captured.

use crate::engine::excluded::{is_excluded, ByteRange};
use crate::engine::zhtype::ChineseType;
use crate::rules::ruleset::{Issue, IssueType, ProfileConfig, RuleType};

use super::{
    already_correct_form, clamp_at_excluded, Scanner, CONTEXT_WINDOW_CHARS, MIN_SCAN_CLUE_MATCHES,
};

/// Pre-computed clue hit list: sorted by start byte offset.
/// Each entry is (start_byte, end_byte, clue_string_index).
type ClueHits = Vec<(usize, usize, u16)>;

impl Scanner {
    /// Pre-scan text for all context-clue strings, filtering out hits inside
    /// excluded ranges.  Returns a sorted list of (start, end, clue_index).
    ///
    /// Uses overlapping iteration so that substring clues (e.g. "下拉" within
    /// "下拉菜單") are all captured — LeftmostLongest non-overlapping iteration
    /// would swallow the shorter match.
    fn build_clue_hits(&self, text: &str, excluded: &[ByteRange]) -> ClueHits {
        let Some(ref clue_ac) = self.clue_ac else {
            return Vec::new();
        };
        let mut hits: ClueHits = Vec::new();
        for mat in clue_ac.find_overlapping_iter(text) {
            if is_excluded(mat.start(), mat.end(), excluded) {
                continue;
            }
            hits.push((mat.start(), mat.end(), mat.pattern().as_usize() as u16));
        }
        // Overlapping iterator yields matches ordered by end position;
        // re-sort by start offset for binary-search proximity checks.
        hits.sort_unstable_by_key(|&(start, _, _)| start);
        hits
    }

    /// Spelling rule scan using Aho-Corasick.
    ///
    /// Uses charwise double-array AC (daachorse) when available for ~3x fewer
    /// state transitions on CJK text.  Falls back to bytewise AC (BurntSushi).
    ///
    /// Before emitting an issue, checks whether the surrounding text already
    /// contains a correct form that is a superstring of the wrong term (e.g.
    /// "演算法" contains "算法").  This prevents false positives that would
    /// otherwise cause apply_fixes to produce gibberish like "演演算法".
    pub(crate) fn scan_spelling(
        &self,
        text: &str,
        excluded: &[ByteRange],
        zh_type: ChineseType,
        issues: &mut Vec<Issue>,
        cfg: &ProfileConfig,
    ) {
        // Lazy clue pre-scan: defer the O(n) clue AC pass until a matched
        // rule actually needs context-clue checking.  Only ~7% of rules
        // have context_clues, so most matches skip the clue AC entirely.
        let mut clue_hits_cache: Option<ClueHits> = None;

        // Dispatch to charwise AC when available; fall back to bytewise.
        if let Some(ref cw_ac) = self.spelling_ac_charwise {
            for mat in cw_ac.leftmost_find_iter(text) {
                self.process_spelling_match(
                    text,
                    excluded,
                    zh_type,
                    issues,
                    cfg,
                    &mut clue_hits_cache,
                    mat.start(),
                    mat.end(),
                    mat.value(),
                );
            }
        } else if let Some(ref bw_ac) = self.spelling_ac_bytewise {
            for mat in bw_ac.find_iter(text) {
                self.process_spelling_match(
                    text,
                    excluded,
                    zh_type,
                    issues,
                    cfg,
                    &mut clue_hits_cache,
                    mat.start(),
                    mat.end(),
                    mat.pattern().as_usize(),
                );
            }
        }
    }

    /// Process a single spelling AC match.  Shared between charwise and
    /// bytewise code paths.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn process_spelling_match(
        &self,
        text: &str,
        excluded: &[ByteRange],
        zh_type: ChineseType,
        issues: &mut Vec<Issue>,
        cfg: &ProfileConfig,
        clue_hits_cache: &mut Option<ClueHits>,
        start: usize,
        end: usize,
        rule_idx: usize,
    ) {
        let rule = &self.spelling_rules[rule_idx];

        // Variant rules are character-form corrections (裏→裡, 着→著) that
        // only apply in Traditional Chinese context.  Skip when the profile
        // disables variant_normalization or when text is Simplified.
        if rule.rule_type == RuleType::Variant
            && (!cfg.variant_normalization || zh_type == ChineseType::Simplified)
        {
            return;
        }

        // AI filler rules are profile-gated: only fire when ai_filler_detection
        // is enabled (de_ai / strict_moe profiles).
        if rule.rule_type == RuleType::AiFiller && !cfg.ai_filler_detection {
            return;
        }

        // Political stance filtering: suppress political_coloring rules
        // based on the active stance sub-profile.
        if rule.rule_type == RuleType::PoliticalColoring
            && !cfg.political_stance.allows_rule(&rule.from)
        {
            return;
        }

        // Skip if the match overlaps any excluded range.
        if is_excluded(start, end, excluded) {
            return;
        }

        // Skip if surrounding text already contains a correct form.
        if already_correct_form(text, start, rule) {
            return;
        }

        // Word-boundary check: skip if a known dictionary word straddles
        // either edge of the AC match.  This catches false positives where
        // the matched pattern spans two distinct words — e.g. "積分" found
        // inside "累積分佈" (累積 + 分佈), "程序" inside "排程序列"
        // (排程 + 序列), "導出" inside "引導出" (引導 + 出).
        if self.segmenter.word_straddles_boundary(text, start)
            || self.segmenter.word_straddles_boundary(text, end)
        {
            return;
        }

        // Exception check: skip if the match falls inside an exception
        // phrase.  Applies to all rule types — variant, cross_strait,
        // typo, confusable, etc.  (e.g. chess term 下著 keeps 着; 分類
        // keeps 類 from firing as an OOP-class warning).
        if let Some(ref exceptions) = rule.exceptions {
            let in_exception = exceptions.iter().any(|exc| {
                for (pos, _) in exc.match_indices(&rule.from) {
                    if let Some(exc_start) = start.checked_sub(pos) {
                        let exc_end = exc_start + exc.len();
                        if text.get(exc_start..exc_end) == Some(exc.as_str()) {
                            return true;
                        }
                    }
                }
                false
            });
            if in_exception {
                return;
            }
        }

        // Context-clue gate via pre-scan hits.
        //
        // The clue AC pass is deferred until a matched rule actually needs
        // context-clue checking (~7% of rules).  This avoids the O(n) clue
        // AC scan entirely for the 93% of matches that never check clues.
        let has_pos = self.rule_pos_clue_ids[rule_idx].is_some();
        let has_neg = self.rule_neg_clue_ids[rule_idx].is_some();

        if has_pos || has_neg {
            let clue_hits =
                clue_hits_cache.get_or_insert_with(|| self.build_clue_hits(text, excluded));

            // Compute byte-offset window matching surrounding_window_bounded
            // semantics: +-CONTEXT_WINDOW_CHARS characters, clamped at excluded
            // range boundaries.
            let (win_start, win_end) = context_byte_window(text, start, end, excluded);

            if let Some(ref pos_ids) = self.rule_pos_clue_ids[rule_idx] {
                let matches = count_clues_in_window(clue_hits, pos_ids, win_start, win_end);
                if matches < MIN_SCAN_CLUE_MATCHES {
                    return;
                }
            }

            if let Some(ref neg_ids) = self.rule_neg_clue_ids[rule_idx] {
                let any_neg = count_clues_in_window(clue_hits, neg_ids, win_start, win_end);
                if any_neg > 0 {
                    return;
                }
            }
        }

        // AiFiller deletion rules: extend span to consume trailing fullwidth
        // punctuation (，：) so that a single base rule handles all variants
        // without leaving dangling punctuation after fix application.
        // Guard: do not extend into an excluded range (code block, URL).
        let end = if rule.is_deletion_rule() {
            match text[end..].chars().next() {
                Some(c @ ('\u{FF0C}' | '\u{FF1A}'))
                    if !is_excluded(end, end + c.len_utf8(), excluded) =>
                {
                    end + c.len_utf8()
                }
                _ => end,
            }
        } else {
            end
        };

        let mut issue = Issue::new(
            start,
            end - start,
            &text[start..end],
            self.spelling_suggestions[rule_idx].clone(),
            IssueType::from(rule.rule_type),
            rule.rule_type.default_severity(),
        );
        issue.context.clone_from(&rule.context);
        issue.english.clone_from(&rule.english);
        issue.context_clues.clone_from(&rule.context_clues);
        issues.push(issue);
    }
}

/// Compute the byte-offset window for context-clue proximity checks.
///
/// Walks ±CONTEXT_WINDOW_CHARS characters from the match boundaries (same
/// as surrounding_window), then clamps at excluded-range boundaries (same
/// as surrounding_window_bounded).  Returns (win_start, win_end) in byte
/// offsets suitable for direct comparison against clue_hits positions.
fn context_byte_window(
    text: &str,
    match_start: usize,
    match_end: usize,
    excluded: &[ByteRange],
) -> (usize, usize) {
    let bytes = text.as_bytes();

    // Find paragraph boundaries (\n\n or \r\n\r\n) around the match.
    // Context clues from a different paragraph are semantically irrelevant
    // and can cause false triggers/suppressions.
    //
    // Clamp the search range to CONTEXT_WINDOW_CHARS * 4 bytes (max possible
    // window extent for CJK text) to avoid O(N) scans per match.
    let max_search = CONTEXT_WINDOW_CHARS * 4;
    let para_start = {
        let search_start = match_start.saturating_sub(max_search);
        let search = &bytes[search_start..match_start];
        find_last_paragraph_break(search).map_or(0, |pos| search_start + pos + 1)
    };
    let para_end = {
        let search_end = (match_end + max_search).min(text.len());
        let search = &bytes[match_end..search_end];
        find_first_paragraph_break(search).map_or(text.len(), |pos| match_end + pos)
    };

    // Walk backward CONTEXT_WINDOW_CHARS characters, clamped at paragraph start.
    let mut byte_start = match_start;
    for _ in 0..CONTEXT_WINDOW_CHARS {
        if byte_start <= para_start {
            byte_start = para_start;
            break;
        }
        byte_start = text.floor_char_boundary(byte_start - 1);
    }
    byte_start = byte_start.max(para_start);

    // Walk forward CONTEXT_WINDOW_CHARS characters, clamped at paragraph end.
    let mut byte_end = match_end;
    for _ in 0..CONTEXT_WINDOW_CHARS {
        if byte_end >= para_end {
            byte_end = para_end;
            break;
        }
        byte_end = text.ceil_char_boundary(byte_end + 1);
    }
    byte_end = byte_end.min(para_end);

    if excluded.is_empty() {
        return (byte_start, byte_end);
    }

    clamp_at_excluded(text, byte_start, byte_end, match_start, match_end, excluded)
}

/// Find the byte offset of the last paragraph break (`\n\n`) in `bytes`.
/// Returns the offset of the second `\n` (i.e. the byte just before the
/// new paragraph starts).  Handles `\r\n\r\n` as well.
fn find_last_paragraph_break(bytes: &[u8]) -> Option<usize> {
    // Scan backward for \n\n.
    let len = bytes.len();
    if len < 2 {
        return None;
    }
    let mut i = len - 1;
    while i > 0 {
        if bytes[i] == b'\n' && bytes[i - 1] == b'\n' {
            return Some(i);
        }
        // Handle \r\n\r\n: bytes[i]=\n, bytes[i-1]=\r, bytes[i-2]=\n
        if i >= 2 && bytes[i] == b'\n' && bytes[i - 1] == b'\r' && bytes[i - 2] == b'\n' {
            return Some(i);
        }
        i -= 1;
    }
    None
}

/// Find the byte offset of the first paragraph break (`\n\n`) in `bytes`.
/// Returns the offset of the first `\n` in the pair.
fn find_first_paragraph_break(bytes: &[u8]) -> Option<usize> {
    let len = bytes.len();
    if len < 2 {
        return None;
    }
    for i in 0..len - 1 {
        if bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            return Some(i);
        }
        // \n\r\n also counts.
        if i + 2 < len && bytes[i] == b'\n' && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
            return Some(i);
        }
    }
    None
}

/// Count how many distinct clue IDs from `needle_ids` appear in `clue_hits`
/// fully within the byte window [win_start, win_end).
///
/// A clue hit is "within" the window only if both its start and end offsets
/// fall inside [win_start, win_end) — this prevents clues that start inside
/// the window but bleed past the right boundary from being counted.
///
/// Uses binary search to skip to the window start, then linear scan within
/// the window.  For typical documents (H ≈ 100-500 clue hits, C ≈ 3-8 clues
/// per rule), this is effectively O(C + window_hits).
fn count_clues_in_window(
    clue_hits: &ClueHits,
    needle_ids: &[u16],
    win_start: usize,
    win_end: usize,
) -> usize {
    if clue_hits.is_empty() || needle_ids.is_empty() {
        return 0;
    }
    // Binary search to the first hit at or after win_start.
    let lo = clue_hits.partition_point(|&(off, _, _)| off < win_start);
    needle_ids
        .iter()
        .filter(|&&nid| {
            clue_hits[lo..]
                .iter()
                .take_while(|&&(off, _, _)| off < win_end)
                .any(|&(_, end, cid)| cid == nid && end <= win_end)
        })
        .count()
}
