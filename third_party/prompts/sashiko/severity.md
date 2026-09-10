# Sashiko Severity Levels

Assign severity from the demonstrated consequence, triggering path, and
reachability. State that reasoning before the label.

## Critical

- unauthenticated remote code execution or credential disclosure;
- reviewing attacker-selected local/internal resources through a new reachable
  path;
- irreversible corruption or deletion of repositories, patches, or durable
  review state across users.

## High

- authenticated or commonly reachable data corruption;
- reviewing or publishing results for the wrong base, head, repository, or PR;
- a daemon-wide deadlock, persistent outage, or unbounded paid-model usage;
- a reliable webhook-authentication bypass or broadly exposed secret.

## Medium

- a recoverable failed review, leaked task/process/worktree, duplicate work, or
  bounded resource exhaustion;
- a compatibility regression affecting a supported configuration or provider;
- incorrect retry, quota, or state reporting with operational impact.

## Low

- a real but minor usability, diagnostic, documentation, or cold-path
  inefficiency issue with limited operational effect.

Severity is determined by demonstrated impact and reachability, not reviewer
confidence. If the triggering path cannot be established after reading the
relevant code, do not report the concern as a finding. For an established
defect, state any residual uncertainty separately; do not lower or raise its
severity merely because reviewer confidence differs. For this profile, this
rule supersedes generic stage prose that caps speculative findings at Medium
or requires an unestablished concern to be retained.
