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
async fn test_cross_thread_complementary_merge() {
    let db = setup_db().await;

    // 1. Create Thread A
    let t_a = db
        .create_thread("root_a", "Series Subject", 1000)
        .await
        .unwrap();

    // First batch: patches 1 to 40 out of 60
    db.create_message(
        /* message_id: */ "patch_1",
        /* thread_id: */ t_a,
        /* in_reply_to: */ None,
        /* author: */ "Author",
        /* subject: */ "[PATCH 1/60] Fix bug part 1",
        /* date: */ 1000,
        /* body: */ "body",
        /* to: */ "",
        /* cc: */ "",
        /* git_blob_hash: */ None,
        /* mailing_list: */ None,
    )
    .await
    .unwrap();

    let ps1 = db
        .create_patchset(
            /* thread_id: */ t_a,
            /* cover_letter_message_id: */ None,
            /* message_id: */ "patch_1",
            /* subject: */ "[PATCH 1/60] Fix bug part 1",
            /* author: */ "Author",
            /* date: */ 1000,
            /* total_parts: */ 60,
            /* parser_version: */ 0,
            /* to: */ "",
            /* cc: */ "",
            /* version: */ None,
            /* part_index: */ 1,
            /* baseline_id: */ None,
            /* strict_author: */ true,
            /* skip_filters: */ None,
            /* only_filters: */ None,
        )
        .await
        .unwrap()
        .unwrap();

    db.create_patch(ps1, "patch_1", 1, "diff").await.unwrap();

    // 2. Resend second batch (41 to 60 out of 60) in Thread B
    let t_b = db
        .create_thread("root_b", "Series Subject", 1010)
        .await
        .unwrap();

    db.create_message(
        /* message_id: */ "patch_41",
        /* thread_id: */ t_b,
        /* in_reply_to: */ None, // No connection to Thread A
        /* author: */ "Author",
        /* subject: */ "[PATCH 41/60] Fix bug part 41",
        /* date: */ 1010,
        /* body: */ "body",
        /* to: */ "",
        /* cc: */ "",
        /* git_blob_hash: */ None,
        /* mailing_list: */ None,
    )
    .await
    .unwrap();

    let ps2 = db
        .create_patchset(
            /* thread_id: */ t_b,
            /* cover_letter_message_id: */ None,
            /* message_id: */ "patch_41",
            /* subject: */ "[PATCH 41/60] Fix bug part 41",
            /* author: */ "Author",
            /* date: */ 1010,
            /* total_parts: */ 60,
            /* parser_version: */ 0,
            /* to: */ "",
            /* cc: */ "",
            /* version: */ None,
            /* part_index: */ 41,
            /* baseline_id: */ None,
            /* strict_author: */ true,
            /* skip_filters: */ None,
            /* only_filters: */ None,
        )
        .await
        .unwrap()
        .unwrap();

    // 3. Assert that they DO merge (ps1 == ps2)
    assert_eq!(
        ps1, ps2,
        "Patchsets from different threads should merge when they are complementary parts of the same series."
    );
}
