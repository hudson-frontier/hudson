"""Local SDK package/credential readiness in the bounded account protocol.

API keys rotate outside Hudson; no login, refresh or provider request is implied.
SPDX-License-Identifier: Apache-2.0
"""
import importlib.metadata
import importlib.util
import os
import pathlib
import sys

spec = importlib.util.spec_from_file_location('hudson_account_protocol',
    pathlib.Path(__file__).resolve().parent.parent/'codex/accounts.py')
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)


class SdkReadiness:
    def __init__(self, _binding):
        pass

    def action(self, action):
        if action != 'status':
            return {'type':'unsupported'}
        try:
            version = importlib.metadata.version('openai-agents')
        except importlib.metadata.PackageNotFoundError:
            version = None
        return {'type':'status','status':{'expected_version':protocol.peer.common.SDK_VERSION,
            'installed_version':version,'protocol_ready':version == protocol.peer.common.SDK_VERSION,
            'credential':'configured' if os.environ.get('OPENAI_API_KEY') else 'required','login':'none'}}

    def close(self):
        pass


if __name__ == '__main__':
    try:
        protocol.serve(sys.stdin.buffer,sys.stdout.buffer,SdkReadiness)
    except Exception:
        sys.stderr.write('SDK readiness failed.\n')
        raise SystemExit(1) from None
