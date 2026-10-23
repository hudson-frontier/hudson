# 0002: External sandbox execution through Hudson Sandbox

- Status: Accepted
- Date: 2026-09-19
- Scope: Sandbox repository ownership and Hudson's integration boundary

## Decision

Use [Hudsonbox](https://github.com/hudson-frontier/hudsonbox) as the separate execution backend for customer code hosted by Hudson.

External means a separate project in the same organization, accessed through an explicit service contract. It does not mean a required third-party hosted service. Hudson's core product remains in this monorepo, with an adapter connecting agent operations to the sandbox backend.

Hudson now implements the harness, durable run state, tool controls, and local HTTP
API. Hudson Sandbox has its own implementation and public HTTP/JSON contract;
its internal guest/supervisor gRPC is a separate transport boundary. Hudson's
`Execution::Sandbox` adapter still rejects execution as unsupported. Neither
repository's implementation alone establishes an integrated isolation guarantee.
The local adapter contract and acceptance work is tracked in
[Hudson issue #17](https://github.com/hudson-infinity/hudson/issues/17).

The current protocol foundation persists exact request bytes, stable mutation
keys, backend profile identity, admitted-attempt fences and original-operation
receipts. The transport crate uses the public v1 SDK and verifies the constructed
client's canonical origin/project declaration before owned receipt observation.
Receipt observation also passes the caller's byte limit to the SDK before JSON
decoding, so ignored fields cannot bypass it. These SDK interfaces are pinned to
independently merged Hudsonbox commit
`df4b53cb8e3e22bd808b0e61f25ac43f3eaeb976`. The
[adapter contract](../sandbox-adapter-contract.md) records the ownership and
recovery mappings. These protocol checks do not enable Runtime dispatch or
establish guest isolation.

Real local acceptance additionally needs a compatible Linux host with KVM,
usable delegated cgroup v2 controllers, a matching verified Firecracker/jailer
pair, trusted image artifacts and an explicit operator-approved command/resource
mapping. The sandbox repository's `scripts/doctor.py --profile linux-runtime`
checks prerequisites. Protocol fixtures or a successful KVM device query cannot
replace create/execute/cancel/destroy and physical cleanup evidence on that host.

## Responsibilities

| Hudson main repository | Hudson Sandbox repository |
| --- | --- |
| Agent definitions, default harness, and run state | Sandbox creation, inspection, and destruction |
| Business permissions, approvals, and credential authority | Validation of scoped execution authority and sandbox ownership |
| Agent budgets and admission decisions | Resource reservations, sandbox quotas, and enforced execution limits |
| Authorization of privileged tool operations | Command execution, bounded output, and workspace file exchange |
| Run history, success assessment, and evaluation orchestration | Sandbox operation receipts, resource usage, and lifecycle evidence |
| Run-level cancellation and recovery decisions | Process termination, reconciliation, and resource cleanup |

Sandbox completion is execution evidence. Hudson determines whether an agent run completed and whether it met its success criteria.

## Existing sandbox design

The sandbox repository's [implementation design](https://github.com/hudson-infinity/hudson-sandbox/blob/main/docs/implementation.md) selects:

- Rust for its API, workers, supervisor, guest agent, and shared protocol types.
- Temporal for durable sandbox coordination outside customer execution environments.
- Firecracker for Linux microVM isolation, with a trusted host supervisor and a guest agent.

Sandbox-specific implementation details and validation belong in that repository. Its use of Temporal does not settle the durable execution engine for Hudson's main agent runtime.

## Integration requirements

- Use versioned, authenticated contracts that carry workspace, run, sandbox, and operation identities. Caller-supplied identifiers do not establish authority.
- Authorize requests in Hudson before dispatch. The sandbox backend validates the scoped authority and enforces the admitted resource and network restrictions.
- Keep business credentials in trusted connector infrastructure. Guest code requests privileged tool actions through a gateway that rechecks current permissions and approvals.
- Preserve stable operation identities across retries. Inspect receipts and reconcile uncertain outcomes before repeating commands that may have caused external effects.
- Attach results, usage, artifacts, cancellation state, and cleanup evidence to the originating Hudson run.
- Keep persisted agent state separate from sandbox process and workspace state. Loss of a VM must not be presented as transparent recovery of arbitrary in-memory customer code.
- Reject execution that requires isolation when the backend is unavailable or cannot enforce the required restrictions; do not silently fall back to an unrestricted process.

Customer-operated HTTP tools and workers remain a separate integration option. They do not automatically run inside Hudson Sandbox, and their operators remain responsible for their execution environments.

## Self-hosting and development

Hudson's installation documentation must explain how to configure a compatible Hudson Sandbox deployment, including credentials, network boundaries, and host prerequisites. A useful self-hosted installation remains a product goal across both repositories.

The current sandbox design requires a Linux execution host with KVM. Development on macOS or other unsupported execution hosts should connect to a suitable Linux host for real isolation. Any development substitutes must state their limitations.

The public API contracts should remain consistent between self-hosted and managed deployments. Compatible versions, packaging, and end-to-end validation are still to be defined.

## Still to decide

- Exact API schemas, capability format, and authentication between services
- Compatibility and upgrade policy across the two repositories
- Mapping of agent deadlines, budgets, cancellation, and recovery to sandbox operations
- Event and artifact exchange, retention, and reconciliation behavior
- Self-hosted packaging and integration acceptance tests

Related documents: [product goals](../goals.md), [Rust decision](0001-rust.md), and [Hudson Sandbox implementation design](https://github.com/hudson-infinity/hudson-sandbox/blob/main/docs/implementation.md).
