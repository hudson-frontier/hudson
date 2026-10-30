"""A single provider request with host-pinned tools and output allocation.

SPDX-License-Identifier: Apache-2.0
"""
import hashlib
import hmac
import http.client
import http.server
import json
import secrets
import threading
import urllib.parse

MAX_BYTES = 4 * 1024 * 1024


def tool_alias(name):
    # Native builtins cannot collide with arbitrary customer-facing aliases.
    return 'hudson_tool_' + hashlib.sha256(name.encode('utf-8')).hexdigest()[:32]


def decode(raw):
    return json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError('JSON constant')))


def validate_response(raw, allowed):
    """Validate every streamed item before a vendor runtime can act on it.

    Buffering is deliberate: filtering only the final output would allow an
    earlier unadvertised builtin to execute before its receipt was rejected.
    """
    events = {
        'response.created', 'response.in_progress', 'response.output_item.added',
        'response.output_item.done', 'response.content_part.added', 'response.content_part.done',
        'response.output_text.delta', 'response.output_text.done',
        'response.function_call_arguments.delta', 'response.function_call_arguments.done',
        'response.reasoning_summary_part.added', 'response.reasoning_summary_part.done',
        'response.reasoning_summary_text.delta', 'response.reasoning_summary_text.done',
        'response.reasoning_text.delta', 'response.reasoning_text.done', 'response.completed',
    }
    def item(value, complete=False):
        if not isinstance(value, dict):
            raise ValueError('invalid provider item')
        kind = value.get('type')
        if kind == 'function_call':
            if (not isinstance(value.get('name'), str) or value['name'] not in allowed or not isinstance(value.get('arguments'), str)
                    or value.get('namespace') is not None):
                raise ValueError('unadvertised native function')
            if complete:
                identifier = value.get('call_id')
                if not isinstance(identifier, str) or not 0 < len(identifier) <= 256:
                    raise ValueError('invalid native call identity')
                decode(value['arguments'])
        elif kind == 'message':
            if value.get('role') != 'assistant' or not isinstance(value.get('content'), list):
                raise ValueError('invalid assistant item')
            for block in value['content']:
                if (not isinstance(block, dict) or block.get('type') != 'output_text'
                        or not isinstance(block.get('text'), str)):
                    raise ValueError('unsupported assistant content')
        elif kind != 'reasoning':
            raise ValueError('unadvertised native item')

    completed = None
    created = None
    count = 0
    normalized = raw.replace(b'\r\n', b'\n')
    if not normalized.endswith(b'\n\n'):
        raise ValueError('truncated provider record')
    for record in normalized.split(b'\n\n'):
        lines = [line[5:].lstrip(b' ') for line in record.split(b'\n') if line.startswith(b'data:')]
        if not lines:
            continue
        count += 1
        if count > 4096:
            raise ValueError('provider event ceiling')
        data = b'\n'.join(lines)
        if data == b'[DONE]' and completed is not None:
            continue
        if completed is not None:
            raise ValueError('provider event after completion')
        event = decode(data)
        if not isinstance(event, dict) or not isinstance(event.get('type'), str) or event['type'] not in events:
            raise ValueError('unsupported provider event')
        kind = event['type']
        if kind in ('response.output_item.added', 'response.output_item.done'):
            item(event.get('item'), complete=kind.endswith('.done'))
        elif kind in ('response.content_part.added', 'response.content_part.done'):
            part = event.get('part')
            if not isinstance(part, dict) or part.get('type') != 'output_text':
                raise ValueError('unsupported native content part')
        elif kind.startswith('response.function_call_arguments.'):
            if 'name' in event and (not isinstance(event['name'], str) or event['name'] not in allowed):
                raise ValueError('unadvertised native argument event')
        elif kind == 'response.created':
            if created is not None:
                raise ValueError('duplicate provider start')
            response = event.get('response')
            if not isinstance(response, dict):
                raise ValueError('invalid provider start')
            created = response.get('id')
            if not isinstance(created, str) or not 0 < len(created) <= 256:
                raise ValueError('invalid provider response identity')
        elif kind == 'response.completed':
            value = event.get('response')
            if (not isinstance(value, dict) or value.get('id') != created or created is None
                    or value.get('status') != 'completed' or not isinstance(value.get('output'), list)
                    or not 0 < len(value['output']) <= 1024):
                raise ValueError('invalid provider completion')
            calls = set()
            for output in value['output']:
                item(output, complete=True)
                if output['type'] == 'function_call':
                    identifier = output['call_id']
                    if identifier in calls or len(calls) >= 128:
                        raise ValueError('duplicate or excessive native calls')
                    calls.add(identifier)
            usage = value.get('usage')
            if usage is not None:
                if not isinstance(usage, dict):
                    raise ValueError('invalid provider usage')
                counts = [usage.get(key) for key in ('input_tokens', 'output_tokens')]
                if any(type(n) is not int or not 0 <= n <= 2**64-1 for n in counts) or sum(counts) > 2**64-1:
                    raise ValueError('invalid provider usage')
            completed = value
    if completed is None:
        raise ValueError('incomplete provider stream')
    return completed


def encode(value):
    result = bytearray()
    for part in json.JSONEncoder(ensure_ascii=False, allow_nan=False,
                                 separators=(',', ':')).iterencode(value):
        raw = part.encode('utf-8')
        if len(raw) > MAX_BYTES - len(result):
            raise ValueError('provider request byte ceiling')
        result.extend(raw)
    return bytes(result)


class OneRequestGateway:
    def __init__(self, endpoint, credential, model, tools, max_output_tokens, reasoning_effort=None):
        url = urllib.parse.urlsplit(endpoint)
        local = url.hostname in ('localhost', '127.0.0.1', '::1')
        if ((url.scheme != 'https' and not (url.scheme == 'http' and local)) or not url.hostname
                or url.username is not None or url.password is not None or url.query or url.fragment
                or not url.path or not credential or len(credential) > 8192
                or '\r' in credential or '\n' in credential
                or type(max_output_tokens) is not int or not 0 < max_output_tokens <= 2**32-1):
            raise ValueError('invalid provider gateway configuration')
        if reasoning_effort is not None and reasoning_effort not in ('none','minimal','low','medium','high','xhigh','max'):
            raise ValueError('unsupported native reasoning effort')
        self.url, self.credential = url, credential
        self.model, self.max_output_tokens = model, max_output_tokens
        self.reasoning_effort = reasoning_effort
        self.by_alias = {tool_alias(tool['name']): tool['name'] for tool in tools}
        if len(self.by_alias) != len(tools):
            raise ValueError('duplicate native tool alias')
        self.tools = [dict(tool, name=tool_alias(tool['name']),
                          description=tool['name']+': '+tool['description']) for tool in tools]
        self.token = secrets.token_urlsafe(32)
        self.used = False
        self.lock = threading.Lock()
        self.completed = threading.Event()
        self.failure = None
        self.body = None
        self.response = None
        self.digest = None
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.0'

            def log_message(self, *_):
                pass

            def reply(self, status):
                self.send_response(status)
                self.send_header('Content-Length', '0')
                self.end_headers()

            def do_POST(self):
                if not hmac.compare_digest(self.headers.get('Authorization', '').encode('latin-1'),
                                           ('Bearer '+owner.token).encode('ascii')):
                    self.reply(401)
                    return
                with owner.lock:
                    if owner.used:
                        self.reply(409)
                        return
                    # Consumption precedes parsing/IO: malformed or ambiguous
                    # requests never authorize a second provider request.
                    owner.used = True
                try:
                    if self.path != '/v1/responses' or self.headers.get('Transfer-Encoding') is not None:
                        raise ValueError('unsupported provider route')
                    length = int(self.headers.get('Content-Length', '-1'))
                    if not 0 < length <= MAX_BYTES:
                        raise ValueError('provider request byte ceiling')
                    raw = self.rfile.read(length)
                    if len(raw) != length:
                        raise ValueError('truncated provider request')
                    value = json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError('JSON constant')))
                    if not isinstance(value, dict) or value.get('model') != owner.model:
                        raise ValueError('provider model binding changed')
                    value['tools'] = [{'type':'function', 'name':tool['name'],
                        'description':tool['description'], 'parameters':tool['input_schema'],
                        'strict':False} for tool in owner.tools]
                    value['tool_choice'] = 'auto'
                    value['parallel_tool_calls'] = False
                    value['max_output_tokens'] = owner.max_output_tokens
                    if owner.reasoning_effort is not None:
                        reasoning = value.get('reasoning') or {}
                        if not isinstance(reasoning, dict):
                            raise ValueError('invalid native reasoning object')
                        value['reasoning'] = dict(reasoning, effort=owner.reasoning_effort)
                    value['store'] = False
                    value['stream'] = True
                    request = encode(value)
                    connection_type = http.client.HTTPSConnection if owner.url.scheme == 'https' else http.client.HTTPConnection
                    connection = connection_type(owner.url.hostname, owner.url.port, timeout=120)
                    try:
                        connection.request('POST', owner.url.path, request, {
                            'Authorization':'Bearer '+owner.credential, 'Content-Type':'application/json',
                            'Accept':'text/event-stream', 'Accept-Encoding':'identity'})
                        response = connection.getresponse()
                        if (response.status != 200 or response.getheader('Content-Encoding', 'identity').lower() != 'identity'
                                or not response.getheader('Content-Type', '').lower().startswith('text/event-stream')):
                            raise ValueError('provider response rejected')
                        output = bytearray()
                        while True:
                            chunk = response.read1(8192)
                            if not chunk:
                                break
                            if len(chunk) > MAX_BYTES - len(output):
                                raise ValueError('provider response byte ceiling')
                            output.extend(chunk)
                        owner.response = validate_response(bytes(output), owner.by_alias)
                        owner.body = bytes(output)
                        owner.digest = hashlib.sha256(output).hexdigest()
                        self.send_response(200)
                        self.send_header('Content-Type', 'text/event-stream')
                        self.send_header('Content-Length', str(len(output)))
                        self.end_headers()
                        self.wfile.write(output)
                        self.wfile.flush()
                    finally:
                        connection.close()
                except (OSError, ValueError, TypeError, KeyError, AttributeError, RecursionError, http.client.HTTPException):
                    owner.failure = 'Provider gateway failed or rejected an unsafe request'
                    self.close_connection = True
                finally:
                    owner.completed.set()

        class Server(http.server.ThreadingHTTPServer):
            request_queue_size = 1
            daemon_threads = True
            block_on_close = False
            slots = threading.BoundedSemaphore(2)

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

            def get_request(self):
                connection, address = super().get_request()
                connection.settimeout(5)
                return connection, address

        self.server = Server(('127.0.0.1', 0), Handler)
        self.base_url = 'http://127.0.0.1:'+str(self.server.server_port)+'/v1'
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={'poll_interval':.1}, daemon=True)
        self.thread.start()

    def close(self):
        # Daemon handlers retain their slot and one-request charge through any
        # ambiguous IO. Shutdown waits only for the bounded accept loop; Hudson
        # owns the enclosing process and cancels all remaining handler work.
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=1)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
