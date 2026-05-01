# Design: Customizable Review Prompts, Stages, and Tools

## Goal

Allow users to customize the AI review pipeline without forking or modifying
core source code. Users can override prompt content, reorder or disable review
stages, define template variables for prompts, filter built-in AI tools, and
register custom shell-based tools -- all through TOML configuration.

## 1. Prompts Directory Resolution

The review pipeline loads prompts from a directory. By default this is
`third_party/prompts/kernel`, but users can override it via configuration or
CLI flag.

### 1.1 Resolution Sources (Priority Order)

1. CLI flag: `--prompts <PATH>`
2. `Settings.toml`: `[prompts] directory = "..."`
3. Default: `third_party/prompts/kernel`

### 1.2 Supported Directory Schemes

The `directory` value in `[prompts]` supports three schemes:

- **Local path**: Absolute or relative to the working directory.
  ```toml
  [prompts]
  directory = "/opt/my-prompts"
  # or
  directory = "./custom-prompts"
  ```

- **Git URL**: Cloned to `.sashiko-cache/prompts/{md5(url)}`.
  ```toml
  [prompts]
  directory = "git://github.com/org/review-prompts.git"
  # or
  directory = "https://github.com/org/review-prompts.git"
  ```

- **HTTP(S) URL**: Downloaded to `.sashiko-cache/prompts/{md5(url)}`.
  ```toml
  [prompts]
  directory = "https://example.com/prompts"
  ```

### 1.3 Caching Strategy

Remote prompts are cached locally under `.sashiko-cache/prompts/` using the
MD5 hash of the source URL as directory name. On cache hit, the local copy is
reused without re-fetching. Git clones will eventually support `git pull` to
update (currently marked as TODO).

### 1.4 Resolution Flow

```text
PromptRegistry::with_settings(settings)
  |
  +--> resolve_prompts_directory(directory_string)
  |      |
  |      +-- starts with http(s):// --> download_remote_prompts()
  |      |     cache in .sashiko-cache/prompts/{md5}
  |      |
  |      +-- starts with git:// or ends with .git --> clone_git_prompts()
  |      |     cache in .sashiko-cache/prompts/{md5}
  |      |
  |      +-- otherwise --> local path resolution
  |            absolute: use as-is
  |            relative: join with cwd
  |
  +--> load_stages_config(base_dir, config_path)
  |      reads stages.toml from base_dir (or custom path)
  |
  +--> copy variables from PromptsSettings
```

### 1.5 Expected Directory Structure

```text
prompts/
├── stages.toml                 # Stage pipeline configuration
├── review-core.md              # Core review identity/workflow
├── technical-patterns.md       # Shared pattern catalog
├── false-positive-guide.md     # Verification guidance
├── severity.md                 # Severity calibration
├── inline-template.md          # Report format template
├── subsystem/
│   ├── networking.md
│   ├── locking.md
│   └── ...
└── stages/
    ├── 01-analyze-goal.md
    ├── 02-implementation.md
    ├── 03-control-flow.md
    ├── 04-resource-mgmt.md
    ├── 05-locking.md
    ├── 06-security.md
    ├── 07-hardware.md
    ├── 08-verification.md
    └── 09-report.md
```

## 2. Stage Configuration (`stages.toml`)

The review pipeline is organized into numbered stages. Each stage runs a
focused analysis pass over the patch. The `stages.toml` file controls which
stages run, what prompt files they load, and which supporting documents to
include.

### 2.1 Data Model

```rust
pub struct StagesConfig {
    pub stages: Vec<StageDefinition>,
}

pub struct StageDefinition {
    pub number: u8,                       // Stage identifier (1-9+)
    pub name: Option<String>,             // Display name
    pub instruction_file: Option<PathBuf>,// Custom prompt file path
    pub supporting_files: Vec<String>,    // Guidance files to include
    pub enabled: bool,                    // Default: true
}
```

### 2.2 Example Configuration

```toml
[[stages]]
number = 1
name = "Analyze commit main goal"
instruction_file = "stages/01-analyze-goal.md"
supporting_files = []
enabled = true

[[stages]]
number = 5
name = "Locking and synchronization"
instruction_file = "stages/05-locking.md"
supporting_files = ["subsystem/locking.md"]
enabled = true

# Disable a stage entirely
[[stages]]
number = 7
enabled = false

# Add a custom stage
[[stages]]
number = 11
name = "Performance analysis"
instruction_file = "custom/performance.md"
supporting_files = ["custom/perf-patterns.md"]
enabled = true
```

### 2.3 Stage Prompt Loading

For each stage, the prompt is resolved in priority order:

1. **Custom file**: `instruction_file` from `stages.toml` (resolved relative
   to the prompts base directory).
2. **Auto-discovered file**: `stages/{NN:02}-*.md` pattern in the base
   directory.
3. **Hardcoded fallback**: Built-in instruction text for stages 1-9.

After loading, the instruction content is passed through variable
substitution (see section 3), and the supporting files listed in
`supporting_files` are appended.

### 2.4 Stage Filtering

Stages can be filtered at two levels:

- **Configuration**: `enabled = false` in `stages.toml` removes a stage from
  the pipeline. This is applied during the planning pre-phase; disabled stages
  are retained out of the planning AI's selection.
- **CLI**: `--stages 1,2,3,5` on the review binary selects specific stages to
  run, overriding both the planning phase and the configuration.

## 3. Template Variable Substitution

Prompt files can contain `{{variable_name}}` placeholders that are replaced
at load time with values from the configuration.

### 3.1 Built-in Variables

| Variable   | Value                                    |
|------------|------------------------------------------|
| `{{date}}` | Current date in `YYYY-MM-DD` format      |
| `{{year}}` | Current year in `YYYY` format            |

### 3.2 User-Defined Variables

```toml
[prompts.variables]
organization = "ACME Corp"
soc = "ARMv8"
kernel_tree = "linux-stable"
```

These are substituted before built-in variables, so user variables take
precedence if names collide.

### 3.3 Implementation

```rust
fn substitute_variables(content: &str, variables: &HashMap<String, String>) -> String {
    let mut result = content.to_string();
    for (key, value) in variables {
        let placeholder = format!("{{{{{}}}}}", key);  // {{key}}
        result = result.replace(&placeholder, value);
    }
    // Built-in variables
    result = result.replace("{{date}}", &chrono::Utc::now().format("%Y-%m-%d"));
    result = result.replace("{{year}}", &chrono::Utc::now().format("%Y"));
    result
}
```

## 4. Tool Filtering

The AI review pipeline provides 14 built-in tools (file reading, git
operations, search, etc.). Tool filtering allows users to restrict which tools
the AI can access during review.

### 4.1 Modes

- **Default**: All built-in tools enabled (no `[tools]` section needed).
- **Allowlist**: Only tools listed in `enabled` are available.
- **Denylist**: All tools available except those in `disabled`.
- **Combined**: `disabled` takes precedence over `enabled`.

### 4.2 Configuration

```toml
[tools]
# Allowlist: only these tools are available
enabled = ["read_files", "git_show", "git_diff", "search_file_content"]

# Denylist: remove these (takes precedence over enabled)
disabled = ["git_checkout"]
```

### 4.3 Built-in Tools

| Tool                  | Description                                |
|-----------------------|--------------------------------------------|
| `read_files`          | Read file content (raw or smart mode)      |
| `git_blame`           | Show per-line revision and author           |
| `git_diff`            | Show changes between commits               |
| `git_show`            | Show objects (blobs, commits, tags)         |
| `git_log`             | Show commit logs                           |
| `git_status`          | Show working tree status                   |
| `git_checkout`        | Switch branches or restore files           |
| `git_branch`          | List branches                              |
| `git_tag`             | List tags                                  |
| `list_dir`            | List directory contents                    |
| `search_file_content` | Grep for patterns in files                 |
| `find_files`          | Find files by glob pattern                 |
| `TodoWrite`           | Write TODO items for structured output     |
| `read_prompt`         | Read prompt files from prompts directory   |

### 4.4 Implementation

```rust
pub fn with_config(
    worktree_path: PathBuf,
    prompts_path: Option<PathBuf>,
    tools_config: Option<&ToolsSettings>,
) -> Self {
    let enabled_tools = tools_config.map(|config| {
        let mut tools: HashSet<String> = if config.enabled.is_empty() {
            // All tools enabled, then subtract disabled
            ALL_TOOL_NAMES.iter().cloned().collect()
        } else {
            config.enabled.iter().cloned().collect()
        };
        for d in &config.disabled {
            tools.remove(d);
        }
        tools.into_iter().collect()
    });
    // ...
}
```

## 5. Custom Tool Definitions

Users can define shell-based tools that the AI can invoke during review. This
enables integration with external analysis tools, custom linters, or
project-specific utilities.

### 5.1 Definition Structure

```rust
pub struct CustomToolDefinition {
    pub name: String,           // Tool identifier
    pub description: String,    // Description shown to AI
    pub parameters: String,     // JSON Schema for tool parameters
    pub command: String,        // Shell command template
    pub allowed_paths: Vec<String>, // Path allowlist (security)
}
```

### 5.2 Configuration

```toml
[[tools.custom]]
name = "run_sparse"
description = "Run sparse static analysis on a C source file"
parameters = '{"type": "object", "properties": {"file": {"type": "string", "description": "Path to the C source file"}}, "required": ["file"]}'
command = "sparse {file}"
allowed_paths = ["drivers/", "fs/", "kernel/", "mm/", "net/"]
```

### 5.3 Security Validation

Custom tools are validated at registration time:

1. **Blocked command patterns**: `rm -rf`, `sudo`, `curl`, `wget`, `dd `,
   `mkfs`. If the command template contains any of these patterns, the tool
   is rejected.

2. **Path containment**: Every entry in `allowed_paths` is validated to not
   escape the worktree directory via path traversal.

### 5.4 Execution Flow

```text
AI requests tool call: run_sparse({"file": "drivers/gpu/drm/foo.c"})
  |
  +--> Parameter substitution
  |      command = "sparse drivers/gpu/drm/foo.c"
  |
  +--> Path allowlist check (if allowed_paths specified)
  |      "drivers/gpu/drm/foo.c" starts with "drivers/" -> allowed
  |
  +--> Execute: sh -c "sparse drivers/gpu/drm/foo.c"
  |      working directory: worktree
  |
  +--> Return stdout (or error if non-zero exit)
```

Parameter substitution replaces `{param_name}` in the command template with
the corresponding argument value. Array values are joined with spaces.

### 5.5 Tool Dispatch

When the AI calls a tool, `ToolBox::call()` checks custom tools first (by
name match), then dispatches to built-in tools. Custom tools take priority
over built-in tools with the same name.

## 6. CLI Integration

### 6.1 Review Binary Flags

The `review` binary accepts these flags for customization:

| Flag                  | Description                              |
|-----------------------|------------------------------------------|
| `--prompts <PATH>`   | Override prompts directory               |
| `--stages <1,2,3>`   | Select specific stages to run            |
| `--ai_provider <P>`  | Override AI provider from settings       |
| `--custom_prompt <S>` | Append text to the user task prompt      |

### 6.2 Priority Chain

```text
CLI flag  >  Settings.toml  >  Built-in default

--prompts     [prompts]         "third_party/prompts/kernel"
--stages      stages.toml       planning pre-phase decides
--ai_provider [ai] provider     Settings.toml value
```

### 6.3 Settings.toml Integration

```toml
[prompts]
directory = "git://github.com/org/custom-prompts.git"
stages_config = "config/my-stages.toml"

[prompts.variables]
organization = "ACME Corp"

[tools]
enabled = ["read_files", "git_show", "git_diff", "search_file_content"]
disabled = []

[[tools.custom]]
name = "custom_lint"
description = "Run project-specific linter"
parameters = '{"type": "object", "properties": {"path": {"type": "string"}}}'
command = "./scripts/lint.sh {path}"
allowed_paths = ["src/"]
```

## 7. Implementation Plan

### `src/settings.rs`
- Add `PromptsSettings` struct with `directory`, `stages_config`, `variables`.
- Add `ToolsSettings` struct with `enabled`, `disabled`, `custom`.
- Add `CustomToolDefinition` struct.
- Add `tools: Option<ToolsSettings>` and `prompts: Option<PromptsSettings>`
  to `Settings`.
- Add `get_prompts_dir()` helper.

### `src/worker/prompts.rs`
- Add `StagesConfig` and `StageDefinition` structs for TOML parsing.
- Add `PromptRegistry.stages_config` and `variables` fields.
- Implement `PromptRegistry::with_settings()` for directory resolution.
- Implement `resolve_prompts_directory()` with local/git/http dispatch.
- Implement `download_remote_prompts()` and `clone_git_prompts()` with caching.
- Implement `load_stages_config()` for TOML parsing.
- Implement `substitute_variables()` for `{{key}}` replacement.
- Update `get_stage_prompt()` to load from stages config and apply
  substitution.
- Update planning pre-phase to filter disabled stages from selection.

### `src/worker/tools.rs`
- Add `enabled_tools: Option<Vec<String>>` and
  `custom_tools: Vec<(AiTool, CustomToolDefinition)>` fields to `ToolBox`.
- Implement `ToolBox::with_config()` constructor.
- Implement `is_tool_enabled()` for allowlist/denylist filtering.
- Implement `register_custom_tools()` with validation.
- Implement `validate_tool_security()` for blocked pattern and path checks.
- Implement `execute_custom_tool()` with parameter substitution.
- Update `get_declarations_generic()` to include custom tools and apply
  filtering.
- Update `call()` to dispatch custom tools before built-in tools.

### `src/bin/review.rs`
- Update `--prompts` flag from `PathBuf` to `Option<PathBuf>`.
- Add `--stages` flag accepting comma-separated stage numbers.
- Wire `PromptRegistry::with_settings()` when settings.prompts is available.
- Wire `ToolBox::with_config()` with settings.tools.

### `third_party/prompts/kernel/stages.toml`
- Default 9-stage configuration matching the built-in pipeline.
- Commented examples showing custom stages and stage disabling.
