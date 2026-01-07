# Design: Security - Sashiko Review Worker

## 1. Overview
This document outlines the security architecture for the `sashiko-review` worker. The worker is a high-privilege automated component that interacts with the Linux kernel source code, git history, and AI models. The primary security goal is to enable the worker to perform deep code analysis while preventing unauthorized system access, data exfiltration, or integrity violations.

## 2. Core Security Principles
*   **Least Privilege**: The worker is granted only the permissions strictly necessary for analysis (Read-Only).
*   **Input Sanitization**: All inputs from the LLM (tool arguments) are treated as untrusted.
*   **Isolation**: Git operations run in a restricted context.

## 3. Access Control Requirements

### 3.1. Linux Source Code & Git History
*   **Requirement**: Full **READ** access.
*   **Scope**: The worker must be able to read any file within the Linux kernel source tree and query the full git history (logs, blames, diffs).
*   **Restriction**: **LIMITED WRITE** access. The worker is primarily read-only but is granted the ability to write specific files (e.g., `review-inline.txt`) to the disposable worktree using the `write_file` tool. It must never be able to modify the repository configuration, commit history, or other critical files.

### 3.2. Prompts
*   **Requirement**: Full **READ** access.
*   **Scope**: The worker must be able to read all prompt templates and configuration files in the `review-prompts` directory.
*   **Restriction**: **NO WRITE** access. The worker cannot modify its own instructions.

## 4. Threat Model & Mitigations

### 4.1. Directory Traversal / Path Injection
*   **Threat**: The LLM (potentially hallucinating or manipulated via prompt injection) attempts to access files outside the repository (e.g., `/etc/passwd`, `~/.ssh`).
*   **Mitigation**:
    *   **Path Validation**: All file path arguments passed to tools (e.g., `read_file`, `git_blame`) must be canonicalized and verified to be descendants of the authorized root (repo or prompts dir).
    *   **Blocklist**: Explicitly reject paths containing `..` or absolute paths starting with `/` (unless resolved to within the root).

### 4.2. Command Injection
*   **Threat**: The LLM attempts to execute arbitrary shell commands via git arguments (e.g., `git log ; rm -rf /`).
*   **Mitigation**:
    *   **No Shell Execution**: Do not use `sh -c` or `system()`. Use `std::process::Command` (Rust) which passes arguments directly to the executable (execvp style), bypassing the shell.
    *   **Allowlist**: Only specific, safe subcommands are exposed (see Section 5).

### 4.3. Resource Exhaustion
*   **Threat**: The LLM requests massive operations (e.g., `git grep` on common words, reading huge binary files).
*   **Mitigation**:
    *   **Timeouts**: Strict timeouts for all tool executions.
    *   **Output Limiting**: Truncate output from tools (e.g., max 20KB per read).
    *   **Rate Limiting**: Limit the number of tool calls per review session.

## 5. Safe Tool Definitions

The `ToolBox` exposes a restricted set of primitives.

### 5.1. Git Operations
Implemented via `git` CLI, strictly parameterized.
*   `git_show(revision, path)`: Safe.
*   `git_diff(range)`: Safe.
*   `git_blame(path, lines)`: Safe.
*   `git_log(path, limit)`: Safe.
*   `git_grep(pattern, path)`: **Caution**. Must prevent ReDoS (Regex Denial of Service) if possible, but standard git grep is generally robust. Enforce `max_count`.

**Banned**: `apply`, `commit`, `push`, `config`, `rm`, `add`.

### 5.2. File System Operations
*   `read_file(path, range)`: Restricted to repo root.
*   `list_dir(path)`: Restricted to repo root.
*   `write_file(path, content)`: Restricted to repo root (specifically intended for `review-inline.txt`).

### 5.3. Prompt Access
*   `read_prompt(name)`: Restricted to `review-prompts` directory.

## 6. Implementation Guidelines

1.  **Worktree Isolation**:
    *   Each review session should ideally spawn a temporary `git worktree` in `review_trees/`.
    *   This ensures that even if the worker *could* modify files (e.g. via a bug in `git apply` before the review), it only affects a disposable directory.
    *   Cleanup: The worktree must be strictly removed after the session.

2.  **Path Sanitization Logic (Rust)**:
    ```rust
    fn validate_path(root: &Path, user_path: &str) -> Result<PathBuf, Error> {
        let path = root.join(user_path);
        let canonical = path.canonicalize()?; // Resolves symlinks and ..
        if canonical.starts_with(root.canonicalize()?) {
            Ok(canonical)
        } else {
            Err(Error::AccessDenied)
        }
    }
    ```

3.  **Audit Logging**:
    *   Log every tool call and its arguments in the `ai_interactions` table.
    *   Log any "Access Denied" attempts as security alerts.
