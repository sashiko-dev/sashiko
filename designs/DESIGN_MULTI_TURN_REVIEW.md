# Design: Programmatic Multi-Turn Review Strategy

## Problem Statement
Currently, Sashiko sends a single, comprehensive prompt to the AI agent ("do everything at once"). This approach is prone to:
1. **Confirmation Bias**: The agent identifies an issue and immediately commits to it, skipping a broader exploration of failure modes.
2. **Context Dilution**: The instruction to perform exploration, verification, and severity grading in one go often leads to the agent "skipping steps" to reach the final output format.
3. **Inconsistent Severity**: Severity is often assigned before the full impact is researched, leading to "severity inflation" or random guesses.

## Proposed Solution
Transition from a "one-shot" interaction to a programmatic, multi-turn orchestration led by the `Worker`. The review process will be broken into discrete stages, each enforced by a **Stage-Specific Submission Tool**.

To preserve **Gemini/Claude Context Caching**, the AI `response_format` will remain static (Generic JSON or Text), while the structured output will be delivered via constant tool definitions.

## Bottom-Up Reasoning (Anti-Bullshit Protocol)
To prevent the agent from "anchoring" on a guess, all submission tools will require fields in an order that forces reasoning before conclusion:
1. **Evidence/Problem**: What is actually wrong?
2. **Mitigation/Suggestion**: How would we fix it? (Thinking about the fix often reveals the bug is minor).
3. **Justification**: Why does this meet a specific escalation gate?
4. **Severity/Summary**: The final label/overview.

## Interaction Stages

### Stage 1: Hypothesis Generation (`cmd_submit_exploration`)
* **Goal**: Brainstorm potential failure modes.
* **Tool Parameters**:
  - `hypotheses`: List of `{ problem_description, potential_impact }`.
  - `exploration_complete`: Boolean.

### Stage 2: Research & Verification (`cmd_submit_verification`)
* **Goal**: Systematically prove or disprove hypotheses.
* **Tool Parameters**:
  - `verifications`: List of:
    - `evidence`: Detailed trace or code proof.
    - `suggestion`: Potential fix.
    - `is_confirmed`: Boolean.
    - `hypothesis_id`: Link to stage 1.
  - `verification_complete`: Boolean.

### Stage 3: Severity & Final Report (`cmd_submit_report`)
* **Goal**: Final consolidation using the `severity.md` protocol.
* **Tool Parameters**:
  - `findings`: List of:
    - `problem`: The technical defect.
    - `suggestion`: The fix.
    - `severity_explanation`: Why it meets the escalation gate.
    - `severity`: The final label (**Low/Medium/High/Critical**).
  - `summary`: High-level overview of the patch (generated *after* findings).
  - `review_inline`: Final formatted text.

## Technical Changes

### 1. `PromptRegistry` (`src/worker/prompts.rs`)
* Updated prompts to explicitly command the use of the new submission tools.

### 2. `Worker` (`src/worker/mod.rs`)
* **Static Schema**: The `response_format` in `AiRequest` will be set to a static "Submission Schema" or standard `Text` mode to ensure the context prefix remains identical across all turns, maximizing cache hits.
* **Tool Interception**: The `Worker` will intercept calls to `cmd_submit_*` tools. These tools won't execute shell commands; they will serve as the "Return" statement for each stage.
* **State Progression**: Stages transition upon successful call of the respective submission tool.

## Benefits
* **Rigorous Discovery**: Hypotheses are documented before research begins.
* **Audit Trail**: The chat history contains the structured proof for every confirmed bug.
* **Structural Consistency**: Forcing JSON at every turn prevents the agent from reverting to conversational filler.

### 3. Tool Loop Handling
The existing loop detection logic will remain active across all stages to prevent the agent from getting stuck.

## Benefits
* **Higher Catch Rate**: Sequential thinking prevents the agent from rushing to a conclusion.
* **Accurate Severity**: Severity is assigned after the research is done, allowing for a relative comparison of all found bugs.
* **Reduced Hallucinations**: Forced "impossible in practice" checks in Stage 2 filter out speculative concerns.

## Cost Considerations
* **Increased Token Usage**: Each turn adds the full previous history to the new request.
* **Latency**: Multiple round-trips to the AI provider.
* **Mitigation**: Use of Gemini Context Caching to minimize the cost of the large system prompt and kernel context.
