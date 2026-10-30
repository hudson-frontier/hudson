# Execution adapters

Hudson's common agent loop can use an operator-owned SDK bridge instead of a
built-in model transport. HTTP and Unix stdio bridges exchange portable model
requests and tool proposals. The normal run, checkpoint, tool policy, approval,
skill, memory, goal, subagent, inbox and verification paths remain authoritative.
Bridges must return proposals without executing tools themselves.

This is an integration protocol, not bundled vendor CLI support. A bridge must
translate the chosen SDK's request, response and usage formats. Native terminal
attachment, vendor session files, token streaming and image inputs are not
implemented by this protocol. `GET /harnesses` reports these limits explicitly.

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

## Limits and lifecycle

Requests and responses default to 1 MiB each; `max_request_bytes` and
`max_response_bytes` accept 1 byte through 4 MiB. Serialization stops at the
request ceiling. Output is bounded before JSON decoding, including HTTP responses
without a content length. Deadlines default to 120 seconds and accept 1–300 seconds.
Core context, payload, step, batch, model and shared-budget limits still apply.

The adapter retains one HTTP client and one IO worker per configured runtime;
configured agent trees cap the number of runtimes at 32. The synchronous host
driver waits outside store transactions and polls cancellation at most four
times per second. Network and pipe IO use the private asynchronous worker.
There are no detached retry loops or unbounded output queues.

HTTP uses TLS except explicit loopback HTTP, disables redirects and bypasses
environment proxies for loopback. Remote transport failure, cancellation or an
unsuccessful status preserves uncertainty; it is not proof the remote model
consumed no tokens. Cancelling HTTP observation closes the local request; the
remote service may continue computing. No remote cancellation receipt is claimed.

Stdio is available on Unix. It requires absolute `executable` and `cwd` paths,
at most 64 arguments with a total 32 KiB ceiling, and at most 32 unique `env`
references. The environment is empty except explicitly named values captured at
construction. Each value is capped at 32 KiB. Interpreter scripts need an explicit
interpreter executable and absolute script argument; no shell is inserted.
Stderr is drained concurrently with stdin/stdout, capped at 64 KiB and never
included in public errors. Deadline, interruption, pipe-limit failure and normal
exit clean up the process group. Windows fails closed until equivalent process
containment is implemented; HTTP bridges work on Windows.

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

Both modes run 100 sequential loopback requests with 1 KiB of text, a 32-token cap,
the same constant answer, and persistent HTTP connections. The output reports p50,
p95 and total microseconds separately for the existing transport and bridge.
This measures local adapter overhead, not provider quality or production throughput.
