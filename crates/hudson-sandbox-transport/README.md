# Public sandbox SDK transport foundation

Contributes to Hudson #17 using `sandbox-client` 0.0.0 pinned to Hudsonbox
merge commit `df4b53cb8e3e22bd808b0e61f25ac43f3eaeb976` from
[PR161](https://github.com/hudson-frontier/hudsonbox/pull/161), independently
merged with passing checks. This crate does not dispatch mutations, enable Sandbox
tools, execute a VM, or attach agent task results to Hudson runs.

`PreparedBody` serializes the SDK's typed create/execute/destroy models.
`prepare_request()` persists the exact canonical bytes and constructed client's
profile fingerprint through Store fencing. It sends nothing and consumes no send
intent. `from_canonical_json()` rejects serialization changes, including defaults,
whitespace, field order, and unknown fields. Absolute deadlines are preserved.
Command/image/policy selection still needs an operator-approved mapping; never
infer it from a package name.

The SDK's optional generation precondition remains omitted for legacy commands.
A compatibility regression verifies their exact persisted canonical bytes across
the SDK upgrade.

The profile fingerprint is **lowercase hexadecimal SHA256 of the UTF-8 bytes from
`serde_json::to_vec(&[client.identity().origin, client.identity().project_id])`**.
Origin uses the SDK's validated URL normalization, including the trailing `/`.
Project is the trusted credential-file declaration. Tokens, CA material, file
paths and credential names are excluded. Same-scope token rotation preserves this
fingerprint; origin/project drift changes it. The SDK exposes an immutable
constructed-client snapshot, so later file changes cannot relabel that client.

`observe_binding()` reads the owning actor's Store binding and fence, compares
that fingerprint **before GET**, polls the original operation exactly once, and
records a matching receipt under existing Store ownership, immutable-request and
current-attempt checks. It does not wait, resubmit, cancel, or destroy. Result and
error payloads stay bounded (caller limit, maximum 64 KiB), sensitive and workload
controlled. The SDK applies that limit to response bytes before JSON decoding,
including whitespace and additive fields that its typed model ignores. The
observer also checks the serialized typed response before releasing payloads.
An oversized success response is an SDK protocol error and cannot record Store
completion; an oversized problem retains its HTTP status without body fields.
Types carrying payloads intentionally lack Debug. Unknown/expired or
mismatched operation/sandbox/kind require reconciliation without writing completion.
A successful execute receipt is operation evidence, not agent task success.

The initial read validates the current pending operation, request digest and
attempt before network IO; a historical binding alone cannot authorize a GET.
Receipt recording repeats these checks after the GET. Both synchronous Store
operations run on Tokio blocking workers, so a PostgreSQL wait cannot stall the
async executor. Only identity values and shared Store handles are cloned for
those workers. Cancellation may leave an already started storage task running;
it cannot cause a mutation to be sent or bypass receipt fencing.

Matching the declared profile prevents accidental configuration drift. It is
**not authenticated project identity or guest policy proof**. The sandbox server
must still validate bearer authorization and project ownership. SDK configuration
retains private-file, HTTPS/trusted-CA, no-proxy, no-redirect and no-retry checks.
Auth failure never becomes completion. Explicit runtime admission/operator mapping,
policy enforcement, bounded result attachment and real Linux/KVM validation remain
required before effectful dispatch.

`inspect_unverified()` remains a non-authoritative qualification probe; its results
must not establish Store completion or cleanup. Prefer owned `observe_binding()`.

Tests use the real SDK against local HTTPS and generated trusted test CAs. The
feature-gated `synthetic_sandbox_protocol_fixture()` creates a fresh in-memory
synthetic Sandbox attempt for protocol tests; it cannot mutate an existing Store
or database namespace and proves no real runtime admission or VM execution.
Direct SDK POSTs exist only in request-shape fixtures; 202 is acceptance, not
completion. No test claims Temporal integration or Linux/KVM enforcement.

Run `cargo test --locked -p hudson-sandbox-transport` from the repository root.
