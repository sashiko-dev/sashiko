use crate::email_policy::{EmailPolicyConfig, SubsystemPolicy};
use std::collections::HashSet;

pub enum Action {
    Mute,
    Send { to: Vec<String>, cc: Vec<String> },
}

pub struct EmailRouter {}

impl EmailRouter {
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

        for (name, sub_policy) in &policy.subsystems {
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
        let mut send_to_author = false;
        let mut send_to_maintainers = false;
        let mut custom_recipients = Vec::new();

        for p in active_policies {
            if p.mute_all {
                mute_all = true;
            }
            if !p.allow_public_reply {
                is_private = true;
            }
            if p.send_to_author {
                send_to_author = true;
            }
            if p.send_to_maintainers {
                send_to_maintainers = true;
            }
            for cr in &p.custom_recipients {
                custom_recipients.push(cr.clone());
            }
        }

        if mute_all {
            return Action::Mute;
        }

        let mut final_to = HashSet::new();
        let mut final_cc = HashSet::new();

        if send_to_author && !patch_author.is_empty() {
            final_to.insert(patch_author.to_string());
        }

        for cr in custom_recipients {
            final_cc.insert(cr);
        }

        // Add original non-mailing-list recipients if send_to_maintainers is true
        // Or if it's public, add everyone (mailing lists included, unless it's private)
        for addr in incoming_to {
            let addr_lower = addr.to_lowercase();
            let is_mailing_list = known_mailing_lists.iter().any(|ml| addr_lower.contains(ml));

            if (!is_private) || (send_to_maintainers && !is_mailing_list) {
                final_to.insert(addr.to_string());
            }
        }

        for addr in incoming_cc {
            let addr_lower = addr.to_lowercase();
            let is_mailing_list = known_mailing_lists.iter().any(|ml| addr_lower.contains(ml));

            if (!is_private) || (send_to_maintainers && !is_mailing_list) {
                final_cc.insert(addr.to_string());
            }
        }

        // Sanitize
        let sashiko_lower = sashiko_address.to_lowercase();
        final_to.retain(|a| !a.to_lowercase().contains(&sashiko_lower));
        final_cc.retain(|a| !a.to_lowercase().contains(&sashiko_lower) && !final_to.contains(a));

        Action::Send {
            to: final_to.into_iter().collect(),
            cc: final_cc.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn build_test_policy() -> EmailPolicyConfig {
        let mut subsystems = HashMap::new();
        subsystems.insert(
            "mm".to_string(),
            SubsystemPolicy {
                lists: vec!["linux-mm@kvack.org".to_string()],
                allow_public_reply: true,
                send_to_author: true,
                send_to_maintainers: true,
                mute_all: false,
                custom_recipients: vec!["mm-bot@test.com".to_string()],
            },
        );
        subsystems.insert(
            "bpf".to_string(),
            SubsystemPolicy {
                lists: vec!["bpf@vger.kernel.org".to_string()],
                allow_public_reply: false,
                send_to_author: true,
                send_to_maintainers: false,
                mute_all: false,
                custom_recipients: vec![],
            },
        );
        subsystems.insert(
            "net".to_string(),
            SubsystemPolicy {
                lists: vec!["netdev@vger.kernel.org".to_string()],
                allow_public_reply: true,
                send_to_author: true,
                send_to_maintainers: true,
                mute_all: true,
                custom_recipients: vec![],
            },
        );

        EmailPolicyConfig {
            defaults: SubsystemPolicy {
                lists: vec![],
                allow_public_reply: false,
                send_to_author: true,
                send_to_maintainers: true,
                mute_all: false,
                custom_recipients: vec![],
            },
            subsystems,
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
            Action::Send { to, cc } => {
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
            Action::Send { to, cc } => {
                assert!(to.contains(&"author@test.com".to_string()));
                // Mailing lists should be stripped
                assert!(!to.contains(&"linux-mm@kvack.org".to_string()));
                assert!(!to.contains(&"bpf@vger.kernel.org".to_string()));
                // Maintainer kept because send_to_maintainers was true in mm policy (union rules)
                assert!(cc.contains(&"maintainer@test.com".to_string()));
                assert!(cc.contains(&"mm-bot@test.com".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }

    #[test]
    fn test_defaults() {
        let policy = build_test_policy();
        // Unknown list -> defaults apply (private, send_to_author=true, send_to_maintainers=true)
        let action = EmailRouter::resolve_recipients(
            &policy,
            &["unknown-list@vger.kernel.org".to_string()],
            &["maintainer@test.com".to_string()],
            "author@test.com",
            "bot@sashiko.dev",
        );

        match action {
            Action::Send { to, cc } => {
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
            Action::Send { to, cc } => {
                assert!(!to.contains(&"bot@sashiko.dev".to_string()));
                assert!(!cc.contains(&"bot@sashiko.dev".to_string()));
            }
            Action::Mute => panic!("Should not mute"),
        }
    }
}
