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

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::StreamExt;
use regex::Regex;
use reqwest::Client;
use sashiko::ai::claude::ClaudeError;
use sashiko::ai::gemini::GeminiError;
use sashiko::ai::openai::OpenAiCompatError;
use sashiko::ai::{AiMessage, AiProvider, AiRequest, AiResponseFormat, AiRole, create_provider};
use sashiko::db::Database;
use sashiko::settings::Settings;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the benchmark JSON file
    #[arg(short, long, group = "input")]
    file: Option<String>,

    /// Path to the corpus directory
    #[arg(short, long, group = "input")]
    corpus: Option<String>,

    /// Override the default port (reads from settings by default)
    #[arg(short, long)]
    port: Option<u16>,

    /// Override the default repo URL (default: kernel.org linux.git, only for --file mode)
    #[arg(short, long)]
    repo: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct BenchmarkEntry {
    #[serde(rename = "Commit")]
    commit: String,
    #[serde(rename = "Fixed-by")]
    _fixed_by: Option<String>,
    #[serde(rename = "subsystem")]
    _subsystem: Option<String>,
    #[serde(rename = "problem_description")]
    problem_description: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SubmitRequest {
    Remote { sha: String, repo: String },
    Inject {
        raw: String,
        base_commit: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct CaseMetadata {
    #[allow(dead_code)]
    patch_file: String,
    base_hash: Option<String>,
    #[allow(dead_code)]
    description: Option<String>,
    test_patch: Option<usize>,
}

#[derive(Debug, Clone)]
struct Annotation {
    label: String,
    description: String,
}

#[derive(Debug)]
struct CorpusCase {
    name: String,
    metadata: CaseMetadata,
    annotations: Vec<Annotation>,
    mbox_content: String,
    test_message_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnnotationResult {
    label: String,
    status: String,
    matched_finding: Option<String>,
    explanation: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FalsePositiveResult {
    finding: String,
    severity: Option<i32>,
    explanation: String,
}

#[derive(Debug, Serialize)]
struct CorpusBenchmarkResult {
    case_name: String,
    description: Option<String>,
    annotation_results: Vec<AnnotationResult>,
    false_positives: Vec<FalsePositiveResult>,
    findings_count: usize,
    concerns_count: usize,
    tokens_in: u32,
    tokens_out: u32,
    turns: u32,
    duration_secs: u64,
    annotations_total: usize,
    annotations_detected: usize,
    annotations_partially_detected: usize,
    annotations_missed: usize,
    false_positive_count: usize,
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    commit: String,
    problem_description: String,
    found: bool,
    status: String, // "DETECTED", "PARTIALLY_DETECTED", "MISSED", "UNKNOWN", "NOT_REVIEWED", "SKIPPED", "NOT_FOUND_IN_DB"
    explanation: String,
    findings_count: usize,
    concerns_count: usize,
    tokens_in: u32,
    tokens_out: u32,
    turns: u32,
    duration_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(env_filter)
        .with_writer(sashiko::logging::IgnoreBrokenPipe(std::io::stdout))
        .init();

    // Initialize settings and DB
    let settings = Settings::new().context("Failed to load settings")?;
    let db = Arc::new(
        Database::new(&settings.database)
            .await
            .context("Failed to connect to database")?,
    );

    let port = args.port.unwrap_or(settings.server.port);

    if let Some(corpus_path) = &args.corpus {
        run_corpus_benchmark(corpus_path, port, &settings, db).await
    } else if let Some(file_path) = &args.file {
        run_json_benchmark(file_path, port, &settings, db, args.repo).await
    } else {
        anyhow::bail!("Either --file or --corpus must be specified")
    }
}

async fn run_json_benchmark(
    file_path: &str,
    port: u16,
    settings: &Settings,
    db: Arc<Database>,
    repo_override: Option<String>,
) -> Result<()> {
    let benchmark_path = Path::new(file_path);
    let file =
        File::open(benchmark_path).with_context(|| format!("Failed to open {}", file_path))?;
    let reader = BufReader::new(file);
    let benchmark_entries: Vec<BenchmarkEntry> = serde_json::from_reader(reader)
        .with_context(|| format!("Failed to parse {}", file_path))?;

    let total_entries = benchmark_entries.len();
    info!("Loaded {} benchmark entries.", total_entries);

    let repo_url = repo_override.unwrap_or_else(|| {
        "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git".to_string()
    });

    let target_url = format!("http://127.0.0.1:{}/api/submit", port);
    let client = Client::new();

    // --- Phase 1: Ingestion ---
    info!("--- Phase 1: Ingesting Patches ---");
    for entry in &benchmark_entries {
        info!("Submitting commit: {}", entry.commit);
        let payload = SubmitRequest::Remote {
            sha: entry.commit.clone(),
            repo: repo_url.clone(),
        };

        let res = client.post(&target_url).json(&payload).send().await;
        match res {
            Ok(response) => {
                if response.status().is_success() {
                    info!("Successfully submitted {}", entry.commit);
                } else {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    error!(
                        "Failed to submit {}: Status {} Body: {}",
                        entry.commit, status, text
                    );
                }
            }
            Err(e) => {
                error!("Failed to send request for {}: {}", entry.commit, e);
            }
        }
    }

    // --- Phase 2: Wait for Reviews to Finish ---
    info!("--- Phase 2: Waiting for Reviews to Complete ---");
    wait_for_reviews(&db, benchmark_entries.iter().map(|e| e.commit.as_str())).await?;

    // --- Phase 3: Evaluate Results ---
    info!("--- Phase 3: Evaluating Results ---");
    let ai_provider = create_provider(settings).context("Failed to create AI provider")?;
    let processed_count = Arc::new(AtomicUsize::new(0));
    let concurrency = settings.review.concurrency;
    info!("Running evaluation with concurrency: {}", concurrency);

    let results: Vec<BenchmarkResult> = futures::stream::iter(benchmark_entries)
        .map(|entry| {
            let db = db.clone();
            let client = ai_provider.clone();
            let processed_count = processed_count.clone();
            async move {
                let res = process_entry(db, client, entry).await;
                let current = processed_count.fetch_add(1, Ordering::Relaxed) + 1;
                if current.is_multiple_of(10) {
                    info!("Progress: {}/{}", current, total_entries);
                }
                res
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    // Aggregate Stats
    let mut detected_count = 0;
    let mut partially_detected_count = 0;
    let mut missed_count = 0;
    let mut not_reviewed_count = 0;
    let mut skipped_count = 0;

    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_turns: u64 = 0;
    let mut total_duration: u64 = 0;
    let mut valid_metric_count: u64 = 0;
    let mut total_findings: u64 = 0;
    let mut total_concerns: u64 = 0;

    for res in &results {
        match res.status.as_str() {
            "DETECTED" => detected_count += 1,
            "PARTIALLY_DETECTED" => partially_detected_count += 1,
            "MISSED" => missed_count += 1,
            "NOT_REVIEWED" | "NOT_FOUND_IN_DB" => not_reviewed_count += 1,
            "SKIPPED" => skipped_count += 1,
            _ => {}
        }

        if res.turns > 0 || res.duration_secs > 0 {
            total_tokens_in += res.tokens_in as u64;
            total_tokens_out += res.tokens_out as u64;
            total_turns += res.turns as u64;
            total_duration += res.duration_secs;
            total_findings += res.findings_count as u64;
            total_concerns += res.concerns_count as u64;
            valid_metric_count += 1;
        }
    }

    // Output results
    let output_file = File::create("benchmark_results.json")?;
    serde_json::to_writer_pretty(output_file, &results)?;

    info!("Benchmark Complete.");
    info!("Total Entries: {}", results.len());
    info!("Detected (Exact): {}", detected_count);
    info!("Partially Detected: {}", partially_detected_count);
    info!("Missed: {}", missed_count);
    info!("Not Reviewed/Found: {}", not_reviewed_count);
    info!("Skipped (No Description): {}", skipped_count);
    info!("Total Concerns (Before Stage 8): {}", total_concerns);
    info!("Total Findings (Final Report): {}", total_findings);

    if valid_metric_count > 0 {
        info!("--- Performance Metrics (averages per reviewed patch) ---");
        info!("Avg Tokens In:  {}", total_tokens_in / valid_metric_count);
        info!("Avg Tokens Out: {}", total_tokens_out / valid_metric_count);
        info!(
            "Avg Turns:      {:.1}",
            total_turns as f64 / valid_metric_count as f64
        );
        info!("Avg Time:       {}s", total_duration / valid_metric_count);
    }

    info!("Detailed results written to benchmark_results.json");

    Ok(())
}

async fn wait_for_reviews<'a>(
    db: &Database,
    message_ids: impl Iterator<Item = &'a str> + Clone,
) -> Result<()> {
    loop {
        let mut all_completed = true;
        let mut missing_patches = 0;
        let mut pending_reviews = 0;
        let mut completed_reviews = 0;

        for msg_id in message_ids.clone() {
            let mut rows = db
                .conn
                .query(
                    "SELECT id FROM patches WHERE message_id = ?",
                    libsql::params![msg_id.to_string()],
                )
                .await?;

            let patch_id = if let Ok(Some(row)) = rows.next().await {
                row.get::<i64>(0).unwrap_or_default()
            } else {
                all_completed = false;
                missing_patches += 1;
                continue;
            };

            let mut rows = db
                .conn
                .query(
                    "SELECT status FROM reviews WHERE patch_id = ? ORDER BY id DESC LIMIT 1",
                    libsql::params![patch_id],
                )
                .await?;

            if let Ok(Some(row)) = rows.next().await {
                let status: String = row.get(0).unwrap_or_default();
                if status == "Pending" || status == "In Review" {
                    all_completed = false;
                    pending_reviews += 1;
                } else {
                    completed_reviews += 1;
                }
            } else {
                all_completed = false;
                pending_reviews += 1;
            }
        }

        if all_completed {
            info!("All patches have been reviewed.");
            break;
        }

        info!(
            "Waiting... Completed: {}, Pending: {}, Missing Patches: {}",
            completed_reviews, pending_reviews, missing_patches
        );
        sleep(Duration::from_secs(5)).await;
    }
    Ok(())
}

async fn run_corpus_benchmark(
    corpus_path: &str,
    port: u16,
    settings: &Settings,
    db: Arc<Database>,
) -> Result<()> {
    let corpus_dir = Path::new(corpus_path);
    let cases = load_corpus(corpus_dir)?;
    let total_cases = cases.len();
    info!("Loaded {} corpus cases.", total_cases);

    let target_url = format!("http://127.0.0.1:{}/api/submit", port);
    let client = Client::new();

    // --- Phase 1: Ingestion ---
    info!("--- Phase 1: Ingesting Patches ---");
    for case in &cases {
        info!("Submitting case: {} (test_message_id: {})", case.name, case.test_message_id);
        let payload = SubmitRequest::Inject {
            raw: case.mbox_content.clone(),
            base_commit: case.metadata.base_hash.clone(),
        };

        let res = client.post(&target_url).json(&payload).send().await;
        match res {
            Ok(response) => {
                if response.status().is_success() {
                    info!("Successfully submitted {}", case.name);
                } else {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    error!("Failed to submit {}: Status {} Body: {}", case.name, status, text);
                }
            }
            Err(e) => {
                error!("Failed to send request for {}: {}", case.name, e);
            }
        }
    }

    // --- Phase 2: Wait for Reviews to Finish ---
    info!("--- Phase 2: Waiting for Reviews to Complete ---");
    wait_for_reviews(&db, cases.iter().map(|c| c.test_message_id.as_str())).await?;

    // --- Phase 3: Evaluate Results ---
    info!("--- Phase 3: Evaluating Results ---");
    let ai_provider = create_provider(settings).context("Failed to create AI provider")?;
    let processed_count = Arc::new(AtomicUsize::new(0));
    let concurrency = settings.review.concurrency;
    info!("Running evaluation with concurrency: {}", concurrency);

    let results: Vec<CorpusBenchmarkResult> = futures::stream::iter(cases)
        .map(|case| {
            let db = db.clone();
            let client = ai_provider.clone();
            let processed_count = processed_count.clone();
            async move {
                let res = process_corpus_case(db, client, case).await;
                let current = processed_count.fetch_add(1, Ordering::Relaxed) + 1;
                info!("Progress: {}/{}", current, total_cases);
                res
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    // Aggregate Stats
    let mut total_annotations = 0usize;
    let mut total_detected = 0usize;
    let mut total_partially = 0usize;
    let mut total_missed = 0usize;
    let mut total_false_positives = 0usize;
    let mut total_findings_count = 0usize;

    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_turns: u64 = 0;
    let mut total_duration: u64 = 0;
    let mut valid_metric_count: u64 = 0;

    for res in &results {
        total_annotations += res.annotations_total;
        total_detected += res.annotations_detected;
        total_partially += res.annotations_partially_detected;
        total_missed += res.annotations_missed;
        total_false_positives += res.false_positive_count;
        total_findings_count += res.findings_count;

        if res.turns > 0 || res.duration_secs > 0 {
            total_tokens_in += res.tokens_in as u64;
            total_tokens_out += res.tokens_out as u64;
            total_turns += res.turns as u64;
            total_duration += res.duration_secs;
            valid_metric_count += 1;
        }

        info!(
            "Case {}: {}/{} detected, {} partial, {} missed, {} false positives",
            res.case_name,
            res.annotations_detected,
            res.annotations_total,
            res.annotations_partially_detected,
            res.annotations_missed,
            res.false_positive_count,
        );
    }

    // Output results
    let output = serde_json::json!({
        "cases": results,
        "summary": {
            "total_cases": results.len(),
            "total_annotations": total_annotations,
            "detected": total_detected,
            "partially_detected": total_partially,
            "missed": total_missed,
            "total_findings": total_findings_count,
            "false_positives": total_false_positives,
            "detection_rate": if total_annotations > 0 {
                (total_detected + total_partially) as f64 / total_annotations as f64
            } else { 0.0 },
            "false_positive_rate": if total_findings_count > 0 {
                total_false_positives as f64 / total_findings_count as f64
            } else { 0.0 },
        }
    });
    let output_file = File::create("corpus_results.json")?;
    serde_json::to_writer_pretty(output_file, &output)?;

    info!("Corpus Benchmark Complete.");
    info!("Total Cases: {}", results.len());
    info!("Total Annotations: {}", total_annotations);
    info!("  Detected: {} ({:.1}%)", total_detected, if total_annotations > 0 { total_detected as f64 / total_annotations as f64 * 100.0 } else { 0.0 });
    info!("  Partially Detected: {} ({:.1}%)", total_partially, if total_annotations > 0 { total_partially as f64 / total_annotations as f64 * 100.0 } else { 0.0 });
    info!("  Missed: {} ({:.1}%)", total_missed, if total_annotations > 0 { total_missed as f64 / total_annotations as f64 * 100.0 } else { 0.0 });
    info!("Total Findings: {}", total_findings_count);
    info!("  False Positives: {} ({:.1}%)", total_false_positives, if total_findings_count > 0 { total_false_positives as f64 / total_findings_count as f64 * 100.0 } else { 0.0 });

    if valid_metric_count > 0 {
        info!("--- Performance Metrics (averages per case) ---");
        info!("Avg Tokens In:  {}", total_tokens_in / valid_metric_count);
        info!("Avg Tokens Out: {}", total_tokens_out / valid_metric_count);
        info!("Avg Turns:      {:.1}", total_turns as f64 / valid_metric_count as f64);
        info!("Avg Time:       {}s", total_duration / valid_metric_count);
    }

    info!("Detailed results written to corpus_results.json");

    Ok(())
}

async fn process_entry(
    db: Arc<Database>,
    client: Arc<dyn AiProvider>,
    entry: BenchmarkEntry,
) -> BenchmarkResult {
    if entry.problem_description.is_none() {
        return BenchmarkResult {
            commit: entry.commit,
            problem_description: "".to_string(),
            found: false,
            status: "SKIPPED".to_string(),
            explanation: "No problem description provided".to_string(),
            findings_count: 0,
            concerns_count: 0,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            duration_secs: 0,
        };
    }
    let problem_description = entry.problem_description.clone().unwrap();

    // 1. Find Patch ID
    let patch_id_result = db
        .conn
        .query(
            "SELECT id FROM patches WHERE message_id = ?",
            libsql::params![entry.commit.clone()],
        )
        .await;

    let patch_id = match patch_id_result {
        Ok(mut rows) => {
            if let Ok(Some(row)) = rows.next().await {
                Some(row.get::<i64>(0).unwrap_or_default())
            } else {
                None
            }
        }
        Err(e) => {
            error!("DB Error finding patch {}: {}", entry.commit, e);
            None
        }
    };

    if patch_id.is_none() {
        warn!("Patch not found for commit {}", entry.commit);
        return BenchmarkResult {
            commit: entry.commit,
            problem_description,
            found: false,
            status: "NOT_FOUND_IN_DB".to_string(),
            explanation: "Patch not found in database.".to_string(),
            findings_count: 0,
            concerns_count: 0,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            duration_secs: 0,
        };
    }
    let patch_id = patch_id.unwrap();

    // 2. Find Review
    let review_result = db
        .conn
        .query(
            "SELECT id, summary, result_description, interaction_id, created_at FROM reviews WHERE patch_id = ? ORDER BY id DESC LIMIT 1",
            libsql::params![patch_id],
        )
        .await;

    let review_data = match review_result {
        Ok(mut rows) => {
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0).unwrap_or_default();
                let summary: Option<String> = row.get(1).ok();
                let result_desc: Option<String> = row.get(2).ok();
                let interaction_id: Option<String> = row.get(3).ok();
                let created_at: Option<i64> = row.get(4).ok();
                Some((id, summary, result_desc, interaction_id, created_at))
            } else {
                None
            }
        }
        Err(_) => None,
    };

    if review_data.is_none() {
        warn!("Review not found for patch {}", patch_id);
        return BenchmarkResult {
            commit: entry.commit,
            problem_description,
            found: false,
            status: "NOT_REVIEWED".to_string(),
            explanation: "Patch found but no review exists.".to_string(),
            findings_count: 0,
            concerns_count: 0,
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            duration_secs: 0,
        };
    }
    let (review_id, summary, result_desc, interaction_id, review_created_at) = review_data.unwrap();

    // Metrics Tracking
    let mut tokens_in = 0;
    let mut tokens_out = 0;
    let mut duration_secs = 0;
    let mut turns = 1; // Minimum 1 turn for the initial prompt
    let mut concerns_count = 0;

    if let Some(iid) = interaction_id {
        let int_rows = db
            .conn
            .query(
                "SELECT tokens_in, tokens_out, created_at, output_raw FROM ai_interactions WHERE id = ?",
                libsql::params![iid],
            )
            .await;

        if let Ok(mut rows) = int_rows
            && let Ok(Some(row)) = rows.next().await
        {
            tokens_in = row.get::<i64>(0).unwrap_or(0) as u32;
            tokens_out = row.get::<i64>(1).unwrap_or(0) as u32;
            let int_created_at = row.get::<i64>(2).unwrap_or(0);

            if let Ok(Some(output_raw)) = row.get::<Option<String>>(3)
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output_raw)
                && let Some(count) = parsed.get("concerns_count").and_then(|v| v.as_u64())
            {
                concerns_count = count as usize;
            }

            if let Some(start_time) = review_created_at
                && int_created_at >= start_time
            {
                duration_secs = (int_created_at - start_time) as u64;
            }
        }
    }

    // Number of turns based on tool usages
    let tool_usages_result = db
        .conn
        .query(
            "SELECT COUNT(*) FROM tool_usages WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await;

    if let Ok(mut rows) = tool_usages_result
        && let Ok(Some(row)) = rows.next().await
    {
        let tool_count: i64 = row.get(0).unwrap_or(0);
        turns = 1 + tool_count as u32; // Each tool call adds a turn, plus final response
    }

    // 3. Find Findings
    let findings_result = db
        .conn
        .query(
            "SELECT problem, severity, severity_explanation FROM findings WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await;

    let mut findings_text = String::new();
    let mut findings_count = 0;

    if let Ok(mut rows) = findings_result {
        while let Ok(Some(row)) = rows.next().await {
            let msg: String = row.get(0).unwrap_or_default();
            let severity: i32 = row.get(1).unwrap_or(0);
            let explanation: Option<String> = row.get(2).ok();

            findings_text.push_str(&format!("- [Severity {}] {}\n", severity, msg));
            if let Some(e) = explanation {
                findings_text.push_str(&format!("  Explanation: {}\n", e));
            }
            findings_count += 1;
        }
    }

    if findings_count == 0 {
        findings_text.push_str("(No structured findings recorded in DB)\n");
    }

    // 4. Evaluate with AI provider
    let review_summary = format!(
        "{}\n{}",
        summary.unwrap_or_default(),
        result_desc.unwrap_or_default()
    );

    let prompt = format!(
        "I am benchmarking an automated code review tool.\n\n\
        The known issue (ground truth) is:\n\
        {}\n\n\
        The tool produced the following findings:\n\
        {}\n\n\
        The review summary was:\n\
        {}\n\n\
        Task:\n\
        Determine if ANY of the findings or the review summary EXACTLY describes the known issue.\n\
        - The description must match the specific problem (e.g., 'memory leak in function X', 'double free', 'missing lock').\n\
        - General warnings about code style, complexity, or unrelated bugs do NOT count.\n\
        - If a finding describes the problem but with slight inaccuracy (e.g. wrong variable name but correct logic), it is PARTIALLY_DETECTED.\n\
        - If no finding matches the problem, it is MISSED.\n\n\
        Respond with EXACTLY one of: [DETECTED, PARTIALLY_DETECTED, MISSED].\n\
        Then provide a short one-sentence explanation referencing the specific finding that matches (if any).",
        problem_description, findings_text, review_summary
    );

    info!("Evaluating commit {}...", entry.commit);

    let r = loop {
        let req = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some(prompt.clone()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        match client.generate_content(req).await {
            Ok(r) => break r,
            Err(e) => {
                let retry_duration =
                    e.downcast_ref::<GeminiError>()
                        .and_then(|err| match err {
                            GeminiError::QuotaExceeded(d) | GeminiError::TransientError(d, _) => {
                                Some(*d)
                            }
                            _ => None,
                        })
                        .or_else(|| {
                            e.downcast_ref::<ClaudeError>().and_then(|err| match err {
                                ClaudeError::RateLimitExceeded(d)
                                | ClaudeError::OverloadedError(d) => Some(*d),
                                _ => None,
                            })
                        })
                        .or_else(|| {
                            e.downcast_ref::<OpenAiCompatError>()
                                .and_then(|err| match err {
                                    OpenAiCompatError::RateLimitExceeded(d)
                                    | OpenAiCompatError::TransientError(d, _) => Some(*d),
                                    _ => None,
                                })
                        });

                let duration = retry_duration.unwrap_or(std::time::Duration::from_secs(30));
                warn!(
                    "API error ({}), pausing for {:?} before retry...",
                    e, duration
                );
                tokio::time::sleep(duration).await;
            }
        }
    };

    let (status, explanation) = {
        let text = r.content.unwrap_or_else(|| "Unknown".to_string());

        let re_status = Regex::new(r"(?i)\b(DETECTED|PARTIALLY_DETECTED|MISSED)\b").unwrap();
        let (status_raw, expl_raw) = if let Some(cap) = re_status.captures(&text) {
            let s = cap[1].to_uppercase();
            let remaining = re_status.replace(&text, "").to_string();
            (s, remaining)
        } else {
            ("UNKNOWN".to_string(), text.clone())
        };

        let expl = expl_raw
            .trim()
            .trim_start_matches([':', '-', ' ', '\n'])
            .to_string();
        (status_raw, expl)
    };

    let found = status == "DETECTED" || status == "PARTIALLY_DETECTED";
    info!("Commit {}: {} ({})", entry.commit, status, explanation);

    BenchmarkResult {
        commit: entry.commit,
        problem_description,
        found,
        status,
        explanation,
        findings_count,
        concerns_count,
        tokens_in,
        tokens_out,
        turns,
        duration_secs,
    }
}

fn parse_annotations(content: &str) -> Vec<Annotation> {
    let mut annotations = Vec::new();
    let mut current_label: Option<String> = None;
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(heading) = line.strip_prefix("# ") {
            if let Some(label) = current_label.take() {
                let desc = current_body.trim().to_string();
                if !desc.is_empty() {
                    annotations.push(Annotation {
                        label,
                        description: desc,
                    });
                }
            }
            current_label = Some(heading.trim().to_string());
            current_body.clear();
        } else if current_label.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    if let Some(label) = current_label {
        let desc = current_body.trim().to_string();
        if !desc.is_empty() {
            annotations.push(Annotation {
                label,
                description: desc,
            });
        }
    }

    annotations
}

fn inject_message_ids(case_name: &str, mbox_raw: &str) -> Result<(String, Vec<String>)> {
    let emails = sashiko::ingestor::split_mbox(mbox_raw.as_bytes());
    if emails.is_empty() {
        anyhow::bail!("No emails found in mbox for {}", case_name);
    }

    let patch_count = emails.len();
    let mut message_ids = Vec::with_capacity(patch_count);
    let mut reassembled = String::new();

    for (i, email_bytes) in emails.iter().enumerate() {
        let email_str = String::from_utf8_lossy(email_bytes);

        let msg_id = if patch_count == 1 {
            format!("{}@sashiko-benchmark", case_name)
        } else {
            format!("{}-{}@sashiko-benchmark", case_name, i + 1)
        };
        message_ids.push(msg_id.clone());

        let mut headers_to_inject = format!("Message-ID: <{}>\n", msg_id);
        if i > 0 {
            headers_to_inject.push_str(&format!(
                "In-Reply-To: <{}>\n",
                message_ids[0]
            ));
        }

        let modified = if let Some(header_end) = email_str.find("\n\n") {
            let header_section = &email_str[..header_end];
            let body_section = &email_str[header_end..];

            let filtered_headers: String = header_section
                .lines()
                .filter(|line| !line.to_lowercase().starts_with("message-id:"))
                .collect::<Vec<_>>()
                .join("\n");

            format!("{}\n{}{}", filtered_headers, headers_to_inject, body_section)
        } else {
            format!("{}\n{}", headers_to_inject.trim_end(), email_str)
        };

        if !reassembled.is_empty() {
            reassembled.push_str("From sashiko@benchmark Mon Jan  1 00:00:00 2024\n");
        }
        reassembled.push_str(&modified);
    }

    Ok((reassembled, message_ids))
}

fn load_corpus(corpus_dir: &Path) -> Result<Vec<CorpusCase>> {
    let mut cases = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(corpus_dir)
        .with_context(|| format!("Failed to read corpus directory: {}", corpus_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let case = load_corpus_case(&entry.path())
            .with_context(|| format!("Failed to load case: {}", entry.path().display()))?;
        cases.push(case);
    }

    Ok(cases)
}

fn load_corpus_case(case_dir: &Path) -> Result<CorpusCase> {
    let case_name = case_dir
        .file_name()
        .context("Invalid case directory name")?
        .to_string_lossy()
        .to_string();

    let metadata_path = case_dir.join("metadata.json");
    let metadata: CaseMetadata = serde_json::from_reader(BufReader::new(
        File::open(&metadata_path)
            .with_context(|| format!("Failed to open {}", metadata_path.display()))?,
    ))?;

    let annotations_path = case_dir.join("annotations.md");
    let annotations_content = std::fs::read_to_string(&annotations_path)
        .with_context(|| format!("Failed to read {}", annotations_path.display()))?;
    let annotations = parse_annotations(&annotations_content);

    let patch_path = case_dir.join(&metadata.patch_file);
    let mbox_raw = std::fs::read_to_string(&patch_path)
        .with_context(|| format!("Failed to read {}", patch_path.display()))?;

    let (mbox_content, message_ids) = inject_message_ids(&case_name, &mbox_raw)?;

    let test_message_id = if let Some(test_idx) = metadata.test_patch {
        message_ids
            .get(test_idx - 1)
            .with_context(|| {
                format!(
                    "test_patch {} out of range (series has {} patches)",
                    test_idx,
                    message_ids.len()
                )
            })?
            .clone()
    } else {
        message_ids[0].clone()
    };

    Ok(CorpusCase {
        name: case_name,
        metadata,
        annotations,
        mbox_content,
        test_message_id,
    })
}

async fn call_with_retry(
    client: &Arc<dyn AiProvider>,
    req_builder: impl Fn() -> AiRequest,
) -> sashiko::ai::AiResponse {
    loop {
        match client.generate_content(req_builder()).await {
            Ok(r) => return r,
            Err(e) => {
                let retry_duration =
                    e.downcast_ref::<GeminiError>()
                        .and_then(|err| match err {
                            GeminiError::QuotaExceeded(d) | GeminiError::TransientError(d, _) => {
                                Some(*d)
                            }
                            _ => None,
                        })
                        .or_else(|| {
                            e.downcast_ref::<ClaudeError>().and_then(|err| match err {
                                ClaudeError::RateLimitExceeded(d)
                                | ClaudeError::OverloadedError(d) => Some(*d),
                                _ => None,
                            })
                        })
                        .or_else(|| {
                            e.downcast_ref::<OpenAiCompatError>()
                                .and_then(|err| match err {
                                    OpenAiCompatError::RateLimitExceeded(d)
                                    | OpenAiCompatError::TransientError(d, _) => Some(*d),
                                    _ => None,
                                })
                        });

                let duration = retry_duration.unwrap_or(std::time::Duration::from_secs(30));
                warn!(
                    "API error ({}), pausing for {:?} before retry...",
                    e, duration
                );
                tokio::time::sleep(duration).await;
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct JudgeResponse {
    annotation_results: Vec<AnnotationResult>,
    false_positives: Vec<FalsePositiveResult>,
}

async fn process_corpus_case(
    db: Arc<Database>,
    client: Arc<dyn AiProvider>,
    case: CorpusCase,
) -> CorpusBenchmarkResult {
    let empty_result = |reason: &str| CorpusBenchmarkResult {
        case_name: case.name.clone(),
        description: case.metadata.description.clone(),
        annotation_results: case
            .annotations
            .iter()
            .map(|a| AnnotationResult {
                label: a.label.clone(),
                status: reason.to_string(),
                matched_finding: None,
                explanation: reason.to_string(),
            })
            .collect(),
        false_positives: vec![],
        findings_count: 0,
        concerns_count: 0,
        tokens_in: 0,
        tokens_out: 0,
        turns: 0,
        duration_secs: 0,
        annotations_total: case.annotations.len(),
        annotations_detected: 0,
        annotations_partially_detected: 0,
        annotations_missed: 0,
        false_positive_count: 0,
    };

    // 1. Find Patch ID
    let patch_id_result = db
        .conn
        .query(
            "SELECT id FROM patches WHERE message_id = ?",
            libsql::params![case.test_message_id.clone()],
        )
        .await;

    let patch_id = match patch_id_result {
        Ok(mut rows) => {
            if let Ok(Some(row)) = rows.next().await {
                row.get::<i64>(0).unwrap_or_default()
            } else {
                warn!("Patch not found for case {}", case.name);
                return empty_result("NOT_FOUND_IN_DB");
            }
        }
        Err(e) => {
            error!("DB Error finding patch for {}: {}", case.name, e);
            return empty_result("NOT_FOUND_IN_DB");
        }
    };

    // 2. Find Review
    let review_result = db
        .conn
        .query(
            "SELECT id, summary, result_description, interaction_id, created_at FROM reviews WHERE patch_id = ? ORDER BY id DESC LIMIT 1",
            libsql::params![patch_id],
        )
        .await;

    let review_data = match review_result {
        Ok(mut rows) => {
            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0).unwrap_or_default();
                let summary: Option<String> = row.get(1).ok();
                let result_desc: Option<String> = row.get(2).ok();
                let interaction_id: Option<String> = row.get(3).ok();
                let created_at: Option<i64> = row.get(4).ok();
                Some((id, summary, result_desc, interaction_id, created_at))
            } else {
                None
            }
        }
        Err(_) => None,
    };

    if review_data.is_none() {
        warn!("Review not found for case {}", case.name);
        return empty_result("NOT_REVIEWED");
    }
    let (review_id, summary, result_desc, interaction_id, review_created_at) =
        review_data.unwrap();

    // Metrics
    let mut tokens_in = 0u32;
    let mut tokens_out = 0u32;
    let mut duration_secs = 0u64;
    let mut turns = 1u32;
    let mut concerns_count = 0usize;

    if let Some(iid) = interaction_id {
        let int_rows = db
            .conn
            .query(
                "SELECT tokens_in, tokens_out, created_at, output_raw FROM ai_interactions WHERE id = ?",
                libsql::params![iid],
            )
            .await;

        if let Ok(mut rows) = int_rows
            && let Ok(Some(row)) = rows.next().await
        {
            tokens_in = row.get::<i64>(0).unwrap_or(0) as u32;
            tokens_out = row.get::<i64>(1).unwrap_or(0) as u32;
            let int_created_at = row.get::<i64>(2).unwrap_or(0);

            if let Ok(Some(output_raw)) = row.get::<Option<String>>(3)
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output_raw)
                && let Some(count) = parsed.get("concerns_count").and_then(|v| v.as_u64())
            {
                concerns_count = count as usize;
            }

            if let Some(start_time) = review_created_at
                && int_created_at >= start_time
            {
                duration_secs = (int_created_at - start_time) as u64;
            }
        }
    }

    let tool_usages_result = db
        .conn
        .query(
            "SELECT COUNT(*) FROM tool_usages WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await;

    if let Ok(mut rows) = tool_usages_result
        && let Ok(Some(row)) = rows.next().await
    {
        let tool_count: i64 = row.get(0).unwrap_or(0);
        turns = 1 + tool_count as u32;
    }

    // 3. Collect Findings
    let findings_result = db
        .conn
        .query(
            "SELECT problem, severity, severity_explanation FROM findings WHERE review_id = ?",
            libsql::params![review_id],
        )
        .await;

    let mut findings_text = String::new();
    let mut findings_count = 0;

    if let Ok(mut rows) = findings_result {
        while let Ok(Some(row)) = rows.next().await {
            let msg: String = row.get(0).unwrap_or_default();
            let severity: i32 = row.get(1).unwrap_or(0);
            let explanation: Option<String> = row.get(2).ok();

            findings_count += 1;
            findings_text.push_str(&format!(
                "Finding {}: [Severity {}] {}\n",
                findings_count, severity, msg
            ));
            if let Some(e) = explanation {
                findings_text.push_str(&format!("  Explanation: {}\n", e));
            }
        }
    }

    if findings_count == 0 {
        findings_text.push_str("(No findings recorded)\n");
    }

    let review_summary = format!(
        "{}\n{}",
        summary.unwrap_or_default(),
        result_desc.unwrap_or_default()
    );

    // 4. Build annotations text
    let mut annotations_text = String::new();
    for (i, ann) in case.annotations.iter().enumerate() {
        annotations_text.push_str(&format!(
            "Annotation {}: [{}]\n{}\n\n",
            i + 1,
            ann.label,
            ann.description
        ));
    }

    // 5. Bidirectional judge prompt
    let prompt = format!(
        "You are evaluating an automated code review tool's output against known ground-truth annotations.\n\n\
        ## Known Issues (Ground Truth Annotations)\n\n\
        {}\n\
        ## Tool's Findings\n\n\
        {}\n\
        ## Review Summary\n\n\
        {}\n\n\
        ## Task\n\n\
        Perform a BIDIRECTIONAL evaluation:\n\n\
        1. For EACH annotation above, determine:\n\
           - DETECTED: A finding describes this exact issue (correct problem, correct logic)\n\
           - PARTIALLY_DETECTED: A finding describes a related issue but with inaccuracies\n\
           - MISSED: No finding matches this issue\n\n\
        2. For EACH finding NOT matched to any annotation, flag it as a false positive.\n\n\
        Respond with JSON matching this structure:\n\
        {{\n\
          \"annotation_results\": [\n\
            {{\"label\": \"<annotation label>\", \"status\": \"DETECTED|PARTIALLY_DETECTED|MISSED\", \"matched_finding\": \"Finding N\" or null, \"explanation\": \"...\"}}\n\
          ],\n\
          \"false_positives\": [\n\
            {{\"finding\": \"Finding N: ...\", \"severity\": <int or null>, \"explanation\": \"...\"}}\n\
          ]\n\
        }}",
        annotations_text, findings_text, review_summary
    );

    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "annotation_results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "label": { "type": "string" },
                        "status": { "type": "string", "enum": ["DETECTED", "PARTIALLY_DETECTED", "MISSED"] },
                        "matched_finding": { "type": ["string", "null"] },
                        "explanation": { "type": "string" }
                    },
                    "required": ["label", "status", "explanation"]
                }
            },
            "false_positives": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "finding": { "type": "string" },
                        "severity": { "type": ["integer", "null"] },
                        "explanation": { "type": "string" }
                    },
                    "required": ["finding", "explanation"]
                }
            }
        },
        "required": ["annotation_results", "false_positives"]
    });

    info!("Evaluating case {}...", case.name);

    let prompt_clone = prompt.clone();
    let r = call_with_retry(&client, || AiRequest {
        system: None,
        messages: vec![AiMessage {
            role: AiRole::User,
            content: Some(prompt_clone.clone()),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        temperature: Some(0.2),
        response_format: Some(AiResponseFormat::Json {
            schema: Some(schema.clone()),
        }),
        context_tag: None,
    })
    .await;

    let text = r.content.unwrap_or_else(|| "{}".to_string());

    let judge_result: JudgeResponse = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(
                "Failed to parse judge response for case {}: {} — raw: {}",
                case.name, e, text
            );
            JudgeResponse {
                annotation_results: case
                    .annotations
                    .iter()
                    .map(|a| AnnotationResult {
                        label: a.label.clone(),
                        status: "UNKNOWN".to_string(),
                        matched_finding: None,
                        explanation: format!("Judge response parse error: {}", e),
                    })
                    .collect(),
                false_positives: vec![],
            }
        }
    };

    let annotations_detected = judge_result
        .annotation_results
        .iter()
        .filter(|a| a.status == "DETECTED")
        .count();
    let annotations_partially = judge_result
        .annotation_results
        .iter()
        .filter(|a| a.status == "PARTIALLY_DETECTED")
        .count();
    let annotations_missed = judge_result
        .annotation_results
        .iter()
        .filter(|a| a.status == "MISSED")
        .count();
    let fp_count = judge_result.false_positives.len();

    info!(
        "Case {}: {}/{} detected, {} partial, {} missed, {} FP",
        case.name,
        annotations_detected,
        case.annotations.len(),
        annotations_partially,
        annotations_missed,
        fp_count,
    );

    CorpusBenchmarkResult {
        case_name: case.name,
        description: case.metadata.description,
        annotation_results: judge_result.annotation_results,
        false_positives: judge_result.false_positives,
        findings_count,
        concerns_count,
        tokens_in,
        tokens_out,
        turns,
        duration_secs,
        annotations_total: case.annotations.len(),
        annotations_detected,
        annotations_partially_detected: annotations_partially,
        annotations_missed,
        false_positive_count: fp_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_annotations_basic() {
        let content = "# ethtool stats deadlock\n\n\
            fbnic_get_ethtool_stats() is called with netdev_lock() already held\n\
            by the core. Taking netdev_lock() again in this path will deadlock.\n\n\
            # sleeping under RCU in get_stats64\n\n\
            fbnic_get_stats64() may be called under RCU. netdev_lock() takes a\n\
            mutex which may sleep. Sleeping under RCU is not allowed.\n";

        let annotations = parse_annotations(content);
        assert_eq!(annotations.len(), 2);

        assert_eq!(annotations[0].label, "ethtool stats deadlock");
        assert!(annotations[0].description.contains("netdev_lock()"));
        assert!(annotations[0].description.contains("deadlock"));

        assert_eq!(annotations[1].label, "sleeping under RCU in get_stats64");
        assert!(annotations[1].description.contains("RCU"));
        assert!(annotations[1].description.contains("mutex"));
    }

    #[test]
    fn test_parse_annotations_empty() {
        let annotations = parse_annotations("");
        assert!(annotations.is_empty());
    }

    #[test]
    fn test_parse_annotations_no_body() {
        let content = "# label only\n";
        let annotations = parse_annotations(content);
        assert!(annotations.is_empty());
    }

    #[test]
    fn test_inject_message_ids_single_patch() {
        let mbox = "From abc123 Mon Sep 17 00:00:00 2001\n\
            From: Test <test@example.com>\n\
            Subject: [PATCH] test patch\n\n\
            Body here.\n\
            ---\n\
            diff --git a/foo b/foo\n";

        let (modified, ids) = inject_message_ids("case-001", mbox).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], "case-001@sashiko-benchmark");
        assert!(modified.contains("Message-ID: <case-001@sashiko-benchmark>"));
        assert!(!modified.contains("In-Reply-To:"));
    }

    #[test]
    fn test_inject_message_ids_replaces_existing() {
        let mbox = "From abc123 Mon Sep 17 00:00:00 2001\n\
            From: Test <test@example.com>\n\
            Message-ID: <old-id@example.com>\n\
            Subject: [PATCH] test patch\n\n\
            Body here.\n";

        let (modified, ids) = inject_message_ids("case-002", mbox).unwrap();
        assert_eq!(ids[0], "case-002@sashiko-benchmark");
        assert!(modified.contains("Message-ID: <case-002@sashiko-benchmark>"));
        assert!(!modified.contains("old-id@example.com"));
    }

    #[test]
    fn test_inject_message_ids_multi_patch() {
        let mbox = "From abc123 Mon Sep 17 00:00:00 2001\n\
            From: Test <test@example.com>\n\
            Subject: [PATCH 1/2] first\n\n\
            Body 1.\n\
            From def456 Mon Sep 17 00:00:00 2001\n\
            From: Test <test@example.com>\n\
            Subject: [PATCH 2/2] second\n\n\
            Body 2.\n";

        let (modified, ids) = inject_message_ids("case-003", mbox).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "case-003-1@sashiko-benchmark");
        assert_eq!(ids[1], "case-003-2@sashiko-benchmark");
        assert!(modified.contains("Message-ID: <case-003-1@sashiko-benchmark>"));
        assert!(modified.contains("Message-ID: <case-003-2@sashiko-benchmark>"));
        assert!(modified.contains("In-Reply-To: <case-003-1@sashiko-benchmark>"));
    }

    #[test]
    fn test_load_corpus_case() {
        let corpus_path =
            Path::new("/home/kicinski/devel/agents/review_experiments/corpus/case-001");
        if !corpus_path.exists() {
            return;
        }

        let case = load_corpus_case(corpus_path).unwrap();
        assert_eq!(case.name, "case-001");
        assert_eq!(case.test_message_id, "case-001@sashiko-benchmark");
        assert_eq!(case.annotations.len(), 2);
        assert!(case.mbox_content.contains("Message-ID: <case-001@sashiko-benchmark>"));
        assert!(case.mbox_content.contains("fbnic"));
    }
}
