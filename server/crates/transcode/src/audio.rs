//! Audio track inventory and stereo downmix matrices (ADR-0012).

use super::subs::container_stream_language;
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// One audio stream in the source. Probed on demand at playback-info time;
/// only the first-audio channel count is stored on the item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStream {
    /// Absolute ffprobe stream index (`-map 0:N`).
    pub stream_index: u32,
    pub codec: String,
    pub language: Option<String>,
    pub channels: u32,
    pub channel_layout: Option<String>,
    pub title: Option<String>,
    /// Exactly one track in a listing carries this: the container's flagged
    /// default, else the first.
    pub is_default: bool,
}

impl AudioStream {
    pub fn track_id(&self) -> String {
        format!("e{}", self.stream_index)
    }
}

/// Lists audio streams in `src` in container order.
pub fn list_audio_tracks(src: &Path) -> Result<Vec<AudioStream>, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-select_streams",
            "a",
        ])
        .arg(src)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "ffprobe not found on PATH".into()
            } else {
                format!("spawn ffprobe for {}: {e}", src.display())
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffprobe failed for {}: {}",
            src.display(),
            stderr.trim()
        ));
    }
    let parsed: FfprobeAudio = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("parse ffprobe json for {}: {e}", src.display()))?;

    let mut out = Vec::new();
    let mut flagged = None;
    for stream in parsed.streams.unwrap_or_default() {
        let Some(index) = stream.index else {
            continue;
        };
        let tags = stream.tags.unwrap_or_default();
        if flagged.is_none() && stream.disposition.unwrap_or_default().default == 1 {
            flagged = Some(out.len());
        }
        out.push(AudioStream {
            stream_index: index,
            codec: stream.codec_name.unwrap_or_default(),
            language: container_stream_language(tags.language),
            channels: stream.channels.unwrap_or(0),
            channel_layout: stream.channel_layout.filter(|s| !s.is_empty()),
            title: tags.title.filter(|s| !s.is_empty()),
            is_default: false,
        });
    }
    if let Some(track) = out.get_mut(flagged.unwrap_or(0)) {
        track.is_default = true;
    }
    Ok(out)
}

/// Stereo pan matrix for a layout above the client channel ceiling
/// (ADR-0012). Centre is mixed into both ears at full gain so dialogue
/// survives; LFE rides at 0.5 so explosions keep weight without burying it.
/// `None` means no table for this layout: the caller falls back to `-ac 2`.
///
/// Layouts are matched by name, not channel count: `6.0` and `5.1(side)`
/// also report six channels but do not share the 5.1(back) index map, and
/// applying that matrix can drop dialogue. Unknown named layouts fall back.
/// When ffprobe omits the layout, channel count is the last-resort guess
/// (anonymous AAC often ships as "6 channels" with no layout tag).
pub fn stereo_downmix_filter(channels: u32, channel_layout: Option<&str>) -> Option<String> {
    // FFmpeg native order for 5.1(back): FL FR FC LFE BL BR; 7.1 adds SL SR.
    let (front, centre, lfe, surround) = ("0.707", "1.0", "0.5", "0.707");
    let five_one = || {
        format!(
            "pan=stereo\
             |c0={front}*c0+{centre}*c2+{surround}*c4+{lfe}*c3\
             |c1={front}*c1+{centre}*c2+{surround}*c5+{lfe}*c3"
        )
    };
    let seven_one = || {
        format!(
            "pan=stereo\
             |c0={front}*c0+{centre}*c2+{surround}*c4+{surround}*c6+{lfe}*c3\
             |c1={front}*c1+{centre}*c2+{surround}*c5+{surround}*c7+{lfe}*c3"
        )
    };
    match channel_layout {
        Some("5.1") | Some("5.1(back)") => Some(five_one()),
        Some("7.1") | Some("7.1(wide)") => Some(seven_one()),
        Some(_) => None,
        None => match channels {
            6 => Some(five_one()),
            8 => Some(seven_one()),
            _ => None,
        },
    }
}

#[derive(Debug, Deserialize)]
struct FfprobeAudio {
    streams: Option<Vec<FfAudioStream>>,
}

#[derive(Debug, Deserialize)]
struct FfAudioStream {
    index: Option<u32>,
    codec_name: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    disposition: Option<FfDisposition>,
    tags: Option<FfAudioTags>,
}

#[derive(Debug, Default, Deserialize)]
struct FfDisposition {
    #[serde(default)]
    default: u8,
}

#[derive(Debug, Default, Deserialize)]
struct FfAudioTags {
    language: Option<String>,
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn embedded_track_id_matches_subtitle_scheme() {
        let s = AudioStream {
            stream_index: 3,
            codec: "aac".into(),
            language: Some("en".into()),
            channels: 2,
            channel_layout: Some("stereo".into()),
            title: None,
            is_default: false,
        };
        assert_eq!(s.track_id(), "e3");
    }

    #[test]
    fn downmix_matrix_table() {
        let five_one = stereo_downmix_filter(6, Some("5.1")).expect("5.1 has a matrix");
        assert_eq!(
            five_one,
            "pan=stereo|c0=0.707*c0+1.0*c2+0.707*c4+0.5*c3|c1=0.707*c1+1.0*c2+0.707*c5+0.5*c3"
        );
        assert_eq!(
            stereo_downmix_filter(6, Some("5.1(back)")).as_deref(),
            Some(five_one.as_str())
        );
        let seven_one = stereo_downmix_filter(8, Some("7.1")).expect("7.1 has a matrix");
        assert_eq!(
            seven_one,
            "pan=stereo|c0=0.707*c0+1.0*c2+0.707*c4+0.707*c6+0.5*c3\
             |c1=0.707*c1+1.0*c2+0.707*c5+0.707*c7+0.5*c3"
        );
        // Silent LFE drops are the "sounds empty" bug report; keep the term.
        for matrix in [&five_one, &seven_one] {
            assert!(matrix.contains("0.5*c3"), "LFE missing from {matrix}");
            assert_eq!(
                matrix.matches("1.0*c2").count(),
                2,
                "centre must reach both ears"
            );
        }
        // Same channel count as 5.1, wrong index map — must not borrow the table.
        assert!(stereo_downmix_filter(6, Some("6.0")).is_none());
        assert!(stereo_downmix_filter(6, Some("5.1(side)")).is_none());
        // No table: the caller falls back to -ac 2 rather than to silence.
        for odd in [1, 2, 3, 4, 5, 7, 12] {
            assert!(
                stereo_downmix_filter(odd, None).is_none(),
                "{odd} has no table"
            );
        }
        // Anonymous 6/8-channel (no layout tag) keeps the channel-count guess.
        assert!(stereo_downmix_filter(6, None).is_some());
        assert!(stereo_downmix_filter(8, None).is_some());
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn lists_every_track_with_language_and_one_default() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_multilang_mkv.mkv");
        if !corpus.exists() {
            eprintln!("skipping: missing {}", corpus.display());
            return;
        }
        let tracks = list_audio_tracks(&corpus).expect("list");
        assert_eq!(tracks.len(), 2, "{tracks:?}");
        let langs: Vec<_> = tracks.iter().map(|t| t.language.as_deref()).collect();
        assert_eq!(langs, vec![Some("en"), Some("es")]);
        assert_eq!(tracks.iter().filter(|t| t.is_default).count(), 1);
        assert!(tracks[0].is_default);
        assert!(tracks.iter().all(|t| t.channels == 2));
        assert_eq!(tracks[1].track_id(), format!("e{}", tracks[1].stream_index));
    }

    /// The pan matrix has to survive FFmpeg's filter parser, not just ours.
    #[test]
    fn ffmpeg_accepts_the_matrices() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for (channels, layout) in [(6u32, "5.1"), (8, "7.1")] {
            let out = dir.path().join(format!("{channels}.wav"));
            let filter = stereo_downmix_filter(channels, Some(layout)).unwrap();
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
                    &format!("anullsrc=r=48000:cl={layout}:d=1"),
                    "-af",
                    &filter,
                ])
                .arg(&out)
                .status()
                .unwrap();
            assert!(status.success(), "{layout}: ffmpeg rejected {filter}");
            assert!(fs::metadata(&out).unwrap().len() > 0, "{layout}: no output");
        }
    }
}
