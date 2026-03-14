// Grammar scanner: pattern-based grammatical checks for zh-TW text.
//
// Detects interlingual transfer errors (English grammar calques in Chinese)
// and structural redundancies without requiring POS tagging.
//
// Phase 2a: interlingual transfer detection
//   - 和-connecting-clauses (和 between verb phrases instead of nouns)
//   - 是+adjective copula (是 before adjective without 很/非常)
//   - Redundant preposition after transitive verb
//
// Phase 2b: A-not-A + 嗎 clash detection
//   - A-not-A question structure with redundant sentence-final 嗎

use crate::engine::excluded::{is_excluded, ByteRange};
use crate::engine::scan::is_cjk_ideograph;
use crate::rules::ruleset::{Issue, IssueType, Severity};

// Common verb-final suffixes that indicate a verb phrase precedes 和.
const VERB_SUFFIXES: &[char] = &['了', '過', '著', '來', '去', '完', '好', '到'];

// Common pronouns for 是+adjective detection.
const PRONOUNS: &[&str] = &[
    "我", "你", "他", "她", "它", "我們", "你們", "他們", "她們", "這", "那", "這個", "那個",
];

// Adjectives commonly misused with bare 是 (English calque).
// Kept small and high-confidence to minimize false positives.
const BARE_SHI_ADJECTIVES: &[&str] = &[
    "漂亮", "高興", "開心", "難過", "傷心", "生氣", "快樂", "緊張", "害怕", "著急", "無聊", "好看",
    "難看", "厲害", "聰明", "笨", "冷", "熱", "忙", "累", "餓", "渴", "胖", "瘦", "大", "小", "多",
    "少", "長", "短", "高", "矮", "好", "壞", "新", "舊", "快", "慢", "早", "晚", "遠", "近", "深",
    "淺", "重", "輕", "難", "容易",
];

// Degree adverbs that make 是+adjective grammatical.
const DEGREE_ADVERBS: &[&str] = &[
    "很",
    "非常",
    "特別",
    "十分",
    "極",
    "超",
    "真",
    "太",
    "蠻",
    "挺",
    "相當",
    "比較",
    "最",
    "更",
    "越來越",
    "有點",
    "稍微",
];

// A-not-A patterns (question structures where 嗎 is redundant).
const A_NOT_A_PATTERNS: &[&str] = &[
    "是不是",
    "有沒有",
    "能不能",
    "會不會",
    "要不要",
    "好不好",
    "對不對",
    "行不行",
    "可不可以",
    "願不願意",
    "想不想",
    "知不知道",
    "喜不喜歡",
    "認不認識",
    "做不做",
    "吃不吃",
    "去不去",
    "來不來",
    "看不看",
    "走不走",
];

// Transitive verb + spurious preposition pairs (English calque).
// (verb, spurious_preposition, context_description)
const TRANSITIVE_VERB_PREPOSITION_PAIRS: &[(&str, &str, &str)] = &[
    ("強調", "在", "transitive verb with redundant preposition"),
    ("討論", "關於", "transitive verb with redundant preposition"),
    ("研究", "關於", "transitive verb with redundant preposition"),
    ("影響", "到", "transitive verb with redundant preposition"),
    ("考慮", "到", "transitive verb with redundant preposition"),
    ("處理", "到", "transitive verb with redundant preposition"),
    ("分析", "關於", "transitive verb with redundant preposition"),
];

// Bureaucratic verbal prefixes (English 'conduct/carry out' calque).
// "進行討論" → "討論", "加以分析" → "分析", "予以處理" → "處理"
const BUREAUCRATIC_PREFIXES: &[&str] = &["進行", "加以", "予以"];

// Verbs commonly nominalized after bureaucratic prefixes.
const NOMINALIZED_VERBS: &[&str] = &[
    "討論", "分析", "研究", "調查", "測試", "開發", "設計", "評估", "檢查", "審查", "修改", "更新",
    "比較", "溝通", "合作", "訓練", "處理", "管理", "規劃", "改善", "調整", "整合", "驗證", "觀察",
    "監控", "維護",
];

// Verbose action prefixes + abstract objects.
// "做出決定" → "決定", "作出回應" → "回應"
const VERBOSE_ACTION_PREFIXES: &[&str] = &["做出", "作出"];

const VERBOSE_ACTION_OBJECTS: &[&str] = &[
    "決定", "回應", "貢獻", "改變", "調整", "承諾", "解釋", "判斷", "選擇", "反應", "讓步", "保證",
    "回答", "犧牲", "努力",
];

// Attribution verbs for double-attribution detection.
// "根據研究顯示" is redundant — use "根據研究" or "研究顯示".
const ATTRIBUTION_VERBS: &[&str] = &["顯示", "指出", "表明", "表示", "說明"];

// Sentence-ending delimiters for boundary detection.
fn is_sentence_end(ch: char) -> bool {
    matches!(ch, '。' | '？' | '！' | '?' | '!' | '\n')
}

// Clause-level delimiters (includes commas, semicolons).
fn is_clause_boundary(ch: char) -> bool {
    is_sentence_end(ch) || matches!(ch, '，' | ',' | '；' | ';' | '：' | ':')
}

fn grammar_issue(
    offset: usize,
    found: &str,
    suggestion: &str,
    context: &str,
    severity: Severity,
) -> Issue {
    Issue::new(
        offset,
        found.len(),
        found,
        vec![suggestion.into()],
        IssueType::Grammar,
        severity,
    )
    .with_context(context)
}

// Phase 2b: detect A-not-A structures co-occurring with sentence-final 嗎.
pub(crate) fn scan_a_not_a_ma(text: &str, excluded: &[ByteRange], issues: &mut Vec<Issue>) {
    for pattern in A_NOT_A_PATTERNS {
        let mut search_start = 0;
        while let Some(pos) = text[search_start..].find(pattern) {
            let abs_pos = search_start + pos;
            let pattern_end = abs_pos + pattern.len();
            search_start = pattern_end;

            if is_excluded(abs_pos, pattern_end, excluded) {
                continue;
            }

            // Find sentence boundary after this A-not-A pattern.
            let rest = &text[pattern_end..];
            let sentence_end_pos = rest
                .char_indices()
                .find(|&(_, ch)| is_sentence_end(ch))
                .map(|(i, _)| pattern_end + i);

            let sentence_slice = if let Some(end) = sentence_end_pos {
                &text[pattern_end..end]
            } else {
                rest
            };

            // Check if 嗎 appears at the end of the sentence (possibly
            // preceded by whitespace only).
            let trimmed = sentence_slice.trim_end();
            if trimmed.ends_with('嗎') {
                let ma_offset = pattern_end + sentence_slice.rfind('嗎').unwrap();
                let ma_end = ma_offset + '嗎'.len_utf8();
                if !is_excluded(ma_offset, ma_end, excluded) {
                    // Report the whole span from A-not-A to 嗎 as the found text.
                    let found = &text[abs_pos..ma_end];
                    issues.push(grammar_issue(
                        abs_pos,
                        found,
                        &text[abs_pos..pattern_end],
                        "A-not-A structure already encodes yes/no question; sentence-final \
                         '\u{55ce}' is redundant",
                        Severity::Warning,
                    ));
                }
            }
        }
    }
}

// Phase 2a: detect 和 connecting clauses (verb phrases) instead of nouns.
pub(crate) fn scan_he_connecting_clauses(
    text: &str,
    excluded: &[ByteRange],
    issues: &mut Vec<Issue>,
) {
    let mut search_start = 0;
    let he = '和';
    let he_len = he.len_utf8();

    while let Some(pos) = text[search_start..].find(he) {
        let abs_pos = search_start + pos;
        let he_end = abs_pos + he_len;
        search_start = he_end;

        if is_excluded(abs_pos, he_end, excluded) {
            continue;
        }

        // Check if the character immediately before 和 is a verb suffix.
        // This is a heuristic: CJK char ending in common verb suffixes
        // (了/過/著/來/去/完/好/到) strongly suggests a verb phrase.
        let before_he = &text[..abs_pos];
        let prev_char = before_he.chars().next_back();
        let has_verb_suffix = prev_char.is_some_and(|ch| VERB_SUFFIXES.contains(&ch));

        if !has_verb_suffix {
            continue;
        }

        // Also check the character after 和 -- if followed by another verb
        // phrase indicator (pronoun starting a new clause), this is likely
        // a clause-connecting 和.
        let after_he = &text[he_end..];

        // Quick check: next CJK character should not be a noun-like context.
        // If the next char is also a verb suffix or a pronoun starts the
        // next segment, flag it.
        let next_is_pronoun = PRONOUNS.iter().any(|p| after_he.starts_with(p));

        if !next_is_pronoun {
            continue;
        }

        // Guard: skip comparative constructions (和X一樣/一般/相同/類似/相似).
        // These use 和 as a preposition, not a conjunction.
        let window_end = text[he_end..]
            .char_indices()
            .nth(10)
            .map_or(text.len(), |(i, _)| he_end + i);
        let comparative_window = &text[he_end..window_end];
        if ["一樣", "一般", "相同", "類似", "相似"]
            .iter()
            .any(|pat| comparative_window.contains(pat))
        {
            continue;
        }

        issues.push(grammar_issue(
            abs_pos,
            &text[abs_pos..he_end],
            "，",
            "'\u{548c}' connects nouns/noun phrases only; use comma or conjunctions \
             like '\u{800c}\u{4e14}'/'\u{4e26}\u{4e14}' for clauses",
            Severity::Info,
        ));
    }
}

// Phase 2a: detect bare 是+adjective (English copula calque).
pub(crate) fn scan_bare_shi_adjective(text: &str, excluded: &[ByteRange], issues: &mut Vec<Issue>) {
    let shi = "是";
    let shi_len = shi.len();
    let mut search_start = 0;

    while let Some(pos) = text[search_start..].find(shi) {
        let abs_pos = search_start + pos;
        let shi_end = abs_pos + shi_len;
        search_start = shi_end;

        if is_excluded(abs_pos, shi_end, excluded) {
            continue;
        }

        // Check if preceded by a pronoun.
        let before = &text[..abs_pos];
        let preceded_by_pronoun = PRONOUNS.iter().any(|p| before.ends_with(p));
        if !preceded_by_pronoun {
            continue;
        }

        // Check if followed by a degree adverb (which makes it grammatical).
        let after = &text[shi_end..];
        let has_degree_adverb = DEGREE_ADVERBS.iter().any(|a| after.starts_with(a));
        if has_degree_adverb {
            continue;
        }

        // Check if followed by a bare adjective.
        let matched_adj = BARE_SHI_ADJECTIVES
            .iter()
            .find(|&&adj| after.starts_with(adj));

        if let Some(adj) = matched_adj {
            let adj_end = shi_end + adj.len();
            if is_excluded(abs_pos, adj_end, excluded) {
                continue;
            }

            // Guard: if the adjective is immediately followed by a CJK
            // character that acts as a noun head, it's a modifier in a noun
            // phrase (e.g. 好消息, 大問題), not a bare adjective predicate.
            // Exclude particles (啊了呢吧嗎呀) and connectors (又且並但而的)
            // which do NOT indicate a noun compound.
            let after_adj = &text[adj_end..];
            if let Some(ch) = after_adj.chars().next() {
                if is_cjk_ideograph(ch)
                    && !matches!(
                        ch,
                        '的' | '了'
                            | '啊'
                            | '呀'
                            | '呢'
                            | '吧'
                            | '嗎'
                            | '又'
                            | '且'
                            | '並'
                            | '但'
                            | '而'
                    )
                {
                    continue;
                }
            }

            // Find the pronoun that precedes 是 to include in the found span.
            let pronoun = PRONOUNS.iter().find(|p| before.ends_with(*p)).unwrap();
            let pronoun_start = abs_pos - pronoun.len();
            let found = &text[pronoun_start..adj_end];
            let suggestion = format!("{}很{}", pronoun, adj,);

            issues.push(grammar_issue(
                pronoun_start,
                found,
                &suggestion,
                "Chinese adjectives are stative verbs; bare '\u{662f}' before adjective \
                 is an English calque — use degree adverb '\u{5f88}' instead",
                Severity::Info,
            ));
        }
    }
}

// Phase 2a: detect transitive verb + redundant preposition.
pub(crate) fn scan_redundant_preposition(
    text: &str,
    excluded: &[ByteRange],
    issues: &mut Vec<Issue>,
) {
    for &(verb, prep, ctx) in TRANSITIVE_VERB_PREPOSITION_PAIRS {
        let mut search_start = 0;
        while let Some(pos) = text[search_start..].find(verb) {
            let abs_pos = search_start + pos;
            let verb_end = abs_pos + verb.len();
            search_start = verb_end;

            if is_excluded(abs_pos, verb_end, excluded) {
                continue;
            }

            // Check if the preposition appears within 6 characters after verb.
            let window_end = text.floor_char_boundary(text.len().min(verb_end + 6 * 4));
            let after = &text[verb_end..window_end];

            if let Some(prep_offset) = after.find(prep) {
                // Only flag if the preposition is close (within ~2 chars of
                // intervening content, to avoid false positives).
                let gap = &after[..prep_offset];
                let gap_chars: usize = gap.chars().count();
                if gap_chars > 2 {
                    continue;
                }

                let prep_abs = verb_end + prep_offset;
                let prep_end = prep_abs + prep.len();
                if is_excluded(prep_abs, prep_end, excluded) {
                    continue;
                }

                let found = &text[abs_pos..prep_end];
                issues.push(grammar_issue(abs_pos, found, verb, ctx, Severity::Info));
            }
        }
    }
}

// Detect bureaucratic nominalization: 進行/加以/予以 + verb.
// These are calques of English "conduct/carry out + noun" and are verbose.
pub(crate) fn scan_bureaucratic_nominalization(
    text: &str,
    excluded: &[ByteRange],
    issues: &mut Vec<Issue>,
) {
    for prefix in BUREAUCRATIC_PREFIXES {
        let prefix_len = prefix.len();
        let mut search_start = 0;
        while let Some(pos) = text[search_start..].find(prefix) {
            let abs_pos = search_start + pos;
            let prefix_end = abs_pos + prefix_len;
            search_start = prefix_end;

            if is_excluded(abs_pos, prefix_end, excluded) {
                continue;
            }

            // Look for a nominalized verb within 2-char gap after prefix.
            let window_end = text.floor_char_boundary(text.len().min(prefix_end + 2 * 4 + 6 * 4));
            let after = &text[prefix_end..window_end];

            // Pick the verb whose match is earliest by text position, not
            // list order — avoids silently matching the wrong verb when two
            // verbs from the list both appear in the window.
            let matched = NOMINALIZED_VERBS
                .iter()
                .filter_map(|verb| {
                    after.find(verb).and_then(|offset| {
                        let gap_chars = after[..offset].chars().count();
                        if gap_chars <= 2 {
                            Some((verb, offset))
                        } else {
                            None
                        }
                    })
                })
                .min_by_key(|&(_, offset)| offset);

            if let Some((verb, verb_offset)) = matched {
                let verb_abs = prefix_end + verb_offset;
                let verb_end = verb_abs + verb.len();
                if is_excluded(verb_abs, verb_end, excluded) {
                    continue;
                }

                let found = &text[abs_pos..verb_end];
                issues.push(grammar_issue(
                    abs_pos,
                    found,
                    verb,
                    "bureaucratic nominalization calque of English 'conduct/carry out \
                     + noun'; use the verb directly",
                    Severity::Info,
                ));
            }
        }
    }
}

// Detect verbose action prefix: 做出/作出 + abstract noun.
// "做出決定" → "決定", "作出回應" → "回應"
pub(crate) fn scan_verbose_action(text: &str, excluded: &[ByteRange], issues: &mut Vec<Issue>) {
    for prefix in VERBOSE_ACTION_PREFIXES {
        let prefix_len = prefix.len();
        let mut search_start = 0;
        while let Some(pos) = text[search_start..].find(prefix) {
            let abs_pos = search_start + pos;
            let prefix_end = abs_pos + prefix_len;
            search_start = prefix_end;

            if is_excluded(abs_pos, prefix_end, excluded) {
                continue;
            }

            // Check if an action object follows immediately (0-1 char gap).
            let window_end = text.floor_char_boundary(text.len().min(prefix_end + 4 + 6 * 4));
            let after = &text[prefix_end..window_end];

            let matched = VERBOSE_ACTION_OBJECTS
                .iter()
                .filter_map(|obj| {
                    after.find(obj).and_then(|offset| {
                        let gap_chars = after[..offset].chars().count();
                        if gap_chars <= 1 {
                            Some((obj, offset))
                        } else {
                            None
                        }
                    })
                })
                .min_by_key(|&(_, offset)| offset);

            if let Some((obj, obj_offset)) = matched {
                let obj_abs = prefix_end + obj_offset;
                let obj_end = obj_abs + obj.len();
                if is_excluded(obj_abs, obj_end, excluded) {
                    continue;
                }

                let found = &text[abs_pos..obj_end];
                issues.push(grammar_issue(
                    abs_pos,
                    found,
                    obj,
                    "verbose nominalization; the object can serve as a verb directly",
                    Severity::Info,
                ));
            }
        }
    }
}

// Verbs commonly found in the 對X進行Y pattern.
const DUI_JINXING_VERBS: &[&str] = &[
    "討論", "分析", "研究", "調查", "測試", "開發", "設計", "評估", "檢查", "審查", "修改", "更新",
    "比較", "處理", "管理", "規劃", "改善", "調整", "整合", "驗證", "觀察", "監控", "維護", "計算",
    "編輯", "翻譯", "優化", "部署", "配置", "重構",
];

// Detect 對X進行Y pattern: fronted-object bureaucratic padding.
// "對資料進行分析" → "分析資料", "對系統進行測試" → "測試系統"
// This is distinct from scan_bureaucratic_nominalization which catches
// standalone "進行分析" — here the explicit 對X object is present, giving
// a better suggestion that preserves the object.
pub(crate) fn scan_dui_jinxing(text: &str, excluded: &[ByteRange], issues: &mut Vec<Issue>) {
    let marker = "對";
    let marker_len = marker.len();
    let jinxing = "進行";
    let jinxing_len = jinxing.len();
    let mut search_start = 0;

    while let Some(pos) = text[search_start..].find(marker) {
        let abs_pos = search_start + pos;
        let marker_end = abs_pos + marker_len;
        search_start = marker_end;

        if is_excluded(abs_pos, marker_end, excluded) {
            continue;
        }

        // Skip if 對 is part of a compound word (針對, 對於, 面對, 絕對, 相對).
        // Check preceding char: if CJK, this 對 is likely a suffix, not a
        // standalone preposition.
        if abs_pos > 0 {
            let prev_ch = text[..abs_pos].chars().next_back();
            if prev_ch.is_some_and(|ch| {
                matches!(
                    ch,
                    '針' | '面' | '絕' | '相' | '反' | '比' | '核' | '校' | '應' | '配'
                )
            }) {
                continue;
            }
        }
        // Check following char: 對於 is a compound preposition, not this pattern.
        if text[marker_end..].starts_with('於') {
            continue;
        }

        // Look for 進行 within a reasonable window (up to 8 CJK chars ≈ 24 bytes).
        let window_end = text.floor_char_boundary(text.len().min(marker_end + 8 * 4));
        let after_dui = &text[marker_end..window_end];

        let Some(jinxing_offset) = after_dui.find(jinxing) else {
            continue;
        };

        // The object sits between 對 and 進行; must be 1-6 chars, non-empty.
        let object = &after_dui[..jinxing_offset];
        let obj_chars = object.chars().count();
        if obj_chars == 0 || obj_chars > 6 {
            continue;
        }

        // Skip if object contains clause boundary chars.
        if object.chars().any(is_clause_boundary) {
            continue;
        }

        let jinxing_abs = marker_end + jinxing_offset;
        let jinxing_end = jinxing_abs + jinxing_len;

        if is_excluded(jinxing_abs, jinxing_end, excluded) {
            continue;
        }

        // Look for a verb after 進行, within 2-char gap.
        let verb_window_end = text.floor_char_boundary(text.len().min(jinxing_end + 2 * 4 + 6 * 4));
        let after_jinxing = &text[jinxing_end..verb_window_end];

        let matched = DUI_JINXING_VERBS
            .iter()
            .filter_map(|verb| {
                after_jinxing.find(verb).and_then(|offset| {
                    let gap_chars = after_jinxing[..offset].chars().count();
                    if gap_chars <= 2 {
                        Some((verb, offset))
                    } else {
                        None
                    }
                })
            })
            .min_by_key(|&(_, offset)| offset);

        if let Some((verb, verb_offset)) = matched {
            let verb_abs = jinxing_end + verb_offset;
            let verb_end = verb_abs + verb.len();
            if is_excluded(verb_abs, verb_end, excluded) {
                continue;
            }

            let found = &text[abs_pos..verb_end];
            let suggestion = format!("{verb}{object}");
            issues.push(grammar_issue(
                abs_pos,
                found,
                &suggestion,
                "fronted-object bureaucratic padding '\u{5c0d}X\u{9032}\u{884c}Y'; \
                 restructure as 'verb + object' directly",
                Severity::Info,
            ));
        }
    }
}

// Detect double attribution: 根據 + attribution verb in same clause.
// "根據研究顯示" is redundant — either "根據研究" or "研究顯示" suffices.
pub(crate) fn scan_double_attribution(text: &str, excluded: &[ByteRange], issues: &mut Vec<Issue>) {
    let marker = "根據";
    let marker_len = marker.len();
    let mut search_start = 0;

    while let Some(pos) = text[search_start..].find(marker) {
        let abs_pos = search_start + pos;
        let marker_end = abs_pos + marker_len;
        search_start = marker_end;

        if is_excluded(abs_pos, marker_end, excluded) {
            continue;
        }

        // Search within current clause (up to next clause boundary).
        let rest = &text[marker_end..];
        let clause_len = rest
            .char_indices()
            .find(|&(_, ch)| is_clause_boundary(ch))
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let clause = &rest[..clause_len];

        // Check for any attribution verb in this clause.
        for verb in ATTRIBUTION_VERBS {
            if let Some(verb_offset) = clause.find(verb) {
                let verb_abs = marker_end + verb_offset;
                let verb_end = verb_abs + verb.len();
                if is_excluded(verb_abs, verb_end, excluded) {
                    continue;
                }

                let found = &text[abs_pos..verb_end];
                let source = &text[marker_end..verb_abs];
                // Skip degenerate case: no source between 根據 and verb.
                if source.trim().is_empty() {
                    continue;
                }
                // Skip when the matched verb is actually a prefix of a longer
                // compound noun (e.g. 說明書, 表示式, 顯示器). Key the
                // suffix check to the specific verb to avoid false negatives
                // like 表示會 (will indicate) or 顯示圖 (show diagram).
                let after_verb = &text[verb_end..];
                let is_compound = match *verb {
                    "說明" => after_verb.starts_with('書') || after_verb.starts_with('文'),
                    "表示" => after_verb.starts_with('式') || after_verb.starts_with('法'),
                    "顯示" => after_verb.starts_with('器') || after_verb.starts_with('屏'),
                    _ => false,
                };
                if is_compound {
                    continue;
                }
                // Skip when a markdown link bracket sits between 根據 and the
                // verb — the verb is inside link text, not an attribution verb.
                if source.contains('[') || source.contains(']') {
                    continue;
                }
                let suggestion = format!("根據{source}");
                issues.push(grammar_issue(
                    abs_pos,
                    found,
                    &suggestion,
                    "double attribution: '\u{6839}\u{64da}' (according to) and \
                     reporting verb are redundant together; use one or the other",
                    Severity::Info,
                ));
                break; // one attribution verb per 根據 instance
            }
        }
    }
}

// Main entry point: run all grammar checks.
pub(crate) fn scan_grammar(text: &str, excluded: &[ByteRange], issues: &mut Vec<Issue>) {
    scan_a_not_a_ma(text, excluded, issues);
    scan_he_connecting_clauses(text, excluded, issues);
    scan_bare_shi_adjective(text, excluded, issues);
    scan_redundant_preposition(text, excluded, issues);
    scan_bureaucratic_nominalization(text, excluded, issues);
    scan_verbose_action(text, excluded, issues);
    scan_dui_jinxing(text, excluded, issues);
    scan_double_attribution(text, excluded, issues);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &str) -> Vec<Issue> {
        let mut issues = Vec::new();
        scan_grammar(text, &[], &mut issues);
        issues
    }

    // =======================================================================
    // Phase 1: plumbing — IssueType::Grammar fundamentals
    // =======================================================================

    #[test]
    fn grammar_issue_type_serde_round_trip() {
        let json = serde_json::to_string(&IssueType::Grammar).unwrap();
        assert_eq!(json, "\"grammar\"");
        let back: IssueType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, IssueType::Grammar);
    }

    #[test]
    fn grammar_sort_order_is_last() {
        // Grammar should sort after all other issue types.
        assert!(IssueType::Grammar.sort_order() > IssueType::Variant.sort_order());
        assert!(IssueType::Grammar.sort_order() > IssueType::Punctuation.sort_order());
    }

    #[test]
    fn grammar_name_matches_serde() {
        assert_eq!(IssueType::Grammar.name(), "grammar");
    }

    #[test]
    fn grammar_issue_fields_populated() {
        let issues = scan("你是不是學生嗎？");
        assert_eq!(issues.len(), 1);
        let i = &issues[0];
        assert_eq!(i.rule_type, IssueType::Grammar);
        assert_eq!(i.severity, Severity::Warning);
        assert!(i.context.is_some(), "grammar issues should have context");
        assert!(!i.suggestions.is_empty(), "should have suggestions");
        assert!(i.length > 0, "should have nonzero byte length");
    }

    #[test]
    fn grammar_issue_offset_is_byte_accurate() {
        let text = "你是不是學生嗎？";
        let issues = scan(text);
        assert_eq!(issues.len(), 1);
        let i = &issues[0];
        // The found text extracted from the reported span should match.
        assert_eq!(&text[i.offset..i.offset + i.length], i.found);
    }

    #[test]
    fn empty_text_produces_no_issues() {
        assert!(scan("").is_empty());
    }

    #[test]
    fn ascii_only_text_produces_no_issues() {
        assert!(scan("Hello world, this is a test.").is_empty());
    }

    #[test]
    fn clean_chinese_text_produces_no_issues() {
        let clean = "台灣是一個美麗的島嶼，有豐富的文化和美食。";
        assert!(scan(clean).is_empty());
    }

    // =======================================================================
    // Phase 2b: A-not-A + 嗎 — all 14 patterns × with/without 嗎
    // =======================================================================

    // -- with 嗎 (should flag) --

    #[test]
    fn a_not_a_shi_bu_shi_with_ma() {
        let issues = scan("你是不是學生嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("是不是"));
        assert!(issues[0].found.contains("嗎"));
    }

    #[test]
    fn a_not_a_you_mei_you_with_ma() {
        let issues = scan("你有沒有吃飯嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("有沒有"));
    }

    #[test]
    fn a_not_a_neng_bu_neng_with_ma() {
        let issues = scan("你能不能來嗎");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("能不能"));
    }

    #[test]
    fn a_not_a_hui_bu_hui_with_ma() {
        let issues = scan("他會不會游泳嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("會不會"));
    }

    #[test]
    fn a_not_a_yao_bu_yao_with_ma() {
        let issues = scan("你要不要喝水嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("要不要"));
    }

    #[test]
    fn a_not_a_hao_bu_hao_with_ma() {
        let issues = scan("這樣好不好嗎");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("好不好"));
    }

    #[test]
    fn a_not_a_dui_bu_dui_with_ma() {
        let issues = scan("答案對不對嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("對不對"));
    }

    #[test]
    fn a_not_a_xing_bu_xing_with_ma() {
        let issues = scan("這樣行不行嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("行不行"));
    }

    #[test]
    fn a_not_a_ke_bu_ke_yi_with_ma() {
        let issues = scan("可不可以走嗎");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("可不可以"));
    }

    #[test]
    fn a_not_a_yuan_bu_yuan_yi_with_ma() {
        let issues = scan("你願不願意幫忙嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("願不願意"));
    }

    #[test]
    fn a_not_a_xiang_bu_xiang_with_ma() {
        let issues = scan("你想不想去嗎");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("想不想"));
    }

    #[test]
    fn a_not_a_zhi_bu_zhi_dao_with_ma() {
        let issues = scan("你知不知道嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("知不知道"));
    }

    #[test]
    fn a_not_a_xi_bu_xi_huan_with_ma() {
        let issues = scan("你喜不喜歡吃飯嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("喜不喜歡"));
    }

    #[test]
    fn a_not_a_ren_bu_ren_shi_with_ma() {
        let issues = scan("你認不認識他嗎");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("認不認識"));
    }

    // -- without 嗎 (should NOT flag) --

    #[test]
    fn a_not_a_shi_bu_shi_without_ma() {
        assert!(scan("你是不是學生？").is_empty());
    }

    #[test]
    fn a_not_a_you_mei_you_without_ma() {
        assert!(scan("你有沒有吃飯？").is_empty());
    }

    #[test]
    fn a_not_a_neng_bu_neng_without_ma() {
        assert!(scan("你能不能來？").is_empty());
    }

    #[test]
    fn a_not_a_hui_bu_hui_without_ma() {
        assert!(scan("他會不會游泳？").is_empty());
    }

    #[test]
    fn a_not_a_yao_bu_yao_without_ma() {
        assert!(scan("你要不要喝水？").is_empty());
    }

    #[test]
    fn a_not_a_hao_bu_hao_without_ma() {
        assert!(scan("這樣好不好？").is_empty());
    }

    #[test]
    fn a_not_a_dui_bu_dui_without_ma() {
        assert!(scan("答案對不對？").is_empty());
    }

    #[test]
    fn a_not_a_xing_bu_xing_without_ma() {
        assert!(scan("這樣行不行？").is_empty());
    }

    #[test]
    fn a_not_a_ke_bu_ke_yi_without_ma() {
        assert!(scan("可不可以走？").is_empty());
    }

    #[test]
    fn a_not_a_yuan_bu_yuan_yi_without_ma() {
        assert!(scan("你願不願意幫忙？").is_empty());
    }

    #[test]
    fn a_not_a_xiang_bu_xiang_without_ma() {
        assert!(scan("你想不想去？").is_empty());
    }

    #[test]
    fn a_not_a_zhi_bu_zhi_dao_without_ma() {
        assert!(scan("你知不知道？").is_empty());
    }

    #[test]
    fn a_not_a_xi_bu_xi_huan_without_ma() {
        assert!(scan("你喜不喜歡吃飯？").is_empty());
    }

    #[test]
    fn a_not_a_ren_bu_ren_shi_without_ma() {
        assert!(scan("你認不認識他？").is_empty());
    }

    // -- A-not-A edge cases --

    #[test]
    fn a_not_a_ma_across_sentence_boundary_clean() {
        // 嗎 is in a different sentence — must not flag.
        assert!(scan("你是不是學生。他好嗎？").is_empty());
    }

    #[test]
    fn a_not_a_ma_across_newline_boundary_clean() {
        assert!(scan("你是不是學生\n他好嗎？").is_empty());
    }

    #[test]
    fn a_not_a_ma_across_exclamation_boundary_clean() {
        assert!(scan("你是不是學生！他好嗎？").is_empty());
    }

    #[test]
    fn ma_only_no_a_not_a_clean() {
        assert!(scan("你是學生嗎？").is_empty());
    }

    #[test]
    fn a_not_a_suggestion_is_pattern_without_ma() {
        let issues = scan("你是不是學生嗎？");
        assert_eq!(issues[0].suggestions[0], "是不是");
    }

    #[test]
    fn a_not_a_severity_is_warning() {
        let issues = scan("你是不是學生嗎？");
        assert_eq!(issues[0].severity, Severity::Warning);
    }

    #[test]
    fn a_not_a_ma_with_trailing_whitespace() {
        // 嗎 followed by spaces before sentence end.
        let issues = scan("你是不是學生嗎  ？");
        assert_eq!(issues.len(), 1);
    }

    // =======================================================================
    // Phase 2a: 和-connecting-clauses
    // =======================================================================

    #[test]
    fn he_verb_suffix_le_with_pronoun() {
        let issues = scan("我吃了和你去看電影");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "和");
        assert_eq!(issues[0].severity, Severity::Info);
    }

    #[test]
    fn he_verb_suffix_guo_with_pronoun() {
        let issues = scan("我去過和他來過");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn he_verb_suffix_zhe_with_pronoun() {
        let issues = scan("我看著和她說話");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn he_verb_suffix_lai_with_pronoun() {
        let issues = scan("我回來和你一起走");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn he_verb_suffix_qu_with_pronoun() {
        let issues = scan("他出去和我回家");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn he_verb_suffix_wan_with_pronoun() {
        let issues = scan("我寫完和你開始");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn he_verb_suffix_hao_with_pronoun() {
        let issues = scan("我準備好和他出發");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn he_verb_suffix_dao_with_pronoun() {
        let issues = scan("我找到和她確認");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn he_between_nouns_clean() {
        assert!(scan("蘋果和橘子都很好吃").is_empty());
    }

    #[test]
    fn he_no_verb_suffix_before_clean() {
        // No verb suffix immediately before 和.
        assert!(scan("老師和學生都來了").is_empty());
    }

    #[test]
    fn he_verb_suffix_but_no_pronoun_after_clean() {
        // Verb suffix before 和, but no pronoun after → not a clause connector.
        assert!(scan("我吃了和飯").is_empty());
    }

    #[test]
    fn he_suggestion_is_comma() {
        let issues = scan("我住在台北了和我有一隻狗");
        assert_eq!(issues[0].suggestions[0], "，");
    }

    // =======================================================================
    // Phase 2a: 是+adjective copula
    // =======================================================================

    #[test]
    fn bare_shi_disyllabic_adj() {
        let issues = scan("她是漂亮");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "她是漂亮");
        assert_eq!(issues[0].suggestions[0], "她很漂亮");
    }

    #[test]
    fn bare_shi_monosyllabic_adj() {
        let issues = scan("我是忙");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "我很忙");
    }

    #[test]
    fn bare_shi_adj_with_ta() {
        let issues = scan("他是高");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "他很高");
    }

    #[test]
    fn bare_shi_adj_with_women() {
        let issues = scan("我們是開心");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "我們很開心");
    }

    #[test]
    fn bare_shi_adj_with_zhe() {
        let issues = scan("這是好");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "這很好");
    }

    #[test]
    fn bare_shi_adj_with_na() {
        let issues = scan("那是遠");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "那很遠");
    }

    #[test]
    fn bare_shi_severity_is_info() {
        let issues = scan("她是漂亮");
        assert_eq!(issues[0].severity, Severity::Info);
    }

    // -- degree adverbs suppress the pattern (negative tests) --

    #[test]
    fn shi_with_hen_clean() {
        assert!(scan("她是很漂亮").is_empty());
    }

    #[test]
    fn shi_with_feichang_clean() {
        assert!(scan("她是非常漂亮").is_empty());
    }

    #[test]
    fn shi_with_tebie_clean() {
        assert!(scan("她是特別漂亮").is_empty());
    }

    #[test]
    fn shi_with_tai_clean() {
        assert!(scan("她是太漂亮").is_empty());
    }

    #[test]
    fn shi_with_zhen_clean() {
        assert!(scan("她是真漂亮").is_empty());
    }

    #[test]
    fn shi_with_bijiao_clean() {
        assert!(scan("她是比較漂亮").is_empty());
    }

    #[test]
    fn shi_with_youdian_clean() {
        assert!(scan("她是有點漂亮").is_empty());
    }

    // -- 是+noun should not fire --

    #[test]
    fn shi_noun_predicate_clean() {
        assert!(scan("她是老師").is_empty());
    }

    #[test]
    fn shi_proper_noun_clean() {
        assert!(scan("他是台灣人").is_empty());
    }

    #[test]
    fn shi_without_pronoun_clean() {
        // No pronoun before 是: e.g. 問題是... — should not fire.
        assert!(scan("問題是很大").is_empty());
    }

    #[test]
    fn shi_adj_as_noun_modifier_clean() {
        // 好消息 — 好 is an adjective modifying a noun, not a bare predicate.
        assert!(scan("這是好消息").is_empty());
    }

    #[test]
    fn shi_adj_as_noun_modifier_da_clean() {
        // 大問題 — same pattern.
        assert!(scan("這是大問題").is_empty());
    }

    #[test]
    fn shi_adj_standalone_still_fires() {
        // 好 at end of text (no following CJK) — still a bare adjective.
        let issues = scan("這是好");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn shi_adj_with_particle_still_fires() {
        // 漂亮啊 — particle after adjective, NOT a noun modifier.
        let issues = scan("她是漂亮啊");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn shi_adj_with_connector_still_fires() {
        // 漂亮又善良 — connector after adjective, NOT a noun modifier.
        let issues = scan("她是漂亮又善良");
        assert_eq!(issues.len(), 1);
    }

    // =======================================================================
    // Phase 2a: redundant preposition
    // =======================================================================

    #[test]
    fn redundant_prep_taolun_guanyu() {
        let issues = scan("我們討論關於這個問題");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("討論關於"));
        assert_eq!(issues[0].suggestions[0], "討論");
    }

    #[test]
    fn redundant_prep_yanjiu_guanyu() {
        let issues = scan("他研究關於量子力學");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("研究關於"));
    }

    #[test]
    fn redundant_prep_qiangdiao_zai() {
        let issues = scan("他強調在這一點上");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("強調在"));
    }

    #[test]
    fn redundant_prep_yingxiang_dao() {
        let issues = scan("這影響到整體計畫");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("影響到"));
    }

    #[test]
    fn redundant_prep_kaolu_dao() {
        let issues = scan("請考慮到這個因素");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("考慮到"));
    }

    #[test]
    fn redundant_prep_chuli_dao() {
        let issues = scan("我處理到這個問題");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("處理到"));
    }

    #[test]
    fn redundant_prep_severity_is_info() {
        let issues = scan("我們討論關於這個問題");
        assert_eq!(issues[0].severity, Severity::Info);
    }

    #[test]
    fn transitive_verb_no_preposition_clean() {
        assert!(scan("我們討論這個問題").is_empty());
    }

    #[test]
    fn preposition_too_far_from_verb_clean() {
        // Gap > 2 chars between verb and preposition.
        assert!(scan("我們討論了很多關於這個問題").is_empty());
    }

    #[test]
    fn redundant_prep_with_one_char_gap() {
        // One char gap between verb and preposition is still flagged.
        let issues = scan("他研究了關於量子力學");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn redundant_prep_fenxi_guanyu() {
        let issues = scan("他分析關於這個現象");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("分析關於"));
    }

    // =======================================================================
    // Extended A-not-A patterns (single-char verbs)
    // =======================================================================

    #[test]
    fn a_not_a_zuo_bu_zuo_with_ma() {
        let issues = scan("你做不做嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("做不做"));
    }

    #[test]
    fn a_not_a_chi_bu_chi_with_ma() {
        let issues = scan("你吃不吃嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("吃不吃"));
    }

    #[test]
    fn a_not_a_qu_bu_qu_with_ma() {
        let issues = scan("你去不去嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("去不去"));
    }

    #[test]
    fn a_not_a_lai_bu_lai_with_ma() {
        let issues = scan("你來不來嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("來不來"));
    }

    #[test]
    fn a_not_a_kan_bu_kan_with_ma() {
        let issues = scan("你看不看嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("看不看"));
    }

    #[test]
    fn a_not_a_zou_bu_zou_with_ma() {
        let issues = scan("你走不走嗎？");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("走不走"));
    }

    #[test]
    fn a_not_a_zuo_bu_zuo_without_ma() {
        assert!(scan("你做不做？").is_empty());
    }

    #[test]
    fn a_not_a_chi_bu_chi_without_ma() {
        assert!(scan("你吃不吃？").is_empty());
    }

    // =======================================================================
    // Bureaucratic nominalization (進行/加以/予以 + verb)
    // =======================================================================

    #[test]
    fn bureaucratic_jinxing_taolun() {
        let issues = scan("我們進行討論");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "進行討論");
        assert_eq!(issues[0].suggestions[0], "討論");
        assert_eq!(issues[0].severity, Severity::Info);
    }

    #[test]
    fn bureaucratic_jinxing_fenxi() {
        let issues = scan("他們進行分析");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "進行分析");
    }

    #[test]
    fn bureaucratic_jinxing_yanjiu() {
        let issues = scan("這個團隊進行研究");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "研究");
    }

    #[test]
    fn bureaucratic_jinxing_ceshi() {
        let issues = scan("我們進行測試");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "進行測試");
    }

    #[test]
    fn bureaucratic_jinxing_with_le_gap() {
        // 了 between prefix and verb (1-char gap, should still flag).
        let issues = scan("我們進行了討論");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "進行了討論");
    }

    #[test]
    fn bureaucratic_jiayi_fenxi() {
        let issues = scan("我們加以分析");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "加以分析");
        assert_eq!(issues[0].suggestions[0], "分析");
    }

    #[test]
    fn bureaucratic_yuyi_chuli() {
        let issues = scan("我們予以處理");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "予以處理");
        assert_eq!(issues[0].suggestions[0], "處理");
    }

    #[test]
    fn bureaucratic_jinxing_standalone_clean() {
        // 進行 as standalone verb ("proceeding") — no nominalized verb after.
        assert!(scan("會議正在進行").is_empty());
    }

    #[test]
    fn bureaucratic_jinxing_zhong_clean() {
        // 進行中 means "in progress" — not a nominalization.
        assert!(scan("專案進行中").is_empty());
    }

    #[test]
    fn bureaucratic_verb_too_far_clean() {
        // Verb too far away (>2 chars gap).
        assert!(scan("我們進行了一些額外的討論").is_empty());
    }

    #[test]
    fn bureaucratic_jinxing_picks_nearest_verb() {
        // Two verbs in window: 管理 (offset 0) and 研究 (offset 2 chars).
        // Should match 管理 (nearest by text position).
        let issues = scan("我們進行管理研究");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "進行管理");
        assert_eq!(issues[0].suggestions[0], "管理");
    }

    #[test]
    fn bureaucratic_multiple_prefixes() {
        let issues = scan("我們進行討論並加以分析");
        assert_eq!(issues.len(), 2);
    }

    // =======================================================================
    // Verbose action prefix (做出/作出 + abstract noun)
    // =======================================================================

    #[test]
    fn verbose_zuochu_jueding() {
        let issues = scan("他做出決定");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "做出決定");
        assert_eq!(issues[0].suggestions[0], "決定");
        assert_eq!(issues[0].severity, Severity::Info);
    }

    #[test]
    fn verbose_zuochu_huiying() {
        let issues = scan("我們做出回應");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "做出回應");
    }

    #[test]
    fn verbose_zuochu_gongxian() {
        let issues = scan("他做出貢獻");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "貢獻");
    }

    #[test]
    fn verbose_zuochu_with_le() {
        let issues = scan("他做出了決定");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "做出了決定");
    }

    #[test]
    fn verbose_zuochu_alt_prefix() {
        // 作出 is an alternate form of 做出.
        let issues = scan("他作出回應");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "作出回應");
    }

    #[test]
    fn verbose_zuochu_no_object_clean() {
        // 做出 without a known object — not flagged.
        assert!(scan("他做出一個蛋糕").is_empty());
    }

    #[test]
    fn verbose_zuochu_object_too_far_clean() {
        // Object too far away (>1 char gap).
        assert!(scan("他做出了很多決定").is_empty());
    }

    // =======================================================================
    // Double attribution (根據...顯示/指出)
    // =======================================================================

    #[test]
    fn double_attribution_genju_xianshi() {
        let issues = scan("根據研究顯示，成果很好");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "根據研究顯示");
        assert_eq!(issues[0].suggestions[0], "根據研究");
        assert_eq!(issues[0].severity, Severity::Info);
    }

    #[test]
    fn double_attribution_genju_zhichu() {
        let issues = scan("根據報告指出，問題嚴重");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "根據報告指出");
    }

    #[test]
    fn double_attribution_genju_biaoming() {
        let issues = scan("根據數據表明這是正確的");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].found, "根據數據表明");
    }

    #[test]
    fn double_attribution_genju_biaoshi() {
        let issues = scan("根據專家表示，這很重要");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("根據專家表示"));
    }

    #[test]
    fn double_attribution_genju_shuoming() {
        let issues = scan("根據文件說明，規格如下");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].found.contains("根據文件說明"));
    }

    #[test]
    fn double_attribution_long_source() {
        // Long source text between 根據 and attribution verb.
        let issues = scan("根據最新發表的一項研究報告顯示，結果令人驚訝");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestions[0], "根據最新發表的一項研究報告");
    }

    #[test]
    fn double_attribution_empty_source_skipped() {
        // Degenerate case: no source between 根據 and verb — skip.
        assert!(scan("根據顯示結果很好").is_empty());
    }

    #[test]
    fn double_attribution_noun_compound_skipped() {
        // 說明書 is a noun compound; 說明 is a prefix, not an attribution verb.
        assert!(scan("根據手冊說明書的內容").is_empty());
    }

    #[test]
    fn double_attribution_verb_at_boundary_still_fires() {
        // 說明 followed by comma (not CJK) — still an attribution verb.
        let issues = scan("根據文件說明，規格如下");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn double_attribution_biaoshi_hui_still_fires() {
        // 表示會 — 會 means "will", not a noun suffix. Must still fire.
        let issues = scan("根據消息表示會延期");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn double_attribution_xianshi_tu_still_fires() {
        // 顯示圖 — 圖 here is "diagram", not a compound suffix. Must fire.
        let issues = scan("根據數據顯示圖表有誤");
        assert_eq!(issues.len(), 1);
    }

    #[test]
    fn double_attribution_markdown_link_skipped() {
        // 根據[link text with 說明](url) — verb inside markdown link, not attribution.
        assert!(scan("根據[維護者設計說明](https://example.com)，新版核心改動很大").is_empty());
    }

    #[test]
    fn double_attribution_markdown_link_bracket_only() {
        // Even a bare [ between 根據 and verb suppresses the match.
        assert!(scan("根據[某研究說明書]的結論").is_empty());
    }

    #[test]
    fn genju_without_verb_clean() {
        // 根據 without attribution verb — prepositional phrase, not redundant.
        assert!(scan("根據研究，成果很好").is_empty());
    }

    #[test]
    fn genju_verb_in_next_clause_clean() {
        // Attribution verb after comma — different clause, not flagged.
        assert!(scan("根據這份報告，研究顯示成果很好").is_empty());
    }

    #[test]
    fn standalone_verb_without_genju_clean() {
        // Attribution verb without 根據 — just a normal verb.
        assert!(scan("研究顯示成果很好").is_empty());
    }

    // =======================================================================
    // Phase 2c: 對X進行Y — fronted-object bureaucratic padding
    // =======================================================================

    #[test]
    fn dui_jinxing_basic() {
        let issues = scan("對資料進行分析");
        let dui: Vec<_> = issues
            .iter()
            .filter(|i| i.found.starts_with("對"))
            .collect();
        assert_eq!(dui.len(), 1);
        assert_eq!(dui[0].found, "對資料進行分析");
        assert_eq!(dui[0].suggestions, vec!["分析資料"]);
        assert_eq!(dui[0].severity, Severity::Info);
    }

    #[test]
    fn dui_jinxing_longer_object() {
        let issues = scan("我們對整個系統進行測試");
        let dui: Vec<_> = issues
            .iter()
            .filter(|i| i.found.starts_with("對"))
            .collect();
        assert_eq!(dui.len(), 1);
        assert_eq!(dui[0].suggestions, vec!["測試整個系統"]);
    }

    #[test]
    fn dui_jinxing_various_verbs() {
        // Each fires dui_jinxing; bureaucratic_nominalization may also fire.
        let check = |text: &str| scan(text).iter().any(|i| i.found.starts_with("對"));
        assert!(check("對程式碼進行審查"));
        assert!(check("對方案進行評估"));
        assert!(check("對架構進行重構"));
    }

    #[test]
    fn dui_jinxing_compound_word_zhendui_skipped() {
        // 針對 is a compound preposition; the 對 is not standalone.
        let issues = scan("針對資料進行分析");
        assert!(
            !issues.iter().any(|i| i.found.starts_with("對")),
            "should not match 對 inside 針對"
        );
    }

    #[test]
    fn dui_jinxing_compound_word_duiyu_skipped() {
        // 對於 is a compound preposition; should not match.
        assert!(!scan("對於資料進行分析")
            .iter()
            .any(|i| i.found.starts_with("對")));
    }

    #[test]
    fn dui_jinxing_compound_miandui_skipped() {
        // 面對 — not a standalone 對.
        assert!(!scan("面對問題進行分析")
            .iter()
            .any(|i| i.found.starts_with("對")));
    }

    #[test]
    fn dui_jinxing_compound_bidui_skipped() {
        // 比對 — technical verb, not standalone 對.
        assert!(!scan("比對資料進行分析")
            .iter()
            .any(|i| i.found.starts_with("對")));
    }

    #[test]
    fn dui_jinxing_compound_hedui_skipped() {
        // 核對 — not standalone 對.
        assert!(!scan("核對資料進行檢查")
            .iter()
            .any(|i| i.found.starts_with("對")));
    }

    #[test]
    fn dui_jinxing_no_verb_after() {
        // 進行 without a matching verb following — not flagged.
        assert!(scan("對資料進行了某些操作").is_empty());
    }

    #[test]
    fn dui_jinxing_no_jinxing() {
        // 對 without 進行 — not flagged.
        assert!(scan("對資料很感興趣").is_empty());
    }

    #[test]
    fn dui_jinxing_object_too_long() {
        // Object between 對 and 進行 exceeds 6 chars — dui_jinxing should skip.
        // (scan_bureaucratic_nominalization may still fire on "進行分析".)
        let issues = scan("對這份非常重要的報告進行分析");
        assert!(
            !issues.iter().any(|i| i.found.starts_with("對")),
            "dui_jinxing should not fire with oversized object"
        );
    }

    #[test]
    fn dui_jinxing_clause_boundary_in_object() {
        // Comma between 對 and 進行 — the 對X進行Y pattern should NOT fire.
        // (scan_bureaucratic_nominalization may still fire on "進行分析".)
        let issues = scan("對資料，進行分析");
        assert!(
            !issues.iter().any(|i| i.found.starts_with("對")),
            "dui_jinxing should not fire across clause boundary"
        );
    }

    #[test]
    fn dui_jinxing_does_not_clash_with_bureaucratic() {
        // Both scanners should fire independently:
        // - scan_bureaucratic_nominalization catches "進行分析" → "分析"
        // - scan_dui_jinxing catches "對資料進行分析" → "分析資料"
        // The broader one (dui_jinxing) covers the full span.
        let issues = scan("對資料進行分析");
        let dui = issues
            .iter()
            .filter(|i| i.found == "對資料進行分析")
            .count();
        let bureau = issues.iter().filter(|i| i.found == "進行分析").count();
        assert_eq!(dui, 1, "dui_jinxing should fire");
        assert_eq!(bureau, 1, "bureaucratic should also fire");
    }

    // =======================================================================
    // Exclusion zone handling
    // =======================================================================

    #[test]
    fn excluded_range_suppresses_a_not_a() {
        let text = "你是不是學生嗎？";
        let excluded = vec![ByteRange {
            start: 0,
            end: text.len(),
        }];
        let mut issues = Vec::new();
        scan_grammar(text, &excluded, &mut issues);
        assert!(issues.is_empty());
    }

    #[test]
    fn excluded_range_suppresses_bare_shi() {
        let text = "她是漂亮";
        let excluded = vec![ByteRange {
            start: 0,
            end: text.len(),
        }];
        let mut issues = Vec::new();
        scan_grammar(text, &excluded, &mut issues);
        assert!(issues.is_empty());
    }

    #[test]
    fn excluded_range_suppresses_redundant_prep() {
        let text = "我們討論關於這個問題";
        let excluded = vec![ByteRange {
            start: 0,
            end: text.len(),
        }];
        let mut issues = Vec::new();
        scan_grammar(text, &excluded, &mut issues);
        assert!(issues.is_empty());
    }

    #[test]
    fn partial_exclusion_still_flags_outside() {
        // Exclude only the first 3 bytes, leaving the rest scannable.
        let text = "你是不是學生嗎？";
        let excluded = vec![ByteRange { start: 0, end: 3 }];
        let mut issues = Vec::new();
        scan_grammar(text, &excluded, &mut issues);
        // 是不是 starts at byte 3 (after 你), should still be detected.
        assert_eq!(issues.len(), 1);
    }

    // =======================================================================
    // Multiple issues in the same text
    // =======================================================================

    #[test]
    fn multiple_grammar_issues_in_one_text() {
        // Contains both A-not-A+嗎 and bare 是+adj.
        let text = "你是不是學生嗎？她是漂亮";
        let issues = scan(text);
        assert_eq!(issues.len(), 2);
        let types: Vec<_> = issues.iter().map(|i| i.rule_type).collect();
        assert!(types.iter().all(|t| *t == IssueType::Grammar));
    }

    #[test]
    fn multiple_a_not_a_in_same_text() {
        let text = "你是不是學生嗎？他有沒有錢嗎？";
        let issues = scan(text);
        assert_eq!(issues.len(), 2);
    }

    // =======================================================================
    // False-positive guards — natural zh-TW text that should NOT trigger
    // =======================================================================

    #[test]
    fn natural_question_with_ma_only() {
        assert!(scan("你今天有空嗎？").is_empty());
    }

    #[test]
    fn natural_he_connecting_nouns() {
        assert!(scan("我喜歡音樂和電影").is_empty());
    }

    #[test]
    fn comparative_he_yiyang_clean() {
        // 和你一樣 is a comparative construction, not clause coordination.
        assert!(scan("找到和你一樣的東西").is_empty());
    }

    #[test]
    fn comparative_he_xiangtong_clean() {
        assert!(scan("做了和他相同的選擇").is_empty());
    }

    #[test]
    fn natural_shi_with_noun() {
        assert!(scan("這是一本好書").is_empty());
    }

    #[test]
    fn natural_shi_de_construction() {
        // 是…的 is a common grammatical construction, not a calque.
        assert!(scan("她是昨天來的").is_empty());
    }

    #[test]
    fn natural_verb_suffix_before_he_but_noun_after() {
        // 了 before 和, but noun (not pronoun) after → no flag.
        assert!(scan("我買了和牛肉").is_empty());
    }

    #[test]
    fn natural_transitive_verb_with_object() {
        assert!(scan("我們討論了技術細節").is_empty());
    }

    #[test]
    fn technical_prose_no_false_positives() {
        let text = "在這個系統中，我們討論了架構設計和效能最佳化。\
                    你有沒有看過相關文件？這是很重要的步驟。";
        assert!(scan(text).is_empty());
    }

    #[test]
    fn natural_jinxing_standalone() {
        // 進行 as "to proceed" without a verb object.
        assert!(scan("工程順利進行，一切正常。").is_empty());
    }

    #[test]
    fn natural_zuochu_physical() {
        // 做出 with a physical object, not abstract action.
        assert!(scan("她做出了一道好菜").is_empty());
    }

    #[test]
    fn natural_genju_prepositional() {
        // 根據 as preposition with comma, no attribution verb in clause.
        assert!(scan("根據合約規定，雙方應遵守以下條款。").is_empty());
    }
}
