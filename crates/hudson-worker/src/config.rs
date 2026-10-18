use clap::Parser;

#[derive(Parser)]
#[command(about = "Run a Hudson agent task or resume a PostgreSQL-backed run.")]
pub struct Args {
    /// Run the explicit scripted order fixture.
    #[arg(long, group = "action")]
    pub demo: bool,
    #[arg(long)]
    pub refund: bool,
    /// JSON agent configuration.
    #[arg(long, conflicts_with = "demo")]
    pub config: Option<std::path::PathBuf>,
    /// Validate configuration and skill files without provider or database IO.
    #[arg(long, requires = "config", group = "action")]
    pub check: bool,
    #[arg(long, group = "action")]
    pub task: Option<String>,
    /// Structured JSON task from a file; use - for standard input.
    #[arg(long, requires = "config", group = "action")]
    pub input_file: Option<std::path::PathBuf>,
    /// Existing run UUID; requires --database.
    #[arg(long, requires = "database", group = "action")]
    pub resume: Option<uuid::Uuid>,
    /// Record a reply without executing the agent; resume separately afterward.
    #[arg(long, requires_all = ["database", "request_key", "reply_value", "question_id"], group = "action")]
    pub reply_run: Option<uuid::Uuid>,
    /// Question identity shown in the run’s user_input wait.
    #[arg(long, requires = "reply_run")]
    pub question_id: Option<u64>,
    #[arg(long, requires = "reply_run", group = "reply_value")]
    pub reply_text: Option<String>,
    /// JSON reply from a file (at most 1 MiB).
    #[arg(long, requires = "reply_run", group = "reply_value")]
    pub reply_file: Option<std::path::PathBuf>,
    /// Local PostgreSQL database (Unix socket /tmp).
    #[arg(long)]
    pub database: Option<String>,
    #[arg(long, default_value = "hudson")]
    pub namespace: String,
    #[arg(long, default_value = "local", conflicts_with = "demo")]
    pub workspace_id: String,
    #[arg(long, default_value = "developer", conflicts_with = "demo")]
    pub actor_id: String,
    #[arg(long)]
    pub request_key: Option<String>,
    #[arg(long, requires = "database", group = "action")]
    pub inspect_run: Option<uuid::Uuid>,
    #[arg(long, requires = "database", group = "action")]
    pub cancel_run: Option<uuid::Uuid>,
    #[arg(long, requires = "database", group = "action")]
    pub inspect_operation: Option<uuid::Uuid>,
    #[arg(long, requires = "database", group = "action")]
    pub approve_operation: Option<uuid::Uuid>,
    #[arg(long, requires = "database", group = "action")]
    pub deny_operation: Option<uuid::Uuid>,
    /// Fence an exact attempt after independently confirming its executor stopped.
    #[arg(long, requires_all = ["database", "attempt_id", "evidence"], groups = ["action", "evidence_action"])]
    pub mark_interrupted: Option<uuid::Uuid>,
    #[arg(long, requires = "mark_interrupted")]
    pub attempt_id: Option<uuid::Uuid>,
    #[arg(long, requires = "evidence_action")]
    pub evidence: Option<String>,
    /// Abandon an unavailable, already uncertain model response without retry/refund.
    #[arg(long, requires_all = ["database", "evidence"], groups = ["action", "evidence_action"])]
    pub abandon_model: Option<uuid::Uuid>,
    /// Record a verified destination result for an uncertain tool operation.
    #[arg(long, requires_all = ["database", "result_file", "receipt"], group = "action")]
    pub reconcile_operation: Option<uuid::Uuid>,
    #[arg(long, requires = "reconcile_operation")]
    pub result_file: Option<std::path::PathBuf>,
    #[arg(long, requires = "reconcile_operation")]
    pub receipt: Option<String>,
    /// JSON regression cases; runs normal tools, policies and budgets.
    #[arg(
        long,
        requires = "config",
        conflicts_with = "request_key",
        group = "action"
    )]
    pub evaluate: Option<std::path::PathBuf>,
    /// Evaluate a candidate configuration against the same cases after the baseline.
    #[arg(long, requires = "evaluate")]
    pub compare_config: Option<std::path::PathBuf>,
    /// Run or resume one bounded offline harness experiment from JSON.
    #[arg(
        long,
        requires = "config",
        conflicts_with = "request_key",
        group = "action"
    )]
    pub experiment: Option<std::path::PathBuf>,
    /// Validate an experiment plan without database, model or tool IO.
    #[arg(long, requires = "experiment")]
    pub experiment_plan: bool,
    /// Limit this invocation to N durable ticks; resume with the same spec.
    #[arg(long, requires = "experiment", conflicts_with = "experiment_plan", value_parser = clap::value_parser!(u32).range(1..=16384))]
    pub experiment_ticks: Option<u32>,
    /// Cancel an experiment independently of its config file or fixture endpoints.
    #[arg(long, requires = "database", group = "action")]
    pub cancel_experiment: Option<uuid::Uuid>,
    /// Inspect an existing experiment without loading providers or configuration.
    #[arg(long, requires = "database", group = "action")]
    pub inspect_experiment: Option<uuid::Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experiments_require_one_action_and_explicit_bounded_tick_batches() {
        let base = [
            "worker",
            "--config",
            "agent.json",
            "--experiment",
            "experiment.json",
        ];
        assert!(Args::try_parse_from(base.into_iter().chain(["--experiment-plan"])).is_ok());
        assert!(Args::try_parse_from(base.into_iter().chain(["--experiment-ticks", "1"])).is_ok());
        assert!(Args::try_parse_from(base.into_iter().chain(["--experiment-ticks", "0"])).is_err());
        assert!(
            Args::try_parse_from(base.into_iter().chain(["--task", "another action"])).is_err()
        );
        assert!(Args::try_parse_from(["worker", "--experiment", "experiment.json"]).is_err());
        assert!(Args::try_parse_from([
            "worker",
            "--database",
            "test",
            "--cancel-experiment",
            "00000000-0000-0000-0000-000000000001"
        ])
        .is_ok());
    }

    #[test]
    fn reply_requires_storage_identity_and_exactly_one_value() {
        let id = "00000000-0000-0000-0000-000000000001";
        let base = [
            "worker",
            "--database",
            "test",
            "--reply-run",
            id,
            "--request-key",
            "reply-1",
            "--question-id",
            "1",
        ];
        assert!(Args::try_parse_from(base).is_err());
        let text = base.into_iter().chain(["--reply-text", "Boston"]);
        assert!(Args::try_parse_from(text.clone()).is_ok());
        assert!(Args::try_parse_from(text.chain(["--reply-file", "reply.json"])).is_err());
        assert!(
            Args::try_parse_from(base.into_iter().chain(["--reply-file", "reply.json"])).is_ok()
        );
        assert!(Args::try_parse_from([
            "worker",
            "--database",
            "test",
            "--reply-run",
            id,
            "--reply-text",
            "Boston"
        ])
        .is_err());
        assert!(Args::try_parse_from([
            "worker",
            "--reply-run",
            id,
            "--request-key",
            "reply-1",
            "--reply-text",
            "Boston"
        ])
        .is_err());
        assert!(Args::try_parse_from(["worker", "--reply-text", "Boston"]).is_err());
        assert!(Args::try_parse_from(base.into_iter().chain([
            "--reply-text",
            "Boston",
            "--resume",
            id
        ]))
        .is_err());
    }
}
