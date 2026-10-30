"""Installed app-server account readiness/discovery; no login or model requests.

SPDX-License-Identifier: Apache-2.0
"""
import argparse
import importlib.util
import json
import os
import pathlib
import tempfile

ROOT=pathlib.Path(__file__).resolve().parent.parent
spec=importlib.util.spec_from_file_location('native_account_rpc',ROOT/'integrations/codex/client.py')
rpc=importlib.util.module_from_spec(spec)
spec.loader.exec_module(rpc)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex',required=True)
    parser.add_argument('--python',required=True)
    args=parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='hudson-account-smoke-') as temporary:
        os.chmod(temporary,0o700)
        env={key:value for key,value in os.environ.items() if key.upper() in ('SYSTEMROOT','WINDIR','PATH','TEMP','TMP')}
        env.update(HUDSON_CODEX_EXECUTABLE=args.codex,HUDSON_CODEX_ACCOUNT_DIR=temporary)
        with rpc.RpcProcess(args.python,['-I',str(ROOT/'integrations/codex/accounts.py')],str(ROOT),env,timeout=30) as process:
            for action in ('status','models','renew','logout'):
                print('Checking native account '+action,flush=True)
                process.send(dict(protocol_version=1,account_id='native',runtime_id='codex',runtime_version=1,
                    actor={'workspace_id':'fixture','id':'owner'},request_key=action,action=action))
                response=process.receive()['response']
                if action in ('status','renew'):
                    assert response['status']['protocol_ready']
                    assert response['status']['credential']=='required'
                elif action=='models':
                    assert response['type']=='unsupported'
                else: assert response['type']=='logged_out'
        assert not list(pathlib.Path(temporary).rglob('auth.json'))
    print('PASS: installed native account/read, unauthenticated discovery gate, explicit unauthenticated renewal and logout; no login or model requests')


if __name__=='__main__': main()
