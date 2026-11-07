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

//! Expected API injection grouping behavior.
//!
//! Each case asserts the desired grouping, so a split or a cross-submission
//! merge fails the test. Run them with:
//! cargo test --bin sashiko tests::api_injection
//! The parent module also covers bare singletons and same-author, same-subject
//! 0/1 submissions in both arrival orders.
//! This module also checks late covers, redelivery, and interleaved multipart
//! submissions, and drives the /api/submit handler end to end.

use super::{
    Database, Event, MessageSource, ProcessStatus, RawMboxSubmission, Settings,
    ingest_api_messages, ingest_api_messages_at, ingest_api_submission, process_parsed_article,
    test_db,
};
use sashiko::ReviewStatus;

const SUBMISSION_A: &str = "sashiko-inject-regression-a@sashiko.local";
const SUBMISSION_B: &str = "sashiko-inject-regression-b@sashiko.local";
const COVER_A: &str = "cover-a@example.com";
const COVER_B: &str = "cover-b@example.com";
const PATCH_A: &str = "patch-a@example.com";
const PATCH_B: &str = "patch-b@example.com";
const BOT: &str = "Kernel Build Bot <ci@example.com>";
const AUTHOR: &str = "Ryosuke Iwahashi <author@example.com>";
const OTHER_AUTHOR: &str = "Other Author <other@example.com>";
const COVER_TITLE: &str = "Series overview";
const COVER_BODY: &str = "CI cover letter\nbase-commit: 1111111111111111111111111111111111111111\n";
const DIFF: &str = "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";

#[derive(Clone, Copy)]
enum ArrivalOrder {
    CoverFirst,
    PatchFirst,
}

impl ArrivalOrder {
    fn messages(self, messages: &[Vec<u8>; 2]) -> [&[u8]; 2] {
        let [cover, patch] = messages;
        match self {
            Self::CoverFirst => [cover, patch],
            Self::PatchFirst => [patch, cover],
        }
    }
}

fn message(id: &str, author: &str, subject: &str, parent: Option<&str>, body: &str) -> Vec<u8> {
    let reply = parent
        .map(|id| format!("In-Reply-To: <{id}>\r\n"))
        .unwrap_or_default();
    format!("Message-ID: <{id}>\r\nFrom: {author}\r\nSubject: {subject}\r\n{reply}\r\n{body}")
        .into_bytes()
}

fn singleton(cover: &str, patch: &str, patch_author: &str, patch_title: &str) -> [Vec<u8>; 2] {
    [
        message(
            cover,
            BOT,
            &format!("[PATCH 0/1] {COVER_TITLE}"),
            None,
            COVER_BODY,
        ),
        message(
            patch,
            patch_author,
            &format!("[PATCH 1/1] {patch_title}"),
            Some(cover),
            DIFF,
        ),
    ]
}

async fn assert_submission(
    db: &Database,
    submission: &str,
    patches: &[&str],
    identity: &str,
) -> i64 {
    assert_submission_status(db, submission, patches, identity, ReviewStatus::Pending).await
}

async fn assert_submission_status(
    db: &Database,
    submission: &str,
    patches: &[&str],
    identity: &str,
    status: ReviewStatus,
) -> i64 {
    let id = db
        .find_patchset_id_by_msgid(submission)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("submission ID must resolve: {submission}"));
    for msgid in std::iter::once(submission)
        .chain(patches.iter().copied())
        .chain(std::iter::once(identity))
    {
        let details = db
            .get_patchset_details_by_msgid(msgid, None, None)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("details must resolve: {msgid}"));
        let summary = db
            .get_patchset_summary_by_msgid(msgid, None, None)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("summary must resolve: {msgid}"));
        assert_eq!(details["id"], id, "details for {msgid}");
        assert_eq!(summary["id"], id, "summary for {msgid}");
        assert_eq!(details["message_id"], identity);
        assert_eq!(details["status"], status.as_str());
        assert_eq!(summary["status"], status.as_str());
        assert_eq!(details["total_parts"], patches.len());
        assert_eq!(details["received_parts"], patches.len());
        let actual: Vec<_> = details["patches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|patch| patch["message_id"].as_str().unwrap())
            .collect();
        assert_eq!(actual, patches, "patch membership for {msgid}");
    }
    id
}

async fn assert_singleton_groups(author: &str, title: &str, order: ArrivalOrder) {
    let messages = singleton(COVER_A, PATCH_A, author, title);
    let db = ingest_api_submission(
        MessageSource::ApiInject,
        SUBMISSION_A,
        &order.messages(&messages),
    )
    .await;
    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 1);
    let id = assert_submission(&db, SUBMISSION_A, &[PATCH_A], COVER_A).await;
    assert_eq!(
        db.find_patchset_id_by_msgid(COVER_A).await.unwrap(),
        Some(id)
    );
    assert_eq!(
        db.get_message_body(COVER_A).await.unwrap().unwrap(),
        COVER_BODY
    );
}

#[tokio::test]
async fn same_author_different_subjects_cover_first() {
    assert_singleton_groups(BOT, "Independent patch title", ArrivalOrder::CoverFirst).await;
}

#[tokio::test]
async fn same_author_different_subjects_patch_first() {
    assert_singleton_groups(BOT, "Independent patch title", ArrivalOrder::PatchFirst).await;
}

#[tokio::test]
async fn different_authors_same_subject_cover_first() {
    assert_singleton_groups(AUTHOR, COVER_TITLE, ArrivalOrder::CoverFirst).await;
}

#[tokio::test]
async fn different_authors_same_subject_patch_first() {
    assert_singleton_groups(AUTHOR, COVER_TITLE, ArrivalOrder::PatchFirst).await;
}

#[tokio::test]
async fn different_authors_different_subjects_cover_first() {
    assert_singleton_groups(AUTHOR, "Independent patch title", ArrivalOrder::CoverFirst).await;
}

#[tokio::test]
async fn different_authors_different_subjects_patch_first() {
    assert_singleton_groups(AUTHOR, "Independent patch title", ArrivalOrder::PatchFirst).await;
}

fn multipart(cover: &str, first: &str, second: &str, authors: [&str; 3]) -> [Vec<u8>; 3] {
    [
        message(
            cover,
            authors[0],
            "[PATCH 0/2] Series overview",
            None,
            COVER_BODY,
        ),
        message(
            first,
            authors[1],
            "[PATCH 1/2] First fix",
            Some(cover),
            DIFF,
        ),
        message(
            second,
            authors[2],
            "[PATCH 2/2] Second fix",
            Some(cover),
            &DIFF.replace("+new", "+second"),
        ),
    ]
}

/// Whether a multipart submission includes its 0/N cover letter.
#[derive(Clone, Copy)]
enum Cover {
    Included,
    Omitted,
}

/// The order in which a multipart submission's messages arrive.
#[derive(Clone, Copy)]
enum MessageOrder {
    Forward,
    Reversed,
}

async fn assert_multipart_groups(cover_letter: Cover, order: MessageOrder) {
    let [cover, p1, p2] = multipart(COVER_A, PATCH_A, PATCH_B, [BOT, AUTHOR, OTHER_AUTHOR]);
    let mut messages = vec![p1.as_slice(), p2.as_slice()];
    if let Cover::Included = cover_letter {
        messages.insert(0, cover.as_slice());
    }
    if let MessageOrder::Reversed = order {
        messages.reverse();
    }
    let db = ingest_api_submission(MessageSource::ApiInject, SUBMISSION_A, &messages).await;
    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 1);
    assert_submission(&db, SUBMISSION_A, &[PATCH_A, PATCH_B], SUBMISSION_A).await;
}

#[tokio::test]
async fn multipart_different_authors_with_cover_forward() {
    assert_multipart_groups(Cover::Included, MessageOrder::Forward).await;
}

#[tokio::test]
async fn multipart_different_authors_with_cover_reverse() {
    assert_multipart_groups(Cover::Included, MessageOrder::Reversed).await;
}

#[tokio::test]
async fn multipart_different_authors_without_cover_forward() {
    assert_multipart_groups(Cover::Omitted, MessageOrder::Forward).await;
}

#[tokio::test]
async fn multipart_different_authors_without_cover_reverse() {
    assert_multipart_groups(Cover::Omitted, MessageOrder::Reversed).await;
}

async fn assert_isolated_singletons(db: &Database) {
    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 2);
    let a = assert_submission(db, SUBMISSION_A, &[PATCH_A], COVER_A).await;
    let b = assert_submission(db, SUBMISSION_B, &[PATCH_B], COVER_B).await;
    assert_ne!(a, b, "unrelated submissions must have separate patchsets");
    assert_eq!(
        db.find_patchset_id_by_msgid(COVER_A).await.unwrap(),
        Some(a)
    );
    assert_eq!(
        db.find_patchset_id_by_msgid(COVER_B).await.unwrap(),
        Some(b)
    );
}

#[tokio::test]
async fn sequential_submissions_with_same_author_and_subject_stay_separate() {
    let a = singleton(COVER_A, PATCH_A, BOT, COVER_TITLE);
    let b = singleton(COVER_B, PATCH_B, BOT, COVER_TITLE);
    let db = ingest_api_submission(
        MessageSource::ApiInject,
        SUBMISSION_A,
        &ArrivalOrder::CoverFirst.messages(&a),
    )
    .await;
    ingest_api_messages(
        &db,
        MessageSource::ApiInject,
        SUBMISSION_B,
        &ArrivalOrder::CoverFirst.messages(&b),
    )
    .await;
    assert_isolated_singletons(&db).await;
}

/// Checks that submission A's cover is still waiting for its patch and that
/// submission B's patch formed its own patchset.
async fn assert_cover_left_waiting(db: &Database) {
    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 2);
    let a_id = db
        .find_patchset_id_by_msgid(SUBMISSION_A)
        .await
        .unwrap()
        .unwrap();
    let a = db
        .get_patchset_details(a_id, None, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a["status"], ReviewStatus::Incomplete.as_str());
    assert_eq!(a["received_parts"], 0);
    assert!(a["patches"].as_array().unwrap().is_empty());
    let b_id = assert_submission(db, SUBMISSION_B, &[PATCH_B], PATCH_B).await;
    assert_ne!(a_id, b_id);
}

async fn finish_review(db: &Database, patchset: i64) -> serde_json::Value {
    let details = db
        .get_patchset_details(patchset, None, None)
        .await
        .unwrap()
        .unwrap();
    let patch = details["patches"][0]["id"].as_i64().unwrap();
    let review = db
        .create_review(patchset, Some(patch), "test", "test", None, None)
        .await
        .unwrap();
    db.complete_review(
        review,
        ReviewStatus::Reviewed.as_str(),
        "No findings",
        Some("Review complete"),
        None,
        Some("No findings"),
        None,
    )
    .await
    .unwrap();
    db.update_patch_status(patch, ReviewStatus::Reviewed.as_str())
        .await
        .unwrap();
    db.update_patchset_status(patchset, ReviewStatus::Reviewed.as_str())
        .await
        .unwrap();
    db.get_review_details(review).await.unwrap().unwrap()
}

async fn assert_late_cover_preserves_review(author: &str) {
    let messages = singleton(COVER_A, PATCH_A, author, "Independent patch title");
    let db = ingest_api_submission(MessageSource::ApiInject, SUBMISSION_A, &[&messages[1]]).await;
    let id = assert_submission(&db, SUBMISSION_A, &[PATCH_A], PATCH_A).await;
    let review_before = finish_review(&db, id).await;

    ingest_api_messages(&db, MessageSource::ApiInject, SUBMISSION_A, &[&messages[0]]).await;

    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 1);
    let resolved = assert_submission_status(
        &db,
        SUBMISSION_A,
        &[PATCH_A],
        COVER_A,
        ReviewStatus::Reviewed,
    )
    .await;
    assert_eq!(resolved, id, "late cover must keep the reviewed patchset");
    assert_eq!(
        db.get_latest_review_for_patchset(id)
            .await
            .unwrap()
            .unwrap(),
        review_before
    );
    assert!(db.get_pending_patchsets(10).await.unwrap().is_empty());
    assert_eq!(
        db.get_message_body(COVER_A).await.unwrap().unwrap(),
        COVER_BODY
    );
}

#[tokio::test]
async fn same_author_cover_after_review_keeps_completed_review() {
    assert_late_cover_preserves_review(BOT).await;
}

#[tokio::test]
async fn different_author_cover_after_review_keeps_completed_review() {
    assert_late_cover_preserves_review(AUTHOR).await;
}

async fn assert_multipart_isolation(cover_letter: Cover, authors: [&str; 3]) {
    let a_second = "second-a@example.com";
    let b_second = "second-b@example.com";
    let a = multipart(COVER_A, PATCH_A, a_second, authors);
    let b = multipart(COVER_B, PATCH_B, b_second, authors);
    let db = test_db().await;
    if let Cover::Included = cover_letter {
        ingest_api_messages(&db, MessageSource::ApiInject, SUBMISSION_A, &[&a[0]]).await;
        ingest_api_messages(&db, MessageSource::ApiInject, SUBMISSION_B, &[&b[0]]).await;
    }
    // Different indexes arrive across submissions before either series is full.
    for (submission, raw) in [
        (SUBMISSION_A, &a[1]),
        (SUBMISSION_B, &b[2]),
        (SUBMISSION_A, &a[2]),
        (SUBMISSION_B, &b[1]),
    ] {
        ingest_api_messages(&db, MessageSource::ApiInject, submission, &[raw]).await;
    }
    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 2);
    let a_id = assert_submission(&db, SUBMISSION_A, &[PATCH_A, a_second], SUBMISSION_A).await;
    let b_id = assert_submission(&db, SUBMISSION_B, &[PATCH_B, b_second], SUBMISSION_B).await;
    assert_ne!(a_id, b_id);
}

#[tokio::test]
async fn interleaved_multipart_same_author_with_covers_stay_separate() {
    assert_multipart_isolation(Cover::Included, [BOT; 3]).await;
}

#[tokio::test]
async fn interleaved_multipart_same_author_without_covers_stay_separate() {
    assert_multipart_isolation(Cover::Omitted, [BOT; 3]).await;
}

#[tokio::test]
async fn interleaved_multipart_different_authors_with_covers_stay_separate() {
    assert_multipart_isolation(Cover::Included, [BOT, AUTHOR, OTHER_AUTHOR]).await;
}

#[tokio::test]
async fn interleaved_multipart_different_authors_without_covers_stay_separate() {
    assert_multipart_isolation(Cover::Omitted, [BOT, AUTHOR, OTHER_AUTHOR]).await;
}

#[tokio::test]
async fn singleton_redelivery_keeps_the_same_patchset() {
    let messages = singleton(COVER_A, PATCH_A, BOT, "Independent patch title");
    let db = ingest_api_submission(
        MessageSource::ApiInject,
        SUBMISSION_A,
        &ArrivalOrder::CoverFirst.messages(&messages),
    )
    .await;
    let id = assert_submission(&db, SUBMISSION_A, &[PATCH_A], COVER_A).await;
    ingest_api_messages(
        &db,
        MessageSource::ApiInject,
        SUBMISSION_A,
        &ArrivalOrder::PatchFirst.messages(&messages),
    )
    .await;
    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 1);
    assert_eq!(
        assert_submission(&db, SUBMISSION_A, &[PATCH_A], COVER_A).await,
        id
    );
}

/// Seconds between the cover and the patch of one submission, kept inside the
/// 24 hour window the singleton fallback uses.
const ARRIVAL_GAPS: [i64; 3] = [0, 3600, 82_800];

async fn assert_singleton_groups_with_gap(author: &str, order: ArrivalOrder, gap: i64) {
    let messages = singleton(COVER_A, PATCH_A, author, "Independent patch title");
    let db = test_db().await;
    for (position, raw) in order.messages(&messages).into_iter().enumerate() {
        let received = 2000 + gap * position as i64;
        ingest_api_messages_at(
            &db,
            MessageSource::ApiInject,
            SUBMISSION_A,
            &[raw],
            received,
        )
        .await;
    }
    assert_eq!(
        db.count_patchsets(None, None).await.unwrap(),
        1,
        "gap {gap}"
    );
    assert_submission(&db, SUBMISSION_A, &[PATCH_A], COVER_A).await;
}

#[tokio::test]
async fn different_authors_group_regardless_of_arrival_gap() {
    for gap in ARRIVAL_GAPS {
        for order in [ArrivalOrder::CoverFirst, ArrivalOrder::PatchFirst] {
            assert_singleton_groups_with_gap(AUTHOR, order, gap).await;
        }
    }
}

#[tokio::test]
async fn same_author_groups_regardless_of_arrival_gap() {
    for gap in ARRIVAL_GAPS {
        for order in [ArrivalOrder::CoverFirst, ArrivalOrder::PatchFirst] {
            assert_singleton_groups_with_gap(BOT, order, gap).await;
        }
    }
}

/// A different-author patch with no parent link must not be pulled into
/// submission A even though it is the 1/1 its 0/1 cover is waiting for.
#[tokio::test]
async fn different_author_patch_without_parent_link_stays_separate() {
    let cover = singleton(COVER_A, PATCH_A, BOT, COVER_TITLE);
    let orphan = message(PATCH_B, AUTHOR, "[PATCH 1/1] Unrelated patch", None, DIFF);
    let db = ingest_api_submission(MessageSource::ApiInject, SUBMISSION_A, &[&cover[0]]).await;
    ingest_api_messages(&db, MessageSource::ApiInject, SUBMISSION_B, &[&orphan]).await;
    assert_cover_left_waiting(&db).await;
}

fn mbox(messages: &[Vec<u8>]) -> String {
    messages
        .iter()
        .map(|raw| {
            let text = String::from_utf8(raw.clone())
                .unwrap()
                .replace("\r\n", "\n");
            format!("From sashiko@example.com Mon Jan  1 00:00:00 2024\n{text}\n")
        })
        .collect()
}

/// Submits through the real /api/submit handler, then feeds the queued event
/// through the same mbox parsing the daemon uses.
async fn submit_over_http(db: &Database, raw: String) -> String {
    let mut settings = Settings::new().unwrap();
    settings.server.testing_mode = true;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(10);
    let (fetch_tx, _fetch_rx) = tokio::sync::mpsc::channel(10);
    let app = sashiko::server::build_router(
        std::sync::Arc::new(settings),
        // The inject path only queues an event, so the router's database is unused.
        std::sync::Arc::new(test_db().await),
        event_tx,
        fetch_tx,
        Default::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("http://{addr}/api/submit"))
        .json(&serde_json::json!({ "type": "inject", "raw": raw }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = response["id"].as_str().unwrap().to_string();

    let Some(Event::RawMboxSubmitted {
        raw,
        submission_id,
        source,
        group,
        baseline,
        skip_subjects,
        only_subjects,
        submitted_at,
    }) = event_rx.recv().await
    else {
        panic!("handler must queue a raw mbox event");
    };
    assert_eq!(submission_id, id, "returned ID must be the queued ID");
    let articles = RawMboxSubmission {
        raw,
        submission_id,
        source,
        group,
        baseline,
        skip_subjects,
        only_subjects,
        submitted_at,
    }
    .into_articles()
    .unwrap();
    let policy = sashiko::email_policy::EmailPolicyConfig::default();
    let review_settings = Settings::new().unwrap().review;
    for article in articles {
        assert_eq!(
            process_parsed_article(db, article, None, &policy, &[], &review_settings).await,
            ProcessStatus::Ingested
        );
    }
    id
}

async fn assert_http_submission_groups(author: &str, order: ArrivalOrder) {
    let messages = singleton(COVER_A, PATCH_A, author, "Independent patch title");
    let raw = mbox(&order.messages(&messages).map(<[u8]>::to_vec));
    let db = test_db().await;
    let id = submit_over_http(&db, raw).await;
    assert_eq!(db.count_patchsets(None, None).await.unwrap(), 1);
    assert_submission(&db, &id, &[PATCH_A], COVER_A).await;
}

#[tokio::test]
async fn http_submit_same_author_cover_first_groups() {
    assert_http_submission_groups(BOT, ArrivalOrder::CoverFirst).await;
}

#[tokio::test]
async fn http_submit_same_author_patch_first_groups() {
    assert_http_submission_groups(BOT, ArrivalOrder::PatchFirst).await;
}

#[tokio::test]
async fn http_submit_different_authors_cover_first_groups() {
    assert_http_submission_groups(AUTHOR, ArrivalOrder::CoverFirst).await;
}

#[tokio::test]
async fn http_submit_different_authors_patch_first_groups() {
    assert_http_submission_groups(AUTHOR, ArrivalOrder::PatchFirst).await;
}
