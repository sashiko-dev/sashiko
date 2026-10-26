use crate::db::Severity;
use crate::email_policy::{
    EmailPolicyConfig, PatchworkPolicy, PositiveReviewPolicy, ReplyTarget, extract_bare_email,
};
use std::collections::{HashMap, HashSet};

pub const KNOWN_MAILING_LIST_DOMAINS: &[&str] = &[
    "vger.kernel.org",
    "lists.linux.dev",
    "lists.infradead.org",
    "lists.freedesktop.org",
    "lists.osuosl.org",
    "kvack.org",
    "lists.open-mesh.org",
    "lists.sourceforge.net",
    "lists.oss.qualcomm.com",
    "lists.subsurface-divelog.org",
    "acpica.org",
    "lists.linaro.org",
    "lists.01.org",
    "lists.xenproject.org",
    "lists.ozlabs.org",
    "nongnu.org",
    "lists.nongnu.org",
];

/// Known public mailing-list addresses hosted on shared project domains (such as
/// `gcc.gnu.org` and `sourceware.org`) where personal developer accounts also
/// use the same domain.
pub const KNOWN_MAILING_LIST_ADDRESSES: &[&str] = &[
    "algol68@gcc.gnu.org",
    "cobol@gcc.gnu.org",
    "fortran@gcc.gnu.org",
    "gcc-bugs@gcc.gnu.org",
    "gcc-cvs@gcc.gnu.org",
    "gcc-help@gcc.gnu.org",
    "gcc-patches@gcc.gnu.org",
    "gcc-prs@gcc.gnu.org",
    "gcc-regression@gcc.gnu.org",
    "gcc-rust@gcc.gnu.org",
    "gcc-testresults@gcc.gnu.org",
    "gcc@gcc.gnu.org",
    "gccadmin@gcc.gnu.org",
    "java-patches@gcc.gnu.org",
    "jit@gcc.gnu.org",
    "libstdc++-cvs@gcc.gnu.org",
    "libstdc++@gcc.gnu.org",
    "binutils@sourceware.org",
    "cgen@sourceware.org",
    "cygwin-patches@sourceware.org",
    "dwz@sourceware.org",
    "elfutils-devel@sourceware.org",
    "gdb-patches@sourceware.org",
    "libabigail@sourceware.org",
    "libc-alpha@sourceware.org",
    "newlib@sourceware.org",
    "overseers@sourceware.org",
    "systemtap@sourceware.org",
];

/// Returns true if `bare` (`local@domain`) belongs to a dedicated mailing-list
/// domain or matches a known public mailing-list address on a shared domain.
pub fn is_known_mailing_list_bare(bare: &str) -> bool {
    if KNOWN_MAILING_LIST_ADDRESSES
        .iter()
        .any(|&addr| addr.eq_ignore_ascii_case(bare))
    {
        return true;
    }
    if let Some((_, domain)) = bare.split_once('@') {
        return KNOWN_MAILING_LIST_DOMAINS
            .iter()
            .any(|&d| d.eq_ignore_ascii_case(domain));
    }
    false
}

/// The largest number of individual To/Cc addresses a review is relayed to
/// under the "recipients" reply target; past it, none of them is, and the
/// lists, the author and the configured cc addresses still get the review.
pub const MAX_RELAYED_RECIPIENTS: usize = 128;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Mute,
    Send {
        to: Vec<String>,
        cc: Vec<String>,
        positive_review: PositiveReviewPolicy,
    },
}

pub struct EmailRouter {}

impl EmailRouter {
    /// Helper to merge two optional severities, returning the lowest (most inclusive).
    /// None represents "all severities" (lowest possible).
    fn merge_severity_opt(a: &Option<String>, b: &Option<String>) -> Option<String> {
        match (a, b) {
            (Some(sa), Some(sb)) => {
                let sev_a = Severity::from_str(sa);
                let sev_b = Severity::from_str(sb);
                let min_sev = std::cmp::min(sev_a, sev_b);
                Some(format!("{:?}", min_sev))
            }
            _ => None, // If either is None (all findings), the merged result is None
        }
    }

    /// Merge the configuration of policy `b` into `a`.
    /// Preserves the first non-empty token.
    fn merge_policies(a: &mut PatchworkPolicy, b: &PatchworkPolicy) {
        a.min_severity = Self::merge_severity_opt(&a.min_severity, &b.min_severity);

        let sev_a = Severity::from_str(&a.fail_severity);
        let sev_b = Severity::from_str(&b.fail_severity);
        let min_fail = std::cmp::min(sev_a, sev_b);
        a.fail_severity = format!("{:?}", min_fail);

        if a.token.is_none() && b.token.is_some() {
            a.token = b.token.clone();
        }
    }

    /// Compares addresses only: a display name is the sender's to choose, so
    /// one naming a list must not select that list's delivery policy.
    fn address_matches_list(incoming: &str, list_bare: &str) -> bool {
        extract_bare_email(incoming) == list_bare
    }

    /// Whether the patch mail carries more individual To/Cc addresses than
    /// MAX_RELAYED_RECIPIENTS. The sender picks them, so relaying them
    /// unbounded lets anyone who can post to a list make the bot mail whoever
    /// they name. The bot's own address is never mailed, and the author (when
    /// included by policy) and configured static CCs are mailed regardless of
    /// the "recipients" target, so none of those are counted against the cap.
    fn too_many_individuals(
        all_incoming: &[&String],
        configured_lists: &HashSet<String>,
        sashiko_bare: &str,
        author_bare: Option<&str>,
        static_cc: &[String],
    ) -> bool {
        let static_cc_bare: HashSet<String> =
            static_cc.iter().map(|c| extract_bare_email(c)).collect();
        let individuals: HashSet<String> = all_incoming
            .iter()
            .filter(|addr| !Self::is_mailing_list_address(addr, configured_lists))
            .map(|addr| extract_bare_email(addr))
            .filter(|bare| {
                !bare.is_empty()
                    && bare != sashiko_bare
                    && author_bare != Some(bare.as_str())
                    && !static_cc_bare.contains(bare)
            })
            .collect();
        if individuals.len() <= MAX_RELAYED_RECIPIENTS {
            return false;
        }
        tracing::warn!(
            "Not relaying to {} individual recipients (limit {})",
            individuals.len(),
            MAX_RELAYED_RECIPIENTS
        );
        true
    }

    fn is_mailing_list_address(addr: &str, configured_lists: &HashSet<String>) -> bool {
        let bare = extract_bare_email(addr);
        configured_lists.contains(&bare) || is_known_mailing_list_bare(&bare)
    }

    fn dedup_addresses(
        candidates: Vec<String>,
        sashiko_bare: &str,
        exclude_bare: Option<&HashSet<String>>,
    ) -> (Vec<String>, HashSet<String>) {
        let mut result: Vec<String> = Vec::new();
        let mut index_by_bare: HashMap<String, usize> = HashMap::new();
        let mut seen_bare: HashSet<String> = HashSet::new();

        for addr in candidates {
            let trimmed = addr.trim();
            if trimmed.is_empty() {
                continue;
            }
            let bare = extract_bare_email(trimmed);
            if bare.is_empty() {
                continue;
            }
            if !sashiko_bare.is_empty()
                && (bare == sashiko_bare || trimmed.to_ascii_lowercase().contains(sashiko_bare))
            {
                continue;
            }
            if let Some(excluded) = exclude_bare
                && excluded.contains(&bare)
            {
                continue;
            }

            if let Some(&idx) = index_by_bare.get(&bare) {
                if !result[idx].contains('<') && trimmed.contains('<') {
                    result[idx] = trimmed.to_string();
                }
            } else {
                index_by_bare.insert(bare.clone(), result.len());
                seen_bare.insert(bare);
                result.push(trimmed.to_string());
            }
        }

        (result, seen_bare)
    }

    pub fn resolve_patchwork(
        policy: &EmailPolicyConfig,
        incoming_to: &[String],
        incoming_cc: &[String],
    ) -> Vec<PatchworkPolicy> {
        let all_incoming: Vec<&String> = incoming_to.iter().chain(incoming_cc.iter()).collect();

        let mut matched_policies = Vec::new();

        for (list_email, list_policy) in &policy.lists {
            let list_bare = extract_bare_email(list_email);
            let matched = all_incoming
                .iter()
                .any(|incoming| Self::address_matches_list(incoming, &list_bare));
            if matched {
                matched_policies.push(list_policy.patchwork.clone());
            }
        }

        if matched_policies.is_empty() {
            matched_policies.push(policy.defaults.patchwork.clone());
        }

        // Filter only enabled policies
        let enabled_policies: Vec<PatchworkPolicy> =
            matched_policies.into_iter().filter(|p| p.enabled).collect();

        let mut api_targets: HashMap<String, PatchworkPolicy> = HashMap::new();
        let mut email_targets: HashMap<String, PatchworkPolicy> = HashMap::new();

        for p in enabled_policies {
            // 1. Process API target if present
            if let Some(ref api_url) = p.api_url {
                let mut api_only_policy = p.clone();
                api_only_policy.email = None; // Strip email for API-only delivery

                if let Some(existing) = api_targets.get_mut(api_url) {
                    Self::merge_policies(existing, &api_only_policy);
                } else {
                    api_targets.insert(api_url.clone(), api_only_policy);
                }
            }

            // 2. Process Email target if present
            if let Some(ref email_addr) = p.email {
                let mut email_only_policy = p.clone();
                email_only_policy.api_url = None; // Strip API for Email-only delivery
                email_only_policy.token = None;

                if let Some(existing) = email_targets.get_mut(email_addr) {
                    Self::merge_policies(existing, &email_only_policy);
                } else {
                    email_targets.insert(email_addr.clone(), email_only_policy);
                }
            }
        }

        // Combine both merged target lists
        let mut final_policies = Vec::new();
        for p in api_targets.into_values() {
            final_policies.push(p);
        }
        for p in email_targets.into_values() {
            final_policies.push(p);
        }

        final_policies
    }

    pub fn resolve_recipients(
        policy: &EmailPolicyConfig,
        incoming_to: &[String],
        incoming_cc: &[String],
        patch_author: &str,
        sashiko_address: &str,
    ) -> Action {
        let sashiko_bare = extract_bare_email(sashiko_address);
        let author_bare = extract_bare_email(patch_author);
        if Self::is_ignored_author(policy, patch_author)
            || (!sashiko_bare.is_empty()
                && !author_bare.is_empty()
                && (author_bare == sashiko_bare
                    || patch_author.to_ascii_lowercase().contains(&sashiko_bare)))
        {
            return Action::Mute;
        }

        let all_incoming: Vec<&String> = incoming_to.iter().chain(incoming_cc.iter()).collect();

        let configured_lists: HashSet<String> =
            policy.lists.keys().map(|k| extract_bare_email(k)).collect();

        let mut matched_lists = Vec::new();
        for (list_email, list_policy) in &policy.lists {
            let list_bare = extract_bare_email(list_email);
            if all_incoming
                .iter()
                .any(|incoming| Self::address_matches_list(incoming, &list_bare))
            {
                matched_lists.push((list_bare, list_policy));
            }
        }

        let mut mute_all = false;
        let mut include_author = false;
        let mut include_recipients = false;
        let mut include_all_lists_from_defaults = false;
        let mut allowed_lists: HashSet<String> = HashSet::new();
        let mut positive_review = PositiveReviewPolicy::None;
        let mut static_cc = Vec::new();

        if matched_lists.is_empty() {
            if policy.defaults.mute_all {
                return Action::Mute;
            }
            if policy.defaults.reply_to.contains(&ReplyTarget::Author) {
                include_author = true;
            }
            if policy.defaults.reply_to.contains(&ReplyTarget::List) {
                include_all_lists_from_defaults = true;
            }
            if policy.defaults.reply_to.contains(&ReplyTarget::Recipients) {
                include_recipients = true;
            }
            positive_review = policy.defaults.positive_review;
        } else {
            // Sort matched lists by bare email for deterministic CC ordering.
            matched_lists.sort_by(|a, b| a.0.cmp(&b.0));

            for (list_bare, list_policy) in matched_lists {
                if list_policy.mute_all.unwrap_or(policy.defaults.mute_all) {
                    mute_all = true;
                }
                let reply_to = list_policy
                    .reply_to
                    .as_deref()
                    .unwrap_or(&policy.defaults.reply_to);
                if reply_to.contains(&ReplyTarget::Author) {
                    include_author = true;
                }
                if reply_to.contains(&ReplyTarget::List) {
                    allowed_lists.insert(list_bare);
                }
                if reply_to.contains(&ReplyTarget::Recipients) {
                    include_recipients = true;
                }
                let list_pos = list_policy
                    .positive_review
                    .unwrap_or(policy.defaults.positive_review);
                if list_pos > positive_review {
                    positive_review = list_pos;
                }
                for cr in &list_policy.cc {
                    static_cc.push(cr.clone());
                }
            }
        }

        if mute_all {
            return Action::Mute;
        }

        for cr in &policy.defaults.cc {
            static_cc.push(cr.clone());
        }

        let mut raw_to = Vec::new();
        let mut raw_cc = Vec::new();

        if include_author && !patch_author.trim().is_empty() {
            raw_to.push(patch_author.to_string());
        }

        if include_recipients
            && Self::too_many_individuals(
                &all_incoming,
                &configured_lists,
                &sashiko_bare,
                include_author.then_some(author_bare.as_str()),
                &static_cc,
            )
        {
            include_recipients = false;
        }

        for addr in incoming_to {
            let is_ml = Self::is_mailing_list_address(addr, &configured_lists);
            if is_ml {
                let bare = extract_bare_email(addr);
                if include_all_lists_from_defaults || allowed_lists.contains(&bare) {
                    raw_to.push(addr.clone());
                }
            } else if include_recipients {
                raw_to.push(addr.clone());
            }
        }

        for addr in incoming_cc {
            let is_ml = Self::is_mailing_list_address(addr, &configured_lists);
            if is_ml {
                let bare = extract_bare_email(addr);
                if include_all_lists_from_defaults || allowed_lists.contains(&bare) {
                    raw_cc.push(addr.clone());
                }
            } else if include_recipients {
                raw_cc.push(addr.clone());
            }
        }

        for cr in static_cc {
            raw_cc.push(cr);
        }

        let (final_to, to_bare_set) = Self::dedup_addresses(raw_to, &sashiko_bare, None);
        let (final_cc, _) = Self::dedup_addresses(raw_cc, &sashiko_bare, Some(&to_bare_set));

        if final_to.is_empty() && final_cc.is_empty() {
            return Action::Mute;
        }

        Action::Send {
            to: final_to,
            cc: final_cc,
            positive_review,
        }
    }

    pub fn resolve_positive_recipients(
        policy: &EmailPolicyConfig,
        incoming_to: &[String],
        incoming_cc: &[String],
        patch_author: &str,
        sashiko_address: &str,
    ) -> Option<(Vec<String>, Vec<String>)> {
        let Action::Send {
            to,
            cc,
            positive_review,
        } = Self::resolve_recipients(
            policy,
            incoming_to,
            incoming_cc,
            patch_author,
            sashiko_address,
        )
        else {
            return None;
        };

        let sashiko_bare = extract_bare_email(sashiko_address);
        match positive_review {
            PositiveReviewPolicy::None => None,
            PositiveReviewPolicy::Author => {
                let author_trimmed = patch_author.trim();
                if author_trimmed.is_empty() {
                    return None;
                }
                let (pos_to, _) =
                    Self::dedup_addresses(vec![author_trimmed.to_string()], &sashiko_bare, None);
                if pos_to.is_empty() {
                    None
                } else {
                    Some((pos_to, Vec::new()))
                }
            }
            PositiveReviewPolicy::All => {
                let all_incoming: Vec<&String> =
                    incoming_to.iter().chain(incoming_cc.iter()).collect();
                let configured_lists: HashSet<String> =
                    policy.lists.keys().map(|k| extract_bare_email(k)).collect();

                let mut matched_lists = Vec::new();
                for (list_email, list_policy) in &policy.lists {
                    let list_bare = extract_bare_email(list_email);
                    if all_incoming
                        .iter()
                        .any(|incoming| Self::address_matches_list(incoming, &list_bare))
                    {
                        matched_lists.push((list_bare, list_policy));
                    }
                }

                if matched_lists.is_empty() {
                    return Some((to, cc));
                }

                matched_lists.sort_by(|a, b| a.0.cmp(&b.0));

                let mut include_author = false;
                let mut include_recipients = false;
                let mut allowed_lists: HashSet<String> = HashSet::new();
                let mut static_cc = Vec::new();

                for (list_bare, list_policy) in matched_lists {
                    let list_pos = list_policy
                        .positive_review
                        .unwrap_or(policy.defaults.positive_review);
                    let reply_to = list_policy
                        .reply_to
                        .as_deref()
                        .unwrap_or(&policy.defaults.reply_to);

                    if list_pos == PositiveReviewPolicy::All {
                        if reply_to.contains(&ReplyTarget::Author) {
                            include_author = true;
                        }
                        if reply_to.contains(&ReplyTarget::List) {
                            allowed_lists.insert(list_bare);
                        }
                        if reply_to.contains(&ReplyTarget::Recipients) {
                            include_recipients = true;
                        }
                        for cr in &list_policy.cc {
                            static_cc.push(cr.clone());
                        }
                    } else if list_pos == PositiveReviewPolicy::Author
                        && reply_to.contains(&ReplyTarget::Author)
                    {
                        include_author = true;
                    }
                }

                for cr in &policy.defaults.cc {
                    static_cc.push(cr.clone());
                }

                let author_bare = extract_bare_email(patch_author);
                if include_recipients
                    && Self::too_many_individuals(
                        &all_incoming,
                        &configured_lists,
                        &sashiko_bare,
                        include_author.then_some(author_bare.as_str()),
                        &static_cc,
                    )
                {
                    include_recipients = false;
                }

                let mut raw_to = Vec::new();
                let mut raw_cc = Vec::new();

                if include_author && !patch_author.trim().is_empty() {
                    raw_to.push(patch_author.to_string());
                }

                for addr in incoming_to {
                    let is_ml = Self::is_mailing_list_address(addr, &configured_lists);
                    if is_ml {
                        let bare = extract_bare_email(addr);
                        if allowed_lists.contains(&bare) {
                            raw_to.push(addr.clone());
                        }
                    } else if include_recipients {
                        raw_to.push(addr.clone());
                    }
                }

                for addr in incoming_cc {
                    let is_ml = Self::is_mailing_list_address(addr, &configured_lists);
                    if is_ml {
                        let bare = extract_bare_email(addr);
                        if allowed_lists.contains(&bare) {
                            raw_cc.push(addr.clone());
                        }
                    } else if include_recipients {
                        raw_cc.push(addr.clone());
                    }
                }

                for cr in static_cc {
                    raw_cc.push(cr);
                }

                let (final_to, to_bare_set) = Self::dedup_addresses(raw_to, &sashiko_bare, None);
                let (final_cc, _) =
                    Self::dedup_addresses(raw_cc, &sashiko_bare, Some(&to_bare_set));

                if final_to.is_empty() && final_cc.is_empty() {
                    None
                } else {
                    Some((final_to, final_cc))
                }
            }
        }
    }

    pub fn is_ignored_author(policy: &EmailPolicyConfig, author_email: &str) -> bool {
        let author_lower = author_email.to_lowercase();

        if policy
            .defaults
            .ignored_emails
            .iter()
            .any(|e| author_lower.contains(&e.to_lowercase()))
        {
            return true;
        }

        for p in policy.lists.values() {
            if p.ignored_emails
                .iter()
                .any(|e| author_lower.contains(&e.to_lowercase()))
            {
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email_policy::{DefaultListPolicy, MailingListPolicy};
    use std::collections::HashMap;

    fn build_test_policy() -> EmailPolicyConfig {
        let mut lists = HashMap::new();
        lists.insert(
            "linux-mm@kvack.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                mute_all: Some(false),
                cc: vec!["mm-bot@test.com".to_string()],
                ..Default::default()
            },
        );
        lists.insert(
            "bpf@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![ReplyTarget::Author]),
                mute_all: Some(false),
                ..Default::default()
            },
        );
        lists.insert(
            "netdev@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                mute_all: Some(true),
                ..Default::default()
            },
        );
        EmailPolicyConfig {
            defaults: DefaultListPolicy {
                reply_to: vec![ReplyTarget::Author, ReplyTarget::Recipients],
                mute_all: false,
                ..Default::default()
            },
            lists,
        }
    }

    #[test]
    fn test_empty_recipients_mute() {
        let policy = build_test_policy();
        let action = EmailRouter::resolve_recipients(
            &policy,
            &[],
            &[],
            "", // no patch author
            "sashiko@sashiko.dev",
        );

        match action {
            Action::Mute => {}
            _ => panic!("Expected Mute when no recipients"),
        }
    }

    #[test]
    fn test_mute_all() {
        let policy = build_test_policy();
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["netdev@vger.kernel.org".to_string()],
            &[],
            "author@test.com",
            "bot@sashiko.dev",
        );
        assert!(matches!(action, Action::Mute));
    }

    #[test]
    fn test_public_reply() {
        let policy = build_test_policy();
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["linux-mm@kvack.org".to_string()],
            &["maintainer@test.com".to_string()],
            "author@test.com",
            "bot@sashiko.dev",
        );

        match action {
            Action::Send { to, cc, .. } => {
                assert!(to.contains(&"author@test.com".to_string()));
                assert!(to.contains(&"linux-mm@kvack.org".to_string()));
                assert!(cc.contains(&"maintainer@test.com".to_string()));
                assert!(cc.contains(&"mm-bot@test.com".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_cross_post_no_veto_per_list_control() {
        let policy = build_test_policy();
        // Patch sent to both linux-mm (reply_to = ["author", "list", "recipients"])
        // and bpf (reply_to = ["author"]).
        // Each list controls itself: linux-mm is kept, bpf is excluded,
        // and maintainer is kept because linux-mm includes "recipients".
        let action = EmailRouter::resolve_recipients(
            &policy,
            &[
                "linux-mm@kvack.org".to_string(),
                "bpf@vger.kernel.org".to_string(),
            ],
            &["maintainer@test.com".to_string()],
            "author@test.com",
            "bot@sashiko.dev",
        );

        match action {
            Action::Send { to, cc, .. } => {
                assert!(to.contains(&"author@test.com".to_string()));
                assert!(to.contains(&"linux-mm@kvack.org".to_string()));
                assert!(!to.contains(&"bpf@vger.kernel.org".to_string()));
                assert!(!cc.contains(&"bpf@vger.kernel.org".to_string()));
                assert!(cc.contains(&"maintainer@test.com".to_string()));
                assert!(cc.contains(&"mm-bot@test.com".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_recipients_excludes_unconfigured_mailing_lists() {
        let policy = build_test_policy();
        // linux-kernel@vger.kernel.org is not in policy.lists, but @vger.kernel.org
        // is a known mailing list domain, so `recipients` on linux-mm must not CC LKML.
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["linux-mm@kvack.org".to_string()],
            &[
                "linux-kernel@vger.kernel.org".to_string(),
                "maintainer@test.com".to_string(),
            ],
            "author@test.com",
            "bot@sashiko.dev",
        );

        match action {
            Action::Send { to, cc, .. } => {
                assert!(to.contains(&"linux-mm@kvack.org".to_string()));
                assert!(cc.contains(&"maintainer@test.com".to_string()));
                assert!(!cc.contains(&"linux-kernel@vger.kernel.org".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_email_aware_deduplication_prefers_display_name() {
        let mut lists = HashMap::new();
        lists.insert(
            "linux-kbuild@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                cc: vec!["nathan@kernel.org".to_string()],
                ..Default::default()
            },
        );
        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        };

        let action = EmailRouter::resolve_recipients(
            &policy,
            &["linux-kbuild@vger.kernel.org".to_string()],
            &[
                "Nathan Chancellor <nathan@kernel.org>".to_string(),
                "author@test.com".to_string(),
            ],
            "Patch Author <author@test.com>",
            "Sashiko Bot <bot@sashiko.dev>",
        );

        match action {
            Action::Send { to, cc, .. } => {
                assert_eq!(
                    to,
                    vec![
                        "Patch Author <author@test.com>".to_string(),
                        "linux-kbuild@vger.kernel.org".to_string(),
                    ]
                );
                // nathan@kernel.org and Nathan Chancellor <nathan@kernel.org> deduplicate
                // to the formatted display-name version, and author@test.com is excluded
                // from Cc because it is already in To.
                assert_eq!(
                    cc,
                    vec!["Nathan Chancellor <nathan@kernel.org>".to_string()]
                );
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_author_only_delivery() {
        let mut lists = HashMap::new();
        lists.insert(
            "intel-xe@lists.freedesktop.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![ReplyTarget::Author]),
                ..Default::default()
            },
        );
        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        };
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["intel-xe@lists.freedesktop.org".to_string()],
            &["maintainer@test.com".to_string()],
            "author@test.com",
            "bot@sashiko.dev",
        );
        match action {
            Action::Send { to, cc, .. } => {
                assert_eq!(to, vec!["author@test.com".to_string()]);
                assert!(cc.is_empty(), "non-list individuals should be dropped");
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_display_name_does_not_select_list_policy() {
        let mut lists = HashMap::new();
        lists.insert(
            "quiet@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![ReplyTarget::List]),
                ..Default::default()
            },
        );
        lists.insert(
            "linux-kbuild@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![ReplyTarget::Author, ReplyTarget::Recipients]),
                ..Default::default()
            },
        );
        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        };
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["quiet@vger.kernel.org".to_string()],
            &["\"linux-kbuild@vger.kernel.org\" <victim@example.com>".to_string()],
            "victim@example.com",
            "bot@sashiko.dev",
        );
        assert_eq!(
            action,
            Action::Send {
                to: vec!["quiet@vger.kernel.org".to_string()],
                cc: vec![],
                positive_review: PositiveReviewPolicy::None,
            }
        );
    }

    #[test]
    fn test_relayed_recipients_are_capped() {
        let mut lists = HashMap::new();
        lists.insert(
            "linux-kbuild@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                ..Default::default()
            },
        );
        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        };
        let resolve = |n: usize| {
            let cc: Vec<String> = (0..n).map(|i| format!("cc{i}@example.com")).collect();
            match EmailRouter::resolve_recipients(
                &policy,
                &["linux-kbuild@vger.kernel.org".to_string()],
                &cc,
                "author@example.com",
                "bot@sashiko.dev",
            ) {
                Action::Send { to, cc, .. } => (to, cc),
                Action::Mute => panic!("Should not mute"),
            }
        };

        let (_, cc) = resolve(MAX_RELAYED_RECIPIENTS);
        assert_eq!(cc.len(), MAX_RELAYED_RECIPIENTS);
        let (_, cc) = resolve(MAX_RELAYED_RECIPIENTS + 1);
        assert!(cc.is_empty());

        // The bot's own address, the author and configured static CCs are not
        // relayed via "recipients", so they take no slot against the cap.
        let mut lists_with_cc = HashMap::new();
        lists_with_cc.insert(
            "linux-kbuild@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                cc: vec!["maintainer@kernel.org".to_string()],
                ..Default::default()
            },
        );
        let policy_with_cc = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists: lists_with_cc,
        };
        let mut cc: Vec<String> = (0..MAX_RELAYED_RECIPIENTS)
            .map(|i| format!("cc{i}@example.com"))
            .collect();
        cc.push("bot@sashiko.dev".to_string());
        cc.push("Patch Author <author@example.com>".to_string());
        cc.push("Maintainer <maintainer@kernel.org>".to_string());
        match EmailRouter::resolve_recipients(
            &policy_with_cc,
            &["linux-kbuild@vger.kernel.org".to_string()],
            &cc,
            "author@example.com",
            "bot@sashiko.dev",
        ) {
            Action::Send { cc, .. } => assert_eq!(cc.len(), MAX_RELAYED_RECIPIENTS + 1),
            Action::Mute => panic!("Should not mute"),
        }

        let (to, cc) = resolve(200);
        assert_eq!(
            to,
            vec![
                "author@example.com".to_string(),
                "linux-kbuild@vger.kernel.org".to_string()
            ]
        );
        assert!(cc.is_empty());
    }

    #[test]
    fn test_positive_review_recipients_are_capped() {
        let mut lists = HashMap::new();
        lists.insert(
            "linux-kbuild@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![ReplyTarget::List, ReplyTarget::Recipients]),
                positive_review: Some(PositiveReviewPolicy::All),
                ..Default::default()
            },
        );
        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        };
        let cc: Vec<String> = (0..200).map(|i| format!("cc{i}@example.com")).collect();
        let (_, pos_cc) = EmailRouter::resolve_positive_recipients(
            &policy,
            &["linux-kbuild@vger.kernel.org".to_string()],
            &cc,
            "author@example.com",
            "bot@sashiko.dev",
        )
        .expect("a positive review is sent");
        assert!(pos_cc.is_empty());
    }

    #[test]
    fn test_defaults() {
        let policy = build_test_policy();
        // Unknown list -> defaults apply (reply_to = ["author", "recipients"])
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["unknown-list@vger.kernel.org".to_string()],
            &["maintainer@test.com".to_string()],
            "author@test.com",
            "bot@sashiko.dev",
        );

        match action {
            Action::Send { to, cc, .. } => {
                assert!(to.contains(&"author@test.com".to_string()));
                assert!(!to.contains(&"unknown-list@vger.kernel.org".to_string()));
                assert!(cc.contains(&"maintainer@test.com".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_sashiko_stripped() {
        let policy = build_test_policy();
        let action = EmailRouter::resolve_recipients(
            &policy,
            &[
                "linux-mm@kvack.org".to_string(),
                "bot@sashiko.dev".to_string(),
            ],
            &["bot@sashiko.dev".to_string()],
            "author@test.com",
            "bot@sashiko.dev",
        );

        match action {
            Action::Send { to, cc, .. } => {
                assert!(!to.contains(&"bot@sashiko.dev".to_string()));
                assert!(!cc.contains(&"bot@sashiko.dev".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_positive_review_policy() {
        let mut policy = build_test_policy();

        // Test 1: Defaults has PositiveReviewPolicy::All, and we fallback to defaults
        policy.defaults.positive_review = PositiveReviewPolicy::All;
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["unknown-list@vger.kernel.org".to_string()],
            &[],
            "author@test.com",
            "bot@sashiko.dev",
        );
        match action {
            Action::Send {
                positive_review, ..
            } => {
                assert_eq!(positive_review, PositiveReviewPolicy::All);
            }
            _ => panic!("Expected Send"),
        }

        // Test 2: List has PositiveReviewPolicy::Author
        policy.defaults.positive_review = PositiveReviewPolicy::None;
        if let Some(sub) = policy.lists.get_mut("linux-mm@kvack.org") {
            sub.positive_review = Some(PositiveReviewPolicy::Author);
        }
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["linux-mm@kvack.org".to_string()],
            &[],
            "author@test.com",
            "bot@sashiko.dev",
        );
        match action {
            Action::Send {
                positive_review, ..
            } => {
                assert_eq!(positive_review, PositiveReviewPolicy::Author);
            }
            _ => panic!("Expected Send"),
        }

        // Test 3: List has PositiveReviewPolicy::None, defaults has All (list overrides)
        policy.defaults.positive_review = PositiveReviewPolicy::All;
        if let Some(sub) = policy.lists.get_mut("linux-mm@kvack.org") {
            sub.positive_review = Some(PositiveReviewPolicy::None);
        }
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["linux-mm@kvack.org".to_string()],
            &[],
            "author@test.com",
            "bot@sashiko.dev",
        );
        match action {
            Action::Send {
                positive_review, ..
            } => {
                assert_eq!(positive_review, PositiveReviewPolicy::None);
            }
            _ => panic!("Expected Send"),
        }
    }

    #[test]
    fn test_resolve_patchwork_deduplication_and_merging() {
        let mut lists = HashMap::new();

        // List 1: mm - API and Email. Strict fail_severity (High), lenient min_severity (Medium)
        lists.insert(
            "linux-mm@kvack.org".to_string(),
            MailingListPolicy {
                patchwork: PatchworkPolicy {
                    enabled: true,
                    api_url: Some("https://patchwork.kernel.org/api".to_string()),
                    token: Some("token_mm".to_string()),
                    email: Some("notify@kernel.org".to_string()),
                    min_severity: Some("Medium".to_string()),
                    fail_severity: "High".to_string(),
                },
                ..Default::default()
            },
        );

        // List 2: bpf - API only. Lenient fail_severity (Critical), strict min_severity (High)
        lists.insert(
            "bpf@vger.kernel.org".to_string(),
            MailingListPolicy {
                patchwork: PatchworkPolicy {
                    enabled: true,
                    api_url: Some("https://patchwork.kernel.org/api".to_string()),
                    token: Some("token_bpf".to_string()),
                    email: None,
                    min_severity: Some("High".to_string()),
                    fail_severity: "Critical".to_string(),
                },
                ..Default::default()
            },
        );

        // List 3: net - Email only. Overlaps email with mm, but has min_severity = None (all / Low)
        lists.insert(
            "netdev@vger.kernel.org".to_string(),
            MailingListPolicy {
                patchwork: PatchworkPolicy {
                    enabled: true,
                    api_url: None,
                    token: None,
                    email: Some("notify@kernel.org".to_string()),
                    min_severity: None, // most inclusive
                    fail_severity: "High".to_string(),
                },
                ..Default::default()
            },
        );

        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        };

        // Resolve patchwork for a patch sent to mm, bpf, and net
        let results = EmailRouter::resolve_patchwork(
            &policy,
            &[
                "linux-mm@kvack.org".to_string(),
                "bpf@vger.kernel.org".to_string(),
                "netdev@vger.kernel.org".to_string(),
            ],
            &[],
        );

        // We expect exactly 2 resolved policies:
        // 1. One API policy for "https://patchwork.kernel.org/api" (merged from mm and bpf)
        // 2. One Email policy for "notify@kernel.org" (merged from mm and net)
        assert_eq!(results.len(), 2);

        let api_policy = results
            .iter()
            .find(|p| p.api_url.is_some())
            .expect("Expected an API policy");
        let email_policy = results
            .iter()
            .find(|p| p.email.is_some())
            .expect("Expected an Email policy");

        // Verify API policy details:
        assert_eq!(
            api_policy.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api")
        );
        assert_eq!(api_policy.email, None); // Split to API-only
        // min_severity: min(Medium, High) -> Medium
        assert_eq!(api_policy.min_severity.as_deref(), Some("Medium"));
        // fail_severity: min(High, Critical) -> High
        assert_eq!(api_policy.fail_severity, "High");
        // token: should pick first non-empty (token_mm or token_bpf)
        assert!(api_policy.token.is_some());

        // Verify Email policy details:
        assert_eq!(email_policy.email.as_deref(), Some("notify@kernel.org"));
        assert_eq!(email_policy.api_url, None); // Split to Email-only
        // min_severity: min(Medium, None) -> None (most inclusive)
        assert_eq!(email_policy.min_severity, None);
        // fail_severity: min(High, High) -> High
        assert_eq!(email_policy.fail_severity, "High");
    }

    #[test]
    fn test_cross_posting_positive_review_all_vs_none() {
        let mut lists = HashMap::new();
        lists.insert(
            "linux-kbuild@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                positive_review: Some(PositiveReviewPolicy::None),
                ..Default::default()
            },
        );
        lists.insert(
            "bpf@vger.kernel.org".to_string(),
            MailingListPolicy {
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                positive_review: Some(PositiveReviewPolicy::All),
                ..Default::default()
            },
        );

        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        };

        let incoming_to = vec![
            "linux-kbuild@vger.kernel.org".to_string(),
            "bpf@vger.kernel.org".to_string(),
        ];
        let incoming_cc = vec!["reviewer@example.com".to_string()];

        // Positive review must only include bpf@vger.kernel.org (which opted into All)
        // and must NOT include linux-kbuild@vger.kernel.org (which set positive_review = "none").
        let (pos_to, pos_cc) = EmailRouter::resolve_positive_recipients(
            &policy,
            &incoming_to,
            &incoming_cc,
            "author@example.com",
            "sashiko@example.com",
        )
        .expect("Expected positive review recipients");

        assert_eq!(
            pos_to,
            vec![
                "author@example.com".to_string(),
                "bpf@vger.kernel.org".to_string()
            ]
        );
        assert_eq!(pos_cc, vec!["reviewer@example.com".to_string()]);
    }

    #[test]
    fn test_resolve_recipients_mutes_ignored_author_and_self() {
        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy {
                mute_all: false,
                reply_to: vec![ReplyTarget::Author, ReplyTarget::List],
                ignored_emails: vec!["lkp@intel.com".to_string()],
                ..Default::default()
            },
            lists: HashMap::new(),
        };

        let to = vec!["linux-kbuild@vger.kernel.org".to_string()];

        assert_eq!(
            EmailRouter::resolve_recipients(
                &policy,
                &to,
                &[],
                "kernel test robot <lkp@intel.com>",
                "sashiko@example.com"
            ),
            Action::Mute
        );

        assert_eq!(
            EmailRouter::resolve_recipients(
                &policy,
                &to,
                &[],
                "Sashiko Bot <sashiko@example.com>",
                "sashiko@example.com"
            ),
            Action::Mute
        );
    }

    #[test]
    fn test_gcc_shared_domain_lists_vs_personal_accounts() {
        assert!(is_known_mailing_list_bare("gcc-patches@gcc.gnu.org"));
        assert!(is_known_mailing_list_bare("fortran@gcc.gnu.org"));
        assert!(is_known_mailing_list_bare("libstdc++@gcc.gnu.org"));
        assert!(is_known_mailing_list_bare("libc-alpha@sourceware.org"));
        assert!(!is_known_mailing_list_bare("jakub@gcc.gnu.org"));
        assert!(!is_known_mailing_list_bare("rguenth@gcc.gnu.org"));
        assert!(!is_known_mailing_list_bare("maintainer@sourceware.org"));

        let mut lists = HashMap::new();
        lists.insert(
            "gcc-patches@gcc.gnu.org".to_string(),
            MailingListPolicy {
                mute_all: Some(false),
                reply_to: Some(vec![
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients,
                ]),
                ..Default::default()
            },
        );
        let policy = EmailPolicyConfig {
            defaults: DefaultListPolicy {
                mute_all: true,
                ..Default::default()
            },
            lists,
        };
        let to = vec![
            "gcc-patches@gcc.gnu.org".to_string(),
            "fortran@gcc.gnu.org".to_string(),
        ];
        let cc = vec!["Jakub Jelinek <jakub@gcc.gnu.org>".to_string()];
        let action = EmailRouter::resolve_recipients(
            &policy,
            &to,
            &cc,
            "Author <author@example.com>",
            "sashiko@sashiko.dev",
        );
        match action {
            Action::Send { to, cc, .. } => {
                assert_eq!(
                    to,
                    vec![
                        "Author <author@example.com>".to_string(),
                        "gcc-patches@gcc.gnu.org".to_string(),
                    ]
                );
                assert_eq!(cc, vec!["Jakub Jelinek <jakub@gcc.gnu.org>".to_string()]);
            }
            Action::Mute => panic!("expected Action::Send"),
        }
    }
}
