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

-- Migration 023: O(1) Prometheus statistics tables, live state gauges,
-- BugAccess-aware subsystem facets, multi-resolution rollups, and compact
-- per-review pipeline summaries.

CREATE TABLE IF NOT EXISTS stats_state_gauges (
    metric TEXT NOT NULL,
    labels_json TEXT NOT NULL DEFAULT '{}',
    value INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (metric, labels_json)
);

-- Normalized (bug_id, subsystem, is_authorizing) projection table whose
-- triggers keep stats_bug_subsystem_facets exact across inserts, updates,
-- overlapping MAINTAINERS + upstream_grant attributions, and bug deletions.
CREATE TABLE IF NOT EXISTS stats_bug_section_links (
    bug_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    is_authorizing INTEGER NOT NULL CHECK (is_authorizing IN (0, 1)),
    lifecycle_status TEXT NOT NULL,
    severity_int INTEGER NOT NULL,
    security_rating TEXT NOT NULL,
    PRIMARY KEY (bug_id, subsystem, is_authorizing)
);

CREATE INDEX IF NOT EXISTS idx_stats_bug_section_links_bug
    ON stats_bug_section_links(bug_id);

CREATE TABLE IF NOT EXISTS stats_bug_subsystem_facets (
    subsystem TEXT NOT NULL,
    is_authorizing INTEGER NOT NULL CHECK (is_authorizing IN (0, 1)),
    lifecycle_status TEXT NOT NULL,
    severity_int INTEGER NOT NULL,
    security_rating TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (
        subsystem,
        is_authorizing,
        lifecycle_status,
        severity_int,
        security_rating
    )
);

CREATE INDEX IF NOT EXISTS idx_stats_bug_subsys_rank
    ON stats_bug_subsystem_facets(is_authorizing, lifecycle_status, count DESC);

CREATE INDEX IF NOT EXISTS idx_stats_bug_subsys_nocase
    ON stats_bug_subsystem_facets(subsystem COLLATE NOCASE, is_authorizing);

CREATE TABLE IF NOT EXISTS stats_series (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    metric TEXT NOT NULL,
    labels_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE(metric, labels_json)
);

-- resolution_secs: 300 (5m, kept 48h), 3600 (1h, kept 45d), 86400 (1d, kept 5y).
CREATE TABLE IF NOT EXISTS stats_rollups (
    series_id INTEGER NOT NULL,
    resolution_secs INTEGER NOT NULL,
    bucket_ts INTEGER NOT NULL,
    value REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (series_id, resolution_secs, bucket_ts),
    FOREIGN KEY(series_id) REFERENCES stats_series(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_stats_rollups_expiry
    ON stats_rollups(resolution_secs, bucket_ts);

-- Pre-extracted uncompressed per-review pipeline summary (< 500 bytes/row) so
-- Last-N pipeline queries never decompress ai_interactions.output_raw blobs.
CREATE TABLE IF NOT EXISTS review_pipeline_stats (
    review_id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    project TEXT NOT NULL DEFAULT '',
    provider TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT '',
    inherited INTEGER NOT NULL DEFAULT 0,
    raw_concerns INTEGER NOT NULL DEFAULT 0,
    stage1_dismissed INTEGER NOT NULL DEFAULT 0,
    deduplicated_clusters INTEGER NOT NULL DEFAULT 0,
    verified_1a INTEGER NOT NULL DEFAULT 0,
    dismissed_1b INTEGER NOT NULL DEFAULT 0,
    inconclusive INTEGER NOT NULL DEFAULT 0,
    verified_pv INTEGER NOT NULL DEFAULT 0,
    dismissed_pv INTEGER NOT NULL DEFAULT 0,
    findings_patch INTEGER NOT NULL DEFAULT 0,
    findings_preexisting INTEGER NOT NULL DEFAULT 0,
    tokens_in INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    tokens_cached INTEGER NOT NULL DEFAULT 0,
    stages_json TEXT NOT NULL DEFAULT '[]',
    overlap_json TEXT NOT NULL DEFAULT '[]',
    tools_json TEXT NOT NULL DEFAULT '[]',
    prior_json TEXT NOT NULL DEFAULT '{}',
    FOREIGN KEY(review_id) REFERENCES reviews(id) ON DELETE CASCADE,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_review_pipeline_stats_patchset
    ON review_pipeline_stats(patchset_id);

-- Projection triggers from stats_bug_section_links -> stats_bug_subsystem_facets.
DROP TRIGGER IF EXISTS trg_stats_bug_links_insert;
CREATE TRIGGER trg_stats_bug_links_insert
AFTER INSERT ON stats_bug_section_links
FOR EACH ROW
BEGIN
    INSERT INTO stats_bug_subsystem_facets (
        subsystem, is_authorizing, lifecycle_status, severity_int, security_rating, count
    ) VALUES (
        new.subsystem, new.is_authorizing, new.lifecycle_status, new.severity_int, new.security_rating, 1
    )
    ON CONFLICT(subsystem, is_authorizing, lifecycle_status, severity_int, security_rating)
    DO UPDATE SET count = count + 1;
END;

DROP TRIGGER IF EXISTS trg_stats_bug_links_update;
CREATE TRIGGER trg_stats_bug_links_update
AFTER UPDATE ON stats_bug_section_links
FOR EACH ROW
WHEN old.subsystem IS NOT new.subsystem
  OR old.is_authorizing IS NOT new.is_authorizing
  OR old.lifecycle_status IS NOT new.lifecycle_status
  OR old.severity_int IS NOT new.severity_int
  OR old.security_rating IS NOT new.security_rating
BEGIN
    UPDATE stats_bug_subsystem_facets
    SET count = MAX(0, count - 1)
    WHERE subsystem = old.subsystem
      AND is_authorizing = old.is_authorizing
      AND lifecycle_status = old.lifecycle_status
      AND severity_int = old.severity_int
      AND security_rating = old.security_rating;

    INSERT INTO stats_bug_subsystem_facets (
        subsystem, is_authorizing, lifecycle_status, severity_int, security_rating, count
    ) VALUES (
        new.subsystem, new.is_authorizing, new.lifecycle_status, new.severity_int, new.security_rating, 1
    )
    ON CONFLICT(subsystem, is_authorizing, lifecycle_status, severity_int, security_rating)
    DO UPDATE SET count = count + 1;
END;

DROP TRIGGER IF EXISTS trg_stats_bug_links_delete;
CREATE TRIGGER trg_stats_bug_links_delete
AFTER DELETE ON stats_bug_section_links
FOR EACH ROW
BEGIN
    UPDATE stats_bug_subsystem_facets
    SET count = MAX(0, count - 1)
    WHERE subsystem = old.subsystem
      AND is_authorizing = old.is_authorizing
      AND lifecycle_status = old.lifecycle_status
      AND severity_int = old.severity_int
      AND security_rating = old.security_rating;
END;
