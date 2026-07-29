//! Slice a WebVTT document into a time window (HLS subtitle segments).

use super::srt::srt_to_webvtt;

/// Cues whose **start** falls in `[start_ms, end_ms)`, with original
/// (unclipped) start/end times.
///
/// Delivery is segmented; cue timing is not. Clipping ends to the window
/// made hls.js drop captions mid-line (Chrome dogfood 2026-07-27). Putting
/// the same full cue in every overlapping window made Safari double-paint.
/// Ownership by start segment keeps one copy, full duration (ADR-0013 §12).
pub fn slice_webvtt(body: &str, start_ms: u64, end_ms: u64) -> String {
    let mut out = String::from("WEBVTT\n\n");
    if end_ms <= start_ms {
        return out;
    }
    let normalised = body.replace("\r\n", "\n").replace('\r', "\n");
    for block in normalised.split("\n\n") {
        let block = block.trim();
        if block.is_empty()
            || block.starts_with("WEBVTT")
            || block.starts_with("NOTE")
            || block.starts_with("STYLE")
        {
            continue;
        }
        let lines: Vec<&str> = block.lines().collect();
        let timing_idx = match lines.iter().position(|l| l.contains("-->")) {
            Some(i) => i,
            None => continue,
        };
        let Some((cue_start, cue_end)) = parse_vtt_timing(lines[timing_idx]) else {
            continue;
        };
        // Start-segment ownership only (not mere overlap).
        if cue_start < start_ms || cue_start >= end_ms {
            continue;
        }
        if cue_end <= cue_start {
            continue;
        }
        // Stable id = original cue start ms (unique across the title).
        out.push_str(&cue_start.to_string());
        out.push('\n');
        out.push_str(&format_vtt_timestamp(cue_start));
        out.push_str(" --> ");
        out.push_str(&format_vtt_timestamp(cue_end));
        out.push('\n');
        for line in &lines[timing_idx + 1..] {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Latest cue end timestamp in `body`, if any cue parsed.
pub fn webvtt_max_cue_end_ms(body: &str) -> Option<u64> {
    let normalised = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut max = None;
    for line in normalised.lines() {
        if !line.contains("-->") {
            continue;
        }
        if let Some((_, end)) = parse_vtt_timing(line) {
            max = Some(max.map_or(end, |m: u64| m.max(end)));
        }
    }
    max
}

fn parse_vtt_timing(line: &str) -> Option<(u64, u64)> {
    let line = line.trim();
    let (left, right) = line.split_once("-->")?;
    let start = parse_vtt_timestamp(left.trim())?;
    let end_tok = right.split_whitespace().next().unwrap_or("");
    let end = parse_vtt_timestamp(end_tok)?;
    Some((start, end))
}

fn parse_vtt_timestamp(ts: &str) -> Option<u64> {
    let ts = ts.trim().replace(',', ".");
    let parts: Vec<&str> = ts.split(':').collect();
    let (h, m, rest) = match parts.len() {
        3 => (
            parts[0].parse::<u64>().ok()?,
            parts[1].parse::<u64>().ok()?,
            parts[2],
        ),
        2 => (0, parts[0].parse::<u64>().ok()?, parts[1]),
        _ => return None,
    };
    let (s, ms) = match rest.split_once('.') {
        Some((s, frac)) => {
            let s = s.parse::<u64>().ok()?;
            let mut digits: String = frac.chars().filter(|c| c.is_ascii_digit()).collect();
            while digits.len() < 3 {
                digits.push('0');
            }
            digits.truncate(3);
            (s, digits.parse::<u64>().ok()?)
        }
        None => (rest.parse::<u64>().ok()?, 0),
    };
    Some(h * 3_600_000 + m * 60_000 + s * 1000 + ms)
}

fn format_vtt_timestamp(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let frac = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{frac:03}")
}

/// Convert SRT bytes and slice in one step (session sidecar / demux helpers).
#[allow(dead_code)]
pub fn srt_bytes_to_sliced_webvtt(bytes: &[u8], start_ms: u64, end_ms: u64) -> String {
    let vtt = srt_to_webvtt(&super::decode_subtitle_bytes(bytes));
    slice_webvtt(&vtt, start_ms, end_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_keeps_cues_that_start_in_window() {
        let body = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:03.000\nA\n\n2\n00:00:05.000 --> 00:00:06.000\nB\n\n";
        let sliced = slice_webvtt(body, 0, 2000);
        assert!(sliced.contains("\nA\n"));
        assert!(!sliced.contains("\nB\n"));
        // Full end time preserved past the segment boundary.
        assert!(sliced.contains("00:00:01.000 --> 00:00:03.000"), "{sliced}");
        let later = slice_webvtt(body, 4000, 6000);
        assert!(later.contains("\nB\n"));
        assert!(!later.contains("\nA\n"));
    }

    #[test]
    fn spanning_cue_owned_by_start_segment_only() {
        let body = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:05.000\nLong\n\n";
        let a = slice_webvtt(body, 0, 2000);
        let b = slice_webvtt(body, 2000, 4000);
        assert!(a.contains("00:00:01.000 --> 00:00:05.000"), "{a}");
        assert!(a.contains("\n1000\n"), "{a}");
        assert!(a.contains("\nLong\n"), "{a}");
        // Must not reappear in later windows (Safari double-paint).
        assert_eq!(b.trim(), "WEBVTT", "{b}");
    }

    #[test]
    fn cue_starting_in_later_window_not_in_earlier() {
        let body = "WEBVTT\n\n1\n00:00:03.000 --> 00:00:05.000\nLate\n\n";
        let early = slice_webvtt(body, 0, 2000);
        let mid = slice_webvtt(body, 2000, 4000);
        assert_eq!(early.trim(), "WEBVTT", "{early}");
        assert!(mid.contains("00:00:03.000 --> 00:00:05.000"), "{mid}");
    }

    #[test]
    fn max_cue_end() {
        let body = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:03.500\nA\n\n";
        assert_eq!(webvtt_max_cue_end_ms(body), Some(3500));
        assert_eq!(webvtt_max_cue_end_ms("WEBVTT\n"), None);
    }

    #[test]
    fn empty_window_is_header_only() {
        let body = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\nA\n\n";
        let sliced = slice_webvtt(body, 10_000, 12_000);
        assert_eq!(sliced.trim(), "WEBVTT");
    }
}
