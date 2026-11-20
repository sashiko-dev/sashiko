# Design: Plain-Text Toolbox Results and Fewer-Turn Git Flags

## 1. Motivation & Telemetry

Analysis of production review traces in `sashiko.db` identified two sources of avoidable token waste, citation friction, and multi-turn exploration loops in agentic review stages:

1. **JSON-escaped tool envelopes and unnumbered source reads:**
   - Every tool in `src/toolbox/` returned a `serde_json::Value::Object` serialized with `.to_string()` in `SessionRunner::run` (`src/ai/session.rs`).
   - Escaping C, Rust, and diff text (`\"`, `\\`, `\n`, `\t`) inflates token counts by ~12–18% and degrades readability.
   - `GitLogTool` (`"commits"` + `"output"`) and `GitFindFilesTool` (`"files"` + `"output"`) serialized identical data twice in every response (`2x` payload overhead).
   - `GitReadFilesTool` and blob `GitShowTool` returned raw lines without line numbers, requiring the LLM to mentally add offsets from `start_index` when citing line numbers.

2. **Multi-turn `git_grep` -> `git_read_files` and unscoped `git_log` loops:**
   - `GitLogTool` lacked path filtering (`-- <paths>`), line/symbol evolution (`-L`), pickaxe search (`-S`/`-G`), and compact `--oneline` output, forcing agents to scan global history or run repeated `git_blame` + `git_show` calls.
   - `GitGrepTool` only supported fixed line counts (`-C`), requiring follow-up `git_read_files` turns to inspect the enclosing C/Rust function around a match.

### Explicit Non-Goals (Measured Trade-offs)
- **No mid-history observation masking:** Production telemetry shows an **88.7% KV prefix-cache hit rate** (`6.56M / 7.40M` prompt tokens cached per review). Mutating earlier `AiRole::Tool` messages invalidates cached suffixes across turns and increases billed cost on Claude/OpenAI/Gemini. All improvements in this design are strictly append-only and cache-preserving.
- **No AST / `tree-sitter` navigation tools:** Kernel, QEMU, and GCC C codebases use heavy conditional compilation (`#ifdef` unbalanced braces) and function-defining macros (`SYSCALL_DEFINE*`, `FOLIO_FLAG`, locking attributes) that break AST symbol boundaries. Git's native `-L`, `-S`/`-G`, and `-W` flags operate without AST fragility.
- **No session-wide repeat-call stubbing or batch token caps:** Existing consecutive duplicate call blocking (`StageSession`) and per-file `Truncator` limits remain unchanged.

---

## 2. Plain-Text Tool Results with Line Numbers

### 2.1. Session & Provider Wire Translation
- **`SessionRunner::run` (`src/ai/session.rs`):**
  - Unwrap `serde_json::Value::String(s)` directly into `AiMessage.content` without JSON quotes or string escaping:
    ```rust
    let content = match result {
        Value::String(s) => s,
        other => other.to_string(),
    };
    ```
  - Structured error objects (`json!({"error": ...})`) from `StageSession` and `LlmSession::call_tools` continue to serialize via `other.to_string()`, preserving compatibility with session error handling.
- **Provider compatibility (`src/ai/gemini.rs` & `src/ai/vertex.rs`):**
  - Claude, OpenAI, Bedrock, Ollama, vLLM, and CLI providers already transmit `AiMessage.content` verbatim as text.
  - Gemini (`generateContent`, also delegated to by `VertexClient`) requires `FunctionResponse.response` to be a JSON Object (`google.protobuf.Struct`). Update `gemini.rs` so that when `AiRole::Tool` content is plain text (or parses to a non-Object JSON value), it wraps the string as `{"output": content}` instead of falling back to `{}`.

### 2.2. Per-Tool Plain-Text Formats (`src/toolbox/`)
All eight read-only git tools return `Ok(Value::String(...))`:

1. **`git_read_files` (`src/toolbox/git_read_files.rs`):**
   - Format each requested file with a clear section header:
     - Non-empty: `=== <path> (lines <start>-<end> of <total>) ===`
     - Empty file (`0` bytes): `=== <path> (empty, 0 lines) ===`
     - Per-file failure: `=== <path>: ERROR: <message> ===`
   - Prepend 1-based line numbers (`"{line_no}: {line}"`) **before** passing the slice to `Truncator::truncate_sequential(&numbered, 20_000)`. Prepending line numbers prior to truncation ensures `Truncator`'s byte budget (`20_000 * 3` bytes) accounts for line-number prefixes while `res.lines_kept` still equals the exact number of source lines kept.
   - When truncated by `Truncator` or `end_line < total_lines`, append:
     `[Truncated: lines <start>-<end> of <total> shown. Call git_read_files with start_line=<end + 1> to read more.]`
2. **`git_show` (`src/toolbox/git_show.rs`):**
   - Preserve the shared raw stdout cache entry under `"git_show_raw:{object}:{suppress_diff}:{paths:?}"` in `context.cache`.
   - For blob objects or line-ranged reads, format 1-based `<line_no>: <line>` lines with a header (`=== <object> (lines <start>-<end> of <total>) ===`) and `start_line=<end + 1>` pagination hint when truncated.
   - For commit objects without line ranges, return `Truncator::truncate_diff(&raw_str, 12_000)` output plus a path-filtering hint when truncated.
3. **`git_blame` (`src/toolbox/git_blame.rs`):**
   - Return `[<path> blame lines <start>-<end> of <total>]` followed by truncated blame output and `start_line=<start + returned_items>` continuation hint when truncated.
4. **`git_diff` (`src/toolbox/git_diff.rs`):**
   - Return `"No differences found."` when empty, or `Truncator::truncate_diff` output plus a path-narrowing hint when truncated.
5. **`git_log` (`src/toolbox/git_log.rs`):**
   - Eliminate the duplicate `"commits"` + `"output"` payload; return truncated log text (or `"No commits found."`) plus a range/max_count hint when truncated.
6. **`git_find_files` (`src/toolbox/git_find_files.rs`):**
   - Eliminate the duplicate `"files"` + `"output"` payload; return a summary line (`Found <total> matching files:` or `Found <total> matching files (showing first 1000):`) followed by newline-separated paths and a narrowing hint when truncated.
7. **`git_ls` (`src/toolbox/git_ls.rs`):**
   - Return compact directory entries (`<name>/` for directories, `<name>` for files) up to 1,000 items with a total/truncation footer, or `Error: ...` on `git ls-tree` failure.
8. **`git_grep` (`src/toolbox/git_grep.rs`):**
   - Return `"No matches found."` on exit code 1 with empty stderr, `"Error: git grep failed: ..."` on non-empty stderr, or formatted grouped grep output plus a narrowing hint when truncated.

### 2.3. Workflow Prompts & Web UI
- Update the 4 workflow system prompts (`linux_patch_review.rs`, `gcc_patch_review.rs`, `qemu_patch_review.rs`, `sashiko_patch_review.rs`) to remove the stale `('truncated': true)` JSON field reference.
- Update `formatJsonExpandable` / `formatJsonPretty` in `static/index.html` so plain-text `AiRole::Tool` messages render directly with real newlines instead of `JSON.stringify` quotes and `\n` escapes.

### 2.4. Downstream Snippet Normalization
- Because `git_read_files`, `git_show`, and `git_grep` prepend 1-based line numbers (`10: code`, `  10:code`, `  10-code`, `  10=code`), LLMs frequently copy numbered lines directly into `Evidence.snippet` (code-notes exploration/verification) or finding `locations[].code_snippet`.
- Provide `strip_tool_line_numbers(raw: &str) -> Option<String>` in `src/toolbox/utils.rs` requiring strictly increasing 1-based line numbers across non-empty, non-elision lines.
- Both `RevisionFiles::check` (`src/notes/evidence.rs`) and `resolve_single_location_two_sided` (`src/worker/location_resolver.rs`) match unstripped `raw` snippets first (preserving verbatim source lines such as assembly labels or tuple-struct fields) and fall back to `strip_tool_line_numbers` (plus `try_strip_diff_markers` for numbered diff lines like `25: -    old_call();`) when unstripped matching fails.

---

## 3. Fewer-Turn Git Flags

### 3.1. `GitLogTool` Extensions (`src/toolbox/git_log.rs`)
Add four optional parameters with identical defaults in `normalize_args` and `call`:

- **`paths`** (`array` of `string`, default `[]`):
  - Reject any path starting with `'-'` and validate each via `validate_path(p, &context.worktree_path)` (allowing historical deleted paths while blocking `..`, absolute paths, and symlink escapes).
  - Passed after `"--"`.
- **`line_range`** (`object`, optional):
  - Fields: `path` (required `string`), plus either `start_line` & `end_line` (`integer`, 1-based, `1 <= start_line <= end_line`) OR `symbol` (`string`).
  - Validate `path` via `!path.starts_with('-')` and `validate_path(path, &context.worktree_path)`.
  - Validate `symbol` contains only safe identifier characters (`[A-Za-z0-9_]+`) so `:` path separators or pathological regexes cannot be injected, emitting `-L:<symbol>:<path>` (documented for top-level C functions) or `-L<start>,<end>:<path>` (language-agnostic).
  - Git forbids combining `-L` with pathspecs (`-- <paths>`). When `line_range` is set: if `paths` is empty or contains only `line_range.path`, omit `-- <paths>`; otherwise return an actionable validation error.
- **`pickaxe`** (`string`, optional) & **`pickaxe_regex`** (`boolean`, default `false`):
  - Emits attached `-S<pickaxe>` (literal count change) or `-G<pickaxe>` (diff regex match) before the revision argument so a leading `-` in `pickaxe` cannot be interpreted as a separate git flag.
  - Reject NUL bytes and enforce a reasonable length bound on `pickaxe`.
- **`oneline`** (`boolean`, default `false`):
  - Emits `--oneline` instead of `--stat` when `true` (unless `-L` is active, which implies patch output).

### 3.2. `GitGrepTool` Function Context (`src/toolbox/git_grep.rs` & `src/toolbox/utils.rs`)
- **`function_context`** (`boolean`, default `false` in both `normalize_args` and `call`):
  - Ignored when `count_only` is `true`.
  - When `function_context` is `true` and `!count_only`, pass `-W` (`--function-context`) without `-C0` (unless `context_lines > 0` is also set, where `-W` still governs enclosing function display).
- **Separator & Hunk-Cap Handling (`src/toolbox/utils.rs`):**
  - `git grep -n -W` uses `=` around line numbers on function header lines (`<rev>:<path>=<line>=<code>`). Extend `get_grep_regex()` from `(:|-)` to `(:|-|=)` so function header lines are stripped of `<revision>:`, grouped under `[file: <path>]`, and preserved cleanly.
  - Cap individual `--`-delimited hunks in `format_git_grep_output` at 200 lines (`... [function hunk capped at 200 lines]`) so a single multi-thousand-line function or Rust `impl` block cannot crowd out subsequent proximity-ranked file matches before the 10,000-token `Truncator` runs.

---

## 4. Security & Cache Invariants
- All git subprocesses spawn exclusively via `crate::git_cmd::in_dir_async` and execute through `capped_output` (`MAX_CAPTURED_STDOUT` + `TOOL_COMMAND_TIMEOUT`).
- Every optional parameter has identical defaults in `normalize_args` and `call` so `ToolBox` cache keys remain canonical.
- Leading `'-'` revisions, ranges, objects, and paths are rejected prior to spawning git.

## 5. Verification Plan
1. Unit and integration tests in `src/toolbox/tools_test.rs`, `src/toolbox/utils.rs`, `src/notes/evidence.rs`, `src/worker/location_resolver.rs`, `src/ai/gemini.rs`, and `src/ai/session.rs` covering plain-text formatting, 1-based line numbers, downstream snippet line-number stripping, truncation line counts, Gemini `Struct` wrapping, `git_log` (`paths`, `line_range`, `pickaxe`, `oneline`, injection rejection), and `git_grep` (`function_context`, `=line=` parsing, hunk cap).
2. `make check-pr` across default, `--no-default-features`, and `--no-default-features --features cache` profiles.
3. Sashiko Stage 1 local self-review (`cargo run --bin sashiko -- review --project sashiko --agent <commit>`).
