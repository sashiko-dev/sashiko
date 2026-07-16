# Sashiko

![Sashiko Logo](static/logo.png)

[![Linux Foundation](https://img.shields.io/badge/Linux%20Foundation-Project-blue.svg)](https://www.linuxfoundation.org/)

> **Sashiko** (刺し子, literally "little stabs") is a form of decorative reinforcement stitching from Japan. Originally used to reinforce points of wear or to repair worn places or tears with patches, here it represents our mission to reinforce the Linux kernel through automated, intelligent patch review.

Sashiko is an agentic Linux kernel code review system. It uses a set Linux kernel-specific prompts and a special protocol to review proposed Linux kernel changes. Sashiko can ingest patches from mailing lists or local git. It's fully self contained (doesn't use any external agentic cli tools) and can work with various LLM providers.

If you are a kernel maintainer, please see our [Guide for Kernel Maintainers](MAINTAINERS_GUIDE.md) for information on interacting with Sashiko.

## Quality of reviews

Sashiko is not perfect, but in our measurements the quality of reviews is high:
in our tests sashiko was able to find 53.6% (with Gemini 3.1 Pro) of bugs based on unfiltered last 1000 upstream commits with Fixed: tags.
In some sense, it's already above the human level given that 100% of these bugs made it through human-driven code reviews and were accepted to the main tree.
The rate of false positives is harder to measure, but based on limited manual reviews it's well within 20% range and the majority of it is a gray zone.

Please, note that as with any other LLM-based tools, Sashiko's output is probabilistic: it might find or not find bugs (or find other bugs) with the same input.

## Features

- **Automated Ingestion**: Monitors mailing lists (`lore.kernel.org`), GitHub PRs, and GitLab MRs for new patch submissions.
- **Manual Ingestion**: Can ingest patches from local git repositories or specific PRs/MRs.
- **Forge Integration** *(Experimental)*: Automatic PR/MR review via GitHub and GitLab webhooks. This feature is unofficial and unsupported — use at your own risk.
- **Self-contained**: Doesn't depend on 3rd-party tools and works with multiple LLM providers (Gemini, Claude, and GitHub Copilot CLI are currently supported).
- **Web interface and CLI**: Provides a web interface for monitoring and a CLI tool for local development. Email support will be added soon.

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
8.  **Stage 8: Deduplication and Consolidation.** Consolidates feedback from stages 1-7, merges duplicates, and groups overlapping issues.
9.  **Stage 9: Concern/dismissed-concern conflict resolution.** Compares consolidated concerns against consolidated dismissed concerns and keeps only concerns that survive concrete code-based conflict checks.
10. **Stage 10: Verification and severity estimation.** Validates the remaining concerns, filters false positives, and estimates severity.
11. **Stage 11: Report generation.** Converts confirmed findings into a polite, standard, inline-commented LKML email reply.


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

- **Rust**: Version 1.90 or later.
- **Git**: For managing the repository and kernel tree.
- **LLM Provider API Key**: Access to an LLM provider (e.g., Google's Gemini or Anthropic's Claude).

## Installation

### From crates.io

```bash
cargo install sashiko
```

### From source

#### 1.  **Clone the repository**:
```bash
git clone --recursive https://github.com/sashiko-dev/sashiko.git
cd sashiko
```
*Note: The `--recursive` flag is important to initialize the `linux` kernel source submodule.*

#### 2.  **Configuration**:
Copy `Settings.toml` to customize your configuration. For a full reference of every
setting, see the [Configuration Reference](docs/configuration.md). The default `Settings.toml` includes sections for:
*   **Database**: SQLite database path (`sashiko.db`).
*   **NNTP**: Server details and groups to monitor.
*   **AI**: Provider and model selection.
*   **Server**: API server host and port.
*   **Git**: Path to the reference kernel repository.
*   **Review**: Concurrency and worktree settings.
*   **Forge** *(Experimental)*: GitHub/GitLab webhook integration (optional, unsupported). See forge setup guides below.
        *   When enabling `[forge]`, the NNTP (mailing list) ingestor is disabled by default. If you need to monitor both, set `disable_nntp = false` in the `[forge]` section of your config.
*   **Subsystems**: Map file patterns to subsystems for targeted reviews (optional). Works with both mailing lists (recipient matching) and git forges (file path matching).

#### Configuring the LLM Provider

Sashiko supports multiple LLM providers. To get started with the default
(Gemini):

```bash
cp docs/examples/Settings.example.toml Settings.toml
export LLM_API_KEY="your-api-key-here"
```

When installed with `cargo install`, create the user config with:

```bash
sashiko init
```

This writes `~/.config/sashiko.toml` by default. Use `sashiko init --print`
to print the template or `sashiko init --path <file>` to choose a different
location.

For Claude, Claude Code CLI, GitHub Copilot CLI, AWS Bedrock, Vertex AI,
Kiro CLI, Devin CLI, and OpenAI-compatible endpoints, see the
[LLM Provider Configuration Guide](docs/llm-providers.md).

#### 3.  **Build**:
```bash
cargo build --release
```

## Usage

Sashiko requires a configuration file to run (usually `~/.config/sashiko.toml` or `./Settings.toml`). You can initialize a default configuration with:

```bash
sashiko init
```

Sashiko can be run in two modes: **Local Review** (standalone, no daemon required) or as a **Daemon** (for automated monitoring, web UI, and team integration).

### 1. Local Review (Recommended)

Sashiko can review your patches locally without starting the daemon, sending emails, or updating the database.

Run it from the Linux source checkout containing the commits to review:

```bash
# Review the latest commit
sashiko review

# Review a range of commits
sashiko review HEAD~3..HEAD
```

This mode:
* Does not start the daemon or open the Sashiko database.
* Does not fetch or configure git remotes.
* Uses a temporary scratch clone for patch application, leaving your source checkout and its git metadata untouched.

For local review, Sashiko loads settings from `./Settings.toml` if it exists in the current directory, otherwise from `~/.config/sashiko.toml`. Use `--settings <path>` to point to a specific settings file.

### 2. Daemon Mode

The daemon is responsible for monitoring mailing lists (NNTP), managing the database, and coordinating the AI review process. It also provides a Web UI and an API.

To start the daemon:

```bash
sashiko
```

(Or from source: `cargo run`, or via Nix: `nix run github:sashiko-dev/sashiko`)

#### Web Interface

Once the daemon is running, you can access the Web UI. The daemon will print the URL to access it from localhost.

### 3. CLI (Interacting with the Daemon)

The CLI tool `sashiko-cli` allows you to interact with a running Sashiko daemon.

```bash
# Submit patches to the running daemon
sashiko-cli submit HEAD~3..HEAD

# Check review status
sashiko-cli show latest
```

(From source: `cargo run --bin sashiko-cli -- [OPTIONS] [COMMAND]`, or via Nix: `nix profile add github:sashiko-dev/sashiko`)

For the full command reference, see the [CLI Reference](docs/sashiko-cli.md).

## Benchmarking

Sashiko includes a benchmark tool to evaluate review quality against known
bugs. See the [Benchmarking Guide](docs/benchmarking.md) for setup and
usage.

## Communication

We welcome contributions and feedback through two main channels:

*   **GitHub:** Feel free to use GitHub issues for bug reports and feature requests, and submit Pull Requests for code changes.
*   **Mailing List:** Join us at `sashiko@lists.linux.dev` (archived at [lore.kernel.org](https://lore.kernel.org/sashiko)) for Sashiko-related announcements and broader AI-review discussions, including general feedback, architectural ideas, and specific prompt discussions. Automated patch reviews are sent from and should be replied to `sashiko-reviews@lists.linux.dev`.

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

### Gemini CLI Skills

For users of the [Gemini CLI](https://github.com/google/gemini-cli), we provide specialized skills to automate development workflows:

- **`review-pr`**: Performs deep, scrutinizing code reviews against `GEMINI.md` and design documents. Detects relevant design files automatically and generates categorized findings with ready-to-paste diffs.
- **`sashiko-feature`**: A meta-skill for implementing new features. It handles design document matching, codebase investigation, and ensures adherence to SOLID/DRY principles in Rust, while iteratively running `make` checks.

#### Installing Skills

To install these skills in your local workspace:

```bash
gemini skills install ./skills/review-pr.skill --scope workspace
gemini skills install ./skills/sashiko-feature.skill --scope workspace
/skills reload
```

For users of other agent interfaces (e.g., OpenCode, Claude Code), we recommend following your interface's specific settings to symlink or copy the skill configurations (the `SKILL.md` and `references/` files) into your agent's custom instruction path.

## License

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
