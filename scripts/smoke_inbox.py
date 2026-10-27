"""Offline queue controls across a live provider boundary, question, and restart."""
import http.server
import json
import os
import pathlib
import re
import select
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / 'integrations/python'))
from hudson_client import HudsonClient, HttpError

requests = []
tool_calls = []
failures = []
entered = threading.Event()
release = threading.Event()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        try:
            if self.path == '/tool':
                tool_calls.append(body)
                assert body == {'key': 'report'}
                response = {'source': 'saved', 'evidence': 'x' * 9000}
            else:
                assert self.path == '/model'
                assert body['reasoning_effort'] == 'low'
                requests.append(body)
                count = len(requests)
                if count == 1:
                    entered.set()
                    assert release.wait(60), 'queue setup did not finish'
                    message = {'tool_calls': [{'id': 'lookup-1', 'type': 'function',
                        'function': {'name': 'lookup', 'arguments': '{"key":"report"}'}}]}
                elif count == 2:
                    assert body['messages'][-1] == {'role': 'user', 'content': 'Use the latest report'}
                    assert body['messages'][-2]['role'] == 'tool'
                    assert 'context_artifact' in body['messages'][-2]['content']
                    message = {'tool_calls': [{'id': 'question-1', 'type': 'function',
                        'function': {'name': 'ask_user', 'arguments': '{"prompt":"Which format?"}'}}]}
                elif count == 3:
                    assert body['messages'][-1]['role'] == 'tool'
                    assert json.loads(body['messages'][-1]['content'])['value'] == 'JSON'
                    message = {'content': '{"report":"ready"}'}
                else:
                    assert count == 4
                    assert body['messages'][-1] == {'role': 'user', 'content': 'Add a summary'}
                    assert body['messages'][-2]['role'] == 'assistant'
                    message = {'content': '{"report":"ready","summary":"complete"}'}
                response = {'choices': [{'finish_reason': 'tool_calls' if 'tool_calls' in message else 'stop',
                                         'message': message}],
                            'usage': {'prompt_tokens': 10, 'completion_tokens': 4}}
            status = 200
        except Exception as error:
            failures.append(repr(error))
            status, response = 500, {}
        data = json.dumps(response).encode()
        self.send_response(status)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


stub = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=stub.serve_forever, daemon=True)
thread.start()
process = None
try:
    with tempfile.TemporaryDirectory() as directory:
        config = pathlib.Path(directory) / 'agent.json'
        origin = f'http://127.0.0.1:{stub.server_port}'
        config.write_text(json.dumps({
            'name': 'queue-fixture', 'instructions': 'Use tools, ask for format, then produce the report.',
            'model': 'local-fixture', 'endpoint': origin + '/model', 'allow_user_input': True,
            'reasoning': {'type': 'effort', 'effort': 'low'},
            'context': {'offload_bytes': 4096, 'recent_exchanges': 1, 'excerpt_bytes': 256,
                        'page_bytes': 512, 'reserve_bytes': 1024},
            'limits': {'max_context_bytes': 16384, 'max_model_calls': 4},
            'output_schema': {'type': 'object', 'properties': {'report': {'const': 'ready'},
                              'summary': {'type': 'string'}}, 'required': ['report']},
            'http_tools': [{'name': 'lookup', 'description': 'Read the report', 'endpoint': origin + '/tool',
                'effect': 'read', 'input_schema': {'type': 'object', 'properties': {'key': {'type': 'string'}},
                                                 'required': ['key'], 'additionalProperties': False}}],
        }), encoding='utf-8')
        database = os.environ['HUDSON_TEST_DATABASE']
        namespace = 'inbox-' + str(uuid.uuid4())
        env = os.environ.copy()
        for name in ('OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY'):
            env.pop(name, None)

        def start():
            global process
            process = subprocess.Popen(['target/debug/hudson-server', '--config', str(config),
                '--database', database, '--namespace', namespace, '--port', '0'],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            assert select.select([process.stderr], [], [], 15)[0], 'API startup timed out'
            line = process.stderr.readline()
            found = re.search(r'http://127.0.0.1:\d+', line)
            assert found, line
            return found.group()

        def cli(api, *arguments):
            result = subprocess.run(['target/debug/hudson-api-fixture', '--url', api, *arguments],
                                    check=True, capture_output=True, text=True, timeout=15)
            return json.loads(result.stdout)

        def until(client, run, expected):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                view = client.run(run)
                if view['status'] == expected:
                    return view
                assert view['status'] != 'failed', (view, failures)
                time.sleep(.05)
            raise AssertionError((view, failures))

        api = start()
        client = HudsonClient(api)
        run = client.start('Find the report', request_key='task')['run_id']
        assert entered.wait(15), failures
        withdrawn = client.queue_message(run, 'discarded marker', kind='steer', request_key='withdrawn')
        assert len(cli(api, 'inbox', run)) == 1
        assert cli(api, 'clear-inbox', run) == {'cleared': 1}
        assert client.queue_message(run, 'discarded marker', kind='steer', request_key='withdrawn') == withdrawn
        assert client.queued_messages(run) == []
        follow = client.queue_message(run, 'Add a summary', kind='follow_up', request_key='follow')
        steer = cli(api, 'steer', run, '--text', 'Use the latest report', '--request-key', 'direction')
        assert cli(api, 'steer', run, '--text', 'Use the latest report', '--request-key', 'direction') == steer
        release.set()
        waiting = until(client, run, 'waiting')
        assert waiting['wait']['type'] == 'user_input'
        assert [item['receipt'] for item in client.queued_messages(run)] == [follow]
        assert len(requests) == 2 and len(tool_calls) == 1 and not failures, failures
        process.terminate()
        process.wait(timeout=5)
        api = start()
        client = HudsonClient(api)
        assert client.run(run)['wait'] == waiting['wait']
        assert [item['receipt'] for item in client.queued_messages(run)] == [follow]
        try:
            client.queue_message(run, 'changed', kind='follow_up', request_key='follow')
            raise AssertionError('changed message key accepted')
        except HttpError as error:
            assert error.status == 409
        client.reply(run, 'JSON', request_key='format', question_id=waiting['wait']['question_id'])
        complete = until(client, run, 'completed')
        assert complete['result'] == {'report': 'ready', 'summary': 'complete'}
        assert complete['usage']['model_calls'] == 4
        assert len(requests) == 4 and len(tool_calls) == 1 and not failures, failures
        assert 'discarded marker' not in json.dumps(requests)
        assert client.queue_message(run, 'Add a summary', kind='follow_up', request_key='follow') == follow
        events = client.events(run, limit=1000)
        assert sum(event['event_type'] == 'input_consumed' for event in events) == 2
        assert sum(event['event_type'] == 'input_queue_cleared' for event in events) == 1
        print('PASS: queued steering and follow-up, CLI withdrawal, retained receipts, complete tool boundaries, context offload, question restart, exact model budget and one tool effect')
finally:
    release.set()
    if process is not None and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
    stub.shutdown()
    stub.server_close()
    thread.join(timeout=5)
