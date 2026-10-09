# Toolbox and LLM Tool Invariants

This guide covers `src/toolbox/` (`framework.rs`, `mod.rs`, `utils.rs`,
`command.rs`, and the tool implementations: `git_read_files`, `git_grep`,
`git_log`, `git_show`, `git_diff`, `git_blame`, `git_ls`, `git_find_files`, and
`read_prompt`).

The toolbox is the boundary where an LLM reviewing untrusted code interacts with
the host filesystem and git subprocesses. Every tool implementation must uphold
strict sandboxing, error recovery, and output-budget invariants.

## 1. Path Confinement (`validate_path`)

Tools execute against a git worktree (`SashikoToolContext::worktree_path`) or
prompt bundle (`prompts_path`). Model tool arguments are attacker-steerable via
prompt injection in the reviewed patch.
- **Mandatory Guard**: Every tool that accepts a file or directory path from the
  model *must* validate and canonicalize it via `utils::validate_path(relative, base)`
  before accessing the filesystem or passing a path to a command.
- **What `validate_path` Enforces**:
  1. Rejects any string containing `..` or starting with `/`.
  2. Joins the path to `base` and canonicalizes both `base` and the target (or
     its parent if the target does not exist).
  3. Verifies `canonical_full.starts_with(&canonical_base)`, preventing symlink
     escapes outside the worktree or prompt directory.
- **Defect to Catch**: A new tool or modified parameter that joins a model
  string directly onto `worktree_path` (e.g., `context.worktree_path.join(path)`)
  or passes an unvalidated path to `std::fs` or `git`.

## 2. Git Subprocess Safety & Virtualized `HEAD`

Tools that invoke `git` (`command::run_git_command`) run inside the active
worktree.
- **Argument Injection**: Model-supplied strings (revisions, regex patterns,
  paths) passed to `git` must never be interpreted as flags.
  - Any path list must follow a `--` separator argument.
  - Revision/commit arguments must be validated or passed where git expects a
    ref, never concatenated into shell strings (all execution uses `tokio::process::Command`
    argv vectors, never a shell).
- **Virtualized `HEAD` (`virtualize_ref`)**: During multi-patch series reviews
  or concurrent single-worktree reviews, `SashikoToolContext::virtual_head`
  holds the target commit SHA. Any tool taking a revision string must pass it
  through `context.virtualize_ref(rev)` so references to `HEAD` (e.g., `HEAD~1`,
  `HEAD:path`) resolve to the virtual commit rather than whatever physical
  checkout state the worktree has.

## 3. Non-Fatal Tool Errors

When an LLM calls a tool with bad arguments (non-existent file, invalid regex,
unknown revision), the tool must *never* panic or fail the Rust workflow stage.
- **Error Contract**: Tools return `Ok(Value)` containing structured output or a
  recoverable error message (e.g., `json!({"error": "File not found: ..."})`).
  `StageSession::call_tools` in `src/workflow/stage.rs` catches `Err` results
  from `ToolBox::call` and converts them into `{"error": e.to_string()}` JSON
  tool responses so the model can adjust its query on the next turn.
- **Duplicate Call Guard**: `StageSession` tracks the previous turn's
  `(tool_name, args)` calls and blocks identical consecutive calls with an
  explicit error JSON payload to break LLM loops. Tools must remain deterministic
  so caching (`SashikoToolContext::cache`) is valid.

## 4. Output Truncation and Paging Contract

Unbounded tool outputs blow up the prompt token budget (`max_input_tokens`) and
cause provider context-window errors.
- **Truncation Metadata**: Tools that return potentially large text (`git_read_files`,
  `git_grep`, `git_show`, `git_diff`, `git_log`) must bound their output size
  and include explicit pagination/truncation metadata (`"truncated": true`, line
  ranges, total lines, or byte offsets) in the returned JSON object.
- **Proximity Sorting**: `utils::format_git_grep_output` groups and sorts `git grep`
  matches by proximity to `active_patch_files` so truncated output preserves
  the most relevant matches first.

## 5. Conditional Registration (`read_prompt`)

`read_prompt::ReadPromptTool` reads files from `context.prompts_path`.
- **Invariant**: `ToolBox::new` registers `ReadPromptTool` *only* when
  `prompts_path.is_some()`. Standalone pipelines (like the Linux bug worker)
  that instantiate `ToolBox::new(path, None)` must not expose `read_prompt`.

## Checklist for Diffs Touching `src/toolbox/`

1. **Path Traversal**: Does every filesystem path argument pass through
   `validate_path` against `worktree_path` or `prompts_path`?
2. **Option Injection**: Are model-provided file paths preceded by `--` in git
   commands? Could a model-provided string starting with `-` be parsed as a git
   flag?
3. **Virtual HEAD**: Does any new revision parameter call `context.virtualize_ref(...)`?
4. **Truncation**: Is the maximum returned string size bounded, and does the JSON
   output signal `"truncated": true` when clipped?
