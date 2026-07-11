-- Minimal `session` DDL: ONLY the columns the opencode reader selects, with their
-- real NULL/NOT NULL constraints (captured from ~/.local/share/opencode/opencode.db,
-- opencode 1.17.17). The live table has ~27 drizzle-managed columns; the reader must
-- depend only on these seven, which this fixture enforces by construction.
CREATE TABLE session (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    directory TEXT NOT NULL,
    title TEXT NOT NULL,
    time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL,
    time_archived INTEGER
);
