//! SRT → WebVTT conversion (ADR-0010). Pure; no FFmpeg.

/// Decode subtitle file bytes: UTF-8 (with BOM) first, else Windows-1252.
/// A wrong UTF-8 guess produces errors or replacement; we only accept strict
/// UTF-8, then fall back so Latin-1 / CP1252 libraries stay legible.
pub fn decode_subtitle_bytes(bytes: &[u8]) -> String {
    let bytes = strip_utf8_bom(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_windows_1252(bytes),
    }
}

fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn decode_windows_1252(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char_from_windows_1252(b)).collect()
}

fn char_from_windows_1252(b: u8) -> char {
    // 0x80–0x9F differ from Latin-1; the rest match Unicode code points.
    match b {
        0x80 => '\u{20AC}',
        0x81 => '\u{0081}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8D => '\u{008D}',
        0x8E => '\u{017D}',
        0x8F => '\u{008F}',
        0x90 => '\u{0090}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9D => '\u{009D}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        _ => b as char,
    }
}

/// Convert an SRT document to WebVTT. Accepts comma or period decimals,
/// CRLF, missing/non-sequential cue numbers, overlapping cues, and
/// HTML-ish inline tags (passed through for the browser).
pub fn srt_to_webvtt(srt: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    let normalised = srt.replace("\r\n", "\n").replace('\r', "\n");
    let blocks = normalised.split("\n\n");
    let mut cue_n = 1u32;
    for block in blocks {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let lines: Vec<&str> = block.lines().collect();
        let (timing_idx, text_start) = match find_timing_line(&lines) {
            Some(i) => (i, i + 1),
            None => continue,
        };
        let Some(vtt_timing) = srt_timing_to_webvtt(lines[timing_idx]) else {
            continue;
        };
        out.push_str(&cue_n.to_string());
        out.push('\n');
        out.push_str(&vtt_timing);
        out.push('\n');
        for line in &lines[text_start..] {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        cue_n += 1;
    }
    out
}

fn find_timing_line(lines: &[&str]) -> Option<usize> {
    lines.iter().position(|l| l.contains("-->"))
}

fn srt_timing_to_webvtt(line: &str) -> Option<String> {
    let line = line.trim();
    let (left, right) = line.split_once("-->")?;
    let start = normalise_timestamp(left.trim())?;
    let end_part = right.trim();
    // SRT may carry positioning after the end time; keep only the timestamp.
    let end_tok = end_part.split_whitespace().next().unwrap_or(end_part);
    let end = normalise_timestamp(end_tok)?;
    Some(format!("{start} --> {end}"))
}

fn normalise_timestamp(ts: &str) -> Option<String> {
    let ts = ts.trim().replace(',', ".");
    // HH:MM:SS.mmm or MM:SS.mmm
    let parts: Vec<&str> = ts.split(':').collect();
    match parts.len() {
        3 => {
            let (h, m, rest) = (parts[0], parts[1], parts[2]);
            let (s, ms) = split_frac(rest)?;
            Some(format!(
                "{:02}:{:02}:{:02}.{:03}",
                parse_u32(h)?,
                parse_u32(m)?,
                s,
                ms
            ))
        }
        2 => {
            let (m, rest) = (parts[0], parts[1]);
            let (s, ms) = split_frac(rest)?;
            Some(format!("00:{:02}:{:02}.{:03}", parse_u32(m)?, s, ms))
        }
        _ => None,
    }
}

fn split_frac(rest: &str) -> Option<(u32, u32)> {
    let (s, frac) = match rest.split_once('.') {
        Some((s, f)) => (s, f),
        None => (rest, "000"),
    };
    let s = parse_u32(s)?;
    let mut ms = frac
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>();
    while ms.len() < 3 {
        ms.push('0');
    }
    ms.truncate(3);
    let ms = parse_u32(&ms)?;
    Some((s, ms))
}

fn parse_u32(s: &str) -> Option<u32> {
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comma_and_period_timestamps() {
        let comma = "1\n00:00:01,500 --> 00:00:02,000\nHello\n";
        let period = "1\n00:00:01.500 --> 00:00:02.000\nHello\n";
        let a = srt_to_webvtt(comma);
        let b = srt_to_webvtt(period);
        assert!(a.contains("00:00:01.500 --> 00:00:02.000"));
        assert!(b.contains("00:00:01.500 --> 00:00:02.000"));
        assert!(a.starts_with("WEBVTT"));
    }

    #[test]
    fn crlf_and_bom() {
        let srt = "1\r\n00:00:00,000 --> 00:00:01,000\r\nHi\r\n";
        let vtt = srt_to_webvtt(srt);
        assert!(vtt.contains("Hi"));
        let with_bom = format!("\u{feff}{srt}");
        assert!(srt_to_webvtt(&with_bom).contains("Hi"));
        let bytes = {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice(srt.as_bytes());
            v
        };
        let decoded = decode_subtitle_bytes(&bytes);
        assert!(!decoded.starts_with('\u{feff}'));
        assert!(srt_to_webvtt(&decoded).contains("Hi"));
    }

    #[test]
    fn missing_and_nonsequential_numbers() {
        let srt = "99\n00:00:00,000 --> 00:00:01,000\nA\n\n\n00:00:01,000 --> 00:00:02,000\nB\n";
        let vtt = srt_to_webvtt(srt);
        assert!(vtt.contains("\n1\n"));
        assert!(vtt.contains("\n2\n"));
        assert!(vtt.contains("A"));
        assert!(vtt.contains("B"));
    }

    #[test]
    fn overlapping_cues_kept() {
        let srt = "1\n00:00:00,000 --> 00:00:02,000\nA\n\n2\n00:00:01,000 --> 00:00:03,000\nB\n";
        let vtt = srt_to_webvtt(srt);
        assert!(vtt.contains("00:00:00.000 --> 00:00:02.000"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:03.000"));
    }

    #[test]
    fn html_ish_tags_passed_through() {
        let srt = "1\n00:00:00,000 --> 00:00:01,000\n<i>italic</i> and <b>bold</b>\n";
        let vtt = srt_to_webvtt(srt);
        assert!(vtt.contains("<i>italic</i>"));
        assert!(vtt.contains("<b>bold</b>"));
    }

    #[test]
    fn windows_1252_fallback() {
        // "café" in Windows-1252: c a f é=0xE9
        let bytes = b"1\n00:00:00,000 --> 00:00:01,000\ncaf\xE9\n".to_vec();
        let text = decode_subtitle_bytes(&bytes);
        assert!(text.contains("café"), "got {text:?}");
        let vtt = srt_to_webvtt(&text);
        assert!(vtt.contains("café"));
    }

    #[test]
    fn utf8_preferred_over_1252() {
        let srt = "1\n00:00:00,000 --> 00:00:01,000\ncafé\n";
        let text = decode_subtitle_bytes(srt.as_bytes());
        assert_eq!(text, srt);
    }

    /// Shapes FFmpeg emits when stream-copying subrip out of an MKV.
    #[test]
    fn ffmpeg_stream_copy_srt_shapes() {
        // Comma millis, multi-line cue body, no hours-omitted form here.
        let srt = "\
1
00:00:01,376 --> 00:00:02,711
[projector sounds]

4
00:00:20,979 --> 00:00:23,940
the human thing is not
so complicated.

5
00:00:23,940 --> 00:00:27,610
[doppler sound effect]
";
        let vtt = srt_to_webvtt(srt);
        assert!(vtt.contains("00:00:01.376 --> 00:00:02.711"));
        assert!(vtt.contains("the human thing is not\nso complicated."));
        assert!(vtt.contains("[doppler sound effect]"));
    }

    #[test]
    fn ffmpeg_font_and_an_tags_passed_through() {
        // mov_text / some rippers emit font face tags; ASS-ish {\an8} can
        // survive a text extract. Pass through — browser ignores unknowns.
        let srt = "\
1
00:00:00,000 --> 00:00:01,500
<font face=\"Arial\" size=\"20\">Hello</font>

2
00:00:01,500 --> 00:00:03,000
{\\an8}Top of screen
";
        let vtt = srt_to_webvtt(srt);
        assert!(vtt.contains("<font face=\"Arial\" size=\"20\">Hello</font>"));
        assert!(vtt.contains("{\\an8}Top of screen"));
    }

    #[test]
    fn ffmpeg_position_coords_stripped_from_timing() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000 X1:40 X2:600 Y1:20 Y2:50\nHi\n";
        let vtt = srt_to_webvtt(srt);
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.000\n"));
        assert!(!vtt.contains("X1:"));
        assert!(vtt.contains("Hi"));
    }
}
