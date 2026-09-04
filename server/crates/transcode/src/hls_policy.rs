//! The decisions a session makes, as pure functions.
//!
//! Split out of `hls.rs` on 2026-09-04 (step 2 of
//! `nightjar-meta/docs/plans/2026-09-04-splitting-hls-rs.md`). Everything here
//! takes scalars and returns a verdict: no `Session`, no `Child`, no path, no
//! lock. That is what makes it separable, and it is why these are the functions
//! the table tests can exercise exhaustively.
//!
//! The arithmetic they reason on stays in `hls.rs`, imported below, because it
//! is shared with the machinery that acts on these verdicts.

use std::time::Duration;

use crate::hls::{
    ALIGN_BEHIND_SEGMENTS, CATCH_UP_SEGMENTS, RESTART_COALESCE_QUIET, RESTART_MIN_INTERVAL,
    SEGMENT_MS, align_to_segment, encode_start_ms,
};

/// Pure window-move decision for an explicit playlist `?startMs=` seek.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// Target on disk, or already cooking at this window — serve/wait.
    Serve,
    /// Restart FFmpeg at the aligned offset.
    Restart,
}

pub fn decide_window_action(
    requested_ms: u64,
    window_start_ms: u64,
    target_on_disk: bool,
) -> WindowAction {
    let aligned = align_to_segment(requested_ms);
    if target_on_disk || aligned == window_start_ms {
        WindowAction::Serve
    } else {
        WindowAction::Restart
    }
}

/// What to do when a segment is missing from disk (ADR-0011 amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentMissAction {
    /// In-window cooking, restart suppressed, or too soon since last restart.
    Wait,
    /// Move the encode window to the requested index.
    Restart,
}

/// What to do when a listed segment is not on disk.
///
/// **Overturned 2026-08-30 — a cold listed URI starts an encoder.** ADR-0054
/// decision 3: *"A listed URI is never 404. … A cold URI is a seek: the
/// session starts an encoder at that media time."*
///
/// The quotation used to read *"never 404 and never 503"*, which is the
/// sentence ADR-0054 withdrew on 2026-08-31: the hold in [`asset_wait`] is
/// bounded by `SEGMENT_WAIT` and answers 503 on expiry, deliberately, because
/// 503 is recoverable where 404 makes hls.js and Safari abandon the fragment.
/// **Nothing about this function changes** — it is the 404 half that bears on
/// a miss — but a citation that outlives its source is how a withdrawn claim
/// gets read as current.
///
/// This function returned `Wait` unconditionally, discarding all six of its
/// arguments, and its comment stated that as policy. It is kept here because
/// it is what is being reversed, not a detail being adjusted:
///
/// > *Deliberate miss policy under ADR-0020 producer-truth playlists.*
/// >
/// > *Segment GETs never move the encode window. Far scrub is
/// > `POST /sessions/{id}/seek`. A miss is always Wait: listed-but-not-ready
/// > cooks under fill-forward; unlisted URIs are 404'd by the asset path once
/// > unreachable. The old behind-play Restart band ([`ALIGN_BEHIND_SEGMENTS`])
/// > and ahead-of-frontier Restart past [`CATCH_UP_SEGMENTS`] fitted WebKit
/// > requesting URIs the synthetic full-title VOD listed but the producer
/// > never wrote — that playlist is gone.*
///
/// **That playlist is coming back**, and its last clause is why the policy has
/// to go with it. Fill-forward reaches a want only while every listed URI sits
/// near the running producer, which is true of a one-window listing and false
/// of a full-title one. For a want no run is heading towards, `Wait` never
/// ends: the hold runs to [`IDLE_TIMEOUT`] and returns an empty 204.
///
/// `Restart` for a want this run cannot reach:
///
/// - **behind the encode window** — this run produces forward from
///   `window_start_ms` and never goes back;
/// - **past the catch-up band** — further ahead of the producer's frontier
///   than [`CATCH_UP_SEGMENTS`], so waiting is not a cook, it is a stall.
///
/// `Wait` otherwise, and `Wait` always while the run is still starting
/// (`!primed`, where the frontier is not yet meaningful) or inside
/// [`RESTART_MIN_INTERVAL`] of the last restart, which is what stops a
/// prefetching client turning a listing into a restart storm.
pub fn decide_segment_miss(
    want_ms: u64,
    window_start_ms: u64,
    play_start_ms: u64,
    latest_on_disk_ms: Option<u64>,
    primed: bool,
    since_last_restart: Duration,
) -> SegmentMissAction {
    // A run that has served nothing yet has no honest frontier, and the want
    // is usually its own land arriving. `play_start_ms` is what it is heading
    // for, so a want at or after it is that case and not a miss to act on.
    if !primed && want_ms >= align_to_segment(play_start_ms) {
        return SegmentMissAction::Wait;
    }
    if since_last_restart < RESTART_MIN_INTERVAL {
        return SegmentMissAction::Wait;
    }
    let want = align_to_segment(want_ms);
    if want < align_to_segment(window_start_ms) {
        return SegmentMissAction::Restart;
    }
    let frontier = latest_on_disk_ms.unwrap_or(window_start_ms);
    if want.saturating_sub(frontier) > CATCH_UP_SEGMENTS * SEGMENT_MS {
        return SegmentMissAction::Restart;
    }
    SegmentMissAction::Wait
}

/// How a scrub intent interacts with an in-flight or just-landed encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalesceDesire {
    /// Already cooking or serving this play land.
    Nop,
    /// Record pending; apply when cooking land is ready, or earlier when
    /// [`coalesce_preempt_before_land`] allows (far pending + min interval).
    HoldInFlight,
    /// Record pending; apply after [`RESTART_COALESCE_QUIET`] of quiet.
    HoldDebounce,
}

/// Classify a scrub toward `want_play_ms` without mutating session state.
/// Used by [`desire_restart`] and unit tests (three rapid desires → last
/// pending, one apply).
pub fn classify_restart_desire(
    want_play_ms: u64,
    play_start_ms: u64,
    encode_start_ms_now: u64,
    first_segment_ready: bool,
) -> CoalesceDesire {
    let aligned = align_to_segment(want_play_ms);
    if encode_start_ms(aligned) == encode_start_ms_now && aligned == play_start_ms {
        return CoalesceDesire::Nop;
    }
    if !first_segment_ready {
        CoalesceDesire::HoldInFlight
    } else {
        CoalesceDesire::HoldDebounce
    }
}

/// Whether a recorded pending play land is due to apply.
///
/// When the cooking land is not ready yet, apply only if `allow_preempt`,
/// [`coalesce_preempt_before_land`] says the pending target is far outside
/// the near-land band, and [`RESTART_MIN_INTERVAL`] has elapsed since the
/// last restart (anti-thrash on the preempt path only — not a substitute
/// for the land gate). Near pending must still wait for the cooking land
/// (dogfood: seg415 after scrub to 1188 — yank before land left Safari
/// retrying the prior URI).
///
/// `allow_preempt` mirrors [`disable_preempt`]: unset leaves preempt **on**.
/// Pass `!disable_preempt()` from production callers.
pub fn pending_restart_due(
    first_segment_ready: bool,
    pending_play_ms: Option<u64>,
    pending_quiet_elapsed: Option<Duration>,
    apply_immediate: bool,
    cooking_play_ms: u64,
    since_last_restart: Duration,
    allow_preempt: bool,
) -> Option<u64> {
    let pending = pending_play_ms?;
    if !first_segment_ready {
        if allow_preempt
            && coalesce_preempt_before_land(cooking_play_ms, pending)
            && since_last_restart >= RESTART_MIN_INTERVAL
        {
            return Some(pending);
        }
        return None;
    }
    if apply_immediate {
        return Some(pending);
    }
    let elapsed = pending_quiet_elapsed?;
    if elapsed < RESTART_COALESCE_QUIET {
        return None;
    }
    Some(pending)
}

/// `NIGHTJAR_DISABLE_PREEMPT=1` (or `true`/`yes`): never preempt before land.
/// Unset (and any value other than an explicit disable) leaves preempt **on** —
/// the polarity measured as scrub-before-play pass under Config D.
pub(crate) fn disable_preempt() -> bool {
    matches!(
        std::env::var("NIGHTJAR_DISABLE_PREEMPT").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

/// Optional pause after kill before the next FFmpeg spawn (`restart_at`).
/// `NIGHTJAR_RESTART_SPAWN_GAP_MS` — distinct from [`RESTART_MIN_INTERVAL`]
/// (decision gate). Used to probe whether rapid dual-init boundaries wedge
/// Safari while keeping preempt's fast target selection.
pub(crate) fn restart_spawn_gap() -> Option<Duration> {
    let ms: u64 = std::env::var("NIGHTJAR_RESTART_SPAWN_GAP_MS")
        .ok()?
        .parse()
        .ok()?;
    if ms == 0 {
        None
    } else {
        Some(Duration::from_millis(ms))
    }
}

/// Far pending may abandon an in-flight cook before its land exists.
///
/// Under ADR-0020 [`ALIGN_BEHIND_SEGMENTS`] is 0 (dig-back band deleted), so
/// any different pending land is "far" and may preempt. Near-identical
/// retargets (same aligned ms) still wait for the cooking land.
pub fn coalesce_preempt_before_land(cooking_play_ms: u64, pending_play_ms: u64) -> bool {
    let cooking = align_to_segment(cooking_play_ms);
    let pending = align_to_segment(pending_play_ms);
    if pending == cooking {
        return false;
    }
    let segs = if pending > cooking {
        (pending - cooking) / SEGMENT_MS
    } else {
        (cooking - pending) / SEGMENT_MS
    };
    segs > ALIGN_BEHIND_SEGMENTS
}

/// Whether a no-fill hold on `want_ms` should end with 503 once the committed
/// play land is ready. Land-ensure 200 does not fill WebKit's buffer; a held
/// dig-back GET can leave the player seeking with zero native land fetches
/// (desktop-native single scrub: seg126 held while land-ensure got seg129).
///
/// Release when the want will never fill under the new encode window
/// (`want` behind `encode_window_start_ms`), or when it is **far** behind
/// play. Near dig-back still inside the lead-in window stays held — lead
/// may still write it. Ahead-of-play / attach-window misses must not use
/// this path (that 503'd in-flight land-ensure while play was still 0).
pub fn no_fill_release_for_new_land(
    want_ms: u64,
    play_start_ms: u64,
    first_segment_ready: bool,
    encode_window_start_ms: u64,
) -> bool {
    if !first_segment_ready {
        return false;
    }
    let want = align_to_segment(want_ms);
    let play = align_to_segment(play_start_ms);
    if want >= play {
        return false;
    }
    let window = align_to_segment(encode_window_start_ms);
    if want < window {
        return true;
    }
    coalesce_preempt_before_land(want, play)
}

/// Missing segment that current policy will not [`desire_restart`] toward.
/// Callers **hold** the connection instead of 503/404 while the session lives.
///
/// **Behind the window is decided by whether the playlist offered the URI**,
/// not by position alone. Position alone shadowed [`decide_segment_miss`],
/// which returns `Restart` for the same position under ADR-0054 decision 3 —
/// *"a cold URI is a seek"* — so the escape hatch below could never be
/// reached, and a want below the window was refused forever. Measured on
/// Safari native (99 asks in a minute, each refused in ~142 ms), on hls.js,
/// and arriving unprovoked on an iPhone.
///
/// **The two listings want opposite answers**, and `want_listed` is what
/// separates them:
///
/// - **ADR-0020's per-run listing** never offered a URI behind this run's
///   window. Nothing promised it, so holding is right — and 404ing an
///   unlisted want stays right, which is what
///   `held_segment_waiter_no_fill_when_pending_moves` pins.
/// - **ADR-0054's full-title listing does offer it.** Refusing a URI the
///   playlist names is the defect, and decision 3 already says a seek is what
///   should happen instead.
///
/// **Not a staleness detector, deliberately.** A live backward scrub and a
/// want orphaned by a newer seek are indistinguishable from here: both retry
/// while the client keeps filling its old buffer — 27 refusals against 9,
/// with healthy traffic alongside each. The only difference is that the stale
/// one eventually stops, and waiting to find out *is* the stall. So it is
/// decided on the cost of being wrong: a restart nothing reads costs one
/// encode start, bounded by [`RESTART_MIN_INTERVAL`] and `desire_restart`'s
/// coalescing, while a hold costs the session.
#[allow(clippy::too_many_arguments)]
pub fn segment_miss_unreachable(
    want_ms: u64,
    cooking_play_ms: u64,
    pending_play_ms: Option<u64>,
    window_start_ms: u64,
    play_start_ms: u64,
    latest_on_disk_ms: Option<u64>,
    primed: bool,
    // Did this session's playlist list `want_ms`? See `want_is_listed`.
    want_listed: bool,
) -> bool {
    let want = align_to_segment(want_ms);
    let window = align_to_segment(window_start_ms);

    // Playlist scrub pending this exact land — about to cook.
    if pending_play_ms.is_some_and(|p| align_to_segment(p) == want) {
        return false;
    }

    // Behind the encode window and never offered: lead-in / fill-forward will
    // not write this index and no playlist promised it. (Dig-back within ALIGN
    // of a *new* play can still be behind that play's window — that is
    // abandoned, not in-window dig-back.)
    if want < window && !want_listed {
        return true;
    }

    // In-window dig-back near committed land: lead-in may still write it.
    if digback_behind_committed(cooking_play_ms, pending_play_ms, want) {
        return false;
    }

    let cool = decide_segment_miss(
        want,
        window,
        play_start_ms,
        latest_on_disk_ms,
        primed,
        RESTART_MIN_INTERVAL,
    );
    if cool == SegmentMissAction::Restart {
        return false;
    }

    // In window and Wait: fill-forward will produce it.
    false
}

/// Segment-miss desire that only nudges an existing pending land a few
/// segments forward is Safari prefetch, not a new scrub. Returns true when
/// the miss should **not** call [`desire_restart`].
///
/// Consults **pending only**, never cooking `play_start_ms`: a deliberate
/// short forward scrub (one segment) and buffer-ahead look identical on the
/// segment path alone; short scrubs land via playlist `?startMs=` instead.
pub fn prefetch_advances_pending(pending_play_ms: Option<u64>, want_play_ms: u64) -> bool {
    let Some(pending) = pending_play_ms else {
        return false;
    };
    let pending = align_to_segment(pending);
    let want = align_to_segment(want_play_ms);
    if want <= pending {
        return false;
    }
    (want - pending) / SEGMENT_MS <= CATCH_UP_SEGMENTS
}

/// Segment miss behind the committed land (cooking play and/or pending).
/// Under ADR-0020 any behind-committed GET is dig-back / stale — do not call
/// [`desire_restart`] (far scrub is `POST /seek`). The old
/// [`ALIGN_BEHIND_SEGMENTS`] near-band is deleted with the synthetic VOD.
pub fn digback_behind_committed(
    cooking_play_ms: u64,
    pending_play_ms: Option<u64>,
    want_play_ms: u64,
) -> bool {
    let want = align_to_segment(want_play_ms);
    let cooking = align_to_segment(cooking_play_ms);
    let committed = match pending_play_ms {
        Some(p) => align_to_segment(p).max(cooking),
        None => cooking,
    };
    want < committed
}

/// Whether a long-poll for `want_play_ms` should keep holding for fill or
/// treat the want as superseded (pending moved to a different scrub).
///
/// Superseded waiters use the no-fill hold (same as abandoned misses): no
/// 503/404 while the session lives. Dig-back pending a few segments *behind*
/// this land still counts as Hold — do not starve the deliberate land
/// waiter for a near-ALIGN steal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingWaiterAction {
    Hold,
    Release,
}

/// Whether a long-poll for `want_play_ms` should keep holding for fill or
/// treat the want as superseded (pending moved to a different scrub).
///
/// Exact pending match Holds. Any other want Releases — the old ALIGN near
/// band for dig-back pending is gone with producer-truth playlists.
pub fn pending_waiter_action(
    pending_play_ms: Option<u64>,
    want_play_ms: u64,
) -> PendingWaiterAction {
    let Some(pending) = pending_play_ms else {
        return PendingWaiterAction::Hold;
    };
    let pending = align_to_segment(pending);
    let want = align_to_segment(want_play_ms);
    if pending == want {
        PendingWaiterAction::Hold
    } else {
        PendingWaiterAction::Release
    }
}

/// Whether bytes read for a segment request may still be returned after
/// [`note_first_segment_ready`] / [`maybe_apply_pending_restart`].
///
/// If `play_start_ms` moved away from this request's land, the bytes belong
/// to the pre-apply window — 503 so the client retries. If play moved *to*
/// this request's land (`want_ms`), the bytes are the new land and must
/// still 200 (land-ensure for the final scrub). Pure helper for serve + tests.
pub fn serve_ok_after_pending_apply(
    play_before_ms: u64,
    play_after_ms: u64,
    want_ms: Option<u64>,
) -> bool {
    if play_before_ms == play_after_ms {
        return true;
    }
    match want_ms {
        Some(want) => align_to_segment(want) == align_to_segment(play_after_ms),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tables exercise the lead; production reads it in `hls.rs`.
    use crate::hls::ENCODE_LEAD_SEGMENTS;

    #[test]
    fn window_decision_table() {
        // (name, requested_ms, window_start_ms, on_disk, expected)
        let cases = [
            ("on disk is serve", 10_000, 0, true, WindowAction::Serve),
            (
                "same window cooking is serve",
                2000,
                2000,
                false,
                WindowAction::Serve,
            ),
            (
                "divergent offset restarts",
                10_000,
                0,
                false,
                WindowAction::Restart,
            ),
            (
                "aligns request before compare",
                2500,
                2000,
                false,
                WindowAction::Serve,
            ),
        ];
        for (name, req, window, on_disk, expected) in cases {
            assert_eq!(
                decide_window_action(req, window, on_disk),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn segment_miss_decision_table() {
        let cool = RESTART_MIN_INTERVAL;
        let hot = Duration::from_millis(0);
        // Overturned 2026-08-30. Every row said Wait under ADR-0020's "segment
        // GETs never Restart". Three of them are now Restart, because a
        // full-title listing names URIs fill-forward cannot reach and Wait on
        // those never ends. The two that still Wait are the guards that keep
        // a prefetching client from restart-storming.
        let cases = [
            (
                "behind window restarts: this run never produces it",
                0,
                600,
                616,
                None,
                false,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "just behind the window still restarts: direction, not distance",
                0,
                4,
                4,
                Some(10),
                true,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "far ahead of frontier restarts: waiting there is a stall",
                20,
                4,
                4,
                Some(10),
                true,
                cool,
                SegmentMissAction::Restart,
            ),
            (
                "near frontier waits (cooking)",
                11,
                4,
                4,
                Some(10),
                true,
                cool,
                SegmentMissAction::Wait,
            ),
            (
                "hot restart interval still waits",
                20,
                4,
                4,
                Some(10),
                true,
                hot,
                SegmentMissAction::Wait,
            ),
        ];
        for (name, idx, window, play, latest, primed, since, want) in cases {
            assert_eq!(
                decide_segment_miss(
                    idx * SEGMENT_MS,
                    window * SEGMENT_MS,
                    play * SEGMENT_MS,
                    latest.map(|l| l * SEGMENT_MS),
                    primed,
                    since,
                ),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn miss_restarts_behind_the_window_and_keeps_the_seek_arithmetic() {
        // These three were `Wait` under ADR-0020's miss policy. Each is a want
        // behind its run's encode window, which that run never produces, so
        // each is now a Restart. The `encode_start_ms` assertion below is what
        // this test was also guarding and is unchanged.
        let cases = [(1040u64, 1052u64), (0u64, 4u64), (1610u64, 1614u64)];
        for (idx, window) in cases {
            let action = decide_segment_miss(
                idx * SEGMENT_MS,
                window * SEGMENT_MS,
                window * SEGMENT_MS,
                Some(window * SEGMENT_MS),
                true,
                RESTART_MIN_INTERVAL,
            );
            assert_eq!(action, SegmentMissAction::Restart, "idx={idx}");
            let want_ms = idx * SEGMENT_MS;
            let new_window = encode_start_ms(want_ms) / SEGMENT_MS;
            assert_eq!(
                new_window,
                idx.saturating_sub(ENCODE_LEAD_SEGMENTS),
                "encode_start still defined for seek path"
            );
        }
    }

    /// Dogfood incident timing: three scrub intents (1084s → 1840s → 2454s)
    /// while the first encode is still landing. Only the last pending applies
    /// when ready — one follow-up restart, not three racing kills.
    #[test]
    fn rapid_restart_intents_coalesce_to_last_target() {
        let targets = [1_084_000u64, 1_840_000, 2_454_000];
        let mut play = 0u64;
        let mut encode = 0u64;
        let mut ready = false;
        let mut pending: Option<u64> = None;
        let mut apply_count = 0u32;

        for &want in &targets {
            let phase = classify_restart_desire(want, play, encode, ready);
            assert_eq!(phase, CoalesceDesire::HoldInFlight, "want={want}");
            pending = Some(align_to_segment(want));
        }
        assert_eq!(pending, Some(2_454_000));

        // First encode lands at the initial scrub target.
        ready = true;
        play = 1_084_000;
        encode = encode_start_ms(play);
        let due = pending_restart_due(ready, pending, None, true, play, RESTART_MIN_INTERVAL, true);
        assert_eq!(due, Some(2_454_000));
        // Apply once to the last intent.
        if let Some(p) = due {
            apply_count += 1;
            play = p;
            encode = encode_start_ms(p);
            pending = None;
        }
        assert_eq!(apply_count, 1);
        assert_eq!(play, 2_454_000);
        assert_eq!(encode, encode_start_ms(2_454_000));
        assert_eq!(encode, 2_454_000 - ENCODE_LEAD_SEGMENTS * SEGMENT_MS);

        // Debounce after land: three quick intents → one apply after quiet.
        ready = true;
        let burst = [2_500_000u64, 2_600_000, 2_700_000];
        for &want in &burst {
            assert_eq!(
                classify_restart_desire(want, play, encode, ready),
                CoalesceDesire::HoldDebounce
            );
            pending = Some(align_to_segment(want));
        }
        assert_eq!(
            pending_restart_due(
                ready,
                pending,
                Some(Duration::from_millis(100)),
                false,
                play,
                RESTART_MIN_INTERVAL,
                true
            ),
            None,
            "quiet not elapsed"
        );
        let due2 = pending_restart_due(
            ready,
            pending,
            Some(RESTART_COALESCE_QUIET),
            false,
            play,
            RESTART_MIN_INTERVAL,
            true,
        );
        assert_eq!(due2, Some(2_700_000));
    }

    /// ADR-0020: lead is 0, so encode window start equals play land. Any
    /// different pending land is "far" (ALIGN dig-back band deleted) and may
    /// preempt after RESTART_MIN_INTERVAL; same-land pending never preempts.
    #[test]
    fn pending_preempt_policy_under_producer_truth() {
        let cooking_play = 1_188_000u64;
        assert_eq!(encode_start_ms(cooking_play), cooking_play);
        assert!(
            !coalesce_preempt_before_land(cooking_play, cooking_play),
            "same land is not preempt"
        );
        let near_fwd = cooking_play + SEGMENT_MS;
        assert!(
            coalesce_preempt_before_land(cooking_play, near_fwd),
            "any different land is preempt-eligible"
        );
        // Before cooking land is ready: far pending may apply only after
        // RESTART_MIN_INTERVAL (see far_pending_preempts_before_land…).
        assert_eq!(
            pending_restart_due(
                false,
                Some(near_fwd),
                None,
                false,
                cooking_play,
                Duration::from_millis(0),
                true
            ),
            None,
            "hot clock: no preempt"
        );
        assert_eq!(
            pending_restart_due(
                false,
                Some(near_fwd),
                None,
                true,
                cooking_play,
                RESTART_MIN_INTERVAL * 2,
                true
            ),
            Some(near_fwd),
            "cool clock + far pending: preempt"
        );
        // Land ready → pending may apply.
        assert_eq!(
            pending_restart_due(
                true,
                Some(near_fwd),
                None,
                true,
                cooking_play,
                RESTART_MIN_INTERVAL,
                true
            ),
            Some(near_fwd)
        );
    }

    /// Rapid B then far C while B still cooking: after RESTART_MIN_INTERVAL,
    /// C preempts without waiting for B's land (Bug 1 third gate). Middle
    /// cook must not block the third target for a full land.
    #[test]
    fn far_pending_preempts_before_land_after_min_interval() {
        let land_b = 1_494_000u64;
        let land_c = 2_070_000u64;
        assert!(
            coalesce_preempt_before_land(land_b, land_c),
            "far C: beyond ALIGN_BEHIND from B"
        );
        // Too soon after B's restart: anti-thrash holds preempt.
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                Duration::from_millis(470),
                true
            ),
            None,
            "preempt still gated by RESTART_MIN_INTERVAL"
        );
        // Interval elapsed, B's land still missing: apply C.
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                RESTART_MIN_INTERVAL,
                true
            ),
            Some(land_c),
            "far C applies before B land once interval cools"
        );
        // Product default (allow_preempt=false): far pending stays held until land.
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                RESTART_MIN_INTERVAL,
                false,
            ),
            None,
            "allow_preempt=false never preempts before cooking land"
        );
    }

    #[test]
    fn no_fill_releases_far_mid_once_new_land_ready() {
        let mid = 258_000u64;
        let land = 748_000u64;
        // lead=0 ⇒ encode window == play land.
        let window = land - ENCODE_LEAD_SEGMENTS * SEGMENT_MS;
        assert_eq!(window, land);
        assert!(
            !no_fill_release_for_new_land(mid, land, false, window),
            "still cooking: keep no-fill hold"
        );
        assert!(
            no_fill_release_for_new_land(mid, land, true, window),
            "far mid after land ready: 503 so WebKit leaves dig-back"
        );
        assert!(
            !no_fill_release_for_new_land(land, land, true, window),
            "want is the play land: do not release"
        );
        assert!(
            !no_fill_release_for_new_land(land, 0, true, 0),
            "ahead of attach play: must not 503"
        );
        // Behind window (lead=0: one seg behind land) → release.
        let behind_window = land - SEGMENT_MS;
        assert!(
            no_fill_release_for_new_land(behind_window, land, true, window),
            "behind encode window after land ready: release"
        );
    }

    /// Selecting which pending land is due is unchanged by a seek keeping the
    /// prior encoder: the gate that used to defer the kill is gone, but the
    /// due decision it sat beside is not.
    #[test]
    fn pending_restart_selects_the_far_land() {
        let land_b = 1_494_000u64;
        let land_c = 2_070_000u64;
        assert_eq!(
            pending_restart_due(
                false,
                Some(land_c),
                Some(Duration::from_millis(470)),
                false,
                land_b,
                RESTART_MIN_INTERVAL,
                true,
            ),
            Some(land_c),
        );
    }

    /// Prefetch seg ahead of an existing pending land must not advance it.
    /// Intentional short forward (startMs / desire at L+1 with no pending)
    /// must still be accepted — never clamp against cooking play_start alone.
    #[test]
    fn prefetch_does_not_advance_pending_short_scrub_via_start_ms_does() {
        let land = 100_000u64; // seg050
        let pending = Some(land);
        assert!(
            prefetch_advances_pending(pending, land + SEGMENT_MS),
            "L+1 miss is prefetch yank"
        );
        assert!(
            prefetch_advances_pending(pending, land + CATCH_UP_SEGMENTS * SEGMENT_MS),
            "L+CATCH_UP miss is prefetch yank"
        );
        assert!(
            !prefetch_advances_pending(pending, land + (CATCH_UP_SEGMENTS + 1) * SEGMENT_MS),
            "far ahead replaces pending"
        );
        assert!(
            !prefetch_advances_pending(pending, land.saturating_sub(SEGMENT_MS)),
            "behind pending is a real dig-back"
        );
        assert!(
            !prefetch_advances_pending(None, land + SEGMENT_MS),
            "no pending: segment path unchanged (short scrub lands via startMs)"
        );

        // startMs-shaped short forward while cooking at L: desire accepts L+1.
        let play = land;
        let encode = encode_start_ms(play);
        let short = land + SEGMENT_MS;
        assert_eq!(
            classify_restart_desire(short, play, encode, true),
            CoalesceDesire::HoldDebounce,
            "intentional short forward is a real land, not prefetch noise"
        );
        let mut pending_after = Some(align_to_segment(short));
        assert_eq!(pending_after, Some(short));
        // Once pending is L+1, a further +1 prefetch must not advance again.
        assert!(prefetch_advances_pending(pending_after, short + SEGMENT_MS));
        // Simulate applying the startMs land (pending becomes cooking).
        let _ = pending_after.take();
        assert!(!prefetch_advances_pending(None, short + SEGMENT_MS));
    }

    #[test]
    fn pending_waiter_holds_match_releases_mismatch() {
        let land = 100_000u64;
        assert_eq!(
            pending_waiter_action(None, land),
            PendingWaiterAction::Hold,
            "no pending: keep polling the cooking window"
        );
        assert_eq!(
            pending_waiter_action(Some(land), land),
            PendingWaiterAction::Hold,
            "pending matches this request"
        );
        assert_eq!(
            pending_waiter_action(Some(land), land + SEGMENT_MS),
            PendingWaiterAction::Release,
            "any non-exact want is superseded"
        );
        assert_eq!(
            pending_waiter_action(Some(land + 20_000), land),
            PendingWaiterAction::Release,
            "pending is a different scrub ahead"
        );
        assert_eq!(
            pending_waiter_action(Some(land), land + 60_000),
            PendingWaiterAction::Release,
            "pending far behind want: real supersede, release"
        );
    }

    #[test]
    fn refuse_serve_when_pending_apply_moved_play_land() {
        let land_a = 622_000u64;
        let land_b = 1_078_000u64;
        assert!(
            serve_ok_after_pending_apply(land_a, land_a, Some(land_a)),
            "same land: serve retained/cooked bytes"
        );
        assert!(
            !serve_ok_after_pending_apply(land_a, land_b, Some(land_a)),
            "play moved during request: do not return prior-land bytes"
        );
        assert!(
            serve_ok_after_pending_apply(land_a, land_b, Some(land_b)),
            "play moved to this request's land: land-ensure must still 200"
        );
        assert!(
            !serve_ok_after_pending_apply(land_a, land_b, None),
            "non-segment asset: play move refuses"
        );
    }

    #[test]
    fn digback_behind_committed_blocks_behind_land_steal() {
        // Any want behind committed is dig-back (no segment-path restart).
        let cooking = 1_482_000u64;
        let dig = 1_478_000u64;
        let far = cooking - 60_000;
        assert!(digback_behind_committed(cooking, None, dig));
        assert!(digback_behind_committed(cooking, None, far));
        assert!(
            !digback_behind_committed(cooking, None, cooking),
            "same land is not dig-back"
        );
        assert!(
            !digback_behind_committed(cooking, None, cooking + SEGMENT_MS),
            "ahead is not dig-back"
        );

        let cooking_b = 1_054_000u64;
        let pending_c = 1_482_000u64;
        assert!(digback_behind_committed(cooking_b, Some(pending_c), dig));
        assert!(
            !digback_behind_committed(cooking_b, Some(pending_c), pending_c),
            "want == committed pending"
        );
    }

    /// Segment miss never Restarts; dig-back still blocks desire_restart.
    #[test]
    fn scrub_shaped_digback_must_not_desire() {
        let cooking = 1_482_000u64;
        let dig = 1_478_000u64;
        let idx = dig / SEGMENT_MS;
        let window = cooking / SEGMENT_MS;
        let play = window;
        assert_eq!(
            decide_segment_miss(
                idx * SEGMENT_MS,
                window * SEGMENT_MS,
                play * SEGMENT_MS,
                Some(window * SEGMENT_MS),
                true,
                RESTART_MIN_INTERVAL,
            ),
            SegmentMissAction::Restart,
            "a dig-back want is behind the window, so the miss decision is Restart"
        );
        assert!(
            digback_behind_committed(cooking, None, dig),
            "and the dig-back guard is what still decides, at the call site"
        );
    }

    /// Abandoned miss predicate: far behind / prior land → hold path; dig-back
    /// and pending land stay reachable (503 cook / desire).
    #[test]
    fn segment_miss_unreachable_table() {
        let cooking = 1_482_000u64;
        let window_ms = encode_start_ms(cooking);
        let play = cooking;
        let latest = Some(window_ms / SEGMENT_MS);
        let prior = 473 * SEGMENT_MS;
        let far = cooking - 60_000;
        let dig = cooking - 2 * SEGMENT_MS;
        let in_window = window_ms;
        let ahead = cooking + (CATCH_UP_SEGMENTS + 2) * SEGMENT_MS;

        // lead=0 ⇒ window == cooking, so every "behind" row here is behind the
        // window. What decides them is the sixth column: **did the playlist
        // list the want**.
        //
        // Unlisted rows are ADR-0020's per-run listing, where nothing ever
        // offered a URI behind this run's window — those stay unreachable and
        // are unchanged. Listed rows are ADR-0054's full-title listing, where
        // the playlist does name it, and decision 3 says a cold listed URI is
        // a seek: they became reachable on 2026-08-31, which is entry 13.
        /// name, want, pending, primed, listed, expect_unreachable
        type Case = (&'static str, u64, Option<u64>, bool, bool, bool);
        let cases: &[Case] = &[
            ("far behind, unlisted", far, None, true, false, true),
            (
                "prior land after jump, unlisted",
                prior,
                None,
                true,
                false,
                true,
            ),
            ("attach-shaped seg000, unlisted", 0, None, true, false, true),
            (
                "behind land dig-back, unlisted",
                dig,
                None,
                true,
                false,
                true,
            ),
            ("unprimed far, unlisted", far, None, false, false, true),
            // The same four positions once the playlist lists them. This is
            // the defect: a URI the session offered was refused forever.
            ("far behind, LISTED", far, None, true, true, false),
            (
                "prior land after jump, LISTED",
                prior,
                None,
                true,
                true,
                false,
            ),
            ("attach-shaped seg000, LISTED", 0, None, true, true, false),
            ("behind land dig-back, LISTED", dig, None, true, true, false),
            // Unprimed is still not a reason to hold a listed want: the run
            // has no honest frontier yet, and `decide_segment_miss` owns that.
            ("unprimed far, LISTED", far, None, false, true, false),
            // Unchanged, and none of them turn on the listing.
            ("pending exact land", prior, Some(prior), true, false, false),
            (
                "in-window fill-forward",
                in_window,
                None,
                true,
                false,
                false,
            ),
            (
                "ahead of frontier (seek owns scrub)",
                ahead,
                None,
                true,
                false,
                false,
            ),
        ];
        for &(name, want, pending, primed, listed, expect_unreachable) in cases {
            assert_eq!(
                segment_miss_unreachable(
                    want, cooking, pending, window_ms, play, latest, primed, listed
                ),
                expect_unreachable,
                "{name}"
            );
        }
    }
}
