CREATE TABLE records_next (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    session_id INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    invocation INTEGER NOT NULL CHECK (invocation > 0),
    runtime_epoch INTEGER NOT NULL CHECK (runtime_epoch > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    payload_version INTEGER NOT NULL CHECK (payload_version > 0),
    input_images INTEGER GENERATED ALWAYS AS (
        CASE WHEN payload_json IS NOT NULL
            THEN coalesce(json_array_length(payload_json, '$.InputAccepted.input.images'), 0)
            ELSE archive_input_images END
    ) STORED,
    is_input INTEGER GENERATED ALWAYS AS (
        CASE WHEN payload_json IS NULL THEN archive_kind = 'InputAccepted'
            ELSE json_type(payload_json, '$.InputAccepted') IS NOT NULL END
    ) STORED,
    payload_json TEXT CHECK (
        payload_json IS NULL OR (json_valid(payload_json) AND json_type(payload_json) = 'object')
    ),
    archive_zlib BLOB,
    archive_decoded_bytes INTEGER,
    archive_sha256 BLOB,
    archive_kind TEXT,
    archive_input_images INTEGER,
    CHECK (
        (payload_json IS NOT NULL AND archive_zlib IS NULL AND archive_decoded_bytes IS NULL
            AND archive_sha256 IS NULL AND archive_kind IS NULL AND archive_input_images IS NULL)
        OR (payload_json IS NULL AND archive_zlib IS NOT NULL
            AND archive_input_images IS NOT NULL AND archive_input_images >= 0
            AND archive_decoded_bytes IS NOT NULL AND archive_sha256 IS NOT NULL AND archive_kind IS NOT NULL
            AND length(archive_zlib) BETWEEN 1 AND 8388608
            AND archive_decoded_bytes BETWEEN 1 AND 8388608
            AND length(archive_sha256) = 32
            AND archive_kind IN ('InputAccepted', 'ModelCompleted', 'CellStarted', 'CellCompleted',
                'OperationStarted', 'OperationFinished', 'InvocationFinished', 'TurnCancelled'))
    ),
    UNIQUE (session_id, agent_id, sequence),
    FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, id)
) STRICT;

INSERT INTO records_next (id, session_id, agent_id, sequence, invocation, runtime_epoch,
    created_at_ms, payload_version, payload_json)
SELECT id, session_id, agent_id, sequence, invocation, runtime_epoch,
    created_at_ms, payload_version, payload_json FROM records;

CREATE TABLE mutation_receipts_previous AS SELECT * FROM mutation_receipts;
DROP TABLE mutation_receipts;
DROP TABLE records;
ALTER TABLE records_next RENAME TO records;
CREATE INDEX records_inputs ON records (session_id, agent_id, invocation, sequence) WHERE is_input = 1;
CREATE INDEX records_image_inputs ON records (session_id, agent_id, sequence) WHERE input_images > 0;
CREATE INDEX records_archive_candidates ON records (session_id, agent_id, sequence)
    WHERE payload_json IS NOT NULL AND octet_length(payload_json) BETWEEN 103 AND 8388608;

CREATE TABLE mutation_receipts (
    operation_record_id INTEGER PRIMARY KEY REFERENCES records(id),
    outcome_version INTEGER NOT NULL CHECK (outcome_version > 0),
    outcome_json TEXT NOT NULL CHECK (
        json_valid(outcome_json) AND json_type(outcome_json) = 'object'
    )
) STRICT;
INSERT INTO mutation_receipts SELECT * FROM mutation_receipts_previous;
DROP TABLE mutation_receipts_previous;

CREATE TABLE checkpoints (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    session_id INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    after_record_id INTEGER NOT NULL REFERENCES records(id),
    through_record_id INTEGER NOT NULL REFERENCES records(id),
    format_version INTEGER NOT NULL CHECK (format_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    payload_json TEXT NOT NULL CHECK (
        json_valid(payload_json) AND json_type(payload_json) = 'object'
    ),
    UNIQUE (session_id, agent_id, sequence),
    FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, id)
) STRICT;

CREATE TRIGGER records_control_identity BEFORE INSERT ON records
WHEN EXISTS (SELECT 1 FROM checkpoints WHERE id = NEW.id)
BEGIN
    SELECT RAISE(ABORT, 'record identity already belongs to a checkpoint');
END;
CREATE TRIGGER checkpoints_record_identity BEFORE INSERT ON checkpoints
WHEN EXISTS (SELECT 1 FROM records WHERE id = NEW.id)
BEGIN
    SELECT RAISE(ABORT, 'checkpoint identity already belongs to an original');
END;
