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

use crate::{
    ai::review_budget::{BudgetConfig, ReviewBudget},
    git_ops::{GitWorktree, extract_patch_metadata, get_commit_hash, resolve_git_range},
    settings::{AiSettings, SemcodeSettings, Settings},
    toolbox::ToolBox,
    worker::{PatchInput, ReviewInput, Worker, WorkerConfig, prompts::PromptRegistry},
};
use anyhow::{Context, Result, anyhow};
use futures::stream::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{error, info};

#[derive(Clone, Debug)]
pub struct WorkerOptions {
    pub settings_path: Option<PathBuf>,
    pub baseline: Option<String>,
    pub repo: Option<PathBuf>,
    pub worktree_dir: Option<PathBuf>,
    pub prompts: PathBuf,
    pub review_patch_index: Option<i64>,
    pub review_commit: Option<String>,
    pub no_ai: bool,
    pub reuse_worktree: Option<PathBuf>,
    pub ai_provider: Option<String>,
    pub custom_prompt: Option<String>,
    pub stages: Option<Vec<u8>>,
    pub scratch_clone: bool,
    pub current_tree: bool,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            settings_path: None,
            baseline: None,
            repo: None,
            worktree_dir: None,
            prompts: PathBuf::from("third_party/prompts/kernel"),
            review_patch_index: None,
            review_commit: None,
            no_ai: false,
            reuse_worktree: None,
            ai_provider: None,
            custom_prompt: None,
            stages: None,
            scratch_clone: false,
            current_tree: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReviewOptions {
    pub baseline: Option<String>,
    pub settings_path: Option<PathBuf>,
    pub prompts: PathBuf,
    pub no_ai: bool,
    pub ai_provider: Option<String>,
    pub custom_prompt: Option<String>,
    pub stages: Option<Vec<u8>>,
}

impl Default for ReviewOptions {
    fn default() -> Self {
        Self {
            baseline: None,
            settings_path: None,
            prompts: PathBuf::from("third_party/prompts/kernel"),
            no_ai: false,
            ai_provider: None,
            custom_prompt: None,
            stages: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProgressEvent {
    ResolvingInput {
        input: String,
    },
    ResolvedCommits {
        commits: Vec<CommitSummary>,
    },
    BaselineResolved {
        rev: String,
        sha: String,
    },
    CurrentTreeReady {
        path: PathBuf,
    },
    WorktreeCreated {
        path: PathBuf,
    },
    ApplyingPatch {
        index: i64,
        total: usize,
        subject: String,
    },
    PatchApplied {
        index: i64,
    },
    PatchFailed {
        index: i64,
        error: String,
    },
    AiReviewStarted {
        patches: usize,
    },
    AiReviewPreScreenStarted {
        patch_index: i64,
    },
    AiReviewPlanningStarted {
        patch_index: i64,
    },
    AiReviewPlanReady {
        patch_index: i64,
        planned_stages: Vec<u8>,
    },
    AiReviewStageStarted {
        patch_index: i64,
        stage: u8,
    },
    AiReviewStageTurn {
        patch_index: i64,
        stage: u8,
        turn: usize,
        max_turns: usize,
    },
    AiReviewStageFinished {
        patch_index: i64,
        stage: u8,
    },
    AiReviewAttempt {
        patch_index: i64,
        attempt: usize,
        max_attempts: usize,
    },
    AiReviewFinished {
        patch_index: i64,
    },
    ReviewComplete,
}

#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub index: i64,
    pub sha: String,
    pub subject: String,
    pub author: String,
}

pub type ProgressCallback<'a> = dyn Fn(ProgressEvent) + Send + Sync + 'a;

pub async fn build_review_input_from_git(
    repo_path: &Path,
    input: &str,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<(ReviewInput, Vec<String>)> {
    emit(
        progress,
        ProgressEvent::ResolvingInput {
            input: input.to_string(),
        },
    );

    let shas = if input.contains("..") {
        resolve_git_range(repo_path, input).await?
    } else {
        vec![get_commit_hash(repo_path, input).await?]
    };

    let mut patches = Vec::new();
    let mut summaries = Vec::new();

    for (i, sha) in shas.iter().enumerate() {
        let meta = extract_patch_metadata(repo_path, sha)
            .await
            .with_context(|| format!("Failed to extract metadata for commit {}", sha))?;
        let index = (i + 1) as i64;
        summaries.push(CommitSummary {
            index,
            sha: short_sha(sha),
            subject: meta.subject.clone(),
            author: meta.author.clone(),
        });
        patches.push(PatchInput {
            index,
            diff: meta.diff,
            subject: Some(meta.subject),
            author: Some(meta.author),
            date: Some(meta.timestamp),
            message_id: None,
            commit_id: Some(sha.clone()),
        });
    }

    emit(
        progress,
        ProgressEvent::ResolvedCommits { commits: summaries },
    );

    let subject = patches
        .first()
        .and_then(|p| p.subject.clone())
        .unwrap_or_else(|| input.to_string());

    Ok((
        ReviewInput {
            id: 0,
            subject,
            patches,
        },
        shas,
    ))
}

pub async fn run_git_review(
    repo_path: PathBuf,
    input: String,
    options: ReviewOptions,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    let (review_input, shas) = build_review_input_from_git(&repo_path, &input, progress).await?;
    let baseline = options
        .baseline
        .clone()
        .or_else(|| shas.first().map(|sha| format!("{}^", sha)));

    run_worker(
        review_input,
        WorkerOptions {
            settings_path: options
                .settings_path
                .or_else(|| Some(Settings::local_review_path())),
            baseline,
            prompts: options.prompts,
            no_ai: options.no_ai,
            ai_provider: options.ai_provider,
            custom_prompt: options.custom_prompt,
            stages: options.stages,
            current_tree: true,
            ..WorkerOptions::default()
        },
        Some(repo_path),
        progress,
    )
    .await
}

pub async fn run_worker(
    input: ReviewInput,
    options: WorkerOptions,
    repo_override: Option<PathBuf>,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    let (mut ai, configured_repo_path, concurrency, semcode) =
        if let Some(path) = &options.settings_path {
            let local_settings = Settings::local_review_from_file(path)
                .with_context(|| format!("Failed to load settings from {}", path.display()))?;
            let concurrency = local_settings
                .review
                .and_then(|r| r.concurrency)
                .unwrap_or(4);
            (local_settings.ai, None, concurrency, local_settings.semcode)
        } else if repo_override.is_some() {
            let local_settings = Settings::local_review_settings()
                .context("Failed to load local review settings")?;
            let concurrency = local_settings
                .review
                .and_then(|r| r.concurrency)
                .unwrap_or(4);
            (local_settings.ai, None, concurrency, local_settings.semcode)
        } else {
            let settings = Settings::new().context("Failed to load settings")?;
            (
                settings.ai,
                Some(PathBuf::from(settings.git.repository_path)),
                settings.review.concurrency,
                settings.semcode,
            )
        };

    if let Some(provider) = &options.ai_provider {
        ai.provider = provider.clone();
    }

    let patchset_id = input.id;
    let subject = input.subject;
    let patches = input.patches;
    let baseline_arg = if options.current_tree {
        options.baseline.clone().unwrap_or_default()
    } else {
        options
            .baseline
            .clone()
            .unwrap_or_else(|| "HEAD".to_string())
    };
    let repo_path = repo_override
        .or(configured_repo_path)
        .ok_or_else(|| anyhow!("Missing repository path"))?;

    let (worktree, baseline_sha) = if options.current_tree {
        emit(
            progress,
            ProgressEvent::CurrentTreeReady {
                path: repo_path.clone(),
            },
        );
        (
            GitWorktree::from_path(repo_path.clone(), repo_path.clone()),
            baseline_arg.clone(),
        )
    } else if let Some(path) = &options.reuse_worktree {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        info!("Reusing existing worktree at {:?}", path);
        (
            GitWorktree::from_path(path.clone(), repo_path.clone()),
            baseline_sha,
        )
    } else if options.scratch_clone {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        let worktree = GitWorktree::new_scratch_clone(
            &repo_path,
            &baseline_sha,
            options.worktree_dir.as_deref(),
        )
        .await?;
        emit(
            progress,
            ProgressEvent::WorktreeCreated {
                path: worktree.path.clone(),
            },
        );
        (worktree, baseline_sha)
    } else {
        let baseline_sha = get_commit_hash(&repo_path, &baseline_arg).await?;
        emit(
            progress,
            ProgressEvent::BaselineResolved {
                rev: baseline_arg.clone(),
                sha: short_sha(&baseline_sha),
            },
        );
        let worktree =
            GitWorktree::new(&repo_path, &baseline_sha, options.worktree_dir.as_deref()).await?;
        emit(
            progress,
            ProgressEvent::WorktreeCreated {
                path: worktree.path.clone(),
            },
        );
        (worktree, baseline_sha)
    };

    let result = run_worker_in_worktree(
        &worktree,
        &ai,
        concurrency,
        patchset_id,
        subject,
        patches,
        &baseline_arg,
        &baseline_sha,
        &options,
        semcode.as_ref(),
        progress,
    )
    .await;

    if !options.current_tree
        && let Err(e) = worktree.remove().await
    {
        error!("Failed to remove worktree: {}", e);
    }
    emit(progress, ProgressEvent::ReviewComplete);

    result
}

#[allow(clippy::too_many_arguments)]
async fn review_single_patch(
    worktree: &GitWorktree,
    ai: &AiSettings,
    patchset_id: i64,
    subject: &str,
    p: &PatchInput,
    rich_patches: &[Value],
    patch_shas: &HashMap<i64, String>,
    options: &WorkerOptions,
    baseline_sha: &str,
    progress: Option<&ProgressCallback<'_>>,
    semcode: Option<&SemcodeSettings>,
) -> Result<Value> {
    let mut last_error = None;
    let mut prior_merge_usage = MergeUsageTotals::default();
    for attempt in 1..=3 {
        emit(
            progress,
            ProgressEvent::AiReviewAttempt {
                patch_index: p.index,
                attempt,
                max_attempts: 3,
            },
        );

        if attempt > 1 {
            info!(
                "Restarting AI review for patch {} (attempt {}/3)...",
                p.index, attempt
            );
        }

        let effective_ai = ai_for_attempt(ai, attempt);
        let main_provider_id = std::env::var("SASHIKO_EXPERIMENT_MAIN_PROVIDER")
            .unwrap_or_else(|_| effective_ai.provider.clone());
        let cohort = crate::ai::model_experiment::ReviewCohort {
            main: crate::ai::model_experiment::SourceIdentity {
                name: "main".to_string(),
                display_name: effective_ai.name.clone(),
                provider: main_provider_id.clone(),
                model: effective_ai.model.clone(),
            },
            variants: effective_ai
                .additional_models
                .iter()
                .map(|model| {
                    let variant_ai = model.effective_ai(&effective_ai);
                    crate::ai::model_experiment::CohortMember {
                        source: crate::ai::model_experiment::SourceIdentity {
                            name: model.name.clone(),
                            display_name: model.name.clone(),
                            provider: model
                                .provider
                                .clone()
                                .unwrap_or_else(|| main_provider_id.clone()),
                            model: variant_ai.model,
                        },
                        selected: crate::ai::model_experiment::sampled_for_review(
                            model.probability,
                            patchset_id,
                            p.index,
                            &model.name,
                        ),
                    }
                })
                .collect(),
        };
        let provider = crate::ai::create_provider_from_ai(&effective_ai)
            .context("Failed to create AI provider")?;
        let retry_provider = build_retry_provider(&effective_ai);
        let additional_models = effective_ai
            .additional_models
            .iter()
            .filter(|model| cohort.contains(&model.name))
            .map(|model| {
                let mut variant_ai = model.effective_ai(&effective_ai);
                let provider_id = model
                    .provider
                    .clone()
                    .unwrap_or_else(|| main_provider_id.clone());
                let is_stdio = effective_ai.provider.starts_with("stdio-");
                if is_stdio {
                    variant_ai.provider.clone_from(&effective_ai.provider);
                }
                let provider =
                    crate::ai::create_provider_from_ai(&variant_ai).unwrap_or_else(|error| {
                        tracing::warn!(
                            "Model experiment {} provider is unavailable: {}",
                            model.name,
                            error
                        );
                        Arc::new(crate::ai::model_experiment::UnavailableProvider::new(
                            error.to_string(),
                            variant_ai.model.clone(),
                        ))
                    });
                let provider: std::sync::Arc<dyn crate::ai::AiProvider> = if is_stdio {
                    std::sync::Arc::new(crate::ai::model_experiment::RoutedProvider::new(
                        provider,
                        model.name.clone(),
                    ))
                } else {
                    provider
                };
                crate::worker::AdditionalModelRunner {
                    name: model.name.clone(),
                    provider,
                    temperature: variant_ai.temperature,
                    max_interactions: variant_ai.max_interactions,
                    model_id: variant_ai.model.clone(),
                    provider_id,
                    budget: Some(ReviewBudget::new(variant_ai.discovery_budget_config())),
                }
            })
            .collect::<Vec<_>>();
        let prompts_tool_path = Some(options.prompts.join("tool.md"));

        let mut patch_files = Vec::new();
        if let Some(sha) = patch_shas.get(&p.index) {
            let output = tokio::process::Command::new("git")
                .current_dir(&worktree.path)
                .args(["diff-tree", "--no-commit-id", "--name-only", "-r", sha])
                .output()
                .await;
            if let Ok(out) = output
                && out.status.success()
            {
                let file_list = String::from_utf8_lossy(&out.stdout);
                for file in file_list.lines() {
                    let trimmed = file.trim().to_string();
                    if !trimmed.is_empty() {
                        patch_files.push(trimmed);
                    }
                }
            }
        }
        info!(
            "Active patch files gathered for patch {}: {:?}",
            p.index, patch_files
        );

        let mut tools = ToolBox::new(worktree.path.clone(), prompts_tool_path);
        if let Some(settings) = semcode.filter(|settings| settings.enabled) {
            match crate::worker::semcode_tools::SemcodeToolBox::start(settings, &worktree.path)
                .await
            {
                Ok(semcode) => tools = tools.with_semcode(semcode),
                Err(error) => tracing::warn!("Failed to start semcode tools: {}", error),
            }
        }
        tools.set_active_patch_files(patch_files);

        if let Some(sha) = patch_shas.get(&p.index) {
            info!("Setting virtual HEAD to {} for patch {}", sha, p.index);
            tools.set_virtual_head(sha.clone());
        }

        let prompts = PromptRegistry::new(options.prompts.clone());
        let series_range = patch_shas
            .get(&p.index)
            .map(|sha| format!("{}..{}", baseline_sha, sha));

        let discovery_budget = ReviewBudget::new(ai.discovery_budget_config());
        let merge_budget = ReviewBudget::new(ai.merge_budget_config());
        let merge_budget_observer = merge_budget.clone();
        let mut worker = Worker::new(
            provider,
            std::sync::Arc::new(tools),
            prompts,
            WorkerConfig {
                main_model: effective_ai.model.clone(),
                max_input_tokens: ai.max_input_tokens,
                max_interactions: ai.max_interactions,
                temperature: ai.temperature,
                custom_prompt: options.custom_prompt.clone(),
                series_range,
                stages: options.stages.clone(),
                dump_conversation: ai.dump_conversation.as_ref().map(PathBuf::from),
                budget: Some(discovery_budget),
                merge_budget: Some(merge_budget),
                retry_provider,
                additional_models,
                cohort,
                validation_budget: {
                    let limits = &effective_ai.model_experiments.validation_budget;
                    if limits.request_input_tokens == 0
                        && limits.request_output_tokens == 0
                        && limits.review_input_tokens == 0
                        && limits.review_output_tokens == 0
                    {
                        None
                    } else {
                        Some(BudgetConfig {
                            stage_input: limits.request_input_tokens,
                            stage_output: limits.request_output_tokens,
                            review_input: limits.review_input_tokens,
                            review_output: limits.review_output_tokens,
                            warn_pct: effective_ai.budget_warn_pct,
                            severe_pct: effective_ai.budget_severe_pct,
                            review_multiplier: 0.0,
                            enforce_hard_limits: true,
                        })
                    }
                },
            },
        );

        let p_index = p.index;
        let progress_cb = progress.map(|cb| {
            move |event| match event {
                crate::worker::WorkerProgressEvent::PreScreenStarted => {
                    cb(ProgressEvent::AiReviewPreScreenStarted {
                        patch_index: p_index,
                    });
                }
                crate::worker::WorkerProgressEvent::PlanningStarted => {
                    cb(ProgressEvent::AiReviewPlanningStarted {
                        patch_index: p_index,
                    });
                }
                crate::worker::WorkerProgressEvent::ReviewStarted { planned_stages } => {
                    cb(ProgressEvent::AiReviewPlanReady {
                        patch_index: p_index,
                        planned_stages,
                    });
                }
                crate::worker::WorkerProgressEvent::StageStarted { stage } => {
                    cb(ProgressEvent::AiReviewStageStarted {
                        patch_index: p_index,
                        stage,
                    });
                }
                crate::worker::WorkerProgressEvent::StageTurn {
                    stage,
                    turn,
                    max_turns,
                } => {
                    cb(ProgressEvent::AiReviewStageTurn {
                        patch_index: p_index,
                        stage,
                        turn,
                        max_turns,
                    });
                }
                crate::worker::WorkerProgressEvent::StageFinished { stage } => {
                    cb(ProgressEvent::AiReviewStageFinished {
                        patch_index: p_index,
                        stage,
                    });
                }
            }
        });

        let patchset_val = json!({
            "id": patchset_id,
            "subject": subject,
            "patches": rich_patches,
            "patch_index": Some(p.index)
        });

        match worker
            .run(
                patchset_val,
                progress_cb
                    .as_ref()
                    .map(|f| f as &(dyn Fn(_) + Send + Sync)),
            )
            .await
        {
            Ok(mut result) => {
                let mut merge_usage = prior_merge_usage;
                merge_usage.add_snapshot(merge_budget_observer.snapshot());
                if let Some(output) = result.output.as_mut()
                    && let Some(object) = output.as_object_mut()
                {
                    object.insert("merge_usage".to_string(), merge_usage.to_json());
                }
                result.tokens_in = result
                    .tokens_in
                    .saturating_add(u32::try_from(prior_merge_usage.tokens_in).unwrap_or(u32::MAX));
                result.tokens_out = result.tokens_out.saturating_add(
                    u32::try_from(prior_merge_usage.tokens_out).unwrap_or(u32::MAX),
                );
                result.tokens_cached = result.tokens_cached.saturating_add(
                    u32::try_from(prior_merge_usage.tokens_cached).unwrap_or(u32::MAX),
                );
                info!("AI review completed for patch {}.", p.index);
                emit(
                    progress,
                    ProgressEvent::AiReviewFinished {
                        patch_index: p.index,
                    },
                );

                let mut inline_content = None;
                if let Some(output) = &result.output
                    && let Some(content) = output.get("review_inline").and_then(|v| v.as_str())
                {
                    inline_content = Some(content.to_string());
                }

                let mut has_findings = false;
                if let Some(output) = &result.output
                    && let Some(findings) = output.get("findings").and_then(|f| f.as_array())
                    && !findings.is_empty()
                {
                    has_findings = true;
                }

                if has_findings && inline_content.is_none() {
                    error!(
                        "Review failure on patch {}: Findings detected but review_inline field was missing or empty.",
                        p.index
                    );
                    if attempt < 3 {
                        prior_merge_usage.add_snapshot(merge_budget_observer.snapshot());
                        continue;
                    }
                }

                return Ok(json!({
                    "patch_index": p.index,
                    "review": result.output,
                    "error": result.error,
                    "inline_review": inline_content,
                    "input_context": result.input_context,
                    "history": result.history,
                    "tokens_in": result.tokens_in,
                    "tokens_out": result.tokens_out,
                    "tokens_cached": result.tokens_cached
                }));
            }
            Err(e) => {
                prior_merge_usage.add_snapshot(merge_budget_observer.snapshot());
                error!(
                    "AI review for patch {} failed with exception: {}",
                    p.index, e
                );
                if attempt == 3 {
                    return Ok(json!({
                        "patch_index": p.index,
                        "review": null,
                        "error": e.to_string(),
                        "inline_review": null,
                        "input_context": "",
                        "history": [],
                        "merge_usage": prior_merge_usage.to_json(),
                        "tokens_in": prior_merge_usage.tokens_in,
                        "tokens_out": prior_merge_usage.tokens_out,
                        "tokens_cached": prior_merge_usage.tokens_cached,
                    }));
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("Patch review failed")))
}

#[derive(Clone, Copy, Default)]
struct MergeUsageTotals {
    tokens_in: usize,
    tokens_out: usize,
    tokens_cached: usize,
    budget_flags: u8,
}

impl MergeUsageTotals {
    fn add_snapshot(&mut self, snapshot: crate::ai::review_budget::BudgetSnapshot) {
        self.tokens_in = self.tokens_in.saturating_add(snapshot.input);
        self.tokens_out = self.tokens_out.saturating_add(snapshot.output);
        self.tokens_cached = self.tokens_cached.saturating_add(snapshot.cached);
        self.budget_flags |= snapshot.flags;
    }

    fn add_json(&mut self, usage: &Value) {
        self.tokens_in = self
            .tokens_in
            .saturating_add(usage["tokens_in"].as_u64().unwrap_or(0) as usize);
        self.tokens_out = self
            .tokens_out
            .saturating_add(usage["tokens_out"].as_u64().unwrap_or(0) as usize);
        self.tokens_cached = self
            .tokens_cached
            .saturating_add(usage["tokens_cached"].as_u64().unwrap_or(0) as usize);
        self.budget_flags |= usage["budget_flags"].as_u64().unwrap_or(0) as u8;
    }

    fn to_json(self) -> Value {
        json!({
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "tokens_cached": self.tokens_cached,
            "budget_flags": self.budget_flags,
        })
    }
}

struct PrivateReviewMetadata {
    canonical_candidates: Vec<Value>,
    model_experiment: Option<Value>,
    merge_usage: MergeUsageTotals,
    errors: Vec<String>,
}

fn collect_private_review_metadata(
    results: &[Value],
    patches: &[PatchInput],
) -> PrivateReviewMetadata {
    let mut metadata = PrivateReviewMetadata {
        canonical_candidates: Vec::new(),
        model_experiment: None,
        merge_usage: MergeUsageTotals::default(),
        errors: Vec::new(),
    };
    for result in results {
        let patch_index = result["patch_index"].as_i64().unwrap_or(0);
        let patch_subject = patches
            .iter()
            .find(|patch| patch.index == patch_index)
            .and_then(|patch| patch.subject.as_deref())
            .unwrap_or_default();
        if let Some(review) = result.get("review") {
            for candidate in review["canonical_candidates"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let mut candidate = candidate.clone();
                candidate["patch_index"] = json!(patch_index);
                candidate["patch_subject"] = json!(patch_subject);
                metadata.canonical_candidates.push(candidate);
            }
            if metadata.model_experiment.is_none() {
                metadata.model_experiment = review.get("model_experiment").cloned();
            }
        }
        if let Some(usage) = result
            .get("review")
            .and_then(|review| review.get("merge_usage"))
            .or_else(|| result.get("merge_usage"))
        {
            metadata.merge_usage.add_json(usage);
        }
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            metadata.errors.push(error.to_string());
        }
    }
    metadata
}

#[cfg(feature = "bedrock")]
fn ai_for_attempt(ai: &AiSettings, attempt: usize) -> AiSettings {
    let mut effective = ai.clone();
    if attempt > 1
        && let Some(bedrock) = effective.bedrock.as_mut()
        && let Some(retry_effort) = bedrock.retry_effort.clone()
    {
        bedrock.effort = Some(retry_effort);
    }
    effective
}

#[cfg(not(feature = "bedrock"))]
fn ai_for_attempt(ai: &AiSettings, _attempt: usize) -> AiSettings {
    ai.clone()
}

#[cfg(feature = "bedrock")]
fn build_retry_provider(ai: &AiSettings) -> Option<std::sync::Arc<dyn crate::ai::AiProvider>> {
    // retry_effort only means anything to the Bedrock provider. When the review
    // runs as a child process the provider is overridden to a stdio transport
    // (which proxies to the parent's real provider) while the [ai.bedrock]
    // config stays in place, so this must key off the *effective* provider
    // rather than the presence of a bedrock section. Building a second stdio
    // client here would start a second stdin reader on the same pipe, and the
    // two readers would steal each other's responses and split each other's
    // lines.
    if !ai.provider.eq_ignore_ascii_case("bedrock") {
        return None;
    }
    let bedrock = ai.bedrock.as_ref()?;
    let retry_effort = bedrock.retry_effort.as_ref()?;
    if bedrock.effort.as_ref() == Some(retry_effort) {
        return None;
    }
    let mut retry_ai = ai.clone();
    retry_ai.bedrock.as_mut()?.effort = Some(retry_effort.clone());
    match crate::ai::create_provider_from_ai(&retry_ai) {
        Ok(provider) => {
            info!(
                "Retry provider ready (effort {:?}); will activate if review passes budget_warn_pct",
                retry_effort
            );
            Some(provider)
        }
        Err(error) => {
            tracing::warn!("Failed to build retry-effort provider: {}", error);
            None
        }
    }
}

#[cfg(not(feature = "bedrock"))]
fn build_retry_provider(_ai: &AiSettings) -> Option<std::sync::Arc<dyn crate::ai::AiProvider>> {
    None
}

#[allow(clippy::too_many_arguments)]
async fn run_worker_in_worktree(
    worktree: &GitWorktree,
    ai: &AiSettings,
    concurrency: usize,
    patchset_id: i64,
    subject: String,
    patches: Vec<PatchInput>,
    baseline_arg: &str,
    baseline_sha: &str,
    options: &WorkerOptions,
    semcode: Option<&SemcodeSettings>,
    progress: Option<&ProgressCallback<'_>>,
) -> Result<Value> {
    info!("Worktree at {:?}", worktree.path);
    info!("Found {} patches total", patches.len());

    let mut patch_results = Vec::new();
    let mut patch_shas = HashMap::new();
    let mut patch_shows = HashMap::new();
    let mut patch_messages = HashMap::new();

    let all_applied = if options.current_tree {
        for p in &patches {
            if let Some(sha) = &p.commit_id {
                patch_shas.insert(p.index, sha.clone());
                if let Ok(show) = worktree.get_commit_show(sha).await {
                    patch_shows.insert(p.index, show);
                }
                if let Ok(msg) = worktree.get_commit_message(sha).await {
                    patch_messages.insert(p.index, msg);
                }
            }
            patch_results.push(json!({
                "index": p.index,
                "status": "present",
                "method": "current-tree",
                "subject": p.subject.clone()
            }));
        }
        true
    } else if let Some(commit_hash) = &options.review_commit {
        info!("Directly reviewing commit {}", commit_hash);
        if let Some(idx) = options.review_patch_index {
            patch_shas.insert(idx, commit_hash.clone());
            if let Ok(show) = worktree.get_commit_show(commit_hash).await {
                patch_shows.insert(idx, show);
            }
            let subject = patches
                .iter()
                .find(|p| p.index == idx)
                .and_then(|p| p.subject.clone());
            patch_results.push(json!({
                "index": idx,
                "status": "applied",
                "method": "pre-applied",
                "subject": subject
            }));
        }
        true
    } else {
        info!(
            "Applying all {} patches to validate series...",
            patches.len()
        );
        let mut applied = true;

        for p in &patches {
            emit(
                progress,
                ProgressEvent::ApplyingPatch {
                    index: p.index,
                    total: patches.len(),
                    subject: p.subject.clone().unwrap_or_else(|| "patch".to_string()),
                },
            );

            let success = apply_single_patch(
                worktree,
                p,
                &mut patch_shas,
                &mut patch_shows,
                &mut patch_messages,
                &mut patch_results,
            )
            .await;

            if success {
                emit(progress, ProgressEvent::PatchApplied { index: p.index });
            } else {
                applied = false;
                let error = patch_results
                    .last()
                    .and_then(|p| p.get("error"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("patch application failed")
                    .to_string();
                emit(
                    progress,
                    ProgressEvent::PatchFailed {
                        index: p.index,
                        error,
                    },
                );
            }
        }
        applied
    };

    let mut patches_to_review: Vec<PatchInput> =
        if let Some(target_idx) = options.review_patch_index {
            patches
                .iter()
                .filter(|p| p.index == target_idx)
                .cloned()
                .collect()
        } else {
            patches.clone()
        };

    if options.no_ai {
        info!("Skipping AI review due to --no-ai flag.");
        patches_to_review.clear();
    }

    if !all_applied {
        info!("Not all patches applied successfully. Skipping AI review.");
        return Ok(json!({
            "patchset_id": patchset_id,
            "baseline": baseline_arg,
            "patches": patch_results,
            "error": "Patch application failed"
        }));
    }

    if patches_to_review.is_empty() {
        info!("No patches matched review index or list empty. Skipping AI review.");
        return Ok(json!({
            "patchset_id": patchset_id,
            "baseline": baseline_arg,
            "patches": patch_results,
            "review": null,
            "input_context": "",
            "tokens_in": 0,
            "tokens_out": 0,
            "tokens_cached": 0
        }));
    }

    info!(
        "Patches applied. Starting AI reviews for {} patches...",
        patches_to_review.len()
    );
    emit(
        progress,
        ProgressEvent::AiReviewStarted {
            patches: patches_to_review.len(),
        },
    );

    let semcode_ready = if let Some(settings) = semcode.filter(|settings| settings.enabled) {
        let setup = async {
            crate::worker::semcode_tools::copy_semcode_db(&worktree.repo_path, &worktree.path)
                .await?;
            let target = patch_shas
                .iter()
                .max_by_key(|(index, _)| *index)
                .map(|(_, sha)| sha.as_str())
                .unwrap_or("HEAD");
            crate::worker::semcode_tools::run_semcode_index(
                settings,
                &worktree.path,
                &format!("{}..{}", baseline_sha, target),
            )
            .await
        };
        if let Err(error) = setup.await {
            tracing::warn!("Semcode setup failed; continuing without it: {}", error);
            false
        } else {
            true
        }
    } else {
        false
    };

    let rich_patches: Vec<Value> = patches_to_review
        .iter()
        .map(|p| {
            let date_str = if let Some(ts) = p.date {
                use chrono::{TimeZone, Utc};
                Utc.timestamp_opt(ts, 0)
                    .single()
                    .map(|dt| dt.to_rfc2822())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            json!({
                "index": p.index,
                "subject": p.subject,
                "author": p.author,
                "date_string": date_str,
                "diff": p.diff,
                "commit_id": patch_shas.get(&p.index).cloned(),
                "git_show": patch_shows.get(&p.index).cloned(),
                "commit_message_full": patch_messages.get(&p.index).cloned()
            })
        })
        .collect();

    // Execute patch reviews concurrently with a limit
    let futures_stream = futures::stream::iter(patches_to_review.iter().map(|p| {
        let rich_patches = rich_patches.clone();
        let patch_shas = &patch_shas;
        let options = &options;
        let subject_clone = subject.clone();
        async move {
            review_single_patch(
                worktree,
                ai,
                patchset_id,
                &subject_clone,
                p,
                &rich_patches,
                patch_shas,
                options,
                baseline_sha,
                progress,
                semcode.filter(|_| semcode_ready),
            )
            .await
        }
    }));

    let mut buffered = futures_stream.buffer_unordered(concurrency);
    let mut results = Vec::new();
    while let Some(res) = buffered.next().await {
        results.push(res?);
    }

    // Aggregate findings, inline reviews, history, input context, and concern counts
    let mut combined_findings = Vec::new();
    let mut combined_dismissed_concerns = Vec::new();
    let mut combined_inline = String::new();
    let mut combined_history = Vec::new();
    let mut combined_input_context = String::new();
    let mut total_tokens_in = 0;
    let mut total_tokens_out = 0;
    let mut total_tokens_cached = 0;
    let mut total_concerns_count = 0;
    let mut total_dismissed_concerns_count = 0;
    let mut budget_flags = 0u64;
    let private_metadata = collect_private_review_metadata(&results, &patches_to_review);

    for res in &results {
        let p_idx = res["patch_index"].as_i64().unwrap_or(0);
        let patch_subject = patches_to_review
            .iter()
            .find(|p| p.index == p_idx)
            .and_then(|p| p.subject.as_ref())
            .cloned()
            .unwrap_or_default();

        if let Some(review) = res.get("review") {
            if let Some(findings) = review.get("findings").and_then(|v| v.as_array()) {
                for f in findings {
                    let mut finding_val = f.clone();
                    finding_val["patch_index"] = json!(p_idx);
                    finding_val["patch_subject"] = json!(patch_subject);
                    combined_findings.push(finding_val);
                }
            }
            if let Some(dismissed) = review.get("dismissed_concerns").and_then(|v| v.as_array()) {
                combined_dismissed_concerns.extend(dismissed.clone());
            }
            if let Some(cc) = review.get("concerns_count").and_then(|v| v.as_u64()) {
                total_concerns_count += cc;
            }
            if let Some(dcc) = review
                .get("dismissed_concerns_count")
                .and_then(|v| v.as_u64())
            {
                total_dismissed_concerns_count += dcc;
            }
            budget_flags |= review
                .get("budget_flags")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
        }

        if let Some(inline) = res["inline_review"].as_str()
            && !inline.trim().is_empty()
            && inline.trim() != "No issues found."
        {
            if !combined_inline.is_empty() {
                combined_inline.push_str("\n\n");
            }
            combined_inline.push_str(&format!("--- Patch [{}]: {} ---\n", p_idx, patch_subject));
            combined_inline.push_str(inline.trim());
        }

        if let Some(hist) = res.get("history").and_then(|h| h.as_array()) {
            combined_history.extend(hist.clone());
        }

        if let Some(inp) = res["input_context"].as_str()
            && !inp.is_empty()
        {
            if !combined_input_context.is_empty() {
                combined_input_context.push_str("\n\n");
            }
            combined_input_context.push_str(inp);
        }

        total_tokens_in += res["tokens_in"].as_u64().unwrap_or(0);
        total_tokens_out += res["tokens_out"].as_u64().unwrap_or(0);
        total_tokens_cached += res["tokens_cached"].as_u64().unwrap_or(0);
    }

    let multi_stage_count = combined_findings
        .iter()
        .filter(|finding| {
            finding
                .get("source_stages")
                .and_then(Value::as_array)
                .is_some_and(|stages| stages.len() > 1)
        })
        .count();
    let unique_findings = combined_findings.len();
    let review_output = json!({
        "findings": combined_findings,
        "dismissed_concerns": combined_dismissed_concerns,
        "concerns_count": total_concerns_count,
        "dismissed_concerns_count": total_dismissed_concerns_count
        ,"budget_flags": budget_flags,
        "canonical_candidates": private_metadata.canonical_candidates,
        "merge_usage": private_metadata.merge_usage.to_json(),
        "model_experiment": private_metadata.model_experiment,
        "dedup_stats": {
            "total_concerns": total_concerns_count,
            "unique_findings": unique_findings,
            "multi_stage_count": multi_stage_count,
            "multi_stage_pct": if unique_findings == 0 { 0.0 } else { multi_stage_count as f64 * 100.0 / unique_findings as f64 }
        }
    });

    let combined_result = json!({
        "patchset_id": patchset_id,
        "baseline": baseline_arg,
        "patches": patch_results,
        "review": review_output,
        "inline_review": if combined_inline.is_empty() { "No issues found.".to_string() } else { combined_inline },
        "history": combined_history,
        "input_context": combined_input_context,
        "tokens_in": total_tokens_in,
        "tokens_out": total_tokens_out,
        "tokens_cached": total_tokens_cached
        ,"merge_usage": private_metadata.merge_usage.to_json()
        ,"error": if private_metadata.errors.is_empty() { Value::Null } else { json!(private_metadata.errors.join("; ")) }
    });

    Ok(combined_result)
}

pub async fn run_worker_from_stdin(options: WorkerOptions) -> Result<Value> {
    let mut buffer = String::new();
    if std::io::stdin().read_line(&mut buffer)? == 0 {
        return Err(anyhow!("No input provided on stdin"));
    }
    let input: ReviewInput = serde_json::from_str(&buffer)?;
    let repo_override = options.repo.clone();
    run_worker(input, options, repo_override, None).await
}

pub fn result_has_error(result: &Value) -> bool {
    result
        .get("error")
        .and_then(|e| e.as_str())
        .map(|e| !e.is_empty())
        .unwrap_or(false)
}

pub fn result_has_high_or_critical_findings(result: &Value) -> bool {
    let Some(findings) = result
        .get("review")
        .and_then(|review| review.get("findings"))
        .and_then(|f| f.as_array())
    else {
        return false;
    };

    findings.iter().any(|finding| {
        let is_new = !finding
            .get("preexisting")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let severity = finding
            .get("severity")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        is_new && matches!(severity.as_str(), "critical" | "high")
    })
}

async fn apply_single_patch(
    worktree: &GitWorktree,
    p: &PatchInput,
    patch_shas: &mut HashMap<i64, String>,
    patch_shows: &mut HashMap<i64, String>,
    patch_messages: &mut HashMap<i64, String>,
    patch_results: &mut Vec<Value>,
) -> bool {
    if let Some(sha) = &p.commit_id {
        info!(
            "Patch {} is identified by commit ID {}, attempting direct checkout...",
            p.index, sha
        );
        return checkout_patch(
            worktree,
            p,
            sha,
            "checkout",
            patch_shas,
            patch_shows,
            patch_messages,
            patch_results,
        )
        .await;
    }

    if let Some(sha) = &p.message_id
        && sha.len() == 40
        && sha.chars().all(|c| c.is_ascii_hexdigit())
    {
        info!(
            "Patch {} message_id looks like a SHA {}, checking out...",
            p.index, sha
        );
        return checkout_patch(
            worktree,
            p,
            sha,
            "checkout",
            patch_shas,
            patch_shows,
            patch_messages,
            patch_results,
        )
        .await;
    }

    if let (Some(author), Some(subject)) = (&p.author, &p.subject) {
        let date_str = if let Some(ts) = p.date {
            use chrono::{TimeZone, Utc};
            Utc.timestamp_opt(ts, 0)
                .single()
                .map(|dt| dt.to_rfc2822())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mbox = format!(
            "From: {}\nDate: {}\nSubject: {}\n\n{}\n",
            author, date_str, subject, p.diff
        );

        match worktree.apply_patch(&mbox).await {
            Ok(_) => {
                if let Ok(sha) = get_commit_hash(&worktree.path, "HEAD").await {
                    patch_shas.insert(p.index, sha.clone());
                    if let Ok(show) = worktree.get_commit_show(&sha).await {
                        patch_shows.insert(p.index, show);
                    }
                    if let Ok(msg) = worktree.get_commit_message(&sha).await {
                        patch_messages.insert(p.index, msg);
                    }
                }
                patch_results.push(json!({
                    "index": p.index,
                    "status": "applied",
                    "method": "git-am",
                    "subject": p.subject.clone()
                }));
                return true;
            }
            Err(e) => {
                error!("git am failed: {}", e);
                patch_results.push(json!({
                    "index": p.index,
                    "status": "error",
                    "method": "git-am",
                    "subject": p.subject.clone(),
                    "error": e.to_string()
                }));
                return false;
            }
        }
    }

    patch_results.push(json!({
        "index": p.index,
        "status": "error",
        "method": "unknown",
        "error": "Missing author or subject for am apply"
    }));
    false
}

#[allow(clippy::too_many_arguments)]
async fn checkout_patch(
    worktree: &GitWorktree,
    p: &PatchInput,
    sha: &str,
    method: &str,
    patch_shas: &mut HashMap<i64, String>,
    patch_shows: &mut HashMap<i64, String>,
    patch_messages: &mut HashMap<i64, String>,
    patch_results: &mut Vec<Value>,
) -> bool {
    match worktree.reset_hard(sha).await {
        Ok(_) => {
            if let Ok(show) = worktree.get_commit_show(sha).await {
                patch_shows.insert(p.index, show);
            }
            if let Ok(msg) = worktree.get_commit_message(sha).await {
                patch_messages.insert(p.index, msg);
            }
            patch_shas.insert(p.index, sha.to_string());
            patch_results.push(json!({
                "index": p.index,
                "status": "applied",
                "method": method,
                "subject": p.subject.clone()
            }));
            true
        }
        Err(e) => {
            error!("Failed to checkout commit {}: {}", sha, e);
            patch_results.push(json!({
                "index": p.index,
                "status": "error",
                "method": method,
                "subject": p.subject.clone(),
                "error": e.to_string()
            }));
            false
        }
    }
}

fn emit(progress: Option<&ProgressCallback<'_>>, event: ProgressEvent) {
    if let Some(progress) = progress {
        progress(event);
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

pub fn print_worker_json(result: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(result)?);
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn private_review_metadata_survives_patch_aggregation() {
        let patches = vec![PatchInput {
            index: 3,
            diff: String::new(),
            subject: Some("Subject".to_string()),
            author: None,
            date: None,
            message_id: None,
            commit_id: None,
        }];
        let results = vec![
            json!({
                "patch_index": 3,
                "review": {
                    "canonical_candidates": [{"finding_ids": ["main-3-0"]}],
                    "model_experiment": {"runs": [{"model": "main"}]},
                    "merge_usage": {
                        "tokens_in": 10,
                        "tokens_out": 4,
                        "tokens_cached": 3,
                        "budget_flags": 2
                    }
                }
            }),
            json!({
                "patch_index": 3,
                "review": null,
                "error": "merge failed",
                "merge_usage": {
                    "tokens_in": 5,
                    "tokens_out": 2,
                    "tokens_cached": 1,
                    "budget_flags": 4
                }
            }),
        ];

        let metadata = collect_private_review_metadata(&results, &patches);
        assert_eq!(metadata.canonical_candidates.len(), 1);
        assert_eq!(metadata.canonical_candidates[0]["patch_index"], 3);
        assert_eq!(
            metadata.model_experiment.unwrap()["runs"][0]["model"],
            "main"
        );
        assert_eq!(metadata.merge_usage.tokens_in, 15);
        assert_eq!(metadata.merge_usage.tokens_cached, 4);
        assert_eq!(metadata.merge_usage.budget_flags, 6);
        assert_eq!(metadata.errors, ["merge failed"]);
    }

    fn git(repo_path: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .current_dir(repo_path)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    async fn test_repo() -> Result<(tempfile::TempDir, PathBuf, String, String)> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        git(&repo_path, &["init"])?;
        git(&repo_path, &["config", "user.email", "test@example.com"])?;
        git(&repo_path, &["config", "user.name", "Test User"])?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Initial")?;
        git(&repo_path, &["add", "."])?;
        git(&repo_path, &["commit", "-m", "Initial"])?;
        let initial_sha = get_commit_hash(&repo_path, "HEAD").await?;

        writeln!(file, "Change")?;
        git(&repo_path, &["add", "."])?;
        git(&repo_path, &["commit", "-m", "Feature"])?;
        let feature_sha = get_commit_hash(&repo_path, "HEAD").await?;

        Ok((temp_dir, repo_path, initial_sha, feature_sha))
    }

    #[tokio::test]
    async fn test_build_review_input_from_git_single_commit() -> Result<()> {
        let (_temp, repo_path, _initial_sha, feature_sha) = test_repo().await?;
        let (input, shas) = build_review_input_from_git(&repo_path, &feature_sha, None).await?;

        assert_eq!(shas, vec![feature_sha.clone()]);
        assert_eq!(input.id, 0);
        assert_eq!(input.patches.len(), 1);
        assert_eq!(input.patches[0].index, 1);
        assert_eq!(
            input.patches[0].commit_id.as_deref(),
            Some(feature_sha.as_str())
        );
        assert_eq!(input.patches[0].subject.as_deref(), Some("Feature"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_remote_checkout() -> Result<()> {
        let (_temp, repo_path, initial_sha, feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "INVALID DIFF content that would fail git apply".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some(feature_sha),
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(success);
        assert_eq!(patch_results[0]["status"], "applied");
        assert_eq!(patch_results[0]["method"], "checkout");
        assert!(std::fs::read_to_string(worktree.path.join("file.txt"))?.contains("Change"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_checkout_failure() -> Result<()> {
        let (_temp, repo_path, initial_sha, _feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "Valid Diff content that would apply if we fell back".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some("some-msg-id".to_string()),
            commit_id: Some("0000000000000000000000000000000000000000".to_string()),
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(!success);
        assert_eq!(patch_results[0]["status"], "error");
        assert_eq!(patch_results[0]["method"], "checkout");

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_single_patch_legacy_message_id_sha() -> Result<()> {
        let (_temp, repo_path, initial_sha, feature_sha) = test_repo().await?;
        let worktree = GitWorktree::new(&repo_path, &initial_sha, None).await?;

        let patch = PatchInput {
            index: 1,
            diff: "INVALID".to_string(),
            subject: Some("Feature".to_string()),
            author: Some("Test User <test@example.com>".to_string()),
            date: None,
            message_id: Some(feature_sha),
            commit_id: None,
        };

        let mut patch_shas = HashMap::new();
        let mut patch_shows = HashMap::new();
        let mut patch_messages = HashMap::new();
        let mut patch_results = Vec::new();

        let success = apply_single_patch(
            &worktree,
            &patch,
            &mut patch_shas,
            &mut patch_shows,
            &mut patch_messages,
            &mut patch_results,
        )
        .await;

        assert!(success);
        assert_eq!(patch_results[0]["status"], "applied");
        assert_eq!(patch_results[0]["method"], "checkout");
        assert!(std::fs::read_to_string(worktree.path.join("file.txt"))?.contains("Change"));

        Ok(())
    }
}
