# Maintainer Notes and Code Notes Invariants

This guide covers `src/notes/`, `src/workflows/code_notes.rs`,
`projects/*/prompts/maintainers/*.md`, and `projects/*/prompts/notes/`.

Sashiko separates human maintainer policy (`<maintainer_notes>`) from
machine-extracted, source-verified codebase facts (`<code_notes>`). Changes to
either layer directly affect what every patch review flags or ignores.

## 1. Maintainer Notes (`projects/*/prompts/maintainers/*.md`, `src/notes/maintainer_notes.rs`)

### 1.1 Front Matter and Parsing (`MaintainerNote::parse`)
Every `maintainers/*.md` file must begin and close with `+++` TOML front matter
(`#[serde(deny_unknown_fields)]`: only `title`, `paths`, and `symbols` are
permitted), define a non-empty `title`, specify at least one entry in `paths`
or `symbols`, and contain a non-empty markdown body.
- **Note**: `test_shipped_maintainer_notes_parse` in `src/notes/maintainer_notes.rs`
  validates all shipped `maintainers/*.md` files deterministically during
  `cargo test`. Do not vibe-guess TOML syntax errors on shipped files unless a
  change weakens `MaintainerNote::parse` or bypasses the test.

### 1.2 Trigger Scoping (`paths` and `symbols` in `MaintainerNote::applies`)
Maintainer notes are selected deterministically per patch without an LLM
pre-screen stage. Overly broad triggers inject subsystem-specific policy into
unrelated kernel reviews:
- **`paths` patterns (`CompiledPattern`)**:
  - Directory prefixes must end with `/` (e.g., `"drivers/net/"`, `"fs/nfsd/"`)
    to match any file under that subtree.
  - Exact file paths or globs (e.g., `"*.rs"`, `"include/linux/bpf*.h"`) match
    specific files.
  - `"*"` matches **every patch in the repository** and is reserved for
    project-wide notes such as `general.md`. Flag any subsystem-specific
    maintainer note that puts `"*"` or an overly broad top-level directory in
    `paths`.
- **`symbols` patterns (`diff_identifiers_all`)**:
  - `symbols` matches identifiers on added/removed diff lines and `@@` hunk
    headers (either an exact identifier or a `<prefix>*` wildcard such as
    `"netdev_*"` or `"bpf_*"`).
  - Because `diff_identifiers_all` retains short and common tokens, flag overly
    generic symbol entries or wildcard prefixes (such as `"dev*"`, `"err"`,
    `"ret"`, `"lock*"`, `"flags"`, `"int"`, `"get_*"`, `"set_*"`) that appear in
    unrelated patches across the kernel.

### 1.3 Content Boundaries: Policy & Preferences vs. Code Facts
- **Preferences and policies only**: `<maintainer_notes>` record subjective
  maintainer expectations that cannot be derived from reading the source code:
  coding preferences, deprecated vs. preferred APIs, patch/tree/UAPI policies,
  and review priorities.
- **No volatile code facts**: Objective facts about how functions, locks,
  lifetimes, or refcounts currently work in the C/Rust source belong in
  source comments or generated `<code_notes>`, not hardcoded in
  `maintainers/*.md` where they go stale as the codebase evolves. Flag
  maintainer-note additions that hardcode volatile implementation details as
  facts instead of stating maintainer policy.

### 1.4 Suppression Rule Precision (Preventing Silent Bug Drops)
Because `<maintainer_notes>` are injected into reviews as authoritative policy,
an over-broad "do not report" bullet will cause Sashiko to silently drop real
bugs:
- **Narrow scoping required**: Every suppression rule must state the exact
  benign pattern to ignore and preserve genuine bug classes in adjacent code.
  For example, suppressing default-config build/Clippy noise in `rust.md` or
  `build.md` must explicitly preserve `CONFIG_*`-gated conditional-compilation
  errors, missing Kconfig dependencies, and broken `CONFIG_*`-gated rustdoc
  intra-doc links.
- **Defect to catch**: Flag any new or modified suppression bullet in
  `maintainers/*.md` (or `false-positive-guide.md`) whose wording is broad
  enough to blanket-suppress genuine runtime, concurrency, resource-leak,
  security, or conditional-compilation bugs.

### 1.5 Calibration for Maintainer Note Commits (No Unnecessary Benchmark Demands)
- Adding or updating a subsystem maintainer note in
  `projects/linux/prompts/maintainers/*.md` to record a maintainer's subjective
  subsystem preference, convention, or narrowly-scoped policy is a **policy
  configuration change** (analogous to `projects/linux/mailing_lists.toml`),
  not a global bug-detection heuristic change.
- Do **not** demand benchmark suite runs (`benchmarks/*.json`) or multi-paragraph
  commit-message essays when a commit adds or updates subsystem maintainer
  preferences in `projects/linux/prompts/maintainers/*.md` and its intent is
  self-explanatory. Only require re-review or benchmark validation when a prompt
  change alters global bug-detection heuristics or broadens cross-subsystem
  suppression rules (`general.md`, `false-positive-guide.md`,
  `linux_patch_review.rs`).

---

## 2. Code Notes Engine and Selection (`src/notes/`, `projects/*/prompts/notes/`)

### 2.1 Verbatim Evidence and Adversarial Verification (`src/notes/build.rs`)
- **Deterministic quote check (`RevisionFiles::check`)**: Every extracted `Fact`
  must cite a verbatim `evidence.quote` verified against the git blob at the
  pinned build revision (`--rev`), and every verifier verdict must have its
  quote checked by `check_verifier_evidence`.
- **Survival filter**: Only facts rated both `Confirmed` and `PatchRule` by the
  independent verifier stage may be synthesized into `units/<slug>.md` (dropping
  `Trivia`, `ExistingWeakness`, `Refuted`, and `Unverifiable` claims).

### 2.2 Snapshot Authority in Review Prompts (`<code_notes>`)
- Code notes are built from a pinned revision (`manifest.json` `revision`) while
  patches under review may target a newer baseline or refactor the code.
- **Invariant**: System prompts (`linux_system_prompt`, `sashiko_system_prompt`,
  and `select::render_block`) must frame `<code_notes>` as reference background
  from a fixed snapshot that is subordinate to the actual repository source at
  `Target Commit SHA` and `Baseline SHA`—never as an "absolute source of truth"
  that overrides the code in the repository.

### 2.3 Deterministic Selection & Bundle Hygiene (`src/notes/select.rs`, `build.rs`)
- **Version selection (`choose_set`)**: Selects the newest installed notes set
  whose `revision` is a git ancestor of the patch baseline (`git merge-base
  --is-ancestor`), falling back to the oldest set only when none is an ancestor.
- **Cross-unit symbol bounding (`select_symbol_notes`)**: Symbol-based cross-unit
  note loading must ignore common symbols (`MAX_SYMBOL_UNIT_FREQ`) and
  non-specific single-word tokens (`is_specific_symbol`) and focus injected
  notes down to only the bullets whose subject names the matched symbols.
- **Sidecar exclusion (`build.rs`)**: `.facts.json` audit sidecars must never be
  embedded into the compiled prompt bundle in `build.rs` or committed to git.

## Checklist for Diffs Touching Maintainer Notes or Code Notes

1. **Trigger Specificity**: Do `paths` and `symbols` in `maintainers/*.md`
   narrowly match only the intended subsystem without `"*"` (outside
   `general.md`) or generic symbol prefixes?
2. **Policy vs. Code Fact**: Does the maintainer note state human preferences
   and policies rather than hardcoding volatile C/Rust implementation facts?
3. **Suppression Precision**: Could any "do not report / ignore" bullet in
   `maintainers/*.md` cause Sashiko to ignore real bugs (such as
   `CONFIG_*`-gated build failures, races, or resource leaks)?
4. **Snapshot Subordinate to Repo**: Do `<code_notes>` framing and verification
   rules keep repository source code at `Target Commit SHA` / `Baseline SHA`
   authoritative over note claims?
