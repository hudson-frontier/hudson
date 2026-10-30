# Execution conformance

Acceptance distinguishes protocol fixtures, installed runtime execution and paid
provider runs. A declared bridge capability is not evidence that an arbitrary
vendor executable or account is ready.

| Transport or peer | Evidence | Runtime-specific limits |
| --- | --- | --- |
| Built-in provider HTTP | Complete offline provider/tool cycles and documented live smokes | Model-specific modes still need acceptance |
| Operator HTTP/stdio bridge | Bounded protocol, identity, cancellation, streaming, ownership and PostgreSQL fixtures | Operator owns its implementation and vendor acceptance |
| OpenAI Agents SDK 0.24.0 | Real installed SDK, immutable sessions, exact replay, actual isolated worker/tool cycle | Private Unix peer; vendor CLI account/filesystem workflows are separate |
| Codex CLI 0.165.0 | Real installed Linux executable, native rollout resume/fork, actor separation, replay and actual isolated worker/tool cycle | Private Unix checkpoint peer; host functions mediate tools; response is validated before display/dispatch |
| Claude Agent SDK 0.2.175 / Claude Code 2.1.314 | Real installed SDK/native CLI, exact transcript mirrors, tool deferral, replay, actor separation and actual isolated worker/tool cycle | One Messages request per attempt; one host function proposal; continuation rebuilds the host transcript rather than resuming a vendor pending tool |

The native model peers enforce a single provider request per host attempt and keep
customer tool execution in Hudson. The real-executable tests use synthetic
Responses API events, not paid acceptance of a model or subscription account.
Provider credentials remain outside native checkpoints. Windows protocol and
RPC fixtures do not establish Windows checkpoint-peer support.

The [SDK guide](../integrations/agent_sdk/README.md),
[native CLI guide](../integrations/codex/README.md) and
[isolation guide](runtime-isolation.md) record setup, bounds and recovery limits.
The [Claude guide](../integrations/claude/README.md) records its typed Messages
boundary, signed-thinking continuation, cache-inclusive usage and vendor-session
limitations. Its installed-runtime smoke uses synthetic provider events and no
paid calls; no subscription-account or arbitrary native-session acceptance is
claimed.
Remaining vendor/ACP, login, active-turn, attachment and compaction work is
tracked in issue [#91](https://github.com/hudson-frontier/hudson/issues/91).
