"""Keep authenticated loopback requests out of environment-configured proxies."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import threading

destination_requests = []
proxy_requests = []
token = 'hudson-loopback-proxy-fixture-token'


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        received = proxy_requests if self.server.is_proxy else destination_requests
        received.append((self.path, self.headers.get('Authorization')))
        body = b'{"ok":true}'
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


servers = []
try:
    for is_proxy in (False, True):
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        server.is_proxy = is_proxy
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        servers.append(server)
    destination, proxy = servers
    for name in ('hudson-cli', 'hudson'):
        binary = str(Path('target/debug', name).resolve())
        for proxy_variable in ('http_proxy', 'all_proxy'):
            for hostname in ('127.0.0.1', 'localhost'):
                environment = dict(os.environ, HUDSON_LOOPBACK_TEST_TOKEN=token)
                for variable in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'NO_PROXY',
                                 'http_proxy', 'https_proxy', 'all_proxy', 'no_proxy'):
                    environment.pop(variable, None)
                environment[proxy_variable] = f'http://127.0.0.1:{proxy.server_port}'
                destination_requests.clear()
                proxy_requests.clear()
                result = subprocess.run([binary, '--url', f'http://{hostname}:{destination.server_port}',
                                         '--api-token-env', 'HUDSON_LOOPBACK_TEST_TOKEN', 'health'],
                                        env=environment, capture_output=True, timeout=20)
                assert result.returncode == 0, result.stderr
                assert json.loads(result.stdout) == {'ok': True}, result.stdout
                assert not proxy_requests, 'authenticated loopback request reached the proxy'
                assert destination_requests == [('/health', 'Bearer ' + token)]
                assert token.encode() not in result.stdout + result.stderr
    print('PASS authenticated loopback HTTP bypasses environment proxies for both CLI entrypoints.')
finally:
    for server in servers:
        server.shutdown()
        server.server_close()
