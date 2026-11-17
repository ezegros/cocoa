//! Usage-calibrated context estimates, independent of encoded transport bytes.
//!
//! Text uses one token per three UTF-8 bytes and images reserve 16,384 tokens.
//! Reported output includes hidden reasoning; encrypted replay is not text.
//! Calibration anchors survive resume and apply only to the same checkpoint.

use cocoa_core::{
    model::{ModelAction, ModelError, ModelRequest, ModelResponse, RequestTokenBudget},
    store::RecordKind,
};
use serde_json::{Value, json};

use crate::{ModelLimits, transport::ExecutionMode};

pub(crate) fn budget(
    request: &ModelRequest,
    execution: ExecutionMode,
    limits: ModelLimits,
) -> Result<(RequestTokenBudget, u64), ModelError> {
    let estimated = estimate(request, execution, limits)?;
    let mut input_tokens = estimated;
    for record in request.context.records.iter().rev() {
        let RecordKind::ModelCompleted { response, .. } = &record.kind else {
            continue;
        };
        let Some(usage) = &response.usage else {
            continue;
        };
        let Some(anchor) = response
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.data.get("cocoa_token_budget"))
        else {
            continue;
        };
        if anchor["checkpoint_through"] != checkpoint_through(request) {
            continue;
        }
        if let Some(previous) = anchor["input_tokens_estimated"].as_u64() {
            input_tokens = usage
                .input_tokens
                .saturating_add(estimated.saturating_sub(previous));
            break;
        }
    }
    let context = u64::from(limits.context_limit());
    let reserve = u64::from(limits.generation_reserve()).max(context.div_ceil(10));
    let budget = RequestTokenBudget {
        input_tokens,
        input_limit: limits.input_limit(),
        compaction_trigger: context - reserve,
    };
    debug_assert!(
        budget.compaction_trigger <= budget.input_limit,
        "proactive compaction must leave the unchanged request admissible"
    );
    Ok((budget, estimated))
}

fn estimate(
    request: &ModelRequest,
    execution: ExecutionMode,
    limits: ModelLimits,
) -> Result<u64, ModelError> {
    let mut tokens = text(&crate::instructions(request, execution)?).saturating_add(128);
    if let Some(checkpoint) = crate::checkpoint_context(request)? {
        tokens = tokens.saturating_add(text(&checkpoint) + 16);
    }
    if let Some(shared) = crate::shared_context(request.context.shared_update())? {
        tokens = tokens.saturating_add(text(&shared) + 16);
    }
    for (index, record) in request.context.records.iter().enumerate() {
        let content = match &record.kind {
            RecordKind::InputAccepted { input } => {
                text(input.text()).saturating_add(input.images().len() as u64 * 16_384)
            }
            RecordKind::ModelCompleted {
                response,
                shared_context,
            } => {
                let shared = crate::shared_context(shared_context.as_ref())?;
                response_tokens(response, limits)
                    .saturating_add(shared.as_ref().map_or(0, |shared| text(shared) + 16))
            }
            RecordKind::CellCompleted { outcome, .. } => {
                text(&crate::execution_feedback(request, index, outcome)?)
            }
            RecordKind::TurnCancelled { runtime } => text(crate::cancellation_notice(*runtime)),
            _ => continue,
        };
        tokens = tokens.saturating_add(content).saturating_add(16);
    }
    Ok(tokens)
}

pub(crate) fn anchor(response: &mut ModelResponse, checkpoint: Value, estimated: u64) {
    if response.usage.is_some()
        && let Some(continuation) = &mut response.continuation
    {
        continuation.data["cocoa_token_budget"] = json!({
            "input_tokens_estimated": estimated,
            "checkpoint_through": checkpoint,
        });
    }
}

pub(crate) fn text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(3)
}

pub(crate) fn checkpoint_through(request: &ModelRequest) -> Value {
    json!(
        request
            .context
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.through)
    )
}

fn response_tokens(response: &ModelResponse, limits: ModelLimits) -> u64 {
    // Usage includes hidden reasoning. Adding a ciphertext or text estimate to
    // a reported output count would count the same response twice.
    if let Some(usage) = &response.usage {
        return usage.output_tokens;
    }
    let mut tokens = match &response.action {
        ModelAction::Final { answer } => text(answer),
        ModelAction::Execute { cells } => cells.iter().fold(0_u64, |tokens, cell| {
            tokens.saturating_add(text(&cell.code) + 32)
        }),
    };
    let Some(continuation) = &response.continuation else {
        return tokens;
    };
    let data = &continuation.data;
    if data["output"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["type"] == "reasoning"))
        || data["message"]["content"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("thinking" | "redacted_thinking")
                )
            })
        })
    {
        return tokens.max(u64::from(limits.output_limit()));
    }
    for field in ["reasoning", "reasoning_content"] {
        if let Some(reasoning) = data["message"][field].as_str() {
            tokens = tokens.saturating_add(text(reasoning));
        }
    }
    if matches!(response.action, ModelAction::Execute { .. }) {
        if let Some(content) = data["message"]["content"].as_str() {
            tokens = tokens.saturating_add(text(content));
        }
        if let Some(items) = data["output"].as_array() {
            for item in items {
                if let Some(parts) = item["content"].as_array() {
                    for part in parts {
                        if let Some(content) = part["text"].as_str() {
                            tokens = tokens.saturating_add(text(content));
                        }
                    }
                }
            }
        }
        if let Some(parts) = data["message"]["content"].as_array() {
            for part in parts {
                if let Some(content) = part["text"].as_str() {
                    tokens = tokens.saturating_add(text(content));
                }
            }
        }
    }
    tokens
}
