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

use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct SubsystemMapping {
    pub pattern: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct SubsystemsSettings {
    #[serde(default)]
    pub mapping: Vec<SubsystemMapping>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct ProjectSettings {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ForgeSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub disable_nntp: bool,
    pub provider: Option<String>,
    pub webhook_secret: Option<String>,
    pub api_token: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct DatabaseSettings {
    pub url: String,
    pub token: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct NntpSettings {
    pub server: String,
    pub port: u16,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct SmtpSettings {
    pub server: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub sender_address: String,
    pub reply_to: Option<String>,
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

fn default_dry_run() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct MailingListsSettings {
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub track: Vec<String>,
}

fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("string or list of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect())
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let mut vec = Vec::new();
            while let Some(elem) = seq.next_element()? {
                vec.push(elem);
            }
            Ok(vec)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

fn default_max_input_tokens() -> usize {
    150_000
}

fn default_main_source_name() -> String {
    "main".to_string()
}

fn deserialize_main_source_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let name = String::deserialize(deserializer)?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(serde::de::Error::custom(
            "main model name must contain only ASCII letters, digits, '_' or '-'",
        ));
    }
    Ok(name)
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ClaudeSettings {
    #[serde(default = "default_prompt_caching")]
    pub prompt_caching: bool,
    #[serde(default = "default_claude_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

fn default_claude_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct GeminiSettings {
    #[serde(default)]
    pub explicit_prompt_caching: bool,
}

#[cfg(feature = "bedrock")]
#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct BedrockSettings {
    /// AWS region for Bedrock API calls (e.g. "us-east-1").
    /// If omitted, uses the standard AWS SDK default chain.
    pub region: Option<String>,
    #[serde(default = "default_prompt_caching")]
    pub prompt_caching: bool,
    /// Max output tokens per Converse call.
    #[serde(default = "default_bedrock_max_tokens")]
    pub max_tokens: u32,
    /// Thinking mode sent as additional_model_request_fields. Opus 4.7+ only accepts "adaptive".
    /// Leave unset to omit. On Opus 4.7/4.8 that disables thinking; on Opus 5 thinking is on by
    /// default, so omitting it is equivalent to "adaptive". Valid values: "adaptive".
    #[serde(default)]
    pub thinking: Option<String>,
    /// output_config.effort level. Valid values: "low", "medium", "high", "xhigh", "max".
    /// Leave unset to use the model default ("high"). "xhigh" requires Opus 4.7 or newer.
    #[serde(default)]
    pub effort: Option<String>,
    /// Effort level used for retries and after the review budget warning.
    #[serde(default)]
    pub retry_effort: Option<String>,
}

#[cfg(feature = "bedrock")]
fn default_bedrock_max_tokens() -> u32 {
    8192
}

fn default_prompt_caching() -> bool {
    true
}

#[cfg(feature = "vertex")]
#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct VertexSettings {
    /// GCP project ID. Falls back to ANTHROPIC_VERTEX_PROJECT_ID env var.
    #[serde(default)]
    pub project_id: Option<String>,
    /// GCP region (e.g., "us-east5", "global"). Falls back to CLOUD_ML_REGION env var.
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default = "default_prompt_caching")]
    pub prompt_caching: bool,
    #[serde(default = "default_vertex_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

#[cfg(feature = "vertex")]
fn default_vertex_max_tokens() -> u32 {
    8192
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct OpenAiCompatSettings {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub context_window_size: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct OllamaSettings {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub context_window_size: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub think: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct KiroCliSettings {
    #[serde(default = "default_kiro_cli_binary")]
    pub binary: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "default_kiro_cli_context_window")]
    pub context_window_size: usize,
}

fn default_kiro_cli_binary() -> String {
    "kiro-cli".to_string()
}

fn default_kiro_cli_context_window() -> usize {
    200_000
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ClaudeCliSettings {
    /// Effort level passed to `claude --effort`. Valid values per Claude Code:
    /// "low", "medium", "high", "xhigh", "max". Leave unset for the model default.
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct DevinCliSettings {
    /// Path to a Devin declarative agent config file (JSON or YAML) passed via
    /// `--agent-config`. Use this to disable all tools for a strictly
    /// text-completion backend.
    #[serde(default)]
    pub agent_config: Option<String>,
    /// Path to a Devin config file passed via `--config`. Use this to apply
    /// custom permission rules (e.g. deny-all) for the provider session
    /// without polluting the user's `~/.config/devin/config.json`.
    #[serde(default)]
    pub config: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct AdditionalModelSettings {
    /// Stable name used in experiment metadata and provider routing.
    pub name: String,
    /// Probability that this model runs for an entire patch review.
    #[serde(deserialize_with = "deserialize_probability")]
    pub probability: f64,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub max_input_tokens: Option<usize>,
    #[serde(default)]
    pub max_interactions: Option<usize>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub api_timeout_secs: Option<u64>,
    #[serde(default)]
    pub budget: SourceBudgetSettings,
    #[serde(default)]
    pub claude: Option<ClaudeSettings>,
    #[serde(default)]
    pub gemini: Option<GeminiSettings>,
    #[cfg(feature = "bedrock")]
    #[serde(default)]
    pub bedrock: Option<BedrockSettings>,
    #[cfg(feature = "vertex")]
    #[serde(default)]
    pub vertex: Option<VertexSettings>,
    #[serde(default)]
    pub openai_compat: Option<OpenAiCompatSettings>,
    #[serde(default)]
    pub ollama: Option<OllamaSettings>,
    #[serde(default)]
    pub kiro_cli: Option<KiroCliSettings>,
    #[serde(default)]
    pub claude_cli: Option<ClaudeCliSettings>,
    #[serde(default)]
    pub devin_cli: Option<DevinCliSettings>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct SourceBudgetSettings {
    #[serde(default, alias = "stage_input_budget")]
    pub stage_input_tokens: Option<usize>,
    #[serde(default, alias = "stage_output_budget")]
    pub stage_output_tokens: Option<usize>,
    #[serde(default)]
    pub review_input_tokens: Option<usize>,
    #[serde(default)]
    pub review_output_tokens: Option<usize>,
    #[serde(default)]
    pub warn_pct: Option<f32>,
    #[serde(default)]
    pub severe_pct: Option<f32>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct ValidationBudgetSettings {
    #[serde(default)]
    pub request_input_tokens: usize,
    #[serde(default)]
    pub request_output_tokens: usize,
    #[serde(default)]
    pub review_input_tokens: usize,
    #[serde(default)]
    pub review_output_tokens: usize,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct ModelExperimentSettings {
    #[serde(default)]
    pub validation_budget: ValidationBudgetSettings,
}

impl SourceBudgetSettings {
    pub(crate) fn with_overrides(&self, overrides: &Self) -> Self {
        Self {
            stage_input_tokens: overrides.stage_input_tokens.or(self.stage_input_tokens),
            stage_output_tokens: overrides.stage_output_tokens.or(self.stage_output_tokens),
            review_input_tokens: overrides.review_input_tokens.or(self.review_input_tokens),
            review_output_tokens: overrides.review_output_tokens.or(self.review_output_tokens),
            warn_pct: overrides.warn_pct.or(self.warn_pct),
            severe_pct: overrides.severe_pct.or(self.severe_pct),
        }
    }
}

fn deserialize_probability<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    if (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "model experiment probability must be between 0.0 and 1.0",
        ))
    }
}

fn deserialize_additional_models<'de, D>(
    deserializer: D,
) -> Result<Vec<AdditionalModelSettings>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let models = Vec::<AdditionalModelSettings>::deserialize(deserializer)?;
    let mut names = std::collections::HashSet::new();
    for model in &models {
        if model.name.is_empty()
            || model.name == "main"
            || !model
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(serde::de::Error::custom(
                "model experiment name must contain only ASCII letters, digits, '_' or '-', and must not be 'main'",
            ));
        }
        if !names.insert(&model.name) {
            return Err(serde::de::Error::custom(format!(
                "duplicate model experiment name: {}",
                model.name
            )));
        }
    }
    Ok(models)
}

impl AdditionalModelSettings {
    pub fn effective_ai(&self, main: &AiSettings) -> AiSettings {
        let mut ai = main.clone();
        ai.additional_models.clear();
        if let Some(value) = &self.provider {
            ai.provider.clone_from(value);
        }
        if let Some(value) = &self.model {
            ai.model.clone_from(value);
        }
        if let Some(value) = self.max_input_tokens {
            ai.max_input_tokens = value;
        }
        if let Some(value) = self.max_interactions {
            ai.max_interactions = value;
        }
        ai.budget = ai.budget.with_overrides(&self.budget);
        if let Some(value) = self.temperature {
            ai.temperature = value;
        }
        if let Some(value) = self.api_timeout_secs {
            ai.api_timeout_secs = value;
        }
        if self.claude.is_some() {
            ai.claude.clone_from(&self.claude);
        }
        if self.gemini.is_some() {
            ai.gemini.clone_from(&self.gemini);
        }
        #[cfg(feature = "bedrock")]
        if self.bedrock.is_some() {
            ai.bedrock.clone_from(&self.bedrock);
        }
        #[cfg(feature = "vertex")]
        if self.vertex.is_some() {
            ai.vertex.clone_from(&self.vertex);
        }
        if self.openai_compat.is_some() {
            ai.openai_compat.clone_from(&self.openai_compat);
        }
        if self.ollama.is_some() {
            ai.ollama.clone_from(&self.ollama);
        }
        if self.kiro_cli.is_some() {
            ai.kiro_cli.clone_from(&self.kiro_cli);
        }
        if self.claude_cli.is_some() {
            ai.claude_cli.clone_from(&self.claude_cli);
        }
        if self.devin_cli.is_some() {
            ai.devin_cli.clone_from(&self.devin_cli);
        }
        ai
    }
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct AiSettings {
    pub provider: String,
    pub model: String,
    /// Stable presentation name for the main review source.
    #[serde(
        default = "default_main_source_name",
        deserialize_with = "deserialize_main_source_name"
    )]
    pub name: String,
    #[serde(default = "default_max_input_tokens")]
    pub max_input_tokens: usize,
    #[serde(default = "default_max_interactions")]
    pub max_interactions: usize,
    /// Maximum number of discovery stage numbers (1-7) run concurrently.
    /// Keeping this at one lets each completed stage steer the token budget,
    /// warning level, and retry-effort provider used by the next stage.
    #[serde(default = "default_analysis_stage_parallelism")]
    pub analysis_stage_parallelism: usize,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_api_timeout_secs")]
    pub api_timeout_secs: u64,
    #[serde(skip, default)]
    pub no_ai: bool,
    /// Log each AI request/response turn at info level (content previews + token counts).
    /// Useful for debugging but verbose; disabled by default.
    #[serde(default)]
    pub log_turns: bool,
    /// Directory where complete per-turn request and response JSON is written.
    #[serde(default)]
    pub dump_conversation: Option<String>,
    /// Per-stage input token budget. Zero disables input budgeting.
    #[serde(default)]
    pub stage_input_budget: usize,
    /// Per-stage output token budget. Zero disables output budgeting.
    #[serde(default)]
    pub stage_output_budget: usize,
    #[serde(default)]
    pub budget_warn_pct: f32,
    #[serde(default)]
    pub budget_severe_pct: f32,
    /// Multiplier from stage budgets to the shared review-wide budget.
    #[serde(default)]
    pub review_budget_multiplier: f32,
    /// Explicit source budget. Values override the legacy flat budget fields.
    #[serde(default)]
    pub budget: SourceBudgetSettings,
    /// Budget for stages 8-11. Unspecified values inherit from `budget`.
    #[serde(default)]
    pub merge_budget: SourceBudgetSettings,
    #[serde(default)]
    pub model_experiments: ModelExperimentSettings,
    #[serde(default)]
    pub response_cache: bool,
    #[serde(default = "default_response_cache_ttl_days")]
    pub response_cache_ttl_days: u64,
    /// Models sampled independently for complete analytical patch reviews.
    #[serde(default, deserialize_with = "deserialize_additional_models")]
    pub additional_models: Vec<AdditionalModelSettings>,
    // Provider-specific settings
    pub claude: Option<ClaudeSettings>,
    pub gemini: Option<GeminiSettings>,
    #[cfg(feature = "bedrock")]
    pub bedrock: Option<BedrockSettings>,
    #[cfg(feature = "vertex")]
    pub vertex: Option<VertexSettings>,
    pub openai_compat: Option<OpenAiCompatSettings>,
    pub ollama: Option<OllamaSettings>,
    pub kiro_cli: Option<KiroCliSettings>,
    pub claude_cli: Option<ClaudeCliSettings>,
    pub devin_cli: Option<DevinCliSettings>,
}

impl AiSettings {
    pub(crate) fn discovery_budget_config(&self) -> crate::ai::review_budget::BudgetConfig {
        self.budget_config(&self.budget)
    }

    pub(crate) fn merge_budget_config(&self) -> crate::ai::review_budget::BudgetConfig {
        self.budget_config(&self.budget.with_overrides(&self.merge_budget))
    }

    fn budget_config(
        &self,
        budget: &SourceBudgetSettings,
    ) -> crate::ai::review_budget::BudgetConfig {
        crate::ai::review_budget::BudgetConfig {
            stage_input: budget.stage_input_tokens.unwrap_or(self.stage_input_budget),
            stage_output: budget
                .stage_output_tokens
                .unwrap_or(self.stage_output_budget),
            review_input: budget.review_input_tokens.unwrap_or(0),
            review_output: budget.review_output_tokens.unwrap_or(0),
            warn_pct: budget.warn_pct.unwrap_or(self.budget_warn_pct),
            severe_pct: budget.severe_pct.unwrap_or(self.budget_severe_pct),
            review_multiplier: self.review_budget_multiplier,
            enforce_hard_limits: budget.stage_input_tokens.is_some()
                || budget.stage_output_tokens.is_some()
                || budget.review_input_tokens.is_some()
                || budget.review_output_tokens.is_some(),
        }
    }
}

fn default_response_cache_ttl_days() -> u64 {
    7
}

fn default_api_timeout_secs() -> u64 {
    300
}

fn default_temperature() -> f32 {
    1.0
}

fn default_max_interactions() -> usize {
    100
}

fn default_analysis_stage_parallelism() -> usize {
    1
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub read_only: bool,
    /// Static secret tokens that let a client see embargoed patchset
    /// details immediately. Passed as `Authorization: Bearer <token>` or
    /// `?token=<token>`. Multiple tokens may be active at once for easy
    /// rotation. Only use over HTTPS — tokens in query strings leak into
    /// logs and referrers.
    #[serde(default)]
    pub embargo_bypass_tokens: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct CustomRemoteSettings {
    pub name: String,
    pub url: String,
    pub check_all_branches: bool,
    pub only_branches: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct GitSettings {
    pub repository_path: String,
    pub custom_remotes: Option<Vec<CustomRemoteSettings>>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ReviewSettings {
    pub concurrency: usize,
    pub worktree_dir: String,
    #[serde(default = "default_review_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_max_lines_changed")]
    pub max_lines_changed: usize,
    #[serde(default = "default_max_files_touched")]
    pub max_files_touched: usize,
    #[serde(default)]
    pub ignore_files: Vec<String>,
    #[serde(default = "default_email_policy_path")]
    pub email_policy_path: String,
    /// Maximum cumulative non-cached tokens (uncached input + output) across all turns in a
    /// single review. Cached input tokens are excluded because they cost ~10x less and don't
    /// reflect runaway model behaviour. At Sonnet 4.6 pricing ($3/M uncached input, $15/M
    /// output) the 5M default costs roughly $15–75 depending on input/output mix; a typical
    /// 7-stage review uses ~300–500k tokens total. Set to 0 to disable.
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: usize,
    /// Maximum cumulative output tokens across all turns in a single review.
    /// Conservative default; set to 0 to disable.
    #[serde(default = "default_max_total_output_tokens")]
    pub max_total_output_tokens: usize,
    /// Override the review tool binary path. Not read from config; set programmatically
    /// (e.g. in tests or via environment).
    #[serde(skip)]
    pub review_tool_override: Option<std::path::PathBuf>,
    #[serde(default)]
    pub stages: Option<Vec<u8>>,
}

fn default_max_total_tokens() -> usize {
    5_000_000
}

fn default_max_total_output_tokens() -> usize {
    500_000
}

fn default_max_lines_changed() -> usize {
    10_000
}

fn default_max_files_touched() -> usize {
    200
}

fn default_review_timeout() -> u64 {
    3600
}

fn default_max_retries() -> u32 {
    3
}

fn default_email_policy_path() -> String {
    "email_policy.toml".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct SemcodeSettings {
    /// Path to the semcode-mcp binary. Defaults to "semcode-mcp" (found via PATH).
    #[serde(default)]
    pub mcp_binary: Option<String>,
    /// Path to the semcode-index binary. Defaults to "semcode-index" (found via PATH).
    #[serde(default)]
    pub index_binary: Option<String>,
    /// Enable semcode tools for AI review. Defaults to false.
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct CrossReviewSettings {
    #[serde(default, deserialize_with = "deserialize_cross_review_instances")]
    pub instances: Vec<CrossReviewInstanceSettings>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct CrossReviewInstanceSettings {
    pub name: String,
    pub url: String,
}

fn deserialize_cross_review_instances<'de, D>(
    deserializer: D,
) -> Result<Vec<CrossReviewInstanceSettings>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut instances = Vec::<CrossReviewInstanceSettings>::deserialize(deserializer)?;
    let mut names = std::collections::HashSet::new();
    for instance in &mut instances {
        if instance.name.is_empty()
            || instance.name == "main"
            || !instance
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || !names.insert(instance.name.clone())
        {
            return Err(serde::de::Error::custom(
                "cross-review instance names must be unique, route-safe, and not 'main'",
            ));
        }
        let parsed = reqwest::Url::parse(&instance.url)
            .map_err(|_| serde::de::Error::custom("invalid cross-review instance URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(serde::de::Error::custom(
                "cross-review instance URL must use HTTP or HTTPS",
            ));
        }
        instance.url = instance.url.trim_end_matches('/').to_string();
    }
    Ok(instances)
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct Settings {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub project: ProjectSettings,
    #[serde(default = "default_subsystems")]
    pub subsystems: SubsystemsSettings,
    #[serde(default = "default_forge")]
    pub forge: ForgeSettings,
    pub database: DatabaseSettings,
    pub nntp: NntpSettings,
    pub smtp: Option<SmtpSettings>,
    pub mailing_lists: MailingListsSettings,
    pub ai: AiSettings,
    pub server: ServerSettings,
    pub git: GitSettings,
    pub review: ReviewSettings,
    #[serde(default)]
    pub semcode: Option<SemcodeSettings>,
    #[serde(default)]
    pub cross_review: CrossReviewSettings,
}

fn default_subsystems() -> SubsystemsSettings {
    SubsystemsSettings { mapping: vec![] }
}

fn default_forge() -> ForgeSettings {
    ForgeSettings {
        enabled: false,
        disable_nntp: true,
        provider: None,
        webhook_secret: None,
        api_token: None,
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct LocalReviewReviewSettings {
    pub concurrency: Option<usize>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LocalReviewSettings {
    pub ai: AiSettings,
    pub review: Option<LocalReviewReviewSettings>,
    #[serde(default)]
    pub semcode: Option<SemcodeSettings>,
}
impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let s = Config::builder()
            .add_source(File::with_name("Settings"))
            .add_source(File::with_name("Settings.local").required(false))
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        s.try_deserialize()
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let s = Config::builder()
            // Start with default settings
            .add_source(File::from(path.as_ref()))
            // Add settings from environment variables (with a prefix of SASHIKO)
            // e.g. SASHIKO__SERVER__PORT=8081 would set the server port
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        s.try_deserialize()
    }

    pub fn local_review_path() -> PathBuf {
        Self::local_review_path_in(Path::new("."))
    }

    pub fn local_review_path_in(base: &Path) -> PathBuf {
        let local = base.join("Settings.toml");
        if local.exists() {
            return local;
        }

        Self::user_config_path()
    }

    pub fn user_config_path() -> PathBuf {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(config_home).join("sashiko.toml");
        }

        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config/sashiko.toml");
        }

        PathBuf::from(".config/sashiko.toml")
    }

    pub fn local_review() -> Result<Self, ConfigError> {
        Self::from_file(Self::local_review_path())
    }

    pub fn local_review_settings() -> Result<LocalReviewSettings, ConfigError> {
        Self::local_review_from_file(Self::local_review_path())
    }

    pub fn local_review_from_file(
        path: impl AsRef<Path>,
    ) -> Result<LocalReviewSettings, ConfigError> {
        let s = Config::builder()
            .add_source(File::from(path.as_ref()))
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        s.try_deserialize()
    }

    pub fn local_review_ai() -> Result<AiSettings, ConfigError> {
        Self::ai_from_file(Self::local_review_path())
    }

    pub fn ai_from_file(path: impl AsRef<Path>) -> Result<AiSettings, ConfigError> {
        let s = Config::builder()
            .add_source(File::from(path.as_ref()))
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        let settings: LocalReviewSettings = s.try_deserialize()?;
        Ok(settings.ai)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct AdditionalModelsWrapper {
        #[serde(deserialize_with = "deserialize_additional_models")]
        models: Vec<AdditionalModelSettings>,
    }

    #[derive(Deserialize)]
    struct CrossReviewWrapper {
        #[serde(deserialize_with = "deserialize_cross_review_instances")]
        instances: Vec<CrossReviewInstanceSettings>,
    }

    #[test]
    fn test_local_review_path_prefers_current_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Settings.toml"), "").unwrap();
        assert_eq!(
            Settings::local_review_path_in(temp.path()),
            temp.path().join("Settings.toml")
        );
    }

    #[test]
    fn test_user_config_path_uses_xdg_config_home() {
        let temp = tempfile::tempdir().unwrap();
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path());
        }

        assert_eq!(
            Settings::user_config_path(),
            temp.path().join("sashiko.toml")
        );

        unsafe {
            if let Some(value) = old_xdg {
                std::env::set_var("XDG_CONFIG_HOME", value);
            } else {
                std::env::remove_var("XDG_CONFIG_HOME");
            }
        }
    }

    #[test]
    fn additional_model_probability_must_be_bounded() {
        let invalid = serde_json::json!({
            "name": "variant",
            "probability": 1.1
        });
        let error = serde_json::from_value::<AdditionalModelSettings>(invalid).unwrap_err();
        assert!(error.to_string().contains("between 0.0 and 1.0"));
    }

    #[test]
    fn additional_model_names_are_route_safe() {
        for name in ["main", "bad]route", "has space"] {
            let value = serde_json::json!({
                "models": [{"name": name, "probability": 0.5}]
            });
            assert!(
                serde_json::from_value::<AdditionalModelsWrapper>(value).is_err(),
                "accepted invalid experiment name {name}"
            );
        }
        let valid = serde_json::json!({
            "models": [{"name": "model-b_2", "probability": 0.5}]
        });
        assert_eq!(
            serde_json::from_value::<AdditionalModelsWrapper>(valid)
                .unwrap()
                .models
                .len(),
            1
        );
    }

    #[test]
    fn main_source_name_defaults_and_is_route_safe() {
        let default: AiSettings = serde_json::from_value(serde_json::json!({
            "provider": "test",
            "model": "model"
        }))
        .unwrap();
        assert_eq!(default.name, "main");

        let named: AiSettings = serde_json::from_value(serde_json::json!({
            "provider": "test",
            "model": "model",
            "name": "opus-5"
        }))
        .unwrap();
        assert_eq!(named.name, "opus-5");

        for name in ["", "has space", "bad]name"] {
            let value = serde_json::json!({
                "provider": "test",
                "model": "model",
                "name": name
            });
            assert!(serde_json::from_value::<AiSettings>(value).is_err());
        }
    }

    #[test]
    fn analysis_stage_parallelism_defaults_to_one_and_can_be_overridden() {
        let default: AiSettings = serde_json::from_value(serde_json::json!({
            "provider": "test",
            "model": "model"
        }))
        .unwrap();
        assert_eq!(default.analysis_stage_parallelism, 1);

        let configured: AiSettings = serde_json::from_value(serde_json::json!({
            "provider": "test",
            "model": "model",
            "analysis_stage_parallelism": 3
        }))
        .unwrap();
        assert_eq!(configured.analysis_stage_parallelism, 3);
    }

    #[test]
    fn experiment_budget_tables_deserialize() {
        let model: AdditionalModelSettings = serde_json::from_value(serde_json::json!({
            "name": "variant",
            "probability": 0.5,
            "budget": {
                "stage_input_tokens": 100,
                "review_output_tokens": 200
            }
        }))
        .unwrap();
        assert_eq!(model.budget.stage_input_tokens, Some(100));
        assert_eq!(model.budget.review_output_tokens, Some(200));

        let experiments: ModelExperimentSettings = serde_json::from_value(serde_json::json!({
            "validation_budget": {
                "request_input_tokens": 300,
                "review_output_tokens": 400
            }
        }))
        .unwrap();
        assert_eq!(experiments.validation_budget.request_input_tokens, 300);
        assert_eq!(experiments.validation_budget.review_output_tokens, 400);
    }

    #[test]
    fn merge_budget_inherits_discovery_values() {
        let ai: AiSettings = serde_json::from_value(serde_json::json!({
            "provider": "test",
            "model": "model",
            "budget": {
                "stage_input_tokens": 100,
                "review_output_tokens": 400,
                "warn_pct": 0.7
            },
            "merge_budget": {
                "stage_output_tokens": 200
            }
        }))
        .unwrap();

        let discovery = ai.discovery_budget_config();
        let merge = ai.merge_budget_config();
        assert_eq!(discovery.stage_input, 100);
        assert_eq!(merge.stage_input, 100);
        assert_eq!(merge.stage_output, 200);
        assert_eq!(merge.review_output, 400);
        assert_eq!(merge.warn_pct, 0.7);
        assert!(merge.enforce_hard_limits);
    }

    #[test]
    fn cross_review_instances_validate_names_and_urls() {
        let valid = serde_json::json!({
            "instances": [{"name": "peer-a_2", "url": "https://peer.example/"}]
        });
        let parsed: CrossReviewWrapper = serde_json::from_value(valid).unwrap();
        assert_eq!(parsed.instances[0].url, "https://peer.example");

        for value in [
            serde_json::json!({"instances": [{"name": "main", "url": "https://peer.example"}]}),
            serde_json::json!({"instances": [{"name": "bad name", "url": "https://peer.example"}]}),
            serde_json::json!({"instances": [{"name": "peer", "url": "file:///tmp/peer"}]}),
            serde_json::json!({"instances": [
                {"name": "peer", "url": "https://one.example"},
                {"name": "peer", "url": "https://two.example"}
            ]}),
        ] {
            assert!(serde_json::from_value::<CrossReviewWrapper>(value).is_err());
        }
    }
}
