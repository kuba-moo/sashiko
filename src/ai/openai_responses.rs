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

//! OpenAI Responses API transport, including Azure AI Foundry's `/openai/v1`
//! endpoint shape.
//!
//! This is intentionally separate from the OpenAI-compatible Chat Completions
//! provider. Responses has a different tool-call wire format and requires
//! opaque reasoning items to be replayed between stateless requests.

use crate::ai::openai::{OpenAiCompatError, estimate_tokens_generic};
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiUsage, ProviderCapabilities,
    ReasoningBlock, ToolCall,
};
use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
#[cfg(feature = "bedrock")]
use std::time::Instant;
use url::Url;

const PROVIDER_TAG: &str = "openai-responses";
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    include: Vec<String>,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_options: Option<Value>,
}

#[derive(Debug, Serialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<ResponsesApiError>,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    input_tokens: u32,
    output_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesApiError {
    #[serde(default)]
    code: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Default)]
pub struct ResponsesOptions {
    pub reasoning_effort: Option<String>,
    pub api_key_env: Option<String>,
    pub prompt_cache: Option<crate::settings::PromptCacheMode>,
    pub prompt_cache_ttl: Option<String>,
    pub bedrock_token: bool,
}

pub struct OpenAiResponsesClient {
    model: String,
    endpoint: String,
    context_window_size: usize,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    prompt_cache: Option<crate::settings::PromptCacheMode>,
    prompt_cache_ttl: Option<String>,
    bedrock_token: bool,
    #[cfg(feature = "bedrock")]
    token_region: Option<String>,
    #[cfg(feature = "bedrock")]
    token_cache: tokio::sync::Mutex<Option<(String, Instant)>>,
    client: Client,
}

/// Bedrock API keys last up to 12 h; refresh well before that.
#[cfg(feature = "bedrock")]
const TOKEN_REFRESH_AFTER: Duration = Duration::from_secs(11 * 3600);

impl OpenAiResponsesClient {
    pub fn new(
        base_url: String,
        model: String,
        context_window_size: usize,
        max_tokens: u32,
        api_timeout_secs: u64,
        options: ResponsesOptions,
    ) -> Result<Self> {
        let ResponsesOptions {
            reasoning_effort,
            api_key_env,
            prompt_cache,
            prompt_cache_ttl,
            bedrock_token,
        } = options;
        #[cfg(not(feature = "bedrock"))]
        if bedrock_token {
            return Err(anyhow!("bedrock_token requires the 'bedrock' feature"));
        }

        let endpoint = Self::normalize_base_url(&base_url)?;

        #[cfg(feature = "bedrock")]
        let token_region = if bedrock_token {
            Some(
                crate::ai::bedrock_token::region_from_base_url(&endpoint)
                    .or_else(|| std::env::var("AWS_REGION").ok().filter(|r| !r.is_empty()))
                    .ok_or_else(|| {
                        anyhow!(
                            "bedrock_token: cannot determine the AWS region from {base_url}; set AWS_REGION"
                        )
                    })?,
            )
        } else {
            None
        };

        let mut headers = reqwest::header::HeaderMap::new();
        if bedrock_token {
            tracing::info!(
                "OpenAI Responses provider for {model} authenticating with a short-term Amazon Bedrock API key"
            );
        } else {
            let api_key =
                Self::resolve_api_key(api_key_env.as_deref(), |name| std::env::var(name).ok())?;

            // Naming the variable rather than the key means a typo in `api_key_env`
            // is silently ignored and falls back to the shared key, so record which
            // variable actually authenticated this provider.
            tracing::info!(
                "OpenAI Responses provider for {model} authenticating with {}",
                api_key_env
                    .as_deref()
                    .unwrap_or("OPENAI_API_KEY/LLM_API_KEY")
            );

            if !api_key.is_empty() {
                let (name, value) = Self::auth_header(&endpoint, &api_key)?;
                headers.insert(name, value);
            }
        }

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(api_timeout_secs))
            .build()?;

        Ok(Self {
            model,
            endpoint,
            context_window_size,
            max_tokens,
            reasoning_effort,
            prompt_cache,
            prompt_cache_ttl,
            bedrock_token,
            #[cfg(feature = "bedrock")]
            token_region,
            #[cfg(feature = "bedrock")]
            token_cache: tokio::sync::Mutex::new(None),
            client,
        })
    }

    pub fn default_base_url() -> String {
        "https://api.openai.com/v1".to_string()
    }

    /// Resolves the API key, reading the environment through `lookup` so the
    /// policy is testable without mutating process-global state.
    ///
    /// `api_key_env` names the variable to read. Naming one asserts that it
    /// exists, so a missing or empty value is an error rather than a silently
    /// unauthenticated request -- otherwise the failure only surfaces as a 401
    /// partway through a review. Without it, fall back to the shared variables,
    /// which stay lenient for compatibility with existing configs.
    fn resolve_api_key(
        api_key_env: Option<&str>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<String> {
        if let Some(name) = api_key_env {
            // Trimmed because `export KEY=$(cat file)` keeps the trailing
            // newline, which HeaderValue rejects with an opaque parse error.
            let key = lookup(name).unwrap_or_default().trim().to_string();
            if key.is_empty() {
                return Err(anyhow!(
                    "api_key_env names {name}, but it is unset or empty in the environment"
                ));
            }
            return Ok(key);
        }

        Ok(lookup("OPENAI_API_KEY")
            .or_else(|| lookup("LLM_API_KEY"))
            .unwrap_or_default()
            .trim()
            .to_string())
    }

    /// Builds the authentication header for `endpoint`.
    ///
    /// Azure API Management gateways take the subscription key in `api-key`;
    /// OpenAI and Azure AI Foundry's OpenAI-compatible endpoints take a bearer
    /// token.
    fn auth_header(endpoint: &str, api_key: &str) -> Result<(HeaderName, HeaderValue)> {
        if Self::uses_azure_api_key(endpoint) {
            Ok((
                HeaderName::from_static("api-key"),
                HeaderValue::from_str(api_key)?,
            ))
        } else {
            Ok((
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))?,
            ))
        }
    }

    /// Accept either an API root ending in `/v1` (including Azure Foundry
    /// project URLs) or the complete `/responses` endpoint.
    fn normalize_base_url(url: &str) -> Result<String> {
        let mut parsed = Url::parse(url).map_err(|_| anyhow!("Invalid OpenAI url {url}"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(anyhow!("Invalid OpenAI url {url}"));
        }

        let path = parsed.path().trim_end_matches('/');
        let normalized_path = if path.ends_with("/responses") {
            path.to_string()
        } else if path.ends_with("/v1") {
            format!("{path}/responses")
        } else {
            return Err(anyhow!(
                "Invalid OpenAI Responses url {url}; expected a /v1 API root or /responses endpoint"
            ));
        };
        parsed.set_path(&normalized_path);
        Ok(parsed.to_string())
    }

    /// Azure API Management gateways authenticate subscription keys with the
    /// `api-key` header. OpenAI and Azure AI Foundry's OpenAI-compatible v1
    /// endpoints continue to use bearer authentication.
    fn uses_azure_api_key(endpoint: &str) -> bool {
        Url::parse(endpoint)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| host.ends_with(".azure-api.net"))
    }

    #[cfg(feature = "bedrock")]
    async fn bedrock_bearer_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((token, created)) = cache.as_ref()
            && created.elapsed() < TOKEN_REFRESH_AFTER
        {
            return Ok(token.clone());
        }
        let region = self
            .token_region
            .as_deref()
            .ok_or_else(|| anyhow!("no region resolved for the Bedrock API key"))?;
        let token = crate::ai::bedrock_token::generate_token(region).await?;
        *cache = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    #[cfg(not(feature = "bedrock"))]
    async fn bedrock_bearer_token(&self) -> Result<String> {
        Err(anyhow!("bedrock_token requires the 'bedrock' feature"))
    }

    async fn post_request(&self, body: &ResponsesRequest) -> Result<ResponsesResponse> {
        let retry_re = Regex::new(r"Please retry in ([0-9.]+)s").expect("valid retry regex");
        let mut request = self.client.post(&self.endpoint).json(body);
        if self.bedrock_token {
            let token = self.bedrock_bearer_token().await.map_err(|error| {
                OpenAiCompatError::AuthenticationError(format!(
                    "failed to create Amazon Bedrock API key: {error:#}"
                ))
            })?;
            request = request.bearer_auth(token);
        }
        let response = request.send().await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let message = redact_secret(&error.to_string());
                tracing::error!("OpenAI Responses request failed (transport): {message}");
                return Err(
                    OpenAiCompatError::TransientError(Duration::from_secs(30), message).into(),
                );
            }
        };

        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let response_text = response.text().await.map_err(|error| {
            let message = redact_secret(&error.to_string());
            OpenAiCompatError::TransientError(Duration::from_secs(30), message)
        })?;

        if status.is_success() {
            let parsed: ResponsesResponse =
                serde_json::from_str(&response_text).map_err(|error| {
                    OpenAiCompatError::ApiError(status, format!("Parse error: {error}"))
                })?;
            if let Some(error) = &parsed.error {
                let message = match &error.code {
                    Some(code) => format!("{code}: {}", error.message),
                    None => error.message.clone(),
                };
                return Err(OpenAiCompatError::ApiError(status, message).into());
            }
            if matches!(parsed.status.as_deref(), Some("failed" | "cancelled")) {
                return Err(OpenAiCompatError::ApiError(
                    status,
                    format!("Responses API returned status {:?}", parsed.status),
                )
                .into());
            }
            if let Some(usage) = &parsed.usage {
                let details = usage.input_tokens_details.as_ref();
                tracing::info!(
                    "Responses usage: in={}, out={}, cache_read={}, cache_write={}",
                    usage.input_tokens,
                    usage.output_tokens,
                    details.and_then(|d| d.cached_tokens).unwrap_or(0),
                    details.and_then(|d| d.cache_write_tokens).unwrap_or(0)
                );
            } else {
                tracing::info!(
                    "{}OpenAI Responses response received without usage telemetry.",
                    crate::ai::get_log_prefix()
                );
            }
            return Ok(parsed);
        }

        let error_text = redact_secret(&response_text);
        let error = match status {
            StatusCode::TOO_MANY_REQUESTS => {
                let mut delay = retry_after.unwrap_or(Duration::from_secs(60));
                if let Some(captures) = retry_re.captures(&error_text)
                    && let Ok(seconds) = captures[1].parse::<f64>()
                {
                    delay = Duration::from_secs_f64(seconds);
                }
                OpenAiCompatError::RateLimitExceeded(delay)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                OpenAiCompatError::AuthenticationError(error_text)
            }
            status if status.is_server_error() => {
                OpenAiCompatError::TransientError(retry_after.unwrap_or(Duration::ZERO), error_text)
            }
            _ => OpenAiCompatError::ApiError(status, error_text),
        };
        Err(error.into())
    }
}

fn message_item(role: &str, content: String) -> Value {
    // Azure AI Foundry requires the discriminator on input message items.
    // OpenAI accepts the same canonical Responses API representation.
    json!({"type": "message", "role": role, "content": content})
}

fn item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

/// Bedrock accepts `prompt_cache_breakpoint` only on `input_text` parts.
fn add_cache_breakpoint(item: &mut Value) -> bool {
    let is_prompt_message = item_type(item) == Some("message")
        && matches!(
            item.get("role").and_then(Value::as_str),
            Some("system" | "developer" | "user")
        );
    if !is_prompt_message {
        return false;
    }

    let marker = json!({"mode": "explicit"});
    match item.get_mut("content") {
        Some(content) if content.is_string() => {
            let text = content.take();
            *content = json!([{
                "type": "input_text",
                "text": text,
                "prompt_cache_breakpoint": marker,
            }]);
            true
        }
        Some(Value::Array(parts)) => {
            match parts
                .iter_mut()
                .rev()
                .find(|part| item_type(part) == Some("input_text"))
            {
                Some(part) => {
                    part["prompt_cache_breakpoint"] = marker;
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

/// Marks the end of the system prompt and the last user message, leaving one
/// of Bedrock's four cache writes for the automatic breakpoint in implicit mode.
fn apply_prompt_cache(
    request: &mut ResponsesRequest,
    mode: crate::settings::PromptCacheMode,
    ttl: Option<&str>,
) {
    let mut options = json!({"mode": mode.as_str()});
    if let Some(ttl) = ttl {
        options["ttl"] = json!(ttl);
    }
    request.prompt_cache_options = Some(options);

    let input = &mut request.input;
    let is_system = |item: &Value| {
        item_type(item) == Some("message")
            && matches!(
                item.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
    };
    let system_end = input
        .iter()
        .take_while(|item| is_system(item))
        .count()
        .checked_sub(1);
    let last_user = input.iter().rposition(|item| {
        item_type(item) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("user")
    });

    for index in [system_end, last_user].into_iter().flatten() {
        add_cache_breakpoint(&mut input[index]);
    }
}

/// Keep only output items accepted as continuation input, removing a trailing
/// reasoning suffix that has no later message or function call.
fn sanitize_replay_items(items: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut candidates: Vec<Value> = items
        .into_iter()
        .filter(|item| {
            matches!(
                item_type(item),
                Some("reasoning" | "message" | "function_call")
            )
        })
        .collect();

    let Some(last_output_index) = candidates
        .iter()
        .rposition(|item| matches!(item_type(item), Some("message" | "function_call")))
    else {
        return Vec::new();
    };

    candidates.truncate(last_output_index + 1);
    candidates
}

fn translate_ai_request(
    request: AiRequest,
    model: String,
    max_tokens: u32,
    reasoning_effort: Option<String>,
) -> Result<ResponsesRequest> {
    let AiRequest {
        system,
        messages,
        tools,
        temperature: _,
        response_format,
        context_tag: _,
    } = request;

    let mut input = Vec::new();
    if let Some(system) = system {
        input.push(message_item("system", system));
    }

    for message in messages {
        match message.role {
            AiRole::System => {
                if let Some(content) = message.content {
                    input.push(message_item("system", content));
                }
            }
            AiRole::User => {
                if let Some(content) = message.content {
                    input.push(message_item("user", content));
                }
            }
            AiRole::Assistant => {
                if let Some(replay_items) = message.reasoning.as_ref().and_then(|blocks| {
                    blocks.iter().find_map(|block| match block {
                        ReasoningBlock::ProviderOutput {
                            provider, items, ..
                        } if provider == PROVIDER_TAG => Some(items),
                        _ => None,
                    })
                }) {
                    let replay_items = sanitize_replay_items(replay_items.iter().cloned());
                    if !replay_items.is_empty() {
                        input.extend(replay_items);
                        continue;
                    }
                }

                let has_content = message
                    .content
                    .as_ref()
                    .is_some_and(|text| !text.is_empty());
                let has_tool_calls = message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty());
                if (has_content || has_tool_calls)
                    && let Some(reasoning) = message.reasoning
                {
                    for block in reasoning {
                        if let ReasoningBlock::Provider { provider, data } = block
                            && provider == PROVIDER_TAG
                            && data.get("type").and_then(Value::as_str) == Some("reasoning")
                        {
                            input.push(data);
                        }
                    }
                }
                if let Some(content) = message.content
                    && !content.is_empty()
                {
                    input.push(message_item("assistant", content));
                }
                if let Some(tool_calls) = message.tool_calls {
                    for tool_call in tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": tool_call.id,
                            "name": tool_call.function_name,
                            "arguments": serde_json::to_string(&tool_call.arguments)?,
                        }));
                    }
                }
            }
            AiRole::Tool => {
                let call_id = message.tool_call_id.ok_or_else(|| {
                    anyhow!("OpenAI Responses tool result is missing tool_call_id")
                })?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": message.content.unwrap_or_default(),
                }));
            }
        }
    }

    let tools = tools.and_then(|tools| {
        (!tools.is_empty()).then(|| {
            tools
                .into_iter()
                .map(|tool| ResponsesTool {
                    tool_type: "function",
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.parameters,
                })
                .collect()
        })
    });

    let text = match response_format {
        Some(AiResponseFormat::Json { .. }) => {
            // The session prompt already carries the full schema. Using
            // json_object here preserves the Chat Completions behavior and
            // avoids imposing OpenAI's stricter structured-output schema
            // subset on schemas designed for other providers.
            let has_json = input.iter().any(|item| {
                item.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.to_ascii_lowercase().contains("json"))
            });
            if !has_json {
                input.insert(
                    0,
                    message_item("system", "Respond in JSON format.".to_string()),
                );
            }
            Some(json!({"format": {"type": "json_object"}}))
        }
        Some(AiResponseFormat::Text) | None => None,
    };

    let reasoning = reasoning_effort.map(|effort| json!({"effort": effort}));

    Ok(ResponsesRequest {
        model,
        input,
        tools,
        max_output_tokens: max_tokens,
        text,
        reasoning,
        include: vec!["reasoning.encrypted_content".to_string()],
        store: false,
        prompt_cache_options: None,
    })
}

fn translate_ai_response(response: ResponsesResponse) -> Result<AiResponse> {
    let incomplete_reason = response
        .incomplete_details
        .as_ref()
        .and_then(|details| details.reason.as_deref());
    if response.status.as_deref() == Some("incomplete")
        && incomplete_reason != Some("max_output_tokens")
    {
        return Err(OpenAiCompatError::ApiError(
            StatusCode::OK,
            format!(
                "Responses API returned an incomplete response: {}",
                incomplete_reason.unwrap_or("unknown reason")
            ),
        )
        .into());
    }

    let output_token_estimate = response
        .usage
        .as_ref()
        .map(|usage| usage.output_tokens as usize);
    let replay_items = sanitize_replay_items(response.output.iter().cloned());
    let response_reasoning_items = response
        .output
        .iter()
        .filter(|item| item_type(item) == Some("reasoning"))
        .count();
    let replay_reasoning_items = replay_items
        .iter()
        .filter(|item| item_type(item) == Some("reasoning"))
        .count();
    if replay_reasoning_items < response_reasoning_items {
        tracing::warn!(
            "{}Ignoring {} orphan OpenAI Responses reasoning item(s).",
            crate::ai::get_log_prefix(),
            response_reasoning_items - replay_reasoning_items
        );
    }
    let has_replay_reasoning = replay_items
        .iter()
        .any(|item| item_type(item) == Some("reasoning"));
    let mut content_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for item in response.output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    content_parts.push(text.to_string());
                                }
                            }
                            Some("refusal") => {
                                if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                                    content_parts.push(text.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Responses function_call is missing call_id"))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("Responses function_call is missing name"))?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(|arguments| serde_json::from_str(arguments).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null);
                tool_calls.push(ToolCall {
                    id: call_id.to_string(),
                    function_name: name.to_string(),
                    arguments,
                    thought_signature: None,
                });
            }
            Some("reasoning") => {}
            _ => {}
        }
    }

    let reasoning = has_replay_reasoning.then(|| {
        vec![ReasoningBlock::ProviderOutput {
            provider: PROVIDER_TAG.to_string(),
            items: replay_items,
            token_estimate: output_token_estimate,
        }]
    });

    let usage = response.usage.map(|usage| AiUsage {
        prompt_tokens: usage.input_tokens as usize,
        completion_tokens: usage.output_tokens as usize,
        total_tokens: usage.total_tokens as usize,
        cached_tokens: usage
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .map(|tokens| tokens as usize),
        cache_write_tokens: usage
            .input_tokens_details
            .as_ref()
            .and_then(|details| details.cache_write_tokens)
            .map(|tokens| tokens as usize),
    });
    let truncated = incomplete_reason == Some("max_output_tokens");

    if truncated {
        tracing::warn!(
            "{}OpenAI Responses output was truncated at max_output_tokens.",
            crate::ai::get_log_prefix()
        );
    }

    Ok(AiResponse {
        content: (!content_parts.is_empty()).then(|| content_parts.join("")),
        thought: None,
        thought_signature: None,
        reasoning,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        usage,
        truncated,
    })
}

fn estimate_tokens(request: &AiRequest) -> usize {
    let mut generic_request = request.clone();
    let mut replay_tokens = 0;

    for message in &mut generic_request.messages {
        let provider_estimate = message.reasoning.as_ref().and_then(|blocks| {
            blocks.iter().find_map(|block| match block {
                ReasoningBlock::ProviderOutput {
                    provider,
                    token_estimate: Some(tokens),
                    ..
                } if provider == PROVIDER_TAG => Some(*tokens),
                _ => None,
            })
        });

        if let Some(tokens) = provider_estimate {
            // The provider's output count covers both visible output and
            // encrypted reasoning. Replay uses the raw transcript instead of
            // these normalized fields, so counting both would double-charge.
            message.content = None;
            message.tool_calls = None;
            replay_tokens += tokens;
        }
    }

    estimate_tokens_generic(&generic_request) + replay_tokens
}

#[async_trait]
impl AiProvider for OpenAiResponsesClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!(
            "{}Sending OpenAI Responses request...",
            crate::ai::get_log_prefix()
        );
        let mut body = translate_ai_request(
            request,
            self.model.clone(),
            self.max_tokens,
            self.reasoning_effort.clone(),
        )?;
        if let Some(mode) = self.prompt_cache {
            apply_prompt_cache(&mut body, mode, self.prompt_cache_ttl.as_deref());
        }
        let response = self.post_request(&body).await?;
        translate_ai_response(response)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        estimate_tokens(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }

    fn caches_prompt_prefix(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiTool};

    fn message(role: AiRole, content: Option<&str>) -> AiMessage {
        AiMessage {
            role,
            content: content.map(str::to_string),
            thought: None,
            thought_signature: None,
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn request(messages: Vec<AiMessage>) -> AiRequest {
        AiRequest {
            system: None,
            messages,
            tools: None,
            temperature: Some(0.7),
            response_format: None,
            context_tag: None,
        }
    }

    #[test]
    fn normalizes_azure_foundry_project_url() -> Result<()> {
        let url = OpenAiResponsesClient::normalize_base_url(
            "https://example.services.ai.azure.com/api/projects/proj-default/openai/v1",
        )?;
        assert_eq!(
            url,
            "https://example.services.ai.azure.com/api/projects/proj-default/openai/v1/responses"
        );
        Ok(())
    }

    #[test]
    fn preserves_query_on_complete_endpoint() -> Result<()> {
        let url = OpenAiResponsesClient::normalize_base_url(
            "https://example.test/openai/v1/responses?api-version=preview",
        )?;
        assert_eq!(
            url,
            "https://example.test/openai/v1/responses?api-version=preview"
        );
        Ok(())
    }

    #[test]
    fn azure_api_management_uses_api_key_authentication() {
        assert!(OpenAiResponsesClient::uses_azure_api_key(
            "https://example.azure-api.net/openai/responses?api-version=2025-04-01-preview"
        ));
        assert!(!OpenAiResponsesClient::uses_azure_api_key(
            "https://example.services.ai.azure.com/api/projects/proj/openai/v1/responses"
        ));
        assert!(!OpenAiResponsesClient::uses_azure_api_key(
            "https://api.openai.com/v1/responses"
        ));
    }

    /// Env lookup stub. The real environment is process-global and these tests
    /// run threaded in one binary, so the policy is exercised through an
    /// injected map instead of `env::set_var`.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn named_api_key_env_takes_precedence_over_shared_keys() -> Result<()> {
        let lookup = env_of(&[
            ("AZURE_GATEWAY_TWO", "second-key"),
            ("OPENAI_API_KEY", "shared-key"),
        ]);
        assert_eq!(
            OpenAiResponsesClient::resolve_api_key(Some("AZURE_GATEWAY_TWO"), lookup)?,
            "second-key"
        );
        Ok(())
    }

    #[test]
    fn named_api_key_env_trims_trailing_newline() -> Result<()> {
        let lookup = env_of(&[("AZURE_GATEWAY_TWO", "second-key\n")]);
        assert_eq!(
            OpenAiResponsesClient::resolve_api_key(Some("AZURE_GATEWAY_TWO"), lookup)?,
            "second-key"
        );
        Ok(())
    }

    #[test]
    fn named_api_key_env_errors_when_missing_or_empty() {
        // Naming a variable asserts it exists; falling back to the shared key
        // here would authenticate gateway #2 with gateway #1's credentials.
        let missing = OpenAiResponsesClient::resolve_api_key(
            Some("AZURE_GATEWAY_TWO"),
            env_of(&[("OPENAI_API_KEY", "shared-key")]),
        );
        let message = missing
            .expect_err("missing variable must error")
            .to_string();
        assert!(message.contains("AZURE_GATEWAY_TWO"), "{message}");

        let empty = OpenAiResponsesClient::resolve_api_key(
            Some("AZURE_GATEWAY_TWO"),
            env_of(&[("AZURE_GATEWAY_TWO", "   ")]),
        );
        assert!(empty.is_err(), "whitespace-only variable must error");
    }

    #[test]
    fn unnamed_api_key_env_falls_back_and_stays_lenient() -> Result<()> {
        assert_eq!(
            OpenAiResponsesClient::resolve_api_key(None, env_of(&[("OPENAI_API_KEY", "first")]))?,
            "first"
        );
        assert_eq!(
            OpenAiResponsesClient::resolve_api_key(None, env_of(&[("LLM_API_KEY", "second")]))?,
            "second"
        );
        // No key configured at all stays a warning-free empty string so that
        // existing unauthenticated/local setups keep working.
        assert_eq!(
            OpenAiResponsesClient::resolve_api_key(None, env_of(&[]))?,
            ""
        );
        Ok(())
    }

    #[test]
    fn auth_header_matches_gateway_flavor() -> Result<()> {
        let (name, value) = OpenAiResponsesClient::auth_header(
            "https://example.azure-api.net/openai/responses?api-version=2025-04-01-preview",
            "apim-key",
        )?;
        assert_eq!(name.as_str(), "api-key");
        assert_eq!(value.to_str()?, "apim-key");

        let (name, value) =
            OpenAiResponsesClient::auth_header("https://api.openai.com/v1/responses", "sk-token")?;
        assert_eq!(name, reqwest::header::AUTHORIZATION);
        assert_eq!(value.to_str()?, "Bearer sk-token");
        Ok(())
    }

    fn bedrock_token_options() -> ResponsesOptions {
        ResponsesOptions {
            bedrock_token: true,
            ..Default::default()
        }
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn bedrock_token_needs_no_static_api_key() {
        // Region comes from the URL, so neither API-key nor AWS env is read.
        let client = OpenAiResponsesClient::new(
            "https://bedrock-mantle.eu-north-1.api.aws/v1".to_string(),
            "model".to_string(),
            1000,
            100,
            30,
            bedrock_token_options(),
        )
        .expect("client without API key");
        assert!(client.bedrock_token);
        assert_eq!(client.token_region.as_deref(), Some("eu-north-1"));
    }

    #[cfg(not(feature = "bedrock"))]
    #[test]
    fn bedrock_token_requires_bedrock_feature() {
        let error = OpenAiResponsesClient::new(
            "https://bedrock-mantle.eu-north-1.api.aws/v1".to_string(),
            "model".to_string(),
            1000,
            100,
            30,
            bedrock_token_options(),
        )
        .err()
        .expect("must fail without the bedrock feature");
        assert_eq!(
            error.to_string(),
            "bedrock_token requires the 'bedrock' feature"
        );
    }

    #[test]
    fn translates_tools_and_omits_temperature() -> Result<()> {
        let mut req = request(vec![message(AiRole::User, Some("Review this"))]);
        req.system = Some("Be precise".to_string());
        req.tools = Some(vec![AiTool {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({"type": "object"}),
        }]);
        let translated =
            translate_ai_request(req, "gpt-5.6".to_string(), 8192, Some("high".to_string()))?;
        let value = serde_json::to_value(translated)?;

        assert_eq!(value["model"], "gpt-5.6");
        assert_eq!(value["max_output_tokens"], 8192);
        assert_eq!(value["input"][0]["type"], "message");
        assert_eq!(value["input"][0]["role"], "system");
        assert_eq!(value["input"][1]["type"], "message");
        assert_eq!(value["input"][1]["role"], "user");
        assert_eq!(value["tools"][0]["name"], "read_file");
        assert_eq!(value["reasoning"]["effort"], "high");
        assert_eq!(value["include"][0], "reasoning.encrypted_content");
        assert_eq!(value["store"], false);
        assert!(value.get("temperature").is_none());
        Ok(())
    }

    #[test]
    fn translates_json_format_without_imposing_strict_schema() -> Result<()> {
        let mut req = request(vec![message(AiRole::User, Some("Score it"))]);
        req.response_format = Some(AiResponseFormat::Json {
            schema: Some(json!({"type": "object"})),
        });
        let translated = translate_ai_request(req, "model".to_string(), 100, None)?;
        assert_eq!(
            translated.text.as_ref().unwrap()["format"]["type"],
            "json_object"
        );
        assert_eq!(translated.input[0]["role"], "system");
        assert_eq!(translated.input[0]["type"], "message");
        assert_eq!(translated.input[0]["content"], "Respond in JSON format.");
        Ok(())
    }

    #[test]
    fn emits_message_discriminator_for_assistant_input() -> Result<()> {
        let translated = translate_ai_request(
            request(vec![message(AiRole::Assistant, Some("Prior answer"))]),
            "model".to_string(),
            100,
            None,
        )?;

        assert_eq!(translated.input[0]["type"], "message");
        assert_eq!(translated.input[0]["role"], "assistant");
        assert_eq!(translated.input[0]["content"], "Prior answer");
        Ok(())
    }

    #[test]
    fn replays_reasoning_function_calls_and_outputs() -> Result<()> {
        let reasoning_item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": "opaque"
        });
        let mut assistant = message(AiRole::Assistant, None);
        assistant.reasoning = Some(vec![ReasoningBlock::Provider {
            provider: PROVIDER_TAG.to_string(),
            data: reasoning_item.clone(),
        }]);
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_string(),
            function_name: "inspect".to_string(),
            arguments: json!({"path": "src/lib.rs"}),
            thought_signature: None,
        }]);
        let mut tool = message(AiRole::Tool, Some("contents"));
        tool.tool_call_id = Some("call_1".to_string());

        let translated = translate_ai_request(
            request(vec![assistant, tool]),
            "model".to_string(),
            100,
            None,
        )?;
        assert_eq!(translated.input[0], reasoning_item);
        assert_eq!(translated.input[1]["type"], "function_call");
        assert_eq!(translated.input[1]["call_id"], "call_1");
        assert_eq!(translated.input[2]["type"], "function_call_output");
        assert_eq!(translated.input[2]["output"], "contents");
        Ok(())
    }

    #[test]
    fn translates_response_reasoning_tools_usage_and_truncation() -> Result<()> {
        let response: ResponsesResponse = serde_json::from_value(json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "partial"}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "inspect",
                 "arguments": "{\"path\":\"src/lib.rs\"}"}
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "total_tokens": 120,
                "input_tokens_details": {"cached_tokens": 80}
            }
        }))?;
        let translated = translate_ai_response(response)?;

        assert_eq!(translated.content.as_deref(), Some("partial"));
        assert!(translated.truncated);
        assert_eq!(translated.usage.as_ref().unwrap().cached_tokens, Some(80));
        assert_eq!(translated.tool_calls.as_ref().unwrap()[0].id, "call_1");
        assert!(matches!(
            translated.reasoning.as_ref().unwrap()[0],
            ReasoningBlock::ProviderOutput {
                ref provider,
                token_estimate: Some(20),
                ..
            } if provider == PROVIDER_TAG
        ));
        Ok(())
    }

    #[test]
    fn preserves_reasoning_and_function_call_interleaving() -> Result<()> {
        let output = vec![
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "one"}),
            json!({"type": "function_call", "call_id": "call_1", "name": "inspect",
                   "arguments": "{\"path\":\"one\"}"}),
            json!({"type": "reasoning", "id": "rs_2", "encrypted_content": "two"}),
            json!({"type": "function_call", "call_id": "call_2", "name": "inspect",
                   "arguments": "{\"path\":\"two\"}"}),
        ];
        let response: ResponsesResponse = serde_json::from_value(json!({
            "status": "completed",
            "output": output,
            "usage": {"input_tokens": 10, "output_tokens": 200, "total_tokens": 210}
        }))?;
        let translated = translate_ai_response(response)?;
        let assistant = AiMessage {
            role: AiRole::Assistant,
            content: translated.content,
            thought: None,
            thought_signature: None,
            reasoning: translated.reasoning,
            tool_calls: translated.tool_calls,
            tool_call_id: None,
        };

        let replay = translate_ai_request(
            request(vec![assistant]),
            "model".to_string(),
            DEFAULT_MAX_OUTPUT_TOKENS,
            None,
        )?;
        assert_eq!(replay.input, output);
        Ok(())
    }

    #[test]
    fn preserves_consecutive_reasoning_and_drops_only_trailing_reasoning() {
        let rs_1 = json!({"type": "reasoning", "id": "rs_1"});
        let rs_2 = json!({"type": "reasoning", "id": "rs_2"});
        let message = json!({"type": "message", "role": "assistant", "content": []});
        let trailing = json!({"type": "reasoning", "id": "rs_trailing"});

        let replay =
            sanitize_replay_items(vec![rs_1.clone(), rs_2.clone(), message.clone(), trailing]);

        assert_eq!(replay, vec![rs_1, rs_2, message]);
    }

    #[test]
    fn drops_reasoning_without_a_following_output_item() -> Result<()> {
        let response: ResponsesResponse = serde_json::from_value(json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_orphan", "encrypted_content": "opaque"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 20, "total_tokens": 30}
        }))?;
        let translated = translate_ai_response(response)?;

        assert!(translated.reasoning.is_none());
        assert!(translated.content.is_none());
        assert!(translated.tool_calls.is_none());

        // Also keep histories serialized by the original implementation from
        // replaying their individual reasoning item as an orphan.
        let mut legacy_assistant = message(AiRole::Assistant, None);
        legacy_assistant.reasoning = Some(vec![ReasoningBlock::Provider {
            provider: PROVIDER_TAG.to_string(),
            data: json!({
                "type": "reasoning",
                "id": "rs_legacy",
                "encrypted_content": "opaque"
            }),
        }]);
        let replay = translate_ai_request(
            request(vec![legacy_assistant]),
            "model".to_string(),
            DEFAULT_MAX_OUTPUT_TOKENS,
            None,
        )?;
        assert!(replay.input.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_non_truncation_incomplete_response() -> Result<()> {
        let response: ResponsesResponse = serde_json::from_value(json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "content_filter"},
            "output": []
        }))?;

        let error = translate_ai_response(response).unwrap_err();
        assert!(error.to_string().contains("content_filter"));
        Ok(())
    }

    #[test]
    fn estimates_replayed_reasoning_from_provider_usage() {
        let mut assistant = message(AiRole::Assistant, Some("visible output"));
        assistant.reasoning = Some(vec![ReasoningBlock::ProviderOutput {
            provider: PROVIDER_TAG.to_string(),
            items: vec![
                json!({"type": "reasoning", "encrypted_content": "very-large-opaque-value"}),
                json!({"type": "message", "role": "assistant", "content": []}),
            ],
            token_estimate: Some(900),
        }]);
        let req = request(vec![assistant]);

        assert_eq!(estimate_tokens(&req), 900);
    }

    fn cached_request_value(
        req: AiRequest,
        cache: Option<(crate::settings::PromptCacheMode, Option<&str>)>,
    ) -> Result<Value> {
        let mut translated = translate_ai_request(req, "model".to_string(), 100, None)?;
        if let Some((mode, ttl)) = cache {
            apply_prompt_cache(&mut translated, mode, ttl);
        }
        Ok(serde_json::to_value(translated)?)
    }

    fn cache_request() -> AiRequest {
        let mut req = request(vec![
            message(AiRole::User, Some("first")),
            message(AiRole::Assistant, Some("answer")),
            message(AiRole::User, Some("second")),
        ]);
        req.system = Some("Be precise".to_string());
        req.tools = Some(vec![AiTool {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({"type": "object"}),
        }]);
        req
    }

    #[test]
    fn prompt_cache_off_leaves_request_unchanged() -> Result<()> {
        let value = cached_request_value(cache_request(), None)?;

        assert!(value.get("prompt_cache_options").is_none());
        assert!(!value.to_string().contains("prompt_cache_breakpoint"));
        assert_eq!(value["input"][0]["content"], "Be precise");
        assert_eq!(value["input"][1]["content"], "first");
        Ok(())
    }

    #[test]
    fn implicit_prompt_cache_marks_system_and_last_user() -> Result<()> {
        use crate::settings::PromptCacheMode;
        let value = cached_request_value(
            cache_request(),
            Some((PromptCacheMode::Implicit, Some("30m"))),
        )?;

        assert_eq!(
            value["prompt_cache_options"],
            json!({"mode": "implicit", "ttl": "30m"})
        );
        let marker = json!({"mode": "explicit"});
        assert_eq!(
            value["input"][0]["content"],
            json!([{
                "type": "input_text",
                "text": "Be precise",
                "prompt_cache_breakpoint": marker,
            }])
        );
        assert_eq!(value["input"][1]["content"], "first");
        assert_eq!(value["input"][2]["content"], "answer");
        assert_eq!(
            value["input"][3]["content"],
            json!([{
                "type": "input_text",
                "text": "second",
                "prompt_cache_breakpoint": marker,
            }])
        );
        assert_eq!(
            value.to_string().matches("prompt_cache_breakpoint").count(),
            2
        );
        assert!(
            !value["tools"]
                .to_string()
                .contains("prompt_cache_breakpoint")
        );
        Ok(())
    }

    #[test]
    fn explicit_prompt_cache_without_ttl_omits_ttl() -> Result<()> {
        use crate::settings::PromptCacheMode;
        let value = cached_request_value(cache_request(), Some((PromptCacheMode::Explicit, None)))?;

        assert_eq!(value["prompt_cache_options"], json!({"mode": "explicit"}));
        Ok(())
    }

    #[test]
    fn prompt_cache_marks_injected_json_system_message() -> Result<()> {
        use crate::settings::PromptCacheMode;
        let mut req = request(vec![message(AiRole::User, Some("Score it"))]);
        req.response_format = Some(AiResponseFormat::Json { schema: None });
        let value = cached_request_value(req, Some((PromptCacheMode::Implicit, None)))?;

        assert_eq!(
            value["input"][0]["content"][0]["text"],
            "Respond in JSON format."
        );
        assert_eq!(
            value["input"][0]["content"][0]["prompt_cache_breakpoint"],
            json!({"mode": "explicit"})
        );
        assert_eq!(
            value.to_string().matches("prompt_cache_breakpoint").count(),
            2
        );
        Ok(())
    }

    #[test]
    fn prompt_cache_keeps_replayed_items_unchanged() -> Result<()> {
        use crate::settings::PromptCacheMode;
        let items = vec![
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"}),
            json!({"type": "message", "role": "assistant",
                   "content": [{"type": "output_text", "text": "done"}]}),
            json!({"type": "function_call", "call_id": "call_1", "name": "inspect",
                   "arguments": "{}"}),
        ];
        let mut assistant = message(AiRole::Assistant, None);
        assistant.reasoning = Some(vec![ReasoningBlock::ProviderOutput {
            provider: PROVIDER_TAG.to_string(),
            items: items.clone(),
            token_estimate: None,
        }]);
        let mut tool = message(AiRole::Tool, Some("contents"));
        tool.tool_call_id = Some("call_1".to_string());
        let mut req = request(vec![message(AiRole::User, Some("go")), assistant, tool]);
        req.system = Some("Be precise".to_string());

        let value = cached_request_value(req, Some((PromptCacheMode::Implicit, None)))?;
        let input = value["input"].as_array().unwrap();

        assert_eq!(input[2..5], items[..]);
        assert_eq!(input[5]["type"], "function_call_output");
        assert!(input[5].get("prompt_cache_breakpoint").is_none());
        assert_eq!(
            value.to_string().matches("prompt_cache_breakpoint").count(),
            2
        );
        Ok(())
    }

    #[test]
    fn maps_cache_write_tokens_from_usage() -> Result<()> {
        let response: ResponsesResponse = serde_json::from_value(json!({
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 2048,
                "output_tokens": 10,
                "total_tokens": 2058,
                "input_tokens_details": {"cached_tokens": 1920, "cache_write_tokens": 128}
            }
        }))?;
        let usage = translate_ai_response(response)?.usage.unwrap();

        assert_eq!(usage.cached_tokens, Some(1920));
        assert_eq!(usage.cache_write_tokens, Some(128));
        Ok(())
    }
}
