# systemd Patch Review Protocol

## Overview

This protocol guides systematic review of systemd patches for correctness,
style compliance, and potential regressions.

## Pre-Review Setup

Before beginning review:
1. ALWAYS load `technical-patterns.md` first
2. Load subsystem-specific files based on changed code (see triggers below)
3. Load pattern files when specific patterns are detected

## Review Tasks

### TASK 0: Identify Changed Functions

Use available tools to identify all functions modified by the patch:
- For git commits: examine the diff
- List each modified function with its file location

### TASK 1: Subsystem Context Loading

Load subsystem files based on code locations and patterns:

| Trigger | File to Load |
|---------|--------------|
| `src/core/`, PID1, unit lifecycle | `core.md` |
| `src/nspawn/`, container, pivot_root | `nspawn.md` |
| namespace, mount, unshare | `namespace.md` |
| `_cleanup_`, TAKE_PTR, TAKE_FD | `cleanup.md` |
| sd-bus, D-Bus, DBus | `dbus.md` |

### TASK 2: Pattern Detection

When you encounter these patterns, load the corresponding pattern file:

| Pattern | File |
|---------|------|
| `_cleanup_` attribute usage | `patterns/CLEANUP-001.md` |
| `TAKE_PTR()` or `TAKE_FD()` | `patterns/TAKE-001.md` |

### TASK 3: Per-Function Analysis

For each modified function, analyze:

**3.1 Error Handling**
- Are all error paths handled correctly?
- Are errors propagated properly (negative errno)?
- Is `RET_NERRNO()` used for libc calls?
- Are cleanup attributes properly releasing resources on error?

**3.2 Resource Management**
- Are all allocated resources freed on all paths?
- Are file descriptors closed (or transferred with TAKE_FD)?
- Are cleanup attributes used correctly (LIFO order)?
- Is ownership transfer explicit with TAKE_PTR/TAKE_FD?

**3.3 File Descriptor Safety**
- Are new FDs created with O_CLOEXEC/SOCK_CLOEXEC?
- Is O_NONBLOCK used for foreign file opens?
- Are FDs properly closed or transferred?

**3.4 Thread/Process Safety**
- If in PID1 code: NO threading, NO NSS calls
- If in library code: thread-safe considerations?
- Is clone()/fork() usage correct?

**3.5 Style Compliance**
- Does the code follow systemd coding style?
- Are destructors NULL-safe and return NULL?
- Are parameter names correctly prefixed (ret_, reterr_, arg_)?

### TASK 4: Integration Analysis

**4.1 Caller Impact**
- How do callers use this function?
- Could the changes break existing callers?
- Are API contracts preserved?

**4.2 Error Propagation**
- Are new error codes properly documented?
- Do callers handle new error conditions?

## Severity Classification

When reporting issues:

**CRITICAL**: Security vulnerabilities, crashes, data corruption
- Use-after-free, double-free
- Missing error checks leading to crashes
- File descriptor leaks in long-running code
- NSS/threading violations in PID1

**HIGH**: Functional bugs, resource leaks
- Memory leaks
- Incorrect error handling
- Missing cleanup on error paths

**MEDIUM**: Style violations, potential issues
- Missing O_CLOEXEC (non-security context)
- Suboptimal patterns
- Missing (void) casts

**LOW**: Cosmetic, suggestions
- Naming conventions
- Code organization

## Output Format

When issues are found, generate `review-inline.txt` using `inline-template.md`.

For each issue:
1. File and line number
2. Severity level
3. Description of the issue
4. Suggested fix (if applicable)

## False Positive Check

Before reporting any issue:
1. Consult `false-positive-guide.md`
2. Verify the issue can actually occur in practice
3. Trace the execution path to confirm
