use std::{io, path::PathBuf, time::Duration};

use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl std::fmt::Display for OutputStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdout => formatter.write_str("stdout"),
            Self::Stderr => formatter.write_str("stderr"),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("herdr executable was not found (last candidate: {candidate})")]
    NotFound { candidate: PathBuf },

    #[error("could not spawn {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not inspect {command}: {source}")]
    Inspect {
        command: String,
        #[source]
        source: io::Error,
    },

    #[error("{command} timed out after {timeout:?}")]
    Timeout { command: String, timeout: Duration },

    #[error("{command} was cancelled")]
    Cancelled { command: String },

    #[error("{command} exceeded its {stream} limit of {limit} bytes")]
    OutputLimit {
        command: String,
        stream: OutputStream,
        limit: usize,
    },

    #[error("Herdr API error {code}: {message}")]
    Api {
        status: i32,
        id: Option<String>,
        code: String,
        message: String,
    },

    #[error("Herdr CLI syntax error: {message}")]
    Syntax { message: String },

    #[error("Herdr transport failed with status {status:?}: {message}")]
    Transport {
        status: Option<i32>,
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum HerdrError {
    #[error(transparent)]
    Process(#[from] ProcessError),

    #[error("invalid Herdr session {session:?}: {message}")]
    InvalidSession {
        session: String,
        message: &'static str,
    },

    #[error("invalid Herdr socket {socket:?}: {message}")]
    InvalidSocket {
        socket: PathBuf,
        message: &'static str,
    },

    #[error("herdr.session and herdr.socket are mutually exclusive")]
    ConflictingTargets,

    #[error("could not decode {command} JSON: {source}")]
    MalformedJson {
        command: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("{command} output was not valid UTF-8")]
    InvalidUtf8 { command: &'static str },

    #[error("{command} response is missing result.type")]
    MissingResultType { command: &'static str },

    #[error("{command} returned result.type={actual:?}; expected {expected:?}")]
    UnexpectedResultType {
        command: &'static str,
        expected: &'static str,
        actual: String,
    },

    #[error("invalid Herdr version output: {output:?}")]
    InvalidVersion { output: String },

    #[error("the {name} concurrency limiter was closed")]
    LimiterClosed { name: &'static str },
}
