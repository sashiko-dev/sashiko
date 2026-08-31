# Design: Bug Frontend Consistency & Processing Logic Alignment

## Context & Motivation

The frontend page for Linux kernel bugs (`#/bug/:id`) was built independently from the patchset and review pages, leading to several UX inconsistencies:
1. **Unreflected Processing Logic**: The page grouped identity and final pipeline enrichments (calibrated severity, introduced-in commit, official subsystems) at the top in a generic table, while hiding the initial candidate input (raw discovery data, candidate reasoning, candidate locations) in a collapsed section at the very bottom beneath the generated defect report.
2. **Prominent / Misplaced Logs Link**: A link to view pipeline logs was embedded in the middle of the top metadata table, rather than at the bottom of the card/page as a small muted link, contrasting with the patchset review cards.
3. **Inconsistent Token Counters**: Token usage on the bug log page reported raw `Prompt | Completion | Total`, which double-counted cached prompt tokens and diverged from the `In: X · Cached: Y · Out: Z` standard used by the rest of Sashiko. The bug details page omitted token counters entirely.
4. **Header Inconsistencies**: The bug view header did not display status badges alongside the title as done in patchset views and baseline logs.

## Architectural Changes

### 1. Sequential Processing Flow
The bug view is restructured to reflect the lifecycle of a defect through the Sashiko pipeline:

```
+-------------------------------------------------------------+
| Header: Bug Title + Status Badge                            |
+-------------------------------------------------------------+
| Section 1: Raw Data                                         |
| - Bug ID, Reported at, Discovered in (Patchset/Patch/Commit)|
| - Candidate Problem statement                               |
| - Initial Finding / Reasoning (from patch review)           |
| - Candidate Files & Locations                               |
| - Collapsible Raw Payload (JSON)                            |
+-------------------------------------------------------------+
| Section 2: Pipeline Enrichments                             |
| - Calibrated Severity & Impact Explanation                  |
| - Pipeline Status & Resolution (Fixed / Duplicate / Dismiss)|
| - Official Subsystems (MAINTAINERS mapping)                 |
| - Verified Source Files & Mainline Locations                |
| - Verified on Mainline Commit                               |
| - Origin Tracing: Introduced-by Commit                      |
| - Deduplication: Duplicate-of or Linked Duplicates          |
+-------------------------------------------------------------+
| Section 3: Defect Report (if description is present)        |
| - Formatted standalone LKML report with Copy button         |
+-------------------------------------------------------------+
| Footer: Metrics & Logs                                      |
| - Tokens used: In: X · Cached: Y · Out: Z                   |
| - Bottom Link: View Raw Log (small, muted link)             |
+-------------------------------------------------------------+
```

### 2. Standardized Token Counter Structure
Adopt the unified token accounting used across Sashiko:
- Net input tokens: `tokensIn = Math.max(0, (tokens_in || 0) - tokensCached)`
- Cached tokens: `tokensCached = (tokens_cached || 0)`
- Output tokens: `tokensOut = (tokens_out || 0)`
- Formatted string: `In: ${tokensIn.toLocaleString()} · Cached: ${tokensCached.toLocaleString()} · Out: ${tokensOut.toLocaleString()}`
- Displayed as: `<strong>Tokens used:</strong> ${tokenStr}` on the bug view footer and `<div class="kv"><div class="label">Tokens used:</div><div>${tokenStr}</div></div>` on the bug log view.

### 3. Log Link Placement & Styling
- Remove the inline `Logs:` row from the top metadata block.
- Place a small, muted link `View Raw Log` at the bottom of the page right beneath the token usage row, matching the style from patchset review cards:
  `style="font-weight:600; color:var(--text-dim); text-decoration:none;"`
