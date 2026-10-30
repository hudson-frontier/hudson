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
    def test_account_lifecycle_actions_are_explicit_and_do_not_accept_credentials(self):
        self.respond = lambda *_: (200, {}, {})
        self.client.accounts()
        self.client.open_account_session('native', request_key='session')
        self.client.account_action(RUN, 'status', request_key='read')
        self.client.account_action(RUN, 'login', request_key='login')
        self.client.account_action(RUN, 'renew', request_key='renew')
        self.client.close_account_session(RUN)
        self.assertEqual([(method,path) for method,path,_,_ in self.requests], [
            ('GET','/accounts'), ('POST','/accounts/native/sessions'),
            ('POST',f'/account-sessions/{RUN}/actions'),
            ('POST',f'/account-sessions/{RUN}/actions'),
            ('POST',f'/account-sessions/{RUN}/actions'), ('DELETE',f'/account-sessions/{RUN}')])
        with self.assertRaises(ValueError): self.client.account_action(RUN,'api_key',request_key='key')
        count=len(self.requests)
        for key in ('x'*129,'control\nkey'):
            with self.assertRaises(ValueError): self.client.open_account_session('native',request_key=key)
            with self.assertRaises(ValueError): self.client.account_action(RUN,'status',request_key=key)
        self.assertEqual(len(self.requests),count)

    def test_harness_discovery_is_a_read_only_catalog_request(self):
        self.respond = lambda *_: (200, [{'id': 'fixture', 'version': 1}], {})
        self.assertEqual(self.client.harnesses(), [{'id': 'fixture', 'version': 1}])
        self.assertEqual([(method, path) for method, path, _, _ in self.requests], [('GET', '/harnesses')])

    def test_spend_views_preserve_decimal_strings_and_unknown_usage(self):
        expected = {'enabled': True, 'currency': 'USD', 'units_per_usd': 1000000000,
                    'entries': [{'allocation_nano_usd': '9007199254740993',
                                 'priced_usage_nano_usd': None, 'status': 'unknown'}]}
        self.respond = lambda *_: (200, expected, {})
        self.assertEqual(self.client.run_spend(RUN), expected)
        self.client.operation_spend(OTHER)
        self.assertEqual([(method, path) for method, path, _, _ in self.requests], [
            ('GET', f'/runs/{RUN}/spend'), ('GET', f'/operations/{OTHER}/spend')])

    def test_explicit_image_input_is_sent_as_data_without_path_or_url_resolution(self):
        images = [{'media_type': 'image/png', 'data': 'YQ=='}]
        self.client.start_images('question', images, request_key='image-one')
        self.client.fork(RUN, 'follow-up', request_key='fork-image', images=images)
        transcript = {'format_version': 2, 'messages': [{'role': 'user', 'content': [
            {'type': 'image', 'image': images[0]}]}]}
        self.client.import_conversation(transcript, 'continue', request_key='import-image', images=images)
        self.assertEqual(self.requests[0][1], '/runs/images')
        self.assertEqual(self.requests[0][3]['images'], images)
        self.assertEqual(self.requests[1][3]['images'], images)
        self.assertEqual(self.requests[2][3]['transcript'], transcript)

    def test_conversation_transfer_has_explicit_sources_keys_and_a_separate_body_bound(self):
        transcript = {'format_version': 1, 'messages': [
            {'role': 'user', 'content': [{'type': 'text', 'text': 'x' * 70000}]}]}
        self.client.export_conversation(RUN)
        self.client.fork(RUN, 'continue', request_key='fork-one')
        self.client.import_conversation(transcript, 'continue', request_key='import-one',
                                        agent_ref={'name': 'target', 'version': 2})
        self.assertEqual([(method, path) for method, path, _, _ in self.requests], [
            ('GET', f'/runs/{RUN}/conversation'), ('POST', f'/runs/{RUN}/fork'),
            ('POST', '/runs/import')])
        self.assertEqual(self.requests[2][3]['transcript'], transcript)
        self.assertEqual(self.requests[1][3], {'input': 'continue', 'request_key': 'fork-one'})
        with self.assertRaises(ValueError):
            self.client.start(transcript, request_key='normal-bound')
        with self.assertRaises(ValueError):
            self.client.import_conversation(transcript, 'continue', request_key='')

    def test_stream_events_preserves_unicode_cursor_and_provisional_output(self):
        events = [{'run_id': RUN, 'sequence': i, 'event_type': 'model_output_delta',
                   'payload': {'text': 'a\u2028😀', 'provisional': True}} for i in (3, 4)]
        frames = ': heartbeat\r\n\r\n' + ''.join(
            f'id: {e["sequence"]}\r\nevent: event\r\ndata: ' +
            json.dumps(e, ensure_ascii=False) + '\r\n\r\n' for e in events)
        self.respond = lambda *_: (200, frames.encode(), {'Content-Type': 'text/event-stream'})
        self.assertEqual(list(self.client.stream_events(RUN, after=2)), events)
        self.assertEqual(self.requests[0][1], f'/runs/{RUN}/events/stream')
        self.assertEqual(self.requests[0][2]['Last-Event-Id'], '2')
        self.assertEqual(self.requests[0][2]['Authorization'], 'Bearer ' + TOKEN)
        self.assertEqual(list(self.client.stream_events(RUN, after=2, max_events=1)), events[:1])

    def test_stream_events_rejects_wrong_identity_sequence_truncation_and_bounds(self):
        for event in [{'run_id': OTHER, 'sequence': 1}, {'run_id': RUN, 'sequence': 0},
                      {'run_id': RUN, 'sequence': True}]:
            frame = ('id: 1\nevent: event\ndata: ' + json.dumps(event) + '\n\n').encode()
            self.respond = lambda *_, frame=frame: (200, frame, {'Content-Type': 'text/event-stream'})
            with self.assertRaises(HudsonError):
                list(self.client.stream_events(RUN))
        for frame in [b'data: {}', b'id: 1\nevent: event\ndata: {}\n', b'data: \xff\n\n']:
            self.respond = lambda *_, frame=frame: (200, frame, {'Content-Type': 'text/event-stream'})
            with self.assertRaises(HudsonError):
                list(self.client.stream_events(RUN))
        self.respond = lambda *_: (200, b':' + b'x' * 129, {'Content-Type': 'text/event-stream'})
        with self.assertRaises(HudsonError):
            list(HudsonClient(self.url, max_response_bytes=128).stream_events(RUN))
        for kwargs in [{'after': -1}, {'max_events': 0}, {'max_seconds': 301}]:
            with self.assertRaises(ValueError):
                list(self.client.stream_events(RUN, **kwargs))

    @classmethod
    def setUpClass(cls):
        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *_):
                pass

            def do_GET(self):
                self.handle_request()

            def do_POST(self):
                self.handle_request()

            def do_DELETE(self):
                self.handle_request()

            def handle_request(self):
                owner = self.server.owner
                raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
                body = json.loads(raw) if raw else None
                owner.requests.append((self.command, self.path, dict(self.headers), body))
                status, data, headers = owner.respond(self.command, self.path, body)
                encoded = data if isinstance(data, bytes) else json.dumps(data).encode()
                self.send_response(status)
                if (not any(name.lower() in ('content-length', 'transfer-encoding') for name in headers)
                        and headers.get('Connection', '').lower() != 'close'):
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

    def test_queue_controls_use_explicit_methods_and_never_resume(self):
        self.client.queue_message(RUN, {'task': 'next'}, kind='steer', request_key='message-1')
        self.client.queued_messages(RUN)
        self.client.clear_queued_messages(RUN)
        self.assertEqual([(method, path) for method, path, _, _ in self.requests], [
            ('POST', f'/runs/{RUN}/messages'), ('GET', f'/runs/{RUN}/messages'),
            ('DELETE', f'/runs/{RUN}/messages')])
        self.assertEqual(self.requests[0][3], {'kind': 'steer', 'input': {'task': 'next'}, 'request_key': 'message-1'})
        for kind in ['reply', 'approval', None]:
            with self.assertRaises(ValueError):
                self.client.queue_message(RUN, 'invalid', kind=kind, request_key='message-2')
        self.assertEqual(len(self.requests), 3)

    def test_compaction_is_an_explicit_null_control_and_is_not_a_direction(self):
        self.client.compact_context(RUN, request_key='compact-1')
        self.assertEqual(self.requests[0][0:2], ('POST', f'/runs/{RUN}/messages'))
        self.assertEqual(self.requests[0][3], {'kind':'compact','input':None,'request_key':'compact-1'})
        with self.assertRaises(ValueError):
            self.client.queue_message(RUN, 'do not interpret this as a command', kind='compact', request_key='bad')
        self.assertEqual(len(self.requests), 1)

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

    def test_incomplete_mutation_responses_fail_without_retry_or_echoed_secrets(self):
        body = json.dumps({'accepted': TOKEN}).encode()
        responses = [
            (body, {'Content-Length': str(len(body) + 10), 'Connection': 'close'}),
            (f'{len(body):x}\r\n'.encode() + body + b'\r\n',
             {'Transfer-Encoding': 'chunked', 'Connection': 'close'}),
        ]
        for payload, headers in responses:
            with self.subTest(headers=headers):
                self.requests.clear()
                self.respond = lambda *_, payload=payload, headers=headers: (201, payload, headers)
                with self.assertRaisesRegex(HudsonError, 'HTTP transport failed') as error:
                    self.client.start('task', request_key='same-key')
                self.assertNotIn(TOKEN, str(error.exception))
                self.assertEqual(len(self.requests), 1)
                self.assertEqual(self.requests[0][0:2], ('POST', '/runs'))
                self.assertEqual(self.requests[0][3]['request_key'], 'same-key')

    def test_complete_bounded_responses_accept_all_supported_framing(self):
        for payload, headers in [
                (b'{}', {}),
                (b'{}', {'Connection': 'close'}),
                (b'2\r\n{}\r\n0\r\n\r\n', {'Transfer-Encoding': 'chunked', 'Connection': 'close'})]:
            with self.subTest(headers=headers):
                self.respond = lambda *_, payload=payload, headers=headers: (200, payload, headers)
                self.assertEqual(HudsonClient(self.url, max_response_bytes=2).health(), {})

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
        self.client.reply(RUN, 'legacy answer', request_key='legacy-answer', question_id=0)
        self.assertEqual(self.requests[-1][3]['question_id'], 0)
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
        for invalid in (-1, True, 2**64):
            with self.assertRaises(ValueError):
                self.client.reply(RUN, 'Boston', request_key='answer-one', question_id=invalid)
        self.assertEqual(len(self.requests), before)

    def test_mismatched_run_and_unknown_status_stop(self):
        for value in [{'id': OTHER, 'status': 'completed'}, {'id': RUN, 'status': 'new-status'}]:
            self.respond = lambda *_, value=value: (200, value, {})
            with self.assertRaises(HudsonError):
                self.client.run(RUN)


if __name__ == '__main__':
    unittest.main()
