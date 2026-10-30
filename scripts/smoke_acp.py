"""Real ACP stdio / Hudson HTTP tool cycle against bounded local model fixtures.

SPDX-License-Identifier: Apache-2.0
"""
import argparse
import http.server
import json
import os
import pathlib
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--server', default=str(ROOT / 'target/debug' / ('hudson-server.exe' if os.name == 'nt' else 'hudson-server')))
    args = parser.parse_args()
    calls, writes, failures = [], [], []
    held, release = threading.Event(), threading.Event()

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            try:
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                if self.path == '/tool':
                    writes.append((self.headers['Idempotency-Key'], body))
                    response, content_type = {'saved': True}, 'application/json'
                else:
                    assert self.path == '/model'
                    calls.append(body)
                    assert body['max_output_tokens'] == 32
                    request = body['request']
                    last = request['messages'][-1]
                    latest = ''.join(block.get('text', '') if block['type'] == 'text'
                                     else block.get('value', '') if isinstance(block.get('value'), str) else ''
                                     for block in last['content'])
                    if latest == 'Hold this ACP request':
                        held.set()
                        assert release.wait(10), 'cancellation fixture was not released'
                        model = {'type': 'final', 'output': 'too late'}
                    elif latest == '/compact\n{"kind":"compact","input":null}':
                        assert any(message['role'] == 'tool' for message in request['messages'])
                        model = {'type': 'final', 'output': 'literal followup'}
                    elif last['role'] == 'tool':
                        model = {'type': 'final', 'output': 'saved'}
                    else:
                        assert request['tools'][0]['name'] == 'save'
                        model = {'type': 'tool_calls', 'calls': [{'name': 'save', 'call_id': 'write-1', 'arguments': {'value': 42}}]}
                    reply = {'protocol_version': 1, 'runtime_id': body['runtime_id'], 'runtime_version': body['runtime_version'],
                             'response': model, 'usage': {'input_tokens': 10, 'output_tokens': 2}}
                    frames = [{'type': 'start', 'protocol_version': 1, 'runtime_id': body['runtime_id'], 'runtime_version': body['runtime_version']}]
                    if model['type'] == 'final':
                        frames.append({'type': 'text_delta', 'text': model['output']})
                    frames.append({'type': 'response', 'result': reply})
                    response, content_type = frames, 'application/x-ndjson'
                raw = (b''.join(json.dumps(frame).encode() + b'\n' for frame in response)
                       if content_type == 'application/x-ndjson' else json.dumps(response).encode())
                self.send_response(200)
                self.send_header('Content-Type', content_type)
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception as error:
                failures.append(repr(error))
                self.send_error(500)

    stub = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    stub.daemon_threads = True
    stub_thread = threading.Thread(target=stub.serve_forever, daemon=True)
    stub_thread.start()
    server = acp = None
    try:
        with tempfile.TemporaryDirectory(prefix='hudson-acp-smoke-') as temporary:
            cwd = str(pathlib.Path(temporary).resolve())
            config = pathlib.Path(temporary) / 'agent.json'
            origin = f'http://127.0.0.1:{stub.server_port}'
            config.write_text(json.dumps({'name': 'acp-fixture', 'instructions': 'Use the save tool.', 'model': 'local-fixture',
                'max_output_tokens': 32, 'limits': {'max_model_calls': 2},
                'runtime': {'id': 'acp-smoke', 'version': 1, 'stream': True,
                            'transport': {'type': 'http', 'endpoint': origin + '/model'}},
                'http_tools': [{'name': 'save', 'description': 'Save the supplied value', 'endpoint': origin + '/tool',
                                'input_schema': {'type': 'object'}, 'effect': 'write', 'require_approval': True}]}), encoding='utf-8')
            env = {key: value for key, value in os.environ.items()
                   if key not in ('OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'GOOGLE_API_KEY')}
            env['TOKIO_WORKER_THREADS'] = '2'
            server = subprocess.Popen([args.server, '--config', str(config), '--port', '0'], env=env,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            startup = queue.Queue(1)
            threading.Thread(target=lambda: startup.put(server.stderr.readline()), daemon=True).start()
            line = startup.get(timeout=60).decode()
            found = re.search(r'http://127.0.0.1:\d+', line)
            assert found, line
            acp = subprocess.Popen([sys.executable, '-I', str(ROOT / 'integrations/acp/peer.py'),
                                    '--url', found.group(), '--cwd', cwd, '--allow-approvals'],
                                   env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            output = queue.Queue(16)

            def read():
                try:
                    while True:
                        raw = acp.stdout.readline(1024 * 1024 + 1)
                        if not raw:
                            return
                        assert len(raw) <= 1024 * 1024 and raw.endswith(b'\n')
                        output.put(json.loads(raw), timeout=1)
                except Exception as error:
                    failures.append(repr(error))
            threading.Thread(target=read, daemon=True).start()

            def send(method, params, identifier=None):
                message = {'jsonrpc': '2.0', 'method': method, 'params': params}
                if identifier is not None:
                    message['id'] = identifier
                acp.stdin.write(json.dumps(message).encode() + b'\n')
                acp.stdin.flush()

            def until(predicate, allow_error=False):
                seen, deadline = [], time.monotonic() + 45
                while time.monotonic() < deadline:
                    try:
                        value = output.get(timeout=max(.01, deadline - time.monotonic()))
                    except queue.Empty:
                        raise AssertionError(('ACP response timed out', seen, len(calls), failures, acp.poll())) from None
                    seen.append(value)
                    assert allow_error or 'error' not in value, (value, calls, failures)
                    if predicate(value):
                        return value, seen
                raise AssertionError(seen)

            def prompt(session, text, identifier):
                send('session/prompt', {'sessionId': session, 'prompt': [{'type': 'text', 'text': text}]}, identifier)

            send('initialize', {'protocolVersion': 1, 'clientCapabilities': {}}, 1)
            assert until(lambda value: value.get('id') == 1)[0]['result']['protocolVersion'] == 1
            send('session/new', {'cwd': cwd, 'mcpServers': []}, 2)
            session = until(lambda value: value.get('id') == 2)[0]['result']['sessionId']
            prompt(session, 'Save 42', 3)
            permission, before = until(lambda value: value.get('method') == 'session/request_permission')
            assert not writes and len(calls) == 1 and not failures, (writes, calls, failures)
            operation = permission['params']['toolCall']
            assert operation['rawInput'] == {'value': 42}
            acp.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': permission['id'], 'result': {
                'outcome': {'outcome': 'selected', 'optionId': 'allow-once'}}}).encode() + b'\n')
            acp.stdin.flush()
            result, after = until(lambda value: value.get('id') == 3)
            assert result['result']['stopReason'] == 'end_turn', result
            receipt = result['result']['_meta']['org.hudson/run']
            assert receipt['usage']['model_calls'] == 2 and receipt['usage']['reported_input_tokens'] == 20, receipt
            assert writes == [(operation['toolCallId'], {'value': 42})], writes
            updates = [value['params']['update'] for value in before + after if value.get('method') == 'session/update']
            assert ''.join(value['content']['text'] for value in updates if 'content' in value) == 'saved', updates
            assert any(value.get('_meta', {}).get('provisional') for value in updates), updates
            assert any(value.get('status') == 'completed' for value in updates), updates
            prompt(session, '/compact\n{"kind":"compact","input":null}', 4)
            result, _ = until(lambda value: value.get('id') == 4)
            assert result['result']['stopReason'] == 'end_turn', result
            assert result['result']['_meta']['org.hudson/run']['runId'] != receipt['runId']
            send('session/new', {'cwd': cwd, 'mcpServers': []}, 5)
            other = until(lambda value: value.get('id') == 5)[0]['result']['sessionId']
            prompt(other, 'Cancel at approval', 6)
            until(lambda value: value.get('method') == 'session/request_permission')
            send('session/cancel', {'sessionId': other})
            result, _ = until(lambda value: value.get('id') == 6)
            assert result['result']['stopReason'] == 'cancelled', result
            send('session/new', {'cwd': cwd, 'mcpServers': []}, 7)
            held_session = until(lambda value: value.get('id') == 7)[0]['result']['sessionId']
            prompt(held_session, 'Hold this ACP request', 8)
            assert held.wait(10), failures
            send('session/cancel', {'sessionId': held_session})
            # Closing remote HTTP observation is not a remote cancellation
            # receipt. Keep the fixture active until Hudson's uncertainty is
            # reported, rather than fabricate a confirmed cancelled outcome.
            result, _ = until(lambda value: value.get('id') == 8, allow_error=True)
            assert result['error']['code'] == -32001 and 'unresolved' in result['error']['message'], result
            assert result['error']['data']['runId']
            release.set()
            assert len(calls) == 5 and len(writes) == 1 and not failures, (calls, writes, failures)
            acp.stdin.close()
            assert acp.wait(timeout=15) == 0, acp.stderr.read().decode()
            print('PASS: ACP v1 stdio, real Hudson budgets, streamed text, exact approval, one host write, conversation continuation, literal controls, confirmed approval cancellation and unresolved remote cancellation; no paid calls')
    finally:
        release.set()
        for process in (acp, server):
            if process is not None:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)
                for stream in (process.stdin, process.stdout, process.stderr):
                    if stream is not None:
                        stream.close()
        stub.shutdown()
        stub.server_close()
        stub_thread.join(timeout=2)


if __name__ == '__main__':
    main()
