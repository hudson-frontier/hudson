# Developing Hudson

Hudson uses Rust 1.92.0, a committed Cargo.lock, and seven Cargo crates. The default
`AgentLoop` supports real model/tool execution, skills, goals, subagents, and
PostgreSQL persistence. The [code structure](code-structure.md) describes ownership;
the [implementation audit](implementation-status.md) distinguishes evidence from
remaining work.

Temporal SDK builds require the Protocol Buffers compiler (`protoc`) on PATH.
On Ubuntu install `protobuf-compiler`; on macOS install `protobuf` with Homebrew.
The hosted workflow installs it explicitly.

## Run or install locally

From the repository root:

```sh
cargo run --locked -p hudson-worker -- --config examples/analyst.json --check
cargo run --locked -p hudson-worker -- --config examples/analyst.json --task 'Find the mean of 10, 20, and 30'
cargo install --debug --locked --path crates/hudson-worker
cargo install --debug --locked --path crates/hudson-server
```

Install the operator client from the separate
[Hudson CLI repository](https://github.com/hudson-frontier/hudson-cli#installation).
Use `hudson agent` to operate this backend. The private `hudson-api-fixture`
executable exists only to keep backend protocol tests independent of a client
release; it is not an installation target or a supported operator interface.

For structured data, supply JSON directly:

```sh
cargo run --locked -p hudson-worker -- --config examples/data-agent.json --input-file examples/data-task.json
```

`--input-file -` reads stdin. The worker caps input files at 1 MiB; the Agent's
payload limit and optional `input_schema` still apply. The HTTP CLI also accepts
files/stdin, subject to the HTTP API's 64 KiB request-body cap. Text tasks use
`--task`. These input modes are mutually exclusive.

The real task uses `OPENAI_API_KEY` by default. The configuration check needs no
credentials or database: it checks the tree, required fields, schemas, explicit
skill files, tool-name conflicts, and budget settings. It does not establish
endpoint/model availability or compatibility with previously published versions.

For an entirely offline fixture:

```sh
cargo run --locked -p hudson-worker -- --demo
cargo run --locked -p hudson-worker -- --demo --refund
```

These commands create separate in-memory stores. Lookup completes; refund pauses
for approval. They simulate every model/tool effect. The standalone worker runs
a supplied task or run ID; it is not a distributed queue consumer.

## Agent configuration

`name` and `instructions` are required. Optional fields are:

| Field | Behavior |
| --- | --- |
| `version` | Immutable positive version; defaults to 1 |
| `provider`, `model`, `endpoint` | Provider preset, model ID, full transport URL |
| `api_key_env` | Credential environment-variable name, never its value |
| `max_output_tokens` | Provider output cap; default 1024 |
| `reasoning` | Explicit effort or native thinking budget, pinned to the agent version |
| `legacy_token_limit` | Use `max_tokens` on a custom compatible chat endpoint |
| `limits` | Partial object overriding call/operation/step/batch/context/payload defaults |
| `input_schema` | Task JSON Schema checked before a Run can dispatch work |
| `output_schema` | Final JSON Schema checked by runtime verification |
| `goal` | Immutable objective, success schema, and optional value/verifier-tool criteria |
| `skills`, `skill_files` | Inline or explicit Markdown instruction catalogs |
| `skill_packages` | Portable skill directories resolved relative to the config file and frozen at load |
| `mcp_servers` | HTTP MCP endpoints, pinned tool schemas, credential references and approval settings |
| `context` | Optional archival thresholds; enables scoped original-output retrieval |
| `memory` | Optional scope, recall limit, and selected output pointer for retention |
| `coordination` | Pinned root/child delegation, repetition, model-call and step bounds |
| `http_tools` | Versioned HTTP tool definitions and approval settings |
| `subagents` | Nested specialist agent configurations |
| `shared_model_budget` | Persisted group ID and model-call ceiling |

The [harness controls guide](harness-controls.md) covers durable steering,
follow-ups, context headroom, and provider reasoning settings.

External schema references are disabled. Literal values in `const`, `enum`, and
annotations can contain keys such as `$ref`; those values remain data. References
inside schemas must use local fragments, including schemas under properties whose
names match schema keywords.

Unknown configuration/limit fields are rejected. When changing definitions in an
existing store, increment the Agent version. Tool references, skill contents, and
nonsecret model transport settings remain pinned; credentials can rotate. Identical
definition publication is atomic, so concurrent startup accepts matching versions
and rejects changed content even across separate PostgreSQL connections.

Configured HTTP tool names are also their model-facing aliases. OpenAI/default
aliases require 1–64 ASCII letters, digits, underscores or hyphens; Anthropic
allows up to 128. Both generated `delegate_` and `join_delegate_` names must fit
the parent provider's limit, including their prefixes. Invalid aliases fail
configuration validation before provider calls; identifiers are never renamed.

A goal looks like:

```json
{"goal":{"objective":"Return a nonnegative total","success_schema":{"type":"object","properties":{"total":{"type":"number","minimum":0}},"required":["total"]}}}
```

See [verification criteria](evaluation.md) for value checks and checks against
recorded customer tool results. The returned assessment includes operation IDs and
request/result digests; these are provenance, not a blanket factual guarantee.

The goal is included in model instructions and verified in addition to the Agent's
output schema. It checks final-output properties, not arbitrary real-world success.
Failed verification feeds repair back to the model within the same limits.

## Models

The default is OpenAI Chat Completions with `gpt-4.1-mini` and `OPENAI_API_KEY`.
Other presets require an explicit model ID:

| Provider | Protocol/default destination | Default credential |
| --- | --- | --- |
| `openai` | Chat Completions | `OPENAI_API_KEY` |
| `anthropic` | Native Messages | `ANTHROPIC_API_KEY` |
| `gemini` | Gemini OpenAI-compatible endpoint | `GEMINI_API_KEY` |
| `ollama` | `http://127.0.0.1:11434/v1/chat/completions` | None |

An explicitly configured `api_key_env` must exist and contain a nonempty value;
otherwise startup fails before provider IO. Optional local endpoints without a
configured credential can still run unauthenticated.

Override `endpoint` for a compatible service. HTTPS is required except localhost;
redirects are disabled, requests time out, responses are capped at 2 MiB, and
ambiguous transport failures are not retried automatically. Gemini/Ollama presets
use `max_tokens`; default OpenAI uses `max_completion_tokens`.

Adapters support text/JSON and client function tools. Chat responses retain
assistant text alongside tool calls through checkpoints and restore that text
exactly once in the next provider request. Refusal-bearing or unsupported content
cannot dispatch tools. The continuation follows the documented
[Chat Completions tool cycle](https://developers.openai.com/api/docs/guides/function-calling).
Older checkpoints remain readable; assistant text discarded by older adapters
cannot be reconstructed.
Chat tool calls retain
`extra_content` and `thought_signature`. Anthropic tool cycles retain signed
thinking, redacted thinking, and interleaved text in checkpoint metadata. Tool
executors receive arguments, not that metadata. Tests cover checkpoint roundtrips.
This is not universal model/modality support: multimodal input, server-side tools,
and some provider-specific modes are absent. Optional reasoning effort and native
thinking budgets are described in the [harness controls guide](harness-controls.md).

Supply `ModelExecutor` to integrate another model protocol. Model/provider
credentials belong to executors and never to Agent definitions or checkpoints.
A previous live GPT arithmetic smoke completed with result 42 using two model
calls and one registered Rust tool. All four configured presets pass complete offline tool cycles, including
authentication, output caps, schemas, and continuation metadata. A live Gemini 3.5 Flash-Lite run also completed the arithmetic tool cycle with two
model calls, one HTTP tool call, and a passed output contract. Anthropic and Ollama
have offline coverage only.

Model, HTTP tool, and MCP clients connect directly to loopback HTTP endpoints.
HTTPS connections retain their normal proxy environment configuration.

## Tools

A configured HTTP tool:

```json
{
  "http_tools": [{
    "name": "lookup",
    "description": "Look up a customer",
    "endpoint": "https://tools.example.com/lookup",
    "input_schema": {"type":"object","properties":{"id":{"type":"string"}},"required":["id"]},
    "effect": "read",
    "token_env": "CUSTOMER_TOOL_TOKEN",
    "require_approval": false
  }]
}
```

Hudson POSTs validated JSON arguments and expects JSON output. Optional
`output_schema` validates the result. `Idempotency-Key` is the stable operation
UUID; the destination implements deduplication. `token_env` supplies bearer auth.
Only host-configured endpoints can execute. Redirects are disabled, timeout is
60 seconds, and response size is capped at 1 MiB. Ambiguous failures remain unknown.
A write with an invalid/oversized result also remains unknown: its effect may have
committed even though its receipt is unusable.

Set `require_approval:true` to pause before execution. Startup cannot silently
remove an existing version's approval gate. Policies default-deny in the library;
the local builder grants its fixed developer identity the configured capabilities.
Intentional administrative policy changes use `set_policy` and are rechecked at
dispatch. The model cannot grant itself a tool or change its endpoint.

For trusted Rust functions, register a handler and publish a matching Tool:

```rust,ignore
let mut tools = ToolRegistry::new();
tools.register("orders.lookup", |invocation| {
    Ok(serde_json::json!({"order_id": invocation.arguments["order_id"], "status":"shipped"}))
})?;
let runtime = Runtime::new(store, AgentLoop, model_executor, tools);
```

The Tool uses `Execution::Registered { key: "orders.lookup" }`; its Agent references
that immutable Tool version. The callback receives validated arguments and an
operation UUID after policy/approval checks. Duplicate registration is rejected;
a panic becomes an unknown outcome. Rust functions are trusted application code,
not sandboxed uploads. See `crates/hudson-core/examples/agent.rs` for complete wiring.

Known tool calls with schema-invalid arguments receive correlated
`invalid_arguments` feedback before execution, letting the model correct them
within its normal budgets. A corrected write still requires its own approval.
See [tool correction and provider failures](harness-controls.md#tool-correction-and-provider-failures)
for ordered batches, recovery, and bounded provider diagnostics.

## Skills and specialists

Skills are named instruction bodies, loaded via an ordinary read-only `load_skill`
tool. Initially the model sees only names/descriptions. Catalog contents are frozen
and hashed; changing a file requires a new Agent version in an existing store.
Bodies are capped at 32 KiB. Skills do not grant permissions.

Use inline `skills`, or `skill_files` entries with `name`, `description`, and `path`.
Relative paths resolve beside the config, including nested specialists. Explicit
files must be UTF-8 regular files. Frontmatter, script execution, and automatic
directory discovery are not implemented. Library APIs are `Skill::from_markdown`
and `SkillCatalog::register`.

`examples/team.json` configures a coordinator and analyst. Each `subagents` entry
has explicit model/tools/skills/instructions; parent tools are not inherited.
The tree is limited to 32 unique names and four levels below the root. The root
must provide `shared_model_budget`; every model in the tree uses that counter.
Children cannot override the budget; their acceptance criteria use `output_schema`.

`delegate_<name>` takes `{"task":"..."}` by default. A child with `input_schema`
can instead accept any JSON task matching that schema, for example
`{"task":{"values":[10,20,30]}}`. The child's schema is advertised as the tool's
`task` property and enforced before dispatch; local schema references retain their
child-resource scope when nested. Delegation creates a normal child Run with a
submission key derived from the parent operation. Repeated delivery finds the
same child. `join_delegate_<name>` takes `{"run_id":"..."}` and resumes that child;
completed children return their recorded result without another model call.
Waiting/uncertain children return status and a handle, never a false completion.
Use `GET /runs/{id}/children` or `hudson agent children RUN_UUID` to discover direct
children. Approval and resume route to each child’s own provider and tools.
The worker also accepts a child ID with `--resume` and the original tree config;
`--inspect-run` includes direct children. Hosts reject agent versions outside
the configured tree before recording an approval decision.

Same-store lineage links each child to its parent operation, prevents reparenting
and cycles, and scopes joins to the original parent/specialist. Parent cancellation
propagates to attached descendants; unresolved child effects keep cancellation
unresolved. The original worker/server execution and joining are cooperative and
synchronous. The [Temporal host](temporal.md) uses `build_temporal_tree` and
`into_scheduled` to submit children, schedule them concurrently, and durably wait
for them. Separate-store library delegation requires application-owned
lifecycle coordination. Library APIs are `subagents::register_with_join` and
`budgets::BudgetedModel`; custom wiring must explicitly share the budget wrapper.

A shared group counts admitted model requests, including failed/uncertain calls.
Reopening preserves usage and rejects a changed limit. Use a fresh group for a new
budgeted batch. Each Run pins its executor’s budget workspace/group at submission;
resuming under a changed or removed binding fails before effects. Reusing a
submission key with a different binding also fails. New runs can use a fresh
batch without changing the Agent version. Legacy records without a binding
cannot resume through a budgeted executor; no group is guessed from old state.
Provider-reported tokens are recorded separately from admission;
currency costs are not calculated.

Run `usage` includes `reported_model_calls`, `reported_input_tokens`, and
`reported_output_tokens`. Totals cover only responses with valid usage reports;
a zero report count means usage is unavailable, not that the provider consumed
zero tokens. Missing or malformed reports never reuse a previous call's usage.
Accounting is stored on the exact attempt, exposed by operation inspection/events,
and committed with its result, so resume cannot double-count it. A response rejected
by the decoder or output checks can still contribute reported tokens.

OpenAI-compatible adapters use reported prompt/completion totals. Anthropic input
totals include ordinary input, cache reads, and cache writes, as documented in its
[prompt-caching usage contract](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).
Each run accounts for its own calls; child runs retain their own totals and return
them through delegation/join. These are provider reports, not billing estimates
or new token-budget enforcement. Lost responses can leave accounting incomplete.

## Storage and management

Add `--database hudson --namespace my-project` for a precreated local PostgreSQL
database on `/tmp`. Otherwise state is in memory. `--request-key` deduplicates
submission; `--resume RUN_UUID` consumes saved state with the original config.
Changed input, Agent reference, goal, or transport binding cannot silently reuse
an old submission or resume another version.

Library code can use `Store::postgres_local` or `Store::from_postgres` with a
caller-configured client/TLS policy. `MemoryStore` is a compatibility name for the
shared handle. PostgreSQL stores a versioned JSONB document per namespace; every
transaction locks/reloads that row and commits records/events/counters together.
Effects occur outside transactions. This serializes writes and rewrites the state
document, so it is a small-deployment backend, not a high-throughput layout.

These local controls need the database/namespace but no model credentials/config:

```sh
hudson-worker --database hudson --namespace my-project --inspect-run RUN_UUID
hudson-worker --database hudson --namespace my-project --inspect-operation OP_UUID
hudson-worker --database hudson --namespace my-project --approve-operation OP_UUID
hudson-worker --database hudson --namespace my-project --deny-operation OP_UUID
hudson-worker --database hudson --namespace my-project --cancel-run RUN_UUID
```

Worker approval commands do not execute tools; explicitly resume afterward.
Their approvals expire in five minutes. [Recovery](recovery.md) documents exact
attempt fencing and destination-receipt reconciliation after an interrupted write.
No age-based retry or automatic lease recovery is implemented.

## HTTP integration

The [README](../README.md#use-the-http-api) documents configured server startup,
submission, polling, approval, cancellation, and explicit resume after restart.
It binds only to loopback with one developer identity. Execution is independent
of the submitting HTTP connection. Blocking model/tool calls do not hold the
inspection/control handle; database handlers run on blocking workers.

`hudson agent start --task '...'` submits text; `start --input-file task.json` submits
arbitrary JSON, and `--input-file -` reads JSON from stdin. Exactly one input mode
is required. `--request-key` deduplicates submission. Encoded request bodies are
capped at the server's 64 KiB limit; malformed JSON and ambiguous input modes fail
locally. `resume RUN_UUID` continues a saved run, while `get`, `events`, `operation`,
`approve [--deny]`, and `cancel` expose its ordinary controls. Put `--url` after
`hudson agent` and before the action to select another server.

HTTP CLI file inputs must be regular files. Symbolic links, Windows reparse points,
and special files are rejected before any request. Use `--input-file -` or a
publication file argument of `-` to select stdin explicitly; the 64 KiB limit remains.

Ordinary CLI responses share the session/follow limit of 8 MiB and must be valid
JSON before output begins. Pretty formatting writes through a buffer instead of
allocating the entire expanded response. A response or output failure never
automatically retries a mutation.
When `--api-token-env` is set, loopback HTTP connects directly instead of using
environment proxies. HTTPS retains its normal proxy configuration.

The local listener accepts only `localhost`, `127.0.0.1`, or `[::1]` request
authorities. A browser's `Origin`, when present, must match the request's HTTP
authority; opaque and cross-origin requests are rejected with `403` before routing
or body processing. Ordinary CLI clients do not need an `Origin` header. Loopback
port forwarding can retain its externally selected port.

The fixture preview remains `hudson-server --demo`; the explicit fixture input is
`hudson agent start --order 123 [--refund]`. Events are cursor-polled;
SSE/WebSocket streaming, public authentication, and multi-tenant hosting are absent.

To test a separately built operator client against the real backend, set
`HUDSON_TEST_OPERATOR_CLI` to its absolute binary path before running
`scripts/smoke_api.py`, `scripts/smoke_auth.py`, or `scripts/smoke_publication.py`.
These scripts add the `agent` command group automatically. The usual dedicated
PostgreSQL and Temporal prerequisites still apply; no model provider is contacted.
Without this explicit override, backend CI uses `hudson-api-fixture` and never
downloads or builds another repository's client.

## Evaluation and extension points

The [local runtime library guide](local-runtime.md) describes Unix snapshot
storage, immutable conversation turns, and opt-in Anthropic text observation for
hosts such as the standalone CLI.

[Evaluation](evaluation.md) documents JSON cases, `--evaluate`, and
`--compare-config`. Cases use normal runtime controls, retain their run identities,
and assess expected outputs separately from runtime completion. Use isolated tools.

[Offline experiments](experiments.md) add an opt-in, one-step
propose/probe/compose workflow with frozen controls, a shared admission budget,
held-out selection, scoped lessons and PostgreSQL resume. Use `--experiment-plan`
before dispatching fixture work.

A custom `Backend` proposes actions through the same core checks. `Engine` validates
backend/schema compatibility and advancing checkpoints. Custom `ModelExecutor` and
`ToolExecutor` implementations own IO. No loop callback bypasses runtime authority.
Public views omit checkpoints, raw requests, and credentials.

Context retains complete exchanges under a byte ceiling and can archive older
evidence with explicit retrieval. It does not silently truncate, summarize, or
share sessions. Execution limits
bound steps, calls, operations, batches, context and payloads; provider transports
have timeouts, but arbitrary synchronous Rust tools cannot be forcibly interrupted.
Sandbox requests fail closed until the separate Hudson Sandbox integration exists.

## Verification

The runner also checks the reviewed preview HTTP contract baseline and runs the
standard-library Python client's local protocol regressions. See
[API compatibility](api-compatibility.md) when changing the HTTP schema.

Run the local suite from any working directory using the script’s path:

```sh
python3 scripts/check.py
```

It checks Rust formatting, tests, Clippy, the core without default features,
builds all three entrypoints, validates the example configurations, and runs
provider, delegation, evaluation, and Python-tool smokes with local model stubs.
It fails at the first failing command and does not run `live_provider.py`.
Cargo may download build dependencies if they are not already cached.

For durable checks, provide a dedicated local PostgreSQL database on `/tmp`:

```sh
python3 scripts/check.py --database hudson_harness_test_20260921
```

This also runs the ignored database tests and approval, API, interrupted-worker,
child-approval, and clarification smokes. Each uses a unique test namespace. The
runner does not create, drop, or clear the database; test records remain there.
Without `--database`, it explicitly reports those checks as skipped.

Individual `scripts/smoke_*.py` checks can still be run directly after building
the worker/server/CLI. Set `HUDSON_TEST_DATABASE` to select their database.
The tests establish the covered local behaviors, not production-scale performance,
provider quality across models, or hostile-code isolation.

### Optional live provider check

`scripts/live_provider.py` is deliberately outside the offline smoke suite. It
reads only credentials already present in its process environment and sends only
synthetic arithmetic. It permits at most two model calls, each capped at 256 output
tokens, and never automatically retries. This may incur provider charges.

```sh
python3 scripts/live_provider.py --provider gemini --model gemini-3.5-flash-lite
```

Add `--database <dedicated_local_database>` to retain the run and attempt evidence.
Model listing alone is not proof that a key can generate with that model: the older
Gemini 2.5 Flash-Lite was listed but returned 404 for this account, directing it to
3.5 Flash-Lite. The capped 3.5 tool-cycle check passed. Failed diagnostics did not
execute any tools. No provider credential is stored in a config or report.

## Asking the user for clarification

Set `"allow_user_input": true` in an agent definition to expose the built-in
`ask_user` capability. It accepts `{"prompt":"Which city?"}` and must be called
alone. Hudson persists a `waiting` run with a `user_input` wait; it does not keep
a model request open while waiting. The prompt is limited to 16 KiB.

Reply through `POST /runs/{id}/input` with
`{"input":"Boston","request_key":"answer-1","question_id":QUESTION_ID}`, or:

```sh
hudson agent reply RUN_UUID --text Boston --request-key answer-1 --question-id QUESTION_ID
```

The API schedules continuation using the run’s configured executor, including
for children. Replies may contain arbitrary JSON through the API. Reuse the same
key and value when retrying a reply; changing the value under that key fails.
Copy `QUESTION_ID` from the run’s `wait.question_id`; every question has its own
identity. A stale answer cannot answer a later question. Each new question needs
a fresh reply key. Retrying an already accepted question/key/value is harmless,
even after another question is waiting. The answer is recorded in the event
history and returned to the model as the question’s correlated tool result.
For a PostgreSQL-backed worker run, record the answer and then resume explicitly:

```sh
cargo run --locked -p hudson-worker -- --database hudson --namespace my-agent \
  --reply-run RUN_UUID --reply-text Boston --request-key answer-1 --question-id QUESTION_ID
cargo run --locked -p hudson-worker -- --database hudson --namespace my-agent \
  --config agent.json --resume RUN_UUID
```

Use `--reply-file reply.json` instead of `--reply-text` for a JSON value (up to
1 MiB, also subject to the run’s payload limit). Recording a reply needs no agent
configuration or provider credentials and executes no tools or models. Use the
same database/namespace as the original run. Library applications use
`Runtime::provide_input` and then drive the run.

When enabled, `ask_user` is reserved and cannot be a customer tool alias. It is
disabled by default for unattended agents. Mixed question/tool batches and invalid
question arguments fail before any tools from that batch execute.

## Hosted checks

The `Harness checks` GitHub Actions workflow runs the aggregate check script on
pull requests and pushes to `main`. It installs the repository-pinned Rust toolchain
and starts an isolated PostgreSQL cluster on `/tmp`, then includes real local
Temporal tests and all smoke scripts. Provider calls use local fixtures; provider
credentials are not needed. Check and database logs are retained for seven days.
The separate Harbor Docker benchmark plumbing remains an explicit check described
in its integration README.

The Harbor worker build uses a fresh Rustup directory under the job's temporary
directory. It still selects `rust-toolchain.toml`, while avoiding incomplete
toolchain components preinstalled in the hosted runner image.
