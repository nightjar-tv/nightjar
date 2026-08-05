-- ADR-0026 §8.1: one-shot repair for dogfood-era rows (autopsy D6). Runs
-- once as migration 015; nothing inside drain_pending repairs historical
-- rows (decision 6).
--
-- (a) E1: an episode holding a `tmdb:episode:{id}` link plus the matching
-- canonical episode row is fully identified; `unmatched` there is a status
-- artifact of the two-tier rollout, not a resolve outcome.
-- (b) E2: a `tmdb:show:{id}` link whose id is a canonical *episode* id is a
-- mis-prefix from the pre-`apply_search_hit` writer bug. Delete the link and
-- send the item back to `pending` so the search tier re-resolves it.

UPDATE media_items
SET metadata_status = 'ready'
WHERE id IN (
    SELECT DISTINCT m.id
    FROM media_items m
    JOIN media_item_links l ON l.media_item_id = m.id
    JOIN metadata_canonical c ON c.provider = 'tmdb'
        AND c.entity_kind = 'episode'
        AND l.item_key = 'tmdb:episode:' || c.provider_id
    WHERE m.metadata_status = 'unmatched'
      AND m.kind = 'episode'
      AND l.item_key LIKE 'tmdb:episode:%'
  );

-- (b) re-queue mis-prefixed items. E1 priority: an item that also holds a
-- qualifying episode link stays `ready` and only loses the garbage link
-- below; it is not sent back to the search tier.
UPDATE media_items
SET metadata_status = 'pending'
WHERE kind = 'episode'
  AND id IN (
    SELECT DISTINCT m.id
    FROM media_items m
    JOIN media_item_links l ON l.media_item_id = m.id
    JOIN metadata_canonical c ON c.provider = 'tmdb'
        AND c.entity_kind = 'episode'
        AND l.item_key = 'tmdb:show:' || c.provider_id
    WHERE m.kind = 'episode'
      AND NOT EXISTS (
        SELECT 1
        FROM media_item_links el
        JOIN metadata_canonical ec ON ec.provider = 'tmdb'
            AND ec.entity_kind = 'episode'
            AND el.item_key = 'tmdb:episode:' || ec.provider_id
        WHERE el.media_item_id = m.id
      )
  );

-- (b) delete the mis-prefixed `tmdb:show:{episode_id}` links themselves.
DELETE FROM media_item_links
WHERE item_key LIKE 'tmdb:show:%'
  AND media_item_id IN (SELECT id FROM media_items WHERE kind = 'episode')
  AND EXISTS (
    SELECT 1 FROM metadata_canonical c
    WHERE c.provider = 'tmdb'
      AND c.entity_kind = 'episode'
      AND 'tmdb:show:' || c.provider_id = media_item_links.item_key
  );
