# Design: Just-In-Time (JIT) Prompt Loading & Instruction Decoupling

## Philosophy: "One Master, One Task"
The primary reason AI agents fail in complex workflows is **Instruction Interference**. When an agent is given a 2,000-line "Core Protocol" containing 10 numbered tasks, it attempts to track its progress through that list while simultaneously trying to obey the programmatic instructions from the `Worker`. 

This design follows the philosophy that the AI should never be responsible for managing its own state machine. The `Worker` owns the state; the AI owns the technical execution of the **current step only**.

## Problem Statement
*   **Procedural Noise**: `review-core.md` contains legacy "Task 1, Task 2" labels that conflict with the Worker's "Stage 1, Stage 2" logic.
*   **Pattern Overload**: Loading every technical pattern at Turn 1 causes the agent to force-fit code into patterns instead of thinking creatively.
*   **Cache Invalidation**: Modifying early system prompts or removing messages voids the "Prefill" cache, leading to high latency and costs.

## Proposed Solution

### 1. Additive-Only Context
To preserve **Gemini/Claude Prefix Caching**, we will use an additive-only strategy.
*   We **NEVER** delete or reorder messages in the `AiRequest` history once they are sent.
*   Context is "grown" by appending new reference documents (e.g., `technical-patterns.md`) as the review transitions into deeper stages.

### 2. The Result Submission Tool (`cmd_submit_results`)
The agent will be given a single, simplified tool to return its structured findings at the end of each turn.
*   **Tool**: `cmd_submit_results(json_text: String)`
*   **Logic**: The worker parses this string based on the current stage's expectations.

### 3. Stage-Specific Injection
The `Worker` manages the instructions and examples for each phase:

*   **PHASE A: Exploration**
    *   **Context**: `review-philosophy.md` (no patterns yet).
    *   **Instruction**: "Brainstorm every way this code could fail. Focus on edge cases and races."
    *   **Expected Output**: JSON list of hypotheses.
    *   **Example**: `{"hypotheses": [{"problem": "...", "impact": "..."}]}`

*   **PHASE B: Verification**
    *   **Context**: Append `technical-patterns.md` and subsystem guides.
    *   **Instruction**: "Systematically verify the brainstormed hypotheses using these patterns."
    *   **Expected Output**: JSON list of confirmed/disproven findings with proof.
    *   **Example**: `{"verifications": [{"evidence": "...", "is_confirmed": true}]}`

*   **PHASE C: Reporting**
    *   **Context**: Append `severity.md` and `inline-template.md`.
    *   **Instruction**: "Consolidate findings and grade severity using the Escalation Protocol."
    *   **Expected Output**: Final summary and `review_inline` (formatted bottom-up).

### 4. Context Compression Strategy (Future-Proofing)
*If* we reach context limits, the `Worker` will use a **Outcome-First Pruning** strategy:
1.  **Keep**: The original patch, the current instructions, and the **JSON Results** returned via `cmd_submit_results`.
2.  **Compress**: The internal "thinking" turns and tool execution logs between the submission points.
*(Note: This is documented for future implementation but not part of the immediate release.)*

## Implementation Details

### `PromptRegistry` Refactor
*   `get_core_philosophy()`: The non-procedural replacement for `review-core.md`.
*   `get_stage_instruction(stage)`: Returns instructions + examples for the specific turn.

### `Worker` State Management
*   The worker will skip the instruction for Phase B if Phase A returns an empty list of hypotheses, jumping straight to the Reporting instruction without explicitly labeling turn numbers to the agent.

## Alternatives Considered

### 1. "The One-Shot Protocol"
*   **Idea**: Keep the current single-turn approach but improve the prompt.
*   **Cons**: Fails to stop the "bullshitting" effect where agents guess severity before they've even read the relevant code.

### 2. "Session Branching"
*   **Idea**: Start a new chat session for each stage.
*   **Cons**: **Breaks Caching.** Every new session forces the model to re-read the 50k token kernel context from scratch.

## Final Justification
This strategy forces **Rational Incrementalism**. The agent cannot skip to the "Reporting" phase because it hasn't been given the `severity.md` guidelines or the reporting template yet. It must perform the work in the order dictated by the `Worker`, while the "Additive-Only" logic ensures we maintain maximum performance through context caching.
