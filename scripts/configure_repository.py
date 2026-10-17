"""Show or apply the checked-in GitHub settings using an administrator's gh login."""

import argparse
import json
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent


def api(endpoint, method='GET', payload=None):
    command = ['gh', 'api', '--method', method, endpoint]
    if payload is None:
        return json.loads(subprocess.check_output(command, text=True) or 'null')
    with tempfile.TemporaryDirectory(prefix='hudson-settings-') as folder:
        body = pathlib.Path(folder) / 'request.json'
        body.write_text(json.dumps(payload), encoding='utf-8')
        output = subprocess.check_output([*command, '--input', str(body)], text=True)
        return json.loads(output or 'null')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', default='hudson-infinity/hudson')
    parser.add_argument('--apply', action='store_true', help='Apply settings; otherwise only print the plan')
    args = parser.parse_args()
    settings = json.loads((ROOT / '.github/repository-settings.json').read_text())
    rules = json.loads((ROOT / '.github/main-ruleset.json').read_text())
    if not args.apply:
        print(json.dumps({'repository': args.repo, 'settings': settings, 'ruleset': rules}, indent=2))
        return
    endpoint = f'repos/{args.repo}'
    api(endpoint, 'PATCH', settings)
    # Upsert only our named ruleset. Other rulesets and classic protection remain.
    existing = api(f'{endpoint}/rulesets?per_page=100')
    matching = [rule for rule in existing if rule['name'] == rules['name'] and rule['source'] == args.repo]
    if len(matching) > 1:
        raise RuntimeError('Multiple Hudson rulesets exist; reconcile them before applying settings')
    saved = api(f'{endpoint}/rulesets/{matching[0]["id"]}', 'PUT', rules) if matching else api(
        f'{endpoint}/rulesets', 'POST', rules)
    api(f'{endpoint}/private-vulnerability-reporting', 'PUT', {})
    actual = api(endpoint)
    for key, value in settings.items():
        if actual.get(key) != value:
            raise RuntimeError(f'Setting did not persist: {key}')
    verified = api(f'{endpoint}/rulesets/{saved["id"]}')
    for key in ('enforcement', 'bypass_actors', 'conditions'):
        if verified.get(key) != rules[key]:
            raise RuntimeError(f'Ruleset setting did not persist: {key}')
    actual_rules = {rule['type']: rule for rule in verified['rules']}
    for rule in rules['rules']:
        actual_rule = actual_rules.get(rule['type'])
        if actual_rule is None or any(actual_rule.get('parameters', {}).get(key) != value
                                      for key, value in rule.get('parameters', {}).items()):
            raise RuntimeError(f'Ruleset rule did not persist: {rule["type"]}')
    print(f'Applied and verified squash-only merging and main protection: {saved["_links"]["html"]["href"]}')
    print('Enabled private vulnerability reporting.', flush=True)
    try:
        api(f'{endpoint}/actions/permissions/workflow', 'PUT', {
            'default_workflow_permissions': 'read', 'can_approve_pull_request_reviews': True,
        })
    except subprocess.CalledProcessError:
        sys.exit('Release PR creation is blocked. An organization administrator must allow Actions PR creation, '
                 'then rerun this script. Branch protection and private reporting are already applied.')
    permissions = api(f'{endpoint}/actions/permissions/workflow')
    if permissions.get('default_workflow_permissions') != 'read' or not permissions.get('can_approve_pull_request_reviews'):
        raise RuntimeError('Actions workflow permissions did not persist')
    print('Enabled release PR creation; default workflow token remains read-only.')


if __name__ == '__main__':
    main()
