# Design: Systematic Overhaul of Linux Bug Report Prompts and Severity Alignment

## 1. Overview & Motivation

The Linux bug workflow in Sashiko automatically verifies candidate bugs and generates standalone technical problem descriptions suitable for upstream Linux Kernel Mailing List (LKML) submission and internal storage (`bugs.inline_review`).

However, the current generation prompts suffer from several systematic shortcomings that degrade the quality of generated descriptions:

1. **Carets (`^^^^^`) Overuse & Misuse**: Carets are currently suggested as a general-purpose highlighter and are frequently misused to point at *missing* code (e.g., pointing `^^^^` at `goto out;` or `return -ENOMEM;` with a comment saying "missing kfree()"). Carets can only point to characters that exist; pointing at an early exit to indicate an omitted call is confusing and non-idiomatic.
2. **Line Length Overflow**: Comments placed alongside caret lines or code lines routinely exceed LKML's strict 72–75 character line width limit.
3. **Incomplete Snippets for Paired Operations**: Memory leaks and resource management defects (allocations/releases, locks/unlocks, refcount inc/dec) frequently clip the allocation site behind `< ... >`, showing only an error branch. The reader has no visibility into what was allocated or why it must be freed.
4. **Rigid Snippet Sizing**: An arbitrary "max 4-6 lines" rule forces the model to either clip necessary context or paste code where pure prose would be cleaner. In reality, some bugs need *no snippet at all*, while paired operations require *larger snippets* (10–20 lines) to show lifecycle.
5. **Artificial Table Formatting for Multi-CPU Concurrency**: Concurrency timelines currently use ASCII table borders with pipes (`|`) and hyphens (`---+---`), which are not the convention on LKML. Kernel developers format multi-CPU race timelines using clean whitespace-delimited columns with simple dashed underlines.
6. **Paraphrased Severity Prompt Drift**: `SeveritySession` currently re-summarizes severity levels and calibration rules with custom text instead of leveraging the authoritative `third_party/prompts/kernel/severity.md` prompt directly, leading to prompt drift from the `linux_patch` workflow.

This document systematically redesigns `ReportSession` (both system prompt, initial prompt, and few-shot examples) and aligns `SeveritySession` with the canonical severity prompt file.

---

## 2. Architectural Design & Prompt Directives

### 2.1 Problem Description Structure (`ReportSession`)

The generated problem description must adhere to maintainer-to-maintainer standards on LKML:
- **No Headers / Titles**: Do not begin with "Report:", "Defect:", "Problem:", or duplicate the bug subject. Start immediately with technical prose.
- **Lead with the Invariant & Scope in Sentence 1**: The first sentence must state what invariant is broken, in which function, and under what condition. If the issue is restricted to specific hardware, architecture (e.g., 32-bit), or config options, state that condition first.
- **Anti-Lecture Directive**: Never explain basic kernel mechanisms (what RCU is, how spinlocks work, slab allocator basics). Assume the reader is a subsystem maintainer.
- **Argue Once**: Do not include introductory chatter ("While auditing...", "I found that..."), defensive rationalizations, or concluding summaries.
- **No Fix / Patch Recommendations**: The description must describe the existing defect in the tree only, not propose patches or remediations.
- **Line Length Discipline**: All prose paragraphs and comment lines must be hard-wrapped at 72–75 columns.

---

### 2.2 Adaptive Code Snippet Strategy

Code snippets are a tool to clarify the explanation, not an obligatory checkbox. `ReportSession` is guided by three distinct snippet regimes:

1. **Zero Snippets (Pure Prose)**:
   - When to use: When the defect is an interface contract violation, an unhandled state transition, or an architectural mismatch that is completely self-evident from 1–2 crisp prose paragraphs.
   - Example: Missing a state reset when an interface transitions from disconnected to connected, or failing to propagate an errno from an underlying helper.

2. **Targeted Snippets (4–8 lines)**:
   - When to use: When an existing statement or expression contains an explicit bug (e.g., unsigned variable checked against `< 0`, inverted relational operator, off-by-one boundary condition, or NULL pointer dereferenced immediately before being checked).
   - Carets (`^^^^^`) are permitted **only** in this regime, pointing strictly to the defective token or operator.

3. **Expanded Lifecycle Snippets (10–20 lines)**:
   - When to use: For defects involving paired actions across branches (memory allocations & releases, lock acquire & release, `get_device()` & `put_device()`).
   - Mandatory rule: The snippet **must show both the acquisition/allocation site and the exit path where release was missed**.
   - Caret prohibition: **Never use `^^^^^` carets to highlight a missing call**. The prose clearly states what is leaked; pointing carets at a clean `return err;` or `goto out;` is forbidden.

---

### 2.3 Strict Rules for Caret Highlighting (`^^^^^`)

1. **Existential Requirement**: Carets may only point to tokens or expressions that exist in the code and are themselves erroneous (such as an incorrect operator, wrong variable name, or flawed type comparison).
2. **Prohibition on Missing Actions**: Never point carets at an empty line, a closing brace, a `goto`, or a `return` statement to denote that a function call (like `kfree()` or `mutex_unlock()`) is missing.
3. **No Line Length Spills**: If an inline comment accompanies carets, the total line length (including leading indentation) must not exceed 75 characters. If the comment is too long, place it on a separate comment line or explain it in the prose below the snippet.

---

### 2.4 LKML-Style Multi-CPU Concurrency Timelines

When illustrating race conditions, deadlocks, or multi-CPU interleavings, the model must follow authentic LKML mailing list conventions:

- **No ASCII Table Borders**: Do not use vertical bars (`|`), crosses (`+`), or markdown table syntax.
- **Whitespace Column Alignment**: Align CPUs in distinct horizontal columns separated by whitespace:
  ```
  CPU 0                               CPU 1
  -----                               -----
  foo_lock();
  list_add(&item->list, &head);
                                      bar_lock();
                                      item = list_first_entry(...);
  kfree(item);
                                      item->val = 1; // UAF
  ```
- **Flexible Representation**: Choose whatever format best explains the specific problem: prose only, code snippet only, multi-CPU timeline diagram, or both if needed to ground the race in code. Do not force an artificial either/or choice if both together make the defect clearer.
- **Non-Concurrency Exclusion**: Never use multi-column timelines for single-threaded bugs (leaks, buffer overflows, null dereferences on error paths).

---

### 2.5 Direct Severity Alignment (`SeveritySession`)

`SeveritySession` will no longer paraphrase or summarize severity definitions. Instead:
- `system_prompt(&self)` will directly return the compiled-in text of `kernel/severity.md` via `crate::prompt_bundle::kernel_severity_guide()`.
- `initial_user_prompt(&self)` will supply the verified defect title, description, and locations, instructing the model to assign a severity level following the calibration guidance in the system prompt and return JSON with `severity` and `severity_explanation`.

---

## 3. Canonical Few-Shot Examples for Prompt Grounding

The revised `ReportSession` will include five generalized canonical examples reflecting real-world LKML bug reporting styles:

1. **Resource Leak (Paired Allocation & Missed Free)**:
   Shows a 14-line snippet displaying `p = kzalloc(...)` followed by an error branch where cleanup is omitted, without carets, formatted with verbatim tab indentation.
2. **Concurrency Race / UAF Across CPUs**:
   Demonstrates a clean whitespace-delimited 2-column LKML timeline showing interleaving between a release thread and a worker thread, with no table borders or pipes.
3. **Pure Prose Defect (No Code Snippet)**:
   Demonstrates a defect where prose alone describes an unhandled state-machine condition clearly, showing the model that code snippets are not mandatory.
4. **Localized Relational / Signedness Bug (Targeted Snippet with Carets)**:
   Demonstrates an unsigned comparison against `< 0`, using carets strictly under the offending operator, with comments respecting the 75-column limit.
5. **Deadlock / Lock Order Inversion Timeline**:
   Demonstrates a clear multi-CPU AB-BA deadlock sequence using whitespace columns.

---

## 4. Implementation Plan

1. Commit this design document to `designs/DESIGN_BUG_REPORT_PROMPTS_AND_SEVERITY_ALIGNMENT.md`.
2. Update `SeveritySession` in `src/workflows/linux_bug.rs` to use `crate::prompt_bundle::kernel_severity_guide()` directly as the system prompt without rephrased instructions.
3. Update `ReportSession` in `src/workflows/linux_bug.rs` (system prompt, user prompt, and examples) implementing the systematic prompt rules and generalized canonical examples.
4. Update and expand unit tests in `src/workflows/linux_bug.rs` to assert the revised directives (no carets for missing code, paired allocation rules, LKML whitespace columns, line length discipline).
5. Verify with `make check-pr` and commit the code change with standard git conventions.
