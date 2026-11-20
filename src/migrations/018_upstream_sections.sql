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

-- Sections whose maintainers hold an upstream grant
-- (projects/<project>/upstream_maintainers.toml) over a bug's or a
-- patchset's files. Every row confers the same access as a
-- 'maintainers_section' row in bug_subsystems or
-- patchset_maintainer_sections.
--
-- The rows are kept apart from those tables, rather than marked with a new
-- source value, so that the CHECK constraints there need no rebuild, so that
-- removing a grant can delete exactly the rows it produced, and so that the
-- subsystem names displayed for a bug or patchset stay what MAINTAINERS says.
-- Everything here is derived from the grant file and stored files and can be
-- rebuilt at any time; see designs/DESIGN_UPSTREAM_MAINTAINER_BUG_ACCESS.md.
CREATE TABLE IF NOT EXISTS bug_upstream_sections (
    bug_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    PRIMARY KEY (bug_id, subsystem),
    FOREIGN KEY(bug_id) REFERENCES bugs(id) ON DELETE CASCADE
);

-- Serves the bug list visibility filter, which asks for the bugs attributed
-- to a set of section titles.
CREATE INDEX IF NOT EXISTS idx_bug_upstream_sections_subsystem
    ON bug_upstream_sections(subsystem, bug_id);

CREATE TABLE IF NOT EXISTS patchset_upstream_sections (
    patchset_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    PRIMARY KEY (patchset_id, subsystem),
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id) ON DELETE CASCADE
);
