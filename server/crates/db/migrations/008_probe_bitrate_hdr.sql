-- ADR-0022: probe fields for bitrate / HDR ceilings (Rule 4.9).
-- Writers fill these on ffprobe; NULL until the next probe after upgrade.
ALTER TABLE media_items ADD COLUMN video_bitrate_bps INTEGER;
-- Source HDR: none | hdr10 | dolby_vision (validated in Rust).
ALTER TABLE media_items ADD COLUMN hdr TEXT;
