"""One admitted Anthropic Messages request, with host-owned tools and history.

SPDX-License-Identifier: Apache-2.0
"""
import hashlib
import hmac
import http.client
import http.server
import json
import secrets
import threading
import time
import urllib.parse

MAX_BYTES = 4 * 1024 * 1024


def encode(value):
    result = bytearray()
    for part in json.JSONEncoder(ensure_ascii=False, allow_nan=False,
                                 separators=(',', ':')).iterencode(value):
        raw = part.encode('utf-8')
        if len(raw) > MAX_BYTES - len(result):
            raise ValueError('Messages byte ceiling')
        result.extend(raw)
    return bytes(result)


def decode(raw):
    def reject(_):
        raise ValueError('nonstandard JSON constant')
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate JSON key')
            result[key] = value
        return result
    return json.loads(raw.decode('utf-8') if isinstance(raw, bytes) else raw,
                      parse_constant=reject, object_pairs_hook=unique)


def tool_alias(name):
    return 'mcp__hudson__h_' + hashlib.sha256(name.encode('utf-8')).hexdigest()[:32]


def count(value):
    if type(value) is not int or not 0 <= value <= 2**64 - 1:
        raise ValueError('invalid Messages usage')
    return value


def identity(value):
    if not isinstance(value, str) or not 0 < len(value) <= 256 or any(ord(char) < 32 for char in value):
        raise ValueError('invalid Messages identity')
    return value


def usage(value):
    if not isinstance(value, dict):
        raise ValueError('missing Messages usage')
    inputs = sum(count(value.get(key, 0)) for key in
                 ('input_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens'))
    return {'input_tokens': count(inputs), 'output_tokens': count(value['output_tokens'])}


def content(value, allowed):
    """Rebuild supported content; never forward opaque fields to the child."""
    if not isinstance(value, dict):
        raise ValueError('invalid Messages content')
    kind = value.get('type')
    if kind == 'text' and isinstance(value.get('text'), str) and not value.get('citations'):
        return {'type': kind, 'text': value['text']}
    if kind == 'tool_use':
        name = value.get('name')
        if not isinstance(name, str) or name not in allowed or not isinstance(value.get('input'), dict):
            raise ValueError('unadvertised Messages tool')
        return {'type': kind, 'id': identity(value.get('id')), 'name': name, 'input': value['input']}
    if (kind == 'thinking' and isinstance(value.get('thinking'), str)
            and isinstance(value.get('signature'), str) and value['signature']):
        return {'type': kind, 'thinking': value['thinking'], 'signature': value['signature']}
    if kind == 'redacted_thinking' and isinstance(value.get('data'), str) and value['data']:
        return {'type': kind, 'data': value['data']}
    raise ValueError('unsupported Messages content')


def validate_stream(raw, allowed, model, allocation):
    """Validate all SSE records before exposing any tool to the native runtime."""
    normalized = raw.replace(b'\r\n', b'\n')
    if len(raw) > MAX_BYTES or not normalized.endswith(b'\n\n'):
        raise ValueError('Messages stream ceiling or truncation')
    message, active, stopped, final = None, None, False, False
    blocks, seen, records = [], set(), 0
    for record in normalized.split(b'\n\n'):
        lines, event_type = [], None
        for line in record.split(b'\n'):
            if not line or line.startswith(b':'):
                continue
            if line.startswith(b'event:'):
                if event_type is not None:
                    raise ValueError('duplicate Messages event type')
                event_type = line[6:].strip().decode('utf-8')
            elif line.startswith(b'data:'):
                lines.append(line[5:].lstrip(b' '))
            else:
                raise ValueError('unsupported Messages SSE field')
        if not lines:
            continue
        records += 1
        if records > 4096 or stopped:
            raise ValueError('Messages event ceiling or trailing event')
        event = decode(b'\n'.join(lines))
        if not isinstance(event, dict) or event_type != event.get('type'):
            raise ValueError('Messages event type mismatch')
        kind = event['type']
        if kind == 'ping':
            continue
        if kind == 'message_stop':
            if message is None or not final or active is not None:
                raise ValueError('Messages stop out of order')
            stopped = True
            continue
        if kind == 'message_start':
            value = event.get('message')
            if (message is not None or not isinstance(value, dict) or value.get('type') != 'message'
                    or value.get('role') != 'assistant' or value.get('content') != []
                    or value.get('model') != model):
                raise ValueError('Messages start binding changed')
            initial = value.get('usage')
            if not isinstance(initial, dict) or 'input_tokens' not in initial:
                raise ValueError('missing Messages input usage')
            usage(initial)
            message = {'id': identity(value.get('id')), 'type': 'message', 'role': 'assistant',
                       'model': model, 'content': blocks, 'usage': dict(initial)}
        elif message is None or final:
            raise ValueError('Messages event out of order')
        elif kind == 'content_block_start':
            index = event.get('index')
            if type(index) is not int or index != len(blocks) or len(blocks) >= 1024 or active is not None:
                raise ValueError('invalid Messages block order')
            value = event.get('content_block')
            if not isinstance(value, dict):
                raise ValueError('invalid Messages block')
            block = dict(value)
            block_kind = block.get('type')
            if block_kind == 'tool_use':
                content(block, allowed)
                if block['id'] in seen or len(seen) >= 1:
                    raise ValueError('duplicate or excessive Messages calls')
                seen.add(block['id'])
            elif block_kind == 'thinking':
                if not isinstance(block.get('thinking'), str) or not isinstance(block.get('signature', ''), str):
                    raise ValueError('invalid Messages thinking')
                block.setdefault('signature', '')
            else:
                content(block, allowed)
            active = {'index': index, 'block': block, 'json': []}
        elif kind == 'content_block_delta':
            if active is None or event.get('index') != active['index']:
                raise ValueError('Messages delta outside block')
            delta, block = event.get('delta'), active['block']
            if not isinstance(delta, dict):
                raise ValueError('invalid Messages delta')
            delta_kind = delta.get('type')
            if block['type'] == 'tool_use' and delta_kind == 'input_json_delta' and isinstance(delta.get('partial_json'), str):
                active['json'].append(delta['partial_json'])
            elif block['type'] == 'text' and delta_kind == 'text_delta' and isinstance(delta.get('text'), str):
                block['text'] += delta['text']
            elif block['type'] == 'thinking' and delta_kind in ('thinking_delta', 'signature_delta'):
                key = 'thinking' if delta_kind == 'thinking_delta' else 'signature'
                if not isinstance(delta.get(key), str):
                    raise ValueError('invalid Messages thinking delta')
                block[key] += delta[key]
            else:
                raise ValueError('unsupported Messages delta')
        elif kind == 'content_block_stop':
            if active is None or event.get('index') != active['index']:
                raise ValueError('Messages block stop out of order')
            block = active['block']
            if active['json']:
                if block['input']:
                    raise ValueError('conflicting Messages tool arguments')
                block['input'] = decode(''.join(active['json']))
            blocks.append(content(block, allowed))
            active = None
        elif kind == 'message_delta':
            delta, reported = event.get('delta'), event.get('usage')
            if active is not None or not blocks or not isinstance(delta, dict) or not isinstance(reported, dict):
                raise ValueError('invalid Messages completion')
            reason = delta.get('stop_reason')
            if reason not in ('end_turn', 'tool_use') or delta.get('stop_sequence') is not None:
                raise ValueError('Messages turn did not complete')
            if bool(seen) != (reason == 'tool_use'):
                raise ValueError('Messages tool stop mismatch')
            for key in ('input_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens', 'output_tokens'):
                if key in reported:
                    message['usage'][key] = count(reported[key])
            if 'output_tokens' not in reported or usage(message['usage'])['output_tokens'] > allocation:
                raise ValueError('Messages output allocation exceeded')
            message['stop_reason'], message['stop_sequence'] = reason, None
            final = True
        else:
            raise ValueError('unsupported Messages event')
    if not stopped:
        raise ValueError('incomplete Messages stream')
    return message


def stream(message):
    """Canonical SSE strips fields that were not part of the reviewed contract."""
    frames = []
    def emit(kind, **values):
        frames.append(b'event: ' + kind.encode() + b'\ndata: ' + encode(dict(values, type=kind)) + b'\n\n')
    initial = dict(message, content=[], stop_reason=None, stop_sequence=None,
                   usage=dict(message['usage'], output_tokens=0))
    emit('message_start', message=initial)
    for index, block in enumerate(message['content']):
        kind = block['type']
        start = dict(block)
        if kind == 'text':
            start['text'] = ''
        elif kind == 'tool_use':
            start['input'] = {}
        elif kind == 'thinking':
            start.update(thinking='', signature='')
        emit('content_block_start', index=index, content_block=start)
        if kind == 'text':
            emit('content_block_delta', index=index, delta={'type': 'text_delta', 'text': block['text']})
        elif kind == 'tool_use':
            emit('content_block_delta', index=index, delta={'type': 'input_json_delta', 'partial_json': encode(block['input']).decode()})
        elif kind == 'thinking':
            emit('content_block_delta', index=index, delta={'type': 'thinking_delta', 'thinking': block['thinking']})
            emit('content_block_delta', index=index, delta={'type': 'signature_delta', 'signature': block['signature']})
        emit('content_block_stop', index=index)
    emit('message_delta', delta={'stop_reason': message['stop_reason'], 'stop_sequence': None}, usage=message['usage'])
    emit('message_stop')
    output = b''.join(frames)
    if len(output) > MAX_BYTES:
        raise ValueError('canonical Messages stream ceiling')
    return output


def messages(history):
    result = []
    for message in history:
        role, values = message['role'], []
        if role not in ('user', 'assistant', 'tool'):
            raise ValueError('unsupported Messages role')
        calls = [block['call'] for block in message['content'] if block['type'] == 'tool_call']
        native = next((call.get('provider_metadata', {}).get('claude_assistant_content') for call in calls
                       if call.get('provider_metadata', {}).get('claude_assistant_content') is not None), None)
        if native is not None:
            if role != 'assistant' or not isinstance(native, list):
                raise ValueError('invalid Messages continuation')
            aliases = {tool_alias(call['name']) for call in calls}
            values = [content(block, aliases) for block in native]
            actual = [(block['id'], block['name'], block['input']) for block in values if block['type'] == 'tool_use']
            expected = [(call['call_id'], tool_alias(call['name']), call['arguments']) for call in calls]
            if actual != expected:
                raise ValueError('Messages continuation binding changed')
        else:
            for block in message['content']:
                kind = block['type']
                if kind in ('text', 'json') and role in ('user', 'assistant'):
                    value = block['text'] if kind == 'text' else block['value']
                    values.append({'type': 'text', 'text': value if isinstance(value, str) else encode(value).decode()})
                elif kind == 'tool_call' and role == 'assistant':
                    call = block['call']
                    values.append({'type': 'tool_use', 'id': identity(call['call_id']),
                                   'name': tool_alias(call['name']), 'input': call['arguments']})
                elif kind == 'tool_result' and role == 'tool':
                    tool = block['result']
                    values.append({'type': 'tool_result', 'tool_use_id': identity(tool['call_id']),
                                   'content': encode(tool['outcome']).decode(),
                                   'is_error': tool['outcome']['status'] != 'success'})
                else:
                    raise ValueError('unsupported Messages input')
        native_role = 'user' if role == 'tool' else role
        if values:
            if result and result[-1]['role'] == native_role:
                result[-1]['content'].extend(values)
            else:
                result.append({'role': native_role, 'content': values})
    if not result or result[0]['role'] != 'user':
        raise ValueError('Messages input must begin with user context')
    return result


class OneRequestGateway:
    def __init__(self, endpoint, credential, request, allocation):
        url = urllib.parse.urlsplit(endpoint)
        if (not url.hostname or url.username is not None or url.password is not None or url.query or url.fragment
                or (url.scheme != 'https' and not (url.scheme == 'http' and url.hostname in ('127.0.0.1', 'localhost', '::1')))
                or not url.path or not credential or len(credential) > 8192 or '\r' in credential or '\n' in credential
                or type(allocation) is not int or not 0 < allocation <= 2**32 - 1):
            raise ValueError('invalid Messages gateway configuration')
        self.url, self.credential, self.model, self.allocation = url, credential, request['model'], allocation
        self.by_alias = {tool_alias(tool['name']): tool['name'] for tool in request['tools']}
        if len(self.by_alias) != len(request['tools']):
            raise ValueError('duplicate Messages tool alias')
        payload = {'model': self.model, 'system': request['instructions'], 'max_tokens': allocation,
            'stream': True, 'messages': messages(request['messages']),
            'tools': [{'name': tool_alias(tool['name']), 'description': tool['description'],
                       'input_schema': tool['input_schema']} for tool in request['tools']]}
        if request['tools']:
            payload['tool_choice'] = {'type': 'auto', 'disable_parallel_tool_use': True}
        self.request = encode(payload)
        self.token, self.used, self.requests = secrets.token_urlsafe(32), False, 0
        self.lock, self.completed = threading.Lock(), threading.Event()
        self.failure, self.response = None, None
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.0'
            def log_message(self, *_):
                pass
            def reply(self, status, drain=False):
                if drain and self.headers.get('Transfer-Encoding') is None:
                    try:
                        remaining = int(self.headers.get('Content-Length', '0'))
                        deadline = time.monotonic() + 1
                        if 0 <= remaining <= MAX_BYTES:
                            while remaining and time.monotonic() < deadline:
                                self.connection.settimeout(max(.001, deadline - time.monotonic()))
                                chunk = self.rfile.read(min(8192, remaining))
                                if not chunk:
                                    break
                                remaining -= len(chunk)
                    except (ValueError, OSError):
                        pass
                self.send_response(status)
                self.send_header('Content-Length', '0')
                self.end_headers()
            def do_POST(self):
                if not hmac.compare_digest(self.headers.get('x-api-key', '').encode('latin-1'), owner.token.encode()):
                    self.reply(401, drain=True)
                    return
                with owner.lock:
                    if owner.used:
                        self.reply(409, drain=True)
                        return
                    owner.used = True
                replied = False
                try:
                    if self.path not in ('/v1/messages', '/v1/messages?beta=true') or self.headers.get('Transfer-Encoding'):
                        raise ValueError('unsupported Messages route')
                    length = int(self.headers.get('Content-Length', '-1'))
                    if not 0 < length <= MAX_BYTES:
                        raise ValueError('Messages request ceiling')
                    raw = self.rfile.read(length)
                    if len(raw) != length:
                        raise ValueError('truncated Messages request')
                    value = decode(raw)
                    if not isinstance(value, dict) or value.get('model') != owner.model or value.get('stream') is not True:
                        raise ValueError('Messages native request binding changed')
                    connection_type = http.client.HTTPSConnection if owner.url.scheme == 'https' else http.client.HTTPConnection
                    connection = connection_type(owner.url.hostname, owner.url.port, timeout=120)
                    try:
                        owner.requests += 1
                        connection.request('POST', owner.url.path, owner.request,
                            {'x-api-key': owner.credential, 'anthropic-version': '2023-06-01',
                             'Content-Type': 'application/json', 'Accept': 'text/event-stream', 'Accept-Encoding': 'identity'})
                        response = connection.getresponse()
                        if (response.status != 200 or response.getheader('Content-Encoding', 'identity').lower() != 'identity'
                                or not response.getheader('Content-Type', '').lower().startswith('text/event-stream')):
                            raise ValueError('Messages provider response rejected')
                        output, deadline = bytearray(), time.monotonic() + 120
                        while True:
                            chunk = response.read1(8192)
                            if not chunk:
                                break
                            if len(chunk) > MAX_BYTES - len(output) or time.monotonic() > deadline:
                                raise ValueError('Messages provider output ceiling')
                            output.extend(chunk)
                        owner.response = validate_stream(bytes(output), owner.by_alias, owner.model, owner.allocation)
                        canonical = stream(owner.response)
                        replied = True
                        self.send_response(200)
                        self.send_header('Content-Type', 'text/event-stream')
                        self.send_header('Content-Length', str(len(canonical)))
                        self.end_headers()
                        self.wfile.write(canonical)
                        self.wfile.flush()
                    finally:
                        connection.close()
                except Exception:
                    owner.failure = 'Messages gateway rejected an unsafe or ambiguous request'
                    if not replied:
                        try:
                            # A terminal local error avoids turning a rejected
                            # response into a native connection-retry loop. The
                            # upstream allowance remains consumed either way.
                            self.reply(400)
                        except OSError:
                            pass
                    self.close_connection = True
                finally:
                    owner.completed.set()

        class Server(http.server.ThreadingHTTPServer):
            daemon_threads, block_on_close, request_queue_size = True, False, 1
            slots = threading.BoundedSemaphore(2)
            def get_request(self):
                connection, address = super().get_request()
                connection.settimeout(5)
                return connection, address
            def process_request(self, request, address):
                if not self.slots.acquire(blocking=False):
                    self.shutdown_request(request)
                    return
                try:
                    super().process_request(request, address)
                except BaseException:
                    self.slots.release()
                    raise
            def process_request_thread(self, request, address):
                try:
                    super().process_request_thread(request, address)
                finally:
                    self.slots.release()
        self.server = Server(('127.0.0.1', 0), Handler)
        self.base_url = 'http://127.0.0.1:' + str(self.server.server_port)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={'poll_interval': .1}, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=1)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
