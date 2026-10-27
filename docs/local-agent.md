# Local terminal agent

`hudson` starts an interactive TUI and an in-process local host in the current
directory. This is the first native Rust local-agent slice. The core remains a
general tool-using runtime; code mode supplies the initial instructions and tools.
It does not embed or depend on Pi. Cloud-service integrations are outside this
slice.

## Start

Build/install using the README, set `ANTHROPIC_API_KEY` in your shell environment,
and run `hudson` in your project. The local host currently uses Anthropic Messages
with `claude-sonnet-4-6`; select an explicit model with `--model MODEL`.
`--endpoint URL` accepts HTTPS or loopback HTTP for compatible test services.
Keys stay in the host process and are not serialized into session state or
forwarded to shell commands. Model requests contain the task, tool schemas,
loaded project instructions and files/results the agent selects through tools.

```sh
hudson --workspace /path/to/project
hudson --permission read-only
hudson --permission full-access
hudson --workspace /path/to/project --resume SESSION_UUID
```

`hudson-cli` keeps its existing HTTP-client behavior. Explicit HTTP commands such
as `hudson health` and `hudson --url URL session` also remain available. Local
options cannot be combined with HTTP commands. A terminal is required for the
local TUI.

## Interaction

Enter a task to start execution. Subsequent tasks inherit the latest completed
turn's full portable history, including tool results. Each task has its own
immutable run, budgets, verification and operation records. Failed/cancelled
turns do not contribute history to subsequent turns; inspect their actual effects
before continuing. There is no implicit rollback of commands or edits.

| Input | Behavior |
| --- | --- |
| `/steer TEXT` | Queue direction at the next complete model/tool boundary |
| `/approve`, `/deny` | Decide the displayed pending write operation |
| `/cancel`, Ctrl-C | Request cancellation; stop a running shell process group |
| `/resume` | Continue a saved safe pending runtime boundary |
| `/help` | Show controls |
| `/quit` | Cancel active work, close the host and print the session resume command |
| PageUp / PageDown | Scroll recent conversation output |

The editor supports ordinary text, bracketed paste and backspace. Responses
stream while the host works on a separate thread. Final results are authoritative;
partial streamed text never authorizes execution. Tool activity, bounded command
output and before/after edit previews appear in the conversation. Previews show
at most 60 lines per side; use Git diff to inspect the full change.

## Workspace and execution policy

The canonical launch directory is the workspace root. `AGENTS.md` at that root
is loaded as project instructions; the agent is instructed to read applicable
nested instruction files before editing. Session instructions are pinned when
created; start a new session to pick up changes.

Built-in tools are `list_directory`, `read_file`, `search`, `write_file`,
`edit_file`, and `shell`. Search requires `rg` on PATH and honors its ordinary
ignore rules. File tools reject paths resolving outside the workspace. Replacing
a file requires the digest from a previous read, or an exact unique edit match;
new-file creation cannot overwrite a concurrently created file. There remains
a final check/rename race with unrelated external editors; filesystem path checks
are not an OS sandbox.

The default `ask` policy admits reads and requires approval for every file write
and shell command. `read-only` publishes only read/search tools. `full-access`
admits writes and shell commands without individual approvals. A resumed session
must retain its workspace, model, endpoint and permission policy.

Shell execution uses `/bin/sh` on Unix, starts in the workspace, and runs with the
user's OS permissions. An approved command can access paths outside the workspace
and the network. The environment is limited to PATH, HOME, TMPDIR, LANG and LC_ALL;
provider keys and other inherited environment credentials are not passed through.
Commands needing other environment settings must set them explicitly. Normal
child process groups are stopped on timeout, cancellation and operation exit;
this does not contain processes that deliberately detach. Managed background
jobs, PTYs for child commands and Windows shell execution are not implemented.

## Persistence, interruption and bounds

State lives under `~/.local/state/hudson/SESSION_UUID/`, or `HUDSON_STATE_DIR`.
Directories are private and files use mode 0600 on Unix. Session metadata pins the
workspace and configuration. A file-backed core store holds runs, checkpoints,
operations and results. It locks the session for the lifetime of one host and
syncs atomic snapshot commits, including operation intent before dispatch. A
second host cannot concurrently open that session. This local snapshot layout
targets small local sessions, not high-throughput hosted execution.

After restart, completed conversations can accept follow-ups. Safe pending work
requires explicit `/resume`. A running or unknown operation left by an interrupted
host is never automatically replayed. The TUI reports that it needs inspection;
automatic reconciliation is not implemented. Start a new session after checking
the actual effects. Cancellation/timeouts can leave partial filesystem or command
effects and do not roll them back. Provider cancellation is checked between stream
records; blocked network IO may wait for the transport's 120-second timeout.

Current limits: 128 tasks per session, 64 model calls and 256 operations per task,
1 MiB model-request context, 64 MiB state snapshot, 32 KiB user input, 64 KiB text
files, 24 KiB combined command output, and 120 seconds per shell command. A model
stream is capped at 2 MiB. Output truncation is explicit. The terminal retains a
bounded recent display; full recorded outcomes remain in local state. Session
context compaction is not enabled in this slice: exhausted context/state fails
explicitly and requires a new session.

## Validation

`python3 scripts/smoke_local_tui.py` launches the actual TUI in a PTY against a
local Anthropic SSE fixture. It reads/edits a real file, runs its Python check,
verifies durable completion, restarts the process and proves a follow-up receives
the prior conversation. Rust tests additionally cover write approval, scope and
stale-edit rejection, uncertain command effects, local-store locking/rollback,
terminal escaping, and refusal to execute incomplete streamed arguments. These
checks make no paid provider calls; they do not establish live-model task quality.
