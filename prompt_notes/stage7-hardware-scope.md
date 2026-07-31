# Stage 7 (hardware review) — duplication scope analysis

Status: **applied** 2026-07-31 to the stage 7 arm of `get_stage_prompt` in
`src/worker/prompts.rs` (SCOPE TEST + HARDWARE RESIDUE added, rings/queues clause reworded;
persona sentence and enumerated capabilities unchanged). Analysed 2026-07-31 against
`findings.source_stages`.

**The validating experiment has not been run.** The edit was adopted on the strength of a proxy
A/B (see Validation), not a pipeline comparison. The test that would settle it — both prompts
over the same patch set, diffing stage 7's emitted concerns — is still outstanding. Until then
treat the edit as a considered bet, and watch stage 7's solo rate and the multi-stage share in
`analyze_stage_*` for regression.

> **Read this as a point-in-time analysis, not ground truth.** Every number below is one
> snapshot of a moving system, and the central judgment — "is this defect really stage 7's?" —
> is an LLM opinion about another LLM's output, not a measurement. Nothing here was validated
> against a live pipeline run. The Scope and limitations section states what would have to be
> true for the conclusions to hold; read it before acting on any figure.

## Scope of the analysis

**What was examined.** Two populations, both from `sashiko.db`, `findings.source_stages`:

| Set | n | Definition |
|---|---|---|
| Sole-source | 276 | `json_array_length(source_stages)=1` containing 7, severity ≥ medium, **all time**, non-cross-review, spanning 223 reviews |
| Duplicates | 129 | `json_array_length(source_stages)>1` containing 7, reviews from the **last 48h only**, spanning 83 reviews |

Every finding in both sets was classified individually by a subagent reading its full
`problem` + `severity_explanation` + `locations` text. No sampling, no truncation.

**What was NOT examined:**

- **Only the findings that survived.** `findings` holds stage 10's accepted output. Concerns
  stage 7 raised that stages 8–10 merged away or rejected were never looked at, so this
  measures stage 7's *surviving* duplication, not what it actually emits.
- **No per-stage conversation dumps.** Attribution of a duplicate to a "trigger phrase" is a
  classifier's plausible reconstruction of why stage 7 raised it — not evidence from stage 7's
  own reasoning trace, which is where the real answer lives.
- **Asymmetric time windows.** Duplicates are 48h; sole-source is all-time. The two tables
  therefore describe different periods and different prompt revisions, and the misfire-rate and
  dependency columns in Finding 2 are **not** strictly commensurable.
- **Single model, single window.** Every review in the 48h duplicate window ran on
  `us.anthropic.claude-opus-5`. Nothing here is known to generalise to another model.
- **No causal test.** The proposed edit was never run through the real pipeline on real
  patches. The validation below is a proxy, with a known bias described in its caveat.
- **`source_stages` is taken at face value.** It is stage 8's account of which stages it merged.
  Stage 8 mis-merges are invisible here and would corrupt both populations — and the
  near-duplicates noted under Open questions are direct evidence that stage 8 does miss repeats.

## Scope and limitations

Ranked by how much each would change the conclusions:

1. **The core verdicts are LLM judgments, and only half were confident.** Of the 72 MISPLACED
   verdicts, **33 were high-confidence and 39 medium**; every DUAL verdict was medium. Treat
   "56% misplaced" as roughly 46%–56% depending on how the mediums break — the direction is
   solid, the precision is not. No verdict was reviewed by a second classifier or a human, so
   correlated classifier bias would not have been caught.
2. **The validation flatters the current prompt and cannot predict production.** Judges saw
   one-line distilled essences, which strips exactly the dense hardware vocabulary that lures
   stage 7 in production. The current prompt scores 92% suppression there against 44% on the
   real population. Directional comparison only.
3. **48h is a small, possibly unrepresentative window.** 129 findings over 83 reviews, and
   stage 7's solo rate is known to be drifting (30% all-time → 20% over 13 days), so this
   window may not resemble next week's.
4. **Trigger-phrase attribution is the weakest link.** Several prompt clauses sit in one
   sentence; asking which fragment "lured" a finding invites over-attribution to whichever
   phrase reads most quotably. The 100%-misfire figures rest on small n (10 and 8).
5. **The severity floor differs between the two sets.** Sole-source is severity ≥ medium
   (2 critical / 57 high / 217 medium); duplicates include low. Cross-set comparisons of
   value-vs-cost are skewed by that.
6. **`would_survive_narrowing` is a counterfactual.** "Would stage 7 still have found this
   under a tighter prompt?" was answered by a classifier imagining a prompt it never ran. The
   18.5% collateral figure is an estimate of an unobservable.

**How to re-derive this.** Working data (per-finding classifications, holdout key, judge
outputs) was left in `/tmp/stage7_dup_analysis/` and is **not durable** — expect it gone. The
SQL shapes are in this note's tables and in `src/db.rs` (`json_each(f.source_stages)` pattern,
~line 2927); re-running against a fresh window is cheap and preferable to trusting these
numbers if more than a few weeks have passed.

## Finding 1: ~56% of stage 7's duplicates look out of scope

Of 129 shared findings: **72 MISPLACED** (general software defect that merely lives in a driver
file), 42 genuinely hardware-owned, 15 dual. Misplacement is uniform across severity
(high 18/29, medium 42/78), so it is not low-severity noise. True owners: stage 2 ×23,
stage 3 ×22, stage 1 ×9, stage 5 ×8, stage 6 ×5, stage 4 ×5.

Confidence: 33 of the 72 MISPLACED verdicts were high-confidence, 39 medium. The headline is
better read as "roughly half, probably more" than as 55.8%.

## Finding 2: the clauses that misfire are also the clauses that pay

The two columns come from **different windows** (misfire = 48h duplicates; dependency =
all-time sole-source) and are indicative, not commensurable. Attribution is a classifier's
reconstruction, not stage 7's own reasoning — see limitation 4.

| Clause | Misfire rate on dups | Sole-source findings depending on it |
|---|---|---|
| `You are a hardware engineer reviewing device driver changes` | 100% (10/10) | **54** |
| `If this patch touches driver or hardware-specific code` | 100% (8/8) | — |
| `hardware rings/queues are actually initialized ... unconditionally accessed` | 86% (18/21) | 9 |
| `Ensure the hardware state machine is handled correctly` | 60% (12/20) | 59 |
| `rigorously review register accesses` | 58% (22/38) | **71** |
| `IRQ handling` / `DMA mapping/unmapping` | 44% / 33% | 14 / 7 |

This is the crux, and it is the one conclusion here robust to the caveats: the persona sentence
is the worst offender *and* the second-largest source of stage 7's unique value. Deleting the
duplicate-attracting clauses would cost more than it saved. The problem is the **entry test**,
not the capability list. (The two 100% rows rest on n=10 and n=8 — small.)

## Finding 3: a blunt narrowing would cost an estimated 18.5%

Restricting stage 7 to "hardware root cause only" is estimated to lose **51 of 276 sole-source
findings, 11 of them High**. Consistent shape: a *software* mistake whose *consequence* is a
device left in a state the driver no longer describes — clocks at mismatched rates after a
failed step, a dropped register read treated as valid, enable with no matching disable,
software/hardware bookkeeping divergence.

This figure is a counterfactual judgment about a prompt that was never run (limitation 6), so
treat it as "order of magnitude: high enough to rule the blunt edit out", not as a rate.

## The discriminator

Not "hardware vs software root cause" — that is what costs 18.5%. It is:

> Does stating this defect require a fact about the device, bus, or firmware contract?

MISPLACED: strip the hardware nouns and the same bug remains, statable from the C alone — the
vocabulary is decoration. Keepers: the hardware fact is load-bearing in the argument.

## Proposed edit (stage 7 arm of `get_stage_prompt`)

Keep the persona sentence and every enumerated capability **verbatim**. Three changes:

1. **Reword the rings/queues clause** (86% misfire, 9 dependents) — it currently reads as a
   licence to report any unvalidated array index:
   → `and that hardware rings/queues are in the hardware state the code assumes before it
   programs or advances them.`

2. **Add a SCOPE TEST paragraph**: a concern is yours only if stating it requires a device/bus/
   firmware fact. Strip the hardware nouns — restate in plain C — is it still the same defect?
   If yes it is another stage's. Name the excluded substances explicitly (missing NULL check,
   unchecked return, error-path unwind, refcount/lifetime, bounds/overflow, lock/race,
   commit-message mismatch) and state that *being located in a driver file does not make a
   defect yours*.

3. **Add a HARDWARE RESIDUE carve-out** naming the four keeper shapes (un-rolled-back partial
   hardware sequence; failed register access treated as successful; enable without matching
   disable; software/hardware bookkeeping divergence), told to describe the defect through the
   hardware state left behind rather than as generic error handling. Close with a
   "do not drop a real defect" sentence, per the `STAGE3_GUIDE_SCOPE_OVERRIDE` precedent
   already in `prompts.rs`.

## Validation (a proxy, not a pipeline run)

Blind A/B on 60 stratified, shuffled held-out cases (18 known-misplaced, 42 known-should-report)
drawn from the same classified populations — so the "ground truth" it scores against is itself
the LLM labelling described in limitation 1, and any bias in that labelling is baked into both
rows. Judges saw only the rules and a one-line defect description, never ground truth or which
variant they held. Two runs per variant, one in reverse case order; inter-run agreement 95%
(current) and 98% (proposed).

| | Duplicates suppressed | Sole-source value retained |
|---|---|---|
| Current | 92% | 86% |
| Proposed | **100%** | **92%** |

Misplaced-duplicate report rate 8% → 0%; at-risk uniques 57% → 75%; genuine hardware
duplicates and safe uniques unchanged at 100%. The proposal dominates on both axes.

**What this does and does not establish.** It shows that under identical, simplified conditions
the proposed rules classify scope better than the current ones. It does **not** predict
production behaviour: the holdout uses distilled one-line essences, which strips the dense
hardware vocabulary that lures stage 7 in real reviews. That flatters the current prompt badly —
92% here against 44% on the real population. Absolute numbers from this table should not be
quoted as expected outcomes. Four cases the proposal still skips are borderline residue: PTP
`*stamp` never written; an unlocked mailbox RMW; an ethtool-mutable queue count; a raw PAPR
hcall code returned as errno.

**The missing experiment.** Nothing short of running both prompts over the same patch set and
diffing stage 7's emitted concerns will settle this. That is the test to run before trusting the
edit, and it would also expose the pre-stage-8 concerns this analysis never saw.

## Open questions

- Stage 7 is **not** the worst duplicator: stage 6 shares 80.4% of its findings, and the 1↔2
  pair is far tighter (2,364 shared, 32.4% Jaccard) than anything involving stage 7.
- Whether stage 7's corroboration on shared findings has independent value is unresolved. Its
  shared findings skew much more severe than its solo ones (27 critical / 270 high shared vs
  2 / 57 solo, all time). `source_stages` cannot separate "reached it independently" from
  "free-rode"; per-stage conversation dumps could.
- Stage 7's solo rate fell from 30% (all time) to 20% (last 13 days) — duplication is
  increasing, cause unexamined.
- Several near-duplicate pairs appear *within* the sole-source set (e.g. 1722/2905, 3382/3545/
  5900, 584/1605), suggesting stage 8 misses some cross-patch repeats. Separate issue — but it
  also means both populations here are slightly inflated by repeats.

## If you are reading this later

Assume it is stale until checked. The prompt may have changed, the model has probably changed,
and the 48h duplicate window is long gone. What is most likely to still hold is the *structural*
claim — that stage 7's highest-value and highest-misfire prompt clauses are the same clauses, so
the fix belongs in the entry test rather than the capability list. What is least likely to hold
is any specific percentage. The finding ids cited throughout are stable database keys and can be
re-read directly if you want to re-judge any verdict yourself.

Not a decision record and not a measurement: one analyst's point-in-time read, offered as a
starting point for whoever changes this prompt next.
