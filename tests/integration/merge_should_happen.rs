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
async fn test_merge_same_email_different_name_format() {
    let db = setup_db().await;

    // 1. Create Thread
    let t1 = db
        .create_thread("root_merge", "Subject", 1000)
        .await
        .unwrap();

    // 2. Create Patchset Part 1
    // "Alexander Graf <graf@amazon.com>"
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg_merge_1",
            "[PATCH 1/2] Series Merge",
            "Alexander Graf <graf@amazon.com>",
            1000,
            2,
            0,
            "",
            "",
            None,
            1,
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // 3. Create Patchset Part 2
    // "graf@amazon.com" (No name)
    let ps2 = db
        .create_patchset(
            t1,
            None,
            "msg_merge_2",
            "[PATCH 2/2] Series Merge",
            "graf@amazon.com",
            1010,
            2,
            0,
            "",
            "",
            None,
            2,
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // 4. Assert they MERGED (ps1 == ps2)
    assert_eq!(
        ps1, ps2,
        "Patchsets with same email but different name format SHOULD merge"
    );
}
