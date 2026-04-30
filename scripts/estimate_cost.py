#!/usr/bin/env python3
"""Estimate AWS Bedrock costs from a sashiko conversation dump.

Usage:
    scripts/estimate_cost.py <dump_dir> [--input-rate N] [--cache-read-rate N]
                                        [--cache-write-rate N] [--output-rate N]

Rates are $/M tokens.  Defaults: Bedrock Claude Opus 4.5 pricing
  input=$5, cache-read=$0.50, cache-write=$6.25, output=$25

The cache-write rate ($6.25) INCLUDES the base input cost ($5 + $1.25 surcharge).
Cache-write tokens are therefore not also billed as uncached input.

When cache_write_tokens is present in the dump (Bedrock/Claude), uses the actual
value.  For older dumps without this field, falls back to estimating writes from
growth in cached_tokens across turns.
"""
import argparse
import json
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
    p.add_argument("--context-split", action="store_true", help="Show how much input is initial context vs tool-use interaction")
    p.add_argument("--output-split", action="store_true", help="Show output tokens split by mid-conversation (tool calls) vs final response")
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
            resp = json.loads(f.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        usage = resp.get("usage", {})
        has_tool_calls = bool(resp.get("tool_calls"))
        has_content = resp.get("content") is not None and len(str(resp.get("content", ""))) > 0
        sessions[sid].append((turn, usage, has_tool_calls, has_content))

    for lst in sessions.values():
        lst.sort(key=lambda x: x[0])
    return sessions


def detect_prompt_includes_cached(turns):
    """Detect whether prompt_tokens already includes cached_tokens.

    Before commit 6a39913, sashiko logged prompt_tokens as uncached-only input
    and cached_tokens separately (so cached could exceed prompt).  After that
    fix, prompt_tokens = uncached + cache_read + cache_write, always >= cached.
    """
    for t in turns:
        u = t[1]
        p = u.get("prompt_tokens", 0)
        c = u.get("cached_tokens", 0)
        if c > p and p > 0:
            return False
    return True


def analyze_session(turns, prompt_includes_cached: bool):
    total_input = 0
    cache_read = 0
    cache_write = 0
    uncached = 0
    completion = 0
    prev_cached = 0
    first_cached = None
    has_explicit_write = any(u.get("cache_write_tokens") is not None for _, u, _, _ in turns)

    # Track per-turn prompt_tokens for context vs interactive breakdown
    per_turn_prompt = []

    # Output split: mid-conversation (tool-call turns) vs final (text-only response)
    mid_output = 0
    final_output_tokens = 0
    max_mid_output = 0

    for i, (_, u, has_tools, has_content) in enumerate(turns):
        p = u.get("prompt_tokens", 0)
        c = u.get("cached_tokens", 0)
        o = u.get("completion_tokens", 0)

        if prompt_includes_cached:
            turn_input = p
        else:
            turn_input = p + c

        # Per-turn decomposition:
        #   turn_input = cache_read + cache_write + uncached
        turn_cache_read = c
        if has_explicit_write:
            turn_cache_write = u.get("cache_write_tokens") or 0
        else:
            # Fallback for old dumps: estimate from cache growth
            turn_cache_write = max(0, c - prev_cached)
        turn_uncached = max(0, turn_input - turn_cache_read - turn_cache_write)

        total_input += turn_input
        cache_read += turn_cache_read
        cache_write += turn_cache_write
        uncached += turn_uncached
        completion += o
        per_turn_prompt.append(turn_input)
        if first_cached is None:
            first_cached = c
        prev_cached = c

        is_final = (i == len(turns) - 1) or (has_content and not has_tools)
        if is_final:
            final_output_tokens += o
        else:
            mid_output += o
            max_mid_output = max(max_mid_output, o)

    # Context vs interactive breakdown
    # Turn 1's prompt_tokens = initial context (system + tools + user message)
    initial_context = per_turn_prompt[0] if per_turn_prompt else 0

    # Overall: each turn repeats the initial context, so across N turns
    # the initial context accounts for N * initial_context tokens of the total
    # input, and the remainder is interactive (tool results, assistant messages)
    overall_context = initial_context * len(per_turn_prompt)
    overall_interactive = max(0, total_input - overall_context)

    # Final exchange: just the last turn
    final_prompt = per_turn_prompt[-1] if per_turn_prompt else 0
    final_interactive = max(0, final_prompt - initial_context)
    final_output = turns[-1][1].get("completion_tokens", 0) if turns else 0

    return {
        "turns": len(turns),
        "total_input": total_input,
        "cache_read": cache_read,
        "cache_write": cache_write,
        "uncached": uncached,
        "completion": completion,
        "first_cached": first_cached or 0,
        "max_cached": max((u.get("cached_tokens", 0) for _, u, _, _ in turns), default=0),
        "initial_context": initial_context,
        "overall_context": overall_context,
        "overall_interactive": overall_interactive,
        "final_prompt": final_prompt,
        "final_context": initial_context,
        "final_interactive": final_interactive,
        "final_output": final_output,
        "mid_output": mid_output,
        "final_output_tokens": final_output_tokens,
        "max_mid_output": max_mid_output,
    }


def cost(tokens, rate):
    return tokens * rate / 1_000_000


def session_cost(r, input_rate, cache_read_rate, cache_write_rate, output_rate):
    return (cost(r["uncached"], input_rate) + cost(r["cache_read"], cache_read_rate)
            + cost(r["cache_write"], cache_write_rate) + cost(r["completion"], output_rate))


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

    # Detect accounting scheme from all turns
    all_turns = [t for lst in sessions.values() for t in lst]
    prompt_includes_cached = detect_prompt_includes_cached(all_turns)

    results = {}
    for sid in sorted(sessions):
        results[sid] = analyze_session(sessions[sid], prompt_includes_cached)

    grand = {k: sum(r[k] for r in results.values())
             for k in ["total_input", "cache_read", "cache_write", "uncached", "completion"]}

    input_cost = cost(grand["uncached"], args.input_rate)
    read_cost = cost(grand["cache_read"], args.cache_read_rate)
    write_cost = cost(grand["cache_write"], args.cache_write_rate)
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
            out["cost"]["no_cache_total"] = round(
                cost(grand["total_input"], args.input_rate) + cost(grand["completion"], args.output_rate), 2
            )
        if args.context_split:
            overall_ctx = sum(r["overall_context"] for r in results.values())
            overall_int = sum(r["overall_interactive"] for r in results.values())
            out["context_breakdown"] = {
                "overall_context_tokens": overall_ctx,
                "overall_interactive_tokens": overall_int,
                "overall_context_pct": round(overall_ctx * 100 / max(1, overall_ctx + overall_int), 1),
                "per_stage": {
                    sid: {
                        "total_input": r["overall_context"] + r["overall_interactive"],
                        "context_tokens": r["overall_context"],
                        "interactive_tokens": r["overall_interactive"],
                        "turns": r["turns"],
                    }
                    for sid, r in results.items()
                },
                "final_exchange": {
                    sid: {
                        "prompt_tokens": r["final_prompt"],
                        "context_tokens": r["final_context"],
                        "interactive_tokens": r["final_interactive"],
                        "output_tokens": r["final_output"],
                    }
                    for sid, r in results.items()
                },
            }
        json.dump(out, sys.stdout, indent=2)
        print()
        return

    rates = (args.input_rate, args.cache_read_rate, args.cache_write_rate, args.output_rate)

    # Table header
    print(f"{'session':<8} {'turns':>5} {'total in':>10} {'uncached':>10} {'cache rd':>10} {'cache wr':>10} {'output':>10} {'cost':>8}")
    print("-" * 83)

    for sid in sorted(results):
        r = results[sid]
        sc = session_cost(r, *rates)
        print(
            f"{sid:<8} {r['turns']:>5} "
            f"{fmt_tokens(r['total_input']):>10} "
            f"{fmt_tokens(r['uncached']):>10} "
            f"{fmt_tokens(r['cache_read']):>10} "
            f"{fmt_tokens(r['cache_write']):>10} "
            f"{fmt_tokens(r['completion']):>10}"
            f" ${sc:>7.2f}"
        )

    print("-" * 83)
    print(
        f"{'TOTAL':<8} {sum(r['turns'] for r in results.values()):>5} "
        f"{fmt_tokens(grand['total_input']):>10} "
        f"{fmt_tokens(grand['uncached']):>10} "
        f"{fmt_tokens(grand['cache_read']):>10} "
        f"{fmt_tokens(grand['cache_write']):>10} "
        f"{fmt_tokens(grand['completion']):>10}"
        f" ${total:>7.2f}"
    )

    print()
    print(f"  Uncached input  ({fmt_tokens(grand['uncached']):>6}) @ ${args.input_rate}/M:      ${input_cost:>8.2f}")
    print(f"  Cache read      ({fmt_tokens(grand['cache_read']):>6}) @ ${args.cache_read_rate}/M:   ${read_cost:>8.2f}")
    print(f"  Cache write     ({fmt_tokens(grand['cache_write']):>6}) @ ${args.cache_write_rate}/M:  ${write_cost:>8.2f}")
    print(f"  Output          ({fmt_tokens(grand['completion']):>6}) @ ${args.output_rate}/M:     ${output_cost:>8.2f}")
    print(f"  {'TOTAL':44s} ${total:>8.2f}")

    if args.no_cache:
        no_cache_total = cost(grand["total_input"], args.input_rate) + cost(grand["completion"], args.output_rate)
        print()
        print(f"  Without caching:         ${no_cache_total:>8.2f}")
        print(f"  Savings:                 ${no_cache_total - total:>8.2f} ({(no_cache_total - total) / no_cache_total * 100:.0f}%)")

    if args.context_split:
        # Context vs interactive breakdown
        overall_ctx = sum(r["overall_context"] for r in results.values())
        overall_int = sum(r["overall_interactive"] for r in results.values())
        total_turns = sum(r["turns"] for r in results.values())
        total_in = overall_ctx + overall_int
        if total_in > 0:
            print()
            print("  Context vs. Interactive Breakdown")
            print("  " + "-" * 60)
            print(f"  (1) Overall (summed across all {total_turns} exchanges):")
            ctx_pct = overall_ctx * 100 / total_in
            int_pct = overall_int * 100 / total_in
            print(f"      Initial context (system+tools+user): {fmt_tokens(overall_ctx):>8}  ({ctx_pct:.0f}%)")
            print(f"      Interactive (tool results+assistant): {fmt_tokens(overall_int):>8}  ({int_pct:.0f}%)")

        # Per-stage aggregate
        print()
        print(f"  (2) Aggregate per stage:")
        for sid in sorted(results):
            r = results[sid]
            ctx = r["overall_context"]
            inter = r["overall_interactive"]
            stage_in = ctx + inter
            if stage_in > 0:
                ctx_pct = ctx * 100 / stage_in
                int_pct = inter * 100 / stage_in
                print(f"      {sid}: {fmt_tokens(stage_in)} in  =  {fmt_tokens(ctx)} context ({ctx_pct:.0f}%) + {fmt_tokens(inter)} interactive ({int_pct:.0f}%),  {r['turns']} turns")

        # Final exchange per session
        print()
        print(f"  (3) Final exchange (last turn per session):")
        for sid in sorted(results):
            r = results[sid]
            fp = r["final_prompt"]
            fc = r["final_context"]
            fi = r["final_interactive"]
            fo = r["final_output"]
            if fp > 0:
                fc_pct = fc * 100 / fp
                fi_pct = fi * 100 / fp
                print(f"      {sid}: {fmt_tokens(fp)} in  =  {fmt_tokens(fc)} context ({fc_pct:.0f}%) + {fmt_tokens(fi)} interactive ({fi_pct:.0f}%),  {fmt_tokens(fo)} out")

    if args.output_split:
        total_mid = sum(r["mid_output"] for r in results.values())
        total_final = sum(r["final_output_tokens"] for r in results.values())
        total_out = total_mid + total_final
        print()
        print("  Output Token Breakdown (mid-conversation vs final response)")
        print("  " + "-" * 60)
        print(f"  {'stage':<8} {'turns':>5} {'mid':>8} {'max mid':>8} {'final':>8} {'total':>8}  {'final%':>6}")
        print(f"  {'-----':<8} {'-----':>5} {'-----':>8} {'-------':>8} {'-----':>8} {'-----':>8}  {'------':>6}")
        for sid in sorted(results):
            r = results[sid]
            mid = r["mid_output"]
            final = r["final_output_tokens"]
            stage_out = mid + final
            pct = f"{final * 100 / stage_out:.0f}%" if stage_out > 0 else "-"
            print(f"  {sid:<8} {r['turns']:>5} {fmt_tokens(mid):>8} {fmt_tokens(r['max_mid_output']):>8} {fmt_tokens(final):>8} {fmt_tokens(stage_out):>8}  {pct:>6}")
        if total_out > 0:
            print(f"  {'TOTAL':<8} {sum(r['turns'] for r in results.values()):>5} {fmt_tokens(total_mid):>8} {'':>8} {fmt_tokens(total_final):>8} {fmt_tokens(total_out):>8}  {total_final * 100 / total_out:.0f}%")


if __name__ == "__main__":
    main()
