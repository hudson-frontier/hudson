"""Standard-library state/protocol regressions; SDK execution is tested separately."""
import asyncio
import concurrent.futures
import importlib.util
import json
import os
import pathlib
import tempfile
import threading
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('native_peer', pathlib.Path(__file__).with_name('peer.py'))
peer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(peer)


def context(actor='owner'):
    return {'actor': {'workspace_id': 'workspace', 'id': actor},
            'call': {'run_id': str(uuid.uuid4()), 'operation_id': str(uuid.uuid4()), 'attempt_id': str(uuid.uuid4())}}


@unittest.skipUnless(os.name == 'posix', 'private Unix checkpoint files')
class StateTests(unittest.TestCase):
    def test_concurrent_attempts_reserve_one_remaining_slot_atomically(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {'HUDSON_SDK_STATE_DIR': directory}), patch.object(peer, 'MAX_SNAPSHOTS', 1):
            barrier = threading.Barrier(2)
            def admit(_):
                owner = context()
                store = peer.Checkpoints(owner)
                try:
                    # Both callers observe capacity before racing admission.
                    self.assertTrue(store.has_capacity())
                    barrier.wait(timeout=5)
                    try:
                        store.begin(owner['call'])
                        return True
                    except ValueError:
                        return False
                finally:
                    store.close()
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                self.assertEqual(sum(pool.map(admit, range(2))), 1)

    def test_checkpoint_publication_is_immutable_and_actor_scoped(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {'HUDSON_SDK_STATE_DIR': directory}):
            owner = context()
            store = peer.Checkpoints(owner)
            try:
                store.save(owner['call'], {'value': 42})
                self.assertEqual(store.read(owner['call']), {'value': 42})
                store.begin(owner['call'])
                with self.assertRaises(FileExistsError):
                    store.begin(owner['call'])
                store.completed(owner['call'])
                with self.assertRaises(FileExistsError):
                    store.save(owner['call'], {'value': 'changed'})
                self.assertEqual(store.read(owner['call']), {'value': 42})
                other = peer.Checkpoints(context('other'))
                try:
                    with self.assertRaises(FileNotFoundError):
                        other.read(owner['call'])
                finally:
                    other.close()
            finally:
                store.close()

    def test_symlinks_and_oversized_encodings_fail_closed(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {'HUDSON_SDK_STATE_DIR': directory}):
            owner = context()
            store = peer.Checkpoints(owner)
            try:
                target = pathlib.Path(directory) / 'external'
                target.write_text('{"value":"private"}')
                (store.path / peer.checkpoint_name(owner['call'])).symlink_to(target)
                with self.assertRaises(OSError):
                    store.read(owner['call'])
                (store.path / '.admission.lock').symlink_to(target)
                with self.assertRaises(OSError):
                    store.begin(owner['call'])
                with self.assertRaises(ValueError):
                    peer.encode({'value': 'x' * 1000}, 128)
            finally:
                store.close()


class ProtocolTests(unittest.TestCase):
    def test_plain_json_is_not_promoted_to_an_image_and_append_skips_saved_assistant_items(self):
        value = {'type': 'image', 'image': {'media_type': 'image/png', 'data': 'YQ=='}}
        items = peer.input_items([{'role': 'user', 'content': [{'type': 'json', 'value': value}]}])
        self.assertEqual(items[0]['content'][0]['type'], 'input_text')
        self.assertEqual(json.loads(items[0]['content'][0]['text']), value)
        items = peer.input_items([{'role': 'assistant', 'content': [{'type': 'text', 'text': 'saved'}]},
                                 {'role': 'user', 'content': [{'type': 'text', 'text': 'new'}]}], append=True)
        self.assertEqual(len(items), 1)
        self.assertEqual(items[0]['content'][0]['text'], 'new')

    def test_native_session_item_bound_is_enforced_before_mutation(self):
        session = peer.NativeSession('fixture', [{'role': 'user', 'content': 'first'}])
        with patch.object(peer, 'MAX_ITEMS', 1), self.assertRaises(ValueError):
            asyncio.run(session.add_items([{'role': 'user', 'content': 'too much'}]))
        self.assertEqual(len(session.items), 1)


if __name__ == '__main__':
    unittest.main()
