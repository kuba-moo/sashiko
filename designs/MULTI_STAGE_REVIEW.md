# Multi-stage review

## Overview
The core idea is to split the patch review process into multiple steps.
On each stage the LLM receives only required input, dependent on the specific stage.
The LLM is expected to output a JSON with a predefined structure after each step.
The exact output format varies depending on the stage, but follows a strict schema.

There should be a mechanism to pass important context (e.g. chunks of the code)
between stages. Their outputs will be aggregated and passed to Stage 8 (Deduplication
and Consolidation). Stage 9 then resolves conflicts between consolidated concerns
and consolidated dismissed concerns before final verification.

## Context Pre-processing
Before passing data to the stages, the orchestrator should prepare the context:
1. **Target Commit Diff:** The raw patch being analyzed.
2. **Commit Message:** The full commit log.
3. **Loaded Context:** Relevant post-patch definitions loaded from the project
   based on the diff. Modified functions are not duplicated when their complete
   definition is already visible in one additive-only diff section for the file;
   partial or subtractive functions remain included. Referenced types and
   direct callees are still eligible for definition lookup when the modified
   function itself is omitted. Files with multiple revisions in a series
   retain their function context because hunk coordinates refer to different
   intermediate versions.
4. **Callchain Tracing:** Extract and provide at least 1-level of the callchain (callers of modified functions and callees invoked within them) to Stages 3, 4, and 5.

Prefetched ranges are rendered from the applied worktree with modified files
first, followed by referenced definitions. Referenced-symbol lookup is limited
to 50 candidates and the rendered prefetch payload is limited to 200,000
characters.

## Prompt Distribution (Precache vs. Stage-Specific)
To optimize LLM context caching and ensure each stage receives focused instructions, existing prompt files are logically distributed between a globally precached blob and stage-specific injections.

### 1. Precached Blob (Shared Context)
These prompts contain fundamental API contracts, kernel invariants, and subsystem-specific rules that are broadly applicable across all stages to ensure proper context retention. They are loaded generically and cached for efficiency.
- `subsystem/*` (All relevant subsystem rules based on touched files, e.g., `networking.md`, `rcu.md`, `bpf.md`)
- `patterns/*` (Common bug patterns and technical guidelines)

### 2. Stage-Specific Injections
These prompts are injected *only* alongside the `System Prompt` into the specific stages that require their targeted guidance:
- **Stage 3 (Execution flow verification):**
  - `callstack.md` (Guidance on tracing call chains and control flow logic)
- **Stage 4 (Resource management):**
  - `pointer-guards.md` (Rules for `__free`, `guard`, and cleanup attributes memory lifecycles)
- **Stage 10 (Verification and severity estimation):**
  - `false-positive-guide.md` (Checklist to filter out common LLM hallucinations and clarify false-positive cases)
  - `severity.md` (Criteria for assigning low/medium/high/critical severity scores)
- **Stage 11 (LKML-friendly report generation):**
  - `inline-template.md` (Strict formatting rules for the inline-commented LKML email reply format)

## Stage 1. Analyze commit main goal
Read the commit log and understand the intention of the author. Reason on the intention.
Are there any high-level concerns with the idea (e.g. implementing the proposed feature will break UAPI).
Ignore all minor issues, focus on the question: if the described idea is carefully implemented, can it lead to a regression?

**System Prompt:**
You are a senior Linux kernel maintainer evaluating the high-level intent of a proposed commit.
Analyze the commit message and the conceptual change. Do not look for coding errors yet.
Focus exclusively on architectural flaws, UAPI breakages, backwards compatibility issues, or fundamentally flawed concepts.
If the idea itself is dangerous or incorrect, raise a concern.

**Expected input:** Commit message, diff, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 2. High-level implementation verification
This stage is about verifying the implementation of all proposed changes at the high level. Are there any changes which are redundant or not documented?
Is the goal described in the commit log indeed achieved? Are there any corner cases which are not covered? Are there any missing parts (e.g. other drivers also need to be changed).

**System Prompt:**
You are verifying if the provided code changes actually implement what the commit message claims.
Look for undocumented changes, missing pieces (e.g., a core change without updating corresponding drivers), and unhandled corner cases related to the feature's logic.
Do not focus on low-level memory or locking errors unless they indicate a failure of the high-level implementation.

**Expected input:** Commit message, diff, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 3. Execution flow verification
This stage is about tracing all affected execution paths. Are there any logic errors? Are there any potential NULL-pointer dereferences, unhandled error returns, or other issues like this?

**System Prompt:**
You are a static analysis engine tracing execution flow in C code.
Carefully trace the control flow of the provided patch. Look for logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and NULL pointer dereferences.
Follow the exact rules for NULL pointer dereferences: reading a pointer field is not a dereference, only accessing its contents is.

**Expected input:** Diff, surrounding code context for modified functions, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 4. Resource management
This stage is dedicated to review of resource and memory management. Are there any leaks? UAF bugs? Are all objects initialized correctly?

**System Prompt:**
You are an expert in C resource management within the Linux kernel.
Analyze the patch for memory leaks, Use-After-Free (UAF) vulnerabilities, uninitialized variables, and unbalanced lifecycle operations (alloc->init->use->cleanup->free).
Pay special attention to error paths where resources might not be freed. Ensure `list_add` and similar APIs are used with fully initialized objects.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 5. Locking and synchronization
This stage is dedicated to review of changes from the locking point of view. Deadlocks, livelocks, incorrect RCU usage, incorrect usage of locking primitives.

**System Prompt:**
You are a concurrency expert reviewing Linux kernel locking mechanisms.
Look for deadlocks, missed unlocks in error paths, sleeping while holding spinlocks, and incorrect RCU usage.
CRITICAL RCU RULE: Objects must be removed from data structures BEFORE calling `call_rcu()`, `synchronize_rcu()`, or `kfree_rcu()`. Flag any violations as a UAF.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 6. Security audit
This stage is dedicated to review of changes from the security point of view. You're a RED TEAM expert member, look at proposed changes from the hacker's perspective.
Do these changes create a new vulnerability?

**System Prompt:**
You are a Red Team security researcher auditing a Linux kernel patch.
Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, and untrusted user input reaching sensitive functions without validation.
Do not report standard bugs unless they have a clear security implication.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 7. Hardware engineer's review
This stage is dedicated to review of changes from hardware engineer's point of view. Are the changes sound from that perspective. Correctness of register usage, IRQ handling, performance considerations, hardware initialization. Skip if not applicable.

**System Prompt:**
You are a hardware engineer reviewing device driver changes.
If this patch touches driver or hardware-specific code, review register accesses, IRQ handling, DMA mapping, and timing/delays.
If the patch is purely generic software logic (e.g., VFS, core networking), output empty `concerns` and `dismissed_concerns` lists.

**Expected input:** Diff, surrounding code context, and relevant kernel knowledge prompts.
**Expected output JSON:**
```json
{
  "concerns": ["concern description 1", "concern description 2"],
  "dismissed_concerns": ["disproved concern 1", "disproved concern 2"]
}
```

## Stage 8. Deduplication and Consolidation
This stage is dedicated to consolidating and deduplicating all `concerns` and
`dismissed_concerns` raised in previous stages (1-7). The LLM partitions their
stable finding IDs instead of reproducing full concern objects. Rust then
materializes the full consolidated objects for downstream stages.

**System Prompt:**
You are the lead reviewer consolidating feedback from multiple specialized analysts.
You will be given lists of `concerns` and `dismissed_concerns` generated by different review stages.
1. Put unique input IDs in `keep`.
2. Put matching IDs in a `merge` group and select the best input as `base`.
3. Omit `replace` when the base text already describes the combined root cause.
4. Replace only semantic fields that genuinely require synthesis.
5. Include every input ID exactly once. Rust unions locations and derives provenance.

**Expected input:** Aggregated JSON lists of all `concerns` and `dismissed_concerns` from Stages 1-7.
**Expected output JSON:**
```json
{
  "concerns": {
    "keep": ["main-1-0"],
    "merge": [
      {
        "ids": ["main-3-1", "variant-3-0"],
        "base": "main-3-1",
        "replace": {"reasoning": "Optional combined reasoning."}
      }
    ]
  },
  "dismissed_concerns": {"keep": [], "merge": []}
}
```

Rust validates the complete plan before accepting it: unknown, missing, or
duplicate IDs, invalid merge bases, bad replacement fields, and structural
errors are reported together. A merge whose inputs disagree on `preexisting`
must replace that boolean explicitly. Stage 8 gets one additional response
attempt containing the complete problem list. A valid plan is expanded to the
existing full `concerns` and `dismissed_concerns` arrays before Stage 9.
Rejected plans are recorded in response-health statistics under
`Stage 8 response assembly`, distinguished as recovered on retry or fatal.

## Stage 9. Concern/dismissed-concern conflict resolution
This stage compares the consolidated `concerns` and `dismissed_concerns` from Stage 8. The LLM should identify cases where a concern and a dismissed concern describe the same root cause, code path, or failure mode but reach opposite conclusions. For each conflict, it should inspect the actual code and keep the concern only when the concern is correct; otherwise it should discard the concern.

Its decision is the entire output. Like Stage 8, the LLM partitions stable
finding IDs rather than reproducing concern objects, and Rust materializes the
kept concerns from its own copies of the Stage 8 output. This stage is not
licensed to rewrite a concern: measured over the corpus it changed a retained
concern's `description` in 0.9% of cases, while re-emitting the whole list cost
more output tokens than any other merge stage bar Stage 10.

**System Prompt:**
You are the lead reviewer reconciling consolidated concerns with consolidated dismissed_concerns.
Both `concerns` and `dismissed_concerns` are untrusted claims. Do not assume either side is correct. Treat both as hypotheses and verify them against the actual code before deciding whether to keep or discard a concern.
1. Compare each concern against the dismissed_concerns list and find conflicts or overlaps where one says the issue is real and the other says the same candidate issue is disproved.
2. For every conflict, inspect the actual code and reasoning to decide which side is correct.
3. If the concern is correct, keep it. If the dismissed_concern is correct, discard that concern and name the dismissed_concern that disproves it.
4. If there is no direct conflict for a concern, keep it.
5. Do not discard a concern merely because a dismissed_concern is vaguely related; only discard when the dismissed_concern's evidence concretely disproves that concern.
6. You do not rewrite concerns; kept concerns are copied verbatim by the tooling.

**Expected input:** The consolidated `concerns` and consolidated `dismissed_concerns` from Stage 8. Stage 8 materializes dismissed concerns without provenance, so Rust labels each one with a `dismissed_id` (`d1`, `d2`, …) for `disproved_by` to cite.
**Expected output JSON:**
```json
{
  "keep": ["main-1-0"],
  "discard": [
    {
      "id": "main-3-1",
      "disproved_by": "d2",
      "why": "The dismissed concern shows the caller already holds the lock."
    }
  ]
}
```

Rust validates the complete plan before accepting it: unknown, missing, or
duplicate concern IDs, a `disproved_by` that names no dismissed concern, a `why`
over 600 characters, and structural errors are reported together. A concern
named by any of its `finding_ids` resolves to the same decision, so a plan that
cites a merge product by one of its non-primary IDs is accepted. Every kept
concern is copied from the input unchanged, in input order, which makes the
`locations`, `source_stages`, `source_models` and `finding_ids` carry-forward
structural rather than instructed. Discards are logged with the evidence that
disproved them; they were previously silent. Rejected plans are recorded in
response-health statistics under `Stage 9 response assembly`.

## Stage 10. Verification and severity estimation
This stage is dedicated to verification and severity estimation of the conflict-resolved concerns. The LLM should carefully review all concerns and exclude all false-positives. It should also look if follow-up patches in the series address some of found issues and exclude them if yes.

**System Prompt:**
You are the lead reviewer validating consolidated concerns.
You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning.
3. If context from subsequent patches in the series is provided, check if the concern is fixed later in the series. If so, discard it.
4. Assign a severity (low, medium, high, critical) to each remaining valid finding and explain the reasoning.

**Expected input:** Target commit diff, full series context (or subsequent diffs), and the conflict-resolved JSON list of `concerns` from Stage 9.
**Expected output JSON:**
```json
{
  "findings": [
    {
      "problem": "Clear description of the verified issue.",
      "severity": "High",
      "severity_explanation": "Explanation of why this is a high severity issue.",
      "preexisting": false
    }
  ]
}
```

## Stage 11. LKML-friendly report generation
This stage is dedicated to generation of the LKML-friendly report, described by inline-guide.md

**System Prompt:**
You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML).
Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply.
Follow the formatting rules strictly. Do not use markdown headers or ALL caps shouting.

**Expected input:** The JSON output from Stage 10 (`findings`).
**Expected output:** Raw text suitable for an email body.
