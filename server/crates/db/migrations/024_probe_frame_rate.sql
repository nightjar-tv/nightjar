-- ADR-0052: the encoder's IDR interval is derived from the source frame rate,
-- so the frame rate has to be a stored probe field like width or bitrate.
--
-- Stored as a rational, not a float. 24000/1001 is the common case and
-- 23.976 is not equal to it; the error accumulates against a title-absolute
-- 2 s grid over an hour of media. ffprobe reports `avg_frame_rate` in exactly
-- this form, so keep it that way rather than rounding at the boundary.
--
-- NULL until probed. A session that needs the value and finds it missing
-- resolves it on demand and writes back (Rule 4.13).
ALTER TABLE media_items ADD COLUMN video_frame_rate_num INTEGER;
ALTER TABLE media_items ADD COLUMN video_frame_rate_den INTEGER;
