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

use sashiko::db::{Database, Finding, Severity};
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
async fn test_findings_sum_across_patches() {
    let db = setup_db().await;

    // 1. Create Thread
    let t1 = db.create_thread("root1", "Subject", 1000).await.unwrap();

    // 2. Create Messages
    db.create_message(
        "msg1",
        t1,
        None,
        "Author",
        "[PATCH 0/2] Fix something",
        1000,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();
    db.create_message(
        "msg2",
        t1,
        None,
        "Author",
        "[PATCH 1/2] Fix part 1",
        1001,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();
    db.create_message(
        "msg3",
        t1,
        None,
        "Author",
        "[PATCH 2/2] Fix part 2",
        1002,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    // 2.5 Create Patchset
    let ps_id = db
        .create_patchset(
            t1,
            None,
            "msg1",
            "[PATCH 0/2] Fix something",
            "Author",
            1000,
            2,
            0,
            "",
            "",
            None,
            0,
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // 3. Create Patches
    let p1_id = db.create_patch(ps_id, "msg2", 1, "diff1").await.unwrap();
    let p2_id = db.create_patch(ps_id, "msg3", 2, "diff2").await.unwrap();

    // 4. Create Reviews
    let r1_id = db
        .create_review(ps_id, Some(p1_id), "provider", "model", None, None)
        .await
        .unwrap();
    let r2_id = db
        .create_review(ps_id, Some(p2_id), "provider", "model", None, None)
        .await
        .unwrap();

    // Update reviews to Reviewed
    db.update_review_status(r1_id, "Reviewed", None)
        .await
        .unwrap();
    db.update_review_status(r2_id, "Reviewed", None)
        .await
        .unwrap();

    // 5. Add findings to Review 1 (2 low, 1 high)
    db.create_finding(Finding {
        review_id: r1_id,
        severity: Severity::Low,
        severity_explanation: Some("low1".into()),
        problem: "low1".into(),
        file_path: None,
        line_number: None,
    })
    .await
    .unwrap();
    db.create_finding(Finding {
        review_id: r1_id,
        severity: Severity::Low,
        severity_explanation: Some("low2".into()),
        problem: "low2".into(),
        file_path: None,
        line_number: None,
    })
    .await
    .unwrap();
    db.create_finding(Finding {
        review_id: r1_id,
        severity: Severity::High,
        severity_explanation: Some("high1".into()),
        problem: "high1".into(),
        file_path: None,
        line_number: None,
    })
    .await
    .unwrap();

    // 6. Add findings to Review 2 (1 low, 2 critical)
    db.create_finding(Finding {
        review_id: r2_id,
        severity: Severity::Low,
        severity_explanation: Some("low3".into()),
        problem: "low3".into(),
        file_path: None,
        line_number: None,
    })
    .await
    .unwrap();
    db.create_finding(Finding {
        review_id: r2_id,
        severity: Severity::Critical,
        severity_explanation: Some("crit1".into()),
        problem: "crit1".into(),
        file_path: None,
        line_number: None,
    })
    .await
    .unwrap();
    db.create_finding(Finding {
        review_id: r2_id,
        severity: Severity::Critical,
        severity_explanation: Some("crit2".into()),
        problem: "crit2".into(),
        file_path: None,
        line_number: None,
    })
    .await
    .unwrap();

    // 7. Get patchsets and verify findings counts
    let patchsets = db.get_patchsets(10, 0, None, None).await.unwrap();
    assert_eq!(patchsets.len(), 1);

    let ps = &patchsets[0];

    // Low: 2 + 1 = 3
    // Medium: 0
    // High: 1
    // Critical: 2
    assert_eq!(
        ps.findings_low,
        Some(3),
        "Low findings should sum across patches"
    );
    assert_eq!(
        ps.findings_medium,
        Some(0),
        "Medium findings should sum across patches"
    );
    assert_eq!(
        ps.findings_high,
        Some(1),
        "High findings should sum across patches"
    );
    assert_eq!(
        ps.findings_critical,
        Some(2),
        "Critical findings should sum across patches"
    );
}
