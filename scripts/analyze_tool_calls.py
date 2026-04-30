#!/usr/bin/env python3
"""Analyze tool call patterns from a sashiko conversation dump.

For each unique tool+args combination, shows which stages called it and how
many times — highlighting cross-stage duplication and intra-stage repeats.

Usage:
    scripts/analyze_tool_calls.py <dump_dir>
    scripts/analyze_tool_calls.py <dump_dir> --tool search_file_content
    scripts/analyze_tool_calls.py <dump_dir> --duplicates-only
"""
import argparse
import json
import os
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path


RESP_RE = re.compile(r"^(s\w+)_(\d+)_resp\.json$")


def parse_args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("dump_dir", type=Path, help="Path to conversation dump directory")
    p.add_argument("--tool", type=str, default=None, help="Filter to a specific tool name")
    p.add_argument("--duplicates-only", action="store_true", help="Only show calls made by multiple stages or repeated within a stage")
    p.add_argument("--json", action="store_true", help="Output as JSON")
    return p.parse_args()


def canonicalize_args(args):
    """Produce a stable string key from tool arguments for dedup."""
    return json.dumps(args, sort_keys=True, separators=(",", ":"))


def load_tool_calls(dump_dir: Path):
    """Returns list of (stage, turn, tool_name, args_dict, args_key)."""
    calls = []
    for f in sorted(dump_dir.iterdir()):
        m = RESP_RE.match(f.name)
        if not m:
            continue
        sid, turn = m.group(1), int(m.group(2))
        try:
            resp = json.loads(f.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        for tc in resp.get("tool_calls", []):
            name = tc.get("function_name", "")
            args = tc.get("arguments", {})
            key = canonicalize_args(args)
            calls.append((sid, turn, name, args, key))
    return calls


def fmt_args(args, max_len=100):
    """Format args dict as a compact one-liner."""
    parts = []
    for k, v in args.items():
        if isinstance(v, str) and len(v) > 60:
            v = v[:57] + "..."
        parts.append(f"{k}={json.dumps(v)}")
    s = ", ".join(parts)
    if len(s) > max_len:
        s = s[:max_len - 3] + "..."
    return s


def main():
    args = parse_args()

    if not args.dump_dir.is_dir():
        print(f"Error: {args.dump_dir} is not a directory", file=sys.stderr)
        sys.exit(1)

    calls = load_tool_calls(args.dump_dir)
    if not calls:
        print(f"Error: no tool calls found in {args.dump_dir}", file=sys.stderr)
        sys.exit(1)

    # --- Summary ---
    tool_counts = Counter()
    stage_set = set()
    for sid, turn, name, _, _ in calls:
        tool_counts[name] += 1
        stage_set.add(sid)

    # --- Group by (tool_name, args_key) ---
    # For each unique call: which stages, how many times per stage
    groups = defaultdict(lambda: {"args": None, "stages": defaultdict(int)})
    for sid, turn, name, tool_args, key in calls:
        gk = (name, key)
        groups[gk]["args"] = tool_args
        groups[gk]["stages"][sid] += 1

    # Classify each group
    records = []
    for (name, key), info in groups.items():
        stages = info["stages"]
        stage_count = len(stages)
        total_calls = sum(stages.values())
        max_per_stage = max(stages.values())
        cross_stage_dup = stage_count > 1
        intra_stage_dup = max_per_stage > 1

        flags = []
        if cross_stage_dup:
            flags.append(f"cross-stage({stage_count})")
        if intra_stage_dup:
            repeats = {s: n for s, n in stages.items() if n > 1}
            parts = ",".join(f"{s}:{n}x" for s, n in sorted(repeats.items()))
            flags.append(f"repeated({parts})")

        records.append({
            "tool": name,
            "args": info["args"],
            "stages": dict(stages),
            "total_calls": total_calls,
            "stage_count": stage_count,
            "cross_stage_dup": cross_stage_dup,
            "intra_stage_dup": intra_stage_dup,
            "flags": flags,
        })

    # Filter
    if args.tool:
        records = [r for r in records if r["tool"] == args.tool]
    if args.duplicates_only:
        records = [r for r in records if r["cross_stage_dup"] or r["intra_stage_dup"]]

    # Sort: most duplicated first, then by tool name
    records.sort(key=lambda r: (-r["total_calls"], r["tool"], canonicalize_args(r["args"])))

    if args.json:
        json.dump(records, sys.stdout, indent=2, default=str)
        print()
        return

    # --- Print summary ---
    print(f"Stages: {', '.join(sorted(stage_set))}")
    print(f"Total tool calls: {sum(tool_counts.values())}")
    print(f"Unique tool+args combinations: {len(groups)}")
    print()

    print(f"{'tool':<25} {'calls':>5} {'unique':>6} {'dup%':>5}")
    print("-" * 45)
    for name, count in tool_counts.most_common():
        if args.tool and name != args.tool:
            continue
        unique = sum(1 for r in records if r["tool"] == name)
        dup_calls = sum(r["total_calls"] - 1 for r in records if r["tool"] == name and (r["cross_stage_dup"] or r["intra_stage_dup"]))
        dup_pct = dup_calls / count * 100 if count else 0
        print(f"  {name:<23} {count:>5} {unique:>6} {dup_pct:>4.0f}%")

    # --- Detailed per-call breakdown ---
    print()
    if args.duplicates_only:
        print("=== Duplicated calls only ===")
    else:
        print("=== All unique tool+args ===")
    print()

    current_tool = None
    for r in records:
        if r["tool"] != current_tool:
            current_tool = r["tool"]
            print(f"--- {current_tool} ---")

        stage_parts = []
        for s in sorted(r["stages"]):
            n = r["stages"][s]
            stage_parts.append(f"{s}" if n == 1 else f"{s}({n}x)")
        stage_str = " ".join(stage_parts)

        flag_str = ""
        if r["flags"]:
            flag_str = "  ** " + ", ".join(r["flags"])

        print(f"  [{stage_str}] {fmt_args(r['args'])}{flag_str}")

    # --- Cross-stage duplication summary ---
    cross = [r for r in records if r["cross_stage_dup"]]
    if cross:
        print()
        print(f"=== Cross-stage duplicates: {len(cross)} unique calls repeated across stages ===")
        wasted = sum(r["total_calls"] - 1 for r in cross)
        print(f"    {wasted} redundant calls could be eliminated by carrying results forward")


if __name__ == "__main__":
    main()
