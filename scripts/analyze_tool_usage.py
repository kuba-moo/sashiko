#!/usr/bin/env python3
"""Classify tool_usages rows as redundant / partial / novel vs pre-fetched context.

Usage:
    scripts/analyze_tool_usage.py --db sashiko.db --prefetch /tmp/prefetch.txt [--review-id N]
"""
import argparse
import json
import re
import sqlite3
import sys
from collections import Counter, defaultdict

HEADER_RE = re.compile(
    r"^--- Extracted (?:Context from|Definition of (\S+) from) (\S+) ---$"
)


def parse_prefetch(text):
    """Returns (files_with_full_body, symbol_definitions, file_text_map).

    files_with_full_body: set of file paths that got an "Extracted Context from"
        block (i.e., the full enclosing function/struct body was included).
    symbol_definitions: dict sym -> set of file paths where its definition was
        pre-fetched.
    file_text_map: dict file_path -> concatenated prefetched text for that file
        (useful for substring checks).
    """
    files_full = set()
    sym_defs = defaultdict(set)
    file_text = defaultdict(list)

    current_path = None
    for line in text.splitlines():
        m = HEADER_RE.match(line)
        if m:
            sym, path = m.group(1), m.group(2)
            current_path = path
            if sym is None:
                files_full.add(path)
            else:
                sym_defs[sym].add(path)
            continue
        if current_path is not None:
            file_text[current_path].append(line)

    return files_full, sym_defs, {p: "\n".join(ls) for p, ls in file_text.items()}


def classify_read_files(args, files_full, file_text):
    """One call may touch multiple files; classify per-file then collapse."""
    entries = args.get("files", [])
    verdicts = []
    for e in entries:
        path = e.get("path", "")
        start = e.get("start_line")
        end = e.get("end_line")
        if path in files_full:
            # We have the full enclosing block. If the caller asked for an
            # explicit range, we can't be 100% sure the range overlaps — but
            # in practice the prefetched block IS the function body, so this
            # is almost always redundant.
            if start is None and end is None:
                verdicts.append("redundant")
            else:
                verdicts.append("redundant_range")
        elif path in file_text:
            # Only a definition-snippet was prefetched for something in this file.
            verdicts.append("partial")
        else:
            verdicts.append("novel")
    # Roll up: worst case wins ordering novel > partial > redundant_range > redundant
    order = {"redundant": 0, "redundant_range": 1, "partial": 2, "novel": 3}
    if not verdicts:
        return "novel"
    return max(verdicts, key=lambda v: order[v])


def classify_search(args, sym_defs, file_text):
    pattern = args.get("pattern", "")
    # Strip simple regex metachars to recover the underlying symbol
    cleaned = re.sub(r"\\[bBswWdDsS]", "", pattern)
    cleaned = re.sub(r"[\\^$.*+?()\[\]{}|]", " ", cleaned)
    tokens = [t for t in cleaned.split() if len(t) >= 3]
    if not tokens:
        return "novel"
    # If any token is a known definition, call it redundant.
    for t in tokens:
        if t in sym_defs:
            return "redundant"
    # If any token appears as literal text in any prefetched block, it's "partial" —
    # the model could have grep'd the prefetched blob in its head.
    blob = "\n".join(file_text.values())
    if any(t in blob for t in tokens):
        return "partial"
    return "novel"


def classify_git_show(args, files_full, file_text):
    obj = args.get("object", "")
    # Typical: "HEAD:path", "HEAD~1:path"
    if ":" in obj:
        ref, path = obj.split(":", 1)
        if ref in ("HEAD", "HEAD^", "HEAD~0") and path in files_full:
            # HEAD is the post-patch state — same thing prefetch captured.
            return "redundant"
        if path in file_text:
            return "partial"
    return "novel"


def classify(row, files_full, sym_defs, file_text):
    tool = row["tool_name"]
    try:
        args = json.loads(row["arguments"])
    except Exception:
        return "novel"
    if tool == "read_files":
        return classify_read_files(args, files_full, file_text)
    if tool == "search_file_content":
        return classify_search(args, sym_defs, file_text)
    if tool == "git_show":
        return classify_git_show(args, files_full, file_text)
    # git_log, git_diff, find_files, TodoWrite: not directly comparable — report as novel/other.
    return "not_prefetchable"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--prefetch", required=True)
    ap.add_argument("--review-id", type=int, default=None)
    ap.add_argument("--show-samples", type=int, default=3,
                    help="How many sample calls to print per (tool, verdict)")
    args = ap.parse_args()

    with open(args.prefetch) as f:
        files_full, sym_defs, file_text = parse_prefetch(f.read())

    print(f"Prefetch summary:", file=sys.stderr)
    print(f"  files with full enclosing block: {len(files_full)}", file=sys.stderr)
    print(f"  symbol definitions extracted:    {len(sym_defs)}", file=sys.stderr)
    print(f"  total prefetched text size:      {sum(len(t) for t in file_text.values())} chars",
          file=sys.stderr)
    print("", file=sys.stderr)

    conn = sqlite3.connect(args.db)
    conn.row_factory = sqlite3.Row
    query = "SELECT id, review_id, tool_name, arguments, output_length FROM tool_usages"
    params = ()
    if args.review_id is not None:
        query += " WHERE review_id = ?"
        params = (args.review_id,)
    query += " ORDER BY id"
    rows = conn.execute(query, params).fetchall()

    counts = Counter()
    per_tool = defaultdict(Counter)
    samples = defaultdict(list)
    for r in rows:
        verdict = classify(r, files_full, sym_defs, file_text)
        counts[verdict] += 1
        per_tool[r["tool_name"]][verdict] += 1
        key = (r["tool_name"], verdict)
        if len(samples[key]) < args.show_samples:
            samples[key].append(r["arguments"])

    total = sum(counts.values())
    print("=" * 60)
    print(f"Total tool calls: {total}")
    print("=" * 60)
    print(f"{'verdict':<22} {'count':>6} {'pct':>6}")
    for v, n in counts.most_common():
        print(f"{v:<22} {n:>6} {100*n/total:>5.1f}%")
    print()
    print("Per tool breakdown:")
    for tool, cnts in sorted(per_tool.items(), key=lambda kv: -sum(kv[1].values())):
        total_tool = sum(cnts.values())
        parts = ", ".join(f"{v}={n}" for v, n in cnts.most_common())
        print(f"  {tool:<22} n={total_tool:<4}  {parts}")
    print()
    print("Sample calls (up to N per tool/verdict):")
    for (tool, verdict), exs in sorted(samples.items()):
        print(f"\n  [{tool} -> {verdict}]")
        for ex in exs:
            print(f"    {ex[:200]}")


if __name__ == "__main__":
    main()
