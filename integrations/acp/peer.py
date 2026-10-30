"""ACP v1 stdio agent backed by Hudson's authorized HTTP run API.

SPDX-License-Identifier: Apache-2.0
"""
import argparse
import hashlib
import importlib.util
import json
import os
import pathlib
import queue
import sys
import threading
import time
import uuid
from dataclasses import dataclass, field

ROOT = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('hudson_acp_http', ROOT.parent / 'python/hudson_client.py')
http = importlib.util.module_from_spec(spec)
spec.loader.exec_module(http)

INPUT_BYTES = 64 * 1024
FRAME_BYTES = 1024 * 1024
TURN_BYTES = 4 * 1024 * 1024
MAX_SESSIONS = 32
MAX_TURNS = 256
MAX_EVENTS = 4096
MAX_CALLS = 4096
META = 'org.hudson/run'


def encode(value, limit=FRAME_BYTES):
    output = bytearray()
    for part in json.JSONEncoder(ensure_ascii=False, allow_nan=False,
                                 separators=(',', ':')).iterencode(value):
        raw = part.encode('utf-8')
        if len(raw) > limit - len(output):
            raise ValueError('ACP byte ceiling')
        output.extend(raw)
    return bytes(output)


def decode(raw):
    def reject(_):
        raise ValueError('nonstandard JSON constant')
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate JSON key')
            result[key] = value
        return result
    return json.loads(raw.decode('utf-8') if isinstance(raw, bytes) else raw,
                      parse_constant=reject, object_pairs_hook=unique)


class ProtocolError(Exception):
    def __init__(self, code, message):
        self.code, self.message = code, message
        super().__init__(message)


@dataclass
class Session:
    id: str
    run: str | None = None
    cursor: int = 0
    turns: int = 0
    # A lost submission response cannot be retried with a new request key.
    uncertain: bool = False
    pending_key: str | None = None


@dataclass
class Turn:
    session: Session
    id: object
    text: str
    deadline: float
    cancel: threading.Event = field(default_factory=threading.Event)
    permission: queue.Queue = field(default_factory=lambda: queue.Queue(1))
    permission_id: str | None = None
    permission_consumed: bool = False
    calls: int = 0
    events: int = 0
    written: int = 0
    tools: set = field(default_factory=set)
    decisions: set = field(default_factory=set)
    stream_operation: str | None = None
    stream_digest: object = field(default_factory=hashlib.sha256)


class Peer:
    """One HTTP worker; session and request IDs belong to this stdio connection.

    The bearer identity and optional approval forwarding are operator settings.
    Protocol messages cannot change endpoints, actors, agents, tools or grants.
    """
    def __init__(self, client, emit, cwd, *, allow_approvals=False, agent_ref=None,
                 turn_seconds=300, poll_seconds=.25):
        if not pathlib.Path(cwd).is_absolute() or not 0 < turn_seconds <= 300:
            raise ValueError('invalid ACP operator configuration')
        self.client, self.emit, self.cwd = client, emit, os.path.normcase(os.path.normpath(cwd))
        self.allow_approvals, self.agent_ref = allow_approvals, agent_ref
        self.turn_seconds, self.poll_seconds = turn_seconds, poll_seconds
        self.initialized = False
        self.sessions = {}
        self.active = None
        self.worker = None
        self.closed = threading.Event()
        self.lock = threading.Lock()
        self.turns = 0
        self.request_ids = set()

    def send(self, value, turn=None):
        size = len(encode(value)) + 1
        if turn is not None:
            turn.written += size
            if turn.written > TURN_BYTES:
                raise ValueError('ACP turn output ceiling')
        self.emit(value)

    def result(self, identifier, result, turn=None):
        self.send({'jsonrpc': '2.0', 'id': identifier, 'result': result}, turn)

    def error(self, identifier, code, message, run=None, request_key=None):
        error = {'code': code, 'message': message}
        if run is not None:
            error['data'] = {'runId': run}
        if request_key is not None:
            error.setdefault('data', {})['requestKey'] = request_key
        self.send({'jsonrpc': '2.0', 'id': identifier, 'error': error})

    def update(self, turn, update):
        self.send({'jsonrpc': '2.0', 'method': 'session/update',
                   'params': {'sessionId': turn.session.id, 'update': update}}, turn)

    def text(self, turn, text, **metadata):
        if text:
            self.update(turn, {'sessionUpdate': 'agent_message_chunk',
                              'content': {'type': 'text', 'text': text},
                              '_meta': metadata})

    def receive(self, message):
        identifier = message.get('id') if isinstance(message, dict) else None
        try:
            if (not isinstance(message, dict) or message.get('jsonrpc') != '2.0'
                    or ('id' in message and (type(identifier) not in (int, str)
                        or (isinstance(identifier, str) and len(identifier) > 128)))):
                raise ProtocolError(-32600, 'Invalid JSON-RPC request')
            if 'method' not in message:
                self._permission_response(message)
                return
            method, params = message['method'], message.get('params', {})
            if not isinstance(method, str) or not isinstance(params, dict):
                raise ProtocolError(-32600, 'Invalid JSON-RPC request')
            notification = 'id' not in message
            if method == 'session/cancel' and notification:
                session = self._session(params)
                with self.lock:
                    if self.active is not None and self.active.session is session:
                        self.active.cancel.set()
                return
            if notification:
                return
            if identifier in self.request_ids:
                raise ProtocolError(-32600, 'Request ID was already used on this connection')
            if len(self.request_ids) >= 8192:
                raise ProtocolError(-32001, 'Request capacity exhausted')
            self.request_ids.add(identifier)
            if method == 'initialize':
                if self.initialized:
                    raise ProtocolError(-32600, 'Connection is already initialized')
                if type(params.get('protocolVersion')) is not int:
                    raise ProtocolError(-32602, 'protocolVersion is required')
                self.initialized = True
                self.result(identifier, {'protocolVersion': 1,
                    'agentInfo': {'name': 'hudson', 'version': '1'}, 'authMethods': [],
                    'agentCapabilities': {'loadSession': False,
                        'promptCapabilities': {'image': False, 'audio': False, 'embeddedContext': False},
                        'mcpCapabilities': {'http': False, 'sse': False}}})
                return
            if not self.initialized:
                raise ProtocolError(-32600, 'Initialize the connection first')
            if method == 'session/new':
                if len(self.sessions) >= MAX_SESSIONS:
                    raise ProtocolError(-32001, 'Session capacity exhausted')
                cwd = params.get('cwd')
                if (not isinstance(cwd, str) or not pathlib.Path(cwd).is_absolute()
                        or os.path.normcase(os.path.normpath(cwd)) != self.cwd):
                    raise ProtocolError(-32602, 'Session cwd must match the operator configured directory')
                if params.get('mcpServers') != [] or params.get('additionalDirectories'):
                    raise ProtocolError(-32602, 'Client supplied servers and directory grants are unsupported')
                session = Session(str(uuid.uuid4()))
                self.sessions[session.id] = session
                self.result(identifier, {'sessionId': session.id})
            elif method == 'session/prompt':
                session = self._session(params)
                prompt = params.get('prompt')
                if (not isinstance(prompt, list) or not prompt or len(prompt) > 64
                        or any(not isinstance(block, dict) or block.get('type') != 'text'
                               or not isinstance(block.get('text'), str) for block in prompt)):
                    raise ProtocolError(-32602, 'This agent accepts text prompts only')
                text = '\n'.join(block['text'] for block in prompt)
                try:
                    length = len(text.encode('utf-8'))
                except UnicodeError:
                    raise ProtocolError(-32602, 'Prompt must contain valid UTF-8 text') from None
                if not text.strip() or length > INPUT_BYTES - 4096:
                    raise ProtocolError(-32602, 'Prompt is empty or exceeds the byte ceiling')
                with self.lock:
                    if self.active is not None:
                        raise ProtocolError(-32001, 'One prompt may be active per connection')
                    if self.turns >= MAX_TURNS or session.uncertain:
                        raise ProtocolError(-32001, 'Turn capacity exhausted or prior submission is uncertain')
                    self.turns += 1
                    session.turns += 1
                    turn = Turn(session, identifier, text, time.monotonic() + self.turn_seconds)
                    self.active = turn
                    self.worker = threading.Thread(target=self._run, args=(turn,), daemon=True)
                    self.worker.start()
            else:
                raise ProtocolError(-32601, 'Method not found')
        except ProtocolError as error:
            if isinstance(message, dict) and 'id' not in message:
                return
            self.error(identifier if type(identifier) in (int, str) else None, error.code, error.message)

    def _session(self, params):
        identifier = params.get('sessionId')
        if not isinstance(identifier, str) or identifier not in self.sessions:
            raise ProtocolError(-32602, 'Session does not belong to this connection')
        return self.sessions[identifier]

    def _permission_response(self, message):
        with self.lock:
            turn = self.active
            if (turn is None or turn.permission_id is None or turn.permission_consumed
                    or message.get('id') != turn.permission_id):
                return
            turn.permission_consumed = True
            turn.permission.put_nowait(message)

    def call(self, turn, method, *args, **kwargs):
        turn.calls += 1
        if turn.calls > MAX_CALLS:
            raise ValueError('ACP HTTP call ceiling')
        return getattr(self.client, method)(*args, **kwargs)

    def admit(self, turn, method, *args, **kwargs):
        # Reserve dispatch before releasing the cancellation lock. Network IO
        # stays outside it so a slow submission cannot block cancellation.
        # A reserved HTTP request can race cancellation at the remote host.
        with self.lock:
            if turn.cancel.is_set() or self.closed.is_set():
                raise ProtocolError(-32800, 'Prompt cancelled before admission')
        return self.call(turn, method, *args, **kwargs)

    def operation(self, turn, identifier):
        operation = self.call(turn, 'operation', identifier)
        if (not isinstance(operation, dict) or operation.get('id') != identifier
                or operation.get('run_id') != turn.session.run):
            raise ValueError('Hudson operation identity changed')
        return operation

    def _events(self, turn, drain=False):
        session = turn.session
        while True:
            page = self.call(turn, 'events', session.run, after=session.cursor, limit=32)
            if not page:
                return
            for event in page:
                turn.events += 1
                if turn.events > MAX_EVENTS:
                    raise ValueError('ACP event ceiling')
                session.cursor = event['sequence']
                kind, operation_id = event['event_type'], event.get('operation_id')
                if kind == 'model_output_delta':
                    if turn.stream_operation != operation_id:
                        turn.stream_operation, turn.stream_digest = operation_id, hashlib.sha256()
                    turn.stream_digest.update(event['payload']['text'].encode('utf-8'))
                    self.text(turn, event['payload']['text'], provisional=True,
                              operationId=operation_id, attemptId=event['payload']['attempt_id'])
                elif kind == 'operation_requested' and operation_id is not None:
                    operation = self.operation(turn, operation_id)
                    if operation['kind'] == 'tool':
                        if len(turn.tools) >= 256:
                            raise ValueError('ACP tool update ceiling')
                        turn.tools.add(operation_id)
                        self.update(turn, dict(self.tool(operation), sessionUpdate='tool_call', status='pending'))
                elif operation_id in turn.tools:
                    status = None
                    if kind == 'operation_started':
                        status = 'in_progress'
                    elif kind == 'operation_settled':
                        status = 'completed' if event['payload']['status'] == 'succeeded' else 'failed'
                    elif kind == 'operation_unknown':
                        status = 'failed'
                    if status:
                        self.update(turn, {'sessionUpdate': 'tool_call_update', 'toolCallId': operation_id,
                                           'status': status, '_meta': {'hudsonStatus': event['payload'].get('status')}})
            if not drain or turn.cancel.is_set():
                return

    @staticmethod
    def tool(operation):
        return {'toolCallId': operation['id'], 'title': operation['tool_name'] or 'Model allocation',
                'kind': 'other', 'rawInput': operation['arguments'],
                '_meta': {'runId': operation['run_id'], 'requestDigest': operation['request_digest'],
                          'operationKind': operation['kind']}}

    def _approve(self, turn, view):
        operation_id = view['wait']['operation_id']
        if operation_id in turn.decisions:
            turn.cancel.wait(self.poll_seconds)
            return True
        operation = self.operation(turn, operation_id)
        if operation.get('status') != 'waiting_approval':
            raise ValueError('Hudson approval is no longer pending')
        if not self.allow_approvals:
            self.text(turn, 'Hudson approval is required. Review operation ' + operation_id +
                      ' through the authorized Hudson approval API.', **{'hudsonWait': 'approval'})
            return False
        permission_id = 'hudson-permission-' + uuid.uuid4().hex
        with self.lock:
            if turn.cancel.is_set() or self.closed.is_set():
                return True
            turn.permission_id, turn.permission_consumed = permission_id, False
        self.send({'jsonrpc': '2.0', 'id': permission_id, 'method': 'session/request_permission',
                   'params': {'sessionId': turn.session.id, 'toolCall': self.tool(operation),
                              'options': [{'optionId': 'allow-once', 'name': 'Allow this exact operation once',
                                           'kind': 'allow_once'},
                                          {'optionId': 'reject-once', 'name': 'Reject this operation',
                                           'kind': 'reject_once'}]}}, turn)
        try:
            while not turn.cancel.is_set() and not self.closed.is_set() and time.monotonic() < turn.deadline:
                try:
                    response = turn.permission.get(timeout=min(.1, self.poll_seconds))
                    break
                except queue.Empty:
                    continue
            else:
                turn.cancel.set()
                return True
        finally:
            with self.lock:
                turn.permission_id = None
        if turn.cancel.is_set() or self.closed.is_set():
            return True
        result = response.get('result')
        outcome = result.get('outcome') if isinstance(result, dict) else None
        if ('error' in response or not isinstance(outcome, dict) or outcome.get('outcome') != 'selected'
                or outcome.get('optionId') not in ('allow-once', 'reject-once')):
            # Cancelled, unknown, malformed and stale decisions never approve.
            turn.cancel.set()
            return True
        current = self.call(turn, 'run', turn.session.run)
        latest = self.operation(turn, operation_id)
        if (current.get('status') != 'waiting' or current.get('wait') != view['wait']
                or latest != operation):
            raise ValueError('Hudson approval binding changed')
        # Linearize permission submission with receipt of session/cancel. A
        # cancellation already received by the dispatcher cannot grant approval.
        with self.lock:
            if not turn.cancel.is_set() and not self.closed.is_set():
                self.call(turn, 'approve', operation_id, approved=outcome['optionId'] == 'allow-once')
                turn.decisions.add(operation_id)
        return True

    def _run(self, turn):
        session = turn.session
        cancelled = False
        try:
            if turn.cancel.is_set() or self.closed.is_set():
                self.result(turn.id, {'stopReason': 'cancelled'}, turn)
                return
            key = 'acp-' + session.id + '-' + str(session.turns)
            session.pending_key = key
            if session.run is None:
                session.uncertain = True
                submitted = self.admit(turn, 'start', turn.text, request_key=key, agent_ref=self.agent_ref)
                session.run = http._id(submitted['run_id'])
                session.uncertain = False
            else:
                previous = self.call(turn, 'run', session.run)
                if previous['status'] == 'waiting' and previous['wait']['type'] == 'user_input':
                    self.admit(turn, 'reply', session.run, turn.text, request_key=key,
                              question_id=previous['wait']['question_id'])
                elif previous['status'] == 'completed':
                    session.uncertain = True
                    submitted = self.admit(turn, 'fork', session.run, turn.text,
                                          request_key=key, agent_ref=self.agent_ref)
                    session.run = http._id(submitted['run_id'])
                    session.cursor = 0
                    session.uncertain = False
                else:
                    raise ProtocolError(-32001, 'Prior Hudson run must complete or receive its pending control first')
            cancel_deadline = None
            while True:
                if self.closed.is_set() or time.monotonic() >= turn.deadline:
                    turn.cancel.set()
                if turn.cancel.is_set() and not cancelled:
                    cancelled = True
                    self.call(turn, 'cancel', session.run)
                    cancel_deadline = time.monotonic() + 5
                view = self.call(turn, 'run', session.run)
                # Read status before draining its events; cancellation can cut
                # provisional output short but cannot turn it into a receipt.
                self._events(turn, drain=view['status'] in ('waiting', 'completed', 'failed', 'cancelled'))
                if turn.cancel.is_set() and not cancelled:
                    continue
                if view['status'] == 'cancelled':
                    self.finish(turn, 'cancelled', view)
                    return
                if cancelled:
                    if time.monotonic() >= cancel_deadline or view['status'] in ('completed', 'failed', 'waiting'):
                        raise ProtocolError(-32001, 'Cancellation is unresolved; inspect the Hudson run')
                elif view['status'] == 'completed':
                    final = (view['result'] if isinstance(view['result'], str)
                             else encode(view['result']).decode('utf-8'))
                    if turn.stream_operation is None or hashlib.sha256(final.encode('utf-8')).digest() != turn.stream_digest.digest():
                        self.text(turn, final, verified=True)
                    self.finish(turn, 'end_turn', view)
                    return
                elif view['status'] == 'failed':
                    raise ProtocolError(-32001, 'Hudson run failed; inspect its recorded outcome')
                elif view['status'] == 'waiting':
                    wait = view['wait']
                    if wait['type'] == 'approval':
                        if self._approve(turn, view):
                            continue
                    elif wait['type'] == 'user_input':
                        self.text(turn, wait['prompt'], hudsonWait='user_input', questionId=wait['question_id'])
                    else:
                        raise ProtocolError(-32001, 'Hudson run requires operator reconciliation')
                    self.finish(turn, 'end_turn', view)
                    return
                turn.cancel.wait(self.poll_seconds)
        except Exception as error:
            # Transport failure is ambiguous: never retry a mutation, model call,
            # tool call or approval, and never expose provider/credential text.
            code = error.code if isinstance(error, ProtocolError) else -32603
            message = error.message if isinstance(error, ProtocolError) else 'Hudson request failed; inspect the run before retrying'
            try:
                self.error(turn.id, code, message, session.run,
                           session.pending_key if session.uncertain else None)
            except Exception:
                self.closed.set()
        finally:
            if (self.closed.is_set() or turn.cancel.is_set()) and session.run is not None and not cancelled:
                # Disconnect is a cancellation request, never a cancellation
                # receipt. A failed cancel is not retried or reported as success.
                try:
                    self.call(turn, 'cancel', session.run)
                except Exception:
                    pass
            with self.lock:
                if self.active is turn:
                    self.active = None

    def finish(self, turn, reason, view):
        # ACP usage_update is context occupancy/cumulative cost, not input/output
        # usage. Preserve Hudson's reported totals in namespaced metadata instead.
        self.result(turn.id, {'stopReason': reason, '_meta': {META: {
            'runId': turn.session.run, 'status': view['status'], 'usage': view.get('usage'),
            'after': turn.session.cursor}}}, turn)

    def close(self):
        self.closed.set()
        with self.lock:
            if self.active is not None:
                self.active.cancel.set()
        if self.worker is not None:
            self.worker.join(timeout=12)


def serve(peer, source, target):
    """Bounded input/output queues keep cancellation responsive to blocked pipes.

    Readers/writers are daemon threads: a disconnected or non-reading client
    cannot pin process shutdown. HTTP requests have their own three-second bound.
    """
    incoming, outgoing = queue.Queue(8), queue.Queue(16)
    failed = threading.Event()

    def emit(message):
        try:
            outgoing.put(encode(message) + b'\n', timeout=.5)
        except queue.Full:
            failed.set()
            raise ValueError('ACP output backpressure deadline') from None
    peer.emit = emit

    def read():
        total = 0
        try:
            for _ in range(8192):
                raw = source.readline(INPUT_BYTES + 1)
                total += len(raw)
                if not raw:
                    incoming.put(None, timeout=.5)
                    return
                if len(raw) > INPUT_BYTES or not raw.endswith(b'\n') or total > 32 * 1024 * 1024:
                    raise ValueError('ACP input ceiling or incomplete frame')
                try:
                    message = decode(raw)
                except (ValueError, UnicodeError, RecursionError):
                    message = {'parse_error': True}
                incoming.put(message, timeout=.5)
            raise ValueError('ACP message ceiling')
        except Exception:
            failed.set()

    def write():
        total = 0
        try:
            while True:
                raw = outgoing.get()
                if raw is None:
                    return
                total += len(raw)
                if total > 128 * 1024 * 1024:
                    raise ValueError('ACP connection output ceiling')
                target.write(raw)
                target.flush()
        except Exception:
            failed.set()
    reader, writer = threading.Thread(target=read, daemon=True), threading.Thread(target=write, daemon=True)
    reader.start()
    writer.start()
    try:
        while not failed.is_set() and not peer.closed.is_set():
            try:
                message = incoming.get(timeout=.1)
            except queue.Empty:
                continue
            if message is None:
                return
            if message == {'parse_error': True}:
                peer.error(None, -32700, 'Parse error')
            else:
                peer.receive(message)
    finally:
        peer.close()
        try:
            outgoing.put(None, timeout=.5)
        except queue.Full:
            pass
        writer.join(timeout=1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', default='http://127.0.0.1:4318')
    parser.add_argument('--token-env', help='Operator-owned scoped Hudson API token environment name')
    parser.add_argument('--cwd', default=os.getcwd(), help='Only accepted ACP session working directory')
    parser.add_argument('--agent', help='Published host agent name (requires --agent-version)')
    parser.add_argument('--agent-version', type=int)
    parser.add_argument('--allow-approvals', action='store_true', help='Forward explicit one-operation decisions using the configured token')
    args = parser.parse_args()
    if (args.agent is None) != (args.agent_version is None):
        parser.error('--agent and --agent-version are required together')
    reference = None
    if args.agent is not None:
        reference = {'name': http._name(args.agent),
                     'version': http._integer(args.agent_version, 1, 2**64 - 1, 'agent version')}
    client = http.HudsonClient(args.url, token_env=args.token_env, timeout=3, max_response_bytes=FRAME_BYTES)
    peer = Peer(client, None, args.cwd, allow_approvals=args.allow_approvals, agent_ref=reference)
    serve(peer, sys.stdin.buffer, sys.stdout.buffer)


if __name__ == '__main__':
    try:
        main()
    except Exception:
        sys.stderr.write('Hudson ACP connection failed; no request is retried automatically.\n')
        raise SystemExit(1) from None
