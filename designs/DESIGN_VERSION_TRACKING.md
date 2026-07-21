# Version Tracking Design

## Objective
Track version chains for patch series from both mailing lists
(NNTP/lore/lkml) and forge webhooks (GitHub/GitLab). When a
maintainer posts [PATCH v2 0/5] or pushes new commits to an MR,
Sashiko recognizes it as a new version, links it to the prior
version, persists the version number, and injects prior review
findings into the new review prompt.

## Version Detection

### Mailing list path
- Extract version from email subjects using `parse_subject_version()`
  in `src/patch.rs` (handles [PATCH v2], [PATCH V3 1/2], [PATCHv5],
  [PATCH RFC v2 8/8], multi-digit, etc.)
- Persist in `version INTEGER DEFAULT 1` column on `patchsets` table
- Default to version 1 when no version indicator is found

### Forge path (MR/PR)
- Store `commit_range` (base_sha..head_sha) on the patchset
- When the same MR/PR number fires again with a different
  `commit_range`, create a new patchset with version incremented
- Same `commit_range` (metadata-only change) updates existing
  patchset without creating a new version

## Version Chain Linkage

### Signal-based matching (no time window)
Version linkage uses signals evaluated in priority order. No
arbitrary time window — if signals match after months, they link.
If no signal confirms a match, the patchset stands alone.

Priority:
1. **Thread match**: Same `thread_id` with `version = current - 1`
2. **Author + subject match**: Same author email, stripped subjects
   match (version tags removed via `strip_subject_version()`),
   `version = current - 1`

### Subject normalization
`strip_subject_version()` removes `vN` from subjects for comparison.
Example: `[PATCH v2 0/5] Refactor scheduler` and
`[PATCH 0/5] Refactor scheduler` compare as equal.

### Forge path
`find_patchset_by_mr_number()` looks up the latest version by
MR/PR number. Commit range comparison determines new version vs
metadata-only update.

## Cross-Version Merge Prevention
- Patchsets with different explicit versions are not merged, even
  in the same thread
- Implicit (None) versions still merge with explicit versions in
  the same thread to handle series where cover letters have version
  tags but individual patches do not
- Patch reassignment between version-tracked patchsets is blocked
  in `create_patch()`

## Prior Review Context Injection (deferred)

**Status: not yet implemented.** The infrastructure (schema,
version chain linkage, `previous_version_id` population) is in
place. The following items are deferred to a follow-up PR:

- `get_previous_version_findings()` — follow `previous_version_id`
  one hop, return non-preexisting findings sorted by severity
- `format_previous_findings()` — format as structured context,
  cap at token limit (2000 tokens, severity-prioritized truncation)
- Inject into `dynamic_context` in the review prompt so the AI
  reviewer can check whether prior issues were addressed
- `ReviewInput` struct gains `previous_context: Option<String>`
- Injection points in `reviewer.rs` and `worker/prompts.rs`

## Schema Changes
```sql
ALTER TABLE patchsets ADD COLUMN version INTEGER DEFAULT 1;
ALTER TABLE patchsets ADD COLUMN previous_version_id
    INTEGER REFERENCES patchsets(id);
ALTER TABLE patchsets ADD COLUMN commit_range TEXT;
CREATE INDEX IF NOT EXISTS idx_patchsets_version
    ON patchsets(version) WHERE version > 1;
```
All migrations are idempotent via `try_add_column`.

## API Changes
- `GET /api/patchsets` and `GET /api/patchset` responses include
  `version` and `previous_version_id` fields (via PatchsetRow
  serialization and manual JSON builders)
- Forge webhook returns `version` in response JSON
- No new endpoints

## UI Changes
- Yellow version badge ("v2", "v3") displayed next to patchset names
  for versions > 1
- v1 patchsets show no badge to reduce visual noise
- Badge uses `.tag-version` CSS class with light/dark mode support

## Input Validation
- `sanitize_message_id()` strips null bytes and control characters
- SHA validation and SSRF blocklist in forge `parse_payload`
  (from webhook-input-hardening prerequisite)
- All new DB operations use parameterized queries

## Risks
- Version detection accuracy: `parse_subject_version()` may miss
  non-standard formats. Mitigation: default to v1.
- Cross-series false linking: Mitigated by requiring author match
  AND cleaned subject match.
- Prior context size: Capped with severity-sorted truncation
  (to be enforced when context injection is implemented).
