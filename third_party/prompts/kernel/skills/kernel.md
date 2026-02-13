---
name: kernel
description: Load anytime the working directory is a linux kernel tree, and always load it when you answer questions inside the kernel tree.  Linux Kernel knowledge, subsystem specific details, analysis, review, debugging protocols.  Read this anytime you're in the linux kernel tree
invocation_policy: automatic
---

## ALWAYS READ 
1. Load `{{KERNEL_REVIEW_PROMPTS_DIR}}/technical-patterns.md`

You consistently skip reading additional prompt files.  These files are
MANDATORY.  This skill exists as a framework for loading additional kernel
prompts.

## Configuration

The review prompts directory is configured during installation:
- **KERNEL_REVIEW_PROMPTS_DIR**: {{KERNEL_REVIEW_PROMPTS_DIR}}

This variable is set by the installation script when the skill is installed.

## Capabilities

### Patch Review
When asked to review a kernel patch, commit, or series of commits:
1. Load `{{KERNEL_REVIEW_PROMPTS_DIR}}/review-core.md`
2. Follow the complete review protocol defined there
3. Load subsystem-specific files as directed by review-core.md

### Debugging
When asked to debug a kernel crash, oops, warning, or stack trace:
1. Load `{{KERNEL_REVIEW_PROMPTS_DIR}}/debugging.md`
2. Follow the complete debugging protocol defined there
3. Use crash information as entry points into the code analysis

### Subsystem Context
When working on kernel code in specific subsystems, load the appropriate
context files from `{{KERNEL_REVIEW_PROMPTS_DIR}}/`:

1.  Always read `technical-patterns.md` before loading subsystem specific files

2. Read `{{KERNEL_REVIEW_PROMPTS_DIR}}/subsystem/subsystem.md` and load matching subsystem
   guides and critical patterns

## Semcode Integration

When available, use semcode MCP tools for efficient code navigation:
- `find_function` / `find_type`: Get definitions
- `find_callchain`: Trace call relationships
- `find_callers` / `find_calls`: Explore call graphs
- `grep_functions`: Search function bodies
- `diff_functions`: Identify changed functions in patches

## Output

- Patch reviews produce `review-inline.txt` when regressions are found
- Debug sessions produce `debug-report.txt` with analysis results
- Both outputs are formatted for the Linux kernel mailing list (plain text, 78 char wrap)
