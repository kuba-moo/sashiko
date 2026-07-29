# Live Model Experiments

## Goals

Sashiko can enroll additional models once per patch review and compare their
discoveries with the main model. An enrolled model mirrors every analytical
stage selected for the main model. Experimental traffic must not change the
main review's stage selection, budget state, retry behavior, or result when an
experiment cannot complete.

The pipeline treats model output as a discovery source rather than embedding
experiment policy in review stages. This boundary also permits a future source
to supply already-deduplicated discoveries from another Sashiko instance.

## Pipeline

```text
cohort selection
      |
      v
source execution (main and enrolled variants)
      |
      v
normalized discoveries with immutable source-owned IDs
      |
      v
cross-source reconciliation and provenance validation
      |
      +----------------------+
      |                      |
      v                      v
baseline verification   experiment validation plan
      |                      |
      +----------+-----------+
                 v
          publication policy
                 |
                 v
             rendering
```

Stages 1 through 7 produce discoveries. Stages 8 and 9 reconcile semantically
equivalent discoveries into canonical findings. Stage 10 supplies the normal
main-model baseline decision. Experiment validation is a separate batched
operation. Stage 11 renders findings selected by publication policy.

## Cohort And Sources

Variant enrollment is deterministic over the patch review identity and
experiment name. The worker materializes the result before constructing stage
work:

```text
ReviewCohort
  main: Source
  variants: [CohortMember]

CohortMember
  source: Source
  enrollment: selected | not-selected
  execution: pending | running | completed | failed
```

Enrollment is never inferred from stage runs. A selected source remains part of
the cohort when provider initialization or an individual stage fails. This
allows statistics to distinguish selection, availability, and model behavior.

A source has a stable experiment name, provider identity, model identity, and
origin. Initially origins are `main` and `variant`. Future external review
results can add another origin without changing reconciliation or validation.

## Discoveries And Provenance

Every Stage 1 through 7 concern becomes a source-owned discovery with a stable
ID. Models do not create or mutate IDs. Reconciliation output references the
IDs of the discoveries it combines. Sashiko derives source and stage provenance
from those references.

```text
Discovery
  id
  source
  source_stage
  content

CanonicalFinding
  id
  content
  contributions: [DiscoveryId]
```

The reconciliation validator requires every input discovery to occur exactly
once in the output contribution sets. It rejects unknown, duplicated, or lost
IDs. JSON provenance fields remain an AI boundary representation, not the
internal authority.

The current implementation assigns immutable IDs in Sashiko and validates the
complete ID-to-source mapping inside the retry loop, but still carries findings
as JSON values between worker stages. Moving those validated values into typed
`Discovery` and `CanonicalFinding` structures is required before accepting
external discovery batches.

## Baseline And Experiment Decisions

Baseline verification and experiment validation are distinct decisions:

```text
FindingDecision
  baseline: accepted | rejected | not-applicable
  experiment: shared | confirmed | rejected | unavailable
  publication: accepted | rejected
```

The experiment validation planner applies these rules:

| Discovery sources | Validation |
|---|---|
| main and one or more variants | Shared; no confirmation |
| main only | One eligible variant confirms |
| one variant only | Main confirms |
| multiple variants, no main | Shared; no confirmation |

All findings assigned to one confirmer are sent in one request with the normal
patch and pre-fetched context. The response must contain one boolean for every
requested finding ID and no unknown IDs.

Provider errors, malformed output, missing decisions, and exhausted validation
budgets produce `unavailable`, never `rejected`. Publication then falls back as
follows:

* a main-only finding uses its normal baseline decision;
* a variant-only finding is not published without main confirmation;
* a shared finding is accepted.

Successful experiment decisions may confirm or reject unique findings. This
keeps experiment policy explicit while guaranteeing that experiment failure
does not weaken normal false-positive filtering.

## Budget Ownership

Every source owns an independent discovery ledger for stages 1 through 7.
Stages 8 through 11 use a separate merge ledger. Confirmation performed while
merging local model results is charged to that merge ledger rather than to a
discovering source.

```text
patch review
  main discovery ledger
  variant A discovery ledger
  variant B discovery ledger
  initial merge ledger
```

The main discovery ledger therefore cannot steer merge retry behavior, and
variant usage cannot consume either the main discovery or merge budget.
Additional merge invocations, such as later cross-instance integration, each
start a fresh ledger from the same merge configuration.

The current implementation records aggregate usage for each discovery stage and
each batched validation run. Retry usage is included when the provider reports
it. Cached input remains part of context usage and is also recorded separately
for pricing. Request- and attempt-level ledger persistence remains future work.

The review subprocess aggregation boundary preserves private per-patch merge
metadata alongside the public findings. In particular, canonical candidates,
experiment runs and comparisons, and the merge-ledger snapshot must reach the
parent reviewer unchanged for a single-patch production invocation. Failed
merge attempts return a structured ledger snapshot as well; a later successful
retry accumulates prior attempt usage, while a terminal failure persists the
snapshot on the failed review.

Each request checks its input estimate against both the source's stage limit and
the remaining review input limit before dispatch. Completion atomically records
actual input and output usage, then checks stage and review limits. Discovery
stages run in ordered batches controlled by `analysis_stage_parallelism`, so
completed usage and warning state steer the next batch. A source can exhaust
only its own ledger. Variant exhaustion fails or cancels that variant's work;
validation exhaustion produces an unavailable decision; neither changes the
main discovery ledger.

Existing flat budget settings remain accepted. New nested source budget values
map directly to stage and review input/output limits. Explicit review limits
supersede the legacy review multiplier when both are present.

`[ai.merge_budget]` accepts the same stage and review fields as `[ai.budget]`.
When omitted it inherits `[ai.budget]`, preserving existing configurations.

## Configuration

```toml
[ai]
name = "opus"

[[ai.additional_models]]
name = "sonnet"
probability = 0.10
model = "claude-sonnet-4-6"

[ai.additional_models.budget]
stage_input_tokens = 120000
stage_output_tokens = 6000
review_input_tokens = 480000
review_output_tokens = 24000

[ai.merge_budget]
stage_input_tokens = 150000
stage_output_tokens = 6000
review_input_tokens = 450000
review_output_tokens = 18000

[ai.model_experiments.validation_budget]
request_input_tokens = 150000
request_output_tokens = 4000
review_input_tokens = 300000
review_output_tokens = 12000
```

Additional-model provider and generation settings continue to inherit from the
main AI configuration. Names contain only ASCII letters, digits, `_`, or `-`;
`main` is reserved. Probability is inclusive from 0.0 through 1.0.

## Persistence

The target persistence model is:

```text
review sources and enrollment
  -> discovery stage runs
  -> canonical finding contributions
  -> validation runs and decisions
  -> publication decisions
```

The current implementation transactionally persists cohort membership, stage
runs, comparison outcomes, and validation runs using the existing experiment
tables plus `model_experiment_sources`. Canonical contributions and separate
baseline, experiment, and publication decision records remain future work.
Provider and model identity are both part of comparison identity.

Comparison cost averages include only stage numbers completed successfully by
both sources. Confirmation cost is reported separately and never folded into
discovery cost. Unknown pricing produces a null cost rather than zero.

The live comparison charts group canonical findings into outcome buckets and
stack each bucket by severity. Each tooltip count includes its share of all
findings at that severity across the displayed outcome buckets. Each outcome
label includes the bucket's share of all displayed findings across critical,
high, medium, and low severities. Empty denominators display as zero percent.

## Finding Presentation

Every generated inline comment carries machine-readable finding-ID and source
annotations copied from the canonical finding's `finding_ids` and
`source_models` provenance. They are an association mechanism, not the final
presentation authority. Rust derives a source-participation manifest from the
cohort and completed discovery runs, and the web UI joins each comment to the
latest structured finding by ID.

The review header lists completed sources, unsampled variants, and incomplete
sources. A finding-level annotation is hidden when every completed source
discovered the finding. On disagreement it identifies the discovering and
missing completed sources, plus the confirming source when confirmation was
required. Unsampled and incomplete sources never count as misses. The internal
`main` sentinel is presented as the configured `[ai].name`; variants use their
configured experiment names.

The same presentation manifest accepts completed cross-instance sources. This
keeps comments accurate when cross-review adds findings or provenance after
Stage 11 originally rendered the local review. Older comments without a
finding-ID annotation retain their stored local source annotation as a
compatibility fallback. Cross-instance sources are excluded from their
finding-level miss calculation because no stable association exists, while the
review-level cross-source status remains visible.

## Compatibility

An absent or empty additional-model list preserves current execution and output.
Existing flat budget settings and database contents remain readable. Startup
migrations create additive objects and columns idempotently. Experimental
failure falls back to baseline behavior rather than failing or broadening the
production review.
