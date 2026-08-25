#!/usr/bin/env python3
"""Measure review context efficiency around a prepared-context change.

Answers "did shrinking the prepared context make reviews cheaper", by pairing
two things the dumps hold but the database does not:

  * the prepared context itself -- the `<pre_fetched_context>` block and the
    `Target Commit:` diff that precede it in every stage's system prompt, so
    context size can be normalized by patch size rather than read raw; and
  * what each stage then did about it -- turns, tool calls, and tool-result
    bytes, which is where a context that under-serves the model shows up.

Both live in the same place: a per-review dump directory of
`<source>-s<N>_<turn>_{req,resp}.json` files written by `ConversationDumper`.
The `<pre_fetched_context>` block is built once per review in the shared system
prompt (prompts.rs), so it is read from a single first-turn request per
directory and applies to every source in it.

Attribution caveat
------------------
`reviews.prompts_hash` is the repo HEAD when the review started, so a hash
identifies a code state, not one change.  If several commits land between two
reviews they share a hash boundary and this tool cannot separate them; use
--source to compare variants within a review, which holds the prepared context
fixed and varies only the per-stage prompts.

Usage:
    scripts/analyze_context_efficiency.py --split 3433d11
    scripts/analyze_context_efficiency.py --split 3433d11 --by-stage
    scripts/analyze_context_efficiency.py --sources main,old-prompts --json
"""
import argparse
import json
import os
import re
import sqlite3
import statistics
import sys
from collections import defaultdict
from pathlib import Path

DUMP_DEFAULT = "/srv/nipa-sashiko/sashiko-dump"
DB_DEFAULT = "sashiko.db"

# <source>-s<stage>_<turn>_<kind>.json, plus the sourceless merge/planning
# labels (sp, s0, s8..s11) that only ever run on the main model.
FILE_RE = re.compile(r"^(?:(?P<source>[a-z0-9._-]+)-)?s(?P<stage>[a-z0-9]+)_(?P<turn>\d+)_(?P<kind>req|resp)\.json$")
PREFETCH_RE = re.compile(r"<pre_fetched_context>\n(.*?)\n</pre_fetched_context>", re.S)
TARGET_RE = re.compile(r"\n\nTarget Commit(?: Diff)?:\n")
CONTEXT_TAG_RE = re.compile(r"\[ps:(\d+) p:(\S+)")

# Discovery stages 1-7 are the ones that receive the prepared context and can
# spend turns on it.  sp/s0 are prompt-selection, s8-s11 merge and publish.
DISCOVERY = {str(n) for n in range(1, 8)}


def parse_args():
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    p.add_argument("--dumps", type=Path, default=Path(DUMP_DEFAULT))
    p.add_argument("--db", type=Path, default=Path(DB_DEFAULT))
    p.add_argument("--split", help="prompts_hash prefix that begins the 'after' era")
    p.add_argument("--sources", default="main",
                   help="comma-separated dump source labels to report (default: main)")
    p.add_argument("--by-stage", action="store_true", help="break metrics down per discovery stage")
    p.add_argument("--min-day", default="20260720", help="earliest dump directory day (YYYYMMDD)")
    p.add_argument("--json", action="store_true")
    p.add_argument("--rows", type=Path,
                   help="also write one JSON record per review here, for stratified analysis")
    return p.parse_args()


def load_prompt_eras(db: Path):
    """Map (patchset_id, patch_index) -> prompts_hash, for reviews we can date."""
    if not db.exists():
        return {}
    con = sqlite3.connect(f"file:{db}?mode=ro&immutable=1", uri=True)
    rows = con.execute(
        """
        SELECT r.patchset_id, p.part_index, r.prompts_hash, r.created_at
        FROM reviews r LEFT JOIN patches p ON p.id = r.patch_id
        WHERE r.prompts_hash IS NOT NULL
        """
    ).fetchall()
    con.close()
    eras = {}
    for ps, part, phash, created in rows:
        key = (str(ps), str(part) if part is not None else "multi")
        # A patch can be reviewed more than once; keep the earliest, which is
        # the attempt whose dump directory sorts first.
        prev = eras.get(key)
        if prev is None or created < prev[1]:
            eras[key] = (phash, created)
    return {k: v[0] for k, v in eras.items()}


def read_json(path: Path):
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def split_prepared_context(system: str):
    """Return (diff_chars, prefetch_chars) from a stage system prompt."""
    prefetch = 0
    m = PREFETCH_RE.search(system)
    if m:
        body = m.group(1)
        # Drop the two-line instruction preamble that precedes the payload.
        parts = body.split("\n\n", 1)
        prefetch = len(parts[1]) if len(parts) == 2 else len(body)
        system = system[: m.start()] + system[m.end():]

    diff = 0
    t = TARGET_RE.search(system)
    if t:
        diff = len(system[t.end():])
    return diff, prefetch


def scan_dump(d: Path):
    """Collect per-source metrics for one review dump directory."""
    try:
        names = os.listdir(d)
    except OSError:
        return None

    # Per (source, stage): turn -> (req path, resp path)
    stages = defaultdict(dict)
    for name in names:
        m = FILE_RE.match(name)
        if not m:
            continue
        src = m.group("source") or "main"
        key = (src, m.group("stage"))
        turn = int(m.group("turn"))
        slot = stages[key].setdefault(turn, {})
        slot[m.group("kind")] = d / name

    if not stages:
        return None

    result = {"dir": d.name, "sources": defaultdict(lambda: defaultdict(dict))}

    # The prepared context is identical for every source in a review, so read
    # it from whichever discovery stage's first turn is available.
    for (src, stage), turns in sorted(stages.items()):
        if stage not in DISCOVERY or 1 not in turns or "req" not in turns[1]:
            continue
        req = read_json(turns[1]["req"])
        if not req or not isinstance(req.get("system"), str):
            continue
        diff, prefetch = split_prepared_context(req["system"])
        if diff == 0 and prefetch == 0:
            continue
        result["diff_chars"] = diff
        result["prefetch_chars"] = prefetch
        tag = CONTEXT_TAG_RE.search(req.get("context_tag") or "")
        if tag:
            result["patchset_id"], result["patch_index"] = tag.group(1), tag.group(2)
        break

    if "diff_chars" not in result:
        return None

    for (src, stage), turns in stages.items():
        turn_nums = sorted(turns)
        resp_turns = [t for t in turn_nums if "resp" in turns[t]]
        if not resp_turns:
            continue
        tool_calls = 0
        tool_bytes = 0
        cache_write = prompt_toks = completion = 0
        for t in resp_turns:
            resp = read_json(turns[t]["resp"])
            if not resp:
                continue
            tool_calls += len(resp.get("tool_calls") or [])
            u = resp.get("usage") or {}
            cache_write += u.get("cache_write_tokens", 0) or 0
            prompt_toks += u.get("prompt_tokens", 0) or 0
            completion += u.get("completion_tokens", 0) or 0
        # Tool-result bytes arrive as `tool` messages in the *next* request, so
        # the last request in the stage carries the full set.
        last_req = max((t for t in turn_nums if "req" in turns[t]), default=None)
        if last_req is not None:
            req = read_json(turns[last_req]["req"])
            for msg in (req or {}).get("messages", []):
                if msg.get("role") == "tool":
                    tool_bytes += len(str(msg.get("content") or ""))

        result["sources"][src][stage] = {
            "turns": len(resp_turns),
            "tool_calls": tool_calls,
            "tool_bytes": tool_bytes,
            "cache_write_tokens": cache_write,
            "prompt_tokens": prompt_toks,
            "completion_tokens": completion,
        }
    return result


def summarize(values):
    if not values:
        return None
    return {
        "n": len(values),
        "mean": statistics.mean(values),
        "median": statistics.median(values),
    }


def pct(after, before):
    if before in (None, 0):
        return None
    return 100.0 * (after - before) / before


def main():
    args = parse_args()
    eras = load_prompt_eras(args.db)
    sources = [s.strip() for s in args.sources.split(",") if s.strip()]

    reviews = []
    for d in sorted(args.dumps.iterdir()):
        if not d.is_dir() or d.name[:8] < args.min_day:
            continue
        r = scan_dump(d)
        if not r:
            continue
        key = (r.get("patchset_id"), r.get("patch_index"))
        r["prompts_hash"] = eras.get(key)
        reviews.append(r)

    if args.split:
        split_hash = next(
            (r["prompts_hash"] for r in reviews
             if r["prompts_hash"] and r["prompts_hash"].startswith(args.split)), None)
        if not split_hash:
            sys.exit(f"no reviews found with prompts_hash starting {args.split}")
        # Era membership by first appearance of the split hash in dump order.
        first_after = min((r["dir"] for r in reviews if r["prompts_hash"] == split_hash),
                          default=None)
        for r in reviews:
            r["era"] = "after" if r["dir"] >= first_after else "before"
    else:
        for r in reviews:
            r["era"] = "all"

    if args.rows:
        with args.rows.open("w") as fh:
            for r in reviews:
                rec = {k: r[k] for k in
                       ("dir", "era", "prompts_hash", "diff_chars", "prefetch_chars")}
                rec["patchset_id"] = r.get("patchset_id")
                rec["sources"] = {
                    src: {st: v for st, v in stages.items() if st in DISCOVERY}
                    for src, stages in r["sources"].items()
                }
                fh.write(json.dumps(rec) + "\n")

    out = {"reviews": len(reviews), "eras": {}}
    for era in ("before", "after", "all"):
        subset = [r for r in reviews if r["era"] == era]
        if not subset:
            continue
        block = {"reviews": len(subset)}
        block["prefetch_chars"] = summarize([r["prefetch_chars"] for r in subset])
        block["diff_chars"] = summarize([r["diff_chars"] for r in subset])
        block["prefetch_per_diff"] = summarize(
            [r["prefetch_chars"] / r["diff_chars"] for r in subset if r["diff_chars"]])
        for src in sources:
            per_review_turns, per_review_calls, per_review_bytes = [], [], []
            per_stage = defaultdict(lambda: defaultdict(list))
            for r in subset:
                st = r["sources"].get(src)
                if not st:
                    continue
                disc = {k: v for k, v in st.items() if k in DISCOVERY}
                if not disc:
                    continue
                per_review_turns.append(sum(v["turns"] for v in disc.values()))
                per_review_calls.append(sum(v["tool_calls"] for v in disc.values()))
                per_review_bytes.append(sum(v["tool_bytes"] for v in disc.values()))
                for stage, v in disc.items():
                    for metric in ("turns", "tool_calls", "tool_bytes"):
                        per_stage[stage][metric].append(v[metric])
            block[src] = {
                "turns_per_review": summarize(per_review_turns),
                "tool_calls_per_review": summarize(per_review_calls),
                "tool_bytes_per_review": summarize(per_review_bytes),
            }
            if args.by_stage:
                block[src]["by_stage"] = {
                    stage: {m: summarize(v) for m, v in metrics.items()}
                    for stage, metrics in sorted(per_stage.items())
                }
        out["eras"][era] = block

    if args.json:
        print(json.dumps(out, indent=2))
        return

    def row(label, before, after, unit=""):
        if before is None and after is None:
            return
        b = f"{before['median']:,.1f}" if before else "-"
        a = f"{after['median']:,.1f}" if after else "-"
        delta = pct(after["median"], before["median"]) if before and after else None
        d = f"{delta:+.1f}%" if delta is not None else ""
        print(f"  {label:<34} {b:>12} {a:>12}   {d:>8}{unit}")

    before = out["eras"].get("before")
    after = out["eras"].get("after") or out["eras"].get("all")
    print(f"Reviews scanned: {out['reviews']}"
          f"  (before={before['reviews'] if before else 0},"
          f" after={after['reviews'] if after else 0})")
    print(f"\n{'medians':<36} {'before':>12} {'after':>12}   {'change':>8}")
    for label, key in (("prefetched context (chars)", "prefetch_chars"),
                       ("patch + message (chars)", "diff_chars"),
                       ("prefetch / patch ratio", "prefetch_per_diff")):
        row(label, before.get(key) if before else None, after.get(key) if after else None)
    for src in sources:
        b = (before or {}).get(src)
        a = (after or {}).get(src)
        if not (b or a):
            continue
        print(f"\n  [{src}] discovery stages 1-7, per review")
        for label, key in (("turns", "turns_per_review"),
                           ("tool calls", "tool_calls_per_review"),
                           ("tool result bytes", "tool_bytes_per_review")):
            row(label, (b or {}).get(key), (a or {}).get(key))
        if args.by_stage:
            for stage in sorted(set((b or {}).get("by_stage", {})) |
                                set((a or {}).get("by_stage", {}))):
                bs = (b or {}).get("by_stage", {}).get(stage, {})
                as_ = (a or {}).get("by_stage", {}).get(stage, {})
                print(f"    stage {stage}")
                for label, key in (("turns", "turns"), ("tool calls", "tool_calls")):
                    row(f"      {label}", bs.get(key), as_.get(key))


if __name__ == "__main__":
    main()
