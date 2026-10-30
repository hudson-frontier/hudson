# ACP agent for Hudson

`peer.py` exposes a configured Hudson HTTP host as an **ACP v1 agent** over
newline-delimited JSON-RPC stdio. It uses Python 3.12+ and the bundled standard
library HTTP client. An ACP client starts this peer; the peer submits ordinary
Hudson runs. Model admission, spend, tool authorization, approvals, verification,
checkpoints and cancellation stay inside Hudson. It does not run an external
ACP vendor as a `ModelExecutor`.

Start a configured Hudson server, then configure the ACP client's agent command
with an absolute interpreter and script path:

```sh
python -I /absolute/hudson/integrations/acp/peer.py \
  --url http://127.0.0.1:4318 --cwd /absolute/project \
  --token-env HUDSON_API_TOKEN
```

`--token-env` names an already provisioned, scoped **Hudson API token**. It is
optional for a local single-user host. This peer does not perform provider login
or copy provider credentials. For a published host, additionally select the
operator-owned `--agent NAME --agent-version VERSION`. Protocol messages cannot
change the endpoint, bearer identity or agent reference.

## Supported behavior

- `initialize`, `session/new`, `session/prompt`, `session/cancel` and streamed
  `session/update`, following the [ACP v1 initialization](https://agentclientprotocol.com/protocol/v1/initialization),
  [stdio transport](https://agentclientprotocol.com/protocol/v1/transports) and
  [prompt lifecycle](https://agentclientprotocol.com/protocol/v1/prompt-turn).
- Text prompts, provisional journal text with operation/attempt metadata, tool
  progress, and verified final output. Matching streamed final text is not
  appended twice. Tool results themselves remain in Hudson; the public operation
  projection does not expose raw result bodies.
- Further prompts fork the last completed run through Hudson's conversation API.
  Each turn has a new run, operations, per-run limits and usage; configured shared
  budgets and spend controls remain authoritative. This is conversation context
  transfer, not replay of old tool authority or verification evidence.
- A waiting user question is shown as assistant text. The next prompt answers
  that exact saved question ID on the same run. Approval and reconciliation waits
  cannot be answered with task text. Literal `/compact` and control-shaped JSON
  remain ordinary prompt content.
- Cancellation during submission, execution or a permission prompt. `cancelled`
  is returned only after Hudson records that state. Unresolved cancellation
  returns a JSON-RPC error with the run ID. Disconnect requests cancellation of a
  known active run; losing HTTP/stdout does not establish a cancellation receipt.

Session IDs belong only to their stdio connection. The server's scoped bearer
identity still authorizes each request. Sessions cannot be loaded by supplying a
run UUID. `loadSession` is false; no session files are written. Completed runs and
unresolved work remain inspectable through the normal Hudson API independently
of this ephemeral peer. The result's `_meta["org.hudson/run"]` carries the run ID,
status, journal cursor and exact Hudson reported usage totals.

ACP's standard `usage_update` describes context occupancy and optional cumulative
cost. It is not a per-request input/output token report. This peer preserves
Hudson's reported token totals in namespaced metadata instead of inventing a
context size, cost or zero usage when reports are missing.

## Approval forwarding

By default this peer leaves approval waits in Hudson and reports the operation
ID for an authorized operator to review. It never grants an approval implicitly.
To present decisions inside the ACP client, the operator can enable
`--allow-approvals` and supply a Hudson token with the required approver authority.
The server continues to enforce that authority; the flag does not grant it.

For each wait the peer sends standard
[`session/request_permission`](https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission)
with the exact operation UUID, tool arguments and request digest. Only its fresh
request ID can consume one offered `allow_once` or `reject_once` option. It
rechecks the run wait and complete public operation projection before forwarding
the decision to Hudson. Unknown, malformed, stale, cancelled and repeated replies
cannot approve. No remembered `allow_always` option is offered. Cancellation
already received by the dispatcher prevents a later approval submission.
Model-allocation approvals are labeled as such, rather than as customer tools.

## Bounds and limitations

One prompt is active per connection, with at most 32 sessions and 256 prompts.
Each turn has a 300-second observation/permission deadline, 4096 journal events,
4096 HTTP calls, 256 tool updates and a 4 MiB output ceiling. HTTP calls time out
after three seconds and never retry. Cancellation confirmation has a further
five-second observation allowance; unresolved work is reported truthfully.
The input queue holds eight frames; output holds sixteen. Input frames are at
most 64 KiB, output frames at most 1 MiB. A connection accepts at most 8192 input
frames/32 MiB and emits at most 128 MiB. Output backpressure has a 500 ms queue
deadline. Blocked pipe threads cannot prevent bounded process shutdown.

Keep the exact request key after an ambiguous submission. Its JSON-RPC error
includes `requestKey`; this connection rejects further prompts on that session
instead of submitting a new paid task. No mutation, approval or model request is
automatically replayed. The existing API supports deliberate idempotent recovery.

Only the configured `--cwd` is accepted. It is display context, not a filesystem
grant or a way to change the server's directory. Client-supplied MCP servers and
additional directories are rejected. Host tools are configured on the server.
Images, audio, embedded resources, arbitrary native sessions, session loading,
mode/model changes, active-turn steering, compaction controls and client-side
filesystem/terminal execution are not implemented or advertised. ACP v2 is a
separate draft and is not supported by this peer.

## Offline validation

```sh
python -I -m unittest discover -s integrations/acp -v
cargo build --locked -p hudson-server
python -I scripts/smoke_acp.py
```

The smoke uses the real stdio peer and real Hudson server with local synthetic
model/tool endpoints: streamed tool cycle, explicit exact approval, one host
write, reported/admitted usage, conversation continuation, literal task content,
confirmed cancellation at an approval wait, and truthful unresolved cancellation
during remote model IO. It uses no provider credentials or paid calls.
The protocol tests also cover actor/session boundaries, unauthorized approval,
changed arguments, stale replies, unknown decisions, resource bounds, and
ambiguous admission. These fixtures do not establish editor UI compatibility or
external vendor execution. See the [conformance matrix](../../docs/runtime-conformance.md).
