"""Local JSON file and explicit-stdin regressions for the private API fixture."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading

requests = []
value = {'operator_secret': 'json-file-fixture-secret'}
raw = json.dumps(value).encode()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        requests.append((self.path, json.loads(self.rfile.read(int(self.headers['Content-Length'])))))
        body = b'{"ok":true}'
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
server.daemon_threads = True
threading.Thread(target=server.serve_forever, daemon=True).start()
environment = os.environ.copy()
for name in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'):
    environment.pop(name, None)
try:
    with tempfile.TemporaryDirectory(prefix='hudson-api-fixture-json-') as directory:
        protected = Path(directory, 'protected.json')
        protected.write_bytes(raw)
        protected.chmod(0o600)
        link = Path(directory, 'input.json')
        link.symlink_to(protected)
        fifo = Path(directory, 'fifo.json')
        os.mkfifo(fifo)
        for name in ('hudson-api-fixture',):
            binary = str(Path('target/debug', name).resolve())
            for source, allowed in ((link, False), (fifo, False), (protected, True), ('-', True)):
                for action, path in (('publish-tool', '/tools'), ('publish-agent', '/agents'), ('start', '/runs')):
                    command = [action, str(source)] if action != 'start' else [
                        'start', '--input-file', str(source), '--request-key', 'json-file-test-key']
                    requests.clear()
                    result = subprocess.run([binary, '--url', f'http://127.0.0.1:{server.server_port}', *command],
                                            input=raw if source == '-' else None, env=environment,
                                            capture_output=True, timeout=5)
                    assert result.returncode == (0 if allowed else 1), (name, str(source), action, result.returncode, len(requests), result.stderr)
                    if allowed:
                        body = value if action != 'start' else {'input': value, 'request_key': 'json-file-test-key'}
                        assert requests == [(path, body)], requests
                        assert json.loads(result.stdout) == {'ok': True}
                    else:
                        assert not requests and not result.stdout
                        assert b'json-file-fixture-secret' not in result.stderr
    print('PASS JSON inputs: no symlink/special-file requests, bounded FIFO rejection, regular files and explicit stdin for the private API fixture.')
finally:
    server.shutdown()
    server.server_close()
