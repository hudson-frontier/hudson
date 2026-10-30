"""Run both execution-bridge transports through the real worker, without paid IO."""
import http.server
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading

root = pathlib.Path(__file__).resolve().parent.parent
bridge = root / 'examples/runtime-bridge/bridge.py'
module_spec = importlib.util.spec_from_file_location('bridge_fixture', bridge)
fixture = importlib.util.module_from_spec(module_spec)
module_spec.loader.exec_module(fixture)
server = http.server.HTTPServer(('127.0.0.1', 0), fixture.Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory() as directory:
        config = json.loads((root / 'examples/runtime-bridge.json').read_text())
        transports = [{'type': 'http', 'endpoint': f'http://127.0.0.1:{server.server_port}/model'}]
        if os.name == 'posix':
            transports.append({'type': 'stdio', 'executable': sys.executable,
                               'args': [str(bridge)], 'cwd': directory})
        for transport in transports:
            config['runtime']['transport'] = transport
            path = pathlib.Path(directory) / 'agent.json'
            path.write_text(json.dumps(config))
            result = subprocess.run([str(root / 'target/debug/hudson-worker'), '--config', str(path),
                                     '--task', 'check execution'], capture_output=True, text=True, timeout=20)
            assert result.returncode == 0, (transport['type'], result.stdout, result.stderr)
            run = json.loads(result.stdout)
            assert run['status'] == 'completed' and run['result'] == {'fixture': True, 'model': 'offline-fixture'}
            assert run['usage']['model_calls'] == 1 and run['usage']['reported_model_calls'] == 0
    print('PASS: runtime bridge transports through the real worker, verified output, no provider requests')
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
