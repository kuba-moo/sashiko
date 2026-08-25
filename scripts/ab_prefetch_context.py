#!/usr/bin/env python3
"""A/B the prepared context of two prefetch implementations on identical patches.

`analyze_context_efficiency.py` compares reviews before and after a change, so
its numbers carry whatever else differed between the two eras -- above all the
patches themselves, which are whatever was posted that day.  This tool removes
that confound: it replays the *same* commits through two `dump_prefetch`
binaries and diffs only the prepared context each one produces.

What it isolates, and what it cannot
------------------------------------
It measures the input side exactly -- prefetch bytes per patch byte, and how
often the new code omits a function the old one included.  It says nothing
about turns or tool calls, because those need a model in the loop; for those,
see the observational tool.  A drop here is a real cost saving only if the
model does not spend the difference reading the omitted code back with tools.

Commits come from the conversation dumps (`Target Commit SHA` in the stage
system prompt), so the sample is the patches actually reviewed, and each is
replayed against the baseline that review used.

Usage:
    scripts/ab_prefetch_context.py --old /tmp/sashiko-pre --new .
    scripts/ab_prefetch_context.py --old /tmp/sashiko-pre --new . --limit 100 --json
"""
import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

DUMP_DEFAULT = "/srv/nipa-sashiko/sashiko-dump"
KERNEL_DEFAULT = "/srv/nipa-sashiko/sashiko/third_party/linux"

REQ_RE = re.compile(r"^(?:[a-z0-9._-]+-)?s[1-7]_001_req\.json$")
SHA_RE = re.compile(r"Target Commit SHA: (\w+)")
BASE_RE = re.compile(r"Baseline SHA: (\w+)")
# Prefetch payload entries look like "--- path:line (symbol) ---".
ENTRY_RE = re.compile(r"^--- (\S+?):(\d+)(?: \((.*?)\))? ---$", re.M)


def parse_args():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--old", type=Path, required=True, help="worktree with the pre-change binary")
    p.add_argument("--new", type=Path, required=True, help="worktree with the post-change binary")
    p.add_argument("--dumps", type=Path, default=Path(DUMP_DEFAULT))
    p.add_argument("--kernel", type=Path, default=Path(KERNEL_DEFAULT))
    p.add_argument("--min-day", default="20260801")
    p.add_argument("--limit", type=int, default=0)
    p.add_argument("--jobs", type=int, default=4)
    # One kernel worktree per job, ~1.5G each, so this must not default to a
    # small tmpfs.
    p.add_argument("--scratch", type=Path, default=Path("/srv/nipa-sashiko/ab-scratch"),
                   help="directory to hold the per-job kernel worktrees")
    p.add_argument("--json", action="store_true")
    return p.parse_args()


def collect_commits(dumps: Path, min_day: str, limit: int):
    """Distinct (sha, baseline) pairs from reviews we dumped, in dump order."""
    seen = {}
    for d in sorted(dumps.iterdir()):
        if not d.is_dir() or d.name[:8] < min_day:
            continue
        for name in sorted(os.listdir(d)):
            if not REQ_RE.match(name):
                continue
            try:
                system = json.loads((d / name).read_text()).get("system")
            except (OSError, json.JSONDecodeError):
                break
            if not isinstance(system, str):
                break
            m, b = SHA_RE.search(system), BASE_RE.search(system)
            if m and m.group(1) not in seen:
                seen[m.group(1)] = b.group(1) if b else None
            break
        if limit and len(seen) >= limit:
            break
    items = list(seen.items())
    return items[:limit] if limit else items


def git(args, cwd, **kw):
    return subprocess.run(["git", "-C", str(cwd)] + args, capture_output=True,
                          text=True, errors="replace", **kw)


def entries(payload: str):
    """Set of (file, symbol-or-line) keys present in a prefetch payload."""
    out = set()
    for m in ENTRY_RE.finditer(payload):
        out.add((m.group(1), m.group(3) or m.group(2)))
    return out


def run_one(sha, baseline, args, worktree):
    """Replay one commit through both binaries in a private kernel worktree."""
    show = git(["show", sha], args.kernel)
    if show.returncode != 0:
        return None
    diff = show.stdout
    idx = diff.find("\ndiff --git ")
    if idx < 0:
        return None

    # prefetch reads the applied tree, so check out the commit itself.
    if git(["checkout", "--detach", "--force", sha], worktree).returncode != 0:
        return None

    with tempfile.NamedTemporaryFile("w", suffix=".patch", delete=False) as fh:
        fh.write(diff)
        patch_path = fh.name
    try:
        payloads = {}
        for era, root in (("old", args.old), ("new", args.new)):
            binary = root / "target" / "release" / "dump_prefetch"
            proc = subprocess.run(
                [str(binary), "--worktree", str(worktree), "--patch", patch_path],
                capture_output=True, text=True, errors="replace", timeout=300)
            if proc.returncode != 0:
                return None
            payloads[era] = proc.stdout
    except subprocess.TimeoutExpired:
        return None
    finally:
        os.unlink(patch_path)

    old_e, new_e = entries(payloads["old"]), entries(payloads["new"])
    return {
        "sha": sha,
        "diff_chars": len(diff[idx + 1:]),
        "old_chars": len(payloads["old"]),
        "new_chars": len(payloads["new"]),
        "old_entries": len(old_e),
        "new_entries": len(new_e),
        "omitted": len(old_e - new_e),
        "added": len(new_e - old_e),
        "identical": payloads["old"] == payloads["new"],
    }


def main():
    args = parse_args()
    for root in (args.old, args.new):
        if not (root / "target" / "release" / "dump_prefetch").exists():
            sys.exit(f"missing {root}/target/release/dump_prefetch")

    commits = collect_commits(args.dumps, args.min_day, args.limit)
    print(f"replaying {len(commits)} commits through both binaries...", file=sys.stderr)

    # Each thread needs its own worktree: prefetch reads the checked-out tree.
    args.scratch.mkdir(parents=True, exist_ok=True)
    base = Path(tempfile.mkdtemp(prefix="ab-prefetch-", dir=str(args.scratch)))
    trees = []
    try:
        for i in range(args.jobs):
            wt = base / f"wt{i}"
            r = git(["worktree", "add", "--detach", "--force", str(wt), "HEAD"], args.kernel)
            if r.returncode != 0:
                sys.exit(f"worktree add failed: {r.stderr}")
            trees.append(wt)

        results = []
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = [pool.submit(run_one, sha, b, args, trees[i % args.jobs])
                       for i, (sha, b) in enumerate(commits)]
            for n, f in enumerate(futures, 1):
                r = f.result()
                if r:
                    results.append(r)
                if n % 25 == 0:
                    print(f"  {n}/{len(futures)}", file=sys.stderr)
    finally:
        for wt in trees:
            git(["worktree", "remove", "--force", str(wt)], args.kernel)
        subprocess.run(["rm", "-rf", str(base)])

    if not results:
        sys.exit("no commits replayed successfully")

    def med(key):
        return statistics.median([r[key] for r in results])

    ratios_old = [r["old_chars"] / r["diff_chars"] for r in results if r["diff_chars"]]
    ratios_new = [r["new_chars"] / r["diff_chars"] for r in results if r["diff_chars"]]
    total_old = sum(r["old_chars"] for r in results)
    total_new = sum(r["new_chars"] for r in results)
    changed = [r for r in results if not r["identical"]]

    out = {
        "commits": len(results),
        "changed_payload": len(changed),
        "median_diff_chars": med("diff_chars"),
        "median_old_chars": med("old_chars"),
        "median_new_chars": med("new_chars"),
        "median_ratio_old": statistics.median(ratios_old),
        "median_ratio_new": statistics.median(ratios_new),
        "total_old_chars": total_old,
        "total_new_chars": total_new,
        "total_change_pct": 100.0 * (total_new - total_old) / total_old if total_old else None,
        "median_entries_old": med("old_entries"),
        "median_entries_new": med("new_entries"),
        "total_omitted_entries": sum(r["omitted"] for r in results),
        "total_added_entries": sum(r["added"] for r in results),
    }
    if changed:
        ch_old = sum(r["old_chars"] for r in changed)
        ch_new = sum(r["new_chars"] for r in changed)
        out["affected_change_pct"] = 100.0 * (ch_new - ch_old) / ch_old if ch_old else None

    if args.json:
        print(json.dumps({"summary": out, "per_commit": results}, indent=2))
        return

    print(f"\nCommits replayed: {out['commits']}"
          f"   payload changed on: {out['changed_payload']}"
          f" ({100.0*out['changed_payload']/out['commits']:.0f}%)")
    print(f"\n{'medians':<32} {'old':>12} {'new':>12}   {'change':>8}")
    for label, a, b in (
        ("prefetch chars", out["median_old_chars"], out["median_new_chars"]),
        ("prefetch / patch ratio", out["median_ratio_old"], out["median_ratio_new"]),
        ("payload entries", out["median_entries_old"], out["median_entries_new"]),
    ):
        d = 100.0 * (b - a) / a if a else 0.0
        print(f"  {label:<30} {a:>12,.2f} {b:>12,.2f}   {d:>+7.1f}%")
    print(f"\n  median patch+message chars: {out['median_diff_chars']:,.0f}")
    print(f"  total prefetch chars: {total_old:,} -> {total_new:,} "
          f"({out['total_change_pct']:+.1f}%)")
    if changed:
        print(f"  on the {len(changed)} affected commits only: "
              f"{out['affected_change_pct']:+.1f}%")
    print(f"  entries omitted by new: {out['total_omitted_entries']:,}"
          f"   newly added: {out['total_added_entries']:,}")


if __name__ == "__main__":
    main()
