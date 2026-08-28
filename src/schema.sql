-- Copyright 2026 The Sashiko Authors
--
-- Licensed under the Apache License, Version 2.0 (the "License");
-- you may not use this file except in compliance with the License.
-- You may obtain a copy of the License at
--
--     https://www.apache.org/licenses/LICENSE-2.0
--
-- Unless required by applicable law or agreed to in writing, software
-- distributed under the License is distributed on an "AS IS" BASIS,
-- WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
-- See the License for the specific language governing permissions and
-- limitations under the License.

CREATE TABLE IF NOT EXISTS mailing_lists (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    nntp_group TEXT NOT NULL UNIQUE,
    last_article_num INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS subsystems (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    mailing_list_address TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS threads (
    id INTEGER PRIMARY KEY,
    root_message_id TEXT,
    subject TEXT,
    last_updated INTEGER
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    message_id TEXT NOT NULL UNIQUE,
    thread_id INTEGER,
    in_reply_to TEXT,
    author TEXT,
    subject TEXT,
    date INTEGER,
    body TEXT,
    to_recipients TEXT,
    cc_recipients TEXT,
    git_blob_hash TEXT,
    mailing_list TEXT,
    references_hdr TEXT,
    FOREIGN KEY(thread_id) REFERENCES threads(id)
);

CREATE TABLE IF NOT EXISTS baselines (
    id INTEGER PRIMARY KEY,
    repo_url TEXT,
    branch TEXT,
    last_known_commit TEXT
);

CREATE TABLE IF NOT EXISTS patchsets (
    id INTEGER PRIMARY KEY,
    thread_id INTEGER,
    cover_letter_message_id TEXT,
    subject TEXT,
    author TEXT,
    date INTEGER,
    status TEXT DEFAULT 'Incomplete', -- Incomplete, Pending, In Review, Cancelled, Reviewed, Failed
    total_parts INTEGER,
    received_parts INTEGER,
    subject_index INTEGER DEFAULT 9999,
    parser_version INTEGER DEFAULT 0,
    to_recipients TEXT,
    cc_recipients TEXT,
    baseline_id INTEGER,
    model_name TEXT,
    prompts_git_hash TEXT,
    baseline_logs TEXT,
    failed_reason TEXT,
    skip_filters TEXT,
    only_filters TEXT,
    target_review_count INTEGER DEFAULT 1,
    provider TEXT,
    embargo_until INTEGER,
    embargo_release_started_at INTEGER,
    cross_review_status TEXT NOT NULL DEFAULT 'disabled',
    cross_reviewed_at INTEGER,
    cross_review_generation INTEGER NOT NULL DEFAULT 0,
    slug TEXT, -- URL-friendly slug like "reponame-725" (repo-mrnum)
    FOREIGN KEY(thread_id) REFERENCES threads(id),
    FOREIGN KEY(cover_letter_message_id) REFERENCES messages(message_id),
    FOREIGN KEY(baseline_id) REFERENCES baselines(id)
);

CREATE INDEX IF NOT EXISTS idx_patchsets_status ON patchsets(status);


CREATE TABLE IF NOT EXISTS patches (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    message_id TEXT NOT NULL UNIQUE,
    part_index INTEGER,
    diff TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(message_id) REFERENCES messages(message_id)
);

CREATE TABLE IF NOT EXISTS reviews (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    patch_id INTEGER, -- Optional link to specific patch
    summary TEXT,
    result_description TEXT,
    created_at INTEGER,
    completed_at INTEGER,
    interaction_id TEXT,
    status TEXT DEFAULT 'Pending', -- Pending, In Review, Cancelled, Reviewed, Failed
    logs TEXT,
    inline_review TEXT,
    baseline_id INTEGER,
    model TEXT,
    prompts_hash TEXT,
    provider TEXT,
    budget_flags INTEGER DEFAULT 0,
    concerns_total INTEGER,
    concerns_unique INTEGER,
    findings_multi_stage INTEGER,
    -- ok, disabled, setup_failed, or tools_refused:<n>: whether semcode tools were
    -- available AND answering. The last two are distinct because a stale index
    -- copies and indexes cleanly, then refuses every read.
    semcode_status TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(patch_id) REFERENCES patches(id),
    FOREIGN KEY(interaction_id) REFERENCES ai_interactions(id),
    FOREIGN KEY(baseline_id) REFERENCES baselines(id)
);

CREATE TABLE IF NOT EXISTS findings (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    severity INTEGER NOT NULL, -- 1: Low, 2: Medium, 3: High, 4: Critical
    severity_explanation TEXT,
    problem TEXT,
    suggestion TEXT,
    preexisting INTEGER, -- 0 = false, 1 = true
    locations TEXT,
    source_stages TEXT,
    cross_review_job_id INTEGER,
    external_finding_id TEXT,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_findings_review_id ON findings(review_id);
CREATE INDEX IF NOT EXISTS idx_findings_severity ON findings(severity);

CREATE TABLE IF NOT EXISTS ai_interactions (
    id TEXT PRIMARY KEY,
    parent_interaction_id TEXT,
    workflow_id TEXT,
    provider TEXT,
    model TEXT,
    input_context TEXT,
    output_raw TEXT,
    tokens_in INTEGER,
    tokens_out INTEGER,
    tokens_cached INTEGER,
    created_at INTEGER
);

CREATE TABLE IF NOT EXISTS messages_subsystems (
    message_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (message_id, subsystem_id),
    FOREIGN KEY(message_id) REFERENCES messages(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE TABLE IF NOT EXISTS threads_subsystems (
    thread_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (thread_id, subsystem_id),
    FOREIGN KEY(thread_id) REFERENCES threads(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE TABLE IF NOT EXISTS patches_subsystems (
    patch_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (patch_id, subsystem_id),
    FOREIGN KEY(patch_id) REFERENCES patches(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE TABLE IF NOT EXISTS patchsets_subsystems (
    patchset_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (patchset_id, subsystem_id),
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE INDEX IF NOT EXISTS idx_patchsets_cover_message_id ON patchsets(cover_letter_message_id);

CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);
CREATE INDEX IF NOT EXISTS idx_patches_patchset_id ON patches(patchset_id);
CREATE INDEX IF NOT EXISTS idx_messages_date ON messages(date);

CREATE INDEX IF NOT EXISTS idx_messages_day ON messages(strftime('%Y-%m-%d', date, 'unixepoch'));
CREATE INDEX IF NOT EXISTS idx_patchsets_day ON patchsets(strftime('%Y-%m-%d', date, 'unixepoch'));
CREATE INDEX IF NOT EXISTS idx_messages_subsystems_sid ON messages_subsystems(subsystem_id);
CREATE INDEX IF NOT EXISTS idx_patchsets_subsystems_sid ON patchsets_subsystems(subsystem_id);

CREATE TABLE IF NOT EXISTS people (
    id INTEGER PRIMARY KEY,
    name TEXT,
    email TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS messages_recipients (
    message_id INTEGER NOT NULL,
    person_id INTEGER NOT NULL,
    recipient_type TEXT NOT NULL, -- 'To', 'Cc'
    PRIMARY KEY (message_id, person_id),
    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
    FOREIGN KEY(person_id) REFERENCES people(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS messages_mailing_lists (
    message_id INTEGER NOT NULL,
    mailing_list_id INTEGER NOT NULL,
    PRIMARY KEY (message_id, mailing_list_id),
    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
    FOREIGN KEY(mailing_list_id) REFERENCES mailing_lists(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tool_usages (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    provider TEXT,
    model TEXT,
    tool_name TEXT,
    arguments TEXT,
    output_length INTEGER,
    created_at INTEGER,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_tool_usages_review ON tool_usages(review_id);

CREATE TABLE IF NOT EXISTS email_outbox (
    id INTEGER PRIMARY KEY,
    patch_id INTEGER,
    status TEXT DEFAULT 'Pending',
    to_addresses TEXT,
    cc_addresses TEXT,
    subject TEXT,
    in_reply_to TEXT,
    references_hdr TEXT,
    body TEXT,
    locked_at INTEGER,
    error_log TEXT,
    created_at INTEGER,
    FOREIGN KEY(patch_id) REFERENCES patches(id)
);
CREATE INDEX IF NOT EXISTS idx_email_outbox_status ON email_outbox(status);

CREATE INDEX IF NOT EXISTS idx_ai_interactions_tokens ON ai_interactions(id, tokens_in, tokens_out, tokens_cached);
CREATE INDEX IF NOT EXISTS idx_reviews_grouping ON reviews(provider, model, status, interaction_id);
CREATE INDEX IF NOT EXISTS idx_tool_usages_stats ON tool_usages(provider, model, tool_name, output_length);

CREATE TABLE IF NOT EXISTS model_experiment_runs (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    experiment_name TEXT NOT NULL,
    model_id TEXT NOT NULL DEFAULT '',
    provider_id TEXT NOT NULL DEFAULT '',
    stage INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'completed',
    error TEXT,
    tokens_in INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    tokens_cached INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_model_experiment_runs_review ON model_experiment_runs(review_id);

CREATE TABLE IF NOT EXISTS model_experiment_sources (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    experiment_name TEXT NOT NULL,
    provider_id TEXT NOT NULL DEFAULT '',
    model_id TEXT NOT NULL DEFAULT '',
    selected INTEGER NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    FOREIGN KEY(review_id) REFERENCES reviews(id),
    UNIQUE(review_id, experiment_name)
);
CREATE INDEX IF NOT EXISTS idx_model_experiment_sources_review ON model_experiment_sources(review_id);

CREATE TABLE IF NOT EXISTS model_experiment_findings (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    additional_model TEXT NOT NULL,
    main_model_id TEXT NOT NULL DEFAULT '',
    additional_model_id TEXT NOT NULL DEFAULT '',
    main_provider_id TEXT NOT NULL DEFAULT '',
    additional_provider_id TEXT NOT NULL DEFAULT '',
    finding_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    severity TEXT,
    confirmed_by TEXT,
    preexisting INTEGER, -- 0 = false, 1 = true, NULL = unknown (see findings.preexisting)
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_model_experiment_findings_review ON model_experiment_findings(review_id);

CREATE TABLE IF NOT EXISTS model_confirmation_runs (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    model TEXT NOT NULL,
    model_id TEXT NOT NULL DEFAULT '',
    provider_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'completed',
    error TEXT,
    tokens_in INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    tokens_cached INTEGER NOT NULL DEFAULT 0,
    budget_input INTEGER NOT NULL DEFAULT 0,
    budget_output INTEGER NOT NULL DEFAULT 0,
    budget_flags INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_model_confirmation_runs_review ON model_confirmation_runs(review_id);

CREATE TABLE IF NOT EXISTS cross_review_jobs (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    source_name TEXT NOT NULL,
    source_url TEXT NOT NULL,
    local_model TEXT NOT NULL DEFAULT '',
    local_provider TEXT NOT NULL DEFAULT '',
    lookup_message_id TEXT NOT NULL,
    fallback_message_id TEXT,
    generation INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    first_attempt_at INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL,
    deadline_at INTEGER NOT NULL,
    lease_until INTEGER,
    lease_token TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    completed_at INTEGER,
    remote_model TEXT,
    remote_provider TEXT,
    payload_hash TEXT,
    merge_tokens_in INTEGER NOT NULL DEFAULT 0,
    merge_tokens_out INTEGER NOT NULL DEFAULT 0,
    merge_tokens_cached INTEGER NOT NULL DEFAULT 0,
    merge_budget_flags INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    UNIQUE(patchset_id, generation, source_name)
);
CREATE INDEX IF NOT EXISTS idx_cross_review_jobs_due
    ON cross_review_jobs(status, next_attempt_at, lease_until);

CREATE TABLE IF NOT EXISTS cross_review_findings (
    id INTEGER PRIMARY KEY,
    job_id INTEGER NOT NULL,
    finding_id TEXT NOT NULL,
    patch_message_id TEXT NOT NULL,
    finding_json TEXT NOT NULL,
    accepted INTEGER,
    FOREIGN KEY(job_id) REFERENCES cross_review_jobs(id),
    UNIQUE(job_id, finding_id)
);
CREATE INDEX IF NOT EXISTS idx_cross_review_findings_job
    ON cross_review_findings(job_id);

CREATE TABLE IF NOT EXISTS cross_review_comparisons (
    id INTEGER PRIMARY KEY,
    job_id INTEGER NOT NULL,
    finding_id TEXT NOT NULL,
    matched_finding_id TEXT,
    outcome TEXT NOT NULL,
    severity TEXT,
    FOREIGN KEY(job_id) REFERENCES cross_review_jobs(id),
    UNIQUE(job_id, finding_id, outcome)
);
CREATE INDEX IF NOT EXISTS idx_cross_review_comparisons_job
    ON cross_review_comparisons(job_id);

CREATE TABLE IF NOT EXISTS local_canonical_findings (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    finding_id TEXT NOT NULL,
    finding_json TEXT NOT NULL,
    accepted INTEGER NOT NULL,
    FOREIGN KEY(review_id) REFERENCES reviews(id),
    UNIQUE(review_id, finding_id)
);
CREATE INDEX IF NOT EXISTS idx_local_canonical_findings_review
    ON local_canonical_findings(review_id);

CREATE TABLE IF NOT EXISTS review_merge_runs (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL UNIQUE,
    tokens_in INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    tokens_cached INTEGER NOT NULL DEFAULT 0,
    budget_flags INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);

CREATE INDEX IF NOT EXISTS idx_patchsets_date ON patchsets(date DESC);
CREATE INDEX IF NOT EXISTS idx_reviews_patchset_status ON reviews(patchset_id, status);
CREATE INDEX IF NOT EXISTS idx_reviews_day ON reviews(strftime('%Y-%m-%d', created_at, 'unixepoch'), status);
CREATE INDEX IF NOT EXISTS idx_email_outbox_patch_id ON email_outbox(patch_id);

CREATE TABLE IF NOT EXISTS patchwork_outbox (
    id INTEGER PRIMARY KEY,
    patch_msg_id TEXT NOT NULL,
    api_url TEXT NOT NULL,
    check_state TEXT NOT NULL,
    description TEXT NOT NULL,
    target_url TEXT NOT NULL,
    context TEXT NOT NULL DEFAULT 'sashiko',
    status TEXT DEFAULT 'Pending',
    retry_count INTEGER DEFAULT 0,
    next_retry_at INTEGER,
    locked_at INTEGER,
    error_log TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_patchwork_outbox_status ON patchwork_outbox(status);

-- Patch state observed on patchwork, pushed in by the nipa poller.  One row per
-- patch we reviewed; patches patchwork knows about but we never reviewed are
-- dropped at ingest rather than stored.  'state' is the raw patchwork slug and
-- is authoritative, 'outcome' is the derived bucket (see state_outcome() in
-- src/patchwork.rs) so re-bucketing is a plain UPDATE.
CREATE TABLE IF NOT EXISTS patchwork_patch_state (
    patch_id INTEGER PRIMARY KEY,
    pw_patch_id INTEGER,
    pw_series_id INTEGER,
    state TEXT NOT NULL,
    outcome TEXT NOT NULL,
    previous_state TEXT,
    initial_state TEXT,
    actor TEXT,
    state_changed_at INTEGER,
    last_event_id INTEGER,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(patch_id) REFERENCES patches(id)
);
CREATE INDEX IF NOT EXISTS idx_patchwork_patch_state_outcome ON patchwork_patch_state(outcome);

-- Single-row watermark for the poller's patch-state-changed event sweep.  The
-- date is fed back to patchwork as the sweep's "since" parameter.
CREATE TABLE IF NOT EXISTS patchwork_sync (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last_event_date TEXT,
    last_event_id INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS json_decode_events (
    id INTEGER PRIMARY KEY,
    review_id INTEGER,
    source TEXT NOT NULL,
    outcome TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_json_decode_events_day ON json_decode_events(created_at);
