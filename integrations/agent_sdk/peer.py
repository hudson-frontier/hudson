"""Native SDK peer: immutable SDK checkpoints, host-executed tool proposals.

SPDX-License-Identifier: Apache-2.0
"""
import argparse
import asyncio
import hashlib
import importlib.metadata
import json
import os
import pathlib
import stat
import sys
import uuid

REQUEST_BYTES = 4 * 1024 * 1024
SNAPSHOT_BYTES = 8 * 1024 * 1024
RESPONSE_BYTES = 1024 * 1024
MAX_SNAPSHOTS = 256
MAX_ITEMS = 8192
SDK_VERSION = '0.24.0'


def encode(value, limit):
    result = bytearray()
    for part in json.JSONEncoder(ensure_ascii=False, allow_nan=False,
                                 separators=(',', ':')).iterencode(value):
        raw = part.encode('utf-8')
        if len(raw) > limit - len(result):
            raise ValueError('encoded byte ceiling')
        result.extend(raw)
    return bytes(result)


def decode(raw):
    return json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError('JSON constant')))


def checkpoint_name(call):
    operation = str(uuid.UUID(call['operation_id']))
    attempt = str(uuid.UUID(call['attempt_id']))
    return operation + '-' + attempt + '.json'


def private_directory(path):
    if os.name != 'posix' or not path.is_absolute():
        raise ValueError('private Unix state directory required')
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    metadata = path.lstat()
    if (not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid() or
            stat.S_IMODE(metadata.st_mode) & 0o077):
        raise ValueError('private state ownership required')
    return path


class Checkpoints:
    def __init__(self, context):
        root = private_directory(pathlib.Path(os.environ['HUDSON_SDK_STATE_DIR']))
        owner = hashlib.sha256(encode(context['actor'], 2048)).hexdigest()
        self.path = private_directory(root / owner)
        self.directory = os.open(self.path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)

    def close(self):
        os.close(self.directory)

    def read(self, call):
        name = checkpoint_name(call)
        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=self.directory)
        with os.fdopen(fd, 'rb') as source:
            metadata = os.fstat(source.fileno())
            if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or
                    metadata.st_nlink != 1 or stat.S_IMODE(metadata.st_mode) & 0o077):
                raise ValueError('invalid checkpoint ownership')
            raw = source.read(SNAPSHOT_BYTES + 1)
        if len(raw) > SNAPSHOT_BYTES:
            raise ValueError('checkpoint byte ceiling')
        return decode(raw)

    def has_capacity(self):
        count = 0
        with os.scandir(self.path) as files:
            for entry in files:
                if entry.name.endswith(('.json', '.pending')):
                    count += 1
                    if count >= MAX_SNAPSHOTS:
                        return False
        return True

    def begin(self, call):
        name = checkpoint_name(call) + '.pending'
        fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=self.directory)
        os.close(fd)
        os.fsync(self.directory)

    def completed(self, call):
        os.unlink(checkpoint_name(call) + '.pending', dir_fd=self.directory)
        os.fsync(self.directory)

    def save(self, call, value):
        raw = encode(value, SNAPSHOT_BYTES)
        name = checkpoint_name(call)
        temporary = '.tmp-' + uuid.uuid4().hex
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=self.directory)
        try:
            with os.fdopen(fd, 'wb') as target:
                target.write(raw)
                target.flush()
                os.fsync(target.fileno())
            # A checkpoint is immutable. Hard-link publication cannot replace
            # another attempt's receipt, including a concurrent duplicate call.
            os.link(temporary, name, src_dir_fd=self.directory, dst_dir_fd=self.directory,
                    follow_symlinks=False)
            os.fsync(self.directory)
        finally:
            os.unlink(temporary, dir_fd=self.directory)


def input_items(messages, append=False):
    items = []
    for message in messages:
        role = message['role']
        if append and role == 'assistant':
            # The previous SDK result already saved its native assistant items.
            continue
        blocks = []
        for block in message['content']:
            kind = block['type']
            if kind in ('text', 'json'):
                value = block['text'] if kind == 'text' else block['value']
                blocks.append({'type': 'input_text', 'text': value if isinstance(value, str)
                               else encode(value, REQUEST_BYTES).decode()})
            elif kind == 'image' and role == 'user':
                image = block['image']
                blocks.append({'type': 'input_image',
                               'image_url': 'data:' + image['media_type'] + ';base64,' + image['data']})
            elif kind == 'tool_call' and role == 'assistant':
                call = block['call']
                metadata = call.get('provider_metadata', {})
                items.extend(metadata.get('sdk_input_prefix', []))
                items.append({'type': 'function_call', 'call_id': call['call_id'],
                              'name': call['name'], 'arguments': encode(call['arguments'], REQUEST_BYTES).decode()})
            elif kind == 'tool_result' and role == 'tool':
                result = block['result']
                items.append({'type': 'function_call_output', 'call_id': result['call_id'],
                              'output': encode(result['outcome'], REQUEST_BYTES).decode()})
            else:
                raise ValueError('unsupported native SDK content')
        if blocks:
            if role not in ('user', 'assistant'):
                raise ValueError('invalid native SDK role')
            items.append({'role': role, 'content': blocks})
    return items


class NativeSession:
    """Bounded implementation of the actual SDK Session protocol."""
    def __init__(self, session_id, items):
        self.session_id = session_id
        self.items = list(items)

    async def get_items(self, limit=None):
        return list(self.items if limit is None else self.items[-limit:])

    async def add_items(self, items):
        if len(self.items) + len(items) > MAX_ITEMS:
            raise ValueError('native session item ceiling')
        encode(self.items + items, SNAPSHOT_BYTES)
        self.items.extend(items)

    async def pop_item(self):
        return self.items.pop() if self.items else None

    async def clear_session(self):
        self.items.clear()


async def run_sdk(body, saved, emit):
    from agents import Agent, FunctionTool, ModelSettings, RunConfig, Runner, set_tracing_disabled
    from agents.models.openai_responses import OpenAIResponsesModel
    from openai import AsyncOpenAI
    import httpx2 as httpx
    from urllib.parse import urlsplit

    if importlib.metadata.version('openai-agents') != SDK_VERSION:
        raise ValueError('unsupported SDK version')
    set_tracing_disabled(True)
    context, request = body['context'], body['request']
    previous = context.get('previous')
    append = bool(saved is not None and context['history_mode'] == 'append')
    if append:
        if (saved['sdk_version'] != SDK_VERSION or saved['runtime_id'] != body['runtime_id'] or
                saved['runtime_version'] != body['runtime_version'] or
                saved['request_fingerprint'] != previous['request_fingerprint'] or
                saved['input_messages'] != previous['input_messages']):
            raise ValueError('native checkpoint binding changed')
        history = saved['items']
        delta = input_items(request['messages'][previous['input_messages']:], append=True)
    else:
        history, delta = [], input_items(request['messages'])
    session = NativeSession(str(uuid.UUID(context['call']['run_id'])), history)
    base_url = os.environ.get('HUDSON_SDK_BASE_URL', 'https://api.openai.com/v1')
    url = urlsplit(base_url)
    if (url.scheme != 'https' and not (url.scheme == 'http' and url.hostname in ('localhost', '127.0.0.1', '::1'))
            or url.username is not None or url.password is not None or url.query or url.fragment):
        raise ValueError('invalid SDK endpoint')

    class LimitedStream(httpx.AsyncByteStream):
        def __init__(self, inner):
            self.inner = inner

        async def __aiter__(self):
            total = 0
            async for chunk in self.inner:
                total += len(chunk)
                if total > REQUEST_BYTES:
                    raise ValueError('native model response byte ceiling')
                yield chunk

        async def aclose(self):
            await self.inner.aclose()

    class Transport(httpx.AsyncBaseTransport):
        def __init__(self):
            self.requests = 0
            self.inner = httpx.AsyncHTTPTransport(retries=0, trust_env=False,
                limits=httpx.Limits(max_connections=2, max_keepalive_connections=1))

        async def handle_async_request(self, request):
            if self.requests != 0:
                raise ValueError('native HTTP request ceiling')
            self.requests += 1
            request.headers['Accept-Encoding'] = 'identity'
            response = await self.inner.handle_async_request(request)
            if response.headers.get('content-encoding', 'identity').lower() != 'identity':
                await response.aclose()
                raise ValueError('unsupported native response encoding')
            response.stream = LimitedStream(response.stream)
            return response

        async def aclose(self):
            await self.inner.aclose()

    class OneCallModel(OpenAIResponsesModel):
        calls = 0

        def admitted(self):
            if self.calls != 0:
                raise ValueError('native model call ceiling')
            self.calls += 1

        async def get_response(self, *args, **kwargs):
            self.admitted()
            return await super().get_response(*args, **kwargs)

        async def stream_response(self, *args, **kwargs):
            self.admitted()
            async for event in super().stream_response(*args, **kwargs):
                yield event

    async def deferred_tool(_context, _arguments):
        raise ValueError('tool execution belongs to the host')

    async def combine(history, new_input):
        return history + new_input

    tools = [FunctionTool(name=tool['name'], description=tool['description'],
              params_json_schema=tool['input_schema'], strict_json_schema=False,
              on_invoke_tool=deferred_tool, needs_approval=True) for tool in request['tools']]
    async with httpx.AsyncClient(transport=Transport(), follow_redirects=False, trust_env=False,
                                timeout=httpx.Timeout(120, connect=10)) as http:
        async with AsyncOpenAI(api_key=os.environ['OPENAI_API_KEY'], base_url=base_url,
                               max_retries=0, timeout=120, http_client=http) as client:
            model = OneCallModel(model=request['model'], openai_client=client)
            agent = Agent(name='Hudson', instructions=request['instructions'], model=model, tools=tools,
                model_settings=ModelSettings(max_tokens=body['max_output_tokens'], parallel_tool_calls=False,
                    truncation='disabled', store=False, response_include=['reasoning.encrypted_content']))
            result = Runner.run_streamed(agent, delta, session=session, max_turns=1,
                run_config=RunConfig(tracing_disabled=True, trace_include_sensitive_data=False,
                                     session_input_callback=combine))
            async for event in result.stream_events():
                if event.type == 'raw_response_event' and event.data.type == 'response.output_text.delta':
                    emit(event.data.delta)
            calls = []
            for item in result.interruptions:
                raw = item.raw_item
                if isinstance(raw, dict):
                    name, call_id, arguments = raw['name'], raw['call_id'], raw['arguments']
                else:
                    name, call_id, arguments = raw.name, raw.call_id, raw.arguments
                calls.append({'name': name, 'call_id': call_id, 'arguments': decode(arguments)})
            if calls:
                response = {'type': 'tool_calls', 'calls': calls}
            else:
                output = result.final_output
                if isinstance(output, str):
                    try:
                        output = decode(output)
                    except (ValueError, UnicodeError):
                        pass
                response = {'type': 'final', 'output': output}
            if model.calls != 1:
                raise ValueError('native call accounting mismatch')
            usage = result.context_wrapper.usage
            checkpoint = dict(context['call'], request_fingerprint=context['request_fingerprint'],
                              input_messages=len(request['messages']))
            reply = {'protocol_version': 1, 'runtime_id': body['runtime_id'], 'runtime_version': body['runtime_version'],
                'response': response, 'usage': {'input_tokens': usage.input_tokens, 'output_tokens': usage.output_tokens},
                'checkpoint': checkpoint}
            return reply, session.items


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe', action='store_true')
    args = parser.parse_args()
    if args.probe:
        try:
            version = importlib.metadata.version('openai-agents')
        except importlib.metadata.PackageNotFoundError:
            version = None
        print(json.dumps({'sdk': 'openai-agents', 'version': version, 'available': version == SDK_VERSION,
                          'credential_configured': bool(os.environ.get('OPENAI_API_KEY'))}))
        return
    raw = sys.stdin.buffer.read(REQUEST_BYTES + 1)
    if len(raw) > REQUEST_BYTES:
        raise ValueError('request byte ceiling')
    body = decode(raw)
    if body['protocol_version'] != 1:
        raise ValueError('unsupported protocol')
    context = body['context']
    checkpoints = Checkpoints(context)
    try:
        try:
            cached = checkpoints.read(context['call'])
        except FileNotFoundError:
            cached = None
        if cached is not None:
            if cached['request_fingerprint'] != context['request_fingerprint']:
                raise ValueError('checkpoint request changed')
            reply = cached['reply']
            if body.get('stream'):
                for value in [{'type': 'start', 'protocol_version': 1, 'runtime_id': body['runtime_id'], 'runtime_version': body['runtime_version']},
                              {'type': 'response', 'result': reply}]:
                    sys.stdout.buffer.write(encode(value, RESPONSE_BYTES) + b'\n')
                sys.stdout.buffer.flush()
                return
        else:
            if not checkpoints.has_capacity():
                raise ValueError('native checkpoint capacity')
            # A crashed/ambiguous attempt keeps its pending marker. Reusing it
            # cannot start another paid request merely because a process died.
            checkpoints.begin(context['call'])
            previous = context.get('previous')
            saved = checkpoints.read(previous) if previous is not None and context['history_mode'] == 'append' else None
            streaming = bool(body.get('stream'))
            written = 0

            def frame(value):
                nonlocal written
                data = encode(value, RESPONSE_BYTES) + b'\n'
                written += len(data)
                if written > RESPONSE_BYTES:
                    raise ValueError('stream byte ceiling')
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()

            if streaming:
                frame({'type': 'start', 'protocol_version': 1, 'runtime_id': body['runtime_id'], 'runtime_version': body['runtime_version']})
            reply, items = asyncio.run(run_sdk(body, saved,
                lambda text: frame({'type': 'text_delta', 'text': text}) if streaming else None))
            checkpoints.save(context['call'], {'sdk_version': SDK_VERSION, 'runtime_id': body['runtime_id'],
                'runtime_version': body['runtime_version'], 'request_fingerprint': context['request_fingerprint'],
                'input_messages': len(body['request']['messages']), 'items': items, 'reply': reply})
            checkpoints.completed(context['call'])
            if streaming:
                frame({'type': 'response', 'result': reply})
                return
        sys.stdout.buffer.write(encode(reply, RESPONSE_BYTES))
        sys.stdout.buffer.flush()
    finally:
        checkpoints.close()


if __name__ == '__main__':
    try:
        main()
    except Exception:
        # SDK errors may contain credentials, prompts or provider bodies.
        print('native SDK execution failed; inspect scoped runtime state', file=sys.stderr)
        raise SystemExit(1) from None
