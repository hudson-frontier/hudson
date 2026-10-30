# Native runtime readiness and account controls

The installation-owned account catalog provides explicit lifecycle control over
an installed runtime. It is separate from `/harnesses`, which describes immutable
execution capabilities. Listing either catalog performs no process, login, token
refresh or model IO. Configuration validation and publication remain offline.

Add `accounts` to the authenticated host catalog. Each entry identifies a control
peer, its runtime version and its contained stdio transport:

```json
{
  "id": "native-account",
  "runtime_id": "codex",
  "runtime_version": 1,
  "lifetime_seconds": 300,
  "actions": ["status", "models", "login", "renew", "cancel_login", "logout"],
  "transport": {
    "type": "stdio",
    "executable": "/absolute/python",
    "args": ["-I", "/absolute/hudson/integrations/codex/accounts.py"],
    "cwd": "/absolute/hudson",
    "env": ["HUDSON_CODEX_EXECUTABLE", "HUDSON_CODEX_ACCOUNT_DIR", "PATH"]
  }
}
```

The directory is a dedicated private Unix directory, owned by the service user,
with mode 0700. The native peer fails closed on other platforms. Windows hosts
can run another operator-installed control peer in a contained Windows job.
Environment names, command paths and settings come only from the operator.
The account control entry is immutable under its ID/runtime-version pair; bump
the version to change it. `actions` defaults to read-only `status`; every other
action requires an explicit installation grant. At most 32 entries are accepted. Account peers use
ordinary contained `stdio`; they are trusted installation code with service-user
filesystem/network authority. Model runtime isolation remains a separate binding.

`GET /accounts` returns IDs, runtime versions and session lifetimes. Open a live
control session with `POST /accounts/native-account/sessions` and an explicit
`request_key`. The response has its UUID and absolute expiry. Then POST to
`/account-sessions/UUID/actions`:

```json
{"request_key":"status-1","action":"status"}
```

Actions are `status`, `models`, `login`, `renew`, `cancel_login` and `logout`.
They use the same host authentication and actor/workspace as normal run controls;
request bodies cannot supply an actor, path, endpoint, credential or auth token.
DELETE `/account-sessions/UUID` closes the live process tree. The Python caller
provides `accounts`, `open_account_session`, `account_action` and
`close_account_session`.
Select a client timeout above the 30-second action exchange bound when waiting
for native startup; for example `HudsonClient(..., timeout=45)`.

`status` distinguishes expected/installed versions, protocol initialization and
credential presence. `configured` means a credential/account is configured;
it is not a live paid-model acceptance result. `renew` explicitly asks the native
runtime to refresh managed credentials. Failed or unavailable refreshes do not
replay automatically. `models` explicitly asks for the native model catalog,
bounded to four pages/128 model IDs and requiring a configured account; an
unauthenticated peer reports `unsupported` without a model catalog request.
Listing does not establish account access
to generate with any model. No action submits a model turn or grants tools.

`login` starts a managed browser login and returns a transient HTTPS authorization
URL. The operator client owns opening that URL and displaying progress. Poll
`status` with new request keys to observe `pending`, `completed`, `failed` or
`cancelled`. The app-server must stay alive for its browser callback. A session
expires after at most 300 seconds, and host restart or close terminates pending
login observation. There is no detached login daemon or durable login handle.
After a completed login, the vendor owns credentials and renewal files in a
hashed actor/workspace/account profile. Concurrent sessions for that profile
fail closed. Tokens, account email/details, vendor error bodies and auth files
are never returned, logged by Hudson, or copied into model checkpoints. The
authorization URL is sensitive transient control data: responses use `no-store`;
clients must avoid persisting or logging it.

There are at most eight live host sessions, one live session per account, one queued command per session and
32 distinct actions per session. Each action has a 30-second exchange deadline;
the whole process tree expires after the session lifetime. Requests are at most
8 KiB, responses 64 KiB and diagnostics 64 KiB per session. Identical keys/actions
replay their in-memory result without repeating vendor IO; changed actions under
the same key fail. A malformed, oversized, interrupted or ambiguous exchange
closes the session and cannot be retried in it. After expiry/restart, create a
new explicit session and inspect status before deciding on another action. These
keys are transient session idempotency, not durable run submission keys.
The native Unix peer also sets its own 300-second wall-clock alarm so an idle
standalone control process cannot hold a pending login indefinitely.

The bundled SDK control peer at `integrations/agent_sdk/accounts.py` uses the same
protocol for local `openai-agents` version and API-key-presence inspection. Login,
renewal and model catalog actions report `unsupported`: API-key rotation belongs
to the installation's credential source. It makes no provider requests.
Configure `optional_env: ["OPENAI_API_KEY"]` on that account entry so an absent key
can report `required`. Required transport `env` references still reject absent
values. Both lists together allow at most 32 unique names, with the same value
byte limits; names and values are omitted from public discovery.

Account lifecycle controls do not change any published agent, provider key
reference, native model gateway, or resumed run. The current Codex model peer
continues using its one-request host gateway. Subscription-backed execution
requires a separately implemented, explicit immutable account binding; completing
a login does not opt a run into that execution path. UI, hosted account tenancy,
commercial sign-in integration and external-token ingestion are separate work.

The integration point for a future account executor is the installed immutable
account entry `(id, runtime_id, runtime_version)` plus the admitted run's actor.
An executor must pin that nonsecret reference in the agent transport binding and
submission/recovery checks. The peer profile uses the SHA-256 of UTF-8, sorted-key
compact JSON containing `account_id`, `runtime_id`, `runtime_version` and `actor`;
the actor has `workspace_id` and `id`. That profile must stay outside checkpoint
storage. Resolving a profile for another actor or silently falling back between
API-key and account modes must fail before model IO.

The native account methods follow the official
[Codex app-server account contract](https://learn.chatgpt.com/docs/app-server#auth-endpoints).
The bundled adapter targets the pinned executable used by the native execution
peer. Fixture tests cover browser challenges, cancel, renewal, redaction,
ownership and retry handling without contacting a login service. Run installed
readiness, the unauthenticated discovery gate and logout acceptance with:

```sh
python -I scripts/smoke_native_accounts.py --codex /absolute/codex --python /absolute/python
```

This smoke deliberately does not start an interactive login or a model request.
