# Severity Levels

When identifying issues, you must assign a severity level to each finding.
Treat this task seriously, it's very important. Don't unnecessarily raise the priority,
critical issues must be critical, high issues must be very damaging.
Use Medium as default and lower/raise depending on the "Question to ask" answer and examples.
Use the following definitions and examples:

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
    - Typos in comments.
    - Formatting issues.
    - Potential build failures.
    - Confusing variable naming or comments.
    - Negligible performance regressions.
    - Unnecessary code complexity.
    - Missing documentation or comments.
