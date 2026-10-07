-- powerqueue schema. Applied idempotently; versioned via user_version.
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    key             TEXT NOT NULL UNIQUE,
    title           TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    source          TEXT NOT NULL,             -- JSON TaskSource
    source_kind     TEXT NOT NULL,             -- linear | manual
    linear_issue_id TEXT,
    state           TEXT NOT NULL,
    criticality     TEXT NOT NULL,
    score           REAL NOT NULL DEFAULT 0,
    labels          TEXT NOT NULL DEFAULT '[]',-- JSON array
    linear_priority INTEGER,
    estimate        REAL,
    project         TEXT,
    cycle           TEXT,                      -- active | next | past | future (v3)
    cycle_number    INTEGER,                   -- Linear Cycle.number (v3)
    blocked_by      TEXT NOT NULL DEFAULT '[]', -- JSON [LinkedIssue]: Linear `blocked by` relations (v4)
    children        TEXT NOT NULL DEFAULT '[]', -- JSON [LinkedIssue]: sub-issues; non-empty = container (v4)
    parent          TEXT,                      -- identifier of the parent issue (v4)
    model_override  TEXT,
    model           TEXT,
    worktree_path   TEXT,
    branch          TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    max_attempts    INTEGER,
    not_before      TEXT,
    last_error      TEXT,
    summary         TEXT,
    score_reasons   TEXT NOT NULL DEFAULT '[]',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    started_at      TEXT,
    completed_at    TEXT,
    pr_url          TEXT,                      -- pull request handed off with `task complete --pr` (v5)
    review          TEXT                       -- JSON ReviewWatch: PR watcher state (v5)
);
CREATE INDEX IF NOT EXISTS tasks_state_idx ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_linear_idx ON tasks(linear_issue_id);

CREATE TABLE IF NOT EXISTS sessions (
    id               TEXT PRIMARY KEY,
    task_id          TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    attempt          INTEGER NOT NULL,
    model            TEXT NOT NULL,
    state            TEXT NOT NULL,
    tmux_session     TEXT NOT NULL,
    tmux_window      TEXT NOT NULL,
    pane_id          TEXT,
    pid              INTEGER,
    transcript_path  TEXT,
    exit_code        INTEGER,
    started_at       TEXT NOT NULL,
    ended_at         TEXT,
    last_activity_at TEXT NOT NULL,
    error            TEXT,
    agent_session_id TEXT,                     -- the CLI's own id (Codex thread, agy conversation); NULL for Claude
    waiting_since    TEXT,                     -- start of the current wait on a human (v6)
    waited_secs      INTEGER NOT NULL DEFAULT 0 -- earlier waits on a human, excluded from max_session_secs (v6)
);
CREATE INDEX IF NOT EXISTS sessions_task_idx ON sessions(task_id);
CREATE INDEX IF NOT EXISTS sessions_state_idx ON sessions(state);

CREATE TABLE IF NOT EXISTS usage (
    message_id                  TEXT PRIMARY KEY,
    session_id                  TEXT NOT NULL,
    task_id                     TEXT NOT NULL,
    model_id                    TEXT NOT NULL,
    tier                        TEXT NOT NULL,
    input_tokens                INTEGER NOT NULL,
    output_tokens               INTEGER NOT NULL,
    cache_creation_input_tokens INTEGER NOT NULL,
    cache_read_input_tokens     INTEGER NOT NULL,
    timestamp                   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_ts_idx ON usage(timestamp);
CREATE INDEX IF NOT EXISTS usage_session_idx ON usage(session_id);
CREATE INDEX IF NOT EXISTS usage_task_idx ON usage(task_id);

CREATE TABLE IF NOT EXISTS resource_samples (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    TEXT NOT NULL,
    task_id       TEXT NOT NULL,
    timestamp     TEXT NOT NULL,
    cpu_percent   REAL NOT NULL,
    rss_bytes     INTEGER NOT NULL,
    process_count INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS resource_session_idx ON resource_samples(session_id);

CREATE TABLE IF NOT EXISTS events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT,
    session_id TEXT,
    timestamp  TEXT NOT NULL,
    level      TEXT NOT NULL,
    kind       TEXT NOT NULL,
    message    TEXT NOT NULL,
    data       TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS events_task_idx ON events(task_id, id);
CREATE INDEX IF NOT EXISTS events_kind_idx ON events(kind);

-- Commands from CLI/dashboard to the daemon.
CREATE TABLE IF NOT EXISTS commands (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,
    payload    TEXT NOT NULL,                 -- JSON DaemonCommand
    consumed_at TEXT
);

-- Hook payloads written by `powerqueue hook`, consumed by the daemon.
CREATE TABLE IF NOT EXISTS hook_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL,
    session_id  TEXT,
    event       TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    consumed_at TEXT
);
CREATE INDEX IF NOT EXISTS hook_events_pending_idx ON hook_events(consumed_at, id);

-- Free-form key/value: daemon heartbeat, budget calibration/observed usage per provider, estimator state.
CREATE TABLE IF NOT EXISTS kv (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Cache of Jev scores keyed by content hash.
CREATE TABLE IF NOT EXISTS jev_scores (
    task_id      TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    score        REAL NOT NULL,
    level        TEXT NOT NULL,
    confidence   REAL NOT NULL,
    raw          TEXT NOT NULL,
    scored_at    TEXT NOT NULL
);
