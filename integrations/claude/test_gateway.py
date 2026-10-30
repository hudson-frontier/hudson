"""Typed Anthropic protocol and actual HTTP admission regressions, offline.

SPDX-License-Identifier: Apache-2.0
"""
import copy
import http.client
import http.server
import importlib.util
import pathlib
import threading
import unittest

spec = importlib.util.spec_from_file_location('hudson_claude_gateway', pathlib.Path(__file__).with_name('gateway.py'))
gateway = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gateway)
ALIAS = gateway.tool_alias('save')
MODEL = 'claude-sonnet-4-6'


def message(calls=False):
    return {'type': 'message', 'id': 'msg-fixture', 'role': 'assistant', 'model': MODEL,
            'content': [{'type': 'tool_use', 'id': 'save-1', 'name': ALIAS, 'input': {'value': 42}}]
                       if calls else [{'type': 'text', 'text': '42'}],
            'usage': {'input_tokens': 10, 'output_tokens': 2, 'cache_read_input_tokens': 3, 'cache_creation_input_tokens': 4},
            'stop_reason': 'tool_use' if calls else 'end_turn', 'stop_sequence': None}


def request():
    return {'instructions': 'Use the host catalog.', 'model': MODEL,
            'messages': [{'role': 'user', 'content': [{'type': 'json', 'value': 'task'}]}],
            'tools': [{'name': 'save', 'description': 'Save a value', 'input_schema': {'type': 'object'}}]}


class Tests(unittest.TestCase):
    def validate(self, value):
        return gateway.validate_stream(gateway.stream(value), {ALIAS}, MODEL, 32)

    def test_text_tool_and_signed_thinking_round_trip(self):
        for calls in (False, True):
            value = message(calls)
            value['content'].insert(0, {'type': 'thinking', 'thinking': 'private thought', 'signature': 'signed-private'})
            value['content'].insert(1, {'type': 'redacted_thinking', 'data': 'opaque-private'})
            self.assertEqual(self.validate(value), value)
            self.assertEqual(gateway.usage(value['usage']), {'input_tokens': 17, 'output_tokens': 2})

    def test_server_tools_refusals_and_unadvertised_names_are_rejected(self):
        for block in ({'type': 'server_tool_use', 'id': 'a', 'name': 'web_search', 'input': {}},
                      {'type': 'tool_use', 'id': 'b', 'name': 'Bash', 'input': {'command': 'bad'}},
                      {'type': 'refusal', 'refusal': 'no'}, {'type': 'text', 'text': 'x', 'citations': [{}]}):
            with self.subTest(block=block), self.assertRaises(ValueError):
                gateway.content(block, {ALIAS})

    def test_truncation_trailing_and_wrong_model_are_rejected(self):
        raw = gateway.stream(message())
        for changed in (raw[:-1], raw + b'event: ping\ndata: {"type":"ping"}\n\n',
                        raw.replace(MODEL.encode(), b'other-model'), raw.replace(b'event: message_start', b'event: message_stop', 1)):
            with self.subTest(changed=changed[:30]), self.assertRaises(ValueError):
                gateway.validate_stream(changed, {ALIAS}, MODEL, 32)

    def test_usage_and_output_allocation_fail_closed(self):
        for reported in ({'input_tokens': -1, 'output_tokens': 1}, {'input_tokens': 1, 'output_tokens': True},
                         {'input_tokens': 2**64 - 1, 'cache_read_input_tokens': 1, 'output_tokens': 0}):
            with self.subTest(reported=reported), self.assertRaises(ValueError):
                gateway.usage(reported)
        value = message()
        value['usage']['output_tokens'] = 33
        with self.assertRaises(ValueError):
            self.validate(value)
        with self.assertRaises(ValueError):
            gateway.decode(b'{"tokens":NaN}')
        with self.assertRaises(ValueError):
            gateway.decode(b'{"id":1,"id":2}')

    def test_duplicate_calls_bad_json_stop_reasons_and_indices_fail(self):
        for mutate in ('duplicate', 'json', 'stop', 'index'):
            value = message(True)
            if mutate == 'duplicate':
                value['content'].append(copy.deepcopy(value['content'][0]))
            raw = gateway.stream(value)
            if mutate == 'json':
                raw = raw.replace(b'{\\"value\\":42}', b'invalid-json')
            elif mutate == 'stop':
                raw = raw.replace(b'"stop_reason":"tool_use"', b'"stop_reason":"max_tokens"')
            elif mutate == 'index':
                raw = raw.replace(b'"index":0', b'"index":2', 1)
            with self.subTest(mutate=mutate), self.assertRaises(ValueError):
                gateway.validate_stream(raw, {ALIAS}, MODEL, 32)

    def test_host_history_and_exact_opaque_continuation(self):
        history = request()['messages']
        call = {'call_id': 'save-1', 'name': 'save', 'arguments': {'value': 42},
                'provider_metadata': {'claude_assistant_content': message(True)['content']}}
        history.extend([{'role': 'assistant', 'content': [{'type': 'tool_call', 'call': call}]},
                        {'role': 'tool', 'content': [{'type': 'tool_result', 'result': {
                            'call_id': 'save-1', 'outcome': {'status': 'success', 'value': 42}}}]}])
        converted = gateway.messages(history)
        self.assertEqual(converted[1]['content'], message(True)['content'])
        self.assertEqual(converted[2]['content'][0]['tool_use_id'], 'save-1')
        self.assertFalse(converted[2]['content'][0]['is_error'])
        call['arguments']['value'] = 43
        with self.assertRaises(ValueError):
            gateway.messages(history)

    def test_input_images_and_unknown_roles_are_not_advertised(self):
        for history in ([{'role': 'system', 'content': []}], [{'role': 'user', 'content': [{'type': 'image'}]}]):
            with self.assertRaises(ValueError):
                gateway.messages(history)

    def test_http_gateway_pins_payload_and_never_retries(self):
        seen = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                seen.append((gateway.decode(self.rfile.read(int(self.headers['Content-Length']))), dict(self.headers)))
                raw = gateway.stream(message(True))
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with gateway.OneRequestGateway(f'http://127.0.0.1:{server.server_port}/v1/messages', 'real-fixture-key', request(), 32) as proxy:
                def invoke(token, value):
                    connection = http.client.HTTPConnection('127.0.0.1', proxy.server.server_port, timeout=5)
                    try:
                        connection.request('POST', '/v1/messages?beta=true', gateway.encode(value), {'x-api-key': token, 'Content-Type': 'application/json'})
                        response = connection.getresponse()
                        return response.status, response.read()
                    finally:
                        connection.close()
                native = {'model': MODEL, 'stream': True, 'max_tokens': 900000,
                          'tools': [{'name': 'Bash'}], 'messages': [{'role': 'user', 'content': 'native injected text'}]}
                self.assertEqual(invoke('wrong', native)[0], 401)
                self.assertEqual(invoke(proxy.token, native)[0], 200)
                self.assertEqual(invoke(proxy.token, native)[0], 409)
                self.assertEqual(proxy.requests, 1)
                self.assertIsNone(proxy.failure)
                self.assertEqual(len(seen), 1)
                payload, headers = seen[0]
                self.assertEqual(payload['max_tokens'], 32)
                self.assertEqual(payload['system'], request()['instructions'])
                self.assertEqual(payload['messages'], gateway.messages(request()['messages']))
                self.assertEqual(payload['tools'][0]['name'], ALIAS)
                self.assertEqual(headers['x-api-key'], 'real-fixture-key')
                self.assertNotIn('real-fixture-key', proxy.token)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_unsafe_endpoints_and_credentials_are_rejected_before_io(self):
        for endpoint, credential in [('http://remote.example/v1/messages', 'x'),
                                     ('https://user:secret@example.com/v1/messages', 'x'),
                                     ('https://example.com/v1/messages', 'key\nheader')]:
            with self.subTest(endpoint=endpoint), self.assertRaises(ValueError):
                gateway.OneRequestGateway(endpoint, credential, request(), 32)

    def test_rejected_provider_content_is_terminal_locally_and_cannot_retry_upstream(self):
        seen = []
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass
            def do_POST(self):
                seen.append(self.rfile.read(int(self.headers['Content-Length'])))
                raw = gateway.stream(message(True)).replace(ALIAS.encode(), b'Bash')
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with gateway.OneRequestGateway(f'http://127.0.0.1:{server.server_port}/v1/messages', 'fixture', request(), 32) as proxy:
                for expected in (400, 409):
                    connection = http.client.HTTPConnection('127.0.0.1', proxy.server.server_port, timeout=5)
                    try:
                        connection.request('POST', '/v1/messages', gateway.encode({'model': MODEL, 'stream': True}), {'x-api-key': proxy.token})
                        response = connection.getresponse()
                        self.assertEqual(response.status, expected)
                        self.assertEqual(response.read(), b'')
                    finally:
                        connection.close()
                self.assertEqual(len(seen), 1)
                self.assertEqual(proxy.requests, 1)
                self.assertIsNotNone(proxy.failure)
                self.assertIsNone(proxy.response)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)


if __name__ == '__main__':
    unittest.main()
