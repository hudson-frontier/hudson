# Opt-in read-only local project tools

This customer-owned Python service connects project listing and reading to Hudson's
existing authenticated HTTP tools. It uses the same `token_env` and `effect: read`
configuration as other customer tools. It does not change Hudson's filesystem
permissions. The project directory must be supplied explicitly; starting Hudson or
its interactive terminal does not automatically expose the current directory.

Requires Python 3.10+ on POSIX (Linux/macOS) with `dir_fd`, `O_NOFOLLOW` and
`O_DIRECTORY`. No Python packages are required.

Choose a trusted project directory containing only data you intend the configured
agent/provider to read. Supply `HUDSON_PROJECT_TOKEN` through your normal secure
environment setup with at least 32 random printable ASCII characters; share this
same value with the service and Hudson worker. Do not paste it into an agent
configuration or command arguments. Then start the service:

```sh
python3 examples/project-files/server.py --root /absolute/path/to/authorized-project
```

In a second terminal with the same token environment and your chosen provider
credentials, run the ordinary configured agent:

```sh
cargo run --locked -p hudson-worker -- \
  --config examples/project-files/agent.json \
  --input-file examples/project-files/task.json
```

The agent input is a plain prompt string, matching the interactive terminal
contract; `task.json` supplies that same string to the worker. In an interactive
terminal session, select this configured agent and ask your project question.
The service still needs explicit operator startup and authority as above.

The worker command invokes your configured model provider and may incur provider
charges; the verification command below uses only fake local fixtures and no model.
The default tool port is 9002; if changing `--port`, change both HTTP endpoints in
`agent.json`. Provider settings use Hudson's usual configuration. The sample agent
limits model calls to eight; this is a budget cap, not a correctness guarantee.

## Authority and limits

The operator explicitly grants reads beneath one root. Directory descriptors and
no-follow opens reject symlink traversal, including symlinked root ancestors.
Paths must be relative, without empty, `.` or `..` segments (`.` alone lists the
root). Names such as `.git`, `.env*`, `.ssh`, `.aws`, `.config`, `.codex`, `.netrc`,
credential/secret files and key extensions are refused and omitted from listings.
Hard-linked files and nonregular files are refused. Reads require UTF-8 and are
limited to 64 KiB; listings return up to 200 sorted entries and a `truncated` flag.
Directories exceeding 1000 scanned entries are refused; narrow the requested path.
There is no recursive listing, shell command, write or delete operation.

Name exclusions cannot detect credentials inside arbitrarily named source files.
A malicious process with permission to change the project can change directory
contents during a request; selecting a trusted, stable root is the operator's
responsibility. Mounts under that root are part of its granted authority. This is a
trusted host process, **not sandbox isolation**. Do not use it to expose hostile
multi-tenant directories or sensitive home directories. Remote cloud agents need
the separately tested sandbox adapter and a sandbox-scoped capability contract.

The service binds only `127.0.0.1`, requires bearer authentication on every request,
does not log requests or print tokens/root paths, and performs no external network
calls. Local processes possessing the token can read the granted root. Tool paths
are `/list` and `/read`; both accept exactly `{"path":"relative/path"}`. Read
results contain `content` and `bytes`; listings contain `entries` and `truncated`.
Tool refusals return HTTP 200 with `{"error":"project request refused"}` so the
existing generic adapter records a read result rather than an uncertain transport
outcome. Authentication and routing errors use HTTP 401/404. Like other read tools,
results reflect the file at request time and need not be stable across invocations;
no write receipts or effect replay are introduced.

Run the boundary and actual authenticated loopback HTTP fixtures:

```sh
python3 -m unittest discover -s examples/project-files -v
```

The tests cover traversal/absolute paths, reserved names, symlinked directories and
root, outside hard links, FIFOs, binary/oversized reads, scan/output bounds, missing
or invalid token, authentication, successful reads/listing and generic refusal
responses. Test tokens and secret file contents are synthetic fixtures.
