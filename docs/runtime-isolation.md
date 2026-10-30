# Isolated runtime processes

Linux operators can select `isolated_stdio` to run an installed runtime peer in
new mount, user, PID, IPC and UTS namespaces using an explicitly installed
Bubblewrap executable. Network access requires an explicit `host` grant;
`deny` creates a separate network namespace with no host connections. There is
no fallback to ordinary stdio if the executable, mounts, kernel namespaces or
required Bubblewrap options are unavailable.

```json
{
  "type": "isolated_stdio",
  "executable": "/usr/bin/python3.12",
  "args": ["-I", "/opt/hudson-peer/peer.py"],
  "cwd": "/srv/hudson/workspaces/analyst",
  "env": ["OPENAI_API_KEY", "HUDSON_SDK_STATE_DIR"],
  "isolation": {
    "bubblewrap": "/usr/bin/bwrap",
    "read_only": ["/usr", "/lib", "/lib64", "/opt/hudson-peer"],
    "network": "deny",
    "tmp_bytes": 33554432
  }
}
```

This is a `runtime.transport` value. Install dependencies under the exposed
read-only paths, select the appropriate interpreter and library paths for the
distribution, and create a canonical absolute working directory beforehand.
The example denies network access, so it cannot contact a hosted model. An SDK
using a hosted endpoint requires `host`, plus explicit certificate/DNS files as
needed by its environment. `host` grants all network destinations available to
the process; it is not an endpoint allowlist. Use a separate authenticated peer
or an operator-enforced network proxy for destination restrictions.

Only the working directory is mounted read-write. Explicit files/directories
are mounted read-only at their configured absolute paths; all other host paths
are absent. Mount grants must be disjoint, bounded to 32 entries, and cannot
replace `/proc`, `/dev`, `/sys`, `/tmp` or `/run`. The executable must be in a
read-only grant. A fresh `/proc`, minimal `/dev` and private `/tmp` are created;
the remaining root is read-only. `/tmp` is limited to 1–256 MiB, default 32 MiB.
No host home directory, credential directory, sockets or environment values
are exposed implicitly. Only configured environment references cross the
boundary, as with ordinary stdio.

Operators must own and protect the launcher, runtime installation, mount roots
and workspace ancestors from concurrent replacement. Review directory grants
for sensitive files, sockets, nested mounts and shared hard links: a writable
workspace exposes those underlying host files, including aliases created by
hard links. Use a dedicated workspace volume and service identity for tenant
separation. Credentials explicitly passed to a peer remain accessible to it.

Capabilities are dropped and nested user namespaces are disabled. The peer
runs without a terminal in a new session. Cancellation/timeout retains Hudson's
bounded pipe handling and process-tree cleanup; PID namespace lifetime also
contains descendants that create new process groups. Request/response caps,
deadlines, authorization, tool approvals and spend admission remain in force.
This mode adds filesystem/network restrictions for trusted operator peers. It
does not accept arbitrary uploaded programs or promise CPU, process-count,
total-memory or workspace disk quotas. Deploy those with service/container
cgroup and volume limits. Windows Job Objects provide process cleanup only;
`isolated_stdio` is rejected on non-Linux platforms.

Bubblewrap is an external system dependency, not redistributed by Hudson. Use
a maintained distribution package; its upstream license and notices remain
with that package. The implementation uses its documented command-line
[namespace and mount options](https://github.com/containers/bubblewrap/blob/main/bwrap.xml).

After building, run real enforcement tests, including a descendant that starts
a new process group:

```sh
HUDSON_TEST_BWRAP=/usr/bin/bwrap cargo test --locked -p hudson-core --test runtime_isolation -- --include-ignored
```

CI installs Bubblewrap and the full database-enabled check runs these tests with
`--include-ignored`. The ordinary workspace test run marks the two host-dependent
cases ignored; that is not enforcement acceptance. The default test launcher is
`/usr/bin/bwrap`. No performance improvement is claimed; isolation adds process and
namespace setup work per attempt.

The native SDK CI job also runs the actual worker/SDK tool cycle through this
transport, using explicit `host` networking to reach its synthetic loopback
provider and private native checkpoints in the writable workspace. Reproduce
with `scripts/smoke_native_sdk.py --python /absolute/sdk/python --worker
/absolute/hudson-worker --bubblewrap /usr/bin/bwrap`.

Adding `BridgeTransport::IsolatedStdio` expands the public Rust enum. Downstream
exhaustive matches must handle the new variant. Existing serialized `http` and
`stdio` configurations are unchanged; public discovery reports the new transport
as `isolated_stdio_bridge` without exposing paths or environment values.
