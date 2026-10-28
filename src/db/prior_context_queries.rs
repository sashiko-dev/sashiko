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

//! Database persistence and lineage queries for transport-agnostic `PriorVersionContext`.

use super::{Database, Severity};
use crate::authority::{
    ContributorAuthority, PatchAuthorityEvaluator, PatchsetAuthority, SpeakerRole, TrustTier,
};
use crate::forge::ForgeFetchedComment;
use crate::maintainers::MaintainersIndex;
use crate::prior_context::{
    OriginKind, PatchRoutingTarget, PriorAttribution, PriorChangelogBlock, PriorCommentMessage,
    PriorDiscussionThread, PriorFindingItem, PriorItemScope, PriorVersionContext, SASHIKO_IDENTITY,
    compute_inter_version_diff, extract_patch_touched_files, extract_version_changelog_blocks,
    finalize_prior_version_context, is_non_actionable_reply_own_text, is_sashiko_outbox_msgid,
    is_sashiko_review_message, message_own_text, normalize_msgid, route_unattached_text_to_patches,
    safe_external_reference_url, safe_lore_reference_url, trim_email_quotes,
};
use crate::project::ProjectId;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

/// Upper bound on ancestor versions walked from vN. Large Linux series
/// routinely reach 20-30 revisions; the row, body and 512 KiB prompt budgets
/// below keep the cost of long chains bounded (newest versions win).
const MAX_ANCESTOR_VERSIONS: usize = 50;
/// Upper bound on header rows loaded per review across every ancestor thread
/// and reply hop, so one enormous thread cannot monopolize the connection.
const MAX_CANDIDATE_EMAIL_ROWS: usize = 50_000;
/// Rows loaded from a single `thread_id`.
const MAX_ROWS_PER_THREAD: usize = 20_000;
/// Upper bound on email bodies fetched and decompressed per review.
const MAX_EMAIL_BODIES: usize = 10_000;
/// Values bound per `IN (SELECT value FROM json_each(?))` query.
const SQL_IN_BATCH: usize = 500;

#[derive(Debug, Clone)]
struct ChainPatchsetRow {
    id: i64,
    thread_id: Option<i64>,
    cover_letter_message_id: Option<String>,
    subject: String,
    author: String,
    date: i64,
    mr_url: Option<String>,
    mr_number: Option<i64>,
    slug: Option<String>,
    stored_version: Option<i64>,
    previous_patchset_id: Option<i64>,
    effective_version: u32,
}

#[derive(Debug)]
struct RawPatchEntry {
    id: i64,
    patchset_id: i64,
    message_id: String,
    part_index: i64,
    previous_patch_id: Option<i64>,
    diff_val: libsql::Value,
    author: Option<String>,
    body_val: Option<libsql::Value>,
    thread_id: Option<i64>,
}

#[derive(Debug, Clone)]
struct DecodedPatchEntry {
    id: i64,
    patchset_id: i64,
    message_id: String,
    part_index: i64,
    previous_patch_id: Option<i64>,
    diff: String,
    author: Option<String>,
    body: String,
    thread_id: Option<i64>,
}

/// Header-only view of a candidate reply email. Bodies are loaded later, and
/// only for messages that survive subtree isolation.
#[derive(Debug, Clone)]
struct EmailMessageMeta {
    id: i64,
    message_id: String,
    norm_id: String,
    in_reply_to: Option<String>,
    references_hdr: Option<String>,
    author: String,
    subject: String,
    date: i64,
}

/// Column list matching `EmailMessageMeta::from_row`.
const EMAIL_META_COLUMNS: &str = "id, message_id, in_reply_to, references_hdr,
     COALESCE(author, ''), COALESCE(subject, ''), COALESCE(date, 0)";

impl EmailMessageMeta {
    fn from_row(r: &libsql::Row) -> Result<Self> {
        let message_id: String = r.get(1)?;
        Ok(Self {
            id: r.get(0)?,
            norm_id: normalize_msgid(&message_id),
            message_id,
            in_reply_to: r.get(2)?,
            references_hdr: r.get(3)?,
            author: r.get(4)?,
            subject: r.get(5)?,
            date: r.get(6)?,
        })
    }
}

/// One isolated reply subtree under an ancestor cover letter or patch.
#[derive(Debug)]
struct EmailSubtree<'a> {
    root_kind: AncestorRootKind,
    top_subject: &'a str,
    members: Vec<&'a EmailMessageMeta>,
}

#[derive(Debug)]
struct RawForgeCommentRow {
    id: i64,
    comment_source: String,
    remote_comment_id: i64,
    in_reply_to_id: Option<i64>,
    author: String,
    author_association: Option<String>,
    commit_id: Option<String>,
    file_path: Option<String>,
    line: Option<i64>,
    body_val: libsql::Value,
    html_url: Option<String>,
    created_at: i64,
}

#[derive(Debug, Clone)]
struct DecodedForgeCommentRow {
    id: i64,
    comment_source: String,
    remote_comment_id: i64,
    in_reply_to_id: Option<i64>,
    author: String,
    author_association: Option<String>,
    commit_id: Option<String>,
    file_path: Option<String>,
    line: Option<i64>,
    body: String,
    html_url: Option<String>,
    created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AncestorRootKind {
    Cover { patchset_id: i64, version: u32 },
    Patch { version: u32, patch_id: i64 },
}

/// Ancestor patch and cover-letter roots whose standalone Sashiko review emails
/// are already represented (or superseded) by a completed DB review in their
/// patch lineage.
#[derive(Debug, Default)]
struct CoveredSashikoRoots {
    patch_ids: HashSet<i64>,
    patchset_ids: HashSet<i64>,
}

impl CoveredSashikoRoots {
    fn contains(&self, kind: AncestorRootKind) -> bool {
        match kind {
            AncestorRootKind::Patch { patch_id, .. } => self.patch_ids.contains(&patch_id),
            AncestorRootKind::Cover { patchset_id, .. } => self.patchset_ids.contains(&patchset_id),
        }
    }
}

#[derive(Debug, Clone)]
struct UnscoredMessage {
    message_id: String,
    author: String,
    author_association: Option<String>,
    timestamp: i64,
    file_path: Option<String>,
    line: Option<i64>,
    reference_url: Option<String>,
    trimmed_body: String,
}

#[derive(Debug, Clone)]
struct PendingThread {
    version: u32,
    subject: Option<String>,
    target_ancestor_patch_id: Option<i64>,
    explicit_file_path: Option<String>,
    messages: Vec<UnscoredMessage>,
}

/// Inputs for `Database::build_prior_version_contexts_with`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PriorContextOptions<'a> {
    /// Repository whose `.mailmap` canonicalizes speaker identities.
    pub repo_path: Option<&'a Path>,
    /// Review baseline commit `.mailmap` is read at, so identities resolve
    /// the same way for every review on that baseline. `None` uses the
    /// repository's default branch.
    pub baseline_commit: Option<&'a str>,
    /// Project whose trusted-bot whitelist applies.
    pub project: ProjectId,
    /// `MAINTAINERS` index; `None` uses the process-wide index.
    pub maintainers: Option<&'a MaintainersIndex>,
    /// Configured forge provider (`forge.provider`). `None` derives it from
    /// the pull request URL host.
    pub forge_provider: Option<&'a str>,
    /// Configured Sashiko forge login (`forge.bot_login`).
    pub forge_bot_login: Option<&'a str>,
}

impl Database {
    /// Upserts fetched Forge PR review summaries, inline diff comments, and
    /// issue comments with transparent gzip compression on `body`.
    pub async fn upsert_forge_comments(&self, comments: &[ForgeFetchedComment]) -> Result<usize> {
        if comments.is_empty() {
            return Ok(0);
        }

        // Compress bodies outside the write transaction so CPU work never holds
        // the SQLite writer lock.
        let mut compressed_bodies = Vec::with_capacity(comments.len());
        for c in comments {
            compressed_bodies.push(crate::compression::compress_string_async(&c.body).await);
        }

        let tx_guard = if self.in_transaction {
            None
        } else {
            Some(self.begin_immediate_transaction().await?)
        };
        let tx_db = match tx_guard.as_ref() {
            Some(tx) => self.with_connection((**tx).clone()),
            None => self.with_connection(self.conn.clone()),
        };

        let mut count = 0usize;
        for (c, body_val) in comments.iter().zip(compressed_bodies) {
            tx_db
                .execute(
                    "INSERT INTO forge_comments (
                        provider, repo, pr_number, comment_source, remote_comment_id,
                        in_reply_to_id, author, author_association, commit_id,
                        file_path, line, body, html_url, created_at, updated_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(provider, repo, comment_source, remote_comment_id) DO UPDATE SET
                        body = excluded.body,
                        updated_at = excluded.updated_at,
                        author = excluded.author,
                        in_reply_to_id = COALESCE(excluded.in_reply_to_id, forge_comments.in_reply_to_id),
                        commit_id = COALESCE(excluded.commit_id, forge_comments.commit_id),
                        file_path = COALESCE(excluded.file_path, forge_comments.file_path),
                        line = COALESCE(excluded.line, forge_comments.line),
                        author_association = COALESCE(excluded.author_association, forge_comments.author_association),
                        html_url = COALESCE(excluded.html_url, forge_comments.html_url)",
                    libsql::params![
                        c.provider.as_str(),
                        c.repo.as_str(),
                        c.pr_number,
                        c.comment_source.as_str(),
                        c.remote_comment_id,
                        c.in_reply_to_id,
                        c.author.as_str(),
                        c.author_association.as_deref(),
                        c.commit_id.as_deref(),
                        c.file_path.as_deref(),
                        c.line,
                        body_val,
                        c.html_url.as_deref(),
                        c.created_at,
                        c.updated_at,
                    ],
                )
                .await?;
            count += 1;
        }

        if let Some(tx) = tx_guard {
            tx.commit().await?;
        }
        Ok(count)
    }

    /// Walks `patchsets.previous_patchset_id` across `v(N-1)..v1` and builds a
    /// transport-agnostic `PriorVersionContext` per `vN` patch (`part_index >= 1`).
    pub async fn build_prior_version_contexts(
        &self,
        patchset_id: i64,
        repo_path: Option<&Path>,
        maintainers_override: Option<&MaintainersIndex>,
    ) -> Result<HashMap<i64, PriorVersionContext>> {
        self.build_prior_version_contexts_with(
            patchset_id,
            &PriorContextOptions {
                repo_path,
                maintainers: maintainers_override,
                ..PriorContextOptions::default()
            },
        )
        .await
    }

    /// `build_prior_version_contexts` with every input spelled out.
    pub async fn build_prior_version_contexts_with(
        &self,
        patchset_id: i64,
        opts: &PriorContextOptions<'_>,
    ) -> Result<HashMap<i64, PriorVersionContext>> {
        let Some((curr_ps, ancestors)) = self.load_ancestor_patchset_chain(patchset_id).await?
        else {
            return Ok(HashMap::new());
        };
        if ancestors.is_empty() {
            return Ok(HashMap::new());
        }

        let mut all_ps_ids = Vec::with_capacity(1 + ancestors.len());
        all_ps_ids.push(curr_ps.id);
        for anc in &ancestors {
            all_ps_ids.push(anc.id);
        }

        let (patches_by_ps, cover_bodies) = self
            .load_and_decode_chain_patches(&curr_ps, &ancestors)
            .await?;
        let Some(curr_patches) = patches_by_ps.get(&curr_ps.id) else {
            return Ok(HashMap::new());
        };
        let v_n_patches: Vec<&DecodedPatchEntry> =
            curr_patches.iter().filter(|p| p.part_index >= 1).collect();
        if v_n_patches.is_empty() {
            return Ok(HashMap::new());
        }

        let mut patches_by_id: HashMap<i64, &DecodedPatchEntry> = HashMap::new();
        for list in patches_by_ps.values() {
            for p in list {
                patches_by_id.insert(p.id, p);
            }
        }
        let version_by_ps_id: HashMap<i64, u32> =
            std::iter::once((curr_ps.id, curr_ps.effective_version))
                .chain(ancestors.iter().map(|a| (a.id, a.effective_version)))
                .collect();

        let routing_targets: Vec<PatchRoutingTarget> = v_n_patches
            .iter()
            .map(|p| PatchRoutingTarget::from_diff(p.id, p.part_index, &p.diff))
            .collect();

        // Map every ancestor patch linked from `vN` to its `vN` descendant patch ID.
        let mut vn_descendant_of_anc_patch: HashMap<i64, i64> = HashMap::new();
        let mut patches_with_newer_successor: HashSet<i64> = HashSet::new();
        for list in patches_by_ps.values() {
            for p in list {
                if let Some(prev_id) = p.previous_patch_id {
                    patches_with_newer_successor.insert(prev_id);
                }
            }
        }
        let mut lineage_by_vn_patch: HashMap<i64, Vec<i64>> = HashMap::new();
        for vp in &v_n_patches {
            let chain = walk_patch_ancestors(vp.previous_patch_id, &patches_by_id);
            for &anc_pid in &chain {
                vn_descendant_of_anc_patch.entry(anc_pid).or_insert(vp.id);
            }
            lineage_by_vn_patch.insert(vp.id, chain);
        }

        let ancestor_versions: Vec<u32> = ancestors.iter().map(|a| a.effective_version).collect();
        let mut contexts: HashMap<i64, PriorVersionContext> = HashMap::new();
        for vp in &v_n_patches {
            let mut ctx = PriorVersionContext {
                current_version: curr_ps.effective_version,
                ancestor_versions: ancestor_versions.clone(),
                ..PriorVersionContext::default()
            };
            populate_interdiff_and_changelogs(
                &mut ctx,
                vp,
                &curr_ps,
                &ancestors,
                &lineage_by_vn_patch,
                &patches_by_id,
                &version_by_ps_id,
                &cover_bodies,
            );
            contexts.insert(vp.id, ctx);
        }

        let covered_sashiko_roots = self
            .attach_prior_sashiko_findings(
                &mut contexts,
                &v_n_patches,
                &ancestors,
                &patches_by_ps,
                &patches_by_id,
                &lineage_by_vn_patch,
                &patches_with_newer_successor,
                &version_by_ps_id,
                &routing_targets,
            )
            .await?;

        let mut pending_threads = self
            .collect_email_discussion_threads(
                &curr_ps,
                &ancestors,
                &patches_by_ps,
                &covered_sashiko_roots,
            )
            .await?;
        let forge_threads = self
            .collect_forge_discussion_threads(&curr_ps, &ancestors, &patches_by_ps, opts)
            .await?;
        pending_threads.extend(forge_threads);

        let routed = route_pending_threads(
            pending_threads,
            &vn_descendant_of_anc_patch,
            &v_n_patches,
            &patches_by_ps,
            &patches_by_id,
            &lineage_by_vn_patch,
            &routing_targets,
        );
        // Authority is loaded once per patchset and only when some thread
        // actually has a destination.
        if routed.iter().any(|r| !r.destinations.is_empty()) {
            let authority = PatchsetAuthority::load(
                opts.repo_path,
                opts.baseline_commit,
                collect_chain_authors(&curr_ps, &ancestors, &patches_by_ps),
                opts.maintainers,
                opts.project.trusted_bots(),
            )
            .await;
            let evaluators = build_target_evaluators(&authority, &routed, &v_n_patches);
            score_routed_threads(&mut contexts, routed, &evaluators);
        }

        for ctx in contexts.values_mut() {
            finalize_prior_version_context(ctx);
        }
        Ok(contexts)
    }

    async fn load_ancestor_patchset_chain(
        &self,
        patchset_id: i64,
    ) -> Result<Option<(ChainPatchsetRow, Vec<ChainPatchsetRow>)>> {
        let Some(mut curr) = self.fetch_chain_patchset_row(patchset_id).await? else {
            return Ok(None);
        };
        let mut visited = HashSet::new();
        visited.insert(curr.id);

        let mut ancestors = Vec::new();
        let mut next_id = curr.previous_patchset_id;
        while let Some(anc_id) = next_id {
            if ancestors.len() >= MAX_ANCESTOR_VERSIONS || !visited.insert(anc_id) {
                break;
            }
            let Some(anc_row) = self.fetch_chain_patchset_row(anc_id).await? else {
                break;
            };
            next_id = anc_row.previous_patchset_id;
            ancestors.push(anc_row);
        }

        assign_effective_versions(&mut curr, &mut ancestors);
        Ok(Some((curr, ancestors)))
    }

    async fn fetch_chain_patchset_row(&self, id: i64) -> Result<Option<ChainPatchsetRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id, thread_id, cover_letter_message_id, COALESCE(subject, ''),
                        COALESCE(author, ''), COALESCE(date, 0), mr_url, mr_number,
                        slug, version, previous_patchset_id
                 FROM patchsets
                 WHERE id = ?
                 LIMIT 1",
                libsql::params![id],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(Some(ChainPatchsetRow {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            cover_letter_message_id: row.get(2)?,
            subject: row.get(3)?,
            author: row.get(4)?,
            date: row.get(5)?,
            mr_url: row.get(6)?,
            mr_number: row.get(7)?,
            slug: row.get(8)?,
            stored_version: row.get(9)?,
            previous_patchset_id: row.get(10)?,
            effective_version: 1,
        }))
    }

    async fn load_and_decode_chain_patches(
        &self,
        curr_ps: &ChainPatchsetRow,
        ancestors: &[ChainPatchsetRow],
    ) -> Result<(HashMap<i64, Vec<DecodedPatchEntry>>, HashMap<i64, String>)> {
        let mut raw_patches = Vec::new();
        let mut raw_covers: Vec<(i64, libsql::Value)> = Vec::new();

        for ps in std::iter::once(curr_ps).chain(ancestors.iter()) {
            let mut rows = self
                .conn
                .query(
                    "SELECT p.id, p.patchset_id, p.message_id, COALESCE(p.part_index, 1),
                            p.previous_patch_id, p.diff, m.author, m.body, m.thread_id
                     FROM patches p
                     LEFT JOIN messages m ON m.message_id = p.message_id
                     WHERE p.patchset_id = ?
                     ORDER BY COALESCE(p.part_index, 1) ASC, p.id ASC
                     LIMIT 1000",
                    libsql::params![ps.id],
                )
                .await?;
            while let Some(r) = rows.next().await? {
                raw_patches.push(RawPatchEntry {
                    id: r.get(0)?,
                    patchset_id: r.get(1)?,
                    message_id: r.get(2)?,
                    part_index: r.get(3)?,
                    previous_patch_id: r.get(4)?,
                    diff_val: r.get(5)?,
                    author: r.get(6)?,
                    body_val: r.get::<libsql::Value>(7).ok(),
                    thread_id: r.get(8)?,
                });
            }

            // Coverless series record patch 1 as their cover letter; its body
            // is a patch, not a cover, so its changelog is not read twice.
            let cover_is_a_patch = |clid: &str| {
                raw_patches.iter().any(|p: &RawPatchEntry| {
                    p.patchset_id == ps.id && p.part_index >= 1 && p.message_id == clid
                })
            };
            if let Some(ref clid) = ps.cover_letter_message_id
                && !cover_is_a_patch(clid)
            {
                let mut mrows = self
                    .conn
                    .query(
                        "SELECT body FROM messages WHERE message_id = ? LIMIT 1",
                        libsql::params![clid.as_str()],
                    )
                    .await?;
                if let Some(mr) = mrows.next().await? {
                    raw_covers.push((ps.id, mr.get::<libsql::Value>(0)?));
                }
            }
        }

        tokio::task::spawn_blocking(move || {
            let mut by_ps: HashMap<i64, Vec<DecodedPatchEntry>> = HashMap::new();
            for rp in raw_patches {
                let diff = crate::compression::decompress_string_value(&rp.diff_val)?;
                let body = match rp.body_val {
                    Some(ref v) => crate::compression::decompress_string_value(v)?,
                    None => String::new(),
                };
                by_ps
                    .entry(rp.patchset_id)
                    .or_default()
                    .push(DecodedPatchEntry {
                        id: rp.id,
                        patchset_id: rp.patchset_id,
                        message_id: rp.message_id,
                        part_index: rp.part_index,
                        previous_patch_id: rp.previous_patch_id,
                        diff,
                        author: rp.author,
                        body,
                        thread_id: rp.thread_id,
                    });
            }
            let mut covers = HashMap::new();
            for (ps_id, val) in raw_covers {
                let text = crate::compression::decompress_string_value(&val)?;
                if !text.trim().is_empty() {
                    covers.insert(ps_id, text);
                }
            }
            Ok((by_ps, covers))
        })
        .await
        .context("prior_context patch decompression task failed")?
    }

    #[allow(clippy::too_many_arguments)]
    async fn attach_prior_sashiko_findings(
        &self,
        contexts: &mut HashMap<i64, PriorVersionContext>,
        v_n_patches: &[&DecodedPatchEntry],
        ancestors: &[ChainPatchsetRow],
        patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
        patches_by_id: &HashMap<i64, &DecodedPatchEntry>,
        lineage_by_vn_patch: &HashMap<i64, Vec<i64>>,
        patches_with_newer_successor: &HashSet<i64>,
        version_by_ps_id: &HashMap<i64, u32>,
        routing_targets: &[PatchRoutingTarget],
    ) -> Result<CoveredSashikoRoots> {
        let mut covered = CoveredSashikoRoots::default();
        let anc_by_id: HashMap<i64, &ChainPatchsetRow> =
            ancestors.iter().map(|a| (a.id, a)).collect();

        // 1. Direct patch lineage: load findings from the FIRST ancestor patch
        // in `v(N-1)..v1` that completed a Sashiko review (`status = 'Reviewed'`).
        for vp in v_n_patches {
            let Some(chain) = lineage_by_vn_patch.get(&vp.id) else {
                continue;
            };
            for (chain_idx, &anc_patch_id) in chain.iter().enumerate() {
                let Some(anc_patch) = patches_by_id.get(&anc_patch_id) else {
                    continue;
                };
                let Some(review_id) = self
                    .find_latest_completed_review_id(anc_patch.patchset_id, anc_patch.id)
                    .await?
                else {
                    continue;
                };
                covered.patch_ids.extend(&chain[chain_idx..]);
                let rev_ver = version_by_ps_id
                    .get(&anc_patch.patchset_id)
                    .copied()
                    .unwrap_or(1);
                let ref_url = anc_by_id
                    .get(&anc_patch.patchset_id)
                    .and_then(|ps| default_sashiko_review_url(ps, &anc_patch.message_id));
                let items = self
                    .load_review_finding_items(
                        review_id,
                        rev_ver,
                        PriorItemScope::DirectPatchLineage,
                        ref_url.as_deref(),
                        &anc_patch.message_id,
                    )
                    .await?;
                if let Some(ctx) = contexts.get_mut(&vp.id) {
                    ctx.prior_findings.extend(items);
                }
                // Stop at the latest completed review even when `items` is empty:
                // a clean `Reviewed` pass on v2 supersedes resolved v1 findings.
                break;
            }
        }

        // 2. Squashed ancestor patches: an ancestor patch with no successor in
        // any newer version was either squashed into another patch, dropped,
        // or applied upstream. Only the first case carries its findings
        // forward, so findings are forwarded only to vN patches that show
        // strong squash evidence (see `squash_evidence_targets`) and dropped
        // otherwise.
        let vn_changed_idents: HashMap<i64, HashSet<String>> = v_n_patches
            .iter()
            .map(|vp| (vp.id, changed_line_identifiers(&vp.diff)))
            .collect();
        for (anc_idx, anc) in ancestors.iter().enumerate() {
            let Some(anc_patches) = patches_by_ps.get(&anc.id) else {
                continue;
            };
            // `ancestors` is ordered newest->oldest, so the version that
            // replaced `anc` is the previous entry, or vN itself.
            let successor_parts = match anc_idx.checked_sub(1) {
                Some(i) => count_series_parts(patches_by_ps.get(&ancestors[i].id)),
                None => v_n_patches.len(),
            };
            if successor_parts >= count_series_parts(Some(anc_patches)) {
                continue;
            }
            for ap in anc_patches {
                if ap.part_index < 1 || patches_with_newer_successor.contains(&ap.id) {
                    continue;
                }
                let squash_targets = squash_evidence_targets(
                    ap,
                    patches_by_ps,
                    patches_by_id,
                    lineage_by_vn_patch,
                    routing_targets,
                    &vn_changed_idents,
                );
                if squash_targets.is_empty() {
                    continue;
                }
                let mut check_chain = vec![ap.id];
                check_chain.extend(walk_patch_ancestors(ap.previous_patch_id, patches_by_id));
                for (chain_idx, &cand_pid) in check_chain.iter().enumerate() {
                    let Some(cand_patch) = patches_by_id.get(&cand_pid) else {
                        continue;
                    };
                    let Some(review_id) = self
                        .find_latest_completed_review_id(cand_patch.patchset_id, cand_patch.id)
                        .await?
                    else {
                        continue;
                    };
                    covered.patch_ids.extend(&check_chain[chain_idx..]);
                    let rev_ver = version_by_ps_id
                        .get(&cand_patch.patchset_id)
                        .copied()
                        .unwrap_or(1);
                    let ref_url = anc_by_id
                        .get(&cand_patch.patchset_id)
                        .and_then(|ps| default_sashiko_review_url(ps, &cand_patch.message_id));
                    let items = self
                        .load_review_finding_items(
                            review_id,
                            rev_ver,
                            PriorItemScope::FileSymbolOverlap,
                            ref_url.as_deref(),
                            &cand_patch.message_id,
                        )
                        .await?;
                    for item in items {
                        let explicit_file = extract_first_finding_file(&item.locations);
                        let route_text = format!("{}\n{}", cand_patch.diff, item.problem);
                        for (target_pid, _) in route_unattached_text_to_patches(
                            &squash_targets,
                            explicit_file.as_deref(),
                            &route_text,
                        ) {
                            if let Some(ctx) = contexts.get_mut(&target_pid) {
                                let mut cloned = item.clone();
                                cloned.scope = PriorItemScope::FileSymbolOverlap;
                                ctx.prior_findings.push(cloned);
                            }
                        }
                    }
                    break;
                }
            }
        }

        let mut newer_patchset_covered = false;
        for anc in ancestors {
            let all_patches_covered = patches_by_ps.get(&anc.id).is_some_and(|plist| {
                let mut saw_patch = false;
                plist.iter().filter(|p| p.part_index >= 1).all(|p| {
                    saw_patch = true;
                    covered.patch_ids.contains(&p.id)
                }) && saw_patch
            });
            if newer_patchset_covered || all_patches_covered {
                covered.patchset_ids.insert(anc.id);
                newer_patchset_covered = true;
            }
        }
        Ok(covered)
    }

    async fn find_latest_completed_review_id(
        &self,
        patchset_id: i64,
        patch_id: i64,
    ) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM reviews
                 WHERE patchset_id = ? AND patch_id = ? AND status = 'Reviewed'
                 ORDER BY id DESC
                 LIMIT 1",
                libsql::params![patchset_id, patch_id],
            )
            .await?;
        match rows.next().await? {
            Some(r) => Ok(Some(r.get(0)?)),
            None => Ok(None),
        }
    }

    async fn load_review_finding_items(
        &self,
        review_id: i64,
        reviewed_in_version: u32,
        scope: PriorItemScope,
        default_reference_url: Option<&str>,
        source_message_id: &str,
    ) -> Result<Vec<PriorFindingItem>> {
        let source_message_id = Some(normalize_msgid(source_message_id)).filter(|m| !m.is_empty());
        let mut rows = self
            .conn
            .query(
                "SELECT severity, severity_explanation, COALESCE(problem, ''),
                        locations
                 FROM findings
                 WHERE review_id = ?
                 ORDER BY id ASC
                 LIMIT 1000",
                libsql::params![review_id],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            let sev_int: i32 = r.get(0)?;
            let severity_explanation: Option<String> = r.get(1)?;
            let problem: String = r.get(2)?;
            if problem.trim().is_empty() {
                continue;
            }
            let locations_raw: Option<String> = r.get(3)?;
            let locations = locations_raw.and_then(|s| serde_json::from_str(&s).ok());
            // A prior finding is always Sashiko's own claim at the version it
            // reviewed. Credit for humans who raised the issue earlier comes
            // only from citing their original messages, never from the
            // attribution stored with the earlier review.
            let attribution = PriorAttribution {
                originally_raised_by: "Sashiko".to_string(),
                originally_raised_in_version: reviewed_in_version,
                origin_kind: OriginKind::SashikoReview,
                reference_url: default_reference_url.map(str::to_string),
                raised_by_identities: vec![SASHIKO_IDENTITY.to_string()],
                evidence: Vec::new(),
                found_by: None,
                attribution_line: None,
            };
            out.push(PriorFindingItem {
                id: String::new(),
                reviewed_in_version,
                scope,
                severity: severity_label_from_int(sev_int).to_string(),
                severity_explanation,
                problem,
                locations,
                attribution,
                source_message_id: source_message_id.clone(),
            });
        }
        Ok(out)
    }

    async fn collect_email_discussion_threads(
        &self,
        curr_ps: &ChainPatchsetRow,
        ancestors: &[ChainPatchsetRow],
        patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
        covered_sashiko_roots: &CoveredSashikoRoots,
    ) -> Result<Vec<PendingThread>> {
        let (root_map, thread_ids) = collect_ancestor_email_roots(ancestors, patches_by_ps);
        if root_map.is_empty() {
            return Ok(Vec::new());
        }

        let metas = self
            .fetch_candidate_email_metadata(&thread_ids, &root_map)
            .await?;
        if metas.is_empty() {
            return Ok(Vec::new());
        }

        let barrier_norms = self
            .find_foreign_patchset_barriers(curr_ps, &metas, &root_map)
            .await?;

        let subtrees = cap_email_subtrees(group_email_subtrees(
            &metas,
            &root_map,
            &barrier_norms,
            covered_sashiko_roots,
        ));
        if subtrees.is_empty() {
            return Ok(Vec::new());
        }
        let body_ids: Vec<i64> = subtrees
            .iter()
            .flat_map(|st| st.members.iter().map(|m| m.id))
            .collect();
        let bodies = self.load_email_bodies(&body_ids).await?;
        Ok(render_email_threads(&subtrees, &bodies))
    }

    /// Loads header rows for every message in the ancestor threads plus
    /// replies reachable from the ancestor roots through `in_reply_to`.
    ///
    /// The reply walk starts from the ancestor cover letters and patches and
    /// from every message loaded by `thread_id` (a reply can land in another
    /// thread when its client breaks `References:`), then follows what it
    /// discovers to any depth. Each hop is one batched query per
    /// `SQL_IN_BATCH` candidates, and the total is bounded by
    /// `MAX_CANDIDATE_EMAIL_ROWS` (reaching it is logged).
    async fn fetch_candidate_email_metadata(
        &self,
        thread_ids: &BTreeSet<i64>,
        root_map: &HashMap<String, AncestorRootKind>,
    ) -> Result<Vec<EmailMessageMeta>> {
        let mut seen_db_ids = HashSet::new();
        let mut seen_norms: HashSet<String> = root_map.keys().cloned().collect();
        let mut out: Vec<EmailMessageMeta> = Vec::new();

        let thread_sql = format!(
            "SELECT {EMAIL_META_COLUMNS} FROM messages
             WHERE thread_id = ?
             ORDER BY date ASC, id ASC
             LIMIT ?"
        );
        for &tid in thread_ids {
            let remaining = MAX_CANDIDATE_EMAIL_ROWS.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            let limit = remaining.min(MAX_ROWS_PER_THREAD) as i64;
            let mut rows = self
                .conn
                .query(&thread_sql, libsql::params![tid, limit])
                .await?;
            while let Some(r) = rows.next().await? {
                let meta = EmailMessageMeta::from_row(&r)?;
                if seen_db_ids.insert(meta.id) {
                    seen_norms.insert(meta.norm_id.clone());
                    out.push(meta);
                }
            }
        }

        let mut frontier: Vec<String> = root_map
            .keys()
            .cloned()
            .chain(out.iter().map(|m| m.norm_id.clone()))
            .filter(|n| !n.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // Follow replies to any depth; `seen_norms` stops cycles.
        while !frontier.is_empty() && out.len() < MAX_CANDIDATE_EMAIL_ROWS {
            let candidates: BTreeSet<String> = frontier
                .iter()
                .flat_map(|norm| msgid_lookup_candidates(norm))
                .collect();
            let candidates: Vec<String> = candidates.into_iter().collect();
            let mut next = Vec::new();
            for chunk in candidates.chunks(SQL_IN_BATCH) {
                let found = self
                    .query_replies_to(chunk, MAX_CANDIDATE_EMAIL_ROWS.saturating_sub(out.len()))
                    .await?;
                for meta in found {
                    if !seen_db_ids.insert(meta.id) {
                        continue;
                    }
                    if !meta.norm_id.is_empty() && seen_norms.insert(meta.norm_id.clone()) {
                        next.push(meta.norm_id.clone());
                    }
                    out.push(meta);
                }
            }
            frontier = next;
        }
        if out.len() >= MAX_CANDIDATE_EMAIL_ROWS {
            tracing::warn!(
                "prior_context: candidate email rows capped at {} for threads {:?}",
                MAX_CANDIDATE_EMAIL_ROWS,
                thread_ids
            );
        }
        Ok(out)
    }

    async fn query_replies_to(
        &self,
        parent_candidates: &[String],
        limit: usize,
    ) -> Result<Vec<EmailMessageMeta>> {
        if parent_candidates.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {EMAIL_META_COLUMNS} FROM messages
             WHERE in_reply_to IN (SELECT value FROM json_each(?))
             ORDER BY date ASC, id ASC
             LIMIT ?"
        );
        let mut rows = self
            .conn
            .query(
                &sql,
                libsql::params![serde_json::to_string(parent_candidates)?, limit as i64],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(EmailMessageMeta::from_row(&r)?);
        }
        Ok(out)
    }

    /// Marks candidate messages that are themselves a cover letter or patch of
    /// some patchset outside the ancestor chain, so subtree isolation never
    /// walks into another series sharing a `thread_id`.
    async fn find_foreign_patchset_barriers(
        &self,
        curr_ps: &ChainPatchsetRow,
        metas: &[EmailMessageMeta],
        root_map: &HashMap<String, AncestorRootKind>,
    ) -> Result<HashSet<String>> {
        let mut barriers = HashSet::new();
        if let Some(ref clid) = curr_ps.cover_letter_message_id {
            barriers.insert(normalize_msgid(clid));
        }

        let mut norm_by_candidate: HashMap<String, String> = HashMap::new();
        for m in metas {
            if m.norm_id.is_empty()
                || root_map.contains_key(&m.norm_id)
                || barriers.contains(&m.norm_id)
            {
                continue;
            }
            for cand in msgid_lookup_candidates(&m.norm_id) {
                norm_by_candidate.insert(cand, m.norm_id.clone());
            }
        }
        let mut candidates: Vec<String> = norm_by_candidate.keys().cloned().collect();
        candidates.sort();

        for chunk in candidates.chunks(SQL_IN_BATCH) {
            let json = serde_json::to_string(chunk)?;
            for sql in [
                "SELECT cover_letter_message_id FROM patchsets
                 WHERE cover_letter_message_id IN (SELECT value FROM json_each(?))",
                "SELECT message_id FROM patches
                 WHERE message_id IN (SELECT value FROM json_each(?))",
            ] {
                let mut rows = self.conn.query(sql, libsql::params![json.as_str()]).await?;
                while let Some(r) = rows.next().await? {
                    let hit: Option<String> = r.get(0)?;
                    if let Some(norm) = hit.and_then(|h| norm_by_candidate.get(&h)) {
                        barriers.insert(norm.clone());
                    }
                }
            }
        }
        Ok(barriers)
    }

    /// Fetches and decompresses bodies for `ids` only, in batches.
    async fn load_email_bodies(&self, ids: &[i64]) -> Result<HashMap<i64, String>> {
        let mut raw: Vec<(i64, libsql::Value)> = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(SQL_IN_BATCH) {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, body FROM messages
                     WHERE id IN (SELECT value FROM json_each(?))",
                    libsql::params![serde_json::to_string(chunk)?],
                )
                .await?;
            while let Some(r) = rows.next().await? {
                raw.push((r.get(0)?, r.get(1)?));
            }
        }
        tokio::task::spawn_blocking(move || -> Result<HashMap<i64, String>> {
            let mut out = HashMap::with_capacity(raw.len());
            for (id, val) in raw {
                out.insert(id, crate::compression::decompress_string_value(&val)?);
            }
            Ok(out)
        })
        .await
        .context("prior_context email decompression task failed")?
    }

    async fn collect_forge_discussion_threads(
        &self,
        curr_ps: &ChainPatchsetRow,
        ancestors: &[ChainPatchsetRow],
        patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
        opts: &PriorContextOptions<'_>,
    ) -> Result<Vec<PendingThread>> {
        let Some(pr_number) = curr_ps
            .mr_number
            .or_else(|| ancestors.iter().find_map(|a| a.mr_number))
        else {
            return Ok(Vec::new());
        };
        let Some(mr_url) = curr_ps
            .mr_url
            .as_deref()
            .or_else(|| ancestors.iter().find_map(|a| a.mr_url.as_deref()))
        else {
            return Ok(Vec::new());
        };
        let Some(repo) = crate::forge::extract_owner_repo_from_mr_url(mr_url) else {
            return Ok(Vec::new());
        };
        let Some(provider) = resolve_forge_provider(mr_url, opts.forge_provider) else {
            return Ok(Vec::new());
        };

        let mut rows = self
            .conn
            .query(
                "SELECT id, comment_source, remote_comment_id, in_reply_to_id,
                        author, author_association, commit_id, file_path, line,
                        body, html_url, created_at
                 FROM forge_comments
                 WHERE provider = ? AND repo = ? AND pr_number = ?
                 ORDER BY created_at ASC, id ASC
                 LIMIT 20000",
                libsql::params![provider.as_str(), repo.as_str(), pr_number],
            )
            .await?;

        let mut raw_comments = Vec::new();
        while let Some(r) = rows.next().await? {
            raw_comments.push(RawForgeCommentRow {
                id: r.get(0)?,
                comment_source: r.get(1)?,
                remote_comment_id: r.get(2)?,
                in_reply_to_id: r.get(3)?,
                author: r.get(4)?,
                author_association: r.get(5)?,
                commit_id: r.get(6)?,
                file_path: r.get(7)?,
                line: r.get(8)?,
                body_val: r.get(9)?,
                html_url: r.get(10)?,
                created_at: r.get(11)?,
            });
        }
        if raw_comments.is_empty() {
            return Ok(Vec::new());
        }

        let decoded =
            tokio::task::spawn_blocking(move || -> Result<Vec<DecodedForgeCommentRow>> {
                let mut out = Vec::with_capacity(raw_comments.len());
                for rc in raw_comments {
                    let body = crate::compression::decompress_string_value(&rc.body_val)?;
                    out.push(DecodedForgeCommentRow {
                        id: rc.id,
                        comment_source: rc.comment_source,
                        remote_comment_id: rc.remote_comment_id,
                        in_reply_to_id: rc.in_reply_to_id,
                        author: rc.author,
                        author_association: rc.author_association,
                        commit_id: rc.commit_id,
                        file_path: rc.file_path,
                        line: rc.line,
                        body,
                        html_url: rc.html_url,
                        created_at: rc.created_at,
                    });
                }
                Ok(out)
            })
            .await
            .context("prior_context forge comment decompression task failed")??;

        Ok(build_forge_pending_threads(
            &decoded,
            curr_ps,
            ancestors,
            patches_by_ps,
            opts.forge_bot_login,
        ))
    }
}

fn assign_effective_versions(curr: &mut ChainPatchsetRow, ancestors: &mut [ChainPatchsetRow]) {
    // Assign oldest->newest so monotonically increasing versions hold even when
    // `patchsets.version` was left NULL on an intermediate row.
    let mut prev_ver = 0u32;
    for anc in ancestors.iter_mut().rev() {
        let min_next = prev_ver.saturating_add(1);
        let candidate = anc
            .stored_version
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0)
            .or_else(|| crate::patch::parse_subject_version(&anc.subject))
            .unwrap_or(min_next);
        anc.effective_version = candidate.max(min_next);
        prev_ver = anc.effective_version;
    }
    let min_curr = prev_ver.saturating_add(1);
    let curr_cand = curr
        .stored_version
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0)
        .or_else(|| crate::patch::parse_subject_version(&curr.subject))
        .unwrap_or(min_curr);
    curr.effective_version = curr_cand.max(min_curr);
}

fn walk_patch_ancestors(
    start_prev_id: Option<i64>,
    patches_by_id: &HashMap<i64, &DecodedPatchEntry>,
) -> Vec<i64> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = start_prev_id;
    while let Some(pid) = cur {
        if chain.len() >= MAX_ANCESTOR_VERSIONS || !seen.insert(pid) {
            break;
        }
        let Some(p) = patches_by_id.get(&pid) else {
            break;
        };
        chain.push(pid);
        cur = p.previous_patch_id;
    }
    chain
}

#[allow(clippy::too_many_arguments)]
fn populate_interdiff_and_changelogs(
    ctx: &mut PriorVersionContext,
    vp: &DecodedPatchEntry,
    curr_ps: &ChainPatchsetRow,
    ancestors: &[ChainPatchsetRow],
    lineage_by_vn_patch: &HashMap<i64, Vec<i64>>,
    patches_by_id: &HashMap<i64, &DecodedPatchEntry>,
    version_by_ps_id: &HashMap<i64, u32>,
    cover_bodies: &HashMap<i64, String>,
) {
    if let Some(chain) = lineage_by_vn_patch.get(&vp.id)
        && let Some(&immediate_anc_id) = chain.first()
        && let Some(anc_patch) = patches_by_id.get(&immediate_anc_id)
    {
        let base_ver = version_by_ps_id
            .get(&anc_patch.patchset_id)
            .copied()
            .unwrap_or(1);
        ctx.inter_version_diff_base_version = Some(base_ver);
        ctx.inter_version_diff = compute_inter_version_diff(&anc_patch.diff, &vp.diff);
    }

    let mut blocks: Vec<PriorChangelogBlock> = extract_version_changelog_blocks(
        curr_ps.effective_version,
        cover_bodies.get(&curr_ps.id).map(String::as_str),
        &vp.body,
    );
    if let Some(chain) = lineage_by_vn_patch
        .get(&vp.id)
        .filter(|chain| !chain.is_empty())
    {
        for &anc_pid in chain {
            if let Some(anc_patch) = patches_by_id.get(&anc_pid) {
                let ver = version_by_ps_id
                    .get(&anc_patch.patchset_id)
                    .copied()
                    .unwrap_or(1);
                if ver >= 2 {
                    blocks.extend(extract_version_changelog_blocks(
                        ver,
                        cover_bodies.get(&anc_patch.patchset_id).map(String::as_str),
                        &anc_patch.body,
                    ));
                }
            }
        }
    } else {
        for anc in ancestors.iter().filter(|a| a.effective_version >= 2) {
            if let Some(cover) = cover_bodies.get(&anc.id) {
                blocks.extend(extract_version_changelog_blocks(
                    anc.effective_version,
                    Some(cover.as_str()),
                    "",
                ));
            }
        }
    }
    ctx.changelogs = blocks;
}

fn count_series_parts(patches: Option<&Vec<DecodedPatchEntry>>) -> usize {
    patches.map_or(0, |list| list.iter().filter(|p| p.part_index >= 1).count())
}

/// Specific identifiers on added or removed lines only, ignoring context
/// lines and hunk headers that unrelated patches to the same function share.
fn changed_line_identifiers(diff: &str) -> HashSet<String> {
    let mut changed = String::new();
    for line in diff.lines() {
        let is_header = line.starts_with("+++") || line.starts_with("---");
        if !is_header && (line.starts_with('+') || line.starts_with('-')) {
            changed.push_str(line);
            changed.push('\n');
        }
    }
    crate::notes::select::diff_identifiers(&changed)
}

/// vN patches that plausibly absorbed the orphaned ancestor patch
/// `orphan_patch`: each must touch a file the orphan touched and change a line
/// naming a non-ubiquitous identifier from the orphan's changed lines that was
/// not already present in the candidate vN patch's own ancestor in the same
/// patchset as `orphan_patch`. File overlap or series-wide boilerplate
/// identifiers alone are not evidence, because a dropped or upstream-applied
/// patch commonly shares files and test/driver scaffolding with the rest of its
/// series.
fn squash_evidence_targets(
    orphan_patch: &DecodedPatchEntry,
    patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
    patches_by_id: &HashMap<i64, &DecodedPatchEntry>,
    lineage_by_vn_patch: &HashMap<i64, Vec<i64>>,
    routing_targets: &[PatchRoutingTarget],
    vn_changed_idents: &HashMap<i64, HashSet<String>>,
) -> Vec<PatchRoutingTarget> {
    let orphan_files: HashSet<String> = extract_patch_touched_files(&orphan_patch.diff)
        .into_iter()
        .collect();
    let mut orphan_idents = changed_line_identifiers(&orphan_patch.diff);
    if orphan_files.is_empty() || orphan_idents.is_empty() {
        return Vec::new();
    }
    if let Some(siblings) = patches_by_ps.get(&orphan_patch.patchset_id) {
        let mut sibling_counts: HashMap<String, usize> = HashMap::new();
        for sib in siblings
            .iter()
            .filter(|p| p.part_index >= 1 && p.id != orphan_patch.id)
        {
            for id in changed_line_identifiers(&sib.diff) {
                *sibling_counts.entry(id).or_default() += 1;
            }
        }
        orphan_idents.retain(|id| sibling_counts.get(id).copied().unwrap_or(0) < 2);
        if orphan_idents.is_empty() {
            return Vec::new();
        }
    }
    routing_targets
        .iter()
        .filter(|t| t.touched_files.iter().any(|f| orphan_files.contains(f)))
        .filter(|t| {
            let Some(vn_ids) = vn_changed_idents.get(&t.patch_id) else {
                return false;
            };
            let same_ps_anc_idents = lineage_by_vn_patch
                .get(&t.patch_id)
                .and_then(|chain| {
                    chain
                        .iter()
                        .filter_map(|anc_id| patches_by_id.get(anc_id).copied())
                        .find(|p| p.patchset_id == orphan_patch.patchset_id)
                })
                .map(|anc| changed_line_identifiers(&anc.diff));
            orphan_idents.iter().any(|id| {
                vn_ids.contains(id)
                    && same_ps_anc_idents
                        .as_ref()
                        .is_none_or(|base| !base.contains(id))
            })
        })
        .cloned()
        .collect()
}

fn severity_label_from_int(sev: i32) -> &'static str {
    if sev == Severity::Critical as i32 {
        "Critical"
    } else if sev == Severity::High as i32 {
        "High"
    } else if sev == Severity::Medium as i32 {
        "Medium"
    } else {
        "Low"
    }
}

fn default_sashiko_review_url(ps: &ChainPatchsetRow, patch_msg_id: &str) -> Option<String> {
    if let Some(ref mr_url) = ps.mr_url
        && let Some(clean) = safe_external_reference_url(mr_url)
    {
        return Some(clean);
    }
    if let Some(lore) = safe_lore_reference_url(patch_msg_id) {
        return Some(lore);
    }
    ps.slug
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|slug| format!("https://sashiko.sashiko.dev/#/patchset/{slug}"))
}

fn extract_first_finding_file(locations: &Option<serde_json::Value>) -> Option<String> {
    locations
        .as_ref()?
        .as_array()?
        .iter()
        .find_map(|loc| loc.get("file")?.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Spellings under which a normalized Message-ID may be stored: bare or
/// bracketed, with or without Sashiko's synthetic `@sashiko.local` suffix.
fn msgid_lookup_candidates(norm: &str) -> [String; 4] {
    let local = format!("{norm}@sashiko.local");
    [
        norm.to_string(),
        format!("<{norm}>"),
        format!("<{local}>"),
        local,
    ]
}

/// Maps every ancestor cover letter and patch Message-ID (normalized) to its
/// root kind, and collects the thread IDs those messages live in.
fn collect_ancestor_email_roots(
    ancestors: &[ChainPatchsetRow],
    patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
) -> (HashMap<String, AncestorRootKind>, BTreeSet<i64>) {
    let mut root_map: HashMap<String, AncestorRootKind> = HashMap::new();
    let mut thread_ids: BTreeSet<i64> = BTreeSet::new();
    for anc in ancestors {
        if let Some(tid) = anc.thread_id {
            thread_ids.insert(tid);
        }
        if let Some(ref clid) = anc.cover_letter_message_id {
            let norm = normalize_msgid(clid);
            if !norm.is_empty() {
                root_map.insert(
                    norm,
                    AncestorRootKind::Cover {
                        patchset_id: anc.id,
                        version: anc.effective_version,
                    },
                );
            }
        }
        for p in patches_by_ps.get(&anc.id).into_iter().flatten() {
            if let Some(tid) = p.thread_id {
                thread_ids.insert(tid);
            }
            let norm = normalize_msgid(&p.message_id);
            if !norm.is_empty() {
                // Patch roots override cover-letter roots when a 1/1
                // series uses its first patch as `cover_letter_message_id`.
                root_map.insert(
                    norm,
                    AncestorRootKind::Patch {
                        version: anc.effective_version,
                        patch_id: p.id,
                    },
                );
            }
        }
    }
    (root_map, thread_ids)
}

/// Splits candidate messages into reply subtrees hanging directly off an
/// ancestor root, never walking into other roots or foreign patchsets.
fn group_email_subtrees<'a>(
    messages: &'a [EmailMessageMeta],
    root_map: &HashMap<String, AncestorRootKind>,
    barrier_norms: &HashSet<String>,
    covered_sashiko_roots: &CoveredSashikoRoots,
) -> Vec<EmailSubtree<'a>> {
    let mut msg_by_norm: HashMap<&str, &EmailMessageMeta> = HashMap::new();
    for m in messages {
        if !m.norm_id.is_empty() {
            msg_by_norm.entry(m.norm_id.as_str()).or_insert(m);
        }
    }

    let mut children_by_parent: HashMap<String, Vec<&EmailMessageMeta>> = HashMap::new();
    for m in messages {
        // Never walk into messages that are themselves ancestor cover/patch roots
        // or foreign patchset roots (pruning shared `thread_id` subtrees cleanly).
        if m.norm_id.is_empty()
            || root_map.contains_key(&m.norm_id)
            || barrier_norms.contains(&m.norm_id)
        {
            continue;
        }
        if let Some(parent_norm) =
            resolve_email_parent_norm(m, root_map, &msg_by_norm, barrier_norms)
        {
            children_by_parent.entry(parent_norm).or_default().push(m);
        }
    }
    for kids in children_by_parent.values_mut() {
        kids.sort_by_key(|m| (m.date, m.id));
    }

    let mut sorted_roots: Vec<(&String, &AncestorRootKind)> = root_map.iter().collect();
    sorted_roots.sort_by_key(|(norm, kind)| match kind {
        AncestorRootKind::Patch { version, patch_id } => (*version, 0u8, *patch_id, norm.as_str()),
        AncestorRootKind::Cover {
            patchset_id,
            version,
        } => (*version, 1u8, *patchset_id, norm.as_str()),
    });

    let mut out = Vec::new();
    let mut visited_reply_norms = HashSet::new();
    for (root_norm, root_kind) in sorted_roots {
        let Some(top_replies) = children_by_parent.get(root_norm.as_str()) else {
            continue;
        };
        let root_covered = covered_sashiko_roots.contains(*root_kind);
        for &top_reply in top_replies {
            let members =
                collect_reply_subtree(top_reply, &children_by_parent, &mut visited_reply_norms);
            // Drop standalone `<sashiko-outbox-*>` emails and standalone
            // Sashiko Lore emails when that ancestor root is already covered
            // (or superseded) by a completed DB review in its lineage, unless
            // a human/bot replied in the subtree. When an ancestor patch root
            // has no completed DB review in its lineage (e.g., its DB review
            // was `Cancelled` or missed locally), keep Sashiko emails so their
            // substantive findings still reach `render_email_threads` (where
            // clean "found no issues" notices are filtered out).
            if members.is_empty()
                || (root_covered
                    && members.iter().all(|m| {
                        is_sashiko_outbox_msgid(&m.message_id)
                            || is_sashiko_review_message(&m.message_id, &m.author)
                    }))
            {
                continue;
            }
            out.push(EmailSubtree {
                root_kind: *root_kind,
                top_subject: top_reply.subject.as_str(),
                members,
            });
        }
    }
    out
}

fn email_root_version(kind: AncestorRootKind) -> u32 {
    match kind {
        AncestorRootKind::Cover { version, .. } | AncestorRootKind::Patch { version, .. } => {
            version
        }
    }
}

/// Keeps whole subtrees, newest ancestor version first, until their combined
/// size reaches `MAX_EMAIL_BODIES`, preserving the original order.
fn cap_email_subtrees(subtrees: Vec<EmailSubtree<'_>>) -> Vec<EmailSubtree<'_>> {
    let total: usize = subtrees.iter().map(|st| st.members.len()).sum();
    if total <= MAX_EMAIL_BODIES {
        return subtrees;
    }
    let mut by_priority: Vec<usize> = (0..subtrees.len()).collect();
    by_priority.sort_by_key(|&i| std::cmp::Reverse(email_root_version(subtrees[i].root_kind)));
    let mut keep = HashSet::new();
    let mut budget = MAX_EMAIL_BODIES;
    for i in by_priority {
        let size = subtrees[i].members.len();
        if size <= budget {
            budget -= size;
            keep.insert(i);
        }
    }
    tracing::warn!(
        "prior_context: kept {} of {} email subtrees ({} messages over the {} body cap)",
        keep.len(),
        subtrees.len(),
        total,
        MAX_EMAIL_BODIES
    );
    subtrees
        .into_iter()
        .enumerate()
        .filter_map(|(i, st)| keep.contains(&i).then_some(st))
        .collect()
}

fn render_email_threads(
    subtrees: &[EmailSubtree<'_>],
    bodies: &HashMap<i64, String>,
) -> Vec<PendingThread> {
    let mut out = Vec::new();
    for st in subtrees {
        let unscored: Vec<UnscoredMessage> = st
            .members
            .iter()
            .filter_map(|m| {
                let trimmed = trim_email_quotes(bodies.get(&m.id)?);
                if trimmed.is_empty() {
                    return None;
                }
                Some(UnscoredMessage {
                    message_id: m.message_id.clone(),
                    author: m.author.clone(),
                    author_association: None,
                    timestamp: m.date,
                    file_path: None,
                    line: None,
                    reference_url: safe_lore_reference_url(&m.message_id),
                    trimmed_body: trimmed,
                })
            })
            .collect();
        if unscored.is_empty()
            || unscored.iter().all(|m| {
                is_non_actionable_reply_own_text(&message_own_text(&m.trimmed_body), &m.author)
            })
        {
            continue;
        }
        let target_ancestor_patch_id = match st.root_kind {
            AncestorRootKind::Cover { .. } => None,
            AncestorRootKind::Patch { patch_id, .. } => Some(patch_id),
        };
        out.push(PendingThread {
            version: email_root_version(st.root_kind),
            subject: Some(st.top_subject.to_string()).filter(|s| !s.is_empty()),
            target_ancestor_patch_id,
            explicit_file_path: None,
            messages: unscored,
        });
    }
    out
}

fn resolve_email_parent_norm(
    msg: &EmailMessageMeta,
    root_map: &HashMap<String, AncestorRootKind>,
    msg_by_norm: &HashMap<&str, &EmailMessageMeta>,
    barrier_norms: &HashSet<String>,
) -> Option<String> {
    let is_known_node = |norm: &str| {
        root_map.contains_key(norm)
            || msg_by_norm.contains_key(norm)
            || barrier_norms.contains(norm)
    };
    if let Some(ref irt) = msg.in_reply_to {
        let norm = normalize_msgid(irt);
        if !norm.is_empty() && norm != msg.norm_id && is_known_node(&norm) {
            return Some(norm);
        }
    }
    if let Some(ref refs) = msg.references_hdr {
        for tok in refs.split_whitespace().rev() {
            let norm = normalize_msgid(tok);
            if !norm.is_empty() && norm != msg.norm_id && is_known_node(&norm) {
                return Some(norm);
            }
        }
    }
    None
}

fn collect_reply_subtree<'a>(
    root_reply: &'a EmailMessageMeta,
    children_by_parent: &HashMap<String, Vec<&'a EmailMessageMeta>>,
    visited: &mut HashSet<String>,
) -> Vec<&'a EmailMessageMeta> {
    let mut subtree = Vec::new();
    let mut queue = VecDeque::new();
    if visited.insert(root_reply.norm_id.clone()) {
        queue.push_back(root_reply);
    }
    while let Some(cur) = queue.pop_front() {
        subtree.push(cur);
        if let Some(kids) = children_by_parent.get(&cur.norm_id) {
            for &k in kids {
                if visited.insert(k.norm_id.clone()) {
                    queue.push_back(k);
                }
            }
        }
    }
    subtree.sort_by_key(|m| (m.date, m.id));
    subtree
}

/// Forge provider for `forge_comments` lookups: the configured one, else the
/// provider whose public host serves `mr_url`. Unknown hosts yield `None`
/// rather than a guess.
fn resolve_forge_provider(mr_url: &str, configured: Option<&str>) -> Option<String> {
    if let Some(p) = configured.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(p.to_ascii_lowercase());
    }
    let after_scheme = mr_url.split_once("://").map_or(mr_url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let host = authority
        .rsplit('@')
        .next()?
        .split(':')
        .next()?
        .to_ascii_lowercase();
    match host.as_str() {
        "github.com" | "www.github.com" => Some("github".to_string()),
        "gitlab.com" | "www.gitlab.com" => Some("gitlab".to_string()),
        _ => None,
    }
}

/// Picks the ancestor patch a forge comment anchored to commit patch `pid`
/// is about. GitHub anchors PR-level reviews and comments made in the
/// all-commits diff view to the PR head, i.e. the last commit, so the commit
/// alone only identifies the patch when the ancestor version has a single
/// patch. Otherwise a file-anchored comment goes to the latest patch of that
/// version touching the file, and anything else (no file, or no patch touching
/// it) returns `None` so file and text routing decide.
fn forge_anchor_patch(
    pid: i64,
    file_path: Option<&str>,
    ancestors: &[ChainPatchsetRow],
    patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
) -> Option<i64> {
    let plist = ancestors
        .iter()
        .filter_map(|anc| patches_by_ps.get(&anc.id))
        .find(|plist| plist.iter().any(|p| p.id == pid))?;
    if plist.len() == 1 {
        return Some(pid);
    }
    let file = file_path.map(str::trim).filter(|f| !f.is_empty())?;
    plist
        .iter()
        .rev()
        .find(|p| {
            extract_patch_touched_files(&p.diff)
                .iter()
                .any(|t| t == file)
        })
        .map(|p| p.id)
}

/// Turns review replies whose top-level comment is not a thread root (it
/// was deleted, fell outside the fetched pages, or is a skipped Sashiko
/// comment) into their own thread: the earliest reply becomes the root and
/// the rest stay attached to it, so their feedback is not silently dropped.
fn promote_orphan_review_replies<'a>(
    roots: &mut Vec<&'a DecodedForgeCommentRow>,
    replies_by_root: &mut BTreeMap<i64, Vec<&'a DecodedForgeCommentRow>>,
) {
    let root_ids: HashSet<i64> = roots
        .iter()
        .filter(|r| r.comment_source == "review_comment")
        .map(|r| r.remote_comment_id)
        .collect();
    let orphan_parents: Vec<i64> = replies_by_root
        .keys()
        .copied()
        .filter(|parent| !root_ids.contains(parent))
        .collect();
    for parent in orphan_parents {
        let Some(mut group) = replies_by_root.remove(&parent) else {
            continue;
        };
        group.sort_by_key(|r| (r.created_at, r.id));
        let mut rest = group.into_iter();
        let Some(new_root) = rest.next() else {
            continue;
        };
        roots.push(new_root);
        let siblings: Vec<_> = rest.collect();
        if !siblings.is_empty() {
            replies_by_root
                .entry(new_root.remote_comment_id)
                .or_default()
                .extend(siblings);
        }
    }
}

fn build_forge_pending_threads(
    comments: &[DecodedForgeCommentRow],
    curr_ps: &ChainPatchsetRow,
    ancestors: &[ChainPatchsetRow],
    patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
    sashiko_login: Option<&str>,
) -> Vec<PendingThread> {
    let mut anc_commit_map: HashMap<String, (u32, i64)> = HashMap::new();
    for anc in ancestors {
        if let Some(plist) = patches_by_ps.get(&anc.id) {
            for p in plist {
                let sha = normalize_msgid(&p.message_id).to_ascii_lowercase();
                if !sha.is_empty() {
                    anc_commit_map
                        .entry(sha)
                        .or_insert((anc.effective_version, p.id));
                }
            }
        }
    }
    let curr_only_shas: HashSet<String> = patches_by_ps
        .get(&curr_ps.id)
        .map(|plist| {
            plist
                .iter()
                .map(|p| normalize_msgid(&p.message_id).to_ascii_lowercase())
                .filter(|sha| !anc_commit_map.contains_key(sha))
                .collect()
        })
        .unwrap_or_default();
    // Anything posted once vN existed may already be about vN, so it is not
    // prior-version feedback regardless of which commit it is anchored to.
    let posted_before_vn = |created_at: i64| curr_ps.date <= 0 || created_at < curr_ps.date;

    let mut replies_by_root: BTreeMap<i64, Vec<&DecodedForgeCommentRow>> = BTreeMap::new();
    let mut roots: Vec<&DecodedForgeCommentRow> = Vec::new();
    for c in comments {
        if crate::forge::is_sashiko_pr_review_comment(&c.author, &c.body, sashiko_login) {
            continue;
        }
        if c.comment_source == "review_comment"
            && let Some(parent_id) = c.in_reply_to_id
        {
            replies_by_root.entry(parent_id).or_default().push(c);
        } else {
            roots.push(c);
        }
    }
    promote_orphan_review_replies(&mut roots, &mut replies_by_root);

    let mut out = Vec::new();
    for root in roots {
        let commit_sha = root
            .commit_id
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase());
        if !posted_before_vn(root.created_at)
            || commit_sha
                .as_ref()
                .is_some_and(|sha| curr_only_shas.contains(sha))
        {
            continue;
        }

        let (version, target_ancestor_patch_id) = match commit_sha
            .as_deref()
            .and_then(|sha| anc_commit_map.get(sha).copied())
        {
            Some((ver, pid)) => (
                ver,
                forge_anchor_patch(pid, root.file_path.as_deref(), ancestors, patches_by_ps),
            ),
            None => (
                infer_ancestor_version_by_time(root.created_at, ancestors),
                None,
            ),
        };

        let mut thread_rows = vec![root];
        if root.comment_source == "review_comment"
            && let Some(replies) = replies_by_root.remove(&root.remote_comment_id)
        {
            thread_rows.extend(replies);
        }
        thread_rows.retain(|r| posted_before_vn(r.created_at));
        thread_rows.sort_by_key(|r| (r.created_at, r.id));

        let messages: Vec<UnscoredMessage> = thread_rows
            .into_iter()
            .filter_map(|r| {
                let trimmed = trim_email_quotes(&r.body);
                if trimmed.is_empty() {
                    return None;
                }
                Some(UnscoredMessage {
                    message_id: format!("{}:{}", r.comment_source, r.remote_comment_id),
                    author: r.author.clone(),
                    author_association: r.author_association.clone(),
                    timestamp: r.created_at,
                    file_path: r.file_path.clone(),
                    line: r.line,
                    reference_url: r.html_url.as_deref().and_then(safe_external_reference_url),
                    trimmed_body: trimmed,
                })
            })
            .collect();
        if messages.is_empty() {
            continue;
        }

        out.push(PendingThread {
            version,
            subject: None,
            target_ancestor_patch_id,
            explicit_file_path: root.file_path.clone(),
            messages,
        });
    }
    out
}

fn infer_ancestor_version_by_time(created_at: i64, ancestors: &[ChainPatchsetRow]) -> u32 {
    // `ancestors` is ordered newest->oldest (`v(N-1)..v1`). Pick the newest
    // ancestor already posted by `created_at`, falling back to the oldest
    // ancestor (`v1`) when the comment predates every known ancestor date.
    // Callers drop comments posted at or after vN before getting here.
    for anc in ancestors {
        if anc.date > 0 && created_at >= anc.date {
            return anc.effective_version;
        }
    }
    ancestors.last().map(|a| a.effective_version).unwrap_or(1)
}

fn collect_chain_authors(
    curr_ps: &ChainPatchsetRow,
    ancestors: &[ChainPatchsetRow],
    patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for ps in std::iter::once(curr_ps).chain(ancestors.iter()) {
        if !ps.author.trim().is_empty() {
            seen.insert(ps.author.clone());
        }
    }
    for plist in patches_by_ps.values() {
        for p in plist {
            if let Some(ref a) = p.author
                && !a.trim().is_empty()
            {
                seen.insert(a.clone());
            }
        }
    }
    seen.into_iter().collect()
}

/// A pending discussion thread together with the vN patches it routes to.
struct RoutedThread {
    thread: PendingThread,
    destinations: Vec<(i64, PriorItemScope)>,
}

#[allow(clippy::too_many_arguments)]
fn route_pending_threads(
    pending_threads: Vec<PendingThread>,
    vn_descendant_of_anc_patch: &HashMap<i64, i64>,
    v_n_patches: &[&DecodedPatchEntry],
    patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
    patches_by_id: &HashMap<i64, &DecodedPatchEntry>,
    lineage_by_vn_patch: &HashMap<i64, Vec<i64>>,
    routing_targets: &[PatchRoutingTarget],
) -> Vec<RoutedThread> {
    let orphan_vn_ids: HashSet<i64> = v_n_patches
        .iter()
        .filter(|vp| vp.previous_patch_id.is_none())
        .map(|vp| vp.id)
        .collect();
    let vn_changed_idents: HashMap<i64, HashSet<String>> = v_n_patches
        .iter()
        .map(|vp| (vp.id, changed_line_identifiers(&vp.diff)))
        .collect();
    pending_threads
        .into_iter()
        .map(|thread| {
            let destinations = thread_destinations(
                &thread,
                &orphan_vn_ids,
                vn_descendant_of_anc_patch,
                patches_by_ps,
                patches_by_id,
                lineage_by_vn_patch,
                &vn_changed_idents,
                routing_targets,
            );
            RoutedThread {
                thread,
                destinations,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn thread_destinations(
    pt: &PendingThread,
    orphan_vn_ids: &HashSet<i64>,
    vn_descendant_of_anc_patch: &HashMap<i64, i64>,
    patches_by_ps: &HashMap<i64, Vec<DecodedPatchEntry>>,
    patches_by_id: &HashMap<i64, &DecodedPatchEntry>,
    lineage_by_vn_patch: &HashMap<i64, Vec<i64>>,
    vn_changed_idents: &HashMap<i64, HashSet<String>>,
    routing_targets: &[PatchRoutingTarget],
) -> Vec<(i64, PriorItemScope)> {
    let mut combined_text = String::new();
    if let Some(ref subj) = pt.subject {
        combined_text.push_str(subj);
        combined_text.push('\n');
    }
    for m in &pt.messages {
        combined_text.push_str(&m.trimmed_body);
        combined_text.push('\n');
    }

    if let Some(anc_pid) = pt.target_ancestor_patch_id {
        if let Some(&vn_pid) = vn_descendant_of_anc_patch.get(&anc_pid) {
            let mut dests = vec![(vn_pid, PriorItemScope::DirectPatchLineage)];
            if let Some(extra) = split_off_destination(
                pt,
                anc_pid,
                vn_pid,
                &combined_text,
                orphan_vn_ids,
                patches_by_id,
                routing_targets,
            ) {
                dests.push(extra);
            }
            return dests;
        }
        let Some(&anc_patch) = patches_by_id.get(&anc_pid) else {
            return Vec::new();
        };
        let squash_targets = squash_evidence_targets(
            anc_patch,
            patches_by_ps,
            patches_by_id,
            lineage_by_vn_patch,
            routing_targets,
            vn_changed_idents,
        );
        if squash_targets.is_empty() {
            return Vec::new();
        }
        for f in extract_patch_touched_files(&anc_patch.diff) {
            combined_text.push_str(&f);
            combined_text.push('\n');
        }
        return route_unattached_text_to_patches(
            &squash_targets,
            pt.explicit_file_path.as_deref(),
            &combined_text,
        );
    }

    route_unattached_text_to_patches(
        routing_targets,
        pt.explicit_file_path.as_deref(),
        &combined_text,
    )
}

/// When an ancestor patch was split in vN, leaving split-off files in a newly
/// introduced orphan vN patch (`previous_patch_id = NULL`), also routes the
/// ancestor patch's discussion to that split-off patch.
fn split_off_destination(
    pt: &PendingThread,
    anc_pid: i64,
    vn_pid: i64,
    combined_text: &str,
    orphan_vn_ids: &HashSet<i64>,
    patches_by_id: &HashMap<i64, &DecodedPatchEntry>,
    routing_targets: &[PatchRoutingTarget],
) -> Option<(i64, PriorItemScope)> {
    if orphan_vn_ids.is_empty() {
        return None;
    }
    let anc_patch = patches_by_id.get(&anc_pid)?;
    let anc_files = extract_patch_touched_files(&anc_patch.diff);
    let primary_files: HashSet<&str> = routing_targets
        .iter()
        .find(|t| t.patch_id == vn_pid)
        .map(|t| t.touched_files.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let split_off_files: HashSet<&str> = anc_files
        .iter()
        .map(String::as_str)
        .filter(|f| !primary_files.contains(*f))
        .collect();
    if split_off_files.is_empty() {
        return None;
    }
    let split_targets: Vec<PatchRoutingTarget> = routing_targets
        .iter()
        .filter(|t| {
            orphan_vn_ids.contains(&t.patch_id)
                && t.patch_id != vn_pid
                && t.touched_files
                    .iter()
                    .any(|f| split_off_files.contains(f.as_str()))
        })
        .cloned()
        .collect();
    match split_targets.as_slice() {
        [] => None,
        [only] => Some((only.patch_id, PriorItemScope::FileSymbolOverlap)),
        _ => route_unattached_text_to_patches(
            &split_targets,
            pt.explicit_file_path.as_deref(),
            combined_text,
        )
        .first()
        .map(|(best_id, _)| (*best_id, PriorItemScope::FileSymbolOverlap)),
    }
}

/// Builds authority evaluators only for vN patches that received at least
/// one thread.
fn build_target_evaluators(
    authority: &PatchsetAuthority<'_>,
    routed: &[RoutedThread],
    v_n_patches: &[&DecodedPatchEntry],
) -> HashMap<i64, PatchAuthorityEvaluator> {
    let needed: HashSet<i64> = routed
        .iter()
        .flat_map(|r| r.destinations.iter().map(|(id, _)| *id))
        .collect();
    let mut evaluators = HashMap::new();
    for vp in v_n_patches.iter().filter(|vp| needed.contains(&vp.id)) {
        let touched = extract_patch_touched_files(&vp.diff);
        evaluators.insert(vp.id, authority.evaluator_for(&touched));
    }
    evaluators
}

fn score_routed_threads(
    contexts: &mut HashMap<i64, PriorVersionContext>,
    routed: Vec<RoutedThread>,
    evaluators: &HashMap<i64, PatchAuthorityEvaluator>,
) {
    for RoutedThread {
        thread: pt,
        destinations,
    } in routed
    {
        for (target_patch_id, scope) in destinations {
            let Some(eval) = evaluators.get(&target_patch_id) else {
                continue;
            };
            let Some(ctx) = contexts.get_mut(&target_patch_id) else {
                continue;
            };
            let (highest_trust, has_high_trust_speaker, messages) =
                score_thread_messages(eval, &pt.messages);
            ctx.discussion_threads.push(PriorDiscussionThread {
                id: String::new(),
                version: pt.version,
                scope,
                subject: pt.subject.clone(),
                highest_trust,
                has_high_trust_speaker,
                messages,
            });
        }
    }
}

/// Scores every message of a thread, returning the highest effective trust,
/// whether any speaker is High, and the scored messages.
fn score_thread_messages(
    eval: &PatchAuthorityEvaluator,
    messages: &[UnscoredMessage],
) -> (TrustTier, bool, Vec<PriorCommentMessage>) {
    let mut highest_trust = TrustTier::Low;
    let mut has_high_trust_speaker = false;
    let mut scored_msgs = Vec::with_capacity(messages.len());
    for m in messages {
        let auth: ContributorAuthority = eval
            .evaluate(&m.author)
            .with_forge_association(m.author_association.as_deref());
        // Check non-author reviewer tier or effective unverifiable trust
        // so a High-trust maintainer thread is prioritized under the 512 KiB budget.
        let effective_for_priority = if auth.speaker_role == SpeakerRole::Reviewer {
            auth.tier
        } else {
            auth.unverifiable_claim_trust
        };
        highest_trust = highest_trust.max(effective_for_priority);
        has_high_trust_speaker |= effective_for_priority == TrustTier::High;
        scored_msgs.push(PriorCommentMessage {
            message_id: m.message_id.clone(),
            author: m.author.clone(),
            timestamp: m.timestamp,
            file_path: m.file_path.clone(),
            line: m.line,
            reference_url: m.reference_url.clone(),
            authority: auth,
            trimmed_body: m.trimmed_body.clone(),
        });
    }
    (highest_trust, has_high_trust_speaker, scored_msgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain_row(id: i64, version: u32) -> ChainPatchsetRow {
        ChainPatchsetRow {
            id,
            thread_id: None,
            cover_letter_message_id: None,
            subject: String::new(),
            author: String::new(),
            date: 0,
            mr_url: None,
            mr_number: None,
            slug: None,
            stored_version: None,
            previous_patchset_id: None,
            effective_version: version,
        }
    }

    fn patch_entry(id: i64, patchset_id: i64, part_index: i64, file: &str) -> DecodedPatchEntry {
        DecodedPatchEntry {
            id,
            patchset_id,
            message_id: format!("sha{id}"),
            part_index,
            previous_patch_id: None,
            diff: format!("diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n"),
            author: None,
            body: String::new(),
            thread_id: None,
        }
    }

    fn review_row(remote: i64, parent: Option<i64>, created_at: i64) -> DecodedForgeCommentRow {
        DecodedForgeCommentRow {
            id: remote,
            comment_source: "review_comment".to_string(),
            remote_comment_id: remote,
            in_reply_to_id: parent,
            author: "reviewer".to_string(),
            author_association: None,
            commit_id: None,
            file_path: None,
            line: None,
            body: format!("comment {remote}"),
            html_url: None,
            created_at,
        }
    }

    #[test]
    fn test_orphan_review_replies_become_their_own_thread() {
        let root = review_row(1, None, 10);
        let reply = review_row(2, Some(1), 20);
        let orphan_a = review_row(3, Some(99), 40);
        let orphan_b = review_row(4, Some(99), 30);
        let mut roots = vec![&root];
        let mut replies_by_root: BTreeMap<i64, Vec<&DecodedForgeCommentRow>> = BTreeMap::new();
        replies_by_root.entry(1).or_default().push(&reply);
        replies_by_root.entry(99).or_default().push(&orphan_a);
        replies_by_root.entry(99).or_default().push(&orphan_b);

        promote_orphan_review_replies(&mut roots, &mut replies_by_root);

        let root_ids: Vec<i64> = roots.iter().map(|r| r.remote_comment_id).collect();
        assert_eq!(root_ids, vec![1, 4]);
        assert!(!replies_by_root.contains_key(&99));
        assert_eq!(replies_by_root[&1].len(), 1);
        assert_eq!(replies_by_root[&4][0].remote_comment_id, 3);
    }

    #[test]
    fn test_forge_anchor_patch_does_not_pin_head_sha_comments_to_last_commit() {
        let ancestors = vec![chain_row(1, 1), chain_row(2, 2)];
        let mut patches_by_ps = HashMap::new();
        patches_by_ps.insert(1, vec![patch_entry(10, 1, 1, "drivers/a.c")]);
        patches_by_ps.insert(
            2,
            vec![
                patch_entry(20, 2, 1, "drivers/a.c"),
                patch_entry(21, 2, 2, "drivers/b.c"),
            ],
        );

        // Single-commit version: the commit identifies the patch.
        assert_eq!(
            forge_anchor_patch(10, None, &ancestors, &patches_by_ps),
            Some(10)
        );
        // Multi-commit version, comment anchored to the head commit (21) on a
        // file only the first commit touches: route to the first commit.
        assert_eq!(
            forge_anchor_patch(21, Some("drivers/a.c"), &ancestors, &patches_by_ps),
            Some(20)
        );
        // PR-level review summary (no file): let file and text routing decide.
        assert_eq!(
            forge_anchor_patch(21, None, &ancestors, &patches_by_ps),
            None
        );
        // File no patch of that version touches: no pinned patch.
        assert_eq!(
            forge_anchor_patch(21, Some("drivers/c.c"), &ancestors, &patches_by_ps),
            None
        );
    }
    use crate::db::{Finding, Severity};
    use crate::forge::ForgeCommentSource;
    use crate::settings::DatabaseSettings;
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

    #[allow(clippy::too_many_arguments)]
    async fn insert_test_message(
        db: &Database,
        message_id: &str,
        thread_id: i64,
        in_reply_to: Option<&str>,
        author: &str,
        subject: &str,
        date: i64,
        body: &str,
    ) {
        db.create_message(
            message_id,
            thread_id,
            in_reply_to,
            author,
            subject,
            date,
            body,
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
    }

    async fn seed_completed_review_with_finding(
        db: &Database,
        patchset_id: i64,
        patch_id: i64,
        problem: &str,
        locations: Option<serde_json::Value>,
        prior_attr: Option<&PriorAttribution>,
    ) -> i64 {
        let review_id = db
            .create_review(patchset_id, Some(patch_id), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.create_finding_with_attribution(
            Finding {
                review_id,
                severity: Severity::High,
                severity_explanation: Some("Memory leak on error path".to_string()),
                problem: problem.to_string(),
                preexisting: Some(false),
                locations,
            },
            prior_attr,
        )
        .await
        .unwrap();
        db.complete_review(
            review_id,
            "Reviewed",
            "ok",
            Some("summary"),
            None,
            Some("inline"),
            None,
        )
        .await
        .unwrap();
        review_id
    }

    #[tokio::test]
    async fn test_prior_context_multihop_cancelled_v2_inherits_v1_sashiko_and_all_human_threads() {
        let db = setup_db().await;
        let maintainers = MaintainersIndex::from_reader(std::io::Cursor::new(
            "Maintainers List\n\nNET CORE\nM:\tJakub Kicinski <kuba@kernel.org>\nS:\tMaintained\nF:\tdrivers/net/\n\n",
        ))
        .unwrap();

        let tid_v1 = db
            .create_thread("v1-cover@kernel.org", "[PATCH 0/1] net: foo", 100)
            .await
            .unwrap();
        insert_test_message(
            &db,
            "v1-cover@kernel.org",
            tid_v1,
            None,
            "Alice Author <alice@example.com>",
            "[PATCH 0/1] net: foo",
            100,
            "Series intro",
        )
        .await;
        insert_test_message(
            &db,
            "v1-p1@kernel.org",
            tid_v1,
            Some("v1-cover@kernel.org"),
            "Alice Author <alice@example.com>",
            "[PATCH 1/1] net: foo: add rx ring",
            101,
            "Commit msg v1",
        )
        .await;
        let ps_v1 = db
            .create_patchset(
                tid_v1,
                Some("v1-cover@kernel.org"),
                "v1-p1@kernel.org",
                "[PATCH 0/1] net: foo",
                "Alice Author <alice@example.com>",
                100,
                1,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v1 = db
            .create_patch(
                ps_v1,
                "v1-p1@kernel.org",
                1,
                "diff --git a/drivers/net/foo.c b/drivers/net/foo.c\n--- a/drivers/net/foo.c\n+++ b/drivers/net/foo.c\n@@ -1,2 +1,3 @@\n+    alloc_ring();\n",
            )
            .await
            .unwrap();
        seed_completed_review_with_finding(
            &db,
            ps_v1,
            p_v1,
            "alloc_ring() leaks dma mapping when register_netdev() fails.",
            None,
            None,
        )
        .await;

        // Jakub leaves a High-trust comment on v1.
        insert_test_message(
            &db,
            "kuba-v1-reply@kernel.org",
            tid_v1,
            Some("v1-p1@kernel.org"),
            "Jakub Kicinski <kuba@kernel.org>",
            "Re: [PATCH 1/1] net: foo: add rx ring",
            120,
            "> +    alloc_ring();\nPlease also tear down napi on probe failure.",
        )
        .await;

        // v2 is posted, gets a kernel test robot reply, and is Cancelled before Sashiko finishes!
        let tid_v2 = db
            .create_thread("v2-p1@kernel.org", "[PATCH v2] net: foo: add rx ring", 200)
            .await
            .unwrap();
        insert_test_message(
            &db,
            "v2-p1@kernel.org",
            tid_v2,
            None,
            "Alice Author <alice@example.com>",
            "[PATCH v2] net: foo: add rx ring",
            200,
            "Commit msg v2\n---\nChanges in v2:\n- rebased onto net-next\n",
        )
        .await;
        let ps_v2 = db
            .create_patchset(
                tid_v2,
                Some("v2-p1@kernel.org"),
                "v2-p1@kernel.org",
                "[PATCH v2] net: foo: add rx ring",
                "Alice Author <alice@example.com>",
                200,
                1,
                1,
                "",
                "",
                Some(2),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v2 = db
            .create_patch(
                ps_v2,
                "v2-p1@kernel.org",
                1,
                "diff --git a/drivers/net/foo.c b/drivers/net/foo.c\n--- a/drivers/net/foo.c\n+++ b/drivers/net/foo.c\n@@ -1,2 +1,4 @@\n+    alloc_ring();\n+    enable_ring();\n",
            )
            .await
            .unwrap();
        db.execute(
            "UPDATE patchsets SET previous_patchset_id = ?, status = 'Cancelled' WHERE id = ?",
            libsql::params![ps_v1, ps_v2],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v1, p_v2],
        )
        .await
        .unwrap();
        insert_test_message(
            &db,
            "lkp-v2-reply@intel.com",
            tid_v2,
            Some("v2-p1@kernel.org"),
            "kernel test robot <lkp@intel.com>",
            "Re: [PATCH v2] net: foo: add rx ring",
            210,
            "drivers/net/foo.c:42: warning: unused variable 'ret'",
        )
        .await;

        // v3 is posted linking to v2.
        let tid_v3 = db
            .create_thread("v3-p1@kernel.org", "[PATCH v3] net: foo: add rx ring", 300)
            .await
            .unwrap();
        insert_test_message(
            &db,
            "v3-p1@kernel.org",
            tid_v3,
            None,
            "Alice Author <alice@example.com>",
            "[PATCH v3] net: foo: add rx ring",
            300,
            "Commit msg v3\n---\nChanges in v3:\n- fix unused variable warning\n",
        )
        .await;
        let ps_v3 = db
            .create_patchset(
                tid_v3,
                Some("v3-p1@kernel.org"),
                "v3-p1@kernel.org",
                "[PATCH v3] net: foo: add rx ring",
                "Alice Author <alice@example.com>",
                300,
                1,
                1,
                "",
                "",
                Some(3),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v3 = db
            .create_patch(
                ps_v3,
                "v3-p1@kernel.org",
                1,
                "diff --git a/drivers/net/foo.c b/drivers/net/foo.c\n--- a/drivers/net/foo.c\n+++ b/drivers/net/foo.c\n@@ -1,2 +1,4 @@\n+    alloc_ring();\n+    enable_ring_checked();\n",
            )
            .await
            .unwrap();
        db.execute(
            "UPDATE patchsets SET previous_patchset_id = ? WHERE id = ?",
            libsql::params![ps_v2, ps_v3],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v2, p_v3],
        )
        .await
        .unwrap();

        let map = db
            .build_prior_version_contexts(ps_v3, None, Some(&maintainers))
            .await
            .unwrap();
        let ctx = map.get(&p_v3).expect("context for v3 patch");
        assert_eq!(ctx.current_version, 3);
        assert_eq!(ctx.ancestor_versions, vec![2, 1]);

        // Sashiko finding falls back across Cancelled v2 to Completed v1!
        assert_eq!(ctx.prior_findings.len(), 1);
        assert_eq!(ctx.prior_findings[0].id, "PF1");
        assert_eq!(ctx.prior_findings[0].reviewed_in_version, 1);
        assert_eq!(
            ctx.prior_findings[0].source_message_id.as_deref(),
            Some("v1-p1@kernel.org")
        );
        assert_eq!(
            ctx.prior_findings[0]
                .attribution
                .originally_raised_in_version,
            1
        );

        // Both v2 (lkp bot) and v1 (Jakub maintainer) threads are collected, newest version first.
        assert_eq!(ctx.discussion_threads.len(), 2);
        assert_eq!(ctx.discussion_threads[0].id, "HM1");
        assert_eq!(ctx.discussion_threads[0].version, 2);
        assert_eq!(
            ctx.discussion_threads[0].messages[0].authority.speaker_role,
            SpeakerRole::AutomatedBot
        );
        assert_eq!(ctx.discussion_threads[1].id, "HM2");
        assert_eq!(ctx.discussion_threads[1].version, 1);
        assert!(ctx.discussion_threads[1].has_high_trust_speaker);
        assert_eq!(
            ctx.discussion_threads[1].messages[0].authority.tier,
            TrustTier::High
        );
        assert!(
            ctx.inter_version_diff
                .as_deref()
                .unwrap()
                .contains("enable_ring_checked")
        );
        assert!(ctx.changelogs.iter().any(|c| c.version == 3));
        assert!(ctx.changelogs.iter().any(|c| c.version == 2));

        // Now mark v2 as `Reviewed` with 0 findings: v3 must NOT re-inherit the resolved v1 Sashiko finding!
        db.execute(
            "UPDATE patchsets SET status = 'In Review' WHERE id = ?",
            libsql::params![ps_v2],
        )
        .await
        .unwrap();
        let clean_rev_v2 = db
            .create_review(ps_v2, Some(p_v2), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.complete_review(
            clean_rev_v2,
            "Reviewed",
            "clean",
            Some("ok"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let map_after = db
            .build_prior_version_contexts(ps_v3, None, Some(&maintainers))
            .await
            .unwrap();
        assert!(
            map_after[&p_v3].prior_findings.is_empty(),
            "clean completed v2 review must stop fallback to resolved v1 Sashiko findings"
        );
        assert_eq!(
            map_after[&p_v3].discussion_threads.len(),
            2,
            "human/bot threads across all ancestors remain visible"
        );
    }

    #[tokio::test]
    async fn test_prior_context_shared_thread_id_prunes_other_versions_and_filters_sashiko_outbox()
    {
        let db = setup_db().await;
        // v2 is sent `--in-reply-to` v1 so everything shares `shared_tid`!
        let shared_tid = db
            .create_thread("v1-p1@kernel.org", "[PATCH] mm: fix", 100)
            .await
            .unwrap();
        insert_test_message(
            &db,
            "v1-p1@kernel.org",
            shared_tid,
            None,
            "Dev <dev@example.com>",
            "[PATCH] mm: fix",
            100,
            "v1 body",
        )
        .await;
        let ps_v1 = db
            .create_patchset(
                shared_tid,
                Some("v1-p1@kernel.org"),
                "v1-p1@kernel.org",
                "[PATCH] mm: fix",
                "Dev <dev@example.com>",
                100,
                1,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v1 = db
            .create_patch(
                ps_v1,
                "v1-p1@kernel.org",
                1,
                "diff --git a/mm/page.c b/mm/page.c\n+int a;\n",
            )
            .await
            .unwrap();

        // 1) Standalone Sashiko outbox email -> kept when v1 has no DB PF*
        // findings (e.g., review was Cancelled), then dropped once v1 has a
        // completed DB review with a finding.
        insert_test_message(
            &db,
            "sashiko-outbox-500@linux.dev",
            shared_tid,
            Some("v1-p1@kernel.org"),
            "Sashiko <sashiko@linux.dev>",
            "Re: [PATCH] mm: fix",
            110,
            "Automated Sashiko review text",
        )
        .await;

        // 2) Sashiko outbox email with a human reply -> MUST be kept!
        insert_test_message(
            &db,
            "sashiko-outbox-501@linux.dev",
            shared_tid,
            Some("v1-p1@kernel.org"),
            "Sashiko <sashiko@linux.dev>",
            "Re: [PATCH] mm: fix",
            115,
            "Automated Sashiko finding about lock",
        )
        .await;
        insert_test_message(
            &db,
            "human-on-sashiko@kernel.org",
            shared_tid,
            Some("sashiko-outbox-501@linux.dev"),
            "Reviewer <rev@kernel.org>",
            "Re: [PATCH] mm: fix",
            118,
            "> Automated Sashiko finding about lock\nSashiko is right, folio_lock is missing here.",
        )
        .await;

        // 3) Unrelated v2 patch sent `--in-reply-to` v1 in the SAME thread_id!
        insert_test_message(
            &db,
            "v2-p1@kernel.org",
            shared_tid,
            Some("v1-p1@kernel.org"),
            "Dev <dev@example.com>",
            "[PATCH v2] mm: fix",
            200,
            "v2 body",
        )
        .await;
        let ps_v2 = db
            .create_patchset(
                shared_tid,
                Some("v2-p1@kernel.org"),
                "v2-p1@kernel.org",
                "[PATCH v2] mm: fix",
                "Dev <dev@example.com>",
                200,
                1,
                1,
                "",
                "",
                Some(2),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v2 = db
            .create_patch(
                ps_v2,
                "v2-p1@kernel.org",
                1,
                "diff --git a/mm/page.c b/mm/page.c\n+int b;\n",
            )
            .await
            .unwrap();
        db.execute(
            "UPDATE patchsets SET previous_patchset_id = ? WHERE id = ?",
            libsql::params![ps_v1, ps_v2],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v1, p_v2],
        )
        .await
        .unwrap();

        // Reply to v2 itself (must NOT appear as a v1 comment when reviewing v2!).
        insert_test_message(
            &db,
            "reply-to-v2@kernel.org",
            shared_tid,
            Some("v2-p1@kernel.org"),
            "Other <other@kernel.org>",
            "Re: [PATCH v2] mm: fix",
            205,
            "Comment on v2 itself",
        )
        .await;

        // Before v1 has a completed DB review with findings, the standalone
        // `sashiko-outbox-500@linux.dev` email is preserved as a fallback
        // thread so findings on `Cancelled` ancestor patchsets are not lost.
        let map_unreviewed = db
            .build_prior_version_contexts(ps_v2, None, None)
            .await
            .unwrap();
        assert_eq!(map_unreviewed[&p_v2].discussion_threads.len(), 2);

        // Once v1 has a completed DB review with a finding (`PF1`), the
        // standalone outbox email is dropped while the human-replied subtree
        // is kept.
        db.execute(
            "UPDATE patchsets SET status = 'In Review' WHERE id = ?",
            libsql::params![ps_v1],
        )
        .await
        .unwrap();
        seed_completed_review_with_finding(
            &db,
            ps_v1,
            p_v1,
            "Automated Sashiko review text",
            None,
            None,
        )
        .await;
        let map = db
            .build_prior_version_contexts(ps_v2, None, None)
            .await
            .unwrap();
        let ctx = &map[&p_v2];
        assert_eq!(ctx.prior_findings.len(), 1);
        assert_eq!(ctx.discussion_threads.len(), 1);
        assert_eq!(ctx.discussion_threads[0].messages.len(), 2);
        assert_eq!(
            ctx.discussion_threads[0].messages[1].message_id,
            "human-on-sashiko@kernel.org"
        );

        // When v2 completes a clean review (`status = 'Reviewed'` with 0
        // findings), it supersedes v1's finding. On v3, v1's standalone
        // `sashiko-outbox-500@linux.dev` email must remain suppressed rather
        // than leaking back in as an `HM*` thread.
        let clean_rev_v2 = db
            .create_review(ps_v2, Some(p_v2), "gemini", "pro", None, None)
            .await
            .unwrap();
        db.complete_review(
            clean_rev_v2,
            "Reviewed",
            "ok",
            Some("No issues found."),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        insert_test_message(
            &db,
            "v3-p1@kernel.org",
            shared_tid,
            Some("v2-p1@kernel.org"),
            "Dev <dev@example.com>",
            "[PATCH v3] mm: fix",
            300,
            "v3 body",
        )
        .await;
        let ps_v3 = db
            .create_patchset(
                shared_tid,
                Some("v3-p1@kernel.org"),
                "v3-p1@kernel.org",
                "[PATCH v3] mm: fix",
                "Dev <dev@example.com>",
                300,
                1,
                1,
                "",
                "",
                Some(3),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v3 = db
            .create_patch(
                ps_v3,
                "v3-p1@kernel.org",
                1,
                "diff --git a/mm/page.c b/mm/page.c\n+int c;\n",
            )
            .await
            .unwrap();
        db.execute(
            "UPDATE patchsets SET previous_patchset_id = ? WHERE id = ?",
            libsql::params![ps_v2, ps_v3],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v2, p_v3],
        )
        .await
        .unwrap();
        let map_v3 = db
            .build_prior_version_contexts(ps_v3, None, None)
            .await
            .unwrap();
        let ctx_v3 = &map_v3[&p_v3];
        assert!(ctx_v3.prior_findings.is_empty());
        assert!(
            ctx_v3.discussion_threads.iter().all(|t| t
                .messages
                .iter()
                .all(|m| m.message_id != "sashiko-outbox-500@linux.dev")),
            "superseded v1 standalone Sashiko outbox email must not leak into v3"
        );
    }

    #[tokio::test]
    async fn test_prior_context_cover_letter_and_squashed_patch_routing() {
        let db = setup_db().await;
        let tid1 = db
            .create_thread("v1-0@k.org", "[PATCH 0/3] net", 100)
            .await
            .unwrap();
        insert_test_message(
            &db,
            "v1-0@k.org",
            tid1,
            None,
            "A <a@k.org>",
            "[PATCH 0/3] net",
            100,
            "cover",
        )
        .await;
        insert_test_message(
            &db,
            "v1-1@k.org",
            tid1,
            Some("v1-0@k.org"),
            "A <a@k.org>",
            "[PATCH 1/3] a",
            101,
            "p1",
        )
        .await;
        insert_test_message(
            &db,
            "v1-2@k.org",
            tid1,
            Some("v1-0@k.org"),
            "A <a@k.org>",
            "[PATCH 2/3] b",
            102,
            "p2",
        )
        .await;
        insert_test_message(
            &db,
            "v1-3@k.org",
            tid1,
            Some("v1-0@k.org"),
            "A <a@k.org>",
            "[PATCH 3/3] c",
            103,
            "p3",
        )
        .await;

        let ps_v1 = db
            .create_patchset(
                tid1,
                Some("v1-0@k.org"),
                "v1-0@k.org",
                "[PATCH 0/3] net",
                "A <a@k.org>",
                100,
                3,
                1,
                "",
                "",
                Some(1),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v1_1 = db
            .create_patch(
                ps_v1,
                "v1-1@k.org",
                1,
                "diff --git a/net/a.c b/net/a.c\n+void alpha_init(void);\n",
            )
            .await
            .unwrap();
        let p_v1_2 = db
            .create_patch(
                ps_v1,
                "v1-2@k.org",
                2,
                "diff --git a/net/b.c b/net/b.c\n+void beta_setup_queue(void);\n",
            )
            .await
            .unwrap();
        let p_v1_3 = db
            .create_patch(
                ps_v1,
                "v1-3@k.org",
                3,
                "diff --git a/net/b.c b/net/b.c\n+void beta_cleanup_Helper(void);\n",
            )
            .await
            .unwrap();

        // Finding on v1 patch 3, which gets squashed into patch 2 in v2 (`previous_patch_id` only links p2->v1_2).
        seed_completed_review_with_finding(
            &db,
            ps_v1,
            p_v1_3,
            "beta_cleanup_Helper() frees ring before napi_disable().",
            Some(serde_json::json!([{ "file": "net/b.c" }])),
            None,
        )
        .await;

        // Cover-letter reply mentioning `beta_setup_queue` -> routes to v2 patch 2!
        insert_test_message(
            &db,
            "cover-reply-sym@k.org",
            tid1,
            Some("v1-0@k.org"),
            "Rev <rev@k.org>",
            "Re: [PATCH 0/3] net",
            110,
            "In patch 2, beta_setup_queue() can sleep under rcu_read_lock.",
        )
        .await;
        // Series-wide cover-letter reply -> falls back once to v2 patch 1!
        insert_test_message(
            &db,
            "cover-reply-general@k.org",
            tid1,
            Some("v1-0@k.org"),
            "Rev2 <rev2@k.org>",
            "Re: [PATCH 0/3] net",
            112,
            "Please target net-next rather than net.",
        )
        .await;

        let tid2 = db
            .create_thread("v2-0@k.org", "[PATCH v2 0/2] net", 200)
            .await
            .unwrap();
        insert_test_message(
            &db,
            "v2-0@k.org",
            tid2,
            None,
            "A <a@k.org>",
            "[PATCH v2 0/2] net",
            200,
            "cover v2",
        )
        .await;
        insert_test_message(
            &db,
            "v2-1@k.org",
            tid2,
            Some("v2-0@k.org"),
            "A <a@k.org>",
            "[PATCH v2 1/2] a",
            201,
            "p1",
        )
        .await;
        insert_test_message(
            &db,
            "v2-2@k.org",
            tid2,
            Some("v2-0@k.org"),
            "A <a@k.org>",
            "[PATCH v2 2/2] b",
            202,
            "p2",
        )
        .await;
        let ps_v2 = db
            .create_patchset(
                tid2,
                Some("v2-0@k.org"),
                "v2-0@k.org",
                "[PATCH v2 0/2] net",
                "A <a@k.org>",
                200,
                2,
                1,
                "",
                "",
                Some(2),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v2_1 = db
            .create_patch(
                ps_v2,
                "v2-1@k.org",
                1,
                "diff --git a/net/a.c b/net/a.c\n+void alpha_init(void);\n",
            )
            .await
            .unwrap();
        let p_v2_2 = db
            .create_patch(ps_v2, "v2-2@k.org", 2, "diff --git a/net/b.c b/net/b.c\n+void beta_setup_queue(void);\n+void beta_cleanup_Helper(void);\n")
            .await
            .unwrap();
        db.execute(
            "UPDATE patchsets SET previous_patchset_id = ? WHERE id = ?",
            libsql::params![ps_v1, ps_v2],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v1_1, p_v2_1],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v1_2, p_v2_2],
        )
        .await
        .unwrap();

        let map = db
            .build_prior_version_contexts(ps_v2, None, None)
            .await
            .unwrap();
        assert_eq!(map[&p_v2_1].discussion_threads.len(), 1);
        assert_eq!(
            map[&p_v2_1].discussion_threads[0].scope,
            PriorItemScope::SeriesCoverFallback
        );
        assert!(map[&p_v2_1].prior_findings.is_empty());

        assert_eq!(map[&p_v2_2].discussion_threads.len(), 1);
        assert_eq!(
            map[&p_v2_2].discussion_threads[0].scope,
            PriorItemScope::FileSymbolOverlap
        );
        assert_eq!(map[&p_v2_2].prior_findings.len(), 1);
        assert_eq!(
            map[&p_v2_2].prior_findings[0].scope,
            PriorItemScope::FileSymbolOverlap
        );
    }

    #[tokio::test]
    async fn test_prior_context_forge_comments_compression_and_normalization() {
        let db = setup_db().await;
        let sha_v1 = "1111111111111111111111111111111111111111";
        let sha_v2 = "2222222222222222222222222222222222222222";
        let mr_url = "https://github.com/sashiko-dev/sashiko/pull/77";

        let tid1 = db
            .create_thread(sha_v1, "#77: Add cache", 100)
            .await
            .unwrap();
        insert_test_message(
            &db,
            sha_v1,
            tid1,
            None,
            "Contributor <c@example.com>",
            "#77: Add cache",
            100,
            "msg",
        )
        .await;
        let ps_v1 = db
            .create_patchset(
                tid1,
                Some(sha_v1),
                sha_v1,
                "#77: Add cache",
                "Contributor <c@example.com>",
                100,
                1,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v1 = db
            .create_patch(
                ps_v1,
                sha_v1,
                1,
                "diff --git a/src/cache.rs b/src/cache.rs\n+pub fn lookup() {}\n",
            )
            .await
            .unwrap();
        db.update_patchset_mr_metadata(
            ps_v1,
            Some(mr_url),
            Some("Add cache"),
            Some(77),
            Some("sashiko-77"),
        )
        .await
        .unwrap();

        let tid2 = db
            .create_thread(sha_v2, "#77 [v2]: Add cache", 300)
            .await
            .unwrap();
        insert_test_message(
            &db,
            sha_v2,
            tid2,
            None,
            "Contributor <c@example.com>",
            "#77 [v2]: Add cache",
            300,
            "msg v2",
        )
        .await;
        let ps_v2 = db
            .create_patchset(
                tid2,
                Some(sha_v2),
                sha_v2,
                "#77 [v2]: Add cache",
                "Contributor <c@example.com>",
                300,
                1,
                1,
                "",
                "",
                Some(2),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let p_v2 = db
            .create_patch(
                ps_v2,
                sha_v2,
                1,
                "diff --git a/src/cache.rs b/src/cache.rs\n+pub fn lookup_v2() {}\n",
            )
            .await
            .unwrap();
        db.update_patchset_mr_metadata(
            ps_v2,
            Some(mr_url),
            Some("Add cache"),
            Some(77),
            Some("sashiko-77"),
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v1, p_v2],
        )
        .await
        .unwrap();

        let long_review_note = format!(
            "Please guard cache eviction against concurrent writers.\n{}",
            "Context detail line for compression check.\n".repeat(40)
        );
        assert!(long_review_note.len() > 1024);

        db.upsert_forge_comments(&[
            ForgeFetchedComment {
                provider: "github".to_string(),
                repo: "sashiko-dev/sashiko".to_string(),
                pr_number: 77,
                comment_source: ForgeCommentSource::ReviewComment,
                remote_comment_id: 9001,
                in_reply_to_id: None,
                author: "repo-maintainer".to_string(),
                author_association: Some("MEMBER".to_string()),
                commit_id: Some(sha_v1.to_string()),
                file_path: Some("src/cache.rs".to_string()),
                line: Some(15),
                body: long_review_note,
                html_url: Some(
                    "https://github.com/sashiko-dev/sashiko/pull/77#discussion_r9001".to_string(),
                ),
                created_at: 150,
                updated_at: 150,
            },
            ForgeFetchedComment {
                provider: "github".to_string(),
                repo: "sashiko-dev/sashiko".to_string(),
                pr_number: 77,
                comment_source: ForgeCommentSource::ReviewComment,
                remote_comment_id: 9002,
                in_reply_to_id: Some(9001),
                author: "Contributor".to_string(),
                author_association: Some("NONE".to_string()),
                commit_id: Some(sha_v1.to_string()),
                file_path: Some("src/cache.rs".to_string()),
                line: Some(15),
                body: "Updated in v2.".to_string(),
                html_url: Some(
                    "https://github.com/sashiko-dev/sashiko/pull/77#discussion_r9002".to_string(),
                ),
                created_at: 160,
                updated_at: 160,
            },
            // Posted after v2 existed, so it is feedback on v2, not on v1.
            ForgeFetchedComment {
                provider: "github".to_string(),
                repo: "sashiko-dev/sashiko".to_string(),
                pr_number: 77,
                comment_source: ForgeCommentSource::IssueComment,
                remote_comment_id: 9003,
                in_reply_to_id: None,
                author: "repo-maintainer".to_string(),
                author_association: Some("MEMBER".to_string()),
                commit_id: None,
                file_path: None,
                line: None,
                body: "The v2 eviction path still races with writers.".to_string(),
                html_url: None,
                created_at: 350,
                updated_at: 350,
            },
        ])
        .await
        .unwrap();

        // Verify SQLite compression invariant (`typeof(body) = 'blob'` for > 1024 bytes).
        let mut comp_rows = db
            .conn
            .query(
                "SELECT typeof(body) FROM forge_comments WHERE remote_comment_id = 9001",
                (),
            )
            .await
            .unwrap();
        let col_type: String = comp_rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(col_type, "blob");

        let map = db
            .build_prior_version_contexts(ps_v2, None, None)
            .await
            .unwrap();
        let ctx = &map[&p_v2];
        assert_eq!(ctx.discussion_threads.len(), 1);
        assert_eq!(ctx.discussion_threads[0].id, "HM1");
        assert!(ctx.discussion_threads[0].has_high_trust_speaker);
        assert_eq!(ctx.discussion_threads[0].messages.len(), 2);
        assert_eq!(
            ctx.discussion_threads[0].messages[0].authority.tier,
            TrustTier::High
        );
        assert_eq!(
            ctx.discussion_threads[0].messages[1].authority.speaker_role,
            crate::authority::SpeakerRole::PatchAuthor
        );
    }

    #[tokio::test]
    async fn test_prior_context_dropped_patch_findings_are_not_forwarded() {
        let db = setup_db().await;
        let mut patch_ids = Vec::new();
        let mut ps_ids = Vec::new();
        for (version, parts, date) in [(1u32, 2u32, 100i64), (2, 1, 200)] {
            let cover = format!("drop-v{version}-0@k.org");
            let subject = format!("[PATCH v{version} 0/{parts}] net");
            let tid = db.create_thread(&cover, &subject, date).await.unwrap();
            insert_test_message(
                &db,
                &cover,
                tid,
                None,
                "A <a@k.org>",
                &subject,
                date,
                "cover",
            )
            .await;
            let ps = db
                .create_patchset(
                    tid,
                    Some(&cover),
                    &cover,
                    &subject,
                    "A <a@k.org>",
                    date,
                    parts,
                    1,
                    "",
                    "",
                    Some(version),
                    0,
                    None,
                    true,
                    None,
                    None,
                )
                .await
                .unwrap()
                .unwrap();
            ps_ids.push(ps);
            for part in 1..=parts {
                let msgid = format!("drop-v{version}-{part}@k.org");
                insert_test_message(
                    &db,
                    &msgid,
                    tid,
                    Some(&cover),
                    "A <a@k.org>",
                    &format!("[PATCH v{version} {part}/{parts}] net"),
                    date + i64::from(part),
                    "patch",
                )
                .await;
                // Part 2 of v1 touches the same file and even shares the
                // `beta_setup_queue` scaffolding identifier that Part 1
                // already has, plus its own `gamma_stats_dump` symbol, and
                // v2 drops Part 2 (or applies it upstream).
                let diff = if part == 1 {
                    "diff --git a/net/b.c b/net/b.c\n+void beta_setup_queue(void);\n"
                } else {
                    "diff --git a/net/b.c b/net/b.c\n+void beta_setup_queue(void);\n+void gamma_stats_dump(void);\n"
                };
                patch_ids.push(db.create_patch(ps, &msgid, part, diff).await.unwrap());
            }
            if version == 1 {
                seed_completed_review_with_finding(
                    &db,
                    ps,
                    patch_ids[1],
                    "gamma_stats_dump() reads the counters without the stats lock in net/b.c.",
                    Some(serde_json::json!([{ "file": "net/b.c" }])),
                    None,
                )
                .await;
                insert_test_message(
                    &db,
                    "reply-on-dropped-v1-2@k.org",
                    tid,
                    Some("drop-v1-2@k.org"),
                    "Reviewer <rev@k.org>",
                    "Re: [PATCH v1 2/2] net",
                    150,
                    "gamma_stats_dump() also needs a NULL check before beta_setup_queue().",
                )
                .await;
            }
        }
        let (p_v1_1, p_v2_1) = (patch_ids[0], patch_ids[2]);
        db.execute(
            "UPDATE patchsets SET previous_patchset_id = ? WHERE id = ?",
            libsql::params![ps_ids[0], ps_ids[1]],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v1_1, p_v2_1],
        )
        .await
        .unwrap();
        let map = db
            .build_prior_version_contexts(ps_ids[1], None, None)
            .await
            .unwrap();
        // The v1 finding is still stored, so the empty result is the
        // squash-evidence gate rather than a missing review.
        let mut rows = db
            .conn
            .query(
                "SELECT COUNT(*) FROM findings f JOIN reviews r ON r.id = f.review_id
                 WHERE r.status = 'Reviewed'",
                (),
            )
            .await
            .unwrap();
        let stored: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(stored, 1);
        assert!(
            map[&p_v2_1].prior_findings.is_empty(),
            "{:?}",
            map[&p_v2_1].prior_findings
        );
        assert!(
            map[&p_v2_1].discussion_threads.is_empty(),
            "Discussion thread on dropped v1 patch 2/2 must not leak onto surviving v2 patch 1/1: {:?}",
            map[&p_v2_1].discussion_threads
        );
    }

    async fn create_single_patch_version(
        db: &Database,
        msgid: &str,
        subject: &str,
        date: i64,
        version: u32,
        diff: &str,
    ) -> (i64, i64) {
        let tid = db.create_thread(msgid, subject, date).await.unwrap();
        insert_test_message(
            db,
            msgid,
            tid,
            None,
            "Dev <dev@example.com>",
            subject,
            date,
            "body",
        )
        .await;
        let ps = db
            .create_patchset(
                tid,
                Some(msgid),
                msgid,
                subject,
                "Dev <dev@example.com>",
                date,
                1,
                1,
                "",
                "",
                Some(version),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch = db.create_patch(ps, msgid, 1, diff).await.unwrap();
        (ps, patch)
    }

    #[tokio::test]
    async fn test_prior_context_follows_cross_thread_replies_and_skips_foreign_patches() {
        let db = setup_db().await;
        let diff = "diff --git a/mm/page.c b/mm/page.c\n+int folio_page_count;\n";
        let (ps_v1, p_v1) =
            create_single_patch_version(&db, "x-v1@kernel.org", "[PATCH] mm: fix", 100, 1, diff)
                .await;
        let (ps_v2, p_v2) =
            create_single_patch_version(&db, "x-v2@kernel.org", "[PATCH v2] mm: fix", 300, 2, diff)
                .await;
        db.execute(
            "UPDATE patchsets SET previous_patchset_id = ? WHERE id = ?",
            libsql::params![ps_v1, ps_v2],
        )
        .await
        .unwrap();
        db.execute(
            "UPDATE patches SET previous_patch_id = ? WHERE id = ?",
            libsql::params![p_v1, p_v2],
        )
        .await
        .unwrap();

        // A reply filed under another thread, and a reply to it in a third
        // thread, are reached through two in_reply_to hops.
        let other_tid = db.create_thread("hop1@k.org", "Re: x", 150).await.unwrap();
        insert_test_message(
            &db,
            "hop1@k.org",
            other_tid,
            Some("<x-v1@kernel.org>"),
            "Rev <rev@kernel.org>",
            "Re: [PATCH] mm: fix",
            150,
            "folio_page_count can overflow on huge folios.",
        )
        .await;
        let third_tid = db.create_thread("hop2@k.org", "Re: x", 160).await.unwrap();
        insert_test_message(
            &db,
            "hop2@k.org",
            third_tid,
            Some("hop1@k.org"),
            "Dev <dev@example.com>",
            "Re: [PATCH] mm: fix",
            160,
            "Will widen it to unsigned long in v2.",
        )
        .await;
        // A foreign series posted as a reply to v1 is a barrier: neither it
        // nor replies under it belong to this lineage.
        let (_ps_foreign, _) = create_single_patch_version(
            &db,
            "foreign@k.org",
            "[PATCH] mm: unrelated",
            170,
            1,
            "diff --git a/mm/other.c b/mm/other.c\n+int other;\n",
        )
        .await;
        db.execute(
            "UPDATE messages SET in_reply_to = 'x-v1@kernel.org' WHERE message_id = 'foreign@k.org'",
            (),
        )
        .await
        .unwrap();
        insert_test_message(
            &db,
            "foreign-reply@k.org",
            third_tid,
            Some("foreign@k.org"),
            "Rev <rev@kernel.org>",
            "Re: [PATCH] mm: unrelated",
            180,
            "This unrelated series should never be attributed to mm: fix.",
        )
        .await;

        let map = db
            .build_prior_version_contexts(ps_v2, None, None)
            .await
            .unwrap();
        let threads = &map[&p_v2].discussion_threads;
        assert_eq!(threads.len(), 1, "{threads:?}");
        let ids: Vec<&str> = threads[0]
            .messages
            .iter()
            .map(|m| m.message_id.as_str())
            .collect();
        assert_eq!(ids, vec!["hop1@k.org", "hop2@k.org"]);
    }

    #[test]
    fn test_resolve_forge_provider_prefers_configuration_over_url() {
        let gh = "https://github.com/sashiko-dev/sashiko/pull/77";
        assert_eq!(resolve_forge_provider(gh, None).as_deref(), Some("github"));
        assert_eq!(
            resolve_forge_provider("https://gitlab.com/a/b/-/merge_requests/3", None).as_deref(),
            Some("gitlab")
        );
        // A path or self-hosted name mentioning gitlab is not a provider.
        assert_eq!(
            resolve_forge_provider("https://github.com/gitlab-org/tool/pull/1", None).as_deref(),
            Some("github")
        );
        assert_eq!(
            resolve_forge_provider("https://code.example.com/a/b/pull/1", None),
            None
        );
        assert_eq!(
            resolve_forge_provider("https://code.example.com/a/b/pull/1", Some("GitLab"))
                .as_deref(),
            Some("gitlab")
        );
    }

    #[tokio::test]
    async fn test_prior_context_explain_query_plan_uses_indexes_only() {
        let db = setup_db().await;
        let queries = [
            "EXPLAIN QUERY PLAN SELECT id, thread_id, cover_letter_message_id, subject, author, date, mr_url, mr_number, slug, version, previous_patchset_id FROM patchsets WHERE id = 1 LIMIT 1",
            "EXPLAIN QUERY PLAN SELECT p.id, p.patchset_id, p.message_id, p.part_index, p.previous_patch_id, p.diff, m.author, m.body, m.thread_id FROM patches p LEFT JOIN messages m ON m.message_id = p.message_id WHERE p.patchset_id = 1",
            "EXPLAIN QUERY PLAN SELECT id FROM reviews WHERE patchset_id = 1 AND patch_id = 1 AND status = 'Reviewed' ORDER BY id DESC LIMIT 1",
            "EXPLAIN QUERY PLAN SELECT severity, severity_explanation, problem, locations, prior_attribution FROM findings WHERE review_id = 1 ORDER BY id ASC",
            "EXPLAIN QUERY PLAN SELECT id, message_id, in_reply_to, references_hdr, author, subject, date FROM messages WHERE thread_id = 1 ORDER BY date ASC, id ASC LIMIT 10",
            "EXPLAIN QUERY PLAN SELECT id, message_id, in_reply_to, references_hdr, author, subject, date FROM messages WHERE in_reply_to IN (SELECT value FROM json_each('[\"x@k.org\"]')) ORDER BY date ASC, id ASC LIMIT 10",
            "EXPLAIN QUERY PLAN SELECT cover_letter_message_id FROM patchsets WHERE cover_letter_message_id IN (SELECT value FROM json_each('[\"x@k.org\"]'))",
            "EXPLAIN QUERY PLAN SELECT message_id FROM patches WHERE message_id IN (SELECT value FROM json_each('[\"x@k.org\"]'))",
            "EXPLAIN QUERY PLAN SELECT id, body FROM messages WHERE id IN (SELECT value FROM json_each('[1, 2]'))",
            "EXPLAIN QUERY PLAN SELECT id, comment_source, remote_comment_id, in_reply_to_id, author, author_association, commit_id, file_path, line, body, html_url, created_at FROM forge_comments WHERE provider = 'github' AND repo = 'a/b' AND pr_number = 1 ORDER BY created_at ASC, id ASC",
        ];

        for q in queries {
            let mut rows = db.conn.query(q, ()).await.unwrap();
            let mut details = Vec::new();
            while let Some(r) = rows.next().await.unwrap() {
                let detail: String = r.get(3).unwrap();
                details.push(detail);
            }
            // The `json_each` table-valued function is always scanned; it
            // holds only the bound batch.
            for d in details.iter().filter(|d| !d.contains("json_each")) {
                assert!(
                    !d.starts_with("SCAN "),
                    "Query must use indexed SEARCH rather than full table SCAN: {q} -> {details:?}"
                );
            }
        }
    }

    #[test]
    fn test_group_email_subtrees_falls_back_to_references_when_parent_missing() {
        let mut root_map = HashMap::new();
        root_map.insert(
            "v1-patch@example.com".to_string(),
            AncestorRootKind::Patch {
                version: 1,
                patch_id: 10,
            },
        );
        root_map.insert(
            "v1-patch-2@example.com".to_string(),
            AncestorRootKind::Patch {
                version: 1,
                patch_id: 11,
            },
        );
        let barrier_norms = HashSet::new();
        let mut covered_sashiko_roots = CoveredSashikoRoots::default();
        covered_sashiko_roots.patch_ids.insert(10);
        let messages = vec![
            EmailMessageMeta {
                id: 2,
                message_id: "<reply-2@example.com>".to_string(),
                norm_id: "reply-2@example.com".to_string(),
                in_reply_to: Some("<missing-intermediate@example.com>".to_string()),
                references_hdr: Some(
                    "<v1-patch@example.com> <missing-intermediate@example.com>".to_string(),
                ),
                author: "Reviewer <rev@example.com>".to_string(),
                subject: "Re: [PATCH] foo".to_string(),
                date: 100,
            },
            // Covered patch 10: standalone Sashiko email is dropped.
            EmailMessageMeta {
                id: 3,
                message_id: "<sashiko-outbox-10@linux.dev>".to_string(),
                norm_id: "sashiko-outbox-10@linux.dev".to_string(),
                in_reply_to: Some("<v1-patch@example.com>".to_string()),
                references_hdr: None,
                author: "Sashiko <sashiko@linux.dev>".to_string(),
                subject: "Re: [PATCH 1/2] foo".to_string(),
                date: 101,
            },
            // Uncovered sibling patch 11 in the same version: standalone
            // Sashiko email is preserved as fallback.
            EmailMessageMeta {
                id: 4,
                message_id: "<sashiko-outbox-11@linux.dev>".to_string(),
                norm_id: "sashiko-outbox-11@linux.dev".to_string(),
                in_reply_to: Some("<v1-patch-2@example.com>".to_string()),
                references_hdr: None,
                author: "Sashiko <sashiko@linux.dev>".to_string(),
                subject: "Re: [PATCH 2/2] bar".to_string(),
                date: 102,
            },
        ];

        let subtrees =
            group_email_subtrees(&messages, &root_map, &barrier_norms, &covered_sashiko_roots);
        assert_eq!(subtrees.len(), 2);
        assert_eq!(subtrees[0].members[0].norm_id, "reply-2@example.com");
        assert_eq!(
            subtrees[1].members[0].norm_id,
            "sashiko-outbox-11@linux.dev"
        );
    }

    #[test]
    fn test_assign_effective_versions_saturates_at_u32_max() {
        let mut curr = ChainPatchsetRow {
            id: 2,
            thread_id: None,
            cover_letter_message_id: None,
            subject: "[PATCH] next".to_string(),
            author: "Author <a@example.com>".to_string(),
            date: 200,
            mr_url: None,
            mr_number: None,
            slug: None,
            stored_version: None,
            previous_patchset_id: Some(1),
            effective_version: 0,
        };
        let mut ancestors = vec![ChainPatchsetRow {
            id: 1,
            thread_id: None,
            cover_letter_message_id: None,
            subject: "[PATCH] prev".to_string(),
            author: "Author <a@example.com>".to_string(),
            date: 100,
            mr_url: None,
            mr_number: None,
            slug: None,
            stored_version: Some(i64::from(u32::MAX)),
            previous_patchset_id: None,
            effective_version: 0,
        }];

        assign_effective_versions(&mut curr, &mut ancestors);
        assert_eq!(ancestors[0].effective_version, u32::MAX);
        assert_eq!(curr.effective_version, u32::MAX);
    }
}
