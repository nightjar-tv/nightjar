//! Regional certification ladders (ADR-0037 items 3 and 4).
//!
//! The ladder ships as a snapshot compiled into the binary, so setup and
//! evaluation never call a provider and first run works with no network
//! (ADR-0037 item 3, Rule 1.2). The embedded snapshot is the runtime source;
//! no refresh path is built, so this module never reads the network and never
//! performs request-time I/O.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The shipped snapshot, embedded at compile time.
const SHIPPED_LADDER: &str = include_str!("certification_ladder.json");

/// The closed cap set (ADR-0037 item 4). Declared least-to-most permissive so
/// "at or under cap" is an ordinary comparison and no second ordering exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CertificationTier {
    LittleKid,
    BigKid,
    Teen,
    Adult,
}

impl CertificationTier {
    pub const ALL: [Self; 4] = [Self::LittleKid, Self::BigKid, Self::Teen, Self::Adult];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LittleKid => "little_kid",
            Self::BigKid => "big_kid",
            Self::Teen => "teen",
            Self::Adult => "adult",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "little_kid" => Some(Self::LittleKid),
            "big_kid" => Some(Self::BigKid),
            "teen" => Some(Self::Teen),
            "adult" => Some(Self::Adult),
            _ => None,
        }
    }
}

/// One raw board label and the named cap it sits at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderRung {
    pub label: String,
    pub tier: CertificationTier,
}

/// One region's ordered ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionLadder {
    rungs: Vec<LadderRung>,
}

impl RegionLadder {
    /// The tier for a raw label, or `None` when the region's board has no such
    /// label. An absent label is a fail-closed unknown (ADR-0037 item 5), not a
    /// reason to fall back to another region's board.
    pub fn tier_for(&self, label: &str) -> Option<CertificationTier> {
        self.rungs
            .iter()
            .find(|rung| rung.label == label)
            .map(|rung| rung.tier)
    }

    pub fn rungs(&self) -> &[LadderRung] {
        &self.rungs
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LadderError {
    Malformed(String),
    BadRegionKey(String),
    EmptyRegion(String),
    EmptyLabel { region: String },
    DuplicateLabel { region: String, label: String },
    UnknownTier { region: String, tier: String },
    OutOfOrder { region: String, label: String },
}

/// A validated ladder snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificationLadder {
    version: u32,
    regions: BTreeMap<String, RegionLadder>,
}

#[derive(Deserialize)]
struct RawLadder {
    version: u32,
    regions: BTreeMap<String, Vec<RawRung>>,
}

#[derive(Deserialize)]
struct RawRung {
    label: String,
    tier: String,
}

impl CertificationLadder {
    /// Parse and validate a snapshot. Every rule is enforced here so a stored
    /// ladder that violates one cannot be used to evaluate anything.
    pub fn parse(json: &str) -> Result<Self, LadderError> {
        let raw: RawLadder =
            serde_json::from_str(json).map_err(|e| LadderError::Malformed(e.to_string()))?;
        if raw.version == 0 {
            return Err(LadderError::Malformed("version must be positive".into()));
        }
        if raw.regions.is_empty() {
            return Err(LadderError::Malformed("no regions".into()));
        }
        let mut regions = BTreeMap::new();
        for (region, raw_rungs) in raw.regions {
            if region.is_empty() || region != region.to_uppercase() {
                return Err(LadderError::BadRegionKey(region));
            }
            if raw_rungs.is_empty() {
                return Err(LadderError::EmptyRegion(region));
            }
            let mut rungs: Vec<LadderRung> = Vec::with_capacity(raw_rungs.len());
            let mut previous: Option<CertificationTier> = None;
            for raw_rung in raw_rungs {
                let label = raw_rung.label.trim().to_string();
                if label.is_empty() {
                    return Err(LadderError::EmptyLabel {
                        region: region.clone(),
                    });
                }
                if rungs.iter().any(|rung| rung.label == label) {
                    return Err(LadderError::DuplicateLabel {
                        region: region.clone(),
                        label,
                    });
                }
                let tier = CertificationTier::parse(&raw_rung.tier).ok_or_else(|| {
                    LadderError::UnknownTier {
                        region: region.clone(),
                        tier: raw_rung.tier.clone(),
                    }
                })?;
                if let Some(prev) = previous
                    && tier < prev
                {
                    return Err(LadderError::OutOfOrder {
                        region: region.clone(),
                        label,
                    });
                }
                previous = Some(tier);
                rungs.push(LadderRung { label, tier });
            }
            regions.insert(region, RegionLadder { rungs });
        }
        Ok(Self {
            version: raw.version,
            regions,
        })
    }

    /// The compiled-in snapshot. It is a build-time asset, so a parse failure
    /// here is a build defect; a test pins that it parses.
    pub fn shipped() -> Self {
        Self::parse(SHIPPED_LADDER).expect("shipped certification ladder parses")
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn has_region(&self, region: &str) -> bool {
        self.regions.contains_key(region)
    }

    pub fn region(&self, region: &str) -> Option<&RegionLadder> {
        self.regions.get(region)
    }

    /// The tier for a label on the selected region's board, or `None` when the
    /// region or the label is unknown. Never substitutes another region's
    /// board: an AU `PG` on a DE server is unknown, not a `DE` rung.
    pub fn tier_for(&self, region: &str, label: &str) -> Option<CertificationTier> {
        self.regions.get(region)?.tier_for(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_ladder_parses_and_covers_the_probe_regions() {
        let ladder = CertificationLadder::shipped();
        assert_eq!(ladder.version(), 1);
        for region in ["US", "AU", "DE", "BR", "GB"] {
            assert!(ladder.has_region(region), "{region} missing");
        }
    }

    #[test]
    fn known_mappings_are_deterministic() {
        let ladder = CertificationLadder::shipped();
        assert_eq!(
            ladder.tier_for("US", "G"),
            Some(CertificationTier::LittleKid)
        );
        assert_eq!(
            ladder.tier_for("US", "PG-13"),
            Some(CertificationTier::Teen)
        );
        assert_eq!(
            ladder.tier_for("US", "TV-MA"),
            Some(CertificationTier::Adult)
        );
        assert_eq!(ladder.tier_for("DE", "12"), Some(CertificationTier::BigKid));
        assert_eq!(ladder.tier_for("BR", "18"), Some(CertificationTier::Adult));
        assert_eq!(
            ladder.tier_for("AU", "MA 15+"),
            Some(CertificationTier::Teen)
        );
    }

    /// Regional isolation: a board label is not a translation of another
    /// region's, so it is unknown on the wrong server rather than remapped.
    #[test]
    fn a_label_from_another_board_is_unknown_not_substituted() {
        let ladder = CertificationLadder::shipped();
        // AU PG exists, but the US ladder is what a US server reads; AU's
        // "MA 15+" and "R 18+" are unknown to DE and US.
        assert_eq!(ladder.tier_for("US", "MA 15+"), None);
        assert_eq!(ladder.tier_for("DE", "MA 15+"), None);
        assert_eq!(ladder.tier_for("DE", "PG-13"), None);
        assert_eq!(ladder.tier_for("BR", "TV-MA"), None);
        // AU and DE happen to share "G"/"PG" and "12"; that is a real shared
        // spelling, not a substitution. The distinct labels are the proof.
        assert_eq!(
            ladder.tier_for("AU", "G"),
            Some(CertificationTier::LittleKid)
        );
        assert_eq!(ladder.tier_for("DE", "18"), Some(CertificationTier::Adult));
    }

    #[test]
    fn unknown_region_denies() {
        let ladder = CertificationLadder::shipped();
        assert!(!ladder.has_region("ZZ"));
        assert_eq!(ladder.tier_for("ZZ", "G"), None);
    }

    #[test]
    fn empty_unknown_and_malformed_labels_deny() {
        let ladder = CertificationLadder::shipped();
        assert_eq!(ladder.tier_for("US", ""), None);
        assert_eq!(ladder.tier_for("US", "UNRATED"), None);
        assert_eq!(ladder.tier_for("US", " pg-13 "), None, "labels are exact");
    }

    fn ladder_with(region: &str, rungs: &str) -> String {
        format!(r#"{{"version":1,"regions":{{"{region}":[{rungs}]}}}}"#)
    }

    #[test]
    fn lowercase_region_key_is_rejected() {
        let err =
            CertificationLadder::parse(&ladder_with("us", r#"{"label":"G","tier":"little_kid"}"#))
                .unwrap_err();
        assert!(matches!(err, LadderError::BadRegionKey(_)), "{err:?}");
    }

    #[test]
    fn duplicate_label_fails_closed() {
        let err = CertificationLadder::parse(&ladder_with(
            "US",
            r#"{"label":"G","tier":"little_kid"},{"label":"G","tier":"teen"}"#,
        ))
        .unwrap_err();
        assert!(matches!(err, LadderError::DuplicateLabel { .. }), "{err:?}");
    }

    #[test]
    fn unknown_tier_fails_closed() {
        let err =
            CertificationLadder::parse(&ladder_with("US", r#"{"label":"G","tier":"toddler"}"#))
                .unwrap_err();
        assert!(matches!(err, LadderError::UnknownTier { .. }), "{err:?}");
    }

    #[test]
    fn empty_region_and_empty_label_fail_closed() {
        let err = CertificationLadder::parse(&ladder_with("US", "")).unwrap_err();
        assert!(matches!(err, LadderError::EmptyRegion(_)), "{err:?}");
        let err = CertificationLadder::parse(&ladder_with("US", r#"{"label":"  ","tier":"teen"}"#))
            .unwrap_err();
        assert!(matches!(err, LadderError::EmptyLabel { .. }), "{err:?}");
    }

    #[test]
    fn a_ladder_that_steps_backwards_is_rejected() {
        let err = CertificationLadder::parse(&ladder_with(
            "US",
            r#"{"label":"R","tier":"adult"},{"label":"G","tier":"little_kid"}"#,
        ))
        .unwrap_err();
        assert!(matches!(err, LadderError::OutOfOrder { .. }), "{err:?}");
    }

    #[test]
    fn malformed_json_fails_closed() {
        assert!(matches!(
            CertificationLadder::parse("not json"),
            Err(LadderError::Malformed(_))
        ));
    }
}
