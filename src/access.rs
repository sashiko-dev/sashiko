//! What one caller is allowed to see and change.
//!
//! Authority comes from three independent sources, composed by taking the
//! strongest: the capability lists in the configuration, the MAINTAINERS file,
//! and, for operators, the local token handled at the HTTP edge. This module
//! owns the first two and the composition; it does not know about HTTP.
//!
//! A caller's standing is resolved once, into [`Principal`], and each domain
//! then asks its own question of it. Today that is bug access, via
//! [`BugAccess`]. The resolution is deliberately domain neutral, because the
//! evidence it rests on, who the configuration names and what MAINTAINERS says
//! they own, is the same evidence whatever is being protected.

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
    /// other bugs; see [`Principal::has_global_bug_visibility`].
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

    /// Parses a bug access level name (`"none"`, `"read"`, `"comment"`, `"manage"`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(BugAccess::None),
            "read" => Some(BugAccess::Read),
            "comment" => Some(BugAccess::Comment),
            "manage" => Some(BugAccess::Manage),
            _ => None,
        }
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

/// The standing of one caller, resolved once per request from the
/// configuration and the MAINTAINERS index.
///
/// This records what the caller is, not what they may do. Turning that into a
/// decision is each domain's own business, because the same maintainer may be
/// owed different answers about different things.
#[derive(Debug, Clone, Default)]
pub struct Principal {
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
    /// Optional privilege ceiling imposed by a scoped API token.
    max_access: Option<BugAccess>,
}

impl Principal {
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
            max_access: None,
        }
    }

    /// Imposes an upper bound on the bug access level this principal may
    /// exercise, used when authenticating with a scoped API token.
    pub fn with_max_access(mut self, max: Option<BugAccess>) -> Self {
        self.max_access = max;
        if let Some(limit) = max
            && limit < BugAccess::Manage
        {
            self.may_create = false;
        }
        self
    }

    /// Whether this principal is a configured Sashiko operator.
    pub fn is_operator(&self) -> bool {
        self.operator
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
        self.access_to_with_bugid(attributed, false)
    }

    /// Resolves the access level for a bug from the subsystems attributed to
    /// it and whether the caller addressed the bug by its `bugid`.
    ///
    /// Knowing the `bugid` grants `BugAccess::Read` on that single bug, subject
    /// to any explicit `max_access` ceiling imposed on a scoped API token.
    pub fn access_to_with_bugid(&self, attributed: &[SectionTitle], has_bugid: bool) -> BugAccess {
        let base = if self.operator
            || self.global_maintainer
            || attributed
                .iter()
                .any(|title| self.maintained_sections.contains(title))
        {
            BugAccess::Manage
        } else if self.security {
            BugAccess::Comment
        } else if has_bugid {
            BugAccess::Read
        } else {
            BugAccess::None
        };
        match self.max_access {
            Some(limit) => base.min(limit),
            None => base,
        }
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
        (self.operator || self.security || self.global_maintainer)
            && self.max_access.is_none_or(|m| m.can_read())
    }

    /// Whether this principal may file a new bug.
    pub fn may_create(&self) -> bool {
        self.may_create
    }

    /// Whether this principal may view embargoed review results on a patchset
    /// from the MAINTAINERS sections attributed to it.
    pub fn may_view_embargoed(&self, attributed: &[SectionTitle]) -> bool {
        if self.max_access.is_some_and(|m| !m.can_read()) {
            return false;
        }
        self.operator
            || self.global_maintainer
            || attributed
                .iter()
                .any(|title| self.maintained_sections.contains(title))
    }

    /// Whether this principal may view embargoed review results on every
    /// patchset regardless of which MAINTAINERS sections it touches.
    pub fn has_global_embargo_visibility(&self) -> bool {
        self.max_access.is_none_or(|m| m.can_read()) && (self.operator || self.global_maintainer)
    }

    /// Whether this principal holds any maintainer or operator standing that
    /// could grant embargo visibility on at least one patchset.
    pub fn has_any_embargo_visibility(&self) -> bool {
        self.max_access.is_none_or(|m| m.can_read())
            && (self.operator || self.global_maintainer || !self.maintained_sections.is_empty())
    }

    /// Whether this principal may release a review embargo on a patchset from
    /// the MAINTAINERS sections attributed to it.
    pub fn may_release_embargo(&self, attributed: &[SectionTitle]) -> bool {
        self.access_to(attributed).can_manage()
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
        if self.max_access.is_some_and(|m| !m.can_read()) {
            crate::db::BugVisibility::Sections(&[])
        } else if self.has_global_bug_visibility() {
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
impl FromRequestParts<Arc<crate::server::AppState>> for Principal {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<crate::server::AppState>,
    ) -> Result<Self, Self::Rejection> {
        if !state.settings.server.login_enabled || state.settings.server.testing_mode {
            return Ok(Principal::testing_operator());
        }
        let user = crate::auth::AuthUser::from_request_parts(parts, state).await?;
        if user
            .sid
            .as_deref()
            .is_some_and(|sid| state.settings.server.acl.is_blocklisted(sid))
        {
            return Err((StatusCode::UNAUTHORIZED, "Token has been revoked."));
        }
        let max_access = match user.max_bug_access.as_deref() {
            Some(s) => match BugAccess::parse(s) {
                Some(level) => Some(level),
                None => {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "Invalid max_bug_access claim in token.",
                    ));
                }
            },
            None => None,
        };
        Ok(Principal::resolve(
            &user.email,
            &state.settings.server.acl,
            crate::maintainers::get_global_maintainers().as_deref(),
        )
        .with_max_access(max_access))
    }
}

/// Extractor for routes that are not bug routes but embed bug summaries, such
/// as the patchset and review views. Those stay readable without a session, so
/// a missing or invalid token resolves to the anonymous principal and the
/// embedded bugs are filtered out rather than the whole page being refused.
pub struct OptionalPrincipal(pub Principal);

impl FromRequestParts<Arc<crate::server::AppState>> for OptionalPrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<crate::server::AppState>,
    ) -> Result<Self, Self::Rejection> {
        match Principal::from_request_parts(parts, state).await {
            Ok(principal) => Ok(OptionalPrincipal(principal)),
            Err(_) => Ok(OptionalPrincipal(Principal::anonymous())),
        }
    }
}

/// Whether a caller may read a patchset's raw review transcripts.
///
/// Two states rather than a bool so that the decision cannot be confused with
/// any other predicate at a call site, and so that a future third state has
/// somewhere to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptAccess {
    Denied,
    Granted,
}

impl TranscriptAccess {
    /// Whether this decision permits reading raw transcripts.
    pub fn is_granted(self) -> bool {
        self == TranscriptAccess::Granted
    }
}

/// A principal together with the local-token evidence the HTTP edge saw.
///
/// Extracted infallibly: an anonymous caller resolves to a principal holding
/// nothing rather than a rejection, because the routes this guards also serve
/// public data and must not start refusing it.
#[derive(Debug, Clone)]
pub struct TranscriptPrincipal {
    principal: Principal,
    local_operator: bool,
}

impl TranscriptPrincipal {
    /// Constructs a transcript principal from a resolved standing and whether
    /// the local operator token was presented.
    pub fn new(principal: Principal, local_operator: bool) -> Self {
        Self {
            principal,
            local_operator,
        }
    }

    /// A caller who proved nothing and presented no local token.
    pub fn anonymous() -> Self {
        Self {
            principal: Principal::anonymous(),
            local_operator: false,
        }
    }

    /// The underlying standing, without the local operator token grant.
    ///
    /// Routes that also redact embedded bugs must pass this to the bug
    /// redactor rather than consulting [`Self::access_to_patchset`], because
    /// the local token never grants bug visibility.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Resolves whether this caller may read a patchset's raw transcripts from
    /// the MAINTAINERS sections attributed to it and the series author string.
    ///
    /// A patchset with no attributed sections fails closed: section-maintainer
    /// standing cannot match an empty list, so only the local token, global
    /// visibility, or being the series author remains.
    pub fn access_to_patchset(
        &self,
        attributed: &[SectionTitle],
        author: Option<&str>,
    ) -> TranscriptAccess {
        if self.local_operator {
            return TranscriptAccess::Granted;
        }
        if self.principal.max_access.is_some_and(|m| !m.can_read()) {
            return TranscriptAccess::Denied;
        }
        if self.principal.has_global_bug_visibility() {
            return TranscriptAccess::Granted;
        }
        if attributed
            .iter()
            .any(|title| self.principal.maintained_sections.contains(title))
        {
            return TranscriptAccess::Granted;
        }
        if !self.principal.email.is_empty()
            && let Some(author) = author
            && let Some(author_email) = crate::maintainers::maintainer_address(author)
            && self.principal.email.eq_ignore_ascii_case(&author_email)
        {
            return TranscriptAccess::Granted;
        }
        TranscriptAccess::Denied
    }
}

impl FromRequestParts<Arc<crate::server::AppState>> for TranscriptPrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<crate::server::AppState>,
    ) -> Result<Self, Self::Rejection> {
        let local_operator = crate::server::presents_local_token(&parts.headers, state);
        let principal = match Principal::from_request_parts(parts, state).await {
            Ok(principal) => principal,
            Err(_) => Principal::anonymous(),
        };
        Ok(Self {
            principal,
            local_operator,
        })
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
        let davem = Principal::resolve("davem@davemloft.net", &acl, Some(&index));
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
        let davem = Principal::resolve(
            "davem@davemloft.net",
            &AclSettings::default(),
            Some(&index()),
        );
        assert_eq!(
            davem.maintained_section_names(),
            vec!["NETWORKING [GENERAL]".to_string()]
        );
        assert!(Principal::anonymous().maintained_section_names().is_empty());
    }

    #[test]
    fn test_security_list_comments_everywhere_but_manages_nothing() {
        let index = index();
        let principal = Principal::resolve("security@example.org", &acl(), Some(&index));
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
            let principal = Principal::resolve(email, &acl(), Some(&index));
            assert_eq!(principal.access_to(&btrfs()), BugAccess::Manage);
            assert_eq!(principal.access_to(&[]), BugAccess::Manage);
            assert!(principal.has_global_bug_visibility());
        }
        // Only the operator may file bugs; being Linus is not a reporter role.
        assert!(Principal::resolve("operator@example.org", &acl(), Some(&index)).may_create());
        assert!(
            !Principal::resolve("torvalds@linux-foundation.org", &acl(), Some(&index)).may_create()
        );
    }

    #[test]
    fn test_strongest_grant_wins() {
        let acl = AclSettings {
            security: vec!["davem@davemloft.net".to_string()],
            ..Default::default()
        };
        let principal = Principal::resolve("davem@davemloft.net", &acl, Some(&index()));
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
        let principal = Principal::resolve("clm@fb.com", &acl(), Some(&index()));
        assert_eq!(principal.access_to(&btrfs()), BugAccess::None);
        assert!(!principal.has_global_bug_visibility());
        assert!(!principal.may_create());
        assert_eq!(principal.email(), "");
    }

    #[test]
    fn test_unknown_and_anonymous_callers_see_nothing() {
        for principal in [
            Principal::anonymous(),
            Principal::resolve("stranger@example.org", &acl(), Some(&index())),
            // A missing index must not accidentally widen anyone's access.
            Principal::resolve("davem@davemloft.net", &AclSettings::default(), None),
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

    #[test]
    fn test_transcript_access_granted_by_local_token() {
        let caller = TranscriptPrincipal::new(Principal::anonymous(), true);
        assert_eq!(
            caller.access_to_patchset(&[], None),
            TranscriptAccess::Granted
        );
        assert!(caller.access_to_patchset(&btrfs(), None).is_granted());
        // The local token grants transcript access without mutating underlying
        // bug visibility.
        assert!(!caller.principal().has_global_bug_visibility());
    }

    #[test]
    fn test_transcript_access_granted_by_global_roles() {
        let index = index();
        for email in [
            "operator@example.org",
            "security@example.org",
            "torvalds@linux-foundation.org",
        ] {
            let caller =
                TranscriptPrincipal::new(Principal::resolve(email, &acl(), Some(&index)), false);
            assert_eq!(
                caller.access_to_patchset(&[], None),
                TranscriptAccess::Granted
            );
            assert_eq!(
                caller.access_to_patchset(&btrfs(), Some("Someone Else <other@example.org>")),
                TranscriptAccess::Granted
            );
        }
    }

    #[test]
    fn test_transcript_access_granted_to_section_maintainer_fails_closed_when_empty() {
        let davem = TranscriptPrincipal::new(
            Principal::resolve(
                "davem@davemloft.net",
                &AclSettings::default(),
                Some(&index()),
            ),
            false,
        );
        assert_eq!(
            davem.access_to_patchset(&[SectionTitle::new("NETWORKING [GENERAL]")], None),
            TranscriptAccess::Granted
        );
        // Someone else's subsystem is denied.
        assert_eq!(
            davem.access_to_patchset(&btrfs(), None),
            TranscriptAccess::Denied
        );
        // Unattributed patchset fails closed for section maintainers.
        assert_eq!(
            davem.access_to_patchset(&[], None),
            TranscriptAccess::Denied
        );
    }

    #[test]
    fn test_transcript_access_granted_to_series_author() {
        let stranger = TranscriptPrincipal::new(
            Principal::resolve("stranger@example.org", &acl(), Some(&index())),
            false,
        );
        assert_eq!(
            stranger.access_to_patchset(&[], Some("A Stranger <Stranger@Example.ORG>")),
            TranscriptAccess::Granted
        );
        assert_eq!(
            stranger.access_to_patchset(&btrfs(), Some("stranger@example.org")),
            TranscriptAccess::Granted
        );
        assert_eq!(
            stranger.access_to_patchset(&[], Some("Someone Else <other@example.org>")),
            TranscriptAccess::Denied
        );
        assert_eq!(
            stranger.access_to_patchset(&[], Some("not a parsable email")),
            TranscriptAccess::Denied
        );
        assert_eq!(
            stranger.access_to_patchset(&[], None),
            TranscriptAccess::Denied
        );

        // An anonymous caller holds no email and never matches an author string.
        let anon = TranscriptPrincipal::anonymous();
        assert_eq!(
            anon.access_to_patchset(&[], Some("Anonymous <stranger@example.org>")),
            TranscriptAccess::Denied
        );
    }

    #[test]
    fn test_blocklisted_caller_denied_transcript_access() {
        let blocklisted = TranscriptPrincipal::new(
            Principal::resolve("clm@fb.com", &acl(), Some(&index())),
            false,
        );
        // Blocklisted maintainer and author still gets Denied.
        assert_eq!(
            blocklisted.access_to_patchset(&btrfs(), Some("Chris Mason <clm@fb.com>")),
            TranscriptAccess::Denied
        );
    }

    #[test]
    fn test_bug_access_parse() {
        assert_eq!(BugAccess::parse("read"), Some(BugAccess::Read));
        assert_eq!(BugAccess::parse("COMMENT"), Some(BugAccess::Comment));
        assert_eq!(BugAccess::parse(" manage "), Some(BugAccess::Manage));
        assert_eq!(BugAccess::parse("invalid"), None);
    }

    #[test]
    fn test_principal_with_max_access_attenuates_never_elevates() {
        let idx = index();
        // Operator with read ceiling gets Read on everything and cannot create bugs.
        let op_read = Principal::resolve("operator@example.org", &acl(), Some(&idx))
            .with_max_access(Some(BugAccess::Read));
        assert_eq!(op_read.access_to(&btrfs()), BugAccess::Read);
        assert_eq!(op_read.access_to(&[]), BugAccess::Read);
        assert!(!op_read.may_create());

        // Subsystem maintainer with read ceiling gets Read on own subsystem and None elsewhere.
        let davem_read = Principal::resolve("davem@davemloft.net", &acl(), Some(&idx))
            .with_max_access(Some(BugAccess::Read));
        assert_eq!(
            davem_read.access_to(&[SectionTitle::new("NETWORKING [GENERAL]")]),
            BugAccess::Read
        );
        assert_eq!(davem_read.access_to(&btrfs()), BugAccess::None);

        // Stranger with manage ceiling still gets None (never elevates).
        let stranger = Principal::resolve("stranger@example.org", &acl(), Some(&idx))
            .with_max_access(Some(BugAccess::Manage));
        assert_eq!(stranger.access_to(&btrfs()), BugAccess::None);

        // Operator or maintainer with None ceiling gets None visibility and Denied transcript access.
        let op_none = Principal::resolve("operator@example.org", &acl(), Some(&idx))
            .with_max_access(Some(BugAccess::None));
        assert!(!op_none.has_global_bug_visibility());
        assert_eq!(
            op_none.visibility(&["BTRFS FILE SYSTEM".to_string()]),
            crate::db::BugVisibility::Sections(&[])
        );
        let tp_none = TranscriptPrincipal::new(op_none, false);
        assert_eq!(
            tp_none.access_to_patchset(&btrfs(), Some("operator@example.org")),
            TranscriptAccess::Denied
        );
    }

    #[test]
    fn test_access_to_with_bugid_grants_read_and_preserves_higher_roles() {
        let idx = index();
        let acl_cfg = acl();
        let net = [SectionTitle::new("NETWORKING [GENERAL]")];

        // Anonymous and unknown callers gain Read when addressing by bugid,
        // including on unattributed bugs.
        for caller in [
            Principal::anonymous(),
            Principal::resolve("stranger@example.org", &acl_cfg, Some(&idx)),
        ] {
            assert_eq!(
                caller.access_to_with_bugid(&btrfs(), false),
                BugAccess::None
            );
            assert_eq!(caller.access_to_with_bugid(&btrfs(), true), BugAccess::Read);
            assert_eq!(caller.access_to_with_bugid(&[], true), BugAccess::Read);
        }

        // Subsystem maintainer keeps Manage on own subsystem and gets Read on
        // another subsystem when addressing by bugid.
        let davem = Principal::resolve("davem@davemloft.net", &acl_cfg, Some(&idx));
        assert_eq!(davem.access_to_with_bugid(&net, true), BugAccess::Manage);
        assert_eq!(davem.access_to_with_bugid(&btrfs(), true), BugAccess::Read);

        // Security list keeps Comment when addressing by bugid.
        let sec = Principal::resolve("security@example.org", &acl_cfg, Some(&idx));
        assert_eq!(sec.access_to_with_bugid(&btrfs(), true), BugAccess::Comment);

        // Explicit None token ceiling attenuates even bugid read access.
        let none_token = Principal::resolve("stranger@example.org", &acl_cfg, Some(&idx))
            .with_max_access(Some(BugAccess::None));
        assert_eq!(
            none_token.access_to_with_bugid(&btrfs(), true),
            BugAccess::None
        );
    }

    #[test]
    fn test_may_release_embargo() {
        let idx = index();
        let acl_cfg = acl();
        let net = [SectionTitle::new("NETWORKING [GENERAL]")];

        let anon = Principal::anonymous();
        assert!(!anon.may_release_embargo(&btrfs()));
        assert!(!anon.may_release_embargo(&[]));

        let stranger = Principal::resolve("stranger@example.org", &acl_cfg, Some(&idx));
        assert!(!stranger.may_release_embargo(&btrfs()));

        // Security list has Comment access, not Manage, so it cannot release patchset embargoes.
        let sec = Principal::resolve("security@example.org", &acl_cfg, Some(&idx));
        assert!(!sec.may_release_embargo(&btrfs()));

        // Subsystem maintainer (M:) can release on their own section only, unless blocklisted.
        let davem = Principal::resolve("davem@davemloft.net", &acl_cfg, Some(&idx));
        assert!(davem.may_release_embargo(&net));
        assert!(!davem.may_release_embargo(&btrfs()));
        assert!(!davem.may_release_embargo(&[]));

        // clm@fb.com is in acl().blocklist, so blocklist revokes embargo release even on BTRFS.
        let clm_blocked = Principal::resolve("clm@fb.com", &acl_cfg, Some(&idx));
        assert!(!clm_blocked.may_release_embargo(&btrfs()));

        // Without the blocklist entry, clm@fb.com can release BTRFS embargoes only.
        let clm = Principal::resolve("clm@fb.com", &AclSettings::default(), Some(&idx));
        assert!(clm.may_release_embargo(&btrfs()));
        assert!(!clm.may_release_embargo(&net));

        // Scoped API token with Read or Comment ceiling attenuates embargo release,
        // but still permits viewing embargoed results on maintained sections unless
        // attenuated to None.
        let clm_none = clm.clone().with_max_access(Some(BugAccess::None));
        assert!(!clm_none.may_view_embargoed(&btrfs()));
        assert!(!clm_none.has_any_embargo_visibility());
        assert!(!clm_none.may_release_embargo(&btrfs()));
        let clm_read = clm.clone().with_max_access(Some(BugAccess::Read));
        assert!(clm_read.may_view_embargoed(&btrfs()));
        assert!(!clm_read.may_view_embargoed(&net));
        assert!(clm_read.has_any_embargo_visibility());
        assert!(!clm_read.has_global_embargo_visibility());
        assert!(!clm_read.may_release_embargo(&btrfs()));
        let clm_comment = clm.clone().with_max_access(Some(BugAccess::Comment));
        assert!(clm_comment.may_view_embargoed(&btrfs()));
        assert!(!clm_comment.may_release_embargo(&btrfs()));
        let clm_manage = clm.with_max_access(Some(BugAccess::Manage));
        assert!(clm_manage.may_view_embargoed(&btrfs()));
        assert!(clm_manage.may_release_embargo(&btrfs()));

        // Global maintainer (THE REST) and operator can view and release even unattributed patchsets.
        let torvalds = Principal::resolve("torvalds@linux-foundation.org", &acl_cfg, Some(&idx));
        assert!(torvalds.may_view_embargoed(&btrfs()));
        assert!(torvalds.may_view_embargoed(&[]));
        assert!(torvalds.has_global_embargo_visibility());
        assert!(torvalds.has_any_embargo_visibility());
        assert!(torvalds.may_release_embargo(&btrfs()));
        assert!(torvalds.may_release_embargo(&[]));

        let op = Principal::resolve("operator@example.org", &acl_cfg, Some(&idx));
        assert!(op.may_view_embargoed(&btrfs()));
        assert!(op.may_view_embargoed(&[]));
        assert!(op.has_global_embargo_visibility());
        assert!(op.has_any_embargo_visibility());
        assert!(op.may_release_embargo(&btrfs()));
        assert!(op.may_release_embargo(&[]));

        assert!(!anon.may_view_embargoed(&btrfs()));
        assert!(!anon.has_any_embargo_visibility());
        assert!(!stranger.may_view_embargoed(&btrfs()));
        assert!(!stranger.has_any_embargo_visibility());
        assert!(!sec.may_view_embargoed(&btrfs()));
        assert!(!sec.has_any_embargo_visibility());
        assert!(davem.may_view_embargoed(&net));
        assert!(!davem.may_view_embargoed(&btrfs()));
        assert!(!davem.may_view_embargoed(&[]));
        assert!(davem.has_any_embargo_visibility());
        assert!(!davem.has_global_embargo_visibility());
        assert!(!clm_blocked.may_view_embargoed(&btrfs()));
    }
}
