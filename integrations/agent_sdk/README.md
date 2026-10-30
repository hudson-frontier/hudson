# Native agent SDK peer

This optional peer runs the real OpenAI Agents SDK while Hudson owns tool
authorization/execution, goals, limits, approvals, usage and recovery. Install the
pinned requirements into a dedicated Python 3.12+ environment. SDK packages retain
their own licenses; this integration is Apache-2.0.

Configure an operator-owned stdio runtime with an absolute Python executable,
`args: ["-I", "/absolute/path/to/integrations/agent_sdk/peer.py"]`, an absolute
working directory, and these explicit environment references:

- `OPENAI_API_KEY`: provider credential; never saved in native checkpoints.
- `HUDSON_SDK_STATE_DIR`: absolute, private Unix directory, owned by the peer user.
- `HUDSON_SDK_BASE_URL`: optional HTTPS or loopback SDK API base URL.

Set `runtime.native_sessions: true`. Enable `stream` and `images` when desired.
Use a new immutable agent version when changing runtime settings. Pure config
validation does not import the SDK, inspect credentials or start the peer.

The SDK receives only advertised function tools, all requiring its approval pause.
Its function bodies cannot execute customer tools. Tool proposals return to
Hudson, and the next admitted model call feeds recorded tool receipts back to the
SDK. A model guard and HTTP-transport guard permit one upstream request per host
attempt; SDK retries, tracing, handoffs and hidden tool execution are disabled.
HTTP redirects and compressed responses are rejected; response bytes and timeouts
are bounded.

The peer implements the SDK Session protocol and preserves its provider-native
conversation items in immutable snapshots keyed by the host operation/attempt.
An acknowledged receipt must match the exact admitted run, operation, attempt,
request fingerprint and message count. Duplicate calls return the saved receipt.
A pending marker prevents concurrent or ambiguous replay from making another
provider request. Process death does not authorize retry.

Snapshots use private Unix files, reject symlinks and are separated by actor and
workspace. Each actor directory holds at most 256 snapshot/pending records;
snapshots are capped at 8 MiB and 8192 native items. An actor-local file lock makes
capacity admission atomic across peer processes and is released before provider
IO. Immutable publication briefly retains its pending marker and temporary file.
Interrupted pending markers
require explicit operator reconciliation. Preserve snapshots needed for resume
or forks. Windows hosts can use a Unix peer over an authenticated bridge; the
bundled filesystem peer requires Unix ownership guarantees.

Host context changes select explicit replacement from portable history, while
append-only transitions continue the saved native session. Same-owner completed
conversation forks can reference their original immutable SDK checkpoint. Opaque
provider continuation fields remain tool metadata, never tool arguments; portable
exports remove them. Reasoning events are not emitted as display text.

Inspect installation status without provider calls:

```sh
/absolute/python -I integrations/agent_sdk/peer.py --probe
```

The result includes package version, availability and credential presence, with
no credential value. This is package readiness, not a live model acceptance test.

Run the actual SDK/worker conformance fixture after building the worker:

```sh
python -I -m unittest discover -s integrations/agent_sdk -v
python -I scripts/smoke_native_sdk.py --python /absolute/sdk/python --worker /absolute/hudson-worker
```

The fixture uses synthetic Responses API events and a local HTTP tool. It checks
SDK pauses, native continuation, immutable/cached replay, streaming, host-owned
tool execution and exact usage. No paid provider calls occur. The separate
`Native SDK conformance` CI job installs the real pinned SDK and runs this fixture.
Vendor CLI session files, terminals and account login workflows require their own
adapters; this peer provides SDK-native conversation checkpoints.
