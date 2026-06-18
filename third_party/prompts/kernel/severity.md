# Severity Levels

When identifying issues, you must assign a severity level to each finding.
Treat this task seriously, it's very important. Don't unnecessarily raise the priority,
critical issues must be critical, high issues must be very damaging.
Use Medium as default and lower/raise depending on the "Question to ask" answer and examples.
Use the following definitions and examples:

## Calibrating the level (reason before you label)

State this reasoning at the start of the severity_explanation so the label is
auditable.

- Consequence: what actually happens if the bug triggers (memory or data
  corruption, crash, info leak, resource leak, incorrect result, performance,
  or other). This is the starting point for the level.
- Triggering path: lay out the concrete path that reaches the bug, naming the
  preconditions a caller or input must satisfy. If you cannot, because it rests
  on an unproven assumption or on an ABI, register, or convention you might be
  misreading, still report the finding and mark it speculative.
- Reachability: if the bug is reachable by untrusted, remote, or unprivileged
  input, raise the level. Do not lower a finding because you believe it is
  unreachable: reachability is hard to establish from a diff, and a wrong call
  buries a real bug. If you cannot establish reachability, leave the level on
  consequence alone.

A speculative finding is the one case where the level is capped, at Medium,
because the open question is whether the bug is real at all. The finding is
always reported, never dropped. This is the only reason to lower a level.
Reachability never does.

## Critical
- **Definition**: Issues that cause data loss, memory corruptions or security vulnerabilities.
- **Question to ask**: Is it actually better for system to crash rather then keep working? If yes, it's a critical issue.
- **Examples**:
    - Security vulnerability.
    - Data corruption.
    - Memory corruption (e.g., buffer overflow, use-after-free).
    - Kernel panic or oops on hot path or which can be triggered by a userspace program or remotely.
    - ABI breakage without proper deprecation.

## High
- **Definition**: Serious issues that can bring the system down or make it fully unusable.
- **Question to ask**: Can the system go down or become totally unusable with a non-trivial probability? If yes, it's a high issue.
- **Examples**:
    - Kernel panic or oops.
    - Logic errors leading to incorrect functional behavior.
    - Resource leaks (memory, locks).
    - Significant performance regression.
    - Violation of core kernel locking rules.

## Medium
- **Definition**: Recoverable issues or non-critical performance regressions.
- **Examples**:
    - Memory or resource leaks on cold paths.
    - Inefficient locking.
    - Incorrect statistics.
    - Meaningful code and commit message mismatch.
    - Non-critical performance regressions.
	- Issues in kselftests, perf and other userspace applications.

## Low
- **Definition**: Naming, style and coding style issues.
- **Question to ask**: Is there any visible real life effect? If no, it's a low issue. Otherwise it's a medium issue.
- **Examples**:
    - Build issues (because there are better ways to find them).
    - Typos in comments.
    - Formatting issues.
    - Confusing variable naming or comments.
    - Negligible performance regressions.
    - Unnecessary code complexity.
    - Missing documentation or comments.
