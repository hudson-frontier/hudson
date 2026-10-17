# Hudson code structure

The implementation is one Cargo workspace with seven crates. Domain behavior
belongs in tools and configuration. The loop has its own module and performs no IO.

```text
crates/
  hudson-harness/src/
    agent_loop.rs      model → tools → model → verify → complete
    backend.rs         effect-free Backend extension contract
    engine.rs          checkpoint compatibility and step validation
    protocol.rs        typed messages, actions, results, provider metadata
    state.rs           serializable checkpoint envelope
    config.rs          model, instructions, tool descriptors, bounds
    context.rs         complete conversation with explicit byte bounds
  hudson-core/src/
    models/            Agent, Tool, Run, Operation, Event; nested value types
    configured.rs      shared JSON configuration and runtime construction
    definitions.rs     immutable versions, schemas, transport bindings
    runtime.rs         submit, advance, collect, cancel, complete
    dispatch.rs        authorization, attempt admission, external execution
    security.rs        tool policy and exact-request approval rules
    budgets.rs         per-run and shared model-call admission
    verification.rs    output/goal contracts and repair feedback
    recovery.rs        interrupted-attempt fencing and receipt reconciliation
    sandbox_binding.rs owned request bytes, mutation keys, attempt and receipt fences
    skills.rs          immutable instruction catalogs and Markdown loading
    subagents.rs       delegate, join, persisted lineage
    evaluation.rs      ordinary-runtime regression cases and reports
    storage/
      memory.rs        transactional in-memory state and shared store handle
      postgres.rs      one locked JSONB state document per namespace
      pairs.rs         serialization of composite map keys
    adapters/
      harness.rs       project core definitions into loop configuration
      models.rs        ModelExecutor extension boundary
      chat.rs          OpenAI-compatible text/function protocol
      anthropic.rs     native Messages text/function protocol
      tools.rs         ToolExecutor and trusted Rust ToolRegistry
      http_tools.rs    configured HTTP tool transport
      sandbox.rs       explicit unsupported Sandbox guard
      sandbox_recovery.rs transport-independent sandbox admission/receipt decisions
    fixtures.rs        opt-in scripted conformance fixtures
  hudson-worker/src/
    main.rs            entrypoint and offline demo
    config.rs          command-line arguments
    agent.rs           configured task/resume driver
    control.rs         local inspection, approval, cancellation, recovery
    evaluation.rs      regression suite/comparison command
  hudson-server/src/
    main.rs            loopback server and startup configuration
    config.rs          server arguments
    routes.rs          configured/fixture HTTP runs and controls
  hudson-temporal/src/
    lib.rs             durable run workflow, child scheduling, runtime activities
    main.rs            worker, foreground/background submissions, resume
  hudson-cli/src/
    main.rs            generic HTTP client
    commands.rs        command arguments
    client.rs          HTTP transport
  hudson-sandbox-transport/src/
    lib.rs             pinned SDK bodies and owned, bounded HTTPS receipt observation
```

## Dependencies

```text
hudson-worker ──┐
hudson-temporal ├─→ hudson-core ─→ hudson-harness
hudson-server ──┘

hudson-cli ── HTTP/JSON ─→ hudson-server
hudson-sandbox-transport ─→ hudson-core + pinned public sandbox-client SDK
```

`hudson-harness` owns the current default state machine. An embedder can supply a
custom `Backend` without replacing runtime enforcement. It receives configuration,
a checkpoint, and an input; it returns the next checkpoint and proposed actions.
It never holds provider clients, credentials, store handles, or tool executors.

`hudson-core` owns authority and effects. It validates the proposed action, commits
operation intent and admission counters, then calls the configured executor
outside the database transaction. Subsequent ticks consume recorded outcomes.
Agent and Tool versions are pinned; mutable policy is checked again at dispatch.

The worker and server share `configured.rs`, so JSON agents, tools, providers,
skills, goals, and subagents have one construction path. The server runs its own
background driver; it does not dispatch to a separate worker process. `ConfiguredTree`
keeps each child executor and routes resume by the run’s pinned Agent version.
Child approval therefore uses that child’s provider and tool bindings, including
after restart. Library
applications can construct `Runtime` directly with custom adapters.

The separate Temporal host schedules the same runtime through activities. It
uses `ScheduledTree` to give specialists independent executor locks and Temporal
child workflows for durable team joins. See [Temporal execution](temporal.md).

## One task through the code

1. A local API request or worker command supplies input and an optional request key.
2. Core submission pins an Agent version and creates a Run and initial Event.
3. `agent_loop.rs` requests a model call using the advertised tool descriptors.
4. Runtime commits an Operation; dispatch applies limits and invokes the provider.
5. Model tool calls become Operations whose exact arguments are validated and
   authorized. An approval wait returns control without executing the tool.
6. Recorded tool results go back into the same loop. Final output passes through
   configured schema/goal verification; failures become bounded repair feedback.
7. Completion, failure, cancellation, or unresolved effects remain inspectable.

The server keeps inspection/control separate from its blocking model/tool driver.
Database handlers run on blocking workers. Public RunView and OperationView omit
checkpoints, executable requests, and credentials. Local operator inspection also
provides attempt IDs for explicit recovery.

## Current boundaries

PostgreSQL transitions lock and replace a namespace's JSONB document. This is
transactional and simple, but serializes writes and is not a high-throughput layout.
The original synchronous hosts have no automatic lease recovery. Their same-store
subagents use cooperative joins and shared budgets; the separate Temporal host
provides durable scheduling and concurrent child workflows.

Rust functions are trusted code. Sandbox execution fails closed; the separate
Hudson Sandbox integration is unfinished. The HTTP host uses one loopback-only
local identity, not hosted multi-tenant authentication.

See [development](development.md), [recovery](recovery.md),
[evaluation](evaluation.md), and the [implementation audit](implementation-status.md).
The [data model](data-model.md) and [product goals](goals.md) also describe later
capabilities; they are not a claim that every planned field or service is present.
