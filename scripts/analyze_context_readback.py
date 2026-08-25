#!/usr/bin/env python3
"""Measure whether stages read back source the prepared context stopped shipping.

Omitting a function from the prefetch payload only saves money if the model
accepts the diff as sufficient.  If it instead re-reads the same file with a
tool, the review pays twice: once in output tokens for the call, and again in
input tokens for a result that is no longer cacheable behind the prompt prefix.

So this counts, per review, how much of the tool traffic lands on the files the
patch itself modified -- the population the change made eligible for omission --
and reports it alongside traffic to every other file, which the change did not
touch and which therefore acts as a within-review control for "the model just
used more tools this week".

Reads modified paths from the `Target Commit:` diff in the stage system prompt
and tool targets from the recorded tool-call arguments, so no repository access
is needed.

Usage:
    scripts/analyze_context_readback.py --split 3433d11 --min-day 20260726
"""
import argparse
import json
import os
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from analyze_context_efficiency import (  # noqa: E402
    DISCOVERY, FILE_RE, load_prompt_eras, read_json, split_prepared_context,
)

DUMP_DEFAULT = "/srv/nipa-sashiko/sashiko-dump"
DIFF_FILE_RE = re.compile(r"^diff --git a/(\S+) b/(\S+)$", re.M)
PREFETCH_ENTRY_RE = re.compile(r"^--- (\S+?):(\d+)(?: \((.*?)\))? ---$", re.M)
# Tools whose arguments name a path we can attribute.
PATH_KEYS = ("path", "file", "filename")


def parse_args():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--dumps", type=Path, default=Path(DUMP_DEFAULT))
    p.add_argument("--db", type=Path, default=Path("sashiko.db"))
    p.add_argument("--split", required=True)
    p.add_argument("--min-day", default="20260726")
    p.add_argument("--source", default="main")
    p.add_argument("--json", action="store_true")
    return p.parse_args()


def tool_paths(args_obj):
    """All repository paths named by one tool call's arguments."""
    out = []
    if not isinstance(args_obj, dict):
        return out
    for key in PATH_KEYS:
        v = args_obj.get(key)
        if isinstance(v, str) and v:
            out.append(v)
    # git_read_files takes a list of {path, start_line, end_line} entries.
    files = args_obj.get("files")
    if isinstance(files, list):
        for f in files:
            if isinstance(f, dict) and isinstance(f.get("path"), str):
                out.append(f["path"])
            elif isinstance(f, str):
                out.append(f)
    return out


def scan(d: Path, source: str):
    try:
        names = os.listdir(d)
    except OSError:
        return None

    stages = defaultdict(dict)
    for name in names:
        m = FILE_RE.match(name)
        if not m or (m.group("source") or "main") != source:
            continue
        if m.group("stage") not in DISCOVERY:
            continue
        stages[m.group("stage")].setdefault(int(m.group("turn")), {})[m.group("kind")] = d / name
    if not stages:
        return None

    modified, prefetch_files, diff_chars, prefetch_chars = set(), set(), 0, 0
    for stage, turns in sorted(stages.items()):
        if 1 not in turns or "req" not in turns[1]:
            continue
        req = read_json(turns[1]["req"])
        system = (req or {}).get("system")
        if not isinstance(system, str):
            continue
        diff_chars, prefetch_chars = split_prepared_context(system)
        for m in DIFF_FILE_RE.finditer(system):
            modified.add(m.group(2))
        pf = re.search(r"<pre_fetched_context>(.*?)</pre_fetched_context>", system, re.S)
        if pf:
            for m in PREFETCH_ENTRY_RE.finditer(pf.group(1)):
                prefetch_files.add(m.group(1))
        break
    if not modified:
        return None

    hits_modified = hits_other = calls_total = 0
    bytes_modified = bytes_other = 0
    for stage, turns in stages.items():
        # Pair each response's tool calls with the results in the next request,
        # so bytes can be attributed to the path that produced them.
        for t in sorted(turns):
            resp = read_json(turns[t].get("resp")) if "resp" in turns[t] else None
            if not resp:
                continue
            calls = resp.get("tool_calls") or []
            nxt = turns.get(t + 1, {}).get("req")
            results = []
            if nxt:
                nreq = read_json(nxt)
                results = [m for m in (nreq or {}).get("messages", [])
                           if m.get("role") == "tool"]
            # The tool messages for this turn are the trailing len(calls) ones.
            tail = results[-len(calls):] if calls and len(results) >= len(calls) else []
            for i, tc in enumerate(calls):
                calls_total += 1
                paths = tool_paths(tc.get("arguments"))
                size = len(str(tail[i].get("content") or "")) if i < len(tail) else 0
                if not paths:
                    continue
                if any(p in modified or any(p.startswith(m.rsplit("/", 1)[0] + "/")
                                            for m in modified) for p in paths):
                    hits_modified += 1
                    bytes_modified += size
                else:
                    hits_other += 1
                    bytes_other += size
    return {
        "dir": d.name,
        "modified_files": len(modified),
        "prefetch_files": len(prefetch_files),
        "diff_chars": diff_chars,
        "prefetch_chars": prefetch_chars,
        "calls_total": calls_total,
        "calls_modified": hits_modified,
        "calls_other": hits_other,
        "bytes_modified": bytes_modified,
        "bytes_other": bytes_other,
    }


def med(rows, key):
    vals = [r[key] for r in rows]
    return statistics.median(vals) if vals else 0.0


def main():
    args = parse_args()
    eras = load_prompt_eras(args.db)

    rows = []
    for d in sorted(args.dumps.iterdir()):
        if not d.is_dir() or d.name[:8] < args.min_day:
            continue
        r = scan(d, args.source)
        if not r:
            continue
        rows.append(r)

    # Era boundary: first dump directory whose review ran at the split hash.
    split_dirs = []
    for d in sorted(args.dumps.iterdir()):
        if not d.is_dir() or d.name[:8] < args.min_day:
            continue
        for name in sorted(os.listdir(d)):
            m = FILE_RE.match(name)
            if not m or m.group("kind") != "req":
                continue
            req = read_json(d / name)
            tag = re.search(r"\[ps:(\d+) p:(\S+)", (req or {}).get("context_tag") or "")
            if tag:
                h = eras.get((tag.group(1), tag.group(2)))
                if h and h.startswith(args.split):
                    split_dirs.append(d.name)
            break
    if not split_dirs:
        sys.exit(f"no reviews found at prompts_hash {args.split}")
    boundary = min(split_dirs)
    for r in rows:
        r["era"] = "after" if r["dir"] >= boundary else "before"

    out = {}
    for era in ("before", "after"):
        sub = [r for r in rows if r["era"] == era]
        if not sub:
            continue
        out[era] = {
            "reviews": len(sub),
            "calls_modified": med(sub, "calls_modified"),
            "calls_other": med(sub, "calls_other"),
            "bytes_modified_kb": med(sub, "bytes_modified") / 1000,
            "bytes_other_kb": med(sub, "bytes_other") / 1000,
            "prefetch_files": med(sub, "prefetch_files"),
            "modified_files": med(sub, "modified_files"),
        }

    if args.json:
        print(json.dumps(out, indent=2))
        return

    b, a = out.get("before"), out.get("after")
    print(f"[{args.source}] discovery-stage tool traffic, medians per review")
    print(f"  before={b['reviews'] if b else 0} reviews, after={a['reviews'] if a else 0}\n")
    print(f"{'metric':<34} {'before':>10} {'after':>10} {'change':>9}")
    for label, key in (("tool calls on modified files", "calls_modified"),
                       ("tool calls elsewhere (control)", "calls_other"),
                       ("result KB from modified files", "bytes_modified_kb"),
                       ("result KB elsewhere (control)", "bytes_other_kb"),
                       ("prefetch entries' files", "prefetch_files"),
                       ("files the patch modifies", "modified_files")):
        if not (b and a):
            continue
        bv, av = b[key], a[key]
        d = 100.0 * (av - bv) / bv if bv else float("nan")
        print(f"  {label:<32} {bv:>10.1f} {av:>10.1f} {d:>+8.1f}%")


if __name__ == "__main__":
    main()
