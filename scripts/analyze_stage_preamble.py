#!/usr/bin/env python3
"""Detect per-stage actions that repeat across reviews, i.e. work that
could be baked into the upfront prompt context instead of costing a
round-trip every time.

Given a set of dump directories:
  1. Group by stage id (s0..s9, sp).
  2. For each stage, count how many reviews invoked each tool at any
     point, and how often each tool appeared in the FIRST turn.
  3. Flag tools that appear in >= --threshold fraction of reviews for a
     stage — those are "always-fetched" candidates.
  4. For the flagged tools, show the distinct args patterns used (so we
     can see whether they resolve to symbols/files already in the patch
     diff — if so, prefetching can trivially cover them).

Usage:
    scripts/analyze_stage_preamble.py <dump_dir> <dump_dir> [...]
    scripts/analyze_stage_preamble.py <dump_dir> ... --threshold 0.75
    scripts/analyze_stage_preamble.py <dump_dir> ... --json
"""
import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

RESP_RE = re.compile(r"^(s\w+)_(\d+)_resp\.json$")


def _short(s, n=120):
    if not isinstance(s, str):
        s = str(s)
    s = s.replace("\n", " ")
    return s if len(s) <= n else s[: n - 3] + "..."


def load_stage_calls(dump_dir: Path):
    """Returns {stage: [ (turn, tool, args), ... ]}."""
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
            stages[sid].append((turn, tc.get("function_name", ""), tc.get("arguments", {}) or {}))
    return stages


def _args_summary(tool, args):
    """Reduce args to the semantic key we care about."""
    if tool in ("sc_find_function", "sc_find_definition", "sc_find_callers"):
        return args.get("name", "") or args.get("symbol", "")
    if tool in ("search_file_content", "sc_grep_functions"):
        path = args.get("path") or args.get("path_pattern") or ""
        return f"grep in {path}"
    if tool == "read_files":
        files = args.get("files", []) or []
        paths = sorted({(e.get("path") or "") for e in files})
        return f"read {','.join(paths)}"
    if tool == "find_files":
        return f"glob {args.get('pattern') or args.get('glob') or ''}"
    if tool == "git_show":
        return f"git_show {args.get('object', '')}"
    if tool == "git_log":
        return f"git_log {args.get('path', '')}"
    if tool == "git_diff":
        return "git_diff"
    return json.dumps(args, sort_keys=True)[:120]


def analyze(all_dump_calls, threshold):
    """all_dump_calls: {dump_path: {stage: [(turn,tool,args)...]}}."""
    # Per stage: how many dumps included that stage at all?
    stages_seen = defaultdict(int)
    # Per (stage, tool): how many dumps used it
    stage_tool_dumps = defaultdict(set)
    # Per (stage, tool): which turns it appeared in (list of turn nums, dump-qualified)
    stage_tool_turns = defaultdict(list)
    # Per (stage, tool, args-summary): count across dumps
    stage_tool_args = defaultdict(lambda: defaultdict(int))
    # First-call tool per (stage, dump)
    stage_first_tool = defaultdict(list)
    # Call count per (stage, dump)
    stage_call_counts = defaultdict(list)

    for dump, stage_map in all_dump_calls.items():
        for sid, calls in stage_map.items():
            stages_seen[sid] += 1
            stage_call_counts[sid].append(len(calls))
            if calls:
                stage_first_tool[sid].append(calls[0][1])
            tools_in_dump = set()
            for turn, tool, args in calls:
                tools_in_dump.add(tool)
                stage_tool_turns[(sid, tool)].append(turn)
                stage_tool_args[(sid, tool)][_args_summary(tool, args)] += 1
            for t in tools_in_dump:
                stage_tool_dumps[(sid, t)].add(dump)

    findings = {}
    for sid, n_dumps in sorted(stages_seen.items()):
        per_stage = {
            "n_reviews": n_dumps,
            "call_counts": stage_call_counts[sid],
            "first_tool_distribution": dict(Counter(stage_first_tool[sid])),
            "always_tools": [],
            "by_tool": {},
        }
        # Collect tools present in ALL stage entries
        tool_names = sorted({t for (s, t) in stage_tool_dumps if s == sid})
        for tool in tool_names:
            n_with = len(stage_tool_dumps[(sid, tool)])
            frac = n_with / n_dumps if n_dumps else 0
            args_dist = dict(stage_tool_args[(sid, tool)])
            per_stage["by_tool"][tool] = {
                "n_reviews_used": n_with,
                "fraction": frac,
                "turn_distribution": sorted(stage_tool_turns[(sid, tool)]),
                "args": args_dist,
            }
            if frac >= threshold and n_dumps >= 2:
                per_stage["always_tools"].append(tool)
        findings[sid] = per_stage
    return findings


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dump_dir", type=Path, nargs="+")
    ap.add_argument("--threshold", type=float, default=0.75,
                    help="fraction of reviews in which a tool must appear to be 'always' (default 0.75)")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    all_calls = {}
    for d in args.dump_dir:
        if not d.is_dir():
            print(f"skip: {d}", file=sys.stderr)
            continue
        all_calls[str(d)] = load_stage_calls(d)

    findings = analyze(all_calls, threshold=args.threshold)

    if args.json:
        json.dump(findings, sys.stdout, indent=2, default=str)
        print()
        return

    print(f"Analyzed {len(all_calls)} dumps (threshold={args.threshold:.0%})")
    for sid, info in findings.items():
        print(f"\n===== stage {sid} — {info['n_reviews']} reviews, "
              f"call counts: {info['call_counts']} =====")
        ft = info["first_tool_distribution"]
        ft_str = ", ".join(f"{t}:{n}" for t, n in sorted(ft.items(), key=lambda x: -x[1]))
        print(f"  first-call tool: {ft_str}")
        if info["always_tools"]:
            print(f"  always-used tools (>= {args.threshold:.0%}): {', '.join(info['always_tools'])}")
        else:
            print(f"  no tool hits the threshold")
        for tool, d in sorted(info["by_tool"].items(), key=lambda kv: -kv[1]["fraction"]):
            flag = " ***" if tool in info["always_tools"] else ""
            print(f"    - {tool:24s} {d['n_reviews_used']}/{info['n_reviews']} "
                  f"({d['fraction']:.0%}){flag}")
            # Top args patterns
            args_sorted = sorted(d["args"].items(), key=lambda kv: -kv[1])
            for a, n in args_sorted[:5]:
                print(f"        {n}x  {_short(a, 90)}")


if __name__ == "__main__":
    main()
