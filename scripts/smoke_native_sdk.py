"""Real installed SDK and native checkpoint protocol against loopback fixtures."""
import argparse
import hashlib
import http.server
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
requests = []
effects = []


def response_events(index):
    tool = {'type': 'function_call', 'id': 'fc_fixture', 'call_id': 'lookup-1',
            'name': 'lookup_order', 'arguments': '{"order_id":"123"}', 'status': 'completed'}
    message = {'type': 'message', 'id': 'msg_fixture', 'role': 'assistant', 'status': 'completed',
               'content': [{'type': 'output_text', 'text': '42', 'annotations': [], 'logprobs': []}]}
    item = tool if index == 0 else message
    response = {'id': f'resp_{index}', 'object': 'response', 'created_at': 1, 'model': 'fixture',
                'status': 'completed', 'output': [item], 'error': None, 'incomplete_details': None,
                'usage': {'input_tokens': 10, 'output_tokens': 2, 'total_tokens': 12}}
    events = [{'type': 'response.created', 'response': dict(response, status='in_progress', output=[], usage=None)},
              {'type': 'response.output_item.added', 'output_index': 0, 'item': dict(item, status='in_progress')}]
    if index == 0:
        events += [{'type': 'response.function_call_arguments.done', 'output_index': 0,
                    'item_id': tool['id'], 'name': tool['name'], 'arguments': tool['arguments']}]
    else:
        events += [{'type': 'response.content_part.added', 'output_index': 0, 'content_index': 0,
                    'item_id': message['id'], 'part': {'type': 'output_text', 'text': '', 'annotations': [], 'logprobs': []}},
                   {'type': 'response.output_text.delta', 'output_index': 0, 'content_index': 0,
                    'item_id': message['id'], 'delta': '42', 'logprobs': []},
                   {'type': 'response.output_text.done', 'output_index': 0, 'content_index': 0,
                    'item_id': message['id'], 'text': '42', 'logprobs': []},
                   {'type': 'response.content_part.done', 'output_index': 0, 'content_index': 0,
                    'item_id': message['id'], 'part': message['content'][0]}]
    events += [{'type': 'response.output_item.done', 'output_index': 0, 'item': item},
               {'type': 'response.completed', 'response': response}]
    for sequence, event in enumerate(events):
        event['sequence_number'] = sequence
    return b''.join(('event: ' + event['type'] + '\ndata: ' + json.dumps(event) + '\n\n').encode() for event in events)


class Handler(http.server.BaseHTTPRequestHandler):
    timeout = 5

    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers.get('Content-Length', '0'))
        assert 0 < length <= 4 * 1024 * 1024
        body = json.loads(self.rfile.read(length))
        if self.path == '/lookup':
            assert body == {'order_id': '123'}
            effects.append(body)
            data = b'42'
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        assert self.path == '/v1/responses'
        assert body['max_output_tokens'] == 32 and body['stream'] is True
        assert self.headers['Authorization'] == 'Bearer synthetic-sdk-key'
        index = len(requests)
        requests.append(body)
        assert index < 2, 'SDK must not make hidden retries or follow-up calls'
        data = response_events(index)
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--python', default=sys.executable)
    parser.add_argument('--debug', action='store_true')
    parser.add_argument('--worker', help='Also run through this real Hudson worker binary')
    parser.add_argument('--bubblewrap', help='Run the actual worker peer through Linux isolation')
    args = parser.parse_args()
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='hudson-native-sdk-', dir='/var/tmp' if args.bubblewrap else None) as state:
            env = {'OPENAI_API_KEY': 'synthetic-sdk-key', 'HUDSON_SDK_STATE_DIR': state,
                   'HUDSON_SDK_BASE_URL': f'http://127.0.0.1:{server.server_port}/v1',
                   'PATH': os.environ.get('PATH', '')}
            call = {'run_id': str(uuid.uuid4()), 'operation_id': str(uuid.uuid4()), 'attempt_id': str(uuid.uuid4())}
            request = {'instructions': 'Return 42 using lookup_order.', 'model': 'fixture',
                       'tools': [{'name': 'lookup_order', 'description': 'Look up an order',
                                  'input_schema': {'type': 'object', 'properties': {'order_id': {'type': 'string'}}, 'required': ['order_id']}}],
                       'messages': [{'role': 'user', 'content': [{'type': 'json', 'value': 'start'}]}]}
            context = {'call': call, 'actor': {'workspace_id': 'fixture', 'id': 'owner'},
                       'previous': None, 'history_mode': 'replace', 'request_fingerprint': fingerprint(request)}
            body = {'protocol_version': 1, 'runtime_id': 'sdk-fixture', 'runtime_version': 1,
                    'max_output_tokens': 32, 'stream': True, 'request': request, 'context': context}

            def invoke():
                peer = str(ROOT/'integrations/agent_sdk/peer.py')
                command = [args.python, '-I', peer] if not args.debug else [args.python, '-I', '-c',
                    "import faulthandler,runpy,sys; faulthandler.dump_traceback_later(8,repeat=True); sys.argv=[sys.argv[1]]; runpy.run_path(sys.argv[0],run_name='__main__')", peer]
                try:
                    result = subprocess.run(command,input=json.dumps(body), capture_output=True, text=True, env=env, timeout=120)
                except subprocess.TimeoutExpired as error:
                    raise AssertionError(f'native peer deadline; fixture requests={len(requests)}; diagnostic={error.stderr!r}') from None
                if result.returncode != 0:
                    raise AssertionError('native SDK peer failed: ' + result.stderr)
                frames = [json.loads(line) for line in result.stdout.splitlines()]
                assert frames[0]['type'] == 'start' and frames[-1]['type'] == 'response'
                return frames, frames[-1]['result']

            _, first = invoke()
            assert first['response']['type'] == 'tool_calls'
            assert first['response']['calls'][0]['call_id'] == 'lookup-1'
            _, repeated = invoke()
            assert repeated == first and len(requests) == 1
            checkpoint = first['checkpoint']
            request['messages'] += [
                {'role': 'assistant', 'content': [{'type': 'tool_call', 'call': first['response']['calls'][0]}]},
                {'role': 'tool', 'content': [{'type': 'tool_result', 'result': {'call_id': 'lookup-1', 'outcome': {'status': 'success', 'value': 42}}}]}]
            context['call'] = dict(call, operation_id=str(uuid.uuid4()), attempt_id=str(uuid.uuid4()))
            context.update(previous=checkpoint, history_mode='append', request_fingerprint=fingerprint(request))
            frames, final = invoke()
            assert final['response'] == {'type': 'final', 'output': 42}
            assert any(frame.get('type') == 'text_delta' and frame['text'] == '42' for frame in frames)
            assert len(requests) == 2
            assert any(item.get('type') == 'function_call_output' and item['call_id'] == 'lookup-1' for item in requests[1]['input'])
            assert all(reply['usage'] == {'input_tokens': 10, 'output_tokens': 2} for reply in (first, final))
            print('PASS: real SDK pause, immutable checkpoint resume, streaming, tool receipts and exact one-call replay', flush=True)
            if args.worker:
                requests.clear()
                configuration = {'name': 'native-sdk-fixture', 'instructions': 'Return 42 using lookup_order.',
                    'model': 'fixture', 'max_output_tokens': 32, 'output_schema': {'type': 'integer'},
                    'runtime': {'id': 'sdk-fixture', 'version': 1, 'stream': True, 'native_sessions': True,
                        'timeout_seconds': 300,
                        'transport': {'type': 'stdio', 'executable': args.python,
                            'args': ['-I', str(ROOT/'integrations/agent_sdk/peer.py')], 'cwd': state,
                            'env': ['OPENAI_API_KEY', 'HUDSON_SDK_STATE_DIR', 'HUDSON_SDK_BASE_URL', 'PATH']}},
                    'http_tools': [{'name': 'lookup_order', 'description': 'Look up an order', 'effect': 'read',
                        'endpoint': f'http://127.0.0.1:{server.server_port}/lookup',
                        'input_schema': {'type': 'object', 'properties': {'order_id': {'type': 'string'}},
                            'required': ['order_id'], 'additionalProperties': False},
                        'output_schema': {'type': 'integer'}}]}
                if args.bubblewrap:
                    prefix = pathlib.Path(subprocess.check_output(
                        [args.python, '-I', '-c', 'import sys; print(sys.prefix)'], text=True, timeout=30).strip())
                    roots = [pathlib.Path('/usr'), pathlib.Path('/lib'), pathlib.Path('/lib64'),
                             ROOT/'integrations/agent_sdk']
                    if not any(prefix.is_relative_to(root) for root in roots):
                        roots.append(prefix)
                    transport = configuration['runtime']['transport']
                    transport['type'] = 'isolated_stdio'
                    transport['isolation'] = {'bubblewrap': args.bubblewrap,
                        'read_only': [str(root) for root in roots], 'network': 'host'}
                path = pathlib.Path(state)/'agent.json'
                path.write_text(json.dumps(configuration))
                result = subprocess.run([args.worker, '--config', str(path), '--task', 'start'],
                    env=env, capture_output=True, text=True, timeout=360)
                assert result.returncode == 0, result.stderr
                run = json.loads(result.stdout)
                assert run['status'] == 'completed' and run['result'] == 42
                assert run['usage']['model_calls'] == 2 and run['usage']['tool_calls'] == 1
                assert run['usage']['reported_input_tokens'] == 20 and len(effects) == 1
                assert len(requests) == 2
                print('PASS: actual worker, native SDK checkpoints, host-owned HTTP tool, goal verification and usage', flush=True)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == '__main__':
    main()
