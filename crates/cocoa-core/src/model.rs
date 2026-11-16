//! Provider-neutral completed responses, including opaque continuation data.

use async_trait::async_trait;
use base64::{Engine as _, prelude::BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    context::Context,
    engine::ObservationSink,
    execution::{Cell, WorkLimits},
};

pub use crate::accounting::{
    AccountingSummary, RequestAccounting, RequestPurpose, RequestRecorder, RequestSettings,
    RequestUpdate,
};

/// User text and optional images. Text-only records retain their string encoding.
/// Input budgets count UTF-8 text plus base64 image data and its URL prefix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserInput {
    Text(String),
    WithImages { text: String, images: Vec<Image> },
}

impl UserInput {
    /// Text authored by the user, without encoded image bytes.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) | Self::WithImages { text, .. } => text,
        }
    }

    /// Images in attachment order; empty for text-only input.
    #[must_use]
    pub fn images(&self) -> &[Image] {
        match self {
            Self::Text(_) => &[],
            Self::WithImages { images, .. } => images,
        }
    }

    /// Combined input size used for admission, excluding JSON framing.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.images()
            .iter()
            .fold(self.text().len(), |bytes, image| {
                bytes.saturating_add(image.data_url().len())
            })
    }
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}

/// A PNG attachment, limited to 8 MiB before base64 encoding. Validates the PNG
/// signature, not the entire image. Clipboard adapters must encode valid pixels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Image {
    data_url: String,
}

impl Image {
    /// Encode PNG bytes as an inline attachment.
    ///
    /// # Errors
    /// Reject a missing PNG signature or more than 8 MiB of encoded PNG bytes.
    pub fn from_png(png: &[u8]) -> Result<Self, ImageError> {
        if png.len() > 8 * 1024 * 1024 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(ImageError);
        }
        Ok(Self {
            data_url: format!("data:image/png;base64,{}", BASE64_STANDARD.encode(png)),
        })
    }

    /// Inline PNG data URL for provider image content parts.
    #[must_use]
    pub fn data_url(&self) -> &str {
        &self.data_url
    }
}

impl<'de> Deserialize<'de> for Image {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl TryFrom<String> for Image {
    type Error = ImageError;

    fn try_from(data_url: String) -> Result<Self, Self::Error> {
        let encoded = data_url
            .strip_prefix("data:image/png;base64,")
            .filter(|encoded| encoded.len() <= (8 * 1024 * 1024usize).div_ceil(3) * 4)
            .ok_or(ImageError)?;
        let png = BASE64_STANDARD.decode(encoded).map_err(|_| ImageError)?;
        if png.len() > 8 * 1024 * 1024 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(ImageError);
        }
        Ok(Self { data_url })
    }
}

/// Invalid, unsupported or oversized image data.
#[derive(Debug, thiserror::Error)]
#[error("expected PNG image data no larger than 8 MiB")]
pub struct ImageError;

/// Provider-owned JSON is retained verbatim, not interpreted as chat text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderData {
    pub provider: String,
    pub data: Value,
}

/// Only complete responses enter canonical history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub action: ModelAction,
    /// Opaque replay data for the producing provider, not a display preview.
    pub continuation: Option<ProviderData>,
    /// Absent means the provider did not report usage, not that usage was zero.
    pub usage: Option<Usage>,
}

/// A model either finishes or requests a bounded sequence of complete cells.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ModelAction {
    Final { answer: String },
    Execute { cells: Vec<Cell> },
}

/// Provider-reported token accounting for a single response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Total input, including any reported cache reads and writes.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Reported cache counts only; absence does not mean zero cache activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cache: Option<InputCacheUsage>,
}

/// Provider-reported cache activity within total input usage, without pricing
/// or inferred uncached usage. Each absent count is independently unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputCacheUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_tokens: Option<u64>,
}

/// Ready request data. Providers must bound transport buffers and decoded output
/// while receiving it, not allocate an unlimited response before returning.
#[derive(Clone)]
pub struct ModelRequest {
    pub context: Context,
    /// Executor-owned runtime facts as plain text, at most 4096 encoded JSON
    /// bytes. Already reserved in the context budget; include in instructions
    /// and the provider's encoded request budget, never as a callable method.
    pub executor_description: String,
    /// Initial stream-inactivity window. Streaming progress extends the window.
    /// The feedback byte budget does not limit model responses; providers own
    /// generation and decoder limits, independently of cell and host feedback.
    pub limits: WorkLimits,
    /// Report nonempty received chunks with `progress`, including framing and
    /// keepalive data that produces no display text. This resets the engine's
    /// stall detector independently of optional live UI notifications.
    /// Completed feedback travels in the response.
    pub observations: ObservationSink,
}

/// Read-only checkpoint generation, with a separate output budget and deadline.
/// `input` contains bounded portable evidence and attempt-local source aliases.
/// Providers disable tool use and return only a successfully completed text answer.
/// Definitions may remain present for prefix caching, but calls must be rejected.
#[derive(Clone)]
pub struct SummaryRequest {
    pub input: String,
    pub output_tokens: u32,
    pub output_bytes: usize,
    pub deadline: tokio::time::Instant,
}

/// Estimated model input, its admission limit and proactive compaction threshold.
/// Estimates are not tokenizer-exact counts or guarantees of provider admission.
#[derive(Clone, Copy, Debug)]
pub struct RequestTokenBudget {
    pub input_tokens: u64,
    /// Maximum admitted input after leaving room for generation.
    pub input_limit: u64,
    /// Attempt compaction at or above this count, no greater than `input_limit`.
    /// Failure may retain the unchanged request if it still fits `input_limit`.
    pub compaction_trigger: u64,
}

/// A ready provider may be shared when its implementation supports it.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Account an ordinary request. Concrete transports override this to retain
    /// individual wire attempts and usage reported before a failed response.
    ///
    /// # Errors
    /// Return provider or accounting failures without replaying inference.
    async fn respond_accounted(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
        accounting: RequestAccounting,
    ) -> Result<ModelResponse, ModelError> {
        let mut attempt = accounting.start(RequestPurpose::Response, None).await?;
        let result = self.respond(request, cancellation).await;
        if let Ok(response) = &result
            && let Some(usage) = &response.usage
        {
            attempt.report(usage.clone(), true).await?;
        }
        attempt.finish(&result).await?;
        result
    }

    /// Account read-only compaction. Legacy summarizers expose no usage, so the
    /// default records unknown counters rather than inferring zero.
    ///
    /// # Errors
    /// Return summary or accounting failures without replaying inference.
    async fn summarize_accounted(
        &self,
        request: SummaryRequest,
        context: &ModelRequest,
        input_bytes: usize,
        cancellation: CancellationToken,
        accounting: RequestAccounting,
    ) -> Result<String, ModelError> {
        let mut attempt = accounting.start(RequestPurpose::Compaction, None).await?;
        let result = self
            .summarize_with_context(request, context, input_bytes, cancellation)
            .await;
        attempt.finish(&result).await?;
        result
    }

    /// Bound the complete encoded provider request before transport admission.
    /// Providers may conservatively count the unfitted generation ceiling.
    /// `None` means accounting is unavailable and automatic compaction is disabled.
    ///
    /// # Errors
    /// Report incompatible replay or encoded request limits without making a call.
    fn request_bytes(&self, _request: &ModelRequest) -> Result<Option<usize>, ModelError> {
        Ok(None)
    }

    /// Budget model-visible input against the selected model's context window.
    /// `None` retains byte-only admission for adapters without token accounting.
    ///
    /// # Errors
    /// Reject invalid token limits or input that cannot be measured, without inference.
    fn request_token_budget(
        &self,
        _request: &ModelRequest,
    ) -> Result<Option<RequestTokenBudget>, ModelError> {
        Ok(None)
    }

    /// Generate checkpoint proposal text without execution capabilities or user
    /// approval/retry machinery. The caller validates and resolves the proposal.
    ///
    /// # Errors
    /// Reject unsupported generation, truncation, tool calls, oversized output,
    /// cancellation or deadline expiry. Never return a partial successful proposal.
    async fn summarize(
        &self,
        _request: SummaryRequest,
        _cancellation: CancellationToken,
    ) -> Result<String, ModelError> {
        Err(ModelError::Protocol {
            message: "checkpoint generation unsupported".into(),
        })
    }

    /// Generate a checkpoint while reusing an ordinary request's eligible prefix.
    /// `input_bytes` bounds the complete encoded summary request. Providers may
    /// omit history or use incremental generation when reuse is unsupported or
    /// would exceed that bound. Context supplies no execution authority, and
    /// generation must not acknowledge its instruction delivery or emit live output.
    /// The default preserves [`Self::summarize`] for providers without this route;
    /// those providers retain their existing input accounting.
    ///
    /// # Errors
    /// Return the same errors as [`Self::summarize`], including tool-call rejection.
    async fn summarize_with_context(
        &self,
        request: SummaryRequest,
        _context: &ModelRequest,
        _input_bytes: usize,
        cancellation: CancellationToken,
    ) -> Result<String, ModelError> {
        self.summarize(request, cancellation).await
    }

    /// Produce one complete response. Partial transport output is not a response.
    ///
    /// # Errors
    /// Return transport, protocol or cancellation failures. Providers may retry
    /// explicit temporary rejections with a bounded server-directed delay before
    /// receiving output. Never automatically replay partial responses or unknown
    /// outcomes. An interactive host may explicitly retry a failed request, whose
    /// partial output must never authorize tool execution or enter model history.
    async fn respond(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError>;
}

/// Provider failures do not imply a completed assistant answer.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// Local admission failure before inference, independent of provider replies.
    #[error("model input exceeds context budget: {input_tokens} tokens, limit {input_limit}")]
    ContextLimit { input_tokens: u64, input_limit: u64 },
    /// Explicit provider rejection before any response output was accepted.
    /// Unlike transport failures, this permits startup-only model fallback.
    #[error("model request rejected: {message}")]
    Rejected { message: String },
    #[error("model transport failed: {message}")]
    Transport { message: String },
    #[error("invalid model response: {message}")]
    Protocol { message: String },
    #[error("model stream stalled")]
    Stalled,
    #[error("model request cancelled")]
    Cancelled,
}
