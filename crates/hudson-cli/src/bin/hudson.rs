#[path = "../client.rs"]
mod client;
#[path = "../commands.rs"]
mod commands;
#[path = "../follow.rs"]
mod follow;
#[path = "../session.rs"]
mod session;

use clap::Parser;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    client::execute(commands::Args::parse())
}
