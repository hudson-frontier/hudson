#[path = "../client.rs"]
mod client;
#[path = "../commands.rs"]
mod commands;
#[path = "../follow.rs"]
mod follow;
#[path = "../local/mod.rs"]
mod local;
#[path = "../session.rs"]
mod session;

use clap::Parser;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = commands::Args::parse();
    if args.command.is_none() && args.api_token_env.is_none() && args.url == "http://127.0.0.1:4318"
    {
        local::run(args)
    } else {
        if args.workspace.is_some()
            || args.resume.is_some()
            || args.provider.is_some()
            || args.model.is_some()
            || args.endpoint.is_some()
            || args.permission.is_some()
        {
            return Err("local options cannot be combined with HTTP client commands".into());
        }
        client::execute(args)
    }
}
