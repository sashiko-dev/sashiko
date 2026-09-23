-- Nullable so legacy findings keep an unknown reachability classification.
ALTER TABLE findings ADD COLUMN currently_unreachable INTEGER;
