"""Non-Rust HTTP client: durable approval/restart and controls during model IO.

Uses only loopback model/tool stubs and a dedicated local PostgreSQL namespace.
Run after cargo build -p hudson-server -p hudson-api-fixture.
"""
import http.server
import json
import os
import pathlib
import re
import subprocess
import tempfile
import threading
import time
import urllib.request
import urllib.error
import uuid

from operator_cli import command as operator_command

executed = []
model_calls = []
entered = threading.Event()
release = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if self.path != '/tool':
            model_calls.append(request)
        if self.path == '/tool':
            executed.append(self.headers['Idempotency-Key'])
            response = {'saved': True}
        elif any('Hold this call' in str(message.get('content')) for message in request['messages']):
            entered.set()
            assert release.wait(15), 'model was not released'
            response = {'choices': [{'finish_reason': 'stop', 'message': {'content': 'done'}}]}
        elif request['messages'][-1]['role'] == 'tool':
            response = {'choices': [{'finish_reason': 'stop', 'message': {'content': 'saved'}}]}
        else:
            response = {'choices': [{'finish_reason': 'tool_calls', 'message': {'tool_calls': [
                {'id': 'write-1', 'type': 'function', 'function': {'name': 'save', 'arguments': '{"value":42}'}}
            ]}}]}
        if self.path != '/tool':
            response['usage'] = {'prompt_tokens': 10, 'completion_tokens': 2}
        body = json.dumps(response).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


stub = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=stub.serve_forever, daemon=True)
thread.start()
process = None
try:
    with tempfile.TemporaryDirectory() as directory:
        config = pathlib.Path(directory) / 'agent.json'
        base = f'http://127.0.0.1:{stub.server_port}'
        config.write_text(json.dumps({
            'name': 'writer', 'instructions': 'Save the supplied value', 'endpoint': base + '/model',
            'input_schema': {'oneOf': [{'type':'string'}, {'type':'object', 'required':['operation','value'],
                              'additionalProperties':False, 'properties':{'operation':{'const':'save'},'value':{'type':'number'}}}]},
            'http_tools': [{'name': 'save', 'description': 'Save a value', 'endpoint': base + '/tool',
                            'input_schema': {'type': 'object'}, 'effect': 'write', 'require_approval': True}]
        }))
        command = ['target/debug/hudson-server', '--config', str(config), '--port', '0', '--database',
                   os.environ.get('HUDSON_TEST_DATABASE', 'hudson_harness_test_20260921'),
                   '--namespace', 'api-' + str(uuid.uuid4())]
        env = os.environ.copy()
        env.pop('OPENAI_API_KEY', None)

        def start():
            global process
            process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            line = process.stderr.readline()
            match = re.search(r'http://127.0.0.1:\d+', line)
            assert match, line + (process.stderr.read() if process.poll() is not None else '')
            return match.group()

        api = start()

        def request(method, path, body=None):
            data = json.dumps(body).encode() if body is not None else None
            req = urllib.request.Request(api + path, data=data, method=method,
                                         headers={'Content-Type': 'application/json'})
            with urllib.request.urlopen(req, timeout=3) as response:
                return json.load(response)

        def until(run, status):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                view = request('GET', '/runs/' + run)
                if view['status'] == status:
                    return view
                time.sleep(.02)
            raise AssertionError(view)

        assert request('GET', '/health') == {'mode': 'configured', 'durable': True}
        def cli(*arguments, stdin=None, success=True):
            result = subprocess.run([*operator_command(), '--url', api, *arguments],
                                    input=stdin, env=env, capture_output=True, text=True, timeout=10)
            if not success:
                assert result.returncode != 0, 'invalid CLI input was accepted'
                return
            assert result.returncode == 0, result.stderr
            return json.loads(result.stdout)

        for invalid in [None, {'operation':'save','value':'not a number'}, {'operation':'erase','value':42}]:
            try:
                request('POST', '/runs', {'input':invalid, 'request_key':'save-42'})
                raise AssertionError('invalid input accepted')
            except urllib.error.HTTPError as error:
                assert error.code == 400
        assert not model_calls and not executed, 'invalid task dispatched work'
        body = {'input': {'operation': 'save', 'value': 42}, 'request_key': 'save-42'}
        input_file = pathlib.Path(directory) / 'input.json'
        input_file.write_text(json.dumps(body['input']))
        run = cli('start', '--input-file', str(input_file), '--request-key', 'save-42')['run_id']
        assert cli('start', '--input-file', '-', '--request-key', 'save-42', stdin=json.dumps(body['input']))['run_id'] == run
        cli('start', '--input-file', '-', stdin='{broken', success=False)
        cli('start', '--input-file', '-', stdin='x' * (64 * 1024 + 1), success=False)
        cli('start', '--task', 'ambiguous', '--input-file', str(input_file), success=False)
        cli('start', '--refund', success=False)
        # Each request closes its connection: execution is independent of the submitting client.
        waiting = until(run, 'waiting')
        assert not executed
        assert waiting['usage']['reported_model_calls'] == 1
        operation = waiting['wait']['operation_id']
        assert request('GET', '/operations/' + operation)['arguments'] == {'value': 42}
        process.terminate()
        process.wait(timeout=5)
        api = start()
        assert request('GET', '/runs/' + run)['status'] == 'waiting'
        request('POST', '/operations/' + operation + '/approval', {'approved': True})
        assert until(run, 'completed')['result'] == 'saved'
        assert executed == [operation]
        assert request('POST', '/runs', body)['run_id'] == run
        assert cli('resume', run)['run_id'] == run
        completed = cli('get', run)
        assert completed['status'] == 'completed'
        assert completed['usage']['reported_model_calls'] == 2
        assert completed['usage']['reported_input_tokens'] == 20
        assert completed['usage']['reported_output_tokens'] == 4
        assert cli('events', run)
        assert request('GET', '/runs/' + run + '/events')
        assert executed == [operation]

        blocked = cli('start', '--task', 'Hold this call')['run_id']
        assert entered.wait(5)
        assert request('GET', '/runs/' + blocked)['status'] == 'running'
        request('POST', '/runs/' + blocked + '/cancel')
        assert request('GET', '/runs/' + blocked)['status'] == 'cancelling'
        release.set()
        until(blocked, 'cancelled')
        print('PASS: generic CLI text/file/stdin inputs, invalid input rejection, HTTP client, detached execution, PostgreSQL approval restart, one write, idempotent resume, inspection/cancellation during model IO')
finally:
    release.set()
    if process is not None and process.poll() is None:
        process.terminate()
        process.wait(timeout=5)
    stub.shutdown()
    stub.server_close()
    thread.join()
