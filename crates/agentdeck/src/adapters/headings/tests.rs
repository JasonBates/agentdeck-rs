use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::State,
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::any,
};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify, Semaphore, mpsc},
    time::{advance, timeout},
};
use url::Url;

use agentdeck_core::headings::{HeadingJob, title_job};

use super::{
    HeadingCapability, HeadingProvider, HeadingProviderError, HeadingProviderSelection, JobModels,
    NoneHeadingProvider, OllamaHeadingProvider, OllamaLimits,
};
use crate::config::{Config, HeadingsBackend, ModelOverride};

async fn start(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("test listener must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("test listener must have an address: {error}"));
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .unwrap_or_else(|error| panic!("test server must run: {error}"));
    });
    format!("http://{address}")
}

fn provider(
    endpoint: String,
    model: &str,
    lane: Arc<Semaphore>,
    limits: OllamaLimits,
) -> OllamaHeadingProvider {
    OllamaHeadingProvider {
        client: reqwest::Client::builder()
            .timeout(limits.request_timeout)
            .build()
            .unwrap_or_else(|error| panic!("test client must build: {error}")),
        endpoint: Url::parse(&endpoint)
            .unwrap_or_else(|error| panic!("test endpoint must parse: {error}")),
        models: JobModels {
            title: Some(model.to_owned()),
            subtitle: Some(model.to_owned()),
            outcome: Some(model.to_owned()),
            activity: Some(model.to_owned()),
        },
        lane,
        limits,
    }
}

fn title_job_for_test() -> HeadingJob {
    title_job(&agentdeck_core::transcript::TranscriptDigest {
        opening: "Build heading adapter".to_owned(),
        requests: String::new(),
        recent: String::new(),
        last_prompt: String::new(),
        last_prompt_key: None,
        last_reply: String::new(),
        last_reply_key: None,
        written_at: 0,
    })
}

#[tokio::test]
async fn none_provider_performs_zero_network_calls() {
    let provider = NoneHeadingProvider;
    assert_eq!(
        provider.generate(&title_job_for_test(), None).await,
        Ok(None)
    );
}

#[tokio::test]
async fn discovery_checks_only_installed_configured_arbitrary_tags() {
    async fn tags() -> impl IntoResponse {
        axum::Json(json!({"models":[{"name":"TINY/PRIVATE-TAG:Q4"}, {"model":"another:tag"}]}))
    }
    let endpoint = start(Router::new().route("/api/tags", any(tags))).await;
    let mut config = Config::default();
    config.headings.backend = HeadingsBackend::Auto;
    config.headings.endpoint = endpoint;
    config.headings.model = Some("tiny/private-tag:q4".to_owned());
    config.headings.subtitle_model = ModelOverride::Tag("another:tag".to_owned());

    let selection = HeadingProviderSelection::discover(&config.headings).await;
    assert_eq!(
        selection.capability,
        HeadingCapability::Available { backend: "ollama" }
    );
}

#[tokio::test]
async fn discovery_missing_model_is_typed_and_never_pulls() {
    let paths = Arc::new(Mutex::new(Vec::new()));
    async fn handler(
        State(paths): State<Arc<Mutex<Vec<String>>>>,
        request: axum::extract::Request,
    ) -> impl IntoResponse {
        paths.lock().await.push(request.uri().path().to_owned());
        axum::Json(json!({"models":[]})).into_response()
    }
    let endpoint = start(
        Router::new()
            .fallback(any(handler))
            .with_state(Arc::clone(&paths)),
    )
    .await;
    let mut config = Config::default();
    config.headings.endpoint = endpoint;
    config.headings.model = Some("chosen/small-model:q4".to_owned());

    let selection = HeadingProviderSelection::discover(&config.headings).await;
    assert!(
        matches!(selection.capability, HeadingCapability::MissingModel { ref model, ref setup_hint, .. } if model == "chosen/small-model:q4" && setup_hint.message.contains(model))
    );
    assert_eq!(*paths.lock().await, vec!["/api/tags"]);
}

#[tokio::test]
async fn unconfigured_auto_is_honest_and_setup_hints_are_ui_safe() {
    let config = Config::default();
    let selection = HeadingProviderSelection::discover(&config.headings).await;
    assert!(matches!(
        selection.capability,
        HeadingCapability::Unconfigured {
            backend: "none",
            ..
        }
    ));
    let hint = selection
        .capability
        .setup_hint()
        .unwrap_or_else(|| panic!("unconfigured capability must explain setup"));
    assert_eq!(hint.action_label, "Learn more");
    assert_eq!(hint.docs_path, "docs/setup.html#contextual-card-headings");
    assert!(hint.message.contains("contextual card headings"));
    let wire = serde_json::to_value(&selection.capability)
        .unwrap_or_else(|error| panic!("capability must serialize: {error}"));
    assert_eq!(wire["state"], "missing");
    assert_eq!(
        wire["setupHint"]["docsPath"],
        "docs/setup.html#contextual-card-headings"
    );
    assert!(
        include_str!("../../../../../Public/docs/setup.html")
            .contains("id=\"contextual-card-headings\"")
    );
}

#[tokio::test]
async fn discovery_missing_provider_is_typed() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("must inspect: {error}"));
    drop(listener);
    let mut config = Config::default();
    config.headings.endpoint = format!("http://{address}");
    config.headings.model = Some("chosen/model".to_owned());

    let selection = HeadingProviderSelection::discover(&config.headings).await;
    assert!(matches!(
        selection.capability,
        HeadingCapability::MissingProvider { .. }
    ));
}

#[tokio::test]
async fn chat_uses_the_exact_ollama_url_and_body() {
    let (sent, mut received) = mpsc::channel(1);
    async fn handler(
        State(sent): State<mpsc::Sender<(Method, String, Value)>>,
        request: axum::extract::Request,
    ) -> impl IntoResponse {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let body = axum::body::to_bytes(request.into_body(), 32 * 1024)
            .await
            .unwrap_or_else(|error| panic!("request body must be readable: {error}"));
        let json = serde_json::from_slice(&body)
            .unwrap_or_else(|error| panic!("request body must be JSON: {error}"));
        sent.send((method, path, json))
            .await
            .unwrap_or_else(|error| panic!("test receiver must remain open: {error}"));
        axum::Json(json!({"message":{"content":"Build heading adapter"}}))
    }
    let endpoint = start(
        Router::new()
            .route("/api/chat", any(handler))
            .with_state(sent),
    )
    .await;
    let job = title_job_for_test();
    let provider = provider(
        endpoint,
        "opaque/model:small",
        Arc::new(Semaphore::new(1)),
        OllamaLimits::default(),
    );

    assert_eq!(
        provider.generate(&job, None).await,
        Ok(Some("Build heading adapter".to_owned()))
    );
    let (method, path, body) = received
        .recv()
        .await
        .unwrap_or_else(|| panic!("request must arrive"));
    assert_eq!(method, Method::POST);
    assert_eq!(path, "/api/chat");
    assert_eq!(
        body,
        json!({
            "model": "opaque/model:small",
            "messages": [{"role": "user", "content": job.prompt}],
            "think": false,
            "stream": false,
            "keep_alive": "30m",
            "options": {"temperature": 0.1, "num_predict": 40, "num_ctx": 4096}
        })
    );
}

#[tokio::test]
async fn generation_distinguishes_http_and_response_failures() {
    async fn handler(State(response): State<(StatusCode, String)>) -> impl IntoResponse {
        response.into_response()
    }
    let job = title_job_for_test();
    for (case, response, expected) in [
        ("non2xx", (StatusCode::SERVICE_UNAVAILABLE, "offline".to_owned()), HeadingProviderError::HttpStatus(503)),
        ("malformed", (StatusCode::OK, "not json".to_owned()), HeadingProviderError::MalformedResponse),
        ("empty", (StatusCode::OK, json!({"message":{"content":""}}).to_string()), HeadingProviderError::EmptyContent),
        ("thinking", (StatusCode::OK, json!({"message":{"content":"", "thinking":"reasoning"}}).to_string()), HeadingProviderError::ThinkingOnly),
        ("long", (StatusCode::OK, json!({"message":{"content": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}).to_string()), HeadingProviderError::TooLong),
        ("quality", (StatusCode::OK, json!({"message":{"content":"Helping with heading adapter"}}).to_string()), HeadingProviderError::Quality(agentdeck_core::headings::HeadingRejection::AssistantAction)),
    ] {
        let endpoint = start(Router::new().route("/api/chat", any(handler)).with_state(response)).await;
        let provider = provider(endpoint, "model", Arc::new(Semaphore::new(1)), OllamaLimits::default());
        assert_eq!(provider.generate(&job, None).await, Err(expected), "case {case}");
    }
}

#[tokio::test]
async fn generation_enforces_body_and_message_content_limits() {
    async fn handler(State(response): State<String>) -> impl IntoResponse {
        response
    }
    let job = title_job_for_test();
    for (response, expected) in [
        (
            "x".repeat(16 * 1024 + 1),
            HeadingProviderError::ResponseBodyTooLarge,
        ),
        (
            json!({"message":{"content": "x".repeat(4097)}}).to_string(),
            HeadingProviderError::ContentTooLong,
        ),
    ] {
        let endpoint = start(
            Router::new()
                .route("/api/chat", any(handler))
                .with_state(response),
        )
        .await;
        let provider = provider(
            endpoint,
            "model",
            Arc::new(Semaphore::new(1)),
            OllamaLimits::default(),
        );
        assert_eq!(provider.generate(&job, None).await, Err(expected));
    }
}

#[tokio::test]
async fn connection_refused_and_timeout_are_distinct() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("must inspect: {error}"));
    drop(listener);
    let refused = provider(
        format!("http://{address}"),
        "model",
        Arc::new(Semaphore::new(1)),
        OllamaLimits::default(),
    );
    assert_eq!(
        refused.generate(&title_job_for_test(), None).await,
        Err(HeadingProviderError::ConnectionRefused)
    );

    async fn slow() -> impl IntoResponse {
        tokio::time::sleep(Duration::from_secs(5)).await;
        axum::Json(json!({"message":{"content":"Build heading adapter"}}))
    }
    let endpoint = start(Router::new().route("/api/chat", any(slow))).await;
    let limits = OllamaLimits {
        request_timeout: Duration::from_millis(20),
        ..OllamaLimits::default()
    };
    let slow = provider(endpoint, "model", Arc::new(Semaphore::new(1)), limits);
    assert_eq!(
        slow.generate(&title_job_for_test(), None).await,
        Err(HeadingProviderError::RequestTimeout)
    );
}

#[tokio::test]
async fn one_generation_at_a_time_holds_the_permit_through_the_response() {
    let started = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    async fn handler(
        State((started, release)): State<(Arc<AtomicUsize>, Arc<Notify>)>,
    ) -> impl IntoResponse {
        started.fetch_add(1, Ordering::SeqCst);
        release.notified().await;
        axum::Json(json!({"message":{"content":"Build heading adapter"}}))
    }
    let endpoint = start(
        Router::new()
            .route("/api/chat", any(handler))
            .with_state((Arc::clone(&started), Arc::clone(&release))),
    )
    .await;
    let provider = Arc::new(provider(
        endpoint,
        "model",
        Arc::new(Semaphore::new(1)),
        OllamaLimits::default(),
    ));
    let first = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move { provider.generate(&title_job_for_test(), None).await })
    };
    timeout(Duration::from_secs(1), async {
        while started.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("first request must start"));
    let second = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move { provider.generate(&title_job_for_test(), None).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(started.load(Ordering::SeqCst), 1);
    release.notify_waiters();
    assert!(
        first
            .await
            .unwrap_or_else(|error| panic!("first task must finish: {error}"))
            .is_ok()
    );
    timeout(Duration::from_secs(1), async {
        while started.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("second request must start"));
    release.notify_waiters();
    assert!(
        second
            .await
            .unwrap_or_else(|error| panic!("second task must finish: {error}"))
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn acquisition_timeout_uses_injected_limits_without_wall_clock_sleep() {
    let lane = Arc::new(Semaphore::new(1));
    let held = lane
        .clone()
        .acquire_owned()
        .await
        .unwrap_or_else(|error| panic!("permit must acquire: {error}"));
    let limits = OllamaLimits {
        acquire_timeout: Duration::from_secs(30),
        ..OllamaLimits::default()
    };
    let provider = provider("http://127.0.0.1:1".to_owned(), "model", lane, limits);
    let task = tokio::spawn(async move { provider.generate(&title_job_for_test(), None).await });
    tokio::task::yield_now().await;
    advance(Duration::from_secs(30)).await;
    assert_eq!(
        task.await
            .unwrap_or_else(|error| panic!("task must finish: {error}")),
        Err(HeadingProviderError::AcquireTimeout)
    );
    drop(held);
}
