const test = require('node:test');
const assert = require('node:assert/strict');
const {eligible, enableDependabotAutoMerge, refreshDependabotBranch} = require('./dependabot.cjs');

const repo = {owner: 'hudson-infinity', repo: 'hudson'};
const repository = 'hudson-infinity/hudson';
const pr = () => ({
  number: 1, node_id: 'PR_node', state: 'open', draft: false,
  user: {login: 'dependabot[bot]', type: 'Bot'},
  head: {sha: 'abc', ref: 'dependabot/cargo/update', repo: {full_name: repository}},
  base: {ref: 'main', sha: 'base', repo: {full_name: repository}}, auto_merge: null,
});

test('only accepts genuine same-repository Dependabot updates to main', () => {
  assert.equal(eligible(pr(), repository), true);
  const variants = [
    {user: {login: 'member', type: 'User'}},
    {user: {login: 'dependabot[bot]', type: 'User'}},
    {state: 'closed'}, {draft: true},
    {head: {...pr().head, repo: {full_name: 'fork/hudson'}}},
    {head: {...pr().head, ref: 'feature'}},
    {base: {...pr().base, ref: 'release'}},
    {base: {...pr().base, repo: {full_name: 'another/repo'}}},
  ];
  for (const change of variants) assert.equal(eligible({...pr(), ...change}, repository), false);
});

function fixture(first = pr(), second = first, reviews = []) {
  const calls = {reviews: [], mutations: []};
  let reads = 0;
  return {calls, merge: async (...args) => calls.mutations.push(args), github: {
    rest: {pulls: {
      get: async () => ({data: reads++ === 0 ? first : second}),
      listReviews: 'reviews',
      createReview: async value => calls.reviews.push(value),
    }},
    paginate: async () => reviews,
  }};
}
const core = {info() {}};

test('pins the approval to the current commit and requests native squash auto-merge', async () => {
  const {github, calls, merge} = fixture();
  await enableDependabotAutoMerge(github, repo, 1, core, merge);
  assert.equal(calls.reviews[0].commit_id, 'abc');
  assert.equal(calls.reviews[0].event, 'APPROVE');
  assert.deepEqual(calls.mutations[0], [repo, 1, 'abc']);
});

test('does not enable auto-merge on a concurrently replaced or closed PR', async () => {
  for (const current of [{...pr(), head: {...pr().head, sha: 'new'}}, {...pr(), state: 'closed'}]) {
    const {github, calls, merge} = fixture(pr(), current);
    await enableDependabotAutoMerge(github, repo, 1, core, merge);
    assert.equal(calls.mutations.length, 0);
  }
});

test('repeated events do not duplicate bot approval or re-enable auto-merge', async () => {
  const current = {...pr(), auto_merge: {merge_method: 'squash'}};
  const {github, calls, merge} = fixture(current, current, [
    {user: {login: 'github-actions[bot]'}, state: 'APPROVED', commit_id: 'abc'},
  ]);
  await enableDependabotAutoMerge(github, repo, 1, core, merge);
  assert.equal(calls.reviews.length, 0);
  assert.equal(calls.mutations.length, 0);
});

test('a human approval or an old bot approval does not count for the new commit', async () => {
  const {github, calls, merge} = fixture(pr(), pr(), [
    {user: {login: 'member'}, state: 'APPROVED', commit_id: 'abc'},
    {user: {login: 'github-actions[bot]'}, state: 'APPROVED', commit_id: 'old'},
  ]);
  await enableDependabotAutoMerge(github, repo, 1, core, merge);
  assert.equal(calls.reviews.length, 1);
});

test('untrusted PRs perform no review or mutation', async () => {
  const {github, calls, merge} = fixture({...pr(), user: {login: 'contributor', type: 'User'}});
  await enableDependabotAutoMerge(github, repo, 1, core, merge);
  assert.equal(calls.reviews.length, 0);
  assert.equal(calls.mutations.length, 0);
});

function refreshFixture({first = pr(), current = {...pr(), head: {...pr().head, sha: 'new'}}, behind = true, checks = [], conflict = false} = {}) {
  let reads = 0;
  const calls = {updates: [], dispatches: [], comparisons: [], warnings: []};
  return {calls, github: {
    rest: {
      pulls: {
        get: async () => ({data: reads++ === 0 ? first : current}),
        updateBranch: async args => {
          calls.updates.push(args);
          if (conflict) throw Object.assign(new Error('conflict'), {status: 422});
        },
      },
      repos: {getBranch: async () => ({data: {commit: {sha: 'current-main'}}}), compareCommitsWithBasehead: async args => {
        calls.comparisons.push(args);
        return {data: {ahead_by: behind ? 1 : 0}};
      }},
      checks: {listForRef: 'checks'},
      actions: {createWorkflowDispatch: async args => calls.dispatches.push(args)},
    },
    paginate: async () => checks,
  }, core: {info() {}, warning: value => calls.warnings.push(value)}};
}

test('behind branches use the external updater and leave ordinary CI triggers intact', async () => {
  const {github, core, calls} = refreshFixture();
  await refreshDependabotBranch(github, repo, 1, core, async () => {}, github.rest.pulls.updateBranch);
  assert.equal(calls.comparisons[0].basehead, 'abc...current-main');
  assert.equal(calls.updates[0].expected_head_sha, 'abc');
  assert.equal(calls.dispatches.length, 0);
});

test('current branches with active checks are not updated or repeatedly dispatched', async () => {
  const checks = ['Repository policy and workflows', 'PR policy'].map(name => ({name, app: {slug: 'github-actions'}}));
  const {github, core, calls} = refreshFixture({behind: false, checks});
  await refreshDependabotBranch(github, repo, 1, core);
  assert.equal(calls.updates.length, 0);
  assert.equal(calls.dispatches.length, 0);
});

test('missing CI dispatches are recovered without changing an up-to-date branch', async () => {
  const {github, core, calls} = refreshFixture({behind: false});
  await refreshDependabotBranch(github, repo, 1, core);
  assert.equal(calls.updates.length, 0);
  assert.equal(calls.dispatches.length, 2);
});

test('conflicting branches stay blocked and receive no synthetic success checks', async () => {
  const {github, core, calls} = refreshFixture({conflict: true});
  assert.equal(await refreshDependabotBranch(github, repo, 1, core, undefined, github.rest.pulls.updateBranch), false);
  assert.equal(calls.warnings.length, 1);
  assert.equal(calls.dispatches.length, 0);
});

test('an incomplete branch update is retried without blocking the remaining PRs', async () => {
  const {github, core, calls} = refreshFixture({current: pr()});
  assert.equal(await refreshDependabotBranch(github, repo, 1, core, async () => {}, github.rest.pulls.updateBranch), false);
  assert.equal(calls.warnings.length, 1);
  assert.equal(calls.dispatches.length, 0);
});

test('branch refresh never touches human or fork PRs', async () => {
  const {github, core, calls} = refreshFixture({first: {...pr(), user: {login: 'member', type: 'User'}}});
  await refreshDependabotBranch(github, repo, 1, core);
  assert.equal(calls.comparisons.length, 0);
  assert.equal(calls.updates.length, 0);
  assert.equal(calls.dispatches.length, 0);
});

test('missing external credentials never create approval-gated branch updates', async () => {
  const {github, core, calls} = refreshFixture();
  assert.equal(await refreshDependabotBranch(github, repo, 1, core), false);
  assert.equal(calls.updates.length, 0);
  assert.equal(calls.dispatches.length, 0);
  assert.match(calls.warnings[0], /DEPENDABOT_UPDATE_TOKEN/);
});
