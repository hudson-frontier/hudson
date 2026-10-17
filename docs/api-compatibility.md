# Preview HTTP compatibility

The current unversioned HTTP routes are a preview. The client contract is
identified by `contract_id` in [api-compatibility.json](api-compatibility.json),
independently of the release version. No stable `/v1` API or cross-version
compatibility guarantee is declared. Use matching server/client revisions and
run local acceptance before upgrading.

The `hudson-preview-2026-10-17-r2` revision adds the actor-scoped
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
