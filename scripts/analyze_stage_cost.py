#!/usr/bin/env python3
"""Chart LLM cost per processing stage over the last N days.

Prints a stacked bar chart of dollar cost per pipeline stage, decomposed by
which model paid for it: the main model, an additional (experiment) model, or
work we do locally on behalf of a remote cross-review instance.

Two data sources, because neither alone covers the pipeline:

  * Conversation dumps (`dump_conversation` in Settings) carry per-stage,
    per-turn usage including real `cache_write_tokens`, so stages sp/0..11 are
    priced exactly and split per model.  Confirmation and cross-review requests
    are NOT dumped -- they bypass SessionRunner.
  * `sashiko.db` carries those two: `model_confirmation_runs` (false-positive
    confirmation) and `cross_review_jobs.merge_tokens_*` (remote-finding dedup
    and confirmation).  Neither stores cache-write counts, so their uncached
    input is priced at the cache-write rate, as the web dashboard does.

Executor vs. driver
-------------------
The two views answer different questions.

`--view executor` (default) asks "which model's rate card was billed".
`--view driver` asks "which program would stop costing money if I turned it
off", and reassigns work executed by one model on another's behalf:

  * Confirmation exists only when an additional model ran: a main-discovered
    finding is confirmed by an additional model, and an additional-discovered
    finding is confirmed by *the main model*.  Both directions are driven by the
    additional-model experiment, so the whole confirm stage moves there.
  * Cross-review merge is main-model inference spent deduplicating, confirming
    and rendering a peer instance's findings, so it moves to the remote bucket.
    The peer's own review cost is the peer's to pay and never appears here.

Repeat LKML reports
-------------------
Stage 11 emits exactly one report per worker run, so repeated reports mean the
whole review ran again (a retry, or `target_review_count` > 1).  Cross-review
results arriving later do *not* re-run stage 11: they render just the new comment
block and splice it into the existing report, charged to the cross-review merge
bucket rather than to stage 11 (see designs/DESIGN_CROSS_INSTANCE_REVIEWS.md).
That means the cross-review merge line covers deduplication, confirmation *and*
report rendering, which are not separable here.  `--repeats` splits every stage
into first-attempt and repeat cost, and calls out stage 11 separately.

Usage:
    scripts/analyze_stage_cost.py --days 7
    scripts/analyze_stage_cost.py --days 3 --view driver --repeats
    scripts/analyze_stage_cost.py --days 30 --by-day --json
"""
import argparse
import json
import os
import re
import sqlite3
import sys
from collections import defaultdict
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

# Anthropic list prices, $/Mtok of input and output.  Bedrock tracks these.
BASE_RATES = {
    "fable-5-1": (10.0, 50.0),
    "fable-5": (10.0, 50.0),
    "opus-5": (5.0, 25.0),
    "opus-4-8": (5.0, 25.0),
    "opus-4-7": (5.0, 25.0),
    "opus-4-6": (5.0, 25.0),
    "opus-4-5": (5.0, 25.0),
    "sonnet-5": (3.0, 15.0),
    "sonnet-4-6": (3.0, 15.0),
    "sonnet-4-5": (3.0, 15.0),
    "haiku-4-5": (1.0, 5.0),
    "gemini-3.1-pro": (1.25, 10.0),
    "gemini-2.5-pro": (1.25, 10.0),
}
# Sonnet 5 introductory pricing, in effect through 2026-08-31.
SONNET_5_INTRO = (2.0, 10.0)

CACHE_READ_MULTIPLIER = 0.1
CACHE_WRITE_MULTIPLIER = 1.25

# Longest first so "opus-4-7" wins over a hypothetical "opus-4" prefix.
RATE_KEYS = sorted(BASE_RATES, key=len, reverse=True)

DUMP_DIR_RE = re.compile(r"^(?P<day>\d{8})-(?P<hhmm>\d{4})-(?P<suffix>[a-z0-9]{4})$")
DUMP_FILE_RE = re.compile(
    r"^(?:(?P<prefix>[a-z0-9.\-]+)-)?s(?P<stage>p|\d+)_(?P<seq>\d+)_(?P<kind>req|resp)\.json$"
)
# `context_tag` is the last field serialized in AiRequest, so the tail of a
# request dump is enough to identify the patch without parsing megabytes of JSON.
CONTEXT_TAG_RE = re.compile(r'"context_tag"\s*:\s*"([^"]*)"')
# Tags come in two shapes: "[ps:12 p:3 s:1] " for stage runs and "[ps:12 p:3s:0] "
# for the pre-phases, which splice "s:N] " onto a truncated prefix.  The patch
# index is digits or "multi", so anchor on that rather than \w+, which would
# otherwise swallow the leading "s" of the stage field.
PATCH_KEY_RE = re.compile(r"ps:(\d+)\s*p:(\d+|multi)")

STAGE_LABELS = {
    "sp": "Stage planning (picks 4-7)",
    "0": "Subsystem guide pre-screen",
    "1": "Analyze commit main goal",
    "2": "High-level implementation",
    "3": "Execution flow",
    "4": "Resource management",
    "5": "Locking and synchronization",
    "6": "Security audit",
    "7": "Hardware engineer's review",
    "8": "Deduplication/consolidation",
    "9": "Concern conflict resolution",
    "10": "Verification and severity",
    "11": "LKML-friendly report",
    "confirm": "False-positive confirmation",
    "cross-review": "Cross-review merge",
}

BUCKETS = ("main", "additional", "remote")
BUCKET_CHARS = {"main": "#", "additional": "=", "remote": "."}


def parse_args():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--days", type=int, default=7, help="Days back to include (default: 7)")
    parser.add_argument(
        "--dump-root",
        type=Path,
        default=Path("/srv/nipa-sashiko/sashiko-dump"),
        help="Directory holding per-review conversation dumps",
    )
    parser.add_argument("--db", type=Path, default=Path("sashiko.db"), help="Path to sashiko.db")
    parser.add_argument(
        "--view",
        choices=["executor", "driver"],
        default="executor",
        help="Attribute cost to the billed model (executor) or the program that caused it (driver)",
    )
    parser.add_argument(
        "--main-model",
        default="us.anthropic.claude-opus-5",
        help="Fallback main model when the DB cannot resolve it per day",
    )
    parser.add_argument(
        "--db-uncached-as-input",
        action="store_true",
        help="Price DB-sourced uncached input at the base input rate instead of the "
        "cache-write rate (lower bound; cache writes are not recorded per run)",
    )
    parser.add_argument(
        "--sonnet-5-intro",
        action="store_true",
        help="Use Sonnet 5 introductory rates ($2/$10) instead of standard ($3/$15)",
    )
    parser.add_argument("--repeats", action="store_true", help="Split first-attempt vs repeat reviews")
    parser.add_argument("--by-day", action="store_true", help="Also chart daily totals per bucket")
    parser.add_argument("--width", type=int, default=46, help="Bar width in columns (default: 46)")
    parser.add_argument("--json", action="store_true", help="Emit JSON instead of a chart")
    return parser.parse_args()


def rate_key_for(model: str):
    """Maps a configured model name or provider model id onto a pricing key.

    Dumps are prefixed with the configured `name` ("fable-5") while the DB stores
    the provider id ("us.anthropic.claude-fable-5"); both must collapse to one
    bucket or the same model is reported twice.
    """
    return next((k for k in RATE_KEYS if k in model), None)


def rates_for(model: str, sonnet_intro: bool):
    """Returns (input, cache_read, cache_write, output) $/Mtok for a model name or id."""
    key = rate_key_for(model)
    if key is None:
        return None
    base_in, base_out = BASE_RATES[key]
    if key == "sonnet-5" and sonnet_intro:
        base_in, base_out = SONNET_5_INTRO
    return (
        base_in,
        base_in * CACHE_READ_MULTIPLIER,
        base_in * CACHE_WRITE_MULTIPLIER,
        base_out,
    )


def usage_cost(prompt, cached, cache_write, completion, rates):
    """Prices one usage block.  prompt_tokens = uncached + cache_read + cache_write."""
    rate_in, rate_read, rate_write, rate_out = rates
    uncached = max(0, prompt - cached - cache_write)
    return (
        uncached * rate_in
        + cached * rate_read
        + cache_write * rate_write
        + completion * rate_out
    ) / 1_000_000


def stage_sort_key(stage):
    # sp runs before the numbered stages; the two DB-sourced stages trail them.
    if stage == "sp":
        return (0, -2)
    if stage == "confirm":
        return (1, 0)
    if stage == "cross-review":
        return (1, 1)
    return (0, int(stage))


def window_days(days):
    """Returns (list of YYYYMMDD strings, unix cutoff) for the trailing window."""
    today = datetime.now(timezone.utc).date()
    dates = [today - timedelta(days=offset) for offset in range(days)]
    cutoff = datetime.combine(
        min(dates), datetime.min.time(), tzinfo=timezone.utc
    ).timestamp()
    return sorted(d.strftime("%Y%m%d") for d in dates), int(cutoff)


def read_context_tag(path: Path):
    """Extracts the `[ps:N p:M ...]` tag from a request dump, reading only its tail."""
    try:
        with path.open("rb") as handle:
            size = handle.seek(0, os.SEEK_END)
            handle.seek(max(0, size - 512))
            tail = handle.read().decode("utf-8", "replace")
    except OSError:
        return None
    match = CONTEXT_TAG_RE.search(tail)
    if match:
        return match.group(1)
    # A truncated tail or reordered fields: fall back to a full parse.  Older
    # dumps are not always objects, so don't assume a dict here.
    try:
        parsed = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return parsed.get("context_tag") if isinstance(parsed, dict) else None


def patch_key(context_tag):
    if not context_tag:
        return None
    match = PATCH_KEY_RE.search(context_tag)
    return f"{match.group(1)}/{match.group(2)}" if match else None


def collect_dumps(dump_root: Path, day_prefixes, main_model_by_day, fallback_main, sonnet_intro):
    """Prices every dumped stage response in the window.

    Returns (rows, stats) where each row is a dict with day, stage, bucket,
    model, cost, tokens and whether its review was a repeat attempt.
    """
    wanted = set(day_prefixes)
    rows = []
    stats = {
        "dirs": 0,
        "dirs_without_tag": 0,
        "responses": 0,
        "unpriced_models": set(),
        "repeat_dirs": 0,
    }
    if not dump_root.is_dir():
        stats["missing_dump_root"] = str(dump_root)
        return rows, stats

    # Group dump dirs by patch so later attempts can be flagged as repeats.  Dir
    # names start with a sortable timestamp, so name order is attempt order.
    selected = []
    for entry in sorted(dump_root.iterdir()):
        match = DUMP_DIR_RE.match(entry.name)
        if not match or match.group("day") not in wanted or not entry.is_dir():
            continue
        selected.append((entry, match.group("day")))
    stats["dirs"] = len(selected)

    attempt_index = {}
    seen_keys = defaultdict(int)
    for entry, _day in selected:
        first_req = next(
            (f for f in sorted(entry.glob("*_req.json"), key=lambda p: p.stat().st_size)),
            None,
        )
        key = patch_key(read_context_tag(first_req)) if first_req else None
        if key is None:
            stats["dirs_without_tag"] += 1
            attempt_index[entry.name] = 0
            continue
        attempt_index[entry.name] = seen_keys[key]
        seen_keys[key] += 1
    stats["repeat_dirs"] = sum(1 for index in attempt_index.values() if index > 0)

    for entry, day in selected:
        repeat = attempt_index.get(entry.name, 0) > 0
        main_model = main_model_by_day.get(day, fallback_main)
        for path in entry.iterdir():
            match = DUMP_FILE_RE.match(path.name)
            if not match or match.group("kind") != "resp":
                continue
            try:
                parsed = json.loads(path.read_text())
            except (OSError, ValueError):
                continue
            if not isinstance(parsed, dict):
                continue
            usage = parsed.get("usage")
            if not isinstance(usage, dict) or not usage:
                continue
            # The filename spells the planning stage "sp_", so the regex group is "p".
            stage = "sp" if match.group("stage") == "p" else match.group("stage")
            prefix = match.group("prefix")
            # No prefix means the main model in older dumps; "main" says so now.
            is_main = prefix is None or prefix == "main"
            model = main_model if is_main else prefix
            rate = rates_for(model, sonnet_intro)
            if rate is None:
                stats["unpriced_models"].add(model)
                continue
            prompt = usage.get("prompt_tokens", 0)
            cached = usage.get("cached_tokens") or 0
            cache_write = usage.get("cache_write_tokens") or 0
            completion = usage.get("completion_tokens", 0)
            stats["responses"] += 1
            rows.append(
                {
                    "day": day,
                    "stage": stage,
                    "bucket": "main" if is_main else "additional",
                    "driver_bucket": "main" if is_main else "additional",
                    "model": rate_key_for(model) or model,
                    "repeat": repeat,
                    "cost": usage_cost(prompt, cached, cache_write, completion, rate),
                    "tokens_in": prompt,
                    "tokens_out": completion,
                    "source": "dump",
                }
            )
    return rows, stats


def collect_db(db_path: Path, cutoff, uncached_as_input, sonnet_intro):
    """Prices the two stages that are never dumped: confirmation and cross-review."""
    rows = []
    stats = {"unpriced_models": set(), "reviews": 0, "main_model_by_day": {}}
    if not db_path.exists():
        stats["missing_db"] = str(db_path)
        return rows, stats
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        # Per-day main model, so a mid-window model swap prices each day correctly.
        counts = defaultdict(lambda: defaultdict(int))
        for row in conn.execute(
            "SELECT strftime('%Y%m%d', created_at, 'unixepoch') AS day, model, count(*) AS n"
            "  FROM reviews WHERE created_at >= ? AND model IS NOT NULL GROUP BY day, model",
            (cutoff,),
        ):
            counts[row["day"]][row["model"]] += row["n"]
            stats["reviews"] += row["n"]
        stats["main_model_by_day"] = {
            day: max(models.items(), key=lambda kv: kv[1])[0] for day, models in counts.items()
        }

        def price(model, tokens_in, tokens_out, tokens_cached):
            rate = rates_for(model, sonnet_intro)
            if rate is None:
                stats["unpriced_models"].add(model)
                return None
            uncached = max(0, (tokens_in or 0) - (tokens_cached or 0))
            # Cache writes are not recorded per run; charge the uncached
            # remainder as a cache write (the dashboard's assumption) unless
            # asked for the base-input lower bound.
            cache_write = 0 if uncached_as_input else uncached
            prompt = uncached + (tokens_cached or 0)
            return usage_cost(prompt, tokens_cached or 0, cache_write, tokens_out or 0, rate)

        for row in conn.execute(
            "SELECT strftime('%Y%m%d', r.created_at, 'unixepoch') AS day,"
            "       c.model AS confirmer, c.model_id AS model_id,"
            "       sum(c.tokens_in) AS tin, sum(c.tokens_out) AS tout,"
            "       sum(c.tokens_cached) AS tcached"
            "  FROM model_confirmation_runs c JOIN reviews r ON r.id = c.review_id"
            " WHERE r.created_at >= ? GROUP BY day, c.model, c.model_id",
            (cutoff,),
        ):
            model = row["model_id"] or row["confirmer"]
            cost = price(model, row["tin"], row["tout"], row["tcached"])
            if cost is None:
                continue
            rows.append(
                {
                    "day": row["day"],
                    "stage": "confirm",
                    # Main confirms additional-model findings and vice versa.
                    "bucket": "main" if row["confirmer"] == "main" else "additional",
                    # Confirmation only runs when an additional model ran, so
                    # the whole stage is downstream of that experiment.
                    "driver_bucket": "additional",
                    "model": rate_key_for(model) or model,
                    "repeat": False,
                    "cost": cost,
                    "tokens_in": row["tin"] or 0,
                    "tokens_out": row["tout"] or 0,
                    "source": "db",
                }
            )

        for row in conn.execute(
            "SELECT strftime('%Y%m%d', COALESCE(completed_at, first_attempt_at), 'unixepoch') AS day,"
            "       local_model, sum(merge_tokens_in) AS tin, sum(merge_tokens_out) AS tout,"
            "       sum(merge_tokens_cached) AS tcached"
            "  FROM cross_review_jobs"
            " WHERE COALESCE(completed_at, first_attempt_at) >= ?"
            " GROUP BY day, local_model",
            (cutoff,),
        ):
            model = row["local_model"] or ""
            cost = price(model, row["tin"], row["tout"], row["tcached"])
            if cost is None or not cost:
                continue
            rows.append(
                {
                    "day": row["day"],
                    "stage": "cross-review",
                    # Merging a peer's findings is main-model inference we pay for.
                    "bucket": "main",
                    "driver_bucket": "remote",
                    "model": rate_key_for(model) or model,
                    "repeat": False,
                    "cost": cost,
                    "tokens_in": row["tin"] or 0,
                    "tokens_out": row["tout"] or 0,
                    "source": "db",
                }
            )
    finally:
        conn.close()
    return rows, stats


def bar(values, width, total_max):
    """Renders one stacked bar of {bucket: cost} scaled against total_max."""
    if total_max <= 0:
        return ""
    out = []
    for bucket in BUCKETS:
        cells = int(round(values.get(bucket, 0.0) / total_max * width))
        out.append(BUCKET_CHARS[bucket] * cells)
    return "".join(out)


def print_chart(title, keys, labels, series, width, unit="$"):
    """Prints a horizontal stacked bar chart, one row per key."""
    totals = {key: sum(series[key].values()) for key in keys}
    top = max(totals.values(), default=0.0)
    grand = sum(totals.values())
    label_width = max((len(labels[key]) for key in keys), default=0)
    print(f"  {'':<{label_width}}  {'cost':>9} {'share':>6}  chart")
    for key in keys:
        share = totals[key] * 100 / grand if grand else 0.0
        print(
            f"  {labels[key]:<{label_width}}  {unit}{totals[key]:>8.2f} {share:>5.1f}%  "
            f"{bar(series[key], width, top)}"
        )
    print(f"  {'':<{label_width}}  {unit}{grand:>8.2f}")
    legend = "   ".join(f"{BUCKET_CHARS[b]} {b}" for b in BUCKETS)
    print(f"\n  legend: {legend}")


def main():
    args = parse_args()
    if args.days < 1:
        sys.exit("error: --days must be at least 1")

    day_prefixes, cutoff = window_days(args.days)
    db_rows, db_stats = collect_db(
        args.db, cutoff, args.db_uncached_as_input, args.sonnet_5_intro
    )
    dump_rows, dump_stats = collect_dumps(
        args.dump_root,
        day_prefixes,
        db_stats["main_model_by_day"],
        args.main_model,
        args.sonnet_5_intro,
    )
    rows = dump_rows + db_rows
    # DB rows are keyed on their own day column; drop any that fall outside the
    # dump window so both sources cover the same days.
    wanted = set(day_prefixes)
    rows = [row for row in rows if row["day"] in wanted]

    bucket_field = "bucket" if args.view == "executor" else "driver_bucket"

    by_stage = defaultdict(lambda: defaultdict(float))
    by_day = defaultdict(lambda: defaultdict(float))
    by_model = defaultdict(float)
    repeat_by_stage = defaultdict(lambda: {"first": 0.0, "repeat": 0.0})
    for row in rows:
        by_stage[row["stage"]][row[bucket_field]] += row["cost"]
        by_day[row["day"]][row[bucket_field]] += row["cost"]
        by_model[row["model"]] += row["cost"]
        repeat_by_stage[row["stage"]]["repeat" if row["repeat"] else "first"] += row["cost"]

    stages = sorted(by_stage, key=stage_sort_key)
    total = sum(sum(v.values()) for v in by_stage.values())

    if args.json:
        json.dump(
            {
                "days": args.days,
                "window": [day_prefixes[0], day_prefixes[-1]],
                "view": args.view,
                "total_cost": round(total, 4),
                "by_stage": {
                    stage: {
                        "label": STAGE_LABELS.get(stage, stage),
                        "total": round(sum(by_stage[stage].values()), 4),
                        **{b: round(by_stage[stage].get(b, 0.0), 4) for b in BUCKETS},
                        "first": round(repeat_by_stage[stage]["first"], 4),
                        "repeat": round(repeat_by_stage[stage]["repeat"], 4),
                    }
                    for stage in stages
                },
                "by_day": {
                    day: {b: round(by_day[day].get(b, 0.0), 4) for b in BUCKETS}
                    for day in sorted(by_day)
                },
                "by_model": {m: round(c, 4) for m, c in sorted(by_model.items())},
                "coverage": {
                    "dump_dirs": dump_stats["dirs"],
                    "dump_responses": dump_stats["responses"],
                    "dump_dirs_repeat": dump_stats["repeat_dirs"],
                    "dump_dirs_without_context_tag": dump_stats["dirs_without_tag"],
                    "db_reviews": db_stats["reviews"],
                    "unpriced_models": sorted(
                        dump_stats["unpriced_models"] | db_stats["unpriced_models"]
                    ),
                },
            },
            sys.stdout,
            indent=2,
        )
        print()
        return

    print("=" * 78)
    print(
        f"COST BY PROCESSING STAGE  ({args.days}d: {day_prefixes[0]}..{day_prefixes[-1]}, "
        f"{args.view} view)"
    )
    print("=" * 78)
    if not rows:
        print("\n  No priced usage found in this window.")
        if "missing_dump_root" in dump_stats:
            print(f"  dump root not found: {dump_stats['missing_dump_root']}")
        if "missing_db" in db_stats:
            print(f"  database not found: {db_stats['missing_db']}")
        return

    labels = {}
    for stage in stages:
        # sp/confirm/cross-review are already named; only numbered stages get "s".
        name = stage if not stage.isdigit() else f"s{stage}"
        labels[stage] = f"{name:<12} {STAGE_LABELS.get(stage, '')}".rstrip()
    print()
    print_chart("stage", stages, labels, by_stage, args.width)

    reviews = db_stats["reviews"]
    if reviews:
        print(f"\n  {reviews} reviews in window -> ${total / reviews:.2f} per review")

    print("\n  Per model:")
    for model, cost in sorted(by_model.items(), key=lambda kv: -kv[1]):
        print(f"    {model:<34} ${cost:>8.2f} ({cost * 100 / total:>4.1f}%)")

    if args.by_day:
        print("\n" + "-" * 78)
        print("DAILY TOTALS")
        print("-" * 78)
        days = sorted(by_day)
        day_labels = {d: f"{d[:4]}-{d[4:6]}-{d[6:]}" for d in days}
        print_chart("day", days, day_labels, by_day, args.width)

    if args.repeats:
        print("\n" + "-" * 78)
        print("REPEATED REVIEWS (dump-sourced stages only)")
        print("-" * 78)
        repeat_total = sum(v["repeat"] for v in repeat_by_stage.values())
        print(
            f"  {dump_stats['repeat_dirs']} of {dump_stats['dirs']} dumped review runs were a "
            f"second or later attempt on the same patch"
        )
        label_width = max(len(labels[stage]) for stage in stages)
        print(f"  {'stage':<{label_width}} {'first':>9} {'repeat':>9} {'repeat%':>8}")
        for stage in stages:
            split = repeat_by_stage[stage]
            stage_total = split["first"] + split["repeat"]
            if stage_total <= 0:
                continue
            pct = split["repeat"] * 100 / stage_total
            print(
                f"  {labels[stage]:<{label_width}} "
                f"${split['first']:>8.2f} ${split['repeat']:>8.2f} {pct:>7.1f}%"
            )
        print(f"  {'TOTAL repeat cost':<{label_width}} {'':>9} ${repeat_total:>8.2f}")
        s11 = repeat_by_stage.get("11", {"first": 0.0, "repeat": 0.0})
        s11_total = s11["first"] + s11["repeat"]
        if s11_total > 0:
            print(
                f"\n  LKML report (stage 11) regenerated on repeat attempts: "
                f"${s11['repeat']:.2f} of ${s11_total:.2f}"
            )
        print(
            "  Cross-review results arriving later render only the new comment block,\n"
            "  billed to cross-review merge, and do NOT re-run stage 11, so repeats\n"
            "  here are review retries or a target_review_count above 1."
        )

    print("\n" + "-" * 78)
    print("COVERAGE AND CAVEATS")
    print("-" * 78)
    print(
        f"  dumps: {dump_stats['responses']} responses across {dump_stats['dirs']} review runs "
        f"(stages sp, 0-11; exact cache-write split)"
    )
    print(
        "  db:    confirmation + cross-review merge; these are not dumped, and neither\n"
        "         table records cache writes, so their uncached input is priced at the\n"
        f"         {'base input' if args.db_uncached_as_input else 'cache-write'} rate"
        f"{' (lower bound)' if args.db_uncached_as_input else ' (as the dashboard does)'}"
    )
    if reviews and dump_stats["dirs"] and dump_stats["dirs"] < reviews * 0.9:
        print(
            f"  WARNING: {dump_stats['dirs']} dump dirs vs {reviews} DB reviews -- dumping was\n"
            "         off or pruned for part of this window, so stage costs are understated."
        )
    if dump_stats["dirs_without_tag"]:
        print(
            f"  {dump_stats['dirs_without_tag']} dump dirs had no context tag; counted as "
            "first attempts."
        )
    unpriced = sorted(dump_stats["unpriced_models"] | db_stats["unpriced_models"])
    if unpriced:
        print(f"  UNPRICED (excluded, no rate known): {', '.join(unpriced)}")
    print(
        f"  rates: cache read {CACHE_READ_MULTIPLIER}x input, cache write "
        f"{CACHE_WRITE_MULTIPLIER}x input; Sonnet 5 at "
        f"{'introductory' if args.sonnet_5_intro else 'standard'} pricing"
    )
    if args.view == "executor":
        print(
            "  executor view: confirmation of additional-model findings is billed to the\n"
            "         main model, and cross-review merge is main-model inference. Use\n"
            "         --view driver to charge those to the program that caused them."
        )
    else:
        print(
            "  driver view: the whole confirm stage is charged to the additional-model\n"
            "         experiment (it only runs when one was sampled) and cross-review\n"
            "         merge to the remote bucket. A peer instance's own review cost is\n"
            "         paid by that peer and never appears here."
        )


if __name__ == "__main__":
    main()
