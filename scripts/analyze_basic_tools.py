#!/usr/bin/env python3
"""Detect places where the model falls back to basic tools (read_files,
search_file_content, git_show, find_files) in ways that suggest it is
scanning because a semantically-richer tool would have served better.

Signals reported per stage:
  1. Overlapping reads — multiple read_files calls on the same file with
     overlapping or adjacent line ranges (model widening its view).
  2. Grep refinement — multiple search_file_content calls sharing a core
     token but with different patterns (model tweaking the query because
     earlier patterns matched nothing / too much).
  3. Empty-result basic calls — search_file_content/sc_* returning 0 hits
     followed immediately by another basic call (fallback scanning).
  4. Basic call for a known symbol — grep for a pattern that matches a C
     identifier that could have been resolved by sc_find_function /
     sc_find_definition instead.
  5. git_show against HEAD:path — redundant with the current tree readable
     by read_files (and likely with prefetched context).

Usage:
    scripts/analyze_basic_tools.py <dump_dir> [<dump_dir> ...]
    scripts/analyze_basic_tools.py <dump_dir> --json
    scripts/analyze_basic_tools.py <dump_dir> -v
"""
import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

RESP_RE = re.compile(r"^(s\w+)_(\d+)_resp\.json$")
REQ_RE = re.compile(r"^(s\w+)_(\d+)_req\.json$")

IDENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def _tokens(s):
    if not isinstance(s, str):
        return []
    return re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", s)


def _short(s, n=120):
    if not isinstance(s, str):
        s = str(s)
    s = s.replace("\n", " ")
    return s if len(s) <= n else s[: n - 3] + "..."


def _result_text(result_content):
    if not isinstance(result_content, str):
        return ""
    try:
        data = json.loads(result_content)
        if isinstance(data, dict):
            return data.get("content", "") or ""
    except (json.JSONDecodeError, TypeError):
        pass
    return result_content


def _hit_count(result_content):
    text = _result_text(result_content)
    if not text.strip():
        return 0
    return text.count("\n") + 1


def load_calls(dump_dir: Path):
    tc_result = {}
    for f in sorted(dump_dir.iterdir()):
        if not REQ_RE.match(f.name):
            continue
        try:
            msgs = json.loads(f.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        if isinstance(msgs, dict):
            msgs = msgs.get("messages", [])
        if not isinstance(msgs, list):
            continue
        for m in msgs:
            if not isinstance(m, dict) or m.get("role") != "tool":
                continue
            tcid = m.get("tool_call_id", "")
            content = m.get("content", "")
            if tcid and isinstance(content, str):
                tc_result[tcid] = content

    stages = defaultdict(list)
    for f in sorted(dump_dir.iterdir()):
        m = RESP_RE.match(f.name)
        if not m:
            continue
        sid, turn = m.group(1), int(m.group(2))
        try:
            resp = json.loads(f.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        for tc in resp.get("tool_calls") or []:
            tcid = tc.get("id", "")
            stages[sid].append({
                "turn": turn,
                "tool": tc.get("function_name", ""),
                "args": tc.get("arguments", {}) or {},
                "result": tc_result.get(tcid, ""),
                "result_hits": _hit_count(tc_result.get(tcid, "")),
            })
    return stages


# --- Signal detectors ---

def sig_overlapping_reads(calls):
    """List of (file, [(start,end,turn), ...]) for files read >=2 times
    with ranges that overlap or are within 20 lines of each other."""
    by_file = defaultdict(list)
    for c in calls:
        if c["tool"] != "read_files":
            continue
        for entry in c["args"].get("files", []) or []:
            p = entry.get("path")
            s, e = entry.get("start_line"), entry.get("end_line")
            if p:
                try:
                    s_i = int(s) if s is not None else None
                    e_i = int(e) if e is not None else None
                except (TypeError, ValueError):
                    s_i = e_i = None
                by_file[p].append((s_i, e_i, c["turn"]))

    results = []
    for p, ranges in by_file.items():
        if len(ranges) < 2:
            continue
        # Check if any two ranges are close (<20 lines apart)
        def overlaps(a, b):
            if None in a or None in b:
                return True  # whole-file reads
            return not (a[1] < b[0] - 20 or b[1] < a[0] - 20)
        close = False
        for i in range(len(ranges)):
            for j in range(i + 1, len(ranges)):
                if overlaps(ranges[i][:2], ranges[j][:2]):
                    close = True
                    break
            if close:
                break
        if close:
            results.append((p, ranges))
    return results


def sig_grep_refinement(calls):
    """Groups of search_file_content calls that share tokens — model
    tweaking the pattern."""
    greps = [c for c in calls if c["tool"] == "search_file_content"]
    groups = defaultdict(list)
    for c in greps:
        pat = c["args"].get("pattern", "")
        # Anchor on the longest identifier-ish token
        toks = sorted(set(_tokens(pat)), key=lambda t: -len(t))
        key = toks[0] if toks else pat
        groups[key].append(c)
    return [(k, g) for k, g in groups.items() if len(g) >= 2]


def sig_empty_then_basic(calls):
    """Pairs: empty-result call immediately followed by another basic call
    on an overlapping token. Indicates the model didn't find what it wanted
    and fell back to scanning."""
    issues = []
    for a, b in zip(calls, calls[1:]):
        if a["result_hits"] != 0:
            continue
        # Did they share any token?
        a_toks = set(_tokens(json.dumps(a["args"])))
        b_toks = set(_tokens(json.dumps(b["args"])))
        if not (a_toks & b_toks):
            continue
        if b["tool"] in ("read_files", "search_file_content", "find_files"):
            issues.append((a, b))
    return issues


def sig_symbol_lookup_via_grep(calls):
    """search_file_content where the pattern is essentially a bare C
    identifier — sc_find_function/sc_find_definition would have done it."""
    issues = []
    for c in calls:
        if c["tool"] != "search_file_content":
            continue
        pat = c["args"].get("pattern", "")
        # Skip if pattern has alternation / wildcards — the model is
        # deliberately broadening the search, which sc_find_function
        # can't replicate.
        if any(ch in pat for ch in "|*+?"):
            continue
        stripped = re.sub(r"[\\^$.()\[\]{}]", "", pat).strip()
        if IDENT_RE.match(stripped) and len(stripped) >= 4:
            issues.append((c, stripped))
    return issues


def sig_git_show_head(calls):
    issues = []
    for c in calls:
        if c["tool"] != "git_show":
            continue
        obj = c["args"].get("object", "")
        if ":" in obj:
            ref, path = obj.split(":", 1)
            if ref in ("HEAD", "HEAD^0", "HEAD~0"):
                issues.append((c, path))
    return issues


def analyze_stage(sid, calls):
    return {
        "n_calls": len(calls),
        "overlapping_reads": sig_overlapping_reads(calls),
        "grep_refinement": sig_grep_refinement(calls),
        "empty_then_basic": sig_empty_then_basic(calls),
        "symbol_via_grep": sig_symbol_lookup_via_grep(calls),
        "git_show_head": sig_git_show_head(calls),
    }


def print_stage(sid, info, verbose=False):
    parts = []
    if info["overlapping_reads"]:
        parts.append(f"{len(info['overlapping_reads'])} files scanned (read_files)")
    if info["grep_refinement"]:
        parts.append(f"{len(info['grep_refinement'])} grep groups refined")
    if info["empty_then_basic"]:
        parts.append(f"{len(info['empty_then_basic'])} empty-then-basic")
    if info["symbol_via_grep"]:
        parts.append(f"{len(info['symbol_via_grep'])} symbol-via-grep")
    if info["git_show_head"]:
        parts.append(f"{len(info['git_show_head'])} git_show-HEAD")
    if not parts:
        return
    print(f"\n  --- {sid}: {info['n_calls']} calls — {'; '.join(parts)} ---")

    for p, ranges in info["overlapping_reads"]:
        rng_str = ", ".join(f"L{s}-{e}@t{t}" for s, e, t in ranges)
        print(f"    [scan_same_file] {p}: {rng_str}")

    for key, group in info["grep_refinement"]:
        print(f"    [grep_refinement] token='{key}' — {len(group)} calls:")
        for c in group:
            p = c["args"].get("path", "?")
            pat = _short(c["args"].get("pattern", ""), 80)
            print(f"      t{c['turn']:03d} path={p} pattern={pat} hits={c['result_hits']}")

    for a, b in info["empty_then_basic"]:
        print(f"    [empty->basic] t{a['turn']} {a['tool']} (0 hits) -> "
              f"t{b['turn']} {b['tool']}")
        if verbose:
            print(f"      first: {_short(json.dumps(a['args']), 100)}")
            print(f"      next:  {_short(json.dumps(b['args']), 100)}")

    for c, ident in info["symbol_via_grep"]:
        print(f"    [symbol_via_grep] t{c['turn']} pattern='{ident}' "
              f"path={c['args'].get('path', '?')}  — could be sc_find_function/sc_find_definition")

    for c, path in info["git_show_head"]:
        print(f"    [git_show_HEAD] t{c['turn']} object={c['args'].get('object')}  "
              f"(use read_files for {path})")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dump_dir", type=Path, nargs="+")
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    all_results = {}
    grand_counters = Counter()
    for d in args.dump_dir:
        if not d.is_dir():
            print(f"skip: {d}", file=sys.stderr)
            continue
        stages = load_calls(d)
        dump_info = {}
        for sid in sorted(stages):
            info = analyze_stage(sid, stages[sid])
            dump_info[sid] = info
            grand_counters["overlapping_reads"] += len(info["overlapping_reads"])
            grand_counters["grep_refinement"] += len(info["grep_refinement"])
            grand_counters["empty_then_basic"] += len(info["empty_then_basic"])
            grand_counters["symbol_via_grep"] += len(info["symbol_via_grep"])
            grand_counters["git_show_head"] += len(info["git_show_head"])
        all_results[str(d)] = dump_info

    if args.json:
        def _ser(o):
            if isinstance(o, tuple):
                return list(o)
            return str(o)
        json.dump(all_results, sys.stdout, indent=2, default=_ser)
        print()
        return

    for dpath, dumps in all_results.items():
        print(f"\n########## {dpath} ##########")
        any_signals = False
        for sid in sorted(dumps):
            info = dumps[sid]
            if any(info[k] for k in ("overlapping_reads", "grep_refinement",
                                     "empty_then_basic", "symbol_via_grep",
                                     "git_show_head")):
                print_stage(sid, info, verbose=args.verbose)
                any_signals = True
        if not any_signals:
            print("  (no basic-tool scan signals detected)")

    print(f"\n########## Totals ##########")
    for k, v in grand_counters.most_common():
        print(f"  {k:<22} {v}")


if __name__ == "__main__":
    main()
