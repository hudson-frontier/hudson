"""Offline process regressions for bounded ordinary CLI response output."""
import http.server
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import threading

if len(sys.argv) > 1 and sys.argv[1] == '--bounded-heap':
    resource.setrlimit(resource.RLIMIT_DATA, (128 * 1024 * 1024,) * 2)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    os.execv(sys.argv[2], sys.argv[2:])

LIMIT = 8 * 1024 * 1024
RUN = 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa'
requests = []
fixture = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def respond(self):
        requests.append((self.command, self.path))
        self.send_response(fixture.get('status', 200))
        self.send_header('Content-Type', 'application/json')
        if fixture.get('length', True):
            self.send_header('Content-Length', str(len(fixture['body'])))
        self.send_header('Connection', 'close')
        self.end_headers()
        try:
            self.wfile.write(fixture['body'])
        except (BrokenPipeError, ConnectionResetError):
            pass

    do_GET = respond
    do_POST = respond


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
url = f'http://127.0.0.1:{server.server_port}'

try:
    for name in ('hudson-api-fixture',):
        binary = str(Path('target/debug', name).resolve())
        for method, command in [('GET', ['health']), ('POST', ['cancel', RUN])]:
            for fixture in [
                {'body': b'{"ok":true}', 'expected': {'ok': True}},
                {'body': b'{"error":"denied"}', 'status': 403, 'expected': {'error': 'denied'}},
                {'body': b'{invalid'},
                {'body': b'"' + b'x' * (LIMIT - 1) + b'"'},
                {'body': b'"' + b'x' * (LIMIT - 1) + b'"', 'length': False},
            ]:
                requests.clear()
                result = subprocess.run([binary, '--url', url, *command], capture_output=True, timeout=20)
                assert requests == [(method, '/health' if method == 'GET' else f'/runs/{RUN}/cancel')], requests
                if 'expected' in fixture:
                    assert json.loads(result.stdout) == fixture['expected'], result.stdout
                    assert result.returncode == (0 if fixture.get('status', 200) == 200 else 1), result.stderr
                else:
                    assert result.returncode == 1 and not result.stdout, (result.returncode, result.stderr)
                    if len(fixture['body']) > LIMIT:
                        assert b'response exceeds 8 MiB' in result.stderr, result.stderr

        # The reader is already closed when the client starts, avoiding a pipe race.
        fixture = {'body': b'{"ok":true}'}
        reader, writer = os.pipe()
        os.close(reader)
        try:
            result = subprocess.run([binary, '--url', url, 'health'], stdout=writer, stderr=subprocess.PIPE, timeout=20)
        finally:
            os.close(writer)
        assert result.returncode == 1 and b'cannot flush response output' in result.stderr, result.stderr
        assert b'panicked' not in result.stderr, result.stderr

        # Small valid JSON can expand beyond the heap limit when pretty printed.
        fixture = {'body': b'[' * 100 + b'0,' * 699999 + b'0' + b']' * 100}
        requests.clear()
        with tempfile.TemporaryFile() as output:
            result = subprocess.run([sys.executable, __file__, '--bounded-heap', binary, '--url', url, 'health'],
                                    stdout=output, stderr=subprocess.PIPE, timeout=30,
                                    env={**os.environ, 'MALLOC_ARENA_MAX': '1'})
            assert result.returncode == 0 and not result.stderr, (result.returncode, result.stderr)
            assert 128 * 1024 * 1024 < output.tell() < 256 * 1024 * 1024, output.tell()
        assert requests == [('GET', '/health')], requests
    print('PASS private API fixture responses: byte limits, JSON validation, HTTP errors, single-attempt mutations, closed output, and bounded formatting heap.')
finally:
    server.shutdown()
    server.server_close()
