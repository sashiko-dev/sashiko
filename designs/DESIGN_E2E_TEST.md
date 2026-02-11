# Design: End-to-End Integration Test

## Objective
To implement a stable, deterministic, and fast end-to-end integration test that verifies the full `sashiko` pipeline without relying on external services (SSO, Real Gemini API) or network connectivity.

## Components

### 1. Mock Git Repository
The test should not clone huge kernels. Instead, it should create a small, local git repository on the fly.
*   **Setup**: `git init`, create a dummy file, commit, create a branch, add a patch (simulating the "Fix" or "Feature").
*   **Advantage**: Creating a new dummy repo is much faster than cloning the Linux repo (gigs of contents and millions of commits)

### 2. Mock Gemini Server (Replayer)
Using the trace captured via the [Gemini Capture Design](DESIGN_GEMINI_CAPTURE.md), we spin up a mock server that replays responses.
*   **Tool**: `wiremock` (Rust crate) is already used in `tests/e2e_gemini_mock.rs` and is suitable.
*   **Logic**:
    *   Match incoming requests (roughly) to the captured requests.
    *   Return the corresponding captured response.
    *   *Simplification*: If deterministic matching is hard (due to non-deterministic fields), match on the *intent* or just replay in sequence if the test flow is linear.

### 3. Test Runner
A Rust integration test (under `tests/`) that orchestrates the flow.

## Test Workflow

1.  **Setup Phase**:
    *   Create a temporary directory `temp_dir`.
    *   Initialize a mock git repo in `temp_dir/repo`.
    *   Start the `MockServer` with the loaded trace data.
    *   Configure `sashiko` (via `Settings` struct or config file) to:
        *   Use `GEMINI_BASE_URL` pointing to the `MockServer`.
        *   Use `repo_path` pointing to `temp_dir/repo`.

2.  **Execution Phase**:
    *   Start `sashiko` in a background task or thread.
    *   Send the API request: `POST /api/submit` with `{"type": "local", "path": "..."}` or `{"type": "remote", "repo": "file://..."}`.

3.  **Verification Phase**:
    *   Poll the `sashiko` status API (or check the output channel/DB).
    *   Assert that the review result matches expectations (e.g., "LGTM", specific findings).
    *   Assert that the expected number of tokens were "consumed" (mocked).

## Benefits
*   **Speed**: Runs in seconds.
*   **Stability**: No flake due to API quotas, network issues, or model non-determinism.
*   **Coverage**: Tests the parsing, prompt construction, worker queue, and API handling code paths.
