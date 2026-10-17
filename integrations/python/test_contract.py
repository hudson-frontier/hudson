"""The contract guard must detect semantic drift without blocking release metadata."""
import copy
import importlib.util
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('api_check', ROOT / 'scripts/check_api_compatibility.py')
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


class ContractTests(unittest.TestCase):
    def test_checked_in_baseline_matches(self):
        self.assertTrue(checker.check(ROOT).startswith('hudson-preview-'))

    def test_release_metadata_is_independent_of_wire_contract(self):
        base = {'info': {'version': '0.1.0'}, 'paths': {'/runs': {}}, 'components': {}}
        updated = dict(base, info={'version': '0.2.0'}, servers=[{'url': 'http://localhost'}])
        self.assertEqual(checker.contract_digest(base), checker.contract_digest(updated))

    def test_paths_required_fields_and_properties_named_like_prose_are_guarded(self):
        base = {'paths': {'/runs': {'post': {'responses': {'202': {}}}}},
                'components': {'schemas': {'Input': {'type': 'object',
                    'properties': {'description': {'type': 'string'}}, 'required': []}}}}
        changed = copy.deepcopy(base)
        del changed['paths']['/runs']
        self.assertNotEqual(checker.contract_digest(base), checker.contract_digest(changed))
        changed = copy.deepcopy(base)
        changed['components']['schemas']['Input']['required'] = ['description']
        self.assertNotEqual(checker.contract_digest(base), checker.contract_digest(changed))
        changed = copy.deepcopy(base)
        changed['components']['schemas']['Input']['properties']['description']['type'] = 'integer'
        self.assertNotEqual(checker.contract_digest(base), checker.contract_digest(changed))


if __name__ == '__main__':
    unittest.main()
