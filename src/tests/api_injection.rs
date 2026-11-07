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

//! Expected API injection behavior, including executable specifications of gaps.
//!
//! Run the known failures with:
//! cargo test --bin sashiko tests::api_injection -- --ignored
//! Validate a fix against the controls and known failures together with:
//! cargo test --bin sashiko tests::api_injection -- --include-ignored
//! Ignored cases assert the desired behavior, rather than accepting a split or
//! a cross-submission merge as correct. Remove each ignore when its bug is fixed.
//! The parent module also covers bare singletons and same-author, same-subject
//! 0/1 submissions in both arrival orders.
//! This module also checks late covers, redelivery, and interleaved multipart
//! submissions.

use super::{Database, MessageSource, ingest_api_messages, ingest_api_submission, test_db};
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
#[ignore = "Known bug: strict singleton author matching splits an injected 0/1 and 1/1"]
async fn different_authors_same_subject_cover_first() {
    assert_singleton_groups(AUTHOR, COVER_TITLE, ArrivalOrder::CoverFirst).await;
}

#[tokio::test]
#[ignore = "Known bug: strict singleton author matching splits an injected 0/1 and 1/1"]
async fn different_authors_same_subject_patch_first() {
    assert_singleton_groups(AUTHOR, COVER_TITLE, ArrivalOrder::PatchFirst).await;
}

#[tokio::test]
#[ignore = "Known bug: strict singleton author matching splits an injected 0/1 and 1/1"]
async fn different_authors_different_subjects_cover_first() {
    assert_singleton_groups(AUTHOR, "Independent patch title", ArrivalOrder::CoverFirst).await;
}

#[tokio::test]
#[ignore = "Known bug: strict singleton author matching splits an injected 0/1 and 1/1"]
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
#[ignore = "Known bug: strict author matching rejects a different-author cover after review"]
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
