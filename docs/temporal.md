# Temporal execution

Hudson uses the same AgentLoop, tools, permission checks, budgets, and Postgres
records in both execution modes. Foreground waits for a Temporal workflow result;
background returns the run ID while a separate worker continues the workflow.

A configuration with no `subagents` defines one agent. A configuration with
`subagents` defines a lead-managed team. Each child retains its own instructions,
model, and tools. Temporal starts all children in a completed delegation batch
before joining them. The lead then uses its join tools to read their results.
Peer-to-peer swarms are not implemented.

## Local usage

Start a Temporal server separately (`temporal server start-dev`) and a Postgres
server. Configure the Temporal connection through the official SDK's environment
configuration. Use a dedicated task queue for each immutable configured tree and
Hudson database namespace; every worker on that queue must use that same config.
Configured activities validate the saved scheduling target on every attempt,
including delegated runs, before inspecting operations or invoking executors.
An absent or mismatched target is rejected; `run --resume` uses the same check.
Rust hosts pass `ExecutionClient::schedule_target(namespace, task_queue)` to
`RunActivities::new` and admit work with `submit_scheduled`.

```sh
cargo run -p hudson-temporal -- \
  --config examples/team.json --database hudson --namespace temporal-team \
  --task-queue temporal-team worker

cargo run -p hudson-temporal -- \
  --config examples/team.json --database hudson --namespace temporal-team \
  --task-queue temporal-team run --task "Research this task" --request-key task-1

# Same execution, return immediately after Temporal accepts it:
cargo run -p hudson-temporal -- \
  --config examples/team.json --database hudson --namespace temporal-team \
  --task-queue temporal-team run --task "Research this task" --request-key task-2 --background
```

`run --input-file task.json` submits structured JSON (use `-` for stdin).

`run --resume <run-id>` starts or attaches to the stable workflow ID. New CLI submissions commit a scheduling request with the run. A matching worker
retries pending requests every five seconds, recovering an interruption between
the Postgres commit and Temporal start without resubmission. Repeating the same
request key remains safe and must retain the original scheduling target. The current Postgres connection uses the local `/tmp` socket.

Use the existing worker control commands to approve operations, answer questions,
cancel runs, and reconcile unknown outcomes in the same database/namespace.
Temporal observes those changes using durable timers. Do not use the old server's
execution driver or the synchronous worker to execute these runs concurrently.
Temporal team definitions have different tool identities; use a new namespace or
increment the agent versions when migrating an existing synchronous tree.

## Workers for published revisions

A trusted host can publish a `Configuration` through
`Store::publish_configuration`, restore it through `published_configuration`,
and submit with its admission tree and the expected scheduling target. The
published worker discovers saved scheduling requests for all published revisions
owned by its configured workspace and actor:

```sh
cargo run -p hudson-temporal -- \
  --published --database hudson --namespace customer-one \
  --workspace-id customer-one --actor-id backend \
  --task-queue customer-one worker
```

This mode requires no agent file. It reloads the root's immutable publication for
each revision and uses that root's tree for delegated children, even when another
publication uses the same child name. Worker-side credentials are resolved when
the revision is first used. The cache holds at most 64 root submissions; evicting a cached
runtime does not remove definitions or run state. Different tree members keep
their independent executor locks.

Use a dedicated queue per configured workspace/actor and database namespace. Do
not mix startup-configured and published workers on that queue. Queue mismatches,
unpublished roots, and another publication owner's runs are rejected. The
scheduler checks pinned goals and budgets before starting a saved request.
`--published` supports `worker`; customers publish and submit through the
[authenticated catalog API](api.md#publish-customer-tools-and-agents). Trusted Rust hosts
can use `RunActivities::published` and `SchedulingPump::published` directly.

## Durability boundary

Workflow code does no provider or tool IO. Activities call the existing runtime,
which commits operation intent before effects. If an activity dies during an
external call, its retry does not repeat a Running or Unknown operation. A trusted
operator must reconcile uncertain effects with evidence through existing controls.
Temporal retries are not proof that a remote write failed.

Activities heartbeat every two seconds, with a ten-second heartbeat timeout.
Worker loss therefore triggers a Temporal retry while persisted operation state
continues to prevent replay of uncertain effects.

Workflow histories continue as new every 500 driver iterations. Child workflows
finish before that rollover. Waiting approvals/input and unresolved operations do
not consume model calls. Different specialist executors can run concurrently;
runs using the same configured agent executor are serialized within one worker.

## Verification

```sh
python3 scripts/check.py --database hudson_harness_test_20260921 --temporal
```

The Temporal tests start real isolated local servers and use local fake model
endpoints; they do not call paid model APIs. On first use the test SDK downloads
the Temporal CLI. Coverage includes cancellation, concurrent team members and
joined results, foreground/background CLI processes, request-key deduplication,
a question resumed from PostgreSQL after a worker restart, approval enforcement,
and SIGKILL during a write followed by a Temporal retry and receipt reconciliation.
Published-revision coverage includes concurrent children, user-input recovery after
a worker restart, and a separate worker process with its source file removed.
No hosted deployment or paid provider calls are part of these checks.

## Embedding execution in a Rust service

`hudson_temporal::ExecutionClient` exposes the same start/attach and result logic
used by the CLI. Build a configured tree and submit with its core runtime to pin
instructions, tools, goal, and budget, then call `execution.start(run_id).await`
to obtain a serializable `RunReceipt`. Return that receipt for background work,
or call `execution.result(run_id).await` for foreground work. A separate worker
runs `RunActivities` from that same configured tree.

Construct the client with the official Temporal `Client`, Hudson storage namespace,
and worker task queue. A repeated start attaches to the stable workflow identity;
completed workflows are never executed again. For automatic recovery, use core `submit_scheduled` with
`ExecutionClient::schedule_target(namespace, task_queue)` and run a `SchedulingPump`
with the worker. Ordinary core submissions are never implicitly adopted. A
scheduling acknowledgement records Temporal acceptance, not task completion. An application exposing
this SDK over a network must authenticate its own callers and supply their trusted
workspace/actor identity. The loopback HTTP development server defaults to its
local driver; use `--temporal-task-queue` with durable storage to save submissions
for a separate worker ([API setup](api.md#separate-api-admission-from-temporal-execution)).
