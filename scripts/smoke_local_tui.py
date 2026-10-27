"""Exercise the default local TUI with real files, shell, SSE and process restart.

Uses a loopback Anthropic protocol fixture; never makes a paid model request.
"""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
requests = []
errors = []


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            requests.append(body)
            assert body['stream'] is True and self.path == '/v1/messages'
            assert self.headers['anthropic-version'] == '2023-06-01'
            text = json.dumps(body['messages'])
            if 'Now explain' in text:
                assert 'fixture verified' in text and 'edit_file' in text
                block = {'type': 'text', 'text': 'follow-up remembered'}
            else:
                result_count = sum(block['type'] == 'tool_result'
                                   for message in body['messages'] for block in message['content'])
                calls = [
                    ('read_file', {'path': 'calculation.py'}),
                    ('edit_file', {'path': 'calculation.py', 'old_text': 'value = 1', 'new_text': 'value = 2'}),
                    ('shell', {'command': 'python3 calculation.py'}),
                ]
                if result_count < len(calls):
                    name, arguments = calls[result_count]
                    block = {'type': 'tool_use', 'id': f'call-{result_count}', 'name': name, 'input': arguments}
                else:
                    results = [json.loads(block['content']) for message in body['messages']
                               for block in message['content'] if block['type'] == 'tool_result']
                    assert any(result.get('value', {}).get('success') is True for result in results)
                    block = {'type': 'text', 'text': 'fixture verified'}
            events = [{'type': 'message_start', 'message': {'content': [], 'usage': {'input_tokens': 10, 'output_tokens': 0}}}]
            if block['type'] == 'text':
                events += [
                    {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}},
                    {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': block['text']}},
                ]
            else:
                events += [
                    {'type': 'content_block_start', 'index': 0, 'content_block': {**block, 'input': {}}},
                    {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'input_json_delta', 'partial_json': json.dumps(block['input'])}},
                ]
            events += [
                {'type': 'content_block_stop', 'index': 0},
                {'type': 'message_delta', 'delta': {'stop_reason': 'tool_use' if block['type'] == 'tool_use' else 'end_turn'}, 'usage': {'output_tokens': 5}},
                {'type': 'message_stop'},
            ]
            output = ''.join(f'event: {event["type"]}\ndata: {json.dumps(event)}\n\n' for event in events).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(output)))
            self.end_headers()
            self.wfile.write(output)
        except Exception as error:
            errors.append(repr(error))
            self.send_error(500)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()


def run_terminal(workspace, state, resume=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 110, 0, 0))
    env = {**os.environ, 'TERM': 'xterm-256color', 'HUDSON_STATE_DIR': str(state)}
    env.pop('ANTHROPIC_API_KEY', None)
    command = [str(ROOT / 'target/debug/hudson'), '--workspace', str(workspace), '--model', 'fixture',
               '--endpoint', f'http://127.0.0.1:{server.server_port}/v1/messages', '--permission', 'full-access']
    if resume:
        command += ['--resume', resume]
    process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave, env=env, cwd=workspace)
    os.close(slave)
    output = bytearray()

    def wait_for(text):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if text.encode() in output:
                return
            if process.poll() is not None:
                break
            readable, _, _ = select.select([master], [], [], .1)
            if readable:
                try:
                    output.extend(os.read(master, 65536))
                except OSError:
                    break
        import re
        cleaned = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', output.decode(errors='replace'))
        raise AssertionError(f'missing {text!r}: {cleaned[-5000:]}; fixture errors: {errors}; requests: {len(requests)}')

    try:
        wait_for('Conversation')
        os.write(master, ('Now explain\r' if resume else 'Fix the calculation\r').encode())
        # Differential rendering can reuse screen cells, so whole final phrases
        # need not occur contiguously in the raw terminal byte stream. Inspect
        # the durable authoritative result while continuing to drain the PTY.
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            sessions = list(state.glob('*/session.json'))
            if sessions:
                session = json.loads(sessions[0].read_text())
                snapshot = json.loads((sessions[0].parent / 'state.json').read_text())
                run = snapshot['state']['runs'].get(session['turns'][-1]['run']) if len(session['turns']) == (2 if resume else 1) else None
                if run and run['status'] == 'completed':
                    assert run['result'] == ('follow-up remembered' if resume else 'fixture verified')
                    break
            readable, _, _ = select.select([master], [], [], .05)
            if readable:
                output.extend(os.read(master, 65536))
        else:
            raise AssertionError('run never committed completion')
        os.write(master, b'/quit\r')
        deadline = time.monotonic() + 10
        while process.poll() is None and time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], .1)
            if readable:
                try:
                    output.extend(os.read(master, 65536))
                except OSError:
                    break
        assert process.wait(timeout=2) == 0, output.decode(errors='replace')[-3000:]
        return session['id']
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)


try:
    with tempfile.TemporaryDirectory(prefix='hudson-tui-smoke-') as temporary:
        root = Path(temporary)
        workspace = root / 'project'
        workspace.mkdir()
        (workspace / 'calculation.py').write_text('value = 1\nassert value == 2\n')
        state = root / 'state'
        session_id = run_terminal(workspace, state)
        assert (workspace / 'calculation.py').read_text() == 'value = 2\nassert value == 2\n'
        assert run_terminal(workspace, state, session_id) == session_id
        assert len(requests) == 5 and not errors, (len(requests), errors)
        print('PASS: default local TUI, SSE, actual read/edit/shell verification, durable completion and fresh-process conversation resume')
finally:
    server.shutdown()
