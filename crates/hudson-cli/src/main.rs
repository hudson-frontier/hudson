mod client;
mod commands;
mod follow;
mod session;

use clap::Parser;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = commands::Args::parse();
    if args.workspace.is_some()
        || args.resume.is_some()
        || args.provider.is_some()
        || args.model.is_some()
        || args.endpoint.is_some()
        || args.permission.is_some()
    {
        return Err(
            "local options are available through `hudson`; hudson-cli is the HTTP client".into(),
        );
    }
    client::execute(args)
}
