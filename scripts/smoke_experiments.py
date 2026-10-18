"""Exercise offline refinement, dry-run, exact resume and cancellation via the worker."""
import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import uuid

calls = []
tools = []
failures = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            length = int(self.headers['Content-Length'])
            assert length <= 128 * 1024
            assert not self.headers.get('Authorization'), 'host credential forwarded to fixture'
            request = json.loads(self.rfile.read(length))
            if self.path == '/tool':
                tools.append(request)
                response = {'observed': request['task']}
            else:
                assert self.path == '/model'
                calls.append(request)
                system = request['messages'][0]['content']
                user = request['messages'][1]['content']
                if system.startswith('Offline harness experiment stage:'):
                    assert 'PRIVATE_HELD_OUT' not in json.dumps(request)
                    assert 'tools' not in request
                    context = json.loads(user)
                    role = context['stage']
                    if role == 'reflect':
                        output = {'cue': 'Fixture labels', 'lesson': 'Inspect the recorded read result before returning a label.',
                                  'run_ids': [run['run_id'] for run in context['probe_executions']]}
                    else:
                        output = {'instructions': {'general': 'GENERAL', 'specific': 'SPECIFIC', 'compose': 'COMPOSED'}[role], 'skills': {}}
                    choice = {'finish_reason': 'stop', 'message': {'content': json.dumps(output)}}
                elif not any(message['role'] == 'tool' for message in request['messages']):
                    assert len(request['tools']) == 1
                    function = request['tools'][0]['function']
                    assert function['name'] == 'fixture_read'
                    assert function['parameters']['additionalProperties'] is False
                    choice = {'finish_reason': 'tool_calls', 'message': {'tool_calls': [{
                        'id': 'read', 'type': 'function', 'function': {'name': 'fixture_read', 'arguments': json.dumps({'task': user})}}]}}
                else:
                    choice = {'finish_reason': 'stop', 'message': {'content': 'ok' if system == 'COMPOSED' else 'wrong'}}
                response = {'choices': [choice], 'usage': {'prompt_tokens': 10, 'completion_tokens': 5}}
            body = json.dumps(response).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except Exception as error:
            failures.append(str(error))
            self.send_error(500)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory() as directory:
        root = pathlib.Path(directory)
        config = root / 'agent.json'
        spec = root / 'experiment.json'
        agent = {'name': 'fixture', 'instructions': 'BASE', 'model': 'fixture',
            'endpoint': f'http://127.0.0.1:{server.server_port}/model', 'output_schema': {'type': 'string'},
            'http_tools': [{'name': 'fixture_read', 'description': 'Read an isolated fixture',
                'endpoint': f'http://127.0.0.1:{server.server_port}/tool', 'effect': 'read',
                'input_schema': {'type': 'object', 'properties': {'task': {'type': 'string'}}, 'required': ['task'], 'additionalProperties': False}}]}
        config.write_text(json.dumps(agent))
        definition = {'request_key': 'pilot', 'domain': 'labels', 'probe': 'Visible probe', 'max_model_calls': 32,
                      'validation': [{'name': 'held-out-label', 'input': 'PRIVATE_HELD_OUT task',
                                      'expected_schema': {'const': 'ok', 'description': 'PRIVATE_HELD_OUT criterion'}}]}
        spec.write_text(json.dumps(definition))
        env = os.environ.copy()
        # Deliberately present: experiment build mode must never read/forward it.
        env['OPENAI_API_KEY'] = 'must-not-forward'
        worker = 'target/debug/hudson-worker'
        command = [worker, '--config', str(config), '--experiment', str(spec)]

        def run(arguments, ok=True):
            result = subprocess.run(arguments, env=env, capture_output=True, text=True, timeout=120)
            assert (result.returncode == 0) == ok, result.stderr
            assert not failures, failures
            return json.loads(result.stdout) if result.stdout.strip() else None

        plan = run(command + ['--experiment-plan', '--database', 'must_not_connect'])
        assert plan['probe_runs'] == 4 and plan['editing_runs'] == 4 and plan['validation_runs'] == 2
        assert not calls and not tools
        database = env.get('HUDSON_TEST_DATABASE')
        if not database:
            completed = run(command)
            assert completed['status'] == 'completed' and completed['selected']['version'] == 4
            assert completed['admitted_model_calls'] == len(calls) == 16 and len(tools) == 6
            print('PASS: offline fixture refinement, held-out improvement, fixed read tools, credential omission and bounded model calls')
            print('PostgreSQL resume/cancellation checks skipped; set HUDSON_TEST_DATABASE to include them.')
            raise SystemExit(0)
        scope = ['--database', database, '--namespace', f'experiment-smoke-{uuid.uuid4()}']
        paused = run(command + scope + ['--experiment-ticks', '1'])
        assert paused['status'] == 'running' and paused['admitted_model_calls'] == 0
        assert len(paused['stages']) == 1 and not calls
        experiment_id = paused['id']
        inspected = run([worker, *scope, '--inspect-experiment', experiment_id])
        assert inspected['manifest_digest'] == paused['manifest_digest']
        completed = run(command + scope)
        assert completed['status'] == 'completed' and completed['selected']['version'] == 4
        assert completed['stages'][0]['run']['id'] == paused['stages'][0]['run']['id']
        assert completed['admitted_model_calls'] == len(calls) == 16
        assert len(tools) == 6 and completed['lesson_id']
        assert completed['comparisons'][0]['baseline_passed'] is False
        assert completed['comparisons'][0]['candidate_passed'] is True
        assert run(command + scope)['admitted_model_calls'] == 16 and len(calls) == 16
        definition['max_model_calls'] = 33
        spec.write_text(json.dumps(definition))
        run(command + scope, ok=False)
        assert len(calls) == 16
        definition.update(request_key='cancel', max_model_calls=32)
        spec.write_text(json.dumps(definition))
        cancel = run(command + scope + ['--experiment-ticks', '1'])
        config.unlink()
        cancelled = run([worker, *scope, '--cancel-experiment', cancel['id']])
        assert cancelled['status'] == 'cancelled' and len(calls) == 16
        # A read tool may still require approval. The experiment cannot grant it.
        agent['http_tools'][0]['require_approval'] = True
        config.write_text(json.dumps(agent))
        definition['request_key'] = 'approval'
        spec.write_text(json.dumps(definition))
        approval_scope = ['--database', database, '--namespace', f'experiment-approval-{uuid.uuid4()}']
        waiting = run(command + approval_scope)
        assert waiting['status'] == 'waiting' and len(calls) == 17 and len(tools) == 6
        operation = waiting['stages'][0]['run']['wait']['operation_id']
        run([worker, *approval_scope, '--approve-operation', operation])
        assert len(calls) == 17 and len(tools) == 6
        stopped = run([worker, *approval_scope, '--cancel-experiment', waiting['id']])
        assert stopped['status'] == 'cancelled' and len(calls) == 17 and len(tools) == 6
        print('PASS: offline propose/probe/compose, held-out gate, frozen read tools, credential omission, CLI dry-run, PostgreSQL process resume, retained budget/lesson, exact replay and config-independent cancellation')
finally:
    server.shutdown()
    server.server_close()
    thread.join()
