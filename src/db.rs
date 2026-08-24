// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::ReviewStatus;
use crate::settings::DatabaseSettings;
use anyhow::Result;
use libsql::Builder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

pub struct Database {
    database: libsql::Database,
    is_in_memory: bool,
    in_memory_transaction: tokio::sync::Mutex<()>,
    pub conn: libsql::Connection,
}

#[derive(Debug, Clone)]
pub struct CrossReviewJob {
    pub id: i64,
    pub patchset_id: i64,
    pub source_name: String,
    pub source_url: String,
    pub local_model: String,
    pub local_provider: String,
    pub lookup_message_id: String,
    pub fallback_message_id: Option<String>,
    pub generation: i64,
    pub lease_token: String,
    pub deadline_at: i64,
    pub attempts: i64,
}

/// Per-patch inputs for rendering an accepted remote finding into its report.
#[derive(Debug, Clone, Default)]
pub struct CrossRenderInput {
    /// The prepared context stage 11 was given, which already carries the patch
    /// and the code it touches. This is why the render needs no worktree.
    pub context: String,
    /// The patch as applied, used as the authoritative source of anchor lines.
    pub diff: String,
    /// The report as it stands, handed to the model as the house style to match.
    pub inline_review: String,
}

fn append_unique_string(value: &mut serde_json::Value, item: &str) {
    if !value.is_array() {
        *value = serde_json::Value::Array(Vec::new());
    }
    let Some(values) = value.as_array_mut() else {
        return;
    };
    if !values.iter().any(|value| value.as_str() == Some(item)) {
        values.push(serde_json::Value::String(item.to_string()));
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Subsystem {
    pub id: i64,
    pub name: String,
    pub mailing_list_address: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PatchsetRow {
    pub id: i64,
    pub subject: Option<String>,
    pub status: Option<String>,
    pub thread_id: Option<i64>,
    pub author: Option<String>,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub total_parts: Option<u32>,
    pub received_parts: Option<u32>,
    pub subsystems: Vec<String>,
    pub findings_low: Option<i64>,
    pub findings_medium: Option<i64>,
    pub findings_high: Option<i64>,
    pub findings_critical: Option<i64>,
    pub baseline_id: Option<i64>,
    pub slug: Option<String>,
    pub failed_reason: Option<String>,
    pub skip_filters: Option<String>,
    pub only_filters: Option<String>,
    pub target_review_count: Option<u32>,
    pub model_name: Option<String>,
    pub prompts_git_hash: Option<String>,
    pub baseline_logs: Option<String>,
    pub provider: Option<String>,
    #[serde(skip)]
    pub embargo_until: Option<i64>,
    pub mr_url: Option<String>,
    pub mr_title: Option<String>,
    pub mr_number: Option<i64>,
    pub concerns_total: Option<i64>,
    pub concerns_unique: Option<i64>,
    pub findings_multi_stage: Option<i64>,
    pub budget_flags_or: Option<i64>,
    pub cross_review_status: Option<String>,
    pub cross_reviewed_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ReleaseReview {
    pub patch_id: i64,
    pub patch_message_id: String,
    pub index: i64,
    pub inline_review: String,
    pub summary: String,
    pub findings: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchsetReviewOutcome {
    Clean,
    HasFindings,
    Incomplete,
}

const CLEAN_PATCHSET_PREDICATE: &str = "
    EXISTS (
        SELECT 1 FROM reviews r
        WHERE r.patchset_id = p.id AND r.status = 'Reviewed'
    )
    AND NOT EXISTS (
        SELECT 1 FROM reviews r
        WHERE r.patchset_id = p.id AND r.status = 'Skipped'
          AND r.result_description = 'Skipped AI review via --no-ai'
    )
    AND NOT EXISTS (
        SELECT 1 FROM patches pa
        WHERE pa.patchset_id = p.id
          AND COALESCE(pa.status, '') != 'Skipped'
          AND NOT EXISTS (
              SELECT 1 FROM reviews skipped
              WHERE skipped.patch_id = pa.id
                AND skipped.status = 'Skipped'
                AND skipped.result_description = 'Skipped: touches only ignored files'
          )
          AND (
              SELECT COUNT(*) FROM reviews completed
              WHERE completed.patch_id = pa.id
                AND completed.status = 'Reviewed'
          ) < COALESCE(p.target_review_count, 1)
    )
    AND NOT EXISTS (
        SELECT 1 FROM reviews r
        JOIN findings f ON f.review_id = r.id
        WHERE r.patchset_id = p.id AND r.status = 'Reviewed'
    )";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub message_id: String,
    pub thread_id: Option<i64>,
    pub in_reply_to: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub date: Option<i64>,
    pub body: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub thread: Option<Vec<serde_json::Value>>,
    pub git_blob_hash: Option<String>,
    pub mailing_list: Option<String>,
    pub diff: Option<String>,
    pub references_hdr: Option<String>,
}

pub struct AiInteractionParams<'a> {
    pub id: &'a str,
    pub parent_id: Option<&'a str>,
    pub workflow_id: Option<&'a str>,
    pub provider: &'a str,
    pub model: &'a str,
    pub input: &'a str,
    pub output: &'a str,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolUsage {
    pub review_id: i64,
    pub provider: String,
    pub model: String,
    pub tool_name: String,
    pub arguments: Option<String>,
    pub output_length: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
}

impl Severity {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        let s = s.trim();
        if s.eq_ignore_ascii_case("critical") {
            Severity::Critical
        } else if s.to_lowercase().starts_with("high") {
            Severity::High
        } else if s.to_lowercase().starts_with("medium") {
            Severity::Medium
        } else {
            Severity::Low
        }
    }
}

pub struct Finding {
    pub review_id: i64,
    pub severity: Severity,
    pub severity_explanation: Option<String>,
    pub problem: String,
    pub preexisting: Option<bool>,
    pub locations: Option<serde_json::Value>,
    pub source_stages: Option<String>,
}

pub struct EmailOutboxRow {
    pub id: i64,
    pub patch_id: Option<i64>,
    pub status: String,
    pub to_addresses: String,
    pub cc_addresses: String,
    pub subject: String,
    pub in_reply_to: String,
    pub references_hdr: String,
    pub body: String,
    pub locked_at: Option<i64>,
    pub error_log: Option<String>,
    pub created_at: i64,
}

pub struct PatchworkOutboxRow {
    pub id: i64,
    pub patch_msg_id: String,
    pub api_url: String,
    pub check_state: String,
    pub description: String,
    pub target_url: String,
    pub context: String,
    pub status: String,
    pub retry_count: i64,
    pub next_retry_at: Option<i64>,
    pub locked_at: Option<i64>,
    pub error_log: Option<String>,
    pub created_at: i64,
}

impl Database {
    pub async fn get_oldest_message_timestamp(&self) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query("SELECT MIN(date) FROM messages WHERE date > 0", ())
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0).ok())
        } else {
            Ok(None)
        }
    }

    pub async fn get_message_details(&self, id: i64) -> Result<Option<MessageRow>> {
        let mut rows = self.conn.query(
            "SELECT m.id, m.message_id, m.thread_id, m.in_reply_to, m.author, m.subject, m.date, m.body, m.to_recipients, m.cc_recipients, m.git_blob_hash, m.mailing_list, p.diff, m.references_hdr 
             FROM messages m 
             LEFT JOIN patches p ON m.message_id = p.message_id
             WHERE m.id = ?",
             libsql::params![id],
        ).await?;

        let row_data = if let Ok(Some(row)) = rows.next().await {
            Some((
                row.get::<i64>(0)?,
                row.get::<String>(1)?,
                row.get::<Option<i64>>(2).ok().flatten(),
                row.get::<Option<String>>(3).ok().flatten(),
                row.get::<Option<String>>(4).ok().flatten(),
                row.get::<Option<String>>(5).ok().flatten(),
                row.get::<Option<i64>>(6).ok().flatten(),
                row.get::<Option<String>>(7).ok().flatten(),
                row.get::<Option<String>>(8).ok().flatten(),
                row.get::<Option<String>>(9).ok().flatten(),
                row.get::<Option<String>>(10).ok().flatten(),
                row.get::<Option<String>>(11).ok().flatten(),
                row.get::<Option<String>>(12).ok().flatten(),
                row.get::<Option<String>>(13).ok().flatten(),
            ))
        } else {
            None
        };

        if let Some((
            id,
            message_id,
            thread_id,
            in_reply_to,
            author,
            subject,
            date,
            body,
            to,
            cc,
            git_blob_hash,
            mailing_list,
            raw_diff,
            references_hdr,
        )) = row_data
        {
            // Fetch thread messages
            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
                    }));
                }
            }

            // For email-based patches, the diff is often just the body.
            // We don't want to show it twice in the UI.
            // For git commits, body is the commit message and diff is the actual diff.
            let diff = if let (Some(b), Some(d)) = (&body, &raw_diff) {
                if b == d { None } else { raw_diff.clone() }
            } else {
                raw_diff.clone()
            };

            Ok(Some(MessageRow {
                id,
                message_id,
                thread_id,
                in_reply_to,
                author,
                subject,
                date,
                body,
                to,
                cc,
                git_blob_hash,
                mailing_list,
                diff,
                references_hdr,
                thread: Some(messages),
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn get_message_details_by_msgid(&self, msg_id: &str) -> Result<Option<MessageRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;

        let id = if let Ok(Some(row)) = rows.next().await {
            Some(row.get::<i64>(0)?)
        } else {
            None
        };

        if let Some(id) = id {
            self.get_message_details(id).await
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_details_by_msgid(
        &self,
        msg_id: &str,
        page: Option<u32>,
        limit: Option<u32>,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        // 1. Try to find a patchset where this is the cover letter
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE cover_letter_message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self
                .get_patchset_details(id, page, limit, bypass_embargo)
                .await;
        }

        // 2. Fallback: Find a patchset that contains this message as a patch
        let mut rows = self
            .conn
            .query(
                "SELECT patchset_id FROM patches WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self
                .get_patchset_details(id, page, limit, bypass_embargo)
                .await;
        }

        Ok(None)
    }

    pub async fn get_message_body(&self, msg_id: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT body, git_blob_hash, mailing_list FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let body: Option<String> = row.get(0).ok();
            if let Some(b) = body
                && !b.is_empty()
            {
                return Ok(Some(b));
            }
            // Try git blob
            let hash: Option<String> = row.get(1).ok();
            let group: Option<String> = row.get(2).ok();

            if let (Some(_h), Some(_g)) = (hash, group) {
                // We don't have easy access to git_ops::read_blob here without repo path.
                // The DB does not know about repo path logic; it must be passed in.
                // The Reviewer service has the repo path.
                // Return None if the body is empty in the DB, and let the caller handle the blob if needed.
                // The body is needed for the base-commit.
                // The body is populated in the DB if it is small.
                // Sashiko stores the body in the DB unless it is a large patch.
                // See "body_to_store" logic in main.rs:
                // `if is_git_hash { ("", Some(hash)) } else { (body, None) }`
                // If it is from a git archive, the body is empty in the DB.
                return Ok(None);
            }
            Ok(None)
        } else {
            Ok(None)
        }
    }

    pub async fn new(settings: &DatabaseSettings) -> Result<Self> {
        info!(
            "Connecting to database at {}",
            crate::utils::redact_secret(&settings.url)
        );

        let db = if settings.url.starts_with("libsql://") || settings.url.starts_with("https://") {
            Builder::new_remote(settings.url.clone(), settings.token.clone())
                .build()
                .await?
        } else {
            Builder::new_local(&settings.url).build().await?
        };

        let conn = db.connect()?;

        // Enable WAL mode for better concurrency
        // PRAGMA journal_mode returns a row (the new mode), so we must use query() instead of execute()
        let _ = conn
            .query("PRAGMA journal_mode=WAL;", ())
            .await?
            .next()
            .await;
        let _ = conn
            .query("PRAGMA busy_timeout = 5000;", ())
            .await?
            .next()
            .await;

        Ok(Self {
            database: db,
            is_in_memory: settings.url == ":memory:",
            in_memory_transaction: tokio::sync::Mutex::new(()),
            conn,
        })
    }

    pub async fn migrate(&self) -> Result<()> {
        let schema = include_str!("schema.sql");
        self.conn.execute_batch(schema).await?;

        // Consolidate 'Applying' and 'In Review' states
        let _ = self
            .conn
            .execute(
                "UPDATE patchsets SET status = 'In Review' WHERE status = 'Applying'",
                (),
            )
            .await;
        let _ = self
            .conn
            .execute(
                "UPDATE reviews SET status = 'In Review' WHERE status = 'Applying'",
                (),
            )
            .await;

        // Manual migrations for existing tables
        let _ = self
            .try_add_column("messages", "to_recipients", "TEXT")
            .await;
        let _ = self
            .try_add_column("messages", "cc_recipients", "TEXT")
            .await;
        let _ = self
            .try_add_column("messages", "git_blob_hash", "TEXT")
            .await;
        let _ = self
            .try_add_column("messages", "mailing_list", "TEXT")
            .await;
        let _ = self
            .try_add_column("messages", "references_hdr", "TEXT")
            .await;
        let _ = self
            .try_create_index(
                "idx_patchsets_cover_message_id",
                "patchsets",
                "cover_letter_message_id",
            )
            .await;
        for sql in [
            "CREATE TABLE IF NOT EXISTS model_experiment_runs (id INTEGER PRIMARY KEY, review_id INTEGER NOT NULL, experiment_name TEXT NOT NULL, model_id TEXT NOT NULL DEFAULT '', provider_id TEXT NOT NULL DEFAULT '', stage INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'completed', error TEXT, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0, tokens_cached INTEGER NOT NULL DEFAULT 0, FOREIGN KEY(review_id) REFERENCES reviews(id))",
            "CREATE TABLE IF NOT EXISTS model_experiment_sources (id INTEGER PRIMARY KEY, review_id INTEGER NOT NULL, experiment_name TEXT NOT NULL, provider_id TEXT NOT NULL DEFAULT '', model_id TEXT NOT NULL DEFAULT '', selected INTEGER NOT NULL, status TEXT NOT NULL, error TEXT, FOREIGN KEY(review_id) REFERENCES reviews(id), UNIQUE(review_id, experiment_name))",
            "CREATE TABLE IF NOT EXISTS model_experiment_findings (id INTEGER PRIMARY KEY, review_id INTEGER NOT NULL, additional_model TEXT NOT NULL, main_model_id TEXT NOT NULL DEFAULT '', additional_model_id TEXT NOT NULL DEFAULT '', main_provider_id TEXT NOT NULL DEFAULT '', additional_provider_id TEXT NOT NULL DEFAULT '', finding_id TEXT NOT NULL, outcome TEXT NOT NULL, severity TEXT, confirmed_by TEXT, FOREIGN KEY(review_id) REFERENCES reviews(id))",
            "CREATE TABLE IF NOT EXISTS model_confirmation_runs (id INTEGER PRIMARY KEY, review_id INTEGER NOT NULL, model TEXT NOT NULL, model_id TEXT NOT NULL DEFAULT '', provider_id TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'completed', error TEXT, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0, tokens_cached INTEGER NOT NULL DEFAULT 0, budget_input INTEGER NOT NULL DEFAULT 0, budget_output INTEGER NOT NULL DEFAULT 0, budget_flags INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, FOREIGN KEY(review_id) REFERENCES reviews(id))",
            "CREATE INDEX IF NOT EXISTS idx_model_experiment_runs_review ON model_experiment_runs(review_id)",
            "CREATE INDEX IF NOT EXISTS idx_model_experiment_sources_review ON model_experiment_sources(review_id)",
            "CREATE INDEX IF NOT EXISTS idx_model_experiment_findings_review ON model_experiment_findings(review_id)",
            "CREATE INDEX IF NOT EXISTS idx_model_confirmation_runs_review ON model_confirmation_runs(review_id)",
            "CREATE TABLE IF NOT EXISTS cross_review_jobs (id INTEGER PRIMARY KEY, patchset_id INTEGER NOT NULL, source_name TEXT NOT NULL, source_url TEXT NOT NULL, local_model TEXT NOT NULL DEFAULT '', local_provider TEXT NOT NULL DEFAULT '', lookup_message_id TEXT NOT NULL, fallback_message_id TEXT, generation INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending', first_attempt_at INTEGER NOT NULL, next_attempt_at INTEGER NOT NULL, deadline_at INTEGER NOT NULL, lease_until INTEGER, lease_token TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, completed_at INTEGER, remote_model TEXT, remote_provider TEXT, payload_hash TEXT, merge_tokens_in INTEGER NOT NULL DEFAULT 0, merge_tokens_out INTEGER NOT NULL DEFAULT 0, merge_tokens_cached INTEGER NOT NULL DEFAULT 0, merge_budget_flags INTEGER NOT NULL DEFAULT 0, FOREIGN KEY(patchset_id) REFERENCES patchsets(id), UNIQUE(patchset_id, generation, source_name))",
            "CREATE INDEX IF NOT EXISTS idx_cross_review_jobs_due ON cross_review_jobs(status, next_attempt_at, lease_until)",
            "CREATE TABLE IF NOT EXISTS cross_review_findings (id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL, finding_id TEXT NOT NULL, patch_message_id TEXT NOT NULL, finding_json TEXT NOT NULL, accepted INTEGER, FOREIGN KEY(job_id) REFERENCES cross_review_jobs(id), UNIQUE(job_id, finding_id))",
            "CREATE INDEX IF NOT EXISTS idx_cross_review_findings_job ON cross_review_findings(job_id)",
            "CREATE TABLE IF NOT EXISTS cross_review_comparisons (id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL, finding_id TEXT NOT NULL, matched_finding_id TEXT, outcome TEXT NOT NULL, severity TEXT, FOREIGN KEY(job_id) REFERENCES cross_review_jobs(id), UNIQUE(job_id, finding_id, outcome))",
            "CREATE INDEX IF NOT EXISTS idx_cross_review_comparisons_job ON cross_review_comparisons(job_id)",
            "CREATE TABLE IF NOT EXISTS local_canonical_findings (id INTEGER PRIMARY KEY, review_id INTEGER NOT NULL, finding_id TEXT NOT NULL, finding_json TEXT NOT NULL, accepted INTEGER NOT NULL, FOREIGN KEY(review_id) REFERENCES reviews(id), UNIQUE(review_id, finding_id))",
            "CREATE INDEX IF NOT EXISTS idx_local_canonical_findings_review ON local_canonical_findings(review_id)",
            "CREATE TABLE IF NOT EXISTS review_merge_runs (id INTEGER PRIMARY KEY, review_id INTEGER NOT NULL UNIQUE, tokens_in INTEGER NOT NULL DEFAULT 0, tokens_out INTEGER NOT NULL DEFAULT 0, tokens_cached INTEGER NOT NULL DEFAULT 0, budget_flags INTEGER NOT NULL DEFAULT 0, FOREIGN KEY(review_id) REFERENCES reviews(id))",
            "CREATE TABLE IF NOT EXISTS json_decode_events (id INTEGER PRIMARY KEY, review_id INTEGER, source TEXT NOT NULL, outcome TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL)",
            "CREATE INDEX IF NOT EXISTS idx_json_decode_events_day ON json_decode_events(created_at)",
            "CREATE TABLE IF NOT EXISTS patchwork_patch_state (patch_id INTEGER PRIMARY KEY, pw_patch_id INTEGER, pw_series_id INTEGER, state TEXT NOT NULL, outcome TEXT NOT NULL, previous_state TEXT, initial_state TEXT, actor TEXT, state_changed_at INTEGER, last_event_id INTEGER, updated_at INTEGER NOT NULL, FOREIGN KEY(patch_id) REFERENCES patches(id))",
            "CREATE INDEX IF NOT EXISTS idx_patchwork_patch_state_outcome ON patchwork_patch_state(outcome)",
            "CREATE TABLE IF NOT EXISTS patchwork_sync (id INTEGER PRIMARY KEY CHECK (id = 1), last_event_date TEXT, last_event_id INTEGER, updated_at INTEGER NOT NULL)",
        ] {
            let _ = self.conn.execute(sql, ()).await;
        }
        let _ = self
            .try_add_column("cross_review_jobs", "fallback_message_id", "TEXT")
            .await;
        let _ = self
            .try_add_column(
                "cross_review_jobs",
                "generation",
                "INTEGER NOT NULL DEFAULT 1",
            )
            .await;
        let _ = self
            .try_add_column("cross_review_jobs", "lease_token", "TEXT")
            .await;
        for column in ["local_model", "local_provider"] {
            let _ = self
                .try_add_column("cross_review_jobs", column, "TEXT NOT NULL DEFAULT ''")
                .await;
        }
        let _ = self
            .try_add_column("cross_review_comparisons", "matched_finding_id", "TEXT")
            .await;
        for column in [
            "merge_tokens_in",
            "merge_tokens_out",
            "merge_tokens_cached",
            "merge_budget_flags",
        ] {
            let _ = self
                .try_add_column("cross_review_jobs", column, "INTEGER NOT NULL DEFAULT 0")
                .await;
        }
        let _ = self
            .try_add_column(
                "patchsets",
                "cross_review_generation",
                "INTEGER NOT NULL DEFAULT 0",
            )
            .await;
        self.migrate_cross_review_job_generations().await?;
        let _ = self
            .try_add_column(
                "model_experiment_runs",
                "model_id",
                "TEXT NOT NULL DEFAULT ''",
            )
            .await;
        let _ = self
            .try_add_column(
                "model_experiment_runs",
                "provider_id",
                "TEXT NOT NULL DEFAULT ''",
            )
            .await;
        let _ = self
            .try_add_column(
                "model_confirmation_runs",
                "provider_id",
                "TEXT NOT NULL DEFAULT ''",
            )
            .await;
        for (column, definition) in [
            ("budget_input", "INTEGER NOT NULL DEFAULT 0"),
            ("budget_output", "INTEGER NOT NULL DEFAULT 0"),
            ("budget_flags", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            let _ = self
                .try_add_column("model_confirmation_runs", column, definition)
                .await;
        }
        for column in ["main_provider_id", "additional_provider_id"] {
            let _ = self
                .try_add_column(
                    "model_experiment_findings",
                    column,
                    "TEXT NOT NULL DEFAULT ''",
                )
                .await;
        }
        for table in ["model_experiment_runs", "model_confirmation_runs"] {
            let _ = self
                .try_add_column(table, "status", "TEXT NOT NULL DEFAULT 'completed'")
                .await;
            let _ = self.try_add_column(table, "error", "TEXT").await;
        }
        let _ = self
            .try_add_column(
                "model_experiment_findings",
                "main_model_id",
                "TEXT NOT NULL DEFAULT ''",
            )
            .await;
        let _ = self
            .try_add_column(
                "model_experiment_findings",
                "additional_model_id",
                "TEXT NOT NULL DEFAULT ''",
            )
            .await;
        let _ = self
            .try_add_column("model_experiment_findings", "confirmed_by", "TEXT")
            .await;
        let _ = self
            .conn
            .execute(
                "UPDATE model_experiment_findings
                 SET main_model_id = COALESCE(NULLIF(main_model_id, ''), (
                         SELECT model_id FROM model_experiment_runs
                         WHERE review_id = model_experiment_findings.review_id
                           AND experiment_name = 'main' LIMIT 1
                     ), ''),
                     additional_model_id = COALESCE(NULLIF(additional_model_id, ''), (
                         SELECT model_id FROM model_experiment_runs
                         WHERE review_id = model_experiment_findings.review_id
                           AND experiment_name = model_experiment_findings.additional_model LIMIT 1
                     ), '')
                 WHERE main_model_id = '' OR additional_model_id = ''",
                (),
            )
            .await;
        let _ = self
            .try_add_column(
                "model_confirmation_runs",
                "model_id",
                "TEXT NOT NULL DEFAULT ''",
            )
            .await;
        let _ = self.try_add_column("patches", "status", "TEXT").await;
        let _ = self.try_add_column("patches", "apply_error", "TEXT").await;
        let _ = self.try_add_column("reviews", "provider", "TEXT").await;
        let _ = self
            .try_add_column("reviews", "prompts_git_hash", "TEXT")
            .await;
        let _ = self
            .try_add_column("reviews", "result_description", "TEXT")
            .await;
        let _ = self.try_add_column("reviews", "status", "TEXT").await;
        let _ = self.try_add_column("reviews", "logs", "TEXT").await;
        let _ = self.try_add_column("reviews", "patch_id", "INTEGER").await;
        let _ = self
            .try_add_column("reviews", "budget_flags", "INTEGER DEFAULT 0")
            .await;
        let _ = self
            .try_add_column("findings", "source_stages", "TEXT")
            .await;
        let _ = self
            .try_add_column("findings", "cross_review_job_id", "INTEGER")
            .await;
        let _ = self
            .try_add_column("findings", "external_finding_id", "TEXT")
            .await;
        let _ = self
            .conn
            .execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_findings_cross_review_external
                 ON findings(cross_review_job_id, external_finding_id)
                 WHERE cross_review_job_id IS NOT NULL",
                (),
            )
            .await;
        let _ = self
            .try_add_column("reviews", "concerns_total", "INTEGER")
            .await;
        let _ = self
            .try_add_column("reviews", "concerns_unique", "INTEGER")
            .await;
        let _ = self
            .try_add_column("reviews", "findings_multi_stage", "INTEGER")
            .await;
        let _ = self
            .try_create_index("idx_reviews_patch_status", "reviews", "patch_id, status")
            .await;
        let _ = self
            .try_add_column("reviews", "inline_review", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "baseline_id", "INTEGER")
            .await;
        let _ = self
            .try_add_column("patchsets", "failed_reason", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "skip_filters", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "only_filters", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "target_review_count", "INTEGER DEFAULT 1")
            .await;
        let _ = self.try_add_column("patchsets", "model_name", "TEXT").await;
        let _ = self
            .try_add_column("patchsets", "prompts_git_hash", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "baseline_logs", "TEXT")
            .await;
        let _ = self.try_add_column("patchsets", "provider", "TEXT").await;
        let _ = self
            .try_add_column("patchsets", "embargo_until", "INTEGER")
            .await;
        let _ = self
            .try_add_column("patchsets", "embargo_release_started_at", "INTEGER")
            .await;
        let _ = self
            .try_add_column(
                "patchsets",
                "cross_review_status",
                "TEXT NOT NULL DEFAULT 'disabled'",
            )
            .await;
        let _ = self
            .try_add_column("patchsets", "cross_reviewed_at", "INTEGER")
            .await;
        let _ = self.try_add_column("patchsets", "slug", "TEXT").await;
        let _ = self
            .conn
            .execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_patchsets_slug ON patchsets(slug) WHERE slug IS NOT NULL",
                (),
            )
            .await;
        let _ = self
            .try_add_column("reviews", "completed_at", "INTEGER")
            .await;
        let _ = self
            .try_create_index(
                "idx_patchsets_status_embargo_until",
                "patchsets",
                "status, embargo_until",
            )
            .await;
        let _ = self.try_add_column("patchsets", "mr_url", "TEXT").await;
        let _ = self.try_add_column("patchsets", "mr_title", "TEXT").await;
        let _ = self
            .try_add_column("patchsets", "mr_number", "INTEGER")
            .await;

        let _ = self
            .conn
            .execute(
                "CREATE TABLE IF NOT EXISTS tool_usages (
                    id INTEGER PRIMARY KEY,
                    review_id INTEGER NOT NULL,
                    provider TEXT,
                    model TEXT,
                    tool_name TEXT,
                    arguments TEXT,
                    output_length INTEGER,
                    created_at INTEGER,
                    FOREIGN KEY(review_id) REFERENCES reviews(id)
                )",
                (),
            )
            .await;
        let _ = self
            .try_create_index("idx_tool_usages_review", "tool_usages", "review_id")
            .await;
        let _ = self
            .try_create_index(
                "idx_ai_interactions_tokens",
                "ai_interactions",
                "id, tokens_in, tokens_out, tokens_cached",
            )
            .await;
        let _ = self
            .try_create_index(
                "idx_reviews_grouping",
                "reviews",
                "provider, model, status, interaction_id",
            )
            .await;
        let _ = self
            .try_create_index(
                "idx_tool_usages_stats",
                "tool_usages",
                "provider, model, tool_name, output_length",
            )
            .await;

        // Manual migration for messages_mailing_lists
        let _ = self
            .conn
            .execute(
                "CREATE TABLE IF NOT EXISTS messages_mailing_lists (
                    message_id INTEGER NOT NULL,
                    mailing_list_id INTEGER NOT NULL,
                    PRIMARY KEY (message_id, mailing_list_id),
                    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
                    FOREIGN KEY(mailing_list_id) REFERENCES mailing_lists(id) ON DELETE CASCADE
                )",
                (),
            )
            .await;

        // Backfill messages_mailing_lists from messages.mailing_list
        let _ = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO messages_mailing_lists (message_id, mailing_list_id)
                 SELECT m.id, ml.id
                 FROM messages m
                 JOIN mailing_lists ml ON m.mailing_list = ml.nntp_group
                 WHERE m.mailing_list IS NOT NULL",
                (),
            )
            .await;

        // Findings table migration
        let _ = self
            .try_add_column("findings", "severity_explanation", "TEXT")
            .await;
        let _ = self
            .try_add_column("findings", "preexisting", "INTEGER")
            .await;
        let _ = self.try_add_column("findings", "locations", "TEXT").await;
        // Ignore errors for these as they might fail on new DBs or if already migrated
        let _ = self
            .conn
            .execute("ALTER TABLE findings RENAME COLUMN message TO problem", ())
            .await;
        let _ = self
            .conn
            .execute("ALTER TABLE findings DROP COLUMN file_path", ())
            .await;
        let _ = self
            .conn
            .execute("ALTER TABLE findings DROP COLUMN line_number", ())
            .await;

        let _ = self
            .try_create_index("idx_patchsets_date", "patchsets", "date DESC")
            .await;
        let _ = self
            .try_create_index(
                "idx_reviews_patchset_status",
                "reviews",
                "patchset_id, status",
            )
            .await;
        let _ = self
            .try_create_index(
                "idx_reviews_day",
                "reviews",
                "strftime('%Y-%m-%d', created_at, 'unixepoch'), status",
            )
            .await;
        Ok(())
    }

    pub async fn get_mailing_list_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM mailing_lists WHERE nntp_group = ?",
                libsql::params![name],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn add_message_to_mailing_list(
        &self,
        message_id: i64,
        mailing_list_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_mailing_lists (message_id, mailing_list_id) VALUES (?, ?)",
                libsql::params![message_id, mailing_list_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_mailing_lists(&self) -> Result<Vec<(String, String)>> {
        let mut rows = self
            .conn
            .query("SELECT name, nntp_group FROM mailing_lists", ())
            .await?;
        let mut lists = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            lists.push((row.get(0)?, row.get(1)?));
        }
        Ok(lists)
    }

    pub async fn get_pending_review_id(
        &self,
        patchset_id: i64,
        patch_id: Option<i64>,
    ) -> Result<Option<i64>> {
        let mut rows = match patch_id {
            Some(pid) => {
                self.conn.query("SELECT id FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status = 'Pending' LIMIT 1", libsql::params![patchset_id, pid]).await?
            }
            None => {
                self.conn.query("SELECT id FROM reviews WHERE patchset_id = ? AND patch_id IS NULL AND status = 'Pending' LIMIT 1", libsql::params![patchset_id]).await?
            }
        };
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn create_review(
        &self,
        patchset_id: i64,
        patch_id: Option<i64>,
        provider: &str,
        model: &str,
        baseline_id: Option<i64>,
        prompts_hash: Option<&str>,
    ) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "INSERT INTO reviews (patchset_id, patch_id, status, created_at, provider, model, baseline_id, prompts_hash)
             VALUES (?, ?, 'Pending', ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![
                    patchset_id,
                    patch_id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs() as i64,
                    provider,
                    model,
                    baseline_id,
                    prompts_hash
                ],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get review ID"))
        }
    }

    pub async fn has_successful_review(
        &self,
        patchset_id: i64,
        patch_id: i64,
        baseline_id: Option<i64>,
    ) -> Result<bool> {
        Ok(self
            .count_successful_reviews(patchset_id, patch_id, baseline_id)
            .await?
            > 0)
    }

    pub async fn count_successful_reviews(
        &self,
        patchset_id: i64,
        patch_id: i64,
        _baseline_id: Option<i64>,
    ) -> Result<usize> {
        let mut rows = self.conn
            .query(
                "SELECT COUNT(*) FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status = 'Reviewed'",
                libsql::params![patchset_id, patch_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn has_failed_review(
        &self,
        patchset_id: i64,
        patch_id: i64,
        _baseline_id: Option<i64>,
    ) -> Result<bool> {
        let mut rows = self.conn
            .query(
                "SELECT 1 FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL",
                libsql::params![patchset_id, patch_id],
            )
            .await?;

        Ok(rows.next().await.ok().flatten().is_some())
    }

    pub async fn update_review_status(
        &self,
        review_id: i64,
        status: &str,
        logs: Option<&str>,
    ) -> Result<()> {
        if let Some(l) = logs {
            self.conn
                .execute(
                    "UPDATE reviews SET status = ?, logs = ? WHERE id = ?",
                    libsql::params![status, l, review_id],
                )
                .await?;
        } else {
            self.conn
                .execute(
                    "UPDATE reviews SET status = ? WHERE id = ?",
                    libsql::params![status, review_id],
                )
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn complete_review(
        &self,
        review_id: i64,
        status: &str,
        result: &str,
        summary: Option<&str>,
        interaction_id: Option<&str>,
        inline_review: Option<&str>,
        logs: Option<&str>,
        budget_flags: Option<u8>,
    ) -> Result<()> {
        let completed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        self.conn
            .execute(
                "UPDATE reviews SET status = ?, result_description = ?, summary = ?, interaction_id = ?, inline_review = ?, logs = ?, budget_flags = ?, completed_at = ? WHERE id = ?",
                libsql::params![status, result, summary, interaction_id, inline_review, logs, budget_flags.unwrap_or(0) as i64, completed_at, review_id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_review_dedup_stats(
        &self,
        review_id: i64,
        total: i64,
        unique: i64,
        multi_stage: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE reviews SET concerns_total = ?, concerns_unique = ?, findings_multi_stage = ? WHERE id = ?",
                libsql::params![total, unique, multi_stage, review_id],
            )
            .await?;
        Ok(())
    }

    pub async fn enqueue_cross_reviews(
        &self,
        patchset_id: i64,
        instances: &[(String, String)],
        now: i64,
    ) -> Result<()> {
        if instances.is_empty() {
            return Ok(());
        }
        let mut rows = self
            .conn
            .query(
                "SELECT (SELECT message_id FROM patches
                         WHERE patchset_id = patchsets.id ORDER BY part_index LIMIT 1),
                        NULLIF(cover_letter_message_id, ''),
                        COALESCE(target_review_count, 1),
                        COALESCE(model_name, ''), COALESCE(provider, '')
                 FROM patchsets WHERE id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("patchset does not exist"))?;
        let first_patch_message_id: Option<String> = row.get(0).ok();
        let cover_message_id: Option<String> = row.get(1).ok();
        let lookup_message_id = first_patch_message_id
            .clone()
            .or_else(|| cover_message_id.clone())
            .ok_or_else(|| anyhow::anyhow!("patchset has no cross-instance message ID"))?;
        let fallback_message_id = first_patch_message_id
            .is_some()
            .then_some(cover_message_id)
            .flatten()
            .filter(|cover| cover != &lookup_message_id);
        let generation: i64 = row.get(2)?;
        let local_model: String = row.get(3)?;
        let local_provider: String = row.get(4)?;
        drop(rows);

        for (name, url) in instances {
            self.conn
                .execute(
                    "INSERT INTO cross_review_jobs
                     (patchset_id, source_name, source_url, local_model,
                      local_provider, lookup_message_id, fallback_message_id,
                      generation, status, first_attempt_at, next_attempt_at,
                      deadline_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)
                     ON CONFLICT(patchset_id, generation, source_name) DO NOTHING",
                    libsql::params![
                        patchset_id,
                        name.clone(),
                        url.clone(),
                        local_model.clone(),
                        local_provider.clone(),
                        lookup_message_id.clone(),
                        fallback_message_id.clone(),
                        generation,
                        now,
                        now,
                        now + 3 * 24 * 60 * 60,
                    ],
                )
                .await?;
        }
        self.conn
            .execute(
                "UPDATE patchsets SET cross_review_status = 'pending',
                 cross_reviewed_at = NULL, cross_review_generation = ? WHERE id = ?",
                libsql::params![generation, patchset_id],
            )
            .await?;
        Ok(())
    }

    pub async fn claim_due_cross_reviews(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<CrossReviewJob>> {
        let mut expired_rows = self
            .conn
            .query(
                "SELECT DISTINCT j.patchset_id FROM cross_review_jobs j
                 JOIN patchsets p ON p.id = j.patchset_id
                 WHERE j.generation = p.cross_review_generation
                   AND j.status IN ('pending', 'processing') AND j.deadline_at <= ?",
                libsql::params![now],
            )
            .await?;
        let mut expired_patchsets = Vec::new();
        while let Some(row) = expired_rows.next().await? {
            expired_patchsets.push(row.get::<i64>(0)?);
        }
        drop(expired_rows);
        self.conn
            .execute(
                "UPDATE cross_review_jobs SET status = 'expired', lease_until = NULL,
                 lease_token = NULL,
                 last_error = 'cross-review deadline expired'
                 WHERE status IN ('pending', 'processing') AND deadline_at <= ?
                   AND generation = (SELECT cross_review_generation FROM patchsets
                                     WHERE id = cross_review_jobs.patchset_id)",
                libsql::params![now],
            )
            .await?;
        for patchset_id in expired_patchsets {
            self.refresh_cross_review_status(patchset_id, now).await?;
        }
        let mut rows = self
            .conn
            .query(
                "SELECT j.id, j.patchset_id, j.source_name, j.source_url,
                        j.local_model, j.local_provider, j.lookup_message_id,
                        j.fallback_message_id, j.generation, j.deadline_at,
                        j.attempts
                 FROM cross_review_jobs j
                 JOIN patchsets p ON p.id = j.patchset_id
                 WHERE j.generation = p.cross_review_generation
                   AND j.status IN ('pending', 'processing')
                   AND next_attempt_at <= ?
                   AND (lease_until IS NULL OR lease_until <= ?)
                   AND deadline_at > ?
                   AND NOT EXISTS (
                       SELECT 1 FROM cross_review_jobs active
                       WHERE active.patchset_id = j.patchset_id
                         AND active.generation = j.generation
                         AND active.id != j.id
                         AND active.status = 'processing'
                         AND active.lease_until > ?)
                 ORDER BY j.next_attempt_at, j.id LIMIT ?",
                libsql::params![now, now, now, now, limit as i64],
            )
            .await?;
        let mut candidates = Vec::new();
        while let Some(row) = rows.next().await? {
            candidates.push(CrossReviewJob {
                id: row.get(0)?,
                patchset_id: row.get(1)?,
                source_name: row.get(2)?,
                source_url: row.get(3)?,
                local_model: row.get(4)?,
                local_provider: row.get(5)?,
                lookup_message_id: row.get(6)?,
                fallback_message_id: row.get(7).ok(),
                generation: row.get(8)?,
                deadline_at: row.get(9)?,
                attempts: row.get(10)?,
                lease_token: String::new(),
            });
        }
        drop(rows);

        let mut claimed = Vec::new();
        for job in candidates {
            let lease_token = format!("{}-{}-{}", job.id, now, job.attempts + 1);
            let changed = self
                .conn
                .execute(
                    "UPDATE cross_review_jobs SET status = 'processing',
                     lease_until = ?, lease_token = ?, attempts = attempts + 1
                     WHERE id = ? AND status IN ('pending', 'processing')
                       AND (lease_until IS NULL OR lease_until <= ?)
                       AND deadline_at > ?
                       AND generation = (SELECT cross_review_generation FROM patchsets
                                         WHERE id = cross_review_jobs.patchset_id)
                       AND NOT EXISTS (
                           SELECT 1 FROM cross_review_jobs active
                           WHERE active.patchset_id = cross_review_jobs.patchset_id
                             AND active.generation = cross_review_jobs.generation
                             AND active.id != cross_review_jobs.id
                             AND active.status = 'processing'
                             AND active.lease_until > ?)",
                    libsql::params![now + 60 * 60, lease_token.clone(), job.id, now, now, now],
                )
                .await?;
            if changed == 1 {
                claimed.push(CrossReviewJob {
                    attempts: job.attempts + 1,
                    lease_token,
                    ..job
                });
            }
        }
        Ok(claimed)
    }

    pub async fn retry_cross_review(
        &self,
        job: &CrossReviewJob,
        now: i64,
        error: &str,
    ) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE cross_review_jobs SET status = 'pending', lease_until = NULL,
                 lease_token = NULL, next_attempt_at = ?, last_error = ?
                 WHERE id = ? AND status = 'processing' AND lease_token = ?
                   AND lease_until > ? AND deadline_at > ?
                   AND generation = (SELECT cross_review_generation FROM patchsets
                                     WHERE id = cross_review_jobs.patchset_id)",
                libsql::params![
                    now + 60 * 60,
                    error,
                    job.id,
                    job.lease_token.clone(),
                    now,
                    now,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    pub async fn record_cross_review_usage(
        &self,
        job: &CrossReviewJob,
        now: i64,
        usage: &crate::cross_review::CrossReviewUsage,
    ) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE cross_review_jobs
                 SET merge_tokens_in = merge_tokens_in + ?,
                     merge_tokens_out = merge_tokens_out + ?,
                     merge_tokens_cached = merge_tokens_cached + ?,
                     merge_budget_flags = merge_budget_flags | ?
                 WHERE id = ? AND status = 'processing' AND lease_token = ?
                   AND lease_until > ? AND deadline_at > ?
                   AND generation = (SELECT cross_review_generation FROM patchsets
                                     WHERE id = cross_review_jobs.patchset_id)",
                libsql::params![
                    usage.tokens_in as i64,
                    usage.tokens_out as i64,
                    usage.tokens_cached as i64,
                    usage.budget_flags as i64,
                    job.id,
                    job.lease_token.clone(),
                    now,
                    now,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    pub async fn finish_cross_review_job(
        &self,
        job: &CrossReviewJob,
        status: &str,
        now: i64,
        error: Option<&str>,
    ) -> Result<bool> {
        let changed = self
            .conn
            .execute(
                "UPDATE cross_review_jobs SET status = ?, lease_until = NULL,
                 lease_token = NULL, completed_at = ?, last_error = ?
                 WHERE id = ? AND status = 'processing' AND lease_token = ?
                   AND lease_until > ? AND deadline_at > ?
                   AND generation = (SELECT cross_review_generation FROM patchsets
                                     WHERE id = cross_review_jobs.patchset_id)",
                libsql::params![
                    status,
                    now,
                    error,
                    job.id,
                    job.lease_token.clone(),
                    now,
                    now,
                ],
            )
            .await?;
        if changed == 1 {
            self.refresh_cross_review_status(job.patchset_id, now)
                .await?;
        }
        Ok(changed == 1)
    }

    async fn refresh_cross_review_status(&self, patchset_id: i64, now: i64) -> Result<()> {
        self.refresh_cross_review_status_on(&self.conn, patchset_id, now)
            .await
    }

    async fn refresh_cross_review_status_on(
        &self,
        connection: &libsql::Connection,
        patchset_id: i64,
        now: i64,
    ) -> Result<()> {
        let mut rows = connection
            .query(
                "SELECT COUNT(*),
                        SUM(CASE WHEN status = 'complete' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status = 'expired' THEN 1 ELSE 0 END)
                 FROM cross_review_jobs
                 WHERE patchset_id = ?
                   AND generation = (SELECT cross_review_generation FROM patchsets WHERE id = ?)",
                libsql::params![patchset_id, patchset_id],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing cross-review aggregate"))?;
        let total: i64 = row.get(0)?;
        let complete: i64 = row.get(1).unwrap_or_default();
        let errors: i64 = row.get(2).unwrap_or_default();
        let expired: i64 = row.get(3).unwrap_or_default();
        let status = if errors > 0 {
            "error"
        } else if expired > 0 {
            "expired"
        } else if total > 0 && complete == total {
            "complete"
        } else {
            "pending"
        };
        let completed_at = (status == "complete").then_some(now);
        connection
            .execute(
                "UPDATE patchsets SET cross_review_status = ?, cross_reviewed_at = ?
                 WHERE id = ?",
                libsql::params![status, completed_at, patchset_id],
            )
            .await?;
        Ok(())
    }

    pub async fn save_local_canonical_findings(
        &self,
        review_id: i64,
        canonical: &serde_json::Value,
        published: &serde_json::Value,
    ) -> Result<()> {
        let published = published.as_array().cloned().unwrap_or_default();
        self.conn
            .execute(
                "DELETE FROM local_canonical_findings WHERE review_id = ?",
                libsql::params![review_id],
            )
            .await?;
        for finding in canonical.as_array().into_iter().flatten() {
            if finding["preexisting"].as_bool() == Some(true) {
                continue;
            }
            let finding_id = finding["finding_ids"]
                .as_array()
                .and_then(|ids| ids.first())
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("canonical finding is missing its ID"))?;
            let candidate_ids: std::collections::HashSet<&str> = finding["finding_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect();
            let published_finding = published.iter().find(|published_finding| {
                published_finding["finding_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .any(|id| candidate_ids.contains(id))
            });
            self.conn
                .execute(
                    "INSERT INTO local_canonical_findings
                     (review_id, finding_id, finding_json, accepted)
                     VALUES (?, ?, ?, ?)",
                    libsql::params![
                        review_id,
                        finding_id,
                        published_finding.unwrap_or(finding).to_string(),
                        i64::from(published_finding.is_some()),
                    ],
                )
                .await?;
        }
        Ok(())
    }

    pub async fn save_review_merge_run(
        &self,
        review_id: i64,
        usage: &serde_json::Value,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO review_merge_runs
                 (review_id, tokens_in, tokens_out, tokens_cached, budget_flags)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(review_id) DO UPDATE SET
                   tokens_in = excluded.tokens_in,
                   tokens_out = excluded.tokens_out,
                   tokens_cached = excluded.tokens_cached,
                   budget_flags = excluded.budget_flags",
                libsql::params![
                    review_id,
                    usage["tokens_in"].as_i64().unwrap_or(0),
                    usage["tokens_out"].as_i64().unwrap_or(0),
                    usage["tokens_cached"].as_i64().unwrap_or(0),
                    usage["budget_flags"].as_i64().unwrap_or(0),
                ],
            )
            .await?;
        Ok(())
    }

    /// Persists structured-response failures so silent malformed replies stay visible.
    /// `review_id` is None for work that runs outside a review (cross-review).
    pub async fn save_json_decode_events(
        &self,
        review_id: Option<i64>,
        events: &[crate::json_health::JsonDecodeEvent],
        now: i64,
    ) -> Result<()> {
        for event in events {
            self.conn
                .execute(
                    "INSERT INTO json_decode_events
                     (review_id, source, outcome, detail, created_at)
                     VALUES (?, ?, ?, ?, ?)",
                    libsql::params![
                        review_id,
                        event.source.as_str(),
                        event.outcome.as_str(),
                        event.detail.as_str(),
                        now,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    /// Daily structured-response failures by source and outcome over the trailing two
    /// weeks, plus a per-source roll-up for the summary table.
    pub async fn get_json_decode_stats(&self) -> Result<serde_json::Value> {
        let mut daily = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT strftime('%Y-%m-%d', created_at, 'unixepoch') AS day,
                        source, outcome, count(*)
                 FROM json_decode_events
                 WHERE created_at >= unixepoch('now', '-13 days', 'start of day')
                 GROUP BY day, source, outcome
                 ORDER BY day, source, outcome",
                (),
            )
            .await?;
        while let Ok(Some(row)) = rows.next().await {
            daily.push(json!({
                "day": row.get::<String>(0)?,
                "source": row.get::<String>(1)?,
                "outcome": row.get::<String>(2)?,
                "count": row.get::<i64>(3)?,
            }));
        }
        drop(rows);

        let mut by_source = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT source,
                        sum(CASE WHEN outcome = 'salvaged' THEN 1 ELSE 0 END),
                        sum(CASE WHEN outcome = 'recovered_on_retry' THEN 1 ELSE 0 END),
                        sum(CASE WHEN outcome = 'fatal' THEN 1 ELSE 0 END),
                        count(*),
                        max(detail)
                 FROM json_decode_events
                 WHERE created_at >= unixepoch('now', '-13 days', 'start of day')
                 GROUP BY source
                 ORDER BY sum(CASE WHEN outcome = 'fatal' THEN 1 ELSE 0 END) DESC, count(*) DESC",
                (),
            )
            .await?;
        while let Ok(Some(row)) = rows.next().await {
            by_source.push(json!({
                "source": row.get::<String>(0)?,
                "salvaged": row.get::<i64>(1).unwrap_or(0),
                "recovered_on_retry": row.get::<i64>(2).unwrap_or(0),
                "fatal": row.get::<i64>(3).unwrap_or(0),
                "total": row.get::<i64>(4).unwrap_or(0),
                "sample_detail": row.get::<String>(5).unwrap_or_default(),
            }));
        }
        Ok(json!({"daily": daily, "by_source": by_source}))
    }

    pub async fn load_cross_review_inputs(
        &self,
        patchset_id: i64,
    ) -> Result<(Vec<crate::cross_review::LocalCanonicalFinding>, String)> {
        let mut rows = self
            .conn
            .query(
                "SELECT l.finding_id, l.finding_json, l.accepted, p.message_id, r.id
                 FROM local_canonical_findings l
                 JOIN reviews r ON r.id = l.review_id
                 JOIN patches p ON p.id = r.patch_id
                 WHERE r.patchset_id = ? AND r.status = 'Reviewed'
                   AND r.id = (SELECT MAX(current.id) FROM reviews current
                               WHERE current.patchset_id = r.patchset_id
                                 AND current.patch_id = r.patch_id
                                 AND current.status = 'Reviewed')",
                libsql::params![patchset_id],
            )
            .await?;
        let mut findings = Vec::new();
        while let Some(row) = rows.next().await? {
            let raw_finding_id: String = row.get(0)?;
            let patch_message_id: String = row.get(3)?;
            let review_id: i64 = row.get(4)?;
            findings.push(crate::cross_review::LocalCanonicalFinding {
                finding_id: format!("{review_id}:{patch_message_id}:{raw_finding_id}"),
                finding: serde_json::from_str(&row.get::<String>(1)?)?,
                accepted: row.get::<i64>(2)? != 0,
                patch_message_id,
                review_id: Some(review_id),
                source_name: None,
                external_finding_id: None,
                cross_review_job_id: None,
            });
        }
        drop(rows);
        let mut rows = self
            .conn
            .query(
                "SELECT f.finding_id, f.finding_json, f.patch_message_id,
                        j.id, j.source_name
                 FROM cross_review_findings f
                 JOIN cross_review_jobs j ON j.id = f.job_id
                 JOIN patchsets p ON p.id = j.patchset_id
                 JOIN findings published
                   ON published.cross_review_job_id = j.id
                  AND published.external_finding_id = f.finding_id
                 WHERE j.patchset_id = ? AND j.generation = p.cross_review_generation
                   AND j.status = 'complete' AND f.accepted = 1",
                libsql::params![patchset_id],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            let external_finding_id: String = row.get(0)?;
            let job_id: i64 = row.get(3)?;
            findings.push(crate::cross_review::LocalCanonicalFinding {
                finding_id: format!("remote:{job_id}:{external_finding_id}"),
                finding: serde_json::from_str(&row.get::<String>(1)?)?,
                accepted: true,
                patch_message_id: row.get(2)?,
                review_id: None,
                source_name: Some(row.get(4)?),
                external_finding_id: Some(external_finding_id),
                cross_review_job_id: Some(job_id),
            });
        }
        drop(rows);
        let mut context_rows = self
            .conn
            .query(
                "SELECT p.message_id, ai.input_context
                 FROM reviews r
                 JOIN patches p ON p.id = r.patch_id
                 JOIN ai_interactions ai ON ai.id = r.interaction_id
                 WHERE r.patchset_id = ? AND r.status = 'Reviewed'
                   AND r.id = (SELECT MAX(current.id) FROM reviews current
                               WHERE current.patchset_id = r.patchset_id
                                 AND current.patch_id = r.patch_id
                                 AND current.status = 'Reviewed')
                 ORDER BY p.part_index",
                libsql::params![patchset_id],
            )
            .await?;
        let mut context = String::from("Prepared code-review context:\n");
        while let Some(row) = context_rows.next().await? {
            use std::fmt::Write;
            let message_id: String = row.get(0)?;
            let prepared: String = row.get(1).unwrap_or_default();
            let _ = write!(context, "\nPatch {message_id}:\n{prepared}\n");
            if context.len() > 500_000 {
                let mut truncate_at = context.len().min(500_000);
                while !context.is_char_boundary(truncate_at) {
                    truncate_at -= 1;
                }
                context.truncate(truncate_at);
                context.push_str("\n[remaining prepared context truncated]");
                break;
            }
        }
        Ok((findings, context))
    }

    /// Loads what rendering an accepted remote finding into a report needs, keyed
    /// by patch message ID.
    ///
    /// `inline_review` here is only the sample of house style handed to the model;
    /// the copy that gets spliced is re-read inside the publishing transaction so
    /// several findings in one job splice onto each other's output.
    pub async fn load_cross_review_render_inputs(
        &self,
        patchset_id: i64,
    ) -> Result<std::collections::HashMap<String, CrossRenderInput>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.message_id, ai.input_context, p.diff, r.inline_review
                 FROM reviews r
                 JOIN patches p ON p.id = r.patch_id
                 LEFT JOIN ai_interactions ai ON ai.id = r.interaction_id
                 WHERE r.patchset_id = ? AND r.status = 'Reviewed'
                   AND r.id = (SELECT MAX(current.id) FROM reviews current
                               WHERE current.patchset_id = r.patchset_id
                                 AND current.patch_id = r.patch_id
                                 AND current.status = 'Reviewed')",
                libsql::params![patchset_id],
            )
            .await?;
        let mut inputs = std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            inputs.insert(
                row.get::<String>(0)?,
                CrossRenderInput {
                    context: row.get(1).unwrap_or_default(),
                    diff: row.get(2).unwrap_or_default(),
                    inline_review: row.get(3).unwrap_or_default(),
                },
            );
        }
        Ok(inputs)
    }

    pub async fn persist_cross_review_result(
        &self,
        job: &CrossReviewJob,
        remote: &crate::cross_review::RemoteReviewResult,
        analysis: &crate::cross_review::CrossReviewAnalysis,
        rendered: &std::collections::HashMap<String, crate::cross_render::RenderedComment>,
        usage: &crate::cross_review::CrossReviewUsage,
        now: i64,
    ) -> Result<()> {
        if self.is_in_memory {
            let _transaction_guard = self.in_memory_transaction.lock().await;
            self.conn.execute("BEGIN IMMEDIATE", ()).await?;
            let result = self
                .persist_cross_review_records(
                    &self.conn, job, remote, analysis, rendered, usage, now,
                )
                .await;
            return match result {
                Ok(()) => {
                    self.conn.execute("COMMIT", ()).await?;
                    Ok(())
                }
                Err(error) => {
                    let _ = self.conn.execute("ROLLBACK", ()).await;
                    Err(error)
                }
            };
        }
        let connection = self.database.connect()?;
        let _ = connection
            .query("PRAGMA busy_timeout = 5000", ())
            .await?
            .next()
            .await;
        let transaction = connection
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let result = self
            .persist_cross_review_records(&transaction, job, remote, analysis, rendered, usage, now)
            .await;
        match result {
            Ok(()) => {
                transaction.commit().await?;
                Ok(())
            }
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_cross_review_records(
        &self,
        connection: &libsql::Connection,
        job: &CrossReviewJob,
        remote: &crate::cross_review::RemoteReviewResult,
        analysis: &crate::cross_review::CrossReviewAnalysis,
        rendered: &std::collections::HashMap<String, crate::cross_render::RenderedComment>,
        usage: &crate::cross_review::CrossReviewUsage,
        now: i64,
    ) -> Result<()> {
        let mut fence = connection
            .query(
                "SELECT 1 FROM cross_review_jobs
                 WHERE id = ? AND status = 'processing' AND lease_token = ?
                   AND lease_until > ? AND deadline_at > ?
                   AND generation = (SELECT cross_review_generation FROM patchsets
                                     WHERE id = cross_review_jobs.patchset_id)",
                libsql::params![job.id, job.lease_token.clone(), now, now],
            )
            .await?;
        if fence.next().await?.is_none() {
            return Ok(());
        }
        drop(fence);
        connection
            .execute(
                "DELETE FROM cross_review_findings WHERE job_id = ?",
                libsql::params![job.id],
            )
            .await?;
        connection
            .execute(
                "DELETE FROM cross_review_comparisons WHERE job_id = ?",
                libsql::params![job.id],
            )
            .await?;
        let accepted_ids: std::collections::HashSet<&str> = analysis
            .comparisons
            .iter()
            .filter(|comparison| comparison.outcome != "remote_hallucination")
            .map(|comparison| comparison.finding_id.as_str())
            .collect();
        for finding in &remote.findings {
            connection
                .execute(
                    "INSERT INTO cross_review_findings
                     (job_id, finding_id, patch_message_id, finding_json, accepted)
                     VALUES (?, ?, ?, ?, ?)",
                    libsql::params![
                        job.id,
                        finding.finding_id.clone(),
                        finding.patch_message_id.clone(),
                        serde_json::to_string(finding)?,
                        i64::from(accepted_ids.contains(finding.finding_id.as_str())),
                    ],
                )
                .await?;
        }
        for comparison in &analysis.comparisons {
            connection
                .execute(
                    "INSERT INTO cross_review_comparisons
                     (job_id, finding_id, matched_finding_id, outcome, severity)
                     VALUES (?, ?, ?, ?, ?)",
                    libsql::params![
                        job.id,
                        comparison.finding_id.clone(),
                        comparison.matched_finding_id.clone(),
                        comparison.outcome.clone(),
                        comparison.severity.clone(),
                    ],
                )
                .await?;
        }
        for finding in &analysis.accepted_remote {
            self.publish_cross_review_finding(
                connection,
                job,
                finding,
                rendered.get(&finding.finding_id),
            )
            .await?;
        }
        for matched in &analysis.matched_local {
            self.merge_local_cross_review_provenance(connection, job, matched)
                .await?;
        }
        for matched in &analysis.matched_remote {
            self.merge_imported_cross_review_provenance(connection, job, matched)
                .await?;
        }
        let changed = connection
            .execute(
                "UPDATE cross_review_jobs SET remote_model = ?, remote_provider = ?,
                 payload_hash = ?, merge_tokens_in = merge_tokens_in + ?,
                 merge_tokens_out = merge_tokens_out + ?,
                 merge_tokens_cached = merge_tokens_cached + ?,
                 merge_budget_flags = merge_budget_flags | ?,
                 status = 'complete', lease_until = NULL,
                 lease_token = NULL, completed_at = ?, last_error = NULL
                 WHERE id = ? AND status = 'processing' AND lease_token = ?
                   AND lease_until > ? AND deadline_at > ?
                   AND generation = (SELECT cross_review_generation FROM patchsets
                                     WHERE id = cross_review_jobs.patchset_id)",
                libsql::params![
                    remote.model.clone(),
                    remote.provider.clone(),
                    remote.payload_hash.clone(),
                    usage.tokens_in as i64,
                    usage.tokens_out as i64,
                    usage.tokens_cached as i64,
                    usage.budget_flags as i64,
                    now,
                    job.id,
                    job.lease_token.clone(),
                    now,
                    now,
                ],
            )
            .await?;
        if changed != 1 {
            anyhow::bail!("cross-review lease was lost while publishing");
        }
        self.refresh_cross_review_status_on(connection, job.patchset_id, now)
            .await
    }

    async fn publish_cross_review_finding(
        &self,
        connection: &libsql::Connection,
        job: &CrossReviewJob,
        finding: &crate::cross_review::RemoteFinding,
        rendered: Option<&crate::cross_render::RenderedComment>,
    ) -> Result<()> {
        let mut rows = connection
            .query(
                "SELECT r.id, r.interaction_id, r.inline_review, ai.output_raw, p.diff
                 FROM reviews r
                 JOIN patches p ON p.id = r.patch_id
                 LEFT JOIN ai_interactions ai ON ai.id = r.interaction_id
                 WHERE r.patchset_id = ? AND p.message_id = ?
                   AND r.status = 'Reviewed'
                 ORDER BY r.created_at DESC LIMIT 1",
                libsql::params![job.patchset_id, finding.patch_message_id.clone()],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("remote finding has no local reviewed patch"))?;
        let review_id: i64 = row.get(0)?;
        let interaction_id: Option<String> = row.get(1).ok();
        let inline_review: Option<String> = row.get(2).ok();
        let output_raw: Option<String> = row.get(3).ok();
        let patch_diff: String = row.get(4).unwrap_or_default();
        drop(rows);
        // The report tag and the stored provenance must agree, so both use the
        // shortened display form rather than the sha256 join key.
        let display_id =
            crate::cross_render::display_finding_id(&job.source_name, &finding.finding_id);

        connection
            .execute(
                "INSERT OR IGNORE INTO findings
                 (review_id, severity, severity_explanation, problem, preexisting,
                  locations, cross_review_job_id, external_finding_id)
                 VALUES (?, ?, ?, ?, 0, ?, ?, ?)",
                libsql::params![
                    review_id,
                    crate::db::Severity::from_str(&finding.severity) as i64,
                    finding.reasoning.clone(),
                    finding.problem.clone(),
                    finding.locations.to_string(),
                    job.id,
                    finding.finding_id.clone(),
                ],
            )
            .await?;

        if let (Some(interaction_id), Some(output_raw)) = (interaction_id, output_raw) {
            let mut output: serde_json::Value =
                serde_json::from_str(&crate::utils::clean_json_string(&output_raw))?;
            let review = if output.get("review").is_some() {
                output
                    .get_mut("review")
                    .ok_or_else(|| anyhow::anyhow!("stored review is unavailable"))?
            } else {
                &mut output
            };
            let findings = review["findings"]
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("stored review output has no findings array"))?;
            if !findings
                .iter()
                .any(|item| item["cross_review_finding_id"].as_str() == Some(&finding.finding_id))
            {
                findings.push(serde_json::json!({
                    "problem": finding.problem,
                    "severity": finding.severity,
                    "severity_explanation": finding.reasoning,
                    "preexisting": false,
                    "locations": finding.locations,
                    "source_models": [job.source_name],
                    "finding_ids": [display_id],
                    "cross_review_source": job.source_name,
                    "cross_review_finding_id": finding.finding_id,
                    "confirmed_by": "main",
                }));
                connection
                    .execute(
                        "UPDATE ai_interactions SET output_raw = ? WHERE id = ?",
                        libsql::params![output.to_string(), interaction_id],
                    )
                    .await?;
            }
        }
        // The finding tag doubles as the idempotency guard. It is derived from the
        // remote finding's content hash, so re-publishing the same finding, or a
        // second source's alias of it, cannot duplicate the comment block.
        let guard = format!("[Finding: {display_id}]");
        let current = inline_review.unwrap_or_default();
        if !current.contains(&guard) {
            let comment = rendered
                .map(|entry| entry.comment.clone())
                .unwrap_or_else(|| crate::cross_render::fallback_comment(finding));
            let block = crate::cross_render::comment_block(
                &finding.severity,
                &display_id,
                &job.source_name,
                &comment,
            );
            let updated = crate::cross_render::splice_comment_block(
                &current,
                &patch_diff,
                &block,
                rendered.and_then(|entry| entry.anchor.as_deref()),
                &finding.locations,
            );
            connection
                .execute(
                    "UPDATE reviews SET inline_review = ? WHERE id = ?",
                    libsql::params![updated, review_id],
                )
                .await?;
        }
        Ok(())
    }

    async fn merge_local_cross_review_provenance(
        &self,
        connection: &libsql::Connection,
        job: &CrossReviewJob,
        matched: &crate::cross_review::CrossLocalMatch,
    ) -> Result<()> {
        if matched.local_accepted {
            return self
                .update_review_finding_provenance(
                    connection,
                    matched.local_review_id,
                    None,
                    &matched.local_finding_ids,
                    std::slice::from_ref(&job.source_name),
                    std::slice::from_ref(&matched.finding_id),
                )
                .await;
        }

        let review_id = self
            .published_cross_review_finding_review_id(connection, job.id, &matched.finding_id)
            .await?;
        self.update_review_finding_provenance(
            connection,
            review_id,
            Some(&matched.finding_id),
            &[],
            &matched.local_source_models,
            &matched.local_finding_ids,
        )
        .await
    }

    async fn merge_imported_cross_review_provenance(
        &self,
        connection: &libsql::Connection,
        job: &CrossReviewJob,
        matched: &crate::cross_review::CrossRemoteMatch,
    ) -> Result<()> {
        let review_id = self
            .published_cross_review_finding_review_id(
                connection,
                matched.existing_job_id,
                &matched.existing_finding_id,
            )
            .await?;
        self.update_review_finding_provenance(
            connection,
            review_id,
            Some(&matched.existing_finding_id),
            &[],
            std::slice::from_ref(&job.source_name),
            std::slice::from_ref(&matched.finding_id),
        )
        .await
    }

    async fn published_cross_review_finding_review_id(
        &self,
        connection: &libsql::Connection,
        job_id: i64,
        finding_id: &str,
    ) -> Result<i64> {
        let mut rows = connection
            .query(
                "SELECT f.review_id FROM findings f
                 WHERE f.cross_review_job_id = ? AND f.external_finding_id = ?
                 LIMIT 1",
                libsql::params![job_id, finding_id],
            )
            .await?;
        let review_id = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("matched finding has no published local row"))?
            .get(0)?;
        drop(rows);
        Ok(review_id)
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_review_finding_provenance(
        &self,
        connection: &libsql::Connection,
        review_id: i64,
        cross_review_finding_id: Option<&str>,
        matched_finding_ids: &[String],
        source_models: &[String],
        finding_ids: &[String],
    ) -> Result<()> {
        let mut rows = connection
            .query(
                "SELECT r.interaction_id, ai.output_raw
                 FROM reviews r
                 JOIN ai_interactions ai ON ai.id = r.interaction_id
                 WHERE r.id = ?",
                libsql::params![review_id],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("matched review has no rendered output"))?;
        let interaction_id: String = row.get(0)?;
        let output_raw: String = row.get(1)?;
        drop(rows);

        let mut output: serde_json::Value =
            serde_json::from_str(&crate::utils::clean_json_string(&output_raw))?;
        let review = if output.get("review").is_some() {
            output
                .get_mut("review")
                .ok_or_else(|| anyhow::anyhow!("stored review is unavailable"))?
        } else {
            &mut output
        };
        let findings = review["findings"]
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("stored review output has no findings array"))?;
        let finding = findings
            .iter_mut()
            .find(|finding| {
                if let Some(cross_id) = cross_review_finding_id {
                    return finding["cross_review_finding_id"].as_str() == Some(cross_id);
                }
                finding["finding_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .any(|id| matched_finding_ids.iter().any(|matched| matched == id))
            })
            .ok_or_else(|| anyhow::anyhow!("matched imported finding is absent from output"))?;
        for source in source_models {
            append_unique_string(&mut finding["source_models"], source);
        }
        for finding_id in finding_ids {
            append_unique_string(&mut finding["finding_ids"], finding_id);
        }
        connection
            .execute(
                "UPDATE ai_interactions SET output_raw = ? WHERE id = ?",
                libsql::params![output.to_string(), interaction_id],
            )
            .await?;
        Ok(())
    }

    pub async fn save_model_experiment(
        &self,
        review_id: i64,
        experiment: &serde_json::Value,
        findings: &serde_json::Value,
    ) -> Result<()> {
        if self.is_in_memory {
            let _transaction_guard = self.in_memory_transaction.lock().await;
            self.conn.execute("BEGIN IMMEDIATE", ()).await?;
            let result = self
                .save_model_experiment_records(&self.conn, review_id, experiment, findings)
                .await;
            return match result {
                Ok(()) => {
                    self.conn.execute("COMMIT", ()).await?;
                    Ok(())
                }
                Err(error) => {
                    let _ = self.conn.execute("ROLLBACK", ()).await;
                    Err(error)
                }
            };
        }
        let connection = self.database.connect()?;
        let _ = connection
            .query("PRAGMA busy_timeout = 5000", ())
            .await?
            .next()
            .await;
        let transaction = connection
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let result = self
            .save_model_experiment_records(&transaction, review_id, experiment, findings)
            .await;
        match result {
            Ok(()) => {
                transaction.commit().await?;
                Ok(())
            }
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }

    async fn save_model_experiment_records(
        &self,
        connection: &libsql::Connection,
        review_id: i64,
        experiment: &serde_json::Value,
        findings: &serde_json::Value,
    ) -> Result<()> {
        use std::collections::HashMap;

        connection
            .execute(
                "DELETE FROM model_experiment_runs WHERE review_id = ?",
                libsql::params![review_id],
            )
            .await?;
        connection
            .execute(
                "DELETE FROM model_experiment_findings WHERE review_id = ?",
                libsql::params![review_id],
            )
            .await?;
        connection
            .execute(
                "DELETE FROM model_confirmation_runs WHERE review_id = ?",
                libsql::params![review_id],
            )
            .await?;
        connection
            .execute(
                "DELETE FROM model_experiment_sources WHERE review_id = ?",
                libsql::params![review_id],
            )
            .await?;

        let runs = experiment["runs"].as_array().cloned().unwrap_or_default();
        let mut sources: Vec<(String, String, String, bool)> = Vec::new();
        if let Some(main) = experiment["cohort"]["main"].as_object() {
            sources.push((
                main.get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("main")
                    .to_string(),
                main.get("provider")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                main.get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                true,
            ));
        }
        for member in experiment["cohort"]["variants"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(source) = member["source"].as_object() {
                sources.push((
                    source
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    source
                        .get("provider")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    source
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    member["selected"].as_bool().unwrap_or(false),
                ));
            }
        }
        if sources.is_empty() {
            let mut legacy_sources = std::collections::BTreeMap::new();
            for run in &runs {
                legacy_sources
                    .entry(run["model"].as_str().unwrap_or("unknown").to_string())
                    .or_insert_with(|| {
                        (
                            run["provider_id"].as_str().unwrap_or("unknown").to_string(),
                            run["model_id"].as_str().unwrap_or("unknown").to_string(),
                        )
                    });
            }
            sources.extend(
                legacy_sources
                    .into_iter()
                    .map(|(name, (provider, model))| (name, provider, model, true)),
            );
        }
        for (name, provider, model, selected) in sources {
            let source_runs: Vec<&serde_json::Value> = runs
                .iter()
                .filter(|run| run["model"].as_str() == Some(name.as_str()))
                .collect();
            let status = if !selected {
                "not_selected"
            } else if source_runs.is_empty()
                || source_runs
                    .iter()
                    .any(|run| run["status"].as_str() != Some("completed"))
            {
                "failed"
            } else {
                "completed"
            };
            let error = source_runs.iter().find_map(|run| run["error"].as_str());
            connection
                .execute(
                    "INSERT INTO model_experiment_sources (review_id, experiment_name, provider_id, model_id, selected, status, error) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    libsql::params![
                        review_id,
                        name,
                        provider,
                        model,
                        i64::from(selected),
                        status,
                        error,
                    ],
                )
                .await?;
        }

        for run in &runs {
            connection
                .execute(
                    "INSERT INTO model_experiment_runs (review_id, experiment_name, model_id, provider_id, stage, status, error, tokens_in, tokens_out, tokens_cached) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    libsql::params![
                        review_id,
                        run["model"].as_str().unwrap_or("unknown"),
                        run["model_id"].as_str().unwrap_or("unknown"),
                        run["provider_id"].as_str().unwrap_or("unknown"),
                        run["stage"].as_i64().unwrap_or_default(),
                        run["status"].as_str().unwrap_or("failed"),
                        run["error"].as_str(),
                        run["tokens_in"].as_i64().unwrap_or_default(),
                        run["tokens_out"].as_i64().unwrap_or_default(),
                        run["tokens_cached"].as_i64().unwrap_or_default(),
                    ],
                )
                .await?;
        }

        let mut severities = HashMap::new();
        for finding in findings.as_array().into_iter().flatten() {
            let severity = finding["severity"].as_str().unwrap_or("unknown");
            for id in finding["finding_ids"].as_array().into_iter().flatten() {
                if let Some(id) = id.as_str() {
                    severities.insert(id, severity);
                }
            }
        }
        for comparison in experiment["comparisons"].as_array().into_iter().flatten() {
            let finding_id = comparison["finding_id"].as_str().unwrap_or("unknown");
            connection
                .execute(
                    "INSERT INTO model_experiment_findings (review_id, additional_model, main_model_id, additional_model_id, main_provider_id, additional_provider_id, finding_id, outcome, severity, confirmed_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    libsql::params![
                        review_id,
                        comparison["additional_model"].as_str().unwrap_or("unknown"),
                        comparison["main_model_id"].as_str().unwrap_or("unknown"),
                        comparison["additional_model_id"].as_str().unwrap_or("unknown"),
                        comparison["main_provider_id"].as_str().unwrap_or("unknown"),
                        comparison["additional_provider_id"].as_str().unwrap_or("unknown"),
                        finding_id,
                        comparison["outcome"].as_str().unwrap_or("unknown"),
                        comparison["severity"]
                            .as_str()
                            .or_else(|| severities.get(finding_id).copied()),
                        comparison["confirmed_by"].as_str(),
                    ],
                )
                .await?;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        for run in experiment["confirmation_runs"]
            .as_array()
            .into_iter()
            .flatten()
        {
            connection
                .execute(
                    "INSERT INTO model_confirmation_runs (review_id, model, model_id, provider_id, status, error, tokens_in, tokens_out, tokens_cached, budget_input, budget_output, budget_flags, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    libsql::params![
                        review_id,
                        run["model"].as_str().unwrap_or("unknown"),
                        run["model_id"].as_str().unwrap_or("unknown"),
                        run["provider_id"].as_str().unwrap_or("unknown"),
                        run["status"].as_str().unwrap_or("failed"),
                        run["error"].as_str(),
                        run["tokens_in"].as_i64().unwrap_or_default(),
                        run["tokens_out"].as_i64().unwrap_or_default(),
                        run["tokens_cached"].as_i64().unwrap_or_default(),
                        run["budget_input"].as_i64().unwrap_or_default(),
                        run["budget_output"].as_i64().unwrap_or_default(),
                        run["budget_flags"].as_i64().unwrap_or_default(),
                        now,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    pub async fn get_model_experiment_stats(&self) -> Result<serde_json::Value> {
        let mut run_status = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT experiment_name, provider_id, model_id, status, count(*) FROM model_experiment_runs GROUP BY experiment_name, provider_id, model_id, status ORDER BY experiment_name, provider_id, model_id, status",
                (),
            )
            .await?;
        while let Ok(Some(row)) = rows.next().await {
            run_status.push(json!({
                "experiment_name": row.get::<String>(0)?,
                "provider": row.get::<String>(1)?,
                "model": row.get::<String>(2)?,
                "status": row.get::<String>(3)?,
                "count": row.get::<i64>(4)?,
            }));
        }
        drop(rows);

        let mut confirmation_outcomes = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT confirmed_by, outcome, severity, count(*)
             FROM (
                 SELECT DISTINCT review_id, finding_id, confirmed_by, outcome,
                        lower(COALESCE(severity, 'unknown')) AS severity
                 FROM model_experiment_findings
                 WHERE confirmed_by IS NOT NULL
             )
             GROUP BY confirmed_by, outcome, severity
             ORDER BY confirmed_by, outcome, severity",
                (),
            )
            .await?;
        while let Ok(Some(row)) = rows.next().await {
            confirmation_outcomes.push(json!({
                "confirmed_by": row.get::<String>(0)?,
                "outcome": row.get::<String>(1)?,
                "severity": row.get::<String>(2)?,
                "count": row.get::<i64>(3)?,
            }));
        }
        drop(rows);

        let mut cohort_status = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT experiment_name, provider_id, model_id, selected, status, count(*) FROM model_experiment_sources GROUP BY experiment_name, provider_id, model_id, selected, status ORDER BY experiment_name, provider_id, model_id, selected, status",
                (),
            )
            .await?;
        while let Ok(Some(row)) = rows.next().await {
            cohort_status.push(json!({
                "experiment_name": row.get::<String>(0)?,
                "provider": row.get::<String>(1)?,
                "model": row.get::<String>(2)?,
                "selected": row.get::<i64>(3)? != 0,
                "status": row.get::<String>(4)?,
                "count": row.get::<i64>(5)?,
            }));
        }
        drop(rows);

        let mut outcomes = Vec::new();
        let mut rows = self.conn.query(
            "SELECT additional_model, main_provider_id, main_model_id, additional_provider_id, additional_model_id, outcome, lower(COALESCE(severity, 'unknown')), count(*) FROM model_experiment_findings GROUP BY additional_model, main_provider_id, main_model_id, additional_provider_id, additional_model_id, outcome, lower(COALESCE(severity, 'unknown')) ORDER BY additional_model, main_provider_id, main_model_id, additional_provider_id, additional_model_id, outcome",
            (),
        ).await?;
        while let Ok(Some(row)) = rows.next().await {
            outcomes.push(json!({
                "additional_model": row.get::<String>(0)?,
                "main_provider_id": row.get::<String>(1)?,
                "main_model_id": row.get::<String>(2)?,
                "additional_provider_id": row.get::<String>(3)?,
                "additional_model_id": row.get::<String>(4)?,
                "outcome": row.get::<String>(5)?,
                "severity": row.get::<String>(6)?,
                "count": row.get::<i64>(7)?,
            }));
        }
        drop(rows);

        let mut paired_cost = Vec::new();
        let mut rows = self.conn.query(
            "SELECT v.experiment_name, m.provider_id, m.model_id, v.provider_id, v.model_id, avg(m.tokens_in), avg(m.tokens_out), avg(m.tokens_cached), avg(v.tokens_in), avg(v.tokens_out), avg(v.tokens_cached), count(*), count(DISTINCT COALESCE(r.patch_id, -r.id)) FROM model_experiment_runs m JOIN model_experiment_runs v ON m.review_id = v.review_id AND m.stage = v.stage JOIN reviews r ON r.id = m.review_id WHERE m.experiment_name = 'main' AND v.experiment_name != 'main' AND m.status = 'completed' AND v.status = 'completed' GROUP BY v.experiment_name, m.provider_id, m.model_id, v.provider_id, v.model_id ORDER BY v.experiment_name, m.provider_id, m.model_id, v.provider_id, v.model_id",
            (),
        ).await?;
        while let Ok(Some(row)) = rows.next().await {
            paired_cost.push(json!({
                "additional_model": row.get::<String>(0)?,
                "main": {"provider": row.get::<String>(1).unwrap_or_default(), "model": row.get::<String>(2).unwrap_or_default(), "tokens_in": row.get::<f64>(5).unwrap_or(0.0), "tokens_out": row.get::<f64>(6).unwrap_or(0.0), "tokens_cached": row.get::<f64>(7).unwrap_or(0.0)},
                "additional": {"provider": row.get::<String>(3).unwrap_or_default(), "model": row.get::<String>(4).unwrap_or_default(), "tokens_in": row.get::<f64>(8).unwrap_or(0.0), "tokens_out": row.get::<f64>(9).unwrap_or(0.0), "tokens_cached": row.get::<f64>(10).unwrap_or(0.0)},
                "paired_stages": row.get::<i64>(11).unwrap_or(0),
                "compared_patches": row.get::<i64>(12).unwrap_or(0),
            }));
        }
        drop(rows);

        let mut confirmation_cost = Vec::new();
        let mut rows = self.conn.query(
            "SELECT strftime('%Y-%m-%d', created_at, 'unixepoch'), provider_id, model_id, sum(tokens_in), sum(tokens_out), sum(tokens_cached) FROM model_confirmation_runs GROUP BY 1, provider_id, model_id ORDER BY 1, provider_id, model_id",
            (),
        ).await?;
        while let Ok(Some(row)) = rows.next().await {
            confirmation_cost.push(json!({
                "day": row.get::<String>(0)?, "provider": row.get::<String>(1)?, "model": row.get::<String>(2)?,
                "tokens_in": row.get::<i64>(3).unwrap_or(0), "tokens_out": row.get::<i64>(4).unwrap_or(0),
                "tokens_cached": row.get::<i64>(5).unwrap_or(0),
            }));
        }
        drop(rows);

        let mut confirmation_health = Vec::new();
        let mut rows = self.conn.query(
            "SELECT model, provider_id, model_id, status, count(*), max(COALESCE(error, '')) FROM model_confirmation_runs GROUP BY model, provider_id, model_id, status ORDER BY model, provider_id, model_id, status",
            (),
        ).await?;
        while let Ok(Some(row)) = rows.next().await {
            confirmation_health.push(json!({
                "model": row.get::<String>(0)?,
                "provider": row.get::<String>(1)?,
                "model_id": row.get::<String>(2)?,
                "status": row.get::<String>(3)?,
                "count": row.get::<i64>(4)?,
                "sample_error": row.get::<String>(5).unwrap_or_default(),
            }));
        }
        Ok(
            json!({"run_status": run_status, "cohort_status": cohort_status, "outcomes": outcomes, "confirmation_outcomes": confirmation_outcomes, "paired_cost": paired_cost, "confirmation_cost": confirmation_cost, "confirmation_health": confirmation_health}),
        )
    }

    pub async fn get_cross_review_stats(&self) -> Result<serde_json::Value> {
        let mut sources = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT j.source_name, j.local_provider,
                        j.local_model, COALESCE(j.remote_provider, ''),
                        COALESCE(j.remote_model, ''), j.status, COUNT(*)
                 FROM cross_review_jobs j
                 GROUP BY j.source_name, j.local_provider, j.local_model,
                          j.remote_provider, j.remote_model, j.status
                 ORDER BY j.source_name, j.status",
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            sources.push(json!({
                "source": row.get::<String>(0)?,
                "local_provider": row.get::<String>(1)?,
                "local_model": row.get::<String>(2)?,
                "remote_provider": row.get::<String>(3)?,
                "remote_model": row.get::<String>(4)?,
                "status": row.get::<String>(5)?,
                "count": row.get::<i64>(6)?,
            }));
        }
        drop(rows);
        let mut outcomes = Vec::new();
        let mut rows = self
            .conn
            .query(
                "SELECT j.source_name, j.local_provider,
                        j.local_model, COALESCE(j.remote_provider, ''),
                        COALESCE(j.remote_model, ''), c.outcome,
                        lower(COALESCE(c.severity, 'unknown')), COUNT(*)
                 FROM cross_review_comparisons c
                 JOIN cross_review_jobs j ON j.id = c.job_id
                 GROUP BY j.source_name, j.local_provider, j.local_model,
                          j.remote_provider, j.remote_model, c.outcome,
                          lower(COALESCE(c.severity, 'unknown'))
                 ORDER BY j.source_name, c.outcome",
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            outcomes.push(json!({
                "source": row.get::<String>(0)?,
                "local_provider": row.get::<String>(1)?,
                "local_model": row.get::<String>(2)?,
                "remote_provider": row.get::<String>(3)?,
                "remote_model": row.get::<String>(4)?,
                "outcome": row.get::<String>(5)?,
                "severity": row.get::<String>(6)?,
                "count": row.get::<i64>(7)?,
            }));
        }
        Ok(json!({"sources": sources, "outcomes": outcomes}))
    }

    pub async fn get_cross_review_status(&self, patchset_id: i64) -> Result<serde_json::Value> {
        let mut rows = self
            .conn
            .query(
                "SELECT cross_review_status, cross_reviewed_at
                 FROM patchsets WHERE id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(serde_json::Value::Null);
        };
        let status = row
            .get::<String>(0)
            .unwrap_or_else(|_| "disabled".to_string());
        let completed_at = row.get::<Option<i64>>(1).ok().flatten();
        drop(rows);

        let mut source_rows = self
            .conn
            .query(
                "SELECT j.source_name, j.status, j.remote_model,
                        j.remote_provider
                 FROM cross_review_jobs j
                 JOIN patchsets p ON p.id = j.patchset_id
                 WHERE j.patchset_id = ?
                   AND j.generation = p.cross_review_generation
                 ORDER BY j.source_name",
                libsql::params![patchset_id],
            )
            .await?;
        let mut sources = Vec::new();
        while let Some(source) = source_rows.next().await? {
            sources.push(json!({
                "name": source.get::<String>(0)?,
                "display_name": source.get::<String>(0)?,
                "kind": "cross_review",
                "status": source.get::<String>(1)?,
                "model": source.get::<Option<String>>(2).ok().flatten(),
                "provider": source.get::<Option<String>>(3).ok().flatten(),
            }));
        }
        Ok(json!({
            "status": status,
            "completed_at": completed_at,
            "sources": sources,
        }))
    }

    pub async fn create_ai_interaction(&self, params: AiInteractionParams<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ai_interactions (id, parent_interaction_id, workflow_id, provider, model, input_context, output_raw, tokens_in, tokens_out, tokens_cached, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                params.id,
                params.parent_id,
                params.workflow_id,
                params.provider,
                params.model,
                params.input,
                params.output,
                params.tokens_in,
                params.tokens_out,
                params.tokens_cached,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
            ],
        ).await?;
        Ok(())
    }

    pub async fn create_tool_usage(&self, usage: ToolUsage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_usages (review_id, provider, model, tool_name, arguments, output_length, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                usage.review_id,
                usage.provider,
                usage.model,
                usage.tool_name,
                usage.arguments,
                usage.output_length,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
            ],
        ).await?;
        Ok(())
    }

    pub async fn update_tool_usage_length(
        &self,
        review_id: i64,
        tool_name: &str,
        arguments: &str,
        output_length: usize,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE tool_usages 
                 SET output_length = ? 
                 WHERE id = (
                     SELECT id FROM tool_usages 
                     WHERE review_id = ? AND tool_name = ? AND arguments = ? AND output_length = 0
                     ORDER BY id DESC LIMIT 1
                 )",
                libsql::params![output_length as i64, review_id, tool_name, arguments],
            )
            .await?;
        Ok(())
    }

    pub async fn create_finding(&self, finding: Finding) -> Result<()> {
        let preexisting_val = finding.preexisting.map(|b| if b { 1 } else { 0 });
        let locations_val = finding
            .locations
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        self.conn
            .execute(
                "INSERT INTO findings (review_id, severity, severity_explanation, problem, preexisting, locations, source_stages)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    finding.review_id,
                    finding.severity as i32,
                    finding.severity_explanation,
                    finding.problem,
                    preexisting_val,
                    locations_val,
                    finding.source_stages,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn get_timeline_stats(&self, subsystem_id: Option<i64>) -> Result<serde_json::Value> {
        let mut messages_data = Vec::new();

        if let Some(sid) = subsystem_id {
            let sql_msgs =
                "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, count(*) FROM messages m
             JOIN messages_subsystems ms ON m.id = ms.message_id
             WHERE ms.subsystem_id = ?
             GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql_msgs, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    messages_data.push(json!({"day": day, "count": count}));
                }
            }
        } else {
            let sql_msgs = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, count(*) FROM messages GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql_msgs, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    messages_data.push(json!({"day": day, "count": count}));
                }
            }
        }

        let mut patchsets_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, status, count(*) FROM patchsets p
             JOIN patchsets_subsystems ps ON p.id = ps.patchset_id
             WHERE ps.subsystem_id = ?
             GROUP BY day, status ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: Option<String> = row.get(1).ok();
                    let count: i64 = row.get(2)?;
                    patchsets_data.push(
                        json!({"day": day, "status": status.unwrap_or_default(), "count": count}),
                    );
                }
            }
        } else {
            let sql = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, status, count(*) FROM patchsets GROUP BY day, status ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: Option<String> = row.get(1).ok();
                    let count: i64 = row.get(2)?;
                    patchsets_data.push(
                        json!({"day": day, "status": status.unwrap_or_default(), "count": count}),
                    );
                }
            }
        }

        // Patches stats (individual patches)
        let mut patches_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql =
                "SELECT strftime('%Y-%m-%d', m.date, 'unixepoch') as day, count(*) FROM patches p
              JOIN messages m ON p.message_id = m.message_id
              JOIN patches_subsystems ps ON p.id = ps.patch_id
              WHERE ps.subsystem_id = ?
              GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    patches_data.push(json!({"day": day, "count": count}));
                }
            }
        } else {
            let sql =
                "SELECT strftime('%Y-%m-%d', m.date, 'unixepoch') as day, count(*) FROM patches p
              JOIN messages m ON p.message_id = m.message_id
              GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    patches_data.push(json!({"day": day, "count": count}));
                }
            }
        }

        // Reviews stats (outcomes over time)
        let mut reviews_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                r.status,
                COUNT(*) as count
            FROM reviews r
            JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id
            WHERE ps.subsystem_id = ?
            GROUP BY day, status
            ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    reviews_data.push(json!({"day": day, "status": status, "count": count}));
                }
            }
        } else {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                r.status,
                COUNT(*) as count
            FROM reviews r
            GROUP BY day, status
            ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    reviews_data.push(json!({"day": day, "status": status, "count": count}));
                }
            }
        }

        // Findings stats
        let mut findings_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                CASE f.severity 
                    WHEN 1 THEN 'low' 
                    WHEN 2 THEN 'medium' 
                    WHEN 3 THEN 'high' 
                    WHEN 4 THEN 'critical' 
                    ELSE 'unknown' 
                END as severity,
                COUNT(*) as count
            FROM findings f
            JOIN reviews r ON f.review_id = r.id
            JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id
            WHERE ps.subsystem_id = ?
            GROUP BY day, severity
            ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let severity: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    findings_data.push(json!({"day": day, "severity": severity, "count": count}));
                }
            }
        } else {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                CASE f.severity 
                    WHEN 1 THEN 'low' 
                    WHEN 2 THEN 'medium' 
                    WHEN 3 THEN 'high' 
                    WHEN 4 THEN 'critical' 
                    ELSE 'unknown' 
                END as severity,
                COUNT(*) as count
            FROM findings f
            JOIN reviews r ON f.review_id = r.id
            GROUP BY day, severity
            ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let severity: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    findings_data.push(json!({"day": day, "severity": severity, "count": count}));
                }
            }
        }

        // Attribute shared findings and rejected main-model candidates to every
        // stage that contributed to them, then normalize against main-model stage
        // invocations. Starting from stage engagements also keeps stages with no
        // findings visible in the chart.
        let subsystem_join = if subsystem_id.is_some() {
            "JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id"
        } else {
            ""
        };
        let subsystem_filter = if subsystem_id.is_some() {
            "AND ps.subsystem_id = ?"
        } else {
            ""
        };
        let sql = format!(
            "WITH eligible_reviews AS (
                 SELECT DISTINCT r.id
                 FROM reviews r
                 {}
                 WHERE r.created_at >= unixepoch('now', '-13 days', 'start of day')
                   {}
             ),
             stage_engagements AS (
                 SELECT mer.stage, COUNT(DISTINCT mer.review_id) AS engagements
                 FROM model_experiment_runs mer
                 JOIN eligible_reviews er ON er.id = mer.review_id
                 WHERE mer.experiment_name = 'main'
                 GROUP BY mer.stage
             ),
             finding_counts AS (
                 SELECT CAST(stage.value AS INTEGER) AS stage,
                        COUNT(DISTINCT CASE WHEN json_array_length(f.source_stages) = 1 THEN f.id END) AS unique_findings,
                        COUNT(DISTINCT CASE WHEN json_array_length(f.source_stages) > 1 THEN f.id END) AS duplicate_findings,
                        COUNT(DISTINCT CASE WHEN json_array_length(f.source_stages) = 1 AND f.severity = 4 THEN f.id END) AS unique_critical,
                        COUNT(DISTINCT CASE WHEN json_array_length(f.source_stages) = 1 AND f.severity = 3 THEN f.id END) AS unique_high,
                        COUNT(DISTINCT CASE WHEN json_array_length(f.source_stages) = 1 AND f.severity = 2 THEN f.id END) AS unique_medium,
                        COUNT(DISTINCT CASE WHEN json_array_length(f.source_stages) = 1 AND f.severity = 1 THEN f.id END) AS unique_low
                 FROM findings f
                 JOIN eligible_reviews er ON er.id = f.review_id
                 JOIN json_each(
                    CASE WHEN json_valid(f.source_stages) THEN f.source_stages ELSE '[]' END
                 ) AS stage
                 WHERE json_valid(f.source_stages)
                   AND json_type(f.source_stages) = 'array'
                   AND json_array_length(f.source_stages) > 0
                   AND stage.type = 'integer'
                 GROUP BY stage.value
             ),
             rejected_main_findings AS (
                 SELECT lcf.id,
                        CASE WHEN json_valid(lcf.finding_json)
                             THEN lcf.finding_json ELSE '{{}}' END AS finding_json
                 FROM local_canonical_findings lcf
                 JOIN eligible_reviews er ON er.id = lcf.review_id
                 WHERE lcf.accepted = 0
             ),
             hallucination_counts AS (
                 SELECT CAST(stage.value AS INTEGER) AS stage,
                        COUNT(DISTINCT rejected.id) AS hallucinations
                 FROM rejected_main_findings rejected
                 JOIN json_each(rejected.finding_json, '$.source_stages') AS stage
                 WHERE json_type(rejected.finding_json, '$.source_stages') = 'array'
                   AND stage.type = 'integer'
                   AND json_type(rejected.finding_json, '$.source_models') = 'array'
                   AND EXISTS (
                       SELECT 1
                       FROM json_each(rejected.finding_json, '$.source_models') AS model
                       WHERE model.type = 'text' AND model.value = 'main'
                   )
                 GROUP BY stage.value
             )
             SELECT se.stage,
                    COALESCE(fc.unique_findings, 0),
                    COALESCE(fc.duplicate_findings, 0),
                    COALESCE(hc.hallucinations, 0),
                    se.engagements,
                    COALESCE(fc.unique_critical, 0),
                    COALESCE(fc.unique_high, 0),
                    COALESCE(fc.unique_medium, 0),
                    COALESCE(fc.unique_low, 0)
             FROM stage_engagements se
             LEFT JOIN finding_counts fc ON fc.stage = se.stage
             LEFT JOIN hallucination_counts hc ON hc.stage = se.stage
             ORDER BY se.stage",
            subsystem_join, subsystem_filter
        );
        let mut findings_by_stage = Vec::new();
        let mut rows = match subsystem_id {
            Some(sid) => self.conn.query(&sql, libsql::params![sid]).await?,
            None => self.conn.query(&sql, ()).await?,
        };
        while let Ok(Some(row)) = rows.next().await {
            findings_by_stage.push(json!({
                "stage": row.get::<i64>(0)?,
                "unique": row.get::<i64>(1)?,
                "duplicates": row.get::<i64>(2)?,
                "hallucinations": row.get::<i64>(3)?,
                "engagements": row.get::<i64>(4)?,
                "unique_by_severity": {
                    "critical": row.get::<i64>(5)?,
                    "high": row.get::<i64>(6)?,
                    "medium": row.get::<i64>(7)?,
                    "low": row.get::<i64>(8)?,
                },
            }));
        }

        Ok(json!({
            "messages": messages_data,
            "patchsets": patchsets_data,
            "patches": patches_data,
            "reviews": reviews_data,
            "findings": findings_data,
            "findings_by_stage": findings_by_stage
        }))
    }

    /// Per-day aggregates used by the cost dashboard: tokens (by model), review
    /// throughput, errors, and budget-pressure counts, plus a with/without
    /// findings split that carries tokens and duration so the UI can compute
    /// average cost and wall-clock per patch.
    ///
    /// Budget bit layout (keep in sync with worker::prompts constants):
    ///   warn bits   = 0x01 | 0x04 | 0x10 | 0x40 = 0x55
    ///   severe bits = 0x02 | 0x08 | 0x20 | 0x80 = 0xAA
    /// A review that triggered any severe bit is counted as `budget_severe`
    /// and deliberately not also counted as `budget_warn` — severe subsumes
    /// warn.
    pub async fn get_cost_stats(&self, subsystem_id: Option<i64>) -> Result<serde_json::Value> {
        use std::collections::BTreeMap;

        let subsystem_join = if subsystem_id.is_some() {
            "JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id"
        } else {
            ""
        };
        let subsystem_filter = if subsystem_id.is_some() {
            "WHERE ps.subsystem_id = ?"
        } else {
            ""
        };

        // Query A: per-day aggregates across the whole reviews table.
        let sql_a = format!(
            "SELECT
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') AS day,
                SUM(CASE WHEN r.status = 'Reviewed' THEN 1 ELSE 0 END) AS reviews_total,
                COUNT(DISTINCT CASE WHEN r.status = 'Reviewed' THEN r.patch_id END) AS patches_reviewed,
                SUM(CASE WHEN r.status IN ('Failed','Failed To Apply') THEN 1 ELSE 0 END) AS errors,
                SUM(CASE WHEN (COALESCE(r.budget_flags,0) & 0x55) != 0
                              AND (COALESCE(r.budget_flags,0) & 0xAA) = 0
                         THEN 1 ELSE 0 END) AS budget_warn,
                SUM(CASE WHEN (COALESCE(r.budget_flags,0) & 0xAA) != 0 THEN 1 ELSE 0 END) AS budget_severe,

                SUM(CASE WHEN r.status = 'Reviewed' AND EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN 1 ELSE 0 END) AS wf_count,
                SUM(CASE WHEN r.status = 'Reviewed' AND EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN COALESCE(ai.tokens_in,0) ELSE 0 END) AS wf_tokens_in,
                SUM(CASE WHEN r.status = 'Reviewed' AND EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN COALESCE(ai.tokens_out,0) ELSE 0 END) AS wf_tokens_out,
                SUM(CASE WHEN r.status = 'Reviewed' AND EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN COALESCE(ai.tokens_cached,0) ELSE 0 END) AS wf_tokens_cached,
                SUM(CASE WHEN r.status = 'Reviewed' AND EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) AND r.completed_at IS NOT NULL THEN (r.completed_at - r.created_at) ELSE 0 END) AS wf_duration_sum_sec,
                SUM(CASE WHEN r.status = 'Reviewed' AND EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) AND r.completed_at IS NOT NULL THEN 1 ELSE 0 END) AS wf_duration_count,

                SUM(CASE WHEN r.status = 'Reviewed' AND NOT EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN 1 ELSE 0 END) AS nf_count,
                SUM(CASE WHEN r.status = 'Reviewed' AND NOT EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN COALESCE(ai.tokens_in,0) ELSE 0 END) AS nf_tokens_in,
                SUM(CASE WHEN r.status = 'Reviewed' AND NOT EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN COALESCE(ai.tokens_out,0) ELSE 0 END) AS nf_tokens_out,
                SUM(CASE WHEN r.status = 'Reviewed' AND NOT EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) THEN COALESCE(ai.tokens_cached,0) ELSE 0 END) AS nf_tokens_cached,
                SUM(CASE WHEN r.status = 'Reviewed' AND NOT EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) AND r.completed_at IS NOT NULL THEN (r.completed_at - r.created_at) ELSE 0 END) AS nf_duration_sum_sec,
                SUM(CASE WHEN r.status = 'Reviewed' AND NOT EXISTS(SELECT 1 FROM findings f WHERE f.review_id = r.id) AND r.completed_at IS NOT NULL THEN 1 ELSE 0 END) AS nf_duration_count
            FROM reviews r
            LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
            {}
            {}
            GROUP BY day
            ORDER BY day",
            subsystem_join, subsystem_filter
        );

        let mut daily: BTreeMap<String, serde_json::Value> = BTreeMap::new();

        let mut rows_a = match subsystem_id {
            Some(sid) => self.conn.query(&sql_a, libsql::params![sid]).await?,
            None => self.conn.query(&sql_a, ()).await?,
        };
        while let Ok(Some(row)) = rows_a.next().await {
            let Ok(day) = row.get::<String>(0) else {
                continue;
            };
            let reviews_total: i64 = row.get(1).unwrap_or(0);
            let patches_reviewed: i64 = row.get(2).unwrap_or(0);
            let errors: i64 = row.get(3).unwrap_or(0);
            let budget_warn: i64 = row.get(4).unwrap_or(0);
            let budget_severe: i64 = row.get(5).unwrap_or(0);
            let wf_count: i64 = row.get(6).unwrap_or(0);
            let wf_tokens_in: i64 = row.get(7).unwrap_or(0);
            let wf_tokens_out: i64 = row.get(8).unwrap_or(0);
            let wf_tokens_cached: i64 = row.get(9).unwrap_or(0);
            let wf_duration_sum_sec: i64 = row.get(10).unwrap_or(0);
            let wf_duration_count: i64 = row.get(11).unwrap_or(0);
            let nf_count: i64 = row.get(12).unwrap_or(0);
            let nf_tokens_in: i64 = row.get(13).unwrap_or(0);
            let nf_tokens_out: i64 = row.get(14).unwrap_or(0);
            let nf_tokens_cached: i64 = row.get(15).unwrap_or(0);
            let nf_duration_sum_sec: i64 = row.get(16).unwrap_or(0);
            let nf_duration_count: i64 = row.get(17).unwrap_or(0);

            daily.insert(
                day.clone(),
                json!({
                    "day": day,
                    "by_model": Vec::<serde_json::Value>::new(),
                    "reviews_total": reviews_total,
                    "patches_reviewed": patches_reviewed,
                    "errors": errors,
                    "budget_warn": budget_warn,
                    "budget_severe": budget_severe,
                    "with_findings": {
                        "count": wf_count,
                        "tokens_in": wf_tokens_in,
                        "tokens_out": wf_tokens_out,
                        "tokens_cached": wf_tokens_cached,
                        "duration_sum_sec": wf_duration_sum_sec,
                        "duration_count": wf_duration_count,
                    },
                    "without_findings": {
                        "count": nf_count,
                        "tokens_in": nf_tokens_in,
                        "tokens_out": nf_tokens_out,
                        "tokens_cached": nf_tokens_cached,
                        "duration_sum_sec": nf_duration_sum_sec,
                        "duration_count": nf_duration_count,
                    },
                }),
            );
        }

        // Query B: per-day-per-model token sums (for stacked cost-by-model).
        //
        // The review's ai_interactions row contains the complete main-model
        // workflow, including its discovery and merge stages.  Variant stages
        // and confirmation requests are deliberately accounted separately,
        // however, as are the local merge requests made for cross reviews.
        // Union those supplemental invocations here so this is a service-wide
        // usage view, while excluding experiment_name = 'main' because those
        // tokens are already included in ai_interactions.
        let supplemental_subsystem_join = if subsystem_id.is_some() {
            "JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id"
        } else {
            ""
        };
        let supplemental_subsystem_filter = if subsystem_id.is_some() {
            "AND ps.subsystem_id = ?"
        } else {
            ""
        };
        let cross_review_subsystem_join = if subsystem_id.is_some() {
            "JOIN patchsets_subsystems ps ON p.id = ps.patchset_id"
        } else {
            ""
        };
        let cross_review_subsystem_filter = if subsystem_id.is_some() {
            "WHERE ps.subsystem_id = ?"
        } else {
            ""
        };
        let sql_b = format!(
            "SELECT day, provider, model,
                    SUM(tokens_in), SUM(tokens_out), SUM(tokens_cached),
                    SUM(reviews)
             FROM (
                SELECT strftime('%Y-%m-%d', r.created_at, 'unixepoch') AS day,
                       COALESCE(r.provider, '') AS provider,
                       COALESCE(r.model, '') AS model,
                       SUM(COALESCE(ai.tokens_in, 0)) AS tokens_in,
                       SUM(COALESCE(ai.tokens_out, 0)) AS tokens_out,
                       SUM(COALESCE(ai.tokens_cached, 0)) AS tokens_cached,
                       COUNT(*) AS reviews
                FROM reviews r
                LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                {subsystem_join}
                {subsystem_filter}
                GROUP BY day, r.provider, r.model

                UNION ALL

                SELECT strftime('%Y-%m-%d', r.created_at, 'unixepoch') AS day,
                       COALESCE(run.provider_id, '') AS provider,
                       COALESCE(run.model_id, '') AS model,
                       SUM(COALESCE(run.tokens_in, 0)) AS tokens_in,
                       SUM(COALESCE(run.tokens_out, 0)) AS tokens_out,
                       SUM(COALESCE(run.tokens_cached, 0)) AS tokens_cached,
                       COUNT(DISTINCT run.review_id) AS reviews
                FROM model_experiment_runs run
                JOIN reviews r ON r.id = run.review_id
                {supplemental_subsystem_join}
                WHERE run.experiment_name != 'main'
                {supplemental_subsystem_filter}
                GROUP BY day, run.provider_id, run.model_id

                UNION ALL

                SELECT strftime('%Y-%m-%d', r.created_at, 'unixepoch') AS day,
                       COALESCE(run.provider_id, '') AS provider,
                       COALESCE(run.model_id, '') AS model,
                       SUM(COALESCE(run.tokens_in, 0)) AS tokens_in,
                       SUM(COALESCE(run.tokens_out, 0)) AS tokens_out,
                       SUM(COALESCE(run.tokens_cached, 0)) AS tokens_cached,
                       COUNT(DISTINCT run.review_id) AS reviews
                FROM model_confirmation_runs run
                JOIN reviews r ON r.id = run.review_id
                {supplemental_subsystem_join}
                WHERE 1 = 1
                {supplemental_subsystem_filter}
                GROUP BY day, run.provider_id, run.model_id

                UNION ALL

                SELECT strftime('%Y-%m-%d',
                                COALESCE(job.completed_at, job.first_attempt_at),
                                'unixepoch') AS day,
                       COALESCE(job.local_provider, '') AS provider,
                       COALESCE(job.local_model, '') AS model,
                       SUM(COALESCE(job.merge_tokens_in, 0)) AS tokens_in,
                       SUM(COALESCE(job.merge_tokens_out, 0)) AS tokens_out,
                       SUM(COALESCE(job.merge_tokens_cached, 0)) AS tokens_cached,
                       COUNT(*) AS reviews
                FROM cross_review_jobs job
                JOIN patchsets p ON p.id = job.patchset_id
                {cross_review_subsystem_join}
                {cross_review_subsystem_filter}
                GROUP BY day, job.local_provider, job.local_model
             ) usage
             GROUP BY day, provider, model
             ORDER BY day, model"
        );
        let mut rows_b = match subsystem_id {
            Some(sid) => {
                self.conn
                    .query(&sql_b, libsql::params![sid, sid, sid, sid])
                    .await?
            }
            None => self.conn.query(&sql_b, ()).await?,
        };
        while let Ok(Some(row)) = rows_b.next().await {
            let Ok(day) = row.get::<String>(0) else {
                continue;
            };
            let provider: String = row.get(1).unwrap_or_default();
            let model: String = row.get(2).unwrap_or_default();
            let tokens_in: i64 = row.get(3).unwrap_or(0);
            let tokens_out: i64 = row.get(4).unwrap_or(0);
            let tokens_cached: i64 = row.get(5).unwrap_or(0);
            let reviews: i64 = row.get(6).unwrap_or(0);

            let entry = daily.entry(day.clone()).or_insert_with(|| {
                json!({
                    "day": day,
                    "by_model": Vec::<serde_json::Value>::new(),
                    "reviews_total": 0,
                    "patches_reviewed": 0,
                    "errors": 0,
                    "budget_warn": 0,
                    "budget_severe": 0,
                    "with_findings": {
                        "count": 0, "tokens_in": 0, "tokens_out": 0,
                        "tokens_cached": 0, "duration_sum_sec": 0, "duration_count": 0,
                    },
                    "without_findings": {
                        "count": 0, "tokens_in": 0, "tokens_out": 0,
                        "tokens_cached": 0, "duration_sum_sec": 0, "duration_count": 0,
                    },
                })
            });
            if let Some(arr) = entry.get_mut("by_model").and_then(|v| v.as_array_mut()) {
                arr.push(json!({
                    "provider": provider,
                    "model": model,
                    "tokens_in": tokens_in,
                    "tokens_out": tokens_out,
                    "tokens_cached": tokens_cached,
                    "reviews": reviews,
                }));
            }
        }

        let ordered: Vec<serde_json::Value> = daily.into_values().collect();
        Ok(json!({ "daily": ordered }))
    }

    pub async fn get_review_stats(&self) -> Result<serde_json::Value> {
        let mut total_rows = self
            .conn
            .query(
                "SELECT count(*) FROM reviews WHERE status NOT IN ('Pending', 'In Review')",
                (),
            )
            .await?;
        let total_reviews: i64 = if let Ok(Some(row)) = total_rows.next().await {
            row.get(0).unwrap_or(0)
        } else {
            0
        };

        let mut failed_rows = self
            .conn
            .query(
                "SELECT count(*) FROM reviews WHERE status NOT IN ('Pending', 'In Review') AND (lower(status) LIKE '%failed%' OR lower(status) LIKE '%error%')",
                (),
            )
            .await?;
        let total_failures: i64 = if let Ok(Some(row)) = failed_rows.next().await {
            row.get(0).unwrap_or(0)
        } else {
            0
        };

        let sql = "WITH last_reviews AS (
            SELECT * FROM reviews ORDER BY id DESC LIMIT 1000
        )
        SELECT
            r.provider,
            r.model,
            r.status,
            count(*),
            sum(COALESCE(ai.tokens_in, 0)),
            sum(COALESCE(ai.tokens_out, 0)),
            sum(COALESCE(ai.tokens_cached, 0))
        FROM last_reviews r
        LEFT JOIN ai_interactions ai INDEXED BY idx_ai_interactions_tokens ON r.interaction_id = ai.id
        GROUP BY r.provider, r.model, r.status";

        let mut rows = self.conn.query(sql, ()).await?;
        let mut stats = Vec::new();
        #[allow(clippy::similar_names)]
        while let Ok(Some(row)) = rows.next().await {
            let provider: Option<String> = row.get(0).ok();
            let model: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let count: i64 = row.get(3)?;
            let tokens_in: i64 = row.get(4).unwrap_or(0);
            let tokens_out: i64 = row.get(5).unwrap_or(0);
            let tokens_cached: i64 = row.get(6).unwrap_or(0);

            stats.push(json!({
                "provider": provider.unwrap_or_default(),
                "model": model.unwrap_or_default(),
                "status": status.unwrap_or_default(),
                "count": count,
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "tokens_cached": tokens_cached
            }));
        }

        Ok(json!({
            "total_reviews": total_reviews,
            "total_failures": total_failures,
            "reviews": stats
        }))
    }

    pub async fn get_tool_usage_stats(&self) -> Result<serde_json::Value> {
        let sql = "WITH last_reviews AS ( \
                       SELECT id FROM reviews ORDER BY id DESC LIMIT 1000 \
                   ) \
                   SELECT tu.provider, tu.model, tu.tool_name, count(*), avg(tu.output_length) \
                   FROM tool_usages tu \
                   JOIN last_reviews r ON tu.review_id = r.id \
                   GROUP BY tu.provider, tu.model, tu.tool_name";
        let mut rows = self.conn.query(sql, ()).await?;
        let mut stats = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let provider: Option<String> = row.get(0).ok();
            let model: Option<String> = row.get(1).ok();
            let tool_name: Option<String> = row.get(2).ok();
            let count: i64 = row.get(3)?;
            let avg_len: f64 = row.get(4).unwrap_or(0.0);
            stats.push(json!({
                "provider": provider.unwrap_or_default(),
                "model": model.unwrap_or_default(),
                "tool": tool_name.unwrap_or_default(),
                "count": count,
                "avg_output_length": avg_len
            }));
        }
        Ok(json!(stats))
    }

    pub async fn begin_transaction(&self) -> Result<()> {
        self.conn.execute("BEGIN IMMEDIATE", ()).await?;
        Ok(())
    }

    pub async fn commit_transaction(&self) -> Result<()> {
        self.conn.execute("COMMIT", ()).await?;
        Ok(())
    }

    async fn migrate_cross_review_job_generations(&self) -> Result<()> {
        let mut indexes = self
            .conn
            .query("PRAGMA index_list(cross_review_jobs)", ())
            .await?;
        let mut rebuild = false;
        while let Some(index) = indexes.next().await? {
            if index.get::<i64>(2).unwrap_or(0) == 0 {
                continue;
            }
            let name: String = index.get(1)?;
            let escaped = name.replace('"', "\"\"");
            let mut columns = self
                .conn
                .query(&format!("PRAGMA index_info(\"{escaped}\")"), ())
                .await?;
            let mut names = Vec::new();
            while let Some(column) = columns.next().await? {
                names.push(column.get::<String>(2)?);
            }
            if names == ["patchset_id", "source_name"] {
                rebuild = true;
                break;
            }
        }
        drop(indexes);

        if rebuild {
            self.rebuild_cross_review_jobs().await?;
            self.conn
                .execute(
                    "UPDATE cross_review_jobs
                     SET generation = COALESCE(
                         (SELECT target_review_count FROM patchsets
                          WHERE id = cross_review_jobs.patchset_id), generation, 1)",
                    (),
                )
                .await?;
        }
        self.conn
            .execute(
                "UPDATE cross_review_jobs
                 SET local_model = COALESCE(NULLIF(local_model, ''),
                                            (SELECT model_name FROM patchsets
                                             WHERE id = cross_review_jobs.patchset_id), ''),
                     local_provider = COALESCE(NULLIF(local_provider, ''),
                                               (SELECT provider FROM patchsets
                                                WHERE id = cross_review_jobs.patchset_id), '')",
                (),
            )
            .await?;
        self.conn
            .execute(
                "UPDATE patchsets SET cross_review_generation = COALESCE(
                     (SELECT MAX(generation) FROM cross_review_jobs
                      WHERE patchset_id = patchsets.id), cross_review_generation)
                 WHERE EXISTS (SELECT 1 FROM cross_review_jobs
                               WHERE patchset_id = patchsets.id)",
                (),
            )
            .await?;
        Ok(())
    }

    async fn rebuild_cross_review_jobs(&self) -> Result<()> {
        self.conn
            .query("PRAGMA foreign_keys=OFF", ())
            .await?
            .next()
            .await?;
        if let Err(error) = self.conn.execute("BEGIN IMMEDIATE", ()).await {
            let _ = self
                .conn
                .query("PRAGMA foreign_keys=ON", ())
                .await?
                .next()
                .await;
            return Err(error.into());
        }
        let result = self
            .conn
            .execute_batch(
                "DROP TABLE IF EXISTS cross_review_jobs_generation_migration;
                 CREATE TABLE cross_review_jobs_generation_migration (
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
                 INSERT INTO cross_review_jobs_generation_migration
                    (id, patchset_id, source_name, source_url, local_model,
                     local_provider, lookup_message_id, fallback_message_id,
                     generation, status, first_attempt_at, next_attempt_at,
                     deadline_at, lease_until, lease_token, attempts, last_error,
                     completed_at, remote_model, remote_provider, payload_hash,
                     merge_tokens_in, merge_tokens_out, merge_tokens_cached,
                     merge_budget_flags)
                 SELECT id, patchset_id, source_name, source_url, local_model,
                        local_provider, lookup_message_id, fallback_message_id,
                        generation, status, first_attempt_at, next_attempt_at,
                        deadline_at, lease_until, lease_token, attempts, last_error,
                        completed_at, remote_model, remote_provider, payload_hash,
                        merge_tokens_in, merge_tokens_out, merge_tokens_cached,
                        merge_budget_flags
                 FROM cross_review_jobs;
                 DROP TABLE cross_review_jobs;
                 ALTER TABLE cross_review_jobs_generation_migration
                    RENAME TO cross_review_jobs;
                 CREATE INDEX idx_cross_review_jobs_due
                    ON cross_review_jobs(status, next_attempt_at, lease_until);",
            )
            .await;
        let result = match result {
            Ok(_) => match self.conn.execute("COMMIT", ()).await {
                Ok(_) => Ok(()),
                Err(error) => {
                    let _ = self.conn.execute("ROLLBACK", ()).await;
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.conn.execute("ROLLBACK", ()).await;
                Err(error)
            }
        };
        let _ = self
            .conn
            .query("PRAGMA foreign_keys=ON", ())
            .await?
            .next()
            .await;
        result?;
        Ok(())
    }

    async fn try_create_index(&self, index_name: &str, table: &str, column: &str) -> Result<()> {
        let sql = format!(
            "CREATE INDEX IF NOT EXISTS {} ON {}({})",
            index_name, table, column
        );
        if let Err(e) = self.conn.execute(&sql, ()).await {
            info!("Migration: Error creating index {}: {}", index_name, e);
        } else {
            info!("Migration: Ensured index {} exists", index_name);
        }
        Ok(())
    }

    async fn try_add_column(&self, table: &str, column: &str, type_def: &str) -> Result<()> {
        let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, type_def);
        if let Err(_e) = self.conn.execute(&sql, ()).await {
            // Ignore error if column likely exists (duplicate column name)
            // info!("Migration: Column {} likely exists or error adding: {}", column, e);
        } else {
            info!("Migration: Added column {} to {}", column, table);
        }
        Ok(())
    }

    // People & Recipients
    pub async fn ensure_person(&self, name: Option<&str>, email: &str) -> Result<i64> {
        let email = email.trim();
        // Try to insert
        self.conn
            .execute(
                "INSERT OR IGNORE INTO people (name, email) VALUES (?, ?)",
                libsql::params![name, email],
            )
            .await?;

        // If a name is provided and the existing record has none, update it.
        // For now, keep it simple. Just get ID.
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM people WHERE email = ?",
                libsql::params![email],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to ensure person: {}", email))
        }
    }

    pub async fn add_message_recipient(
        &self,
        message_id: i64,
        person_id: i64,
        recipient_type: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_recipients (message_id, person_id, recipient_type) VALUES (?, ?, ?)",
                libsql::params![message_id, person_id, recipient_type],
            )
            .await?;
        Ok(())
    }

    // Subsystems
    pub async fn ensure_subsystem(&self, name: &str, mailing_list_address: &str) -> Result<i64> {
        // Try to insert
        self.conn
            .execute(
                "INSERT OR IGNORE INTO subsystems (name, mailing_list_address) VALUES (?, ?)",
                libsql::params![name, mailing_list_address],
            )
            .await?;

        // Get ID
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM subsystems WHERE mailing_list_address = ?",
                libsql::params![mailing_list_address],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            // Fallback: Get ID by name (Collision on name with different address)
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM subsystems WHERE name = ?",
                    libsql::params![name],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                Ok(row.get(0)?)
            } else {
                Err(anyhow::anyhow!("Failed to ensure subsystem"))
            }
        }
    }

    pub async fn add_subsystem_to_message(
        &self,
        message_id_db: i64,
        subsystem_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_subsystems (message_id, subsystem_id) VALUES (?, ?)",
                libsql::params![message_id_db, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_thread(&self, thread_id: i64, subsystem_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO threads_subsystems (thread_id, subsystem_id) VALUES (?, ?)",
                libsql::params![thread_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_patch(&self, patch_id: i64, subsystem_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patches_subsystems (patch_id, subsystem_id) VALUES (?, ?)",
                libsql::params![patch_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_patchset(
        &self,
        patchset_id: i64,
        subsystem_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id) VALUES (?, ?)",
                libsql::params![patchset_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_message_id_by_msg_id(&self, msg_id: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn ensure_mailing_list(&self, name: &str, group: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO mailing_lists (name, nntp_group, last_article_num) VALUES (?, ?, 0)
                 ON CONFLICT(nntp_group) DO UPDATE SET name = excluded.name",
                libsql::params![name, group],
            )
            .await?;
        Ok(())
    }

    pub async fn get_last_article_num(&self, group: &str) -> Result<u64> {
        let mut rows = self
            .conn
            .query(
                "SELECT last_article_num FROM mailing_lists WHERE nntp_group = ?",
                libsql::params![group],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let num: i64 = row.get(0)?;
            Ok(num as u64)
        } else {
            Ok(0)
        }
    }

    pub async fn update_last_article_num(&self, group: &str, num: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE mailing_lists SET last_article_num = ? WHERE nntp_group = ?",
                libsql::params![num as i64, group],
            )
            .await?;
        Ok(())
    }

    pub async fn create_thread(
        &self,
        root_message_id: &str,
        subject: &str,
        date: i64,
    ) -> Result<i64> {
        let mut rows = self.conn
            .query(
                "INSERT INTO threads (root_message_id, subject, last_updated) VALUES (?, ?, ?) RETURNING id",
                libsql::params![root_message_id, subject, date],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get thread ID"))
        }
    }

    pub async fn get_thread_id_for_message(&self, message_id: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT thread_id FROM messages WHERE message_id = ?",
                libsql::params![message_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn ensure_thread_for_message(&self, message_id: &str, date: i64) -> Result<i64> {
        // 1. Check if message exists
        if let Some(tid) = self.get_thread_id_for_message(message_id).await? {
            return Ok(tid);
        }

        // 2. Not found, create new thread and placeholder message
        let thread_id = self
            .create_thread(message_id, "(placeholder)", date)
            .await?;

        self.create_message(
            message_id,
            thread_id,
            None,
            "unknown",
            "(placeholder)",
            date,
            "",
            "",
            "",
            None,
            None,
        )
        .await?;

        Ok(thread_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_message(
        &self,
        message_id: &str,
        thread_id: i64,
        in_reply_to: Option<&str>,
        author: &str,
        subject: &str,
        date: i64,
        body: &str,
        to: &str,
        cc: &str,
        git_blob_hash: Option<&str>,
        mailing_list: Option<&str>,
    ) -> Result<()> {
        self.create_message_with_references(
            message_id,
            thread_id,
            in_reply_to,
            author,
            subject,
            date,
            body,
            to,
            cc,
            git_blob_hash,
            mailing_list,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_message_with_references(
        &self,
        message_id: &str,
        thread_id: i64,
        in_reply_to: Option<&str>,
        author: &str,
        subject: &str,
        date: i64,
        body: &str,
        to: &str,
        cc: &str,
        git_blob_hash: Option<&str>,
        mailing_list: Option<&str>,
        references_hdr: Option<&str>,
    ) -> Result<()> {
        // Check for thread merge (Thread split resolution)
        if let Ok(Some(old_thread_id)) = self.get_thread_id_for_message(message_id).await
            && old_thread_id != thread_id
        {
            info!("Merging thread {} into {}", old_thread_id, thread_id);
            // 1. Move messages
            self.conn
                .execute(
                    "UPDATE messages SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;

            // 2. Move patchsets
            self.conn
                .execute(
                    "UPDATE patchsets SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;

            // 3. Merge subsystems
            self.conn
                .execute(
                    "UPDATE OR IGNORE threads_subsystems SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;
            // Delete any remaining (conflicting) subsystem mappings for the old thread
            self.conn
                .execute(
                    "DELETE FROM threads_subsystems WHERE thread_id = ?",
                    libsql::params![old_thread_id],
                )
                .await?;

            // 5. Delete old thread
            self.conn
                .execute(
                    "DELETE FROM threads WHERE id = ?",
                    libsql::params![old_thread_id],
                )
                .await?;
        }

        // Use INSERT OR REPLACE to handle updating placeholders.
        // We want to preserve thread_id if it was set by placeholder (which is correct).
        // Actually, if we are "creating" the real message now, we should overwrite the placeholder fields.
        // Ensure the same thread_id is kept if it exists.
        // The caller (main.rs) resolves thread_id before calling create_message.
        // If we found a placeholder, we use its thread_id.
        // So here we just upsert.

        // Blindly replacing might change the thread_id if a different one is passed.
        // But main.rs logic should ensure consistency.
        // Use INSERT OR REPLACE.
        self.conn.execute(
            "INSERT INTO messages (message_id, thread_id, in_reply_to, author, subject, date, body, to_recipients, cc_recipients, git_blob_hash, mailing_list, references_hdr) 
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(message_id) DO UPDATE SET
                thread_id=excluded.thread_id,
                in_reply_to=excluded.in_reply_to,
                author=excluded.author,
                subject=excluded.subject,
                date=excluded.date,
                body=excluded.body,
                to_recipients=excluded.to_recipients,
                cc_recipients=excluded.cc_recipients,
                git_blob_hash=excluded.git_blob_hash,
                mailing_list=excluded.mailing_list,
                references_hdr=excluded.references_hdr",
            libsql::params![message_id, thread_id, in_reply_to, author, subject, date, body, to, cc, git_blob_hash, mailing_list, references_hdr],
        ).await?;
        Ok(())
    }

    pub async fn create_baseline(
        &self,
        repo_url: Option<&str>,
        branch: Option<&str>,
        commit: Option<&str>,
    ) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM baselines WHERE repo_url IS ? AND branch IS ? AND last_known_commit IS ?",
                libsql::params![repo_url, branch, commit],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            return Ok(row.get(0)?);
        }

        let mut rows = self.conn
            .query(
                "INSERT INTO baselines (repo_url, branch, last_known_commit) VALUES (?, ?, ?) RETURNING id",
                libsql::params![repo_url, branch, commit],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get baseline ID"))
        }
    }

    pub async fn get_baseline_commit(&self, id: i64) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT last_known_commit FROM baselines WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0).ok())
        } else {
            Ok(None)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_patchset(
        &self,
        thread_id: i64,
        cover_letter_message_id: Option<&str>,
        message_id: &str,
        subject: &str,
        author: &str,
        date: i64,
        total_parts: u32,
        parser_version: i32,
        to: &str,
        cc: &str,
        version: Option<u32>,
        part_index: u32,
        baseline_id: Option<i64>,
        strict_author: bool,
        skip_filters: Option<&Vec<String>>,
        only_filters: Option<&Vec<String>>,
    ) -> Result<Option<i64>> {
        let skip_filters_json = skip_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        let only_filters_json = only_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        // 1. Try to find by cover_letter_message_id first (handles placeholders from API/Fetcher)
        let mut clid_candidates = Vec::new();
        if let Some(clid) = cover_letter_message_id {
            clid_candidates.push(clid.to_string());
        }
        // Fallback for single-patch git imports where placeholder is sha@sashiko.local
        // but the actual cover letter becomes the sha itself.
        clid_candidates.push(format!("{}@sashiko.local", message_id));
        // A series sent without a cover letter has 1/N as its thread root, so a
        // thread-fetch placeholder was created under 1/N's own message-id. Such a
        // patch has no In-Reply-To to derive a cover letter from, so match on self.
        // Singletons already get this via the total == 1 case in the caller.
        if part_index == 1 && total_parts > 1 {
            clid_candidates.push(message_id.to_string());
        }

        for clid in clid_candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, date, author, subject, subject_index, total_parts, status FROM patchsets WHERE cover_letter_message_id = ?",
                    libsql::params![clid.clone()],
                )
                .await?;
            while let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                let existing_subject: String = row.get(3)?;
                let existing_status: String = row.get(6).unwrap_or_else(|_| "Unknown".to_string());

                let is_placeholder =
                    existing_subject == "(placeholder)" || existing_status == "Fetching";

                let existing_version = crate::patch::parse_subject_version(&existing_subject);
                let v_new = version.unwrap_or(1);
                let v_old = existing_version.unwrap_or(1);
                let versions_compatible = v_new == v_old;

                let index_collision = if part_index == 0 {
                    false
                } else {
                    let mut p_rows = self
                        .conn
                        .query(
                            "SELECT 1 FROM patches WHERE patchset_id = ? AND part_index = ? AND message_id != ?",
                            libsql::params![id, part_index, message_id],
                        )
                        .await?;
                    p_rows.next().await.ok().flatten().is_some()
                };

                if index_collision || (!is_placeholder && !versions_compatible) {
                    continue;
                }

                // Found it! Use this ID. We'll update its fields below.
                let subject_index: u32 = row.get(4).unwrap_or(9999);
                let existing_total: u32 = row.get(5).unwrap_or(1);

                // Prevent downgrading a series to a singleton if we already have multiple parts.
                // This handles cases where a singleton root (1/1) overwrites a series (N/N) inferred from replies.
                let final_total = if total_parts == 1 && existing_total > 1 {
                    existing_total
                } else {
                    total_parts
                };

                // We proceed to update this record with the full metadata
                self.conn.execute(
                    "UPDATE patchsets SET thread_id = ?, author = ?, total_parts = ?, parser_version = ?, to_recipients = ?, cc_recipients = ? WHERE id = ?",
                    libsql::params![thread_id, author, final_total, parser_version, to, cc, id],
                ).await?;

                if let Some(real_clid) = cover_letter_message_id {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET cover_letter_message_id = ? WHERE id = ?",
                            libsql::params![real_clid, id],
                        )
                        .await?;
                }

                if let Some(bid) = baseline_id {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET baseline_id = ? WHERE id = ?",
                            libsql::params![bid, id],
                        )
                        .await?;
                }

                // Update subject if this is a better index (e.g. going from placeholder to real subject)
                if part_index < subject_index {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET subject = ?, subject_index = ? WHERE id = ?",
                            libsql::params![subject, part_index, id],
                        )
                        .await?;
                }

                self.conn.execute(
                    "UPDATE patchsets SET status = 'Incomplete' WHERE id = ? AND status = 'Fetching'",
                    libsql::params![id],
                ).await?;

                self.conn.execute(
                    "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
                    libsql::params![id],
                ).await?;

                return Ok(Some(id));
            }
        }

        // 2. Normal matching logic: Find candidate patchsets in this thread OR matching author/time
        // We expand the search window to finding ANY patchset by this author in the last 24h
        let window_start = date - 86400;
        let window_end = date + 86400;
        let mut rows = self
            .conn
            .query(
                "SELECT id, date, author, subject, subject_index, total_parts, received_parts, cover_letter_message_id, thread_id FROM patchsets 
                 WHERE thread_id = ? OR (author = ? AND date BETWEEN ? AND ?)",
                libsql::params![thread_id, author, window_start, window_end],
            )
            .await?;

        let mut matches = Vec::new();

        while let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let existing_date: i64 = row.get(1)?;
            // Placeholder rows from create_fetching_patchset leave author NULL, so a
            // strict get() here would abort the whole scan and discard the patch.
            let existing_author: String = row.get(2).unwrap_or_default();
            let existing_subject: String = row.get(3)?;
            let existing_subject_index: u32 = row.get(4).unwrap_or(9999);
            let existing_total: u32 = row.get(5).unwrap_or(1);
            let existing_received: u32 = row.get(6).unwrap_or(0);
            let existing_cover_id: Option<String> = row.get(7).ok();
            let existing_thread_id: Option<i64> = row.get(8).ok();

            // Check if this message is already part of this patchset (Duplicate processing)
            // 1. Check if it is the cover letter.
            let is_cover_duplicate = existing_cover_id.as_deref() == Some(message_id);

            // 2. Check if it is an existing patch.
            let is_patch_duplicate = if !is_cover_duplicate {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT 1 FROM patches WHERE patchset_id = ? AND message_id = ?",
                        libsql::params![id, message_id],
                    )
                    .await?;
                p_rows.next().await.ok().flatten().is_some()
            } else {
                false
            };

            let is_duplicate = is_cover_duplicate || is_patch_duplicate;

            // If the patchset is already full, do not merge more patches into it,
            // UNLESS it is a duplicate of a message already in the set.
            // This prevents merging unrelated patchsets that happen to look similar (same author/size).
            if existing_received >= existing_total && !is_duplicate && part_index != 0 {
                continue;
            }

            // Parse version from existing subject
            let existing_version = crate::patch::parse_subject_version(&existing_subject);

            // Clean subjects for comparison
            let clean_new = crate::patch::clean_subject(subject);
            let clean_old = crate::patch::clean_subject(&existing_subject);

            // Check for index collision
            // If the patchset already contains a patch with this index (and different message_id), it's a collision.
            // This prevents merging [PATCH 1/2] Series A and [PATCH 1/2] Series B.
            let index_collision = if part_index == 0 {
                existing_cover_id.is_some()
                    && existing_cover_id.as_deref() != Some(message_id)
                    && existing_subject_index == 0
            } else {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT 1 FROM patches WHERE patchset_id = ? AND part_index = ? AND message_id != ?",
                        libsql::params![id, part_index, message_id],
                    )
                    .await?;
                p_rows.next().await.ok().flatten().is_some()
            };

            let mut existing_msgid_prefix = None;
            if let Some(ref cover_id) = existing_cover_id {
                existing_msgid_prefix =
                    Some(cover_id.split('-').next().unwrap_or(cover_id).to_string());
            } else {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT message_id FROM patches WHERE patchset_id = ? LIMIT 1",
                        libsql::params![id],
                    )
                    .await?;
                if let Ok(Some(p_row)) = p_rows.next().await {
                    let pid: String = p_row.get(0)?;
                    existing_msgid_prefix = Some(pid.split('-').next().unwrap_or(&pid).to_string());
                }
            }

            let new_msgid_prefix = message_id.split('-').next().unwrap_or(message_id);
            let msgid_prefix_match = existing_msgid_prefix.as_deref() == Some(new_msgid_prefix)
                && new_msgid_prefix.len() > 10;

            // Matching logic:
            // 1. Author matches OR it's a multi-part series with matching total_parts (trusting thread context)
            //    BUT strict_author enforces strict author matching (for Email/NNTP).
            // 2. Time must be close (within 24 hours / 86400s)
            // 3. Total parts must match
            // 4. Versions must match (treating None as v1)
            // 5. For singletons (total=1), Subject must match (fuzzy) to avoid merging unrelated patches

            let v_new = version.unwrap_or(1);
            let v_old = existing_version.unwrap_or(1);
            let versions_compatible = v_new == v_old;

            let is_singleton = total_parts == 1;
            // For singletons, we require the subject to be somewhat similar to avoid merging unrelated patches.
            let subject_match = if is_singleton {
                if subject == existing_subject {
                    true
                } else {
                    // Allow merging 0/1 (cover) and 1/1 (patch) even if subjects differ
                    if (part_index == 0 && existing_subject_index == 1)
                        || (part_index == 1 && existing_subject_index == 0)
                    {
                        true
                    } else {
                        clean_new == clean_old
                    }
                }
            } else {
                // For series:
                // If we are replacing/matching the SAME index as the one that defined the patchset subject,
                // we require the subjects to match.
                // e.g. [PATCH 1/2] Series A vs [PATCH 1/2] Series B -> Mismatch.
                if part_index == existing_subject_index {
                    clean_new == clean_old
                } else {
                    true // For other parts (1/N vs 2/N), subjects differ naturally.
                }
            };

            // Relaxed author check logic
            let author_match = crate::patch::authors_match(&existing_author, author);
            let series_match = (total_parts > 1 && total_parts == existing_total)
                || existing_total == 1
                || total_parts == 1;

            let author_or_series_match = if strict_author {
                author_match
            } else {
                author_match || series_match
            };

            // Prefix matching (to separate different series from same author)
            let same_thread = existing_thread_id == Some(thread_id);
            let prefix_match = if same_thread {
                true // Trust thread
            } else {
                let new_prefixes = crate::patch::get_subject_prefixes(subject);
                let old_prefixes = crate::patch::get_subject_prefixes(&existing_subject);
                new_prefixes == old_prefixes
            };

            // Thread Enforcement: To prevent cross-thread "stealing" of patches for resends of the same series,
            // we strictly require multi-part series patches to belong to the same thread,
            // unless they share a git send-email Message-ID prefix indicating they were sent together unthreaded.
            let thread_compatible = same_thread || is_singleton || msgid_prefix_match;

            if author_or_series_match
                && (!strict_author || (date - existing_date).abs() < 86400)
                && (versions_compatible || same_thread)
                && (total_parts == existing_total || existing_total == 1 || total_parts == 1)
                && subject_match
                && prefix_match
                && thread_compatible
                && !index_collision
            {
                matches.push((id, existing_subject_index));
            }
        }

        if !matches.is_empty() {
            // Sort matches to pick the "best" one to keep (e.g. oldest ID or one with lowest subject index)
            // Let's keep the one with the lowest ID (created first)
            matches.sort_by_key(|k| k.0);

            let target_id = matches[0].0;
            let mut current_subject_index = matches[0].1;

            // If we have multiple matches, merge others into target_id
            for (merge_from_id, merge_subject_index) in matches.iter().skip(1) {
                let merge_from_id = *merge_from_id;
                info!("Merging patchset {} into {}", merge_from_id, target_id);

                // Reassign patches
                self.conn
                    .execute(
                        "UPDATE OR IGNORE patches SET patchset_id = ? WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;

                // Reassign reviews
                self.conn
                    .execute(
                        "UPDATE reviews SET patchset_id = ? WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;

                // Merge subsystems
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id)
                         SELECT ?, subsystem_id FROM patchsets_subsystems WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;
                self.conn
                    .execute(
                        "DELETE FROM patchsets_subsystems WHERE patchset_id = ?",
                        libsql::params![merge_from_id],
                    )
                    .await?;

                // If the merged patchset had a better subject index, track it
                if *merge_subject_index < current_subject_index {
                    current_subject_index = *merge_subject_index;
                }

                // Delete the merged patchset
                self.conn
                    .execute(
                        "DELETE FROM patchsets WHERE id = ?",
                        libsql::params![merge_from_id],
                    )
                    .await?;
            }

            // Update the target patchset
            self.conn.execute(
                "UPDATE patchsets SET author = ?, total_parts = ?, parser_version = ?, to_recipients = ?, cc_recipients = ? WHERE id = ?",
                libsql::params![author, total_parts, parser_version, to, cc, target_id],
            ).await?;

            if skip_filters_json.is_some() || only_filters_json.is_some() {
                self.conn.execute(
                    "UPDATE patchsets SET skip_filters = COALESCE(?, skip_filters), only_filters = COALESCE(?, only_filters) WHERE id = ?",
                    libsql::params![skip_filters_json.clone(), only_filters_json.clone(), target_id],
                ).await?;
            }

            if let Some(bid) = baseline_id {
                self.conn
                    .execute(
                        "UPDATE patchsets SET baseline_id = ? WHERE id = ?",
                        libsql::params![bid, target_id],
                    )
                    .await?;
            }

            // Conditionally update subject
            // Note: We check against the best index found among all merged sets OR the new part_index
            if part_index < current_subject_index {
                self.conn
                    .execute(
                        "UPDATE patchsets SET subject = ?, subject_index = ? WHERE id = ?",
                        libsql::params![subject, part_index, target_id],
                    )
                    .await?;
            } else if matches.len() > 1 {
                // If we merged, we might need to update the subject index of the target to the best one we found.
                // But we don't have the subject string from the merged one easily available here.
                // However, the existing target subject is likely fine unless part_index is better.
                // Update subject_index to be correct if a better one was merged.
                // Actually, if matches[i].1 was better, we should have used its subject.
                // But that's complicated. Assuming the target (oldest) usually has the cover letter or we eventually find it.
                // Simplification: We only update if CURRENT patch is better.
                // If we merged a patchset that HAD the cover letter, we ideally want that subject.
                // But we lost it.
                // TODO: Optimize merge subject selection. For now, this is better than duplicates.
            }

            if let Some(clid) = cover_letter_message_id {
                self.conn
                    .execute(
                        "UPDATE patchsets SET cover_letter_message_id = ? WHERE id = ?",
                        libsql::params![clid, target_id],
                    )
                    .await?;
            }

            // Recalculate received parts for target (in case we merged)
            self.conn
            .execute(
                "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                libsql::params![target_id, target_id],
            )
            .await?;

            self.conn.execute(
                "UPDATE patchsets SET status = 'Incomplete' WHERE id = ? AND status = 'Fetching'",
                libsql::params![target_id],
            ).await?;

            self.conn.execute(
                "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
                libsql::params![target_id],
            ).await?;

            return Ok(Some(target_id));
        }

        // No match found, create new patchset
        let mut rows = self.conn
            .query(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, author, date, total_parts, received_parts, status, parser_version, to_recipients, cc_recipients, subject_index, baseline_id, skip_filters, only_filters) 
                 VALUES (?, ?, ?, ?, ?, ?, 0, 'Incomplete', ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![thread_id, cover_letter_message_id, subject, author, date, total_parts, parser_version, to, cc, part_index, baseline_id, skip_filters_json.clone(), only_filters_json.clone()],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            Ok(Some(id))
        } else {
            Err(anyhow::anyhow!(
                "Failed to retrieve patchset ID after insert"
            ))
        }
    }

    pub async fn create_patch(
        &self,
        patchset_id: i64,
        message_id: &str,
        part_index: u32,
        diff: &str,
    ) -> Result<i64> {
        // Check if index collision occurs for this patchset
        let collision_exists: bool = {
            let mut rows = self
                .conn
                .query(
                    "SELECT 1 FROM patches WHERE patchset_id = ? AND part_index = ? AND message_id != ?",
                    libsql::params![patchset_id, part_index, message_id],
                )
                .await?;
            rows.next().await.ok().flatten().is_some()
        };

        if collision_exists {
            return Err(anyhow::anyhow!(
                "Index collision: index {} already exists in patchset {}",
                part_index,
                patchset_id
            ));
        }

        // Check if patch exists and get old patchset_id to fix counts if we steal it
        let old_patchset_id: Option<i64> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT patchset_id FROM patches WHERE message_id = ?",
                    libsql::params![message_id],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                Some(row.get(0)?)
            } else {
                None
            }
        };

        // Insert or Update (Move patch to new patchset if duplicate)
        self.conn.execute(
            "INSERT INTO patches (patchset_id, message_id, part_index, diff) VALUES (?, ?, ?, ?)
             ON CONFLICT(message_id) DO UPDATE SET
                patchset_id=excluded.patchset_id,
                part_index=excluded.part_index,
                diff=excluded.diff",
            libsql::params![patchset_id, message_id, part_index, diff]
        ).await?;

        // Update received_parts for the NEW patchset
        self.conn
            .execute(
                "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                libsql::params![patchset_id, patchset_id],
            )
            .await?;

        // Update received_parts for the OLD patchset (if we moved it)
        if let Some(old_id) = old_patchset_id
            && old_id != patchset_id
        {
            self.conn
                        .execute(
                            "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                            libsql::params![old_id, old_id],
                        )
                        .await?;
        }

        // Check if complete and update status
        // We transition from 'Incomplete' OR 'Fetching' to 'Pending' (ready for review)
        self.conn.execute(
            "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
            libsql::params![patchset_id],
        ).await?;

        // Get the patch ID
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patches WHERE message_id = ?",
                libsql::params![message_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get patch ID"))
        }
    }

    fn build_search(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
        target: &str,
    ) -> (String, Vec<String>) {
        let mut conditions = Vec::new();
        let mut params = Vec::new();

        // Always exclude placeholders
        conditions.push("subject != '(placeholder)'".to_string());

        if let Some(list) = mailing_list
            && !list.is_empty()
        {
            if target == "patchset" {
                // Filter patchsets where any patch OR the cover letter is in the mailing list
                // We use p.id to avoid ambiguity with joined tables (e.g. subsystems.id)
                conditions.push(
                    "p.id IN (
                        SELECT patchset_id FROM patches p2 
                        JOIN messages m ON p2.message_id = m.message_id 
                        JOIN messages_mailing_lists mml ON m.id = mml.message_id 
                        JOIN mailing_lists ml ON mml.mailing_list_id = ml.id 
                        WHERE ml.nntp_group = ?
                        UNION
                        SELECT ps.id FROM patchsets ps 
                        JOIN messages m ON ps.cover_letter_message_id = m.message_id 
                        JOIN messages_mailing_lists mml ON m.id = mml.message_id 
                        JOIN mailing_lists ml ON mml.mailing_list_id = ml.id 
                        WHERE ml.nntp_group = ?
                    )"
                    .to_string(),
                );
                params.push(list.clone());
                params.push(list);
            } else {
                // Filter messages
                conditions.push("id IN (SELECT message_id FROM messages_mailing_lists mml JOIN mailing_lists ml ON mml.mailing_list_id = ml.id WHERE ml.nntp_group = ?)".to_string());
                params.push(list);
            }
        }

        if let Some(q) = query {
            let q = q.trim();
            if !q.is_empty() {
                if let Some(val) = q.strip_prefix("author:") {
                    conditions.push("author LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("subject:") {
                    conditions.push("subject LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("date:") {
                    conditions.push("datetime(date, 'unixepoch') LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("subsystem:") {
                    let sub_query = if target == "patchset" {
                        "p.id IN (SELECT patchset_id FROM patchsets_subsystems ps JOIN subsystems s ON ps.subsystem_id = s.id WHERE s.name LIKE ?)"
                    } else {
                        "id IN (SELECT message_id FROM messages_subsystems ms JOIN subsystems s ON ms.subsystem_id = s.id WHERE s.name LIKE ?)"
                    };
                    conditions.push(sub_query.to_string());
                    params.push(format!("%{}%", val.trim()));
                } else {
                    conditions.push("(subject LIKE ? OR author LIKE ?)".to_string());
                    params.push(format!("%{}%", q));
                    params.push(format!("%{}%", q));
                }
            }
        }

        if conditions.is_empty() {
            (String::new(), vec![])
        } else {
            (format!("WHERE {}", conditions.join(" AND ")), params)
        }
    }

    pub async fn set_patchset_embargo_until(&self, id: i64, embargo_until: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET embargo_until = ? WHERE id = ?",
                libsql::params![embargo_until, id],
            )
            .await?;
        Ok(())
    }

    pub async fn clear_patchset_embargo(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets
                 SET embargo_until = NULL, embargo_release_started_at = NULL
                 WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_patchsets(
        &self,
        limit: usize,
        offset: usize,
        query: Option<String>,
        mailing_list: Option<String>,
        bypass_embargo: bool,
    ) -> Result<Vec<PatchsetRow>> {
        let (where_clause, params) = self.build_search(query, mailing_list, "patchset");
        // We use p.* alias implicitely by using unqualified names in WHERE which is fine given no collisions.
        // But for clarity/safety we should alias in FROM.
        // build_search returns "WHERE author ...".

        let sql = format!(
            "SELECT p.id, p.subject, p.status, p.thread_id, p.author, p.date, p.cover_letter_message_id, p.total_parts, p.received_parts, GROUP_CONCAT(s.name, ','),
             COALESCE(f.low, 0), COALESCE(f.medium, 0), COALESCE(f.high, 0), COALESCE(f.critical, 0), p.baseline_id, p.failed_reason, p.target_review_count, p.skip_filters, p.only_filters,
             p.embargo_until, p.mr_url, p.mr_title, p.mr_number, p.slug,
             d.concerns_total, d.concerns_unique, d.findings_multi_stage, b.budget_flags_or,
             p.cross_review_status, p.cross_reviewed_at
             FROM (
                 SELECT id FROM patchsets p
                 {}
                 ORDER BY p.date DESC LIMIT ? OFFSET ?
             ) p_lim
             JOIN patchsets p ON p_lim.id = p.id
             LEFT JOIN patchsets_subsystems ps ON p.id = ps.patchset_id
             LEFT JOIN subsystems s ON ps.subsystem_id = s.id
             LEFT JOIN (
                SELECT r.patchset_id,
                    SUM(CASE WHEN f.severity = 1 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as low,
                    SUM(CASE WHEN f.severity = 2 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as medium,
                    SUM(CASE WHEN f.severity = 3 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as high,
                    SUM(CASE WHEN f.severity = 4 AND COALESCE(f.preexisting, 0) = 0 THEN 1 ELSE 0 END) as critical
                FROM reviews r
                JOIN findings f ON r.id = f.review_id
                WHERE r.status = 'Reviewed'
                GROUP BY r.patchset_id
             ) f ON p.id = f.patchset_id
             LEFT JOIN (
                SELECT r.patchset_id, SUM(r.concerns_total) AS concerns_total,
                    SUM(r.concerns_unique) AS concerns_unique,
                    SUM(r.findings_multi_stage) AS findings_multi_stage
                FROM reviews r
                WHERE r.status = 'Reviewed' AND r.concerns_total IS NOT NULL
                GROUP BY r.patchset_id
             ) d ON p.id = d.patchset_id
             LEFT JOIN (
                SELECT r.patchset_id,
                    MAX(r.budget_flags & 1) | MAX(r.budget_flags & 2)
                    | MAX(r.budget_flags & 4) | MAX(r.budget_flags & 8)
                    | MAX(r.budget_flags & 16) | MAX(r.budget_flags & 32)
                    | MAX(r.budget_flags & 64) | MAX(r.budget_flags & 128)
                    AS budget_flags_or
                FROM reviews r
                WHERE r.status = 'Reviewed' AND r.budget_flags > 0
                GROUP BY r.patchset_id
             ) b ON p.id = b.patchset_id
             GROUP BY p.id
             ORDER BY p.date DESC",
            where_clause
        );

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }
        args.push(libsql::Value::Integer(limit as i64));
        args.push(libsql::Value::Integer(offset as i64));

        let mut rows = self.conn.query(&sql, args).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;

        let mut patchsets = Vec::new();
        loop {
            match rows.next().await {
                Ok(Some(row)) => {
                    let subsystems_str: Option<String> = row.get(9).ok();
                    let subsystems = if let Some(s) = subsystems_str {
                        s.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    } else {
                        Vec::new()
                    };

                    let embargo_until: Option<i64> = row.get(19).ok();
                    let is_embargoed =
                        !bypass_embargo && embargo_until.map(|u| u > now).unwrap_or(false);

                    let (low, medium, high, critical) = if is_embargoed {
                        (0, 0, 0, 0)
                    } else {
                        (
                            row.get::<Option<i64>>(10).ok().flatten().unwrap_or(0),
                            row.get::<Option<i64>>(11).ok().flatten().unwrap_or(0),
                            row.get::<Option<i64>>(12).ok().flatten().unwrap_or(0),
                            row.get::<Option<i64>>(13).ok().flatten().unwrap_or(0),
                        )
                    };

                    let mut status: Option<String> = row.get(2).ok();
                    if is_embargoed && status.as_deref() == Some("Reviewed") {
                        status = Some("Embargoed".to_string());
                    }

                    patchsets.push(PatchsetRow {
                        id: row.get(0).unwrap_or_default(),
                        subject: row.get(1).ok(),
                        status,
                        thread_id: row.get(3).ok(),
                        author: row.get(4).ok(),
                        date: row.get(5).ok(),
                        message_id: row.get(6).ok(),
                        total_parts: row.get(7).ok(),
                        received_parts: row.get(8).ok(),
                        subsystems,
                        findings_low: Some(low),
                        findings_medium: Some(medium),
                        findings_high: Some(high),
                        findings_critical: Some(critical),
                        baseline_id: row.get(14).ok(),
                        failed_reason: row.get(15).ok(),
                        target_review_count: row.get(16).ok(),
                        skip_filters: row.get(17).ok(),
                        only_filters: row.get(18).ok(),
                        model_name: None,
                        prompts_git_hash: None,
                        baseline_logs: None,
                        provider: None,
                        embargo_until: row.get(19).ok(),
                        mr_url: row.get(20).ok(),
                        mr_title: row.get(21).ok(),
                        mr_number: row.get(22).ok(),
                        slug: row.get(23).ok(),
                        concerns_total: row.get(24).ok(),
                        concerns_unique: row.get(25).ok(),
                        findings_multi_stage: row.get(26).ok(),
                        budget_flags_or: row.get::<Option<i64>>(27).ok().flatten(),
                        cross_review_status: row.get(28).ok(),
                        cross_reviewed_at: row.get(29).ok(),
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("Error fetching row: {:?}", e);
                    break;
                }
            }
        }
        Ok(patchsets)
    }

    pub async fn get_messages(
        &self,
        limit: usize,
        offset: usize,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<Vec<MessageRow>> {
        let (where_clause, params) = self.build_search(query, mailing_list, "message");
        let sql = format!(
            "SELECT id, message_id, thread_id, in_reply_to, author, subject, date, body, to_recipients, cc_recipients, git_blob_hash, mailing_list, references_hdr FROM messages {} ORDER BY date DESC LIMIT ? OFFSET ?",
            where_clause
        );

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }
        args.push(libsql::Value::Integer(limit as i64));
        args.push(libsql::Value::Integer(offset as i64));

        let mut rows = self.conn.query(&sql, args).await?;
        let mut messages = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            messages.push(MessageRow {
                id: row.get(0)?,
                message_id: row.get(1)?,
                thread_id: row.get(2).ok(),
                in_reply_to: row.get(3).ok(),
                author: row.get(4).ok(),
                subject: row.get(5).ok(),
                date: row.get(6).ok(),
                body: row.get(7).ok(),
                to: row.get(8).ok(),
                cc: row.get(9).ok(),
                git_blob_hash: row.get(10).ok(),
                mailing_list: row.get(11).ok(),
                diff: None,
                references_hdr: row.get(12).ok(),
                thread: None,
            });
        }
        Ok(messages)
    }

    pub async fn count_patchsets(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, mailing_list, "patchset");
        // We must alias patchsets as p because build_search uses p.id for filters
        let sql = format!("SELECT COUNT(*) FROM patchsets p {}", where_clause);

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }

        let mut rows = self.conn.query(&sql, args).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_pending_patches(&self) -> Result<usize> {
        let mut rows = self.conn.query(
            "SELECT COUNT(p.id) FROM patches p JOIN patchsets ps ON p.patchset_id = ps.id 
             WHERE ps.status IN ('Pending', 'In Review') AND p.status IS NULL
             AND p.id NOT IN (SELECT patch_id FROM reviews WHERE status IN ('In Review', 'Applying') AND patch_id IS NOT NULL)",
            ()
        ).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_reviewing_patches(&self) -> Result<usize> {
        let mut rows = self.conn.query(
            "SELECT COUNT(DISTINCT patch_id) FROM reviews WHERE status IN ('In Review', 'Applying') AND patch_id IS NOT NULL",
            ()
        ).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_messages(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, mailing_list, "message");
        let sql = format!("SELECT COUNT(*) FROM messages {}", where_clause);

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }

        let mut rows = self.conn.query(&sql, args).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn get_patchset_details(
        &self,
        id: i64,
        page: Option<u32>,
        limit: Option<u32>,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients,
                    p.author, p.date, p.cover_letter_message_id, p.thread_id,
                    p.total_parts, p.received_parts, p.failed_reason,
                    p.model_name, p.prompts_git_hash, p.baseline_logs, p.baseline_id, p.provider,
                    p.embargo_until, p.mr_url, p.slug
                FROM patchsets p
                WHERE p.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let pid: i64 = row.get(0)?;
            let subject: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let to: Option<String> = row.get(3).ok();
            let cc: Option<String> = row.get(4).ok();
            let author: Option<String> = row.get(5).ok();
            let date: Option<i64> = row.get(6).ok();
            let mid: Option<String> = row.get(7).ok();
            let thread_id: Option<i64> = row.get(8).ok();
            let total_parts: Option<u32> = row.get(9).ok();
            let received_parts: Option<u32> = row.get(10).ok();
            let failed_reason: Option<String> = row.get(11).ok();
            let model_name: Option<String> = row.get(12).ok();
            let prompts_git_hash: Option<String> = row.get(13).ok();
            let baseline_logs: Option<String> = row.get(14).ok();
            let baseline_id: Option<i64> = row.get(15).ok();
            let provider: Option<String> = row.get(16).ok();
            let embargo_until: Option<i64> = row.get(17).ok();
            let mr_url: Option<String> = row.get(18).ok();
            let slug: Option<String> = row.get(19).ok();
            // Fetch baseline details if needed
            let baseline = if let Some(bid) = baseline_id {
                let mut browse = self
                    .conn
                    .query(
                        "SELECT repo_url, branch, last_known_commit FROM baselines WHERE id = ?",
                        libsql::params![bid],
                    )
                    .await?;
                if let Ok(Some(brow)) = browse.next().await {
                    Some(serde_json::json!({
                       "repo_url": brow.get::<Option<String>>(0).ok(),
                       "branch": brow.get::<Option<String>>(1).ok(),
                       "commit": brow.get::<Option<String>>(2).ok(),
                    }))
                } else {
                    None
                }
            } else {
                None
            };

            // Calculate pagination
            let limit_val = limit.unwrap_or(50);
            let page_val = page.unwrap_or(1);
            let offset_val = limit_val * (page_val.saturating_sub(1));

            // Fetch subsystems
            let mut subsystems = Vec::new();
            let mut sub_rows = self
                .conn
                .query(
                    "SELECT s.name FROM subsystems s
                 JOIN patchsets_subsystems ps ON s.id = ps.subsystem_id
                 WHERE ps.patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            while let Ok(Some(row)) = sub_rows.next().await {
                subsystems.push(row.get::<String>(0)?);
            }

            let mut total_patches = 0;
            let mut count_rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            if let Ok(Some(row)) = count_rows.next().await {
                total_patches = row.get::<i64>(0)?;
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;

            let is_embargoed = !bypass_embargo && embargo_until.map(|u| u > now).unwrap_or(false);

            // Fetch patches with subject and msg_db_id
            let mut patches = Vec::new();
            let mut patch_ids = Vec::new();
            let mut patch_rows = self
                .conn
                .query(
                    "SELECT p.id, p.message_id, p.part_index, m.id, m.subject, p.status, p.apply_error, 
                            eo.status as email_status, eo.to_addresses, eo.cc_addresses
                 FROM patches p
                 LEFT JOIN messages m ON p.message_id = m.message_id
                 LEFT JOIN email_outbox eo ON eo.patch_id = p.id
                 WHERE p.patchset_id = ? 
                 ORDER BY p.part_index ASC
                 LIMIT ? OFFSET ?",
                    libsql::params![pid, limit_val, offset_val],
                )
                .await?;
            #[allow(clippy::similar_names)]
            while let Ok(Some(p)) = patch_rows.next().await {
                let p_id: i64 = p.get(0)?;
                patch_ids.push(p_id);
                let mut p_status = p.get::<Option<String>>(5).ok().flatten();
                if is_embargoed && p_status.as_deref() == Some("Reviewed") {
                    p_status = Some("Embargoed".to_string());
                }
                patches.push(serde_json::json!({
                    "id": p_id,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                    "msg_db_id": p.get::<Option<i64>>(3).ok(),
                    "subject": p.get::<Option<String>>(4).ok(),
                    "status": p_status,
                    "apply_error": p.get::<Option<String>>(6).ok(),
                    "email_status": p.get::<Option<String>>(7).ok(),
                    "email_to": p.get::<Option<String>>(8).ok(),
                    "email_cc": p.get::<Option<String>>(9).ok(),
                }));
            }

            // Fetch reviews
            let mut reviews = Vec::new();
            let mut in_clause = patch_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            if in_clause.is_empty() {
                in_clause = "-1".to_string(); // Fallback so SQL doesn't error
            }
            let query_str = format!(
                "SELECT r.summary, r.created_at, ai.input_context, ai.output_raw, 
                        r.result_description, r.status, r.inline_review, r.logs, ai.tokens_in, ai.tokens_out, r.patch_id, r.id, ai.tokens_cached, r.budget_flags
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 WHERE r.patchset_id = ? AND (r.patch_id IS NULL OR r.patch_id IN ({}))
                 ORDER BY r.created_at ASC", in_clause);

            let mut params = vec![libsql::Value::Integer(pid)];
            for &pid_val in &patch_ids {
                params.push(libsql::Value::Integer(pid_val));
            }

            let mut rev_rows = self.conn.query(&query_str, params).await?;

            while let Ok(Some(r)) = rev_rows.next().await {
                reviews.push(serde_json::json!({
                    "summary": r.get::<Option<String>>(0).ok(),
                    "created_at": r.get::<Option<i64>>(1).ok(),
                    "output": r.get::<Option<String>>(3).ok(),
                    "result": r.get::<Option<String>>(4).ok(),
                    "status": r.get::<Option<String>>(5).ok(),
                    "inline_review": r.get::<Option<String>>(6).ok(),
                    "logs": r.get::<Option<String>>(7).ok(),
                    "tokens_in": r.get::<Option<u32>>(8).ok(),
                    "tokens_out": r.get::<Option<u32>>(9).ok(),
                    "patch_id": r.get::<Option<i64>>(10).ok(),
                    "id": r.get::<i64>(11).ok(),
                    "tokens_cached": r.get::<Option<u32>>(12).ok(),
                    "budget_flags": r.get::<Option<i64>>(13).ok().flatten().unwrap_or(0),
                    "model": model_name.clone(),
                    "provider": provider.clone(),
                    "prompts_hash": prompts_git_hash.clone(),
                    "baseline": baseline.clone()
                }));
            }

            // Fetch thread messages
            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
                    }));
                }
            }

            let mut final_status = status;
            if is_embargoed && final_status.as_deref() == Some("Reviewed") {
                final_status = Some("Embargoed".to_string());
            }

            let reviews = if is_embargoed { Vec::new() } else { reviews };

            Ok(Some(serde_json::json!({
                "id": pid,
                "message_id": mid,
                "subject": subject,
                "author": author,
                "date": date,
                "status": final_status,
                "failed_reason": failed_reason,
                "to": to,
                "cc": cc,
                "total_parts": total_parts,
                "total_patches_in_db": total_patches,
                "page": page_val,
                "limit": limit_val,
                "received_parts": received_parts,
                "reviews": reviews,
                "patches": patches,
                "thread": messages,
                "subsystems": subsystems,
                "model_name": model_name,
                "prompts_git_hash": prompts_git_hash,
                "baseline_logs": baseline_logs,
                "baseline": baseline,
                "provider": provider,
                "embargo_until": embargo_until,
                "mr_url": mr_url,
                "slug": slug
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_summary(
        &self,
        id: i64,
        page: Option<u32>,
        limit: Option<u32>,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients,
                    p.author, p.date, p.cover_letter_message_id, p.thread_id,
                    p.total_parts, p.received_parts, p.failed_reason,
                    p.model_name, p.prompts_git_hash, p.baseline_logs, p.baseline_id, p.provider,
                    p.embargo_until, p.mr_url, p.slug
                FROM patchsets p
                WHERE p.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let pid: i64 = row.get(0)?;
            let subject: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let to: Option<String> = row.get(3).ok();
            let cc: Option<String> = row.get(4).ok();
            let author: Option<String> = row.get(5).ok();
            let date: Option<i64> = row.get(6).ok();
            let mid: Option<String> = row.get(7).ok();
            let thread_id: Option<i64> = row.get(8).ok();
            let total_parts: Option<u32> = row.get(9).ok();
            let received_parts: Option<u32> = row.get(10).ok();
            let failed_reason: Option<String> = row.get(11).ok();
            let model_name: Option<String> = row.get(12).ok();
            let prompts_git_hash: Option<String> = row.get(13).ok();
            let baseline_logs: Option<String> = row.get(14).ok();
            let baseline_id: Option<i64> = row.get(15).ok();
            let provider: Option<String> = row.get(16).ok();
            let embargo_until: Option<i64> = row.get(17).ok();
            let mr_url: Option<String> = row.get(18).ok();
            let slug: Option<String> = row.get(19).ok();
            let baseline = if let Some(bid) = baseline_id {
                let mut browse = self
                    .conn
                    .query(
                        "SELECT repo_url, branch, last_known_commit FROM baselines WHERE id = ?",
                        libsql::params![bid],
                    )
                    .await?;
                if let Ok(Some(brow)) = browse.next().await {
                    Some(serde_json::json!({
                       "repo_url": brow.get::<Option<String>>(0).ok(),
                       "branch": brow.get::<Option<String>>(1).ok(),
                       "commit": brow.get::<Option<String>>(2).ok(),
                    }))
                } else {
                    None
                }
            } else {
                None
            };

            let limit_val = limit.unwrap_or(50);
            let page_val = page.unwrap_or(1);
            let offset_val = limit_val * (page_val.saturating_sub(1));

            let mut subsystems = Vec::new();
            let mut sub_rows = self
                .conn
                .query(
                    "SELECT s.name FROM subsystems s
                 JOIN patchsets_subsystems ps ON s.id = ps.subsystem_id
                 WHERE ps.patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            while let Ok(Some(row)) = sub_rows.next().await {
                subsystems.push(row.get::<String>(0)?);
            }

            let mut total_patches = 0;
            let mut count_rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            if let Ok(Some(row)) = count_rows.next().await {
                total_patches = row.get::<i64>(0)?;
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;

            let is_embargoed = !bypass_embargo && embargo_until.map(|u| u > now).unwrap_or(false);

            let mut patches = Vec::new();
            let mut patch_ids = Vec::new();
            let mut patch_rows = self
                .conn
                .query(
                    "SELECT p.id, p.message_id, p.part_index, m.id, m.subject, p.status, p.apply_error, 
                            eo.status as email_status, eo.to_addresses, eo.cc_addresses
                 FROM patches p
                 LEFT JOIN messages m ON p.message_id = m.message_id
                 LEFT JOIN email_outbox eo ON eo.patch_id = p.id
                 WHERE p.patchset_id = ? 
                 ORDER BY p.part_index ASC
                 LIMIT ? OFFSET ?",
                    libsql::params![pid, limit_val, offset_val],
                )
                .await?;

            #[allow(clippy::similar_names)]
            while let Ok(Some(p)) = patch_rows.next().await {
                let p_id: i64 = p.get(0)?;
                patch_ids.push(p_id);
                let mut p_status = p.get::<Option<String>>(5).ok().flatten();
                if is_embargoed && p_status.as_deref() == Some("Reviewed") {
                    p_status = Some("Embargoed".to_string());
                }
                patches.push(serde_json::json!({
                    "id": p_id,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                    "msg_db_id": p.get::<Option<i64>>(3).ok(),
                    "subject": p.get::<Option<String>>(4).ok(),
                    "status": p_status,
                    "apply_error": p.get::<Option<String>>(6).ok(),
                    "email_status": p.get::<Option<String>>(7).ok(),
                    "email_to": p.get::<Option<String>>(8).ok(),
                    "email_cc": p.get::<Option<String>>(9).ok(),
                }));
            }

            let mut reviews = Vec::new();
            let mut in_clause = patch_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            if in_clause.is_empty() {
                in_clause = "-1".to_string();
            }
            let query_str = format!(
                "SELECT r.summary, r.created_at, ai.output_raw, 
                        r.result_description, r.status, r.inline_review, ai.tokens_in, ai.tokens_out, r.patch_id, r.id, ai.tokens_cached, r.budget_flags
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 WHERE r.patchset_id = ? AND (r.patch_id IS NULL OR r.patch_id IN ({}))
                 ORDER BY r.created_at ASC", in_clause);

            let mut params = vec![libsql::Value::Integer(pid)];
            for &pid_val in &patch_ids {
                params.push(libsql::Value::Integer(pid_val));
            }

            let mut rev_rows = self.conn.query(&query_str, params).await?;

            while let Ok(Some(r)) = rev_rows.next().await {
                reviews.push(serde_json::json!({
                    "summary": r.get::<Option<String>>(0).ok(),
                    "created_at": r.get::<Option<i64>>(1).ok(),
                    "output": r.get::<Option<String>>(2).ok(),
                    "result": r.get::<Option<String>>(3).ok(),
                    "status": r.get::<Option<String>>(4).ok(),
                    "inline_review": r.get::<Option<String>>(5).ok(),
                    "tokens_in": r.get::<Option<u32>>(6).ok(),
                    "tokens_out": r.get::<Option<u32>>(7).ok(),
                    "patch_id": r.get::<Option<i64>>(8).ok(),
                    "id": r.get::<i64>(9).ok(),
                    "tokens_cached": r.get::<Option<u32>>(10).ok(),
                    "budget_flags": r.get::<Option<i64>>(11).ok().flatten().unwrap_or(0),
                    "model": model_name.clone(),
                    "provider": provider.clone(),
                    "prompts_hash": prompts_git_hash.clone(),
                    "baseline": baseline.clone()
                }));
            }

            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
                    }));
                }
            }

            let mut final_status = status;
            if is_embargoed && final_status.as_deref() == Some("Reviewed") {
                final_status = Some("Embargoed".to_string());
            }

            let reviews = if is_embargoed { Vec::new() } else { reviews };

            Ok(Some(serde_json::json!({
                "id": pid,
                "message_id": mid,
                "subject": subject,
                "author": author,
                "date": date,
                "status": final_status,
                "failed_reason": failed_reason,
                "to": to,
                "cc": cc,
                "total_parts": total_parts,
                "total_patches_in_db": total_patches,
                "page": page_val,
                "limit": limit_val,
                "received_parts": received_parts,
                "reviews": reviews,
                "patches": patches,
                "thread": messages,
                "subsystems": subsystems,
                "model_name": model_name,
                "prompts_git_hash": prompts_git_hash,
                "baseline_logs": baseline_logs,
                "baseline": baseline,
                "provider": provider,
                "embargo_until": embargo_until,
                "mr_url": mr_url,
                "slug": slug
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_summary_by_msgid(
        &self,
        msg_id: &str,
        page: Option<u32>,
        limit: Option<u32>,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE cover_letter_message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self
                .get_patchset_summary(id, page, limit, bypass_embargo)
                .await;
        }

        let mut rows = self
            .conn
            .query(
                "SELECT patchset_id FROM patches WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self
                .get_patchset_summary(id, page, limit, bypass_embargo)
                .await;
        }

        Ok(None)
    }

    pub async fn get_patchset_details_by_slug(
        &self,
        slug: &str,
        page: Option<u32>,
        limit: Option<u32>,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE slug = ?",
                libsql::params![slug],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self
                .get_patchset_details(id, page, limit, bypass_embargo)
                .await;
        }

        Ok(None)
    }

    pub async fn get_patchset_summary_by_slug(
        &self,
        slug: &str,
        page: Option<u32>,
        limit: Option<u32>,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE slug = ?",
                libsql::params![slug],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self
                .get_patchset_summary(id, page, limit, bypass_embargo)
                .await;
        }

        Ok(None)
    }

    pub async fn get_review_details(
        &self,
        id: i64,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT r.id, r.model, r.summary, r.created_at, ai.input_context, ai.output_raw, 
                        b.repo_url, b.branch, b.last_known_commit,
                        r.provider, r.prompts_git_hash, r.result_description,
                        r.status, r.inline_review, r.logs, ai.tokens_in, ai.tokens_out, r.patch_id, ai.tokens_cached,
                        r.budget_flags, p.embargo_until
             FROM reviews r
             LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
             LEFT JOIN baselines b ON r.baseline_id = b.id
             LEFT JOIN patchsets p ON r.patchset_id = p.id
             WHERE r.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(r)) = rows.next().await {
            let embargo_until: Option<i64> = r.get(20).ok();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            let is_embargoed = !bypass_embargo && embargo_until.map(|u| u > now).unwrap_or(false);
            if is_embargoed {
                return Ok(Some(serde_json::json!({
                    "id": r.get::<i64>(0)?,
                    "status": "Embargoed",
                    "embargo_until": embargo_until,
                })));
            }
            Ok(Some(serde_json::json!({
                "id": r.get::<i64>(0)?,
                "model": r.get::<Option<String>>(1).ok(),
                "summary": r.get::<Option<String>>(2).ok(),
                "created_at": r.get::<Option<i64>>(3).ok(),
                "input": r.get::<Option<String>>(4).ok(),
                "output": r.get::<Option<String>>(5).ok(),
                "baseline": {
                    "repo_url": r.get::<Option<String>>(6).ok(),
                    "branch": r.get::<Option<String>>(7).ok(),
                    "commit": r.get::<Option<String>>(8).ok(),
                },
                "provider": r.get::<Option<String>>(9).ok(),
                "prompts_hash": r.get::<Option<String>>(10).ok(),
                "result": r.get::<Option<String>>(11).ok(),
                "status": r.get::<Option<String>>(12).ok(),
                "inline_review": r.get::<Option<String>>(13).ok(),
                "logs": r.get::<Option<String>>(14).ok(),
                "tokens_in": r.get::<Option<u32>>(15).ok(),
                "tokens_out": r.get::<Option<u32>>(16).ok(),
                "patch_id": r.get::<Option<i64>>(17).ok(),
                "tokens_cached": r.get::<Option<u32>>(18).ok(),
                "budget_flags": r.get::<Option<i64>>(19).ok().flatten().unwrap_or(0),
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_latest_review_for_patchset(
        &self,
        patchset_id: i64,
        bypass_embargo: bool,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM reviews WHERE patchset_id = ? ORDER BY created_at DESC LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            self.get_review_details(id, bypass_embargo).await
        } else {
            Ok(None)
        }
    }

    pub async fn get_patch_diffs(
        &self,
        patchset_id: i64,
    ) -> Result<Vec<(i64, i64, String, String, String, i64, String)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.part_index, p.diff, m.subject, m.author, m.date, m.message_id 
             FROM patches p 
             JOIN messages m ON p.message_id = m.message_id 
             WHERE p.patchset_id = ? 
             ORDER BY p.part_index ASC",
                libsql::params![patchset_id],
            )
            .await?;

        let mut diffs = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let index: i64 = row.get(1).unwrap_or(0);
            let diff: String = row.get(2)?;
            let subject: String = row.get(3).unwrap_or_default();
            let author: String = row.get(4).unwrap_or_default();
            let date: i64 = row.get(5).unwrap_or(0);
            let message_id: String = row.get(6)?;
            diffs.push((id, index, diff, subject, author, date, message_id));
        }
        Ok(diffs)
    }

    pub async fn get_pending_patchsets(&self, limit: usize) -> Result<Vec<PatchsetRow>> {
        let mut rows = self.conn.query(
            "SELECT id, subject, status, thread_id, author, date, cover_letter_message_id, total_parts, received_parts, baseline_id, failed_reason, target_review_count, skip_filters, only_filters, embargo_until, slug
             FROM patchsets WHERE status = 'Pending' ORDER BY date ASC LIMIT ?",
            libsql::params![limit as i64],
        ).await?;

        let mut patchsets = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            patchsets.push(PatchsetRow {
                id: row.get(0).unwrap_or_default(),
                subject: row.get(1).ok(),
                status: row.get(2).ok(),
                thread_id: row.get(3).ok(),
                author: row.get(4).ok(),
                date: row.get(5).ok(),
                message_id: row.get(6).ok(),
                total_parts: row.get(7).ok(),
                received_parts: row.get(8).ok(),
                subsystems: Vec::new(),
                findings_low: None,
                findings_medium: None,
                findings_high: None,
                findings_critical: None,
                baseline_id: row.get(9).ok(),
                failed_reason: row.get(10).ok(),
                target_review_count: row.get(11).ok(),
                skip_filters: row.get(12).ok(),
                only_filters: row.get(13).ok(),
                model_name: None,
                prompts_git_hash: None,
                baseline_logs: None,
                provider: None,
                embargo_until: row.get(14).ok(),
                mr_url: None,
                mr_title: None,
                mr_number: None,
                slug: row.get(15).ok(),
                budget_flags_or: None,
                concerns_total: None,
                concerns_unique: None,
                findings_multi_stage: None,
                cross_review_status: None,
                cross_reviewed_at: None,
            });
        }
        Ok(patchsets)
    }

    pub async fn get_releasable_embargoed_patchsets(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<PatchsetRow>> {
        let sql = format!(
            "SELECT p.id, p.subject, p.status, p.thread_id, p.author, p.date, p.cover_letter_message_id, p.total_parts, p.received_parts, p.baseline_id, p.failed_reason, p.target_review_count, p.skip_filters, p.only_filters, p.embargo_until
             FROM patchsets p
             WHERE p.status = 'Reviewed' AND p.embargo_until IS NOT NULL
             AND (p.embargo_release_started_at IS NULL OR p.embargo_release_started_at <= ?)
             AND (
                 p.embargo_until <= ?
                 OR ({CLEAN_PATCHSET_PREDICATE})
             )
             ORDER BY CASE WHEN p.embargo_until <= ? THEN 0 ELSE 1 END, p.date ASC LIMIT ?"
        );
        let mut rows = self
            .conn
            .query(&sql, libsql::params![now - 600, now, now, limit as i64])
            .await?;

        let mut patchsets = Vec::new();
        loop {
            match rows.next().await {
                Ok(Some(row)) => {
                    patchsets.push(PatchsetRow {
                        id: row.get(0).unwrap_or_default(),
                        subject: row.get(1).ok(),
                        status: row.get(2).ok(),
                        thread_id: row.get(3).ok(),
                        author: row.get(4).ok(),
                        date: row.get(5).ok(),
                        message_id: row.get(6).ok(),
                        total_parts: row.get(7).ok(),
                        received_parts: row.get(8).ok(),
                        subsystems: Vec::new(),
                        findings_low: None,
                        findings_medium: None,
                        findings_high: None,
                        findings_critical: None,
                        baseline_id: row.get(9).ok(),
                        failed_reason: row.get(10).ok(),
                        target_review_count: row.get(11).ok(),
                        skip_filters: row.get(12).ok(),
                        only_filters: row.get(13).ok(),
                        model_name: None,
                        prompts_git_hash: None,
                        baseline_logs: None,
                        provider: None,
                        embargo_until: row.get(14).ok(),
                        mr_url: None,
                        mr_title: None,
                        mr_number: None,
                        slug: None,
                        budget_flags_or: None,
                        concerns_total: None,
                        concerns_unique: None,
                        findings_multi_stage: None,
                        cross_review_status: None,
                        cross_reviewed_at: None,
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("Error fetching row: {:?}", e);
                    break;
                }
            }
        }
        Ok(patchsets)
    }

    pub async fn get_patchset_review_outcome(
        &self,
        patchset_id: i64,
    ) -> Result<PatchsetReviewOutcome> {
        let mut rows = self
            .conn
            .query(
                "SELECT status, COALESCE(target_review_count, 1) FROM patchsets WHERE id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(PatchsetReviewOutcome::Incomplete);
        };
        let status: String = row.get(0).unwrap_or_default();
        let target_review_count: i64 = row.get(1).unwrap_or(1);
        if status != ReviewStatus::Reviewed.as_str() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut no_ai_rows = self
            .conn
            .query(
                "SELECT 1 FROM reviews
                 WHERE patchset_id = ? AND status = 'Skipped'
                   AND result_description = 'Skipped AI review via --no-ai'
                 LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;
        if no_ai_rows.next().await?.is_some() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut incomplete_rows = self
            .conn
            .query(
                "SELECT 1
                 FROM patches p
                 WHERE p.patchset_id = ?
                   AND COALESCE(p.status, '') != 'Skipped'
                   AND NOT EXISTS (
                       SELECT 1 FROM reviews skipped
                       WHERE skipped.patch_id = p.id
                         AND skipped.status = 'Skipped'
                         AND skipped.result_description = 'Skipped: touches only ignored files'
                   )
                   AND (
                       SELECT COUNT(*) FROM reviews r
                       WHERE r.patch_id = p.id AND r.status = 'Reviewed'
                   ) < ?
                 LIMIT 1",
                libsql::params![patchset_id, target_review_count],
            )
            .await?;
        if incomplete_rows.next().await?.is_some() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut reviewed_rows = self
            .conn
            .query(
                "SELECT 1 FROM reviews WHERE patchset_id = ? AND status = 'Reviewed' LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;
        if reviewed_rows.next().await?.is_none() {
            return Ok(PatchsetReviewOutcome::Incomplete);
        }

        let mut finding_rows = self
            .conn
            .query(
                "SELECT 1 FROM findings f
                 JOIN reviews r ON r.id = f.review_id
                 WHERE r.patchset_id = ? AND r.status = 'Reviewed'
                 LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;
        if finding_rows.next().await?.is_some() {
            Ok(PatchsetReviewOutcome::HasFindings)
        } else {
            Ok(PatchsetReviewOutcome::Clean)
        }
    }

    pub async fn claim_patchset_embargo_release(&self, id: i64, now: i64) -> Result<bool> {
        let sql = format!(
            "UPDATE patchsets AS p SET embargo_release_started_at = ?
             WHERE p.id = ? AND p.status = 'Reviewed' AND p.embargo_until IS NOT NULL
               AND (p.embargo_release_started_at IS NULL OR p.embargo_release_started_at <= ?)
               AND (p.embargo_until <= ? OR ({CLEAN_PATCHSET_PREDICATE}))"
        );
        let updated = self
            .conn
            .execute(&sql, libsql::params![now, id, now - 600, now])
            .await?;
        Ok(updated == 1)
    }

    pub async fn clear_patchset_embargo_release_claim(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET embargo_release_started_at = NULL WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_completed_reviews_for_release(
        &self,
        patchset_id: i64,
    ) -> Result<Vec<ReleaseReview>> {
        let mut rows = self
            .conn
            .query(
                "SELECT r.id, r.patch_id, r.inline_review, r.summary, m.message_id, p.part_index
             FROM reviews r
             JOIN patches p ON r.patch_id = p.id
             JOIN messages m ON p.message_id = m.message_id
             WHERE r.patchset_id = ? AND r.status = 'Reviewed'",
                libsql::params![patchset_id],
            )
            .await?;

        let mut temp_reviews = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let review_id: i64 = row.get(0)?;
            let patch_id: i64 = row.get(1)?;
            let inline_review: String = row.get(2).unwrap_or_default();
            let summary: String = row.get(3).unwrap_or_default();
            let patch_message_id: String = row.get(4).unwrap_or_default();
            let index: i64 = row.get(5).unwrap_or_default();
            temp_reviews.push((
                review_id,
                patch_id,
                inline_review,
                summary,
                patch_message_id,
                index,
            ));
        }

        let mut reviews = Vec::new();
        for (review_id, patch_id, inline_review, summary, patch_message_id, index) in temp_reviews {
            // Fetch findings for this review
            let mut findings_rows = self.conn.query(
                "SELECT severity, problem, severity_explanation, preexisting, locations FROM findings WHERE review_id = ?",
                libsql::params![review_id],
            ).await?;

            let mut findings = Vec::new();
            while let Ok(Some(f_row)) = findings_rows.next().await {
                let severity_int: i64 = f_row.get(0).unwrap_or(1);
                let severity = match severity_int {
                    4 => "Critical",
                    3 => "High",
                    2 => "Medium",
                    _ => "Low",
                }
                .to_string();
                let problem: String = f_row.get(1).unwrap_or_default();
                let severity_explanation: Option<String> = f_row.get(2).ok();
                let preexisting_int: Option<i64> = f_row.get(3).ok();
                let preexisting = preexisting_int.map(|val| val != 0);
                let locations_str: Option<String> = f_row.get(4).ok();
                let locations =
                    locations_str.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

                findings.push(json!({
                    "severity": severity,
                    "problem": problem,
                    "severity_explanation": severity_explanation,
                    "preexisting": preexisting,
                    "locations": locations,
                }));
            }

            reviews.push(ReleaseReview {
                patch_id,
                patch_message_id,
                index,
                inline_review,
                summary,
                findings,
            });
        }
        Ok(reviews)
    }

    pub async fn update_patchset_status(&self, id: i64, status: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET status = ? WHERE id = ?",
                libsql::params![status, id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patch_status(&self, patch_id: i64, status: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patches SET status = ? WHERE id = ?",
                libsql::params![status, patch_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_patchset_status(&self, id: i64) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT status FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn cancel_patchset(&self, id: i64, force: bool) -> Result<bool> {
        let query = if force {
            "UPDATE patchsets SET status = 'Cancelled' WHERE id = ? AND status IN ('Pending', 'Incomplete', 'In Review')"
        } else {
            "UPDATE patchsets SET status = 'Cancelled' WHERE id = ? AND status IN ('Pending', 'Incomplete')"
        };
        let count = self.conn.execute(query, libsql::params![id]).await?;
        Ok(count > 0)
    }

    pub async fn rerun_patchset(&self, id: i64) -> Result<()> {
        // 1. Get current status of the patchset
        let mut rows = self
            .conn
            .query(
                "SELECT status FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await?;

        let mut current_status = None;
        if let Ok(Some(row)) = rows.next().await {
            let status: String = row.get(0)?;
            current_status = Some(status);
        }

        let should_increment = current_status.as_deref() == Some("Reviewed");

        // 2. Reset patchset status to Pending
        self.conn
            .execute(
                "UPDATE patchsets SET status = 'Pending',
                 cross_review_status = 'disabled', cross_reviewed_at = NULL,
                 cross_review_generation = 0
                 WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        self.conn
            .execute(
                "UPDATE cross_review_jobs SET status = 'superseded',
                 lease_until = NULL, lease_token = NULL,
                 last_error = 'superseded by local review rerun'
                 WHERE patchset_id = ? AND status IN ('pending', 'processing')",
                libsql::params![id],
            )
            .await?;

        // 3. Increment target_review_count only if it was previously Reviewed
        if should_increment {
            self.conn
                .execute(
                    "UPDATE patchsets SET target_review_count = COALESCE(target_review_count, 1) + 1 WHERE id = ?",
                    libsql::params![id],
                )
                .await?;
        }

        // 4. Delete associated tool usages and findings for failed reviews that block retrying
        self.conn
            .execute(
                "DELETE FROM tool_usages WHERE review_id IN (
                    SELECT id FROM reviews WHERE patchset_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL
                )",
                libsql::params![id],
            )
            .await?;

        self.conn
            .execute(
                "DELETE FROM findings WHERE review_id IN (
                    SELECT id FROM reviews WHERE patchset_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL
                )",
                libsql::params![id],
            )
            .await?;

        // 5. Delete failed reviews that block retrying (infra failures)
        self.conn
            .execute(
                "DELETE FROM reviews WHERE patchset_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL",
                libsql::params![id],
            )
            .await?;

        Ok(())
    }

    pub async fn rerun_patch(&self, patchset_id: i64, _patch_id: i64) -> Result<()> {
        // NOTE: Currently we only support re-running the entire patchset to trigger more reviews.
        // Even if the user requested a specific patch, we increment the set's target count
        // to allow the reviewer service to proceed.
        self.rerun_patchset(patchset_id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_fetching_patchset(
        &self,
        article_id: &str,
        subject: &str,
        skip_filters: Option<&Vec<String>>,
        only_filters: Option<&Vec<String>>,
        mr_url: Option<&str>,
        mr_title: Option<&str>,
        mr_number: Option<i64>,
        slug: Option<&str>,
    ) -> Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;

        let root_msg_id = if article_id.contains('@') {
            article_id.to_string()
        } else {
            format!("{}@sashiko.local", article_id)
        };

        let clid_candidates = vec![article_id.to_string(), root_msg_id.clone()];

        let skip_filters_json = skip_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        let only_filters_json = only_filters.map(|f| serde_json::to_string(f).unwrap_or_default());

        // 1. Check if it already exists
        for clid in clid_candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, status FROM patchsets WHERE cover_letter_message_id = ?",
                    libsql::params![clid.clone()],
                )
                .await?;

            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                let status: String = row.get(1).unwrap_or_default();

                // Only reset to Fetching if it failed or is currently fetching.
                // We don't want to reset if it is already Incomplete, Pending, or Reviewed.
                if status == "Failed" || status == "Fetching" {
                    self.conn.execute(
                        "UPDATE patchsets SET status = 'Fetching', failed_reason = NULL, skip_filters = ?, only_filters = ?, mr_url = ?, mr_title = ?, mr_number = ?, slug = ? WHERE id = ?",
                        libsql::params![skip_filters_json.clone(), only_filters_json.clone(), mr_url, mr_title, mr_number, slug, id]
                    ).await?;
                }
                return Ok(id);
            }
        }

        // 2. Ensure a placeholder thread and message exist to satisfy Foreign Key constraints
        let thread_id = self.ensure_thread_for_message(&root_msg_id, now).await?;

        // 3. Create the fetching patchset
        let mut rows = self.conn
            .query(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, status, date, skip_filters, only_filters, mr_url, mr_title, mr_number, slug)
                     VALUES (?, ?, ?, 'Fetching', ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![thread_id, root_msg_id, subject, now, skip_filters_json, only_filters_json, mr_url, mr_title, mr_number, slug],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get patchset ID"))
        }
    }
    pub async fn update_patchset_error(&self, article_id: &str, error: &str) -> Result<()> {
        let root_msg_id = if article_id.contains('@') {
            article_id.to_string()
        } else {
            format!("{}@sashiko.local", article_id)
        };
        self.conn
            .execute(
                "UPDATE patchsets SET status = 'Failed', failed_reason = ? WHERE cover_letter_message_id = ?",
                libsql::params![error, root_msg_id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patchset_baseline_info(
        &self,
        id: i64,
        baseline_id: Option<i64>,
        model_name: Option<&str>,
        prompts_hash: Option<&str>,
        logs: Option<&str>,
        provider: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET baseline_id = ?, model_name = ?, prompts_git_hash = ?, baseline_logs = ?, provider = ? WHERE id = ?",
                libsql::params![baseline_id, model_name, prompts_hash, logs, provider, id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patch_application_status(
        &self,
        patchset_id: i64,
        part_index: i64,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE patches SET status = ?, apply_error = ? WHERE patchset_id = ? AND part_index = ?",
            libsql::params![status, error, patchset_id, part_index],
        ).await?;
        Ok(())
    }

    pub async fn reset_reviewing_status(&self) -> Result<u64> {
        let status_pending = ReviewStatus::Pending.as_str();
        // Reset Patchsets
        let count_ps = self
            .conn
            .execute(
                format!(
                    "UPDATE patchsets SET status = '{}' WHERE status IN ('In Review', 'Reviewing')",
                    status_pending
                )
                .as_str(),
                (),
            )
            .await?;

        // Reset Reviews
        let count_rev = self
            .conn
            .execute(
                format!(
                    "UPDATE reviews SET status = '{}' WHERE status = 'In Review'",
                    status_pending
                )
                .as_str(),
                (),
            )
            .await?;

        Ok(count_ps + count_rev)
    }

    pub async fn get_patchset_counts_by_status(
        &self,
    ) -> Result<std::collections::HashMap<String, usize>> {
        let mut rows = self
            .conn
            .query("SELECT status, COUNT(*) FROM patchsets GROUP BY status", ())
            .await?;

        let mut counts = std::collections::HashMap::new();
        while let Ok(Some(row)) = rows.next().await {
            let status: Option<String> = row.get(0).ok();
            let count: i64 = row.get(1)?;
            let status_key = status.unwrap_or_else(|| "Unknown".to_string());
            counts.insert(status_key, count as usize);
        }
        Ok(counts)
    }
}

impl Database {
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_email_outbox(
        &self,
        patch_id: i64,
        status: &str,
        to_addresses: &str,
        cc_addresses: &str,
        subject: &str,
        in_reply_to: &str,
        references_hdr: &str,
        body: &str,
    ) -> Result<()> {
        // Prevent duplicate emails for the same patch
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM email_outbox WHERE patch_id = ?",
                libsql::params![patch_id],
            )
            .await?;

        if let Ok(Some(_)) = rows.next().await {
            tracing::info!(
                "Email outbox entry already exists for patch_id {}, skipping to prevent duplicates.",
                patch_id
            );
            return Ok(());
        }

        let created_at = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO email_outbox (patch_id, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                patch_id,
                status,
                to_addresses,
                cc_addresses,
                subject,
                in_reply_to,
                references_hdr,
                body,
                created_at,
            ],
        ).await?;
        Ok(())
    }

    pub async fn lock_pending_email(&self) -> Result<Option<EmailOutboxRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self.conn.query(
            "UPDATE email_outbox 
             SET status = 'Sending', locked_at = ? 
             WHERE id = (SELECT id FROM email_outbox WHERE status = 'Pending' LIMIT 1)
             RETURNING id, patch_id, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, locked_at, error_log, created_at",
            libsql::params![now]
        ).await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let patch_id: Option<i64> = row.get::<i64>(1).ok();
            let status: String = row.get(2)?;
            let to_addresses: String = row.get(3)?;
            let cc_addresses: String = row.get(4)?;
            let subject: String = row.get(5)?;
            let in_reply_to: String = row.get(6)?;
            let references_hdr: String = row.get(7)?;
            let body: String = row.get(8)?;
            let locked_at: Option<i64> = row.get(9).ok();
            let error_log: Option<String> = row.get(10).ok();
            let created_at: i64 = row.get(11)?;

            Ok(Some(EmailOutboxRow {
                id,
                patch_id,
                status,
                to_addresses,
                cc_addresses,
                subject,
                in_reply_to,
                references_hdr,
                body,
                locked_at,
                error_log,
                created_at,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn mark_email_sent(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE email_outbox SET status = 'Sent', locked_at = NULL WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn mark_email_failed(&self, id: i64, error_log: &str) -> Result<()> {
        self.conn.execute("UPDATE email_outbox SET status = 'Failed', error_log = ?, locked_at = NULL WHERE id = ?", libsql::params![error_log.to_string(), id]).await?;
        Ok(())
    }

    pub async fn sweep_ghost_emails(&self) -> Result<u64> {
        let ten_mins_ago = chrono::Utc::now().timestamp() - 600;
        let count = self.conn.execute(
            "UPDATE email_outbox SET status = 'Pending', locked_at = NULL WHERE status = 'Sending' AND locked_at < ?",
            libsql::params![ten_mins_ago]
        ).await?;
        Ok(count)
    }

    // -- Patchwork outbox operations --

    pub async fn insert_patchwork_outbox(
        &self,
        patch_msg_id: &str,
        api_url: &str,
        check_state: &str,
        description: &str,
        target_url: &str,
        context: &str,
    ) -> Result<()> {
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM patchwork_outbox
                 WHERE patch_msg_id = ? AND api_url = ? AND context = ?",
                libsql::params![patch_msg_id, api_url, context],
            )
            .await?;
        if rows.next().await?.is_some() {
            return Ok(());
        }

        let created_at = chrono::Utc::now().timestamp();
        self.conn
            .execute(
                "INSERT INTO patchwork_outbox (patch_msg_id, api_url, check_state, description, target_url, context, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                libsql::params![
                    patch_msg_id,
                    api_url,
                    check_state,
                    description,
                    target_url,
                    context,
                    created_at,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn lock_pending_patchwork(&self) -> Result<Option<PatchworkOutboxRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self
            .conn
            .query(
                "UPDATE patchwork_outbox
                 SET status = 'Sending', locked_at = ?
                 WHERE id = (
                     SELECT id FROM patchwork_outbox
                     WHERE status = 'Pending'
                       AND (next_retry_at IS NULL OR next_retry_at <= ?)
                     LIMIT 1
                 )
                 RETURNING id, patch_msg_id, api_url, check_state, description, target_url, context, status, retry_count, next_retry_at, locked_at, error_log, created_at",
                libsql::params![now, now],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let patch_msg_id: String = row.get(1)?;
            let api_url: String = row.get(2)?;
            let check_state: String = row.get(3)?;
            let description: String = row.get(4)?;
            let target_url: String = row.get(5)?;
            let context: String = row.get(6)?;
            let status: String = row.get(7)?;
            let retry_count: i64 = row.get(8)?;
            let next_retry_at: Option<i64> = row.get::<i64>(9).ok();
            let locked_at: Option<i64> = row.get::<i64>(10).ok();
            let error_log: Option<String> = row.get::<String>(11).ok();
            let created_at: i64 = row.get(12)?;

            Ok(Some(PatchworkOutboxRow {
                id,
                patch_msg_id,
                api_url,
                check_state,
                description,
                target_url,
                context,
                status,
                retry_count,
                next_retry_at,
                locked_at,
                error_log,
                created_at,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn mark_patchwork_sent(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Sent', locked_at = NULL WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn mark_patchwork_failed(&self, id: i64, error_log: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Failed', error_log = ?, locked_at = NULL WHERE id = ?",
                libsql::params![error_log.to_string(), id],
            )
            .await?;
        Ok(())
    }

    /// Mark a patchwork outbox entry for retry at a future timestamp.
    /// Increments retry_count, sets next_retry_at, and returns to
    /// Pending status so the worker loop continues without blocking.
    pub async fn set_patchwork_retry_at(&self, id: i64, next_retry_at: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Pending', retry_count = retry_count + 1, next_retry_at = ?, locked_at = NULL WHERE id = ?",
                libsql::params![next_retry_at, id],
            )
            .await?;
        Ok(())
    }

    pub async fn sweep_ghost_patchwork(&self) -> Result<u64> {
        let ten_mins_ago = chrono::Utc::now().timestamp() - 600;
        let count = self.conn
            .execute(
                "UPDATE patchwork_outbox SET status = 'Pending', locked_at = NULL WHERE status = 'Sending' AND locked_at < ?",
                libsql::params![ten_mins_ago],
            )
            .await?;
        Ok(count)
    }

    /// Insert a patchwork notification email into the email outbox.
    ///
    /// Uses patch_id = NULL to avoid colliding with the per-patch dedup
    /// guard in insert_email_outbox(). The EmailWorker processes these
    /// rows normally since it picks up any row with status = 'Pending'.
    pub async fn insert_patchwork_notification(
        &self,
        status: &str,
        to_address: &str,
        subject: &str,
        in_reply_to: &str,
        references_hdr: &str,
        body: &str,
    ) -> Result<()> {
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM email_outbox
                 WHERE patch_id IS NULL AND to_addresses = ? AND subject = ? AND in_reply_to = ?",
                libsql::params![
                    serde_json::to_string(&[to_address])
                        .map_err(|e| libsql::Error::Misuse(e.to_string()))?,
                    subject,
                    in_reply_to
                ],
            )
            .await?;
        if rows.next().await?.is_some() {
            return Ok(());
        }

        let created_at = chrono::Utc::now().timestamp();
        let to_json = serde_json::to_string(&[to_address])
            .map_err(|e| libsql::Error::Misuse(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO email_outbox (patch_id, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, created_at)
                 VALUES (NULL, ?, ?, '[]', ?, ?, ?, ?, ?)",
                libsql::params![
                    status,
                    to_json,
                    subject,
                    in_reply_to,
                    references_hdr,
                    body,
                    created_at,
                ],
            )
            .await?;
        Ok(())
    }

    // -- Patchwork patch state (feedback from patchwork back into sashiko) --

    /// Record patch states observed on patchwork and advance the sweep
    /// watermark.
    ///
    /// Records whose message-id is not a patch sashiko reviewed are dropped and
    /// counted in `unknown`: no row is written and no placeholder patch or
    /// patchset is created.  That is the normal case, since the poller pushes
    /// the whole project's event stream and most of it is series we never
    /// submitted.
    ///
    /// Ordering is guarded by the patchwork event id, which increases with the
    /// event date, so re-sweeping a date range is a no-op and an out-of-order
    /// event cannot move a patch backwards.
    pub async fn record_patchwork_states(
        &self,
        records: &[crate::patchwork::PatchworkStateRecord],
        last_event_date: Option<&str>,
        last_event_id: Option<i64>,
    ) -> Result<crate::patchwork::PatchworkIngestSummary> {
        use crate::patchwork::{is_known_state, parse_patchwork_timestamp, state_outcome};

        let mut summary = crate::patchwork::PatchworkIngestSummary::default();
        let now = chrono::Utc::now().timestamp();
        let mut unknown_states: std::collections::BTreeSet<String> = Default::default();

        self.begin_transaction().await?;

        for record in records {
            let msgid = record
                .msgid
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>');
            if msgid.is_empty() || record.state.trim().is_empty() {
                summary.unknown += 1;
                continue;
            }

            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM patches WHERE message_id = ?",
                    libsql::params![msgid],
                )
                .await?;
            let patch_id: i64 = match rows.next().await? {
                Some(row) => row.get(0)?,
                None => {
                    summary.unknown += 1;
                    continue;
                }
            };
            drop(rows);

            let state = record.state.trim();
            if !is_known_state(state) {
                unknown_states.insert(state.to_string());
            }
            let outcome = state_outcome(state);
            let changed_at = record
                .changed_at
                .as_deref()
                .and_then(parse_patchwork_timestamp)
                .unwrap_or(now);

            if record.seed {
                // A seed only establishes the baseline.  If an event already
                // moved this patch, leave the current state alone and just
                // backfill what the seed uniquely knows.
                self.conn
                    .execute(
                        "INSERT INTO patchwork_patch_state
                             (patch_id, pw_patch_id, pw_series_id, state, outcome,
                              initial_state, state_changed_at, updated_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                         ON CONFLICT(patch_id) DO UPDATE SET
                             pw_patch_id = COALESCE(patchwork_patch_state.pw_patch_id, excluded.pw_patch_id),
                             pw_series_id = COALESCE(patchwork_patch_state.pw_series_id, excluded.pw_series_id),
                             initial_state = COALESCE(patchwork_patch_state.initial_state, excluded.initial_state),
                             updated_at = excluded.updated_at",
                        libsql::params![
                            patch_id,
                            record.pw_patch_id,
                            record.pw_series_id,
                            state,
                            outcome,
                            state,
                            changed_at,
                            now,
                        ],
                    )
                    .await?;
                summary.stored += 1;
                continue;
            }

            // Transition.  Skip it if we have already applied this event or a
            // later one for the same patch.
            if let Some(event_id) = record.event_id {
                let mut rows = self
                    .conn
                    .query(
                        "SELECT last_event_id FROM patchwork_patch_state WHERE patch_id = ?",
                        libsql::params![patch_id],
                    )
                    .await?;
                if let Some(row) = rows.next().await?
                    && row
                        .get::<Option<i64>>(0)?
                        .is_some_and(|seen| seen >= event_id)
                {
                    summary.stale += 1;
                    continue;
                }
            }

            self.conn
                .execute(
                    "INSERT INTO patchwork_patch_state
                         (patch_id, pw_patch_id, pw_series_id, state, outcome,
                          previous_state, actor, state_changed_at, last_event_id, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(patch_id) DO UPDATE SET
                         pw_patch_id = COALESCE(excluded.pw_patch_id, patchwork_patch_state.pw_patch_id),
                         pw_series_id = COALESCE(excluded.pw_series_id, patchwork_patch_state.pw_series_id),
                         state = excluded.state,
                         outcome = excluded.outcome,
                         previous_state = excluded.previous_state,
                         actor = excluded.actor,
                         state_changed_at = excluded.state_changed_at,
                         last_event_id = excluded.last_event_id,
                         updated_at = excluded.updated_at",
                    libsql::params![
                        patch_id,
                        record.pw_patch_id,
                        record.pw_series_id,
                        state,
                        outcome,
                        record.previous_state.as_deref(),
                        record.actor.as_deref(),
                        changed_at,
                        record.event_id,
                        now,
                    ],
                )
                .await?;
            summary.stored += 1;
        }

        // The watermark moves in the same transaction as the records, so a
        // crash cannot leave it ahead of the data it describes.
        if last_event_date.is_some() || last_event_id.is_some() {
            self.conn
                .execute(
                    "INSERT INTO patchwork_sync (id, last_event_date, last_event_id, updated_at)
                     VALUES (1, ?, ?, ?)
                     ON CONFLICT(id) DO UPDATE SET
                         last_event_date = MAX(
                             COALESCE(excluded.last_event_date, patchwork_sync.last_event_date),
                             COALESCE(patchwork_sync.last_event_date, excluded.last_event_date)
                         ),
                         last_event_id = MAX(
                             COALESCE(excluded.last_event_id, 0),
                             COALESCE(patchwork_sync.last_event_id, 0)
                         ),
                         updated_at = excluded.updated_at",
                    libsql::params![last_event_date, last_event_id, now],
                )
                .await?;
        }

        self.commit_transaction().await?;

        if !unknown_states.is_empty() {
            warn!(
                "Unrecognised patchwork state(s) bucketed as active: {}",
                unknown_states.into_iter().collect::<Vec<_>>().join(", ")
            );
        }

        Ok(summary)
    }

    /// Daily patchwork outcome counts, split by severity group.
    ///
    /// Returns raw counts and lets the UI derive percentages, the same way the
    /// cost dashboard works off raw token counts.  One row per
    /// (day, scope, outcome); `day` is the series date so every patch in a
    /// series lands on the same day.
    ///
    /// A patch's severity group is the *maximum* severity of its findings, so
    /// the groups are disjoint and composition percentages sum to 100%.
    ///
    /// `scope = 'main'` uses the stored review's findings, excluding
    /// pre-existing ones to match the per-patch aggregation the UI already does.
    /// `scope = 'other'` pools the additional experiment models and is
    /// restricted to patches where an additional model actually ran — otherwise
    /// "ran and found nothing" would be conflated with "never ran".  A source
    /// row alone does not mean it ran: every candidate model gets one, most of
    /// them 'not_selected', and a selected one can still fail.
    /// model_experiment_findings has no preexisting flag, so that filter cannot
    /// be applied on the 'other' side; the asymmetry is deliberate.
    pub async fn get_patchwork_stats(&self, window_days: i64) -> Result<serde_json::Value> {
        let cutoff = chrono::Utc::now().timestamp() - window_days * 86400;

        let sql = "
            WITH pat AS (
                SELECT p.id AS patch_id,
                       strftime('%Y-%m-%d', s.date, 'unixepoch') AS day
                FROM patches p
                JOIN patchsets s ON s.id = p.patchset_id
                WHERE s.status = 'Reviewed' AND s.date >= ?
            ),
            main_sev AS (
                SELECT r.patch_id AS patch_id, MAX(f.severity) AS sev
                FROM reviews r
                JOIN findings f ON f.review_id = r.id
                WHERE r.status = 'Reviewed'
                  AND r.patch_id IS NOT NULL
                  AND COALESCE(f.preexisting, 0) = 0
                GROUP BY r.patch_id
            ),
            other_ran AS (
                SELECT DISTINCT r.patch_id AS patch_id
                FROM model_experiment_sources ms
                JOIN reviews r ON r.id = ms.review_id
                WHERE r.patch_id IS NOT NULL
                  AND ms.selected = 1
                  AND ms.status = 'completed'
            ),
            other_sev AS (
                SELECT r.patch_id AS patch_id, MAX(CASE lower(COALESCE(mf.severity, ''))
                           WHEN 'critical' THEN 4
                           WHEN 'high' THEN 3
                           WHEN 'medium' THEN 2
                           WHEN 'low' THEN 1
                           ELSE 0 END) AS sev
                FROM model_experiment_findings mf
                JOIN reviews r ON r.id = mf.review_id
                WHERE r.patch_id IS NOT NULL
                  AND mf.outcome IN ('both', 'additional_only', 'additional_hallucination')
                GROUP BY r.patch_id
            )
            SELECT pat.day, 'main' AS scope, st.outcome,
                   SUM(CASE WHEN COALESCE(main_sev.sev, 0) <= 0 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(main_sev.sev, 0) = 1 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(main_sev.sev, 0) = 2 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(main_sev.sev, 0) = 3 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(main_sev.sev, 0) >= 4 THEN 1 ELSE 0 END)
            FROM pat
            JOIN patchwork_patch_state st ON st.patch_id = pat.patch_id
            LEFT JOIN main_sev ON main_sev.patch_id = pat.patch_id
            GROUP BY pat.day, st.outcome
            UNION ALL
            SELECT pat.day, 'other' AS scope, st.outcome,
                   SUM(CASE WHEN COALESCE(other_sev.sev, 0) <= 0 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(other_sev.sev, 0) = 1 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(other_sev.sev, 0) = 2 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(other_sev.sev, 0) = 3 THEN 1 ELSE 0 END),
                   SUM(CASE WHEN COALESCE(other_sev.sev, 0) >= 4 THEN 1 ELSE 0 END)
            FROM pat
            JOIN patchwork_patch_state st ON st.patch_id = pat.patch_id
            JOIN other_ran ON other_ran.patch_id = pat.patch_id
            LEFT JOIN other_sev ON other_sev.patch_id = pat.patch_id
            GROUP BY pat.day, st.outcome
            ORDER BY 1, 2, 3";

        let mut rows = self.conn.query(sql, libsql::params![cutoff]).await?;
        let mut daily = Vec::new();
        while let Some(row) = rows.next().await? {
            daily.push(json!({
                "day": row.get::<String>(0)?,
                "scope": row.get::<String>(1)?,
                "outcome": row.get::<String>(2)?,
                "none": row.get::<i64>(3)?,
                "low": row.get::<i64>(4)?,
                "medium": row.get::<i64>(5)?,
                "high": row.get::<i64>(6)?,
                "critical": row.get::<i64>(7)?,
            }));
        }

        Ok(json!({
            "window_days": window_days,
            "daily": daily,
        }))
    }

    /// The poller's durable sweep watermark: the date of the newest event we
    /// have ingested, which the poller feeds back to patchwork as `since`.
    pub async fn get_patchwork_sync(&self) -> Result<serde_json::Value> {
        let mut rows = self
            .conn
            .query(
                "SELECT last_event_date, last_event_id, updated_at
                 FROM patchwork_sync WHERE id = 1",
                (),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            return Ok(json!({
                "last_event_date": row.get::<Option<String>>(0)?,
                "last_event_id": row.get::<Option<i64>>(1)?,
                "updated_at": row.get::<Option<i64>>(2)?,
            }));
        }
        Ok(json!({"last_event_date": null, "last_event_id": null, "updated_at": null}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;
    use std::sync::Arc;

    async fn setup_db() -> Arc<Database> {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();
        Arc::new(db)
    }

    #[tokio::test]
    async fn timeline_stats_count_findings_and_hallucinations_by_stage() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO patchsets (id, thread_id) VALUES (1, 1);
                 INSERT INTO reviews (id, patchset_id, created_at) VALUES
                    (1, 1, unixepoch('now')),
                    (2, 1, unixepoch('now', '-14 days')),
                    (3, 1, unixepoch('now'));
                 INSERT INTO findings (review_id, severity, source_stages) VALUES
                    (1, 1, '[1]'),
                    (1, 2, '[2]'),
                    (1, 3, '[3]'),
                    (1, 4, '[4]'),
                    (1, 1, '[1,2]'),
                    (1, 1, '[2,3]'),
                    (1, 1, '[3,3]'),
                    (1, 1, 'invalid'),
                    (1, 1, NULL),
                    (2, 1, '[1]');
                 INSERT INTO model_experiment_runs
                    (review_id, experiment_name, stage, status) VALUES
                    (1, 'main', 1, 'completed'),
                    (1, 'main', 2, 'completed'),
                    (1, 'main', 3, 'completed'),
                    (1, 'main', 4, 'completed'),
                    (1, 'variant', 1, 'completed'),
                    (2, 'main', 1, 'completed'),
                    (3, 'main', 1, 'completed'),
                    (3, 'main', 2, 'failed');
                 INSERT INTO local_canonical_findings
                    (review_id, finding_id, finding_json, accepted) VALUES
                    (1, 'main-one-stage', json_object(
                        'source_models', json_array('main'),
                        'source_stages', json_array(2)), 0),
                    (1, 'main-two-stages', json_object(
                        'source_models', json_array('main'),
                        'source_stages', json_array(2, 4)), 0),
                    (1, 'variant-only', json_object(
                        'source_models', json_array('variant'),
                        'source_stages', json_array(1)), 0),
                    (1, 'invalid-json', 'invalid', 0),
                    (2, 'too-old', json_object(
                        'source_models', json_array('main'),
                        'source_stages', json_array(1)), 0),
                    (3, 'accepted', json_object(
                        'source_models', json_array('main'),
                        'source_stages', json_array(1)), 1);",
            )
            .await
            .unwrap();

        let stats = db.get_timeline_stats(None).await.unwrap();

        assert_eq!(
            stats["findings_by_stage"],
            json!([
                {"stage": 1, "unique": 1, "duplicates": 1, "hallucinations": 0, "engagements": 2,
                 "unique_by_severity": {"critical": 0, "high": 0, "medium": 0, "low": 1}},
                {"stage": 2, "unique": 1, "duplicates": 2, "hallucinations": 2, "engagements": 2,
                 "unique_by_severity": {"critical": 0, "high": 0, "medium": 1, "low": 0}},
                {"stage": 3, "unique": 1, "duplicates": 2, "hallucinations": 0, "engagements": 1,
                 "unique_by_severity": {"critical": 0, "high": 1, "medium": 0, "low": 0}},
                {"stage": 4, "unique": 1, "duplicates": 0, "hallucinations": 1, "engagements": 1,
                 "unique_by_severity": {"critical": 1, "high": 0, "medium": 0, "low": 0}},
            ])
        );
    }

    /// A series sent without a cover letter has 1/N as its thread root, so a
    /// thread-fetch placeholder gets created under 1/N's own message-id with a
    /// NULL author. Ingesting 1/N first used to abort on that NULL and silently
    /// discard the patch, leaving the series stuck at Incomplete forever.
    #[tokio::test]
    async fn cover_letterless_series_adopts_placeholder_when_first_patch_leads() {
        let db = setup_db().await;

        // The placeholder the thread-fetch API creates before any message arrives.
        let ps_placeholder = db
            .create_fetching_patchset(
                "root-1@example.com",
                "Fetching thread root-1@example.com...",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let author = "\"Real Author\" <real@example.com>";
        let thread_id = db
            .ensure_thread_for_message("root-1@example.com", 1000)
            .await
            .unwrap();

        // Patch 1/2 is the thread root: no In-Reply-To, so no cover letter to
        // derive a clid from. This is the message that used to be dropped.
        db.create_message(
            "root-1@example.com",
            thread_id,
            None,
            author,
            "[PATCH 1/2] first",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "root-1@example.com",
                "[PATCH 1/2] first",
                author,
                1000,
                2,
                1,
                "to",
                "cc",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .expect("1/2 must not be dropped");
        assert_eq!(
            ps1, ps_placeholder,
            "1/2 should adopt the placeholder created under its own message-id"
        );
        db.create_patch(ps1, "root-1@example.com", 1, "diff --git a/a b/a\n")
            .await
            .unwrap();

        // Patch 2/2 replies to 1/2, so it resolves via the normal cover-letter path.
        db.create_message(
            "root-2@example.com",
            thread_id,
            Some("root-1@example.com"),
            author,
            "[PATCH 2/2] second",
            1001,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps2 = db
            .create_patchset(
                thread_id,
                Some("root-1@example.com"),
                "root-2@example.com",
                "[PATCH 2/2] second",
                author,
                1001,
                2,
                1,
                "to",
                "cc",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .expect("2/2 must resolve");
        assert_eq!(ps2, ps1, "both parts belong to one patchset");
        db.create_patch(ps2, "root-2@example.com", 2, "diff --git a/b b/b\n")
            .await
            .unwrap();

        // Both parts present means the series becomes reviewable rather than
        // sitting at Incomplete with a lost patch.
        let mut rows = db
            .conn
            .query(
                "SELECT received_parts, total_parts, status, author FROM patchsets WHERE id = ?",
                libsql::params![ps1],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let received: u32 = row.get(0).unwrap();
        let total: u32 = row.get(1).unwrap();
        let status: String = row.get(2).unwrap();
        let stored_author: String = row.get(3).unwrap();

        assert_eq!(received, 2, "both parts recorded");
        assert_eq!(total, 2);
        assert_eq!(status, "Pending", "complete series must leave Incomplete");
        assert_eq!(
            stored_author, author,
            "placeholder NULL author is filled in"
        );

        // And exactly one patchset owns the thread — no duplicate split.
        let mut count_rows = db
            .conn
            .query(
                "SELECT COUNT(*) FROM patchsets WHERE thread_id = ?",
                libsql::params![thread_id],
            )
            .await
            .unwrap();
        let n: i64 = count_rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(n, 1, "series must not split into multiple patchsets");
    }

    /// A patch whose cover-letter candidates miss the placeholder falls through
    /// to the author/thread scan. With git send-email --thread=deep, 3/3 replies
    /// to 2/3 rather than to the root, so if it is ingested first the scan is the
    /// only path available and must survive the placeholder's NULL author.
    #[tokio::test]
    async fn patchset_scan_tolerates_placeholder_with_null_author() {
        let db = setup_db().await;

        let ps_placeholder = db
            .create_fetching_patchset(
                "deep-1@example.com",
                "Fetching thread deep-1@example.com...",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let author = "\"Deep Author\" <deep@example.com>";
        let thread_id = db
            .ensure_thread_for_message("deep-1@example.com", 2000)
            .await
            .unwrap();

        // 3/3 arrives first and replies to 2/3, so neither clid candidate matches
        // the placeholder created under 1/3's message-id.
        for (msgid, subject, date) in [
            ("deep-2@example.com", "[PATCH 2/3] second", 2001),
            ("deep-3@example.com", "[PATCH 3/3] third", 2002),
        ] {
            db.create_message(
                msgid, thread_id, None, author, subject, date, "", "", "", None, None,
            )
            .await
            .unwrap();
        }

        let ps3 = db
            .create_patchset(
                thread_id,
                Some("deep-2@example.com"),
                "deep-3@example.com",
                "[PATCH 3/3] third",
                author,
                2002,
                3,
                1,
                "to",
                "cc",
                None,
                3,
                None,
                true,
                None,
                None,
            )
            .await
            .expect("NULL author on a placeholder must not abort the scan")
            .expect("3/3 must not be dropped");

        // The scan does not adopt the placeholder here: an empty author cannot
        // satisfy strict_author matching, so 3/3 correctly starts its own
        // patchset. What matters is that the diff survives at all.
        db.create_patch(ps3, "deep-3@example.com", 3, "diff --git a/c b/c\n")
            .await
            .unwrap();

        let mut rows = db
            .conn
            .query(
                "SELECT COUNT(*) FROM patches WHERE message_id = ?",
                libsql::params!["deep-3@example.com"],
            )
            .await
            .unwrap();
        let saved: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(saved, 1, "3/3's diff must be persisted, not discarded");
        assert_ne!(
            ps3, ps_placeholder,
            "sanity: this path forks rather than adopting, unlike the 1/N case"
        );
    }

    #[tokio::test]
    async fn test_create_multiple_patchsets_in_thread() {
        let db = setup_db().await;

        // Create a thread
        let thread_id = db.create_thread("root", "Test Thread", 1000).await.unwrap();

        // 1. Create first patchset from Patch 1 (index 1)
        db.create_message(
            "msg1", thread_id, None, "Author A", "Patch 1", 1000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg1",
                "Patch 1",
                "Author A",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(ps1.is_some());

        // 2. Add Cover Letter (index 0)
        // Should return same ID and update subject to "Cover Letter"
        db.create_message(
            "root",
            thread_id,
            None,
            "Author A",
            "Cover Letter",
            1005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1_update = db
            .create_patchset(
                thread_id,
                Some("root"),
                "root",
                "Cover Letter",
                "Author A",
                1005,
                2,
                1,
                "to",
                "cc",
                Some(1),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ps1, ps1_update);

        let list = db.get_patchsets(1, 0, None, None, false).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 3. Add Patch 2 (index 2)
        // Should NOT update subject (index 2 > index 0)
        db.create_message(
            "msg2", thread_id, None, "Author A", "Patch 2", 1006, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patchset(
            thread_id,
            None,
            "msg2",
            "Patch 2",
            "Author A",
            1006,
            2,
            1,
            "to",
            "cc",
            Some(1),
            2,
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap();

        let list = db.get_patchsets(1, 0, None, None, false).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 4. Create NEW patchset in same thread (Author B, Time 1000 - same time but diff author)
        // With relaxed logic, this SHOULD merge if total_parts match (assuming same series).
        let ps3 = db
            .create_patchset(
                thread_id,
                None,
                "msg_other",
                "Other Author",
                "Author B",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ps3, ps1, "Different author in same series should merge");

        // 5. Create NEW patchset v2 (Author A, Time 1002 - close time, but v2)
        // Under new logic "Implicit matches Explicit", this SHOULD merge with ps1 (Implicit)
        // because time/author/total match.
        let ps_v2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v2",
                "[PATCH v2] Patchset 1",
                "Author B",
                1002,
                2,
                1,
                "to",
                "cc",
                Some(2),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert_ne!(
            ps1, ps_v2,
            "Implicit v1 should NOT merge with v2 even if time/author match"
        );

        // 7. Test Merging: Create disjoint patchsets then bridge them
        let t_merge = db
            .create_thread("root_merge", "Merge Test", 10000)
            .await
            .unwrap();

        // PS A (Time 10000)
        db.create_message(
            "m1", t_merge, None, "Merger", "P1", 10000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psa = db
            .create_patchset(
                t_merge,
                None,
                "m1",
                "Series",
                "Merger",
                10000,
                3,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // PS B (Time 200000) - 190000s diff > 86400s limit -> New PS
        db.create_message(
            "m2", t_merge, None, "Merger", "P3", 200000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psb = db
            .create_patchset(
                t_merge,
                None,
                "m2",
                "Series",
                "Merger",
                200000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(psa, psb);

        // PS C (Time 100000) - 90000s diff from A (>86400), 100000s diff from B (>86400)
        // Wait, if C is > 86400 from both, it won't match either!
        // We need C to match BOTH.
        // A=10000. B=200000. Gap=190000.
        // If we want C to bridge, C needs to be within 86400 of A AND within 86400 of B.
        // But 190000 > 86400 * 2 (172800).
        // So it's IMPOSSIBLE to bridge with ONE message if they are that far apart!
        // We need A and B to be < 2 * 86400 apart.
        // Let's set B = 10000 + 100000 = 110000.
        // Diff = 100000. > 86400. So disjoint.
        // C = 10000 + 50000 = 60000.
        // Diff(A, C) = 50000 < 86400. Match A.
        // Diff(B, C) = 110000 - 60000 = 50000 < 86400. Match B.
        // So C bridges A and B.

        db.create_message(
            "m2_fixed", t_merge, None, "Merger", "P3_fixed", 120000, "", "", "", None, None,
        )
        .await
        .unwrap(); // 120000. Diff 110000 > 86400.
        let psb_fixed = db
            .create_patchset(
                t_merge,
                None,
                "m2_fixed",
                "Series",
                "Merger",
                120000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(psa, psb_fixed);

        // PS C (Time 65000)
        // Diff(A, C) = 55000 < 86400.
        // Diff(B, C) = 120000 - 65000 = 55000 < 86400.
        db.create_message(
            "m3", t_merge, None, "Merger", "P2", 65000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psc = db
            .create_patchset(
                t_merge,
                None,
                "m3",
                "Series",
                "Merger",
                65000,
                3,
                1,
                "",
                "",
                Some(1),
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(psc, psa);
    }

    #[tokio::test]
    async fn test_five_patch_series_merging() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_5", "Five Patch Series", 20000)
            .await
            .unwrap();
        let author = "Series Author <author@example.com>";

        // Patches arrive in order: 1/5, 0/5, 2/5, 4/5, 3/5
        let indices = [1, 0, 2, 4, 3];
        let mut patchset_ids = Vec::new();

        for (i, &idx) in indices.iter().enumerate() {
            let msg_id = format!("msg_{}", idx);
            let subject = format!("[PATCH {}/5] Feature part {}", idx, idx);
            let time = 20000 + (i as i64 * 10); // 10s apart

            db.create_message(
                &msg_id, thread_id, None, author, &subject, time, "", "", "", None, None,
            )
            .await
            .unwrap();
            let ps_id = db
                .create_patchset(
                    thread_id,
                    if idx == 0 { Some(&msg_id) } else { None },
                    &msg_id,
                    &subject,
                    author,
                    time,
                    5,
                    1,
                    "to",
                    "cc",
                    None,
                    idx as u32,
                    None,
                    true,
                    None,
                    None,
                )
                .await
                .unwrap()
                .unwrap();

            patchset_ids.push(ps_id);
        }

        // All IDs should be the same
        let first_id = patchset_ids[0];
        for id in patchset_ids {
            assert_eq!(
                id, first_id,
                "All parts of the same series should share the same patchset ID"
            );
        }

        // Verify the final subject is the cover letter (index 0)
        let list = db.get_patchsets(1, 0, None, None, false).await.unwrap();
        assert_eq!(
            list[0].subject.as_deref(),
            Some("[PATCH 0/5] Feature part 0")
        );
    }

    #[tokio::test]
    async fn test_patchset_status_transition() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_status", "Status Test", 60000)
            .await
            .unwrap();
        let author = "Status Author <status@example.com>";

        // 1. Create patchset with 2 parts. received=0 initially (cover letter doesn't count as received part in DB logic usually, but here we insert it)
        // Wait, create_patchset creates the set. create_patch updates received count.
        // We call create_patchset first.
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_status",
                "Status Test",
                author,
                60000,
                2,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Check initial status
        let list = db.get_patchsets(1, 0, None, None, false).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 2. Add Patch 1. received=1. Total=2. Status should be Incomplete.
        db.create_message(
            "msg_1", thread_id, None, author, "Part 1", 60005, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_1", 1, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None, None, false).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 3. Add Patch 2. received=2. Total=2. Status should transition to Pending.
        db.create_message(
            "msg_2", thread_id, None, author, "Part 2", 60010, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_2", 2, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None, None, false).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Pending"));
    }

    #[tokio::test]
    async fn test_embargoed_patchset_dynamic_recalculation() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_embargo", "Embargo Test", 60000)
            .await
            .unwrap();
        let author = "Embargo Author <embargo@example.com>";

        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_embargo",
                "Embargo Test",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        db.create_message(
            "msg_embargo",
            thread_id,
            None,
            author,
            "Embargo Test",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_embargo", 1, "diff")
            .await
            .unwrap();

        db.conn
            .execute(
                "UPDATE patchsets SET status = 'Reviewed' WHERE id = ?",
                libsql::params![ps_id],
            )
            .await
            .unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        db.set_patchset_embargo_until(ps_id, now + 3600)
            .await
            .unwrap();

        let review_id = db
            .create_review(ps_id, None, "gemini", "test-model", None, None)
            .await
            .unwrap();
        db.conn
            .execute(
                "UPDATE reviews SET status = 'Reviewed', summary = 'private review', budget_flags = 0x42 WHERE id = ?",
                libsql::params![review_id],
            )
            .await
            .unwrap();

        let hidden = db
            .get_review_details(review_id, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hidden["status"], "Embargoed");
        assert!(hidden.get("summary").is_none());

        let visible = db
            .get_review_details(review_id, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(visible["summary"], "private review");
        assert_eq!(visible["budget_flags"], 0x42);

        let patchsets = db.get_patchsets(10, 0, None, None, false).await.unwrap();
        assert_eq!(patchsets[0].status.as_deref(), Some("Embargoed"));
        let details = db
            .get_patchset_details(ps_id, None, None, false)
            .await
            .unwrap()
            .unwrap();
        assert!(
            details
                .get("reviews")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );

        db.set_patchset_embargo_until(ps_id, now - 3600)
            .await
            .unwrap();

        let patchsets = db.get_patchsets(10, 0, None, None, false).await.unwrap();
        assert_eq!(patchsets[0].status.as_deref(), Some("Reviewed"));
        let details = db
            .get_patchset_details(ps_id, None, None, false)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !details
                .get("reviews")
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_clean_patchset_is_releasable_before_embargo_expiry() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_clean_embargo", "Clean Embargo", 70000)
            .await
            .unwrap();
        db.create_message(
            "msg_clean_embargo",
            thread_id,
            None,
            "Author <author@example.com>",
            "Clean Embargo",
            70000,
            "body",
            "list@example.com",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_clean_embargo",
                "Clean Embargo",
                "Author <author@example.com>",
                70000,
                1,
                1,
                "list@example.com",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db
            .create_patch(ps_id, "msg_clean_embargo", 1, "diff")
            .await
            .unwrap();
        let review_id = db
            .create_review(ps_id, Some(patch_id), "test", "test", None, None)
            .await
            .unwrap();
        db.complete_review(
            review_id,
            "Reviewed",
            "Review completed successfully.",
            Some("clean"),
            None,
            Some("No issues found."),
            None,
            None,
        )
        .await
        .unwrap();
        db.update_patchset_status(ps_id, "Reviewed").await.unwrap();

        let now = chrono::Utc::now().timestamp();
        db.set_patchset_embargo_until(ps_id, now + 3600)
            .await
            .unwrap();

        assert_eq!(
            db.get_patchset_review_outcome(ps_id).await.unwrap(),
            PatchsetReviewOutcome::Clean
        );
        let releasable = db
            .get_releasable_embargoed_patchsets(now, 10)
            .await
            .unwrap();
        assert!(releasable.iter().any(|patchset| patchset.id == ps_id));

        assert!(db.claim_patchset_embargo_release(ps_id, now).await.unwrap());
        assert!(!db.claim_patchset_embargo_release(ps_id, now).await.unwrap());
        assert!(
            db.claim_patchset_embargo_release(ps_id, now + 601)
                .await
                .unwrap()
        );
        db.clear_patchset_embargo_release_claim(ps_id)
            .await
            .unwrap();

        db.update_patchset_status(ps_id, "Pending").await.unwrap();
        assert!(
            !db.claim_patchset_embargo_release(ps_id, now + 601)
                .await
                .unwrap()
        );
        db.update_patchset_status(ps_id, "Reviewed").await.unwrap();

        db.create_finding(Finding {
            review_id,
            severity: Severity::Low,
            severity_explanation: None,
            problem: "Pre-existing issue".to_string(),
            preexisting: Some(true),
            locations: None,
            source_stages: None,
        })
        .await
        .unwrap();

        assert_eq!(
            db.get_patchset_review_outcome(ps_id).await.unwrap(),
            PatchsetReviewOutcome::HasFindings
        );
        let releasable = db
            .get_releasable_embargoed_patchsets(now, 10)
            .await
            .unwrap();
        assert!(!releasable.iter().any(|patchset| patchset.id == ps_id));
    }

    #[tokio::test]
    async fn test_no_ai_review_is_not_clean() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_no_ai_embargo", "No AI Embargo", 71000)
            .await
            .unwrap();
        db.create_message(
            "msg_no_ai_embargo",
            thread_id,
            None,
            "Author <author@example.com>",
            "No AI Embargo",
            71000,
            "body",
            "list@example.com",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_no_ai_embargo",
                "No AI Embargo",
                "Author <author@example.com>",
                71000,
                1,
                1,
                "list@example.com",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db
            .create_patch(ps_id, "msg_no_ai_embargo", 1, "diff")
            .await
            .unwrap();
        let review_id = db
            .create_review(ps_id, Some(patch_id), "test", "test", None, None)
            .await
            .unwrap();
        db.complete_review(
            review_id,
            "Skipped",
            "Skipped AI review via --no-ai",
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.update_patchset_status(ps_id, "Reviewed").await.unwrap();

        assert_eq!(
            db.get_patchset_review_outcome(ps_id).await.unwrap(),
            PatchsetReviewOutcome::Incomplete
        );

        let now = chrono::Utc::now().timestamp();
        db.set_patchset_embargo_until(ps_id, now + 3600)
            .await
            .unwrap();
        let releasable = db.get_releasable_embargoed_patchsets(now, 1).await.unwrap();
        assert!(releasable.iter().all(|patchset| patchset.id != ps_id));
    }

    #[tokio::test]
    async fn test_implicit_version_mismatch_should_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_v6", "Version 6 Series", 30000)
            .await
            .unwrap();
        let author = "Author V6 <v6@example.com>";

        // Case: Cover letter has v6, but patches don't say v6 (implicitly v1).
        // If the user forgot to version patches, they should NOT merge with strict version checking.
        // This prevents merging v1 patches into v6 series if timestamps overlap.

        // 1. Cover letter: [PATCH 00/33 v6] -> v6
        db.create_message(
            "msg_00",
            thread_id,
            None,
            author,
            "[PATCH 00/33 v6] Cover",
            30000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_cover = db
            .create_patchset(
                thread_id,
                Some("msg_00"),
                "msg_00",
                "[PATCH 00/33 v6] Cover",
                author,
                30000,
                33,
                1,
                "",
                "",
                Some(6),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Patch 1: [PATCH 01/33] -> v1 (implicit). Pass None.
        db.create_message(
            "msg_01",
            thread_id,
            None,
            author,
            "[PATCH 01/33] Part 1",
            30005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_p1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_01",
                "[PATCH 01/33] Part 1",
                author,
                30005,
                33,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Relaxed checking: Should merge because same thread
        assert_eq!(
            ps_cover, ps_p1,
            "Should merge explicit v6 cover with implicit v1 patch if in same thread"
        );
    }

    #[tokio::test]
    async fn test_unrelated_singletons_no_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_single", "Singletons", 60000)
            .await
            .unwrap();
        let author = "Author S <s@example.com>";

        // Patch A
        db.create_message(
            "msg_a",
            thread_id,
            None,
            author,
            "[PATCH] Fix A",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_a = db
            .create_patchset(
                thread_id,
                None,
                "msg_a",
                "[PATCH] Fix A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patch B (Close time, same author, implicit version, total=1)
        db.create_message(
            "msg_b",
            thread_id,
            None,
            author,
            "[PATCH] Fix B",
            60005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_b = db
            .create_patchset(
                thread_id,
                None,
                "msg_b",
                "[PATCH] Fix B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps_a, ps_b,
            "Should NOT merge unrelated singletons even if author/time match"
        );
    }

    #[tokio::test]
    async fn test_singleton_cover_patch_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_1of1", "Singleton Series", 60000)
            .await
            .unwrap();
        let author = "Author 1of1 <1@example.com>";

        // Cover: [PATCH 0/1] Subject A
        db.create_message(
            "msg_0",
            thread_id,
            None,
            author,
            "[PATCH 0/1] Subject A",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_0 = db
            .create_patchset(
                thread_id,
                Some("msg_0"),
                "msg_0",
                "[PATCH 0/1] Subject A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patch: [PATCH 1/1] Subject B (Different subject)
        db.create_message(
            "msg_1",
            thread_id,
            None,
            author,
            "[PATCH 1/1] Subject B",
            60005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_1",
                "[PATCH 1/1] Subject B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps_0, ps_1,
            "Should merge 0/1 and 1/1 even if subjects differ"
        );
    }

    #[tokio::test]
    async fn test_version_mismatch_no_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_diff_ver", "Version Mismatch", 40000)
            .await
            .unwrap();
        let author = "Author Diff <diff@example.com>";

        // v5
        db.create_message(
            "msg_v5",
            thread_id,
            None,
            author,
            "[PATCH v5 1/2] Part 1",
            40000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_v5 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v5",
                "[PATCH v5 1/2] Part 1",
                author,
                40000,
                2,
                1,
                "",
                "",
                Some(5),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Add patch to trigger index collision logic
        db.create_patch(ps_v5, "msg_v5", 1, "diff").await.unwrap();

        // v6 (Close time)
        db.create_message(
            "msg_v6",
            thread_id,
            None,
            author,
            "[PATCH v6 1/2] Part 1",
            40010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_v6 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v6",
                "[PATCH v6 1/2] Part 1",
                author,
                40010,
                2,
                1,
                "",
                "",
                Some(6),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps_v5, ps_v6,
            "Should NOT merge different explicit versions (v5 vs v6)"
        );
    }

    #[tokio::test]
    async fn test_v3_series_fragmentation() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_v3", "v3 Series", 50000)
            .await
            .unwrap();
        let author = "Author V3 <v3@example.com>";

        // 1. [PATCH v3 0/2] (Cover)
        db.create_message(
            "v3_0",
            thread_id,
            None,
            author,
            "[PATCH v3 0/2] Cover",
            50000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_0 = db
            .create_patchset(
                thread_id,
                Some("v3_0"),
                "v3_0",
                "[PATCH v3 0/2] Cover",
                author,
                50000,
                2,
                1,
                "",
                "",
                Some(3),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. [PATCH v3 1/2]
        db.create_message(
            "v3_1",
            thread_id,
            None,
            author,
            "[PATCH v3 1/2] Part 1",
            50005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_1 = db
            .create_patchset(
                thread_id,
                None,
                "v3_1",
                "[PATCH v3 1/2] Part 1",
                author,
                50005,
                2,
                1,
                "",
                "",
                Some(3),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. [PATCH v3 2/2]
        db.create_message(
            "v3_2",
            thread_id,
            None,
            author,
            "[PATCH v3 2/2] Part 2",
            50010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_2 = db
            .create_patchset(
                thread_id,
                None,
                "v3_2",
                "[PATCH v3 2/2] Part 2",
                author,
                50010,
                2,
                1,
                "",
                "",
                Some(3),
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(ps_0, ps_1, "Patch 1 should merge with Cover");
        assert_eq!(ps_0, ps_2, "Patch 2 should merge with Cover");
    }

    #[tokio::test]
    async fn test_merge_with_confusing_version_in_subject() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_confusing", "Confusing Versions", 80000)
            .await
            .unwrap();
        let author = "Confused Author <confused@example.com>";

        // 1. [PATCH v3 00/17] (v3)
        db.create_message(
            "msg_v3_conf_00",
            thread_id,
            None,
            author,
            "[PATCH v3 00/17] Cover",
            80000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_cover = db
            .create_patchset(
                thread_id,
                Some("msg_v3_conf_00"),
                "msg_v3_conf_00",
                "[PATCH v3 00/17] Cover",
                author,
                80000,
                17,
                1,
                "",
                "",
                Some(3),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. [PATCH 01/17] Support v2 hardware. Treat as implicit version (None), NOT v2.
        db.create_message(
            "msg_conf_01",
            thread_id,
            None,
            author,
            "[PATCH 01/17] Support v2 hardware",
            80005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        // Here we simulate the parser extracting "2" from "v2" if it's aggressive
        // But `create_patchset` takes the *parsed* version.
        // If we want to simulate the BUG, we must pass what `parse_email` WOULD pass.
        // `parse_email` uses `parse_subject_version`.
        // Let's check what `parse_subject_version` does for this string.
        let subject = "[PATCH v3 01/17] Support v2 hardware";
        let parsed_ver = crate::patch::parse_subject_version(subject);

        let ps_part1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_conf_01",
                subject,
                author,
                80005,
                17,
                1,
                "",
                "",
                parsed_ver, // Pass the result of the potentially buggy parser
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps_cover, ps_part1,
            "Should merge because subject implies v3 (and ignores v2 in text)"
        );
    }

    #[tokio::test]
    async fn test_merge_patchsets_with_dependencies() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_deps", "Dependencies Test", 90000)
            .await
            .unwrap();
        let author = "Deps Author <deps@example.com>";

        // 1. Create first patchset part [PATCH 1/2]
        db.create_message(
            "msg_deps_1",
            thread_id,
            None,
            author,
            "[PATCH 1/2] Part 1",
            90000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_deps_1",
                "[PATCH 1/2] Part 1",
                author,
                90000,
                2,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Add dependencies to ps1 (Review, Tag, Subsystem)
        let review_id = db
            .create_review(ps1, None, "gemini", "test-model", None, None)
            .await
            .unwrap();

        let sub_id = db
            .ensure_subsystem("test_sub", "test@example.com")
            .await
            .unwrap();
        db.add_subsystem_to_patchset(ps1, sub_id).await.unwrap();

        // 3. Create second patchset part [PATCH 2/2] -> Should merge into ps1 (or ps1 into ps2, but we keep oldest ID so ps2 into ps1)
        // ps1 ID should be preserved because it was created first.
        db.create_message(
            "msg_deps_2",
            thread_id,
            None,
            author,
            "[PATCH 2/2] Part 2",
            90005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_deps_2",
                "[PATCH 2/2] Part 2",
                author,
                90005, // Close enough
                2,
                1,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(ps1, ps2, "Patchsets should have merged");

        // 4. Verify dependencies moved
        // Check review
        let mut rows = db
            .conn
            .query(
                "SELECT patchset_id FROM reviews WHERE id = ?",
                libsql::params![review_id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let review_ps_id: i64 = row.get(0).unwrap();
        assert_eq!(review_ps_id, ps1);

        // Check subsystem
        let mut rows = db
            .conn
            .query(
                "SELECT count(*) FROM patchsets_subsystems WHERE patchset_id = ?",
                libsql::params![ps1],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let count: i64 = row.get(0).unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_create_ai_interaction_with_cached_tokens() {
        let db = setup_db().await;

        // Create interaction
        let params = AiInteractionParams {
            id: "test_id",
            parent_id: None,
            workflow_id: None,
            provider: "test_provider",
            model: "test_model",
            input: "input",
            output: "output",
            tokens_in: 100,
            tokens_out: 50,
            tokens_cached: 25,
        };

        db.create_ai_interaction(params).await.unwrap();

        // Verify via raw query since there is no direct get_ai_interaction method exposed
        // (get_review_details joins it, but requires a review)

        let mut rows = db
            .conn
            .query(
                "SELECT tokens_cached FROM ai_interactions WHERE id = 'test_id'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let cached: u32 = row.get(0).unwrap();

        assert_eq!(cached, 25);
    }

    #[tokio::test]
    async fn test_has_failed_review_logic() {
        let db = setup_db().await;

        // Setup patchset
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
        db.create_message(
            "msg1", thread_id, None, "Author", "Subject", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                Some("msg1"),
                "msg1",
                "Subject",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db.create_patch(ps_id, "msg1", 1, "diff").await.unwrap();

        // 1. Initial State: No reviews
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 2. Failed Review (No interaction) -> Should be detected
        let review_id = db
            .create_review(ps_id, Some(patch_id), "gemini", "test-model", None, None)
            .await
            .unwrap();
        db.update_review_status(review_id, "FailedToApply", None)
            .await
            .unwrap();

        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 3. Status "Failed" (No interaction) -> Should be detected
        db.update_review_status(review_id, "Failed", None)
            .await
            .unwrap();
        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 4. Status "Reviewed" (Success) -> Should NOT be detected
        db.update_review_status(review_id, "Reviewed", None)
            .await
            .unwrap();
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 5. Status "Failed" WITH interaction_id -> Should NOT be detected (reached AI)
        // Revert to Failed first
        db.update_review_status(review_id, "Failed", None)
            .await
            .unwrap();

        // Create interaction first to satisfy FK
        db.create_ai_interaction(AiInteractionParams {
            id: "int_id",
            parent_id: None,
            workflow_id: None,
            provider: "p",
            model: "m",
            input: "",
            output: "",
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        })
        .await
        .unwrap();

        // Set interaction_id
        db.complete_review(
            review_id,
            "Failed",
            "desc",
            None,
            Some("int_id"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());
    }

    #[tokio::test]
    async fn test_rerun_patchset_logic() {
        let db = setup_db().await;

        // Setup patchset
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();

        // Create messages to satisfy FK constraints
        db.create_message(
            "msg_cl1", thread_id, None, "Author", "Cover 1", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_message(
            "msg_p1",
            thread_id,
            Some("msg_cl1"),
            "Author",
            "Patch 1",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        db.create_message(
            "msg_cl2", thread_id, None, "Author", "Cover 2", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_message(
            "msg_p2",
            thread_id,
            Some("msg_cl2"),
            "Author",
            "Patch 2",
            100,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        // Create a patchset that is "Reviewed"
        let ps_reviewed = db
            .create_patchset(
                thread_id,
                Some("msg_cl1"),
                "msg_cl1",
                "Subject Reviewed",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.update_patchset_status(ps_reviewed, "Reviewed")
            .await
            .unwrap();

        // Create a patchset that is "Failed"
        let ps_failed = db
            .create_patchset(
                thread_id,
                Some("msg_cl2"),
                "msg_cl2",
                "Subject Failed",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.update_patchset_status(ps_failed, "Failed")
            .await
            .unwrap();

        // Add a patch to ps_failed
        let patch_id = db
            .create_patch(ps_failed, "msg_p2", 1, "diff")
            .await
            .unwrap();

        // Add a failed review without interaction (infra failure) to ps_failed
        let review_infra = db
            .create_review(ps_failed, Some(patch_id), "p", "m", None, None)
            .await
            .unwrap();
        db.update_review_status(review_infra, "FailedToApply", None)
            .await
            .unwrap();

        // Add a failed review WITH interaction (AI failure) to ps_failed
        let review_ai = db
            .create_review(ps_failed, Some(patch_id), "p", "m", None, None)
            .await
            .unwrap();
        db.create_ai_interaction(AiInteractionParams {
            id: "int_id2",
            parent_id: None,
            workflow_id: None,
            provider: "p",
            model: "m",
            input: "",
            output: "",
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        })
        .await
        .unwrap();
        db.complete_review(
            review_ai,
            "Failed",
            "desc",
            None,
            Some("int_id2"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // Verify initial target counts (should be 1)
        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_reviewed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 1);

        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_failed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 1);

        // Verify blocking review is present
        assert!(
            db.has_failed_review(ps_failed, patch_id, None)
                .await
                .unwrap()
        );

        // RERUN Reviewed patchset -> Should increment target count
        db.rerun_patchset(ps_reviewed).await.unwrap();
        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_reviewed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 2);

        // RERUN Failed patchset -> Should NOT increment target count
        db.rerun_patchset(ps_failed).await.unwrap();
        let mut rows = db
            .conn
            .query(
                "SELECT target_review_count FROM patchsets WHERE id = ?",
                libsql::params![ps_failed],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let target: i64 = row.get(0).unwrap();
        assert_eq!(target, 1);

        // Verify blocking review was deleted
        let mut rows = db
            .conn
            .query(
                "SELECT 1 FROM reviews WHERE id = ?",
                libsql::params![review_infra],
            )
            .await
            .unwrap();
        assert!(rows.next().await.unwrap().is_none());

        // Verify blocking review is NO LONGER blocking
        assert!(
            !db.has_failed_review(ps_failed, patch_id, None)
                .await
                .unwrap()
        );

        // Verify AI failure review is NOT cancelled (remains Failed)
        let mut rows = db
            .conn
            .query(
                "SELECT status FROM reviews WHERE id = ?",
                libsql::params![review_ai],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let status: String = row.get(0).unwrap();
        assert_eq!(status, "Failed");
    }

    #[tokio::test]
    async fn test_cross_thread_no_merge() {
        let db = setup_db().await;

        // 1. Create Thread A and Patchset A (1/2)
        let t1 = db
            .create_thread("root1", "Subject 1/2", 1000)
            .await
            .unwrap();
        db.create_message(
            "msg1",
            t1,
            None,
            "Author",
            "[PATCH 1/2] Series",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                t1,
                None,
                "msg1",
                "[PATCH 1/2] Series",
                "Author",
                1000,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Create Thread B and Patchset B (2/2) - Same Author, Close Time, Different Thread
        let t2 = db
            .create_thread("root2", "Subject 2/2", 1005)
            .await
            .unwrap(); // 5 seconds later
        db.create_message(
            "msg2",
            t2,
            None,
            "Author",
            "[PATCH 2/2] Series",
            1005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps2 = db
            .create_patchset(
                t2,
                None,
                "msg2",
                "[PATCH 2/2] Series",
                "Author",
                1005,
                2,
                0,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. Assert they DID NOT merge (ps2 should NOT equal ps1)
        assert_ne!(
            ps1, ps2,
            "Patchsets from different threads should NOT merge even if author/time match"
        );

        // 4. Verify total patches count or received parts
        db.create_patch(ps1, "msg1", 1, "").await.unwrap();
        db.create_patch(ps2, "msg2", 2, "").await.unwrap();

        let details1 = db
            .get_patchset_details(ps1, None, None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details1["received_parts"], 1);
        let details2 = db
            .get_patchset_details(ps2, None, None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details2["received_parts"], 1);
    }

    #[tokio::test]
    async fn test_duplicate_ingestion_on_full_patchset() {
        let db = setup_db().await;

        // 1. Create Patchset (1/1)
        let t1 = db.create_thread("root1", "Subject", 1000).await.unwrap();
        let msg_id = "msg1";

        db.create_message(
            msg_id,
            t1,
            None,
            "Author",
            "[PATCH 1/1] Subject",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps1 = db
            .create_patchset(
                t1,
                None,
                msg_id,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Add patch so it becomes full
        db.create_patch(ps1, msg_id, 1, "diff").await.unwrap();

        let details = db
            .get_patchset_details(ps1, None, None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details["received_parts"], 1);
        assert_eq!(details["total_parts"], 1);

        // 3. Try to ingest the SAME patch again
        // It matches the existing patchset (Author/Time/Thread).
        // It IS full (1/1).
        // But it IS a duplicate (msg_id matches).
        // So it SHOULD merge.
        let ps2 = db
            .create_patchset(
                t1,
                None,
                msg_id,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps1, ps2,
            "Should merge duplicate into existing patchset even if full"
        );

        // 4. Try to ingest a NEW patch (different ID) that looks like it belongs
        // This simulates a collision or a separate series with same metadata.
        // It should NOT merge because the set is full and it's NOT a duplicate.
        let msg_id_new = "msg_new";
        db.create_message(
            msg_id_new,
            t1,
            None,
            "Author",
            "[PATCH 1/1] Subject",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps3 = db
            .create_patchset(
                t1,
                None,
                msg_id_new,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps1, ps3,
            "Should create NEW patchset for non-duplicate when full"
        );
    }

    #[tokio::test]
    async fn test_mailing_list_filtering() {
        let db = setup_db().await;

        // 1. Setup lists
        db.ensure_mailing_list("List A", "list-a").await.unwrap();
        db.ensure_mailing_list("List B", "list-b").await.unwrap();
        let id_a = db
            .get_mailing_list_id_by_name("list-a")
            .await
            .unwrap()
            .unwrap();
        let id_b = db
            .get_mailing_list_id_by_name("list-b")
            .await
            .unwrap()
            .unwrap();

        // 2. Create threads
        let t_a = db.create_thread("root_a", "Subject A", 100).await.unwrap();
        let t_b = db.create_thread("root_b", "Subject B", 100).await.unwrap();

        // 3. Create Message A (in List A)
        db.create_message(
            "msg_a",
            t_a,
            None,
            "Author",
            "Subject A",
            100,
            "",
            "",
            "",
            None,
            Some("list-a"),
        )
        .await
        .unwrap();
        let msg_a_id = db.get_message_id_by_msg_id("msg_a").await.unwrap().unwrap();
        db.add_message_to_mailing_list(msg_a_id, id_a)
            .await
            .unwrap();

        // 4. Create Message B (in List B)
        db.create_message(
            "msg_b",
            t_b,
            None,
            "Author",
            "Subject B",
            100,
            "",
            "",
            "",
            None,
            Some("list-b"),
        )
        .await
        .unwrap();
        let msg_b_id = db.get_message_id_by_msg_id("msg_b").await.unwrap().unwrap();
        db.add_message_to_mailing_list(msg_b_id, id_b)
            .await
            .unwrap();

        // 5. Create Patchsets
        // Patchset A linked to msg_a (as cover letter)
        let ps_a = db
            .create_patchset(
                t_a,
                Some("msg_a"),
                "msg_a",
                "Subject A",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patchset B linked to msg_b (as cover letter)
        let ps_b = db
            .create_patchset(
                t_b,
                Some("msg_b"),
                "msg_b",
                "Subject B",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 6. Test filtering messages
        let msgs_a = db
            .get_messages(10, 0, None, Some("list-a".to_string()))
            .await
            .unwrap();
        assert_eq!(msgs_a.len(), 1);
        assert_eq!(msgs_a[0].message_id, "msg_a");

        let msgs_b = db
            .get_messages(10, 0, None, Some("list-b".to_string()))
            .await
            .unwrap();
        assert_eq!(msgs_b.len(), 1);
        assert_eq!(msgs_b[0].message_id, "msg_b");

        // 7. Add patch to ps_a to make it pass the CURRENT logic (patches only)
        // db.create_message(
        //     "patch_a_1", t_a, None, "Author", "Patch A 1", 101, "", "", "", None, Some("list-a")
        // ).await.unwrap();
        // let p_a_1_id = db.get_message_id_by_msg_id("patch_a_1").await.unwrap().unwrap();
        // db.add_message_to_mailing_list(p_a_1_id, id_a).await.unwrap();
        // db.create_patch(ps_a, "patch_a_1", 1, "").await.unwrap();

        // Now ps_a has a patch in list-a.
        // UPDATE: We commented out the patch creation above.
        // ps_a only has a cover letter in list-a.
        // The UNION query should find it.
        let psets_a = db
            .get_patchsets(10, 0, None, Some("list-a".to_string()), false)
            .await
            .unwrap();
        assert_eq!(psets_a.len(), 1);
        assert_eq!(psets_a[0].id, ps_a);

        let psets_b = db
            .get_patchsets(10, 0, None, Some("list-a".to_string()), false)
            .await
            .unwrap();
        let found_b = psets_b.iter().any(|p| p.id == ps_b);
        assert!(!found_b);
    }

    #[tokio::test]
    async fn test_tool_usages_telemetry() {
        let db = setup_db().await;

        let thread_id = db.create_thread("root", "Test Thread", 1000).await.unwrap();
        db.create_message(
            "msg1", thread_id, None, "Author", "Subject", 1000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "msg1", "Subject", "Author", 1000, 1, 1, "", "", None, 1, None,
                true, None, None,
            )
            .await
            .unwrap()
            .unwrap();

        let review_id = db
            .create_review(ps_id, None, "gemini", "test-model", None, None)
            .await
            .unwrap();

        db.create_tool_usage(ToolUsage {
            review_id,
            provider: "test_prov".to_string(),
            model: "test_model".to_string(),
            tool_name: "git_grep".to_string(),
            arguments: Some("{\"pattern\":\"gup_fast\"}".to_string()),
            output_length: 0,
        })
        .await
        .unwrap();

        db.update_tool_usage_length(review_id, "git_grep", "{\"pattern\":\"gup_fast\"}", 456)
            .await
            .unwrap();

        let stmt = db
            .conn
            .prepare("SELECT output_length FROM tool_usages WHERE review_id = ?")
            .await
            .unwrap();
        let mut rows = stmt.query(libsql::params![review_id]).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let length: i64 = row.get(0).unwrap();
        assert_eq!(length, 456);
    }

    #[tokio::test]
    async fn test_message_references_header_storage() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root", "References Thread", 1000)
            .await
            .unwrap();

        db.create_message_with_references(
            "msg1",
            thread_id,
            None,
            "Author",
            "Subject 1",
            1000,
            "",
            "",
            "",
            None,
            None,
            None,
        )
        .await
        .unwrap();

        db.create_message_with_references(
            "msg2",
            thread_id,
            Some("msg1"),
            "Author",
            "Subject 2",
            1001,
            "",
            "",
            "",
            None,
            None,
            Some("msg1"),
        )
        .await
        .unwrap();

        db.create_message_with_references(
            "msg3",
            thread_id,
            Some("msg2"),
            "Author",
            "Subject 3",
            1002,
            "",
            "",
            "",
            None,
            None,
            Some("msg1 msg2"),
        )
        .await
        .unwrap();

        let msg3 = db
            .get_message_details_by_msgid("msg3")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg3.references_hdr.as_deref(), Some("msg1 msg2"));

        let msg2 = db
            .get_message_details_by_msgid("msg2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg2.references_hdr.as_deref(), Some("msg1"));

        let msg1 = db
            .get_message_details_by_msgid("msg1")
            .await
            .unwrap()
            .unwrap();
        assert!(msg1.references_hdr.is_none());
    }

    #[tokio::test]
    async fn test_merge_b4_relay_alias_with_real_author() {
        let db = setup_db().await;

        // 1. Create Thread
        let thread_id = db
            .create_thread("root_b4_merge", "Subject", 1000)
            .await
            .unwrap();

        // 2. Create Patchset Part 1 (devnull alias)
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_b4_1",
                "[PATCH 1/2] B4 Merge Series",
                "devnull+author.example.com@kernel.org",
                1000,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. Create Patchset Part 2 (real email address)
        let ps2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_b4_2",
                "[PATCH 2/2] B4 Merge Series",
                "Real Author <author@example.com>",
                1010,
                2,
                0,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 4. Assert they merged (ps1 == ps2)
        assert_eq!(
            ps1, ps2,
            "Patchset from B4 Relay devnull alias and real author email MUST merge"
        );
    }

    #[tokio::test]
    async fn cost_stats_include_all_supplemental_llm_usage() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO patchsets (id, thread_id) VALUES (1, 1);
                 INSERT INTO subsystems (id, name, mailing_list_address)
                    VALUES (1, 'networking', 'net@example.com');
                 INSERT INTO patchsets_subsystems (patchset_id, subsystem_id) VALUES (1, 1);
                 INSERT INTO ai_interactions
                    (id, provider, model, tokens_in, tokens_out, tokens_cached, created_at)
                    VALUES ('main-interaction', 'openai', 'main-model', 100, 20, 10, 1000);
                 INSERT INTO reviews
                    (id, patchset_id, interaction_id, status, created_at, provider, model)
                    VALUES (99, 1, 'main-interaction', 'Reviewed', 1000, 'openai', 'main-model');
                 INSERT INTO cross_review_jobs
                    (id, patchset_id, source_name, source_url, local_model, local_provider,
                     lookup_message_id, generation, status, first_attempt_at, next_attempt_at,
                     deadline_at, completed_at, merge_tokens_in, merge_tokens_out,
                     merge_tokens_cached)
                    VALUES (1, 1, 'peer', 'https://peer.example', 'merge-model', 'openai',
                            'lookup', 1, 'complete', 1000, 1000, 2000, 1000, 30, 3, 2);",
            )
            .await
            .unwrap();

        let experiment = json!({
            "cohort": {
                "main": {"name": "main", "provider": "openai", "model": "main-model"},
                "variants": [{
                    "source": {"name": "variant", "provider": "anthropic", "model": "variant-model"},
                    "selected": true
                }]
            },
            "runs": [
                {"model": "main", "provider_id": "openai", "model_id": "main-model", "stage": 1, "status": "completed", "tokens_in": 60, "tokens_out": 6, "tokens_cached": 5},
                {"model": "variant", "provider_id": "anthropic", "model_id": "variant-model", "stage": 1, "status": "completed", "tokens_in": 50, "tokens_out": 5, "tokens_cached": 4},
                {"model": "variant", "provider_id": "anthropic", "model_id": "variant-model", "stage": 2, "status": "completed", "tokens_in": 30, "tokens_out": 3, "tokens_cached": 2}
            ],
            "confirmation_runs": [
                {"model": "main", "provider_id": "openai", "model_id": "main-model", "status": "completed", "tokens_in": 40, "tokens_out": 4, "tokens_cached": 3}
            ]
        });
        db.save_model_experiment(99, &experiment, &json!([]))
            .await
            .unwrap();

        let stats = db.get_cost_stats(Some(1)).await.unwrap();
        let models = stats["daily"][0]["by_model"].as_array().unwrap();
        let usage = |model: &str| models.iter().find(|row| row["model"] == model).unwrap();

        let main = usage("main-model");
        assert_eq!(main["tokens_in"], 140);
        assert_eq!(main["tokens_out"], 24);
        assert_eq!(main["tokens_cached"], 13);

        let variant = usage("variant-model");
        assert_eq!(variant["tokens_in"], 80);
        assert_eq!(variant["tokens_out"], 8);
        assert_eq!(variant["tokens_cached"], 6);

        let cross_review = usage("merge-model");
        assert_eq!(cross_review["tokens_in"], 30);
        assert_eq!(cross_review["tokens_out"], 3);
        assert_eq!(cross_review["tokens_cached"], 2);
    }

    #[tokio::test]
    async fn json_decode_stats_group_by_source_and_outcome() {
        use crate::json_health::{JsonDecodeEvent, JsonDecodeOutcome};
        let temp = tempfile::tempdir().unwrap();
        let settings = DatabaseSettings {
            url: temp.path().join("decode.db").display().to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();

        let now = chrono::Utc::now().timestamp();
        let event = |source: &str, outcome: JsonDecodeOutcome, detail: &str| JsonDecodeEvent {
            source: source.to_string(),
            outcome,
            detail: detail.to_string(),
        };
        db.save_json_decode_events(
            Some(1),
            &[
                event("stage:10", JsonDecodeOutcome::Salvaged, "trailing comma"),
                event("stage:10", JsonDecodeOutcome::Fatal, "expected value"),
                event("confirmation", JsonDecodeOutcome::Fatal, "no object"),
                event("confirmation", JsonDecodeOutcome::Fatal, "no object"),
            ],
            now,
        )
        .await
        .unwrap();
        // Cross-review has no review to attribute to.
        db.save_json_decode_events(
            None,
            &[event(
                "cross-review:dedup",
                JsonDecodeOutcome::RecoveredOnRetry,
                "unterminated",
            )],
            now,
        )
        .await
        .unwrap();
        // Outside the 14-day window, so it must not appear.
        db.save_json_decode_events(
            None,
            &[event("stage:3", JsonDecodeOutcome::Fatal, "ancient")],
            now - 60 * 60 * 24 * 30,
        )
        .await
        .unwrap();

        let stats = db.get_json_decode_stats().await.unwrap();
        let by_source = stats["by_source"].as_array().unwrap();
        assert!(by_source.iter().all(|row| row["source"] != "stage:3"));

        let find = |source: &str| {
            by_source
                .iter()
                .find(|row| row["source"] == source)
                .unwrap_or_else(|| panic!("missing {source}"))
                .clone()
        };
        // Ordered by fatal count, so confirmation leads.
        assert_eq!(by_source[0]["source"], "confirmation");
        assert_eq!(find("confirmation")["fatal"], 2);
        assert_eq!(find("stage:10")["salvaged"], 1);
        assert_eq!(find("stage:10")["fatal"], 1);
        assert_eq!(find("stage:10")["total"], 2);
        assert_eq!(find("cross-review:dedup")["recovered_on_retry"], 1);

        let daily = stats["daily"].as_array().unwrap();
        assert!(daily.iter().all(|row| row["source"] != "stage:3"));
        assert!(daily.iter().any(|row| row["source"] == "stage:10"
            && row["outcome"] == "salvaged"
            && row["count"] == 1));
    }

    #[tokio::test]
    async fn test_model_experiment_stats_use_paired_stage_costs() {
        let temp = tempfile::tempdir().unwrap();
        let settings = DatabaseSettings {
            url: temp.path().join("experiment.db").display().to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&settings).await.unwrap());
        db.migrate().await.unwrap();
        db.conn
            .execute(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root')",
                (),
            )
            .await
            .unwrap();
        db.conn
            .execute("INSERT INTO patchsets (id, thread_id) VALUES (1, 1)", ())
            .await
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO reviews (id, patchset_id, status, created_at) VALUES (99, 1, 'Reviewed', 1000), (100, 1, 'Reviewed', 1000)",
                (),
            )
            .await
            .unwrap();
        let experiment = json!({
            "cohort": {
                "main": {"name": "main", "provider": "openai", "model": "model-a"},
                "variants": [{
                    "source": {"name": "variant", "provider": "claude", "model": "model-b"},
                    "selected": true
                }]
            },
            "runs": [
                {"model": "main", "provider_id": "openai", "model_id": "model-a", "stage": 3, "status": "completed", "tokens_in": 100, "tokens_out": 10, "tokens_cached": 20},
                {"model": "variant", "provider_id": "claude", "model_id": "model-b", "stage": 3, "status": "completed", "tokens_in": 80, "tokens_out": 8, "tokens_cached": 10},
                {"model": "main", "provider_id": "openai", "model_id": "model-a", "stage": 5, "status": "completed", "tokens_in": 60, "tokens_out": 6, "tokens_cached": 10},
                {"model": "variant", "provider_id": "claude", "model_id": "model-b", "stage": 5, "status": "completed", "tokens_in": 40, "tokens_out": 4, "tokens_cached": 5},
                {"model": "variant", "provider_id": "claude", "model_id": "model-b", "stage": 4, "status": "failed", "error": "provider unavailable", "tokens_in": 0, "tokens_out": 0, "tokens_cached": 0}
            ],
            "comparisons": [
                {"additional_model": "variant", "main_provider_id": "openai", "main_model_id": "model-a", "additional_provider_id": "claude", "additional_model_id": "model-b", "finding_id": "f1", "outcome": "main_hallucination", "confirmed_by": "variant"},
                {"additional_model": "variant", "main_provider_id": "openai", "main_model_id": "model-a", "additional_provider_id": "claude", "additional_model_id": "model-b", "finding_id": "f1", "outcome": "main_hallucination", "confirmed_by": "variant"}
            ],
            "confirmation_runs": [
                {"model": "main", "provider_id": "openai", "model_id": "model-a", "status": "completed", "tokens_in": 40, "tokens_out": 4, "tokens_cached": 5, "budget_input": 40, "budget_output": 4, "budget_flags": 1},
                {"model": "variant", "provider_id": "claude", "model_id": "model-b", "status": "failed", "error": "unparseable reply", "tokens_in": 30, "tokens_out": 3, "tokens_cached": 0, "budget_input": 30, "budget_output": 3, "budget_flags": 0}
            ]
        });
        let findings = json!([{"finding_ids": ["f1"], "severity": "High"}]);
        db.save_model_experiment(99, &experiment, &findings)
            .await
            .unwrap();
        let stats = db.get_model_experiment_stats().await.unwrap();
        assert_eq!(stats["outcomes"][0]["severity"], "high");
        assert_eq!(stats["paired_cost"][0]["main"]["model"], "model-a");
        assert_eq!(stats["paired_cost"][0]["main"]["provider"], "openai");
        assert_eq!(stats["outcomes"][0]["additional_provider_id"], "claude");
        assert_eq!(stats["confirmation_outcomes"][0]["confirmed_by"], "variant");
        assert_eq!(
            stats["confirmation_outcomes"][0]["outcome"],
            "main_hallucination"
        );
        assert_eq!(stats["confirmation_outcomes"][0]["severity"], "high");
        assert_eq!(stats["confirmation_outcomes"][0]["count"], 1);
        assert_eq!(stats["paired_cost"][0]["paired_stages"], 2);
        assert_eq!(stats["paired_cost"][0]["compared_patches"], 1);
        assert!(
            stats["confirmation_cost"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["model"] == "model-a" && row["provider"] == "openai")
        );
        let health = stats["confirmation_health"].as_array().unwrap();
        let failed = health
            .iter()
            .find(|row| row["status"] == "failed")
            .expect("failed confirmation run is reported");
        assert_eq!(failed["model"], "variant");
        assert_eq!(failed["count"], 1);
        assert_eq!(failed["sample_error"], "unparseable reply");
        assert!(
            health
                .iter()
                .any(|row| row["status"] == "completed" && row["model"] == "main")
        );
        assert!(
            stats["run_status"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| { row["status"] == "failed" && row["count"] == 1 })
        );
        let mut rows = db
            .conn
            .query(
                "SELECT error FROM model_experiment_runs WHERE status = 'failed'",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next()
                .await
                .unwrap()
                .unwrap()
                .get::<String>(0)
                .unwrap(),
            "provider unavailable"
        );

        let (first, second) = tokio::join!(
            db.save_model_experiment(99, &experiment, &findings),
            db.save_model_experiment(100, &experiment, &findings),
        );
        first.unwrap();
        second.unwrap();

        let mut invalid = experiment.clone();
        invalid["cohort"]["variants"] = json!([{
            "source": {"name": "main", "provider": "other", "model": "duplicate"},
            "selected": true
        }]);
        assert!(
            db.save_model_experiment(99, &invalid, &findings)
                .await
                .is_err()
        );
        let mut rows = db
            .conn
            .query(
                "SELECT count(*) FROM model_experiment_runs WHERE review_id = 99",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            5
        );
    }

    #[tokio::test]
    async fn concurrent_in_memory_experiment_saves_are_serialized() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO patchsets (id, thread_id) VALUES (1, 1);
                 INSERT INTO reviews (id, patchset_id, status, created_at) VALUES (99, 1, 'Reviewed', 1000), (100, 1, 'Reviewed', 1000)",
            )
            .await
            .unwrap();
        let experiment = json!({
            "cohort": {
                "main": {"name": "main", "provider": "openai", "model": "model-a"},
                "variants": []
            },
            "runs": [],
            "comparisons": [],
            "confirmation_runs": []
        });
        let findings = json!([]);

        let (first, second) = tokio::join!(
            db.save_model_experiment(99, &experiment, &findings),
            db.save_model_experiment(100, &experiment, &findings),
        );

        first.unwrap();
        second.unwrap();
    }

    #[tokio::test]
    async fn cross_review_jobs_are_durable_and_lease_claimed() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('cover@example', 1);
                 INSERT INTO patchsets
                    (id, thread_id, cover_letter_message_id, status)
                    VALUES (1, 1, 'cover@example', 'Reviewed');",
            )
            .await
            .unwrap();
        db.enqueue_cross_reviews(
            1,
            &[("peer".to_string(), "https://peer.example".to_string())],
            1000,
        )
        .await
        .unwrap();

        let claimed = db.claim_due_cross_reviews(1000, 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].lookup_message_id, "cover@example");
        assert!(
            db.claim_due_cross_reviews(1001, 10)
                .await
                .unwrap()
                .is_empty()
        );

        db.retry_cross_review(&claimed[0], 1001, "embargoed")
            .await
            .unwrap();
        assert!(
            db.claim_due_cross_reviews(4600, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let reclaimed = db.claim_due_cross_reviews(4601, 10).await.unwrap();
        assert_eq!(reclaimed.len(), 1);
        db.finish_cross_review_job(&reclaimed[0], "complete", 4601, None)
            .await
            .unwrap();

        let mut rows = db
            .conn
            .query(
                "SELECT cross_review_status, cross_reviewed_at FROM patchsets WHERE id = 1",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "complete");
        assert_eq!(row.get::<i64>(1).unwrap(), 4601);
    }

    #[tokio::test]
    async fn cross_review_result_publishes_confirmed_remote_findings() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('patch@example', 1);
                 INSERT INTO patchsets
                    (id, thread_id, cover_letter_message_id, status, provider, model_name)
                    VALUES (1, 1, 'patch@example', 'Reviewed', 'local-provider', 'local-model');
                 INSERT INTO patches (id, patchset_id, message_id, part_index, diff)
                    VALUES (10, 1, 'patch@example', 1,
                    'diff --git a/foo.c b/foo.c
--- a/foo.c
+++ b/foo.c
@@ -1,2 +1,3 @@
 int foo(void)
+	kfree(ring);
 	return 0;
');
                 INSERT INTO ai_interactions (id, input_context, output_raw)
                    VALUES ('interaction', 'prepared context', '{\"review\":{\"findings\":[]}}');
                 INSERT INTO reviews
                    (id, patchset_id, patch_id, interaction_id, status, created_at, inline_review)
                    VALUES (20, 1, 10, 'interaction', 'Reviewed', 1000,
                    '> @@ -1,2 +1,3 @@
> +	kfree(ring);

[ ... ]');",
            )
            .await
            .unwrap();
        db.save_local_canonical_findings(
            20,
            &json!([{"finding_ids": ["local"], "severity": "High"}]),
            &json!([{"finding_ids": ["local"], "severity": "High"}]),
        )
        .await
        .unwrap();
        db.enqueue_cross_reviews(
            1,
            &[("peer".to_string(), "https://peer.example".to_string())],
            1000,
        )
        .await
        .unwrap();
        let job = db
            .claim_due_cross_reviews(1000, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let finding = crate::cross_review::RemoteFinding {
            finding_id: "remote".to_string(),
            patch_message_id: "patch@example".to_string(),
            severity: "Medium".to_string(),
            problem: "remote problem".to_string(),
            reasoning: "confirmed".to_string(),
            locations: json!([]),
        };
        let remote = crate::cross_review::RemoteReviewResult {
            model: "remote-model".to_string(),
            provider: "remote-provider".to_string(),
            payload_hash: "hash".to_string(),
            findings: vec![finding.clone()],
        };
        let analysis = crate::cross_review::CrossReviewAnalysis {
            accepted_remote: vec![finding],
            matched_local: Vec::new(),
            matched_remote: Vec::new(),
            comparisons: vec![crate::cross_review::CrossComparison {
                finding_id: "remote".to_string(),
                matched_finding_id: None,
                outcome: "remote_only".to_string(),
                severity: "Medium".to_string(),
            }],
        };

        let rendered = std::collections::HashMap::from([(
            "remote".to_string(),
            crate::cross_render::RenderedComment {
                comment: "Should this free happen before the reset?".to_string(),
                anchor: Some("+\tkfree(ring);".to_string()),
            },
        )]);
        db.persist_cross_review_result(
            &job,
            &remote,
            &analysis,
            &rendered,
            &crate::cross_review::CrossReviewUsage::default(),
            1100,
        )
        .await
        .unwrap();

        let mut rows = db
            .conn
            .query(
                "SELECT COUNT(*) FROM findings WHERE external_finding_id = 'remote'",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            1
        );
        let mut rows = db
            .conn
            .query(
                "SELECT output_raw FROM ai_interactions WHERE id = 'interaction'",
                (),
            )
            .await
            .unwrap();
        let output: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert!(output.contains("remote problem"));
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["review"]["findings"][0]["confirmed_by"], "main");
        // The display ID in the report has to be the one stored for the UI join.
        assert_eq!(
            output["review"]["findings"][0]["finding_ids"][0],
            "peer-remote"
        );
        let mut rows = db
            .conn
            .query("SELECT inline_review FROM reviews WHERE id = 20", ())
            .await
            .unwrap();
        let inline: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert!(inline.contains("[Finding: peer-remote]"));
        assert!(inline.contains("[Sources: peer]"));
        assert!(inline.contains("Should this free happen before the reset?"));
        // The block lands under the line it is about, not after the whole report.
        let anchor = inline.find("> +\tkfree(ring);").unwrap();
        let block = inline.find("[Severity: Medium]").unwrap();
        assert!(anchor < block && block < inline.find("[ ... ]").unwrap());
        // No internal marker line and no raw content hash in the mail text.
        assert!(!inline.contains("Cross-instance finding from"));
        let stats = db.get_cross_review_stats().await.unwrap();
        assert_eq!(stats["outcomes"][0]["outcome"], "remote_only");
        assert_eq!(stats["outcomes"][0]["local_model"], "local-model");
        db.conn
            .execute(
                "UPDATE patchsets SET model_name = 'new-model', provider = 'new-provider'
                 WHERE id = 1",
                (),
            )
            .await
            .unwrap();
        let stats = db.get_cross_review_stats().await.unwrap();
        assert_eq!(stats["outcomes"][0]["local_model"], "local-model");
        assert_eq!(stats["outcomes"][0]["local_provider"], "local-provider");
    }

    #[tokio::test]
    async fn unrendered_remote_findings_still_publish_anchored_and_tagged() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('patch@example', 1);
                 INSERT INTO patchsets (id, thread_id, cover_letter_message_id, status)
                    VALUES (1, 1, 'patch@example', 'Reviewed');
                 INSERT INTO patches (id, patchset_id, message_id, part_index, diff)
                    VALUES (10, 1, 'patch@example', 1,
                    'diff --git a/foo.c b/foo.c
--- a/foo.c
+++ b/foo.c
@@ -1,2 +1,3 @@
 int foo(void)
+	kfree(ring);
 	return 0;
');
                 INSERT INTO ai_interactions (id, input_context, output_raw)
                    VALUES ('interaction', 'context', '{\"review\":{\"findings\":[]}}');
                 INSERT INTO reviews
                    (id, patchset_id, patch_id, interaction_id, status, created_at, inline_review)
                    VALUES (20, 1, 10, 'interaction', 'Reviewed', 1000, 'No issues found.');",
            )
            .await
            .unwrap();
        db.enqueue_cross_reviews(
            1,
            &[("peer".to_string(), "https://peer.example".to_string())],
            1000,
        )
        .await
        .unwrap();
        let job = db
            .claim_due_cross_reviews(1000, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let finding = crate::cross_review::RemoteFinding {
            finding_id: "abcdef01".to_string(),
            patch_message_id: "patch@example".to_string(),
            severity: "High".to_string(),
            problem: "The ring is freed twice.".to_string(),
            reasoning: "Consequence: the allocator poisons the pool.".to_string(),
            locations: json!([{"file": "foo.c", "line": 2}]),
        };
        let remote = crate::cross_review::RemoteReviewResult {
            model: "remote-model".to_string(),
            provider: "remote-provider".to_string(),
            payload_hash: "hash".to_string(),
            findings: vec![finding.clone()],
        };
        let analysis = crate::cross_review::CrossReviewAnalysis {
            accepted_remote: vec![finding],
            matched_local: Vec::new(),
            matched_remote: Vec::new(),
            comparisons: vec![crate::cross_review::CrossComparison {
                finding_id: "abcdef01".to_string(),
                matched_finding_id: None,
                outcome: "remote_only".to_string(),
                severity: "High".to_string(),
            }],
        };

        // An empty render map is what a failed or budget-starved render call
        // leaves behind; publication must not depend on it.
        db.persist_cross_review_result(
            &job,
            &remote,
            &analysis,
            &std::collections::HashMap::new(),
            &crate::cross_review::CrossReviewUsage::default(),
            1100,
        )
        .await
        .unwrap();

        let mut rows = db
            .conn
            .query("SELECT inline_review FROM reviews WHERE id = 20", ())
            .await
            .unwrap();
        let inline: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        // The report may no longer claim there is nothing to report.
        assert!(!inline.contains("No issues found."));
        assert!(inline.contains("[Finding: peer-abcdef01]"));
        // The whole remote finding is carried over, not just its first sentence.
        assert!(inline.contains("The ring is freed twice."));
        assert!(inline.contains("Consequence: the allocator poisons the pool."));
        // locations alone are enough to quote and anchor the hunk.
        assert!(inline.contains("> @@ -1,2 +1,3 @@"));
        let anchor = inline.find("> +\tkfree(ring);").unwrap();
        assert!(anchor < inline.find("[Severity: High]").unwrap());
    }

    #[tokio::test]
    async fn cross_review_local_matches_preserve_both_sources() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('patch@example', 1);
                 INSERT INTO patchsets
                    (id, thread_id, status, target_review_count, provider, model_name)
                    VALUES (1, 1, 'Reviewed', 1, 'local-provider', 'local-model');
                 INSERT INTO patches (id, patchset_id, message_id, part_index, diff)
                    VALUES (10, 1, 'patch@example', 1, 'diff');
                 INSERT INTO ai_interactions (id, input_context, output_raw)
                    VALUES ('interaction', 'context',
                    '{\"review\":{\"findings\":[{\"problem\":\"local accepted\",\"finding_ids\":[\"local-a\"],\"source_models\":[\"main\"]}]}}');
                 INSERT INTO reviews
                    (id, patchset_id, patch_id, interaction_id, status, created_at)
                    VALUES (20, 1, 10, 'interaction', 'Reviewed', 1000);",
            )
            .await
            .unwrap();
        db.enqueue_cross_reviews(
            1,
            &[("peer".to_string(), "https://peer.example".to_string())],
            1000,
        )
        .await
        .unwrap();
        let job = db
            .claim_due_cross_reviews(1000, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let remote_finding = |id: &str, problem: &str| crate::cross_review::RemoteFinding {
            finding_id: id.to_string(),
            patch_message_id: "patch@example".to_string(),
            severity: "High".to_string(),
            problem: problem.to_string(),
            reasoning: String::new(),
            locations: json!([]),
        };
        let accepted = remote_finding("remote-a", "local accepted");
        let corroborated = remote_finding("remote-r", "local rejected");
        let remote = crate::cross_review::RemoteReviewResult {
            model: "remote-model".to_string(),
            provider: "remote-provider".to_string(),
            payload_hash: "hash".to_string(),
            findings: vec![accepted, corroborated.clone()],
        };
        let analysis = crate::cross_review::CrossReviewAnalysis {
            accepted_remote: vec![corroborated],
            matched_local: vec![
                crate::cross_review::CrossLocalMatch {
                    finding_id: "remote-a".to_string(),
                    local_finding_id: "20:patch@example:local-a".to_string(),
                    local_review_id: 20,
                    local_finding_ids: vec!["local-a".to_string()],
                    local_source_models: vec!["main".to_string()],
                    local_accepted: true,
                },
                crate::cross_review::CrossLocalMatch {
                    finding_id: "remote-r".to_string(),
                    local_finding_id: "20:patch@example:local-r".to_string(),
                    local_review_id: 20,
                    local_finding_ids: vec!["local-r".to_string()],
                    local_source_models: vec!["main".to_string()],
                    local_accepted: false,
                },
            ],
            matched_remote: Vec::new(),
            comparisons: vec![
                crate::cross_review::CrossComparison {
                    finding_id: "remote-a".to_string(),
                    matched_finding_id: Some("20:patch@example:local-a".to_string()),
                    outcome: "both".to_string(),
                    severity: "High".to_string(),
                },
                crate::cross_review::CrossComparison {
                    finding_id: "remote-r".to_string(),
                    matched_finding_id: Some("20:patch@example:local-r".to_string()),
                    outcome: "both".to_string(),
                    severity: "High".to_string(),
                },
            ],
        };
        db.persist_cross_review_result(
            &job,
            &remote,
            &analysis,
            &std::collections::HashMap::new(),
            &crate::cross_review::CrossReviewUsage::default(),
            1100,
        )
        .await
        .unwrap();

        let mut rows = db
            .conn
            .query(
                "SELECT output_raw FROM ai_interactions WHERE id = 'interaction'",
                (),
            )
            .await
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(
            &rows
                .next()
                .await
                .unwrap()
                .unwrap()
                .get::<String>(0)
                .unwrap(),
        )
        .unwrap();
        let findings = output["review"]["findings"].as_array().unwrap();
        let local = findings
            .iter()
            .find(|finding| finding["problem"] == "local accepted")
            .unwrap();
        assert!(
            local["source_models"]
                .as_array()
                .unwrap()
                .contains(&json!("peer"))
        );
        assert!(
            local["finding_ids"]
                .as_array()
                .unwrap()
                .contains(&json!("remote-a"))
        );
        let imported = findings
            .iter()
            .find(|finding| finding["cross_review_finding_id"] == "remote-r")
            .unwrap();
        assert!(
            imported["source_models"]
                .as_array()
                .unwrap()
                .contains(&json!("main"))
        );
        assert!(
            imported["finding_ids"]
                .as_array()
                .unwrap()
                .contains(&json!("local-r"))
        );

        let mut rows = db
            .conn
            .query(
                "SELECT matched_finding_id FROM cross_review_comparisons
                 ORDER BY finding_id",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next()
                .await
                .unwrap()
                .unwrap()
                .get::<String>(0)
                .unwrap(),
            "20:patch@example:local-a"
        );
        assert_eq!(
            rows.next()
                .await
                .unwrap()
                .unwrap()
                .get::<String>(0)
                .unwrap(),
            "20:patch@example:local-r"
        );
    }

    #[tokio::test]
    async fn cross_review_migration_upgrades_existing_findings_table() {
        let temp = tempfile::tempdir().unwrap();
        let settings = DatabaseSettings {
            url: temp.path().join("old.db").display().to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.conn
            .execute(
                "CREATE TABLE findings (
                    id INTEGER PRIMARY KEY,
                    review_id INTEGER NOT NULL,
                    severity INTEGER NOT NULL
                 )",
                (),
            )
            .await
            .unwrap();

        db.migrate().await.unwrap();

        let mut rows = db
            .conn
            .query("PRAGMA table_info(findings)", ())
            .await
            .unwrap();
        let mut columns = std::collections::HashSet::new();
        while let Some(row) = rows.next().await.unwrap() {
            columns.insert(row.get::<String>(1).unwrap());
        }
        assert!(columns.contains("cross_review_job_id"));
        assert!(columns.contains("external_finding_id"));
    }

    #[tokio::test]
    async fn model_experiment_migration_adds_confirmer_without_backfill() {
        let temp = tempfile::tempdir().unwrap();
        let settings = DatabaseSettings {
            url: temp.path().join("old-experiment.db").display().to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();
        db.conn
            .execute_batch(
                "DROP TABLE model_experiment_findings;
                 CREATE TABLE model_experiment_findings (
                    id INTEGER PRIMARY KEY,
                    review_id INTEGER NOT NULL,
                    additional_model TEXT NOT NULL,
                    main_model_id TEXT NOT NULL DEFAULT '',
                    additional_model_id TEXT NOT NULL DEFAULT '',
                    main_provider_id TEXT NOT NULL DEFAULT '',
                    additional_provider_id TEXT NOT NULL DEFAULT '',
                    finding_id TEXT NOT NULL,
                    outcome TEXT NOT NULL,
                    severity TEXT
                 );
                 INSERT INTO model_experiment_findings
                    (id, review_id, additional_model, finding_id, outcome)
                    VALUES (1, 1, 'variant', 'old-finding', 'main_only');",
            )
            .await
            .unwrap();

        db.migrate().await.unwrap();

        let mut rows = db
            .conn
            .query("PRAGMA table_info(model_experiment_findings)", ())
            .await
            .unwrap();
        let mut columns = std::collections::HashSet::new();
        while let Some(row) = rows.next().await.unwrap() {
            columns.insert(row.get::<String>(1).unwrap());
        }
        assert!(columns.contains("confirmed_by"));

        let mut rows = db
            .conn
            .query(
                "SELECT confirmed_by FROM model_experiment_findings WHERE id = 1",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert!(row.get::<Option<String>>(0).unwrap().is_none());
    }

    #[tokio::test]
    async fn cross_review_migration_replaces_the_pre_generation_constraint() {
        let temp = tempfile::tempdir().unwrap();
        let settings = DatabaseSettings {
            url: temp.path().join("old-cross.db").display().to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();
        db.conn
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
                 DROP TABLE cross_review_jobs;
                 CREATE TABLE cross_review_jobs (
                    id INTEGER PRIMARY KEY,
                    patchset_id INTEGER NOT NULL,
                    source_name TEXT NOT NULL,
                    source_url TEXT NOT NULL,
                    lookup_message_id TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    first_attempt_at INTEGER NOT NULL,
                    next_attempt_at INTEGER NOT NULL,
                    deadline_at INTEGER NOT NULL,
                    lease_until INTEGER,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    completed_at INTEGER,
                    remote_model TEXT,
                    remote_provider TEXT,
                    payload_hash TEXT,
                    UNIQUE(patchset_id, source_name)
                 );
                 PRAGMA foreign_keys=ON;
                 INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('cover@example', 1);
                 INSERT INTO patchsets
                    (id, thread_id, cover_letter_message_id, status,
                     target_review_count, model_name, provider)
                    VALUES (1, 1, 'cover@example', 'Reviewed', 1,
                            'local-model', 'local-provider');
                 INSERT INTO cross_review_jobs
                    (id, patchset_id, source_name, source_url, lookup_message_id,
                     status, first_attempt_at, next_attempt_at, deadline_at)
                    VALUES (10, 1, 'peer', 'https://peer.example', 'cover@example',
                            'complete', 1000, 1000, 2000);",
            )
            .await
            .unwrap();

        db.migrate().await.unwrap();
        db.conn
            .execute(
                "UPDATE patchsets SET target_review_count = 2 WHERE id = 1",
                (),
            )
            .await
            .unwrap();
        db.enqueue_cross_reviews(
            1,
            &[("peer".to_string(), "https://peer.example".to_string())],
            3000,
        )
        .await
        .unwrap();
        let mut rows = db
            .conn
            .query(
                "SELECT generation, local_model, local_provider
                 FROM cross_review_jobs WHERE patchset_id = 1 ORDER BY generation",
                (),
            )
            .await
            .unwrap();
        let first = rows.next().await.unwrap().unwrap();
        assert_eq!(first.get::<i64>(0).unwrap(), 1);
        assert_eq!(first.get::<String>(1).unwrap(), "local-model");
        assert_eq!(first.get::<String>(2).unwrap(), "local-provider");
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            2
        );
        drop(rows);

        db.migrate().await.unwrap();
        let mut rows = db
            .conn
            .query(
                "SELECT generation FROM cross_review_jobs
                 WHERE patchset_id = 1 ORDER BY generation",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            1
        );
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn cross_review_completion_requires_every_remote() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('cover@example', 1);
                 INSERT INTO patchsets
                    (id, thread_id, cover_letter_message_id, status)
                    VALUES (1, 1, 'cover@example', 'Reviewed');",
            )
            .await
            .unwrap();
        db.enqueue_cross_reviews(
            1,
            &[
                ("peer-a".to_string(), "https://a.example".to_string()),
                ("peer-b".to_string(), "https://b.example".to_string()),
            ],
            1000,
        )
        .await
        .unwrap();
        let jobs = db.claim_due_cross_reviews(1000, 10).await.unwrap();
        db.finish_cross_review_job(&jobs[0], "complete", 1100, None)
            .await
            .unwrap();
        let status = db.get_cross_review_status(1).await.unwrap();
        assert_eq!(status["status"], "pending");
        assert_eq!(status["sources"].as_array().unwrap().len(), 2);
        assert_eq!(
            status["sources"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|source| source["status"] == "complete")
                .count(),
            1
        );
        let second = db
            .claim_due_cross_reviews(1101, 10)
            .await
            .unwrap()
            .pop()
            .unwrap();
        db.finish_cross_review_job(&second, "complete", 1200, None)
            .await
            .unwrap();
        let status = db.get_cross_review_status(1).await.unwrap();
        assert_eq!(status["status"], "complete");
        assert!(
            status["sources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|source| source["status"] == "complete")
        );
    }

    #[tokio::test]
    async fn cross_review_claims_are_fenced_and_generation_scoped() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES
                    ('cover@example', 1), ('patch@example', 1);
                 INSERT INTO patchsets
                    (id, thread_id, cover_letter_message_id, status, target_review_count)
                    VALUES (1, 1, 'cover@example', 'Reviewed', 1);
                 INSERT INTO patches (id, patchset_id, message_id, part_index, diff)
                    VALUES (10, 1, 'patch@example', 1, 'diff');",
            )
            .await
            .unwrap();
        let sources = [("peer".to_string(), "https://peer.example".to_string())];
        db.enqueue_cross_reviews(1, &sources, 1000).await.unwrap();
        let first = db
            .claim_due_cross_reviews(1000, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(first.lookup_message_id, "patch@example");
        assert_eq!(first.fallback_message_id.as_deref(), Some("cover@example"));

        let second = db
            .claim_due_cross_reviews(4600, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_ne!(first.lease_token, second.lease_token);
        assert!(
            !db.finish_cross_review_job(&first, "complete", 4601, None)
                .await
                .unwrap()
        );
        assert!(
            db.finish_cross_review_job(&second, "complete", 4601, None)
                .await
                .unwrap()
        );

        db.conn
            .execute(
                "UPDATE patchsets SET target_review_count = 2 WHERE id = 1",
                (),
            )
            .await
            .unwrap();
        db.enqueue_cross_reviews(1, &sources, 5000).await.unwrap();
        let generation_two = db
            .claim_due_cross_reviews(5000, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(generation_two.generation, 2);
        assert_eq!(
            db.get_cross_review_status(1).await.unwrap()["status"],
            "pending"
        );
        db.conn
            .execute(
                "UPDATE patchsets SET cross_review_generation = 0 WHERE id = 1",
                (),
            )
            .await
            .unwrap();
        assert!(
            !db.finish_cross_review_job(&generation_two, "complete", 5001, None)
                .await
                .unwrap()
        );
        db.conn
            .execute(
                "UPDATE patchsets SET cross_review_generation = 2 WHERE id = 1",
                (),
            )
            .await
            .unwrap();
        db.finish_cross_review_job(&generation_two, "complete", 5001, None)
            .await
            .unwrap();
        assert_eq!(
            db.get_cross_review_status(1).await.unwrap()["status"],
            "complete"
        );

        let mut rows = db
            .conn
            .query(
                "SELECT COUNT(*) FROM cross_review_jobs WHERE patchset_id = 1",
                (),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn canonical_candidates_match_any_published_provenance_id() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO patchsets (id, status) VALUES (1, 'Reviewed');
                 INSERT INTO reviews (id, patchset_id) VALUES (1, 1);",
            )
            .await
            .unwrap();
        db.save_local_canonical_findings(
            1,
            &json!([{
                "finding_ids": ["main-3-0", "variant-3-0"],
                "problem": "candidate",
                "preexisting": false
            }]),
            &json!([{
                "finding_ids": ["variant-3-0"],
                "problem": "published",
                "preexisting": false
            }]),
        )
        .await
        .unwrap();

        let mut rows = db
            .conn
            .query(
                "SELECT accepted, finding_json FROM local_canonical_findings WHERE review_id = 1",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 1);
        assert!(row.get::<String>(1).unwrap().contains("published"));
    }

    #[tokio::test]
    async fn cross_review_inputs_exclude_unpublished_reviews() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('patch@example', 1);
                 INSERT INTO patchsets (id, thread_id, status) VALUES (1, 1, 'Reviewed');
                 INSERT INTO patches (id, patchset_id, message_id, part_index, diff)
                    VALUES (10, 1, 'patch@example', 1, 'diff');
                 INSERT INTO ai_interactions (id, input_context, output_raw) VALUES
                    ('old', 'old context', '{}'),
                    ('published', 'new context', '{}'),
                    ('unfinished', 'unfinished context', '{}');
                 INSERT INTO reviews
                    (id, patchset_id, patch_id, interaction_id, status) VALUES
                    (19, 1, 10, 'old', 'Reviewed'),
                    (20, 1, 10, 'published', 'Reviewed'),
                    (21, 1, 10, 'unfinished', 'In Review');
                 INSERT INTO local_canonical_findings
                    (review_id, finding_id, finding_json, accepted) VALUES
                    (19, 'old', '{\"severity\":\"Low\"}', 1),
                    (20, 'published', '{\"severity\":\"High\"}', 1),
                    (21, 'unfinished', '{\"severity\":\"High\"}', 1);",
            )
            .await
            .unwrap();

        let (findings, context) = db.load_cross_review_inputs(1).await.unwrap();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].finding_id.contains("published"));
        assert!(context.contains("new context"));
        assert!(!context.contains("old context"));
    }

    #[tokio::test]
    async fn serial_cross_results_merge_remote_provenance() {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO messages (message_id, thread_id) VALUES ('patch@example', 1);
                 INSERT INTO patchsets (id, thread_id, cover_letter_message_id, status)
                    VALUES (1, 1, 'patch@example', 'Reviewed');
                 INSERT INTO patches (id, patchset_id, message_id, part_index, diff)
                    VALUES (10, 1, 'patch@example', 1, 'diff');
                 INSERT INTO ai_interactions (id, input_context, output_raw)
                    VALUES ('interaction', 'context', '{\"review\":{\"findings\":[]}}');
                 INSERT INTO reviews
                    (id, patchset_id, patch_id, interaction_id, status, created_at, inline_review)
                    VALUES (20, 1, 10, 'interaction', 'Reviewed', 1000, 'Initial');",
            )
            .await
            .unwrap();
        db.enqueue_cross_reviews(
            1,
            &[
                ("peer-a".to_string(), "https://a.example".to_string()),
                ("peer-b".to_string(), "https://b.example".to_string()),
            ],
            1000,
        )
        .await
        .unwrap();
        let first = db
            .claim_due_cross_reviews(1000, 2)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let make_result = |id: &str| {
            let finding = crate::cross_review::RemoteFinding {
                finding_id: id.to_string(),
                patch_message_id: "patch@example".to_string(),
                severity: "Medium".to_string(),
                problem: format!("problem {id}"),
                reasoning: "confirmed".to_string(),
                locations: json!([]),
            };
            (
                crate::cross_review::RemoteReviewResult {
                    model: "remote".to_string(),
                    provider: "remote".to_string(),
                    payload_hash: id.to_string(),
                    findings: vec![finding.clone()],
                },
                crate::cross_review::CrossReviewAnalysis {
                    accepted_remote: vec![finding],
                    matched_local: Vec::new(),
                    matched_remote: Vec::new(),
                    comparisons: vec![crate::cross_review::CrossComparison {
                        finding_id: id.to_string(),
                        matched_finding_id: None,
                        outcome: "remote_only".to_string(),
                        severity: "Medium".to_string(),
                    }],
                },
            )
        };
        let (remote_a, analysis_a) = make_result("remote-a");
        let usage = crate::cross_review::CrossReviewUsage::default();
        let rendered = std::collections::HashMap::new();
        db.persist_cross_review_result(&first, &remote_a, &analysis_a, &rendered, &usage, 1100)
            .await
            .unwrap();
        let second = db
            .claim_due_cross_reviews(1101, 2)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let (remote_b, _) = make_result("remote-b");
        let analysis_b = crate::cross_review::CrossReviewAnalysis {
            accepted_remote: Vec::new(),
            matched_local: Vec::new(),
            matched_remote: vec![crate::cross_review::CrossRemoteMatch {
                finding_id: "remote-b".to_string(),
                existing_job_id: first.id,
                existing_finding_id: "remote-a".to_string(),
            }],
            comparisons: vec![crate::cross_review::CrossComparison {
                finding_id: "remote-b".to_string(),
                matched_finding_id: Some(format!("remote:{}:remote-a", first.id)),
                outcome: "remote_only".to_string(),
                severity: "Medium".to_string(),
            }],
        };
        db.persist_cross_review_result(&second, &remote_b, &analysis_b, &rendered, &usage, 1200)
            .await
            .unwrap();

        let mut rows = db
            .conn
            .query(
                "SELECT output_raw FROM ai_interactions WHERE id = 'interaction'",
                (),
            )
            .await
            .unwrap();
        let output: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert!(output.contains("problem remote-a"));
        assert!(!output.contains("problem remote-b"));
        assert!(output.contains("peer-a"));
        assert!(output.contains("peer-b"));
        assert!(output.contains("remote-b"));
        let (inputs, _) = db.load_cross_review_inputs(1).await.unwrap();
        let remote_inputs = inputs
            .iter()
            .filter(|finding| finding.source_name.is_some())
            .collect::<Vec<_>>();
        assert_eq!(remote_inputs.len(), 1);
        assert_eq!(remote_inputs[0].source_name.as_deref(), Some("peer-a"));
        let mut rows = db
            .conn
            .query("SELECT COUNT(*) FROM findings WHERE review_id = 20", ())
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            1
        );
    }

    // -- Patchwork patch state ingest --

    fn pw_event(msgid: &str, state: &str, event_id: i64) -> crate::patchwork::PatchworkStateRecord {
        crate::patchwork::PatchworkStateRecord {
            msgid: msgid.to_string(),
            state: state.to_string(),
            previous_state: Some("new".to_string()),
            actor: Some("maintainer".to_string()),
            changed_at: Some("2026-08-24T20:11:10.614568".to_string()),
            event_id: Some(event_id),
            pw_patch_id: Some(1000 + event_id),
            pw_series_id: Some(7),
            seed: false,
        }
    }

    fn pw_seed(msgid: &str, state: &str) -> crate::patchwork::PatchworkStateRecord {
        crate::patchwork::PatchworkStateRecord {
            msgid: msgid.to_string(),
            state: state.to_string(),
            previous_state: None,
            actor: None,
            changed_at: None,
            event_id: None,
            pw_patch_id: Some(555),
            pw_series_id: Some(7),
            seed: true,
        }
    }

    async fn setup_patchwork_db() -> Arc<Database> {
        let db = setup_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO threads (id, root_message_id) VALUES (1, 'root');
                 INSERT INTO patchsets (id, thread_id, status, date)
                     VALUES (1, 1, 'Reviewed', unixepoch('now', '-3 days'));
                 -- patches.message_id is a foreign key into messages.
                 INSERT INTO messages (id, message_id, thread_id) VALUES
                     (1, 'patch-one@example.com', 1),
                     (2, 'patch-two@example.com', 1),
                     (3, 'patch-three@example.com', 1),
                     (4, 'patch-four@example.com', 1);
                 INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES
                     (1, 1, 'patch-one@example.com', 1),
                     (2, 1, 'patch-two@example.com', 2);",
            )
            .await
            .unwrap();
        db
    }

    async fn pw_state_row(
        db: &Database,
        patch_id: i64,
    ) -> Option<(String, String, Option<String>, Option<i64>)> {
        let mut rows = db
            .conn
            .query(
                "SELECT state, outcome, initial_state, last_event_id
                 FROM patchwork_patch_state WHERE patch_id = ?",
                libsql::params![patch_id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap()?;
        Some((
            row.get(0).unwrap(),
            row.get(1).unwrap(),
            row.get(2).unwrap(),
            row.get(3).unwrap(),
        ))
    }

    /// Patches sashiko never reviewed are dropped outright: nothing is stored,
    /// no placeholder rows appear, and the watermark still advances so the
    /// poller does not re-fetch the same events forever.
    #[tokio::test]
    async fn patchwork_ingest_drops_patches_we_do_not_know() {
        let db = setup_patchwork_db().await;
        let records = vec![
            pw_event("<someone-elses@example.com>", "accepted", 10),
            pw_event("<also-not-ours@example.com>", "superseded", 11),
        ];

        let summary = db
            .record_patchwork_states(&records, Some("2026-08-24T20:11:10"), Some(11))
            .await
            .unwrap();

        assert_eq!(summary.stored, 0);
        assert_eq!(summary.unknown, 2);
        let mut rows = db
            .conn
            .query("SELECT COUNT(*) FROM patchwork_patch_state", ())
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            0
        );
        // No patch or patchset was invented for the unknown message-ids.
        let mut rows = db
            .conn
            .query("SELECT COUNT(*) FROM patches", ())
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            2
        );

        let sync = db.get_patchwork_sync().await.unwrap();
        assert_eq!(sync["last_event_date"], "2026-08-24T20:11:10");
        assert_eq!(sync["last_event_id"], 11);
    }

    /// Angle brackets are optional on the wire; sashiko stores message-ids bare.
    #[tokio::test]
    async fn patchwork_ingest_normalises_angle_brackets() {
        let db = setup_patchwork_db().await;
        let summary = db
            .record_patchwork_states(
                &[pw_event("<patch-one@example.com>", "accepted", 5)],
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(summary.stored, 1);
        assert_eq!(
            pw_state_row(&db, 1).await.unwrap(),
            ("accepted".into(), "accepted".into(), None, Some(5))
        );
    }

    /// Re-sweeping a date range must be a no-op, and a late-arriving older
    /// event must not move a patch backwards.
    #[tokio::test]
    async fn patchwork_ingest_guards_event_ordering() {
        let db = setup_patchwork_db().await;

        let summary = db
            .record_patchwork_states(
                &[pw_event("patch-one@example.com", "changes-requested", 100)],
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(summary.stored, 1);

        // Same event again — replay after a crash.
        let summary = db
            .record_patchwork_states(
                &[pw_event("patch-one@example.com", "changes-requested", 100)],
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!((summary.stored, summary.stale), (0, 1));

        // Older event arriving late must not overwrite.
        let summary = db
            .record_patchwork_states(&[pw_event("patch-one@example.com", "new", 90)], None, None)
            .await
            .unwrap();
        assert_eq!((summary.stored, summary.stale), (0, 1));
        assert_eq!(pw_state_row(&db, 1).await.unwrap().0, "changes-requested");

        // A newer event applies, and superseded rolls into changes_requested.
        let summary = db
            .record_patchwork_states(
                &[pw_event("patch-one@example.com", "superseded", 110)],
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(summary.stored, 1);
        let (state, outcome, _, last_event) = pw_state_row(&db, 1).await.unwrap();
        assert_eq!(
            (state.as_str(), outcome.as_str(), last_event),
            ("superseded", "changes_requested", Some(110))
        );
    }

    /// A seed snapshot establishes the baseline but must never clobber a state
    /// an event already recorded.
    #[tokio::test]
    async fn patchwork_seed_never_overwrites_event_state() {
        let db = setup_patchwork_db().await;

        db.record_patchwork_states(
            &[pw_event("patch-one@example.com", "accepted", 42)],
            None,
            None,
        )
        .await
        .unwrap();
        // Seed arrives afterwards (poller restarted mid-series, say).
        db.record_patchwork_states(&[pw_seed("patch-one@example.com", "new")], None, None)
            .await
            .unwrap();

        let (state, outcome, initial, last_event) = pw_state_row(&db, 1).await.unwrap();
        assert_eq!(state, "accepted");
        assert_eq!(outcome, "accepted");
        assert_eq!(initial.as_deref(), Some("new"));
        assert_eq!(last_event, Some(42));
    }

    /// The normal order: seed at review time, then events move the patch while
    /// initial_state keeps recording what we reviewed against.
    #[tokio::test]
    async fn patchwork_seed_then_event_keeps_initial_state() {
        let db = setup_patchwork_db().await;

        db.record_patchwork_states(&[pw_seed("patch-two@example.com", "new")], None, None)
            .await
            .unwrap();
        assert_eq!(
            pw_state_row(&db, 2).await.unwrap(),
            ("new".into(), "active".into(), Some("new".into()), None)
        );

        db.record_patchwork_states(
            &[pw_event("patch-two@example.com", "changes-requested", 7)],
            None,
            None,
        )
        .await
        .unwrap();
        let (state, outcome, initial, _) = pw_state_row(&db, 2).await.unwrap();
        assert_eq!(state, "changes-requested");
        assert_eq!(outcome, "changes_requested");
        assert_eq!(initial.as_deref(), Some("new"));
    }

    /// The watermark only ever moves forward, even if a batch reports an older
    /// position — a `--backfill-states` run over an old range must not rewind
    /// the live sweep.
    #[tokio::test]
    async fn patchwork_watermark_never_moves_backwards() {
        let db = setup_patchwork_db().await;
        db.record_patchwork_states(&[], Some("2026-08-24T00:00:00"), Some(500))
            .await
            .unwrap();
        db.record_patchwork_states(&[], Some("2026-08-20T00:00:00"), Some(400))
            .await
            .unwrap();
        let sync = db.get_patchwork_sync().await.unwrap();
        assert_eq!(sync["last_event_id"], 500);
        assert_eq!(sync["last_event_date"], "2026-08-24T00:00:00");
    }

    /// Severity groups must be disjoint (max severity wins) so the UI's
    /// composition percentages sum to 100%, pre-existing findings must not
    /// count, and the 'other' scope must only include patches where an
    /// additional model actually ran.
    #[tokio::test]
    async fn patchwork_stats_group_by_max_severity() {
        let db = setup_patchwork_db().await;
        db.conn
            .execute_batch(
                "INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES
                     (3, 1, 'patch-three@example.com', 3),
                     (4, 1, 'patch-four@example.com', 4);
                 INSERT INTO reviews (id, patchset_id, patch_id, status, created_at) VALUES
                     (1, 1, 1, 'Reviewed', unixepoch('now')),
                     (2, 1, 2, 'Reviewed', unixepoch('now')),
                     (3, 1, 3, 'Reviewed', unixepoch('now')),
                     (4, 1, 4, 'Reviewed', unixepoch('now'));
                 -- patch 1: Low + High -> High group.  patch 2: only a
                 -- pre-existing Critical, so it belongs in the 'none' group.
                 INSERT INTO findings (id, review_id, severity, problem, preexisting) VALUES
                     (1, 1, 1, 'low', 0),
                     (2, 1, 3, 'high', 0),
                     (3, 2, 4, 'preexisting critical', 1);
                 -- An additional model ran on patches 1 and 2 only.  Patch 4
                 -- has source rows but nothing that ran: every candidate model
                 -- gets a row, and a selected one can still fail.
                 INSERT INTO model_experiment_sources (id, review_id, experiment_name, selected, status) VALUES
                     (1, 1, 'sonnet-5', 1, 'completed'),
                     (2, 2, 'sonnet-5', 1, 'completed'),
                     (3, 4, 'sonnet-5', 0, 'not_selected'),
                     (4, 4, 'fable-5', 1, 'failed');
                 INSERT INTO model_experiment_findings (id, review_id, additional_model, finding_id, outcome, severity) VALUES
                     (1, 1, 'sonnet-5', 'f1', 'additional_only', 'Medium'),
                     -- main_only is the main model's finding, not the peer's.
                     (2, 2, 'sonnet-5', 'f2', 'main_only', 'Critical');",
            )
            .await
            .unwrap();
        db.record_patchwork_states(
            &[
                pw_event("patch-one@example.com", "accepted", 1),
                pw_event("patch-two@example.com", "accepted", 2),
                pw_event("patch-three@example.com", "accepted", 3),
                pw_event("patch-four@example.com", "accepted", 4),
            ],
            None,
            None,
        )
        .await
        .unwrap();

        let stats = db.get_patchwork_stats(90).await.unwrap();
        let daily = stats["daily"].as_array().unwrap();

        let main = daily
            .iter()
            .find(|row| row["scope"] == "main" && row["outcome"] == "accepted")
            .expect("main row");
        // patch 1 -> high, patches 2 (pre-existing only), 3 and 4 -> none
        assert_eq!(main["high"], 1);
        assert_eq!(
            main["low"], 0,
            "max severity wins, so Low must not double count"
        );
        assert_eq!(main["critical"], 0, "pre-existing findings must not count");
        assert_eq!(main["none"], 3);

        let other = daily
            .iter()
            .find(|row| row["scope"] == "other" && row["outcome"] == "accepted")
            .expect("other row");
        // Only patches 1 and 2 had an additional model run.  Patch 3 has no
        // source row at all and patch 4's sources never produced a review, so
        // neither may be counted as "ran and found nothing".
        assert_eq!(other["medium"], 1);
        assert_eq!(other["none"], 1);
        assert_eq!(other["critical"], 0, "main_only is not the peer's finding");
        let other_total: i64 = ["none", "low", "medium", "high", "critical"]
            .iter()
            .map(|k| other[*k].as_i64().unwrap())
            .sum();
        assert_eq!(other_total, 2);
    }
}
