"""Keep backend conformance checks independent of external client installation."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import operator_cli


class OperatorClientTests(unittest.TestCase):
    def test_default_selects_private_fixture_in_selected_build_directory(self):
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(operator_cli.command('build/debug'),
                             [str(Path('build/debug/hudson-api-fixture'))])

    def test_explicit_external_binary_gets_agent_command_group(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory, 'hudson')
            binary.touch()
            with patch.dict(os.environ, {'HUDSON_TEST_OPERATOR_CLI': str(binary)}):
                self.assertEqual(operator_cli.command(), [str(binary), 'agent'])

    def test_invalid_override_fails_instead_of_falling_back(self):
        for value in ['hudson', str(Path(tempfile.gettempdir()) / 'missing-hudson-binary')]:
            with self.subTest(value=value), patch.dict(os.environ, {'HUDSON_TEST_OPERATOR_CLI': value}):
                with self.assertRaises(ValueError):
                    operator_cli.command()
