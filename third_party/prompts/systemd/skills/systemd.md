---
name: systemd
description: Load anytime the working directory is a systemd tree. systemd-specific knowledge, subsystem details, code review, and debugging protocols. Read this anytime you're in the systemd tree.
invocation_policy: automatic
---

## ALWAYS READ
1. Load `{{SYSTEMD_REVIEW_PROMPTS_DIR}}/technical-patterns.md`

These files are MANDATORY. This skill exists as a framework for loading
additional systemd prompts.

## Configuration

The review prompts directory is configured during installation:
- **SYSTEMD_REVIEW_PROMPTS_DIR**: {{SYSTEMD_REVIEW_PROMPTS_DIR}}

This variable is set by the installation script when the skill is installed.

## Capabilities

### Patch Review
When asked to review a systemd patch, commit, or series of commits:
1. Load `{{SYSTEMD_REVIEW_PROMPTS_DIR}}/review-core.md`
2. Follow the complete review protocol defined there
3. Load subsystem-specific files as directed by review-core.md

### Debugging
When asked to debug a systemd crash, hang, or unexpected behavior:
1. Load `{{SYSTEMD_REVIEW_PROMPTS_DIR}}/debugging.md`
2. Follow the complete debugging protocol defined there
3. Use crash/log information as entry points into code analysis

### Subsystem Context
When working on systemd code in specific subsystems, load the appropriate
context files from `{{SYSTEMD_REVIEW_PROMPTS_DIR}}/`:

1. Always read `technical-patterns.md` before loading subsystem specific files

2. Select subsystem specific files as needed:

| Subsystem | Trigger | File |
|-----------|---------|------|
| Service Manager | src/core/, Unit, Manager, Job | core.md |
| Namespaces | namespace, unshare, setns, CLONE_NEW* | namespace.md |
| Containers | src/nspawn/, container, pivot_root | nspawn.md |
| D-Bus | sd-bus, dbus, bus_ | dbus.md |
| Journal | src/journal/, sd-journal | journal.md |
| udev | src/udev/, rules, udevd | udev.md |
| Network | src/network/, networkd, netlink | network.md |
| Cleanup | _cleanup_, TAKE_PTR, TAKE_FD | cleanup.md |
| Credentials | credentials, encrypted | credentials.md |

## Semcode Integration

When available, use semcode MCP tools for efficient code navigation:
- `find_function` / `find_type`: Get function and type definitions
- `find_callchain`: Trace call relationships up and down
- `find_callers` / `find_calls`: Explore call graphs
- `grep_functions`: Search function bodies with regex
- `diff_functions`: Identify changed functions in patches
- `find_commit` / `vcommit_similar_commits`: Search commit history

The [semcode github repository](https://github.com/facebookexperimental/semcode)
has instructions on setting up the indexing and MCP server.

## Output

- Patch reviews produce `review-inline.txt` when regressions are found
- Debug sessions produce `debug-report.txt` with analysis results
- Both outputs are plain text, suitable for GitHub PRs or mailing lists

## Key systemd Conventions

### Error Handling
- Return negative errno values: `return -EINVAL;`
- Use `RET_NERRNO()` for libc calls
- Library code doesn't log (except DEBUG level)

### Memory Safety
- Use `_cleanup_*` attributes extensively
- Use `TAKE_PTR()`/`TAKE_FD()` for ownership transfer
- Initialize cleanup variables to NULL/-EBADF

### File Descriptors
- ALL FDs must have O_CLOEXEC from creation
- Use `safe_close()` which handles -EBADF

### Threading
- No threads in PID1 (service manager)
- No NSS calls from PID1
- Prefer fork() over clone() where possible
