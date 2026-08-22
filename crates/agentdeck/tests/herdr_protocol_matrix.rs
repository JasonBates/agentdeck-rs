use agentdeck::adapters::herdr::{
    AgentDto, AgentSessionDto, SchemaDto, SnapshotDto, TabDto, WorkspaceDto, WorktreeDto,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const P19_SCHEMA: &str = include_str!("../../../Tests/fixtures/herdr/protocol-19-schema.json");
const P20_SCHEMA: &str = include_str!("../../../Tests/fixtures/herdr/protocol-20-schema.json");
const P19_SNAPSHOT: &str = include_str!("../../../Tests/fixtures/herdr/protocol-19-snapshot.json");
const P20_SNAPSHOT: &str = include_str!("../../../Tests/fixtures/herdr/protocol-20-snapshot.json");

#[test]
fn protocol_schema_subsets_decode_table_driven_and_preserve_audited_headers() {
    for (name, fixture, protocol) in [
        ("protocol 19", P19_SCHEMA, 19),
        ("protocol 20", P20_SCHEMA, 20),
    ] {
        let schema: SchemaDto = serde_json::from_str(fixture)
            .unwrap_or_else(|error| panic!("{name} schema DTO failed: {error}"));
        assert_eq!(schema.schema_version, 1, "{name}");
        assert_eq!(schema.protocol, protocol, "{name}");

        let value: Value = serde_json::from_str(fixture)
            .unwrap_or_else(|error| panic!("{name} schema JSON failed: {error}"));
        assert_eq!(
            value["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(value["title"], "Herdr API");
        assert_eq!(
            value["schemas"]["success_response"]["$defs"]["SessionSnapshot"]["required"],
            json!([
                "version",
                "protocol",
                "workspaces",
                "tabs",
                "panes",
                "layouts",
                "agents"
            ]),
            "{name}"
        );
    }
}

#[test]
fn every_audited_required_dto_field_rejects_deletion_and_null() {
    assert_required::<SnapshotDto>(
        "SnapshotDto",
        snapshot_value(),
        &[
            "version",
            "protocol",
            "agents",
            "workspaces",
            "tabs",
            "panes",
            "layouts",
        ],
    );
    assert_required::<AgentDto>(
        "AgentDto",
        agent_value(),
        &[
            "terminal_id",
            "agent_status",
            "focused",
            "pane_id",
            "tab_id",
            "workspace_id",
            "revision",
        ],
    );
    assert_required::<AgentSessionDto>(
        "AgentSessionDto",
        session_value(),
        &["source", "agent", "kind", "value"],
    );
    assert_required::<WorkspaceDto>(
        "WorkspaceDto",
        workspace_value(),
        &[
            "workspace_id",
            "label",
            "number",
            "agent_status",
            "focused",
            "pane_count",
            "tab_count",
            "active_tab_id",
        ],
    );
    assert_required::<TabDto>(
        "TabDto",
        tab_value(),
        &[
            "tab_id",
            "workspace_id",
            "label",
            "number",
            "agent_status",
            "focused",
            "pane_count",
        ],
    );
    assert_required::<WorktreeDto>(
        "WorktreeDto",
        worktree_value(),
        &[
            "repo_key",
            "repo_name",
            "repo_root",
            "checkout_path",
            "is_linked_worktree",
        ],
    );
}

#[test]
fn audited_nullable_optionals_accept_both_omission_and_null() {
    for field in ["focused_pane_id", "focused_tab_id", "focused_workspace_id"] {
        assert_optional_nullable::<SnapshotDto>("SnapshotDto", snapshot_value(), field);
    }
    for field in ["agent", "agent_session", "cwd", "terminal_title_stripped"] {
        assert_optional_nullable::<AgentDto>("AgentDto", agent_value(), field);
    }
    assert_optional_nullable::<WorkspaceDto>("WorkspaceDto", workspace_value(), "worktree");
}

#[test]
fn optional_nonnullable_state_sequence_accepts_omission_but_rejects_null() {
    let mut omitted = agent_value();
    remove_field(&mut omitted, "state_change_seq");
    assert!(serde_json::from_value::<AgentDto>(omitted).is_ok());

    let mut null = agent_value();
    set_field(&mut null, "state_change_seq", Value::Null);
    assert!(serde_json::from_value::<AgentDto>(null).is_err());
}

#[test]
fn all_herdr_schema_and_snapshot_fixtures_are_sanitized_json() {
    for (name, fixture) in [
        ("protocol-19-schema", P19_SCHEMA),
        ("protocol-20-schema", P20_SCHEMA),
        ("protocol-19-snapshot", P19_SNAPSHOT),
        ("protocol-20-snapshot", P20_SNAPSHOT),
    ] {
        serde_json::from_str::<Value>(fixture)
            .unwrap_or_else(|error| panic!("{name} is not JSON: {error}"));
        for forbidden in [
            "/Users/",
            "/home/",
            "C:\\Users\\",
            "github.com/",
            "Authorization:",
            "Bearer ",
            "api_key",
            "apiKey",
            "sk-",
            "ghp_",
            "xox",
        ] {
            assert!(
                !fixture.contains(forbidden),
                "{name} contains forbidden marker {forbidden:?}"
            );
        }
    }
}

fn assert_required<T: DeserializeOwned>(name: &str, base: Value, fields: &[&str]) {
    serde_json::from_value::<T>(base.clone())
        .unwrap_or_else(|error| panic!("valid {name} baseline failed: {error}"));
    for field in fields {
        let mut deleted = base.clone();
        remove_field(&mut deleted, field);
        assert!(
            serde_json::from_value::<T>(deleted).is_err(),
            "{name} accepted deleted required field {field}"
        );

        let mut null = base.clone();
        set_field(&mut null, field, Value::Null);
        assert!(
            serde_json::from_value::<T>(null).is_err(),
            "{name} accepted null required field {field}"
        );
    }
}

fn assert_optional_nullable<T: DeserializeOwned>(name: &str, base: Value, field: &str) {
    let mut omitted = base.clone();
    remove_field(&mut omitted, field);
    assert!(
        serde_json::from_value::<T>(omitted).is_ok(),
        "{name} rejected omitted optional field {field}"
    );

    let mut null = base;
    set_field(&mut null, field, Value::Null);
    assert!(
        serde_json::from_value::<T>(null).is_ok(),
        "{name} rejected null optional field {field}"
    );
}

fn remove_field(value: &mut Value, field: &str) {
    value
        .as_object_mut()
        .unwrap_or_else(|| panic!("matrix baseline must be an object"))
        .remove(field);
}

fn set_field(value: &mut Value, field: &str, replacement: Value) {
    value
        .as_object_mut()
        .unwrap_or_else(|| panic!("matrix baseline must be an object"))
        .insert(field.to_owned(), replacement);
}

fn snapshot_value() -> Value {
    json!({
        "version": "0.8.2",
        "protocol": 20,
        "agents": [],
        "workspaces": [],
        "tabs": [],
        "panes": [],
        "layouts": [],
        "focused_pane_id": "w1:p1",
        "focused_tab_id": "w1:t1",
        "focused_workspace_id": "w1"
    })
}

fn agent_value() -> Value {
    json!({
        "terminal_id": "terminal-1",
        "agent": "claude",
        "agent_session": session_value(),
        "agent_status": "idle",
        "cwd": "/workspace/example",
        "focused": false,
        "pane_id": "w1:p1",
        "tab_id": "w1:t1",
        "workspace_id": "w1",
        "terminal_title_stripped": "Example",
        "state_change_seq": 4,
        "revision": 8
    })
}

fn session_value() -> Value {
    json!({
        "source": "hook",
        "agent": "claude",
        "kind": "id",
        "value": "synthetic-session"
    })
}

fn workspace_value() -> Value {
    json!({
        "workspace_id": "w1",
        "label": "Example",
        "number": 1,
        "agent_status": "idle",
        "focused": false,
        "pane_count": 1,
        "tab_count": 1,
        "active_tab_id": "w1:t1",
        "worktree": worktree_value()
    })
}

fn tab_value() -> Value {
    json!({
        "tab_id": "w1:t1",
        "workspace_id": "w1",
        "label": "Example",
        "number": 1,
        "agent_status": "idle",
        "focused": false,
        "pane_count": 1
    })
}

fn worktree_value() -> Value {
    json!({
        "repo_key": "repo:example",
        "repo_name": "example",
        "repo_root": "/workspace/example",
        "checkout_path": "/workspace/example",
        "is_linked_worktree": false
    })
}
