//! What a scan's progress supports being drawn as (ADR-0004 job states).
//!
//! The denominator is unknown while the walk runs, because the walk is still
//! discovering files. A percentage computed there moves backwards as the total
//! grows, so the decision of whether a percentage exists at all is made here,
//! once, and the client renders what it is told (Rule 2.1).

/// Whether the index pass is still adding work to the probe queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexPass {
    Running,
    Complete,
    Idle,
}

impl IndexPass {
    /// Map an ADR-0004 `scan_jobs.state`. `None` is a library with no job.
    ///
    /// `probing` is the state the job enters when the walk stops, which is the
    /// moment the probe total becomes fixed (`set_scan_job_index_done`).
    pub fn from_job_state(state: Option<&str>) -> Self {
        match state {
            Some("queued") | Some("indexing") => Self::Running,
            Some("probing") => Self::Complete,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressDisplay {
    None,
    Count,
    Bar,
}

impl ProgressDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Count => "count",
            Self::Bar => "bar",
        }
    }
}

/// Probe queue depth at or above which a bar is worth drawing.
///
/// Chosen from two measured regimes rather than by taste. On 2026-08-09
/// (cross-queue preempt probe, 2026-08-09) kept pace
/// with the walk and the backlog oscillated between 0 and 175, draining in
/// seconds. On 2026-08-07, before probes were enqueued as the walk discovered
/// them, the same library built a backlog of 23,244 and a priority item waited
/// 35 minutes behind it. 500 sits in the empty gap between those: near 3x the
/// deepest keep-pace oscillation, so ordinary drain never crosses it and a bar
/// never flickers into existence, and about 2% of the backlog peak, so a run
/// that is genuinely behind crosses it immediately and stays across it.
pub const PROBE_BAR_MIN_QUEUE_DEPTH: i64 = 500;

/// Probe line: a bar only once the total is fixed and the remainder is worth
/// one, a count whenever there is anything to count, nothing otherwise.
pub fn probe_display(index: IndexPass, queued: i64) -> ProgressDisplay {
    match index {
        // Mid-walk the count is all there is. A bar here would need a total
        // the walk has not finished producing.
        IndexPass::Running => ProgressDisplay::Count,
        IndexPass::Complete if queued >= PROBE_BAR_MIN_QUEUE_DEPTH => ProgressDisplay::Bar,
        _ if queued > 0 => ProgressDisplay::Count,
        _ => ProgressDisplay::None,
    }
}

/// The probe bar's denominator, or `None` while it would still be growing.
pub fn probe_total(index: IndexPass, done: i64, queued: i64) -> Option<i64> {
    match index {
        IndexPass::Running => None,
        _ => Some(done + queued),
    }
}

/// Metadata line: its own count, never folded into the probe bar. The two
/// finish at different times, so one percentage over both would be dishonest
/// in the way the no-bar-during-walk rule exists to avoid.
pub fn metadata_display(pending: i64) -> ProgressDisplay {
    if pending > 0 {
        ProgressDisplay::Count
    } else {
        ProgressDisplay::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_states_map_to_index_pass() {
        let cases: &[(Option<&str>, IndexPass)] = &[
            (Some("queued"), IndexPass::Running),
            (Some("indexing"), IndexPass::Running),
            (Some("probing"), IndexPass::Complete),
            (Some("completed"), IndexPass::Idle),
            (Some("failed"), IndexPass::Idle),
            (None, IndexPass::Idle),
        ];
        for (state, expected) in cases {
            assert_eq!(IndexPass::from_job_state(*state), *expected, "{state:?}");
        }
    }

    /// The whole risk in this feature is computing a percentage mid-walk, so
    /// the negative case is asserted directly rather than left implied.
    #[test]
    fn no_total_and_no_bar_while_the_walk_runs() {
        for queued in [0, 1, 175, 500, 23_244, 250_000] {
            assert_eq!(
                probe_total(IndexPass::Running, 4_000, queued),
                None,
                "queued={queued}"
            );
            assert_eq!(
                probe_display(IndexPass::Running, queued),
                ProgressDisplay::Count,
                "queued={queued}"
            );
        }
    }

    #[test]
    fn total_is_fixed_once_the_index_pass_finishes() {
        assert_eq!(
            probe_total(IndexPass::Complete, 22_800, 444),
            Some(23_244),
            "total is done plus queued, not either alone"
        );
        assert_eq!(probe_total(IndexPass::Idle, 25_038, 0), Some(25_038));
    }

    #[test]
    fn probe_display_at_the_threshold() {
        let cases: &[(IndexPass, i64, ProgressDisplay)] = &[
            // The 2026-08-09 keep-pace regime never draws a bar.
            (IndexPass::Complete, 0, ProgressDisplay::None),
            (IndexPass::Complete, 175, ProgressDisplay::Count),
            (IndexPass::Complete, 499, ProgressDisplay::Count),
            // The 2026-08-07 backlog regime does.
            (IndexPass::Complete, 500, ProgressDisplay::Bar),
            (IndexPass::Complete, 23_244, ProgressDisplay::Bar),
            // A terminal job with probes still queued keeps a count rather
            // than dropping the line; with none, it shows nothing.
            (IndexPass::Idle, 40, ProgressDisplay::Count),
            (IndexPass::Idle, 0, ProgressDisplay::None),
            // Idle never draws a bar, however deep the queue: nothing is
            // driving it down, so a percentage would sit still.
            (IndexPass::Idle, 23_244, ProgressDisplay::Count),
        ];
        for (index, queued, expected) in cases {
            assert_eq!(
                probe_display(*index, *queued),
                *expected,
                "{index:?} queued={queued}"
            );
        }
    }

    #[test]
    fn metadata_counts_alone() {
        assert_eq!(metadata_display(200), ProgressDisplay::Count);
        assert_eq!(metadata_display(0), ProgressDisplay::None);
    }
}
