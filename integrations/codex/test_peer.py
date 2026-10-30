"""Native continuation and display/privacy regressions.

SPDX-License-Identifier: Apache-2.0
"""
import importlib.util
import json
import os
import pathlib
import tempfile
import unittest
import uuid
import tomllib
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('codex_peer', pathlib.Path(__file__).with_name('peer.py'))
peer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peer)


class ProtocolTests(unittest.TestCase):
    def test_private_profile_disables_background_state_and_does_not_inherit_credentials(self):
        class Gateway:
            base_url = 'http://127.0.0.1:1/v1'
            token = 'synthetic-child-token'
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {
                'OPENAI_API_KEY':'must-stay-in-parent','SSH_AUTH_SOCK':'private-agent',
                'GIT_CONFIG_GLOBAL':'private-global-config'}):
            home = pathlib.Path(temporary)
            child = peer.configuration(home, Gateway(), 'fixture')
            self.assertNotIn('OPENAI_API_KEY', child)
            self.assertNotIn('SSH_AUTH_SOCK', child)
            self.assertEqual(child['GIT_CONFIG_GLOBAL'], os.devnull)
            config = tomllib.loads((home/'config.toml').read_text())
            for feature in ('shell_snapshot','workspace_dependencies','hooks','plugins','multi_agent',
                            'goals','daemon_auto_start','unbounded_connection_retries'):
                self.assertFalse(config['features'][feature])
            self.assertEqual(child['HUDSON_CODEX_PROXY_TOKEN'], Gateway.token)

    def test_native_aliases_do_not_turn_literal_json_into_images(self):
        literal = {'type':'image','image':{'media_type':'image/png','data':'YQ=='}}
        messages = [{'role':'user','content':[{'type':'json','value':literal}]},
                    {'role':'assistant','content':[{'type':'tool_call','call':{
                        'name':'view_image','call_id':'c','arguments':{'value':1}}}]}]
        items = peer.input_items(messages)
        self.assertEqual(items[0]['type'], 'message')
        self.assertEqual(items[0]['content'][0]['type'], 'input_text')
        self.assertEqual(json.loads(items[0]['content'][0]['text']), literal)
        self.assertEqual(items[1]['name'], peer.gateway.tool_alias('view_image'))
        self.assertNotEqual(items[1]['name'], 'view_image')

    def test_analysis_and_reasoning_are_not_displayed_or_promoted_to_final_text(self):
        response = {'output':[
            {'type':'reasoning','summary':[{'type':'summary_text','text':'private'}]},
            {'type':'message','phase':'analysis','content':[{'type':'output_text','text':'private analysis'}]},
            {'type':'message','phase':'final_answer','content':[{'type':'output_text','text':'42'}]},
        ]}
        result, text = peer.model_response(response, {})
        self.assertEqual(result, {'type':'final','output':42})
        self.assertEqual(text, '42')


@unittest.skipUnless(os.name == 'posix', 'native private Unix files')
class StateTests(unittest.TestCase):
    def test_checkpoint_rejects_synthetic_tool_output_created_during_shutdown(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = pathlib.Path(temporary)
            (home/'sessions').mkdir()
            thread_id = str(uuid.uuid4())
            alias = peer.gateway.tool_alias('lookup')
            call = {'type':'function_call','call_id':'c','name':alias,'arguments':'{}'}
            records = [{'type':'session_meta','payload':{'id':thread_id}},
                {'type':'response_item','payload':call}, {'type':'response_item','payload':{
                    'type':'function_call_output','call_id':'c','output':'cancelled'}}]
            path = home/'sessions/native.jsonl'
            path.write_text(''.join(json.dumps(record)+'\n' for record in records))
            with self.assertRaises(ValueError):
                peer.capture(home, thread_id, {'output':[call]})
