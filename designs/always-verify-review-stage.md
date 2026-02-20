# Always Proceed to Verification Stage

## Status
Proposed

## Context
The current review process in `sashiko` consists of three sequential stages:
1. **Exploration**: The agent brainstorms potential failure modes (hypotheses) without reading files.
2. **Verification**: The agent uses tools to prove or disprove the hypotheses generated in the first stage, guided by technical patterns.
3. **Reporting**: confirmed findings are consolidated into a final report.

In the current implementation of `Worker::run` (in `src/worker/mod.rs`), there is a shortcut logic: if the agent submits an empty list of hypotheses during the **Exploration** stage, the process skips the **Verification** stage and jumps directly to **Reporting**.

```rust
// src/worker/mod.rs:350
if hypotheses_len == 0 {
    info!("No hypotheses submitted. Skipping to Reporting.");
    current_stage = ReviewStage::Reporting;
    // ...
}
```

The goal is to change this behavior so that the agent *always* goes through the **Verification** stage, even if no initial hypotheses were generated. This ensures a more thorough review by forcing the agent to apply technical patterns even if it didn't immediately see potential issues.

## Proposed Changes

### 1. Logic Change in `Worker::run`
Modify the transition logic in `ReviewStage::Exploration` to always transition to `ReviewStage::Verification`.

### 2. Prompt Enhancement in `PromptRegistry`
Add a new prompt variant in `src/worker/prompts.rs` for the **Verification** stage when no hypotheses were provided. The current prompt assumes there are hypotheses to verify:

> "Now, using your available tools, systematically verify each hypothesis from the brainstorming phase."

A new prompt, e.g., `get_verification_instructions_no_hypotheses`, will be introduced:

> "You didn't find any potential issues during brainstorming. However, I'd like you to double check the code by systematically applying the technical patterns listed below. If you discover any issues during this deeper research, include them in the `verifications` list."

## Alternatives Considered

### Alternative A: Keep the jump, but improve Exploration prompt
One could argue that if Exploration found nothing, Verification is likely to find nothing too. We could try to make the Exploration prompt even more "aggressive". However, the brainstorming phase is intentionally limited (no file reads), so it's prone to missing context-heavy bugs.

### Alternative B: Merge Exploration and Verification
Combining brainstorming and verification into a single "Research" phase. This would simplify the state machine but would lose the "thinking first" benefit of the current two-stage approach.

## Chosen Solution
The chosen solution is to **Always transition to Verification**.

- **Pros**: Ensures consistency and thoroughness. The agent is forced to "look harder" using technical patterns even if it was initially confident.
- **Cons**: Might increase token usage for very clean patches that really have no issues.

### Implementation Details:
1.  **`src/worker/prompts.rs`**: Add `get_verification_instructions_no_hypotheses()`.
2.  **`src/worker/mod.rs`**: Update the `match current_stage` block in `run()` to remove the jump to `Reporting` and instead use the appropriate prompt based on `hypotheses_len`.
