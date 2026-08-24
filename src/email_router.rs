use crate::db::Severity;
use crate::email_policy::{EmailPolicyConfig, PatchworkPolicy};
use std::collections::HashMap;
use std::collections::HashSet;

pub enum Action {
    Mute,
    Send {
        to: Vec<String>,
        cc: Vec<String>,
        send_positive_review: bool,
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

    pub fn resolve_patchwork(
        policy: &EmailPolicyConfig,
        incoming_to: &[String],
        incoming_cc: &[String],
    ) -> Vec<PatchworkPolicy> {
        let mut all_incoming: Vec<&String> = Vec::new();
        for addr in incoming_to {
            all_incoming.push(addr);
        }
        for addr in incoming_cc {
            all_incoming.push(addr);
        }

        let mut matched_policies = Vec::new();

        for sub_policy in policy.subsystems.values() {
            let mut matched = false;
            for list in &sub_policy.lists {
                for incoming in &all_incoming {
                    if incoming.to_lowercase().contains(&list.to_lowercase()) {
                        matched = true;
                    }
                }
            }
            if matched {
                matched_policies.push(sub_policy.patchwork.clone());
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
        let mut all_incoming: Vec<&String> = Vec::new();
        for addr in incoming_to {
            all_incoming.push(addr);
        }
        for addr in incoming_cc {
            all_incoming.push(addr);
        }

        let mut active_policies = Vec::new();
        let mut known_mailing_lists = HashSet::new();

        for sub_policy in policy.subsystems.values() {
            let mut matched = false;
            for list in &sub_policy.lists {
                known_mailing_lists.insert(list.to_lowercase());
                for incoming in &all_incoming {
                    if incoming.to_lowercase().contains(&list.to_lowercase()) {
                        matched = true;
                    }
                }
            }
            if matched {
                active_policies.push(sub_policy);
            }
        }

        if active_policies.is_empty() {
            active_policies.push(&policy.defaults);
        }

        let mut mute_all = false;
        let mut is_private = false;
        let mut reply_to_author = false;
        let mut cc_individuals = false;
        let mut send_positive_review = false;
        let mut cc = Vec::new();

        for p in active_policies {
            if p.mute_all {
                mute_all = true;
            }
            if !p.reply_all {
                is_private = true;
            }
            if p.reply_to_author {
                reply_to_author = true;
            }
            if p.cc_individuals {
                cc_individuals = true;
            }
            if p.send_positive_review {
                send_positive_review = true;
            }
            for cr in &p.cc {
                cc.push(cr.clone());
            }
        }

        // Always append defaults.cc so users can define a global CC
        for cr in &policy.defaults.cc {
            cc.push(cr.clone());
        }

        if mute_all {
            return Action::Mute;
        }

        let mut final_to = HashSet::new();
        let mut final_cc = HashSet::new();

        if reply_to_author && !patch_author.is_empty() {
            final_to.insert(patch_author.to_string());
        }

        for cr in cc {
            final_cc.insert(cr);
        }

        // Add original non-mailing-list recipients if cc_individuals is true
        // Or if it's public, add everyone (mailing lists included, unless it's private)
        for addr in incoming_to {
            let addr_lower = addr.to_lowercase();
            let is_mailing_list = known_mailing_lists.iter().any(|ml| addr_lower.contains(ml));

            if (!is_private) || (cc_individuals && !is_mailing_list) {
                final_to.insert(addr.to_string());
            }
        }

        for addr in incoming_cc {
            let addr_lower = addr.to_lowercase();
            let is_mailing_list = known_mailing_lists.iter().any(|ml| addr_lower.contains(ml));

            if (!is_private) || (cc_individuals && !is_mailing_list) {
                final_cc.insert(addr.to_string());
            }
        }

        // Sanitize
        let sashiko_lower = sashiko_address.to_lowercase();
        final_to.retain(|a| !a.to_lowercase().contains(&sashiko_lower));
        final_cc.retain(|a| !a.to_lowercase().contains(&sashiko_lower) && !final_to.contains(a));

        if final_to.is_empty() && final_cc.is_empty() {
            return Action::Mute;
        }

        Action::Send {
            to: final_to.into_iter().collect(),
            cc: final_cc.into_iter().collect(),
            send_positive_review,
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

        for p in policy.subsystems.values() {
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
    use crate::email_policy::SubsystemPolicy;
    use std::collections::HashMap;

    fn build_test_policy() -> EmailPolicyConfig {
        let mut subsystems = HashMap::new();
        subsystems.insert(
            "mm".to_string(),
            SubsystemPolicy {
                lists: vec!["linux-mm@kvack.org".to_string()],
                reply_all: true,
                reply_to_author: true,
                cc_individuals: true,
                mute_all: false,
                cc: vec!["mm-bot@test.com".to_string()],
                ignored_emails: vec![],
                patchwork: Default::default(),
                embargo_hours: None,
                ..Default::default()
            },
        );
        subsystems.insert(
            "bpf".to_string(),
            SubsystemPolicy {
                lists: vec!["bpf@vger.kernel.org".to_string()],
                reply_all: false,
                reply_to_author: true,
                cc_individuals: false,
                mute_all: false,
                cc: vec![],
                ignored_emails: vec![],
                patchwork: Default::default(),
                embargo_hours: None,
                ..Default::default()
            },
        );
        subsystems.insert(
            "net".to_string(),
            SubsystemPolicy {
                lists: vec!["netdev@vger.kernel.org".to_string()],
                reply_all: true,
                reply_to_author: true,
                cc_individuals: true,
                mute_all: true,
                cc: vec![],
                ignored_emails: vec![],
                patchwork: Default::default(),
                embargo_hours: None,
                ..Default::default()
            },
        );
        EmailPolicyConfig {
            defaults: SubsystemPolicy {
                lists: vec![],
                reply_all: false,
                reply_to_author: true,
                cc_individuals: true,
                mute_all: false,
                cc: vec![],
                ignored_emails: vec![],
                patchwork: Default::default(),
                embargo_hours: None,
                ..Default::default()
            },
            subsystems,
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
    fn test_downgrade_to_private() {
        let policy = build_test_policy();
        // Patch sent to both mm (public) and bpf (private) -> should downgrade
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
                // Mailing lists should be stripped
                assert!(!to.contains(&"linux-mm@kvack.org".to_string()));
                assert!(!to.contains(&"bpf@vger.kernel.org".to_string()));
                // Maintainer kept because cc_individuals was true in mm policy (union rules)
                assert!(cc.contains(&"maintainer@test.com".to_string()));
                assert!(cc.contains(&"mm-bot@test.com".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_author_only_delivery() {
        // A subsystem that tracks its list but sends the review only to the
        // patch author: reply_all=false strips the list, reply_to_author adds
        // the author, cc_individuals=false drops non-list individuals.
        let mut subsystems = HashMap::new();
        subsystems.insert(
            "drm-intel".to_string(),
            SubsystemPolicy {
                lists: vec!["intel-xe@lists.freedesktop.org".to_string()],
                reply_all: false,
                reply_to_author: true,
                cc_individuals: false,
                mute_all: false,
                ..Default::default()
            },
        );
        let policy = EmailPolicyConfig {
            defaults: SubsystemPolicy {
                reply_all: false,
                reply_to_author: false,
                cc_individuals: false,
                mute_all: false,
                ..Default::default()
            },
            subsystems,
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
                assert!(to.contains(&"author@test.com".to_string()));
                assert!(!to.contains(&"intel-xe@lists.freedesktop.org".to_string()));
                assert!(cc.is_empty(), "non-list individuals should be dropped");
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_defaults() {
        let policy = build_test_policy();
        // Unknown list -> defaults apply (private, reply_to_author=true, cc_individuals=true)
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
                assert!(to.contains(&"unknown-list@vger.kernel.org".to_string()));
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
    fn test_send_positive_review() {
        let mut policy = build_test_policy();

        // Test 1: Defaults has it true, and we fallback to defaults
        policy.defaults.send_positive_review = true;
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["unknown-list@vger.kernel.org".to_string()],
            &[],
            "author@test.com",
            "bot@sashiko.dev",
        );
        match action {
            Action::Send {
                send_positive_review,
                ..
            } => {
                assert!(send_positive_review);
            }
            _ => panic!("Expected Send"),
        }

        // Test 2: Subsystem has it true
        policy.defaults.send_positive_review = false;
        if let Some(sub) = policy.subsystems.get_mut("mm") {
            sub.send_positive_review = true;
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
                send_positive_review,
                ..
            } => {
                assert!(send_positive_review);
            }
            _ => panic!("Expected Send"),
        }

        // Test 3: Subsystem has it false, defaults has it true (should be false because subsystem matches and overrides)
        policy.defaults.send_positive_review = true;
        if let Some(sub) = policy.subsystems.get_mut("mm") {
            sub.send_positive_review = false;
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
                send_positive_review,
                ..
            } => {
                assert!(!send_positive_review);
            }
            _ => panic!("Expected Send"),
        }
    }

    #[test]
    fn test_resolve_patchwork_deduplication_and_merging() {
        let mut subsystems = HashMap::new();

        // Subsystem 1: mm - API and Email. Strict fail_severity (High), lenient min_severity (Medium)
        subsystems.insert(
            "mm".to_string(),
            SubsystemPolicy {
                lists: vec!["linux-mm@kvack.org".to_string()],
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

        // Subsystem 2: bpf - API only. Lenient fail_severity (Critical), strict min_severity (High)
        subsystems.insert(
            "bpf".to_string(),
            SubsystemPolicy {
                lists: vec!["bpf@vger.kernel.org".to_string()],
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

        // Subsystem 3: net - Email only. Overlaps email with mm, but has min_severity = None (all / Low)
        subsystems.insert(
            "net".to_string(),
            SubsystemPolicy {
                lists: vec!["netdev@vger.kernel.org".to_string()],
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
            defaults: SubsystemPolicy {
                lists: vec![],
                patchwork: PatchworkPolicy {
                    enabled: false,
                    api_url: None,
                    token: None,
                    email: None,
                    min_severity: None,
                    fail_severity: "High".to_string(),
                },
                ..Default::default()
            },
            subsystems,
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
}
