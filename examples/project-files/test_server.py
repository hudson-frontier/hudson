import json
import os
from pathlib import Path
import tempfile
import subprocess
import sys
import threading
import unittest
from urllib.error import HTTPError
from urllib.request import ProxyHandler, Request, build_opener

from server import MAX_BYTES, MAX_ENTRIES, MAX_SCAN, ProjectFiles, make_server


class ProjectTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name).resolve()
        (self.root / 'src').mkdir()
        (self.root / 'src/main.txt').write_text('hello π', encoding='utf-8')
        self.project = ProjectFiles(str(self.root))

    def tearDown(self):
        self.project.close()
        self.temp.cleanup()

    def test_read_and_sorted_listing(self):
        self.assertEqual(self.project.read('src/main.txt'), {'content': 'hello π', 'bytes': 8})
        self.assertEqual(self.project.list('.'), {'entries': [{'name': 'src', 'kind': 'directory'}], 'truncated': False})

    def test_paths_and_reserved_names(self):
        for path in ('/etc/passwd', '../outside', 'src/../main.txt', '', 'src//main.txt',
                     './src', '.git/config', '.env.local', '.ssh/id_rsa', 'key.pem',
                     'credentials.json', 'src\x00'):
            with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                self.project.read(path)
        for name in ('.env', '.git', '.aws', '.netrc', 'key.pem'):
            (self.root / name).write_text('fake fixture secret')
        self.assertEqual(len(self.project.list('.')['entries']), 1)

    def test_symlink_and_hardlink(self):
        outside = self.root.parent / (self.root.name + '-outside')
        outside.write_text('fake fixture secret')
        try:
            (self.root / 'link').symlink_to(outside)
            (self.root / 'dirlink').symlink_to(self.root / 'src', target_is_directory=True)
            os.link(outside, self.root / 'hardlink')
            for path in ('link', 'dirlink/main.txt', 'hardlink'):
                with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                    self.project.read(path)
            self.assertEqual(len(self.project.list('.')['entries']), 1)
            with self.assertRaises(OSError):
                self.project.list('dirlink')
            with self.assertRaises(OSError):
                ProjectFiles(str(self.root / 'dirlink'))
            with self.assertRaises(OSError):
                ProjectFiles(str(self.root / 'dirlink' / 'child'))
        finally:
            outside.unlink()

    def test_root_explicit(self):
        for path in ('src', '.', str(self.root) + '/../'):
            with self.assertRaises(ValueError):
                ProjectFiles(path)

    def test_nonregular_binary_and_bounds(self):
        os.mkfifo(self.root / 'pipe')
        (self.root / 'binary').write_bytes(b'\xff')
        (self.root / 'large').write_bytes(b'x' * (MAX_BYTES + 1))
        for path in ('pipe', 'binary', 'large', 'src'):
            with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                self.project.read(path)
        (self.root / 'limit').write_bytes(b'x' * MAX_BYTES)
        self.assertEqual(self.project.read('limit')['bytes'], MAX_BYTES)

    def test_listing_limits(self):
        for i in range(MAX_ENTRIES + 1):
            (self.root / f'f{i:04}').touch()
        result = self.project.list('.')
        self.assertTrue(result['truncated'])
        self.assertEqual(len(result['entries']), MAX_ENTRIES)
        for i in range(MAX_ENTRIES + 1, MAX_SCAN + 1):
            (self.root / f'f{i:04}').touch()
        with self.assertRaises(ValueError):
            self.project.list('.')

    def test_http_auth_and_refusal(self):
        token = 'fixture-only-token-not-a-real-secret-' + 'x' * 32
        with make_server(self.project, token, 0) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            opener = build_opener(ProxyHandler({}))  # Never inherit external proxy settings.
            def call(path, args, auth=token):
                request = Request(f'http://127.0.0.1:{server.server_port}{path}',
                                  data=json.dumps(args).encode(),
                                  headers={'Authorization': 'Bearer ' + auth}, method='POST')
                with opener.open(request, timeout=2) as response:
                    return json.load(response)
            try:
                self.assertEqual(call('/read', {'path': 'src/main.txt'})['content'], 'hello π')
                self.assertEqual(call('/list', {'path': '.'})['entries'][0]['name'], 'src')
                for args in ({'path': '../fake-secret'}, {'path': []}, {}, {'path': '.', 'extra': 1}):
                    self.assertEqual(call('/read', args), {'error': 'project request refused'})
                with self.assertRaises(HTTPError) as error:
                    call('/read', {'path': 'src/main.txt'}, 'wrong')
                self.assertEqual(error.exception.code, 401)
                self.assertEqual(json.load(error.exception), {'error': 'unauthorized'})
                error.exception.close()
                with self.assertRaises(HTTPError) as error:
                    call('/unknown', {})
                self.assertEqual(error.exception.code, 404)
                error.exception.close()
            finally:
                server.shutdown()
                thread.join(timeout=2)

    def test_startup_failures_are_sanitized(self):
        command = [sys.executable, str(Path(__file__).with_name('server.py'))]
        env = os.environ.copy()
        env.pop('HUDSON_PROJECT_TOKEN', None)
        result = subprocess.run(command + ['--root', str(self.root)], env=env, capture_output=True, text=True, timeout=2)
        self.assertEqual(result.returncode, 2)
        self.assertNotIn(str(self.root), result.stderr)
        self.assertEqual(result.stdout, '')
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=2)
        self.assertEqual(result.returncode, 2)

    def test_token_required(self):
        for token in ('', 'short', 'x' * 32 + '\n', 'π' * 32):
            with self.assertRaises(ValueError):
                make_server(self.project, token, 0)


if __name__ == '__main__':
    unittest.main()
