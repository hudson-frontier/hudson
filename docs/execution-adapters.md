# Execution adapters

The bundled [native CLI peer](../integrations/codex/README.md) adds installed
Codex app-server execution and native rollout continuation. See the
[conformance matrix](runtime-conformance.md) for installed-runtime evidence and
limits alongside the SDK and generic bridge.
The optional [Claude Agent SDK peer](../integrations/claude/README.md) adds an
installed Claude runtime with one guarded Anthropic Messages request per admitted
attempt, native tool deferral and actor-owned immutable transcript receipts.

The [ACP v1 frontend](../integrations/acp/README.md) exposes Hudson runs to ACP
clients with bounded sessions, streamed text, explicit optional approvals and
confirmed cancellation. Hudson remains the agent execution owner; this frontend
does not enable external ACP agents as model executors.

Hudson's common agent loop can use an operator-owned SDK bridge instead of a
built-in model transport. HTTP and contained stdio bridges exchange portable model
requests and tool proposals. The normal run, checkpoint, tool policy, approval,
skill, memory, goal, subagent, inbox and verification paths remain authoritative.
Bridges must return proposals without executing tools themselves.

The optional [native agent SDK peer](../integrations/agent_sdk/README.md) provides
a real SDK implementation with immutable checkpoints and host-owned tools.
Other SDK bridges must
translate the chosen SDK's request, response and usage formats. Native terminal
attachment and vendor session files still require separate integrations.
[Inline image input](images.md) requires `runtime.images: true`; the default is
false and rejects images before external IO. Optional text streaming is described below. `GET /harnesses`
reports the configured streaming capability explicitly.

## Configuration and discovery

The optional `runtime` field selects a bridge for an agent or specialist. Supply
an explicit `model`; omit `provider`, `endpoint`, `api_key_env`, `reasoning` and
`legacy_token_limit`. These provider settings cannot be mixed with a bridge.
`max_output_tokens` still reaches the adapter in every request.

```json
{
  "name": "researcher",
  "instructions": "Answer using the registered tools.",
  "model": "your-model-id",
  "runtime": {
    "id": "research-sdk",
    "version": 1,
    "transport": {
      "type": "http",
      "endpoint": "https://bridge.example.com/model",
      "bearer_env": "RESEARCH_BRIDGE_TOKEN"
    }
  }
}
```

Bridge identity, version, transport settings and limits are included in the
immutable agent transport binding. Change the agent version when changing them.
Credential environment names are pinned; their values can rotate when rebuilding
the executor. Values never enter definitions, checkpoints, discovery or errors.
Configuration checks and admission-only publication do not resolve credentials,
spawn programs or contact bridge endpoints. Offline refinement experiments reject
external bridges.

An installation-owned model profile can also contain `runtime`; customers select
that profile by reference. Customers cannot supply endpoints, executable paths,
environment references or bridge definitions in agent publication requests.

`GET /harnesses` returns the configured execution identities, versions, transports
and common capabilities, including specialists. It requires the same authentication
as every other host route. Fixture mode returns an empty array. The Python caller
exposes `client.harnesses()`. Discovery contains no endpoints, paths, credentials
or implied live vendor acceptance results. `steering` means durable delivery at
Hudson's next safe model boundary; it does not mean injecting text into a running
vendor request.

## Wire protocol

HTTP sends one POST with JSON. Stdio starts one process per model call, writes one
JSON request, closes stdin, and expects one JSON response followed by successful
process exit. Stdout is exclusively the response; diagnostics belong on stderr.
Neither transport automatically retries.

```json
{
  "protocol_version": 1,
  "runtime_id": "research-sdk",
  "runtime_version": 1,
  "max_output_tokens": 1024,
  "request": {
    "instructions": "Answer using the registered tools.",
    "model": "your-model-id",
    "messages": [],
    "tools": []
  }
}
```

`request` uses the public `hudson_harness::ModelRequest` format. Messages contain
`role` and typed `content` blocks. Tools contain `name`, `description` and
`input_schema`. A bridge must deliver the supplied instructions, model, history,
tool descriptors and token cap to its SDK without granting additional tools.
The complete portable history is supplied on each call; no process-local session
is required to resume. Provider continuation data can travel in a tool call's
`provider_metadata`; it is not passed as tool arguments.

```json
{
  "protocol_version": 1,
  "runtime_id": "research-sdk",
  "runtime_version": 1,
  "response": {
    "type": "tool_calls",
    "calls": [{"call_id": "lookup-1", "name": "lookup", "arguments": {"id": 42}}]
  },
  "usage": {"input_tokens": 120, "output_tokens": 24}
}
```

Alternatively, `response` is `{"type":"final","output":...}`. Hudson verifies
the final output before completing the run. Tool IDs must be unique and tool
names must have been advertised. Core applies the configured batch ceiling,
validates arguments and rechecks authorization and approval before execution.
Usage is optional; missing usage is unavailable, not zero consumption. Reports
are consumed once and committed to the exact operation attempt. Malformed or
wrong-identity responses cannot reuse an earlier call's report.

## Provisional text streaming

Set `runtime.stream: true` to request LF-delimited JSON frames on HTTP or stdio.
The request includes `stream: true`; HTTP also sends `Accept: application/x-ndjson`.
The bridge must send an identity frame before any text, then text deltas, then
exactly one response frame. Each record ends with LF; CRLF is accepted. Example:

```jsonl
{"type":"start","protocol_version":1,"runtime_id":"research-sdk","runtime_version":1}
{"type":"text_delta","text":"Looking up the answer…"}
{"type":"response","result":{"protocol_version":1,"runtime_id":"research-sdk","runtime_version":1,"response":{"type":"final","output":42}}}
```

The entire encoded stream, including its final response, shares
`max_response_bytes`; at most 8192 frames are accepted. Truncated, mismatched,
out-of-order or trailing frames never settle a successful response. The IO queue
holds at most 16 notices and applies backpressure. Cancellation is checked at
least every 250 milliseconds while the blocking host driver is observing IO.
No streaming setting changes the nonstreaming protocol or retries a failed call.

Text becomes `model_output_delta` journal events bound to the exact run,
operation and attempt. UTF-8 chunks are at most 4096 bytes; the first delta is
committed immediately, then deltas are batched by size or 250 milliseconds.
Per attempt, text is capped at the run's payload limit or 4 MiB, whichever is
smaller, and 4096 events. Cancelled, terminal and replaced attempts cannot append
output. These events are display evidence with `provisional: true`; they never
execute tools, satisfy verification or complete a run. Reasoning and opaque
continuation metadata are not text deltas.

`GET /runs/{id}/events/stream` serves the journal as SSE and
`GET /runs/{id}/events/ws` as WebSocket text messages. Both support an exclusive
`after` cursor or `Last-Event-ID`, check run ownership, revalidate bearer tokens,
and close after 300 seconds. There are at most 32 combined connections per host.
Read pages hold at most 32 events and 128 KiB of encoded events, except that a
single larger event is delivered alone. Database reads have a five-second wait
bound and a separate 32-reader ceiling that remains held until a blocked query
actually finishes. The server's TCP wrapper enforces a five-second idle write
deadline and a 300-second connection lifetime, including upgraded WebSockets and
SSE clients that stop polling their response bodies. Socket reads have a
60-second idle deadline. Keep-alive clients reconnect when the connection lease
ends; embedding the Router in another HTTP host requires equivalent IO bounds.
A disconnected
observer changes no run state; reconnect explicitly from the last processed
sequence. The Python caller exposes `stream_events()` with byte, duration and
event bounds; it never reconnects automatically.

## Limits and lifecycle

Requests and responses default to 1 MiB each; `max_request_bytes` and
`max_response_bytes` accept 1 byte through 4 MiB. Serialization stops at the
request ceiling. Output is bounded before JSON decoding, including HTTP responses
without a content length. Deadlines default to 120 seconds and accept 1–300 seconds.
Core context, payload, step, batch, model and shared-budget limits still apply.

The adapter retains one HTTP client and one asynchronous IO worker per configured
runtime, with at most two blocking resolver workers;
configured agent trees cap the number of runtimes at 32. The synchronous host
driver waits outside store transactions and polls cancellation at most four
times per second. Network and pipe IO use the private asynchronous worker.
There are no detached retry loops or unbounded output queues.

HTTP uses TLS except explicit loopback HTTP, disables redirects and bypasses
environment proxies for loopback. Remote transport failure, cancellation or an
unsuccessful status preserves uncertainty; it is not proof the remote model
consumed no tokens. Cancelling HTTP observation closes the local request; the
remote service may continue computing. No remote cancellation receipt is claimed.

Stdio is available on Unix and Windows. It requires absolute `executable` and `cwd` paths,
at most 64 arguments with a total 32 KiB ceiling, and at most 32 unique `env`
references. The environment is empty except explicitly named values captured at
construction. Each value is capped at 32 KiB. Interpreter scripts need an explicit
interpreter executable and absolute script argument; no shell is inserted.
Stderr is drained concurrently with stdin/stdout, capped at 64 KiB and never
included in public errors. Deadline, interruption, pipe-limit failure and normal
exit clean up the process group on Unix. Windows starts the process suspended,
assigns it to a kill-on-close Job Object, then resumes it without a visible
console. Job assignment failure rejects startup; cancellation and deadlines
terminate descendants as well as the direct process.

An adapter is trusted operator code, like a registered Rust tool. Process groups
provide lifecycle cleanup, not filesystem/network isolation against hostile
programs. SDK adapters must not launch independent tool execution, retain mutable
sessions outside Hudson's checkpoint, detach work, or treat a lost response as
permission to retry. Unresolved attempts after host death use the existing
[recovery controls](recovery.md); restarting never guesses a completed response.

## Offline example and verification

Start the synthetic bridge, then run the supplied configuration in another shell:

```sh
python3 examples/runtime-bridge/bridge.py --port 4321
cargo run --locked -p hudson-worker -- --config examples/runtime-bridge.json --task 'Check execution'
```

This returns a fixture result and makes no provider calls. The same fixture runs
over stdio without `--port`; configure `/usr/bin/python3` as `executable`, the
absolute path to `bridge.py` as its argument, and an absolute working directory.

`scripts/smoke_runtime_bridge.py` exercises both transports through the real worker.
`crates/hudson-core/tests/runtime_bridge.rs` covers the tool cycle, approval and
adapter reconstruction, usage, immutable bindings, output/request bounds,
redacted errors, deadlines and process-group interruption. Its dedicated database
case is included by the full PostgreSQL runner. These fixture checks establish
the bridge contract; each SDK integration still needs its own acceptance evidence.

For a reproducible latency comparison with the existing direct HTTP model adapter:

```sh
cargo build --locked -p hudson-core --example bridge_benchmark
python3 scripts/bench_runtime_bridge.py
```

The three text modes run 100 sequential loopback requests with 1 KiB of text, a 32-token cap,
the same constant answer, and persistent HTTP connections. The output reports p50,
p95 and total microseconds separately for the existing transport and bridge.
The streaming mode adds an identity frame, one two-byte text delta and a final
response. This measures local adapter overhead, not provider quality or production
throughput; it excludes durable journal writes. The additional image mode runs
25 calls with 2 MiB minus two synthetic decoded bytes and reports process peak
RSS on Linux. See [image workload measurements](images.md#transport-measurement).

See [dependency licensing](dependency-licensing.md) for the process and WebSocket
components used by these integrations.
## Explicit native reasoning

Set `runtime.reasoning_effort` to an explicit effort such as `"high"` for a peer
that implements the setting. The pinned setting travels with every bridge
request and the peer must return the same `reasoning_effort` receipt. Missing,
different or unsolicited receipts fail before tool proposals are admitted;
valid reported usage remains latched. The output-token cap stays authoritative.
Omitted settings preserve existing JSON and transport fingerprints. Native
discovery reports the optional `reasoning_effort` flag when explicitly enabled.

Both bundled peers forward the setting to their provider request and acknowledge
it after checking the applied configuration. Model-specific acceptance remains
the operator's responsibility. Change immutable agent/runtime versions when
changing execution settings; use conversation transfer for an explicit model
change. See the [SDK guide](../integrations/agent_sdk/README.md) and
[CLI guide](../integrations/codex/README.md).

## Linux isolation

Use the opt-in [`isolated_stdio` transport](runtime-isolation.md) for explicit
read-only runtime mounts, a writable workspace and a required network policy.
It requires real Linux namespaces and Bubblewrap and fails closed when setup
is unavailable. Ordinary `stdio` remains trusted operator code with process
cleanup; use isolated transport when filesystem/network restrictions are needed.
