"""Offline CLI follow checks: paging, reconnect, waits, interruption and safe errors."""
import http.server
import json
import os
import pathlib
import select
import signal
import subprocess
import threading
import time
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent
BINARY = pathlib.Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')) / 'debug/hudson-cli'
RUN = '00000000-0000-0000-0000-000000000018'
TOKEN = 'follow-fixture-secret'
ENV = dict(os.environ, HUDSON_FOLLOW_FIXTURE_TOKEN=TOKEN)


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        fixture = self.server.fixture
        fixture['calls'].append(self.path)
        assert self.headers.get('Authorization') == 'Bearer ' + TOKEN
        path = urllib.parse.urlparse(self.path)
        if fixture.get('error'):
            self.send_response(fixture['error'])
            self.end_headers()
            self.wfile.write(TOKEN.encode())  # Error bodies must never be echoed.
            return
        if fixture.get('malformed'):
            body = b'not JSON'
        elif path.path.endswith('/events'):
            if fixture.get('outages', 0):
                fixture['outages'] -= 1
                self.send_response(503)
                self.end_headers()
                return
            query = urllib.parse.parse_qs(path.query)
            after = int(query['after'][0])
            limit = int(query['limit'][0])
            selected = [event for event in fixture['events'] if event['sequence'] > after][:limit]
            body = json.dumps(selected).encode()
        else:
            statuses = fixture.get('statuses', ['completed'])
            status = statuses.pop(0) if len(statuses) > 1 else statuses[0]
            if fixture.get('grow'):
                fixture['events'] = events({'waiting': 1, 'running': 2, 'completed': 3}[status])
            body = json.dumps({'id': RUN, 'status': status,
                               'wait': {'type': 'user_input', 'question_id': 3, 'prompt': 'Which record?'} if status == 'waiting' else None,
                               'result': {'answer': 42} if status == 'completed' else None}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        raise AssertionError('following must never mutate a run')


def events(count):
    return [{'sequence': index, 'run_id': RUN, 'event_type': 'operation_settled', 'payload': {'index': index}}
            for index in range(1, count + 1)]


def command(port, *extra):
    return [str(BINARY), '--url', f'http://127.0.0.1:{port}', '--api-token-env', 'HUDSON_FOLLOW_FIXTURE_TOKEN',
            'follow', RUN, '--page-size', '2', '--poll-ms', '50', '--max-retries', '3', *extra]


def invoke(server, *extra):
    result = subprocess.run(command(server.server_port, *extra), env=ENV, text=True, capture_output=True, timeout=10)
    assert TOKEN not in result.stdout + result.stderr, result
    return result


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
server.fixture = {'events': events(7), 'calls': [], 'outages': 1}
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    # A terminal run still drains every page, including its final partial page.
    result = invoke(server)
    assert result.returncode == 0, result.stderr
    records = [json.loads(line) for line in result.stdout.splitlines()]
    assert [record['after'] for record in records if record['type'] == 'event'] == list(range(1, 8))
    assert records[-1]['type'] == 'run' and records[-1]['after'] == 7
    assert records[-1]['run']['result'] == {'answer': 42}
    assert 'after=6&limit=2' in ' '.join(server.fixture['calls'])
    assert 'after=7&limit=2' in ' '.join(server.fixture['calls'])
    assert 'retrying unavailable server' in result.stderr

    # Waiting remains observable; the watcher never supplies an answer or resumes.
    server.fixture = {'events': events(1), 'calls': [], 'statuses': ['waiting', 'running', 'completed'], 'grow': True}
    result = invoke(server, '--after', '1')
    assert result.returncode == 0, result.stderr
    records = [json.loads(line) for line in result.stdout.splitlines()]
    assert [record['after'] for record in records if record['type'] == 'event'] == [2, 3]
    assert [record['run']['status'] for record in records if record['type'] == 'run'] == ['waiting', 'running', 'completed']

    # SIGINT ends observation, not the run. Resume from the last processed cursor.
    server.fixture = {'events': events(3), 'calls': [], 'statuses': ['waiting']}
    process = subprocess.Popen(command(server.server_port), env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        ready, _, _ = select.select([process.stdout], [], [], 5)
        assert ready, 'follow did not flush its output'
        first = json.loads(process.stdout.readline())
        assert first['type'] == 'event' and first['after'] == 1
        process.send_signal(signal.SIGINT)
        stdout, stderr = process.communicate(timeout=5)
        assert process.returncode != 0
        assert TOKEN not in stdout + stderr
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
    server.fixture['statuses'] = ['completed']
    result = invoke(server, '--after', str(first['after']))
    assert result.returncode == 0
    assert [json.loads(line)['after'] for line in result.stdout.splitlines() if json.loads(line)['type'] == 'event'] == [2, 3]

    for status in ['failed', 'cancelled']:
        server.fixture = {'events': events(1), 'calls': [], 'statuses': [status]}
        result = invoke(server)
        assert result.returncode != 0 and f'run {status}' in result.stderr
        assert 'reconnect with --after 1' in result.stderr
        assert json.loads(result.stdout.splitlines()[-1])['run']['status'] == status

    for error in [401, 404, 302]:
        server.fixture = {'events': [], 'calls': [], 'error': error}
        result = invoke(server)
        assert result.returncode != 0 and str(error) in result.stderr
        assert len(server.fixture['calls']) == 1  # Never retry auth or follow a redirect.
    server.fixture = {'events': [], 'calls': [], 'error': 503}
    result = invoke(server)
    assert result.returncode != 0 and 'retry limit reached' in result.stderr
    assert len(server.fixture['calls']) == 4
    server.fixture = {'events': [], 'calls': [], 'malformed': True}
    result = invoke(server)
    assert result.returncode != 0 and 'invalid JSON' in result.stderr
    assert len(server.fixture['calls']) == 1

    # Losing an actual listening socket is retried without exposing transport secrets.
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
    port = server.server_port
    process = subprocess.Popen(command(port), env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    time.sleep(0.12)
    replacement = http.server.ThreadingHTTPServer(('127.0.0.1', port), Fixture)
    replacement.fixture = {'events': events(2), 'calls': []}
    replacement_thread = threading.Thread(target=replacement.serve_forever, daemon=True)
    replacement_thread.start()
    try:
        stdout, stderr = process.communicate(timeout=10)
        assert process.returncode == 0, stderr
        assert [json.loads(line)['after'] for line in stdout.splitlines() if json.loads(line)['type'] == 'event'] == [1, 2]
        assert 'retrying unavailable server' in stderr
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        replacement.shutdown()
        replacement.server_close()
        replacement_thread.join(timeout=5)
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)

print('PASS: CLI follow pages, terminal draining, waits, stable cursor reconnect, socket outage, SIGINT without mutation, bounded retries, malformed responses and auth/redirect secrecy')
