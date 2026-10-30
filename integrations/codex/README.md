# Native CLI peer

This optional peer runs an operator-installed Codex CLI **0.165.0** app-server.
Hudson admits each provider request, executes customer tools and verifies goals.
The peer uses the real native rollout file for continuation and independent
forks. Install the pinned official CLI separately; its Apache-2.0 license and
notices remain with that package. This first-party adapter uses only Python's
standard library and the shared private checkpoint helpers.

Configure an absolute Python 3.12+ executable with
`args: ["-I", "/absolute/hudson/integrations/codex/peer.py"]`. Enable
`runtime.native_sessions: true`, and explicitly pass these environment
references through the transport:

- `HUDSON_CODEX_EXECUTABLE`: absolute path to the installed native executable.
- `HUDSON_CODEX_STATE_DIR`: absolute private Unix checkpoint directory.
- `OPENAI_API_KEY`: provider credential, retained by the peer and not its child.
- `HUDSON_CODEX_ENDPOINT`: optional full HTTPS or loopback Responses API endpoint.
- `PATH`: explicit executable search path if needed by the installation.

Use the [Linux isolated transport](../../docs/runtime-isolation.md) for explicit
runtime/package mounts, a writable checkpoint workspace and private temporary
CLI state. Hosted model access requires an explicit `host` network grant and
any certificate/DNS files required by the environment. Pure configuration
validation does not start the executable or establish readiness. The bundled
checkpoint peer requires Unix ownership guarantees; Windows hosts can use a
Unix peer through an authenticated bridge.

The child receives a fresh private profile and a synthetic local gateway token.
It cannot inherit an account login or the real provider key. The gateway permits
one upstream request per host attempt, disables HTTP and stream retries,
replaces built-in tool catalogs with the host's function catalog and pins the
output allocation. Function aliases cannot collide with native built-ins.
Every response item is checked before forwarding the bounded provider stream
to the CLI. Unadvertised functions, native server tools, unsupported content,
invalid usage and incomplete streams fail closed. Request, response, RPC queue,
frame, diagnostic, event and lifetime limits are explicit; redirects and encoded
provider responses are rejected.

Native dynamic-function calls pause without executing customer code. Before
acknowledging a checkpoint, the peer verifies that the native file contains the
exact admitted output. At a tool pause it stops the CLI without synthesizing a
cancellation result. The next admitted attempt restores an independent file
copy and injects only the host's recorded tool receipts and new directions.
Neither historical file contents nor forks grant tool authority, approval or
current-run verifier evidence.

Snapshots use the shared actor/workspace-scoped immutable store, serialized
capacity admission and pending markers. Ambiguous attempts cannot retry paid
requests; duplicate successful attempts return their saved receipt. Native files
are limited to 4 MiB and 8192 records, within the shared 8 MiB snapshot bound and
256-entry admission ceiling. Preserve files needed for continuation/forks;
interrupted pending markers require operator reconciliation. Temporary files
and processes also require the deployment's volume/cgroup controls.

Optional `stream: true` emits visible text through the bounded host protocol.
This version buffers and validates the provider response before CLI dispatch;
text is delivered after provider completion, ahead of host goal verification.
Reasoning and analysis are retained only as private continuation data and are
not display text. Context replacement rebuilds native history from the host's
portable messages. Native automatic model compaction is disabled; context
archival remains controlled by Hudson. Model changes use a newly published
target agent and explicit conversation transfer.

Inspect executable version and credential presence without provider calls:

```sh
HUDSON_CODEX_EXECUTABLE=/absolute/codex python -I integrations/codex/peer.py --probe
```

Run installed-executable acceptance with synthetic provider events:

```sh
python -I -m unittest discover -s integrations/codex -v
python -I scripts/smoke_native_codex.py --codex /absolute/codex --python /absolute/python --worker /absolute/hudson-worker --bubblewrap /usr/bin/bwrap
```

The separate CI job checks native file continuation/fork, actor separation,
cached replay, one provider request per attempt and the actual isolated worker
with one host-owned HTTP tool. No paid requests are made. Account login flows,
arbitrary external native-file imports, active-turn vendor steering and other
CLI/ACP families require further integrations. Native built-in command execution
is not enabled by this peer; use explicitly registered host tools.
