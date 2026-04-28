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
async fn test_cover_letter_matching_with_different_versions_should_not_merge() {
    let db = setup_db().await;

    // Create Thread
    let t1 = db
        .create_thread("root1", "Test Series", 1000)
        .await
        .unwrap();

    // v1 Series: Part 1/2
    db.create_message(
        /* message_id: */ "v1_patch1",
        /* thread_id: */ t1,
        /* in_reply_to: */ None,
        /* author: */ "Author",
        /* subject: */ "[PATCH 1/2] Fix bug",
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
            /* thread_id: */ t1,
            /* cover_letter_message_id: */ None, // No cover letter yet
            /* message_id: */ "v1_patch1",
            /* subject: */ "[PATCH 1/2] Fix bug",
            /* author: */ "Author",
            /* date: */ 1000,
            /* total_parts: */ 2,
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

    db.create_patch(ps1, "v1_patch1", 1, "diff").await.unwrap();

    // v1 Series: Part 2/2 (reply to v1 1/2)
    db.create_message(
        /* message_id: */ "v1_patch2",
        /* thread_id: */ t1,
        /* in_reply_to: */ Some("v1_patch1"),
        /* author: */ "Author",
        /* subject: */ "[PATCH 2/2] Fix bug",
        /* date: */ 1010,
        /* body: */ "body",
        /* to: */ "",
        /* cc: */ "",
        /* git_blob_hash: */ None,
        /* mailing_list: */ None,
    )
    .await
    .unwrap();

    let ps1_b = db
        .create_patchset(
            /* thread_id: */ t1,
            /* cover_letter_message_id: */
            Some("v1_patch1"), // Treated as cover letter by in_reply_to logic
            /* message_id: */ "v1_patch2",
            /* subject: */ "[PATCH 2/2] Fix bug",
            /* author: */ "Author",
            /* date: */ 1010,
            /* total_parts: */ 2,
            /* parser_version: */ 0,
            /* to: */ "",
            /* cc: */ "",
            /* version: */ None,
            /* part_index: */ 2,
            /* baseline_id: */ None,
            /* strict_author: */ true,
            /* skip_filters: */ None,
            /* only_filters: */ None,
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(ps1, ps1_b); // They merge fine

    // v2 Series: Part 1/2 (in reply to v1 1/2)
    db.create_message(
        /* message_id: */ "v2_patch1",
        /* thread_id: */ t1,
        /* in_reply_to: */ Some("v1_patch1"),
        /* author: */ "Author",
        /* subject: */ "[PATCH v2 1/2] Fix bug",
        /* date: */ 1100,
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
            /* thread_id: */ t1,
            /* cover_letter_message_id: */
            Some("v1_patch1"), // Incoming patch has in_reply_to pointing to v1
            /* message_id: */ "v2_patch1",
            /* subject: */ "[PATCH v2 1/2] Fix bug",
            /* author: */ "Author",
            /* date: */ 1100,
            /* total_parts: */ 2,
            /* parser_version: */ 0,
            /* to: */ "",
            /* cc: */ "",
            /* version: */ Some(2),
            /* part_index: */ 1,
            /* baseline_id: */ None,
            /* strict_author: */ true,
            /* skip_filters: */ None,
            /* only_filters: */ None,
        )
        .await
        .unwrap()
        .unwrap();

    assert_ne!(
        ps1, ps2,
        "v2 patch 1/2 should NOT merge into v1 patchset via cover_letter_message_id matching"
    );
}
