use crate::config::Args;
use hudson_core::{configured::Configuration, models::*, storage::Store};

pub fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.experiment.is_some()
        || args.inspect_experiment.is_some()
        || args.cancel_experiment.is_some()
    {
        return super::experiments::run(args);
    }
    if super::control::execute(&args)? {
        return Ok(());
    }
    if args.evaluate.is_some() {
        return super::evaluation::run(args);
    }
    let path = args
        .config
        .ok_or("provide --config <agent.json> or --demo")?;
    if !args.check && args.task.is_none() && args.input_file.is_none() && args.resume.is_none() {
        return Err("provide --task, --input-file, or --resume".into());
    }
    let configuration = Configuration::load(&path)?;
    if args.check {
        println!("Configuration valid (syntax, tree, schemas, and skill files); no provider or database calls made.");
        return Ok(());
    }
    let input = if let Some(path) = args.input_file {
        use std::io::Read;
        let reader: Box<dyn Read> = if path == std::path::Path::new("-") {
            Box::new(std::io::stdin())
        } else {
            Box::new(std::fs::File::open(path)?)
        };
        let mut bytes = Vec::new();
        reader.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err("input file exceeds 1 MiB".into());
        }
        Some(serde_json::from_slice(&bytes)?)
    } else {
        args.task.map(serde_json::Value::String)
    };
    let store = match args.database {
        Some(db) => Store::postgres_local("/tmp", &db, &args.namespace)?,
        None => Store::default(),
    };
    let actor = Actor {
        workspace_id: args.workspace_id.clone(),
        id: args.actor_id.clone(),
    };
    let mut tree = configuration.build_tree(store, &actor)?;
    let id = match args.resume {
        Some(id) => {
            let run = tree.runtime.store.inspect(&actor, id)?;
            let bindings = tree.bindings();
            let goal = bindings
                .get(&run.agent_ref)
                .ok_or("agent version is not configured in this tree")?;
            tree.runtime
                .store
                .validate_resume(&actor, id, &run.agent_ref, goal)?;
            id
        }
        None => tree.runtime.submit_with_goal(
            &actor,
            tree.reference.clone(),
            input.ok_or("task input is required")?,
            args.request_key,
            tree.goal.clone(),
        )?,
    };
    eprintln!("Run: {id}");
    loop {
        let view = tree.tick(&actor, id)?;
        if view.status.terminal()
            || matches!(view.status, RunStatus::Waiting | RunStatus::Cancelling)
        {
            println!("{}", serde_json::to_string_pretty(&view)?);
            if view.status == RunStatus::Failed {
                return Err("agent run failed".into());
            }
            return Ok(());
        }
        if tree
            .runtime
            .store
            .operations(&actor, id)?
            .iter()
            .any(|operation| operation.status == OperationStatus::Running)
        {
            println!("{}", serde_json::to_string_pretty(&view)?);
            eprintln!("An operation is already running. Confirm its executor state before resuming or marking it interrupted.");
            return Ok(());
        }
    }
}
