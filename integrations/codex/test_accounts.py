"""Scoped account protocol fixtures; never begin a real login or model request.

SPDX-License-Identifier: Apache-2.0
"""
import importlib.util
import io
import json
import os
import pathlib
import tempfile
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('native_accounts',pathlib.Path(__file__).with_name('accounts.py'))
accounts=importlib.util.module_from_spec(spec)
spec.loader.exec_module(accounts)


def body(action='status',key='one',owner='owner'):
    return dict(protocol_version=1,account_id='local',runtime_id='codex',runtime_version=1,
        actor={'workspace_id':'project','id':owner},request_key=key,action=action)


class AccountTests(unittest.TestCase):
    def test_scope_and_retry_keys_are_checked_before_repeating_account_io(self):
        calls=[]
        class Fixture:
            def __init__(self,binding): calls.append(('open',binding))
            def action(self,action):
                calls.append(action)
                return {'type':'login','authorization_url':'https://auth.example.com/?state=private'}
            def close(self): calls.append('close')
        output=io.BytesIO()
        same=json.dumps(body('login')).encode()+b'\n'
        accounts.serve(io.BytesIO(same+same),output,Fixture)
        rows=[json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(rows[0],rows[1])
        self.assertEqual(calls[1:],['login','close'])
        for changed in [body('logout'),body('login',owner='another')]:
            calls.clear()
            with self.assertRaises(ValueError):
                accounts.serve(io.BytesIO(same+json.dumps(changed).encode()+b'\n'),io.BytesIO(),Fixture)
            self.assertEqual(calls[1:],['login','close'])

    def test_native_account_methods_filter_tokens_emails_and_vendor_errors(self):
        native=accounts.NativeAccount.__new__(accounts.NativeAccount)
        native.available=True
        native.login_id=None
        native.login_state='none'
        calls=[]
        def call(method,params):
            calls.append((method,params))
            if method=='account/read':
                return {'account':{'type':'chatgpt','email':'private@example.com','accessToken':'secret-token'},'requiresOpenaiAuth':True}
            if method=='account/login/start':
                return {'loginId':'login-id','authUrl':'https://auth.example.com/?state=challenge','accessToken':'secret-token'}
            if method=='model/list':
                return {'data':[{'id':'fixture-model','email':'private@example.com'}],'nextCursor':None}
            return {}
        native.call=call
        status=native.action('status')
        self.assertEqual(status['status']['credential'],'configured')
        self.assertNotIn('secret-token',json.dumps(status))
        self.assertNotIn('private@example.com',json.dumps(status))
        native.action('renew')
        self.assertEqual(calls[-1],('account/read',{'refreshToken':True}))
        self.assertEqual(native.action('login')['type'],'login')
        native.action('cancel_login')
        self.assertEqual(calls[-1],('account/login/cancel',{'loginId':'login-id'}))
        self.assertEqual(native.action('models'),{'type':'models','models':['fixture-model']})
        native.action('logout')
        self.assertEqual(calls[-1],('account/logout',{}))

    def test_native_model_discovery_and_login_challenges_fail_closed_at_bounds(self):
        native=accounts.NativeAccount.__new__(accounts.NativeAccount)
        native.available=True
        native.login_id=None
        native.call=lambda *_:{'loginId':'id','authUrl':'http://insecure.example.com/'}
        with self.assertRaises(ValueError): native.action('login')
        native.call=lambda method,*_:{'account':{'type':'chatgpt'}} if method=='account/read' else {'data':[],'nextCursor':'again'}
        with self.assertRaises(ValueError): native.action('models')
        native.available=False
        native.installed_version='0.0.0'
        self.assertFalse(native.action('status')['status']['protocol_ready'])
        self.assertEqual(native.action('login'),{'type':'unsupported'})

    def test_protocol_rejects_injected_tokens_actor_fields_and_oversized_requests(self):
        for field in ('access_token','executable','endpoint'):
            value=body();value[field]='private'
            with self.assertRaises(ValueError): accounts.request(json.dumps(value))
        with self.assertRaises(ValueError): accounts.serve(io.BytesIO(b'x'*8193),io.BytesIO())


@unittest.skipUnless(os.name=='posix','private Unix account profiles')
class ProfileTests(unittest.TestCase):
    def test_profiles_are_actor_bound_canonically_locked_and_do_not_inherit_provider_keys(self):
        class RpcFixture:
            def __init__(self,_executable,_args,_cwd,env,**_kwargs):
                self.env=env
            def call(self,method,_params,_event):
                self.method=method
                return {'userAgent':'hudson_peer/'+accounts.peer.VERSION}
            def send(self,_value): pass
            def deny(self,_value): pass
            def close(self,**_kwargs): pass
        with tempfile.TemporaryDirectory() as temporary:
            os.chmod(temporary,0o700)
            with patch.dict(os.environ,{'HUDSON_CODEX_ACCOUNT_DIR':temporary,
                    'HUDSON_CODEX_EXECUTABLE':'/unavailable/fixture','OPENAI_API_KEY':'private-token'},clear=True),\
                    patch.object(accounts.peer.rpc,'RpcProcess',RpcFixture):
                binding=accounts.scope(body())
                first=accounts.NativeAccount(binding)
                try:
                    self.assertNotIn('OPENAI_API_KEY',first.client.env)
                    self.assertEqual(first.client.env['HOME'],str(first.home))
                    reordered=dict(binding,actor={'id':'owner','workspace_id':'project'})
                    with self.assertRaises(BlockingIOError): accounts.NativeAccount(reordered)
                    other=accounts.NativeAccount(accounts.scope(body(owner='another')))
                    try: self.assertNotEqual(first.home,other.home)
                    finally: other.close()
                finally: first.close()
                reopened=accounts.NativeAccount(binding)
                home=reopened.home
                reopened.close()
                (home/'auth.json').symlink_to(pathlib.Path(temporary)/'outside-secret')
                with self.assertRaises(OSError): accounts.NativeAccount(binding)


if __name__=='__main__': unittest.main()
