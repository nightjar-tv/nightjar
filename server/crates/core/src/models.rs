use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
    Movies,
    Shows,
}

impl LibraryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movies => "movies",
            Self::Shows => "shows",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movies" => Some(Self::Movies),
            "shows" => Some(Self::Shows),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Movie,
    Episode,
    Unknown,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Episode => "episode",
            Self::Unknown => "unknown",
        }
    }
}

/// Account authority (ADR-0040 item 1). Three closed values on the account,
/// not a roles table and not a permission set.
///
/// Deliberately **not** `Ord`. Authority here is the action list in ADR-0040
/// item 3, not a ranking: `owner` is not "more" than `manager` on some scale,
/// it is the only role that may perform exactly three actions. A comparable
/// enum invites `role >= Manager`, which is the precedence field item 4
/// rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Manager,
    Member,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Manager => "manager",
            Self::Member => "member",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "manager" => Some(Self::Manager),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    /// Full account powers across the server: create accounts, delete member
    /// accounts, manage anyone's profiles, reset passwords, revoke sessions
    /// (ADR-0040 item 1). Excludes the owner-only actions.
    pub fn has_account_powers(self) -> bool {
        matches!(self, Self::Owner | Self::Manager)
    }

    /// The three owner-only actions (ADR-0040 item 3): transfer ownership,
    /// change any account's role, delete a `manager` account. Named as one
    /// predicate so a route cannot invent a fourth by reaching for a
    /// comparison.
    pub fn is_owner(self) -> bool {
        matches!(self, Self::Owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_the_stored_spelling() {
        for role in [Role::Owner, Role::Manager, Role::Member] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("admin"), None);
        assert_eq!(Role::parse("Owner"), None, "the CHECK is lowercase");
        assert_eq!(Role::parse(""), None);
    }

    /// ADR-0040 item 1's boundaries, asserted rather than left to each route.
    #[test]
    fn account_powers_and_owner_only_are_separate_questions() {
        assert!(Role::Owner.has_account_powers());
        assert!(Role::Manager.has_account_powers());
        assert!(!Role::Member.has_account_powers());

        assert!(Role::Owner.is_owner());
        assert!(!Role::Manager.is_owner(), "a manager is not a lesser owner");
        assert!(!Role::Member.is_owner());
    }

    /// ADR-0040 item 4: authority is the action list, not a precedence field.
    /// The two predicates are the whole interface, so this pins how many roles
    /// each one admits. A third question asked by comparing roles would have to
    /// change one of these counts to be useful, which is what makes them a
    /// guard rather than a restatement.
    ///
    /// `Ord` is deliberately not derived on `Role`; that is enforced by its
    /// absence from the derive list and the note above the type, not here. A
    /// test cannot assert the absence of a trait impl on stable Rust, and one
    /// that appears to is worse than none.
    #[test]
    fn authority_is_two_predicates_and_not_a_ranking() {
        let all = [Role::Owner, Role::Manager, Role::Member];
        assert_eq!(all.iter().filter(|r| r.is_owner()).count(), 1);
        assert_eq!(all.iter().filter(|r| r.has_account_powers()).count(), 2);
        // Manager is the middle role and it is middle in exactly one way: it
        // has account powers and is not the owner. Anything else it "cannot
        // do" is item 3's list, not a rank.
        assert!(Role::Manager.has_account_powers() && !Role::Manager.is_owner());
    }
}
