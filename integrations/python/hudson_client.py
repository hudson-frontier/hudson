"""Bounded standard-library client for Hudson's preview HTTP API.

No request is automatically retried. Retain caller-selected request keys and
event cursors across reconnects. This module makes no provider calls itself.
SPDX-License-Identifier: Apache-2.0
"""

import http.client
import ipaddress
import json
import os
import re
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


class HudsonError(Exception):
    """Client error with no credentials, response body, or request body in its text."""


class HttpError(HudsonError):
    def __init__(self, status):
        self.status = status
        super().__init__(f'Hudson returned HTTP {status}')


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _integer(value, minimum, maximum, name):
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError(f'invalid {name}')
    return value


def _id(value):
    try:
        return str(uuid.UUID(value))
    except (ValueError, TypeError, AttributeError):
        raise ValueError('invalid resource UUID') from None


def _name(value):
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,64}', value):
        raise ValueError('invalid definition name')
    return value


def _key(value):
    if not isinstance(value, str) or not value.strip():
        raise ValueError('an explicit nonempty request key is required')
    return value


def _reject_constant(_):
    raise ValueError('nonstandard JSON constant')


class HudsonClient:
    def __init__(self, url='http://127.0.0.1:4318', *, token=None,
                 token_env=None, timeout=15, max_response_bytes=8 * 1024 * 1024):
        try:
            parsed = urllib.parse.urlsplit(url)
            host = parsed.hostname
            port = parsed.port
            try:
                loopback = ipaddress.ip_address(host).is_loopback
            except ValueError:
                loopback = host == 'localhost'
            valid = (bool(host) and
                     (parsed.scheme == 'https' or (parsed.scheme == 'http' and loopback)) and
                     parsed.username is None and parsed.password is None and
                     '?' not in url and '#' not in url and parsed.path in ('', '/') and
                     (port is None or 1 <= port <= 65535) and
                     not any(ord(c) <= 32 or ord(c) == 127 for c in url))
        except (ValueError, TypeError, AttributeError):
            valid = False
        if not valid:
            raise ValueError('API URL must be an HTTPS or loopback HTTP origin without credentials')
        if token is not None and token_env is not None:
            raise ValueError('choose token or token_env')
        if token_env is not None:
            token = os.environ.get(token_env)
            if token is None or not token:
                raise ValueError('API token environment variable is missing or empty')
        if token is not None and (not isinstance(token, str) or not token or
                                  not all(33 <= ord(c) <= 126 for c in token)):
            raise ValueError('invalid API token')
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not 0 < timeout <= 60:
            raise ValueError('timeout must be between 0 and 60 seconds')
        self.timeout = timeout
        self.max_response_bytes = _integer(max_response_bytes, 1, 8 * 1024 * 1024, 'response bound')
        self.url = url.rstrip('/')
        self._token = token
        # Do not inherit proxy configuration or forward credentials across redirects.
        self._opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())

    def __repr__(self):
        return f'HudsonClient({self.url!r})'

    def _request(self, method, path, body=None, *, max_request_bytes=64 * 1024):
        data = None
        if body is not None:
            try:
                data = json.dumps(body, ensure_ascii=False, allow_nan=False,
                                  separators=(',', ':')).encode('utf-8')
            except (TypeError, ValueError, UnicodeError):
                raise ValueError('request body must be JSON') from None
            if len(data) > max_request_bytes:
                raise ValueError('request body exceeds configured byte bound')
        headers = {'Accept': 'application/json'}
        if data is not None:
            headers['Content-Type'] = 'application/json'
        if self._token is not None:
            headers['Authorization'] = 'Bearer ' + self._token
        request = urllib.request.Request(self.url + path, data=data, method=method, headers=headers)
        try:
            with self._opener.open(request, timeout=self.timeout) as response:
                # Check status before parsing: framework errors may be plain text.
                if not 200 <= response.status < 300:
                    raise HttpError(response.status)
                raw = response.read(self.max_response_bytes + 1)
                # A bounded read can return at EOF without rejecting a short body.
                # Oversized responses retain their separate byte-bound error below.
                if len(raw) <= self.max_response_bytes and response.length not in (None, 0):
                    raise http.client.HTTPException('incomplete response body')
        except urllib.error.HTTPError as error:
            status = error.code
            error.close()
            raise HttpError(status) from None
        except (urllib.error.URLError, OSError, http.client.HTTPException):
            raise HudsonError('HTTP transport failed; a submitted effect may have an unknown outcome') from None
        if len(raw) > self.max_response_bytes:
            raise HudsonError('response exceeds configured byte bound')
        try:
            return json.loads(raw, parse_constant=_reject_constant)
        except (ValueError, UnicodeError, RecursionError):
            raise HudsonError('successful response is not valid JSON') from None

    def health(self):
        return self._request('GET', '/health')

    def harnesses(self):
        return self._request('GET', '/harnesses')

    def capabilities(self):
        return self._request('GET', '/capabilities')

    def publish_tool(self, tool, *, request_key):
        return self._request('POST', '/tools', {'tool': tool, 'request_key': _key(request_key)})

    def publish_agent(self, agent, *, request_key):
        return self._request('POST', '/agents', {'agent': agent, 'request_key': _key(request_key)})

    def tool(self, name, version):
        version = _integer(version, 1, 2**64 - 1, 'version')
        return self._request('GET', f'/tools/{_name(name)}/versions/{version}')

    def agent(self, name, version):
        version = _integer(version, 1, 2**64 - 1, 'version')
        return self._request('GET', f'/agents/{_name(name)}/versions/{version}')

    def start(self, input, *, request_key, agent_ref=None):
        body = {'input': input, 'request_key': _key(request_key)}
        if agent_ref is not None:
            body['agent_ref'] = agent_ref
        return self._request('POST', '/runs', body)

    def start_images(self, input, images, *, request_key, agent_ref=None):
        body = {'input': input, 'images': images, 'request_key': _key(request_key)}
        if agent_ref is not None:
            body['agent_ref'] = agent_ref
        return self._request('POST', '/runs/images', body, max_request_bytes=5 * 1024 * 1024)

    def run(self, run_id):
        run_id = _id(run_id)
        view = self._request('GET', f'/runs/{run_id}')
        if not isinstance(view, dict) or view.get('id') != run_id or view.get('status') not in {
                'queued', 'running', 'waiting', 'cancelling', 'completed', 'failed', 'cancelled'}:
            raise HudsonError('invalid run identity or status')
        return view

    def events(self, run_id, *, after=0, limit=100):
        run_id = _id(run_id)
        _integer(after, 0, 2**64 - 1, 'event cursor')
        _integer(limit, 1, 1000, 'event page limit')
        value = self._request('GET', f'/runs/{run_id}/events?after={after}&limit={limit}')
        if not isinstance(value, list) or len(value) > limit:
            raise HudsonError('invalid event page')
        previous = after
        for event in value:
            if (not isinstance(event, dict) or event.get('run_id') != run_id or
                    type(event.get('sequence')) is not int or
                    not previous < event['sequence'] <= 2**64 - 1):
                raise HudsonError('invalid event identity or sequence')
            previous = event['sequence']
        return value

    def operation(self, operation_id):
        return self._request('GET', f'/operations/{_id(operation_id)}')

    def export_conversation(self, run_id):
        return self._request('GET', f'/runs/{_id(run_id)}/conversation')

    def import_conversation(self, transcript, input, *, request_key, agent_ref=None, images=None):
        body = {'transcript': transcript, 'input': input, 'request_key': _key(request_key)}
        if agent_ref is not None:
            body['agent_ref'] = agent_ref
        if images is not None:
            body['images'] = images
        return self._request('POST', '/runs/import', body, max_request_bytes=5 * 1024 * 1024)

    def fork(self, run_id, input, *, request_key, agent_ref=None, images=None):
        body = {'input': input, 'request_key': _key(request_key)}
        if agent_ref is not None:
            body['agent_ref'] = agent_ref
        if images is not None:
            body['images'] = images
        return self._request('POST', f'/runs/{_id(run_id)}/fork', body,
                             max_request_bytes=5 * 1024 * 1024 if images is not None else 64 * 1024)

    def stream_events(self, run_id, *, after=0, max_events=10000, max_seconds=300):
        """Observe one bounded SSE connection. Save each sequence before reconnecting.

        Closing this generator closes observation only. There are no implicit
        reconnects, effects, or retries. Partial model output is provisional.
        """
        run_id = _id(run_id)
        _integer(after, 0, 2**64 - 1, 'event cursor')
        _integer(max_events, 1, 10000, 'stream event budget')
        _integer(max_seconds, 1, 300, 'stream duration')
        headers = {'Accept': 'text/event-stream', 'Last-Event-ID': str(after)}
        if self._token is not None:
            headers['Authorization'] = 'Bearer ' + self._token
        request = urllib.request.Request(
            self.url + f'/runs/{run_id}/events/stream', headers=headers)
        started = time.monotonic()
        total = count = 0
        data, sequence, event_type = [], None, None
        try:
            with self._opener.open(request, timeout=self.timeout) as response:
                if not 200 <= response.status < 300:
                    raise HttpError(response.status)
                if response.headers.get_content_type() != 'text/event-stream':
                    raise HudsonError('unexpected event stream content type')
                while True:
                    if time.monotonic() - started >= max_seconds:
                        raise HudsonError(f'event stream duration exhausted; last cursor {after}')
                    line = response.readline(self.max_response_bytes - total + 1)
                    total += len(line)
                    if total > self.max_response_bytes:
                        raise HudsonError('event stream exceeds configured byte bound')
                    if not line:
                        if data or sequence is not None or event_type is not None:
                            raise HudsonError('event stream ended inside a frame')
                        return
                    if not line.endswith(b'\n'):
                        raise HudsonError('event stream ended inside a line')
                    try:
                        line = line[:-1].removesuffix(b'\r').decode('utf-8')
                    except UnicodeError:
                        raise HudsonError('event stream is not UTF-8') from None
                    if not line:
                        if not data:
                            sequence, event_type = None, None
                            continue
                        try:
                            event = json.loads('\n'.join(data), parse_constant=_reject_constant)
                        except (ValueError, UnicodeError, RecursionError):
                            raise HudsonError('invalid event stream JSON') from None
                        if (event_type != 'event' or not isinstance(event, dict) or
                                event.get('run_id') != run_id or
                                type(event.get('sequence')) is not int or
                                not after < event['sequence'] <= 2**64 - 1 or
                                sequence != str(event['sequence'])):
                            raise HudsonError('invalid event stream identity or sequence')
                        after = event['sequence']
                        count += 1
                        yield event
                        if count == max_events:
                            return
                        data, sequence, event_type = [], None, None
                    elif not line.startswith(':'):
                        name, _, value = line.partition(':')
                        value = value.removeprefix(' ')
                        if name == 'data':
                            data.append(value)
                        elif name == 'id':
                            sequence = value
                        elif name == 'event':
                            event_type = value
        except urllib.error.HTTPError as error:
            status = error.code
            error.close()
            raise HttpError(status) from None
        except (urllib.error.URLError, OSError, http.client.HTTPException):
            raise HudsonError(f'event stream transport failed; last cursor {after}') from None

    def approve(self, operation_id, *, approved):
        if type(approved) is not bool:
            raise ValueError('approved must be boolean')
        return self._request('POST', f'/operations/{_id(operation_id)}/approval', {'approved': approved})

    def reply(self, run_id, input, *, request_key, question_id):
        return self._request('POST', f'/runs/{_id(run_id)}/input', {
            'input': input, 'request_key': _key(request_key),
            'question_id': _integer(question_id, 0, 2**64 - 1, 'question identity')})

    def resume(self, run_id):
        return self._request('POST', f'/runs/{_id(run_id)}/resume')

    def queue_message(self, run_id, input, *, kind, request_key):
        """Queue steering or a follow-up; this does not approve, reply, or resume."""
        if kind not in ('steer', 'follow_up'):
            raise ValueError('message kind must be steer or follow_up')
        return self._request('POST', f'/runs/{_id(run_id)}/messages', {
            'kind': kind, 'input': input, 'request_key': _key(request_key)})

    def queued_messages(self, run_id):
        return self._request('GET', f'/runs/{_id(run_id)}/messages')

    def clear_queued_messages(self, run_id):
        return self._request('DELETE', f'/runs/{_id(run_id)}/messages')

    def cancel(self, run_id):
        return self._request('POST', f'/runs/{_id(run_id)}/cancel')

    def follow(self, run_id, *, after=0, page_size=100, max_polls=60, max_pages=1000, poll_seconds=1):
        """Read through a wait/terminal snapshot; retain each yielded `after` to reconnect.

        A wait stops observation so the caller can explicitly approve or answer.
        Closing the generator changes no run state. Page/poll budgets bound IO.
        """
        _integer(max_polls, 1, 10000, 'poll budget')
        _integer(max_pages, 1, 10000, 'page budget')
        if (isinstance(poll_seconds, bool) or not isinstance(poll_seconds, (int, float)) or
                not 0.05 <= poll_seconds <= 60):
            raise ValueError('poll interval must be between 0.05 and 60 seconds')
        pages = 0
        for poll in range(max_polls):
            # Read status first, then include every event preceding that snapshot.
            view = self.run(run_id)
            while True:
                if pages >= max_pages:
                    raise HudsonError(f'event page budget exhausted; last cursor {after}')
                page = self.events(run_id, after=after, limit=page_size)
                pages += 1
                if not page:
                    break
                for event in page:
                    yield {'type': 'event', 'after': event['sequence'], 'event': event}
                    after = event['sequence']
            yield {'type': 'run', 'after': after, 'run': view}
            if view['status'] in {'waiting', 'completed', 'failed', 'cancelled'}:
                return
            if poll + 1 < max_polls:
                time.sleep(poll_seconds)
        raise HudsonError(f'observation poll budget exhausted; last cursor {after}')
