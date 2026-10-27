"""Offline authenticated admission, scoped credentials, revocation and CLI transport."""
import json
import os
import pathlib
import re
import select
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

from operator_cli import command as operator_command

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / 'integrations/python'))
from hudson_client import HudsonClient, HttpError

binary = pathlib.Path(os.environ.get('HUDSON_TEST_BIN_DIR', 'target/debug'))
database = os.environ.get('HUDSON_TEST_DATABASE', 'hudson_harness_test_20260921')
namespace = 'api-auth-' + str(uuid.uuid4())
scope = ['--database', database, '--namespace', namespace,
         '--workspace-id', 'customer-one', '--actor-id', 'backend']
process = None


def operator(*args, other=False):
    binding = list(scope)
    if other:
        binding[binding.index('customer-one')] = 'customer-two'
    result = subprocess.run([str(binary / 'hudson-credentials'), *binding, *args],
                            capture_output=True, text=True, timeout=10)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


try:
    with tempfile.TemporaryDirectory() as directory:
        config = pathlib.Path(directory) / 'agent.json'
        config.write_text(json.dumps({'name': 'authenticated-agent', 'instructions': 'finish',
            'endpoint': 'http://127.0.0.1:1/model', 'api_key_env': 'HUDSON_ABSENT_WORKER_KEY',
            'input_schema': {'type': 'object', 'required': ['task']}}))
        env = os.environ.copy()
        env.pop('HUDSON_ABSENT_WORKER_KEY', None)
        process = subprocess.Popen([str(binary / 'hudson-server'), '--config', str(config),
            *scope, '--temporal-task-queue', namespace, '--require-api-token', '--port', '0'],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        assert select.select([process.stderr], [], [], 10)[0], 'API startup timed out'
        line = process.stderr.readline()
        match = re.search(r'http://127.0.0.1:\d+', line)
        assert match, line
        api = match.group()

        def client_denied(token=None, status=401):
            try:
                HudsonClient(api, token=token).health()
            except HttpError as error:
                assert error.status == status
                assert token is None or token not in str(error)
            else:
                raise AssertionError('client accepted a denied credential')

        client_denied()

        def request(method, path, token=None, body=None):
            headers = {'Content-Type': 'application/json'}
            if token is not None:
                headers['Authorization'] = 'Bearer ' + token
            req = urllib.request.Request(api + path, method=method, headers=headers,
                data=json.dumps(body).encode() if body is not None else None)
            try:
                response = urllib.request.urlopen(req, timeout=5)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                raw = response.read().decode()
                try:
                    body = json.loads(raw)
                except json.JSONDecodeError:
                    body = {'error': raw}
                return response.status, body

        first = operator('issue', '--label', 'backend', '--ttl-seconds', '60')
        rotated = operator('issue', '--label', 'rotation', '--ttl-seconds', '60')
        wrong = operator('issue', '--label', 'other', '--ttl-seconds', '60', other=True)
        task = {'input': {'task': 'finish'}, 'request_key': 'one-task'}
        assert request('POST', '/runs', body=task)[0] == 401
        assert request('POST', '/runs', 'forged', task)[0] == 401
        assert request('POST', '/runs', wrong['token'], task)[0] == 403
        client_denied(wrong['token'], 403)
        code, receipt = request('POST', '/runs', first['token'], task)
        assert code == 202
        assert request('POST', '/runs', rotated['token'], task) == (202, receipt)
        run = receipt['run_id']
        assert request('GET', '/runs/' + run, first['token'])[1]['status'] == 'queued'
        assert request('POST', '/runs', first['token'], dict(task, workspace_id='customer-two'))[0] == 422
        cli_env = dict(env, HUDSON_TEST_API_TOKEN=first['token'])
        cli = [*operator_command(binary), '--url', api, '--api-token-env', 'HUDSON_TEST_API_TOKEN']
        result = subprocess.run([*cli, 'get', run], env=cli_env, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0
        assert first['token'] not in result.stdout + result.stderr
        operator('revoke', first['metadata']['id'])
        assert request('GET', '/runs/' + run, first['token'])[0] == 401
        client_denied(first['token'])
        result = subprocess.run([*cli, 'get', run], env=cli_env, capture_output=True, text=True, timeout=10)
        assert result.returncode != 0
        assert first['token'] not in result.stdout + result.stderr
        assert request('GET', '/runs/' + run, rotated['token'])[0] == 200
        expired = operator('issue', '--label', 'short-lived', '--ttl-seconds', '1')
        time.sleep(1.1)
        assert request('GET', '/health', expired['token'])[0] == 401
        client_denied(expired['token'])
        result = subprocess.run([*operator_command(binary), '--url', 'http://example.invalid',
            '--api-token-env', 'HUDSON_TEST_API_TOKEN', 'health'], env=cli_env,
            capture_output=True, text=True, timeout=5)
        assert result.returncode != 0 and 'requires HTTPS' in result.stderr
        assert first['token'] not in result.stdout + result.stderr
        assert request('POST', '/runs/' + run + '/cancel', rotated['token'], {})[0] == 200
    process.send_signal(signal.SIGINT)
    _, errors = process.communicate(timeout=10)
    assert process.returncode == 0, errors
    process = None
    print('PASS: scoped API credentials, denial before admission, rotation, expiry, persistent revocation, authenticated CLI and graceful shutdown')
finally:
    if process:
        process.kill()
        process.wait(timeout=5)
