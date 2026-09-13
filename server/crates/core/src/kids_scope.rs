//! Pure kids-scope evaluator (ADR-0037 items 5, 6 and 7).
//!
//! The evaluator takes facts and returns visible or denied with a reason. It
//! never performs I/O and it never reads a provider: the query layer resolves
//! the stored facts, and this function decides. Every denial carries its
//! reason, which is what item 11's counts aggregate and what the tests assert.

use crate::certification::{CertificationLadder, CertificationTier};

/// Who is browsing (ADR-0037 item 7). Two-valued, no `Default`, not `Option`:
/// a new item-returning call site cannot compile without deciding, which is the
/// guarantee — the route-enumeration test is only a backstop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerScope {
    /// Unrestricted: an account session, or a profile that carries no cap.
    Account,
    /// A capped profile, on the locked server region.
    Profile {
        cap: CertificationTier,
        region: String,
    },
}

/// What the query layer found for one item on the server region's board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemCertification {
    /// A non-empty raw label read from the canonical row, or from the parent
    /// entity for an episode (ADR-0039 item 6).
    Label(String),
    /// No canonical row, or a row with no label for the server region.
    Missing,
    /// The stored `certifications_json` is not the decided region→label shape.
    Malformed,
    /// The stored facts carry two different non-empty labels for one region,
    /// so the projection could not choose one.
    Conflicting,
    /// An episode whose `tmdb_show` parent has no canonical `tv` row.
    UnresolvedParent,
}

/// Why a capped profile cannot see an item (ADR-0037 item 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KidsDenialReason {
    MetadataNotReady,
    MissingCertification,
    UnknownLabel,
    MalformedCertification,
    ConflictingCertification,
    UnresolvedParent,
    OverCap,
}

impl KidsDenialReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MetadataNotReady => "metadata_not_ready",
            Self::MissingCertification => "missing_certification",
            Self::UnknownLabel => "unknown_label",
            Self::MalformedCertification => "malformed_certification",
            Self::ConflictingCertification => "conflicting_certification",
            Self::UnresolvedParent => "unresolved_parent",
            Self::OverCap => "over_cap",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KidsScopeDecision {
    Visible,
    Denied(KidsDenialReason),
}

impl KidsScopeDecision {
    pub fn is_visible(self) -> bool {
        matches!(self, Self::Visible)
    }
}

/// The facts one item contributes to the decision. Borrowed, so the evaluator
/// allocates nothing and cannot mutate.
pub struct KidsScopeFacts<'a> {
    pub metadata_ready: bool,
    pub region: &'a str,
    pub cap: CertificationTier,
    pub certification: &'a ItemCertification,
    pub ladder: &'a CertificationLadder,
}

/// Decide one item for one capped profile. No I/O, no overrides: B2-7 owns the
/// allow/remove table and the plan forbids provisional behaviour for it.
pub fn decide_kids_scope(facts: &KidsScopeFacts<'_>) -> KidsScopeDecision {
    use KidsDenialReason as Reason;
    use KidsScopeDecision::{Denied, Visible};

    if !facts.metadata_ready {
        return Denied(Reason::MetadataNotReady);
    }
    let label = match facts.certification {
        ItemCertification::Label(label) => label,
        ItemCertification::Missing => return Denied(Reason::MissingCertification),
        ItemCertification::Malformed => return Denied(Reason::MalformedCertification),
        ItemCertification::Conflicting => return Denied(Reason::ConflictingCertification),
        ItemCertification::UnresolvedParent => return Denied(Reason::UnresolvedParent),
    };
    if label.trim().is_empty() {
        return Denied(Reason::MissingCertification);
    }
    match facts.ladder.tier_for(facts.region, label) {
        None => Denied(Reason::UnknownLabel),
        Some(tier) if tier <= facts.cap => Visible,
        Some(_) => Denied(Reason::OverCap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(
        ready: bool,
        region: &str,
        cap: CertificationTier,
        cert: &ItemCertification,
    ) -> KidsScopeDecision {
        let ladder = CertificationLadder::shipped();
        decide_kids_scope(&KidsScopeFacts {
            metadata_ready: ready,
            region,
            cap,
            certification: cert,
            ladder: &ladder,
        })
    }

    #[test]
    fn metadata_that_is_not_ready_denies_before_the_ladder() {
        // Even a correctly certified item denies while metadata is not ready.
        let cert = ItemCertification::Label("G".into());
        assert_eq!(
            decide(false, "US", CertificationTier::Adult, &cert),
            KidsScopeDecision::Denied(KidsDenialReason::MetadataNotReady)
        );
    }

    /// All four tiers against one region. Every denial has its asserted reason,
    /// and the boundary is exactly "at or under cap".
    #[test]
    fn four_tiers_and_the_at_or_under_boundary() {
        let cases = [
            ("G", CertificationTier::LittleKid, true),
            ("G", CertificationTier::BigKid, true),
            ("G", CertificationTier::Teen, true),
            ("G", CertificationTier::Adult, true),
            ("PG", CertificationTier::LittleKid, true),
            ("TV-PG", CertificationTier::LittleKid, false),
            ("TV-PG", CertificationTier::BigKid, true),
            ("PG-13", CertificationTier::BigKid, false),
            ("PG-13", CertificationTier::Teen, true),
            ("TV-14", CertificationTier::Teen, true),
            ("R", CertificationTier::Teen, false),
            ("R", CertificationTier::Adult, true),
            ("NC-17", CertificationTier::Adult, true),
            ("TV-MA", CertificationTier::Adult, true),
        ];
        for (label, cap, visible) in cases {
            let cert = ItemCertification::Label(label.into());
            let got = decide(true, "US", cap, &cert);
            if visible {
                assert_eq!(got, KidsScopeDecision::Visible, "{label}/{cap:?}");
            } else {
                assert_eq!(
                    got,
                    KidsScopeDecision::Denied(KidsDenialReason::OverCap),
                    "{label}/{cap:?}"
                );
            }
        }
    }

    /// A label belongs to one board, and an unknown one is not remapped. Two
    /// boards that happen to share a spelling (`12`) read the same only because
    /// the snapshot says so, not because the evaluator substituted.
    #[test]
    fn regional_isolation_changes_the_answer() {
        let twelve = ItemCertification::Label("12".into());
        assert_eq!(
            decide(true, "DE", CertificationTier::BigKid, &twelve),
            KidsScopeDecision::Visible
        );
        assert_eq!(
            decide(true, "DE", CertificationTier::LittleKid, &twelve),
            KidsScopeDecision::Denied(KidsDenialReason::OverCap)
        );
        assert_eq!(
            decide(true, "BR", CertificationTier::BigKid, &twelve),
            KidsScopeDecision::Visible,
            "BR maps 12 to big_kid"
        );
        // A label that belongs to another board is unknown, not remapped.
        let au = ItemCertification::Label("MA 15+".into());
        assert_eq!(
            decide(true, "US", CertificationTier::Adult, &au),
            KidsScopeDecision::Denied(KidsDenialReason::UnknownLabel)
        );
        assert_eq!(
            decide(true, "DE", CertificationTier::Adult, &au),
            KidsScopeDecision::Denied(KidsDenialReason::UnknownLabel)
        );
        let us_only = ItemCertification::Label("PG-13".into());
        assert_eq!(
            decide(true, "DE", CertificationTier::Adult, &us_only),
            KidsScopeDecision::Denied(KidsDenialReason::UnknownLabel)
        );
    }

    #[test]
    fn missing_and_empty_deny() {
        assert_eq!(
            decide(
                true,
                "US",
                CertificationTier::Adult,
                &ItemCertification::Missing
            ),
            KidsScopeDecision::Denied(KidsDenialReason::MissingCertification)
        );
        assert_eq!(
            decide(
                true,
                "US",
                CertificationTier::Adult,
                &ItemCertification::Label("   ".into())
            ),
            KidsScopeDecision::Denied(KidsDenialReason::MissingCertification)
        );
    }

    #[test]
    fn unknown_malformed_conflicting_and_unresolved_deny_with_reasons() {
        let cases = [
            (
                ItemCertification::Label("UNRATED".into()),
                KidsDenialReason::UnknownLabel,
            ),
            (
                ItemCertification::Malformed,
                KidsDenialReason::MalformedCertification,
            ),
            (
                ItemCertification::Conflicting,
                KidsDenialReason::ConflictingCertification,
            ),
            (
                ItemCertification::UnresolvedParent,
                KidsDenialReason::UnresolvedParent,
            ),
        ];
        for (cert, reason) in cases {
            assert_eq!(
                decide(true, "US", CertificationTier::Adult, &cert),
                KidsScopeDecision::Denied(reason),
                "{cert:?}"
            );
        }
    }

    #[test]
    fn unknown_region_denies_as_unknown_label() {
        let cert = ItemCertification::Label("G".into());
        assert_eq!(
            decide(true, "ZZ", CertificationTier::Adult, &cert),
            KidsScopeDecision::Denied(KidsDenialReason::UnknownLabel)
        );
    }

    /// Episode inputs carry the parent's label through the entity edge
    /// (ADR-0039 item 6), so the same ladder decision applies. The parent
    /// edge's absence is its own reason.
    #[test]
    fn episode_inputs_resolve_through_the_parent_edge() {
        let parent_label = ItemCertification::Label("TV-PG".into());
        assert_eq!(
            decide(true, "US", CertificationTier::BigKid, &parent_label),
            KidsScopeDecision::Visible
        );
        assert_eq!(
            decide(true, "US", CertificationTier::LittleKid, &parent_label),
            KidsScopeDecision::Denied(KidsDenialReason::OverCap)
        );
        assert_eq!(
            decide(
                true,
                "US",
                CertificationTier::Adult,
                &ItemCertification::UnresolvedParent
            ),
            KidsScopeDecision::Denied(KidsDenialReason::UnresolvedParent)
        );
    }

    #[test]
    fn every_reason_has_a_distinct_token() {
        let reasons = [
            KidsDenialReason::MetadataNotReady,
            KidsDenialReason::MissingCertification,
            KidsDenialReason::UnknownLabel,
            KidsDenialReason::MalformedCertification,
            KidsDenialReason::ConflictingCertification,
            KidsDenialReason::UnresolvedParent,
            KidsDenialReason::OverCap,
        ];
        let mut tokens: Vec<&str> = reasons.iter().map(|r| r.as_str()).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), reasons.len());
    }
}
