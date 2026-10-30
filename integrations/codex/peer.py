"""Native file checkpoints with host-owned model admission and tool execution.

SPDX-License-Identifier: Apache-2.0
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
import pathlib
import stat
import sys
import tempfile
import uuid
from collections import deque

ROOT = pathlib.Path(__file__).resolve().parent
VERSION = '0.165.0'
ROLLOUT_BYTES = 4 * 1024 * 1024


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


rpc = module('hudson_codex_rpc', ROOT/'client.py')
gateway = module('hudson_codex_gateway', ROOT/'gateway.py')
common = module('hudson_native_state', ROOT.parent/'agent_sdk/peer.py')


def configuration(home, proxy, model):
    values = [
        'model = '+json.dumps(model), 'model_provider = "hudson"',
        'project_doc_max_bytes = 0', 'model_auto_compact_token_limit = 9223372036854775807',
        '[features]', 'shell_tool = false', 'shell_snapshot = false',
        'workspace_dependencies = false', 'hooks = false', 'plugins = false', 'apps = false',
        'multi_agent = false', 'goals = false', 'daemon_auto_start = false',
        'skill_mcp_dependency_install = false', 'skip_host_skill_discovery = true',
        'unbounded_connection_retries = false', 'unified_exec = false', 'code_mode_host = false',
        'browser_use = false', 'computer_use = false', 'image_generation = false',
        'view_image = false', 'sleep_tool = false',
        '[model_providers.hudson]', 'name = "Hudson"',
        'base_url = '+json.dumps(proxy.base_url), 'wire_api = "responses"',
        'env_key = "HUDSON_CODEX_PROXY_TOKEN"', 'request_max_retries = 0',
        'stream_max_retries = 0', 'requires_openai_auth = false', 'supports_websockets = false',
    ]
    (home/'config.toml').write_text('\n'.join(values)+'\n', encoding='utf-8')
    # The child never receives the real provider credential, inherited account
    # configuration or caller-supplied command-line settings.
    env = {key: value for key, value in os.environ.items()
           if key.upper() in ('SYSTEMROOT', 'WINDIR', 'PATH', 'TEMP', 'TMP')}
    env.update(CODEX_HOME=str(home), HUDSON_CODEX_PROXY_TOKEN=proxy.token)
    # Private native profiles need no global Git hooks, credentials or detached
    # maintenance. Any required local metadata work stays in the owned tree.
    env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull,
        GIT_CONFIG_COUNT='3', GIT_CONFIG_KEY_0='gc.auto', GIT_CONFIG_VALUE_0='0',
        GIT_CONFIG_KEY_1='gc.autoDetach', GIT_CONFIG_VALUE_1='false',
        GIT_CONFIG_KEY_2='maintenance.auto', GIT_CONFIG_VALUE_2='false')
    return env


def initialize(client):
    result = client.call('initialize', {'clientInfo': {'name': 'hudson_peer', 'version': '1'},
        'capabilities': {'experimentalApi': True}}, client.deny)
    if result.get('userAgent', '').split(' ', 1)[0] != 'hudson_peer/'+VERSION:
        raise ValueError('unsupported native executable version')
    client.send({'method': 'initialized', 'params': {}})


def input_items(messages, append=False):
    items = common.input_items(messages, append=append)
    for item in items:
        if 'role' in item:
            item['type'] = 'message'
        if item.get('type') == 'function_call':
            item['name'] = gateway.tool_alias(item['name'])
    return items


def restore(home, saved):
    relative = pathlib.PurePosixPath(saved['rollout_path'])
    if (relative.is_absolute() or not relative.parts or relative.parts[0] != 'sessions'
            or any(part in ('..', '.') for part in relative.parts) or len(str(relative)) > 4096
            or relative.suffix != '.jsonl'):
        raise ValueError('invalid native checkpoint path')
    raw = base64.b64decode(saved['rollout'], validate=True)
    if (len(raw) > ROLLOUT_BYTES or hashlib.sha256(raw).hexdigest() != saved['rollout_sha256']):
        raise ValueError('invalid native checkpoint bytes')
    path = home.joinpath(*relative.parts)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as output:
        output.write(raw)
    return path


def capture(home, thread_id, response):
    paths = []
    for path in (home/'sessions').rglob('*.jsonl'):
        paths.append(path)
        if len(paths) > 1:
            raise ValueError('ambiguous native rollout')
    if len(paths) != 1:
        raise ValueError('native rollout unavailable')
    path = paths[0]
    if not path.resolve().is_relative_to(home.resolve()):
        raise ValueError('native rollout escapes private directory')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1:
            raise ValueError('invalid native rollout ownership')
        raw = source.read(ROLLOUT_BYTES+1)
    if len(raw) > ROLLOUT_BYTES or not raw.endswith(b'\n'):
        raise ValueError('native rollout byte ceiling or incomplete record')
    records = raw.split(b'\n')
    if len(records) > 8192:
        raise ValueError('native rollout record ceiling')
    parsed = [common.decode(line) for line in records if line]
    if not parsed or parsed[0].get('type') != 'session_meta' or parsed[0].get('payload', {}).get('id') != thread_id:
        raise ValueError('native rollout identity mismatch')
    logged = [record['payload'] for record in parsed if record.get('type') == 'response_item']
    pending = {item['call_id'] for item in response['output'] if item['type']=='function_call'}
    if any(item.get('type')=='function_call_output' and item.get('call_id') in pending for item in logged):
        raise ValueError('native shutdown synthesized an unexecuted tool receipt')
    for expected in response['output']:
        if expected['type'] == 'function_call':
            if not any(item.get('type') == 'function_call' and all(item.get(key) == expected.get(key)
                    for key in ('call_id', 'name', 'arguments')) for item in logged):
                raise ValueError('native call was not checkpointed')
        elif expected['type'] == 'message':
            if not any(item.get('type') == 'message' and item.get('role') == 'assistant'
                    and [(block.get('type'), block.get('text')) for block in item.get('content', [])]
                    == [(block['type'], block['text']) for block in expected['content']] for item in logged):
                raise ValueError('native message was not checkpointed')
    return {'rollout_path': path.relative_to(home).as_posix(),
            'rollout': base64.b64encode(raw).decode('ascii'),
            'rollout_sha256': hashlib.sha256(raw).hexdigest(), 'thread_id': thread_id}


def model_response(response, by_alias):
    calls, prefix, visible, last = [], [], [], None
    for item in response['output']:
        if item['type'] == 'function_call':
            call = {'name': by_alias[item['name']], 'call_id': item['call_id'],
                    'arguments': common.decode(item['arguments'])}
            if prefix:
                call['provider_metadata'] = {'sdk_input_prefix': prefix}
            prefix = []
            calls.append(call)
            last = call
        else:
            prefix.append(item)
            if item['type'] == 'message' and item.get('phase') not in ('analysis', 'reasoning'):
                visible.extend(block['text'] for block in item['content'])
    text = ''.join(visible)
    if calls:
        if prefix:
            last.setdefault('provider_metadata', {})['sdk_input_suffix'] = prefix
        if text:
            calls[0].setdefault('provider_metadata', {})['chat_assistant_text'] = text
        return {'type': 'tool_calls', 'calls': calls}, text
    if not visible:
        raise ValueError('native completion has no visible result')
    try:
        output = common.decode(text)
    except ValueError:
        output = text
    return {'type': 'final', 'output': output}, text


def run(body, saved):
    context, request = body['context'], body['request']
    executable = pathlib.Path(os.environ['HUDSON_CODEX_EXECUTABLE'])
    if not executable.is_absolute() or not executable.is_file():
        raise ValueError('native executable unavailable')
    append = saved is not None and context['history_mode'] == 'append'
    if append:
        previous = context['previous']
        if (saved['vendor_version'] != VERSION or saved['runtime_id'] != body['runtime_id']
                or saved['runtime_version'] != body['runtime_version']
                or saved['request_fingerprint'] != previous['request_fingerprint']
                or saved['input_messages'] != previous['input_messages']
                or saved['reply']['checkpoint'] != previous):
            raise ValueError('native continuation binding changed')
    with tempfile.TemporaryDirectory(prefix='hudson-codex-') as temporary:
        home = pathlib.Path(temporary)
        endpoint = os.environ.get('HUDSON_CODEX_ENDPOINT', 'https://api.openai.com/v1/responses')
        with gateway.OneRequestGateway(endpoint, os.environ['OPENAI_API_KEY'], request['model'],
                request['tools'], body['max_output_tokens']) as proxy:
            env = configuration(home, proxy, request['model'])
            with rpc.RpcProcess(executable, ['app-server', '--listen', 'stdio://'], home, env) as client:
                initialize(client)
                parameters = {'model': request['model'], 'modelProvider': 'hudson', 'cwd': str(home),
                    'baseInstructions': request['instructions'], 'approvalPolicy': 'never', 'sandbox': 'read-only'}
                if append:
                    parameters.update(threadId=saved['thread_id'], path=str(restore(home, saved)))
                    result = client.call('thread/resume', parameters, client.deny)
                    delta = input_items(request['messages'][previous['input_messages']:], append=True)
                else:
                    parameters.update(ephemeral=False, experimentalRawEvents=False, dynamicTools=[
                        {'type':'function', 'name':tool['name'], 'description':tool['description'],
                         'inputSchema':tool['input_schema']} for tool in proxy.tools])
                    result = client.call('thread/start', parameters, client.deny)
                    delta = input_items(request['messages'])
                if result.get('modelProvider') != 'hudson' or result.get('model') != request['model']:
                    raise ValueError('native resolved provider binding changed')
                thread_id = str(uuid.UUID(result['thread']['id']))
                client.call('thread/inject_items', {'threadId':thread_id, 'items':delta}, client.deny)
                early = deque()
                def turn_event(event):
                    if event.get('method') in ('item/tool/call', 'turn/completed'):
                        if len(early)>=128:
                            raise ValueError('native callback ceiling')
                        early.append(event)
                    elif 'id' in event:
                        client.deny(event)
                        raise ValueError('unsupported native execution callback')
                    elif event.get('method') == 'error':
                        raise ValueError('native runtime failed')
                started = client.call('turn/start', {'threadId':thread_id, 'input':[]}, turn_event)
                turn_id = started['turn']['id']
                callback = None
                while True:
                    event = early.popleft() if early else client.receive()
                    if event.get('method') == 'item/tool/call':
                        callback = event['params']
                        if callback.get('threadId') != thread_id or callback.get('turnId') != turn_id:
                            raise ValueError('native callback thread changed')
                        break
                    if event.get('method') == 'turn/completed':
                        if (event['params'].get('threadId') != thread_id
                                or event['params']['turn'].get('id') != turn_id
                                or event['params']['turn']['status'] != 'completed'):
                            raise ValueError('native turn did not complete')
                        break
                    if 'id' in event:
                        client.deny(event)
                        raise ValueError('unsupported native execution callback')
                    if event.get('method') == 'error':
                        raise ValueError('native runtime failed')
                if proxy.response is None or proxy.failure is not None:
                    raise ValueError('native provider receipt unavailable')
                response = proxy.response
                if callback is not None:
                    expected = next((item for item in response['output'] if item['type']=='function_call'
                        and item['call_id']==callback.get('callId')), None)
                    if (expected is None or expected['name'] != callback.get('tool')
                            or common.decode(expected['arguments']) != callback.get('arguments')):
                        raise ValueError('native proposal receipt mismatch')
                    # SIGTERM can make the vendor log a synthetic "cancelled"
                    # function output. Preserve the actual pending native call;
                    # only the next admitted host receipt may resolve it.
                    client.close(force=True)
            # Stop the vendor while its customer tool remains unexecuted. A fresh
            # admitted attempt restores the file and injects the host's receipt.
            native = capture(home, thread_id, response)
            result, text = model_response(response, proxy.by_alias)
            checkpoint = dict(context['call'], request_fingerprint=context['request_fingerprint'],
                              input_messages=len(request['messages']))
            usage = response.get('usage')
            reply = {'protocol_version':1, 'runtime_id':body['runtime_id'], 'runtime_version':body['runtime_version'],
                'response':result, 'checkpoint':checkpoint,
                'usage':None if usage is None else {key:usage[key] for key in ('input_tokens','output_tokens')}}
            return reply, native, text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe', action='store_true')
    args = parser.parse_args()
    if args.probe:
        executable = pathlib.Path(os.environ.get('HUDSON_CODEX_EXECUTABLE', ''))
        available = False
        try:
            with tempfile.TemporaryDirectory(prefix='hudson-codex-probe-') as temporary:
                home = pathlib.Path(temporary)
                with gateway.OneRequestGateway('http://127.0.0.1:1/responses', 'probe-only', 'probe', [], 1) as proxy:
                    with rpc.RpcProcess(executable, ['app-server','--listen','stdio://'], home,
                            configuration(home, proxy, 'probe'), timeout=10) as client:
                        initialize(client)
                        available = not proxy.used
        except Exception:
            pass
        print(json.dumps({'runtime':'codex', 'version':VERSION if available else None,
                          'available':available, 'credential_configured':bool(os.environ.get('OPENAI_API_KEY'))}))
        return
    raw = sys.stdin.buffer.read(common.REQUEST_BYTES+1)
    if len(raw)>common.REQUEST_BYTES:
        raise ValueError('native request byte ceiling')
    body = common.decode(raw)
    if body['protocol_version'] != 1 or body.get('context') is None:
        raise ValueError('admitted native context required')
    context = body['context']
    checkpoints = common.Checkpoints(context, root_env='HUDSON_CODEX_STATE_DIR')
    try:
        try:
            cached = checkpoints.read(context['call'])
        except FileNotFoundError:
            cached = None
        if cached is not None:
            if (cached['request_fingerprint'] != context['request_fingerprint']
                    or cached['runtime_id'] != body['runtime_id'] or cached['runtime_version'] != body['runtime_version']
                    or cached['vendor_version'] != VERSION):
                raise ValueError('native replay binding changed')
            reply, text = cached['reply'], ''
        else:
            checkpoints.begin(context['call'])
            previous = context.get('previous')
            saved = checkpoints.read(previous) if previous is not None and context['history_mode']=='append' else None
            reply, native, text = run(body, saved)
            checkpoints.save(context['call'], dict(native, vendor_version=VERSION, runtime_id=body['runtime_id'],
                runtime_version=body['runtime_version'], request_fingerprint=context['request_fingerprint'],
                input_messages=len(body['request']['messages']), reply=reply))
            checkpoints.completed(context['call'])
        if body.get('stream'):
            frames = [{'type':'start','protocol_version':1,'runtime_id':body['runtime_id'],'runtime_version':body['runtime_version']}]
            if text:
                frames.append({'type':'text_delta','text':text})
            frames.append({'type':'response','result':reply})
            output = b''.join(common.encode(frame, common.RESPONSE_BYTES)+b'\n' for frame in frames)
        else:
            output = common.encode(reply, common.RESPONSE_BYTES)
        if len(output)>common.RESPONSE_BYTES:
            raise ValueError('native response byte ceiling')
        sys.stdout.buffer.write(output)
        sys.stdout.buffer.flush()
    finally:
        checkpoints.close()


if __name__ == '__main__':
    try:
        main()
    except Exception:
        sys.stderr.write('Native CLI attempt failed; no automatic retry is authorized.\n')
        raise SystemExit(1)
