mod agent;
mod config;
mod control;
mod evaluation;
mod experiments;
use clap::Parser;
use config::Args;
use hudson_core::{fixtures, models::WaitReason};
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if !args.demo {
        return agent::run(args);
    }
    let mut runtime = fixtures::runtime()?;
    let actor = fixtures::actor();
    let id = runtime.submit(
        &actor,
        fixtures::agent_ref(),
        json!({"order_id":"123","action":if args.refund {"refund"} else {"lookup"}}),
        None,
    )?;
    let view = fixtures::drive(&mut runtime, &actor, id)?;
    println!("{}", serde_json::to_string_pretty(&view)?);
    if matches!(view.wait, Some(WaitReason::Approval { .. })) {
        eprintln!(
            "Fixture paused for approval. Use the local server + CLI to approve and continue."
        );
    }
    Ok(())
}
