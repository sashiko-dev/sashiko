---
name: process-failed-reviews
description: Batch process review-failed.md files across multiple directories, updating subsystem guides and committing after each
tools: Read, Write, Glob, Bash, Task, TaskCreate, TaskUpdate, TaskList
---

# Process Failed Reviews Agent

This agent batch processes `review-failed.md` files across multiple commit
directories, running the failed-review agent on each one sequentially and
committing subsystem guide changes after each directory completes.

## Input

You will be given:
1. Working directory containing `linux.<sha>/` subdirectories
2. The prompt directory path (contains `agent/`, `subsystem/`, and pattern files)

Each `linux.<sha>` subdirectory contains a `review-failed.md` file to process.

## Workflow

### Step 1: Discover Directories

Find all directories matching `linux.*/review-failed.md`:

```bash
find . -maxdepth 2 -name "review-failed.md" -path "./linux.*/*" 2>/dev/null | \
  sed 's|^\./||; s|/review-failed.md||' | sort
```

### Step 2: Create Task List

Create a task for each directory using TaskCreate:

```
Subject: Process linux.<sha> review-failed.md
Description: Run failed-review agent on linux.<sha>/review-failed.md, output to linux.<sha>/failed-review-report.md
ActiveForm: Processing linux.<sha>
```

### Step 3: Process Each Directory Sequentially

For each directory, in order:

1. **Mark task in_progress**

2. **Run failed-review subagent**:
   ```
   Task(
     description: "Failed-review linux.<sha>",
     subagent_type: "general-purpose",
     prompt: """
       You are the failed-review agent. Follow the instructions in
       <prompt_dir>/agent/failed-review.md

       Working directory: <work_dir>/linux.<sha>
       Prompt directory: <prompt_dir>

       Read ./review-failed.md and process it according to the failed-review.md
       instructions. Write your report to ./failed-review-report.md

       You may need to update subsystem guides in <prompt_dir>/subsystem/
       based on the classification of missed bugs.
     """
   )
   ```

3. **Check for changes and commit if needed**:

   Check git status for both modified and untracked files:
   ```bash
   cd <prompt_dir>/.. && git status
   ```

   Look for:
   - **Modified files**: `modified: kernel/subsystem/*.md`
   - **New files**: `Untracked files: ... kernel/subsystem/*.md`

   If there are ANY changes (modified OR new files) in `kernel/subsystem/`:
   ```bash
   cd <prompt_dir>/.. && \
     git add -A kernel/subsystem/ && \
     git commit -s -m "$(cat <<'EOF'
   subsystem[/<file>]: <brief description of changes>

   <1-2 sentence explanation of what was added/updated>

   Learned from: <sha> ("<commit subject>")
   EOF
   )"
   ```

   The `git add -A` stages both new and modified files. Common scenarios:
   - **New subsystem guide created**: e.g., `kernel/subsystem/drm.md` appears as untracked
   - **Existing guide updated**: e.g., `kernel/subsystem/btrfs.md` appears as modified
   - **Trigger table updated**: `kernel/subsystem/subsystem.md` modified to add new entry

4. **Mark task completed**

5. **Continue to next directory**

## Commit Message Format

```
subsystem[/<file>]: <brief description>

<What knowledge was added and why it matters>

Learned from: <sha> ("<commit subject>")
```

Examples:
- `subsystem: add DRM/Display guide with atomic context rules`
- `subsystem/drm: add system PM vs runtime PM context section`
- `subsystem/btrfs: add zoned storage zone limits section`

## Important Notes

- **Sequential processing required**: Agents must run one at a time to avoid
  conflicts when updating shared subsystem guides
- **Commit after each directory**: Do not batch commits; commit immediately
  after each directory that produces changes
- **Skip commits when no changes**: If the failed-review agent classifies all
  bugs as `process error` or `other`, there will be no guide changes to commit
- **Use signed commits**: Always use `git commit -s`

## Reference

**Directory layout**:
```
<prompt_dir>/
├── agent/
│   ├── failed-review.md
│   ├── process-failed-reviews.md  (this file)
│   └── ...
├── subsystem/
│   ├── subsystem.md
│   ├── networking.md
│   ├── drm.md
│   ├── locking.md
│   └── ...
├── technical-patterns.md
└── ...

<work_dir>/
├── linux.<sha1>/
│   ├── review-failed.md        (input)
│   └── failed-review-report.md (output)
├── linux.<sha2>/
│   ├── review-failed.md
│   └── failed-review-report.md
└── ...
```

## Final Output

After all directories are processed, report:
```
================================================================================
PROCESS-FAILED-REVIEWS COMPLETE
================================================================================

Directories processed: <count>
Commits made: <count>

Subsystem guides created:
  - <path>: <description>

Subsystem guides updated:
  - <path>: <section added>

Classifications summary:
  missing subsystem knowledge: <count> (guides updated)
  process error: <count> (no changes)
  other: <count> (no changes)
================================================================================
```
