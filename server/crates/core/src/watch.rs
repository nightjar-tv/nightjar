//! Watch-state threshold policy (ADR-0035 item 2).

/// Below this percentage of duration the server keeps no resume point, so a
/// title opened by accident does not land on the continue-watching rail.
///
/// The value is ADR-0035 item 2's. It is a constant and not a setting
/// (Rule 4.12): the honest reason somebody wants a setting is a title with
/// eight minutes of credits, and the fix for that is reading the credits
/// offset, not asking the user for a percentage.
pub const RESUME_FLOOR_PERCENT: i64 = 2;

/// At or above this percentage of duration the item is played and drops off
/// the rail (ADR-0035 item 2). Constant, not a setting, for the same reason.
pub const PLAYED_AT_PERCENT: i64 = 90;

/// What one valid progress report means for the stored row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchReport {
    /// Below the floor. The writer removes any existing state.
    Clear,
    /// At or above the floor. `played` is true at or above the ceiling.
    Keep { played: bool },
}

/// Derive the policy for a report, or `None` when the report is not valid.
///
/// Invalid means a duration of zero or less, a negative position, or a
/// position past the duration. The policy carries no HTTP status; the API
/// turns `None` into its typed validation response.
pub fn watch_report(position_ms: i64, duration_ms: i64) -> Option<WatchReport> {
    if duration_ms <= 0 || position_ms < 0 || position_ms > duration_ms {
        return None;
    }
    if at_or_above(position_ms, duration_ms, RESUME_FLOOR_PERCENT) {
        Some(WatchReport::Keep {
            played: at_or_above(position_ms, duration_ms, PLAYED_AT_PERCENT),
        })
    } else {
        Some(WatchReport::Clear)
    }
}

/// Exact integer comparison, so the 2% and 90% boundaries are inclusive and no
/// float rounding moves a report across one. Cross-multiplication in `i128`
/// cannot overflow for `i64` inputs.
fn at_or_above(position_ms: i64, duration_ms: i64, percent: i64) -> bool {
    i128::from(position_ms) * 100 >= i128::from(duration_ms) * i128::from(percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0035 item 2 asks for the named boundary points, and the amendment
    /// fixes the inclusive edges. Both are here, because 1.9% and 2.1% do not
    /// say whether 2.0% keeps and 89.9% and 90.1% do not say whether 90.0%
    /// plays.
    #[test]
    fn thresholds_are_inclusive_at_the_edges() {
        let cases = [
            // (position, duration, expected)
            (190, 10_000, WatchReport::Clear), // 1.9%
            (199, 10_000, WatchReport::Clear), // 1.99%
            (200, 10_000, WatchReport::Keep { played: false }), // 2.0%
            (210, 10_000, WatchReport::Keep { played: false }), // 2.1%
            (8_990, 10_000, WatchReport::Keep { played: false }), // 89.9%
            (8_999, 10_000, WatchReport::Keep { played: false }), // 89.99%
            (9_000, 10_000, WatchReport::Keep { played: true }), // 90.0%
            (9_010, 10_000, WatchReport::Keep { played: true }), // 90.1%
            (10_000, 10_000, WatchReport::Keep { played: true }), // 100%
            (0, 10_000, WatchReport::Clear),
        ];
        for (position, duration, expected) in cases {
            assert_eq!(
                watch_report(position, duration),
                Some(expected),
                "{position}/{duration}"
            );
        }
    }

    /// A duration of zero has no ratio, so the report is not a valid one and
    /// the API refuses it rather than treating the ratio as zero.
    #[test]
    fn zero_and_negative_durations_are_invalid() {
        assert_eq!(watch_report(0, 0), None);
        assert_eq!(watch_report(1_000, 0), None);
        assert_eq!(watch_report(0, -1), None);
    }

    /// A negative position and a position past the end are both invalid. The
    /// second is the one a client can send by rounding up a duration.
    #[test]
    fn positions_outside_the_duration_are_invalid() {
        assert_eq!(watch_report(-1, 10_000), None);
        assert_eq!(watch_report(10_001, 10_000), None);
        assert_eq!(watch_report(1, -10_000), None);
    }

    /// The cross-multiplication is the reason `i64::MAX` is exact rather than
    /// overflowing: a large duration must not wrap the comparison.
    #[test]
    fn a_large_duration_does_not_overflow_the_comparison() {
        assert_eq!(
            watch_report(i64::MAX, i64::MAX),
            Some(WatchReport::Keep { played: true })
        );
        assert_eq!(
            watch_report(i64::MAX - 1, i64::MAX),
            Some(WatchReport::Keep { played: true })
        );
    }
}
