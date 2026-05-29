# Severity Levels

When identifying issues, you must assign a severity level to each finding.
Treat this task seriously, it's very important. Don't unnecessarily raise the priority,
critical issues must be critical, high issues must be very damaging.
Use Medium as default and lower/raise depending on the "Question to ask" answer and examples.
Use the following definitions and examples:

## Calibrate the label through three axes

Do not jump straight to a label. First reason through three independent
axes, then collapse them into one of the four levels below. State all three
axis values and the collapse in the finding's explanation so the label is
auditable. Separating "how bad if true" from "how reachable" from "how sure"
is what keeps the label honest: a finding that is `unprivileged` +
`crash` + `high` confidence is genuinely Critical, while one that is
`internal` + `crash` + `low` confidence is not.

- **Reachability** (who can actually reach the buggy code): one of
  `unprivileged`, `privileged`, `guest`, `internal`, or
  `unreachable`. An attacker-reachable path raises severity, while a path no
  one can trigger caps it.
- **Consequence** (what happens if it triggers): one of `crash`,
  `data-corruption`, `info-leak`, `perf`, `commit-msg-only`, or `style`.
  This is the "how bad if true" axis on its own.
- **Confidence** (how sure you are the bug is real and reachable): `low`,
  `medium`, or `high`. Reserve `high` for a concrete reproducible path with
  every precondition named. If you are speculating about whether the
  triggering condition can occur, say so and lower confidence.

Collapse the axes into the label, applying these rules in order:

- An `unreachable` reachability or a `commit-msg-only`/`style` consequence is
  at most Low, no matter how damaging the consequence would be if it were
  reachable. A bug nobody can hit is not Critical.
- `low` confidence caps the label at Medium. Still raise the finding, but do
  not inflate an unproven hypothesis to High or Critical. If the uncertainty
  is itself the point, state precisely what would confirm it.
- Critical or High requires all three of: a damaging consequence (crash,
  data-corruption, info-leak, or a security vulnerability), real
  reachability (unprivileged, privileged, or guest), and `medium`-or-better confidence.
- Otherwise pick Medium (recoverable, or a cold-path resource issue) or Low
  per the definitions below.

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
