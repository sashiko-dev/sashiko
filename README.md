# Sashiko

![Sashiko Logo](static/logo.png)

> **Sashiko** (刺し子, literally "little stabs") is a form of decorative reinforcement stitching from Japan. Originally used to reinforce points of wear or to repair worn places or tears with patches, here it represents our mission to reinforce the Linux kernel through automated, intelligent patch review.

Sashiko is an agentic Linux kernel code review system. It uses a set Linux kernel-specific prompts and a special protocol to review proposed Linux kernel changes. Sashiko can ingest patches from mailing lists or local git. It's fully self contained (doesn't use any external agentic cli tools) and can work with various LLM providers.

## Quality of reviews

Sashiko is not perfect, but in our measurements the quality of reviews is high:
in our tests sashiko was able to find 53.6% (with Gemini 3.1 Pro) of bugs based on unfiltered last 1000 upstream commits with Fixed: tags.
In some sense, it's already above the human level given that 100% of these bugs made it through human-driven code reviews and were accepted to the main tree.
The rate of false positives is harder to measure, but based on limited manual reviews it's well within 20% range and the majority of it is a gray zone.

Please, note that as with any other LLM-based tools, Sashiko's output is probabilistic: it might find or not find bugs (or find other bugs) with the same input.

## Features

- **Automated Ingestion**: Monitors mailing lists (using `lore.kernel.org`) for new patch submissions.
- **Manual Ingestion**: Can ingest patches from a local git repository.
- **Self-contained**: Doesn't depend on 3rd-party tools and can work with various LLM providers (Gemini and Claude are currently supported).
- **Web interface and CLI**: Provides a web interface and a CLI tool. Email support will be added soon.

## Prompts

Sashiko uses a multi-stage review protocol to evaluate patches thoroughly from multiple perspectives, mimicking a team of specialized reviewers.

### Review Stages
1.  **Stage 1: Analyze commit main goal.** Focuses on the big picture, architectural flaws, UAPI breakages, and conceptual correctness.
2.  **Stage 2: High-level implementation verification.** Verifies if the code matches the commit message claims, checking for missing pieces, undocumented side-effects, and API contract violations.
3.  **Stage 3: Execution flow verification.** Traces C code execution flow, checking for logic errors, missing return checks, unhandled error paths, and off-by-one errors.
4.  **Stage 4: Resource management.** Analyzes memory leaks, use-after-free (UAF), double frees, and object lifecycles across queues, timers, and workqueues.
5.  **Stage 5: Locking and synchronization.** Investigates concurrency issues, deadlocks, RCU rule violations, and thread-safety.
6.  **Stage 6: Security audit.** Audits for buffer overflows, OOB reads/writes, TOCTOU races, and information leaks (like copying uninitialized memory).
7.  **Stage 7: Hardware engineer's review.** Specifically reviews driver and hardware code for correct register accesses, DMA mapping, memory barriers, and state machine constraints.
8.  **Stage 8: Verification and severity estimation.** Consolidates feedback from stages 1-7, deduplicates concerns, and attempts to logically prove/disprove findings to minimize false positives.
9.  **Stage 9: Report generation.** Converts confirmed findings into a polite, standard, inline-commented LKML email reply.

Also Sashiko is using per-subsystem and generic prompts, initially developed by Chris Mason:

*   [**review-prompts**](https://github.com/masoncl/review-prompts)

## Important Disclaimers

Before using Sashiko, please be aware of the following:

### 1. Data Privacy and Code Sharing
Sashiko operates by sending patch data and potentially extensive portions of the Linux kernel git history to your configured Large Language Model (LLM) provider.
*   **What is shared:** This may include not just the patch being reviewed, but also related commits, file contents, and other context from the configured kernel repository to provide the LLM with sufficient context.
*   **Your responsibility:** You must ensure you are authorized and comfortable sharing this code and data with the third-party LLM provider.
*   **Liability:** The authors of Sashiko assume no responsibility for any consequences regarding data privacy, confidentiality, or intellectual property rights resulting from the transmission of this data.

### 2. Operational Costs
Running an automated review system like Sashiko can be computationally expensive and may incur significant API costs.
*   **Cost factors:** The total cost depends heavily on the volume of patches reviewed, the complexity of individual patches, and the pricing model of your chosen LLM provider and specific model.
*   **Monitoring:** It is the user's sole responsibility to monitor token usage and billing. While Sashiko may provide usage estimates, these are approximations and should not be relied upon for billing purposes.
*   **Liability:** The authors of Sashiko are not responsible for any financial costs, fees, or unexpected charges incurred by the use of this software.

## Prerequisites

- **Rust**: Version 1.86 or later.
- **Git**: For managing the repository and kernel tree.
- **LLM Provider API Key**: Access to an LLM provider (e.g., Google's Gemini or Anthropic's Claude).

## Setup

1.  **Clone the repository**:
    ```bash
    git clone --recursive https://github.com/rgushchin/sashiko.git
    cd sashiko
    ```
    *Note: The `--recursive` flag is important to initialize the `linux` kernel source submodule.*

2.  **Configuration**:
    Copy `Settings.toml` to customize your configuration. The default `Settings.toml` includes sections for:
    *   **Database**: SQLite database path (`sashiko.db`).
    *   **NNTP**: Server details and groups to monitor.
    *   **AI**: Provider and model selection.
    *   **Server**: API server host and port.
    *   **Git**: Path to the reference kernel repository.
    *   **Review**: Concurrency and worktree settings.

    ### Configuring the LLM Provider

    Sashiko supports multiple LLM providers (e.g. `gemini`). You must configure the provider and model in `Settings.toml`. There are no default values, so please set them explicitly.

    Example `Settings.toml` configuration for Gemini:

    ```toml
    [ai]
    provider = "gemini"
    model = "gemini-3.1-pro-preview"
    # Optional settings
    # max_input_tokens = 950000
    # temperature = 1.0
    ```

    You can also configure settings via environment variables using the `SASHIKO` prefix and double underscores for nesting (e.g., `SASHIKO_AI__PROVIDER=gemini`).

    **Important**: You must set the `LLM_API_KEY` environment variable with your provider's API key.
    ```bash
    export LLM_API_KEY="your_api_key_here"
    ```

    ### Claude Setup

    Sashiko supports Anthropic's Claude models via the Claude API.

    **Get an API key**: https://console.anthropic.com/

    **Configure environment**:
    ```bash
    export ANTHROPIC_API_KEY="sk-ant-..."
    # Or use the generic key (LLM_API_KEY serves as fallback):
    export LLM_API_KEY="sk-ant-..."
    ```

    **Update Settings.toml**:
    ```toml
    [ai]
    provider = "claude"
    model = "claude-sonnet-4-6"
    max_input_tokens = 950000

    [ai.claude]
    prompt_caching = true
    ```

    **Features**:
    - Automatic prompt caching (5-minute TTL) reduces costs for repeated context
    - Full tool/function calling support for git operations
    - Automatic retry logic for rate limits and API overload
    - 1M context window for claude-sonnet-4-6 (use max_input_tokens = 950000 for safety margin)

    ### AWS Bedrock Setup

    Sashiko supports AWS Bedrock via the Converse API, which works with any Bedrock-hosted model (Claude, Llama, Mistral, etc.).

    **Prerequisites**: Enable model access in the [AWS Bedrock console](https://console.aws.amazon.com/bedrock/) for your desired model and region.

    **Configure AWS credentials** using any standard method:
    ```bash
    # Option 1: Environment variables
    export AWS_ACCESS_KEY_ID="..."
    export AWS_SECRET_ACCESS_KEY="..."
    export AWS_REGION="us-east-1"

    # Option 2: AWS CLI profile (~/.aws/credentials)
    aws configure
    ```

    **Update Settings.toml**:
    ```toml
    [ai]
    provider = "bedrock"
    model = "us.anthropic.claude-sonnet-4-6-20250514-v1:0"
    max_input_tokens = 950000

    [ai.bedrock]
    region = "us-east-1"  # Optional, falls back to AWS SDK defaults
    ```

    **Features**:
    - Uses the Converse API — works with any Bedrock-hosted model
    - No API key needed — uses standard AWS IAM authentication
    - Supports cross-region inference profiles (e.g., `us.anthropic.claude-*`)
    - Full tool/function calling support for git operations

3.  **Build**:
    ```bash
    cargo build --release
    ```

## Usage

Sashiko consists of two main components: the **Daemon** and the **CLI**.

### 1. Daemon

The daemon is responsible for monitoring mailing lists, managing the database, and coordinating the AI review process. It also provides a Web UI and an API for the CLI.

To start the daemon:

```bash
cargo run
```
### 2. CLI

The CLI allows you to interact with the running Sashiko daemon from your terminal.

To run the CLI:

```bash
cargo run --bin sashiko-cli -- [COMMAND]
```

**Commands:**

- **`submit [INPUT]`**: Submit a patch or range for review.
  - `INPUT` can be a file path (mbox), a commit SHA, or a range (e.g., `HEAD~3..HEAD`).
  - If `INPUT` is omitted and stdin is piped, it reads an mbox from stdin.
  - If `INPUT` is omitted and stdin is a terminal, it defaults to the current `HEAD`.
- **`status`**: Show the current server status and queue statistics.
- **`list [FILTER]`**: List recent patchsets.
  - `FILTER` can be a status (e.g., `pending`, `failed`, `reviewed`) or a search term.
- **`show [ID]`**: Show detailed information about a patchset and its AI review.
  - `ID` defaults to `latest`.

### 3. Getting Sashiko to Review Your Kernel Patch Series Locally

Sashiko is highly effective for reviewing your own changes during local kernel
development. To get the most out of it:

1.  **Configure `Settings.toml`**: Set `git.repository_path` to the absolute
    path of the repository you are actively developing in.
    ```toml
    [git]
    repository_path = "/home/user/src/linux"
    ```
2.  **Submit for Review**: Use the CLI to submit your recent commits. Since
    the daemon is looking at the same directory, you can use relative references
    like `HEAD` or ranges.
    ```bash
    # Review the latest 3 commits in your local dev tree
    sashiko-cli submit HEAD~3..HEAD
    ```

### 4. Web Interface

Once the daemon is running, you can access the Web UI, the daemon will print the
URL to access it from localhost.

## Contributing

This project uses the Developer Certificate of Origin (DCO). All contributions must include a `Signed-off-by` line to certify that you wrote the code or have the right to contribute it.

You can automatically add this line by using the `-s` flag when committing:

```bash
git commit -s
```

## Development

This project was built using Gemini CLI. If you're using other development agents, make sure they follow the guidance in GEMINI.md.
Please, make sure your code is working before sending PR. Make sure it can be built without warnings, all tests pass, run cargo fmt and clippy.
If you're changing AI-related parts, please, run at least several code reviews.
Development got much faster these days, but testing is as important as ever.

## License

[![Linux Foundation](https://img.shields.io/badge/Linux%20Foundation-Project-blue.svg)](https://www.linuxfoundation.org/)

Copyright The Linux Foundation and its contributors. All rights reserved.

The Linux Foundation has registered trademarks and uses trademarks. For a list of trademarks of The Linux Foundation, please see our [Trademark Usage page](https://www.linuxfoundation.org/trademark-usage/).

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
