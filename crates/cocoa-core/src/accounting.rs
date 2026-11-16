//! Request evidence is independent of replay history and live observations.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    model::{InputCacheUsage, ModelError, Usage},
    store::StoreError,
};

/// Why an inference request was made. Agent identity belongs to its store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    Response,
    Compaction,
}

/// Effective wire settings, never credentials, endpoints or continuation data.
/// `model` is the requested model, not a claim about undisclosed provider routing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSettings {
    pub provider: String,
    pub model: String,
    pub thinking_level: Option<String>,
    pub output_tokens: Option<u32>,
    pub reasoning_summary: bool,
}

/// Local terminal result. Cancellation does not prove remote generation stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    Completed,
    Rejected,
    Transport,
    Protocol,
    Stalled,
    Cancelled,
}

/// Latest cumulative provider counters. Updates replace, never add to, an earlier
/// snapshot for the same request. Missing counters and outcomes remain unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUpdate {
    pub usage: Option<Usage>,
    pub usage_complete: bool,
    pub outcome: Option<RequestOutcome>,
    pub error: Option<String>,
}

/// Durable request ledger bound to an agent and invocation. Starts commit before
/// transport; unmatched starts retain an unknown outcome after interruption.
#[async_trait]
pub trait RequestRecorder: Send + Sync {
    /// Commit request admission without initiating inference.
    ///
    /// # Errors
    /// Propagate storage failures; callers must not send after failed admission.
    async fn start(
        &self,
        purpose: RequestPurpose,
        settings: Option<&RequestSettings>,
    ) -> Result<i64, StoreError>;

    /// Replace the latest cumulative accounting snapshot for an admitted request.
    ///
    /// # Errors
    /// Propagate storage failures without replaying inference or the write.
    async fn update(&self, request: i64, update: &RequestUpdate) -> Result<(), StoreError>;
}

/// Coverage and token totals. `reported_usage` sums known snapshots and may be a
/// lower bound. `usage` is present only when every request has final counters.
/// Cache fields remain independently unknown when any counted request omits them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountingSummary {
    pub requests_started: u64,
    pub requests_finished: u64,
    pub requests_with_usage: u64,
    pub requests_with_final_usage: u64,
    /// Completed responses from older sessions without request-ledger coverage.
    pub legacy_responses: u64,
    pub usage: Option<Usage>,
    pub reported_usage: Option<Usage>,
}

impl AccountingSummary {
    /// Aggregate one latest snapshot per request, with checked token arithmetic.
    ///
    /// # Errors
    /// Reject counter overflow instead of publishing a wrapped or saturated total.
    pub fn from_updates<'a>(
        updates: impl IntoIterator<Item = &'a RequestUpdate>,
        legacy_responses: u64,
    ) -> Result<Self, StoreError> {
        let mut result = Self {
            legacy_responses,
            ..Self::default()
        };
        for update in updates {
            result.requests_started += 1;
            result.requests_finished += u64::from(update.outcome.is_some());
            let Some(usage) = &update.usage else { continue };
            result.requests_with_usage += 1;
            result.requests_with_final_usage += u64::from(update.usage_complete);
            result.reported_usage = Some(match result.reported_usage.take() {
                None => usage.clone(),
                Some(total) => Usage {
                    input_tokens: total
                        .input_tokens
                        .checked_add(usage.input_tokens)
                        .ok_or(StoreError::Capacity)?,
                    output_tokens: total
                        .output_tokens
                        .checked_add(usage.output_tokens)
                        .ok_or(StoreError::Capacity)?,
                    input_cache: match (total.input_cache, &usage.input_cache) {
                        (Some(total), Some(next)) => Some(InputCacheUsage {
                            read_tokens: add_optional(total.read_tokens, next.read_tokens)?,
                            write_tokens: add_optional(total.write_tokens, next.write_tokens)?,
                        }),
                        _ => None,
                    },
                },
            });
        }
        if result.requests_started > 0
            && result.requests_with_final_usage == result.requests_started
            && legacy_responses == 0
        {
            result.usage = result.reported_usage.clone();
        }
        Ok(result)
    }
}

/// Bounded invocation-local accounting with an optional durable recorder. This is
/// separate from UI notifications. At most 65,536 transport attempts are admitted.
#[derive(Clone, Default)]
pub struct RequestAccounting {
    state: Arc<Mutex<Vec<RequestUpdate>>>,
    recorder: Option<Arc<dyn RequestRecorder>>,
}

impl RequestAccounting {
    #[must_use]
    pub fn new(recorder: Option<Arc<dyn RequestRecorder>>) -> Self {
        Self {
            recorder,
            ..Self::default()
        }
    }

    /// Read complete invocation-local coverage, including unfinished attempts.
    ///
    /// # Errors
    /// Reject poisoned state or counter overflow.
    pub fn summary(&self) -> Result<AccountingSummary, StoreError> {
        AccountingSummary::from_updates(
            self.state
                .lock()
                .map_err(|_| StoreError::InvalidRecord)?
                .iter(),
            0,
        )
    }

    /// Admit a request before sending it. A dropped attempt stays unfinished.
    ///
    /// # Errors
    /// Reject storage failures or the invocation's attempt bound.
    pub async fn start(
        &self,
        purpose: RequestPurpose,
        settings: Option<&RequestSettings>,
    ) -> Result<AccountedRequest, ModelError> {
        let index = {
            let mut state = self.state.lock().map_err(|_| accounting_error())?;
            if state.len() >= 65_536 {
                return Err(accounting_error());
            }
            let index = state.len();
            state.push(RequestUpdate::default());
            index
        };
        let durable = match &self.recorder {
            Some(recorder) => Some(
                recorder
                    .start(purpose, settings)
                    .await
                    .map_err(|_| accounting_error())?,
            ),
            None => None,
        };
        Ok(AccountedRequest {
            accounting: self.clone(),
            index,
            durable,
            latest: RequestUpdate::default(),
        })
    }
}

/// A single transport attempt. Only cumulative provider snapshots are accepted.
pub struct AccountedRequest {
    accounting: RequestAccounting,
    index: usize,
    durable: Option<i64>,
    latest: RequestUpdate,
}

impl AccountedRequest {
    /// Retain reported counters before decoding or accepting model actions.
    ///
    /// # Errors
    /// Propagate accounting storage failures without authorizing execution.
    pub async fn report(&mut self, usage: Usage, complete: bool) -> Result<(), ModelError> {
        self.latest.usage = Some(usage);
        self.latest.usage_complete = complete;
        self.publish().await
    }

    /// Retain the local terminal result, without changing provider counters.
    ///
    /// # Errors
    /// Propagate accounting storage failures without replaying the request.
    pub async fn finish<T>(&mut self, result: &Result<T, ModelError>) -> Result<(), ModelError> {
        self.latest.outcome = Some(match result {
            Ok(_) => RequestOutcome::Completed,
            Err(ModelError::Rejected { .. } | ModelError::ContextLimit { .. }) => {
                RequestOutcome::Rejected
            }
            Err(ModelError::Transport { .. }) => RequestOutcome::Transport,
            Err(ModelError::Protocol { .. }) => RequestOutcome::Protocol,
            Err(ModelError::Stalled) => RequestOutcome::Stalled,
            Err(ModelError::Cancelled) => RequestOutcome::Cancelled,
        });
        self.latest.error = result
            .as_ref()
            .err()
            .map(|error| error.to_string().chars().take(2048).collect());
        self.publish().await
    }

    async fn publish(&self) -> Result<(), ModelError> {
        if let (Some(recorder), Some(request)) = (&self.accounting.recorder, self.durable) {
            recorder
                .update(request, &self.latest)
                .await
                .map_err(|_| accounting_error())?;
        }
        self.accounting
            .state
            .lock()
            .map_err(|_| accounting_error())?[self.index] = self.latest.clone();
        Ok(())
    }
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Result<Option<u64>, StoreError> {
    match (left, right) {
        (Some(left), Some(right)) => left
            .checked_add(right)
            .map(Some)
            .ok_or(StoreError::Capacity),
        _ => Ok(None),
    }
}

fn accounting_error() -> ModelError {
    ModelError::Protocol {
        message: "request accounting could not be retained".into(),
    }
}
