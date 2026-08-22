//! Optional, local-only heading providers.
//!
//! Core policy decides when a heading is due and retains accepted values. This module
//! only discovers an explicitly selected local provider and turns one bounded prompt
//! into one candidate; it never reads transcripts, changes runtime state, or pulls a
//! model.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::timeout,
};
use url::Url;

use agentdeck_core::headings::{HeadingJob, HeadingKind, HeadingRejection, tidy, validate};

use crate::config::{HeadingsBackend, HeadingsConfig};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_CONTENT_BYTES: usize = 4 * 1024;

/// The small, typed setup hint consumed by the future capability payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadingSetupHint {
    pub message: String,
    pub action_label: String,
    pub docs_path: String,
}

/// Discovery has a distinct outcome from an established provider failing a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all_fields = "camelCase")]
pub enum HeadingCapability {
    #[serde(rename = "available")]
    Available { backend: &'static str },
    #[serde(rename = "disabled")]
    Disabled { backend: &'static str },
    #[serde(rename = "missing")]
    MissingProvider {
        backend: &'static str,
        setup_hint: HeadingSetupHint,
    },
    #[serde(rename = "missing")]
    MissingModel {
        backend: &'static str,
        model: String,
        setup_hint: HeadingSetupHint,
    },
    #[serde(rename = "missing")]
    Unconfigured {
        backend: &'static str,
        setup_hint: HeadingSetupHint,
    },
    #[serde(rename = "error")]
    Error {
        backend: &'static str,
        reason: &'static str,
    },
}

impl HeadingCapability {
    #[must_use]
    pub fn setup_hint(&self) -> Option<&HeadingSetupHint> {
        match self {
            Self::MissingProvider { setup_hint, .. }
            | Self::MissingModel { setup_hint, .. }
            | Self::Unconfigured { setup_hint, .. } => Some(setup_hint),
            Self::Available { .. } | Self::Disabled { .. } | Self::Error { .. } => None,
        }
    }
}

/// One heading call's typed failure. Its display text intentionally contains no prompt
/// or provider response text, because both can be derived from local transcripts.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum HeadingProviderError {
    #[error("heading generation lane was busy for too long")]
    AcquireTimeout,
    #[error("Ollama refused the connection")]
    ConnectionRefused,
    #[error("Ollama request timed out")]
    RequestTimeout,
    #[error("Ollama transport failed")]
    Transport,
    #[error("Ollama returned HTTP {0}")]
    HttpStatus(u16),
    #[error("Ollama response body exceeded the limit")]
    ResponseBodyTooLarge,
    #[error("Ollama returned malformed JSON")]
    MalformedResponse,
    #[error("Ollama returned no final message content")]
    EmptyContent,
    #[error("Ollama spent its response on thinking without a final answer")]
    ThinkingOnly,
    #[error("Ollama message content exceeded the limit")]
    ContentTooLong,
    #[error("Ollama candidate was too long")]
    TooLong,
    #[error("Ollama candidate did not pass heading quality checks")]
    Quality(HeadingRejection),
}

/// Async boundary for optional heading generation.
#[async_trait]
pub trait HeadingProvider: Send + Sync {
    async fn generate(
        &self,
        job: &HeadingJob,
        current_title: Option<&str>,
    ) -> Result<Option<String>, HeadingProviderError>;
}

/// Explicitly disabled headings. `generate` is deliberately inert: it cannot make a
/// network call, which keeps `none` safe even if it is accidentally polled.
#[derive(Clone, Debug, Default)]
pub struct NoneHeadingProvider;

#[async_trait]
impl HeadingProvider for NoneHeadingProvider {
    async fn generate(
        &self,
        _job: &HeadingJob,
        _current_title: Option<&str>,
    ) -> Result<Option<String>, HeadingProviderError> {
        Ok(None)
    }
}

/// A discovered provider plus its setup/capability outcome. A missing capability never
/// owns a network-capable provider, so callers can safely keep deterministic headings.
pub struct HeadingProviderSelection {
    pub capability: HeadingCapability,
    pub provider: Box<dyn HeadingProvider>,
}

impl HeadingProviderSelection {
    /// Bounded local discovery. This only uses `GET /api/tags`; it never invokes the
    /// Ollama CLI and never sends a pull/create/generate request.
    pub async fn discover(config: &HeadingsConfig) -> Self {
        if config.backend == HeadingsBackend::None {
            return Self::disabled();
        }

        let Some(models) = JobModels::from_config(config) else {
            return Self::unconfigured();
        };
        let provider = match OllamaHeadingProvider::new(config, models.clone()) {
            Ok(provider) => provider,
            Err(()) => return Self::error("invalid-endpoint"),
        };

        match provider.discover_models().await {
            Ok(installed) => match models.first_missing(&installed) {
                Some(model) => Self::missing_model(model.to_owned()),
                None => Self {
                    capability: HeadingCapability::Available { backend: "ollama" },
                    provider: Box::new(provider),
                },
            },
            Err(HeadingProviderError::ConnectionRefused | HeadingProviderError::RequestTimeout) => {
                Self::missing_provider()
            }
            Err(_) => Self::error("discovery-failed"),
        }
    }

    fn disabled() -> Self {
        Self {
            capability: HeadingCapability::Disabled { backend: "none" },
            provider: Box::new(NoneHeadingProvider),
        }
    }

    fn missing_provider() -> Self {
        Self {
            capability: HeadingCapability::MissingProvider {
                backend: "ollama",
                setup_hint: HeadingSetupHint {
                    message: "Install or start Ollama to generate contextual card headings."
                        .to_owned(),
                    action_label: "Learn more".to_owned(),
                    docs_path: "docs/setup.html#contextual-card-headings".to_owned(),
                },
            },
            provider: Box::new(NoneHeadingProvider),
        }
    }

    fn missing_model(model: String) -> Self {
        let message = format!(
            "Install configured Ollama model '{model}' to generate contextual card headings."
        );
        Self {
            capability: HeadingCapability::MissingModel {
                backend: "ollama",
                model,
                setup_hint: HeadingSetupHint {
                    message,
                    action_label: "Learn more".to_owned(),
                    docs_path: "docs/setup.html#contextual-card-headings".to_owned(),
                },
            },
            provider: Box::new(NoneHeadingProvider),
        }
    }

    fn unconfigured() -> Self {
        Self {
            capability: HeadingCapability::Unconfigured {
                // `auto` is a policy, not a closed capability backend.
                backend: "none",
                setup_hint: HeadingSetupHint {
                    message:
                        "Configure an installed Ollama model to generate contextual card headings."
                            .to_owned(),
                    action_label: "Learn more".to_owned(),
                    docs_path: "docs/setup.html#contextual-card-headings".to_owned(),
                },
            },
            provider: Box::new(NoneHeadingProvider),
        }
    }

    fn error(reason: &'static str) -> Self {
        Self {
            capability: HeadingCapability::Error {
                backend: "ollama",
                reason,
            },
            provider: Box::new(NoneHeadingProvider),
        }
    }
}

#[derive(Clone, Debug)]
struct JobModels {
    title: Option<String>,
    subtitle: Option<String>,
    outcome: Option<String>,
    activity: Option<String>,
}

impl JobModels {
    fn from_config(config: &HeadingsConfig) -> Option<Self> {
        let models = Self {
            title: config.model_for(HeadingKind::Title).map(ToOwned::to_owned),
            subtitle: config
                .model_for(HeadingKind::Subtitle)
                .map(ToOwned::to_owned),
            outcome: config
                .model_for(HeadingKind::Outcome)
                .map(ToOwned::to_owned),
            activity: config
                .model_for(HeadingKind::Activity)
                .map(ToOwned::to_owned),
        };
        (models.title.is_some()
            || models.subtitle.is_some()
            || models.outcome.is_some()
            || models.activity.is_some())
        .then_some(models)
    }

    fn for_kind(&self, kind: HeadingKind) -> Option<&str> {
        match kind {
            HeadingKind::Title => self.title.as_deref(),
            HeadingKind::Subtitle => self.subtitle.as_deref(),
            HeadingKind::Outcome => self.outcome.as_deref(),
            HeadingKind::Activity => self.activity.as_deref(),
        }
    }

    fn first_missing<'a>(&'a self, installed: &[String]) -> Option<&'a str> {
        [
            self.title.as_deref(),
            self.subtitle.as_deref(),
            self.outcome.as_deref(),
            self.activity.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find(|model| {
            !installed
                .iter()
                .any(|available| available.eq_ignore_ascii_case(model))
        })
    }
}

#[derive(Clone, Debug)]
struct OllamaLimits {
    request_timeout: Duration,
    acquire_timeout: Duration,
    max_response_body_bytes: usize,
    max_message_content_bytes: usize,
}

impl Default for OllamaLimits {
    fn default() -> Self {
        Self {
            request_timeout: REQUEST_TIMEOUT,
            acquire_timeout: ACQUIRE_TIMEOUT,
            max_response_body_bytes: MAX_RESPONSE_BODY_BYTES,
            max_message_content_bytes: MAX_MESSAGE_CONTENT_BYTES,
        }
    }
}

/// Local Ollama `/api/chat` provider. The shared semaphore is held until the response
/// body is drained/abandoned, preventing overlapping model loads and residency pressure.
#[derive(Clone)]
pub struct OllamaHeadingProvider {
    client: Client,
    endpoint: Url,
    models: JobModels,
    lane: Arc<Semaphore>,
    limits: OllamaLimits,
}

impl OllamaHeadingProvider {
    fn new(config: &HeadingsConfig, models: JobModels) -> Result<Self, ()> {
        let endpoint = config.endpoint_url().map_err(|_| ())?;
        let limits = OllamaLimits::default();
        let client = Client::builder()
            // Validated Ollama endpoints are loopback-only; never route private model
            // content through a proxy inherited from the environment.
            .no_proxy()
            .timeout(limits.request_timeout)
            .build()
            .map_err(|_| ())?;
        Ok(Self {
            client,
            endpoint,
            models,
            lane: global_generation_lane(),
            limits,
        })
    }

    async fn discover_models(&self) -> Result<Vec<String>, HeadingProviderError> {
        let url = self
            .endpoint
            .join("api/tags")
            .map_err(|_| HeadingProviderError::Transport)?;
        let response = self.send(self.client.get(url)).await?;
        let body = self.read_body(response).await?;
        let parsed: TagsResponse =
            serde_json::from_slice(&body).map_err(|_| HeadingProviderError::MalformedResponse)?;
        Ok(parsed
            .models
            .into_iter()
            .flat_map(|model| [model.name, model.model])
            .flatten()
            .collect())
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, HeadingProviderError> {
        let response = request.send().await.map_err(classify_request_error)?;
        if !response.status().is_success() {
            return Err(HeadingProviderError::HttpStatus(response.status().as_u16()));
        }
        Ok(response)
    }

    async fn acquire(&self) -> Result<OwnedSemaphorePermit, HeadingProviderError> {
        timeout(
            self.limits.acquire_timeout,
            self.lane.clone().acquire_owned(),
        )
        .await
        .map_err(|_| HeadingProviderError::AcquireTimeout)?
        .map_err(|_| HeadingProviderError::Transport)
    }

    async fn read_body(
        &self,
        response: reqwest::Response,
    ) -> Result<Vec<u8>, HeadingProviderError> {
        if response
            .content_length()
            .is_some_and(|length| length as usize > self.limits.max_response_body_bytes)
        {
            return Err(HeadingProviderError::ResponseBodyTooLarge);
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(classify_request_error)?;
            if body.len().saturating_add(chunk.len()) > self.limits.max_response_body_bytes {
                return Err(HeadingProviderError::ResponseBodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[async_trait]
impl HeadingProvider for OllamaHeadingProvider {
    async fn generate(
        &self,
        job: &HeadingJob,
        current_title: Option<&str>,
    ) -> Result<Option<String>, HeadingProviderError> {
        let Some(model) = self.models.for_kind(job.kind) else {
            return Ok(None);
        };
        let _permit = self.acquire().await?;
        let url = self
            .endpoint
            .join("api/chat")
            .map_err(|_| HeadingProviderError::Transport)?;
        let body = ChatRequest::new(model, job);
        let response = self.send(self.client.post(url).json(&body)).await?;
        let response = self.read_body(response).await?;
        let response: ChatResponse = serde_json::from_slice(&response)
            .map_err(|_| HeadingProviderError::MalformedResponse)?;
        let content = response.message.content;
        if content.trim().is_empty() {
            return if response
                .message
                .thinking
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                Err(HeadingProviderError::ThinkingOnly)
            } else {
                Err(HeadingProviderError::EmptyContent)
            };
        }
        if content.len() > self.limits.max_message_content_bytes {
            return Err(HeadingProviderError::ContentTooLong);
        }
        let tidy = tidy(&content, job.kind).map_err(map_rejection)?;
        validate(tidy, job.kind, current_title)
            .map(Some)
            .map_err(HeadingProviderError::Quality)
    }
}

fn map_rejection(rejection: HeadingRejection) -> HeadingProviderError {
    match rejection {
        HeadingRejection::TooLong => HeadingProviderError::TooLong,
        other => HeadingProviderError::Quality(other),
    }
}

fn classify_request_error(error: reqwest::Error) -> HeadingProviderError {
    if error.is_timeout() {
        HeadingProviderError::RequestTimeout
    } else if error.is_connect() && error_chain_contains(&error, "refused") {
        HeadingProviderError::ConnectionRefused
    } else {
        HeadingProviderError::Transport
    }
}

fn error_chain_contains(error: &reqwest::Error, needle: &str) -> bool {
    let needle = needle.to_ascii_lowercase();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(value) = current {
        if value.to_string().to_ascii_lowercase().contains(&needle) {
            return true;
        }
        current = value.source();
    }
    false
}

fn global_generation_lane() -> Arc<Semaphore> {
    static LANE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(LANE.get_or_init(|| Arc::new(Semaphore::new(1))))
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 1],
    think: bool,
    stream: bool,
    keep_alive: &'static str,
    options: ChatOptions,
}

impl<'a> ChatRequest<'a> {
    fn new(model: &'a str, job: &'a HeadingJob) -> Self {
        Self {
            model,
            messages: [ChatMessage {
                role: "user",
                content: &job.prompt,
            }],
            think: false,
            stream: false,
            keep_alive: "30m",
            options: ChatOptions {
                temperature: 0.1,
                num_predict: job.kind.max_tokens(),
                num_ctx: 4096,
            },
        }
    }
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatOptions {
    temperature: f32,
    num_predict: u32,
    num_ctx: u32,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
    #[serde(default)]
    thinking: Option<String>,
}

#[cfg(test)]
mod tests;
