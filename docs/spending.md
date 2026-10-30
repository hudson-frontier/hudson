# Model allocation admission

An operator can add `spend` to an agent configuration or installation-owned model
profile. Existing configurations leave it disabled. Policies pin a per-call
allocation, input/output rates, and up to four quota scopes. Amounts use integer
nano USD: 1000000000 units represent one USD. Public monetary amounts are decimal
strings so clients preserve values beyond JavaScript's exact-integer range.

This example uses fictional fixture rates:

```json
{"spend":{"allocation_nano_usd":100000000,"rates":{"input_nano_usd_per_million_tokens":1000000000,"output_nano_usd_per_million_tokens":1000000000},"quotas":[{"id":"model-pool","scope":"workspace","limit_nano_usd":10000000000,"soft_nano_usd":5000000000,"approvers":["developer"]}]}}
```

Choose rates and allocations explicitly for the pinned model and its request
limits. Allocations govern admission; priced usage reflects reported tokens and
operator-pinned rates. Provider charges can differ. Reported input includes
cached tokens at the configured input rate. No provider price lookup or hidden
model/token-count request occurs during admission.

Quota scopes are `run`, `agent_version`, `workspace`, and `actor_day`. The last
uses the UTC day of allocation admission and the original run actor. A delayed
approval retains that quoted day. Shared quota IDs require identical scope,
limit, threshold and approver definitions. Publishing another agent version
does not reset workspace or actor-day accounts. Teams with spend controls must
price every child and retain parent workspace/daily quota definitions.

Every applicable account is checked in the same transaction before any account
is reserved. A hard allocation failure prevents model IO. Crossing a soft
threshold requires an approver accepted by all triggered quota scopes. The
approval binds the exact model-operation digest; changing an instruction/input
or reusing approval for another operation cannot authorize execution. Shared
accounts and frozen scopes are checked again before dispatch.

Inspect `GET /runs/{id}/spend` or `GET /operations/{id}/spend`. A reserved entry
shows the amount before execution. Use the existing operation approval route
with `{"approved":true}` or `false`. The fixed-identity HTTP host needs an
appropriate configured approver; separate trusted Rust controls can use other
authorized approver identities. Model approval receipts are capped at 64 per
operation. Approval authorizes that allocation; it does not raise a quota.

An entry progresses through `reserved`, `in_flight`, then `priced` or `unknown`.
Only a valid usage report produces priced usage. Missing or lost reports retain
their full allocation, even when a run ends. Known usage rounds upward to the
nearest nano USD with checked integer arithmetic. An unexpected priced overrun
or unrepresentable amount freezes its accounts and prevents returned tool
proposals from executing. No billing or latency guarantee follows from a quote.

Cancellation and rejection release only reservations whose model attempt never
started. Interrupted attempts retain allocations. After independently verifying
a provider report, a trusted operator can call `Store::reconcile_spend_usage`
with the original actor, operation, attempt, usage and evidence. This control is
absent from HTTP. Exact receipt retries are idempotent; changed receipts and
live/stale attempts are rejected. Evidence is hashed in the audit record.
Pricing a report does not recover a lost model response or resume execution.

Policies cannot be changed, removed or enabled after runs have started under an
agent version. Publish a new version to change pricing. A changed shared quota
definition needs a new quota ID; prior accounts remain auditable. Customer
publication requests select operator-owned profiles and cannot override pricing,
allocations, endpoints or credentials. Offline refinement experiments reject
spend-controlled profiles.

PostgreSQL namespaces promote atomically to state schema 4 when spend state is
introduced. Local snapshots use envelope schema 2, retained across later writes.
Upgrade every host sharing that state before enabling the feature; older readers
reject these formats. Legacy states remain readable by the updated binaries.
Initialization uses a bounded transaction/advisory lock to prevent catalog races
between new hosts. The ledger grows with admitted model operations and preserves
unresolved allocations until explicit reconciliation.

The Python client provides `run_spend` and `operation_spend`. These routes return
only actor-owned run/operation data. Empty entries with `enabled: false` mean
spending was not configured, not that provider consumption was zero. Customer
tool charges use their tool contracts; these quotas cover model allocations.
