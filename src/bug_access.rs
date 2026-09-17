//! Who may see and change a Linux kernel bug.
//!
//! Authority over a bug comes from three independent sources, composed by
//! taking the strongest: the capability lists in the configuration, the
//! MAINTAINERS file, and, for operators, the local token handled at the HTTP
//! edge. This module owns the first two and the composition; it does not know
//! about HTTP.

use crate::maintainers::MaintainersIndex;
use crate::settings::AclSettings;
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use std::collections::HashSet;
use std::sync::Arc;

/// A MAINTAINERS section title, carried both as written and normalized for
/// comparison.
///
/// The type exists so that it is a compile error to compare a section title
/// against a directory prefix. Both are strings that name a subsystem, and
/// conflating them is the single most likely way to get this wrong: a
/// directory prefix is derived from the touched paths and names nobody, while
/// a section title names the people the kernel trusts with that code.
///
/// Equality, ordering and hashing all run on the normalized form, so two
/// spellings of the same section are the same title. The original is retained
/// because the database stores titles as MAINTAINERS spells them, and a query
/// parameter has to match that.
#[derive(Debug, Clone)]
pub struct SectionTitle {
    original: String,
    normalized: String,
}

impl SectionTitle {
    /// Normalizes a title by trimming, collapsing internal whitespace and
    /// lowercasing, so that a title stored on a bug matches the same title
    /// read out of MAINTAINERS regardless of incidental formatting.
    pub fn new(title: &str) -> Self {
        let mut normalized = String::with_capacity(title.len());
        for word in title.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.extend(word.chars().flat_map(char::to_lowercase));
        }
        Self {
            original: title.trim().to_string(),
            normalized,
        }
    }

    /// The normalized form, which is what comparisons use.
    pub fn as_str(&self) -> &str {
        &self.normalized
    }

    /// The title as MAINTAINERS spells it, for matching against stored rows.
    pub fn original(&self) -> &str {
        &self.original
    }
}

impl PartialEq for SectionTitle {
    fn eq(&self, other: &Self) -> bool {
        self.normalized == other.normalized
    }
}

impl Eq for SectionTitle {}

impl std::hash::Hash for SectionTitle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.normalized.hash(state);
    }
}

impl PartialOrd for SectionTitle {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SectionTitle {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.normalized.cmp(&other.normalized)
    }
}

/// What a principal may do to one specific bug.
///
/// Ordered from least to most authority, so that composing two grants over the
/// same bug is a maximum and invalid combinations cannot be represented. A
/// security list member who also maintains the affected subsystem gets Manage,
/// not Comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BugAccess {
    /// The bug is invisible. Reads answer as though it does not exist, so that
    /// its existence is not itself disclosed.
    None,
    /// May read the bug, its report, its enrichments and its comments.
    /// Deliberately excludes the raw analysis transcripts, which disclose
    /// other bugs; see [`BugPrincipal::has_global_bug_visibility`].
    Read,
    /// Read, plus may attach a comment.
    Comment,
    /// Comment, plus may close, dismiss, assign and mark duplicate.
    ///
    /// A future re-analysis endpoint belongs here rather than at Comment,
    /// because re-running an analysis spends money and the security list must
    /// not be able to trigger it outside its own maintained subsystems.
    Manage,
}

impl BugAccess {
    /// Whether this level permits reading the bug at all.
    pub fn can_read(self) -> bool {
        self >= BugAccess::Read
    }

    /// Whether this level permits attaching a comment.
    pub fn can_comment(self) -> bool {
        self >= BugAccess::Comment
    }

    /// Whether this level permits changing the bug.
    pub fn can_manage(self) -> bool {
        self >= BugAccess::Manage
    }

    /// Names the level for a refusal message. Only meaningful for a level that
    /// was demanded and not met, so the caller already knows the bug exists.
    pub fn describe(self) -> &'static str {
        match self {
            BugAccess::None => "no",
            BugAccess::Read => "read",
            BugAccess::Comment => "comment",
            BugAccess::Manage => "maintainer",
        }
    }
}

/// The bug-domain authority of one caller, resolved once per request from the
/// configuration and the MAINTAINERS index.
#[derive(Debug, Clone, Default)]
pub struct BugPrincipal {
    email: String,
    /// Sashiko operator. Manage on every bug.
    operator: bool,
    /// Kernel security list member. Comment on every bug.
    security: bool,
    /// Maintainer of a section that claims the whole tree, which today means
    /// THE REST. Manage on every bug.
    global_maintainer: bool,
    /// The sections this address maintains. Manage within these.
    maintained_sections: HashSet<SectionTitle>,
    /// May file a new bug over HTTP.
    may_create: bool,
}

impl BugPrincipal {
    /// A caller who proved nothing. Holds no authority over any bug.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Resolves what an authenticated address may do.
    ///
    /// A blocklisted address resolves to the anonymous principal, so that
    /// every later question about it answers no without the caller having to
    /// remember to ask about the blocklist first.
    pub fn resolve(email: &str, acl: &AclSettings, maintainers: Option<&MaintainersIndex>) -> Self {
        if email.trim().is_empty() || acl.is_blocklisted(email) {
            return Self::anonymous();
        }
        let operator = acl.is_admin(email);
        let maintained_sections = maintainers
            .and_then(|index| index.subsystems_for_address(email))
            .map(|titles| titles.iter().map(|t| SectionTitle::new(t)).collect())
            .unwrap_or_default();
        Self {
            email: email.trim().to_string(),
            operator,
            security: acl.is_security(email),
            global_maintainer: maintainers.is_some_and(|index| index.is_global_maintainer(email)),
            maintained_sections,
            // Operators can always file bugs; the list exists to admit tools
            // in addition to them.
            may_create: operator || acl.is_bug_reporter(email),
        }
    }

    /// The address this principal was resolved from.
    pub fn email(&self) -> &str {
        &self.email
    }

    /// Resolves the access level for a bug from the subsystems attributed to
    /// it.
    ///
    /// Only titles matched out of MAINTAINERS may be passed here. A directory
    /// prefix or a name the reporter invented names nobody, so a bug carrying
    /// only those matches no maintainer and stays invisible to them, which is
    /// the intended outcome for a bug nobody could classify.
    pub fn access_to(&self, attributed: &[SectionTitle]) -> BugAccess {
        if self.operator || self.global_maintainer {
            return BugAccess::Manage;
        }
        if attributed
            .iter()
            .any(|title| self.maintained_sections.contains(title))
        {
            return BugAccess::Manage;
        }
        if self.security {
            return BugAccess::Comment;
        }
        BugAccess::None
    }

    /// Whether this principal can read every bug regardless of subsystem.
    ///
    /// This gates the raw analysis transcripts. A transcript embeds the
    /// problem statements of unrelated bugs, because the deduplication stage
    /// compares the bug against every other bug in the database, so serving
    /// one to a subsystem-scoped maintainer would leak across the boundary the
    /// rest of the model maintains. For a principal who can already read every
    /// bug it discloses nothing new.
    pub fn has_global_bug_visibility(&self) -> bool {
        self.operator || self.security || self.global_maintainer
    }

    /// Whether this principal may file a new bug.
    pub fn may_create(&self) -> bool {
        self.may_create
    }

    /// The sections this principal maintains, spelled as MAINTAINERS spells
    /// them, for use as query parameters. Sorted so that the value is stable
    /// enough to key a cache on.
    pub fn maintained_section_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .maintained_sections
            .iter()
            .map(|title| title.original().to_string())
            .collect();
        names.sort();
        names
    }

    /// How much of the bug database this principal may list, as a database
    /// filter.
    pub fn visibility<'a>(&self, scope: &'a [String]) -> crate::db::BugVisibility<'a> {
        if self.has_global_bug_visibility() {
            crate::db::BugVisibility::Unrestricted
        } else {
            crate::db::BugVisibility::Sections(scope)
        }
    }

    /// The principal a server running in testing mode answers as.
    ///
    /// Testing mode already bypasses every other capability check, so bug
    /// routes follow it rather than becoming the one part of the server a
    /// developer cannot exercise locally. It carries no address, because no
    /// address was proved.
    fn testing_operator() -> Self {
        Self {
            operator: true,
            may_create: true,
            ..Self::default()
        }
    }
}

/// Extractor. Rejects with 401 when the request carries no valid session
/// token, then resolves the configuration and MAINTAINERS authority behind the
/// address that token names.
///
/// Being fallible is the point: a bug route cannot reach bug data without
/// naming this extractor in its signature, so forgetting the check does not
/// compile into an open route. The local token accepted by `is_authorized` is
/// deliberately not consulted here.
impl FromRequestParts<Arc<crate::api::AppState>> for BugPrincipal {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<crate::api::AppState>,
    ) -> Result<Self, Self::Rejection> {
        if !state.settings.server.login_enabled || state.settings.server.testing_mode {
            return Ok(BugPrincipal::testing_operator());
        }
        let user = crate::auth::AuthUser::from_request_parts(parts, state).await?;
        Ok(BugPrincipal::resolve(
            &user.email,
            &state.settings.server.acl,
            crate::maintainers::get_global_maintainers().as_deref(),
        ))
    }
}

/// Extractor for routes that are not bug routes but embed bug summaries, such
/// as the patchset and review views. Those stay readable without a session, so
/// a missing or invalid token resolves to the anonymous principal and the
/// embedded bugs are filtered out rather than the whole page being refused.
pub struct OptionalBugPrincipal(pub BugPrincipal);

impl FromRequestParts<Arc<crate::api::AppState>> for OptionalBugPrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<crate::api::AppState>,
    ) -> Result<Self, Self::Rejection> {
        match BugPrincipal::from_request_parts(parts, state).await {
            Ok(principal) => Ok(OptionalBugPrincipal(principal)),
            Err(_) => Ok(OptionalBugPrincipal(BugPrincipal::anonymous())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MAINTAINERS: &str = r#"
Maintainers List
===================

BTRFS FILE SYSTEM
M:	Chris Mason <clm@fb.com>
S:	Maintained
F:	fs/btrfs/

NETWORKING [GENERAL]
M:	David S. Miller <davem@davemloft.net>
S:	Maintained
F:	net/

THE REST
M:	Linus Torvalds <torvalds@linux-foundation.org>
S:	Buried alive in reporters
F:	*
F:	*/
"#;

    fn index() -> MaintainersIndex {
        MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap()
    }

    fn acl() -> AclSettings {
        AclSettings {
            admins: vec!["operator@example.org".to_string()],
            security: vec!["security@example.org".to_string()],
            bug_reporters: vec!["tool@example.org".to_string()],
            blocklist: vec!["clm@fb.com".to_string()],
            ..Default::default()
        }
    }

    fn btrfs() -> Vec<SectionTitle> {
        vec![SectionTitle::new("BTRFS FILE SYSTEM")]
    }

    #[test]
    fn test_section_title_ignores_incidental_formatting() {
        assert_eq!(
            SectionTitle::new("  BTRFS   FILE\tSYSTEM "),
            SectionTitle::new("btrfs file system")
        );
        assert_ne!(
            SectionTitle::new("BTRFS FILE SYSTEM"),
            SectionTitle::new("fs/btrfs")
        );
    }

    #[test]
    fn test_maintainer_manages_own_subsystem_only() {
        let index = index();
        let acl = AclSettings::default();
        let davem = BugPrincipal::resolve("davem@davemloft.net", &acl, Some(&index));
        assert_eq!(
            davem.access_to(&[SectionTitle::new("NETWORKING [GENERAL]")]),
            BugAccess::Manage
        );
        // Someone else's subsystem is invisible, not merely read only.
        assert_eq!(davem.access_to(&btrfs()), BugAccess::None);
        assert!(!davem.has_global_bug_visibility());
        assert!(!davem.may_create());
    }

    #[test]
    fn test_scope_is_reported_as_maintainers_spells_it() {
        // The database stores titles verbatim, so the projection that feeds the
        // query has to hand back the original spelling, not the normalized one
        // used for comparison.
        let davem = BugPrincipal::resolve(
            "davem@davemloft.net",
            &AclSettings::default(),
            Some(&index()),
        );
        assert_eq!(
            davem.maintained_section_names(),
            vec!["NETWORKING [GENERAL]".to_string()]
        );
        assert!(
            BugPrincipal::anonymous()
                .maintained_section_names()
                .is_empty()
        );
    }

    #[test]
    fn test_security_list_comments_everywhere_but_manages_nothing() {
        let index = index();
        let principal = BugPrincipal::resolve("security@example.org", &acl(), Some(&index));
        assert_eq!(principal.access_to(&btrfs()), BugAccess::Comment);
        // Including bugs nobody could attribute to a section at all.
        assert_eq!(principal.access_to(&[]), BugAccess::Comment);
        assert!(principal.has_global_bug_visibility());
        assert!(!principal.may_create());
    }

    #[test]
    fn test_operator_and_catch_all_maintainer_manage_everything() {
        let index = index();
        for email in ["operator@example.org", "torvalds@linux-foundation.org"] {
            let principal = BugPrincipal::resolve(email, &acl(), Some(&index));
            assert_eq!(principal.access_to(&btrfs()), BugAccess::Manage);
            assert_eq!(principal.access_to(&[]), BugAccess::Manage);
            assert!(principal.has_global_bug_visibility());
        }
        // Only the operator may file bugs; being Linus is not a reporter role.
        assert!(BugPrincipal::resolve("operator@example.org", &acl(), Some(&index)).may_create());
        assert!(
            !BugPrincipal::resolve("torvalds@linux-foundation.org", &acl(), Some(&index))
                .may_create()
        );
    }

    #[test]
    fn test_strongest_grant_wins() {
        let acl = AclSettings {
            security: vec!["davem@davemloft.net".to_string()],
            ..Default::default()
        };
        let principal = BugPrincipal::resolve("davem@davemloft.net", &acl, Some(&index()));
        // Maintainer of the affected subsystem and on the security list.
        assert_eq!(
            principal.access_to(&[SectionTitle::new("NETWORKING [GENERAL]")]),
            BugAccess::Manage
        );
        // Elsewhere the weaker grant still applies.
        assert_eq!(principal.access_to(&btrfs()), BugAccess::Comment);
    }

    #[test]
    fn test_blocklisted_maintainer_holds_nothing() {
        let principal = BugPrincipal::resolve("clm@fb.com", &acl(), Some(&index()));
        assert_eq!(principal.access_to(&btrfs()), BugAccess::None);
        assert!(!principal.has_global_bug_visibility());
        assert!(!principal.may_create());
        assert_eq!(principal.email(), "");
    }

    #[test]
    fn test_unknown_and_anonymous_callers_see_nothing() {
        for principal in [
            BugPrincipal::anonymous(),
            BugPrincipal::resolve("stranger@example.org", &acl(), Some(&index())),
            // A missing index must not accidentally widen anyone's access.
            BugPrincipal::resolve("davem@davemloft.net", &AclSettings::default(), None),
        ] {
            assert_eq!(principal.access_to(&btrfs()), BugAccess::None);
            assert_eq!(principal.access_to(&[]), BugAccess::None);
            assert!(!principal.has_global_bug_visibility());
            assert!(!principal.may_create());
        }
    }

    #[test]
    fn test_access_levels_are_ordered() {
        assert!(BugAccess::None < BugAccess::Read);
        assert!(BugAccess::Read < BugAccess::Comment);
        assert!(BugAccess::Comment < BugAccess::Manage);
        assert!(!BugAccess::None.can_read());
        assert!(BugAccess::Read.can_read());
        assert!(!BugAccess::Read.can_comment());
        assert!(BugAccess::Comment.can_comment());
        assert!(!BugAccess::Comment.can_manage());
        assert!(BugAccess::Manage.can_manage());
    }
}
