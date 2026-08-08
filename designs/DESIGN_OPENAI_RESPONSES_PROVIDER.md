# DESIGN: OpenAI Responses API and Azure AI Foundry

## Goals

- Run OpenAI reasoning models through the Responses API, including Azure AI
  Foundry project endpoints whose API root ends in `/openai/v1`.
- Allow a Responses model to run as an additional review source while a
  Bedrock/Claude model remains the main source in the same Sashiko instance.
- Preserve tool calling, reasoning continuity, token budgets, retry
  classification, truncation handling, and provider-reported prompt-cache
  usage across the existing provider and stdio IPC boundaries.
- Keep the generic OpenAI-compatible Chat Completions transport unchanged.

## Non-goals

- Replacing the `openai` or `openai-compatible` Chat Completions providers.
- Sharing a mutable Responses conversation or `previous_response_id` between
  review stages.
- Translating OpenAI encrypted reasoning into Claude/Bedrock reasoning blocks.
- Adding explicit OpenAI cache breakpoints; OpenAI and Azure cache eligible
  prompt prefixes automatically.

## Provider Boundary

The provider name is `openai-responses`. It has a separate implementation in
`src/ai/openai_responses.rs` because Responses differs materially from Chat
Completions:

- messages, function calls, and function-call outputs are peer input items;
- tools use the flat Responses function-tool shape;
- output is a heterogeneous item list rather than `choices`;
- reasoning models return opaque reasoning items that must be echoed back;
- the output limit is `max_output_tokens`.

The provider reuses `[ai.openai_compat]` for endpoint and token limits so an
additional model can override those settings without adding another parallel
configuration hierarchy. `reasoning_effort` is used only by Responses.

## Azure Endpoint and Authentication

`base_url` accepts either:

- an HTTP(S) API root ending in `/v1`; or
- a complete endpoint ending in `/responses`.

The client appends `/responses` to `/v1` roots and preserves URL query
parameters, including `?api-version=preview` on deployments that require it.
This includes Azure project roots such as:

```text
https://RESOURCE.services.ai.azure.com/api/projects/PROJECT/openai/v1
```

Authentication uses `Authorization: Bearer` with `OPENAI_API_KEY`, falling
back to `LLM_API_KEY`, matching the OpenAI-compatible v1 interface exposed by
Azure AI Foundry. Credentials remain environment-only.

On Azure, `model` is the deployment name rather than necessarily the upstream
OpenAI model identifier.

## Stateless Reasoning Continuity

One provider instance can serve concurrent stages, so it must not keep a
mutable `previous_response_id`. Every request instead sets:

```json
{
  "store": false,
  "include": ["reasoning.encrypted_content"]
}
```

Replayable output items are retained as one ordered
`ReasoningBlock::ProviderOutput` transcript. The complete reasoning, message,
and function-call JSON items pass through the existing `AiResponse`/`AiMessage`
IPC and session history path, preserving sequences such as `reasoning`,
`function_call`, `reasoning`, `function_call`. Replay uses this transcript
instead of reconstructing and reordering the normalized message fields.
Trailing reasoning without a following message or function call is discarded
because the Responses API rejects orphan reasoning items. Opaque state tagged
for another provider is ignored. Bedrock likewise skips OpenAI-owned state
rather than attempting an invalid translation.

This data is stripped from normal persisted review history by
`scrub_ai_signatures`, but remains in live session memory. The opt-in
`dump_conversation` and local `response_cache` features retain complete raw
requests/responses, including opaque reasoning, on local disk; their files
must be protected accordingly.

## Request Mapping

| Sashiko value | Responses value |
|---|---|
| system/user/assistant text | `{ "role": ..., "content": ... }` input item |
| assistant tool call | `function_call` with `call_id`, `name`, and JSON-string arguments |
| tool result | `function_call_output` with matching `call_id` |
| tool definition | flat `function` tool with `name`, `description`, and `parameters` |
| output limit | `max_output_tokens` |
| reasoning effort | `reasoning.effort` |
| JSON response | `text.format.type = "json_object"` |

Temperature is deliberately omitted. Reasoning models can reject sampling
parameters accepted by Chat Completions.

Sashiko's schema remains in the prompt. The provider uses JSON-object mode
rather than strict OpenAI structured output because review schemas are shared
with other providers and do not all satisfy OpenAI's strict schema subset.

## Response Mapping

- `message.content[].output_text` becomes `AiResponse.content`.
- `function_call` items become Sashiko `ToolCall`s keyed by `call_id`.
- `reasoning` items become provider-owned `ReasoningBlock`s.
- `input_tokens`, `output_tokens`, and `total_tokens` map directly to
  `AiUsage`.
- `input_tokens_details.cached_tokens` maps to `AiUsage.cached_tokens`.
- `incomplete_details.reason == "max_output_tokens"` marks the response as
  truncated so the session reports output exhaustion instead of a misleading
  JSON validation error.
- Other incomplete reasons, including `content_filter`, are surfaced as fatal
  provider errors instead of entering JSON-validation retries.

Responses counts hidden reasoning against `max_output_tokens`, so this
provider defaults to 16384 rather than the Chat Completions default of 4096.
For later requests, the provider-reported output token count is retained with
the ordered transcript and used to estimate its input cost. Tokenizing opaque
encrypted content directly would produce an unreliable estimate.

HTTP authentication failures are fatal, 429 responses retain retry timing,
and transport/5xx failures use the existing typed OpenAI retry classifier.

## Prompt and Response Caching

The provider logs uncached input, cached input, and output counts from each
successful response. It advertises prompt-prefix caching because supported
OpenAI/Azure models automatically reuse eligible prefixes. This lets Sashiko
elect one opener before concurrent stages sharing a prefix proceed. The
service remains the authority for whether a particular request was cached;
usage telemetry is read from the response.

Sashiko's optional local response cache has two important invariants:

1. Provider reasoning and signatures are part of the cache key. They are
   semantically relevant continuation state even though logs scrub them.
2. A local cache hit replaces provider-native cache telemetry:
   `cached_tokens` becomes the full prompt count and `cache_write_tokens` is
   cleared. Adding the prompt count to the original provider cache-read count
   would report impossible totals.

`context_tag` remains excluded from the key because it is logging metadata.

## Additional-model Configuration

```toml
[[ai.additional_models]]
name = "azure-gpt"
probability = 1.0
provider = "openai-responses"
model = "DEPLOYMENT-NAME"

[ai.additional_models.openai_compat]
base_url = "https://RESOURCE.services.ai.azure.com/api/projects/PROJECT/openai/v1"
context_window_size = 400000
max_tokens = 16384
reasoning_effort = "high"
```

The additional source receives its own provider, session history, budget, and
provenance route. Bedrock credentials and the OpenAI key can therefore coexist
in one daemon process.

## Validation and Known Limits

Regression coverage includes Azure URL normalization, request shape,
temperature omission, JSON mode, ordered tool/reasoning replay, orphan
reasoning removal, incomplete-response handling, replay token estimation,
usage and truncation mapping, additional-model configuration, provider
construction, reasoning-aware response-cache keys, and local-cache accounting.

The implementation is validated with unit, integration, formatting, and lint
checks. A live Azure request is still an operational verification step because
the development sandbox used for this change could not resolve external DNS.
