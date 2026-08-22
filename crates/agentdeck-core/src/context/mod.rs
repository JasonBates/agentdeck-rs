//! Pure context-window extraction from normalized transcript bytes.

use serde_json::Value;

use crate::{
    ContextUsage,
    transcript::{
        CONTEXT_TAIL_BYTES, TailRead, TranscriptKind, bounded_tail_read,
        copilot_event_is_non_ephemeral, parse_transcript_window,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextOutcome {
    Unavailable,
    NotYetCreated,
    Malformed,
    Empty,
    Ready(ContextUsage),
}

#[must_use]
pub fn extract_context(
    kind: TranscriptKind,
    input: crate::transcript::TranscriptInput<'_>,
) -> ContextOutcome {
    if !kind.supports_enrichment()
        || matches!(input, crate::transcript::TranscriptInput::Unavailable)
    {
        return ContextOutcome::Unavailable;
    }
    let crate::transcript::TranscriptInput::Bytes(bytes) = input else {
        return ContextOutcome::NotYetCreated;
    };
    let read = if bytes.len() > CONTEXT_TAIL_BYTES {
        let start = bytes.len() - CONTEXT_TAIL_BYTES;
        TailRead {
            preceding_byte: Some(bytes[start - 1]),
            bytes: &bytes[start..],
        }
    } else {
        TailRead {
            preceding_byte: None,
            bytes,
        }
    };
    extract_context_window(kind, read)
}

/// Parses an adapter-provided tail read. The byte probe lets core discard a partial
/// first NDJSON record while parsing at most 1 MiB of payload bytes.
#[must_use]
pub fn extract_context_window(kind: TranscriptKind, tail: TailRead<'_>) -> ContextOutcome {
    if !kind.supports_enrichment() {
        return ContextOutcome::Unavailable;
    }
    let lines = parse_transcript_window(kind, bounded_tail_read(tail, CONTEXT_TAIL_BYTES));
    if lines.nonempty_lines > 0 && lines.values.is_empty() {
        return ContextOutcome::Malformed;
    }
    let value = if kind == TranscriptKind::Copilot {
        parse_copilot_context(&lines.values)
    } else {
        lines
            .values
            .iter()
            .rev()
            .find_map(|value| parse_one(kind, value))
    };
    value.map_or(ContextOutcome::Empty, ContextOutcome::Ready)
}

#[must_use]
pub fn infer_limit(model: Option<&str>, used: i64) -> i64 {
    let model = model.unwrap_or_default().to_lowercase();
    if model.contains("[1m]") {
        return 1_000_000;
    }
    let tiers: &[i64] = if model.contains("gemini") {
        &[1_000_000]
    } else if model.contains("gpt") {
        &[400_000]
    } else {
        &[200_000, 1_000_000]
    };
    tiers
        .iter()
        .copied()
        .find(|tier| used <= tier.saturating_sub(20_000))
        .unwrap_or(*tiers.last().unwrap_or(&200_000))
}

fn parse_one(kind: TranscriptKind, value: &Value) -> Option<ContextUsage> {
    match kind {
        TranscriptKind::Claude => {
            let message = value.get("message")?.as_object()?;
            let usage = message.get("usage")?.as_object()?;
            let used = component(usage.get("input_tokens"))?
                .checked_add(component(usage.get("cache_read_input_tokens"))?)?
                .checked_add(component(usage.get("cache_creation_input_tokens"))?)?;
            let model = message
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            make(used, infer_limit(model.as_deref(), used), model)
        }
        TranscriptKind::Pi => {
            if value.get("type")?.as_str()? != "message" {
                return None;
            }
            let message = value.get("message")?.as_object()?;
            let usage = message.get("usage")?.as_object()?;
            let used =
                component(usage.get("input"))?.checked_add(component(usage.get("cacheRead"))?)?;
            let model = message
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            make(used, infer_limit(model.as_deref(), used), model)
        }
        TranscriptKind::Codex => {
            if value.get("type")?.as_str()? != "event_msg"
                || value.pointer("/payload/type")?.as_str()? != "token_count"
            {
                return None;
            }
            let info = value.pointer("/payload/info")?.as_object()?;
            let used = nonnegative(
                info.get("last_token_usage")
                    .and_then(Value::as_object)
                    .and_then(|usage| usage.get("total_tokens")),
            )?;
            let limit = match info.get("model_context_window") {
                Some(value) => nonnegative(Some(value))?,
                None => infer_limit(None, used),
            };
            make(used, limit, None)
        }
        TranscriptKind::Copilot | TranscriptKind::Unknown => None,
    }
}

fn parse_copilot_context(values: &[Value]) -> Option<ContextUsage> {
    let mut session_model = None;
    let mut assistant_model = None;
    let mut latest_usage = None;
    for value in values {
        if !copilot_root_event(value) || !copilot_event_is_non_ephemeral(value) {
            continue;
        }
        let Some(event) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(data) = value.get("data").and_then(Value::as_object) else {
            continue;
        };
        match event {
            "session.start" => {
                if let Some(selected) = data.get("selectedModel").and_then(Value::as_str) {
                    session_model = Some(selected.to_owned());
                }
            }
            "assistant.usage" if data.get("parentToolCallId").is_none_or(Value::is_null) => {
                if let Some(current) = data.get("model").and_then(Value::as_str) {
                    assistant_model = Some(current.to_owned());
                }
            }
            "session.usage_info" => {
                let (Some(used), Some(limit)) = (
                    nonnegative(data.get("currentTokens")),
                    nonnegative(data.get("tokenLimit")),
                ) else {
                    continue;
                };
                latest_usage = Some((used, limit));
            }
            _ => {}
        }
    }
    let (used, limit) = latest_usage?;
    make(used, limit, assistant_model.or(session_model))
}

fn copilot_root_event(value: &Value) -> bool {
    value.get("agentId").is_none_or(Value::is_null)
}

fn nonnegative(value: Option<&Value>) -> Option<i64> {
    value?.as_i64().filter(|number| *number >= 0)
}

fn component(value: Option<&Value>) -> Option<i64> {
    value.map_or(Some(0), |value| {
        value.as_i64().filter(|number| *number >= 0)
    })
}

fn make(used: i64, limit: i64, model: Option<String>) -> Option<ContextUsage> {
    if used <= 0 || limit <= 0 {
        return None;
    }
    let percent = ((used as f64 / limit as f64) * 100.0).round() as i64;
    Some(ContextUsage {
        used,
        limit,
        percent: percent.min(100),
        model,
    })
}
