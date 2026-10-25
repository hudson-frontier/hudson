"""Exercise the local API's browser boundary with synthetic demo requests."""
import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, default=Path('target/debug/hudson-server'))
args = parser.parse_args()
with socket.socket() as reservation:
    reservation.bind(('127.0.0.1', 0))
    port = reservation.getsockname()[1]


def request(host, origin=None, method='GET', path='/health', body=None, extra=()):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=2)
    try:
        connection.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        if host is not None:
            connection.putheader('Host', host)
        if origin is not None:
            connection.putheader('Origin', origin)
        for name, value in extra:
            connection.putheader(name, value)
        raw = b'' if body is None else json.dumps(body).encode()
        connection.putheader('Content-Type', 'application/json')
        connection.putheader('Content-Length', str(len(raw)))
        connection.endheaders(raw)
        response = connection.getresponse()
        payload = response.read(65537)
        assert len(payload) <= 65536
        return response.status, payload
    finally:
        connection.close()


environment = {name: value for name, value in os.environ.items()
               if name in ('PATH', 'HOME', 'LANG', 'LC_ALL', 'SYSTEMROOT', 'TEMP', 'TMP')}
with tempfile.TemporaryFile() as log:
    process = subprocess.Popen([str(args.binary.resolve()), '--demo', '--port', str(port)],
                               env=environment, stdout=subprocess.DEVNULL, stderr=log)
    try:
        deadline = time.monotonic() + 30
        host = f'127.0.0.1:{port}'
        while True:
            try:
                assert request(host)[0] == 200
                break
            except (ConnectionError, OSError):
                assert process.poll() is None, 'demo server exited before readiness'
                assert time.monotonic() < deadline, 'demo server readiness timed out'
                time.sleep(0.025)
        for value in (host, f'localhost:{port}', f'LOCALHOST:{port}', f'[::1]:{port}', 'localhost:80'):
            assert request(value)[0] == 200, value
            assert request(value, 'http://' + value)[0] == 200, value
        body = {'input': {'order_id': 'browser-boundary-fixture', 'action': 'lookup'}, 'request_key': 'boundary-rejected'}
        for value in (f'attacker.invalid:{port}', f'127.0.0.1.attacker.invalid:{port}',
                      f'attacker@localhost:{port}', f'localhost:{port}/extra'):
            status, _ = request(value, 'http://' + value, 'POST', '/runs', body)
            assert status == 403, ('untrusted authority reached run admission', value, status)
        for origin in ('null', 'http://attacker.invalid', f'http://localhost:{port}',
                       'https://' + host, 'http://' + host + '/extra'):
            assert request(host, origin, 'POST', '/runs', body)[0] == 403, origin
        assert request(host, path=f'http://attacker.invalid:{port}/health')[0] == 403
        assert request(host, extra=(('Origin', 'http://' + host), ('Origin', 'http://' + host)))[0] in (400, 403)
        assert request(host, extra=(('Host', host),))[0] in (400, 403)
        assert request(None)[0] in (400, 403)
        status, response = request(host, method='POST', path='/runs', body={
            'input': {'order_id': 'browser-boundary-fixture', 'action': 'lookup'}, 'request_key': 'boundary-valid'})
        assert status == 202 and json.loads(response)['run_id'], (status, response)
        print('PASS local API authority/origin rejection before admission, canonical and forwarded loopback access, and ordinary submission.')
    except BaseException:
        log.seek(0)
        print(log.read(8192).decode(errors='replace'), file=sys.stderr)
        raise
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
