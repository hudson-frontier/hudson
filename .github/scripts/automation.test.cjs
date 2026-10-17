const test = require('node:test');
const assert = require('node:assert/strict');
const {labelsFor, syncLabels, labelItem} = require('./automation.cjs');

test('classifies breaking changes and multiple components without duplicate labels', () => {
  const labels = labelsFor('feat(core)!: change run schema', ['crates/hudson-core/src/lib.rs', 'docs/api.md', 'README.md']);
  assert.deepEqual(new Set(labels), new Set(['enhancement', 'breaking change', 'area: core', 'documentation']));
  assert.equal(labels.length, new Set(labels).size);
});

test('untrusted title text is inert and malformed titles do not imply a type', () => {
  assert.deepEqual(labelsFor('$(curl example.com)\nfeat: injected'), []);
  assert.deepEqual(labelsFor('fix(core): preserve `literal` input'), ['bug']);
});

test('PR files are paginated and renamed paths retain both component labels', async () => {
  let written;
  const listFiles = Symbol('listFiles');
  const github = {
    rest: {pulls: {listFiles}, issues: {addLabels: async value => { written = value; }}},
    paginate: async (method, params) => {
      assert.equal(method, listFiles);
      assert.equal(params.per_page, 100);
      return [{filename: 'crates/hudson-core/a.rs', previous_filename: 'crates/hudson-harness/a.rs'}];
    },
  };
  await labelItem(github, {owner: 'owner', repo: 'repo'}, {number: 7, title: 'fix: move code', pull_request: {}}, true);
  assert.deepEqual(new Set(written.labels), new Set(['bug', 'area: core', 'area: harness', 'triage']));
});

test('plain issues require no PR file lookup and edits do not restore triage', async () => {
  let written;
  const github = {rest: {issues: {addLabels: async value => { written = value; }}}};
  await labelItem(github, {}, {number: 1, title: 'feat: add export'});
  assert.deepEqual(written.labels, ['enhancement']);
});

test('label sync is idempotent and preserves unrelated repository labels', async () => {
  const definitions = require('../labels.json');
  const github = {
    rest: {issues: {listLabelsForRepo: 'labels', createLabel: async () => assert.fail('unexpected create'), updateLabel: async () => assert.fail('unexpected update')}},
    paginate: async () => [...definitions, {name: 'custom', color: '000000'}],
  };
  await syncLabels(github, {});
});
