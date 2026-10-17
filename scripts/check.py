"""Run local Hudson checks with loopback model stubs; never runs live_provider.py."""
import argparse
import os
import pathlib
import shlex
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LOCAL_SMOKES = ('providers', 'subagents', 'evaluation', 'python_tool', 'real_estate', 'follow', 'session')
DATABASE_SMOKES = ('approval', 'api', 'recovery', 'child_approval', 'user_input', 'memory', 'auth')
CONFIGS = ('analyst.json', 'data-agent.json', 'team.json', 'python-tool/agent.json',
           'customer-mcp.json', 'real-estate/agent.json', 'project-files/agent.json')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--database', help='Dedicated local PostgreSQL test database on /tmp; enables durable recovery checks')
    parser.add_argument('--temporal', action='store_true', help='Include real local Temporal integration tests (downloads the CLI on first use; requires --database)')
    args = parser.parse_args()
    if args.temporal and not args.database:
        parser.error('--temporal requires --database')
    env = os.environ.copy()
    # Smoke scripts use target/debug. Keep Cargo and the scripts on the same path.
    env['CARGO_TARGET_DIR'] = str(ROOT / 'target')
    for name in ('OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'GOOGLE_API_KEY', 'HUDSON_TEST_DATABASE'):
        env.pop(name, None)
    if args.database:
        env['HUDSON_TEST_DATABASE'] = args.database

    def run(*command):
        print('\n> ' + shlex.join(map(str, command)), flush=True)
        subprocess.run(command, cwd=ROOT, env=env, check=True)

    run('cargo', 'fmt', '--all', '--', '--check')
    run(sys.executable, '-m', 'unittest', 'discover', '-s', 'examples/project-files', '-v')
    test = ['cargo', 'test', '--locked', '--workspace', '--all-features', '--exclude', 'hudson-temporal']
    if args.database:
        test += ['--', '--include-ignored']
    run(*test)
    if args.temporal:
        run('cargo', 'build', '--locked', '-p', 'hudson-server', '-p', 'hudson-cli')
    temporal_test = ['cargo', 'test', '--locked', '-p', 'hudson-temporal']
    if args.temporal:
        temporal_test += ['--', '--include-ignored']
    run(*temporal_test)
    run('cargo', 'clippy', '--locked', '--workspace', '--all-targets', '--all-features', '--', '-D', 'warnings')
    run('cargo', 'check', '--locked', '-p', 'hudson-core', '--no-default-features')
    run('cargo', 'build', '--locked', '-p', 'hudson-worker', '-p', 'hudson-server', '-p', 'hudson-cli', '-p', 'hudson-temporal')
    for config in CONFIGS:
        run('target/debug/hudson-worker', '--config', 'examples/' + config, '--check')
    for name in LOCAL_SMOKES + (DATABASE_SMOKES if args.database else ()):
        run(sys.executable, 'scripts/smoke_' + name + '.py')
    print('\nPASS: local checks complete; no live-provider checks were run.')
    if not args.database:
        print('PostgreSQL and durable recovery checks were skipped. Use --database to include them.')


if __name__ == '__main__':
    try:
        main()
    except subprocess.CalledProcessError as error:
        print(f'Check failed with exit code {error.returncode}.', file=sys.stderr)
        sys.exit(1)
