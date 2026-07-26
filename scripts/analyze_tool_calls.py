#!/usr/bin/env python3
"""Analyze tool call patterns from a sashiko conversation dump.

For each unique tool+args combination, shows which stages called it and how
many times — highlighting cross-stage duplication and intra-stage repeats.

With --heatmap, builds a per-file, per-line access count from tools that
reference file locations (read_files, search_file_content, git_blame,
git_show with SHA:path objects).  Lines are counted from tool arguments
(start_line/end_line ranges) and, for search_file_content, from the
grep-style line numbers in the tool result.

Usage:
    scripts/analyze_tool_calls.py <dump_dir>
    scripts/analyze_tool_calls.py <dump_dir> --tool search_file_content
    scripts/analyze_tool_calls.py <dump_dir> --duplicates-only
    scripts/analyze_tool_calls.py <dump_dir> --heatmap
    scripts/analyze_tool_calls.py <dump_dir> --heatmap --heatmap-file net/core/dev.c
    scripts/analyze_tool_calls.py <dump_dir> --heatmap --heatmap-file net/core/dev.c --repo /path/to/linux
"""
import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path


RESP_RE = re.compile(r"^(s\w+)_(\d+)_resp\.json$")
REQ_RE = re.compile(r"^(s\w+)_(\d+)_req\.json$")


def parse_args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("dump_dir", type=Path, help="Path to conversation dump directory")
    p.add_argument("--tool", type=str, default=None, help="Filter to a specific tool name")
    p.add_argument("--duplicates-only", action="store_true", help="Only show calls made by multiple stages or repeated within a stage")
    p.add_argument("--json", action="store_true", help="Output as JSON")
    p.add_argument("--heatmap", action="store_true", help="Show per-file, per-line access heatmap")
    p.add_argument("--heatmap-file", type=str, default=None, help="Filter heatmap to a specific file path (substring match)")
    p.add_argument("--heatmap-top", type=int, default=30, help="Number of top lines to show per file (default: 30)")
    p.add_argument("--repo", type=Path, default=None, help="Path to the source repo; when set, heatmap shows actual file contents")
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


# ---- Heatmap support --------------------------------------------------------

GREP_LINE_RE = re.compile(r"^(.+?)[:-](\d+)[:-]")
GIT_SHOW_OBJ_RE = re.compile(r"^[0-9a-fA-F]+(?:[~^]\d*)*:(.+)$")


def _build_tool_result_map(dump_dir: Path):
    """Map tool_call_id -> result content string by scanning *_req.json files."""
    result_map = {}
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
        for msg in msgs:
            if msg.get("role") != "tool":
                continue
            tcid = msg.get("tool_call_id", "")
            content = msg.get("content", "")
            if tcid and isinstance(content, str):
                result_map[tcid] = content
    return result_map


def _parse_grep_lines(result_content: str):
    """Extract (file, line_no) pairs from grep-style tool result."""
    hits = []
    try:
        data = json.loads(result_content)
        text = data.get("content", "")
    except (json.JSONDecodeError, TypeError):
        return hits
    for raw_line in text.split("\n"):
        m = GREP_LINE_RE.match(raw_line)
        if m:
            hits.append((m.group(1), int(m.group(2))))
    return hits


def _line_range(start, end):
    """Generate line numbers for a range, clamping to reasonable bounds."""
    if start is None or end is None:
        return []
    try:
        start, end = int(start), int(end)
    except (TypeError, ValueError):
        return []
    if start < 1 or end < start or (end - start) > 50_000:
        return []
    return list(range(start, end + 1))


def build_heatmap(dump_dir: Path):
    """Build {file: {line: {"count": N, "stages": set, "tools": set}}} from dump.

    Sources:
      - read_files: path + start_line/end_line from args
      - search_file_content: grep-style lines from result content
      - git_blame: path + start_line/end_line from args
      - git_show: object "SHA:path" + start_line/end_line from args
    """
    result_map = _build_tool_result_map(dump_dir)
    heatmap = defaultdict(lambda: defaultdict(lambda: {"count": 0, "stages": set(), "tools": set()}))

    for f in sorted(dump_dir.iterdir()):
        m = RESP_RE.match(f.name)
        if not m:
            continue
        sid = m.group(1)
        try:
            resp = json.loads(f.read_text())
        except (json.JSONDecodeError, OSError):
            continue

        for tc in resp.get("tool_calls", []):
            name = tc.get("function_name", "")
            args = tc.get("arguments", {})
            tcid = tc.get("id", "")

            if name == "read_files":
                for entry in args.get("files", []):
                    path = entry.get("path", "")
                    if not path:
                        continue
                    lines = _line_range(entry.get("start_line"), entry.get("end_line"))
                    if lines:
                        for ln in lines:
                            heatmap[path][ln]["count"] += 1
                            heatmap[path][ln]["stages"].add(sid)
                            heatmap[path][ln]["tools"].add(name)
                    else:
                        heatmap[path][0]["count"] += 1
                        heatmap[path][0]["stages"].add(sid)
                        heatmap[path][0]["tools"].add(name)

            elif name == "search_file_content":
                if tcid and tcid in result_map:
                    for fpath, ln in _parse_grep_lines(result_map[tcid]):
                        heatmap[fpath][ln]["count"] += 1
                        heatmap[fpath][ln]["stages"].add(sid)
                        heatmap[fpath][ln]["tools"].add(name)

            elif name == "git_blame":
                path = args.get("path", "")
                if not path:
                    continue
                lines = _line_range(args.get("start_line"), args.get("end_line"))
                if lines:
                    for ln in lines:
                        heatmap[path][ln]["count"] += 1
                        heatmap[path][ln]["stages"].add(sid)
                        heatmap[path][ln]["tools"].add(name)
                else:
                    heatmap[path][0]["count"] += 1
                    heatmap[path][0]["stages"].add(sid)
                    heatmap[path][0]["tools"].add(name)

            elif name == "git_show":
                obj = args.get("object", "")
                gm = GIT_SHOW_OBJ_RE.match(obj)
                if not gm:
                    continue
                path = gm.group(1)
                lines = _line_range(args.get("start_line"), args.get("end_line"))
                if lines:
                    for ln in lines:
                        heatmap[path][ln]["count"] += 1
                        heatmap[path][ln]["stages"].add(sid)
                        heatmap[path][ln]["tools"].add(name)
                else:
                    heatmap[path][0]["count"] += 1
                    heatmap[path][0]["stages"].add(sid)
                    heatmap[path][0]["tools"].add(name)

    return heatmap


def _load_file_lines(repo: Path, rel_path: str):
    """Load file contents as a dict mapping 1-based line number -> text."""
    full = repo / rel_path
    try:
        raw_lines = full.read_text(errors="replace").splitlines()
    except OSError:
        return {}
    return {i + 1: raw_lines[i] for i in range(len(raw_lines))}


def print_heatmap(heatmap, file_filter=None, top_n=30, as_json=False, repo=None):
    """Print per-file line-level access heatmap."""
    # Convert sets to lists for JSON serialization
    serializable = {}
    for fpath in sorted(heatmap):
        if file_filter and file_filter not in fpath:
            continue
        file_lines = {}
        for ln, info in sorted(heatmap[fpath].items()):
            file_lines[ln] = {
                "count": info["count"],
                "stages": sorted(info["stages"]),
                "tools": sorted(info["tools"]),
            }
        serializable[fpath] = file_lines

    if as_json:
        if repo:
            for fpath, lines in serializable.items():
                src = _load_file_lines(repo, fpath)
                for ln, info in lines.items():
                    info["source"] = src.get(ln, "")
        json.dump(serializable, sys.stdout, indent=2)
        print()
        return

    # Per-file summary sorted by total accesses
    file_stats = []
    for fpath, lines in serializable.items():
        total_hits = sum(v["count"] for v in lines.values())
        unique_lines = len([ln for ln in lines if ln > 0])
        all_stages = set()
        all_tools = set()
        for v in lines.values():
            all_stages.update(v["stages"])
            all_tools.update(v["tools"])
        file_stats.append((fpath, total_hits, unique_lines, all_stages, all_tools))

    file_stats.sort(key=lambda x: -x[1])

    if not repo:
        print(f"{'file':<60} {'hits':>6} {'lines':>6} {'stages':>6} {'via'}")
        print("-" * 100)
        for fpath, hits, unique_lines, stages, tools in file_stats:
            display_path = fpath if len(fpath) <= 58 else "..." + fpath[-55:]
            print(f"  {display_path:<58} {hits:>6} {unique_lines:>6} {len(stages):>6}   {','.join(sorted(tools))}")
        print()

    # Detailed per-file breakdown
    for fpath, hits, unique_lines, stages, tools in file_stats:
        lines = serializable[fpath]
        line_items = [(ln, info) for ln, info in lines.items() if ln > 0]

        src = _load_file_lines(repo, fpath) if repo else {}

        if repo:
            # With repo: show lines in file order so the code reads naturally.
            # Group into contiguous ranges, show each range with hit annotations.
            line_items.sort(key=lambda x: x[0])
            print(f"=== {fpath} ({hits} total hits, {unique_lines} unique lines, stages: {','.join(sorted(stages))}) ===")
            if not line_items:
                print("  (whole-file access only, no line-level data)")
                print()
                continue

            line_nums = sorted(ln for ln, _ in line_items)
            ranges = _coalesce_ranges(line_nums)
            count_map = {ln: info["count"] for ln, info in line_items}
            stage_map = {ln: info["stages"] for ln, info in line_items}

            for rng_start, rng_end in ranges:
                max_count = max(count_map.get(ln, 0) for ln in range(rng_start, rng_end + 1))
                rng_stages = set()
                for ln in range(rng_start, rng_end + 1):
                    rng_stages.update(stage_map.get(ln, []))
                print(f"  --- L{rng_start}-{rng_end} (max {max_count}x, stages: {','.join(sorted(rng_stages))}) ---")
                for ln in range(rng_start, rng_end + 1):
                    cnt = count_map.get(ln, 0)
                    src_line = src.get(ln, "")
                    if len(src_line) > 100:
                        src_line = src_line[:97] + "..."
                    bar = "#" * min(cnt, 20) if cnt else " "
                    print(f"  {ln:>6} {cnt:>2}x {bar:<20} {src_line}")
            print()
        else:
            line_items.sort(key=lambda x: (-x[1]["count"], x[0]))
            print(f"=== {fpath} ({hits} total hits, {unique_lines} unique lines, stages: {','.join(sorted(stages))}) ===")

            if not line_items:
                print("  (whole-file access only, no line-level data)")
                print()
                continue

            shown = line_items[:top_n]
            for ln, info in shown:
                stage_str = ",".join(info["stages"])
                tool_str = ",".join(info["tools"])
                bar = "#" * min(info["count"], 40)
                print(f"  L{ln:<6} {info['count']:>3}x  [{stage_str:<30}] {tool_str:<25} {bar}")

            if len(line_items) > top_n:
                print(f"  ... and {len(line_items) - top_n} more lines")

            all_lines_sorted = sorted(ln for ln, _ in line_items)
            ranges = _coalesce_ranges(all_lines_sorted)
            if ranges:
                print(f"  Hot ranges: {_fmt_ranges(ranges)}")
            print()


def _coalesce_ranges(lines):
    """Merge sorted line numbers into (start, end) ranges."""
    if not lines:
        return []
    ranges = []
    start = prev = lines[0]
    for ln in lines[1:]:
        if ln <= prev + 2:
            prev = ln
        else:
            ranges.append((start, prev))
            start = prev = ln
    ranges.append((start, prev))
    return ranges


def _fmt_ranges(ranges):
    parts = []
    for s, e in ranges:
        if s == e:
            parts.append(str(s))
        else:
            parts.append(f"{s}-{e}")
    return ", ".join(parts)


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

    if args.heatmap:
        hm = build_heatmap(args.dump_dir)
        if not hm:
            print(f"Error: no file-referencing tool calls found in {args.dump_dir}", file=sys.stderr)
            sys.exit(1)
        print_heatmap(hm, file_filter=args.heatmap_file, top_n=args.heatmap_top, as_json=args.json, repo=args.repo)
        return

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
