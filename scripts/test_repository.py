"""Regressions for release policy and workspace version synchronization."""
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

    def test_backend_cannot_install_standalone_operator_binary_names(self):
        member = 'crates/hudson-api-fixture'
        path = self.root / member / 'Cargo.toml'
        original = path.read_text()
        message = f'{member}: hudson and hudson-cli binaries belong to the standalone CLI repository'
        for name in ('hudson', 'hudson-cli'):
            with self.subTest(name=name, source='explicit'):
                path.write_text(original + f'\n[[bin]]\nname = "{name}"\npath = "src/main.rs"\n')
                self.assertIn(message, check_repository.check(self.root))
            path.write_text(original)
            implicit = self.root / member / 'src/bin' / f'{name}.rs'
            implicit.parent.mkdir(parents=True, exist_ok=True)
            implicit.write_text('fn main() {}')
            with self.subTest(name=name, source='implicit'):
                self.assertIn(message, check_repository.check(self.root))
            implicit.unlink()

    def test_release_strategy_cannot_skip_patch_increments(self):
        path = self.root / 'release-please-config.json'
        release = json.loads(path.read_text())
        for strategy in (None, 'default', 'always-bump-minor', 'always-bump-major'):
            with self.subTest(strategy=strategy):
                if strategy is None:
                    release['packages']['.'].pop('versioning')
                else:
                    release['packages']['.']['versioning'] = strategy
                path.write_text(json.dumps(release))
                self.assertIn('Release automation must use patch version increments',
                              check_repository.check(self.root))

    def test_release_proposals_must_stay_drafts(self):
        path = self.root / 'release-please-config.json'
        release = json.loads(path.read_text())
        for location, draft in (('global', None), ('global', False), ('package', False)):
            with self.subTest(location=location, draft=draft):
                config = json.loads(json.dumps(release))
                target = config if location == 'global' else config['packages']['.']
                if draft is None:
                    target.pop('draft-pull-request', None)
                else:
                    target['draft-pull-request'] = draft
                path.write_text(json.dumps(config))
                self.assertIn('Release automation must prepare draft pull requests',
                              check_repository.check(self.root))

    def test_fixed_release_versions_cannot_override_patch_progression(self):
        path = self.root / 'release-please-config.json'
        release = json.loads(path.read_text())
        for location in ('global', 'package'):
            with self.subTest(location=location):
                config = json.loads(json.dumps(release))
                target = config if location == 'global' else config['packages']['.']
                target['release-as'] = '1.0.0'
                path.write_text(json.dumps(config))
                self.assertIn(
                    'Release automation must derive the next patch from the release manifest',
                    check_repository.check(self.root))

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
