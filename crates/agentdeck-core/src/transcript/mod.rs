//! Bounded, adapter-independent transcript normalization and digest policy.
//!
//! Filesystem access is deliberately absent: adapters supply bytes and metadata, then
//! own missing-file/error mapping. This keeps parsing deterministic and portable.

use sha2::{Digest, Sha256};
use unicode_segmentation::UnicodeSegmentation;

use serde_json::Value;

use crate::HerdrAgentSession;

pub const OPENING_HEAD_BYTES: usize = 256 * 1024;
// Reuse the full bounded context tail. Tool-heavy Codex turns can put the latest user
// message more than 512 KiB behind the growing end of the rollout before a restarted
// bridge gets its first observation; truncating the already-read 1 MiB window then
// leaves titles with replies but no request. This changes no filesystem I/O bound.
pub const DIGEST_TAIL_BYTES: usize = 1024 * 1024;
pub const CONTEXT_TAIL_BYTES: usize = 1024 * 1024;
const _: () = assert!(DIGEST_TAIL_BYTES <= CONTEXT_TAIL_BYTES);
pub const USER_TURNS: usize = 5;
pub const ASSISTANT_TURNS: usize = 2;
pub const USER_CLIP_GRAPHEMES: usize = 400;
pub const OPENING_CLIP_GRAPHEMES: usize = 300;
pub const NEWEST_ASSISTANT_CLIP_GRAPHEMES: usize = 1400;
/// Copilot can persist very large tool events. Individual physical records over
/// this bound are ignored before JSON allocation or parsing.
pub const COPILOT_MAX_PHYSICAL_EVENT_BYTES: usize = 96 * 1024;
/// A corrupt Copilot physical line may contain a later complete event suffix.
/// Recovery is deliberately bounded and only considers this many event anchors.
pub const COPILOT_MAX_RECOVERY_ANCHORS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptKind {
    Claude,
    Pi,
    Codex,
    Copilot,
    Unknown,
}

impl TranscriptKind {
    /// Whether this transcript format may enrich reply/read-age/context data.
    #[must_use]
    pub const fn supports_enrichment(self) -> bool {
        matches!(self, Self::Claude | Self::Pi | Self::Codex | Self::Copilot)
    }

    /// Whether transcript data may be supplied to generated-heading policy.
    /// Copilot intentionally remains excluded pending a separate evaluation.
    #[must_use]
    pub const fn supports_generated_headings(self) -> bool {
        matches!(self, Self::Claude | Self::Pi | Self::Codex)
    }

    /// Historical heading-policy predicate. Use [`Self::supports_enrichment`]
    /// for local transcript parsing.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        self.supports_generated_headings()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptInput<'a> {
    Unavailable,
    NotYetCreated,
    Bytes(&'a [u8]),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptOutcome {
    Unavailable,
    NotYetCreated,
    Malformed,
    Empty,
    Ready(Box<TranscriptAnalysis>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptAnalysis {
    pub opening: Option<String>,
    pub digest: Option<TranscriptDigest>,
    pub malformed_lines: usize,
    pub decoded_records: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptDigest {
    pub opening: String,
    pub requests: String,
    pub recent: String,
    pub last_prompt: String,
    pub last_prompt_key: Option<String>,
    pub last_reply: String,
    pub last_reply_key: Option<String>,
    pub written_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptRecord {
    pub role: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLines {
    pub values: Vec<Value>,
    pub malformed_lines: usize,
    pub nonempty_lines: usize,
    /// Number of Copilot recovery candidates passed to the JSON parser. This is
    /// zero for ordinary NDJSON and bounded for Copilot event logs.
    pub recovery_attempts: usize,
}

/// A tail read supplied by a filesystem adapter. `preceding_byte` is the one byte
/// immediately before `bytes` in the file, if one exists. Adapters obtain it by
/// reading at most `limit + 1` bytes; core consumes no more than `limit` payload
/// bytes and uses the probe to decide whether the first line is partial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TailRead<'a> {
    pub preceding_byte: Option<u8>,
    pub bytes: &'a [u8],
}

/// Returns at most `limit` bytes from the beginning of a document.
#[must_use]
pub fn bounded_head(bytes: &[u8], limit: usize) -> &[u8] {
    &bytes[..bytes.len().min(limit)]
}

/// Returns at most `limit` tail bytes, discarding a partial first NDJSON record.
/// A final line without a trailing newline is retained.
#[must_use]
pub fn bounded_tail(bytes: &[u8], limit: usize) -> &[u8] {
    if bytes.len() <= limit {
        return bytes;
    }
    let start = bytes.len() - limit;
    bounded_tail_read(
        TailRead {
            preceding_byte: Some(bytes[start - 1]),
            bytes: &bytes[start..],
        },
        limit,
    )
}

/// Caps an adapter-provided tail to `limit` bytes and discards a partial first line.
/// The final unterminated line is always retained.
#[must_use]
pub fn bounded_tail_read(read: TailRead<'_>, limit: usize) -> &[u8] {
    let (preceding_byte, window) = if read.bytes.len() > limit {
        let start = read.bytes.len() - limit;
        (Some(read.bytes[start - 1]), &read.bytes[start..])
    } else {
        (read.preceding_byte, read.bytes)
    };
    if preceding_byte.is_none() || preceding_byte == Some(b'\n') {
        return window;
    }
    window
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(&[], |newline| &window[newline + 1..])
}

/// Lossily decodes and parses NDJSON. Isolated bad lines do not poison valid records.
#[must_use]
pub fn parse_ndjson(bytes: &[u8]) -> ParsedLines {
    let text = String::from_utf8_lossy(bytes);
    let mut values = Vec::new();
    let mut malformed_lines = 0;
    let mut nonempty_lines = 0;
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        nonempty_lines += 1;
        match serde_json::from_str(line) {
            Ok(value) => values.push(value),
            Err(_) => malformed_lines += 1,
        }
    }
    ParsedLines {
        values,
        malformed_lines,
        nonempty_lines,
        recovery_attempts: 0,
    }
}

/// Parses Copilot's local event log within a bounded window. A malformed
/// physical line is soft-failed; when it contains a later `{"type"...}` event
/// suffix, at most [`COPILOT_MAX_RECOVERY_ANCHORS`] suffixes are attempted.
#[must_use]
pub fn parse_copilot_events(bytes: &[u8]) -> ParsedLines {
    let mut values = Vec::new();
    let mut malformed_lines = 0;
    let mut nonempty_lines = 0;
    let mut recovery_attempts = 0;
    for raw_line in bytes.split(|byte| *byte == b'\n') {
        // A physical line excludes its LF delimiter but deliberately includes a
        // preceding CR. That makes CRLF accounting exact and prevents padding
        // whitespace from bypassing this raw-byte cap.
        if raw_line.len() > COPILOT_MAX_PHYSICAL_EVENT_BYTES {
            nonempty_lines += 1;
            malformed_lines += 1;
            continue;
        }
        let line = trim_ascii_line(raw_line);
        if line.is_empty() {
            continue;
        }
        nonempty_lines += 1;
        match serde_json::from_slice(line) {
            Ok(value) => values.push(value),
            Err(_) => {
                malformed_lines += 1;
                recover_copilot_suffixes(line, &mut values, &mut recovery_attempts);
            }
        }
    }
    ParsedLines {
        values,
        malformed_lines,
        nonempty_lines,
        recovery_attempts,
    }
}

fn trim_ascii_line(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn recover_copilot_suffixes(line: &[u8], values: &mut Vec<Value>, attempts: &mut usize) {
    for (offset, window) in line.windows(7).enumerate() {
        if window != b"{\"type\"" {
            continue;
        }
        if *attempts >= COPILOT_MAX_RECOVERY_ANCHORS {
            break;
        }
        *attempts += 1;
        if let Ok(value) = serde_json::from_slice(&line[offset..]) {
            if copilot_envelope(&value) {
                values.push(value);
            }
        }
    }
}

fn copilot_envelope(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str).is_some()
        && value.get("data").is_some_and(Value::is_object)
}

/// Selects the parser appropriate for a bounded transcript window.
#[must_use]
pub fn parse_transcript_window(kind: TranscriptKind, bytes: &[u8]) -> ParsedLines {
    match kind {
        TranscriptKind::Copilot => parse_copilot_events(bytes),
        TranscriptKind::Claude
        | TranscriptKind::Pi
        | TranscriptKind::Codex
        | TranscriptKind::Unknown => parse_ndjson(bytes),
    }
}

/// Extracts a normalized visible conversation turn from one decoded record.
#[must_use]
pub fn extract_record(kind: TranscriptKind, value: &Value) -> Option<TranscriptRecord> {
    match kind {
        TranscriptKind::Claude | TranscriptKind::Pi => {
            if kind == TranscriptKind::Pi && string_at(value, &["type"]) != Some("message") {
                return None;
            }
            if kind == TranscriptKind::Claude && bool_at(value, &["isMeta"]) == Some(true) {
                return None;
            }
            let message = value.get("message")?.as_object()?;
            let role = message.get("role")?.as_str()?.to_owned();
            let text = flatten_content(message.get("content"))?;
            Some(TranscriptRecord {
                role: role.clone(),
                text: if role == "user" {
                    unwrap_command(&text)
                } else {
                    text
                },
            })
        }
        TranscriptKind::Codex => {
            if string_at(value, &["type"]) != Some("response_item") {
                return None;
            }
            let payload = value.get("payload")?.as_object()?;
            if payload.get("type")?.as_str()? != "message" {
                return None;
            }
            let role = payload.get("role")?.as_str()?.to_owned();
            Some(TranscriptRecord {
                role,
                text: flatten_content(payload.get("content"))?,
            })
        }
        TranscriptKind::Copilot => extract_copilot_record(value),
        TranscriptKind::Unknown => None,
    }
}

fn extract_copilot_record(value: &Value) -> Option<TranscriptRecord> {
    if !copilot_root_event(value) || !copilot_event_is_non_ephemeral(value) {
        return None;
    }
    let data = value.get("data")?.as_object()?;
    match value.get("type")?.as_str()? {
        "user.message" if source_is_user(data.get("source")) => Some(TranscriptRecord {
            role: "user".to_owned(),
            text: data.get("content")?.as_str()?.to_owned(),
        }),
        "assistant.message" if assistant_is_final(data) => Some(TranscriptRecord {
            role: "assistant".to_owned(),
            text: data.get("content")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

fn copilot_root_event(value: &Value) -> bool {
    value.get("agentId").is_none_or(Value::is_null)
}

/// Copilot's ephemeral marker is accepted only when omitted or explicitly false.
/// Schema drift must fail closed so unrecognized event visibility cannot leak into
/// transcript-derived fields.
#[must_use]
pub(crate) fn copilot_event_is_non_ephemeral(value: &Value) -> bool {
    copilot_ephemeral_is_false_or_absent(value.get("ephemeral"))
        && value
            .get("data")
            .and_then(Value::as_object)
            .is_some_and(|data| copilot_ephemeral_is_false_or_absent(data.get("ephemeral")))
}

fn copilot_ephemeral_is_false_or_absent(value: Option<&Value>) -> bool {
    value.is_none_or(|value| matches!(value, Value::Bool(false)))
}

fn source_is_user(source: Option<&Value>) -> bool {
    source.is_none_or(|source| source.as_str() == Some("user"))
}

fn assistant_is_final(data: &serde_json::Map<String, Value>) -> bool {
    data.get("parentToolCallId").is_none_or(Value::is_null)
        && data
            .get("toolRequests")
            .is_none_or(|requests| requests.as_array().is_some_and(Vec::is_empty))
}

/// Builds the opening and digest using bounded head/tail parsing only.
#[must_use]
pub fn analyze(
    kind: TranscriptKind,
    input: TranscriptInput<'_>,
    written_at: i64,
) -> TranscriptOutcome {
    if !kind.supports_enrichment() || matches!(input, TranscriptInput::Unavailable) {
        return TranscriptOutcome::Unavailable;
    }
    let TranscriptInput::Bytes(bytes) = input else {
        return TranscriptOutcome::NotYetCreated;
    };

    analyze_windows(
        kind,
        bounded_head(bytes, OPENING_HEAD_BYTES),
        TailRead {
            preceding_byte: if bytes.len() > DIGEST_TAIL_BYTES {
                Some(bytes[bytes.len() - DIGEST_TAIL_BYTES - 1])
            } else {
                None
            },
            bytes: if bytes.len() > DIGEST_TAIL_BYTES {
                &bytes[bytes.len() - DIGEST_TAIL_BYTES..]
            } else {
                bytes
            },
        },
        written_at,
    )
}

/// Analyzes independently-read bounded windows. A file adapter can obtain these with
/// one 256 KiB head read and one 1 MiB tail read, without loading a 66 MiB rollout.
/// `malformed_lines` and `decoded_records` describe the digest tail only, avoiding
/// double counting when head and tail windows overlap.
#[must_use]
pub fn analyze_windows(
    kind: TranscriptKind,
    opening_head: &[u8],
    digest_tail: TailRead<'_>,
    written_at: i64,
) -> TranscriptOutcome {
    if !kind.supports_enrichment() {
        return TranscriptOutcome::Unavailable;
    }
    let head = parse_transcript_window(kind, bounded_head(opening_head, OPENING_HEAD_BYTES));
    let tail = parse_transcript_window(kind, bounded_tail_read(digest_tail, DIGEST_TAIL_BYTES));
    if head.values.is_empty()
        && tail.values.is_empty()
        && (head.nonempty_lines > 0 || tail.nonempty_lines > 0)
    {
        return TranscriptOutcome::Malformed;
    }

    let opening = opening_from_values(kind, &head.values);
    let digest = digest_from_values(kind, &tail.values, opening.clone(), written_at);
    if opening.is_none() && digest.is_none() {
        return TranscriptOutcome::Empty;
    }
    TranscriptOutcome::Ready(Box::new(TranscriptAnalysis {
        opening,
        digest,
        malformed_lines: tail.malformed_lines,
        decoded_records: tail.values.len(),
    }))
}

#[must_use]
pub fn opening_from_values(kind: TranscriptKind, values: &[Value]) -> Option<String> {
    let mut social = None;
    for value in values {
        let Some(record) = extract_record(kind, value) else {
            continue;
        };
        if record.role != "user" {
            continue;
        }
        let text = record.text.trim();
        if !is_real_prompt(text) {
            continue;
        }
        if carries_intent(text) {
            return Some(clip_graphemes(text, OPENING_CLIP_GRAPHEMES));
        }
        if social.is_none() {
            social = Some(clip_graphemes(text, OPENING_CLIP_GRAPHEMES));
        }
    }
    social
}

#[must_use]
pub fn digest_from_values(
    kind: TranscriptKind,
    values_oldest_first: &[Value],
    opening: Option<String>,
    written_at: i64,
) -> Option<TranscriptDigest> {
    let mut users = Vec::new();
    let mut assistants = Vec::new();
    let mut social = Vec::new();
    for value in values_oldest_first.iter().rev() {
        let Some(record) = extract_record(kind, value) else {
            continue;
        };
        let text = record.text.trim();
        if grapheme_count(text) <= 2 {
            continue;
        }
        match record.role.as_str() {
            "user" if users.len() < USER_TURNS && is_real_prompt(text) => {
                if carries_intent(text) {
                    users.push(clip_graphemes(text, USER_CLIP_GRAPHEMES));
                } else if social.len() < 2 {
                    social.push(clip_graphemes(text, USER_CLIP_GRAPHEMES));
                }
            }
            "assistant" if assistants.len() < ASSISTANT_TURNS => {
                let limit = if assistants.is_empty() {
                    NEWEST_ASSISTANT_CLIP_GRAPHEMES
                } else {
                    USER_CLIP_GRAPHEMES
                };
                assistants.push(clip_graphemes(text, limit));
            }
            _ => {}
        }
        if users.len() >= USER_TURNS && assistants.len() >= ASSISTANT_TURNS {
            break;
        }
    }
    if users.is_empty() {
        users = social;
    }
    if users.is_empty() && assistants.is_empty() {
        return None;
    }

    let last_prompt = users.first().cloned().unwrap_or_default();
    let last_reply = assistants.first().cloned().unwrap_or_default();
    let recent = users
        .iter()
        .rev()
        .map(|turn| format!("USER: {turn}"))
        .chain(
            assistants
                .iter()
                .rev()
                .map(|turn| format!("ASSISTANT: {turn}")),
        )
        .collect::<Vec<_>>()
        .join("\n");
    let request_lines = users
        .iter()
        .rev()
        .map(|turn| format!("- {turn}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(TranscriptDigest {
        opening: opening
            .or_else(|| users.last().cloned())
            .unwrap_or_default(),
        requests: request_lines,
        recent,
        last_prompt_key: (!last_prompt.is_empty()).then(|| stable_key(&last_prompt)),
        last_reply_key: (!last_reply.is_empty()).then(|| stable_key(&last_reply)),
        last_prompt,
        last_reply,
        written_at,
    })
}

#[must_use]
pub fn unwrap_command(text: &str) -> String {
    let Some(open) = text.find("<command-args>") else {
        return text.to_owned();
    };
    if !text.contains("<command-name>") {
        return text.to_owned();
    }
    let after_open = open + "<command-args>".len();
    let Some(close_relative) = text[after_open..].find("</command-args>") else {
        return text.to_owned();
    };
    let args = text[after_open..after_open + close_relative].trim();
    if args.is_empty() {
        text.to_owned()
    } else {
        args.to_owned()
    }
}

#[must_use]
pub fn is_real_prompt(text: &str) -> bool {
    let text = text.trim();
    if grapheme_count(text) <= 8 || text.starts_with('<') || text.starts_with('{') {
        return false;
    }
    if text.starts_with("Base directory for this skill")
        || text.starts_with("# AGENTS.md instructions")
        || text.starts_with("## Mem0 context")
        || text.starts_with("This session is being continued from a previous conversation")
        || text.starts_with("[Request interrupted by user")
        || text.starts_with("[Image")
        || text.starts_with("[Screenshot")
        || (text.starts_with("http") && !text.contains(char::is_whitespace))
    {
        return false;
    }
    ![
        "system-reminder",
        "command-name",
        "local-command",
        "tool_use_error",
        "Caveat:",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

#[must_use]
pub fn carries_intent(text: &str) -> bool {
    let text = text.trim();
    if grapheme_count(text) > 40 {
        return true;
    }
    let normalized = text
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .to_lowercase();
    let words = normalized
        .split_whitespace()
        .filter(|word| {
            ![
                "claude", "codex", "pi", "there", "mate", "buddy", "again", "all", "cool",
                "please", "now", "then",
            ]
            .contains(word)
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        return false;
    }
    ![
        "hi",
        "hey",
        "hello",
        "yo",
        "gm",
        "morning",
        "afternoon",
        "evening",
        "hiya",
        "good morning",
        "good afternoon",
        "good evening",
        "good day",
        "good night",
        "howdy",
        "greetings",
        "welcome back",
        "how are you",
        "how are things",
        "thanks",
        "thank you",
        "thanks so much",
        "cheers",
        "ta",
        "much appreciated",
        "appreciated",
        "nice one",
        "perfect",
        "great",
        "excellent",
        "lovely",
        "brilliant",
        "awesome",
        "amazing",
        "wonderful",
        "sounds good",
        "looks good",
        "ok",
        "okay",
        "k",
        "sure",
        "yes",
        "yep",
        "yeah",
        "yup",
        "no",
        "nope",
        "nah",
        "right",
        "fine",
        "got it",
        "understood",
        "noted",
        "agreed",
        "indeed",
        "no worries",
        "no problem",
        "np",
        "never mind",
        "nvm",
        "carry on",
        "go ahead",
        "go on",
        "continue",
        "proceed",
        "keep going",
        "done",
        "ready",
        "bye",
        "goodbye",
        "see you",
        "later",
        "good stuff",
        "well done",
        "nice",
    ]
    .contains(&words.join(" ").as_str())
}

#[must_use]
pub fn stable_key(text: &str) -> String {
    stable_bytes_key(text.as_bytes())
}

#[must_use]
pub fn stable_bytes_key(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[must_use]
pub fn clip_graphemes(text: &str, max: usize) -> String {
    UnicodeSegmentation::graphemes(text, true)
        .take(max)
        .collect()
}

#[must_use]
pub fn grapheme_count(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true).count()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptCacheFingerprint {
    pub path: String,
    pub size: u64,
    pub modified_unix_seconds: i64,
    pub modified_nanoseconds: u32,
    pub content_sha256: Option<String>,
}

impl TranscriptCacheFingerprint {
    /// A metadata-only observation is deliberately not cacheable: same-size rewrites
    /// can preserve coarse mtime values on several filesystems.
    #[must_use]
    pub const fn is_cacheable(&self) -> bool {
        self.content_sha256.is_some()
    }
}

#[must_use]
pub fn cache_fingerprint(
    path: &str,
    size: u64,
    modified_unix_seconds: i64,
    modified_nanoseconds: u32,
    content: Option<&[u8]>,
) -> TranscriptCacheFingerprint {
    TranscriptCacheFingerprint {
        path: path.to_owned(),
        size,
        modified_unix_seconds,
        modified_nanoseconds,
        content_sha256: content.map(stable_bytes_key),
    }
}

#[must_use]
pub fn claude_relative_path(cwd: &str, session_uuid: &str) -> Option<SafeRelativePath> {
    let slug = safe_cwd_slug(cwd)?;
    let session_id = valid_opaque_session_id(session_uuid).then_some(session_uuid)?;
    SafeRelativePath::new(&format!("{slug}/{session_id}.jsonl"))
}

/// Copilot persists a session below an adapter-selected session-state root.
/// Only a Herdr-reported opaque `id` is accepted; never a session path.
#[must_use]
pub fn copilot_relative_path(session: &HerdrAgentSession) -> Option<SafeRelativePath> {
    (session.agent.eq_ignore_ascii_case("copilot")
        && session.kind == "id"
        && valid_opaque_session_id(&session.value))
    .then(|| SafeRelativePath::new(&format!("{}/events.jsonl", session.value)))
    .flatten()
}

#[must_use]
pub fn pi_exact_path(session_kind: &str, value: &str) -> Option<TrustedPiPath> {
    (session_kind == "path" && valid_trusted_pi_path(value))
        .then(|| TrustedPiPath(value.to_owned()))
}

/// A pure resolution request. Adapters supply roots/listings and perform existence
/// checks; core never reads home directories or recursively walks session roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptLocationPlan {
    Unavailable,
    ClaudeRelative(SafeRelativePath),
    CopilotRelative(SafeRelativePath),
    PiExact(TrustedPiPath),
    Codex(CodexLocatorPlan),
}

#[must_use]
pub fn location_plan(
    kind: TranscriptKind,
    session: Option<&HerdrAgentSession>,
    cwd: &str,
    codex_sessions_root: &str,
    codex_max_candidates: usize,
) -> TranscriptLocationPlan {
    let Some(session) = session.filter(|session| !session.value.is_empty()) else {
        return TranscriptLocationPlan::Unavailable;
    };
    match kind {
        TranscriptKind::Claude if session.kind == "id" => claude_relative_path(cwd, &session.value)
            .map_or(
                TranscriptLocationPlan::Unavailable,
                TranscriptLocationPlan::ClaudeRelative,
            ),
        TranscriptKind::Pi => pi_exact_path(&session.kind, &session.value).map_or(
            TranscriptLocationPlan::Unavailable,
            TranscriptLocationPlan::PiExact,
        ),
        TranscriptKind::Codex
            if session.kind == "id" && valid_opaque_session_id(&session.value) =>
        {
            TranscriptLocationPlan::Codex(CodexLocatorPlan {
                sessions_root: codex_sessions_root.to_owned(),
                session_uuid: session.value.clone(),
                max_candidates: codex_max_candidates,
            })
        }
        TranscriptKind::Copilot => copilot_relative_path(session).map_or(
            TranscriptLocationPlan::Unavailable,
            TranscriptLocationPlan::CopilotRelative,
        ),
        TranscriptKind::Claude | TranscriptKind::Codex | TranscriptKind::Unknown => {
            TranscriptLocationPlan::Unavailable
        }
    }
}

/// A validated descendant-relative path. An adapter must join it beneath its chosen
/// root and must not treat it as an absolute path on any host platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeRelativePath(String);

impl SafeRelativePath {
    #[must_use]
    pub fn new(value: &str) -> Option<Self> {
        let value = value.replace('\\', "/");
        if value.is_empty()
            || value.starts_with('/')
            || value.contains('\0')
            || value.chars().any(char::is_control)
        {
            return None;
        }
        let components = value.split('/').collect::<Vec<_>>();
        if components.is_empty()
            || components.iter().any(|component| {
                component.is_empty()
                    || *component == "."
                    || *component == ".."
                    || component.contains(':')
            })
        {
            return None;
        }
        Some(Self(components.join("/")))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An absolute, clean path explicitly supplied by Herdr for Pi. It is not a safe
/// relative descendant; adapters must preserve its distinct trusted-path handling
/// rather than joining it to a transcript root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedPiPath(String);

impl TrustedPiPath {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Accepts only opaque ID components, never a filesystem path. Herdr UUID-like IDs
/// are ASCII alphanumeric with hyphen/underscore, so this remains conservative.
#[must_use]
pub fn valid_opaque_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_trusted_pi_path(value: &str) -> bool {
    if value.is_empty()
        || value.trim() != value
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return false;
    }
    let bytes = value.as_bytes();
    let windows_drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    let windows_unc_absolute = value.starts_with("\\\\");
    let unix_absolute = value.starts_with('/');
    (unix_absolute || windows_drive_absolute || windows_unc_absolute)
        && value
            .split(['/', '\\'])
            .all(|component| !matches!(component, "." | ".."))
}

fn safe_cwd_slug(cwd: &str) -> Option<String> {
    if cwd.trim().is_empty() {
        return None;
    }
    let slug = cwd
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    (!slug.is_empty()).then_some(slug)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexLocatorPlan {
    pub sessions_root: String,
    pub session_uuid: String,
    pub max_candidates: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexLocateError {
    CandidateLimitExceeded,
}

/// Selects a matching *relative descendant* from an adapter-provided bounded listing.
/// The adapter must canonicalize candidates and strip/verify the configured sessions
/// root before calling this function; raw absolute paths are intentionally rejected.
pub fn select_codex_candidate(
    plan: &CodexLocatorPlan,
    candidates: &[String],
) -> Result<Option<SafeRelativePath>, CodexLocateError> {
    if candidates.len() > plan.max_candidates {
        return Err(CodexLocateError::CandidateLimitExceeded);
    }
    let suffix = format!("{}.jsonl", plan.session_uuid);
    Ok(candidates
        .iter()
        .filter(|candidate| candidate.ends_with(&suffix))
        .find_map(|candidate| SafeRelativePath::new(candidate)))
}

fn flatten_content(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_str)
}

fn bool_at(value: &Value, path: &[&str]) -> Option<bool> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_bool)
}
