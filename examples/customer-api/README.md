# Customer-defined tools and agents

The installation operator supplies `host.json`: approved model and tool
connections, credential environment variable names, ownership, and execution
ceilings. Replace the example tool URL with your service. This example explicitly
trusts that endpoint as read-only; new connections otherwise default to writes
requiring approval. A tool description alone never grants read-only authority.

Start local PostgreSQL and Temporal, then issue a customer API credential:

```sh
cargo run -p hudson-server --bin hudson-credentials -- \
  --database hudson --namespace company --workspace-id company --actor-id backend \
  issue --label customer-backend --ttl-seconds 86400
```

Store the returned token in the customer's `HUDSON_API_TOKEN` environment
variable. The API needs database access and this host catalog, but no execution
credentials or Temporal connection:

```sh
cargo run -p hudson-server -- \
  --catalog examples/customer-api/host.json --database hudson --namespace company \
  --workspace-id company --actor-id backend --require-api-token \
  --temporal-task-queue company
```

In a separate process, provide `OPENAI_API_KEY`, `CUSTOMER_TOOLS_TOKEN`, and the
Temporal connection environment settings to the worker:

```sh
cargo run -p hudson-temporal -- \
  --published --database hudson --namespace company \
  --workspace-id company --actor-id backend --task-queue company worker
```

Customers only supply the tool and agent request files. No endpoint, credential
name, storage namespace, or orchestration code belongs in these public requests:

```sh
hudson agent --api-token-env HUDSON_API_TOKEN capabilities
hudson agent --api-token-env HUDSON_API_TOKEN \
  publish-tool examples/customer-api/tool.json
hudson agent --api-token-env HUDSON_API_TOKEN \
  publish-agent examples/customer-api/agent.json
hudson agent --api-token-env HUDSON_API_TOKEN \
  start --agent customer-assistant --agent-version 1 \
  --task 'Summarize customer C-42' --request-key customer-C42
```

`start` returns the saved run ID. Use `get`, `events`, `children`, `reply`,
`approve`, and `cancel` to inspect and control it. The worker keeps running when
the client or API exits. Reuse request keys when retrying. A changed definition
requires a new version; already-running tasks retain their original definitions.

The equivalent API flow is `GET /capabilities`, `POST /tools`, `POST /agents`, then
`POST /runs` with `agent_ref`, `request_key`, and `input`. The live `/openapi.json`
describes the routes and exact payloads. See [the API guide](../../docs/api.md).

Built-ins are optional: bounded context and user questions are enabled by default;
scoped memory is disabled by default. Inline skills and nested subagents are also
supported. Each task gets its own shared model budget, including its delegated
children; this is an execution bound, not a monetary or account-wide quota.

This server binds to loopback and serves one configured identity. An installation
operator must provision approved connections; public requests cannot add arbitrary
network destinations. Customers currently host their tool implementation as an
HTTP or MCP service. Uploading executable code, a connected-worker SDK, multi-tenant
hosting, and Hudson Sandbox integration are separate work.
