-- First-audio channel count so the decision engine can apply the client
-- channel ceiling without a live probe (ADR-0012).
ALTER TABLE media_items ADD COLUMN audio_channels INTEGER;
