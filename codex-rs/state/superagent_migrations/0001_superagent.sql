-- Superagent coordination engine (codex-rs/ext/superagent). The engine is the only writer.
-- Aggregates are stored as JSON, as the prototype keeps them; generated columns index the working
-- sets so a pass reads only active rows.

CREATE TABLE kv (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

CREATE TABLE trigger (
    id TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    at TEXT NOT NULL
);
CREATE INDEX trigger_at ON trigger(at);

CREATE TABLE disposition (
    id TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

-- Which Threads each Trigger is on, in order of time and then id.
CREATE TABLE trigger_thread (
    thread TEXT NOT NULL,
    id TEXT NOT NULL,
    at TEXT NOT NULL,
    human INTEGER NOT NULL,
    PRIMARY KEY (thread, id)
);
CREATE INDEX trigger_thread_at ON trigger_thread(thread, at);

CREATE TABLE thread (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    active INTEGER GENERATED ALWAYS AS (
        CASE WHEN json_array_length(value, '$.pending') > 0 OR value -> '$.evaluating' IS NOT NULL THEN 1 END
    ) VIRTUAL,
    subscribed TEXT GENERATED ALWAYS AS (
        CASE WHEN json_array_length(value, '$.subscriptions') > 0 THEN value ->> '$.ref.source' END
    ) VIRTUAL
);
CREATE INDEX thread_active ON thread(active) WHERE active IS NOT NULL;
CREATE INDEX thread_subscribed ON thread(subscribed) WHERE subscribed IS NOT NULL;

CREATE TABLE ticket (
    id TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    open INTEGER GENERATED ALWAYS AS (
        CASE WHEN value -> '$.ended' IS NULL THEN 1 END
    ) VIRTUAL,
    -- Only the latest assignment can be pending or running: the engine ends one before adding the next.
    active INTEGER GENERATED ALWAYS AS (
        CASE WHEN json_array_length(value, '$.undelivered') > 0
            OR value ->> '$.assignments[#-1].state' IN ('pending', 'running') THEN 1 END
    ) VIRTUAL
);
CREATE INDEX ticket_open ON ticket(open) WHERE open IS NOT NULL;
CREATE INDEX ticket_active ON ticket(active) WHERE active IS NOT NULL;

CREATE TABLE decision (
    id TEXT PRIMARY KEY NOT NULL,
    thread TEXT NOT NULL,
    value TEXT NOT NULL
);
CREATE INDEX decision_thread ON decision(thread);

-- The receipt of each message for a conversation; an id is never queued twice.
CREATE TABLE outbox (
    id TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    dropped TEXT GENERATED ALWAYS AS (value ->> '$.dropped.at') VIRTUAL,
    pending TEXT GENERATED ALWAYS AS (
        CASE WHEN value ->> '$.status' = 'pending' AND value ->> '$.ticketId' IS NOT NULL
            THEN (value ->> '$.ticketId') || ' ' || (value ->> '$.channel.thread') END
    ) VIRTUAL
);
CREATE INDEX outbox_dropped ON outbox(dropped) WHERE dropped IS NOT NULL;
CREATE INDEX outbox_pending ON outbox(pending) WHERE pending IS NOT NULL;

-- The effect queue: what a committed command still has to do, per kind and key.
CREATE TABLE effect (
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    seq INTEGER NOT NULL,
    generation INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '',
    due INTEGER NOT NULL,
    since TEXT NOT NULL,
    PRIMARY KEY (kind, key)
);
CREATE INDEX effect_due ON effect(kind, due, seq);
