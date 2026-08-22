use std::{env, process::ExitCode};

use agentdeck::{
    cli::{Cli, Command, ConfigCommand, ServeArgs},
    config::Config,
    config_init::{ConfigInitOptions, ConfigInitOutcome, initialize_config},
    doctor,
    paths::default_config_file,
    runtime,
};
use anyhow::Result;
use clap::Parser;
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Version { json }) => print_version(json),
        Some(Command::Config {
            command: ConfigCommand::Print,
        }) => {
            let config = effective_config(cli.config.as_deref(), None)?;
            print!("{}", config.redacted_toml()?);
            Ok(())
        }
        Some(Command::Config {
            command: ConfigCommand::Init(args),
        }) => {
            let path = match cli.config {
                Some(path) => path,
                None => default_config_file()?,
            };
            match initialize_config(&ConfigInitOptions {
                path,
                force: args.force,
                stdout: args.stdout,
            })? {
                ConfigInitOutcome::Printed { contents } => print!("{contents}"),
                ConfigInitOutcome::Written { path } => {
                    println!("wrote {}", path.display());
                }
            }
            Ok(())
        }
        Some(Command::Doctor { json }) => {
            let report = doctor::inspect(cli.config.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", doctor::render_human(&report));
            }
            Ok(())
        }
        Some(Command::Serve(args)) => {
            let config = effective_config(cli.config.as_deref(), Some(&args))?;
            runtime::serve(&config).await
        }
        None => {
            let args = ServeArgs::default();
            let config = effective_config(cli.config.as_deref(), Some(&args))?;
            runtime::serve(&config).await
        }
    }
}

fn effective_config(path: Option<&std::path::Path>, args: Option<&ServeArgs>) -> Result<Config> {
    let default_path;
    let path = match path {
        Some(path) => path,
        None => {
            default_path = default_config_file()?;
            &default_path
        }
    };
    let mut config = Config::read(path)?;
    config.apply_environment(|key| env::var(key).ok())?;
    if let Some(args) = args {
        config.apply_serve_args(args)?;
    }
    config.validate()?;
    Ok(config)
}

fn print_version(as_json: bool) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    if as_json {
        println!("{}", serde_json::to_string(&json!({ "version": version }))?);
    } else {
        println!("agentdeck {version}");
    }
    Ok(())
}
