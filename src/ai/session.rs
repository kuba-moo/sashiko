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

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::review_budget::{BudgetLevel, ReviewBudget};
use super::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    AiUsage, ToolCall, classify_ai_error,
};

/// The unified result of executing an [`LlmSession`].
pub struct SessionResult<T> {
    /// The validated output of the session.
    pub output: T,
    /// The full conversation history.
    pub history: Vec<AiMessage>,
    /// Accumulated token usage statistics.
    pub usage: AiUsage,
}

/// Session failure that retains usage reported before a hard budget limit was detected.
#[derive(Debug)]
pub struct SessionBudgetError {
    usage: AiUsage,
}

impl SessionBudgetError {
    pub fn usage(&self) -> &AiUsage {
        &self.usage
    }

    /// Builds an error carrying no usage, for tests that only need the type.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            usage: AiUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cached_tokens: None,
                cache_write_tokens: None,
            },
        }
    }
}

impl std::fmt::Display for SessionBudgetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("source-owned stage or review token limit was exceeded")
    }
}

impl std::error::Error for SessionBudgetError {}

pub struct ConversationDumper {
    directory: PathBuf,
}

impl ConversationDumper {
    pub async fn new(base: &Path) -> Result<Self> {
        let now = chrono::Utc::now();
        let suffix: String = (0..4)
            .map(|_| {
                let index = fastrand::u8(0..36);
                if index < 10 {
                    (b'0' + index) as char
                } else {
                    (b'a' + index - 10) as char
                }
            })
            .collect();
        let directory = base.join(format!("{}-{}", now.format("%Y%m%d-%H%M"), suffix));
        tokio::fs::create_dir_all(&directory).await?;
        Ok(Self { directory })
    }

    pub async fn write<T: Serialize>(
        &self,
        label: &str,
        turn: usize,
        kind: &str,
        value: &T,
    ) -> Result<()> {
        let path = self
            .directory
            .join(format!("{}_{:03}_{}.json", label, turn, kind));
        tokio::fs::write(path, serde_json::to_vec_pretty(value)?).await?;
        Ok(())
    }
}

/// Result of validating a session's final response.
#[derive(Debug)]
pub enum ValidationError {
    /// The response was invalid but can be retried.
    /// Contains a feedback message to append to the LLM prompt.
    FormatViolation(String),
    /// A fatal error that cannot be resolved by retrying.
    Fatal(String),
}

/// Action to take upon encountering a provider error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAction {
    /// Retry the request after appending the feedback message to the prompt history.
    RetryWithFeedback(String),
    /// Abort the session immediately.
    Fail,
}

/// Represents a stateful, task-oriented interaction session with an LLM.
#[async_trait]
pub trait LlmSession: Send {
    /// The final output type returned by the session after validation.
    type Output: Send;

    /// The system prompt guiding the LLM.
    fn system_prompt(&self) -> String;

    /// The initial user prompt.
    fn initial_user_prompt(&self) -> String;

    /// The user prompt to store in history/logs (for space saving).
    /// Defaults to `initial_user_prompt()`.
    fn log_user_prompt(&self) -> String {
        self.initial_user_prompt()
    }

    /// Customizes the validation feedback message.
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "Previous attempt was rejected: {}. Please correct your output format.",
            violation
        )
    }

    /// Optional list of tools available in this session.
    fn tools(&self) -> Option<Vec<AiTool>> {
        None
    }

    /// Optional temperature override.
    fn temperature(&self) -> Option<f32> {
        None
    }

    /// Optional context tag for logging.
    fn context_tag(&self) -> Option<String> {
        None
    }

    /// Optional expected response format.
    fn response_format(&self) -> Option<AiResponseFormat> {
        None
    }

    /// Executes a tool call requested by the LLM.
    async fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value> {
        anyhow::bail!("Tool execution not implemented for this session: {}", name)
    }

    /// Executes multiple tool calls requested by the LLM.
    /// Default implementation runs them sequentially.
    async fn call_tools(&mut self, calls: Vec<ToolCall>) -> Result<Vec<(String, Value)>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let res = self.call_tool(&call.function_name, call.arguments).await?;
            results.push((call.id, res));
        }
        Ok(results)
    }

    /// Validates the final response content.
    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError>;

    /// Hook to handle provider errors (e.g. safety blocks, rate limits).
    fn handle_provider_error(&mut self, error: &anyhow::Error, _attempt: usize) -> ErrorAction {
        let err_str = error.to_string();
        if err_str.contains("RECITATION") || err_str.contains("blocked") {
            ErrorAction::RetryWithFeedback(
                "IMPORTANT: Your previous response was blocked by a recitation filter. \
                 Please do NOT copy large blocks of code verbatim in your response. \
                 Describe changes in prose, or use highly simplified pseudo-code if you must show code structure."
                    .to_string(),
            )
        } else {
            ErrorAction::Fail
        }
    }
}

/// Orchestrates the execution of an [`LlmSession`].
pub struct SessionRunner<'a> {
    provider: &'a dyn AiProvider,
    max_turns: usize,
    max_validation_attempts: usize,
    max_transient_retries: usize,
    max_provider_error_retries: usize,
    on_turn: Option<Box<dyn Fn(usize, usize) + Send + Sync + 'a>>,
    on_prefix_cached: Option<Box<dyn Fn() + Send + Sync + 'a>>,
    conversation_dump: Option<(Arc<ConversationDumper>, String)>,
    budget: Option<ReviewBudget>,
}

impl<'a> SessionRunner<'a> {
    /// Creates a new `SessionRunner` with default limits.
    pub fn new(provider: &'a dyn AiProvider) -> Self {
        Self {
            provider,
            max_turns: 15,
            max_validation_attempts: 3,
            max_transient_retries: 5,
            max_provider_error_retries: 3,
            on_turn: None,
            on_prefix_cached: None,
            conversation_dump: None,
            budget: None,
        }
    }

    /// Configures the maximum validation retries.
    pub fn with_max_validation_attempts(mut self, attempts: usize) -> Self {
        self.max_validation_attempts = attempts;
        self
    }

    /// Configures the maximum conversational turns.
    pub fn with_max_turns(mut self, turns: usize) -> Self {
        self.max_turns = turns;
        self
    }

    /// Configures the maximum transient and rate-limit retries.
    pub fn with_max_transient_retries(mut self, retries: usize) -> Self {
        self.max_transient_retries = retries;
        self
    }

    /// Configures the maximum provider error retries.
    pub fn with_max_provider_error_retries(mut self, retries: usize) -> Self {
        self.max_provider_error_retries = retries;
        self
    }

    /// Configures a turn callback.
    pub fn with_turn_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(usize, usize) + Send + Sync + 'a,
    {
        self.on_turn = Some(Box::new(cb));
        self
    }

    pub fn with_conversation_dump(
        mut self,
        dump: Option<Arc<ConversationDumper>>,
        label: impl Into<String>,
    ) -> Self {
        self.conversation_dump = dump.map(|dump| (dump, label.into()));
        self
    }

    pub fn with_budget(mut self, budget: Option<ReviewBudget>) -> Self {
        self.budget = budget;
        self
    }

    /// Configures a callback fired once the first response arrives, i.e. once the
    /// provider has written this session's prompt prefix to its cache.  Callers use
    /// it to release work that shares the same prefix and would otherwise pay for a
    /// redundant cache write.
    pub fn with_prefix_cached_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn() + Send + Sync + 'a,
    {
        self.on_prefix_cached = Some(Box::new(cb));
        self
    }

    /// Runs the session to completion. Returns the validated output and conversation history (for logging).
    pub async fn run<S>(&self, session: &mut S) -> Result<SessionResult<S::Output>>
    where
        S: LlmSession,
    {
        let inherited_budget_level = self.budget.as_ref().and_then(ReviewBudget::review_level);
        let inherited_budget_warning = match inherited_budget_level {
            Some(BudgetLevel::Severe) => Some(
                "[SYSTEM - TOKEN BUDGET]\nThe shared review token budget is already severely consumed. Conclude with the information available and do not perform broad investigation or optional tool calls.",
            ),
            Some(BudgetLevel::Warn) => Some(
                "[SYSTEM - TOKEN BUDGET]\nA significant portion of the shared review token budget was consumed by earlier stages. Be selective and investigate only remaining critical concerns.",
            ),
            None => None,
        };
        let initial_prompt = |mut prompt: String| {
            if let Some(warning) = inherited_budget_warning {
                prompt.push_str("\n\n");
                prompt.push_str(warning);
            }
            prompt
        };
        let mut history = vec![AiMessage {
            role: AiRole::User,
            content: Some(initial_prompt(session.initial_user_prompt())),
            thought: None,
            thought_signature: None,
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        let mut log_history = vec![AiMessage {
            role: AiRole::User,
            content: Some(initial_prompt(session.log_user_prompt())),
            thought: None,
            thought_signature: None,
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        let mut turns = 0;
        let mut validation_attempts = 0;
        let mut transient_retries = 0;
        let mut provider_error_retries = 0;
        let mut total_prompt_tokens: usize = 0;
        let mut total_completion_tokens: usize = 0;
        let mut total_cached_tokens: usize = 0;
        let mut severe_seen = inherited_budget_level == Some(BudgetLevel::Severe);
        let mut force_conclude = false;
        let mut stage_budget_flags = 0;

        loop {
            turns += 1;
            if turns > self.max_turns {
                anyhow::bail!("Session exceeded max turns limit ({})", self.max_turns);
            }
            if let Some(ref cb) = self.on_turn {
                cb(turns, self.max_turns);
            }

            let request = AiRequest {
                system: Some(session.system_prompt()),
                messages: history.clone(),
                tools: session.tools(),
                temperature: session.temperature(),
                response_format: session.response_format(),
                context_tag: session.context_tag(),
            };
            let estimated_input = self.provider.estimate_tokens(&request);
            if self.budget.as_ref().is_some_and(|budget| {
                !budget.allows_request_input(
                    total_prompt_tokens.saturating_add(estimated_input),
                    estimated_input,
                )
            }) {
                anyhow::bail!("request input exceeds the source-owned stage or review token limit");
            }

            if let Some((dump, label)) = &self.conversation_dump
                && let Err(error) = dump.write(label, turns, "req", &request).await
            {
                tracing::warn!("Failed to dump {} turn {} request: {}", label, turns, error);
            }

            let resp = match self.provider.generate_content(request).await {
                Ok(r) => r,
                Err(e) => match classify_ai_error(&e) {
                    AiErrorClass::RateLimit { retry_after }
                    | AiErrorClass::Transient { retry_after } => {
                        transient_retries += 1;
                        if transient_retries > self.max_transient_retries {
                            anyhow::bail!(
                                "Session failed after {} transient/rate-limit errors. Last error: {}",
                                self.max_transient_retries,
                                e
                            );
                        }
                        tracing::warn!(
                            "API error ({}), pausing for {:?} before retry (attempt {}/{})...",
                            e,
                            retry_after,
                            transient_retries,
                            self.max_transient_retries
                        );
                        tokio::time::sleep(retry_after).await;
                        turns = turns.saturating_sub(1);
                        continue;
                    }
                    AiErrorClass::Fatal => {
                        match session.handle_provider_error(&e, provider_error_retries) {
                            ErrorAction::RetryWithFeedback(feedback) => {
                                provider_error_retries += 1;
                                if provider_error_retries > self.max_provider_error_retries {
                                    anyhow::bail!(
                                        "Session failed after {} provider error retries. Last error: {}",
                                        self.max_provider_error_retries,
                                        e
                                    );
                                }
                                let msg = AiMessage {
                                    role: AiRole::User,
                                    content: Some(feedback.clone()),
                                    thought: None,
                                    thought_signature: None,
                                    reasoning: None,
                                    tool_calls: None,
                                    tool_call_id: None,
                                };
                                history.push(msg.clone());
                                log_history.push(msg);
                                turns = turns.saturating_sub(1);
                                continue;
                            }
                            ErrorAction::Fail => return Err(e),
                        }
                    }
                },
            };

            // The first response means the provider has cached this prompt prefix.
            // Release anything waiting to reuse it, even if this turn later fails.
            if turns == 1
                && let Some(ref cb) = self.on_prefix_cached
            {
                cb();
            }

            if let Some((dump, label)) = &self.conversation_dump
                && let Err(error) = dump.write(label, turns, "resp", &resp).await
            {
                tracing::warn!(
                    "Failed to dump {} turn {} response: {}",
                    label,
                    turns,
                    error
                );
            }

            if resp.truncated {
                // Typed, so this classifies as `Fatal` and short-circuits the
                // retry loops. A plain `anyhow` error here would be treated as a
                // transient blip and re-run the whole review, which hits the same
                // output limit again at full cost.
                return Err(crate::worker::prompts::ReviewError::OutputTruncated.into());
            }

            let mut budget_level = None;
            if let Some(usage) = &resp.usage {
                total_prompt_tokens += usage.prompt_tokens;
                total_completion_tokens += usage.completion_tokens;
                total_cached_tokens += usage.cached_tokens.unwrap_or(0);
                if let Some(budget) = &self.budget {
                    budget_level = budget.record_and_check(
                        &mut stage_budget_flags,
                        total_prompt_tokens,
                        total_completion_tokens,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        usage.cached_tokens.unwrap_or(0),
                    );
                    if budget.hard_limit_exceeded(total_prompt_tokens, total_completion_tokens) {
                        return Err(SessionBudgetError {
                            usage: AiUsage {
                                prompt_tokens: total_prompt_tokens,
                                completion_tokens: total_completion_tokens,
                                total_tokens: total_prompt_tokens + total_completion_tokens,
                                cached_tokens: Some(total_cached_tokens),
                                cache_write_tokens: None,
                            },
                        }
                        .into());
                    }
                }
            }

            let assistant_msg = AiMessage {
                role: AiRole::Assistant,
                content: resp.content.clone(),
                thought: resp.thought.clone(),
                thought_signature: resp.thought_signature.clone(),
                reasoning: resp.reasoning.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            };
            history.push(assistant_msg.clone());
            log_history.push(assistant_msg);

            // Handle Tool Calls
            if let Some(tool_calls) = &resp.tool_calls {
                if force_conclude {
                    tracing::warn!(
                        "Model made tool calls after the final token-budget warning; \
                         accepting any valid final content without executing the calls"
                    );
                    let output = match session.validate(&resp) {
                        Ok(output) => output,
                        Err(ValidationError::FormatViolation(violation)) => anyhow::bail!(
                            "Token budget exhausted and final response was invalid: {}",
                            violation
                        ),
                        Err(ValidationError::Fatal(error)) => anyhow::bail!(
                            "Token budget exhausted and final response failed validation: {}",
                            error
                        ),
                    };
                    let usage = AiUsage {
                        prompt_tokens: total_prompt_tokens,
                        completion_tokens: total_completion_tokens,
                        total_tokens: total_prompt_tokens + total_completion_tokens,
                        cached_tokens: Some(total_cached_tokens),
                        cache_write_tokens: None,
                    };
                    return Ok(SessionResult {
                        output,
                        history: log_history,
                        usage,
                    });
                }
                let results = session.call_tools(tool_calls.clone()).await?;
                for (call_id, result) in results {
                    let tool_msg = AiMessage {
                        role: AiRole::Tool,
                        content: Some(result.to_string()),
                        thought: None,
                        thought_signature: None,
                        reasoning: None,
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                    };
                    history.push(tool_msg.clone());
                    log_history.push(tool_msg);
                }

                let warning = if severe_seen {
                    force_conclude = true;
                    Some(
                        "[SYSTEM - TOKEN BUDGET]\nYour token budget is exhausted. Provide your final answer now and do not call any more tools.",
                    )
                } else {
                    match budget_level {
                        Some(BudgetLevel::Severe) => {
                            severe_seen = true;
                            Some(
                                "[SYSTEM - TOKEN BUDGET]\nYou are approaching your token budget limit. Conclude now with the information already gathered; do not make further tool calls.",
                            )
                        }
                        Some(BudgetLevel::Warn) => Some(
                            "[SYSTEM - TOKEN BUDGET]\nA significant portion of the token budget has been used. Be selective and investigate only remaining critical concerns.",
                        ),
                        None => None,
                    }
                };
                if let Some(warning) = warning {
                    let message = AiMessage {
                        role: AiRole::User,
                        content: Some(warning.to_string()),
                        thought: None,
                        thought_signature: None,
                        reasoning: None,
                        tool_calls: None,
                        tool_call_id: None,
                    };
                    history.push(message.clone());
                    log_history.push(message);
                }
                continue; // Loop again to feed tool results back to LLM
            }

            // No tool calls: validate response
            match session.validate(&resp) {
                Result::Ok(output) => {
                    let usage = AiUsage {
                        prompt_tokens: total_prompt_tokens,
                        completion_tokens: total_completion_tokens,
                        total_tokens: total_prompt_tokens + total_completion_tokens,
                        cached_tokens: Some(total_cached_tokens),
                        cache_write_tokens: None,
                    };
                    return Ok(SessionResult {
                        output,
                        history: log_history,
                        usage,
                    });
                }
                Result::Err(ValidationError::FormatViolation(violation)) => {
                    validation_attempts += 1;
                    if validation_attempts >= self.max_validation_attempts {
                        anyhow::bail!(
                            "Failed to generate valid response after {} validation attempts. Last violation: {}",
                            self.max_validation_attempts,
                            violation
                        );
                    }
                    let feedback = session.format_validation_feedback(&violation);
                    let msg = AiMessage {
                        role: AiRole::User,
                        content: Some(feedback),
                        thought: None,
                        thought_signature: None,
                        reasoning: None,
                        tool_calls: None,
                        tool_call_id: None,
                    };
                    history.push(msg.clone());
                    log_history.push(msg);
                    turns = turns.saturating_sub(1);
                }
                Result::Err(ValidationError::Fatal(err)) => {
                    anyhow::bail!("Fatal validation error: {}", err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ProviderCapabilities;
    use crate::ai::review_budget::BudgetConfig;

    struct UsageProvider;

    #[async_trait]
    impl AiProvider for UsageProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            Ok(AiResponse {
                content: Some("{}".to_string()),
                thought: None,
                thought_signature: None,
                reasoning: None,
                tool_calls: None,
                usage: Some(AiUsage {
                    prompt_tokens: 11,
                    completion_tokens: 7,
                    total_tokens: 18,
                    cached_tokens: Some(3),
                    cache_write_tokens: None,
                }),
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            10
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "usage-provider".to_string(),
                context_window_size: 1000,
            }
        }
    }

    struct TestSession;

    #[async_trait]
    impl LlmSession for TestSession {
        type Output = Value;

        fn system_prompt(&self) -> String {
            String::new()
        }

        fn initial_user_prompt(&self) -> String {
            String::new()
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            Ok(
                serde_json::from_str(response.content.as_deref().unwrap_or("{}"))
                    .unwrap_or_default(),
            )
        }
    }

    struct CapturingProvider {
        prompt: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl AiProvider for CapturingProvider {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            *self.prompt.lock().unwrap() = request
                .messages
                .first()
                .and_then(|message| message.content.clone());
            Ok(AiResponse {
                content: Some("{}".to_string()),
                thought: None,
                thought_signature: None,
                reasoning: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            1
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "capturing-provider".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn new_session_inherits_shared_review_warning() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 1000,
            stage_output: 1000,
            review_input: 100,
            review_output: 100,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
            enforce_hard_limits: false,
        });
        let mut stage_flags = 0;
        budget.record_and_check(&mut stage_flags, 60, 0, 60, 0, 0);
        assert_eq!(budget.review_level(), Some(BudgetLevel::Warn));

        let provider = CapturingProvider {
            prompt: std::sync::Mutex::new(None),
        };
        SessionRunner::new(&provider)
            .with_budget(Some(budget))
            .run(&mut TestSession)
            .await
            .unwrap();

        let prompt = provider.prompt.lock().unwrap().clone().unwrap();
        assert!(prompt.contains("consumed by earlier stages"));
    }

    #[tokio::test]
    async fn new_session_inherits_shared_review_severe_warning() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 1000,
            stage_output: 1000,
            review_input: 100,
            review_output: 100,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
            enforce_hard_limits: false,
        });
        let mut stage_flags = 0;
        budget.record_and_check(&mut stage_flags, 95, 0, 95, 0, 0);
        assert_eq!(budget.review_level(), Some(BudgetLevel::Severe));

        let provider = CapturingProvider {
            prompt: std::sync::Mutex::new(None),
        };
        SessionRunner::new(&provider)
            .with_budget(Some(budget))
            .run(&mut TestSession)
            .await
            .unwrap();

        let prompt = provider.prompt.lock().unwrap().clone().unwrap();
        assert!(prompt.contains("already severely consumed"));
    }

    #[tokio::test]
    async fn hard_budget_error_retains_reported_usage() {
        let budget = ReviewBudget::new(BudgetConfig {
            stage_input: 100,
            stage_output: 5,
            review_input: 100,
            review_output: 100,
            warn_pct: 0.5,
            severe_pct: 0.9,
            review_multiplier: 0.0,
            enforce_hard_limits: true,
        });
        let result = SessionRunner::new(&UsageProvider)
            .with_budget(Some(budget))
            .run(&mut TestSession)
            .await;
        let error = match result {
            Ok(_) => panic!("session unexpectedly stayed within its budget"),
            Err(error) => error,
        };
        let usage = error.downcast_ref::<SessionBudgetError>().unwrap().usage();

        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.cached_tokens, Some(3));
    }

    struct TruncatingProvider;

    #[async_trait]
    impl AiProvider for TruncatingProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            // A partial JSON review report, cut off mid-string exactly as a
            // max_tokens stop leaves it.
            Ok(AiResponse {
                content: Some("{\"concerns\": [{\"type\": \"NULL deref".to_string()),
                thought: None,
                thought_signature: None,
                reasoning: None,
                tool_calls: None,
                usage: None,
                truncated: true,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            10
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "truncating-provider".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[tokio::test]
    async fn truncated_response_reports_truncation_not_a_schema_violation() {
        let result = SessionRunner::new(&TruncatingProvider)
            .run(&mut TestSession)
            .await;

        let error = match result {
            Ok(_) => panic!("a truncated response must not be accepted"),
            Err(error) => error,
        };

        // Must be the typed error so the retry loops classify it `Fatal`, and must
        // not be reported as whatever the missing tail would have contained.
        assert!(
            matches!(
                error.downcast_ref::<crate::worker::prompts::ReviewError>(),
                Some(crate::worker::prompts::ReviewError::OutputTruncated)
            ),
            "expected ReviewError::OutputTruncated, got: {error}"
        );
        assert!(
            crate::worker::prompts::is_fatal_review_error(&error),
            "truncation must not be retried as a transient failure"
        );
    }
}
