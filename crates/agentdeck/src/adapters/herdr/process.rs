use std::{
    ffi::OsString, future::Future, io, path::PathBuf, pin::Pin, process::Stdio, time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::{Child, Command},
    sync::{OwnedSemaphorePermit, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

use super::{OutputStream, ProcessError, dto::ApiErrorEnvelope};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandLimits {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub env_set: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub limits: CommandLimits,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub type ProcessFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommandOutput, ProcessError>> + Send + 'a>>;

pub trait ProcessRunner: Send + Sync {
    /// Run one admitted command. Implementations must retain `permit` until the
    /// external process and all owned I/O work have terminated.
    fn run(&self, spec: CommandSpec, permit: OwnedSemaphorePermit) -> ProcessFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioProcessRunner;

impl ProcessRunner for TokioProcessRunner {
    fn run(&self, spec: CommandSpec, permit: OwnedSemaphorePermit) -> ProcessFuture<'_> {
        Box::pin(async move {
            let label = spec.label.clone();
            let (cancel_tx, cancel_rx) = oneshot::channel();
            let mut cancel = CancelOnDrop(Some(cancel_tx));
            let supervisor = tokio::spawn(run_process(spec, cancel_rx, permit));
            let result = supervisor.await.map_err(|source| ProcessError::Inspect {
                command: label,
                source: io::Error::other(format!("process supervisor failed: {source}")),
            })?;
            cancel.0.take();
            result
        })
    }
}

struct CancelOnDrop(Option<oneshot::Sender<()>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct ReaderMessage {
    stream: OutputStream,
    result: Result<Vec<u8>, ReaderError>,
}

enum ReaderError {
    Io(io::Error),
    Limit,
}

async fn run_process(
    spec: CommandSpec,
    mut cancellation: oneshot::Receiver<()>,
    _permit: OwnedSemaphorePermit,
) -> Result<CommandOutput, ProcessError> {
    let deadline = Instant::now()
        .checked_add(spec.limits.timeout)
        .ok_or_else(|| ProcessError::Inspect {
            command: spec.label.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "command timeout is too large"),
        })?;
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &spec.env_set {
        command.env(name, value);
    }

    let mut child = command.spawn().map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ProcessError::NotFound {
                candidate: spec.executable.clone(),
            }
        } else {
            ProcessError::Spawn {
                executable: spec.executable.clone(),
                source,
            }
        }
    })?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_and_reap(&mut child, &spec.label).await?;
            return Err(ProcessError::Inspect {
                command: spec.label,
                source: io::Error::other("spawned process has no stdout pipe"),
            });
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_and_reap(&mut child, &spec.label).await?;
            return Err(ProcessError::Inspect {
                command: spec.label,
                source: io::Error::other("spawned process has no stderr pipe"),
            });
        }
    };

    let (reader_tx, mut reader_rx) = mpsc::unbounded_channel();
    let mut stdout_task = spawn_reader(
        stdout,
        OutputStream::Stdout,
        spec.limits.stdout_bytes,
        reader_tx.clone(),
    );
    let mut stderr_task = spawn_reader(
        stderr,
        OutputStream::Stderr,
        spec.limits.stderr_bytes,
        reader_tx,
    );

    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    while status.is_none() || stdout.is_none() || stderr.is_none() {
        let now = Instant::now();
        if now >= deadline {
            terminate_and_abort_readers(
                &mut child,
                &spec.label,
                &mut stdout_task,
                &mut stderr_task,
            )
            .await?;
            return Err(ProcessError::Timeout {
                command: spec.label,
                timeout: spec.limits.timeout,
            });
        }

        let remaining = deadline.saturating_duration_since(now);
        tokio::select! {
            _ = &mut cancellation => {
                terminate_and_abort_readers(
                    &mut child,
                    &spec.label,
                    &mut stdout_task,
                    &mut stderr_task,
                )
                .await?;
                return Err(ProcessError::Cancelled {
                    command: spec.label,
                });
            }
            message = reader_rx.recv(), if stdout.is_none() || stderr.is_none() => {
                let Some(message) = message else {
                    terminate_and_abort_readers(
                        &mut child,
                        &spec.label,
                        &mut stdout_task,
                        &mut stderr_task,
                    )
                    .await?;
                    return Err(ProcessError::Inspect {
                        command: spec.label,
                        source: io::Error::other("process output reader stopped unexpectedly"),
                    });
                };
                match message.result {
                    Ok(bytes) => match message.stream {
                        OutputStream::Stdout => stdout = Some(bytes),
                        OutputStream::Stderr => stderr = Some(bytes),
                    },
                    Err(ReaderError::Limit) => {
                        terminate_and_abort_readers(
                            &mut child,
                            &spec.label,
                            &mut stdout_task,
                            &mut stderr_task,
                        )
                        .await?;
                        let limit = match message.stream {
                            OutputStream::Stdout => spec.limits.stdout_bytes,
                            OutputStream::Stderr => spec.limits.stderr_bytes,
                        };
                        return Err(ProcessError::OutputLimit {
                            command: spec.label,
                            stream: message.stream,
                            limit,
                        });
                    }
                    Err(ReaderError::Io(source)) => {
                        terminate_and_abort_readers(
                            &mut child,
                            &spec.label,
                            &mut stdout_task,
                            &mut stderr_task,
                        )
                        .await?;
                        return Err(ProcessError::Inspect {
                            command: spec.label,
                            source,
                        });
                    }
                }
            }
            () = tokio::time::sleep(remaining.min(Duration::from_millis(10))) => {}
        }

        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(source) => {
                    let primary = ProcessError::Inspect {
                        command: spec.label.clone(),
                        source,
                    };
                    let cleanup = force_terminate_and_abort_readers(
                        &mut child,
                        &spec.label,
                        &mut stdout_task,
                        &mut stderr_task,
                    )
                    .await;
                    return Err(match cleanup {
                        Ok(()) => primary,
                        Err(cleanup) => cleanup_failure(&spec.label, primary, cleanup),
                    });
                }
            };
        }
    }

    // Both readers have completed and the child was observed through try_wait,
    // which reaps it on supported platforms. Awaiting the tasks also surfaces a
    // panic rather than leaving detached test/runtime work behind.
    finish_reader(stdout_task, &spec.label).await?;
    finish_reader(stderr_task, &spec.label).await?;
    let status = status.ok_or_else(|| ProcessError::Inspect {
        command: spec.label.clone(),
        source: io::Error::other("process completed without an exit status"),
    })?;
    let stdout = stdout.ok_or_else(|| ProcessError::Inspect {
        command: spec.label.clone(),
        source: io::Error::other("stdout reader did not complete"),
    })?;
    let stderr = stderr.ok_or_else(|| ProcessError::Inspect {
        command: spec.label.clone(),
        source: io::Error::other("stderr reader did not complete"),
    })?;

    if status.success() {
        return Ok(CommandOutput { stdout, stderr });
    }
    Err(classify_exit(status.code(), &stdout, &stderr))
}

fn spawn_reader(
    reader: impl AsyncRead + Unpin + Send + 'static,
    stream: OutputStream,
    limit: usize,
    sender: mpsc::UnboundedSender<ReaderMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = read_capped(reader, limit).await;
        let _ = sender.send(ReaderMessage { stream, result });
    })
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<Vec<u8>, ReaderError> {
    let mut output = Vec::with_capacity(limit.min(16 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut chunk).await.map_err(ReaderError::Io)?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > limit {
            return Err(ReaderError::Limit);
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

async fn terminate_and_reap(child: &mut Child, label: &str) -> Result<(), ProcessError> {
    match child.try_wait() {
        Ok(Some(_)) => child
            .wait()
            .await
            .map(|_| ())
            .map_err(|source| ProcessError::Inspect {
                command: label.to_owned(),
                source,
            }),
        Ok(None) => force_terminate_and_reap(child, label).await,
        Err(source) => {
            let primary = ProcessError::Inspect {
                command: label.to_owned(),
                source,
            };
            match force_terminate_and_reap(child, label).await {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(cleanup_failure(label, primary, cleanup)),
            }
        }
    }
}

async fn terminate_and_abort_readers(
    child: &mut Child,
    label: &str,
    stdout: &mut JoinHandle<()>,
    stderr: &mut JoinHandle<()>,
) -> Result<(), ProcessError> {
    let termination = terminate_and_reap(child, label).await;
    abort_readers(stdout, stderr).await;
    termination
}

/// `try_wait` itself failed, so do not probe it again before attempting cleanup.
/// Waiting after `start_kill` is what reaps the child; reader tasks are joined even
/// when process cleanup reports an error.
async fn force_terminate_and_abort_readers(
    child: &mut Child,
    label: &str,
    stdout: &mut JoinHandle<()>,
    stderr: &mut JoinHandle<()>,
) -> Result<(), ProcessError> {
    let termination = force_terminate_and_reap(child, label).await;
    abort_readers(stdout, stderr).await;
    termination
}

async fn force_terminate_and_reap(child: &mut Child, label: &str) -> Result<(), ProcessError> {
    let kill_error = child.start_kill().err();
    match child.wait().await {
        Ok(_) => Ok(()),
        Err(wait_error) => {
            let source = match kill_error {
                Some(kill_error) => io::Error::other(format!(
                    "could not kill process: {kill_error}; could not reap process: {wait_error}"
                )),
                None => wait_error,
            };
            Err(ProcessError::Inspect {
                command: label.to_owned(),
                source,
            })
        }
    }
}

fn cleanup_failure(label: &str, primary: ProcessError, cleanup: ProcessError) -> ProcessError {
    ProcessError::Inspect {
        command: label.to_owned(),
        source: io::Error::other(format!("{primary}; cleanup also failed: {cleanup}")),
    }
}

async fn abort_readers(stdout: &mut JoinHandle<()>, stderr: &mut JoinHandle<()>) {
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
}

async fn finish_reader(reader: JoinHandle<()>, label: &str) -> Result<(), ProcessError> {
    reader.await.map_err(|source| ProcessError::Inspect {
        command: label.to_owned(),
        source: io::Error::other(format!("process output reader failed: {source}")),
    })
}

fn classify_exit(status: Option<i32>, stdout: &[u8], stderr: &[u8]) -> ProcessError {
    let message_bytes = if stderr.is_empty() { stdout } else { stderr };
    let message = String::from_utf8_lossy(message_bytes).trim().to_owned();
    if status == Some(1) {
        if let Ok(envelope) = serde_json::from_slice::<ApiErrorEnvelope>(message_bytes) {
            return ProcessError::Api {
                status: 1,
                id: envelope.id,
                code: envelope.error.code,
                message: envelope.error.message,
            };
        }
    }
    if status == Some(2) {
        return ProcessError::Syntax { message };
    }
    ProcessError::Transport { status, message }
}

#[cfg(test)]
mod tests {
    use super::classify_exit;
    use crate::adapters::herdr::ProcessError;

    #[test]
    fn exit_one_api_json_is_typed() {
        let error = classify_exit(
            Some(1),
            b"",
            br#"{"id":"req","error":{"code":"pane_not_found","message":"gone"}}"#,
        );
        assert!(matches!(
            error,
            ProcessError::Api { status: 1, id: Some(id), code, message }
                if id == "req" && code == "pane_not_found" && message == "gone"
        ));
    }

    #[test]
    fn syntax_and_transport_errors_remain_distinct() {
        assert!(matches!(
            classify_exit(Some(2), b"", b"usage: herdr"),
            ProcessError::Syntax { message } if message == "usage: herdr"
        ));
        assert!(matches!(
            classify_exit(Some(1), b"", b"connection refused"),
            ProcessError::Transport { status: Some(1), message }
                if message == "connection refused"
        ));
    }
}
