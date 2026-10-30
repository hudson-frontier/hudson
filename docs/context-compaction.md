# Explicit context compaction

An actor-owned run with a configured `context` policy can queue an explicit
compaction request through the existing message endpoint:

```json
{"kind":"compact","input":null,"request_key":"compact-1"}
```

POST this body to `/runs/{id}/messages`, or use
`client.compact_context(run_id, request_key="compact-1")` in Python. The response
is a durable queue receipt. The request executes at the next completed input,
tool or verification boundary; queuing it does not approve, answer, resume or
cancel work. The ordinary queue limits, actor ownership, explicit withdrawal
and idempotency rules apply. A repeated key returns its original receipt even
after consumption. Compaction arguments must be `null`.

Each control archives at most 128 eligible complete exchanges, preserving the
original task, latest direction, images, pending tool cycles and at least the
policy's configured recent exchanges. Original messages and provider metadata
remain in immutable, run-scoped artifacts with links to older history. The
context contains a bounded verbatim index and the registered
`read_context_artifact` tool can retrieve originals. The control makes no model
calls, invents no summary and changes no instructions, goal, tool permissions,
approvals, usage counters or execution limits.

Compaction can be consumed alongside one queued direction or follow-up. The
artifacts, checkpoint, both receipts and `context_compacted` event commit
together. A concurrent enqueue or withdrawal retries preparation against the
new queue revision. Cancellation discards pending controls through the same
queue path. Frozen experiment stages and agents without a configured archive
reader/context policy reject the request.

The event reports `archived_exchanges`, `artifact_id`, and serialized
`history_bytes_before`/`history_bytes_after` for the explicit archival pass,
before the boundary's new input is applied. A zero exchange count is valid when
only protected/recent context remains. Compaction cannot guarantee that a large
fixed instruction, image or new input will fit. Normal context limits and
automatic archival continue to apply.

The explicit pass scans eligible ranges once and moves each history message at
most once. Artifact serialization and hashing remain proportional to the
archived bytes; source evidence is retained rather than deleted. No latency or
memory improvement is claimed. The fixed regression workload verifies that 118
of 120 exchanges can be archived while the next action and exact source evidence
remain unchanged.

Native peers receive context replacement when archival changes the portable
history prefix. This preserves host authority while allowing native history to
be rebuilt from the retained context and archive index.

The preview API revision is `hudson-preview-2026-10-30-r5`. Exhaustive Rust/API
consumers must accept `MessageKind::Compact` / `compact` and
`EventType::ContextCompacted` / `context_compacted`. Use matching binaries for
namespaces containing these typed messages/events. Existing steering and
follow-up payloads retain their meaning; literal task text is never interpreted
as a compaction command.
