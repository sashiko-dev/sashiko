# systemd Debugging Protocol

## Overview

This protocol guides systematic debugging of systemd crashes, assertions,
and unexpected behavior.

## Pre-Debug Setup

1. ALWAYS load `technical-patterns.md` first
2. Identify the component that crashed
3. Load component-specific context files

## Component Identification

Identify the failing component from the crash/log:

| Component | Binary/Context | Files to Load |
|-----------|----------------|---------------|
| PID1 | systemd, init | `core.md` |
| nspawn | systemd-nspawn | `nspawn.md`, `namespace.md` |
| Journal | systemd-journald | - |
| Network | systemd-networkd | - |
| Udev | systemd-udevd | - |
| Login | systemd-logind | - |
| Resolver | systemd-resolved | - |

## Debug Tasks

### DEBUG.1: Crash Information Extraction

From the crash report, extract:
- Faulting address/instruction
- Stack trace (all frames)
- Register state (if available)
- Signal type (SIGSEGV, SIGABRT, etc.)
- Assertion message (if assert failure)

### DEBUG.2: Stack Trace Analysis

For each frame in the stack trace:
1. Identify the function name and source file
2. Look up the function implementation
3. Identify the failing line if possible
4. Note relevant local variables

### DEBUG.3: Root Cause Hypothesis

Based on the crash type, form hypotheses:

**SIGSEGV (Segmentation Fault)**:
- NULL pointer dereference
- Use-after-free
- Invalid pointer arithmetic
- Stack overflow

**SIGABRT (Abort)**:
- `assert()` failure
- `assert_return()` failure
- Memory allocator corruption
- Double-free detected

**SIGFPE (Floating Point Exception)**:
- Division by zero
- Integer overflow (with -ftrapv)

### DEBUG.4: Code Path Tracing

Trace backwards from the crash point:
1. What function called the crashing function?
2. What values were passed as arguments?
3. Under what conditions does this path execute?
4. What state must be true for the crash to occur?

### DEBUG.5: Resource State Analysis

Check resource states leading to crash:
- Were cleanup attributes active?
- Were file descriptors valid?
- Was memory properly allocated?
- Were reference counts correct?

### DEBUG.6: Reproduction Analysis

Determine conditions for reproduction:
- What sequence of operations triggers the bug?
- Is it timing-dependent (race condition)?
- Is it state-dependent (specific configuration)?
- Is it input-dependent (specific data)?

## Common Crash Patterns

### Pattern: Use-After-Free
```
Symptoms: SIGSEGV accessing freed memory
Check: Was object freed before use?
Check: Did cleanup attribute trigger early?
Check: Was TAKE_PTR/TAKE_FD used correctly?
```

### Pattern: Double-Free
```
Symptoms: SIGABRT in free(), malloc corruption
Check: Was object freed twice?
Check: Did cleanup attribute and explicit free both run?
Check: Was pointer set to NULL after free?
```

### Pattern: NULL Dereference
```
Symptoms: SIGSEGV at address 0x0 (or small offset)
Check: Was return value checked?
Check: Was pointer initialized?
Check: Was error path taken that left pointer NULL?
```

### Pattern: File Descriptor Misuse
```
Symptoms: EBADF errors, wrong file operations
Check: Was FD closed prematurely?
Check: Was TAKE_FD used correctly?
Check: Was FD initialized to -EBADF?
```

### Pattern: Assert Failure in PID1
```
Symptoms: SIGABRT with assert message, system freeze
Check: What condition failed?
Check: Is this a programming error or unexpected state?
Check: Can this state be reached in production?
```

## Output Format

Generate `debug-report.txt` with:

1. **Summary**: One-line description of the bug
2. **Component**: Which systemd component
3. **Crash Type**: SIGSEGV/SIGABRT/etc.
4. **Root Cause**: Explanation of why crash occurs
5. **Code Path**: How execution reaches the crash
6. **Fix Recommendation**: Suggested code changes
7. **Severity**: CRITICAL/HIGH/MEDIUM/LOW

## Special Considerations

### PID1 Crashes
- PID1 crash = system freeze/reboot
- Extra scrutiny for assert() conditions
- Check for NSS or threading violations
- Consider recovery mechanisms

### Cleanup Attribute Issues
- Check LIFO cleanup order
- Verify all paths trigger cleanup
- Check for double-free with explicit cleanup
- Verify TAKE_PTR/TAKE_FD usage

### Memory Corruption
- May crash far from actual bug
- Look for buffer overflows
- Check array bounds
- Verify struct sizes match expectations
