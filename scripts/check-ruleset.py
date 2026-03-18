#!/usr/bin/env python3
"""Check, deduplicate, sort, and compact-format assets/ruleset.json.

- spelling_rules: unique by "from", sorted by "from"
- case_rules: unique by "term", sorted by "term"
- First occurrence wins when duplicates exist
- Short arrays (single-element to/alternatives) are kept on one line
- Detects semantic conflicts between spelling rules (--lint)
- Online verification of to-terms via Wikipedia/zh and MoE dict (--verify)
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


def dedup_sort(rules: list[dict[str, Any]], key: str) -> list[dict[str, Any]]:
    seen: set[str] = set()
    out: list[dict[str, Any]] = []
    for rule in rules:
        k = rule[key]
        if k not in seen:
            seen.add(k)
            out.append(rule)
    return sorted(out, key=lambda r: r[key])


# Valid rule types (must match RuleType enum in src/rules/ruleset.rs).
VALID_RULE_TYPES = {
    "cross_strait",
    "variant",
    "typo",
    "confusable",
    "political_coloring",
    "ai_filler",
}

# All known spelling rule fields (anything else is an unknown key warning).
KNOWN_SPELLING_FIELDS = {
    "from",
    "to",
    "type",
    "disabled",
    "context",
    "english",
    "exceptions",
    "context_clues",
    "negative_context_clues",
    "tags",
}

# Field order for spelling rules (stable, human-scannable output).
SPELLING_FIELD_ORDER = [
    "from",
    "to",
    "type",
    "disabled",
    "context",
    "english",
    "context_clues",
    "negative_context_clues",
    "exceptions",
    "tags",
]

CASE_FIELD_ORDER = ["term", "alternatives", "disabled"]


def ordered_rule(rule: dict[str, Any], order: list[str]) -> dict[str, Any]:
    """Return a dict with keys in the specified order, extras appended."""
    out: dict[str, Any] = {}
    for k in order:
        if k in rule:
            out[k] = rule[k]
    for k in rule:
        if k not in out:
            out[k] = rule[k]
    return out


def format_rule(rule: dict[str, Any], base: str = "    ") -> str:
    """Format a single rule object with compact arrays."""
    inner = base + "  "
    lines = [base + "{"]
    items = list(rule.items())
    for i, (key, value) in enumerate(items):
        comma = "," if i < len(items) - 1 else ""
        val_str = json.dumps(value, ensure_ascii=False)
        lines.append(f'{inner}"{key}": {val_str}{comma}')
    lines.append(base + "}")
    return "\n".join(lines)


def format_ruleset(data: dict[str, Any]) -> str:
    """Format the entire ruleset with compact rule objects."""
    parts = ["{"]

    for section_idx, (section_key, order) in enumerate(
        [
            ("spelling_rules", SPELLING_FIELD_ORDER),
            ("case_rules", CASE_FIELD_ORDER),
        ]
    ):
        rules = data[section_key]
        parts.append(f'  "{section_key}": [')
        for i, rule in enumerate(rules):
            ordered = ordered_rule(rule, order)
            comma = "," if i < len(rules) - 1 else ""
            rule_str = format_rule(ordered)
            if comma:
                # Append comma to the closing brace line
                rule_str = rule_str[:-1] + "},"
            parts.append(rule_str)
        section_comma = "," if section_idx == 0 else ""
        parts.append(f"  ]{section_comma}")

    parts.append("}")
    return "\n".join(parts) + "\n"


# ---------------------------------------------------------------------------
# Online verification helpers (--verify)
# ---------------------------------------------------------------------------

_HTTP_HEADERS = {"User-Agent": "zhtw-mcp-check/1.0"}
_RATE_LIMIT = 0.25  # seconds between requests
# Bump when the lookup algorithm changes to invalidate stale entries.
_CACHE_VERSION = 2

# Sentinel: distinguish "confirmed missing" from "network error".
_NETWORK_ERROR = object()


def _http_get_json(url: str) -> dict[str, Any] | None:
    """GET *url* and return parsed JSON, None on 404, _NETWORK_ERROR otherwise."""
    req = urllib.request.Request(url, headers=_HTTP_HEADERS)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
            if not isinstance(data, dict):
                return _NETWORK_ERROR  # type: ignore[return-value]
            return data
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None  # confirmed missing
        return _NETWORK_ERROR  # type: ignore[return-value]
    except Exception:
        return _NETWORK_ERROR  # type: ignore[return-value]


def wiki_zh_exists(term: str) -> tuple[bool | None, str | None]:
    """Check whether *term* is a recognized zh-TW term on zh.wikipedia.org.

    Two-tier lookup:
    1. Title match with zh-TW variant conversion (strong: dedicated page).
    2. Full-text search fallback (weaker: term appears in any article).

    Returns (True, title), (False, None) for confirmed missing,
    or (None, None) on network error.
    """
    # Tier 1: exact title with zh-TW variant conversion.
    params = urllib.parse.urlencode(
        {
            "action": "query",
            "titles": term,
            "format": "json",
            "redirects": "1",
            "converttitles": "1",
            "variant": "zh-tw",
        }
    )
    data = _http_get_json(f"https://zh.wikipedia.org/w/api.php?{params}")
    if data is _NETWORK_ERROR:
        return None, None
    if data:
        pages = data.get("query", {}).get("pages", {})
        for pid, page in pages.items():
            if "missing" not in page:
                return True, page.get("title")

    # Tier 2: search — does the term appear anywhere in zh Wikipedia?
    params = urllib.parse.urlencode(
        {
            "action": "query",
            "list": "search",
            "srsearch": term,
            "format": "json",
            "srlimit": "1",
            "srnamespace": "0",
        }
    )
    time.sleep(_RATE_LIMIT)
    data = _http_get_json(f"https://zh.wikipedia.org/w/api.php?{params}")
    if data is _NETWORK_ERROR:
        return None, None
    if not data:
        return False, None
    hits = data.get("query", {}).get("search", [])
    if hits:
        return True, hits[0].get("title")
    return False, None


def moedict_exists(word: str) -> tuple[bool | None, str | None]:
    """Look up *word* in the MoE Revised Mandarin Dictionary (moedict.tw).

    Returns (True, title), (False, None) for confirmed missing,
    or (None, None) on network error.
    """
    encoded = urllib.parse.quote(word)
    data = _http_get_json(f"https://www.moedict.tw/a/{encoded}.json")
    if data is _NETWORK_ERROR:
        return None, None
    if not data:
        return False, None
    title = data.get("t", "").replace("`", "").replace("~", "")
    return True, title or word


def _load_cache(cache_path: Path) -> dict[str, Any]:
    if cache_path.exists():
        try:
            data = json.loads(cache_path.read_text(encoding="utf-8"))
            if not isinstance(data, dict):
                print("  cache: unexpected format, discarding", file=sys.stderr)
                return {"version": _CACHE_VERSION, "wiki": {}, "moedict": {}}
            if data.get("version") == _CACHE_VERSION:
                return data
            # Version mismatch: discard stale cache.
            print(
                f"  cache version mismatch (have {data.get('version')}, "
                f"want {_CACHE_VERSION}), discarding",
                file=sys.stderr,
            )
        except (json.JSONDecodeError, OSError):
            pass
    return {"version": _CACHE_VERSION, "wiki": {}, "moedict": {}}


def _atomic_write(path: Path, content: str) -> None:
    """Write *content* to *path* atomically via temp + rename."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(content, encoding="utf-8")
    tmp.replace(path)


def _save_cache(cache_path: Path, cache: dict[str, Any]) -> None:
    cache["version"] = _CACHE_VERSION
    _atomic_write(
        cache_path,
        json.dumps(cache, ensure_ascii=False, indent=2) + "\n",
    )


def verify_terms(
    spelling_rules: list[dict[str, Any]],
    cache_path: Path,
) -> tuple[list[str], int]:
    """Verify to-field terms online.

    1. Wikipedia/zh: check that each non-empty to-term is a recognized
       zh-TW term (page exists or redirects to one).
    2. MoE dict: for variant rules, confirm the to-character is in the
       Ministry of Education dictionary.

    Returns (warnings, net_errors).
    """
    cache = _load_cache(cache_path)
    wiki_cache: dict[str, bool] = cache.get("wiki", {})
    moe_cache: dict[str, bool] = cache.get("moedict", {})
    warnings: list[str] = []

    # Collect unique to-terms and variant to-terms.
    all_to: dict[str, list[str]] = {}  # term -> [from_keys that reference it]
    variant_to: dict[str, list[str]] = {}

    for rule in spelling_rules:
        if rule.get("disabled"):
            continue
        from_key = rule["from"]
        for target in rule.get("to", []):
            if not target:
                continue
            all_to.setdefault(target, []).append(from_key)
            if rule.get("type") == "variant":
                variant_to.setdefault(target, []).append(from_key)

    def _checkpoint() -> None:
        cache["wiki"] = wiki_cache
        cache["moedict"] = moe_cache
        _save_cache(cache_path, cache)

    net_errors = 0

    # --- Wikipedia/zh verification ---
    wiki_todo = [t for t in all_to if t not in wiki_cache]
    total_wiki = len(wiki_todo)
    if total_wiki:
        print(
            f"  Wikipedia/zh: checking {total_wiki} uncached terms ...",
            file=sys.stderr,
            flush=True,
        )
    for i, term in enumerate(wiki_todo):
        exists, _ = wiki_zh_exists(term)
        if exists is None:
            net_errors += 1  # network error -- don't cache
        else:
            wiki_cache[term] = exists
        if (i + 1) % 50 == 0:
            print(f"    ... {i + 1}/{total_wiki}", file=sys.stderr, flush=True)
            _checkpoint()
        time.sleep(_RATE_LIMIT)

    wiki_missing = [t for t in all_to if wiki_cache.get(t) is False]
    for term in sorted(wiki_missing):
        sources = ", ".join(all_to[term][:3])
        if len(all_to[term]) > 3:
            sources += f" +{len(all_to[term]) - 3} more"
        warnings.append(f'wikipedia missing: "{term}" (from: {sources})')

    # --- MoE dict verification (variant rules only) ---
    # For multi-character terms (e.g. 臺北), the variant rule validates the
    # character form, not the compound word.  If the full term is absent from
    # moedict, fall back to checking each individual character -- all must be
    # present for the term to pass.
    moe_todo = [t for t in variant_to if t not in moe_cache]
    total_moe = len(moe_todo)
    if total_moe:
        print(
            f"  MoE dict: checking {total_moe} uncached variant terms ...",
            file=sys.stderr,
            flush=True,
        )
    for i, term in enumerate(moe_todo):
        exists, _ = moedict_exists(term)
        if exists is None:
            net_errors += 1
            continue  # network error -- skip, don't cache
        if not exists and len(term) > 1:
            # Fall back: check each character individually.
            all_chars_ok = True
            skip = False
            for ch in term:
                if ch in moe_cache:
                    if not moe_cache[ch]:
                        all_chars_ok = False
                        break
                    continue
                ch_exists, _ = moedict_exists(ch)
                if ch_exists is None:
                    net_errors += 1
                    skip = True
                    break
                moe_cache[ch] = ch_exists
                time.sleep(_RATE_LIMIT)
                if not ch_exists:
                    all_chars_ok = False
                    break
            if skip:
                continue  # don't cache partial result
            exists = all_chars_ok
        moe_cache[term] = exists
        if (i + 1) % 20 == 0:
            print(f"    ... {i + 1}/{total_moe}", file=sys.stderr, flush=True)
            _checkpoint()
        time.sleep(_RATE_LIMIT)

    moe_missing = [t for t in variant_to if moe_cache.get(t) is False]
    for term in sorted(moe_missing):
        sources = ", ".join(variant_to[term][:3])
        if len(variant_to[term]) > 3:
            sources += f" +{len(variant_to[term]) - 3} more"
        warnings.append(f'moedict missing: "{term}" (variant from: {sources})')

    if net_errors:
        print(
            f"  warning: {net_errors} network errors (not cached, retry later)",
            file=sys.stderr,
        )

    # Persist cache.
    cache["wiki"] = wiki_cache
    cache["moedict"] = moe_cache
    _save_cache(cache_path, cache)

    return warnings, net_errors


# Valid @domain labels (canonical single-label taxonomy).
VALID_DOMAINS = {
    "IT",
    "UI",
    "程式設計",
    "作業系統",
    "硬體",
    "電子",
    "網路",
    "通訊",
    "資安",
    "資料結構",
    "資料庫",
    "資料",
    "雲端",
    "數學",
    "科學",
    "語言學",
    "醫學",
    "金融",
    "商業",
    "電商",
    "社群",
    "教育",
    "日常",
    "圖形",
    "航太",
    "文書",
    "版本控制",
    "系統程式",
    "軟體授權",
    "生物學",
    "能源",
    "材料",
}

# Valid @geo sub-types.
VALID_GEO_TYPES = {"country", "city", "landmark", "university"}


def detect_conflicts(spelling_rules: list[dict[str, Any]]) -> list[str]:
    """Detect semantic conflicts between spelling rules.

    Skips disabled rules.  Returns a list of warning strings for:
    1.  Circular mappings (to of rule A is from of rule B)
    2.  Empty to without english fallback
    3.  Variant rule invariants (single non-empty to)
    4.  Orphaned seealso references in context fields
    5.  AC compound decomposition conflicts (individual rules would produce
        wrong output for a compound term that lacks its own rule)
    6.  Suggestion-is-from conflicts (a rule's to[] value is another rule's
        from, creating unintended re-flagging)
    7.  Schema validation (required fields, valid types, unknown keys)
    8.  Compound suffix preservation (longer rules must not drop suffixes
        that the base rule would preserve)
    9.  context_clues / negative_context_clues field validation
    10. Self-referencing to (from value appears in its own to array)
    11. Annotation validation (@domain/@geo tag format and coverage)
    12. Redundant domain constraint (限X語境 duplicates @domain X)
    13. Ungated domain constraint (限...語境 without context_clues/exceptions)
    14. ai_filler trailing punctuation (scanner handles it automatically)
    """
    warnings: list[str] = []

    # All rules (including disabled) for seealso reference validation.
    all_from: dict[str, dict[str, Any]] = {r["from"]: r for r in spelling_rules}
    # Active-only for structural checks (cycles, empty-to, variant invariants).
    from_set: dict[str, dict[str, Any]] = {
        k: v for k, v in all_from.items() if not v.get("disabled")
    }

    # 1. Circular: detect actual cycles (A→B→...→A) in to→from chains.
    #    A chain A→B→C that terminates (C∉from_set) is fine — converges.
    #    Only A→B→...→A (cycle) means zh_check fix mode never converges.
    reported: set[str] = set()
    for rule in from_set.values():
        start = rule["from"]
        if start in reported:
            continue
        stack = [(start, [start])]
        found = False
        while stack and not found:
            node, path = stack.pop()
            node_rule = from_set.get(node)
            if not node_rule:
                continue
            for target in node_rule.get("to", []):
                if target == start:
                    # Context-gated cycles are safe: if every rule in the
                    # cycle has context_clues, the rules have mutually
                    # exclusive firing conditions and fix mode converges.
                    cycle_rules = [from_set[n] for n in path if n in from_set]
                    all_gated = all(r.get("context_clues") for r in cycle_rules)
                    if all_gated:
                        reported.update(path)
                        found = True
                        break
                    cycle = path + [target]
                    chain = " -> ".join(f'"{p}"' for p in cycle)
                    warnings.append(f"circular: {chain}")
                    reported.update(path)
                    found = True
                    break
                if target in from_set and target not in set(path):
                    stack.append((target, path + [target]))

    # 2. Empty to requires non-empty english (use English form convention).
    #    Exception: ai_filler rules use empty to intentionally (deletion).
    for rule in from_set.values():
        targets = [t for t in rule.get("to", []) if t]
        if not targets:
            if rule.get("type") == "ai_filler":
                continue
            english = rule.get("english", "")
            if not english:
                warnings.append(
                    f'empty to without english: "{rule["from"]}" '
                    f'needs "english" as fallback'
                )

    # 3. Variant rules: must have single non-empty to.
    for rule in from_set.values():
        if rule.get("type") == "variant":
            targets = [t for t in rule.get("to", []) if t]
            if len(targets) != 1:
                warnings.append(
                    f'variant to count: "{rule["from"]}" has {len(targets)} '
                    f"non-empty to entries, expected exactly 1"
                )

    # 4. Orphaned seealso (check against all rules, including disabled).
    for rule in from_set.values():
        ctx = rule.get("context", "")
        for m in re.finditer(r"\(@seealso\s+([^)]+)\)", ctx):
            for ref_name in m.group(1).split(","):
                ref_name = ref_name.strip()
                if ref_name and ref_name not in all_from:
                    warnings.append(
                        f'orphan seealso: "{rule["from"]}" references '
                        f'"{ref_name}" (not found)'
                    )

    # 5. AC compound decomposition: detect multi-char 'from' patterns whose
    #    individual characters are each 'from' keys of other rules.  Without
    #    a dedicated compound rule, LeftmostLongest AC may match the shorter
    #    individual rules and produce concatenated gibberish.
    #    Example: 堆棧 without its own rule → 堆→堆積 + 棧→堆疊 = 堆積堆疊
    single_char_from: dict[str, str] = {}
    for rule in from_set.values():
        if len(rule["from"]) == 1:
            targets = [t for t in rule.get("to", []) if t]
            if targets:
                single_char_from[rule["from"]] = targets[0]
    for rule in from_set.values():
        frm = rule["from"]
        if len(frm) < 2:
            continue
        # Check if every character in 'from' is itself a single-char rule.
        decomposable_chars = [ch for ch in frm if ch in single_char_from]
        if len(decomposable_chars) >= 2 and len(decomposable_chars) == len(frm):
            # The compound has a rule — good.  But check if its 'to' would
            # differ from naively concatenating individual replacements.
            naive = "".join(single_char_from[ch] for ch in frm)
            targets = [t for t in rule.get("to", []) if t]
            if targets and targets[0] != naive:
                # This is fine — the compound rule overrides the naive result.
                pass
            elif not targets:
                warnings.append(
                    f'compound decomposition: "{frm}" has no to[] but '
                    f'individual rules would produce "{naive}"'
                )
    # Also check that existing compound rules whose 'from' can be
    # decomposed into single-char rules have correct 'to' values
    # (i.e., the compound rule isn't accidentally doing the same thing
    # as naive concatenation when it shouldn't, or vice versa).
    # We intentionally do NOT enumerate all possible 2-char pairs — that
    # produces a noisy cartesian product.  Instead we rely on the compound
    # decomposition check above for existing rules and on manual review
    # for new compound terms.

    # 6. Suggestion-is-from: a rule's to[] value is another active rule's
    #    from key.  This means applying fix mode once leaves a term that
    #    will be re-flagged on the next scan — the fix doesn't converge in
    #    one pass.  Chains that terminate (A→B, B→C, C∉from) are fine
    #    (caught by circular check above).  Flag single-hop re-flagging.
    for rule in from_set.values():
        for target in rule.get("to", []):
            if not target:
                continue
            if target in from_set and target != rule["from"]:
                target_rule = from_set[target]
                # Skip if the target rule has context_clues (it won't
                # always fire, so re-flagging is conditional).
                if target_rule.get("context_clues"):
                    continue
                target_to = [t for t in target_rule.get("to", []) if t]
                if target_to:
                    warnings.append(
                        f'suggestion-is-from: "{rule["from"]}" suggests '
                        f'"{target}" which is from of another rule '
                        f"(→ {target_to[0]}); fix mode needs 2 passes"
                    )

    # 7. Schema validation: required fields, valid types, unknown keys.
    for rule in spelling_rules:
        frm = rule.get("from")
        if not frm:
            warnings.append("schema: rule missing required 'from' field")
            continue
        if "to" not in rule:
            warnings.append(f"schema: \"{frm}\" missing required 'to' field")
        if "type" not in rule:
            warnings.append(f"schema: \"{frm}\" missing required 'type' field")
        else:
            rtype = rule["type"]
            if rtype not in VALID_RULE_TYPES:
                warnings.append(
                    f'schema: "{frm}" has unknown type "{rtype}" '
                    f"(valid: {', '.join(sorted(VALID_RULE_TYPES))})"
                )
        unknown = set(rule.keys()) - KNOWN_SPELLING_FIELDS
        if unknown:
            warnings.append(f'schema: "{frm}" has unknown fields: {sorted(unknown)}')

    # 8. Compound suffix preservation: when a longer rule A contains a
    #    shorter rule B as prefix, AND both produce the same prefix in
    #    their replacement, A must not silently drop the remaining suffix.
    #    Example: "批量處理" → "批次" is wrong (drops 處理);
    #             "批量" → "批次" + 處理 = "批次處理" is correct.
    #
    #    Whole-phrase translations where the zh-TW term is structurally
    #    different (e.g. 航天飛機→太空梭, 調製解調器→數據機) are NOT
    #    flagged — the compound replacement is a distinct lexical item.
    #    We detect this by checking if the compound's to[] starts with
    #    the base rule's to[] — if not, it's a whole-phrase replacement.
    for rule in from_set.values():
        frm = rule["from"]
        targets = [t for t in rule.get("to", []) if t]
        if not targets or len(frm) < 3:
            continue
        for base_rule in from_set.values():
            base_frm = base_rule["from"]
            if base_frm == frm or len(base_frm) >= len(frm):
                continue
            if not frm.startswith(base_frm):
                continue
            base_targets = [t for t in base_rule.get("to", []) if t]
            if not base_targets:
                continue
            suffix = frm[len(base_frm) :]
            compound_result = targets[0]
            base_to = base_targets[0]
            # Only flag if the compound's replacement shares the same
            # prefix as the base rule's replacement — this means the
            # compound is doing a prefix swap and dropping the suffix.
            # Whole-phrase replacements (different prefix) are intentional.
            if not compound_result.startswith(base_to):
                continue
            # Flag only when the compound result equals the base result
            # exactly (suffix completely discarded) or the compound result
            # is just the base_to with no suffix replacement at all.
            # Suffix transformations (文件→檔, 程序→器) are intentional.
            #
            # Exclude: when base_to already ends with the suffix, the
            # compound rule exists to absorb it and prevent doubling
            # (e.g. SQL隱碼攻擊 already contains 攻擊; 公車 contains 車).
            if compound_result == base_to and not base_to.endswith(suffix):
                # Skip when the base rule already lists the compound's
                # from term as an exception — the scanner will never apply
                # the base rule to that compound, so no conflict in practice.
                base_exceptions = base_rule.get("exceptions", [])
                if frm in base_exceptions:
                    continue
                base_result = base_to + suffix
                warnings.append(
                    f'compound-suffix: "{frm}" → "{compound_result}" '
                    f'drops suffix "{suffix}"; base rule "{base_frm}" '
                    f'→ "{base_to}" would give "{base_result}"'
                )

    # 9. context_clues / negative_context_clues validation.
    for rule in from_set.values():
        frm = rule["from"]
        for field in ("context_clues", "negative_context_clues"):
            clues = rule.get(field)
            if clues is None:
                continue
            if not isinstance(clues, list):
                warnings.append(f'clue-type: "{frm}" {field} must be a list')
                continue
            if not clues:
                warnings.append(f'clue-empty: "{frm}" has empty {field} list')
            for clue in clues:
                if not clue or not clue.strip():
                    warnings.append(f'clue-blank: "{frm}" has blank entry in {field}')
        # Overlap: same term in both positive and negative clues.
        pos = set(rule.get("context_clues") or [])
        neg = set(rule.get("negative_context_clues") or [])
        overlap = pos & neg
        if overlap:
            warnings.append(
                f'clue-overlap: "{frm}" has terms in both context_clues '
                f"and negative_context_clues: {sorted(overlap)}"
            )

    # 10. Self-referencing to: from value must not appear in its own to array.
    for rule in from_set.values():
        frm = rule["from"]
        if frm in rule.get("to", []):
            warnings.append(
                f'self-ref: "{frm}" appears in its own to array '
                f"(identity suggestion)"
            )

    # 11. Annotation validation: @domain and @geo tags.
    #
    # Rules that use structured annotations:
    #   @geo TYPE (LABEL)        -- geographic entities
    #   @domain LABEL            -- domain-specific terms
    #   @domain LABEL。note      -- domain + disambiguation
    #
    # cross_strait rules must have one of: @domain, @geo, (@seealso ...),
    # or compound: prefix.  Bare prose without a structured tag is flagged.
    # Anchored: after the tag, only 。(note) or end-of-string is valid.
    geo_re = re.compile(r"^@geo\s+(\w+)\s*(?:\([^)]*\))?\s*(?:。|$)")
    domain_re = re.compile(r"^@domain\s+([^。\s]+)\s*(?:。|$)")

    for rule in from_set.values():
        frm = rule["from"]
        ctx = rule.get("context", "")
        rtype = rule.get("type", "")

        # Only cross_strait rules need annotation.
        if rtype != "cross_strait":
            continue

        # Check @geo format.
        geo_m = geo_re.match(ctx)
        if geo_m:
            geo_type = geo_m.group(1)
            if geo_type not in VALID_GEO_TYPES:
                warnings.append(
                    f'geo-type: "{frm}" has unknown @geo type '
                    f'"{geo_type}" (valid: {", ".join(sorted(VALID_GEO_TYPES))})'
                )
            continue  # has @geo -- skip further annotation checks

        # Detect malformed @geo (starts with @geo but regex didn't match).
        if ctx.startswith("@geo"):
            warnings.append(
                f'geo-malformed: "{frm}" has malformed @geo tag: ' f'"{ctx[:40]}"'
            )
            continue

        # Check @domain format.
        dom_m = domain_re.match(ctx)
        if dom_m:
            domain = dom_m.group(1)
            if domain not in VALID_DOMAINS:
                warnings.append(
                    f'domain-label: "{frm}" has unknown @domain ' f'"{domain}"'
                )
            continue  # has @domain -- skip further annotation checks

        # Detect malformed @domain (starts with @domain but regex didn't match).
        if ctx.startswith("@domain"):
            warnings.append(
                f'domain-malformed: "{frm}" has malformed @domain tag: ' f'"{ctx[:40]}"'
            )
            continue

        # Other structured annotations: (@seealso ...) and compound: are
        # acceptable without a @domain/@geo prefix.  Match the actual
        # (@seealso REF) syntax, not a bare substring.
        if "(@seealso " in ctx or ctx.startswith("compound:"):
            continue

        # cross_strait rules must have a structured annotation tag.
        # Bare prose context without @domain/@geo is flagged so new rules
        # are required to declare their domain explicitly.
        warnings.append(
            f'annotation-missing: "{frm}" has no @domain/@geo tag'
            + (f' (context: "{ctx[:30]}...")' if ctx.strip() else "")
        )

    # Duplicate @geo: same to[0] from multiple from values.
    # OpenCC character variants (裡/里, 羣/群, 託/托) intentionally produce
    # duplicate from→to mappings so both text forms are caught.  Only flag
    # true duplicates where the from values are identical or differ by more
    # than single-character variant swaps.
    geo_to_map: dict[str, list[str]] = {}
    for rule in from_set.values():
        ctx = rule.get("context", "")
        if not ctx.startswith("@geo"):
            continue
        targets = [t for t in rule.get("to", []) if t]
        if targets:
            geo_to_map.setdefault(targets[0], []).append(rule["from"])
    for to_val, froms in geo_to_map.items():
        if len(froms) <= 1:
            continue
        # Check if all pairs differ by only single-char substitutions
        # (OpenCC variant pairs like 裡/里).  If so, it is intentional.
        # NOTE: this is a Hamming-distance heuristic, not true OpenCC
        # normalization.  It tolerates ≤2 char diffs at equal length.
        # For geographic names this is sufficient — two unrelated countries
        # with same-length names differing by ≤2 chars is not realistic.
        is_variant_pair = True
        for i in range(len(froms)):
            for j in range(i + 1, len(froms)):
                a, b = froms[i], froms[j]
                if len(a) != len(b):
                    is_variant_pair = False
                    break
                diffs = sum(1 for x, y in zip(a, b) if x != y)
                if diffs > 2:  # allow up to 2 char differences
                    is_variant_pair = False
                    break
            if not is_variant_pair:
                break
        if not is_variant_pair:
            warnings.append(
                f'geo-duplicate: {froms} all map to "{to_val}" '
                f"(redundant @geo rules)"
            )

    # 12. Redundant domain constraint: @domain X + 限X語境 in the same
    #     context is redundant — the @domain tag already declares the domain.
    for rule in from_set.values():
        frm = rule["from"]
        ctx = rule.get("context", "")
        dom_m = domain_re.match(ctx)
        if not dom_m:
            continue
        domain = dom_m.group(1)
        if f"限{domain}語境" in ctx:
            warnings.append(
                f'domain-redundant: "{frm}" has @domain {domain} '
                f"and redundant 限{domain}語境"
            )

    # 13. Ungated domain constraint: context says 限...語境 but the rule
    #     lacks context_clues, negative_context_clues, and exceptions to
    #     enforce it.  This is a latent false-positive bug per CLAUDE.md
    #     conventions.  Rules with negative_context_clues are considered
    #     gated (they fire by default and are suppressed in wrong contexts).
    limit_re = re.compile(r"限[^。]+語境")
    for rule in from_set.values():
        frm = rule["from"]
        ctx = rule.get("context", "")
        if not limit_re.search(ctx):
            continue
        has_clues = bool(rule.get("context_clues"))
        has_neg_clues = bool(rule.get("negative_context_clues"))
        has_exceptions = bool(rule.get("exceptions"))
        if not has_clues and not has_neg_clues and not has_exceptions:
            m = limit_re.search(ctx)
            constraint = m.group(0) if m else "?"
            warnings.append(
                f'ungated-constraint: "{frm}" says "{constraint}" '
                f"but has no context_clues or exceptions"
            )

    # 14. ai_filler trailing punctuation: the scanner extends deletion
    #     spans (is_deletion_rule: to == [""]) to consume trailing ，/：
    #     automatically.  Separate rules for phrase+punctuation variants
    #     are redundant only when the base rule is a deletion rule.
    #     Replacement ai_filler rules (to == ["總之"] etc.) do NOT get
    #     automatic trailing-punctuation handling.
    ai_filler_deletion_from = {
        r["from"]
        for r in from_set.values()
        if r.get("type") == "ai_filler"
        and len(r.get("to", [])) == 1
        and r["to"][0] == ""
    }
    for rule in from_set.values():
        if rule.get("type") != "ai_filler":
            continue
        frm = rule["from"]
        if frm.endswith("\uff0c") or frm.endswith("\uff1a"):  # ， or ：
            base = frm[:-1]
            if base in ai_filler_deletion_from:
                punct = frm[-1]
                warnings.append(
                    f'ai-filler-punct: "{frm}" is redundant — '
                    f'base rule "{base}" is a deletion rule and scanner '
                    f"auto-consumes trailing {punct}"
                )

    return warnings


def default_path() -> Path:
    return Path(__file__).resolve().parent.parent / "assets" / "ruleset.json"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", nargs="?", type=Path, default=default_path())
    parser.add_argument(
        "--lint",
        action="store_true",
        help="detect conflicts without rewriting the file (exit 1 if any)",
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help="online verification of to-terms via Wikipedia/zh and MoE dict",
    )
    parser.add_argument(
        "--cache",
        type=Path,
        default=Path(__file__).resolve().parent.parent / ".verify-cache.json",
        help="path to verification cache file (default: .verify-cache.json)",
    )
    args = parser.parse_args()
    path: Path = args.path

    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        print(f"error: file not found: {path}", file=sys.stderr)
        return 1
    except json.JSONDecodeError as e:
        print(f"error: invalid JSON in {path}: {e}", file=sys.stderr)
        return 1

    for key in ("spelling_rules", "case_rules"):
        if key not in data or not isinstance(data[key], list):
            print(f'error: missing or invalid "{key}" in {path}', file=sys.stderr)
            return 1

    orig_spelling = len(data["spelling_rules"])
    orig_case = len(data["case_rules"])

    data["spelling_rules"] = dedup_sort(data["spelling_rules"], "from")
    data["case_rules"] = dedup_sort(data["case_rules"], "term")

    new_spelling = len(data["spelling_rules"])
    new_case = len(data["case_rules"])
    removed = (orig_spelling - new_spelling) + (orig_case - new_case)

    # Detect semantic conflicts in spelling rules.
    conflicts = detect_conflicts(data["spelling_rules"])
    if conflicts:
        print(f"conflicts ({len(conflicts)}):", file=sys.stderr)
        for w in conflicts:
            print(f"  {w}", file=sys.stderr)

    # Online verification (opt-in).
    verify_warnings: list[str] = []
    verify_net_errors = 0
    if args.verify:
        verify_warnings, verify_net_errors = verify_terms(
            data["spelling_rules"],
            args.cache,
        )
        if verify_warnings:
            print(f"verify ({len(verify_warnings)}):", file=sys.stderr)
            for w in verify_warnings:
                print(f"  {w}", file=sys.stderr)
        else:
            print("verify: all terms confirmed", file=sys.stderr)

    if args.lint:
        print(
            f"ruleset: {new_spelling} spelling + {new_case} case"
            f" ({removed} duplicates removed)"
        )
        if conflicts or verify_warnings:
            return 1
        if verify_net_errors:
            print(
                "error: incomplete verification due to network errors", file=sys.stderr
            )
            return 1
        return 0

    _atomic_write(path, format_ruleset(data))

    print(
        f"ruleset: {new_spelling} spelling + {new_case} case"
        f" ({removed} duplicates removed)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
