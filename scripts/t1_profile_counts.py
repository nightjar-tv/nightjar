#!/usr/bin/env python3
"""Count directPlay / remux / transcode under client capability profiles.

Mirrors server/crates/core/src/playback.rs::decide_playback against the
dogfood SQLite index. Profiles are capability sets, not optimism.

Usage:
  python3 scripts/t1_profile_counts.py [/path/to/nightjar.db]
  NIGHTJAR_DATA_DIR=~/nightjar-data python3 scripts/t1_profile_counts.py
"""

from __future__ import annotations

import os
import sqlite3
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


@dataclass(frozen=True)
class Profile:
    name: str
    video_codecs: frozenset[str]
    audio_codecs: frozenset[str]
    containers: frozenset[str]
    extensions: frozenset[str]
    max_audio_channels: Optional[int]
    notes: str


# AVPlayer / AVFoundation on Apple platforms. Will not open Matroska.
# HEVC + AAC/AC-3/E-AC-3 in MP4/MOV is the common direct-play set on modern
# tvOS/iOS; DTS/TrueHD/Opus/FLAC force a session; AV1 is not assumed for V0
# (device-generation split).
APPLE_AVPLAYER_V0 = Profile(
    name="APPLE_AVPLAYER_V0",
    video_codecs=frozenset({"h264", "avc", "avc1", "hevc", "h265", "hev1", "hvc1"}),
    audio_codecs=frozenset({"aac", "mp4a", "ac3", "eac3", "mp3", "alac"}),
    containers=frozenset({"mp4", "m4v", "mov"}),
    extensions=frozenset({"mp4", "m4v", "mov"}),
    max_audio_channels=None,
    notes="No Matroska. No DTS/TrueHD/FLAC/Opus/Vorbis. No AV1/VP9/MPEG-4 for V0.",
)

# Media3 / ExoPlayer on a typical Android TV (2020+): Matroska demux is native;
# HEVC/VP9/AV1 decode is common; DTS/TrueHD usually need a licensed decoder and
# are not claimed for V0.
ANDROID_MEDIA3_V0 = Profile(
    name="ANDROID_MEDIA3_V0",
    video_codecs=frozenset(
        {
            "h264",
            "avc",
            "avc1",
            "hevc",
            "h265",
            "hev1",
            "hvc1",
            "vp9",
            "av1",
            "mpeg4",
        }
    ),
    audio_codecs=frozenset(
        {"aac", "mp4a", "ac3", "eac3", "mp3", "opus", "flac", "vorbis"}
    ),
    containers=frozenset({"matroska", "webm", "mp4", "m4v", "mov", "avi"}),
    extensions=frozenset({"mkv", "webm", "mp4", "m4v", "mov", "avi"}),
    max_audio_channels=None,
    notes="Matroska yes. No DTS/TrueHD for V0 (licence/device split).",
)

# libmpv / mpv defaults: demux + decode the codecs named in the bake-off brief.
# PGS/ASS are subtitle capabilities (out of band for decide_playback).
_ENGINE_VIDEO = frozenset(
    {
        "h264",
        "avc",
        "avc1",
        "hevc",
        "h265",
        "hev1",
        "hvc1",
        "av1",
        "vp9",
        "mpeg4",
        "mpeg2video",
        "vc1",
    }
)
_ENGINE_AUDIO = frozenset(
    {
        "aac",
        "mp4a",
        "ac3",
        "eac3",
        "mp3",
        "opus",
        "flac",
        "vorbis",
        "dts",
        "truehd",
        "mlp",
        "pcm_s16le",
        "pcm_s24le",
        "pcm_bluray",
        "alac",
    }
)
_ENGINE_CONTAINERS = frozenset(
    {"matroska", "webm", "mp4", "m4v", "mov", "avi", "mpegts", "mpeg"}
)
_ENGINE_EXTENSIONS = frozenset(
    {"mkv", "webm", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "mpg", "mpeg"}
)

MPV_V0 = Profile(
    name="MPV_V0",
    video_codecs=_ENGINE_VIDEO,
    audio_codecs=_ENGINE_AUDIO,
    containers=_ENGINE_CONTAINERS,
    extensions=_ENGINE_EXTENSIONS,
    max_audio_channels=None,
    notes="Matroska, HEVC, AV1, VP9, DTS, TrueHD, FLAC, Opus; PGS/ASS via libmpv.",
)

VLC_V0 = Profile(
    name="VLC_V0",
    video_codecs=_ENGINE_VIDEO,
    audio_codecs=_ENGINE_AUDIO,
    containers=_ENGINE_CONTAINERS,
    extensions=_ENGINE_EXTENSIONS,
    max_audio_channels=None,
    notes="Same codec/container floor as MPV_V0 for this count (brief parity).",
)

PROFILES = (APPLE_AVPLAYER_V0, ANDROID_MEDIA3_V0, MPV_V0, VLC_V0)


def matches_codec(codec: Optional[str], accepted: frozenset[str]) -> bool:
    if codec is None or codec == "":
        return False
    return codec.lower() in accepted


def matches_container(path: str, container: Optional[str], profile: Profile) -> bool:
    path_l = path.lower()
    for ext in profile.extensions:
        if path_l.endswith("." + ext):
            return True
    c = (container or "").lower()
    return any(part.strip() in profile.containers for part in c.split(","))


def channel_ceiling_forces_session(
    audio_channels: Optional[int], profile: Profile
) -> bool:
    if profile.max_audio_channels is None:
        return False
    if audio_channels is None:
        return True
    return audio_channels > profile.max_audio_channels


def decide(
    path: str,
    container: Optional[str],
    video_codec: Optional[str],
    audio_codec: Optional[str],
    audio_channels: Optional[int],
    scan_error: Optional[str],
    probe_status: str,
    profile: Profile,
) -> str:
    if probe_status in ("indexed", "unavailable"):
        return "transcode"
    if scan_error:
        return "transcode"

    video_ok = matches_codec(video_codec, profile.video_codecs)
    audio_ok = matches_codec(audio_codec, profile.audio_codecs)
    container_ok = matches_container(path, container, profile)

    if video_ok and audio_ok:
        if channel_ceiling_forces_session(audio_channels, profile):
            return "remux"
        if container_ok:
            return "directPlay"
        return "remux"
    return "transcode"


def default_db_path() -> Path:
    env = os.environ.get("NIGHTJAR_DATA_DIR")
    if env:
        return Path(env) / "nightjar.db"
    home = Path.home() / "nightjar-data" / "nightjar.db"
    if home.is_file():
        return home
    return Path("data") / "nightjar.db"


def load_rows(db: Path) -> list[tuple]:
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        cur = con.execute(
            """
            SELECT path, container, video_codec, audio_codec, audio_channels,
                   scan_error, probe_status
            FROM media_items
            """
        )
        return cur.fetchall()
    finally:
        con.close()


def count_profile(rows: list[tuple], profile: Profile) -> Counter:
    counts: Counter = Counter()
    for path, container, video, audio, channels, scan_error, probe_status in rows:
        method = decide(
            path or "",
            container,
            video,
            audio,
            channels,
            scan_error,
            probe_status or "probed",
            profile,
        )
        counts[method] += 1
    return counts


def main() -> int:
    db = Path(sys.argv[1]) if len(sys.argv) > 1 else default_db_path()
    if not db.is_file():
        print(f"database not found: {db}", file=sys.stderr)
        return 1

    rows = load_rows(db)
    total = len(rows)
    print(f"db={db}")
    print(f"items={total}")
    print()

    methods = ("directPlay", "remux", "transcode")
    header = f"{'profile':<22} " + " ".join(f"{m:>12}" for m in methods)
    print(header)
    print("-" * len(header))

    for profile in PROFILES:
        counts = count_profile(rows, profile)
        cells = []
        for m in methods:
            n = counts[m]
            pct = (100.0 * n / total) if total else 0.0
            cells.append(f"{n:>5} {pct:5.1f}%")
        print(f"{profile.name:<22} " + " ".join(f"{c:>12}" for c in cells))
        print(f"  # {profile.notes}")

    print()
    print("Decision engine: nightjar-core decide_playback rules (probe pending/")
    print("failed → transcode; codecs ok + container ok → directPlay; codecs ok")
    print("+ wrong container → remux; else transcode). Channel ceiling only when set.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
