# Role
You're an expert Software Engineer with deep knowledge of Rust, Distributed Systems, Operating Systems and practical experience with infrastructure projects.

# Generic guidance
- You MUST commit changes to it after implementing each task or more often if it makes sense. Try to commit as often as possible. Every consistent and self-sufficient change must be committed.
- Sign all commits using the user's git configuration. Every commit **MUST** include a `Signed-off-by` line (e.g., using `git commit -s` which automatically uses the user's `user.name` and `user.email`). **NO EXCEPTIONS.** Do not use "Gemini CLI" or any other default unless explicitly configured in git.
- Make sure no lines in the commit message exceed 72 characters. Hard-wrap the commit message body to enforce this length.
- **Never** use backticks to quote any code, functions and variables names, etc. in the commit message.
- **Never** include metadata tags like `TAG` or `CONV` in commit messages. Only include standard git trailers (like `Signed-off-by`).
- After each change if it touches the Rust code make sure the code compiles and all tests pass. Never start a new task with non-clean git status. Clear the context between tasks.
- Make sure to not commit any logs or temporary files. NEVER commit before running `make check-pr` to ensure CI/CD checks pass.
- Once the task is done, no local changes should remain. Amend them to the previous commit, if it makes sense, make a standalone commit or get rid of them.
- Each commit should implement one consistent and self-sufficient change. Never create commits like "do X and Y", create 2 commits instead.
- For any non-trivial feature create a design document first, then review it and then implement it step by step.
- If not sure, ask the user, don't proceed without confidence. Also ask for confirmation for any high-level architecture decisions, propose options if applicable.
- Before starting any test or running the main binary, ensure no other `sashiko` processes are running to avoid port conflicts or database locking issues.

# Development Workflow

## 1. Prerequisites
Install the following tools to manage the development lifecycle:
- **make:** Command runner for project tasks. (Usually pre-installed or available via `build-essential` on Debian/Ubuntu).
- **yamllint:** Linter for YAML files. [Installation Guide](https://github.com/adrienverge/yamllint#installation)

## 2. Common Commands
Use `make` to run common development tasks:
- `make lint`: Run all linters (clippy, fmt, yamllint).
- `make test`: Run unit tests.
- `make integration-test`: Run the full integration smoke tests (starts server, runs benchmark, cleans up).
- `make sob`: Validate Signed-off-by tags for a commit range.
- `make check-pr`: Run all checks required for a Pull Request (`sob`, `lint`, `test`).
- `make check-all`: Run the complete suite including integration tests and database invariants (`check-pr`, `check-integration`, `check-db-invariants`).
- `make check-db-invariants`: Run lightweight database invariant checks.

# Rust Coding Standards

## 1. Idiomatic Rust

- **Version:** Make sure the code can be compiled with Rust 1.90, don't use unstable new features.
- **Safety First:** Prioritize safe Rust. Only use `unsafe` blocks when absolutely necessary and document the safety invariant clearly.
- **Error Handling:** Use `Result<T, E>` for recoverable errors. Avoid `.unwrap()` and `.expect()` in production code unless you can statically prove it will never panic (and document why). Prefer `?` operator for error propagation.
- **Ownership & Borrowing:** Leverage the borrow checker. Prefer borrowing (`&T`, `&mut T`) over cloning (`.clone()`) unless necessary for ownership transfer.
- **Iterators:** Use iterator chains (`map`, `filter`, `fold`, etc.) over explicit `for` loops where it increases clarity and conciseness.
- **Clippy:** Ensure code passes `cargo clippy`. Respect its suggestions.
- **Formatting:** Code must be formatted with `rustfmt` (`cargo fmt`).

## 2. Complexity & Structure

- **Cyclomatic Complexity:** Keep cyclomatic complexity low (target < 15). If a function has too many branches or loops, refactor it.
- **Function Length:** Avoid excessively long functions. A function should ideally fit on a single screen (soft limit of ~50 lines) or focus on a single responsibility. Break down large functions into smaller helper functions.
- **Modules:** Use the module system effectively to organize code logically. Keep public APIs clean and minimal.

## 3. Comments

- **Statements, Not Questions:** Comments should explain *why* something is done or clarify complex logic. They must be declarative statements.
  - **Bad:** `// Should we check for null here?`
  - **Good:** `// Check for null to prevent panic during initialization.`
- **Doc Comments:** Use `///` for documentation comments on public items. Include examples where helpful.

## 4. Code Reuse (DRY)

- **Aggressive Reuse:** Do not duplicate code. If logic appears in multiple places, extract it into a shared function, struct, or trait.
- **Generic Programming:** Use generics and traits to write flexible, reusable code rather than duplicating logic for different types.
- **Libraries:** leverage standard library and existing crate dependencies before writing custom implementations.

## 5. Testing

- **Unit Tests:** Write unit tests for new logic, ideally in the same file within a `tests` module.
- **Integration Tests:** Use `tests/` directory for integration tests that test the public API.

## 6. Asynchronous Code

- **Async/Await:** Use idiomatic `async`/`await` patterns. Be mindful of blocking operations in async contexts; use `tokio::task::spawn_blocking` if necessary.

# Project Map

## Core Application (`src/`)
- `main.rs`: Application entry point (server).
- `bin/`: CLI and utility binaries (`sashiko-cli.rs`, `benchmark.rs`).
- `lib.rs`: Shared library code.
- `worker/`: Background worker implementations (Review, Security, AI).
- `workflow/`: The core state-machine workflow engine.
- `toolbox/`: Tooling and capabilities for agents.
- `ai/`: Artificial Intelligence integration logic.
- `ingestor.rs`: Ingests patches/emails.
- `fetcher.rs`: Fetches emails/threads (e.g., from lore.kernel.org).
- `reviewer.rs`: Logic for reviewing patches.
- `local_review.rs`: Logic for `sashiko-cli local` executing reviews locally.
- `git_ops.rs`: Git operations wrapper.
- `nntp.rs`: NNTP protocol handling.
- `patch.rs`: Patch parsing and manipulation.
- `forge.rs`: Webhook integration and parsing for external forges (GitHub, GitLab).
- `email_router.rs` & `email_policy.rs`: Email routing and policy enforcement.
- `db.rs`: Database interactions.
- `api.rs`: API endpoints.
- `settings.rs`: Application settings management.
- `events.rs`: Event handling system.
- `baseline.rs`: Baseline detection logic.

## Configuration & Assets
- `Settings.toml`: Main application configuration.
- `email_policy.toml`: Email policy configuration.
- `third_party/prompts/`: Markdown templates/prompts for AI reviews.
- `skills/`: Agent skills directory.
- `static/`: Web assets (HTML, images).

## Data & External
- `third_party/linux/`: Linux kernel source tree (reference/analysis).
- `archives/`: Storage for mailing list archives.
- `review_trees/`: Git worktrees used during the review process.

## Documentation
- `designs/`: Architecture and design documents.


# Benchmarking

**CRITICAL RULE:** Never run the full (`benchmark.json`) or small (`benchmark_small.json`) benchmarks without an explicit human request.

To evaluate the AI's review performance against a set of known issues, follow this workflow:

1.  **Prepare the environment:**
    Stop any currently running sashiko processes, then move or drop the existing database to start with a clean state.
    ```bash
    mv sashiko.db sashiko.db.bak
    ```

2.  **Start the Server:**
    The benchmark tool submits code for review via the REST API, so the main server must be running. In a separate terminal, start the server:
    ```bash
    cargo run --bin sashiko
    ```

3.  **Run the benchmark tool:**
    Use the unified `benchmark` tool with a benchmark JSON file (e.g., `benchmark_small.json`). This tool will automatically ingest the patches via the API, wait for all AI review processes to complete in the background, and then dynamically evaluate the generated findings against ground-truth descriptions.
    ```bash
    cargo run --bin benchmark -- --file benchmarks/benchmark_small.json
    ```

    *   A summary of detection rates (Detected, Missed, Partially Detected) along with performance metrics (Average Tokens In/Out, Average Turns, Average Time) and counts of total concerns and findings will be printed to the console upon completion.
    *   Detailed evaluation results are written to `benchmark_results.json` in the current working directory, which contains explanations from the AI judge for each finding.

## Available Benchmark Suites

When running the benchmark tool, you can select from several suites in the `benchmarks/` directory:

*   **`benchmark.json`**: The complete benchmark suite (999 entries) for comprehensive testing.
*   **`benchmark_small.json`**: A smaller representative subset (99 entries) for standard testing.
*   **`benchmark_tiny.json`**: A very brief subset (9 entries) for quick iteration.
*   **`benchmark_smoke.json`**: Extremely small (3 entries) smoke test.
*   **`benchmark_preexisting.json`**: **IMPORTANT** - Used to test the kernel bug framework using known, existing kernel bugs and their original patches.


# LLM Workflow Design

When designing and implementing new workflows and stages in Sashiko, adhere to the following principles to ensure our code remains robust, efficient, resilient to AI hallucinations, and exceptionally clear for both the LLM and future engineers.

## 1. Stage Design & Data Flow
- **Single Responsibility:** Preferably, each stage should focus on solving a single, well-defined problem.
- **Minimal But Sufficient Data:** Stages must receive *only* the specific information required to complete their task, but not less. **You must verify that the task is actually solvable using only the data and tools provided to the LLM.** (Rule of thumb: if a human couldn't confidently solve it with just that context, the LLM won't be able to either).
- **Diverge & Converge (Map-Reduce):** For broad complex analyses, do not rely on one massive mega-prompt. Split the workload into specialized, parallel "expert" stages (mapping), followed by a single "Consolidation" stage (reducing) to merge, deduplicate, and verify the independent results.
- **Negative Data Tracking:** When an LLM investigates a potential issue and determines it is *not* a bug, have it explicitly output that as a "dismissed concern." This explains *why* something isn't a problem, preventing future stages or humans from having to re-verify things the LLM already checked.
- **Early Exits (Short-Circuiting):** Defensively bail out of workflows as soon as further processing is unnecessary (e.g., if finding arrays are empty after a stage). This saves execution time and tokens, and prevents the LLM from trying to "force" an output when there's nothing to report.
- **Lean, Consumable Outputs:** Stages should produce minimal outputs, and those outputs should generally be consumed in full by follow-up stages in the pipeline.
- **When to Combine Tasks:** The *only* valid reason to combine multiple tasks into a single stage is to optimize token usage and latency. If answering two or more questions requires reasoning over a highly overlapping set of context or steps, it is reasonable to group them to avoid duplicated effort.
- **Anti-Patterns:**
  - **The Kitchen Sink:** Throwing all available unstructured data into a stage "just in case" the LLM needs it.
  - **Dead Outputs:** Generating intermediate fields or outputs that no subsequent stage or user actually consumes.

## 2. Prompt Engineering & Schema Design
- **Unambiguous Naming:** Every field in your input and output JSON schemas must have the best possible name to eliminate ambiguity. There must be only one single, reasonable interpretation of what a field expects.
- **Shared Vocabulary:** Prompts must use consistent language across all stages and closely match the exact naming conventions present in the input and output schemas.
- **The "Escape Hatch" (Avoid Rigid Classification):** Never force the LLM to choose between a fixed number of options if those options do not definitively cover all possible real-world scenarios. Always leave an escape path in your Enums or classifiers (e.g., `"Other"`, `"Unknown"`, `"Not Applicable"`). If you box an LLM into a corner, it will hallucinate an incorrect classification.
- **Precision over Prose:** Avoid ambiguity in prompts as much as possible. Instructions must be explicit, direct, and leave no room for creative misinterpretation by the LLM.
- **"Anti-Charity" Directives:** When analyzing code for defects, explicitly instruct the LLM not to give code "the benefit of the doubt". If a stage dismisses an issue as a false positive, it must be required to cite *concrete code* that proves it is safe, rather than assuming a surrounding system handles it perfectly.

## 3. Resilient & Idiomatic Rust
- **Type-Driven State:** Write idiomatic, robust Rust code. **Never** rely on raw text/string values to represent application state. Heavily leverage Rust's type system (e.g., `enums`, well-defined states, and traits) so that invalid states are unrepresentable and caught at compile time.
- **Idempotency & Safe Retries:** Workflows must be designed to expect transient LLM failures (e.g., malformed JSON). Stages should be idempotent, allowing the Rust orchestrator to safely retry a failed step without duplicating external side-effects.
- **Custom Validators & LLM Feedback:** When structural validation fails, do not just fail the pipeline. Use customized validation functions that construct specific, readable error strings to feed back into the LLM context. Tell the model *exactly* which formatting rule it violated so the retry mechanism succeeds.

## 4. Observability
- **Comprehensive Logging:** The framework must log *all* interactions with the LLM.
- **Full Context Capture:** Every input sent to the LLM and every output string returned must be recorded in full to ensure end-to-end traceability and debugging.

