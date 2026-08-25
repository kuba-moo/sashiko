#!/usr/bin/env python3
"""Analyze review wall-clock latency from the sashiko database.

Two independent reports:

  --compare-windows   Single-model (no additional-model experiment) review
                      duration in two time windows, to isolate the effect of
                      pipeline changes from the multi-model sampling overhead.

  --by-model          Multi-model review duration broken down by which
                      additional model was sampled (fable-5, opus-4-7,
                      sonnet-5), to see whether the choice of extra model
                      changes wall-clock time.

  --throughput        Output tokens per wall-clock minute per day.  Separates
                      "the pipeline is doing more work" from "generation got
                      slower", which raw duration alone cannot distinguish.
                      Additional-model tokens live in model_experiment_runs
                      rather than ai_interactions, so they are added in
                      explicitly; omitting them understates multi-model
                      throughput badly.

Durations come from reviews.created_at/completed_at (per-patch reviews).

A review counts as multi-model only when model_experiment_sources holds a
selected=1 row whose experiment_name is NOT 'main'.  The 'main' experiment is
the primary model and is recorded as selected=1 on every review once the
experiment framework is active, so keying off selected=1 alone would classify
every recent review as multi-model.

Usage:
    scripts/analyze_review_latency.py --db sashiko.db --compare-windows
    scripts/analyze_review_latency.py --db sashiko.db --by-model
"""
import argparse
import sqlite3
import statistics
import sys
from pathlib import Path

# A review whose duration exceeds this is almost certainly an artifact of a
# restart or a stuck lease rather than real compute, so it is reported
# separately instead of skewing the means.
OUTLIER_MINUTES = 240.0


def parse_args():
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    p.add_argument("--db", type=Path, default=Path("sashiko.db"), help="Path to sashiko.db")
    p.add_argument(
        "--compare-windows",
        action="store_true",
        help="Compare single-model duration: last 24h vs the same 24h one week earlier",
    )
    p.add_argument("--by-model", action="store_true", help="Break multi-model duration down by sampled model")
    p.add_argument(
        "--throughput",
        action="store_true",
        help="Daily output-tokens-per-minute, to separate more work from slower generation",
    )
    p.add_argument(
        "--throughput-days", type=float, default=5.0, help="Lookback for --throughput (default: 5)"
    )
    p.add_argument(
        "--window-hours", type=float, default=24.0, help="Window length in hours (default: 24)"
    )
    p.add_argument(
        "--lookback-days",
        type=float,
        default=7.0,
        help="How far back the comparison window sits (default: 7 days)",
    )
    p.add_argument(
        "--by-model-days",
        type=float,
        default=3.0,
        help="Lookback for the --by-model report (default: 3 days)",
    )
    return p.parse_args()


def connect(db_path):
    if not db_path.exists():
        sys.exit(f"error: database not found: {db_path}")
    # Read-only so this never interferes with the running service.
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def fetch_reviews(conn, start_ts, end_ts):
    """Returns completed reviews in [start_ts, end_ts) with their sampled model.

    extra_model is the additional model sampled for that review, or None when
    the review ran single-model.  'main' is excluded because it denotes the
    primary model rather than an additional one.  When more than one additional
    model was sampled the names are joined, so such reviews form their own
    bucket instead of being misattributed to a single model.

    experiment_active reports whether the experiment framework recorded
    anything for the review at all, which distinguishes a genuine single-model
    run from a review predating the framework.
    """
    return conn.execute(
        """
        SELECT r.id,
               r.patchset_id,
               r.model                                   AS main_model,
               (r.completed_at - r.created_at) / 60.0    AS minutes,
               (SELECT group_concat(s.experiment_name, '+')
                  FROM (SELECT s2.experiment_name
                          FROM model_experiment_sources s2
                         WHERE s2.review_id = r.id
                           AND s2.selected = 1
                           AND s2.experiment_name <> 'main'
                         ORDER BY s2.experiment_name) s)  AS extra_model,
               EXISTS (SELECT 1
                         FROM model_experiment_sources s3
                        WHERE s3.review_id = r.id)        AS experiment_active,
               (SELECT length(p.diff)
                  FROM patches p
                 WHERE p.id = r.patch_id)                 AS diff_len
          FROM reviews r
         WHERE r.completed_at IS NOT NULL
           AND r.created_at IS NOT NULL
           AND r.completed_at >= ?
           AND r.completed_at <  ?
           AND r.completed_at > r.created_at
        """,
        (start_ts, end_ts),
    ).fetchall()


def summarize(minutes):
    """Mean/median/p90 over a list of durations, with outliers split out."""
    kept = [m for m in minutes if m <= OUTLIER_MINUTES]
    dropped = len(minutes) - len(kept)
    if not kept:
        return None
    ordered = sorted(kept)
    return {
        "n": len(kept),
        "dropped": dropped,
        "mean": statistics.mean(kept),
        "median": statistics.median(kept),
        "p90": ordered[min(len(ordered) - 1, int(round(0.9 * (len(ordered) - 1))))],
        "min": ordered[0],
        "max": ordered[-1],
    }


def fmt_row(label, s, width=26):
    if s is None:
        return f"  {label:<{width}} (no data)"
    extra = f"   [{s['dropped']} outlier(s) >{OUTLIER_MINUTES:.0f}m excluded]" if s["dropped"] else ""
    return (
        f"  {label:<{width}} n={s['n']:<4} mean={s['mean']:6.1f}  "
        f"median={s['median']:6.1f}  p90={s['p90']:6.1f}  "
        f"range={s['min']:.1f}-{s['max']:.1f}{extra}"
    )


def now_ts(conn):
    return conn.execute("SELECT strftime('%s','now')").fetchone()[0]


def iso(conn, ts):
    return conn.execute("SELECT datetime(?, 'unixepoch')", (ts,)).fetchone()[0]


def report_compare_windows(conn, args):
    now = int(now_ts(conn))
    window = int(args.window_hours * 3600)
    lookback = int(args.lookback_days * 86400)

    recent = (now - window, now)
    prior = (now - lookback - window, now - lookback)

    print("=" * 78)
    print("SINGLE-MODEL REVIEW DURATION (minutes) — pipeline change isolated")
    print("=" * 78)
    print(
        "Only reviews with NO additional model sampled, so multi-model overhead\n"
        "cannot confound the comparison.\n"
    )

    results = {}
    for name, (start, end) in (("recent", recent), ("prior", prior)):
        rows = fetch_reviews(conn, start, end)
        single = [r["minutes"] for r in rows if r["extra_model"] is None]
        multi = [r["minutes"] for r in rows if r["extra_model"] is not None]
        results[name] = {
            "single": summarize(single),
            "multi": summarize(multi),
            "span": (start, end),
            "total": len(rows),
            "framework_on": sum(1 for r in rows if r["experiment_active"]),
        }

    for name, title in (("prior", "One week ago"), ("recent", "Last 24h")):
        r = results[name]
        start, end = r["span"]
        print(f"{title}  [{iso(conn, start)} .. {iso(conn, end)}]")
        print(fmt_row("single-model", r["single"]))
        print(fmt_row("multi-model (context)", r["multi"]))
        print(
            f"  {'experiment framework':<26} recorded on {r['framework_on']}/{r['total']} reviews"
        )
        print()

    a, b = results["prior"]["single"], results["recent"]["single"]
    if a and b:
        print("-" * 78)
        d_mean = b["mean"] - a["mean"]
        d_med = b["median"] - a["median"]
        pct = (d_mean / a["mean"] * 100.0) if a["mean"] else float("nan")
        print(
            f"DELTA (single-model): mean {a['mean']:.1f} -> {b['mean']:.1f} min "
            f"({d_mean:+.1f}, {pct:+.0f}%)   median {a['median']:.1f} -> {b['median']:.1f} ({d_med:+.1f})"
        )
        print("-" * 78)


def report_by_model(conn, args):
    now = int(now_ts(conn))
    start = now - int(args.by_model_days * 86400)
    rows = fetch_reviews(conn, start, now)

    print()
    print("=" * 78)
    print(f"MULTI-MODEL REVIEW DURATION BY SAMPLED MODEL (last {args.by_model_days:g} days)")
    print("=" * 78)
    print(f"Window: {iso(conn, start)} .. {iso(conn, now)}\n")

    # Restricting to framework-active reviews keeps the baseline on the same
    # side of the pipeline upgrade as the multi-model rows; otherwise the
    # baseline is diluted by faster pre-upgrade reviews and every additional
    # model looks slower than it is.
    rows = [r for r in rows if r["experiment_active"]]
    if not rows:
        print("No reviews with experiment data in this window.")
        return

    groups = {}
    sizes = {}
    for r in rows:
        key = r["extra_model"] or "(single-model baseline)"
        groups.setdefault(key, []).append(r["minutes"])
        if r["diff_len"]:
            sizes.setdefault(key, []).append((r["minutes"], r["diff_len"]))

    baseline_key = "(single-model baseline)"
    baseline = summarize(groups.get(baseline_key, []))

    print(fmt_row(baseline_key, baseline))
    print()

    named = sorted(k for k in groups if k != baseline_key)
    ranked = []
    for key in named:
        s = summarize(groups[key])
        if s:
            ranked.append((s["mean"], key, s))
    ranked.sort()

    for _, key, s in ranked:
        line = fmt_row(key, s)
        if baseline and baseline["mean"]:
            delta = s["mean"] - baseline["mean"]
            line += f"  ({delta:+.1f} vs base)"
        print(line)

    if len(ranked) >= 2:
        print()
        lo, hi = ranked[0], ranked[-1]
        print("-" * 78)
        print(
            f"Spread across additional models: {lo[1]} {lo[0]:.1f} min "
            f"-> {hi[1]} {hi[0]:.1f} min  (delta {hi[0] - lo[0]:+.1f} min)"
        )
        print("-" * 78)

    # Sampling probabilities are configured per model, so show whether the
    # observed mix matches expectations and flag thin samples.
    total_multi = sum(len(groups[k]) for k in named)
    if total_multi:
        print("\nSample mix (of multi-model reviews):")
        for _, key, s in ranked:
            print(f"  {key:<26} {s['n']:>4}  ({s['n'] / total_multi * 100:5.1f}%)")
        thin = [k for _, k, s in ranked if s["n"] < 15]
        if thin:
            print(f"\n  NOTE: thin sample (<15) for: {', '.join(thin)} — treat means as indicative.")

    # Patch size is the main confounder on small samples: one huge diff can
    # dominate a bucket of 8.  Minutes per KB of diff makes buckets comparable.
    print("\nSize-normalized (median minutes per KB of diff):")
    norm = []
    for key, pairs in sizes.items():
        rates = [m / (d / 1024.0) for m, d in pairs if d and m <= OUTLIER_MINUTES]
        if rates:
            norm.append((statistics.median(rates), key, len(rates), statistics.median(d for _, d in pairs)))
    for rate, key, n, med_size in sorted(norm):
        print(f"  {key:<26} {rate:5.2f} min/KB   (n={n}, median diff {med_size / 1024.0:.1f} KB)")


def mannwhitney_p(a, b):
    """Two-sided Mann-Whitney U p-value via a normal approximation with ties.

    Used instead of a t-test because review durations are right-skewed, and
    without scipy available this keeps the script dependency-free.  Returns None
    when either sample is too small for the approximation to mean anything.
    """
    n1, n2 = len(a), len(b)
    if n1 < 3 or n2 < 3:
        return None
    combined = sorted([(v, 0) for v in a] + [(v, 1) for v in b])
    # Midranks so tied durations do not bias U.
    ranks = [0.0] * len(combined)
    i = 0
    while i < len(combined):
        j = i
        while j + 1 < len(combined) and combined[j + 1][0] == combined[i][0]:
            j += 1
        midrank = (i + j) / 2.0 + 1.0
        for k in range(i, j + 1):
            ranks[k] = midrank
        i = j + 1

    r1 = sum(rank for rank, (_, grp) in zip(ranks, combined) if grp == 0)
    u1 = r1 - n1 * (n1 + 1) / 2.0
    mu = n1 * n2 / 2.0

    tie_term = 0.0
    i = 0
    while i < len(combined):
        j = i
        while j + 1 < len(combined) and combined[j + 1][0] == combined[i][0]:
            j += 1
        t = j - i + 1
        tie_term += t**3 - t
        i = j + 1
    n = n1 + n2
    var = n1 * n2 / 12.0 * ((n + 1) - tie_term / (n * (n - 1)))
    if var <= 0:
        return None
    # Clamp at 0 so the continuity correction cannot push |U-mu| negative and
    # yield a p-value above 1 when the two samples are nearly identical.
    z = max(0.0, abs(u1 - mu) - 0.5) / (var**0.5)
    # Two-sided normal tail via erfc, no scipy needed.
    import math

    return min(1.0, math.erfc(z / math.sqrt(2)))


def report_marginal(conn, args):
    """Per-model effect pooling co-sampled reviews, for usable sample sizes.

    The exact-combination buckets are too small to read; comparing "this model
    present" against "this model absent" uses every review that involved the
    model and gives each comparison a real baseline.
    """
    now = int(now_ts(conn))
    start = now - int(args.by_model_days * 86400)
    rows = [r for r in fetch_reviews(conn, start, now) if r["experiment_active"]]
    if not rows:
        return

    print()
    print("=" * 78)
    print("MARGINAL EFFECT PER ADDITIONAL MODEL (pooled, framework-active only)")
    print("=" * 78)
    print('"present" pools every review where the model was sampled, alone or alongside another.\n')

    names = sorted(
        {
            n
            for r in rows
            if r["extra_model"]
            for n in r["extra_model"].split("+")
        }
    )
    none_group = [r["minutes"] for r in rows if not r["extra_model"]]
    base = summarize(none_group)
    print(fmt_row("no additional model", base))
    print()

    for name in names:
        present = [
            r["minutes"]
            for r in rows
            if r["extra_model"] and name in r["extra_model"].split("+")
        ]
        s = summarize(present)
        line = fmt_row(f"{name} present", s)
        if s and base:
            p = mannwhitney_p(
                [m for m in present if m <= OUTLIER_MINUTES],
                [m for m in none_group if m <= OUTLIER_MINUTES],
            )
            delta = s["median"] - base["median"]
            sig = "n/a" if p is None else f"p={p:.3f}{' *' if p < 0.05 else ''}"
            line += f"  (median {delta:+.1f} vs none, {sig})"
        print(line)

    print(
        "\n  * = significant at p<0.05 vs the no-additional-model group"
        " (Mann-Whitney, two-sided)."
    )


def report_throughput(conn, args):
    """Daily generation rate, with concurrent-overlap context.

    Wall time can grow either because a review generates more tokens or because
    each token arrives more slowly.  Only the rate distinguishes them.
    """
    now = int(now_ts(conn))
    start = now - int(args.throughput_days * 86400)

    rows = conn.execute(
        """
        SELECT date(r.completed_at, 'unixepoch')               AS day,
               r.id,
               r.created_at,
               r.completed_at,
               (r.completed_at - r.created_at) / 60.0          AS wall,
               a.tokens_out
                 + COALESCE((SELECT SUM(e.tokens_out)
                               FROM model_experiment_runs e
                              WHERE e.review_id = r.id
                                AND e.experiment_name <> 'main'), 0)  AS total_out,
               EXISTS (SELECT 1 FROM model_experiment_sources s
                        WHERE s.review_id = r.id AND s.selected = 1
                          AND s.experiment_name <> 'main')     AS multi
          FROM reviews r
          JOIN ai_interactions a ON a.id = r.interaction_id
         WHERE r.completed_at >= ?
           AND r.completed_at > r.created_at
        """,
        (start,),
    ).fetchall()

    print()
    print("=" * 78)
    print(f"GENERATION THROUGHPUT BY DAY (last {args.throughput_days:g} days)")
    print("=" * 78)
    print("Total output tokens include additional-model runs.\n")
    print(f"  {'day':<12} {'n':>4} {'k out':>8} {'wall min':>9} {'tok/min':>9} {'overlap':>8}")

    by_day = {}
    for r in rows:
        by_day.setdefault(r["day"], []).append(r)

    for day in sorted(by_day):
        group = by_day[day]
        rates = [r["total_out"] / r["wall"] for r in group if r["wall"] > 0]
        overlaps = [
            sum(
                1
                for o in rows
                if o["id"] != r["id"]
                and o["created_at"] < r["completed_at"]
                and o["completed_at"] > r["created_at"]
            )
            for r in group
        ]
        print(
            f"  {day:<12} {len(group):>4} "
            f"{statistics.mean(r['total_out'] for r in group) / 1000.0:>8.1f} "
            f"{statistics.mean(r['wall'] for r in group):>9.1f} "
            f"{statistics.mean(rates):>9.0f} "
            f"{statistics.mean(overlaps):>8.2f}"
        )

    print("\n  single vs multi (whole window, total tokens incl. experiments):")
    for label, want in (("single-model", 0), ("multi-model", 1)):
        group = [r for r in rows if bool(r["multi"]) == bool(want) and r["wall"] > 0]
        if group:
            print(
                f"    {label:<14} n={len(group):<4} "
                f"k_out={statistics.mean(r['total_out'] for r in group) / 1000.0:6.1f}  "
                f"wall={statistics.mean(r['wall'] for r in group):5.1f} min  "
                f"tok/min={statistics.mean(r['total_out'] / r['wall'] for r in group):6.0f}"
            )


def main():
    args = parse_args()
    if not args.compare_windows and not args.by_model and not args.throughput:
        args.compare_windows = args.by_model = args.throughput = True

    conn = connect(args.db)
    try:
        if args.compare_windows:
            report_compare_windows(conn, args)
        if args.by_model:
            report_by_model(conn, args)
            report_marginal(conn, args)
        if args.throughput:
            report_throughput(conn, args)
    finally:
        conn.close()


if __name__ == "__main__":
    main()
