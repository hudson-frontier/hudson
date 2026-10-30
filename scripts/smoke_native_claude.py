"""Installed Claude Agent SDK and native CLI conformance with synthetic Messages.

SPDX-License-Identifier: Apache-2.0
"""
import argparse
import hashlib
import http.server
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--python', required=True, help='Absolute Python with the pinned Claude SDK installed')
    parser.add_argument('--worker')
    parser.add_argument('--bubblewrap')
    parser.add_argument('--debug', action='store_true', help='Private receipt diagnostics for synthetic fixtures only')
    args = parser.parse_args()
    gateway = load('claude_fixture_gateway', ROOT / 'integrations/claude/gateway.py')
    requests, effects, failures = [], [], []
    unsafe = threading.Event()
    model = 'claude-sonnet-4-6'
    alias = gateway.tool_alias('lookup_order')

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass
        def do_POST(self):
            try:
                body = gateway.decode(self.rfile.read(int(self.headers['Content-Length'])))
                if self.path == '/lookup':
                    effects.append((self.headers['Idempotency-Key'], body))
                    raw, content_type = b'42', 'application/json'
                else:
                    assert self.path == '/v1/messages'
                    assert self.headers['x-api-key'] == 'synthetic-claude-provider-key'
                    assert self.headers['anthropic-version'] == '2023-06-01'
                    assert body['model'] == model and body['max_tokens'] == 32
                    assert body['tools'][0]['name'] == alias and len(body['tools']) == 1
                    requests.append(body)
                    completed_tool = any(block['type'] == 'tool_result' for message in body['messages'] for block in message['content'])
                    blocks = [{'type': 'text', 'text': '42'}] if completed_tool else [
                        {'type': 'thinking', 'thinking': 'private fixture reasoning', 'signature': 'fixture-signed'},
                        {'type': 'text', 'text': 'Looking up the answer.'},
                        {'type': 'tool_use', 'id': 'lookup-1', 'name': alias, 'input': {'order_id': '123'}}]
                    if completed_tool:
                        prior = [block for message in body['messages'] if message['role'] == 'assistant' for block in message['content']]
                        assert sum(block.get('signature') == 'fixture-signed' for block in prior) == 1
                        assert sum(block.get('text') == 'Looking up the answer.' for block in prior) == 1
                    if unsafe.is_set():
                        blocks = [{'type': 'tool_use', 'id': 'unadvertised', 'name': 'Bash', 'input': {'command': 'must not execute'}}]
                    response = {'id': 'msg-fixture-' + str(len(requests)), 'type': 'message', 'role': 'assistant',
                        'model': model, 'content': blocks, 'stop_reason': 'end_turn' if completed_tool else 'tool_use',
                        'stop_sequence': None, 'usage': {'input_tokens': 10, 'output_tokens': 2,
                            'cache_read_input_tokens': 3, 'cache_creation_input_tokens': 4}}
                    raw, content_type = gateway.stream(response), 'text/event-stream'
                self.send_response(200)
                self.send_header('Content-Type', content_type)
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
            except Exception as error:
                failures.append(repr(error))
                self.send_error(500)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='hudson-claude-fixture-', dir='/var/tmp' if args.bubblewrap else None) as state:
            env = {'ANTHROPIC_API_KEY': 'synthetic-claude-provider-key', 'HUDSON_CLAUDE_PYTHON': args.python,
                'HUDSON_CLAUDE_STATE_DIR': state, 'PATH': os.environ.get('PATH', ''),
                'HUDSON_CLAUDE_ENDPOINT': f'http://127.0.0.1:{server.server_port}/v1/messages'}
            call = {'run_id': str(uuid.uuid4()), 'operation_id': str(uuid.uuid4()), 'attempt_id': str(uuid.uuid4())}
            request = {'instructions': 'Return 42 using lookup_order.', 'model': model,
                'tools': [{'name': 'lookup_order', 'description': 'Look up an order', 'input_schema': {
                    'type': 'object', 'properties': {'order_id': {'type': 'string'}}, 'required': ['order_id']}}],
                'messages': [{'role': 'user', 'content': [{'type': 'json', 'value': 'start'}]}]}
            context = {'call': call, 'actor': {'workspace_id': 'fixture', 'id': 'owner'}, 'previous': None,
                       'history_mode': 'replace', 'request_fingerprint': fingerprint(request)}
            body = {'protocol_version': 1, 'runtime_id': 'claude-fixture', 'runtime_version': 1,
                    'max_output_tokens': 32, 'stream': True, 'request': request, 'context': context}
            peer = str(ROOT / 'integrations/claude/peer.py')
            command = [args.python, '-I', peer]
            if args.debug:
                debug = ("import runpy,sys,json; p=sys.argv[1]; sys.argv=[p]; n=runpy.run_path(p,run_name='fixture'); "
                    "original=n['validate_native']; "
                    "exec('def checked(native,response,aliases):\\n try: return original(native,response,aliases)\\n except Exception:\\n  print(json.dumps(native),file=sys.stderr)\\n  raise'); "
                    "n['run'].__globals__['validate_native']=checked; n['main']()")
                command = [args.python, '-I', '-c', debug, peer]

            def invoke(expect_success=True):
                result = subprocess.run(command, input=json.dumps(body), env=env, text=True, capture_output=True, timeout=150)
                if not expect_success:
                    assert result.returncode != 0
                    return None
                assert result.returncode == 0, result.stderr
                frames = [json.loads(line) for line in result.stdout.splitlines()]
                assert frames[0]['type'] == 'start' and frames[-1]['type'] == 'response'
                return frames[-1]['result']

            first = invoke()
            assert first['response']['type'] == 'tool_calls'
            tool_call = first['response']['calls'][0]
            assert tool_call['name'] == 'lookup_order' and tool_call['call_id'] == 'lookup-1'
            assert invoke() == first and len(requests) == 1
            request['messages'] += [
                {'role': 'assistant', 'content': [{'type': 'tool_call', 'call': tool_call}]},
                {'role': 'tool', 'content': [{'type': 'tool_result', 'result': {
                    'call_id': 'lookup-1', 'outcome': {'status': 'success', 'value': 42}}}]}]
            context.update(call=dict(call, operation_id=str(uuid.uuid4()), attempt_id=str(uuid.uuid4())),
                previous=first['checkpoint'], history_mode='append', request_fingerprint=fingerprint(request))
            final = invoke()
            assert final['response'] == {'type': 'final', 'output': 42}
            assert all(reply['usage'] == {'input_tokens': 17, 'output_tokens': 2} for reply in (first, final))
            context.update(call={'run_id': str(uuid.uuid4()), 'operation_id': str(uuid.uuid4()), 'attempt_id': str(uuid.uuid4())},
                           previous=final['checkpoint'])
            request['messages'] += [{'role': 'user', 'content': [{'type': 'json', 'value': 'independent fork'}]}]
            context['request_fingerprint'] = fingerprint(request)
            assert invoke()['response'] == {'type': 'final', 'output': 42} and len(requests) == 3
            context['actor']['id'] = 'different-owner'
            context['call'] = {'run_id': str(uuid.uuid4()), 'operation_id': str(uuid.uuid4()), 'attempt_id': str(uuid.uuid4())}
            invoke(expect_success=False)
            assert len(requests) == 3 and not failures, failures
            assert 'synthetic-claude-provider-key' not in ''.join(path.read_text() for path in pathlib.Path(state).rglob('*.json'))
            print('PASS: installed Claude SDK/native CLI, tool deferral, actor-owned native snapshots, replay, host transcript continuation/fork, one provider request and cache-inclusive usage', flush=True)
            unsafe.set()
            context.update(actor={'workspace_id': 'fixture', 'id': 'owner'}, previous=None, history_mode='replace',
                call={'run_id': str(uuid.uuid4()), 'operation_id': str(uuid.uuid4()), 'attempt_id': str(uuid.uuid4())})
            invoke(expect_success=False)
            assert len(requests) == 4 and not effects, (len(requests), effects)
            invoke(expect_success=False)
            assert len(requests) == 4, 'ambiguous attempt retried a provider request'
            unsafe.clear()
            print('PASS: unadvertised native effect rejected before dispatch; ambiguous replay cannot issue another provider request', flush=True)
            if args.worker:
                requests.clear()
                runtime = {'id': 'claude-fixture', 'version': 1, 'stream': True, 'native_sessions': True,
                    'timeout_seconds': 300, 'transport': {'type': 'stdio', 'executable': args.python,
                    'args': ['-I', peer], 'cwd': state, 'env': list(env)}}
                if args.bubblewrap:
                    prefix = pathlib.Path(subprocess.check_output([args.python, '-I', '-c', 'import sys; print(sys.prefix)'], text=True, timeout=30).strip())
                    roots = [pathlib.Path('/usr'), pathlib.Path('/lib'), pathlib.Path('/lib64'), ROOT / 'integrations', prefix]
                    runtime['transport'].update(type='isolated_stdio', isolation={'bubblewrap': args.bubblewrap,
                        'read_only': [str(path) for path in roots], 'network': 'host'})
                config = {'name': 'native-claude-fixture', 'instructions': 'Return 42 using lookup_order.', 'model': model,
                    'max_output_tokens': 32, 'output_schema': {'type': 'integer'}, 'runtime': runtime,
                    'http_tools': [{'name': 'lookup_order', 'description': 'Look up an order', 'effect': 'read',
                        'endpoint': f'http://127.0.0.1:{server.server_port}/lookup', 'input_schema': {
                            'type': 'object', 'properties': {'order_id': {'type': 'string'}}, 'required': ['order_id'],
                            'additionalProperties': False}, 'output_schema': {'type': 'integer'}}]}
                path = pathlib.Path(state) / 'agent.json'
                path.write_text(json.dumps(config))
                result = subprocess.run([args.worker, '--config', str(path), '--task', 'start'], env=env,
                    capture_output=True, text=True, timeout=360)
                assert result.returncode == 0, result.stderr
                run = json.loads(result.stdout)
                assert run['status'] == 'completed' and run['result'] == 42, run
                assert run['usage']['model_calls'] == 2 and run['usage']['tool_calls'] == 1
                assert run['usage']['reported_input_tokens'] == 34 and len(effects) == 1 and len(requests) == 2
                print('PASS: actual worker, isolated Claude SDK/native CLI, host-owned tool and goal verification', flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


if __name__ == '__main__':
    main()
