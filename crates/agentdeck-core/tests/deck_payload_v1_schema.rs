use std::collections::BTreeSet;

use serde_json::Value;

const SCHEMA: &str = include_str!("../../../schemas/deck-payload-v1.schema.json");
const EMPTY_DEGRADED: &str = include_str!("../../../Tests/golden/deck/empty-degraded.json");
const FULL: &str = include_str!("../../../Tests/golden/deck/full.json");
const CONTRACT_CAPTURE: &str =
    include_str!("../../../Tests/fixtures/contract/deck-payload-v1-sanitized.json");

#[test]
fn schema_is_valid_draft_2020_12_and_requires_only_top_level_non_optionals() {
    let schema = parse(SCHEMA, "schema");
    jsonschema::draft202012::meta::validate(&schema).unwrap_or_else(|error| {
        panic!("schema must satisfy the Draft 2020-12 meta-schema: {error}")
    });
    let required = schema["required"]
        .as_array()
        .unwrap_or_else(|| panic!("schema required must be an array"))
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .unwrap_or_else(|| panic!("required entry must be a string"))
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(
        required,
        BTreeSet::from(["agents", "capacity", "herdr", "host", "workspaces"])
    );
    assert_eq!(schema["additionalProperties"], Value::Bool(false));
}

#[test]
fn every_contract_fixture_validates_with_draft_2020_12() {
    let schema = parse(SCHEMA, "schema");
    let validator = jsonschema::draft202012::new(&schema)
        .unwrap_or_else(|error| panic!("Draft 2020-12 schema must compile: {error}"));

    for (name, fixture) in [
        ("empty-degraded", EMPTY_DEGRADED),
        ("full", FULL),
        ("contract-capture", CONTRACT_CAPTURE),
    ] {
        let instance = parse(fixture, name);
        if let Err(error) = validator.validate(&instance) {
            panic!("{name} must validate against DeckPayload v1: {error}");
        }
    }
}

#[test]
fn optional_properties_are_non_null_when_present() {
    let schema = parse(SCHEMA, "schema");
    let validator = jsonschema::draft202012::new(&schema)
        .unwrap_or_else(|error| panic!("Draft 2020-12 schema must compile: {error}"));
    let mut instance = parse(EMPTY_DEGRADED, "empty-degraded");
    instance
        .as_object_mut()
        .unwrap_or_else(|| panic!("fixture root must be an object"))
        .insert("localModel".to_owned(), Value::Null);

    assert!(validator.validate(&instance).is_err());
}

#[test]
fn every_nested_optional_property_may_be_omitted() {
    let schema = parse(SCHEMA, "schema");
    let validator = jsonschema::draft202012::new(&schema)
        .unwrap_or_else(|error| panic!("Draft 2020-12 schema must compile: {error}"));
    let mut instance = parse(FULL, "full");

    remove(&mut instance, &["herdr"], "detail");
    for key in ["focus", "state", "repliedAgo", "background", "activity"] {
        remove(&mut instance, &["agents", "0"], key);
    }
    for key in ["elapsed", "tokens"] {
        remove(&mut instance, &["agents", "0", "phase"], key);
    }
    remove(&mut instance, &["agents", "0", "context"], "model");
    remove(&mut instance, &["capacity"], "reason");
    for key in ["percentUsed", "note"] {
        remove(&mut instance, &["capacity", "providers", "0"], key);
    }
    for key in ["expected", "resets"] {
        remove(
            &mut instance,
            &["capacity", "providers", "0", "windows", "0"],
            key,
        );
    }
    remove(&mut instance, &["localModel"], "residentGB");

    if let Err(error) = validator.validate(&instance) {
        panic!("all nested v1 optional properties must be omittable: {error}");
    }
}

#[test]
fn schema_preserves_wire_acronym_spellings() {
    let schema = parse(SCHEMA, "schema");
    let system = &schema["$defs"]["systemSnapshot"]["properties"];

    for key in [
        "ramUsedGB",
        "ramTotalGB",
        "compressorGB",
        "swapUsedGB",
        "swapTotalGB",
        "agentRSSGB",
    ] {
        assert!(system.get(key).is_some(), "missing exact wire key {key}");
    }
}

#[test]
fn additive_capability_payload_validates_and_keeps_optional_fields_nonnullable() {
    let schema = parse(SCHEMA, "schema");
    let validator = jsonschema::draft202012::new(&schema)
        .unwrap_or_else(|error| panic!("Draft 2020-12 schema must compile: {error}"));
    let mut instance = parse(EMPTY_DEGRADED, "empty-degraded");
    instance
        .as_object_mut()
        .unwrap_or_else(|| panic!("fixture root must be an object"))
        .insert(
            "capabilities".to_owned(),
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
                "hostTelemetry": { "state": "available", "level": "basic" },
                "localModelTelemetry": { "state": "unsupported" },
                "tabTitleSync": { "state": "error", "reason": "state_write_failed" }
            }),
        );
    if let Err(error) = validator.validate(&instance) {
        panic!("additive capabilities must validate: {error}");
    }

    instance["capabilities"]["headings"]["setupHint"]["command"] = Value::Null;
    assert!(validator.validate(&instance).is_err());
}

fn parse(input: &str, name: &str) -> Value {
    serde_json::from_str(input).unwrap_or_else(|error| panic!("{name} must be valid JSON: {error}"))
}

fn remove(value: &mut Value, path: &[&str], key: &str) {
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
    let _ = object.remove(key);
}
