// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use sashiko::db::Database;
use sashiko::settings::DatabaseSettings;
use std::sync::Arc;

async fn setup_db() -> Arc<Database> {
    let settings = DatabaseSettings::memory();
    let db = Database::new(&settings).await.unwrap();
    db.migrate().await.unwrap();
    Arc::new(db)
}

#[tokio::test]
async fn test_merge_different_series_same_author_should_not_merge() {
    let db = setup_db().await;

    // 1. Create Thread
    let t1 = db.create_thread("root_bug", "Subject", 1000).await.unwrap();

    // 2. Create Patchset Series A - Part 1/2
    // [PATCH 1/2] Series A
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg_a_1",
            "[PATCH 1/2] Series A",
            "Author Same",
            1000,
            2, // total parts
            0,
            "",
            "",
            None,
            1, // index
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // 3. Create Patchset Series B - Part 1/2
    // [PATCH 1/2] Series B
    // Same author, same total parts, same version (implicit), close time
    let ps2 = db
        .create_patchset(
            t1,
            None,
            "msg_b_1",
            "[PATCH 1/2] Series B",
            "Author Same",
            1010, // 10s later
            2,    // total parts
            0,
            "",
            "",
            None,
            1, // index
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // 4. Assert they are DIFFERENT (should NOT merge)
    // If the bug exists, ps1 will equal ps2
    assert_ne!(
        ps1, ps2,
        "Different series (Series A vs Series B) from same author should NOT merge"
    );
}
