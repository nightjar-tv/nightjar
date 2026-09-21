use crate::ffprobe_child::{ChildFailure, ChildFailureKind, StderrTail, spawn, supervise};
use serde::Deserialize;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct ProbeResult {
    pub duration_ms: Option<i64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    /// Absolute `index` of the selected (first) video stream (ADR-0058).
    /// `None` only when the file carries no video stream.
    pub video_stream_index: Option<u32>,
    /// Codec of the first audio track, for the ADR-0012 compatibility
    /// projection. It follows that track's `unknown` fallback.
    pub audio_codec: Option<String>,
    /// Channel count of the first audio stream (ADR-0012 channel ceiling).
    pub audio_channels: Option<i64>,
    /// Complete audio inventory, ordered by absolute stream index (ADR-0058).
    pub audio_streams: Vec<ProbeAudioStream>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// Video stream bitrate when ffprobe reports it (ADR-0022).
    pub video_bitrate_bps: Option<i64>,
    /// `none` | `hdr10` | `dolby_vision` | `dolby_vision_p5` (ADR-0022).
    /// Profile 5 is distinct: IPT-PQ has no zscale tonemap path.
    pub hdr: Option<String>,
    /// Source frame rate as ffprobe's `avg_frame_rate` rational, numerator
    /// and denominator (ADR-0052). Kept rational because the encoder derives
    /// its IDR interval from it: 24000/1001 is not 23.976, and rounding here
    /// drifts the 2 s grid over a title.
    pub video_frame_rate: Option<(i64, i64)>,
    /// Subtitle streams the container carries (ADR-0041 Decision 1). The
    /// parser no longer drops them; the probe persists one row per stream.
    pub subtitle_streams: Vec<ProbeSubtitleStream>,
}

/// One ffprobe-reported audio stream (ADR-0058 audio inventory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeAudioStream {
    /// Absolute stream index (`index` from ffprobe).
    pub stream_index: u32,
    /// `codec_name`; `unknown` when ffprobe reports no codec name.
    pub codec: String,
    pub language: Option<String>,
    pub channels: Option<i64>,
    pub channel_layout: Option<String>,
    pub title: Option<String>,
    /// `disposition.default == 1` (ADR-0058 `is_default`).
    pub is_default: bool,
}

/// One ffprobe-reported subtitle stream (ADR-0041 Decision 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSubtitleStream {
    /// Absolute stream index (`index` from ffprobe).
    pub stream_index: u32,
    /// `codec_name`; `unknown` when ffprobe reports an unmapped codec.
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub forced: bool,
    pub sdh: bool,
}

#[derive(Debug, Deserialize)]
struct FfprobeJson {
    format: Option<FfFormat>,
    streams: Option<Vec<FfStream>>,
}

#[derive(Debug, Deserialize)]
struct FfFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfStream {
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    channels: Option<i64>,
    channel_layout: Option<String>,
    width: Option<i32>,
    height: Option<i32>,
    bit_rate: Option<String>,
    avg_frame_rate: Option<String>,
    color_transfer: Option<String>,
    #[serde(default)]
    tags: Option<FfTags>,
    #[serde(default)]
    disposition: Option<FfDisposition>,
    #[serde(default)]
    side_data_list: Vec<FfSideData>,
}

#[derive(Debug, Default, Deserialize)]
struct FfTags {
    language: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FfDisposition {
    default: Option<i64>,
    forced: Option<i64>,
    hearing_impaired: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct FfSideData {
    side_data_type: Option<String>,
    /// Present on DOVI configuration records (ffprobe).
    dv_profile: Option<u64>,
}

/// Approved diagnostic retention policy. Excess stderr is discarded while the
/// pipe keeps draining; this is not a cap on ffprobe's stderr delivery.
const STDERR_TAIL: usize = 4 * 1024;

/// Approved structured-output retention policy. It is an operational refusal
/// policy, not a measured maximum or a promise that every valid media file
/// fits the allowance.
const METADATA_OUTPUT_BUDGET: usize = 16 * 1024 * 1024;

/// Approved maximum occupancy of a metadata operation.
const METADATA_DEADLINE: Duration = Duration::from_secs(120);

/// Cancellation/deadline observation granularity; it does not govern output
/// retention or operation lifetime.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Probe a media file with ffprobe. `should_cancel` is the library
/// reachability signal (ADR-0014 / ADR-0041 Decision 8.7 amendment): when it
/// turns true the ffprobe child is killed and the probe reports
/// `unavailable`, never `probed` or `error`.
///
/// The output pipes are drained concurrently on reader threads, like
/// `Command::output()` does: a stream-heavy title's JSON can exceed the pipe
/// buffer, and a drain-after-exit loop would let ffprobe block on a full pipe
/// forever (same shape as `keymap::packet_walk::walk`).
///
/// The wait wakes on the readers' EOF signal rather than sleeping a fixed
/// interval, so a completed probe is reported as soon as ffprobe closes its
/// output. The cancellation signal is still re-checked every `CANCEL_POLL`
/// while the child runs.
pub fn ffprobe(
    path: &Path,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Result<ProbeResult, String> {
    let mut command = Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn(&mut command)
        .map_err(|error| format!("{} for {}", error.message(), path.display()))?;
    let completed = supervise(
        child,
        METADATA_DEADLINE,
        CANCEL_POLL,
        should_cancel,
        |reader| read_limited(reader, METADATA_OUTPUT_BUDGET),
        |reader| StderrTail::read(reader, STDERR_TAIL),
    )
    .map_err(|error| {
        if error.kind == ChildFailureKind::Cancelled {
            format!("unavailable: {}", error.message())
        } else {
            format!("ffprobe for {}: {}", path.display(), error.message())
        }
    })?;

    if let Err(error) = completed.status {
        let tail = completed.stderr.display();
        return Err(format!("{error} for {}: {tail}", path.display()));
    }

    parse_probe_json(completed.stdout.as_slice(), path)
}

/// Fixed-size storage makes the requested 16 MiB allocation explicit and
/// avoids a geometric `Vec` capacity crossing the retention policy.
#[derive(Debug)]
struct LimitedBytes {
    storage: Box<[u8]>,
    len: usize,
}

impl LimitedBytes {
    fn as_slice(&self) -> &[u8] {
        &self.storage[..self.len]
    }
}

fn read_limited<R: Read>(mut reader: R, limit: usize) -> Result<LimitedBytes, ChildFailure> {
    let mut storage = vec![0_u8; limit].into_boxed_slice();
    let mut len = 0usize;
    loop {
        if len == limit {
            let mut extra = [0_u8; 1];
            return match reader.read(&mut extra)? {
                0 => Ok(LimitedBytes { storage, len }),
                _ => Err(ChildFailure::new(
                    ChildFailureKind::OutputBudget,
                    format!("ffprobe metadata stdout exceeds {limit} byte policy"),
                )),
            };
        }
        let read = reader.read(&mut storage[len..])?;
        if read == 0 {
            return Ok(LimitedBytes { storage, len });
        }
        len += read;
    }
}

/// Parse one `-show_format -show_streams` JSON document into a `ProbeResult`
/// (ADR-0058).
///
/// The audio and subtitle inventories are keyed by absolute stream index, so a
/// stream without one is rejected rather than silently dropped: publication is
/// all-or-nothing, and a partial inventory would present a track set the
/// container does not have. The rejection reuses the bounded
/// `parse ffprobe json` error shape — it names the stream type and never
/// embeds stream data — so a broken container cannot grow `scan_error`
/// without bound.
///
/// Both inventories come back ordered by absolute stream index, which is the
/// order ADR-0058's read path returns. ffprobe's own emission order is not
/// part of its contract, so the parser does not rely on it.
fn parse_probe_json(json: &[u8], path: &Path) -> Result<ProbeResult, String> {
    let parsed: FfprobeJson = serde_json::from_slice(json)
        .map_err(|e| format!("parse ffprobe json for {}: {e}", path.display()))?;

    let duration_ms = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_ref())
        .and_then(|d| d.parse::<f64>().ok())
        .map(|s| (s * 1000.0).round() as i64);

    let format_bitrate = parsed
        .format
        .as_ref()
        .and_then(|f| f.bit_rate.as_ref())
        .and_then(|b| b.parse::<i64>().ok())
        .filter(|&b| b > 0);

    let container = parsed
        .format
        .and_then(|f| f.format_name)
        .map(|s| s.split(',').next().unwrap_or("unknown").trim().to_string());

    let missing_index = |kind: &str| {
        format!(
            "parse ffprobe json for {}: {kind} stream without absolute index",
            path.display()
        )
    };

    let mut video_codec = None;
    let mut video_stream_index = None;
    // Selection is by encounter order, not by codec presence: a first video
    // stream without `codec_name` must not let a later video stream overwrite
    // it. `video_stream_index` cannot serve as the guard because a selected
    // stream without an index rejects the result instead of being recorded.
    let mut video_selected = false;
    let mut audio_codec = None;
    let mut audio_channels = None;
    let mut width = None;
    let mut height = None;
    let mut video_bitrate_bps = None;
    let mut video_frame_rate = None;
    let mut hdr = None;
    let mut audio_streams: Vec<ProbeAudioStream> = Vec::new();
    let mut subtitle_streams: Vec<ProbeSubtitleStream> = Vec::new();
    for stream in parsed.streams.unwrap_or_default() {
        match stream.codec_type.as_deref() {
            Some("video") if !video_selected => {
                // The selected stream is the one publication keys on, so a
                // missing index invalidates the whole result (ADR-0058).
                let Some(index) = stream.index else {
                    return Err(missing_index("video"));
                };
                video_selected = true;
                video_stream_index = Some(index);
                video_codec = stream.codec_name;
                width = stream.width;
                height = stream.height;
                video_bitrate_bps = stream
                    .bit_rate
                    .as_ref()
                    .and_then(|b| b.parse::<i64>().ok())
                    .filter(|&b| b > 0)
                    .or(format_bitrate);
                video_frame_rate = parse_frame_rate(stream.avg_frame_rate.as_deref());
                hdr = Some(classify_hdr(
                    stream.color_transfer.as_deref(),
                    &stream.side_data_list,
                ));
            }
            Some("audio") => {
                let Some(index) = stream.index else {
                    return Err(missing_index("audio"));
                };
                let tags = stream.tags.unwrap_or_default();
                let disp = stream.disposition.unwrap_or_default();
                audio_streams.push(ProbeAudioStream {
                    stream_index: index,
                    codec: stream.codec_name.unwrap_or_else(|| "unknown".to_string()),
                    language: tags.language,
                    channels: stream.channels,
                    channel_layout: stream.channel_layout,
                    title: tags.title.filter(|t| !t.is_empty()),
                    is_default: disp.default == Some(1),
                });
            }
            Some("subtitle") => {
                let Some(index) = stream.index else {
                    return Err(missing_index("subtitle"));
                };
                let tags = stream.tags.unwrap_or_default();
                let disp = stream.disposition.unwrap_or_default();
                subtitle_streams.push(ProbeSubtitleStream {
                    stream_index: index,
                    codec: stream.codec_name.unwrap_or_else(|| "unknown".to_string()),
                    language: tags.language,
                    title: tags.title.filter(|t| !t.is_empty()),
                    forced: disp.forced == Some(1),
                    sdh: disp.hearing_impaired == Some(1),
                });
            }
            _ => {}
        }
    }

    audio_streams.sort_by_key(|s| s.stream_index);
    subtitle_streams.sort_by_key(|s| s.stream_index);

    // ADR-0012 compatibility projection: the first audio track (lowest
    // absolute index, now that the inventory is ordered) supplies the codec
    // and channel count the old scalar columns carried.
    if let Some(first) = audio_streams.first() {
        audio_codec = Some(first.codec.clone());
        audio_channels = first.channels;
    }

    Ok(ProbeResult {
        duration_ms,
        container,
        video_codec,
        video_stream_index,
        audio_codec,
        audio_channels,
        audio_streams,
        width,
        height,
        video_bitrate_bps,
        video_frame_rate,
        hdr,
        subtitle_streams,
    })
}

/// Parse ffprobe's `avg_frame_rate` (`"24000/1001"`). Returns `None` for the
/// forms that carry no rate: absent, `"0/0"` on a stream ffprobe could not
/// time, and any zero denominator.
fn parse_frame_rate(raw: Option<&str>) -> Option<(i64, i64)> {
    let (num, den) = raw?.split_once('/')?;
    let num: i64 = num.trim().parse().ok()?;
    let den: i64 = den.trim().parse().ok()?;
    if num <= 0 || den <= 0 {
        return None;
    }
    Some((num, den))
}

fn classify_hdr(color_transfer: Option<&str>, side_data: &[FfSideData]) -> String {
    for side in side_data {
        let Some(t) = side.side_data_type.as_deref() else {
            continue;
        };
        let lower = t.to_ascii_lowercase();
        if lower.contains("dovi") || lower.contains("dolby vision") {
            // Profile 5 is IPT-PQ; zscale+hable cannot map it (no colourspace path).
            if side.dv_profile == Some(5) {
                return "dolby_vision_p5".into();
            }
            return "dolby_vision".into();
        }
    }
    match color_transfer.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("smpte2084") | Some("arib-std-b67") => "hdr10".into(),
        _ => "none".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

    #[test]
    fn process_failure_includes_exit_code() {
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_none()
            && !Command::new("ffprobe")
                .arg("-version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        {
            return;
        }
        let path = PathBuf::from("/tmp/nightjar-definitely-missing-probe-target.mkv");
        let err = ffprobe(&path, None).unwrap_err();
        assert!(
            err.contains("exit ") || err.starts_with("spawn ffprobe"),
            "expected exit code or spawn, got {err}"
        );
        if err.starts_with("ffprobe failed") {
            assert!(!err.ends_with(": "), "empty body after colon: {err}");
        }
    }

    #[test]
    fn metadata_retention_accepts_the_limit_and_rejects_one_extra_byte() {
        let below_limit = read_limited(Cursor::new(b"123"), 4).unwrap();
        assert_eq!(below_limit.as_slice(), b"123");
        assert_eq!(below_limit.storage.len(), 4);
        let at_limit = read_limited(Cursor::new(b"1234"), 4).expect("exact limit is retained");
        assert_eq!(at_limit.as_slice(), b"1234");
        assert_eq!(at_limit.storage.len(), 4, "requested allocation high-water");

        let error = read_limited(Cursor::new(b"12345"), 4).expect_err("one extra byte fails");
        assert_eq!(error.kind, ChildFailureKind::OutputBudget);
        assert!(error.message().contains("exceeds 4 byte policy"), "{error}");
        eprintln!(
            "metadata below={} exact={} allocation={} above={:?}",
            below_limit.len,
            at_limit.len,
            at_limit.storage.len(),
            error.kind
        );
    }

    #[test]
    fn classify_hdr_dovi_and_pq() {
        assert_eq!(classify_hdr(None, &[]), "none");
        assert_eq!(classify_hdr(Some("smpte2084"), &[]), "hdr10");
        assert_eq!(
            classify_hdr(
                Some("bt709"),
                &[FfSideData {
                    side_data_type: Some("DOVI configuration record".into()),
                    dv_profile: Some(8),
                }]
            ),
            "dolby_vision"
        );
        assert_eq!(
            classify_hdr(
                None,
                &[FfSideData {
                    side_data_type: Some("DOVI configuration record".into()),
                    dv_profile: Some(5),
                }]
            ),
            "dolby_vision_p5"
        );
    }

    /// Parse a JSON document through the production entry point, so every test
    /// below exercises the same path `ffprobe` uses.
    fn parse(json: &str) -> Result<ProbeResult, String> {
        parse_probe_json(json.as_bytes(), Path::new("probe-json-test"))
    }

    /// ADR-0058: the first video stream's absolute index, complete audio and
    /// subtitle inventories, ordered by absolute stream index, with every
    /// optional track value carried through. The JSON lists audio index 3 and
    /// subtitle index 5 before their lower-indexed siblings, so a parser that
    /// trusted ffprobe's emission order would fail the order assertions.
    #[test]
    fn parse_json_carries_complete_inventories_in_absolute_index_order() {
        let p = parse(
            r#"{
              "format": {
                "format_name": "matroska,webm",
                "duration": "120.5",
                "bit_rate": "8000000"
              },
              "streams": [
                {
                  "index": 0,
                  "codec_type": "video",
                  "codec_name": "hevc",
                  "width": 1920,
                  "height": 1080,
                  "bit_rate": "7000000",
                  "avg_frame_rate": "24000/1001",
                  "color_transfer": "smpte2084"
                },
                {
                  "index": 3,
                  "codec_type": "audio",
                  "codec_name": "ac3",
                  "channels": 6,
                  "channel_layout": "5.1(side)",
                  "disposition": { "default": 0 },
                  "tags": { "language": "fre", "title": "VFF" }
                },
                {
                  "index": 5,
                  "codec_type": "subtitle",
                  "codec_name": "hdmv_pgs_subtitle",
                  "disposition": { "forced": 0, "hearing_impaired": 1 },
                  "tags": { "language": "eng" }
                },
                {
                  "index": 2,
                  "codec_type": "subtitle",
                  "codec_name": "subrip",
                  "disposition": { "forced": 1, "hearing_impaired": 0 }
                },
                {
                  "index": 1,
                  "codec_type": "audio",
                  "codec_name": "aac",
                  "channels": 2,
                  "channel_layout": "stereo",
                  "disposition": { "default": 1 },
                  "tags": { "language": "eng", "title": "Main" }
                }
              ]
            }"#,
        )
        .expect("valid complete probe json");

        assert_eq!(p.video_stream_index, Some(0));
        assert_eq!(p.duration_ms, Some(120_500));
        assert_eq!(p.container.as_deref(), Some("matroska"));
        assert_eq!(p.video_codec.as_deref(), Some("hevc"));
        assert_eq!(p.width, Some(1920));
        assert_eq!(p.height, Some(1080));
        assert_eq!(p.video_bitrate_bps, Some(7_000_000));
        assert_eq!(p.video_frame_rate, Some((24_000, 1_001)));
        assert_eq!(p.hdr.as_deref(), Some("hdr10"));

        assert_eq!(
            p.audio_streams
                .iter()
                .map(|s| s.stream_index)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "audio inventory must be ordered by absolute stream index"
        );
        let first_audio = &p.audio_streams[0];
        assert_eq!(first_audio.codec, "aac");
        assert_eq!(first_audio.channels, Some(2));
        assert_eq!(first_audio.channel_layout.as_deref(), Some("stereo"));
        assert_eq!(first_audio.language.as_deref(), Some("eng"));
        assert_eq!(first_audio.title.as_deref(), Some("Main"));
        assert!(first_audio.is_default);
        let second_audio = &p.audio_streams[1];
        assert_eq!(second_audio.codec, "ac3");
        assert_eq!(second_audio.channels, Some(6));
        assert_eq!(second_audio.channel_layout.as_deref(), Some("5.1(side)"));
        assert_eq!(second_audio.title.as_deref(), Some("VFF"));
        assert!(!second_audio.is_default);

        // ADR-0012 projection: the first audio track after ordering.
        assert_eq!(p.audio_codec.as_deref(), Some("aac"));
        assert_eq!(p.audio_channels, Some(2));

        assert_eq!(
            p.subtitle_streams
                .iter()
                .map(|s| s.stream_index)
                .collect::<Vec<_>>(),
            vec![2, 5],
            "subtitle inventory must be ordered by absolute stream index"
        );
        assert!(p.subtitle_streams[0].forced);
        assert!(!p.subtitle_streams[0].sdh);
        assert!(!p.subtitle_streams[1].forced);
        assert!(p.subtitle_streams[1].sdh);
        assert_eq!(p.subtitle_streams[1].language.as_deref(), Some("eng"));
    }

    /// ADR-0058: codec `unknown` when `codec_name` is absent, and every
    /// optional value that ffprobe omits or leaves empty stays `None`.
    #[test]
    fn parse_json_uses_unknown_codec_and_preserves_absent_optionals() {
        let p = parse(
            r#"{
              "streams": [
                { "index": 0, "codec_type": "video" },
                { "index": 1, "codec_type": "audio", "disposition": {} },
                {
                  "index": 3,
                  "codec_type": "audio",
                  "codec_name": "opus",
                  "tags": { "language": "eng", "title": "" }
                },
                { "index": 2, "codec_type": "subtitle" }
              ]
            }"#,
        )
        .expect("valid probe json");

        assert_eq!(p.video_stream_index, Some(0));
        assert_eq!(p.video_codec, None);
        assert_eq!(p.duration_ms, None);
        assert_eq!(p.container, None);

        assert_eq!(p.audio_streams.len(), 2);
        let first = &p.audio_streams[0];
        assert_eq!(first.stream_index, 1);
        assert_eq!(first.codec, "unknown");
        assert_eq!(first.language, None);
        assert_eq!(first.channels, None);
        assert_eq!(first.channel_layout, None);
        assert_eq!(first.title, None);
        assert!(!first.is_default);
        let second = &p.audio_streams[1];
        assert_eq!(second.codec, "opus");
        assert_eq!(second.language.as_deref(), Some("eng"));
        assert_eq!(second.title, None, "an empty title is not a title");

        assert_eq!(p.subtitle_streams.len(), 1);
        assert_eq!(p.subtitle_streams[0].codec, "unknown");
        assert_eq!(p.subtitle_streams[0].language, None);
        assert_eq!(p.subtitle_streams[0].title, None);
        assert!(!p.subtitle_streams[0].forced);
        assert!(!p.subtitle_streams[0].sdh);
    }

    /// ADR-0058: the selected video stream is the first one by encounter
    /// order, independent of `codec_name`. A first video stream without a
    /// codec must not let a later, codec-bearing video stream overwrite its
    /// index or scalars. The old guard keyed on `video_codec.is_none()`, so it
    /// stayed true past the first stream and this JSON reported the second
    /// stream's index, codec and dimensions.
    #[test]
    fn parse_json_selects_the_first_video_stream_even_without_codec_name() {
        let p = parse(
            r#"{
              "streams": [
                {
                  "index": 0,
                  "codec_type": "video",
                  "width": 1280,
                  "height": 720,
                  "avg_frame_rate": "24000/1001"
                },
                {
                  "index": 1,
                  "codec_type": "video",
                  "codec_name": "hevc",
                  "width": 1920,
                  "height": 1080,
                  "avg_frame_rate": "30000/1001"
                }
              ]
            }"#,
        )
        .expect("valid probe json");

        assert_eq!(
            p.video_stream_index,
            Some(0),
            "the first video stream must stay selected"
        );
        assert_eq!(
            p.video_codec, None,
            "video keeps `unknown` semantics; a missing codec is not a fallback"
        );
        assert_eq!(p.width, Some(1280));
        assert_eq!(p.height, Some(720));
        assert_eq!(p.video_frame_rate, Some((24_000, 1_001)));
    }

    /// ADR-0058: only the selected video stream's missing index invalidates
    /// the result. Once the first video stream is selected, a later video
    /// stream without an index is not part of the selection and must not
    /// reject the whole probe. The old guard re-entered the arm for a later
    /// stream (the first had no `codec_name`) and returned `missing_index`.
    #[test]
    fn parse_json_ignores_a_later_video_stream_without_an_index() {
        let p = parse(
            r#"{
              "streams": [
                { "index": 0, "codec_type": "video", "width": 1280, "height": 720 },
                { "codec_type": "video", "codec_name": "hevc" }
              ]
            }"#,
        )
        .expect("only the selected video stream's index is required");

        assert_eq!(p.video_stream_index, Some(0));
        assert_eq!(p.width, Some(1280));
        assert_eq!(p.height, Some(720));
    }

    /// ADR-0058: a stream the inventory must key but that carries no absolute
    /// index invalidates the whole result. The error is the bounded parse
    /// shape, so the huge title on the offending stream never reaches
    /// `scan_error`.
    #[test]
    fn parse_json_rejects_streams_without_an_absolute_index() {
        let video = parse(r#"{"streams":[{"codec_type":"video"}]}"#)
            .expect_err("a selected video stream without an index must be rejected");
        assert!(
            video.contains("video stream without absolute index"),
            "{video}"
        );

        let audio =
            parse(r#"{"streams":[{"index":0,"codec_type":"video"},{"codec_type":"audio"}]}"#)
                .expect_err("an audio stream without an index must be rejected");
        assert!(
            audio.contains("audio stream without absolute index"),
            "{audio}"
        );

        let loud_title = "A".repeat(4096);
        let subtitle = parse(&format!(
            r#"{{"streams":[{{"codec_type":"subtitle","tags":{{"title":"{loud_title}"}}}}]}}"#
        ))
        .expect_err("a subtitle stream without an index must be rejected");
        assert!(
            subtitle.contains("subtitle stream without absolute index"),
            "{subtitle}"
        );
        assert!(
            subtitle.starts_with("parse ffprobe json for probe-json-test:"),
            "the rejection must keep the bounded parse-error shape: {subtitle}"
        );
        assert!(
            subtitle.len() < 256,
            "the rejection embedded stream data: {} bytes",
            subtitle.len()
        );
        assert!(
            !subtitle.contains(&"A".repeat(64)),
            "the rejection echoed the stream title"
        );
    }

    /// A malformed document keeps the pre-existing parse-error shape and still
    /// names the file, so a broken container is not silently reported probed.
    #[test]
    fn parse_json_rejects_a_malformed_document() {
        let err = parse("{not json").expect_err("malformed json must not parse");
        assert!(
            err.starts_with("parse ffprobe json for probe-json-test:"),
            "{err}"
        );
    }
}
