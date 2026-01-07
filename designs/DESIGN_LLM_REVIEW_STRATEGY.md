# Design: LLM Review Strategy for Sashiko

## Overview
This document outlines the strategic approach for leveraging Large Language Models (LLMs) like Gemini 3 Pro within `sashiko` to perform high-quality, stable, and actionable code reviews for the Linux kernel. It complements `DESIGN_REVIEW_AGENT.md` (architecture) and `DESIGN_REVIEW_PROMPTS.md` (knowledge base) by focusing on prompt engineering techniques, stability heuristics, and quality assurance mechanisms.

## 1. Core Philosophy: The "Maintainer-in-the-Loop" Simulation
The LLM should not act as a mere text generator but as a simulated maintainer who:
1.  **Investigates**: Actively looks up context (definitions, history) before forming an opinion.
2.  **Verifies**: Uses tools to confirm assumptions (e.g., "does this function exist?").
3.  **Deliberates**: Reasons through the changes step-by-step (Chain-of-Thought).
4.  **Decides**: Provides a structured verdict based on evidence.

## 2. Advanced Prompting Techniques

### A. Chain-of-Thought (CoT) & Structured Reasoning
Instead of asking for a review directly, we force a multi-step reasoning process.
**Prompt Structure:**
> "You are reviewing a patch. Do not output the review yet. First, perform the following analysis steps:
> 1.  **Summary**: Briefly summarize what the patch claims to do.
> 2.  **Safety Check**: List potential security implications (buffer overflows, UAF).
> 3.  **Context Verification**: Identify external functions/structs modified. Do you need to see their definitions? (Use `read_file` if so).
> 4.  **Style Check**: Does it match kernel coding style?
> 5.  **Plan**: What specific files or lines do you need to inspect to verify correctness?"

### B. Few-Shot Prompting (Dynamic Injection)
We improve consistency by providing examples of *good* vs. *bad* reviews dynamically.
-   **Source**: `review-prompts/examples/` (e.g., `good-review-net.md`, `bad-review-style.md`).
-   **Technique**: If reviewing `net/`, inject `net` specific examples.
-   **Goal**: Align tone (terse, professional) and depth (technical, not nitpicky).

### C. Role-Based Persona Enforcement
The System Prompt must strictly define the persona to avoid "helpful assistant" chatter.
-   **Good**: "You are a Linux Kernel Maintainer. You are strict, precise, and care about stability and performance. You do not compliment code unless it's exceptional. You reject vague commit messages."
-   **Bad**: "You are a helpful AI assistant who helps coders."

## 3. Context Management & Token Efficiency

### A. The "Needle in a Haystack" Problem
The Linux kernel is too large for the context window. The worker must *retrieval-augment* itself.
-   **Initial Context**: Cover letter + Patch diffs (limited to N lines).
-   **Active Retrieval**: The Worker *must* use tools (`git_grep`, `read_file`) to fetch definitions of modified functions.
    -   *Rule*: "If a patch modifies `foo()`, you must read the definition of `foo()` unless it's trivial."
-   **Diff Context Expansion**: Allow the worker to request `git_diff -U20` if the default context is insufficient.

### B. Smart Pruning
-   **Ignore**: Documentation files (unless checking typos), `MAINTAINERS` updates (unless verifying logic), generated files.
-   **Focus**: `.c`, `.h`, `.rs`, `Kconfig`, `Makefile`.

## 4. Stability & Reliability Features

### A. Tool Use Enforcement
-   **Problem**: LLMs might hallucinate file content or API existence.
-   **Solution**:
    -   **Mandatory Verification**: Before claiming "function X does not exist", the worker must execute `git_grep X`.
    -   **Loop Prevention**: If the worker asks for the same file twice, interrupt and prompt: "You already have this. Proceed to analysis."

### B. Structured Output (JSON Mode)
For the final verdict, we rely on JSON to ensure machine-readability by `sashiko`.
-   **Schema**:
    ```json
    {
      "summary": "...",
      "score": 1-10,
      "verdict": "Reviewed-by" | "Acked-by" | "Naked-by" (Changes Requested),
      "findings": [
        { "file": "path/to/file", "line": 123, "severity": "High", "message": "..." }
      ]
    }
    ```
-   **Retry Logic**: If JSON parsing fails, feed the error back to the LLM: "Invalid JSON. Fix syntax and retry."

### C. Self-Correction Loop
If the LLM generates a review, we can run a second "Critic" pass (cheaper model or same model with different prompt).
-   **Prompt**: "Review this review. Does it hallucinate? Is the tone appropriate? Are the line numbers correct?"
-   **Action**: If the Critic flags issues, regenerate the review.

## 5. Linux Kernel Specific Tricks

### A. API Evolution Check
If the patch uses a deprecated API (e.g., `simple_strtol`), the worker should suggest the modern alternative (`kstrtol`) by referencing internal knowledge bases or previously ingested examples.

## 6. Limitations & Mitigation

### A. Compilation (The Missing Link)
-   **Limitation**: The worker cannot reliably compile the kernel (takes too long, requires huge dependencies/config).
-   **Mitigation**: Acknowledge this. "I have performed a static analysis. I cannot verify runtime behavior or compilation success."

### B. Hallucination
-   **Limitation**: Inventing struct members.
-   **Mitigation**: Strict "Evidence-Based" rule. "Quote the line of code from the context that supports your finding."

### C. Subjectivity
-   **Limitation**: Arguing about style.
-   **Mitigation**: Focus review on logic, concurrency, and security, avoiding subjective style debates unless they violate well-known kernel patterns.

## 7. Implementation Roadmap (Features)

1.  **Phase 1**: Basic Review (Text output, diff-only context).
2.  **Phase 2**: Tool-Assisted (Worker can `read_file`, `grep`).
3.  **Phase 3**: Structured JSON Output & Database Integration.
4.  **Phase 4**: Multi-turn "Critic" Loop.
