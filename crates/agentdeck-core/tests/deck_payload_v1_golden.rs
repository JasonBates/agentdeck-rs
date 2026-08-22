use std::collections::BTreeSet;

use agentdeck_core::{
    CapabilityBackend, CapabilityLevel, CapabilityReason, CapabilityState, CapabilityStatus,
    DeckCapabilities, DeckPayload, LocalModelStatus, SetupHint, TitleSource,
};
use serde_json::{Map, Value};

// Keep the public fixture tree's capital-T spelling explicit so these includes
// also compile on case-sensitive hosts.
const EMPTY_DEGRADED: &str = include_str!("../../../Tests/golden/deck/empty-degraded.json");
const FULL: &str = include_str!("../../../Tests/golden/deck/full.json");
const CONTRACT_CAPTURE: &str =
    include_str!("../../../Tests/fixtures/contract/deck-payload-v1-sanitized.json");

#[test]
fn rust_goldens_round_trip_without_wire_changes() {
    for fixture in [EMPTY_DEGRADED, FULL] {
        let payload: DeckPayload = serde_json::from_str(fixture)
            .unwrap_or_else(|error| panic!("golden must decode as DeckPayload: {error}"));
        let expected: Value = serde_json::from_str(fixture)
            .unwrap_or_else(|error| panic!("golden must be valid JSON: {error}"));
        let encoded = serde_json::to_value(&payload)
            .unwrap_or_else(|error| panic!("DeckPayload must encode: {error}"));

        assert_eq!(encoded, expected);
    }
}

#[test]
fn contract_fixture_is_semantic_json_equivalence() {
    let contract: Value = serde_json::from_str(CONTRACT_CAPTURE)
        .unwrap_or_else(|error| panic!("contract capture must be valid JSON: {error}"));
    let payload: DeckPayload = serde_json::from_value(contract.clone())
        .unwrap_or_else(|error| panic!("contract capture must decode: {error}"));
    let rust = serde_json::to_value(payload)
        .unwrap_or_else(|error| panic!("DeckPayload must encode: {error}"));

    // JSON object key order and equivalent numeric spellings have no semantic
    // meaning. Runtime change suppression still uses deterministic Rust bytes.
    assert_json_semantically_equal(&contract, &rust, "$".to_owned());
}

#[test]
fn deck_payload_v1_uses_exact_top_level_and_agent_field_names() {
    let value: Value = serde_json::from_str(FULL)
        .unwrap_or_else(|error| panic!("golden must be valid JSON: {error}"));
    let root = value
        .as_object()
        .unwrap_or_else(|| panic!("golden root must be an object"));
    let root_keys = root.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(
        root_keys,
        BTreeSet::from([
            "agents",
            "capacity",
            "herdr",
            "host",
            "localModel",
            "workspaces"
        ])
    );

    let agent = root["agents"][0]
        .as_object()
        .unwrap_or_else(|| panic!("agent must be an object"));
    let agent_keys = agent.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(
        agent_keys,
        BTreeSet::from([
            "activity",
            "background",
            "context",
            "cwd",
            "focus",
            "focused",
            "kind",
            "paneId",
            "phase",
            "project",
            "projectId",
            "status",
            "tabLabel",
            "title",
            "titleSource",
            "unread",
            "workspaceId",
            "workspaceLabel"
        ])
    );
}

#[test]
fn absent_and_null_optionals_decode_but_none_always_encodes_as_omitted() {
    let mut leaves: Value = serde_json::from_str(FULL)
        .unwrap_or_else(|error| panic!("full golden must be valid JSON: {error}"));
    insert_null(&mut leaves, &["herdr"], "detail");
    for key in ["focus", "state", "repliedAgo"] {
        insert_null(&mut leaves, &["agents", "0"], key);
    }
    for key in ["elapsed", "tokens"] {
        insert_null(&mut leaves, &["agents", "0", "phase"], key);
    }
    insert_null(&mut leaves, &["agents", "0", "context"], "model");
    insert_null(&mut leaves, &["capacity"], "reason");
    for key in ["percentUsed", "note"] {
        insert_null(&mut leaves, &["capacity", "providers", "0"], key);
    }
    for key in ["expected", "resets"] {
        insert_null(
            &mut leaves,
            &["capacity", "providers", "0", "windows", "0"],
            key,
        );
    }
    insert_null(&mut leaves, &["localModel"], "residentGB");
    let payload: DeckPayload = serde_json::from_value(leaves)
        .unwrap_or_else(|error| panic!("explicit null leaf optionals must decode: {error}"));
    let encoded = serde_json::to_value(payload)
        .unwrap_or_else(|error| panic!("DeckPayload must encode: {error}"));
    assert_no_nulls(&encoded, "$".to_owned());

    let mut containers: Value = serde_json::from_str(CONTRACT_CAPTURE)
        .unwrap_or_else(|error| panic!("contract capture must be valid JSON: {error}"));
    insert_null(&mut containers, &[], "localModel");
    for key in [
        "focus",
        "state",
        "repliedAgo",
        "phase",
        "background",
        "activity",
        "context",
    ] {
        insert_null(&mut containers, &["agents", "0"], key);
    }
    insert_null(&mut containers, &["host"], "system");

    let payload: DeckPayload = serde_json::from_value(containers)
        .unwrap_or_else(|error| panic!("explicit null container optionals must decode: {error}"));
    let encoded = serde_json::to_value(payload)
        .unwrap_or_else(|error| panic!("DeckPayload must encode: {error}"));
    assert_no_nulls(&encoded, "$".to_owned());
    assert!(encoded.get("localModel").is_none());
    assert!(encoded["host"].get("system").is_none());
}

#[test]
fn enum_spellings_match_the_wire_contract() {
    assert_eq!(
        serde_json::to_string(&TitleSource::Model)
            .unwrap_or_else(|error| panic!("enum must encode: {error}")),
        "\"model\""
    );
    assert_eq!(
        serde_json::to_string(&TitleSource::Herdr)
            .unwrap_or_else(|error| panic!("enum must encode: {error}")),
        "\"herdr\""
    );
    assert_eq!(
        serde_json::to_string(&LocalModelStatus::Unloaded)
            .unwrap_or_else(|error| panic!("enum must encode: {error}")),
        "\"unloaded\""
    );
}

#[test]
fn additive_capabilities_have_exact_wire_names_and_omit_absent_leaves() {
    let capabilities = DeckCapabilities {
        headings: CapabilityStatus {
            state: CapabilityState::Missing,
            backend: Some(CapabilityBackend::Ollama),
            level: None,
            reason: Some(CapabilityReason::ProviderMissing),
            setup_hint: Some(SetupHint {
                message: "Install Ollama to generate contextual card headings.".to_owned(),
                action_label: "Learn more".to_owned(),
                docs_path: "docs/setup.html#contextual-card-headings".to_owned(),
                command: None,
            }),
        },
        capacity: CapabilityStatus {
            state: CapabilityState::Disabled,
            backend: None,
            level: None,
            reason: None,
            setup_hint: None,
        },
        host_telemetry: CapabilityStatus {
            state: CapabilityState::Available,
            backend: Some(CapabilityBackend::Native),
            level: Some(CapabilityLevel::Basic),
            reason: None,
            setup_hint: None,
        },
        local_model_telemetry: CapabilityStatus {
            state: CapabilityState::Unsupported,
            backend: None,
            level: None,
            reason: Some(CapabilityReason::Unsupported),
            setup_hint: None,
        },
        tab_title_sync: CapabilityStatus {
            state: CapabilityState::Error,
            backend: Some(CapabilityBackend::Herdr),
            level: None,
            reason: Some(CapabilityReason::StateWriteFailed),
            setup_hint: None,
        },
    };
    let value = serde_json::to_value(capabilities)
        .unwrap_or_else(|error| panic!("capabilities must encode: {error}"));
    assert_eq!(
        value,
        serde_json::json!({
            "headings": {
                "state": "missing",
                "backend": "ollama",
                "reason": "provider_missing",
                "setupHint": {
                    "message": "Install Ollama to generate contextual card headings.",
                    "actionLabel": "Learn more",
                    "docsPath": "docs/setup.html#contextual-card-headings"
                }
            },
            "capacity": { "state": "disabled" },
            "hostTelemetry": {
                "state": "available",
                "backend": "native",
                "level": "basic"
            },
            "localModelTelemetry": { "state": "unsupported", "reason": "unsupported" },
            "tabTitleSync": {
                "state": "error",
                "backend": "herdr",
                "reason": "state_write_failed"
            }
        })
    );
}

#[test]
fn rust_struct_serialization_is_byte_stable_for_change_suppression() {
    let payload: DeckPayload =
        serde_json::from_str(FULL).unwrap_or_else(|error| panic!("golden must decode: {error}"));
    let first = serde_json::to_vec(&payload)
        .unwrap_or_else(|error| panic!("DeckPayload must encode: {error}"));
    let second = serde_json::to_vec(&payload)
        .unwrap_or_else(|error| panic!("DeckPayload must encode: {error}"));

    assert_eq!(first, second);
}

#[test]
fn contract_fixtures_are_sanitized() {
    const FORBIDDEN: [&str; 8] = [
        "/Users/",
        "/home/",
        "github.com/",
        "Authorization:",
        "Bearer ",
        "api_key",
        "apiKey",
        "sk-",
    ];

    for fixture in [EMPTY_DEGRADED, FULL, CONTRACT_CAPTURE] {
        for forbidden in FORBIDDEN {
            assert!(
                !fixture.contains(forbidden),
                "fixture contains forbidden private marker {forbidden:?}"
            );
        }
    }
}

fn insert_null(value: &mut Value, path: &[&str], key: &str) {
    let mut cursor = value;
    for segment in path {
        cursor = match cursor {
            Value::Object(object) => object
                .get_mut(*segment)
                .unwrap_or_else(|| panic!("missing object path segment {segment}")),
            Value::Array(array) => {
                let index = segment
                    .parse::<usize>()
                    .unwrap_or_else(|error| panic!("array path must be numeric: {error}"));
                array
                    .get_mut(index)
                    .unwrap_or_else(|| panic!("missing array index {index}"))
            }
            _ => panic!("path segment {segment} does not name a container"),
        };
    }
    let object = cursor
        .as_object_mut()
        .unwrap_or_else(|| panic!("target for {key} must be an object"));
    object.insert(key.to_owned(), Value::Null);
}

fn assert_no_nulls(value: &Value, path: String) {
    match value {
        Value::Null => panic!("encoded payload contains null at {path}"),
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                assert_no_nulls(value, format!("{path}[{index}]"));
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                assert_no_nulls(value, format!("{path}.{key}"));
            }
        }
        _ => {}
    }
}

fn assert_json_semantically_equal(expected: &Value, actual: &Value, path: String) {
    match (expected, actual) {
        (Value::Number(left), Value::Number(right)) => {
            let left = left
                .as_f64()
                .unwrap_or_else(|| panic!("number is outside fixture comparison range at {path}"));
            let right = right
                .as_f64()
                .unwrap_or_else(|| panic!("number is outside fixture comparison range at {path}"));
            assert_eq!(left, right, "numeric mismatch at {path}");
        }
        (Value::Array(left), Value::Array(right)) => {
            assert_eq!(left.len(), right.len(), "array length mismatch at {path}");
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                assert_json_semantically_equal(left, right, format!("{path}[{index}]"));
            }
        }
        (Value::Object(left), Value::Object(right)) => {
            assert_eq!(
                object_keys(left),
                object_keys(right),
                "key mismatch at {path}"
            );
            for (key, left) in left {
                let right = right
                    .get(key)
                    .unwrap_or_else(|| panic!("missing key {path}.{key}"));
                assert_json_semantically_equal(left, right, format!("{path}.{key}"));
            }
        }
        _ => assert_eq!(expected, actual, "value mismatch at {path}"),
    }
}

fn object_keys(object: &Map<String, Value>) -> BTreeSet<&str> {
    object.keys().map(String::as_str).collect()
}
