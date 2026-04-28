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
    let settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: String::new(),
    };
    let db = Database::new(&settings).await.unwrap();
    db.migrate().await.unwrap();
    Arc::new(db)
}

#[tokio::test]
async fn test_merge_prefixes_mismatch_should_split() {
    let db = setup_db().await;

    // 1. Create Thread 1
    let t1 = db
        .create_thread("root_prefix_1", "Subject A", 1000)
        .await
        .unwrap();

    // 2. Create Patchset A - Part 1/2 with Prefix "net-next"
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg_a_1",
            "[PATCH net-next 1/2] Series A",
            "Author Same",
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

    // 3. Create Thread 2 (Different thread context)
    let t2 = db
        .create_thread("root_prefix_2", "Subject B", 1010)
        .await
        .unwrap();

    // 4. Create Patchset B - Part 2/2 with NO Prefix
    // Same author, same total, close time.
    let ps2 = db
        .create_patchset(
            t2, // DIFFERENT THREAD
            None,
            "msg_b_2",
            "[PATCH 2/2] Series B",
            "Author Same",
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

    // 4. Assert they are DIFFERENT (should NOT merge)
    assert_ne!(
        ps1, ps2,
        "Mismatching prefixes (net-next vs empty) should prevent merge"
    );
}

#[tokio::test]
async fn test_merge_prefixes_match_should_merge_if_complementary() {
    let db = setup_db().await;

    let t1 = db
        .create_thread("root_prefix_match", "Subject", 2000)
        .await
        .unwrap();

    // [PATCH net-next 1/2]
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg_c_1",
            "[PATCH net-next 1/2] Series C",
            "Author Match",
            2000,
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

    // Create Thread 2
    let t2 = db
        .create_thread("root_prefix_match_2", "Subject C", 2010)
        .await
        .unwrap();

    // [PATCH net-next 2/2]
    let ps2 = db
        .create_patchset(
            t2, // DIFFERENT THREAD
            None,
            "msg_c_2",
            "[PATCH net-next 2/2] Series C",
            "Author Match",
            2010,
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

    assert_eq!(
        ps1, ps2,
        "Matching prefixes (net-next) SHOULD merge across threads when they are complementary"
    );
}
