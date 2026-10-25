use clap::{Parser, Subcommand};
use hudson_core::{configured::Configuration, models::Actor, storage::Store};
use hudson_temporal::{RunActivities, RunWorkflow};
use std::path::PathBuf;
use temporalio_client::{
    envconfig::LoadClientConfigProfileOptions, Client, ClientOptions, Connection,
};
use temporalio_sdk::{Runtime, Worker, WorkerOptions};
use uuid::Uuid;

#[derive(Parser)]
#[command(group(clap::ArgGroup::new("configuration").required(true).args(["config", "published"])))]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Load this owner's immutable published revisions on demand (worker only).
    #[arg(long)]
    published: bool,
    #[arg(long)]
    database: String,
    #[arg(long, default_value = "local")]
    namespace: String,
    #[arg(long, default_value = "hudson")]
    task_queue: String,
    #[arg(long, default_value = "local")]
    workspace_id: String,
    #[arg(long, default_value = "developer")]
    actor_id: String,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Worker,
    Run {
        #[arg(long, conflicts_with_all = ["resume", "input_file"], required_unless_present_any = ["resume", "input_file"])]
        task: Option<String>,
        #[arg(long, conflicts_with_all = ["resume", "task"])]
        input_file: Option<PathBuf>,
        #[arg(long)]
        resume: Option<Uuid>,
        #[arg(long, conflicts_with = "resume")]
        request_key: Option<String>,
        #[arg(long)]
        background: bool,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.published && !matches!(&args.command, Command::Worker) {
        return Err(
            "--published supports worker mode; submit runs through the admission API".into(),
        );
    }
    let actor = Actor {
        workspace_id: args.workspace_id.clone(),
        id: args.actor_id.clone(),
    };
    let store = Store::postgres_local("/tmp", &args.database, &args.namespace)?;
    let tree = args
        .config
        .as_ref()
        .map(|path| Configuration::load(path)?.build_temporal_tree(store.clone(), &actor))
        .transpose()?;
    let schedule_target =
        hudson_temporal::ExecutionClient::schedule_target(&args.namespace, &args.task_queue);
    let pump = match &tree {
        Some(tree) => {
            hudson_temporal::SchedulingPump::new(tree, actor.clone(), schedule_target.clone())
        }
        None => hudson_temporal::SchedulingPump::published(
            store.clone(),
            actor.clone(),
            schedule_target.clone(),
        ),
    };
    // Blocking providers and PostgreSQL are constructed outside the Tokio runtime.
    let run_id = match &args.command {
        Command::Worker => None,
        Command::Run {
            task,
            input_file,
            resume,
            request_key,
            ..
        } => {
            let tree = tree.as_ref().ok_or("configured runtime required")?;
            Some(match resume {
                Some(id) => {
                    tree.runtime
                        .store
                        .validate_schedule(&actor, *id, Some(&schedule_target))?;
                    let run = tree.runtime.store.inspect(&actor, *id)?;
                    if run.parent_operation.is_some() {
                        return Err("child runs are scheduled by their parent workflow; resume the root run".into());
                    }
                    let bindings = tree.bindings();
                    let goal = bindings
                        .get(&run.agent_ref)
                        .ok_or("agent is not configured")?;
                    tree.runtime
                        .store
                        .validate_resume(&actor, *id, &run.agent_ref, goal)?;
                    tree.runtime.store.validate_model_budget(
                        &actor,
                        *id,
                        &tree.runtime.model_budget_binding(),
                    )?;
                    *id
                }
                None => tree.runtime.submit_scheduled(
                    &actor,
                    tree.reference.clone(),
                    read_input(task.as_deref(), input_file.as_deref())?,
                    request_key.clone(),
                    tree.goal.clone(),
                    Some(schedule_target.clone()),
                )?,
            })
        }
    };
    if let Some(id) = run_id {
        eprintln!("Run: {id}");
    }
    let activities = match tree {
        Some(tree) => RunActivities::new(tree.into_scheduled(), actor, schedule_target),
        None => RunActivities::published(store.clone(), actor, schedule_target)?,
    };
    tokio::runtime::Runtime::new()?.block_on(async move {
        let runtime = Runtime::from_current_tokio(Default::default())?;
        let (connection, options) =
            ClientOptions::load_from_config(LoadClientConfigProfileOptions::default())?;
        let client = Client::new(Connection::connect(connection).await?, options)?;
        match args.command {
            Command::Worker => {
                let execution = hudson_temporal::ExecutionClient::new(
                    client.clone(),
                    args.namespace,
                    args.task_queue.clone(),
                );
                let options = WorkerOptions::new(args.task_queue)
                    .register_workflow::<RunWorkflow>()?
                    .register_activities(activities)
                    .build();
                let mut worker = Worker::new(&runtime, client, options)?;
                let publisher = async {
                    loop {
                        match pump.publish_pending(&execution).await {
                            Ok(report) => {
                                for (id, error) in report.deferred {
                                    eprintln!("Scheduling {id} deferred: {error}");
                                }
                            }
                            Err(error) => eprintln!("Scheduling scan deferred: {error}"),
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                };
                tokio::select! {
                    result = worker.run() => { result?; }
                    () = publisher => {}
                }
            }
            Command::Run { background, .. } => {
                drop(activities);
                let id = run_id.ok_or("missing run id")?;
                let execution =
                    hudson_temporal::ExecutionClient::new(client, args.namespace, args.task_queue);
                let receipt = execution.start(id).await?;
                println!("{}", serde_json::to_string(&receipt)?);
                if !background {
                    let view = execution
                        .result(id)
                        .await
                        .map_err(|error| -> Box<dyn std::error::Error> { error })?;
                    println!("{}", serde_json::to_string_pretty(&view)?);
                    if view.status == hudson_core::models::RunStatus::Failed {
                        return Err("agent run failed".into());
                    }
                }
            }
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}

fn read_input(
    task: Option<&str>,
    file: Option<&std::path::Path>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use std::io::Read;
    if let Some(file) = file {
        let reader: Box<dyn Read> = if file == std::path::Path::new("-") {
            Box::new(std::io::stdin())
        } else {
            Box::new(std::fs::File::open(file)?)
        };
        let mut bytes = Vec::new();
        reader.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err("input exceeds 1 MiB".into());
        }
        Ok(serde_json::from_slice(&bytes)?)
    } else {
        Ok(serde_json::Value::String(
            task.ok_or("task input required")?.into(),
        ))
    }
}
