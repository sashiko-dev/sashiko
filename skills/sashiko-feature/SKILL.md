---
name: sashiko-feature
description: Meta-skill for implementing new features. Guides the agent through requirement analysis, design doc lookup, codebase investigation, SOLID/DRY implementation in Rust, and iterative validation with Makefile checks. Use for feature creation requests.
---

# Sashiko Feature Creation Skill

## Core Workflow
Follow this structured process for every feature implementation request:

### 1. Analysis & Discovery
- **Read Baseline:** Read `GEMINI.md` to identify project-wide constraints, coding standards, and mandatory practices.
- **Detect Design Docs:** Search the `designs/` directory for documents relevant to the feature request. If multiple docs seem relevant or none are found, **ask the user for clarification**.
- **Investigate Codebase:** Invoke the `codebase_investigator` on the modules likely to be affected. Determine:
    - Existing paradigms and data structures.
    - Integration points.
    - Potential impacts on other system components.

### 2. Planning
- Propose a step-by-step implementation plan.
- **Commit Strategy:** Plan to split the work into small, logical, and self-sufficient commits.
- **Principles Alignment:** Reference [references/principles.md](references/principles.md) to ensure the proposed design adheres to SOLID/DRY principles in a Rust context.

### 3. Implementation & Iterative Validation
- Execute the plan one step at a time.
- **Standard Adherence:** Adhere strictly to idiomatic Rust (v1.90 compatibility) and the rules in `GEMINI.md`.
- **Continuous Checking:** After **every** iteration of code changes:
    - Run `make lint` for formatting and clippy checks.
    - Run `make test` for unit testing.
    - Run `make check-pr` before completing a task to ensure all CI-level checks pass.
- **Sign-off:** All commits must be signed-off (`git commit -s`).

### 4. Final Review
- Summarize the changes against the original request.
- Verify that no temporary logs or files are left behind.
- Ensure the final state of the repository is clean.