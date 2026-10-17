# Sandbox adapter recovery contract

Tracked by Hudson #17 and hudson-sandbox #112. Public HTTP schema baseline:
`hudson-infinity/hudson-sandbox` commit `aa5cef5c51b7c735e6ede82d2c4f6e90f4f4f119`,
`api/openapi.json` and generated `crates/sandbox-client/src/models.rs`.

Durable request preparation checks the run's payload byte limit before JSON
validation. Invalid or oversized bodies do not bind an idempotency key or consume
an admission intent; callers can correct the body before sending. Validation
preserves the exact original bytes without retaining a parsed JSON tree.

Reproduce the validation-only comparison with Rust 1.92.0 in release mode:

```sh
cargo build --release --locked -p hudson-harness --example json_validation
/usr/bin/time -f 'peak_rss_kib=%M' target/release/examples/json_validation value
/usr/bin/time -f 'peak_rss_kib=%M' target/release/examples/json_validation ignored
```

On x86_64 Ubuntu under WSL, three paired runs each validated the same 46,081-byte
JSON array 1,000 times. The original `Value` parse took 3.062–3.199 seconds
(median 3.196), with median peak process RSS 3,032 KiB. `IgnoredAny` validation
took 0.382–0.423 seconds (median 0.398), with median peak process RSS 2,296 KiB.
Both paths check the same bounded JSON input; only the original allocates and
drops a complete JSON tree. This measures parsing, not PostgreSQL transactions,
external execution or end-to-end agent throughput.

The backend already has an authenticated HTTP API and generated Rust client.
Hudson must reuse that transport rather than invent another guest RPC protocol.
Create, execute, destroy and cancellation are independent admitted operations.
`Idempotency-Key` identifies a request; it does not prove exactly-once guest effects.

`adapters::sandbox_recovery` is a transport-independent decision primitive for a
future persisted activity, not enabled sandbox execution. Existing dispatch still
rejects Sandbox tools. The primitive does not establish isolation or authority.

An operator explicitly binds a package name to a pinned image and fixed command,
working directory and output limit. Agent arguments must never become an executable,
shell string or arbitrary argv. Resource/network admission and credential scope must
be enforced by the backend before enabling this mapping. Current create schema has
image/resources/correlation fields and no client network-policy field; do not claim
per-run network isolation from that schema alone.

Persist run/workspace identity, immutable request body, idempotency key and admission
intent before network dispatch. Persist attempted=true before sending. Save the
returned sandbox and operation identities atomically with run state. After restart,
inspect the original operation. A lost admission response without an operation ID
requires reconciliation using backend admission evidence; this primitive deliberately
refuses to resubmit. Never rotate a key to recover an uncertain execute.

Receipts must match the recorded operation, sandbox and operation kind. Unknown,
expired, missing or unrecognized outcomes require explicit reconciliation. A successful
execute receipt is command evidence, not proof of agent task success. Retrieve bounded
outputs on the same operation, preserve truncation/simulated/guest-reported evidence,
and authorize result ownership using the run's workspace.

Cancellation must inspect both its own receipt and target execution evidence; an
accepted cancel is not termination proof. Destroy follows cancellation/reconciliation
and retains its own stable key and receipt until cleanup is confirmed. Neither retry
nor timeout may silently create a new sandbox or host process.

Remaining integration: durable store/activity wiring, pinned SDK dependency, secret
redaction/auth setup, explicit operator registration and enforced policy checks,
output/result attachment, cancellation targets, cleanup evidence, and local mock
acceptance followed by real Linux/KVM validation. The JSON roundtrip tests represent serialized recovery plans, not implemented durable storage. The unit tests cover restart,
lost response, mismatched receipts and expired/unknown status without VM claims.

The receipt's omitted `response_expired` means false in the pinned OpenAPI schema;
this matches the generated SDK default. The receipt cannot bind a Hudson workspace
or run because those fields are not in the public operation response. The future
store must enforce that binding and immutable request ownership; public Rust plan
fields are serialization data, not an authority boundary. Structural validation
rejects empty identities/keys and execute/destroy plans without a sandbox identity.
Reproduce the selected schema with `scripts/extract-sandbox-operation-schema.py`
against the verified pinned repository's `api/openapi.json`.

Public v1 mutation keys are project-wide, 16–128 ASCII alphanumeric or `._-`.
A 202 AdmittedResponse binds operation identities only; its status never substitutes
for a completion receipt. A 409 conflict is not admission. A 410 expired response
requires reconciliation, even when it retains an operation ID. The admission binder
rejects other HTTP statuses and different existing identities/status URLs. Persist
exact request bytes including the absolute command deadline before dispatch.

## Durable request storage foundation

`Store` sandbox binding methods persist an exact JSON request body and immutable
idempotency key in the existing state document before consuming a single admission
send intent. The binding belongs to the owning actor's run, its pending Sandbox
Tool operation, the current admitted attempt, and the operation request digest.
Registered tools and model operations cannot acquire this authority. Repeated
preparation preserves the original bytes; changed bodies, metadata, or a key reused
elsewhere under the same backend profile conflict. A 202 response binds IDs only. Unknown
attempts permit receipt reconciliation without granting another send. Terminal
receipt status is immutable, including destroy: requested cleanup is not confirmed
cleanup.

This is storage protocol infrastructure for #17, not a working sandbox executor.
Production dispatch still rejects Sandbox execution. Its tests seed an admitted
Running Sandbox Tool attempt explicitly; they do not demonstrate runtime approval
or VM execution. The JSON restart and opt-in PostgreSQL reconnect tests demonstrate
persistence and fencing. Transport, operator-approved package/command mapping,
verified SDK endpoint/project identity binding, and real Linux/KVM validation remain
required before enabling dispatch. The request also requires an immutable 64-character lowercase SHA256 profile
fingerprint of canonical operator-bound origin plus project, excluding the token.
The trusted worker constructor must verify and derive it; an agent-supplied label
is insufficient. The store validates its encoding and immutability, not its
provenance. Keys are unique across phases and Hudson workspaces sharing that
backend profile. A future transport must verify that the actual SDK client origin
and project produce the stored fingerprint before sending or polling. The current
SDK does not expose that identity, so this remains an explicit live transport gate.
