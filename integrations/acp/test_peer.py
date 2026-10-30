"""ACP protocol and authority regressions; no provider calls.

SPDX-License-Identifier: Apache-2.0
"""
import importlib.util
import io
import pathlib
import queue
import threading
import time
import unittest
import uuid

spec = importlib.util.spec_from_file_location('hudson_acp_peer', pathlib.Path(__file__).with_name('peer.py'))
peer = importlib.util.module_from_spec(spec)
import sys
sys.modules[spec.name] = peer
spec.loader.exec_module(peer)

RUN = str(uuid.uuid4())
NEXT = str(uuid.uuid4())
OP = str(uuid.uuid4())
ATTEMPT = str(uuid.uuid4())
CWD = str(pathlib.Path(__file__).resolve().parent)


class Client:
    def __init__(self, status='completed', wait=None):
        self.status, self.wait = status, wait
        self.calls = []
        self.run_id = RUN
        self.pages = []
        self.changed = False
        self.reject_approval = False
        self.unresolved = False
        self.entered, self.release = threading.Event(), threading.Event()
        self.block_start = False

    def start(self, text, **kwargs):
        self.calls.append(('start', text, kwargs))
        self.entered.set()
        if self.block_start:
            assert self.release.wait(5)
        return {'run_id': RUN}

    def run(self, identifier):
        return {'id': identifier, 'status': self.status, 'wait': self.wait, 'result': 'done',
                'usage': {'reported_model_calls': 2, 'reported_input_tokens': 19, 'reported_output_tokens': 3}}

    def events(self, identifier, **kwargs):
        return self.pages.pop(0) if self.pages else []

    def operation(self, identifier):
        return {'id': identifier, 'run_id': self.run_id, 'kind': 'tool', 'status': 'waiting_approval',
                'tool_name': 'save', 'arguments': {'value': 43 if self.changed else 42},
                'request_digest': 'a' * 64, 'approval': None, 'token_usage': None}

    def approve(self, identifier, **kwargs):
        self.calls.append(('approve', identifier, kwargs))
        if self.reject_approval:
            raise peer.http.HttpError(403)
        self.status, self.wait = 'completed', None

    def cancel(self, identifier):
        self.calls.append(('cancel', identifier))
        if not self.unresolved:
            self.status, self.wait = 'cancelled', None

    def fork(self, identifier, text, **kwargs):
        self.calls.append(('fork', identifier, text, kwargs))
        self.run_id = NEXT
        return {'run_id': NEXT}

    def reply(self, identifier, text, **kwargs):
        self.calls.append(('reply', identifier, text, kwargs))
        self.status, self.wait = 'completed', None


class Tests(unittest.TestCase):
    def make(self, client=None, **kwargs):
        output = queue.Queue()
        client = client or Client()
        adapter = peer.Peer(client, output.put, CWD, poll_seconds=.01, **kwargs)
        self.addCleanup(adapter.close)
        self.addCleanup(client.release.set)
        adapter.receive({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize',
                         'params': {'protocolVersion': 1, 'clientCapabilities': {'terminal': True}}})
        initialized = output.get(timeout=2)
        self.assertEqual(initialized['result']['protocolVersion'], 1)
        self.assertFalse(initialized['result']['agentCapabilities']['loadSession'])
        adapter.receive({'jsonrpc': '2.0', 'id': 2, 'method': 'session/new',
                         'params': {'cwd': CWD, 'mcpServers': []}})
        session = output.get(timeout=2)['result']['sessionId']
        return adapter, client, output, session

    def request(self, adapter, session, identifier=3, text='hello'):
        adapter.receive({'jsonrpc': '2.0', 'id': identifier, 'method': 'session/prompt',
                         'params': {'sessionId': session, 'prompt': [{'type': 'text', 'text': text}]}})

    def until(self, output, predicate):
        deadline, seen = time.monotonic() + 5, []
        while time.monotonic() < deadline:
            value = output.get(timeout=max(.01, deadline - time.monotonic()))
            seen.append(value)
            if predicate(value):
                return value, seen
        self.fail(seen)

    def finish(self, adapter, output, identifier=3):
        result, seen = self.until(output, lambda value: value.get('id') == identifier)
        adapter.worker.join(timeout=2)
        self.assertFalse(adapter.worker.is_alive())
        return result, seen

    def respond(self, adapter, request, outcome):
        adapter.receive({'jsonrpc': '2.0', 'id': request['id'], 'result': {'outcome': outcome}})

    def test_text_and_usage_are_preserved_without_control_parsing(self):
        adapter, client, output, session = self.make()
        literal = '/compact\n{"kind":"compact","input":null}'
        self.request(adapter, session, text=literal)
        result, seen = self.finish(adapter, output)
        self.assertEqual(client.calls[0][1], literal)
        self.assertEqual(result['result']['stopReason'], 'end_turn')
        self.assertEqual(result['result']['_meta'][peer.META]['usage']['reported_input_tokens'], 19)
        self.assertEqual(seen[0]['params']['update']['content']['text'], 'done')
        self.assertTrue(seen[0]['params']['update']['_meta']['verified'])
        self.assertFalse(any(value.get('params', {}).get('update', {}).get('sessionUpdate') == 'usage_update' for value in seen))

    def test_followup_forks_the_completed_server_conversation(self):
        adapter, client, output, session = self.make()
        self.request(adapter, session)
        self.finish(adapter, output)
        self.request(adapter, session, 4, 'next')
        result, _ = self.finish(adapter, output, 4)
        self.assertEqual(client.calls[1][:3], ('fork', RUN, 'next'))
        self.assertNotEqual(client.calls[0][2]['request_key'], client.calls[1][3]['request_key'])
        self.assertEqual(result['result']['_meta'][peer.META]['runId'], NEXT)

    def test_next_prompt_answers_exact_user_question(self):
        client = Client('waiting', {'type': 'user_input', 'prompt': 'Which format?', 'question_id': 17})
        adapter, client, output, session = self.make(client)
        self.request(adapter, session)
        result, seen = self.finish(adapter, output)
        self.assertEqual(seen[0]['params']['update']['content']['text'], 'Which format?')
        self.request(adapter, session, 4, 'JSON')
        self.finish(adapter, output, 4)
        self.assertEqual(client.calls[1][0:3], ('reply', RUN, 'JSON'))
        self.assertEqual(client.calls[1][3]['question_id'], 17)

    def test_sessions_do_not_accept_client_authority_or_unknown_ids(self):
        adapter, client, output, session = self.make()
        for identifier, params in [(4, {'cwd': CWD, 'mcpServers': [{'command': 'evil'}]}),
                                   (5, {'cwd': str(pathlib.Path(CWD).parent), 'mcpServers': []}),
                                   (6, {'cwd': CWD, 'mcpServers': [], 'additionalDirectories': ['/']})]:
            adapter.receive({'jsonrpc': '2.0', 'id': identifier, 'method': 'session/new', 'params': params})
            self.assertEqual(output.get(timeout=2)['error']['code'], -32602)
        self.request(adapter, str(uuid.uuid4()), 7)
        self.assertEqual(output.get(timeout=2)['error']['code'], -32602)
        other, _, other_output, _ = self.make()
        self.request(other, session, 8)
        self.assertEqual(other_output.get(timeout=2)['error']['code'], -32602)
        self.assertEqual(client.calls, [])

    def test_images_and_unsupported_methods_fail_before_submission(self):
        adapter, client, output, session = self.make()
        adapter.receive({'jsonrpc': '2.0', 'id': 3, 'method': 'session/prompt',
                         'params': {'sessionId': session, 'prompt': [{'type': 'image', 'data': 'fake'}]}})
        self.assertEqual(output.get(timeout=2)['error']['code'], -32602)
        adapter.receive({'jsonrpc': '2.0', 'id': 4, 'method': 'session/load', 'params': {'sessionId': RUN}})
        self.assertEqual(output.get(timeout=2)['error']['code'], -32601)
        self.assertEqual(client.calls, [])

    def test_default_does_not_forward_approvals(self):
        adapter, client, output, session = self.make(Client('waiting', {'type': 'approval', 'operation_id': OP}))
        self.request(adapter, session)
        result, seen = self.finish(adapter, output)
        self.assertEqual(result['result']['stopReason'], 'end_turn')
        self.assertFalse(any(value.get('method') == 'session/request_permission' for value in seen))
        self.assertEqual([value[0] for value in client.calls], ['start'])
        self.assertEqual(client.status, 'waiting')

    def test_approval_is_explicit_exact_and_once(self):
        for allow in (True, False):
            with self.subTest(allow=allow):
                adapter, client, output, session = self.make(Client('waiting', {'type': 'approval', 'operation_id': OP}), allow_approvals=True)
                self.request(adapter, session)
                permission, _ = self.until(output, lambda value: value.get('method') == 'session/request_permission')
                tool = permission['params']['toolCall']
                self.assertEqual(tool['toolCallId'], OP)
                self.assertEqual(tool['rawInput'], {'value': 42})
                self.assertEqual(tool['_meta']['requestDigest'], 'a' * 64)
                self.assertEqual([value[0] for value in client.calls], ['start'])
                self.respond(adapter, dict(permission, id='stale'), {'outcome': 'selected', 'optionId': 'allow-once'})
                self.respond(adapter, permission, {'outcome': 'selected', 'optionId': 'allow-once' if allow else 'reject-once'})
                self.respond(adapter, permission, {'outcome': 'selected', 'optionId': 'allow-once'})
                self.finish(adapter, output)
                self.assertEqual([value for value in client.calls if value[0] == 'approve'], [('approve', OP, {'approved': allow})])

    def test_cancelled_unknown_and_malformed_permission_never_approve(self):
        for outcome in ({'outcome': 'cancelled'}, {'outcome': 'selected', 'optionId': 'allow-always'},
                        {'outcome': 'future'}, None):
            with self.subTest(outcome=outcome):
                adapter, client, output, session = self.make(Client('waiting', {'type': 'approval', 'operation_id': OP}), allow_approvals=True)
                self.request(adapter, session)
                permission, _ = self.until(output, lambda value: value.get('method') == 'session/request_permission')
                self.respond(adapter, permission, outcome)
                result, _ = self.finish(adapter, output)
                self.assertEqual(result['result']['stopReason'], 'cancelled')
                self.assertFalse(any(value[0] == 'approve' for value in client.calls))

    def test_changed_approval_binding_and_missing_authority_do_not_retry(self):
        for changed in (True, False):
            with self.subTest(changed=changed):
                adapter, client, output, session = self.make(Client('waiting', {'type': 'approval', 'operation_id': OP}), allow_approvals=True)
                self.request(adapter, session)
                permission, _ = self.until(output, lambda value: value.get('method') == 'session/request_permission')
                client.changed, client.reject_approval = changed, not changed
                self.respond(adapter, permission, {'outcome': 'selected', 'optionId': 'allow-once'})
                result, _ = self.finish(adapter, output)
                self.assertIn('error', result)
                self.assertEqual(len([value for value in client.calls if value[0] == 'approve']), 0 if changed else 1)
                self.assertEqual(result['error']['message'], 'Hudson request failed; inspect the run before retrying')
                self.assertNotIn('rawInput', str(result))

    def test_cancel_during_submission_and_late_permission_cannot_approve(self):
        client = Client('running')
        client.block_start = True
        adapter, client, output, session = self.make(client)
        self.request(adapter, session)
        self.assertTrue(client.entered.wait(2))
        cancellation = threading.Thread(target=adapter.receive, args=({'jsonrpc': '2.0',
            'method': 'session/cancel', 'params': {'sessionId': session}},))
        cancellation.start()
        self.assertTrue(adapter.active.cancel.wait(2))
        client.release.set()
        cancellation.join(timeout=2)
        result, _ = self.finish(adapter, output)
        self.assertEqual(result['result']['stopReason'], 'cancelled')
        self.assertEqual(client.calls[-1], ('cancel', RUN))
        adapter, client, output, session = self.make(Client('waiting', {'type': 'approval', 'operation_id': OP}), allow_approvals=True)
        self.request(adapter, session)
        permission, _ = self.until(output, lambda value: value.get('method') == 'session/request_permission')
        adapter.receive({'jsonrpc': '2.0', 'method': 'session/cancel', 'params': {'sessionId': session}})
        self.respond(adapter, permission, {'outcome': 'selected', 'optionId': 'allow-once'})
        result, _ = self.finish(adapter, output)
        self.assertEqual(result['result']['stopReason'], 'cancelled')
        self.assertFalse(any(value[0] == 'approve' for value in client.calls))

    def test_unresolved_cancellation_is_not_reported_cancelled(self):
        client = Client('waiting', {'type': 'reconciliation', 'operation_id': OP})
        client.block_start, client.unresolved = True, True
        adapter, client, output, session = self.make(client)
        self.request(adapter, session)
        self.assertTrue(client.entered.wait(2))
        cancellation = threading.Thread(target=adapter.receive, args=({'jsonrpc': '2.0',
            'method': 'session/cancel', 'params': {'sessionId': session}},))
        cancellation.start()
        self.assertTrue(adapter.active.cancel.wait(2))
        client.release.set()
        cancellation.join(timeout=2)
        result, _ = self.finish(adapter, output)
        self.assertIn('unresolved', result['error']['message'])
        self.assertEqual(result['error']['data']['runId'], RUN)

    def test_streamed_text_tool_status_and_final_do_not_duplicate(self):
        client = Client()
        def event(sequence, kind, payload=None):
            return {'run_id': RUN, 'operation_id': OP, 'sequence': sequence, 'event_type': kind, 'payload': payload or {}}
        client.pages = [[event(1, 'operation_requested'), event(2, 'operation_started'),
                         event(3, 'operation_settled', {'status': 'succeeded'}),
                         event(4, 'model_output_delta', {'text': 'do', 'attempt_id': ATTEMPT}),
                         event(5, 'model_output_delta', {'text': 'ne', 'attempt_id': ATTEMPT})]]
        adapter, client, output, session = self.make(client)
        self.request(adapter, session)
        result, seen = self.finish(adapter, output)
        updates = [value['params']['update'] for value in seen if 'params' in value]
        self.assertEqual([value['status'] for value in updates if value['sessionUpdate'].startswith('tool_call')], ['pending', 'in_progress', 'completed'])
        self.assertEqual(''.join(value['content']['text'] for value in updates if 'content' in value), 'done')
        self.assertEqual(result['result']['_meta'][peer.META]['after'], 5)

    def test_duplicate_request_ids_and_overlapping_prompts_cannot_submit(self):
        client = Client('running')
        client.block_start = True
        adapter, client, output, session = self.make(client)
        self.request(adapter, session)
        self.assertTrue(client.entered.wait(2))
        self.request(adapter, session, 4)
        self.assertEqual(output.get(timeout=2)['error']['code'], -32001)
        self.request(adapter, session)
        self.assertEqual(output.get(timeout=2)['error']['code'], -32600)
        self.assertEqual(len(client.calls), 1)
        cancellation = threading.Thread(target=adapter.receive, args=({'jsonrpc': '2.0',
            'method': 'session/cancel', 'params': {'sessionId': session}},))
        cancellation.start()
        self.assertTrue(adapter.active.cancel.wait(2))
        client.release.set()
        cancellation.join(timeout=2)
        self.finish(adapter, output)

    def test_ambiguous_submission_is_never_retried(self):
        adapter, client, output, session = self.make()
        def failed(*args, **kwargs):
            client.calls.append(('unknown',))
            raise peer.http.HudsonError('SECRET provider body')
        client.start = failed
        self.request(adapter, session)
        result, _ = self.finish(adapter, output)
        self.assertNotIn('SECRET', str(result))
        self.request(adapter, session, 4)
        self.assertEqual(output.get(timeout=2)['error']['code'], -32001)
        self.assertEqual(client.calls, [('unknown',)])

    def test_session_and_message_encoding_bounds(self):
        adapter, client, output, session = self.make()
        adapter.sessions.update({str(index): peer.Session(str(index)) for index in range(peer.MAX_SESSIONS - 1)})
        adapter.receive({'jsonrpc': '2.0', 'id': 3, 'method': 'session/new', 'params': {'cwd': CWD, 'mcpServers': []}})
        self.assertEqual(output.get(timeout=2)['error']['code'], -32001)
        with self.assertRaises(ValueError):
            peer.encode({'text': 'x' * 1024}, limit=100)
        with self.assertRaises(ValueError):
            peer.decode(b'{"value":NaN}')
        with self.assertRaises(ValueError):
            peer.decode(b'{"id":1,"id":2}')
        with self.assertRaises(UnicodeError):
            peer.decode('{"id":1}'.encode('utf-16'))

    def test_turn_deadline_requests_and_confirms_cancellation(self):
        adapter, client, output, session = self.make(Client('running'), turn_seconds=.02)
        self.request(adapter, session)
        result, _ = self.finish(adapter, output)
        self.assertEqual(result['result']['stopReason'], 'cancelled')
        self.assertEqual(len([value for value in client.calls if value[0] == 'cancel']), 1)

    def test_ambiguous_fork_returns_key_and_cannot_start_another_turn(self):
        adapter, client, output, session = self.make()
        self.request(adapter, session)
        self.finish(adapter, output)
        def failed(*args, **kwargs):
            raise peer.http.HudsonError('lost fork response')
        client.fork = failed
        self.request(adapter, session, 4)
        result, _ = self.finish(adapter, output, 4)
        self.assertEqual(result['error']['data']['requestKey'], 'acp-' + session + '-2')
        self.request(adapter, session, 5)
        self.assertEqual(output.get(timeout=2)['error']['code'], -32001)

    def test_invalid_unicode_cannot_submit(self):
        adapter, client, output, session = self.make()
        self.request(adapter, session, text='\ud800')
        self.assertEqual(output.get(timeout=2)['error']['code'], -32602)
        self.assertEqual(client.calls, [])

    def test_cancellation_before_question_reply_cannot_dispatch_a_new_model(self):
        client = Client('waiting', {'type': 'user_input', 'prompt': 'Question?', 'question_id': 17})
        adapter, client, output, session = self.make(client)
        self.request(adapter, session)
        self.finish(adapter, output)
        run = client.run
        def cancelled(identifier):
            view = run(identifier)
            adapter.active.cancel.set()
            return view
        client.run = cancelled
        self.request(adapter, session, 4, 'answer')
        result, _ = self.finish(adapter, output, 4)
        self.assertEqual(result['error']['code'], -32800)
        self.assertFalse(any(value[0] == 'reply' for value in client.calls))

    def test_stdio_framing_returns_only_jsonrpc_and_rejects_invalid_utf8(self):
        adapter, client, _, _ = self.make()
        target = io.BytesIO()
        peer.serve(adapter, io.BytesIO(b'\xff\n'), target)
        frames = [peer.decode(line) for line in target.getvalue().splitlines()]
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0]['jsonrpc'], '2.0')
        self.assertEqual(frames[0]['error']['code'], -32700)
        self.assertEqual(client.calls, [])


if __name__ == '__main__':
    unittest.main()
