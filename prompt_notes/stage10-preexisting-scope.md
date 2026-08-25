# Stage 10: `preexisting` is over-claimed for incomplete and superseded fixes

**Status:** proposed, not applied. Targets the default prompt arm
(`PromptRegistry::get_stage_prompt()` stage 10 at `src/worker/prompts.rs:600-614`,
the shared concern schema at `src/worker/prompts.rs:2153`, and
`third_party/prompts/kernel/severity.md`).

**Whether the validating experiment ran:** **No.** No pipeline A/B was run. The
counts below are SQL over already-stored findings, so the *population sizes* are
real measurements. The judgment that a given finding is mislabelled is an LLM
opinion (mine) about another LLM's output, and the projected effect of the
proposed wording is **not measured at all**.

> **This is a point-in-time analysis, not ground truth.** The core question —
> "does this defect belong to the patch or to the tree?" — is a judgment call,
> and I am making it by reading finding text rather than by re-reviewing the
> patches. Two findings (10267, 10563) were confirmed against the reviews by the
> user; the rest of the population is inferred from text patterns.

## The defect

Two reviews flagged by the user:

| Finding | Patchset | Msg-id | Severity | `preexisting` |
|---|---|---|---|---|
| 10563 | 4026 | `e93d0d2f...zhilinz@nebusec.ai` | 3 (High) | **1** |
| 10267 | 3951 | `20260731162938.3388534-1-nicoyip.dev@gmail.com` | 3 (High) | **1** |

Both findings are *correct on the merits* and both contain, in their own
`severity_explanation`, the argument that they belong to the patch:

- 10563: "*the patch defines the intended cb contract for the peer branch while
  deliberately (and silently) leaving this path as-is, so the fix and its
  changelog are incomplete for the very threat model they describe*"
- 10267: "*the patch touches precisely this locking and fixes only the
  reclaim/UAF half, so the residual write-side race should be addressed here*"

So the model already reached the right conclusion in prose and then set the
boolean the other way. This is not a detection failure — it is a labelling
failure, and the label is the thing with teeth.

## Why the label matters

`PatchworkVerdict` computes state from **new findings only**
(`src/patchwork.rs:78-80`):

```rust
let state = if total_new == 0 { "success" } else { ... };
```

`total_preexisting` feeds the *description* string but never the state. A High
stamped `preexisting: true` therefore posts **`success`** to Patchwork — the
author sees "Sashiko AI review found no regressions"-class output for a
genuine, High-severity problem with their patch. `sashiko-cli` mirrors this:
`src/bin/sashiko-cli.rs:605-609` and `:1885` filter `preexisting` out of the
headline issue set.

## Why the model labels it this way

The schema definition is a pure origin test, with no notion of ownership
(`src/worker/prompts.rs:2153`, and again in stage 10's own output spec at
`:1763` / `:1769`):

> `"preexisting"`: A boolean value: `true` if this bug/vulnerability **already
> existed in the codebase before these patches were applied**, or `false` if the
> issue was newly introduced by the reviewed patchset.

Read literally, both findings *are* `true`: the buggy bytes predate the patch.
Nothing in the prompt distinguishes "this bug is in a part of the tree the patch
never claimed to touch" from "this bug is the other half of the exact thing the
patch claims to fix". Stage 10 rule 6 then reinforces the origin framing and
attaches a discard rule to it:

> 6. If the problem did exist in the code before the patch was applied, say it
>    explicitly: 'This problem wasn't introduced by this patch, but...'. Discard
>    low- and medium-severity pre-existing problems, report only high- and
>    critical severity issues.

Note the second-order risk: because `preexisting` also gates *discarding* at
low/medium, widening what counts as "not preexisting" will surface some
low/medium findings that are currently dropped. That is a real behavioural
change, not just a relabelling — see Limitations.

`severity.md` compounds it. The rubric calibrates on consequence / triggering
path / reachability and never asks whether the fix accomplishes its stated
purpose, so "the fix is useless in itself" has no home in the scale.

## Population size

All findings, severity >= High:

| `preexisting` | High | Critical |
|---|---|---|
| NULL (pre-flag era) | 1230 | 210 |
| 0 | 225 | 23 |
| 1 | **236** | **10** |

Within the 246 `preexisting=1` High+Critical findings:

- **44** are headlined "Incomplete fix".
- **119** match a self-contradiction pattern — the explanation argues patch
  ownership (`but the patch...`, `should be addressed here`, `the patch
  touches...`, `incomplete for...`) while the flag says otherwise.
- **156** use sibling/duplicate language (`same`, `identical`, `sibling`,
  `peer`, `adjacent`, `counterpart`).

These sets overlap and the predicates are keyword heuristics, not verdicts, so
119/246 is an upper bound on the mislabelling rate, not an estimate of it. The
honest claim: **the pattern is systematic rather than anecdotal, and its scale
is order-100 findings.**

Two of these were already known. `STAGE3_GUIDE_SCOPE_OVERRIDE`'s doc comment
(`src/worker/prompts.rs:168-170`) cites findings 7881 and 9863 as stage-3-only
"Incomplete fix" Highs, and 9863 is in this mislabelled set — so the override
preserved the *detection* while the `preexisting` flag silently discarded the
*verdict*.

## Proposed change

Three coordinated edits. The design principle: **do not touch the origin
question.** `preexisting` keeps meaning "did these bytes predate the patch".
Instead, make ownership a separate, explicit test that overrides the flag, so
the model is never asked to lie about history.

### 1. Shared concern schema (`src/worker/prompts.rs:2153`)

Append to the `"preexisting"` bullet:

> Set `false` — the defect is the patch's own — whenever any of these hold, even
> though the offending code predates the patch:
> (a) **Same-fix-scope defect.** The same bug class, root cause, or mechanism the
> patch fixes is still reachable in a sibling location the patch could have
> covered in the same edit: another branch of the same function, a peer helper,
> the IPv4/IPv6 or TX/RX counterpart, another member of the same driver family,
> or another caller of the same API. A fix that closes one instance and leaves
> its siblings is an incomplete fix, and incompleteness is this patch's defect.
> (b) **Superseding fix.** The defect is such that fixing it correctly would
> subsume or render unnecessary the change this patch makes.
> (c) **Contract-defining patch.** The patch establishes or documents an
> invariant that some untouched path visibly violates.
> Reserve `true` for defects in code the patch neither touches nor claims a
> contract over.

### 2. Stage 10 rule 6 (`src/worker/prompts.rs:607`)

Replace with:

> 6. Decide ownership before severity. Apply the `preexisting` test in the
>    concern schema: a defect is the patch's own when the patch leaves the same
>    bug class reachable in a sibling location it could have fixed in the same
>    edit, when fixing the defect properly would supersede this patch's change,
>    or when the patch defines a contract the defect violates — set
>    `preexisting: false` for these even though the code predates the patch, and
>    say what the patch should have covered. Only for a defect in code the patch
>    neither touches nor claims a contract over, set `preexisting: true`, say
>    'This problem wasn't introduced by this patch, but...', and discard it if it
>    is low or medium severity.

### 3. `severity.md` — add a High example

Under `## High`, add:

>     - A fix that is superseded by the correct fix: the defect is such that
>       fixing it properly would make this patch's change unnecessary, so the
>       patch as written adds churn without achieving its stated purpose.
>     - An incomplete fix that leaves the same bug class reachable in a sibling
>       location (peer branch, IPv4/IPv6 counterpart, other caller of the same
>       API) that the patch could have covered in the same edit.

`severity.md` is vendored under `third_party/prompts/` and would be reverted by
the next upstream sync (see `third_party/prompts/REVISION`) — the same trap
documented for the stage-3 override. Land this as a Rust-side appended override
in the stage 10 prompt, not as an edit to the vendored file.

### Not proposed

Changing `src/patchwork.rs` to fail on preexisting Highs. That would convert
every genuinely-unowned pre-existing High into a build failure for an unrelated
author — the opposite complaint. The fix belongs at the labelling boundary.

## Scope of the analysis

Examined: all rows in `findings` with `severity >= 3`
(`SELECT ... FROM findings WHERE severity >= 3`, n=1934 across all
`preexisting` values; n=246 for `preexisting=1`). Findings 10563 and 10267 read
in full. Consumers traced: `src/patchwork.rs`, `src/bin/sashiko-cli.rs`.
Definition sites traced: `src/worker/prompts.rs:1763`, `:1769`, `:2153`, `:607`,
`:617`, plus stage 8/9 preservation rules at `:223`, `:580`, `:595`, `:1552`,
`:1589`.

**What was NOT examined:**
- **No A/B run.** Zero evidence the proposed wording changes any label. This is
  the single largest gap.
- **No `prompts_hash` split.** Per `[[prompt-era-splits-via-prompts-hash]]` the
  population spans multiple prompt eras; the NULL-`preexisting` rows (1440
  findings) predate the flag and were excluded without checking when it landed.
- **False-positive direction.** I did not sample `preexisting=0` findings to see
  whether the flag is *also* under-claimed, so I cannot say whether the net
  effect is more accurate labels or just more `false`.
- **The low/medium discard interaction.** Rule 6 currently drops low/medium
  preexisting findings. Widening "not preexisting" un-drops some. Volume
  unmeasured.
- **Stage 8/9 merge behaviour.** When merged concerns disagree on `preexisting`,
  `:223` requires an explicit choice; I did not check which way merges resolve
  in practice, so dedup may be a second, independent source of mislabelling.
- **Cross-review and confirmer paths.** Untouched here; per
  [[confirmer-runs-blind]] the confirmer drops deep claims, and an
  ownership-flip argument is exactly the kind of reasoning it cannot see.
- Stage 11's report wording (`:617`) keys off the same flag and would follow
  automatically, but I did not check its phrasing against flipped findings.

## Scope and limitations

Ranked by how much each would change the conclusions:

1. **No experiment.** Everything about effectiveness is projection.
2. **Keyword predicates are not verdicts.** 119/246 counts text patterns. The
   true mislabelling rate needs per-finding adjudication.
3. **Prompt-era confounding.** Findings span prompt revisions; a rate computed
   across eras can reflect prompt churn rather than a stable behaviour.
4. **Selection effect.** The user surfaced two cases they already judged wrong.
   Starting from known-bad examples inflates apparent prevalence.
5. **Ownership is genuinely contestable.** For (a) especially, "could have
   covered in the same edit" is a maintainer judgment. Net-next convention often
   *prefers* narrow fixes; an over-broad rule will produce "you should have fixed
   more" noise on legitimately-scoped patches. Rule (b) is much safer than (a).

## How to re-derive

Population and pattern counts (read-only; the server owns the DB, so use
`mode=ro` and never write):

```sh
sqlite3 'file:sashiko.db?mode=ro' -header -column "
SELECT preexisting, severity, COUNT(*) FROM findings
WHERE severity >= 3 GROUP BY preexisting, severity;"

sqlite3 'file:sashiko.db?mode=ro' -header -column "
SELECT COUNT(*) FROM findings WHERE preexisting=1 AND severity>=3
  AND problem LIKE '%ncomplete fix%';"
```

The two confirmed findings: `SELECT * FROM findings WHERE id IN (10267, 10563);`
Message-id to finding: join `messages` -> `patchsets` on `thread_id`, then
`reviews` on `patchset_id`, then `findings` on `review_id`.

Scratch dirs (e.g. `.analysis-*`) are not durable; re-derive from the DB.

See [[stage6-worst-duplicate-ratio]] for the sibling case of a stage-scoping
rule that self-nullifies, and [[prompt-notes-convention]] for this note's format.

## Appendix: candidate prompt text

Verbatim replacement for stage 10 rule 6 (`src/worker/prompts.rs:607`):

```
6. Decide ownership before severity. A defect is this patch's own — set
"preexisting": false even though the offending code predates the patch — when
any of these hold: (a) the patch leaves the same bug class, root cause, or
mechanism it fixes still reachable in a sibling location it could have covered
in the same edit (another branch of the same function, a peer helper, the
IPv4/IPv6 or TX/RX counterpart, another driver in the same family, another
caller of the same API); (b) fixing the defect correctly would subsume or
render unnecessary the change this patch makes; or (c) the patch establishes or
documents an invariant that the defect visibly violates. In each case name what
the patch should have covered in the same edit. Only when the defect lives in
code this patch neither touches nor claims a contract over, set "preexisting":
true, state 'This problem wasn't introduced by this patch, but...', and discard
it if it is low or medium severity.
```

Verbatim append to the `"preexisting"` bullet in the shared concern schema
(`src/worker/prompts.rs:2153`):

```
Set false — the defect belongs to this patch — whenever the patch leaves the
same bug class it fixes reachable in a sibling location it could have fixed in
the same edit, whenever fixing the defect properly would supersede this patch's
change, or whenever the patch defines a contract the defect violates, even
though the offending code predates the patch. Reserve true for defects in code
the patch neither touches nor claims a contract over.
```

Verbatim override to append to the stage 10 prompt for the severity scale
(do NOT edit `third_party/prompts/kernel/severity.md` directly):

```

# Severity override for incomplete and superseded fixes

Rate High: a fix whose stated purpose is defeated because the correct fix for a
defect you found would supersede it entirely — the patch as written adds churn
without achieving what its changelog claims. Rate High likewise a fix that
closes one instance of a bug class while leaving the same class reachable in a
sibling location it could have covered in the same edit. In both cases the
consequence to state is that the patch does not accomplish its stated purpose,
and the author must respin rather than land it.
```
