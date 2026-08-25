# Stage 10: `preexisting` is over-claimed for incomplete and superseded fixes

**Status:** **applied** (branch `haystack`). The shipped wording is narrower than
what this note originally proposed — see "What shipped and why it narrowed".
Sites touched, all in `src/worker/prompts.rs`:

| Site | Line | Change |
|---|---|---|
| Shared concern schema (stages 1–7) | `:2162` | ownership exception appended to the `preexisting` bullet |
| Stage 10 output spec | `:1767` | same exception, inline in the `preexisting` key |
| Stage 10 clean output spec | `:1773` | same, kept in sync |
| Stage 10 rule 6 | `:607` | rewritten: ownership decided before discarding |
| Stage 10 instruction tail | `:610-614` | new "Severity of a fix that misses its target" override |

`third_party/prompts/kernel/severity.md` was **not** edited; the severity change
is a Rust-side override appended to the stage 10 prompt instead.

**Whether the validating experiment ran:** **No.** No pipeline A/B was run,
before or after applying. The population counts below are SQL over already-stored
findings, so the *sizes* are real measurements. The judgment that a given finding
is mislabelled is an LLM opinion (mine) about another LLM's output, and **the
effect of the shipped wording on any label is not measured at all.**

> **This is a point-in-time analysis, not ground truth.** The core question —
> "does this defect belong to the patch or to the tree?" — is a judgment call,
> and I am making it by reading finding text rather than by re-reviewing the
> patches. Seven findings were confirmed against the reviews by the user
> (10267, 10563, then 9207, 9208, 9209, 9731, 10124); the rest of the population
> is inferred from text patterns.

## The defect

Reviews flagged by the user, in two batches. All five patchsets in the second
batch are single-patch **fixes**, which is the case the rule now keys on:

| Finding | Review | Msg-id | Sev | `preexisting` | Posted state |
|---|---|---|---|---|---|
| 10563 | — | `e93d0d2f...zhilinz@nebusec.ai` | High | **1** | — |
| 10267 | — | `20260731162938.3388534-1-nicoyip.dev@gmail.com` | High | **1** | — |
| 9731 | 8583 | `20260730081843.287563-1-phind.uet@gmail.com` | High | **1** | **SUCCESS** |
| 10124 | 8709 | `b7bf56ea-7522-4163-acd5-aaa69ad03b3a@mail.kernel.org` | High | **1** | WARNING |
| 9207, 9208, 9209 | 8400 | `20260729055258.1416225-1-nikhil.rao@amd.com` | High ×3 | **1** | WARNING |

The vsock case (8583) is the clean demonstration: the High headlined *"Incomplete
fix"* was the review's **only** finding, so `total_new == 0` and the author was
told the review found no regressions.

Each finding is *correct on the merits* and each contains, in its own text, the
argument that it belongs to the patch:

- 9731: "*the patch is advertised as ensuring the error 'cannot affect subsequent
  operations on the same socket', which the implementation does not achieve. A
  structural fix belongs in `vsock_listen()` ... and/or `vsock_accept()`*"
- 10124: "*the patch is presented as the fix for exactly this deadlock class and
  leaves an equivalent path open ... Converting a single writer to
  `devl_trylock()` therefore does not remove the root cause*"
- 9207: "*the patch's stated purpose is to make this loop safe against a
  concurrent reset and it leaves the completed-reset case undetected*"
- 9208: "*the patch introduces the invariant that these two sites violate and
  hardens only one of three call sites*"
- 10563: "*the patch defines the intended cb contract for the peer branch while
  deliberately (and silently) leaving this path as-is, so the fix and its
  changelog are incomplete for the very threat model they describe*"
- 10267: "*the patch touches precisely this locking and fixes only the
  reclaim/UAF half, so the residual write-side race should be addressed here*"

So the model already reached the right conclusion in prose and then set the
boolean the other way. This is not a detection failure — it is a labelling
failure, and the label is the thing with teeth.

### 9209 is the boundary case, and the shipped rule leaves it `true`

Review 8400's third High concerns `info_regs`, and that patch's changelog
*explicitly* defers it: "*pdsc_unmap_bars() also clears info_regs ... that
teardown race is pre-existing and handled separately*". Under the shipped rule
the defect is not in "the bug class its commit message claims to close", so
`preexisting: true` stands — correctly, in my reading. Its residual complaint
(devcmd_lock is the wrong lock for a function invalidating four pointers) is a
design critique that the rule deliberately does not convert into ownership.

Worth stating plainly: the user grouped this msgid with the others, but the
mislabelled findings in that review are 9207 and 9208, not 9209.

### Severity was a second, independent failure

Review 8709's finding **10125** is `preexisting: 0` and says the trylock
approach violates the netdev guideline "*using mutex_trylock() ... to avoid a
lock ordering issue is a sign that the locking design is wrong*" — the clearest
"must be rewritten" signal in the set. It landed at **Medium**, because the
rubric scores the consequence of the *bug* and never asks whether the *fix*
works. Relabelling alone would not have fixed that; hence the severity override.

## Why the label matters

`PatchworkVerdict` computes state from **new findings only**
(`src/patchwork.rs:80`):

```rust
let state = if total_new == 0 { "success" } else { ... };
```

`total_preexisting` feeds the *description* string but never the state, and
`fail_severity` defaults to `High` (`src/email_policy.rs:28`). A High stamped
`preexisting: true` is therefore worth exactly nothing to the verdict.
`sashiko-cli` mirrors this: `src/bin/sashiko-cli.rs:605-609` filters
`preexisting` out of the headline issue set, and `:1840-1852` counts it
separately.

## Why the model labelled it this way

The definition was a pure origin test, with no notion of ownership:

> `"preexisting"`: A boolean value: `true` if this bug/vulnerability **already
> existed in the codebase before these patches were applied**, or `false` if the
> issue was newly introduced by the reviewed patchset.

Read literally, every one of these findings *is* `true`: the buggy bytes predate
the patch. Nothing distinguished "this bug is in a part of the tree the patch
never claimed to touch" from "this bug is the other half of the exact thing the
patch claims to fix". Old stage 10 rule 6 then reinforced the origin framing and
attached a discard rule to it:

> 6. If the problem did exist in the code before the patch was applied, say it
>    explicitly: 'This problem wasn't introduced by this patch, but...'. Discard
>    low- and medium-severity pre-existing problems, report only high- and
>    critical severity issues.

### Stage 10 never saw the shared schema

Found while applying, and it changed the placement: the `preexisting` bullet
lives in `format_guidance`, which is built inside `execute_stage()`, and that
runs only for **discovery stages 1–7** (`src/worker/prompts.rs:1207`, `for
stage_num in 1..=7`). Stages 8–11 take a different path. Stage 10 — the stage
that sets the final label and applies the discard — only ever saw its own
restatement of the bare origin test at `:1767`/`:1773`.

So editing the shared schema alone would have left the stage with teeth
unchanged, and rule 6 could not cross-reference the schema. Hence four
insertions rather than two, and a deliberately self-contained rule 6. Both
output specs had to move together: `clean_user_prompt` is what feeds
`prompts_hash`, so letting the pair drift would make the era hash describe a
prompt that was never sent.

## Population size

All findings, severity >= High (n=2801 across all `preexisting` values):

| `preexisting` | High | Critical | Total |
|---|---|---|---|
| NULL (pre-flag era) | 1230 | 210 | 1440 |
| 0 | 624 | 52 | 676 |
| 1 | **657** | **28** | **685** |

Within the 685 `preexisting=1` High+Critical findings, by the keyword predicates
in `scripts/analyze_preexisting_purpose.py`:

- **143 (21%)** match the narrow *purpose-defeated* test — the text argues the
  patch does not achieve what its changelog claims.
- **125 of those 143** sit in reviews that posted `success` or `warning`, i.e.
  **no fail signal reached the author**.
- **235** use sibling/peer/counterpart language but do *not* match the narrow
  test — this is the gap between the originally-proposed rule and the shipped one.
- **80** are headlined "Incomplete fix".

The predicates are keyword heuristics, not verdicts, so 143/685 is an upper bound
on the mislabelling rate, not an estimate of it. The honest claim: **the pattern
is systematic rather than anecdotal, and its scale is order-100 findings.**

The 143 spread across nine `prompts_hash` eras (largest: `8b3064ea49` 52,
`3433d11662` 40, `22346a4566` 16), so this is stable behaviour rather than one
prompt revision's quirk. The three flagged reviews sit on `74a779e940`
(pds_core) and `d5ba12ed00` (vsock, netdevsim).

Two were already known. `STAGE3_GUIDE_SCOPE_OVERRIDE`'s doc comment
(`src/worker/prompts.rs:158-169`) cites findings 7881 and 9863 as stage-3-only
"Incomplete fix" Highs, and 9863 is in this mislabelled set — so the override
preserved the *detection* while the `preexisting` flag silently discarded the
*verdict*.

## What shipped and why it narrowed

Design principle, unchanged from the original proposal: **do not touch the origin
question.** `preexisting` keeps meaning "did these bytes predate the patch", and
ownership is an explicit exception that overrides it, so the model is never asked
to lie about history.

What *did* change is the discriminator. The original proposal keyed on "a sibling
location the patch could have covered in the same edit" (its rule (a)). That is a
maintainer judgment, and net-next convention often *prefers* narrow fixes — it
was the note's own limitation #5, and it would have produced "you should have
fixed more" noise on legitimately-scoped patches. The 235 sibling-language
findings that do not match the narrow test are the volume that rule would have
swept in.

The shipped rule instead keys on **the bug class the patch's own commit message
claims to close**, which is checkable against the changelog rather than against a
reviewer's sense of proper scope. Rules (b) supersession and (c) contract
survive, compressed into "the correct fix would replace rather than extend its
approach" and "defects its commit message ... relies on".

### 1. Shared concern schema (`:2162`) and both stage 10 output specs (`:1767`, `:1773`)

```
Exception: a fix owns its own completeness, even over code predating it. Set
false if the bug class its commit message claims to close stays reachable, or if
the correct fix would replace rather than extend its approach. Reserve true for
defects its commit message neither claims to fix nor relies on.
```

### 2. Stage 10 rule 6 (`:607`)

```
6. A fix owns its own completeness, even over code predating it: if the bug class
its commit message claims to close stays reachable, or the correct fix would
replace rather than extend its approach, set "preexisting": false and name the
gap against that claim. Only otherwise say 'This problem wasn't introduced by
this patch, but...' and discard low- and medium-severity pre-existing problems,
reporting only high and critical.
```

### 3. Severity override, appended to the stage 10 prompt (`:610-614`)

```
# Severity of a fix that misses its target

Judge a fix also on whether it achieves its stated purpose. Rate High when the
bug class its commit message claims to close stays reachable, or when the correct
fix would replace rather than extend its approach: the patch cannot be merged as
written and the author must respin, so the consequence to state is a release
cycle spent without removing the bug.
```

### Not done

Changing `src/patchwork.rs` to fail on preexisting Highs. That would convert
every genuinely-unowned pre-existing High into a build failure for an unrelated
author — the opposite complaint. The fix belongs at the labelling boundary, which
is where it landed.

## A tension worth watching

Stage 3 now receives two instructions that pull against each other. Its scope
override (`:170`) says: "*Never report a concern about ... an inaccurate
commit-message claim*" and "*Describe it mechanically — the code path and the
resulting failure — rather than as a shortfall against the commit message*". The
new schema exception, which stage 3 also sees, asks it to compare the defect
against what the commit message claims to close.

These are reconcilable — the override governs how to *describe* a defect, the
exception governs how to *label* its ownership — but the wording is close enough
to confuse a reader, and the interaction is untested. If stage 3's contribution
to the 143 drops rather than rises, this is the first place to look.

## Verification performed

`cargo build` clean, `cargo fmt --check` clean, **611 tests pass / 0 fail**.
Rule 6's `\"preexisting\": false` escaping renders as `"preexisting": false` —
the stage 10 string is a regular (non-raw) literal, matching the existing
`\"mm: fix allocation\"` in rule 4.

**This verifies the prompt text is well-formed, not that it works.** No review
has run against it.

## Scope of the analysis

Examined: all rows in `findings` with `severity >= 3` (n=2801 across all
`preexisting` values; n=685 for `preexisting=1`). Findings 9207, 9208, 9209,
9210, 9731, 10124, 10125, 10126, 10267, 10563 read in full. Consumers traced:
`src/patchwork.rs`, `src/bin/sashiko-cli.rs`, `src/email_policy.rs`. Definition
and prompt-assembly sites traced: `src/worker/prompts.rs:607`, `:617`, `:1207`,
`:1767`, `:1773`, `:2162`, plus stage 8/9 preservation rules at `:223`, `:580`,
`:595`, `:1552`, `:1589`.

**What was NOT examined:**
- **No A/B run, before or after applying.** Zero evidence the shipped wording
  changes any label. This remains the single largest gap. The `prompts_hash`
  change from this commit is the clean split point for measuring it.
- **Upstream ground truth.** `patchwork_patch_state` has no rows for patchsets
  3780, 3856 or 3916, so what maintainers actually did with these three patches
  is unknown. The "strong signal not to merge" premise is the user's judgment
  and mine, not an observed outcome.
- **False-positive direction.** I did not sample the 676 `preexisting=0`
  findings to see whether the flag is *also* under-claimed, so I cannot say
  whether the net effect is more accurate labels or just more `false`.
- **The low/medium discard interaction.** Old rule 6 dropped low/medium
  preexisting findings; narrowing "preexisting" un-drops some. Direction known,
  volume unmeasured.
- **The 235 sibling-language findings.** Deliberately left labelled `true` by
  the narrowing. Whether that is the right call is untested — some are probably
  genuine incompleteness whose changelog simply did not spell out a claim.
- **Stage 8/9 merge behaviour.** When merged concerns disagree on `preexisting`,
  `:223` requires an explicit choice; I did not check which way merges resolve
  in practice, so dedup may be a second, independent source of mislabelling.
- **Cross-review and confirmer paths.** Untouched; per [[confirmer-runs-blind]]
  the confirmer drops deep claims, and an ownership-flip argument is exactly the
  kind of reasoning it cannot see.
- Stage 11's report wording (`:617`) keys off the same flag and should follow
  automatically, but I did not check its phrasing against flipped findings.

## Scope and limitations

Ranked by how much each would change the conclusions:

1. **No experiment.** Everything about effectiveness is projection. The claim
   "all three reviews would post FAIL" is arithmetic on a hypothetical relabel,
   not an observation.
2. **Keyword predicates are not verdicts.** 143/685 counts text patterns. The
   true mislabelling rate needs per-finding adjudication.
3. **Selection effect.** The user surfaced cases they had already judged wrong.
   Starting from known-bad examples inflates apparent prevalence.
4. **Prompt-era confounding.** Findings span nine `prompts_hash` values; per
   [[prompt-era-splits-via-prompts-hash]] a rate computed across eras can
   reflect prompt churn rather than a stable behaviour. The nine-era spread
   argues against that here but does not rule it out.
5. **Ownership is still contestable**, just less so. "The bug class its commit
   message claims to close" is checkable, but *how broadly* a changelog claims
   is itself a reading. A terse "fix foo race" claims more than it means to, and
   the rule will over-fire on exactly those.
6. **New failure mode: changelog gaming.** The rule keys on the commit message,
   so a narrower changelog now buys a cleaner review. Nothing detects that.

## How to re-derive

`scripts/analyze_preexisting_purpose.py` produces every count in this note —
posted state per review, the population table, the narrow-test split, the era
breakdown, and the clean-state count. Read-only (`mode=ro`; the server owns the
DB, so never write to it).

```sh
python3 scripts/analyze_preexisting_purpose.py
```

Message-id to finding: join `messages` -> `patchsets` on `thread_id`, then
`reviews` on `patchset_id`, then `findings` on `review_id`. Scratch dirs (e.g.
`.analysis-*`) are not durable; re-derive from the DB.

See [[stage6-worst-duplicate-ratio]] for the sibling case of a stage-scoping
rule that self-nullifies, and [[prompt-notes-convention]] for this note's format.
