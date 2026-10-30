"""SDK readiness never mistakes package/key presence for live acceptance.

SPDX-License-Identifier: Apache-2.0
"""
import importlib.util
import json
import os
import pathlib
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('sdk_accounts',pathlib.Path(__file__).with_name('accounts.py'))
accounts=importlib.util.module_from_spec(spec)
spec.loader.exec_module(accounts)


class ReadinessTests(unittest.TestCase):
    def test_local_readiness_does_not_return_credential_values(self):
        sdk=accounts.SdkReadiness({})
        expected=accounts.protocol.peer.common.SDK_VERSION
        with patch.object(accounts.importlib.metadata,'version',return_value=expected),patch.dict(os.environ,{'OPENAI_API_KEY':'private-token'},clear=True):
            result=sdk.action('status')
        self.assertTrue(result['status']['protocol_ready'])
        self.assertEqual(result['status']['credential'],'configured')
        self.assertNotIn('private-token',json.dumps(result))
        with patch.object(accounts.importlib.metadata,'version',return_value='0.0.0'),patch.dict(os.environ,{},clear=True):
            result=sdk.action('status')
        self.assertFalse(result['status']['protocol_ready'])
        self.assertEqual(result['status']['credential'],'required')
        for action in ('login','renew','logout','cancel_login','models'):
            self.assertEqual(sdk.action(action),{'type':'unsupported'})


if __name__=='__main__': unittest.main()
