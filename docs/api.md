# Local HTTP API

The machine-readable contract is [openapi.json](openapi.json). Each server also
serves it at `GET /openapi.json`. Credential-protected instances authenticate that
request through PostgreSQL; serving the schema makes no model calls. The live
schema declares bearer authentication as required on protected instances and
no authentication on unprotected local instances.
Import that document into an OpenAPI-capable client to inspect request and
response schemas. The API is currently a preview with unversioned paths.

Start with the [README server command](../README.md#use-the-http-api). This host
binds to loopback and uses one configured workspace/actor identity. Agent definitions
can be loaded at startup or published through authenticated catalog mode. Credential issuance uses separate operator commands.

| Request | Purpose |
| --- | --- |
| `GET /health` | Read configured/fixture mode and persistence status |
| `POST /runs` | Submit JSON input to the root agent; optionally deduplicate with `request_key` |
| `GET /runs/{id}` | Read status, question/approval wait, usage and result |
| `GET /runs/{id}/schedule` | Inspect durable scheduling publication evidence, without executing work |
| `GET /runs/{id}/children` | Discover direct children and their statuses |
| `GET /runs/{id}/events?after=0&limit=100` | Poll a bounded event page after an exclusive sequence cursor |
| `POST /runs/{id}/resume` | Schedule continuation with the configured executor |
| `POST /runs/{id}/input` | Record an answer for `question_id` and schedule continuation |
| `POST /runs/{id}/cancel` | Request cancellation of the run and known children |
| `GET /operations/{id}` | Inspect exact arguments, status and approval details |
| `POST /operations/{id}/approval` | Submit `{"approved":true}` or `{"approved":false}` |

Submission returns `202` with a `run_id`; it does not promise completion. Poll the
run until it waits or reaches a terminal status. Approval, reply, and resume
schedule work and return the run handle. Inspect the saved outcome afterward.
Resume, reply, and approval reject a changed saved budget binding with `409`
before accepting the control action. Restore the original budget group to continue
a nonterminal run; starting a new batch requires a new run.

After restarting the server, use the same configuration/database/namespace and
explicitly resume interrupted runs. Unknown external effects need the separate
[recovery workflow](recovery.md).

Migration from the earlier preview API: an omitted `limit` now returns at most
100 events rather than all remaining history. HTTP consumers must drain pages
before treating history as complete; the array shape alone does not indicate
completion. There is currently no published Hudson SDK to update. The CLI
`hudson-cli events RUN_UUID --after N` reads one page, so repeat it with the last
returned `sequence` until the result is `[]`. Use the `follow` command below for automatic draining and live observation. Internal Rust
`Store::events()` callers continue receiving the complete history.

Event polling returns an array in ascending sequence order. `limit` defaults to
100 and must be between 1 and 1000. After each nonempty page, set `after` to its
last event's `sequence` and request the next page until it is empty. Retain that
cursor to reconnect, and poll again for later events. A full page does not imply
that more events exist. Even an empty page checks run ownership. These limits
bound event counts, not payload bytes; arbitrary JSON payloads may still be large.
The PostgreSQL backend still reads its namespace JSONB snapshot; paging avoids
cloning and serializing the entire remaining history but does not change storage.

The body limit is 64 KiB. Runtime errors use `{"error":"..."}`; malformed JSON,
UUIDs, content types, and oversized bodies can receive plain-text framework
errors. Handle status before decoding the body. Event payloads and run results
are arbitrary JSON. Responses expose no executable checkpoint or credentials.

A reply must include `input`, `request_key`, and the `question_id` copied from
`run.wait.question_id`. Reuse all three when retrying the same answer. A changed
answer under the same key or an answer for the wrong pending question conflicts.

Follow a run without changing its execution state:

```sh
hudson-cli follow RUN_UUID --after 0
# Reconnect from the final event sequence your client processed:
hudson-cli follow RUN_UUID --after 123 --page-size 100 --poll-ms 1000
```

`follow` writes flushed JSON Lines: `type: "event"` records carry the original
event plus `after`, and `type: "run"` records carry changed run status, waits,
usage and the terminal result. It reads status before draining all event pages,
so it includes final events before exiting on a terminal run. Waiting states
remain observable; use a separate CLI command to reply, approve, resume or cancel.
Ctrl-C stops observation only. Reconnect with `--after` set to the last event your
consumer fully processed; replay is possible if that cursor was not retained.
No checkpoint file is automatically written.

Successful completion exits zero. Failed/cancelled runs, invalid responses,
authentication failures and exhausted retries exit nonzero; errors include the
last emitted cursor. On Unix, Ctrl-C uses the normal interrupt exit status.
Transport failures, 429 and 5xx responses retry the same read up to
`--max-retries` (default 5, range 0–10), with exponential delays capped at 30
seconds. Each HTTP request times out after 15 seconds. Poll intervals range from
50 to 60000 milliseconds; event pages from 1 to 1000. Each response is bounded to
8 MiB; reduce page size for large event payloads. Invalid JSON, ordering, run
identity and other HTTP errors stop immediately. The existing authentication,
HTTPS/loopback URL restrictions and redirect rejection apply to this command.

Provider usage may be absent. A zero `reported_model_calls` count means no usable
provider reports were recorded, not that the task cost nothing. Parent usage
covers the parent only; child usage belongs to each child run.

Server tests compile the schemas and validate HTTP responses against this
contract. The standard `scripts/check.py` command includes those tests.

## Separate API admission from Temporal execution

Start the API with durable storage and a task queue:

```sh
cargo run --locked -p hudson-server -- --config examples/analyst.json \
  --database hudson --namespace my-project --temporal-task-queue my-agents
```

Run the matching worker in another process:

```sh
cargo run --locked -p hudson-temporal -- --config examples/analyst.json \
  --database hudson --namespace my-project --task-queue my-agents worker
```

The existing `POST /runs` payload and run inspection/control routes remain the
same. The API commits the run and scheduling intent together, then returns `202`.
Acceptance means the request is saved, not that a worker has started or completed
it. The API makes no model or tool calls in this mode; it does not need a Temporal
connection or the configured model/HTTP-tool credential environment variables.
It publishes the same immutable definitions using an admission-only builder with
disabled executors. Execution credentials belong in the worker environment. A matching worker publishes saved requests and owns execution. An API
exit after acceptance does not discard the work. Reuse `request_key` for retries.

`GET /health` reports `mode: "temporal"`. `--temporal-task-queue` requires
`--database`. Both hosts must use the same configuration, storage namespace and
task queue. Controls validate the root scheduling target, including for delegated
runs. Approvals and replies are saved in PostgreSQL; the worker observes them on
its next tick. This mode does not adopt runs created by the local execution mode.

This remains a loopback API with one configured identity. Optional scoped bearer
authentication is described below; hosted multi-tenant deployment remains separate work.
Hudson Sandbox is a separate product and is not required for this mode. A future
adapter will connect its execution operations through Hudson's tool controls.

## Require scoped API credentials

For a credential-protected instance, run the API with `--require-api-token`,
`--database`, `--workspace-id` and `--actor-id`. Every route, including health and
OpenAPI, then requires exactly one `Authorization: Bearer <token>` header. The
stored credential must match the configured workspace and actor. Request bodies
cannot select an identity. The server remains loopback-only; terminate HTTPS at a
trusted reverse proxy when making it reachable from another host.

An installation operator with local database access issues a credential:

```sh
cargo run --locked -p hudson-server --bin hudson-credentials -- \
  --database hudson --namespace my-project \
  --workspace-id customer-one --actor-id backend \
  issue --label application-backend --ttl-seconds 86400
```

This prints the token once. Keep it in the calling backend's secret storage; only
its SHA-256 hash and ownership/expiry/revocation metadata are persisted. No public
HTTP endpoint can issue credentials. These commands require trusted database
access and are not agent tools.

Start the API and worker with matching identities:

```sh
cargo run --locked -p hudson-server -- --config examples/analyst.json \
  --database hudson --namespace my-project --temporal-task-queue my-agents \
  --workspace-id customer-one --actor-id backend --require-api-token
cargo run --locked -p hudson-temporal -- --config examples/analyst.json \
  --database hudson --namespace my-project --task-queue my-agents \
  --workspace-id customer-one --actor-id backend worker
```

Configure `HUDSON_API_TOKEN` in the calling process environment, then use:

```sh
cargo run --locked -p hudson-cli -- --api-token-env HUDSON_API_TOKEN \
  start --task 'Analyze these records' --request-key analysis-001
```

The CLI sends credentials only to HTTPS or loopback HTTP URLs, follows no
redirects, and marks the authorization header sensitive. Tokens are backend
credentials; this does not implement browser login or session cookies.

To rotate, issue a replacement for the same identity, update the caller, then
revoke the old token by the ID returned at issuance:

```sh
cargo run --locked -p hudson-server --bin hudson-credentials -- \
  --database hudson --namespace my-project \
  --workspace-id customer-one --actor-id backend revoke TOKEN_UUID
```

Authentication reads current persisted expiry/revocation on each request.
Revocation blocks later API requests; an already admitted run retains its identity
and continues. Use run cancellation when execution should stop. Tokens for this
identity can perform all the API's run controls, including approval decisions
permitted by runtime policy. There are no token-specific roles in this version.

The default unprotected mode remains a local development convenience. This is one
configured workspace/actor per server, not a hosted multi-tenant platform. Separate
customers need separate configured instances and storage namespaces. Hudson and
Hudson Sandbox use separate credentials and resource ownership; future integration
will use an explicit service adapter.

## Publish customer tools and agents

Run with `--catalog host.json` instead of `--config`; this requires `--database`,
`--require-api-token`, and `--temporal-task-queue`. The catalog belongs to the
configured workspace/actor and contains approved versioned model profiles and tool
connections. Worker credentials are never read by this admission API. Follow the
[complete CLI walkthrough](../examples/customer-api/README.md).

| Request | Purpose |
| --- | --- |
| `GET /capabilities` | List permitted model/connection references and limits |
| `POST /tools` | Publish `{request_key, tool}`; return an immutable revision receipt |
| `GET /tools/{name}/versions/{version}` | Read the public tool definition |
| `POST /agents` | Publish `{request_key, agent}` selecting tool revisions |
| `GET /agents/{name}/versions/{version}` | Read the public agent definition |
| `POST /runs` | Submit `{agent_ref, request_key, input}` for a published revision |

Publication returns `201` on success, including identical retries. Changed content
under the same version or retry key returns `409`. A policy or execution ceiling
violation returns `403`; unknown or unavailable references return `404`. Public
bodies cannot set identities, credentials, destinations, host paths or worker queues.
The existing 64 KiB body limit applies.

The run's shared model-call budget is per root submission, including its children.
A retry keeps the same counters. Another request key creates another task with its
own allowance. Model-call limits do not represent currency quotas. Tools inherit
host effect/approval requirements and can request stricter handling.

The same run status, approval, reply and cancellation routes work after API restart.
Workers started with `hudson-temporal --published ... worker` load the saved agent
and execution credentials independently. Never mix configured and published
workers on the same queue. The live OpenAPI document shows publication routes only
in catalog mode and requires `agent_ref` and `request_key` there.

### Interactive terminal operator

`hudson` (no subcommand) opens a terminal prompt; `hudson session --agent NAME
--agent-version VERSION` selects a published agent. This client uses the same
authenticated API and exclusive bounded event cursors as `follow`, yielding at
waits for explicit reply/approval actions. It does not create a host, automatically
retry mutations, upload the working directory, or change configured tool
authority. See the README's interactive session instructions.

## Scheduling publication receipts

`GET /runs/{id}/schedule` returns `root_run_id`, `status` (`unscheduled`,
`pending`, or `published`), and nullable `last_attempt_at` / `published_at`
Unix-millisecond timestamps. Delegated runs inherit the owned root's receipt.
`unscheduled` means no saved remote scheduling request, as with the local driver;
`pending` means no acknowledgment was recorded. An interrupted acknowledgment
can leave the request pending even if Temporal accepted it. The existing pump
retries publication using the stable workflow identity, without replaying effects.

`published` means the scheduler accepted the workflow; it does not prove a worker
started, a task completed, or sandbox cleanup succeeded. Inspect the ordinary
run and operation views for those outcomes. A cancelled run may retain pending
or published historical intent; its run status governs whether it remains eligible
for scheduling. Legacy acknowledged snapshots may have no publication timestamp.
No worker queue, configuration, or credentials appear in this receipt. Inspection
requires the same authenticated run ownership as other read endpoints and never
contacts Temporal or mutates a run.
