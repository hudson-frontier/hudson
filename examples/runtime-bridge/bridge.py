"""Offline protocol fixture, not a vendor SDK adapter. No external requests.

Run as a one-request stdio bridge, or with --port 4321 as a local HTTP bridge.
Replace respond() in a trusted adapter to translate the portable model/tool
protocol into an SDK's requests and proposals. Never execute proposed tools here.
"""
import argparse
import http.server
import json
import sys

LIMIT = 1024 * 1024


def respond(body):
    if body['protocol_version'] != 1:
        raise ValueError('unsupported protocol')
    return {
        'protocol_version': 1,
        'runtime_id': body['runtime_id'],
        'runtime_version': body['runtime_version'],
        'response': {'type': 'final', 'output': {'fixture': True, 'model': body['request']['model']}},
    }


def encode(body):
    result = respond(body)
    if body.get('stream', False):
        frames = [dict(type='start', protocol_version=1, runtime_id=body['runtime_id'],
                       runtime_version=body['runtime_version']),
                  dict(type='text_delta', text='Checking execution…'),
                  dict(type='response', result=result)]
        return b''.join((json.dumps(frame, ensure_ascii=False) + '\n').encode() for frame in frames)
    return json.dumps(result).encode()


class Handler(http.server.BaseHTTPRequestHandler):
    timeout = 10

    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            length = int(self.headers.get('Content-Length', '0'))
            if not 0 < length <= LIMIT:
                raise ValueError('invalid body length')
            data = self.rfile.read(length)
            if len(data) != length:
                raise ValueError('incomplete input')
            request = json.loads(data)
            data = encode(request)
            status = 200
        except (ValueError, KeyError, TypeError):
            status, data, request = 400, b'{"error":"invalid bridge request"}', {}
        self.send_response(status)
        self.send_header('Content-Type', 'application/x-ndjson' if request.get('stream') else 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int)
    args = parser.parse_args()
    if args.port is None:
        data = sys.stdin.buffer.read(LIMIT + 1)
        if len(data) > LIMIT:
            raise SystemExit('bridge request exceeds byte limit')
        sys.stdout.buffer.write(encode(json.loads(data)))
        sys.stdout.buffer.flush()
    else:
        with http.server.HTTPServer(('127.0.0.1', args.port), Handler) as server:
            print(f'http://127.0.0.1:{server.server_port}/model', flush=True)
            server.serve_forever()
