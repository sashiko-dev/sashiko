# Toolbox and LLM Tool Invariants

This guide covers `src/toolbox/` (`framework.rs`, `mod.rs`, `utils.rs`,
`command.rs`, and the tool implementations: `git_read_files`, `git_grep`,
`git_log`, `git_show`, `git_diff`, `git_blame`, `git_ls`, `git_find_files`,
`read_prompt`, and the remote MCP tools in `mcp.rs`).

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

## 6. Remote MCP Tools (`mcp.rs`)

`mcp::McpTool` forwards model-chosen arguments to an operator-configured
Model Context Protocol server (`[[mcp.servers]]`, see
`designs/DESIGN_MCP_CLIENT_TOOLS.md`). It is the only tool that leaves the
host, so injected patch text can steer what it sends.
- **Fixed Endpoints**: The URL, headers and bearer token come only from the
  settings and the environment. The model chooses the tool name and its
  arguments, nothing else. Redirects stay disabled
  (`reqwest::redirect::Policy::none()`) so the token only reaches the
  configured origin.
- **Allowlist**: Only server tools named in `allowed_tools` are registered.
  A server's listing must never widen the set, and names it supplies are
  validated before they become `mcp_<server>_<tool>`.
- **Stage Exposure**: `ToolBox::is_tool_visible_in_stage` decides which
  stages see an MCP tool. `StageSession::tools()` filters declarations by it
  and `StageSession::refuse_hidden_tool` refuses calls to hidden tools in
  both `call_tool` and `call_tools`.
- **Secrets**: The token is never logged, cached, put in an error, or
  included in `Debug` output. Error text from the transport or the server
  passes through `redact_secret`.
- **Untrusted Output**: MCP results and server-supplied descriptions are data.
  They must never become a path, an `@include`, a prompt name or a lookup key,
  and server text must never be placed in a system prompt; the prompt hint
  comes from the settings file only.
- **Non-Fatal and Bounded**: Discovery failures skip the server with a
  warning; call failures and `isError` results become `{"error": ...}`
  values. Requests carry a timeout, response bodies and `tools/list` pages
  are capped, and results are truncated to `max_output_bytes` with
  `"truncated": true`.

## Checklist for Diffs Touching `src/toolbox/`

1. **Path Traversal**: Does every filesystem path argument pass through
   `validate_path` against `worktree_path` or `prompts_path`?
2. **Option Injection**: Are model-provided file paths preceded by `--` in git
   commands? Could a model-provided string starting with `-` be parsed as a git
   flag?
3. **Virtual HEAD**: Does any new revision parameter call `context.virtualize_ref(...)`?
4. **Truncation**: Is the maximum returned string size bounded, and does the JSON
   output signal `"truncated": true` when clipped?
5. **Remote Tools**: Does an MCP change keep endpoints, tokens and the
   allowlist out of the model's reach, keep tokens out of logs and errors,
   and keep server text out of prompts, paths and lookup keys?
