# Design: Credit Only People, and Only for the Same Defect

Status: Proposed

Related: `designs/DESIGN_PREVIOUS_VERSION_FEEDBACK.md` (Section 4.2,
"Evidence-Anchored prior-attribution"), `docs/benchmarking.md`
("Attribution ground truth").

## 1. Problem

Every attributed finding is rendered as `As reported by <who> on <version>`,
crediting the earliest version that has validated evidence. Two gaps make
that line claim more than it should:

- **Sashiko credits itself.** Prior Sashiko findings (`PF*`) and Sashiko
  emails inside `HM*` threads are citable, so reviews print "As reported by
  Sashiko on vK". That adds nothing for the reader, because the current
  review is Sashiko's own opinion either way. It invites disputes over
  Sashiko's history instead of the code, and it repeats Sashiko's own nits
  with a credit line attached.
- **Similar bugs can be credited.** The prompt asks for "the same issue
  (same defect on the same code, resource, or symbol)". It does not say
  explicitly that an analogous bug elsewhere (another function, another
  error path, another symbol of the same kind) does not count.

### 1.1 Motivating case

LKML patchset "tracefs: Add read-only eventfs filesystem at
/sys/kernel/events" (Steven Rostedt, v2..v7, sashiko.dev patchset 101781):

| Version | Event |
|---------|-------|
| v4 | Sashiko: read-only eventfs inodes sit on the global `tracefs_inodes` list, so `tracefs_apply_options()` overwrites their `i_uid`/`i_gid` on a tracefs remount. |
| v5 | Sashiko repeats the v4 finding. |
| v6 | Author adds `TRACEFS_RO_EVENT_INODE`, but sets it only on child inodes. Sashiko: "As reported by Sashiko on v4 [2], should `eventfs_create_events_ro_copy()` also set `TRACEFS_RO_EVENT_INODE` on the root inode?" |
| v6 reply | Author: "Well, it couldn't have been reported in v4 as that flag was just added in v6 ;-)" |
| v7 | Sashiko repeats "As reported by Sashiko on v4" for the remaining root-inode window. |

Every cited source was Sashiko. Under this design neither review prints a
credit line, and the findings themselves are unchanged.

### 1.2 What stays: incomplete fixes of human reports

Rule 3 of `STAGE_PRIOR_ATTRIBUTION_INSTRUCTION` ("Incomplete or broken fixes
in `vN`") credits a reviewer when the author's fix for their report falls
short. That rule is deliberate (commit 99f710708f70), and the versioned
benchmark encodes it: about half of the human-credit entries in
`benchmark_versioned.json` are exactly this pattern (Paolo Abeni on
`net_qlcnic_sriov_vlan_leak`, Dan Carpenter on
`staging_most_video_unreg_leak`, Ilpo Järvinen on `amd_pmf_kzalloc_errptr`,
Oliver Neukum on `chain_net_mctp_usb_dev_ref_leak`, Eric Dumazet on
`net_l2tp_dst_release`). For maintainers, "Dan raised this on v3, and the v4
fix still misses X" is the most useful credit there is. Dropping it would
also make Sashiko look like it is taking credit for a human's point.

The tracefs dispute does not argue against this rule: the issue there was
raised only by Sashiko, which Section 3.1 removes.

## 2. Goals and Non-Goals

Goals:
- Never credit Sashiko.
- Never credit a message that raised a merely similar or analogous bug.
- Keep crediting people (and third-party bots) for the same defect,
  including when the author's fix for their report is incomplete or broken.
- Keep the deterministic quote check: missing credit stays acceptable,
  wrong credit stays unacceptable.

Non-goals:
- Changing which findings are reported.
- Changing the `prior-feedback` audit. Prior Sashiko findings stay in the
  audit and in `prior_feedback_resolutions`; only the credit line goes away.
- Suppressing findings the patch author explicitly declined (for example the
  kernel-doc rename finding repeated on v6 and v7 after the author said "I
  rather keep it as is" on v5). That gets its own design.

## 3. Design

### 3.1 Linkage versus credit

`prior_attribution` currently serves two purposes that this design pulls
apart:

- **Linkage:** the finding is the same issue as these prior items. The
  review map (`enrich_prior_resolutions()` and `compute_v_n_outcome()` in
  `review_map.rs`) and `found_by()` depend on it. A `None` result stamps
  `prior_attribution_cleared`, which downstream means "the linkage was
  refuted": `thread_matches_prior_id()` then ignores `prior_source_ids`, a
  still-unaddressed `PF*` resolution becomes `dismissed_in_verification`
  instead of `confirmed_finding`, and `found_by()` reports `Fresh` instead of
  `Both`.
- **Credit:** the `attribution_line` printed in the review.

Linkage must keep working for Sashiko sources; only credit goes away.
Therefore:

- `PF*` items and Sashiko `HM*` messages stay rendered and citable in the
  `prior-attribution` stage, exactly as today. The stage keeps running
  whenever there is prior context, and `prior_attribution_cleared` keeps its
  current meaning.
- `compute_prior_attribution()` keeps all validated evidence, but picks the
  credited speaker as the earliest **non-Sashiko** source. Mixed evidence
  (`PF1` on v1, a human message on v2) credits the human on v2.
- When every validated source is Sashiko, it still returns a
  `PriorAttribution` (so linkage survives), with `origin_kind:
  sashiko_review`, `originally_raised_in_version` from the earliest evidence,
  and no credit: `stamp_attribution()` leaves `attribution_line` as `None`.
- `PriorAttribution::is_credit()` (`origin_kind != SashikoReview`) is the
  single predicate for "print or score credit"; no caller compares
  `origin_kind` directly.

The report stage must not credit Sashiko on its own initiative from the JSON
it sees:

- The report inputs built in `linux_patch_review.rs`, `gcc_patch_review.rs`,
  and `sashiko_patch_review.rs` drop `prior_attribution` from findings for
  which `is_credit()` is false.
- The rule in `projects/linux/prompts/inline-template.md`,
  `projects/gcc/prompts/inline-template.md`, and
  `projects/sashiko/prompts/github-summary-template.md` changes from
  "Never credit earlier reviewers unless prior_attribution is present" to
  "unless prior_attribution with an attribution_line is present".

Third-party bots (`SpeakerRole::AutomatedBot`, e.g. syzbot or the kernel
test robot) keep credit: they are independent reporters, and the benchmark
credits them (`net_ipv6_gro_udp6_static`, `drm_msm_kms_aspace_errptr`).

### 3.2 Similar bugs do not count

Rule 4 of `STAGE_PRIOR_ATTRIBUTION_INSTRUCTION` ("No false attribution")
gains an explicit sentence:

> A similar or analogous bug does not count: the same mistake in another
> function, on another error path, or on another symbol of the same kind is
> a different issue. When unsure whether a message raised the same defect,
> return empty evidence.

Rule 3 (incomplete or broken fixes) is unchanged. Its scope is already
limited to the same site that the earlier reviewer flagged.

There is no schema change: the output stays `{finding_id, evidence}`.

### 3.3 Benchmark ground truth and scoring

Three `detect_with_attribution` entries in `benchmark_versioned.json`
expect `"Sashiko"` as the credited reviewer:
`can_gs_usb_urb_anchor`, `seeded_pci_pwrctrl_endpoint_leak`, and
`seeded_spi_stm32_ospi_dma_leak`. They become `detect_bug` (detection is
still required, credit is not), dropping `expected_reviewer`,
`expected_prior_version`, and `expected_prior_message_ids`. The fourth
entry naming Sashiko, `usb_tcpm_role_sw_errptr`, is a
`suppress_false_positive` entry whose reviewer field is informational, so it
stays as is.

`src/benchmark.rs` changes:

- The suite loader rejects `"Sashiko"` as an `expected_reviewer` or
  `acceptable_attributions` reviewer, and the load error says why.
- The scorer treats an attribution with `is_credit() == false` as no credit:
  on a `detect_bug` entry it is neither "Wrong" nor counted as an unexpected
  attribution. This also covers old preserved runs.

`docs/benchmarking.md` drops the `"Sashiko"` reviewer option and documents
the scoring rule.

### 3.4 Unchanged pieces

- `attribution_line()` keeps the single form `As reported by <who> on
  <label>`.
- The `prior-feedback` audit, `prior_feedback_resolutions`, review-map
  lineage, and `found_by` behave as today for Sashiko-only findings.
- The "As reported by Sashiko on v1" examples in
  `designs/DESIGN_PREVIOUS_VERSION_FEEDBACK.md` and in the inline rules of
  `sashiko_patch_review.rs` are replaced with human examples.
- No migration. The `findings` columns keep recording
  `origin_kind = 'sashiko_review'` for linkage-only attributions, which keeps
  SQL analysis of Sashiko-to-Sashiko lineage possible.

## 4. Testing

Deterministic unit tests (`make test`):

- `prior_attribution.rs`: replace `test_sashiko_only_evidence_credits_sashiko`
  with a test asserting that Sashiko-only evidence yields a linkage-only
  attribution (`is_credit() == false`, no `attribution_line`, evidence
  kept); mixed evidence (`PF1` on v1 plus a human `HM` message on v2)
  credits the human on v2 and keeps both evidence records; bot evidence
  still yields a credit with `OriginKind::AutomatedBot`.
- `review_map.rs`: a still-unaddressed `PF*` resolution matched only by a
  linkage-only attribution keeps `v_n_outcome = confirmed_finding`, and
  `found_by()` stays `Both` for a fresh plus `PF*` finding.
- Report input builders: linkage-only `prior_attribution` is omitted.
- `benchmark.rs`: loading an entry with `expected_reviewer: "Sashiko"` fails
  with the new message; a linkage-only attribution on a `detect_bug` entry
  is not scored as "Wrong".

The similar-bug rule is prompt behavior and cannot be unit tested
deterministically. `benchmark_versioned.json` runs only on explicit human
request, per repository policy.

## 5. Implementation Plan

Each step is one self-contained commit:

1. `prior_context`, `prior_attribution`: `is_credit()`, earliest non-Sashiko
   credit selection, no `attribution_line` for linkage-only attributions,
   tests including the review-map invariants.
2. `workflows`, prompts: omit linkage-only attributions from report inputs
   and tighten the template rule.
3. `prior_attribution`: similar-bug sentence in rule 4.
4. `benchmark`: convert the three Sashiko-credited entries to `detect_bug`,
   reject `"Sashiko"` reviewers at load time, score linkage-only
   attributions as no credit, update `docs/benchmarking.md`.
5. `designs`, `sashiko_patch_review`: replace "As reported by Sashiko"
   examples.

## 6. Open Question

1. A prior Sashiko finding may itself carry a human attribution (a `PF` on v3
   crediting a v2 reviewer). Earlier-version threads are still in the
   context, so the stage can cite that reviewer directly; this design does
   not inherit credit through `PF` items. Is that sufficient?
