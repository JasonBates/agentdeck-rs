use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration};

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::HerdrConfig;

use super::{
    CommandEnvelopeDto, CommandLimits, CommandOutput, CommandSpec, HerdrError, HerdrTarget,
    ProcessError, ProcessRunner, SchemaDto, SnapshotDto, TokioProcessRunner,
    dto::{ApiErrorEnvelope, SnapshotEnvelope},
    resolve_herdr_binary,
};

const VERSION_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(2),
    stdout_bytes: 64 * 1024,
    stderr_bytes: 64 * 1024,
};
const SCHEMA_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(5),
    stdout_bytes: 2 * 1024 * 1024,
    stderr_bytes: 256 * 1024,
};
const SNAPSHOT_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(12),
    stdout_bytes: 4 * 1024 * 1024,
    stderr_bytes: 256 * 1024,
};
const COMMAND_LIMITS: CommandLimits = CommandLimits {
    timeout: Duration::from_secs(12),
    stdout_bytes: 512 * 1024,
    stderr_bytes: 256 * 1024,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisibleLines {
    Background16,
    Phase40,
}

impl VisibleLines {
    const fn count(self) -> u32 {
        match self {
            Self::Background16 => 16,
            Self::Phase40 => 40,
        }
    }
}

#[derive(Clone)]
pub struct HerdrClient {
    binary: PathBuf,
    target: HerdrTarget,
    runner: Arc<dyn ProcessRunner>,
    diagnostics: Arc<Semaphore>,
    snapshots: Arc<Semaphore>,
    mutations: Arc<Semaphore>,
    reads: Arc<Semaphore>,
}

impl std::fmt::Debug for HerdrClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HerdrClient")
            .field("binary", &self.binary)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl HerdrClient {
    pub fn from_config(config: &HerdrConfig) -> Result<Self, HerdrError> {
        let target = HerdrTarget::from_config(config)?;
        let binary = resolve_herdr_binary()?;
        Ok(Self::with_runner(
            binary,
            target,
            Arc::new(TokioProcessRunner),
        ))
    }

    pub fn with_runner(
        binary: PathBuf,
        target: HerdrTarget,
        runner: Arc<dyn ProcessRunner>,
    ) -> Self {
        Self {
            binary,
            target,
            runner,
            diagnostics: Arc::new(Semaphore::new(1)),
            snapshots: Arc::new(Semaphore::new(1)),
            mutations: Arc::new(Semaphore::new(4)),
            reads: Arc::new(Semaphore::new(8)),
        }
    }

    pub fn binary(&self) -> &PathBuf {
        &self.binary
    }

    pub fn target(&self) -> &HerdrTarget {
        &self.target
    }

    pub async fn version(&self) -> Result<String, HerdrError> {
        let permit = Arc::clone(&self.diagnostics)
            .acquire_owned()
            .await
            .map_err(|_| HerdrError::LimiterClosed {
                name: "Herdr diagnostics",
            })?;
        let output = self
            .run_unrouted("herdr --version", ["--version"], VERSION_LIMITS, permit)
            .await?;
        parse_version(&output.stdout)
    }

    pub async fn schema(&self) -> Result<SchemaDto, HerdrError> {
        let permit = Arc::clone(&self.diagnostics)
            .acquire_owned()
            .await
            .map_err(|_| HerdrError::LimiterClosed {
                name: "Herdr diagnostics",
            })?;
        let output = self
            .run_unrouted(
                "herdr api schema --json",
                ["api", "schema", "--json"],
                SCHEMA_LIMITS,
                permit,
            )
            .await?;
        serde_json::from_slice(&output.stdout).map_err(|source| HerdrError::MalformedJson {
            command: "herdr api schema --json",
            source,
        })
    }

    pub async fn snapshot(&self) -> Result<SnapshotDto, HerdrError> {
        let permit = Arc::clone(&self.snapshots)
            .acquire_owned()
            .await
            .map_err(|_| HerdrError::LimiterClosed {
                name: "Herdr snapshots",
            })?;
        let output = self
            .run_routed(
                "herdr api snapshot",
                [OsString::from("api"), OsString::from("snapshot")],
                SNAPSHOT_LIMITS,
                permit,
            )
            .await?;
        let value = parse_json_value("herdr api snapshot", &output.stdout)?;
        require_result_type("herdr api snapshot", &value, "session_snapshot")?;
        let envelope: SnapshotEnvelope =
            serde_json::from_value(value).map_err(|source| HerdrError::MalformedJson {
                command: "herdr api snapshot",
                source,
            })?;
        let _request_id = envelope.id;
        let _kind = envelope.result.kind;
        Ok(envelope.result.snapshot)
    }

    pub async fn focus_pane(&self, pane_id: &str) -> Result<CommandEnvelopeDto, HerdrError> {
        self.mutation(
            "herdr agent focus",
            ["agent", "focus", pane_id],
            "agent_info",
        )
        .await
    }

    pub async fn focus_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<CommandEnvelopeDto, HerdrError> {
        self.mutation(
            "herdr workspace focus",
            ["workspace", "focus", workspace_id],
            "workspace_info",
        )
        .await
    }

    pub async fn create_focused_tab(
        &self,
        workspace_id: &str,
    ) -> Result<CommandEnvelopeDto, HerdrError> {
        self.mutation(
            "herdr tab create",
            ["tab", "create", "--workspace", workspace_id, "--focus"],
            "tab_created",
        )
        .await
    }

    pub async fn rename_tab(
        &self,
        tab_id: &str,
        title: &str,
    ) -> Result<CommandEnvelopeDto, HerdrError> {
        self.mutation(
            "herdr tab rename",
            ["tab", "rename", tab_id, title],
            "tab_info",
        )
        .await
    }

    pub async fn read_visible(
        &self,
        pane_id: &str,
        lines: VisibleLines,
    ) -> Result<String, HerdrError> {
        let permit = Arc::clone(&self.reads).acquire_owned().await.map_err(|_| {
            HerdrError::LimiterClosed {
                name: "Herdr visible reads",
            }
        })?;
        let output = self
            .run_routed(
                "herdr agent read",
                [
                    OsString::from("agent"),
                    OsString::from("read"),
                    OsString::from(pane_id),
                    OsString::from("--source"),
                    OsString::from("visible"),
                    OsString::from("--lines"),
                    OsString::from(lines.count().to_string()),
                    OsString::from("--format"),
                    OsString::from("text"),
                ],
                COMMAND_LIMITS,
                permit,
            )
            .await?;
        String::from_utf8(output.stdout).map_err(|_| HerdrError::InvalidUtf8 {
            command: "herdr agent read",
        })
    }

    async fn mutation<const N: usize>(
        &self,
        command: &'static str,
        args: [&str; N],
        expected: &'static str,
    ) -> Result<CommandEnvelopeDto, HerdrError> {
        let permit = Arc::clone(&self.mutations)
            .acquire_owned()
            .await
            .map_err(|_| HerdrError::LimiterClosed {
                name: "Herdr mutations",
            })?;
        let output = self
            .run_routed(
                command,
                args.into_iter().map(OsString::from),
                COMMAND_LIMITS,
                permit,
            )
            .await?;
        let value = parse_json_value(command, &output.stdout)?;
        require_result_type(command, &value, expected)?;
        serde_json::from_value(value)
            .map_err(|source| HerdrError::MalformedJson { command, source })
    }

    async fn run_unrouted<const N: usize>(
        &self,
        label: &str,
        args: [&str; N],
        limits: CommandLimits,
        permit: OwnedSemaphorePermit,
    ) -> Result<CommandOutput, ProcessError> {
        self.runner
            .run(
                CommandSpec {
                    executable: self.binary.clone(),
                    args: args.into_iter().map(OsString::from).collect(),
                    env_set: Vec::new(),
                    env_remove: Vec::new(),
                    limits,
                    label: label.to_owned(),
                },
                permit,
            )
            .await
    }

    async fn run_routed(
        &self,
        label: &str,
        args: impl IntoIterator<Item = OsString>,
        limits: CommandLimits,
        permit: OwnedSemaphorePermit,
    ) -> Result<CommandOutput, ProcessError> {
        let routed = self.target.route(args);
        self.runner
            .run(
                CommandSpec {
                    executable: self.binary.clone(),
                    args: routed.args,
                    env_set: routed.env_set,
                    env_remove: routed.env_remove,
                    limits,
                    label: label.to_owned(),
                },
                permit,
            )
            .await
    }
}

fn parse_version(bytes: &[u8]) -> Result<String, HerdrError> {
    let text = std::str::from_utf8(bytes).map_err(|_| HerdrError::InvalidUtf8 {
        command: "herdr --version",
    })?;
    let output = text.trim();
    let version = output
        .strip_prefix("herdr ")
        .filter(|version| !version.is_empty() && !version.chars().any(char::is_whitespace))
        .ok_or_else(|| HerdrError::InvalidVersion {
            output: output.to_owned(),
        })?;
    Ok(version.to_owned())
}

fn parse_json_value(command: &'static str, bytes: &[u8]) -> Result<Value, HerdrError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|source| HerdrError::MalformedJson { command, source })?;
    if value.get("error").is_some() {
        let envelope: ApiErrorEnvelope = serde_json::from_value(value)
            .map_err(|source| HerdrError::MalformedJson { command, source })?;
        return Err(ProcessError::Api {
            status: 0,
            id: envelope.id,
            code: envelope.error.code,
            message: envelope.error.message,
        }
        .into());
    }
    Ok(value)
}

fn require_result_type(
    command: &'static str,
    value: &Value,
    expected: &'static str,
) -> Result<(), HerdrError> {
    let actual = value
        .pointer("/result/type")
        .and_then(Value::as_str)
        .ok_or(HerdrError::MissingResultType { command })?;
    if actual == expected {
        Ok(())
    } else {
        Err(HerdrError::UnexpectedResultType {
            command,
            expected,
            actual: actual.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn version_parser_requires_the_exact_cli_shape() {
        assert_eq!(
            parse_version(b"herdr 0.8.2\n")
                .unwrap_or_else(|error| panic!("valid version rejected: {error}")),
            "0.8.2"
        );
        for invalid in [b"0.8.2\n".as_slice(), b"herdr\n", b"herdr 0.8.2 extra\n"] {
            assert!(parse_version(invalid).is_err());
        }
    }
}
