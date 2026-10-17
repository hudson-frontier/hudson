"""Local HTTP protocol regressions; no provider, database, or Temporal calls."""
import http.server
import json
import os
import threading
import unittest
import urllib.parse
from unittest.mock import patch

from hudson_client import HudsonClient, HudsonError, HttpError

RUN = '11111111-1111-4111-8111-111111111111'
OTHER = '22222222-2222-4222-8222-222222222222'
TOKEN = 'private-test-token'


class ClientTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                self.handle_request()

            def do_POST(self):
                self.handle_request()

            def handle_request(self):
                owner = self.server.owner
                raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
                body = json.loads(raw) if raw else None
                owner.requests.append((self.command, self.path, dict(self.headers), body))
                status, data, headers = owner.respond(self.command, self.path, body)
                encoded = data if isinstance(data, bytes) else json.dumps(data).encode()
                self.send_response(status)
                self.send_header('Content-Length', str(len(encoded)))
                for name, value in headers.items():
                    self.send_header(name, value)
                self.end_headers()
                self.wfile.write(encoded)

        cls.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.url = f'http://127.0.0.1:{cls.server.server_port}'

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=5)

    def setUp(self):
        self.server.owner = self
        self.requests = []
        self.respond = lambda *_: (200, {}, {})
        self.client = HudsonClient(self.url, token=TOKEN)

    def test_rejects_unsafe_urls_and_credentials_before_io(self):
        for url in ['http://example.com', 'ftp://localhost', 'http://localhost@evil.test',
                    'https://user:secret@example.com', 'https://example.com?key=secret',
                    'https://example.com/#fragment', 'https://example.com/api',
                    'https://example.com?', 'https://example.com#',
                    'https://example.com:99999', 'https://example.com\n', 'http://']:
            with self.subTest(url=url), self.assertRaises(ValueError):
                HudsonClient(url, token=TOKEN)
        for token in ['', 'line\nbreak', 'with space', 'non-ascii-é']:
            with self.subTest(token=token), self.assertRaises(ValueError):
                HudsonClient(self.url, token=token)
        with patch.dict(os.environ, {}, clear=True), self.assertRaises(ValueError):
            HudsonClient(self.url, token_env='MISSING_TOKEN')
        self.assertEqual(self.requests, [])

    def test_does_not_forward_auth_on_redirect(self):
        self.respond = lambda *_: (307, b'', {'Location': self.url + '/redirect-target'})
        with self.assertRaises(HttpError) as error:
            self.client.health()
        self.assertEqual(error.exception.status, 307)
        self.assertEqual(len(self.requests), 1)
        self.assertEqual(self.requests[0][2]['Authorization'], 'Bearer ' + TOKEN)

    def test_errors_do_not_expose_echoed_tokens_and_are_not_retried(self):
        for status in (401, 403, 409, 413, 422, 429, 503):
            self.requests.clear()
            self.respond = lambda *_, status=status: (status, TOKEN.encode(), {})
            with self.assertRaises(HttpError) as error:
                self.client.start('task', request_key='same-key')
            self.assertEqual(error.exception.status, status)
            self.assertNotIn(TOKEN, str(error.exception))
            self.assertEqual(len(self.requests), 1)
        self.assertNotIn(TOKEN, repr(self.client))

    def test_explicit_keys_and_publication_shapes(self):
        self.respond = lambda method, path, body: (201, body, {})
        tool = {'name': 'lookup', 'version': 1}
        first = self.client.publish_tool(tool, request_key='tool-one')
        self.assertEqual(first, self.client.publish_tool(tool, request_key='tool-one'))
        self.assertEqual(first, {'tool': tool, 'request_key': 'tool-one'})
        self.client.publish_agent({'name': 'agent'}, request_key='agent-one')
        self.assertEqual(self.requests[-1][1], '/agents')
        self.client.start({'task': 'go'}, request_key='run-one', agent_ref={'id': 'agent', 'version': 1})
        self.assertEqual(self.requests[-1][3]['request_key'], 'run-one')
        before = len(self.requests)
        for key in ('', ' ', None):
            with self.assertRaises(ValueError):
                self.client.start('task', request_key=key)
        with self.assertRaises(ValueError):
            self.client.start('x' * 65536, request_key='large')
        with self.assertRaises(ValueError):
            self.client.start(float('nan'), request_key='invalid')
        self.assertEqual(len(self.requests), before)

    def test_response_byte_limit_and_malformed_success(self):
        self.respond = lambda *_: (200, b'x' * 33, {})
        with self.assertRaisesRegex(HudsonError, 'byte bound'):
            HudsonClient(self.url, max_response_bytes=32).health()
        self.respond = lambda *_: (200, b'not JSON', {})
        with self.assertRaisesRegex(HudsonError, 'valid JSON'):
            self.client.health()
        self.respond = lambda *_: (200, b'NaN', {})
        with self.assertRaisesRegex(HudsonError, 'valid JSON'):
            self.client.health()

    def test_event_pages_reject_other_runs_and_invalid_sequences(self):
        for page in [[{'run_id': OTHER, 'sequence': 2}],
                     [{'run_id': RUN, 'sequence': 1}],
                     [{'run_id': RUN, 'sequence': True}],
                     [{'run_id': RUN, 'sequence': 2}] * 2, {'events': []}]:
            self.respond = lambda *_, page=page: (200, page, {})
            with self.subTest(page=page), self.assertRaises(HudsonError):
                self.client.events(RUN, after=1, limit=2)
        before = len(self.requests)
        for after, limit in [(-1, 1), (True, 1), (0, 0), (0, 1001)]:
            with self.assertRaises(ValueError):
                self.client.events(RUN, after=after, limit=limit)
        self.assertEqual(len(self.requests), before)

    def test_reconnect_drains_final_pages_without_execution(self):
        def respond(method, path, body):
            if '/events?' not in path:
                return 200, {'id': RUN, 'status': 'completed', 'result': 42}, {}
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(path).query)
            after = int(query['after'][0])
            page = [{'run_id': RUN, 'sequence': after + 1}] if after < 3 else []
            return 200, page, {}
        self.respond = respond
        records = list(self.client.follow(RUN, after=1, page_size=1))
        self.assertEqual([r['after'] for r in records], [2, 3, 3])
        self.assertEqual(records[-1]['run']['result'], 42)
        reconnect = list(self.client.follow(RUN, after=3, page_size=1))
        self.assertEqual([r['type'] for r in reconnect], ['run'])
        self.assertTrue(all(r[0] == 'GET' for r in self.requests))

    def test_wait_returns_without_approval_and_budgets_stop_observation(self):
        def respond(method, path, body):
            if '/events?' in path:
                return 200, [], {}
            return 200, {'id': RUN, 'status': 'waiting', 'wait': {'kind': 'approval'}}, {}
        self.respond = respond
        self.assertEqual(len(list(self.client.follow(RUN))), 1)
        self.assertTrue(all(r[0] == 'GET' for r in self.requests))
        self.respond = lambda *args: (200, [] if '/events?' in args[1] else
                                      {'id': RUN, 'status': 'running'}, {})
        with self.assertRaisesRegex(HudsonError, 'poll budget exhausted; last cursor 0'):
            list(self.client.follow(RUN, max_polls=1))
        self.respond = lambda *args: (200, [{'run_id': RUN, 'sequence': 1}]
                                      if '/events?' in args[1] else
                                      {'id': RUN, 'status': 'completed'}, {})
        with self.assertRaisesRegex(HudsonError, 'page budget exhausted; last cursor 1'):
            list(self.client.follow(RUN, max_pages=1))

    def test_explicit_controls_and_safe_resource_segments(self):
        self.client.approve(OTHER, approved=False)
        self.assertEqual(self.requests[-1][3], {'approved': False})
        self.client.reply(RUN, {'city': 'Boston'}, request_key='answer-one', question_id=12)
        self.assertEqual(self.requests[-1][3]['question_id'], 12)
        self.client.resume(RUN)
        self.client.cancel(RUN)
        self.client.tool('lookup', 1)
        self.client.agent('agent', 1)
        before = len(self.requests)
        with self.assertRaises(ValueError):
            self.client.run('../health')
        with self.assertRaises(ValueError):
            self.client.tool('../health', 1)
        with self.assertRaises(ValueError):
            self.client.approve(OTHER, approved='yes')
        with self.assertRaises(ValueError):
            self.client.agent('agent', 0)
        with self.assertRaises(ValueError):
            self.client.reply(RUN, 'Boston', request_key='answer-one', question_id=OTHER)
        self.assertEqual(len(self.requests), before)

    def test_mismatched_run_and_unknown_status_stop(self):
        for value in [{'id': OTHER, 'status': 'completed'}, {'id': RUN, 'status': 'new-status'}]:
            self.respond = lambda *_, value=value: (200, value, {})
            with self.assertRaises(HudsonError):
                self.client.run(RUN)


if __name__ == '__main__':
    unittest.main()
