"""Detect unreviewed preview HTTP contract drift, excluding prose/release metadata."""
import hashlib
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def contract_digest(spec):
    # Host mode changes auth/submission schemas dynamically; those are covered by
    # fixture tests. Keep every static path, schema, parameter and security declaration.
    value = {key: item for key, item in spec.items() if key not in {'info', 'servers'}}
    encoded = json.dumps(value, sort_keys=True, separators=(',', ':')).encode()
    return hashlib.sha256(encoded).hexdigest()


def check(root=ROOT):
    spec = json.loads((root / 'docs/openapi.json').read_text(encoding='utf-8'))
    baseline = json.loads((root / 'docs/api-compatibility.json').read_text(encoding='utf-8'))
    if contract_digest(spec) != baseline['contract_sha256']:
        raise ValueError('HTTP contract drift: review compatibility and update docs/api-compatibility.json explicitly')
    return baseline['contract_id']


if __name__ == '__main__':
    try:
        print('PASS: HTTP contract matches ' + check())
    except (OSError, ValueError, KeyError) as error:
        sys.exit(str(error))
