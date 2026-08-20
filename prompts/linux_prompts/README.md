# Linux Kernel Prompt Review Workflow

This directory contains prompt templates and guidelines for Sashiko's automated review workflow for Linux kernel prompt modifications.

## Review Stages

1. **Stage 1: Factual & Guideline Constraints (`stage1_factual_constraints.md`)**
   Evaluates prompt text to ensure it contains only factual, domain-specific information. Ensures prompt instructions do not require unsupported actions (such as compiling code or web searching) and do not contain trivial programming language explanations.

2. **Stage 2: Codebase Verification (`stage2_codebase_verification.md`)**
   Validates claims against the latest Linux kernel source code (Linus tree HEAD). Verifies cited function signatures, struct layouts, locking requirements, and invariants. If verification is not possible, does not raise false alarms.

3. **Stage 3: Index & Placement Verification (`stage3_index_placement.md`)**
   Ensures prompt additions and modifications adhere to the prompt repository structure (`api/` for widely used API usage rules, `subsystems/` for subsystem internal implementations, and `generic/` for output formats, severity, and core workflow policies) and are correctly registered in the root `index.md`.

4. **Stage 4: Concern Aggregation & Report Generation (`stage4_report_generation.md`)**
   Synthesizes concerns raised across all stages and produces a constructive, polite, plain-text report matching Sashiko's LKML review conventions.
