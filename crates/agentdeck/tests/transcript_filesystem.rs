use std::{
    fs::{self, OpenOptions},
    io::{Seek as _, SeekFrom, Write as _},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
};

use agentdeck::adapters::transcripts::{
    BlockingTranscriptIo, BlockingTranscriptRead, CodexScanLimits, FileTimestamp,
    FilesystemTranscriptSource, StdTranscriptIo, TranscriptAdapterLimits, TranscriptIoError,
    TranscriptRequest, TranscriptRoots, TranscriptSource, TranscriptSourceBuildError,
    TranscriptWindows,
};
use agentdeck_core::{
    HerdrAgentSession,
    context::ContextOutcome,
    transcript::{
        CONTEXT_TAIL_BYTES, OPENING_HEAD_BYTES, TranscriptKind, TranscriptLocationPlan,
        TranscriptOutcome, location_plan, pi_exact_path,
    },
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Notify;

fn roots(temp: &TempDir) -> TranscriptRoots {
    let claude_projects_root = temp.path().join("claude-projects");
    let codex_sessions_root = temp.path().join("codex-sessions");
    let copilot_session_state_root = temp.path().join("copilot-session-state");
    fs::create_dir_all(&claude_projects_root)
        .unwrap_or_else(|error| panic!("create Claude root: {error}"));
    fs::create_dir_all(&codex_sessions_root)
        .unwrap_or_else(|error| panic!("create Codex root: {error}"));
    fs::create_dir_all(&copilot_session_state_root)
        .unwrap_or_else(|error| panic!("create Copilot root: {error}"));
    TranscriptRoots {
        claude_projects_root,
        codex_sessions_root,
        copilot_session_state_root,
    }
}

fn session(kind: &str, value: &str) -> HerdrAgentSession {
    HerdrAgentSession {
        source: "fixture".to_owned(),
        agent: "fixture".to_owned(),
        kind: kind.to_owned(),
        value: value.to_owned(),
    }
}

fn request(kind: TranscriptKind, session: HerdrAgentSession, cwd: &str) -> TranscriptRequest {
    TranscriptRequest {
        kind,
        session: Some(session),
        cwd: cwd.to_owned(),
    }
}

fn claude_line(role: &str, content: &str) -> String {
    serde_json::to_string(&json!({"message": {"role": role, "content": content}}))
        .unwrap_or_else(|error| panic!("serialize fixture: {error}"))
}

fn claude_content() -> String {
    [
        claude_line("user", "inspect the bounded transcript adapter now"),
        claude_line("assistant", "adapter reply with enough useful detail"),
        serde_json::to_string(&json!({"message": {"usage": {"input_tokens": 120}}}))
            .unwrap_or_else(|error| panic!("serialize usage fixture: {error}")),
    ]
    .join("\n")
}

fn pi_content() -> String {
    [
        json!({"type": "message", "message": {
            "role": "user", "content": "inspect the explicit Pi transcript path"
        }}),
        json!({"type": "message", "message": {
            "role": "assistant", "content": "Pi reply from its explicit transcript"
        }}),
    ]
    .into_iter()
    .map(|value| {
        serde_json::to_string(&value)
            .unwrap_or_else(|error| panic!("serialize Pi fixture: {error}"))
    })
    .collect::<Vec<_>>()
    .join("\n")
}

fn codex_line(role: &str, text: &str) -> String {
    serde_json::to_string(&json!({
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": role,
            "content": [{"type": "input_text", "text": text}]
        }
    }))
    .unwrap_or_else(|error| panic!("serialize Codex fixture: {error}"))
}

fn copilot_line(event: &str, data: serde_json::Value) -> String {
    serde_json::to_string(&json!({"type": event, "data": data}))
        .unwrap_or_else(|error| panic!("serialize Copilot fixture: {error}"))
}

fn write_claude(roots: &TranscriptRoots, cwd_slug: &str, id: &str, content: &[u8]) -> PathBuf {
    let directory = roots.claude_projects_root.join(cwd_slug);
    fs::create_dir_all(&directory).unwrap_or_else(|error| panic!("create Claude project: {error}"));
    let path = directory.join(format!("{id}.jsonl"));
    fs::write(&path, content).unwrap_or_else(|error| panic!("write Claude transcript: {error}"));
    path
}

#[tokio::test]
async fn source_distinguishes_unavailable_not_yet_malformed_empty_and_ready() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let roots = roots(&temp);
    let source = FilesystemTranscriptSource::new(roots.clone())
        .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let missing = request(TranscriptKind::Claude, session("id", "missing"), "/repo");
    let missing_result = source.observe(missing).await;
    assert!(matches!(
        missing_result.analysis,
        TranscriptOutcome::NotYetCreated
    ));
    assert!(matches!(
        missing_result.context,
        ContextOutcome::NotYetCreated
    ));

    let unsupported = source
        .observe(request(
            TranscriptKind::Copilot,
            session("id", "ignored"),
            "/repo",
        ))
        .await;
    assert!(matches!(
        unsupported.analysis,
        TranscriptOutcome::Unavailable
    ));
    assert!(matches!(unsupported.context, ContextOutcome::Unavailable));
    let unknown = source
        .observe(request(
            TranscriptKind::Unknown,
            session("id", "ignored"),
            "/repo",
        ))
        .await;
    assert!(matches!(unknown.analysis, TranscriptOutcome::Unavailable));

    write_claude(&roots, "-repo", "empty", b"");
    let empty = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "empty"),
            "/repo",
        ))
        .await;
    assert!(matches!(empty.analysis, TranscriptOutcome::Empty));
    assert!(matches!(empty.context, ContextOutcome::Empty));
    assert!(empty.written_at.is_some());

    write_claude(&roots, "-repo", "bad", b"{bad\n");
    let malformed = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "bad"),
            "/repo",
        ))
        .await;
    assert!(matches!(malformed.analysis, TranscriptOutcome::Malformed));
    assert!(matches!(malformed.context, ContextOutcome::Malformed));

    let mixed = format!("{{bad\r\n{}", claude_content().replace('\n', "\r\n"));
    write_claude(&roots, "-repo", "mixed", mixed.as_bytes());
    let isolated_malformed = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "mixed"),
            "/repo",
        ))
        .await;
    assert!(matches!(
        isolated_malformed.analysis,
        TranscriptOutcome::Ready(_)
    ));
    assert_eq!(
        isolated_malformed
            .context_usage()
            .map(|context| context.used),
        Some(120)
    );

    write_claude(&roots, "-repo", "ready", claude_content().as_bytes());
    let ready = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "ready"),
            "/repo",
        ))
        .await;
    assert!(matches!(ready.analysis, TranscriptOutcome::Ready(_)));
    assert_eq!(ready.context_usage().map(|context| context.used), Some(120));
    assert!(ready.reply_key().is_some());
    assert!(ready.written_at.is_some());
}

#[tokio::test]
async fn pi_uses_only_herdrs_absolute_path_and_never_joins_a_configured_root() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let roots = roots(&temp);
    let pi_path = temp.path().join("pi.jsonl");
    fs::write(&pi_path, pi_content())
        .unwrap_or_else(|error| panic!("write Pi transcript: {error}"));
    let source = FilesystemTranscriptSource::new(roots)
        .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let accepted = source
        .observe(request(
            TranscriptKind::Pi,
            session("path", &pi_path.to_string_lossy()),
            "",
        ))
        .await;
    assert!(matches!(accepted.analysis, TranscriptOutcome::Ready(_)));
    let wrong_kind = source
        .observe(request(
            TranscriptKind::Pi,
            session("id", &pi_path.to_string_lossy()),
            "",
        ))
        .await;
    assert!(matches!(
        wrong_kind.analysis,
        TranscriptOutcome::Unavailable
    ));
    let relative = source
        .observe(request(
            TranscriptKind::Pi,
            session("path", "relative.jsonl"),
            "",
        ))
        .await;
    assert!(matches!(relative.analysis, TranscriptOutcome::Unavailable));
}

#[cfg(unix)]
#[tokio::test]
async fn pi_rejects_device_and_directory_paths_without_reading_them() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let source = FilesystemTranscriptSource::new(roots(&temp))
        .unwrap_or_else(|error| panic!("build source: {error:?}"));
    for path in [PathBuf::from("/dev/null"), temp.path().to_path_buf()] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            source.observe(request(
                TranscriptKind::Pi,
                session("path", &path.to_string_lossy()),
                "",
            )),
        )
        .await
        .unwrap_or_else(|error| panic!("special file check blocked: {error}"));
        assert!(matches!(result.analysis, TranscriptOutcome::Unavailable));
    }
}

#[tokio::test]
async fn absent_roots_are_not_yet_created_but_a_root_file_is_unavailable() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let absent = FilesystemTranscriptSource::new(TranscriptRoots {
        claude_projects_root: temp.path().join("absent-projects"),
        codex_sessions_root: temp.path().join("absent-sessions"),
        copilot_session_state_root: temp.path().join("absent-copilot-session-state"),
    })
    .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let request = request(TranscriptKind::Claude, session("id", "missing"), "/repo");
    let missing = absent.observe(request.clone()).await;
    assert!(matches!(missing.analysis, TranscriptOutcome::NotYetCreated));

    let root_file = temp.path().join("not-a-directory");
    fs::write(&root_file, b"not a transcript root")
        .unwrap_or_else(|error| panic!("write root file: {error}"));
    let invalid_root = FilesystemTranscriptSource::new(TranscriptRoots {
        claude_projects_root: root_file,
        codex_sessions_root: temp.path().join("unused-sessions"),
        copilot_session_state_root: temp.path().join("unused-copilot-session-state"),
    })
    .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let unavailable = invalid_root.observe(request).await;
    assert!(matches!(
        unavailable.analysis,
        TranscriptOutcome::Unavailable
    ));
}

#[tokio::test]
async fn every_supported_agent_recovers_when_a_missing_transcript_is_created() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let roots = roots(&temp);
    let pi_path = temp.path().join("late-pi.jsonl");
    let source = FilesystemTranscriptSource::new(roots.clone())
        .unwrap_or_else(|error| panic!("build source: {error:?}"));

    let claude_request = request(
        TranscriptKind::Claude,
        session("id", "late-claude"),
        "/repo",
    );
    let pi_request = request(
        TranscriptKind::Pi,
        session("path", &pi_path.to_string_lossy()),
        "",
    );
    let codex_request = request(TranscriptKind::Codex, session("id", "late-codex"), "");

    for missing in [
        source.observe(claude_request.clone()).await,
        source.observe(pi_request.clone()).await,
        source.observe(codex_request.clone()).await,
    ] {
        assert!(matches!(missing.analysis, TranscriptOutcome::NotYetCreated));
        assert!(matches!(missing.context, ContextOutcome::NotYetCreated));
    }

    write_claude(&roots, "-repo", "late-claude", claude_content().as_bytes());
    fs::write(&pi_path, pi_content()).unwrap_or_else(|error| panic!("write late Pi: {error}"));
    let codex_path = roots
        .codex_sessions_root
        .join("2026/08/22/rollout-late-codex.jsonl");
    fs::create_dir_all(
        codex_path
            .parent()
            .unwrap_or_else(|| panic!("Codex parent")),
    )
    .unwrap_or_else(|error| panic!("create Codex date tree: {error}"));
    fs::write(
        codex_path,
        serde_json::to_vec(&json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "late Codex request"}]
            }
        }))
        .unwrap_or_else(|error| panic!("serialize Codex fixture: {error}")),
    )
    .unwrap_or_else(|error| panic!("write late Codex: {error}"));

    for created in [
        source.observe(claude_request).await,
        source.observe(pi_request).await,
        source.observe(codex_request).await,
    ] {
        assert!(matches!(created.analysis, TranscriptOutcome::Ready(_)));
    }
}

#[tokio::test]
async fn adapter_lossily_recovers_valid_json_containing_invalid_utf8() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let roots = roots(&temp);
    let mut bytes = br#"{"message":{"role":"user","content":"recover "#.to_vec();
    bytes.push(0xff);
    bytes.extend_from_slice(b" request after invalid utf8\"}}\n");
    write_claude(&roots, "-repo", "lossy", &bytes);
    let source = FilesystemTranscriptSource::new(roots)
        .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let observation = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "lossy"),
            "/repo",
        ))
        .await;
    assert!(matches!(observation.analysis, TranscriptOutcome::Ready(_)));
}

#[tokio::test]
async fn reads_final_unterminated_record_after_a_partial_context_tail_line() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let roots = roots(&temp);
    let valid = claude_line("user", "the final unterminated request stays visible");
    let mut bytes = vec![b'x'; CONTEXT_TAIL_BYTES + 64];
    bytes.push(b'\n');
    bytes.extend_from_slice(valid.as_bytes());
    write_claude(&roots, "-repo", "tail", &bytes);
    let source = FilesystemTranscriptSource::new(roots)
        .unwrap_or_else(|error| panic!("build source: {error:?}"));

    let observation = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "tail"),
            "/repo",
        ))
        .await;
    let TranscriptOutcome::Ready(analysis) = observation.analysis else {
        panic!("bounded tail must parse the final record")
    };
    assert_eq!(
        analysis
            .digest
            .as_ref()
            .map(|digest| digest.last_prompt.as_str()),
        Some("the final unterminated request stays visible")
    );
}

#[tokio::test]
async fn every_agent_parser_handles_malformed_crlf_invalid_utf8_and_partial_unterminated_tails() {
    let cases = [
        (
            TranscriptKind::Claude,
            [
                claude_line("user", "first Claude request"),
                claude_line("assistant", "final Claude reply"),
            ]
            .join("\r\n"),
            claude_line("user", "unterminated Claude tail request"),
        ),
        (
            TranscriptKind::Pi,
            [
                serde_json::to_string(&json!({"type":"message","message":{"role":"user","content":"first Pi request"}}))
                    .unwrap_or_else(|error| panic!("serialize Pi: {error}")),
                serde_json::to_string(&json!({"type":"message","message":{"role":"assistant","content":"final Pi reply"}}))
                    .unwrap_or_else(|error| panic!("serialize Pi: {error}")),
            ]
            .join("\r\n"),
            serde_json::to_string(&json!({"type":"message","message":{"role":"user","content":"unterminated Pi tail request"}}))
                .unwrap_or_else(|error| panic!("serialize Pi tail: {error}")),
        ),
        (
            TranscriptKind::Codex,
            [
                codex_line("user", "first Codex request"),
                codex_line("assistant", "final Codex reply"),
            ]
            .join("\r\n"),
            codex_line("user", "unterminated Codex tail request"),
        ),
        (
            TranscriptKind::Copilot,
            [
                copilot_line(
                    "user.message",
                    json!({"content": "first Copilot request", "source": "user"}),
                ),
                copilot_line(
                    "assistant.message",
                    json!({"content": "final Copilot reply", "toolRequests": []}),
                ),
            ]
            .join("\r\n"),
            copilot_line(
                "user.message",
                json!({"content": "unterminated Copilot tail request", "source": "user"}),
            ),
        ),
    ];

    for (kind, valid_crlf, unterminated) in cases {
        let root = PathBuf::from(format!("/fixture/{kind:?}"));
        let io = MutableIo::new(root.clone(), make_windows(&valid_crlf));
        let source = FilesystemTranscriptSource::with_io(
            TranscriptRoots {
                claude_projects_root: root,
                codex_sessions_root: PathBuf::from("/fixture/codex"),
                copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
            },
            io.clone(),
            TranscriptAdapterLimits::default(),
        )
        .unwrap_or_else(|error| panic!("build source: {error:?}"));
        let request = parser_request(kind);

        let crlf = source.observe(request.clone()).await;
        assert!(matches!(crlf.analysis, TranscriptOutcome::Ready(_)));

        io.replace(make_windows("{malformed\r\n"));
        let malformed = source.observe(request.clone()).await;
        assert!(matches!(malformed.analysis, TranscriptOutcome::Malformed));
        assert!(matches!(malformed.context, ContextOutcome::Malformed));

        let mut invalid_utf8 = valid_crlf.into_bytes();
        let replacement = invalid_utf8
            .iter()
            .position(|byte| *byte == b'f')
            .unwrap_or_else(|| panic!("fixture contains replaceable text"));
        invalid_utf8[replacement] = 0xff;
        io.replace(make_windows_bytes(&invalid_utf8));
        let lossy = source.observe(request.clone()).await;
        assert!(matches!(lossy.analysis, TranscriptOutcome::Ready(_)));

        io.replace(make_partial_tail_windows(unterminated.as_bytes()));
        let partial = source.observe(request).await;
        assert!(matches!(partial.analysis, TranscriptOutcome::Ready(_)));
    }
}

#[test]
fn standard_reader_uses_descriptor_bounded_bytes_for_a_sparse_66_mib_file() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let path = temp.path().join("large.jsonl");
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|error| panic!("create sparse fixture: {error}"));
    let final_line = claude_line("user", "final bounded request in a sparse rollout");
    let size = 66_u64 * 1024 * 1024;
    file.set_len(size)
        .unwrap_or_else(|error| panic!("set sparse length: {error}"));
    let offset = size
        .checked_sub(u64::try_from(final_line.len() + 1).unwrap_or(u64::MAX))
        .unwrap_or_else(|| panic!("fixture offset"));
    file.seek(SeekFrom::Start(offset))
        .unwrap_or_else(|error| panic!("seek sparse tail: {error}"));
    file.write_all(b"\n")
        .unwrap_or_else(|error| panic!("write tail separator: {error}"));
    file.write_all(final_line.as_bytes())
        .unwrap_or_else(|error| panic!("write final record: {error}"));
    file.sync_all()
        .unwrap_or_else(|error| panic!("sync sparse fixture: {error}"));

    let plan = pi_exact_path("path", &path.to_string_lossy())
        .map(TranscriptLocationPlan::PiExact)
        .unwrap_or_else(|| panic!("absolute Pi fixture plan"));
    let read = StdTranscriptIo
        .read_plan(
            &TranscriptRoots {
                claude_projects_root: temp.path().join("unused-claude"),
                codex_sessions_root: temp.path().join("unused-codex"),
                copilot_session_state_root: temp.path().join("unused-copilot-session-state"),
            },
            &plan,
            None,
            CodexScanLimits::default(),
        )
        .unwrap_or_else(|error| panic!("bounded read: {error:?}"));
    let windows = read.windows();
    assert_eq!(windows.size(), size);
    assert!(
        windows.bytes_read() <= OPENING_HEAD_BYTES + CONTEXT_TAIL_BYTES + 1,
        "read {} bytes from a {} byte file",
        windows.bytes_read(),
        size
    );
    assert!(
        windows
            .context_tail()
            .bytes
            .ends_with(final_line.as_bytes())
    );
}

#[tokio::test]
async fn codex_scan_is_date_bounded_revalidates_cache_and_never_uses_a_missing_candidate() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let roots = roots(&temp);
    let rollout = roots
        .codex_sessions_root
        .join("2026")
        .join("08")
        .join("22")
        .join("rollout-fixture-id.jsonl");
    fs::create_dir_all(rollout.parent().unwrap_or_else(|| panic!("rollout parent")))
        .unwrap_or_else(|error| panic!("create Codex date tree: {error}"));
    fs::write(
        &rollout,
        serde_json::to_vec(&json!({
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {
                "last_token_usage": {"total_tokens": 50_000},
                "model_context_window": 100_000
            }}
        }))
        .unwrap_or_else(|error| panic!("serialize Codex fixture: {error}")),
    )
    .unwrap_or_else(|error| panic!("write Codex fixture: {error}"));
    let source = FilesystemTranscriptSource::new(roots)
        .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let transcript_request = request(TranscriptKind::Codex, session("id", "fixture-id"), "");
    let first = source.observe(transcript_request.clone()).await;
    assert_eq!(
        first.context_usage().map(|context| context.limit),
        Some(100_000)
    );

    fs::remove_file(&rollout).unwrap_or_else(|error| panic!("remove cached candidate: {error}"));
    let second = source.observe(transcript_request).await;
    assert!(matches!(second.analysis, TranscriptOutcome::NotYetCreated));
    assert!(matches!(second.context, ContextOutcome::NotYetCreated));
}

#[test]
fn codex_scanner_stops_at_directory_entry_and_depth_caps() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let root = temp.path().join("sessions");
    fs::create_dir_all(root.join("2026").join("08").join("22"))
        .unwrap_or_else(|error| panic!("create valid date tree: {error}"));
    fs::create_dir_all(root.join("2025"))
        .unwrap_or_else(|error| panic!("create second date tree: {error}"));
    let roots = TranscriptRoots {
        claude_projects_root: temp.path().join("unused-claude"),
        codex_sessions_root: root.clone(),
        copilot_session_state_root: temp.path().join("unused-copilot-session-state"),
    };
    let limits = CodexScanLimits {
        max_candidates: 2,
        max_directories: 8,
        max_entries_per_directory: 1,
        max_depth: 3,
    };
    assert_eq!(
        StdTranscriptIo.read_plan(
            &roots,
            &location_plan(
                TranscriptKind::Codex,
                Some(&session("id", "fixture")),
                "",
                root.to_str().unwrap_or_default(),
                limits.max_candidates
            ),
            None,
            limits,
        ),
        Err(TranscriptIoError::BoundsExceeded)
    );

    let too_deep = root
        .join("2026")
        .join("08")
        .join("22")
        .join("extra")
        .join("rollout-fixture.jsonl");
    fs::create_dir_all(
        too_deep
            .parent()
            .unwrap_or_else(|| panic!("deep fixture parent")),
    )
    .unwrap_or_else(|error| panic!("create deep fixture: {error}"));
    fs::write(&too_deep, b"{}").unwrap_or_else(|error| panic!("write deep fixture: {error}"));
    let generous = CodexScanLimits {
        max_candidates: 2,
        max_directories: 16,
        max_entries_per_directory: 16,
        max_depth: 3,
    };
    assert!(matches!(
        StdTranscriptIo.read_plan(
            &roots,
            &location_plan(
                TranscriptKind::Codex,
                Some(&session("id", "fixture")),
                "",
                root.to_str().unwrap_or_default(),
                generous.max_candidates
            ),
            None,
            generous,
        ),
        Err(TranscriptIoError::NotFound)
    ));
}

#[test]
fn codex_scanner_enforces_exact_candidate_directory_and_entry_boundaries() {
    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let root = temp.path().join("sessions");
    let candidate = root.join("2026/08/22/rollout-exact.jsonl");
    fs::create_dir_all(
        candidate
            .parent()
            .unwrap_or_else(|| panic!("candidate parent")),
    )
    .unwrap_or_else(|error| panic!("create candidate tree: {error}"));
    fs::write(&candidate, codex_line("user", "exact cap request"))
        .unwrap_or_else(|error| panic!("write candidate: {error}"));
    let roots = TranscriptRoots {
        claude_projects_root: temp.path().join("unused-claude"),
        codex_sessions_root: root.clone(),
        copilot_session_state_root: temp.path().join("unused-copilot-session-state"),
    };
    let codex_session = session("id", "exact");
    let plan = location_plan(
        TranscriptKind::Codex,
        Some(&codex_session),
        "",
        root.to_str().unwrap_or_default(),
        1,
    );
    let exact = CodexScanLimits {
        max_candidates: 1,
        max_directories: 4,
        max_entries_per_directory: 1,
        max_depth: 3,
    };
    assert!(
        StdTranscriptIo
            .read_plan(&roots, &plan, None, exact)
            .is_ok()
    );

    for too_small in [
        CodexScanLimits {
            max_candidates: 0,
            ..exact
        },
        CodexScanLimits {
            max_directories: 3,
            ..exact
        },
        CodexScanLimits {
            max_entries_per_directory: 0,
            ..exact
        },
    ] {
        assert_eq!(
            StdTranscriptIo.read_plan(&roots, &plan, None, too_small),
            Err(TranscriptIoError::BoundsExceeded)
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn claude_pi_and_codex_symlinks_are_unavailable_or_ignored() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap_or_else(|error| panic!("temp root: {error}"));
    let roots = roots(&temp);
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap_or_else(|error| panic!("create outside root: {error}"));
    fs::write(outside.join("escape.jsonl"), claude_content())
        .unwrap_or_else(|error| panic!("write outside transcript: {error}"));
    symlink(&outside, roots.claude_projects_root.join("-repo"))
        .unwrap_or_else(|error| panic!("link Claude escape: {error}"));
    let source = FilesystemTranscriptSource::new(roots.clone())
        .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let escaped = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "escape"),
            "/repo",
        ))
        .await;
    assert!(matches!(escaped.analysis, TranscriptOutcome::Unavailable));

    let pi_link = temp.path().join("pi-link.jsonl");
    symlink(outside.join("escape.jsonl"), &pi_link)
        .unwrap_or_else(|error| panic!("link Pi transcript: {error}"));
    let pi = source
        .observe(request(
            TranscriptKind::Pi,
            session("path", &pi_link.to_string_lossy()),
            "",
        ))
        .await;
    assert!(matches!(pi.analysis, TranscriptOutcome::Unavailable));

    let date_link = roots.codex_sessions_root.join("2026");
    symlink(&outside, &date_link).unwrap_or_else(|error| panic!("link Codex date dir: {error}"));
    let codex = source
        .observe(request(TranscriptKind::Codex, session("id", "escape"), ""))
        .await;
    assert!(matches!(codex.analysis, TranscriptOutcome::NotYetCreated));

    fs::remove_file(&date_link).unwrap_or_else(|error| panic!("remove Codex date link: {error}"));
    let cached_path = roots
        .codex_sessions_root
        .join("2026/08/22/rollout-cached.jsonl");
    fs::create_dir_all(
        cached_path
            .parent()
            .unwrap_or_else(|| panic!("cached Codex parent")),
    )
    .unwrap_or_else(|error| panic!("create cached Codex tree: {error}"));
    fs::write(&cached_path, codex_line("user", "safe cached request"))
        .unwrap_or_else(|error| panic!("write cached Codex: {error}"));
    let cached_request = request(TranscriptKind::Codex, session("id", "cached"), "");
    let cached = source.observe(cached_request.clone()).await;
    assert!(matches!(cached.analysis, TranscriptOutcome::Ready(_)));
    fs::remove_file(&cached_path).unwrap_or_else(|error| panic!("remove cached Codex: {error}"));
    symlink(outside.join("escape.jsonl"), &cached_path)
        .unwrap_or_else(|error| panic!("replace cached Codex with symlink: {error}"));
    let replaced = source.observe(cached_request).await;
    assert!(matches!(
        replaced.analysis,
        TranscriptOutcome::NotYetCreated
    ));
}

#[tokio::test]
async fn cache_fingerprint_reparses_same_metadata_when_bounded_raw_content_changes() {
    let root = PathBuf::from("/fixture/claude-projects");
    let first =
        make_windows(&[claude_line("assistant", "first stable assistant reply")].join("\n"));
    let io = MutableIo::new(root.clone(), first);
    let source = FilesystemTranscriptSource::with_io(
        TranscriptRoots {
            claude_projects_root: root.clone(),
            codex_sessions_root: PathBuf::from("/fixture/codex-sessions"),
            copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
        },
        io.clone(),
        TranscriptAdapterLimits::default(),
    )
    .unwrap_or_else(|error| panic!("build source: {error:?}"));
    let transcript_request = request(TranscriptKind::Claude, session("id", "cache"), "/repo");
    let first = source.observe(transcript_request.clone()).await;
    io.replace(make_windows(
        &[claude_line("assistant", "second stable assistant reply")].join("\n"),
    ));
    let second = source.observe(transcript_request).await;

    assert_ne!(first.reply_key(), second.reply_key());
    assert_eq!(io.reads(), 2);
}

#[tokio::test]
async fn source_rejects_zero_and_panicking_semaphore_sizes_without_panicking() {
    let root = PathBuf::from("/fixture/claude-projects");
    let io = MutableIo::new(root.clone(), make_windows(&pi_content()));
    for read_concurrency in [0, tokio::sync::Semaphore::MAX_PERMITS.saturating_add(1)] {
        let result = FilesystemTranscriptSource::with_io(
            TranscriptRoots {
                claude_projects_root: root.clone(),
                codex_sessions_root: PathBuf::from("/fixture/codex-sessions"),
                copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
            },
            io.clone(),
            TranscriptAdapterLimits {
                read_concurrency,
                ..TranscriptAdapterLimits::default()
            },
        );
        assert!(matches!(
            result,
            Err(TranscriptSourceBuildError::InvalidReadConcurrency)
        ));
    }

    let invalid_cache = FilesystemTranscriptSource::with_io(
        TranscriptRoots {
            claude_projects_root: root.clone(),
            codex_sessions_root: PathBuf::from("/fixture/codex-sessions"),
            copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
        },
        io.clone(),
        TranscriptAdapterLimits {
            cache_entries: 4_097,
            ..TranscriptAdapterLimits::default()
        },
    );
    assert!(matches!(
        invalid_cache,
        Err(TranscriptSourceBuildError::InvalidCacheEntries)
    ));

    for codex_scan in [
        CodexScanLimits {
            max_candidates: 4_097,
            ..CodexScanLimits::default()
        },
        CodexScanLimits {
            max_depth: 4,
            ..CodexScanLimits::default()
        },
    ] {
        let result = FilesystemTranscriptSource::with_io(
            TranscriptRoots {
                claude_projects_root: root.clone(),
                codex_sessions_root: PathBuf::from("/fixture/codex-sessions"),
                copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
            },
            io.clone(),
            TranscriptAdapterLimits {
                codex_scan,
                ..TranscriptAdapterLimits::default()
            },
        );
        assert!(matches!(
            result,
            Err(TranscriptSourceBuildError::InvalidCodexScanLimits)
        ));
    }
}

#[tokio::test]
async fn request_path_fields_accept_exactly_4096_bytes_and_reject_cap_plus_one() {
    let root = PathBuf::from("/fixture/claude-projects");
    let io = MutableIo::new(root.clone(), make_windows(&pi_content()));
    let source = FilesystemTranscriptSource::with_io(
        TranscriptRoots {
            claude_projects_root: root,
            codex_sessions_root: PathBuf::from("/fixture/codex-sessions"),
            copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
        },
        io.clone(),
        TranscriptAdapterLimits::default(),
    )
    .unwrap_or_else(|error| panic!("build source: {error:?}"));

    let exact_pi = format!("/{}", "a".repeat(4_095));
    let oversized_pi = format!("/{}", "a".repeat(4_096));
    let exact = source
        .observe(request(TranscriptKind::Pi, session("path", &exact_pi), ""))
        .await;
    assert!(matches!(exact.analysis, TranscriptOutcome::Ready(_)));
    let oversized = source
        .observe(request(
            TranscriptKind::Pi,
            session("path", &oversized_pi),
            "",
        ))
        .await;
    assert!(matches!(oversized.analysis, TranscriptOutcome::Unavailable));

    let exact_cwd = format!("/{}", "b".repeat(4_095));
    let oversized_cwd = format!("/{}", "b".repeat(4_096));
    let exact = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "exact"),
            &exact_cwd,
        ))
        .await;
    assert!(matches!(exact.analysis, TranscriptOutcome::Ready(_)));
    let oversized = source
        .observe(request(
            TranscriptKind::Claude,
            session("id", "oversized"),
            &oversized_cwd,
        ))
        .await;
    assert!(matches!(oversized.analysis, TranscriptOutcome::Unavailable));
    assert_eq!(io.reads(), 2);
}

#[tokio::test]
async fn blocking_read_concurrency_remains_capped_after_callers_are_queued() {
    let root = PathBuf::from("/fixture/claude-projects");
    let io = GatedIo::new(root.clone(), make_windows(&claude_content()));
    let started = Arc::clone(&io.started);
    let source = Arc::new(
        FilesystemTranscriptSource::with_io(
            TranscriptRoots {
                claude_projects_root: root,
                codex_sessions_root: PathBuf::from("/fixture/codex-sessions"),
                copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
            },
            io.clone(),
            TranscriptAdapterLimits {
                read_concurrency: 1,
                cache_entries: 0,
                codex_scan: CodexScanLimits::default(),
            },
        )
        .unwrap_or_else(|error| panic!("build source: {error:?}")),
    );
    let waiting = started.notified();
    let first_source = Arc::clone(&source);
    let first = tokio::spawn(async move {
        first_source
            .observe(request(
                TranscriptKind::Claude,
                session("id", "first"),
                "/repo",
            ))
            .await
    });
    waiting.await;
    let second_source = Arc::clone(&source);
    let second = tokio::spawn(async move {
        second_source
            .observe(request(
                TranscriptKind::Claude,
                session("id", "second"),
                "/repo",
            ))
            .await
    });
    assert_eq!(io.active(), 1);
    assert_eq!(io.peak(), 1);
    io.release();
    let _ = first
        .await
        .unwrap_or_else(|error| panic!("first task: {error}"));
    let _ = second
        .await
        .unwrap_or_else(|error| panic!("second task: {error}"));
    assert_eq!(io.peak(), 1);
}

#[tokio::test]
async fn aborting_a_caller_keeps_its_blocking_lane_bounded_then_releases_it() {
    let root = PathBuf::from("/fixture/claude-projects");
    let io = GatedIo::new(root.clone(), make_windows(&claude_content()));
    let source = Arc::new(
        FilesystemTranscriptSource::with_io(
            TranscriptRoots {
                claude_projects_root: root,
                codex_sessions_root: PathBuf::from("/fixture/codex-sessions"),
                copilot_session_state_root: PathBuf::from("/fixture/copilot-session-state"),
            },
            io.clone(),
            TranscriptAdapterLimits {
                read_concurrency: 1,
                cache_entries: 0,
                codex_scan: CodexScanLimits::default(),
            },
        )
        .unwrap_or_else(|error| panic!("build source: {error:?}")),
    );

    let first_started = io.started.notified();
    let first_source = Arc::clone(&source);
    let first = tokio::spawn(async move {
        first_source
            .observe(request(
                TranscriptKind::Claude,
                session("id", "aborted"),
                "/repo",
            ))
            .await
    });
    first_started.await;
    first.abort();

    let second_source = Arc::clone(&source);
    let second = tokio::spawn(async move {
        second_source
            .observe(request(
                TranscriptKind::Claude,
                session("id", "successor"),
                "/repo",
            ))
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(io.active(), 1);
    assert_eq!(io.peak(), 1);

    io.release();
    let successor = tokio::time::timeout(std::time::Duration::from_secs(2), second)
        .await
        .unwrap_or_else(|error| panic!("successor remained starved: {error}"))
        .unwrap_or_else(|error| panic!("successor task: {error}"));
    assert!(matches!(successor.analysis, TranscriptOutcome::Ready(_)));
    assert_eq!(io.peak(), 1);
}

#[derive(Clone)]
struct MutableIo {
    root: PathBuf,
    windows: Arc<Mutex<TranscriptWindows>>,
    read_count: Arc<Mutex<usize>>,
}

impl MutableIo {
    fn new(root: PathBuf, windows: TranscriptWindows) -> Self {
        Self {
            root,
            windows: Arc::new(Mutex::new(windows)),
            read_count: Arc::new(Mutex::new(0)),
        }
    }

    fn replace(&self, windows: TranscriptWindows) {
        if let Ok(mut current) = self.windows.lock() {
            *current = windows;
        }
    }

    fn reads(&self) -> usize {
        self.read_count.lock().map_or(0, |count| *count)
    }
}

impl BlockingTranscriptIo for MutableIo {
    fn read_plan(
        &self,
        _roots: &TranscriptRoots,
        _plan: &TranscriptLocationPlan,
        _cached_codex_candidate: Option<&agentdeck_core::transcript::SafeRelativePath>,
        _limits: CodexScanLimits,
    ) -> Result<BlockingTranscriptRead, TranscriptIoError> {
        if let Ok(mut count) = self.read_count.lock() {
            *count += 1;
        }
        let windows = self
            .windows
            .lock()
            .map(|windows| windows.clone())
            .map_err(|_| TranscriptIoError::Unavailable)?;
        Ok(BlockingTranscriptRead::new(
            self.root.join("fixture.jsonl"),
            windows,
            None,
        ))
    }
}

#[derive(Clone)]
struct GatedIo {
    inner: MutableIo,
    state: Arc<(Mutex<GateState>, Condvar)>,
    started: Arc<Notify>,
}

#[derive(Default)]
struct GateState {
    active: usize,
    peak: usize,
    released: bool,
}

impl GatedIo {
    fn new(root: PathBuf, windows: TranscriptWindows) -> Self {
        Self {
            inner: MutableIo::new(root, windows),
            state: Arc::new((Mutex::new(GateState::default()), Condvar::new())),
            started: Arc::new(Notify::new()),
        }
    }

    fn active(&self) -> usize {
        self.state.0.lock().map_or(0, |state| state.active)
    }

    fn peak(&self) -> usize {
        self.state.0.lock().map_or(0, |state| state.peak)
    }

    fn release(&self) {
        if let Ok(mut state) = self.state.0.lock() {
            state.released = true;
            self.state.1.notify_all();
        }
    }
}

impl BlockingTranscriptIo for GatedIo {
    fn read_plan(
        &self,
        roots: &TranscriptRoots,
        plan: &TranscriptLocationPlan,
        cached_codex_candidate: Option<&agentdeck_core::transcript::SafeRelativePath>,
        limits: CodexScanLimits,
    ) -> Result<BlockingTranscriptRead, TranscriptIoError> {
        let mut state = self
            .state
            .0
            .lock()
            .map_err(|_| TranscriptIoError::Unavailable)?;
        state.active += 1;
        state.peak = state.peak.max(state.active);
        self.started.notify_one();
        while !state.released {
            state = self
                .state
                .1
                .wait(state)
                .map_err(|_| TranscriptIoError::Unavailable)?;
        }
        state.active = state.active.saturating_sub(1);
        drop(state);
        self.inner
            .read_plan(roots, plan, cached_codex_candidate, limits)
    }
}

fn make_windows(content: &str) -> TranscriptWindows {
    make_windows_bytes(content.as_bytes())
}

fn make_windows_bytes(content: &[u8]) -> TranscriptWindows {
    TranscriptWindows::try_new(
        content.to_vec(),
        None,
        content.to_vec(),
        u64::try_from(content.len()).unwrap_or(u64::MAX),
        FileTimestamp {
            unix_seconds: 10,
            nanoseconds: 1,
        },
        content.len().saturating_mul(2),
    )
    .unwrap_or_else(|error| panic!("valid fixture windows: {error:?}"))
}

fn make_partial_tail_windows(record: &[u8]) -> TranscriptWindows {
    let padding = CONTEXT_TAIL_BYTES
        .checked_sub(record.len().saturating_add(1))
        .unwrap_or_else(|| panic!("record fits bounded tail"));
    let mut tail = vec![b'x'; padding];
    tail.push(b'\n');
    tail.extend_from_slice(record);
    let head = vec![b' '; OPENING_HEAD_BYTES];
    TranscriptWindows::try_new(
        head,
        Some(b'x'),
        tail,
        u64::try_from(CONTEXT_TAIL_BYTES + 1).unwrap_or(u64::MAX),
        FileTimestamp {
            unix_seconds: 10,
            nanoseconds: 1,
        },
        OPENING_HEAD_BYTES + CONTEXT_TAIL_BYTES + 1,
    )
    .unwrap_or_else(|error| panic!("valid partial-tail windows: {error:?}"))
}

fn parser_request(kind: TranscriptKind) -> TranscriptRequest {
    match kind {
        TranscriptKind::Claude => request(kind, session("id", "fixture"), "/fixture"),
        TranscriptKind::Pi => request(kind, session("path", "/fixture/pi.jsonl"), ""),
        TranscriptKind::Codex => request(kind, session("id", "fixture"), ""),
        TranscriptKind::Copilot => request(
            kind,
            HerdrAgentSession {
                source: "fixture".to_owned(),
                agent: "copilot".to_owned(),
                kind: "id".to_owned(),
                value: "fixture".to_owned(),
            },
            "",
        ),
        TranscriptKind::Unknown => {
            panic!("parser fixture requires a supported kind")
        }
    }
}
