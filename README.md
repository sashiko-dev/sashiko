# Sashiko

![Sashiko Logo](static/logo.png)

[![Linux Foundation](https://img.shields.io/badge/Linux%20Foundation-Project-blue.svg)](https://www.linuxfoundation.org/)

> **Sashiko** (刺し子, literally "little stabs") is a form of decorative reinforcement stitching from Japan. Originally used to reinforce points of wear or to repair worn places or tears with patches, here it represents our mission to reinforce the Linux kernel through automated, intelligent patch review.

Sashiko is a self-contained, agentic Linux kernel code review system. It uses kernel-specific prompts and a multi-stage verification protocol to review proposed changes locally from your git checkout or automatically from mailing lists (`lore.kernel.org`) and git forges.

- **Kernel Maintainers:** See the [Guide for Kernel Maintainers](MAINTAINERS_GUIDE.md) for configuring mailing list tracking, maintainer notes, and email policies.
- **Mailing List:** Join [`sashiko@lists.linux.dev`](https://lore.kernel.org/sashiko) for announcements, feedback, and discussions (automated review replies use `sashiko-reviews@lists.linux.dev`).
- **Community & Press:** Read what kernel maintainers and the media say in [Sashiko in the Press](PRESS.md).

> [!NOTE]
> **Other Open-Source Projects:** Sashiko is exploring covering other open-source projects beyond the Linux kernel. If you are interested in bringing Sashiko to your project, please reach out to Roman Gushchin (`roman.gushchin@linux.dev`).

## Quick Start (Local Patch Review)

The easiest way to use Sashiko is `sashiko review`, which reviews commits directly in your local Linux kernel checkout without running a daemon or database.

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

Run `sashiko review` from inside your Linux kernel checkout:

```bash
# Review the latest commit
sashiko review

# Review a range of commits (pass --report-preexisting to also report pre-existing bugs)
sashiko review HEAD~3..HEAD
sashiko review --report-preexisting
```

Local review uses a temporary scratch clone for patch application, leaving your working tree and git metadata untouched.

> [!IMPORTANT]
> **Data Privacy & API Costs:** Sashiko sends patch data and relevant surrounding files/history from your repository to your configured LLM provider. Ensure you are authorized to share this code with your chosen provider and monitor your token usage and billing, as multi-stage reviews can incur significant API costs. The Sashiko authors assume no liability for data transmission or API charges.

## How It Works & Review Quality

### Review Quality

In benchmarks against the last 1,000 unfiltered upstream commits with `Fixes:` tags, Sashiko (with Gemini 4 Argon, high reasoning effort) detected **67.3%** of bugs that had originally passed human review and been merged into mainline. Based on manual sampling, the false-positive rate is under **20%** (mostly gray-area concerns).

As with any LLM-based tool, Sashiko's output is probabilistic and may vary across runs on the same input.

### Multi-Stage Review Pipeline

Sashiko evaluates patches through specialized parallel analysis stages followed by sequential consolidation stages, guided by a first-party three-layer prompt architecture: automatic code notes generated and verified directly against the kernel source tree, human-curated maintainer notes for each subsystem's preferences and policies, and universal review procedures.

**Analysis stages** (run in parallel; `goal`, `implementation`, and `execution-flow` always run, while the planning stage selects the rest unless overridden via `--stages`):

- **`goal`** — architectural flaws, UAPI breakages, and conceptual correctness.
- **`implementation`** — whether code matches the commit message, undocumented side-effects, and API contract violations.
- **`execution-flow`** — logic errors, missing return checks, unhandled error paths, and off-by-one errors.
- **`resources`** — memory leaks, use-after-free (UAF), double frees, and object lifecycles across queues, timers, and workqueues.
- **`locking`** — concurrency issues, deadlocks, RCU rule violations, and thread-safety.
- **`security`** — buffer overflows, OOB reads/writes, TOCTOU races, and uninitialized memory leaks.
- **`hardware`** — register accesses, DMA mapping, memory barriers, and state machine constraints.

**Consolidation stages** (run in sequence):

1. **`verification`** — consolidates concerns and dismissed concerns across analysis stages,
   emits well-justified findings and dismissals directly, and routes contested or speculative candidates to `hard_cases`.
2. **`post-verification`** — runs parallel per-finding verification stages (`post-verification-1` .. `post-verification-10`) with repository tools to validate or disprove `hard_cases`.
3. **`report`** — formats confirmed findings into a standard inline-commented LKML review report.

## Documentation

- **[Guide for Kernel Maintainers](MAINTAINERS_GUIDE.md)** — mailing list tracking, maintainer notes, baseline detection, and email delivery options.
- **[Sashiko in the Press](PRESS.md)** — quotes from Linux kernel maintainers on LKML and press coverage.
- **[LLM Provider Configuration](docs/llm-providers.md)** — setup instructions for Gemini, Claude, OpenAI, Bedrock, Vertex AI, and CLI providers.
- **[Running the Daemon (Server Mode)](docs/daemon.md)** — building from source, monitoring mailing lists (NNTP) and forges, Web UI, `sashiko-cli`, and server ACL security.
- **[Configuration Reference](docs/configuration.md)** — complete reference for `Settings.toml`, `projects/<project>/mailing_lists.toml`, Patchwork integration, and environment variables.
- **[CLI Reference (`sashiko-cli`)](docs/sashiko-cli.md)** — submitting patches and querying status on a running Sashiko daemon.
- **[Forge Setup Guide](docs/FORGE_SETUP.md)** *(Experimental)* — webhook integration for [GitHub](docs/GITHUB_SETUP.md), [GitLab](docs/GITLAB_SETUP.md), and [Webhook Security](docs/WEBHOOK_SECURITY.md).
- **[Benchmarking Guide](docs/benchmarking.md)** — evaluating review accuracy against historical bugs.
- **[Contributing Guide](CONTRIBUTING.md)** — DCO sign-off, local verification (`make check-pr`), and the Sashiko-for-Sashiko review workflow.

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
