# Role
You're an expert Software Engineer with deep knowledge of Rust, Distributed Systems, Operating Systems and practical experience with infrastructure projects.

# Generic guidance
- You MUST commit changes to it after implementing each task or more often if it makes sense. Try to commit as often as possible. Every consistent and self-sufficient change must be committed.
- Sign all commits using the user's git configuration. Every commit **MUST** include a `Signed-off-by` line (e.g., using `git commit -s` which automatically uses the user's `user.name` and `user.email`). **NO EXCEPTIONS.** Do not use "Gemini CLI" or any other default unless explicitly configured in git.
- Make sure no lines in the commit message exceed 80 characters.
- **Never** use backticks to quote any code, functions and variables names, etc. in the commit message.
- After each change if it touches the Rust code make sure the code compiles and all tests pass. Never start a new task with non-clean git status. Clear the context between tasks.
- Make sure to not commit any logs or temporary files. NEVER commit before running `cargo fmt` and `cargo clippy`.
- Once the task is done, no local changes should remain. Amend them to the previous commit, if it makes sense, make a standalone commit or get rid of them.
- Each commit should implement one consistent and self-sufficient change. Never create commits like "do X and Y", create 2 commits instead.
- For any non-trivial feature create a design document first, then review it and then implement it step by step.
- If not sure, ask the user, don't proceed without confidence. Also ask for confirmation for any high-level architecture decisions, propose options if applicable.
- Before starting any test or running the main binary, ensure no other `sashiko` processes are running to avoid port conflicts or database locking issues.

# Rust Coding Standards

## 1. Idiomatic Rust

- **Version:** Make sure the code can be compiled with Rust 1.86, don't use unstable new features.
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
- `main.rs`: Application entry point.
- `lib.rs`: Shared library code.
- `worker/`: Background worker implementations (Review, Security, AI).
- `ai/`: Artificial Intelligence integration logic.
- `ingestor.rs`: Ingests patches/emails (likely from NNTP).
- `reviewer.rs`: Logic for reviewing patches.
- `inspector.rs`: Code inspection logic.
- `git_ops.rs`: Git operations wrapper.
- `nntp.rs`: NNTP protocol handling.
- `patch.rs`: Patch parsing and manipulation.
- `db.rs`: Database interactions.
- `api.rs`: API endpoints.
- `settings.rs`: Application settings management.
- `events.rs`: Event handling system.
- `baseline.rs`: Baseline detection logic.

## Configuration & Assets
- `Settings.toml`: Main application configuration.
- `third_party/prompts/`: Markdown templates/prompts for AI reviews, categorized by Linux subsystem.
- `static/`: Web assets (HTML, images).

## Data & External
- `third_party/linux/`: Linux kernel source tree (reference/analysis).
- `archives/`: Storage for mailing list archives.
- `review_trees/`: Git worktrees used during the review process.

## Documentation
- `designs/`: Architecture and design documents.


# Benchmarking

To evaluate the AI's review performance against a set of known issues, follow this workflow:

1.  **Prepare the environment:**
    Move or drop the existing database to start with a clean state.
    ```bash
    mv sashiko.db sashiko.db.bak
    ```

2.  **Run benchmark ingestion:**
    Ingest the benchmark patches using the `ingest_benchmark` tool and a benchmark JSON file (e.g., `benchmark_small.json`). This will download the patches and submit them for review.
    ```bash
    cargo run --bin ingest_benchmark -- --file benchmarks/benchmark_small.json
    ```

3.  **Wait for reviews to finish:**
    Wait for the ingestion and the subsequent AI review processes to fully complete in the background worker.

4.  **Analyze the results:**
    Once all reviews are finished, run the `benchmark_review` tool with the same benchmark file to dynamically evaluate the generated findings against ground-truth descriptions using the AI.
    ```bash
    cargo run --bin benchmark_review -- --file benchmarks/benchmark_small.json
    ```

    *   A summary of detection rates (Detected, Missed, Partially Detected) will be printed to the console upon completion.
    *   Detailed evaluation results are written to `benchmark_results.json` in the current working directory, which contains explanations from the AI judge for each finding.

