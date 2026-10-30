"""Claude Agent SDK model-step peer with host admission and immutable receipts.

SPDX-License-Identifier: Apache-2.0
"""
import argparse
import importlib.util
import os
import pathlib
import stat
import sys
import tempfile
import uuid

ROOT = pathlib.Path(__file__).resolve().parent
SDK_VERSION, CLI_VERSION = '0.2.175', '2.1.314'


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


gateway = load('hudson_claude_messages', ROOT / 'gateway.py')
common = load('hudson_claude_state', ROOT.parent / 'agent_sdk/peer.py')
rpc = load('hudson_claude_rpc', ROOT.parent / 'codex/client.py')


def environment(home, proxy, allocation):
    result = {key: value for key, value in os.environ.items()
              if key.upper() in ('PATH', 'SYSTEMROOT', 'WINDIR', 'TMP', 'TEMP')}
    result.update(HOME=str(home), CLAUDE_CONFIG_DIR=str(home / 'config'),
        XDG_CONFIG_HOME=str(home / 'xdg'), ANTHROPIC_API_KEY=proxy.token,
        ANTHROPIC_BASE_URL=proxy.base_url, CLAUDE_CODE_MAX_OUTPUT_TOKENS=str(allocation),
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1', CLAUDE_CODE_DISABLE_AUTO_MEMORY='1',
        CLAUDE_CODE_DISABLE_BACKGROUND_TASKS='1', CLAUDE_CODE_DISABLE_AUTO_COMPACT='1',
        DISABLE_AUTOUPDATER='1', DISABLE_TELEMETRY='1', DISABLE_ERROR_REPORTING='1',
        GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull)
    return result


def read_receipt(home):
    fd = os.open(home / 'native.json', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as source:
        metadata = os.fstat(source.fileno())
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or metadata.st_nlink != 1 or stat.S_IMODE(metadata.st_mode) & 0o077):
            raise ValueError('invalid native SDK receipt ownership')
        raw = source.read(common.SNAPSHOT_BYTES + 1)
    if len(raw) > common.SNAPSHOT_BYTES:
        raise ValueError('native SDK receipt ceiling')
    return gateway.decode(raw)


def validate_native(native, response, aliases):
    if (native.get('sdk_version') != SDK_VERSION or native.get('invocations') != 0
            or native.get('failure') or len(native.get('results', [])) != 1
            or not isinstance(native.get('entries'), list) or len(native['entries']) > 8192):
        raise ValueError('native SDK attempt did not complete safely')
    result = native['results'][0]
    initialization = native.get('init')
    if (not isinstance(initialization, list) or len(initialization) != 1
            or initialization[0].get('claude_code_version') != CLI_VERSION
            or set(initialization[0].get('tools', [])) != set(aliases)
            or initialization[0].get('additional_directories') != []):
        raise ValueError('native Claude version or tool catalog changed')
    session_id = str(uuid.UUID(result['session_id']))
    if initialization[0].get('session_id') != session_id:
        raise ValueError('native Claude initialization identity changed')
    key = native.get('key')
    if not isinstance(key, dict) or key.get('session_id') != session_id or key.get('subpath') is not None:
        raise ValueError('native SDK snapshot identity mismatch')
    assistants = []
    for item in native['assistants']:
        if item.get('error') is not None or item.get('message_id') != response['id']:
            raise ValueError('native SDK assistant identity mismatch')
        assistants.extend(item['content'])
    if assistants != response['content']:
        raise ValueError('native SDK response receipt changed')
    calls = [block for block in response['content'] if block['type'] == 'tool_use']
    pending = {block['id'] for block in calls}
    recorded = []
    for entry in native['entries']:
        if entry.get('sessionId') not in (None, session_id):
            raise ValueError('native Claude snapshot contains another session')
        message = entry.get('message', {})
        blocks = message.get('content', []) if isinstance(message, dict) else []
        if not isinstance(blocks, list):
            continue
        if message.get('role') == 'assistant' and message.get('id') == response['id']:
            recorded.extend(blocks)
        if any(block.get('type') == 'tool_result' and block.get('tool_use_id') in pending for block in blocks):
            raise ValueError('native SDK synthesized an unexecuted tool receipt')
    if recorded != response['content']:
        raise ValueError('native SDK snapshot lacks the exact provider response')
    if calls:
        expected = [{'name': block['name'], 'input': block['input'], 'id': block['id']} for block in calls]
        if (native['hooks'] != expected or result.get('stop_reason') != 'tool_deferred'
                or result.get('deferred_tool_use') != expected[0]):
            raise ValueError('native SDK did not defer the exact host tool')
    elif native['hooks'] or result.get('is_error') or result.get('stop_reason') != 'end_turn':
        raise ValueError('native SDK final result did not complete')
    return session_id


def model_response(response, by_alias):
    calls = [{'call_id': block['id'], 'name': by_alias[block['name']], 'arguments': block['input']}
             for block in response['content'] if block['type'] == 'tool_use']
    text = ''.join(block['text'] for block in response['content'] if block['type'] == 'text')
    if calls:
        calls[0]['provider_metadata'] = {'claude_assistant_content': response['content']}
        if text:
            calls[0]['provider_metadata']['chat_assistant_text'] = text
        return {'type': 'tool_calls', 'calls': calls}, text
    if not any(block['type'] == 'text' for block in response['content']):
        raise ValueError('native SDK final output has no supported text')
    try:
        output = gateway.decode(text)
    except (ValueError, UnicodeError):
        output = text
    return {'type': 'final', 'output': output}, text


def run(body, saved):
    context, request = body['context'], body['request']
    if body.get('reasoning_effort') is not None:
        raise ValueError('native Claude reasoning settings require explicit acceptance')
    if saved is not None:
        previous = context['previous']
        if (saved['sdk_version'] != SDK_VERSION or saved['cli_version'] != CLI_VERSION
                or saved['runtime_id'] != body['runtime_id'] or saved['runtime_version'] != body['runtime_version']
                or saved['request_fingerprint'] != previous['request_fingerprint']
                or saved['input_messages'] != previous['input_messages']):
            raise ValueError('native Claude checkpoint binding changed')
    endpoint = os.environ.get('HUDSON_CLAUDE_ENDPOINT', 'https://api.anthropic.com/v1/messages')
    python = pathlib.Path(os.environ['HUDSON_CLAUDE_PYTHON'])
    with gateway.OneRequestGateway(endpoint, os.environ['ANTHROPIC_API_KEY'], request, body['max_output_tokens']) as proxy:
        with tempfile.TemporaryDirectory(prefix='hudson-claude-attempt-') as temporary:
            home = pathlib.Path(temporary)
            (home / 'instructions.txt').write_text(request['instructions'], encoding='utf-8')
            config = {'model': request['model'], 'tools': [dict(tool,
                alias=gateway.tool_alias(tool['name']).removeprefix('mcp__hudson__')) for tool in request['tools']]}
            (home / 'request.json').write_bytes(gateway.encode(config))
            with rpc.RpcProcess(python, ['-I', str(ROOT / 'driver.py')], home,
                                environment(home, proxy, body['max_output_tokens']), timeout=120) as client:
                if client.receive() != {'type': 'complete'} or client.process.wait(timeout=client.remaining()) != 0:
                    raise ValueError('native Claude child did not complete')
                if client.error is not None or not proxy.completed.wait(timeout=1) or proxy.failure is not None or proxy.requests != 1:
                    raise ValueError('native Claude provider accounting mismatch')
                native = read_receipt(home)
                session_id = validate_native(native, proxy.response, proxy.by_alias)
            response, text = model_response(proxy.response, proxy.by_alias)
            checkpoint = dict(context['call'], request_fingerprint=context['request_fingerprint'],
                              input_messages=len(request['messages']))
            reply = {'protocol_version': 1, 'runtime_id': body['runtime_id'], 'runtime_version': body['runtime_version'],
                     'response': response, 'usage': gateway.usage(proxy.response['usage']), 'checkpoint': checkpoint}
            return reply, dict(native, native_session_id=session_id), text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args()
    raw = sys.stdin.buffer.read(common.REQUEST_BYTES + 1)
    if len(raw) > common.REQUEST_BYTES:
        raise ValueError('native Claude request ceiling')
    body = gateway.decode(raw)
    if body['protocol_version'] != 1 or body.get('context') is None:
        raise ValueError('admitted native context required')
    context = body['context']
    checkpoints = common.Checkpoints(context, root_env='HUDSON_CLAUDE_STATE_DIR')
    try:
        try:
            cached = checkpoints.read(context['call'])
        except FileNotFoundError:
            cached = None
        if cached is not None:
            if (cached['request_fingerprint'] != context['request_fingerprint'] or cached['runtime_id'] != body['runtime_id']
                    or cached['runtime_version'] != body['runtime_version'] or cached['sdk_version'] != SDK_VERSION
                    or cached['cli_version'] != CLI_VERSION):
                raise ValueError('native Claude replay binding changed')
            reply, text = cached['reply'], ''
        else:
            checkpoints.begin(context['call'])
            previous = context.get('previous')
            saved = checkpoints.read(previous) if previous is not None and context['history_mode'] == 'append' else None
            reply, native, text = run(body, saved)
            checkpoints.save(context['call'], dict(native, sdk_version=SDK_VERSION, cli_version=CLI_VERSION,
                runtime_id=body['runtime_id'], runtime_version=body['runtime_version'],
                request_fingerprint=context['request_fingerprint'], input_messages=len(body['request']['messages']), reply=reply))
            checkpoints.completed(context['call'])
        if body.get('stream'):
            frames = [{'type': 'start', 'protocol_version': 1, 'runtime_id': body['runtime_id'], 'runtime_version': body['runtime_version']}]
            if text:
                frames.append({'type': 'text_delta', 'text': text})
            frames.append({'type': 'response', 'result': reply})
            output = b''.join(common.encode(frame, common.RESPONSE_BYTES) + b'\n' for frame in frames)
        else:
            output = common.encode(reply, common.RESPONSE_BYTES)
        if len(output) > common.RESPONSE_BYTES:
            raise ValueError('native Claude response ceiling')
        sys.stdout.buffer.write(output)
        sys.stdout.buffer.flush()
    finally:
        checkpoints.close()


if __name__ == '__main__':
    try:
        main()
    except Exception:
        sys.stderr.write('Native Claude attempt failed; no automatic retry is authorized.\n')
        raise SystemExit(1) from None
