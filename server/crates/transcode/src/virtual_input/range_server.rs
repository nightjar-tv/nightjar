//! Session-scoped HTTP Range server for virtual media files (ADR-0023 §4).
//!
//! This is not a public `/media/{id}` range server: it listens on loopback
//! under an unguessable path, serves exactly one virtual layout, and stops
//! when the session that bound it drops the handle.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Ranges stream in chunks: a far Matroska remainder is multi-GB and must
/// never be buffered whole.
const CHUNK_BYTES: usize = 256 * 1024;
/// Request head ceiling. FFmpeg sends a handful of short headers.
const MAX_REQUEST_BYTES: u64 = 8 * 1024;
/// Accept loop wake-up, so a dropped handle stops the listener promptly.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// One span of the virtual file.
pub(crate) enum Piece {
    /// Bytes computed at bind time (a rewritten MP4 `moov`).
    Bytes(Vec<u8>),
    /// A byte range of the real media file, read on demand.
    FileRange { offset: u64, len: u64 },
}

impl Piece {
    fn len(&self) -> u64 {
        match self {
            Piece::Bytes(b) => b.len() as u64,
            Piece::FileRange { len, .. } => *len,
        }
    }
}

/// The byte layout FFmpeg sees, plus the real file the ranges read from.
pub(crate) struct VirtualFile {
    src: PathBuf,
    pieces: Vec<Piece>,
    total: u64,
    content_type: &'static str,
    /// Request target, e.g. `/3f1c….mkv`.
    target: String,
}

impl VirtualFile {
    pub(crate) fn new(
        src: &Path,
        pieces: Vec<Piece>,
        content_type: &'static str,
        extension: &str,
    ) -> Self {
        let total = pieces.iter().map(Piece::len).sum();
        Self {
            src: src.to_path_buf(),
            pieces,
            total,
            content_type,
            target: format!("/{}.{extension}", unguessable_token()),
        }
    }
}

/// Running loopback server for one [`VirtualFile`]. Dropping it stops the
/// listener and aborts in-flight range writes.
pub(crate) struct RangeServer {
    url: String,
    shutdown: Arc<AtomicBool>,
}

impl RangeServer {
    pub(crate) fn start(file: VirtualFile) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("bind virtual input listener: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("set virtual input listener nonblocking: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("virtual input listener address: {e}"))?
            .port();
        let url = format!("http://127.0.0.1:{port}{}", file.target);

        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown = Arc::clone(&shutdown);
        let file = Arc::new(file);
        std::thread::Builder::new()
            .name("hls-virtual-input".into())
            .spawn(move || {
                while !accept_shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let file = Arc::clone(&file);
                            let shutdown = Arc::clone(&accept_shutdown);
                            // FFmpeg opens concurrent Range connections; a
                            // serial accept loop deadlocks it (0 bytes served).
                            let spawned = std::thread::Builder::new()
                                .name("hls-virtual-range".into())
                                .spawn(move || serve_connection(stream, &file, &shutdown));
                            if let Err(e) = spawned {
                                tracing::warn!(error = %e, "virtual input: spawn range handler");
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(ACCEPT_POLL)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "virtual input: accept failed");
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("spawn virtual input server: {e}"))?;

        Ok(Self { url, shutdown })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for RangeServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Random enough that the URL is not guessable from a session id or a port
/// scan. `RandomState` is seeded from the OS random source.
fn unguessable_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let high = RandomState::new().build_hasher().finish();
    let low = RandomState::new().build_hasher().finish();
    format!("{high:016x}{low:016x}")
}

struct Request {
    method: String,
    target: String,
    /// `bytes=start-` or `bytes=start-end`.
    range: Option<(u64, Option<u64>)>,
}

fn serve_connection(stream: TcpStream, file: &VirtualFile, shutdown: &AtomicBool) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut out = write_half;
    let request = match read_request(&stream) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(e) => {
            tracing::debug!(error = %e, "virtual input: bad request");
            let _ = write_status(&mut out, "400 Bad Request", &[]);
            return;
        }
    };
    if request.target != file.target {
        let _ = write_status(&mut out, "404 Not Found", &[]);
        return;
    }
    let total = file.total;
    let head_only = request.method == "HEAD";
    if !head_only && request.method != "GET" {
        let _ = write_status(&mut out, "405 Method Not Allowed", &[]);
        return;
    }

    let (status, start, end) = match request.range {
        None => ("200 OK", 0, total.saturating_sub(1)),
        Some((start, end)) => {
            let end = end.unwrap_or(total.saturating_sub(1)).min(total - 1);
            if start > end || start >= total {
                let _ = write_status(
                    &mut out,
                    "416 Range Not Satisfiable",
                    &[("Content-Range", &format!("bytes */{total}"))],
                );
                return;
            }
            ("206 Partial Content", start, end)
        }
    };
    let length = (end - start + 1).to_string();
    let content_range = format!("bytes {start}-{end}/{total}");
    let mut headers = vec![
        ("Content-Length", length.as_str()),
        ("Content-Type", file.content_type),
        ("Accept-Ranges", "bytes"),
        ("Connection", "close"),
    ];
    if status.starts_with("206") {
        headers.push(("Content-Range", content_range.as_str()));
    }
    if write_status(&mut out, status, &headers).is_err() {
        return;
    }
    if head_only {
        return;
    }
    if let Err(e) = write_range(&mut out, file, start, end, shutdown) {
        // A killed FFmpeg closes mid-range; that is the normal end of a run.
        tracing::debug!(error = %e, "virtual input: range write ended early");
    }
}

fn read_request(stream: &TcpStream) -> Result<Option<Request>, String> {
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .map_err(|e| format!("read request line: {e}"))?
        == 0
    {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "empty request line".to_string())?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| "request line without target".to_string())?
        .to_string();

    let mut range = None;
    loop {
        let mut header = String::new();
        let read = reader
            .read_line(&mut header)
            .map_err(|e| format!("read header: {e}"))?;
        if read == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("range")
        {
            range = parse_byte_range(value.trim());
        }
    }
    Ok(Some(Request {
        method,
        target,
        range,
    }))
}

/// Parses a single `bytes=start-end` spec. Multi-range requests (which
/// FFmpeg does not send) and suffix ranges are refused as "no range".
fn parse_byte_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = end.trim();
    if end.is_empty() {
        return Some((start, None));
    }
    Some((start, Some(end.parse().ok()?)))
}

fn write_status(
    out: &mut impl Write,
    status: &str,
    headers: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())
}

/// Writes virtual bytes `[start, end]`, mapping each piece onto literal
/// bytes or a pread of the real file.
fn write_range(
    out: &mut impl Write,
    file: &VirtualFile,
    start: u64,
    end: u64,
    shutdown: &AtomicBool,
) -> std::io::Result<()> {
    let mut src = File::open(&file.src)?;
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut base = 0u64;
    for piece in &file.pieces {
        let piece_end = base + piece.len();
        if piece_end <= start {
            base = piece_end;
            continue;
        }
        if base > end {
            break;
        }
        let from = start.max(base) - base;
        let to = end.min(piece_end - 1) - base;
        match piece {
            Piece::Bytes(bytes) => out.write_all(&bytes[from as usize..=to as usize])?,
            Piece::FileRange { offset, .. } => {
                src.seek(SeekFrom::Start(offset + from))?;
                let mut left = to - from + 1;
                while left > 0 {
                    if shutdown.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let want = left.min(CHUNK_BYTES as u64) as usize;
                    let read = src.read(&mut buf[..want])?;
                    if read == 0 {
                        return Ok(());
                    }
                    out.write_all(&buf[..read])?;
                    left -= read as u64;
                }
            }
        }
        base = piece_end;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_open_and_closed_ranges() {
        assert_eq!(parse_byte_range("bytes=0-"), Some((0, None)));
        assert_eq!(parse_byte_range("bytes=10-19"), Some((10, Some(19))));
        assert_eq!(parse_byte_range("bytes=10-19, 30-39"), None);
        assert_eq!(parse_byte_range("items=0-1"), None);
        assert_eq!(parse_byte_range("bytes=-500"), None);
    }

    fn fixture(dir: &Path) -> VirtualFile {
        let src = dir.join("src.bin");
        std::fs::write(&src, b"0123456789ABCDEF").unwrap();
        VirtualFile::new(
            &src,
            vec![
                Piece::FileRange { offset: 0, len: 4 },
                Piece::Bytes(b"XY".to_vec()),
                Piece::FileRange { offset: 10, len: 6 },
            ],
            "video/mp4",
            "mp4",
        )
    }

    fn collect(file: &VirtualFile, start: u64, end: u64) -> Vec<u8> {
        let mut out = Vec::new();
        write_range(&mut out, file, start, end, &AtomicBool::new(false)).unwrap();
        out
    }

    #[test]
    fn pieces_join_into_one_virtual_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        assert_eq!(collect(&file, 0, 11), b"0123XYABCDEF".to_vec());
    }

    #[test]
    fn range_reads_map_onto_the_right_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        assert_eq!(collect(&file, 3, 7), b"3XYAB".to_vec());
        assert_eq!(collect(&file, 6, 11), b"ABCDEF".to_vec());
        assert_eq!(collect(&file, 11, 11), b"F".to_vec());
    }

    #[test]
    fn serves_ranges_over_loopback_and_stops_on_drop() {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let server = RangeServer::start(file).unwrap();
        let url = server.url().to_string();
        let (host_port, target) = url.trim_start_matches("http://").split_once('/').unwrap();
        let target = format!("/{target}");

        let mut stream = TcpStream::connect(host_port).unwrap();
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: localhost\r\nRange: bytes=3-7\r\n\r\n"
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            head.push_str(&line);
        }
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert!(head.contains("Content-Range: bytes 3-7/12"), "{head}");
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"3XYAB");

        drop(server);
        std::thread::sleep(ACCEPT_POLL * 4);
        assert!(
            TcpStream::connect(host_port).is_err(),
            "listener still accepting after drop"
        );
    }

    #[test]
    fn wrong_token_is_not_served() {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let file = fixture(dir.path());
        let server = RangeServer::start(file).unwrap();
        let host_port = server
            .url()
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let mut stream = TcpStream::connect(&host_port).unwrap();
        write!(
            stream,
            "GET /guessed.mkv HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert!(line.starts_with("HTTP/1.1 404"), "{line}");
    }
}
