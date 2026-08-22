use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agentdeck",
    version,
    about = "Portable browser dashboard for Herdr coding agents"
)]
pub struct Cli {
    /// Read configuration from this file instead of the platform default.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the AgentDeck HTTP/SSE bridge.
    Serve(ServeArgs),
    /// Inspect dependencies and runtime compatibility.
    Doctor {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect AgentDeck configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Print build version information.
    Version {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args, Default)]
pub struct ServeArgs {
    /// Override only the port in server.listen.
    #[arg(long)]
    pub port: Option<u16>,

    /// Override the reconciliation interval in seconds.
    #[arg(long, value_name = "SECONDS")]
    pub interval: Option<f64>,

    /// Ollama model name, or `off` to disable generated headings.
    #[arg(long)]
    pub model: Option<String>,

    /// Optional separate Ollama model used for card titles.
    #[arg(long)]
    pub title_model: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Create a secure minimal configuration file.
    Init(ConfigInitArgs),
    /// Print the effective configuration after environment overrides.
    Print,
}

#[derive(Debug, Args)]
pub struct ConfigInitArgs {
    /// Replace an existing configuration file.
    #[arg(long, conflicts_with = "stdout")]
    pub force: bool,

    /// Print the minimal configuration without writing a file.
    #[arg(long)]
    pub stdout: bool,
}
