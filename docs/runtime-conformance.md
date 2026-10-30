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
| ACP v1 frontend | Standard-library stdio peer, protocol/authority regressions and real Hudson HTTP tool-cycle smoke with synthetic providers | Text-only process-owned sessions; Hudson runs the agent; external ACP vendor execution is not implemented |

The native model peers enforce a single provider request per host attempt and keep
customer tool execution in Hudson. The real-executable tests use synthetic
Responses or Messages API events, not paid acceptance of a model or subscription account.
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

The [ACP guide](../integrations/acp/README.md) documents initialization, sessions,
streamed updates, exact optional permission forwarding, continuation and
cancellation. Its usage metadata retains Hudson's actual reported totals; ACP
context occupancy is not substituted for per-attempt token accounting.

## External vendor execution gaps

An ACP agent is an execution owner, not simply a model transport. The standard
[`session/prompt` lifecycle](https://agentclientprotocol.com/protocol/v1/prompt-turn)
can make several model requests and execute its own tools before returning.
Permission requests are optional. Wrapping that whole prompt as one Hudson model
attempt would not establish per-request admission or host-owned tool execution.
An external runtime needs an independently enforced provider request boundary,
host tool mediation, reported usage, cancellation and private durable state
acceptance before being added as a supported executor.

| Vendor path | Primary contract reviewed | Hudson executor status |
| --- | --- | --- |
| Gemini CLI ACP | [Official ACP mode](https://geminicli.com/docs/cli/acp-mode/) exposes the CLI's agent session and approval modes | Not implemented; the built-in Gemini provider transport is separate |
| OpenCode ACP | [Official ACP support](https://opencode.ai/docs/acp/) includes native tools, project rules and configured MCP servers | Not implemented; no installed-runtime acceptance claimed |
| Claude Code / agent SDK | [Official CLI reference](https://code.claude.com/docs/en/cli-reference) describes its native session and headless execution | Guarded SDK executor above; arbitrary native pending-tool resume remains unsupported |
| Pi SDK/native | No installed-runtime acceptance yet | Not implemented |
| Antigravity | No installed-runtime acceptance yet | Not implemented |
| Cursor | No installed-runtime acceptance yet | Not implemented |
| Devin | No installed-runtime acceptance yet | Not implemented |
| Goose | No installed-runtime acceptance yet | Not implemented |
| Hermes | No installed-runtime acceptance yet | Not implemented |
| Kimi | No installed-runtime acceptance yet | Not implemented |
| Kiro | No installed-runtime acceptance yet | Not implemented |
| Qwen | No installed-runtime acceptance yet | Not implemented |

An installed executable, successful protocol initialization or configured account
alone is not evidence of safe executor support. No external vendor is enabled by
the ACP frontend, and no unsupported one-step extension is assumed.

Remaining vendor execution, login, active-turn and attachment work is
tracked in issue [#91](https://github.com/hudson-frontier/hudson/issues/91).
