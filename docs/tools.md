# AI Tools

> **Experimental / Unsupported:** Custom tool configuration is experimental
> and provided as-is. Bugs and issues with the Sashiko ingestor, reviewer, or
> core tooling can and should still be filed. However, we will not provide
> support for debugging custom tool definitions or troubleshooting token
> consumption caused by custom tool configurations.

Sashiko gives the AI reviewer a set of built-in tools for navigating the
codebase under review. All built-in tools are enabled by default; you can
restrict or extend them via `Settings.toml`.

For the `[tools]` configuration reference, see
[Configuration Reference](configuration.md). For prompt customization,
see [Prompt Customization](prompts.md).

## Built-in tools

### File operations

| Tool | Description |
|------|-------------|
| `git_read_files` | Read file content at a git revision. Supports raw and smart modes. |
| `git_ls` | List directory contents at a git revision. |
| `git_find_files` | Locate files via glob patterns (e.g. `*.rs`, `src/**/mod.rs`). |
| `git_grep` | Search for patterns in files at a git revision. Returns matching lines with context. |

### Git operations

| Tool | Description |
|------|-------------|
| `git_diff` | Show changes between commits or refs. |
| `git_log` | View commit history for a range. |
| `git_show` | Inspect git objects (blobs, commits, tags). |
| `git_blame` | Identify line-level modification history. |

### Specialized

| Tool | Description |
|------|-------------|
| `read_prompt` | Read files from the prompt registry (only available when a prompts directory is configured). |

## Configuration

Control tool access in `Settings.toml`:

### Allowlist mode

```toml
[tools]
enabled = ["git_read_files", "git_diff", "git_show"]
```

Only the listed tools are available to the AI.

### Denylist mode

```toml
[tools]
disabled = ["git_log"]
```

All tools are available except those listed. `disabled` takes precedence
over `enabled` when both are specified.

## Custom tools

Define external shell commands as AI-callable tools:

```toml
[[tools.custom]]
name = "static_check"
description = "Run external analyzer on a source file"
parameters = """
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path to the source file" }
  },
  "required": ["path"]
}
"""
command = "/usr/bin/check --file {path}"
allowed_paths = ["src/"]
```

### Parameter substitution

Use `{parameter_name}` in the command string. The AI's argument values
are substituted before execution. Array values are space-joined.

### Security constraints

- **Blocked patterns** -- commands containing `sudo`, `rm -rf`, `curl`,
  `wget`, `dd `, or `mkfs` are rejected at registration time.
- **Worktree isolation** -- custom tools run inside the review worktree
  via `sh -c`.
- **Path validation** -- when `allowed_paths` is set, any argument whose
  key contains `path` or `file` is checked against the allowlist.
  Paths that escape the worktree are rejected.

## Examples

### Restriction patterns

```toml
# Minimal read-only set
[tools]
enabled = ["git_read_files", "git_diff", "git_show", "git_grep"]
```

### Custom tool definitions

```toml
# Sparse static analysis
[[tools.custom]]
name = "run_sparse"
description = "Run sparse static analysis on a C source file"
parameters = """
{
  "type": "object",
  "properties": {
    "file": { "type": "string", "description": "Path to the C source file" }
  },
  "required": ["file"]
}
"""
command = "sparse {file}"
allowed_paths = ["drivers/", "fs/", "kernel/", "mm/", "net/"]

# Documentation generator
[[tools.custom]]
name = "generate_docs"
description = "Generate API docs for a Rust module"
parameters = """
{
  "type": "object",
  "properties": {
    "module_path": { "type": "string", "description": "Path to the module" }
  },
  "required": ["module_path"]
}
"""
command = "rustdoc {module_path} --output /tmp/docs"
allowed_paths = ["src/"]
```
