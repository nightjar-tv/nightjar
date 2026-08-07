//! Fallback keyframe extraction via an ffprobe packet walk (ADR-0023 §2).
//! Demuxes the whole file, so it is reserved for sources whose container
//! index is missing or truncated — not the default path. A whole-file read
//! against a library root, so the caller's bulk-reader gate serialises it
//! with subtitle extract (ADR-0041 Decision 8.6) and the reachability signal
//! cancels it in flight (Decision 8.7).

use super::KeyframeEntry;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const STDERR_TAIL: usize = 512;
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Demux the whole file and return every video keyframe (PTS, byte offset).
/// `should_cancel` is the library reachability signal (ADR-0014): when it
/// turns true the ffprobe child is killed and the walk reports
/// `unavailable`, never a partial map.
///
/// The output pipes are drained concurrently on reader threads, like
/// `Command::output()` does: `ffprobe -show_packets` emits one CSV line per
/// video packet, and a full-length title's output dwarfs the pipe buffer, so
/// a drain-after-exit loop would let ffprobe block on a full pipe forever.
pub fn walk(
    path: &Path,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Result<Vec<KeyframeEntry>, String> {
    let mut child = Command::new("ffprobe")
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "spawn ffprobe: not found on PATH".to_string()
            } else {
                format!("spawn ffprobe for {}: {e}", path.display())
            }
        })?;

    // Drain both pipes now, not after exit: ffprobe blocks once a pipe fills,
    // and the packet CSV of a real title is far larger than the pipe buffer.
    // The threads finish at EOF, which is guaranteed once the child exits or
    // is killed below.
    let stdout_reader = spawn_pipe_reader("packet-walk-stdout", child.stdout.take())?;
    let stderr_reader = spawn_pipe_reader("packet-walk-stderr", child.stderr.take())?;

    let status = loop {
        // Cancel wins over a just-finished walk: once the library is
        // unreachable the run is aborted, never reported ready (ADR-0041
        // Decision 8.7). Stamped "unavailable:" for the pool's classifier.
        if should_cancel.is_some_and(|c| c()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("unavailable: keyframe packet walk cancelled (library unreachable)".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(CANCEL_POLL),
            Err(e) => {
                return Err(format!(
                    "wait ffprobe packet walk for {}: {e}",
                    path.display()
                ));
            }
        }
    };

    // The child has exited, so both pipes are at EOF and the readers are done.
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let tail = stderr_tail(stderr.trim());
        return Err(format!(
            "ffprobe packet walk failed for {} (exit {code}): {tail}",
            path.display()
        ));
    }
    let mut entries: Vec<KeyframeEntry> = stdout.lines().filter_map(parse_packet_line).collect();
    entries.sort_by_key(|e| e.pts_ms);
    Ok(entries)
}

/// Drain one of ffprobe's pipes on a reader thread so the child can never
/// block on a full pipe while the walk polls for exit or cancel.
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    name: &str,
    pipe: Option<R>,
) -> Result<std::thread::JoinHandle<String>, String> {
    let Some(mut pipe) = pipe else {
        return Err(format!("{name}: child pipe missing"));
    };
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut buf = String::new();
            let _ = pipe.read_to_string(&mut buf);
            buf
        })
        .map_err(|e| format!("spawn {name} reader: {e}"))
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
        let entries = walk(&path, None).unwrap();
        assert!(!entries.is_empty());
        assert!(entries.windows(2).all(|w| w[0].pts_ms <= w[1].pts_ms));
    }

    /// ADR-0041 Decision 8.7: the reachability cancel signal kills the ffprobe
    /// child mid-walk and stamps the run `unavailable`, never a partial map.
    #[test]
    fn packet_walk_cancel_aborts_in_flight() {
        if !ffprobe_available() {
            return;
        }
        let path = testdata_path("h264_aac_mkv.mkv");
        if !path.exists() {
            return;
        }
        let err = walk(&path, Some(&|| true)).unwrap_err();
        assert!(err.starts_with("unavailable:"), "{err}");
    }

    /// Regression lock for the pipe-fill deadlock: a fixture whose packet CSV
    /// exceeds the pipe buffer (10 s at 600 fps ≈ 6,000 packets × ~30 B ≈
    /// 120 KB, beyond both the 16 KiB macOS and 64 KiB Linux pipes) must
    /// complete within a bounded wall. A drain-after-exit walk would let
    /// ffprobe block on the full pipe and hang forever; the pipes must be
    /// drained concurrently.
    #[test]
    fn packet_walk_completes_when_output_exceeds_pipe_buffer() {
        if !ffprobe_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mkv = dir.path().join("many_packets.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=64x64:d=10:r=600",
                "-an",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mkv)
            .status();
        let Ok(status) = status else {
            eprintln!("skipping: could not spawn ffmpeg");
            return;
        };
        if !status.success() {
            eprintln!("skipping: ffmpeg many-packet fixture mux failed");
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let path = mkv.clone();
        std::thread::spawn(move || {
            let _ = tx.send(walk(&path, None));
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(entries)) => assert!(
                !entries.is_empty(),
                "packet walk of a real fixture must find keyframes"
            ),
            Ok(Err(e)) => panic!("packet walk failed on many-packet fixture: {e}"),
            Err(_) => {
                panic!("packet walk hung on a >pipe-buffer packet stream (pipe-fill deadlock)")
            }
        }
    }
}
