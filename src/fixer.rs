// Fix application: apply suggested corrections to source text.
//
// Three modes:
//   - None: lint only, no fixes applied.
//   - Safe: only apply fixes where suggestions.len() == 1 (unambiguous)
//     and no context_clues are present (or context is confirmed).
//   - Aggressive: pick suggestions[0]; for rules with context_clues,
//     check if 2+ clue words appear in surrounding text before applying.
//
// Fixes are applied in a single forward pass (ascending offset order).

#[cfg(test)]
use crate::engine::scan::surrounding_window;
use crate::engine::segment::Segmenter;
use crate::rules::ruleset::{Issue, IssueType};

/// Fix mode controlling ambiguity handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixMode {
    /// Lint only — no fixes applied.
    None,
    /// Only apply unambiguous fixes (exactly one suggestion, no context ambiguity).
    Safe,
    /// Always apply first suggestion; for ambiguous rules with context_clues,
    /// apply only when context clues confirm the intended meaning.
    Aggressive,
}

/// Record of a single fix applied to the text.
#[derive(Debug, Clone)]
pub struct AppliedFix {
    /// Byte offset in the original text where the replacement was written.
    pub offset: usize,
    /// Byte length of the original span that was replaced.
    pub old_len: usize,
    /// The replacement string that was written.
    pub replacement: String,
}

/// Result of applying fixes to text.
#[derive(Debug, Clone)]
pub struct FixResult {
    /// The corrected text.
    pub text: String,
    /// Number of fixes applied.
    pub applied: usize,
    /// Number of issues skipped (ambiguous in Safe mode, or in excluded regions).
    pub skipped: usize,
    /// Detailed record of each applied fix, stored in ascending offset
    /// order (forward pass). Used for position-based convergence
    /// suppression and exact offset remapping after re-scan.
    pub applied_fixes: Vec<AppliedFix>,
}

/// Minimum context clue words for aggressive fixer: confusable rules need
/// higher confidence (2 clues) because both forms are valid in different
/// contexts. Cross-strait and other rule types need only 1 clue because
/// the match itself is already a strong signal of incorrect regional usage.
const MIN_CLUE_MATCHES_CONFUSABLE: usize = 2;
const MIN_CLUE_MATCHES_DEFAULT: usize = 1;

/// Apply fixes to text based on the given issues.
///
/// Convenience wrapper that calls [apply_fixes_with_context] without a
/// segmenter.  Context-clue-dependent rules are treated as ambiguous.
pub fn apply_fixes(
    text: &str,
    issues: &[Issue],
    mode: FixMode,
    excluded_offsets: &[(usize, usize)],
) -> FixResult {
    apply_fixes_with_context(text, issues, mode, excluded_offsets, None)
}

/// Apply fixes to text using an optional segmenter for context-clue analysis.
///
/// Issues must be sorted by offset (ascending) and non-overlapping
/// (guaranteed by the scanner's resolve_overlaps pass).  Fixes are
/// applied in a single forward pass (ascending offset order): chunks of
/// unchanged text are copied between replacement spans, yielding O(N).
///
/// When a Segmenter is provided, rules with context_clues are checked
/// against the surrounding text:
///   - aggressive mode: apply if MIN_CLUE_MATCHES or more clue words
///     are found in a window around the match; otherwise skip.
///   - safe mode: always skip rules that have context_clues (ambiguous).
pub fn apply_fixes_with_context(
    text: &str,
    issues: &[Issue],
    mode: FixMode,
    excluded_offsets: &[(usize, usize)],
    segmenter: Option<&Segmenter>,
) -> FixResult {
    let mut out = String::with_capacity(text.len());
    let mut applied = 0usize;
    let mut skipped = 0usize;
    let mut applied_fixes = Vec::new();
    // Byte position up to which we have already copied into `out`.
    let mut cursor: usize = 0;

    // Issues are already sorted ascending by offset and non-overlapping
    // (scanner's resolve_overlaps guarantees this).  Iterate forward,
    // copying unchanged gaps and appending replacements.
    for issue in issues {
        let Some(end) = issue.offset.checked_add(issue.length) else {
            log::warn!(
                "skipping malformed issue at offset {}: length overflow",
                issue.offset
            );
            skipped += 1;
            continue;
        };

        // Skip overlapping issues: grammar issues are appended after
        // overlap resolution and may overlap each other (e.g. 對X進行Y
        // overlaps the inner 進行Y).  The fixer must not apply both.
        if issue.offset < cursor {
            skipped += 1;
            continue;
        }

        // Skip if the issue span overlaps any excluded region.
        if excluded_offsets
            .iter()
            .any(|&(s, e)| issue.offset < e && end > s)
        {
            skipped += 1;
            continue;
        }

        // Context-clue check: rules with non-empty context_clues are ambiguous.
        // Safe mode always skips them; aggressive mode applies only when a
        // segmenter confirms enough clue words in the surrounding text.
        //
        // Threshold is type-aware: confusable rules (both forms valid in
        // different contexts) need 2 clues for confidence; cross-strait and
        // other rules need only 1 (the match itself is a strong regional
        // signal, one nearby clue is sufficient to confirm domain).
        let has_clues = issue.context_clues.as_ref().is_some_and(|c| !c.is_empty());
        if has_clues {
            let min_clues = if issue.rule_type == IssueType::Confusable {
                MIN_CLUE_MATCHES_CONFUSABLE
            } else {
                MIN_CLUE_MATCHES_DEFAULT
            };
            let confirmed = mode == FixMode::Aggressive
                && segmenter.is_some_and(|seg| {
                    let excluded_ranges: Vec<crate::engine::excluded::ByteRange> = excluded_offsets
                        .iter()
                        .map(|&(start, end)| crate::engine::excluded::ByteRange { start, end })
                        .collect();
                    let window = crate::engine::scan::surrounding_window_bounded(
                        text,
                        issue.offset,
                        end,
                        &excluded_ranges,
                    );
                    let clue_strs: Vec<&str> = issue
                        .context_clues
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|s| s.as_str())
                        .collect();
                    seg.count_context_clues(window, &clue_strs) >= min_clues
                });
            if !confirmed {
                skipped += 1;
                continue;
            }
        }

        let rep = match mode {
            FixMode::Safe if issue.suggestions.len() == 1 => Some(&issue.suggestions[0]),
            FixMode::Aggressive => issue.suggestions.first(),
            _ => None,
        };
        let Some(rep) = rep.filter(|_| end <= text.len()) else {
            skipped += 1;
            continue;
        };

        out.push_str(&text[cursor..issue.offset]);
        out.push_str(rep);
        cursor = end;
        applied_fixes.push(AppliedFix {
            offset: issue.offset,
            old_len: issue.length,
            replacement: rep.clone(),
        });
        applied += 1;
    }

    // Copy the remaining tail after the last fix (or the entire text if
    // no fixes were applied).
    out.push_str(&text[cursor..]);

    FixResult {
        text: out,
        applied,
        skipped,
        applied_fixes,
    }
}

/// Map an original-text byte offset to its position in the fixed text.
///
/// Accumulates byte deltas (replacement.len() - old_len) from all applied
/// fixes whose original offset is strictly before orig_offset.  All fix
/// offsets are in original-text coordinates and non-overlapping.
pub fn remap_to_post_fix(orig_offset: usize, applied_fixes: &[AppliedFix]) -> usize {
    let mut delta: isize = 0;
    for fix in applied_fixes {
        if fix.offset < orig_offset {
            delta += fix.replacement.len() as isize - fix.old_len as isize;
        }
    }
    let result = orig_offset as isize + delta;
    debug_assert!(result >= 0, "remap produced negative offset");
    result.max(0) as usize
}

/// Remove re-scan issues whose byte range overlaps a region written by the fixer.
///
/// After applying fixes and re-scanning, the fixer may have introduced new
/// text that triggers rules (convergent chain).  These are noise: the fixer
/// already chose the best replacement.  This function suppresses them by
/// checking each re-scan issue against the post-fix byte ranges of applied
/// fixes.
pub fn suppress_convergent_issues(issues: &mut Vec<Issue>, applied_fixes: &[AppliedFix]) {
    if applied_fixes.is_empty() {
        return;
    }
    let fix_ranges: Vec<(usize, usize)> = applied_fixes
        .iter()
        .map(|fix| {
            let post = remap_to_post_fix(fix.offset, applied_fixes);
            (post, post + fix.replacement.len())
        })
        .collect();
    issues.retain(|issue| {
        let issue_end = issue.offset + issue.length;
        !fix_ranges.iter().any(|&(start, end)| {
            if start == end {
                // Zero-length deletion: suppress issues touching this offset.
                issue.offset <= start && issue_end > start
            } else {
                issue.offset < end && issue_end > start
            }
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::ruleset::{IssueType, Severity};

    fn make_issue(offset: usize, found: &str, suggestions: Vec<&str>) -> Issue {
        Issue::new(
            offset,
            found.len(),
            found,
            suggestions.into_iter().map(String::from).collect(),
            IssueType::CrossStrait,
            Severity::Warning,
        )
    }

    fn make_issue_with_clues(
        offset: usize,
        found: &str,
        suggestions: Vec<&str>,
        clues: Vec<&str>,
    ) -> Issue {
        Issue::new(
            offset,
            found.len(),
            found,
            suggestions.into_iter().map(String::from).collect(),
            IssueType::Confusable,
            Severity::Warning,
        )
        .with_english("program")
        .with_context_clues(clues.into_iter().map(String::from).collect())
    }

    #[test]
    fn safe_mode_single_suggestion() {
        let text = "這個軟件很好用";
        let issues = vec![make_issue(6, "軟件", vec!["軟體"])];
        let result = apply_fixes(text, &issues, FixMode::Safe, &[]);
        assert_eq!(result.text, "這個軟體很好用");
        assert_eq!(result.applied, 1);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn safe_mode_multiple_suggestions_skipped() {
        let text = "這個視頻很好看";
        let issues = vec![make_issue(6, "視頻", vec!["影片", "影音"])];
        let result = apply_fixes(text, &issues, FixMode::Safe, &[]);
        assert_eq!(result.text, text); // unchanged
        assert_eq!(result.applied, 0);
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn aggressive_mode_picks_first() {
        let text = "這個視頻很好看";
        let issues = vec![make_issue(6, "視頻", vec!["影片", "影音"])];
        let result = apply_fixes(text, &issues, FixMode::Aggressive, &[]);
        assert_eq!(result.text, "這個影片很好看");
        assert_eq!(result.applied, 1);
    }

    #[test]
    fn multiple_fixes_end_to_start() {
        // "軟件" at byte 6, "內存" somewhere after.
        let text = "這個軟件的內存";
        let issues = vec![
            make_issue(6, "軟件", vec!["軟體"]),
            make_issue(15, "內存", vec!["記憶體"]),
        ];
        let result = apply_fixes(text, &issues, FixMode::Safe, &[]);
        assert_eq!(result.text, "這個軟體的記憶體");
        assert_eq!(result.applied, 2);
    }

    #[test]
    fn excluded_offset_skipped() {
        let text = "這個軟件很好用";
        let issues = vec![make_issue(6, "軟件", vec!["軟體"])];
        // Mark offset 6 as excluded (inside a code fence, say).
        let result = apply_fixes(text, &issues, FixMode::Safe, &[(0, 21)]);
        assert_eq!(result.text, text);
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn empty_issues() {
        let text = "hello";
        let result = apply_fixes(text, &[], FixMode::Safe, &[]);
        assert_eq!(result.text, "hello");
        assert_eq!(result.applied, 0);
    }

    // -- Context clue tests --

    #[test]
    fn safe_mode_skips_issues_with_context_clues() {
        let text = "我需要編寫一個程序來執行";
        let offset = text.find("程序").unwrap();
        let issues = vec![make_issue_with_clues(
            offset,
            "程序",
            vec!["程式"],
            vec!["編寫", "代碼", "執行", "開發"],
        )];
        let result = apply_fixes(text, &issues, FixMode::Safe, &[]);
        assert_eq!(result.text, text); // unchanged -- safe mode refuses context-clue rules
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn aggressive_with_segmenter_applies_when_clues_match() {
        // "程序" with context clues; surrounding text contains "編寫" and "執行"
        let text = "我需要編寫一個程序來執行";
        let offset = text.find("程序").unwrap();
        let issues = vec![make_issue_with_clues(
            offset,
            "程序",
            vec!["程式"],
            vec!["編寫", "代碼", "執行", "開發"],
        )];
        let seg = Segmenter::new(
            ["編寫", "代碼", "執行", "開發", "程序", "程式"]
                .iter()
                .map(|s| s.to_string()),
        );
        let result = apply_fixes_with_context(text, &issues, FixMode::Aggressive, &[], Some(&seg));
        assert_eq!(result.text, "我需要編寫一個程式來執行");
        assert_eq!(result.applied, 1);
    }

    #[test]
    fn aggressive_with_segmenter_skips_when_clues_insufficient() {
        // "程序" but surrounding text has zero matching clues
        let text = "這個程序很重要";
        let offset = text.find("程序").unwrap();
        let issues = vec![make_issue_with_clues(
            offset,
            "程序",
            vec!["程式"],
            vec!["編寫", "代碼", "執行", "開發"],
        )];
        let seg = Segmenter::new(
            ["編寫", "代碼", "執行", "開發", "程序", "程式"]
                .iter()
                .map(|s| s.to_string()),
        );
        let result = apply_fixes_with_context(text, &issues, FixMode::Aggressive, &[], Some(&seg));
        assert_eq!(result.text, text); // unchanged -- insufficient clues
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn aggressive_without_segmenter_skips_clue_rules() {
        // Without a segmenter, aggressive mode cannot verify context clues
        // and skips the rule (same as safe mode).
        let text = "這個程序很重要";
        let offset = text.find("程序").unwrap();
        let issues = vec![make_issue_with_clues(
            offset,
            "程序",
            vec!["程式"],
            vec!["編寫", "代碼", "執行", "開發"],
        )];
        let result = apply_fixes(text, &issues, FixMode::Aggressive, &[]);
        assert_eq!(result.text, text); // unchanged -- no segmenter, cannot verify clues
        assert_eq!(result.skipped, 1);
    }

    #[test]
    fn surrounding_window_basic() {
        let text = "AABBCCDDEE";
        let window = surrounding_window(text, 4, 6);
        // Window should include chars around the CC range
        assert!(window.contains('A'));
        assert!(window.contains('E'));
    }

    #[test]
    fn surrounding_window_cjk() {
        let text = "我需要編寫一個程序來執行這個任務";
        let offset = text.find("程序").unwrap();
        let end = offset + "程序".len();
        let window = surrounding_window(text, offset, end);
        assert!(window.contains("編寫"));
        assert!(window.contains("執行"));
    }

    #[test]
    fn surrounding_window_empty_text() {
        let window = surrounding_window("", 0, 0);
        assert_eq!(window, "");
    }

    #[test]
    fn surrounding_window_at_boundaries() {
        // Match spans entire text -- window should return the whole string.
        let text = "程序";
        let window = surrounding_window(text, 0, text.len());
        assert_eq!(window, "程序");
    }

    #[test]
    fn empty_context_clues_vec_treated_as_no_clues() {
        // Issue with context_clues: Some(vec![]) should NOT be skipped in safe mode
        // because the empty vec means no ambiguity (the !clues.is_empty() guard).
        let text = "這個軟件很好用";
        let mut issue = make_issue(6, "軟件", vec!["軟體"]);
        issue.context_clues = Some(vec![]);
        let result = apply_fixes(text, &[issue], FixMode::Safe, &[]);
        assert_eq!(result.text, "這個軟體很好用");
        assert_eq!(result.applied, 1);
    }
}
