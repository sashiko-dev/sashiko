# Prompt Customization

## Configuration

Set prompts directory in `Settings.toml`:

```toml
[prompts]
# Local path
directory = "./my-prompts"

# Remote Git URL (cached locally)
directory = "git://github.com/org/prompts.git"
```

## Stage Management

Sashiko uses a multi-stage review process (1-9 default). Customize via `stages.toml` in your prompts directory:

```toml
# Disable a stage
[[stages]]
number = 7
enabled = false

# Add custom stage
[[stages]]
number = 10
name = "Performance"
instruction_file = "custom/perf.md"
supporting_files = ["patterns.md"]
```

### Default Stages
1. **Analyze goal**: Architectural intent.
2. **Implementation**: High-level correctness.
3. **Control flow**: Execution paths.
4. **Resource management**: Memory and handles.
5. **Locking**: Concurrency safety.
6. **Security**: Vulnerability audit.
7. **Hardware**: Device-specific logic.
8. **Verification**: Severity assessment.
9. **Report**: Final summary generation.

## Template Variables

Define in `Settings.toml`:

```toml
[prompts.variables]
project = "Linux Kernel"
subsystem = "network"
```

Use in markdown files: `Review for {{project}}, focusing on {{subsystem}}.`

### Built-in Variables
- `{{date}}`: Current date.
- `{{year}}`: Current year.

## Custom Directory Structure

```text
my-prompts/
├── stages.toml          # Custom stage config
├── stages/              # Stage markdown files
│   ├── 01-goal.md
│   └── ...
├── technical-patterns.md # Supporting context
└── tool.md               # Tool instructions
```

## Examples

### Custom Configurations
```toml
# Security-focused
[prompts]
directory = "./security-prompts"
[prompts.variables]
focus_area = "memory safety and input validation"

# Performance-focused
[prompts.variables]
focus_area = "algorithmic complexity and cache efficiency"

# Subsystem-specific (Git remote)
[prompts]
directory = "git://github.com/myorg/networking-prompts.git"
[prompts.variables]
subsystem = "networking"
```

### Advanced stages.toml
```toml
# Reorder: Run security (6) before control flow (3)
[[stages]]
number = 3
instruction_file = "stages/06-security.md"

[[stages]]
number = 6
instruction_file = "stages/03-control-flow.md"

# Add custom analysis
[[stages]]
number = 10
name = "Performance"
instruction_file = "custom/performance.md"
supporting_files = ["custom/perf-patterns.md"]
```
