use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Client for the local Hudson agent API")]
pub struct Args {
    #[arg(long, default_value = "http://127.0.0.1:4318")]
    pub url: String,
    /// Read the bearer token from this environment variable, never from an argument.
    #[arg(long)]
    pub api_token_env: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}
#[derive(Subcommand)]
pub enum Command {
    /// Interactive terminal session using an already running configured host.
    Session(Session),
    Health,
    Capabilities,
    /// Publish a JSON request containing request_key and tool.
    PublishTool {
        file: std::path::PathBuf,
    },
    /// Publish a JSON request containing request_key and agent.
    PublishAgent {
        file: std::path::PathBuf,
    },
    Tool {
        name: String,
        version: u32,
    },
    Agent {
        name: String,
        version: u32,
    },
    Start(Start),
    /// Answer a waiting agent; reuse the key when retrying the same reply.
    Reply {
        run_id: String,
        #[arg(long)]
        question_id: u64,
        #[arg(long)]
        text: String,
        #[arg(long)]
        request_key: String,
    },
    Resume {
        run_id: String,
    },
    Get {
        run_id: String,
    },
    Children {
        run_id: String,
    },
    Operation {
        operation_id: String,
    },
    /// Read one event page (default 100); continue with --after the last sequence.
    Events {
        run_id: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Follow durable events until terminal; Ctrl-C stops observation only.
    Follow {
        run_id: String,
        /// Resume after the last fully processed event sequence.
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(1..=1000))]
        page_size: u64,
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(50..=60000))]
        poll_ms: u64,
        /// Consecutive retries for each unavailable request; backoff caps at 30 seconds.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(0..=10))]
        max_retries: u32,
    },
    Approve {
        operation_id: String,
        #[arg(long)]
        deny: bool,
    },
    Cancel {
        run_id: String,
    },
}

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("input").required(true).args(["task", "input_file", "order"])))]
pub struct Start {
    /// Select an immutable published agent; omit for startup-configured hosts.
    #[arg(long, requires_all = ["agent_version", "request_key"], conflicts_with = "order")]
    pub agent: Option<String>,
    #[arg(long, requires = "agent")]
    pub agent_version: Option<u32>,
    /// Submit a plain-text task to the configured agent.
    #[arg(long)]
    pub task: Option<String>,
    /// Submit arbitrary JSON from a file; use - to read standard input.
    #[arg(long)]
    pub input_file: Option<std::path::PathBuf>,
    /// Explicit input for the offline order fixture.
    #[arg(long)]
    pub order: Option<String>,
    #[arg(long, requires = "order")]
    pub refund: bool,
    #[arg(long)]
    pub request_key: Option<String>,
}

#[derive(clap::Args, Default)]
pub struct Session {
    /// Select an immutable published agent instead of the startup-configured agent.
    #[arg(long, requires = "agent_version")]
    pub agent: Option<String>,
    #[arg(long, requires = "agent")]
    pub agent_version: Option<u32>,
}
