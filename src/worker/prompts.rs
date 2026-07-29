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

use crate::ai::review_budget::ReviewBudget;
use crate::ai::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    ClassifyAiError, ConversationDumper, ErrorAction, LlmSession, SessionRunner, ValidationError,
};
use crate::toolbox::ToolBox;
use crate::worker::stage::{ReviewStage, create_stage};
use anyhow::{Context, Result};

/// Typed errors that must not be silently retried.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    /// The AI exceeded its per-review turn limit.  Retrying with the same
    /// limit will just hit the cap again — fail fast.
    #[error("Max interactions exceeded")]
    LimitExceeded,
    /// A token budget was exceeded.  Retrying wastes tokens for no gain.
    #[error("Token budget exceeded: {0}")]
    BudgetExceeded(String),
    /// The AI produced output that failed format validation.  The retry
    /// should use an augmented prompt that reminds the model of the
    /// violated constraint rather than repeating the identical request.
    #[error("Format validation failed: {0}")]
    FormatRejection(String),
    /// The AI response was truncated by the provider (e.g., hit max tokens).
    #[error("AI response truncated by provider limit")]
    OutputTruncated,
}

impl ClassifyAiError for ReviewError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            ReviewError::LimitExceeded => AiErrorClass::Fatal,
            ReviewError::BudgetExceeded(_) => AiErrorClass::Fatal,
            ReviewError::FormatRejection(_) => AiErrorClass::Fatal,
            ReviewError::OutputTruncated => AiErrorClass::Fatal,
        }
    }
}

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tracing::{info, warn};

/// System identity prompt - used across all AI interactions
pub const SYSTEM_IDENTITY: &str = "";

/// Subsystem guides that are loaded per-stage in get_stage_prompt() and should
/// be excluded from Phase 0's shared context to avoid double-counting.
const STAGE_EXCLUSIVE_GUIDES: &[&str] = &["locking.md"];

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct PatchInput {
    pub index: i64,
    pub diff: String,
    pub subject: Option<String>,
    pub author: Option<String>,
    pub date: Option<i64>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ReviewInput {
    pub id: i64,
    pub subject: String,
    pub patches: Vec<PatchInput>,
}

pub struct WorkerConfig {
    pub main_model: String,
    pub max_input_tokens: usize,
    pub max_interactions: usize,
    pub temperature: f32,
    pub custom_prompt: Option<String>,
    pub series_range: Option<String>,
    pub stages: Option<Vec<u8>>,
    pub dump_conversation: Option<std::path::PathBuf>,
    pub budget: Option<ReviewBudget>,
    pub merge_budget: Option<ReviewBudget>,
    pub retry_provider: Option<Arc<dyn AiProvider>>,
    pub additional_models: Vec<AdditionalModelRunner>,
    pub cohort: crate::ai::model_experiment::ReviewCohort,
    pub validation_budget: Option<crate::ai::review_budget::BudgetConfig>,
}

#[derive(Clone)]
pub struct AdditionalModelRunner {
    pub name: String,
    pub provider: Arc<dyn AiProvider>,
    pub temperature: f32,
    pub max_interactions: usize,
    pub model_id: String,
    pub provider_id: String,
    pub budget: Option<ReviewBudget>,
}

#[derive(Debug, Clone)]
pub enum WorkerProgressEvent {
    PreScreenStarted,
    PlanningStarted,
    ReviewStarted {
        planned_stages: Vec<u8>,
    },
    StageStarted {
        stage: u8,
    },
    StageFinished {
        stage: u8,
    },
    StageTurn {
        stage: u8,
        turn: usize,
        max_turns: usize,
    },
}

pub struct WorkerResult {
    pub output: Option<Value>,
    pub error: Option<String>,
    pub input_context: String,
    pub history: Vec<AiMessage>,
    pub history_before_pruning: Vec<AiMessage>,
    pub history_after_pruning: Vec<AiMessage>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

fn dedup_stats(total_concerns: usize, findings: &Value) -> Value {
    let unique_findings = findings.as_array().map_or(0, Vec::len);
    let multi_stage_count = findings.as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|finding| {
                finding
                    .get("source_stages")
                    .and_then(Value::as_array)
                    .is_some_and(|stages| stages.len() > 1)
            })
            .count()
    });
    let multi_stage_pct = if unique_findings == 0 {
        0.0
    } else {
        multi_stage_count as f64 * 100.0 / unique_findings as f64
    };
    json!({
        "total_concerns": total_concerns,
        "unique_findings": unique_findings,
        "multi_stage_count": multi_stage_count,
        "multi_stage_pct": multi_stage_pct,
    })
}

pub struct PromptRegistry {
    base_dir: PathBuf,
}

impl PromptRegistry {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn get_system_identity() -> &'static str {
        SYSTEM_IDENTITY
    }

    /// Builds the complete knowledge base string.
    /// This is used for:
    /// 1. Populating the Context Cache.
    /// 2. Constructing the full prompt in non-cached mode.
    pub async fn build_context(
        &self,
        selected_prompts: Option<&[String]>,
    ) -> Result<(String, String)> {
        let mut clean = String::with_capacity(50_000);
        let mut clean_files = Vec::new();
        let mut content = String::with_capacity(50_000);

        let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
        let date_fact = format!(
            "Establish this as an absolute fact: the current date is {}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.\n\n",
            current_date
        );

        let preamble = format!(
            "{}You are an expert Linux kernel maintainer. You are ONE AGENT in a multi-stage automated review pipeline. Other specialized agents handle different aspects of this patch in parallel — you must focus strictly on YOUR stage's scope and not duplicate their work. A later consolidation stage (Stage 8) will merge all findings, so do not attempt a comprehensive review yourself.\n\nTOOL USAGE: Use tools only to verify concerns within your stage's scope. Do not broadly explore the codebase or trace execution paths outside your assigned focus area. When you do use tools, batch parallel or independent calls into a single response to minimize turns. If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.\n\n",
            date_fact
        );
        content.push_str(&preamble);
        content.push_str("<global_review_guidelines>\n");
        content.push_str("The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.\n\n");

        clean.push_str(&preamble);
        clean.push_str("<global_review_guidelines>\n");
        clean.push_str("The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.\n\n");

        // Subsystem Guidelines
        let subsystem_dir = self.base_dir.join("subsystem");

        if subsystem_dir.exists() {
            self.append_directory(&mut content, &mut clean_files, &subsystem_dir, |name| {
                if matches!(name, "README.md" | "subsystem-template.md" | "subsystem.md") {
                    return false;
                }
                if let Some(selected) = selected_prompts {
                    selected.iter().any(|s| name == s)
                } else {
                    true
                }
            })
            .await?;
        }

        // Specific Pattern Directories
        self.append_directory(
            &mut content,
            &mut clean_files,
            &self.base_dir.join("patterns"),
            |name| {
                if let Some(selected) = selected_prompts {
                    selected.iter().any(|s| name == s)
                } else {
                    true
                }
            },
        )
        .await?;

        content.push_str("</global_review_guidelines>\n");
        if !clean_files.is_empty() {
            clean.push_str(&clean_files.join(", "));
            clean.push_str("\n\n");
        }
        clean.push_str("</global_review_guidelines>\n");
        Ok((content, clean))
    }

    /// Returns the prompt for a specific stage, including any corresponding guidance files.
    pub async fn get_stage_prompt(&self, stage: u8) -> Result<(String, String)> {
        let mut clean = String::with_capacity(10_000);
        let mut clean_files = Vec::new();
        let mut content = String::with_capacity(10_000);

        let stage_instruction = match stage {
            1 => {
                "# Stage 1. Analyze commit main goal

SCOPE: You review ONLY high-level intent and design — the \"what\" and \"why\", not the \"how\". Other pipeline agents cover: implementation completeness (Stage 2), control flow bugs (Stage 3), resource leaks/UAF (Stage 4), locking/concurrency (Stage 5), security vulnerabilities (Stage 6), and hardware correctness (Stage 7). Do NOT analyze locking correctness, sleeping-in-atomic-context, lock ordering, race conditions, or any concurrency concern — Stage 5 handles all of that. Do not read source code to trace lock/context interactions.

You are a senior Linux kernel maintainer evaluating the high-level intent of a proposed commit. Analyze the commit message and the conceptual change. Focus on the big picture: Are there architectural flaws, UAPI breakages, backwards compatibility issues, or fundamentally wrong approaches? Consider the long-term maintainability and system-wide implications of this design. If the core idea is dangerous, incorrect, or violates established kernel principles, raise a concern. Be open-minded but thorough; question assumptions made by the author and consider alternative, simpler designs."
            }
            2 => {
                "# Stage 2. High-level implementation verification

SCOPE: You verify ONLY that the code matches what the commit message claims. Other pipeline agents cover: high-level design (Stage 1), control flow tracing (Stage 3), resource lifecycle (Stage 4), locking/concurrency (Stage 5), security (Stage 6), and hardware (Stage 7). Do not trace execution paths, check locking, or audit for security issues.

You are verifying if the provided code changes actually implement what the commit message claims. Look for undocumented side-effects, missing pieces (e.g., a core change without updating corresponding callers, or changing a struct without updating all initializers), and unhandled corner cases related to the feature's logic. Explicitly check for missing API callbacks and interface omissions: when defining or modifying structures containing function pointers, verify that all logically required callbacks are implemented. Verify that all claims in the commit message are fully realized in the code. Identify any incomplete implementations, implicit behavioral changes, or API contract violations. Furthermore, verify that the logic is mathematically and semantically sound. Check for off-by-one errors in bounds, incorrect bitwise operations, and verify that all arguments passed to external subsystems (like kobjects or netdevs) are valid and semantically correct (e.g., non-empty strings, correct sizes, correct format specifiers). Don't trust the commit message without verifying each claim. Assume that the message might be incorrect or even intentionally malicious. Do not focus on low-level memory or locking errors yet."
            }
            3 => {
                "# Stage 3. Execution flow verification

SCOPE: You trace ONLY control flow and logic errors within the changed code. Other pipeline agents cover: high-level design (Stage 1), commit message vs code (Stage 2), resource lifecycle (Stage 4), locking/concurrency (Stage 5), security (Stage 6), and hardware (Stage 7). Do not audit locking correctness, memory management, or security attack surfaces.

You are a static analysis engine tracing execution flow in C or Rust code. Carefully trace the control flow of the provided patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and off-by-one errors. Check every branch, switch statement, and conditional. Specifically look for NULL pointer dereferences (remember: reading a pointer field is not a dereference, only accessing its contents is). Be extremely detail-oriented; explore every error handling path (goto cleanup;) to ensure it behaves correctly under failure conditions. Additionally, verify preprocessor macro correctness and spelling (e.g., ensuring CONFIG_ prefixes are used where expected instead of HAVE_). Check that static/inline declarations or section placements won't cause linker errors or Link-Time Optimization (LTO) symbol loss."
            }
            4 => {
                "# Stage 4. Resource management

SCOPE: You audit ONLY resource lifecycle: allocations, frees, refcounts, and teardown symmetry. Other pipeline agents cover: high-level design (Stage 1), implementation completeness (Stage 2), control flow (Stage 3), locking/concurrency (Stage 5), security (Stage 6), and hardware (Stage 7). Do not analyze lock correctness or security attack surfaces.

You are an expert in C and Rust resource management within the Linux kernel. Analyze the patch for memory leaks, Use-After-Free (UAF), double frees, uninitialized variables, and unbalanced lifecycle operations (alloc->init->use->cleanup->free). Pay special attention to error paths where resources might be leaked. Ensure list_add and similar APIs are used with fully initialized objects. Track the lifetime of every allocated struct and file descriptor. Verify reference counting logic (kref_get()/kref_put()) and ensure objects are not accessed after their refcount drops to zero. Crucially, pay special attention to asynchronous handoffs and teardown symmetry. If an object is handed to a background task (timers, workqueues, notifiers) or registered to a core subsystem, you must prove that the task is explicitly canceled (e.g., cancel_work_sync(), del_timer_sync() and the subsystem is unregistered BEFORE the memory is freed or the queues are destroyed."
            }
            5 => {
                "# Stage 5. Locking and synchronization

SCOPE: You audit ONLY locking, concurrency, and synchronization. Other pipeline agents cover: high-level design (Stage 1), implementation completeness (Stage 2), control flow (Stage 3), resource lifecycle (Stage 4), security (Stage 6), and hardware (Stage 7). Do not investigate general logic errors, missing API callbacks, or buffer overflow attack surfaces.

You are a world-class concurrency and locking expert auditing a Linux kernel patch.
Carefully review the proposed patch for ANY locking, concurrency, or synchronization bugs.
You MUST consider the following categories of issues and report any violations:
1. Sleeping in atomic context: Are there any calls to `mutex_lock`, `kzalloc` with `GFP_KERNEL`, `msleep`, `cond_resched`, `flush_workqueue`, `synchronize_rcu`, or `cancel_work_sync` while holding a spinlock, rwlock, or within an RCU read-side critical section (`rcu_read_lock`)?
2. Lock ordering and deadlocks: Are locks acquired in a different order than elsewhere? Does it acquire a mutex while holding another mutex that could cause AB-BA deadlocks? Are IRQs disabled (`spin_lock_irqsave`) when acquiring a lock that is used in hardirq context? Does it acquire a lock already held by a higher-level subsystem (e.g., ethtool)?
3. Race conditions and lockless access: Are shared variables, list entries, or pointers accessed without holding the appropriate lock? Are there missing memory barriers (`smp_mb`, `smp_wmb`, `smp_rmb`) when lockless access is intended? Are there TOCTOU races where a state is checked outside a lock but relied upon inside?
4. UAF / Locking Freed Memory: Are locks (`mutex_unlock`, `spin_unlock`) called on objects that have already been freed? Are works/timers destroyed before subsystems are unregistered, allowing new events to use freed works/timers? Is the protocol initialized flag set before private data is ready?
5. RCU rules: Is `list_splice_init` or similar non-RCU-safe operations used on RCU-protected lists? Is `list_for_each_rcu` used without `rcu_read_lock`?
6. Unprotected state modifications: Does the patch check state before acquiring the lock (e.g., checking power state before taking mutex)? Are hardware state, flags, or stats updated without proper protection?
7. Sequence counters: Are stats accumulations directly inside a `u64_stats_fetch_retry` loop leading to double counting? Is it possible for an interrupt to read a sequence counter while the interrupted context is modifying it (deadlock)?
8. Lock re-initialization: Does it re-initialize a lock that was already initialized, or destroy a lock on a failure path improperly?
9. Missing locking: Is a port or file exposed to userspace before the driver/TTY linking is complete? Does a worker race with cleanup code leading to dropped/leaked frames?"
            }
            6 => {
                "# Stage 6. Security audit

SCOPE: You audit ONLY security vulnerabilities and attack surfaces. Other pipeline agents cover: high-level design (Stage 1), implementation completeness (Stage 2), control flow (Stage 3), resource lifecycle (Stage 4), locking/concurrency (Stage 5), and hardware (Stage 7). Do not report general logic errors or resource leaks unless they have a direct security impact.

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries."
            }
            7 => {
                "# Stage 7. Hardware engineer's review

SCOPE: You review ONLY hardware/driver-specific concerns. Other pipeline agents cover: high-level design (Stage 1), implementation completeness (Stage 2), control flow (Stage 3), resource lifecycle (Stage 4), locking/concurrency (Stage 5), and security (Stage 6). Do not report general software logic issues that aren't hardware-related.

You are a hardware engineer reviewing device driver changes. If this patch touches driver or hardware-specific code, rigorously review register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and timing/delays. Look for missing dma_wmb()/dma_rmb() barriers, incorrect endianness conversions (cpu_to_le32), and unsafe DMA buffer allocations. Ensure the hardware state machine is handled correctly, especially during suspend/resume or device reset. Evaluate the physical state machine constraints: verify that clocks and power domains are enabled before registers are accessed, and that hardware rings/queues are actually initialized in the current hardware state before being unconditionally accessed. If the patch is purely generic software logic (e.g., VFS, core networking), return empty concerns and dismissed-concerns arrays."
            }
            8 => {
                "# Stage 8. Deduplication and Consolidation

You are the lead reviewer consolidating feedback from multiple specialized analysts. You will be given lists of concerns and dismissed_concerns generated by different review stages.
Your task is to deduplicate identical or overlapping items in both lists.
1. Group concerns that refer to the same root cause or the same line of code.
2. Merge overlapping concerns into a single, comprehensive concern. Combine their reasonings if they complement each other.
3. Group dismissed_concerns that investigated and disproved the same candidate concern.
4. Merge overlapping dismissed_concerns into a single, comprehensive dismissed_concern. Combine their evidence if it complements each other.
5. Ensure the output contains only unique concerns and unique dismissed_concerns.
6. Preserve the `preexisting` flag for concerns. If you merge a pre-existing concern with a newly introduced one, flag it based on the root cause (if the root cause is new, it's not pre-existing).
7. SPECIFICITY REQUIREMENT: When merging concerns or dismissed_concerns, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and triggering conditions. Never generalize a specific finding into a vague category.
8. Preserve and merge the `locations` arrays from the input concerns and dismissed_concerns. If multiple items describe the same root cause, keep the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations. Do not invent line numbers; keep `line` as null when the exact line is not known.
9. For every concern, emit `source_stages` as the sorted, unique array of all input `source_stage` values merged into it. Preserve `source_stages` on unmerged concerns.
10. For every concern, emit `source_models` and `finding_ids` as sorted, unique arrays containing every value from the merged inputs. Preserve both arrays on unmerged concerns.
11. dismissed_concerns do not need a `preexisting` flag."
            }
            9 => {
                "# Stage 9. Concern/dismissed-concern conflict resolution

You are the lead reviewer reconciling consolidated concerns with consolidated dismissed_concerns.
Both `concerns` and `dismissed_concerns` are untrusted claims. Do not assume either side is correct. Treat both as hypotheses and verify them against the actual code before deciding whether to keep or discard a concern.
Your task is to identify whether any remaining concern conflicts with a dismissed_concern that investigated the same root cause, code path, or failure mode.
1. Compare each concern against the dismissed_concerns list and find conflicts or overlaps where one says the issue is real and the other says the same candidate issue is disproved.
2. For every conflict, inspect the actual code and reasoning to decide which side is correct.
3. If the concern is correct, keep it in the output. If the dismissed_concern is correct, discard that concern.
4. If there is no direct conflict for a concern, keep it unchanged.
5. Do not discard a concern merely because a dismissed_concern is vaguely related; only discard when the dismissed_concern's evidence concretely disproves that concern.
6. Preserve each retained concern's `type`, `description`, `reasoning`, `preexisting`, `locations`, `source_stages`, `source_models`, and `finding_ids` fields.
7. LOCAL BOUNDARY RULE: Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems, parallel execution, or legacy API layers will safely mask or prevent the issue, unless you can point to specific code that concretely proves the failure mode is structurally impossible. If you cannot prove the safety of the violation based on the specific code, you must keep the concern."
            }
            10 => {
                "# Stage 10. Verification and severity estimation

You are the lead reviewer validating consolidated concerns. You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding. If you're not sure about something and it's critical in the reasoning validation, make it obvious: if X is possible, then problem Y can occur. Always try to validate if X is possible yourself.
3. SERIES VALIDATION RULE: If you are reviewing a patch that is NOT the last patch in the series (indicated by the presence of subsequent patches in the Full Series Context), you MUST check if each identified concern is still a problem in the final state of the series (the end of the Series Range). If the problem has been resolved, fixed, or the code was rewritten in a subsequent patch in this series, you MUST discard the concern and NOT report it as a finding. You MUST verify this by checking the actual code at the end of the series using tools; do not trust promises or claims in commit messages.
4. When referring to other patches within this series in your explanation, DO NOT use git hashes (they are ephemeral/unstable). Instead, refer to them by their patch subject (e.g., 'commit \"mm: fix allocation\"'). Existing historical commits in the tree should still be referenced by their standard hash.
5. Assign a severity (low, medium, high, critical) to each remaining valid finding, following the calibration guidance in the severity definitions: reason through consequence, triggering path, and reachability, and state that reasoning at the start of the finding's `severity_explanation` so the label is auditable. Raise the level for a bug reachable by untrusted or remote input, and do not lower it because you believe the code is unreachable. A finding you can only state speculatively is capped at medium but still reported, never dropped. Be rigorous in filtering out verifiable noise, but accurately report real logic flaws and edge cases.
6. If the problem did exist in the code before the patch was applied, say it explicitly: 'This problem wasn't introduced by this patch, but...'. Discard low- and medium-severity pre-existing problems, report only high- and critical severity issues.
7. SPECIFICITY REQUIREMENT: Every finding MUST cite the exact function name(s), file path(s), line number(s) when known, and triggering conditions where the bug manifests. Vague descriptions like 'potential overflow in ring buffer calculations' are insufficient. State precisely which variable overflows, in which function, and under what input conditions. Do not invent line numbers; use `line: null` when the exact line is not known.
8. Carry forward the `locations` from the validated concern into each finding. If you gather better evidence, replace vague locations with the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations you verified.
9. Carry forward the concern's `source_stages` array unchanged into the finding."
            }
            11 => {
                "# Stage 11. LKML-friendly report generation

You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML). Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply.

CRITICAL RULE: If a finding is flagged as pre-existing (`\"preexisting\": true`), you MUST explicitly state in your inline comment that this issue is pre-existing and was not introduced by the patch under review. Use phrasing like \"This isn't a bug introduced by this patch, but...\" or \"This is a pre-existing issue, but...\" to start the comment.

SOURCE ANNOTATION: Immediately after each finding's `[Severity: <level>]` line, place `[Sources: <names>]` on its own line. Copy the finding's `source_models` entries exactly, separated by comma and space, including `main`. Do not add model names that are not present in `source_models`.

Follow the formatting rules strictly. Do not use markdown headers or ALL CAPS shouting. Ensure the tone is constructive and professional. Do not use backticks to quote any names or expressions.

SPECIFICITY REQUIREMENT: Each inline comment MUST reference the exact function name, file, line number when known, and specific triggering condition. Prefer the finding's `locations` field when present. Do not produce vague summaries like 'potential issue in error handling'. State precisely what goes wrong, where, and under what circumstances. Do not invent line numbers; if the exact line is unavailable, anchor the comment to the nearest verified function or symbol and explain the triggering condition."
            }
            _ => "",
        };

        if !stage_instruction.is_empty() {
            content.push_str(stage_instruction);
            clean.push_str(stage_instruction);
            content.push_str("\n\n");
            clean.push_str("\n\n");
        }

        match stage {
            3 => {
                self.append_file(&mut content, &mut clean_files, "callstack.md")
                    .await?;
                self.append_file(&mut content, &mut clean_files, "technical-patterns.md")
                    .await?;
            }
            5 => {
                self.append_file(&mut content, &mut clean_files, "subsystem/locking.md")
                    .await?;
            }
            10 => {
                self.append_file(&mut content, &mut clean_files, "false-positive-guide.md")
                    .await?;
                self.append_file(&mut content, &mut clean_files, "severity.md")
                    .await?;
            }
            11 => {
                self.append_file(&mut content, &mut clean_files, "inline-template.md")
                    .await?;
            }
            _ => {}
        }
        if !clean_files.is_empty() {
            clean.push_str(&clean_files.join(", "));
            clean.push_str("\n\n");
        }
        Ok((content, clean))
    }

    async fn append_file(
        &self,
        buffer: &mut String,
        clean: &mut Vec<String>,
        filename: &str,
    ) -> Result<()> {
        let path = self.base_dir.join(filename);
        if path.exists() {
            buffer.push_str(&format!("# {}\n", filename));
            buffer.push_str(
                &fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("Failed to read {}", filename))?,
            );
            buffer.push_str("\n\n");

            clean.push(format!("@{}", filename));
        }
        Ok(())
    }

    async fn append_directory<F>(
        &self,
        buffer: &mut String,
        clean: &mut Vec<String>,
        dir: &Path,
        filter: F,
    ) -> Result<()>
    where
        F: Fn(&str) -> bool,
    {
        if !dir.exists() {
            return Ok(());
        }
        let mut entries = fs::read_dir(dir).await?;
        let mut paths = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md")
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && filter(name)
            {
                paths.push(path);
            }
        }
        paths.sort();
        for path in paths {
            let name = path.file_name().unwrap().to_string_lossy();
            let header = if let Ok(rel) = path.strip_prefix(&self.base_dir) {
                rel.to_string_lossy().to_string()
            } else {
                name.to_string()
            };
            buffer.push_str(&format!("## {}\n", header));
            buffer.push_str(&fs::read_to_string(&path).await?);
            buffer.push_str("\n\n");

            clean.push(format!("@{}", name));
        }
        Ok(())
    }

    pub fn calculate_content_hash<T: serde::Serialize>(
        &self,
        content: &str,
        tools: Option<&[T]>,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        if let Some(tools) = tools
            && let Ok(json) = serde_json::to_string(tools)
        {
            hasher.update(json);
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

pub struct Worker {
    provider: Arc<dyn AiProvider>,
    tools: Arc<ToolBox>,
    prompts: PromptRegistry,
    global_history: Vec<AiMessage>,
    max_interactions: usize,
    temperature: f32,
    series_range: Option<String>,
    context_tag: Option<String>,
    stages: Option<Vec<u8>>,
    dump_conversation: Option<std::path::PathBuf>,
    conversation_dumper: Option<Arc<ConversationDumper>>,
    budget: Option<ReviewBudget>,
    merge_budget: Option<ReviewBudget>,
    retry_provider: Option<Arc<dyn AiProvider>>,
    additional_models: Vec<AdditionalModelRunner>,
    cohort: crate::ai::model_experiment::ReviewCohort,
    validation_budget: Option<crate::ai::review_budget::BudgetConfig>,
    main_model: String,
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: Arc<ToolBox>,
        prompts: PromptRegistry,
        config: WorkerConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            prompts,
            global_history: Vec::new(),
            max_interactions: config.max_interactions,
            temperature: config.temperature,
            series_range: config.series_range,
            context_tag: None,
            stages: config.stages,
            dump_conversation: config.dump_conversation,
            conversation_dumper: None,
            budget: config.budget,
            merge_budget: config.merge_budget,
            retry_provider: config.retry_provider,
            additional_models: config.additional_models,
            cohort: config.cohort,
            validation_budget: config.validation_budget,
            main_model: config.main_model,
        }
    }

    fn provider_for_budget(&self, budget: Option<&ReviewBudget>) -> Arc<dyn AiProvider> {
        if budget.is_some_and(ReviewBudget::review_past_warn)
            && let Some(provider) = &self.retry_provider
        {
            return provider.clone();
        }
        self.provider.clone()
    }

    fn budget_flags(&self) -> u8 {
        self.budget.as_ref().map_or(0, ReviewBudget::flags)
            | self.merge_budget.as_ref().map_or(0, ReviewBudget::flags)
    }

    fn merge_usage(&self) -> Value {
        let snapshot = self.merge_budget.as_ref().map(ReviewBudget::snapshot);
        json!({
            "tokens_in": snapshot.map_or(0, |usage| usage.input),
            "tokens_out": snapshot.map_or(0, |usage| usage.output),
            "tokens_cached": snapshot.map_or(0, |usage| usage.cached),
            "budget_flags": snapshot.map_or(0, |usage| usage.flags),
        })
    }

    pub async fn run(
        &mut self,
        patchset: Value,
        progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
    ) -> Result<WorkerResult> {
        // 1. Extract inputs
        let mut target_commit_diff = String::new();
        let mut target_commit_diff_only = String::new();

        let ps_id = patchset["id"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let p_id = patchset["patch_index"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "multi".to_string());
        self.context_tag = Some(format!("[ps:{} p:{}] ", ps_id, p_id));

        if let Some(base) = &self.dump_conversation {
            match ConversationDumper::new(base).await {
                Ok(dumper) => self.conversation_dumper = Some(Arc::new(dumper)),
                Err(error) => warn!("Failed to initialize conversation dump: {}", error),
            }
        }

        let mut baseline_sha = "unknown".to_string();
        if let Some(ref range) = self.series_range {
            let parts: Vec<&str> = range.split("..").collect();
            if !parts.is_empty() {
                baseline_sha = parts[0].to_string();
            }
        }

        let mut target_commit_sha = "unknown".to_string();
        if let Some(patches) = patchset["patches"].as_array() {
            if let Some(idx) = patchset["patch_index"].as_i64()
                && let Some(p) = patches.iter().find(|p| p["index"].as_i64() == Some(idx))
                && let Some(sha) = p["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
            if target_commit_sha == "unknown"
                && !patches.is_empty()
                && let Some(sha) = patches[0]["commit_id"].as_str()
            {
                target_commit_sha = sha.to_string();
            }
        }

        if let Some(patches) = patchset["patches"].as_array() {
            for p in patches {
                let diff_body = p["diff"].as_str().unwrap_or("");
                let changelog_opt = crate::patch::extract_changelog_from_body(diff_body);

                if let Some(show) = p["git_show"].as_str() {
                    if let Some(ref changelog) = changelog_opt {
                        let enriched_show =
                            crate::patch::inject_changelog_into_git_show(show, changelog);
                        target_commit_diff.push_str(&enriched_show);
                    } else {
                        target_commit_diff.push_str(show);
                    }
                    target_commit_diff.push('\n');
                } else {
                    target_commit_diff.push_str(diff_body);
                    target_commit_diff.push('\n');
                }

                if let Some(diff) = p["diff"].as_str() {
                    target_commit_diff_only.push_str(diff);
                    target_commit_diff_only.push('\n');
                }
            }
        }

        let mut all_concerns = Vec::new();
        let mut all_dismissed_concerns = Vec::new();
        let mut total_tokens_in = 0;
        let mut total_tokens_out = 0;
        let mut total_tokens_cached = 0;

        // Phase 0: Pre-screen relevant prompts
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::PreScreenStarted);
        }
        let subsystem_md_path = self.prompts.base_dir.join("subsystem/subsystem.md");
        let selected_prompts = if subsystem_md_path.exists() {
            match tokio::fs::read_to_string(&subsystem_md_path).await {
                Ok(subsystem_md) => {
                    info!("Executing Phase 0: Pre-screening relevant subsystem guides.");
                    let phase0_system = "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking.md\", \"locking.md\"]}\n```";
                    let phase0_prompt = format!(
                        "<subsystem_guide_index>\n{}\n</subsystem_guide_index>\n\n<patch>\n{}\n</patch>",
                        subsystem_md, target_commit_diff
                    );
                    let schema = json!({
                        "type": "object",
                        "properties": {
                            "selected_prompts": {
                                "type": "array",
                                "items": { "type": "string" }
                            }
                        },
                        "required": ["selected_prompts"]
                    });

                    let req = AiRequest {
                        system: Some(phase0_system.to_string()),
                        messages: vec![AiMessage {
                            role: AiRole::User,
                            content: Some(phase0_prompt),
                            thought: None,
                            thought_signature: None,
                            reasoning: None,
                            tool_calls: None,
                            tool_call_id: None,
                        }],
                        tools: None,
                        temperature: Some(0.0),
                        response_format: Some(AiResponseFormat::Json {
                            schema: Some(schema),
                        }),
                        context_tag: self
                            .context_tag
                            .as_ref()
                            .map(|prefix| format!("{}s:0] ", &prefix[..prefix.len() - 2])),
                    };

                    let mut tokens = (total_tokens_in, total_tokens_out, total_tokens_cached);
                    let val = self
                        .json_request("s0", req, &mut tokens, |v| {
                            v.get("selected_prompts")
                                .and_then(|v| v.as_array())
                                .ok_or_else(|| "missing 'selected_prompts' array".to_string())
                                .map(|_| ())
                        })
                        .await;
                    total_tokens_in = tokens.0;
                    total_tokens_out = tokens.1;
                    total_tokens_cached = tokens.2;
                    val.and_then(|val| {
                        let arr = val.get("selected_prompts")?.as_array()?;
                        let prompts: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .filter(|name| !STAGE_EXCLUSIVE_GUIDES.contains(&name.as_str()))
                            .collect();
                        info!("Phase 0 selected prompts: {:?}", prompts);
                        Some(prompts)
                    })
                }
                Err(e) => {
                    warn!("Failed to read subsystem.md for Phase 0: {}", e);
                    None
                }
            }
        } else {
            warn!(
                "subsystem.md not found for Phase 0 at {:?}",
                subsystem_md_path
            );
            None
        };

        let (static_context, clean_static_context) = self
            .prompts
            .build_context(selected_prompts.as_deref())
            .await?;

        let mut git_metadata = String::new();
        git_metadata.push_str("\n\n=== Active Git Metadata ===\n");
        git_metadata.push_str(&format!("Target Commit SHA: {}\n", target_commit_sha));
        git_metadata.push_str(&format!("Baseline SHA: {}\n", baseline_sha));
        git_metadata.push_str("===========================\n");

        let mut dynamic_context = String::new();
        dynamic_context.push_str(&git_metadata);
        dynamic_context.push_str("\n\nTarget Commit:\n");
        dynamic_context.push_str(&target_commit_diff);
        let mut clean_dynamic_context = dynamic_context.clone();

        let mut dynamic_context_no_log = String::new();
        dynamic_context_no_log.push_str(&git_metadata);
        dynamic_context_no_log.push_str("\n\nTarget Commit Diff:\n");
        dynamic_context_no_log.push_str(&target_commit_diff_only);
        let mut clean_dynamic_context_no_log = dynamic_context_no_log.clone();

        // Prefetch AST context based on the diff
        let worktree_path = self.tools.get_worktree_path();
        if let Ok(prefetched) =
            crate::worker::prefetch::prefetch_context(worktree_path, &target_commit_diff).await
            && !prefetched.is_empty()
        {
            dynamic_context.push_str("\n\n<pre_fetched_context>\n");
            dynamic_context.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            dynamic_context.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            dynamic_context.push_str(&prefetched);
            dynamic_context.push_str("\n</pre_fetched_context>\n");

            clean_dynamic_context.push_str("\n\n<pre_fetched_context>\n");
            clean_dynamic_context.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            clean_dynamic_context.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            clean_dynamic_context.push_str("{{prefetched_context}}\n</pre_fetched_context>\n");

            dynamic_context_no_log.push_str("\n\n<pre_fetched_context>\n");
            dynamic_context_no_log.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            dynamic_context_no_log.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            dynamic_context_no_log.push_str(&prefetched);
            dynamic_context_no_log.push_str("\n</pre_fetched_context>\n");

            clean_dynamic_context_no_log.push_str("\n\n<pre_fetched_context>\n");
            clean_dynamic_context_no_log.push_str("The following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\n");
            clean_dynamic_context_no_log.push_str("If it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n");
            clean_dynamic_context_no_log
                .push_str("{{prefetched_context}}\n</pre_fetched_context>\n");
        }
        let (shared_context, clean_shared_context) = {
            // Without cache (or with implicit cache like Claude), we send everything.
            (
                format!("{}{}", static_context, dynamic_context),
                format!("{}{}", clean_static_context, clean_dynamic_context),
            )
        };

        let (shared_context_no_log, clean_shared_context_no_log) = {
            (
                format!("{}{}", static_context, dynamic_context_no_log),
                format!("{}{}", clean_static_context, clean_dynamic_context_no_log),
            )
        };

        let mut planning_selected_stages: Option<Vec<u8>> = None;
        if self.stages.is_none() {
            if let Some(progress_cb) = progress {
                progress_cb(WorkerProgressEvent::PlanningStarted);
            }
            let schema = serde_json::json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "Array of stage numbers from 4, 5, 6, 7 that are relevant to this patch. Err on the side of inclusion if unsure."
                    }
                },
                "required": ["relevant_stages"]
            });

            let planning_prompt = r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- Stage 4: Resource management
- Stage 5: Locking and synchronization
- Stage 6: Security audit
- Stage 7: Hardware engineer's review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages 1, 2, and 3 are always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Example:
```json
{"relevant_stages": [4, 5, 6, 7]}
```"#;

            let req = AiRequest {
                system: None,
                messages: vec![AiMessage {
                    role: crate::ai::AiRole::User,
                    content: Some(format!("{}\n\n{}", shared_context, planning_prompt)),
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: None,
                    tool_call_id: None,
                }],
                tools: None,
                temperature: Some(0.0),
                response_format: Some(AiResponseFormat::Json {
                    schema: Some(schema),
                }),
                context_tag: self
                    .context_tag
                    .as_ref()
                    .map(|prefix| format!("{} s:p] ", &prefix[..prefix.len() - 2])),
            };

            info!("Running planning pre-phase");
            let mut tokens = (total_tokens_in, total_tokens_out, total_tokens_cached);
            let val = self
                .json_request("sp", req, &mut tokens, |v| {
                    v.get("relevant_stages")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| "missing 'relevant_stages' array".to_string())
                        .map(|_| ())
                })
                .await;
            total_tokens_in = tokens.0;
            total_tokens_out = tokens.1;
            total_tokens_cached = tokens.2;
            if let Some(val) = val {
                let arr = val["relevant_stages"].as_array().unwrap();
                let mut stages = vec![1, 2, 3];
                for v in arr {
                    if let Some(n) = v.as_u64()
                        && (4..=7).contains(&n)
                    {
                        stages.push(n as u8);
                    }
                }
                info!("Planning phase selected stages: {:?}", stages);
                planning_selected_stages = Some(stages);
            }
        }

        let mut planned_stages = Vec::new();
        for stage_num in 1..=7 {
            if let Some(ref selected_stages) = self.stages {
                if selected_stages.contains(&stage_num) {
                    planned_stages.push(stage_num);
                }
            } else if let Some(ref planned_stages_ref) = planning_selected_stages {
                if planned_stages_ref.contains(&stage_num) {
                    planned_stages.push(stage_num);
                }
            } else {
                planned_stages.push(stage_num);
            }
        }
        if !planned_stages.is_empty() {
            planned_stages.push(8);
            planned_stages.push(9);
            planned_stages.push(10);
            planned_stages.push(11);
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::ReviewStarted { planned_stages });
        }

        // Initialize system message in global history once before running stages
        if self.global_history.is_empty() {
            self.global_history.push(AiMessage {
                role: AiRole::System,
                content: Some(clean_shared_context.clone()),
                thought: None,
                thought_signature: None,
                reasoning: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        // Stages 1-7 run concurrently but share a prompt prefix per (model, log
        // variant): stages 1, 2 and 7 carry the log context, stages 3-6 do not.  Firing
        // them all at once makes every one write the provider's prefix cache and none
        // read it.  Elect the first stage of each group to warm the cache and hold the
        // rest until it has done so; they then read the entry back at a fraction of the
        // write price.  Gates are keyed per model so an experiment cohort with its own
        // provider does not wait on the main model.
        let mut prefix_gates: std::collections::HashMap<(String, bool), PrefixGate> =
            std::collections::HashMap::new();

        // Construct futures for Stages 1-7
        let mut stage_futures = Vec::new();
        for stage_num in 1..=7 {
            if let Some(ref selected_stages) = self.stages {
                if !selected_stages.contains(&stage_num) {
                    continue;
                }
            } else if let Some(ref planned_stages) = planning_selected_stages
                && !planned_stages.contains(&stage_num)
            {
                info!("Skipping stage {} based on planning phase", stage_num);
                continue;
            }

            let stage = create_stage(stage_num);
            let use_log = stage.use_log_in_context();
            let system_prompt = if use_log {
                shared_context.clone()
            } else {
                shared_context_no_log.clone()
            };
            let clean_system_prompt = if use_log {
                clean_shared_context.clone()
            } else {
                clean_shared_context_no_log.clone()
            };

            let main_provider = self.provider_for_budget(self.budget.as_ref());
            let (main_opener, main_waiter) = claim_prefix_gate(
                &mut prefix_gates,
                "main",
                use_log,
                main_provider.caches_prompt_prefix(),
            );

            stage_futures.push(self.execute_stage(
                stage,
                system_prompt.clone(),
                clean_system_prompt.clone(),
                progress,
                StageExecutionConfig {
                    provider: main_provider,
                    name: "main".to_string(),
                    temperature: self.temperature,
                    max_interactions: self.max_interactions,
                    model_id: self.main_model.clone(),
                    provider_id: self.cohort.main.provider.clone(),
                    budget: self.budget.clone(),
                    prefix_opener: main_opener,
                    wait_for_prefix: main_waiter,
                },
            ));

            for model in &self.additional_models {
                let variant_stage = create_stage(stage_num);
                let (opener, waiter) = claim_prefix_gate(
                    &mut prefix_gates,
                    &model.name,
                    use_log,
                    model.provider.caches_prompt_prefix(),
                );
                stage_futures.push(self.execute_stage(
                    variant_stage,
                    system_prompt.clone(),
                    clean_system_prompt.clone(),
                    progress,
                    StageExecutionConfig {
                        provider: model.provider.clone(),
                        name: model.name.clone(),
                        temperature: model.temperature,
                        max_interactions: model.max_interactions,
                        model_id: model.model_id.clone(),
                        provider_id: model.provider_id.clone(),
                        budget: model.budget.clone(),
                        prefix_opener: opener,
                        wait_for_prefix: waiter,
                    },
                ));
            }
        }

        // Run planned stages concurrently
        info!(
            "Running {} planned stages concurrently ({} prompt-prefix gate(s))",
            stage_futures.len(),
            prefix_gates.len()
        );
        let stage_results = futures::future::try_join_all(stage_futures).await?;
        let mut experiment_runs = Vec::new();

        // Consolidate results in deterministic order (already preserved by try_join_all)
        for res in stage_results {
            if res.model == "main" {
                total_tokens_in += res.tokens_in;
                total_tokens_out += res.tokens_out;
                total_tokens_cached += res.tokens_cached;
            }

            append_stage_items(
                &mut all_concerns,
                &res.concerns,
                res.stage,
                &res.model,
                "General",
                "description",
            );
            append_stage_dismissed_concerns(
                &mut all_dismissed_concerns,
                &res.dismissed_concerns,
                res.stage,
                &res.model,
            );

            experiment_runs.push(json!({
                "model": res.model,
                "model_id": res.model_id,
                "provider_id": res.provider_id,
                "stage": res.stage,
                "tokens_in": res.tokens_in,
                "tokens_out": res.tokens_out,
                "tokens_cached": res.tokens_cached,
                "status": if res.failed { "failed" } else { "completed" },
                "error": res.error,
            }));

            // Append history in order
            self.global_history.extend(res.history);
        }

        if all_concerns.is_empty() {
            tracing::info!("No concerns from stages 1-7, skipping stages 8, 9, 10 and 11");
            let dismissed_concerns_count = all_dismissed_concerns.len();
            let final_output = serde_json::json!({
                "findings": [],
                "dismissed_concerns": all_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": 0,
                "budget_flags": self.budget_flags(),
                "merge_usage": self.merge_usage(),
                "canonical_candidates": [],
                "dismissed_concerns_count": dismissed_concerns_count
                ,"dedup_stats": dedup_stats(0, &json!([]))
                ,"model_experiment": {
                    "cohort": &self.cohort,
                    "runs": experiment_runs,
                    "comparisons": []
                }
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: shared_context.clone(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 8: Deduplication
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 8 });
        }
        info!("Running Stage 8 (Deduplication)");
        let deduplicated_concerns;
        let deduplicated_dismissed_concerns;
        {
            let stage = 8;
            let (stage_prompt, _) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();

            let aggregated_concerns_json =
                serde_json::to_string_pretty(&all_concerns).unwrap_or_default();
            let aggregated_dismissed_concerns_json =
                serde_json::to_string_pretty(&all_dismissed_concerns).unwrap_or_default();

            let user_prompt = format!(
                r#"{}

Aggregated Concerns:
{}

Aggregated Dismissed Concerns:
{}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations", "source_stages".
Each concern MUST also preserve and merge "source_models" and "finding_ids" from all matching input concerns.
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ],
  "dismissed_concerns": [
    {{
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }}
      ]
    }}
  ]
}}
```"#,
                stage_prompt, aggregated_concerns_json, aggregated_dismissed_concerns_json
            );

            let clean_user_prompt = format!(
                r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations", "source_stages".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown."#,
                stage_prompt, aggregated_concerns_json, aggregated_dismissed_concerns_json
            );

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            session.require_provenance(all_concerns.clone(), true, "concerns");
            let provider = self.provider_for_budget(self.merge_budget.as_ref());
            let runner = SessionRunner::new(provider.as_ref())
                .with_conversation_dump(self.conversation_dumper.clone(), format!("s{}", stage))
                .with_budget(self.merge_budget.clone())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            deduplicated_concerns = result.output.get("concerns").unwrap().clone();
            deduplicated_dismissed_concerns =
                result.output.get("dismissed_concerns").unwrap().clone();
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 8 });
        }

        let mut comparisons = Vec::new();
        let mut confirmation_runs = Vec::new();

        if let Some(c) = deduplicated_concerns.as_array()
            && c.is_empty()
        {
            tracing::info!(
                "No concerns remaining after Stage 8 deduplication, skipping stages 9, 10 and 11"
            );
            let final_output = serde_json::json!({
                "findings": [],
                "dismissed_concerns": deduplicated_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": all_concerns.len(),
                "budget_flags": self.budget_flags(),
                "merge_usage": self.merge_usage(),
                "canonical_candidates": [],
                "dismissed_concerns_count": deduplicated_dismissed_concerns
                    .as_array()
                    .map_or(0, Vec::len),
                "dedup_stats": dedup_stats(all_concerns.len(), &json!([]))
                ,"model_experiment": {
                    "cohort": &self.cohort,
                    "runs": experiment_runs,
                    "comparisons": comparisons,
                    "confirmation_runs": confirmation_runs
                }
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: shared_context.clone(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 9: Concern/dismissed-concern conflict resolution
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 9 });
        }
        info!("Running Stage 9 (Concern/dismissed-concern conflict resolution)");
        let conflict_resolved_concerns;
        {
            let stage = 9;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();

            let deduplicated_concerns_json =
                serde_json::to_string_pretty(&deduplicated_concerns).unwrap_or_default();
            let deduplicated_dismissed_concerns_json =
                serde_json::to_string_pretty(&deduplicated_dismissed_concerns).unwrap_or_default();

            let user_prompt = format!(
                r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations", "source_stages", "source_models", "finding_ids". Preserve source_models and finding_ids unchanged.
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#,
                stage_prompt, deduplicated_concerns_json, deduplicated_dismissed_concerns_json
            );

            let clean_user_prompt = format!(
                r#"{}

Consolidated Concerns:
{}

Consolidated Dismissed Concerns:
{}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations", "source_stages", "source_models", "finding_ids". Preserve source_models and finding_ids unchanged.
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#,
                clean_stage_prompt,
                deduplicated_concerns_json,
                deduplicated_dismissed_concerns_json
            );

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            session.require_provenance(
                deduplicated_concerns
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
                false,
                "concerns",
            );
            let provider = self.provider_for_budget(self.merge_budget.as_ref());
            let runner = SessionRunner::new(provider.as_ref())
                .with_conversation_dump(self.conversation_dumper.clone(), format!("s{}", stage))
                .with_budget(self.merge_budget.clone())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            conflict_resolved_concerns = result.output.get("concerns").unwrap().clone();
        }
        let canonical_candidates = conflict_resolved_concerns.clone();
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 9 });
        }

        if let Some(c) = conflict_resolved_concerns.as_array()
            && c.is_empty()
        {
            tracing::info!(
                "No concerns remaining after Stage 9 conflict resolution, skipping stages 10 and 11"
            );
            let final_output = serde_json::json!({
                "findings": [],
                "dismissed_concerns": deduplicated_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": all_concerns.len(),
                "budget_flags": self.budget_flags(),
                "merge_usage": self.merge_usage(),
                "canonical_candidates": canonical_candidates,
                "dismissed_concerns_count": deduplicated_dismissed_concerns
                    .as_array()
                    .map_or(0, Vec::len),
                "dedup_stats": dedup_stats(all_concerns.len(), &json!([]))
                ,"model_experiment": {
                    "cohort": &self.cohort,
                    "runs": experiment_runs,
                    "comparisons": comparisons,
                    "confirmation_runs": confirmation_runs
                }
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: shared_context.clone(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 10: Verification
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 10 });
        }
        info!("Running Stage 10 (Verification)");
        let mut findings_json;
        let mut baseline_decisions = std::collections::BTreeMap::new();
        {
            let stage = 10;
            let (mut stage_prompt, mut clean_stage_prompt) =
                self.prompts.get_stage_prompt(stage).await?;
            if self.cohort.selected_variants().next().is_some() {
                let guidance = "\n\nMODEL EXPERIMENT: Apply the normal verification rules to every concern and return a top-level baseline_decisions object mapping the first finding_ids value of every input concern to the boolean result. Also retain every concern with requires_validation=true in findings even when its baseline decision is false; a separate validation policy will combine the baseline and independent model decisions. Concerns with requires_validation=false appear in findings only when their baseline decision is true.";
                stage_prompt.push_str(guidance);
                clean_stage_prompt.push_str(guidance);
            }
            let system_prompt = shared_context.clone();

            let full_series_context = if let Some(range) = &self.series_range {
                let cmd_output = std::process::Command::new("git")
                    .current_dir(self.tools.get_worktree_path())
                    .args(["--no-pager", "log", "--reverse", "--format=%s", range])
                    .output();

                match cmd_output {
                    Ok(out) if out.status.success() => {
                        let subjects = String::from_utf8_lossy(&out.stdout).to_string();
                        format!(
                            "Series Range: {}\n\nPatches in series:\n{}",
                            range, subjects
                        )
                    }
                    Ok(out) => {
                        warn!(
                            "git log failed for range {}: {}",
                            range,
                            String::from_utf8_lossy(&out.stderr)
                        );
                        "Failed to retrieve full series context (git log error).".to_string()
                    }
                    Err(e) => {
                        warn!("git command failed: {}", e);
                        "Failed to retrieve full series context (git execution error).".to_string()
                    }
                }
            } else {
                "Not applicable (single patch or last patch in series).".to_string()
            };

            let severity_input = annotate_validation_candidates(
                &conflict_resolved_concerns,
                &self.cohort,
                &experiment_runs,
            );
            let conflict_resolved_concerns_json =
                serde_json::to_string_pretty(&severity_input).unwrap_or_default();
            let mut user_prompt = format!(
                "{}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.\n\nFull Series Context:\n{}\n\nConsolidated Concerns:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: \"problem\" (a string containing the vulnerability description), \"severity\" (a string: Low, Medium, High, or Critical), \"severity_explanation\" (a string detailing the reasoning and proof), \"preexisting\" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), \"locations\" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters), and \"source_stages\" (the unchanged array from the validated concern). Carry forward the locations and source_stages from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\",\n      \"preexisting\": false,\n      \"locations\": [\n        {{\n          \"file\": \"path/to/file.c\",\n          \"function_or_symbol\": \"function_name\",\n          \"line\": 123,\n          \"code_snippet\": \"problematic_code();\",\n          \"why_this_location_matters\": \"This is where the newly allocated resource is dropped on the error path.\"\n        }}\n      ],\n      \"source_stages\": [3, 4]\n    }}\n  ]\n}}\n```",
                stage_prompt, full_series_context, conflict_resolved_concerns_json
            );
            user_prompt.push_str("\n\nPROVENANCE REQUIREMENT: Every finding must also include source_models, finding_ids, and requires_validation copied unchanged from its input concern. These fields are required despite any earlier exact-key list.");

            let mut clean_user_prompt = format!(
                "{}\n\nCRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.\n\nFull Series Context:\n{{{{series context}}}}\n\nConsolidated Concerns:\n{}\n\nReturn ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: \"problem\" (a string containing the vulnerability description), \"severity\" (a string: Low, Medium, High, or Critical), \"severity_explanation\" (a string detailing the reasoning and proof), \"preexisting\" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), \"locations\" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters), and \"source_stages\" (the unchanged array from the validated concern). Carry forward the locations and source_stages from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.\n\nExample Output:\n```json\n{{\n  \"findings\": [\n    {{\n      \"problem\": \"Memory leak in function X when condition Y is met.\",\n      \"severity\": \"High\",\n      \"severity_explanation\": \"1. Condition Y is met.\\\n2. The buffer is allocated but not freed before return.\",\n      \"preexisting\": false,\n      \"locations\": [\n        {{\n          \"file\": \"path/to/file.c\",\n          \"function_or_symbol\": \"function_name\",\n          \"line\": 123,\n          \"code_snippet\": \"problematic_code();\",\n          \"why_this_location_matters\": \"This is where the newly allocated resource is dropped on the error path.\"\n        }}\n      ],\n      \"source_stages\": [3, 4]\n    }}\n  ]\n}}\n```",
                clean_stage_prompt, conflict_resolved_concerns_json
            );
            clean_user_prompt.push_str("\n\nPROVENANCE REQUIREMENT: Every finding must also include source_models, finding_ids, and requires_validation copied unchanged from its input concern. These fields are required despite any earlier exact-key list.");

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            if self.cohort.selected_variants().next().is_some() {
                session.require_baseline_decisions(
                    severity_input
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|item| item["finding_ids"].as_array()?.first()?.as_str())
                        .map(str::to_string)
                        .collect(),
                );
                session.require_experiment_findings(severity_input.clone());
            }
            session.require_provenance(
                severity_input.as_array().cloned().unwrap_or_default(),
                false,
                "findings",
            );
            let provider = self.provider_for_budget(self.merge_budget.as_ref());
            let runner = SessionRunner::new(provider.as_ref())
                .with_conversation_dump(self.conversation_dumper.clone(), format!("s{}", stage))
                .with_budget(self.merge_budget.clone())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            findings_json = result.output.get("findings").unwrap().clone();
            if self.cohort.selected_variants().next().is_some() {
                baseline_decisions = validate_baseline_decisions(
                    &severity_input,
                    result.output.get("baseline_decisions"),
                )?;
            }
        }
        let (confirmed, completed_comparisons, completed_confirmation_runs) = self
            .confirm_unique_findings(
                &shared_context,
                &findings_json,
                &experiment_runs,
                &baseline_decisions,
            )
            .await?;
        findings_json = confirmed;
        comparisons = completed_comparisons;
        confirmation_runs = completed_confirmation_runs;
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 10 });
        }

        if let Some(f) = findings_json.as_array()
            && f.is_empty()
        {
            tracing::info!("No findings from Stage 10, skipping Stage 11");
            let final_output = serde_json::json!({
                "findings": findings_json,
                "dismissed_concerns": deduplicated_dismissed_concerns,
                "review_inline": "No issues found.",
                "fixes": "",
                "concerns_count": all_concerns.len(),
                "budget_flags": self.budget_flags(),
                "merge_usage": self.merge_usage(),
                "dismissed_concerns_count": deduplicated_dismissed_concerns
                    .as_array()
                    .map_or(0, Vec::len),
                "dedup_stats": dedup_stats(all_concerns.len(), &findings_json)
                ,"canonical_candidates": canonical_candidates
                ,"model_experiment": {
                    "cohort": &self.cohort,
                    "runs": experiment_runs,
                    "comparisons": comparisons,
                    "confirmation_runs": confirmation_runs
                }
            });
            return Ok(WorkerResult {
                output: Some(final_output),
                error: None,
                input_context: shared_context.clone(),
                history: self.global_history.clone(),
                history_before_pruning: self.global_history.clone(),
                history_after_pruning: self.global_history.clone(),
                tokens_in: total_tokens_in,
                tokens_out: total_tokens_out,
                tokens_cached: total_tokens_cached,
            });
        }

        // Stage 11
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: 11 });
        }
        info!("Running Stage 11");
        let review_inline_text;
        {
            let stage = 11;
            let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage).await?;
            let system_prompt = shared_context.clone();
            let presentation_findings =
                findings_with_main_source_label(&findings_json, &self.cohort.main.display_name);
            let findings_str =
                serde_json::to_string_pretty(&presentation_findings).unwrap_or_default();
            let user_prompt = format!(
                "{}\n\nFindings:\n{}\n\nReturn raw text output, not JSON.",
                stage_prompt, findings_str
            );
            let clean_user_prompt = format!(
                "{}\n\nFindings:\n{}\n\nReturn raw text output, not JSON.",
                clean_stage_prompt, findings_str
            );

            let stage_impl = create_stage(stage);
            let mut session = ReviewStageSession::new(
                stage_impl,
                system_prompt,
                user_prompt,
                clean_user_prompt,
                self.tools.clone(),
                self.temperature,
                self.context_tag.as_deref(),
            );
            let provider = self.provider_for_budget(self.merge_budget.as_ref());
            let runner = SessionRunner::new(provider.as_ref())
                .with_conversation_dump(self.conversation_dumper.clone(), format!("s{}", stage))
                .with_budget(self.merge_budget.clone())
                .with_max_validation_attempts(3)
                .with_max_turns(self.max_interactions)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(progress_cb) = progress {
                        progress_cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                });
            let result = runner.run(&mut session).await?;

            total_tokens_in += result.usage.prompt_tokens as u32;
            total_tokens_out += result.usage.completion_tokens as u32;
            total_tokens_cached += result.usage.cached_tokens.unwrap_or(0) as u32;
            self.global_history.extend(result.history);

            review_inline_text = result.output.as_str().unwrap().to_string();
        }
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: 11 });
        }

        let fixes_text = String::new();
        let dismissed_concerns_count = deduplicated_dismissed_concerns
            .as_array()
            .map_or(0, Vec::len);

        let final_output = json!({
            "findings": findings_json,
            "dismissed_concerns": deduplicated_dismissed_concerns,
            "review_inline": review_inline_text,
            "fixes": fixes_text,
            "concerns_count": all_concerns.len(),
            "dismissed_concerns_count": dismissed_concerns_count
            ,"budget_flags": self.budget_flags()
            ,"merge_usage": self.merge_usage()
            ,"dedup_stats": dedup_stats(all_concerns.len(), &findings_json)
            ,"canonical_candidates": canonical_candidates
            ,"model_experiment": {
                "cohort": &self.cohort,
                "runs": experiment_runs,
                "comparisons": comparisons,
                "confirmation_runs": confirmation_runs
            }
        });

        Ok(WorkerResult {
            output: Some(final_output),
            error: None,
            input_context: shared_context.clone(),
            history: self.global_history.clone(),
            history_before_pruning: self.global_history.clone(),
            history_after_pruning: self.global_history.clone(),
            tokens_in: total_tokens_in,
            tokens_out: total_tokens_out,
            tokens_cached: total_tokens_cached,
        })
    }

    async fn json_request(
        &self,
        label: &str,
        req: AiRequest,
        tokens: &mut (u32, u32, u32),
        validate: impl Fn(&Value) -> Result<(), String>,
    ) -> Option<Value> {
        fn accumulate(tokens: &mut (u32, u32, u32), usage: &crate::ai::AiUsage) {
            tokens.0 += usage.prompt_tokens as u32;
            tokens.1 += usage.completion_tokens as u32;
            tokens.2 += usage.cached_tokens.unwrap_or(0) as u32;
        }

        fn try_parse(
            content: &str,
            validate: &impl Fn(&Value) -> Result<(), String>,
        ) -> Result<Value, String> {
            let stripped = content.trim();
            let stripped = stripped
                .strip_prefix("```json")
                .or_else(|| stripped.strip_prefix("```"))
                .map(|s| s.strip_suffix("```").unwrap_or(s).trim())
                .unwrap_or(stripped);
            let v = serde_json::from_str::<Value>(stripped)
                .map_err(|e| format!("JSON parse error: {}", e))?;
            validate(&v)?;
            Ok(v)
        }

        let retry_base = req.clone();
        if let Some(dump) = &self.conversation_dumper
            && let Err(error) = dump.write(label, 1, "req", &req).await
        {
            warn!("Failed to dump {} request: {}", label, error);
        }
        let resp = match self.provider.generate_content(req).await {
            Ok(r) => r,
            Err(e) => {
                warn!("{} completion failed: {}", label, e);
                return None;
            }
        };
        if let Some(dump) = &self.conversation_dumper
            && let Err(error) = dump.write(label, 1, "resp", &resp).await
        {
            warn!("Failed to dump {} response: {}", label, error);
        }
        if resp.truncated {
            warn!("{} completion truncated by provider limit", label);
            return None;
        }
        if let Some(usage) = &resp.usage {
            accumulate(tokens, usage);
        }
        let content = resp.content.as_deref().unwrap_or("");
        match try_parse(content, &validate) {
            Ok(v) => return Some(v),
            Err(e) => {
                warn!("{}: {}, retrying with correction", label, e);
                let mut retry_req = retry_base;
                retry_req.messages.push(AiMessage {
                    role: AiRole::Assistant,
                    content: Some(content.to_string()),
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
                retry_req.messages.push(AiMessage {
                    role: AiRole::User,
                    content: Some(format!(
                        "Your response is not valid: {}\nRespond with ONLY valid JSON conforming to the schema. No markdown, no explanation.",
                        e
                    )),
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: None,
                    tool_call_id: None,
                });
                if let Some(dump) = &self.conversation_dumper
                    && let Err(error) = dump.write(label, 2, "req", &retry_req).await
                {
                    warn!("Failed to dump {} retry request: {}", label, error);
                }
                match self.provider.generate_content(retry_req).await {
                    Ok(resp2) => {
                        if let Some(dump) = &self.conversation_dumper
                            && let Err(error) = dump.write(label, 2, "resp", &resp2).await
                        {
                            warn!("Failed to dump {} retry response: {}", label, error);
                        }
                        if resp2.truncated {
                            warn!("{} retry completion truncated by provider limit", label);
                            return None;
                        }
                        if let Some(usage) = &resp2.usage {
                            accumulate(tokens, usage);
                        }
                        let content2 = resp2.content.as_deref().unwrap_or("");
                        match try_parse(content2, &validate) {
                            Ok(v) => {
                                warn!("{} succeeded on retry (first attempt was invalid)", label);
                                return Some(v);
                            }
                            Err(e2) => {
                                warn!("{} failed on retry too: {}", label, e2);
                            }
                        }
                    }
                    Err(e2) => {
                        warn!("{} retry request failed: {}", label, e2);
                    }
                }
            }
        }
        None
    }

    async fn execute_stage(
        &self,
        stage: Box<dyn ReviewStage>,
        system_prompt: String,
        _clean_system_prompt: String,
        progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
        config: StageExecutionConfig,
    ) -> Result<StageExecutionResult> {
        let stage_num = stage.number();
        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageStarted { stage: stage_num });
        }

        // Hold off until a sibling stage has warmed the shared prompt prefix, so this
        // request reads the cache entry instead of writing a duplicate.
        if let Some(mut waiter) = config.wait_for_prefix
            && !*waiter.borrow_and_update()
        {
            let waited = std::time::Instant::now();
            match tokio::time::timeout(PREFIX_GATE_TIMEOUT, waiter.changed()).await {
                Ok(Ok(())) => {
                    info!(
                        "Stage {} waited {:?} for the shared prompt prefix cache",
                        stage_num,
                        waited.elapsed()
                    );
                }
                // Sender dropped: the warming stage is gone, so nothing to wait for.
                Ok(Err(_)) => {}
                Err(_) => warn!(
                    "Stage {} timed out after {:?} waiting for the shared prompt prefix cache; \
                     proceeding and paying for its own cache write",
                    stage_num, PREFIX_GATE_TIMEOUT
                ),
            }
        }

        info!("Running Stage {}", stage_num);
        let (stage_prompt, clean_stage_prompt) = self.prompts.get_stage_prompt(stage_num).await?;

        let format_guidance = r#"EFFICIENCY: Stay focused on this stage's scope. Use tools only to verify specific concerns from the diff; do not explore broadly. Aim to finish in 3-5 tool calls or fewer.

TodoWrite compatibility: vendored prompts may ask you to add tasks or suspected bugs to TodoWrite. Do not call or mention TodoWrite. Treat those instructions as an internal checklist only. If that checklist identifies a concrete suspected bug, carry it forward as a JSON concern with file, function_or_symbol, line when known, triggering condition, and evidence. Do not output generic checklist progress as a concern.

Once you have gathered sufficient information, return ONLY a JSON object with "concerns" and "dismissed_concerns" arrays.
If you find no concerns and no dismissed concerns, return `{"concerns": [], "dismissed_concerns": []}`.
If you find concerns, each must be an object with:
- "type": A short category string.
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation.
- "preexisting": A boolean value: `true` if this bug/vulnerability already existed in the codebase before these patches were applied, or `false` if the issue was newly introduced by the reviewed patchset.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line_range" (e.g., "120-125"), and "why_this_location_matters". Use `null` for "file", "function_or_symbol", or "line_range" when an issue is non-local or the exact value is not known. Do not invent line numbers; use `line_range: null` when the exact lines are not known and explain the triggering condition in "reasoning".

Use the "dismissed_concerns" array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.
If you find dismissed_concerns, each must use the same item schema as concerns except that dismissed_concerns do not need the "preexisting" field:
- "type": A short category string.
- "description": The candidate concern that was investigated and disproved.
- "reasoning": A step-by-step explanation of the evidence proving the candidate concern does not apply.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line_range" (e.g., "145-150"), and "why_this_location_matters". Use `null` for unknown values. Do not invent line numbers.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding system or caller handles it perfectly. Do not be overly charitable to the existing code. If there is a missing initialization, an unhandled edge case, or a brittle logic flow, report it as a concern immediately. Assume the worst-case scenario where external inputs and caller states are malformed.

Example:
```json
{
  "concerns": [
    {
      "type": "Issue Category",
      "description": "What is wrong.",
      "reasoning": "Why it is wrong.",
      "preexisting": false,
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line_range": "120-125",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Issue Category",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line_range": "145-150",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }
      ]
    }
  ]
}
```"#;

        let user_prompt = format!("{}\n\n{}", stage_prompt, format_guidance);
        let clean_user_prompt = format!("{}\n\n{}", clean_stage_prompt, format_guidance);

        let mut session = ReviewStageSession::new(
            stage,
            system_prompt,
            user_prompt,
            clean_user_prompt,
            self.tools.clone(),
            config.temperature,
            self.context_tag.as_deref(),
        );

        let mut runner = SessionRunner::new(config.provider.as_ref())
            .with_conversation_dump(
                self.conversation_dumper.clone(),
                format!("{}-s{}", config.name, stage_num),
            )
            .with_budget(config.budget)
            .with_max_validation_attempts(3)
            .with_max_turns(config.max_interactions)
            .with_turn_callback(move |turn, max_turns| {
                if let Some(progress_cb) = progress {
                    progress_cb(WorkerProgressEvent::StageTurn {
                        stage: stage_num,
                        turn,
                        max_turns,
                    });
                }
            });

        // This stage warms the shared prompt prefix; release its siblings as soon as the
        // provider has cached it.  The opener also opens on drop, so siblings are never
        // stranded if this stage fails before its first response.
        if let Some(opener) = config.prefix_opener {
            runner = runner.with_prefix_cached_callback(move || opener.open());
        }

        let result = match runner.run(&mut session).await {
            Ok(result) => result,
            Err(error) if config.name != "main" => {
                warn!(
                    "Experiment model {} failed stage {}: {}",
                    config.name, stage_num, error
                );
                let usage = error
                    .downcast_ref::<crate::ai::session::SessionBudgetError>()
                    .map(crate::ai::session::SessionBudgetError::usage);
                return Ok(StageExecutionResult {
                    stage: stage_num,
                    model: config.name,
                    model_id: config.model_id,
                    provider_id: config.provider_id,
                    concerns: Vec::new(),
                    dismissed_concerns: Vec::new(),
                    tokens_in: usage.map_or(0, |usage| usage.prompt_tokens as u32),
                    tokens_out: usage.map_or(0, |usage| usage.completion_tokens as u32),
                    tokens_cached: usage.and_then(|usage| usage.cached_tokens).unwrap_or(0) as u32,
                    history: Vec::new(),
                    failed: true,
                    error: Some(error.to_string()),
                });
            }
            Err(error) => return Err(error),
        };

        let mut concerns_out = Vec::new();
        let mut dismissed_concerns_out = Vec::new();

        if let Some(c) = result.output.get("concerns").and_then(|v| v.as_array()) {
            concerns_out = c.clone();
        }
        if let Some(c) = result
            .output
            .get("dismissed_concerns")
            .and_then(|v| v.as_array())
        {
            dismissed_concerns_out = c.clone();
        }

        if let Some(progress_cb) = progress {
            progress_cb(WorkerProgressEvent::StageFinished { stage: stage_num });
        }

        Ok(StageExecutionResult {
            stage: stage_num,
            model: config.name,
            model_id: config.model_id,
            provider_id: config.provider_id,
            concerns: concerns_out,
            dismissed_concerns: dismissed_concerns_out,
            tokens_in: result.usage.prompt_tokens as u32,
            tokens_out: result.usage.completion_tokens as u32,
            tokens_cached: result.usage.cached_tokens.unwrap_or(0) as u32,
            history: result.history,
            failed: false,
            error: None,
        })
    }

    async fn confirm_unique_findings(
        &self,
        shared_context: &str,
        concerns: &Value,
        experiment_runs: &[Value],
        baseline_decisions: &std::collections::BTreeMap<String, bool>,
    ) -> Result<(Value, Vec<Value>, Vec<Value>)> {
        use std::collections::BTreeMap;

        let Some(items) = concerns.as_array() else {
            return Ok((concerns.clone(), Vec::new(), Vec::new()));
        };
        let mut batches: BTreeMap<String, Vec<ConfirmationTarget>> = BTreeMap::new();
        let mut comparisons = Vec::new();
        let model_ids: BTreeMap<&str, &str> = self
            .additional_models
            .iter()
            .map(|model| (model.name.as_str(), model.model_id.as_str()))
            .collect();
        let provider_ids: BTreeMap<&str, &str> = self
            .additional_models
            .iter()
            .map(|model| (model.name.as_str(), model.provider_id.as_str()))
            .collect();
        let mut accepted = vec![true; items.len()];

        for (index, concern) in items.iter().enumerate() {
            let models: Vec<&str> = concern["source_models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let finding_id = concern["finding_ids"]
                .as_array()
                .and_then(|ids| ids.first())
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let severity = concern["severity"]
                .as_str()
                .unwrap_or("unknown")
                .to_string();
            let source_stages: Vec<u64> = concern["source_stages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .collect();
            let ran_models: Vec<&str> = experiment_runs
                .iter()
                .filter(|run| run["status"].as_str() == Some("completed"))
                .filter(|run| {
                    run["stage"]
                        .as_u64()
                        .is_some_and(|stage| source_stages.contains(&stage))
                })
                .filter_map(|run| run["model"].as_str())
                .filter(|model| *model != "main")
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if models.len() > 1 {
                if models.contains(&"main") {
                    for model in &ran_models {
                        comparisons.push(json!({
                            "finding_id": finding_id,
                            "additional_model": model,
                            "main_model_id": self.main_model,
                            "additional_model_id": model_ids.get(model).copied().unwrap_or("unknown"),
                            "main_provider_id": self.cohort.main.provider,
                            "additional_provider_id": provider_ids.get(model).copied().unwrap_or("unknown"),
                            "outcome": if models.contains(model) { "both" } else { "main_only" },
                            "severity": severity,
                        }));
                    }
                } else {
                    for model in models {
                        comparisons.push(json!({
                            "finding_id": finding_id,
                            "additional_model": model,
                            "main_model_id": self.main_model,
                            "additional_model_id": model_ids.get(model).copied().unwrap_or("unknown"),
                            "main_provider_id": self.cohort.main.provider,
                            "additional_provider_id": provider_ids.get(model).copied().unwrap_or("unknown"),
                            "outcome": "additional_only",
                            "severity": severity,
                        }));
                    }
                }
                continue;
            }
            let discoverer = models.first().copied().unwrap_or("main");
            let confirmer = if discoverer == "main" {
                ran_models.first().copied()
            } else {
                Some("main")
            };
            if let Some(confirmer) = confirmer {
                batches
                    .entry(confirmer.to_string())
                    .or_default()
                    .push(ConfirmationTarget {
                        index,
                        finding_id,
                        discoverer: discoverer.to_string(),
                        compared_models: if discoverer == "main" {
                            ran_models
                                .iter()
                                .map(|model| (*model).to_string())
                                .collect()
                        } else {
                            vec![discoverer.to_string()]
                        },
                    });
            } else if discoverer == "main" {
                accepted[index] = baseline_decisions.get(&finding_id).copied().unwrap_or(true);
            }
        }

        let mut confirmation_runs = Vec::new();
        for (confirmer, batch) in batches {
            let (provider, confirmer_model_id, confirmer_provider_id) = if confirmer == "main" {
                (
                    Arc::new(crate::ai::model_experiment::RoutedProvider::new(
                        self.provider.clone(),
                        "main",
                    )) as Arc<dyn AiProvider>,
                    self.main_model.as_str(),
                    self.cohort.main.provider.as_str(),
                )
            } else {
                let model = self
                    .additional_models
                    .iter()
                    .find(|model| model.name == confirmer)
                    .ok_or_else(|| anyhow::anyhow!("Missing confirmer provider: {confirmer}"))?;
                (
                    model.provider.clone(),
                    model.model_id.as_str(),
                    model.provider_id.as_str(),
                )
            };
            let payload: Vec<Value> = batch
                .iter()
                .map(|target| {
                    json!({"finding_id": target.finding_id, "finding": items[target.index]})
                })
                .collect();
            let prompt = format!(
                "{shared_context}\n\nYou are the false-positive confirmation reviewer. Independently verify every finding below against the patch and supplied code context. Return one boolean for every finding ID: true only when the issue is real and actionable, false when it is unsupported or incorrect.\n\nFindings:\n{}",
                serde_json::to_string_pretty(&payload)?
            );
            let request = AiRequest {
                system: None,
                messages: vec![AiMessage {
                    role: AiRole::User,
                    content: Some(prompt),
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: None,
                    tool_call_id: None,
                }],
                tools: None,
                temperature: Some(0.0),
                response_format: Some(AiResponseFormat::Json { schema: None }),
                context_tag: self.context_tag.clone(),
            };
            let expected_ids: Vec<&str> = batch
                .iter()
                .map(|target| target.finding_id.as_str())
                .collect();
            let validation_budget = self.validation_budget.map(ReviewBudget::new);
            let (decision_result, (tokens_in, tokens_out, tokens_cached)) = request_confirmation(
                provider.as_ref(),
                request,
                &expected_ids,
                validation_budget.as_ref(),
            )
            .await;
            let budget_snapshot = validation_budget.as_ref().map(ReviewBudget::snapshot);
            let decisions = match decision_result {
                Ok(decisions) => decisions,
                Err(error) => {
                    warn!("Confirmation model {} failed: {}", confirmer, error);
                    for target in &batch {
                        if target.discoverer == "main" {
                            accepted[target.index] = baseline_decisions
                                .get(&target.finding_id)
                                .copied()
                                .unwrap_or(true);
                        } else {
                            accepted[target.index] = false;
                        }
                    }
                    confirmation_runs.push(json!({
                        "model": confirmer,
                        "model_id": confirmer_model_id,
                        "provider_id": confirmer_provider_id,
                        "status": "failed",
                        "error": error.to_string(),
                        "tokens_in": tokens_in,
                        "tokens_out": tokens_out,
                        "tokens_cached": tokens_cached,
                        "budget_input": budget_snapshot.map(|snapshot| snapshot.input).unwrap_or(0),
                        "budget_output": budget_snapshot.map(|snapshot| snapshot.output).unwrap_or(0),
                        "budget_flags": budget_snapshot.map(|snapshot| snapshot.flags).unwrap_or(0),
                    }));
                    continue;
                }
            };
            for target in batch {
                let confirmed = decisions[&target.finding_id].as_bool().unwrap_or(false);
                accepted[target.index] = confirmed;
                let outcome = match (target.discoverer.as_str() == "main", confirmed) {
                    (true, true) => "main_only",
                    (true, false) => "main_hallucination",
                    (false, true) => "additional_only",
                    (false, false) => "additional_hallucination",
                };
                for additional_model in target.compared_models {
                    comparisons.push(json!({
                        "finding_id": target.finding_id,
                        "additional_model": additional_model,
                        "main_model_id": self.main_model,
                        "additional_model_id": model_ids.get(additional_model.as_str()).copied().unwrap_or("unknown"),
                        "main_provider_id": self.cohort.main.provider,
                        "additional_provider_id": provider_ids.get(additional_model.as_str()).copied().unwrap_or("unknown"),
                        "outcome": outcome,
                        "confirmed_by": confirmer,
                        "severity": items[target.index]["severity"].as_str().unwrap_or("unknown"),
                    }));
                }
            }
            confirmation_runs.push(json!({
                "model": confirmer,
                "model_id": confirmer_model_id,
                "provider_id": confirmer_provider_id,
                "status": "completed",
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "tokens_cached": tokens_cached,
                "budget_input": budget_snapshot.map(|snapshot| snapshot.input).unwrap_or(0),
                "budget_output": budget_snapshot.map(|snapshot| snapshot.output).unwrap_or(0),
                "budget_flags": budget_snapshot.map(|snapshot| snapshot.flags).unwrap_or(0),
            }));
        }

        let filtered: Vec<Value> = items
            .iter()
            .zip(accepted)
            .filter(|(_, accepted)| *accepted)
            .map(|(item, _)| item.clone())
            .collect();
        Ok((json!(filtered), comparisons, confirmation_runs))
    }
}

async fn request_confirmation(
    provider: &dyn AiProvider,
    request: AiRequest,
    expected_ids: &[&str],
    budget: Option<&ReviewBudget>,
) -> (Result<Value>, (usize, usize, usize)) {
    let mut last_error = None;
    let mut tokens = (0, 0, 0);
    for _ in 0..2 {
        let estimated_input = provider.estimate_tokens(&request);
        if budget.is_some_and(|budget| !budget.allows(estimated_input, 0)) {
            return (
                Err(anyhow::anyhow!(
                    "validation input estimate exceeds the confirmation budget"
                )),
                tokens,
            );
        }
        match provider.generate_content(request.clone()).await {
            Ok(response) => {
                if let Some(usage) = &response.usage {
                    let within_budget = budget.is_none_or(|budget| {
                        budget.allows(usage.prompt_tokens, usage.completion_tokens)
                    });
                    tokens.0 += usage.prompt_tokens;
                    tokens.1 += usage.completion_tokens;
                    tokens.2 += usage.cached_tokens.unwrap_or(0);
                    if let Some(budget) = budget {
                        let mut flags = 0;
                        budget.record_and_check(
                            &mut flags,
                            usage.prompt_tokens,
                            usage.completion_tokens,
                            usage.prompt_tokens,
                            usage.completion_tokens,
                            usage.cached_tokens.unwrap_or(0),
                        );
                    }
                    if !within_budget {
                        return (
                            Err(anyhow::anyhow!(
                                "confirmation usage exceeded the validation budget"
                            )),
                            tokens,
                        );
                    }
                }
                let content = response.content.as_deref().unwrap_or("{}");
                let cleaned = crate::utils::clean_json_string(content);
                match serde_json::from_str::<Value>(&cleaned) {
                    Ok(decisions) => {
                        match validate_confirmation_decisions(&decisions, expected_ids) {
                            Ok(()) => return (Ok(decisions), tokens),
                            Err(error) => last_error = Some(error),
                        }
                    }
                    Err(error) => last_error = Some(error.into()),
                }
            }
            Err(error) => last_error = Some(error),
        }
    }
    (
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("confirmation request failed"))),
        tokens,
    )
}

fn validate_confirmation_decisions(decisions: &Value, expected_ids: &[&str]) -> Result<()> {
    let object = decisions
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("confirmation response is not an object"))?;
    let expected: std::collections::BTreeSet<&str> = expected_ids.iter().copied().collect();
    let actual: std::collections::BTreeSet<&str> = object.keys().map(String::as_str).collect();
    if actual != expected || object.values().any(|decision| !decision.is_boolean()) {
        anyhow::bail!(
            "confirmation response must contain exactly one boolean for every requested finding"
        );
    }
    Ok(())
}

fn provenance_map(items: &[Value]) -> Result<std::collections::BTreeMap<String, Vec<String>>> {
    let mut provenance = std::collections::BTreeMap::new();
    for item in items {
        let model_values = item["source_models"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing source_models"))?;
        let models: Vec<String> = model_values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if models.len() != model_values.len()
            || models
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != models.len()
        {
            return Err(anyhow::anyhow!(
                "source_models contains duplicate or non-string entries"
            ));
        }
        let ids = item["finding_ids"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing finding_ids"))?;
        let string_ids: Vec<&str> = ids.iter().filter_map(Value::as_str).collect();
        if string_ids.len() != ids.len() {
            return Err(anyhow::anyhow!("finding_ids contains non-string entries"));
        }
        for id in string_ids {
            if provenance.insert(id.to_string(), models.clone()).is_some() {
                return Err(anyhow::anyhow!("duplicate finding ID: {id}"));
            }
        }
    }
    Ok(provenance)
}

fn validate_provenance(inputs: &[Value], outputs: &Value, require_all: bool) -> Result<()> {
    use std::collections::BTreeSet;

    let input = provenance_map(inputs)?;
    let output_items = outputs
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("provenance output is not an array"))?;
    let mut seen = BTreeSet::new();
    for output in output_items {
        let id_values = output["finding_ids"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing finding_ids"))?;
        let ids: Vec<&str> = id_values.iter().filter_map(Value::as_str).collect();
        if ids.len() != id_values.len() {
            return Err(ReviewError::FormatRejection(
                "finding_ids contains non-string entries".to_string(),
            )
            .into());
        }
        let actual_models: BTreeSet<&str> = output["source_models"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("missing source_models"))?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        if actual_models.len() != output["source_models"].as_array().map_or(0, Vec::len) {
            return Err(ReviewError::FormatRejection(
                "source_models contains duplicate or non-string entries".to_string(),
            )
            .into());
        }
        let mut expected_models = BTreeSet::new();
        for id in ids {
            if !seen.insert(id) {
                return Err(ReviewError::FormatRejection(format!(
                    "finding ID {id} appears more than once"
                ))
                .into());
            }
            let models = input
                .get(id)
                .ok_or_else(|| ReviewError::FormatRejection(format!("unknown finding ID {id}")))?;
            expected_models.extend(models.iter().map(String::as_str));
        }
        if actual_models != expected_models {
            return Err(ReviewError::FormatRejection(
                "source_models does not match the finding ID provenance".to_string(),
            )
            .into());
        }
    }
    if require_all && seen.len() != input.len() {
        return Err(ReviewError::FormatRejection(
            "deduplication dropped one or more finding IDs".to_string(),
        )
        .into());
    }
    Ok(())
}

fn annotate_validation_candidates(
    concerns: &Value,
    cohort: &crate::ai::model_experiment::ReviewCohort,
    runs: &[Value],
) -> Value {
    let mut marked = concerns.clone();
    for concern in marked.as_array_mut().into_iter().flatten() {
        let models: Vec<&str> = concern["source_models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let stages: Vec<u64> = concern["source_stages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .collect();
        let variant_ran = runs.iter().any(|run| {
            run["status"].as_str() == Some("completed")
                && run["model"]
                    .as_str()
                    .is_some_and(|model| cohort.contains(model))
                && run["stage"]
                    .as_u64()
                    .is_some_and(|stage| stages.contains(&stage))
        });
        let keep =
            models.len() > 1 || models.first().is_some_and(|model| *model != "main") || variant_ran;
        if let Some(object) = concern.as_object_mut() {
            object.insert("requires_validation".to_string(), json!(keep));
        }
    }
    marked
}

fn validate_required_experiment_findings(inputs: &Value, outputs: &Value) -> Result<()> {
    let required: std::collections::BTreeSet<&str> = inputs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["requires_validation"].as_bool() == Some(true))
        .flat_map(|item| item["finding_ids"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    let present: std::collections::BTreeSet<&str> = outputs
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| item["finding_ids"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    if !required.is_subset(&present) {
        return Err(ReviewError::FormatRejection(
            "severity estimation dropped a required experiment finding".to_string(),
        )
        .into());
    }
    Ok(())
}

fn validate_baseline_decisions(
    inputs: &Value,
    decisions: Option<&Value>,
) -> Result<std::collections::BTreeMap<String, bool>> {
    let expected: std::collections::BTreeSet<&str> = inputs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["finding_ids"].as_array()?.first()?.as_str())
        .collect();
    let object = decisions
        .and_then(Value::as_object)
        .ok_or_else(|| ReviewError::FormatRejection("missing baseline_decisions object".into()))?;
    let actual: std::collections::BTreeSet<&str> = object.keys().map(String::as_str).collect();
    if actual != expected {
        return Err(ReviewError::FormatRejection(
            "baseline_decisions must contain exactly one entry for every input concern".into(),
        )
        .into());
    }
    object
        .iter()
        .map(|(id, decision)| {
            decision
                .as_bool()
                .map(|decision| (id.clone(), decision))
                .ok_or_else(|| {
                    ReviewError::FormatRejection(format!(
                        "baseline decision for {id} is not a boolean"
                    ))
                    .into()
                })
        })
        .collect()
}

struct StageExecutionResult {
    stage: u8,
    model: String,
    model_id: String,
    provider_id: String,
    concerns: Vec<Value>,
    dismissed_concerns: Vec<Value>,
    tokens_in: u32,
    tokens_out: u32,
    tokens_cached: u32,
    history: Vec<AiMessage>,
    failed: bool,
    error: Option<String>,
}

struct StageExecutionConfig {
    provider: Arc<dyn AiProvider>,
    name: String,
    temperature: f32,
    max_interactions: usize,
    model_id: String,
    provider_id: String,
    budget: Option<ReviewBudget>,
    /// Set on the one stage per prompt-prefix group that warms the provider cache.
    prefix_opener: Option<Arc<PrefixGateOpener>>,
    /// Set on stages that must wait for that warm-up before their first request.
    wait_for_prefix: Option<tokio::sync::watch::Receiver<bool>>,
}

/// Upper bound on how long a stage waits for a sibling to warm the shared prompt
/// prefix.  Only a failsafe: the gate is normally opened by the first response, and
/// unconditionally on drop.
///
/// Providers commit the cache entry when the response completes, not when they
/// ingest the prompt, so this has to cover a whole first turn.  That is not one
/// generation: a format violation retries in place (`turns` is decremented), so the
/// first turn can span up to `max_validation_attempts` responses.  Measured first
/// turns reach ~325s (median 22s, p95 172s), and 300s clipped exactly that tail,
/// making slow openers lose the saving for every sibling.  Keep this well under
/// `review.timeout_seconds` so a wedged gate can never be what fails a review.
const PREFIX_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Opens a prompt-prefix gate, releasing stages that share the prefix.
///
/// Opens on the first response (the point at which the provider has written the
/// cache entry) and again on drop, so a stage that fails outright cannot strand
/// its siblings.
struct PrefixGateOpener {
    tx: tokio::sync::watch::Sender<bool>,
}

impl PrefixGateOpener {
    fn open(&self) {
        let _ = self.tx.send(true);
    }
}

impl Drop for PrefixGateOpener {
    fn drop(&mut self) {
        self.open();
    }
}

/// A prompt-prefix group that already has a stage elected to warm it.
struct PrefixGate {
    rx: tokio::sync::watch::Receiver<bool>,
}

/// Assigns a stage its role in the prompt-prefix group keyed by `(model, use_log)`.
///
/// The first caller for a group becomes the opener and runs immediately; later callers
/// wait on it.  Returns no gate at all when the provider does not cache prefixes, so
/// those stages keep running fully concurrently.
fn claim_prefix_gate(
    gates: &mut std::collections::HashMap<(String, bool), PrefixGate>,
    model: &str,
    use_log: bool,
    provider_caches: bool,
) -> (
    Option<Arc<PrefixGateOpener>>,
    Option<tokio::sync::watch::Receiver<bool>>,
) {
    if !provider_caches {
        return (None, None);
    }
    let key = (model.to_string(), use_log);
    match gates.get(&key) {
        Some(gate) => (None, Some(gate.rx.clone())),
        None => {
            let (tx, rx) = tokio::sync::watch::channel(false);
            gates.insert(key, PrefixGate { rx });
            (Some(Arc::new(PrefixGateOpener { tx })), None)
        }
    }
}

struct ConfirmationTarget {
    index: usize,
    finding_id: String,
    discoverer: String,
    compared_models: Vec<String>,
}

pub fn calculate_series_range(
    patches: &[PatchInput],
    patches_to_review: &[PatchInput],
    patch_shas: &std::collections::HashMap<i64, String>,
    baseline_sha: &str,
) -> Option<String> {
    if patches.is_empty() {
        return None;
    }

    let max_patch_index = patches.iter().map(|p| p.index).max().unwrap_or(0);
    let is_last_patch_review =
        patches_to_review.len() == 1 && patches_to_review[0].index == max_patch_index;

    if is_last_patch_review {
        None
    } else {
        patches
            .iter()
            .map(|p| p.index)
            .max()
            .and_then(|max_idx| {
                patches
                    .iter()
                    .find(|p| p.index == max_idx)
                    .and_then(|p| p.commit_id.clone())
                    .or_else(|| patch_shas.get(&max_idx).cloned())
            })
            .map(|end_sha| format!("{}..{}", baseline_sha, end_sha))
    }
}

fn append_stage_items(
    target: &mut Vec<Value>,
    items: &[Value],
    stage: u8,
    model: &str,
    default_type: &str,
    default_text_key: &str,
) {
    for item in items {
        if let Some(mut item) = normalize_stage_item(item, stage, default_type, default_text_key) {
            if let Some(object) = item.as_object_mut() {
                object.insert("source_models".to_string(), json!([model]));
                object.insert(
                    "finding_ids".to_string(),
                    json!([format!("{}-{}-{}", model, stage, target.len())]),
                );
            }
            target.push(item);
        }
    }
}

fn findings_with_main_source_label(findings: &Value, main_source_name: &str) -> Value {
    let mut labeled = findings.clone();
    for source in labeled
        .as_array_mut()
        .into_iter()
        .flatten()
        .filter_map(|finding| finding["source_models"].as_array_mut())
        .flatten()
    {
        if source.as_str() == Some("main") {
            *source = Value::String(main_source_name.to_string());
        }
    }
    labeled
}

fn append_stage_dismissed_concerns(
    target: &mut Vec<Value>,
    items: &[Value],
    stage: u8,
    model: &str,
) {
    append_stage_items(target, items, stage, model, "General", "description");
}

fn normalize_stage_item(
    item: &Value,
    stage: u8,
    default_type: &str,
    default_text_key: &str,
) -> Option<Value> {
    if let Some(obj) = item.as_object() {
        let mut with_stage = obj.clone();
        with_stage.insert("source_stage".to_string(), json!(stage));
        Some(Value::Object(with_stage))
    } else {
        item.as_str().map(|s| {
            let mut obj = serde_json::Map::new();
            obj.insert("source_stage".to_string(), json!(stage));
            obj.insert("type".to_string(), json!(default_type));
            obj.insert(default_text_key.to_string(), json!(s));
            Value::Object(obj)
        })
    }
}

struct ReviewStageSession {
    stage: Box<dyn ReviewStage>,
    system_prompt: String,
    user_prompt: String,
    clean_user_prompt: String,
    tools: std::sync::Arc<ToolBox>,
    temperature: f32,
    context_tag: Option<String>,
    last_tool_call: Option<(String, Value)>,
    recitation_retries: usize,
    required_baseline_ids: Option<std::collections::BTreeSet<String>>,
    required_provenance: Option<(Vec<Value>, bool, &'static str)>,
    required_experiment_findings: Option<Value>,
}

impl ReviewStageSession {
    fn new(
        stage: Box<dyn ReviewStage>,
        system_prompt: String,
        user_prompt: String,
        clean_user_prompt: String,
        tools: std::sync::Arc<ToolBox>,
        temperature: f32,
        context_prefix: Option<&str>,
    ) -> Self {
        let stage_num = stage.number();
        let context_tag = context_prefix.map(|prefix| {
            if prefix.len() >= 2 {
                format!("{} s:{}] ", &prefix[..prefix.len() - 2], stage_num)
            } else {
                format!("s:{}] ", stage_num)
            }
        });
        Self {
            stage,
            system_prompt,
            user_prompt,
            clean_user_prompt,
            tools,
            temperature,
            context_tag,
            last_tool_call: None,
            recitation_retries: 0,
            required_baseline_ids: None,
            required_provenance: None,
            required_experiment_findings: None,
        }
    }

    fn require_baseline_decisions(&mut self, ids: std::collections::BTreeSet<String>) {
        self.required_baseline_ids = Some(ids);
    }

    fn require_provenance(
        &mut self,
        inputs: Vec<Value>,
        require_all: bool,
        output_key: &'static str,
    ) {
        self.required_provenance = Some((inputs, require_all, output_key));
    }

    fn require_experiment_findings(&mut self, inputs: Value) {
        self.required_experiment_findings = Some(inputs);
    }
}

#[async_trait::async_trait]
impl LlmSession for ReviewStageSession {
    type Output = serde_json::Value;

    fn system_prompt(&self) -> String {
        self.system_prompt.clone()
    }

    fn initial_user_prompt(&self) -> String {
        self.user_prompt.clone()
    }

    fn log_user_prompt(&self) -> String {
        self.clean_user_prompt.clone()
    }

    fn format_validation_feedback(&self, violation: &str) -> String {
        self.stage.format_validation_feedback(violation)
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        Some(self.tools.get_declarations_generic())
    }

    fn temperature(&self) -> Option<f32> {
        Some(self.temperature)
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if self
            .last_tool_call
            .as_ref()
            .map_or(false, |(last_name, last_args)| {
                last_name == name && last_args == &args
            })
        {
            tracing::warn!("Blocked duplicate tool call: {} with args {:?}", name, args);
            return Ok(serde_json::json!({
                "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
            }));
        }
        self.last_tool_call = Some((name.to_string(), args.clone()));
        match self.tools.call(name, args).await {
            Ok(v) => Ok(v),
            Err(e) => Ok(serde_json::json!({
                "error": e.to_string()
            })),
        }
    }

    async fn call_tools(
        &mut self,
        calls: Vec<crate::ai::ToolCall>,
    ) -> Result<Vec<(String, Value)>> {
        let mut results = vec![None; calls.len()];
        let mut calls_to_run = Vec::new();

        for (idx, call) in calls.into_iter().enumerate() {
            let name = call.function_name;
            let args = call.arguments;
            let call_id = call.id;

            if self
                .last_tool_call
                .as_ref()
                .map_or(false, |(last_name, last_args)| {
                    last_name == &name && last_args == &args
                })
            {
                tracing::warn!("Blocked duplicate tool call: {} with args {:?}", name, args);
                results[idx] = Some((
                    call_id,
                    serde_json::json!({
                        "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
                    }),
                ));
            } else {
                self.last_tool_call = Some((name.clone(), args.clone()));
                calls_to_run.push((idx, call_id, name, args));
            }
        }

        if !calls_to_run.is_empty() {
            let tools = self.tools.clone();
            let futures: Vec<_> = calls_to_run
                .into_iter()
                .map(|(idx, call_id, name, args)| {
                    let tools = tools.clone();
                    async move {
                        let res = match tools.call(&name, args).await {
                            Ok(v) => v,
                            Err(e) => serde_json::json!({"error": e.to_string()}),
                        };
                        (idx, (call_id, res))
                    }
                })
                .collect();

            let parallel_results = futures::future::join_all(futures).await;
            for (idx, res) in parallel_results {
                results[idx] = Some(res);
            }
        }

        Ok(results.into_iter().map(|o| o.unwrap()).collect())
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let output = self.stage.validate(response)?;
        if let Some(expected) = &self.required_baseline_ids {
            let decisions = output
                .get("baseline_decisions")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    ValidationError::FormatViolation(
                        "missing baseline_decisions object".to_string(),
                    )
                })?;
            let actual: std::collections::BTreeSet<String> = decisions.keys().cloned().collect();
            if &actual != expected || decisions.values().any(|decision| !decision.is_boolean()) {
                return Err(ValidationError::FormatViolation(
                    "baseline_decisions must contain one boolean for every input concern"
                        .to_string(),
                ));
            }
        }
        if let Some((inputs, require_all, output_key)) = &self.required_provenance {
            let outputs = output.get(*output_key).ok_or_else(|| {
                ValidationError::FormatViolation(format!("missing {output_key} output"))
            })?;
            validate_provenance(inputs, outputs, *require_all).map_err(|error| {
                ValidationError::FormatViolation(format!("invalid finding provenance: {error}"))
            })?;
        }
        if let Some(inputs) = &self.required_experiment_findings {
            let outputs = output.get("findings").ok_or_else(|| {
                ValidationError::FormatViolation("missing findings output".to_string())
            })?;
            validate_required_experiment_findings(inputs, outputs).map_err(|error| {
                ValidationError::FormatViolation(format!(
                    "missing required experiment finding: {error}"
                ))
            })?;
        }
        Ok(output)
    }

    fn handle_provider_error(&mut self, error: &anyhow::Error, _attempt: usize) -> ErrorAction {
        let err_str = error.to_string();
        let is_recitation = err_str.contains("RECITATION") || err_str.contains("blocked");

        if is_recitation {
            self.recitation_retries += 1;
            if self.recitation_retries > 3 {
                return ErrorAction::Fail;
            }

            if let Some(action) = self.stage.handle_recitation_error() {
                return action;
            }

            return ErrorAction::RetryWithFeedback(
                "IMPORTANT: Your previous response was blocked by a recitation filter. \
                 Please do NOT copy large blocks of code verbatim in your response. \
                 Describe changes in prose, or use highly simplified pseudo-code if you must show code structure."
                    .to_string(),
            );
        }

        ErrorAction::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cohort() -> crate::ai::model_experiment::ReviewCohort {
        crate::ai::model_experiment::ReviewCohort {
            main: crate::ai::model_experiment::SourceIdentity {
                name: "main".to_string(),
                display_name: "primary".to_string(),
                provider: "test".to_string(),
                model: "mock".to_string(),
            },
            variants: Vec::new(),
        }
    }

    fn test_variant_cohort() -> crate::ai::model_experiment::ReviewCohort {
        let mut cohort = test_cohort();
        cohort
            .variants
            .push(crate::ai::model_experiment::CohortMember {
                source: crate::ai::model_experiment::SourceIdentity {
                    name: "variant".to_string(),
                    display_name: "variant".to_string(),
                    provider: "test".to_string(),
                    model: "variant-model".to_string(),
                },
                selected: true,
            });
        cohort
    }

    #[tokio::test]
    async fn stage_eleven_requires_source_annotations() {
        let temp = tempfile::tempdir().unwrap();
        let prompts = PromptRegistry::new(temp.path().to_path_buf());

        let (prompt, clean_prompt) = prompts.get_stage_prompt(11).await.unwrap();

        for text in [prompt, clean_prompt] {
            assert!(text.contains("[Sources: <names>]"));
            assert!(text.contains("Copy the finding's `source_models` entries exactly"));
        }
    }

    #[test]
    fn presentation_sources_name_the_main_source() {
        let findings = json!([
            {"source_models": ["main"]},
            {"source_models": ["fable", "main"]}
        ]);

        let labeled = findings_with_main_source_label(&findings, "opus-5");

        assert_eq!(findings[0]["source_models"], json!(["main"]));
        assert_eq!(labeled[0]["source_models"], json!(["opus-5"]));
        assert_eq!(labeled[1]["source_models"], json!(["fable", "opus-5"]));
    }

    struct ConfirmationProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    struct FailingConfirmationProvider;

    #[async_trait::async_trait]
    impl AiProvider for FailingConfirmationProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            anyhow::bail!("confirmation unavailable")
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "failing-confirmation".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[async_trait::async_trait]
    impl AiProvider for ConfirmationProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(AiResponse {
                content: Some(if call == 0 {
                    json!({"a": true}).to_string()
                } else {
                    json!({"a": true, "b": false}).to_string()
                }),
                thought: None,
                thought_signature: None,
                reasoning: None,
                tool_calls: None,
                usage: Some(crate::ai::AiUsage {
                    prompt_tokens: 10,
                    completion_tokens: 1,
                    total_tokens: 11,
                    cached_tokens: None,
                    cache_write_tokens: None,
                }),
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            10
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "confirmation-mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn confirmation_retries_incomplete_mappings() {
        let provider = ConfirmationProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let request = AiRequest {
            system: None,
            messages: Vec::new(),
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };
        let (decisions, _) = request_confirmation(&provider, request, &["a", "b"], None).await;
        let decisions = decisions.unwrap();
        assert_eq!(decisions["b"], false);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn confirmation_retry_cannot_exceed_its_review_budget() {
        let provider = ConfirmationProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let request = AiRequest {
            system: None,
            messages: Vec::new(),
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };
        let budget = ReviewBudget::new(crate::ai::review_budget::BudgetConfig {
            stage_input: 100,
            stage_output: 100,
            review_input: 15,
            review_output: 100,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
            enforce_hard_limits: true,
        });

        let (decisions, tokens) =
            request_confirmation(&provider, request, &["a", "b"], Some(&budget)).await;

        assert!(decisions.is_err());
        assert_eq!(tokens.0, 10);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shared_discoveries_are_not_confirmed_again() {
        let temp = tempfile::tempdir().unwrap();
        let main = Arc::new(ConfirmationProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let variant = Arc::new(ConfirmationProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let worker = Worker::new(
            main.clone(),
            Arc::new(ToolBox::new(temp.path().to_path_buf(), None)),
            PromptRegistry::new(temp.path().to_path_buf()),
            WorkerConfig {
                main_model: "model-a".to_string(),
                max_input_tokens: 1000,
                max_interactions: 2,
                temperature: 0.0,
                custom_prompt: None,
                series_range: None,
                stages: None,
                dump_conversation: None,
                budget: None,
                merge_budget: None,
                retry_provider: None,
                additional_models: vec![AdditionalModelRunner {
                    name: "variant".to_string(),
                    provider: variant.clone(),
                    temperature: 0.0,
                    max_interactions: 2,
                    model_id: "model-b".to_string(),
                    provider_id: "test".to_string(),
                    budget: None,
                }],
                cohort: crate::ai::model_experiment::ReviewCohort {
                    main: crate::ai::model_experiment::SourceIdentity {
                        name: "main".to_string(),
                        display_name: "primary".to_string(),
                        provider: "test".to_string(),
                        model: "model-a".to_string(),
                    },
                    variants: vec![crate::ai::model_experiment::CohortMember {
                        source: crate::ai::model_experiment::SourceIdentity {
                            name: "variant".to_string(),
                            display_name: "variant".to_string(),
                            provider: "test".to_string(),
                            model: "model-b".to_string(),
                        },
                        selected: true,
                    }],
                },
                validation_budget: None,
            },
        );
        let findings = json!([{
            "finding_ids": ["main-3-0", "variant-3-0"],
            "source_models": ["main", "variant"],
            "source_stages": [3],
            "severity": "High"
        }]);
        let runs = json!([
            {"model": "main", "stage": 3, "status": "completed"},
            {"model": "variant", "stage": 3, "status": "completed"}
        ]);
        let (accepted, comparisons, confirmation_runs) = worker
            .confirm_unique_findings(
                "context",
                &findings,
                runs.as_array().unwrap(),
                &std::collections::BTreeMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.as_array().unwrap().len(), 1);
        assert_eq!(comparisons[0]["outcome"], "both");
        assert!(confirmation_runs.is_empty());
        assert_eq!(main.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(variant.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_confirmation_uses_the_main_baseline_decision() {
        let temp = tempfile::tempdir().unwrap();
        let main = Arc::new(ConfirmationProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let worker = Worker::new(
            main.clone(),
            Arc::new(ToolBox::new(temp.path().to_path_buf(), None)),
            PromptRegistry::new(temp.path().to_path_buf()),
            WorkerConfig {
                main_model: "model-a".to_string(),
                max_input_tokens: 1000,
                max_interactions: 2,
                temperature: 0.0,
                custom_prompt: None,
                series_range: None,
                stages: None,
                dump_conversation: None,
                budget: None,
                merge_budget: None,
                retry_provider: None,
                additional_models: vec![AdditionalModelRunner {
                    name: "variant".to_string(),
                    provider: Arc::new(FailingConfirmationProvider),
                    temperature: 0.0,
                    max_interactions: 2,
                    model_id: "model-b".to_string(),
                    provider_id: "test".to_string(),
                    budget: None,
                }],
                cohort: test_variant_cohort(),
                validation_budget: None,
            },
        );
        let findings = json!([{
            "finding_ids": ["main-3-0"],
            "source_models": ["main"],
            "source_stages": [3],
            "severity": "High"
        }]);
        let runs = json!([
            {"model": "main", "stage": 3, "status": "completed"},
            {"model": "variant", "stage": 3, "status": "completed"}
        ]);
        let baseline = std::collections::BTreeMap::from([("main-3-0".to_string(), false)]);

        let (accepted, _, confirmation_runs) = worker
            .confirm_unique_findings("context", &findings, runs.as_array().unwrap(), &baseline)
            .await
            .unwrap();

        assert!(accepted.as_array().unwrap().is_empty());
        assert_eq!(confirmation_runs[0]["status"], "failed");
        assert_eq!(main.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn baseline_decisions_require_a_complete_boolean_mapping() {
        let inputs = json!([
            {"finding_ids": ["a"]},
            {"finding_ids": ["b"]}
        ]);
        let valid = json!({"a": true, "b": false});
        assert!(validate_baseline_decisions(&inputs, Some(&valid)).unwrap()["a"]);
        assert!(validate_baseline_decisions(&inputs, Some(&json!({"a": true}))).is_err());
        assert!(
            validate_baseline_decisions(&inputs, Some(&json!({"a": true, "b": "no"}))).is_err()
        );
    }

    #[test]
    fn validation_candidates_require_an_available_variant_for_main_findings() {
        let concern = json!([{
            "source_models": ["main"],
            "source_stages": [3],
            "finding_ids": ["main-3-0"]
        }]);
        let no_sample = json!([]);
        assert_eq!(
            annotate_validation_candidates(
                &concern,
                &test_variant_cohort(),
                no_sample.as_array().unwrap(),
            )[0]["requires_validation"],
            false
        );

        let sample = json!([{"model": "variant", "stage": 3, "status": "completed"}]);
        assert_eq!(
            annotate_validation_candidates(
                &concern,
                &test_variant_cohort(),
                sample.as_array().unwrap(),
            )[0]["requires_validation"],
            true
        );
    }

    #[test]
    fn shared_findings_are_always_retained_for_experiments() {
        let concern = json!([{
            "source_models": ["variant-a", "variant-b"],
            "source_stages": [4],
            "finding_ids": ["a-4-0", "b-4-0"]
        }]);
        assert_eq!(
            annotate_validation_candidates(&concern, &test_variant_cohort(), &[])[0]["requires_validation"],
            true
        );
    }

    #[test]
    fn provenance_validation_is_order_independent_and_rejects_corruption() {
        let inputs = vec![
            json!({"finding_ids": ["a"], "source_models": ["main"]}),
            json!({"finding_ids": ["b"], "source_models": ["variant"]}),
        ];
        let reordered = json!([
            {"finding_ids": ["b"], "source_models": ["variant"]},
            {"finding_ids": ["a"], "source_models": ["main"]}
        ]);
        assert!(validate_provenance(&inputs, &reordered, true).is_ok());

        let corrupted = json!([
            {"finding_ids": ["a"], "source_models": ["variant"]},
            {"finding_ids": ["b"], "source_models": ["main"]}
        ]);
        assert!(validate_provenance(&inputs, &corrupted, true).is_err());

        let duplicated = json!([
            {"finding_ids": ["a"], "source_models": ["main", "main"]},
            {"finding_ids": ["b"], "source_models": ["variant"]}
        ]);
        assert!(validate_provenance(&inputs, &duplicated, true).is_err());

        let non_string_input = vec![json!({"finding_ids": ["a", null], "source_models": ["main"]})];
        assert!(provenance_map(&non_string_input).is_err());

        let non_string_output = json!([
            {"finding_ids": ["a", null], "source_models": ["main"]},
            {"finding_ids": ["b"], "source_models": ["variant"]}
        ]);
        assert!(validate_provenance(&inputs, &non_string_output, true).is_err());
    }

    #[test]
    fn confirmation_decisions_reject_unknown_and_non_boolean_ids() {
        assert!(validate_confirmation_decisions(&json!({"a": true}), &["a"]).is_ok());
        assert!(
            validate_confirmation_decisions(&json!({"a": true, "unknown": false}), &["a"]).is_err()
        );
        assert!(validate_confirmation_decisions(&json!({"a": "yes"}), &["a"]).is_err());
    }

    #[test]
    fn test_append_stage_dismissed_concerns_preserves_category_type() {
        let mut items = Vec::new();
        let input = vec![json!({
            "type": "Resource Management",
            "description": "suspected cross-zone page leak does not apply",
            "reasoning": "hugetlb_free_cross_zone_pages() runs before HVO init"
        })];

        append_stage_dismissed_concerns(&mut items, &input, 1, "main");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 1);
        assert_eq!(items[0]["type"], "Resource Management");
        assert_eq!(
            items[0]["reasoning"],
            "hugetlb_free_cross_zone_pages() runs before HVO init"
        );
    }

    #[test]
    fn test_append_stage_dismissed_concerns_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("suspected missing cleanup does not apply")];

        append_stage_dismissed_concerns(&mut items, &input, 2, "main");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 2);
        assert_eq!(items[0]["type"], "General");
        assert_eq!(
            items[0]["description"],
            "suspected missing cleanup does not apply"
        );
    }

    #[test]
    fn test_append_stage_items_overwrites_existing_source_stage() {
        let mut items = Vec::new();
        let input = vec![json!({
            "source_stage": 3,
            "type": "Execution flow",
            "description": "already annotated"
        })];

        append_stage_items(&mut items, &input, 4, "main", "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 4);
    }

    #[test]
    fn test_append_stage_items_normalizes_string_items() {
        let mut items = Vec::new();
        let input = vec![json!("plain concern")];

        append_stage_items(&mut items, &input, 6, "main", "General", "description");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["source_stage"], 6);
        assert_eq!(items[0]["type"], "General");
        assert_eq!(items[0]["description"], "plain concern");
    }

    #[test]
    fn test_calculate_series_range_single_patch() {
        let p = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let patches = vec![p.clone()];
        let patches_to_review = vec![p.clone()];
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_last() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p2.clone()]; // Reviewing last
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_middle() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()]; // Reviewing first
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2".to_string())
        );
    }

    #[test]
    fn test_calculate_series_range_use_patch_shas_map() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None, // Missing in input
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()];

        let mut patch_shas = std::collections::HashMap::new();
        patch_shas.insert(2, "sha2_resolved".to_string());

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2_resolved".to_string())
        );
    }

    struct MockProviderAlwaysFails;
    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderAlwaysFails {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            anyhow::bail!("mock: simulated AI failure")
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_stage_failure_aborts_review() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockProviderAlwaysFails);
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            main_model: "mock".to_string(),
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            custom_prompt: None,
            stages: None,
            dump_conversation: None,
            budget: None,
            merge_budget: None,
            retry_provider: None,
            additional_models: Vec::new(),
            cohort: test_cohort(),
            validation_budget: None,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        match worker.run(patchset, None).await {
            Ok(_) => panic!("Expected stage failure error, got Ok"),
            Err(e) => assert!(
                e.to_string().contains("simulated AI failure"),
                "unexpected error: {e}"
            ),
        }
    }

    // ReviewError tests

    #[test]
    fn test_limit_exceeded_classifies_as_fatal() {
        let err = ReviewError::LimitExceeded;

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_budget_exceeded_classifies_as_fatal() {
        let err = ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_format_rejection_classifies_as_fatal() {
        let err = ReviewError::FormatRejection("contains markdown code blocks".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_limit_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error = ReviewError::LimitExceeded.into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "LimitExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_budget_exceeded_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::BudgetExceeded("1000 tokens used (limit: 500)".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "BudgetExceeded must downcast to ReviewError so the retry loop can fail fast"
        );
    }

    #[test]
    fn test_generic_error_does_not_downcast_as_review_error() {
        let err: anyhow::Error = anyhow::anyhow!("transient JSON parse failure");
        assert!(
            err.downcast_ref::<ReviewError>().is_none(),
            "Plain anyhow errors must NOT match ReviewError so they remain retryable"
        );
    }

    #[test]
    fn test_format_rejection_downcasts_as_review_error() {
        let err: anyhow::Error =
            ReviewError::FormatRejection("contains markdown code blocks".to_string()).into();
        assert!(
            err.downcast_ref::<ReviewError>().is_some(),
            "FormatRejection must downcast to ReviewError"
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockProviderDuplicateCalls {
        turn: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderDuplicateCalls {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            if turn == 0 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_1".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else if turn == 1 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_2".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else {
                Ok(crate::ai::AiResponse {
                    content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: None,
                    usage: None,
                    truncated: false,
                })
            }
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    struct MockProviderNonConsecutiveDuplicate {
        turn: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockProviderNonConsecutiveDuplicate {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            if turn == 0 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_1".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else if turn == 1 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_2".to_string(),
                        function_name: "git_ls".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else if turn == 2 {
                Ok(crate::ai::AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: Some(vec![crate::ai::ToolCall {
                        id: "call_3".to_string(),
                        function_name: "git_log".to_string(),
                        arguments: json!({"revision": "HEAD"}),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                })
            } else {
                Ok(crate::ai::AiResponse {
                    content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                    thought: None,
                    thought_signature: None,
                    reasoning: None,
                    tool_calls: None,
                    usage: None,
                    truncated: false,
                })
            }
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_duplicate_tool_call_blocked() {
        let temp_dir = tempfile::tempdir().unwrap();
        let provider = std::sync::Arc::new(MockProviderDuplicateCalls {
            turn: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let mut session = ReviewStageSession::new(
            create_stage(1),
            "sys".to_string(),
            "user".to_string(),
            "user".to_string(),
            std::sync::Arc::new(tools),
            0.0,
            None,
        );
        let runner = SessionRunner::new(provider.as_ref()).with_max_validation_attempts(3);

        let res = runner.run(&mut session).await;

        assert!(res.is_ok());
        let result = res.unwrap();
        let stage_history = result.history;
        assert_eq!(stage_history.len(), 6);

        let blocked_msg = &stage_history[4];
        assert_eq!(blocked_msg.role, AiRole::Tool);
        let content = blocked_msg.content.as_ref().unwrap();
        assert!(content.contains("Duplicate tool call blocked"));
    }

    #[tokio::test]
    async fn test_non_consecutive_duplicate_allowed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let provider = std::sync::Arc::new(MockProviderNonConsecutiveDuplicate {
            turn: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let mut session = ReviewStageSession::new(
            create_stage(1),
            "sys".to_string(),
            "user".to_string(),
            "user".to_string(),
            std::sync::Arc::new(tools),
            0.0,
            None,
        );
        let runner = SessionRunner::new(provider.as_ref()).with_max_validation_attempts(3);

        let res = runner.run(&mut session).await;

        assert!(res.is_ok());
        let result = res.unwrap();
        let stage_history = result.history;
        assert_eq!(stage_history.len(), 8);

        let response_msg = &stage_history[6];
        assert_eq!(response_msg.role, AiRole::Tool);
        let content = response_msg.content.as_ref().unwrap();
        assert!(!content.contains("Duplicate tool call detected"));
    }

    struct MockBlockedProvider {
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for MockBlockedProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked (finish reason: RECITATION)"
                )
            } else {
                let has_filter = request.messages.iter().any(|m| {
                    m.role == crate::ai::AiRole::User
                        && m.content
                            .as_ref()
                            .is_some_and(|c| c.contains("recitation filter"))
                });
                if has_filter {
                    return Ok(crate::ai::AiResponse {
                        content: Some(r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()),
                        thought: None,
                        thought_signature: None,
                        reasoning: None,
                        tool_calls: None,
                        usage: None,
                        truncated: false,
                    });
                }
                anyhow::bail!(
                    "Remote AI Error: Gemini candidate blocked again (finish reason: RECITATION)"
                )
            }
        }

        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn test_recitation_error_triggers_prompt_perturbation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let provider = std::sync::Arc::new(MockBlockedProvider {
            attempts: AtomicUsize::new(0),
        });
        let tools = crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf(), None);
        let prompts = PromptRegistry::new(prompts_dir);
        let config = WorkerConfig {
            main_model: "mock".to_string(),
            max_input_tokens: 10000,
            max_interactions: 3,
            temperature: 0.0,
            series_range: None,
            custom_prompt: None,
            stages: Some(vec![1]),
            dump_conversation: None,
            budget: None,
            merge_budget: None,
            retry_provider: None,
            additional_models: Vec::new(),
            cohort: test_cohort(),
            validation_budget: None,
        };
        let mut worker = Worker::new(provider, std::sync::Arc::new(tools), prompts, config);

        let patchset = serde_json::json!({
            "id": 1,
            "patch_index": 1,
            "patches": [{"diff": "diff --git a/foo.c b/foo.c\n+int x;"}]
        });

        let res = worker.run(patchset, None).await;
        if let Err(e) = &res {
            panic!("Expected run to succeed, got error: {:?}", e);
        }
    }

    #[test]
    fn prefix_gate_elects_one_opener_per_model_and_log_variant() {
        let mut gates = std::collections::HashMap::new();

        // First stage of a group warms the cache and does not wait.
        let (opener, waiter) = claim_prefix_gate(&mut gates, "main", true, true);
        assert!(opener.is_some());
        assert!(waiter.is_none());

        // Later stages in the same group wait instead of writing the cache again.
        let (second, second_waiter) = claim_prefix_gate(&mut gates, "main", true, true);
        assert!(second.is_none());
        assert!(second_waiter.is_some());

        // The other log variant is a distinct prefix, so it gets its own opener.
        let (nolog, nolog_waiter) = claim_prefix_gate(&mut gates, "main", false, true);
        assert!(nolog.is_some());
        assert!(nolog_waiter.is_none());

        // A variant model has its own provider and must not wait on the main model.
        let (variant, variant_waiter) = claim_prefix_gate(&mut gates, "fable", true, true);
        assert!(variant.is_some());
        assert!(variant_waiter.is_none());

        assert_eq!(gates.len(), 3);
    }

    #[test]
    fn prefix_gate_is_skipped_when_the_provider_does_not_cache() {
        let mut gates = std::collections::HashMap::new();
        for _ in 0..3 {
            let (opener, waiter) = claim_prefix_gate(&mut gates, "main", true, false);
            assert!(opener.is_none(), "no stage should be held back");
            assert!(waiter.is_none(), "no stage should wait");
        }
        assert!(gates.is_empty());
    }

    #[tokio::test]
    async fn prefix_gate_releases_waiters_once_the_cache_is_warm() {
        let mut gates = std::collections::HashMap::new();
        let (opener, _) = claim_prefix_gate(&mut gates, "main", true, true);
        let (_, waiter) = claim_prefix_gate(&mut gates, "main", true, true);
        let mut waiter = waiter.expect("second stage waits");

        assert!(!*waiter.borrow_and_update(), "gate starts closed");
        opener.expect("first stage opens").open();
        assert!(waiter.changed().await.is_ok());
        assert!(*waiter.borrow_and_update(), "gate is open");
    }

    #[tokio::test]
    async fn dropping_the_opener_releases_waiters() {
        let mut gates = std::collections::HashMap::new();
        let (opener, _) = claim_prefix_gate(&mut gates, "main", true, true);
        let (_, waiter) = claim_prefix_gate(&mut gates, "main", true, true);
        let mut waiter = waiter.expect("second stage waits");

        // A stage that dies before its first response must not strand its siblings.
        drop(opener);
        assert!(waiter.changed().await.is_ok());
        assert!(*waiter.borrow_and_update(), "drop opens the gate");
    }
}
