#!/usr/bin/env python3
"""Estimate AWS Bedrock costs from a sashiko conversation dump.

Usage:
    scripts/estimate_cost.py <dump_dir> [--input-rate N] [--cache-read-rate N]
                                        [--cache-write-rate N] [--output-rate N]

Rates are $/M tokens.  Defaults: Bedrock Claude Opus 4.5 pricing
  input=$5, cache-read=$0.50, cache-write=$6.25, output=$25

Cache-write tokens are estimated from growth in cached_tokens across turns
(the Bedrock usage API doesn't report writes directly).
"""
import argparse
import json
import os
import re
import sys
from collections import defaultdict
from pathlib import Path


def parse_args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("dump_dir", type=Path, help="Path to conversation dump directory")
    p.add_argument("--input-rate", type=float, default=5.0, help="$/M for non-cached input tokens (default: 5.0)")
    p.add_argument("--cache-read-rate", type=float, default=0.50, help="$/M for cache-read tokens (default: 0.50)")
    p.add_argument("--cache-write-rate", type=float, default=6.25, help="$/M for cache-write tokens (default: 6.25)")
    p.add_argument("--output-rate", type=float, default=25.0, help="$/M for output tokens (default: 25.0)")
    p.add_argument("--no-cache", action="store_true", help="Show hypothetical cost with no caching")
    p.add_argument("--json", action="store_true", help="Output as JSON")
    return p.parse_args()


RESP_RE = re.compile(r"^(s\w+)_(\d+)_resp\.json$")


def load_sessions(dump_dir: Path):
    sessions = defaultdict(list)
    for f in sorted(dump_dir.iterdir()):
        m = RESP_RE.match(f.name)
        if not m:
            continue
        sid, turn = m.group(1), int(m.group(2))
        try:
            usage = json.loads(f.read_text()).get("usage", {})
        except (json.JSONDecodeError, OSError):
            continue
        sessions[sid].append((turn, usage))

    for lst in sessions.values():
        lst.sort(key=lambda x: x[0])
    return sessions


def analyze_session(turns):
    prompt = 0
    cached = 0
    completion = 0
    cache_writes = 0
    prev_cached = 0
    first_cached = None

    for _, u in turns:
        p = u.get("prompt_tokens", 0)
        c = u.get("cached_tokens", 0)
        o = u.get("completion_tokens", 0)
        prompt += p
        cached += c
        completion += o
        if first_cached is None:
            first_cached = c
        if c > prev_cached:
            cache_writes += c - prev_cached
        prev_cached = c

    return {
        "turns": len(turns),
        "prompt": prompt,
        "cached": cached,
        "completion": completion,
        "cache_writes": cache_writes,
        "first_cached": first_cached or 0,
        "max_cached": max((u.get("cached_tokens", 0) for _, u in turns), default=0),
    }


def cost(tokens, rate):
    return tokens * rate / 1_000_000


def fmt_tokens(n):
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}k"
    return str(n)


def main():
    args = parse_args()

    if not args.dump_dir.is_dir():
        print(f"Error: {args.dump_dir} is not a directory", file=sys.stderr)
        sys.exit(1)

    sessions = load_sessions(args.dump_dir)
    if not sessions:
        print(f"Error: no *_resp.json files found in {args.dump_dir}", file=sys.stderr)
        sys.exit(1)

    results = {}
    for sid in sorted(sessions):
        results[sid] = analyze_session(sessions[sid])

    grand = {k: sum(r[k] for r in results.values()) for k in ["prompt", "cached", "completion", "cache_writes"]}

    input_cost = cost(grand["prompt"], args.input_rate)
    read_cost = cost(grand["cached"], args.cache_read_rate)
    write_cost = cost(grand["cache_writes"], args.cache_write_rate)
    output_cost = cost(grand["completion"], args.output_rate)
    total = input_cost + read_cost + write_cost + output_cost

    if args.json:
        out = {
            "dump_dir": str(args.dump_dir),
            "sessions": results,
            "totals": grand,
            "cost": {
                "input": round(input_cost, 2),
                "cache_read": round(read_cost, 2),
                "cache_write": round(write_cost, 2),
                "output": round(output_cost, 2),
                "total": round(total, 2),
            },
        }
        if args.no_cache:
            no_cache_input = grand["prompt"] + grand["cached"]
            out["cost"]["no_cache_total"] = round(
                cost(no_cache_input, args.input_rate) + cost(grand["completion"], args.output_rate), 2
            )
        json.dump(out, sys.stdout, indent=2)
        print()
        return

    # Table header
    print(f"{'session':<8} {'turns':>5} {'input':>10} {'cached':>10} {'writes':>10} {'output':>10} {'1st cached':>10}")
    print("-" * 75)

    for sid in sorted(results):
        r = results[sid]
        print(
            f"{sid:<8} {r['turns']:>5} "
            f"{fmt_tokens(r['prompt']):>10} "
            f"{fmt_tokens(r['cached']):>10} "
            f"{fmt_tokens(r['cache_writes']):>10} "
            f"{fmt_tokens(r['completion']):>10} "
            f"{fmt_tokens(r['first_cached']):>10}"
        )

    print("-" * 75)
    print(
        f"{'TOTAL':<8} {sum(r['turns'] for r in results.values()):>5} "
        f"{fmt_tokens(grand['prompt']):>10} "
        f"{fmt_tokens(grand['cached']):>10} "
        f"{fmt_tokens(grand['cache_writes']):>10} "
        f"{fmt_tokens(grand['completion']):>10}"
    )

    print()
    print(f"  Input ({args.input_rate}/M):       ${input_cost:>8.2f}")
    print(f"  Cache read ({args.cache_read_rate}/M):   ${read_cost:>8.2f}")
    print(f"  Cache write ({args.cache_write_rate}/M):  ${write_cost:>8.2f}")
    print(f"  Output ({args.output_rate}/M):      ${output_cost:>8.2f}")
    print(f"  {'TOTAL':24s} ${total:>8.2f}")

    if args.no_cache:
        no_cache_input = grand["prompt"] + grand["cached"]
        no_cache_total = cost(no_cache_input, args.input_rate) + cost(grand["completion"], args.output_rate)
        print()
        print(f"  Without caching:         ${no_cache_total:>8.2f}")
        print(f"  Savings:                 ${no_cache_total - total:>8.2f} ({(no_cache_total - total) / no_cache_total * 100:.0f}%)")


if __name__ == "__main__":
    main()
