# Sashiko

![Sashiko Logo](static/logo.png)

[![Linux Foundation](https://img.shields.io/badge/Linux%20Foundation-Project-blue.svg)](https://www.linuxfoundation.org/)

> **Sashiko** (刺し子, literally "little stabs") is a form of decorative reinforcement stitching from Japan. Originally used to reinforce points of wear or to repair worn places or tears with patches, here it represents our mission to reinforce the Linux kernel and other critical open-source systems software through automated, intelligent patch review.

Sashiko is a self-contained, agentic code review system built primarily for the Linux kernel, with dedicated profiles for GCC, QEMU, Zephyr RTOS, and Sashiko itself. It combines project-specific prompts, source-verified code notes, and a multi-stage verification protocol to review proposed changes locally from your git checkout or automatically from mailing lists and git forges.

- **Linux Kernel (`sashiko.dev`):** Browse live kernel patch reviews and tracked bugs at [sashiko.dev](https://sashiko.dev), and see the [Guide for Kernel Maintainers](projects/linux/MAINTAINERS_GUIDE.md) for configuring mailing list tracking, maintainer notes, and email policies.
- **Mailing List:** Join [`sashiko@lists.linux.dev`](https://lore.kernel.org/sashiko) for announcements, feedback, and discussions (automated review replies use `sashiko-reviews@lists.linux.dev`).
- **Community & Press:** Read what kernel maintainers and the media say in [Sashiko in the Press](PRESS.md).

## Supported Projects

The Linux kernel is Sashiko's primary focus and most mature target (`--project linux` is the default everywhere). Additional projects are supported via `--project <project>` (or `[project] kind` in `Settings.toml`):

| Project | `--project` | Status | Public Instance | Capabilities |
| :--- | :--- | :--- | :--- | :--- |
| **Linux Kernel** | `linux` | **Production** | [sashiko.dev](https://sashiko.dev) | Local review, `lore.kernel.org` mailing lists, bug tracking, codebase scanning |
| **GCC** | `gcc` | **Official pilot** | [gcc.sashiko.dev](https://gcc.sashiko.dev) | Local review, `gcc-patches@gcc.gnu.org` mailing list, bug tracking |
| **Sashiko** | `sashiko` | **Production** | [sashiko.sashiko.dev](https://sashiko.sashiko.dev) | Local review, GitHub pull requests, bug tracking |
| **QEMU** | `qemu` | **Pilot** | None | Local review, mailing lists (self-hosted) |
| **Zephyr RTOS** | `zephyr` | **Pilot** | None | Local review |

> [!NOTE]
> **Bringing Sashiko to Your Project:** Interested in piloting Sashiko for another open-source project? Please reach out to Roman Gushchin (`roman.gushchin@linux.dev`) or [`sashiko@lists.linux.dev`](https://lore.kernel.org/sashiko).

## Quick Start (Local Patch Review)

The easiest way to use Sashiko is `sashiko review`, which reviews commits directly in your local git checkout (defaulting to the Linux kernel) without running a daemon or database.

### 1. Install

Requires **Rust 1.90+**, **Git**, and an **LLM Provider API Key**.

```bash
cargo install sashiko
```

### 2. Configure

Initialize a configuration file (`~/.config/sashiko.toml` by default, or `./Settings.toml` in the working directory) and set your API key:

```bash
sashiko init
export LLM_API_KEY="your-api-key-here"
```

Gemini is used by default. For Claude, OpenAI-compatible endpoints, AWS Bedrock, Vertex AI, Claude Code CLI, GitHub Copilot CLI, Kiro CLI, Devin CLI, or goose, see the [LLM Provider Configuration Guide](docs/llm-providers.md).

### 3. Review Your Patches

Run `sashiko review` from inside your Linux kernel checkout (or pass `--project <gcc|qemu|zephyr|sashiko>` in another supported repository):

```bash
# Review the latest Linux kernel commit
sashiko review

# Review a range of commits (pass --report-preexisting to also report pre-existing bugs)
sashiko review HEAD~3..HEAD
sashiko review --report-preexisting

# Report only the one or two pre-existing bugs closely related to the change
sashiko review --report-related-preexisting [--related-preexisting-min-severity high]

# Review commits in another supported project (gcc, qemu, zephyr, sashiko)
sashiko review --project gcc

# Continue a review cut short by a quota stop, a timeout or Ctrl-C
sashiko review HEAD~3..HEAD --resume

# Combine all exploratory analysis stages into a single pass to save tokens
sashiko review --cheap
```

A local review records each request and the model's response under `$XDG_DATA_HOME/sashiko/`. `--resume` replays them, so the run sends the model only the requests the interrupted one never had answered. Without it, and without `[ai] response_cache`, every run sends every request to the model.

Local review reads each commit in the range from git, without a scratch clone or patch application, and leaves your working tree untouched.

> [!IMPORTANT]
> **Data Privacy & API Costs:** Sashiko sends patch data and relevant surrounding files/history from your repository to your configured LLM provider. Ensure you are authorized to share this code with your chosen provider and monitor your token usage and billing, as multi-stage reviews can incur significant API costs. The Sashiko authors assume no liability for data transmission or API charges.

## How It Works & Review Quality

### Review Quality

Evaluated against historical commits whose bugs had originally passed human review and been merged upstream (see the [Benchmarking Guide](docs/benchmarking.md)):

| Project | Benchmark Suite | Sashiko | Model | Effort | Bugs Detected |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Linux Kernel** | [`projects/linux/benchmarks/benchmark.json`](projects/linux/benchmarks/benchmark.json) | `v0.9.0` | Gemini 4 Argon | `high` | **67.5%** |
| **GCC** | [`projects/gcc/benchmarks/benchmark.json`](projects/gcc/benchmarks/benchmark.json) | `v0.7.0` | Gemini 4 Argon | `high` | **58.5%** |

Based on manual sampling of Linux kernel reviews, the false-positive rate is under **20%** (mostly gray-area concerns). As with any LLM-based tool, Sashiko's output is probabilistic and may vary across runs on the same input.

### Multi-Stage Review Pipeline

Sashiko evaluates patches through specialized parallel analysis stages followed by sequential consolidation stages, guided by a first-party three-layer prompt architecture under `projects/<project>/prompts/`: automatic code notes generated and verified directly against the target source tree (currently built for Linux, GCC, and Sashiko), human-curated maintainer notes for subsystem preferences and policies, and universal review procedures.

**Analysis stages** (shown for the default Linux kernel pipeline; `goal`, `implementation`, and `execution-flow` always run, while the planning stage selects the rest unless overridden via `--stages`, and non-Linux profiles adapt domain stages to their codebase):

- **`goal`:** Architectural flaws, UAPI breakages, and conceptual correctness.
- **`implementation`:** Whether code matches the commit message, undocumented side-effects, and API contract violations.
- **`execution-flow`:** Logic errors, missing return checks, unhandled error paths, and off-by-one errors.
- **`resources`:** Memory leaks, use-after-free (UAF), double frees, and object lifecycles across queues, timers, and workqueues.
- **`locking`:** Concurrency issues, deadlocks, RCU rule violations, and thread-safety.
- **`security`:** Buffer overflows, OOB reads/writes, TOCTOU races, and uninitialized memory leaks.
- **`hardware`:** Register accesses, DMA mapping, memory barriers, and state machine constraints.

**Consolidation stages** (run in sequence):

1. **`verification`:** Consolidates concerns and dismissed concerns across analysis stages,
   emits well-justified findings and dismissals directly, and routes contested or speculative candidates to `inconclusive_concerns`.
2. **`post-verification`:** Runs parallel per-finding verification stages (`post-verification-1` .. `post-verification-10`) with repository tools to validate or disprove `inconclusive_concerns`.
3. **`report`:** Formats confirmed findings into a standard inline-commented review report (LKML format for mailing lists, or pull-request review comments for git forges).

## Documentation

- **[Guide for Kernel Maintainers](projects/linux/MAINTAINERS_GUIDE.md):** Mailing list tracking, maintainer notes, baseline detection, and email delivery options.
- **[Sashiko in the Press](PRESS.md):** Quotes from Linux kernel maintainers on LKML and press coverage.
- **[LLM Provider Configuration](docs/llm-providers.md):** Setup instructions for Gemini, Claude, OpenAI, Bedrock, Vertex AI, and CLI providers.
- **[Running the Daemon (Server Mode)](docs/daemon.md):** Building from source, monitoring mailing lists (NNTP) and forges, Web UI, `sashiko-cli`, and server ACL security.
- **[Configuration Reference](docs/configuration.md):** Complete reference for `Settings.toml`, `projects/<project>/mailing_lists.toml`, Patchwork integration, and environment variables.
- **[CLI Reference (`sashiko-cli`)](docs/sashiko-cli.md):** Submitting patches and querying status on a running Sashiko daemon.
- **[Forge Setup Guide](docs/FORGE_SETUP.md):** Webhook integration for [GitHub](docs/GITHUB_SETUP.md) (used in production on `sashiko.sashiko.dev`; third-party setups experimental), [GitLab](docs/GITLAB_SETUP.md), and [Webhook Security](docs/WEBHOOK_SECURITY.md).
- **[Reviewing Zephyr](docs/zephyr-review.md)** *(Pilot)*: Local review of Zephyr RTOS commits with `--project zephyr`.
- **[Benchmarking Guide](docs/benchmarking.md):** Evaluating review accuracy against historical bugs.
- **[Contributing Guide](CONTRIBUTING.md):** DCO sign-off, local verification (`make check-pr`), and the Sashiko-for-Sashiko review workflow.

## Acknowledgments

Special thanks to Chris Mason, whose [review-prompts](https://github.com/masoncl/review-prompts) project powered Sashiko's initial launch and shaped our early review workflow. While Sashiko has since replaced `review-prompts` with its own code notes and maintainer notes, the project owes a great deal to that foundation.

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
