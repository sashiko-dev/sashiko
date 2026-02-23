# Patch Subject Filtering

## Motivation
Users often want to run the AI reviewer only against specific patches in a patchset. Some patches might be minor refactors, documentation updates, or belong to subsystems they don't want AI to review right now. Providing a way to filter which patches get reviewed based on their subjects allows users to save tokens, time, and focus on the patches that matter most.

## Design

### Command Line Interface
We will extend `sashiko-cli submit` to accept two new list arguments:
- `--skip-subject <PATTERN>`: A glob pattern. Patches with subjects matching any of these patterns will be marked as `Skipped` and not reviewed by AI.
- `--only-subject <PATTERN>`: A glob pattern. If provided, *only* patches with subjects matching at least one of these patterns will be reviewed. Patches not matching any of the `only-subject` patterns will be skipped.

If both are specified, a patch must match an `only-subject` pattern and *must not* match a `skip-subject` pattern.

### API Changes
The `SubmitRequest` API models (`Inject`, `Remote`, `RemoteRange`) will be updated to include two new optional fields:
- `skip_subjects: Option<Vec<String>>`
- `only_subjects: Option<Vec<String>>`

### Database Changes
To preserve the filters for the review phase, the `patchsets` table will be updated:
- `skip_filters TEXT` (storing a JSON-serialized array)
- `only_filters TEXT` (storing a JSON-serialized array)

`PatchsetRow` will be updated to fetch and decode these values.

### Ingestion Flow
1. API receives `SubmitRequest` with filters.
2. For Mbox `Inject`, `Event::RawMboxSubmitted` carries the filters. Then they are passed into `ParsedArticle`, and finally `create_patchset` persists them to the database.
3. For `Remote` / `RemoteRange`, `create_fetching_patchset` persists the filters right at the API handler. When `FetchAgent` resolves the commits, `save_parsed_article` updates the patchset without overwriting the stored filters.

### Review Phase
In `Reviewer::review_patchset_task` (which orchestrates the AI review loops), we will extract `skip_filters` and `only_filters` from the current patchset.
Before calling `process_patch_review`, we evaluate each patch's subject against the glob patterns (translated into Regex internally).
If a patch is to be skipped:
1. It is marked as `Skipped` in the `patches` table via a new `update_patch_status` database method.
2. We `continue` to the next patch in the series, completely bypassing the AI generation call.
The overall patchset review can still conclude with a `Reviewed` status since the unskipped patches are processed normally.