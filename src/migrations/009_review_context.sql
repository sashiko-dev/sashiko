-- Review pipeline selector / context for patchsets.
-- NULL (default) selects the standard patch review pipeline.
-- When present, contains a serialized ReviewKind JSON selecting an alternate
-- pipeline (such as cherry-pick review).

ALTER TABLE patchsets ADD COLUMN review_context TEXT;
