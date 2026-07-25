# Design: Sashiko Review Worker

## Goal
Implement an automated AI worker (`sashiko-review`) that uses **Gemini 3 Pro** to review Linux kernel patchsets. The worker should emulate a maintainer's review process, leveraging the `masoncl/review-prompts` philosophy. It requires read-only access to the git repository to inspect context (blame, history, file content) before delivering a verdict.

## Architecture

### 1. Binary: `sashiko-review` (`src/bin/review.rs`)
A standalone CLI tool that interfaces with the existing `sashiko` database and the local git repository.

**Arguments:**
- `--patchset <ID>`: Database ID of the patchset to review.
- `--model <NAME>`: Defaults to the model configured in `Settings.toml`.
- `--dry-run`: Output review to stdout instead of saving to DB.

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

#### C. `ToolBox` (`src/worker/tools.rs`)
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

1.  **Trigger**: User runs `sashiko-review --patchset 123`.
2.  **Context Loading**:
    -   Fetch `Patchset(123)` from DB.
    -   Fetch associated `Patches` and `Messages`.
    -   Identify the base git repo/commit (using `baselines` table).
3.  **Analysis Pipeline**:
    -   Pre-screen and planning establish the relevant scope.
    -   Investigation stages use the shared toolbox to gather evidence and emit
        concerns tagged with their source stage.
    -   Consolidation merges duplicate concerns while unioning provenance.
    -   Validation stages retain supported concerns and produce findings.
    -   The application derives deduplication statistics and budget flags.
4.  **Storage**:
    -   Save output to `reviews` table.
    -   Save token usage to `ai_interactions` table.

## Execution Plan

See the CLI output for the step-by-step execution plan.
