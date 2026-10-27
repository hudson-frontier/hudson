mod client;
mod commands;
mod follow;
mod session;

use clap::Parser;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    client::execute(commands::Args::parse())
}
