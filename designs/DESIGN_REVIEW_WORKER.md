# Design: Sashiko Review Worker

## Goal
Implement an automated AI worker (`sashiko-review`) that uses **Gemini 3 Pro** to review Linux kernel patchsets. The worker should emulate a maintainer's review process, leveraging the `masoncl/review-prompts` philosophy. It requires read-only access to the git repository to inspect context (blame, history, file content) before delivering a verdict.

## Architecture

### 1. Binary: `sashiko-review` (`src/bin/review.rs`)
A standalone CLI tool that interfaces with the existing `sashiko` database and the local git repository.

**Arguments:**
- `--patchset <ID>`: Database ID of the patchset to review.
- `--model <NAME>`: Defaults to `gemini-1.5-pro-latest` (or `gemini-3-pro` if available).
- `--dry-run`: Output review to stdout instead of saving to DB.

### 2. Core Components

#### A. `Worker` (`src/worker.rs`)
The central orchestrator.
- **Responsibility**: Manages the conversation loop with the LLM.
- **State**: Holds conversation history (Messages), available Tools, and the DB connection.
- **Logic**:
    1.  Initializes with a System Prompt (Persona: Linux Kernel Maintainer).
    2.  Ingests the Patchset (Cover letter + Patches).
    3.  Sends request to Gemini.
    4.  If Gemini requests a tool (e.g., `git_blame`), executes it and feeds the result back.
    5.  Repeats until a final text response is received.
    6.  Parses the response into a structured Review object.

#### B. `GeminiClient` (`src/ai/gemini.rs`)
A specialized client for the Google Generative AI API.
- **Contrast to existing `AiProvider`**: The current trait is too simple (single turn, no tools). This client will support:
    -   Multi-turn chat (`contents`).
    -   Function Calling (`tools`, `function_declarations`).
    -   Streaming (optional, but good for long reviews).

#### C. `ToolBox` (`src/worker/tools.rs`)
Provides a safe, read-only interface to the system.
- **Git Tools**:
    - `git_show(ref, path)`: Read file content at specific revision.
    - `git_diff(range)`: Get patch/diff content.
    - `git_blame(path, start_line, end_line)`: Check authorship context.
    - `git_grep(pattern, path_glob)`: Search for symbols or patterns.
- **Analysis Tools**:
    - `read_file_lines(path, start, end)`: Read specific line range.
    - `list_dir(path)`: Explore directory structure.
- **Worker Tools**:
    - `todo_write(task, status)`: Help worker track progress as required by prompts.
    - `read_prompt(name)`: Read specific guideline from `review-prompts/`.
- **Safety**: Strictly validates paths to ensure they stay within the repo or submodule.

#### E. State Management
- **Conversation History**: Full history of turns, tool calls, and results.
- **Todo List**: Internal tracker for "TodoWrite" requests from prompts (e.g., Task 1-6 for each category).
- **Worktree**: A dedicated `git worktree` where the patch is applied for analysis.


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
3.  **Analysis Loop**:
    -   Worker constructs prompt: "Review patchset: [Subject]..."
    -   **Gemini**: "I need to see `net/core/dev.c` lines 100-150 to check locking."
    -   **Worker**: Runs `read_file` -> Returns content.
    -   **Gemini**: "Checks out. But who touched this last? `git blame` please."
    -   **Worker**: Runs `git blame` -> Returns result.
    -   **Gemini**: "Looks good. Reviewed-by: Gemini <...>"
4.  **Storage**:
    -   Save output to `reviews` table.
    -   Save token usage to `ai_interactions` table.

## Execution Plan

See the CLI output for the step-by-step execution plan.
