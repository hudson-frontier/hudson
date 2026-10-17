const definitions = require('../labels.json');

function labelsFor(title, filenames = []) {
  const match = /^(feat|fix|perf|docs|test|refactor|build|ci|chore|revert)(?:\([^()\r\n]+\))?(!)?: \S[^\r\n]*$/.exec(title);
  const labels = definitions.filter(label =>
    label.types?.includes(match?.[1]) ||
    label.paths?.some(prefix => filenames.some(path => path.startsWith(prefix)))
  ).map(label => label.name);
  if (match?.[2]) labels.push('breaking change');
  return labels;
}

async function syncLabels(github, repo) {
  const existing = await github.paginate(github.rest.issues.listLabelsForRepo, {...repo, per_page: 100});
  for (const {name, color, description} of definitions) {
    const old = existing.find(label => label.name === name);
    if (!old) {
      await github.rest.issues.createLabel({...repo, name, color, description});
    } else if (old.color.toLowerCase() !== color.toLowerCase() || old.description !== description) {
      await github.rest.issues.updateLabel({...repo, name, color, description});
    }
  }
}

async function labelItem(github, repo, item, opened = false) {
  const files = item.pull_request || item.head
    ? await github.paginate(github.rest.pulls.listFiles, {...repo, pull_number: item.number, per_page: 100})
    : [];
  // Include old paths when a change moves files between components.
  const paths = files.flatMap(file => [file.filename, file.previous_filename].filter(Boolean));
  const labels = labelsFor(item.title, paths);
  if (opened) labels.push('triage');
  if (labels.length) {
    // Additive: never remove a maintainer's labels or the release bot's state.
    await github.rest.issues.addLabels({...repo, issue_number: item.number, labels});
  }
}

module.exports = {labelsFor, syncLabels, labelItem};
