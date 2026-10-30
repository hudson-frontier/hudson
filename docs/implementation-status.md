# Implementation audit

The bundled Codex 0.165.0 peer now has installed-executable acceptance for native
rollout continuation/fork, actor separation, cached replay and exact one-request
admission. The actual worker completes its host-owned tool cycle inside Linux
isolation. Native built-ins remain unavailable; customer tools use the host's
authorization and approval path. The [conformance matrix](runtime-conformance.md)
distinguishes this evidence from other vendors, account workflows, active-turn
controls and live model acceptance.

The opt-in Linux `isolated_stdio` transport adds explicit read-only mounts, a
writable workspace, bounded private temporary storage and deny/host networking.
Its separate real-Bubblewrap CI tests cover enforcement and descendant cleanup.
It requires a protected operator installation; CPU, process-count, total-memory
and workspace-volume quotas remain deployment responsibilities. See
[runtime isolation](runtime-isolation.md).

The execution-runtime extension now has optional bridge text streaming, an
attempt-fenced provisional-output journal, SSE/WebSocket observation and Windows
Job Object cleanup. Protocol fixtures and native Windows descendant-cleanup
tests cover these paths; they do not establish vendor runtime parity. Native
CLI adapters/session files, broader modalities and compaction remain tracked in
issue #91. Persistent currency allocations are described in [spending](spending.md). See
[execution adapters](execution-adapters.md) for current limits.

Portable conversation export, fork and import use actor-scoped source artifacts
and fresh target admission. Transferred history supplies no approvals, prior
operation evidence or provider state. The implementation supports changing the
target's pinned agent/model configuration between tasks; active native vendor
session controls remain separate work. See [conversation transfer](conversations.md).

Explicit inline user images have bounded API/Rust/Python admission, native Chat
Completions/Anthropic encoding, checkpoint/reconnect retention and portable
format-2 export. Bridges require operator opt-in. This does not establish live
vision-model acceptance, provider file APIs or image-bearing tool-result support;
see [image input](images.md).

The local preview Python client and explicit HTTP contract baseline are documented
in [api-compatibility.md](api-compatibility.md). Protocol regressions cover transport
bounds, credential-safe errors, event paging and waits. The opt-in local
PostgreSQL/Temporal fixtures additionally exercise that caller across publication,
authentication, approval and restart. This does not declare a stable versioned API
or a registry-published SDK.

The managed-capability extension is tracked in [capability-delivery.md](capability-delivery.md).
Its requirement-by-requirement audit and local verification are complete. The
historical initial-milestone notes below are supplemented by that audit and
the [customer publication delivery](customer-publication.md).

Status: the requested initial agent harness is implemented and locally verified.
The user clarified provider scope to **OpenAI, Anthropic, and Gemini for now**.
This audit covers their text-and-tool agent interfaces with GPT as the default,
plus the generic loop, PostgreSQL state, tools, skills, goals, and subagents.
It does not claim every model variant, native multimodal workflows, or production
hosting. The broader platform roadmap in `goals.md` remains future work, consistent
with the request to focus on the agent rather than infrastructure.

The final verification used `python3 scripts/check.py --database
hudson_harness_test_20260921 --temporal`: all checks passed, with no paid provider requests.
Provider protocol tests establish adapter behavior; they do not establish live
availability or quality for every model. Earlier live GPT and Gemini checks are
recorded below; a live Anthropic request has not been verified.

This audit records local implementation and verification evidence. Git history
records delivery to main; package publication and deployment are separate steps.

## User-requested capabilities

| Requirement | Current evidence | Boundaries and later work |
| --- | --- | --- |
| Generic, domain-independent loop | `hudson-harness/src/agent_loop.rs`; checkpoint and real tool-cycle tests | Bounded context archival/retrieval is implemented in the extension; multimodal behavior remains outside scope |
| Rust implementation, maintainable module boundaries | Six Cargo crates; workspace tests and Clippy | Current architecture/development guides consolidated; later API cleanup remains possible |
| PostgreSQL | Reconnect and shared-budget database tests; multi-process approval smoke | SIGKILL recovery smoke and local operator receipt reconciliation now pass; automatic ownership remains deferred |
| GPT default; OpenAI, Anthropic, Gemini | Current protocol tests cover authentication, schemas, complete tool cycles, continuation metadata, output caps and usage; default model selection tests pass | Anthropic live requests are unverified; model-specific/native multimodal features are outside this version |
| Customer-defined tools | Rust registry, HTTP/MCP adapters, and authenticated versioned tool/agent registration; approval-aware runtime tests | One configured identity per API; connections are provisioned by the operator; no arbitrary uploaded code |
| Skills | Inline, Markdown and standard SKILL.md packages, on-demand resources, immutable contents tests | Package scripts are read-only resources and are never executed |
| Goals | Run objective, deterministic value criteria, recorded verifier-tool checks and operation evidence | Tool owners establish artifact-specific correctness; passing shape checks alone do not prove truth |
| Subagents | Configured trees, shared call budgets, persisted lineage, cancellation and join; child-provider routing and approval after restart pass | Durable dependencies and bounded repeated delegation are in the extension; the original worker/server remains synchronous |
| Temporal execution | Foreground/background process test, real-server concurrent team test, Postgres question replay after worker restart; SIGKILL during an approved write with no replay and receipt reconciliation | Local configuration and identity; durable submission intent and worker publication recovery are implemented; no hosted multi-tenant service |
| Low model spend | Live GPT and capped Gemini tool cycles; routine checks use local stubs; output and shared-call caps | Provider token reports are recorded; lost responses and currency costs remain unavailable |
| Usable product | README commands, authenticated publication CLI/API, JSON examples, config check, operator controls | Local worker/server installation passed; package-registry publication and hosted deployment remain separate |

## Documented first milestone

The numbered requirements below correspond to `goals.md`.

1. **Local start:** worker examples build and run; examples pass `--check`.
   Worker/server install into a temporary Cargo root with `--debug --locked
   --offline`; installed entrypoints and the installed worker config check pass.
2. **Non-Rust public API:** `scripts/smoke_api.py` combines Python HTTP requests
   with generic CLI text/file/stdin submissions to execute a configured agent and
   HTTP tool through the Axum server.
3. **Versioned agent, default loop, structured tool:** proven locally by registered
   tool tests, HTTP smoke, and the earlier live GPT tool cycle.
4. **Unauthorized operation denied visibly:** covered by runtime policy tests;
   hosted authentication is not established.
5. **Approval pause/resume:** proven across PostgreSQL-backed worker processes by
   `scripts/smoke_approval.py`, including rejection of a config that removes approval.
6. **Client disconnect:** configured HTTP execution is detached from its client
   connection. The API smoke closes connections, restarts the server during a
   persisted approval wait, then approves and completes the same run.
7. **Worker interruption:** `scripts/smoke_recovery.py` kills a worker after a
   destination write commits, then verifies restart does not replay it. Exact
   attempt fencing and a destination receipt resume the run with one total write.
8. **Cancellation and limits:** runtime, parent-child cancellation, concurrent
   shared-budget, and PostgreSQL budget-reconnect tests pass.
9. **Inspectable final record:** run/operation/events and success assessments are
   available. Model-call counts and provider-reported input/output tokens are
   available, with an explicit report count for coverage; currency costs are not.
10. **Version comparison regression:** `--evaluate` and `--compare-config` run
    the same suite through the ordinary runtime. The offline smoke demonstrates
    a completed-but-wrong baseline and a passing candidate, with distinct versioned
    runs, a shared case digest, and expected criteria kept outside model context.

## Earlier first-milestone verification

- Review fix: continuation controls now validate the saved model-budget binding
  before accepting requests. `smoke_child_approval.py` restarts with a different
  group, verifies resume/approval return 409 without decisions or writes, then
  restores the original group and verifies exactly one child write.

- The accepted language-neutral API direction now has `docs/openapi.json`, a
  served `/openapi.json` endpoint, and `docs/api.md`. Server tests compile its
  schemas and validate actual response bodies. Paths remain preview/unversioned;
  generated SDK compatibility and event streaming are not implemented.
- Completion audit: the user resolved provider scope to OpenAI, Anthropic, and
  Gemini. All three configured text/tool adapters pass current protocol tests.
  No unanswered provider-scope decision remains for this version.

- `python3 scripts/check.py --database hudson_harness_test_20260921` passed:
  workspace tests including all four opt-in database tests, formatting, Clippy,
  binary builds, all four example configurations, and all nine offline smoke
  scripts. This is local evidence; it is not hosted CI or registry publication.

- `examples/python-tool/` supplies a runnable customer-owned tool using only
  Python’s standard library. `scripts/smoke_python_tool.py` verifies its actual
  HTTP service, invalid numeric-input rejection, structured task input, the
  configured agent loop, two model calls, one tool call, and expected statistics.
  The model is a local protocol stub; this is not a live-provider quality test.

- Opt-in user clarification: default-loop checkpoint tests and
  `scripts/smoke_user_input.py` cover an advertised question, PostgreSQL/server
  restart, CLI reply, correlated provider continuation, one recorded input,
  duplicate reply without another call, and changed-reply rejection. The same
  smoke verifies worker text/JSON replies without provider construction, explicit
  resume, and no execution while recording replies. Replies bind to the saved
  question ID; delayed answers cannot satisfy a later question.

- Per-run shared-budget bindings reject changed/removed groups before execution
  and request-key reuse. Tests cover new batches with the same Agent version,
  restored bindings, boxed executors, and PostgreSQL reconnect.

- `scripts/smoke_child_approval.py`: child discovery, approval after PostgreSQL/server
  restart, distinct parent Chat Completions and child Anthropic protocols, child
  API/worker resume, one write and a shared four-call budget.

- `cargo test --locked --workspace --all-features`: passed; opt-in database tests
  are ignored by this command.
- PostgreSQL reconnect and shared-budget tests were separately executed and passed.
- `cargo clippy --locked --workspace --all-features --all-targets -- -D warnings`:
  passed after recovery/evaluation and partial-limit configuration changes.
- `scripts/smoke_subagents.py`: passed after rebuilding the worker; delegate,
  join, isolated child tools, and shared four-call budget.
- `scripts/smoke_approval.py`: passed after rebuilding the worker; durable wait,
  exact preview, policy-change rejection, approval, and one HTTP write on resume.

- `scripts/smoke_api.py`: configured HTTP execution, durable approval restart,
  exact argument preview, a single write, idempotent resume, and inspection and
  cancellation while a model call is blocked.

- `scripts/smoke_recovery.py`: SIGKILL after write commit, restart without replay,
  exact attempt fencing, receipt/schema validation, completion with one write;
  interrupted model response abandonment without retry or budget refund.
- `scripts/smoke_evaluation.py`: failing baseline, passing candidate, ordinary
  runtime verification and budgets, evaluator-only expected criteria.

- Local installation of worker/server into a temporary root passed; no global
  binaries or shell configuration were changed.

- `scripts/smoke_providers.py`: all four configured presets complete a tool cycle
  with asserted authentication, output caps, tool schemas, continuation metadata,
  correlated tool results, and verified final output. Missing/empty explicit
  credentials fail before IO. Uses local protocol stubs.

- Concurrent identical Agent/Tool publication passes in memory and across four
  PostgreSQL connections; changed versions remain immutable.
- Live Gemini 3.5 Flash-Lite completed with result 42, two model calls, one tool
  call, and a passed output contract. Older-model diagnostics returned 404 and
  executed no tools. The live script is opt-in and capped.

- Provider usage tests cover cache-inclusive Anthropic totals, optional/malformed
  reports, failed output accounting, consumption exactly once, PostgreSQL restart,
  and independent parent/child totals through the shared-budget wrapper.

- Structured task checks: configured `input_schema` rejects invalid tasks before
  model/tool IO and preserves request-key availability; the worker accepts JSON
  files/stdin, and delegation advertises/enforces child schemas with local references.

No hosted CI, merge, deployment, or production-scale result is claimed. Changes
remain in the local implementation worktree.
