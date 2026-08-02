-- ADR-0026 §3 negative-result cache; §4 entity-keyed raw provider payloads.

CREATE TABLE metadata_negative_cache (
    provider TEXT NOT NULL,
    kind TEXT NOT NULL,
    query_key TEXT NOT NULL,
    reason TEXT NOT NULL,
    confidence REAL,
    attempt_count INTEGER NOT NULL DEFAULT 1,
    attempted_at TEXT NOT NULL,
    next_retry_at TEXT NOT NULL,
    PRIMARY KEY (provider, kind, query_key)
);

CREATE TABLE metadata_raw_payloads (
    provider TEXT NOT NULL,
    entity_kind TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    fetched_at TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (provider, entity_kind, provider_id)
);
