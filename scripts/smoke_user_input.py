"""Offline user clarification, durable restart, correlated reply and idempotency."""
import http.server
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import urllib.error
import uuid
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / 'integrations/python'))
from hudson_client import HudsonClient, HttpError
calls = []
failures = []
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        try:
            calls.append(body)
            assert any(t['function']['name'] == 'ask_user' for t in body['tools'])
            if len(calls) == 1:
                response = {'choices':[{'finish_reason':'tool_calls','message':{'tool_calls':[{'id':'question-1','type':'function','function':{'name':'ask_user','arguments':'{"prompt":"Which city?"}'}}]}}]}
            else:
                assert len(calls) == 2
                message = body['messages'][-1]
                assert message['role'] == 'tool' and message['tool_call_id'] == 'question-1'
                assert json.loads(message['content'])['value'] == 'Boston'
                response = {'choices':[{'finish_reason':'stop','message':{'content':'{"city":"Boston"}'}}]}
            self.send_response(200)
        except Exception as error:
            failures.append(repr(error)); self.send_response(500); response = {}
        data = json.dumps(response).encode()
        self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)
stub = http.server.HTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=stub.serve_forever, daemon=True)
thread.start()
process = None
try:
    with tempfile.TemporaryDirectory() as directory:
        base = f'http://127.0.0.1:{stub.server_port}'
        config = pathlib.Path(directory) / 'team.json'
        config.write_text(json.dumps({'name':'clarifier','instructions':'Ask for the city, then return it.',
            'model':'stub','endpoint':base + '/model','allow_user_input':True,
            'output_schema':{'const':{'city':'Boston'}}}))
        database = os.environ.get('HUDSON_TEST_DATABASE', 'hudson_harness_test_20260921')
        namespace = 'user-input-' + str(uuid.uuid4())
        common = ['--config', str(config), '--database', database, '--namespace', namespace]
        env = os.environ.copy()
        env.pop('OPENAI_API_KEY', None)
        env.pop('ANTHROPIC_API_KEY', None)

        def start():
            global process
            process = subprocess.Popen(['target/debug/hudson-server'] + common + ['--port', '0'], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            line = process.stderr.readline()
            match = re.search(r'http://127.0.0.1:\d+', line)
            assert match, line
            return match.group()

        api = start()

        def request(method, path, body=None):
            data = json.dumps(body).encode() if body is not None else None
            req = urllib.request.Request(api + path, data=data, method=method, headers={'Content-Type': 'application/json'})
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

        run = request('POST', '/runs', {'input':'Plan a trip'})['run_id']
        waiting = until(run, 'waiting')
        assert waiting['wait']['type'] == 'user_input' and waiting['wait']['prompt'] == 'Which city?'
        question_id = waiting['wait']['question_id']
        assert question_id > 0
        observed_wait = list(HudsonClient(api).follow(run))[-1]['run']['wait']
        assert observed_wait == waiting['wait']
        assert len(calls) == 1
        process.terminate(); process.wait(timeout=5)
        api = start()
        assert request('GET', '/runs/' + run)['wait'] == waiting['wait']
        # Zero is a valid wire identity; the server decides whether it matches
        # this pending question, rather than the client rejecting it locally.
        try:
            HudsonClient(api).reply(run, 'stale answer', request_key='zero-question', question_id=0)
            raise AssertionError('stale zero question accepted')
        except HttpError as error:
            assert error.status == 409
        try:
            request('POST', '/runs/' + run + '/input', {'input':'stale answer','request_key':'stale','question_id':question_id + 1})
            raise AssertionError('wrong question accepted')
        except urllib.error.HTTPError as error:
            assert error.code == 409
        assert request('GET', '/runs/' + run)['wait'] == waiting['wait']
        assert len(calls) == 1
        reply = ['target/debug/hudson-api-fixture','--url',api,'reply',run,'--text','Boston','--request-key','answer-1','--question-id',str(question_id)]
        subprocess.run(reply, check=True, capture_output=True, text=True)
        complete = until(run, 'completed')
        assert complete['result'] == {'city':'Boston'}
        subprocess.run(reply, check=True, capture_output=True, text=True)
        HudsonClient(api).reply(run, 'Boston', request_key='answer-1', question_id=question_id)
        try:
            request('POST', '/runs/' + run + '/input', {'input':'Paris','request_key':'answer-1','question_id':question_id})
            raise AssertionError('changed reply accepted')
        except urllib.error.HTTPError as error:
            assert error.code == 409
        events = request('GET', '/runs/' + run + '/events')
        assert len([event for event in events if event['event_type'] == 'input_received']) == 1
        assert len(calls) == 2 and not failures, failures
        # The worker can record replies without constructing provider clients.
        process.terminate(); process.wait(timeout=5)
        for reply_option in ('text', 'file'):
            calls.clear()
            started = subprocess.run(['target/debug/hudson-worker'] + common + ['--task', 'Plan a trip'], env=env, check=True, capture_output=True, text=True)
            view = json.loads(started.stdout)
            assert view['status'] == 'waiting' and len(calls) == 1
            reply_args = ['--reply-text', 'Boston']
            if reply_option == 'file':
                reply_path = pathlib.Path(directory) / 'reply.json'
                reply_path.write_text(json.dumps('Boston'))
                reply_args = ['--reply-file', str(reply_path)]
            record = ['target/debug/hudson-worker', '--database', database, '--namespace', namespace,
                      '--reply-run', view['id'], '--request-key', 'worker-answer', '--question-id', str(view['wait']['question_id'])] + reply_args
            # An unusable config is deliberately ignored by management commands.
            saved = subprocess.run(record + ['--config', '/no/such/config.json'], env=env, check=True, capture_output=True, text=True)
            assert json.loads(saved.stdout)['status'] == 'running'
            assert len(calls) == 1
            subprocess.run(record, env=env, check=True, capture_output=True, text=True)
            assert len(calls) == 1
            resumed = subprocess.run(['target/debug/hudson-worker'] + common + ['--resume', view['id']], env=env, check=True, capture_output=True, text=True)
            assert json.loads(resumed.stdout)['result'] == {'city': 'Boston'}
            assert len(calls) == 2 and not failures, failures
        print('PASS: optional ask_user, PostgreSQL restart, API/CLI reply, worker text/JSON reply without execution, explicit resume, correlated continuation and idempotency')
finally:
    if process is not None and process.poll() is None:
        process.terminate(); process.wait(timeout=5)
    stub.shutdown(); stub.server_close()
