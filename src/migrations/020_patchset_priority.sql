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

-- Add priority to patchsets to support fair queue scheduling and
-- prevent bulk submission floods from starving urgent single fixes.
ALTER TABLE patchsets ADD COLUMN priority INTEGER;
UPDATE patchsets SET priority = 500 WHERE priority IS NULL;

-- Composite index serving get_pending_patchsets in exact queue dispatch order.
CREATE INDEX IF NOT EXISTS idx_patchsets_pending_priority
    ON patchsets(status, priority DESC, date ASC);
