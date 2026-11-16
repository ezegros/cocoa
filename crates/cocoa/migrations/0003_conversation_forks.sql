CREATE TABLE conversation_forks (
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id),
    source_session_id INTEGER NOT NULL CHECK (source_session_id > 0),
    before_record_id INTEGER NOT NULL CHECK (before_record_id > 0),
    CHECK (session_id != source_session_id)
) STRICT;
