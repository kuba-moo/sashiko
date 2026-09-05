# Cross-Instance Reviews

## Status

Proposed and approved for implementation.

## Problem

One Sashiko deployment may review the same patch series as another deployment
using a different model. Once the local review completes, the local instance
should import the remote deployment's published findings, compare them with the
local canonical findings, validate novel remote findings, and update the locally
rendered result.

The remote deployment is not under our control. It is assumed to run Sashiko
from origin/main and cannot be changed to expose a purpose-built peer API.

## Goals

- Configure zero or more named remote Sashiko instances.
- Start cross-review work as soon as the normal local review completes,
  independently of whether a local embargo is still withholding it.
- Persist all polling state so work resumes after process restarts.
- Poll a remote hourly for up to three days while its result is unavailable.
- Stop immediately when the remote reports a terminal error.
- Import only final, non-pre-existing remote findings.
- Compare remote results with the already deduplicated local canonical set.
- Batch-confirm remote-only findings with the local main model.
- Add confirmed remote-only findings to the local rendered result.
- Report per-remote severity comparisons without remote cost data.
- Mark a patchset cross-reviewed only after every configured remote completes
  successfully, including remotes that report no findings.

## Non-Goals

- Changing the remote Sashiko API.
- Fetching embargoed findings before the remote publishes them.
- Comparing remote token costs or stage-level execution.
- Asking the remote model to confirm local-only findings.
- Editing email that has already been sent. An embargoed patchset has not sent
  any yet, so its release email is composed from the merged report; see
  [Local Embargo Independence](#local-embargo-independence).
- Treating remote instances as model experiment executions.

## Configuration

Remote instances are configured with stable, route-safe names:

```toml
[[cross_review.instances]]
name = "gemini-instance"
url = "https://gemini-review.example.org"
```

Names accept ASCII letters, digits, underscore, and hyphen. The reserved name
`main` is rejected. URLs must use HTTP or HTTPS and are normalized without a
trailing slash. HTTP is permitted for explicitly configured private or local
deployments; operators are responsible for transport security.

The polling interval and deadline are intentionally fixed at one hour and
three days for the first implementation. They are workflow policy rather than
per-source model settings.

## Remote Compatibility Contract

The client consumes the existing origin/main endpoint:

```text
GET {base_url}/api/patchset?id={message_id}&per_page=100
```

Patchset database IDs and slugs are instance-local. Lookup starts with the
first patch message ID, which a reviewing instance must have, and falls back to
the cover-letter message ID when it differs.
The response maps remote review patch IDs back to RFC message IDs through the
returned `patches` array.

The response is display-oriented and not a versioned federation protocol. The
client therefore uses a tolerant compatibility parser:

- Unknown fields are ignored.
- Required identity and completed-result fields are validated.
- Each `reviews[].output` string is parsed as worker JSON.
- Only the newest completed review for each patch is part of the active remote
  result; historical reruns are ignored.
- Findings are read from `review.findings`.
- Findings with `preexisting = true` are discarded locally even if a remote
  version normally omits them from its public result.
- A completed response whose reviews cannot be associated safely with patch
  message IDs is a terminal protocol error.
- Unknown patchset statuses are treated as unavailable and remain retryable
  until the normal polling deadline.
- Duplicate normalized findings retain one deterministic explanation instead
  of failing the complete import when only their reasoning differs.

Captured origin/main responses provide compatibility fixtures.

## Durable Scheduling

There is one Tokio polling task for the service, not one task or thread per
patchset or remote. All authoritative state is stored in the database.

```text
local review completed
        |
        v
create one job per configured remote
        |
        v
single scheduler claims due jobs with a lease
        |
        +-- unavailable --> next_attempt_at = now + 1 hour
        +-- reviewed ----> import and complete
        +-- error -------> terminal error
        +-- deadline ----> expired
```

The scheduler shares the reviewer service loop with embargo release and
patchset dispatch, and runs ahead of dispatch. Cross-review completes a review
that already exists, the remote side is embargoed for far longer than a local
review takes, and starvation does not extend a job's deadline, so a backlog of
pending patchsets must never delay a due job. Dispatch therefore claims only
review capacity that is free at that moment and never waits for a permit.

The scheduler periodically claims a bounded batch of jobs using an atomic
state transition, a lease timestamp, and a unique fencing token. Every retry,
completion, and persistence mutation must still own that token. A crashed or
slow process cannot overwrite a reclaimed, completed, or expired job. The
original deadline is never reset by restart or transient failure.

Polling retries and integration retries are separate. Missing, incomplete,
embargoed, and transient HTTP results return to hourly polling. Once a final
remote result arrives, Sashiko performs at most three immediate integration
attempts. Each attempt starts a fresh merge budget, while usage from all three
attempts is accumulated for accounting. Exhausting all three attempts marks
the source terminally `error`; integration failures never return to hourly
polling.

Remote states are classified as follows:

| Remote result | Action |
| --- | --- |
| `Reviewed` | Import final result. |
| `Skipped` | Complete successfully with no findings. |
| `Incomplete`, `Pending`, `In Review`, `Embargoed` | Retry hourly. |
| HTTP 404 for both lookup IDs, transport error, HTTP 429, HTTP 5xx | Retry hourly. |
| `Failed`, `Failed To Apply`, `Cancelled` | Stop with terminal error. |
| Malformed completed result | Stop with terminal protocol error. |
| Three-day deadline reached | Stop as expired. |

## Persistence

The schema is additive and safe for existing databases.

`cross_review_jobs` stores one row per local review generation and configured
source:

- Local patchset ID, review generation, and source name.
- Source URL captured when the job is created.
- `pending`, `processing`, `complete`, `error`, `expired`, or `superseded`
  status. A local rerun supersedes unfinished jobs from the prior generation.
- First attempt, next attempt, lease, completion, deadline, and fencing token.
- Attempt count and last error.
- Remote model and provider when available.
- Local model and provider captured at enqueue time for historical statistics.
- A hash of the imported payload for idempotency.

Upgrading from the pre-generation schema rebuilds the job table so the old
`(patchset_id, source_name)` uniqueness constraint is replaced by
`(patchset_id, generation, source_name)`. Existing jobs are assigned to the
patchset generation active at migration time, and the patchset generation is
backfilled before polling resumes. This assignment runs only as part of that
one-time table rebuild; later startups never rewrite historical generations.

`cross_review_findings` stores normalized remote discoveries and their local
confirmation decision. `cross_review_comparisons` stores the pairwise outcome
used by statistics. Both are keyed by job and deterministic finding identity,
so replaying a completed response is idempotent.

Locally rejected canonical findings are derived without changing Stage 10.
The worker privately returns the already-existing Stage 9 canonical candidate
set. The parent compares its finding IDs with the normal published Stage 10
IDs: published IDs are accepted, and remaining Stage 9 candidates are rejected.
The parent removes this internal field before storing public AI output.

Patchsets receive orthogonal cross-review state rather than a new review
status:

```text
cross_review_status = disabled | pending | complete | error | expired
cross_reviewed_at = timestamp or null
```

The existing `Reviewed` status remains unchanged because it controls release,
statistics, reruns, and scheduler behavior. The UI may display
`Cross-reviewed` when normal status is `Reviewed` and cross-review status is
`complete`.

With multiple sources, aggregate completion requires every source to complete
successfully. An empty or skipped remote result is successful. Any terminal
error or expiration prevents the aggregate state from becoming complete.

## Discovery Flow

Remote results enter after the local model pipeline has produced its canonical
set:

```text
main raw discoveries ----+
variant raw discoveries -+--> local canonical findings
                                      |
remote final findings --> normalize --+--> cross-source deduplication
                                               |
                                               +--> shared: retain
                                               +--> remote-only: confirm
```

The cross-source deduplication input contains:

1. Accepted local canonical findings that were eligible for publication.
2. Rejected local canonical findings retained for comparison provenance.
3. Final, already deduplicated remote findings after pre-existing filtering.

The local portion is snapshotted from the newest completed review for each
patch in the active generation. Historical local reruns are not inputs to a
new cross-review generation.

Comparing with raw stage findings is prohibited. Raw findings contain
stage-level duplicates, rejected candidates, and temporary artifacts that do
not correspond to the published local review.

Remote findings receive deterministic IDs derived from source name, patch
message ID, and normalized finding content. Deduplication preserves all source
IDs and source names.

Remote sources for one patchset generation are integrated serially. The
semantic deduplication pool contains the immutable local canonical set plus
accepted remote-only findings that were published by already completed remote
jobs. Remote records classified as `both`, and aliases of an earlier imported
finding, are not independent anchors. A finding matching an earlier published
remote remains `remote_only` for its local-vs-remote graph, skips a second
confirmation, and merges its source provenance into the existing published
finding rather than creating a duplicate. Serialization closes the race where
two remote analyses would otherwise both observe an empty imported set.

Imported remote findings remain part of the consolidated published result but
never become local-main findings in another remote's pairwise graph. Match
records retain both the incoming and matched finding identities. A local match
also merges the remote source name and finding ID into rendered provenance,
whether the local candidate was already published or is published only after
the remote corroborates it.

## Validation And Publication

A remote finding matching any local canonical finding has outcome `both` and
does not require another confirmation. A remote-only finding is submitted to
the local main provider in one batch per remote result. The prompt includes
the normal patch and prepared code-review context.

Confirmation returns an exact boolean mapping for every requested finding ID.
Malformed or failed confirmation consumes one of the three immediate
integration attempts. It is never interpreted as acceptance and never returns
the job to hourly polling.

Confirmed remote-only findings are appended to the local persisted findings and
to the structured review output that the web and API surfaces read. Rejected
findings are retained only as comparison evidence. Existing local findings are
never removed.

A remote finding also reaches the inline report, which is a separate artifact
from the structured output. See Report Rendering.

## Merge Budget

Cross-instance analysis uses the same role-based merge configuration as the
initial local integration, but starts a fresh ledger for each invocation:

```text
main and variant discovery
  -> initial merge ledger

remote result arrives later
  -> fresh cross merge ledger
```

The cross merge ledger covers semantic deduplication, remote-only confirmation
and report rendering, including retry attempts. It never consumes a discovery
ledger. Rendering is charged last, so exhausting the ledger degrades report
prose rather than losing a verified finding.
Usage and budget flags are persisted on `cross_review_jobs`; initial stages
8-11 usage is persisted in `review_merge_runs`. Budget exhaustion fails only
the cross-review job and cannot change the already published `Reviewed` result.

Configuration uses `[ai.merge_budget]`, which accepts the same per-stage and
per-run input/output fields as `[ai.budget]` and inherits `[ai.budget]` when
omitted.

Cross-result persistence and report splicing are one fenced transaction.
Rendering happens before that transaction opens, because it is a model call and
the transaction must not hold a write lock across one. Multiple patchsets may
finish concurrently, while remotes for the same patchset generation are
serialized so semantic publication deduplication is stable and so a render
observes the report that its splice will edit.

## Comparison Statistics

Each graph compares the local main instance with one configured remote:

- `both`: matched canonical local and remote finding.
- `local_only`: canonical local finding with no remote match.
- `remote_only`: novel remote finding confirmed by the local main model.
- `remote_hallucination`: novel remote finding rejected by the local main
  model.

Rows are horizontally stacked by critical, high, medium, and low severity.
Remote cost rows are omitted. A local hallucination row is also omitted because
the immutable remote cannot confirm local-only findings.

## Review Source Presentation

Cross-review instances participate in the same presentation manifest as local
model sources, using their configured instance names because the origin/main
API does not guarantee remote model identity. A completed remote counts as a
comparison participant even when it returned no findings. Pending, processing,
errored, and expired remotes are displayed separately and never count as
having missed a finding.

The patchset API exposes current-generation job status per configured remote.
The UI combines those records with the local cohort manifest only for the
newest review shown for a patch. Before all remotes arrive, the review header
shows the pending sources. After each fenced integration transaction completes,
the existing structured finding provenance is authoritative: matched findings
include the remote source and confirmed remote-only findings are published with
that source. Stable finding-ID annotations associate inline comments with this
updated provenance.

## Report Rendering

The inline report is the LKML-style text a review sends and the web UI displays.
Stage 11 writes it during the initial review with review tools and the patch
worktree available. A cross-review result arrives up to three days later, when
that worktree is gone, so stage 11 cannot be re-run for it. Cross-review renders
only the comment block for each newly published remote finding and splices that
block into the existing report.

Rendering is triggered by a non-empty accepted-remote set, which covers both
novel remote findings and remote findings that corroborate a local candidate
stage 10 had rejected. It is not triggered for `remote_hallucination`, for a
`both` match against an already accepted local finding, or for a remote finding
that aliases an earlier import; those cases change provenance only, which the
structured output already carries.

One model call is made per patch, batching that patch's findings, because the
prepared context dominates the prompt. The prepared context stored on the review
interaction is exactly the system prompt stage 11 was given, and it already
carries the patch and the surrounding code, which is why no worktree or baseline
re-apply is needed. The call also receives the patch diff as the authoritative
source of anchor lines and the current report as the house style to match.

Responsibilities are split so that a bad render degrades text but never
correctness:

- The model returns prose and a suggested anchor line, nothing else.
- Rust owns the `[Severity:]`, `[Finding:]` and `[Sources:]` tag lines, so
  untrusted remote text cannot forge a severity, a finding ID or a source
  attribution in the UI's block parser. Tag-shaped text inside returned prose is
  defused, and backticks are stripped because the report template forbids them.
- Rust owns placement. The anchor is resolved against the report's quoted lines,
  then against the diff, using the model's suggestion first and the remote
  finding's own locations as a deterministic fallback. Placement degrades from
  after the quoted hunk holding the anchored line, to after a freshly quoted copy
  of that hunk, to the end of the report.
- Rust owns the display finding ID, derived from the remote finding's content
  hash. The full hash stays the join and idempotency key in the database; the
  report and the stored `finding_ids` array both carry the shortened form, so the
  UI join cannot break and the same finding cannot be spliced twice.
- A report that previously said there were no issues stops saying so.

Within a location, the code snippet is load bearing and the line number is not.
Remote line numbers are routinely a few lines off; the case that motivated this
work pointed at a blank context line three lines above the code it described. A
line number that resolves to a line too short to be distinctive is discarded
rather than trusted, which is also what keeps a finding from anchoring on a bare
brace. The comment is placed after the whole quoted hunk rather than beside the
one anchored line, matching how the local report and LKML replies read; a report
that quotes without snipping keeps the comment beside the anchor instead, so an
untrimmed quote cannot separate a comment from its subject.

Rendering is best effort. A model, budget or transport failure is logged and the
finding is published with the remote's own problem and reasoning text, still
anchored and still tagged. Publication of a verified remote finding never
depends on a model being reachable.

Email already sent by the initial review cannot be edited, so for a patchset that
mailed its review at review time a spliced comment reaches the web and API
surfaces only. A separate follow-up email policy may be added later. An embargoed
patchset is the exception, because it has not mailed anything yet; see
[Local Embargo Independence](#local-embargo-independence).

### If A Full Re-render Is Revisited

Rendering one block was chosen over re-running the whole report because the
existing local prose was written with tool access and a toolless re-render would
rewrite it with less information than it was written with. Provisioning a
worktree and the review tools for cross-review would remove that objection.
Anyone attempting it should know:

- Tool provisioning is a prerequisite, not an optimization. The vendored report
  template requires the quoted diff to be obtained through git tools rather than
  generated from context, and it requires supporting-code lookup that a context
  window alone cannot serve.
- The baseline commit a review was taken against may be garbage collected within
  the three-day cross-review window, so the worktree may not be re-creatable and
  the fallback path still has to exist.
- Re-applying the patch introduces apply failures into a path that currently
  cannot fail that way, and an apply failure must not fail the cross-review job
  or the already published review.
- The per-patch report header that the combined report is assembled from has to
  be reconstructed, because a full re-render replaces the whole per-patch body.
- The re-render must stay outside the fenced transaction, relying on the
  per-generation job serialization that already guarantees a single in-flight
  render per patchset generation.
- A full re-render rewrites text that may already have been mailed. The mailed
  and displayed versions would then diverge in wording, not just in content,
  which is a larger change in behaviour than adding a block.

## Local Embargo Independence

Cross-review originally started only once the local embargo had lifted. That was
incidental to the design rather than required by it, and it stopped being tenable
once the two sides began holding for different lengths of time: a remote instance
on a fixed 24h embargo publishes days before a local dynamic embargo with a long
`max_hold_hours` does. Maintainers who hold an embargo bypass token can already
peek a review early, and what they saw was a merged report missing findings that
had been public for days. Jobs are therefore created when the local review
completes, embargoed or not.

Nothing in the polling or merge machinery needed the local release. The
consequences that did need deciding:

- **Redaction covers the imported half.** The merge writes `findings`,
  `reviews.inline_review` and `ai_interactions.output_raw` -- rows the embargo
  read paths already redact. An anonymous reader sees the embargo banner; a token
  holder sees the merged report. The cross-review rollup
  (`cross_review_status`, `cross_reviewed_at`, and the `cross_review` object on
  the patchset endpoints) is withheld too: a "Cross-reviewed" badge beside a
  withheld review only invites the question of what the peer found.
- **The release email carries confirmed remote findings.** The release query
  reads exactly the rows the merge rewrites, so an embargoed patchset mails the
  merged report and posts a Patchwork check derived from it. This is accepted,
  not incidental. It is asymmetric with a non-embargoed patchset, whose email is
  already gone before any peer answers -- and that asymmetry is inherent to
  emailing at review time.
- **A confirmed remote finding blocks the early-release fast path.** The clean
  patchset predicate counts every finding on a reviewed series, imported ones
  included, so importing one holds the series to its full embargo window. Keeping
  it that way is deliberate: the fast path exists because "no findings" means
  there is nothing to hold, and releasing early would mail a remote finding out
  under a release that claims the series is clean. The review pass runs the
  immediate release *before* enqueueing peer jobs, so the common case is decided
  by ordering rather than by a race.
- **Enqueueing is idempotent.** Two call sites now enqueue the same generation:
  the review pass, and the release pass. The release pass is the backfill -- for a
  patchset that was already reviewed and embargoed before this change shipped and
  so has no jobs at all, and for a generation whose three-day deadline expired
  while a week-long embargo was still running. Re-enqueueing a recorded generation
  must not walk a completed cross-review back to pending or move its completion
  time to the release, so the rollup is derived from the job rows that exist
  rather than assumed. The release path reports success when it merely declines an
  ineligible embargo claim, so the enqueue cannot be made conditional on it.
- **The peer's embargo is still honoured.** The client is an unauthenticated GET
  and a remote reporting `Embargoed` stays retryable. Only the local embargo
  stopped being a gate.
- **The deadline now runs from review completion, not release.** A peer that is
  unreachable throughout loses the local embargo as extra window, which the
  expired-generation re-arm in the release pass exists to offset.
- **`/api/stats/cross-reviews` is deliberately not redacted.** It has no patchset
  filter, so embargoed patchsets now contribute to its global outcome-by-severity
  histogram. No stats query filters on embargo, and the response carries no
  patchset identity, message ID or problem text.
- **Merge timing affects the release email.** The release pass runs in the service
  loop while merges run as detached tasks. A merge committing after the release
  has read its rows is omitted from that email and its Patchwork check, with no
  later publication path -- the same outcome as a post-release merge today, but
  reached by timing rather than by construction.

## Failure Isolation

Cross-review begins only after the local review has completed and never changes
the result of that review. Remote network, protocol, deduplication, or model
confirmation failures update only cross-review state. They do not change the
patchset or review status from `Reviewed` to `Failed`, and they never hold up a
local publication or embargo release.

## Security

- Remote URLs are operator configuration, never request input.
- Redirects are bounded and must remain on the configured origin.
- Response bodies are streamed through a per-import aggregate byte limit
  before JSON parsing; pagination shares that limit.
- Request timeouts prevent scheduler starvation.
- Remote strings are treated as untrusted data and validated before storage or
  prompt construction.
- Report rendering turns untrusted remote text into text that is displayed and,
  for an embargoed patchset or on a later review, mailed. The render prompt
  forbids following instructions found in finding or patch text, and the tag
  lines that drive severity, finding identity and source attribution are emitted
  by Rust rather than by the model,
  so a hostile remote cannot escalate its own severity or impersonate another
  source. Returned prose has tag-shaped text defused and is length bounded.
- The remote API has no peer authentication in origin/main, so embargoed
  results are polled until publicly available.

## Testing

- Configuration validation for names and URLs.
- Origin/main response fixtures for reviewed, empty, embargoed, and failed
  patchsets.
- Parser rejection of malformed patch/review identity mappings.
- Filtering of pre-existing findings.
- Durable claim, lease recovery, hourly retry, and three-day expiry tests.
- Restart simulation using database state only.
- Idempotent repeated import.
- Deduplication against accepted and rejected local canonical findings.
- Exact batched confirmation response validation and failure fallback.
- Multiple-source aggregate status tests.
- Severity comparison query and web rendering tests.
- Migration tests against a database created before this feature.
