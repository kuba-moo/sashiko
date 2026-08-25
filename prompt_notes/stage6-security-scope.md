# Stage 6 (security audit) — duplication scope analysis

Status: **proposed, not applied.** The text below is a recommendation for the stage 6 arm of
`get_stage_prompt` in `src/worker/prompts.rs`. Nothing has been changed in the codebase.
Analysed 2026-07-31 against `findings.source_stages` and `reviews.logs`.

**The validating experiment has not been run.** Every effectiveness number here comes from
subagents simulating a Red Team reviewer reading the proposed prompt — not from running both
prompts through the pipeline on the same patches and diffing stage 6's emitted concerns. That
comparison is the test that would settle it, and it is outstanding.

> **Read this as a point-in-time analysis, not ground truth.** The central judgment —
> "does this defect really belong to stage 6?" — is an LLM opinion about another LLM's output.
> The recall and suppression figures are LLM predictions about a prompt that never ran. Both
> could be wrong in correlated ways. The Scope and limitations section states what would have to
> hold for the conclusions to stand; read it before acting on any figure.

## Scope of the analysis

**What was examined.** Two populations from `sashiko.db`:

| Set | n | Definition |
|---|---|---|
| Sole-source | 145 | `source_stages == '[6]'`, severity ≥ medium, **all time**, non-cross-review |
| Duplicates | 116 | `json_array_length(source_stages) > 1` containing 6, reviews from the **last 48h only**, spanning 78 reviews |

Sole-source is all-time deliberately: the 48h window holds only 14, too few to define the set a
prompt edit must not break. This makes the two tables **non-commensurable** — different periods,
different prompt revisions.

Every finding in both sets was classified individually by a subagent. No sampling, no truncation.

**What was different from the stage 7 analysis:** duplicates were judged on stage 6's *verbatim
pre-merge concern text*, recovered by parsing the stage 8 consolidation prompt out of
`reviews.logs` and linking through `finding_ids` (91% exact, remainder by scored text
similarity). Judging post-merge `problem` text would have measured stage 8's prose instead of
stage 6's reasoning. The sole-source set, by contrast, *was* judged on post-merge text — so the
two halves of this analysis rest on different evidence quality.

**What was NOT examined:**

- **Concerns that stage 6 raised and stages 8–10 discarded.** The `findings` table holds stage
  10's accepted output. Stage 6 emits ~3.9 pre-merge items per review of which ~77% are already
  its own `dismissed_concerns`; none of that was assessed for duplication.
- **Whether the sibling stage would still have found the bug.** "This belongs to stage 3" means
  a classifier judged the subject matter stage 3's. In the 48h window a sibling *did* co-report
  every one of these (that is what makes them duplicates), but under the edited prompt stage 6
  would stay silent, and whether the sibling still catches it in a future run is unmeasured.
- **Single model, single window.** Every review in the 48h window ran on
  `us.anthropic.claude-opus-5`.
- **`source_stages` at face value.** It is stage 8's account of what it merged. Stage 8
  mis-merges are invisible here and would corrupt both populations.
- **No human review of any verdict.**

## Scope and limitations

Ranked by how much each would change the conclusions:

1. **Effectiveness was measured by simulation, three times over, and the answer moved each
   time.** v1 scored 83 pct suppression / 69 pct moderate recall; v2 recovered 78 pct of v1's
   losses but regressed 5 of 20 suppression controls; v3 scored 42/46 on a targeted regression
   set. That the numbers moved this much across drafts is itself evidence that a simulated
   reviewer's verdict is sensitive to wording in ways that may not survive contact with the real
   pipeline.
2. **The verdicts are LLM judgments and only 40 of 69 were high-confidence.** Read
   "59 pct misplaced" as roughly 50–59 pct depending on how the mediums break. Direction is
   solid, precision is not. No verdict had a second independent classifier.
3. **Recall was tested on 133 of 145 sole-source findings, but only the strong tier held
   steady.** All 37 strong survived every draft. The moderate tier (73) swung between 69 pct and
   the high 80s across revisions, and the weak tier (35) is largely and deliberately dropped. If
   the strong/moderate/weak split is itself mis-scored, the headline "no strong finding lost" is
   the claim that fails first.
4. **The last hand-off bullet took four passes and its boundary is still framing-dependent.**
   Drafts 1–3 each traded one correct outcome for another (documented below). The final
   adversarial check found #9604 still admitted through a second clause and #10055 suppressed by
   a different bullet than intended — i.e. two of seven boundary cases land correctly for the
   wrong reason. Assume this clause is the least stable part of the text.
5. **A concurrent process edited the analysis workspace during the run.** Later rounds tested
   prompt revisions that supersede what earlier rounds saw, so pooled figures across rounds are
   not strictly internally consistent. Per-round numbers are reliable; the aggregate is
   indicative.
6. **48h is a small window.** 116 findings over 78 reviews.

**How to re-derive this.** Scripts and per-finding classifications are in
`.analysis-stage6-security-audit/` (untracked, not durable — expect it gone; the candidate
prompt text itself is embedded in the appendix below, so it survives):
`extract2.py` → `make_shards.py` → `aggregate.py` → `score.py`. The
`json_each(f.source_stages)` SQL shape is in `src/db.rs` around line 2927. Re-running against a
fresh window is cheap and preferable to trusting these numbers after a few weeks.

## Finding 1: ~59 pct of stage 6's duplicates look out of scope

Of 116 shared findings: **69 misplaced**, 35 genuinely shared (stage 6 adds real signal),
12 stage-6-owned (a sibling strayed). Misplacement is not low-severity noise — it spans
2 critical, 35 high, 26 medium, 6 low. True owners: stage 3 ×35, stage 4 ×28, stage 2 ×23,
stage 1 ×20, stage 5 ×17, stage 7 ×9 (a finding can name more than one).

68 of 69 were judged preventable by wording; 40 at high confidence.

## Finding 2: stage 6 has the pipeline's worst duplicate ratio

Trailing 48h, engagement from `model_experiment_runs`:

| stage | sole-source | dup | dup:sole | sole/run |
|---|---|---|---|---|
| 1 | 111 | 291 | 2.6x | 0.44 |
| 2 | 35 | 293 | 8.4x | 0.14 |
| 3 | 50 | 262 | 5.2x | 0.20 |
| 4 | 16 | 100 | 6.2x | 0.07 |
| 5 | 47 | 97 | 2.1x | 0.33 |
| **6** | **14** | **116** | **8.3x** | **0.06** |
| 7 | 19 | 129 | 6.8x | 0.11 |

Worst sole-source-per-run in the pipeline, near-worst ratio, on mid-pack token spend
(177k in / 12.6k out, 4.4 tool calls per run).

## Finding 3: the cause is structural — the only conditional exclusion in the pipeline

Every sibling stage ends its SCOPE with an unconditional exclusion:

- Stage 3: "Do not audit locking correctness, memory management, or security attack surfaces."
- Stage 4: "Do not analyze lock correctness or security attack surfaces."
- Stage 5: "Do not investigate general logic errors, missing API callbacks, or buffer overflow attack surfaces."

Stage 6 ends with: *"Do not report general logic errors or resource leaks **unless they have a
direct security impact**."*

Any kernel defect can be framed as an availability impact, so that clause is self-nullifying.
Siblings defer *to* stage 6 unconditionally; stage 6 defers to nobody. Discovery stages run
independently and never see each other's output, so prompt wording is the only lever.

## Finding 4: which framings are duplication risk, and which are load-bearing

Marker prevalence in stage 6's raw pre-merge text, duplicates vs sole-source:

| marker | dup | sole | lift |
|---|---|---|---|
| DoS label | 42 pct | 14 pct | 2.9x |
| leak/refcount/UAF as mechanism | 31 pct | 7 pct | 4.5x |
| NULL deref / missing return check | 24 pct | 6 pct | 4.4x |
| TOCTOU word | 14 pct | 3 pct | 5.0x |
| lock/race/deadlock as mechanism | 6 pct | 1 pct | 8.8x |
| **OOB/overflow/bounds** | **36 pct** | **45 pct** | **0.8x** |
| **infoleak / copy_to_user / uninit** | **22 pct** | **22 pct** | **1.0x** |

The first five discriminate. The last two do not — they are load-bearing and must not be
restricted. Caveat: these are regex counts over concern text, so they measure vocabulary, not
mechanism.

A rule requiring a **named attacker** was tested and rejected: it fails to discriminate
(19 pct dup vs 12 pct sole) and was the most expensive rule tried, losing 36 sole-source
findings including the only strong finding any rule lost (#2337, TLS plaintext on the wire).

## The five escalation routes

Every shard independently found the same ones:

1. **Availability relabeled as security** (37/69) → stages 3, 5, 7. "even under panic_on_warn"
   recurs verbatim.
2. **Leak / refcount / UAF as a security primitive** (28/69) → stage 4. Often with stage 6's own
   text conceding the impact is bounded to memory growth.
3. **NULL deref / error-path omission as "unprivileged DoS"** (16/69) → stage 3. Trigger is an
   allocation failure or a privileged unbind.
4. **TOCTOU as a keyword, not a mechanism** (9/69) → stage 5. Note the collision: "TOCTOU"
   appears in both stage 5's and stage 6's scope lines.
5. **Over-strict validation / unenforced policy** → stages 1, 2, 3. Checks that reject
   *legitimate* traffic, filed as remote DoS.

A sixth, flagged independently by three shards and more actionable than any category rule:
stage 6 pursues a genuine security hypothesis, disproves it, then **files the sibling's bug
anyway** (#10037, #9837, #9743, #9932). A filing-decision failure, not scope confusion.

## The proposed text

Unchanged verbatim: the Red Team persona (load-bearing for all 145 sole-source findings), the
enumerated bug list, "untrusted user input reaches sensitive functions without validation",
"all length checks and bounds checks are robust against malicious input", "attack surfaces and
data boundaries".

Three additions widen rather than restrict, to protect sole-source findings the old wording
already put at risk: device/firmware/DMA/mailbox values named as untrusted input (40 sole-source
findings have `device_hardware_input` as their surface, and the old text says only "user
input"); disclosure covering escape to a peer, a device, or the wire, plus key material past its
lifetime; and actor-driven unbounded growth kept in scope.

The full candidate text is in the appendix at the end of this note. Its shape:

- **SCOPE** — sibling exclusion made unconditional, with the impact override pointed at a closed
  REPORT enumeration instead of open-ended "direct security impact".
- **REPORT** — five outcome bullets: OOB/wrong-object/freed-object access where you can say what
  it yields; disclosure (incl. to a peer, device, or the wire, and key lifetime); a
  bounds/permission/ownership/auth/input-validation check bypassed or absent where untrusted
  input then reaches a constrained operation; a crossed privilege/tenant/namespace/peer-function/
  VF-to-host boundary; a length/size/index/offset/divisor driven out of range or made to wrap,
  or actor-driven unbounded growth.
- **HAND OFF** — six bullets, explicitly taking precedence over REPORT: availability-only;
  leak/refcount/UAF without a nameable yield; missing return check, NULL deref, error path,
  wrong branch, missing lock, and TOCTOU-without-a-raced-security-decision; over-strict checks;
  teardown that fails to revoke a published DMA address or handle; and a defect another stage's
  bug must enable first.
- **A standalone carve-out** for missing bounds, which no hand-off bullet covers because it needs
  nothing else to break first.
- **A closing filing rule** — stop at dismissed_concerns when your hypothesis dies rather than
  re-filing under another stage's heading, but report an established outcome whose boundary you
  could not confirm, with the uncertainty stated.

## The clause that took four passes

The latent-defect hand-off bullet is the least stable part of the text:

- **Draft 1** excluded anything needing "a future change to this tree's data structures *or
  callers*" → closed #9703/#9604, lost #4513/#4244 (genuine latent missing bounds).
- **Draft 2** narrowed to size/layout changes → recovered #4513/#4244, re-opened #9703, whose
  precondition is a *caller sequence*, not a size change.
- **Draft 3** separated on *what must change* and named the enabling-defect categories with
  their owning stage.
- **Draft 4** (current) closed a second route the final check found: the missing-bound carve-out
  admitted #9604 through its in-tree-helper arm, fixed by requiring the value to already exceed
  the bound at current in-tree type sizes.

Even now, of seven boundary cases, #9604 and #10055 land correctly for the wrong reason —
#10055 suppresses on the teardown-revocation bullet rather than the separator. Expect this
clause to need another pass against real pipeline output.

## Accepted residual costs

- **#9697 / #10055** (device DMA into freed pages during teardown) may still be filed. Closing
  them cleanly would cost #8749 and #271. "A device can still DMA into a freed object" is
  simultaneously a stage 4 lifecycle bug and a real memory-safety exposure; the overlap looks
  irreducible by wording.
- **#2533** (reachable WARN in `ovpn_tcp_poll`) is suppressed — availability-first by
  construction, intended.
- **Userspace `tools/` and selftest defects** (#9513) are textually in scope. Pre-existing in
  the current prompt too.
- **#9743** (SCTP `auth_chunks` OOB read) was lost in early drafts because stage 6's own raw
  text talked itself out of the finding. Root cause was under-investigation, not scope.

## If this is applied

- **Protect:** the `[6]`-only count, 14 per 239 stage 6 engagements in the measured window. If it
  falls, revert.
- **Expect:** duplicates containing 6 drop from 116 toward 50–65; dup:sole moves from 8.3x toward
  ~4x, in line with stages 3 and 5. This is an extrapolation from simulated verdicts, not a
  measurement.
- **Leading indicator:** stage 6's `dismissed_concerns` share, currently 77 pct of its output.
  The edit should move items from `concerns` to `dismissed_concerns`, not reduce total analysis.
  If pre-merge item volume (3.9/review) falls sharply, stage 6 is analysing less rather than
  filing more precisely — also a revert signal.

## Open questions

1. `local_canonical_findings` is empty for 25 of the 78 reviews in the window, so
   finding→concern provenance is unrecoverable for them. Worth checking whether that is
   retention policy or a write path that fails.
2. One shard reported `sibling_stage_concerns` empty for #9932 despite `source_stages` being
   `[1,3,4,6]` — a possible gap in stage 8 input capture, or in this analysis's own extraction.
3. Stage 2 (8.4x) and stage 7 (6.8x) have ratios as bad as stage 6's. Whether the same
   conditional-exclusion defect explains them is untested here; stage 7 is covered in
   [stage7-hardware-scope.md](stage7-hardware-scope.md).
4. The sixth escalation route (file the sibling's bug after your own hypothesis dies) may be
   better addressed by a shared instruction across all discovery stages than by stage 6's prompt
   alone.

## Appendix: the candidate text, verbatim

This replaces the `6 =>` arm's string in `get_stage_prompt`. Everything appended after it
by `format_guidance` (output schema, EFFICIENCY, CRITICAL REVIEW DIRECTIVE) is unchanged.

```
# Stage 6. Security audit

SCOPE: You audit ONLY security vulnerabilities and attack surfaces. Other pipeline agents cover: high-level design (Stage 1), implementation completeness (Stage 2), control flow (Stage 3), resource lifecycle (Stage 4), locking/concurrency (Stage 5), and hardware (Stage 7). Those stages report their own bugs. A bug one of them owns does not become yours because a kernel defect is bad for the system; it becomes yours only when it produces one of the REPORT outcomes listed below.

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries. Values supplied by a device, its firmware, a DMA descriptor, or a peer or lower-privileged function over a mailbox are untrusted input too, however well-behaved conforming hardware would be.

REPORT a concern when the patch allows, or leaves in place, one of these outcomes, and name the outcome and the data path that produces it:

- An out-of-bounds, wrong-object, wrong-register, or freed-object read, write, or indirect call, where you can say what the access yields: attacker-influenced content, a controlled write, or data that escapes. Memory or type confusion.
- Disclosure of kernel memory, kernel or physical addresses, uninitialized or stale bytes, or secrets, reaching user space, a peer, a device, or the wire. This includes key material or plaintext readable past its intended lifetime, and kernel metadata that reaches a packet header it should not.
- A bounds, permission, ownership, authentication, or input-validation check that untrusted input can bypass, or that is absent where untrusted input then reaches an access, allocation, or privileged operation that the check existed to constrain. A mitigation this patch introduces that attacker-supplied input can still evade is yours, not a policy question.
- A crossed privilege, tenant, namespace, peer-function, or VF/guest-to-host boundary: attacker-supplied data or an attacker's access reaches the far side, or data is delivered to a socket, object, or context that a demux, lookup, or routing decision should have excluded. A reference merely spanning the boundary is not enough.
- A length, size, index, offset, or divisor that an attacker, peer, or device can drive out of range or make wrap, reaching an access, allocation, or division; or growth of a live memory, buffer, table, or queue allocation, or of recursion or duplication depth, that such an actor can drive with no accounting limit or ceiling. Repeated orphaning of freed-and-forgotten objects is a leak, not growth, and belongs to Stage 4.

HAND OFF, and do not report, the following. These take precedence over the REPORT list, and a bullet applies even when you can also name a REPORT outcome:

- A crash, oops, BUG(), WARN, splat, hang, livelock, deadlock, interrupt storm, log flood, or a wrong or lost result, with no REPORT outcome beyond it. Availability by itself is not your finding, even when an unprivileged or remote actor can trigger it, and even under panic_on_warn. If a REPORT outcome does accompany the crash or hang, the finding is yours: report it on that outcome and describe the crash as impact. A check an attacker can bypass is a REPORT outcome even when its only visible effect is a wrong value.
- A memory leak, refcount imbalance, use-after-free, or double-free whose consequence is only that accounting is wrong or the kernel dies. It is yours when an attacker supplies the data or controls the timing that reaches the freed or reused object, or when the resulting access yields attacker-influenced content, a controlled write, or data that escapes to user space, a peer, a device, or the wire. That the freed object is merely touched is not enough. A timing story resting on an administrative action or an allocation failure does not by itself make it yours — but that only rules out the timing route; if the resulting access still yields content, a write, or data that escapes, the finding is yours on that yield regardless of who triggered it. Name the data that escapes and where it lands: bytes read from freed memory that only feed a statistics counter or a summary statistic have not escaped.
- A missing return-value check, NULL dereference, uninitialized-pointer use, error-path omission, or wrong branch. That is Stage 3's scope, even when the missing check would have validated a device-, firmware-, or peer-supplied value, unless untrusted input then reaches an access, allocation, or privileged operation with a REPORT outcome you can name. A NULL or error pointer that only faults is Stage 3's. A missing lock, lock ordering, memory barrier, or RCU-annotation defect is Stage 5's, unless the missing synchronization is what lets an actor reach a REPORT outcome. TOCTOU is yours only when the raced value reaches a bounds, permission, ownership, authentication, or input-validation decision, or a length, index, or copy computation, over data an attacker, peer device, or DMA agent supplies; an unsynchronized read of kernel-internal state is Stage 5's.
- A check that is too strict and rejects legitimate input, whose harm is that valid traffic is dropped; or a policy choice you would argue differently. Your subject is input that should have been rejected and was not.
- A teardown path that fails to revoke a DMA address, handle, or registration already published to a device or peer before releasing the object. Stage 4 owns teardown symmetry.
- A defect that something else must enable before any input could reach it: a missing state reset or re-entry guard (Stage 3), an unenforced call-ordering or lifecycle invariant (Stage 4), a missing lock (Stage 5), or a struct, union, or in-tree type growing in size (no stage owns a change that has not happened; say so). Record your concern in dismissed_concerns, naming the enabler and its owner where there is one.

A missing bound, clamp, or overflow check is the one thing no hand-off bullet covers, because it needs nothing else to break first. It stays yours on any length, size, index, or offset supplied by incoming data, or passed by in-tree code into a helper that does not bound it, where the value can already exceed the bound at the types' current in-tree sizes — even when no caller is proven to violate it, when the existing caller does not yet pass the triggering argument, or in a new helper whose caller arrives in this or a later series. Report it and state the unproven precondition. A UAPI or ABI layout defect that becomes permanent when the patch merges is also yours; that is the one layout question that is not a size change.

If a security hypothesis does not survive your own investigation, record it in dismissed_concerns and stop. Do not re-file the underlying bug under another stage's heading. A concern whose REPORT outcome you established but whose trust boundary or trigger you could not confirm belongs in your report with that uncertainty stated, not in dismissed_concerns. An upstream invariant you have not verified is not a defense: if the patch itself does not establish a bound before dereferencing attacker-supplied data, the missing check is yours. Every concern you keep must name a REPORT outcome together with the actor, the input, or the structural precondition (skb geometry, buffer or chunk size, configuration) that reaches it. For a disclosure, key-exposure, or stale-data outcome, naming the data that escapes — or that is left uninitialized or stale where a later consumer reads it — is sufficient; no attacker need trigger it.
```
