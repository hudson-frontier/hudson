"""Compare bridge HTTP latency with the existing direct adapter, using local fixtures.

Run after cargo build --locked -p hudson-core --example bridge_benchmark.
No credentials, external endpoints or paid requests. Each mode executes 100
sequential calls with the same 1 KiB text, 32-token cap and constant result.
"""
import http.server
import json
import pathlib
import socket
import subprocess
import threading


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers['Content-Length'])
        assert 0 < length < 65536
        request = json.loads(self.rfile.read(length))
        if self.path == '/bridge':
            assert request['max_output_tokens'] == 32
            response = {'protocol_version': 1, 'runtime_id': 'benchmark', 'runtime_version': 1,
                        'response': {'type': 'final', 'output': 42}}
        else:
            assert request['max_completion_tokens'] == 32
            response = {'choices': [{'finish_reason': 'stop', 'message': {'content': '42'}}]}
        data = json.dumps(response).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == '__main__':
    root = pathlib.Path(__file__).resolve().parent.parent
    server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        for mode in ('direct', 'bridge'):
            result = subprocess.run([str(root/'target/debug/examples/bridge_benchmark'), mode,
                                     f'http://127.0.0.1:{server.server_port}/{mode}', '100'],
                                    capture_output=True, text=True, check=True, timeout=30)
            print(result.stdout.strip(), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
