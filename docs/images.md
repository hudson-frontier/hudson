# Explicit image input

`POST /runs/images` accepts the usual JSON task plus inline image attachments:

```json
{"input":"Describe this image","request_key":"photo-once","images":[{"media_type":"image/png","data":"BASE64_BYTES"}]}
```

Supply standard base64 bytes without a data-URL prefix. PNG, JPEG and WebP media
types are accepted. Each image is capped at 2 MiB decoded, at most four images
are accepted in a new turn, and encoded image data is capped at 4 MiB across a
model request with at most 16 retained images. Hudson validates transport
encoding and size; the chosen provider validates the image format. Hudson never
decompresses image data, reads an image path or fetches an image URL.

The endpoint uses a 5 MiB body ceiling and shares four large-request slots with
conversation import/fork. A 30-second request deadline applies; admitted database
work retains its slot until it actually finishes. Authentication still precedes
body extraction. The usual configured agent is used; published mode requires
an actor-owned `agent_ref` and restores its pinned goal, policies and budget.

Agent payload and context limits continue to apply. Larger photographs normally
need an explicit configuration such as `max_payload_bytes: 3145728` and
`max_context_bytes: 4194304` under `limits`. The JSON `input_schema` validates the
original task value. Images do not change that business-data shape. The retry
key binds task input, image bytes and media types, agent, goal and budget;
changing an image under an accepted key conflicts before execution.

The Chat Completions transport sends user `image_url` blocks containing data
URLs. The Anthropic transport sends native base64 `image` source blocks. These
follow the [OpenAI image-input format](https://developers.openai.com/api/docs/guides/images-vision)
and [Anthropic image-input format](https://platform.claude.com/docs/en/build-with-claude/vision).
The configured model must support image input; transport support is not live
acceptance evidence for every model. Protocol tests use local fixtures and make
no provider calls.

SDK bridges require explicit operator opt-in through `runtime.images: true`.
The default is false and rejects image requests before external IO. Enabled
bridges receive portable `{"type":"image","image":{"media_type":...,"data":...}}`
content blocks. The setting is part of the immutable agent transport binding.
`GET /harnesses` reports the transport capability without claiming model readiness.

Images remain in user history across tool cycles, checkpoints, PostgreSQL
reconnect and portable conversation export. Context archival keeps image-bearing
messages live instead of converting their bytes into text excerpts. If live
images cannot fit the context limit, the next model request fails explicitly;
there is no silent resize, truncation or hidden summarization call. A transcript
must retain image blocks in live user messages, not quoted archive exchanges.
Image-bearing exports use transcript `format_version: 2`; the reader accepts
versions 1 and 2, and text-only exports retain version 1.

Fork/import requests may include `images` for the new turn. Those requests use
the same 5 MiB body bound and shared large-request slots. Earlier tools and
approvals are still not inherited. Ordinary `/runs` JSON and queued JSON inputs
remain data; a JSON object resembling an image is not automatically promoted to
an attachment. Provider file IDs, URLs, PDFs and image-bearing tool results need
separate typed integrations.

The Python caller provides `start_images(input, images, request_key=...)` and
optional `images=` on `fork`/`import_conversation`. A caller that chooses a local
file should read it with an explicit bound before encoding:

```python
import base64
with open('photo.png', 'rb') as stream:
    data = stream.read(2 * 1024 * 1024 + 1)
if len(data) > 2 * 1024 * 1024:
    raise ValueError('image exceeds 2 MiB')
image = {'media_type': 'image/png', 'data': base64.b64encode(data).decode('ascii')}
run = client.start_images('Describe this image', [image], request_key='photo-once')
```

Rust callers use `Runtime::submit_images` with `ImageTurn`, and
`import_turn_with_images` when transferring a conversation. Source-breaking enum
and configuration changes are listed in the preview compatibility guide.

## Transport measurement

Run `cargo build --locked -p hudson-core --example bridge_benchmark` followed by
`python3 scripts/bench_runtime_bridge.py`. The image mode performs 25 sequential
loopback calls with 2097150 synthetic decoded bytes, 1 KiB text, a 32-token cap
and constant output. It does not parse an actual image or call a vision provider.

The same fixture and benchmark driver compared the initial encoder (`919dc49`)
with ownership transfers between JSON containers (`f804a50`):

| Encoder | p50 | p95 | Peak process RSS |
| --- | --- | --- | --- |
| Initial image encoder | 178.371 ms | 211.788 ms | 26588 KiB |
| JSON containers moved by ownership | 180.439 ms | 204.947 ms | 27412 KiB |

These measurements establish no latency or peak-memory improvement within run
variation. The implementation avoids repeated intermediate JSON-container copies;
base64 validation and transport serialization remain linear in encoded bytes.
Memory remains explicitly bounded by attachment, request and concurrency ceilings.
No production throughput, billing or model-quality claim follows from this test.
