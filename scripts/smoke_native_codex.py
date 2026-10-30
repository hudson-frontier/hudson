"""Installed native CLI, immutable file recovery and actual worker conformance."""
import argparse
import hashlib
import http.server
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def fingerprint(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', required=True)
    parser.add_argument('--python', default=sys.executable)
    parser.add_argument('--worker')
    parser.add_argument('--bubblewrap')
    parser.add_argument('--debug', action='store_true', help='Tracebacks for synthetic fixture inputs only')
    args = parser.parse_args()
    fixture = load('sdk_fixture', ROOT/'scripts/smoke_native_sdk.py')
    transport = load('codex_gateway', ROOT/'integrations/codex/gateway.py')
    original = fixture.response_events
    alias = transport.tool_alias('lookup_order')
    fixture.response_events = lambda index: original(index).replace(
        b'"name": "lookup_order"', b'"name": "'+alias.encode()+b'"')
    class Handler(fixture.Handler):
        expected_key = 'synthetic-native-key'
        max_requests = 3
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='hudson-codex-fixture-', dir='/var/tmp' if args.bubblewrap else None) as state:
            env = {'OPENAI_API_KEY':'synthetic-native-key', 'HUDSON_CODEX_EXECUTABLE':args.codex,
                'HUDSON_CODEX_STATE_DIR':state, 'PATH':os.environ.get('PATH',''),
                'HUDSON_CODEX_ENDPOINT':f'http://127.0.0.1:{server.server_port}/v1/responses'}
            call = {'run_id':str(uuid.uuid4()), 'operation_id':str(uuid.uuid4()), 'attempt_id':str(uuid.uuid4())}
            request = {'instructions':'Return 42 using lookup_order.', 'model':'fixture',
                'tools':[{'name':'lookup_order','description':'Look up an order','input_schema':{
                    'type':'object','properties':{'order_id':{'type':'string'}},'required':['order_id']}}],
                'messages':[{'role':'user','content':[{'type':'json','value':'start'}]}]}
            context = {'call':call, 'actor':{'workspace_id':'fixture','id':'owner'}, 'previous':None,
                       'history_mode':'replace','request_fingerprint':fingerprint(request)}
            body = {'protocol_version':1,'runtime_id':'codex-fixture','runtime_version':1,
                    'max_output_tokens':32,'stream':True,'request':request,'context':context}
            peer = str(ROOT/'integrations/codex/peer.py')
            command = [args.python,'-I',peer]
            if args.debug:
                command = [args.python,'-I','-c',
                    "import runpy,sys; p=sys.argv[1]; sys.argv=[p]; runpy.run_path(p,run_name='fixture')['main']()",peer]

            def invoke(expect_success=True):
                result = subprocess.run(command, input=json.dumps(body), env=env, text=True,
                    capture_output=True, timeout=150)
                if not expect_success:
                    assert result.returncode != 0
                    return None
                assert result.returncode == 0, result.stderr
                frames = [json.loads(line) for line in result.stdout.splitlines()]
                assert frames[0]['type']=='start' and frames[-1]['type']=='response'
                return frames[-1]['result']

            first = invoke()
            assert first['response']['type']=='tool_calls'
            tool_call = first['response']['calls'][0]
            assert tool_call['name']=='lookup_order' and tool_call['call_id']=='lookup-1'
            assert invoke()==first and len(fixture.requests)==1
            request['messages'] += [
                {'role':'assistant','content':[{'type':'tool_call','call':tool_call}]},
                {'role':'tool','content':[{'type':'tool_result','result':{
                    'call_id':'lookup-1','outcome':{'status':'success','value':42}}}]}]
            context.update(call=dict(call,operation_id=str(uuid.uuid4()),attempt_id=str(uuid.uuid4())),
                previous=first['checkpoint'],history_mode='append',request_fingerprint=fingerprint(request))
            final = invoke()
            assert final['response']=={'type':'final','output':42}
            outputs = [item for item in fixture.requests[1]['input'] if item.get('type')=='function_call_output']
            assert len(outputs)==1 and outputs[0]['call_id']=='lookup-1'
            assert all(result['usage']=={'input_tokens':10,'output_tokens':2} for result in (first,final))
            context.update(call={'run_id':str(uuid.uuid4()),'operation_id':str(uuid.uuid4()),'attempt_id':str(uuid.uuid4())},
                previous=final['checkpoint'])
            request['messages'] += [{'role':'user','content':[{'type':'json','value':'continue in an independent fork'}]}]
            context['request_fingerprint']=fingerprint(request)
            assert invoke()['response']=={'type':'final','output':42} and len(fixture.requests)==3
            context['actor']['id']='different-owner'
            context['call']={'run_id':str(uuid.uuid4()),'operation_id':str(uuid.uuid4()),'attempt_id':str(uuid.uuid4())}
            invoke(expect_success=False)
            assert len(fixture.requests)==3
            print('PASS: real CLI native rollout resume/fork, actor isolation, cached replay, one request per attempt and exact usage',flush=True)
            if args.worker:
                fixture.requests.clear()
                runtime = {'id':'codex-fixture','version':1,'stream':True,'native_sessions':True,'timeout_seconds':300,
                    'transport':{'type':'stdio','executable':args.python,'args':['-I',peer],'cwd':state,'env':list(env)}}
                if args.bubblewrap:
                    prefix = pathlib.Path(subprocess.check_output([args.python,'-I','-c','import sys; print(sys.prefix)'],text=True,timeout=30).strip())
                    roots = [pathlib.Path('/usr'),pathlib.Path('/lib'),pathlib.Path('/lib64'),ROOT/'integrations',pathlib.Path(args.codex).parent.parent]
                    if not any(prefix.is_relative_to(path) for path in roots):
                        roots.append(prefix)
                    runtime['transport'].update(type='isolated_stdio',isolation={'bubblewrap':args.bubblewrap,
                        'read_only':[str(path) for path in roots],'network':'host'})
                config = {'name':'native-codex-fixture','instructions':'Return 42 using lookup_order.','model':'fixture',
                    'max_output_tokens':32,'output_schema':{'type':'integer'},'runtime':runtime,
                    'http_tools':[{'name':'lookup_order','description':'Look up an order','effect':'read',
                        'endpoint':f'http://127.0.0.1:{server.server_port}/lookup','input_schema':{
                            'type':'object','properties':{'order_id':{'type':'string'}},'required':['order_id'],
                            'additionalProperties':False},'output_schema':{'type':'integer'}}]}
                path = pathlib.Path(state)/'agent.json'
                path.write_text(json.dumps(config))
                result = subprocess.run([args.worker,'--config',str(path),'--task','start'],env=env,
                    capture_output=True,text=True,timeout=360)
                assert result.returncode==0,result.stderr
                run = json.loads(result.stdout)
                assert run['status']=='completed' and run['result']==42, run
                assert run['usage']['model_calls']==2 and run['usage']['tool_calls']==1
                assert run['usage']['reported_input_tokens']==20 and len(fixture.effects)==1
                assert len(fixture.requests)==2
                print('PASS: actual worker, isolated native CLI, host-owned tool, native file checkpoints and goal verification',flush=True)
    finally:
        server.shutdown(); server.server_close(); thread.join(timeout=5)


if __name__ == '__main__':
    main()
