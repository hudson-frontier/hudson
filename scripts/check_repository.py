"""Check Hudson's license declarations and synchronized release versions."""

import hashlib
import json
import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent


def read_toml(path):
    with path.open('rb') as stream:
        return tomllib.load(stream)


def check(root=ROOT):
    errors = []

    def require(condition, message):
        if not condition:
            errors.append(message)

    license_text = (root / 'LICENSE').read_text(encoding='utf-8')
    # Normalize checkout line endings, but require the complete upstream license.
    require(hashlib.sha256(license_text.encode()).hexdigest() ==
            'cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30', 'LICENSE must contain the unmodified Apache-2.0 text')
    require((root / 'integrations/harbor/LICENSE').read_text(encoding='utf-8') == license_text,
            'The Harbor distribution must carry the same Apache-2.0 license')
    workspace = read_toml(root / 'Cargo.toml')['workspace']
    package = workspace['package']
    version = package['version']
    require(re.fullmatch(r'\d+\.\d+\.\d+', version), 'Workspace version must be stable SemVer')
    require(package.get('license') == 'Apache-2.0', 'Workspace license must be Apache-2.0')
    require(package.get('publish') is False, 'Crate publication is not enabled by release automation')
    names = set()
    for member in workspace['members']:
        manifest = read_toml(root / member / 'Cargo.toml')['package']
        names.add(manifest['name'])
        for field in ('version', 'license', 'repository', 'publish'):
            require(manifest.get(field) == {'workspace': True},
                    f'{member}: {field} must inherit from the workspace')
    locked = [item for item in read_toml(root / 'Cargo.lock')['package']
              if item['name'] in names and 'source' not in item]
    require({item['name'] for item in locked} == names, 'Cargo.lock must contain every workspace crate')
    require(all(item['version'] == version for item in locked), 'Workspace versions in Cargo.lock differ')
    harbor = read_toml(root / 'integrations/harbor/pyproject.toml')['project']
    require(harbor.get('license') == 'Apache-2.0', 'Harbor license must be Apache-2.0')
    require(harbor['version'] == version, 'Harbor and Rust versions differ')
    require((root / 'version.txt').read_text().strip() == version, 'version.txt differs from Rust version')
    manifest = json.loads((root / '.release-please-manifest.json').read_text())
    require(manifest == {'.': version}, 'Release manifest differs from workspace version')
    labels = json.loads((root / '.github/labels.json').read_text())
    require(len({label['name'] for label in labels}) == len(labels), 'Label names must be unique')
    for label in labels:
        require(re.fullmatch(r'[0-9A-Fa-f]{6}', label['color']), f'Invalid label color: {label["name"]}')
    # Workflow dependencies must be immutable; Dependabot updates these pins.
    for path in (root / '.github/workflows').glob('*.yml'):
        for action in re.findall(r'^\s*(?:- )?uses:\s*(\S+)', path.read_text(), re.MULTILINE):
            require(re.fullmatch(r'[\w.-]+/[\w./-]+@[a-f0-9]{40}', action),
                    f'{path.name}: pin {action} to a full commit SHA')
    return errors


if __name__ == '__main__':
    try:
        failures = check()
    except (OSError, ValueError, KeyError, TypeError) as error:
        sys.exit(f'Repository policy failed: {error}')
    if failures:
        sys.exit('\n'.join(failures))
    print('PASS: Apache-2.0 licensing, release versions, labels, and action pins')
