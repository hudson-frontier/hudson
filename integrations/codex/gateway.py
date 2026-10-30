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
    def __init__(self, endpoint, credential, model, tools, max_output_tokens):
        url = urllib.parse.urlsplit(endpoint)
        local = url.hostname in ('localhost', '127.0.0.1', '::1')
        if ((url.scheme != 'https' and not (url.scheme == 'http' and local)) or not url.hostname
                or url.username is not None or url.password is not None or url.query or url.fragment
                or not url.path or not credential or len(credential) > 8192
                or '\r' in credential or '\n' in credential
                or type(max_output_tokens) is not int or not 0 < max_output_tokens <= 2**32-1):
            raise ValueError('invalid provider gateway configuration')
        self.url, self.credential = url, credential
        self.model, self.tools, self.max_output_tokens = model, tools, max_output_tokens
        self.token = secrets.token_urlsafe(32)
        self.used = False
        self.lock = threading.Lock()
        self.completed = threading.Event()
        self.failure = None
        self.body = None
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
                if not hmac.compare_digest(self.headers.get('Authorization', ''), 'Bearer '+owner.token):
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
                        self.send_response(200)
                        self.send_header('Content-Type', 'text/event-stream')
                        self.end_headers()
                        output = bytearray()
                        while True:
                            chunk = response.read1(8192)
                            if not chunk:
                                break
                            if len(chunk) > MAX_BYTES - len(output):
                                raise ValueError('provider response byte ceiling')
                            output.extend(chunk)
                            self.wfile.write(chunk)
                            self.wfile.flush()
                        owner.body = bytes(output)
                        owner.digest = hashlib.sha256(output).hexdigest()
                    finally:
                        connection.close()
                except (OSError, ValueError, RecursionError, http.client.HTTPException):
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
