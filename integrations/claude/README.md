# Claude Agent SDK peer

This optional first-party peer uses the real **Claude Agent SDK 0.2.175** and its
bundled **Claude Code 2.1.314** executable. Hudson admits every model request,
executes customer tools, applies approval and spend policy, and verifies output.
The peer does not run a whole unconstrained Claude agent turn as one model call.
Install the pinned SDK separately, retaining its license, bundled CLI terms and
notices. No vendor implementation or binary is redistributed in this repository.

```sh
python3 -m venv /absolute/claude-sdk
/absolute/claude-sdk/bin/pip install -r integrations/claude/requirements.txt
```

Configure an absolute Python 3.12+ interpreter with
`args: ["-I", "/absolute/hudson/integrations/claude/peer.py"]`,
`runtime.native_sessions: true`, and these explicit environment references:

- `HUDSON_CLAUDE_PYTHON`: absolute interpreter with the pinned SDK installed.
- `HUDSON_CLAUDE_STATE_DIR`: absolute private Unix checkpoint directory.
- `ANTHROPIC_API_KEY`: provider credential, retained only by the outer peer.
- `HUDSON_CLAUDE_ENDPOINT`: optional full HTTPS or loopback Messages endpoint;
  defaults to `https://api.anthropic.com/v1/messages`.
- `PATH`: explicit executable search path needed by the installation.

Use the [Linux isolated transport](../../docs/runtime-isolation.md) with the SDK
environment, Hudson integrations and required runtime directories mounted read
only, plus a writable checkpoint workspace. Hosted provider access needs an
explicit network grant and the deployment's certificate/DNS files. The bundled
private checkpoint peer requires Unix ownership semantics. Windows can use an
authenticated Unix bridge; Windows protocol tests do not establish native
Windows checkpoint support.

## Provider admission and tool authority

Each attempt starts a fresh SDK child with an empty private profile, no inherited
provider/account/cloud credentials, and a short-lived local gateway token. The
SDK uses its pinned bundled executable, documented restricted mode, no built-in
tools, an explicit in-process MCP catalog, `strict_mcp_config`, empty filesystem
setting sources, and disabled slash commands. The gateway replaces the native
request's system prompt, full history, model, tools and output allocation with
Hudson's exact admitted request. Vendor defaults cannot silently change them.

The gateway admits at most **one** actual upstream HTTP request, consuming that
allowance before parsing or IO. It does not redirect or retry. A native SDK retry
can only hit the already consumed local gateway; it cannot create another paid
request. Requests pin `max_tokens`, the stable Messages API version, and disabled
parallel tool use. Only one customer tool proposal is supported per model step.
Native server tools, unadvertised names, unsupported blocks, incomplete streams,
duplicate IDs, invalid usage and output-allocation violations fail closed.

The complete bounded Anthropic SSE response is typed and validated before the
native runtime sees any tool. The gateway emits canonical events containing only
reviewed fields. The SDK's `PreToolUse` hook then **defers** the exact MCP call;
the MCP handler cannot execute customer code. Before returning a proposal, the
peer checks the real SDK hook/result, native version/catalog, exact assistant
response and exact mirrored native transcript. Any native tool invocation or
synthetic tool-result receipt rejects the attempt.

`--bare` is deliberately not used: installed-runtime testing showed that it
disabled the SDK deferral hook and synthesized a denial receipt. A denial or
interrupt is not reported to Hudson as a successful native tool pause.

## Continuation, accounting and recovery

Successful attempts store the real SDK transcript mirror and exact response in
the shared actor/workspace-scoped immutable checkpoint store. Duplicate
successful attempts return their saved receipt without launching the SDK or
requesting the provider. Pending markers fence ambiguous attempts from paid
replay; operator reconciliation is required after interruption or loss.
Capacity admission, ownership, symlink checks, atomic publication and bounds use
the same private helpers as the other native peers.

Continuation and independent forks rebuild the provider request from Hudson's
complete pinned conversation. Signed thinking, interleaved assistant text and
tool-use content remain in private provider metadata and are restored exactly
once alongside Hudson's recorded tool receipts. This peer does **not** resume a
vendor pending-tool session: a vendor resume can execute its deferred tool before
host callbacks run. Native snapshots are per-attempt execution receipts, not
importable authority or a claim of native file resume/fork support.

Reported input tokens include ordinary input, cache reads and cache creation.
Output tokens use the provider's final cumulative report. SDK session cost
estimates remain private vendor state and are not substituted for Hudson spend
receipts or reported as billed cost. Optional host `stream: true` emits only
visible text after native response/snapshot validation; thinking and signatures
are never display deltas. Buffering adds latency to first visible text in exchange
for validating the complete response before native dispatch.

Requests/provider responses are capped at 4 MiB, provider SSE at 4096 events and
1024 blocks, and one proposed tool per attempt. The native SDK input buffer is
1 MiB, session mirrors hold at most 8192 records/8 MiB, and host responses share a
1 MiB total. Private storage admits at most 256 snapshots/pending markers per
actor. The bounded RPC client limits diagnostic bytes, output bytes, frame count
and queue depth. The child/gateway deadline is 120 seconds inside the host's
configured transport deadline. Host cancellation cleans up descendants through
the normal contained process lifetime controls; a lost remote response can leave
usage or cancellation uncertain. Process cleanup alone is not hostile-code
filesystem/network isolation.

This version supports text/JSON input, client functions and signed-thinking
continuation. Images, redacted-thinking acceptance through the installed SDK,
explicit reasoning settings, provider betas/server tools, subscription login,
Bedrock/Vertex, model fallback, arbitrary vendor session import/resume,
active-turn steering, background agents and vendor compaction are not enabled.
Unknown explicit reasoning settings are rejected rather than silently ignored.
Host-controlled context replacement rebuilds the pinned conversation normally.

## Offline installed-runtime acceptance

```sh
python -I -m unittest discover -s integrations/claude -v
python -I scripts/smoke_native_claude.py --python /absolute/claude-sdk/bin/python \
  --worker /absolute/hudson-worker --bubblewrap /usr/bin/bwrap
```

The native smoke exercises the installed SDK/CLI, real transcript mirrors,
deferral without tool execution, replay, actor separation, signed thinking/text
continuation, independent host forks, actual provider request admission,
cache-inclusive usage and the real isolated worker with a host-owned HTTP tool.
Adversarial provider output cannot dispatch an unadvertised native effect or
authorize another provider request after an ambiguous attempt. All endpoints and
credentials are synthetic; no paid model/account acceptance is claimed. CI runs
these checks in a separate required job.

Primary contracts: [SDK reference](https://code.claude.com/docs/en/agent-sdk/python),
[tool deferral](https://code.claude.com/docs/en/hooks#defer-a-tool-call-for-later),
[external transcript storage](https://code.claude.com/docs/en/agent-sdk/session-storage),
[Messages streaming](https://platform.claude.com/docs/en/build-with-claude/streaming),
and [CLI restrictions](https://code.claude.com/docs/en/cli-reference).
