use crate::config::Args;
use hudson_core::{
    configured::Configuration,
    experiments::{Driver, Spec, Status},
    models::Actor,
    storage::Store,
};
use std::io::Read;

pub fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let actor = Actor {
        workspace_id: args.workspace_id,
        id: args.actor_id,
    };
    if let Some(id) = args.cancel_experiment {
        let store = Store::postgres_local(
            "/tmp",
            args.database.as_deref().ok_or("database is required")?,
            &args.namespace,
        )?;
        let mut driver = Driver::resume(store, actor, id)?;
        driver.cancel()?;
        println!("{}", serde_json::to_string_pretty(&driver.tick()?)?);
        return Ok(());
    }
    if let Some(id) = args.inspect_experiment {
        let store = Store::postgres_local(
            "/tmp",
            args.database.as_deref().ok_or("database is required")?,
            &args.namespace,
        )?;
        println!(
            "{}",
            serde_json::to_string_pretty(&store.inspect_experiment(&actor, id)?)?
        );
        return Ok(());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(args.experiment.ok_or("experiment spec is required")?)?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        return Err("experiment spec exceeds 128 KiB".into());
    }
    let spec: Spec = serde_json::from_slice(&bytes)?;
    let configuration = Configuration::load(args.config.as_deref().ok_or("config is required")?)?;
    // The dry-run validates both the fixed template and spec before opening a store.
    let plan = Driver::plan(&configuration, &spec)?;
    if args.experiment_plan {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    let store = match args.database {
        Some(database) => Store::postgres_local("/tmp", &database, &args.namespace)?,
        None => Store::default(),
    };
    let mut driver = Driver::open(store, actor, configuration, spec)?;
    eprintln!("Experiment: {}", driver.id());
    // Independent host bound in addition to each stage's budgets and 24-run cap.
    for _ in 0..args.experiment_ticks.unwrap_or(16_384) {
        let view = driver.tick()?;
        if view.status.terminal()
            || view.status == Status::Waiting
            || view.status == Status::Cancelling
        {
            println!("{}", serde_json::to_string_pretty(&view)?);
            if view.status == Status::Failed {
                return Err("experiment failed; baseline retained".into());
            }
            return Ok(());
        }
    }
    println!("{}", serde_json::to_string_pretty(&driver.inspect()?)?);
    eprintln!("Experiment paused at the invocation's tick limit; resume with the same spec.");
    Ok(())
}
