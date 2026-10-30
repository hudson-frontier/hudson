# Portable conversation transfer

Export a completed actor-owned run with `GET /runs/{id}/conversation`. The
versioned transcript contains conversation messages and optional original
archived exchanges. It excludes provider continuation metadata, credentials,
approvals, checkpoints and operation receipts. Export requires the default
Hudson backend and never calls a model or executes a tool.

`POST /runs/{id}/fork` starts a fresh task from that saved conversation:

```json
{"input":"Explain the result using the new model","request_key":"explain-once"}
```

The source remains completed. The fork gets new operation identities, usage,
limits, verification and policy checks. Earlier tool results remain context and
cannot satisfy criteria that require recorded tool executions in the new task.
The retry key binds the source run, normalized transcript, target agent, new
input, goal and budget. Changing any of those under the same key conflicts.
Fork admission verifies source ownership and the transcript inside its store
transaction; it does not trust a caller's claim about a local source.

For explicit transfer between hosts, submit `POST /runs/import`:

```json
{"input":"Continue this analysis","request_key":"transfer-once","transcript":{"format_version":1,"messages":[{"role":"user","content":[{"type":"text","text":"Earlier task"}]}]}}
```

Import treats all supplied history as caller-provided context. It requires
complete tool-call/result exchanges, rejects provider state and unresolved
artifact references, and accepts text, JSON and portable tool exchanges. Each
transcript is capped at 4 MiB, 4096 combined messages, 128 archived exchanges,
1024 blocks per message and 128 pending tool calls. The target's context ceiling
also applies. The import HTTP body has a separate 5 MiB ceiling for the transcript
and new input, at most four concurrent readers, and a 30-second request deadline.
Its permit remains held by admitted database work after a client timeout.
Retain the exact retry key and body after an uncertain HTTP result.

In published mode, fork/import requires `agent_ref` selecting an existing
actor-owned publication. Choose a different immutable agent/model profile to
change the runtime, model or reasoning settings. Configured hosts use their
configured root agent and reject `agent_ref`. The target's pinned goal and
per-submission budget are restored normally. Temporal hosts atomically persist
the new scheduling intent; accepting a transfer makes no provider calls.

Export expands bounded, run-owned tool-output artifacts and retains archived
exchanges as quoted evidence in oldest-first order. Earlier archive formats did
not record their absolute position among retained user directions; import keeps
this distinction explicit instead of reconstructing an invented message order.
Original artifact IDs never grant access to another run's storage. Native vendor
session files and in-flight turns are not portable transcripts.

The Python caller provides `export_conversation`, `fork`, and
`import_conversation`. Rust applications use `Store::export_transcript` and
`Runtime::import_turn` with `ImportedTurn`; a local fork supplies `source_run`,
while an external import leaves it absent. No method replays the source's tools
or imports its verifier assessment.
