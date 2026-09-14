-- ADR-0058: revision-safe atomic probe snapshots. Technical media facts are
-- observations of mutable files, so they are certified against a media
-- revision and a content identity rather than trusted on their own.
--
-- `media_revision` moves only when the scanner accepts a change to the
-- observed bytes, the source path, or the library-root binding. `probe_revision`
-- counts accepted publications and is independent of media identity.
-- `probed_media_revision` and `probed_content_id` are the validity stamps: a
-- read is `Ready` only when both match the item's current revision and identity.
-- `video_stream_index` is the selected absolute video stream index.
--
-- Existing rows keep their technical facts and statuses; they read revision 1
-- with probe revision 0 and NULL validity stamps. Subtitle rows carry probe
-- revision 0 and the audio inventory arrives empty. Nothing here probes: this
-- migration only adds columns and a table.

ALTER TABLE media_items
    ADD COLUMN media_revision INTEGER NOT NULL DEFAULT 1 CHECK(media_revision >= 1);

ALTER TABLE media_items
    ADD COLUMN probe_revision INTEGER NOT NULL DEFAULT 0 CHECK(probe_revision >= 0);

ALTER TABLE media_items
    ADD COLUMN probed_media_revision INTEGER;

ALTER TABLE media_items
    ADD COLUMN video_stream_index INTEGER;

ALTER TABLE media_item_subtitle_tracks
    ADD COLUMN probe_revision INTEGER NOT NULL DEFAULT 0;

-- A legacy `probed_content_id` was written without a media revision, so it
-- cannot certify a revision-1 snapshot. Clear it; `probed_media_revision` is
-- NULL by the new column's own default. Facts and statuses stay put and read
-- diagnostic-only until a publication lands.
UPDATE media_items SET probed_content_id = NULL;

CREATE TABLE media_item_audio_tracks (
    media_item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    probe_revision INTEGER NOT NULL,
    stream_index INTEGER NOT NULL,
    codec TEXT NOT NULL,
    language TEXT,
    channels INTEGER,
    channel_layout TEXT,
    title TEXT,
    is_default INTEGER NOT NULL CHECK(is_default IN (0,1)),
    PRIMARY KEY (media_item_id, stream_index)
);

CREATE INDEX idx_media_item_audio_tracks_item ON media_item_audio_tracks(media_item_id);
