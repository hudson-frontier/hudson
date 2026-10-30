# Preview HTTP compatibility

The `hudson-preview-2026-10-30-r3` revision adds actor-scoped allocation views and opt-in model-spend
approval waits. Model operations can now use the existing approval wait type
when configured thresholds require it. Monetary values are decimal strings;
exhaustive event consumers must accept `spend_reconciled` without treating it
as model-output recovery or resuming a run.
missing priced usage is `null`. Rust `ModelProfile` literals must add
`spend: None` to retain disabled behavior. PostgreSQL schema 4 and local snapshot
envelope 2 protect spend state against older writers. See [migration and
allocation semantics](spending.md).

The `hudson-preview-2026-10-30-r2` revision adds explicit image submission,
optional image attachments on fork/import, and image blocks in portable
transcripts using `format_version: 2`. Text-only exports retain format 1.
Exhaustive transcript content consumers must accept the `image`
variant. Fork bodies now use the same 5 MiB allowance and four shared request
slots as import/image submission. Ordinary `/runs` JSON retains its meaning and
64 KiB body ceiling. Rust bridge configuration literals need `images: false`
unless opted in; exhaustive `Content` and `Input` matches must handle `Image`
and `StartImages`. Existing JSON bridge configurations default to no images.
Use matching binaries in a namespace containing image operations: older readers
reject those new typed variants. See [image transport and bounds](images.md).

The current unversioned HTTP routes are a preview. The client contract is
identified by `contract_id` in [api-compatibility.json](api-compatibility.json),
independently of the release version. No stable `/v1` API or cross-version
compatibility guarantee is declared. Use matching server/client revisions and
run local acceptance before upgrading.

The `hudson-preview-2026-10-30-r1` revision adds the optional SSE/WebSocket event
routes and the `model_output_delta` event type. Clients with exhaustive
event-type enums must accept this value before upgrading. Deltas contain
`attempt_id`, `text` and `provisional: true`; partial text never replaces a
verified final result. The default bridge wire format and existing HTTP routes
keep their behavior. Rust callers constructing `BridgeConfig` or `BridgeRequest`
as literals must add `stream: false` to retain nonstreaming behavior; old JSON
configurations keep that default. `ModelExecutor::call_observed` has a default
implementation so existing custom executors remain compatible. See
[streaming limits and reconnect semantics](execution-adapters.md#provisional-text-streaming).

The earlier `hudson-preview-2026-10-17-r2` revision adds the actor-scoped
`/runs/{id}/messages` queue controls and the `input_queued`, `input_consumed`, and
`input_queue_cleared` event types. Existing routes retain their request and
response shapes. Clients with exhaustive event-type enums must accept these
values before upgrading. The bundled Python caller already treats event payloads
as arbitrary JSON; its queue methods require a server implementing this revision.

Within a reviewed contract revision, clients tolerate new optional response
properties and preserve arbitrary JSON results/event payloads. Adding optional
request properties or endpoints is compatible when existing defaults, status
codes, authorization, bounds and idempotency semantics remain unchanged.
Clients must not assume exhaustive response object properties.

The additive `/harnesses` discovery route lists configured execution adapters and
their declared common controls. It retains this contract ID, requires the same
host authentication and exposes no endpoint or credential configuration. Existing
request/response schemas are unchanged. The Python caller's `harnesses()` method
requires a server implementing this addition.

The initial conversation export, fork and import routes retain revision r1.
They use a separate `format_version: 1` portable transcript and preserve existing
submission behavior. Imported context does not carry execution authority. The
initial larger 5 MiB body allowance was restricted to `/runs/import`; r2 extends
it to image submission and fork. Other routes retain 64 KiB. The Python transfer
methods require these routes.

Removing/renaming routes or properties, adding required request fields, changing
types or defaults, tightening accepted values, changing terminal/wait semantics,
introducing new status values, or changing identity/retry-key interpretation
requires a new preview contract ID, migration notes and updated fixture-backed
client acceptance. Preview clients fail visibly on unknown run statuses. A
future stable API will use explicit versioned paths and a separately documented
support window before any preview route is retired.

`python3 scripts/check_api_compatibility.py` compares a canonical digest of all
static paths, schemas, parameters, responses and security declarations with the
reviewed baseline. Top-level API information and server URL examples are
excluded so release-version changes do not invalidate the baseline. Every other
change, including additive changes and schema descriptions, requires explicit
baseline review. The check detects drift; it does not decide compatibility.
For an intentional change, assess the rules above, update acceptance tests, then
replace `contract_sha256` with `contract_digest()` from the checker. Change
`contract_id` and add migration notes when the change is breaking.

The served OpenAPI document selects authentication and submission schemas for
the configured host mode. Existing server schema tests and the real local
publication fixture validate those variants; a static digest cannot establish
runtime behavior. The [Python client](../integrations/python/README.md) is a
thin local caller, not a generated SDK or a hosted authentication system.
