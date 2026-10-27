# Harness controls and context

The default loop accepts durable steering and follow-up messages from the run's
actor. These messages are user content. Agent definitions, tools, credentials,
provider options, policies, goals, and budgets keep their existing authority.

## Queued input

```sh
hudson agent steer RUN_UUID --text 'Use the latest report' --request-key direction-1
hudson agent follow-up RUN_UUID --text 'Add a summary' --request-key summary-1
hudson agent inbox RUN_UUID
hudson agent clear-inbox RUN_UUID
```

The HTTP controls are `POST`, `GET`, and `DELETE /runs/{id}/messages`. POST accepts
`kind` (`steer` or `follow_up`), arbitrary JSON `input`, and `request_key`. It
returns a receipt with `id` and `kind`. GET returns pending entries with their
receipt and value; DELETE returns a cleared count. These controls never start or
resume execution.

At each complete boundary the loop consumes one steering message first. When
there is no steering message, a follow-up is consumed after the current answer
passes verification. That answer remains in the conversation and another model
request begins in the same run. The subsequent answer must still satisfy the
pinned goal and output schema. The run finishes after verification succeeds and
no eligible input remains.

Messages queued during model or tool execution wait for a complete exchange.
They cannot answer an outstanding question, approve a tool, or reconcile an
uncertain effect. Use the existing correlated reply, approval, recovery, and
resume controls for those waits. Cancellation and a fresh run abandon a task's
original goal.

Each run permits 16 pending messages, 64 KiB of pending serialized data including
queue metadata, and 128 lifetime request keys. Individual values must also fit
the run's payload limit. Keys contain 1–128 UTF-8 bytes without control
characters. Reusing a key with the same kind and value returns its original
receipt, including after consumption, withdrawal, cancellation, or completion.
Changing the contents conflicts. Receipts retain hashes instead of another copy
of message values. New messages are rejected after terminal state or cancellation.

Queue consumption, checkpoint changes, source artifacts, and operation intents
commit together. An enqueue or withdrawal racing with preparation makes that
preparation retry. It cannot disappear behind a completion commit. PostgreSQL
reconnects preserve pending messages and receipts. The `input_queued`,
`input_consumed`, and `input_queue_cleared` events identify receipts or counts;
their payloads do not duplicate user messages.

## Context headroom

```json
{"context":{"offload_bytes":8192,"recent_exchanges":2,"excerpt_bytes":1024,"page_bytes":1024,"reserve_bytes":4096}}
```

`reserve_bytes` starts archival before a portable checkpoint or action reaches
the hard context limit. It is advisory: the original task, latest user direction,
recent complete exchanges, and pending tool/results are retained even when they
use the reserve. Requests exceeding the hard limit still fail explicitly. The
reserve must be below the context limit and at most 64 KiB. It defaults to zero
and is omitted from default policy serialization, preserving existing fingerprints.

Compaction counts complete exchanges in one scan instead of repeatedly cloning
and draining the history to recount them. It preserves full source artifacts,
provider continuation metadata, and complete tool-call/result groups. Duplicate
call IDs and incomplete groups do not form archive boundaries. The archive index
contains partial verbatim evidence and links to the original run-scoped source.
There are no hidden summarization requests or exact-token claims.

The comparison workload is `context::tests::compaction_workload`: 120 complete
exchanges, 512-byte results, a 16 KiB checkpoint, and five timed preparations:

```sh
cargo test --locked --release -p hudson-core --features fixtures --lib compaction_workload -- --ignored --nocapture
```

## Provider reasoning controls

Configure a compatible model explicitly, then select an effort:

```json
{"reasoning":{"type":"effort","effort":"low"}}
```

OpenAI chat requests send `reasoning_effort`. The Gemini compatible endpoint
uses the same field; this adapter accepts `none`, `minimal`, `low`, `medium`, and
`high` there. Native Anthropic requests select adaptive thinking and
`output_config.effort`; `none` and `minimal` are rejected there. The chosen model
must support the requested effort. The harness does not infer capabilities from
model-name strings or contact a model catalog during configuration.

For native models supporting manual thinking budgets:

```json
{"max_output_tokens":4096,"reasoning":{"type":"budget","tokens":1024}}
```

Manual budgets are Anthropic-only, at least 1024 tokens, and below
`max_output_tokens`. The overall output limit remains unchanged. Newer Anthropic
model families reject manual mode; select adaptive effort there. Consult the
[thinking documentation](https://platform.claude.com/docs/en/build-with-claude/extended-thinking),
[OpenAI request reference](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create),
and [Gemini compatibility reference](https://ai.google.dev/gemini-api/docs/openai)
for model-specific availability.

Reasoning settings are pinned in the model transport fingerprint. Changing or
removing them requires a new agent version. Omitting them preserves existing
fingerprints and defaults. Failed or ambiguous transports follow the existing
recovery contract; these controls introduce no retries.

## Coverage and host boundaries

Operator-owned model profiles accept the same optional `reasoning` setting.
Customer agents select a provisioned profile by its immutable reference; their
publication requests cannot override provider options. Omitted profile settings
retain the previous serialized representation.

| Harness pattern | Hudson implementation |
| --- | --- |
| Effect-free loop and replaceable backends | Backend/Engine contracts, versioned checkpoints, runtime-owned effects |
| Durable steering and follow-ups | Actor-scoped inbox, bounded receipts, atomic boundary consumption |
| Context preparation | Scoped memory/dependencies, archival, complete exchanges, advisory reserve |
| Provider continuation fidelity | Assistant text, signed thinking, redacted blocks, thought signatures |
| Model controls and spend bounds | Pinned provider/model/options, output limits, run/shared call budgets, reported usage |
| Tool lifecycle and extensions | Registered executors, schemas, dispatch authorization, approvals, recorded outcomes |
| Model correction of tool arguments | Correlated validation errors before dispatch, normal budgets, approval on each corrected write |
| Provider failure diagnostics | Bounded error-body inspection, fixed categories, no response-body disclosure or hidden retries |
| Cancellation and recovery | Attempt fencing, uncertain-effect reconciliation, child cancellation |
| Skills and lazy resources | Frozen catalogs, portable packages, scoped bounded resource reads |
| Durable graphs and concurrency | Dependencies, child workflows, repetition bounds, shared budgets, Temporal scheduling |
| Observation and caller control | Durable event pages, CLI follow/session, HTTP and Python callers |

Provider token streaming, live model/provider mutation, transcript forks, and
hot-loaded executable plugins remain outside the current public contract. The
runtime records complete provider outcomes and keeps immutable execution
bindings. Parallel work uses bounded child scheduling; tools in one ordered
batch retain sequential dispatch and its recovery semantics. Real sandbox
execution is tracked separately from the protocol implementation.

`scripts/smoke_inbox.py` checks the queue against local model/tool fixtures,
PostgreSQL, the actual CLI, and an API restart. The full checker includes it
with `--database --temporal`, without paid provider requests.

## Tool correction and provider failures

A known tool call whose arguments fail its pinned input schema receives an
`invalid_arguments` tool result. It has a failed operation with no execution
attempt, approval request, or external effect. The model can correct the call in
its next ordinary request. Invalid calls count against tool/operation budgets;
correction requests count against model-call budgets. There is no separate repair
loop or unbounded retry allowance. Arguments are never coerced into another type.

Current policy denial takes precedence over validation feedback. A corrected
call creates a new operation and cannot reuse a previous approval. Cancellation
and uncertain earlier effects stop ordered batch dispatch as before. Valid
siblings execute once in their original order; their recorded results and the
correlated errors are returned together. Unknown tools, duplicate IDs, malformed
provider output, and changed backend requests still fail closed. The built-in
`ask_user` retains its separate single-call validation and correlated reply flow.

HTTP model errors retain the status and a fixed category in the private attempt
diagnostic. Categories distinguish invalid requests, explicit context-limit
codes, authentication, permissions, missing resources, request size, billing or
quota, throttling, timeouts, conflicts, and provider unavailability. Classification
reads at most 32 KiB plus one overflow byte. Malformed, oversized, or interrupted
error bodies fall back to the status category. Only recognized structured codes
are used; arbitrary message text, codes, headers, and bodies are never recorded.
Context detection is diagnostic and does not claim universal provider coverage.

Billing and quota errors can share HTTP 429 with temporary throttling; recognized
quota codes therefore take precedence over the status fallback, following the
[OpenAI error contract](https://developers.openai.com/api/docs/guides/error-codes).
Native status mapping follows the
[Anthropic error contract](https://platform.claude.com/docs/en/api/errors).
No diagnostic permits an automatic retry, provider switch, budget refund, or
silent context loss. Transport ambiguity still requires existing recovery controls.
`tool_feedback` and `provider_failures` integration tests exercise these contracts;
the PostgreSQL variant verifies correction across actual database reconnects.

## Adoption audit

The harness-oriented review covers the following mechanisms. Existing durable
controls are reused where they already provide the required behavior.

| Mechanism | Delivery and evidence |
| --- | --- |
| Replaceable loop and provider boundary | Pure `Backend`/`Engine`, portable checkpoints, `ModelExecutor`; engine and resume tests |
| Steering at complete boundaries, follow-up after answers | Durable inbox with actor checks, receipts, queue fencing and cancellation; `inbox` tests and `smoke_inbox.py` |
| Complete tool groups and context preparation | Whole-exchange archival, recent evidence, original task/latest direction and headroom; context and dependency-context tests |
| Recoverable tool failures | Structured execution errors plus pre-dispatch argument correction; execution and tool-feedback tests |
| Provider fidelity and diagnostics | Continuation text, signed thinking, opaque signatures, explicit reasoning, usage and bounded safe errors; chat, reasoning, token-usage and failure tests |
| Cancellation barriers and durable task scheduling | Exact attempt fencing, no ambiguous replay, child cancellation and bounded Temporal concurrency; recovery and Temporal tests |
| Tool discovery and lazy context | Pinned MCP schemas, frozen skills, paged package resources and scoped artifact retrieval; MCP, skill and context tests |
| Extension authority | Versioned registered/HTTP/MCP tools with policy and approval checks; publication and execution tests |
| Sessions and observation | Durable run/checkpoint history, event cursors, API/CLI/Python controls and independent task submission; reconnect and caller smokes |

Token streaming would add provisional observations to a contract that currently
records complete, durable outcomes. Live model mutation and transcript forks
would need explicit execution-binding, lineage, budget and approval rules.
Executable plugin reload requires a trusted host lifecycle; frozen capabilities
remain the extension mechanism here. These features are unimplemented, rather
than implied by the observation or session controls above. Terminal rendering,
editor bindings, provider account login flows and document editors are outside
the current harness interface. Real sandbox isolation acceptance remains open.

## Preview migration

JSON configurations and persisted defaults retain their existing fingerprints.
Rust embedders using struct literals must add `reserve_bytes: 0` to
`ContextPolicy` and `reasoning: None` to `ModelProfile`; context policies can also
use `..Default::default()`. Exhaustive matches on `Input` must handle `Queued`.
Custom backends may reject queued input; the store exposes this control only to
the default loop's checkpoint contract. HTTP callers with exhaustive event enums
must add the three queue event types from the reviewed preview revision.

The first accepted queued message promotes its PostgreSQL namespace row to
storage format 2, atomically with the message and receipt. Current binaries read
formats 1–3; older binaries reject format 2 instead of silently dropping the
inbox. The format does not downgrade after messages are consumed or withdrawn.
Upgrade all database writers before using queue controls. Namespaces that never
use queues retain format 1; no automatic downgrade migration is provided.

[Offline experiments](experiments.md) promote a namespace to format 3. Upgrade
all writers before using either control; experiments also freeze their stage
inputs against queue steering and follow-ups.

Reserved headroom can archive evidence earlier and increase explicit retrieval
tool calls. It introduces no hidden summaries, and those calls still consume the
ordinary run/tool budgets. Zero reserve retains the previous hard-limit trigger.
