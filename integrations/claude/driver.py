"""Credential-free child driving the installed Claude Agent SDK once.

SPDX-License-Identifier: Apache-2.0
"""
import asyncio
import dataclasses
import importlib.metadata
import json
import os
import pathlib
import sys

SDK_VERSION = '0.2.175'
MAX_BYTES = 8 * 1024 * 1024


def encoded(value):
    result = bytearray()
    for part in json.JSONEncoder(ensure_ascii=False, allow_nan=False, separators=(',', ':')).iterencode(value):
        raw = part.encode('utf-8')
        if len(raw) > MAX_BYTES - len(result):
            raise ValueError('native SDK receipt ceiling')
        result.extend(raw)
    return bytes(result)


async def run(config):
    import claude_agent_sdk
    from claude_agent_sdk import (AssistantMessage, ClaudeAgentOptions, HookMatcher,
        PermissionResultDeny, ResultMessage, SystemMessage, create_sdk_mcp_server, query, tool)
    if importlib.metadata.version('claude-agent-sdk') != SDK_VERSION:
        raise ValueError('unsupported Claude SDK version')
    invocations, hooks, assistants, results, init = [], [], [], [], []

    class Store:
        def __init__(self):
            self.key, self.entries, self.bytes = None, [], 0
        async def append(self, key, entries):
            if key.get('subpath') is not None or (self.key is not None and key != self.key):
                raise ValueError('native SDK created an unadmitted session')
            if len(self.entries) + len(entries) > 8192:
                raise ValueError('native SDK session event ceiling')
            self.bytes += len(encoded(entries))
            if self.bytes > MAX_BYTES:
                raise ValueError('native SDK session byte ceiling')
            self.key = dict(key)
            self.entries.extend(entries)
        async def load(self, _key):
            # A vendor pending-tool resume must never execute ahead of Hudson.
            raise ValueError('native SDK resume is not authorized')

    async def deferred_tool(arguments):
        invocations.append(True)
        raise ValueError('customer tool execution belongs to Hudson')

    async def defer(input, _identifier, _context):
        if len(hooks) >= 128:
            raise ValueError('native SDK hook ceiling')
        hooks.append({'name': input.get('tool_name'), 'input': input.get('tool_input'),
                      'id': input.get('tool_use_id')})
        return {'hookSpecificOutput': {'hookEventName': 'PreToolUse',
                'permissionDecision': 'defer', 'permissionDecisionReason': 'Execution belongs to Hudson'}}

    async def deny(_name, _arguments, _context):
        return PermissionResultDeny(message='Execution belongs to Hudson', interrupt=True)

    tools = [tool(item['alias'], item['description'], item['input_schema'])(deferred_tool)
             for item in config['tools']]
    store = Store()
    options = ClaudeAgentOptions(model=config['model'], tools=[],
        cli_path=str(pathlib.Path(claude_agent_sdk.__file__).parent / '_bundled/claude'),
        system_prompt={'type': 'file', 'path': str(pathlib.Path('instructions.txt').resolve())},
        mcp_servers={'hudson': create_sdk_mcp_server(name='hudson', version='1', tools=tools)} if tools else {},
        strict_mcp_config=True, setting_sources=[], permission_mode='default',
        max_turns=1, hooks={'PreToolUse': [HookMatcher(hooks=[defer], timeout=5)]},
        can_use_tool=deny, cwd=os.getcwd(), session_store=store, session_store_flush='eager',
        max_buffer_size=1024 * 1024, extra_args={'disable-slash-commands': None, 'restricted': None})
    failure = False
    retained = 0
    try:
        async for message in query(prompt='Process the admitted Hudson model request.', options=options):
            if isinstance(message, AssistantMessage):
                values = []
                for block in message.content:
                    kind = type(block).__name__
                    fields = dataclasses.asdict(block)
                    name = {'TextBlock': 'text', 'ToolUseBlock': 'tool_use', 'ThinkingBlock': 'thinking',
                            'RedactedThinkingBlock': 'redacted_thinking'}.get(kind)
                    if name is None:
                        raise ValueError('unsupported native SDK block')
                    values.append(dict(fields, type=name))
                assistants.append({'message_id': message.message_id, 'content': values, 'error': message.error})
                retained += len(encoded(assistants[-1]))
            elif isinstance(message, ResultMessage):
                results.append(dataclasses.asdict(message))
                retained += len(encoded(results[-1]))
            elif isinstance(message, SystemMessage) and message.subtype == 'init':
                init.append(message.data)
            if len(assistants) > 1024 or len(results) > 1 or retained > MAX_BYTES:
                raise ValueError('native SDK message ceiling')
    except Exception:
        failure = True
    receipt = {'sdk_version': SDK_VERSION, 'invocations': len(invocations), 'hooks': hooks,
               'assistants': assistants, 'results': results, 'key': store.key,
               'entries': store.entries, 'failure': failure, 'init': init}
    raw = encoded(receipt)
    fd = os.open('native.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'wb') as target:
        target.write(raw)
        target.flush()
        os.fsync(target.fileno())
    sys.stdout.write('{"type":"complete"}\n')
    sys.stdout.flush()


def main():
    raw = pathlib.Path('request.json').read_bytes()
    if len(raw) > 4 * 1024 * 1024:
        raise ValueError('native SDK input ceiling')
    asyncio.run(run(json.loads(raw)))


if __name__ == '__main__':
    try:
        main()
    except Exception:
        sys.stderr.write('Claude SDK child failed without authorizing a retry.\n')
        raise SystemExit(1) from None
