# Prompt Customization

> **Experimental / Unsupported:** Custom prompt configuration is experimental
> and provided as-is. Bugs and issues with the Sashiko ingestor, reviewer, or
> core tooling can and should still be filed. However, we will not provide
> support for debugging custom prompts or troubleshooting token consumption
> caused by custom prompt configurations.

Sashiko's multi-stage review pipeline loads its prompts from a
configurable directory. You can override prompt content, reorder or
disable stages, define template variables, and point to a remote prompt
repository -- all through `Settings.toml`.

For the `[prompts]` configuration reference, see
[Configuration Reference](configuration.md). For tool customization,
see [AI Tools](tools.md).

## Prompts directory

Set the prompts directory in `Settings.toml`:

```toml
[prompts]
# Local path (absolute or relative to working directory)
directory = "./my-prompts"

# Remote Git repository (cloned and cached locally)
directory = "git://github.com/org/prompts.git"

# HTTP(S) URL (downloaded and cached locally)
directory = "https://example.com/prompts"
```

Remote sources are cached under `.sashiko-cache/prompts/` using the MD5
hash of the URL as directory name. On cache hit the local copy is reused
without re-fetching.

When no directory is configured, Sashiko defaults to
`third_party/prompts/kernel`.

## Stage management

Sashiko uses an 11-stage review pipeline by default. Customize it via a
`stages.toml` file in your prompts directory:

```toml
# Disable a stage
[[stages]]
number = 7
enabled = false

# Add a custom stage
[[stages]]
number = 12
name = "Performance"
instruction_file = "custom/perf.md"
supporting_files = ["patterns.md"]
```

### Default stages

| Stage | Name | Focus |
|-------|------|-------|
| 1 | Analyze goal | Architectural intent and design flaws |
| 2 | Implementation | High-level correctness |
| 3 | Control flow | Execution paths and logic errors |
| 4 | Resource management | Memory, handles, lifetimes |
| 5 | Locking | Concurrency and synchronization |
| 6 | Security | Vulnerability audit |
| 7 | Hardware | Device-specific logic |
| 8 | Deduplication | Consolidate overlapping concerns |
| 9 | Conflict resolution | Reconcile concerns vs dismissed concerns |
| 10 | Verification | Severity estimation and false-positive filtering |
| 11 | Report | LKML-friendly output generation |

## Convention over configuration

If a custom `directory` is provided, Sashiko resolves prompt files using
naming conventions before falling back to hardcoded defaults. You do not
need a `stages.toml` if your files follow the standard patterns.

### Implicit stage overrides

The registry looks for files in a `stages/` sub-directory matching the
pattern `{number:02}-*.md`:

- `stages/01-goal.md` replaces the built-in Stage 1 instruction.
- `stages/05-concurrency.md` replaces the built-in Stage 5 instruction.

### Standard supporting files

Specific stages look for these filenames by default:

- **Stage 3** -- `callstack.md`, `technical-patterns.md`
- **Stage 5** -- `subsystem/locking.md`
- **Stage 10** -- `false-positive-guide.md`, `severity.md`
- **Stage 11** -- `inline-template.md`

### Automatic context injection

Files in specific sub-directories are gathered and injected into the
AI's shared knowledge base:

- `subsystem/*.md` -- selected during Phase 0 based on patch relevance.

## Template variables

Define variables in `Settings.toml`:

```toml
[prompts.variables]
project = "Linux Kernel"
subsystem = "network"
```

Use `{{variable_name}}` syntax in prompt markdown files:

```markdown
Review for {{project}}, focusing on {{subsystem}}.
```

### Built-in variables

| Variable | Value |
|----------|-------|
| `{{date}}` | Current date (`YYYY-MM-DD`) |
| `{{year}}` | Current year (`YYYY`) |

User-defined variables are substituted before built-in variables and
take precedence on name collision.

## Custom directory structure

```text
my-prompts/
├── stages.toml            # Stage pipeline configuration
├── stages/                # Stage instruction files
│   ├── 01-goal.md
│   ├── 02-implementation.md
│   └── ...
├── technical-patterns.md  # Supporting context
├── false-positive-guide.md
├── severity.md
├── inline-template.md
├── subsystem/
│   ├── locking.md
│   └── ...
└── tool.md                # Tool usage instructions
```

## Examples

### Security-focused review

```toml
[prompts]
directory = "./security-prompts"
[prompts.variables]
focus_area = "memory safety and input validation"
```

### Subsystem-specific prompts from a git remote

```toml
[prompts]
directory = "git://github.com/myorg/networking-prompts.git"
[prompts.variables]
subsystem = "networking"
```

### Advanced stages.toml

```toml
# Swap stage instructions
[[stages]]
number = 3
instruction_file = "stages/06-security.md"

[[stages]]
number = 6
instruction_file = "stages/03-control-flow.md"

# Add custom analysis pass
[[stages]]
number = 12
name = "Performance"
instruction_file = "custom/performance.md"
supporting_files = ["custom/perf-patterns.md"]
```
