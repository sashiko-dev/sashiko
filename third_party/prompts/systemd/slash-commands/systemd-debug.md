---
name: systemd-debug
description: Debug systemd crashes and issues
---

Using the prompt REVIEW_DIR/debugging.md, analyze the provided crash
information, logs, or stack trace.

Load REVIEW_DIR/technical-patterns.md first, then follow the complete
debugging protocol in debugging.md.

Expected input:
- journalctl output
- Coredump/stack trace
- Error messages
- Reproduction steps

The protocol will:
1. Extract crash information
2. Identify the affected component
3. Analyze the code for root cause
4. Create debug-report.txt with findings
