# Prefetch context reduction — review cost analysis

Status: **measured, not conclusive on turns.** Analysis of `24fe7f4` ("prefetch: omit redundant
additive function context", 2026-07-31 14:55 PDT) against reviews through 2026-08-04.

> **Read this as a point-in-time analysis.** The controlled part (prefetch bytes) is a direct
> measurement and should reproduce exactly. The behavioural part (turns, tool calls, cost) is
> observational, taken from a live queue whose patch mix changed across the boundary, and only
> the cost effect clears a significance test. Read *Attribution problem* and *Limitations*
> before quoting any figure.

## What the change does

Omits a modified function's post-patch definition from `<pre_fetched_context>` when a unified
diff already shows that function completely and removes nothing inside it. Partial, subtractive,
ambiguous, and multi-revision cases keep their context. Types and callees referenced by an
omitted function are still looked up.

## Attribution problem

`reviews.prompts_hash` is the repo HEAD at review start, so it identifies a code state, not a
change. Four other commits landed in the same hash window (`3433d11`), and three of them rewrite
discovery-stage prompts:

| Commit | Time (PDT) | Touches |
|---|---|---|
| `38cf4c2` | 14:03 | stage 1-3 prompt rescope |
| `297ae2c` | 14:03 | dismissed-concern guidance |
| `aa08a67` | 14:03 | stage 7 scope gate |
| `24fe7f4` | **14:55** | **prefetch context (this analysis)** |
| `3433d11` | 15:18 | stage 8 dedup response shape (merge stage, not discovery) |

A before/after split on `prompts_hash` therefore measures all five at once. Two things separate
them, and both are used below.

**1. Direct replay (isolates prefetch exactly).** `dump_prefetch` was built at `24fe7f4~1` and at
HEAD, and both were run over the same 1005 commits taken from the conversation dumps, each
against the baseline its review used. Same input, same tree, only the prefetch code differs.

**2. The `old-prompts` control (isolates the prompt rewrites).** `c9810ec` added an
`alternative_prompts` variant carrying the *pre*-rewrite stage 1/2/7 text. It runs inside the same
review as `main`, on the same prepared context, so the `main` vs `old-prompts` gap is the prompt
rewrites with prefetch held fixed. On 40 paired reviews the new prompts *reduce* turns (median
paired diff −1.5 turns, 26 down vs 13 up) and calls (−1.5, 24 down vs 14 up). The prompt rewrites
are therefore not inflating the turn counts reported below; if anything they mask the prefetch
effect's sign.

## Result 1 — context size (controlled, n=1005)

`scripts/ab_prefetch_context.py`. The change fires on **40.4%** of patches; on those it cuts the
payload 12.9%. Fleet-wide the prepared context is **8.4% smaller** for the same patches.

Prefetch bytes per patch byte, and the dose by patch size:

| patch size | n | fires | old ratio | new ratio | prefetch bytes |
|---|---|---|---|---|---|
| 0-2 KB | 482 | 15% | 6.37 | 6.35 | −0.5% |
| 2-4 KB | 201 | 52% | 3.81 | 3.77 | −1.3% |
| 4-8 KB | 171 | 58% | 2.91 | 2.68 | −6.6% |
| 8-16 KB | 69 | 71% | 2.12 | 1.57 | −14.8% |
| ≥16 KB | 82 | 99% | 1.73 | 1.40 | −18.2% |

The gradient is the mechanism, not an artifact: a small patch is usually a partial hunk inside a
large function, which the change deliberately keeps, while a large patch more often adds whole
functions, which is exactly the omittable case. 53 of 406 affected commits got *larger* (median
+3.0%), from the referenced-type and callee lookups that now run for omitted functions.

Note the ratio itself is a poor headline metric: it is dominated by patch size (6.4x at 1 KB vs
1.7x at 16 KB), so an unstratified before/after ratio moved −0.6% only because the after-era
patch mix was 16.5% smaller. Stratify or the confound swallows the effect.

## Result 2 — turns, tool calls, cost (observational)

`scripts/analyze_context_efficiency.py`, 1101 dumped reviews since 2026-07-26, `main` source,
discovery stages 1-7 only, **stage count pinned at 7** (the after-era ran slightly fewer stages,
which alone would move totals):

| patch size | A/B dose | turns | calls | $/review | n (b/a) |
|---|---|---|---|---|---|
| 0-2 KB | −0.5% | 27.5 → 28.5 (+4%) | 30 → 32 (+7%) | 3.10 → 3.56 (+15%) | 24/14 |
| 2-4 KB | −1.3% | 27.5 → 28.0 (+2%) | 31.5 → 31.0 (−2%) | 3.74 → 3.74 (−0%) | 92/52 |
| 4-8 KB | −6.6% | 30.0 → 29.5 (−2%) | 36 → 35.5 (−1%) | 5.28 → 4.90 (−7%) | 109/52 |
| 8-16 KB | −14.8% | 30.5 → 27.0 (−11%) | 35 → 33 (−6%) | 6.59 → 5.32 (−19%) | 70/29 |
| ≥16 KB | −18.2% | 33.0 → 30.0 (−9%) | 42 → 34.5 (−18%) | 8.60 → 8.26 (−4%) | 74/28 |

Aggregated at the ≥4 KB threshold where the change actually fires (58-99% of patches):

| metric | before | after | change | permutation p |
|---|---|---|---|---|
| turns | 31.00 | 29.00 | −6.5% | 0.105 |
| tool calls | 37.00 | 34.00 | −8.1% | 0.110 |
| cost/review | $6.65 | $5.81 | −12.6% | **0.040** |

Below 4 KB, where the change is nearly inert, nothing moves (turns +1.8% p=1.0, calls 0% p=1.0).
That the response tracks the dose across five bands — and vanishes in the band where the dose
vanishes — is the strongest evidence here that the prefetch change is what moved cost. But on
turns alone, taken as a single before/after comparison, **the effect is not statistically
significant** and should not be reported as a turn reduction.

## Result 3 — the model does not read the omitted code back

The failure mode worth ruling out: if omitting a function makes stages re-read it with a tool, the
review pays twice (output tokens for the call, non-cacheable input for the result).
`scripts/analyze_context_readback.py` splits discovery tool traffic by whether it targets a file
the patch modifies — the population the change made eligible for omission — against everything
else, which the change never touched and which acts as a within-review control:

| metric | before | after | change |
|---|---|---|---|
| tool calls on modified files | 18.0 | 17.0 | −5.6% |
| tool calls elsewhere (control) | 3.0 | 5.0 | +66.7% |
| result KB from modified files | 77.1 | 72.3 | −6.3% |
| result KB elsewhere (control) | 5.9 | 12.3 | +109.5% |

Traffic to the omitted population went *down*. The fleet-wide rise in tool calls is entirely in
files the patch does not touch, which the prefetch change cannot have caused and which the
stage 1-3 rescope (explicitly redirecting stages toward cross-artifact and callchain work) can.
This is the cleanest signal in the analysis and it is unambiguous: no read-back penalty.

## Verdict

The change did what it was designed to do. Context is 8.4% smaller fleet-wide and up to 18% on
large patches, with no read-back penalty, and cost on the patches it affects is down ~13%
(p=0.04). The turn reduction is directionally consistent and dose-tracking but individually
underpowered at n=109 after-era reviews; another week of data at this `prompts_hash` would settle
it.

Where it did *not* help: patches under 4 KB — 68% of distinct commits replayed, 50% of dumped
reviews — where the change fires on only 15-52% of cases. If further context reduction is wanted, that is where the
remaining headroom is — those patches carry a 6.4x prefetch-to-patch ratio, the worst in the
fleet, because a small hunk inside a big function pulls the whole function in. That case is
correctly excluded by this change's contract, so a separate mechanism would be needed.

## Limitations and what was not examined

- **Turn effect is not significant** (p≈0.11). Only the cost effect clears p<0.05, and cost is
  partly a token-mix measure that moves with output length, not just turns.
- **Cost is reconstructed, not billed.** Per-review dollars are computed from dump
  `usage` at opus-5 list rates (cache read 0.1x, write 1.25x), treating
  `prompt_tokens − cache_write_tokens` as cache reads. `scripts/analyze_stage_cost.py` is the
  authority for absolute cost; used here only for before/after ratios, where the approximation
  cancels.
- **The two prompt-era confounds are separated by argument, not by a randomized run.** The
  `old-prompts` control has only 40 paired reviews and, per `c9810ec`, reverts stage 1/2/7 text
  only — not `297ae2c`'s dismissed-concern guidance. The prompt rewrites' own effect on turns is
  therefore bounded, not eliminated.
- **Findings quality was not measured at all.** This analysis is entirely about cost. Whether
  omitting context changed what the pipeline *finds* — precision, recall, severity mix,
  sole-source yield — is untested, and is the question that actually decides whether the change
  should stay. `findings` holds only what survived stage 8-10 merging, so pre-merge concern
  counts per stage would need the dumps.
- **Not examined:** stage 8-11 merge cost (the stage-8 commit in the same window changes response
  shape, so merge-side numbers are attributable to it, not to prefetch); the 53 commits whose
  payload grew; cross-review and confirmation paths, which bypass `SessionRunner` and are not
  dumped; non-opus sources beyond the counts above; and latency.

## Reproducing

```sh
# controlled: same commits through both prefetch implementations
git worktree add /tmp/sashiko-pre 24fe7f4~1
(cd /tmp/sashiko-pre && cargo build --release --bin dump_prefetch)
cargo build --release --bin dump_prefetch
scripts/ab_prefetch_context.py --old /tmp/sashiko-pre --new . --min-day 20260726

# observational: context size, turns, tool bytes per review
scripts/analyze_context_efficiency.py --split 3433d11 --min-day 20260726 \
    --sources main,old-prompts --rows /tmp/rows.jsonl

# read-back check
scripts/analyze_context_readback.py --split 3433d11 --min-day 20260726
```

`ab_prefetch_context.py` needs one ~1.5 G kernel worktree per job; keep `--scratch` off tmpfs.
