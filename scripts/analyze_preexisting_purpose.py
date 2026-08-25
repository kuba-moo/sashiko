#!/usr/bin/env python3
"""Quantify `preexisting=1` findings whose own prose says the patch's stated
purpose is defeated -- the "useless fix" class the human reviewer flagged.

Read-only: the server owns sashiko.db, so open with mode=ro and never write.

Two questions:
  1. What Patchwork state did a given review post? (preexisting is excluded
     from the state calculation in src/patchwork.rs, so a High can post
     "success".)
  2. How large is the population of preexisting=1 findings that argue, in
     severity_explanation or problem, that the patch does not achieve what its
     changelog claims?

The keyword predicates below are heuristics for *sizing* the population, not
verdicts on individual findings.
"""

import re
import sqlite3
import sys
from collections import Counter

DB = "file:sashiko.db?mode=ro"

# Severity ints per src/schema.sql: 1 Low, 2 Medium, 3 High, 4 Critical.
SEV = {1: "Low", 2: "Medium", 3: "High", 4: "Critical"}

# The narrow discriminator: the finding ties the defect to the patch's own
# advertised purpose, not merely to a nearby sibling location.
PURPOSE_DEFEATED = [
    r"stated purpose",
    r"patch(?:'s)? (?:is )?(?:advertised|presented) as",
    r"claims to (?:fix|close|address)",
    r"does not achieve",
    r"incomplete fix",
    r"incomplete for",
    r"leaves? (?:an )?equivalent path",
    r"does not remove the root cause",
    r"still reachable",
]

# The broader "sibling location" test, for contrast.
SIBLING = [
    r"\bsibling\b", r"\bpeer\b", r"\bcounterpart\b", r"\badjacent\b",
    r"\bidentical\b", r"same bug class", r"other call site", r"three call sites",
]

# Would the correct fix supersede this patch? Strongest not-merge signal.
SUPERSEDE = [
    r"structural fix belongs",
    r"would (?:subsume|supersede|render unnecessary)",
    r"centraliz\w+ the check",
    r"locking design is wrong",
    r"instead of removing the inversion",
    r"does not remove the root cause",
]


def matches(text, pats):
    return [p for p in pats if re.search(p, text, re.I)]


def verdict(conn, review_id, fail_severity=3):
    """Reproduce src/patchwork.rs PatchworkVerdict::from_findings."""
    rows = conn.execute(
        "SELECT severity, COALESCE(preexisting,0) FROM findings WHERE review_id=?",
        (review_id,),
    ).fetchall()
    new = Counter()
    pre = Counter()
    for sev, p in rows:
        (pre if p else new)[sev] += 1
    total_new = sum(new.values())
    if total_new == 0:
        state = "success"
    else:
        state = "fail" if any(s >= fail_severity and c for s, c in new.items()) else "warning"
    return state, dict(new), dict(pre)


def main():
    conn = sqlite3.connect(DB, uri=True)

    print("=" * 78)
    print("1. Posted Patchwork state for the three flagged reviews")
    print("=" * 78)
    flagged = {
        8400: "pds_core: fix cmd_regs access racing BAR unmap on reset",
        8583: "vsock: use sock_error() to consume sk_err",
        8709: "netdevsim: fix deadlock in nsim_bus_dev_max_vfs_write()",
    }
    for rid, subj in flagged.items():
        state, new, pre = verdict(conn, rid)
        nice_new = ", ".join(f"{SEV[s]}={c}" for s, c in sorted(new.items(), reverse=True)) or "none"
        nice_pre = ", ".join(f"{SEV[s]}={c}" for s, c in sorted(pre.items(), reverse=True)) or "none"
        print(f"\nreview {rid}  {subj}")
        print(f"  posted state : {state.upper()}")
        print(f"  new          : {nice_new}")
        print(f"  preexisting  : {nice_pre}   <- excluded from state")

    print()
    print("=" * 78)
    print("2. Population: preexisting=1, severity >= High")
    print("=" * 78)
    rows = conn.execute(
        """SELECT f.id, f.severity, f.problem, f.severity_explanation, r.prompts_hash
           FROM findings f JOIN reviews r ON r.id = f.review_id
           WHERE f.preexisting = 1 AND f.severity >= 3"""
    ).fetchall()

    tally = Counter()
    era = Counter()
    purpose_ids = []
    for fid, sev, problem, expl, phash in rows:
        text = f"{problem or ''}\n{expl or ''}"
        p = bool(matches(text, PURPOSE_DEFEATED))
        s = bool(matches(text, SIBLING))
        u = bool(matches(text, SUPERSEDE))
        tally["total"] += 1
        tally["purpose_defeated"] += p
        tally["sibling_only"] += (s and not p)
        tally["supersede"] += u
        tally["either"] += (p or s or u)
        if p:
            purpose_ids.append(fid)
            era[(phash or "NULL")[:10]] += 1

    print(f"\n  total preexisting=1 High+Critical : {tally['total']}")
    print(f"  purpose-defeated (narrow test)   : {tally['purpose_defeated']}"
          f"  ({100*tally['purpose_defeated']/max(tally['total'],1):.0f}%)")
    print(f"  sibling language but NOT purpose  : {tally['sibling_only']}")
    print(f"  correct-fix-supersedes            : {tally['supersede']}")
    print(f"  any of the three                  : {tally['either']}")

    print("\n  purpose-defeated split by prompts_hash era"
          " (per [[prompt-era-splits-via-prompts-hash]]):")
    for h, c in era.most_common():
        print(f"    {h:12s} {c}")

    print("\n  Are the three flagged findings caught by the narrow test?")
    for fid in (9207, 9208, 9209, 9731, 10124):
        row = conn.execute(
            "SELECT problem, severity_explanation FROM findings WHERE id=?", (fid,)
        ).fetchone()
        if not row:
            print(f"    {fid}: not found")
            continue
        text = f"{row[0] or ''}\n{row[1] or ''}"
        hits = matches(text, PURPOSE_DEFEATED)
        mark = "YES" if hits else "no "
        print(f"    {fid}: {mark}  {hits[:3]}")

    print()
    print("=" * 78)
    print("3. How many of those purpose-defeated findings posted a clean state?")
    print("=" * 78)
    clean = 0
    checked = 0
    for fid in purpose_ids:
        rid = conn.execute("SELECT review_id FROM findings WHERE id=?", (fid,)).fetchone()[0]
        state, _, _ = verdict(conn, rid)
        checked += 1
        if state in ("success", "warning"):
            clean += 1
    print(f"\n  {clean}/{checked} sit in reviews that posted success or warning,"
          f" i.e. no fail signal reached the author.")


if __name__ == "__main__":
    sys.exit(main())
