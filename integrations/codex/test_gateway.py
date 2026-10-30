"""One-request admission against a real local HTTP destination.

SPDX-License-Identifier: Apache-2.0
"""
import http.client
import http.server
import importlib.util
import json
import pathlib
import threading
import unittest

spec = importlib.util.spec_from_file_location('codex_gateway', pathlib.Path(__file__).with_name('gateway.py'))
gateway = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gateway)


class GatewayTests(unittest.TestCase):
    def test_authentication_single_request_and_pinned_model_tools_and_allocation(self):
        captured = []
        output = b'data: {"type":"response.completed","response":{"output":[]}}\n\n'
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_POST(self):
                captured.append((self.headers.get('Authorization'), json.loads(self.rfile.read(int(self.headers['Content-Length'])))))
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Content-Length', str(len(output)))
                self.end_headers()
                self.wfile.write(output)
        destination = http.server.HTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=destination.serve_forever, daemon=True)
        thread.start()
        try:
            tool = {'name':'lookup','description':'Read fixture','input_schema':{'type':'object'}}
            with gateway.OneRequestGateway('http://127.0.0.1:'+str(destination.server_port)+'/responses',
                    'synthetic-upstream-secret', 'fixture', [tool], 32) as peer:
                def call(token, body):
                    connection = http.client.HTTPConnection('127.0.0.1', peer.server.server_port, timeout=5)
                    try:
                        connection.request('POST', '/v1/responses', json.dumps(body), {'Authorization':'Bearer '+token})
                        response = connection.getresponse()
                        return response.status, response.read()
                    finally:
                        connection.close()
                body = {'model':'fixture','input':[], 'tools':[{'type':'web_search'}],
                        'max_output_tokens':1000000, 'parallel_tool_calls':True, 'store':True}
                self.assertEqual(call('wrong', body)[0], 401)
                self.assertFalse(peer.used)
                self.assertEqual(call(peer.token, body), (200, output))
                self.assertTrue(peer.completed.wait(timeout=5))
                self.assertIsNone(peer.failure)
                self.assertEqual(peer.body, output)
                self.assertEqual(call(peer.token, body)[0], 409)
            self.assertEqual(len(captured), 1)
            auth, request = captured[0]
            self.assertEqual(auth, 'Bearer synthetic-upstream-secret')
            self.assertEqual(request['max_output_tokens'], 32)
            self.assertFalse(request['parallel_tool_calls'])
            self.assertFalse(request['store'])
            self.assertEqual([tool['name'] for tool in request['tools']], ['lookup'])
        finally:
            destination.shutdown(); destination.server_close(); thread.join(timeout=1)

    def test_invalid_model_consumes_attempt_without_provider_io(self):
        # No process listens at this port. Model binding must fail before a
        # connection, latch failure, and reject a second corrected request.
        with gateway.OneRequestGateway('http://127.0.0.1:1/responses', 'synthetic', 'fixture', [], 32) as peer:
            connection = http.client.HTTPConnection('127.0.0.1', peer.server.server_port, timeout=5)
            try:
                connection.request('POST', '/v1/responses', json.dumps({'model':'changed'}),
                                   {'Authorization':'Bearer '+peer.token})
                with self.assertRaises(http.client.RemoteDisconnected):
                    connection.getresponse()
            finally:
                connection.close()
            self.assertTrue(peer.completed.wait(timeout=5))
            self.assertIsNotNone(peer.failure)
            self.assertTrue(peer.used)
            self.assertIsNone(peer.body)


if __name__ == '__main__':
    unittest.main()
