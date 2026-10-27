# Local runtime library APIs

The standalone [Hudson CLI](https://github.com/hudson-frontier/hudson-cli) owns the
terminal agent and operator commands. This backend supplies reusable runtime,
storage, and model transport APIs; it does not install another operator CLI.

## Persist a single local host

`Store::local_file(path)` opens a versioned snapshot in an existing private
directory on Unix, including Linux under WSL. The directory must belong to the
effective user and exclude group/other access. Existing state and lock files
must be regular files with the same ownership, private permissions, and one link.
Symlinks, special files, public directories, and shared files are rejected.
Other platforms return `Error::Unsupported`; use WSL or PostgreSQL.

Hosts with separate session metadata should pin the private directory once and
call `Store::local_file_in(&directory_file, "state.json")` using that same file
handle. The helper validates the directory and accepts a single filename.
Locking, snapshot reads, temporary files, atomic replacement, cleanup, and syncing
all use that pinned directory. Renaming the directory or replacing its old path
cannot redirect later snapshot writes. Keep metadata IO on the same capability.

Keep this directory outside the agent's editable workspace. It contains trusted
definitions, operation requests, conversation history, and other private state.
The host owns directory creation and access control. These checks do not isolate
arbitrary programs running as the same OS user.

One host holds the snapshot's lifetime lock; cloned store handles share that
ownership. Transactions serialize and sync an atomic snapshot before returning.
Failed transactions preserve the prior in-memory state. A directory-sync failure
after replacement leaves durability uncertain and blocks further use of the
handle; reopen and inspect persisted runs before deciding how to proceed.
Snapshots are capped at 64 MiB while reading and serializing. The backend still
clones its transaction state and rewrites the snapshot, so this store suits a
single bounded local session rather than a shared database.

Missing state opens a new store. A host that separately tracks session metadata
must reject missing state when that metadata refers to existing runs. Restored
unknown/running operations retain the normal recovery fences; reopening never
authorizes replay of an uncertain effect.

## Continue a completed conversation

`Runtime::submit_turn(actor, agent_ref, input, request_key, previous_run_id)`
creates a new run using the previous completed local run's portable history.
The actor must own that run, and its agent revision must match. Scheduled runs,
unfinished runs, unsupported backends, and archived history without explicit
artifact transfer are rejected. Prior results and operation receipts stay intact;
their effects are not executed again.

Each turn has its own input, ordinary limits, usage, and retry receipt. Reuse the
same request key, input, and previous run when retrying that submission. Shared
model budgets retain their ordinary host-configured binding. This history-only
API does not inherit a previous task's goal; hosts needing per-task goals should
use the normal submission APIs with explicit goals.

`Store::conversation_history(actor, run_id)` exposes owned host-side history for
local rendering. It is not a public HTTP checkpoint endpoint or a permission to
send history to a different identity or provider.

## Observe Anthropic text safely

`AnthropicModel::with_streaming(observer, cancelled)` opts into bounded Anthropic
SSE assembly. The observer receives text deltas for display; signed thinking and
tool argument fragments remain inside the adapter. Only a complete validated
message becomes a model response. Interrupted, malformed, over-limit, or cancelled
streams never dispatch partial tool arguments and are not automatically retried.
The parser accepts cumulative message deltas as specified by the
[Anthropic streaming contract](https://platform.claude.com/docs/en/build-with-claude/streaming).

The stream caps total bytes at 2 MiB, a line at 128 KiB, and content blocks at 64.
Cancellation is checked between bounded reads; it cannot interrupt a blocked
network read before the existing request timeout. The final runtime result and
operation receipts remain authoritative. Hosts must bound observer queues and
escape terminal control characters before rendering untrusted text.
