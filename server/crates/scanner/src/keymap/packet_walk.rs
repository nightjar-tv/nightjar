//! Fallback keyframe extraction via an ffprobe packet walk (ADR-0023 §2).
//! Demuxes the whole file, so it is reserved for sources whose container
//! index is missing or truncated — not the default path. A whole-file read
//! against a library root, so the caller's bulk-reader gate serialises it
//! with subtitle extract (ADR-0041 Decision 8.6) and the reachability signal
//! cancels it in flight (Decision 8.7).

use super::KeyframeEntry;
use crate::ffprobe_child::{ChildFailure, ChildFailureKind, StderrTail, spawn, supervise};
use std::io::Read;
use std::mem::size_of;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Approved diagnostic retention policy. Excess stderr is discarded while the
/// pipe continues draining, so diagnostic overflow never kills a packet walk.
const STDERR_TAIL: usize = 4 * 1024;
/// Approved parser working-buffer policy, counted before the newline byte.
const PACKET_RECORD_BUDGET: usize = 4 * 1024;
/// Approved retained keyframe storage policy. The entry count is derived from
/// the actual entry shape rather than a separately maintained magic count.
const KEYFRAME_STORAGE_BUDGET: usize = 64 * 1024 * 1024;
/// Approved maximum occupancy of a whole-file packet walk.
const PACKET_WALK_DEADLINE: Duration = Duration::from_secs(1_800);
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
    let mut command = Command::new("ffprobe");
    command
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
        .stderr(Stdio::piped());
    let child = spawn(&mut command)
        .map_err(|error| format!("{} for {}", error.message(), path.display()))?;
    let completed = supervise(
        child,
        PACKET_WALK_DEADLINE,
        CANCEL_POLL,
        should_cancel,
        |reader| read_packet_entries(reader, PACKET_RECORD_BUDGET, KEYFRAME_STORAGE_BUDGET),
        |reader| StderrTail::read(reader, STDERR_TAIL),
    )
    .map_err(|error| {
        if error.kind == ChildFailureKind::Cancelled {
            format!("unavailable: keyframe packet walk: {}", error.message())
        } else {
            format!(
                "ffprobe packet walk for {}: {}",
                path.display(),
                error.message()
            )
        }
    })?;

    if let Err(error) = completed.status {
        let tail = completed.stderr.display();
        return Err(format!(
            "{error} during packet walk for {}: {tail}",
            path.display()
        ));
    }
    let mut entries = completed.stdout;
    entries.sort_by_key(|e| e.pts_ms);
    Ok(entries)
}

fn read_packet_entries<R: Read>(
    mut reader: R,
    record_budget: usize,
    storage_budget: usize,
) -> Result<Vec<KeyframeEntry>, ChildFailure> {
    let entry_size = size_of::<KeyframeEntry>();
    let max_entries = storage_budget / entry_size;
    if max_entries == 0 {
        return Err(ChildFailure::new(
            ChildFailureKind::OutputBudget,
            "keyframe storage policy cannot retain one entry",
        ));
    }
    let mut entries = Vec::new();
    let mut record = Vec::with_capacity(record_budget);
    #[cfg(test)]
    {
        assert!(entries.capacity() * entry_size <= storage_budget);
        assert!(record.capacity() <= record_budget);
        eprintln!(
            "packet allocation record={} keyframes={} budget={storage_budget}",
            record.capacity(),
            entries.capacity() * entry_size
        );
    }
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            if !record.is_empty() {
                append_packet_record(&mut entries, &record, max_entries, storage_budget)?;
            }
            return Ok(entries);
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                append_packet_record(&mut entries, &record, max_entries, storage_budget)?;
                record.clear();
            } else {
                if record.len() == record_budget {
                    return Err(ChildFailure::new(
                        ChildFailureKind::OutputBudget,
                        format!("ffprobe packet record exceeds {record_budget} byte policy"),
                    ));
                }
                record.push(*byte);
            }
        }
    }
}

fn append_packet_record(
    entries: &mut Vec<KeyframeEntry>,
    record: &[u8],
    max_entries: usize,
    storage_budget: usize,
) -> Result<(), ChildFailure> {
    let record = std::str::from_utf8(record).map_err(|error| {
        ChildFailure::new(
            ChildFailureKind::Read,
            format!("ffprobe packet record is not UTF-8: {error}"),
        )
    })?;
    let Some(entry) = parse_packet_line(record) else {
        return Ok(());
    };
    if entries.len() == max_entries {
        return Err(ChildFailure::new(
            ChildFailureKind::OutputBudget,
            format!("ffprobe keyframe map exceeds {storage_budget} byte policy"),
        ));
    }
    let target = entries.len().checked_add(1).ok_or_else(|| {
        ChildFailure::new(ChildFailureKind::OutputBudget, "keyframe count overflow")
    })?;
    if entries.capacity() < target {
        let growth = entries.capacity().max(1).saturating_mul(2);
        let requested = growth.min(max_entries).max(target);
        entries
            .try_reserve_exact(requested - entries.len())
            .map_err(|_| {
                ChildFailure::new(
                    ChildFailureKind::OutputBudget,
                    format!("keyframe map allocation exceeds {storage_budget} byte policy"),
                )
            })?;
    }
    entries.push(entry);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
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

    #[test]
    fn packet_stream_preserves_unterminated_final_records() {
        let entries = read_packet_entries(
            Cursor::new(b"1.5,10,K__\n2.0,20,___\n3.0,30,K__"),
            PACKET_RECORD_BUDGET,
            KEYFRAME_STORAGE_BUDGET,
        )
        .expect("streamed records");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].pts_ms, 1_500);
        assert_eq!(entries[1].pts_ms, 3_000);
    }

    #[test]
    fn packet_record_and_keyframe_storage_boundaries_are_exact() {
        let size = size_of::<KeyframeEntry>();
        for length in [3, 4, 5] {
            for newline in [false, true] {
                let mut record = vec![b'x'; length];
                if newline {
                    record.push(b'\n');
                }
                let result = read_packet_entries(Cursor::new(record), 4, 2 * size);
                if length <= 4 {
                    assert!(result.unwrap().is_empty());
                } else {
                    assert_eq!(result.unwrap_err().kind, ChildFailureKind::OutputBudget);
                }
            }
        }
        let below = read_packet_entries(Cursor::new(b"1,1,K"), 16, 2 * size).unwrap();
        assert_eq!(below.len(), 1);
        assert_eq!(below.capacity() * size, 2 * size);
        for budget in [2 * size - 1, 2 * size, 2 * size + 1] {
            let result = read_packet_entries(Cursor::new(b"1,1,K\n2,2,K"), 16, budget);
            if budget < 2 * size {
                assert_eq!(result.unwrap_err().kind, ChildFailureKind::OutputBudget);
            } else {
                let entries = result.unwrap();
                assert_eq!(entries.len(), 2);
                assert!(entries.capacity() * size <= budget);
            }
        }
        let entries = read_packet_entries(
            Cursor::new(vec![b'x'; 4]),
            4,
            2 * size_of::<KeyframeEntry>(),
        )
        .expect("a final record at the byte limit is accepted");
        assert!(entries.is_empty());
        let error = read_packet_entries(
            Cursor::new(vec![b'x'; 5]),
            4,
            2 * size_of::<KeyframeEntry>(),
        )
        .expect_err("record limit plus one is rejected");
        assert_eq!(error.kind, ChildFailureKind::OutputBudget);
        assert!(error.message().contains("exceeds 4 byte policy"), "{error}");

        let entries = read_packet_entries(
            Cursor::new(b"1,1,K\n2,2,K"),
            16,
            2 * size_of::<KeyframeEntry>(),
        )
        .expect("two entries fit exactly");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.capacity(), 2, "requested allocation high-water");
        let error = read_packet_entries(
            Cursor::new(b"1,1,K\n2,2,K\n3,3,K"),
            16,
            2 * size_of::<KeyframeEntry>(),
        )
        .expect_err("keyframe budget plus one entry is rejected");
        assert_eq!(error.kind, ChildFailureKind::OutputBudget);
        assert!(error.message().contains("keyframe map exceeds"), "{error}");
    }

    #[test]
    fn packet_records_are_parsed_across_actual_read_boundaries() {
        struct Fragmented<'a> {
            remaining: &'a [u8],
            chunk: usize,
            calls: usize,
        }
        impl Read for Fragmented<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                let count = self.chunk.min(output.len()).min(self.remaining.len());
                output[..count].copy_from_slice(&self.remaining[..count]);
                self.remaining = &self.remaining[count..];
                Ok(count)
            }
        }
        let csv = "1.5,10,K__\r\nbad\n2.0,20,___\nN/A,4,K\n3.0,30,K__";
        let expected: Vec<_> = csv.lines().filter_map(parse_packet_line).collect();
        for chunk in [1, 2, 3, 7] {
            let mut input = Fragmented {
                remaining: csv.as_bytes(),
                chunk,
                calls: 0,
            };
            let entries = read_packet_entries(
                &mut input,
                PACKET_RECORD_BUDGET,
                4 * size_of::<KeyframeEntry>(),
            )
            .unwrap();
            assert_eq!(entries, expected);
            assert_eq!(entries.len(), 2);
            assert!(input.calls > 2);
            eprintln!("packet fragmented chunk={chunk} read_calls={}", input.calls);
        }
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
