# Structured Inline Review Design

## 1. Motivation
Currently, the review worker (specifically Stage 9) produces a single, large Markdown text blob for its inline review (`review_inline`). While this format is easily readable by humans on mailing lists, it is extremely difficult to parse programmatically for injection into code review platforms like Gerrit, which require strict `(file, start_line, end_line, message)` mappings.

Furthermore, LLMs are notoriously bad at outputting accurate line numbers. However, they are excellent at identifying the *exact string snippets* of code that contain issues. We have also observed the LLM commenting on bugs in code that was *not* touched by the current patch, which creates noise for developers.

## 2. Objective
Transition the Stage 9 review generation to output a structured format (JSON) containing file paths, code snippets, and review messages. 
The backend must then:
1. Resolve these snippets to exact line numbers in the target files.
2. Validate that the resolved lines intersect with the actual lines modified by the patch.
3. Provide a feedback loop (retry mechanism) if the LLM hallucinates snippets or comments on unmodified code.

We must maintain **backward compatibility** (Alternative A), allowing users to toggle between the legacy "text" blob format and the new "structured" format.

## 3. Architecture & Approach
We will use the **"JSON Retry Loop"** approach combined with **Config-driven Prompting (Alternative A)**.

### 3.1 Backwards Compatibility (Alternative A)
- We will add a configuration parameter `inline_review_style` (values: `"text"` or `"structured"`) to `Settings.toml`, parsed in `src/settings.rs` and passed down to `WorkerConfig`.
- In `src/worker/prompts.rs`, during Stage 9, we will branch based on this config:
  - **If `"text"`:** Load `inline-template.md` and append instructions to return raw text. (Legacy behavior).
  - **If `"structured"`:** Load a new template `inline-template-structured.md` and append instructions to return a JSON array matching our specific schema.

### 3.2 The Target JSON Schema
When running in "structured" mode, Stage 9 will instruct the LLM to output a JSON array of objects:
```json
[
  {
    "file": "src/main.c",
    "compromised_line": "    if (ptr == NULL) {\n        return -1;\n    }",
    "approx_line": 45,
    "issue": "This return leaks the lock acquired on line 40."
  }
]
```
with approx_line being optional

### 3.3 Snippet Resolution & Validation
We will implement a new validation pipeline (likely in a new module or within `src/worker/prompts.rs`):
1. **Fuzzy Snippet Matching:** A function that takes the `compromised_line` (string snippet), target `file`, and `approx_line`. It splits the snippet by newlines, strips whitespace, removes "-" and "+" that could come from the diff, and searches the actual file content near `approx_line` to find the exact `start_line` and `end_line`.
2. **Diff Intersection:** Using the existing `parse_diff_ranges()` from `src/worker/prefetch.rs`, we check if the resolved `start_line..end_line` overlaps with the 0-based lines touched by the patch hunks.

### 3.4 The Retry Loop
Inside `Worker::run` for Stage 9:
1. The LLM generates the JSON.
2. We parse the JSON and run the Snippet Resolution & Validation.
3. **If all issues are valid:** We store the structured array in the final output and break the loop.
4. **If there are errors (e.g., snippet not found, or snippet outside the diff):** We construct a feedback string detailing which issues failed and why (e.g., "Issue in src/main.c: lines 50-52 were not modified by this patch. Discard this issue."). We append this string as an `AiRole::User` message to the `local_history` and trigger another generation, letting the LLM correct its output.

## 4. Required File Changes

1. **`Settings.toml` & `src/settings.rs`**
   - Add `inline_review_style` (defaulting to `"text"` for now, or `"structured"` depending on preference).
2. **`third_party/prompts/inline-template-structured.md` (New File)**
   - Create a prompt that explains the required JSON schema.
3. **`src/worker/prompts.rs`**
   - Update `WorkerConfig` and `Worker` struct to hold `inline_review_style`.
   - Update `get_stage_prompt(9)` to return different templates based on the style.
   - Refactor the Stage 9 execution block in `Worker::run` to handle the JSON parsing, validation, and feedback retry loop.
4. **`src/worker/review_validator.rs` (Suggested New File) or inside `prompts.rs`**
   - Implement `resolve_and_validate_snippets(json_array, worktree_path, diff_ranges) -> Result<Vec<ResolvedIssue>, Vec<String>>`.
   - Ensure it handles whitespace-agnostic line matching.
5. **`src/worker/prefetch.rs`**
   - Ensure `parse_diff_ranges` is accessible and clearly mapped to the validation logic.
6. **`src/bin/review.rs`**
   - Update the final JSON output formatting to include the structured data if generated, and ensure it passes settings down to `WorkerConfig`.
