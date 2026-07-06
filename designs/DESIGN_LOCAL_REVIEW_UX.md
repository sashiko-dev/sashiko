# Sashiko Local Review UX Improvements Design

## Objective
Improve the user experience (UX) and responsiveness of the local review workflow in the `sashiko` CLI tool.
Address the big delay before the progress bar appears, make progress updates more frequent, and fix the rendering of findings.

## Current Issues
1. **Initial Delay:** There is a significant delay (often 10-30+ seconds) between "Running AI review for X patches" and the appearance of the progress bar. This is because the progress bar is initialized only when the AI Review Plan (consisting of the list of stages to run) is ready, which itself requires two sequential LLM API calls (Phase 0: Subsystem Pre-screening, and Phase 1: Planning / Stage selection).
2. **Infrequent Updates:** The progress bar only updates when a stage starts or finishes. Since a single stage can take 10-30 seconds (involving multiple tool calls/turns), the progress bar appears frozen for long periods.
3. **Broken Findings Output:** The final report prints empty findings (e.g., `[Medium]` with no description, file, or line number). This is caused by a schema mismatch: the CLI printing code looks for `problem_description` and flat `file`/`line` keys, whereas the AI worker returns findings conforming to a newer schema using `problem` and nested `locations`.
4. **Lack of Commit Context in Status:** The "Patch Status" section lists patches by index (e.g., `[1] present`) but does not display the commit subject or short SHA, making it hard to correlate the status back to the original commits.

## Proposed Changes

### 1. Planning/Pre-screening Progress Updates
We will introduce new progress events during Phase 0 and Phase 1, allowing the CLI to display status before the plan is resolved:
- **New `WorkerProgressEvent` variants:**
  ```rust
  pub enum WorkerProgressEvent {
      PreScreenStarted,
      PlanningStarted,
      ReviewStarted { planned_stages: Vec<u8> },
      StageStarted { stage: u8 },
      StageFinished { stage: u8 },
  }
  ```
- **New `ProgressEvent` variants:**
  ```rust
  pub enum ProgressEvent {
      ...
      AiReviewPreScreenStarted,
      AiReviewPlanningStarted,
      ...
  }
  ```
- **Console Progress Bar updates:**
  We will modify `render_progress` to support rendering states when `planned_stages` is empty but planning is in progress.
  - When pre-screening:
    `      Progress: [░░░░░░░░░░░░░░░░░░░░] | Pre-screening subsystem guides...`
  - When planning:
    `      Progress: [░░░░░░░░░░░░░░░░░░░░] | Planning review stages...`

### 2. Intra-Stage Progress (Turns/Interactions)
We will report turn-level progress from the LLM session runner. This will update the "Active" section of the progress bar to show the current interaction turn.
- **New `WorkerProgressEvent` and `ProgressEvent` variants:**
  ```rust
  WorkerProgressEvent::StageTurn { stage: u8, turn: usize, max_turns: usize }
  ProgressEvent::AiReviewStageTurn { stage: u8, turn: usize, max_turns: usize }
  ```
- **Console Progress Bar updates:**
  - Show the current turn for the active stage:
    `      Progress: [████░░░░░░░░░░░░░░░░] 20% | 2/10 stages | Active: Locking (turn 3/15)`

### 3. Fixing and Formatting Findings
We will align `print_finding` in `src/main.rs` with the correct JSON schema returned by the worker:
- **Correct Keys:**
  - Use `problem` instead of `problem_description`.
  - Parse `locations` (array of objects containing `file`, `line`, `function_or_symbol`, `code_snippet`, `why_this_location_matters`).
- **Improved Findings Layout:**
  - Print the severity block in color: Red for `Critical`/`High`, Yellow for `Medium`, Cyan for `Low`.
  - Loop over and format each location clearly.
  - Display the `severity_explanation` / reasoning if available.
  - Example Layout:
    ```
    [Medium] Memory leak in function X when condition Y is met.
      Locations:
      - path/to/file.c:123 (in function_name)
        Snippet: `problematic_code();`
        Reason:  This is where the newly allocated resource is dropped on the error path.
      Reasoning:
        1. Condition Y is met.
        2. The buffer is allocated but not freed before return.
    ```

### 4. Correlation of Patch Status
We will improve the `Patch Status` printout to display the commit subject and index:
- Store the commit subject when resolving patches.
- In `src/main.rs`, print:
  `  [1] present (current-tree) - bpf: selftests: add config for psi`
  instead of just:
  `  [1] present (current-tree)`

## Implementation Steps
1. **Phase 1: Update Progress Events & Worker**
   - Add new variants to `WorkerProgressEvent` and `ProgressEvent`.
   - Update `Worker::run` (in `src/worker/prompts.rs`) to emit events for Phase 0 and Phase 1 start.
   - Update `SessionRunner` to accept a callback or check where turn updates can be intercepted, and emit `StageTurn` events.
2. **Phase 2: Update CLI Progress Rendering**
   - Update the mapping from `WorkerProgressEvent` to `ProgressEvent` in `src/local_review.rs`.
   - Update `render_progress` in `src/main.rs` to handle pre-plan events and show turn counts.
3. **Phase 3: Fix Findings Output**
   - Update `print_finding` and helper types in `src/main.rs` to match the `problem` and `locations` schema.
4. **Phase 4: Enhance Patch Status Printing**
   - Update CLI to output commit subjects in the final status summary.
5. **Phase 5: Verification & Tests**
   - Compile the codebase and run tests (`make check-pr`).
