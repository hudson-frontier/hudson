# Local Python client

`accounts()`, `open_account_session()`, `account_action()` and
`close_account_session()` expose explicitly configured native account controls.
Sessions and login challenges are transient; no method changes a run's execution
binding. See [account lifecycle and readiness](../../docs/runtime-accounts.md).

`run_spend(run_id)` and `operation_spend(operation_id)` read configured model
allocations and priced/unknown outcomes. Amounts use decimal strings in nano USD;
missing usage stays `None`. Inspect a model-spend quote before using the normal
approval control. See [allocation semantics](../../docs/spending.md).

The standard-library `hudson_client.py` module targets the documented preview
HTTP API. Python is used by the existing publication/restart acceptance caller
in `scripts/smoke_publication.py`; this client replaces repeated transport code
for that caller. No package registry installation is needed. Requires Python 3.12+.

Run from the repository root with `PYTHONPATH=integrations/python`, or add that
directory to your application's module path. See
[preview compatibility](../../docs/api-compatibility.md) before upgrading.

```python
from hudson_client import HudsonClient, HttpError

client = HudsonClient('http://127.0.0.1:4318', token_env='HUDSON_API_TOKEN')
# tool.json and agent.json follow examples/customer-api/README.md and select
# connections and model profiles already provisioned by the operator.
import json
with open('tool.json', encoding='utf-8') as stream:
    tool = json.load(stream)
with open('agent.json', encoding='utf-8') as stream:
    agent = json.load(stream)
client.publish_tool(tool, request_key='tool-v1')
published = client.publish_agent(agent, request_key='agent-v1')
run_id = client.start({'task': 'Look up record 42'}, request_key='record-42',
                      agent_ref=published['agent_ref'])['run_id']

# Persist run_id and the last fully processed event sequence in your application.
after = 0
for record in client.follow(run_id, after=after):
    print(record)
    after = record['after']  # save only after processing the record
# Reconnect with the same run_id and after. Observation only makes GET requests.
```

`follow` drains every event page before reporting the status snapshot. It returns
on completed, failed, cancelled, or waiting status. Waiting is not completion:
inspect `record['run']['wait']`, then explicitly call `approve(operation_id,
approved=True)` or `reply(run_id, value, request_key=..., question_id=...)` as
appropriate. Question identity comes from `wait.question_id`. `resume` and
`cancel` are explicit controls. Closing the observation generator never cancels
or resumes the run.

`stream_events(run_id, after=cursor)` observes one SSE connection and yields the
original event objects. Save each event's `sequence` only after processing it.
The generator closes after 10000 events, 300 seconds, the configured response
byte ceiling, or server EOF. These bounds can be lowered with `max_events`,
`max_seconds` and `max_response_bytes`. Socket idle timeout still applies.
Reconnect explicitly with the last saved cursor. Partial `model_output_delta`
events are provisional; inspect `run(run_id)` for the verified final result.

`export_conversation(run_id)` returns a portable transcript from a completed
actor-owned run. Use `fork(run_id, input, request_key=...)` for a local source or
`import_conversation(transcript, input, request_key=...)` for explicit transfer.
Published hosts also require the target `agent_ref`. The new task gets fresh
limits, usage and policy checks; earlier tool calls are never replayed. Import
has a separate 5 MiB request ceiling; image-bearing forks use that ceiling too. See
[format and ownership rules](../../docs/conversations.md).

`start_images(input, images, request_key=...)` sends explicit inline PNG/JPEG/WebP
attachments. Each entry contains `media_type` and standard base64 `data`; no
paths or URLs are resolved. Fork/import accept optional `images=` for the new
turn. See [image limits and a bounded file-reading example](../../docs/images.md).

`queue_message(run_id, value, kind='steer', request_key='direction-1')` adds user
direction at the next complete boundary. Use `kind='follow_up'` to wait for a
verified answer first. `queued_messages` inspects pending input and
`clear_queued_messages` withdraws it. Replays retain their original receipt even
after consumption or withdrawal. Queue controls never approve, answer a saved
question, or resume execution. See [limits and ordering](../../docs/harness-controls.md).

All requests time out (default 15 seconds; maximum 60). Encoded request bodies
are capped at 64 KiB except the explicit 5 MiB transfer/image methods; responses
default to 8 MiB and can be bounded lower.
Event pages contain 1–1000 events. Observation has explicit total page/poll
budgets and bounded poll intervals. Exhaustion reports the last processed
cursor. HTTP and transport failures stop observation, preserving previously
yielded cursors. The client makes no automatic retries or extra model calls.
Truncated HTTP responses are transport failures even when the received prefix
contains valid JSON.

Reuse the exact publication/submission key and body after an ambiguous failure;
never invent a new key to retry a write. Reusing a version/key with changed
content produces HTTP 409. Approval and resume are not blindly retried by this
client. After an uncertain external effect, follow the ordinary
[recovery procedure](../../docs/recovery.md).

URLs must be HTTPS or loopback HTTP origins without URL credentials, paths,
queries or fragments. Redirects and environment proxies are disabled. TLS uses
the platform trust configuration. Tokens are absent from client representations
and exception text; error responses are identified by HTTP status before JSON
decoding, including plain-text framework failures. `HttpError.status` exposes
the status. Missing/expired/revoked credentials surface as errors and are never
silently refreshed. Construct a new client with the rotated credential.

Run protocol regressions with:

```sh
python3 -m unittest discover -s integrations/python -v
python3 scripts/check_api_compatibility.py
```

The ordinary check runner includes these offline tests. The opt-in local
PostgreSQL/Temporal suite additionally runs the client against authenticated
publication, conflicts, approval waits, API/worker restarts and reconnects.
Neither test path needs paid providers or hosted infrastructure.
