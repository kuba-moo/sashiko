# Findings and Model Usage Statistics

The statistics page reports how review stages contribute to final findings over
the current UTC day and preceding 13 UTC calendar days. A finding with one entry
in its `source_stages` JSON array is unique to that stage. A finding with
multiple entries is a duplicate, and is counted once for every distinct
contributing stage. Rows without valid, non-empty stage provenance are omitted
because they cannot be attributed accurately.

The timeline statistics endpoint performs the aggregation so subsystem filters
have the same behavior as the other findings statistics. The UI renders the
result as a horizontal stacked bar chart with unique and duplicate segments.

Monthly LLM usage is derived in the UI from the existing daily per-model cost
feed. Input and output tokens are summed by UTC calendar month and model, then
rendered as stacked bars. Cached input is already included in `tokens_in`, so it
is not added a second time. Calendar keys include the year to keep data from
different years separate.
