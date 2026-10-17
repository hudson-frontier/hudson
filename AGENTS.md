# Working on Hudson

Follow [CONTRIBUTING.md](CONTRIBUTING.md) and [docs/development.md](docs/development.md).

- Work on a focused branch. Never push directly to `main` or bypass its protections.
- Write clear, efficient Rust. Bound memory, payloads, retries, concurrency, and
  external calls. Preserve cancellation, authorization, idempotency, and recovery.
- Measure performance changes with a reproducible workload; include before/after
  results. Avoid speculative optimization, unnecessary cloning, and blocking IO
  on async executors. Explain meaningful complexity and resource tradeoffs.
- Add regression coverage for changed behavior and update relevant documentation.
- Run `python3 scripts/check.py --database YOUR_TEST_DATABASE --temporal` and
  `python3 scripts/check_repository.py` before opening a PR. Push the branch and
  wait for every check in its latest `Harness checks` run to pass before creating
  the PR. Never describe skipped or unavailable checks as passed.
- Use a Conventional Commit PR title. All required checks must pass on the latest
  revision, review conversations must be resolved, and merges must use squash.
- Let the release bot update versions and changelogs. Keep Apache-2.0 metadata
  intact; do not add incompatible third-party code or credentials.
