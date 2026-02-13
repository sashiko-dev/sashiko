# Review Scripts Documentation

These scripts automate the process of reviewing a series of git commits using Claude.

## Scripts Overview

| Script | Purpose |
|--------|---------|
| `create_changes.py` | Extracts and categorizes commit changes for structured review |
| `review_one.sh` | Reviews a single commit SHA |
| `claude_xargs.py` | Runs multiple reviews in parallel |
| `claude-json.py` | Parses Claude's stream-json output to markdown |
| `lore-reply` | Creates reply emails to patches on lore.kernel.org |

---

## create_changes.py

Extracts commit information and categorizes changes from a Linux kernel commit into a structured format suitable for parallel agent-based review.

### Installation

Copy or symlink to a directory in your PATH:

```bash
ln -s /path/to/review-prompts/kernel/scripts/create_changes.py ~/.local/bin/create_changes.py
# or
cp /path/to/review-prompts/kernel/scripts/create_changes.py ~/.local/bin/
```

### Usage

```bash
create_changes.py [options] <commit_ref>
```

### Options

| Option | Description | Default |
|--------|-------------|---------|
| `<commit_ref>` | Commit reference (SHA, HEAD, etc.) or path to a patch file | `HEAD` |
| `-o, --output-dir` | Directory for output files | `./review-context` |
| `-C, --git-dir` | Git repository directory | Current directory |
| `--no-semcode` | Skip semcode integration, extract definitions from source | Auto-detect |

### What it does

1. Reads the commit (or patch file) and extracts metadata (SHA, author, date, subject, body, tags)
2. Parses the diff into hunks, grouping them by source file and function
3. Uses semcode for static analysis if `.semcode.db` exists; otherwise falls back to a Python-based parser
4. Combines small changes intelligently:
   - Modifications to the same function: combined up to 50 added lines
   - New functions in the same file: combined up to 200 total lines
   - Small files: merged into groups up to 200 lines total
5. Splits large files into multiple FILE-N groups (max 400 lines per group)
6. Writes structured output for downstream review agents

### Output Files

All output goes to the `--output-dir` directory (default: `./review-context/`):

| File | Description |
|------|-------------|
| `change.diff` | Full commit message and unified diff |
| `commit-message.json` | Parsed commit metadata (SHA, author, subject, body, tags, files-changed, subsystems) |
| `index.json` | Index of all files and changes with version 2.0 schema |
| `FILE-N-CHANGE-M.json` | One file per change, grouped by source file |

### index.json Schema

```json
{
  "version": "2.0",
  "commit": {
    "sha": "abc123...",
    "subject": "commit subject line",
    "author": "Name <email>"
  },
  "files": [
    {
      "file_num": 1,
      "file": "path/to/file.c",
      "files": ["path/to/file.c"],
      "total_lines": 150,
      "changes": [
        {
          "id": "FILE-1-CHANGE-1",
          "function": "function_name",
          "file": "path/to/file.c",
          "hunk": "-10,5 +20,5"
        }
      ]
    }
  ],
  "files-modified": ["path/to/file.c", "other/file.h"],
  "total-files": 3,
  "total-changes": 7
}
```

### FILE-N-CHANGE-M.json Schema

```json
{
  "id": "FILE-1-CHANGE-1",
  "file": "path/to/file.c",
  "function": "function_name",
  "hunk_header": "-10,5 +20,5",
  "diff": "@@ -10,5 +20,5 @@ function_name\n ...",
  "total_lines": 25,
  "modifies": "function_name",
  "types": ["struct foo"],
  "calls": ["helper_func", "another_func"],
  "callers": ["caller1", "caller2"],
  "definition": "static int function_name(...) { ... }"
}
```

The `modifies`, `types`, `calls`, and `callers` fields come from semcode analysis when available. The `definition` field is extracted from source when semcode is unavailable.

### Example

```bash
# Analyze HEAD commit
create_changes.py HEAD -o ./review-context

# Analyze a specific commit
create_changes.py abc123def -o ./review-context

# Analyze a patch file
create_changes.py /path/to/patch.diff -o ./review-context

# Use with a different git directory
create_changes.py -C /path/to/linux abc123 -o ./review-context

# Force Python-only parsing (no semcode)
create_changes.py --no-semcode HEAD
```

### Integration with Agent Workflow

This script is designed to be called by the context-analyzer agent (see `kernel/agent/context.md`). The agent runs this script to prepare structured context files that other review agents can consume in parallel, with each agent reviewing a subset of FILE-N groups.

---

## review_one.sh

Reviews a single git commit by setting up a worktree and running Claude's review.

### Usage

```bash
review_one.sh [options] <sha>
```

### Options

| Option | Description | Default |
|--------|-------------|---------|
| `--linux <path>` | Path to the base linux directory | `./linux` |
| `--prompt <file>` | Path to the review prompt file | `<script_dir>/../review-core.md` |
| `--series <sha>` | SHA of the last commit in the series (optional) | - |
| `--working-dir <dir>` | Directory where worktrees are created | Current directory or `$WORKING_DIR` |
| `--model <model>` | Claude model to use | `sonnet` or `$CLAUDE_MODEL` |

### What it does

1. Creates a git worktree at `linux.<sha>` for the specified commit
2. If the base linux directory has `.semcode.db`, hard-links it into the worktree and configures MCP
3. Runs Claude with the review prompt
4. Outputs results to:
   - `review.json` - Raw Claude output (stream-json format)
   - `review.md` - Parsed markdown review
   - `review.duration.txt` - Elapsed time
5. Retries up to 5 times if Claude exits without output

### Directory Structure Assumptions

`review_one.sh` uses its install location to find related files:

| Path | Description |
|------|-------------|
| `$SCRIPT_DIR/claude-json.py` | JSON parser script (must be in same directory) |
| `$SCRIPT_DIR/../review-core.md` | Default review prompt (parent directory of scripts/) |

Where `$SCRIPT_DIR` is the directory containing `review_one.sh`.

### Prerequisites

- The SHA range must be indexed with semcode first:
  ```bash
  cd linux && semcode-index -s . --git base..last_sha
  ```
- `semcode-mcp` must be in your PATH if using semcode integration

---

## claude_xargs.py

Runs multiple `claude -p` commands in parallel, similar to xargs but with timeout support and proper signal handling.

### Usage

```bash
claude_xargs.py -c <command> -f <sha_file> [options]
```

### Options

| Option | Description | Default |
|--------|-------------|---------|
| `-c, --command` | The claude command template to run (required) | - |
| `-f, --sha-file` | File containing list of SHAs, one per line (required) | - |
| `-n, --parallel` | Number of parallel instances | 24 |
| `--series <sha>` | SHA of the last commit in the series | - |
| `--timeout <seconds>` | Timeout for each command | - |
| `-v, --verbose` | Print stderr output from commands | - |

### Features

- Runs commands in parallel using a thread pool
- Handles Ctrl-C gracefully, killing all spawned processes
- Supports per-command timeout
- Reports progress and failure counts

### Example

```bash
/path/claude_xargs.py -n 5 -f shas.txt -c '/path/review_one.sh'
```

---

## claude-json.py

Parses Claude's stream-json output format and converts it to plain text/markdown.

### Usage

```bash
# Pipe from claude
claude -p "prompt" --output-format=stream-json | python claude-json.py

# From file
python claude-json.py -i input.json -o output.txt

# Debug mode
python claude-json.py -d < input.json
```

### Options

| Option | Description |
|--------|-------------|
| `-i, --input` | Input file (default: stdin) |
| `-o, --output` | Output file (default: stdout) |
| `-d, --debug` | Enable debug output to stderr |

### Why it exists

When using `claude -p` (non-interactive mode), normal output is disabled. The only way to capture output is with `--output-format=stream-json` and `--verbose`. This script parses that JSON stream back into readable markdown.

---

## lore-reply

Creates properly formatted reply emails to patches posted on lore.kernel.org, with optional AI-assisted analysis of existing thread replies and patch verification.

### Usage

```bash
# Reply to a patch by commit reference (uses b4 dig to find it on lore)
lore-reply [--dry-run] [--force] <COMMIT-REF>

# Reply to a patch from a local mbox file
lore-reply [--dry-run] --mbox <MBOX-FILE> [MESSAGE-ID]
```

### Options

| Option | Description | Default |
|--------|-------------|---------|
| `--dry-run` | Don't actually send the email | Sends email |
| `--force` | Skip patch-id verification and Claude analysis | - |
| `--mbox <file>` | Use existing mbox file instead of downloading | - |

### What it does

**Commit reference mode:**
1. Uses `b4 dig` to find the patch on lore.kernel.org by commit hash
2. Downloads the thread mbox
3. Uses Claude (haiku) to summarize existing replies in the thread
4. Verifies patch-id matches between the email and local commit
5. If patch-ids differ, uses Claude to explain the differences
6. Creates a reply email with proper headers (In-Reply-To, References) and quoted body
7. Opens `git send-email --annotate` to edit and send

**Mbox mode:**
1. Reads the specified mbox file directly
2. Skips all verification and analysis
3. Creates reply email and opens git send-email

### Reply Analysis

When run without `--force`, the script checks for `./review-inline.txt` and asks Claude to:
- Summarize existing replies to the patch
- Check if anyone has reported similar issues to those in the review file

### Example Workflow

```bash
# After reviewing a commit with review_one.sh:
cd linux.<sha>

# Reply to the patch
lore-reply HEAD

# Test without sending (dry-run)
lore-reply --dry-run HEAD

# Skip AI analysis and patch verification
lore-reply --force HEAD

# Reply to a manually downloaded mbox
lore-reply --mbox thread.mbox
```

### Prerequisites

- `b4` - For finding and downloading patches from lore.kernel.org
- `git send-email` - For sending the reply
- `claude` CLI (optional) - For thread analysis and patch comparison

---

## Complete Workflow Example

Review a patch series applied to linux:

```bash
# 1. Prepare the linux tree
cd linux
git reset --hard v6.19
git am -s patches/*.patch
git rev-list v6.19..HEAD > ../series

# 2. Index with semcode (optional but recommended)
semcode-index -s . --git v6.19..HEAD

# 3. Run parallel reviews
cd ..
/path/to/scripts/claude_xargs.py \
    -n 10 \
    -f series \
    -c "/path/to/scripts/review_one.sh" \
    --series $(git -C linux rev-parse HEAD)
```

### Output

After completion, you'll have:
- `linux.<sha>/` directories for each commit
- `linux.<sha>/review.md` - The review for each commit
- `linux.<sha>/review-inline.txt` - Any bugs found (if applicable)
- `linux.<sha>/review.duration.txt` - Time taken for each review
