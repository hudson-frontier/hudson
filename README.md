<h1 align="center">Hudson</h1>

<p align="center">
  Build agents with controlled tools, durable execution, and recoverable state.
</p>

<p align="center">
  <a href="https://github.com/hudson-frontier/hudson/actions/workflows/ci.yml"><img alt="Harness checks" src="https://github.com/hudson-frontier/hudson/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="LICENSE"><img alt="Apache-2.0 license" src="https://img.shields.io/badge/license-Apache%202.0-blue?style=flat-square"></a>
</p>

<p align="center">
  <a href="#installation">Installation</a> ·
  <a href="#run-an-agent">Run an agent</a> ·
  <a href="#use-the-http-api">HTTP API</a> ·
  <a href="#reference">Documentation</a>
</p>

---

Hudson is a Rust agent harness. Define instructions, connect tools, and run the
model → tools → results → verification loop. The same runtime supports structured
tasks, approval gates, versioned agents, skills, and teams of specialist agents.

The current delivery focuses on text-and-tool agents, a loopback HTTP API, and
one configured workspace/actor identity per server. Application Rust tools are
trusted code. See the [implementation audit](docs/implementation-status.md) for
verified behavior and the remaining hosting and isolation work.

## Installation

Build the backend from source with the pinned Rust 1.92.0 toolchain:

```sh
git clone https://github.com/hudson-frontier/hudson.git
cd hudson
cargo build --locked -p hudson-worker -p hudson-server
```

Full workspace development also needs Python 3.12+, Node.js 22+, and `protoc`.
PostgreSQL and recovery checks use Unix facilities; Linux or WSL is recommended.
Follow the [development guide](docs/development.md) for prerequisites and coverage.

The separately distributed [Hudson CLI](https://github.com/hudson-frontier/hudson-cli#installation)
connects to an existing service with `hudson agent`. Installing that client and
building a backend are separate setup steps.
The backend workspace does not install a `hudson` or `hudson-cli` executable.
Its worker, server, credential utility, and Temporal host are service and
development tools; use the standalone CLI for normal agent operation.

## Run an agent

Start a configured backend with `OPENAI_API_KEY` in its process environment:

```sh
cargo run --locked -p hudson-server -- --config examples/analyst.json
```

In another terminal, use the installed Hudson CLI:

```sh
hudson agent health
hudson agent start --task "Find the mean of 10, 20, and 30" --request-key analysis-001
hudson agent follow RUN_UUID
hudson agent session
```

Copy the returned `run_id` into `follow`. Submission returns before completion;
stopping observation leaves the run active. `session` opens an interactive terminal
against the running service. The CLI operates runs through the HTTP API; service
installation and worker startup remain backend administration steps.

The default provider is OpenAI. Configure Anthropic, Gemini, Ollama, or compatible
endpoints through the [model settings](docs/development.md#models).
Structured tasks use `hudson agent start --input-file task.json`;
`--input-file -` selects stdin. The
[structured-data example](examples/data-agent.json) shows an input schema and
output contract. The backend [development examples](#backend-development-examples)
include configuration validation and an offline fixture with no provider calls.

## Connect tools and skills

For backend development, run the example Python tool service in one terminal:

```sh
python3 examples/python-tool/server.py
```

In another terminal, with `OPENAI_API_KEY` set:

```sh
cargo run --locked -p hudson-worker -- --config examples/python-tool/agent.json \
  --input-file examples/data-task.json
```

The model selects the tool; the Python service computes the statistics.
The [walkthrough](examples/python-tool/README.md) describes the request contract
and limits.

| Capability | Configure it with |
| --- | --- |
| HTTP and trusted Rust tools | [Tool definitions and registries](docs/development.md#tools) |
| MCP tools and portable skill packages | [MCP and skills](docs/mcp-skills.md) |
| Bounded history and evidence retrieval | [Context management](docs/context-management.md) |
| Scoped recall, corrections, and retention | [Memory](docs/memory.md) |
| Output schemas, goals, and verifier tools | [Evaluation and verification](docs/evaluation.md) |
| Specialist agents and delegation budgets | [Team coordination](docs/team-coordination.md) |

Tools, model connections, and permissions come from host configuration.
Agent instructions and skill files do not grant new authority.

## Keep runs in PostgreSQL

For a direct backend worker invocation, with a dedicated PostgreSQL database named `hudson` available on the local
`/tmp` socket, retain runs across processes:

```sh
cargo run --locked -p hudson-worker -- --config examples/analyst.json \
  --database hudson --namespace my-project --request-key analysis-001 \
  --task "Find the mean of 10, 20, and 30"
```

Resume a saved run with `--resume RUN_UUID`, keeping the same configuration,
database, and namespace. Without `--database`, state stays in memory.
The current storage layout serializes namespace writes and suits small
deployments. See [recovery](docs/recovery.md) for interrupted attempts and
evidence-based reconciliation of uncertain external effects.

## Use the HTTP API

Start a configured service using the same database:

```sh
cargo run --locked -p hudson-server -- --config examples/analyst.json \
  --database hudson --namespace my-project
```

From another terminal, use the published Hudson CLI:

```sh
hudson agent health
hudson agent start --task "Find the mean of 10, 20, and 30" --request-key analysis-002
hudson agent follow RUN_UUID
hudson agent session
```

Copy the returned `run_id` into `follow`. Submission returns before completion;
stopping observation leaves the run active. `session` opens an interactive terminal
against the already running service.

The server binds to loopback and uses one configured identity. The
[HTTP API guide](docs/api.md) covers scoped bearer credentials, replies, approvals,
cancellation, restart recovery, and separate Temporal execution. The
[OpenAPI contract](docs/openapi.json) describes the current preview routes.

## Publish customer-defined agents

Catalog mode lets customers publish immutable tool and agent revisions, then
submit tasks by agent reference. Operators supply approved model/tool connections;
customers supply schemas and instructions. A separate published-revision worker
handles execution and recovery.

Follow the [customer API walkthrough](examples/customer-api/README.md) for
credential provisioning, catalog startup, publication, and task submission.

## Run durable background work and teams

Start a local Temporal service and PostgreSQL first. Run a worker with the team
configuration:

```sh
cargo run --locked -p hudson-temporal -- \
  --config examples/team.json --database hudson --namespace team-demo \
  --task-queue team-demo worker
```

Submit work from another terminal using the same configuration, namespace, and
task queue:

```sh
cargo run --locked -p hudson-temporal -- \
  --config examples/team.json --database hudson --namespace team-demo \
  --task-queue team-demo run --task "Find the mean of 10, 20, and 30" \
  --request-key team-001 --background
```

Omit `--background` to wait for the result. The worker runs the same agent loop,
permissions, tools, and budgets; configured specialists execute as child
workflows. See [Temporal execution](docs/temporal.md) for service setup,
scheduling receipts, and recovery boundaries.

## Reference

| Guide | Covers |
| --- | --- |
| [Development](docs/development.md) | Configuration, providers, tools, and local setup |
| [HTTP API](docs/api.md) | Authentication, publication, run controls, and event cursors |
| [Recovery](docs/recovery.md) | Interrupted attempts and verified tool receipts |
| [Harness controls](docs/harness-controls.md) | Steering, follow-ups, context, and reasoning settings |
| [Code structure](docs/code-structure.md) | Crate ownership and execution boundaries |
| [Implementation status](docs/implementation-status.md) | Verification evidence and current limitations |
| [Harbor integration](integrations/harbor/README.md) | Regression tasks and independent verifiers |

[Hudsonbox](https://github.com/hudson-frontier/hudsonbox) is the separate Linux
workload runtime. Effectful sandbox integration remains unfinished; its
[adapter contract](docs/sandbox-adapter-contract.md) defines the required boundary.

## Developing

### Backend development examples

Validate the example configuration without credentials or a database:

```sh
cargo run --locked -p hudson-worker -- --config examples/analyst.json --check
```

To exercise the worker directly, set `OPENAI_API_KEY` in its environment:

```sh
cargo run --locked -p hudson-worker -- --config examples/analyst.json \
  --task "Find the mean of 10, 20, and 30"
```

For a fixture-only preview with no provider calls:

```sh
cargo run --locked -p hudson-worker -- --demo
```

The worker is a backend development and execution host. Normal operator
submissions use the installed `hudson agent` client described above.

### Verification

With the pinned toolchain, development prerequisites, and a dedicated PostgreSQL
test database on `/tmp`:

```sh
python3 scripts/check_repository.py
node --test .github/scripts/*.test.cjs
python3 scripts/check.py --database YOUR_TEST_DATABASE --temporal
```

The aggregate checks use local model stubs and real isolated Temporal test
servers. They make no paid provider calls. Omitting the database or Temporal flags
runs a subset; see the [verification guide](docs/development.md#verification).

| Path | Purpose |
| --- | --- |
| [crates/hudson-harness](crates/hudson-harness) | Agent loop and checkpoint protocol |
| [crates/hudson-core](crates/hudson-core) | Policies, persistence, tools, and runtime controls |
| [crates/hudson-worker](crates/hudson-worker) | Configured local tasks and operator commands |
| [crates/hudson-server](crates/hudson-server) | Loopback admission and inspection API |
| [crates/hudson-temporal](crates/hudson-temporal) | Durable scheduling and team execution |
| [crates/hudson-api-fixture](crates/hudson-api-fixture) | Private HTTP conformance fixture used by backend tests |

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) for the branch, validation, and squash-merge
process. Follow the [Code of Conduct](CODE_OF_CONDUCT.md), and report
vulnerabilities through [SECURITY.md](SECURITY.md). Versions and changelogs are
maintained by the release bot.

## License

Released under [Apache-2.0](LICENSE).
