-- ADR-0037 items 2 and 8 (B2-6): the certification projection column and the
-- single-row server classification region.
--
-- `certifications_json` holds an object from an uppercase region code to a
-- non-empty raw board label, decided in ADR-0037 item 8. Null means no
-- projected certification for this entity. The column is added, never
-- rewritten, so an existing row keeps whatever a previous projection wrote
-- until the back-fill reaches it.
--
-- `server_settings` is a single row (`id = 1`). `classification_region` is the
-- one board the server compares against (ADR-0037 item 2). It is NOT NULL:
-- there is no half-selected state, the setup boundary writes it once, and no
-- HTTP route changes it. The CHECK pins the uppercase non-empty region shape
-- the ladder and the projection both use, so a lowercase row cannot exist and
-- then fail every lookup.

ALTER TABLE metadata_canonical ADD COLUMN certifications_json TEXT;

CREATE TABLE server_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    classification_region TEXT NOT NULL
        CHECK (
            length(classification_region) > 0
            AND classification_region = UPPER(classification_region)
        )
);
