# Integration Guide for review-prompts and GitHub Actions

## Overview

Claude officially supports GitHub-triggered code reviews through workflows. This document describes how to embed kernel review prompts into the [claude-code-action](https://github.com/anthropics/claude-code-action).

## Steps

These steps are adapted from [claude-code-action setup](https://github.com/anthropics/claude-code-action/blob/main/docs/setup.md) with additional detailed explanations:

1. Install the Claude app for your repository. You can visit [Marketplace](https://github.com/apps/claude) to install the APP for your repository. This repository may be one you created or cloned from another source.

2. Execute `/install-github-app` in Claude's interactive command line. This will guide you step-by-step through the app configuration process. Ultimately, it will generate and save a TOKEN for your project and create a basic workflow in `.github/workflows/claude.yml`. Alternatively, you can manually complete the above steps as described in the `claude-code-action` setup documentation.

3. Edit `.github/workflows/claude.yml` in your repository. Below is an example YAML configuration that differs slightly from the official [claude-code-action example](https://github.com/anthropics/claude-code-action/blob/main/examples/claude.yml). The key additions are the `Checkout prompts repo` step and the inclusion of custom `prompt` content. Additionally, we use `track_progress: true` to preserve `claude-code-action` related MCP services.

```
name: Claude Code

on:
  issue_comment:
    types: [created]
  pull_request_review_comment:
    types: [created]
  issues:
    types: [opened, assigned]
  pull_request_review:
    types: [submitted]

jobs:
  claude:
    if: |
      (github.event_name == 'issue_comment' && contains(github.event.comment.body, '@claude')) ||
      (github.event_name == 'pull_request_review_comment' && contains(github.event.comment.body, '@claude')) ||
      (github.event_name == 'pull_request_review' && contains(github.event.review.body, '@claude')) ||
      (github.event_name == 'issues' && (contains(github.event.issue.body, '@claude') || contains(github.event.issue.title, '@claude')))
    runs-on: ubuntu-latest
    permissions:
      contents: write
      pull-requests: write
      issues: write
      id-token: write
      actions: read # Required for Claude to read CI results on PRs
    steps:
      - name: Checkout repository
        uses: actions/checkout@v5
        with:
          fetch-depth: 1
      - name: Checkout prompts repo
        uses: actions/checkout@v5
        with:
          repository: 'masoncl/review-prompts'
          path: 'review'
      - name: Run Claude Code
        id: claude
        uses: anthropics/claude-code-action@v1
        with:
          claude_code_oauth_token: ${{ secrets.CLAUDE_CODE_OAUTH_TOKEN }}
          track_progress: true
          prompt: |
            Current directory is the root of a Linux Kernel git repository.
            Using the prompt `review/review-core.md` and the prompt directory `review`
            do a code review.
```

4. With the above YAML configuration, after creating a PR, you can trigger a code review by mentioning @claude in the PR comments. You can also modify the trigger conditions as needed. The code review results will be posted as comments in your PR.

