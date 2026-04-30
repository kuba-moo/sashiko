#!/usr/bin/env python3
"""Reconstruct per-stage tool-call trajectories from one or more sashiko dumps.

Each sashiko stage runs until the model emits a final text answer with no
tool calls. The full run of tool calls before that answer is the stage's
"trajectory". This script presents each trajectory and highlights:

  - long trajectories (>= --long N calls) — one stage asking N questions
  - recurring tool-sequence bigrams / trigrams (e.g., sc_find_function ->
    search_file_content) — candidates to merge into one semantic tool
  - per-trajectory "focus tokens": symbols/paths the model kept coming back
    to, even if interleaved with side-quests

Usage:
    scripts/analyze_call_chains.py <dump_dir> [<dump_dir> ...]
    scripts/analyze_call_chains.py <dump_dir> --long 6
    scripts/analyze_call_chains.py <dump_dir> -v           # dump each call
    scripts/analyze_call_chains.py <dump_dir> --ngrams 3   # show trigrams
    scripts/analyze_call_chains.py <dump_dir> --json
"""
import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

RESP_RE = re.compile(r"^(s\w+)_(\d+)_resp\.json$")
REQ_RE = re.compile(r"^(s\w+)_(\d+)_req\.json$")

BASIC_TOOLS = {"read_files", "search_file_content", "find_files",
               "git_show", "git_log", "git_diff", "git_blame"}
SEMANTIC_TOOLS = {"sc_find_function", "sc_find_callers", "sc_grep_functions",
                  "sc_find_definition"}


def _tokens(s):
    if not isinstance(s, str):
        return set()
    return {t for t in re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", s)}


def _arg_tokens(tool, args):
    toks = set()
    if tool in ("search_file_content", "sc_grep_functions"):
        toks |= _tokens(args.get("pattern", ""))
    elif tool in ("sc_find_function", "sc_find_definition", "sc_find_callers"):
        toks |= _tokens(args.get("name", "") or args.get("symbol", ""))
    elif tool == "read_files":
        for e in args.get("files", []) or []:
            toks |= _tokens(e.get("path", ""))
    elif tool == "find_files":
        toks |= _tokens(args.get("pattern", "") or args.get("glob", ""))
    elif tool == "git_show":
        toks |= _tokens(args.get("object", ""))
    return toks


def _arg_paths(tool, args):
    paths = []
    if tool == "read_files":
        for e in args.get("files", []) or []:
            p = e.get("path")
            if p:
                paths.append(p)
    elif tool in ("git_show", "git_blame"):
        obj = args.get("object") or args.get("path", "")
        m = re.match(r"^[0-9a-fA-F]+(?:[~^]\d*)*:(.+)$", obj)
        if m:
            paths.append(m.group(1))
        elif args.get("path"):
            paths.append(args["path"])
    elif tool in ("search_file_content", "sc_grep_functions"):
        p = args.get("path") or args.get("path_pattern")
        if p:
            paths.append(p)
    return paths


def _short(s, n=140):
    if not isinstance(s, str):
        s = str(s)
    s = s.replace("\n", " ")
    return s if len(s) <= n else s[: n - 3] + "..."


def _result_preview(result_content):
    if not isinstance(result_content, str):
        return ""
    try:
        data = json.loads(result_content)
        if isinstance(data, dict) and "content" in data:
            return _short(data["content"])
    except (json.JSONDecodeError, TypeError):
        pass
    return _short(result_content)


def _result_hit_count(result_content):
    try:
        data = json.loads(result_content)
        text = data.get("content", "") if isinstance(data, dict) else str(data)
    except (json.JSONDecodeError, TypeError):
        text = result_content if isinstance(result_content, str) else ""
    if not text.strip():
        return 0
    return text.count("\n") + 1


def load_dump(dump_dir: Path):
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
    stage_final_text = {}
    for f in sorted(dump_dir.iterdir()):
        m = RESP_RE.match(f.name)
        if not m:
            continue
        sid, turn = m.group(1), int(m.group(2))
        try:
            resp = json.loads(f.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        tcs = resp.get("tool_calls") or []
        content = resp.get("content") or ""
        if not tcs and content:
            stage_final_text[sid] = content
            continue
        for tc in tcs:
            name = tc.get("function_name", "")
            args = tc.get("arguments", {}) or {}
            tcid = tc.get("id", "")
            result = tc_result.get(tcid, "")
            stages[sid].append({
                "turn": turn,
                "tool": name,
                "args": args,
                "result": result,
                "result_hits": _result_hit_count(result),
            })
    return stages, stage_final_text


def classify_trajectory(calls):
    """Return list of tags describing patterns in the trajectory."""
    tags = []
    tools = [c["tool"] for c in calls]
    counter = Counter(tools)
    n = len(calls)
    if n == 0:
        return ["empty"]

    basic = sum(counter.get(t, 0) for t in BASIC_TOOLS)
    sem = sum(counter.get(t, 0) for t in SEMANTIC_TOOLS)
    if sem == 0 and basic >= 3:
        tags.append(f"basic_only({basic})")
    if counter.get("search_file_content", 0) >= 4:
        tags.append(f"heavy_grep({counter['search_file_content']})")
    if counter.get("read_files", 0) >= 4:
        tags.append(f"heavy_read({counter['read_files']})")

    # scanning the same file across many read_files calls
    read_paths = []
    for c in calls:
        if c["tool"] == "read_files":
            read_paths.extend(_arg_paths(c["tool"], c["args"]))
    rp_counter = Counter(read_paths)
    multiread = [(p, n_) for p, n_ in rp_counter.items() if n_ >= 3]
    if multiread:
        tags.append("scan_same_file:" + ",".join(f"{p}({n_}x)" for p, n_ in multiread))

    # repeated greps against same path (probing)
    grep_paths = []
    for c in calls:
        if c["tool"] == "search_file_content":
            grep_paths.extend(_arg_paths(c["tool"], c["args"]))
    gp_counter = Counter(grep_paths)
    multigrep = [(p, n_) for p, n_ in gp_counter.items() if n_ >= 3]
    if multigrep:
        tags.append("probe_same_file:" + ",".join(f"{p}({n_}x)" for p, n_ in multigrep))

    # sc_find_function then grep: symbol lookup fallback pattern
    for a, b in zip(tools, tools[1:]):
        if a == "sc_find_function" and b == "search_file_content":
            tags.append("sc_find_then_grep")
            break
    return tags


def focus_tokens(calls, top=5):
    counter = Counter()
    for c in calls:
        counter.update(_arg_tokens(c["tool"], c["args"]))
    return counter.most_common(top)


def compute_ngrams(all_trajs, n=2):
    """Given {label: [tool_sequence]}, count ngrams across all trajectories."""
    counter = Counter()
    for seq in all_trajs:
        for i in range(len(seq) - n + 1):
            counter[tuple(seq[i:i + n])] += 1
    return counter


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dump_dir", type=Path, nargs="+")
    ap.add_argument("--long", type=int, default=6,
                    help="trajectory length above which to flag as long (default 6)")
    ap.add_argument("--ngrams", type=int, default=2,
                    help="size of tool-sequence ngrams to report (default 2)")
    ap.add_argument("--ngrams-top", type=int, default=15)
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="print every tool call with a preview of its result")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    all_dumps = {}
    all_tool_seqs = []
    for d in args.dump_dir:
        if not d.is_dir():
            print(f"skip: {d}", file=sys.stderr)
            continue
        stages, final = load_dump(d)
        dump_info = {"stages": {}}
        for sid in sorted(stages):
            calls = stages[sid]
            tool_seq = [c["tool"] for c in calls]
            all_tool_seqs.append(tool_seq)
            dump_info["stages"][sid] = {
                "n_calls": len(calls),
                "tools": tool_seq,
                "tags": classify_trajectory(calls),
                "focus": focus_tokens(calls),
                "is_long": len(calls) >= args.long,
                "calls": calls if args.verbose else None,
            }
        all_dumps[str(d)] = dump_info

    ngrams = compute_ngrams(all_tool_seqs, n=args.ngrams)

    if args.json:
        for di in all_dumps.values():
            for s in di["stages"].values():
                s.pop("calls", None)
        out = {
            "dumps": all_dumps,
            "top_ngrams": [{"ngram": list(k), "count": v}
                           for k, v in ngrams.most_common(args.ngrams_top)],
        }
        json.dump(out, sys.stdout, indent=2, default=str)
        print()
        return

    for dpath, di in all_dumps.items():
        print(f"\n########## {dpath} ##########")
        for sid in sorted(di["stages"]):
            s = di["stages"][sid]
            flag = "  ** LONG" if s["is_long"] else ""
            print(f"\n  --- {sid}: {s['n_calls']} calls{flag} ---")
            if s["tags"]:
                print(f"    tags: {'; '.join(s['tags'])}")
            if s["focus"]:
                tok_str = ", ".join(f"{t}({n})" for t, n in s["focus"])
                print(f"    focus tokens: {tok_str}")
            tool_str = " -> ".join(s["tools"])
            print(f"    sequence: {_short(tool_str, 200)}")
            if args.verbose and s.get("calls"):
                for c in s["calls"]:
                    preview = _result_preview(c["result"])[:70]
                    print(f"      t{c['turn']:03d} {c['tool']:22s} "
                          f"{_short(json.dumps(c['args']), 90)} "
                          f"=> hits={c['result_hits']} {preview}")

    print(f"\n\n########## Top {args.ngrams}-grams across all stages ##########")
    for ng, count in ngrams.most_common(args.ngrams_top):
        print(f"  {count:4d}x  {' -> '.join(ng)}")


if __name__ == "__main__":
    main()
