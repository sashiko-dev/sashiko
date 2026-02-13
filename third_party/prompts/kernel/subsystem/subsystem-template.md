# Subsystem Guide Format

This document describes the format used by subsystem `.md` files in this
directory. Use it when writing new guides or restructuring existing ones.

## Purpose

Each subsystem guide is a **knowledge reference** — it contains invariants,
API contracts, struct field semantics, and common bug patterns for a specific
kernel subsystem. It is loaded during review when the patch touches that
subsystem (see `subsystem.md` for the trigger table).

A subsystem guide is NOT:
- A workflow or analysis procedure (no TodoWrite steps, no "step 1, step 2")
- A checklist to follow mechanically
- A place for generic kernel knowledge covered by `../technical-patterns.md`

## File Structure

```
# <Name> Subsystem Details

## <Concept Section>

<Consequence paragraph>

<Rules, invariants, API details>

## <Concept Section>

...

## Quick Checks

<Short items not covered in detail above>
```

### Title

Always `# <Name> Subsystem Details`.

### Concept Sections

Organize by concept or topic, not by numbered patterns. Each section covers
one coherent area of the subsystem.

**Do not use inline pattern IDs** like `SCHED-001`, `NET-002`, etc. as section
headers. If a section covers what was previously a numbered pattern, it can
retain the ID as a sub-heading within the section (e.g., `### BT-001: Extent
Map Field Confusion`) for cross-reference continuity, but the section itself
should be named after the concept.

### Consequence Paragraph

Each section should open with 1-3 sentences explaining **what goes wrong** if
the rules in that section are violated. State the concrete consequence:
deadlock, use-after-free, data corruption, NULL dereference, silent wrong
behavior, etc. This gives the reader immediate context for why the section
matters.

Examples from existing guides:

> Accessing bio data fields on a bio that has no data buffers (e.g., discard,
> flush) causes a NULL pointer dereference.

> Incorrect PTE flag combinations cause data corruption (dirty data silently
> dropped), security holes (writable pages that should be read-only), and
> kernel crashes on architectures that trap invalid combinations.

> Using the wrong lock type for the execution context causes deadlocks
> (sleeping in atomic context), missed wakeups, or priority inversion.

### Rules and Details

After the consequence paragraph, present the actual rules, invariants, and
API details using:

- **Bullet points** for rules, invariants, and API descriptions. Use inline
  backticks for all function names, type names, field names, macros, and
  constants.

- **Tables** for reference data where multiple items share the same set of
  attributes. Always explain non-obvious column meanings in a paragraph before
  the table. Examples:
  - Operation → field validity mapping (block.md bio operations)
  - Flag → behavior mapping (mm.md GFP flags)
  - Variant → API/capability mapping (rcu.md RCU variants)
  - Intent → correct field (btrfs.md extent map fields)

- **Bold sub-headers** (`**Like this:**`) for sub-topics within a section.
  Use `###` sub-headings only when the sub-topic is substantial enough to
  warrant its own anchor.

- **Code examples** when the correct vs incorrect pattern is non-obvious or
  when the bug is a subtle ordering issue. Use `// CORRECT` and `// WRONG`
  comments. Only include examples where they genuinely clarify — not every
  section needs one.

- **ASCII diagrams** only when they clarify a spatial or temporal relationship
  that prose cannot convey efficiently (e.g., btrfs extent map layout with
  compressed vs uncompressed). Do not use diagrams for simple linear sequences.

- **Kernel source references** — include function names and file paths where
  the rule is enforced or can be verified: `see foo_bar() in path/to/file.c`.
  These let reviewers verify claims against the source.
  - **NEVER USE LINE NUMBERS:** these change over time and are not a useful way to
    find things in the source code.

- **Numbered lists** for sequential procedures or ordered lifecycles (e.g.,
  writeback tag lifecycle, RCU reclaim sequence).

### Common Mistakes and Bug Patterns

When a section covers a pattern where code commonly gets it wrong, explain
**why** the mistake is hard to catch. For example, btrfs extent map fields
are often confused because for uncompressed extents without partial references,
all three size fields are equal — the wrong field gives the right answer.

Use **`REPORT as bugs`** (bold) to flag specific high-signal patterns that
should always be reported when found:

> **REPORT as bugs**: Code that calls `call_rcu()` or `kfree_rcu()` on an
> object that is still reachable through an RCU-protected data structure.

### Quick Checks

The final section. Short bullet points for review pitfalls that are not
covered in detail by the sections above. Each item is **bold-named** with
a brief explanation:

> - **Lock drop and reacquire**: when a lock is dropped and retaken, verify
>   the code re-validates all protected state after reacquiring.

Do not repeat items already explained in detail above. Quick Checks is for
additional items that don't warrant their own section.

## What NOT to Include

- **Risk / When to check / Details boilerplate** — the old pattern format
  (`**Risk**: Use-after-free`, `**Details**: Check X`). The consequence
  paragraph replaces Risk, and the section content replaces Details.

- **TodoWrite workflow steps** — analysis procedures belong in agent prompts
  (e.g., `../agent/review.md`, `../callstack.md`), not in subsystem
  knowledge files.

- **Generic kernel knowledge** — topics like "don't sleep in atomic context"
  or "check return values" belong in `../technical-patterns.md`, not in every
  subsystem guide.

## Checklist for New Guides

1. Title is `# <Name> Subsystem Details`
2. Every section opens with a consequence paragraph
3. All function/type/field/macro names use backticks
4. Tables have column explanations when meanings aren't self-evident
5. No numbered pattern IDs as top-level headers
6. No Risk/Details/When-to-check boilerplate
7. No TodoWrite or workflow steps
8. Quick Checks section at the end (if applicable)
9. Code examples use `// CORRECT` / `// WRONG` labels
10. Added to `subsystem.md` trigger table
