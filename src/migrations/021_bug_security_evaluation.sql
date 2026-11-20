-- Copyright 2026 The Sashiko Authors
--
-- Licensed under the Apache License, Version 2.0 (the "License");
-- you may not use this file except in compliance with the License.
-- You may obtain a copy of the License at
--
--     https://www.apache.org/licenses/LICENSE-2.0
--
-- Unless required by applicable law or agreed to in writing, software
-- distributed under the License is distributed on an "AS IS" BASIS,
-- WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
-- See the License for the specific language governing permissions and
-- limitations under the License.

-- Project security evaluation fields onto bugs so that listing, filtering,
-- sorting, and leased background backfill sweeps never require correlated
-- json_extract subqueries over bug_enrichments.
--
-- Nullable and constant-default columns avoid rewriting existing rows on
-- large databases.
ALTER TABLE bugs ADD COLUMN security_score_tenths INTEGER;
ALTER TABLE bugs ADD COLUMN security_class TEXT;
ALTER TABLE bugs ADD COLUMN cvss_vector TEXT;
ALTER TABLE bugs ADD COLUMN security_eval_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE bugs ADD COLUMN security_eval_requested INTEGER NOT NULL DEFAULT 0;

-- Serves /api/bugs sorting and >= score filtering.
CREATE INDEX IF NOT EXISTS idx_bugs_security_score
    ON bugs(security_score_tenths DESC, severity_int DESC, id DESC);

-- Serves /api/bugs filtering by security classification.
CREATE INDEX IF NOT EXISTS idx_bugs_security_class
    ON bugs(security_class, security_score_tenths DESC, id DESC);

-- Partial index serving the leased background security evaluation sweep over
-- canonical verified bugs.
CREATE INDEX IF NOT EXISTS idx_bugs_security_backfill
    ON bugs(security_eval_requested DESC, lifecycle_status, severity_int DESC, id DESC)
    WHERE duplicate_of_id IS NULL AND pipeline_state = 'succeeded';

-- Keep projected security columns on bugs synchronized whenever a
-- security_evaluation enrichment is recorded, and clear any pending manual
-- re-evaluation flag. None of these columns are audited by triggers on bugs,
-- so this projection cannot cascade into audit enrichments.
CREATE TRIGGER IF NOT EXISTS trg_bug_enrichments_project_security
AFTER INSERT ON bug_enrichments
FOR EACH ROW
WHEN new.kind = 'security_evaluation' AND new.data_json IS NOT NULL
BEGIN
    UPDATE bugs
    SET security_score_tenths = CASE
            WHEN CAST(COALESCE(
                json_extract(new.data_json, '$.score_tenths'),
                json_extract(new.data_json, '$.security_score_tenths')
            ) AS INTEGER) BETWEEN 0 AND 100
            THEN CAST(COALESCE(
                json_extract(new.data_json, '$.score_tenths'),
                json_extract(new.data_json, '$.security_score_tenths')
            ) AS INTEGER)
            ELSE NULL
        END,
        security_class = CASE LOWER(json_extract(new.data_json, '$.security_class'))
            WHEN 'vulnerability' THEN 'vulnerability'
            WHEN 'hardening' THEN 'hardening'
            WHEN 'not_security' THEN 'not_security'
            ELSE NULL
        END,
        cvss_vector = json_extract(new.data_json, '$.cvss_vector'),
        security_eval_requested = 0
    WHERE id = new.bug_id;
END;

-- Backfill projected columns from the latest security_evaluation enrichment if
-- any rows were recorded prior to this migration.
UPDATE bugs
SET security_score_tenths = (
        SELECT CASE
            WHEN CAST(COALESCE(
                json_extract(e.data_json, '$.score_tenths'),
                json_extract(e.data_json, '$.security_score_tenths')
            ) AS INTEGER) BETWEEN 0 AND 100
            THEN CAST(COALESCE(
                json_extract(e.data_json, '$.score_tenths'),
                json_extract(e.data_json, '$.security_score_tenths')
            ) AS INTEGER)
            ELSE NULL
        END
        FROM bug_enrichments e
        WHERE e.bug_id = bugs.id
          AND e.kind = 'security_evaluation'
          AND e.data_json IS NOT NULL
        ORDER BY e.created_at DESC, e.id DESC
        LIMIT 1
    ),
    security_class = (
        SELECT CASE LOWER(json_extract(e.data_json, '$.security_class'))
            WHEN 'vulnerability' THEN 'vulnerability'
            WHEN 'hardening' THEN 'hardening'
            WHEN 'not_security' THEN 'not_security'
            ELSE NULL
        END
        FROM bug_enrichments e
        WHERE e.bug_id = bugs.id
          AND e.kind = 'security_evaluation'
          AND e.data_json IS NOT NULL
        ORDER BY e.created_at DESC, e.id DESC
        LIMIT 1
    ),
    cvss_vector = (
        SELECT json_extract(e.data_json, '$.cvss_vector')
        FROM bug_enrichments e
        WHERE e.bug_id = bugs.id
          AND e.kind = 'security_evaluation'
          AND e.data_json IS NOT NULL
        ORDER BY e.created_at DESC, e.id DESC
        LIMIT 1
    )
WHERE EXISTS (
    SELECT 1
    FROM bug_enrichments e
    WHERE e.bug_id = bugs.id
      AND e.kind = 'security_evaluation'
      AND e.data_json IS NOT NULL
);
