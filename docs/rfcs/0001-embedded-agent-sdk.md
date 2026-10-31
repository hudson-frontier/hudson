# RFC 0001: Embedded autonomous-agent SDK

Status: implemented first slice; responsive driving follows in issue #107.
Tracks: #106, epic #112.

## Public contract

The public Rust facade is `hudson_core::sdk`. `SessionBuilder` fixes one actor,
workspace and immutable agent revision, an explicit model name and host-owned
`ModelExecutor`. Registered tool definitions, handlers and explicit policies are
assembled before building. No provider, credential, tool grant or goal is inferred.
Default storage is in memory; default limits are `Limits::default()` (128 harness
steps, eight model calls, 32 operations, batch four, 64 KiB context and payload).
Credentials belong exclusively to model executors, never session metadata.

`Session` wraps the existing `Runtime<AgentLoop, _, ToolRegistry>`. The host calls
`advance` once per runtime tick; there is no hidden loop, thread, retry or external
call on construction. `submit` admits a task with an optional explicit goal and
required idempotency key. `continue_turn` creates a fresh run from a completed
run's history; previous effects and verification evidence are not replayed.
`inspect`, bounded `events`, `steer`, `cancel`, `approve` and `reply` delegate to
existing core checks. A session may address only its actor's runs with the same
agent revision. A run ID is a reference, never an authorization grant.

A session is an application handle, not a new persisted entity. A turn is a run;
operations and attempts retain their existing identities. Reopening uses the same
store, actor, immutable definitions and model binding, then addresses saved run
IDs. Registration may leave inert definitions after a later build error; it never
executes effects, replaces existing policy or rolls back unrelated store state.

Run states remain queued/running/waiting/cancelling/completed/failed/cancelled.
Inspect `wait` for approval, user input or reconciliation, and `reason` for failure
or limit exhaustion. Only completed means verified completion under configured
criteria, not universal factual correctness. Errors returned by API calls are
separate from recorded run failures. Cancel does not undo effects or establish
remote cleanup. Resume means calling advance after resolving a wait; uncertain
effects require the existing explicit recovery APIs, never guessed replay.

Events use run-local increasing sequence numbers and exclusive cursors. The SDK
uses core's byte/count-bounded stream pages, including its single-large-event
exception. Model deltas remain provisional; operation settlement is authoritative.

## Extension and dependency boundaries

Today: application facade → core runtime → effect-free harness. Core dispatch
calls model/tool executors outside store transactions; storage owns state commits.
The harness owns portable protocol, checkpoint and next-action decisions, without
clients, credentials, policy mutation or store access. The facade adds no second
implementation of admission, verification, inbox, cancellation or persistence.

Future extractions should follow actual ownership: portable protocol → model
adapters and agent loop; runtime composes loop, policy, executors and storage;
applications assemble the runtime. Avoid cyclic imports and a crate per feature.
Provider extraction is optional follow-up, not necessary to this API slice.

Current executors and store operations are synchronous. Call the facade on a
host-owned blocking driver, never directly on an async executor. Arbitrary trusted
Rust functions cannot be forcibly interrupted. #107 owns bounded asynchronous
session driving and #110 owns async tool ergonomics; this slice promises neither.
External bridges return model/tool proposals under Hudson authority. A complete
external agent owning its tools/session requires a separate delegation contract;
it must not impersonate a model adapter to bypass admission.

## Persistence and distribution

`with_store` selects existing local-file or PostgreSQL stores without changing
semantics. Local files require a private Unix directory, a single host lifetime
lock and storage outside the editable workspace; snapshots are bounded but rewrite
whole state. PostgreSQL requires a precreated namespace/database and serializes
namespace writes. Neither backend gains automatic uncertain-attempt recovery.
Memory mode has no crash durability. No server, Temporal or sandbox is mandatory.

This is an additive preview module. Direct Runtime callers keep working. The
standalone hudson-cli remains a separate repository; no second CLI, Python
bindings, package publication or version bump is introduced here.

## Acceptance and follow-ups

`examples/embedded.rs` runs a synthetic model/custom-tool cycle without credentials
or networking. Integration tests cover real tool admission, schema checks,
approval, actor/revision scoping, idempotency, steering and persisted reconstruction.
Existing core suites retain recovery coverage. This facade makes no performance
claim: it delegates one tick without an additional state store or scheduler.
Managed jobs (#108), autonomy policy composition (#109), tool ergonomics (#110)
and broad examples/benchmarks (#111) remain separate milestones.
