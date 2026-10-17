"""Exercise each configured provider through a complete real-loop tool cycle.

Loopback protocol stubs assert auth, token caps, tool arguments and continuation
metadata. This checks our adapters, not availability of real provider models.
"""
import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading

requests = {}
executed = {}
failures = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            provider, kind = self.path.strip('/').split('/')
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            if kind == 'tool':
                assert body == {'left': 17, 'right': 25}
                assert self.headers['Idempotency-Key']
                executed.setdefault(provider, []).append(self.headers['Idempotency-Key'])
                response = {'answer': 42}
            else:
                calls = requests.setdefault(provider, [])
                calls.append(body)
                assert len(calls) <= 2, 'unexpected model retry'
                assert body['model'] == 'local-test-model'
                if provider == 'anthropic':
                    assert self.headers['x-api-key'] == 'offline-test-key'
                    assert self.headers['anthropic-version'] == '2023-06-01'
                    assert body['max_tokens'] == 128
                    assert body['tools'][0]['name'] == 'sum'
                    assert body['tools'][0]['input_schema']['required'] == ['left', 'right']
                    blocks = [
                        {'type': 'thinking', 'thinking': 'test continuation', 'signature': 'signed-test'},
                        {'type': 'text', 'text': 'I will add the numbers.'},
                        {'type': 'tool_use', 'id': 'sum-1', 'name': 'sum', 'input': {'left': 17, 'right': 25}},
                    ]
                    if len(calls) == 1:
                        response = {'stop_reason': 'tool_use', 'content': blocks}
                    else:
                        assert body['messages'][-2]['role'] == 'assistant'
                        assert body['messages'][-2]['content'] == blocks
                        result = body['messages'][-1]['content'][0]
                        assert result['type'] == 'tool_result' and result['tool_use_id'] == 'sum-1'
                        assert result['is_error'] is False
                        assert json.loads(result['content'])['value'] == {'answer': 42}
                        response = {'stop_reason': 'end_turn', 'content': [{'type': 'text', 'text': '{"answer":42}'}]}
                else:
                    assert self.headers['Authorization'] == 'Bearer offline-test-key'
                    token_field = 'max_completion_tokens' if provider == 'openai' else 'max_tokens'
                    assert body[token_field] == 128
                    assert body['tools'][0]['function']['name'] == 'sum'
                    call = {'id': 'sum-1', 'type': 'function',
                            'function': {'name': 'sum', 'arguments': '{"left":17,"right":25}'},
                            'extra_content': {'google': {'thought_signature': 'signed-test'}}}
                    if len(calls) == 1:
                        response = {'choices': [{'finish_reason': 'tool_calls', 'message': {
                            'content': 'I will add the numbers.', 'tool_calls': [call]}}]}
                    else:
                        assert body['messages'][-2]['content'] == 'I will add the numbers.'
                        previous = body['messages'][-2]['tool_calls'][0]
                        assert 'chat_assistant_text' not in previous
                        assert previous['extra_content'] == call['extra_content']
                        assert json.loads(previous['function']['arguments']) == {'left': 17, 'right': 25}
                        result = body['messages'][-1]
                        assert result['role'] == 'tool' and result['tool_call_id'] == 'sum-1'
                        assert json.loads(result['content'])['value'] == {'answer': 42}
                        response = {'choices': [{'finish_reason': 'stop', 'message': {'content': '{"answer":42}'}}]}
            if kind == 'model':
                response['usage'] = ({'input_tokens': 10, 'cache_read_input_tokens': 3,
                                      'cache_creation_input_tokens': 2, 'output_tokens': 4}
                                     if provider == 'anthropic' else {'prompt_tokens': 15, 'completion_tokens': 4})
            data = json.dumps(response).encode()
            self.send_response(200)
        except Exception as error:
            failures.append(repr(error))
            data = b'{}'
            self.send_response(500)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


stub = http.server.HTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=stub.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory() as directory:
        config = pathlib.Path(directory) / 'agent.json'
        env = os.environ.copy()
        env['HUDSON_OFFLINE_TEST_KEY'] = 'offline-test-key'
        for provider in ['openai', 'anthropic', 'gemini', 'ollama']:
            base = f'http://127.0.0.1:{stub.server_port}/{provider}'
            config.write_text(json.dumps({
                'name': 'calculator', 'instructions': 'Use sum to add the supplied numbers.',
                'provider': provider, 'model': 'local-test-model', 'endpoint': base + '/model',
                'api_key_env': 'HUDSON_OFFLINE_TEST_KEY', 'max_output_tokens': 128,
                'limits': {'max_model_calls': 2}, 'output_schema': {'const': {'answer': 42}},
                'http_tools': [{'name': 'sum', 'description': 'Add two numbers', 'endpoint': base + '/tool',
                                'effect': 'read', 'input_schema': {'type': 'object', 'required': ['left', 'right'],
                                                                 'properties': {'left': {'type': 'number'}, 'right': {'type': 'number'}}},
                                'output_schema': {'const': {'answer': 42}}}]
            }))
            run = subprocess.run(['target/debug/hudson-worker', '--config', str(config), '--task', 'Add 17 and 25'],
                                 env=env, capture_output=True, text=True, timeout=15)
            assert not failures, failures
            assert run.returncode == 0, run.stderr + run.stdout
            result = json.loads(run.stdout)
            assert result['status'] == 'completed' and result['result'] == {'answer': 42}
            assert result['usage']['model_calls'] == 2 and result['usage']['tool_calls'] == 1
            assert result['assessment']['passed']
            assert result['usage']['reported_model_calls'] == 2
            assert result['usage']['reported_input_tokens'] == 30
            assert result['usage']['reported_output_tokens'] == 8
            assert len(executed[provider]) == 1
            print(f'PASS: {provider} configured authentication, token cap, tool execution, continuation metadata, verified completion')
            for missing_value in [None, '']:
                missing_env = env.copy()
                if missing_value is None:
                    missing_env.pop('HUDSON_OFFLINE_TEST_KEY', None)
                else:
                    missing_env['HUDSON_OFFLINE_TEST_KEY'] = missing_value
                rejected = subprocess.run(['target/debug/hudson-worker', '--config', str(config), '--task', 'Must not dispatch'],
                                          env=missing_env, capture_output=True, text=True, timeout=15)
                assert rejected.returncode != 0 and 'credential' in rejected.stderr
                assert len(requests[provider]) == 2 and len(executed[provider]) == 1
        print('PASS: missing/empty explicitly configured credentials fail before provider IO for every preset')
finally:
    stub.shutdown()
    stub.server_close()
    thread.join()
