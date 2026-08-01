//! Fallback keyframe extraction via an ffprobe packet walk (ADR-0023 §2).
//! Demuxes the whole file, so it is reserved for sources whose container
//! index is missing or truncated — not the default path.

use super::KeyframeEntry;
use std::path::Path;
use std::process::Command;

const STDERR_TAIL: usize = 512;

pub fn walk(path: &Path) -> Result<Vec<KeyframeEntry>, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,pos,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "spawn ffprobe: not found on PATH".to_string()
            } else {
                format!("spawn ffprobe for {}: {e}", path.display())
            }
        })?;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr_tail(stderr.trim());
        return Err(format!(
            "ffprobe packet walk failed for {} (exit {code}): {tail}",
            path.display()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries: Vec<KeyframeEntry> = stdout.lines().filter_map(parse_packet_line).collect();
    entries.sort_by_key(|e| e.pts_ms);
    Ok(entries)
}

/// Parses one `csv=p=0` line of `pts_time,pos,flags`, keeping only keyframes
/// (flags containing `K`). Returns `None` for unparseable or non-key lines.
fn parse_packet_line(line: &str) -> Option<KeyframeEntry> {
    let mut fields = line.splitn(3, ',');
    let pts_time = fields.next()?;
    let pos = fields.next()?;
    let flags = fields.next()?;
    if !flags.contains('K') {
        return None;
    }
    let pts_time: f64 = pts_time.parse().ok()?;
    let byte_offset: i64 = pos.parse().ok()?;
    Some(KeyframeEntry {
        pts_ms: (pts_time * 1000.0).round() as i64,
        byte_offset,
    })
}

fn stderr_tail(s: &str) -> String {
    if s.is_empty() {
        return "(no stderr)".into();
    }
    if s.len() <= STDERR_TAIL {
        return s.to_string();
    }
    let start = s.len() - STDERR_TAIL;
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_packet_line_keeps_only_keyframes() {
        assert_eq!(
            parse_packet_line("1.500000,12345,K__"),
            Some(KeyframeEntry {
                pts_ms: 1500,
                byte_offset: 12345
            })
        );
        assert_eq!(parse_packet_line("1.500000,12345,___"), None);
    }

    #[test]
    fn parse_packet_line_rejects_unparseable_fields() {
        assert_eq!(parse_packet_line("N/A,12345,K__"), None);
        assert_eq!(parse_packet_line("1.5,N/A,K__"), None);
        assert_eq!(parse_packet_line("1.5,12345"), None);
    }

    #[test]
    fn parse_packet_line_rounds_pts_to_nearest_ms() {
        let e = parse_packet_line("0.041667,0,K__").unwrap();
        assert_eq!(e.pts_ms, 42);
    }

    fn ffprobe_available() -> bool {
        Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn testdata_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files")
            .join(name)
    }

    #[test]
    fn packet_walk_finds_keyframes_in_real_corpus_file() {
        if !ffprobe_available() {
            return;
        }
        let path = testdata_path("h264_aac_mkv.mkv");
        if !path.exists() {
            return;
        }
        let entries = walk(&path).unwrap();
        assert!(!entries.is_empty());
        assert!(entries.windows(2).all(|w| w[0].pts_ms <= w[1].pts_ms));
    }
}
