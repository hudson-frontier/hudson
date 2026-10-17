# Hudson

[![CI](https://github.com/hudson-infinity/hudson/actions/workflows/ci.yml/badge.svg)](https://github.com/hudson-infinity/hudson/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

Hudson is open source under the [Apache License 2.0](LICENSE).
Read [Contributing](CONTRIBUTING.md) before opening an issue or pull request,
the [Code of Conduct](CODE_OF_CONDUCT.md) for community expectations, and
[Security](SECURITY.md) to report vulnerabilities privately.

A generic Rust agent harness. Define instructions, connect tools, and let Hudson
run the model → tools → results → verification loop.

The same loop supports coding services, data processing, analysis, and delegated
work. Domain behavior belongs in tools, skills, and agent configuration.

## Serve customer-defined agents

Customers can publish versioned tools and agents over an authenticated API, then
submit tasks by agent reference. Hudson persists the run and a separate worker
handles execution, approvals and recovery. The installation operator supplies
approved model/tool connections; customers supply schemas and instructions.
See the [customer API walkthrough](examples/customer-api/README.md).

## Run an agent

Requires Rust 1.92.0 (pinned in this repository) and an `OPENAI_API_KEY` in your
process environment.

```sh
cargo run --locked -p hudson-worker -- --config examples/analyst.json \
  --task 'Find the mean of 10, 20, and 30'
```

This version targets **OpenAI, Anthropic, and Gemini** text-and-tool agents, with
GPT as the default. Anthropic uses its native Messages protocol; Gemini uses its
OpenAI-compatible endpoint. Custom compatible endpoints are also configurable.
Model-specific capabilities vary; native image/audio/video workflows are outside
this version. See the [implementation audit](docs/implementation-status.md) for
verification evidence and current limitations.

Task inputs can be validated with `input_schema`. The worker accepts structured
JSON via `--input-file`; subagents can advertise and receive structured tasks too.

A minimal configuration:

```json
{
  "name": "assistant",
  "instructions": "Help with the user's task. State assumptions and be concise."
}
```

## Foreground, background, and teams

The Temporal host runs either a single agent or a lead with configured `subagents`.
Foreground waits for the result; `run --background` returns a run ID while a
separate worker continues. Both use the same loop, permissions, tools, budgets,
and PostgreSQL records. Team members execute as child workflows.

See [Temporal setup and commands](docs/temporal.md). The existing synchronous
worker and local HTTP server remain available for local workflows.

## Connect your tools and specialists

- **HTTP tools:** configure endpoint, JSON input schema, effect, and optional
  credential environment variable. Hudson validates arguments and sends an
  operation ID as the service's idempotency key.
- **Rust tools:** register application functions with `ToolRegistry`.
- **MCP tools:** configure servers and pinned tool schemas. Calls use the same
  authorization, approval, and durable-operation path as other tools.
- **Skills:** provide instruction bodies or standard `SKILL.md` packages; the
  model loads instructions and resources on demand.
- **Context:** enable bounded history and large-output archival with scoped
  retrieval of the original evidence.
- **Memory:** configure a private scope for bounded recall, corrections, deletion,
  and retention of explicitly selected completed-output fields.
- **Subagents:** nest agent definitions under `subagents`. Each child has its own
  model, tools, instructions, and limits. The lead can select dependencies at run
  time; Hudson enforces child/root budgets and bounds repeated delegation.
- **Goals:** attach an objective, output schema, value criteria, or required
  verifier-tool results. Failed checks enter the repair loop; assessments include
  references to recorded tool evidence.

See [analyst configuration](examples/analyst.json), [structured data agent](examples/data-agent.json), [team configuration](examples/team.json),
the [runnable Python HTTP tool](examples/python-tool/README.md),
and the [development guide](docs/development.md) for configuration and library APIs.

Customer examples include a [real-estate inventory tool](examples/real-estate/README.md)
and [MCP/portable skill configuration](examples/customer-mcp.json). Capability
configuration is optional: omitted memory/context/MCP/skills are not exposed.
Bash and sandbox execution are not built in to this delivery.

For service embedding, the [Temporal execution client](docs/temporal.md#embedding-execution-in-a-rust-service)
starts background runs or waits for foreground results. See [context](docs/context-management.md),
[memory](docs/memory.md), [MCP and skills](docs/mcp-skills.md), and
[team coordination](docs/team-coordination.md) for the contracts.

[Evaluation](docs/evaluation.md) supports held-out checks and reports usage/latency.
The [Harbor integration](integrations/harbor/README.md) supplies coding, data and
research regression tasks with independent verifiers. Fixture success verifies
execution behavior; it is not evidence of model quality or superiority.

## Keep runs in PostgreSQL

Create a dedicated local PostgreSQL database, then add storage options:

```sh
cargo run --locked -p hudson-worker -- --config examples/analyst.json \
  --database hudson --namespace my-project --request-key analysis-001 \
  --task 'Find the mean of 10, 20, and 30'
```

Resume with the same config and storage options using `--resume <run_uuid>`.
Without `--database`, state lives in memory for that process. The local worker
connects through `/tmp`; library users can provide a configured PostgreSQL client.
The initial backend stores one transactional JSONB document per namespace and
serializes its writes. It suits small deployments, not high-throughput workloads.

## Use the HTTP API

Start the same configured agent for non-Rust applications:

```sh
cargo run --locked -p hudson-server -- --config examples/analyst.json \
  --database hudson --namespace my-project
curl -X POST http://127.0.0.1:4318/runs \
  -H 'Content-Type: application/json' \
  -d '{"input":"Find the mean of 10, 20, and 30","request_key":"analysis-002"}'
```

The CLI uses that same API:

```sh
cargo run --locked -p hudson-cli -- start --task 'Find the mean of 10, 20, and 30'
cargo run --locked -p hudson-cli -- start --input-file task.json --request-key analysis-003
cargo run --locked -p hudson-cli -- resume RUN_UUID
```

`--input-file -` reads JSON from stdin. Use `--url` before the command to select a
different server. Inputs may be JSON objects, arrays, strings, or other JSON values.

The API returns `run_id` immediately and runs independently of the HTTP connection.
Read `/runs/{id}`, `/runs/{id}/events`, and `/runs/{id}/children` (CLI: `children RUN_UUID`).
Event reads return at most 100 entries by default. HTTP clients must advance
`after` to the final sequence and drain pages until empty. `hudson-cli events
RUN_UUID --after N` reads one page; repeat with the final sequence. See the
[API migration guidance](docs/api.md) for limits and reconnect behavior.
`hudson-cli follow RUN_UUID --after N` automatically drains pages and observes
waits and terminal outcomes. Ctrl-C stops observation; retain the last processed
sequence to reconnect.
Child runs expose their pinned agent reference and can be resumed with the same
tree configuration through either the API or worker. POST `/runs/{id}/resume` after a server
restart. An approval wait includes an operation ID: inspect `/operations/{id}`,
then POST `{"approved":true}` to `/operations/{id}/approval` to resume execution.
POST `/runs/{id}/cancel` to request cancellation, including during model/tool IO.
Unknown effects stay unresolved until an operator reconciles them.

This server binds only to loopback and uses one configured workspace/actor identity. It runs
one configured agent tree per process. The default local driver executes sequentially;
`--temporal-task-queue QUEUE` with `--database` instead saves work for a separate
Temporal worker ([setup](docs/api.md#separate-api-admission-from-temporal-execution)). It is a local
integration API. Enable `--require-api-token` for persisted scoped bearer
authentication ([setup](docs/api.md#require-scoped-api-credentials)). Hosted
multi-tenant serving is not implemented.
Without `--database`, its state is in memory. `--demo` selects the fixture preview.

## Recover and evaluate

Local operator commands can inspect an interrupted attempt and record a verified
tool receipt before resuming. See [recovery](docs/recovery.md) for the exact steps.

Compare two agent versions against the same JSON cases:

```sh
cargo run --locked -p hudson-worker -- --config agent-v1.json \
  --evaluate cases.json --compare-config agent-v2.json
```

See [evaluation](docs/evaluation.md) for the case format and report. Evaluations use
the same runtime and policies as ordinary runs; configure isolated tool services.

The [HTTP API guide](docs/api.md) and [OpenAPI contract](docs/openapi.json)
describe all current endpoints. Running servers expose `GET /openapi.json`.

## Check the implementation

```sh
python3 scripts/check.py
# Include PostgreSQL and process-recovery checks:
python3 scripts/check.py --database hudson_harness_test_20260921
```

The suite uses local model stubs and makes no paid model calls. The database must
already exist on the local `/tmp` socket. See the [development guide](docs/development.md)
for the covered checks and separate live-provider checks.

## Architecture

```text
Agent configuration + task
          ↓
Runtime: policies, operations, budgets, persistence
          ↕
AgentLoop: model → tools → model → verify → complete
          ↓
Model adapters / application tools / child runtimes
```

`hudson-harness` owns the portable loop and checkpoints. `hudson-core` owns the
five primary models—Agent, Tool, Run, Operation, Event—and execution controls.
`hudson-worker` and `hudson-server` share the configuration builder in
`hudson-core`. The server exposes configured runs over HTTP; `hudson-cli` submits arbitrary
inputs and provides inspection, approval, cancellation, and resume commands.

## Verification and current limits

```sh
cargo test --locked --workspace --all-features
cargo clippy --locked --workspace --all-features --all-targets -- -D warnings
cargo build --locked -p hudson-worker
python3 scripts/smoke_subagents.py
```

Live GPT and Gemini runs completed tool cycles. Offline tests cover
provider mappings, HTTP tools, approvals, limits, goals, and configured subagent
execution. Explicit PostgreSQL tests cover reconnect and shared-budget persistence.
Database tests are opt-in; ordinary workspace tests do not run them.

The local worker defaults to the local developer identity; `--workspace-id` and
`--actor-id` select a trusted host identity. Configured HTTP tools may
require approval; delegation is preauthorized. Library callers can configure policies.
Uncertain effects are never automatically replayed. Operator-assisted interruption
marking and tool receipt reconciliation exist. The Temporal host provides durable
background scheduling and concurrent team workflows; uncertain external effects
still require evidence-based reconciliation. Temporal submissions persist scheduling intent
atomically and matching workers recover unpublished runs. Currency accounting, automatic skill-file discovery,
and hosted product integration remain unfinished. No sandbox isolation is implemented;
application Rust tools are trusted code.

[Product goals](docs/goals.md), [data model](docs/data-model.md),
[code structure](docs/code-structure.md), and
[implementation decisions](docs/implementation-decisions/) describe the wider design.
Sandbox infrastructure belongs to the separate
[Hudson Sandbox repository](https://github.com/hudson-infinity/hudson-sandbox).

See the [implementation audit](docs/implementation-status.md) for requirement-by-requirement evidence and remaining work.

For interactive clarification, enable `"allow_user_input": true` in the agent
configuration. A waiting agent’s question appears in its run status. Answer with
`hudson-cli reply RUN_UUID --text "Your answer" --request-key answer-1 --question-id QUESTION_ID`; Hudson
continues the saved run. See the [development guide](docs/development.md).

## Interactive terminal sessions

Install the local client with `cargo install --locked --path crates/hudson-cli`.
From a terminal in your project, run `hudson` to open a line-oriented interactive
session. It connects to an **already running** `hudson-server` at
`http://127.0.0.1:4318`; it does not start a host or choose a model for you.
Start the server with your configured agent as described above, then:

```sh
hudson
# For a token-protected server, keep the bearer token in the named environment variable:
hudson --api-token-env HUDSON_API_TOKEN
# Or select an immutable published agent:
hudson session --agent analyst --agent-version 1
```

Type a plain-text task to create an independent run. The session prints the run
ID and request key, follows bounded real event pages, and returns to the prompt
when the run completes, fails, or waits. `/reply QUESTION_ID answer` responds to a
user-input wait; `/approve OPERATION_UUID` and `/deny OPERATION_UUID` explicitly
decide approvals. `/status`, `/resume`, `/cancel`, `/open RUN_UUID`, and `/follow`
inspect/control or reconnect to the current run. `/help` lists the controls.
Reconciliation waits still require the existing evidence-based operator recovery
workflow. Mutation failures are never automatically replayed; retain the printed
request key and use the existing `start` or `reply` command with the same input/key
to reconcile an ambiguous submission. Ctrl-C exits the client without cancelling
the run; reopen its ID with `/open` in another session.

The displayed project directory is local context only. This first terminal slice
does not upload files, grant filesystem tools, create a sandbox, or provide a
continuous coding conversation. Each task remains a separate run governed by the
host's configured tools, policies, agent input schema, and bearer-token scope.
The configured agent must accept text inputs. Full-screen rendering, local host
bootstrap, and explicitly scoped project tools are subsequent work. Existing
`hudson-cli` subcommands remain available; scripts should use `start`/`follow`
because the interactive entrypoint requires a real terminal.
