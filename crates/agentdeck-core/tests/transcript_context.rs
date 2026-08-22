use agentdeck_core::HerdrAgentSession;
use agentdeck_core::{
    context::{ContextOutcome, extract_context, extract_context_window, infer_limit},
    headings::HeadingStore,
    transcript::{
        ASSISTANT_TURNS, CONTEXT_TAIL_BYTES, COPILOT_MAX_PHYSICAL_EVENT_BYTES,
        COPILOT_MAX_RECOVERY_ANCHORS, DIGEST_TAIL_BYTES, OPENING_HEAD_BYTES, TailRead,
        TranscriptInput, TranscriptKind, TranscriptOutcome, analyze, analyze_windows, bounded_tail,
        bounded_tail_read, cache_fingerprint, carries_intent, claude_relative_path, clip_graphemes,
        copilot_relative_path, grapheme_count, is_real_prompt, location_plan, parse_copilot_events,
        pi_exact_path, select_codex_candidate, stable_bytes_key, stable_key, unwrap_command,
        valid_opaque_session_id,
    },
};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;

fn line(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|error| panic!("JSON fixture: {error}"))
}

fn claude(role: &str, text: &str) -> String {
    line(json!({"message": {"role": role, "content": text}}))
}

fn claude_meta(text: &str) -> String {
    line(json!({"isMeta": true, "message": {"role": "user", "content": text}}))
}

fn pi(role: &str, text: &str) -> String {
    line(
        json!({"type": "message", "message": {"role": role, "content": [{"type": "text", "text": text}]}}),
    )
}

fn codex(role: &str, text: &str) -> String {
    line(
        json!({"type": "response_item", "payload": {"type": "message", "role": role, "content": [{"type": "input_text", "text": text}]}}),
    )
}

fn copilot(event: &str, data: Value) -> String {
    let event = serde_json::to_string(event).unwrap_or_else(|error| panic!("event JSON: {error}"));
    let data = serde_json::to_string(&data).unwrap_or_else(|error| panic!("data JSON: {error}"));
    format!(
        "{{\"type\":{event},\"data\":{data},\"id\":\"fixture\",\"timestamp\":\"2000-01-01T00:00:00Z\"}}"
    )
}

#[test]
fn rejects_injected_agents_and_mem0_but_keeps_real_requests_about_them() {
    assert!(!is_real_prompt(
        "# AGENTS.md instructions\n\n<INSTRUCTIONS>Injected</INSTRUCTIONS>"
    ));
    assert!(!is_real_prompt("## Mem0 context\n- injected"));
    assert!(is_real_prompt(
        "Can you update the AGENTS.md instructions for this repository?"
    ));
    assert!(is_real_prompt(
        "Why are the Mem0 instructions appearing in the title?"
    ));
}

#[test]
fn rejects_compaction_interruption_images_urls_and_system_wrappers() {
    for text in [
        "This session is being continued from a previous conversation that ran out of context.",
        "[Request interrupted by user for tool use]",
        "[Image 1]",
        "https://example.test",
        "<local-command-stdout>echo</local-command-stdout>",
        "a system-reminder with a task",
    ] {
        assert!(!is_real_prompt(text), "must filter {text:?}");
    }
    assert!(is_real_prompt(
        "Recreate something like https://example.test on macOS"
    ));
}

#[test]
fn claude_codex_and_pi_openings_skip_harness_records() {
    let claude_bytes = [
        claude_meta("Automated task"),
        claude(
            "user",
            "This session is being continued from a previous conversation with synthetic history.",
        ),
        claude("user", "Diagnose misleading generated session names."),
    ]
    .join("\n");
    let TranscriptOutcome::Ready(claude_result) = analyze(
        TranscriptKind::Claude,
        TranscriptInput::Bytes(claude_bytes.as_bytes()),
        1,
    ) else {
        panic!("Claude should parse")
    };
    assert_eq!(
        claude_result.opening.as_deref(),
        Some("Diagnose misleading generated session names.")
    );

    let codex_bytes = [
        codex("user", "# AGENTS.md instructions\nInjected"),
        codex("user", "Explain why generated names are wrong."),
    ]
    .join("\n");
    let TranscriptOutcome::Ready(codex_result) = analyze(
        TranscriptKind::Codex,
        TranscriptInput::Bytes(codex_bytes.as_bytes()),
        1,
    ) else {
        panic!("Codex should parse")
    };
    assert_eq!(
        codex_result.opening.as_deref(),
        Some("Explain why generated names are wrong.")
    );

    let pi_bytes = [
        line(json!({"type": "compaction", "summary": "synthetic"})),
        pi("toolResult", "tool noise"),
        pi("user", "<skill>injected</skill>"),
        pi("user", "Review the chapter structure."),
    ]
    .join("\n");
    let TranscriptOutcome::Ready(pi_result) = analyze(
        TranscriptKind::Pi,
        TranscriptInput::Bytes(pi_bytes.as_bytes()),
        1,
    ) else {
        panic!("Pi should parse")
    };
    assert_eq!(
        pi_result.opening.as_deref(),
        Some("Review the chapter structure.")
    );
}

#[test]
fn copilot_keeps_only_root_prompts_final_replies_and_safe_context() {
    let bytes = [
        copilot("session.start", json!({"selectedModel": "fixture-model"})),
        copilot("system.message", json!({"content": "DO_NOT_INCLUDE", "role": "system"})),
        copilot("user.message", json!({"content": "Review the bounded local transcript adapter.", "transformedContent": "DO_NOT_INCLUDE", "source": "user", "attachments": ["DO_NOT_INCLUDE"]})),
        copilot("user.message", json!({"content": "DO_NOT_INCLUDE", "source": "system"})),
        copilot("user.message", json!({"content": "DO_NOT_INCLUDE", "ephemeral": true, "source": "user"})),
        line(json!({"type": "assistant.message", "agentId": "subagent", "data": {"content": "DO_NOT_INCLUDE", "toolRequests": []}})),
        copilot("assistant.message", json!({"content": "DO_NOT_INCLUDE", "toolRequests": [{"name": "shell"}]})),
        copilot("assistant.message", json!({"content": "DO_NOT_INCLUDE", "parentToolCallId": "call-1"})),
        copilot("assistant.message", json!({"content": "The bounded parser is ready.", "toolRequests": []})),
        copilot("session.usage_info", json!({"currentTokens": 120, "tokenLimit": 200})),
        copilot("assistant.usage", json!({"model": "fixture-model", "inputTokens": 20, "outputTokens": 4, "parentToolCallId": null})),
        copilot("assistant.reasoning", json!({"content": "DO_NOT_INCLUDE_REASONING"})),
        copilot("error", json!({"message": "DO_NOT_INCLUDE_ERROR"})),
        copilot("phase.start", json!({"content": "DO_NOT_INCLUDE_PHASE"})),
        copilot("tool.result", json!({"content": "DO_NOT_INCLUDE_TOOL"})),
        copilot("skill.invoked", json!({"content": "DO_NOT_INCLUDE"})),
        copilot("session.compaction_complete", json!({"summaryContent": "DO_NOT_INCLUDE", "checkpointPath": "DO_NOT_INCLUDE"})),
    ].join("\n");
    let TranscriptOutcome::Ready(analysis) = analyze(
        TranscriptKind::Copilot,
        TranscriptInput::Bytes(bytes.as_bytes()),
        7,
    ) else {
        panic!("Copilot fixture should parse")
    };
    let digest = analysis.digest.unwrap_or_else(|| panic!("Copilot digest"));
    assert_eq!(
        digest.last_prompt,
        "Review the bounded local transcript adapter."
    );
    assert_eq!(digest.last_reply, "The bounded parser is ready.");
    assert!(!format!("{digest:?}").contains("DO_NOT_INCLUDE"));
    assert!(matches!(
        extract_context(TranscriptKind::Copilot, TranscriptInput::Bytes(bytes.as_bytes())),
        ContextOutcome::Ready(context) if context.used == 120 && context.limit == 200 && context.model.as_deref() == Some("fixture-model")
    ));
    assert!(TranscriptKind::Copilot.supports_enrichment());
    assert!(TranscriptKind::Copilot.supports_generated_headings());
    assert!(TranscriptKind::Copilot.is_supported());
}

#[test]
fn copilot_ephemeral_is_fail_closed_except_for_absent_or_false() {
    let visible = copilot(
        "user.message",
        json!({"content": "Visible only with a defined ephemeral marker.", "ephemeral": false, "source": "user"}),
    );
    assert!(matches!(
        analyze(
            TranscriptKind::Copilot,
            TranscriptInput::Bytes(visible.as_bytes()),
            1
        ),
        TranscriptOutcome::Ready(_)
    ));
    let top_level_false = line(json!({
        "type": "user.message",
        "ephemeral": false,
        "data": {"content": "Visible with top-level false too.", "source": "user"}
    }));
    assert!(matches!(
        analyze(
            TranscriptKind::Copilot,
            TranscriptInput::Bytes(top_level_false.as_bytes()),
            1
        ),
        TranscriptOutcome::Ready(_)
    ));

    for ephemeral in [
        Value::Bool(true),
        Value::Null,
        Value::String("false".to_owned()),
        json!(0),
        json!({"visible": false}),
        json!([false]),
    ] {
        for event in [
            line(
                json!({"type": "user.message", "ephemeral": ephemeral.clone(), "data": {"content": "DO_NOT_INCLUDE_EPHEMERAL", "source": "user"}}),
            ),
            copilot(
                "user.message",
                json!({"content": "DO_NOT_INCLUDE_EPHEMERAL", "ephemeral": ephemeral, "source": "user"}),
            ),
        ] {
            assert!(matches!(
                analyze(
                    TranscriptKind::Copilot,
                    TranscriptInput::Bytes(event.as_bytes()),
                    1
                ),
                TranscriptOutcome::Empty
            ));
        }
    }

    let unsafe_context = line(json!({
        "type": "session.usage_info",
        "ephemeral": null,
        "data": {"currentTokens": 1, "tokenLimit": 2}
    }));
    assert_eq!(
        extract_context(
            TranscriptKind::Copilot,
            TranscriptInput::Bytes(unsafe_context.as_bytes())
        ),
        ContextOutcome::Empty
    );
}

#[test]
fn copilot_context_prefers_latest_assistant_model_and_falls_back_to_session_start() {
    let fallback = [
        copilot("session.start", json!({"selectedModel": "start-model"})),
        copilot(
            "session.usage_info",
            json!({"currentTokens": 9, "tokenLimit": 10}),
        ),
    ]
    .join("\n");
    assert!(matches!(
        extract_context(TranscriptKind::Copilot, TranscriptInput::Bytes(fallback.as_bytes())),
        ContextOutcome::Ready(context) if context.model.as_deref() == Some("start-model")
    ));

    let preferred = [
        copilot("session.start", json!({"selectedModel": "start-model"})),
        copilot(
            "assistant.usage",
            json!({"model": "earlier-assistant", "parentToolCallId": null}),
        ),
        copilot(
            "session.usage_info",
            json!({"currentTokens": 1, "tokenLimit": 10}),
        ),
        copilot(
            "assistant.usage",
            json!({"model": "latest-assistant", "parentToolCallId": null}),
        ),
    ]
    .join("\n");
    assert!(matches!(
        extract_context(TranscriptKind::Copilot, TranscriptInput::Bytes(preferred.as_bytes())),
        ContextOutcome::Ready(context) if context.used == 1 && context.limit == 10 && context.model.as_deref() == Some("latest-assistant")
    ));
}

#[test]
fn copilot_corruption_oversize_and_concatenated_suffixes_stay_soft_and_bounded() {
    let valid = copilot(
        "user.message",
        json!({"content": "Recover a complete event suffix safely.", "source": "user"}),
    );
    let concatenated =
        format!("{{\"type\":\"assistant.message\",\"data\":{{\"content\":\"truncated{valid}");
    let suffix = concatenated
        .rfind("{\"type\"")
        .unwrap_or_else(|| panic!("fixture recovery anchor"));
    serde_json::from_str::<Value>(&concatenated[suffix..])
        .unwrap_or_else(|error| panic!("synthetic suffix JSON: {error}"));
    let parsed = parse_copilot_events(concatenated.as_bytes());
    assert_eq!(parsed.values.len(), 1);
    assert_eq!(parsed.malformed_lines, 1);
    assert_eq!(parsed.recovery_attempts, 2);
    let huge = format!(
        "{{\"type\":\"user.message\",\"data\":{{\"content\":\"{}",
        "x".repeat(COPILOT_MAX_PHYSICAL_EVENT_BYTES)
    );
    let mixed = format!("{huge}\n{valid}");
    let parsed = parse_copilot_events(mixed.as_bytes());
    assert_eq!(parsed.values.len(), 1);
    assert_eq!(parsed.malformed_lines, 1);
    assert_eq!(parsed.recovery_attempts, 0);
    let TranscriptOutcome::Ready(analysis) = analyze(
        TranscriptKind::Copilot,
        TranscriptInput::Bytes(mixed.as_bytes()),
        1,
    ) else {
        panic!("later valid Copilot event survives")
    };
    assert_eq!(
        analysis.opening.as_deref(),
        Some("Recover a complete event suffix safely.")
    );
}

#[test]
fn copilot_raw_physical_line_cap_counts_padding_and_crlf_before_trimming() {
    let valid = copilot(
        "user.message",
        json!({"content": "A bounded valid event.", "source": "user"}),
    );
    for (raw_len, accepted) in [
        (COPILOT_MAX_PHYSICAL_EVENT_BYTES - 1, true),
        (COPILOT_MAX_PHYSICAL_EVENT_BYTES, true),
        (COPILOT_MAX_PHYSICAL_EVENT_BYTES + 1, false),
    ] {
        let padded = format!("{valid}{}", " ".repeat(raw_len - valid.len()));
        let parsed = parse_copilot_events(padded.as_bytes());
        assert_eq!(parsed.values.len(), usize::from(accepted));
        assert_eq!(parsed.malformed_lines, usize::from(!accepted));
    }

    for (raw_len, accepted) in [
        (COPILOT_MAX_PHYSICAL_EVENT_BYTES - 1, true),
        (COPILOT_MAX_PHYSICAL_EVENT_BYTES, true),
        (COPILOT_MAX_PHYSICAL_EVENT_BYTES + 1, false),
    ] {
        // The split delimiter is LF, so the preceding CR is part of the raw
        // physical record and consumes one byte of the cap.
        let crlf = format!("{valid}{}\r\n", " ".repeat(raw_len - valid.len() - 1));
        let parsed = parse_copilot_events(crlf.as_bytes());
        assert_eq!(parsed.values.len(), usize::from(accepted));
        assert_eq!(parsed.malformed_lines, usize::from(!accepted));
    }

    let whitespace_padded = format!("{}{}", " ".repeat(COPILOT_MAX_PHYSICAL_EVENT_BYTES), valid);
    let parsed = parse_copilot_events(whitespace_padded.as_bytes());
    assert!(parsed.values.is_empty());
    assert_eq!(parsed.malformed_lines, 1);
}

#[test]
fn copilot_recovery_caps_json_attempts_at_the_64th_anchor() {
    let valid = copilot(
        "user.message",
        json!({"content": "Recover the bounded final suffix.", "source": "user"}),
    );
    let malformed_anchor = r#"{"type"x"#;
    let at_64 = format!("{}{}", malformed_anchor.repeat(63), valid);
    let parsed = parse_copilot_events(at_64.as_bytes());
    assert_eq!(parsed.recovery_attempts, COPILOT_MAX_RECOVERY_ANCHORS);
    assert_eq!(parsed.values.len(), 1);

    let at_65 = format!("{}{}", malformed_anchor.repeat(64), valid);
    let parsed = parse_copilot_events(at_65.as_bytes());
    assert_eq!(parsed.recovery_attempts, COPILOT_MAX_RECOVERY_ANCHORS);
    assert!(parsed.values.is_empty());

    let adversarial = format!("{}{}", malformed_anchor.repeat(5_000), valid);
    let parsed = parse_copilot_events(adversarial.as_bytes());
    assert_eq!(parsed.recovery_attempts, COPILOT_MAX_RECOVERY_ANCHORS);
    assert!(parsed.values.is_empty());
    assert_eq!(parsed.malformed_lines, 1);
}

#[test]
fn whole_turn_pleasantries_do_not_carry_intent_but_wrapped_requests_do() {
    for turn in [
        "good morning",
        "Good morning!",
        "morning Claude",
        "hi there",
        "thanks",
        "ok",
        "sounds good",
        "Nice one 🎉",
    ] {
        assert!(!carries_intent(turn), "pleasantry {turn:?}");
    }
    for turn in [
        "good morning, can you check the deploy?",
        "thanks — now fix the heading generator",
        "ok do the same for the subtitle",
    ] {
        assert!(carries_intent(turn), "request {turn:?}");
    }
}

#[test]
fn opening_skips_greeting_and_digest_prefers_requests() {
    let substance = "check whether the scheduled validation run completed";
    let bytes = [
        claude("user", "good morning"),
        claude("user", substance),
        claude("user", "thanks"),
    ]
    .join("\n");
    let TranscriptOutcome::Ready(result) = analyze(
        TranscriptKind::Claude,
        TranscriptInput::Bytes(bytes.as_bytes()),
        99,
    ) else {
        panic!("must parse")
    };
    assert_eq!(result.opening.as_deref(), Some(substance));
    let digest = result.digest.unwrap_or_else(|| panic!("digest"));
    assert_eq!(digest.last_prompt, substance);
    assert_eq!(digest.requests, format!("- {substance}"));
    assert_eq!(digest.written_at, 99);
}

#[test]
fn greeting_only_is_a_valid_fallback_not_an_empty_transcript() {
    let bytes = claude("user", "good morning");
    let TranscriptOutcome::Ready(result) = analyze(
        TranscriptKind::Claude,
        TranscriptInput::Bytes(bytes.as_bytes()),
        1,
    ) else {
        panic!("greeting is a valid turn")
    };
    assert_eq!(result.opening.as_deref(), Some("good morning"));
    assert_eq!(
        result
            .digest
            .unwrap_or_else(|| panic!("digest"))
            .last_prompt,
        "good morning"
    );
}

#[test]
fn slash_command_arguments_become_request_and_empty_command_stays_filtered() {
    let command = "<command-name>/goal</command-name><command-args>fix the drag bug</command-args>";
    assert_eq!(unwrap_command(command), "fix the drag bug");
    let empty = "<command-name>/endday</command-name><command-args></command-args>";
    assert_eq!(unwrap_command(empty), empty);
    assert!(!is_real_prompt(&unwrap_command(empty)));
}

#[test]
fn digest_is_bounded_keyed_stably_and_clips_by_grapheme() {
    let users = (0..7)
        .map(|index| claude("user", &format!("request {index} has enough words")))
        .collect::<Vec<_>>();
    let assistants = (0..3)
        .map(|index| {
            claude(
                "assistant",
                &format!("assistant reply {index} {}", "x".repeat(1500)),
            )
        })
        .collect::<Vec<_>>();
    let bytes = users
        .into_iter()
        .chain(assistants)
        .collect::<Vec<_>>()
        .join("\n");
    let TranscriptOutcome::Ready(result) = analyze(
        TranscriptKind::Claude,
        TranscriptInput::Bytes(bytes.as_bytes()),
        7,
    ) else {
        panic!("parse")
    };
    let digest = result.digest.unwrap_or_else(|| panic!("digest"));
    assert_eq!(digest.requests.lines().count(), 5);
    assert_eq!(digest.recent.matches("ASSISTANT:").count(), ASSISTANT_TURNS);
    assert!(digest.last_reply.graphemes(true).count() <= 1400);
    assert_eq!(
        digest.last_prompt_key.as_deref(),
        Some(stable_key(&digest.last_prompt).as_str())
    );
    assert_ne!(digest.last_prompt_key, digest.last_reply_key);
    assert_eq!(clip_graphemes("👩‍💻👩‍💻", 1), "👩‍💻");
}

#[test]
fn bounded_tail_discards_partial_first_line_and_accepts_unterminated_final_line() {
    let valid = claude("user", "real request with enough words");
    let input = format!("{}\n{valid}", "x".repeat(DIGEST_TAIL_BYTES));
    let tail = bounded_tail(input.as_bytes(), DIGEST_TAIL_BYTES);
    assert_eq!(
        std::str::from_utf8(tail).unwrap_or_else(|error| panic!("UTF-8: {error}")),
        valid
    );
    let TranscriptOutcome::Ready(result) =
        analyze(TranscriptKind::Claude, TranscriptInput::Bytes(tail), 1)
    else {
        panic!("unterminated valid final line")
    };
    assert!(result.digest.is_some());
}

#[test]
fn bounded_tail_retains_an_exact_record_boundary_and_window_analysis_is_independent() {
    let valid = claude("user", "request from the exact bounded boundary");
    let window = format!(
        "{valid}\n{}",
        "q".repeat(DIGEST_TAIL_BYTES - valid.len() - 1)
    );
    let input = format!("outside\n{window}");
    let tail = bounded_tail(input.as_bytes(), DIGEST_TAIL_BYTES);
    assert!(
        std::str::from_utf8(tail)
            .unwrap_or_else(|error| panic!("UTF-8: {error}"))
            .starts_with(&valid)
    );
    let opening = claude("user", "opening request with sufficient substance");
    let TranscriptOutcome::Ready(result) = analyze_windows(
        TranscriptKind::Claude,
        opening.as_bytes(),
        TailRead {
            preceding_byte: None,
            bytes: valid.as_bytes(),
        },
        9,
    ) else {
        panic!("separate windows")
    };
    assert_eq!(
        result.opening.as_deref(),
        Some("opening request with sufficient substance")
    );
    assert_eq!(result.decoded_records, 1);
}

#[test]
fn byte_limits_malformed_lines_lossy_utf8_and_empty_are_distinguished() {
    assert_eq!(OPENING_HEAD_BYTES, 256 * 1024);
    assert_eq!(DIGEST_TAIL_BYTES, 1024 * 1024);
    assert_eq!(CONTEXT_TAIL_BYTES, 1024 * 1024);
    assert!(matches!(
        analyze(TranscriptKind::Claude, TranscriptInput::Unavailable, 0),
        TranscriptOutcome::Unavailable
    ));
    assert!(matches!(
        analyze(TranscriptKind::Claude, TranscriptInput::NotYetCreated, 0),
        TranscriptOutcome::NotYetCreated
    ));
    assert!(matches!(
        analyze(TranscriptKind::Copilot, TranscriptInput::Bytes(b"{}"), 0),
        TranscriptOutcome::Empty
    ));
    assert!(matches!(
        analyze(TranscriptKind::Claude, TranscriptInput::Bytes(b"{bad"), 0),
        TranscriptOutcome::Malformed
    ));
    assert!(matches!(
        analyze(
            TranscriptKind::Claude,
            TranscriptInput::Bytes(b"{\"other\":true}"),
            0
        ),
        TranscriptOutcome::Empty
    ));
    let bytes = [
        b"\xff\xfebad\n".as_slice(),
        claude("user", "valid request after bad bytes").as_bytes(),
    ]
    .concat();
    let TranscriptOutcome::Ready(result) =
        analyze(TranscriptKind::Claude, TranscriptInput::Bytes(&bytes), 0)
    else {
        panic!("lossy bad line must not poison good record")
    };
    assert!(result.malformed_lines >= 1);
}

#[test]
fn context_formulas_inference_rounding_and_missing_usage_are_honest() {
    let claude_bytes = line(
        json!({"message": {"model": "claude-opus", "usage": {"input_tokens": 100000, "cache_read_input_tokens": 30000, "cache_creation_input_tokens": 20000}}}),
    );
    let ContextOutcome::Ready(claude_context) = extract_context(
        TranscriptKind::Claude,
        TranscriptInput::Bytes(claude_bytes.as_bytes()),
    ) else {
        panic!("Claude context")
    };
    assert_eq!(
        (
            claude_context.used,
            claude_context.limit,
            claude_context.percent
        ),
        (150000, 200000, 75)
    );

    let pi_bytes = line(
        json!({"type": "message", "message": {"model": "gpt", "usage": {"input": 101000, "cacheRead": 0, "totalTokens": 999999}}}),
    );
    let ContextOutcome::Ready(pi_context) = extract_context(
        TranscriptKind::Pi,
        TranscriptInput::Bytes(pi_bytes.as_bytes()),
    ) else {
        panic!("Pi context")
    };
    assert_eq!(
        (pi_context.used, pi_context.limit, pi_context.percent),
        (101000, 400000, 25)
    );

    let codex_bytes = line(
        json!({"type": "event_msg", "payload": {"type": "token_count", "info": {"last_token_usage": {"total_tokens": 50001}, "total_token_usage": 900000, "model_context_window": 100000}}}),
    );
    let ContextOutcome::Ready(codex_context) = extract_context(
        TranscriptKind::Codex,
        TranscriptInput::Bytes(codex_bytes.as_bytes()),
    ) else {
        panic!("Codex context")
    };
    assert_eq!(
        (
            codex_context.used,
            codex_context.limit,
            codex_context.percent
        ),
        (50001, 100000, 50)
    );
    assert_eq!(infer_limit(Some("claude [1m]"), 1), 1_000_000);
    assert_eq!(infer_limit(Some("claude"), 180000), 200000);
    assert_eq!(infer_limit(Some("claude"), 180001), 1_000_000);
    let cache_only_claude = line(json!({"message": {"usage": {"cache_read_input_tokens": 17}}}));
    assert!(
        matches!(extract_context(TranscriptKind::Claude, TranscriptInput::Bytes(cache_only_claude.as_bytes())), ContextOutcome::Ready(context) if context.used == 17)
    );
    let cache_only_pi = line(json!({"type": "message", "message": {"usage": {"cacheRead": 19}}}));
    assert!(
        matches!(extract_context(TranscriptKind::Pi, TranscriptInput::Bytes(cache_only_pi.as_bytes())), ContextOutcome::Ready(context) if context.used == 19)
    );
    let negative =
        line(json!({"message": {"usage": {"input_tokens": -1, "cache_read_input_tokens": 99}}}));
    assert!(matches!(
        extract_context(
            TranscriptKind::Claude,
            TranscriptInput::Bytes(negative.as_bytes())
        ),
        ContextOutcome::Empty
    ));
    assert!(matches!(
        extract_context(
            TranscriptKind::Claude,
            TranscriptInput::Bytes(br#"{"message":{"usage":{}}}"#)
        ),
        ContextOutcome::Empty
    ));
}

#[test]
fn context_tolerates_isolated_bad_json_and_rejects_only_all_malformed() {
    let valid = line(json!({"message": {"usage": {"input_tokens": 1}}}));
    let bytes = format!("{{bad\r\n{valid}\r\n");
    assert!(matches!(
        extract_context(
            TranscriptKind::Claude,
            TranscriptInput::Bytes(bytes.as_bytes())
        ),
        ContextOutcome::Ready(_)
    ));
    assert!(matches!(
        extract_context(TranscriptKind::Codex, TranscriptInput::Bytes(b"{bad")),
        ContextOutcome::Malformed
    ));
}

#[test]
fn deeply_nested_records_and_token_overflow_degrade_without_panicking() {
    let nested = format!("{}0{}", "{\"a\":".repeat(256), "}".repeat(256));
    assert!(matches!(
        analyze(
            TranscriptKind::Claude,
            TranscriptInput::Bytes(nested.as_bytes()),
            0
        ),
        TranscriptOutcome::Malformed
    ));
    let overflow = line(
        json!({"message": {"usage": {"input_tokens": i64::MAX, "cache_read_input_tokens": 1}}}),
    );
    assert!(matches!(
        extract_context(
            TranscriptKind::Claude,
            TranscriptInput::Bytes(overflow.as_bytes())
        ),
        ContextOutcome::Empty
    ));
}

#[test]
fn pure_path_locator_and_cache_policies_are_bounded_and_stable() {
    assert_eq!(
        claude_relative_path("/tmp/Example Notes", "uuid").map(|path| path.as_str().to_owned()),
        Some("-tmp-Example-Notes/uuid.jsonl".to_owned())
    );
    assert_eq!(
        pi_exact_path("path", "/safe/pi.jsonl").map(|path| path.as_str().to_owned()),
        Some("/safe/pi.jsonl".to_owned())
    );
    assert_eq!(pi_exact_path("uuid", "/safe/pi.jsonl"), None);
    let plan = agentdeck_core::transcript::CodexLocatorPlan {
        sessions_root: "/sessions".to_owned(),
        session_uuid: "abc".to_owned(),
        max_candidates: 2,
    };
    assert_eq!(
        select_codex_candidate(&plan, &["x/rollout-abc.jsonl".to_owned()])
            .unwrap_or_else(|_| panic!("within limit"))
            .map(|path| path.as_str().to_owned()),
        Some("x/rollout-abc.jsonl".to_owned())
    );
    assert!(
        select_codex_candidate(&plan, &["a".to_owned(), "b".to_owned(), "c".to_owned()]).is_err()
    );
    let session = HerdrAgentSession {
        source: "local".to_owned(),
        agent: "pi".to_owned(),
        kind: "path".to_owned(),
        value: "/exact/pi.jsonl".to_owned(),
    };
    assert!(
        matches!(location_plan(TranscriptKind::Pi, Some(&session), "", "/codex", 8), agentdeck_core::transcript::TranscriptLocationPlan::PiExact(path) if path.as_str() == "/exact/pi.jsonl")
    );
    assert!(matches!(
        location_plan(TranscriptKind::Copilot, Some(&session), "", "/codex", 8),
        agentdeck_core::transcript::TranscriptLocationPlan::Unavailable
    ));
    let copilot_session = HerdrAgentSession {
        source: "herdr".to_owned(),
        agent: "copilot".to_owned(),
        kind: "id".to_owned(),
        value: "safe-session_42".to_owned(),
    };
    assert_eq!(
        copilot_relative_path(&copilot_session).map(|path| path.as_str().to_owned()),
        Some("safe-session_42/events.jsonl".to_owned())
    );
    assert!(matches!(
        location_plan(TranscriptKind::Copilot, Some(&copilot_session), "", "/codex", 8),
        agentdeck_core::transcript::TranscriptLocationPlan::CopilotRelative(path)
            if path.as_str() == "safe-session_42/events.jsonl"
    ));
    let wrong_copilot = HerdrAgentSession {
        kind: "path".to_owned(),
        value: "/outside/events.jsonl".to_owned(),
        ..copilot_session.clone()
    };
    assert!(copilot_relative_path(&wrong_copilot).is_none());
    for invalid in [
        HerdrAgentSession {
            agent: "claude".to_owned(),
            ..copilot_session.clone()
        },
        HerdrAgentSession {
            value: "../outside".to_owned(),
            ..copilot_session.clone()
        },
        HerdrAgentSession {
            value: "C:\\outside".to_owned(),
            ..copilot_session
        },
    ] {
        assert!(copilot_relative_path(&invalid).is_none());
    }
    let original = cache_fingerprint("p", 1, 5, 1, Some(b"a"));
    let same_metadata_rewrite = cache_fingerprint("p", 1, 5, 1, Some(b"b"));
    assert_ne!(original, same_metadata_rewrite);
    assert_ne!(original, cache_fingerprint("p", 1, 5, 2, Some(b"a")));
    assert!(!cache_fingerprint("p", 1, 5, 1, None).is_cacheable());
    assert!(original.is_cacheable());
    assert_ne!(stable_bytes_key(&[0xff]), stable_bytes_key(&[0xfe]));
}

#[test]
fn tail_probe_keeps_exact_boundaries_and_discards_mid_record_prefixes() {
    let valid = claude("user", "a retained request after the tail boundary");
    assert_eq!(
        bounded_tail_read(
            TailRead {
                preceding_byte: Some(b'\n'),
                bytes: valid.as_bytes(),
            },
            DIGEST_TAIL_BYTES,
        ),
        valid.as_bytes()
    );
    assert!(
        bounded_tail_read(
            TailRead {
                preceding_byte: Some(b'x'),
                bytes: valid.as_bytes(),
            },
            DIGEST_TAIL_BYTES,
        )
        .is_empty()
    );

    let mid_record = format!("partial record\r\n{valid}\r\n");
    let retained = bounded_tail_read(
        TailRead {
            preceding_byte: Some(b'x'),
            bytes: mid_record.as_bytes(),
        },
        DIGEST_TAIL_BYTES,
    );
    assert_eq!(
        std::str::from_utf8(retained).unwrap_or_else(|error| panic!("UTF-8: {error}")),
        format!("{valid}\r\n")
    );

    let small = bounded_tail_read(
        TailRead {
            preceding_byte: None,
            bytes: valid.as_bytes(),
        },
        DIGEST_TAIL_BYTES,
    );
    assert_eq!(small, valid.as_bytes());
}

#[test]
fn transcript_windows_cap_parsing_to_the_contract_even_for_oversized_inputs() {
    let opening = claude("user", "opening request remains in the capped head window");
    let digest_turn = claude("user", "digest request remains in the capped tail window");
    let oversized_head = [
        opening.as_bytes(),
        b"\n".as_slice(),
        &vec![b'x'; OPENING_HEAD_BYTES + 4096],
    ]
    .concat();
    let oversized_tail = [
        &vec![b'x'; DIGEST_TAIL_BYTES + 4096],
        b"\n".as_slice(),
        digest_turn.as_bytes(),
    ]
    .concat();
    assert!(
        bounded_tail_read(
            TailRead {
                preceding_byte: None,
                bytes: &oversized_tail,
            },
            DIGEST_TAIL_BYTES,
        )
        .len()
            <= DIGEST_TAIL_BYTES
    );
    let TranscriptOutcome::Ready(result) = analyze_windows(
        TranscriptKind::Claude,
        &oversized_head,
        TailRead {
            preceding_byte: None,
            bytes: &oversized_tail,
        },
        1,
    ) else {
        panic!("capped windows should preserve their leading records")
    };
    assert_eq!(result.decoded_records, 1);
    assert_eq!(
        result.opening.as_deref(),
        Some("opening request remains in the capped head window")
    );
}

#[test]
fn convenience_analysis_keeps_a_66_mib_rollout_parse_bounded() {
    let valid = claude(
        "user",
        "the final request is still visible in a large rollout",
    );
    let mut huge = vec![b'x'; 66 * 1024 * 1024];
    huge.push(b'\n');
    huge.extend_from_slice(valid.as_bytes());
    let TranscriptOutcome::Ready(result) =
        analyze(TranscriptKind::Claude, TranscriptInput::Bytes(&huge), 1)
    else {
        panic!("the bounded tail must recover the final record")
    };
    assert_eq!(result.decoded_records, 1);
    assert_eq!(
        result
            .digest
            .as_ref()
            .map(|digest| digest.last_prompt.as_str()),
        Some("the final request is still visible in a large rollout")
    );
}

#[test]
fn codex_digest_keeps_a_prompt_behind_more_than_512_kib_of_tool_records() {
    const OLD_DIGEST_TAIL_BYTES: usize = 512 * 1024;
    let request = "retain the current Codex request after tool-heavy output";
    let prompt = codex("user", request);
    let tool_records = (0..3)
        .map(|index| {
            line(json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "synthetic_tool",
                    "call_id": format!("fixture-{index}"),
                    "arguments": "x".repeat(200 * 1024)
                }
            }))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let bytes = format!("{prompt}\n{tool_records}");
    let bytes_after_prompt = bytes.len() - prompt.len() - 1;
    assert!(bytes_after_prompt > OLD_DIGEST_TAIL_BYTES);
    assert!(bytes.len() < DIGEST_TAIL_BYTES);

    let TranscriptOutcome::Ready(result) = analyze(
        TranscriptKind::Codex,
        TranscriptInput::Bytes(bytes.as_bytes()),
        11,
    ) else {
        panic!("tool-heavy Codex transcript should parse")
    };
    let digest = result.digest.unwrap_or_else(|| panic!("Codex digest"));
    assert_eq!(digest.last_prompt, request);
    assert_eq!(digest.requests, format!("- {request}"));
    assert_eq!(
        digest.last_prompt_key.as_deref(),
        Some(stable_key(request).as_str())
    );

    let mut headings = HeadingStore::default();
    assert!(headings.plan_transcript("pane", &digest, 0).subtitle);
}

#[test]
fn lossy_utf8_inside_a_recoverable_record_keeps_the_record() {
    let mut bytes = br#"{"message":{"role":"user","content":"valid "#.to_vec();
    bytes.push(0xff);
    bytes.extend_from_slice(b" request after invalid utf8\"}}\n");
    let TranscriptOutcome::Ready(result) =
        analyze(TranscriptKind::Claude, TranscriptInput::Bytes(&bytes), 1)
    else {
        panic!("lossy decoding should retain structurally valid JSON")
    };
    assert!(
        result
            .opening
            .as_deref()
            .is_some_and(|opening| opening.contains('\u{fffd}'))
    );
}

#[test]
fn grapheme_count_gates_and_digest_clip_boundaries_are_exact() {
    let emoji = "👩‍💻";
    assert_eq!(grapheme_count(&emoji.repeat(3)), 3);
    assert!(!is_real_prompt(&emoji.repeat(8)));
    assert!(is_real_prompt(&emoji.repeat(9)));
    assert!(!carries_intent(&emoji.repeat(40)));
    assert!(carries_intent(&emoji.repeat(41)));

    let user = emoji.repeat(401);
    let reply = "é".repeat(1401);
    let bytes = [claude("user", &user), claude("assistant", &reply)].join("\n");
    let TranscriptOutcome::Ready(result) = analyze(
        TranscriptKind::Claude,
        TranscriptInput::Bytes(bytes.as_bytes()),
        1,
    ) else {
        panic!("emoji and composed accents are valid turns")
    };
    let digest = result.digest.unwrap_or_else(|| panic!("digest"));
    assert_eq!(grapheme_count(&digest.last_prompt), 400);
    assert_eq!(grapheme_count(&digest.last_reply), 1400);
    assert_eq!(digest.last_prompt, emoji.repeat(400));
    assert_eq!(digest.last_reply, "é".repeat(1400));
}

#[test]
fn location_plans_keep_relative_and_trusted_paths_distinct_and_safe() {
    for bad in ["", " ", ".", "..", "/absolute", "a/b", "a\\b", "a\n", "a\0"] {
        assert!(!valid_opaque_session_id(bad), "must reject {bad:?}");
    }
    assert!(valid_opaque_session_id("018e9c87-3aaf-72a1_aabbcc"));
    assert!(claude_relative_path("", "uuid").is_none());
    assert!(claude_relative_path("   ", "uuid").is_none());
    for cwd in ["/", "C:\\repo\\project", "..", "/abs/../repo"] {
        let path =
            claude_relative_path(cwd, "uuid").unwrap_or_else(|| panic!("safe slug for {cwd:?}"));
        assert!(!path.as_str().starts_with('/'));
        assert!(!path.as_str().contains('\\'));
        assert!(
            path.as_str()
                .split('/')
                .all(|component| !matches!(component, "." | ".."))
        );
    }

    let claude_wrong_kind = HerdrAgentSession {
        source: "local".to_owned(),
        agent: "claude".to_owned(),
        kind: "path".to_owned(),
        value: "uuid".to_owned(),
    };
    assert!(matches!(
        location_plan(
            TranscriptKind::Claude,
            Some(&claude_wrong_kind),
            "/repo",
            "/codex",
            8
        ),
        agentdeck_core::transcript::TranscriptLocationPlan::Unavailable
    ));
    let codex_wrong_kind = HerdrAgentSession {
        agent: "codex".to_owned(),
        ..claude_wrong_kind.clone()
    };
    assert!(matches!(
        location_plan(
            TranscriptKind::Codex,
            Some(&codex_wrong_kind),
            "/repo",
            "/codex",
            8
        ),
        agentdeck_core::transcript::TranscriptLocationPlan::Unavailable
    ));

    let pi_relative = HerdrAgentSession {
        source: "local".to_owned(),
        agent: "pi".to_owned(),
        kind: "path".to_owned(),
        value: "../not-a-safe-relative.jsonl".to_owned(),
    };
    assert!(matches!(
        location_plan(TranscriptKind::Pi, Some(&pi_relative), "", "/codex", 8),
        agentdeck_core::transcript::TranscriptLocationPlan::Unavailable
    ));
    assert!(pi_exact_path("path", "C:\\repo\\pi.jsonl").is_some());
    for invalid_pi_path in [
        "../pi.jsonl",
        "relative/pi.jsonl",
        "/repo/../pi.jsonl",
        " /pi.jsonl",
        "/pi\n.jsonl",
    ] {
        assert!(pi_exact_path("path", invalid_pi_path).is_none());
    }

    let plan = agentdeck_core::transcript::CodexLocatorPlan {
        sessions_root: "/codex".to_owned(),
        session_uuid: "abc".to_owned(),
        max_candidates: 8,
    };
    assert_eq!(
        select_codex_candidate(
            &plan,
            &[
                "/absolute/rollout-abc.jsonl".to_owned(),
                "../rollout-abc.jsonl".to_owned(),
                "nested\\..\\rollout-abc.jsonl".to_owned(),
                "2026/08/rollout-abc.jsonl".to_owned(),
            ],
        )
        .unwrap_or_else(|_| panic!("bounded candidate listing"))
        .map(|path| path.as_str().to_owned()),
        Some("2026/08/rollout-abc.jsonl".to_owned())
    );
}

#[test]
fn context_window_uses_probe_and_latest_valid_codex_record() {
    let valid_claude = line(json!({"message": {"usage": {"input_tokens": 9}}}));
    let partial = format!("partial record\n{valid_claude}");
    assert!(matches!(
        extract_context_window(
            TranscriptKind::Claude,
            TailRead {
                preceding_byte: Some(b'x'),
                bytes: partial.as_bytes(),
            }
        ),
        ContextOutcome::Ready(context) if context.used == 9
    ));
    assert!(matches!(
        extract_context_window(
            TranscriptKind::Claude,
            TailRead {
                preceding_byte: Some(b'\n'),
                bytes: valid_claude.as_bytes(),
            }
        ),
        ContextOutcome::Ready(context) if context.used == 9
    ));

    let older_valid = line(json!({
        "type": "event_msg",
        "payload": {"type": "token_count", "info": {
            "last_token_usage": {"total_tokens": 50_000},
            "model_context_window": 100_000
        }}
    }));
    for invalid_limit in [json!(0), json!(-1), json!("200000"), json!({})] {
        let newer_invalid = line(json!({
            "type": "event_msg",
            "payload": {"type": "token_count", "info": {
                "last_token_usage": {"total_tokens": 90_000},
                "model_context_window": invalid_limit
            }}
        }));
        let bytes = format!("{older_valid}\n{newer_invalid}");
        assert!(matches!(
            extract_context(TranscriptKind::Codex, TranscriptInput::Bytes(bytes.as_bytes())),
            ContextOutcome::Ready(context) if context.used == 50_000 && context.limit == 100_000
        ));
    }
    let missing_limit = line(json!({
        "type": "event_msg",
        "payload": {"type": "token_count", "info": {
            "last_token_usage": {"total_tokens": 50_000}
        }}
    }));
    assert!(matches!(
        extract_context(TranscriptKind::Codex, TranscriptInput::Bytes(missing_limit.as_bytes())),
        ContextOutcome::Ready(context) if context.limit == 200_000
    ));
}

#[test]
fn context_windows_cap_oversized_inputs_and_keep_final_unterminated_records() {
    let valid = line(json!({"message": {"usage": {"input_tokens": 11}}}));
    let oversized = [
        &vec![b'x'; CONTEXT_TAIL_BYTES + 4096],
        b"\n".as_slice(),
        valid.as_bytes(),
    ]
    .concat();
    assert!(matches!(
        extract_context_window(
            TranscriptKind::Claude,
            TailRead {
                preceding_byte: None,
                bytes: &oversized,
            }
        ),
        ContextOutcome::Ready(context) if context.used == 11
    ));
}
