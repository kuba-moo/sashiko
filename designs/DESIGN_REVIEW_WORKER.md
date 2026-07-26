# Design: Sashiko Review Worker

## Goal
Implement an automated, provider-neutral AI worker (`sashiko-review`) that
reviews Linux kernel patchsets as a staged maintainer workflow. It requires
read-only access to the repository so stages can inspect history and source
context before producing a structured verdict.

## Architecture

### 1. Binary: `sashiko-review` (`src/bin/review.rs`)
A worker entry point that reads a serialized review request from standard input
and delegates repository preparation and review execution to `local_review`.

Its command-line options override request or configuration values, including
`--baseline`, `--repo`, `--worktree-dir`, `--prompts`,
`--review-patch-index`, `--review-commit`, `--no-ai`, `--reuse-worktree`,
`--ai-provider`, and `--custom-prompt`. `--json` remains as a deprecated
compatibility flag; JSON input is always expected.

### 2. Core Components

#### A. Review orchestration (`src/local_review.rs`, `src/worker/`)
The review is a staged pipeline rather than one provider-specific conversation.
`local_review` prepares the repository, providers, shared tools, token budget,
and conversation dump sink. Stage implementations build scoped prompts and use
the provider-neutral `SessionRunner` for the model/tool loop and validation.

Stages 1-7 investigate distinct classes of defects and run with deliberately
limited scopes. Later stages merge concerns, discard unsupported candidates,
and produce the findings and review text. Shared review-token totals remain
correct when investigation stages execute concurrently.

#### B. AI providers and sessions (`src/ai/`)
Providers translate the common request, response, tool-call, usage, and
reasoning-block representations to their native APIs. `SessionRunner` owns the
multi-turn loop, tool dispatch, validation feedback, optional transcript dumps,
and optional budget steering. This keeps worker stages independent of Gemini,
Bedrock, Claude, or other provider protocols.

#### C. `ToolBox` (`src/toolbox/`)
Provides a safe, read-only interface to the system.
- **Git Tools**:
    - `git_show(ref, path)`: Read file content at specific revision.
    - `git_diff(range)`: Get patch/diff content.
    - `git_blame(path, start_line, end_line)`: Check authorship context.
    - `search_file_content(pattern, path, context_lines)`: Search for symbols or patterns.
- **Analysis Tools**:
    - `read_files(files, mode)`: Read one or more files or line ranges.
    - `list_dir(path)`: Explore directory structure.
- **Worker Tools**:
    - `read_prompt(name)`: Read specific guideline from `review-prompts/`.
- **Safety**: Strictly validates paths to ensure they stay within the repo or submodule.

#### E. State Management
- **Conversation History**: Full history of turns, tool calls, and results.
- **Worktree**: A dedicated `git worktree` where the patch is applied for analysis.
- **Budget State**: Per-stage counters plus atomic totals shared by the review.
- **Provenance**: Every initial concern records its source stage. Merge and
  filtering stages preserve the sorted, unique `source_stages` set through to
  each finding.
- **Deduplication Metrics**: The application computes total concerns, unique
  findings, and multi-stage findings from the pipeline data. These values are
  not trusted from model-authored summary fields; they are persisted with the
  review and surfaced by the API and UI.


#### D. `PromptRegistry` (`src/worker/prompts.rs`)
-   **Dynamic Loading**: Instead of static prompts, implements the logic defined in `DESIGN_REVIEW_PROMPTS.md`.
-   **Responsibility**:
    -   Scans the external `review-prompts` repository.
    -   Matches Patchset file paths to specific subsystem/language prompt files.
    -   Constructs the full System Prompt and Context block.
    -   Provides tools for the Worker to browse these rules (`read_prompt`, `list_guidelines`).
-   **Config**: Requires `--prompts <PATH>` argument.

### 3. Data Flow

1.  **Trigger**: The reviewer launches `sashiko-review` and writes a JSON
    review request to its standard input.
2.  **Context Loading**:
    -   Parse patchset, patch, provider, and repository data from the request.
    -   Resolve the configured baseline and prepare or reuse a worktree.
    -   Apply the requested patches and construct per-patch review context.
3.  **Analysis Pipeline**:
    -   Pre-screen and planning establish the relevant scope.
    -   Investigation stages use the shared toolbox to gather evidence and emit
        concerns tagged with their source stage.
    -   Consolidation merges duplicate concerns while unioning provenance.
    -   Validation stages retain supported concerns and produce findings.
    -   The application derives deduplication statistics and budget flags.
4.  **Result Handling**:
    -   The worker writes its structured result to standard output.
    -   The parent reviewer persists review output, findings, provenance,
        budget flags, and AI interaction usage.

## Execution Plan

See the CLI output for the step-by-step execution plan.
