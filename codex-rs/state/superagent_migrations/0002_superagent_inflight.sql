-- What the executor sent to a worker's thread and has not seen the end of (design §8.1, the
-- Codex pilot's rule): written before the delivery, deleted in the transaction that records the
-- turn's end. A row that survives a restart is never replayed; the ticket is reported blocked.
CREATE TABLE inflight (
    thread_id TEXT PRIMARY KEY NOT NULL,
    assignment_id TEXT NOT NULL,
    input_ids TEXT NOT NULL,
    turn_id TEXT,
    started_at TEXT NOT NULL
);
