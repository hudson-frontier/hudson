"""Authenticated customer publication through a restarted API and separate workers.
Called by the Temporal integration test with an isolated local server and database.
"""
import concurrent.futures
import copy
import http.server
import json
import os
import pathlib
import re
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

from operator_cli import command as operator_command

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / 'integrations/python'))
from hudson_client import HudsonClient, HttpError

binary = pathlib.Path(os.environ['HUDSON_TEST_BIN_DIR'])
database = os.environ['HUDSON_TEST_DATABASE']
namespace = 'publication-api-' + str(uuid.uuid4())
scope = ['--database', database, '--namespace', namespace,
         '--workspace-id', 'company', '--actor-id', 'owner']
processes = []
observed = {'models': 0, 'writes': [], 'errors': []}
lock = threading.Lock()


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            if self.path == '/model':
                assert self.headers['Authorization'] == 'Bearer model-secret'
                with lock:
                    observed['models'] += 1
                if any(m['role'] == 'tool' for m in body['messages']):
                    message = {'role': 'assistant', 'content': 'Record 42 verified'}
                    reason = 'stop'
                else:
                    message = {'role': 'assistant', 'content': None, 'tool_calls': [
                        {'id': 'lookup-call', 'type': 'function', 'function': {
                            'name': 'lookup', 'arguments': '{"customer_id":42}'}}]}
                    reason = 'tool_calls'
                result = {'choices': [{'message': message, 'finish_reason': reason}]}
            else:
                assert self.path == '/tool'
                assert self.headers['Authorization'] == 'Bearer tool-secret'
                assert body == {'customer_id': 42}
                operation = self.headers['Idempotency-Key']
                with lock:
                    assert operation not in observed['writes'], 'duplicate external write'
                    observed['writes'].append(operation)
                result = {'record_id': 42}
            encoded = json.dumps(result).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        except Exception as error:
            with lock:
                observed['errors'].append(repr(error))
            self.send_error(500)


def stop(process, graceful=False):
    if process.poll() is None:
        if graceful:
            process.send_signal(signal.SIGINT)
        else:
            process.kill()
    _, errors = process.communicate(timeout=10)
    if graceful:
        assert process.returncode == 0, errors
    processes.remove(process)


def worker():
    env = dict(os.environ, HUDSON_PUBLICATION_MODEL_KEY='model-secret',
               HUDSON_PUBLICATION_TOOL_KEY='tool-secret')
    process = subprocess.Popen([str(binary / 'hudson-temporal'), '--published', *scope,
        '--task-queue', namespace, 'worker'], env=env, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, text=True)
    processes.append(process)
    return process


def request(method, path, body=None, bearer=None):
    headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + (bearer or token)}
    req = urllib.request.Request(api + path, method=method, headers=headers,
        data=json.dumps(body).encode() if body is not None else None)
    try:
        response = urllib.request.urlopen(req, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        raw = response.read().decode()
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            value = {'error': raw}
        return response.status, value


def expect(method, path, body=None, code=200, bearer=None):
    status, value = request(method, path, body, bearer)
    assert status == code, (method, path, status, value)
    return value


def until(run, status, process):
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        assert process.poll() is None, process.communicate(timeout=5)
        view = HudsonClient(api, token=token).run(run)
        if view['status'] == status:
            return view
        assert view['status'] not in ('failed', 'cancelled'), view
        time.sleep(.1)
    raise AssertionError(('run did not reach ' + status, view, observed))


fixture = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
thread = threading.Thread(target=fixture.serve_forever, daemon=True)
thread.start()
endpoint = 'http://127.0.0.1:' + str(fixture.server_port)
try:
    with tempfile.TemporaryDirectory() as directory:
        path = pathlib.Path(directory) / 'catalog.json'
        catalog = {'owner': {'workspace_id': 'company', 'id': 'owner'},
            'models': [{'reference': {'id': 'standard', 'version': 1},
                        'provider': 'openai', 'model': 'fixture', 'endpoint': endpoint + '/model',
                        'api_key_env': 'HUDSON_PUBLICATION_MODEL_KEY'}],
            'connections': [{'reference': {'id': 'crm', 'version': 1},
                'transport': {'type': 'http', 'endpoint': endpoint + '/tool',
                              'token_env': 'HUDSON_PUBLICATION_TOOL_KEY'}}],
            'max_total_model_calls': 2}
        path.write_text(json.dumps(catalog))
        issued = subprocess.run([str(binary / 'hudson-credentials'), *scope, 'issue',
            '--label', 'customer', '--ttl-seconds', '300'], capture_output=True,
            text=True, timeout=10)
        assert issued.returncode == 0, issued.stderr
        token = json.loads(issued.stdout)['token']

        def start_api():
            env = os.environ.copy()
            env.pop('HUDSON_PUBLICATION_MODEL_KEY', None)
            env.pop('HUDSON_PUBLICATION_TOOL_KEY', None)
            process = subprocess.Popen([str(binary / 'hudson-server'), '--catalog', str(path),
                *scope, '--temporal-task-queue', namespace, '--require-api-token', '--port', '0'],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            processes.append(process)
            assert select.select([process.stderr], [], [], 10)[0], 'API startup timed out'
            line = process.stderr.readline()
            match = re.search(r'http://127.0.0.1:\d+', line)
            assert match, line
            return process, match.group()

        server, api = start_api()
        def cli(*arguments):
            result = subprocess.run([*operator_command(binary), '--url', api,
                '--api-token-env', 'HUDSON_CUSTOMER_TOKEN', *arguments],
                env=dict(os.environ, HUDSON_CUSTOMER_TOKEN=token), capture_output=True,
                text=True, timeout=15)
            assert result.returncode == 0, result.stderr
            assert token not in result.stdout + result.stderr
            return json.loads(result.stdout)
        assert cli('capabilities')['models']
        assert HudsonClient(api, token=token).capabilities()['models']
        try:
            HudsonClient(api, token='forged').capabilities()
        except HttpError as error:
            assert error.status == 401
        else:
            raise AssertionError('client accepted invalid credential')
        expect('GET', '/capabilities', code=401, bearer='forged')
        other_scope = [value if value != 'company' else 'other-company' for value in scope]
        other = subprocess.run([str(binary / 'hudson-credentials'), *other_scope, 'issue',
            '--label', 'other', '--ttl-seconds', '300'], capture_output=True, text=True, timeout=10)
        assert other.returncode == 0, other.stderr
        expect('GET', '/capabilities', code=403, bearer=json.loads(other.stdout)['token'])
        capabilities = expect('GET', '/capabilities')
        assert 'endpoint' not in json.dumps(capabilities)
        assert 'KEY' not in json.dumps(capabilities)
        spec = expect('GET', '/openapi.json')
        assert spec['security'] == [{'HudsonBearer': []}]
        assert spec['paths']['/runs']['post']['requestBody']['content']['application/json']['schema']['$ref'].endswith('/PublishedSubmission')
        tool = {'name': 'lookup', 'version': 1, 'description': 'Read the customer record',
                'connection': {'id': 'crm', 'version': 1},
                'input_schema': {'type': 'object', 'properties': {'customer_id': {'type': 'integer'}},
                                 'required': ['customer_id'], 'additionalProperties': False},
                'output_schema': {'type': 'object', 'required': ['record_id']}}
        expect('POST', '/tools', {'request_key': 'denied', 'tool': dict(tool, effect='read')}, code=403)
        expect('POST', '/tools', {'request_key': 'denied', 'tool': dict(tool, require_approval=False)}, code=403)
        expect('POST', '/tools', {'request_key': 'denied', 'tool': dict(tool, token_env='BORROWED')}, code=422)
        injected = copy.deepcopy(tool)
        injected['connection']['workspace_id'] = 'other'
        expect('POST', '/tools', {'request_key': 'denied', 'tool': injected}, code=422)
        body = {'request_key': 'tool-one', 'tool': tool}
        with concurrent.futures.ThreadPoolExecutor(max_workers=3) as executor:
            receipts = list(executor.map(lambda _: expect('POST', '/tools', body, code=201), range(3)))
        receipt = receipts[0]
        assert all(item == receipt for item in receipts)
        assert expect('POST', '/tools', body, code=201) == receipt
        assert HudsonClient(api, token=token).publish_tool(tool, request_key='tool-one') == receipt
        tool_path = pathlib.Path(directory) / 'tool.json'
        tool_path.write_text(json.dumps(body))
        assert cli('publish-tool', str(tool_path)) == receipt
        expect('POST', '/tools', {'request_key': 'changed', 'tool': dict(tool, description='changed')}, code=409)
        assert expect('GET', '/tools/lookup/versions/1')['name'] == 'lookup'
        agent = {'name': 'customer-agent', 'version': 1, 'instructions': 'Look up the record and finish',
                 'model_profile': {'id': 'standard', 'version': 1}, 'tools': [{'id': 'lookup', 'version': 1}],
                 'capabilities': {'context': False, 'memory': False, 'user_input': False},
                 'input_schema': {'type': 'object', 'required': ['task']},
                 'output_schema': {'type': 'string'}}
        expect('POST', '/agents', {'request_key': 'denied', 'agent': dict(agent, endpoint=endpoint)}, code=422)
        expect('POST', '/agents', {'request_key': 'denied', 'agent': dict(agent, model_budget=3)}, code=403)
        expect('POST', '/agents', {'request_key': 'denied', 'agent': dict(agent, tools=[{'id': 'unknown', 'version': 1}])}, code=404)
        publication = HudsonClient(api, token=token).publish_agent(agent, request_key='agent-one')
        assert expect('POST', '/agents', {'request_key': 'agent-one', 'agent': agent}, code=201) == publication
        agent_path = pathlib.Path(directory) / 'agent.json'
        agent_path.write_text(json.dumps({'request_key': 'agent-one', 'agent': agent}))
        assert cli('publish-agent', str(agent_path)) == publication
        assert cli('agent', 'customer-agent', '1')['name'] == 'customer-agent'
        assert cli('tool', 'lookup', '1')['name'] == 'lookup' 
        assert expect('GET', '/agents/customer-agent/versions/1')['tools'] == agent['tools']
        assert observed['models'] == 0 and not observed['writes'], 'publication performed execution'
        task = {'agent_ref': publication['agent_ref'], 'input': {'task': 'first'}, 'request_key': 'first'}
        expect('POST', '/runs', dict(task, workspace_id='other'), code=422)
        expect('POST', '/runs', {'input': {}, 'request_key': 'missing-agent'}, code=400)
        first = HudsonClient(api, token=token).start(task['input'], request_key='first',
                    agent_ref=task['agent_ref'])['run_id']
        assert expect('POST', '/runs', task, code=202)['run_id'] == first
        input_path = pathlib.Path(directory) / 'input.json'
        input_path.write_text(json.dumps(task['input']))
        assert cli('start', '--agent', 'customer-agent', '--agent-version', '1', '--request-key', 'first', '--input-file', str(input_path))['run_id'] == first
        expect('POST', '/runs', dict(task, input={'task': 'changed'}), code=409)
        stop(server, graceful=True)
        work = worker()
        server, api = start_api()
        waiting = until(first, 'waiting', work)
        wait_records = list(HudsonClient(api, token=token).follow(first, page_size=1))
        assert wait_records[-1]['run']['status'] == 'waiting'
        after = wait_records[-1]['after']
        operation = waiting['wait']['operation_id']
        assert expect('GET', '/operations/' + operation)['arguments'] == {'customer_id': 42}
        assert not observed['writes'], 'write ran without approval'
        stop(work)
        HudsonClient(api, token=token).approve(operation, approved=True)
        assert not observed['writes'], 'API executed the customer tool'
        work = worker()
        assert until(first, 'completed', work)['result'] == 'Record 42 verified'
        finished = list(HudsonClient(api, token=token).follow(first, after=after, page_size=1))
        assert finished[-1]['run']['result'] == 'Record 42 verified'
        reconnect = list(HudsonClient(api, token=token).follow(first,
                         after=finished[-1]['after'], page_size=1))
        assert [record['type'] for record in reconnect] == ['run']
        # A second task on the same immutable revision gets a fresh two-call budget.
        second_task = dict(task, request_key='second', input={'task': 'second'})
        second = expect('POST', '/runs', second_task, code=202)['run_id']
        waiting = until(second, 'waiting', work)
        expect('POST', '/operations/' + waiting['wait']['operation_id'] + '/approval', {'approved': True})
        until(second, 'completed', work)
        assert expect('POST', '/runs', task, code=202)['run_id'] == first
        assert observed['models'] == 4 and len(observed['writes']) == 2, observed
        assert not observed['errors'], observed
        stop(work)
        stop(server, graceful=True)
    print('PASS: authenticated customer tools and agents, immutable retries, policy floors, no admission secrets/IO, API restart, separate worker restart, approved customer effects, and independent per-task budgets')
finally:
    for process in list(processes):
        stop(process)
    fixture.shutdown()
    fixture.server_close()
    thread.join(timeout=5)
