# systemd Review Prompts for Claude Code

AI-assisted code review prompts optimized for the systemd codebase.

## Installation

Run the setup script to install the skill and slash commands:

```bash
./scripts/claude-setup.sh
```

This will install:
- The `systemd` skill to `~/.claude/skills/systemd/SKILL.md`
- Slash commands to `~/.claude/commands/`

## Usage

The systemd skill loads automatically when working in a systemd tree.

### Slash Commands

- `/systemd-review` - Review commits for regressions and issues
- `/systemd-debug` - Debug crashes, assertions, and stack traces
- `/systemd-verify` - Verify findings against false positive patterns

### Manual Loading

If the skill doesn't auto-load, you can manually trigger it by asking
about systemd-specific topics or requesting a review.

## File Structure

```
review-prompts/
├── README.md                 # This file
├── technical-patterns.md     # Core patterns (always loaded)
├── review-core.md            # Main review protocol
├── debugging.md              # Debugging protocol
├── namespace.md              # Mount namespace patterns
├── core.md                   # PID1/service manager patterns
├── cleanup.md                # Cleanup attribute patterns
├── nspawn.md                 # Container patterns
├── dbus.md                   # D-Bus patterns
├── patterns/                 # Detailed pattern explanations
├── skills/                   # Skill template
├── scripts/                  # Setup script
├── slash-commands/           # Slash command definitions
├── false-positive-guide.md   # False positive checklist
└── inline-template.md        # Report template
```

## Integration with Kernel Review-Prompts

This setup is designed to coexist with the kernel review-prompts.
Both can be installed simultaneously - each installs to a separate
skill directory (`~/.claude/skills/kernel/` vs `~/.claude/skills/systemd/`).

To install both:
1. Run `./scripts/claude-setup.sh` from the systemd review-prompts directory
2. Run `./scripts/claude-setup.sh` from the kernel review-prompts directory

Each skill auto-loads based on the working directory context.
