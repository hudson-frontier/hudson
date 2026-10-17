"""Regressions for release synchronization when workspace membership changes."""
import json
import pathlib
import shutil
import tempfile
import unittest

import check_repository


class ReleasePolicyTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='hudson-repository-policy-')
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name)
        source = check_repository.ROOT
        paths = ['LICENSE', 'integrations/harbor/LICENSE', 'Cargo.toml', 'Cargo.lock',
                 'integrations/harbor/pyproject.toml', 'version.txt',
                 '.release-please-manifest.json', '.github/labels.json', 'release-please-config.json']
        paths += [member + '/Cargo.toml' for member in
                  check_repository.read_toml(source / 'Cargo.toml')['workspace']['members']]
        paths += [str(path.relative_to(source)) for path in (source / '.github/workflows').glob('*.yml')]
        for name in paths:
            destination = self.root / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / name, destination)

    def test_current_release_covers_workspace_crates(self):
        self.assertEqual(check_repository.check(self.root), [])

    def test_new_workspace_crate_cannot_be_omitted_from_release_versions(self):
        path = self.root / 'release-please-config.json'
        release = json.loads(path.read_text())
        updater = next(item for item in release['packages']['.']['extra-files']
                       if item['path'] == 'Cargo.lock')
        updater['jsonpath'] = updater['jsonpath'].replace(
            " || @.name.value=='hudson-sandbox-transport'", '')
        path.write_text(json.dumps(release))
        self.assertIn('Release Cargo.lock allowlist must match every workspace crate',
                      check_repository.check(self.root))

    def test_external_packages_cannot_be_added_to_workspace_release_versions(self):
        path = self.root / 'release-please-config.json'
        release = json.loads(path.read_text())
        updater = next(item for item in release['packages']['.']['extra-files']
                       if item['path'] == 'Cargo.lock')
        updater['jsonpath'] = updater['jsonpath'].replace(
            '))].version', " || @.name.value=='sandbox-client'))].version")
        path.write_text(json.dumps(release))
        self.assertIn('Release Cargo.lock allowlist must match every workspace crate',
                      check_repository.check(self.root))


if __name__ == '__main__':
    unittest.main()
