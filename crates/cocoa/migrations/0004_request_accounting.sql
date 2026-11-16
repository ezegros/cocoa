ALTER TABLE sessions ADD COLUMN accounting_version INTEGER NOT NULL DEFAULT 0
    CHECK (accounting_version IN (0, 1));

CREATE TABLE model_requests (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    session_id INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    invocation INTEGER NOT NULL CHECK (invocation > 0),
    runtime_epoch INTEGER NOT NULL CHECK (runtime_epoch > 0),
    started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= started_at_ms),
    purpose_json TEXT NOT NULL CHECK (json_valid(purpose_json)),
    settings_json TEXT NOT NULL CHECK (json_valid(settings_json)),
    update_json TEXT NOT NULL CHECK (json_valid(update_json)),
    CHECK (octet_length(settings_json) <= 4096),
    CHECK (octet_length(update_json) <= 16384),
    FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, id)
) STRICT;
CREATE INDEX model_requests_session ON model_requests(session_id, id);
