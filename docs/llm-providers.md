# LLM Provider Configuration

Sashiko supports multiple LLM providers. This guide covers setup for each
one. Gemini is the default and simplest to configure; the others are
alternatives you can swap in depending on your infrastructure and
preferences.

For all providers, configuration lives in `Settings.toml` at the project
root. Per-provider example files are available in
[docs/examples/](examples/).

## Gemini (default)

The quickest way to get started.

```bash
cp docs/examples/Settings.example.toml Settings.toml
export LLM_API_KEY="your-gemini-api-key"
```

The example file sets `provider = "gemini"` and
`model = "gemini-3.1-pro-preview"`. Adjust the model name as needed.

You can also set any config value via environment variables using the
`SASHIKO` prefix with `__` (double underscore) as the separator:

```bash
export SASHIKO__AI__PROVIDER=gemini
export SASHIKO__AI__MODEL=gemini-3.1-pro-preview
```

## Claude (API)

Uses Anthropic's Claude API directly.

**Get an API key:** https://console.anthropic.com/

**Set credentials:**

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
# Or use the generic fallback:
export LLM_API_KEY="sk-ant-..."
```

**Apply the example config:**

```bash
cp docs/examples/Settings.claude.toml Settings.toml
```

**What you get:**

- Automatic prompt caching (5-minute TTL) to reduce costs on repeated context
- Full tool/function calling support for git operations
- Automatic retry on rate limits and API overload
- 200K context window (use `max_input_tokens = 40000` for cost-conscious
  defaults)
- Extended thinking via the `thinking` and `effort` settings in
  `[ai.claude]`

## Claude Code CLI

Uses a local [Claude Code](https://claude.com/claude-code) installation as
the completion backend. This uses your Claude Code subscription -- no
per-token charge, no API key.

**Prerequisites:** Install Claude Code and sign in. Verify with
`claude --version`.

**Apply the example config:**

```bash
cp docs/examples/Settings.claude-cli.toml Settings.toml
```

**Model selection:**

`model` accepts any identifier the CLI supports via `--model` -- aliases
like `opus` or `sonnet`, or full names like `claude-opus-4-7`,
`claude-sonnet-4-6`.

Context window sizes:

- `claude-opus-4-7`: 1M tokens by default.
- `claude-sonnet-4-6` and `claude-opus-4-6`: 1M capable, but the CLI
  defaults to 200K. Append `[1m]` (e.g. `claude-sonnet-4-6[1m]`) to
  opt into the 1M variant.
- `claude-haiku-4-5`: 200K only, no 1M variant.
- Pre-thinking models (Claude 3.x) are rejected with HTTP 404.

**What you get:**

- No API key needed -- uses Claude Code's subscription auth.
- Stateless: spawns `claude --print --output-format json` per request.
  No tool access, no file access, no session reuse.
- Prompt caching handled automatically by Claude Code.
- `effort` controls the model's thinking budget. Opus 4.7 uses adaptive
  thinking; Sonnet 4.6 and Haiku 4.5 use extended thinking. If you need
  to pick the mode explicitly or disable thinking, use `provider = "claude"`
  (API) instead.
- `[ai.claude]` settings (`prompt_caching`, `thinking`, etc.) apply only
  to the API provider above -- they are ignored on this path.

**Note:** Each review may spawn many CLI processes. Lower
`review.concurrency` if you hit subscription rate limits.

## GitHub Copilot CLI

Uses a local
[GitHub Copilot CLI](https://docs.github.com/en/copilot/github-copilot-in-the-cli)
installation as the completion backend. This uses your GitHub Copilot
subscription -- no per-token charge, no API key.

**Prerequisites:**

- `copilot` CLI installed and on `$PATH`
- Authenticated session (run `copilot` once interactively to log in)

**Apply the example config:**

```bash
cp docs/examples/Settings.copilot-cli.toml Settings.toml
```

**What you get:**

- `model` follows GitHub Copilot's catalog (e.g. `claude-sonnet-4.5`,
  `gpt-5.5`); pick a model your subscription has access to.
- Sashiko invokes `copilot` with `--disable-builtin-mcps`,
  `--no-custom-instructions`, and `--allow-all-tools` so it acts as a
  pure text-completion-with-tools backend.
- The prompt is sent via stdin (not `-p`) to avoid Linux's
  `MAX_ARG_STRLEN` cap (~128 KB per argv element).
- `max_interactions` controls tool-call rounds before aborting.

**Note:** Each review may spawn many `copilot` processes. Lower
`review.concurrency` if you hit subscription rate limits.

## AWS Bedrock

Uses AWS Bedrock via the Converse API. Works with any Bedrock-hosted
model (Claude, Llama, Mistral, etc.).

**Prerequisites:** Enable model access in the
[AWS Bedrock console](https://console.aws.amazon.com/bedrock/) for your
desired model and region.

**Set AWS credentials** using any standard method:

```bash
# Option 1: Environment variables
export AWS_ACCESS_KEY_ID="..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_REGION="us-east-1"

# Option 2: AWS CLI profile (~/.aws/credentials)
aws configure
```

**Apply the example config:**

```bash
cp docs/examples/Settings.claude-bedrock.toml Settings.toml
```

**What you get:**

- Converse API -- works with any Bedrock-hosted model
- No API key needed -- uses standard AWS IAM authentication
- Cross-region inference profiles (e.g. `us.anthropic.claude-*`)
- Full tool/function calling support

## Google Cloud Vertex AI

Uses Claude models (and potentially others) via Google Cloud
infrastructure. Requires building with `--features vertex`.

**Prerequisites:** Enable the Vertex AI API and model access in the
[Vertex AI Model Garden](https://cloud.google.com/model-garden).

**Authenticate:**

```bash
gcloud auth application-default login
```

**Set project and region:**

```bash
export ANTHROPIC_VERTEX_PROJECT_ID="my-gcp-project"
export CLOUD_ML_REGION="us-east5"  # "global" endpoint not currently supported
```

**Apply the example config:**

```bash
cp docs/examples/Settings.claude-vertex.toml Settings.toml
```

**What you get:**

- No API key needed -- uses Google Cloud Application Default Credentials
- Global, multi-region, and regional endpoint support
- 1M context window for Claude Opus 4.7/4.6 and Sonnet 4.6 on Vertex
- Full tool/function calling and prompt caching support

## Kiro CLI

Uses the local `kiro-cli` as a completion backend.

**Prerequisites:** Install `kiro-cli` and authenticate with
`KIRO_API_KEY` or a browser login.

**Apply the example config:**

```bash
cp docs/examples/Settings.kiro-cli.toml Settings.toml
```

**What you get:**

- Runs `kiro-cli acp` as a stateless completion backend
- Kiro native tools are disabled by default; Sashiko's own tool protocol
  is used instead
- An isolated temporary agent with a deny-all hook prevents accidental
  tool execution

## Codex CLI

Uses a local [Codex CLI](https://github.com/openai/codex) (OpenAI)
installation as the completion backend. This uses your Codex
subscription -- no per-token charge, no API key.

**Prerequisites:** Install the `codex` CLI and authenticate.

**Apply the example config:**

```bash
cp docs/examples/Settings.codex-cli.toml Settings.toml
```

**What you get:**

- Runs `codex exec --json --sandbox read-only` as a stateless backend
- Prompt sent via stdin to avoid `ARG_MAX` issues
- No tool access -- sandbox is read-only

#### Devin CLI Setup

Sashiko can use a local [Devin for Terminal](https://cli.devin.ai/) install as
a completion backend. This path uses your Devin subscription, so there is no
per-token API charge and no API key to configure.

**Prerequisites**:
- `devin` CLI installed and on `$PATH`
- Authenticated session (run `devin auth login` once)

**Update Settings.toml**:
Copy `examples/Settings.devin-cli.toml` to your `Settings.toml` and adjust as needed.

**Notes**:
- `model` accepts any identifier `devin --model` accepts (e.g. `opus`,
  `swe`, `gpt`, `codex`). Leave it empty in `[ai]` to use the Devin default.
- Sashiko invokes `devin --print --prompt-file <tmp>`
  per request. The prompt is written to a temp file which is passed to `devin`.
- Each review may spawn many `devin` processes. Lower `review.concurrency`
  if you hit subscription rate limits.

## OpenAI-Compatible Providers

Sashiko includes an OpenAI-compatible provider for endpoints that
implement the OpenAI chat completions API.

**Apply the example config:**

```bash
cp docs/examples/Settings.openai-compat.toml Settings.toml
```

Adjust `base_url` to point to your provider's endpoint.

This provider uses Chat Completions. For OpenAI reasoning models and Azure AI
Foundry project endpoints, use the Responses provider below.

## OpenAI Responses API and Azure AI Foundry

Use `provider = "openai-responses"` for OpenAI's Responses API. It accepts
both the public OpenAI `/v1` root and Azure AI Foundry project URLs ending in
`/openai/v1`; Sashiko appends `/responses` automatically. On Azure, `model`
is the deployment name. Query parameters are preserved, so deployments that
require the preview API can use a URL such as
`.../openai/v1?api-version=preview`.

Azure API Management gateway endpoints may instead use a complete URL such as
`https://RESOURCE.azure-api.net/openai/responses?api-version=VERSION`. For
`.azure-api.net` hosts, Sashiko sends `OPENAI_API_KEY` in Azure's `api-key`
header; OpenAI and Azure AI Foundry v1 endpoints continue to use bearer auth.

```bash
export OPENAI_API_KEY="your-api-key"
cp docs/examples/Settings.azure-openai-responses.toml Settings.toml
```

The provider uses bearer authentication, requests encrypted reasoning content,
and replays that opaque item on later tool-call turns. Requests are stateless
(`store = false`), so one provider instance can safely serve concurrent review
stages. Sampling temperature is intentionally omitted because GPT reasoning
models may reject it. Azure/OpenAI prompt caching is automatic; cached input
tokens are reported in usage and the prefix-cache warmup path is enabled.
The default output limit is 16384 because Responses counts hidden reasoning
against `max_output_tokens`; set a larger deployment-supported value for
especially reasoning-heavy reviews. Reasoning-effort values are also
deployment-specific.

To run Azure GPT as a secondary model while Bedrock remains primary, add this
to the existing configuration:

```toml
[[ai.additional_models]]
name = "azure-gpt"
probability = 1.0
provider = "openai-responses"
model = "gpt-5.6"
max_input_tokens = 200000

[ai.additional_models.openai_compat]
base_url = "https://YOUR-RESOURCE.services.ai.azure.com/api/projects/YOUR-PROJECT/openai/v1"
context_window_size = 400000
max_tokens = 16384
reasoning_effort = "high"
```

`probability = 1.0` runs the secondary model on every review; lower values
sample it per patch review. Keep the API key in `OPENAI_API_KEY`, not in
`Settings.toml`.
