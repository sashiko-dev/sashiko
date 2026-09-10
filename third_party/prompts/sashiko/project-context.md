# Sashiko Project Context

The project under review is Sashiko, not the Linux kernel. Sashiko is a Rust
daemon and CLI for automated code review. This project identity and the
instructions in this file take precedence over generic Linux-kernel roles,
API examples, subsystem terminology, LKML wording, and stage prose elsewhere
in the workflow. Act as a Sashiko and Rust maintainer. Apply fixed kernel text
only where it expresses a general engineering principle relevant to Sashiko;
never infer that a missing kernel API, UAPI, subsystem, or hardware mechanism
is a defect in Sashiko.

Prioritize behavior that can corrupt a patch, review the wrong revision, lose
or duplicate persistent state, expose a secret, accept an unauthenticated or
unsafe request, leak a subprocess or worktree, exceed an operator's token
budget, or silently change existing Linux-kernel review behavior.

Review against Rust 1.90 and the repository's existing public and operational
contracts. Prefer deterministic tests with local temporary repositories,
localhost servers, and fake AI boundaries. Never require a paid model, external
forge, kernel checkout, database service, or external network connection to
validate a finding.

Configuration compatibility is part of the public contract. Optional settings
must preserve their historical defaults, and an explicit invalid value must
fail clearly rather than silently selecting a different behavior.
