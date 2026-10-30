"""RPC boundary regressions; no installed CLI or provider credentials required.

SPDX-License-Identifier: Apache-2.0
"""
import importlib.util
import pathlib
import sys
import tempfile
import time
import unittest

spec = importlib.util.spec_from_file_location('codex_rpc', pathlib.Path(__file__).with_name('client.py'))
rpc = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rpc)


class RpcTests(unittest.TestCase):
    def test_host_rejects_unsolicited_execution_before_correlated_result(self):
        code = """
import json,sys
request=json.loads(sys.stdin.buffer.readline())
print(json.dumps({'id':'vendor-call','method':'execute','params':{'program':'forbidden'}}),flush=True)
denied=json.loads(sys.stdin.buffer.readline())
assert denied['id']=='vendor-call' and denied['error']['code']==-32601
print(json.dumps({'method':'fixture/event','params':{'ready':True}}),flush=True)
print(json.dumps({'id':request['id'],'result':{'denied':True}}),flush=True)
"""
        with tempfile.TemporaryDirectory() as cwd:
            with rpc.RpcProcess(sys.executable, ['-I', '-c', code], cwd, {}, timeout=5) as client:
                seen = []
                def event(value):
                    seen.append(value['method'])
                    client.deny(value)
                self.assertEqual(client.call('fixture', {}, event), {'denied': True})
                self.assertEqual(seen, ['execute', 'fixture/event'])

    def test_oversized_or_truncated_native_output_fails_without_retry(self):
        for code in ["import sys; sys.stdout.buffer.write(b'x'*(1048576+1)+b'\\n')",
                     "import sys; sys.stdout.buffer.write(b'{\\\"id\\\":1}')"]:
            with self.subTest(code=code), tempfile.TemporaryDirectory() as cwd:
                with rpc.RpcProcess(sys.executable, ['-I', '-c', code], cwd, {}, timeout=5) as client:
                    with self.assertRaises(ValueError):
                        client.receive()

    def test_deadline_terminates_unresponsive_process(self):
        with tempfile.TemporaryDirectory() as cwd:
            start = time.monotonic()
            with rpc.RpcProcess(sys.executable, ['-I', '-c', 'import time; time.sleep(30)'], cwd, {}, timeout=.2) as client:
                with self.assertRaises(TimeoutError):
                    client.receive()
            self.assertLess(time.monotonic() - start, 5)
            self.assertIsNotNone(client.process.poll())


if __name__ == '__main__':
    unittest.main()
