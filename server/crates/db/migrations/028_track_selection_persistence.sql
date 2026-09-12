-- ADR-0038 amendment 2026-09-12 (B2-8): profile track-selection defaults and
-- the per-series override table.
--
-- `preferred_language` is one field for audio and subtitles (ADR-0038 item 1);
-- null means no preference, which behaves as ADR-0024's no-preference case.
-- `subtitle_default` is `auto` or `off`: `off` is a real choice and is not the
-- same as having no preference (item 1).
--
-- `profile_track_choice` stores descriptions, never stream indices (item 2),
-- keyed `(profile_id, series_key)` with the ADR-0039 key. The audio description
-- is present when `audio_kind` is non-null and is all-or-nothing, so a
-- half-written description cannot exist; `audio_language` may still be null.
-- The subtitle choice is three-valued so "turned off for this show" is distinct
-- from "not chosen" (item 4): description columns are all null unless the mode
-- is `track`, and all non-null except `subtitle_language` when it is.
-- `updated_at` is server time only, and the ADR-0039 item 7 migrator merges on
-- it when two folders bind one show.

ALTER TABLE profiles ADD COLUMN preferred_language TEXT;

ALTER TABLE profiles ADD COLUMN subtitle_default TEXT NOT NULL DEFAULT 'auto'
    CHECK (subtitle_default IN ('auto', 'off'));

CREATE TABLE profile_track_choice (
    profile_id INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    series_key TEXT NOT NULL,
    audio_language TEXT,
    audio_kind TEXT CHECK (audio_kind IN ('main', 'commentary', 'signs')),
    audio_sdh INTEGER CHECK (audio_sdh IN (0, 1)),
    audio_forced INTEGER CHECK (audio_forced IN (0, 1)),
    subtitle_mode TEXT NOT NULL CHECK (subtitle_mode IN ('unset', 'off', 'track')),
    subtitle_language TEXT,
    subtitle_kind TEXT CHECK (subtitle_kind IN ('main', 'commentary', 'signs')),
    subtitle_sdh INTEGER CHECK (subtitle_sdh IN (0, 1)),
    subtitle_forced INTEGER CHECK (subtitle_forced IN (0, 1)),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (profile_id, series_key),
    -- The audio description is present as a set: either all four columns are
    -- null, or `kind`, `sdh` and `forced` are present (`language` may be null).
    CHECK (
        (audio_kind IS NULL AND audio_sdh IS NULL
            AND audio_forced IS NULL AND audio_language IS NULL)
        OR
        (audio_kind IS NOT NULL AND audio_sdh IS NOT NULL AND audio_forced IS NOT NULL)
    ),
    -- Description columns exist only for `track`; `unset` and `off` carry none.
    CHECK (
        (subtitle_mode IN ('unset', 'off')
            AND subtitle_language IS NULL AND subtitle_kind IS NULL
            AND subtitle_sdh IS NULL AND subtitle_forced IS NULL)
        OR
        (subtitle_mode = 'track'
            AND subtitle_kind IS NOT NULL AND subtitle_sdh IS NOT NULL
            AND subtitle_forced IS NOT NULL)
    )
);
