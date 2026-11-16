PRAGMA application_id = 0x434f434f;

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    public_id TEXT NOT NULL UNIQUE CHECK (
        length(CAST(public_id AS BLOB)) = 37
        AND length(public_id) = 37
        AND substr(public_id, 1, 5) = 'sess_'
        AND substr(public_id, 6) NOT GLOB '*[^0-9a-f]*'
    ),
    working_directory TEXT NOT NULL CHECK (
        length(CAST(working_directory AS BLOB)) BETWEEN 1 AND 16384
    ),
    title TEXT CHECK (
        title IS NULL OR length(CAST(title AS BLOB)) BETWEEN 1 AND 512
    ),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0),
    last_opened_at_ms INTEGER NOT NULL CHECK (last_opened_at_ms >= 0),
    runtime_epoch INTEGER NOT NULL CHECK (runtime_epoch > 0),
    settings_version INTEGER NOT NULL CHECK (settings_version > 0),
    settings_json TEXT NOT NULL CHECK (
        json_valid(settings_json) AND json_type(settings_json) = 'object'
    )
) STRICT;

CREATE INDEX sessions_directory_activity
    ON sessions (working_directory, updated_at_ms DESC, id DESC);
CREATE INDEX sessions_activity
    ON sessions (updated_at_ms DESC, id DESC);

CREATE TABLE agents (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    name TEXT NOT NULL CHECK (
        length(CAST(name AS BLOB)) BETWEEN 1 AND 64
        AND instr(name, char(0)) = 0
        AND name NOT GLOB '*[^A-Za-z0-9_-]*'
    ),
    parent_agent_id INTEGER,
    format_version INTEGER NOT NULL CHECK (format_version > 0),
    assignment_json TEXT CHECK (
        assignment_json IS NULL OR (
            json_valid(assignment_json) AND json_type(assignment_json) = 'object'
        )
    ),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('participating', 'retired')),
    outcome_json TEXT CHECK (
        outcome_json IS NULL OR (
            json_valid(outcome_json) AND json_type(outcome_json) = 'object'
        )
    ),
    UNIQUE (session_id, id),
    FOREIGN KEY (session_id, parent_agent_id) REFERENCES agents(session_id, id),
    CHECK (parent_agent_id IS NULL OR parent_agent_id < id),
    CHECK (
        (parent_agent_id IS NULL AND assignment_json IS NULL
            AND outcome_json IS NULL AND lifecycle = 'participating')
        OR (parent_agent_id IS NOT NULL AND assignment_json IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX agents_one_root
    ON agents (session_id) WHERE parent_agent_id IS NULL;
CREATE INDEX agents_parent ON agents (session_id, parent_agent_id);

CREATE TABLE records (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    session_id INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    invocation INTEGER NOT NULL CHECK (invocation > 0),
    runtime_epoch INTEGER NOT NULL CHECK (runtime_epoch > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    payload_json TEXT NOT NULL CHECK (
        json_valid(payload_json) AND json_type(payload_json) = 'object'
    ),
    UNIQUE (session_id, agent_id, sequence),
    FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, id)
) STRICT;

CREATE TABLE context_records (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    session_id INTEGER NOT NULL,
    key TEXT NOT NULL CHECK (length(CAST(key AS BLOB)) BETWEEN 1 AND 128),
    owner_agent_id INTEGER NOT NULL,
    content_version INTEGER NOT NULL CHECK (content_version > 0),
    content_json TEXT NOT NULL CHECK (
        json_valid(content_json) AND json_type(content_json) = 'object'
    ),
    UNIQUE (session_id, key),
    FOREIGN KEY (session_id, owner_agent_id) REFERENCES agents(session_id, id)
) STRICT;

CREATE TABLE mutation_receipts (
    operation_record_id INTEGER PRIMARY KEY REFERENCES records(id),
    outcome_version INTEGER NOT NULL CHECK (outcome_version > 0),
    outcome_json TEXT NOT NULL CHECK (
        json_valid(outcome_json) AND json_type(outcome_json) = 'object'
    )
) STRICT;

CREATE TABLE instruction_state (
    agent_id INTEGER PRIMARY KEY REFERENCES agents(id),
    format_version INTEGER NOT NULL CHECK (format_version > 0),
    state_json TEXT NOT NULL CHECK (
        json_valid(state_json) AND json_type(state_json) = 'object'
    )
) STRICT;
