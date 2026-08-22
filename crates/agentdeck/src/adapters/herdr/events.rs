//! Long-lived Herdr event subscription over its raw NDJSON local-socket API.
//!
//! Event payloads are intentionally discarded. A recognized event is only an
//! invalidation hint for a later authoritative CLI snapshot.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::{Stream, prelude::*};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    time::{Instant, sleep, sleep_until},
};
use tokio_util::sync::CancellationToken;

use super::{HerdrError, HerdrTarget};

pub const SUBSCRIPTION_ID: &str = "agentdeck";
pub const MAX_UNTERMINATED_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const ACK_TIMEOUT: Duration = Duration::from_secs(5);
const MALFORMED_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(60);

/// Every unparameterized global event which can invalidate AgentDeck's
/// consumed Herdr snapshot subset. Keep this order stable: it is part of the
/// wire-contract test.
pub const EVENT_SUBSCRIPTIONS: [&str; 23] = [
    "workspace.created",
    "workspace.updated",
    "workspace.metadata_updated",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.closed",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
];

pub const EVENT_WIRE_NAMES: [&str; 23] = [
    "workspace_created",
    "workspace_updated",
    "workspace_metadata_updated",
    "workspace_renamed",
    "workspace_moved",
    "workspace_reordered",
    "workspace_closed",
    "workspace_focused",
    "worktree_created",
    "worktree_opened",
    "worktree_removed",
    "tab_created",
    "tab_closed",
    "tab_focused",
    "tab_renamed",
    "tab_moved",
    "pane_created",
    "pane_closed",
    "pane_updated",
    "pane_focused",
    "pane_moved",
    "pane_exited",
    "pane_agent_detected",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEndpoint {
    marker: PathBuf,
}

impl EventEndpoint {
    pub fn from_marker(marker: impl Into<PathBuf>) -> Self {
        Self {
            marker: marker.into(),
        }
    }

    pub fn marker(&self) -> &Path {
        &self.marker
    }
}

/// Mirror Herdr's release-build config directory lookup without reading global
/// process state in deterministic tests. The temporary fallback is injected.
pub fn herdr_config_dir_with(
    mut get_env: impl FnMut(&str) -> Option<OsString>,
    temporary_directory: &Path,
) -> PathBuf {
    if let Some(directory) = get_env("XDG_CONFIG_HOME") {
        return PathBuf::from(directory).join("herdr");
    }
    platform_config_dir_with(&mut get_env, temporary_directory)
}

#[cfg(windows)]
fn platform_config_dir_with(
    get_env: &mut impl FnMut(&str) -> Option<OsString>,
    temporary_directory: &Path,
) -> PathBuf {
    if let Some(directory) = get_env("APPDATA") {
        return PathBuf::from(directory).join("herdr");
    }
    if let Some(profile) = get_env("USERPROFILE") {
        return PathBuf::from(profile).join("AppData/Roaming/herdr");
    }
    if let Some(home) = get_env("HOME") {
        return PathBuf::from(home).join(".config/herdr");
    }
    temporary_directory.join("herdr")
}

#[cfg(not(windows))]
fn platform_config_dir_with(
    get_env: &mut impl FnMut(&str) -> Option<OsString>,
    temporary_directory: &Path,
) -> PathBuf {
    get_env("HOME").map_or_else(
        || temporary_directory.join("herdr"),
        |home| PathBuf::from(home).join(".config/herdr"),
    )
}

/// Resolve the same explicit/inherited/default routing precedence used by the
/// Herdr CLI. `herdr_config_dir` is injected so path discovery stays outside
/// deterministic adapter logic.
pub fn resolve_event_endpoint_with(
    target: &HerdrTarget,
    mut get_env: impl FnMut(&str) -> Option<OsString>,
    herdr_config_dir: &Path,
) -> Result<EventEndpoint, HerdrError> {
    let marker = match target {
        HerdrTarget::Session(session) => {
            HerdrTarget::session(session.clone())?;
            marker_for_session(herdr_config_dir, session)
        }
        HerdrTarget::Socket(socket) => {
            HerdrTarget::socket(socket)?;
            socket.clone()
        }
        HerdrTarget::Auto => {
            if let Some(socket) = get_env("HERDR_SOCKET_PATH") {
                let socket = PathBuf::from(socket);
                HerdrTarget::socket(&socket)?;
                socket
            } else if let Some(session) = get_env("HERDR_SESSION") {
                let session = session.to_string_lossy().into_owned();
                HerdrTarget::session(session.clone())?;
                marker_for_session(herdr_config_dir, &session)
            } else {
                marker_for_session(herdr_config_dir, "default")
            }
        }
    };
    Ok(EventEndpoint::from_marker(marker))
}

fn marker_for_session(config_dir: &Path, session: &str) -> PathBuf {
    if session == "default" {
        config_dir.join("herdr.sock")
    } else {
        config_dir.join("sessions").join(session).join("herdr.sock")
    }
}

/// Convert the marker through interprocess's platform mapping and connect.
/// Unix uses a filesystem local socket. Windows deliberately uses
/// `GenericNamespaced`, the same stable mapping Herdr uses for its named pipe.
pub async fn connect_event_endpoint(endpoint: &EventEndpoint) -> io::Result<Stream> {
    connect_marker(&endpoint.marker).await
}

#[cfg(unix)]
async fn connect_marker(marker: &Path) -> io::Result<Stream> {
    let name = marker.as_os_str().to_fs_name::<GenericFilePath>()?;
    Stream::connect(name).await
}

#[cfg(windows)]
async fn connect_marker(marker: &Path) -> io::Result<Stream> {
    // This intentionally matches Herdr's mapping byte for byte.
    let name = marker.to_string_lossy().into_owned();
    let name = name.to_ns_name::<GenericNamespaced>()?;
    Stream::connect(name).await
}

#[cfg(not(any(unix, windows)))]
async fn connect_marker(_marker: &Path) -> io::Result<Stream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Herdr event IPC is supported only on Unix and Windows",
    ))
}

#[derive(Serialize)]
struct SubscribeRequest<'a> {
    id: &'a str,
    method: &'a str,
    params: SubscribeParams<'a>,
}

#[derive(Serialize)]
struct SubscribeParams<'a> {
    subscriptions: Vec<Subscription<'a>>,
}

#[derive(Serialize)]
struct Subscription<'a> {
    #[serde(rename = "type")]
    event_type: &'a str,
}

pub fn event_subscription_request() -> Result<Vec<u8>, EventError> {
    let request = SubscribeRequest {
        id: SUBSCRIPTION_ID,
        method: "events.subscribe",
        params: SubscribeParams {
            subscriptions: EVENT_SUBSCRIPTIONS
                .iter()
                .map(|event_type| Subscription { event_type })
                .collect(),
        },
    };
    let mut frame = serde_json::to_vec(&request).map_err(EventError::EncodeRequest)?;
    frame.push(b'\n');
    Ok(frame)
}

#[derive(Debug, Error)]
pub enum EventError {
    #[error("could not encode the fixed Herdr subscription request: {0}")]
    EncodeRequest(serde_json::Error),
    #[error("Herdr event transport failed: {0}")]
    Io(#[from] io::Error),
    #[error("Herdr event acknowledgement was not received within {0:?}")]
    AckTimeout(Duration),
    #[error("Herdr event stream ended before acknowledgement")]
    EofBeforeAck,
    #[error("Herdr event stream ended after acknowledgement")]
    EofAfterAck,
    #[error("Herdr event frame exceeded the {limit}-byte limit")]
    FrameTooLarge { limit: usize },
    #[error("Herdr event frame was not valid UTF-8")]
    InvalidUtf8,
    #[error("Herdr event frame was malformed JSON: {0}")]
    MalformedJson(serde_json::Error),
    #[error("Herdr subscription acknowledgement did not match id/type")]
    AckMismatch,
    #[error("Herdr event stream returned an error frame")]
    ServerError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameAction {
    Ignore,
    Acknowledged,
    Invalidate,
}

#[derive(Debug)]
pub struct EventFrameDecoder {
    buffer: Vec<u8>,
    pending_carriage_return: bool,
    max_frame_bytes: usize,
}

impl Default for EventFrameDecoder {
    fn default() -> Self {
        Self::new(MAX_UNTERMINATED_FRAME_BYTES)
    }
}

impl EventFrameDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            pending_carriage_return: false,
            max_frame_bytes,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, EventError> {
        let mut frames = Vec::new();
        for &byte in bytes {
            if self.pending_carriage_return {
                if byte == b'\n' {
                    frames.push(std::mem::take(&mut self.buffer));
                    self.pending_carriage_return = false;
                    continue;
                }
                self.push_payload_byte(b'\r')?;
                self.pending_carriage_return = false;
            }

            match byte {
                b'\r' => self.pending_carriage_return = true,
                b'\n' => frames.push(std::mem::take(&mut self.buffer)),
                payload => self.push_payload_byte(payload)?,
            }
        }
        Ok(frames)
    }

    fn push_payload_byte(&mut self, byte: u8) -> Result<(), EventError> {
        if self.buffer.len() == self.max_frame_bytes {
            return Err(EventError::FrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        self.buffer.push(byte);
        Ok(())
    }
}

pub fn decode_event_frame(line: &[u8], acknowledged: bool) -> Result<FrameAction, EventError> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(FrameAction::Ignore);
    }
    let text = std::str::from_utf8(line).map_err(|_| EventError::InvalidUtf8)?;
    let value: Value = serde_json::from_str(text).map_err(EventError::MalformedJson)?;
    let Some(object) = value.as_object() else {
        return Err(EventError::AckMismatch);
    };
    if object.contains_key("error") {
        return if object.get("id").and_then(Value::as_str) == Some(SUBSCRIPTION_ID) {
            Err(EventError::ServerError)
        } else {
            Ok(FrameAction::Ignore)
        };
    }

    if let Some(result) = object.get("result") {
        let matches = object.get("id").and_then(Value::as_str) == Some(SUBSCRIPTION_ID)
            && result.get("type").and_then(Value::as_str) == Some("subscription_started");
        return if matches {
            Ok(if acknowledged {
                FrameAction::Ignore
            } else {
                FrameAction::Acknowledged
            })
        } else {
            Err(EventError::AckMismatch)
        };
    }

    if !acknowledged {
        return Err(EventError::AckMismatch);
    }
    let Some(event) = object.get("event").and_then(Value::as_str) else {
        return Ok(FrameAction::Ignore);
    };
    if is_subscribed_wire_event(event) {
        Ok(FrameAction::Invalidate)
    } else {
        Ok(FrameAction::Ignore)
    }
}

fn is_subscribed_wire_event(event: &str) -> bool {
    EVENT_WIRE_NAMES.contains(&event)
}

#[derive(Clone, Copy, Debug)]
pub struct EventLoopOptions {
    pub acknowledgement_timeout: Duration,
    pub max_unterminated_frame_bytes: usize,
}

impl Default for EventLoopOptions {
    fn default() -> Self {
        Self {
            acknowledgement_timeout: ACK_TIMEOUT,
            max_unterminated_frame_bytes: MAX_UNTERMINATED_FRAME_BYTES,
        }
    }
}

/// Deterministic reconnect schedule. Supply a uniform sample in `0.0..=1.0`;
/// production uses `rand`, while tests inject fixed samples.
#[derive(Clone, Debug, Default)]
pub struct ReconnectBackoff {
    failures: u32,
}

#[derive(Debug, Default)]
struct EventDiagnosticLimiter {
    last_malformed: Option<Instant>,
}

impl EventDiagnosticLimiter {
    fn should_report(&mut self, error: &EventError, now: Instant) -> bool {
        if !matches!(error, EventError::MalformedJson(_)) {
            return true;
        }
        let report = self.last_malformed.is_none_or(|last| {
            now.saturating_duration_since(last) >= MALFORMED_DIAGNOSTIC_INTERVAL
        });
        if report {
            self.last_malformed = Some(now);
        }
        report
    }
}

impl ReconnectBackoff {
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    pub fn next_delay(&mut self, uniform_sample: f64) -> Duration {
        let failure = self.failures;
        self.failures = self.failures.saturating_add(1);
        if failure == 0 {
            return Duration::ZERO;
        }
        let exponent = (failure - 1).min(3);
        let base_seconds = 1_u64 << exponent;
        let sample = uniform_sample.clamp(0.0, 1.0);
        let jitter_factor = 0.8 + sample * 0.4;
        Duration::from_secs_f64(base_seconds as f64 * jitter_factor)
    }
}

/// Run a bounded, cancellation-clean subscription loop. Transport failures are
/// degraded input, not process failures: they reconnect while the independent
/// poll task remains untouched.
pub async fn run_event_subscription(
    endpoint: EventEndpoint,
    invalidations: mpsc::Sender<()>,
    cancellation: CancellationToken,
) -> Result<(), EventError> {
    run_event_subscription_with_jitter(
        endpoint,
        invalidations,
        cancellation,
        EventLoopOptions::default(),
        rand::random::<f64>,
    )
    .await
}

pub async fn run_event_subscription_with_jitter<J>(
    endpoint: EventEndpoint,
    invalidations: mpsc::Sender<()>,
    cancellation: CancellationToken,
    options: EventLoopOptions,
    mut jitter: J,
) -> Result<(), EventError>
where
    J: FnMut() -> f64,
{
    let mut backoff = ReconnectBackoff::default();
    let mut diagnostics = EventDiagnosticLimiter::default();
    loop {
        let connection = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = connect_event_endpoint(&endpoint) => result,
        };

        let (acknowledged, failure) = match connection {
            Ok(mut stream) => {
                match run_connection(&mut stream, &invalidations, &cancellation, options).await {
                    ConnectionEnd::Cancelled => return Ok(()),
                    ConnectionEnd::Ended {
                        acknowledged,
                        error,
                    } => (acknowledged, Some(error)),
                    ConnectionEnd::ReceiverClosed => return Ok(()),
                }
            }
            Err(error) => (false, Some(EventError::Io(error))),
        };

        if let Some(error) = failure {
            if diagnostics.should_report(&error, Instant::now()) {
                tracing::warn!(error = %error, "Herdr event subscription disconnected; retrying");
            }
        }

        if acknowledged {
            backoff.reset();
        }
        let delay = backoff.next_delay(jitter());
        if !delay.is_zero() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                () = sleep(delay) => {}
            }
        }
    }
}

enum ConnectionEnd {
    Cancelled,
    ReceiverClosed,
    Ended {
        acknowledged: bool,
        error: EventError,
    },
}

async fn run_connection<S>(
    stream: &mut S,
    invalidations: &mpsc::Sender<()>,
    cancellation: &CancellationToken,
    options: EventLoopOptions,
) -> ConnectionEnd
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = match event_subscription_request() {
        Ok(request) => request,
        Err(error) => {
            return ConnectionEnd::Ended {
                acknowledged: false,
                error,
            };
        }
    };
    let write = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return ConnectionEnd::Cancelled,
        result = stream.write_all(&request) => result,
    };
    if let Err(error) = write {
        return ConnectionEnd::Ended {
            acknowledged: false,
            error: EventError::Io(error),
        };
    }

    let acknowledgement_deadline = Instant::now() + options.acknowledgement_timeout;
    let mut acknowledged = false;
    let mut decoder = EventFrameDecoder::new(options.max_unterminated_frame_bytes);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = if acknowledged {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return ConnectionEnd::Cancelled,
                result = stream.read(&mut chunk) => result,
            }
        } else {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return ConnectionEnd::Cancelled,
                () = sleep_until(acknowledgement_deadline) => {
                    return ConnectionEnd::Ended {
                        acknowledged: false,
                        error: EventError::AckTimeout(options.acknowledgement_timeout),
                    };
                }
                result = stream.read(&mut chunk) => result,
            }
        };
        let read = match read {
            Ok(0) => {
                return ConnectionEnd::Ended {
                    acknowledged,
                    error: if acknowledged {
                        EventError::EofAfterAck
                    } else {
                        EventError::EofBeforeAck
                    },
                };
            }
            Ok(read) => read,
            Err(error) => {
                return ConnectionEnd::Ended {
                    acknowledged,
                    error: EventError::Io(error),
                };
            }
        };
        let frames = match decoder.push(&chunk[..read]) {
            Ok(frames) => frames,
            Err(error) => {
                return ConnectionEnd::Ended {
                    acknowledged,
                    error,
                };
            }
        };
        for frame in frames {
            match decode_event_frame(&frame, acknowledged) {
                Ok(FrameAction::Ignore) => {}
                Ok(FrameAction::Acknowledged) => acknowledged = true,
                Ok(FrameAction::Invalidate) => match invalidations.try_send(()) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                    Err(mpsc::error::TrySendError::Closed(())) => {
                        return ConnectionEnd::ReceiverClosed;
                    }
                },
                Err(error) => {
                    return ConnectionEnd::Ended {
                        acknowledged,
                        error,
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        ffi::{OsStr, OsString},
        time::Duration,
    };

    use serde_json::Value;
    use tokio::{io::AsyncWriteExt, sync::mpsc};
    use tokio_util::sync::CancellationToken;

    use super::{
        EVENT_SUBSCRIPTIONS, EVENT_WIRE_NAMES, EventEndpoint, EventError, EventFrameDecoder,
        EventLoopOptions, FrameAction, MAX_UNTERMINATED_FRAME_BYTES, ReconnectBackoff,
        SUBSCRIPTION_ID, decode_event_frame, event_subscription_request, herdr_config_dir_with,
        resolve_event_endpoint_with, run_connection,
    };
    use crate::adapters::herdr::HerdrTarget;

    fn request_bytes() -> Vec<u8> {
        event_subscription_request()
            .unwrap_or_else(|error| panic!("request encoding failed: {error}"))
    }

    #[test]
    fn subscribe_request_is_exact_and_contains_only_all_23_global_types() {
        let expected = concat!(
            r#"{"id":"agentdeck","method":"events.subscribe","params":{"subscriptions":["#,
            r#"{"type":"workspace.created"},{"type":"workspace.updated"},{"type":"workspace.metadata_updated"},"#,
            r#"{"type":"workspace.renamed"},{"type":"workspace.moved"},{"type":"workspace.reordered"},"#,
            r#"{"type":"workspace.closed"},{"type":"workspace.focused"},{"type":"worktree.created"},"#,
            r#"{"type":"worktree.opened"},{"type":"worktree.removed"},{"type":"tab.created"},"#,
            r#"{"type":"tab.closed"},{"type":"tab.focused"},{"type":"tab.renamed"},{"type":"tab.moved"},"#,
            r#"{"type":"pane.created"},{"type":"pane.closed"},{"type":"pane.updated"},{"type":"pane.focused"},"#,
            r#"{"type":"pane.moved"},{"type":"pane.exited"},{"type":"pane.agent_detected"}]}}"#,
            "\n"
        );
        assert_eq!(request_bytes(), expected.as_bytes());

        let parsed: Value = serde_json::from_slice(&request_bytes())
            .unwrap_or_else(|error| panic!("request JSON invalid: {error}"));
        let subscriptions = parsed["params"]["subscriptions"]
            .as_array()
            .unwrap_or_else(|| panic!("subscriptions was not an array"));
        assert_eq!(subscriptions.len(), 23);
        assert!(
            !request_bytes()
                .windows(14)
                .any(|part| part == b"layout.updated")
        );
        assert!(
            !request_bytes()
                .windows(22)
                .any(|part| part == b"agent_status_changed")
        );
    }

    #[test]
    fn every_dotted_subscription_accepts_its_underscored_wire_frame() {
        let mut seen = BTreeSet::new();
        for (dotted, wire) in EVENT_SUBSCRIPTIONS.into_iter().zip(EVENT_WIRE_NAMES) {
            assert_eq!(dotted.replace('.', "_"), wire);
            seen.insert(wire);
            let frame = format!(r#"{{"event":"{wire}","data":{{}}}}"#);
            assert_eq!(
                decode_event_frame(frame.as_bytes(), true)
                    .unwrap_or_else(|error| panic!("{wire} rejected: {error}")),
                FrameAction::Invalidate
            );
        }
        assert_eq!(seen.len(), 23);
        assert_eq!(
            decode_event_frame(br#"{"event":"layout_updated","data":{}}"#, true)
                .unwrap_or_else(|error| panic!("unknown event failed: {error}")),
            FrameAction::Ignore
        );
    }

    #[test]
    fn acknowledgement_requires_matching_id_and_result_type() {
        let valid =
            format!(r#"{{"id":"{SUBSCRIPTION_ID}","result":{{"type":"subscription_started"}}}}"#);
        assert_eq!(
            decode_event_frame(valid.as_bytes(), false)
                .unwrap_or_else(|error| panic!("valid ack failed: {error}")),
            FrameAction::Acknowledged
        );
        for invalid in [
            r#"{"id":"other","result":{"type":"subscription_started"}}"#,
            r#"{"id":"agentdeck","result":{"type":"other"}}"#,
            r#"{"event":"pane_focused"}"#,
        ] {
            assert!(matches!(
                decode_event_frame(invalid.as_bytes(), false),
                Err(EventError::AckMismatch)
            ));
        }
    }

    #[test]
    fn error_malformed_json_and_invalid_utf8_force_connection_failure() {
        assert!(matches!(
            decode_event_frame(br#"{"id":"agentdeck","error":{"code":"bad"}}"#, false),
            Err(EventError::ServerError)
        ));
        assert!(matches!(
            decode_event_frame(b"{", true),
            Err(EventError::MalformedJson(_))
        ));
        assert!(matches!(
            decode_event_frame(&[0xff], true),
            Err(EventError::InvalidUtf8)
        ));
    }

    #[test]
    fn only_matching_subscription_error_frames_force_reconnect() {
        for acknowledged in [false, true] {
            assert!(matches!(
                decode_event_frame(
                    br#"{"id":"agentdeck","error":{"code":"secret","message":"do not log me"}}"#,
                    acknowledged
                ),
                Err(EventError::ServerError)
            ));
            for unrelated in [
                br#"{"id":"other","error":{"code":"bad"}}"#.as_slice(),
                br#"{"error":{"code":"bad"}}"#.as_slice(),
            ] {
                assert_eq!(
                    decode_event_frame(unrelated, acknowledged)
                        .unwrap_or_else(|error| panic!("unrelated error failed: {error}")),
                    FrameAction::Ignore
                );
            }
        }
    }

    #[test]
    fn decoder_accepts_split_coalesced_crlf_and_blank_frames() {
        let mut decoder = EventFrameDecoder::new(1024);
        assert!(decoder.push(b"{\"a\":").unwrap_or_default().is_empty());
        let frames = decoder
            .push(b"1}\r\n\n  \r\n{\"b\":2}\n")
            .unwrap_or_else(|error| panic!("decode failed: {error}"));
        assert_eq!(
            frames,
            vec![
                br#"{"a":1}"#.to_vec(),
                Vec::new(),
                b"  ".to_vec(),
                br#"{"b":2}"#.to_vec()
            ]
        );
        for blank in [&frames[1], &frames[2]] {
            assert_eq!(
                decode_event_frame(blank, true)
                    .unwrap_or_else(|error| panic!("blank failed: {error}")),
                FrameAction::Ignore
            );
        }
    }

    #[test]
    fn decoder_accepts_exact_cap_with_lf_crlf_split_and_same_read() {
        for terminator in [b"\n".as_slice(), b"\r\n".as_slice()] {
            let mut same_read = vec![b'x'; MAX_UNTERMINATED_FRAME_BYTES];
            same_read.extend_from_slice(terminator);
            let mut decoder = EventFrameDecoder::default();
            let frames = decoder
                .push(&same_read)
                .unwrap_or_else(|error| panic!("exact-cap same-read frame failed: {error}"));
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].len(), MAX_UNTERMINATED_FRAME_BYTES);

            let mut decoder = EventFrameDecoder::default();
            let prefix = vec![b'x'; MAX_UNTERMINATED_FRAME_BYTES - 1];
            assert!(decoder.push(&prefix).unwrap_or_default().is_empty());
            let mut suffix = vec![b'x'];
            suffix.extend_from_slice(terminator);
            let frames = decoder
                .push(&suffix)
                .unwrap_or_else(|error| panic!("exact-cap split frame failed: {error}"));
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].len(), MAX_UNTERMINATED_FRAME_BYTES);
        }
    }

    #[test]
    fn decoder_rejects_cap_plus_one_with_lf_crlf_split_and_same_read() {
        for terminator in [b"\n".as_slice(), b"\r\n".as_slice()] {
            let mut same_read = vec![b'x'; MAX_UNTERMINATED_FRAME_BYTES + 1];
            same_read.extend_from_slice(terminator);
            assert!(matches!(
                EventFrameDecoder::default().push(&same_read),
                Err(EventError::FrameTooLarge {
                    limit: MAX_UNTERMINATED_FRAME_BYTES
                })
            ));

            let mut decoder = EventFrameDecoder::default();
            let exact = vec![b'x'; MAX_UNTERMINATED_FRAME_BYTES];
            assert!(decoder.push(&exact).unwrap_or_default().is_empty());
            let mut suffix = vec![b'x'];
            suffix.extend_from_slice(terminator);
            assert!(matches!(
                decoder.push(&suffix),
                Err(EventError::FrameTooLarge {
                    limit: MAX_UNTERMINATED_FRAME_BYTES
                })
            ));
        }
    }

    #[test]
    fn routing_is_explicit_then_inherited_then_default() {
        let root = std::path::Path::new("/config/herdr");
        let inherited = |key: &str| match key {
            "HERDR_SOCKET_PATH" => Some(OsString::from("/inherited.sock")),
            "HERDR_SESSION" => Some(OsString::from("ignored")),
            _ => None,
        };
        let explicit_session = resolve_event_endpoint_with(
            &HerdrTarget::session("default")
                .unwrap_or_else(|error| panic!("session rejected: {error}")),
            inherited,
            root,
        )
        .unwrap_or_else(|error| panic!("routing failed: {error}"));
        assert_eq!(explicit_session.marker(), root.join("herdr.sock"));

        let explicit_socket = resolve_event_endpoint_with(
            &HerdrTarget::socket("/explicit.sock")
                .unwrap_or_else(|error| panic!("socket rejected: {error}")),
            inherited,
            root,
        )
        .unwrap_or_else(|error| panic!("routing failed: {error}"));
        assert_eq!(
            explicit_socket.marker(),
            std::path::Path::new("/explicit.sock")
        );

        let automatic = resolve_event_endpoint_with(&HerdrTarget::Auto, inherited, root)
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        assert_eq!(automatic.marker(), std::path::Path::new("/inherited.sock"));

        let inherited_session = resolve_event_endpoint_with(
            &HerdrTarget::Auto,
            |key| (key == "HERDR_SESSION").then(|| OsString::from("team")),
            root,
        )
        .unwrap_or_else(|error| panic!("routing failed: {error}"));
        assert_eq!(
            inherited_session.marker(),
            root.join("sessions/team/herdr.sock")
        );

        let default = resolve_event_endpoint_with(&HerdrTarget::Auto, |_| None, root)
            .unwrap_or_else(|error| panic!("routing failed: {error}"));
        assert_eq!(default.marker(), root.join("herdr.sock"));

        assert!(
            resolve_event_endpoint_with(
                &HerdrTarget::Session("bad session".to_owned()),
                |_| None,
                root
            )
            .is_err()
        );
        assert!(
            resolve_event_endpoint_with(
                &HerdrTarget::Socket(std::path::PathBuf::new()),
                |_| None,
                root
            )
            .is_err()
        );
    }

    #[test]
    fn config_directory_matches_herdr_xdg_home_and_temp_fallbacks() {
        let xdg = herdr_config_dir_with(
            |key| (key == "XDG_CONFIG_HOME").then(|| OsString::from("/xdg")),
            std::path::Path::new("/temporary"),
        );
        assert_eq!(xdg, std::path::Path::new("/xdg/herdr"));

        #[cfg(not(windows))]
        {
            let home = herdr_config_dir_with(
                |key| (key == "HOME").then(|| OsString::from("/home/person")),
                std::path::Path::new("/temporary"),
            );
            assert_eq!(home, std::path::Path::new("/home/person/.config/herdr"));
            let temporary = herdr_config_dir_with(|_| None, std::path::Path::new("/temporary"));
            assert_eq!(temporary, std::path::Path::new("/temporary/herdr"));
        }
    }

    #[test]
    fn backoff_is_immediate_then_one_two_four_eight_with_uniform_jitter_and_reset() {
        let mut backoff = ReconnectBackoff::default();
        assert_eq!(backoff.next_delay(0.5), Duration::ZERO);
        assert_eq!(backoff.next_delay(0.5), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(0.5), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(0.5), Duration::from_secs(4));
        assert_eq!(backoff.next_delay(0.5), Duration::from_secs(8));
        assert_eq!(backoff.next_delay(0.5), Duration::from_secs(8));

        backoff.reset();
        assert_eq!(backoff.next_delay(0.0), Duration::ZERO);
        let low = backoff.next_delay(0.0);
        backoff.reset();
        let _ = backoff.next_delay(1.0);
        let high = backoff.next_delay(1.0);
        assert_eq!(low, Duration::from_millis(800));
        assert_eq!(high, Duration::from_millis(1200));
    }

    #[test]
    fn malformed_json_diagnostics_are_limited_without_hiding_other_failures() {
        fn malformed() -> EventError {
            let error = serde_json::from_str::<Value>("{")
                .err()
                .unwrap_or_else(|| panic!("fixture unexpectedly parsed"));
            EventError::MalformedJson(error)
        }

        let mut limiter = super::EventDiagnosticLimiter::default();
        let start = tokio::time::Instant::now();
        assert!(limiter.should_report(&malformed(), start));
        assert!(!limiter.should_report(&malformed(), start + Duration::from_secs(59)));
        assert!(limiter.should_report(&malformed(), start + Duration::from_secs(60)));
        assert!(limiter.should_report(&EventError::AckMismatch, start));
        assert!(limiter.should_report(&EventError::AckMismatch, start));

        let secret = "private-token-must-not-appear";
        let malformed_frame = format!(r#"{{"secret":"{secret}""#);
        let malformed_error = decode_event_frame(malformed_frame.as_bytes(), true)
            .err()
            .unwrap_or_else(|| panic!("malformed diagnostic fixture unexpectedly succeeded"));
        assert!(!malformed_error.to_string().contains(secret));
        let server_error = decode_event_frame(
            format!(r#"{{"id":"agentdeck","error":{{"message":"{secret}"}}}}"#).as_bytes(),
            true,
        )
        .err()
        .unwrap_or_else(|| panic!("server diagnostic fixture unexpectedly succeeded"));
        assert!(!server_error.to_string().contains(secret));
    }

    #[tokio::test(start_paused = true)]
    async fn acknowledgement_deadline_is_five_seconds() {
        let (mut client, _server) = tokio::io::duplex(4096);
        let (tx, _rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(async move {
            run_connection(&mut client, &tx, &cancellation, EventLoopOptions::default()).await
        });
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        let end = task
            .await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
        assert!(matches!(
            end,
            super::ConnectionEnd::Ended {
                error: EventError::AckTimeout(duration),
                ..
            } if duration == Duration::from_secs(5)
        ));
    }

    #[tokio::test]
    async fn split_ack_and_coalesced_events_emit_bounded_invalidations_then_eof() {
        let (mut client, mut server) = tokio::io::duplex(16 * 1024);
        let (tx, mut rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(async move {
            run_connection(&mut client, &tx, &cancellation, EventLoopOptions::default()).await
        });

        let mut request = vec![0_u8; request_bytes().len()];
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut request)
            .await
            .unwrap_or_else(|error| panic!("request read failed: {error}"));
        assert_eq!(request, request_bytes());
        server
            .write_all(b"{\"id\":\"agent")
            .await
            .unwrap_or_else(|error| panic!("write failed: {error}"));
        server
            .write_all(b"deck\",\"result\":{\"type\":\"subscription_started\"}}\r\n\n{\"event\":\"pane_focused\"}\n{\"event\":\"pane_focused\"}\n")
            .await
            .unwrap_or_else(|error| panic!("write failed: {error}"));
        drop(server);

        assert_eq!(rx.recv().await, Some(()));
        let end = task
            .await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
        assert!(matches!(
            end,
            super::ConnectionEnd::Ended {
                acknowledged: true,
                error: EventError::EofAfterAck,
            }
        ));
    }

    #[tokio::test]
    async fn unrelated_error_ids_are_ignored_before_and_after_acknowledgement() {
        let (mut client, mut server) = tokio::io::duplex(16 * 1024);
        let (tx, mut rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(async move {
            run_connection(&mut client, &tx, &cancellation, EventLoopOptions::default()).await
        });

        let mut request = vec![0_u8; request_bytes().len()];
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut request)
            .await
            .unwrap_or_else(|error| panic!("request read failed: {error}"));
        server
            .write_all(
                b"{\"id\":\"other-before\",\"error\":{\"message\":\"ignore\"}}\n\
                  {\"id\":\"agentdeck\",\"result\":{\"type\":\"subscription_started\"}}\n\
                  {\"id\":\"other-after\",\"error\":{\"message\":\"ignore\"}}\n\
                  {\"event\":\"pane_updated\"}\n",
            )
            .await
            .unwrap_or_else(|error| panic!("response write failed: {error}"));
        drop(server);

        assert_eq!(rx.recv().await, Some(()));
        let end = task
            .await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
        assert!(matches!(
            end,
            super::ConnectionEnd::Ended {
                acknowledged: true,
                error: EventError::EofAfterAck,
            }
        ));
    }

    #[tokio::test]
    async fn thousand_event_burst_collapses_into_one_bounded_invalidation() {
        let (mut client, mut server) = tokio::io::duplex(128 * 1024);
        let (tx, mut rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(async move {
            run_connection(&mut client, &tx, &cancellation, EventLoopOptions::default()).await
        });

        let mut request = vec![0_u8; request_bytes().len()];
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut request)
            .await
            .unwrap_or_else(|error| panic!("request read failed: {error}"));
        let mut response =
            b"{\"id\":\"agentdeck\",\"result\":{\"type\":\"subscription_started\"}}\n".to_vec();
        for _ in 0..1_000 {
            response.extend_from_slice(b"{\"event\":\"pane_updated\"}\n");
        }
        server
            .write_all(&response)
            .await
            .unwrap_or_else(|error| panic!("burst write failed: {error}"));
        drop(server);

        let end = task
            .await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
        assert!(matches!(
            end,
            super::ConnectionEnd::Ended {
                acknowledged: true,
                error: EventError::EofAfterAck,
            }
        ));
        assert_eq!(rx.len(), 1);
        assert_eq!(rx.recv().await, Some(()));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn eof_before_ack_is_a_failed_connection() {
        let (mut client, server) = tokio::io::duplex(16 * 1024);
        let (tx, _rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task = tokio::spawn(async move {
            run_connection(&mut client, &tx, &cancellation, EventLoopOptions::default()).await
        });
        drop(server);
        let end = task
            .await
            .unwrap_or_else(|error| panic!("task failed: {error}"));
        assert!(matches!(
            end,
            super::ConnectionEnd::Ended {
                acknowledged: false,
                error: EventError::Io(_) | EventError::EofBeforeAck,
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_filesystem_socket_uses_interprocess_mapping_end_to_end() {
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory failed: {error}"));
        let marker = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&marker)
            .unwrap_or_else(|error| panic!("socket bind failed: {error}"));
        let endpoint = EventEndpoint::from_marker(&marker);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .unwrap_or_else(|error| panic!("accept failed: {error}"));
            let mut request = vec![0_u8; request_bytes().len()];
            tokio::io::AsyncReadExt::read_exact(&mut stream, &mut request)
                .await
                .unwrap_or_else(|error| panic!("request read failed: {error}"));
            assert_eq!(request, request_bytes());
            stream
                .write_all(
                    b"{\"id\":\"agentdeck\",\"result\":{\"type\":\"subscription_started\"}}\n{\"event\":\"workspace_updated\"}\n",
                )
                .await
                .unwrap_or_else(|error| panic!("response write failed: {error}"));
        });

        let mut client = super::connect_event_endpoint(&endpoint)
            .await
            .unwrap_or_else(|error| panic!("interprocess connect failed: {error}"));
        let (tx, mut rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let end =
            run_connection(&mut client, &tx, &cancellation, EventLoopOptions::default()).await;
        assert_eq!(rx.recv().await, Some(()));
        assert!(matches!(
            end,
            super::ConnectionEnd::Ended {
                acknowledged: true,
                error: EventError::EofAfterAck,
            }
        ));
        server
            .await
            .unwrap_or_else(|error| panic!("server task failed: {error}"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_frame_reconnects_and_valid_ack_resets_to_immediate_retry() {
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory failed: {error}"));
        let marker = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&marker)
            .unwrap_or_else(|error| panic!("socket bind failed: {error}"));
        let endpoint = EventEndpoint::from_marker(&marker);
        let server = tokio::spawn(async move {
            for response in [
                b"{\n".as_slice(),
                b"{\"id\":\"agentdeck\",\"result\":{\"type\":\"subscription_started\"}}\n"
                    .as_slice(),
                b"{\"id\":\"agentdeck\",\"result\":{\"type\":\"subscription_started\"}}\n{\"event\":\"tab_moved\"}\n"
                    .as_slice(),
            ] {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .unwrap_or_else(|error| panic!("accept failed: {error}"));
                let mut request = vec![0_u8; request_bytes().len()];
                tokio::io::AsyncReadExt::read_exact(&mut stream, &mut request)
                    .await
                    .unwrap_or_else(|error| panic!("request read failed: {error}"));
                assert_eq!(request, request_bytes());
                stream
                    .write_all(response)
                    .await
                    .unwrap_or_else(|error| panic!("response write failed: {error}"));
            }
        });

        let (tx, mut rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            super::run_event_subscription_with_jitter(
                endpoint,
                tx,
                task_cancel,
                EventLoopOptions::default(),
                || 0.5,
            )
            .await
        });

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(250), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("reset retry was not immediate")),
            Some(())
        );
        cancellation.cancel();
        task.await
            .unwrap_or_else(|error| panic!("subscriber task failed: {error}"))
            .unwrap_or_else(|error| panic!("subscriber failed: {error}"));
        server
            .await
            .unwrap_or_else(|error| panic!("server task failed: {error}"));
    }

    #[cfg(unix)]
    async fn assert_reconnects_after(first_response: Option<Vec<u8>>) {
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory failed: {error}"));
        let marker = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&marker)
            .unwrap_or_else(|error| panic!("socket bind failed: {error}"));
        let endpoint = EventEndpoint::from_marker(&marker);
        let server = tokio::spawn(async move {
            for response in [
                first_response,
                Some(
                    b"{\"id\":\"agentdeck\",\"result\":{\"type\":\"subscription_started\"}}\n{\"event\":\"pane_updated\"}\n"
                        .to_vec(),
                ),
            ] {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .unwrap_or_else(|error| panic!("accept failed: {error}"));
                let mut request = vec![0_u8; request_bytes().len()];
                tokio::io::AsyncReadExt::read_exact(&mut stream, &mut request)
                    .await
                    .unwrap_or_else(|error| panic!("request read failed: {error}"));
                if let Some(response) = response {
                    // Oversize rejection may close the peer before the server
                    // finishes its write; either outcome exercises reconnect.
                    let _write_result = stream.write_all(&response).await;
                }
            }
        });

        let (tx, mut rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            super::run_event_subscription_with_jitter(
                endpoint,
                tx,
                task_cancel,
                EventLoopOptions::default(),
                || 0.5,
            )
            .await
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("subscriber did not reconnect")),
            Some(())
        );
        cancellation.cancel();
        task.await
            .unwrap_or_else(|error| panic!("subscriber task failed: {error}"))
            .unwrap_or_else(|error| panic!("subscriber failed: {error}"));
        server
            .await
            .unwrap_or_else(|error| panic!("server task failed: {error}"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn eof_invalid_utf8_and_oversize_each_force_reconnect() {
        assert_reconnects_after(None).await;
        assert_reconnects_after(Some(vec![0xff, b'\n'])).await;
        assert_reconnects_after(Some(vec![b'x'; MAX_UNTERMINATED_FRAME_BYTES + 1])).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_interrupts_disconnected_backoff() {
        let directory = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("temporary directory failed: {error}"));
        let endpoint = EventEndpoint::from_marker(directory.path().join("missing.sock"));
        let (tx, _rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            super::run_event_subscription_with_jitter(
                endpoint,
                tx,
                task_cancel,
                EventLoopOptions::default(),
                || 0.5,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        cancellation.cancel();
        tokio::time::timeout(Duration::from_millis(250), task)
            .await
            .unwrap_or_else(|_| panic!("backoff cancellation was not clean"))
            .unwrap_or_else(|error| panic!("subscriber task failed: {error}"))
            .unwrap_or_else(|error| panic!("subscriber failed: {error}"));
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_read() {
        let (mut client, mut server) = tokio::io::duplex(16 * 1024);
        let (tx, _rx) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            run_connection(&mut client, &tx, &task_cancel, EventLoopOptions::default()).await
        });
        let mut request = vec![0_u8; request_bytes().len()];
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut request)
            .await
            .unwrap_or_else(|error| panic!("request read failed: {error}"));
        cancellation.cancel();
        let end = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap_or_else(|_| panic!("connection did not cancel"))
            .unwrap_or_else(|error| panic!("task failed: {error}"));
        assert!(matches!(end, super::ConnectionEnd::Cancelled));
    }

    #[test]
    fn endpoint_preserves_marker_spelling() {
        let endpoint = EventEndpoint::from_marker(OsStr::new("socket marker ü"));
        assert_eq!(endpoint.marker(), std::path::Path::new("socket marker ü"));
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_gate_uses_generic_namespaced_mapping() {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};

        let marker = std::path::Path::new(r"C:\Users\fixture\AppData\Roaming\herdr\herdr.sock");
        let marker_name = marker.to_string_lossy().into_owned();
        assert!(marker_name.to_ns_name::<GenericNamespaced>().is_ok());

        let appdata = herdr_config_dir_with(
            |key| (key == "APPDATA").then(|| OsString::from(r"C:\Users\fixture\AppData\Roaming")),
            std::path::Path::new(r"C:\Temp"),
        );
        assert_eq!(
            appdata,
            std::path::Path::new(r"C:\Users\fixture\AppData\Roaming\herdr")
        );
    }
}
