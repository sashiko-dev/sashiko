# Sashiko

![Sashiko Logo](static/logo.png)

> **Sashiko** (刺し子, literally "little stabs") is a form of decorative reinforcement stitching from Japan. Originally used to reinforce points of wear or to repair worn places or tears with patches, here it represents our mission to reinforce the Linux kernel through automated, intelligent patch review.

Sashiko is an automated system designed to assist in the review of Linux kernel patches. It ingests patches from mailing lists, analyzes them using AI-powered prompts, and provides feedback to help maintainers and developers ensure code quality and adherence to kernel standards.

## Features

- **Automated Ingestion**: Monitors mailing lists (using `lore.kernel.org`) for new patch submissions.
- **Manual Ingestion**: Can ingest patches from a local git repository.
- **AI-Powered Review**: Utilizes LLM models to analyze patches against subsystem-specific guidelines.
- **Self-contained**: Doesn't depend on 3rd-party tools and can work with various LLM providers.

## Prompts

Sashiko relies on the set of carefully crafted prompts to guide the AI in its reviews. These prompts were initially created by Chris Mason and are developed by the community of developers in a separate repository:

*   [**review-prompts**](https://github.com/masoncl/review-prompts)

This repository is included as a submodule in the `third_party/review-prompts/` directory.

## Prerequisites

- **Rust**: Version 1.86 or later.
- **Git**: For managing the repository and kernel tree.
- **Gemini API Key**: Access to Google's Gemini models (other models can be used, but it's not tested and might require some minimal code changes)

## Setup

1.  **Clone the repository**:
    ```bash
    git clone --recursive https://github.com/rgushchin/sashiko.git
    cd sashiko
    ```
    *Note: The `--recursive` flag is important to initialize the `linux` kernel source and `review-prompts` submodules.*

2.  **Configuration**:
    Copy `Settings.toml` to customize your configuration. The default `Settings.toml` includes sections for:
    *   **Database**: SQLite database path (`sashiko.db`).
    *   **NNTP**: Server details and groups to monitor.
    *   **AI**: Provider (Gemini), model selection, and token limits.
    *   **Server**: API server host and port.
    *   **Git**: Path to the reference kernel repository.
    *   **Review**: Concurrency and worktree settings.

    You can also configure settings via environment variables using the `SASHIKO` prefix and double underscores for nesting (e.g., `SASHIKO__SERVER__PORT=8081`).

    **Important**: You must set your Gemini API key. This is typically done via an environment variable, depending on the underlying client library, or potentially in a secrets file if supported. Ensure your environment has the necessary credentials loaded.

3.  **Build**:
    ```bash
    cargo build --release
    ```

## Usage

To start the application:

```bash
cargo run --release
```

This will start the Sashiko daemon, which will begin ingesting and reviewing patches based on your configuration.

## Benchmarking

Sashiko includes a benchmarking suite to evaluate the effectiveness of its AI reviews against known kernel bugs. The benchmark uses a dataset of commits (`benchmark.json`) where bugs were fixed, and checks if Sashiko can identify the issues in the original buggy commits.

1.  **Start the Sashiko Server**:
    Ensure the main application is running to handle ingestion requests.
    ```bash
    cargo run --release
    ```

2.  **Ingest Benchmark Data**:
    In a separate terminal, run the ingestion tool to submit the benchmark commits to the running server.
    ```bash
    cargo run --release --bin ingest_benchmark
    ```
    This reads `benchmark.json` and submits each commit to the local Sashiko instance for review. Wait for the server to process these reviews (check the server logs).

3.  **Run Evaluation**:
    Once the reviews are complete, run the evaluation tool. This compares the AI's findings against the ground truth in `benchmark.json`.
    ```bash
    cargo run --release --bin benchmark_review
    ```
    The results will be printed to the console and saved to `benchmark_results.json`.

## License

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
