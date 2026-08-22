# Sashiko Review Profile

This profile adds project-specific guidance for reviewing Sashiko's Rust
daemon, forge integrations, Git operations, persistence, and AI-provider
boundaries.

The existing review binary can select it explicitly with:

```text
review --prompts third_party/prompts/sashiko [other arguments]
```

Daemon configuration for selecting this directory is separate from the
profile and is not introduced here.

The profile is local and deterministic. Loading it does not contact an AI
provider or any external service; model calls occur only when a review runs.

The current review engine retains several kernel-oriented stage descriptions.
`project-context.md` identifies Sashiko as the target and limits those examples
to cases that actually apply to this Rust service. Making every stage
project-neutral is intentionally left to a separate refactor.
