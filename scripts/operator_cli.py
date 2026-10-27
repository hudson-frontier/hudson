"""Select the private API fixture or an explicitly supplied standalone client."""
import os
from pathlib import Path


def command(binary_directory='target/debug'):
    operator = os.environ.get('HUDSON_TEST_OPERATOR_CLI')
    if operator:
        path = Path(operator)
        if not path.is_absolute() or not path.is_file():
            raise ValueError('HUDSON_TEST_OPERATOR_CLI must name an existing absolute binary path')
        return [str(path), 'agent']
    return [str(Path(binary_directory) / 'hudson-api-fixture')]
