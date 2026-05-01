# AI Tools

Sashiko provides built-in and custom tools for AI interaction with the codebase. All built-in tools are enabled by default.

## Built-in Tools

### File Operations
- `read_files`: Read file content. Supports "smart" mode (collapses boilerplate).
- `list_dir`: List directory contents.
- `find_files`: Locate files via glob patterns (`*.rs`).
- `search_file_content`: Grep-like pattern search.

### Git Operations
- `git_diff`: Show changes between commits/refs.
- `git_log`: View commit history. Use `--since` for performance.
- `git_show`: Inspect git objects (blobs, commits).
- `git_blame`: Identify line-level modification history.
- `git_status`: Check working tree state.
- `git_branch`: List local and remote branches.
- `git_tag`: List repository tags.
- `git_checkout`: Switch branches (modifies worktree).

### Specialized
- `read_prompt`: Access files from the prompt registry.
- `TodoWrite`: Append items to `TODO.md`.

## Configuration

Control tool access in `Settings.toml`:

### Allowlist Mode
```toml
[tools]
enabled = ["read_files", "git_diff", "git_show"]
```

### Denylist Mode
```toml
[tools]
# Disable state-modifying tools for read-only environments
disabled = ["git_checkout", "TodoWrite"]
```

*Note: `disabled` takes precedence over `enabled`.*

## Custom Tools

Define external commands as AI tools in `Settings.toml`:

```toml
[[tools.custom]]
name = "static_check"
description = "Run external analyzer"
parameters = """
{
  "type": "OBJECT",
  "properties": {
    "path": { "type": "STRING" }
  },
  "required": ["path"]
}
"""
command = "/usr/bin/check --file {path}"
allowed_paths = ["src/"]
```

### Security Constraints
- **Blocked**: `sudo`, `rm -rf`, `curl`, `wget`, `dd`, `mkfs`.
- **Isolation**: Runs inside the isolated review worktree.
- **Path Validation**: `allowed_paths` prevents traversal beyond the worktree.
- **Substitutions**: Use `{parameter_name}` in the command string. Arrays are space-joined.

## Examples

### Restriction Patterns
```toml
# Read-only environment
[tools]
disabled = ["git_checkout", "TodoWrite"]

# Performance-optimized (minimal set)
[tools]
enabled = ["read_files", "git_diff", "git_show", "search_file_content"]
```

### Custom Tool Definitions
```toml
# Performance Profiler
[[tools.custom]]
name = "profile_function"
description = "Profile function with perf"
parameters = """
{
  "type": "OBJECT",
  "properties": {
    "function_name": { "type": "STRING" },
    "file": { "type": "STRING" }
  },
  "required": ["function_name", "file"]
}
"""
command = "perf-profiler --function {function_name} --file {file}"
allowed_paths = ["src/", "kernel/"]

# Documentation Generator
[[tools.custom]]
name = "generate_docs"
description = "Generate API docs"
parameters = """
{
  "type": "OBJECT",
  "properties": {
    "module_path": { "type": "STRING" }
  },
  "required": ["module_path"]
}
"""
command = "rustdoc {module_path} --output /tmp/docs"
allowed_paths = ["src/"]
```
