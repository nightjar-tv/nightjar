//! Per-rung time-keyed HLS segment map (ADR-0020, ADR-0051).
//!
//! Producer runs write `segNNN.m4s` under `v<rung>/run_<n>/`. This module
//! parses each run's honest `index.m3u8`, gates entries with
//! `sidx.earliest_presentation_time`, and stores them under title-absolute
//! start milliseconds. Served URIs are `seg_<ms:011>.m4s` (milliseconds,
//! zero-padded).

use crate::hls_master::VideoRung;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Wire name for a segment whose media starts at `start_ms` (title-absolute).
pub fn time_keyed_segment_name(start_ms: u64) -> String {
    format!("seg_{start_ms:011}.m4s")
}

/// Parse `seg_00001277151.m4s` → start_ms. Rejects the old `segNNN.m4s` form.
pub fn parse_time_keyed_segment_name(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("seg_")?.strip_suffix(".m4s")?;
    if rest.len() != 11 || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// On-disk rung directory relative to the session directory (ADR-0051).
pub fn rung_rel_dir(rung: VideoRung) -> PathBuf {
    PathBuf::from(format!("v{}", rung.as_str()))
}

/// On-disk producer run directory relative to the session directory.
///
/// Ingest and lifecycle code share this composer so a rung path cannot be
/// written in one shape and later searched or evicted in another.
pub fn run_rel_dir(rung: VideoRung, run_id: u64) -> PathBuf {
    rung_rel_dir(rung).join(format!("run_{run_id}"))
}

/// Minimal fMP4 with a video (ref_id=1) sidx v0; timescale 1000 → ms.
///
/// Lives here rather than in a test module so `hls.rs` can write producer
/// files too: the gate [`ingest_run_index`] applies is a sidx read, so a test
/// that writes an `index.m3u8` has to write bytes that carry one.
#[cfg(test)]
pub(crate) fn fake_sidx_seg(earliest_ms: u32) -> Vec<u8> {
    let mut body = vec![0u8; 20];
    body[4..8].copy_from_slice(&1u32.to_be_bytes());
    body[8..12].copy_from_slice(&1000u32.to_be_bytes());
    body[12..16].copy_from_slice(&earliest_ms.to_be_bytes());
    let size = (8 + body.len()) as u32;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(b"sidx");
    out.extend_from_slice(&body);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedSegment {
    pub start_ms: u64,
    pub duration_ms: u64,
    pub run_id: u64,
    /// Relative to the session dir, e.g. `vsingle/run_0/seg042.m4s`.
    pub rel_path: PathBuf,
}

#[derive(Debug, Default)]
pub struct SegmentMap {
    /// Title-absolute start_ms → segment. One entry per start; a newer run
    /// that produces a different packing at the same start replaces the
    /// prior entry (bytes remain under the old run until eviction).
    by_start: BTreeMap<u64, MappedSegment>,
}

impl SegmentMap {
    pub fn get(&self, start_ms: u64) -> Option<&MappedSegment> {
        self.by_start.get(&start_ms)
    }

    #[allow(dead_code)] // map API for eviction / future callers
    pub fn contains(&self, start_ms: u64) -> bool {
        self.by_start.contains_key(&start_ms)
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.by_start.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }

    /// Segments whose media interval intersects `[window_start, window_end)`.
    #[allow(dead_code)]
    pub fn overlapping(&self, window_start_ms: u64, window_end_ms: u64) -> Vec<&MappedSegment> {
        self.by_start
            .values()
            .filter(|s| {
                let end = s.start_ms.saturating_add(s.duration_ms);
                s.start_ms < window_end_ms && end > window_start_ms
            })
            .collect()
    }

    /// All segments in start-time order (for playlist assembly).
    pub fn iter_ordered(&self) -> impl DoubleEndedIterator<Item = &MappedSegment> {
        self.by_start.values()
    }

    /// Drop every map entry belonging to `run_id` (after that run dir is evicted).
    pub fn remove_run(&mut self, run_id: u64) {
        self.by_start.retain(|_, s| s.run_id != run_id);
    }

    /// Drop a single start key (file gone under an otherwise live run).
    pub fn remove_start(&mut self, start_ms: u64) {
        self.by_start.remove(&start_ms);
    }

    /// Run ids that still back at least one map entry (authoritative for eviction).
    pub fn referenced_run_ids(&self) -> std::collections::BTreeSet<u64> {
        self.by_start.values().map(|s| s.run_id).collect()
    }

    /// True when any map entry points at this run.
    pub fn run_is_referenced(&self, run_id: u64) -> bool {
        self.by_start.values().any(|s| s.run_id == run_id)
    }

    pub fn insert(&mut self, seg: MappedSegment) {
        self.by_start.insert(seg.start_ms, seg);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FfmpegIndexEntry {
    pub file_name: String,
    pub extinf_secs: f64,
}

/// Parse FFmpeg's HLS media playlist into ordered EXTINF + file pairs.
pub fn parse_ffmpeg_index(text: &str) -> Result<Vec<FfmpegIndexEntry>, String> {
    let mut out = Vec::new();
    let mut pending_extinf: Option<f64> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest
                .split(',')
                .next()
                .ok_or_else(|| format!("bad EXTINF line: {line}"))?;
            let secs: f64 = dur
                .parse()
                .map_err(|_| format!("bad EXTINF duration in: {line}"))?;
            pending_extinf = Some(secs);
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let secs = pending_extinf
            .take()
            .ok_or_else(|| format!("segment URI without EXTINF: {line}"))?;
        out.push(FfmpegIndexEntry {
            file_name: line.to_string(),
            extinf_secs: secs,
        });
    }
    Ok(out)
}

/// Read `sidx` earliest_presentation_time for track ref_id 1 (video), in ms.
pub fn sidx_video_earliest_ms(seg: &[u8]) -> Result<u64, String> {
    let mut off = 0usize;
    while off + 8 <= seg.len() {
        let size = u32::from_be_bytes(seg[off..off + 4].try_into().unwrap()) as usize;
        let typ = &seg[off + 4..off + 8];
        if size < 8 || off + size > seg.len() {
            break;
        }
        if typ == b"sidx" {
            let body = &seg[off + 8..off + size];
            if body.len() < 20 {
                return Err("sidx too short".into());
            }
            let version = body[0];
            let ref_id = u32::from_be_bytes(body[4..8].try_into().unwrap());
            if ref_id == 1 {
                let timescale = u32::from_be_bytes(body[8..12].try_into().unwrap());
                if timescale == 0 {
                    return Err("sidx timescale 0".into());
                }
                let earliest = if version == 0 {
                    if body.len() < 16 {
                        return Err("sidx v0 too short".into());
                    }
                    u64::from(u32::from_be_bytes(body[12..16].try_into().unwrap()))
                } else {
                    if body.len() < 20 {
                        return Err("sidx v1 too short".into());
                    }
                    u64::from_be_bytes(body[12..20].try_into().unwrap())
                };
                return Ok((earliest * 1000) / u64::from(timescale));
            }
        }
        off += size;
    }
    Err("no video sidx".into())
}

/// When mid-start encode stamps title-absolute `sidx` (FFmpeg 7+/8 with
/// `-output_ts_offset`), first earliest ≈ `encode_start_ms`. Ubuntu apt 6.1
/// still emits encode-relative sidx from 0 despite that flag — shift onto the
/// encode window so time-keyed URIs stay title-absolute (ADR-0020).
pub fn sidx_title_offset_ms(encode_start_ms: u64, first_sidx_ms: u64) -> u64 {
    if encode_start_ms > 0 && first_sidx_ms < encode_start_ms / 2 {
        encode_start_ms
    } else {
        0
    }
}

/// How a producer key is put onto the listing's keys.
///
/// Transcode's listing is a regular cadence, so a key rounds onto a multiple.
/// Copy's is the keyframe walk, which is irregular by construction, so a key
/// rounds onto the nearest listed point.
#[derive(Debug, Clone, Copy)]
pub enum KeySnap<'a> {
    Cadence(u64),
    Points(&'a [u64]),
}

/// How far a copy key may sit from the point the walk listed.
///
/// **Measured 2026-08-30 on the N150 through the session API, two titles.**
/// `Camp Rock 3` (h264+aac Matroska, pure `-c copy`) and `Birder` (the same
/// with a 5.1 downmix), 18 segments between them: the producer's sidx is
/// **exactly 1 ms below** the keyframe `pts_ms` the walk lists, on every
/// window after the first, and the first is exact. Distinct offsets across the
/// run: `[-1, 0]` and nothing else.
///
/// It is a rounding difference between the map's stored keyframe time and the
/// sidx the muxer stamps, not drift — it does not accumulate.
///
/// **100 ms is two orders of magnitude above that measurement**, half a
/// percent of a [`super::hls::COPY_WINDOW_MS`] window, and nowhere near far
/// enough to reach a neighbouring listed point. Beyond it the key is not this
/// rounding and the snap refuses.
///
/// **The listing is not adjusted to match.** Emitting `pts_ms - 1` would put a
/// rounding artefact into the wire format and be wrong the moment the
/// truncation changes.
pub const COPY_KEY_TOLERANCE_MS: u64 = 100;

/// Round a producer key onto the nearest listed point, or refuse.
///
/// The copy half of [`snap_to_cadence`]. Same contract: within the bound it
/// returns the listed key, beyond it `None`, and the caller keeps the
/// producer's own key and says so.
pub fn snap_to_points(key_ms: u64, points: &[u64]) -> Option<u64> {
    let nearest = match points.binary_search(&key_ms) {
        Ok(_) => return Some(key_ms),
        Err(i) => {
            let before = i.checked_sub(1).and_then(|j| points.get(j)).copied();
            let after = points.get(i).copied();
            match (before, after) {
                (Some(b), Some(a)) => {
                    if key_ms - b <= a - key_ms {
                        b
                    } else {
                        a
                    }
                }
                (Some(b), None) => b,
                (None, Some(a)) => a,
                (None, None) => return None,
            }
        }
    };
    (key_ms.abs_diff(nearest) <= COPY_KEY_TOLERANCE_MS).then_some(nearest)
}

/// A producer key may sit this fraction of a segment from the listed multiple.
///
/// **One divisor, two callers.** [`snap_to_cadence`] uses it to decide whether
/// a key rounds onto the listing, and `produced_segment_ms` uses it to decide
/// whether a rounded cadence is listable at all — the second spends the budget
/// the first enforces, so they must not drift apart.
pub const KEY_SNAP_DIVISOR: u64 = 8;

/// Round a producer key onto the run's cadence, or refuse.
///
/// **The producer's first segment is not always where it was asked to start.**
/// Measured through the transcode crate's own start path, at land 0 with no
/// seek to discard the encoder's priming frames, the first sidx lands **two
/// video frames late** and every later key inherits the same offset: 83 ms at
/// `24000/1001` (keys 83, 2085, 4087), 80 ms at `25`, 33 ms at `60`. At any
/// land above zero the `-ss` discards those frames and the keys are exact.
///
/// Nothing we pass ffmpeg moves it. Both candidate fixes were measured and
/// refused to: passing `-output_ts_offset 0` at land 0, and passing a nominal
/// `-ss 0`. So the wire key is snapped here instead of chased there.
///
/// **The bound is asserted, not assumed.** A correction beyond an eighth of a
/// segment is not this offset — it is drift, or a cadence that does not match
/// the producer — and this returns `None` so the caller keeps the producer's
/// own key and says so. At `SEGMENT_MS` that bound is 250 ms, three times the
/// largest offset measured, and far inside half a segment.
pub fn snap_to_cadence(key_ms: u64, cadence_ms: u64) -> Option<u64> {
    if cadence_ms == 0 {
        return None;
    }
    let nearest = key_ms.div_ceil(cadence_ms) * cadence_ms;
    let below = (key_ms / cadence_ms) * cadence_ms;
    let nearest = if key_ms - below <= nearest - key_ms {
        below
    } else {
        nearest
    };
    (key_ms.abs_diff(nearest) <= cadence_ms / KEY_SNAP_DIVISOR).then_some(nearest)
}

/// Ingest one producer run's `index.m3u8` into `map`.
///
/// For each EXTINF entry, reads the segment file, requires contiguous
/// `sidx` timeline (within 1 ms after any title offset), and inserts a
/// time-keyed map entry. Disagreement skips that segment (hard failure to
/// publish — never map wrong content).
///
/// `encode_start_ms` is the run's `-ss` / window start (written beside the
/// producer files). Used only to detect encode-relative sidx on older FFmpeg.
pub fn ingest_run_index(
    map: &mut SegmentMap,
    session_dir: &Path,
    rung: VideoRung,
    run_id: u64,
    index_text: &str,
    encode_start_ms: u64,
    snap: Option<KeySnap<'_>>,
) -> Result<usize, String> {
    let entries = parse_ffmpeg_index(index_text)?;
    let run_rel = run_rel_dir(rung, run_id);
    let mut inserted = 0usize;
    // FFmpeg EXTINF starts are relative to the first packet after seek; with
    // a working `-output_ts_offset` the sidx carries title-absolute time. We
    // trust (possibly offset) sidx for the key and EXTINF only for duration.
    let mut prev_end_ms: Option<u64> = None;
    let mut title_offset_ms: Option<u64> = None;
    for entry in entries {
        let rel = run_rel.join(&entry.file_name);
        let abs = session_dir.join(&rel);
        let bytes = fs::read(&abs).map_err(|e| format!("read {}: {e}", abs.display()))?;
        let sidx_ms = sidx_video_earliest_ms(&bytes)?;
        let duration_ms = (entry.extinf_secs * 1000.0).round() as u64;
        if duration_ms == 0 {
            continue;
        }
        let offset = *title_offset_ms.get_or_insert_with(|| {
            let off = sidx_title_offset_ms(encode_start_ms, sidx_ms);
            if off > 0 {
                tracing::info!(
                    run_id,
                    encode_start_ms,
                    first_sidx_ms = sidx_ms,
                    title_offset_ms = off,
                    "hls map: encode-relative sidx; applying title offset"
                );
            }
            off
        });
        let raw_start_ms = sidx_ms.saturating_add(offset);

        // Gate: after the first segment, the producer's own start should equal
        // its own previous end within one millisecond.
        //
        // **Both sides are the producer's values, and that is the whole
        // point.** The question is whether this segment's content follows the
        // last one — "contiguous producer output", which is what ADR-0020's
        // original implementation asked and what closes false-time mapping.
        //
        // **It asked it in two coordinate systems from #182 to 2026-08-31.**
        // The key snap landed between the raw value and this check, so a
        // *snapped* start was compared against a *raw* end. That requires the
        // grid spacing to equal the produced duration, which nothing
        // guarantees; it held only because 2002 is the modal `EXTINF` at
        // `24000/1001`. **41,062 segments were skipped from the map on
        // `a2d0d73` against a real library** — 1,322 triggers, every one of
        // them 21, 40 or 42 ms, at or under one frame period, and 39,740
        // cascade behind them because a skip does not advance the expected
        // end. Not one was a genuine discontiguity.
        //
        // **What the snap guarantees is a different thing and guards itself.**
        // That a stored key is close enough to the listed key to be named by
        // it is `snap_to_cadence`'s bound, which refuses and warns on its own.
        // Two invariants, two guards, each in one coordinate system.
        if let Some(expect) = prev_end_ms {
            let delta = raw_start_ms.abs_diff(expect);
            if delta > 1 {
                tracing::warn!(
                    run_id,
                    file = %entry.file_name,
                    sidx_ms,
                    raw_start_ms,
                    expect_ms = expect,
                    delta_ms = delta,
                    "hls map-build gate: sidx disagrees with EXTINF timeline; skipping"
                );
                continue;
            }
        }
        prev_end_ms = Some(raw_start_ms.saturating_add(duration_ms));

        // Put the wire key on the run's cadence, so a full-title listing can
        // name it before the producer has written it. Out of bound, keep the
        // producer's key and say so: the listing will then hold on a URI it
        // named, which is visible, rather than mapping content to a time it
        // does not have.
        let snapped = snap.and_then(|policy| match policy {
            KeySnap::Cadence(c) => snap_to_cadence(raw_start_ms, c),
            KeySnap::Points(points) => snap_to_points(raw_start_ms, points),
        });
        let start_ms = match snapped {
            Some(snapped) => snapped,
            None => {
                if snap.is_some() {
                    tracing::warn!(
                        run_id,
                        file = %entry.file_name,
                        raw_start_ms,
                        policy = ?snap,
                        "producer key is further from the listing than the \
                         measured rounding explains; keeping it unsnapped"
                    );
                }
                raw_start_ms
            }
        };
        map.insert(MappedSegment {
            start_ms,
            duration_ms,
            run_id,
            rel_path: rel,
        });
        inserted += 1;
    }
    Ok(inserted)
}

/// Build an EVENT (or ENDLIST) media playlist from ordered map segments.
///
/// `init_uri` is the EXT-X-MAP URI (run-relative or session-absolute).
/// Media playlist for a set of `(start_ms, duration_ms)` entries.
///
/// **Always `VOD` with `ENDLIST`.** It was `EVENT` without one until
/// 2026-08-30, because the listing grew between fetches: it named the segments
/// that existed, so a client had to be told to expect more. A full-title
/// listing names every entry from the first fetch and only the backing files
/// arrive, so there is nothing left for `EVENT` to describe.
///
/// Entries are `(start_ms, duration_ms)` rather than [`MappedSegment`] because
/// a full-title listing names URIs before any run has written them, and a
/// mapped segment is by definition already on disk.
///
/// `start_offset_ms` is where a fresh attach begins, title-absolute. It was
/// always `0` while the listing began at the land, because the window's start
/// *was* the land. **A full-title listing begins at 0, so a zero offset would
/// attach at the title start rather than where the session landed** — the two
/// have to be said separately now that they differ.
pub fn build_map_playlist(entries: &[(u64, u64)], init_uri: &str, start_offset_ms: u64) -> Vec<u8> {
    use std::fmt::Write;
    let target = entries
        .iter()
        .map(|(_, duration_ms)| ((*duration_ms as f64) / 1000.0).ceil() as u64)
        .max()
        .unwrap_or(2)
        .max(1);
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-MAP:URI=\"{init_uri}\"\n\
         #EXT-X-START:TIME-OFFSET={start_secs:.3},PRECISE=YES\n",
        start_secs = start_offset_ms as f64 / 1000.0
    );
    for (start_ms, duration_ms) in entries {
        let secs = *duration_ms as f64 / 1000.0;
        let _ = writeln!(
            out,
            "#EXTINF:{secs:.6},\n{}",
            time_keyed_segment_name(*start_ms)
        );
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Copy's producer key rounds onto the point the walk listed.
    #[test]
    fn copy_keys_snap_onto_the_listed_walk_and_refuse_beyond_it() {
        // The walk for a 20 s window on keyframes that are not on a grid.
        let points = [0u64, 20_020, 40_040, 60_060, 80_080];

        // Measured on the N150, two titles, 18 segments: the producer's sidx
        // is exactly 1 ms below the listed keyframe on every window after the
        // first, and the first is exact.
        assert_eq!(snap_to_points(0, &points), Some(0));
        assert_eq!(snap_to_points(20_019, &points), Some(20_020));
        assert_eq!(snap_to_points(40_039, &points), Some(40_040));
        assert_eq!(snap_to_points(60_059, &points), Some(60_060));
        assert_eq!(snap_to_points(80_079, &points), Some(80_080));

        // An exact key is itself.
        assert_eq!(snap_to_points(40_040, &points), Some(40_040));

        // The bound, asserted on both sides. 100 ms is two orders above the
        // measurement and half a percent of a window.
        assert_eq!(
            snap_to_points(20_020 + COPY_KEY_TOLERANCE_MS, &points),
            Some(20_020),
            "exactly the bound snaps"
        );
        assert_eq!(
            snap_to_points(20_020 + COPY_KEY_TOLERANCE_MS + 1, &points),
            None,
            "one past the bound is not this rounding"
        );
        assert_eq!(
            snap_to_points(30_000, &points),
            None,
            "halfway between two listed points is never a snap"
        );
        assert_eq!(snap_to_points(500_000, &points), None, "past the walk");
        assert_eq!(snap_to_points(5, &[]), None, "no listing, no snap");
    }

    /// The measured first-segment offset snaps; anything larger does not.
    #[test]
    fn snap_takes_the_first_segment_offset_and_refuses_drift() {
        // Measured through the transcode start path at land 0: the producer's
        // first sidx is two video frames late and every later key inherits it.
        // 24000/1001, cadence 2002, keys 83 / 2085 / 4087.
        assert_eq!(snap_to_cadence(83, 2002), Some(0));
        assert_eq!(snap_to_cadence(2085, 2002), Some(2002));
        assert_eq!(snap_to_cadence(4087, 2002), Some(4004));
        // 25 fps, cadence 2000, offset 80. 60 fps, offset 33.
        assert_eq!(snap_to_cadence(80, 2000), Some(0));
        assert_eq!(snap_to_cadence(2080, 2000), Some(2000));
        assert_eq!(snap_to_cadence(33, 2000), Some(0));
        // A key already on the cadence is unchanged.
        assert_eq!(snap_to_cadence(4004, 2002), Some(4004));
        assert_eq!(snap_to_cadence(0, 2002), Some(0));

        // The bound is an eighth of a segment, asserted on both sides so it
        // fails loudly rather than absorbing real drift. At 2000 that is 250.
        assert_eq!(
            snap_to_cadence(250, 2000),
            Some(0),
            "exactly the bound snaps"
        );
        assert_eq!(
            snap_to_cadence(251, 2000),
            None,
            "one past the bound is drift, not the first-segment offset"
        );
        assert_eq!(
            snap_to_cadence(1750, 2000),
            Some(2000),
            "the bound is symmetric below the next multiple"
        );
        assert_eq!(snap_to_cadence(1749, 2000), None);
        // Half a segment is never a snap.
        assert_eq!(snap_to_cadence(1000, 2000), None);
        assert_eq!(snap_to_cadence(5, 0), None, "no cadence, no snap");

        // **The far end of a rounded cadence, which is what entry 18's fix
        // spends this budget on.** At 2997/125 the true cadence is 2002.002
        // ms and the listing names multiples of 2002, so a producer key drifts
        // ~0.002 ms per segment away from its listed multiple. At segment 5000
        // of a long film that is 10 ms of drift on top of the 83 ms
        // first-segment offset, and it still snaps.
        let listed = 5000 * 2002;
        assert_eq!(
            snap_to_cadence(listed + 93, 2002),
            Some(listed),
            "83 ms offset plus 10 ms accumulated drift is inside the budget"
        );
        // And the budget is still the budget at that distance: 250 admits,
        // 251 does not, exactly as at segment zero.
        assert_eq!(snap_to_cadence(listed + 250, 2002), Some(listed));
        assert_eq!(
            snap_to_cadence(listed + 251, 2002),
            None,
            "past the bound a drifted key is not this rounding and must not \
             resolve to a neighbour"
        );
    }

    /// A segment URI's bytes are **not** immutable within a session.
    ///
    /// Written for the cache-header slice, where the question was whether a
    /// segment could carry a long `max-age` and `immutable`. It cannot: the
    /// map is keyed on title-absolute start and [`SegmentMap::insert`]
    /// replaces, so a later run that produces a different packing at the same
    /// start takes over that URI. The doc on `by_start` says so; this asserts
    /// it, because a header was about to rest on the opposite.
    #[test]
    fn a_segment_uri_is_not_immutable_within_a_session() {
        let mut map = SegmentMap::default();
        map.insert(MappedSegment {
            start_ms: 42_000,
            duration_ms: 2_000,
            run_id: 0,
            rel_path: PathBuf::from("vsingle/run_0/seg000.m4s"),
        });
        assert_eq!(
            map.get(42_000).map(|s| s.rel_path.clone()),
            Some(PathBuf::from("vsingle/run_0/seg000.m4s"))
        );

        // A later run produces its own packing at the same title start.
        map.insert(MappedSegment {
            start_ms: 42_000,
            duration_ms: 2_000,
            run_id: 3,
            rel_path: PathBuf::from("vsingle/run_3/seg007.m4s"),
        });

        let now = map.get(42_000).expect("the key still resolves");
        assert_eq!(now.run_id, 3, "the newer run owns the URI");
        assert_eq!(
            now.rel_path,
            PathBuf::from("vsingle/run_3/seg007.m4s"),
            "same URI, different bytes on disk — so no `immutable`, and no \
             max-age that outlives a replacement"
        );
        assert_eq!(map.len(), 1, "one entry per start, replaced not appended");
    }

    #[test]
    fn time_keyed_round_trip() {
        let name = time_keyed_segment_name(1_277_151);
        assert_eq!(name, "seg_00001277151.m4s");
        assert_eq!(parse_time_keyed_segment_name(&name), Some(1_277_151));
        assert_eq!(parse_time_keyed_segment_name("seg042.m4s"), None);
        assert_eq!(parse_time_keyed_segment_name("seg_1277151.m4s"), None);
    }

    #[test]
    fn sidx_title_offset_detects_encode_relative() {
        assert_eq!(sidx_title_offset_ms(40_000, 0), 40_000);
        assert_eq!(sidx_title_offset_ms(2_000, 0), 2_000);
        assert_eq!(sidx_title_offset_ms(40_000, 40_000), 0);
        assert_eq!(sidx_title_offset_ms(40_000, 40_080), 0);
        assert_eq!(sidx_title_offset_ms(0, 0), 0);
    }

    #[test]
    fn ingest_offsets_relative_sidx_onto_encode_start() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(run_rel_dir(VideoRung::SingleVideo, 0));
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("seg020.m4s"), fake_sidx_seg(0)).unwrap();
        fs::write(run.join("seg021.m4s"), fake_sidx_seg(2000)).unwrap();
        let index = "\
#EXTM3U
#EXTINF:2.000000,
seg020.m4s
#EXTINF:2.000000,
seg021.m4s
#EXT-X-ENDLIST
";
        let mut map = SegmentMap::default();
        let n = ingest_run_index(
            &mut map,
            dir.path(),
            VideoRung::SingleVideo,
            0,
            index,
            40_000,
            None,
        )
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(map.get(40_000).unwrap().duration_ms, 2000);
        assert_eq!(map.get(42_000).unwrap().duration_ms, 2000);
        assert!(map.get(0).is_none());
    }

    /// **The entry 21 case.** A run whose grid spacing differs from the
    /// durations it writes must ingest every segment.
    ///
    /// The producer here cuts at `2002` ms (`24000/1001`, 48 frames) while the
    /// listing names `2000` — which is what a leg honouring
    /// `-force_key_frames` actually does (entry 20). Every start is contiguous
    /// with the previous end in the producer's own values, so every segment
    /// belongs in the map. Before 2026-08-31 the gate compared the *snapped*
    /// start against the *raw* end and skipped all but the first.
    #[test]
    fn a_grid_that_differs_from_the_duration_still_ingests_every_segment() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(run_rel_dir(VideoRung::SingleVideo, 0));
        fs::create_dir_all(&run).unwrap();
        let mut index = String::from(
            "#EXTM3U
",
        );
        for n in 0..6u32 {
            let name = format!("seg{n:03}.m4s");
            fs::write(run.join(&name), fake_sidx_seg(n * 2002)).unwrap();
            index.push_str(&format!(
                "#EXTINF:2.002000,
{name}
"
            ));
        }
        index.push_str(
            "#EXT-X-ENDLIST
",
        );

        let mut map = SegmentMap::default();
        let n = ingest_run_index(
            &mut map,
            dir.path(),
            VideoRung::SingleVideo,
            0,
            &index,
            0,
            Some(KeySnap::Cadence(2000)),
        )
        .unwrap();
        assert_eq!(n, 6, "every segment ingests; the grid is not the duration");
        // Stored on the listing's grid, which is what the playlist names.
        for k in 0..6u64 {
            assert!(
                map.get(k * 2000).is_some(),
                "segment {k} must be stored at its listed key {}",
                k * 2000
            );
        }
    }

    /// **And a genuinely discontiguous run still skips.** Whatever the gate
    /// protects against has to survive moving it back into one coordinate
    /// system: segment 3's content does not follow segment 2's, so it must not
    /// be mapped to a time it does not have (ADR-0020 §9).
    #[test]
    fn a_discontiguous_producer_run_is_still_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(run_rel_dir(VideoRung::SingleVideo, 0));
        fs::create_dir_all(&run).unwrap();
        // 0, 2000, 4000, then a 500 ms hole, then contiguous again.
        let starts = [0u32, 2000, 4000, 6500, 8500];
        let mut index = String::from(
            "#EXTM3U
",
        );
        for (n, start) in starts.iter().enumerate() {
            let name = format!("seg{n:03}.m4s");
            fs::write(run.join(&name), fake_sidx_seg(*start)).unwrap();
            index.push_str(&format!(
                "#EXTINF:2.000000,
{name}
"
            ));
        }
        index.push_str(
            "#EXT-X-ENDLIST
",
        );

        let mut map = SegmentMap::default();
        let n = ingest_run_index(
            &mut map,
            dir.path(),
            VideoRung::SingleVideo,
            0,
            &index,
            0,
            None,
        )
        .unwrap();
        assert_eq!(n, 3, "everything up to the hole keeps");
        assert!(
            map.get(6500).is_none(),
            "content that does not follow is not mapped"
        );
        // **And the run does not recover.** A skip does not advance the
        // expected end, so every later segment is compared against the last
        // *accepted* one and fails by a further segment length. That is
        // unchanged by moving the gate, and it is what turned entry 21's 1,322
        // coordinate triggers into 41,062 skips. **Left alone deliberately**:
        // whether a genuine hole should drop the rest of a run is its own
        // question, and this change is about which coordinate system the gate
        // asks in, not about what it does once it refuses.
        assert!(map.get(8500).is_none(), "the cascade is existing behaviour");
    }

    /// **The `(a, a)` control.** A run whose spacing and duration agree ingests
    /// exactly as it did before the gate moved: same keys, same count.
    #[test]
    fn a_grid_that_matches_the_duration_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(run_rel_dir(VideoRung::SingleVideo, 0));
        fs::create_dir_all(&run).unwrap();
        let mut index = String::from(
            "#EXTM3U
",
        );
        for n in 0..5u32 {
            let name = format!("seg{n:03}.m4s");
            fs::write(run.join(&name), fake_sidx_seg(n * 2000)).unwrap();
            index.push_str(&format!(
                "#EXTINF:2.000000,
{name}
"
            ));
        }
        index.push_str(
            "#EXT-X-ENDLIST
",
        );

        let mut map = SegmentMap::default();
        let n = ingest_run_index(
            &mut map,
            dir.path(),
            VideoRung::SingleVideo,
            0,
            &index,
            0,
            Some(KeySnap::Cadence(2000)),
        )
        .unwrap();
        assert_eq!(n, 5);
        for k in 0..5u64 {
            assert_eq!(map.get(k * 2000).unwrap().duration_ms, 2000);
        }
    }

    #[test]
    fn ingest_keeps_absolute_sidx_unshifted() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join(run_rel_dir(VideoRung::SingleVideo, 0));
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("seg020.m4s"), fake_sidx_seg(40_000)).unwrap();
        fs::write(run.join("seg021.m4s"), fake_sidx_seg(42_000)).unwrap();
        let index = "\
#EXTM3U
#EXTINF:2.000000,
seg020.m4s
#EXTINF:2.000000,
seg021.m4s
#EXT-X-ENDLIST
";
        let mut map = SegmentMap::default();
        let n = ingest_run_index(
            &mut map,
            dir.path(),
            VideoRung::SingleVideo,
            0,
            index,
            40_000,
            None,
        )
        .unwrap();
        assert_eq!(n, 2);
        assert!(map.get(40_000).is_some());
        assert!(map.get(42_000).is_some());
    }

    #[test]
    fn parse_ffmpeg_index_basic() {
        let text = "\
#EXTM3U
#EXT-X-TARGETDURATION:5
#EXTINF:4.004000,
seg005.m4s
#EXTINF:2.000000,
seg006.m4s
#EXT-X-ENDLIST
";
        let entries = parse_ffmpeg_index(text).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file_name, "seg005.m4s");
        assert!((entries[0].extinf_secs - 4.004).abs() < 1e-6);
    }

    #[test]
    fn overlapping_and_remove_run() {
        let mut map = SegmentMap::default();
        map.insert(MappedSegment {
            start_ms: 1000,
            duration_ms: 1000,
            run_id: 0,
            rel_path: PathBuf::from("vsingle/run_0/seg000.m4s"),
        });
        map.insert(MappedSegment {
            start_ms: 5000,
            duration_ms: 1000,
            run_id: 1,
            rel_path: PathBuf::from("vsingle/run_1/seg000.m4s"),
        });
        assert_eq!(map.overlapping(0, 3000).len(), 1, "early window");
        assert_eq!(map.overlapping(4000, 7000).len(), 1, "late window");
        assert_eq!(map.overlapping(0, 7000).len(), 2, "full span");
        map.remove_run(0);
        assert_eq!(map.len(), 1);
        assert!(map.get(5000).is_some());
    }

    #[test]
    fn build_playlist_event_shape() {
        let segs = [
            MappedSegment {
                start_ms: 8008,
                duration_ms: 4004,
                run_id: 0,
                rel_path: PathBuf::from("vsingle/run_0/a.m4s"),
            },
            MappedSegment {
                start_ms: 12_012,
                duration_ms: 4004,
                run_id: 0,
                rel_path: PathBuf::from("vsingle/run_0/b.m4s"),
            },
        ];
        let entries: Vec<(u64, u64)> = segs.iter().map(|s| (s.start_ms, s.duration_ms)).collect();
        let bytes = build_map_playlist(&entries, "init.mp4", 0);
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.contains("#EXT-X-PLAYLIST-TYPE:VOD"),
            "every playlist is VOD; EVENT described a listing that grew"
        );
        assert!(
            !text.contains("#EXT-X-PLAYLIST-TYPE:EVENT"),
            "EVENT must not appear in any playlist: {text}"
        );
        assert!(text.contains("#EXT-X-START:TIME-OFFSET=0.000,PRECISE=YES"));
        assert!(text.contains("seg_00000008008.m4s"));
        assert!(text.contains("seg_00000012012.m4s"));
        assert!(
            text.contains("#EXT-X-ENDLIST"),
            "a complete listing always ends"
        );
        // A full-title listing begins at 0, so the attach point has to be
        // stated rather than implied by where the listing starts.
        let landed = build_map_playlist(&entries, "init.mp4", 8008);
        assert!(
            String::from_utf8(landed)
                .unwrap()
                .contains("#EXT-X-START:TIME-OFFSET=8.008,PRECISE=YES"),
            "the attach point is the land, not the first listed entry"
        );
    }
}
