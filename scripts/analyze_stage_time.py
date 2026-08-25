#!/usr/bin/env python3
"""Attribute review wall-clock time to individual stages from conversation dumps.

Each dump directory holds <prefix>s<stage>_<seq>_{req,resp}.json files.  A
stage's wall time is taken as the span from its first request to its last
response, using file mtimes.  Comparing dump sets from two dates shows which
stages account for a change in total review duration.

Prefixed files (e.g. "sonnet-5-s4_001_req.json") belong to an additional-model
run; "main-" and unprefixed files belong to the primary model.  Additional-model
stages usually overlap the main run in time, so their spans are reported
separately and are not summed into the main total.

Usage:
    scripts/analyze_stage_time.py /path/to/dumps --dates 20260723 20260730
"""
import argparse
import re
import statistics
import sys
from collections import defaultdict
from pathlib import Path

FILE_RE = re.compile(r"^(?:(?P<prefix>[a-z0-9.\-]+)-)?s(?P<stage>p|\d+)_(?P<seq>\d+)_(?P<kind>req|resp)\.json$")


def parse_args():
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    p.add_argument("dump_root", type=Path, help="Directory holding per-review dump dirs")
    p.add_argument(
        "--dates",
        nargs="+",
        required=True,
        help="Dump-dir date prefixes to compare, e.g. 20260723 20260730",
    )
    p.add_argument(
        "--max-dirs", type=int, default=400, help="Max dump dirs per date (default: 400)"
    )
    return p.parse_args()


def stage_sort_key(stage):
    # "p" is the prompt-selection preamble stage and sorts before the numbered ones.
    return (0, -1) if stage == "p" else (0, int(stage))


def collect(dump_dir):
    """Returns {(is_additional, stage): (span_seconds, call_count)} for one review."""
    times = defaultdict(list)
    for entry in dump_dir.iterdir():
        m = FILE_RE.match(entry.name)
        if not m:
            continue
        prefix = m.group("prefix")
        # "main" is the primary model's explicit prefix; absence of a prefix
        # means the same thing in older dumps.
        is_additional = prefix is not None and prefix != "main"
        try:
            times[(is_additional, m.group("stage"))].append(entry.stat().st_mtime)
        except OSError:
            continue
    out = {}
    for key, stamps in times.items():
        if len(stamps) >= 2:
            out[key] = (max(stamps) - min(stamps), len(stamps))
        else:
            out[key] = (0.0, len(stamps))
    return out


def review_total(dump_dir):
    """Total wall time for a dump dir, from earliest to latest dump file."""
    stamps = []
    for entry in dump_dir.iterdir():
        if FILE_RE.match(entry.name):
            try:
                stamps.append(entry.stat().st_mtime)
            except OSError:
                pass
    return (max(stamps) - min(stamps)) if len(stamps) >= 2 else 0.0


def main():
    args = parse_args()
    if not args.dump_root.is_dir():
        sys.exit(f"error: not a directory: {args.dump_root}")

    per_date = {}
    for date in args.dates:
        dirs = sorted(d for d in args.dump_root.glob(f"{date}-*") if d.is_dir())[: args.max_dirs]
        if not dirs:
            print(f"warning: no dump dirs for {date}", file=sys.stderr)
            continue
        stage_spans = defaultdict(list)
        totals = []
        for d in dirs:
            try:
                spans = collect(d)
            except OSError:
                continue
            for key, (span, _count) in spans.items():
                stage_spans[key].append(span / 60.0)
            t = review_total(d)
            if t:
                totals.append(t / 60.0)
        per_date[date] = {"stages": stage_spans, "totals": totals, "ndirs": len(dirs)}

    for date, data in per_date.items():
        print("=" * 78)
        print(f"{date}   ({data['ndirs']} dump dirs, {len(data['totals'])} with timing)")
        print("=" * 78)
        if data["totals"]:
            print(
                f"  review wall time: median {statistics.median(data['totals']):.1f} min, "
                f"mean {statistics.mean(data['totals']):.1f} min\n"
            )
        main_keys = sorted(
            (k for k in data["stages"] if not k[0]), key=lambda k: stage_sort_key(k[1])
        )
        add_keys = sorted(
            (k for k in data["stages"] if k[0]), key=lambda k: stage_sort_key(k[1])
        )

        print("  primary model stages:")
        main_median_sum = 0.0
        for key in main_keys:
            vals = data["stages"][key]
            med = statistics.median(vals)
            main_median_sum += med
            print(f"    s{key[1]:<3} n={len(vals):<4} median={med:6.2f} min  mean={statistics.mean(vals):6.2f}")
        print(f"    {'sum of medians':<14} {main_median_sum:6.2f} min")

        if add_keys:
            print("\n  additional-model stages (overlap the primary run):")
            for key in add_keys:
                vals = data["stages"][key]
                print(
                    f"    s{key[1]:<3} n={len(vals):<4} median={statistics.median(vals):6.2f} min  "
                    f"mean={statistics.mean(vals):6.2f}"
                )
        print()

    if len(per_date) == 2:
        (d1, a), (d2, b) = list(per_date.items())
        print("=" * 78)
        print(f"STAGE-LEVEL DELTA  {d1} -> {d2}  (primary model, median minutes)")
        print("=" * 78)
        stages = sorted(
            {k[1] for k in a["stages"] if not k[0]} | {k[1] for k in b["stages"] if not k[0]},
            key=stage_sort_key,
        )
        for st in stages:
            va = a["stages"].get((False, st))
            vb = b["stages"].get((False, st))
            ma = statistics.median(va) if va else None
            mb = statistics.median(vb) if vb else None
            if ma is None and mb is not None:
                print(f"  s{st:<3} NEW           -> {mb:6.2f} min   (+{mb:.2f})")
            elif ma is not None and mb is None:
                print(f"  s{st:<3} {ma:6.2f} min    -> REMOVED")
            elif ma is not None and mb is not None:
                print(f"  s{st:<3} {ma:6.2f} min    -> {mb:6.2f} min   ({mb - ma:+.2f})")
        if a["totals"] and b["totals"]:
            print(
                f"\n  total review median: {statistics.median(a['totals']):.1f} -> "
                f"{statistics.median(b['totals']):.1f} min"
            )


if __name__ == "__main__":
    main()
