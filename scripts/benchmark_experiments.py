"""Compare retained experiment resources with release after each tick; local fixtures only."""
import argparse
import hashlib
import json
import os
import pathlib
import re
import statistics
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent


def source_digest():
    paths = [ROOT / 'Cargo.toml', ROOT / 'Cargo.lock', ROOT / 'rust-toolchain.toml']
    paths += sorted((ROOT / 'crates').rglob('*.rs'))
    paths += sorted((ROOT / 'crates').glob('*/Cargo.toml'))
    digest = hashlib.sha256()
    for path in paths:
        digest.update(str(path.relative_to(ROOT)).replace('\\', '/').encode())
        digest.update(b'\0')
        digest.update(path.read_bytes())
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rounds', type=int, choices=range(1, 6), default=3)
    args = parser.parse_args()
    env = os.environ.copy()
    for name in ('OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GEMINI_API_KEY', 'GOOGLE_API_KEY', 'HUDSON_TEST_DATABASE', 'HUDSON_EXPERIMENT_RELEASE_EACH_TICK'):
        env.pop(name, None)
    before = source_digest()
    build = subprocess.run(['cargo', 'test', '--locked', '-p', 'hudson-core', '--all-features', '--test', 'experiments',
                            '--no-run', '--message-format=json'], cwd=ROOT, env=env, capture_output=True, text=True, timeout=1200)
    if build.returncode:
        raise SystemExit(build.stderr)
    executables = []
    for line in build.stdout.splitlines():
        value = json.loads(line)
        if value.get('reason') == 'compiler-artifact' and value.get('executable') and value['target']['name'] == 'experiments':
            executables.append(value['executable'])
    if len(executables) != 1:
        raise SystemExit('expected one experiments workload executable')
    executable = executables[0]
    samples = {'retained': [], 'released_each_tick': []}
    for index in range(args.rounds):
        for release in ([True, False] if index % 2 == 0 else [False, True]):
            run_env = env.copy()
            if release:
                run_env['HUDSON_EXPERIMENT_RELEASE_EACH_TICK'] = '1'
            result = subprocess.run([executable, '--exact', 'fixture_experiment_workload', '--ignored', '--nocapture'],
                                    cwd=ROOT, env=run_env, capture_output=True, text=True, timeout=180)
            if result.returncode:
                raise SystemExit(result.stdout + result.stderr)
            match = re.search(r'experiment_workload release_each_tick=(true|false) samples_us=(\[[0-9, ]+\])', result.stdout)
            if not match or (match[1] == 'true') != release:
                raise SystemExit('missing or mismatched workload report')
            values = json.loads(match[2])
            if len(values) != 5 or any(value <= 0 for value in values):
                raise SystemExit('invalid workload timing samples')
            samples['released_each_tick' if release else 'retained'].extend(values)
    if source_digest() != before:
        raise SystemExit('runtime source changed during comparison')
    report = {'runtime_source_digest': before, 'executable_sha256': hashlib.sha256(pathlib.Path(executable).read_bytes()).hexdigest(),
              'workload': 'one probe, three held-out cases, four editing runs, fourteen runs, twenty-four local model requests; five experiments per invocation',
              'rounds': args.rounds, 'cargo_features': 'all', 'samples_us': samples,
              'median_us': {name: statistics.median(values) for name, values in samples.items()},
              'profile_environment': {name: env.get(name) for name in ('CARGO_INCREMENTAL', 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_PROFILE_TEST_DEBUG', 'CARGO_BUILD_JOBS')}}
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
