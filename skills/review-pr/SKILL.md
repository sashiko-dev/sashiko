---
name: review-pr
description: Performs scrutinizing code reviews against project standards (e.g., GEMINI.md) and design docs. Intelligently detects relevant design files, categorizes findings (Safety, Complexity, Style), and generates actionable diffs. Use when the user asks to review a PR, diff, or specific codebase components.
---

# Review PR Skill

## Core Directives
When asked to perform a code review, follow these steps strictly:

### 1. Context Gathering & Design Detection
- **Read Baseline Standards:** Immediately read `GEMINI.md` (or the equivalent local project standard file) to establish the coding standards, style guidelines, and mandatory architectural rules.
- **Analyze Code Scope:** Look at the files modified in the PR or the specific code components requested for review. Identify the core features or modules being touched (e.g., `cli`, `auth`, `db`).
- **Detect Design Documents:** Automatically locate the project's design directory. Look for directories named `designs/`, `docs/design/`, `rfcs/`, or `architecture/`. 
- **Map and Read Design Docs:** Map the modified files/modules to specific design documents. For example, if `sashiko-cli.rs` is modified, look for a file like `DESIGN_CLI_TOOL.md`. Read the matched design documents to understand the intended architecture and behavior.

### 2. Analysis
Perform a deep, scrutinizing code review of the target code against the gathered context. Evaluate for:
- **Design Alignment:** Does the implementation match the intent described in the design documents?
- **Safety:** Are there any risky operations (e.g., unchecked unwraps, poor error handling, thread safety issues, SQL injection risks)?
- **Idiomatic Conventions:** Does the code use the language idiomatic patterns and the project-specific coding standards defined in `GEMINI.md`?
- **SOLID/DRY Principles:** Reference [references/principles.md](references/principles.md) and evaluate the code for adherence to SOLID and DRY principles within the Rust context.
- **Test Coverage:** Are unit or integration tests missing for new logic?

### 3. Reporting
Output your findings in a structured, categorized markdown format. Use the following emoji prefixes for each finding to indicate severity/type:
- 🔴 **Safety / Bugs:** Broken behavior, potential crashes, security risks, or unhandled errors.
- 🟡 **Complexity / Logic / SOLID:** High cyclomatic complexity, functions that are too long, risky architectural choices, or violations of SOLID principles.
- 🔵 **Style / Nits / DRY:** Style violations, naming issues, duplicated code (DRY violations), or minor optimizations.
- 🟢 **Design Alignment:** Noting where the code successfully or unsuccessfully aligns with the mapped design documents.

Keep descriptions terse and focused on the *why* and the *fix*.

### 4. Fix Generation
For *every* actionable finding, provide a unified diff in a markdown code block that the author can directly apply or paste into a PR comment. 
- Ensure diffs are accurate and apply cleanly.
- Never use "..." or placeholders in diffs. Provide the exact text replacement.