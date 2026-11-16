//! Literal root-message search in a bounded read view, without a text projection.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use cocoa_core::{
    model::ModelAction,
    store::{RecordKind, StoreError, encoded_size},
};
use serde::Serialize;
use sqlx::{Connection, FromRow, QueryBuilder, Sqlite, SqliteConnection};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    PersistenceError, RECORD_VERSION, SessionCursor, SessionDatabase, SessionSummary, archives,
    reads,
};

/// Matching text excludes provider continuation, reasoning, tools and children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSource {
    Title,
    User,
    Assistant,
}

/// One result per session, with a title-preferred deterministic text snippet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchMatch {
    #[serde(flatten)]
    pub session: SessionSummary,
    pub snippet: String,
    pub source: SearchSource,
}

/// Explicitly capped results in descending activity/ID order, without ranking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionSearch {
    pub sessions: Vec<SearchMatch>,
    pub limit: u32,
}

impl SessionDatabase {
    /// Match the trimmed complete query as one Unicode-lowercased substring.
    /// Punctuation, operators, accents, `%` and `_` remain literal. Empty queries
    /// list sessions. At most 50 results and 256 KiB are returned, never a silently
    /// partial scan after cancellation or the cooperative two-second budget.
    ///
    /// # Errors
    /// Reject queries over 512 UTF-8 bytes, invalid limits, malformed payloads,
    /// exhausted budgets/admission and database errors.
    pub async fn search_sessions(
        &self,
        directory: Option<&Path>,
        query: &str,
        limit: u32,
    ) -> Result<SessionSearch, PersistenceError> {
        if !(1..=50).contains(&limit) {
            return Err(PersistenceError::InvalidSessionLimit);
        }
        if query.len() > 512 {
            return Err(PersistenceError::InvalidData(
                "search query exceeds 512 bytes",
            ));
        }
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            let page = self.list_sessions(directory, limit, None).await?;
            let result = SessionSearch {
                sessions: page
                    .sessions
                    .into_iter()
                    .map(|session| SearchMatch {
                        snippet: session
                            .title
                            .clone()
                            .unwrap_or_else(|| "Untitled session".into()),
                        session,
                        source: SearchSource::Title,
                    })
                    .collect(),
                limit,
            };
            if encoded_size(&result)? > super::SESSION_RESPONSE_BYTES {
                return Err(StoreError::Capacity.into());
            }
            return Ok(result);
        }
        let directory = directory
            .map(super::session_directory)
            .transpose()?
            .map(str::to_owned);
        let request = self.inner.request()?;
        let database = Arc::clone(&self.inner);
        let control = SearchControl {
            deadline: Instant::now() + Duration::from_secs(2),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let cancellation = CancelOnDrop(Arc::clone(&control.cancelled));
        let result = tokio::spawn(async move {
            let _request = request;
            let mut reader = tokio::time::timeout(super::WAIT, database.reader.lock())
                .await
                .map_err(|_| PersistenceError::Busy)?;
            reader.settle().await?;
            let active = reader.search.clone();
            {
                let mut active = active
                    .lock()
                    .map_err(|_| PersistenceError::InvalidData("search control poisoned"))?;
                *active = Some(control.clone());
            }
            let reset = ResetSearch(active);
            let result = async {
                let mut transaction = reader.connection()?.begin().await?;
                let result = scan(
                    &mut transaction,
                    &database,
                    directory.as_deref(),
                    query,
                    limit,
                    &control,
                )
                .await;
                drop(reset);
                match result {
                    Ok(result) => {
                        transaction.commit().await?;
                        Ok(result)
                    }
                    Err(error) => {
                        transaction.rollback().await?;
                        Err(error)
                    }
                }
            }
            .await;
            if control.stopped() {
                return Err(PersistenceError::SearchBudget);
            }
            result
        })
        .await?;
        drop(cancellation);
        result
    }
}

#[derive(Clone)]
pub(super) struct SearchControl {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl SearchControl {
    pub(super) fn stopped(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }
    fn check(&self) -> Result<(), PersistenceError> {
        if self.stopped() {
            Err(PersistenceError::SearchBudget)
        } else {
            Ok(())
        }
    }
}

pub(super) type ActiveSearch = Arc<Mutex<Option<SearchControl>>>;

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

struct ResetSearch(ActiveSearch);
impl Drop for ResetSearch {
    fn drop(&mut self) {
        match self.0.lock() {
            Ok(mut active) => *active = None,
            Err(error) => tracing::error!(%error, "SQLite search control poisoned during reset"),
        }
    }
}

async fn scan(
    connection: &mut SqliteConnection,
    database: &super::Database,
    directory: Option<&str>,
    query: String,
    limit: u32,
    control: &SearchControl,
) -> Result<SessionSearch, PersistenceError> {
    let mut result = SessionSearch {
        sessions: Vec::new(),
        limit,
    };
    let mut before = None;
    let mut candidates = 0;
    loop {
        control.check()?;
        let rows = reads::sessions(connection, directory, 16, before).await?;
        let done = rows.len() <= 16;
        let sessions: Vec<_> = rows
            .into_iter()
            .take(16)
            .map(SessionSummary::try_from)
            .collect::<Result<_, _>>()?;
        if sessions.is_empty() {
            break;
        }
        candidates += sessions.len();
        if candidates > 10000 {
            return Err(StoreError::Capacity.into());
        }
        before = sessions
            .last()
            .map(|session| SessionCursor::new(session.updated_at_ms, session.id))
            .transpose()?;
        let mut matches = BTreeMap::new();
        let mut remaining = Vec::new();
        for session in &sessions {
            if let Some(title) = &session.title
                && let Some(snippet) = snippet(title, &query, control)?
            {
                matches.insert(i64::from(session.id), (snippet, SearchSource::Title));
            } else {
                remaining.push(i64::from(session.id));
            }
        }
        if !remaining.is_empty() {
            roots(
                connection,
                database,
                &remaining,
                &query,
                control,
                &mut matches,
            )
            .await?;
        }
        for session in sessions {
            if let Some((snippet, source)) = matches.remove(&i64::from(session.id)) {
                result.sessions.push(SearchMatch {
                    session,
                    snippet,
                    source,
                });
                if encoded_size(&result)? > super::SESSION_RESPONSE_BYTES {
                    return Err(StoreError::Capacity.into());
                }
                if result.sessions.len() == limit as usize {
                    return Ok(result);
                }
            }
        }
        if done {
            break;
        }
    }
    Ok(result)
}

async fn roots(
    connection: &mut SqliteConnection,
    database: &super::Database,
    sessions: &[i64],
    query: &str,
    control: &SearchControl,
    matches: &mut BTreeMap<i64, (String, SearchSource)>,
) -> Result<(), PersistenceError> {
    let mut cursor = (0_i64, 0_i64, 0_i64);
    let mut completed = BTreeSet::new();
    loop {
        control.check()?;
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT r.id,r.session_id,r.agent_id,r.sequence,r.payload_version,coalesce(octet_length(r.payload_json),r.archive_decoded_bytes) AS bytes FROM records r WHERE (",
        );
        {
            let mut pairs = builder.separated(" OR ");
            for session in sessions {
                pairs
                    .push("(r.session_id=")
                    .push_bind_unseparated(*session)
                    .push_unseparated(
                        " AND r.agent_id=(SELECT a.id FROM agents a WHERE a.session_id=",
                    )
                    .push_bind_unseparated(*session)
                    .push_unseparated(" AND a.parent_agent_id IS NULL))");
            }
        }
        builder
            .push(") AND (r.session_id,r.agent_id,r.sequence)>(")
            .push_bind(cursor.0)
            .push(",")
            .push_bind(cursor.1)
            .push(",")
            .push_bind(cursor.2)
            .push(") ORDER BY r.session_id,r.agent_id,r.sequence LIMIT 64");
        let metadata: Vec<SearchRecord> =
            builder.build_query_as().fetch_all(&mut *connection).await?;
        if metadata.is_empty() {
            break;
        }
        let mut selected = Vec::new();
        let mut page_bytes = 0;
        for row in metadata {
            if row.payload_version != RECORD_VERSION {
                return Err(PersistenceError::UnsupportedPayload(row.payload_version));
            }
            if row.bytes <= 0 || row.bytes > super::PENDING_BYTES as i64 {
                return Err(StoreError::RecordLimit.into());
            }
            if !selected.is_empty() && page_bytes + row.bytes > 4 * 1024 * 1024 {
                break;
            }
            cursor = (row.session_id, row.agent_id, row.sequence);
            page_bytes += row.bytes;
            selected.push(row);
        }
        let _payload =
            database.payload(usize::try_from(page_bytes).map_err(|_| StoreError::Capacity)?)?;
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
        builder
            .push(archives::PAYLOAD_COLUMNS)
            .push(" FROM records NOT INDEXED WHERE id IN (");
        {
            let mut ids = builder.separated(",");
            for row in &selected {
                ids.push_bind(row.id);
            }
        }
        builder.push(") ORDER BY session_id,agent_id,sequence");
        let rows: Vec<archives::PayloadRow> =
            builder.build_query_as().fetch_all(&mut *connection).await?;
        if rows.len() != selected.len() {
            return Err(PersistenceError::InvalidData("search page changed"));
        }
        let query = query.to_owned();
        let control = control.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut found = Vec::new();
            for (payload, metadata) in rows.into_iter().zip(selected) {
                control.check()?;
                if payload.id != metadata.id {
                    return Err(PersistenceError::InvalidData("search record identity"));
                }
                let record = payload.decode(metadata.bytes)?;
                let candidate = match &record {
                    RecordKind::InputAccepted { input } => Some((input.text(), SearchSource::User)),
                    RecordKind::ModelCompleted { response } => match &response.action {
                        ModelAction::Final { answer } => {
                            Some((answer.as_str(), SearchSource::Assistant))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((text, source)) = candidate
                    && let Some(snippet) = snippet(text, &query, &control)?
                {
                    found.push((metadata.session_id, snippet, source));
                }
            }
            Ok::<_, PersistenceError>(found)
        })
        .await??;
        for (session, snippet, source) in found {
            if completed.insert(session) {
                matches.insert(session, (snippet, source));
            }
        }
    }
    Ok(())
}

fn snippet(
    text: &str,
    query: &str,
    control: &SearchControl,
) -> Result<Option<String>, PersistenceError> {
    control.check()?;
    let lowered = text.to_lowercase();
    control.check()?;
    let Some(position) = lowered.find(query) else {
        return Ok(None);
    };
    let mut lowered_bytes = 0;
    let mut original = 0;
    for (index, character) in text.char_indices() {
        if lowered_bytes >= position {
            original = index;
            break;
        }
        lowered_bytes += character.to_lowercase().map(char::len_utf8).sum::<usize>();
        original = index;
    }
    let mut start = 0;
    let mut preceding = std::collections::VecDeque::with_capacity(60);
    for (index, _) in text.grapheme_indices(true) {
        if index >= original {
            break;
        }
        if preceding.len() == 60 {
            preceding.pop_front();
        }
        preceding.push_back(index);
        if index % 4096 == 0 {
            control.check()?;
        }
    }
    if let Some(index) = preceding.front() {
        start = *index;
    }
    let mut result = String::new();
    if start > 0 {
        result.push('…');
    }
    let mut end = start;
    for grapheme in text[start..].graphemes(true).take(238) {
        if result.len() + grapheme.len() > 2045 {
            break;
        }
        result.push_str(grapheme);
        end += grapheme.len();
    }
    if end < text.len() {
        result.push('…');
    }
    Ok(Some(result))
}

#[derive(FromRow)]
struct SearchRecord {
    id: i64,
    session_id: i64,
    agent_id: i64,
    sequence: i64,
    payload_version: i64,
    bytes: i64,
}
