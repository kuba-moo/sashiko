# Cross-Instance Reviews

## Status

Proposed and approved for implementation.

## Problem

One Sashiko deployment may review the same patch series as another deployment
using a different model. After the local review is published, the local
instance should import the remote deployment's published findings, compare
them with the local canonical findings, validate novel remote findings, and
update the locally rendered result.

The remote deployment is not under our control. It is assumed to run Sashiko
from origin/main and cannot be changed to expose a purpose-built peer API.

## Goals

- Configure zero or more named remote Sashiko instances.
- Start cross-review work only after the normal local review is published.
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
- Editing email that has already been sent.
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
local review published
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

Confirmed remote-only findings are appended to the local persisted findings
and included in regenerated web and API output. Rejected findings are retained
only as comparison evidence. Existing local findings are never removed.

Email already sent by the initial review cannot be edited. The initial
implementation updates database-backed web and API rendering only; a separate
follow-up email policy may be added later.

## Merge Budget

Cross-instance analysis uses the same role-based merge configuration as the
initial local integration, but starts a fresh ledger for each invocation:

```text
main and variant discovery
  -> initial merge ledger

remote result arrives later
  -> fresh cross merge ledger
```

The cross merge ledger covers semantic deduplication and remote-only
confirmation, including retry attempts. It never consumes a discovery ledger.
Usage and budget flags are persisted on `cross_review_jobs`; initial stages
8-11 usage is persisted in `review_merge_runs`. Budget exhaustion fails only
the cross-review job and cannot change the already published `Reviewed` result.

Configuration uses `[ai.merge_budget]`, which accepts the same per-stage and
per-run input/output fields as `[ai.budget]` and inherits `[ai.budget]` when
omitted.

Cross-result persistence and rerendering are one fenced transaction. Multiple
patchsets may finish concurrently, while remotes for the same patchset
generation are serialized so semantic publication deduplication is stable.

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

## Failure Isolation

Cross-review begins only after normal publication and never changes the result
of the initial local review. Remote network, protocol, deduplication, or model
confirmation failures update only cross-review state. They do not change the
patchset or review status from `Reviewed` to `Failed`.

## Security

- Remote URLs are operator configuration, never request input.
- Redirects are bounded and must remain on the configured origin.
- Response bodies are streamed through a per-import aggregate byte limit
  before JSON parsing; pagination shares that limit.
- Request timeouts prevent scheduler starvation.
- Remote strings are treated as untrusted data and validated before storage or
  prompt construction.
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
