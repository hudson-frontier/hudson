"""Bounded stdio RPC for an operator-installed app-server.

SPDX-License-Identifier: Apache-2.0
"""
import json
import os
import pathlib
import queue
import subprocess
import threading
import time

LINE_BYTES = 1024 * 1024
TOTAL_BYTES = 8 * 1024 * 1024
MAX_EVENTS = 4096


def encode(value):
    result = bytearray()
    for part in json.JSONEncoder(ensure_ascii=False, allow_nan=False,
                                 separators=(',', ':')).iterencode(value):
        data = part.encode('utf-8')
        if len(data) > LINE_BYTES - len(result):
            raise ValueError('RPC request byte ceiling')
        result.extend(data)
    return bytes(result)


def decode(raw):
    value = json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError('JSON constant')))
    if not isinstance(value, dict):
        raise ValueError('RPC object required')
    return value


class RpcProcess:
    """One in-flight request, bounded readers, no automatic retries.

    Hudson's enclosing contained stdio process owns descendant lifetime. Do not
    detach children into new sessions; that would escape its Unix process group.
    """
    def __init__(self, executable, args, cwd, env, timeout=120):
        if (not pathlib.Path(executable).is_absolute() or not pathlib.Path(executable).is_file()
                or not pathlib.Path(cwd).is_absolute() or not 0 < timeout <= 300
                or len(args) > 64 or sum(len(arg) for arg in args) > 32768):
            raise ValueError('invalid RPC process configuration')
        self.deadline = time.monotonic() + timeout
        self.events = queue.Queue(8)
        self.stopping = threading.Event()
        self.error = None
        self.next_id = 1
        self.process = subprocess.Popen([str(executable), *args], cwd=cwd, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            shell=False, close_fds=True, bufsize=0)
        self.readers = [threading.Thread(target=self._read_stdout, daemon=True),
                        threading.Thread(target=self._read_stderr, daemon=True)]
        for reader in self.readers:
            reader.start()

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('RPC process deadline')
        return remaining

    def _put(self, value):
        while not self.stopping.is_set() and time.monotonic() < self.deadline:
            try:
                self.events.put(value, timeout=.1)
                return
            except queue.Full:
                pass

    def _read_stdout(self):
        total, count = 0, 0
        pending = bytearray()
        try:
            while not self.stopping.is_set():
                chunk = os.read(self.process.stdout.fileno(), 4096)
                if not chunk:
                    if pending:
                        raise ValueError('incomplete RPC frame')
                    self._put(None)
                    return
                total += len(chunk)
                if total > TOTAL_BYTES:
                    raise ValueError('RPC output byte ceiling')
                pending.extend(chunk)
                while True:
                    end = pending.find(b'\n')
                    if end < 0:
                        if len(pending) > LINE_BYTES:
                            raise ValueError('RPC frame byte ceiling')
                        break
                    if end + 1 > LINE_BYTES or count >= MAX_EVENTS:
                        raise ValueError('RPC frame or event ceiling')
                    self._put(decode(pending[:end]))
                    del pending[:end + 1]
                    count += 1
        except (OSError, ValueError, RecursionError):
            self.error = 'RPC output is invalid or exceeds limits'
            self._put(None)

    def _read_stderr(self):
        total = 0
        try:
            while not self.stopping.is_set():
                chunk = self.process.stderr.read(4096)
                if not chunk:
                    return
                total += len(chunk)
                if total > 65536:
                    self.error = 'RPC diagnostic byte ceiling'
                    self._put(None)
                    return
        except OSError:
            pass

    def receive(self):
        if self.error is not None:
            raise ValueError(self.error)
        try:
            value = self.events.get(timeout=self.remaining())
        except queue.Empty:
            raise TimeoutError('RPC response deadline') from None
        if self.error is not None or value is None:
            raise ValueError(self.error or 'RPC process closed before completion')
        return value

    def send(self, value):
        # The request and its flush are bounded by the enclosing bridge process
        # deadline. Never launch this synchronous peer on an async executor.
        self.remaining()
        data = memoryview(encode(value) + b'\n')
        while data:
            written = self.process.stdin.write(data)
            if not written:
                raise ValueError('RPC input closed')
            data = data[written:]
        self.process.stdin.flush()

    def call(self, method, params, on_event):
        identifier = self.next_id
        self.next_id += 1
        self.send({'id': identifier, 'method': method, 'params': params})
        while True:
            event = self.receive()
            if event.get('id') == identifier and 'method' not in event:
                if 'error' in event or 'result' not in event:
                    raise ValueError('RPC request was rejected')
                return event['result']
            on_event(event)

    def deny(self, event):
        if 'id' in event:
            self.send({'id': event['id'], 'error': {
                'code': -32601, 'message': 'Execution belongs to the host'}})

    def close(self):
        self.stopping.set()
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        for reader in self.readers:
            reader.join(timeout=1)
        for stream in [self.process.stdin, self.process.stdout, self.process.stderr]:
            # Unbuffered FileIO avoids a BufferedReader lock held by a blocked
            # daemon thread while closing a descendant-held pipe.
            stream.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
