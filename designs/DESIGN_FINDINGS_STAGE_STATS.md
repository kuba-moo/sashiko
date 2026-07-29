# Findings and Model Usage Statistics

The statistics page reports how review stages contribute to final findings over
the current UTC day and preceding 13 UTC calendar days. A finding with one entry
in its `source_stages` JSON array is unique to that stage. A finding with
multiple entries is a duplicate, and is counted once for every distinct
contributing stage. Rows without valid, non-empty stage provenance are omitted
because they cannot be attributed accurately.

A hallucination is a rejected canonical candidate whose persisted
`source_models` includes the main model. It is attributed once to every
distinct stage in its `source_stages`. Rejected variant-only candidates are
excluded from this main-model chart.

The timeline statistics endpoint performs the aggregation so subsystem filters
have the same behavior as the other findings statistics. It also reports the
number of distinct reviews in which the main model engaged each discovery
stage. Variant-model runs are excluded so experiments do not dilute the rate.
The UI divides all three category counts by these engagements and renders the
result as a horizontal stacked bar chart with unique-per-run,
duplicate-per-run, and hallucination-per-run segments. Tooltips retain the raw
counts and engagement totals. Legend labels show each category's share of all
raw stage-attributed counts in the graph. Starting from stage engagements keeps
stages that produced no findings visible.

A second per-stage-run chart considers only accepted findings with exactly one
source stage, excluding duplicates and validation-rejected hallucinations. It
stacks those unique findings by critical, high, medium, and low severity. Its
tooltips show both the normalized severity rate and the underlying raw count.

Monthly LLM cost is derived in the UI from the existing daily per-model usage
feed and the shared model pricing table. Estimated costs are summed by UTC
calendar month and model, then rendered as stacked dollar-value bars. Cached
input receives its configured discounted rate through the common estimator.
Calendar keys include the year to keep data from different years separate.
