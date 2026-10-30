"""Compare bridge HTTP latency with the existing direct adapter, using local fixtures.

Run after cargo build --locked -p hudson-core --example bridge_benchmark.
No credentials, external endpoints or paid requests. Text modes execute 100
sequential calls with 1 KiB text, a 32-token cap and constant result. Image mode
executes 25 calls adding 2 MiB minus two synthetic decoded bytes. Those bytes
exercise transport/allocation behavior; this is not a vision acceptance test.
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
        assert 0 < length < (3 * 1024 * 1024 if self.path == '/image' else 65536)
        request = json.loads(self.rfile.read(length))
        if self.path in ('/bridge', '/stream'):
            assert request['max_output_tokens'] == 32
            response = {'protocol_version': 1, 'runtime_id': 'benchmark', 'runtime_version': 1,
                        'response': {'type': 'final', 'output': 42}}
        else:
            assert request['max_completion_tokens'] == 32
            if self.path == '/image':
                content = request['messages'][1]['content']
                assert content[1]['image_url']['url'].startswith('data:image/png;base64,')
                assert len(content[1]['image_url']['url']) == 22 + 2796200
            response = {'choices': [{'finish_reason': 'stop', 'message': {'content': '42'}}]}
        if self.path == '/stream':
            assert request['stream'] is True
            frames = [{'type': 'start', 'protocol_version': 1, 'runtime_id': 'benchmark', 'runtime_version': 1},
                      {'type': 'text_delta', 'text': '42'}, {'type': 'response', 'result': response}]
            data = b''.join((json.dumps(frame) + '\n').encode() for frame in frames)
        else:
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
        for mode in ('direct', 'bridge', 'stream', 'image'):
            result = subprocess.run([str(root/'target/debug/examples/bridge_benchmark'), mode,
                                     f'http://127.0.0.1:{server.server_port}/{mode}', '25' if mode == 'image' else '100'],
                                    capture_output=True, text=True, check=True, timeout=90)
            print(result.stdout.strip(), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
