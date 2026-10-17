# Contributing to Hudson

Bug reports, documentation, tests, and focused improvements are welcome. Follow
our [Code of Conduct](CODE_OF_CONDUCT.md). For a substantial API or architecture
change, open an issue first so maintainers can discuss the approach.

## Issues

Use the bug or feature issue form, search for duplicates, and include enough
information to reproduce the problem. Keep credentials and customer data out of
reports. Report vulnerabilities through [Security](SECURITY.md).

The bot assigns `triage`, a change type, and applicable component labels.
Maintainers remove `triage` after assessment and may add `good first issue` or
`help wanted`. Labels describe work; they do not imply approval or priority.

## Development and code quality

Install the pinned Rust toolchain, Python 3.12 or newer, Node.js 22 or newer, and
`protoc`. Full integration checks also need PostgreSQL on a local `/tmp` socket
and a dedicated, disposable test database. Linux or WSL is recommended because
the database and process-recovery checks use Unix facilities. See the
[development guide](docs/development.md) for setup and test coverage.

Keep changes small and use existing ownership and error-handling conventions.
Test observable behavior and failure paths. Preserve authorization, cancellation,
durable recovery, idempotency, and bounded resource usage. Avoid unnecessary
allocations/clones, repeated parsing, unbounded collections or retries, and
blocking IO inside async tasks. Optimize measured bottlenecks; include a
reproducible workload and before/after latency, throughput, or memory results
when claiming a performance improvement. Do not trade correctness for speed.

## Before opening a pull request

1. Create a branch from current `main`; never commit directly to `main`.
2. Add relevant regression coverage and update documentation.
3. Run the same checks used in CI:

   ```sh
   python3 scripts/check_repository.py
   node --test .github/scripts/*.test.cjs
   python3 scripts/check.py --database YOUR_TEST_DATABASE --temporal
   ```

   `python3 scripts/check.py` is a useful local subset, but skips database and
   durable Temporal tests. CI also runs actionlint on all workflows and the
   [Harbor integration tests](integrations/harbor/README.md#local-validation-without-paid-calls)
   with Python 3.13 and the real worker.
4. Push your branch. The `Harness checks` workflow runs on every branch push.
   Wait until **every job passes for the latest pushed commit**, then open a PR
   against `main`. Contributors using forks should enable Actions in their fork
   and check that run before opening the upstream PR. GitHub does not prevent
   opening a PR with failing checks; this is a contributor requirement, and
   branch protection enforces the merge gate.
5. Complete the PR template, link the issue, and describe verification and any
   compatibility or performance impact. Repeat checks after changes.

Use a Conventional Commit title, for example `fix(core): preserve cancellation`
or `feat(cli): add run filtering`. Accepted types are `feat`, `fix`, `perf`,
`docs`, `test`, `refactor`, `build`, `ci`, `chore`, and `revert`. Add `!` for a
breaking change and explain migration in the PR body. The squash commit uses
the PR title, so keep it accurate throughout review.

## Review and merge

`main` requires a PR, resolved conversations, an up-to-date branch, and successful
`CI` and `PR policy` checks. Members with write access may squash-merge their own
PRs once these requirements pass; approval from another member is not required.
Dependabot PRs receive a bot approval and squash auto-merge; GitHub waits for the
same required checks and an up-to-date branch. Failed updates remain unmerged.
`CI` fails if any validation job fails, is cancelled, or is skipped.
Administrators and bots have no bypass. Force pushes and branch deletion are
blocked. Only squash merging is enabled; do not use merge commits or rebase
merges. Maintainers review correctness, tests, resource bounds, and compatibility.
GitHub automatically deletes merged PR branches. After merging, switch back to
`main`, update it, and prune obsolete local branches; retain work that has not
been merged.

## Versions and releases

The release bot runs after successful CI on `main`, maintains a release PR, and
updates the shared Rust version, local Cargo.lock entries, Harbor integration
version, `version.txt`, and changelog together. Do not bump these by hand in
feature PRs. `fix` and `perf` produce patch releases, `feat` produces minor
releases, and breaking changes produce major releases (including before 1.0).
Documentation and maintenance changes join the next release.

Release PRs receive the same required checks as other PRs, with no required
approval from another member. Once the release PR is squash-merged and `main`
CI passes, the bot creates its tag and
GitHub release. Crates and Python packages are not automatically published.
See [repository automation](docs/repository-automation.md) for setup and recovery.

## License

By submitting a contribution, you agree that your contribution is provided under
Hudson's [Apache License 2.0](LICENSE). Keep third-party copyright and license
notices and document new dependency licensing. No CLA is currently required.
