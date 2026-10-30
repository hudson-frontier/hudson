"""Explicit, scoped native account lifecycle; no model requests or token export.

The enclosing contained host owns the process tree and a maximum 300-second
session. A managed browser login needs this live process until completion.
SPDX-License-Identifier: Apache-2.0
"""
import hashlib
import json
import os
import pathlib
import re
import signal
import stat
import sys
import urllib.parse

# Isolated interpreter mode deliberately ignores the working directory.
import importlib.util
ROOT = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('hudson_codex_account_peer', ROOT/'peer.py')
peer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peer)

REQUEST_BYTES = 8192
RESPONSE_BYTES = 64 * 1024
ACTIONS = {'status', 'login', 'renew', 'cancel_login', 'logout', 'models'}


def request(raw):
    value = peer.common.decode(raw)
    if (not isinstance(value, dict) or set(value) != {'protocol_version', 'account_id',
            'runtime_id', 'runtime_version', 'actor', 'request_key', 'action'}
            or type(value['protocol_version']) is not int or value['protocol_version'] != 1
            or type(value['runtime_version']) is not int or value['runtime_version'] <= 0
            or value['action'] not in ACTIONS):
        raise ValueError('invalid account request')
    for field in ('account_id', 'runtime_id'):
        if not isinstance(value[field], str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,64}', value[field]):
            raise ValueError('invalid account identity')
    actor = value['actor']
    if (not isinstance(actor, dict) or set(actor) != {'workspace_id', 'id'}
            or any(not isinstance(v, str) or not v.strip() or len(v.encode()) > 256
                   or any(ord(c) < 32 for c in v) for v in actor.values())):
        raise ValueError('invalid account owner')
    key = value['request_key']
    if not isinstance(key, str) or not key.strip() or len(key.encode()) > 128 or any(ord(c) < 32 for c in key):
        raise ValueError('invalid account request key')
    return value


def scope(value):
    return {key:value[key] for key in ('account_id', 'runtime_id', 'runtime_version', 'actor')}


def private_file(directory, name, flags):
    fd = os.open(directory/name, flags | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    metadata = os.fstat(fd)
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
            or metadata.st_nlink != 1 or stat.S_IMODE(metadata.st_mode) & 0o077):
        os.close(fd)
        raise ValueError('invalid account file ownership')
    return fd


class NativeAccount:
    def __init__(self, binding):
        import fcntl
        root = peer.common.private_directory(pathlib.Path(os.environ['HUDSON_CODEX_ACCOUNT_DIR']))
        digest = hashlib.sha256(json.dumps(binding,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()
        self.home = peer.common.private_directory(root/digest)
        self.lock = private_file(self.home, '.account.lock', os.O_WRONLY | os.O_CREAT)
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            for name in ('auth.json', 'config.toml'):
                try:
                    fd = private_file(self.home, name, os.O_RDONLY)
                except FileNotFoundError:
                    continue
                else:
                    os.close(fd)
            config = '\n'.join([
                'model = "account-control-only"', 'model_provider = "hudson_account_control"',
                'cli_auth_credentials_store = "file"', 'check_for_update_on_startup = false',
                'project_doc_max_bytes = 0', '[features]', 'shell_tool = false',
                'shell_snapshot = false', 'workspace_dependencies = false', 'hooks = false',
                'plugins = false', 'apps = false', 'multi_agent = false', 'goals = false',
                'daemon_auto_start = false', 'skill_mcp_dependency_install = false',
                'skip_host_skill_discovery = true', 'unified_exec = false',
                'unbounded_connection_retries = false',
                'browser_use = false', 'computer_use = false', 'image_generation = false',
                '[analytics]', 'enabled = false', '[feedback]', 'enabled = false',
                '[model_providers.hudson_account_control]', 'name = "Account control only"',
                'base_url = "http://127.0.0.1:1/v1"', 'wire_api = "responses"',
                'requires_openai_auth = false', 'supports_websockets = false',
                'request_max_retries = 0', 'stream_max_retries = 0',
            ])+'\n'
            fd = private_file(self.home, 'config.toml', os.O_WRONLY | os.O_CREAT)
            with os.fdopen(fd, 'wb') as output:
                output.truncate()
                output.write(config.encode())
            env = {key:value for key,value in os.environ.items()
                   if key.upper() in ('SYSTEMROOT', 'WINDIR', 'PATH', 'TEMP', 'TMP')}
            env.update(CODEX_HOME=str(self.home), HOME=str(self.home),
                GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull)
            self.client = peer.rpc.RpcProcess(os.environ['HUDSON_CODEX_EXECUTABLE'],
                ['app-server', '--listen', 'stdio://'], self.home, env, timeout=300)
            try:
                result = self.client.call('initialize', {'clientInfo':{'name':'hudson_peer','version':'1'},
                    'capabilities':{'experimentalApi':True}}, self.client.deny)
                agent = result.get('userAgent', '').split(' ', 1)[0]
                self.installed_version = agent.removeprefix('hudson_peer/') if agent.startswith('hudson_peer/') else None
                if self.installed_version is not None and not re.fullmatch(r'[A-Za-z0-9._-]{1,64}', self.installed_version):
                    self.installed_version = None
                self.available = self.installed_version == peer.VERSION
                self.client.send({'method':'initialized','params':{}})
            except BaseException:
                self.client.close(force=True)
                raise
            self.login_id = None
            self.login_state = 'none'
        except BaseException:
            os.close(self.lock)
            raise

    def event(self, value):
        self.client.deny(value)
        if value.get('method') == 'account/login/completed':
            params = value.get('params', {})
            if params.get('loginId') == self.login_id:
                self.login_state = 'completed' if params.get('success') is True else 'failed'
                self.login_id = None

    def call(self, method, params):
        return self.client.call(method, params, self.event)

    def action(self, action):
        if not self.available:
            if action == 'status':
                return {'type':'status','status':{'expected_version':peer.VERSION,
                    'installed_version':self.installed_version,'protocol_ready':False,
                    'credential':'unknown','login':'none'}}
            return {'type':'unsupported'}
        if action in ('status', 'renew'):
            result = self.call('account/read', {'refreshToken':action == 'renew'})
            account = result.get('account')
            required = result.get('requiresOpenaiAuth')
            if not isinstance(required, bool) or account is not None and not isinstance(account, dict):
                raise ValueError('invalid native account status')
            # This process manages OpenAI accounts but its deliberately inert
            # model provider never needs credentials or performs a model call.
            credential = 'configured' if account is not None else 'required'
            return {'type':'status', 'status':{'expected_version':peer.VERSION,
                'installed_version':peer.VERSION, 'protocol_ready':True,
                'credential':credential, 'login':self.login_state}}
        if action == 'login':
            if self.login_id is not None:
                raise ValueError('login already pending')
            result = self.call('account/login/start', {'type':'chatgpt'})
            url = result.get('authUrl')
            parsed = urllib.parse.urlsplit(url) if isinstance(url, str) else None
            if (not isinstance(result.get('loginId'), str) or len(result['loginId']) > 128
                    or parsed is None or parsed.scheme != 'https' or not parsed.hostname
                    or parsed.username or parsed.password or parsed.fragment or len(url) > 4096):
                raise ValueError('invalid native login challenge')
            self.login_id, self.login_state = result['loginId'], 'pending'
            return {'type':'login', 'authorization_url':url}
        if action == 'cancel_login':
            if self.login_id is not None:
                self.call('account/login/cancel', {'loginId':self.login_id})
            self.login_id, self.login_state = None, 'cancelled'
            return {'type':'cancelled'}
        if action == 'logout':
            if self.login_id is not None:
                self.action('cancel_login')
            self.call('account/logout', {})
            self.login_state = 'none'
            return {'type':'logged_out'}
        if action == 'models':
            status = self.call('account/read', {'refreshToken':False})
            if status.get('account') is None:
                return {'type':'unsupported'}
            models, cursor = [], None
            for _ in range(4):
                result = self.call('model/list', {'cursor':cursor, 'limit':32})
                rows = result.get('data')
                if not isinstance(rows, list) or len(rows) > 32:
                    raise ValueError('native model page ceiling')
                for model in rows:
                    identifier = model.get('id') if isinstance(model, dict) else None
                    if not isinstance(identifier, str) or not re.fullmatch(r'[A-Za-z0-9._:/-]{1,128}', identifier):
                        raise ValueError('invalid native model identifier')
                    models.append(identifier)
                cursor = result.get('nextCursor')
                if cursor is None:
                    return {'type':'models', 'models':models}
                if not isinstance(cursor, str) or len(cursor) > 4096:
                    raise ValueError('invalid native model cursor')
            raise ValueError('native model discovery page ceiling')
        raise ValueError('unsupported account action')

    def close(self):
        # Host expiry/drop also kills the whole process tree. Closing the native
        # process cancels its pending browser callback; no detached login exists.
        try:
            self.client.close()
        finally:
            os.close(self.lock)


def serve(source, output, factory=NativeAccount):
    account, binding, cached = None, None, {}
    try:
        while True:
            raw = source.readline(REQUEST_BYTES+1)
            if not raw:
                return
            if len(raw) > REQUEST_BYTES or not raw.endswith(b'\n'):
                raise ValueError('account request byte ceiling')
            value = request(raw)
            if binding is None:
                binding = scope(value)
                account = factory(binding)
            elif scope(value) != binding:
                raise ValueError('account owner or runtime binding changed')
            key, action = value['request_key'], value['action']
            if key in cached:
                previous, response = cached[key]
                if previous != action:
                    raise ValueError('account request key changed')
            else:
                if len(cached) >= 32:
                    raise ValueError('account action ceiling')
                response = account.action(action)
                cached[key] = action, response
            result = dict(binding, protocol_version=1, request_key=key, response=response)
            del result['actor']
            output.write(peer.common.encode(result, RESPONSE_BYTES-1)+b'\n')
            output.flush()
    finally:
        if account is not None:
            account.close()


if __name__ == '__main__':
    try:
        def terminate(_signal, _frame):
            raise SystemExit(1)
        signal.signal(signal.SIGTERM, terminate)
        if hasattr(signal, 'SIGALRM'):
            def expire(_signal, _frame):
                raise TimeoutError('account session expired')
            signal.signal(signal.SIGALRM, expire)
            signal.alarm(300)
        os.umask(0o077)
        serve(sys.stdin.buffer, sys.stdout.buffer)
    except Exception:
        sys.stderr.write('Native account control failed; no automatic retry is authorized.\n')
        raise SystemExit(1) from None
