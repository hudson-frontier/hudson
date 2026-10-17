"""Offline real-terminal interaction: no provider, tool, or paid-service calls."""
import http.server
import json
import os
import pathlib
import pty
import select
import subprocess
import threading
import time

RUN = 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa'
OPERATION = 'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb'
requests = []
status = 'waiting'
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args): pass
    def send(self, body, code=200):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_POST(self):
        global status
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append((self.path, body, self.headers.get('Authorization')))
        if self.path == '/runs' and body['input'] == 'Rejected task':
            self.send({'error':'offline-session-token'},403)
            return
        if self.path.endswith('/input'):
            assert body['question_id'] == 7 and body['input'] == 'Boston'
            status = 'completed'
        self.send({'run_id':RUN})
    def do_GET(self):
        requests.append((self.path, None, self.headers.get('Authorization')))
        if '/events?' in self.path:
            if 'after=0&' in self.path:
                self.send([{'sequence':1,'run_id':RUN,'event_type':'run_waiting','payload':{'message':'\x1b]52;c;secret\x07'}}])
            else: self.send([])
        else:
            self.send({'id':RUN,'status':status,'wait':{'type':'user_input','question_id':7,'prompt':'Which city?'} if status == 'waiting' else None,'result':{'city':'Boston'} if status == 'completed' else None})
server = http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
master, slave = pty.openpty()
env = os.environ.copy()
env['HUDSON_SESSION_TEST_TOKEN'] = 'offline-session-token'
process = subprocess.Popen(['target/debug/hudson','--url',f'http://127.0.0.1:{server.server_port}','--api-token-env','HUDSON_SESSION_TEST_TOKEN'], stdin=slave,stdout=slave,stderr=slave,env=env)
os.close(slave)
output = bytearray()
def wait_for(text, since=0):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if text.encode() in output[since:]: return
        readable, _, _ = select.select([master], [], [], .1)
        if readable: output.extend(os.read(master,65536))
    raise AssertionError(f'missing {text!r}: {output.decode(errors="replace")}')
def send(text):
    before = len(output)
    os.write(master,(text+'\n').encode())
    wait_for('hudson> ',before)
try:
    wait_for('hudson> ')
    send('Please find a city')
    assert b'Which city?' in output and b'\\u001b' in output, output.decode(errors='replace')
    assert b'\x1b]52' not in output
    send('/reply 7 Boston')
    assert b'Result:' in output and b'Boston' in output
    send('Rejected task')
    assert b'403 Forbidden' in output
    send('/status')
    send('/open '+RUN)
    send('/deny '+OPERATION)
    send('/resume')
    send('/cancel')
    send('/unknown')
    assert b'unknown command' in output
    os.write(master,b'/quit\n')
    assert process.wait(timeout=5) == 0
    submissions = [body for path,body,_ in requests if path == '/runs']
    assert len(submissions) == 2 and submissions[0]['input'] == 'Please find a city'
    assert submissions[1]['input'] == 'Rejected task' # rejected mutation not replayed
    assert set(submissions[0]) == {'input','request_key'} # no implicit file/project authority
    assert all(auth == 'Bearer offline-session-token' for _,_,auth in requests)
    assert len([1 for path,_,_ in requests if path.endswith('/input')]) == 1
    assert any(path.endswith('/approval') and body == {'approved':False} for path,body,_ in requests)
    assert 'offline-session-token' not in output.decode()
    script = subprocess.run(['target/debug/hudson'],input='/quit\n',text=True,capture_output=True)
    assert script.returncode != 0 and 'requires a terminal' in script.stderr
    pathlib.Path('/tmp/hudson-session-transcript.txt').write_bytes(output)
    print('interactive PTY smoke passed: task, waits/reply, results, reopen, controls, auth, escape safety, no implicit project uploads')
finally:
    if process.poll() is None: process.kill(); process.wait()
    os.close(master)
    server.shutdown()
