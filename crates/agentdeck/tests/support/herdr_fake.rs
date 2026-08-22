use std::{
    env, ffi::OsString, fs, future::Future, io::Write, path::PathBuf, pin::Pin, process, sync::Arc,
    thread, time::Duration,
};

use agentdeck::adapters::herdr::{
    CommandOutput, CommandSpec, HerdrClient, HerdrTarget, ProcessError, ProcessRunner,
    TokioProcessRunner,
};
use serde_json::json;
use tokio::sync::OwnedSemaphorePermit;

const EXACT_CAP: usize = 64 * 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if args.first().and_then(|arg| arg.to_str()) == Some("--driver") {
        return run_driver(&args).await;
    }
    if let Some(path) = env::var_os("AGENTDECK_FAKE_RECORD") {
        let record = json!({
            "args": args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>(),
            "socket": env::var("HERDR_SOCKET_PATH").ok(),
            "session": env::var("HERDR_SESSION").ok(),
            "pid": process::id(),
        });
        fs::write(path, serde_json::to_vec(&record)?)?;
    }

    match env::var("AGENTDECK_FAKE_SCENARIO").as_deref() {
        Ok("duplex") => write_duplex()?,
        Ok("success_warning") => {
            std::io::stdout().write_all(b"valid stdout")?;
            std::io::stderr().write_all(b"synthetic warning")?;
        }
        Ok("empty_success") => {}
        Ok("silent_timeout") => thread::sleep(Duration::from_secs(60)),
        Ok("stdout_exact") => write_repeated(std::io::stdout(), b'o', EXACT_CAP)?,
        Ok("stderr_exact") => write_repeated(std::io::stderr(), b'e', EXACT_CAP)?,
        Ok("stdout_over") => write_repeated(std::io::stdout(), b'o', EXACT_CAP + 1)?,
        Ok("stderr_over") => write_repeated(std::io::stderr(), b'e', EXACT_CAP + 1)?,
        Ok("stdout_cap") => write_then_wait(false)?,
        Ok("stderr_cap") => write_then_wait(true)?,
        Ok("partial_timeout") => {
            std::io::stdout().write_all(b"partial")?;
            std::io::stdout().flush()?;
            thread::sleep(Duration::from_secs(60));
        }
        Ok("api_error") => {
            std::io::stderr().write_all(
                br#"{"id":"fake","error":{"code":"pane_not_found","message":"gone"}}"#,
            )?;
            process::exit(1);
        }
        Ok("syntax") => {
            std::io::stderr().write_all(b"usage: herdr fake")?;
            process::exit(2);
        }
        Ok("transport") => {
            std::io::stderr().write_all(b"connection refused")?;
            process::exit(1);
        }
        Ok("malformed") => std::io::stdout().write_all(b"{not-json")?,
        Ok("missing_type") => {
            std::io::stdout().write_all(br#"{"id":"fake","result":{}}"#)?;
        }
        Ok("wrong_type") => {
            std::io::stdout().write_all(br#"{"id":"fake","result":{"type":"wrong"}}"#)?;
        }
        Ok("invalid_utf8") => std::io::stdout().write_all(&[0xff, 0xfe])?,
        _ => respond_to_command(&args)?,
    }
    Ok(())
}

struct ChildRunner {
    record: PathBuf,
}

impl ProcessRunner for ChildRunner {
    fn run(
        &self,
        mut spec: CommandSpec,
        permit: OwnedSemaphorePermit,
    ) -> Pin<Box<dyn Future<Output = Result<CommandOutput, ProcessError>> + Send + '_>> {
        spec.env_remove
            .push(OsString::from("AGENTDECK_FAKE_SCENARIO"));
        spec.env_remove
            .push(OsString::from("AGENTDECK_FAKE_RECORD"));
        spec.env_set.push((
            OsString::from("AGENTDECK_FAKE_RECORD"),
            self.record.as_os_str().to_owned(),
        ));
        Box::pin(async move { TokioProcessRunner.run(spec, permit).await })
    }
}

async fn run_driver(args: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let target_kind = required_utf8(args, 1, "target kind")?;
    let target_value = required_utf8(args, 2, "target value")?;
    let record = PathBuf::from(
        args.get(3)
            .ok_or_else(|| driver_error("driver requires a record path"))?
            .to_owned(),
    );
    let operation = required_utf8(args, 4, "operation")?;
    let target = match target_kind {
        "auto" => HerdrTarget::Auto,
        "session" => HerdrTarget::session(target_value.to_owned())?,
        "socket" => HerdrTarget::socket(target_value)?,
        other => return Err(driver_error(format!("unknown driver target {other:?}")).into()),
    };
    let executable = env::current_exe()?;
    let client = HerdrClient::with_runner(executable, target, Arc::new(ChildRunner { record }));
    match operation {
        "rename" => {
            let tab_id = required_utf8(args, 5, "tab ID")?;
            let title = required_utf8(args, 6, "tab title")?;
            client.rename_tab(tab_id, title).await?;
        }
        other => {
            return Err(driver_error(format!("unknown driver operation {other:?}")).into());
        }
    }
    Ok(())
}

fn required_utf8<'a>(
    args: &'a [OsString],
    index: usize,
    label: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    args.get(index)
        .ok_or_else(|| driver_error(format!("driver requires {label}")))
        .and_then(|value| {
            value
                .to_str()
                .ok_or_else(|| driver_error(format!("driver {label} must be UTF-8")))
        })
        .map_err(Into::into)
}

fn driver_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

fn write_duplex() -> std::io::Result<()> {
    let stdout = thread::spawn(|| write_repeated(std::io::stdout(), b'o', 384 * 1024));
    let stderr = thread::spawn(|| write_repeated(std::io::stderr(), b'e', 384 * 1024));
    join_writer(stdout)?;
    join_writer(stderr)
}

fn join_writer(writer: thread::JoinHandle<std::io::Result<()>>) -> std::io::Result<()> {
    writer
        .join()
        .map_err(|_| std::io::Error::other("writer thread panicked"))?
}

fn write_repeated(mut writer: impl Write, byte: u8, count: usize) -> std::io::Result<()> {
    let chunk = vec![byte; 16 * 1024];
    for _ in 0..(count / chunk.len()) {
        writer.write_all(&chunk)?;
    }
    writer.write_all(&chunk[..count % chunk.len()])?;
    writer.flush()
}

fn write_then_wait(stderr: bool) -> std::io::Result<()> {
    if stderr {
        write_repeated(std::io::stderr(), b'e', 256 * 1024)?;
    } else {
        write_repeated(std::io::stdout(), b'o', 256 * 1024)?;
    }
    thread::sleep(Duration::from_secs(60));
    Ok(())
}

fn respond_to_command(args: &[OsString]) -> std::io::Result<()> {
    let strings: Vec<_> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let command = if strings.first().map(String::as_str) == Some("--session") {
        strings.get(2..).unwrap_or_default()
    } else {
        strings.as_slice()
    };

    let output = match command {
        [version] if version == "--version" => "herdr 0.8.2\n".to_owned(),
        [api, schema, json] if api == "api" && schema == "schema" && json == "--json" => {
            r#"{"schema_version":1,"protocol":20,"unknown":"ignored"}"#.to_owned()
        }
        [api, snapshot] if api == "api" && snapshot == "snapshot" => r#"{"id":"cli:api:snapshot","result":{"type":"session_snapshot","snapshot":{"version":"0.8.2","protocol":20,"workspaces":[],"tabs":[],"panes":[],"layouts":[],"agents":[]}}}"#.to_owned(),
        [agent, focus, _] if agent == "agent" && focus == "focus" => success("agent_info"),
        [workspace, focus, _] if workspace == "workspace" && focus == "focus" => {
            success("workspace_info")
        }
        [tab, create, workspace, _, focus]
            if tab == "tab"
                && create == "create"
                && workspace == "--workspace"
                && focus == "--focus" =>
        {
            success("tab_created")
        }
        [tab, rename, _, _] if tab == "tab" && rename == "rename" => success("tab_info"),
        [agent, read, ..] if agent == "agent" && read == "read" => "visible output".to_owned(),
        _ => {
            std::io::stderr().write_all(b"unrecognized fake command")?;
            process::exit(2);
        }
    };
    std::io::stdout().write_all(output.as_bytes())
}

fn success(kind: &str) -> String {
    json!({"id": "fake", "result": {"type": kind}}).to_string()
}
