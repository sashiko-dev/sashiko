# Design: Programmatic Multi-Turn Review Strategy

## Problem Statement
Currently, Sashiko sends a single, comprehensive prompt to the AI agent ("do everything at once"). This approach is prone to:
1. **Confirmation Bias**: The agent identifies an issue and immediately commits to it, skipping a broader exploration of failure modes.
2. **Context Dilution**: The instruction to perform exploration, verification, and severity grading in one go often leads to the agent "skipping steps" to reach the final output format.
3. **Inconsistent Severity**: Severity is often assigned before the full impact is researched, leading to "severity inflation" or random guesses.

## Proposed Solution
Transition from a "one-shot" interaction to a programmatic, multi-turn orchestration led by the `Worker`. The review process will be broken into discrete stages, each enforced by a **Stage-Specific JSON Schema**.

## Interaction Stages

### Stage 1: Hypothesis Generation (The "Exploration" Phase)
* **Goal**: Force the agent to consider every possible way the patch could fail before verifying any of them.
* **Schema**: 
  ```json
  {
    "hypotheses": [{"id": 1, "description": "...", "potential_impact": "..."}],
    "exploration_complete": true
  }
  ```
* **Worker Logic**: Injects the exploration prompt. 
  * **Early Exit**: If `hypotheses` is empty and `exploration_complete` is `true`, the Worker jumps directly to Stage 3 (Reporting) to generate a "Clean" summary.

### Stage 2: Research & Verification (The "Deep Dive" Phase)
* **Goal**: Systematically prove or disprove hypotheses, and allow for serendipitous discovery of new issues.
* **Schema**:
  ```json
  {
    "verifications": [
      {"hypothesis_id": 1, "status": "confirmed/disproven", "proof": "..."},
      {"id": "serendipitous_N", "status": "confirmed", "problem": "...", "proof": "..."}
    ],
    "verification_complete": true
  }
  ```
* **Worker Logic**: Injects the verification prompt. The agent is explicitly told it can add new findings not listed in the exploration phase.
  * **Early Exit**: If all `verifications` have status `disproven` and no new findings are added, the Worker jumps to reporting with "no findings".

### Stage 3: Severity & Final Report (The "Reporting" Phase)
* **Goal**: Consolidate confirmed regressions and apply the `severity.md` Escalation Protocol.
* **Schema**: Existing `summary`, `findings`, and `review_inline` structure.
* **Worker Logic**: Injects the reporting prompt. 

## Technical Changes

### 1. `PromptRegistry` (`src/worker/prompts.rs`)
Added the `ReviewStage` enum to track the sequence.

### 2. `Worker` (`src/worker/mod.rs`)
* **Dynamic Schema Selection**: The `response_schema` in the loop is now determined by the current `ReviewStage`.
* **State Progression**: The `Worker` will transition stages when the respective `complete` flag is detected in the JSON response.
* **Parsing**: Use `serde_json` to validate and extract stage flags.

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
