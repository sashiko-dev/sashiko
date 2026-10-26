+++
title = "Agent Toolbox and Worktree Confinement Policy"
paths = [
    "src/toolbox/",
]
symbols = [
    "LlmTool",
    "ToolBox",
    "capped_output",
]
+++

## Worktree Sandboxing and Read-Only Confinement

- All agent tools exposed through `ToolBox` (`src/toolbox/`) are invoked with
  LLM-controlled arguments while reviewing untrusted patches and must remain
  strictly read-only and confined to the target git worktree (and the optional
  read-only prompts directory when explicitly configured).
- Reject any tool path argument containing `..` traversal components, leading
  `/` absolute paths, or symlink escapes outside the allowed root before reading
  from the filesystem.
- When a tool delegates to git (`git_blame`, `git_diff`, `git_log`, `git_show`,
  `git_grep`, `read_file` at virtual head), always spawn via `git_cmd` and pass
  `--end-of-options` or `--` before user/model-supplied paths or regexes so
  leading `-` arguments cannot inject git flags.
- Every tool must bound its captured stdout/stderr and returned JSON payload size
  (`MAX_CAPTURED_STDOUT` / `capped_output`) and enforce `TOOL_COMMAND_TIMEOUT`
  so a runaway search or huge file cannot stall the review stage or blow out the
  LLM context window.
- Tool execution errors (such as missing files, invalid regexes, or non-existent
  revisions) must return structured error descriptions to the model rather than
  panicking or aborting the entire review workflow.
