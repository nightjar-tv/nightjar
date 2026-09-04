//! The IDR grid a session's segments land on.
//!
//! Split out of `hls.rs` on 2026-09-04 (step 3 of
//! `nightjar-meta/docs/plans/2026-09-04-splitting-hls-rs.md`). ADR-0052 makes
//! the 2 s cadence per encode leg and derives it from the source frame rate,
//! and ADR-0051 decision 2 rests on every rung deriving the same one.
//!
//! `snap_plan_to_grid` stays in `hls.rs`: it takes a `StartPlan`, and keeping
//! that dependency out is worth thirteen lines.

use nightjar_core::VideoEncodePlan;

use crate::hls::{SEGMENT_MS, SessionMode};

/// What grid a session's listing can name.
///
/// **Three answers, because there are three cases and one of them used to be
/// invisible.** This was an `Option<u64>` whose `None` meant *"copy, no derived
/// cadence by design"* and *"transcode, no honest grid"* at once.
/// `full_title_entries` could only see the absence, so it gave a transcode
/// session copy's keyframe walk: a playlist naming source keyframe times that
/// a transcode encoder never writes. Every request held to `SEGMENT_WAIT` and
/// **129 of 1814 items in a real library could not play.**
///
/// Rule 4.11 asks which field distinguishes two cases rather than which branch.
/// The field is session mode, and ADR-0054 decision 2 already says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GridCadence {
    /// Copy and remux. They place no IDRs, so their listing is the greedy walk
    /// of the source keyframe map (ADR-0054 decision 2).
    KeyframeWalk,
    /// Transcode, on a grid the listing can name and the encoder will hit.
    Cadence(u64),
    /// Transcode with no listable grid: no source rate, or a rounding whose
    /// drift outruns the key snap before the title ends. **Not copy's walk** —
    /// the run's own window listing is the honest answer.
    NoHonestGrid,
}

impl GridCadence {
    /// The cadence when there is one. Callers that only need a grid point.
    pub(crate) fn cadence(self) -> Option<u64> {
        match self {
            Self::Cadence(c) => Some(c),
            Self::KeyframeWalk | Self::NoHonestGrid => None,
        }
    }
}

pub(crate) fn grid_cadence_ms(
    mode: SessionMode,
    has_burn_in: bool,
    leg: &crate::EncodeLeg,
    plan: &VideoEncodePlan,
    title_ms: u64,
) -> GridCadence {
    // Burn-in re-encodes video whatever the session mode says (ADR-0018), so
    // it is transcode for this question.
    let transcode = mode == SessionMode::Transcode || has_burn_in;
    if !transcode {
        return GridCadence::KeyframeWalk;
    }
    match produced_segment_ms(leg, plan, title_ms) {
        Some(c) => GridCadence::Cadence(c),
        None => GridCadence::NoHonestGrid,
    }
}

/// The producer's own first key is late by a couple of frames at land 0, and
/// every later key in that run inherits the offset. Measured through the
/// transcode start path: 83 ms at `24000/1001`, 80 at `25`, 33 at `60`
/// ([`crate::hls_segment_map::snap_to_cadence`] records the method). Runs at a
/// land above zero discard those frames and are exact, so this is the worst
/// case rather than the usual one.
const FIRST_SEGMENT_OFFSET_MS: u64 = 83;

/// Milliseconds of media in one produced segment, for the leg that will produce
/// it, on the integer grid the listing can name.
///
/// **Two legs, two answers, and the branch was missing.** Which arguments a leg
/// gets decides where its keys land, and this took no leg at all until
/// 2026-08-31.
///
/// **A leg that honours `-force_key_frames` is given
/// `expr:gte(t,n_forced*2.0)`, so its key is the first frame at or after
/// `n × SEGMENT_MS`.** The grid is `SEGMENT_MS`, and the producer sits within
/// one frame period of it for the life of the title — bounded, not
/// accumulating. Measured 2026-08-31 on `libx264` and `h264_videotoolbox`,
/// byte-identical starts on both, matching `ceil(n · 2.0 · fps) / fps` to three
/// decimals out to segment 200.
///
/// **A leg that discards the flag is given `-g <frames>`**, so its cadence is
/// the frame count in milliseconds. That is rational, the listing is integers,
/// so it rounds and then checks the rounding survives the title.
///
/// The residual per segment is `|N - r·D| / D` for `N = frames · den · 1000`
/// and `D = num`, so over a title it is `|N - r·D| · title / N` — integer
/// throughout. Add [`FIRST_SEGMENT_OFFSET_MS`], which spends the same budget,
/// and compare against the snap bound.
///
/// **`None` refuses rather than inventing a grid.** No source rate; a frame
/// period that would not fit inside the snap bound on its own; or a rounding
/// whose drift outruns that bound before the title ends. `22018000/918487` is
/// the last of those: 23.972 rather than 23.976, `2002.3334 ms` for 48 frames,
/// about 1199 ms of drift over two hours.
///
/// **The frame-count arm is inferred, not measured here.** Only `h264_qsv`
/// takes it and the N150 was unreachable on 2026-08-31. What it rests on is
/// the QSV measurement already in this file's history — 1061 of 1062 starts
/// off the 2000 ms grid at `c43b440`, modal delta 2002 — which is consistent
/// with `-g 48` at `24000/1001` and is not the same thing as having run it.
pub(crate) fn produced_segment_ms(
    leg: &crate::EncodeLeg,
    plan: &VideoEncodePlan,
    title_ms: u64,
) -> Option<u64> {
    let (num, den) = plan.source_frame_rate?;
    let (num, den) = (u64::from(num), u64::from(den));
    if num == 0 {
        return None;
    }
    let budget = |cadence: u64| cadence / crate::hls_segment_map::KEY_SNAP_DIVISOR;

    if leg.honours_force_key_frames {
        // One frame period is the whole error, and it never grows. Refuse only
        // when a frame is itself wider than the snap allows.
        let frame_ms = den.saturating_mul(1000) / num;
        return (frame_ms <= budget(SEGMENT_MS)).then_some(SEGMENT_MS);
    }

    let frames = u64::from(plan.gop_frames(SEGMENT_MS)?);
    let ms_numerator = frames * den * 1000;
    let cadence = (ms_numerator + num / 2) / num;
    if cadence == 0 {
        return None;
    }
    let residual = ms_numerator.abs_diff(cadence * num);
    if residual == 0 {
        return Some(cadence);
    }
    let drift_ms = residual.saturating_mul(title_ms) / ms_numerator;
    (drift_ms + FIRST_SEGMENT_OFFSET_MS <= budget(cadence)).then_some(cadence)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cadence a leg will actually produce, which is not always the one
    /// the session asked for — and which leg it is decides the answer.
    #[test]
    fn produced_segment_ms_follows_the_leg_then_the_frames() {
        let plan_at = |num, den| VideoEncodePlan {
            source_frame_rate: Some((num, den)),
            ..VideoEncodePlan::default()
        };
        let film = plan_at(24000, 1001);
        const FILM: u64 = 7_200_000;
        // libx264 and VideoToolbox honour `-force_key_frames`; QSV does not.
        let sw = crate::EncodeLeg::software();
        let vt = crate::EncodeLeg::videotoolbox();
        let qsv = crate::EncodeLeg::qsv_sysmem();

        // **A leg that honours the flag cuts at the first frame at or after
        // `n × SEGMENT_MS`, so the grid is SEGMENT_MS whatever the rate.**
        // Measured 2026-08-31 on both such legs, byte-identical starts,
        // matching `ceil(n · 2.0 · fps) / fps` to three decimals out to
        // segment 200.
        for leg in [&sw, &vt] {
            assert_eq!(produced_segment_ms(leg, &film, FILM), Some(SEGMENT_MS));
            // The rate does not move it. This is the whole point: the two
            // spellings of 23.976 both list the same grid, and so does 25.
            assert_eq!(
                produced_segment_ms(leg, &plan_at(2997, 125), FILM),
                Some(SEGMENT_MS)
            );
            assert_eq!(
                produced_segment_ms(leg, &plan_at(25, 1), FILM),
                Some(SEGMENT_MS)
            );
        }

        // **A leg that discards the flag gets `-g <frames>`, so its cadence is
        // the frame count.** 48 frames at 24000/1001 is 2002 ms.
        assert_eq!(
            produced_segment_ms(&qsv, &film, FILM),
            Some(2002),
            "48 frames at 24000/1001 is 2002 ms when the flag is discarded"
        );
        assert_eq!(
            produced_segment_ms(&qsv, &plan_at(60, 1), FILM),
            Some(2000),
            "120 frames"
        );
        assert_eq!(
            produced_segment_ms(&qsv, &plan_at(25, 1), FILM),
            Some(2000),
            "50 frames"
        );

        // No rate is no answer on either arm: the frame period cannot be
        // bounded, so the caller falls back to a per-run listing.
        for leg in [&sw, &qsv] {
            assert_eq!(
                produced_segment_ms(leg, &VideoEncodePlan::default(), FILM),
                None,
                "no source rate means no honest cadence"
            );
        }

        // A frame wider than the snap budget cannot be absorbed even by the
        // flag-honouring arm, because the offset is a whole frame.
        assert_eq!(
            produced_segment_ms(&sw, &plan_at(2, 1), FILM),
            None,
            "at 2 fps one frame is 500 ms, twice the 250 ms budget"
        );
    }
    /// **ADR-0051 decision 2: all three rungs transcode on the same 2 s IDR
    /// grid.** A ladder is only switchable if a segment boundary in one
    /// rendition is a segment boundary in every other, so this is the property
    /// the whole ladder rests on — and S6 is not scoped yet, which is why the
    /// assertion lands before the feature rather than after it.
    ///
    /// **It holds structurally, and this pins the reason.** The grid comes
    /// from [`produced_segment_ms`] and [`VideoEncodePlan::gop_frames`], and
    /// both read `source_frame_rate` and nothing else. **The three fields a
    /// rung varies — `max_height`, `max_bitrate_bps` and `tone_map` — are
    /// never consulted.** One session, one source, one rate, so one grid.
    ///
    /// **What this does not prove.** That two real encoder processes place
    /// IDRs on identical frames. That is a property of FFmpeg and the driver,
    /// not of this arithmetic, and it needs two legs run over one source and
    /// their boundaries compared. `-sc_threshold 0` on the
    /// `honours_force_key_frames` arm removes the obvious way it could differ
    /// — a scene cut detected at 6 Mbps and not at 2 — and the frame-count arm
    /// pins `keyint_min` to `-g`, but neither is the same as having measured
    /// it.
    ///
    /// **Naming the guard's reach:** what fails here is a change that makes
    /// the grid depend on a per-rung field. A change to how FFmpeg is invoked
    /// per rung will not fail here and is not covered.
    #[test]
    fn every_rung_of_a_ladder_derives_one_grid() {
        // ADR-0051 decision 2's rungs. One source, so one rate for all three.
        let rate = Some((24000u32, 1001u32));
        let rung = |max_height, max_bitrate_bps| VideoEncodePlan {
            max_height,
            max_bitrate_bps,
            source_frame_rate: rate,
            ..VideoEncodePlan::default()
        };
        let ladder = [
            ("high 1080p @ 6M", rung(Some(1080), Some(6_000_000))),
            ("mid  1080p @ 3M", rung(Some(1080), Some(3_000_000))),
            ("low   720p @ 2M", rung(Some(720), Some(2_000_000))),
        ];
        const FILM: u64 = 7_200_000;

        for leg in [
            &crate::EncodeLeg::software(),
            &crate::EncodeLeg::videotoolbox(),
            &crate::EncodeLeg::qsv_sysmem(),
        ] {
            let mut grids = ladder.iter().map(|(name, p)| {
                (
                    *name,
                    produced_segment_ms(leg, p, FILM),
                    p.gop_frames(SEGMENT_MS),
                )
            });
            let (first_name, first_ms, first_gop) = grids.next().expect("three rungs");
            assert!(
                first_ms.is_some(),
                "{}: {first_name} has no grid",
                leg.encoder
            );
            for (name, ms, gop) in grids {
                assert_eq!(
                    ms, first_ms,
                    "{}: {name} lists a different segment cadence from {first_name}; \
                     a boundary in one rung would not be a boundary in another",
                    leg.encoder
                );
                assert_eq!(
                    gop, first_gop,
                    "{}: {name} derives a different -g from {first_name}",
                    leg.encoder
                );
            }
        }
    }
    /// The control for the test above, and the reason it is a separate one:
    /// **an assertion that everything is equal passes just as well when the
    /// function ignores its input entirely.**
    ///
    /// So this fails the same comparison on the one field a rung must never
    /// change — the source rate — and does it on the frame-count arm, which is
    /// the only arm where the rate moves the answer. On a leg that honours
    /// `-force_key_frames` every rate lists `SEGMENT_MS`, so that arm cannot
    /// tell an ignored input from an equal one.
    #[test]
    fn the_grid_does_move_when_the_source_rate_does() {
        let plan_at = |num, den| VideoEncodePlan {
            max_height: Some(1080),
            max_bitrate_bps: Some(6_000_000),
            source_frame_rate: Some((num, den)),
            ..VideoEncodePlan::default()
        };
        let qsv = crate::EncodeLeg::qsv_sysmem();
        const FILM: u64 = 7_200_000;

        assert_ne!(
            produced_segment_ms(&qsv, &plan_at(24000, 1001), FILM),
            produced_segment_ms(&qsv, &plan_at(500, 21), FILM),
            "23.976 and 23.8095 must not derive the same cadence, or the \
             equality above proves nothing"
        );
        assert_ne!(
            plan_at(24000, 1001).gop_frames(SEGMENT_MS),
            plan_at(60, 1).gop_frames(SEGMENT_MS),
            "24 and 60 fps must not derive the same -g"
        );
    }
    /// The rates a real 1814-item library holds, on the leg whose arithmetic
    /// they exercise.
    ///
    /// **Pinned against the population that found entry 18, not two
    /// examples.** Every rate below is within a thousandth of 23.976 and they
    /// are written 72 different ways. **They only matter on the `-g` arm**:
    /// a leg that honours `-force_key_frames` lists SEGMENT_MS for all of
    /// them, which is asserted above.
    ///
    /// **That arm is inferred, not measured here.** Only `h264_qsv` takes it
    /// and the N150 was unreachable on 2026-08-31.
    #[test]
    fn the_rate_corpus_derives_a_listable_cadence_on_the_frame_count_arm() {
        let plan_at = |num, den| VideoEncodePlan {
            source_frame_rate: Some((num, den)),
            ..VideoEncodePlan::default()
        };
        let qsv = crate::EncodeLeg::qsv_sysmem();
        const FILM: u64 = 7_200_000;

        let corpus: &[(u32, u32, Option<u64>, &str)] = &[
            (24000, 1001, Some(2002), "1419 items"),
            (24, 1, Some(2000), "220 items"),
            (25, 1, Some(2000), "32 items"),
            (13978, 583, Some(2002), "26 items: 2002.0031"),
            (
                2997,
                125,
                Some(2002),
                "17 items: 2002.0020, the one that found entry 18",
            ),
            (250000, 10427, Some(2002), "14 items: 2001.9840"),
            (27021, 1127, Some(2002), "12 items: 2001.9984"),
            (30000, 1001, Some(2002), "8 items"),
            (30, 1, Some(2000), "3 items"),
            (500, 21, Some(2016), "2 items: 23.8095"),
            (2997, 100, Some(2002), "1 item: 29.97"),
            (12060, 503, Some(2002), "1 item: 2001.9900"),
            (14937, 623, Some(2002), "1 item: 2002.0084"),
            (743630848, 30985109, Some(2000), "1 item: 2000.0317"),
            (2146177256, 89512761, Some(2002), "the long-tail shape"),
            // **Refused, and this is the finding rather than a gap.**
            // 23.9720, not 23.976: 2002.3334 ms for 48 frames, about 1199 ms
            // of drift over this title against a 250 ms budget.
            (22018000, 918487, None, "the one rate rounding cannot carry"),
        ];

        for &(num, den, want, why) in corpus {
            assert_eq!(
                produced_segment_ms(&qsv, &plan_at(num, den), FILM),
                want,
                "{num}/{den} ({why})"
            );
        }
    }
    /// The drift budget, both sides, on the arm that spends it.
    #[test]
    fn the_drift_budget_admits_and_refuses_either_side_of_its_bound() {
        let qsv = crate::EncodeLeg::qsv_sysmem();
        let rate = VideoEncodePlan {
            source_frame_rate: Some((2997, 125)),
            ..VideoEncodePlan::default()
        };
        // 0.002 ms per segment against 250 ms of budget less the 83 ms
        // first-segment offset: 167 ms spendable, so about 83_500 segments.
        assert_eq!(
            produced_segment_ms(&qsv, &rate, 100_000_000),
            Some(2002),
            "a 27-hour title still fits the budget at this residual"
        );
        assert_eq!(
            produced_segment_ms(&qsv, &rate, 200_000_000),
            None,
            "past the budget the rounding is refused, not stretched"
        );
    }
    #[test]
    fn gop_frames_follow_the_source_rate() {
        let plan_at = |num, den| VideoEncodePlan {
            source_frame_rate: Some((num, den)),
            ..VideoEncodePlan::default()
        };
        // 23.976 fps: 48 frames is 2.002 s, not 2 s. 2000 ms is 47.952
        // frames, so no frame count hits the grid at this rate, and this
        // comment claimed the opposite until 2026-08-30. That claim is why
        // nobody checked: measured on the N150 at c43b440, 1061 of 1062
        // segment starts were off the 2000 ms grid, modal delta 2002 ms.
        // `produced_segment_ms` is the honest cadence; `gop_frames` is only
        // the frame count that produces it.
        assert_eq!(plan_at(24000, 1001).gop_frames(2000), Some(48));
        assert_eq!(plan_at(24, 1).gop_frames(2000), Some(48));
        // 60 fps needs 120. A hardcoded 48 would cut every 0.8 s here.
        assert_eq!(plan_at(60, 1).gop_frames(2000), Some(120));
        assert_eq!(plan_at(30000, 1001).gop_frames(2000), Some(60));
        assert_eq!(plan_at(25, 1).gop_frames(2000), Some(50));
        // The interval follows SEGMENT_MS, not a second copy of it.
        assert_eq!(plan_at(24000, 1001).gop_frames(4000), Some(96));
        // No rate means no honest answer, and the caller must not invent one.
        assert_eq!(VideoEncodePlan::default().gop_frames(2000), None);
        assert_eq!(plan_at(0, 1).gop_frames(2000), None);
        assert_eq!(plan_at(24, 0).gop_frames(2000), None);
    }
}
