-- Debugging SSoT: one store, every signal, correlated by trace id.
--
-- Design rules:
--  * Bodies are never inlined. They live in the blob store, addressed by
--    BLAKE3 hash, so identical payloads across runs and browsers are stored
--    once and parity comparisons are exact.
--  * Every observation carries a trace_id. A "trace" is one logical
--    measurement: one page load, in one browser, from one host.
--  * Clock skew is real across vantage hosts, so every table keeps both a
--    monotonic offset (us_offset, microseconds from trace start) and the
--    wall-clock value it came from.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- A single logical measurement.
CREATE TABLE IF NOT EXISTS trace (
    id              TEXT PRIMARY KEY,          -- uuid
    started_at      TEXT NOT NULL,             -- RFC3339 UTC
    finished_at     TEXT,
    target_url      TEXT NOT NULL,
    -- provenance: which browser, which engine, which machine
    browser         TEXT,                      -- "librewolf", "firefox", "chrome", "curl"
    browser_version TEXT,
    engine          TEXT,                      -- "gecko" | "blink" | "webkit" | "goanna" | null
    host            TEXT NOT NULL,             -- vantage point name; "local" by default
    -- how the observation was made
    via             TEXT NOT NULL,             -- "proxy" | "marionette" | "rdp" | "bidi" | "curl" | "extension"
    headless        INTEGER NOT NULL DEFAULT 0,
    cache_mode      TEXT NOT NULL DEFAULT 'live', -- "live" | "cached" | "bypass"
    label           TEXT,                      -- free-text run label
    notes           TEXT
);
CREATE INDEX IF NOT EXISTS trace_target  ON trace(target_url);
CREATE INDEX IF NOT EXISTS trace_host    ON trace(host, started_at);

-- Content-addressed payload store. Bodies on disk, metadata here.
CREATE TABLE IF NOT EXISTS blob (
    hash        TEXT PRIMARY KEY,              -- blake3 hex
    size        INTEGER NOT NULL,              -- decoded size in bytes
    media_type  TEXT,
    path        TEXT NOT NULL,                 -- relative path in the blob dir
    created_at  TEXT NOT NULL
);

-- One HTTP exchange. Populated by the proxy, and by transports that can see
-- network events.
CREATE TABLE IF NOT EXISTS exchange (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    trace_id        TEXT NOT NULL REFERENCES trace(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,          -- order within the trace
    us_offset       INTEGER NOT NULL,          -- microseconds from trace start
    method          TEXT NOT NULL,
    url             TEXT NOT NULL,
    status          INTEGER,
    -- the question "did compression actually apply" must be answerable
    -- without re-reading headers, so both sizes are first-class
    wire_bytes      INTEGER,                   -- encoded, as it crossed the socket
    decoded_bytes   INTEGER,
    content_encoding TEXT,
    -- timing, microseconds, null when not observable
    us_dns          INTEGER,
    us_connect      INTEGER,
    us_tls          INTEGER,
    us_ttfb         INTEGER,
    us_total        INTEGER,
    from_cache      INTEGER NOT NULL DEFAULT 0,
    req_headers     TEXT,                      -- json object
    res_headers     TEXT,                      -- json object
    body_hash       TEXT REFERENCES blob(hash),
    error           TEXT
);
CREATE INDEX IF NOT EXISTS exchange_trace ON exchange(trace_id, seq);
CREATE INDEX IF NOT EXISTS exchange_url   ON exchange(url);

-- Page-level timing from the Navigation Timing API.
CREATE TABLE IF NOT EXISTS navigation_timing (
    trace_id            TEXT PRIMARY KEY REFERENCES trace(id) ON DELETE CASCADE,
    us_ttfb             INTEGER,
    us_response_end     INTEGER,
    us_dom_interactive  INTEGER,
    us_dcl              INTEGER,
    us_load             INTEGER,
    transfer_size       INTEGER,
    encoded_body_size   INTEGER,
    decoded_body_size   INTEGER,
    dom_nodes           INTEGER
);

-- Per-resource timing from the Resource Timing API.
CREATE TABLE IF NOT EXISTS resource_timing (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    trace_id    TEXT NOT NULL REFERENCES trace(id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    initiator   TEXT,
    us_start    INTEGER,
    us_ttfb     INTEGER,
    us_end      INTEGER,
    transfer_size     INTEGER,
    encoded_body_size INTEGER,
    decoded_body_size INTEGER
);
CREATE INDEX IF NOT EXISTS resource_trace ON resource_timing(trace_id);
CREATE INDEX IF NOT EXISTS resource_url   ON resource_timing(url);

-- MOZ_LOG lines and console messages, parsed. The join against exchange and
-- resource_timing by url is the reason this table exists at all.
CREATE TABLE IF NOT EXISTS log_line (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    trace_id    TEXT NOT NULL REFERENCES trace(id) ON DELETE CASCADE,
    source      TEXT NOT NULL,                 -- "moz_log" | "console" | "stderr"
    module      TEXT,                          -- e.g. "nsHttp"
    level       INTEGER,
    us_offset   INTEGER,
    url         TEXT,                           -- extracted when the line names one
    message     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS log_trace  ON log_line(trace_id, us_offset);
CREATE INDEX IF NOT EXISTS log_url    ON log_line(url);

-- Rendered screenshots, for parity work.
CREATE TABLE IF NOT EXISTS render (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    trace_id    TEXT NOT NULL REFERENCES trace(id) ON DELETE CASCADE,
    width       INTEGER NOT NULL,
    height      INTEGER NOT NULL,
    dpr         REAL NOT NULL DEFAULT 1.0,
    full_page   INTEGER NOT NULL DEFAULT 0,
    blob_hash   TEXT NOT NULL REFERENCES blob(hash),
    captured_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS render_trace ON render(trace_id);

-- Pairwise render comparison. Scores are evidence for a human, not a verdict:
-- engines differ legitimately in font rasterisation and widget metrics.
CREATE TABLE IF NOT EXISTS parity (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    render_a      INTEGER NOT NULL REFERENCES render(id) ON DELETE CASCADE,
    render_b      INTEGER NOT NULL REFERENCES render(id) ON DELETE CASCADE,
    ssim          REAL,
    phash_hamming INTEGER,
    backend       TEXT NOT NULL,               -- "gpu" | "cpu"
    computed_at   TEXT NOT NULL,
    UNIQUE(render_a, render_b, backend)
);

-- Convenience view: the compression question, answered per exchange.
CREATE VIEW IF NOT EXISTS v_compression AS
SELECT
    e.trace_id,
    e.url,
    e.content_encoding,
    e.wire_bytes,
    e.decoded_bytes,
    CASE
        WHEN e.wire_bytes IS NULL OR e.decoded_bytes IS NULL THEN NULL
        WHEN e.wire_bytes < e.decoded_bytes THEN 1
        ELSE 0
    END AS compressed,
    CASE
        WHEN e.decoded_bytes > 0 AND e.wire_bytes IS NOT NULL
        THEN 1.0 - (CAST(e.wire_bytes AS REAL) / e.decoded_bytes)
        ELSE NULL
    END AS saved_fraction
FROM exchange e;

-- Proxy cache index. Bodies live in the shared blob store, so a payload that
-- is both cached and captured is stored once.
--
-- The key folds in the request headers named by the response's Vary, because
-- serving a gzip body to a client that did not ask for gzip is a real bug and
-- the measurement story here is specifically about content-encoding.
CREATE TABLE IF NOT EXISTS cache_entry (
    key             TEXT PRIMARY KEY,          -- blake3(method, url, vary-selected request headers)
    method          TEXT NOT NULL,
    url             TEXT NOT NULL,
    status          INTEGER NOT NULL,
    res_headers     TEXT NOT NULL,             -- json object
    body_hash       TEXT NOT NULL REFERENCES blob(hash),
    wire_bytes      INTEGER NOT NULL,
    decoded_bytes   INTEGER,
    content_encoding TEXT,
    vary            TEXT,                      -- raw Vary header, for auditing the key
    etag            TEXT,
    last_modified   TEXT,
    stored_at       TEXT NOT NULL,
    expires_at      TEXT,                      -- null = no freshness info; revalidate
    hit_count       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS cache_url ON cache_entry(url);
