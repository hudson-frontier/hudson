"""Native receipts must agree with the admitted provider response exactly.

SPDX-License-Identifier: Apache-2.0
"""
import copy
import importlib.util
import os
import pathlib
import tempfile
import unittest
import uuid
from unittest import mock

spec = importlib.util.spec_from_file_location('hudson_claude_peer', pathlib.Path(__file__).with_name('peer.py'))
peer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peer)
ALIAS = peer.gateway.tool_alias('save')


def fixture(calls=True):
    session = str(uuid.uuid4())
    block = {'type': 'tool_use', 'id': 'save-1', 'name': ALIAS, 'input': {'value': 42}} if calls else {'type': 'text', 'text': '42'}
    response = {'id': 'msg-1', 'content': [block]}
    deferred = {'id': 'save-1', 'name': ALIAS, 'input': {'value': 42}}
    native = {'sdk_version': peer.SDK_VERSION, 'invocations': 0, 'failure': False,
        'init': [{'claude_code_version': peer.CLI_VERSION, 'session_id': session, 'tools': [ALIAS], 'additional_directories': []}],
        'results': [{'session_id': session, 'stop_reason': 'tool_deferred' if calls else 'end_turn',
                     'is_error': False, 'deferred_tool_use': deferred if calls else None}],
        'key': {'project_key': 'fixture', 'session_id': session},
        'assistants': [{'message_id': 'msg-1', 'content': [block], 'error': None}],
        'hooks': [deferred] if calls else [],
        'entries': [{'type': 'assistant', 'sessionId': session, 'message': {'role': 'assistant', 'id': 'msg-1', 'content': [block]}}]}
    return native, response


class Tests(unittest.TestCase):
    def test_native_tool_and_final_receipts_match_the_provider(self):
        for calls in (True, False):
            native, response = fixture(calls)
            self.assertEqual(peer.validate_native(native, response, {ALIAS}), native['results'][0]['session_id'])
            result, text = peer.model_response(response, {ALIAS: 'save'})
            if calls:
                self.assertEqual(result['calls'][0]['name'], 'save')
                self.assertEqual(result['calls'][0]['arguments'], {'value': 42})
                self.assertEqual(result['calls'][0]['provider_metadata']['claude_assistant_content'], response['content'])
            else:
                self.assertEqual(result, {'type': 'final', 'output': 42})
                self.assertEqual(text, '42')

    def test_invoked_tools_failed_sdk_and_unadvertised_catalog_are_rejected(self):
        for mutate in ('invocations', 'failure', 'sdk', 'cli', 'catalog', 'session', 'assistant', 'deferred'):
            native, response = fixture()
            if mutate == 'invocations':
                native['invocations'] = 1
            elif mutate == 'failure':
                native['failure'] = True
            elif mutate == 'sdk':
                native['sdk_version'] = 'other'
            elif mutate == 'cli':
                native['init'][0]['claude_code_version'] = 'other'
            elif mutate == 'catalog':
                native['init'][0]['tools'].append('Bash')
            elif mutate == 'session':
                native['entries'][0]['sessionId'] = str(uuid.uuid4())
            elif mutate == 'assistant':
                native['assistants'][0]['message_id'] = 'different'
            else:
                native['results'][0]['deferred_tool_use'] = None
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                peer.validate_native(native, response, {ALIAS})

    def test_synthetic_native_tool_results_are_never_checkpointed_as_execution(self):
        native, response = fixture()
        native['entries'].append({'type': 'user', 'message': {'role': 'user', 'content': [
            {'type': 'tool_result', 'tool_use_id': 'save-1', 'is_error': True, 'content': 'Native synthetic denial'}]}})
        with self.assertRaisesRegex(ValueError, 'synthesized'):
            peer.validate_native(native, response, {ALIAS})

    def test_native_snapshot_cannot_drop_or_rewrite_admitted_content(self):
        native, response = fixture()
        native = copy.deepcopy(native)
        native['entries'][0]['message']['content'][0]['input'] = {'value': 43}
        with self.assertRaises(ValueError):
            peer.validate_native(native, response, {ALIAS})

    def test_real_credentials_and_inherited_profiles_never_reach_the_child(self):
        class Gateway:
            token, base_url = 'ephemeral-local-key', 'http://127.0.0.1:1234'
        with mock.patch.dict(os.environ, {'ANTHROPIC_API_KEY': 'real-secret', 'CLAUDE_CODE_OAUTH_TOKEN': 'login-secret',
                                         'AWS_SECRET_ACCESS_KEY': 'aws-secret', 'HTTP_PROXY': 'http://proxy'}, clear=False):
            env = peer.environment(pathlib.Path('/private'), Gateway(), 32)
        self.assertEqual(env['ANTHROPIC_API_KEY'], 'ephemeral-local-key')
        self.assertNotIn('login-secret', str(env))
        self.assertNotIn('real-secret', str(env))
        self.assertNotIn('aws-secret', str(env))
        self.assertNotIn('HTTP_PROXY', env)
        self.assertEqual(env['CLAUDE_CODE_MAX_OUTPUT_TOKENS'], '32')

    @unittest.skipUnless(os.name == 'posix', 'Native checkpoint receipts require Unix ownership')
    def test_native_receipt_rejects_links_and_wrong_permissions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            path = root / 'native.json'
            path.write_text('{}')
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                peer.read_receipt(root)
            path.chmod(0o600)
            self.assertEqual(peer.read_receipt(root), {})
            path.rename(root / 'target')
            path.symlink_to(root / 'target')
            with self.assertRaises(OSError):
                peer.read_receipt(root)


if __name__ == '__main__':
    unittest.main()
