# Repository automation

## Merge policy

The GitHub repository settings are declared in
[repository-settings.json](../.github/repository-settings.json), and the `main`
ruleset is in [main-ruleset.json](../.github/main-ruleset.json). Workflow files
alone do not enforce branch protection. An administrator applies these with:

```sh
python3 scripts/configure_repository.py          # preview
python3 scripts/configure_repository.py --apply  # requires authenticated gh admin access
```

The script updates only Hudson's named ruleset, preserving unrelated rules. It
enables automatic deletion of PR branches after merge. `main` remains protected
from deletion; branches with unmerged work are retained during manual cleanup. It
also enables private vulnerability reports and GitHub Actions PR creation while
keeping default workflow token permissions read-only. It verifies persisted
merge settings and the ruleset. No administrator or bot bypass is configured.

Required checks are `CI` and `PR policy`, restricted to the GitHub Actions app.
`CI` waits for **Repository policy and workflows**, **Harbor integration**, and
**Rust, PostgreSQL, and Temporal** and rejects any result other than success. When adding a validation
job, add it to `CI.needs`. Keep required check names stable or update the ruleset
at the same time. `PR policy` validates the title that becomes the squash commit.
Branches must be current with `main`, and conversations must be resolved. Members
with write access can squash-merge their own PRs without another member's
approval. Never disable required checks to get a release or dependency update in.

CI runs on all branch pushes so contributors can verify the latest commit before
opening a PR. It runs again for the proposed PR merge. A local subset cannot
replace the full PostgreSQL/Temporal run. GitHub can block merges but does not
block PR creation based on CI results.

## Labels and dependencies

[labels.json](../.github/labels.json) defines label names, colors, descriptions,
Conventional Commit types, and path prefixes. Community automation synchronizes
the catalog and labels new or edited issues and PRs, including fork PRs. It uses
paginated file metadata for component labels and adds `triage` on creation.
Labels are additive: maintainers can remove stale classifications after a scope
change. Manual and release-state labels are preserved.

The privileged `pull_request_target` job checks out only the default branch. It
never checks out a PR head, executes PR code, or inserts titles/bodies into shell
commands. Keep this boundary when changing the bot. Dependabot sends weekly
updates for Cargo, GitHub Actions, and the Harbor Python integration; it uses the
same checks and merge requirements as human changes.

## Dependabot approval and auto-merge

Native auto-merge is enabled in the repository settings. The Dependabot workflow
approves open, non-draft PRs authored by `dependabot[bot]` whose source and target
belong to this repository and whose target is `main`. It covers all configured
update types, including major versions. Approval records permission to merge
after validation; a failing update remains blocked by required CI.

The workflow runs only trusted default-branch code under `pull_request_target`.
It pins each approval to a commit, rechecks the PR after approval, and enables
GitHub's native **squash** auto-merge through `gh pr merge --auto --squash` with
an expected commit SHA. GitHub merges an already eligible PR immediately or
queues it until branch requirements pass. The workflow never bypasses checks
or executes code from the dependency branch.
Repeated events preserve an existing approval and auto-merge request.

For unattended branch updates, add an Actions secret named
`DEPENDABOT_UPDATE_TOKEN`: a fine-grained personal access token restricted to
`hudson-infinity/hudson` with **Pull requests: read and write**. The token is used
only for GitHub's branch-update API; reviews, checks, and merge requests continue
to use the built-in token. Rotate the secret before its token expires.

GitHub now places PR workflows from `GITHUB_TOKEN`-created updates behind a
maintainer approval gate. Explicitly dispatching CI does not remove that gate.
An external credential avoids it. Without the secret, new Dependabot PRs can
still be approved and queued, but automatic updates of behind branches pause
with a warning instead of creating more approval-gated runs. See GitHub's
[token trigger rules](https://docs.github.com/en/actions/concepts/security/github_token)
and [branch-update permissions](https://docs.github.com/en/rest/pulls/pulls#update-a-pull-request-branch).

The strict up-to-date requirement remains enabled. On main pushes and every
15 minutes, the workflow updates eligible branches with missing main commits
using GitHub's branch-update API and an expected head SHA. Conflicts stay blocked.
The periodic sweep also covers merges whose token suppresses push workflows.
The dedicated credential lets updated branches trigger normal CI and PR-title
checks. Missing checks on an otherwise current branch can be dispatched again.
Runs already awaiting approval from an older built-in-token update need a
maintainer to approve them once. New commits must pass CI again.
Harbor's real-worker fixture tests run in CI so Python dependency updates are
tested alongside Rust updates. API-breaking updates need a compatibility fix
before their queued merge can proceed.

After installing the workflow, process existing Dependabot PRs with:

```sh
gh workflow run dependabot-automerge.yml --ref main
```

This reuses the same bot identity and repository checks as new PR events and
leaves human PRs untouched. To pause automatic handling of an individual update,
mark its PR as draft. Marking it ready for review resumes automatic handling.

## License and versions

`scripts/check_repository.py` checks the complete Apache license text, Harbor's
distributed license, workspace license inheritance, matching release versions,
label definitions, and immutable action pins. It uses only Python's standard
library. Third-party dependencies retain their own licenses; the check does not
relicense them or replace a dependency license review.

[Release Please](https://github.com/googleapis/release-please-action) runs only
after successful push CI on current `main`. It updates one release PR for the
whole repository. Conventional squash titles determine SemVer bumps: features
are minor, fixes/performance are patch, and breaking changes are major. The
initial manifest is `0.1.0`; the bootstrap SHA excludes older, nonconventional
history. Versions and changelogs change only through the release PR, which must
pass the same merge requirements as other changes.

The `simple` strategy plus TOML extra-file updaters preserves inherited
`version.workspace = true`. It updates `Cargo.toml`, the six local packages in
`Cargo.lock`, the Harbor `pyproject.toml`, and `version.txt` together. Do not use
the upstream `rust` strategy without checking virtual workspace inheritance.
The lockfile selector uses `name.value` because Release Please's TOML parser
wraps scalar values with source positions. Retest the selector when upgrading
Release Please; repository CI rejects a release PR with inconsistent versions.

The bot uses `GITHUB_TOKEN`; no extra secret is needed. GitHub suppresses implicit
workflow triggers for that token, so the release workflow explicitly dispatches
both `ci.yml` and `pr-policy.yml` at the release PR branch and labels it directly.
The dispatched title check verifies that its commit is the actual current PR
head. Required checks still block a release PR until they pass. See the upstream
[token behavior](https://github.com/googleapis/release-please-action#other-actions-on-release-please-prs).

After a release PR is squash-merged and `main` CI passes, Release Please creates
the version tag, GitHub release, and changelog entry. It does not publish to
crates.io or PyPI. Do not remove `autorelease: pending` from an untagged release.

To recover from an API failure, rerun the failed Release job. If `main` has moved,
wait for its new CI run. To rerun checks on an unchanged release branch:

```sh
gh workflow run ci.yml --ref RELEASE_BRANCH
gh workflow run pr-policy.yml --ref RELEASE_BRANCH -f pull_request_number=NUMBER
```

If repository or organization policy prevents Actions from creating PRs, an
administrator must enable that setting; the bot should fail visibly. Review the
automation run logs instead of bypassing protection or manually marking checks
successful. Release Please and GitHub Actions versions are pinned and updated by
Dependabot.
