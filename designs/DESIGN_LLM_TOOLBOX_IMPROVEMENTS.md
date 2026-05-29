# Design: LLM Toolbox Improvements for Truncation and Efficiency

## 1. Objective
Improve Sashiko's LLM review workflow by explicitly instructing the LLM to detect and handle truncated tool outputs (Eliminating Silent Truncations & LLM Confusion).

## 2. Proposed Changes

### 2.1. Truncation Awareness Guidelines
We will modify `build_context` in `src/worker/prompts.rs` to append a new section to the global review guidelines. This section will explicitly instruct the LLM on how to handle truncated outputs from tools.

**New Guideline Text:**
```markdown
### TRUNCATION & PAGINATION MANAGEMENT
Many of your information-gathering tools (such as `git_read_files`, `git_diff`, `git_show`, `git_grep`, `git_log`) will truncate their output if it exceeds token limits to protect the context window.
When truncation occurs, the tool's JSON response will contain:
- `"truncated": true`
- A `"next_page_hint"` explaining how to fetch the next slice of data (usually by specifying `start_line` or narrowing the search).

You MUST actively check for the `"truncated"` flag in every tool response. If `"truncated"` is `true`, you MUST NOT assume you have the complete picture. You are REQUIRED to follow the `"next_page_hint"` and make subsequent tool calls with adjusted parameters (e.g., `start_line`, `end_line`, narrower `paths`, or specific regex) to fetch the remaining content before finalizing your analysis. Failing to retrieve truncated content is a failure of rigor.
```


## 3. Verification Plan

### 3.1. Compilation & Linting
Ensure the project compiles and passes all linters:
```bash
make lint
make test
```

### 3.2. Manual Verification of Prompt Integration
Verify that the new guidelines are correctly appended to the system context. We can do this by running a test or temporarily logging the built context.

### 3.3. Benchmark (Optional)
Run a small benchmark to ensure no regression in detection rates:
```bash
cargo run --bin benchmark -- --file benchmarks/benchmark_small.json
```
